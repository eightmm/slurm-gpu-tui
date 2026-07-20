"""Notifier state-machine tests: node down/recovered, collect-blind,
job-done diffing, debounce, persistence. No network — _post is stubbed."""
import json
import time
from datetime import datetime

import pytest

from sgpu.notify import Notifier, _fmt_dur, DEBOUNCE_SEC


def _mk(tmp_path, **cfg_over):
    cfg = {"bot_token": "xoxb-test", "channel": "#gpu", "node_health": False,
           "collect_alert": False, "rogue_alert": False, "ecc_alert": False}
    cfg.update(cfg_over)
    p = tmp_path / "webhook.json"
    p.write_text(json.dumps(cfg))
    n = Notifier(tmp_path, cfg_path=p)
    # Existing state-machine tests focus on alert replies. Treat today's
    # proactive parent as already created unless a test explicitly clears it.
    n._thread_day = datetime.now().strftime("%Y-%m-%d")
    n._thread_ts = "parent-ts"
    sent = []
    n._post = lambda text, key="": sent.append(text)
    return n, sent


def _node(name="gpu1", state="idle", gpus=None, **kw):
    d = {"name": name, "state": state, "partition": "gpu",
         "has_gpu": True, "stale": False, "error": "",
         "gpus": gpus if gpus is not None else [], "jobs": []}
    d.update(kw)
    return d


def test_ecc_alert_names_slot_and_device_node(tmp_path):
    n, sent = _mk(tmp_path, ecc_alert=True)
    gpu = {"index": "4", "minor": "4", "slot": "7", "name": "RTX 4090",
           "uuid": "GPU-29e3b221", "pci_bus": "00000000:81:00.0",
           "serial": "[N/A]", "ecc": "2", "pids": [], "users": []}
    n.process({"nodes": [_node(name="gpu7", gpus=[gpu])], "jobs": [], "errors": ""})
    assert len(sent) == 1
    assert "gpu7 GPU4" in sent[0]
    assert "slot 7" in sent[0] and "/dev/nvidia4" in sent[0]
    assert "UUID GPU-29e3b221" in sent[0] and "bus 00000000:81:00.0" in sent[0]
    assert "S/N" not in sent[0]  # [N/A] serial must be skipped


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


# ── job-fail ──────────────────────────────────────────────────────────────

def test_job_fail_alerts_on_bad_outcome(tmp_path):
    n, sent = _mk(tmp_path, job_fail_users=["*"])
    n._job_final_state = lambda jid: "OUT_OF_MEMORY"
    n.process({"nodes": [], "jobs": [_job()], "errors": ""})
    n.process({"nodes": [], "jobs": [], "errors": ""})
    assert len(sent) == 1 and "OUT_OF_MEMORY" in sent[0]


def test_job_fail_quiet_on_clean_finish(tmp_path):
    # COMPLETED job of a non-job_done user: no alert at all
    n, sent = _mk(tmp_path, job_fail_users=["*"])
    n._job_final_state = lambda jid: "COMPLETED"
    n.process({"nodes": [], "jobs": [_job()], "errors": ""})
    n.process({"nodes": [], "jobs": [], "errors": ""})
    assert sent == []


# ── pending-stuck ─────────────────────────────────────────────────────────

def _pend(jid="9", reason="Resources"):
    return {"jobid": jid, "user": "alice", "jobname": "big", "reason": reason}


def test_pending_stuck_alerts_after_threshold(tmp_path):
    n, sent = _mk(tmp_path, pending_alert_hours=1)
    snap = {"nodes": [], "jobs": [], "pending": [_pend()], "errors": ""}
    n.process(snap)
    assert sent == []  # just seen
    n._pend_seen["9"] -= 3601
    n.process(snap)
    assert len(sent) == 1 and "pending" in sent[0]


def test_pending_stuck_ignores_user_holds(tmp_path):
    n, sent = _mk(tmp_path, pending_alert_hours=1)
    snap = {"nodes": [], "jobs": [],
            "pending": [_pend(reason="Dependency")], "errors": ""}
    n.process(snap)
    assert n._pend_seen == {}  # never tracked, never alerts


# ── slack DM ──────────────────────────────────────────────────────────────

def test_job_done_also_dms_the_user(tmp_path):
    n, _ = _mk(tmp_path, job_done_users=["alice"],
               bot_token="xoxb-x", channel="#gpu",
               dm_users={"alice": "U012AB"})
    calls = []
    n._post = lambda text, key="", channel="": calls.append((text, channel))
    n.process({"nodes": [], "jobs": [_job()], "errors": ""})
    n.process({"nodes": [], "jobs": [], "errors": ""})
    channels = [c for _, c in calls]
    assert "" in channels and "U012AB" in channels  # channel post + DM


def test_notifier_disabled_without_bot_token(tmp_path):
    n, _ = _mk(tmp_path, bot_token="", job_done_users=["alice"],
               dm_users={"alice": "U012AB"})
    calls = []
    n._post = lambda text, key="", channel="": calls.append(channel)
    n.process({"nodes": [], "jobs": [_job()], "errors": ""})
    n.process({"nodes": [], "jobs": [], "errors": ""})
    assert calls == []


def test_legacy_webhook_url_does_not_enable_notifier(tmp_path):
    p = tmp_path / "webhook.json"
    p.write_text(json.dumps({"url": "https://example.invalid/hook"}))
    n = Notifier(tmp_path, cfg_path=p)
    assert not n.enabled


def test_delivery_always_uses_slack_api(tmp_path):
    n, _ = _mk(tmp_path)
    calls = []
    n._post_bot = lambda body, channel="": calls.append((body, channel)) or True
    assert n._deliver("alert", "U012AB")
    assert calls == [("alert", "U012AB")]


def test_daily_parent_created_without_alerts(tmp_path):
    n, sent = _mk(tmp_path)
    n._thread_day = ""
    n._thread_ts = ""
    calls = []
    n._slack_api = lambda method, payload: calls.append((method, payload)) or {"ts": "123.456"}

    n.process({"nodes": [], "jobs": [], "errors": ""})
    assert n._parent_worker is not None
    n._parent_worker.join(timeout=1)

    assert sent == []
    assert calls == [("chat.postMessage", {
        "channel": "#gpu",
        "text": n._m("parent", date=n._thread_day, sender=n.sender),
    })]
    assert n._thread_ts == "123.456"

    n.process({"nodes": [], "jobs": [], "errors": ""})
    assert len(calls) == 1


# ── misc ──────────────────────────────────────────────────────────────────

def test_fmt_dur():
    assert _fmt_dur(120) == "2m"
    assert _fmt_dur(7200) == "2.0h"
    assert _fmt_dur(172800) == "2.0d"
    assert _fmt_dur(7200, "ko") == "2.0시간"
