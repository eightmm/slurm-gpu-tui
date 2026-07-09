"""Webhook notifications, driven by the collector's per-cycle snapshot.

Config: ~/.sgpu/webhook.json (or SLURM_GPU_TUI_WEBHOOK_URL for URL only)
{
  "url": "https://hooks.slack.com/services/...",   # Slack-compatible {"text": ...}
  "bot_token": "xoxb-...",        # optional: Slack bot token -> alerts become
  "channel": "#gpu-cluster",      #   replies under one parent message per day
                                  #   (incoming webhooks can't thread; needs a
                                  #   bot with chat:write invited to the channel)
  "sender_name": "AI-master",     # identity in the alert footer/username
  "lang": "en",                   # alert language: "en" or "ko"
  "node_health": true,            # node down/recovered alerts
  "down_grace_sec": 180,          # down must persist this long before alerting
  "waste_alert_hours": 2,         # GPU idle/parked >= N hours (0 = off)
  "rogue_alert": true,            # GPU used outside SLURM
  "temp_alert_c": 0,              # GPU temperature >= N°C (0 = off; ~90 typical)
  "ecc_alert": true,             # uncorrectable ECC errors (silent HW failure)
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
# waste/rogue conditions persist for hours — re-nag much less often
NAG_REALERT_SEC = int(os.getenv("SLURM_GPU_TUI_WEBHOOK_NAG_SEC", "21600"))
# override for tests; real Slack Web API otherwise
SLACK_API_BASE = os.getenv("SLURM_GPU_TUI_SLACK_API_BASE", "https://slack.com/api")

# Alert message templates per language. These are user-facing Slack strings
# (intentionally localizable); all code/logs stay English.
MSG = {
    "en": {
        "parent": ":calendar: *GPU cluster alerts — {date}* · {sender}",
        "down": ":rotating_light: *node {name} down* — {why}\n{detail}",
        "down_detail": "partition {part} · {ngpu} GPUs · {njobs} running job(s)",
        "recovered": ":white_check_mark: *node {name} recovered* after {dur}",
        "job_done": ":checkered_flag: job {jid} ({name}) by {user} finished after {elapsed}",
        "waste": ":hourglass: *{loc} {kind} {dur}* — held by {user} (job {jid}), no compute",
        "rogue": ":no_entry: *{loc} used outside SLURM* by {user}",
        "free": ":sparkles: {free} free GPU(s) available",
        "temp": ":thermometer: *{loc} {temp}°C* — over {thr}°C\n{hw}",
        "ecc": ":warning: *{loc} uncorrectable ECC errors: {n}* — GPU may be failing\n{hw}",
        "idle": "idle", "parked": "parked",
    },
    "ko": {
        "parent": ":calendar: *GPU 클러스터 알림 — {date}* · {sender}",
        "down": ":rotating_light: *{name} 노드 다운* — {why}\n{detail}",
        "down_detail": "파티션 {part} · GPU {ngpu}개 · 실행 작업 {njobs}개",
        "recovered": ":white_check_mark: *{name} 노드 복구* — {dur} 만에",
        "job_done": ":checkered_flag: 작업 {jid} ({name}, {user}) 종료 — {elapsed} 경과",
        "waste": ":hourglass: *{loc} {kind} {dur}* — {user} 점유(작업 {jid}), 연산 없음",
        "rogue": ":no_entry: *{loc} SLURM 외부 사용* — {user}",
        "free": ":sparkles: 여유 GPU {free}장 사용 가능",
        "temp": ":thermometer: *{loc} {temp}°C* — {thr}°C 초과\n{hw}",
        "ecc": ":warning: *{loc} uncorrectable ECC 에러 {n}건* — GPU 이상 가능\n{hw}",
        "idle": "유휴", "parked": "점유",
    },
}


def _gpu_is_free(g: dict) -> bool:
    return not g.get("alloc_jobid") and not g.get("alloc_user") and not g.get("users")


def _to_int(v) -> Optional[int]:
    """nvidia-smi numeric field -> int, or None for N/A / non-numeric."""
    try:
        return int(str(v).strip())
    except (ValueError, TypeError, AttributeError):
        return None


def _hw_id(g: dict) -> str:
    """Physical-identity line for a GPU (UUID / PCI bus / serial), skipping
    fields the driver reports as N/A."""
    def ok(v: str) -> bool:
        return bool(v) and "N/A" not in v and "Not Supported" not in v
    parts = []
    if ok(g.get("uuid", "")):
        parts.append(f"UUID {g['uuid']}")
    if ok(g.get("pci_bus", "")):
        parts.append(f"bus {g['pci_bus']}")
    if ok(g.get("serial", "")):
        parts.append(f"S/N {g['serial']}")
    return "_" + " · ".join(parts) + "_" if parts else ""


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


def _fmt_dur(sec: float, lang: str = "en") -> str:
    m, h, d = ("분", "시간", "일") if lang == "ko" else ("m", "h", "d")
    if sec < 3600:
        return f"{sec / 60:.0f}{m}"
    if sec < 86400:
        return f"{sec / 3600:.1f}{h}"
    return f"{sec / 86400:.1f}{d}"


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
        self._down: Dict[str, float] = st.get("down", {})
        self._jobs: Dict[str, dict] = st.get("jobs", {})
        self._last_sent: Dict[str, float] = st.get("last_sent", {})
        self._free_was_below = bool(st.get("free_was_below", True))
        # daily thread parent (bot mode): reuse across restarts within a day
        self._thread_day: str = st.get("thread_day", "")
        self._thread_ts: str = st.get("thread_ts", "")
        self._post_lock = threading.Lock()
        # first-seen ts of an unconfirmed down; deliberately NOT persisted so
        # a collector restart re-starts the grace clock (cold start looks
        # exactly like an outage for the first cycles)
        self._pending_down: Dict[str, float] = {}

    def _load_config(self) -> None:
        cfg: dict = {}
        try:
            self._cfg_mtime = self._cfg_path.stat().st_mtime
            cfg = json.loads(self._cfg_path.read_text())
        except (OSError, ValueError):
            self._cfg_mtime = None
        self.url: str = cfg.get("url") or os.getenv("SLURM_GPU_TUI_WEBHOOK_URL", "")
        # bot mode: threads alerts under one parent per day (needs chat:write)
        self.bot_token: str = cfg.get("bot_token", "") or os.getenv("SLURM_GPU_TUI_SLACK_BOT_TOKEN", "")
        self.channel: str = cfg.get("channel", "")
        self.node_health: bool = bool(cfg.get("node_health", True))
        self.job_done_users: List[str] = list(cfg.get("job_done_users", []))
        self.free_gpus_min: int = int(cfg.get("free_gpus_min", 0))
        self.waste_alert_hours: float = float(cfg.get("waste_alert_hours", 0))
        self.rogue_alert: bool = bool(cfg.get("rogue_alert", False))
        self.temp_alert_c: float = float(cfg.get("temp_alert_c", 0))
        self.ecc_alert: bool = bool(cfg.get("ecc_alert", True))
        self.down_grace_sec: float = float(cfg.get("down_grace_sec", 180))
        # sender is the single identity in alerts — the real hostname is NOT
        # included ("master" here is also a compute node name; confusing)
        self.sender: str = cfg.get("sender_name", "AI-master")
        self.lang: str = cfg.get("lang", "en") if cfg.get("lang") in MSG else "en"
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

    def _m(self, key: str, **kw) -> str:
        return MSG.get(self.lang, MSG["en"])[key].format(**kw)

    @property
    def _bot_mode(self) -> bool:
        return bool(self.bot_token and self.channel)

    @property
    def enabled(self) -> bool:
        return bool(self.url) or self._bot_mode

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
                # Node health = SLURM state only (from local sinfo, always
                # available and authoritative). sgpu's own SSH/agent collection
                # errors or staleness are NOT node-down — SSH-pull clusters
                # routinely fail to reach CPU/GPU-less nodes, which used to
                # spam false "down" alerts.
                state = n.get("state", "").lower()
                down = any(s in state for s in ("down", "drain", "fail", "err", "boot"))
                # _down: 0/absent = up, else ts of the CONFIRMED (alerted)
                # outage start (older state files stored bools; coerce)
                was = self._down.get(name, 0)
                confirmed = float(was) if not isinstance(was, bool) else (now if was else 0)
                if down:
                    first = self._pending_down.setdefault(name, now)
                    # grace: cold starts and transient SSH/agent staleness
                    # look identical to an outage for a few cycles
                    if not confirmed and now - first >= self.down_grace_sec \
                            and self._ok_to_send(f"down:{name}", now):
                        why = n.get("state", "") or "unreachable"
                        njobs = n.get("jobs", [])
                        users: Dict[str, int] = {}
                        for j in njobs:
                            users[j.get("user", "?")] = users.get(j.get("user", "?"), 0) + 1
                        detail = self._m("down_detail", part=n.get("partition", "?"),
                                         ngpu=len(n.get("gpus", [])), njobs=len(njobs))
                        if users:
                            detail += " — " + ", ".join(f"{u}×{c}" for u, c in sorted(users.items()))
                        self._post(self._m("down", name=name, why=why[:120], detail=detail))
                        self._down[name] = first
                else:
                    self._pending_down.pop(name, None)
                    if confirmed:
                        self._post(self._m("recovered", name=name,
                                           dur=_fmt_dur(now - confirmed, self.lang)))
                    self._down[name] = 0

        if self.job_done_users:
            current = {j["jobid"]: j for j in data.get("jobs", [])
                       if j.get("user") in self.job_done_users}
            for jid, j in self._jobs.items():
                if jid not in current:
                    self._post(self._m("job_done", jid=jid, name=j.get("jobname", "?"),
                                       user=j.get("user", "?"), elapsed=j.get("elapsed", "?")))
            self._jobs = {jid: {"jobname": j.get("jobname", ""), "user": j.get("user", ""),
                                "elapsed": j.get("elapsed", "")} for jid, j in current.items()}

        if self.waste_alert_hours > 0 or self.rogue_alert:
            for n in nodes:
                for g in n.get("gpus", []):
                    loc = f"{n['name']} GPU{g.get('index', '?')}"
                    wasted = max(g.get("idle_sec", 0), g.get("parked_sec", 0))
                    if self.waste_alert_hours > 0 and wasted >= self.waste_alert_hours * 3600:
                        kind = self._m("idle" if g.get("idle_sec", 0) >= g.get("parked_sec", 0)
                                       else "parked")
                        jid = g.get("alloc_jobid", "")
                        key = f"waste:{n['name']}:{g.get('index')}:{jid}"
                        if self._ok_to_send(key, now, NAG_REALERT_SEC):
                            self._post(self._m("waste", loc=loc, kind=kind,
                                               dur=_fmt_dur(wasted, self.lang),
                                               user=g.get("alloc_user", "?"), jid=jid or "?"))
                    if self.rogue_alert and not g.get("alloc_jobid") and not g.get("alloc_user"):
                        rogues = [u for u in (g.get("users") or []) if u]
                        for u in rogues:
                            if self._ok_to_send(f"rogue:{n['name']}:{g.get('index')}:{u}",
                                                now, NAG_REALERT_SEC):
                                self._post(self._m("rogue", loc=loc, user=u))

        if self.temp_alert_c > 0 or self.ecc_alert:
            for n in nodes:
                for g in n.get("gpus", []):
                    loc = f"{n['name']} GPU{g.get('index', '?')} ({g.get('name', '?')})"
                    hw = _hw_id(g)
                    if self.temp_alert_c > 0:
                        temp = _to_int(g.get("temp"))
                        if temp is not None and temp >= self.temp_alert_c \
                                and self._ok_to_send(f"temp:{n['name']}:{g.get('index')}",
                                                     now, NAG_REALERT_SEC):
                            self._post(self._m("temp", loc=loc, temp=temp,
                                               thr=int(self.temp_alert_c), hw=hw))
                    if self.ecc_alert:
                        ecc = _to_int(g.get("ecc"))
                        if ecc and ecc > 0 \
                                and self._ok_to_send(f"ecc:{n['name']}:{g.get('index')}",
                                                     now, NAG_REALERT_SEC):
                            self._post(self._m("ecc", loc=loc, n=ecc, hw=hw))

        if self.free_gpus_min > 0:
            free = sum(1 for n in nodes for g in n.get("gpus", []) if _gpu_is_free(g))
            if free >= self.free_gpus_min:
                if self._free_was_below and self._ok_to_send("free_gpus", now):
                    self._post(self._m("free", free=free))
                self._free_was_below = False
            else:
                self._free_was_below = True

        self._save()

    def _ok_to_send(self, key: str, now: float, min_gap: float = DEBOUNCE_SEC) -> bool:
        if now - self._last_sent.get(key, 0) < min_gap:
            return False
        self._last_sent[key] = now
        return True

    def _post(self, text: str) -> None:
        stamp = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        body = f"{text}\n_{self._origin} · {stamp}_"

        def worker() -> None:
            try:
                if self._bot_mode:
                    self._post_bot(body)
                else:
                    req = urllib.request.Request(
                        self.url,
                        data=json.dumps({
                            "text": body,
                            # honored by legacy incoming webhooks, ignored by
                            # app-scoped ones (those show the app's own name)
                            "username": self.sender,
                            "icon_emoji": ":robot_face:",
                        }).encode(),
                        headers={"Content-Type": "application/json"})
                    urllib.request.urlopen(req, timeout=10).read()
            except Exception as e:
                print(f"[notify] post failed: {e}", flush=True)

        threading.Thread(target=worker, daemon=True, name="webhook").start()

    def _post_bot(self, body: str) -> None:
        """chat.postMessage as a reply under today's parent message."""
        thread_ts = self._ensure_daily_parent()
        payload = {"channel": self.channel, "text": body}
        if thread_ts:
            payload["thread_ts"] = thread_ts
        self._slack_api("chat.postMessage", payload)

    def _ensure_daily_parent(self) -> str:
        """Return today's thread parent ts, creating the parent if needed.
        Serialized so a burst of alerts doesn't spawn duplicate parents."""
        today = datetime.now().strftime("%Y-%m-%d")
        with self._post_lock:
            if self._thread_day == today and self._thread_ts:
                return self._thread_ts
            resp = self._slack_api("chat.postMessage", {
                "channel": self.channel,
                "text": self._m("parent", date=today, sender=self.sender),
            })
            ts = (resp or {}).get("ts", "")
            if ts:
                self._thread_day, self._thread_ts = today, ts
                self._save()
            return ts

    def _slack_api(self, method: str, payload: dict) -> Optional[dict]:
        try:
            req = urllib.request.Request(
                f"{SLACK_API_BASE}/{method}",
                data=json.dumps(payload).encode(),
                headers={"Content-Type": "application/json; charset=utf-8",
                         "Authorization": f"Bearer {self.bot_token}"})
            resp = json.loads(urllib.request.urlopen(req, timeout=10).read())
            if not resp.get("ok"):
                print(f"[notify] slack {method} error: {resp.get('error')}", flush=True)
            return resp
        except Exception as e:
            print(f"[notify] slack {method} failed: {e}", flush=True)
            return None

    def _save(self) -> None:
        try:
            tmp = self._state_file.with_suffix(".tmp")
            tmp.write_text(json.dumps({
                "down": self._down, "jobs": self._jobs,
                "last_sent": self._last_sent,
                "free_was_below": self._free_was_below,
                "thread_day": self._thread_day, "thread_ts": self._thread_ts,
            }))
            tmp.rename(self._state_file)
        except OSError:
            pass
