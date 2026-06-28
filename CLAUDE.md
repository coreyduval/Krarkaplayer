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
./target/release/krarksim selftest     # must print "... passed" (resolver/win/loops/planner)
```
Binary: `rust/target/release/krarksim(.exe)`.

## Run
- `krarksim` — dump deck + registry. Card names print exactly as `--cut` / `--add` expect them.
- `krarksim sweep [flags]` — batch sim. Key output lines: `mean per-seed P(win)`, `TTK`,
  `EARLY-WIN SCORE`, `P(win by turn)`, win-condition + engine breakdowns, `Ns total, X s/game`.
- `krarksim audit [flags]` — per-mana-source utilization + waste report (how often each source
  taps, mana produced vs wasted, one-shot mis-fires, affordability of hand cards).
- `krarksim diag --seed N [--luck L] [--max-turns T]` — verbose single-game log
  (per turn: DRAW / DUG / TUTOR / EXILE / PITCH / DISCARD / CAST / CHECK).
- `python diag_table.py N [extra diag flags]` (run from `rust/`) — the **preferred** way to show a
  game: parses the diag into a clean per-turn Markdown table (Drew / Land / Plays with `xN`
  attempts + `x/y` flips), the win line, and the go-off flip sequence. Handles `[KILL]` + `[P(win)]`.

**Convention:**
- "run a sim" = **one-seed verbose output** — a single `diag --seed N` game (preferably via
  `python diag_table.py N`); show the play-by-play, not aggregate stats.
- "run a sweep" = **600 random seeds × 8 flips** by default (`sweep --games 600 --flip-trials 8`);
  report win% / early-win / TTK over the batch.

## Metric framing (speed-first)
cEDH games are decided early, so the default `--max-turns 12` caps compute at turn 12 and the
deck is judged on **speed**, not just eventual win%:
- **EARLY-WIN SCORE** — geometric, weights T2–8, earlier = better (~1.71 baseline). Primary lever.
- **win-by-T12** (~98%) and **TTK** (non-wins penalized at turn 15, ~6.2). Past T12 is ~worthless.

## Sweep flags (defaults in parens)
`--games N` (30) · `--flip-trials N` (10) · `--seed N` (0) · `--max-turns N` (12) ·
`--win-threshold F` (0.95) · `--send-gate F` (0.20, commit gate when a fizzle isn't fatal) ·
`--cut "Card"` / `--add "Card"` (both repeatable; LOO + bench swaps) ·
`--keep-gate fast|mana|none` (**fast**) · `--keep-min-lands N` (2) · `--mull-depth N` (2) ·
`--no-fast-mull` · `--no-dead-hand-mull` (dead-hand override is **on**) · `--no-smart-land` ·
`--no-aggro-cantrips` · `--no-jeska-boost` · `--ritual-prelude` (off; experimental) ·
`--dev-cap N` (12) · `--rollout-steps N` (20). `audit` shares most flags (games default 300).

## Sample size & timing
A **converged** sweep is **1200 seeds × 8 flips** — heavy-tailed bricks make smaller samples
over-estimate; the early-win noise floor at 600×8 is ~0.03–0.04. At full CPU clock ~0.057
s/game → 1200×8 ≈ 9–11 min. This machine throttles ~2× (~0.11–0.16 s/game) when monitors are
off / idle; factor that into ETAs and **state an ETA up front for any long run**.

## Source map
| File | Responsibility |
|---|---|
| `src/main.rs` | CLI, modes, `build_deck` (+`DECK_EXCLUDE`), `run_sweep`, `run_audit`, `parse_mull_cfg` |
| `src/sim.rs` | per-game loop: mulligan (`MullCfg`/`MULL_CFG`), turns, ramp, `develop`, deploy, tap (`untapped_sources`/`tap_source`), `is_dormant_rock`, source utilization |
| `src/planner.rs` | `DeterministicKillSearch`, `ProbabilisticPlanner`, `tap_out`, `deploy_engine_perms`, `apply_mana_ability_reg` |
| `src/loops.rs` | `develop_candidates`, `develop_score`, `estimate_p_lethal`, `analyze_runaway`, loop detection, `MAGECRAFT_FUEL`, `CAST_VALUE_ENGINES`, `convert_available` |
| `src/resolver.rs` | `resolve_cast_sample` (Krark flips / copies / storm / magecraft), ETB tutors, discard |
| `src/wishlist.rs` | `card_value` (tutor/keep priority + cost tiebreaker), `best`, `tutor` |
| `src/cards.rs` | `CardDef` overlay: type lists, `subtypes_for`, per-cast trigger values (`mana_per_trigger`, etc.) |
| `src/game_state.rs` | `GameState`, `ManaPool` (strict colors; `*` = wildcard), `LEGENDARY_CREATURES` helpers |
| `src/tables.rs` | `mana_source` (mode + produced), `life_per_tap` |
| `src/win.rs` | win predicate |
| `src/tests.rs` | selftest scenarios |

## Registry & adding a card (read this before touching cards)
- The registry is **`krarkashima.txt`**, read at **runtime** (`main.rs` load_registry): one line
  per card, `name|mana_cost|mana_value|rules_text`. `build_deck` auto-includes every registry
  card **not** a commander and **not** in the compiled `DECK_EXCLUDE`.
- **Gotcha:** editing `krarkashima.txt` *or rebuilding* while a sweep/audit is running CORRUPTS
  it (or panics on an unknown card). Do registry edits only when nothing is running.
- A new card needs **(1)** a registry line, **(2)** a `cards.rs` overlay entry (type list +
  any mechanic), and **(3)** for bench-only test cards, a `DECK_EXCLUDE` entry so it's reachable
  only via `--add`. Rules text in the registry is informational — behavior is keyed by name in code.
- **Lands gotcha:** `is_land_name` checks a hand-maintained `lands_set()` name list (type-blind),
  so any new land must be added there or it sits dead in hand.
- Bench cards already wired for `--add` A/B: Crimson Wisps / Renegade Tactics / Accelerate (red
  cantrips), extra fetches, The One Ring, Electro Assaulting Battery (Birgi-clone), Grim Monolith.

## Model assumptions — already implemented, do not regress
- Goldfish, no interaction. Counters (FoW / Pact / Fierce Guardianship / Flusterstorm /
  Deflecting Swat / Mogg Salvage / An Offer / Cyclonic Rift) are **not** dead cards — they
  are magecraft/storm **loop fuel** (need a spell on the stack to target; loopable only with
  ≥2 counters **and** a non-counter seed spell). **Pact of Negation fires ONLY as part of a kill**
  — outside a winning turn it owes {3}{U}{U} next upkeep or you lose.
- **Win lines the planner recognizes (do not regress):** *combat* — Dualcaster Mage +
  Twinflame/Heat Shimmer = infinite hasty attackers, AND Krark + a shimmer + the Sakashima
  legend-rule break + renewable mana = infinite hasty Krarks (steer each cast to 1 loss → it
  returns, wins → token Krarks); *burn* — Vivi/Urabrask 3-per-cast + Grapeshot storm; *mill* —
  Brain Freeze decks the pod. Thassa's Oracle is **cut** (`DECK_EXCLUDE`).
- Seat randomization: 0.75 on the draw (extra T1 draw), seeded per game.
- **Mulligan default `gate=fast`** (mulligan-for-speed: first keep needs explosive mana / an
  engine / a combo piece; validated −0.09 turns free). `--no-fast-mull` reverts to `none`.
  **Dead-hand override** (default on; `--no-dead-hand-mull`): at the mulligan floor, don't
  force-keep a hand with no land AND no land-independent mana (Lotus Petal / Chrome Mox / LED /
  Simian) — it can't make turn-1 mana — mulligan deeper (up to depth+2) instead.
- Manabase: fetchland package (Steam Vents + 3 fetches; grab Vents, thin the library, shock in
  untapped), color-aware land sequencing (`SMART_LAND`, default on). Scry/surveil modeled
  (Opt/Consider/Serum/Preordain/Ponder). Cantrips cast aggressively for flow (`AGGRO_CANTRIPS`).
- Mana-source costs modeled: LED / Lotus Petal one-shot, Mox Diamond land-pitch, dormant rocks
  (Mana Vault, Grim Monolith) no free untap (Mana Vault also bleeds 1 life/turn), life-on-tap
  (Ancient Tomb / Mana Confluence / Shivan Reef), Relic of Legends taps idle legendary creatures.
- Strict mana colors: colorless `C` cannot pay a colored pip; `*` (wildcard) and Treasures
  can pay any color.

## Conventions
- Verify every change: build + `selftest` + a representative `sweep`; report regressions
  honestly, including the numbers.
- For risky engine changes, A/B sweep at 1200×8 vs the baseline (~98% win-by-T12 / early-win
  ~1.71 / TTK ~6.2). Optimize the **early-win score**; guard win-by-T12 ≥ ~95%.
- Keep changes surgical; match surrounding Rust style.
