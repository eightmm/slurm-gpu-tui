"""Rich cell/formatting helpers for the sgpu TUI and CLI."""
from __future__ import annotations

import os
import time
from datetime import datetime
from typing import List

from rich.cells import cell_len
from rich.text import Text

from .common import GpuInfo, NodeInfo, ROGUE_IGNORE
from .telemetry import metric_number
from .node_telemetry import clean_sample, TELEMETRY_MAX_AGE

# ── Rich visual helpers ───────────────────────────────────────────────────

def mb_to_gb(val: str) -> str:
    try:
        return f"{float(val) / 1024:.1f}"
    except (ValueError, TypeError):
        return val


def pct_color(pct: float) -> str:
    if pct >= 0.90:
        return "red"
    if pct >= 0.60:
        return "yellow"
    return "green"


def make_bar(pct: float, width: int = 20) -> Text:
    if metric_number(pct) is None:
        return Text("?", style="bright_black")
    filled = int(min(1.0, max(0.0, pct)) * width)
    empty = width - filled
    color = pct_color(pct)
    bar = Text()
    bar.append("█" * filled, style=f"bold {color}")
    bar.append("░" * empty, style="bright_black")
    return bar


def util_cell(util_str: str) -> Text:
    if not util_str:
        return Text("-", style="bright_black")
    val = metric_number(util_str)
    if val is None:
        return Text(f"{util_str}%", style="bright_black")
    pct = val / 100.0
    bar = make_bar(pct, width=10)
    label = Text(f" {val:3.0f}%", style=f"bold {pct_color(pct)}")
    return bar + label


def vram_cell(used_str: str, total_str: str) -> Text:
    try:
        t_gb = float(total_str) / 1024
    except (ValueError, TypeError):
        t_gb = 0.0
    try:
        u_gb = float(used_str) / 1024
        if t_gb <= 0:
            return Text("?")
        pct = u_gb / t_gb
    except (ValueError, TypeError):
        # no live reading — still show the capacity when inventory knows it
        return Text(f"-/{t_gb:.0f}G" if t_gb > 0 else "-", style="bright_black")
    bar = make_bar(pct, width=12)
    label = Text(f" {u_gb:.1f}/{t_gb:.0f}G {pct:.0%}", style=f"bold {pct_color(pct)}")
    return bar + label


def temp_cell(temp_str: str) -> Text:
    if not temp_str:
        return Text("-", style="bright_black")
    try:
        t = float(temp_str)
    except (ValueError, TypeError):
        return Text(f"{temp_str}C", style="bright_black")
    if t >= 85:
        return Text(f"{t:.0f}C HOT", style="bold red")
    if t >= 70:
        return Text(f"{t:.0f}C", style="bold yellow")
    return Text(f"{t:.0f}C", style="green")


def power_cell(power_str: str, cap_str: str) -> Text:
    if not power_str:
        return Text("-", style="bright_black")
    try:
        pw = float(power_str)
        cap = float(cap_str)
        pct = pw / cap if cap > 0 else 0
    except (ValueError, TypeError):
        return Text(f"{power_str}W", style="bright_black")
    bar = make_bar(pct, width=6)
    label = Text(f" {pw:.0f}/{cap:.0f}W", style=f"{pct_color(pct)}")
    return bar + label


# Long slurm state names get cut by the column — shorten, keep meaning
_STATE_SHORT = {"allocated": "alloc", "draining": "drain↓", "drained": "drain",
                "completing": "compl", "reserved": "resvd"}


def state_cell(state: str) -> Text:
    s = state.lower()
    disp = state
    for long, short in _STATE_SHORT.items():
        disp = disp.replace(long, short)
    if any(word in s for word in ("down", "fail")):
        return Text(f"✖ {disp}", style="bold red")
    if any(word in s for word in ("drain", "maint", "unknown")):
        return Text(f"! {disp}", style="yellow")
    return Text(disp, style="dim")


def ellipsize(s: str, n: int = 20) -> str:
    """Truncate to n display cells (CJK chars are 2 cells wide)."""
    if cell_len(s) <= n:
        return s
    out = ""
    for ch in s:
        if cell_len(out + ch) > n - 1:
            break
        out += ch
    return out + "…"


def highlight_row(cells: list) -> list:
    """Apply 'My Jobs' background highlight to all cells in a row."""
    for cell in cells:
        if isinstance(cell, Text):
            cell.stylize("on #1a1a3a")
    return cells


def mem_cell(node: NodeInfo) -> Text:
    """Memory bar. Sources, best first: OS meminfo from the node's agent/SSH
    payload, sinfo FreeMem, then slurm AllocMem ('~' prefix — allocation, not
    live usage — but always available, e.g. Ubuntu slurm without FreeMem)."""
    try:
        total_mb = float(node.mem_total)
    except (ValueError, TypeError):
        return Text("?")
    used_mb = None
    approx = False
    for src in (node.mem_avail, node.mem_free):
        try:
            used_mb = total_mb - float(src)
            break
        except (ValueError, TypeError):
            continue
    if used_mb is None:
        try:
            used_mb = float(node.mem_alloc)
            approx = True
        except (ValueError, TypeError):
            return Text(f"-/{total_mb / 1024:.0f}G", style="bright_black")
    # sinfo free_mem can exceed configured total; clamp to sane range
    used_mb = min(max(used_mb, 0.0), total_mb)
    pct = used_mb / total_mb if total_mb > 0 else 0
    cell = make_bar(pct, width=6)
    prefix = "~" if approx else ""
    cell.append(f" {prefix}{used_mb / 1024:.0f}/{total_mb / 1024:.0f}G {pct:.0%}",
                style=f"{pct_color(pct)}")
    return cell


def parse_slurm_duration(s: str) -> int:
    """Parse SLURM duration string to total seconds. Returns -1 on failure."""
    if not s or s in ("N/A", "UNLIMITED", "Partition_Limit"):
        return -1
    try:
        days = 0
        if "-" in s:
            d, s = s.split("-", 1)
            days = int(d)
        parts = s.split(":")
        if len(parts) == 3:
            h, m, sec = int(parts[0]), int(parts[1]), int(parts[2])
        elif len(parts) == 2:
            h, m, sec = 0, int(parts[0]), int(parts[1])
        else:
            return -1
        return days * 86400 + h * 3600 + m * 60 + sec
    except (ValueError, IndexError):
        return -1


def classify_gpu(g: GpuInfo) -> str:
    """One of: rogue (GPU process outside any SLURM allocation) / busy /
    parked (VRAM held, no compute) / idle (reserved, no process) / free /
    unknown (no data yet)."""
    real_users = [u for u in g.users if u not in ROGUE_IGNORE]
    if real_users and not g.alloc_jobid:
        return "rogue"
    util = metric_number(g.util)
    if util is None:
        return "unknown"
    if util > 5:
        return "busy"
    try:
        total = float(g.mem_total)
        vram_pct = float(g.mem_used) / total if total > 0 else 0.0
    except (ValueError, TypeError):
        vram_pct = 0.0
    if vram_pct >= 0.3:
        return "parked"
    if g.alloc_jobid or real_users:
        return "idle"
    return "free"


_CLASS_GLYPH = {
    "busy": ("█", "green"),
    "parked": ("▅", "blue"),
    "idle": ("▂", "yellow"),
    "free": ("▁", "bold cyan"),
    "rogue": ("!", "bold red"),
    "unknown": ("?", "bright_black"),
}


def node_schedulable(node: NodeInfo) -> bool:
    return not (node.stale or node.error or not node.scheduler_available or node.scheduler_age_sec > 30
                or any(state in node.state.lower() for state in ("down", "drain", "fail", "maint", "unknown")))


def node_gpu_classes(node: NodeInfo) -> List[str]:
    if node.stale:
        return ["unknown"] * len(node.gpus)
    classes = [classify_gpu(gpu) for gpu in node.gpus]
    return ["unknown" if kind == "free" and (not node_schedulable(node) or gpu_health_status(gpu)[1] == 2) else kind
            for gpu, kind in zip(node.gpus, classes, strict=True)]


def gpu_health_status(gpu: GpuInfo, now: float | None = None) -> tuple[str, int]:
    now = time.time() if now is None else now
    flags, severity = [], 0
    if gpu.health_observed_at > 0 and -5 <= now - gpu.health_observed_at <= TELEMETRY_MAX_AGE:
        action = gpu.recovery_action.lower()
        if action and action != "none":
            flags.append("REBOOT" if action == "reboot" else "RESET" if action == "reset" else "DRAIN")
            severity = 2
        if "thermal" in gpu.clock_reasons:
            flags.append("THERMAL")
            severity = 2
        elif any(reason in gpu.clock_reasons for reason in ("hw-slowdown", "power-brake")):
            flags.append("SLOW")
            severity = max(severity, 1)
        elif "power-cap" in gpu.clock_reasons:
            flags.append("CAP")
    temperature, ecc = metric_number(gpu.temp), metric_number(gpu.ecc)
    if temperature is not None and temperature >= 85:
        flags.append("HOT")
        severity = max(severity, 1)
    if ecc is not None and ecc > 0:
        flags.append(f"ECC:{ecc:.0f}")
        severity = max(severity, 1)
    known = temperature is not None or ecc is not None or (gpu.health_observed_at > 0 and -5 <= now - gpu.health_observed_at <= TELEMETRY_MAX_AGE)
    return "/".join(flags) or ("OK" if known else "—"), severity


def pending_wait_seconds(job, now: float | None = None) -> float | None:
    try:
        stamp = datetime.fromisoformat(job.submit_time).timestamp()
    except (ValueError, TypeError):
        return None
    return max(0, (time.time() if now is None else now) - stamp)


def pending_reason(job) -> str:
    if job.reason in ("Dependency", "DependencyNeverSatisfied"):
        dependency = job.dependency if job.dependency not in ("", "(null)", "None") else ""
        return job.reason + (f" {dependency}" if dependency else "")
    if job.reason.startswith(("QOS", "Assoc")) and job.qos:
        return f"{job.reason} [{job.qos}]"
    return job.reason or "unknown"


def pending_explanation(job) -> str:
    explanations = {
        "Resources": "Requested resources are not currently available.",
        "Priority": "Higher-priority jobs are ahead in scheduling consideration.",
        "Dependency": "Waiting for the specified job dependencies.",
        "DependencyNeverSatisfied": "A dependency cannot be satisfied.",
        "BeginTime": "The requested begin time has not arrived.",
        "ReqNodeNotAvail": "A requested node is unavailable or reserved.",
        "PartitionTimeLimit": "The time request exceeds the partition limit.",
        "PartitionNodeLimit": "The node request does not fit the partition limits.",
    }
    if job.reason.startswith(("QOS", "Assoc")):
        return "A QOS or account policy limit blocks scheduling; inspect the exact reason and QOS."
    return explanations.get(job.reason, "Inspect the scheduler's exact reason below.")


def job_resource_summary(job, nodes, now: float | None = None) -> dict:
    from .common import _strict_expand_nodes
    now = time.time() if now is None else now
    expected = set(_strict_expand_nodes(job.node) or ())
    live = {node.name for node in nodes if not node.stale and not node.error and node.scheduler_available and node.scheduler_age_sec <= 30}
    samples = {}
    if isinstance(job.telemetry, dict):
        for name, value in job.telemetry.items():
            sample = clean_sample(value)
            stamp = sample.get("observed_at", 0)
            if len(sample) > 1 and name in expected & live and stamp > 0 and -5 <= now - stamp <= TELEMETRY_MAX_AGE:
                samples[name] = sample
    result = {"sampled_nodes": len(samples), "expected_nodes": len(expected)}
    for key in ("cpu_cores", "mem_current_mib", "mem_peak_mib", "mem_limit_mib", "oom_kill", "vram_mib"):
        values = [sample[key] for sample in samples.values() if key in sample]
        if values:
            result[key] = sum(values)
        result[key + "_nodes"] = len(values)
    return result


def gpu_strip(classes: List[str]) -> Text:
    """One glyph per GPU: at-a-glance node state, useful when collapsed."""
    strip = Text()
    for c in classes:
        ch, style = _CLASS_GLYPH.get(c, ("?", "bright_black"))
        strip.append(ch, style=style)
    return strip


def fmt_span(sec: int) -> str:
    """'3.2h' / '12m' / '' for sub-minute durations."""
    if sec >= 3600:
        return f"{sec / 3600:.1f}h"
    if sec >= 60:
        return f"{sec // 60}m"
    return ""


def fmt_idle_age(sec: int) -> str:
    """'idle 3.2h' / 'idle 12m' / 'idle' for short/unknown durations."""
    span = fmt_span(sec)
    return f"idle {span}" if span else "idle"


def collect_waste(nodes: List[NodeInfo], min_sec: int) -> List[dict]:
    """Idle/parked offenders over the threshold, worst first."""
    rows: List[dict] = []
    for n in nodes:
        for g in n.gpus:
            real_users = [u for u in g.users if u not in ROGUE_IGNORE]
            if real_users and not g.alloc_jobid:
                # Link to the culprit job when it's a gres-less SLURM job
                jid = next((j.jobid for j in n.jobs
                            if j.user in real_users and j.gpu_count == 0), "")
                rows.append({
                    "node": n.name, "gpu": g.index,
                    "kind": "no-gres" if jid else "rogue",
                    "sec": 0, "user": ",".join(real_users), "jobid": jid,
                })
            elif g.idle_sec >= min_sec:
                rows.append({
                    "node": n.name, "gpu": g.index, "kind": "idle",
                    "sec": g.idle_sec, "user": g.alloc_user,
                    "jobid": g.alloc_jobid,
                })
            elif g.parked_sec >= min_sec:
                rows.append({
                    "node": n.name, "gpu": g.index, "kind": "parked",
                    "sec": g.parked_sec,
                    "user": g.alloc_user or ",".join(g.users),
                    "jobid": g.alloc_jobid,
                })
    # unauthorized GPU use first (rogue / gres-less job), then worst waste
    rows.sort(key=lambda r: (r["kind"] not in ("rogue", "no-gres"), -r["sec"]))
    return rows


WASTE_MIN_SEC = int(os.getenv("SLURM_GPU_TUI_WASTE_MIN_SEC", "600"))


def _waste_thr() -> str:
    return fmt_span(WASTE_MIN_SEC) or f"{WASTE_MIN_SEC}s"


def fmt_start_time(s: str) -> str:
    """Compact squeue %S estimate: '15:00' today, '07-08 15:00' otherwise."""
    if not s or s in ("N/A", "(null)"):
        return ""
    try:
        dt = datetime.fromisoformat(s)
    except ValueError:
        return s
    if dt.date() == datetime.now().date():
        return dt.strftime("%H:%M")
    return dt.strftime("%m-%d %H:%M")


def remaining_cell(elapsed: str, time_limit: str) -> Text:
    """Show time remaining in job. Red <1h, yellow <12h, green otherwise."""
    elapsed_s = parse_slurm_duration(elapsed)
    limit_s = parse_slurm_duration(time_limit)
    if elapsed_s < 0 or limit_s < 0:
        return Text(elapsed or "", style="dim")
    remaining_s = limit_s - elapsed_s
    if remaining_s < 0:
        remaining_s = 0
    h = remaining_s // 3600
    m = (remaining_s % 3600) // 60
    label = f"{h}:{m:02d}h" if h > 0 else f"{m}m"
    if remaining_s < 3600:
        return Text(label, style="bold red")
    elif remaining_s < 43200:
        return Text(label, style="yellow")
    else:
        return Text(label, style="dim green")
