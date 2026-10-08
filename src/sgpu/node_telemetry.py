"""Bounded local CPU/PSI and Slurm cgroup-v2 sampling; no scheduler RPCs."""
from __future__ import annotations

import os
import re
import stat
import time
from collections import deque
from pathlib import Path

from .telemetry import metric_number

_DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
TELEMETRY_MAX_AGE = max(30.0, 2 * float(os.getenv("SLURM_GPU_TUI_TELEMETRY_SEC", "10")))


def _read_at(fd: int, name: str, first_line: bool = False) -> str:
    source = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC, dir_fd=fd)
    try:
        if not stat.S_ISREG(os.fstat(source).st_mode):
            raise OSError("not a regular kernel file")
        data = os.read(source, 8193)
        if first_line:
            data = data.partition(b"\n")[0]
        if len(data) > 8192:
            raise OSError("kernel record too large")
        return data.decode("ascii")
    finally:
        os.close(source)


def read_pressure(root: Path = Path("/proc/pressure")) -> dict:
    values = {}
    try:
        fd = os.open(root, _DIR_FLAGS)
    except OSError:
        return values
    try:
        for resource in ("cpu", "memory", "io"):
            try:
                for line in _read_at(fd, resource).splitlines():
                    if line.startswith("some "):
                        raw = dict(word.split("=", 1) for word in line.split()[1:] if "=" in word).get("avg10")
                        value = metric_number(raw)
                        if value is not None and 0 <= value <= 100:
                            values[resource] = f"{value:.1f}"
            except (OSError, ValueError, UnicodeError):
                continue
    finally:
        os.close(fd)
    return values


class NodeSampler:
    def __init__(self, proc_root: Path = Path("/proc"), cgroup_root: Path = Path("/sys/fs/cgroup"),
                 interval: float = 10, max_jobs: int = 256):
        self.proc_root, self.cgroup_root = proc_root, cgroup_root
        self.interval, self.max_jobs = max(1, interval), max(1, min(4096, max_jobs))
        self.next_sample = 0.0
        self.cached = {}
        self.cpu_previous = None
        self.job_previous = {}

    def sample(self, now: float | None = None, wall_time: float | None = None) -> dict:
        now = time.monotonic() if now is None else now
        wall_time = time.time() if wall_time is None else wall_time
        if now < self.next_sample:
            return self.cached
        result = {"observed_at": wall_time, "pressure": read_pressure(self.proc_root / "pressure")}
        proc_fd = None
        try:
            proc_fd = os.open(self.proc_root, _DIR_FLAGS)
            fields = _read_at(proc_fd, "stat", first_line=True).split()
            counters = tuple(int(value) for value in fields[1:9])
            if fields[0] == "cpu" and len(counters) == 8:
                previous, self.cpu_previous = self.cpu_previous, counters
                if previous is not None and all(a >= b for a, b in zip(counters, previous, strict=True)):
                    total = sum(counters) - sum(previous)
                    idle = counters[3] + counters[4] - previous[3] - previous[4]
                    if total > 0:
                        result["cpu_util"] = f"{100 * (total - idle) / total:.1f}"
        except (OSError, ValueError, IndexError, UnicodeError, OverflowError):
            self.cpu_previous = None
        finally:
            if proc_fd is not None:
                os.close(proc_fd)
        jobs, truncated = self._jobs(now, wall_time)
        result.update(jobs=jobs, jobs_truncated=truncated)
        self.cached, self.next_sample = result, now + self.interval
        return result

    def _scope_fds(self, root_fd: int):
        # Only Slurm-managed trees: never traverse user.slice or arbitrary jobs.
        for parent in ("system.slice", ""):
            try:
                fd = os.open(parent, _DIR_FLAGS, dir_fd=root_fd) if parent else os.dup(root_fd)
            except OSError:
                continue
            try:
                with os.scandir(fd) as entries:
                    candidates = []
                    for offset, entry in enumerate(entries):
                        if offset >= 4096:
                            break
                        if entry.is_dir(follow_symlinks=False) and (entry.name == "slurm" or re.fullmatch(r"(?:[A-Za-z0-9_.-]+_)?slurmstepd\.scope", entry.name)):
                            candidates.append(entry.name)
                        if len(candidates) >= 64:
                            break
                for name in sorted(candidates):
                    try:
                        yield os.open(name, _DIR_FLAGS, dir_fd=fd)
                    except OSError:
                        continue
            finally:
                os.close(fd)

    def _sluid_job_id(self, fd: int) -> str:
        # Slurm >=26.05 may use SLUID directories. The root-owned step daemon
        # provides the numeric ID; user process names/environment are not authority.
        try:
            steps = []
            with os.scandir(fd) as entries:
                for offset, entry in enumerate(entries):
                    if offset >= 4096 or len(steps) >= 32:
                        break
                    if re.fullmatch(r"step_[A-Za-z0-9_]+", entry.name):
                        steps.append(entry.name)
        except OSError:
            return ""
        for step in steps:
            step_fd = daemon_fd = proc_fd = pid_fd = None
            try:
                step_fd = os.open(step, _DIR_FLAGS, dir_fd=fd)
                daemon_fd = os.open("slurm", _DIR_FLAGS, dir_fd=step_fd)
                pids = _read_at(daemon_fd, "cgroup.procs").split()[:32]
                proc_fd = os.open(self.proc_root, _DIR_FLAGS)
                for pid in pids:
                    if not re.fullmatch(r"[0-9]{1,20}", pid):
                        continue
                    pid_fd = os.open(pid, _DIR_FLAGS, dir_fd=proc_fd)
                    try:
                        if os.fstat(pid_fd).st_uid != 0:
                            continue
                        command = _read_at(pid_fd, "cmdline").split("\0", 1)[0]
                        match = re.fullmatch(r"slurmstepd: \[([0-9]{1,20})\.[A-Za-z0-9_]+\]", command)
                        if match:
                            return match[1]
                    finally:
                        os.close(pid_fd)
                        pid_fd = None
            except (OSError, UnicodeError):
                continue
            finally:
                for opened in (pid_fd, proc_fd, daemon_fd, step_fd):
                    if opened is not None:
                        os.close(opened)
        return ""

    def _job_values(self, fd: int, now: float, wall_time: float) -> dict:
        result = {"observed_at": wall_time}
        identity = (os.fstat(fd).st_dev, os.fstat(fd).st_ino)
        try:
            counters = dict(line.split() for line in _read_at(fd, "cpu.stat").splitlines() if len(line.split()) == 2)
            usage = int(counters["usage_usec"])
            if usage < 0:
                raise ValueError("negative CPU counter")
            previous = self.job_previous.get(identity)
            self.job_previous[identity] = (now, usage)
            if previous and now > previous[0] and usage >= previous[1]:
                result["cpu_cores"] = (usage - previous[1]) / (now - previous[0]) / 1e6
        except (OSError, ValueError, KeyError, UnicodeError, OverflowError):
            self.job_previous.pop(identity, None)
        for file, key in (("memory.current", "mem_current_mib"), ("memory.peak", "mem_peak_mib"), ("memory.max", "mem_limit_mib")):
            try:
                value = int(_read_at(fd, file).strip())
                if value >= 0:
                    result[key] = value / 1024 ** 2
            except (OSError, ValueError, UnicodeError, OverflowError):
                continue
        try:
            events = dict(line.split() for line in _read_at(fd, "memory.events").splitlines() if len(line.split()) == 2)
            count = int(events["oom_kill"])
            if count >= 0:
                result["oom_kill"] = count
        except (OSError, ValueError, KeyError, UnicodeError):
            pass
        return result

    def _jobs(self, now: float, wall_time: float) -> tuple[dict, bool]:
        jobs, seen, seen_identities = {}, set(), set()
        duplicate_ids = set()
        truncated = False
        visited = 0
        try:
            root_fd = os.open(self.cgroup_root, _DIR_FLAGS)
        except OSError:
            self.job_previous.clear()
            return {}, False
        try:
            try:
                _read_at(root_fd, "cgroup.controllers")
            except (OSError, UnicodeError):
                self.job_previous.clear()
                return {}, False
            for scope_fd in self._scope_fds(root_fd):
                queue = deque([(scope_fd, 0)])
                try:
                    while queue:
                        fd, depth = queue.popleft()
                        try:
                            identity = (os.fstat(fd).st_dev, os.fstat(fd).st_ino)
                            if identity in seen:
                                continue
                            seen.add(identity)
                            with os.scandir(fd) as entries:
                                for entry in entries:
                                    visited += 1
                                    if visited > 4096 or len(jobs) >= self.max_jobs:
                                        truncated = True
                                        break
                                    if not entry.is_dir(follow_symlinks=False):
                                        continue
                                    match = re.fullmatch(r"job_([0-9]{1,20})", entry.name)
                                    sluid = re.fullmatch(r"s[A-Za-z0-9]{1,32}", entry.name)
                                    if not match and not sluid and not (depth < 2 and re.fullmatch(r"uid_[0-9]{1,20}", entry.name)):
                                        continue
                                    child_fd = os.open(entry.name, _DIR_FLAGS, dir_fd=fd)
                                    if not match and not sluid:
                                        queue.append((child_fd, depth + 1))
                                        continue
                                    try:
                                        jid = match[1] if match else self._sluid_job_id(child_fd)
                                        if not jid:
                                            continue
                                        child_identity = (os.fstat(child_fd).st_dev, os.fstat(child_fd).st_ino)
                                        seen_identities.add(child_identity)
                                        if jid in jobs or jid in duplicate_ids:
                                            jobs.pop(jid, None)
                                            duplicate_ids.add(jid)
                                        else:
                                            jobs[jid] = self._job_values(child_fd, now, wall_time)
                                    finally:
                                        os.close(child_fd)
                        except OSError:
                            continue
                        finally:
                            os.close(fd)
                        if truncated:
                            break
                finally:
                    for fd, _depth in queue:
                        os.close(fd)
                if truncated:
                    break
        finally:
            os.close(root_fd)
        self.job_previous = {identity: value for identity, value in self.job_previous.items() if identity in seen_identities}
        return jobs, truncated


def clean_sample(value: object) -> dict:
    """Allowlist numeric telemetry at the agent/collector boundary."""
    if not isinstance(value, dict):
        return {}
    result = {}
    for key in ("observed_at", "cpu_cores", "mem_current_mib", "mem_peak_mib", "mem_limit_mib", "oom_kill", "vram_mib"):
        number = metric_number(value.get(key))
        if number is not None and number >= 0 and not isinstance(value.get(key), bool):
            result[key] = number
    return result


def clean_telemetry(value: object) -> dict:
    if not isinstance(value, dict):
        return {}
    result = clean_sample(value)
    cpu = metric_number(value.get("cpu_util"))
    if cpu is not None and 0 <= cpu <= 100 and not isinstance(value.get("cpu_util"), bool):
        result["cpu_util"] = f"{cpu:.1f}"
    pressure = value.get("pressure", {})
    result["pressure"] = {}
    if isinstance(pressure, dict):
        for resource in ("cpu", "memory", "io"):
            number = metric_number(pressure.get(resource))
            if number is not None and 0 <= number <= 100 and not isinstance(pressure.get(resource), bool):
                result["pressure"][resource] = f"{number:.1f}"
    jobs = value.get("jobs", {})
    result["jobs"] = {}
    if isinstance(jobs, dict):
        for jid, sample in list(jobs.items())[:4096]:
            if isinstance(jid, str) and re.fullmatch(r"[0-9]{1,20}", jid):
                result["jobs"][jid] = clean_sample(sample)
    result["jobs_truncated"] = value.get("jobs_truncated") is True or isinstance(jobs, dict) and len(jobs) > 4096
    return result
