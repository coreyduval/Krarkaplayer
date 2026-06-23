"""
game_state.py — The simulator's source of truth.

ONE object holds the entire solitaire game state. It is:
  (1) an EXACT game state the search planner mutates/clones, and
  (2) the thing that produces the coarse observation vector for a future
      AlphaZero-style guiding network (encode_observation()).

Both consumers read from this object; neither owns its own copy of "the state".

Key modelling choices, and why:
  * Ordered zones (library, stack) are lists — order is semantically load-bearing
    (Brainstorm/Ponder/top-of-library; stack is LIFO and trigger ordering matters).
  * Unordered zones (hand, battlefield, graveyard, exile) are lists too, but the
    canonical_key() projects them into sorted multisets so structurally identical
    states memoize to the same key.
  * Coin flips are NOT resolved inside the state. A pending Krark flip trigger is
    an explicit StackObject; the search branches into chance nodes when it pops.
    This is exactly what lets us read off P(lethal) instead of sampling one path.
"""
from __future__ import annotations

import copy
import functools
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Tuple

import cards
from cards import CardDef


# A permanent's FUNCTIONAL identity depends only on (name, copy_of), which never change
# after it's created — but `functions_as` / `is_krark_body` / `blue_pips` were recomputed
# (via cards.get) on every access, and the battlefield genexprs hit them billions of times
# in the MC rollouts. Memoize on (name, copy_of) so each is a one-time CardDef lookup.
@functools.lru_cache(maxsize=None)
def _functions_as(name: str, copy_of: Optional[str]) -> CardDef:
    return cards.get(copy_of) if copy_of else cards.get(name)


# The partner commanders. Controlling either turns on the "cast without paying its
# mana cost" clause on Fierce Guardianship / Deflecting Swat.
_COMMANDERS = frozenset({"Krark, the Thumbless", "Sakashima of a Thousand Faces"})
_FREE_WITH_COMMANDER = frozenset({"Fierce Guardianship", "Deflecting Swat"})


# --------------------------------------------------------------------------- #
# Mana pool
# --------------------------------------------------------------------------- #

@dataclass
class ManaPool:
    pool: Dict[str, int] = field(default_factory=dict)  # color symbol -> amount; "C" = colorless
    # PERSISTENT any-color mana: Treasure tokens (Strike It Rich, Storm-Kiln, Tavern, ...).
    # A Treasure is an artifact you sacrifice for one mana of any color, so it pays like a
    # wildcard '*' pip — but unlike the floating pool (which empties every turn) Treasures
    # stay on the battlefield, so the simulator carries them across turns. They are therefore
    # SPENT LAST (after every ephemeral pip) so unused ramp is banked, not wasted.
    treasures: int = 0

    def add(self, sym: str, n: int = 1) -> None:
        self.pool[sym] = self.pool.get(sym, 0) + n

    def add_cost(self, mana: Dict[str, int]) -> None:
        for k, v in mana.items():
            self.add(k, v)

    def total(self) -> int:
        return sum(self.pool.values()) + self.treasures

    def can_pay(self, cost: Dict[str, int]) -> bool:
        """Colored pips paid by their color or by wildcard '*'/Treasure (any-color);
        generic by anything. 'X' in a cost is ignored here (set X before paying)."""
        avail = dict(self.pool)
        wild = avail.pop("*", 0) + self.treasures      # Treasures are wildcard for affordability
        for sym, need in cost.items():
            if sym in ("generic", "X"):
                continue
            use = min(avail.get(sym, 0), need)
            avail[sym] = avail.get(sym, 0) - use
            need -= use
            if need > 0:
                if wild >= need:
                    wild -= need
                else:
                    return False
        generic = cost.get("generic", 0)
        return sum(avail.values()) + wild >= generic

    def pay(self, cost: Dict[str, int]) -> None:
        """Mutates the pool. Pays colored pips from their color then wildcard;
        generic from colorless/off-color first, wildcard last. Greedy generic
        payment is a known simplification — branch on it in the action layer when
        the choice matters downstream."""
        if not self.can_pay(cost):
            raise ValueError(f"Cannot pay {cost} from {self.pool} (+{self.treasures} treasure)")

        def spend_wild(n: int) -> None:
            """Pay n wildcard from the floating '*' pool FIRST, then crack Treasures. Treasures
            are spent last because they persist to later turns — keep the ramp banked."""
            use = min(self.pool.get("*", 0), n)
            self.pool["*"] = self.pool.get("*", 0) - use
            n -= use
            if n > 0:
                self.treasures -= n

        for sym, need in cost.items():
            if sym in ("generic", "X"):
                continue
            use = min(self.pool.get(sym, 0), need)
            self.pool[sym] = self.pool.get(sym, 0) - use
            need -= use
            if need > 0:
                spend_wild(need)
        # Generic is paid to preserve the most flexibility for LATER spells, since
        # the same engine keeps producing mana and a payoff is cast at the end of
        # the chain (Thoracle {U}{U}, Grapeshot {R}+generic). Spend, in order:
        #   1. colorless "C" — it can never satisfy a colored pip, so dump it first;
        #   2. the most ABUNDANT color — a red-flooding Jeska's Will chain pays its
        #      own generic out of the red it just made, keeping scarce {U} alive for
        #      Thassa's Oracle (the bug this replaces burned {U} before {R});
        #   3. floating wildcard "*" — pays any pip, so keep it over plain colors;
        #   4. Treasures LAST — they bank across turns, the most valuable to keep.
        generic = cost.get("generic", 0)
        while generic > 0 and self.pool.get("C", 0) > 0:
            self.pool["C"] -= 1
            generic -= 1
        while generic > 0:
            colored = {k: v for k, v in self.pool.items() if k != "*" and v > 0}
            if colored:
                self.pool[max(colored, key=colored.get)] -= 1   # dump the flooded color
            elif self.pool.get("*", 0) > 0:
                self.pool["*"] -= 1                              # floating wildcard next
            elif self.treasures > 0:
                self.treasures -= 1                             # crack a Treasure only last
            else:
                break                                           # can_pay() guaranteed affordability
            generic -= 1
        self.pool = {k: v for k, v in self.pool.items() if v > 0}

    def key(self) -> Tuple:
        return tuple(sorted(self.pool.items())) + (("T", self.treasures),) if self.treasures else \
            tuple(sorted(self.pool.items()))


# --------------------------------------------------------------------------- #
# Battlefield permanents
# --------------------------------------------------------------------------- #

@dataclass
class Permanent:
    name: str                       # printed/base name
    copy_of: Optional[str] = None   # if a copy effect: the name it copies (e.g. "Krark, the Thumbless")
    tapped: bool = False
    summoning_sick: bool = True
    is_token: bool = False
    counters: Dict[str, int] = field(default_factory=dict)

    @property
    def cdef(self) -> CardDef:
        return _functions_as(self.name, None)

    @property
    def effective_name(self) -> str:
        """What the permanent functionally *is*. A Sakashima copying Krark keeps
        the Sakashima name (legend-safe) but functions as Krark."""
        return self.copy_of or self.name

    @property
    def functions_as(self) -> CardDef:
        return _functions_as(self.name, self.copy_of)

    @property
    def is_krark_body(self) -> bool:
        f = self.functions_as
        return f.is_krark_body  # Krark, or anything currently copying Krark

    @property
    def is_trigger_doubler(self) -> bool:
        return self.functions_as.is_trigger_doubler

    @property
    def blue_pips(self) -> int:
        # A copy's mana cost becomes the copied card's cost (CR 707.2). So a
        # Sakashima copying Krark contributes Krark's pips (0 blue), NOT {U}{U}.
        # This feeds Thoracle gating, so it must use functions_as, not printed.
        return self.functions_as.blue_pips

    def key(self) -> Tuple:
        return (self.name, self.copy_of, self.tapped, self.summoning_sick,
                self.is_token, tuple(sorted(self.counters.items())))

    def _copy(self) -> "Permanent":
        p = Permanent.__new__(Permanent)
        p.name = self.name
        p.copy_of = self.copy_of
        p.tapped = self.tapped
        p.summoning_sick = self.summoning_sick
        p.is_token = self.is_token
        # counters is never written in this model (no effect populates it), so the common
        # case is empty — only pay for a dict copy if it's actually non-empty.
        p.counters = dict(self.counters) if self.counters else {}
        return p


# --------------------------------------------------------------------------- #
# Stack objects (spells, triggered abilities, and Krark flip triggers)
# --------------------------------------------------------------------------- #

@dataclass
class StackObject:
    kind: str                       # "spell" | "ability" | "krark_flip" | "copy"
    name: str                       # card name or ability label
    controller_choices: Dict = field(default_factory=dict)  # targets, X, modes
    # For krark_flip: how many flips this single trigger represents is implicit
    # (one StackObject == one flip trigger). The number of flip StackObjects put
    # on the stack per cast == flips_per_cast at cast time.
    source_cast_name: Optional[str] = None  # the spell whose cast spawned this flip

    def key(self) -> Tuple:
        return (self.kind, self.name, self.source_cast_name,
                tuple(sorted((k, str(v)) for k, v in self.controller_choices.items())))

    def _copy(self) -> "StackObject":
        s = StackObject.__new__(StackObject)
        s.kind = self.kind
        s.name = self.name
        s.controller_choices = dict(self.controller_choices)
        s.source_cast_name = self.source_cast_name
        return s


# --------------------------------------------------------------------------- #
# The state
# --------------------------------------------------------------------------- #

@dataclass
class GameState:
    # zones
    library: List[str] = field(default_factory=list)        # ordered, top = index 0
    hand: List[str] = field(default_factory=list)
    battlefield: List[Permanent] = field(default_factory=list)
    graveyard: List[str] = field(default_factory=list)
    exile: List[str] = field(default_factory=list)
    # Cards exiled face-up that you MAY PLAY this turn (Jeska's Will mode 2). Distinct from
    # `exile` (a void): these are a turn-scoped castable zone; unplayed ones are lost at EOT.
    exiled_play: List[str] = field(default_factory=list)
    stack: List[StackObject] = field(default_factory=list)  # top = last element

    # resources / trackers
    mana: ManaPool = field(default_factory=ManaPool)
    storm_count: int = 0
    turn: int = 1
    is_my_turn: bool = True

    # opponents are abstracted to aggregate numbers (solitaire model). LIFE is a SINGLE pool:
    # a 3-opponent table is 3x40 = 120, but a burn/Grapeshot win requires 160 total damage as a
    # robustness margin (lifegain, imperfect bolt assignment, an extra player's worth of buffer).
    # One pool means every point counts toward the kill (no free-assignment overkill waste).
    opponent_life: Tuple[int, ...] = (160,)
    opponent_library: Tuple[int, ...] = (99, 99, 99)
    # Jeska's Will mode 1 adds {R} per card in the SINGLE LARGEST opp hand. In the goldfish
    # we don't track real opponents, so default to 5 (a realistic mid-game hand) -> Jeska's
    # Will gives 5 R per resolution on cast. (With a commander out BOTH modes fire: 5 R AND
    # exile-3-and-play; see resolver._jeskas_will.)
    opponent_hand: Tuple[int, ...] = (5, 5, 5)

    # established infinities — set by the loop detector, consumed by win predicate.
    # Strings like {"hasty_attackers", "storm", "mana_R", "draw", "mill"}.
    infinite: frozenset = frozenset()

    # populated by the engine when a terminal event fires (read by the planner)
    game_result: Optional[str] = None  # None | "WIN:<type>" | "LOSS:<reason>"

    # ----------------------------------------------------------------- #
    # Flip engine — straight from the engine spec, §1–§2
    # ----------------------------------------------------------------- #

    @property
    def krark_bodies(self) -> int:
        return sum(1 for p in self.battlefield if p.is_krark_body)

    @property
    def trigger_doublers(self) -> int:
        # Each Veyran and each Harmonic counts once (additive, can exceed 2).
        return sum(1 for p in self.battlefield if p.is_trigger_doubler)

    @property
    def flips_per_cast(self) -> int:
        """bodies × (1 + doublers).  2 bodies + 2 doublers = 6.  1 body + 2 = 3.
        This is exact for Krark's flip: Krark is a Wizard (Harmonic applies) and
        the flip is caused by casting an I/S (Veyran applies), so both doublers
        always count toward flips."""
        return self.krark_bodies * (1 + self.trigger_doublers)

    def _count_functioning(self, name: str) -> int:
        return sum(1 for p in self.battlefield if p.effective_name == name)

    def value_multiplier(self, engine: CardDef, cast_is_instant_or_sorcery: bool = True) -> int:
        """How many times `engine`'s value trigger fires per qualifying event,
        accounting additively for Veyran and Harmonic — but only where each
        legally applies:

          * Veyran doubles a trigger only if it is CAUSED by casting/copying an
            instant or sorcery. So: magecraft (is_cast_or_copy) and I/S-cast
            triggers (is_cast) always qualify; a 'spell_cast' trigger (Birgi)
            qualifies only when the cast spell is itself an I/S; a coin-flip-win
            trigger (Tavern) never qualifies.
          * Harmonic doubles a trigger only if the engine is a Shaman or Wizard,
            regardless of cause.

        Each doubler is additive and applies once per event (the once-per-event
        replacement rule), matching the flip model."""
        m = 1
        cause = engine.trigger_cause
        veyran_applies = (
            cause in ("is_cast_or_copy", "is_cast")
            or (cause == "spell_cast" and cast_is_instant_or_sorcery)
        )
        if veyran_applies:
            m += self._count_functioning("Veyran, Voice of Duality")
        if engine.is_shaman_or_wizard:
            m += self._count_functioning("Harmonic Prodigy")
        return m

    def value_engines(self):
        """(Permanent, CardDef) for every permanent on the battlefield that has a
        value/mana trigger the resolver must fire."""
        out = []
        for p in self.battlefield:
            f = p.functions_as
            if f.has_value_trigger:
                out.append((p, f))
        return out

    @property
    def has_krarks_thumb(self) -> bool:
        return any(p.effective_name == "Krark's Thumb" for p in self.battlefield)

    @property
    def flip_p(self) -> float:
        """Single-flip win probability. Thumb -> flip two keep one -> 0.75."""
        return 0.75 if self.has_krarks_thumb else 0.50

    @property
    def defense_grid(self) -> bool:
        return any(p.effective_name == "Defense Grid" for p in self.battlefield)

    def cast_cost(self, card_name: str) -> Dict[str, int]:
        """Cost to cast `card_name` from hand right now, including Defense Grid's
        +{3} generic on the pilot's OWN spells during opponents' turns (pure mana
        math, per the spec — not a counterspell hedge)."""
        # Fierce Guardianship / Deflecting Swat: "you may cast this without paying its
        # mana cost" while you control a commander (always, here) — makes them free,
        # repeatable counterspell engines (see cards.FREE_COUNTERS). Pact is already {0}.
        if card_name in _FREE_WITH_COMMANDER and self.controls_commander:
            cost: Dict[str, int] = {}
        else:
            cost = dict(cards.get(card_name).cost)
        # is_my_turn is the cheap bool; check it FIRST so we skip the battlefield scan in
        # `defense_grid` on our own turns (the solitaire is always our turn, so this short-
        # circuits ~every cast_cost call — cast_cost is one of the hottest paths in the search).
        if not self.is_my_turn and self.defense_grid:
            cost["generic"] = cost.get("generic", 0) + 3
        return cost

    @property
    def controls_commander(self) -> bool:
        """Whether a commander (Krark or Sakashima) is on the battlefield — gates the
        'cast without paying its mana cost' clause on Fierce Guardianship / Deflecting
        Swat. A Sakashima copying Krark still has Sakashima's printed name, so a name
        check catches both bodies."""
        return any(p.name in _COMMANDERS for p in self.battlefield)

    @property
    def blue_devotion(self) -> int:
        return sum(p.blue_pips for p in self.battlefield)

    def has_permanent(self, effective_name: str) -> bool:
        return any(p.effective_name == effective_name for p in self.battlefield)

    def count_in_zone(self, zone: List[str], name: str) -> int:
        return sum(1 for c in zone if c == name)

    # ----------------------------------------------------------------- #
    # Cloning + canonical hashing (for search memoization)
    # ----------------------------------------------------------------- #

    def clone(self) -> "GameState":
        # Hand-rolled deep copy — clone() runs on every rollout step, so deepcopy's
        # reflection was ~half of all sim time (profiled). Zones are lists of immutable
        # strings (shallow list copy suffices); only Permanent.counters, StackObject
        # .controller_choices, and the mana pool are mutable dicts that need their own copy.
        new = GameState.__new__(GameState)
        new.library = self.library[:]
        new.hand = self.hand[:]
        # COPY-ON-WRITE: share Permanent objects (shallow list copy). Permanents are mutated
        # only by tapping (planner.tap_out / _apply_mana_ability / _deploy_engine_perms), and
        # those sites copy the one permanent they touch first (see planner._cow). This skips
        # the per-permanent _copy that was the #1 hotspot — clone() runs on every rollout step.
        new.battlefield = self.battlefield[:]
        new.graveyard = self.graveyard[:]
        new.exile = self.exile[:]
        new.exiled_play = self.exiled_play[:]
        new.stack = [s._copy() for s in self.stack]
        new.mana = ManaPool(dict(self.mana.pool), treasures=self.mana.treasures)
        new.storm_count = self.storm_count
        new.turn = self.turn
        new.is_my_turn = self.is_my_turn
        new.opponent_life = self.opponent_life            # tuple, immutable
        new.opponent_library = self.opponent_library      # tuple, immutable
        new.opponent_hand = self.opponent_hand            # tuple, immutable
        new.infinite = self.infinite                      # frozenset, immutable
        new.game_result = self.game_result
        return new

    def canonical_key(self) -> Tuple:
        """Order-insensitive where order is irrelevant; order-preserving where it
        matters (library, stack). Two states that are genuinely equivalent for
        future play hash equal.

        Caveat: targets on the stack are referenced by name/choice text, not by a
        permanent's object id, so two structurally identical boards don't desync
        the key. If you add effects that distinguish two otherwise-identical
        permanents by identity, revisit this."""
        return (
            tuple(self.library),                                   # ordered
            tuple(sorted(self.hand)),                              # multiset
            tuple(sorted(p.key() for p in self.battlefield)),      # multiset of states
            tuple(sorted(self.graveyard)),
            tuple(sorted(self.exile)),
            tuple(sorted(self.exiled_play)),
            tuple(s.key() for s in self.stack),                    # ordered (LIFO)
            self.mana.key(),
            self.storm_count,
            self.turn,
            self.is_my_turn,
            self.opponent_life,
            self.opponent_library,
            self.opponent_hand,
            self.infinite,
        )

    def __hash__(self) -> int:
        return hash(self.canonical_key())

    # ----------------------------------------------------------------- #
    # NN observation bridge (coarse, lossy — for guidance only)
    # ----------------------------------------------------------------- #

    def encode_observation(self) -> Dict[str, object]:
        """Fixed-shape, lossy summary for an AlphaZero-style policy/value net.
        Deliberately separate from canonical_key(): the net gets a smooth coarse
        view; the solver gets exact identity. Returned as a Dict space; flatten to
        a tensor at the trainer boundary.

        IMPORTANT: this is for *guiding* search (pruning / move priors), never for
        deciding a kill. Kills come from exact simulation + the win predicate."""
        from cards import Tag

        def tag_count(zone: List[str], tag: Tag) -> int:
            return sum(1 for n in zone if tag in cards.get(n).tags)

        hand_by_tag = {t.value: tag_count(self.hand, t) for t in Tag}

        return {
            # mana (the three that matter for this deck + generic capacity)
            "mana_U": self.mana.pool.get("U", 0),
            "mana_R": self.mana.pool.get("R", 0),
            "mana_generic_capacity": self.mana.total(),
            # engine counts (exact, cheap, high-signal)
            "krark_bodies": self.krark_bodies,
            "trigger_doublers": self.trigger_doublers,
            "flips_per_cast": self.flips_per_cast,
            "thumb": int(self.has_krarks_thumb),
            "flip_p_x100": int(self.flip_p * 100),
            "mana_engines": sum(1 for p in self.battlefield
                                if p.functions_as.mana_per_trigger or p.functions_as.treasure_per_trigger
                                or p.functions_as.treasure_per_flip_win),
            "draw_engines": sum(1 for p in self.battlefield if p.functions_as.draw_per_trigger),
            # hand as multi-hot-by-category (lossy on purpose)
            "hand": hand_by_tag,
            "hand_size": len(self.hand),
            # graveyard escape targets (presence flags)
            "gy_breach": int("Underworld Breach" in self.graveyard),
            "gy_brain_freeze": int("Brain Freeze" in self.graveyard),
            "gy_size": len(self.graveyard),
            # trackers
            "storm_count": self.storm_count,
            "turn": self.turn,
            "library_size": len(self.library),
            "blue_devotion": self.blue_devotion,
            "infinite": sorted(self.infinite),
        }


# --------------------------------------------------------------------------- #
# convenience builder for tests / dry runs
# --------------------------------------------------------------------------- #

def make_state(**kw) -> GameState:
    return GameState(**kw)


def krark_body(name: str = "Krark, the Thumbless",
               copy_of: Optional[str] = None,
               token: bool = False) -> Permanent:
    """Helper: a Krark on the battlefield. To add a *body* the legend-safe way,
    pass a Sakashima/clone permanent copying Krark, e.g.
        krark_body("Sakashima of a Thousand Faces", copy_of="Krark, the Thumbless")
        krark_body("Glasspool Mimic", copy_of="Sakashima of a Thousand Faces", token=True)
    """
    return Permanent(name=name, copy_of=copy_of, is_token=token, summoning_sick=False)
