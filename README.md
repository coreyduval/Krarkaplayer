# Krark / Sakashima cEDH play advisor

A solitaire combo solver for the Krark, the Thumbless + Sakashima Izzet coin-flip
deck. Give it a board state; it returns either a deterministic `KILL` (with the
exact action sequence) or a probabilistic line with `P(win)`.

## For Claude Code: run this for me

Everything is plain Python 3 with **no third-party dependencies** (stdlib only).

### 1. Layout

Put all files in one directory. Required:

```
cards.py          # static card data, loaded from krarkashima.txt
game_state.py     # the GameState object (truth) + observation encoder
resolver.py       # cast resolver: flip chance-nodes, storm math, EV
win.py            # win predicate (Thoracle / Grapeshot / Brain Freeze / combat)
loops.py          # loop detector + semi-infinite "runaway" analysis
planner.py        # search: deterministic DFS + probabilistic planner
dryrun.py         # board-notation parser + run() entry point
krarkashima.txt   # the decklist with costs + rules text (data source)
```

`krarkashima.txt` must sit next to `cards.py` — the loader reads it from there.

### 2. Verify the build (each module self-tests)

```bash
python cards.py        # loads 70 cards, prints type breakdown, 0 stubs
python win.py          # win-predicate + flip-count assertions
python resolver.py     # EV + storm-math assertions
python loops.py        # runaway analysis + loop detector (slowest, ~30-60s)
python planner.py      # search smoke test
python dryrun.py       # 3 worked examples end to end
```

All six should exit 0. If `cards.py` raises "can't classify", `krarkashima.txt`
is missing or moved.

### 3. Run a sim

```bash
python -c "
from dryrun import run
run('''
MANA: R R U
HAND: Jeska\'s Will, Thassa\'s Oracle
BOARD: Krark, Sakashima(as Krark), Veyran, Harmonic, Archmage, Storm-Kiln
LIBRARY: 40
STORM: 0
TURN: my T4, main 1
DEFENSE GRID: no
''')
"
```

Or start a REPL (`python`) and call `run("...")` repeatedly with new states.

## Board-state notation (the `run()` input)

One field per line, `KEY: value`:

| Field | Meaning | Example |
|---|---|---|
| `MANA` | floating mana; letters = colors, number = generic, `*` = any | `R R U 1` |
| `HAND` | comma-separated card names (short names OK) | `Jeska's Will, Grapeshot` |
| `BOARD` | permanents; `X(as Y)` for copies | `Krark, Sakashima(as Krark), Veyran` |
| `GY` | graveyard cards | `Brain Freeze, Strike It Rich` |
| `LIBRARY` | card count (filled with neutral fillers) | `40` |
| `STORM` | spells cast so far this turn | `0` |
| `TURN` | starts with `opp`/`their` = not my turn | `my T4, main 1` |
| `DEFENSE GRID` | `yes` adds the +3 tax on my spells during opp turns | `no` |
| `STACK` | accepted but not modeled yet | `empty` |

Short names resolve to full ones (`Krark` → `Krark, the Thumbless`,
`Storm-Kiln` → `Storm-Kiln Artist`, `thoracle` → `Thassa's Oracle`, etc.).
Use short names in `BOARD`/`HAND` so commas only separate cards.

## Reading the output

```
READ:
  bodies=2 doublers=2 flips/cast=6 p=0.50 | blue devotion=3 | storm=0 library=40 | mana={'R':1,'C':2} (my turn)
RESULT:
  [P(win)=1.000] cast:Jeska's Will  (MANA_RUNAWAY via Jeska's Will -> Thassa's Oracle (mean 3.7 casts, deck-out 0.00))
```

- `[KILL]` = deterministic, wins under worst-case flips; the `line:` lists the
  exact actions (including which lands to tap).
- `[P(win)=…]` = probabilistic; the detail names the runaway engine and payoff.

## What it models (and what it doesn't)

- **Solves only your side, assumes uninterrupted resolution** — no counterspell
  hedging, no opponent interaction. Defense Grid's +3 on your spells during
  opponents' turns *is* counted (pure mana math).
- **Flip engine**: flips/cast = bodies × (1 + doublers); `p = 0.75` with Krark's
  Thumb. Value triggers use the subtype-aware doubler rule (Harmonic only doubles
  Shaman/Wizard; Veyran only doubles I/S-caused triggers).
- **Copies are not cast** — Krark and Storm copies never add to the storm count.
- **Mana fixing / color conversion**: a red-flooded runaway reaches the Oracle's
  `{U}{U}` by converting — casting Strike It Rich (a Treasure per resolution, so
  the Krark copies make several) or an any-color ramp rock. The payoff's colored
  pips are checked against this, not just the floating pool.
- **Treasures persist across turns**: a Treasure is a token you sacrifice later,
  not floating mana, so the sim banks unspent ones (`mana.treasures`) and spends
  them only after ephemeral mana — they carry to the next turn as any-color ramp.
- **An infinite loop is never a win without an accessible payoff.**
- **Library = filler cards**: it reasons about library *size* (correct for
  Thassa's Oracle gating) but won't draw into a specific card. Put the pieces you
  care about in `HAND`/`BOARD`.
- **Not certified**: Tavern Scoundrel and Vivi Ornitier (see `cards.VERIFY`) —
  the engine flags any line leaning on them instead of asserting a kill.

## Knobs

- `solve(state, payoffs=("Grapeshot","Thassa's Oracle"))` — restrict win routes.
- `DeterministicKillSearch(max_depth=12, node_budget=30000)` — DFS budget.
- `ProbabilisticPlanner(mc_sims=1500)` — Monte-Carlo samples for P(win).

## Architecture (for extending)

`cards → game_state → resolver → win → loops → planner → dryrun`. The two search
stubs worth growing are deeper expectimax in `ProbabilisticPlanner.best_line` and
richer `enumerate_actions` (targets/X/mana plans). `game_state.encode_observation()`
and the `PolicyValueNet` protocol in `planner.py` are the hooks for an
AlphaZero-style guide if/when search branching demands it.
