# Krarkashima — Krark / Sakashima cEDH combo simulator

A Rust engine that **goldfishes** a Krark, the Thumbless + Sakashima of a Thousand Faces
Izzet (UR) coin-flip storm combo deck and measures how reliably — and how fast — it wins.

It shuffles the deck thousands of times, pilots each game with a planner that finds
deterministic kills and high-probability go-offs, and reports the win rate, the turn it
wins, and which line won.

> **Opponents are inert by design.** This is a pure goldfish: a 4-player pod is modeled as
> 160 combined life with no interaction, and you start at 40. Interaction (counters,
> removal, racing) is the pilot's job — not the calculator's.

## Results

Current model (seat-randomized, all fixes), over 1200 seeds × 8 flips:

| Metric | Value |
|---|---|
| **Win rate** | **~99.2%** |
| Win-turn | mean ~7.9, median 7, fastest 3 |
| Win conditions | Thassa's Oracle deck-out ~42% · Dualcaster combat ~32% · Urabrask/Vivi burn ~19% · Brain Freeze mill ~5% · Grapeshot |

## Build

Requires a Rust toolchain (`cargo`). From the repo root:

```bash
cd rust
python run.py build          # wraps `cargo build --release`
#  or:  cargo build --release
```

Binary lands at `rust/target/release/krarksim` (`.exe` on Windows).

## Run

```bash
cd rust
EXE=./target/release/krarksim                 # .exe on Windows

$EXE                                          # dump the deck + card registry
$EXE selftest                                 # engine self-tests (prints 4x "passed")
$EXE sweep --flip-trials 8                    # the standard sim: win% over 8 flips
$EXE sweep --games 1200 --flip-trials 8       # full convergence (~12 min)
$EXE diag --seed 11                           # verbose play-by-play of one game
```

### Useful sweep flags

| Flag | Meaning |
|---|---|
| `--games N` | number of distinct shuffles / openings |
| `--flip-trials N` | go-off coin-flip re-rolls per opening |
| `--seed N` | base RNG seed |
| `--cut "Card Name"` | remove one copy of a card (leave-one-out analysis) |
| `--keep-gate fast\|mana\|none` | mulligan first-hand gate (default `none`) |
| `--keep-min-lands N` | minimum lands in a keepable hand |
| `--mull-depth N` | how deep to mulligan |
| `--max-turns N` | turn cap before a game counts as a brick |

## Project layout

```
rust/src/
  main.rs        CLI + modes (sweep / diag / selftest / dump)
  sim.rs         game loop: mulligan, turns, deploy, develop
  planner.rs     deterministic kill search + probabilistic planner + mana
  loops.rs       go-off detection, develop scoring, loop / runaway analysis
  resolver.rs    cast resolution: Krark flips, copies, storm, magecraft
  wishlist.rs    card valuation (tutor / keep / discard priority)
  cards.rs       card registry + data
  game_state.rs  GameState, ManaPool, board helpers
  tables.rs      mana-source table
  win.rs         win predicate
overnight/         analysis logs from prior runs
original decklist/ the source decklist
```

## For AI agents

See **[CLAUDE.md](CLAUDE.md)** — an operational guide (build/run commands, model
assumptions, source map, conventions) aimed at AI coding assistants.

## Status

Rust is the canonical and only engine; the old Python port has been removed.
