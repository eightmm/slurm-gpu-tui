"""usage.json readers and the Usage-tab renderer."""
from __future__ import annotations

import json
import time
from datetime import datetime, timedelta
from typing import Dict, List, Optional, Tuple

from rich.text import Text

from .cells import _waste_thr
from .runtime import state_dir_candidates


def render_usage(days: int = 7) -> Text:
    """Per-user GPU-hours table (Usage tab / former modal)."""
    global _usage_render_cache
    loaded = _load_usage_window(days)
    # The footer's "N minutes ago" text is the only time-varying part when
    # usage.json itself is unchanged. Rebuild at most once per minute.
    cache_key = (
        _usage_window_cache[0], int(time.time() // 60), _waste_thr(),
    )
    if cache_key == _usage_render_cache[0]:
        cached = _usage_render_cache[1]
        return cached.copy() if cached is not None else Text()

    body = Text()
    body.append(f"GPU-hours by user — last {days} days\n\n", style="bold")
    if loaded is None:
        body.append("No usage data (collector not running or too new).", style="dim")
    else:
        totals, daily, covered, sacct_ts = loaded
        if not totals:
            body.append("No GPU usage recorded in this window.", style="dim")
        else:
            body.append(
                f" {'user':<14}{'alloc':>9}{'busy':>9}{'eff':>6}{'waste':>9}\n",
                style="bold underline",
            )
            for user, alloc, busy, sampled_alloc, waste in totals:
                eff = busy / sampled_alloc if sampled_alloc > 0 else 0
                eff_style = "green" if eff >= 0.7 else "yellow" if eff >= 0.4 else "red"
                body.append(f" {user:<14}", style="magenta")
                body.append(f"{alloc / 3600:>8.1f}h{busy / 3600:>8.1f}h", style="bold")
                body.append(f"{eff:>6.0%}", style=eff_style)
                body.append(
                    f"{waste / 3600:>8.1f}h\n",
                    style="red" if waste >= 3600 else "dim",
                )
            if len(daily) > 1:
                body.append(
                    f"\n {'day':<7}{'alloc':>7}{'busy':>7}  cluster GPU-hours/day\n",
                    style="bold underline",
                )
                width = 30
                peak = max(a for _, a, _, _ in daily) or 1.0
                for day, alloc, busy, covered_day in daily:
                    cells = round(alloc / peak * width)
                    busy_cells = min(cells, round(busy / peak * width))
                    busy_txt = (
                        f"{busy / 3600:>6.0f}h"
                        if covered_day >= 60 or busy > 0 else f"{'-':>7}"
                    )
                    body.append(f" {day[5:]:<7}", style="cyan")
                    body.append(f"{alloc / 3600:>6.0f}h{busy_txt}  ", style="bold")
                    body.append("█" * busy_cells, style="green")
                    body.append("█" * (cells - busy_cells), style="bright_black")
                    body.append("\n")
                if any(c < 60 and b <= 0 for _, _, b, c in daily):
                    body.append(
                        "   - = collector wasn't sampling that day (busy unknown)\n",
                        style="dim",
                    )
            body.append(
                "\nalloc = GPU held by your jobs · busy = GPU actually computing"
                f" · waste = idle/parked ≥{_waste_thr()}", style="dim",
            )
            if sacct_ts:
                body.append(
                    f"\nalloc from slurmdbd (sacct, {(time.time() - sacct_ts) / 60:.0f}m ago)"
                    f" · busy/eff sampled ({covered / 3600:.1f}h observed)",
                    style="dim",
                )
            else:
                body.append(
                    f"\nsampling-based (collector observed {covered / 3600:.1f}h of this window)",
                    style="dim",
                )
    _usage_render_cache = (cache_key, body.copy())
    return body


_usage_cache: Tuple[Optional[tuple], Optional[dict]] = (None, None)
_usage_window_cache: Tuple[Optional[tuple], Optional[tuple]] = (None, None)
_usage_render_cache: Tuple[Optional[tuple], Optional[Text]] = (None, None)


def _read_usage_raw() -> Optional[dict]:
    """Freshest usage.json across the published state locations.

    Cached on the file's path and full stat identity: render_usage runs on
    every refresh and would otherwise re-read and re-parse. Keying on mtime
    alone was wrong — two candidates can share an mtime and return each
    other's cached content, or a same-size replacement can be missed.

    Newest wins rather than first-found: a root collector publishes to
    /var/lib/sgpu, but an install that used to run as a user leaves a stale
    copy behind in the data dir, and silently showing weeks-old GPU-hours is
    worse than showing none.
    """
    global _usage_cache
    best: Optional[tuple] = None
    for d in state_dir_candidates():
        p = d / "usage.json"
        try:
            st = p.stat()
        except OSError:
            continue
        if best is None or st.st_mtime_ns > best[1].st_mtime_ns:
            best = (p, st)
    if best is None:
        return None
    st = best[1]
    key = (
        str(best[0]), st.st_mode, st.st_dev, st.st_ino, st.st_size,
        st.st_mtime_ns, st.st_ctime_ns,
    )
    if key == _usage_cache[0]:
        return _usage_cache[1]
    try:
        raw = json.loads(best[0].read_text())
    except (OSError, ValueError):
        return None
    _usage_cache = (key, raw)
    return raw


def merge_usage_window(raw: dict, keep) -> Tuple[Dict[str, List[float]], Dict[str, List[float]]]:
    """Fold usage.json's two alloc sources over the days `keep(day)` accepts.

    alloc per user-day = max(sampled, slurmdbd/sacct): sacct survives collector
    downtime, sampling covers jobs slurmdbd has not flushed yet. busy and waste
    exist only in sampling, so efficiency must be computed against
    sampled_alloc — the same observation window as busy — not merged alloc.

    Returns ({user: [alloc, busy, sampled_alloc, waste]}, {day: [alloc, busy]}).
    This is the one implementation: the totals view, the daily view, and the
    monthly report each used to carry their own copy of the merge rule.
    """
    sampled = raw.get("days") if isinstance(raw.get("days"), dict) else {}
    sacct = raw.get("sacct_days") if isinstance(raw.get("sacct_days"), dict) else {}
    users: Dict[str, List[float]] = {}
    daily: Dict[str, List[float]] = {}
    for day in set(sampled) | set(sacct):
        if not keep(day):
            continue
        s_users = sampled.get(day, {})
        a_users = sacct.get(day, {})
        d = daily.setdefault(day, [0.0, 0.0])
        for user in set(s_users) | set(a_users):
            su = s_users.get(user, {})
            alloc = max(su.get("alloc", 0), a_users.get(user, 0.0))
            t = users.setdefault(user, [0.0, 0.0, 0.0, 0.0])
            t[0] += alloc
            t[1] += su.get("busy", 0)
            t[2] += su.get("alloc", 0)
            t[3] += su.get("waste", 0)
            d[0] += alloc
            d[1] += su.get("busy", 0)
    return users, daily


def _window_cutoff(days: int) -> str:
    return (datetime.now() - timedelta(days=days)).strftime("%Y-%m-%d")


UsageTotals = List[Tuple[str, float, float, float, float]]
UsageDaily = List[Tuple[str, float, float, float]]


def _load_usage_window(
    days: int,
) -> Optional[Tuple[UsageTotals, UsageDaily, float, Optional[float]]]:
    """Load and merge one window for both Usage-tab views."""
    global _usage_window_cache
    raw = _read_usage_raw()
    if raw is None:
        _usage_window_cache = (("missing", days, _window_cutoff(days)), None)
        return None
    cutoff = _window_cutoff(days)
    source_key = (
        _usage_cache[0] if _usage_cache[1] is raw
        else ("object", id(raw))
    )
    cache_key = (source_key, days, cutoff)
    if cache_key == _usage_window_cache[0]:
        return _usage_window_cache[1]  # type: ignore[return-value]
    totals, daily = merge_usage_window(raw, lambda d: d >= cutoff)
    meta = raw.get("meta", {})
    total_rows = sorted(
        ((u, a, b, sa, w) for u, (a, b, sa, w) in totals.items()),
        key=lambda x: -x[1],
    )
    daily_rows = [
        (day, daily[day][0], daily[day][1], float(meta.get(day, 0)))
        for day in sorted(daily)
    ]
    covered = sum(v for d, v in meta.items() if d >= cutoff)
    sacct_ts = raw.get("sacct_ts") if raw.get("sacct_days") else None
    result = (total_rows, daily_rows, covered, sacct_ts)
    _usage_window_cache = (cache_key, result)
    return result


def load_usage_daily(days: int) -> List[Tuple[str, float, float, float]]:
    """Cluster-wide per-day totals [(day, alloc_sec, busy_sec, covered_sec)],
    oldest first. covered_sec ~ 0 means the collector never sampled that day:
    busy is unknown there, not zero."""
    loaded = _load_usage_window(days)
    return loaded[1] if loaded is not None else []


def load_usage_totals(
    days: int,
) -> Optional[Tuple[UsageTotals, float, Optional[float]]]:
    """Sum usage.json daily buckets over the window.

    Returns ([(user, alloc, busy, sampled_alloc, waste)] alloc desc,
             covered_seconds, sacct_ts or None)."""
    loaded = _load_usage_window(days)
    if loaded is None:
        return None
    totals, _daily, covered, sacct_ts = loaded
    return totals, covered, sacct_ts
