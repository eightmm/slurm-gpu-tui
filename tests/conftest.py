"""Suite-wide safety nets."""
import urllib.request

import pytest


@pytest.fixture(autouse=True)
def no_outbound_http(monkeypatch):
    """Make a real network call impossible from a test.

    The notifier delivers from a daemon thread, so an unstubbed Notifier would
    POST a fixture's fake bot token at the real Slack API — after the summary
    line, where nobody looks, and only when CI happens to have egress. Turning
    that into an immediate error keeps the suite hermetic.
    """
    def _blocked(*_a, **_kw):
        raise AssertionError(
            "test attempted a real network call — stub the transport "
            "(e.g. Notifier._slack_api) instead"
        )

    monkeypatch.setattr(urllib.request, "urlopen", _blocked)
