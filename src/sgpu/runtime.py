"""Runtime paths and symlink-safe writes.

The collector normally runs as root and publishes a world-readable snapshot for
unprivileged users to read (docs/PUSH.md). That makes every path it writes a
potential root-write primitive: if an unprivileged user owns the directory, they
can pre-plant a symlink at the temp name and redirect root's write anywhere.
The same applies harder to the node agents — they run as root on compute nodes
where every user has a login shell.

So: the runtime directory is validated (and repaired, when we have the
privileges) before anything is written into it, and every open refuses to
follow a symlink.

The default location stays ``/tmp/slurm-gpu-tui``. node_exporter's textfile
collector, the Grafana bridge script, and the docs all point at it; moving it
would silently break the metrics pipeline. Sites that prefer a root-only tmpfs
can set ``SLURM_GPU_TUI_DATA_DIR=/run/sgpu``.
"""
from __future__ import annotations

import errno
import os
import stat
import tempfile
from pathlib import Path
from typing import Union


class UnsafeRuntimeDir(RuntimeError):
    """A runtime directory an unprivileged user could use to redirect writes."""


def dir_trust_problem(path: Union[str, Path]) -> str:
    """Why `path` is unsafe for privileged writes; "" when it is fine.

    A missing directory is fine — the caller creates it with a safe mode.
    """
    try:
        st = os.lstat(path)
    except FileNotFoundError:
        return ""
    except OSError as e:
        return f"cannot stat ({e.strerror or e})"
    if stat.S_ISLNK(st.st_mode):
        return "is a symlink"
    if not stat.S_ISDIR(st.st_mode):
        return "is not a directory"
    me = os.geteuid()
    if st.st_uid not in (0, me):
        return f"owned by uid {st.st_uid}, expected uid 0 or {me}"
    # Sticky does not help here: the temp names we create do not exist yet, so
    # anyone with write permission can still plant a symlink for us to follow.
    if st.st_mode & (stat.S_IWGRP | stat.S_IWOTH):
        return f"is group/world-writable (mode {stat.S_IMODE(st.st_mode):04o})"
    return ""


def _open_dir(path: Union[str, Path]) -> int:
    """Open a directory without traversing a final symlink."""
    return os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)


def ensure_secure_dir(path: Union[str, Path], mode: int = 0o755) -> Path:
    """Create or repair `path` so this process can safely write inside it.

    Repair (chown/chmod through an already-open fd, so there is no window where
    the path could be swapped) only happens when we actually have the
    privileges. Anything left unsafe raises rather than writing into a
    directory somebody else controls.
    """
    p = Path(path)
    problem = dir_trust_problem(p)
    if "symlink" in problem or "not a directory" in problem:
        raise UnsafeRuntimeDir(
            f"{p} {problem} — refusing to write. Remove it and restart, "
            f"or point SLURM_GPU_TUI_DATA_DIR at a directory you own."
        )
    # The parent may be missing on a fresh boot (/run/sgpu, /var/lib/sgpu).
    p.mkdir(parents=True, mode=mode, exist_ok=True)
    fd = None
    try:
        fd = _open_dir(p)
        st = os.fstat(fd)
        mine = st.st_uid == os.geteuid()
        if not mine and os.geteuid() == 0:
            os.fchown(fd, 0, -1)
            mine = True
        # mkdir()'s mode is masked by umask, so a service started with
        # umask 077 would publish a 0700 directory and silently lock every
        # ordinary user out of the snapshot. Set it explicitly.
        if mine and stat.S_IMODE(st.st_mode) != mode:
            os.fchmod(fd, mode)
    except OSError as e:
        raise UnsafeRuntimeDir(
            f"{p} {problem or 'could not be secured'} ({e.strerror or e}) — "
            f"refusing to write. chown it to uid {os.geteuid()} and "
            f"chmod {mode:04o}, or set SLURM_GPU_TUI_DATA_DIR elsewhere."
        ) from e
    finally:
        if fd is not None:
            os.close(fd)
    remaining = dir_trust_problem(p)
    if remaining:
        raise UnsafeRuntimeDir(f"{p} {remaining} — refusing to write")
    return p


def _tmp_sibling(path: Path) -> Path:
    # Same directory, so the rename stays atomic; pid-scoped so two writers
    # (collector main loop vs. a state-saving worker) cannot collide.
    return path.with_name(f".{path.name}.{os.getpid()}.tmp")


def atomic_write(
    path: Union[str, Path],
    data: Union[str, bytes],
    mode: int = 0o644,
    fsync: bool = False,
) -> None:
    """Replace `path` with `data`, never following a symlink.

    ``O_NOFOLLOW|O_EXCL`` on the temp name means a planted symlink makes us
    fail loudly instead of writing through it. ``os.replace`` swaps the
    directory entry, so a symlink sitting at `path` is replaced rather than
    followed.
    """
    p = Path(path)
    payload = data.encode() if isinstance(data, str) else data
    tmp = _tmp_sibling(p)
    # Clears a leftover from a crashed run — and defuses a planted symlink,
    # since unlink acts on the link itself.
    try:
        os.unlink(tmp)
    except FileNotFoundError:
        pass
    except OSError as e:
        if e.errno != errno.ENOENT:
            raise
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode)
    try:
        with os.fdopen(fd, "wb", closefd=False) as f:
            f.write(payload)
            f.flush()
            if fsync:
                os.fsync(fd)
        os.fchmod(fd, mode)  # defeat a restrictive umask; readers need this
    except BaseException:
        os.close(fd)
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise
    os.close(fd)
    os.replace(tmp, p)


def open_append(path: Union[str, Path], mode: int = 0o644) -> int:
    """Append-only fd that refuses a symlink at `path` (log files)."""
    return os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND | os.O_NOFOLLOW, mode)


def open_lock(path: Union[str, Path], mode: int = 0o644) -> int:
    """Lock-file fd that refuses a symlink at `path`.

    Deliberately no ``O_TRUNC``: truncation through a planted symlink is its
    own destructive primitive, and flock() does not need an empty file.
    """
    return os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, mode)


def default_data_dir() -> Path:
    """Live snapshot directory (data.json, metrics.prom, pid/lock).

    Kept at /tmp/slurm-gpu-tui by default: node_exporter's textfile collector
    argument, grafana/sgpu-remote-bridge.sh, and docs/GRAFANA.md all name it.
    """
    return Path(os.getenv("SLURM_GPU_TUI_DATA_DIR", "/tmp/slurm-gpu-tui"))


def default_state_dir() -> Path:
    """Where the *writer* keeps persistent state (usage, waste ages, inventory).

    Must survive reboots, so not /tmp. A root collector cannot use
    ``~/.sgpu/state``: /root is mode 0700, so the published TUI running as an
    ordinary user could never read the usage history — it would silently fall
    back to whatever stale copy happened to be in the data dir. /var/lib/sgpu
    is root-owned, world-readable, and persistent.
    """
    env = os.getenv("SLURM_GPU_TUI_STATE_DIR")
    if env:
        return Path(env)
    if os.geteuid() == 0:
        return Path("/var/lib/sgpu")
    return Path.home() / ".sgpu" / "state"


def state_dir_candidates() -> list:
    """Reader-side search order for published state, best first.

    Includes the pre-/var/lib locations so an upgraded install keeps showing
    history collected by the previous layout.
    """
    env = os.getenv("SLURM_GPU_TUI_STATE_DIR")
    if env:
        return [Path(env)]
    out = [Path("/var/lib/sgpu")]
    try:
        out.append(Path.home() / ".sgpu" / "state")
    except (OSError, RuntimeError):  # no resolvable home
        pass
    out.append(default_data_dir())
    return out


def agent_runtime_path(name: str) -> Path:
    """Node-local path for the agent's lock/log.

    A fixed ``/tmp/sgpu-agent.log`` is a root-append target on a compute node,
    where the agent runs as root and every user has a shell. Prefer root-owned
    ``/run``; unprivileged runs get a uid-scoped name so users cannot collide
    with (or plant files for) each other.
    """
    if os.geteuid() == 0 and os.path.isdir("/run"):
        return Path("/run") / f"sgpu-agent.{name}"
    return Path(tempfile.gettempdir()) / f"sgpu-agent-{os.geteuid()}.{name}"


def trusted_payload_uids(agent_dir: Union[str, Path, None] = None) -> frozenset:
    """UIDs whose push-agent payloads the collector will believe.

    ``AGENT_DIR`` is mode 1777 (docs/PUSH.md: node agents run as root and an
    NFS export with root_squash turns their writes into ``nobody``), so file
    existence proves nothing about who wrote it. Without this check any user
    can plant ``<node>.json`` and forge utilization, process owners, or ECC
    counts — which then drive Slack alerts and GPU-hour accounting.

    Trusted by default: root, us, whoever owns the agent dir, and nobody
    (root_squash). ``SLURM_GPU_TUI_AGENT_TRUSTED_UIDS`` replaces the default
    set for sites that map agent writes to some other account.
    """
    override = os.getenv("SLURM_GPU_TUI_AGENT_TRUSTED_UIDS", "").strip()
    if override:
        return frozenset(
            int(x) for x in override.replace(",", " ").split() if x.lstrip("-").isdigit()
        )
    uids = {0, os.geteuid()}
    try:  # nobody: what root_squash rewrites the agents' writes to
        import pwd

        uids.add(pwd.getpwnam("nobody").pw_uid)
    except (KeyError, ImportError):
        uids.add(65534)
    if agent_dir is not None:
        try:
            uids.add(os.stat(agent_dir).st_uid)
        except OSError:
            pass
    return frozenset(uids)
