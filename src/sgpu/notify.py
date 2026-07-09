"""Webhook notifications, driven by the collector's per-cycle snapshot.

Config: ~/.sgpu/webhook.json (or SLURM_GPU_TUI_WEBHOOK_URL for URL only)
{
  "url": "https://hooks.slack.com/services/...",   # Slack-compatible {"text": ...}
  "node_health": true,            # node down/recovered alerts
  "job_done_users": ["alice"],    # notify when these users' jobs finish
  "free_gpus_min": 0              # alert when free-GPU count reaches N (0 = off)
}

No config and no env URL -> notifier is inert. POSTs run in a daemon
thread so a slow webhook never blocks the collect loop. State persists
so restarts don't re-fire old alerts.
"""
from __future__ import annotations

import json
import os
import socket
import threading
import time
import urllib.request
from datetime import datetime
from pathlib import Path
from typing import Dict, List, Optional

DEBOUNCE_SEC = int(os.getenv("SLURM_GPU_TUI_WEBHOOK_DEBOUNCE_SEC", "1800"))


def _gpu_is_free(g: dict) -> bool:
    return not g.get("alloc_jobid") and not g.get("alloc_user") and not g.get("users")


def _host_ip() -> str:
    """Primary outbound IP (no traffic actually sent)."""
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("8.8.8.8", 80))
        ip = s.getsockname()[0]
        s.close()
        return ip
    except OSError:
        return ""


def _fmt_dur(sec: float) -> str:
    if sec < 3600:
        return f"{sec / 60:.0f}m"
    if sec < 86400:
        return f"{sec / 3600:.1f}h"
    return f"{sec / 86400:.1f}d"


class Notifier:
    def __init__(self, state_dir: Path) -> None:
        self._state_file = state_dir / "notify_state.json"
        self._cfg_path = Path.home() / ".sgpu" / "webhook.json"
        self._cfg_mtime: Optional[float] = None
        self._load_config()
        # persisted: node down-state, last-seen jobs, last alert ts per key
        st: dict = {}
        try:
            st = json.loads(self._state_file.read_text())
        except (OSError, ValueError):
            pass
        self._down: Dict[str, bool] = st.get("down", {})
        self._jobs: Dict[str, dict] = st.get("jobs", {})
        self._last_sent: Dict[str, float] = st.get("last_sent", {})
        self._free_was_below = bool(st.get("free_was_below", True))

    def _load_config(self) -> None:
        cfg: dict = {}
        try:
            self._cfg_mtime = self._cfg_path.stat().st_mtime
            cfg = json.loads(self._cfg_path.read_text())
        except (OSError, ValueError):
            self._cfg_mtime = None
        self.url: str = cfg.get("url") or os.getenv("SLURM_GPU_TUI_WEBHOOK_URL", "")
        self.node_health: bool = bool(cfg.get("node_health", True))
        self.job_done_users: List[str] = list(cfg.get("job_done_users", []))
        self.free_gpus_min: int = int(cfg.get("free_gpus_min", 0))
        # sender is the single identity in alerts — the real hostname is NOT
        # included ("master" here is also a compute node name; confusing)
        self.sender: str = cfg.get("sender_name", "AI-master")
        ip = _host_ip()
        self._origin = self.sender + (f" ({ip})" if ip else "")

    def _maybe_reload(self) -> None:
        """Pick up webhook.json edits without a collector restart."""
        try:
            mtime = self._cfg_path.stat().st_mtime
        except OSError:
            mtime = None
        if mtime != self._cfg_mtime:
            self._load_config()
            print(f"[notify] webhook config reloaded (enabled={self.enabled})", flush=True)

    @property
    def enabled(self) -> bool:
        return bool(self.url)

    def process(self, data: dict) -> None:
        """Diff one collector snapshot against remembered state; fire alerts."""
        self._maybe_reload()
        if not self.enabled:
            return
        now = time.time()
        nodes = data.get("nodes", [])

        if self.node_health:
            for n in nodes:
                name = n["name"]
                down = bool(n.get("stale")) or bool(n.get("error")) \
                    or any(s in n.get("state", "") for s in ("down", "drain", "fail"))
                # _down: 0/absent = up, else timestamp it went down
                # (older state files stored bools; coerce)
                was = self._down.get(name, 0)
                down_since = float(was) if not isinstance(was, bool) else (now if was else 0)
                if down and not down_since and self._ok_to_send(f"down:{name}", now):
                    why = n.get("error") or n.get("state", "unreachable")
                    njobs = n.get("jobs", [])
                    users: Dict[str, int] = {}
                    for j in njobs:
                        users[j.get("user", "?")] = users.get(j.get("user", "?"), 0) + 1
                    detail = (f"partition {n.get('partition', '?')} · "
                              f"{len(n.get('gpus', []))} GPUs · "
                              f"{len(njobs)} running job(s)")
                    if users:
                        detail += " — " + ", ".join(f"{u}×{c}" for u, c in sorted(users.items()))
                    self._post(f":rotating_light: *node {name} down* — {why[:120]}\n{detail}")
                elif down_since and not down:
                    self._post(f":white_check_mark: *node {name} recovered* "
                               f"after {_fmt_dur(now - down_since)}")
                self._down[name] = (down_since or now) if down else 0

        if self.job_done_users:
            current = {j["jobid"]: j for j in data.get("jobs", [])
                       if j.get("user") in self.job_done_users}
            for jid, j in self._jobs.items():
                if jid not in current:
                    self._post(f":checkered_flag: sgpu: job {jid} "
                               f"({j.get('jobname', '?')}) by {j.get('user', '?')} "
                               f"left the queue (done/cancelled) after {j.get('elapsed', '?')}")
            self._jobs = {jid: {"jobname": j.get("jobname", ""), "user": j.get("user", ""),
                                "elapsed": j.get("elapsed", "")} for jid, j in current.items()}

        if self.free_gpus_min > 0:
            free = sum(1 for n in nodes for g in n.get("gpus", []) if _gpu_is_free(g))
            if free >= self.free_gpus_min:
                if self._free_was_below and self._ok_to_send("free_gpus", now):
                    self._post(f":sparkles: sgpu: {free} free GPU(s) available")
                self._free_was_below = False
            else:
                self._free_was_below = True

        self._save()

    def _ok_to_send(self, key: str, now: float) -> bool:
        if now - self._last_sent.get(key, 0) < DEBOUNCE_SEC:
            return False
        self._last_sent[key] = now
        return True

    def _post(self, text: str) -> None:
        stamp = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        payload = {
            "text": f"{text}\n_{self._origin} · {stamp}_",
            # honored by legacy incoming webhooks, silently ignored by
            # app-scoped ones (those show the Slack app's own name)
            "username": self.sender,
            "icon_emoji": ":robot_face:",
        }

        def worker() -> None:
            try:
                req = urllib.request.Request(
                    self.url, data=json.dumps(payload).encode(),
                    headers={"Content-Type": "application/json"})
                urllib.request.urlopen(req, timeout=10).read()
            except Exception as e:
                print(f"[notify] webhook failed: {e}", flush=True)

        threading.Thread(target=worker, daemon=True, name="webhook").start()

    def _save(self) -> None:
        try:
            tmp = self._state_file.with_suffix(".tmp")
            tmp.write_text(json.dumps({
                "down": self._down, "jobs": self._jobs,
                "last_sent": self._last_sent,
                "free_was_below": self._free_was_below,
            }))
            tmp.rename(self._state_file)
        except OSError:
            pass
