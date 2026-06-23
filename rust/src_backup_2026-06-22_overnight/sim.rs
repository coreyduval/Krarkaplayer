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

const SAC_ON_PLAY: &[&str] = &["Lotus Petal"];
const DISCARD_LAND_ON_PLAY: &[&str] = &["Mox Diamond"];
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
        "Mystic Remora" => 2,
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
    dev_rng: StdRng,
    goff_base: Option<GameState>,

    hand: Vec<String>,
    library: Vec<String>,
    board: Vec<(String, Option<String>)>, // (name, copy_of)
    tapped: HashSet<usize>,
    turn: i64,
    command_zone: Vec<String>,
    cmd_tax: HashMap<String, i64>,
    one_ring: i64,
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
}

impl<'a> SimGame<'a> {
    pub fn new(reg: &'a Registry, deck: &[String], rng_seed: u64, win_threshold: f64) -> SimGame<'a> {
        let dev_rng = StdRng::seed_from_u64(rng_seed.wrapping_mul(7919).wrapping_add(1));
        let mut shuffle_rng = StdRng::seed_from_u64(rng_seed);
        let mut g = SimGame {
            reg,
            win_threshold,
            dev_rng,
            goff_base: None,
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
        };
        g.london_mulligan(deck, &mut shuffle_rng);
        g
    }

    pub fn set_dev_seed(&mut self, seed: u64) {
        self.dev_rng = StdRng::seed_from_u64(seed);
    }

    /// Verbose-mode opening summary (kept hand / mulligans / library size).
    pub fn print_opening(&self) {
        let mtag = if self.mulligans > 0 {
            format!("({} mulligan(s) -> {}-card)", self.mulligans, self.hand.len())
        } else {
            "(kept 7)".to_string()
        };
        println!("  OPENING : {} {}", mtag, self.hand.join(", "));
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
        if lands < 1 || lands > 4 {
            return false;
        }
        if !(2..=5).contains(&mana) {
            return false;
        }
        hand.iter().any(|c| is_action(c) || wishlist::is_payoff(c))
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
        for mulls in 0..3usize {
            let mut d = deck.to_vec();
            shuffle(&mut d, rng);
            let hand: Vec<String> = d[..7].to_vec();
            let lib: Vec<String> = d[7..].to_vec();
            let keep = (mulls == 2) || self.keepable(&hand);
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

    fn untapped_sources(&self) -> Vec<(usize, String)> {
        self.board
            .iter()
            .enumerate()
            .filter(|(i, (n, _))| !self.tapped.contains(i) && is_mana_source(n))
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
        if fetched.is_some() {
            self.library = state.library.clone();
            self.hand = state.hand.clone();
        }
    }

    // ── state builder ─────────────────────────────────────────────────────
    fn build_state(&self, pool: &ManaPool) -> GameState {
        let bf: Vec<Permanent> = self
            .board
            .iter()
            .enumerate()
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
        let mut cands: Vec<(String, bool)> = Vec::new();
        for c in &self.hand {
            if play_priority(c).is_some() && !skip.contains(c) {
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

    fn try_win(&mut self, pool: &ManaPool, det: &DeterministicKillSearch, prob: &mut ProbabilisticPlanner) -> (Option<Line>, Line, GameState) {
        let state = self.build_state(pool);
        prob.decision_threshold = Some(self.win_threshold);
        let line = solve(&state, self.reg, DEV_PAYOFFS, det, prob, &mut self.dev_rng);
        let decl = self.declare(&line, &state);
        (decl, line, state)
    }

    fn declare(&mut self, line: &Line, state: &GameState) -> Option<Line> {
        if line.p_win >= 1.0 {
            return Some(line.clone());
        }
        if line.kind == "probabilistic" && line.p_win >= self.win_threshold {
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
                if self.fizzle_fatal {
                    let burn_engine = self
                        .board
                        .iter()
                        .any(|(n, _)| n == "Urabrask" || n == "Vivi Ornitier");
                    let grapeshot = self.hand.iter().any(|c| c == "Grapeshot");
                    // Pact of Negation looped as fuel owes {3}{U}{U} PER cast next upkeep -> a
                    // fizzled go-off with Pact in hand can't pay -> dead (no second chance).
                    let pact = self.hand.iter().any(|c| c == "Pact of Negation");
                    if pact || (grapeshot && !burn_engine) {
                        self.dead = true;
                    }
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
        for _ in 0..8 {
            if state.hand.iter().any(|nm| is_engine_permanent(self.reg, nm)) {
                if !deploy_engine_perms(&mut state, self.reg).is_empty() {
                    recompute(&state, &mut scores, self.reg);
                }
            }
            let oracle_acc = state.hand.iter().any(|c| c == "Thassa's Oracle")
                || state.has_permanent("Thassa's Oracle")
                || loops::can_escape(&state, self.reg, "Thassa's Oracle");
            let sustaining = state.krark_bodies(self.reg) >= 1
                && ["Storm-Kiln Artist", "Birgi, God of Storytelling", "Urabrask", "Tavern Scoundrel"]
                    .iter()
                    .any(|n| state.has_permanent(n));
            let closing = oracle_acc && sustaining;
            let floor = 8.max(state.blue_devotion(self.reg) + 4);
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
            let sig = |g: &GameState| (g.library.len(), g.graveyard.len(), g.opponent_life.iter().sum::<i64>());
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
            let (mut nstate, _log) = match nxt {
                Some(x) => x,
                None => break,
            };
            cast.push(chosen.clone());
            // Jeska mode 2 gas
            if !nstate.exiled_play.is_empty() {
                let gas = nstate.exiled_play.clone();
                nstate.exiled_play.clear();
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
        cast
    }

    // ── one turn ──────────────────────────────────────────────────────────
    pub fn play_turn(&mut self, det: &DeterministicKillSearch, prob: &mut ProbabilisticPlanner) -> Option<Line> {
        self.turn += 1;
        self.tapped.clear();
        self.exile_gas = HashMap::new();
        self.played_land = false;
        if self.verbose {
            println!("\n=== TURN {} ===", self.turn);
        }
        let mut last_pwin = 0.0f64; // best solver p_win seen this turn (for the verbose CHECK)

        if self.turn > 1 {
            self.draw();
        }

        let mut pool = ManaPool { slots: [0; 7], treasures: self.treasures };

        // play first land
        if let Some(card) = self.hand.iter().find(|c| is_land_name(c)).cloned() {
            self.play_land(&card, &mut pool, true);
            if self.verbose {
                println!("  LAND    : {card}");
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
        let developed = self.develop(&mut pool);
        if self.verbose {
            if !developed.is_empty() {
                println!("  DEVELOP : {}", developed.join(", "));
            }
            let dug: Vec<String> = self.hand.iter().filter(|c| !hand_before.contains(c)).cloned().collect();
            if !dug.is_empty() {
                println!("  DUG     : {}", dug.join(", "));
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
}

pub fn play_quiet_luck(
    reg: &Registry,
    deck: &[String],
    seed: u64,
    luck: u64,
    max_turns: i64,
    win_threshold: f64,
    fizzle_fatal: bool,
) -> GameResult {
    let mut game = SimGame::new(reg, deck, seed, win_threshold);
    game.set_dev_seed(seed.wrapping_mul(1_000_003).wrapping_add(luck));
    game.fizzle_fatal = fizzle_fatal;
    let det = DeterministicKillSearch::default();
    let mut prob = ProbabilisticPlanner { mc_sims: 80, max_first: 2, rollout_steps: 20, ..Default::default() };
    let mut won = false;
    for _ in 0..max_turns {
        if game.play_turn(&det, &mut prob).is_some() {
            won = true;
            break;
        }
        if game.dead {
            break; // committed go-off fizzled -> dead, game over (loss)
        }
    }
    GameResult { seed, luck, won, turn: if won { game.turn } else { 0 } }
}
