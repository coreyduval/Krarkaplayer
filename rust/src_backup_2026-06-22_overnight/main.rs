//! krarksim — a Rust port of the Krark/Sakashima cEDH solitaire solver.
//!
//! Track A (statistical equivalence): we reproduce the PYTHON behaviour faithfully,
//! aiming for matching aggregates over a seeds x flip-trials sweep, not bit-exact RNG.
//!
//! PHASE 1 (done): card-DB loader + deck builder, validated byte-identical to
//! `seed.build_deck()` (82 registry cards, 98-card deck, 8 Island / 12 Mountain).
//!
//! PHASE 2 (this file): the engine core —
//!   * CardDef registry with the curated TYPES / SUBTYPES / ENGINE overlays (cards.py),
//!   * ManaPool can_pay/pay/treasures + cast_cost + the Krark flip math (game_state.py),
//!   * analyze_cast + resolve_cast_sample + the per-card EFFECTS table (resolver.py).
//! A `--selftest` mode mirrors the Python modules' `__main__` asserts so the port can be
//! validated the moment a build is run.

use std::collections::HashMap;
use std::env;
use std::fs;

mod cards;
mod game_state;
mod loops;
mod planner;
mod resolver;
mod sim;
mod tables;
mod win;
mod wishlist;

use rayon::prelude::*;

#[cfg(test)]
mod tests;

use cards::Registry;

/// Mirror of `seed.build_deck()`: every non-commander registry card, plus basic-land
/// filler to 98 (registry already holds 1 Island + 1 Mountain, so add 7 + 11 -> 8/12).
fn build_deck(reg: &Registry, islands: u32, mountains: u32) -> Vec<String> {
    const COMMANDERS: [&str; 2] = ["Krark, the Thumbless", "Sakashima of a Thousand Faces"];
    let mut deck: Vec<String> = reg
        .ordered_names()
        .iter()
        .filter(|n| !COMMANDERS.contains(&n.as_str()))
        .cloned()
        .collect();
    for _ in 0..(islands - 1) {
        deck.push("Island".to_string());
    }
    for _ in 0..(mountains - 1) {
        deck.push("Mountain".to_string());
    }
    deck
}

fn dump_deck(reg: &Registry) {
    let deck = build_deck(reg, 8, 12);
    let mut counts: HashMap<&str, u32> = HashMap::new();
    for n in &deck {
        *counts.entry(n.as_str()).or_insert(0) += 1;
    }
    let mut rows: Vec<(&str, u32)> = counts.into_iter().collect();
    rows.sort_by(|a, b| a.0.cmp(b.0));
    eprintln!("registry: {} cards | deck: {} cards", reg.len(), deck.len());
    for (name, n) in rows {
        println!("{n} {name}");
    }
}

fn arg_val(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

fn load_registry() -> Registry {
    // try cwd-relative first (cwd=rust during cargo run / tests), then ./ for the binary.
    for path in ["../krarkashima.txt", "krarkashima.txt", "./krarkashima.txt"] {
        if let Ok(text) = fs::read_to_string(path) {
            return Registry::load(&text);
        }
    }
    panic!("cannot find krarkashima.txt");
}

fn median(xs: &mut [i64]) -> f64 {
    xs.sort();
    let n = xs.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        xs[n / 2] as f64
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) as f64 / 2.0
    }
}

fn percentile(sorted: &[i64], pct: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((pct / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn run_sweep(reg: &Registry, n_games: u64, trials: u64, max_turns: i64, win_threshold: f64, seed_base: u64, fizzle_fatal: bool) {
    let deck = build_deck(reg, 8, 12);
    let mut tasks: Vec<(u64, u64)> = Vec::new();
    for s in seed_base..seed_base + n_games {
        for k in 0..trials {
            tasks.push((s, k));
        }
    }
    let total = tasks.len();
    println!("======================================================================");
    println!(
        "  FLIP-DISTRIBUTION SWEEP: {n_games} seeds x {trials} coin-flip trials = {total} games (rust/rayon)"
    );
    println!(
        "  (seeds {}-{}, go-off p>={win_threshold:.2})",
        seed_base,
        seed_base + n_games - 1
    );
    println!("======================================================================");

    let t0 = std::time::Instant::now();
    let results: Vec<sim::GameResult> = tasks
        .par_iter()
        .map(|(s, k)| sim::play_quiet_luck(reg, &deck, *s, *k, max_turns, win_threshold, fizzle_fatal))
        .collect();
    let elapsed = t0.elapsed().as_secs_f64();

    // group by seed
    let mut by_seed: HashMap<u64, Vec<&sim::GameResult>> = HashMap::new();
    for r in &results {
        by_seed.entry(r.seed).or_default().push(r);
    }
    let mut per_seed_wp: Vec<f64> = Vec::new();
    let mut all_turns: Vec<i64> = Vec::new();
    for s in seed_base..seed_base + n_games {
        let rs = by_seed.get(&s).cloned().unwrap_or_default();
        let wins: Vec<&sim::GameResult> = rs.iter().filter(|r| r.won).cloned().collect();
        let mut turns: Vec<i64> = wins.iter().map(|r| r.turn).collect();
        turns.sort();
        let wp = if rs.is_empty() { 0.0 } else { wins.len() as f64 / rs.len() as f64 };
        per_seed_wp.push(wp);
        all_turns.extend(&turns);
        let spread = if turns.is_empty() {
            "median   -- ".to_string()
        } else {
            let mut t = turns.clone();
            format!("median {:>4.1}  (best {}, worst {})", median(&mut t), turns[0], turns[turns.len() - 1])
        };
        println!("  seed {:<4} win {:3.0}% over {} flips   {}", s, 100.0 * wp, rs.len(), spread);
    }

    let total_wins = all_turns.len();
    println!("  ------------------------------------------------------------------");
    let mean_wp = if per_seed_wp.is_empty() { 0.0 } else { per_seed_wp.iter().sum::<f64>() / per_seed_wp.len() as f64 };
    println!(
        "  mean per-seed P(win) {:.1}%   overall {}/{} ({:.0}%) winning trials",
        100.0 * mean_wp,
        total_wins,
        total,
        100.0 * total_wins as f64 / total as f64
    );
    if !all_turns.is_empty() {
        all_turns.sort();
        let mean: f64 = all_turns.iter().sum::<i64>() as f64 / all_turns.len() as f64;
        let mut med = all_turns.clone();
        println!(
            "  win-turn over all winning trials: mean {:.2}  median {}  best {}  worst {}",
            mean,
            median(&mut med) as i64,
            all_turns[0],
            all_turns[all_turns.len() - 1]
        );
        println!(
            "  P10 {}  P25 {}  P50 {}  P75 {}  P90 {}",
            percentile(&all_turns, 10.0),
            percentile(&all_turns, 25.0),
            percentile(&all_turns, 50.0),
            percentile(&all_turns, 75.0),
            percentile(&all_turns, 90.0)
        );
    }
    println!(
        "  {:.1}s total, {:.3}s/game",
        elapsed,
        elapsed / total as f64
    );
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("dump");
    let reg = load_registry();

    match mode {
        "selftest" => {
            resolver::selftest(&reg);
            println!();
            win::selftest(&reg);
            println!();
            loops::selftest(&reg);
            println!();
            planner::selftest(&reg);
        }
        "win" => win::selftest(&reg),
        "loops" => loops::selftest(&reg),
        "planner" => planner::selftest(&reg),
        "devscore" => {
            use game_state::{krark_body, GameState, Permanent};
            let mut s = GameState {
                library: vec!["Island".into(); 40],
                hand: vec![
                    "Ponder".into(), "Brainstorm".into(), "Jeska's Will".into(),
                    "Frantic Search".into(), "Grapeshot".into(), "Strike It Rich".into(),
                ],
                battlefield: vec![
                    krark_body("Krark, the Thumbless", None, false),
                    krark_body("Sakashima of a Thousand Faces", Some("Krark, the Thumbless"), false),
                    Permanent { summoning_sick: false, ..Permanent::new("Veyran, Voice of Duality") },
                    Permanent { summoning_sick: false, ..Permanent::new("Storm-Kiln Artist") },
                ],
                opponent_life: vec![160],
                ..Default::default()
            };
            s.mana.add("R", 2);
            s.mana.add("C", 2);
            for c in ["Ponder", "Brainstorm", "Jeska's Will", "Frantic Search", "Strike It Rich"] {
                println!("{c} {:.4}", loops::develop_score(&s, &reg, c));
            }
        }
        "bench" => {
            // Deterministic, single-threaded compute benchmark (no rayon scheduling noise):
            // run a fixed (seed,trial) workload and report pure wall time. Used to measure
            // optimization deltas more stably than the parallel sweep.
            let games: u64 = arg_val(&args, "--games").and_then(|v| v.parse().ok()).unwrap_or(12);
            let trials: u64 = arg_val(&args, "--flip-trials").and_then(|v| v.parse().ok()).unwrap_or(5);
            let deck = build_deck(&reg, 8, 12);
            let t0 = std::time::Instant::now();
            let mut wins = 0u64;
            let mut total = 0u64;
            for s in 0..games {
                for k in 0..trials {
                    let r = sim::play_quiet_luck(&reg, &deck, s, k, 18, 0.95, false);
                    if r.won {
                        wins += 1;
                    }
                    total += 1;
                }
            }
            let e = t0.elapsed().as_secs_f64();
            println!("bench: {total} games, {wins} wins, {e:.3}s total, {:.4}s/game", e / total as f64);
        }
        "sweep" => {
            let games: u64 = arg_val(&args, "--games").and_then(|v| v.parse().ok()).unwrap_or(30);
            let trials: u64 = arg_val(&args, "--flip-trials").and_then(|v| v.parse().ok()).unwrap_or(10);
            let max_turns: i64 = arg_val(&args, "--max-turns").and_then(|v| v.parse().ok()).unwrap_or(18);
            let win_threshold: f64 = arg_val(&args, "--win-threshold").and_then(|v| v.parse().ok()).unwrap_or(0.95);
            let seed_base: u64 = arg_val(&args, "--seed").and_then(|v| v.parse().ok()).unwrap_or(0);
            let fizzle_fatal = args.iter().any(|a| a == "--fizzle-fatal");
            run_sweep(&reg, games, trials, max_turns, win_threshold, seed_base, fizzle_fatal);
        }
        "diag" => {
            // Verbose single-game log: `krarksim diag --seed N [--luck L] [--max-turns T]`.
            let seed: u64 = arg_val(&args, "--seed").and_then(|v| v.parse().ok()).unwrap_or(0);
            let luck: u64 = arg_val(&args, "--luck").and_then(|v| v.parse().ok()).unwrap_or(0);
            let max_turns: i64 = arg_val(&args, "--max-turns").and_then(|v| v.parse().ok()).unwrap_or(20);
            let deck = build_deck(&reg, 8, 12);
            let mut game = sim::SimGame::new(&reg, &deck, seed, 0.95);
            game.set_dev_seed(seed.wrapping_mul(1_000_003).wrapping_add(luck));
            game.verbose = true;
            println!("=== GAME seed={seed} luck={luck} ===");
            game.print_opening();
            let det = planner::DeterministicKillSearch::default();
            let mut prob = planner::ProbabilisticPlanner {
                mc_sims: 80, max_first: 2, rollout_steps: 20, ..Default::default()
            };
            let mut win_line = None;
            let mut won_turn = 0i64;
            for t in 1..=max_turns {
                if let Some(line) = game.play_turn(&det, &mut prob) {
                    win_line = Some(line);
                    won_turn = t;
                    break;
                }
            }
            match win_line {
                Some(line) => {
                    println!("\n=== WIN — turn {won_turn} ===");
                    let head = if line.kind == "deterministic" {
                        "KILL".to_string()
                    } else {
                        format!("P(win)={:.3}", line.p_win)
                    };
                    println!("  [{head}] {}", line.detail);
                    for a in &line.actions {
                        match &a.card {
                            Some(c) => println!("    - {}:{}", a.kind, c),
                            None => println!("    - {}", a.kind),
                        }
                    }
                }
                None => println!("\n=== NO WIN in {max_turns} turns (BRICK) ==="),
            }
            game.print_zone_inspection();
        }
        _ => dump_deck(&reg),
    }
}
