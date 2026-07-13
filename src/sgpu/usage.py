"""usage.json readers and the Usage-tab renderer."""
from __future__ import annotations

import json
import os
import time
from datetime import datetime, timedelta
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from rich.text import Text

from .cells import _waste_thr

_DATA_DIR = Path(os.getenv("SLURM_GPU_TUI_DATA_DIR", "/tmp/slurm-gpu-tui"))

def render_usage(days: int = 7) -> Text:
    """Per-user GPU-hours table (Usage tab / former modal)."""
    body = Text()
    body.append(f"GPU-hours by user — last {days} days\n\n", style="bold")
    loaded = load_usage_totals(days)
    if loaded is None:
        body.append("No usage data (collector not running or too new).", style="dim")
        return body
    totals, covered, sacct_ts = loaded
    if not totals:
        body.append("No GPU usage recorded in this window.", style="dim")
        return body
    body.append(f" {'user':<14}{'alloc':>9}{'busy':>9}{'eff':>6}{'waste':>9}\n", style="bold underline")
    for user, alloc, busy, sampled_alloc, waste in totals:
        eff = busy / sampled_alloc if sampled_alloc > 0 else 0
        eff_style = "green" if eff >= 0.7 else "yellow" if eff >= 0.4 else "red"
        body.append(f" {user:<14}", style="magenta")
        body.append(f"{alloc / 3600:>8.1f}h{busy / 3600:>8.1f}h", style="bold")
        body.append(f"{eff:>6.0%}", style=eff_style)
        body.append(f"{waste / 3600:>8.1f}h\n", style="red" if waste >= 3600 else "dim")
    daily = load_usage_daily(days)
    if len(daily) > 1:
        body.append(f"\n {'day':<7}{'alloc':>7}{'busy':>7}  cluster GPU-hours/day\n",
                    style="bold underline")
        width = 30
        peak = max(a for _, a, _, _ in daily) or 1.0
        for day, alloc, busy, covered in daily:
            cells = round(alloc / peak * width)
            busy_cells = min(cells, round(busy / peak * width))
            busy_txt = f"{busy / 3600:>6.0f}h" if covered >= 60 or busy > 0 else f"{'-':>7}"
            body.append(f" {day[5:]:<7}", style="cyan")
            body.append(f"{alloc / 3600:>6.0f}h{busy_txt}  ", style="bold")
            body.append("█" * busy_cells, style="green")
            body.append("█" * (cells - busy_cells), style="bright_black")
            body.append("\n")
        if any(c < 60 and b <= 0 for _, _, b, c in daily):
            body.append("   - = collector wasn't sampling that day (busy unknown)\n", style="dim")
    body.append("\nalloc = GPU held by your jobs · busy = GPU actually computing"
                f" · waste = idle/parked ≥{_waste_thr()}", style="dim")
    if sacct_ts:
        body.append(f"\nalloc from slurmdbd (sacct, {(time.time() - sacct_ts) / 60:.0f}m ago)"
                    f" · busy/eff sampled ({covered / 3600:.1f}h observed)", style="dim")
    else:
        body.append(f"\nsampling-based (collector observed {covered / 3600:.1f}h of this window)", style="dim")
    return body


_usage_cache: Tuple[Optional[float], Optional[dict]] = (None, None)


def _read_usage_raw() -> Optional[dict]:
    """usage.json, cached by mtime — render_usage runs every refresh and
    would otherwise re-read and re-parse the file each time."""
    global _usage_cache
    state_dir = Path(os.getenv("SLURM_GPU_TUI_STATE_DIR", str(Path.home() / ".sgpu" / "state")))
    for p in (state_dir / "usage.json", _DATA_DIR / "usage.json"):
        try:
            mtime = p.stat().st_mtime
            if mtime == _usage_cache[0]:
                return _usage_cache[1]
            raw = json.loads(p.read_text())
            _usage_cache = (mtime, raw)
            return raw
        except (OSError, ValueError):
            continue
    return None


def load_usage_daily(days: int) -> List[Tuple[str, float, float, float]]:
    """Cluster-wide per-day totals [(day, alloc_sec, busy_sec, covered_sec)],
    oldest first. Same max(sampled, sacct) alloc merge as load_usage_totals.
    covered_sec ~ 0 means the collector never sampled that day: busy is
    unknown there, not zero."""
    raw = _read_usage_raw()
    if raw is None:
        return []
    cutoff = (datetime.now() - timedelta(days=days)).strftime("%Y-%m-%d")
    sampled = raw.get("days", {})
    sacct = raw.get("sacct_days", {}) if isinstance(raw.get("sacct_days"), dict) else {}
    meta = raw.get("meta", {})
    out: List[Tuple[str, float, float, float]] = []
    for day in sorted(set(sampled) | set(sacct)):
        if day < cutoff:
            continue
        s_users = sampled.get(day, {})
        a_users = sacct.get(day, {})
        alloc = busy = 0.0
        for user in set(s_users) | set(a_users):
            su = s_users.get(user, {})
            alloc += max(su.get("alloc", 0), a_users.get(user, 0.0))
            busy += su.get("busy", 0)
        out.append((day, alloc, busy, float(meta.get(day, 0))))
    return out


def load_usage_totals(days: int) -> Optional[Tuple[List[Tuple[str, float, float, float]], float, Optional[float]]]:
    """Sum usage.json daily buckets over the window.

    alloc per user-day = max(sampled, slurmdbd/sacct) — sacct survives
    collector downtime, sampling covers jobs slurmdbd hasn't flushed yet.
    busy exists only in sampling. eff should be computed against
    sampled_alloc (same observation window as busy), not merged alloc.

    Returns ([(user, alloc, busy, sampled_alloc, waste)] alloc desc,
             covered_seconds, sacct_ts or None)."""
    raw = _read_usage_raw()
    if raw is None:
        return None
    cutoff = (datetime.now() - timedelta(days=days)).strftime("%Y-%m-%d")
    sampled = raw.get("days", {})
    sacct = raw.get("sacct_days", {}) if isinstance(raw.get("sacct_days"), dict) else {}
    totals: Dict[str, List[float]] = {}
    for day in set(sampled) | set(sacct):
        if day < cutoff:
            continue
        s_users = sampled.get(day, {})
        a_users = sacct.get(day, {})
        for user in set(s_users) | set(a_users):
            su = s_users.get(user, {})
            t = totals.setdefault(user, [0.0, 0.0, 0.0, 0.0])
            t[0] += max(su.get("alloc", 0), a_users.get(user, 0.0))
            t[1] += su.get("busy", 0)
            t[2] += su.get("alloc", 0)
            t[3] += su.get("waste", 0)
    covered = sum(v for d, v in raw.get("meta", {}).items() if d >= cutoff)
    sacct_ts = raw.get("sacct_ts") if sacct else None
    return (sorted(((u, a, b, sa, w) for u, (a, b, sa, w) in totals.items()), key=lambda x: -x[1]),
            covered, sacct_ts)
