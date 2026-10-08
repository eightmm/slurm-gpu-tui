"""Bounded local sampling and numeric publication at the agent boundary."""
import os
from types import SimpleNamespace

from sgpu.node_telemetry import NodeSampler, clean_telemetry, read_pressure


def _kernel(tmp_path):
    proc, cg = tmp_path / 'proc', tmp_path / 'cgroup'
    proc.mkdir()
    (proc / 'stat').write_text('cpu 1000 0 1000 8000 0 0 0 0 0 0\n' + 'cpu0 0\n' * 2000)
    pressure = proc / 'pressure'
    pressure.mkdir()
    for resource, number in [('cpu', '12.3'), ('memory', '1.0'), ('io', 'nan')]:
        (pressure / resource).write_text(f'some avg10={number} avg60=0.00 total=10\nfull avg10=99.0\n')
    cg.mkdir()
    (cg / 'cgroup.controllers').write_text('cpu memory io\n')
    scope = cg / 'system.slice' / 'slurmstepd.scope'
    scope.mkdir(parents=True)
    return proc, cg, scope


def _job(scope, name='job_123'):
    job = scope / name
    job.mkdir()
    for file, text in [('cpu.stat', 'usage_usec 2000000\nuser_usec 1000000\n'),
                       ('memory.current', str(512 * 1024**2)), ('memory.peak', str(768 * 1024**2)),
                       ('memory.max', 'max'), ('memory.events', 'oom 3\noom_kill 2\n')]:
        (job / file).write_text(text)
    return job


def test_sampler_deltas_cache_memory_pressure_and_counter_reset(tmp_path):
    proc, cg, scope = _kernel(tmp_path)
    job = _job(scope)
    sampler = NodeSampler(proc, cg, interval=10)
    first = sampler.sample(now=100, wall_time=1000)
    assert first['pressure'] == {'cpu': '12.3', 'memory': '1.0'}
    assert 'cpu_util' not in first
    assert 'cpu_cores' not in first['jobs']['123']
    assert first['jobs']['123']['mem_current_mib'] == 512
    assert first['jobs']['123']['mem_peak_mib'] == 768
    assert first['jobs']['123']['oom_kill'] == 2
    assert 'mem_limit_mib' not in first['jobs']['123']
    (proc / 'stat').write_text('cpu 1400 0 1200 8400 0 0 0 0 0 0\n')
    (job / 'cpu.stat').write_text('usage_usec 27000000\n')
    (job / 'memory.max').write_text(str(1024 * 1024**2))
    assert sampler.sample(now=105, wall_time=1005) is first
    second = sampler.sample(now=110, wall_time=1010)
    assert second['cpu_util'] == '60.0'
    assert second['jobs']['123']['cpu_cores'] == 2.5
    assert second['jobs']['123']['mem_limit_mib'] == 1024
    (job / 'cpu.stat').write_text('usage_usec 1\n')
    (proc / 'stat').write_text('cpu 1 0 1 1 0 0 0 0\n')
    assert 'cpu_cores' not in sampler.sample(now=120, wall_time=1020)['jobs']['123']
    assert 'cpu_util' not in sampler.cached


def test_sampler_does_not_follow_links_fifos_or_user_trees(tmp_path):
    proc, cg, scope = _kernel(tmp_path)
    job = _job(scope)
    victim = tmp_path / 'private'
    victim.write_text(str(123 * 1024**2))
    (job / 'memory.peak').unlink()
    (job / 'memory.peak').symlink_to(victim)
    (job / 'memory.current').unlink()
    os.mkfifo(job / 'memory.current')
    outside = _job(tmp_path, 'job_999')
    (scope / 'job_999').symlink_to(outside, target_is_directory=True)
    other = cg / 'user.slice' / 'slurmstepd.scope'
    other.mkdir(parents=True)
    _job(other, 'job_888')
    (proc / 'pressure' / 'io').unlink()
    (proc / 'pressure' / 'io').symlink_to(victim)
    sample = NodeSampler(proc, cg).sample(now=100, wall_time=1000)
    assert set(sample['jobs']) == {'123'}
    assert 'mem_peak_mib' not in sample['jobs']['123']
    assert 'mem_current_mib' not in sample['jobs']['123']
    assert 'io' not in sample['pressure']


def test_cgroup_v1_unknown_and_scan_limits(tmp_path):
    proc, cg, scope = _kernel(tmp_path)
    _job(scope)
    _job(scope, 'job_124')
    limited = NodeSampler(proc, cg, max_jobs=1).sample(now=100, wall_time=1000)
    assert len(limited['jobs']) == 1 and limited['jobs_truncated']
    (cg / 'cgroup.controllers').unlink()
    assert NodeSampler(proc, cg).sample(now=100, wall_time=1000)['jobs'] == {}
    assert read_pressure(tmp_path / 'missing') == {}


def test_duplicate_job_ids_are_not_guessed(tmp_path):
    proc, cg, scope = _kernel(tmp_path)
    _job(scope)
    second_scope = cg / 'slurm'
    second_scope.mkdir()
    _job(second_scope)
    assert NodeSampler(proc, cg).sample(now=100, wall_time=1000)['jobs'] == {}


def test_sluid_requires_root_step_daemon_not_user_process_title(tmp_path, monkeypatch):
    proc, cg, scope = _kernel(tmp_path)
    job = _job(scope, 's5K1KKYAYG5D00')
    daemon = job / 'step_0' / 'slurm'
    daemon.mkdir(parents=True)
    (daemon / 'cgroup.procs').write_text('456\n')
    pid = proc / '456'
    pid.mkdir()
    (pid / 'cmdline').write_bytes(b'slurmstepd: [123.0]\0')
    original_fstat = os.fstat
    root_owned = False

    def fstat(fd):
        if os.readlink(f'/proc/self/fd/{fd}') == str(pid):
            return SimpleNamespace(st_uid=0 if root_owned else 1000)
        return original_fstat(fd)

    monkeypatch.setattr(os, 'fstat', fstat)
    sampler = NodeSampler(proc, cg)
    assert sampler.sample(now=100, wall_time=1000)['jobs'] == {}
    root_owned = True
    assert set(sampler.sample(now=110, wall_time=1010)['jobs']) == {'123'}
    (pid / 'cmdline').write_bytes(b'python: [123.0]\0')
    assert sampler.sample(now=120, wall_time=1020)['jobs'] == {}


def test_telemetry_boundary_allows_only_finite_bounded_numeric_fields():
    cleaned = clean_telemetry({'observed_at': 100, 'cpu_util': float('nan'),
                              'pressure': {'cpu': 5, 'memory': -1, 'io': True, 'secret': 'path'},
                              'jobs': {'123': {'cpu_cores': 2, 'mem_current_mib': 10, 'oom_kill': True,
                                               'mem_peak_mib': float('inf'), 'path': '/private', 'observed_at': 100},
                                       '../../private': {'cpu_cores': 99}, '456': []},
                              'path': '/private'})
    assert 'cpu_util' not in cleaned and 'path' not in cleaned
    assert cleaned['pressure'] == {'cpu': '5.0'}
    assert cleaned['jobs'] == {'123': {'cpu_cores': 2, 'mem_current_mib': 10, 'observed_at': 100}, '456': {}}
