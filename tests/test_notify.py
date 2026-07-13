"""Notifier state-machine tests: node down/recovered, collect-blind,
job-done diffing, debounce, persistence. No network — _post is stubbed."""
import json
import time

import pytest

from sgpu.notify import Notifier, _fmt_dur, DEBOUNCE_SEC


def _mk(tmp_path, **cfg_over):
    cfg = {"url": "http://example.invalid/hook", "node_health": False,
           "collect_alert": False, "rogue_alert": False, "ecc_alert": False}
    cfg.update(cfg_over)
    p = tmp_path / "webhook.json"
    p.write_text(json.dumps(cfg))
    n = Notifier(tmp_path, cfg_path=p)
    sent = []
    n._post = lambda text, key="": sent.append(text)
    return n, sent


def _node(name="gpu1", state="idle", gpus=None, **kw):
    d = {"name": name, "state": state, "partition": "gpu",
         "has_gpu": True, "stale": False, "error": "",
         "gpus": gpus if gpus is not None else [], "jobs": []}
    d.update(kw)
    return d


# ── node down / recovered ─────────────────────────────────────────────────

def test_node_down_respects_grace_then_alerts_and_recovers(tmp_path):
    n, sent = _mk(tmp_path, node_health=True)
    down = {"nodes": [_node(state="down")], "jobs": [], "errors": ""}
    n.process(down)
    assert sent == []  # inside grace
    n._pending_down["gpu1"] -= n.down_grace_sec + 1
    n.process(down)
    assert len(sent) == 1 and "down" in sent[0]
    n.process({"nodes": [_node(state="idle")], "jobs": [], "errors": ""})
    assert len(sent) == 2 and "recovered" in sent[1]


def test_node_down_transient_blip_never_alerts(tmp_path):
    n, sent = _mk(tmp_path, node_health=True)
    n.process({"nodes": [_node(state="down")], "jobs": [], "errors": ""})
    n.process({"nodes": [_node(state="idle")], "jobs": [], "errors": ""})
    n.process({"nodes": [_node(state="down")], "jobs": [], "errors": ""})
    assert sent == []  # pending clock restarted, grace never elapsed


# ── collect-blind ─────────────────────────────────────────────────────────

def test_blind_only_when_slurm_up(tmp_path):
    n, sent = _mk(tmp_path, collect_alert=True)
    # stale + slurm down -> node_health's business, not blind
    n.process({"nodes": [_node(state="down", stale=True)], "jobs": [], "errors": ""})
    assert n._pending_blind == {}
    blind = {"nodes": [_node(state="idle", stale=True)], "jobs": [], "errors": ""}
    n.process(blind)
    assert "gpu1" in n._pending_blind
    n._pending_blind["gpu1"] -= n.collect_grace_sec + 1
    n.process(blind)
    assert len(sent) == 1 and "collect" in sent[0]


# ── job-done ──────────────────────────────────────────────────────────────

def _job(jid="7", user="alice"):
    return {"jobid": jid, "user": user, "jobname": "train", "elapsed": "1:00:00"}


def test_job_done_fires_when_job_leaves_queue(tmp_path):
    n, sent = _mk(tmp_path, job_done_users=["alice"])
    n.process({"nodes": [], "jobs": [_job()], "errors": ""})
    assert sent == []
    n.process({"nodes": [], "jobs": [], "errors": ""})
    assert len(sent) == 1 and "7" in sent[0]


def test_job_done_skipped_on_collect_error(tmp_path):
    n, sent = _mk(tmp_path, job_done_users=["alice"])
    n.process({"nodes": [], "jobs": [_job()], "errors": ""})
    # squeue hiccup: empty jobs + error string must NOT fire "finished"
    n.process({"nodes": [], "jobs": [], "errors": "squeue failed: timeout"})
    assert sent == []
    assert "7" in n._jobs  # baseline preserved for recovery
    n.process({"nodes": [], "jobs": [], "errors": ""})
    assert len(sent) == 1  # real disappearance still alerts afterwards


# ── debounce ──────────────────────────────────────────────────────────────

def test_ok_to_send_debounces(tmp_path):
    n, _ = _mk(tmp_path)
    now = time.time()
    assert n._ok_to_send("k", now)
    assert not n._ok_to_send("k", now + 1)
    assert n._ok_to_send("k", now + DEBOUNCE_SEC + 1)


def test_failed_delivery_rolls_back_debounce(tmp_path):
    n, _ = _mk(tmp_path)
    now = time.time()
    assert n._ok_to_send("k", now)
    # simulate the consumer's failure path
    with n._state_lock:
        if n._last_sent.get("k", 0) <= time.time():
            n._last_sent.pop("k", None)
    assert n._ok_to_send("k", time.time())


# ── persistence ───────────────────────────────────────────────────────────

def test_state_persists_across_restart(tmp_path):
    n, _ = _mk(tmp_path)
    now = time.time()
    n._ok_to_send("waste:n1:0:42", now)
    n._down["gpu1"] = now - 100
    n._jobs["7"] = {"jobname": "x", "user": "alice", "elapsed": "1:00"}
    n._save()
    n2 = Notifier(tmp_path, cfg_path=tmp_path / "webhook.json")
    assert "waste:n1:0:42" in n2._last_sent
    assert n2._down["gpu1"] == pytest.approx(now - 100)
    assert n2._jobs["7"]["user"] == "alice"


def test_save_prunes_expired_debounce_keys(tmp_path):
    n, _ = _mk(tmp_path)
    n._last_sent["ancient"] = 1.0  # 1970
    n._last_sent["fresh"] = time.time()
    n._save()
    assert "ancient" not in n._last_sent
    assert "fresh" in n._last_sent


# ── misc ──────────────────────────────────────────────────────────────────

def test_fmt_dur():
    assert _fmt_dur(120) == "2m"
    assert _fmt_dur(7200) == "2.0h"
    assert _fmt_dur(172800) == "2.0d"
    assert _fmt_dur(7200, "ko") == "2.0시간"
