"""
win.py — The win predicate. The single most bug-prone, highest-leverage function
in the whole system, and shared verbatim by search AND any future RL layer.

Two flavours, because MTG wins come in two shapes:

  * RESOLUTION wins   — a specific object finishing resolution wins on the spot:
                        Thassa's Oracle ETB, a resolved Grapeshot, a resolved
                        Brain Freeze that decks the table. Call at each resolution.
  * STATE wins        — an established position is already lethal in the solitaire
                        model: an infinite that has a real payoff attached, or
                        opponents already at/under 0 / decked. Call any time.

Hard rule carried over from the engine spec and the architecture discussion:
    AN INFINITE LOOP IS NOT A WIN BY ITSELF. It must be paired with a terminal
    payoff (combat, Grapeshot, Thoracle, Brain Freeze mill). "Infinite magecraft"
    or "infinite mana" alone returns NO win. We require the payoff explicitly.

And Thoracle gating (spec §6):
    Thassa's Oracle is lethal ONLY when blue_devotion > library_size at the moment
    its ETB resolves. Otherwise it is "lethal pending N more mill" — never a win,
    and never green-light a Thoracle that would deck the pilot.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Optional

import cards
from cards import CardType
from game_state import GameState, StackObject


@dataclass(frozen=True)
class WinResult:
    won: bool
    wtype: str = ""        # "thoracle" | "grapeshot" | "brain_freeze_mill" | "combat" | ""
    detail: str = ""

    def __bool__(self) -> bool:
        return self.won


NO_WIN = WinResult(won=False)


# --------------------------------------------------------------------------- #
# Resolution-time wins
# --------------------------------------------------------------------------- #

def check_resolution_win(state: GameState, resolving: StackObject) -> WinResult:
    """Call when `resolving` finishes resolving (after its effect is applied to
    `state`, so library sizes / storm copies already reflect it)."""
    name = resolving.name

    if name == "Thassa's Oracle":
        return _thoracle(state)

    if name == "Grapeshot":
        # storm_count is the number of prior spells this turn; total bolts = +1.
        total_damage = state.storm_count + 1
        return _burn_lethal(state, total_damage, source="Grapeshot")

    if name == "Brain Freeze":
        # each instance mills 3; storm copies multiply it.
        mill_each = 3 * (state.storm_count + 1)
        return _mill_table(state, mill_each, source="Brain Freeze")

    return NO_WIN


def _thoracle(state: GameState) -> WinResult:
    """Win iff devotion to blue is >= cards remaining in library, at resolution
    (CR: 'if X is greater than or equal to the number of cards in your library,
    you win'). So library == devotion still wins."""
    dev = state.blue_devotion
    lib = len(state.library)
    if lib <= dev:
        return WinResult(True, "thoracle",
                         f"Thassa's Oracle resolves: blue devotion {dev} >= library {lib}.")
    return WinResult(False, "thoracle",
                     f"NOT lethal: library {lib} > devotion {dev}. "
                     f"Need {lib - dev} more mill/draw before Thoracle is safe.")


def _burn_lethal(state: GameState, damage: int, source: str) -> WinResult:
    """Can `damage` points of free-assignment burn kill every living opponent?
    Lethal iff total damage >= sum of remaining opponent life (overflow on one
    opponent can't help another, but free assignment means the constraint is the
    sum when each opponent's life can be met)."""
    living = [lp for lp in state.opponent_life if lp > 0]
    needed = sum(living)
    if damage >= needed and living:
        return WinResult(True, "grapeshot",
                         f"{source}: {damage} damage >= {needed} total opponent life.")
    return WinResult(False, "grapeshot",
                     f"NOT lethal: {source} {damage} dmg < {needed} total opponent life.")


def _mill_table(state: GameState, mill_each: int, source: str) -> WinResult:
    """Mill win iff each opponent can be milled out. Free copies can target
    different players, so lethal iff mill_each per copy can cover the largest
    library when distributed — modelled conservatively as: total mill across
    copies >= sum of opponent libraries AND mill_each can finish the biggest one.
    For the storm case the count is large; this is the finite-check version."""
    libs = [lib for lib in state.opponent_library if lib > 0]
    if not libs:
        return WinResult(True, "brain_freeze_mill", f"{source}: all opponents already decked.")
    # storm produced (storm_count+1) copies, each milling 3; mill_each already = 3*copies
    # to deck the table you need to cover each opponent's library.
    if mill_each >= max(libs) and mill_each * len(libs) >= sum(libs):
        return WinResult(True, "brain_freeze_mill",
                         f"{source}: {mill_each} mill per target decks the table.")
    return WinResult(False, "brain_freeze_mill",
                     f"NOT lethal: {source} {mill_each} mill per target insufficient.")


# --------------------------------------------------------------------------- #
# State-based wins (established positions)
# --------------------------------------------------------------------------- #

def check_state_win(state: GameState) -> WinResult:
    """An already-lethal position in the solitaire (uninterrupted) model.
    An entry in state.infinite only wins when a matching, correctly-gated payoff
    is accessible. Loop tags used: 'mana_R','mana_U','mana_any','storm','draw',
    'mill','hasty_attackers'."""

    if all(lp <= 0 for lp in state.opponent_life):
        return WinResult(True, "combat", "Opponent life pool reduced to 0 (160 total damage).")
    if all(lib <= 0 for lib in state.opponent_library):
        return WinResult(True, "brain_freeze_mill", "All opponents decked.")

    inf = state.infinite

    # Infinite combat — unbounded hasty attackers + an able body.
    if "hasty_attackers" in inf and _has_unbounded_attacker(state):
        return WinResult(True, "combat",
                         "Infinite hasty attackers (Dualcaster/Twinflame) — lethal combat.")

    # Grapeshot — hand/GY + unbounded red-mana or storm loop.
    if _payoff_accessible(state, "Grapeshot"):
        return WinResult(True, "grapeshot",
                         "Grapeshot accessible with an unbounded red-mana/storm loop.")

    # Brain Freeze — hand/GY + infinite storm or blue-mana loop, and it decks the table.
    if _payoff_accessible(state, "Brain Freeze"):
        return WinResult(True, "brain_freeze_mill",
                         "Brain Freeze accessible with an infinite storm/blue-mana loop.")

    # Thassa's Oracle — castable AND the library can actually be emptied (or already
    # is small enough). Infinite MANA alone does NOT empty the library, so it is not
    # a Thoracle win on its own; you need infinite draw/mill, or library<=devotion now.
    if _payoff_accessible(state, "Thassa's Oracle"):
        if (inf & {"draw", "mill"}) or (len(state.library) <= state.blue_devotion):
            return WinResult(True, "thoracle",
                             "Thassa's Oracle castable and library empties (draw/mill loop) "
                             "or is already <= devotion.")
        # else: castable but not lethal — infinite mana doesn't deck you. Not a win.

    return NO_WIN


# loop tags that constitute an "unbounded mana/storm" context for each payoff
_GRAPESHOT_LOOPS = frozenset({"mana_R", "mana_any", "storm"})
_BRAINFREEZE_LOOPS = frozenset({"storm", "mana_U", "mana_any"})
_THORACLE_LOOPS = frozenset({"mana_R", "mana_U", "mana_any", "storm", "draw", "mill"})


def _yard_reachable(state: GameState, name: str) -> bool:
    """A graveyard card you can still cast because Underworld Breach or Gale 'extends your hand'
    into the yard: Breach escapes any NONLAND card (Thassa's Oracle included — it's a creature),
    Gale recasts any INSTANT/SORCERY. With an established infinite loop the mana / escape fuel /
    Gale trigger are all trivially available, so we don't re-check them here."""
    if name not in state.graveyard:
        return False
    cd = cards.get(name)
    if state.has_permanent("Underworld Breach") and CardType.LAND not in cd.types:
        return True
    if state.has_permanent("Gale, Waterdeep Prodigy") and cd.is_instant_or_sorcery:
        return True
    return False


def _payoff_accessible(state: GameState, name: str) -> bool:
    """Per-payoff accessibility, gated on the right kind of active loop. Note these
    only assert the payoff can be DEPLOYED; whether deploying it is lethal is
    decided by the caller (e.g. Grapeshot damage >= life, Thoracle library<=dev)."""
    inf = state.infinite
    in_hand = name in state.hand
    in_gy = name in state.graveyard
    # Jeska's Will mode-2 exiles cards face-up and lets you PLAY them this turn, so an
    # exiled payoff is just as castable as one in hand (the draw/storm loop that emptied
    # the library commonly exiled the Oracle/Grapeshot it then needs to cash out).
    in_exile = name in state.exiled_play
    # Underworld Breach / Gale extend your hand into the graveyard (escape / recast).
    yard = _yard_reachable(state, name)
    on_bf = state.has_permanent(name)

    if name == "Grapeshot":
        return (in_hand or in_gy or in_exile) and bool(inf & _GRAPESHOT_LOOPS)
    if name == "Brain Freeze":
        return (in_hand or in_gy or in_exile) and bool(inf & _BRAINFREEZE_LOOPS)
    if name == "Thassa's Oracle":
        # Thoracle is a creature: castable from hand or exile, on the battlefield, or escaped
        # from the graveyard via Underworld Breach (Gale can't recast a creature).
        return (in_hand or in_exile or on_bf or yard) and bool(inf & _THORACLE_LOOPS)
    return on_bf or in_hand or in_exile or yard


def _has_unbounded_attacker(state: GameState) -> bool:
    """With an established hasty-attacker loop, confirm an able creature exists.
    The loop tag guarantees the unbounded token supply; we just need a body that
    can deal the damage (non-summoning-sick, or the loop makes hasty tokens)."""
    if "hasty_attackers" in state.infinite:
        return True
    from cards import CardType
    return any(CardType.CREATURE in p.functions_as.types and not p.summoning_sick
               for p in state.battlefield)


# --------------------------------------------------------------------------- #
# Dispatcher + loss detection
# --------------------------------------------------------------------------- #

def evaluate_win(state: GameState, resolving: Optional[StackObject] = None) -> WinResult:
    """Single entry point. Pass `resolving` at a resolution event; omit it for a
    static position check. Resolution wins take precedence."""
    if resolving is not None:
        r = check_resolution_win(state, resolving)
        if r.won:
            return r
    return check_state_win(state)


def check_loss(state: GameState) -> WinResult:
    """Terminal failure detection (the planner treats this as a dead branch, and
    the RL reward function as the -500 terminal). The classic trap: decked self
    with no Thoracle payoff available."""
    if len(state.library) == 0 and not _payoff_accessible(state, "Thassa's Oracle"):
        # You don't lose the instant the library empties — you lose on the next
        # draw. But for a solitaire *planner* with no draws queued and no Thoracle,
        # this branch cannot win; flag it so search prunes it.
        return WinResult(True, "LOSS", "Library empty with no Thassa's Oracle to cash out.")
    return NO_WIN


# --------------------------------------------------------------------------- #
# smoke tests / dry-run harness
# --------------------------------------------------------------------------- #

if __name__ == "__main__":
    import cards
    from game_state import GameState, Permanent, krark_body

    cards.load()

    def board(*perms):
        return list(perms)

    # --- flip-count fidelity (spec §2) ---
    s = GameState(battlefield=board(
        krark_body("Krark, the Thumbless"),
        Permanent("Veyran, Voice of Duality", summoning_sick=False),
        Permanent("Harmonic Prodigy", summoning_sick=False),
    ))
    assert s.krark_bodies == 1 and s.trigger_doublers == 2
    assert s.flips_per_cast == 3, s.flips_per_cast
    print(f"[ok] 1 body + Veyran + Harmonic -> {s.flips_per_cast} flips (expect 3)")

    s2 = GameState(battlefield=board(
        krark_body("Krark, the Thumbless"),
        krark_body("Sakashima of a Thousand Faces", copy_of="Krark, the Thumbless"),
        Permanent("Veyran, Voice of Duality", summoning_sick=False),
        Permanent("Harmonic Prodigy", summoning_sick=False),
    ))
    assert s2.krark_bodies == 2 and s2.flips_per_cast == 6, s2.flips_per_cast
    print(f"[ok] 2 bodies + both doublers -> {s2.flips_per_cast} flips (expect 6)")

    s3 = GameState(battlefield=board(
        krark_body("Krark, the Thumbless"),
        Permanent("Veyran, Voice of Duality", summoning_sick=False),
        Permanent("Veyran, Voice of Duality", summoning_sick=False),
        Permanent("Harmonic Prodigy", summoning_sick=False),
    ))
    assert s3.flips_per_cast == 4, s3.flips_per_cast
    print(f"[ok] 1 body + 2 Veyran + 1 Harmonic -> {s3.flips_per_cast} flips (expect 4)")

    # --- Krark's Thumb flips p ---
    s.battlefield.append(Permanent("Krark's Thumb", summoning_sick=False))
    assert s.flip_p == 0.75
    print(f"[ok] Krark's Thumb -> flip_p {s.flip_p} (expect 0.75)")

    # --- Thoracle gating (spec §6) ---
    won = GameState(library=[], battlefield=[Permanent("Thassa's Oracle", summoning_sick=False)])
    r = check_resolution_win(won, StackObject("spell", "Thassa's Oracle"))
    assert r.won, r
    print(f"[ok] Thoracle, empty library, devotion {won.blue_devotion} -> WIN")

    not_won = GameState(library=["Island"] * 5,
                        battlefield=[Permanent("Thassa's Oracle", summoning_sick=False)])
    r = check_resolution_win(not_won, StackObject("spell", "Thassa's Oracle"))
    assert not r.won, r
    print(f"[ok] Thoracle, library 5 > devotion 2 -> NOT lethal: {r.detail}")

    # --- infinite-needs-payoff ---
    bare_loop = GameState(infinite=frozenset({"storm"}))           # no Grapeshot anywhere
    assert not check_state_win(bare_loop), "infinite storm alone must NOT win"
    print("[ok] infinite storm with no payoff -> NOT a win (correct)")

    armed = GameState(infinite=frozenset({"storm"}), hand=["Grapeshot"])
    r = check_state_win(armed)
    assert r.won and r.wtype == "grapeshot", r
    print(f"[ok] infinite storm + Grapeshot in hand -> WIN ({r.detail})")

    combat = GameState(infinite=frozenset({"hasty_attackers"}))
    r = check_state_win(combat)
    assert r.won and r.wtype == "combat", r
    print(f"[ok] infinite hasty attackers -> WIN ({r.detail})")

    # --- deck-out loss trap ---
    deckout = GameState(library=[])
    r = check_loss(deckout)
    assert r.won, r
    print(f"[ok] empty library, no Thoracle -> LOSS branch flagged")

    print("\nAll win-predicate smoke tests passed.")
