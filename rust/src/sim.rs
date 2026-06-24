//! sim.rs — port of sim.py SimGame + sweep CLI. Solitaire game simulation.

use crate::cards::{CardType, Registry};
use crate::game_state::{GameState, ManaPool, Permanent};
use crate::loops;
use crate::planner::{
    deploy_engine_perms, is_engine_permanent, solve, tap_out, DeterministicKillSearch, Line,
    ProbabilisticPlanner,
};
use crate::resolver::{apply_etb, discard_rank, ResolveLog};
use crate::tables::{is_mana_source, mana_source, SrcMode};
use crate::wishlist;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

const DEV_PAYOFFS: &[&str] = &["Grapeshot", "Thassa's Oracle", "Brain Freeze"];
const MAX_HAND_SIZE: usize = 7;

/// Verbose-only: render a cast cost HashMap as a stable string, e.g. "{U}{1}" or "{2}{R}".
/// Colored pips first (canonical W U B R G C order), then generic as "{N}". Free = "{0}".
fn fmt_cost(cost: &HashMap<String, i64>) -> String {
    use crate::game_state::MANA_SYMS;
    let mut s = String::new();
    for sym in MANA_SYMS.iter() {
        if let Some(&n) = cost.get(*sym) {
            for _ in 0..n {
                s.push_str(&format!("{{{sym}}}"));
            }
        }
    }
    let generic = cost.get("generic").copied().unwrap_or(0);
    if generic > 0 {
        s.push_str(&format!("{{{generic}}}"));
    }
    if let Some(&x) = cost.get("X") {
        if x > 0 {
            s.push_str("{X}");
        }
    }
    if s.is_empty() {
        s.push_str("{0}");
    }
    s
}

/// Verbose-only: render a ManaPool's floating mana + persistent treasures, e.g.
/// "[U U R, +2T]" or "[empty]". '*' is wildcard floating mana.
fn fmt_pool(pool: &ManaPool) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (sym, n) in pool.iter() {
        for _ in 0..n {
            parts.push(sym.to_string());
        }
    }
    let body = if parts.is_empty() { "empty".to_string() } else { parts.join(" ") };
    if pool.treasures > 0 {
        format!("[{body}, +{}T]", pool.treasures)
    } else {
        format!("[{body}]")
    }
}

fn mana_rocks() -> &'static [&'static str] {
    &[
        "Sol Ring", "Arcane Signet", "Chrome Mox", "Mox Diamond", "Lotus Petal",
        "Springleaf Drum", "Mana Vault", "Mox Amber", "Relic of Legends",
    ]
}

fn lands_set() -> &'static [&'static str] {
    &[
        "Island", "Mountain", "Great Furnace", "Seat of the Synod", "Otawara, Soaring City",
        "Ancient Tomb", "Command Tower", "Shivan Reef", "Sulfur Falls", "Volcanic Island",
        "Mana Confluence",
    ]
}
fn is_land_name(n: &str) -> bool {
    lands_set().contains(&n)
}

// #1 adaptive go-off search: cheap scan with SCAN_SIMS rollouts first; only escalate to the full
// 80-sim solve when the cheap pass scores in [SCAN_FLOOR, 1.0). A true win (~0.95) is never judged
// below SCAN_FLOOR by 20 sims, so escalation never misses a real win.
const SCAN_SIMS: i64 = 20;
const SCAN_FLOOR: f64 = 0.5;
// Option 3 (mulligan-for-speed): cards that make a hand EXPLOSIVE — fast mana, a per-cast value
// engine, or a combo piece / tutor. With --fast-mull on, the FIRST keep requires one of these, so a
// merely-functional slow (lands + payoff, no acceleration) hand is mulliganed once to seek tempo.
const FAST_KEEP: &[&str] = &[
    // explosive mana
    "Sol Ring", "Mana Vault", "Lotus Petal", "Chrome Mox", "Mox Diamond", "Mox Amber",
    "Lion's Eye Diamond", "Jeska's Will", "Rite of Flame", "Pyretic Ritual", "Desperate Ritual",
    "Strike It Rich",
    // per-cast engines + trigger doublers
    "Storm-Kiln Artist", "Archmage Emeritus", "Birgi, God of Storytelling", "Tavern Scoundrel",
    "Veyran, Voice of Duality", "Harmonic Prodigy", "Vivi Ornitier", "Urabrask",
    // combo pieces + tutors
    "Twinflame", "Heat Shimmer", "Dualcaster Mage", "Spellseeker", "Imperial Recruiter", "Gamble",
];
// Explosive-mana subset of FAST_KEEP — fast mana that deploys Krark (a body) by ~turn 2.
const FAST_MANA_KEEP: &[&str] = &[
    "Sol Ring", "Mana Vault", "Lotus Petal", "Chrome Mox", "Mox Diamond", "Mox Amber",
    "Lion's Eye Diamond", "Jeska's Will", "Rite of Flame", "Pyretic Ritual", "Desperate Ritual",
    "Strike It Rich",
];

// ── Mulligan policy (3 experiment axes) ──────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MullGate {
    Fast, // first hand must hold any FAST_KEEP card (default)
    Mana, // first hand must hold explosive MANA (guarantees an early body)
    None, // no first-hand requirement (plain keepable)
}
#[derive(Clone, Copy, Debug)]
pub struct MullCfg {
    pub min_lands: usize, // Axis A: minimum lands in a keepable hand (default 1)
    pub gate: MullGate,   // Axis B: first-hand (mulls==0) explosiveness requirement
    pub depth: usize,     // Axis C: forced-keep mulligan count → keep down to 7-depth cards (default 2 → 5)
}
impl Default for MullCfg {
    fn default() -> Self {
        // gate=None validated as the best mulligan policy (2026-06-23 experiment): keeping
        // slow-but-functional hands beats mulliganing for tempo. The old Fast gate was net-negative
        // after the correctness + card-flow changes inverted its original "faster" result.
        MullCfg { min_lands: 1, gate: MullGate::None, depth: 2 }
    }
}
/// Process-global mulligan policy, set once from the CLI before the (parallel) sweep runs.
pub static MULL_CFG: std::sync::OnceLock<MullCfg> = std::sync::OnceLock::new();
fn mull_cfg() -> MullCfg {
    MULL_CFG.get().copied().unwrap_or_default()
}

/// Max develop casts per turn (the loop also stops earlier on no carryover-progress / the library
/// floor). 8 under-develops; 60 over-commits to marginal go-offs; default a middle ground.
pub static DEV_CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
fn dev_cap() -> usize {
    *DEV_CAP.get().unwrap_or(&12)
}

const SAC_ON_PLAY: &[&str] = &["Lotus Petal"];
const DISCARD_LAND_ON_PLAY: &[&str] = &["Mox Diamond"];
// Cards Chrome Mox should not imprint (exile) if anything else is available — payoffs/combo pieces.
const NEVER_IMPRINT: &[&str] = &[
    "Thassa's Oracle", "Grapeshot", "Brain Freeze", "Underworld Breach",
    "Gale, Waterdeep Prodigy", "Twinflame", "Heat Shimmer", "Dualcaster Mage",
];
const RAMP_SPELLS: &[&str] = &["Jeska's Will"];

const CLONES: &[&str] = &["Sakashima of a Thousand Faces", "Glasspool Mimic", "Phantasmal Image"];

// _ACTION = wishlist._ENGINE | {Twinflame, Dualcaster Mage, Heat Shimmer}
fn is_action(name: &str) -> bool {
    wishlist::in_engine_pub(name)
        || matches!(name, "Twinflame" | "Dualcaster Mage" | "Heat Shimmer")
}

fn play_priority(name: &str) -> Option<i64> {
    let p = match name {
        "Chrome Mox" => 0,
        "Mox Amber" => 0,
        "Mox Diamond" => 1,
        "Sol Ring" => 1,
        "Mana Vault" => 1,
        "Arcane Signet" => 2,
        "Springleaf Drum" => 2,
        "Relic of Legends" => 3,
        // Mystic Remora: {U} for one delayed draw then sac (conservative: ~1 opponent trigger).
        // Low priority — cast it for card flow when nothing better to do (the grindy tail). The
        // draw + sacrifice are handled in play_turn. (user model, 2026-06-22)
        "Mystic Remora" => 6,
        "Krark's Thumb" => 3,
        "Tavern Scoundrel" => 3,
        "Baral, Chief of Compliance" => 3,
        "Ragavan, Nimble Pilferer" => 3,
        "Krark, the Thumbless" => 3,
        "Birgi, God of Storytelling" => 4,
        "Harmonic Prodigy" => 4,
        "Okaun, Eye of Chaos" => 4,
        "Sakashima of a Thousand Faces" => 4,
        "Zndrsplt, Eye of Wisdom" => 4,
        "Rhystic Study" => 4,
        "Glasspool Mimic" => 5,
        "Phantasmal Image" => 5,
        "Vivi Ornitier" => 5,
        "Archmage Emeritus" => 5,
        "Storm-Kiln Artist" => 5,
        "Veyran, Voice of Duality" => 5,
        "Urabrask" => 5,
        "Imperial Recruiter" => 5,
        "Spellseeker" => 5,
        "The One Ring" => 5,
        "Valley Floodcaller" => 6,
        "Gale, Waterdeep Prodigy" => 6,
        _ => return None,
    };
    Some(p)
}

// ETB tutors (mirror resolver.ETB_TUTORS keys)
fn is_etb_tutor(name: &str) -> bool {
    matches!(
        name,
        "Spellseeker" | "Imperial Recruiter" | "Okaun, Eye of Chaos" | "Zndrsplt, Eye of Wisdom"
    )
}

pub struct SimGame<'a> {
    reg: &'a Registry,
    win_threshold: f64,
    send_gate: f64, // commit threshold when a fizzle ISN'T fatal (>= win_threshold = old behavior)
    fast_mull: bool, // Option 3: first keep must be explosive (mulligan slow hands for tempo)
    rock_skip_cutoff: i64, // stop deploying rocks once Krark out + this many mana sources (MAX=off)
    check_kill_first: bool, // A/B: check for a win at turn start, before ramping/casting
    dev_rng: StdRng,
    goff_base: Option<GameState>,
    scan_nowin: HashSet<u64>, // per-turn memo of state signatures already proven no-win (#3)

    hand: Vec<String>,
    library: Vec<String>,
    board: Vec<(String, Option<String>)>, // (name, copy_of)
    tapped: HashSet<usize>,
    turn: i64,
    command_zone: Vec<String>,
    cmd_tax: HashMap<String, i64>,
    one_ring: i64,
    vivi_power: i64,        // Vivi Ornitier's accumulated +1/+1 counters (persists across turns)
    vivi_mana_used: bool,   // Vivi's {0} mana ability is once per turn; reset each turn
    treasures: i64,
    opponent_life: Vec<i64>,
    our_life: i64,
    graveyard: Vec<String>,
    exile_gas: HashMap<String, i64>,
    played_land: bool,

    pub mulligans: usize,
    pub bottomed: Vec<String>,
    pub verbose: bool,
    pub fizzle_fatal: bool, // when true, an ATTEMPTED go-off that fizzles ends the game as a LOSS
    pub dead: bool,         // set when a fatal fizzle happens (no second chance — dead next turn)
    pub on_the_draw: bool,  // 4-player pod: random seat, so 3/4 of games we're on the draw (T1 draw)
    pub sacrificed: HashSet<usize>, // board idxs spent as one-shot sac sources (Lotus Petal): gone for good
}

impl<'a> SimGame<'a> {
    pub fn new(reg: &'a Registry, deck: &[String], rng_seed: u64, win_threshold: f64, fast_mull: bool) -> SimGame<'a> {
        let dev_rng = StdRng::seed_from_u64(rng_seed.wrapping_mul(7919).wrapping_add(1));
        let mut shuffle_rng = StdRng::seed_from_u64(rng_seed);
        // 4-player pod: random seat draw. Only the starting player (1/4) is on the play; the other
        // 3/4 are on the draw and see an extra card on turn 1. Seeded off rng_seed for reproducibility.
        let on_the_draw = StdRng::seed_from_u64(rng_seed ^ 0x5EA7_5EA7_5EA7_5EA7).gen_bool(0.75);
        let mut g = SimGame {
            reg,
            win_threshold,
            send_gate: win_threshold, // default: identical to old behavior until set
            fast_mull,
            rock_skip_cutoff: i64::MAX, // off by default (rocks always allowed)
            check_kill_first: false,
            dev_rng,
            goff_base: None,
            scan_nowin: HashSet::new(),
            hand: Vec::new(),
            library: Vec::new(),
            board: Vec::new(),
            tapped: HashSet::new(),
            turn: 0,
            command_zone: vec![
                "Krark, the Thumbless".into(),
                "Sakashima of a Thousand Faces".into(),
            ],
            cmd_tax: HashMap::from([
                ("Krark, the Thumbless".to_string(), 0),
                ("Sakashima of a Thousand Faces".to_string(), 0),
            ]),
            one_ring: 0,
            vivi_power: 0,
            vivi_mana_used: false,
            treasures: 0,
            opponent_life: vec![160],
            our_life: crate::game_state::STARTING_LIFE,
            graveyard: Vec::new(),
            exile_gas: HashMap::new(),
            played_land: false,
            mulligans: 0,
            bottomed: Vec::new(),
            verbose: false,
            fizzle_fatal: false,
            dead: false,
            on_the_draw,
            sacrificed: HashSet::new(),
        };
        g.london_mulligan(deck, &mut shuffle_rng);
        g
    }

    pub fn set_dev_seed(&mut self, seed: u64) {
        self.dev_rng = StdRng::seed_from_u64(seed);
    }

    pub fn set_send_gate(&mut self, g: f64) {
        self.send_gate = g;
    }

    pub fn set_rock_cutoff(&mut self, c: i64) {
        self.rock_skip_cutoff = c;
    }

    pub fn set_check_first(&mut self, b: bool) {
        self.check_kill_first = b;
    }

    /// Verbose-mode opening summary (kept hand / mulligans / library size).
    pub fn print_opening(&self) {
        let mtag = if self.mulligans > 0 {
            format!("({} mulligan(s) -> {}-card)", self.mulligans, self.hand.len())
        } else {
            "(kept 7)".to_string()
        };
        let seat = if self.on_the_draw { "on the draw" } else { "on the play" };
        println!("  OPENING : [{seat}] {} {}", mtag, self.hand.join(", "));
        if !self.bottomed.is_empty() {
            println!("  BOTTOM  : {}", self.bottomed.join(", "));
        }
        println!("  LIBRARY : {} cards", self.library.len());
    }

    // ── mulligan ──────────────────────────────────────────────────────────
    fn lands(&self, hand: &[String]) -> usize {
        hand.iter().filter(|c| self.reg.get(c).types.contains(&CardType::Land)).count()
    }

    fn keepable(&self, hand: &[String]) -> bool {
        let lands = self.lands(hand);
        let rocks = hand.iter().filter(|c| mana_rocks().contains(&c.as_str())).count();
        let mana = lands + rocks;
        if lands < mull_cfg().min_lands || lands > 4 {
            return false;
        }
        if !(2..=5).contains(&mana) {
            return false;
        }
        hand.iter().any(|c| is_action(c) || wishlist::is_payoff(c))
    }

    /// Keep decision, mulligan-aware. With fast_mull on, the FIRST hand (mulls==0) must also be
    /// EXPLOSIVE (an accelerant/engine/combo/tutor); a slow-but-functional hand is mulliganed once
    /// to seek tempo. Subsequent mulls relax back to plain keepable (never spiral on card loss).
    fn keepable_at(&self, hand: &[String], mulls: usize) -> bool {
        if !self.keepable(hand) {
            return false;
        }
        if mulls == 0 {
            match mull_cfg().gate {
                MullGate::Fast => return hand.iter().any(|c| FAST_KEEP.contains(&c.as_str())),
                MullGate::Mana => return hand.iter().any(|c| FAST_MANA_KEEP.contains(&c.as_str())),
                MullGate::None => {}
            }
        }
        true
    }

    fn choose_bottom(&self, hand: &[String], m: usize) -> Vec<String> {
        if m == 0 {
            return Vec::new();
        }
        let snap = GameState { hand: hand.to_vec(), ..Default::default() };
        const KEEP_SOURCES: i64 = 3;
        let mut prio: Vec<f64> = Vec::new();
        let mut src_seen = 0i64;
        for c in hand {
            let is_source = self.reg.get(c).types.contains(&CardType::Land) || mana_rocks().contains(&c.as_str());
            if is_source {
                src_seen += 1;
                prio.push(if src_seen <= KEEP_SOURCES { 1e6 } else { -1.0 });
            } else {
                prio.push(wishlist::card_value(&snap, self.reg, c, false) + 1.0);
            }
        }
        let mut order: Vec<usize> = (0..hand.len()).collect();
        order.sort_by(|&a, &b| prio[a].partial_cmp(&prio[b]).unwrap_or(std::cmp::Ordering::Equal));
        order.into_iter().take(m).map(|i| hand[i].clone()).collect()
    }

    fn london_mulligan(&mut self, deck: &[String], rng: &mut StdRng) {
        self.mulligans = 0;
        self.bottomed = Vec::new();
        let depth = mull_cfg().depth;
        for mulls in 0..=depth {
            let mut d = deck.to_vec();
            shuffle(&mut d, rng);
            let hand: Vec<String> = d[..7].to_vec();
            let lib: Vec<String> = d[7..].to_vec();
            let keep = (mulls == depth) || self.keepable_at(&hand, mulls);
            if keep {
                let bottom = self.choose_bottom(&hand, mulls);
                let mut h = hand.clone();
                let mut l = lib.clone();
                for c in &bottom {
                    if let Some(pos) = h.iter().position(|x| x == c) {
                        h.remove(pos);
                    }
                    l.push(c.clone());
                }
                self.mulligans = mulls;
                self.bottomed = bottom;
                self.hand = h;
                self.library = l;
                return;
            }
        }
    }

    // ── zone helpers ──────────────────────────────────────────────────────
    fn draw(&mut self) -> Option<String> {
        if self.library.is_empty() {
            return None;
        }
        let c = self.library.remove(0);
        self.hand.push(c.clone());
        if self.verbose {
            println!("  DRAW    : {c}");
        }
        Some(c)
    }

    fn board_names(&self) -> Vec<String> {
        self.board.iter().map(|(n, _)| n.clone()).collect()
    }

    /// Is the board permanent at `idx` a creature (resolving clones via copy-of)?
    fn is_creature_at(&self, idx: usize) -> bool {
        let (name, copy_of) = &self.board[idx];
        let eff = copy_of.as_deref().unwrap_or(name);
        self.reg.get(eff).is_creature()
    }

    /// Index of an untapped creature that could be tapped (excluding `exclude`).
    /// Springleaf Drum needs one to make mana.
    fn untapped_creature_idx(&self, exclude: usize) -> Option<usize> {
        (0..self.board.len()).find(|&i| {
            i != exclude && !self.tapped.contains(&i) && self.is_creature_at(i)
        })
    }

    /// The primary value engine that powered the win, read off the board at the kill. Falls back
    /// to "ritual/Jeska burst" when no permanent engine is out (mana came from spells, e.g. the
    /// Dualcaster combo fueled by Jeska's Will).
    pub fn engine_used(&self) -> String {
        let has = |n: &str| self.board.iter().any(|(b, cp)| b == n || cp.as_deref() == Some(n));
        if has("Storm-Kiln Artist") { "Storm-Kiln Artist".into() }
        else if has("Archmage Emeritus") { "Archmage Emeritus".into() }
        else if has("Tavern Scoundrel") { "Tavern Scoundrel".into() }
        else if has("Birgi, God of Storytelling") { "Birgi".into() }
        else if has("Vivi Ornitier") || has("Urabrask") { "Vivi/Urabrask (burn)".into() }
        else if has("Underworld Breach") { "Underworld Breach".into() }
        else { "ritual/Jeska burst (no perm engine)".into() }
    }

    /// A nonland/nonartifact card in hand to imprint (exile) for Chrome Mox; prefer the least
    /// valuable, never a payoff/combo piece unless that's all there is. None → can't play Chrome Mox.
    fn chrome_imprint_target(&self) -> Option<String> {
        let eligible: Vec<&String> = self
            .hand
            .iter()
            .filter(|c| {
                let cd = self.reg.get(c);
                !cd.is_land() && !cd.is_artifact()
            })
            .collect();
        eligible
            .iter()
            .find(|c| !NEVER_IMPRINT.contains(&c.as_str()))
            .or_else(|| eligible.first())
            .map(|c| (*c).clone())
    }

    /// Is a legendary creature in play? (Mox Amber needs one to make mana.) The deck's relevant
    /// legendary creatures are the commanders — Krark, Sakashima, and clones copying Krark.
    fn has_legend_in_play(&self) -> bool {
        self.board.iter().any(|(n, cp)| {
            n == "Krark, the Thumbless"
                || n == "Sakashima of a Thousand Faces"
                || cp.as_deref() == Some("Krark, the Thumbless")
        })
    }

    /// Does the mana source at `idx` actually produce mana right now? Mox Amber is dead unless a
    /// legendary creature is in play. (Chrome Mox is gated at cast time — only deployed when it can
    /// imprint — so any Chrome Mox on the board is already a live source.)
    fn source_active(&self, idx: usize) -> bool {
        match self.board[idx].0.as_str() {
            "Mox Amber" => self.has_legend_in_play(),
            _ => true,
        }
    }

    fn untapped_sources(&self) -> Vec<(usize, String)> {
        self.board
            .iter()
            .enumerate()
            .filter(|(i, (n, _))| {
                !self.tapped.contains(i)
                    && !self.sacrificed.contains(i) // one-shot sac sources are gone after use
                    && is_mana_source(n)
                    // Lion's Eye Diamond ({T}, Sac, DISCARD YOUR HAND: add 3) is never a free setup
                    // source — using it nukes the hand you still need. Its only legitimate use is the
                    // Underworld Breach combo, modeled separately in loops.rs. Keep it out of the
                    // turn-by-turn casting tap-path.
                    && !matches!(mana_source(n), Some((SrcMode::SacHand, _)))
            })
            .filter(|(i, _)| self.source_active(*i))
            .filter(|(i, (n, _))| {
                // Springleaf Drum requires tapping an untapped creature; it makes
                // no mana if none is available.
                if let Some((SrcMode::TapCreature, _)) = mana_source(n) {
                    self.untapped_creature_idx(*i).is_some()
                } else {
                    true
                }
            })
            .map(|(i, (n, _))| (i, n.clone()))
            .collect()
    }

    fn tap_source(&mut self, idx: usize, pool: &mut ManaPool) {
        if !self.source_active(idx) {
            return; // e.g. Mox Amber with no legend in play → produces nothing, stays untapped
        }
        let name = self.board[idx].0.clone();
        if let Some((mode, produced)) = mana_source(&name) {
            if mode == SrcMode::TapCreature {
                // Tapping an untapped creature is part of the cost. Gated by
                // untapped_sources(), so a creature is available here.
                match self.untapped_creature_idx(idx) {
                    Some(cidx) => {
                        self.tapped.insert(cidx);
                    }
                    None => return, // no creature → produces nothing, stays untapped
                }
            }
            pool.add_cost(&produced);
            self.our_life -= crate::tables::life_per_tap(&name);
            if mode == SrcMode::Sac {
                // one-shot: sacrifice after producing mana (e.g. Lotus Petal) — it can't be tapped
                // again and won't untap next turn.
                self.sacrificed.insert(idx);
                if self.verbose {
                    println!("  SAC     : {name} sacrificed for mana");
                }
            }
        }
        self.tapped.insert(idx);
    }

    fn try_afford(&self, cost: &HashMap<String, i64>, pool: &ManaPool) -> bool {
        let mut temp = *pool;
        if temp.can_pay(cost) {
            return true;
        }
        for (idx, _) in self.untapped_sources() {
            if let Some((_, produced)) = mana_source(&self.board[idx].0) {
                temp.add_cost(&produced);
            }
            if temp.can_pay(cost) {
                return true;
            }
        }
        false
    }

    fn pay(&mut self, cost: &HashMap<String, i64>, pool: &mut ManaPool) {
        while !pool.can_pay(cost) {
            let srcs = self.untapped_sources();
            if srcs.is_empty() {
                panic!("Tried to pay {:?} but ran out of mana", cost);
            }
            self.tap_source(srcs[0].0, pool);
        }
        pool.pay(cost);
    }

    // ── casting ───────────────────────────────────────────────────────────
    fn cast_cost(&self, card: &str) -> HashMap<String, i64> {
        let mut cost = self.reg.get(card).cost.clone();
        if let Some(t) = self.cmd_tax.get(card) {
            *cost.entry("generic".to_string()).or_insert(0) += 2 * t;
        }
        if self.board_names().iter().any(|n| n == "Baral, Chief of Compliance")
            && self.reg.get(card).is_instant_or_sorcery()
        {
            let g = cost.get("generic").copied().unwrap_or(0);
            cost.insert("generic".to_string(), (g - 1).max(0));
        }
        cost
    }

    fn copy_target(&self, card: &str) -> Option<String> {
        if !CLONES.contains(&card) {
            return None;
        }
        let krark_out = self
            .board
            .iter()
            .any(|(n, cp)| n == "Krark, the Thumbless" || cp.as_deref() == Some("Krark, the Thumbless"));
        if !krark_out {
            return None;
        }
        if card == "Sakashima of a Thousand Faces" {
            return Some("Krark, the Thumbless".to_string());
        }
        let sak_out = self.board.iter().any(|(n, _)| n == "Sakashima of a Thousand Faces");
        if sak_out {
            Some("Krark, the Thumbless".to_string())
        } else {
            None
        }
    }

    fn try_cast(&mut self, card: &str, pool: &mut ManaPool, from_command: bool) -> bool {
        if CLONES.contains(&card) && self.copy_target(card).is_none() {
            return false;
        }
        // Chrome Mox: imprint exiles a nonland/nonartifact card from hand. No imprint target →
        // it makes no mana, so don't play it (modeled as not castable). Charged once it's down.
        let chrome_imprint = if card == "Chrome Mox" {
            match self.chrome_imprint_target() {
                Some(t) => Some(t),
                None => return false,
            }
        } else {
            None
        };
        // Phyrexian pips: pay 2 life each by default, or route to colored mana when life is low.
        let (extra_mana, life_pay) =
            crate::game_state::plan_phyrexian(&self.reg.get(card).phyrexian, self.our_life);
        let mut cost = self.cast_cost(card);
        for (k, v) in &extra_mana {
            *cost.entry(k.clone()).or_insert(0) += v;
        }
        if !self.try_afford(&cost, pool) {
            return false;
        }
        let mut discard_land: Option<String> = None;
        if DISCARD_LAND_ON_PLAY.contains(&card) {
            discard_land = self.hand.iter().find(|c| c.as_str() != card && is_land_name(c)).cloned();
            if discard_land.is_none() {
                return false;
            }
        }
        let pool_floating = *pool;
        // Auto-tap sources up front (same logic as `pay`) so the verbose line can show the
        // mana actually AVAILABLE for this cast, then the leftover after paying.
        while !pool.can_pay(&cost) {
            let srcs = self.untapped_sources();
            if srcs.is_empty() {
                panic!("Tried to pay {:?} but ran out of mana", cost);
            }
            self.tap_source(srcs[0].0, pool);
        }
        let pool_avail = *pool;
        pool.pay(&cost);
        self.our_life -= life_pay;
        if self.verbose {
            let cp = self.copy_target(card);
            let zone = if from_command { "command" } else { "hand" };
            let cp_note = match &cp {
                Some(t) => format!("  [copy-of {t}]"),
                None => String::new(),
            };
            let life_note = if life_pay > 0 {
                format!("  [-{life_pay} life -> {}]", self.our_life)
            } else {
                String::new()
            };
            // floating-before -> available-after-tapping -> leftover-after-paying
            println!(
                "  CAST    : {card} (from {zone}) cost {} | float {} +tap-> avail {} -pay-> {}{}{}",
                fmt_cost(&cost),
                fmt_pool(&pool_floating),
                fmt_pool(&pool_avail),
                fmt_pool(pool),
                life_note,
                cp_note
            );
        }
        if let Some(dl) = discard_land {
            if let Some(pos) = self.hand.iter().position(|c| *c == dl) {
                self.hand.remove(pos);
            }
            if self.verbose {
                println!("  PITCH   : {card} discards {dl} (kept on battlefield)");
            }
            self.graveyard.push(dl);
        }
        if from_command {
            if let Some(pos) = self.command_zone.iter().position(|c| c == card) {
                self.command_zone.remove(pos);
            }
            *self.cmd_tax.entry(card.to_string()).or_insert(0) += 1;
        } else {
            if let Some(pos) = self.hand.iter().position(|c| c == card) {
                self.hand.remove(pos);
            }
            if self.cmd_tax.contains_key(card) {
                *self.cmd_tax.get_mut(card).unwrap() += 1;
            }
        }
        if let Some(t) = &chrome_imprint {
            if let Some(pos) = self.hand.iter().position(|c| c == t) {
                self.hand.remove(pos);
                if self.verbose {
                    println!("  IMPRINT : Chrome Mox exiles {t} (imprint cost)");
                }
            }
        }
        if SAC_ON_PLAY.contains(&card) {
            if let Some((_, produced)) = mana_source(card) {
                pool.add_cost(&produced);
            }
        } else {
            let cp = self.copy_target(card);
            let new_idx = self.board.len();
            self.board.push((card.to_string(), cp.clone()));
            if is_mana_source(card) {
                self.tap_source(new_idx, pool);
            }
            if cp.is_none() && is_etb_tutor(card) {
                self.etb_tutor(card);
            }
        }
        true
    }

    fn etb_tutor(&mut self, card: &str) {
        let mut state = self.build_state(&ManaPool::new());
        let fetched = apply_etb(&mut state, self.reg, card);
        if let Some(ref t) = fetched {
            self.library = state.library.clone();
            self.hand = state.hand.clone();
            if self.verbose {
                println!("  TUTOR   : {card} fetched {t}");
            }
        }
    }

    // ── state builder ─────────────────────────────────────────────────────
    fn build_state(&self, pool: &ManaPool) -> GameState {
        let bf: Vec<Permanent> = self
            .board
            .iter()
            .enumerate()
            .filter(|(idx, _)| !self.sacrificed.contains(idx)) // sacrificed one-shots are gone
            .map(|(idx, (name, copy_of))| Permanent {
                name: name.clone(),
                copy_of: copy_of.clone(),
                tapped: self.tapped.contains(&idx),
                summoning_sick: false,
                is_token: false,
            })
            .collect();
        GameState {
            library: self.library.clone(),
            hand: self.hand.clone(),
            graveyard: self.graveyard.clone(),
            battlefield: bf,
            mana: *pool,
            storm_count: 0,
            is_my_turn: true,
            opponent_life: self.opponent_life.clone(),
            our_life: self.our_life,
            vivi_power: self.vivi_power,
            vivi_mana_used: self.vivi_mana_used,
            ..Default::default()
        }
    }

    fn play_land(&mut self, card: &str, pool: &mut ManaPool, from_hand: bool) {
        if from_hand {
            if let Some(pos) = self.hand.iter().position(|c| c == card) {
                self.hand.remove(pos);
            }
        }
        let new_idx = self.board.len();
        self.board.push((card.to_string(), None));
        self.tap_source(new_idx, pool);
        self.played_land = true;
    }

    fn candidates_perm(&self, skip: &HashSet<String>) -> Vec<(String, bool)> {
        // Once the Krark flip engine is online, stop deploying MANA ROCKS for development — that's
        // durdle; dig for the kill instead (user, 2026-06-22). Engine pieces (doublers, value
        // engines, The One Ring, tutors) aren't rocks, so they keep getting deployed.
        // Skip deploying more mana rocks only once the Krark engine is out AND we already hold at
        // least `rock_skip_cutoff` mana sources (lands+rocks) — below that, rocks still fuel the dig;
        // above it they're durdle, so dig instead. cutoff = i64::MAX disables (rocks always allowed).
        let krark_out = self.board.iter().any(|(n, cp)| {
            n == "Krark, the Thumbless" || cp.as_deref() == Some("Krark, the Thumbless")
        });
        let mana_srcs = self.board.iter().filter(|(n, _)| is_mana_source(n)).count() as i64;
        let skip_rocks = krark_out && mana_srcs >= self.rock_skip_cutoff;
        let mut cands: Vec<(String, bool)> = Vec::new();
        for c in &self.hand {
            if play_priority(c).is_some()
                && !skip.contains(c)
                && !(skip_rocks && mana_rocks().contains(&c.as_str()))
            {
                cands.push((c.clone(), false));
            }
        }
        for c in &self.command_zone {
            if play_priority(c).is_some() && !skip.contains(c) {
                cands.push((c.clone(), true));
            }
        }
        cands.sort_by_key(|t| play_priority(&t.0).unwrap());
        cands
    }

    fn cast_permanents(&mut self, pool: &mut ManaPool, skip: &HashSet<String>) -> bool {
        let mut cast_any = false;
        let mut progress = true;
        while progress {
            progress = false;
            for (card, from_command) in self.candidates_perm(skip) {
                if self.try_cast(&card, pool, from_command) {
                    cast_any = true;
                    progress = true;
                    break;
                }
            }
        }
        cast_any
    }

    fn has_stuck_permanent(&self, pool: &ManaPool) -> bool {
        for c in &self.hand {
            if play_priority(c).is_some()
                && !(CLONES.contains(&c.as_str()) && self.copy_target(c).is_none())
                && !self.try_afford(&self.cast_cost(c), pool)
            {
                return true;
            }
        }
        for c in &self.command_zone {
            if play_priority(c).is_some() && !self.try_afford(&self.cast_cost(c), pool) {
                return true;
            }
        }
        false
    }

    fn ramp(&mut self, pool: &mut ManaPool) {
        // default mode "sink"
        for _ in 0..6 {
            let ramp = match self.hand.iter().find(|c| RAMP_SPELLS.contains(&c.as_str())).cloned() {
                Some(r) => r,
                None => break,
            };
            let probe = self.build_state(pool);
            if probe.flips_per_cast(self.reg) < 1 || loops::develop_score(&probe, self.reg, &ramp) <= 0.0 {
                break;
            }
            if !self.try_afford(&probe.cast_cost(self.reg, &ramp), pool) {
                break;
            }
            // sink gate: stuck permanent OR develop_candidates non-empty
            if !(self.has_stuck_permanent(pool) || !loops::develop_candidates(&probe, self.reg).is_empty()) {
                break;
            }
            // tap everything
            for (idx, _) in self.untapped_sources() {
                self.tap_source(idx, pool);
            }
            let state = self.build_state(pool);
            let cast = loops::do_cast(&state, self.reg, &ramp, "hand", &mut self.dev_rng, DEV_PAYOFFS);
            let (ns, _log) = match cast {
                Some(x) => x,
                None => break,
            };
            self.hand = ns.hand.clone();
            self.library = ns.library.clone();
            self.graveyard = ns.graveyard.clone();
            *pool = ns.mana;
            self.opponent_life = ns.opponent_life.clone();
            // Jeska mode 2 gas
            if !ns.exiled_play.is_empty() {
                let gas = ns.exiled_play.clone();
                if self.verbose {
                    println!("  EXILE   : play-this-turn {}", gas.join(", "));
                }
                for c in gas {
                    if is_land_name(&c) {
                        if !self.played_land {
                            self.play_land(&c, pool, false);
                        }
                        continue;
                    }
                    self.hand.push(c.clone());
                    *self.exile_gas.entry(c).or_insert(0) += 1;
                }
            }
        }
    }

    fn zndrsplt_combat_draw(&mut self) {
        let st = self.build_state(&ManaPool::new());
        let copies = st
            .battlefield
            .iter()
            .filter(|p| p.effective_name() == "Zndrsplt, Eye of Wisdom")
            .count() as i64;
        if copies == 0 {
            return;
        }
        let p = st.flip_p();
        let mut wins = 0i64;
        for _ in 0..copies {
            while self.dev_rng.gen::<f64>() < p {
                wins += 1;
            }
        }
        let n = (wins * copies).min((self.library.len() as i64 - 1).max(0));
        for _ in 0..n {
            self.draw();
        }
    }

    /// Faithful signature of the win-relevant game state (#3 memo key). A cache hit means the
    /// state is identical for detection purposes, so a previously-proven no-win can be skipped.
    fn win_sig(&self, st: &GameState) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let mut bf: Vec<(&str, bool)> =
            st.battlefield.iter().map(|p| (p.effective_name(), p.tapped)).collect();
        bf.sort_unstable();
        bf.hash(&mut h);
        st.hand.hash(&mut h); // order doesn't matter for detection, but cheap & faithful
        st.graveyard.hash(&mut h);
        st.library.hash(&mut h); // contents+order: rollouts dig from the top
        st.exiled_play.hash(&mut h);
        st.mana.slots.hash(&mut h);
        st.mana.treasures.hash(&mut h);
        st.opponent_life.hash(&mut h);
        st.opponent_library.hash(&mut h);
        st.storm_count.hash(&mut h);
        h.finish()
    }

    fn try_win(&mut self, pool: &ManaPool, det: &DeterministicKillSearch, prob: &mut ProbabilisticPlanner) -> (Option<Line>, Line, GameState) {
        let state = self.build_state(pool);
        // #3: skip a state already proven no-win this turn (identical signature).
        let sig = self.win_sig(&state);
        if self.scan_nowin.contains(&sig) {
            return (None, Line::default(), state);
        }
        // #1: cheap MC scan first; only escalate to the full-fidelity solve when promising.
        let mut scan = ProbabilisticPlanner {
            mc_sims: SCAN_SIMS,
            max_first: prob.max_first,
            rollout_steps: prob.rollout_steps,
            max_depth: prob.max_depth,
            decision_threshold: Some(self.win_threshold),
        };
        let cheap = solve(&state, self.reg, DEV_PAYOFFS, det, &mut scan, &mut self.dev_rng);
        // Escalate to the full solve for any line that could be COMMITTABLE (cheap.p_win >= the
        // gate we'd use), so an aggressive send_gate below SCAN_FLOOR still gets an accurate number.
        let esc_floor = SCAN_FLOOR.min(self.commit_gate());
        let line = if cheap.p_win >= 1.0 || cheap.p_win < esc_floor {
            cheap // deterministic kill (exact) OR confidently no win — no full pass needed
        } else {
            // promising probabilistic board -> re-solve at full fidelity for an accurate decision
            prob.decision_threshold = Some(self.win_threshold);
            solve(&state, self.reg, DEV_PAYOFFS, det, prob, &mut self.dev_rng)
        };
        let decl = self.declare(&line, &state);
        if decl.is_none() {
            self.scan_nowin.insert(sig);
        }
        (decl, line, state)
    }

    /// Would a committed go-off that fizzles this turn cost us the game? (No second chance.)
    /// Conditions match the fizzle-fatal model: Pact of Negation in hand (owes {3}{U}{U} next
    /// upkeep), or a one-shot Grapeshot with no repeatable burn engine (Urabrask/Vivi) to keep
    /// chipping. Deck-out is already a loss via the brick path.
    fn fizzle_is_fatal(&self) -> bool {
        let burn_engine = self.board.iter().any(|(n, _)| n == "Urabrask" || n == "Vivi Ornitier");
        let grapeshot = self.hand.iter().any(|c| c == "Grapeshot");
        let pact = self.hand.iter().any(|c| c == "Pact of Negation");
        pact || (grapeshot && !burn_engine)
    }

    /// p_win required to COMMIT to a go-off. If a fizzle isn't fatal, trying is ~free (the attempt
    /// runs on a clone; on failure the real game just keeps developing), so we send aggressively at
    /// `send_gate`. If a fizzle would be fatal, demand the full `win_threshold`.
    fn commit_gate(&self) -> f64 {
        if self.fizzle_is_fatal() {
            self.win_threshold
        } else {
            self.send_gate
        }
    }

    fn declare(&mut self, line: &Line, state: &GameState) -> Option<Line> {
        if line.p_win >= 1.0 {
            return Some(line.clone());
        }
        if line.kind == "probabilistic" && line.p_win >= self.commit_gate() {
            if let (Some(base), Some(first)) = (&line.base, &line.first) {
                let proven = loops::prove_go_off(
                    base,
                    self.reg,
                    (&first.0, &first.1),
                    line.loop_line,
                    &mut self.dev_rng,
                    DEV_PAYOFFS,
                    40,
                    80,
                    self.verbose, // diag: trace each turn's actual go-off attempt (fizzle or win)
                );
                if proven {
                    // proven -> accurate number via full solve (no threshold early-stop)
                    let det = DeterministicKillSearch::default();
                    let mut full = ProbabilisticPlanner::default();
                    full.decision_threshold = None;
                    return Some(solve(state, self.reg, DEV_PAYOFFS, &det, &mut full, &mut self.dev_rng));
                }
                // Cleared the threshold so we COMMITTED to the go-off, but the real flips
                // fizzled. Whether that kills you (no second chance) is CONDITIONAL (user's rules):
                //   * deck-out (empty library, no payoff) is already a loss via the brick path.
                //   * Pact of Negation: the sim never casts it (no legal solitaire target) -> n/a.
                //   * insufficient BURN: fatal ONLY if you can't keep burning next turn — i.e. a
                //     one-shot Grapeshot attempt with NO repeatable burn engine (Urabrask/Vivi) on
                //     board. If Urabrask/Vivi is out, you keep chipping next turn -> survive.
                if self.fizzle_fatal && self.fizzle_is_fatal() {
                    self.dead = true;
                }
            }
        }
        None
    }

    fn snapcaster_flashback(&mut self, pool: &mut ManaPool) -> bool {
        if !self.hand.iter().any(|c| c == "Snapcaster Mage") {
            return false;
        }
        let mut st = tap_out(&self.build_state(pool));
        let sc_cost = st.cast_cost(self.reg, "Snapcaster Mage");
        if !st.mana.can_pay(&sc_cost) {
            return false;
        }
        st.mana.pay(&sc_cost);
        if let Some(pos) = st.hand.iter().position(|c| c == "Snapcaster Mage") {
            st.hand.remove(pos);
        }
        st.battlefield.push(Permanent { summoning_sick: false, ..Permanent::new("Snapcaster Mage") });
        // best flashback target
        let mut seen: HashSet<&str> = HashSet::new();
        let mut targets: Vec<String> = Vec::new();
        for c in &st.graveyard {
            if !seen.insert(c.as_str()) {
                continue;
            }
            if self.reg.get(c).is_instant_or_sorcery()
                && st.mana.can_pay(&st.cast_cost(self.reg, c))
                && loops::develop_score(&st, self.reg, c) > 0.0
            {
                targets.push(c.clone());
            }
        }
        if targets.is_empty() {
            return false;
        }
        let target = targets
            .iter()
            .max_by(|a, b| {
                loops::develop_score(&st, self.reg, a)
                    .partial_cmp(&loops::develop_score(&st, self.reg, b))
                    .unwrap()
            })
            .unwrap()
            .clone();
        if let Some(pos) = st.graveyard.iter().position(|c| *c == target) {
            st.graveyard.remove(pos);
        }
        st.hand.push(target.clone());
        let cast = loops::do_cast(&st, self.reg, &target, "hand", &mut self.dev_rng, DEV_PAYOFFS);
        let (mut ns, _) = match cast {
            Some(x) => x,
            None => return false,
        };
        // flashback exiles instead of hand/GY
        if let Some(pos) = ns.hand.iter().position(|c| *c == target) {
            ns.hand.remove(pos);
        } else if let Some(pos) = ns.graveyard.iter().position(|c| *c == target) {
            ns.graveyard.remove(pos);
        }
        self.board.push(("Snapcaster Mage".to_string(), None));
        self.hand = ns.hand.clone();
        self.library = ns.library.clone();
        self.graveyard = ns.graveyard.clone();
        pool.treasures = ns.mana.treasures;
        self.opponent_life = ns.opponent_life.clone();
        true
    }

    fn cleanup_exile_gas(&mut self) {
        if self.exile_gas.is_empty() {
            return;
        }
        let gas = std::mem::take(&mut self.exile_gas);
        for (name, cnt) in gas {
            for _ in 0..cnt {
                if let Some(pos) = self.hand.iter().position(|c| *c == name) {
                    self.hand.remove(pos);
                }
            }
        }
        self.exile_gas = HashMap::new();
    }

    fn discard_to_hand_size(&mut self, pool: &ManaPool) {
        if self.hand.len() <= MAX_HAND_SIZE {
            return;
        }
        let excess = self.hand.len() - MAX_HAND_SIZE;
        let state = self.build_state(pool);
        let mut order: Vec<String> = self.hand.clone();
        order.sort_by(|a, b| {
            discard_rank(&state, self.reg, a)
                .partial_cmp(&discard_rank(&state, self.reg, b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let drop: Vec<String> = order.into_iter().take(excess).collect();
        if self.verbose && !drop.is_empty() {
            println!("  DISCARD : {} (end-of-turn to hand size)", drop.join(", "));
        }
        for c in &drop {
            if let Some(pos) = self.hand.iter().position(|x| x == c) {
                self.hand.remove(pos);
            }
            self.graveyard.push(c.clone());
        }
    }

    // ── develop ───────────────────────────────────────────────────────────
    fn develop(&mut self, pool: &mut ManaPool) -> Vec<String> {
        let payoffs = DEV_PAYOFFS;
        let mut state = tap_out(&self.build_state(pool));
        let orig_bf = state.battlefield.len();
        let need_life: i64 = state.opponent_life.iter().copied().filter(|l| *l > 0).sum();
        let mut scores: HashMap<String, f64> = HashMap::new();

        let recompute = |state: &GameState, scores: &mut HashMap<String, f64>, reg: &Registry| {
            scores.clear();
            let mut set: HashSet<String> = HashSet::new();
            for c in state.hand.iter().chain(state.graveyard.iter()) {
                set.insert(c.clone());
            }
            for c in set {
                if reg.get(&c).is_instant_or_sorcery() && !["Grapeshot", "Thassa's Oracle"].contains(&c.as_str()) {
                    let sc = loops::develop_score(state, reg, &c);
                    scores.insert(c, sc);
                }
            }
        };
        recompute(&state, &mut scores, self.reg);

        let mut cast: Vec<String> = Vec::new();
        let mut dead: HashSet<String> = HashSet::new();
        // Keep developing as long as casts make CARRYOVER progress (cards drawn, opponent damaged/
        // milled, graveyard fuel) — a mana-neutral/positive loop that draws or burns can be looped as
        // far as the kill needs. A cast that makes no carryover progress (only ephemeral storm count /
        // floating mana, which reset next turn) marks that card `dead` and drops it; when no scoring
        // candidate makes progress, `cands` empties and we break. 60 is just a termination backstop —
        // the real stop is the no-progress check + the library floor (deck-out guard).
        for _ in 0..dev_cap() {
            if state.hand.iter().any(|nm| is_engine_permanent(self.reg, nm)) {
                if !deploy_engine_perms(&mut state, self.reg).is_empty() {
                    recompute(&state, &mut scores, self.reg);
                }
            }
            let oracle_acc = state.hand.iter().any(|c| c == "Thassa's Oracle")
                || state.has_permanent("Thassa's Oracle")
                || loops::can_escape(&state, self.reg, "Thassa's Oracle");
            let dev = state.blue_devotion(self.reg);
            // Raw treasures can fuel the dig-out with no engine permanent: with F flips/cast each
            // cantrip draws ~F/2 cards, so ~2*(lib-dev)/F mana empties the library to devotion
            // (+2 for Oracle's {U}{U}). Gated on Oracle actually IN HAND so we never deck out
            // without the payoff secured. Rescues "Oracle + big mana, no engine perm" stalls.
            let flips = state.flips_per_cast(self.reg).max(1);
            let lib = state.library.len() as i64;
            let treasure_dig_out = state.hand.iter().any(|c| c == "Thassa's Oracle")
                && state.mana.treasures >= 2 * (lib - dev).max(0) / flips + 2;
            let krark = state.krark_bodies(self.reg) >= 1;
            let engine_perm = ["Storm-Kiln Artist", "Birgi, God of Storytelling", "Urabrask", "Tavern Scoundrel"]
                .iter()
                .any(|n| state.has_permanent(n));
            // Oracle closes by drawing the library down to devotion, so raw treasures (which fund the
            // dig) suffice. Grapeshot (burn) and Brain Freeze (mill) close on STORM COUNT, not library
            // size, so digging past the floor only pays off with a real storm engine that keeps the
            // count climbing — raw treasures can't reach the ~160-storm burn. Gating the burn/mill
            // close on an engine permanent keeps it from decking us out chasing an unreachable kill.
            let oracle_close = oracle_acc && krark && (engine_perm || treasure_dig_out);
            let burn_acc = state.hand.iter().any(|c| c == "Grapeshot")
                || loops::can_escape(&state, self.reg, "Grapeshot");
            let mill_acc = state.hand.iter().any(|c| c == "Brain Freeze")
                || loops::can_escape(&state, self.reg, "Brain Freeze");
            let burn_close = (burn_acc || mill_acc) && krark && engine_perm;
            let closing = oracle_close || burn_close;
            let floor = 8.max(dev + 4);
            if !closing && (state.library.len() as i64) <= floor {
                break;
            }
            let cands: Vec<(String, String)> = loops::develop_candidates(&state, self.reg)
                .into_iter()
                .filter(|(c, _)| {
                    let sc = scores.get(c).copied().unwrap_or(0.0);
                    // Magecraft fuel (returning counters / Cyclonic Rift) sustains the loop even
                    // when it merely breaks even on mana, so admit a 0 score for those.
                    let score_ok = if loops::is_magecraft_fuel(c) { sc >= 0.0 } else { sc > 0.0 };
                    score_ok
                        && !dead.contains(c)
                        && (closing || (state.library.len() as i64 - loops::max_draws(&state, self.reg, c) as i64) >= floor)
                        && (c != "Jeska's Will" || closing)
                })
                .collect();
            if cands.is_empty() {
                break;
            }
            let mut cands = cands;
            cands.sort_by(|a, b| {
                let sa = scores.get(&a.0).copied().unwrap_or(0.0);
                let sb = scores.get(&b.0).copied().unwrap_or(0.0);
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            });
            // Carryover progress: cards drawn/milled (library), graveyard fuel, opponent damage, and
            // opponent mill. Deliberately EXCLUDES storm count, floating mana, and treasures — those
            // either reset next turn or are mere fuel, so a loop that only spins them isn't "progress"
            // and gets dropped (prevents e.g. an infinite no-draw treasure loop).
            let sig = |g: &GameState| {
                (
                    g.library.len(),
                    g.graveyard.len(),
                    g.opponent_life.iter().sum::<i64>(),
                    g.opponent_library.iter().sum::<i64>(),
                )
            };
            let before = sig(&state);
            let mut nxt: Option<(GameState, ResolveLog)> = None;
            let mut chosen = String::new();
            for (card, source) in &cands {
                if let Some(r) = loops::do_cast(&state, self.reg, card, source, &mut self.dev_rng, payoffs) {
                    nxt = Some(r);
                    chosen = card.clone();
                    break;
                }
            }
            let (mut nstate, log) = match nxt {
                Some(x) => x,
                None => break,
            };
            cast.push(chosen.clone());
            if self.verbose {
                println!("{}", loops::trace_cast_line("DEV ", cast.len() as i64, &chosen, &log, &nstate));
            }
            // Jeska mode 2 gas
            if !nstate.exiled_play.is_empty() {
                let gas = nstate.exiled_play.clone();
                nstate.exiled_play.clear();
                if self.verbose {
                    println!("  EXILE   : play-this-turn {}", gas.join(", "));
                }
                for c in gas {
                    if is_land_name(&c) {
                        if !self.played_land {
                            nstate.battlefield.push(Permanent { summoning_sick: false, ..Permanent::new(&c) });
                            if let Some((_, produced)) = mana_source(&c) {
                                nstate.mana.add_cost(&produced);
                            }
                            self.played_land = true;
                        }
                        continue;
                    }
                    nstate.hand.push(c.clone());
                    *self.exile_gas.entry(c).or_insert(0) += 1;
                }
                state = nstate;
                recompute(&state, &mut scores, self.reg);
            } else {
                state = nstate;
            }
            if loops::winning_payoff(&state, self.reg, payoffs, need_life).is_some() {
                break;
            }
            if sig(&state) == before {
                dead.insert(chosen);
            }
        }
        for p in &state.battlefield[orig_bf..] {
            self.board.push((p.name.clone(), p.copy_of.clone()));
        }
        self.hand = state.hand.clone();
        self.library = state.library.clone();
        self.graveyard = state.graveyard.clone();
        pool.treasures = state.mana.treasures;
        self.opponent_life = state.opponent_life.clone();
        self.vivi_power = state.vivi_power; // counters earned developing persist across turns
        self.vivi_mana_used = state.vivi_mana_used; // honor once-per-turn into the go-off
        cast
    }

    // ── one turn ──────────────────────────────────────────────────────────
    pub fn play_turn(&mut self, det: &DeterministicKillSearch, prob: &mut ProbabilisticPlanner) -> Option<Line> {
        self.turn += 1;
        // Mana Vault doesn't untap during the untap step (only by paying {4}, which this burst-combo
        // deck never bothers with), so it stays tapped once used. Untap everything else.
        self.tapped.retain(|&i| self.board.get(i).map(|(n, _)| n == "Mana Vault").unwrap_or(false));
        // Mana Vault deals 1 damage to you each upkeep while it's tapped.
        let mv_tapped = self
            .tapped
            .iter()
            .filter(|&&i| self.board.get(i).map(|(n, _)| n == "Mana Vault").unwrap_or(false))
            .count() as i64;
        self.our_life -= mv_tapped;
        self.scan_nowin.clear(); // #3 memo is per-turn
        self.exile_gas = HashMap::new();
        self.played_land = false;
        self.vivi_mana_used = false; // Vivi's {0} ability recharges each turn
        if self.verbose {
            println!("\n=== TURN {} ===", self.turn);
        }
        let mut last_pwin = 0.0f64; // best solver p_win seen this turn (for the verbose CHECK)

        if self.turn > 1 || self.on_the_draw {
            self.draw();
        }

        let mut pool = ManaPool { slots: [0; 7], treasures: self.treasures };

        // Ragavan, Nimble Pilferer: vs inert opponents it connects in combat every turn it's been in
        // play, making one Treasure. We bank it at turn start from the PERSISTENT board, which models
        // both the combat timing (the Treasure from last turn's swing is available now) and summoning
        // sickness (a Ragavan deployed THIS turn isn't on the board yet here). The impulse-exiled card
        // is ignored. NOTE: a turn that WINS via combat can't also bank this Treasure, but combat wins
        // don't need the mana, so the value is the ramp accumulated across prior turns.
        let ragavan = self
            .board_names()
            .iter()
            .filter(|n| n.as_str() == "Ragavan, Nimble Pilferer")
            .count() as i64;
        if ragavan > 0 {
            pool.treasures += ragavan;
            if self.verbose {
                println!("  RAGAVAN : +{ragavan} Treasure (combat)");
            }
        }

        // play first land
        if let Some(card) = self.hand.iter().find(|c| is_land_name(c)).cloned() {
            self.play_land(&card, &mut pool, true);
            if self.verbose {
                println!("  LAND    : {card}");
            }
        }

        // Early kill-check (A/B flag): look for a win BEFORE spending the turn ramping/casting.
        if self.check_kill_first {
            let (win, line, state) = self.try_win(&pool, det, prob);
            last_pwin = line.p_win;
            if let Some(w) = win {
                self.goff_base = Some(state);
                return Some(w);
            }
        }

        self.ramp(&mut pool);

        // free draws: The One Ring
        if self.board_names().iter().any(|n| n == "The One Ring") {
            self.one_ring += 1;
            let mut drawn = 0i64;
            while drawn < self.one_ring && self.library.len() > 6 {
                self.draw();
                drawn += 1;
            }
        }
        // Rhystic Study: conservative baseline of ~1 card per turn cycle (the "tax" most opponents
        // won't pay), modeled as a second upkeep draw each turn it's in play.
        if self.board_names().iter().any(|n| n == "Rhystic Study") && self.library.len() > 6 {
            self.draw();
        }
        // Mystic Remora: one delayed draw (a single opponent trigger), then sacrificed — the
        // cumulative upkeep isn't paid. Fires the turn AFTER it lands, then it's gone.
        for idx in 0..self.board.len() {
            if self.board[idx].0 == "Mystic Remora" && !self.sacrificed.contains(&idx) {
                if self.library.len() > 6 {
                    self.draw();
                }
                self.sacrificed.insert(idx);
                if self.verbose {
                    println!("  REMORA  : Mystic Remora draws 1, then sacrificed (upkeep unpaid)");
                }
            }
        }
        self.zndrsplt_combat_draw();

        let (win, line, state) = self.try_win(&pool, det, prob);
        last_pwin = line.p_win;
        if let Some(w) = win {
            self.goff_base = Some(state);
            return Some(w);
        }

        let empty: HashSet<String> = HashSet::new();
        // Per-cast CAST lines (card/zone/cost/mana) are emitted inside `try_cast` when verbose.
        self.cast_permanents(&mut pool, &empty);

        let (win, line, state) = self.try_win(&pool, det, prob);
        last_pwin = line.p_win;
        if let Some(w) = win {
            self.goff_base = Some(state);
            return Some(w);
        }

        let hand_before: Vec<String> = if self.verbose { self.hand.clone() } else { Vec::new() };
        let gy_before: Vec<String> = if self.verbose { self.graveyard.clone() } else { Vec::new() };
        let developed = self.develop(&mut pool);
        if self.verbose {
            if !developed.is_empty() {
                println!("  DEVELOP : {}", developed.join(", "));
            }
            let dug: Vec<String> = self.hand.iter().filter(|c| !hand_before.contains(c)).cloned().collect();
            if !dug.is_empty() {
                println!("  DUG     : {}", dug.join(", "));
            }
            // Cards that hit the graveyard beyond the spells we actually cast — e.g. Gamble's
            // random discard. Multiset-subtract the cast list from the graveyard delta.
            let mut before_pool = gy_before.clone();
            let mut gy_delta: Vec<String> = Vec::new();
            for c in &self.graveyard {
                if let Some(pos) = before_pool.iter().position(|x| x == c) {
                    before_pool.remove(pos);
                } else {
                    gy_delta.push(c.clone());
                }
            }
            let mut cast_pool = developed.clone();
            let pitched: Vec<String> = gy_delta
                .into_iter()
                .filter(|c| match cast_pool.iter().position(|x| x == c) {
                    Some(pos) => {
                        cast_pool.remove(pos);
                        false
                    }
                    None => true,
                })
                .collect();
            if !pitched.is_empty() {
                println!("  DISCARD : {} (pitched during develop)", pitched.join(", "));
            }
        }

        if !developed.is_empty() {
            let (win, _line, state) = self.try_win(&pool, det, prob);
            if let Some(w) = win {
                self.goff_base = Some(state);
                return Some(w);
            }
        }

        let flashed = self.snapcaster_flashback(&mut pool);
        // Per-cast CAST lines for redeployed permanents are emitted inside `try_cast` when verbose.
        let deployed_more = self.cast_permanents(&mut pool, &empty);
        if flashed || deployed_more {
            let (win, _line, state) = self.try_win(&pool, det, prob);
            if let Some(w) = win {
                self.goff_base = Some(state);
                return Some(w);
            }
        }

        self.cleanup_exile_gas();
        self.treasures = pool.treasures;
        self.discard_to_hand_size(&pool);
        if self.verbose {
            let st = self.build_state(&pool);
            println!(
                "  CHECK   : bodies={} doublers={} flips/cast={} devotion={}  best p_win={:.3}",
                st.krark_bodies(self.reg),
                st.trigger_doublers(self.reg),
                st.flips_per_cast(self.reg),
                st.blue_devotion(self.reg),
                last_pwin
            );
            let breach = self.hand.iter().any(|c| c == "Underworld Breach")
                || self.board_names().iter().any(|n| n == "Underworld Breach");
            println!(
                "  GY({:>2})  : {}{}",
                self.graveyard.len(),
                self.graveyard.join(", "),
                if breach { "   [Breach in hand/play: escape needs card cost + exile 3 others]" } else { "" }
            );
        }
        None
    }

    /// End-of-game zone inspection for the diag mode: what win-enablers were SEEN
    /// (hand/graveyard/board) vs. still buried in the LIBRARY, plus zone sizes.
    pub fn print_zone_inspection(&self) {
        use std::collections::HashSet;
        let seen: HashSet<&str> = self
            .hand
            .iter()
            .chain(self.graveyard.iter())
            .map(|s| s.as_str())
            .chain(self.board.iter().map(|(n, _)| n.as_str()))
            .collect();
        let lib: HashSet<&str> = self.library.iter().map(|s| s.as_str()).collect();
        let cats: [(&str, &[&str]); 4] = [
            ("PAYOFFS", &["Thassa's Oracle", "Grapeshot", "Brain Freeze"]),
            ("COMBO", &["Twinflame", "Heat Shimmer", "Dualcaster Mage"]),
            ("MANA-ENG", &[
                "Storm-Kiln Artist", "Archmage Emeritus", "Birgi, God of Storytelling",
                "Jeska's Will", "Strike It Rich", "Rite of Flame", "Pyretic Ritual", "Desperate Ritual",
            ]),
            ("BURN", &["Urabrask", "Vivi Ornitier"]),
        ];
        println!(
            "\n  ZONES   : lib {} | gy {} | treasures {} | opp_life {}",
            self.library.len(),
            self.graveyard.len(),
            self.treasures,
            self.opponent_life.iter().sum::<i64>()
        );
        for (label, members) in cats {
            let s: Vec<&str> = members.iter().copied().filter(|m| seen.contains(m)).collect();
            let b: Vec<&str> = members.iter().copied().filter(|m| lib.contains(m)).collect();
            println!("  {label:8}: seen [{}]  | buried [{}]", s.join(", "), b.join(", "));
        }
        println!("  HAND    : {}", self.hand.join(", "));
        let engines: Vec<String> = self
            .board
            .iter()
            .filter(|(n, _)| !is_land_name(n))
            .map(|(n, _)| n.clone())
            .collect();
        println!("  BOARD   : {}", engines.join(", "));
    }
}

// Fisher-Yates shuffle (rand) — not byte-identical to Python's MT, fine for Track A.
fn shuffle(deck: &mut [String], rng: &mut StdRng) {
    let n = deck.len();
    for i in (1..n).rev() {
        let j = rng.gen_range(0..=i);
        deck.swap(i, j);
    }
}

// --------------------------------------------------------------------------- //
// Sweep harness
// --------------------------------------------------------------------------- //

pub struct GameResult {
    pub seed: u64,
    pub luck: u64,
    pub won: bool,
    pub turn: i64,
    pub wincon: String,
    pub engine: String,
}

/// Bucket a winning Line's detail into a win-condition category.
pub fn classify_wincon(detail: &str) -> &'static str {
    let d = detail;
    if d.contains("hasty attackers") || d.contains("Dualcaster") || d.contains("Twinflame") || d.contains("Heat Shimmer") {
        "combat (Dualcaster combo)"
    } else if d.contains("Oracle") {
        "Thassa's Oracle (deck-out)"
    } else if d.contains("Grapeshot") {
        "Grapeshot (storm burn)"
    } else if d.contains("Brain Freeze") || d.contains("mill") {
        "Brain Freeze (mill)"
    } else if d.contains("burn") {
        "burn (Urabrask/Vivi)"
    } else {
        "other"
    }
}

pub fn play_quiet_luck(
    reg: &Registry,
    deck: &[String],
    seed: u64,
    luck: u64,
    max_turns: i64,
    win_threshold: f64,
    fizzle_fatal: bool,
    send_gate: f64,
    fast_mull: bool,
    rock_cutoff: i64,
    check_first: bool,
) -> GameResult {
    let mut game = SimGame::new(reg, deck, seed, win_threshold, fast_mull);
    game.set_dev_seed(seed.wrapping_mul(1_000_003).wrapping_add(luck));
    game.fizzle_fatal = fizzle_fatal;
    game.set_send_gate(send_gate);
    game.set_rock_cutoff(rock_cutoff);
    game.set_check_first(check_first);
    let det = DeterministicKillSearch::default();
    let mut prob = ProbabilisticPlanner { mc_sims: 80, max_first: 2, rollout_steps: 20, ..Default::default() };
    let mut won = false;
    let mut detail = String::new();
    for _ in 0..max_turns {
        if let Some(line) = game.play_turn(&det, &mut prob) {
            won = true;
            detail = line.detail.clone();
            break;
        }
        if game.dead {
            break; // committed go-off fizzled -> dead, game over (loss)
        }
    }
    let (wincon, engine) = if won {
        (classify_wincon(&detail).to_string(), game.engine_used())
    } else {
        (String::new(), String::new())
    };
    GameResult { seed, luck, won, turn: if won { game.turn } else { 0 }, wincon, engine }
}
