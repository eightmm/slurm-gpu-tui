"""Slack config discovery.

Same trap the usage history fell into: a root collector's ``~`` is /root, mode
0700, so a config an admin writes to "their" ~/.sgpu is one the collector can
never open — and the failure mode is alerts that silently never fire.
"""
import json
import os

import pytest

from sgpu import notify
from sgpu.notify import Notifier, _default_cfg_path, cfg_search_paths


@pytest.fixture(autouse=True)
def _isolate_system_path(tmp_path, monkeypatch):
    """Point the system config at an absent path under tmp.

    Otherwise a real /etc/sgpu/slack.json on the machine running the tests
    silently becomes "the first existing candidate" and the results depend on
    the host.
    """
    monkeypatch.setattr(notify, "SYSTEM_CFG_PATH", tmp_path / "etc" / "slack.json")
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    yield


def test_root_prefers_the_system_path(monkeypatch):
    monkeypatch.setattr(os, "geteuid", lambda: 0)
    assert cfg_search_paths()[0] == notify.SYSTEM_CFG_PATH


def test_unprivileged_prefers_the_home_path(monkeypatch, tmp_path):
    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    assert cfg_search_paths()[0] == tmp_path / "home" / ".sgpu" / "slack.json"


def test_system_path_is_always_searched(monkeypatch):
    # even unprivileged, so `sgpu doctor` as a user still finds a root
    # collector's config instead of reporting "not configured"
    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    assert notify.SYSTEM_CFG_PATH in cfg_search_paths()


def test_legacy_webhook_name_is_still_searched(monkeypatch, tmp_path):
    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    assert tmp_path / "home" / ".sgpu" / "webhook.json" in cfg_search_paths()


def test_first_existing_config_wins(monkeypatch, tmp_path):
    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    legacy = tmp_path / "home" / ".sgpu" / "webhook.json"
    legacy.parent.mkdir(parents=True)
    legacy.write_text("{}")
    assert _default_cfg_path() == legacy


def test_system_config_wins_for_root_over_a_home_one(monkeypatch, tmp_path):
    monkeypatch.setattr(os, "geteuid", lambda: 0)
    home_cfg = tmp_path / "home" / ".sgpu" / "slack.json"
    home_cfg.parent.mkdir(parents=True)
    home_cfg.write_text("{}")
    notify.SYSTEM_CFG_PATH.parent.mkdir(parents=True)
    notify.SYSTEM_CFG_PATH.write_text("{}")
    assert _default_cfg_path() == notify.SYSTEM_CFG_PATH


def test_preferred_path_returned_when_nothing_exists(monkeypatch, tmp_path):
    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    assert _default_cfg_path() == tmp_path / "home" / ".sgpu" / "slack.json"


def test_missing_home_does_not_raise(monkeypatch):
    def boom():
        raise RuntimeError("no home")

    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    monkeypatch.setattr("pathlib.Path.home", staticmethod(boom))
    assert cfg_search_paths() == [notify.SYSTEM_CFG_PATH]


def test_notifier_reads_a_config_found_by_search(monkeypatch, tmp_path):
    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    cfg = tmp_path / "home" / ".sgpu" / "slack.json"
    cfg.parent.mkdir(parents=True)
    cfg.write_text(json.dumps({"bot_token": "xoxb-x", "channel": "#gpu"}))
    n = Notifier(tmp_path)
    n._slack_api = lambda method, payload: None
    assert n.enabled and n.channel == "#gpu"


def test_unreadable_config_leaves_the_notifier_inert(tmp_path, monkeypatch):
    # 0600 by design; doctor must distinguish this from "not configured"
    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    cfg = tmp_path / "home" / ".sgpu" / "slack.json"
    cfg.parent.mkdir(parents=True)
    cfg.write_text(json.dumps({"bot_token": "xoxb-x", "channel": "#gpu"}))
    cfg.chmod(0o000)
    try:
        assert not Notifier(tmp_path, cfg_path=cfg).enabled
    finally:
        cfg.chmod(0o600)
