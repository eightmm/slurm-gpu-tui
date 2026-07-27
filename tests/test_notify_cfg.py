"""Slack config discovery.

Same trap the usage history fell into: a root collector's ``~`` is /root, mode
0700, so a config an admin writes to "their" ~/.sgpu is one the collector can
never open — and the failure mode is alerts that silently never fire.
"""
import json
import os

from sgpu.notify import SYSTEM_CFG_PATH, Notifier, _default_cfg_path, cfg_search_paths


def test_root_prefers_the_system_path(monkeypatch):
    monkeypatch.setattr(os, "geteuid", lambda: 0)
    assert cfg_search_paths()[0] == SYSTEM_CFG_PATH


def test_unprivileged_prefers_the_home_path(monkeypatch, tmp_path):
    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    monkeypatch.setenv("HOME", str(tmp_path))
    assert cfg_search_paths()[0] == tmp_path / ".sgpu" / "slack.json"


def test_system_path_is_always_searched(monkeypatch, tmp_path):
    # even unprivileged, so `sgpu doctor` as a user still finds a root
    # collector's config instead of reporting "not configured"
    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    monkeypatch.setenv("HOME", str(tmp_path))
    assert SYSTEM_CFG_PATH in cfg_search_paths()


def test_legacy_webhook_name_is_still_searched(monkeypatch, tmp_path):
    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    monkeypatch.setenv("HOME", str(tmp_path))
    assert tmp_path / ".sgpu" / "webhook.json" in cfg_search_paths()


def test_first_existing_config_wins(monkeypatch, tmp_path):
    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    monkeypatch.setenv("HOME", str(tmp_path))
    legacy = tmp_path / ".sgpu" / "webhook.json"
    legacy.parent.mkdir(parents=True)
    legacy.write_text("{}")
    assert _default_cfg_path() == legacy


def test_preferred_path_returned_when_nothing_exists(monkeypatch, tmp_path):
    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    monkeypatch.setenv("HOME", str(tmp_path))
    assert _default_cfg_path() == tmp_path / ".sgpu" / "slack.json"


def test_missing_home_does_not_raise(monkeypatch):
    def boom():
        raise RuntimeError("no home")

    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    monkeypatch.setattr("pathlib.Path.home", staticmethod(boom))
    assert cfg_search_paths() == [SYSTEM_CFG_PATH]


def test_notifier_reads_a_config_found_by_search(monkeypatch, tmp_path):
    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    monkeypatch.setenv("HOME", str(tmp_path))
    cfg = tmp_path / ".sgpu" / "slack.json"
    cfg.parent.mkdir(parents=True)
    cfg.write_text(json.dumps({"bot_token": "xoxb-x", "channel": "#gpu"}))
    n = Notifier(tmp_path)
    n._slack_api = lambda method, payload: None
    assert n.enabled and n.channel == "#gpu"
