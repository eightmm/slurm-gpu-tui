"""Offline TUI replay and repeatable synthetic update benchmarks."""
from __future__ import annotations

import asyncio
import json
import statistics
import time
from copy import deepcopy
from pathlib import Path

from .common import GpuInfo, JobInfo, NodeInfo, PendingJob
from .tui import SlurmGpuTui, _parse_daemon_data


class _OfflineTui(SlurmGpuTui):
    def refresh_all(self):
        pass


def synthetic_snapshot(count: int):
    jobs = [JobInfo(jobid=str(1000 + i), user="fixture", node=f"gpu{i:04}",
                    partition="gpu", jobname="fixture", elapsed="01:00",
                    time_limit="1-00:00:00", gpu_count=8) for i in range(count)]
    nodes = [NodeInfo(name=job.node, state="mix", partition="gpu", cpus="64",
                      cpu_alloc="32", mem_total="262144", mem_avail="131072",
                      jobs=[job], observed_at=1.0,
                      gpus=[GpuInfo(index=str(i), name="H100", util="60", temp="60",
                                    power="200", mem_used="10000", mem_total="80000",
                                    users=["fixture"], alloc_jobid=job.jobid,
                                    alloc_user="fixture") for i in range(8)]) for job in jobs]
    pending = [PendingJob(jobid=str(100000 + i), user="fixture", partition="gpu",
                          priority="10", reason="Priority") for i in range(20)]
    return nodes, jobs, pending, ""


async def _measure(nodes: int, repeat: int, replay: list | None = None) -> list[dict]:
    app = _OfflineTui()
    results = []
    async with app.run_test(size=(160, 50)) as pilot:
        app._auto_collapsed = True
        baseline = _parse_daemon_data(replay[0]) if replay else synthetic_snapshot(nodes)
        app._apply(*deepcopy(baseline))
        await pilot.pause()
        clears = {"gpu": 0, "pending": 0}
        for name, table in (("gpu", app.tbl), ("pending", app.pending_tbl)):
            original = table.clear

            def counted(*args, _name=name, _original=original, **kwargs):
                clears[_name] += 1
                return _original(*args, **kwargs)

            table.clear = counted
        cases = ("replay",) if replay else ("identical", "one_util", "hidden_temp", "missing_metric", "stale_node", "pending_only", "collapsed_pending")
        for case in cases:
            snapshot = deepcopy(baseline)
            if case == "collapsed_pending":
                app._collapsed = {node.name for node in snapshot[0]}
            app._apply(*deepcopy(snapshot))
            await pilot.pause()
            before = dict(clears)
            times = []
            for step in range(repeat):
                if replay:
                    snapshot = _parse_daemon_data(replay[step % len(replay)])
                else:
                    snapshot = deepcopy(snapshot)
                    if case == "one_util":
                        snapshot[0][0].gpus[0].util = str(70 + step % 20)
                    elif case == "hidden_temp":
                        snapshot[0][0].gpus[0].temp = str(61 + step % 20)
                    elif case == "missing_metric":
                        snapshot[0][0].gpus[0].util = "N/A" if step % 2 == 0 else "NaN"
                    elif case == "stale_node":
                        snapshot[0][0].stale = True
                        snapshot[0][0].data_age_sec = 100 + step
                    elif "pending" in case:
                        snapshot[2][0].priority = str(20 + step)
                started = time.perf_counter()
                app._apply(*snapshot)
                times.append((time.perf_counter() - started) * 1000)
            await pilot.pause()
            results.append({"case": case, "nodes": len(snapshot[0]), "gpu_rows": app.tbl.row_count,
                            "median_apply_ms": round(statistics.median(times), 3),
                            "gpu_clear_calls": clears["gpu"] - before["gpu"],
                            "pending_clear_calls": clears["pending"] - before["pending"]})
    return results


def run_benchmark(nodes: int = 128, repeat: int = 5, replay_path: str = "") -> list[dict]:
    if not 1 <= nodes <= 4096 or not 1 <= repeat <= 1000:
        raise ValueError("nodes must be 1..4096 and repeat 1..1000")
    replay = None
    if replay_path:
        path = Path(replay_path)
        if path.stat().st_size > 64 * 1024 * 1024:
            raise ValueError("replay exceeds 64 MiB")
        raw = json.loads(path.read_text())
        replay = raw.get("snapshots", [raw]) if isinstance(raw, dict) else raw
        if not isinstance(replay, list) or not replay or not all(isinstance(item, dict) for item in replay):
            raise ValueError("replay requires a snapshot or a nonempty list of snapshots")
    return asyncio.run(_measure(nodes, repeat, replay))
