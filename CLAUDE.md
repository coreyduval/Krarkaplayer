# CLAUDE.md — agent guide for the Krarkashima simulator

Operational notes for an AI assistant working on this repo. Pair with the human-facing
[README.md](README.md).

## What this is
A Rust **goldfish** simulator for a Krark, the Thumbless + Sakashima of a Thousand Faces
Izzet (UR) coin-flip storm combo deck. It shuffles the ~98-card library (plus the two
commanders) many times, pilots each game, and reports win rate / win-turn / win-condition
mix. **Opponents are inert** — a 4-player pod is modeled as 160 combined life, you at 40,
with no interaction. **Do not add an opponent-interaction model**; that is deliberately out
of scope (it's the human pilot's job).

## Build & test — always verify before claiming success
```bash
cd rust
python run.py build                    # cargo build --release (run.py puts cargo on PATH)
./target/release/krarksim selftest     # must print 4x "... passed"
```
Binary: `rust/target/release/krarksim(.exe)`.

## Run
- `krarksim` — dump deck + registry. Card names print exactly as `--cut` expects them.
- `krarksim sweep [flags]` — batch sim. Key output lines: `mean per-seed P(win)`,
  `win-turn over all winning trials`, win-condition + engine breakdowns, `Ns total, X s/game`.
- `krarksim diag --seed N [--luck L] [--max-turns T]` — verbose single-game log
  (per turn: DRAW / DUG / TUTOR / EXILE / PITCH / DISCARD / CAST / CHECK).

**Convention:** "run a sim" = `sweep --flip-trials 8`; report win% over the 8 flips.

## Sweep flags
`--games N` (seeds/openings) · `--flip-trials N` · `--seed N` (base) · `--max-turns N` ·
`--cut "Card"` (leave-one-out) · `--keep-gate fast|mana|none` · `--keep-min-lands N` ·
`--mull-depth N` · `--send-gate F` · `--win-threshold F`.

## Sample size & timing
A **converged** sweep is **1200 seeds × 8 flips** — heavy-tailed bricks make smaller
samples over-estimate. At full CPU clock ~0.074 s/game → 1200×8 ≈ 12 min. This machine
throttles ~2× (~0.16 s/game) when monitors are off / idle; factor that into ETAs and state
an ETA up front for any long run.

## Source map
| File | Responsibility |
|---|---|
| `src/main.rs` | CLI, modes, `build_deck`, `run_sweep` |
| `src/sim.rs` | per-game loop: mulligan (`MullCfg` / `MULL_CFG`), turns, ramp, `develop`, deploy, tap (`untapped_sources` / `tap_source`) |
| `src/planner.rs` | `DeterministicKillSearch`, `ProbabilisticPlanner`, `tap_out`, `deploy_engine_perms`, `apply_mana_ability_reg` |
| `src/loops.rs` | `develop_candidates`, `develop_score`, `estimate_p_lethal`, `analyze_runaway`, loop detection, `MAGECRAFT_FUEL`, `convert_available` |
| `src/resolver.rs` | `resolve_cast_sample` (Krark flips / copies / storm / magecraft), ETB tutors, discard |
| `src/wishlist.rs` | `card_value` (tutor/keep priority + cost tiebreaker), `best`, `tutor` |
| `src/cards.rs` | card registry + `CardDef` |
| `src/game_state.rs` | `GameState`, `ManaPool` (strict colors; `*` = wildcard), legendary helpers |
| `src/tables.rs` | `mana_source` (mode + produced), `life_per_tap` |
| `src/win.rs` | win predicate |

## Model assumptions — already implemented, do not regress
- Goldfish, no interaction. Counters (FoW / Pact / Fierce Guardianship / Flusterstorm /
  Deflecting Swat / Mogg Salvage / An Offer / Cyclonic Rift) are **not** dead cards — they
  are magecraft/storm **loop fuel**.
- Seat randomization: 0.75 on the draw (extra T1 draw), seeded per game.
- Mulligan default `gate=none` (validated best — keeping functional hands beats
  mulliganing for tempo).
- Mana-source costs modeled: LED / Lotus Petal one-shot, Mox Diamond land-pitch, Mana Vault
  no free untap (+ upkeep damage), life-on-tap (Ancient Tomb / Mana Confluence / Shivan
  Reef), Relic of Legends taps idle legendary creatures.
- Strict mana colors: colorless `C` cannot pay a colored pip; `*` (wildcard) and Treasures
  can pay any color.

## Conventions
- Verify every change: build + `selftest` + a representative `sweep`; report regressions
  honestly, including the numbers.
- For risky engine changes, A/B sweep vs the ~99.2% baseline at 1200×8.
- Keep changes surgical; match surrounding Rust style.
