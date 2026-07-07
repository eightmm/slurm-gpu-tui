"""Per-node resident agent: collects local GPU data and pushes it to shared FS.

Runs on each GPU node, writes ~/.sgpu/nodes/<hostname>.json every few seconds.
The collector on the master reads these files locally (master is the NFS
server) and only falls back to SSH pulling for nodes without a live agent.
Launched and kept alive by the collector via SSH self-healing.
"""
from __future__ import annotations

import fcntl
import json
import os
import signal
import socket
import subprocess
import sys
import time
from dataclasses import asdict
from pathlib import Path

from .common import NODE_PAYLOAD_CMD, parse_node_payload

AGENT_DIR = Path(os.getenv("SLURM_GPU_TUI_AGENT_DIR", str(Path.home() / ".sgpu" / "nodes")))
INTERVAL = int(os.getenv("SLURM_GPU_TUI_AGENT_SEC", "3"))
CMD_TIMEOUT = int(os.getenv("SLURM_GPU_TUI_AGENT_CMD_TIMEOUT_SEC", "20"))

# Node-local (NOT on NFS): one agent per node, log stays on the node
LOCK_FILE = Path("/tmp/sgpu-agent.lock")
LOG_FILE = Path("/tmp/sgpu-agent.log")
LOG_MAX_BYTES = 2 * 1024 * 1024

AGENT_PAYLOAD_VERSION = 1

_running = True


def _handle_signal(signum, frame):
    global _running
    _running = False


def collect_local() -> dict:
    """Run the payload command locally and return the JSON payload."""
    out = subprocess.run(
        ["bash", "-c", NODE_PAYLOAD_CMD],
        capture_output=True, text=True, timeout=CMD_TIMEOUT,
    ).stdout
    gpus, mem = parse_node_payload(out)
    return {
        "agent_version": AGENT_PAYLOAD_VERSION,
        "ts": time.time(),
        "hostname": socket.gethostname().split(".")[0],
        "gpus": [asdict(g) for g in gpus],
        "mem": {"total": mem.total, "used": mem.used, "avail": mem.avail},
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


def run_agent() -> None:
    host = socket.gethostname().split(".")[0]
    out_path = AGENT_DIR / f"{host}.json"

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
    print(f"[agent] started (host={host}, pid={os.getpid()}, "
          f"interval={INTERVAL}s, out={out_path})")

    consecutive_failures = 0
    while _running:
        t0 = time.time()
        try:
            payload = collect_local()
            took = time.time() - t0
            if took > INTERVAL * 3:
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
        deadline = t0 + INTERVAL
        while _running and time.time() < deadline:
            time.sleep(0.5)
    print("[agent] stopped")


def daemonize() -> None:
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
    run_agent()


def main() -> None:
    if "--daemon" in sys.argv:
        daemonize()
    else:
        run_agent()
