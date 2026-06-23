"""
sim.py — Solitaire game simulation for the Krark/Sakashima deck.

Runs a complete game from opening hand, simulating each turn (draw, land,
cast permanents) until planner.solve() detects a winning line.  Every turn
is printed verbosely so the output can be verified by hand.

    python sim.py               # random game
    python sim.py --seed 42     # reproducible shuffle
    python sim.py --max-turns 20
"""
from __future__ import annotations

import argparse
import contextlib
import multiprocessing
import os
import random
import statistics
import time
from collections import Counter
from typing import Optional

import cards
from cards import CardType
from game_state import GameState, Permanent, ManaPool
from planner import (solve, MANA_SOURCES, DeterministicKillSearch, ProbabilisticPlanner,
                     tap_out, _is_engine_permanent, _deploy_engine_perms)
from resolver import apply_etb, ETB_TUTORS, discard_rank
from seed import build_deck
import loops
import wishlist

# Mana rocks count toward the opening hand's "mana sources" alongside lands.
_MANA_ROCKS = {"Sol Ring", "Arcane Signet", "Chrome Mox", "Mox Diamond",
               "Lotus Petal", "Springleaf Drum",
               "Mana Vault", "Mox Amber", "Relic of Legends"}   # added 2026-06-21
# Something to DO with the hand (commanders are always available from the command
# zone, so a doubler / engine / gas / combo piece is enough to keep).
_ACTION = wishlist._ENGINE | {"Twinflame", "Dualcaster Mage", "Heat Shimmer"}
# Clones that enter as a copy of a creature — useless without a Krark body to mirror.
_CLONES = {"Sakashima of a Thousand Faces", "Glasspool Mimic", "Phantasmal Image"}

# Mana produced per activation — keys and amounts extracted from the planner's
# authoritative MANA_SOURCES table so the two never diverge.
_MANA: dict[str, dict[str, int]] = {
    name: produced for name, (_, produced) in MANA_SOURCES.items()
}

LANDS = {
    "Island", "Mountain", "Great Furnace", "Seat of the Synod",
    "Otawara, Soaring City", "Ancient Tomb", "Command Tower",
    "Shivan Reef", "Sulfur Falls",
    "Volcanic Island", "Mana Confluence",            # added 2026-06-21
}

# Lotus Petal is sacrificed (not tapped) the moment it's played; keep it off
# the board so the solver doesn't see a "used" copy as still available.
_SAC_ON_PLAY = {"Lotus Petal"}

# Mox Diamond's entry cost: discard a land card or it goes to the graveyard. So it costs a
# card (a land), and you can't play it with no land in hand — pure card disadvantage to do so.
_DISCARD_LAND_ON_PLAY = {"Mox Diamond"}

# Permanents a pilot plays out as early as affordable.
# Cards absent from this dict stay in hand as combo/interaction pieces.
_PLAY_PRIORITY: dict[str, int] = {
    "Chrome Mox":                    0,
    "Mox Amber":                     0,   # 0-cost rock, on once a legend (commander) is out
    "Mox Diamond":                   1,
    "Sol Ring":                      1,
    "Mana Vault":                    1,   # {1} for {C}{C}{C} — explosive ramp
    "Arcane Signet":                 2,
    "Springleaf Drum":               2,
    "Relic of Legends":              3,   # {3} any-color rock
    "Mystic Remora":                 2,
    "Krark's Thumb":                 3,
    "Tavern Scoundrel":              3,   # 2 treasures/flip-win — infinite-mana enabler
    "Baral, Chief of Compliance":    3,
    "Ragavan, Nimble Pilferer":      3,
    "Krark, the Thumbless":          3,
    "Birgi, God of Storytelling":    4,
    "Harmonic Prodigy":              4,
    "Okaun, Eye of Chaos":           4,
    "Sakashima of a Thousand Faces": 4,
    "Zndrsplt, Eye of Wisdom":       4,
    "Rhystic Study":                 4,
    "Glasspool Mimic":               5,
    "Phantasmal Image":              5,
    "Vivi Ornitier":                 5,
    "Archmage Emeritus":             5,
    "Storm-Kiln Artist":             5,
    "Veyran, Voice of Duality":      5,
    "Urabrask":                      5,
    "Imperial Recruiter":            5,
    # NOTE: Snapcaster Mage is cast for its ETB FLASHBACK (recast an instant/sorcery from the
    # graveyard — e.g. a Brain Freeze / ritual / Grapeshot in the go-off turn), almost never
    # for the 2/1 body. The flashback isn't modeled yet, so don't durdle it out as a vanilla
    # body; the pilot deploys it as part of a combo turn. (TODO: model the ETB flashback.)
    "Spellseeker":                   5,
    "The One Ring":                  5,   # escalating draw engine; cast when affordable
    # NOTE: Subtlety is a flash interaction piece (evoke a free counter/bounce on a cast
    # creature/PW), almost never hard-cast as a body — the pilot casts it by hand as
    # interaction, so the sim does NOT auto-hard-cast it for {2}{U}{U} (it has no solitaire
    # target anyway; see cards._NO_SOLITAIRE_TARGET).
    "Valley Floodcaller":            6,
    "Gale, Waterdeep Prodigy":       6,
}
_PLAY_SET = set(_PLAY_PRIORITY)

# Spells worth casting BEFORE permanents to RAMP the turn: with a Krark body out, Krark
# copies multiply Jeska's Will into a pile of red (mode 1 adds {R} per card in an opp's
# hand, every copy). That mana funds the rest of the turn's permanents/develop — and since
# mana empties at end of turn, making it early to spend it is the whole point. Holding the
# card does nothing, so the pilot fires it the moment a body is online (~turn 5-6).
_RAMP_SPELLS = ("Jeska's Will",)
_DEV_PAYOFFS = ("Grapeshot", "Thassa's Oracle", "Brain Freeze")

# Ramp-gate tuning knob (env KRARK_RAMP). Controls when _ramp fires Jeska's Will:
#   off     - never ramp (develop-phase only, the old behaviour)
#   uncond  - ramp whenever a body is out and it's net-positive (most aggressive)
#   stuck   - ramp only to unlock a permanent we can't currently afford
#   stuck2  - like stuck but require >=2 bodies (more conservative, bigger ramp)
#   sink    - ramp when there's ANY mana sink this turn (stuck permanent OR a develop spell)
# Default 'sink': tuned over 200-game sweeps it ties uncond for fastest kill (median
# turn 11 vs 12 for stuck/off) at the same 82% win rate, while its one guard avoids
# firing Jeska's Will into a dead board. See _ramp_tune.py.
def _ramp_mode() -> str:
    return os.environ.get("KRARK_RAMP", "sink")

# Testing knob: comma-separated card names the pilot should NOT deploy (held in hand). Lets us
# A/B a "what if we never play X" question — e.g. KRARK_SKIP="Tavern Scoundrel".
def _skip_deploy() -> frozenset:
    return frozenset(c.strip() for c in os.environ.get("KRARK_SKIP", "").split(",") if c.strip())

# Slow card-advantage-OVER-TIME engines (no help to THIS turn's cantrip chain). When
# KRARK_DEFER_SLOW is on, these are deferred past the develop/dig phase so the free/mana-
# neutral cantrips (Frantic Search untaps its own lands; net mana + storm with a couple
# Krarks out) get FULL mana to dig, and the slow engine is cast with whatever's left.
_DEFER_SLOW = frozenset({"The One Ring", "Rhystic Study", "Mystic Remora"})
def _defer_slow() -> bool:
    return os.environ.get("KRARK_DEFER_SLOW", "0") == "1"

# Mulligan aggressiveness (env KRARK_MULL_AGGR, an int >= 0). 0 (default) = the legacy policy:
# keep any hand that passes _keepable (sensible land + an action). When >0 it sets the QUALITY
# bar for the FIRST keep decision: ship a merely-playable opener and dig for a loaded one, then
# settle for anything playable on later draws. _hand_quality counts how many engine "pillars"
# the hand has (fast mana / doubler / payoff / dig / value engine / tutor / combo piece), so
# e.g. KRARK_MULL_AGGR=3 means "only keep the opening 7 if it has >=3 of those pillars."
_PILLAR_FAST_MANA = frozenset({
    "Sol Ring", "Ancient Tomb", "Mana Vault", "Mox Amber", "Mox Diamond", "Chrome Mox",
    "Lotus Petal", "Lion's Eye Diamond", "Rite of Flame", "Pyretic Ritual", "Desperate Ritual",
    "Strike It Rich", "Jeska's Will", "Mana Confluence", "Springleaf Drum", "Arcane Signet",
    "Relic of Legends"})
_PILLAR_DOUBLER = frozenset({"Harmonic Prodigy", "Veyran, Voice of Duality", "Krark's Thumb"})
_PILLAR_DIG = frozenset({
    "Brainstorm", "Ponder", "Peek", "Gitaxian Probe", "Frantic Search", "Borne Upon a Wind",
    "Gale, Waterdeep Prodigy", "Archmage Emeritus", "The One Ring",
    "Opt", "Consider", "Serum Visions", "Preordain"})
_PILLAR_VALUE = frozenset({
    "Storm-Kiln Artist", "Archmage Emeritus", "Birgi, God of Storytelling", "Urabrask",
    "Vivi Ornitier", "Tavern Scoundrel"})
_PILLAR_TUTOR = frozenset({
    "Imperial Recruiter", "Spellseeker", "Gamble", "Wishclaw Talisman", "Okaun, Eye of Chaos"})
_PILLAR_COMBO = frozenset({"Twinflame", "Heat Shimmer", "Dualcaster Mage"})


def _mull_aggr() -> int:
    try:
        return max(0, int(os.environ.get("KRARK_MULL_AGGR", "0")))
    except ValueError:
        return 0

# Sweeps default to evaluating each seed over THIS many coin-flip samples (same deck, varied
# flips), so reported win rates/turns reflect the flip distribution, not one sample per seed.
_DEFAULT_SWEEP_TRIALS = 8

# Per-turn win SCAN uses a coarse Monte-Carlo: the sim only needs to know whether
# a winning line exists (p_win > 0), not its precise probability, and a full
# 1500-sim estimate every turn makes a 20-turn game take minutes (each sim
# deep-copies the whole library).  When a win IS found we re-solve that one board
# at full fidelity for an accurate reported number.
_SCAN_DET = DeterministicKillSearch()                 # default budget; DFS is cheap post-prune
# Coarse: just detect whether a winning line exists (p_win > 0). The rollout policy is
# heavier per sim than the old single-engine estimate, so keep the scan budget small and
# few first-moves; the win turn is re-solved at full fidelity for the reported number.
_SCAN_PROB = ProbabilisticPlanner(mc_sims=80, max_first=2, rollout_steps=20)


class SimGame:
    def __init__(self, rng_seed: Optional[int] = None, win_threshold: float = 0.95):
        cards.load()
        self.win_threshold = win_threshold      # min p_win to "go off" on a risky line
        # Separate RNG for the develop phase's real coin flips, so a seed reproduces
        # the whole game (shuffle AND development), not just the shuffle.
        self.dev_rng = random.Random((rng_seed if rng_seed is not None else 0) * 7919 + 1)
        self._goff_base = None                   # board the winning go-off was declared on
        rng = random.Random(rng_seed)
        deck = build_deck()
        self.hand, self.library = self._london_mulligan(deck, rng)
        # Each board entry is (card_name, copy_of) where copy_of is determined
        # at cast time so clone relationships don't drift as the game evolves.
        self.board: list[tuple[str, Optional[str]]] = []
        self.tapped: set[int] = set()           # indices of tapped permanents
        self.turn: int = 0
        # Partner commanders live in the command zone, NOT the deck. They are the
        # combo's foundation (Krark = the flip body, Sakashima = a second, legend-
        # safe Krark body) so the pilot casts them as soon as mana allows.
        self.command_zone: list[str] = [
            "Krark, the Thumbless",
            "Sakashima of a Thousand Faces",
        ]
        self.cmd_tax: dict[str, int] = {
            "Krark, the Thumbless": 0,
            "Sakashima of a Thousand Faces": 0,
        }
        self.one_ring: int = 0          # The One Ring's burden counters (draws/turn)
        self.treasures: int = 0         # banked Treasure tokens — PERSIST across turns (any-color ramp)
        # Opponent life PERSISTS across turns (it's damage, not mana): Urabrask's 1-per-cast
        # burn, Gut Shot, etc. chip the table down over many turns toward a burn kill. Floating
        # mana empties each turn, but life totals carry — so we track them on the game. Modeled
        # as ONE pool requiring 160 total damage to win (see GameState.opponent_life).
        self.opponent_life: tuple = (160,)
        self.graveyard: list[str] = []  # persistent yard (develop discards/mills, Breach fuel)
        # Jeska's Will mode-2: nonland cards exiled face-up and merged into hand as castable
        # gas for THIS turn; the Counter tracks how many to discard at EOT if left unplayed.
        self._exile_gas: Counter = Counter()
        self._dev_flip_lines: list[str] = []   # per-turn coin-flip reports from _develop

    # ── London mulligan ───────────────────────────────────────────────────────

    @staticmethod
    def _lands(hand) -> int:
        return sum(1 for c in hand if CardType.LAND in cards.get(c).types)

    def _keepable(self, hand) -> bool:
        """Keep on a REASONABLE land count first (not too few / too many), then on
        having something to do (an engine piece or combo/payoff). Commanders are always
        available from the command zone, so the hand just needs mana + an action."""
        lands = self._lands(hand)
        rocks = sum(1 for c in hand if c in _MANA_ROCKS)
        mana = lands + rocks
        if lands < 1 or lands > 4:          # primary: a sensible amount of land
            return False
        if not (2 <= mana <= 5):            # not mana-screwed, not all sources
            return False
        return any(c in _ACTION or c in wishlist._PAYOFFS for c in hand)

    @staticmethod
    def _hand_quality(hand) -> int:
        """How LOADED an opener is: the number of distinct engine 'pillars' present —
        fast mana, a flip doubler, a payoff, a dig/draw spell, a per-cast value engine,
        a tutor, and a combo piece. Higher = a faster, more self-sufficient hand. Used as
        the mulligan-aggressiveness bar (KRARK_MULL_AGGR)."""
        hs = set(hand)
        pillars = 0
        pillars += any(c in _PILLAR_FAST_MANA for c in hs)
        pillars += any(c in _PILLAR_DOUBLER for c in hs)
        pillars += any(c in wishlist._PAYOFFS for c in hs)
        pillars += any(c in _PILLAR_DIG for c in hs)
        pillars += any(c in _PILLAR_VALUE for c in hs)
        pillars += any(c in _PILLAR_TUTOR for c in hs)
        pillars += any(c in _PILLAR_COMBO for c in hs)
        return pillars

    def _choose_bottom(self, hand, m: int) -> list[str]:
        """Pick the m weakest cards to bottom (London). Protect the first 3 MANA SOURCES
        (lands + rocks) above every spell so we never bottom into mana screw; surplus sources
        (4th+) go first, then the lowest-wishlist-value spells.

        The old version protected only lands and only at priority 5.0 — but premium engine
        pieces score well above that (wishlist.card_value), so a hand with a couple of strong
        spells would ship a NEEDED land (e.g. seed 7 bottomed one of its two Mountains, leaving
        1 land + a rock and bricking on mana for ~9 turns). Protecting sources at a priority no
        spell can outrank fixes that."""
        if m <= 0:
            return []
        snap = GameState(hand=list(hand))   # empty board: wishlist ranks payoffs/pieces high
        KEEP_SOURCES = 3
        prio, src_seen = [], 0
        for c in hand:
            is_source = CardType.LAND in cards.get(c).types or c in _MANA_ROCKS
            if is_source:
                src_seen += 1
                # Protected sources outrank all spells; surplus sources (4th+) ship first.
                prio.append(1e6 if src_seen <= KEEP_SOURCES else -1.0)
            else:
                prio.append(wishlist.card_value(snap, c) + 1.0)  # spells: between surplus and kept sources
        order = sorted(range(len(hand)), key=lambda i: prio[i])
        return [hand[i] for i in order[:m]]

    def _london_mulligan(self, deck, rng):
        """Draw 7, keep if reasonable, else mulligan (reshuffle, redraw 7) up to a
        5-card hand; on keep, bottom one card per mulligan taken."""
        self.mulligans, self.bottomed = 0, []
        A = _mull_aggr()
        for mulls in range(3):              # keep at 0, 1, or (forced) 2 mulligans
            rng.shuffle(deck)
            hand, lib = list(deck[:7]), list(deck[7:])
            # Aggressive FIRST keep: when KRARK_MULL_AGGR is set, the opening 7 must clear the
            # quality bar too — ship a merely-playable hand to dig for a loaded one. After that
            # (mulls>=1) settle for anything _keepable; mulls==2 is the forced floor.
            if mulls == 0 and A > 0:
                keep = self._keepable(hand) and self._hand_quality(hand) >= A
            else:
                keep = (mulls == 2) or self._keepable(hand)
            if keep:
                bottom = self._choose_bottom(hand, mulls)
                for c in bottom:
                    hand.remove(c)
                    lib.append(c)
                self.mulligans, self.bottomed = mulls, bottom
                return hand, lib

    # ── zone helpers ──────────────────────────────────────────────────────────

    def draw(self) -> Optional[str]:
        if not self.library:
            return None
        card = self.library.pop(0)
        self.hand.append(card)
        return card

    def _board_names(self) -> list[str]:
        return [n for n, _ in self.board]

    # ── mana ──────────────────────────────────────────────────────────────────

    def _untapped_sources(self) -> list[tuple[int, str]]:
        return [(i, n) for i, (n, _) in enumerate(self.board)
                if i not in self.tapped and n in _MANA]

    def _tap_source(self, idx: int, pool: ManaPool) -> None:
        name = self.board[idx][0]
        for color, amt in _MANA.get(name, {}).items():
            pool.add(color, amt)
        self.tapped.add(idx)

    def _try_afford(self, cost: dict, pool: ManaPool) -> bool:
        """Check (without mutating) whether cost is payable from pool + untapped sources."""
        temp = ManaPool(dict(pool.pool), treasures=pool.treasures)
        if temp.can_pay(cost):
            return True
        for idx, _ in self._untapped_sources():
            for color, amt in _MANA[self.board[idx][0]].items():
                temp.add(color, amt)
            if temp.can_pay(cost):
                return True
        return False

    def _pay(self, cost: dict, pool: ManaPool) -> None:
        """Pay cost, tapping sources lazily until the pool covers it."""
        while not pool.can_pay(cost):
            srcs = self._untapped_sources()
            if not srcs:
                raise RuntimeError(f"Tried to pay {cost} but ran out of mana")
            self._tap_source(srcs[0][0], pool)
        pool.pay(cost)

    # ── casting ───────────────────────────────────────────────────────────────

    def _cast_cost(self, card: str) -> dict:
        cost = dict(cards.get(card).cost)
        if card in self.cmd_tax:
            cost["generic"] = cost.get("generic", 0) + 2 * self.cmd_tax[card]
        if "Baral, Chief of Compliance" in self._board_names():
            if cards.get(card).is_instant_or_sorcery:
                cost["generic"] = max(0, cost.get("generic", 0) - 1)
        return cost

    def _copy_target(self, card: str) -> Optional[str]:
        """What a clone enters copying, at cast time (checks the current board). Clones are
        only worth casting as an extra Krark flip body. The LEGEND RULE matters here:
        Glasspool Mimic / Phantasmal Image copying Krark would make a second 'Krark, the
        Thumbless' and you'd have to bin one — UNLESS a Sakashima of a Thousand Faces is on
        the battlefield, whose ability turns off the legend rule for all your permanents.
        Sakashima itself is inherently legend-safe. So a non-Sakashima clone may copy Krark
        only while a Sakashima permanent is out; before that it's held."""
        if card not in _CLONES:
            return None
        krark_out = any(n == "Krark, the Thumbless" or cp == "Krark, the Thumbless"
                        for n, cp in self.board)
        if not krark_out:
            return None                      # no Krark to mirror -> a clone is a dead vanilla body
        if card == "Sakashima of a Thousand Faces":
            return "Krark, the Thumbless"    # inherently legend-safe (its own ability)
        sakashima_out = any(n == "Sakashima of a Thousand Faces" for n, _ in self.board)
        return "Krark, the Thumbless" if sakashima_out else None   # legend rule needs Sakashima

    def _try_cast(self, card: str, pool: ManaPool, from_command: bool = False) -> bool:
        # Don't cast a clone (Sakashima / Glasspool / Phantasmal) with no Krark body to
        # copy — it would enter as a vanilla creature. Hold it until there's a body.
        if card in _CLONES and self._copy_target(card) is None:
            return False
        cost = self._cast_cost(card)
        if not self._try_afford(cost, pool):
            return False
        # Mox Diamond's additional cost: discard a land card (else it's binned). With no land
        # to pitch, don't play it — that's pure card disadvantage a pilot would never take.
        discard_land = None
        if card in _DISCARD_LAND_ON_PLAY:
            discard_land = next((c for c in self.hand if c != card and c in LANDS), None)
            if discard_land is None:
                return False
        self._pay(cost, pool)
        if discard_land is not None:
            self.hand.remove(discard_land)
            self.graveyard.append(discard_land)
            print(f"  DISCARD  : {discard_land}  (Mox Diamond cost)")
        if from_command:
            self.command_zone.remove(card)
            # +{2} generic for each future cast of this commander from the zone
            self.cmd_tax[card] += 1
        else:
            self.hand.remove(card)
            if card in self.cmd_tax:
                self.cmd_tax[card] += 1

        if card in _SAC_ON_PLAY:
            # Sacrifice for mana immediately; never hits the battlefield
            for color, amt in _MANA.get(card, {}).items():
                pool.add(color, amt)
            print(f"  CAST+SAC : {card}  [{_fmt_cost(cost)} -> pool {_fmt_pool(pool)}]")
        else:
            cp = self._copy_target(card)
            new_idx = len(self.board)
            self.board.append((card, cp))
            # Tap new mana sources immediately so they fund further casts this turn
            if card in _MANA:
                self._tap_source(new_idx, pool)
            src = " (from command zone)" if from_command else ""
            label = f" (copying {cp})" if cp else ""
            print(f"  CAST     : {card}{src}{label}  [{_fmt_cost(cost)} -> pool {_fmt_pool(pool)}]")
            if cp is None and card in ETB_TUTORS:        # Spellseeker / Imperial Recruiter
                self._etb_tutor(card)
        return True

    def _etb_tutor(self, card: str) -> None:
        """Resolve an ETB tutor (Spellseeker / Imperial Recruiter) against the real
        zones: fetch the best wishlist card matching its filter into hand."""
        state = self._build_state(ManaPool())
        fetched = apply_etb(state, card)
        if fetched:
            self.library = list(state.library)
            self.hand = list(state.hand)
            print(f"  TUTOR    : {card} fetches {fetched}")

    # ── state builder for the solver ──────────────────────────────────────────

    def _build_state(self, pool: ManaPool) -> GameState:
        bf = [
            Permanent(name=name, copy_of=copy_of, summoning_sick=False,
                      tapped=(idx in self.tapped))
            for idx, (name, copy_of) in enumerate(self.board)
        ]
        return GameState(
            library=list(self.library),
            hand=list(self.hand),
            graveyard=list(self.graveyard),      # carry the real yard (Breach fuel, milled cards)
            battlefield=bf,
            mana=ManaPool(dict(pool.pool), treasures=pool.treasures),
            storm_count=0,
            is_my_turn=True,
            opponent_life=tuple(self.opponent_life),   # carry persistent burn damage across turns
        )

    def _play_land(self, card: str, pool: ManaPool, source: str = "hand") -> None:
        """Put a land onto the battlefield and tap it for mana, consuming the land drop.
        `source`='hand' removes it from hand; 'exile' is a Jeska's-Will exiled land."""
        if source == "hand":
            self.hand.remove(card)
        new_idx = len(self.board)
        self.board.append((card, None))
        self._tap_source(new_idx, pool)
        self._played_land = True
        tag = "" if source == "hand" else "  (from Jeska exile)"
        print(f"  LAND     : {card}  [pool {_fmt_pool(pool)}]{tag}")

    def _cast_permanents(self, pool: ManaPool, skip: frozenset = frozenset()) -> bool:
        """Cast permanents from hand + command zone, cheapest-priority-first, looping
        until nothing more is affordable. Re-gathers after every cast so a piece that
        just became affordable (or was just drawn) is reconsidered. `skip` holds names to
        defer this pass (e.g. slow value engines held back so develop digs on full mana).
        Returns whether anything was cast."""
        skip = skip | _skip_deploy()                     # honour the KRARK_SKIP testing knob
        def _candidates() -> list[tuple[str, bool]]:
            cands = [(c, False) for c in self.hand if c in _PLAY_SET and c not in skip]
            cands += [(c, True) for c in self.command_zone if c in _PLAY_SET and c not in skip]
            return sorted(cands, key=lambda t: _PLAY_PRIORITY[t[0]])

        cast_any, progress = False, True
        while progress:
            progress = False
            for card, from_command in _candidates():
                if self._try_cast(card, pool, from_command=from_command):
                    cast_any = progress = True
                    break  # re-gather: priorities/affordability may have shifted
        return cast_any

    def _has_stuck_permanent(self, pool: ManaPool) -> bool:
        """A castable permanent in hand or command zone we can't afford right now — i.e.
        ramp mana would let us deploy it THIS turn. Clones with no Krark body to copy
        aren't castable yet, so they don't count as stuck."""
        skip = _skip_deploy()
        for c in self.hand:
            if c in _PLAY_SET and c not in skip and not (c in _CLONES and self._copy_target(c) is None) \
               and not self._try_afford(self._cast_cost(c), pool):
                return True
        for c in self.command_zone:
            if c in _PLAY_SET and c not in skip and not self._try_afford(self._cast_cost(c), pool):
                return True
        return False

    def _ramp(self, pool: ManaPool) -> list[str]:
        """Cast mana-positive rituals (Jeska's Will) BEFORE permanents so their mana funds
        the turn. Resolves with real Krark flips; the produced mana lands back in `pool` for
        _cast_permanents to spend. Only fires with a Krark body out — that's when the copies
        make it explosive — and loops while it stays affordable (a lost flip returns Jeska's
        Will to hand, so it can chain into even more mana)."""
        mode = _ramp_mode()
        if mode == "off":
            return []
        min_bodies = 2 if mode == "stuck2" else 1
        cast: list[str] = []
        for _ in range(6):
            ramp = next((c for c in self.hand if c in _RAMP_SPELLS), None)
            if ramp is None:
                break
            probe = self._build_state(pool)
            # need a body out (copies = the "ton of mana"); skip if not net-positive
            if probe.flips_per_cast < min_bodies or loops.develop_score(probe, ramp) <= 0:
                break
            if not self._try_afford(probe.cast_cost(ramp), pool):
                break
            # Ramp only when the mana has a USE this turn (else it floats and empties at
            # cleanup, and we'd rather hold Jeska's Will as a combo/payoff piece). The gate
            # mode decides what counts as a use; once the ramp mana lands in `pool` the stuck
            # permanents read as affordable and the loop stops (ramps just enough).
            if mode in ("stuck", "stuck2") and not self._has_stuck_permanent(pool):
                break
            if mode == "sink" and not (self._has_stuck_permanent(pool)
                                       or loops.develop_candidates(probe)):
                break
            # commit: tap every source into the pool so the resolve can pay, then cast
            for idx, _n in self._untapped_sources():
                self._tap_source(idx, pool)
            state = self._build_state(pool)               # state.mana == full pool
            ns, log = loops._do_cast(state, ramp, "hand", self.dev_rng, _DEV_PAYOFFS,
                                     return_log=True)
            if ns is None:
                break
            self.hand = list(ns.hand)
            self.library = list(ns.library)
            self.graveyard = list(ns.graveyard)
            pool.pool = dict(ns.mana.pool)                # leftover + Jeska's produced mana
            pool.treasures = ns.mana.treasures            # any Treasures the ramp banked (persist)
            self.opponent_life = tuple(ns.opponent_life)  # carry any burn dealt this cast
            cast.append(ramp)
            print(f"  RAMP     : cast {ramp}  [-> pool {_fmt_pool(pool)}]")
            # Krark coin-flip outcome + (for Jeska) the explicit mode-1 mana breakdown so
            # it's clear the red is max(single largest opp hand) x resolutions, not a sum.
            fl = _fmt_flips(ramp, log)
            if fl:
                extra = ""
                if ramp == "Jeska's Will":
                    mh = max(probe.opponent_hand) if probe.opponent_hand else 0
                    res = log.get("resolutions", 1)
                    extra = (f"  |  mode1: {mh} (largest opp hand) x {res} res = {mh * res}R")
                print(f"  FLIPS    : {fl}{extra}")
            # Jeska mode 2: top 3-per-resolution exiled face-up, playable this turn. Show
            # them; merge the nonland gas into hand so the rest of the turn can play it
            # (unplayed gas is discarded at end of turn — see _cleanup_exile_gas).
            if ns.exiled_play:
                gas = list(ns.exiled_play)
                print(f"  EXILE    : {ramp} exiled (may play this turn): {', '.join(gas)}")
                for c in gas:
                    if c in LANDS:
                        # "You may play those cards" includes lands — but only one land
                        # drop per turn. Play it if the drop is unused; else it's lost.
                        if not self._played_land:
                            self._play_land(c, pool, source="exile")
                        continue
                    self.hand.append(c)       # nonland gas: castable the rest of this turn
                    self._exile_gas[c] += 1
        return cast

    # ── one turn ──────────────────────────────────────────────────────────────

    def _try_win(self, pool):
        """Look for a go-off on the CURRENT board/pool. Returns (win_line_or_None, line, state).
        Called at every point resources change — and crucially BEFORE every mana-spending phase
        — so a lethal payoff (e.g. Oracle once the dig emptied the library) is taken on full
        untapped mana instead of being stranded after we durdle the mana onto rocks."""
        state = self._build_state(pool)
        # The scan only needs the binary "does p_win clear the threshold" — early-stop is lossless.
        _SCAN_PROB.decision_threshold = self.win_threshold
        line = solve(state, deterministic=_SCAN_DET, probabilistic=_SCAN_PROB)
        return self._declare(line, state), line, state

    def _declare(self, line, state):
        """Decide whether a scanned go-off counts as a WIN this turn. A deterministic kill
        (p_win>=1.0) is guaranteed. A probabilistic go-off that clears the threshold must
        PROVE itself — replay the committed line once with the real game flips and only count
        it if it actually reaches lethal. Returns the full-fidelity winning Line, or None
        (gamble missed / below threshold) so the turn keeps developing."""
        if line.p_win >= 1.0:
            return line                              # guaranteed kill
        if line.kind == "probabilistic" and line.p_win >= self.win_threshold:
            if loops.prove_go_off(line.base, line.first, line.loop,
                                  self.dev_rng, _DEV_PAYOFFS):
                return solve(state)                  # proven -> accurate number for the report
            print(f"  FIZZLE   : go-off P={line.p_win:.2f} missed the real flips; develop on")
        return None

    def goff_flip_report(self, line):
        """Replay the declared winning go-off ONCE with the real game flips and return a
        formatted trace — every mana ability, permanent, and spell in the kill, in order,
        with the running mana pool (and banked Treasure) BEFORE -> AFTER each step and the
        Krark coin-flip outcome of each cast. So the win turn shows EXACTLY where the mana
        comes from and what the flips do. Reporting only: runs on a private clone and never
        mutates game state. Empty when there's nothing flip/mana-worthy to show."""
        sink = []
        start_mana = None
        if line is None:
            return []
        if getattr(line, "base", None) is not None and getattr(line, "first", None) is not None:
            # Probabilistic loop / develop go-off: prove_go_off replays it move-for-move.
            base = line.base.clone()
            start_mana = (dict(base.mana.pool), base.mana.treasures)
            try:
                loops.prove_go_off(line.base, line.first, line.loop, self.dev_rng,
                                   _DEV_PAYOFFS, flip_sink=sink)
            except Exception:
                pass
        elif self._goff_base is not None:
            # Deterministic kill: walk the committed action list with the REAL flips. Each
            # cast_perm/activate is luck-free; each I/S cast is sampled (and may make Krark
            # copies / storm). A spell already binned this chain is simply skipped.
            from planner import _apply_perm_cast, _apply_mana_ability
            s = self._goff_base.clone()
            start_mana = (dict(s.mana.pool), s.mana.treasures)

            def snap(st):
                return (dict(st.mana.pool), st.mana.treasures)

            for a in line.actions:
                try:
                    if a.kind == "activate":
                        idx = dict(a.choices).get("idx")
                        nm = (s.battlefield[idx].effective_name
                              if idx is not None and idx < len(s.battlefield) else (a.card or "source"))
                        before = snap(s)
                        s = _apply_mana_ability(s, a)
                        sink.append({"kind": "activate", "card": nm,
                                     "before": before, "after": snap(s)})
                    elif a.kind == "cast_perm":
                        before = snap(s); cost = dict(s.cast_cost(a.card))
                        s = _apply_perm_cast(s, a)
                        sink.append({"kind": "cast_perm", "card": a.card, "cost": cost,
                                     "before": before, "after": snap(s)})
                    elif a.kind == "cast":
                        src = loops._cast_source(s, a.card)
                        if src is None:
                            continue
                        before = snap(s); cost = dict(s.cast_cost(a.card))
                        ns, lg = loops._do_cast(s, a.card, src, self.dev_rng,
                                                _DEV_PAYOFFS, return_log=True)
                        if ns is None:
                            continue
                        s = ns
                        sink.append({"kind": "cast", "card": a.card, "cost": cost, "log": lg,
                                     "before": before, "after": snap(s)})
                except Exception:
                    continue

        if not sink:
            return []
        out = []
        if start_mana is not None:
            out.append(f"start: pool {_fmt_mana(start_mana)}")
        for e in sink:
            out.append(_fmt_goff_step(e))
        return out

    def play_turn(self):
        self.turn += 1
        self.tapped.clear()
        self._exile_gas = Counter()        # Jeska mode-2 gas is turn-scoped
        self._dev_flip_lines = []
        self._dev_exile_lines = []         # Jeska mode-2 EXILE reports from _develop
        self._played_land = False

        print(f"\n{'=' * 60}")
        print(f"  TURN {self.turn}")
        print(f"{'=' * 60}")

        # Draw
        if self.turn > 1:
            drawn = self.draw()
            print(f"  DRAW     : {drawn or '(empty library)'}")

        print(f"  HAND     : {', '.join(self.hand) or '(empty)'}")

        # Treasures banked on earlier turns are available again now (they sit on the battlefield
        # until sacrificed) — the floating pool empties each turn, the Treasure bank does not.
        pool = ManaPool(treasures=self.treasures)
        if self.treasures:
            print(f"  TREASURE : {self.treasures} banked (any-color, carried from earlier turns)")

        # Play first land in hand
        for card in list(self.hand):
            if card in LANDS:
                self._play_land(card, pool)
                break

        # Ramp first: with a Krark body out, fire Jeska's Will up front so its pile of red
        # funds this turn's permanents (it's wasted mana otherwise — it empties at cleanup).
        self._ramp(pool)

        # FREE per-turn draws BEFORE committing any mana, so a payoff they turn up is taken
        # on full untapped mana rather than after we've durdled it onto rocks.
        # The One Ring — escalating draw engine: add a burden counter, draw that many.
        if "The One Ring" in self._board_names():
            self.one_ring += 1
            drawn = []
            while len(drawn) < self.one_ring and len(self.library) > 6:
                drawn.append(self.draw())
            if drawn:
                print(f"  ONE RING : counter={self.one_ring}, draw {len(drawn)}: "
                      f"{', '.join(drawn)}")
        # Zndrsplt — beginning-of-combat: flip till you lose, draw a card per won flip.
        self._zndrsplt_combat_draw()

        # Check for a go-off NOW, before spending a drop of mana on permanents — take the kill
        # on full mana if it's already there (pilot: check often, never strand a lethal payoff).
        win, line, state = self._try_win(pool)
        if win is not None:
            self._goff_base = state
            return win

        # Cast permanents, cheapest-priority-first, looping until nothing more
        # affordable. Candidates come from the hand AND the command zone (the
        # commanders), each tagged with its source zone so casting pulls from the
        # right place. Slow value engines are deferred (when enabled) so develop digs first.
        first_skip = _DEFER_SLOW if _defer_slow() else frozenset()
        self._cast_permanents(pool, skip=first_skip)

        # Summary — lands kept in their own area (counted), engine permanents in BOARD.
        land_ct = Counter(n for n, _ in self.board if n in LANDS)
        lands_str = (f"{sum(land_ct.values())}  ("
                     + ", ".join(f"{k}x {n}" if k > 1 else n
                                 for n, k in sorted(land_ct.items())) + ")") \
            if land_ct else "0"
        board_str = ", ".join(
            f"{n}->{cp}" if cp else n for n, cp in self.board if n not in LANDS
        ) or "(empty)"
        hand_str = ", ".join(self.hand) or "(empty)"
        cmd_str = ", ".join(self.command_zone) or "(empty)"
        print(f"  COMMAND  : {cmd_str}")
        print(f"  LANDS    : {lands_str}")
        print(f"  BOARD    : {board_str}")
        print(f"  HAND     : {hand_str}")
        print(f"  GY       : {len(self.graveyard)} cards")
        print(f"  POOL     : {_fmt_pool(pool) or '{}'}")
        if sum(self.opponent_life) < 160:                 # show the table's life once burn starts
            print(f"  OPP LIFE : {sum(self.opponent_life)}/160 remaining  "
                  f"({160 - sum(self.opponent_life)} dmg dealt, persists across turns)")

        # Win detection. A guaranteed kill always counts; a probabilistic line must clear the
        # pilot's RISK THRESHOLD and PROVE itself (see _declare). Re-check after the deploy.
        win, line, state = self._try_win(pool)
        if win is not None:
            self._goff_base = state
            return win

        # No go-off this turn -> DEVELOP for future turns: actually cast value
        # instants/sorceries (cantrips dig the real library for payoffs/pieces, rituals
        # fire magecraft, spells fall into the yard as Breach fuel). This makes
        # persistent progress so the go-off turn arrives sooner.
        developed = self._develop(pool)
        if developed:
            print(f"  DEVELOP  : cast {', '.join(developed)}")
            for fl in self._dev_flip_lines:
                print(f"  FLIPS    : {fl}")
            for el in self._dev_exile_lines:
                print(f"  EXILE    : {el}")
            print(f"  -> HAND  : {', '.join(self.hand) or '(empty)'}")
            print(f"  -> ZONES : library {len(self.library)}  graveyard {len(self.graveyard)}")

        # KEY: re-check the go-off RIGHT AFTER the dig, BEFORE deploying anything else. The dig
        # commonly mills/draws the library to <= devotion (or to 0) with the Oracle in hand —
        # that's lethal NOW, and _develop spent no real mana, so casting it wins this turn. The
        # old order deployed rocks first (Mox Diamond / Springleaf / ...) and stranded the kill
        # for a whole turn because the mana was gone by the time we checked.
        if developed:
            win, line, state = self._try_win(pool)
            if win is not None:
                self._goff_base = state
                print(f"  GO OFF   : dig assembled the kill -> go off")
                return win

        # Deploy leftover permanents on the mana the dig left — INCLUDING the slow value engines
        # deferred above. Snapcaster Mage: cast it for the ETB FLASHBACK (an extra graveyard
        # spell), not the body. Done after the dig so the yard has targets. Then RE-CHECK.
        flashed = self._snapcaster_flashback(pool)
        deployed_more = self._cast_permanents(pool)
        if flashed or deployed_more:
            win, line, state = self._try_win(pool)
            if win is not None:
                self._goff_base = state
                print(f"  REDEPLOY : engine assembled mid-dig -> go off")
                return win

        best = f"best P(win)={line.p_win:.3f} (< {self.win_threshold:.2f})" \
            if line.p_win > 0.0 else "no line"
        print(f"  CHECK    : {best}  "
              f"(bodies={state.krark_bodies}  doublers={state.trigger_doublers}  "
              f"flips/cast={state.flips_per_cast}  devotion={state.blue_devotion})")
        self._cleanup_exile_gas()
        # Floating mana empties at cleanup; banked Treasures stay on the battlefield -> carry over.
        self.treasures = pool.treasures
        self._discard_to_hand_size(pool)
        return None

    _MAX_HAND_SIZE = 7

    def _discard_to_hand_size(self, pool) -> None:
        """Enforce the 7-card maximum hand size at the cleanup step (a real rule the sim was
        skipping, so hands grew unbounded). Discards down to 7 using the SAME priority as the
        loot discard (resolver.discard_rank): redundant resources you already have go first,
        win-cons / recursion / combo pieces are protected to the last."""
        excess = len(self.hand) - self._MAX_HAND_SIZE
        if excess <= 0:
            return
        state = self._build_state(pool)
        drop = sorted(self.hand, key=lambda c: discard_rank(state, c))[:excess]
        for c in drop:
            self.hand.remove(c)
            self.graveyard.append(c)
        print(f"  EOT DISC : over hand size, discard {len(drop)}: {', '.join(drop)}")

    def _zndrsplt_combat_draw(self) -> None:
        """Zndrsplt, Eye of Wisdom's beginning-of-combat trigger: flip a coin until you lose,
        drawing a card per won flip (its 'whenever you win a coin flip, draw a card'). Fires
        every turn it's on the battlefield — a free, passive draw engine separate from the
        Krark-cast draws (those fire inside resolve_cast_sample on each won flip during a
        spell). Krark's Thumb lifts the per-flip win prob to 0.75 (so ~3 draws/turn). With
        N Zndrsplt copies each combat trigger flips AND each won flip draws once per copy.
        Sampled with dev_rng; capped to keep >=1 card so the passive engine never self-decks
        (the deck-into-Oracle win is found by solve())."""
        st = self._build_state(ManaPool())
        copies = sum(1 for p in st.battlefield
                     if p.effective_name == "Zndrsplt, Eye of Wisdom")
        if not copies:
            return
        p = st.flip_p
        wins = 0
        for _ in range(copies):                 # each copy's own flip-til-lose sequence
            while self.dev_rng.random() < p:
                wins += 1
        n = min(wins * copies, max(0, len(self.library) - 1))   # each win draws per copy
        drawn = [self.draw() for _ in range(n)]
        drawn = [c for c in drawn if c]
        if wins:
            tag = f" x{copies} copies" if copies > 1 else ""
            print(f"  ZNDRSPLT : combat flips won {wins}{tag} -> draw {len(drawn)}"
                  + (f": {', '.join(drawn)}" if drawn else " (library floor)"))

    def _snapcaster_flashback(self, pool: ManaPool) -> bool:
        """Snapcaster Mage's real use: cast it (Flash 2/1) for the ETB that gives an
        instant/sorcery in the graveyard flashback, then take that extra cast this turn —
        a ritual (mana), Brain Freeze (mill toward the Oracle), a cantrip (dig), etc., NOT
        the 2/1 body. The flashed-back card is EXILED afterwards (flashback's replacement
        effect), so a lost Krark flip exiles it instead of looping. Uses this turn's mana
        on a tapped-out clone (like _develop); writes back the dug zones + the body.
        Returns True if it fired."""
        if "Snapcaster Mage" not in self.hand:
            return False
        st = tap_out(self._build_state(pool))
        sc_cost = st.cast_cost("Snapcaster Mage")
        if not st.mana.can_pay(sc_cost):
            return False
        # Deploy Snapcaster on the clone so the flashback cast sees the body (devotion, etc.).
        st.mana.pay(sc_cost)
        st.hand.remove("Snapcaster Mage")
        st.battlefield.append(Permanent("Snapcaster Mage", summoning_sick=False))
        # Best flashback target: an I/S in the yard we can pay for that makes real progress.
        targets = [c for c in dict.fromkeys(st.graveyard)
                   if cards.get(c).is_instant_or_sorcery
                   and st.mana.can_pay(st.cast_cost(c))
                   and loops.develop_score(st, c) > 0]
        if not targets:
            return False                       # nothing worth flashing back — hold Snapcaster
        target = max(targets, key=lambda c: loops.develop_score(st, c))
        # Flashback = cast from the yard; model as a hand cast, then EXILE the card after.
        st.graveyard.remove(target)
        st.hand.append(target)
        ns, log = loops._do_cast(st, target, "hand", self.dev_rng, _DEV_PAYOFFS,
                                 return_log=True)
        if ns is None:
            return False
        for z in (ns.hand, ns.graveyard):      # flashback exiles instead of hand/GY
            if target in z:
                z.remove(target)
                break
        # Commit: Snapcaster body + dug zones (mana stays this-turn-only, like _develop).
        self.board.append(("Snapcaster Mage", None))
        self.hand = list(ns.hand)
        self.library = list(ns.library)
        self.graveyard = list(ns.graveyard)
        pool.treasures = ns.mana.treasures       # bank any Treasures the flashback made (persist)
        self.opponent_life = tuple(ns.opponent_life)   # carry any burn from the flashback cast
        print(f"  SNAPCAST : Snapcaster Mage -> flashback {target} (exiled after)")
        fl = _fmt_flips(target, log)
        if fl:
            print(f"  FLIPS    : {fl}")
        return True

    def _cleanup_exile_gas(self):
        """End of turn: Jeska's Will mode-2 cards you didn't play this turn stay exiled
        (lost), so drop any still sitting in hand. (Cards drawn off a gas cantrip are real
        library cards and are kept — only the exiled gas itself is purged.)"""
        if not self._exile_gas:
            return
        lost = []
        for name, cnt in self._exile_gas.items():
            for _ in range(cnt):
                if name in self.hand:
                    self.hand.remove(name)
                    lost.append(name)
        if lost:
            print(f"  EXILE    : unplayed Jeska gas lost at EOT: {', '.join(lost)}")
        self._exile_gas = Counter()

    # ── develop phase ───────────────────────────────────────────────────────────

    def _develop(self, pool: ManaPool) -> list[str]:
        """Play value instants/sorceries this turn to make PERSISTENT progress (draw
        the real library toward payoffs/pieces, fill the yard) when we can't go off yet.
        Uses this turn's mana (which empties anyway). Greedy on develop_score, stops
        before decking out. Writes the drawn/cast results back to the real zones."""
        payoffs = _DEV_PAYOFFS
        state = tap_out(self._build_state(pool))
        orig_bf = len(state.battlefield)        # everything past this index is deployed here
        need_life = sum(lp for lp in state.opponent_life if lp > 0)
        scores: dict[str, float] = {}

        def _recompute():
            # Develop value per spell. The board CAN change mid-phase now (we deploy the
            # engine pieces we dig into), so recompute whenever it does. A spell only
            # develops if it's net-positive (cantrip draws, ritual mana, or a free counter
            # that draws via magecraft); a free spell with no engine behind it scores ~0.
            scores.clear()
            for c in set(state.hand) | set(state.graveyard):
                if cards.get(c).is_instant_or_sorcery and c not in loops.PAYOFF_ONLY:
                    scores[c] = loops.develop_score(state, c)

        _recompute()
        cast: list[str] = []
        dead: set[str] = set()             # spells that make no persistent progress here
        for _ in range(8):
            # Deploy any engine permanents we've dug into (extra Krark bodies, doublers,
            # Storm-Kiln, ...) BEFORE the next dig cast, so a free-spell loop runs WITH the
            # engine — Storm-Kiln's treasures make it mana-positive instead of fizzling.
            if any(_is_engine_permanent(nm) for nm in state.hand):
                if _deploy_engine_perms(state):
                    _recompute()
            # Closing: a sustaining mana engine (Storm-Kiln / Birgi / Urabrask) + a Krark
            # body + an accessible Oracle means decking yourself INTO the Oracle is the win
            # (library 0 <= devotion), so dig freely. Otherwise stay above a safe floor.
            oracle_acc = ("Thassa's Oracle" in state.hand
                          or state.has_permanent("Thassa's Oracle")
                          or loops.can_escape(state, "Thassa's Oracle"))
            sustaining = state.krark_bodies >= 1 and any(
                state.has_permanent(n) for n in
                ("Storm-Kiln Artist", "Birgi, God of Storytelling", "Urabrask",
                 "Tavern Scoundrel"))   # 2 Treasures per won flip -> sustains the cast chain
            closing = oracle_acc and sustaining
            # Deck-out guard (only when NOT closing): casting can draw cards, and with a
            # draw-on-coin-flip engine (Zndrsplt) a SINGLE cast can win every flip and empty
            # the library — decking you out (a loss on your next draw). So gate each cast on
            # the WORST CASE draw (loops.max_draws, all flips won), not the expected draw:
            # you must not be able to deck yourself out by flipping coins you can't win.
            floor = max(8, state.blue_devotion + 4)
            if not closing and len(state.library) <= floor:
                break
            cands = [(c, s) for (c, s) in loops.develop_candidates(state)
                     if scores.get(c, 0) > 0 and c not in dead
                     and (closing or len(state.library) - loops.max_draws(state, c) >= floor)
                     # Jeska's Will mode-2 EXILES ~3-per-resolution library cards as "play it
                     # this turn or lose it" gas. Firing it on a develop-and-pass turn throws
                     # those cards away PERMANENTLY (it exiled our own Thassa's Oracle on seed
                     # 711). Only cast it mid-dig when we're CLOSING — going off this turn, so
                     # the exiled gas actually gets played. Otherwise leave it for _ramp (which
                     # runs first, at full mana, and merges the gas back) or a real go-off turn.
                     and (c != "Jeska's Will" or closing)]
            if not cands:
                break
            cands.sort(key=lambda cs: scores[cs[0]], reverse=True)
            def _sig(g):                                      # persistent-progress signature
                return (len(g.library), len(g.graveyard), sum(g.opponent_life))
            before = _sig(state)
            nxt = None
            nlog = None
            for card, source in cands:
                nxt, nlog = loops._do_cast(state, card, source, self.dev_rng, payoffs,
                                           return_log=True)
                if nxt is not None:
                    break
            if nxt is None:
                break                                         # mana ruin
            state = nxt
            cast.append(card)
            fl = _fmt_flips(card, nlog)                        # coin-flip outcome (Krark out)
            if fl:
                self._dev_flip_lines.append(fl)
            # Jeska's Will mode-2 cast mid-dig exiles top-3-per-resolution cards face-up,
            # playable THIS turn. Drain that zone like _ramp does: merge nonland gas into
            # hand so the continuing dig can cast/deploy it (engine pieces, more cantrips,
            # the Oracle), play one exiled land if the drop is free, and record the gas for
            # EOT cleanup. Without this the exiled cards (incl. our wincon) were SILENTLY
            # dropped on the floor — e.g. Jeska's Will exiling Thassa's Oracle = an unwinnable
            # game with no trace in the log.
            if state.exiled_play:
                gas = list(state.exiled_play)
                state.exiled_play.clear()
                self._dev_exile_lines.append(
                    f"{card} exiled (may play this turn): {', '.join(gas)}")
                for c in gas:
                    if c in LANDS:
                        if not self._played_land:
                            state.battlefield.append(Permanent(c, summoning_sick=False))
                            for color, amt in _MANA.get(c, {}).items():
                                state.mana.add(color, amt)
                            self._played_land = True
                        continue                              # extra land drops are lost
                    state.hand.append(c)        # nonland gas: castable the rest of this turn
                    self._exile_gas[c] += 1
                _recompute()                     # gas may have added new develop candidates
            if loops._winning_payoff(state, payoffs, need_life):
                break                                         # stumbled into lethal
            if _sig(state) == before:
                dead.add(card)   # pure floating mana, no persistent progress -> stop picking it
        # Write back the dug zones AND any engine permanents deployed this phase (append
        # only, so existing board indices / tapped-state stay valid).
        for p in state.battlefield[orig_bf:]:
            self.board.append((p.name, p.copy_of))
        self.hand = list(state.hand)
        self.library = list(state.library)
        self.graveyard = list(state.graveyard)
        pool.treasures = state.mana.treasures    # bank Treasures made/kept this dig (persist)
        self.opponent_life = tuple(state.opponent_life)   # carry Urabrask/etc. burn chipped this dig
        return cast


# ── formatting ────────────────────────────────────────────────────────────────

def _fmt_pool(pool: ManaPool) -> str:
    if not pool.pool:
        return ""
    return " ".join(f"{v}{k}" for k, v in sorted(pool.pool.items()) if v > 0)


def _fmt_flips(card: str, log: Optional[dict]) -> str:
    """One-line Krark coin-flip outcome for a noncreature spell. Empty string when no
    flips happened (no Krark body out), so callers can skip the line."""
    F = (log or {}).get("flips", 0)
    if not F:
        return ""
    wins = log.get("wins", 0)
    res = log.get("resolutions", 1)
    storm = log.get("storm_copies", 0)
    copies = max(res - 1, 0)
    detail = f"orig + {copies} Krark cop{'y' if copies == 1 else 'ies'}"
    if storm:
        detail += f" + {storm} storm"
    return (f"{card}: {wins}/{F} flip{'s' if F != 1 else ''} won "
            f"-> {res} resolution{'s' if res != 1 else ''} ({detail})")


def _fmt_mana(snap) -> str:
    """Format a (pool_dict, treasures) snapshot as e.g. '1C 1U +3T' (T = banked Treasure)."""
    pd, tr = snap
    body = " ".join(f"{v}{k}" for k, v in sorted(pd.items()) if v > 0) or "(empty)"
    return body + (f" +{tr}T" if tr else "")


def _fmt_goff_step(e: dict) -> str:
    """One trace line for a go-off step: the mana pool BEFORE -> AFTER, plus the flip
    outcome for a spell cast. Built from the dict entries goff_flip_report collects."""
    before = _fmt_mana(e["before"]); after = _fmt_mana(e["after"])
    if e.get("kind") == "activate":
        return f"  tap {e['card']:<26} pool {before:>10}  ->  {after}"
    cost = _fmt_cost(e.get("cost") or {})
    head = f"cast {e['card']} {cost}"
    flip = ""
    if e.get("log"):
        fl = _fmt_flips(e["card"], e["log"])
        if fl and ": " in fl:
            flip = "   | " + fl.split(": ", 1)[1]
    return f"  {head:<30} pool {before:>10}  ->  {after}{flip}"


def _fmt_cost(cost: dict) -> str:
    if not cost or not any(v for v in cost.values()):
        return "{0}"
    parts = []
    if cost.get("generic"):
        parts.append(str(cost["generic"]))
    for sym in ("U", "R", "G", "W", "B", "C", "*", "X"):
        parts.extend([sym] * cost.get(sym, 0))
    return "{" + "".join(parts) + "}"


# ── entry point ───────────────────────────────────────────────────────────────

def run_sim(rng_seed: Optional[int] = None, max_turns: int = 20,
            win_threshold: float = 0.95) -> None:
    game = SimGame(rng_seed=rng_seed, win_threshold=win_threshold)
    label = f"seed={rng_seed}" if rng_seed is not None else "random"
    print(f"{'=' * 60}")
    print(f"  GAME ({label})   go-off threshold p_win >= {win_threshold:.2f}")
    print(f"{'=' * 60}")
    kept = len(game.hand)
    mtag = f"  ({game.mulligans} mulligan{'s' if game.mulligans != 1 else ''} -> {kept}-card)" \
        if game.mulligans else "  (kept 7)"
    print(f"  OPENING   :{mtag} {', '.join(game.hand)}")
    if game.bottomed:
        print(f"  BOTTOMED  : {', '.join(game.bottomed)}")
    print(f"  LIBRARY   : {len(game.library)} cards")

    for _ in range(max_turns):
        line = game.play_turn()
        if line:
            print(f"\n{'=' * 60}")
            print(f"  WIN DETECTED — turn {game.turn}")
            print(f"{'=' * 60}")
            print(f"  {line}")
            if line.actions:
                for a in line.actions:
                    print(f"    - {a}")
            trace = game.goff_flip_report(line)
            if trace:
                print(f"  GO-OFF TRACE — mana pool + Krark flips, one real replay of the kill:")
                for t in trace:
                    print(f"    {t}")
            return

    print(f"\n  (no win found in {max_turns} turns)")


# ── parallel sweep ──────────────────────────────────────────────────────────────

def _play_quiet(seed: int, max_turns: int, win_threshold: float) -> dict:
    """Run one game with output suppressed; return a result summary."""
    with open(os.devnull, "w") as dn, contextlib.redirect_stdout(dn):
        game = SimGame(rng_seed=seed, win_threshold=win_threshold)
        line = None
        for _ in range(max_turns):
            line = game.play_turn()
            if line:
                break
    won = line is not None
    return {
        "seed": seed, "won": won,
        "turn": game.turn if won else None,
        "kind": line.kind if won else "",
        "p_win": line.p_win if won else 0.0,
        "detail": line.detail if won else "",
        "mulligans": game.mulligans,
    }


def _worker(args) -> dict:                 # module-level so it pickles for the Pool
    return _play_quiet(*args)


def _play_quiet_luck(args) -> dict:
    """One game on seed `seed`'s FIXED deck/opening but a specific coin-flip sample `luck`.
    The deck is built from random.Random(seed) inside __init__; we then override only the
    develop/flip RNG so repeated luck values explore the SAME game's coin-flip outcomes."""
    seed, luck, max_turns, win_threshold = args
    with open(os.devnull, "w") as dn, contextlib.redirect_stdout(dn):
        game = SimGame(rng_seed=seed, win_threshold=win_threshold)
        game.dev_rng = random.Random(seed * 1_000_003 + luck)
        line = None
        for _ in range(max_turns):
            line = game.play_turn()
            if line:
                break
    won = line is not None
    return {"seed": seed, "luck": luck, "won": won,
            "turn": game.turn if won else None, "mulligans": game.mulligans}


def run_flip_dist(n_games: int, trials: int, workers: int = 0, seed_base: int = 0,
                  max_turns: int = 20, win_threshold: float = 0.95) -> None:
    """Evaluate each seed over `trials` coin-flip samples (same deck/opening, different
    flips), so a seed's result is a DISTRIBUTION — P(win) and the win-turn spread — not a
    single lucky/unlucky trajectory. The overall win rate then integrates BOTH deck variance
    (across seeds) and flip variance (within a seed)."""
    workers = workers or (os.cpu_count() or 1)
    seeds = list(range(seed_base, seed_base + n_games))
    tasks = [(s, k, max_turns, win_threshold) for s in seeds for k in range(trials)]
    workers = max(1, min(workers, len(tasks)))

    print(f"{'=' * 70}")
    print(f"  FLIP-DISTRIBUTION SWEEP: {n_games} seeds x {trials} coin-flip trials "
          f"= {len(tasks)} games on {workers} workers")
    print(f"  (seeds {seed_base}-{seed_base + n_games - 1}, go-off p>={win_threshold:.2f})")
    print(f"{'=' * 70}")
    t0 = time.time()
    if workers == 1:
        results = [_play_quiet_luck(t) for t in tasks]
    else:
        with multiprocessing.Pool(workers) as pool:
            results = pool.map(_play_quiet_luck, tasks)
    elapsed = time.time() - t0

    by_seed: dict[int, list] = {s: [] for s in seeds}
    for r in results:
        by_seed[r["seed"]].append(r)

    per_seed_winprob, all_turns = [], []
    for s in seeds:
        rs = by_seed[s]
        wins = [r for r in rs if r["won"]]
        turns = sorted(r["turn"] for r in wins)
        wp = len(wins) / len(rs)
        per_seed_winprob.append(wp)
        all_turns += turns
        if turns:
            med = statistics.median(turns)
            spread = f"median {med:>4.1f}  (best {turns[0]}, worst {turns[-1]})"
        else:
            spread = "median   -- "
        print(f"  seed {s:<4} win {100 * wp:3.0f}% over {len(rs)} flips   {spread}")

    total_wins = len(all_turns)
    total = len(results)
    print("  " + "-" * 66)
    print(f"  mean per-seed P(win) {100 * statistics.mean(per_seed_winprob):.1f}%   "
          f"overall {total_wins}/{total} ({100 * total_wins / total:.0f}%) winning trials")
    if all_turns:
        print(f"  win-turn over all winning trials: mean {statistics.mean(all_turns):.2f}  "
              f"median {int(statistics.median(all_turns))}  "
              f"best {min(all_turns)}  worst {max(all_turns)}")
    print(f"  {elapsed:.1f}s total, {elapsed / total:.3f}s/game across {workers} cores")


def run_many(n_games: int, workers: int = 0, seed_base: int = 0,
             max_turns: int = 20, win_threshold: float = 0.95) -> None:
    """Run n_games in parallel across `workers` cores (0 = all cores) and report."""
    workers = workers or (os.cpu_count() or 1)
    workers = max(1, min(workers, n_games))
    seeds = range(seed_base, seed_base + n_games)
    tasks = [(s, max_turns, win_threshold) for s in seeds]

    print(f"{'=' * 64}")
    print(f"  PARALLEL SWEEP: {n_games} games on {workers} workers  "
          f"(seeds {seed_base}-{seed_base + n_games - 1}, go-off p>={win_threshold:.2f})")
    print(f"{'=' * 64}")
    t0 = time.time()
    if workers == 1:
        results = [_worker(t) for t in tasks]
    else:
        with multiprocessing.Pool(workers) as pool:
            results = pool.map(_worker, tasks)
    elapsed = time.time() - t0

    for r in sorted(results, key=lambda r: r["seed"]):
        if r["won"]:
            tag = "[KILL]" if r["kind"] == "deterministic" else f"p={r['p_win']:.2f}"
            print(f"  seed {r['seed']:<4} WIN turn {r['turn']:<3} {tag:<8} {r['detail'][:46]}")
        else:
            print(f"  seed {r['seed']:<4} no win in {max_turns}")

    wins = [r for r in results if r["won"]]
    print("  " + "-" * 62)
    if wins:
        turns = [r["turn"] for r in wins]
        kills = sum(1 for r in wins if r["kind"] == "deterministic")
        print(f"  wins {len(wins)}/{n_games} ({100 * len(wins) / n_games:.0f}%)   "
              f"avg turn {statistics.mean(turns):.1f}   median {int(statistics.median(turns))}   "
              f"deterministic kills {kills}")
    else:
        print(f"  wins 0/{n_games}")
    print(f"  mean mulligans {statistics.mean(r['mulligans'] for r in results):.2f}   |   "
          f"{elapsed:.1f}s total, {elapsed / n_games:.2f}s/game across {workers} cores")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(
        description="Simulate a Krark/Sakashima solitaire game to win detection."
    )
    parser.add_argument("--seed", type=int, default=None,
                        help="single-game seed (full per-turn log); also the sweep's base seed")
    parser.add_argument("--games", type=int, default=1,
                        help="number of games; >1 runs a parallel sweep with summary output")
    parser.add_argument("--workers", type=int, default=0,
                        help="cores for the sweep; 0 = all cores (default)")
    parser.add_argument("--max-turns", type=int, default=20)
    parser.add_argument("--win-threshold", type=float, default=0.95,
                        help="min p_win to declare a (risky) go-off win; 1.0 = only guaranteed")
    parser.add_argument("--flip-trials", type=int, default=None,
                        help="coin-flip samples PER seed (same deck, varied flips). Default: "
                             f"{_DEFAULT_SWEEP_TRIALS} for sweeps (--games>1) so results reflect "
                             "the flip distribution; 1 = a single verbose game. Pass 1 to force "
                             "an old-style single-sample sweep.")
    args = parser.parse_args()

    ft = args.flip_trials
    if args.games > 1:
        # Sweep: evaluate each seed over its coin-flip distribution by default.
        trials = ft if ft is not None else _DEFAULT_SWEEP_TRIALS
        if trials > 1:
            run_flip_dist(args.games, trials, workers=args.workers,
                          seed_base=(args.seed or 0), max_turns=args.max_turns,
                          win_threshold=args.win_threshold)
        else:
            run_many(args.games, workers=args.workers, seed_base=(args.seed or 0),
                     max_turns=args.max_turns, win_threshold=args.win_threshold)
    elif ft is not None and ft > 1:
        # Single seed, but asked for its flip distribution.
        run_flip_dist(1, ft, workers=args.workers, seed_base=(args.seed or 0),
                      max_turns=args.max_turns, win_threshold=args.win_threshold)
    else:
        # Single verbose game: default to a fresh RANDOM seed each run (and print it, so the
        # game stays reproducible). Passing None would leave the flip RNG fixed at seed 0.
        seed = args.seed if args.seed is not None else random.randint(0, 2**31 - 1)
        run_sim(rng_seed=seed, max_turns=args.max_turns, win_threshold=args.win_threshold)
