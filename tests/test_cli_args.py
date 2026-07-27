"""CLI argument surface.

The previous parser scanned argv by hand: unknown flags silently fell through
to the TUI, `logs -f JOBID` was rejected, and two verbs in one command line
resolved by source order rather than by complaining.
"""
import pytest

from sgpu.cli import _LEGACY_FLAGS, _build_parser, _normalize_argv


def parse(*argv):
    return _build_parser().parse_args(_normalize_argv(list(argv)))


# ── legacy spellings ──────────────────────────────────────────────────────

@pytest.mark.parametrize("flag,verb", sorted(_LEGACY_FLAGS.items()))
def test_every_legacy_flag_maps_to_its_verb(flag, verb):
    assert _normalize_argv([flag])[0] == verb


def test_legacy_flag_with_value_keeps_the_value():
    assert _normalize_argv(["--usage", "30"]) == ["usage", "30"]


def test_verb_is_hoisted_ahead_of_leading_options():
    # `sgpu --partition gpu --wait-free 2` worked under the old scanner.
    assert _normalize_argv(["--partition", "gpu", "--wait-free", "2"]) == [
        "wait-free", "--partition", "gpu", "2"]


def test_bare_verbs_are_untouched():
    assert _normalize_argv(["fit", "2", "--vram", "40"]) == [
        "fit", "2", "--vram", "40"]


# ── parsing ───────────────────────────────────────────────────────────────

def test_logs_accepts_flag_before_jobid():
    args = parse("logs", "-f", "12345")
    assert (args.cmd, args.jobid, args.follow) == ("logs", "12345", True)


def test_logs_accepts_jobid_before_flag():
    args = parse("logs", "12345", "-e")
    assert (args.jobid, args.err) == ("12345", True)


def test_usage_defaults_and_overrides():
    assert (parse("usage").days, parse("usage").daily) == (7, False)
    args = parse("--usage", "30", "--daily")
    assert (args.days, args.daily) == (30, True)


def test_fit_parses_vram_and_partition():
    args = parse("fit", "2", "--vram", "40", "--partition", "gpu")
    assert (args.count, args.vram, args.partition) == (2, 40.0, "gpu")


def test_wait_free_defaults():
    args = parse("wait-free", "3")
    assert (args.count, args.interval, args.partition) == (3, 10, "")


def test_jobs_user_filter():
    assert parse("--jobs", "14", "--user", "alice").user == "alice"


def test_no_arguments_selects_no_subcommand():
    assert parse().cmd is None


# ── rejection ─────────────────────────────────────────────────────────────

def test_unknown_flag_is_an_error_not_a_silent_tui_launch():
    with pytest.raises(SystemExit) as e:
        parse("--typo")
    assert e.value.code == 2


def test_two_verbs_are_rejected_rather_than_resolved_by_source_order():
    with pytest.raises(SystemExit) as e:
        parse("--waste", "--json")
    assert e.value.code == 2


def test_logs_without_a_jobid_is_an_error():
    with pytest.raises(SystemExit) as e:
        parse("logs")
    assert e.value.code == 2


def test_non_numeric_day_count_is_an_error():
    with pytest.raises(SystemExit) as e:
        parse("usage", "lots")
    assert e.value.code == 2
