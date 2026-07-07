"""Parser tests against captured real-cluster output."""
from sgpu.common import (
    NodeErrorKind, _classify_error, _expand_idx, expand_nodelist,
    parse_gpu_alloc, parse_gres_models, parse_node_payload, shorten_gpu_name,
)
from sgpu.common import GpuInfo, JobInfo, NodeInfo
from sgpu.tui import (
    classify_gpu, collect_waste, fmt_idle_age, fmt_span, fmt_start_time,
    mem_cell, parse_slurm_duration,
)


# ── nodelist / index expansion ────────────────────────────────────────────

def test_expand_nodelist():
    assert expand_nodelist("gpu4") == ["gpu4"]
    assert expand_nodelist("gpu[1-3,5],node7") == ["gpu1", "gpu2", "gpu3", "gpu5", "node7"]
    assert expand_nodelist("gpu[01-03]") == ["gpu01", "gpu02", "gpu03"]


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
    alloc = parse_gpu_alloc(SCONTROL_LINE)
    assert alloc == {"gpu4": {"0": "37671"}}


def test_parse_gpu_alloc_multinode_range():
    alloc = parse_gpu_alloc(SCONTROL_MULTI)
    assert alloc == {"gpu1": {"2": "100", "3": "100"}, "gpu2": {"2": "100", "3": "100"}}


def test_parse_gpu_alloc_skips_pending():
    assert parse_gpu_alloc(SCONTROL_PENDING) == {}


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
