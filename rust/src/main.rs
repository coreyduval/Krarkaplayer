//! krarksim — a Rust port of the Krark/Sakashima cEDH solitaire solver.
//!
//! Track A (statistical equivalence): we reproduce the PYTHON behaviour faithfully,
//! aiming for matching aggregates over a seeds x flip-trials sweep, not bit-exact RNG.
//!
//! PHASE 1 (done): card-DB loader + deck builder, validated byte-identical to
//! `seed.build_deck()` (98-card deck, 28 lands: 8 Island / 11 Mountain + 9 nonbasic).
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
mod play;
mod resolver;
mod sim;
mod tables;
mod web;
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
    // Cards kept in the registry but intentionally NOT in the deck. Crimson Wisps / Renegade Tactics /
    // Accelerate: red 1-mana cantrips kept in the registry for A/B testing (added to a deck via --add),
    // not in the default list.
    // Extra fetches (Misty Rainforest..Flooded Strand) kept in the registry for --add A/B testing of
    // higher fetch counts; not in the default 3-fetch deck.
    const DECK_EXCLUDE: [&str; 13] = [
        "Crimson Wisps", "Renegade Tactics", "Accelerate",
        "Misty Rainforest", "Arid Mesa", "Wooded Foothills", "Flooded Strand",
        "The One Ring", "Electro, Assaulting Battery", "Grim Monolith",
        // Cut for Molten Duplication / Flare of Duplication (maindeck). Treasonous Ogre stays modeled
        // but benched — Gut Shot kept instead (faster early-win in the A/B, 1.748 vs 1.718).
        "Peek", "Snap", "Treasonous Ogre",
    ];
    let mut deck: Vec<String> = reg
        .ordered_names()
        .iter()
        .filter(|n| !COMMANDERS.contains(&n.as_str()) && !DECK_EXCLUDE.contains(&n.as_str()))
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
    let deck = build_deck(reg, 6, 8);
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

/// All values for a flag that may appear more than once (e.g. repeated `--cut`/`--add`).
fn arg_vals(args: &[String], flag: &str) -> Vec<String> {
    args.iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == flag)
        .filter_map(|(i, _)| args.get(i + 1).cloned())
        .collect()
}

// Mulligan experiment axes: --keep-min-lands N (A), --keep-gate fast|mana|none (B), --mull-depth N (C).
fn parse_mull_cfg(args: &[String]) -> sim::MullCfg {
    let min_lands = arg_val(args, "--keep-min-lands").and_then(|v| v.parse().ok()).unwrap_or(2);
    let depth = arg_val(args, "--mull-depth").and_then(|v| v.parse().ok()).unwrap_or(2);
    let gate = match arg_val(args, "--keep-gate").as_deref() {
        Some("mana") => sim::MullGate::Mana,
        Some("none") => sim::MullGate::None,
        Some("fast") => sim::MullGate::Fast,
        _ if args.iter().any(|a| a == "--no-fast-mull") => sim::MullGate::None,
        _ => sim::MullGate::Fast,
    };
    sim::MullCfg { min_lands, gate, depth }
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

fn run_sweep(reg: &Registry, n_games: u64, trials: u64, max_turns: i64, win_threshold: f64, seed_base: u64, fizzle_fatal: bool, send_gate: f64, fast_mull: bool, rock_cutoff: i64, check_first: bool, t3_probe: bool, cuts: Vec<String>, adds: Vec<String>) {
    let mut deck = build_deck(reg, 6, 8);
    // Leave-one-out / manabase swaps: drop one copy per `--cut`, append one copy per `--add`.
    // A land<->spell swap is `--cut Mountain --add Ponder` (deck stays 98). No-op if a cut isn't present.
    for c in &cuts {
        if let Some(pos) = deck.iter().position(|x| x == c) {
            deck.remove(pos);
        }
    }
    for a in &adds {
        if reg.ordered_names().iter().any(|n| n == a) {
            deck.push(a.clone());
        } else {
            eprintln!("warning: --add '{a}' not in registry; skipped");
        }
    }
    if !cuts.is_empty() || !adds.is_empty() {
        println!("  DECK MOD: cut [{}]  add [{}]  -> {} cards", cuts.join(", "), adds.join(", "), deck.len());
    }
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
        .map(|(s, k)| sim::play_quiet_luck(reg, &deck, *s, *k, max_turns, win_threshold, fizzle_fatal, send_gate, fast_mull, rock_cutoff, check_first))
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
    // Penalized TTK: count any non-win (didn't win by max_turns) as turn 15, averaged over ALL games.
    // One comparable number for "how fast does it close, losses included" (lower = better).
    let pen_ttk: f64 =
        results.iter().map(|r| if r.won { r.turn } else { 15 }).sum::<i64>() as f64 / total as f64;
    println!("  TTK (losses=turn 15): {:.2}  over all {} games", pen_ttk, total);
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
        // 5%-trimmed mean: drop the fastest 5% and slowest 5% of winning trials, average the rest.
        let lo = (all_turns.len() as f64 * 0.05).floor() as usize;
        let hi = all_turns.len() - lo;
        let trimmed = &all_turns[lo..hi];
        let tmean = trimmed.iter().sum::<i64>() as f64 / trimmed.len() as f64;
        println!(
            "  win-turn 5%-trimmed mean (drop best/worst 5%): {:.2}  over middle {} of {} wins",
            tmean, trimmed.len(), all_turns.len()
        );
        // cEDH framing: games rarely pass turn ~10, so wins after that are effectively worthless
        // (someone else closes first). Show the cumulative early-win curve P(win by turn T) over ALL
        // games, the effective win rate by turn 10, and how many "wins" land in the dead zone (T>10).
        let by = |t: i64| all_turns.iter().filter(|&&x| x <= t).count();
        let pct = |c: usize| 100.0 * c as f64 / total as f64;
        println!(
            "  P(win by turn) over all {} games:  T2 {:.1}%  T3 {:.1}%  T4 {:.1}%  T5 {:.1}%  T6 {:.1}%  T7 {:.1}%  T8 {:.1}%  T10 {:.1}%  T12 {:.1}%",
            total, pct(by(2)), pct(by(3)), pct(by(4)), pct(by(5)), pct(by(6)), pct(by(7)), pct(by(8)), pct(by(10)), pct(by(12))
        );
        // cEDH objective: a win on turn t scores 10*0.6^(t-2) points for t in [2,8], else 0 — earlier
        // wins weighted much higher, tailing off non-linearly toward the turn-8 cutoff
        // (T2=10, T3=6, T4=3.6, T5=2.2, T6=1.3, T7=0.78, T8=0.47). Reported as avg points/game
        // (max ~10 if every game won on T2). This is the number to MAXIMIZE.
        let w = |t: i64| -> f64 {
            if (2..=8).contains(&t) { 10.0 * 0.6_f64.powi((t - 2) as i32) } else { 0.0 }
        };
        let early_score: f64 = all_turns.iter().map(|&t| w(t)).sum::<f64>() / total as f64;
        println!(
            "  >>> EARLY-WIN SCORE (geo0.6 T2-8, earlier=better, max~10): {:.3}   |   P(win by T6) {:.1}%   P(win by T8) {:.1}%",
            early_score, pct(by(6)), pct(by(8))
        );
    }
    // win-condition + engine breakdown over winning trials
    if total_wins > 0 {
        let mut wincon: HashMap<String, Vec<i64>> = HashMap::new();
        let mut engine: HashMap<String, usize> = HashMap::new();
        for r in &results {
            if r.won {
                wincon.entry(r.wincon.clone()).or_default().push(r.turn);
                *engine.entry(r.engine.clone()).or_insert(0) += 1;
            }
        }
        println!("  --- WIN CONDITIONS (share of {total_wins} wins, with win-turn) ---");
        let mut wc: Vec<(String, Vec<i64>)> = wincon.into_iter().collect();
        wc.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
        for (name, mut turns) in wc {
            turns.sort();
            let cnt = turns.len();
            let mean = turns.iter().sum::<i64>() as f64 / cnt as f64;
            let mut med = turns.clone();
            println!(
                "    {:<28} {:5} ({:4.1}%)   turn mean {:.1}  median {}  [{}-{}]",
                name, cnt, 100.0 * cnt as f64 / total_wins as f64,
                mean, median(&mut med) as i64, turns[0], turns[cnt - 1]
            );
        }
        println!("  --- ENGINE USED (share of {total_wins} wins) ---");
        let mut ec: Vec<(String, usize)> = engine.into_iter().collect();
        ec.sort_by(|a, b| b.1.cmp(&a.1));
        for (name, cnt) in ec {
            println!("    {:<36} {:5} ({:4.1}%)", name, cnt, 100.0 * cnt as f64 / total_wins as f64);
        }
    }
    // --- Sakashima-deploy probe (option-a lever sizing) ---
    // How often a game sat in a state where the namesake krark-shimmer line was structurally
    // available but Sakashima was stranded in the command zone (a kill the go-off can't currently
    // reach because it never deploys Sakashima mid-combo). Split by eventual outcome: among
    // opportunity games, "lost / won late" ~ a potential WIN-RATE gain; "won anyway" ~ a SPEED gain.
    let opp: Vec<&sim::GameResult> = results.iter().filter(|r| r.sak_opp).collect();
    let opp_n = opp.len();
    let opp_lost = opp.iter().filter(|r| !r.won).count();
    let opp_won = opp_n - opp_lost;
    let opp_won_after = opp.iter().filter(|r| r.won && r.turn > r.sak_opp_turn).count();
    let mean_opp_turn = if opp_n > 0 {
        opp.iter().map(|r| r.sak_opp_turn).sum::<i64>() as f64 / opp_n as f64
    } else {
        0.0
    };
    println!(
        "  --- SAKASHIMA-DEPLOY PROBE (lever size) ---  opportunity in {}/{} games ({:.1}%); mean first-opp turn {:.1}",
        opp_n, total, 100.0 * opp_n as f64 / total as f64, mean_opp_turn
    );
    println!(
        "      of those: {} lost the game (potential win-rate lever), {} won anyway ({} won AFTER the opp turn — potential speed lever)",
        opp_lost, opp_won, opp_won_after
    );
    // --- T3-both-commanders card-lift probe (--t3-probe) ---
    // Which opening-hand cards drive Krark + Sakashima both on board by turn 3? For each card, the
    // rate of T3-both among games whose OPENING HAND held it, vs the base rate. Lift = conditional/base.
    if t3_probe {
        let t3_n = results.iter().filter(|r| r.t3_both).count();
        let base = t3_n as f64 / total as f64;
        let mut open: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
        let mut hit: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
        for r in &results {
            let uniq: std::collections::HashSet<&str> = r.opening.iter().map(|s| s.as_str()).collect();
            for c in uniq {
                *open.entry(c).or_insert(0) += 1;
                if r.t3_both {
                    *hit.entry(c).or_insert(0) += 1;
                }
            }
        }
        println!(
            "  --- T3-BOTH-COMMANDERS PROBE ---  base rate {:.1}% ({}/{} games hit Krark+Sakashima by T3)",
            100.0 * base, t3_n, total
        );
        let mut rows: Vec<(&str, u64, f64, f64)> = open
            .iter()
            .filter(|(_, &o)| o >= (total as u64 / 100).max(20)) // seen in >=1% of games (min 20)
            .map(|(&c, &o)| {
                let h = *hit.get(c).unwrap_or(&0);
                let cond = h as f64 / o as f64;
                (c, o, cond, if base > 0.0 { cond / base } else { 0.0 })
            })
            .collect();
        rows.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
        println!("      {:<34} {:>7} {:>9} {:>6}", "card (in opening hand)", "games", "P(T3)", "lift");
        for (c, o, cond, lift) in rows.iter().take(30) {
            println!("      {:<34} {:>7} {:>8.1}% {:>5.2}x", c, o, 100.0 * cond, lift);
        }
    }
    println!(
        "  {:.1}s total, {:.3}s/game",
        elapsed,
        elapsed / total as f64
    );
}

/// Source-utilization audit: run N games serially with per-source instrumentation on, then report
/// how each mana source is actually used during NON-winning development turns — taps, mana produced,
/// and how often it fires on a turn that ends with mana still floating (wasted). This separates
/// fast-mana that's genuinely WASTED (a pilot problem) from fast-mana that's merely REDUNDANT (the
/// deck had enough mana anyway). Winning turns are excluded — there the mana feeds the kill.
fn run_audit(reg: &Registry, n_games: u64, trials: u64, max_turns: i64, seed_base: u64, send_gate: f64, fast_mull: bool, cuts: Vec<String>, adds: Vec<String>) {
    let mut deck = build_deck(reg, 6, 8);
    for c in &cuts {
        if let Some(pos) = deck.iter().position(|x| x == c) {
            deck.remove(pos);
        }
    }
    for a in &adds {
        if reg.ordered_names().iter().any(|n| n == a) {
            deck.push(a.clone());
        }
    }
    let mut tasks: Vec<(u64, u64)> = Vec::new();
    for s in seed_base..seed_base + n_games {
        for k in 0..trials {
            tasks.push((s, k));
        }
    }
    let t0 = std::time::Instant::now();
    let acc = tasks
        .par_iter()
        .map(|(s, k)| sim::play_audit(reg, &deck, *s, *k, max_turns, send_gate, fast_mull))
        .reduce(sim::AuditStats::default, |mut a, b| {
            a.merge(&b);
            a
        });
    let elapsed = t0.elapsed().as_secs_f64();

    println!("======================================================================");
    println!("  SOURCE-UTILIZATION AUDIT: {} games ({n_games} seeds x {trials} flips)", acc.games);
    if !cuts.is_empty() || !adds.is_empty() {
        println!("  DECK MOD: cut [{}]  add [{}]", cuts.join(", "), adds.join(", "));
    }
    println!("======================================================================");
    println!(
        "  dev (non-winning) turns: {}   mana produced on them: {}   wasted (floated & lost): {} ({:.1}%)",
        acc.dev_turns, acc.produced, acc.wasted,
        if acc.produced > 0 { 100.0 * acc.wasted as f64 / acc.produced as f64 } else { 0.0 }
    );
    println!("  ------------------------------------------------------------------");
    println!("  PER-SOURCE (over dev turns; 'waste%' = taps on a turn that ended with mana floating)");
    println!("  {:<26} {:>8} {:>10} {:>9} {:>8}", "source", "taps", "mana", "mana/tap", "waste%");
    let mut rows: Vec<(&String, &u64)> = acc.src_taps.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1));
    for (name, taps) in rows {
        let mana = *acc.src_produced.get(name).unwrap_or(&0);
        let waste = *acc.src_waste_taps.get(name).unwrap_or(&0);
        println!(
            "  {:<26} {:>8} {:>10} {:>9.2} {:>7.1}%",
            name, taps, mana,
            mana as f64 / *taps as f64,
            100.0 * waste as f64 / *taps as f64
        );
    }
    println!("  ------------------------------------------------------------------");
    println!("  ONE-SHOT FAST MANA (fires = sac'd for mana; waste = fired on a turn ending with mana left)");
    println!("  {:<26} {:>8} {:>10} {:>8}", "source", "fires", "wasteful", "waste%");
    let mut osr: Vec<(&String, &u64)> = acc.oneshot_fires.iter().collect();
    osr.sort_by(|a, b| b.1.cmp(a.1));
    for (name, fires) in osr {
        let w = *acc.oneshot_waste_fires.get(name).unwrap_or(&0);
        println!(
            "  {:<26} {:>8} {:>10} {:>7.1}%",
            name, fires, w, 100.0 * w as f64 / *fires as f64
        );
    }
    println!("  ------------------------------------------------------------------");
    println!(
        "  WHY UNSPENT — turns 2-6 ending with mana floating: {} turns, avg leftover {:.2} mana",
        acc.early_waste_turns,
        if acc.early_waste_turns > 0 { acc.early_waste_mana as f64 / acc.early_waste_turns as f64 } else { 0.0 }
    );
    println!("  non-land cards stuck in hand on those turns (afford% = leftover could pay its cost):");
    println!("  {:<26} {:>8} {:>9} {:>9}", "card", "in-hand", "afford", "afford%");
    let mut hrows: Vec<(&String, &u64)> = acc.hand_card.iter().collect();
    hrows.sort_by(|a, b| b.1.cmp(a.1));
    for (name, cnt) in hrows.iter().take(30) {
        let aff = *acc.hand_card_affordable.get(*name).unwrap_or(&0);
        println!(
            "  {:<26} {:>8} {:>9} {:>8.1}%",
            name, cnt, aff, 100.0 * aff as f64 / **cnt as f64
        );
    }
    println!("  {:.1}s total, {:.4}s/game", elapsed, elapsed / acc.games as f64);
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
        "whatif" => {
            // Diagnostic: ENTIRE non-commander deck in hand, N Krark bodies out, varying starting
            // floating mana. Asks the real win-search (solve) whether it can find a storm/payoff kill.
            // Storm payoffs still need MANA to cast the chain (drawing the deck != casting it).
            use game_state::{krark_body, GameState};
            use rand::SeedableRng;
            // --no-combo: forbid the Dualcaster/Twinflame infinite, forcing a STORM win.
            let no_combo = args.iter().any(|a| a == "--no-combo");
            let combo_pieces = ["Dualcaster Mage", "Twinflame", "Molten Duplication"];
            let deck: Vec<String> = build_deck(&reg, 6, 8)
                .into_iter()
                .filter(|c| !no_combo || !combo_pieces.contains(&c.as_str()))
                .collect();
            let det = planner::DeterministicKillSearch::default();
            let prob = planner::ProbabilisticPlanner { mc_sims: 400, max_first: 3, rollout_steps: 80, ..Default::default() };
            println!("WHATIF{}: entire deck in hand ({} cards incl ~28 lands), EMPTY library, opp 160 life, opp hands [4,4,4].",
                if no_combo { " [no Dualflame combo]" } else { "" }, deck.len());
            println!("p_win that the win-search finds a kill, by #Krark bodies x starting floating mana (R):\n");
            for bodies in [1, 2, 3, 4] {
                for mana in [0, 4, 8, 15] {
                    let mut bf = vec![krark_body("Krark, the Thumbless", None, false)];
                    for _ in 1..bodies {
                        bf.push(krark_body("Sakashima of a Thousand Faces", Some("Krark, the Thumbless"), false));
                    }
                    let mut s = GameState {
                        library: Vec::new(),
                        hand: deck.clone(),
                        battlefield: bf,
                        opponent_life: vec![160],
                        opponent_hand: vec![4, 4, 4],
                        ..Default::default()
                    };
                    s.mana.add("R", mana);
                    let mut rng = rand::rngs::StdRng::seed_from_u64(1);
                    let line = planner::solve(&s, &reg, loops::DEV_PAYOFFS, &det, &prob, &mut rng);
                    let detail: String = line.detail.chars().take(70).collect();
                    println!("  bodies={bodies}  startR={mana:>2}:  p_win={:.3}  [{}]  {}", line.p_win, line.kind, detail);
                }
            }
            // Scenario B: the FAST MANA DEPLOYED on the battlefield (untapped), 0 floating, 2 Krark,
            // rest of the gas in hand. tap_out the rocks, then solve — does the fast mana itself carry
            // the storm kill? (This is what cast_permanents/ramp do for real before the go-off.)
            use game_state::Permanent;
            let rocks = [
                "Sol Ring", "Mox Amber", "Chrome Mox", "Mox Diamond", "Mana Vault",
                "Arcane Signet", "Talisman of Creativity", "Lotus Petal", "Relic of Legends",
            ];
            let mut bf = vec![
                krark_body("Krark, the Thumbless", None, false),
                krark_body("Sakashima of a Thousand Faces", Some("Krark, the Thumbless"), false),
            ];
            for r in rocks {
                bf.push(Permanent { summoning_sick: false, ..Permanent::new(r) });
            }
            // Krark's Thumb deployed too — the reliable return that lets a storm loop sustain.
            bf.push(Permanent { summoning_sick: false, ..Permanent::new("Krark's Thumb") });
            let hand: Vec<String> = deck.iter()
                .filter(|c| !rocks.contains(&c.as_str()) && c.as_str() != "Krark's Thumb")
                .cloned().collect();
            let s = GameState {
                library: Vec::new(),
                hand,
                battlefield: bf,
                opponent_life: vec![160],
                opponent_hand: vec![4, 4, 4],
                ..Default::default()
            };
            let tapped = planner::tap_out(&s);
            let floating: i64 = tapped.mana.total();
            let mut rng = rand::rngs::StdRng::seed_from_u64(1);
            let line = planner::solve(&tapped, &reg, loops::DEV_PAYOFFS, &det, &prob, &mut rng);
            println!("\nScenario B: 9 fast-mana rocks + Krark's Thumb DEPLOYED, 0 floating, 2 Krark, rest of gas in hand.");
            println!("  tap_out produced {floating} floating mana from the rocks.");
            println!("  p_win={:.3}  [{}]  {}", line.p_win, line.kind, line.detail);
        }
        "goff-trace" => {
            // Instrumentation: run the ADAPTIVE rollout (rollout_from) vs the dumb single-card LOOP
            // (estimate_p_lethal) on a realistic go-off-entry state, with the adaptive rollout tracing
            // its per-step option scores. Shows where the greedy policy diverges from the loop.
            use game_state::{krark_body, GameState, Permanent};
            use rand::SeedableRng;
            let mana: i64 = arg_val(&args, "--mana").and_then(|v| v.parse().ok()).unwrap_or(6);
            let mut library = vec!["Island".to_string(); 22];
            for c in ["Ponder", "Opt", "Consider", "Serum Visions", "Mountain", "Brainstorm", "Strike It Rich", "Rite of Flame"] {
                library.push(c.to_string());
            }
            let mut state = GameState {
                library,
                hand: vec![
                    "Jeska's Will".into(), "Rite of Flame".into(), "Pyretic Ritual".into(),
                    "Desperate Ritual".into(), "Grapeshot".into(), "Brain Freeze".into(),
                    "Ponder".into(), "Brainstorm".into(), "Serum Visions".into(),
                    "Frantic Search".into(), "Strike It Rich".into(),
                ],
                battlefield: {
                    let mut bf = vec![
                        krark_body("Krark, the Thumbless", None, false),
                        krark_body("Sakashima of a Thousand Faces", Some("Krark, the Thumbless"), false),
                        Permanent { summoning_sick: false, ..Permanent::new("Storm-Kiln Artist") },
                    ];
                    if args.iter().any(|a| a == "--thumb") {
                        bf.push(Permanent { summoning_sick: false, ..Permanent::new("Krark's Thumb") });
                    }
                    bf
                },
                opponent_life: vec![160],
                opponent_hand: vec![4, 4, 4],
                ..Default::default()
            };
            state.mana.add("R", mana.min(4));
            state.mana.add("*", (mana - 4).max(0));
            let payoffs = loops::DEV_PAYOFFS;
            let need_life: i64 = state.opponent_life.iter().sum();
            println!("=== GO-OFF ENTRY: bodies={} flips/cast={} floating_mana={} storm={} opp_life={} ===",
                state.krark_bodies(&reg), state.flips_per_cast(&reg), state.mana.total(), state.storm_count, need_life);
            let mut cands = loops::develop_candidates(&state, &reg);
            cands.sort_by(|a, b| loops::develop_score(&state, &reg, &b.0)
                .partial_cmp(&loops::develop_score(&state, &reg, &a.0)).unwrap_or(std::cmp::Ordering::Equal));
            println!("  develop menu: {}", cands.iter().take(10)
                .map(|(c, _)| format!("{c}({:+.2})", loops::develop_score(&state, &reg, c))).collect::<Vec<_>>().join(", "));

            println!("\n=== ADAPTIVE rollout_from — one traced run (greedy best-score each step) ===");
            let mut rng = rand::rngs::StdRng::seed_from_u64(7);
            let res = loops::rollout_from(state.clone(), &reg, payoffs, need_life, &mut rng, 60, true);
            println!("  => {:?}", res);
            let n = 400i64;
            let awins = (0..n).filter(|i| {
                let mut r = rand::rngs::StdRng::seed_from_u64(1000 + *i as u64);
                loops::rollout_from(state.clone(), &reg, payoffs, need_life, &mut r, 60, false).is_some()
            }).count();
            println!("  ADAPTIVE p_win over {n} runs: {:.3}", awins as f64 / n as f64);

            println!("\n=== LOOP estimate_p_lethal — single-card spam, per card ===");
            let mut best = (String::new(), 0.0f64);
            for card in ["Grapeshot", "Brain Freeze", "Jeska's Will", "Rite of Flame", "Pyretic Ritual", "Ponder", "Brainstorm"] {
                if !state.hand.iter().any(|c| c == card) { continue; }
                let mut r = rand::rngs::StdRng::seed_from_u64(55);
                let est = loops::estimate_p_lethal(&state, &reg, card, payoffs, n, 80, &mut r, None);
                println!("  loop {card:<16}: p_win={:.3}", est.p_win);
                if est.p_win > best.1 { best = (card.to_string(), est.p_win); }
            }
            println!("  BEST LOOP: {} ({:.3})", best.0, best.1);
        }
        "bench" => {
            // Deterministic, single-threaded compute benchmark (no rayon scheduling noise):
            // run a fixed (seed,trial) workload and report pure wall time. Used to measure
            // optimization deltas more stably than the parallel sweep.
            let games: u64 = arg_val(&args, "--games").and_then(|v| v.parse().ok()).unwrap_or(12);
            let trials: u64 = arg_val(&args, "--flip-trials").and_then(|v| v.parse().ok()).unwrap_or(5);
            let deck = build_deck(&reg, 6, 8);
            let t0 = std::time::Instant::now();
            let mut wins = 0u64;
            let mut total = 0u64;
            for s in 0..games {
                for k in 0..trials {
                    let r = sim::play_quiet_luck(&reg, &deck, s, k, 18, 0.95, false, 0.95, false, i64::MAX, false);
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
            sim::MULL_CFG.set(parse_mull_cfg(&args)).ok();
            sim::DEV_CAP.set(arg_val(&args, "--dev-cap").and_then(|v| v.parse().ok()).unwrap_or(12)).ok();
            sim::ROLLOUT_STEPS.set(arg_val(&args, "--rollout-steps").and_then(|v| v.parse().ok()).unwrap_or(20)).ok();
            planner::RITUAL_PRELUDE.set(args.iter().any(|a| a == "--ritual-prelude")).ok();
            sim::DEAD_HAND_MULL.set(!args.iter().any(|a| a == "--no-dead-hand-mull")).ok();
            loops::AGGRO_CANTRIPS.set(!args.iter().any(|a| a == "--no-aggro-cantrips")).ok();
            loops::PRE_KRARK_DIG.set(args.iter().any(|a| a == "--precrark-dig")).ok();
            sim::SMART_LAND.set(!args.iter().any(|a| a == "--no-smart-land")).ok();
            sim::ADAPTIVE_GATE.set(args.iter().any(|a| a == "--adaptive-gate")).ok();
            sim::SAK_DEPLOY.set(!args.iter().any(|a| a == "--no-sak-deploy")).ok();
            sim::KEEP_ROCK_SOURCES.set(!args.iter().any(|a| a == "--no-keep-rock-sources")).ok();
            sim::T3_MULL.set(args.iter().any(|a| a == "--t3-mull")).ok();
            wishlist::JESKA_BOOST.set(!args.iter().any(|a| a == "--no-jeska-boost")).ok();
            wishlist::TUTOR_CREATURE_KEEP.set(args.iter().any(|a| a == "--tutor-keep")).ok();
            resolver::SHIMMER_TOKENS.set(!args.iter().any(|a| a == "--no-shimmer-tokens")).ok();
            let games: u64 = arg_val(&args, "--games").and_then(|v| v.parse().ok()).unwrap_or(30);
            let trials: u64 = arg_val(&args, "--flip-trials").and_then(|v| v.parse().ok()).unwrap_or(10);
            // cEDH default: games are decided by ~turn 12, so cap compute there (faster; the slow tail
            // is ~worthless anyway). TTK(losses=15) penalizes non-wins instead of dropping them.
            let max_turns: i64 = arg_val(&args, "--max-turns").and_then(|v| v.parse().ok()).unwrap_or(12);
            let win_threshold: f64 = arg_val(&args, "--win-threshold").and_then(|v| v.parse().ok()).unwrap_or(0.95);
            let seed_base: u64 = arg_val(&args, "--seed").and_then(|v| v.parse().ok()).unwrap_or(0);
            let fizzle_fatal = args.iter().any(|a| a == "--fizzle-fatal");
            // commit gate used when a fizzle ISN'T fatal. Default 0.20 (validated best at 1200x8:
            // 99.4% / 7.79 turns vs 0.50's 99.3% / 7.87 — plateaus flat to 0.10, lower just costs solve
            // time): send aggressively when trying is free, keep win_threshold when a fizzle is fatal.
            let send_gate: f64 = arg_val(&args, "--send-gate").and_then(|v| v.parse().ok()).unwrap_or(0.20);
            // mulligan-for-speed default ON (validated free −0.09 turns); opt out with --no-fast-mull.
            let fast_mull = !args.iter().any(|a| a == "--no-fast-mull");
            // stop deploying mana rocks once Krark out + this many mana sources held; default off.
            let rock_cutoff: i64 = arg_val(&args, "--rock-cutoff").and_then(|v| v.parse().ok()).unwrap_or(i64::MAX);
            let check_first = args.iter().any(|a| a == "--check-first");
            let t3_probe = args.iter().any(|a| a == "--t3-probe");
            let cuts = arg_vals(&args, "--cut");
            let adds = arg_vals(&args, "--add");
            run_sweep(&reg, games, trials, max_turns, win_threshold, seed_base, fizzle_fatal, send_gate, fast_mull, rock_cutoff, check_first, t3_probe, cuts, adds);
        }
        "audit" => {
            sim::MULL_CFG.set(parse_mull_cfg(&args)).ok();
            sim::DEV_CAP.set(arg_val(&args, "--dev-cap").and_then(|v| v.parse().ok()).unwrap_or(12)).ok();
            sim::ROLLOUT_STEPS.set(arg_val(&args, "--rollout-steps").and_then(|v| v.parse().ok()).unwrap_or(20)).ok();
            planner::RITUAL_PRELUDE.set(args.iter().any(|a| a == "--ritual-prelude")).ok();
            sim::DEAD_HAND_MULL.set(!args.iter().any(|a| a == "--no-dead-hand-mull")).ok();
            loops::AGGRO_CANTRIPS.set(!args.iter().any(|a| a == "--no-aggro-cantrips")).ok();
            loops::PRE_KRARK_DIG.set(args.iter().any(|a| a == "--precrark-dig")).ok();
            resolver::SHIMMER_TOKENS.set(!args.iter().any(|a| a == "--no-shimmer-tokens")).ok();
            let games: u64 = arg_val(&args, "--games").and_then(|v| v.parse().ok()).unwrap_or(300);
            let trials: u64 = arg_val(&args, "--flip-trials").and_then(|v| v.parse().ok()).unwrap_or(8);
            let max_turns: i64 = arg_val(&args, "--max-turns").and_then(|v| v.parse().ok()).unwrap_or(12);
            let seed_base: u64 = arg_val(&args, "--seed").and_then(|v| v.parse().ok()).unwrap_or(0);
            let send_gate: f64 = arg_val(&args, "--send-gate").and_then(|v| v.parse().ok()).unwrap_or(0.20);
            let fast_mull = !args.iter().any(|a| a == "--no-fast-mull");
            let cuts = arg_vals(&args, "--cut");
            let adds = arg_vals(&args, "--add");
            run_audit(&reg, games, trials, max_turns, seed_base, send_gate, fast_mull, cuts, adds);
        }
        "serve" => {
            // Web UI: `krarksim serve [--port N] [--seed N] <mull flags>`. Opens a local browser app.
            sim::MULL_CFG.set(parse_mull_cfg(&args)).ok();
            sim::DEV_CAP.set(arg_val(&args, "--dev-cap").and_then(|v| v.parse().ok()).unwrap_or(12)).ok();
            sim::ROLLOUT_STEPS.set(arg_val(&args, "--rollout-steps").and_then(|v| v.parse().ok()).unwrap_or(20)).ok();
            planner::RITUAL_PRELUDE.set(args.iter().any(|a| a == "--ritual-prelude")).ok();
            sim::DEAD_HAND_MULL.set(!args.iter().any(|a| a == "--no-dead-hand-mull")).ok();
            loops::AGGRO_CANTRIPS.set(!args.iter().any(|a| a == "--no-aggro-cantrips")).ok();
            loops::PRE_KRARK_DIG.set(args.iter().any(|a| a == "--precrark-dig")).ok();
            resolver::SHIMMER_TOKENS.set(!args.iter().any(|a| a == "--no-shimmer-tokens")).ok();
            let port: u16 = arg_val(&args, "--port").and_then(|v| v.parse().ok()).unwrap_or(8088);
            let seed: u64 = arg_val(&args, "--seed").and_then(|v| v.parse().ok()).unwrap_or(0);
            let deck = build_deck(&reg, 6, 8);
            web::serve(reg, deck, port, seed);
        }
        "play" => {
            // Interactive REPL: `krarksim play --seed N [--luck L] [--max-turns T] <mull flags>`.
            sim::MULL_CFG.set(parse_mull_cfg(&args)).ok();
            sim::DEV_CAP.set(arg_val(&args, "--dev-cap").and_then(|v| v.parse().ok()).unwrap_or(12)).ok();
            sim::ROLLOUT_STEPS.set(arg_val(&args, "--rollout-steps").and_then(|v| v.parse().ok()).unwrap_or(20)).ok();
            planner::RITUAL_PRELUDE.set(args.iter().any(|a| a == "--ritual-prelude")).ok();
            sim::DEAD_HAND_MULL.set(!args.iter().any(|a| a == "--no-dead-hand-mull")).ok();
            loops::AGGRO_CANTRIPS.set(!args.iter().any(|a| a == "--no-aggro-cantrips")).ok();
            loops::PRE_KRARK_DIG.set(args.iter().any(|a| a == "--precrark-dig")).ok();
            resolver::SHIMMER_TOKENS.set(!args.iter().any(|a| a == "--no-shimmer-tokens")).ok();
            let seed: u64 = arg_val(&args, "--seed").and_then(|v| v.parse().ok()).unwrap_or(0);
            let luck: u64 = arg_val(&args, "--luck").and_then(|v| v.parse().ok()).unwrap_or(0);
            let max_turns: i64 = arg_val(&args, "--max-turns").and_then(|v| v.parse().ok()).unwrap_or(12);
            let deck = build_deck(&reg, 6, 8);
            let fast_mull = !args.iter().any(|a| a == "--no-fast-mull");
            play::run_play(&reg, &deck, seed, luck, max_turns, fast_mull);
        }
        "diag" => {
            sim::MULL_CFG.set(parse_mull_cfg(&args)).ok();
            sim::DEV_CAP.set(arg_val(&args, "--dev-cap").and_then(|v| v.parse().ok()).unwrap_or(12)).ok();
            sim::ROLLOUT_STEPS.set(arg_val(&args, "--rollout-steps").and_then(|v| v.parse().ok()).unwrap_or(20)).ok();
            planner::RITUAL_PRELUDE.set(args.iter().any(|a| a == "--ritual-prelude")).ok();
            sim::DEAD_HAND_MULL.set(!args.iter().any(|a| a == "--no-dead-hand-mull")).ok();
            loops::PRE_KRARK_DIG.set(args.iter().any(|a| a == "--precrark-dig")).ok();
            sim::SAK_DEPLOY.set(!args.iter().any(|a| a == "--no-sak-deploy")).ok();
            sim::KEEP_ROCK_SOURCES.set(!args.iter().any(|a| a == "--no-keep-rock-sources")).ok();
            sim::T3_MULL.set(args.iter().any(|a| a == "--t3-mull")).ok();
            // Verbose single-game log: `krarksim diag --seed N [--luck L] [--max-turns T]`.
            let seed: u64 = arg_val(&args, "--seed").and_then(|v| v.parse().ok()).unwrap_or(0);
            let luck: u64 = arg_val(&args, "--luck").and_then(|v| v.parse().ok()).unwrap_or(0);
            let max_turns: i64 = arg_val(&args, "--max-turns").and_then(|v| v.parse().ok()).unwrap_or(12);
            let deck = build_deck(&reg, 6, 8);
            let fast_mull = !args.iter().any(|a| a == "--no-fast-mull");
            let mut game = sim::SimGame::new(&reg, &deck, seed, 0.95, fast_mull);
            game.set_dev_seed(seed.wrapping_mul(1_000_003).wrapping_add(luck));
            game.set_send_gate(0.20); // match the sweep's default aggressive commit gate
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
                    if !line.actions.is_empty() {
                        println!("  SETUP   :");
                        for a in &line.actions {
                            match &a.card {
                                Some(c) => println!("    - {}:{}", a.kind, c),
                                None => println!("    - {}", a.kind),
                            }
                        }
                    }
                    // Deterministic kills get a step-by-step combo walkthrough (probabilistic lines get
                    // the cast-by-cast go-off trace below instead).
                    if !line.walkthrough.is_empty() {
                        println!("  KILL-LINE :");
                        for step in &line.walkthrough {
                            println!("    {step}");
                        }
                    }
                    // Go-off VARIANCE: re-roll the committed go-off independently to show a TYPICAL
                    // attempt (not the proof, which is by construction a winner).
                    if line.kind == "probabilistic" {
                        if let (Some(base), Some(first)) = (&line.base, &line.first) {
                            use rand::SeedableRng;
                            let mut rng = rand::rngs::StdRng::seed_from_u64(
                                seed.wrapping_mul(2_654_435_761).wrapping_add(luck).wrapping_add(0xA5A5),
                            );
                            let n = 12;
                            let wins = (0..n)
                                .filter(|_| {
                                    loops::prove_go_off(base, &reg, (&first.0, &first.1), line.loop_line,
                                        &mut rng, loops::DEV_PAYOFFS, 40, 80, false)
                                })
                                .count();
                            println!("\n=== GO-OFF VARIANCE — {n} independent re-rolls of the committed go-off ===");
                            println!("  reached lethal: {wins}/{n}  (~the line's true success rate; the game committed");
                            println!("   because a fizzle here isn't fatal, so unlucky re-rolls just develop next turn)");
                            // Trace a FAILED re-roll for contrast (winning runs are long by nature — they
                            // last precisely because they dodge the all-win that ends the loop).
                            let mut shown = false;
                            for _ in 0..40 {
                                let mut probe = rng.clone();
                                let won = loops::prove_go_off(base, &reg, (&first.0, &first.1),
                                    line.loop_line, &mut probe, loops::DEV_PAYOFFS, 40, 80, false);
                                if !won {
                                    println!("  --- one re-roll that FIZZLED, cast by cast (this is the unlucky ~half) ---");
                                    loops::prove_go_off(base, &reg, (&first.0, &first.1), line.loop_line,
                                        &mut rng, loops::DEV_PAYOFFS, 40, 80, true);
                                    shown = true;
                                    break;
                                }
                                rng = probe;
                            }
                            if !shown {
                                println!("  --- one RANDOM re-roll, cast by cast ---");
                                loops::prove_go_off(base, &reg, (&first.0, &first.1), line.loop_line,
                                    &mut rng, loops::DEV_PAYOFFS, 40, 80, true);
                            }
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
