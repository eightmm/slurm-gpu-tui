"""RAM fair-share: mem parsing, collector ratio metric, notify alert."""
import json

from sgpu.common import mem_to_mib
from sgpu.collector import _format_metrics


def test_mem_to_mib_suffixes():
    assert mem_to_mib("128G") == 128 * 1024
    assert mem_to_mib("4000M") == 4000
    assert mem_to_mib("1T") == 1024 * 1024
    assert mem_to_mib("512K") == 0.5
    assert mem_to_mib("64") == 64          # bare number = MiB
    assert mem_to_mib("24Gn") == 24 * 1024  # per-node suffix
    assert mem_to_mib("4Gc", cpus=8) == 4 * 1024 * 8  # per-CPU × cpus
    assert mem_to_mib("") == 0.0
    assert mem_to_mib("garbage") == 0.0
    assert mem_to_mib("0") == 0.0


def _data(jobs, node_mem="256000", ngpus=8):
    return {
        "jobs": jobs,
        "pending": [],
        "nodes": [{
            "name": "gpu1", "state": "mixed", "mem_total": node_mem,
            "gpus": [{"index": i} for i in range(ngpus)],
        }],
    }


def test_fair_ratio_metric():
    # 8-GPU node with 256000 MiB: 1 GPU entitles 32000 MiB. 128G req = 4.096x
    out = _format_metrics(_data([{
        "jobid": "9", "user": "bob", "node": "gpu1",
        "gpu_count": 1, "cpu_count": 4, "mem": "128G",
    }]))
    assert 'sgpu_job_mem_mib{jobid="9",user="bob",node="gpu1",gpus="1"} 131072' in out
    assert 'sgpu_job_mem_fair_ratio{jobid="9",user="bob",node="gpu1",gpus="1"} 4.096' in out


def test_fair_ratio_skips_non_gpu_and_unknown():
    out = _format_metrics(_data([
        {"jobid": "1", "user": "a", "node": "gpu1", "gpu_count": 0, "mem": "128G"},
        {"jobid": "2", "user": "b", "node": "nope", "gpu_count": 1, "mem": "128G"},
        {"jobid": "3", "user": "c", "node": "gpu1", "gpu_count": 1, "mem": ""},
    ]))
    assert "sgpu_job_mem_fair_ratio" not in out.split("# TYPE sgpu_job_mem_fair_ratio gauge")[1]


def test_notify_mem_hog(tmp_path):
    import sgpu.notify as notify_mod
    cfg = {"bot_token": "xoxb-test", "channel": "#gpu", "node_health": False,
           "collect_alert": False, "rogue_alert": False, "ecc_alert": False,
           "mem_fair_factor": 1.0, "dm_users": {"bob": "U01"}}
    p = tmp_path / "webhook.json"
    p.write_text(json.dumps(cfg))
    n = notify_mod.Notifier(tmp_path, cfg_path=p)
    posts = []
    n._post = lambda text, key="", channel="": posts.append((channel, text))

    data = _data([
        {"jobid": "9", "user": "bob", "jobname": "hog", "node": "gpu1",
         "gpu_count": 1, "cpu_count": 4, "mem": "128G"},     # 4x share -> alert
        {"jobid": "10", "user": "eve", "jobname": "fair", "node": "gpu1",
         "gpu_count": 1, "cpu_count": 4, "mem": "24G"},      # under share
    ])
    data["errors"] = ""
    n.process(data)
    texts = [t for _, t in posts]
    assert any("128G" in t and "hog" in t for t in texts), texts
    assert not any("job 10 " in t for t in texts)  # under-share job stays quiet
    assert any(c == "U01" for c, _ in posts)  # DM to the owner
    # debounced: second cycle must not re-post
    before = len(posts)
    n.process(data)
    assert len(posts) == before
