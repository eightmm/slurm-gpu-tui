"""sgpu - SLURM GPU monitoring TUI."""

import hashlib
from importlib.metadata import PackageNotFoundError, version
from pathlib import Path

try:
    __version__ = version("sgpu")
except PackageNotFoundError:  # source tree without an installed distribution
    __version__ = "0+unknown"


def _source_build_id() -> str:
    try:
        digest = hashlib.sha256()
        for path in sorted(Path(__file__).parent.glob("*.py")):
            digest.update(path.name.encode())
            digest.update(path.read_bytes())
        return digest.hexdigest()[:12]
    except OSError:
        return "unknown"


__build__ = _source_build_id()
