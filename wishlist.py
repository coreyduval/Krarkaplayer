"""
wishlist.py — what the deck most wants to draw or fetch, given the board.

Selection effects (Brainstorm, Ponder, Frantic Search) and tutors (Gamble, ...) use
this to KEEP/FETCH the best cards instead of blind top-of-library, so the sim and the
planner's rollouts reflect real selective card advantage rather than random draws.

Priority is tiered, exactly as the pilot asked:
    Tier 1  state-specific wishes — fill the biggest gap in the engine right now
            (a payoff if none is accessible, a 2nd Krark body, a doubler, Krark's
            Thumb, a draw/mana engine the board lacks).
    Tier 2  generic engine cards — bodies, doublers, payoffs, gas.
    Tier 3  everything else (lands, interaction) ~ 0.

`card_value(state, name)` scores a single card; rank any drawn/searchable set by it.
"""
from __future__ import annotations

import cards

_PAYOFFS = frozenset({"Thassa's Oracle", "Grapeshot", "Brain Freeze"})
_BODIES = frozenset({"Krark, the Thumbless", "Sakashima of a Thousand Faces",
                     "Glasspool Mimic", "Phantasmal Image"})
_DOUBLERS = frozenset({"Veyran, Voice of Duality", "Harmonic Prodigy"})
_DRAW_ENGINES = frozenset({"Archmage Emeritus"})
_MANA_ENGINES = frozenset({"Storm-Kiln Artist", "Birgi, God of Storytelling"})
# Acceleration the pilot wants to TUTOR for by default — pieces that advance the game plan
# toward "power out the whole deck", after which the finish is trivially found. (Jeska's Will
# is the flagship ramp; scored specially below.)
_FAST_MANA = frozenset({"Sol Ring", "Mox Diamond", "Chrome Mox", "Lotus Petal",
                        "Arcane Signet", "Springleaf Drum", "Lion's Eye Diamond",
                        "Mana Vault", "Mox Amber", "Relic of Legends"})   # added 2026-06-21
_RITUALS = frozenset({"Pyretic Ritual", "Desperate Ritual", "Strike It Rich",
                      "Rite of Flame"})
# Clean finishes worth tutoring for unconditionally. Brain Freeze is NOT here: it only
# wins off a big self-mill, so it's only worth grabbing once a mill/draw engine is set up.
_CLEAR_PAYOFFS = frozenset({"Thassa's Oracle", "Grapeshot"})

# The Dualcaster combo: Dualcaster Mage + (Twinflame | Heat Shimmer) is a 2-card DETERMINISTIC
# kill (flash Dualcaster in response to the shimmer -> infinite hasty tokens). If you already hold
# one half, the other half is the single best card a tutor can find — it wins next turn — so it
# outranks generic acceleration. Spellseeker can fetch Twinflame (mv2); Imperial Recruiter fetches
# Dualcaster Mage; Gamble fetches either.
_COMBO_DUALCASTER = "Dualcaster Mage"
_COMBO_SHIMMERS = frozenset({"Twinflame", "Heat Shimmer"})


def _have(state, name: str) -> bool:
    return name in state.hand or state.has_permanent(name)


def _brain_freeze_ready(state) -> bool:
    """A clear Brain Freeze plan exists: 2+ bodies plus an engine to fuel repeated casts."""
    return state.krark_bodies >= 2 and (
        state.has_permanent("Archmage Emeritus")
        or state.has_permanent("Storm-Kiln Artist")
        or state.has_permanent("Birgi, God of Storytelling"))


def _combo_ready(state) -> bool:
    """The engine is assembled enough to power out the deck and close THIS turn-ish: 2+
    Krark bodies plus a sustaining mana/draw engine (or Breach to recur the finish). Only
    then is it worth burning a tutor on the end-game payoff — before that, Thassa's Oracle
    is dead weight you'd rather find naturally while digging."""
    return state.krark_bodies >= 2 and (
        any(state.has_permanent(m) for m in _MANA_ENGINES)
        or any(state.has_permanent(d) for d in _DRAW_ENGINES)
        or state.has_permanent("Urabrask")
        or state.has_permanent("Tavern Scoundrel")
        or state.has_permanent("Underworld Breach"))


def _ready_to_finish(state, name: str) -> bool:
    """Is it time to TUTOR `name` as the finish? Brain Freeze needs its mill plan; the
    Oracle/Grapeshot need the combo assembled."""
    if name == "Brain Freeze":
        return _brain_freeze_ready(state)
    return _combo_ready(state)

# Generic engine value (Tier 2) — the cards that are good in most spots.
_ENGINE = (_BODIES | _DOUBLERS | _DRAW_ENGINES | _MANA_ENGINES | _PAYOFFS | frozenset({
    "Krark's Thumb", "Baral, Chief of Compliance", "Underworld Breach",
    "Lion's Eye Diamond", "Jeska's Will", "Brainstorm", "Ponder", "Frantic Search",
    "Gamble", "Pyretic Ritual", "Desperate Ritual", "Strike It Rich",
    "Gitaxian Probe", "Peek", "Borne Upon a Wind", "Rite of Flame",   # added 2026-06-21 (cantrips + ritual)
    "Opt", "Consider", "Serum Visions", "Preordain",   # added 2026-06-21 (cut4 dig swap)
}))


def payoff_accessible(state) -> bool:
    """A finish is in hand, on the battlefield, or in the yard (Breach can escape it)."""
    return any(p in state.hand or state.has_permanent(p) or p in state.graveyard
               for p in _PAYOFFS)


def card_value(state, name: str, for_tutor: bool = False) -> float:
    """Higher = more wanted NOW. `for_tutor` distinguishes FETCHING (advance the plan —
    get acceleration; the end-game finish is found naturally) from KEEPING/selection
    (don't bottom your wincon). Tier-1 wishes dominate Tier-2 engine value, which
    dominates everything else."""
    score = 0.0
    is_body = name in _BODIES
    accessible = payoff_accessible(state)

    # Combo completion (top priority): holding one half of the Dualcaster combo, the missing half
    # is a guaranteed kill — fetch/keep it above everything else.
    have_dc = _have(state, _COMBO_DUALCASTER)
    have_shimmer = any(_have(state, sh) for sh in _COMBO_SHIMMERS)
    if name == _COMBO_DUALCASTER and have_shimmer and not have_dc:
        score += 120.0
    if name in _COMBO_SHIMMERS and have_dc and not have_shimmer:
        score += 120.0

    # Tier 1 — ACCELERATION: the pieces that advance the game plan, which the pilot wants
    # tutors to grab by default. A 2nd Krark body only matters once you already have one
    # (a library "body" is a clone — dead without a Krark to copy).
    if is_body and 1 <= state.krark_bodies < 2:
        score += 80.0
    if name in _DOUBLERS and state.trigger_doublers == 0:
        score += 60.0
    if name == "Krark's Thumb" and not state.has_permanent("Krark's Thumb"):
        score += 50.0
    if name in _DRAW_ENGINES and not any(state.has_permanent(d) for d in _DRAW_ENGINES):
        score += 50.0
    if name in _MANA_ENGINES and not any(state.has_permanent(m) for m in _MANA_ENGINES):
        score += 40.0
    # Ramp + fast mana: Jeska's Will is the flagship accelerant (explosive with a body out);
    # fast rocks/rituals power the turn out. These are the preferred default tutor targets.
    if name == "Jeska's Will":
        score += 60.0 if state.krark_bodies >= 1 else 35.0
    elif name in _FAST_MANA:
        score += 35.0
    elif name in _RITUALS:
        score += 20.0

    # Payoffs. Thassa's Oracle is an END-GAME finish — with a mana engine + a fully-seen
    # deck it's trivially found, so DON'T burn an early tutor on it. KEEP it when drawn
    # (never bottom the wincon), but only TUTOR a finish once the combo is assembled.
    if name in _PAYOFFS:
        if not for_tutor:                              # keeping/selection: hold the wincon
            score += 45.0 if name == "Thassa's Oracle" else 15.0
        if not accessible and _ready_to_finish(state, name):
            score += 100.0                             # engine assembled -> the finish is the best
                                                       # fetch (above a 3rd body / more acceleration),
                                                       # just under combo-completion (+120)

    # Tier 2 — generic engine value (payoffs already scored above).
    if name in _ENGINE and name not in _PAYOFFS:
        score += 10.0
        if is_body:
            score += 5.0

    # Tier 3 — lands / interaction / filler stay ~0.
    return score


def best(state, pool, k: int = 1, for_tutor: bool = False):
    """The k highest-value cards from `pool` (a list of names), best first."""
    return sorted(pool, key=lambda c: card_value(state, c, for_tutor), reverse=True)[:k]


def tutor(state, predicate):
    """Move the highest-value library card matching `predicate` into hand (the library
    is conceptually shuffled afterward). Returns the fetched card name, or None. Uses the
    tutor ranking — fetch acceleration, not the end-game finish."""
    matches = [c for c in state.library if predicate(c)]
    if not matches:
        return None
    fetched = best(state, matches, 1, for_tutor=True)[0]
    state.library.remove(fetched)
    state.hand.append(fetched)
    return fetched


if __name__ == "__main__":
    cards.load()
    from game_state import GameState, Permanent, krark_body

    # 1 body, no engine: a TUTOR should grab acceleration (2nd body / doubler / Jeska),
    # NOT the end-game Oracle (found naturally once the deck is powered out).
    s = GameState(battlefield=[krark_body("Krark, the Thumbless")])
    pool = {"Thassa's Oracle", "Sakashima of a Thousand Faces", "Veyran, Voice of Duality",
            "Jeska's Will", "Brainstorm", "Island", "Force of Will"}
    tut = sorted(pool, key=lambda c: card_value(s, c, for_tutor=True), reverse=True)
    print("1 body, no payoff -> TUTOR ranking:", tut)
    assert tut[0] != "Thassa's Oracle", "tutor should not grab the end-game Oracle first"
    assert card_value(s, "Thassa's Oracle", for_tutor=True) < card_value(s, "Jeska's Will", for_tutor=True)
    assert card_value(s, "Island") == 0.0
    # Keeping/selection still values the Oracle (don't bottom the wincon).
    assert card_value(s, "Thassa's Oracle") > card_value(s, "Island")
    # Once the combo is assembled, the finish becomes the top tutor target.
    s2 = GameState(battlefield=[
        krark_body("Krark, the Thumbless"),
        krark_body("Sakashima of a Thousand Faces", copy_of="Krark, the Thumbless"),
        Permanent("Storm-Kiln Artist", summoning_sick=False)])
    assert card_value(s2, "Thassa's Oracle", for_tutor=True) >= 70.0
    print("[ok] tutor grabs acceleration early, the finish once combo-ready")
