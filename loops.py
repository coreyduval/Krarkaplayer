"""
loops.py — Runaway / semi-infinite analysis.

The pilot's point: with a few Krark bodies, half the deck becomes semi-infinite.
A spell that RETURNS to hand on a lost flip can be recast, and if each cast nets
positive resource in expectation, the chain is a positive-feedback engine. It is
NOT a deterministic infinite — the chain ends when the spell finally resolves to
the graveyard (probability p^F per cast) or when mana runs dry on a bad streak —
so the right output is a probability of reaching lethal, not a KILL stamp.

Two tools:

  analyze_runaway(state, card)  -> RunawayAssessment
      Analytic, no RNG. Per cast: E[net mana], E[net cards], P(return), and the
      expected chain length 1/p^F. Classifies MANA_RUNAWAY / DRAW_RUNAWAY /
      POSITIVE_BUT_BOUNDED / NONE. The expected totals IGNORE mana-ruin (a bad
      streak stalling the chain), so they are upper-ish bounds — that's what the
      Monte-Carlo estimator is for.

  estimate_p_lethal(state, engine, payoffs, ...) -> dict
      Plays the recast chain with REAL mana payment (so ruin is captured), tracks
      mana / storm / library, and checks the accessible payoffs at the end:
        * Grapeshot  -> storm_count + 1 >= total living opponent life
        * Thoracle   -> library emptied to <= devotion (needs Thoracle accessible)
      Returns P(win) and a breakdown. THIS is the number for a probabilistic line.

Jeska's Will is the flagship: choosing both modes with a commander out, it adds
{R} per card in an opponent's hand each resolution — strongly mana-positive, and
it returns ~1-p^F of the time, so it snowballs. (Mode 2's exiled cards are extra
gas not modeled here, so estimates are conservative.)
"""
from __future__ import annotations

import random
from dataclasses import dataclass, field
from typing import Dict, List, Optional

import cards
from game_state import GameState
from resolver import (analyze_cast, resolve_cast_sample, untap_mana, STORM_SPELLS,
                      quasi_value)
import win as winmod
import wishlist

# Instant/sorcery tutors and the library filter they fetch under. Their develop value
# is the BEST card they can find (via the wishlist), not their raw card/mana delta.
_IS_TUTORS = {"Gamble": lambda c: True}      # Gamble fetches any card

# Cards removed from YOUR OWN library per resolution — the resource the Thassa's Oracle
# finish actually consumes (you win when blue devotion >= cards in library, so shrinking
# the library toward your devotion IS progress). Brain Freeze self-mills 3; Frantic Search
# nets -2 (draw 2, discard 2 to the yard); selective cantrips net -1.
_LIBRARY_REDUCTION = {
    "Brain Freeze": 3, "Frantic Search": 2,
    "Brainstorm": 1, "Ponder": 1, "Gamble": 1,
    "Gitaxian Probe": 1, "Peek": 1, "Borne Upon a Wind": 1,   # cantrips: draw 1 (library -1)
    "Opt": 1, "Consider": 1, "Serum Visions": 1, "Preordain": 1,   # added 2026-06-21 (cut4 dig swap)
}
_MILL_WEIGHT = 1.5                            # value per library-card removed toward the win
_BURN_WEIGHT = 1.5                            # value per point of opponent life removed toward the kill
_DIG_WEIGHT = 1.0                             # value per fresh card a cantrip digs to FIND the wincon
                                              # (no payoff in hand yet) — keeps the pilot active on a
                                              # live Krark engine instead of waiting to topdeck the kill
_TREASURE_BANK_WEIGHT = 0.5                   # value per Treasure banked for later turns (< a cantrip's dig)


# own-mana produced per RESOLUTION of the spell (red unless noted). For Jeska's
# Will the amount is dynamic (opponent hand), handled in analyze_runaway directly.
SPELL_RED_PER_RESOLUTION = {
    "Pyretic Ritual": 3,     # adds RRR
    "Desperate Ritual": 3,   # adds RRR
    "Rite of Flame": 2,      # {R} -> adds RR (net +R)
}
SPELL_GENERIC_PER_RESOLUTION = {
    "Strike It Rich": 1,     # a Treasure (any-color, count as 1 generic-capable)
}


@dataclass
class RunawayAssessment:
    card: str
    kind: str                      # MANA_RUNAWAY | DRAW_RUNAWAY | POSITIVE_BUT_BOUNDED | NONE
    flips: int
    p_return: float
    e_chain_len: float             # 1 / p^F  (expected casts before it sticks)
    e_net_mana_per_cast: float
    e_net_cards_per_cast: float
    e_total_mana: float            # e_net_mana_per_cast * e_chain_len (ignores ruin)
    e_total_cards: float
    notes: List[str] = field(default_factory=list)

    def summary(self) -> str:
        lines = [
            f"{self.card}: {self.kind}",
            f"  {self.flips} flips, P(return)={self.p_return:.3f}, "
            f"E[chain length]={self.e_chain_len:.1f} casts",
            f"  E[net mana/cast]={self.e_net_mana_per_cast:+.2f}  "
            f"E[net cards/cast]={self.e_net_cards_per_cast:+.2f}",
            f"  E[total mana over chain]={self.e_total_mana:.0f}  "
            f"E[total cards]={self.e_total_cards:.0f}",
        ]
        for n in self.notes:
            lines.append(f"  ! {n}")
        return "\n".join(lines)


def _cost_total(cost: Dict[str, int]) -> int:
    return sum(v for k, v in cost.items() if k != "X")


def analyze_runaway(state: GameState, card_name: str) -> RunawayAssessment:
    a = analyze_cast(state, card_name)
    F, p = a.flips, a.p
    notes = list(a.notes)

    # mana produced per cast = value-engine mana + treasures (as generic) + the
    # spell's own mana across its resolutions.
    eng_mana = sum(a.e_mana.values()) + a.e_treasures
    own_red = SPELL_RED_PER_RESOLUTION.get(card_name, 0)
    own_gen = SPELL_GENERIC_PER_RESOLUTION.get(card_name, 0) + untap_mana(state, card_name)
    if card_name == "Jeska's Will":
        own_red = max(state.opponent_hand) if state.opponent_hand else 0
        notes.append("Jeska's Will mode-2 (exile/play 3) gas is NOT modeled — estimate is conservative.")
    own_mana = (own_red + own_gen) * a.e_effect_resolutions

    cast_cost = _cost_total(state.cast_cost(card_name))
    e_net_mana = eng_mana + own_mana - cast_cost
    e_net_cards = a.e_draws - 1.0   # spent the card; gained a.e_draws from engines

    p_resolve = a.p_resolve
    e_chain = (1.0 / p_resolve) if p_resolve > 0 else float("inf")
    e_total_mana = e_net_mana * e_chain
    e_total_cards = e_net_cards * e_chain

    if F == 0:
        kind = "NONE"
        notes.append("No Krark body — no flips, no return, no runaway.")
    elif e_net_mana > 0 and a.p_return > 0.5:
        kind = "MANA_RUNAWAY"
    elif e_net_cards > 0 and a.p_return > 0.5:
        kind = "DRAW_RUNAWAY"
    elif e_net_mana > 0 or e_net_cards > 0:
        kind = "POSITIVE_BUT_BOUNDED"
        notes.append("Positive per cast but low P(return) — chain is short; verify with MC.")
    else:
        kind = "NONE"

    return RunawayAssessment(
        card=card_name, kind=kind, flips=F, p_return=a.p_return, e_chain_len=e_chain,
        e_net_mana_per_cast=e_net_mana, e_net_cards_per_cast=e_net_cards,
        e_total_mana=e_total_mana, e_total_cards=e_total_cards, notes=notes,
    )


# --------------------------------------------------------------------------- #
# Underworld Breach — graveyard cards gain escape (mana cost + exile 3 others)
# --------------------------------------------------------------------------- #

def gy_fuel(s: GameState, exclude: Optional[str] = None) -> int:
    """Cards in the graveyard usable as escape fuel — everything except (one copy
    of) the card currently being escaped. Lands count; the exile cost is any cards."""
    n = len(s.graveyard)
    if exclude is not None and exclude in s.graveyard:
        n -= 1
    return n


def can_escape(s: GameState, card: str) -> bool:
    """Underworld Breach gives every nonland card in your graveyard escape: you may
    cast it from the graveyard for its mana cost PLUS exiling three other cards from
    the graveyard. So `card` is escapable iff Breach is out, the card is in the yard,
    and there are >=3 other cards to pay the exile cost. (Mana is checked separately.)"""
    if not s.has_permanent("Underworld Breach") or card not in s.graveyard:
        return False
    if cards.CardType.LAND in cards.get(card).types:
        return False
    return gy_fuel(s, exclude=card) >= 3


def can_gale_recast(s: GameState, card: str) -> bool:
    """Gale, Waterdeep Prodigy: "Whenever you cast an instant or sorcery from your hand, you may
    cast up to one target card of THE OTHER TYPE from your graveyard. If a spell cast this way
    would be put into your graveyard, exile it instead."

    So a yard SORCERY is recastable only by casting an INSTANT from hand to trigger it, and a yard
    INSTANT only by casting a SORCERY — never the same type. We model that by requiring a hand spell
    of the opposite type to be available as the trigger. (Krark's return-to-hand PROTECTS the recast
    from exile: Gale's exile only replaces going to the GRAVEYARD, and a returned spell goes to HAND
    — handled in _do_cast, which exiles only a copy that actually resolved to the yard.)

    Once-per-hand-cast is approximated away; in a mixed develop chain you fire it most casts. Each
    yard spell is recast at most once (exiled after resolving), so it can't loop."""
    if not s.has_permanent("Gale, Waterdeep Prodigy") or card not in s.graveyard:
        return False
    cd = cards.get(card)
    if not cd.is_instant_or_sorcery:
        return False
    trigger = (cards.CardType.SORCERY if cards.CardType.INSTANT in cd.types
               else cards.CardType.INSTANT)             # the OTHER type must be castable from hand
    return any(trigger in cards.get(h).types for h in s.hand)


def crack_led(s: GameState) -> bool:
    """Sacrifice Lion's Eye Diamond if it's on the battlefield: discard the (already
    spent) hand into the graveyard and float three wildcard mana. Mirrors the
    'sac_hand' source in planner.MANA_SOURCES. In a Breach line this is pure upside —
    the discarded hand becomes escape fuel and the 3 wildcard can pay any payoff
    (e.g. Thoracle's {U}{U}). Returns True if it fired."""
    for i, p in enumerate(s.battlefield):
        if p.effective_name == "Lion's Eye Diamond":
            s.battlefield.pop(i)
            s.graveyard.extend(s.hand)     # hand -> yard: more escape fuel
            s.hand = []
            s.graveyard.append("Lion's Eye Diamond")   # sacrificed LED -> yard (re-escapable)
            s.mana.add("*", 3)             # "three mana of any one color" ~= 3 wildcard
            return True
    return False


def breach_led_mana(s: GameState, protect=()) -> bool:
    """Underworld Breach + Lion's Eye Diamond as a graveyard-mana engine: LED is a
    nonland card, so Breach lets you re-escape it from the yard for its {0} cost plus
    exiling three cards, then sacrifice it for 3 wildcard mana — and the sacrifice
    drops LED right back into the graveyard to be escaped again. Net: 3 graveyard
    cards -> 3 mana, repeatable while fuel lasts. LED is an artifact, so this does NOT
    trigger Krark flips or magecraft (no storm). Returns True if it fired."""
    if not can_escape(s, "Lion's Eye Diamond"):     # Breach out, LED in yard, >=3 other fuel
        return False
    s.graveyard.remove("Lion's Eye Diamond")        # escape onto the battlefield
    exile_fuel(s, 3, protect=protect)               # the escape exile cost
    s.mana.add("*", 3)                              # sac for three wildcard mana
    s.graveyard.append("Lion's Eye Diamond")        # sacrificed back into the yard
    return True


def exile_fuel(s: GameState, k: int, protect=()) -> None:
    """Pay an escape cost: exile k cards from the graveyard, keeping `protect`ed
    cards (the payoffs we still want to escape later) in the yard when possible."""
    removed = 0
    for card in list(s.graveyard):
        if removed >= k:
            break
        if card in protect:
            continue
        s.graveyard.remove(card)
        removed += 1
    while removed < k and s.graveyard:        # forced to dip into protected cards
        s.graveyard.pop()
        removed += 1


def _castable_now(s: GameState, name: str) -> bool:
    """Is `name` a spell we can CAST this turn from a non-battlefield zone? That's the
    hand, OR Underworld Breach escape from the graveyard, OR Jeska's Will mode-2 exile
    (`exiled_play` — "exile the top cards, you MAY PLAY them this turn"). The Jeska zone
    is the crux of the runaway: the same cast that empties the library also exiles your
    Oracle/Grapeshot face-up, and those are castable for the rest of the turn — counting
    only hand/escape silently threw the go-off's own payoff away (see _winning_payoff)."""
    return (name in s.hand or name in s.exiled_play
            or can_escape(s, name) or can_gale_recast(s, name))   # Gale recasts I/S from the yard


# Spells that, once cast, NET wildcard ("any color") mana via a Treasure token — the
# end-of-combo color fixer. With a live Krark engine each cast copies (more Treasures) and a
# lost flip returns it to hand to recast, so in a red-flooded runaway these make effectively
# unbounded blue. Costs come from cards data so we don't double-count their own price.
_TREASURE_SPELLS = ("Strike It Rich",)


def _convert_available(s: GameState, color: str, need: int) -> bool:
    """Can we pay `need` pips of `color` once COLOR CONVERSION is allowed? Beyond the pool's
    own `color`/wildcard, a red-flooded Jeska runaway reaches the Oracle's {U}{U} by:
      * tapping an untapped any-color / on-color mana source already in play (a land or rock
        the go-off hasn't tapped yet);
      * casting an any-color rock from hand off the surplus pool (Lotus Petal, a signet, a
        Mox — spend red, tap it for blue);
      * casting Strike It Rich — a live engine recasts it (lost-flip return) for unbounded
        Treasures (wildcard), so the deficit is coverable whenever there's red to seed it.
    This is the analytic counterpart to "I have infinite red and drew my converters." It does
    NOT fire without an actual converter, so a genuinely blue-screwed board still reads no-win."""
    from planner import MANA_SOURCES        # lazy: planner imports loops (circular at module load)

    pool = s.mana.pool
    have = pool.get(color, 0) + pool.get("*", 0) + s.mana.treasures   # Treasures pay any pip
    if have >= need:
        return True
    # Strike It Rich + a live engine + red to seed it -> effectively unbounded any-color mana.
    if s.flips_per_cast >= 1 and pool.get("R", 0) >= 2 \
       and any(_castable_now(s, c) for c in _TREASURE_SPELLS):
        return True
    # Otherwise sum concrete any-color/on-color production we can still tap or deploy.
    spare = s.mana.total() - have               # off-color mana free to spend on rocks
    for p in s.battlefield:                     # untapped sources already in play
        if p.tapped:
            continue
        src = MANA_SOURCES.get(p.effective_name)
        if src and src[0] in ("tap", "sac"):
            have += src[1].get("*", 0) + src[1].get(color, 0)
    for name in dict.fromkeys(s.hand):          # any-color rocks castable off the surplus pool
        src = MANA_SOURCES.get(name)
        if not src:
            continue
        yield_ = src[1].get("*", 0) + src[1].get(color, 0)
        cost = sum(s.cast_cost(name).values())
        if yield_ and spare >= cost:
            have += yield_
            spare -= cost
    return have >= need


def _winning_payoff(s: GameState, payoffs, need_life) -> Optional[str]:
    """Return the name of a payoff that is lethal RIGHT NOW in state s, else None.
    Checks every finish and returns the first that is actually lethal. A payoff in the
    graveyard only counts as castable when Underworld Breach can escape it (mana still
    required); one Jeska's-Will-exiled (`exiled_play`) counts too — playable this turn.
    Colored pips on the payoff may be paid through color conversion (_convert_available),
    so an all-red runaway with a Treasure-maker still cashes out the {U}{U} Oracle."""
    if need_life > 0 and all(lp <= 0 for lp in s.opponent_life):
        return "burn"        # Urabrask / Gut Shot per-cast damage has killed the table
    if all(lib <= 0 for lib in s.opponent_library):
        return "mill"        # the whole table is decked out -> they lose on their next draw
                             # (cumulative Brain Freeze mill across the recast loop)
    if "Thassa's Oracle" in payoffs and len(s.library) <= s.blue_devotion:
        if s.has_permanent("Thassa's Oracle") or \
           (_castable_now(s, "Thassa's Oracle") and _convert_available(s, "U", 2)):
            return "Thassa's Oracle"
    if "Grapeshot" in payoffs and s.storm_count + 1 >= need_life:
        if _castable_now(s, "Grapeshot") and _convert_available(s, "R", 1):
            return "Grapeshot"
    if "Brain Freeze" in payoffs:
        # each of the (storm+1) instances mills 3; decks the table if it covers every
        # opponent's library. (Self-mill -> Thoracle is handled by the Thoracle branch.)
        mill_each = 3 * (s.storm_count + 1)
        libs = [lib for lib in s.opponent_library if lib > 0]
        if libs and mill_each >= max(libs) \
           and _castable_now(s, "Brain Freeze") \
           and _convert_available(s, "U", 1):
            return "Brain Freeze"
    return None


def _do_cast(s: GameState, card: str, source: str, rng, payoffs, return_log=False):
    """Cast `card` from `source` ('hand' or 'escape'), paying mana — refuelling via LED
    recursion if short — and resolving with sampled Krark flips. Returns the resulting
    state, or None on mana ruin. Never exiles the card or a payoff as fuel.

    return_log=True returns (state, flip_log) so callers can report the coin-flip outcome
    (flips/wins/resolutions); on mana ruin it returns (None, None).

    Non-destructive: works on a private clone so the caller's state is untouched on failure.
    Critical for the multi-candidate loops (_rollout_from, SimGame._develop) that try casts
    in turn against one state — without this, a failed first attempt that cracked LED (mana +
    hand-to-yard, exiling fuel) would corrupt the state the NEXT candidate was gathered
    against (e.g. removing a card a later 'escape' candidate expected in the graveyard)."""
    s = s.clone()
    keep = tuple(payoffs) + (card,)
    cost = s.cast_cost(card)
    # An escaping card goes to the stack BEFORE its costs are paid, so it leaves the
    # graveyard up front. Critical: while we gather mana below, breach_led_mana's
    # exile_fuel can be forced to pop protected cards — if `card` were still in the
    # yard it could be cannibalised as its own escape fuel, then the remove() below
    # would crash. Pulling it now also keeps gy_fuel honest (it's no longer fuel).
    if source in ("escape", "gale"):                   # both leave the yard before paying costs
        s.graveyard.remove(card)
    while not s.mana.can_pay(cost) and breach_led_mana(s, protect=keep):
        pass
    if not s.mana.can_pay(cost):
        return (None, None) if return_log else None
    s.mana.pay(cost)
    if source == "hand":
        s.hand.remove(card)
    elif source == "escape":                           # Breach escape exiles 3 other yard cards
        exile_fuel(s, 3, protect=keep)
    # 'gale' has no extra cost (card already pulled from the yard above); it's exiled post-resolve.
    # Brain Freeze self-mills toward the Oracle ONLY once the Oracle is secured (in hand /
    # on board / Breach-escapable). If the Oracle is still in the library, self-milling
    # risks binning your own wincon into the yard (where it's dead without Breach), so mill
    # opponents instead — the cast still digs via magecraft, and your library stays safe.
    choices = None
    if card == "Brain Freeze":
        oracle_ready = ("Thassa's Oracle" in s.hand or s.has_permanent("Thassa's Oracle")
                        or can_escape(s, "Thassa's Oracle"))
        choices = {"target": "self" if oracle_ready else "opponents"}
    # s is already our private clone (cloned at the top of _do_cast) — let resolve mutate it
    # in place instead of cloning again (halves clones on the cast hot path).
    ns, log = resolve_cast_sample(s, card, rng, choices=choices, copy=False)
    if source == "gale" and card in ns.graveyard:      # Gale exiles the recast spell on resolution
        ns.graveyard.remove(card)                      # (a Krark-returned copy stays in hand, kept)
        ns.exile.append(card)
    return (ns, log) if return_log else ns


def _cast_source(s: GameState, card: str):
    """Where `card` can be cast from right now: 'hand', 'gale' (recast from yard, exiled after),
    'escape' (Breach, exile 3), or None. Gale is preferred over Breach — no fuel cost."""
    if card in s.hand:
        return "hand"
    if can_gale_recast(s, card):
        return "gale"
    if can_escape(s, card):
        return "escape"
    return None


# Per-cast burn engines: each I/S cast deals table damage that a recast loop adds up to a kill.
# Urabrask = 1 to one opponent (+{R}); Vivi Ornitier = 1 to EACH opponent -> 3 to the single 160
# pool (3x faster at table burn), and it's a Wizard so Harmonic doubles it and Veyran doubles the
# I/S-caused trigger. A board with either is a burn engine even with no payoff card in hand.
_BURN_ENGINES = ("Urabrask", "Vivi Ornitier")


def _has_burn_engine(s: GameState) -> bool:
    return any(s.has_permanent(n) for n in _BURN_ENGINES)


def estimate_p_lethal(state: GameState, engine_card: str,
                      payoffs=("Grapeshot", "Thassa's Oracle", "Brain Freeze"),
                      n_sims: int = 3000, max_iters: int = 80,
                      seed: Optional[int] = 0, decision_threshold: Optional[float] = None) -> dict:
    """Monte-Carlo P(win) for recasting `engine_card` until a payoff is lethal,
    the chain sticks (spell resolves to grave), or mana ruins. Captures mana-ruin
    and breaks the moment a win is reachable.

    decision_threshold: if set, stop once `p_win >= threshold` is mathematically LOCKED
    (lossless for the go-off gate; big savings at a high threshold)."""
    rng = random.Random(seed)
    wins = {pf: 0 for pf in payoffs}
    wins["any"] = 0
    deckouts = 0
    chain_lens = []
    need_life = sum(lp for lp in state.opponent_life if lp > 0)
    need = decision_threshold * n_sims if decision_threshold is not None else None

    ran = 0
    for _ in range(n_sims):
        s = state.clone()
        # In a Breach line, crack Lion's Eye Diamond up front: the spent hand falls
        # into the yard as fuel and 3 wildcard mana seeds the escape storm. Gated on
        # Breach so a plain Krark runaway never discards its own hand for mana.
        if s.has_permanent("Underworld Breach"):
            crack_led(s)
        iters = 0
        won_with = _winning_payoff(s, payoffs, need_life)
        while won_with is None and iters < max_iters:
            # Cast source: the hand (Krark returned it on a lost flip — no extra cost)
            # or, once it has resolved into the graveyard, an Underworld Breach escape
            # (pay again AND exile 3 cards) — what keeps a deep yard recasting a stuck
            # engine piece "in a pinch".
            source = _cast_source(s, engine_card)
            if source is None:
                break                          # nowhere left to cast it from
            ns = _do_cast(s, engine_card, source, rng, payoffs)
            if ns is None:
                break                          # mana ruin — no LED/fuel left to convert
            s = ns
            iters += 1
            won_with = _winning_payoff(s, payoffs, need_life)
        # Sustaining the recast loop to the cap (no mana ruin, never stuck) means the
        # mana engine is effectively unbounded — with Urabrask's per-cast damage on the
        # board, that's infinite damage and a kill (e.g. Tavern Scoundrel + a Krark army
        # feeding rituals into Urabrask).
        if won_with is None and iters >= max_iters and _has_burn_engine(s) \
           and any(lp > 0 for lp in s.opponent_life):
            won_with = "burn"
        chain_lens.append(iters)
        if won_with is not None:
            wins[won_with] = wins.get(won_with, 0) + 1
            wins["any"] += 1
        elif len(s.library) == 0:
            deckouts += 1
        ran += 1
        if need is not None:
            if wins["any"] >= need:                  # locked ABOVE threshold: will fire
                break
            if wins["any"] + (n_sims - ran) < need:  # locked BELOW: can't reach
                break

    return {
        "p_win": wins["any"] / ran if ran else 0.0,
        # every win category that actually occurred (incl. "burn"/"mill", not just the
        # payoff-card names) so callers can LABEL the line by what really closed it.
        "by_payoff": {k: v / ran for k, v in wins.items() if k != "any" and v > 0} if ran else {},
        "p_deckout_no_win": deckouts / ran if ran else 0.0,
        "mean_chain_len": sum(chain_lens) / len(chain_lens) if chain_lens else 0.0,
        "need_life_for_grapeshot": need_life,
    }


def prove_go_off(base, first, loop, rng,
                 payoffs=("Grapeshot", "Thassa's Oracle", "Brain Freeze"),
                 max_steps: int = 40, max_iters: int = 80, flip_sink=None) -> bool:
    """Replay a committed probabilistic go-off ONCE with `rng` (the REAL game flips) and
    return True iff it reaches a winning terminal. This PROVES a declared win actually
    resolves with this game's coin flips, instead of trusting the MC estimate — so an
    aggressive (low-threshold) go-off only counts when it really happens. Mirrors the
    single-sim body of rollout_estimate (develop line) / estimate_p_lethal (loop line).

    flip_sink: if a list is passed, every cast's (card, flip_log) is appended to it so the
    caller can REPORT the actual Krark coin-flip outcomes that produced the win."""
    if base is None or first is None:
        return False
    log_it = flip_sink is not None

    def _cast(s, card, src):
        if log_it:
            before = (dict(s.mana.pool), s.mana.treasures)
            ns, lg = _do_cast(s, card, src, rng, payoffs, return_log=True)
            if ns is not None:
                flip_sink.append({"kind": "cast", "card": card, "log": lg,
                                  "cost": dict(s.cast_cost(card)), "before": before,
                                  "after": (dict(ns.mana.pool), ns.mana.treasures)})
            return ns
        return _do_cast(s, card, src, rng, payoffs)

    need_life = sum(lp for lp in base.opponent_life if lp > 0)
    s = base.clone()
    if s.has_permanent("Underworld Breach"):
        crack_led(s)
    won = _winning_payoff(s, payoffs, need_life)
    if won is not None:
        return True
    card, src0 = first
    if loop:
        iters = 0
        while won is None and iters < max_iters:
            src = _cast_source(s, card)
            if src is None:
                break
            ns = _cast(s, card, src)
            if ns is None:
                break
            s = ns; iters += 1
            won = _winning_payoff(s, payoffs, need_life)
        if won is None and iters >= max_iters and _has_burn_engine(s) \
           and any(lp > 0 for lp in s.opponent_life):
            won = "burn"
        return won is not None
    # develop line: commit the first move, then play the develop policy forward once
    src = _cast_source(s, card) or src0
    ns = _cast(s, card, src)
    if ns is None:
        return False
    return _rollout_from(ns, payoffs, need_life, rng, max_steps - 1,
                         flip_sink=flip_sink) is not None


# --------------------------------------------------------------------------- #
# Rollout planner — value "develop now vs. hold" by playing a policy forward
# --------------------------------------------------------------------------- #
#
# The single-engine chain above only ever recasts ONE card. But the real decision —
# "is it worth casting Brainstorm NOW to dig / fill the yard / build the engine,
# then win two casts later with something else?" — is a sequential choice under
# coin-flip uncertainty, so a fixed threshold can't price it. Instead we roll a
# greedy DEVELOP POLICY forward and measure how often it reaches lethal. The policy's
# action choice is a soft heuristic (cheap, static per board); the value of acting is
# the measured win rate, not a hand-tuned lever.

# Pure payoffs are terminal — cast only when lethal, never as a develop spell.
PAYOFF_ONLY = frozenset({"Grapeshot", "Thassa's Oracle"})


# --- What loops under Krark — the value/win engines -------------------------------------------
# A spell loops profitably iff it RETURNS on a lost flip (any I/S with a Krark body), its NET
# per-cast resource is >= 0, and it doesn't destroy a resource it needs to repeat. The net-value
# math (analyze_cast -> analyze_runaway / develop_score) already folds in the per-cast engines,
# so BIRGI (+{R}/cast), URABRASK (+{R} +1 dmg/cast) and STORM-KILN (+Treasure/cast & copy) push
# an otherwise break-even spell positive — i.e. they turn cantrips and free spells INTO loops.
# Three families qualify:
_MANA_POSITIVE_LOOP = frozenset({           # net mana/Treasure on resolution, no target needed
    "Jeska's Will", "Rite of Flame", "Pyretic Ritual", "Desperate Ritual", "Strike It Rich"})
_CANTRIP_LOOP = frozenset({                 # net card-neutral dig (-> draw the deck into Oracle);
    "Brainstorm", "Ponder", "Gitaxian Probe", "Peek",   # with a per-cast mana engine, also mana-positive
    "Frantic Search", "Snap", "Borne Upon a Wind",
    "Opt", "Consider", "Serum Visions", "Preordain"})   # added 2026-06-21 (cut4 dig swap)
# STORM / non-destructive REDIRECT fuel: worth casting ONLY to fire storm + magecraft. Flusterstorm
# (Storm copies coexist + self-target, each a magecraft trigger) and Deflecting Swat (free, only
# REDIRECTS — destroys nothing — so it recasts as long as a spell/ability is on the stack). Gated
# on a value engine: that's both what pays off the casts AND what guarantees a legal target.
# NB: HARD COUNTERS are excluded everywhere — they CONSUME their target on resolution, so they
# can't loop (see cards._NO_SOLITAIRE_TARGET: An Offer, Fierce Guardianship, Force of Will, ...).
_MAGECRAFT_FUEL = frozenset({"Flusterstorm", "Deflecting Swat"})
# Permanents that turn a cast (or copy / won flip) into a resource — the payoff that makes the
# fuel spells (and otherwise break-even cantrips/free spells) loop, and whose triggers give
# Deflecting Swat its on-stack targets. Tavern Scoundrel is here too: it banks 2 Treasures per
# WON coin flip, and Krark flips on every cast, so it's a per-cast mana engine like the rest.
_CAST_VALUE_ENGINES = ("Storm-Kiln Artist", "Archmage Emeritus",
                       "Birgi, God of Storytelling", "Urabrask", "Tavern Scoundrel",
                       "Vivi Ornitier")   # per-cast table burn — fuel counters chip with it


def develop_candidates(s: GameState):
    """Instant/sorcery spells castable now for development (not pure payoffs): from
    hand, or via Breach escape from the yard. Must have a legal target in the solitaire
    model (counters with no opponent spell, etc. are skipped). Affordability is left to
    the cast step (LED can refuel). Returns [(card, source)]."""
    has_creature = any(cards.CardType.CREATURE in p.functions_as.types
                       for p in s.battlefield)
    cast_value_engine = any(s.has_permanent(n) for n in _CAST_VALUE_ENGINES)

    def ok(c):
        cd = cards.get(c)
        if not (cd.is_instant_or_sorcery and c not in PAYOFF_ONLY):
            return False
        if c in _MAGECRAFT_FUEL:
            return cast_value_engine          # only worth it as storm/magecraft fuel (+ legal target)
        return cards.castable_in_solitaire(c, has_creature)

    out, seen = [], set()
    for c in s.hand:
        if c not in seen and ok(c):
            out.append((c, "hand")); seen.add(c)
    for c in set(s.graveyard):                          # recast from the yard: Gale or Breach escape
        if c in seen or not ok(c):
            continue
        src = "gale" if can_gale_recast(s, c) else ("escape" if can_escape(s, c) else None)
        if src:
            out.append((c, src)); seen.add(c)
    return out


def _finish_progress(s: GameState, card: str, a) -> float:
    """Bonus for advancing the LIVE finish. Two finite-reachable finishes:
      * Thassa's Oracle — you win when blue devotion >= cards in your library, so once the
        Oracle is accessible, reward shrinking that gap by milling/drawing your own library
        (Brain Freeze self-mill especially).
      * Burn (Gut Shot et al.) — per-copy damage is modeled (resolver._DAMAGE_SPELLS), so
        reward chipping a living table toward 0. A lone Krark+Gut Shot chain self-terminates
        (no recursion), so this only surfaces burn as a candidate first move; the rollout
        (rollout_estimate) is what prices whether it actually closes (Urabrask / low life).
    Grapeshot/Brain-Freeze-table need an actual infinite (storm >= 120 / 32), which the
    deterministic search handles, not this chain."""
    need_life = sum(lp for lp in s.opponent_life if lp > 0)
    if a.e_damage > 0 and need_life > 0:
        # Burn only PROGRESSES toward a kill when it can actually close: Urabrask turns each
        # cast into table damage (the recast loop adds up), or this very cast already finishes
        # the remaining life. A lone Gut Shot chain self-terminates (~5 dmg) and can't touch a
        # 120-life table, so chipping it is a wasted "random cast" — score it 0.
        if _has_burn_engine(s) or a.e_damage >= need_life:
            return min(a.e_damage, need_life) * _BURN_WEIGHT
        return 0.0
    oracle = ("Thassa's Oracle" in s.hand or s.has_permanent("Thassa's Oracle")
              or can_escape(s, "Thassa's Oracle"))
    if not oracle:
        # No payoff in hand YET — but digging is how you FIND one. With a Krark engine a drawing
        # cantrip sees several cards a cast (copies), each a fresh look for the Oracle / Grapeshot /
        # a combo piece, so reward the dig. Keeps the pilot actively digging on a live engine instead
        # of sitting on it waiting to topdeck the kill (the seed-100 stall: 3 bodies idle ~10 turns).
        # ONLY the self-replacing DRAW cantrips (_CANTRIP_LOOP) — never Brain Freeze, whose self-mill
        # would just deck you toward a LOSS with no Oracle to cash out. The develop deck-out floor
        # still caps how far it digs.
        if s.flips_per_cast >= 1 and card in _CANTRIP_LOOP:
            return _LIBRARY_REDUCTION.get(card, 0) * a.e_effect_resolutions * _DIG_WEIGHT
        return 0.0
    gap = len(s.library) - s.blue_devotion        # cards that must still leave the library
    if gap <= 0:
        return 0.0
    removed = _LIBRARY_REDUCTION.get(card, 0) * a.e_effect_resolutions + a.e_draws
    return min(removed, gap) * _MILL_WEIGHT


_SINK_PAYOFFS = frozenset({"Grapeshot", "Thassa's Oracle", "Brain Freeze"})
# Engine PERMANENTS whose deployment is a real outlet for ritual mana. Deliberately excludes
# spells/rituals/cantrips (they aren't "deployed" — a ritual must not count itself as its own
# sink) and dead cards (Defense Grid, naked counters), so a hand of junk + a ritual gates off.
_SINK_PERMS = (wishlist._BODIES | wishlist._DOUBLERS | wishlist._DRAW_ENGINES
               | wishlist._MANA_ENGINES
               | frozenset({"Krark's Thumb", "Baral, Chief of Compliance", "Urabrask",
                            "Tavern Scoundrel", "Vivi Ornitier", "Valley Floodcaller",
                            "Okaun, Eye of Chaos", "Zndrsplt, Eye of Wisdom"}))


def _has_mana_sink(s: GameState) -> bool:
    """Is there anything this turn that could actually USE extra mana? A payoff to cast,
    Urabrask's per-cast burn, or an engine permanent still in hand to deploy. If not, making
    mana (a ritual) is pointless — it just floats and empties at cleanup."""
    if any(pf in s.hand or pf in s.graveyard or s.has_permanent(pf) for pf in _SINK_PAYOFFS):
        return True
    if _has_burn_engine(s):                        # Urabrask / Vivi: every cast is a burn outlet
        return True
    return any(c in _SINK_PERMS for c in s.hand)   # an engine permanent the mana could deploy


def develop_score(s: GameState, card: str) -> float:
    """Static, board-dependent value of casting `card` for development: expected net mana
    plus cards drawn per cast, PLUS progress toward the live finish (milling toward the
    Oracle). The board doesn't change mid-rollout (no permanents are played) so this is
    constant and can be cached per rollout."""
    a = analyze_cast(s, card)
    if card == "Quasiduplicate":
        # Token-copies a creature: fetch-creature tokens dig (ETB tutor), Krark tokens add
        # flip bodies. Valued by resolver.quasi_value (best fetch / engine growth × copies).
        return quasi_value(s, a.e_effect_resolutions)
    if card in _IS_TUTORS:
        # A tutor is worth the BEST card it can find. Gamble ALSO discards AT RANDOM — and
        # crucially, when Krark copies the cast each COPY searches-then-discards, so the
        # random discard fires once PER RESOLUTION. Looping Gamble into a multi-Krark board
        # therefore fetches your best card and then bins a fresh random card every copy — a
        # hand-shredder, not card selection. So weight the loss by the resolution count:
        # with bodies out the expected loss swamps the single fetch and Gamble scores < 0
        # (don't cast it), while a 0/1-body board still treats it as a clean one-shot tutor.
        flt = _IS_TUTORS[card]
        best = max((wishlist.card_value(s, c, for_tutor=True) for c in s.library if flt(c)),
                   default=0.0)
        avg_loss = (sum(wishlist.card_value(s, c) for c in s.hand) / len(s.hand)
                    if s.hand else 0.0)
        discards = max(1.0, a.e_effect_resolutions)   # one random discard per copy
        return (best - avg_loss * discards) / 20.0 + a.e_draws
    mana = sum(a.e_mana.values()) + a.e_treasures
    own = (SPELL_RED_PER_RESOLUTION.get(card, 0)
           + SPELL_GENERIC_PER_RESOLUTION.get(card, 0)
           + untap_mana(s, card))               # Frantic Search / Snap untap lands
    if card == "Jeska's Will":
        own += max(s.opponent_hand) if s.opponent_hand else 0
    mana += own * a.e_effect_resolutions
    cost = sum(v for k, v in s.cast_cost(card).items() if k != "X")
    finish = _finish_progress(s, card, a)
    # Treasures this cast BANKS — persistent ramp, available on later turns (not just floating
    # mana that empties at cleanup). Engine triggers (a.e_treasures) + the spell's own Treasure
    # (Strike It Rich makes one per resolution).
    treasures_made = a.e_treasures + (a.e_effect_resolutions if card in _TREASURE_SPELLS else 0.0)
    # NO SINK, NO RANDOM CASTS: a spell that yields ONLY mana (no draw, no dig/mill, no finish
    # progress) is worth casting only if that mana has an outlet this turn. Otherwise it just
    # floats and empties. EXCEPTION: a Treasure-maker banks any-color ramp that PERSISTS across
    # turns (fixes the {U}{U} for a later Oracle, funds a future combo), so it's worth a little
    # even with no sink now — but below a cantrip's dig value, so we don't durdle on it.
    pure_mana = (a.e_draws == 0 and finish == 0.0
                 and _LIBRARY_REDUCTION.get(card, 0) == 0 and a.e_damage == 0)
    if pure_mana and not _has_mana_sink(s):
        return treasures_made * _TREASURE_BANK_WEIGHT if treasures_made > 0 else -1.0
    return (mana - cost) + a.e_draws + finish


def max_draws(s: GameState, card: str) -> float:
    """Upper bound on cards a SINGLE cast of `card` removes from your library — the
    worst case where every Krark flip is WON (max magecraft copies, max coin-flip-win
    triggers). Decking out loses the game on your next draw, so the deck-out safety floor
    must assume this unlucky-max draw, not the expected draw: with a Zndrsplt-style
    draw-on-flip engine and a small library, casting at all can deck you out."""
    cd = cards.get(card)
    if not cd.is_instant_or_sorcery:
        return 0.0
    F = s.flips_per_cast
    storm = s.storm_count if card in STORM_SPELLS else 0
    max_copies = (1 + F + storm) if F > 0 else (1 + storm)   # cast + every flip won + storm
    total = 0.0
    for _perm, eng in s.value_engines():
        mult = s.value_multiplier(eng, cast_is_instant_or_sorcery=True)
        cause = eng.trigger_cause
        if cause == "is_cast_or_copy":
            events = max_copies
        elif cause in ("is_cast", "spell_cast"):
            events = 1
        elif cause == "coin_flip_win":
            events = F                                       # every flip won
        else:
            events = 0
        total += eng.draw_per_trigger * mult * events
    # the spell's own library bite (Brainstorm/Ponder/Frantic/Brain Freeze), once per copy
    total += _LIBRARY_REDUCTION.get(card, 0) * max_copies
    return total


def _rollout_from(s: GameState, payoffs, need_life, rng, max_steps: int,
                  flip_sink=None) -> Optional[str]:
    """Play the develop policy forward from `s` until a payoff is lethal, nothing is
    castable (mana/fuel ruin), the deck empties with no win, or the step cap. Returns
    the winning payoff name, or None. flip_sink: optional list collecting (card, log)
    per cast for the win-turn flip report."""
    score_cache = {}

    def score(c):
        if c not in score_cache:
            score_cache[c] = develop_score(s, c)
        return score_cache[c]

    for _ in range(max_steps):
        won = _winning_payoff(s, payoffs, need_life)
        if won:
            return won
        cands = develop_candidates(s)
        if not cands:
            break
        cands.sort(key=lambda cs: score(cs[0]), reverse=True)   # best develop first
        nxt = None
        for card, source in cands:                              # take the first castable
            if flip_sink is not None:
                before = (dict(s.mana.pool), s.mana.treasures)
                nxt, lg = _do_cast(s, card, source, rng, payoffs, return_log=True)
                if nxt is not None:
                    flip_sink.append({"kind": "cast", "card": card, "log": lg,
                                      "cost": dict(s.cast_cost(card)), "before": before,
                                      "after": (dict(nxt.mana.pool), nxt.mana.treasures)})
            else:
                nxt = _do_cast(s, card, source, rng, payoffs)
            if nxt is not None:
                break
        if nxt is None:
            break                                               # nothing affordable
        s = nxt
        if not s.library and not _winning_payoff(s, payoffs, need_life):
            break                                               # decked, no payoff
    else:
        # ran the full step cap without ruin: the engine is effectively unbounded, so
        # a per-cast burn engine (Urabrask / Vivi) is a kill.
        if _has_burn_engine(s) and any(lp > 0 for lp in s.opponent_life):
            return "burn"
    return _winning_payoff(s, payoffs, need_life)


def rollout_estimate(state: GameState, first,
                     payoffs=("Grapeshot", "Thassa's Oracle", "Brain Freeze"),
                     n_sims: int = 400, max_steps: int = 40,
                     seed: Optional[int] = 0, decision_threshold: Optional[float] = None) -> dict:
    """P(win) for committing to `first` = (card, source) NOW and then playing the
    develop policy. Captures the stochastic value of developing (dig/fill/build) versus
    holding — the answer to 'should I cast Brainstorm here?'.

    decision_threshold: if set, stop early the moment the comparison `p_win >= threshold`
    is mathematically LOCKED (no remaining sims can flip it) — lossless for the gate, and
    huge at a high threshold where most boards are clearly below. The reported p_win is the
    running rate over the sims actually run (always on the correct side of the threshold)."""
    rng = random.Random(seed)
    wins = {pf: 0 for pf in payoffs}
    wins["any"] = 0
    steps_used = []
    need_life = sum(lp for lp in state.opponent_life if lp > 0)
    fcard, fsrc = first
    need = decision_threshold * n_sims if decision_threshold is not None else None

    ran = 0
    for _ in range(n_sims):
        s = state.clone()
        if s.has_permanent("Underworld Breach"):
            crack_led(s)
        won = _winning_payoff(s, payoffs, need_life)
        if won is None:
            # crack_led may have dumped the hand into the yard — re-derive where the
            # committed first card is castable from now (hand vs. Breach escape). If it's
            # NOT castable anywhere (None), it must NOT fall back to the stale `fsrc`
            # ("hand"): _do_cast would then s.hand.remove() a card crack_led already
            # discarded -> ValueError, which (via pool.map re-raise) crashes whole sweeps.
            # An uncastable first move just doesn't fire this sim — same as mana ruin.
            src = _cast_source(s, fcard)
            ns = _do_cast(s, fcard, src, rng, payoffs) if src is not None else None
            if ns is None:
                steps_used.append(0); ran += 1
                if need is not None and wins["any"] + (n_sims - ran) < need:
                    break                                       # locked BELOW threshold
                continue
            s = ns
            won = _rollout_from(s, payoffs, need_life, rng, max_steps - 1)
        if won is not None:
            wins[won] = wins.get(won, 0) + 1
            wins["any"] += 1
        ran += 1
        if need is not None:
            if wins["any"] >= need:                             # locked ABOVE: will fire
                break
            if wins["any"] + (n_sims - ran) < need:             # locked BELOW: can't reach
                break
    # by_payoff over the win categories that actually occurred (incl. "burn"), so callers
    # label the line by what really won, not just the payoff-card list.
    return {
        "p_win": wins["any"] / ran if ran else 0.0,
        "by_payoff": {k: v / ran for k, v in wins.items() if k != "any" and v > 0} if ran else {},
    }


# --------------------------------------------------------------------------- #
# Loop detection — pattern-match assembled combos, set infinite tags
# --------------------------------------------------------------------------- #

@dataclass
class LoopReport:
    confirmed: frozenset                 # infinite tags genuinely established (free loop)
    reasons: List[str] = field(default_factory=list)
    candidates: List[tuple] = field(default_factory=list)   # (tags, description, caveat)

    def summary(self) -> str:
        out = []
        if self.confirmed:
            out.append(f"CONFIRMED loops -> infinite tags {sorted(self.confirmed)}")
            out += [f"  + {r}" for r in self.reasons]
        else:
            out.append("CONFIRMED loops: none")
        for tags, desc, caveat in self.candidates:
            out.append(f"CANDIDATE {sorted(tags)}: {desc}\n    caveat: {caveat}")
        return "\n".join(out)


def _add_costs(*costs: Dict[str, int]) -> Dict[str, int]:
    out: Dict[str, int] = {}
    for c in costs:
        for k, v in c.items():
            out[k] = out.get(k, 0) + v
    return out


def _engine_tags(state: GameState) -> set:
    """Extra infinite tags a hasty-attacker/magecraft loop also yields, given the
    value engines on board. Each Dualcaster ETB copies an I/S -> magecraft fires."""
    tags = set()
    if state.has_permanent("Archmage Emeritus"):
        tags.add("draw")            # infinite copies -> infinite draw -> Thoracle
    if state.has_permanent("Storm-Kiln Artist"):
        tags.add("mana_any")        # infinite treasures
    return tags


def _cheapest_instant_in_hand(state: GameState) -> Optional[str]:
    """Cheapest INSTANT in hand — the spell you cast to trigger Gale's recast of a graveyard
    SORCERY (the shimmers are sorceries, so the Gale trigger must be an instant)."""
    insts = [c for c in dict.fromkeys(state.hand)
             if cards.CardType.INSTANT in cards.get(c).types]
    return min(insts, key=lambda c: _cost_total(state.cast_cost(c)), default=None)


def _shimmer_start(state: GameState):
    """Cheapest way to get a Twinflame/Heat Shimmer onto the stack to START the Dualcaster combo:
      * cast it from HAND; or
      * escape it from the GRAVEYARD via Underworld Breach (pay its cost, exile 3 fuel); or
      * recast it from the GRAVEYARD via Gale — cast an instant from hand to trigger Gale (which
        recasts the sorcery shimmer; you still PAY its mana cost). Gale must be on the battlefield,
        or in hand (cast it first).
    Both Breach and Gale just 'extend your hand' into the yard for the combo's missing half.
    Returns (shimmer, total_cost_to_initiate, route) or (None, None, '')."""
    best = (None, None, "")

    def consider(sh, cost, route):
        nonlocal best
        if best[0] is None or _cost_total(cost) < _cost_total(best[1]):
            best = (sh, cost, route)

    gale_play = state.has_permanent("Gale, Waterdeep Prodigy")
    gale_hand = "Gale, Waterdeep Prodigy" in state.hand
    trig = _cheapest_instant_in_hand(state)
    for sh in ("Twinflame", "Heat Shimmer"):
        if sh in state.hand:
            consider(sh, state.cast_cost(sh), "in hand")
            continue
        if sh not in state.graveyard:
            continue
        if can_escape(state, sh):                          # Underworld Breach escape
            consider(sh, state.cast_cost(sh), "escaped from graveyard via Underworld Breach")
        if (gale_play or gale_hand) and trig is not None:  # Gale recast (instant trigger)
            c = _add_costs(state.cast_cost(sh), state.cast_cost(trig))
            route = f"recast from graveyard via Gale (trigger: cast {trig})"
            if not gale_play:
                c = _add_costs(c, state.cast_cost("Gale, Waterdeep Prodigy"))
                route += " after casting Gale"
            consider(sh, c, route)
    return best


def detect_loops(state: GameState) -> LoopReport:
    confirmed = set()
    reasons: List[str] = []
    candidates: List[tuple] = []

    # --- (Twinflame | Heat Shimmer) + Dualcaster Mage : free infinite hasty tokens ---
    # Heat Shimmer is functionally a second Twinflame (token copy with haste); either one
    # plus Dualcaster Mage loops: Dualcaster's ETB copies the shimmer spell, the copy makes a
    # hasty Dualcaster token, whose ETB copies the shimmer spell again -> infinite. The shimmer
    # can come from HAND or be recast from the GRAVEYARD via Gale (see _shimmer_start).
    dc_bodies = sum(1 for p in state.battlefield if p.effective_name == "Dualcaster Mage")
    shimmer, shimmer_cost, route = _shimmer_start(state)
    pieces = "Dualcaster Mage" in state.hand and shimmer is not None
    combined = _add_costs(shimmer_cost, state.cast_cost("Dualcaster Mage")) if pieces else {}
    if dc_bodies >= 2:
        confirmed |= {"hasty_attackers"} | _engine_tags(state)
        reasons.append("Multiple Dualcaster Mage bodies present — loop already established.")
    elif pieces and state.mana.can_pay(combined):
        confirmed |= {"hasty_attackers"} | _engine_tags(state)
        reasons.append(
            f"{shimmer} ({route}) + Dualcaster Mage with mana for both — flash Dualcaster in "
            f"response to {shimmer}; each {shimmer} copy makes a hasty Dualcaster token whose "
            f"ETB copies {shimmer} again. Infinite hasty attackers (and infinite magecraft "
            "copies). Assumes uninterrupted resolution; Krark flips only add copies.")
    elif pieces:
        candidates.append((
            frozenset({"hasty_attackers"}),
            f"{shimmer} ({route}) + Dualcaster Mage but not enough mana to start both.",
            f"need {combined} to initiate; current pool {state.mana.pool}."))

    # --- Underworld Breach + LED + Brain Freeze : storm/self-mill (sustained) ---
    breach = state.has_permanent("Underworld Breach") or "Underworld Breach" in state.hand
    led = (state.has_permanent("Lion's Eye Diamond") or "Lion's Eye Diamond" in state.hand
           or "Lion's Eye Diamond" in state.graveyard)
    bfreeze = "Brain Freeze" in state.graveyard or "Brain Freeze" in state.hand
    if breach and led and bfreeze:
        candidates.append((
            frozenset({"storm", "mill", "mana_any"}),
            "Underworld Breach + Lion's Eye Diamond + Brain Freeze — escape Brain Freeze "
            "repeatedly off LED mana for storm + self-mill into Thassa's Oracle (or table mill).",
            "sustainability depends on graveyard fuel (escape exiles 3 cards each) and net "
            "mana; NOT a free loop — verify with estimate_p_lethal / explicit GY accounting."))

    return LoopReport(confirmed=frozenset(confirmed), reasons=reasons, candidates=candidates)


def apply_loops(state: GameState) -> LoopReport:
    """Detect loops and fold CONFIRMED tags into state.infinite (candidates are
    left for the planner/simulator to validate — we never assert an unproven kill)."""
    report = detect_loops(state)
    if report.confirmed:
        state.infinite = state.infinite | report.confirmed
    return report


# --------------------------------------------------------------------------- #
# demonstration
# --------------------------------------------------------------------------- #

if __name__ == "__main__":
    cards.load()
    from game_state import GameState, Permanent, krark_body

    def board(*p): return list(p)

    # "A few Krarks." 2 bodies + both doublers + Archmage (draw) + Storm-Kiln (mana).
    base = dict(battlefield=board(
        krark_body("Krark, the Thumbless"),
        krark_body("Sakashima of a Thousand Faces", copy_of="Krark, the Thumbless"),
        Permanent("Veyran, Voice of Duality", summoning_sick=False),
        Permanent("Harmonic Prodigy", summoning_sick=False),
        Permanent("Archmage Emeritus", summoning_sick=False),
        Permanent("Storm-Kiln Artist", summoning_sick=False),
    ))

    print("=== analyze_runaway: Jeska's Will (2 bodies + both doublers) ===")
    s = GameState(library=["Island"] * 60, hand=["Jeska's Will"], **base)
    ra = analyze_runaway(s, "Jeska's Will")
    print(ra.summary())
    assert ra.kind == "MANA_RUNAWAY", ra.kind
    assert ra.e_net_mana_per_cast > 0

    print("\n=== estimate_p_lethal: Jeska chain -> Grapeshot/Thoracle ===")
    # Give a realistic kit: Jeska + Grapeshot + Thoracle in hand, library 40, seed mana.
    s2 = GameState(library=["Island"] * 40,
                   hand=["Jeska's Will", "Grapeshot", "Thassa's Oracle"],
                   opponent_life=(40, 40, 40), **base)
    s2.mana.add("R", 1); s2.mana.add("C", 2)   # enough for the first Jeska cast
    res = estimate_p_lethal(s2, "Jeska's Will", n_sims=4000, seed=1)
    print(f"P(win) = {res['p_win']:.3f}   by payoff: "
          + ", ".join(f"{k}={v:.3f}" for k, v in res['by_payoff'].items()))
    print(f"mean chain length = {res['mean_chain_len']:.1f} casts, "
          f"deck-out-without-win rate = {res['p_deckout_no_win']:.3f}, "
          f"Grapeshot needs {res['need_life_for_grapeshot']} damage")

    print("\n=== contrast: same engine, NO payoff in hand/GY ===")
    s3 = GameState(library=["Island"] * 40, hand=["Jeska's Will"], opponent_life=(40, 40, 40), **base)
    s3.mana.add("R", 1); s3.mana.add("C", 2)
    res3 = estimate_p_lethal(s3, "Jeska's Will", n_sims=2000, seed=2)
    print(f"P(win) = {res3['p_win']:.3f}  (no payoff accessible -> mana/draw alone is not a win)")
    assert res3["p_win"] == 0.0

    print("\n=== contrast: single body, no doublers (should NOT be a runaway) ===")
    s4 = GameState(library=["Island"] * 60, hand=["Jeska's Will"],
                   battlefield=board(krark_body("Krark, the Thumbless")))
    ra4 = analyze_runaway(s4, "Jeska's Will")
    print(ra4.summary())

    print("\n=== loop detector ===")
    import win as _win
    # Twinflame + Dualcaster in hand with RRR+2 mana -> confirmed infinite combat
    lp = GameState(library=["Island"] * 40, hand=["Twinflame", "Dualcaster Mage"],
                   battlefield=board(krark_body("Krark, the Thumbless"),
                                     Permanent("Archmage Emeritus", summoning_sick=False)))
    lp.mana.add("R", 3); lp.mana.add("C", 2)
    rep = apply_loops(lp)
    print(rep.summary())
    assert "hasty_attackers" in lp.infinite and "draw" in lp.infinite  # Archmage -> draw
    assert _win.evaluate_win(lp).wtype == "combat"
    print(f"  -> evaluate_win: {_win.evaluate_win(lp).detail}")

    # same pieces but no mana -> NOT confirmed, surfaced as a candidate
    lp2 = GameState(hand=["Twinflame", "Dualcaster Mage"])
    rep2 = detect_loops(lp2)
    assert not rep2.confirmed and rep2.candidates
    print("  [ok] no mana -> not confirmed, flagged as candidate")

    # Breach + LED + Brain Freeze -> candidate (not auto-asserted)
    lp3 = GameState(battlefield=board(Permanent("Underworld Breach", summoning_sick=False)),
                    hand=["Lion's Eye Diamond"], graveyard=["Brain Freeze"])
    rep3 = detect_loops(lp3)
    assert not rep3.confirmed and any("Breach" in c[1] for c in rep3.candidates)
    print("  [ok] Breach+LED+Brain Freeze -> candidate, not asserted as a kill")

    print("\nloops.py demonstration complete.")
