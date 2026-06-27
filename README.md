# Krarkashima — Krark / Sakashima cEDH combo simulator

A Rust engine that **goldfishes** a Krark, the Thumbless + Sakashima of a Thousand Faces
Izzet (UR) coin-flip storm combo deck and measures how reliably — and how **fast** — it wins.

It shuffles the deck thousands of times, pilots each game with a planner that finds
deterministic kills and high-probability go-offs, and reports the win rate, the turn it
wins, and which line won.

> **Opponents are inert by design.** This is a pure goldfish: a 4-player pod is modeled as
> 160 combined life with no interaction, and you start at 40. Interaction (counters,
> removal, racing) is the pilot's job — not the calculator's.

## Results

cEDH games are decided by the early turns, so the model is **speed-first**: sims cap at
**turn 12** (`--max-turns 12`) and the headline metric is how often — and how early — the
deck wins by then. Latest converged baseline, 1200 seeds × 8 flips:

| Metric | Value |
|---|---|
| **Win by turn 12** | **~96%** (T6 53% · T8 83% · T10 93%) |
| Win-turn | mean ~6.6, median **6**, fastest 2 |
| **Early-win score** (geo, T2–8, earlier = better) | **~1.31** |
| TTK (non-wins penalized at turn 15) | ~6.9 |
| Win conditions | Dualcaster combat ~46% · Urabrask/Vivi burn ~25% · Brain Freeze mill ~17% · Grapeshot storm-burn ~11% |
| Engines | Storm-Kiln Artist ~37% · ritual/Jeska burst ~21% · Vivi/Urabrask ~12% · Archmage ~10% · Tavern Scoundrel ~10% · Birgi ~10% |

## Build

Requires a Rust toolchain (`cargo`). From the repo root:

```bash
cd rust
python run.py build          # wraps `cargo build --release` (puts cargo on PATH)
#  or:  cargo build --release
```

Binary lands at `rust/target/release/krarksim` (`.exe` on Windows).

## Run

```bash
cd rust
EXE=./target/release/krarksim                 # .exe on Windows

$EXE                                          # dump the deck + card registry
$EXE selftest                                 # engine self-tests (prints "passed")
$EXE sweep --flip-trials 8                    # the standard sim: win% over 8 flips
$EXE sweep --games 1200 --flip-trials 8       # full convergence (~9–18 min)
$EXE audit --games 300                        # per-source utilization / waste report
$EXE diag --seed 11                           # verbose play-by-play of one game
python diag_table.py 11                        # same game as a clean per-turn table
                                              #   (Drew / Land / Plays with xN attempts + x/y flips)
```

Convention: **"run a sim" = `sweep --flip-trials 8`**, reporting win% over the 8 flips.

### Useful sweep flags

| Flag | Meaning | Default |
|---|---|---|
| `--games N` | number of distinct shuffles / openings | 30 |
| `--flip-trials N` | go-off coin-flip re-rolls per opening | 10 |
| `--max-turns N` | turn cap; past it a game counts as a non-win | 12 |
| `--seed N` | base RNG seed | 0 |
| `--cut "Card"` | remove one copy (leave-one-out); repeatable | — |
| `--add "Card"` | add one copy from the registry bench; repeatable | — |
| `--send-gate F` | commit gate for non-fatal go-offs (send when P ≥ F) | 0.20 |
| `--win-threshold F` | P(win) the planner treats as lethal | 0.95 |
| `--keep-gate fast\|mana\|none` | mulligan first-hand gate | `fast` |
| `--keep-min-lands N` / `--mull-depth N` | min lands to keep / how deep to mulligan | 2 / 2 |
| `--no-fast-mull` | disable mulligan-for-speed (also sets gate `none`) | off (fast-mull on) |
| `--no-smart-land` | disable color-aware land sequencing | off (smart on) |
| `--no-aggro-cantrips` | stop casting cantrips just for card flow | off (aggro on) |
| `--no-jeska-boost` | drop Jeska's Will's elevated tutor priority | off (boost on) |
| `--dev-cap N` · `--rollout-steps N` | develop-loop cap · go-off rollout depth | 12 · 20 |

A land↔spell swap keeps the deck at 98: `--cut "Mountain" --add "Preordain"`.

## Project layout

```
rust/src/
  main.rs        CLI + modes (sweep / audit / diag / selftest / dump), build_deck
  sim.rs         game loop: mulligan, turns, ramp/develop, deploy, tap, source utilization
  planner.rs     deterministic kill search + probabilistic planner + mana tap-out
  loops.rs       go-off detection, develop scoring, loop / runaway analysis, magecraft fuel
  resolver.rs    cast resolution: Krark flips, copies, storm, magecraft, ETB tutors
  wishlist.rs    card valuation (tutor / keep / discard priority)
  cards.rs       CardDef overlay (type, subtypes, per-cast triggers)
  game_state.rs  GameState, ManaPool (strict colors), legendary helpers
  tables.rs      mana-source table (mode + produced), life-per-tap
  win.rs         win predicate
  tests.rs       selftest scenarios
krarkashima.txt    card registry (name | mana_cost | mana_value | rules_text), read at runtime
overnight/         analysis logs / LOO CSVs from prior runs
original decklist/ the source decklist
```

## Status

Rust is the canonical and only engine; the old Python port has been removed.
For agent-facing operational notes (model assumptions, source map, conventions),
see **[CLAUDE.md](CLAUDE.md)**.
