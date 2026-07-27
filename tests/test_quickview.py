"""chkgpu's parsing helpers.

The quick-view kept its own slurm.conf/squeue parsers, and while it lived
outside the package they were never covered by anything.
"""
from datetime import datetime

from sgpu.quickview import (
    _expand_gpu_node_field, aggregate_gpu_jobs, compute_thread_usage,
    expand_hostlist, extract_gpu_count, natural_sort_key, parse_gpu_nodes,
    split_squeue,
)


def row(jobid="1", part="gpu", name="job", user="alice", state="RUNNING",
        elapsed="1:00", nodes="1", cpus="8", prio="1", q="1",
        gres="gpu:2", end="2026-07-28T10:00:00", nodelist="gpu1"):
    return "|".join([jobid, part, name, user, state, elapsed, nodes, cpus,
                     prio, q, gres, end, nodelist])


# ── field parsing ─────────────────────────────────────────────────────────

def test_split_squeue_survives_spaces_in_job_names():
    assert split_squeue("1| gpu |my job name |alice ")[2] == "my job name"


def test_expand_gpu_node_field_forms():
    assert _expand_gpu_node_field("gpu3") == [3]
    assert _expand_gpu_node_field("gpu[1-4,7]") == [1, 2, 3, 4, 7]
    assert _expand_gpu_node_field("cpu1") == []


def test_extract_gpu_count_handles_typed_and_plain_gres():
    assert extract_gpu_count("gpu:2") == 2
    assert extract_gpu_count("gpu:a100:4") == 4
    assert extract_gpu_count("gpu:h100:1,gpu:a6000:2") == 3
    assert extract_gpu_count("cpu=8") == 0


def test_expand_hostlist_forms():
    assert expand_hostlist("gpu[1-3,5],node7") == (
        "gpu1", "gpu2", "gpu3", "gpu5", "node7")
    assert expand_hostlist("gpu4.cluster.local") == ("gpu4",)


def test_natural_sort_key_orders_numerically():
    assert sorted(["gpu10", "gpu2", "gpu1"], key=natural_sort_key) == [
        "gpu1", "gpu2", "gpu10"]


def test_parse_gpu_nodes_expands_ranges(tmp_path):
    cfg = tmp_path / "slurm.conf"
    cfg.write_text(
        "NodeName=gpu[1-2] Gres=gpu:a100:4 CPUs=64\n"
        "NodeName=gpu9 Gres=gpu:2 CPUs=32\n"
        "NodeName=cpu1 CPUs=32\n"
    )
    assert parse_gpu_nodes(str(cfg)) == ([1, 2, 9], [4, 4, 2])


# ── GPU aggregation ───────────────────────────────────────────────────────

def test_multi_node_job_counts_gpus_on_every_node():
    # %b is per node, so a 2-GPU request across 2 nodes is 4 GPUs total.
    rows = aggregate_gpu_jobs([row(nodelist="gpu[1-2]", gres="gpu:2")])
    user, per_node, total, _ends = rows[0]
    assert (user, total) == ("alice", 4)
    assert per_node == {"gpu1": 2, "gpu2": 2}


def test_only_running_jobs_are_counted():
    assert aggregate_gpu_jobs([row(state="PENDING", nodelist="(null)")]) == []


def test_cpu_only_jobs_are_skipped():
    assert aggregate_gpu_jobs([row(gres="cpu=8")]) == []


def test_users_are_sorted_by_total_gpus_desc():
    rows = aggregate_gpu_jobs([
        row(user="alice", gres="gpu:1", nodelist="gpu1"),
        row(user="bob", gres="gpu:8", nodelist="gpu2"),
    ])
    assert [r[0] for r in rows] == ["bob", "alice"]


def test_earliest_end_time_per_node_wins():
    rows = aggregate_gpu_jobs([
        row(nodelist="gpu1", end="2026-07-28T10:00:00"),
        row(nodelist="gpu1", end="2026-07-28T08:00:00"),
    ])
    assert rows[0][3]["gpu1"] == datetime(2026, 7, 28, 8, 0, 0)


def test_unknown_end_time_is_ignored_not_fatal():
    rows = aggregate_gpu_jobs([row(nodelist="gpu1", end="Unknown")])
    assert rows[0][3] == {}


# ── CPU thread aggregation ────────────────────────────────────────────────

def test_cpu_threads_split_across_nodes_with_remainder():
    user_node, node_used, user_total = compute_thread_usage(
        [row(cpus="7", nodelist="gpu[1-2]")])
    assert dict(user_node["alice"]) == {"gpu1": 4, "gpu2": 3}
    assert user_total["alice"] == 7
    assert node_used["gpu1"] == 4


def test_cpu_threads_accumulate_across_jobs():
    _u, node_used, user_total = compute_thread_usage([
        row(cpus="4", nodelist="gpu1"),
        row(cpus="6", nodelist="gpu1"),
    ])
    assert node_used["gpu1"] == 10 and user_total["alice"] == 10


def test_non_numeric_cpu_field_is_skipped():
    _u, node_used, _t = compute_thread_usage([row(cpus="N/A", nodelist="gpu1")])
    assert node_used == {}
