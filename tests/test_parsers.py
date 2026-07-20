"""Parser tests against captured real-cluster output."""
from sgpu.common import (
    NodeErrorKind, _classify_error, _expand_idx, _gpu_count_from_gres,
    assign_node_jobs, expand_nodelist, parse_gpu_alloc, parse_gres_models,
    parse_node_payload, shorten_gpu_name,
)
from sgpu.common import GpuInfo, JobInfo, NodeInfo
from sgpu.cells import (
    classify_gpu, collect_waste, fmt_idle_age, fmt_span, fmt_start_time,
    mem_cell, parse_slurm_duration,
)


# ── nodelist / index expansion ────────────────────────────────────────────

def test_expand_nodelist():
    assert expand_nodelist("gpu4") == ["gpu4"]
    assert expand_nodelist("gpu[1-3,5],node7") == ["gpu1", "gpu2", "gpu3", "gpu5", "node7"]
    assert expand_nodelist("gpu[01-03]") == ["gpu01", "gpu02", "gpu03"]


def test_assign_node_jobs_multinode():
    # squeue %N compresses multi-node jobs ('cpu[5-8]') — the map must expand
    # them and split the job's total CPUs across its nodes
    jobs = [
        JobInfo(jobid="1", user="a", node="gpu3", cpu_count=8),
        JobInfo(jobid="2", user="b", node="cpu[5-8]", cpu_count=256),
        JobInfo(jobid="3", user="c", node="cpu[1-2]", cpu_count=5),
    ]
    m = assign_node_jobs(jobs)
    assert [j.jobid for j in m["gpu3"]] == ["1"] and m["gpu3"][0].cpu_count == 8
    assert all(m[f"cpu{i}"][0].cpu_count == 64 for i in range(5, 9))
    assert m["cpu1"][0].cpu_count == 3 and m["cpu2"][0].cpu_count == 2


def test_expand_idx():
    assert _expand_idx("0") == ["0"]
    assert _expand_idx("0-1,3") == ["0", "1", "3"]
    assert _expand_idx("N/A") == []


# ── scontrol job -d GPU allocation ────────────────────────────────────────

SCONTROL_LINE = (
    "JobId=37671 JobName=train UserId=jwsong(1003) JobState=RUNNING "
    "NumNodes=1 NumCPUs=8 JOB_GRES=gpu:1 "
    "Nodes=gpu4 CPU_IDs=0-3,16-19 Mem=0 GRES=gpu:1(IDX:0) "
)
SCONTROL_MULTI = (
    "JobId=100 JobName=big UserId=a(1) JobState=RUNNING "
    "Nodes=gpu[1-2] CPU_IDs=0-7 Mem=0 GRES=gpu:2080ti:2(IDX:2-3) "
)
SCONTROL_PENDING = "JobId=999 JobName=w UserId=b(2) JobState=PENDING Nodes=gpu1 CPU_IDs=0 Mem=0 GRES=gpu:1(IDX:0) "


def test_parse_gpu_alloc_single():
    alloc, users = parse_gpu_alloc(SCONTROL_LINE)
    assert alloc == {"gpu4": {"0": "37671"}}
    assert users == {"37671": "jwsong"}


def test_parse_gpu_alloc_multinode_range():
    alloc, users = parse_gpu_alloc(SCONTROL_MULTI)
    assert alloc == {"gpu1": {"2": "100", "3": "100"}, "gpu2": {"2": "100", "3": "100"}}
    assert users == {"100": "a"}


def test_parse_gpu_alloc_mixed_gpu_types():
    line = (
        "JobId=101 JobName=mixed UserId=a(1) JobState=RUNNING "
        "Nodes=gpu1 CPU_IDs=0-7 Mem=0 "
        "GRES=gpu:h100:1(IDX:0),gpu:6000pro_maxq:2(IDX:1-2) "
    )
    alloc, users = parse_gpu_alloc(line)
    assert alloc == {"gpu1": {"0": "101", "1": "101", "2": "101"}}
    assert users == {"101": "a"}


def test_parse_gpu_alloc_skips_pending():
    assert parse_gpu_alloc(SCONTROL_PENDING) == ({}, {})


def test_parse_gpu_alloc_keeps_completing():
    # epilog/teardown window: processes may still sit on the GPU — not rogue
    line = ("JobId=555 JobName=t UserId=c(3) JobState=COMPLETING "
            "Nodes=gpu2 CPU_IDs=0 Mem=0 GRES=gpu:1(IDX:4) ")
    assert parse_gpu_alloc(line) == ({"gpu2": {"4": "555"}}, {"555": "c"})


def test_resolve_user():
    from sgpu.common import resolve_user
    assert resolve_user("untaek") == "untaek"   # already a name → unchanged
    assert resolve_user("") == ""
    assert resolve_user("4000000000") == "4000000000"  # unknown UID → passthrough
    assert resolve_user("0") == "root"           # uid 0 is root on every POSIX host


def test_parse_gpu_alloc_array_task_user():
    # array task: real JobId (38192) never appears in squeue's 38182_0 form,
    # so the UserId map is the only join to a login name
    line = ("JobId=38192 ArrayJobId=38182 ArrayTaskId=0 JobName=stb "
            "UserId=untaek(1019) JobState=RUNNING "
            "Nodes=gpu2 CPU_IDs=2-5 Mem=0 GRES=gpu:2080ti:1(IDX:0) ")
    alloc, users = parse_gpu_alloc(line)
    assert alloc == {"gpu2": {"0": "38192"}}
    assert users == {"38192": "untaek"}


# ── SSH node payload (nvidia-smi + pmon + meminfo + ps) ───────────────────

NODE_PAYLOAD = """\
0, GPU-aaa, NVIDIA RTX 6000 Ada Generation, 97, 41370, 49140, 77, 289.51, 300.00
1, GPU-bbb, NVIDIA RTX 6000 Ada Generation, 0, 3, 49140, 35, 21.05, 300.00
---SEP---
# gpu         pid   type     fb   ccpm  command
# Idx           #    C/G     MB     MB  name
    0       12345      C  41370      0  python
    1           -      -      -      -  -
---SEP---
244140 114000 130140
---SEP---
12345 hlkim
"""


def test_parse_node_payload():
    gpus, mem = parse_node_payload(NODE_PAYLOAD)
    assert len(gpus) == 2
    g0, g1 = gpus
    assert g0.index == "0" and g0.util == "97" and g0.name == "RTX 6000 Ada"
    assert g0.pids == ["12345"] and g0.users == ["hlkim"]
    assert g1.users == [] and g1.pids == []
    assert mem.total == "244140" and mem.used == "114000" and mem.avail == "130140"


def test_parse_node_payload_empty():
    gpus, mem = parse_node_payload("")
    assert gpus == [] and mem.total == ""


def test_parse_gres_models():
    assert parse_gres_models("gpu:h100:1(S:0-1),gpu:6000pro_maxq:3(S:0-1)") == \
        ["h100", "6000pro_maxq", "6000pro_maxq", "6000pro_maxq"]
    assert parse_gres_models("gpu:8(S:0-1)") == [""] * 8
    assert parse_gres_models("gpu:2080ti:4") == ["2080ti"] * 4
    assert parse_gres_models("") == []


def test_gpu_count_from_mixed_gres():
    assert _gpu_count_from_gres("gpu:2") == 2
    assert _gpu_count_from_gres("gpu:h100:1,gpu:6000pro_maxq:3") == 4
    assert _gpu_count_from_gres("gpu:h100:1(S:0-1),gpu:a5000:2") == 3
    assert _gpu_count_from_gres("N/A") == 0


# ── misc pure helpers ─────────────────────────────────────────────────────

def test_shorten_gpu_name():
    assert shorten_gpu_name("NVIDIA RTX 6000 Ada Generation") == "RTX 6000 Ada"
    assert shorten_gpu_name("NVIDIA GeForce RTX 2080 Ti") == "RTX 2080 Ti"


def test_classify_error():
    assert _classify_error("Command timed out after 30 seconds") == NodeErrorKind.SSH_TIMEOUT
    assert _classify_error("ssh: connect: Connection refused") == NodeErrorKind.SSH_UNREACHABLE
    assert _classify_error("Permission denied (publickey)") == NodeErrorKind.SSH_AUTH
    assert _classify_error("something weird") == NodeErrorKind.UNKNOWN


def test_parse_slurm_duration():
    assert parse_slurm_duration("1:02:03") == 3723
    assert parse_slurm_duration("2-01:00:00") == 2 * 86400 + 3600
    assert parse_slurm_duration("15:30") == 930
    assert parse_slurm_duration("UNLIMITED") == -1
    assert parse_slurm_duration("garbage") == -1


def test_fmt_idle_age():
    assert fmt_idle_age(30) == "idle"
    assert fmt_idle_age(120) == "idle 2m"
    assert fmt_idle_age(11520) == "idle 3.2h"
    assert fmt_span(59) == ""
    assert fmt_span(7200) == "2.0h"


def test_classify_gpu():
    assert classify_gpu(GpuInfo(util="85")) == "busy"
    assert classify_gpu(GpuInfo(util="0", mem_used="40000", mem_total="48000")) == "parked"
    assert classify_gpu(GpuInfo(util="0", alloc_jobid="1")) == "idle"
    assert classify_gpu(GpuInfo(util="0")) == "free"
    assert classify_gpu(GpuInfo(util="")) == "unknown"
    # GPU process without any SLURM allocation = rogue (root daemons ignored)
    assert classify_gpu(GpuInfo(util="90", users=["someone"])) == "rogue"
    assert classify_gpu(GpuInfo(util="90", users=["someone"], alloc_jobid="5")) == "busy"
    assert classify_gpu(GpuInfo(util="2", users=["root"])) == "free"


def test_collect_waste():
    nodes = [NodeInfo(name="n1", gpus=[
        GpuInfo(index="0", idle_sec=7200, alloc_user="a", alloc_jobid="10"),
        GpuInfo(index="1", parked_sec=900, users=["b"], alloc_jobid="11"),
        GpuInfo(index="2", idle_sec=30),  # below threshold
    ])]
    rows = collect_waste(nodes, 600)
    assert [(r["kind"], r["user"]) for r in rows] == [("idle", "a"), ("parked", "b")]
    assert rows[0]["sec"] == 7200


def test_collect_waste_rogue_first():
    nodes = [NodeInfo(name="n1", gpus=[
        GpuInfo(index="0", idle_sec=7200, alloc_user="a", alloc_jobid="10"),
        GpuInfo(index="1", users=["intruder"]),  # no alloc -> rogue
    ])]
    rows = collect_waste(nodes, 600)
    assert rows[0]["kind"] == "rogue" and rows[0]["user"] == "intruder"


def test_collect_waste_gresless_job_linked():
    # Same-user SLURM job without gres on the node -> flagged as no-gres + jobid
    nodes = [NodeInfo(
        name="n1",
        gpus=[GpuInfo(index="0", users=["c"], util="95")],
        jobs=[JobInfo(jobid="77", user="c", gpu_count=0)],
    )]
    rows = collect_waste(nodes, 600)
    assert rows[0]["kind"] == "no-gres" and rows[0]["jobid"] == "77"


def test_mem_cell_fallbacks():
    # OS meminfo > sinfo FreeMem > slurm AllocMem (approx, '~') > nothing
    assert "60%" in str(mem_cell(NodeInfo(mem_total="1000", mem_avail="400")))
    assert "30%" in str(mem_cell(NodeInfo(mem_total="1000", mem_free="700")))
    t3 = str(mem_cell(NodeInfo(mem_total="1000", mem_free="N/A", mem_alloc="250")))
    assert "~" in t3 and "25%" in t3
    assert str(mem_cell(NodeInfo(mem_total="1000", mem_free="N/A"))) == "-/1G"


def test_fmt_start_time():
    assert fmt_start_time("N/A") == ""
    assert fmt_start_time("") == ""
    assert fmt_start_time("2030-01-02T15:00:00") == "01-02 15:00"


def test_payload_minor_mapping_and_alloc():
    # gpu4-style board: nvidia-smi (PCI) order differs from /dev/nvidiaN
    # minors, and SLURM GRES IDX means the minor. Captured from gpu4.
    payload = (
        "0, GPU-aaa, RTX 6000 Ada, 7, 2603, 49140, 60, 100, 300, 00000000:06:00.0\n"
        "1, GPU-bbb, RTX 6000 Ada, 96, 46321, 49140, 70, 250, 300, 00000000:07:00.0\n"
        "2, GPU-ccc, RTX 6000 Ada, 0, 0, 49140, 40, 20, 300, 00000000:46:00.0\n"
        "---SEP---\n"
        "# gpu pid mm\n"
        "0 1718866 2592\n"
        "---SEP---\n"
        "128000 64000 64000"
        "---SEP---\n"
        "1718866 jwsong\n"
        "---SEP---\n"
        "0000:06:00.0 2\n"
        "0000:07:00.0 3\n"
        "0000:46:00.0 0\n"
    )
    gpus, _ = parse_node_payload(payload)
    assert [g.minor for g in gpus] == ["2", "3", "0"]
    # job holds SLURM IDX 2 -> must land on smi GPU0 (where the process is)
    from sgpu.common import apply_gpu_alloc
    node = NodeInfo(name="gpu4", gpus=gpus)
    apply_gpu_alloc([node], {"gpu4": {"2": "37885"}}, [JobInfo(jobid="37885", user="jwsong")])
    assert gpus[0].alloc_jobid == "37885" and gpus[0].alloc_user == "jwsong"
    assert gpus[2].alloc_jobid == ""  # smi GPU2 (minor 0) is truly free


def test_apply_gpu_alloc_hetero_binds_to_real_process_gpu():
    # gpu1: mixed H100 + RTX-6000s. SLURM's typed-GRES IDX does not track
    # /dev/nvidiaN, so the raw IDX hint (here 0 and 1) misses the cards the
    # processes actually run on (smi 0 and 2). Under ConstrainDevices the
    # process owner is authoritative -> allocations must bind to smi 0 and 2,
    # leaving the empty cards (smi 1, 3) unallocated (no phantom).
    from sgpu.common import apply_gpu_alloc
    gpus = [
        GpuInfo(index="0", minor="0", util="75", users=["jwsong"], pids=["635929"]),
        GpuInfo(index="1", minor="1", util="0", users=[]),
        GpuInfo(index="2", minor="2", util="99", users=["jwsong"], pids=["528425"]),
        GpuInfo(index="3", minor="3", util="0", users=[]),
    ]
    node = NodeInfo(name="gpu1", gpus=gpus)
    apply_gpu_alloc(
        [node], {"gpu1": {"0": "38211", "1": "38246"}},
        [JobInfo(jobid="38211", user="jwsong"), JobInfo(jobid="38246", user="jwsong")],
    )
    assert gpus[0].alloc_user == "jwsong" and gpus[0].alloc_jobid in ("38211", "38246")
    assert gpus[2].alloc_user == "jwsong" and gpus[2].alloc_jobid in ("38211", "38246")
    assert gpus[0].alloc_jobid != gpus[2].alloc_jobid
    assert gpus[1].alloc_jobid == "" and gpus[1].alloc_user == ""  # empty card, no phantom
    assert gpus[3].alloc_jobid == ""


def test_reconcile_gpu_alloc_dict_path():
    # the collector's dict-based merge path delegates here — same gpu1
    # scenario as above, expressed as (users, idx-key, jobids) triples
    from sgpu.common import reconcile_gpu_alloc
    pairs = reconcile_gpu_alloc(
        {"0": "38211", "1": "38246"},
        {"38211": "jwsong", "38246": "jwsong"},
        [(["jwsong"], "0", []), ([], "1", []), (["jwsong"], "2", []), ([], "3", [])],
    )
    jids = [j for j, _ in pairs]
    assert jids[1] == "" and jids[3] == ""          # empty cards: no phantom
    assert sorted([jids[0], jids[2]]) == ["38211", "38246"]
    assert pairs[0][1] == pairs[2][1] == "jwsong"


def test_reconcile_gpu_alloc_cgroup_exact_beats_user_heuristic():
    # same user holds two jobs; the cgroup probe names each process's job,
    # so the binding must be exact (not first-match by user)
    from sgpu.common import reconcile_gpu_alloc
    pairs = reconcile_gpu_alloc(
        {"0": "38211", "1": "38246"},
        {"38211": "jwsong", "38246": "jwsong"},
        [(["jwsong"], "0", ["38246"]), ([], "1", []),
         (["jwsong"], "2", ["38211"]), ([], "3", [])],
    )
    assert pairs[0] == ("38246", "jwsong")
    assert pairs[2] == ("38211", "jwsong")
    assert pairs[1][0] == "" and pairs[3][0] == ""


def test_apply_gpu_alloc_idle_reservation_and_rogue():
    # userA holds a reservation but hasn't launched (idle) -> lands on a free
    # card by IDX hint; userB runs without any allocation -> stays a rogue
    # (alloc left blank so classify_gpu flags it).
    from sgpu.common import apply_gpu_alloc
    gpus = [
        GpuInfo(index="0", minor="0", util="90", users=["userB"], pids=["1"]),  # rogue
        GpuInfo(index="1", minor="1", util="0", users=[]),                        # idle resv
    ]
    node = NodeInfo(name="gpu9", gpus=gpus)
    apply_gpu_alloc([node], {"gpu9": {"1": "500"}}, [JobInfo(jobid="500", user="userA")])
    assert gpus[0].alloc_jobid == ""  # rogue: userB has no allocation
    assert gpus[1].alloc_jobid == "500" and gpus[1].alloc_user == "userA"


def test_payload_without_minor_falls_back_to_index():
    # macOS-style / old-agent payload with no minor section, no pci column
    payload = ("0, GPU-aaa, A100, 50, 100, 40000, 50, 100, 300\n"
               "---SEP---\n\n---SEP---\n1 1 0---SEP---\n")
    gpus, _ = parse_node_payload(payload)
    assert gpus[0].minor == ""
    from sgpu.common import apply_gpu_alloc
    node = NodeInfo(name="n1", gpus=gpus)
    apply_gpu_alloc([node], {"n1": {"0": "9"}}, [JobInfo(jobid="9", user="u")])
    assert gpus[0].alloc_jobid == "9"


def test_prometheus_metrics_summary():
    from sgpu import __build__, __version__
    from sgpu.collector import _format_metrics

    text = _format_metrics({
        "jobs": [{"jobid": "1"}],
        "pending": [{"jobid": "2"}],
        "nodes": [{
            "name": "gpu1",
            "partition": "gpu",
            "source": "agent",
            "error": "",
            "stale": False,
            "gpus": [
                {
                    "index": "0", "name": "A100", "util": "75",
                    "mem_used": "20480", "mem_total": "40960",
                    "temp": "70", "power": "250",
                    "alloc_jobid": "10", "alloc_user": "alice",
                    "users": ["alice"], "idle_sec": 0, "parked_sec": 0,
                },
                {
                    "index": "1", "name": "A100", "util": "0",
                    "mem_used": "0", "mem_total": "40960",
                    "alloc_jobid": "", "alloc_user": "",
                    "users": [], "idle_sec": 0, "parked_sec": 0,
                },
                {
                    "index": "2", "name": "A100", "util": "90",
                    "mem_used": "1024", "mem_total": "40960",
                    "alloc_jobid": "", "alloc_user": "",
                    "users": ["bob"], "idle_sec": 0, "parked_sec": 0,
                },
                {
                    "index": "3", "name": "A100", "util": "0",
                    "mem_used": "30000", "mem_total": "40960",
                    "alloc_jobid": "11", "alloc_user": "carol",
                    "users": [], "idle_sec": 800, "parked_sec": 900,
                },
            ],
        }],
    })

    assert "sgpu_jobs_running 1" in text
    assert f'sgpu_build_info{{version="{__version__}",build="{__build__}"}} 1' in text
    assert "sgpu_jobs_pending 1" in text
    assert "sgpu_nodes_total 1" in text
    assert "sgpu_gpus_total 4" in text
    assert "sgpu_gpus_allocated 2" in text
    assert "sgpu_gpus_free 1" in text
    assert "sgpu_gpus_rogue 1" in text
    assert "sgpu_gpus_idle 1" in text
    assert "sgpu_gpus_parked 1" in text
    assert 'sgpu_node_info{node="gpu1",partition="gpu",source="agent"} 1' in text
    assert 'sgpu_gpu_mem_used_percent{node="gpu1",gpu="0"} 50' in text


# ── rogue alert grace (notify) ────────────────────────────────────────────

def _mk_notifier(tmp_path):
    import json as _json
    from sgpu.notify import Notifier
    cfg = tmp_path / "webhook.json"
    cfg.write_text(_json.dumps({
        "bot_token": "xoxb-test", "channel": "#gpu", "rogue_alert": True,
        "node_health": False, "collect_alert": False, "ecc_alert": False,
    }))
    n = Notifier(tmp_path, cfg_path=cfg)
    sent = []
    n._post = lambda text, key="": sent.append(text)
    return n, sent


def _rogue_data(users=("intruder",), errors="", stale=False):
    return {"nodes": [{"name": "gpu1", "state": "idle", "stale": stale, "gpus": [
        {"index": "0", "users": list(users), "alloc_jobid": "", "alloc_user": ""},
    ]}], "errors": errors}


def test_notify_rogue_needs_grace(tmp_path):
    n, sent = _mk_notifier(tmp_path)
    n.process(_rogue_data())
    assert sent == []  # first sighting: pending, no alert yet
    for k in n._pending_rogue:
        n._pending_rogue[k] -= n.rogue_grace_sec + 1
    n.process(_rogue_data())
    assert len(sent) == 1 and "intruder" in sent[0]


def test_notify_rogue_pending_clears_when_gone(tmp_path):
    n, sent = _mk_notifier(tmp_path)
    n.process(_rogue_data())
    assert n._pending_rogue
    n.process(_rogue_data(users=()))  # condition cleared -> clock restarts
    assert n._pending_rogue == {}
    assert sent == []


def test_notify_rogue_skips_on_collect_error(tmp_path):
    n, sent = _mk_notifier(tmp_path)
    n.process(_rogue_data(errors="scontrol failed: timeout"))
    assert sent == [] and n._pending_rogue == {}


def test_notify_rogue_skips_stale_node(tmp_path):
    n, sent = _mk_notifier(tmp_path)
    n.process(_rogue_data(stale=True))
    assert sent == [] and n._pending_rogue == {}


def test_notify_rogue_ignores_system_users(tmp_path):
    n, sent = _mk_notifier(tmp_path)
    n.process(_rogue_data(users=("root", "gdm")))
    assert sent == [] and n._pending_rogue == {}
