"""Tests for the bridge-solver Python package. Run with `pytest python/tests`."""

import threading

import pytest

import bridge_solver as bs

DEAL = "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72"
# East leads the HA, then the HK: two heart tricks for the defence.
TWO_HEARTS = "HA H5 H3 H6 HK H9 H7 HJ"


def test_version_matches_the_library():
    assert bs.__version__.count(".") == 2


def test_dd_table_is_keyed_by_seat_and_strain():
    table = bs.dd_table(DEAL)
    assert list(table) == ["N", "E", "S", "W"]
    assert table["N"] == {"C": 8, "D": 7, "H": 8, "S": 10, "NT": 9}
    assert table["E"]["S"] == 3


def test_par():
    result = bs.par(DEAL, "None")
    assert result["optimum_score"] == "NS 420"
    assert result["par_contract"] == "NS 4S="


def test_solve_counts_north_south():
    assert bs.solve(DEAL, "S", "E") == 10


def test_a_defender_claim():
    # East has the first two tricks. A third needs the DA, which declarer,
    # unseen by East, might ruff.
    assert bs.check_claim(DEAL, "S", "N", 2, plays=TWO_HEARTS, claimer="E")["verdict"] == "valid"
    third = bs.check_claim(DEAL, "S", "N", 3, plays=TWO_HEARTS.split(), claimer="E")
    assert third["verdict"] == "invalid"
    assert third["tricks_won"] == 2


def test_max_claim():
    result = bs.max_claim(DEAL, "S", "N", plays=TWO_HEARTS, claimer="E")
    assert result["guaranteed"] == 2
    assert result["undecided"] is False


def test_the_leader_defaults_to_declarers_left():
    # North declares, so East leads; an East card first is legal.
    assert bs.check_claim(DEAL, "S", "N", 2, plays="HA", claimer="E")["tricks_left"] == 13


def test_errors_are_bridge_solver_errors():
    with pytest.raises(bs.BridgeSolverError, match="could not parse deal"):
        bs.dd_table("not a deal")
    with pytest.raises(bs.BridgeSolverError, match="unknown seat"):
        bs.check_claim(DEAL, "S", "X", 10)
    with pytest.raises(ValueError):  # BridgeSolverError is a ValueError
        bs.par(DEAL, "sideways")


def test_play_analysis():
    analyzer = bs.PlayAnalyzer()
    result = analyzer.trace(DEAL, "S", "N", TWO_HEARTS)
    assert result["contract_tricks"] == 10
    assert [step["card"] for step in result["trace"]][:2] == ["HA", "H5"]
    assert analyzer.cached_positions > 0
    alternatives = analyzer.alternatives(DEAL, "S", "N", TWO_HEARTS, 0)
    assert alternatives
    analyzer.clear_cache()
    assert analyzer.cached_positions == 0


def test_threads_solve_in_parallel():
    results = []

    def work():
        results.append(bs.dd_table(DEAL)["N"]["S"])

    threads = [threading.Thread(target=work) for _ in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert results == [10, 10, 10, 10]
