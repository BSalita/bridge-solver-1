"""Bridge double-dummy analysis, par, and claim checking.

A fast double-dummy solver for contract bridge, with par scoring,
card-by-card play analysis, and a claim checker that decides whether a claim
holds however the unseen cards lie, the way BBO's does.

    >>> import bridge_solver as bs
    >>> deal = "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72"
    >>> bs.dd_table(deal)["N"]["S"]
    10
    >>> bs.check_claim(deal, "S", "N", 10)["verdict"]
    'invalid'

Conventions:

* A deal is PBN: ``"N:<north> <east> <south> <west>"``, each hand
  ``spades.hearts.diamonds.clubs``, e.g. ``"AKQT3.J6.KJ42.95"``.
* Seats are ``"N"``, ``"E"``, ``"S"``, ``"W"``; strains ``"C"``, ``"D"``,
  ``"H"``, ``"S"``, ``"NT"``; cards a suit letter then a rank, ``"SA"``,
  ``"HT"``.
* Plays are a list of cards, ``["HA", "H5", "H3", "H6"]``, or one string,
  ``"HA H5 H3 H6"``, starting with the opening lead.
* Anything the solver cannot use raises :class:`BridgeSolverError`, a
  ``ValueError``.

Every call releases the GIL while it computes, so threads can solve in
parallel. Source, documentation and the C library:
https://github.com/bridge-craftwork/bridge-solver
"""

from __future__ import annotations

import json
from typing import Any, Dict, Iterable, Optional, Union

from . import _native
from ._native import BridgeSolverError

__all__ = [
    "BridgeSolverError",
    "PlayAnalyzer",
    "SEATS",
    "STRAINS",
    "check_claim",
    "dd_table",
    "max_claim",
    "par",
    "release_memory",
    "solve",
]

__version__: str = _native.version()

#: Seats in clockwise order, as the solver names them.
SEATS = ("N", "E", "S", "W")
#: Strains in the order a double-dummy table lists them.
STRAINS = ("C", "D", "H", "S", "NT")

Plays = Union[str, Iterable[str]]


def _plays(plays: Plays) -> list:
    return plays.split() if isinstance(plays, str) else list(plays)


def _lho(seat: str) -> str:
    """The seat on `seat`'s left, which leads against it."""
    try:
        return SEATS[(SEATS.index(seat.upper()) + 1) % 4]
    except ValueError:
        raise BridgeSolverError(f'unknown seat "{seat}"') from None


def _play_request(deal: str, trump: str, declarer: str, plays: Plays, leader: Optional[str]) -> Dict[str, Any]:
    return {
        "dealstr": deal,
        "trump": trump,
        "declarer": declarer,
        "leader": leader if leader is not None else _lho(declarer),
        "plays": _plays(plays),
    }


def dd_table(deal: str) -> Dict[str, Dict[str, int]]:
    """The double-dummy table: tricks each seat takes as declarer in each strain.

    >>> table = dd_table("N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72")
    >>> table["N"]
    {'C': 8, 'D': 7, 'H': 8, 'S': 10, 'NT': 9}
    """
    rows = json.loads(_native.dd_table(deal))["tricks"]
    return {seat: dict(zip(STRAINS, row)) for seat, row in zip(SEATS, rows)}


def par(deal: str, vulnerable: str = "None") -> Dict[str, Any]:
    """Par for the deal, given who is vulnerable: ``"None"``, ``"NS"``,
    ``"EW"`` or ``"All"``.

    Returns ``{"score_ns": 420, "optimum_score": "NS 420",
    "par_contract": "NS 4S="}``. ``score_ns`` is from North-South's side.
    ``par_contract`` lists every contract tied at par, and is missing when
    the deal is passed out.
    """
    return json.loads(_native.par(deal, vulnerable))


def solve(deal: str, trump: str, leader: str) -> int:
    """North-South's double-dummy tricks with `leader` on lead in `trump`."""
    return _native.solve(deal, trump, leader)


def check_claim(
    deal: str,
    trump: str,
    declarer: str,
    tricks: int,
    *,
    plays: Plays = (),
    leader: Optional[str] = None,
    claimer: Optional[str] = None,
    budget: Optional[int] = None,
) -> Dict[str, Any]:
    """Whether a claim holds however the cards the claimer cannot see lie.

    `tricks` is the claiming side's total for the whole hand, counting tricks
    already won, as BBO shows it. `plays` is the play so far from the opening
    lead; `leader` defaults to declarer's left-hand opponent and `claimer` to
    declarer. A defender may claim too, but the claim must then work whatever
    partner holds and however partner plays.

    Returns ``{"verdict": "valid" | "invalid" | "undecided", "tricks_won": 2,
    "tricks_left": 11, "nodes": 19}``. ``"undecided"`` means the search ran
    out of its node `budget` (2,000,000 by default); treat it as rejected, as
    BBO does.
    """
    request = _play_request(deal, trump, declarer, plays, leader)
    request["total_tricks"] = tricks
    if claimer is not None:
        request["claimer"] = claimer
    if budget is not None:
        request["budget"] = budget
    return json.loads(_native.check_claim(json.dumps(request)))


def max_claim(
    deal: str,
    trump: str,
    declarer: str,
    *,
    plays: Plays = (),
    leader: Optional[str] = None,
    claimer: Optional[str] = None,
    budget: Optional[int] = None,
) -> Dict[str, Any]:
    """The largest claim the claimer could make: the most tricks their side
    is sure of, without seeing the hidden cards.

    Returns ``{"guaranteed": 10, "undecided": False, "tricks_won": 3,
    "tricks_left": 9, "nodes": 1234}``; ``guaranteed`` is a total for the
    hand. ``undecided`` is True when the budget ran out before one more trick
    could be ruled out, so the true answer may be higher. Call it once with
    declarer and once with a defender as `claimer` for both sides.
    """
    request = _play_request(deal, trump, declarer, plays, leader)
    if claimer is not None:
        request["claimer"] = claimer
    if budget is not None:
        request["budget"] = budget
    return json.loads(_native.check_claim(json.dumps(request)))


class PlayAnalyzer:
    """Double-dummy analysis of the play, card by card.

    Keeps a cache of solved positions, so analysing a hand as it is played
    pays for each new position once. Use one analyzer per thread.
    """

    def __init__(self) -> None:
        self._inner = _native.PlayAnalyzer()

    @property
    def cached_positions(self) -> int:
        """How many positions are cached."""
        return self._inner.cached_positions

    def clear_cache(self) -> None:
        """Forget every cached position and release the solver's memory."""
        self._inner.clear_cache()

    def trace(
        self, deal: str, trump: str, declarer: str, plays: Plays, *, leader: Optional[str] = None
    ) -> Dict[str, Any]:
        """Every card played, with the tricks it cost its side double dummy.

        Returns ``{"contract_tricks": 10, "trace": [{"index": 0, "seat": "E",
        "card": "HA", "cost": 0}, ...], "cached": False}``.
        """
        request = _play_request(deal, trump, declarer, plays, leader)
        return json.loads(self._inner.play(json.dumps(request)))

    def alternatives(
        self, deal: str, trump: str, declarer: str, plays: Plays, card: int, *, leader: Optional[str] = None
    ) -> Dict[str, Any]:
        """Every card that could have been played at position `card` (0-based
        into `plays`), with what each would have cost."""
        request = _play_request(deal, trump, declarer, plays, leader)
        return json.loads(self._inner.play_node(json.dumps(request), card))

    def optimal_line(
        self, deal: str, trump: str, declarer: str, plays: Plays, start: int, *, leader: Optional[str] = None
    ) -> Dict[str, Any]:
        """A double-dummy-perfect continuation from card `start` onwards."""
        request = _play_request(deal, trump, declarer, plays, leader)
        return json.loads(self._inner.optimal_line(json.dumps(request), start))


def release_memory() -> None:
    """Hand the solver's pooled memory on this thread back to the system."""
    _native.release_memory()
