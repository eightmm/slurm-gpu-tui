"""Per-node resident agent: pushes lightweight node telemetry to shared FS.

GPU mode collects nvidia-smi data every few seconds. CPU mode only reads
/proc/meminfo at a slower interval and is normally kept alive by systemd.
The collector reads both payloads locally and falls back to SSH when stale.
"""
from __future__ import annotations

import argparse
import fcntl
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import time
from dataclasses import asdict
from pathlib import Path

from . import __build__, __version__
from .common import NODE_PAYLOAD_CMD, parse_node_payload

AGENT_DIR = Path(os.getenv("SLURM_GPU_TUI_AGENT_DIR", str(Path.home() / ".sgpu" / "nodes")))
GPU_INTERVAL = int(os.getenv("SLURM_GPU_TUI_AGENT_SEC", "3"))
CPU_INTERVAL = int(os.getenv("SLURM_GPU_TUI_CPU_AGENT_SEC", "20"))
# Generous: without GPU persistence mode each nvidia-smi call can take ~5s
# (driver re-init), and the payload runs three of them
CMD_TIMEOUT = int(os.getenv("SLURM_GPU_TUI_AGENT_CMD_TIMEOUT_SEC", "40"))

# Node-local (NOT on NFS): one agent per node, log stays on the node
LOCK_FILE = Path("/tmp/sgpu-agent.lock")
LOG_FILE = Path("/tmp/sgpu-agent.log")
LOG_MAX_BYTES = 2 * 1024 * 1024

AGENT_PAYLOAD_VERSION = 7  # v7: node power dict (RAPL cpu/ram + IPMI sys watts)

# Fingerprint of the agent source (shared FS ⇒ same value on all hosts).
# The collector compares this against agent.py's current mtime and restarts
# agents left running with older code — upgrades propagate automatically.
try:
    AGENT_BUILD = str(int(os.path.getmtime(__file__)))
except OSError:
    AGENT_BUILD = "0"

_running = True


def _handle_signal(signum, frame):
    global _running
    _running = False


def _read_meminfo(path: Path = Path("/proc/meminfo")) -> dict:
    """Return total/used/available RAM in MiB without spawning a process."""
    values = {}
    for line in path.read_text().splitlines():
        if ":" not in line:
            continue
        key, raw = line.split(":", 1)
        parts = raw.split()
        if parts and parts[0].isdigit():
            values[key] = int(parts[0])
    total_kib = values.get("MemTotal", 0)
    avail_kib = values.get("MemAvailable", values.get("MemFree", 0))
    if total_kib <= 0 or avail_kib < 0:
        raise RuntimeError("invalid /proc/meminfo")
    total = total_kib // 1024
    avail = avail_kib // 1024
    used = max(0, (total_kib - avail_kib) // 1024)
    return {"total": str(total), "used": str(used), "avail": str(avail)}


RAPL_ROOT = Path("/sys/class/powercap")
# domain-name -> payload key: top-level packages are CPU; "dram" subdomains
# are RAM. Other subdomains (core/uncore) are subsets of package — skip to
# avoid double counting.
_rapl_prev: dict = {}  # sysfs dir -> (monotonic_ts, energy_uj)


def _read_rapl_power(root: Path = RAPL_ROOT, now: float | None = None) -> dict:
    """Return {"cpu": watts, "ram": watts} from RAPL energy deltas.

    Needs a previous sample: the first call (and any counter wrap) yields no
    value for that domain. Requires root — energy_uj is 0400 — so failures
    just produce an empty dict.
    """
    now = time.monotonic() if now is None else now
    watts = {"cpu": 0.0, "ram": 0.0}
    seen = {"cpu": False, "ram": False}
    try:
        domains = sorted(root.glob("intel-rapl:*"))
    except OSError:
        return {}
    for d in domains:
        depth = d.name.count(":")
        try:
            name = (d / "name").read_text().strip()
            if depth == 1:
                key = "cpu"  # package-N
            elif name == "dram":
                key = "ram"
            else:
                continue
            uj = int((d / "energy_uj").read_text())
        except (OSError, ValueError):
            continue
        prev = _rapl_prev.get(str(d))
        _rapl_prev[str(d)] = (now, uj)
        if prev is None:
            continue
        dt = now - prev[0]
        duj = uj - prev[1]
        if dt <= 0 or duj < 0:  # counter wrapped — resync next cycle
            continue
        watts[key] += duj / dt / 1e6
        seen[key] = True
    return {k: f"{v:.1f}" for k, v in watts.items() if seen[k]}


IPMI_MIN_INTERVAL = 10.0   # BMC reads are slow-ish; power moves slowly anyway
IPMI_FAIL_BACKOFF = 60.0
_ipmi_cache = [0.0, ""]  # next-read-not-before (monotonic), last value


def _parse_ipmi_power(out: str) -> str:
    """Extract watts from `ipmitool dcmi power reading` output."""
    for line in out.splitlines():
        if "Instantaneous power reading" in line and ":" in line:
            parts = line.split(":", 1)[1].split()
            if parts:
                try:
                    float(parts[0])
                except ValueError:
                    return ""
                return parts[0]
    return ""


def _read_ipmi_power() -> str:
    """Whole-node wall power from the BMC (root + /dev/ipmi0 required)."""
    now = time.monotonic()
    if now < _ipmi_cache[0]:
        return _ipmi_cache[1]
    err = ""
    try:
        r = subprocess.run(
            ["ipmitool", "dcmi", "power", "reading"],
            capture_output=True, text=True, timeout=5,
        )
        val = _parse_ipmi_power(r.stdout)
        if not val:
            err = (r.stderr or r.stdout).strip().splitlines()[:1]
            err = err[0] if err else f"exit {r.returncode}, unparsable output"
    except Exception as e:
        val, err = "", repr(e)
    if err and not _ipmi_cache[1]:
        # log only on failure streaks, once per backoff window
        print(f"[agent] ipmi power read failed: {err}")
    _ipmi_cache[0] = now + (IPMI_MIN_INTERVAL if val else IPMI_FAIL_BACKOFF)
    _ipmi_cache[1] = val
    return val


def collect_local(mode: str = "gpu") -> dict:
    """Collect a GPU or CPU-only payload locally."""
    if mode == "cpu":
        gpu_dicts = []
        mem_dict = _read_meminfo()
    elif mode == "gpu":
        out = subprocess.run(
            ["bash", "-c", NODE_PAYLOAD_CMD],
            capture_output=True, text=True, timeout=CMD_TIMEOUT,
        ).stdout
        gpus, mem = parse_node_payload(out)
        gpu_dicts = [asdict(g) for g in gpus]
        mem_dict = {"total": mem.total, "used": mem.used, "avail": mem.avail}
    else:
        raise ValueError(f"unknown agent mode: {mode}")
    return {
        "agent_version": AGENT_PAYLOAD_VERSION,
        "release": __version__,
        "package_build": __build__,
        "agent_build": AGENT_BUILD,
        "ts": time.time(),
        "hostname": socket.gethostname().split(".")[0],
        "node_kind": mode,
        "gpus": gpu_dicts,
        "mem": mem_dict,
        "power": {
            **_read_rapl_power(),
            **({"sys": v} if (v := _read_ipmi_power()) else {}),
        },
    }


_daemonized = False


def _rotate_log() -> None:
    if not _daemonized:
        return
    try:
        if LOG_FILE.exists() and LOG_FILE.stat().st_size > LOG_MAX_BYTES:
            LOG_FILE.rename(LOG_FILE.with_suffix(".log.1"))
            fd = os.open(LOG_FILE, os.O_WRONLY | os.O_CREAT | os.O_APPEND)
            os.dup2(fd, 1)
            os.dup2(fd, 2)
            os.close(fd)
    except Exception:
        pass


def run_agent(mode: str = "gpu") -> None:
    host = socket.gethostname().split(".")[0]
    out_path = AGENT_DIR / f"{host}.json"
    interval = CPU_INTERVAL if mode == "cpu" else GPU_INTERVAL

    # Single instance per node; retry briefly so restarts can overlap shutdown
    lock = open(LOCK_FILE, "w")
    deadline = time.time() + 5
    while True:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            break
        except OSError:
            if time.time() >= deadline:
                print("[agent] another agent is already running, exiting")
                sys.exit(1)
            time.sleep(0.5)

    signal.signal(signal.SIGTERM, _handle_signal)
    signal.signal(signal.SIGINT, _handle_signal)
    AGENT_DIR.mkdir(parents=True, exist_ok=True)
    print(f"[agent] started (host={host}, mode={mode}, pid={os.getpid()}, "
          f"interval={interval}s, out={out_path})")

    consecutive_failures = 0
    while _running:
        t0 = time.time()
        try:
            payload = collect_local(mode)
            # nvidia-smi present but no GPUs parsed = wedged driver. Writing a
            # fresh empty payload would make the collector treat the node as
            # healthy-with-zero-GPUs; failing lets the file go stale instead.
            if mode == "gpu" and not payload["gpus"]:
                installed = "installed" if shutil.which("nvidia-smi") else "missing"
                raise RuntimeError(
                    f"nvidia-smi returned no GPUs (binary {installed}; driver problem?)"
                )
            took = time.time() - t0
            if took > interval * 3:
                print(f"[agent] slow collect: {took:.1f}s")
            # tmp file on the same NFS dir so rename stays atomic
            tmp = out_path.with_name(f".{host}.json.tmp")
            tmp.write_text(json.dumps(payload, ensure_ascii=False))
            tmp.rename(out_path)
            if consecutive_failures:
                print(f"[agent] recovered after {consecutive_failures} failures")
            consecutive_failures = 0
        except Exception as e:
            # No write on failure: the file goes stale and the collector
            # falls back to SSH / marks the node stale.
            consecutive_failures += 1
            print(f"[agent] collect/write failed ({consecutive_failures}): {e}")
        _rotate_log()
        deadline = t0 + interval
        while _running and time.time() < deadline:
            time.sleep(0.5)
    print("[agent] stopped")


def daemonize(mode: str = "gpu") -> None:
    global _daemonized
    if os.fork() > 0:
        sys.exit(0)
    os.setsid()
    if os.fork() > 0:
        sys.exit(0)
    # dup2 over the real fds — merely rebinding sys.stdout would leave the
    # inherited ssh pipe open and the launching ssh session hanging
    null_fd = os.open(os.devnull, os.O_RDONLY)
    log_fd = os.open(LOG_FILE, os.O_WRONLY | os.O_CREAT | os.O_APPEND)
    os.dup2(null_fd, 0)
    os.dup2(log_fd, 1)
    os.dup2(log_fd, 2)
    os.close(null_fd)
    os.close(log_fd)
    sys.stdout = os.fdopen(1, "w", buffering=1)
    sys.stderr = os.fdopen(2, "w", buffering=1)
    _daemonized = True
    run_agent(mode)


def main() -> None:
    parser = argparse.ArgumentParser(description="sgpu node telemetry agent")
    parser.add_argument("--daemon", action="store_true", help="detach into the background")
    parser.add_argument("--mode", choices=("gpu", "cpu"), default="gpu")
    args = parser.parse_args()
    if args.daemon:
        daemonize(args.mode)
    else:
        run_agent(args.mode)
