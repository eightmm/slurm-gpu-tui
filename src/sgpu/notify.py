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
import threading
import time
import urllib.request
from pathlib import Path
from typing import Dict, List, Optional

DEBOUNCE_SEC = int(os.getenv("SLURM_GPU_TUI_WEBHOOK_DEBOUNCE_SEC", "1800"))


def _gpu_is_free(g: dict) -> bool:
    return not g.get("alloc_jobid") and not g.get("alloc_user") and not g.get("users")


class Notifier:
    def __init__(self, state_dir: Path) -> None:
        self._state_file = state_dir / "notify_state.json"
        cfg: dict = {}
        cfg_path = Path.home() / ".sgpu" / "webhook.json"
        try:
            cfg = json.loads(cfg_path.read_text())
        except (OSError, ValueError):
            pass
        self.url: str = cfg.get("url") or os.getenv("SLURM_GPU_TUI_WEBHOOK_URL", "")
        self.node_health: bool = bool(cfg.get("node_health", True))
        self.job_done_users: List[str] = list(cfg.get("job_done_users", []))
        self.free_gpus_min: int = int(cfg.get("free_gpus_min", 0))
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

    @property
    def enabled(self) -> bool:
        return bool(self.url)

    def process(self, data: dict) -> None:
        """Diff one collector snapshot against remembered state; fire alerts."""
        if not self.enabled:
            return
        now = time.time()
        nodes = data.get("nodes", [])

        if self.node_health:
            for n in nodes:
                name = n["name"]
                down = bool(n.get("stale")) or bool(n.get("error")) \
                    or any(s in n.get("state", "") for s in ("down", "drain", "fail"))
                was_down = self._down.get(name, False)
                if down and not was_down and self._ok_to_send(f"down:{name}", now):
                    why = n.get("error") or n.get("state", "unreachable")
                    self._post(f":rotating_light: sgpu: node *{name}* down — {why[:120]}")
                elif was_down and not down:
                    self._post(f":white_check_mark: sgpu: node *{name}* recovered")
                self._down[name] = down

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
        def worker() -> None:
            try:
                req = urllib.request.Request(
                    self.url, data=json.dumps({"text": text}).encode(),
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
