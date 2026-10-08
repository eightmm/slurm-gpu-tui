"""Bounded, process-local monitor diagnostics and short observation history."""
from __future__ import annotations

import math
import shlex
import threading
from collections import deque


def metric_number(value) -> float | None:
    try:
        number = float(value)
    except (TypeError, ValueError, OverflowError):
        return None
    return number if math.isfinite(number) else None


_command_lock = threading.Lock()
_commands: dict[str, dict] = {}


def record_command(command: str, elapsed: float, success: bool) -> None:
    try:
        words = shlex.split(command)
    except ValueError:
        words = []
    # Fixed labels avoid publishing arguments or unbounded per-job series.
    label = next((w for w in words if w in ("sinfo", "squeue", "scontrol", "sacct")), "other")
    if label == "scontrol":
        label += "_nodes" if "node" in words else "_jobs"
    with _command_lock:
        item = _commands.setdefault(label, {"calls": 0, "failures": 0, "seconds": 0.0, "last_seconds": 0.0})
        item["calls"] += 1
        item["failures"] += int(not success)
        item["seconds"] += elapsed
        item["last_seconds"] = elapsed


def command_snapshot() -> dict:
    with _command_lock:
        return {name: dict(values) for name, values in _commands.items()}


class ObservationHistory:
    """One aggregate observation per source timestamp; unknowns stay gaps."""

    def __init__(self, window: float = 300, max_samples: int = 301):
        self.window = window
        self.samples = deque(maxlen=max_samples)
        self._last_stamp = None

    def observe(self, nodes, now: float) -> None:
        stamp = tuple(sorted((n.name, n.observed_at, n.stale) for n in nodes if n.has_gpu))
        if stamp == self._last_stamp:
            return
        self._last_stamp = stamp
        util, vram, power = [], [], []
        for node in nodes:
            if node.stale or node.observed_at <= 0:
                continue
            for gpu in node.gpus:
                u = metric_number(gpu.util)
                used, total = metric_number(gpu.mem_used), metric_number(gpu.mem_total)
                p = metric_number(gpu.power)
                if u is not None:
                    util.append(u)
                if used is not None and total is not None and total > 0:
                    vram.append(100 * used / total)
                if p is not None:
                    power.append(p)
        self.samples.append((now, sum(util) / len(util) if util else None,
                             sum(vram) / len(vram) if vram else None,
                             sum(power) if power else None))
        while self.samples and self.samples[0][0] < now - self.window:
            self.samples.popleft()

    def sparkline(self, column: int, now: float, width: int = 30) -> str:
        rows = [row for row in self.samples if now - self.window <= row[0] <= now]
        if not rows:
            return "·"
        bins = {}
        for row in rows:
            index = min(width - 1, int((row[0] - (now - self.window)) / self.window * width))
            bins[index] = row[column]
        values = [bins.get(index) for index in range(min(bins), width)]
        peak = 100 if column in (1, 2) else max((v for v in values if v is not None), default=1) or 1
        glyphs = "▁▂▃▄▅▆▇█"
        return "".join("·" if v is None else glyphs[min(7, max(0, round(v / peak * 7)))] for v in values) or "·"
