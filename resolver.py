"""
resolver.py — The Krark cast resolver: the calculator core.

A single cast of an instant/sorcery, with b Krark bodies and the doublers out,
spawns F = flips_per_cast independent coin flips. Each won flip copies the spell;
the original returns to hand unless EVERY flip is won (spec §1). Copies resolve
regardless. The cast + each copy feeds the value engines through the subtype-aware
doubler multiplier.

STORM (Grapeshot / Brain Freeze / Flusterstorm): casting a storm spell triggers
Storm = copy it once per spell cast earlier this turn. So a single storm cast has
THREE copy sources, all of which deal/mill and all of which are "copy" events for
magecraft:
    storm copies  = storm_count BEFORE this cast        (deterministic)
    Krark copies  = flips won                           (E = F·p)
    original      = resolves iff all flips won          (P = p^F)

INVARIANT — neither Krark copies nor Storm copies are CAST, so:
  * neither increments the storm count (only the actual cast does, +1);
  * a Krark copy of a storm spell does NOT make its own Storm copies;
  * neither re-triggers Krark.
They DO still trigger Archmage/Storm-Kiln/Veyran (magecraft fires on cast OR copy).

Entry points:
  analyze_cast(state, name)        -> CastAnalysis  (exact EV/probabilities)
  resolve_cast_sample(state, name) -> (new_state, log)  (one playout step)
"""
from __future__ import annotations

import math
import random
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Tuple

import cards
import wishlist
from game_state import GameState, Permanent, krark_body

# Hard cap on coin flips sampled/analyzed per cast. The flip loop is O(F); with a runaway
# body-maker (Quasiduplicate cloning Krark) F can grow without bound and the sampler would
# hang. 40 flips already produces ~30 copies a cast — effectively infinite for any kill — so
# capping here is harmless for real boards while making pathological growth safe.
_MAX_FLIPS = 40
# Quasiduplicate stops adding Krark bodies past this many — more don't change the outcome
# (the engine is maxed and _MAX_FLIPS caps the flip payoff) and would only bloat the board.
_QUASI_BODY_CAP = 4
# Hard cap on token creatures Quasiduplicate adds total. Since it returns to hand on a lost
# flip, the develop loop recasts it every turn; without a ceiling the battlefield grows each
# cast and clone()/iteration crawl (a slowdown that looks like a hang — clone()/property
# access is O(battlefield) and runs billions of times in the rollouts). Kept small: 3-4 extra
# bodies already max the engine (and _MAX_FLIPS caps the payoff), so more only cost speed.
_QUASI_TOKEN_CAP = 5

# Spells with the Storm keyword in this deck.
STORM_SPELLS = {"Grapeshot", "Brain Freeze", "Flusterstorm"}
# Spells whose own effect deals 1 damage per instance (Grapeshot is storm-scaled;
# Gut Shot has no storm but Krark copies multiply it into a burn finish).
_DAMAGE_SPELLS = {"Grapeshot", "Gut Shot"}


# --------------------------------------------------------------------------- #
# Pre-cast analysis (exact, no sampling)
# --------------------------------------------------------------------------- #

@dataclass
class CastAnalysis:
    card: str
    flips: int
    p: float
    p_resolve: float
    p_return: float
    e_copies: float                  # Krark copies (E = F·p)
    e_storm_copies: int              # Storm-keyword copies (deterministic)
    e_effect_resolutions: float      # total times the spell's effect happens
    e_storm_after: int
    e_draws: float = 0.0
    e_treasures: float = 0.0
    e_mana: Dict[str, float] = field(default_factory=dict)
    e_damage: float = 0.0            # includes Grapeshot's own expected damage
    wins_pmf: List[float] = field(default_factory=list)
    notes: List[str] = field(default_factory=list)

    def summary(self) -> str:
        mana = ", ".join(f"{v:.2f}{k}" for k, v in self.e_mana.items()) or "0"
        lines = [
            f"cast {self.card}: {self.flips} flips @ p={self.p:.2f}"
            + (f"  (+{self.e_storm_copies} storm copies)" if self.e_storm_copies else ""),
            f"  P(resolve)={self.p_resolve:.4f}  P(return)={self.p_return:.4f}",
            f"  E[Krark copies]={self.e_copies:.2f}  E[effect resolutions]={self.e_effect_resolutions:.2f}",
            f"  E[draws]={self.e_draws:.2f}  E[treasures]={self.e_treasures:.2f}"
            f"  E[mana]={mana}  E[damage]={self.e_damage:.2f}",
            f"  storm -> {self.e_storm_after}",
        ]
        return "\n".join(lines + [f"  ! {n}" for n in self.notes])


def _binom_pmf(n: int, p: float) -> List[float]:
    return [math.comb(n, k) * p**k * (1 - p)**(n - k) for k in range(n + 1)]


def analyze_cast(state: GameState, card_name: str) -> CastAnalysis:
    cdef = cards.get(card_name)
    if not cdef.is_instant_or_sorcery:
        raise ValueError(f"{card_name} is not an instant/sorcery; Krark won't flip for it.")

    F = min(state.flips_per_cast, _MAX_FLIPS)
    p = state.flip_p
    notes: List[str] = []
    if F == 0:
        notes.append("No Krark body in play — casting triggers no flips (one normal cast).")

    storm_copies = state.storm_count if card_name in STORM_SPELLS else 0
    pmf = _binom_pmf(F, p) if F > 0 else [1.0]
    e_wins = F * p
    p_resolve = p**F if F > 0 else 1.0
    p_return = 1.0 - p_resolve
    e_copies = e_wins

    # effect happens: each Krark copy + original-if-all-won + each Storm copy
    e_effect_resolutions = (e_wins + p_resolve if F > 0 else 1.0) + storm_copies
    # magecraft events: the cast + Krark copies + Storm copies
    e_cast_copy_events = 1 + e_wins + storm_copies

    e_draws = e_treasures = e_damage = 0.0
    e_mana: Dict[str, float] = {}
    for perm, eng in state.value_engines():
        mult = state.value_multiplier(eng, cast_is_instant_or_sorcery=True)
        if perm.effective_name in cards.VERIFY:
            notes.append(f"{perm.effective_name}: {cards.VERIFY[perm.effective_name]} — "
                         f"output NOT certified; treat as estimate.")
        cause = eng.trigger_cause
        if cause == "is_cast_or_copy":
            events = e_cast_copy_events
        elif cause in ("is_cast", "spell_cast"):
            events = 1.0
        elif cause == "coin_flip_win":
            events = e_wins
        else:
            events = 0.0
        e_draws += eng.draw_per_trigger * mult * events
        e_treasures += (eng.treasure_per_trigger + eng.treasure_per_flip_win) * mult * events
        e_damage += eng.damage_per_trigger * mult * events
        for col, amt in eng.mana_per_trigger.items():
            e_mana[col] = e_mana.get(col, 0.0) + amt * mult * events

    if card_name in _DAMAGE_SPELLS:
        e_damage += e_effect_resolutions  # each instance deals 1
        notes.append(f"{card_name}: E[total bolts] = {e_effect_resolutions:.2f} "
                     f"({storm_copies} storm + {e_wins:.2f} Krark + {p_resolve:.3f} original).")

    return CastAnalysis(
        card=card_name, flips=F, p=p, p_resolve=p_resolve, p_return=p_return,
        e_copies=e_copies, e_storm_copies=storm_copies,
        e_effect_resolutions=e_effect_resolutions, e_storm_after=state.storm_count + 1,
        e_draws=e_draws, e_treasures=e_treasures, e_mana=e_mana, e_damage=e_damage,
        wins_pmf=pmf, notes=notes,
    )


# --------------------------------------------------------------------------- #
# Per-card effect scripts
# --------------------------------------------------------------------------- #

EFFECTS = {}

def effect(*names):
    def deco(fn):
        for n in names:
            EFFECTS[n] = fn
        return fn
    return deco


@effect("Ponder", "Brainstorm")
def _dig_cantrip(state, n, choices):
    # SELECTIVE card advantage, per instance: look at the top 3 and keep the BEST card
    # by the wishlist, leaving the rest on top. Net +1 card (Brainstorm draws 3 / puts 2
    # back; Ponder looks 3 / draws 1) — not the old blind +3, and it finds the right card.
    for _ in range(n):
        if not state.library:
            break
        keep = wishlist.best(state, state.library[:3], 1)[0]
        state.library.remove(keep)
        state.hand.append(keep)

# "Untap up to N lands" spells: each instance frees N (currently-tapped) lands, which
# you re-tap for mana. With Krark copies that's MORE mana than the cast cost, so these
# become mana engines (and Frantic Search a draw engine) — the line a pile of Krarks
# wants. Modelled as wildcard mana, capped by lands actually on the battlefield.
UNTAP_LANDS = {"Frantic Search": 3, "Snap": 2}


def untap_mana(state, card: str) -> int:
    """Mana freed per resolution by `card`'s 'untap up to N lands' (capped by lands
    in play). Wildcard, since you untap whatever colors you need."""
    cap = UNTAP_LANDS.get(card, 0)
    if not cap:
        return 0
    n_lands = sum(1 for p in state.battlefield if cards.CardType.LAND in p.cdef.types)
    return min(cap, n_lands)


# Never pitch a "good card" to a CHOSEN loot/discard (Frantic Search etc.). Like the mulligan
# protects mana SOURCES above all spells, the dig must protect the cards that actually WIN or
# enable a win — burying them in an over-aggressive dig is how a loaded engine ends up with no
# castable payoff (the seed-888 brick: Oracle/Grapeshot/Brain Freeze AND the Twinflame/Dualcaster
# combo AND both recursion enablers all got looted into a dead graveyard). This protects:
#   * payoffs — the only ways the game ends
#   * graveyard recursion — without these a binned payoff is unreachable (Gale recasts I/S from the
#     yard; Breach escapes any nonland — note Breach itself self-sacs at end step, win-turn only)
#   * the infinite-combat combo pieces (each is dead without its partner, so binning one forecloses
#     the whole line — keep both halves diggable)
# NB: applies to CHOSEN discards only. Gamble (random discard) and LED (discard your whole hand)
# are rules-forced costs and are modelled faithfully, not "protected".
_NEVER_DISCARD = frozenset({
    "Thassa's Oracle", "Grapeshot", "Brain Freeze",
    "Underworld Breach", "Gale, Waterdeep Prodigy",
    "Twinflame", "Heat Shimmer", "Dualcaster Mage",
})


# Mana rocks (mirror of sim._MANA_ROCKS). discard_rank uses these to spot SURPLUS mana you
# already have so a flooded hand pitches an extra rock/land before a unique spell.
_MANA_ROCK_NAMES = frozenset({
    "Sol Ring", "Mana Vault", "Chrome Mox", "Mox Amber", "Mox Diamond", "Lotus Petal",
    "Arcane Signet", "Springleaf Drum", "Relic of Legends"})


def _is_source(name: str) -> bool:
    return cards.CardType.LAND in cards.get(name).types or name in _MANA_ROCK_NAMES


def discard_rank(state, card: str) -> float:
    """Sort key for what to DISCARD (lowest = pitch first). Protects win-cons / recursion /
    combo pieces (+inf, kept unless the hand is nothing but those), then PREFERS DROPPING A
    RESOURCE YOU ALREADY HAVE — a duplicate of a permanent already on the battlefield, or a
    surplus mana source when you're not mana-light — ahead of any unique spell. Past that,
    the lowest wishlist value goes first. Shared by the loot discard and the end-of-turn
    discard-to-hand-size so both follow one policy."""
    if card in _NEVER_DISCARD:
        return float("inf")
    val = wishlist.card_value(state, card)
    redundant = 0.0
    # A permanent of this name is already on the battlefield -> the copy in hand is redundant.
    if any(p.effective_name == card or getattr(p, "name", None) == card
           for p in state.battlefield):
        redundant += 5.0
    # Surplus mana: extra sources once you already control/hold plenty are first to go.
    if _is_source(card):
        sources = sum(1 for p in state.battlefield if _is_source(p.effective_name)) \
            + sum(1 for c in state.hand if _is_source(c))
        if sources > 4:
            redundant += 3.0 + 0.5 * (sources - 4)
    return val - redundant


def _pitch_worst(state, k: int) -> None:
    """Discard k cards from hand to the graveyard by discard_rank (redundant resources first,
    NEVER a protected good card unless the hand is nothing but protected cards). Mirrors the
    mulligan's 'protect what wins, ship the chaff you already have spares of' policy."""
    for _ in range(k):
        pool = [c for c in state.hand if c not in _NEVER_DISCARD] or state.hand
        if not pool:
            return
        worst = min(pool, key=lambda c: discard_rank(state, c))
        state.hand.remove(worst)
        state.graveyard.append(worst)


@effect("Frantic Search")
def _frantic_search(state, n, choices):
    # Draw 2, discard 2 (loot, the pitched cards become Breach fuel), AND untap up to 3
    # lands -> wildcard mana. Net 0 cards; with Krark copies, strongly mana-positive.
    state.mana.add("*", untap_mana(state, "Frantic Search") * n)
    for _ in range(n):
        for _ in range(min(2, len(state.library))):
            state.hand.append(state.library.pop(0))
        _pitch_worst(state, min(2, len(state.hand)))

@effect("Snap")
def _snap(state, n, choices):
    # Untap up to 2 lands -> wildcard mana (the {1}{U} cost is refunded; with Krark
    # copies it nets positive). The bounce-a-creature is modelled as a no-op: you return
    # a spare/clone you don't need (or re-use an ETB), never a Krark body you want.
    state.mana.add("*", untap_mana(state, "Snap") * n)

@effect("Gamble")
def _gamble(state, n, choices):
    # Tutor for the single best wishlist card (the search IS your choice), then discard a
    # card AT RANDOM (the real downside — it can bin your wincon, even the card you just
    # found). Net 0 cards, library -1.
    rng = choices.get("_rng") or random.Random()
    for _ in range(n):
        if not state.library:
            break
        fetched = wishlist.best(state, state.library, 1, for_tutor=True)[0]
        state.library.remove(fetched)
        state.hand.append(fetched)
        if state.hand:
            pitched = rng.choice(state.hand)        # RANDOM, not chosen
            state.hand.remove(pitched)
            state.graveyard.append(pitched)

@effect("Pyretic Ritual", "Desperate Ritual")
def _ritual(state, n, choices):
    state.mana.add("R", 3 * n)

@effect("Rite of Flame")
def _rite_of_flame(state, n, choices):
    state.mana.add("R", 2 * n)   # {R} -> {R}{R}; net +{R} per resolution (no snow here)

@effect("Gitaxian Probe", "Peek", "Borne Upon a Wind",
        "Opt", "Consider", "Serum Visions", "Preordain")
def _plain_cantrip(state, n, choices):
    # "Draw a card" with no selection — take the top card per resolution. (Gitaxian Probe is
    # nearly free via Phyrexian {U/P}, but we model it at {U} for parity with Gut Shot; Borne
    # Upon a Wind's flash-granting is irrelevant in the solitaire model.)
    for _ in range(n):
        if state.library:
            state.hand.append(state.library.pop(0))

@effect("Strike It Rich")
def _strike(state, n, choices):
    state.mana.treasures += 1 * n  # a Treasure token per resolution: any color, persists to later turns

@effect("Jeska's Will")
def _jeskas_will(state, n, choices):
    # Commander out -> choose BOTH modes. n = total resolutions (the original + Krark
    # copies + storm); each resolution does both modes.
    # Mode 1: add {R} for each card in the SINGLE LARGEST opponent's hand (you target ONE
    #         opponent), times n. NOT the sum across opponents.
    state.mana.add("R", (max(state.opponent_hand) if state.opponent_hand else 0) * n)
    # Mode 2: exile the top 3 cards per resolution; you MAY play them this turn. Modelled
    #         as a turn-scoped castable zone (state.exiled_play); the sim plays the usable
    #         gas and discards the rest at end of turn.
    k = min(3 * n, len(state.library))
    for _ in range(k):
        state.exiled_play.append(state.library.pop(0))

@effect("Grapeshot", "Gut Shot")
def _grapeshot(state, n, choices):
    # n = total bolt instances (Storm + Krark + original); 1 damage each, free assign.
    # Gut Shot is the no-storm sibling — a single bolt that Krark copies multiply.
    D = n
    life = list(state.opponent_life)
    while D > 0 and any(l > 0 for l in life):
        j = min((j for j, l in enumerate(life) if l > 0), key=lambda j: life[j])
        take = min(D, life[j])
        life[j] -= take; D -= take
        if life[j] > 0:   # couldn't finish this one -> remaining can't kill it
            break
    state.opponent_life = tuple(life)

@effect("Brain Freeze")
def _brain_freeze(state, n, choices):
    # n = total instances; each mills 3. target 'opponents' (deck table) or 'self'
    # (feed Thassa's Oracle). Storm copies can split across players.
    mill = 3 * n
    if choices.get("target") == "self":
        for _ in range(min(mill, len(state.library))):
            state.graveyard.append(state.library.pop(0))
    else:
        libs = list(state.opponent_library)
        m = mill
        while m > 0 and any(l > 0 for l in libs):
            j = min((j for j, l in enumerate(libs) if l > 0), key=lambda j: libs[j])
            take = min(m, libs[j])
            libs[j] -= take; m -= take
            if libs[j] > 0:
                break
        state.opponent_library = tuple(libs)

@effect("Thassa's Oracle")
def _thoracle(state, n, choices):
    pass  # creature ETB; win decided by the predicate, not flipped


def quasi_target(state) -> Optional[str]:
    """What Quasiduplicate's token copy should be. Prefer a FETCH-CREATURE while we still
    need a finish (no payoff accessible): the token's ETB tutor digs for one. Otherwise copy
    KRARK for another flip body (engine growth). Returns the effective name to copy, or None
    if there's no worthwhile creature to copy."""
    # Cheap (no library scan): this runs inside every develop_score eval in the MC rollouts,
    # so it must be O(battlefield), not O(library)·wishlist. Bound total tokens first so
    # repeated develop-turn recasts can't bloat the battlefield.
    if sum(1 for p in state.battlefield if p.is_token) >= _QUASI_TOKEN_CAP:
        return None
    on_bf = {p.effective_name for p in state.battlefield}
    payoff_acc = any(pf in state.hand or pf in state.graveyard or state.has_permanent(pf)
                     for pf in ("Grapeshot", "Thassa's Oracle", "Brain Freeze"))
    # Still need a finish? Copy a fetch-creature to dig (Imperial Recruiter first — it finds
    # creatures incl. the Oracle and the engine bodies; Spellseeker for cheap I/S like Brain
    # Freeze). Which one fetches what is decided at resolution by apply_etb's wishlist.tutor.
    if not payoff_acc:
        for t in ("Imperial Recruiter", "Spellseeker"):
            if t in on_bf:
                return t
    # Otherwise copy Krark for another flip body, while bodies still matter (past a handful the
    # engine is maxed and _MAX_FLIPS caps the payoff, so more would only bloat the board).
    if "Krark, the Thumbless" in on_bf and state.krark_bodies < _QUASI_BODY_CAP:
        return "Krark, the Thumbless"
    return None


def quasi_value(state, resolutions: float) -> float:
    """Develop value of casting Quasiduplicate NOW. Each of its `resolutions` token copies
    either fetches a card (tutor-creature token's ETB) or adds a Krark body (which multiplies
    the whole engine). <=0 means 'no good target, don't bother'."""
    tgt = quasi_target(state)
    if tgt is None:
        return -1.0
    if tgt == "Krark, the Thumbless":
        return resolutions * 3.0          # an extra flip body compounds everything
    return resolutions * 2.0              # a tutor token ≈ one fetched piece per copy


@effect("Quasiduplicate")
def _quasiduplicate(state, n, choices):
    """Create n token copies of a creature you control (n = resolutions incl. Krark copies;
    jump-start lets it recur from the yard). Each token copies either a FETCH-CREATURE
    (Spellseeker / Imperial Recruiter — its ETB tutor fires, digging for a payoff/engine) or
    KRARK (another legend-safe flip body, the way the deck makes extra Krarks via Sakashima).
    Recomputed per token, so once a tutor finds a payoff the remaining tokens become bodies."""
    for _ in range(n):
        tgt = quasi_target(state)
        if tgt is None:
            break
        if tgt == "Krark, the Thumbless":
            # Legend-safe Krark body: Quasiduplicate copies a Sakashima-as-Krark, which keeps
            # Sakashima's legend-rule exemption. The model counts bodies by FUNCTION and does
            # not enforce the legend rule, so this just adds a flip body.
            state.battlefield.append(krark_body("Sakashima of a Thousand Faces",
                                                copy_of="Krark, the Thumbless", token=True))
        else:
            state.battlefield.append(Permanent(name=tgt, is_token=True, summoning_sick=True))
            apply_etb(state, tgt)         # the token's ETB tutor resolves


# --------------------------------------------------------------------------- #
# ETB tutors — creatures that search the library on entering the battlefield.
# They fetch via the wishlist (best card matching their filter), so they grab the
# piece the board most needs. (Imperial Recruiter's "power 2 or less" is approximated
# as any creature — every engine creature in the deck qualifies; CardDef has no power.)
# --------------------------------------------------------------------------- #

ETB_TUTORS = {
    "Spellseeker": lambda c: (cards.get(c).is_instant_or_sorcery
                              and cards.get(c).mana_value <= 2),
    "Imperial Recruiter": lambda c: cards.CardType.CREATURE in cards.get(c).types,
    # Partner-with: each fetches the other to hand on ETB.
    "Okaun, Eye of Chaos": lambda c: c == "Zndrsplt, Eye of Wisdom",
    "Zndrsplt, Eye of Wisdom": lambda c: c == "Okaun, Eye of Chaos",
}


def apply_etb(state, name: str) -> Optional[str]:
    """If `name` is a tutor creature entering the battlefield, fetch the best wishlist
    card matching its filter from library to hand. Returns the fetched card, or None."""
    pred = ETB_TUTORS.get(name)
    return wishlist.tutor(state, pred) if pred is not None else None


# --------------------------------------------------------------------------- #
# Sampling resolver (one playout step)
# --------------------------------------------------------------------------- #

def resolve_cast_sample(state: GameState, card_name: str,
                        rng: Optional[random.Random] = None,
                        choices: Optional[Dict] = None,
                        forced_wins: Optional[int] = None,
                        copy: bool = True) -> Tuple[GameState, dict]:
    """Mutates a CLONE: put spell on stack, flip, resolve Storm + Krark copies +
    value triggers, run the effect, return/resolve the original. Caller pays mana
    beforehand and reads win/loss via win.evaluate_win afterward.

    forced_wins: if given, the number of won flips is fixed (used by the planner's
    expand_chance to enumerate exact outcomes); otherwise flips are sampled.

    copy: clone `state` first (default). Pass copy=False when the caller already owns a
    private clone (loops._do_cast) — that throwaway can be mutated in place, saving a full
    clone on the hottest path (every cast in every rollout/estimate sim)."""
    choices = dict(choices or {})
    choices.setdefault("card", card_name)
    s = state.clone() if copy else state

    F, p = min(s.flips_per_cast, _MAX_FLIPS), s.flip_p
    storm_prior = s.storm_count
    storm_copies = storm_prior if card_name in STORM_SPELLS else 0
    s.storm_count += 1                       # ONLY the cast bumps storm; copies don't

    if forced_wins is not None:
        wins = max(0, min(forced_wins, F))
    else:
        rng = rng or random.Random()
        wins = sum(1 for _ in range(F) if rng.random() < p)
    all_won = (F == 0) or (wins == F)
    resolutions = (wins + (1 if all_won else 0)) if F > 0 else 1
    magecraft_events = 1 + wins + storm_copies

    log = {"flips": F, "wins": wins, "storm_copies": storm_copies,
           "resolutions": resolutions, "triggers": []}

    for perm, eng in s.value_engines():
        mult = s.value_multiplier(eng, cast_is_instant_or_sorcery=True)
        cause = eng.trigger_cause
        events = (magecraft_events if cause == "is_cast_or_copy"
                  else 1 if cause in ("is_cast", "spell_cast")
                  else wins if cause == "coin_flip_win" else 0)
        fires = events * mult
        if eng.draw_per_trigger:
            for _ in range(min(eng.draw_per_trigger * fires, len(s.library))):
                s.hand.append(s.library.pop(0))
        if eng.treasure_per_trigger or eng.treasure_per_flip_win:
            # Treasure tokens: any-color AND persistent (carried across turns), unlike the
            # floating pool — banked as .treasures, spent only after ephemeral mana.
            s.mana.treasures += (eng.treasure_per_trigger + eng.treasure_per_flip_win) * fires
        for col, amt in eng.mana_per_trigger.items():
            s.mana.add(col, amt * fires)
        if eng.damage_per_trigger and fires:
            # Urabrask: 1 damage to an opponent per I/S cast. Free-assign across the
            # living table (same as Grapeshot). With an infinite cast loop this is the kill.
            D = eng.damage_per_trigger * fires
            life = list(s.opponent_life)
            while D > 0 and any(l > 0 for l in life):
                j = min((j for j, l in enumerate(life) if l > 0), key=lambda j: life[j])
                take = min(D, life[j]); life[j] -= take; D -= take
            s.opponent_life = tuple(life)
        if fires:
            log["triggers"].append((perm.effective_name, fires))

    fn = EFFECTS.get(card_name)
    total_instances = resolutions + storm_copies
    choices["_rng"] = rng if rng is not None else random.Random()   # for random effects (Gamble)
    if fn is not None:
        fn(s, total_instances, choices)
    else:
        log.setdefault("warnings", []).append(f"effect for {card_name} not scripted")

    if F == 0 or all_won:
        s.graveyard.append(card_name)        # resolved to grave
    else:
        s.hand.append(card_name)             # returned by a lost flip
    return s, log


# --------------------------------------------------------------------------- #
# validation
# --------------------------------------------------------------------------- #

if __name__ == "__main__":
    cards.load()
    from game_state import GameState, Permanent, krark_body
    import win as winmod

    def board(*p): return list(p)

    # ---- EV with value engines (unchanged core) ----
    s = GameState(library=["Island"] * 40, hand=["Ponder"], battlefield=board(
        krark_body("Krark, the Thumbless"),
        Permanent("Archmage Emeritus", summoning_sick=False),
        Permanent("Storm-Kiln Artist", summoning_sick=False),
        Permanent("Veyran, Voice of Duality", summoning_sick=False),
        Permanent("Harmonic Prodigy", summoning_sick=False),
    ))
    a = analyze_cast(s, "Ponder")
    assert abs(a.p_resolve - 0.125) < 1e-9 and abs(a.e_draws - 7.5) < 1e-9
    print("[ok] value-engine EV unchanged (P(resolve)=0.125, E[draws]=7.5)")

    # ---- storm math: Grapeshot with 9 prior spells, 1 Krark + both doublers (F=3) ----
    g = GameState(storm_count=9, hand=["Grapeshot"], opponent_life=(40, 40, 40),
                  battlefield=board(
                      krark_body("Krark, the Thumbless"),
                      Permanent("Veyran, Voice of Duality", summoning_sick=False),
                      Permanent("Harmonic Prodigy", summoning_sick=False)))
    ag = analyze_cast(g, "Grapeshot")
    expect = 9 + 3 * 0.5 + 0.5**3       # storm + E[Krark wins] + P(original)
    assert abs(ag.e_damage - expect) < 1e-9, (ag.e_damage, expect)
    print(f"[ok] Grapeshot E[bolts]={ag.e_damage:.3f} = 9 storm + 1.5 Krark + 0.125 original")

    # ---- deterministic storm kill: no Krark, 9 prior spells, opps at 3 life ----
    g2 = GameState(storm_count=9, hand=["Grapeshot"], opponent_life=(3, 3, 3))
    ns, log = resolve_cast_sample(g2, "Grapeshot")
    # instances = 1 original + 9 storm = 10 bolts; 10 >= 9 total life -> table dead
    assert all(l <= 0 for l in ns.opponent_life), ns.opponent_life
    assert winmod.evaluate_win(ns).won
    print(f"[ok] Grapeshot (storm 9, no Krark) deals 10 -> kills 3x3 table: {ns.opponent_life}")

    # ---- Brain Freeze self-mill feeds Thoracle ----
    bf = GameState(storm_count=5, library=["Island"] * 18, hand=["Brain Freeze"],
                   battlefield=board(Permanent("Thassa's Oracle", summoning_sick=False)))
    ns2, _ = resolve_cast_sample(bf, "Brain Freeze", choices={"target": "self"})
    # instances = 1 + 5 storm = 6; mills 18 -> library 0; devotion 2 -> Thoracle lethal
    print(f"[ok] Brain Freeze self-mill: library {len(bf.library)} -> {len(ns2.library)}; "
          f"devotion {ns2.blue_devotion} -> Thoracle "
          f"{'LETHAL' if len(ns2.library) <= ns2.blue_devotion else 'pending'}")

    # ---- INVARIANT: copies are not cast, so they never count for storm ----
    inv = GameState(library=["Island"] * 200, storm_count=0, hand=["Ponder"],
                    battlefield=board(
                        krark_body("Krark, the Thumbless"),
                        krark_body("Sakashima of a Thousand Faces", copy_of="Krark, the Thumbless"),
                        Permanent("Veyran, Voice of Duality", summoning_sick=False),
                        Permanent("Harmonic Prodigy", summoning_sick=False)))
    assert inv.flips_per_cast == 6
    forced_wins = random.Random(7)
    ns3, log3 = resolve_cast_sample(inv, "Ponder", forced_wins)
    assert log3["wins"] >= 1, "test needs at least one Krark copy to be meaningful"
    assert ns3.storm_count == 1, ns3.storm_count          # 1 cast, NOT 1 + Krark copies
    # now the next storm spell sees only the single prior CAST, not the copies:
    ns3.hand.append("Grapeshot")
    ag2 = analyze_cast(ns3, "Grapeshot")
    assert ag2.e_storm_copies == 1, ag2.e_storm_copies    # == prior casts, ignores copies
    print(f"[ok] INVARIANT: Ponder made {log3['wins']} Krark copies, storm still = "
          f"{ns3.storm_count}; next Grapeshot sees {ag2.e_storm_copies} storm copy (cast only)")

    print("\nResolver storm-math validation passed.")
