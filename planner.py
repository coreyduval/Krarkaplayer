"""
planner.py — OUTLINE of the search layer that strings casts into a kill.

This is the top of the stack. Everything below it is built and tested:
  cards (data) -> game_state (truth + encoder) -> resolver (chance nodes + EV)
              -> win (predicate) -> loops (detector + runaway)
The planner consumes all of it. It has THREE layers, cheapest first:

  0. solve() fast paths        — already implemented below. O(1)-ish checks:
       already won? confirmed loop assembled? mana/draw runaway with a payoff?
       These resolve the common cases without any search.

  1. DeterministicKillSearch   — DFS over game states for a line that wins with
       probability 1. Krark casts are collapsed to their WORST-CASE flip outcome
       (adversarial: assume every flip lost), so a node only counts as a kill if
       it wins regardless of luck — e.g. Twinflame+Dualcaster (needs no flips) or
       a Thoracle when the library is already empty. Returns "KILL + line" or None.
       STUBBED — algorithm in the docstring.

  2. ProbabilisticPlanner      — expectimax / MCTS over decision nodes (the pilot
       picks an action) and chance nodes (flip outcomes, weighted by the Binomial
       pmf from resolver.analyze_cast). Node value = P(win). For small flip counts
       enumerate outcomes exactly; for large counts sample. Returns the best action
       with P(win) + EV stats. STUBBED — algorithm in the docstring.

  3. PolicyValueNet (optional) — the AlphaZero hook. When raw search drowns in the
       branching factor (mana-payment choices × targets × stack order), a net
       trained on self-play consumes state.encode_observation() and returns move
       priors + a value estimate to guide MCTS. Search runs WITHOUT it (uniform
       priors + rollouts); the net only prunes/prioritizes. It NEVER certifies a
       kill — deterministic kills always come from layer 1's exact simulation.

Design invariants carried from the rest of the engine:
  * Solve only the pilot's side; assume lines resolve uninterrupted (spec §0).
  * An infinite loop is never a win without an accessible, gated payoff.
  * Never certify a kill that leans on a cards.VERIFY card.
  * Memoize on state.canonical_key(); the encoder is a SEPARATE, lossy view.
"""
from __future__ import annotations

from collections import Counter
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Protocol, Tuple

import cards
from cards import CardType
from game_state import GameState, Permanent
import win as winmod
import loops as loopmod
from resolver import analyze_cast, resolve_cast_sample, STORM_SPELLS, apply_etb


# --------------------------------------------------------------------------- #
# Actions
# --------------------------------------------------------------------------- #

@dataclass(frozen=True)
class Action:
    kind: str                       # "cast" | "activate" | "play_land" | "pass"
    card: Optional[str] = None
    choices: Tuple = ()             # frozen (key,value) pairs: targets, X, modes, mana plan

    def __str__(self):
        c = f" {dict(self.choices)}" if self.choices else ""
        return f"{self.kind}:{self.card or ''}{c}"


@dataclass
class Line:
    actions: List[Action] = field(default_factory=list)
    kind: str = ""                  # "deterministic" | "probabilistic"
    p_win: float = 0.0
    detail: str = ""
    # For a probabilistic line: enough to REPLAY the committed go-off once with the real
    # game flips (so a declared win can be PROVEN, not just estimated). base = the tapped/
    # deployed board the line was measured on; first = (card, source) it commits to; loop =
    # True if it's a focus-fire single-engine line (estimate_p_lethal) vs a develop rollout.
    base: object = None
    first: object = None
    loop: bool = False

    def __str__(self):
        head = "KILL" if (self.kind == "deterministic" and self.p_win >= 1.0) else \
               f"P(win)={self.p_win:.3f}"
        return f"[{head}] " + " ; ".join(str(a) for a in self.actions) + \
               (f"  ({self.detail})" if self.detail else "")


# --------------------------------------------------------------------------- #
# Action enumeration + application  (STUBS — signatures fixed, bodies to build)
# --------------------------------------------------------------------------- #

# Mana sources in the deck. mode: how it produces; produced: pool delta.
# Multi-color sources collapse to "*" (wildcard) to keep branching down — a
# wildcard dominates a fixed color for payability, so this loses no reachable line.
MANA_SOURCES = {
    "Sol Ring": ("tap", {"C": 2}),
    "Ancient Tomb": ("tap", {"C": 2}),
    "Great Furnace": ("tap", {"R": 1}),
    "Otawara, Soaring City": ("tap", {"U": 1}),
    "Seat of the Synod": ("tap", {"U": 1}),
    "Island": ("tap", {"U": 1}),
    "Mountain": ("tap", {"R": 1}),
    "Command Tower": ("tap", {"*": 1}),
    "Arcane Signet": ("tap", {"*": 1}),
    "Shivan Reef": ("tap", {"*": 1}),
    "Sulfur Falls": ("tap", {"*": 1}),
    "Mox Diamond": ("tap", {"*": 1}),
    "Chrome Mox": ("tap", {"*": 1}),
    "Springleaf Drum": ("tap_creature", {"*": 1}),
    "Lotus Petal": ("sac", {"*": 1}),
    "Lion's Eye Diamond": ("sac_hand", {"*": 3}),
    # --- added 2026-06-21 ---
    "Volcanic Island": ("tap", {"*": 1}),   # {U} or {R} -> wildcard (only U/R/C matter here)
    "Mana Confluence": ("tap", {"*": 1}),   # any color, pay 1 life (life cost not modeled)
    "Mana Vault": ("tap", {"C": 3}),        # {C}{C}{C}; colorless only (no-untap downside ignored)
    "Mox Amber": ("tap", {"*": 1}),         # any color among your legends; ~always on (a commander is out)
    "Relic of Legends": ("tap", {"*": 1}),  # {T}: any color (the creature-tap 2nd mana is not modeled)
}


def _cow(s: GameState, idx: int) -> Permanent:
    """Copy-on-write a battlefield permanent before mutating it. clone() shallow-shares
    Permanent objects across sibling states for speed, so any in-place mutation (tapping)
    must first swap in a private copy — else it would corrupt the states that share it."""
    p = s.battlefield[idx]._copy()
    s.battlefield[idx] = p
    return p


def tap_out(state: GameState) -> GameState:
    """Clone with every untapped land/rock tapped into the pool — models tapping out
    for a combo turn. Sac sources (Lotus Petal, Lion's Eye Diamond) are left in place:
    LED is the Breach mana engine and gets cracked by the chain itself."""
    s = state.clone()
    for i, p in enumerate(s.battlefield):
        src = MANA_SOURCES.get(p.effective_name)
        if not src:
            continue
        mode, produced = src
        if mode == "tap" and not p.tapped:
            _cow(s, i).tapped = True
            s.mana.add_cost(produced)
    return s


def _has_other_untapped_creature(state: GameState, exclude_idx: int) -> bool:
    return any(i != exclude_idx and CardType.CREATURE in p.functions_as.types and not p.tapped
               for i, p in enumerate(state.battlefield))


def _default_action_choices(state: GameState, name: str):
    """Frozen choice tuples for a cast. Brain Freeze gets two variants elsewhere."""
    return ()


def enumerate_actions(state: GameState) -> List[Action]:
    acts: List[Action] = []

    # mana abilities
    for i, p in enumerate(state.battlefield):
        nm = p.effective_name
        src = MANA_SOURCES.get(nm)
        if not src:
            continue
        mode, _ = src
        if mode in ("tap", "tap_creature") and p.tapped:
            continue
        if mode == "tap_creature" and not _has_other_untapped_creature(state, i):
            continue
        acts.append(Action("activate", nm, choices=(("idx", i),)))

    # casts from hand (dedup by name)
    has_creature = any(CardType.CREATURE in p.functions_as.types for p in state.battlefield)
    for nm in dict.fromkeys(state.hand):
        cdef = cards.get(nm)
        cost = state.cast_cost(nm)
        if not state.mana.can_pay(cost):
            continue
        if cdef.is_instant_or_sorcery:
            if not cards.castable_in_solitaire(nm, has_creature):
                continue                       # no legal target (counter, etc.)
            if nm == "Brain Freeze":
                acts.append(Action("cast", nm, choices=(("target", "self"),)))
                acts.append(Action("cast", nm, choices=(("target", "opponents"),)))
            else:
                acts.append(Action("cast", nm, choices=_default_action_choices(state, nm)))
        elif cdef.is_permanent and CardType.LAND not in cdef.types:
            acts.append(Action("cast_perm", nm))

    return acts


def _apply_mana_ability(s: GameState, action: Action) -> GameState:
    idx = dict(action.choices)["idx"]
    p = s.battlefield[idx]
    mode, produced = MANA_SOURCES[p.effective_name]
    if mode == "tap":
        _cow(s, idx).tapped = True
    elif mode == "tap_creature":
        _cow(s, idx).tapped = True
        for j, q in enumerate(s.battlefield):   # tap any other untapped creature as the cost
            if j != idx and CardType.CREATURE in q.functions_as.types and not q.tapped:
                _cow(s, j).tapped = True
                break
    elif mode == "sac":
        s.graveyard.append(s.battlefield.pop(idx).name)   # sacrificed -> graveyard
    elif mode == "sac_hand":
        s.graveyard.append(s.battlefield.pop(idx).name)   # Lion's Eye Diamond -> yard
        s.graveyard.extend(s.hand)
        s.hand = []
    s.mana.add_cost(produced)
    return s


def _apply_perm_cast(s: GameState, action: Action) -> GameState:
    s.hand.remove(action.card)
    s.mana.pay(s.cast_cost(action.card))
    perm = Permanent(action.card, summoning_sick=True)
    # the only copy choice that matters for the engine: Sakashima enters as Krark.
    if action.card == "Sakashima of a Thousand Faces" and \
       any(p.effective_name == "Krark, the Thumbless" for p in s.battlefield):
        perm.copy_of = "Krark, the Thumbless"
    s.battlefield.append(perm)
    apply_etb(s, action.card)              # Spellseeker / Imperial Recruiter fetch on ETB
    return s


# Permanents that BUILD the Krark engine: extra coin-flip bodies, flip/trigger
# doublers, and per-cast value engines (treasure / draw / mana / burn). When these are
# sitting in HAND on a go-off turn they must be deployed before the free-spell loop is
# evaluated — otherwise the loop (e.g. Gut Shot recast by Krark) is scored on an
# under-built board and undervalued, when with the engine ONLINE it just wins.
_ENGINE_PERM_FLAGS = ("is_krark_body", "clones_sakashima_safe", "is_trigger_doubler",
                      "draw_per_trigger", "treasure_per_trigger", "mana_per_trigger",
                      "damage_per_trigger", "treasure_per_flip_win")


def _is_engine_permanent(name: str) -> bool:
    cd = cards.get(name)
    if not cd.is_permanent or CardType.LAND in cd.types:
        return False
    eng = cards.ENGINE.get(name, {})
    return name == "Krark's Thumb" or any(eng.get(f) for f in _ENGINE_PERM_FLAGS)


def _deploy_engine_perms(s: GameState) -> List[str]:
    """Cast every affordable engine permanent from hand IN PLACE on `s` (which should
    already have its mana floated), ramping with any mana rocks in hand first so the
    engine pieces become affordable. Returns the names deployed, in order."""
    deployed: List[str] = []
    while True:
        cands = [nm for nm in dict.fromkeys(s.hand)
                 if (_is_engine_permanent(nm) or nm in MANA_SOURCES)
                 and CardType.LAND not in cards.get(nm).types]
        # mana rocks first (they ramp toward the engine pieces), then cheapest engine piece
        cands.sort(key=lambda nm: (nm not in MANA_SOURCES, sum(s.cast_cost(nm).values())))
        for nm in cands:
            if not s.mana.can_pay(s.cast_cost(nm)):
                continue
            _apply_perm_cast(s, Action("cast_perm", nm))
            deployed.append(nm)
            src = MANA_SOURCES.get(nm)            # a fresh rock can tap for mana right now
            if src and src[0] == "tap":
                for j, p in enumerate(s.battlefield):
                    if p.effective_name == nm and not p.tapped:
                        _cow(s, j).tapped = True
                        s.mana.add_cost(src[1])
                        break
            break
        else:
            break                                # nothing left to deploy
    return deployed


def _deploy_engine_base(state: GameState):
    """Tap out, then deploy all affordable engine permanents from hand, so the rollout
    evaluates the loop with the doublers / Storm-Kiln / extra bodies in play. Returns
    (base_state, deploy_actions); deploy_actions is empty if nothing could be cast."""
    s = tap_out(state)
    actions = [Action("cast_perm", nm) for nm in _deploy_engine_perms(s)]
    return s, actions


def apply_deterministic(state: GameState, action: Action) -> GameState:
    """The successor for a luck-free choice. For an instant/sorcery cast this uses
    the WORST case (forced 0 Krark wins) — so a deterministic kill must hold even
    if every flip is lost. Storm copies and the cast-event magecraft still fire,
    since those are luck-independent."""
    s = state.clone()
    if action.kind == "activate":
        return _apply_mana_ability(s, action)
    if action.kind == "cast_perm":
        return _apply_perm_cast(s, action)
    if action.kind == "cast":
        s.hand.remove(action.card)
        s.mana.pay(s.cast_cost(action.card))
        s2, _ = resolve_cast_sample(s, action.card, choices=dict(action.choices), forced_wins=0)
        return s2
    return s  # pass / unknown


def expand_chance(state: GameState, action: Action) -> List[Tuple[float, GameState]]:
    """For an I/S cast with F flips, the exact distribution over successors: one
    branch per number of won flips k=0..F, weighted by the Binomial pmf."""
    a = analyze_cast(state, action.card)
    F = a.flips
    out: List[Tuple[float, GameState]] = []
    for k in range(F + 1):
        prob = a.wins_pmf[k] if k < len(a.wins_pmf) else 0.0
        if prob <= 0:
            continue
        s = state.clone()
        s.hand.remove(action.card)
        s.mana.pay(s.cast_cost(action.card))
        s2, _ = resolve_cast_sample(s, action.card, choices=dict(action.choices), forced_wins=k)
        out.append((prob, s2))
    return out


# --------------------------------------------------------------------------- #
# Terminal evaluation — REAL (this is the glue, and it's cheap)
# --------------------------------------------------------------------------- #

def terminal_value(state: GameState) -> Optional[float]:
    """Return 1.0 for a winning terminal, 0.0 for a dead one (deck-out with no
    payoff), or None if the state is non-terminal. Runs the loop detector first so
    an assembled combo is recognized, then the win predicate, then loss check."""
    loopmod.apply_loops(state)                 # fold confirmed loops into state.infinite
    if winmod.evaluate_win(state).won:
        return 1.0
    if winmod.check_loss(state).won:           # WinResult(won=True, wtype="LOSS", ...)
        return 0.0
    return None


def _kill_detail(state: GameState, win_detail: str, line=None) -> str:
    """A self-consistent label for a deterministic kill. When the win is an assembled free
    loop, the loop's OWN reason (which states the pieces are in hand / on board WITH mana)
    is more accurate than the abstract win-predicate string — it stops the report reading as
    if attackers are already swinging when the combo is really sitting in hand, just dug into.
    Falls back to the win-predicate detail for non-loop kills (table already dead/decked,
    library already <= devotion).

    `line` (the committed action list) is used to keep the wording CONSISTENT with the casts
    shown: when the kill is reached by FOCUS-FIRING a dig spell (e.g. looping Brain Freeze,
    whose magecraft draws empty the deck into hand) that ASSEMBLES a combo the action list
    never actually casts, the report would otherwise describe a Twinflame/Dualcaster (or
    Oracle) finish out of nowhere. Prefix it with the dig so the text matches what's logged."""
    rep = loopmod.apply_loops(state.clone())
    reason = "; ".join(rep.reasons) if (rep.confirmed and rep.reasons) \
        else (win_detail or "guaranteed lethal")
    if line is not None and rep.confirmed and getattr(line, "actions", None):
        casts = [a.card for a in line.actions if a.kind == "cast"]
        if casts:
            dig, n = Counter(casts).most_common(1)[0]
            # combo/payoff pieces the REASON claims but the line never casts (they were dug
            # into hand, not hard-cast) — that's the gap that reads as inconsistent.
            assembled = [p for p in ("Twinflame", "Heat Shimmer", "Dualcaster Mage",
                                     "Thassa's Oracle", "Grapeshot")
                         if p in reason and p not in casts]
            if n >= 2 and dig not in reason and assembled:
                return (f"Loop {dig} x{n} (its magecraft draws empty the deck into hand) "
                        f"assembles the kill, then: {reason}")
    return reason


# --------------------------------------------------------------------------- #
# Layer 1: deterministic kill search  (STUB + algorithm)
# --------------------------------------------------------------------------- #

def _deterministic_useful(state: GameState, action: Action) -> bool:
    """Prune actions that cannot contribute to a PROBABILITY-1 line.

    A guaranteed kill must hold under every flip outcome. For a non-storm
    instant/sorcery cast with Krark out (flips_per_cast > 0) the two extremes
    diverge: winning all flips sends the spell to the graveyard (one resolution),
    losing all flips returns it to hand with zero Krark copies. No single such
    cast guarantees lethal, and — because each worst-case recast returns the spell
    to hand with a new storm/mana fingerprint — letting the DFS explore them
    explodes the search (every recast is a fresh, un-memoized node). Those lines
    are exactly what the PROBABILISTIC layer is for, so the deterministic search
    skips them. Storm spells are kept: their storm copies resolve regardless of
    any flip, so a single cast can be luck-free lethal."""
    if action.kind != "cast":
        return True
    cdef = cards.get(action.card)
    if cdef.is_instant_or_sorcery and action.card not in STORM_SPELLS \
            and state.flips_per_cast > 0:
        return False
    return True


class DeterministicKillSearch:
    """DFS for a probability-1 line.

    Algorithm:
      visited = set()                     # canonical_key()s seen, for memoization
      def dfs(state, line, depth):
          tv = terminal_value(state)
          if tv == 1.0: return line       # found a guaranteed kill
          if tv == 0.0 or depth == 0: return None
          key = state.canonical_key()
          if key in visited: return None
          visited.add(key)
          for action in enumerate_actions(state):
              if action.kind == "cast" and state.flips_per_cast > 0:
                  # adversarial flips: a deterministic kill must hold even if EVERY
                  # flip is lost. Take the worst-case successor (0 wins -> spell
                  # returns to hand, only cast-event value triggers fire).
                  succ = worst_case_successor(state, action)
              else:
                  succ = apply_deterministic(state, action)
              result = dfs(succ, line + [action], depth - 1)
              if result is not None: return result
          return None

    Notes:
      * worst_case_successor = resolve with wins forced to 0 (but still apply Storm
        copies and the cast-event magecraft, which are luck-independent).
      * Skip any action whose only path to lethal depends on a cards.VERIFY card.
      * Bound with a depth/time budget; the loop detector short-circuits the big
        combos so depth stays small in practice.
    """
    def __init__(self, max_depth: int = 12, node_budget: int = 30000):
        self.max_depth = max_depth
        self.node_budget = node_budget

    def find_kill(self, state: GameState) -> Optional[Line]:
        visited = set()
        budget = [self.node_budget]

        def dfs(s: GameState, line: List[Action], depth: int) -> Optional[Line]:
            if budget[0] <= 0:
                return None
            budget[0] -= 1
            tv = terminal_value(s)              # runs apply_loops + win + loss
            if tv == 1.0:
                w = winmod.evaluate_win(s)
                ln = Line(actions=line, kind="deterministic", p_win=1.0)
                ln.detail = _kill_detail(s, w.detail, ln)
                return ln
            if tv == 0.0 or depth == 0:
                return None
            key = s.canonical_key()
            if key in visited:
                return None
            visited.add(key)
            for action in enumerate_actions(s):
                if not _deterministic_useful(s, action):
                    continue
                try:
                    succ = apply_deterministic(s, action)
                except Exception:
                    continue
                got = dfs(succ, line + [action], depth - 1)
                if got is not None:
                    return got
            return None

        return dfs(state.clone(), [], self.max_depth)


# --------------------------------------------------------------------------- #
# Layer 2: probabilistic planner  (STUB + algorithm)
# --------------------------------------------------------------------------- #

class ProbabilisticPlanner:
    """Expectimax over decision + chance nodes; value = P(win).

    value(state, depth):
        tv = terminal_value(state)
        if tv is not None: return tv
        if depth == 0: return heuristic_value(state)   # e.g. estimate_p_lethal or 0
        best = 0.0
        for action in enumerate_actions(state):
            if action.kind == "cast" and state.flips_per_cast > 0:
                v = sum(p * value(succ, depth-1) for p, succ in expand_chance(state, action))
            else:
                v = value(apply_deterministic(state, action), depth-1)
            best = max(best, v)            # the pilot maximizes P(win)
        return best

    best_line(state) returns the argmax action plus value(state) and, from
    analyze_cast, the EV stats the spec wants (E[mana/storm/draws], P(return)).

    Scaling:
      * memoize value() on canonical_key();
      * for runaway engines (loops.analyze_runaway == MANA/DRAW_RUNAWAY) short-cut
        the subtree with loops.estimate_p_lethal instead of expanding casts;
      * when the branching factor (mana plans × targets × stack orders) explodes,
        switch from full expectimax to MCTS with progressive widening, optionally
        guided by a PolicyValueNet.
    """
    def __init__(self, max_depth: int = 8, mc_sims: int = 1500,
                 max_first: int = 3, rollout_steps: int = 40,
                 decision_threshold: Optional[float] = None):
        self.max_depth = max_depth
        self.mc_sims = mc_sims
        self.max_first = max_first          # candidate first moves scored per base
        self.rollout_steps = rollout_steps  # depth cap on a single policy rollout
        # When set (the per-turn SCAN), the MC estimators stop the moment `p_win >= this`
        # is locked and best_line short-circuits once a firing line is found — the scan only
        # needs the binary go-off decision (the win turn is re-solved at full fidelity).
        self.decision_threshold = decision_threshold

    def best_line(self, state: GameState,
                  payoffs=("Grapeshot", "Thassa's Oracle", "Brain Freeze")) -> Line:
        """One-ply expectimax with rollout values. For each candidate FIRST action
        (the develop spells castable now), measure P(win) by committing to it and then
        playing the develop policy forward (loops.rollout_estimate). Return the argmax.

        This is what prices 'develop now vs. hold': casting Brainstorm to dig / fill
        the yard / build the engine surfaces as the best move exactly when rolling it
        forward wins often enough — no hand-tuned threshold."""
        best = Line(kind="probabilistic", p_win=0.0, detail="no winning line found")
        thr = self.decision_threshold

        def _fired() -> bool:
            # Short-circuit: once a line clears the go-off threshold the scan is done — the
            # gate is binary and the win turn is re-solved at full fidelity anyway.
            return thr is not None and best.p_win >= thr

        def evaluate(base: GameState, prefix, label: str):
            nonlocal best
            cands = loopmod.develop_candidates(base)
            # rank by static develop value; only score the top few first moves
            cands.sort(key=lambda cs: loopmod.develop_score(base, cs[0]), reverse=True)
            for card, source in cands[:self.max_first]:
                if _fired():
                    return
                est = loopmod.rollout_estimate(base, (card, source), payoffs=payoffs,
                                               n_sims=self.mc_sims,
                                               max_steps=self.rollout_steps,
                                               decision_threshold=thr)
                if est["p_win"] > best.p_win:
                    winning = max(est["by_payoff"], key=est["by_payoff"].get)
                    best = Line(actions=prefix + [Action("cast", card)],
                                kind="probabilistic", p_win=est["p_win"],
                                detail=f"{label}{card} -> {winning} (P={est['p_win']:.3f})",
                                base=base, first=(card, source), loop=False)

        def eval_loops(base: GameState, prefix, label: str):
            """Focused single-engine recast loops (Brain Freeze storm-mill, ritual/Gut Shot
            + Urabrask burn, Jeska's Will runaway). The greedy develop rollout above DIVERSIFIES
            its casts (picks the best develop_score card each step), so it under-finds a kill that
            needs you to FOCUS-FIRE ONE card until it goes lethal. estimate_p_lethal models exactly
            that. This is what makes a loaded board (4 bodies + doublers + Storm-Kiln/Urabrask) read
            as a win even with the Oracle gone — there are many other lines."""
            nonlocal best
            if base.krark_bodies < 1:
                return                            # no flips -> no return-to-hand loop
            # A focused loop can only WIN through a payoff (Grapeshot/Brain Freeze/Oracle)
            # or per-cast burn (Urabrask). With none of those reachable the sims can only
            # return 0 — so skip the (expensive) estimate_p_lethal entirely. This is what
            # keeps a no-payoff board (e.g. a genuine brick) from running full loop-sims on
            # every turn for 20 turns (the slowdown that looked like a hang).
            payoff_here = any(pf in base.hand or pf in base.graveyard or base.has_permanent(pf)
                              for pf in payoffs)
            if not payoff_here and not loopmod._has_burn_engine(base):
                return
            # Engine cards to focus-fire: the payoffs themselves (Brain Freeze loops itself),
            # plus only the TOP FEW develop candidates by static value — looping every cantrip/
            # ritual is redundant (they drive the same storm/burn) and the per-card sims are the
            # cost. Keep this list short; the rollout above already covers diversified lines.
            engines = []
            for pf in payoffs:
                if pf in base.hand or loopmod.can_escape(base, pf):
                    engines.append(pf)
            devs = sorted((c for c, _ in loopmod.develop_candidates(base)),
                          key=lambda c: loopmod.develop_score(base, c), reverse=True)
            for c in devs[:2]:
                if c not in engines:
                    engines.append(c)
            n = max(60, self.mc_sims // 2)        # coarse gate; full solve re-confirms a hit
            for card in engines:
                if _fired():
                    return
                est = loopmod.estimate_p_lethal(base, card, payoffs=payoffs,
                                                n_sims=n, seed=0, decision_threshold=thr)
                if est["p_win"] > best.p_win:
                    bp = est["by_payoff"]
                    won = max(bp, key=bp.get) if bp else "loop"
                    src = "hand" if card in base.hand else "escape"
                    best = Line(actions=prefix + [Action("cast", card)],
                                kind="probabilistic", p_win=est["p_win"],
                                detail=f"{label}loop {card} -> {won} (P={est['p_win']:.3f})",
                                base=base, first=(card, src), loop=True)

        # develop straight off the current board — tap out first (it's the combo turn;
        # the pilot taps all lands/rocks to go off, same as the deterministic search).
        dev_base = tap_out(state)
        evaluate(dev_base, [], "develop ")
        if not _fired():
            eval_loops(dev_base, [], "develop ")

        # Underworld Breach combo turn. Breach is a go-off enchantment — you don't
        # durdle it out, you cast it as the turn opens, tap out, and escape spells from
        # the yard. So when it's on the battlefield OR castable from hand, build that
        # turn (tap out, deploy if needed) and run the same rollout on it. Gated to skip
        # the cost unless a payoff is reachable AND there's real fuel (one escape's worth,
        # or a battlefield LED that dumps the hand into the yard).
        breach_base, deployed = None, False
        if _fired():
            return best
        if state.has_permanent("Underworld Breach"):
            breach_base = tap_out(state)
        elif "Underworld Breach" in state.hand:
            s = tap_out(state)
            bcost = s.cast_cost("Underworld Breach")
            if s.mana.can_pay(bcost):
                s.mana.pay(bcost)
                s.hand.remove("Underworld Breach")
                s.battlefield.append(Permanent("Underworld Breach", summoning_sick=True))
                breach_base, deployed = s, True

        if breach_base is not None:
            payoff_here = any(pf in breach_base.hand or pf in breach_base.graveyard
                              or breach_base.has_permanent(pf) for pf in payoffs)
            enough_fuel = (loopmod.gy_fuel(breach_base) >= 4
                           or breach_base.has_permanent("Lion's Eye Diamond"))
            if payoff_here and enough_fuel:
                prefix = [Action("cast", "Underworld Breach")] if deployed else []
                lbl = "deploy Breach + " if deployed else "Breach: "
                evaluate(breach_base, prefix, lbl)
                eval_loops(breach_base, prefix, lbl)

        # Engine-deploy combo turn. If the flip/trigger doublers, extra Krark bodies, or
        # per-cast value engines are stranded in HAND, deploy them (tap out, ramp, play
        # them) and run the same rollout — so a free-spell loop is judged with the engine
        # online. This is what makes "Gut Shot + the engine in hand" read as a win.
        if not _fired() and any(_is_engine_permanent(nm) for nm in state.hand):
            eng_base, deploy_acts = _deploy_engine_base(state)
            if deploy_acts:
                evaluate(eng_base, deploy_acts, "deploy engine + ")
                if not _fired():
                    eval_loops(eng_base, deploy_acts, "deploy engine + ")

        return best


# --------------------------------------------------------------------------- #
# Layer 3: the AlphaZero hook  (interface only)
# --------------------------------------------------------------------------- #

class PolicyValueNet(Protocol):
    """Plug-in guidance for MCTS. Consumes the lossy observation, returns a prior
    over actions and a value estimate (~P(win)). Search works without it."""
    def evaluate(self, obs: Dict, legal: List[Action]) -> Tuple[Dict[Action, float], float]:
        ...


# --------------------------------------------------------------------------- #
# solve() — fast paths (REAL) then hand off to the search layers
# --------------------------------------------------------------------------- #

def solve(state: GameState,
          payoffs=("Grapeshot", "Thassa's Oracle", "Brain Freeze"),
          deterministic: Optional[DeterministicKillSearch] = None,
          probabilistic: Optional[ProbabilisticPlanner] = None) -> Line:
    """Resolve the common cases cheaply, then search.

    Order matters: a guaranteed kill beats any probabilistic line, so we look for
    deterministic wins first (already-won, confirmed loop, then DFS that can tap
    mana / cast pieces to assemble one), and only then quantify the best
    probabilistic (runaway) line."""
    s = state.clone()
    deterministic = deterministic or DeterministicKillSearch()
    probabilistic = probabilistic or ProbabilisticPlanner()

    # already terminal?
    if terminal_value(s) == 1.0:
        return Line(kind="deterministic", p_win=1.0,
                    detail=_kill_detail(s, winmod.evaluate_win(s).detail or "already lethal"))

    # confirmed free loop assembled right now?
    report = loopmod.apply_loops(s)
    if report.confirmed and winmod.evaluate_win(s).won:
        return Line(kind="deterministic", p_win=1.0,
                    detail=_kill_detail(s, winmod.evaluate_win(s).detail))

    # deterministic search: tap mana / cast pieces to reach a guaranteed kill
    kill = deterministic.find_kill(s)
    if kill is not None:
        return kill

    # else the best probabilistic (runaway) line
    return probabilistic.best_line(s, payoffs=payoffs)


# --------------------------------------------------------------------------- #
# smoke test of the fast paths
# --------------------------------------------------------------------------- #

if __name__ == "__main__":
    cards.load()
    from game_state import GameState, Permanent, krark_body

    def board(*p): return list(p)

    # 1) Twinflame + Dualcaster in hand with mana -> deterministic KILL via fast path
    s1 = GameState(library=["Island"] * 40, hand=["Twinflame", "Dualcaster Mage"],
                   battlefield=board(krark_body("Krark, the Thumbless")))
    s1.mana.add("R", 3); s1.mana.add("C", 2)
    line1 = solve(s1)
    print("1)", line1)
    assert line1.kind == "deterministic" and line1.p_win == 1.0

    # 2) Jeska runaway + Thoracle in hand -> probabilistic win, quantified
    s2 = GameState(library=["Island"] * 40,
                   hand=["Jeska's Will", "Thassa's Oracle", "Grapeshot"],
                   battlefield=board(
                       krark_body("Krark, the Thumbless"),
                       krark_body("Sakashima of a Thousand Faces", copy_of="Krark, the Thumbless"),
                       Permanent("Veyran, Voice of Duality", summoning_sick=False),
                       Permanent("Harmonic Prodigy", summoning_sick=False),
                       Permanent("Archmage Emeritus", summoning_sick=False),
                       Permanent("Storm-Kiln Artist", summoning_sick=False)))
    s2.mana.add("R", 1); s2.mana.add("C", 2)
    line2 = solve(s2)
    print("2)", line2)
    assert line2.kind == "probabilistic" and line2.p_win > 0.5

    # 3) nothing assembled -> honest "no fast-path win"
    s3 = GameState(library=["Island"] * 40, hand=["Ponder"],
                   battlefield=board(krark_body("Krark, the Thumbless")))
    line3 = solve(s3)
    print("3)", line3)
    assert line3.p_win == 0.0

    print("\nplanner fast-path smoke test passed.")
