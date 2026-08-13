"""Bounded job-owner log reader used for root-squashed shared homes.

This module is launched in a separate process after the collector drops that
child to the scheduler-reported job owner.  Keeping the privilege change out
of the multi-threaded collector avoids process-wide credential races.
"""
from __future__ import annotations

import os
import stat
import sys


EXIT_UNREADABLE = 3
EXIT_UNSAFE = 4


def read_owned_tail(path: str, limit: int) -> bytes:
    before = os.lstat(path)
    if not stat.S_ISREG(before.st_mode) or before.st_uid != os.geteuid():
        raise ValueError("source is not a regular file owned by the job user")
    flags = os.O_RDONLY | os.O_NONBLOCK
    flags |= getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        fd = os.open(path, flags)
    except OSError as exc:
        raise PermissionError from exc
    try:
        st = os.fstat(fd)
        if (
            not stat.S_ISREG(st.st_mode)
            or st.st_uid != os.geteuid()
            or (st.st_dev, st.st_ino) != (before.st_dev, before.st_ino)
        ):
            raise ValueError("source is not a regular file owned by the job user")
        if st.st_size > limit:
            os.lseek(fd, st.st_size - limit, os.SEEK_SET)
        chunks = []
        remaining = limit
        while remaining > 0:
            chunk = os.read(fd, min(64 * 1024, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        return b"".join(chunks)
    finally:
        os.close(fd)


def main() -> int:
    if len(sys.argv) != 3:
        return EXIT_UNREADABLE
    try:
        limit = max(0, min(int(sys.argv[2]), 1024 * 1024))
        data = read_owned_tail(sys.argv[1], limit)
    except ValueError:
        return EXIT_UNSAFE
    except (OSError, PermissionError):
        return EXIT_UNREADABLE
    sys.stdout.buffer.write(data)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
