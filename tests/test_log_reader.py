"""Privilege-dropped reader used for root-squashed job homes."""
import os

import pytest

from sgpu.log_reader import read_owned_tail


def test_owned_reader_returns_only_bounded_tail(tmp_path):
    path = tmp_path / "job.out"
    path.write_bytes(b"prefix" + b"x" * 100 + b"TAIL")

    assert read_owned_tail(str(path), 16) == b"x" * 12 + b"TAIL"


def test_owned_reader_rejects_final_symlink(tmp_path):
    real = tmp_path / "private"
    real.write_text("secret")
    link = tmp_path / "job.out"
    link.symlink_to(real)

    with pytest.raises(ValueError, match="regular file"):
        read_owned_tail(str(link), 64)


def test_owned_reader_rejects_owner_mismatch(tmp_path, monkeypatch):
    path = tmp_path / "job.out"
    path.write_text("secret")
    monkeypatch.setattr(os, "geteuid", lambda: path.stat().st_uid + 1)

    with pytest.raises(ValueError, match="owned by the job user"):
        read_owned_tail(str(path), 64)
