"""Slack bot notifications, driven by the collector's per-cycle snapshot.

Config: ~/.sgpu/webhook.json
{
  "bot_token": "xoxb-...",        # Slack bot token; alerts become
  "channel": "#gpu-cluster",      #   replies under one parent message per day
                                  #   (needs chat:write and a bot invited to it)
  "sender_name": "AI-master",     # identity in the alert footer/username
  "lang": "en",                   # alert language: "en" or "ko"
  "node_health": true,            # node down/recovered alerts (SLURM state)
  "down_grace_sec": 180,          # down must persist this long before alerting
  "collect_alert": true,          # GPU node reporting fine then goes blind
  "collect_grace_sec": 600,       #   (agent died / node hung) — SLURM still up
  "waste_alert_hours": 2,         # GPU idle/parked >= N hours (0 = off)
  "rogue_alert": true,            # GPU used outside SLURM
  "rogue_grace_sec": 300,         #   must persist this long before alerting
  "temp_alert_c": 0,              # GPU temperature >= N°C (0 = off; ~90 typical)
  "ecc_alert": true,             # uncorrectable ECC errors (silent HW failure)
  "job_done_users": ["alice"],    # notify when these users' jobs finish
  "job_fail_users": ["*"],        # FAILED/OOM/TIMEOUT alerts; ["*"] = everyone
                                  #   (owner's DM carries the stderr tail when
                                  #   readable — never the shared channel)
  "pending_alert_hours": 0,       # job stuck PENDING >= N hours (0 = off;
                                  #   user holds/dependencies never alert)
  "dm_users": {"alice": "U012AB"},# per-user Slack DMs (member id) for alerts
                                  #   about their own jobs (bot mode only)
  "free_gpus_min": 0,             # alert when free-GPU count reaches N (0 = off)
  "mem_fair_factor": 0            # RAM hog alerts: job RAM > factor × its GPU
                                  #   fair share (node RAM × job GPUs / node
                                  #   GPUs). 1.0 = strict, 0 = off
}

No bot token/channel -> notifier is inert. POSTs run in a daemon thread so a
slow Slack API call never blocks the collect loop. State persists
so restarts don't re-fire old alerts.
"""
from __future__ import annotations

import json
import os
import queue
import socket
import threading
import time
import urllib.error
import urllib.request
from datetime import datetime
from pathlib import Path
from typing import Dict, List, Optional

from .common import ROGUE_IGNORE, job_log_paths, mem_to_mib, run_cmd, tail_file

DEBOUNCE_SEC = int(os.getenv(
    "SLURM_GPU_TUI_SLACK_DEBOUNCE_SEC",
    os.getenv("SLURM_GPU_TUI_WEBHOOK_DEBOUNCE_SEC", "1800"),
))
# waste/rogue conditions persist for hours — re-nag much less often
NAG_REALERT_SEC = int(os.getenv(
    "SLURM_GPU_TUI_SLACK_NAG_SEC",
    os.getenv("SLURM_GPU_TUI_WEBHOOK_NAG_SEC", "21600"),
))
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
        "collect_lost": ":electric_plug: *{name}: sgpu can't collect GPU data* — agent/SSH down, node still {state} in SLURM",
        "collect_ok": ":arrows_counterclockwise: *{name}: sgpu collection restored* after {dur}",
        "job_fail": ":x: *job {jid} ({name}) by {user} {state}* after {elapsed}",
        "pend_stuck": ":hourglass_flowing_sand: *job {jid} ({name}) by {user} pending {dur}* — {reason}",
        "mem_hog": ":pig: *job {jid} ({name}) by {user} holds {mem}G RAM on {node}* — "
                   "fair share for {gpus} GPU(s) is {share}G",
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
        "collect_lost": ":electric_plug: *{name}: sgpu GPU 데이터 수집 불가* — 에이전트/SSH 끊김, SLURM상 {state}",
        "collect_ok": ":arrows_counterclockwise: *{name}: sgpu 수집 복구* — {dur} 만에",
        "job_fail": ":x: *작업 {jid} ({name}, {user}) {state}* — {elapsed} 경과",
        "pend_stuck": ":hourglass_flowing_sand: *작업 {jid} ({name}, {user}) {dur}째 대기* — {reason}",
        "mem_hog": ":pig: *작업 {jid} ({name}, {user}) — {node}에서 RAM {mem}G 점유* — "
                   "GPU {gpus}장 공정 몫은 {share}G",
        "idle": "유휴", "parked": "점유",
    },
}

# job outcomes worth a failure alert (sacct State prefixes)
_FAIL_STATES = ("FAILED", "OUT_OF_MEMORY", "TIMEOUT", "NODE_FAIL")
# pending reasons that are the user's own doing — waiting is expected
_PEND_QUIET = ("held", "dependency", "begintime", "jobarraytasklimit")


def _gpu_is_free(g: dict) -> bool:
    return not g.get("alloc_jobid") and not g.get("alloc_user") and not g.get("users")


def _to_int(v) -> Optional[int]:
    """nvidia-smi numeric field -> int, or None for N/A / non-numeric."""
    try:
        return int(str(v).strip())
    except (ValueError, TypeError, AttributeError):
        return None


def _hw_id(g: dict) -> str:
    """Physical-identity line for a GPU (slot / device node / UUID / PCI bus /
    serial), skipping fields the driver reports as N/A. Slot and /dev/nvidiaN
    come first — that's what someone standing at the machine needs."""
    def ok(v: str) -> bool:
        return bool(v) and "N/A" not in v and "Not Supported" not in v
    parts = []
    if ok(g.get("slot", "")):
        parts.append(f"slot {g['slot']}")
    if ok(g.get("minor", "")):
        parts.append(f"/dev/nvidia{g['minor']}")
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
    def __init__(self, state_dir: Path, cfg_path: Optional[Path] = None) -> None:
        self._state_file = state_dir / "notify_state.json"
        self._cfg_path = cfg_path or Path.home() / ".sgpu" / "webhook.json"
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
        self._pend_seen: Dict[str, float] = st.get("pend_seen", {})
        self._last_sent: Dict[str, float] = st.get("last_sent", {})
        self._free_was_below = bool(st.get("free_was_below", True))
        # daily thread parent (bot mode): reuse across restarts within a day
        self._thread_day: str = st.get("thread_day", "")
        self._thread_ts: str = st.get("thread_ts", "")
        self._post_lock = threading.Lock()
        self._state_lock = threading.Lock()
        # single consumer thread: preserves alert order (down before recovered)
        # and stops a hung Slack call from stacking one thread per alert
        self._queue: "queue.Queue[tuple[str, str, float, str]]" = queue.Queue(maxsize=200)
        self._consumer: Optional[threading.Thread] = None
        self._parent_worker: Optional[threading.Thread] = None
        self._parent_fail_ts = 0.0
        self._save_failed = False
        self._blind: Dict[str, float] = st.get("blind", {})
        # first-seen ts of an unconfirmed down/blind; deliberately NOT persisted
        # so a collector restart re-starts the grace clock (cold start looks
        # exactly like an outage for the first cycles)
        self._pending_down: Dict[str, float] = {}
        self._pending_blind: Dict[str, float] = {}
        self._pending_rogue: Dict[str, float] = {}

    def _load_config(self) -> None:
        cfg: dict = {}
        try:
            self._cfg_mtime = self._cfg_path.stat().st_mtime
            cfg = json.loads(self._cfg_path.read_text())
        except (OSError, ValueError):
            self._cfg_mtime = None
        # Threads alerts under one parent per day (needs chat:write).
        self.bot_token: str = cfg.get("bot_token", "") or os.getenv("SLURM_GPU_TUI_SLACK_BOT_TOKEN", "")
        self.channel: str = cfg.get("channel", "")
        self.node_health: bool = bool(cfg.get("node_health", True))
        self.collect_alert: bool = bool(cfg.get("collect_alert", True))
        self.collect_grace_sec: float = float(cfg.get("collect_grace_sec", 600))
        self.job_done_users: List[str] = list(cfg.get("job_done_users", []))
        # job failure alerts: list of users, or ["*"] for everyone
        self.job_fail_users: List[str] = list(cfg.get("job_fail_users", []))
        # job stuck in the queue (scheduler wait, not user holds) >= N hours
        self.pending_alert_hours: float = float(cfg.get("pending_alert_hours", 0))
        # personal Slack DMs: {"login": "U0123ABC"} member ids
        self.dm_users: Dict[str, str] = dict(cfg.get("dm_users", {}))
        self.free_gpus_min: int = int(cfg.get("free_gpus_min", 0))
        # RAM hogs: alert when a job's requested RAM exceeds factor × its GPU
        # fair share (node RAM × job GPUs / node GPUs). 0 = off.
        self.mem_fair_factor: float = float(cfg.get("mem_fair_factor", 0))
        self.waste_alert_hours: float = float(cfg.get("waste_alert_hours", 0))
        self.rogue_alert: bool = bool(cfg.get("rogue_alert", False))
        self.rogue_grace_sec: float = float(cfg.get("rogue_grace_sec", 300))
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
        """Pick up Slack config edits without a collector restart."""
        try:
            mtime = self._cfg_path.stat().st_mtime
        except OSError:
            mtime = None
        if mtime != self._cfg_mtime:
            self._load_config()
            print(f"[notify] Slack config reloaded (enabled={self.enabled})", flush=True)

    def _m(self, key: str, **kw) -> str:
        return MSG.get(self.lang, MSG["en"])[key].format(**kw)

    @property
    def enabled(self) -> bool:
        return bool(self.bot_token and self.channel)

    def process(self, data: dict) -> None:
        """Diff one collector snapshot against remembered state; fire alerts."""
        self._maybe_reload()
        if not self.enabled:
            return
        self._maybe_start_daily_parent()
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
                        self._post(self._m("down", name=name, why=why[:120],
                                           detail=detail), key=f"down:{name}")
                        self._down[name] = first
                else:
                    self._pending_down.pop(name, None)
                    if confirmed:
                        self._post(self._m("recovered", name=name,
                                           dur=_fmt_dur(now - confirmed, self.lang)))
                    self._down[name] = 0

        if self.collect_alert:
            for n in nodes:
                name = n["name"]
                state = n.get("state", "").lower()
                slurm_down = any(s in state for s in ("down", "drain", "fail", "err", "boot"))
                # only GPU nodes (CPU/GPU-less nodes are commonly unreachable
                # by design in SSH-pull mode), and only when SLURM says the
                # node is UP — a real outage is node_health's job, not this
                blind = (n.get("has_gpu", True)
                         and (bool(n.get("stale")) or bool(n.get("error")))
                         and not slurm_down)
                was = self._blind.get(name, 0)
                confirmed = float(was) if not isinstance(was, bool) else (now if was else 0)
                if blind:
                    first = self._pending_blind.setdefault(name, now)
                    if not confirmed and now - first >= self.collect_grace_sec \
                            and self._ok_to_send(f"blind:{name}", now, NAG_REALERT_SEC):
                        self._post(self._m("collect_lost", name=name,
                                           state=n.get("state", "?")),
                                   key=f"blind:{name}")
                        self._blind[name] = first
                else:
                    self._pending_blind.pop(name, None)
                    if confirmed:
                        self._post(self._m("collect_ok", name=name,
                                           dur=_fmt_dur(now - confirmed, self.lang)))
                    self._blind[name] = 0

        # A failed squeue/controller query yields an empty jobs list — diffing
        # against it would fire a false "finished" for every tracked job (and
        # wipe _jobs so it can't recover). Only diff when the snapshot is clean.
        fail_all = "*" in self.job_fail_users
        if (self.job_done_users or self.job_fail_users) and not data.get("errors"):
            def _watched(u: str) -> bool:
                return fail_all or u in self.job_done_users or u in self.job_fail_users
            current = {j["jobid"]: j for j in data.get("jobs", [])
                       if _watched(j.get("user", ""))}
            for jid, j in self._jobs.items():
                if jid in current:
                    continue
                user = j.get("user", "?")
                state = ""
                if fail_all or user in self.job_fail_users:
                    state = self._job_final_state(jid)
                if state.startswith(_FAIL_STATES):
                    text = self._m("job_fail", jid=jid, name=j.get("jobname", "?"),
                                   user=user, state=state,
                                   elapsed=j.get("elapsed", "?"))
                    self._post(text)
                    # stderr tail goes to the owner's DM only — the channel
                    # is everyone, and stderr can leak paths/secrets (the
                    # collector may read files the audience can't)
                    dm_text = text
                    if self.dm_users.get(user):
                        tail = self._fail_log_tail(jid)
                        if tail:
                            dm_text += f"\n```{tail}```"
                    self._post_dm(user, dm_text)
                elif user in self.job_done_users:
                    text = self._m("job_done", jid=jid, name=j.get("jobname", "?"),
                                   user=user, elapsed=j.get("elapsed", "?"))
                    self._post(text)
                    self._post_dm(user, text)
            self._jobs = {jid: {"jobname": j.get("jobname", ""), "user": j.get("user", ""),
                                "elapsed": j.get("elapsed", "")} for jid, j in current.items()}

        # A job stuck PENDING on the scheduler (not a user hold/dependency)
        # for hours usually means an impossible request or a starved queue.
        if self.pending_alert_hours > 0 and not data.get("errors"):
            pend_now: Dict[str, float] = {}
            for p in data.get("pending", []):
                jid = p.get("jobid", "")
                if not jid:
                    continue
                if any(r in (p.get("reason") or "").lower() for r in _PEND_QUIET):
                    continue
                first = self._pend_seen.get(jid, now)
                pend_now[jid] = first
                if now - first >= self.pending_alert_hours * 3600 \
                        and self._ok_to_send(f"pend:{jid}", now, NAG_REALERT_SEC):
                    text = self._m("pend_stuck", jid=jid, name=p.get("jobname", "?"),
                                   user=p.get("user", "?"),
                                   dur=_fmt_dur(now - first, self.lang),
                                   reason=p.get("reason", "?"))
                    self._post(text, key=f"pend:{jid}")
                    self._post_dm(p.get("user", ""), text)
            self._pend_seen = pend_now

        # RAM hogs: requested memory beyond the job's GPU-count fair share.
        # Allocation-based (squeue %m), so it flags greedy --mem requests even
        # before the job actually touches that memory.
        if self.mem_fair_factor > 0 and not data.get("errors"):
            node_ram = {n["name"]: _to_int(n.get("mem_total")) for n in nodes}
            node_gpus = {n["name"]: len(n.get("gpus", [])) for n in nodes}
            for j in data.get("jobs", []):
                gpus = j.get("gpu_count", 0)
                node = j.get("node", "")
                if not gpus or not node_ram.get(node) or not node_gpus.get(node):
                    continue
                mem_mib = mem_to_mib(j.get("mem", ""), int(j.get("cpu_count") or 1))
                share = node_ram[node] * gpus / node_gpus[node]
                if mem_mib and share > 0 and mem_mib > self.mem_fair_factor * share:
                    key = f"memhog:{j.get('jobid', '')}"
                    if self._ok_to_send(key, now, NAG_REALERT_SEC):
                        text = self._m("mem_hog", jid=j.get("jobid", "?"),
                                       name=j.get("jobname", "?"),
                                       user=j.get("user", "?"), node=node,
                                       mem=f"{mem_mib / 1024:.0f}",
                                       share=f"{share / 1024:.0f}", gpus=gpus)
                        self._post(text, key=key)
                        self._post_dm(j.get("user", ""), text)

        if self.waste_alert_hours > 0 or self.rogue_alert:
            # Rogue needs trustworthy allocation data: a failed controller
            # query means empty gpu_alloc (every busy GPU would look rogue),
            # and a stale node payload lists processes from before jobs ended.
            rogue_ok = self.rogue_alert and not data.get("errors")
            active_rogue: set = set()
            for n in nodes:
                node_rogue_ok = rogue_ok and not n.get("stale")
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
                                               user=g.get("alloc_user", "?"),
                                               jid=jid or "?"), key=key)
                    if node_rogue_ok and not g.get("alloc_jobid") and not g.get("alloc_user"):
                        rogues = [u for u in (g.get("users") or [])
                                  if u and u not in ROGUE_IGNORE]
                        for u in rogues:
                            key = f"rogue:{n['name']}:{g.get('index')}:{u}"
                            active_rogue.add(key)
                            first = self._pending_rogue.setdefault(key, now)
                            # grace: job start/end races and transient scontrol
                            # gaps look identical to real off-SLURM use
                            if now - first >= self.rogue_grace_sec \
                                    and self._ok_to_send(key, now, NAG_REALERT_SEC):
                                self._post(self._m("rogue", loc=loc, user=u), key=key)
            if rogue_ok:
                # condition cleared (or node data untrusted) → restart clock
                for key in list(self._pending_rogue):
                    if key not in active_rogue:
                        del self._pending_rogue[key]

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
                                               thr=int(self.temp_alert_c), hw=hw),
                                       key=f"temp:{n['name']}:{g.get('index')}")
                    if self.ecc_alert:
                        ecc = _to_int(g.get("ecc"))
                        if ecc and ecc > 0 \
                                and self._ok_to_send(f"ecc:{n['name']}:{g.get('index')}",
                                                     now, NAG_REALERT_SEC):
                            self._post(self._m("ecc", loc=loc, n=ecc, hw=hw),
                                       key=f"ecc:{n['name']}:{g.get('index')}")

        if self.free_gpus_min > 0:
            free = sum(1 for n in nodes for g in n.get("gpus", []) if _gpu_is_free(g))
            if free >= self.free_gpus_min:
                if self._free_was_below and self._ok_to_send("free_gpus", now):
                    self._post(self._m("free", free=free), key="free_gpus")
                self._free_was_below = False
            else:
                self._free_was_below = True

        self._save()

    def _fail_log_tail(self, jid: str, max_lines: int = 15) -> str:
        """Last lines of a failed job's stderr (stdout when merged). Best
        effort: scontrol still knows the log paths for ~MinJobAge after the
        job ends; after that fall back to the default slurm-<jid>.out in the
        job's WorkDir. '' when nothing is readable."""
        path = ""
        ok, out = run_cmd(f"scontrol show job {jid}", timeout=10)
        if ok and "JobId=" in out:
            stdout_path, stderr_path = job_log_paths(out)
            path = stderr_path or stdout_path
        if not path:
            ok, wd = run_cmd(f"sacct -j {jid} -X -n -o WorkDir --parsable2", timeout=10)
            if ok and wd.strip():
                cand = os.path.join(wd.strip().splitlines()[0], f"slurm-{jid}.out")
                if os.path.exists(cand):
                    path = cand
        if not path:
            return ""
        text = tail_file(path, limit=8192)
        if text.startswith("("):  # missing / unreadable / empty
            return ""
        lines = text.splitlines()
        if lines and lines[0].startswith("…"):  # tail_file's truncation banner
            lines = lines[1:]
        return "\n".join(lines[-max_lines:]).strip()

    def _job_final_state(self, jid: str) -> str:
        """Outcome of a finished job from slurmdbd ('' when sacct is absent)."""
        ok, out = run_cmd(f"sacct -j {jid} -X -n -o State --parsable2", timeout=10)
        if not ok or not out.strip():
            return ""
        return out.strip().splitlines()[0].split()[0]  # "CANCELLED by 1234" -> CANCELLED

    def _post_dm(self, user: str, text: str) -> None:
        """Also deliver an alert as a Slack DM to the user it concerns.
        Needs a dm_users mapping; silently skipped otherwise."""
        member_id = self.dm_users.get(user, "")
        if member_id and self.enabled:
            self._post(text, channel=member_id)

    def _ok_to_send(self, key: str, now: float, min_gap: float = DEBOUNCE_SEC) -> bool:
        """Debounce check. Marks the slot immediately so the next collect
        cycle doesn't enqueue a duplicate, but the consumer rolls the mark
        back if delivery ultimately fails, so the alert can re-fire."""
        if now - self._last_sent.get(key, 0) < min_gap:
            return False
        self._last_sent[key] = now
        return True

    def _post(self, text: str, key: str = "", channel: str = "") -> None:
        stamp = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        body = f"{text}\n_{self._origin} · {stamp}_"
        if self._consumer is None or not self._consumer.is_alive():
            self._consumer = threading.Thread(target=self._drain, daemon=True,
                                              name="slack")
            self._consumer.start()
        try:
            self._queue.put_nowait((body, key, time.time(), channel))
        except queue.Full:
            print("[notify] alert queue full, dropping alert", flush=True)

    def _drain(self) -> None:
        while True:
            body, key, enq_ts, channel = self._queue.get()
            ok = self._deliver(body, channel)
            if not ok and key:
                # free the debounce slot so the condition re-alerts next cycle
                with self._state_lock:
                    if self._last_sent.get(key, 0) <= enq_ts:
                        self._last_sent.pop(key, None)
                self._save()
            self._queue.task_done()

    def _deliver(self, body: str, channel: str = "") -> bool:
        return self._post_bot(body, channel)

    def _post_bot(self, body: str, channel: str = "") -> bool:
        """chat.postMessage as a reply under today's parent message.
        A channel override (e.g. a DM member id) posts standalone instead."""
        if channel:
            return self._slack_api("chat.postMessage",
                                   {"channel": channel, "text": body}) is not None
        thread_ts = self._ensure_daily_parent()
        payload = {"channel": self.channel, "text": body}
        if thread_ts:
            payload["thread_ts"] = thread_ts
        return self._slack_api("chat.postMessage", payload) is not None

    def _ensure_daily_parent(self) -> str:
        """Return today's thread parent ts, creating the parent if needed.
        Serialized so a burst of alerts doesn't spawn duplicate parents;
        a failed create is not retried for 60s so a Slack outage doesn't
        add a parent attempt to every queued alert."""
        today = datetime.now().strftime("%Y-%m-%d")
        with self._post_lock:
            if self._thread_day == today and self._thread_ts:
                return self._thread_ts
            if time.time() - self._parent_fail_ts < 60:
                return ""
            resp = self._slack_api("chat.postMessage", {
                "channel": self.channel,
                "text": self._m("parent", date=today, sender=self.sender),
            })
            ts = (resp or {}).get("ts", "")
            if ts:
                self._thread_day, self._thread_ts = today, ts
                self._save()
            else:
                self._parent_fail_ts = time.time()
            return ts

    def _maybe_start_daily_parent(self) -> None:
        """Create today's parent even when there are no alert replies.

        The Slack request stays off the collect loop. A failed attempt uses the
        same cooldown as alert-triggered parent creation, then a later collect
        cycle retries it.
        """
        today = datetime.now().strftime("%Y-%m-%d")
        if self._thread_day == today and self._thread_ts:
            return
        if time.time() - self._parent_fail_ts < 60:
            return
        if self._parent_worker is not None and self._parent_worker.is_alive():
            return
        self._parent_worker = threading.Thread(
            target=self._ensure_daily_parent,
            daemon=True,
            name="slack-daily-parent",
        )
        self._parent_worker.start()

    def _slack_api(self, method: str, payload: dict) -> Optional[dict]:
        resp = self._http_post(
            f"{SLACK_API_BASE}/{method}", payload,
            {"Content-Type": "application/json; charset=utf-8",
             "Authorization": f"Bearer {self.bot_token}"})
        if resp is not None and not resp.get("ok"):
            print(f"[notify] slack {method} error: {resp.get('error')}", flush=True)
            return None
        return resp

    def _http_post(self, url: str, payload: dict,
                   headers: dict) -> Optional[dict]:
        """POST with bounded retry: 3 attempts, exponential backoff, honoring
        Retry-After on 429. Returns parsed JSON, or None after the last
        failure."""
        data = json.dumps(payload).encode()
        last_err: object = None
        for attempt in range(3):
            if attempt:
                time.sleep(min(2 ** attempt, 30))
            try:
                req = urllib.request.Request(url, data=data, headers=headers)
                raw = urllib.request.urlopen(req, timeout=10).read()
                try:
                    return json.loads(raw)
                except ValueError:
                    return {}
            except urllib.error.HTTPError as e:
                last_err = e
                if e.code == 429:
                    try:
                        time.sleep(min(int(e.headers.get("Retry-After", "5")), 60))
                    except ValueError:
                        time.sleep(5)
                elif e.code < 500:
                    break  # 4xx other than 429 won't heal on retry
            except Exception as e:
                last_err = e
        print(f"[notify] post failed after retries: {last_err}", flush=True)
        return None

    def _save(self) -> None:
        with self._state_lock:
            # drop expired debounce slots — per-job keys (waste:node:idx:jid)
            # otherwise accumulate forever in memory and on disk
            cutoff = time.time() - max(DEBOUNCE_SEC, NAG_REALERT_SEC) * 2
            for k in [k for k, ts in self._last_sent.items() if ts < cutoff]:
                del self._last_sent[k]
            payload = json.dumps({
                "down": self._down, "blind": self._blind, "jobs": self._jobs,
                "pend_seen": self._pend_seen,
                "last_sent": self._last_sent,
                "free_was_below": self._free_was_below,
                "thread_day": self._thread_day, "thread_ts": self._thread_ts,
            })
            # Parent creation and alert delivery run in separate workers. Keep
            # their shared temp-file write inside the lock as well as snapshot
            # construction so concurrent saves cannot rename each other's file.
            try:
                tmp = self._state_file.with_suffix(".tmp")
                tmp.write_text(payload)
                tmp.rename(self._state_file)
                self._save_failed = False
            except OSError as e:
                if not self._save_failed:  # log once, not every 3s cycle
                    print(f"[notify] state save failed: {e}", flush=True)
                    self._save_failed = True
