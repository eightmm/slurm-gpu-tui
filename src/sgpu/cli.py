"""One-shot CLI subcommands (--json/--once/--waste/--usage/... and doctor)."""
from __future__ import annotations

import json
import os
import re
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import asdict
from datetime import datetime, timedelta
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from . import __build__, __version__
from .cells import (
    WASTE_MIN_SEC, _waste_thr, classify_gpu, collect_waste, fmt_idle_age,
    fmt_span, fmt_start_time, mb_to_gb,
)
from .common import (
    GpuInfo, JobInfo, NodeInfo, apply_gpu_alloc, build_nodes, cleanup_ssh_pool,
    collect_basic, collect_node_data_parallel, job_log_paths, run_cmd,
    ssh_cmd, tail_file,
)
from .tui import _DAEMON_DATA_FILE, _DAEMON_MAX_AGE, SlurmGpuTui
from .usage import _read_usage_raw, load_usage_daily, load_usage_totals

# ── One-shot CLI mode ─────────────────────────────────────────────────────

def _oneshot_snapshot() -> dict:
    """Fresh snapshot dict: daemon file if recent, else direct collection."""
    try:
        age = time.time() - _DAEMON_DATA_FILE.stat().st_mtime
        if age <= _DAEMON_MAX_AGE:
            return json.loads(_DAEMON_DATA_FILE.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        pass
    nodes_raw, jobs, pending, node_jobs, gpu_alloc, alloc_user_map, err = collect_basic()
    node_names = [n["name"] for n in nodes_raw]
    ssh_results, stale_nodes, ssh_errors = collect_node_data_parallel(node_names)
    nodes = build_nodes(nodes_raw, node_jobs, ssh_results, stale_nodes)
    apply_gpu_alloc(nodes, gpu_alloc, jobs, alloc_user_map)
    return {
        "version": 1,
        "release": __version__,
        "build": __build__,
        "ts": datetime.now().isoformat(),
        "nodes": [asdict(n) for n in nodes],
        "jobs": [asdict(j) for j in jobs],
        "pending": [asdict(p) for p in pending],
        "stale_nodes": stale_nodes,
        "errors": " | ".join(x for x in [err] + ssh_errors if x),
    }


def _print_once(data: dict) -> None:
    print(f"sgpu {data.get('ts', '')}")
    for n in data.get("nodes", []):
        stale = " ~stale" if n.get("stale") else ""
        err = f" ERROR: {n['error']}" if n.get("error") else ""
        print(f"\n{n['name']}  {n['state']}  [{n.get('partition', '')}]  "
              f"CPU {n.get('cpu_alloc') or '0'}/{n.get('cpus', '?')}{stale}{err}")
        for g in n.get("gpus", []):
            user = ",".join(g.get("users", []))
            note = ""
            if not user and g.get("alloc_user"):
                user = g["alloc_user"]
                note = f"  [{fmt_idle_age(g.get('idle_sec', 0)).upper()}]"
            job = f"  job {g['alloc_jobid']}" if g.get("alloc_jobid") else ""
            print(f"  GPU{g.get('index', '?')}  {g.get('name', ''):<14} "
                  f"util {g.get('util', '?'):>3}%  "
                  f"{mb_to_gb(g.get('mem_used', '')):>6}/{mb_to_gb(g.get('mem_total', ''))}G"
                  f"  {user}{note}{job}")
    pending = data.get("pending", [])
    if pending:
        print(f"\nPENDING ({len(pending)}):")
        for p in pending:
            start = fmt_start_time(p.get("start_time", ""))
            start = f"  est.start {start}" if start else ""
            print(f"  {p['jobid']}  {p['user']}  x{p.get('gpu_count', 0)}  "
                  f"{p.get('partition', '')}  {p.get('reason', '')}{start}")


def _snapshot_nodes() -> List[NodeInfo]:
    """Snapshot as NodeInfo list (daemon file or direct collection)."""
    data = _oneshot_snapshot()
    nodes: List[NodeInfo] = []
    for n in data.get("nodes", []):
        gpus = [
            GpuInfo(**{k: g.get(k, d) for k, d in (
                ("index", ""), ("minor", ""), ("uuid", ""), ("pci_bus", ""),
                ("slot", ""), ("serial", ""),
                ("name", ""), ("util", ""), ("mem_used", ""),
                ("mem_total", ""), ("temp", ""), ("power", ""), ("power_cap", ""), ("ecc", ""),
                ("pids", []), ("users", []), ("pid_mem", {}), ("pid_jobid", {}),
                ("alloc_jobid", ""), ("alloc_user", ""), ("idle_sec", 0), ("parked_sec", 0),
            )})
            for g in n.get("gpus", [])
        ]
        node_jobs = [
            JobInfo(jobid=j.get("jobid", ""), user=j.get("user", ""),
                    gpu_count=j.get("gpu_count", 0))
            for j in n.get("jobs", [])
        ]
        nodes.append(NodeInfo(
            name=n.get("name", ""), state=n.get("state", ""),
            partition=n.get("partition", ""), gpus=gpus, jobs=node_jobs,
        ))
    return nodes


def _cli_waste(verbose: bool = False) -> int:
    rows = collect_waste(_snapshot_nodes(), WASTE_MIN_SEC)
    if not rows:
        print(f"no wasted GPUs over threshold ({_waste_thr()})")
        return 0
    print(f"threshold {_waste_thr()} (SLURM_GPU_TUI_WASTE_MIN_SEC)")
    for r in rows:
        job = f"  job {r['jobid']}" if r["jobid"] else ""
        span = "-" if r["kind"] in ("rogue", "no-gres") else (fmt_span(r["sec"]) or "<1m")
        print(f"{r['node']}/{r['gpu']}  {r['kind']:<7} {span:>7}  {r['user']}{job}")
        if verbose and r["jobid"]:
            ok, out = run_cmd(f"scontrol show job {r['jobid']}", timeout=10)
            if ok:
                for m in re.finditer(r"(JobName|Command|WorkDir)=(\S+)", out):
                    print(f"    {m.group(1)}: {m.group(2)}")
    return 1  # non-zero so cron/scripts can alert on it


def _cli_fit(want: int, vram_gb: float = 0, partition: str = "") -> int:
    """Where can a job run right now: nodes with >= N free GPUs (optionally
    with >= vram_gb per GPU), plus a ready-to-paste sbatch line."""
    nodes = _snapshot_nodes()
    hits: List[Tuple[str, str, int, List[GpuInfo]]] = []
    for n in nodes:
        parts = [p for p in (n.partition or "").split(",") if p]
        if partition and partition not in parts:
            continue
        if any(s in n.state.lower() for s in ("down", "drain", "fail")):
            continue
        free = [g for g in n.gpus if classify_gpu(g) == "free"]
        if vram_gb > 0:
            free = [g for g in free
                    if g.mem_total.isdigit() and float(g.mem_total) / 1024 >= vram_gb]
        if len(free) >= want:
            hits.append((n.name, parts[0] if parts else "", len(free), free))
    if not hits:
        where = f" in partition {partition}" if partition else ""
        vr = f" with ≥{vram_gb:.0f}G VRAM" if vram_gb else ""
        print(f"no node has {want} free GPU(s){vr}{where} right now")
        print("tip: sgpu --wait-free N blocks until enough GPUs free up")
        return 1
    hits.sort(key=lambda h: h[2])  # tightest fit first: leave big nodes free
    print(f"{'node':<10}{'partition':<14}{'free':>5}  models (VRAM)")
    for name, part, nfree, free in hits:
        models: Dict[str, int] = {}
        for g in free:
            vr = f"{float(g.mem_total) / 1024:.0f}G" if g.mem_total.isdigit() else "?"
            key = f"{g.name} ({vr})"
            models[key] = models.get(key, 0) + 1
        desc = ", ".join(f"{c}x {m}" for m, c in models.items())
        print(f"{name:<10}{part:<14}{nfree:>5}  {desc}")
    name, part, _, _ = hits[0]
    part_arg = f" -p {part}" if part else ""
    print(f"\nsbatch{part_arg} --gres=gpu:{want} -w {name} your_job.sh")
    return 0


def _cli_me() -> int:
    """Personal dashboard: my jobs, my waste, my week."""
    me = os.environ.get("USER") or ""
    data = _oneshot_snapshot()
    running = [j for j in data.get("jobs", []) if j.get("user") == me]
    pending = [p for p in data.get("pending", []) if p.get("user") == me]
    print(f"{me} — {len(running)} running, {len(pending)} pending")
    for j in running:
        print(f"  {j['jobid']}  {j.get('jobname', '')[:24]:<24} {j.get('node', ''):<8}"
              f" x{j.get('gpu_count', 0)}  {j.get('elapsed', '')}"
              f" / {j.get('time_limit', '')}")
    for p in pending:
        start = fmt_start_time(p.get("start_time", ""))
        print(f"  {p['jobid']}  {p.get('jobname', '')[:24]:<24} PENDING "
              f"({p.get('reason', '')})" + (f"  est.start {start}" if start else ""))
    # my waste: GPUs I hold that do nothing (idle) or merely park VRAM
    mine_waste = [r for r in collect_waste(_snapshot_nodes(), WASTE_MIN_SEC)
                  if r["user"] == me or me in r["user"].split(",")]
    if mine_waste:
        print("\nwasting:")
        for r in mine_waste:
            span = fmt_span(r["sec"]) or "<1m"
            print(f"  {r['node']}/GPU{r['gpu']}  {r['kind']} {span}"
                  + (f"  job {r['jobid']}" if r["jobid"] else ""))
    loaded = load_usage_totals(7)
    if loaded:
        for user, alloc, busy, sampled_alloc, waste in loaded[0]:
            if user == me:
                eff = busy / sampled_alloc if sampled_alloc > 0 else 0
                print(f"\nlast 7d: alloc {alloc / 3600:.1f}h · busy {busy / 3600:.1f}h"
                      f" · eff {eff:.0%} · wasted {waste / 3600:.1f}h")
                break
    return 1 if mine_waste else 0


def _cli_usage(days: int, daily: bool = False) -> int:
    loaded = load_usage_totals(days)
    if loaded is None:
        print("no usage data (collector not running or too new)")
        return 1
    totals, covered, sacct_ts = loaded
    src = "alloc:sacct busy:sampled" if sacct_ts else f"sampled {covered / 3600:.1f}h"
    print(f"{'user':<14}{'alloc':>9}{'busy':>9}{'eff':>6}{'waste':>9}   (last {days}d, {src})")
    for user, alloc, busy, sampled_alloc, waste in totals:
        eff = busy / sampled_alloc if sampled_alloc > 0 else 0
        print(f"{user:<14}{alloc / 3600:>8.1f}h{busy / 3600:>8.1f}h{eff:>6.0%}{waste / 3600:>8.1f}h")
    if daily:
        rows = load_usage_daily(days)
        if rows:
            width = 30
            peak = max(a for _, a, _, _ in rows) or 1.0
            print(f"\n{'day':<12}{'alloc':>7}{'busy':>7}  cluster GPU-hours/day")
            for day, alloc, busy, covered in rows:
                cells = round(alloc / peak * width)
                busy_cells = min(cells, round(busy / peak * width))
                bar = "█" * busy_cells + "░" * (cells - busy_cells)
                busy_txt = f"{busy / 3600:>6.0f}h" if covered >= 60 or busy > 0 else f"{'-':>7}"
                print(f"{day:<12}{alloc / 3600:>6.0f}h{busy_txt}  {bar}")
            if any(c < 60 and b <= 0 for _, _, b, c in rows):
                print("- = collector wasn't sampling that day (busy unknown)")
    return 0


def _sacct_jobs(start: str, end: str = "", user: str = "") -> Optional[List[dict]]:
    """Fetch job rows from slurmdbd. None = sacct failed (no accounting?)."""
    from .collector import _gpu_count_from_tres, _parse_sacct_time
    who = f"-u {user}" if user else "-a"
    span = f"-S {start}" + (f" -E {end}" if end else "")
    ok, out = run_cmd(
        f"sacct {who} -X --noheader --parsable2 "
        f"--format=JobID,User,JobName,Partition,AllocTRES,Submit,Start,End,State {span}",
        timeout=30)
    if not ok:
        print(f"sacct failed: {out.splitlines()[0][:80] if out else 'no output'}")
        return None
    now = time.time()
    rows = []
    for line in out.splitlines():
        parts = line.split("|")
        if len(parts) != 9:
            continue
        jobid, juser, name, part, tres, submit, s_start, s_end, state = parts
        state = state.split()[0]  # "CANCELLED by 1234" -> CANCELLED
        t_sub = _parse_sacct_time(submit)
        t0 = _parse_sacct_time(s_start)
        t1 = _parse_sacct_time(s_end) or now
        wait = (t0 - t_sub) if (t0 and t_sub and t0 > t_sub) else 0.0
        elapsed = (min(t1, now) - t0) if t0 else 0.0
        rows.append({"jobid": jobid, "user": juser, "name": name, "part": part,
                     "gpus": _gpu_count_from_tres(tres), "wait": wait,
                     "elapsed": elapsed, "state": state, "start": t0 or t_sub or 0})
    rows.sort(key=lambda r: r["start"])
    return rows


def _job_summaries(rows: List[dict]) -> Tuple[Dict[str, List[float]], Dict[str, List[float]]]:
    """(gpu-seconds by outcome state, wait-seconds by partition)."""
    by_state: Dict[str, List[float]] = {}
    by_part: Dict[str, List[float]] = {}
    for r in rows:
        by_state.setdefault(r["state"], []).append(r["gpus"] * r["elapsed"])
        if r["start"] and r["state"] != "PENDING":
            by_part.setdefault(r["part"], []).append(r["wait"])
    return by_state, by_part


def _cli_jobs(days: int, user: str = "") -> int:
    """Job history from slurmdbd: per-job rows + failure/wait summary."""
    start = (datetime.now() - timedelta(days=days)).strftime("%Y-%m-%dT00:00:00")
    rows = _sacct_jobs(start, user=user)
    if rows is None:
        return 1
    if not rows:
        print(f"no jobs in the last {days} day(s)")
        return 0
    print(f"{'jobid':<10}{'user':<12}{'state':<12}{'gpus':>4}{'wait':>8}{'elapsed':>9}  "
          f"{'partition':<10} name   (last {days}d)")
    for r in rows:
        print(f"{r['jobid']:<10}{r['user']:<12}{r['state']:<12}{r['gpus'] or '-':>4}"
              f"{fmt_span(r['wait']) or '-':>8}{fmt_span(r['elapsed']) or '-':>9}  "
              f"{r['part']:<10} {r['name'][:24]}")

    by_state, by_part = _job_summaries(rows)
    print(f"\n{'outcome':<14}{'jobs':>6}{'gpu-hours':>11}")
    for state, hours in sorted(by_state.items(), key=lambda kv: -sum(kv[1])):
        print(f"{state:<14}{len(hours):>6}{sum(hours) / 3600:>10.1f}h")

    print(f"\n{'partition':<14}{'jobs':>6}{'median wait':>12}{'max wait':>10}")
    for part, waits in sorted(by_part.items()):
        waits.sort()
        med = waits[len(waits) // 2]
        print(f"{part:<14}{len(waits):>6}{fmt_span(med) or '0m':>12}{fmt_span(waits[-1]) or '0m':>10}")
    return 0


def _cli_report(month: str) -> int:
    """Markdown usage report for a month (YYYY-MM) — for lab meetings/admin."""
    try:
        first = datetime.strptime(month, "%Y-%m")
    except ValueError:
        print(f"bad month {month!r} — expected YYYY-MM")
        return 1
    nxt = datetime(first.year + (first.month == 12), first.month % 12 + 1, 1)
    end_day = min(nxt - timedelta(days=1), datetime.now()).strftime("%Y-%m-%d")
    start_day = first.strftime("%Y-%m-%d")

    print(f"# GPU usage report — {month}\n")

    raw = _read_usage_raw() or {}
    sampled = raw.get("days", {})
    sacct_days = raw.get("sacct_days", {}) if isinstance(raw.get("sacct_days"), dict) else {}
    in_month = lambda d: start_day <= d <= end_day
    users: Dict[str, List[float]] = {}  # alloc, busy, sampled_alloc, waste
    daily: Dict[str, List[float]] = {}  # alloc, busy
    for day in set(sampled) | set(sacct_days):
        if not in_month(day):
            continue
        s_users = sampled.get(day, {})
        a_users = sacct_days.get(day, {})
        for user in set(s_users) | set(a_users):
            su = s_users.get(user, {})
            alloc = max(su.get("alloc", 0), a_users.get(user, 0.0))
            t = users.setdefault(user, [0.0, 0.0, 0.0, 0.0])
            t[0] += alloc
            t[1] += su.get("busy", 0)
            t[2] += su.get("alloc", 0)
            t[3] += su.get("waste", 0)
            d = daily.setdefault(day, [0.0, 0.0])
            d[0] += alloc
            d[1] += su.get("busy", 0)

    print("## Per-user GPU-hours\n")
    print("| user | alloc | busy | eff* | waste |")
    print("|------|------:|-----:|-----:|------:|")
    for user, (a, b, sa, w) in sorted(users.items(), key=lambda kv: -kv[1][0]):
        eff = f"{b / sa:.0%}" if sa > 0 else "-"
        print(f"| {user} | {a / 3600:.1f}h | {b / 3600:.1f}h | {eff} | {w / 3600:.1f}h |")
    print("\n\\* eff/busy/waste are sampling-based (collector uptime only); "
          "alloc includes slurmdbd backfill\n")

    print("## Daily cluster GPU-hours\n")
    print("| day | alloc | busy |")
    print("|-----|------:|-----:|")
    meta = raw.get("meta", {})
    for day in sorted(daily):
        a, b = daily[day]
        busy_txt = f"{b / 3600:.0f}h" if float(meta.get(day, 0)) >= 60 or b > 0 else "-"
        print(f"| {day} | {a / 3600:.0f}h | {busy_txt} |")
    print("\n(busy `-` = collector wasn't sampling that day)\n")

    rows = _sacct_jobs(f"{start_day}T00:00:00", f"{end_day}T23:59:59")
    if rows:
        by_state, by_part = _job_summaries(rows)
        print("## Job outcomes\n")
        print("| outcome | jobs | gpu-hours |")
        print("|---------|-----:|----------:|")
        for state, hours in sorted(by_state.items(), key=lambda kv: -sum(kv[1])):
            print(f"| {state} | {len(hours)} | {sum(hours) / 3600:.1f}h |")
        print("\n## Queue wait by partition\n")
        print("| partition | jobs | median wait | max wait |")
        print("|-----------|-----:|------------:|---------:|")
        for part, waits in sorted(by_part.items()):
            waits.sort()
            med = waits[len(waits) // 2]
            print(f"| {part} | {len(waits)} | {fmt_span(med) or '0m'} | {fmt_span(waits[-1]) or '0m'} |")
    return 0


def _cli_wait_free(want: int, partition: str, interval: int) -> int:
    while True:
        free = 0
        for n in _snapshot_nodes():
            if partition and partition not in n.partition.split(","):
                continue
            free += sum(1 for g in n.gpus if classify_gpu(g) == "free")
        if free >= want:
            print(f"{free} free GPU(s) available" + (f" in {partition}" if partition else ""))
            return 0
        time.sleep(interval)


def _cli_logs(jobid: str, follow: bool = False, want_err: bool = False) -> int:
    """Tail a job's stdout (or stderr with -e); -f keeps following like tail -f."""
    ok, out = run_cmd(f"scontrol show job {jobid}")
    if not ok or "JobId=" not in out:
        print(f"scontrol: {out.strip() or 'job not found'} "
              "(log paths only exist for queued/running jobs)", file=sys.stderr)
        return 1
    stdout_path, stderr_path = job_log_paths(out)
    path = stderr_path if want_err else stdout_path
    if want_err and not stderr_path and stdout_path:
        print("(stderr is merged into stdout)", file=sys.stderr)
        path = stdout_path
    if not path:
        print("job has no log file path", file=sys.stderr)
        return 1
    print(f"== {path}", file=sys.stderr)
    if not follow:
        text = tail_file(path)
        sys.stdout.write(text if text.endswith("\n") else text + "\n")
        return 0
    try:
        pos = max(0, os.path.getsize(path) - 4096)
    except OSError:
        pos = 0
    last_job_check = time.time()
    try:
        while True:
            try:
                size = os.path.getsize(path)
                if size < pos:
                    pos = 0  # truncated — start over
                if size > pos:
                    with open(path, "rb") as f:
                        f.seek(pos)
                        data = f.read()
                    pos += len(data)
                    sys.stdout.write(data.decode(errors="replace"))
                    sys.stdout.flush()
            except FileNotFoundError:
                pass  # not created yet; keep waiting
            if time.time() - last_job_check > 30:
                last_job_check = time.time()
                ok2, out2 = run_cmd(f"scontrol show job {jobid}")
                if not ok2 or "JobId=" not in out2:
                    print("\n== job left the queue (finished)", file=sys.stderr)
                    return 0
            time.sleep(1)
    except KeyboardInterrupt:
        return 0


def _cli_doctor() -> int:
    """Self-diagnosis: is the data trustworthy, and if not, why."""
    problems = 0

    def report(ok: Optional[bool], name: str, detail: str) -> None:
        nonlocal problems
        mark = "OK  " if ok else ("WARN" if ok is None else "FAIL")
        if ok is False:  # WARN is informational, only FAIL flips the exit code
            problems += 1
        print(f"[{mark}] {name:<22} {detail}")

    # slurm commands
    for cmd in ("sinfo -h -o %N", "squeue -h -o %i", "scontrol show config"):
        ok, out = run_cmd(cmd, timeout=10)
        report(ok, cmd.split()[0], "reachable" if ok else out.splitlines()[0][:70])

    # Doctor may run as a different user (e.g. root) than the collector, so
    # home-relative defaults would point at the wrong account. The data.json
    # owner IS the collector user — use their identity for state/Slack/
    # sudoers checks below.
    collector_user: Optional[str] = None
    collector_home: Optional[Path] = None
    try:
        import pwd
        pw = pwd.getpwuid(_DAEMON_DATA_FILE.stat().st_uid)
        collector_user, collector_home = pw.pw_name, Path(pw.pw_dir)
    except (OSError, KeyError):
        pass

    # collector data
    gpu_srcs: Dict[str, int] = {}
    cpu_srcs: Dict[str, int] = {}
    raw: dict = {}
    try:
        age = time.time() - _DAEMON_DATA_FILE.stat().st_mtime
        raw = json.loads(_DAEMON_DATA_FILE.read_text())
        fresh = age <= _DAEMON_MAX_AGE
        report(fresh, "collector data", f"{_DAEMON_DATA_FILE} age {age:.0f}s"
               + ("" if fresh else " — STALE, is sgpu-collector running?"))
        gpu_srcs, cpu_srcs = _split_node_sources(raw.get("nodes", []))
        stale_n = gpu_srcs.get("stale", 0) + cpu_srcs.get("stale", 0)
        source_parts = [f"gpu-{k}:{v}" for k, v in sorted(gpu_srcs.items())]
        source_parts += [
            f"cpu-{'ssh-poll' if k == 'ssh' else k}:{v}"
            for k, v in sorted(cpu_srcs.items())
        ]
        report(stale_n == 0 if source_parts else None, "node sources",
               " ".join(source_parts) or "no nodes")
        collector_release = str(raw.get("release") or "")
        collector_build = str(raw.get("build") or "")
        if collector_release:
            same_release = collector_release == __version__
            same_build = collector_build == __build__ if collector_build else False
            same = same_release and same_build
            report(same if same else None, "release",
                   f"cli={__version__}+{__build__} "
                   f"collector={collector_release}+{collector_build or 'unknown'}"
                   + ("" if same else " — restart/deploy collector"))
        else:
            report(None, "release",
                   f"cli={__version__} collector=unknown — restart/deploy collector")
    except (OSError, ValueError):
        report(False, "collector data", f"{_DAEMON_DATA_FILE} missing — collector not running (TUI falls back to slow SSH)")

    # GPU→job attribution sanity: a process's cgroup names its own job. If a
    # card runs job X's process but the snapshot binds job Y (or nothing),
    # allocation mapping is broken (heterogeneous-node IDX drift regression).
    if raw:
        mismatches: List[str] = []
        probed = 0
        for n in raw.get("nodes", []):
            for g in n.get("gpus", []):
                jids = set((g.get("pid_jobid") or {}).values())
                if not jids:
                    continue
                probed += 1
                if g.get("alloc_jobid", "") not in jids:
                    mismatches.append(
                        f"{n['name']}/GPU{g.get('index')} runs job "
                        f"{','.join(sorted(jids))} but bound to "
                        f"'{g.get('alloc_jobid', '')}'")
        if mismatches:
            report(False, "gpu-job binding",
                   f"{len(mismatches)} mismatch(es): " + "; ".join(mismatches[:4]))
        elif probed:
            report(True, "gpu-job binding",
                   f"{probed} busy GPU(s) cgroup-verified against allocation")

    # collector unit: site-wide kill sweeps (pkill -f python and friends) send
    # SIGTERM, the collector exits 0, and Restart=on-failure leaves it dead
    # until someone notices — only Restart=always survives a clean kill.
    unit = Path("/etc/systemd/system/sgpu-collector.service")
    if not unit.exists():
        for home in (Path.home(), collector_home):
            if home and (home / ".config/systemd/user/sgpu-collector.service").exists():
                unit = home / ".config/systemd/user/sgpu-collector.service"
                break
    unit_text = ""
    try:
        unit_text = unit.read_text()
        restart = next((ln.split("=", 1)[1].strip()
                        for ln in unit_text.splitlines()
                        if ln.startswith("Restart=")), "unset")
        if restart == "always":
            report(True, "collector unit", f"{unit} Restart=always")
        else:
            report(None, "collector unit",
                   f"{unit} Restart={restart} — a clean kill (exit 0) stays dead; "
                   "set Restart=always (rerun installer or deploy.sh)")
    except OSError:
        pass  # no unit installed (nohup mode) — nothing to check

    # node delivery: trust data.json's per-node source counts (authoritative —
    # what the collector actually produced). A disk glob of AGENT_DIR is
    # unreliable here: an interactive `sgpu doctor` doesn't see the collector's
    # SLURM_GPU_TUI_AGENT_DIR (that's baked into the service unit), so it would
    # look in the wrong dir and cry "no push agents" while push is working.
    cpu_details = []
    if cpu_srcs.get("agent", 0):
        cpu_details.append(f"CPU push: {cpu_srcs['agent']} nodes")
    if cpu_srcs.get("ssh", 0):
        cpu_details.append(f"CPU/RAM fallback: {cpu_srcs['ssh']} nodes via SSH")
    cpu_suffix = f"; {', '.join(cpu_details)}" if cpu_details else ""
    if gpu_srcs.get("agent", 0) > 0 and gpu_srcs.get("ssh", 0) > 0:
        report(True, "node delivery",
               f"GPU mixed: {gpu_srcs['agent']} push + "
               f"{gpu_srcs['ssh']} SSH-pull{cpu_suffix}")
    elif gpu_srcs.get("agent", 0) > 0:
        report(True, "node delivery",
               f"GPU push mode ({gpu_srcs['agent']} nodes via agent){cpu_suffix}")
    elif gpu_srcs.get("ssh", 0) > 0:
        # SSH-pull is a fully supported mode, not a problem
        report(True, "node delivery",
               f"GPU SSH-pull mode ({gpu_srcs['ssh']} nodes) — push agents not "
               f"in use (shared-FS install enables them){cpu_suffix}")
    elif cpu_srcs:
        report(None, "node delivery", f"no GPU nodes; CPU/RAM polling only{cpu_suffix}")
    else:
        agent_dir = Path(os.getenv("SLURM_GPU_TUI_AGENT_DIR", str(Path.home() / ".sgpu" / "nodes")))
        report(None, "node delivery", f"no node data yet (checked {agent_dir})")

    # Persistence avoids repeated NVIDIA driver initialization on idle/headless
    # nodes. Check actual GPU state as well as sgpu's boot unit: distro-provided
    # nvidia-persistenced units can be active while explicitly using
    # --no-persistence-mode.
    gpu_nodes = sorted({
        str(n.get("name")) for n in raw.get("nodes", [])
        if n.get("name") and (n.get("has_gpu") is True or n.get("gpus"))
    })
    if gpu_nodes:
        enabled_nodes: List[str] = []
        inactive_units: List[str] = []
        persistence_bad: List[str] = []

        def check_persistence(node: str) -> Tuple[str, bool, str]:
            ok, out = ssh_cmd(node, _PERSISTENCE_STATUS_CMD, timeout=10)
            modes, unit = _parse_persistence_status(out)
            return node, bool(ok and modes and all(m == "Enabled" for m in modes)), unit

        with ThreadPoolExecutor(max_workers=min(8, len(gpu_nodes))) as pool:
            futures = {
                pool.submit(check_persistence, node): node for node in gpu_nodes
            }
            for future in as_completed(futures):
                try:
                    node, enabled, unit_state = future.result()
                except Exception:
                    node = futures[future]
                    persistence_bad.append(node)
                    inactive_units.append(node)
                    continue
                if enabled:
                    enabled_nodes.append(node)
                else:
                    persistence_bad.append(node)
                if unit_state != "active":
                    inactive_units.append(node)
        detail = f"{len(enabled_nodes)}/{len(gpu_nodes)} nodes enabled"
        if persistence_bad:
            detail += f"; disabled/unreachable: {','.join(sorted(persistence_bad))}"
        if inactive_units:
            detail += f"; boot unit inactive/missing: {','.join(sorted(inactive_units))}"
        report(True if not persistence_bad and not inactive_units else None,
               "GPU persistence", detail)

    # persistent state — fall back to the collector user's home when ours
    # has no state (doctor as root, collector as a regular user)
    state_dir = Path(os.getenv("SLURM_GPU_TUI_STATE_DIR", str(Path.home() / ".sgpu" / "state")))
    try:
        if (not (state_dir / "usage.json").exists()
                and not os.getenv("SLURM_GPU_TUI_STATE_DIR") and collector_home
                and (collector_home / ".sgpu" / "state" / "usage.json").exists()):
            state_dir = collector_home / ".sgpu" / "state"
    except OSError:
        pass  # collector home unreadable (doctor as regular user, collector root)
    usage = state_dir / "usage.json"
    if usage.exists():
        report(True, "usage history", f"{usage} age {(time.time() - usage.stat().st_mtime):.0f}s")
    else:
        report(None, "usage history", "not started yet (collector writes it)")

    # slurmdbd backfill (alloc GPU-hours survive collector downtime).
    # Absolute -S time: Slurm < 20.11 rejects relative forms like "now-1hour".
    since = (datetime.now() - timedelta(hours=1)).strftime("%Y-%m-%dT%H:%M:%S")
    ok, out = run_cmd(f"sacct -a -X --noheader -S {since} --format=JobID", timeout=10)
    if not ok:
        report(None, "sacct backfill", "sacct unavailable — alloc is sampling-only "
               + (out.splitlines()[0][:50] if out else ""))
    else:
        sacct_ts = None
        try:
            sacct_ts = json.loads(usage.read_text()).get("sacct_ts")
        except (OSError, ValueError):
            pass
        if sacct_ts:
            age = time.time() - sacct_ts
            fresh = age <= 2 * 3600
            report(True if fresh else None, "sacct backfill",
                   f"last refresh {age / 60:.0f}m ago" + ("" if fresh else " — stale, collector running?"))
        else:
            report(None, "sacct backfill", "sacct works but no backfill yet (collector runs it hourly)")

    # script sharing (sudoers). What matters is whether the COLLECTOR user
    # holds the grant — it does the fetching. Probing `sudo -n` as root is
    # meaningless (root always passes), so read the rule itself instead.
    if collector_user == "root":
        sharing_enabled = _unit_env_enabled(
            unit_text, "SLURM_GPU_TUI_SHARE_SCRIPTS",
        )
        report(True if sharing_enabled else None, "script sharing",
               "root collector; enabled in unit" if sharing_enabled
               else "root collector; disabled in unit")
    elif os.geteuid() == 0 and collector_user is not None:
        grantee = None
        try:
            for line in Path("/etc/sudoers.d/sgpu").read_text().splitlines():
                if line.strip() and not line.lstrip().startswith("#"):
                    grantee = line.split()[0]
                    break
        except OSError:
            pass
        if grantee == collector_user:
            report(True, "script sharing", f"sudoers rule active (grants {grantee})")
        elif grantee:
            report(None, "script sharing", f"sudoers rule grants '{grantee}' but "
                   f"collector runs as '{collector_user}' — rerun installer as {collector_user}")
        else:
            report(None, "script sharing", "not configured (own jobs only) — rerun installer to enable")
    else:
        ok, out = run_cmd("sudo -n scontrol write batch_script 999999999 -", timeout=10)
        if "Invalid job id" in out or ok:
            report(True, "script sharing", "sudoers rule active (all-user script view)")
        else:
            report(None, "script sharing", "not configured (own jobs only) — rerun installer to enable")

    # Slack notifier (optional) — same collector-home fallback as state
    from .notify import Notifier
    try:
        cfg = Path.home() / ".sgpu" / "webhook.json"
        if not cfg.exists() and collector_home:
            alt = collector_home / ".sgpu" / "webhook.json"
            if alt.exists():
                cfg = alt
        nf = Notifier(state_dir, cfg_path=cfg)
        if nf.enabled:
            on = [k for k, v in (("node", nf.node_health), ("collect", nf.collect_alert),
                                 ("waste", nf.waste_alert_hours > 0), ("rogue", nf.rogue_alert),
                                 ("ecc", nf.ecc_alert), ("temp", nf.temp_alert_c > 0)) if v]
            report(True, "slack", f"bot→{nf.channel} daily-thread, lang={nf.lang}, "
                   f"alerts: {'+'.join(on) or 'none'}")
        else:
            report(None, "slack", "not configured (optional) — ~/.sgpu/webhook.json")
    except Exception as e:
        report(False, "slack", f"config error: {e}")

    # prometheus (honors SLURM_GPU_TUI_METRICS_FILE override)
    prom = Path(os.getenv("SLURM_GPU_TUI_METRICS_FILE",
                          str(_DAEMON_DATA_FILE.parent / "metrics.prom")))
    report(True if prom.exists() else None, "prometheus",
           str(prom) if prom.exists() else "no metrics file yet")

    # power telemetry coverage: silent gaps here undercount the cluster's
    # wall-power totals on the dashboards
    try:
        nodes = json.loads(_DAEMON_DATA_FILE.read_text()).get("nodes", [])
    except Exception:
        nodes = []
    if nodes:
        live = [n for n in nodes if not n.get("error")]
        no_bmc = sorted(n["name"] for n in live if not n.get("sys_power"))
        no_rapl = sorted(n["name"] for n in live if not n.get("cpu_power"))
        if not no_bmc and not no_rapl:
            report(True, "power telemetry", f"all {len(live)} reporting nodes have BMC + RAPL")
        else:
            if no_bmc:
                report(None, "power (BMC)",
                       f"no wall power from {len(no_bmc)}/{len(live)}: {','.join(no_bmc[:8])}"
                       f"{'…' if len(no_bmc) > 8 else ''} — needs ipmitool, ipmi_devintf "
                       "module (/dev/ipmi0), agent as root")
            if no_rapl:
                report(None, "power (RAPL)",
                       f"no CPU power from {len(no_rapl)}/{len(live)}: {','.join(no_rapl[:8])}"
                       f"{'…' if len(no_rapl) > 8 else ''} — needs intel_rapl powercap "
                       "readable by the agent (root); AMD CPUs need kernel ≥5.11 "
                       "(expected gap on older kernels — wall power still counts them)")

    print(f"\n{'all checks passed' if problems == 0 else f'{problems} problem(s) found'}")
    return 0 if problems == 0 else 1


def _arg_value(argv: List[str], flag: str, default: str) -> str:
    if flag in argv:
        i = argv.index(flag)
        if i + 1 < len(argv):
            return argv[i + 1]
    return default


def _unit_env_enabled(unit_text: str, name: str) -> bool:
    return any(
        line.strip() == f"Environment={name}=1"
        for line in unit_text.splitlines()
    )


def _split_node_sources(nodes: List[dict]) -> Tuple[Dict[str, int], Dict[str, int]]:
    """Split collector delivery sources into GPU transport and CPU polling."""
    gpu: Dict[str, int] = {}
    cpu: Dict[str, int] = {}
    for node in nodes:
        has_gpu = node.get("has_gpu")
        if has_gpu is None:  # backward compatibility with older data.json
            has_gpu = bool(node.get("gpus")) or "gpu" in str(node.get("gres", "")).lower()
        bucket = gpu if has_gpu else cpu
        source = str(node.get("source") or "?")
        bucket[source] = bucket.get(source, 0) + 1
    return gpu, cpu


_PERSISTENCE_STATUS_CMD = (
    "printf 'modes='; "
    "nvidia-smi --query-gpu=persistence_mode --format=csv,noheader 2>/dev/null "
    "| tr '\\n' ','; "
    "printf '\\nunit='; "
    "systemctl is-active sgpu-gpu-persistence.service 2>/dev/null || true"
)


def _parse_persistence_status(out: str) -> Tuple[List[str], str]:
    """Parse the compact node-side persistence probe used by doctor."""
    fields = {}
    for line in out.splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            fields[key.strip()] = value.strip()
    modes = [m.strip() for m in fields.get("modes", "").split(",") if m.strip()]
    return modes, fields.get("unit", "")


def main():
    argv = sys.argv[1:]
    try:
        if "--version" in argv or (argv and argv[0] == "version"):
            print(f"sgpu {__version__} (build {__build__})")
            return
        if "--json" in argv or "--once" in argv:
            data = _oneshot_snapshot()
            if "--json" in argv:
                print(json.dumps(data, ensure_ascii=False, indent=2))
            else:
                _print_once(data)
            return
        if "--doctor" in argv or "doctor" in argv:
            sys.exit(_cli_doctor())
        if "--waste" in argv:
            sys.exit(_cli_waste(verbose="-v" in argv or "--verbose" in argv))
        if "--usage" in argv:
            v = _arg_value(argv, "--usage", "7")
            sys.exit(_cli_usage(int(v) if v.isdigit() else 7, daily="--daily" in argv))
        if "--report" in argv:
            sys.exit(_cli_report(_arg_value(argv, "--report", datetime.now().strftime("%Y-%m"))))
        if "--jobs" in argv:
            v = _arg_value(argv, "--jobs", "7")
            sys.exit(_cli_jobs(int(v) if v.isdigit() else 7,
                               user=_arg_value(argv, "--user", "")))
        if "--fit" in argv or "fit" in argv[:1]:
            flag = "--fit" if "--fit" in argv else "fit"
            v = _arg_value(argv, flag, "1")
            vram = _arg_value(argv, "--vram", "0")
            sys.exit(_cli_fit(int(v) if v.isdigit() else 1,
                              vram_gb=float(vram) if vram.replace(".", "").isdigit() else 0,
                              partition=_arg_value(argv, "--partition", "")))
        if "--me" in argv or "me" in argv[:1]:
            sys.exit(_cli_me())
        if argv[:1] == ["logs"] or "--logs" in argv:
            jid = (_arg_value(argv, "--logs", "") if "--logs" in argv
                   else (argv[1] if len(argv) > 1 and not argv[1].startswith("-") else ""))
            if not jid:
                print("usage: sgpu logs JOBID [-f] [-e]", file=sys.stderr)
                sys.exit(2)
            sys.exit(_cli_logs(jid, follow="-f" in argv or "--follow" in argv,
                               want_err="-e" in argv or "--err" in argv))
        if "--wait-free" in argv:
            want = int(_arg_value(argv, "--wait-free", "1"))
            part = _arg_value(argv, "--partition", "")
            interval = int(_arg_value(argv, "--interval", "10"))
            sys.exit(_cli_wait_free(want, part, interval))
        if argv and argv[0] in ("-h", "--help"):
            print("usage: sgpu [--version | --json | --once | --waste [-v] | --usage [days] [--daily] |\n"
                  "             --jobs [days] [--user U] | --report [YYYY-MM] | --wait-free N |\n"
                  "             fit N [--vram G] [--partition P] | logs JOBID [-f] [-e] | me | doctor]\n"
                  "  (no args)      interactive TUI\n"
                  "  --version      print installed release and exit\n"
                  "  --json         print snapshot as JSON and exit\n"
                  "  --once         print snapshot as plain text and exit\n"
                  "  --waste [-v]   list idle/parked/rogue GPUs; exit 1 if any (cron-friendly)\n"
                  "                 -v adds JobName/Command/WorkDir per offender\n"
                  "  --usage [N]    per-user GPU-hours over last N days (default 7)\n"
                  "                 --daily adds a per-day cluster trend\n"
                  "  --jobs [N]     job history from slurmdbd: outcomes, queue waits\n"
                  "                 [--user U] filters to one user\n"
                  "  --report [M]   markdown monthly report (default: current month)\n"
                  "  --wait-free N  block until N GPUs are free\n"
                  "                 [--partition P] [--interval sec]\n"
                  "  fit N          nodes that fit N free GPUs now + sbatch line\n"
                  "                 [--vram G] minimum VRAM per GPU, [--partition P]\n"
                  "  logs JOBID     tail a job's stdout (last 64KB)\n"
                  "                 -f follow like tail -f, -e stderr instead\n"
                  "  me             my jobs, my wasted GPUs, my week (exit 1 if wasting)\n"
                  "  doctor         self-diagnosis: data freshness, agents, slurm, sharing")
            return
    finally:
        if argv:
            cleanup_ssh_pool()
    SlurmGpuTui().run()
