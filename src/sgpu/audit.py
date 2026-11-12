"""Read-only, privacy-conscious ROI audit of existing sgpu usage.json data.

This is a decision-support estimate, not a claim of cash savings. In particular,
sacct backfill is used for allocation, but cannot reconstruct GPU utilization
during periods without fresh telemetry.
"""
from __future__ import annotations

import math
import re
from datetime import date, timedelta
from typing import Any

from .usage import merge_usage_window


def _nonnegative(value: Any) -> float:
    """Only count finite, nonnegative counter values."""
    try:
        number = float(value)
    except (TypeError, ValueError, OverflowError):
        return 0.0
    return number if math.isfinite(number) and number >= 0 else 0.0


def _valid_day(day: str, lower: str, upper: str) -> bool:
    if not isinstance(day, str) or not (lower <= day <= upper):
        return False
    try:
        return date.fromisoformat(day).isoformat() == day
    except ValueError:
        return False


def build_audit(
    raw: dict[str, Any],
    *,
    days: int = 30,
    rate: float | None = None,
    reclaimable: float | None = None,
    currency: str = "USD",
    include_users: bool = False,
    as_of: date | None = None,
) -> dict[str, Any]:
    """Summarize sampling-derived busy/waste and merged sacct allocations.

    Monetary impact is only modeled with *both* an explicitly supplied
    currency-denominated GPU-hour rate and an explicit recoverable share.
    No implicit cost model is applied to owned or leased infrastructure.
    """
    if not isinstance(raw, dict):
        raise ValueError("usage input must be a JSON object")
    if not isinstance(days, int) or not 1 <= days <= 366:
        raise ValueError("days must be between 1 and 366")
    if not re.fullmatch(r"[A-Z]{3}", currency):
        raise ValueError("currency must be a three-letter uppercase ISO currency code")
    if (rate is None) != (reclaimable is None):
        raise ValueError("rate and reclaimable must be provided together")
    if rate is not None and (not math.isfinite(rate) or rate < 0):
        raise ValueError("rate must be finite and nonnegative")
    if reclaimable is not None and (
        not math.isfinite(reclaimable) or not 0 <= reclaimable <= 1
    ):
        raise ValueError("reclaimable must be a fraction between 0 and 1")

    through = as_of or date.today()
    start = through - timedelta(days=days - 1)
    lower, upper = start.isoformat(), through.isoformat()
    keep = lambda day: _valid_day(day, lower, upper)

    # Reuse sgpu's canonical per-user/day merge rule, preserving sacct
    # allocation backfill without treating non-observed hours as idle.
    users, _daily = merge_usage_window(raw, keep)
    alloc_sec = sum(_nonnegative(v[0]) for v in users.values())
    busy_sec = sum(_nonnegative(v[1]) for v in users.values())
    observed_sec = sum(_nonnegative(v[2]) for v in users.values())
    waste_sec = sum(_nonnegative(v[3]) for v in users.values())

    # No cross-user identifiers, jobs, node names or exact paths are emitted
    # unless an operator explicitly requests the per-user table.
    meta = raw.get("meta") if isinstance(raw.get("meta"), dict) else {}
    coverage_seconds = sum(_nonnegative(v) for d, v in meta.items() if keep(d))
    telemetry = raw.get("telemetry") if isinstance(raw.get("telemetry"), dict) else {}
    fresh = stale = 0.0
    for day, stats in telemetry.items():
        if keep(day) and isinstance(stats, dict):
            fresh += _nonnegative(stats.get("fresh_gpu_sec"))
            stale += _nonnegative(stats.get("stale_gpu_sec"))
    freshness = fresh / (fresh + stale) if fresh + stale > 0 else None
    efficiency = busy_sec / observed_sec if observed_sec > 0 else None

    modeled_hours = waste_sec / 3600 * reclaimable if reclaimable is not None else None
    modeled_value = modeled_hours * rate if modeled_hours is not None else None
    result: dict[str, Any] = {
        "schema_version": 1,
        "report_type": "historical_gpu_usage_audit",
        "window": {"start": lower, "end": upper, "days": days},
        "summary": {
            "allocated_gpu_hours": round(alloc_sec / 3600, 3),
            "observed_allocated_gpu_hours": round(observed_sec / 3600, 3),
            "observed_busy_gpu_hours": round(busy_sec / 3600, 3),
            "sampled_efficiency": round(efficiency, 4) if efficiency is not None else None,
            "detected_waste_gpu_hours": round(waste_sec / 3600, 3),
            "telemetry_fresh_ratio": round(freshness, 4) if freshness is not None else None,
            "collector_observed_wall_hours": round(coverage_seconds / 3600, 3),
        },
        "scenario": {
            "gpu_hour_rate": rate,
            "reclaimable_fraction": reclaimable,
            "currency": currency,
            "modeled_reclaimable_gpu_hours": round(modeled_hours, 3)
            if modeled_hours is not None else None,
            "modeled_capacity_value": round(modeled_value, 2)
            if modeled_value is not None else None,
        },
        "limitations": [
            "Detected waste is sampled idle/parked time, not necessarily avoidable spend.",
            "Allocated GPU-hours may include sacct backfill when utilization telemetry is absent.",
            "Efficiency uses only observed allocation; unobserved hours are not assumed idle.",
            "Modeled capacity value assumes the specified share can be reused or avoided at "
            "the supplied rate; it is not realized cash savings.",
            "The report is historical, not a prediction of future demand or costs.",
        ],
    }
    if freshness is None:
        result["limitations"].append("Telemetry freshness unavailable; check collector coverage.")
    elif freshness < 0.8:
        result["limitations"].append("Low telemetry freshness; waste may be undercounted.")
    if observed_sec <= 0:
        result["limitations"].append("No observed allocation; utilization cannot be estimated.")
    if observed_sec > 0 and busy_sec > observed_sec + 1e-6:
        result["limitations"].append("Busy time exceeds observed allocation; verify source counters.")
    if observed_sec > 0 and waste_sec > observed_sec + 1e-6:
        result["limitations"].append("Waste exceeds observed allocation; verify source counters.")
    if include_users:
        result["users"] = [
            {
                "user": str(user),
                "allocated_gpu_hours": round(_nonnegative(vals[0]) / 3600, 3),
                "observed_gpu_hours": round(_nonnegative(vals[2]) / 3600, 3),
                "busy_gpu_hours": round(_nonnegative(vals[1]) / 3600, 3),
                "detected_waste_gpu_hours": round(_nonnegative(vals[3]) / 3600, 3),
            }
            for user, vals in sorted(users.items())
        ]
    return result


def _fmt(number: float | None, suffix: str = "") -> str:
    return "unavailable" if number is None else f"{number:,.2f}{suffix}"


def render_audit_markdown(report: dict[str, Any]) -> str:
    """Render a portable Markdown artifact suitable for a client pilot."""
    window = report["window"]
    summary = report["summary"]
    scenario = report["scenario"]
    efficiency = summary["sampled_efficiency"]
    freshness = summary["telemetry_fresh_ratio"]
    lines = [
        "# GPU utilization & waste audit",
        "",
        f"Period: {window['start']} to {window['end']} ({window['days']} days)",
        "",
        "| Metric | Value |",
        "| --- | ---: |",
        f"| Allocated GPU-hours (including sacct backfill) | {_fmt(summary['allocated_gpu_hours'])} |",
        f"| Observed allocated GPU-hours | {_fmt(summary['observed_allocated_gpu_hours'])} |",
        f"| Observed busy GPU-hours | {_fmt(summary['observed_busy_gpu_hours'])} |",
        f"| Observed efficiency | {_fmt(efficiency * 100 if efficiency is not None else None, '%')} |",
        f"| Detected idle/parked waste GPU-hours | {_fmt(summary['detected_waste_gpu_hours'])} |",
        f"| Fresh telemetry share | {_fmt(freshness * 100 if freshness is not None else None, '%')} |",
        f"| Collector observed wall-hours | {_fmt(summary['collector_observed_wall_hours'])} |",
        "",
        "## Optional capacity-value scenario",
        "",
    ]
    if scenario["modeled_capacity_value"] is None:
        lines.append("Not calculated. Provide both --rate and --reclaimable to model a scenario.")
    else:
        lines += [
            f"- Unit GPU-hour rate: {scenario['gpu_hour_rate']:,.2f} {scenario['currency']}",
            f"- Reclaimable fraction (operator assumption): {scenario['reclaimable_fraction']:.0%}",
            f"- Modeled reclaimable capacity: {_fmt(scenario['modeled_reclaimable_gpu_hours'])} GPU-hours",
            f"- **Modeled capacity value: {_fmt(scenario['modeled_capacity_value'])} "
            f"{scenario['currency']} (NOT realized cash savings)**",
        ]
    lines += ["", "## Limitations", ""]
    lines += [f"- {item}" for item in report["limitations"]]
    if "users" in report:
        lines += ["", "## Per-user detail (explicitly requested)", "",
                  "| User | Allocated GPU-h | Observed GPU-h | Busy GPU-h | Waste GPU-h |",
                  "| --- | ---: | ---: | ---: | ---: |"]
        for row in report["users"]:
            # Preserve TSV/Markdown integrity for unexpected Slurm usernames.
            user = row["user"].replace("|", "\\|").replace("\n", " ")
            lines.append(
                f"| {user} | {_fmt(row['allocated_gpu_hours'])} | "
                f"{_fmt(row['observed_gpu_hours'])} | "
                f"{_fmt(row['busy_gpu_hours'])} | "
                f"{_fmt(row['detected_waste_gpu_hours'])} |"
            )
    return "\n".join(lines) + "\n"
