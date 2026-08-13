"""Runtime path hardening.

The collector and the node agents run as root while unprivileged users share
the machine, so these are the checks that stop a planted symlink from turning
a routine write into a root-write primitive.
"""
import os
import stat

import pytest

from sgpu.runtime import (
    UnsafeRuntimeDir, agent_runtime_path, atomic_write,
    atomic_write_with_signature, default_state_dir, dir_trust_problem,
    ensure_secure_dir, open_append, open_lock, trusted_payload_uids,
)


# ── directory trust ───────────────────────────────────────────────────────

def test_missing_dir_is_not_a_problem(tmp_path):
    assert dir_trust_problem(tmp_path / "nope") == ""


def test_plain_owned_dir_is_trusted(tmp_path):
    d = tmp_path / "d"
    d.mkdir(mode=0o755)
    assert dir_trust_problem(d) == ""


def test_symlinked_dir_is_rejected(tmp_path):
    real = tmp_path / "real"
    real.mkdir()
    link = tmp_path / "link"
    link.symlink_to(real)
    assert "symlink" in dir_trust_problem(link)


def test_world_writable_dir_is_rejected(tmp_path):
    d = tmp_path / "d"
    d.mkdir(mode=0o777)
    os.chmod(d, 0o777)
    assert "writable" in dir_trust_problem(d)


def test_sticky_does_not_rescue_world_writable(tmp_path):
    # /tmp-style 1777: sticky stops deletion of *existing* files, but our temp
    # names do not exist yet, so anyone can still plant a symlink for us.
    d = tmp_path / "d"
    d.mkdir()
    os.chmod(d, 0o1777)
    assert "writable" in dir_trust_problem(d)


def test_non_directory_is_rejected(tmp_path):
    f = tmp_path / "f"
    f.write_text("x")
    assert "not a directory" in dir_trust_problem(f)


def test_ensure_secure_dir_creates_with_mode(tmp_path):
    d = ensure_secure_dir(tmp_path / "a" / "b")
    assert d.is_dir()
    assert stat.S_IMODE(d.stat().st_mode) == 0o755


def test_ensure_secure_dir_repairs_loose_permissions(tmp_path):
    d = tmp_path / "d"
    d.mkdir()
    os.chmod(d, 0o777)
    ensure_secure_dir(d)
    assert dir_trust_problem(d) == ""


def test_ensure_secure_dir_refuses_symlink(tmp_path):
    real = tmp_path / "real"
    real.mkdir()
    link = tmp_path / "link"
    link.symlink_to(real)
    with pytest.raises(UnsafeRuntimeDir):
        ensure_secure_dir(link)


# ── writes ────────────────────────────────────────────────────────────────

def test_atomic_write_creates_and_replaces(tmp_path):
    p = tmp_path / "data.json"
    atomic_write(p, "one")
    assert p.read_text() == "one"
    atomic_write(p, "two")
    assert p.read_text() == "two"


def test_atomic_write_signature_identifies_installed_file(tmp_path):
    p = tmp_path / "data.json"
    signature = atomic_write_with_signature(p, "one")
    st = p.stat()
    assert signature == (
        st.st_mode, st.st_dev, st.st_ino, st.st_size,
        st.st_mtime_ns, st.st_ctime_ns,
    )


def test_atomic_write_applies_mode_despite_umask(tmp_path):
    old = os.umask(0o077)
    try:
        p = tmp_path / "data.json"
        atomic_write(p, "x", mode=0o644)
        assert stat.S_IMODE(p.stat().st_mode) == 0o644
    finally:
        os.umask(old)


def test_atomic_write_leaves_no_temp_file(tmp_path):
    atomic_write(tmp_path / "data.json", "x")
    assert [p.name for p in tmp_path.iterdir()] == ["data.json"]


def test_atomic_write_replaces_a_symlink_instead_of_following_it(tmp_path):
    # The attack: point the published name at something valuable and wait for
    # root to write. os.replace swaps the directory entry, so the target is
    # untouched and the link is gone.
    victim = tmp_path / "victim"
    victim.write_text("precious")
    p = tmp_path / "data.json"
    p.symlink_to(victim)
    atomic_write(p, "snapshot")
    assert victim.read_text() == "precious"
    assert not p.is_symlink()
    assert p.read_text() == "snapshot"


def test_atomic_write_defuses_a_planted_temp_symlink(tmp_path):
    victim = tmp_path / "victim"
    victim.write_text("precious")
    p = tmp_path / "data.json"
    planted = tmp_path / f".{p.name}.{os.getpid()}.tmp"
    planted.symlink_to(victim)
    atomic_write(p, "snapshot")
    assert victim.read_text() == "precious"
    assert p.read_text() == "snapshot"


def test_open_append_refuses_a_symlink(tmp_path):
    victim = tmp_path / "victim"
    victim.write_text("")
    log = tmp_path / "agent.log"
    log.symlink_to(victim)
    with pytest.raises(OSError):
        open_append(log)


def test_open_lock_refuses_a_symlink_and_does_not_truncate(tmp_path):
    victim = tmp_path / "victim"
    victim.write_text("precious")
    lock = tmp_path / "agent.lock"
    lock.symlink_to(victim)
    with pytest.raises(OSError):
        open_lock(lock)
    assert victim.read_text() == "precious"


def test_open_lock_preserves_existing_content(tmp_path):
    lock = tmp_path / "agent.lock"
    lock.write_text("keep")
    os.close(open_lock(lock))
    assert lock.read_text() == "keep"


# ── agent paths and payload trust ─────────────────────────────────────────

def test_agent_runtime_path_is_uid_scoped_when_unprivileged():
    if os.geteuid() == 0:
        pytest.skip("root uses /run")
    p = agent_runtime_path("log")
    assert str(os.geteuid()) in p.name
    assert p.name.endswith(".log")


def test_trusted_payload_uids_includes_root_and_self():
    uids = trusted_payload_uids()
    assert 0 in uids and os.geteuid() in uids


def test_trusted_payload_uids_env_override(monkeypatch):
    monkeypatch.setenv("SLURM_GPU_TUI_AGENT_TRUSTED_UIDS", "1234, 5678")
    assert trusted_payload_uids() == frozenset({1234, 5678})


def test_trusted_payload_uids_includes_agent_dir_owner(tmp_path):
    assert tmp_path.stat().st_uid in trusted_payload_uids(tmp_path)


def test_default_state_dir_env_override(monkeypatch):
    monkeypatch.setenv("SLURM_GPU_TUI_STATE_DIR", "/somewhere/else")
    assert str(default_state_dir()) == "/somewhere/else"


def test_default_state_dir_is_published_for_root(monkeypatch):
    # /root is mode 0700: keeping usage history there makes it unreadable by
    # the very users the published TUI is for.
    monkeypatch.delenv("SLURM_GPU_TUI_STATE_DIR", raising=False)
    monkeypatch.setattr(os, "geteuid", lambda: 0)
    assert str(default_state_dir()) == "/var/lib/sgpu"
