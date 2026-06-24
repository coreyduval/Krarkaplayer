//! game_state.rs — port of game_state.py. The simulator's source of truth: ManaPool,
//! Permanent, StackObject, GameState, plus the Krark flip math and cast_cost.

use crate::cards::{CardType, ManaCost, Registry};
use std::collections::{HashMap, HashSet};

const COMMANDERS: [&str; 2] = ["Krark, the Thumbless", "Sakashima of a Thousand Faces"];
// "Free" casts (no mana) used as loop fuel: Fierce Guardianship / Deflecting Swat are free while
// you control a commander; Mogg Salvage is treated as free (it costs {3} less with an opponent
// Island, ~always true in cEDH). Pact of Negation is already {0} in the registry.
const FREE_WITH_COMMANDER: [&str; 3] = ["Fierce Guardianship", "Deflecting Swat", "Mogg Salvage"];

// Phyrexian payment: a {C/P} pip costs 2 life OR its colored mana. We start at the Commander 40
// and prefer paying life (Phyrexian's purpose is to conserve mana for the kill), but never below
// LIFE_FLOOR — once life is low we route the pip to its colored mana instead. This is sustainable
// in a loop: life pays the first handful of casts, then the per-cast treasure engine covers the
// color. STARTING_LIFE seeds GameState/SimGame.
pub const STARTING_LIFE: i64 = 40;
pub const LIFE_FLOOR: i64 = 6;
pub const PHYREXIAN_LIFE: i64 = 2;

/// Decide how to pay a card's Phyrexian pips given current life. Returns
/// `(extra_mana_cost, life_to_pay)`: pips routed to life cost `life_to_pay` total and reduce life;
/// pips routed to mana (because paying life would drop below LIFE_FLOOR) are returned as
/// `extra_mana_cost` for the caller to fold into the spell's mana cost.
pub fn plan_phyrexian(phyrexian: &ManaCost, our_life: i64) -> (ManaCost, i64) {
    let mut extra_mana: ManaCost = HashMap::new();
    let mut life_pay = 0i64;
    let mut life = our_life;
    for (color, count) in phyrexian {
        for _ in 0..*count {
            if life - PHYREXIAN_LIFE >= LIFE_FLOOR {
                life -= PHYREXIAN_LIFE;
                life_pay += PHYREXIAN_LIFE;
            } else {
                *extra_mana.entry(color.clone()).or_insert(0) += 1;
            }
        }
    }
    (extra_mana, life_pay)
}

// --------------------------------------------------------------------------- //
// Mana pool
// --------------------------------------------------------------------------- //
//
// Performance + determinism note (Phase 7): the floating mana pool is the hottest
// data structure in the engine (can_pay / pay / cast_cost run on every DFS node and
// MC rollout). It used to be a `HashMap<String,i64>`, which (a) hashed strings on every
// access and reallocated on every clone, and (b) — because `pay`'s "dump the most
// abundant color" tie-break iterated the map — produced RUN-TO-RUN NON-DETERMINISTIC
// output (HashMap iteration order is randomized per process). It is now a fixed `[i64;7]`
// array over a canonical color order, so clones are a flat Copy, lookups are an index,
// and the generic-payment tie-break is deterministic (scanning a fixed order, keeping the
// scarce U alive — matching the Python comment's intent).

/// Canonical color slots: W U B R G C  *(wildcard). Index by `color_idx`.
pub const MANA_SYMS: [&str; 7] = ["W", "U", "B", "R", "G", "C", "*"];
const IDX_C: usize = 5;
const IDX_STAR: usize = 6;
/// Scan order for "dump the most abundant color" generic payment. R/G/B/W/U with a strict
/// `>` keeps the FIRST maximum, so on an R-vs-U flood tie R is dumped and the scarce U is
/// preserved (mirror of game_state.py's `max(colored, key=colored.get)` intent).
const GENERIC_SCAN: [usize; 5] = [3, 4, 2, 0, 1]; // R G B W U

#[inline]
fn color_idx(sym: &str) -> Option<usize> {
    match sym {
        "W" => Some(0),
        "U" => Some(1),
        "B" => Some(2),
        "R" => Some(3),
        "G" => Some(4),
        "C" => Some(IDX_C),
        "*" => Some(IDX_STAR),
        _ => None,
    }
}

/// Legendary creatures in the deck (by name). Clones resolve via `effective_name`, so a Glasspool
/// Mimic / Phantasmal Image copying Krark counts here. Used by Relic of Legends' creature-tap mana.
pub const LEGENDARY_CREATURES: &[&str] = &[
    "Krark, the Thumbless",
    "Sakashima of a Thousand Faces",
    "Baral, Chief of Compliance",
    "Birgi, God of Storytelling",
    "Vivi Ornitier",
    "Urabrask",
    "Veyran, Voice of Duality",
    "Gale, Waterdeep Prodigy",
    "Ragavan, Nimble Pilferer",
    "Okaun, Eye of Chaos",
    "Zndrsplt, Eye of Wisdom",
];

pub fn is_legendary_creature_name(name: &str) -> bool {
    LEGENDARY_CREATURES.contains(&name)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ManaPool {
    pub slots: [i64; 7],
    pub treasures: i64, // PERSISTENT any-color mana, spent last
}

impl ManaPool {
    #[inline]
    pub fn new() -> ManaPool {
        ManaPool::default()
    }

    /// Construct from (symbol, amount) pairs — used by tests and call sites that previously
    /// built a HashMap literal.
    pub fn from_pairs(pairs: &[(&str, i64)], treasures: i64) -> ManaPool {
        let mut mp = ManaPool { slots: [0; 7], treasures };
        for (k, v) in pairs {
            if let Some(i) = color_idx(k) {
                mp.slots[i] += *v;
            }
        }
        mp
    }

    #[inline]
    pub fn get(&self, sym: &str) -> i64 {
        color_idx(sym).map(|i| self.slots[i]).unwrap_or(0)
    }

    /// Deterministic (symbol, amount) view of the non-empty floating pool, canonical order.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, i64)> + '_ {
        (0..7).filter_map(move |i| {
            if self.slots[i] != 0 {
                Some((MANA_SYMS[i], self.slots[i]))
            } else {
                None
            }
        })
    }

    #[inline]
    pub fn add(&mut self, sym: &str, n: i64) {
        if let Some(i) = color_idx(sym) {
            self.slots[i] += n;
        }
    }

    pub fn add_cost(&mut self, mana: &ManaCost) {
        for (k, v) in mana {
            self.add(k, *v);
        }
    }

    #[inline]
    pub fn total(&self) -> i64 {
        self.slots.iter().sum::<i64>() + self.treasures
    }

    /// Colored pips paid by their color or by wildcard '*'/Treasure (any-color); generic by
    /// anything. 'X' in a cost is ignored. Mirror of ManaPool.can_pay.
    pub fn can_pay(&self, cost: &ManaCost) -> bool {
        let mut avail = self.slots;
        let mut wild = std::mem::take(&mut avail[IDX_STAR]) + self.treasures;
        for (sym, need0) in cost {
            if sym == "generic" || sym == "X" {
                continue;
            }
            let i = match color_idx(sym) {
                Some(i) => i,
                None => continue,
            };
            let mut need = *need0;
            let have = avail[i];
            let use_ = have.min(need);
            avail[i] = have - use_;
            need -= use_;
            if need > 0 {
                if wild >= need {
                    wild -= need;
                } else {
                    return false;
                }
            }
        }
        let generic = cost.get("generic").copied().unwrap_or(0);
        avail.iter().sum::<i64>() + wild >= generic
    }

    /// Mutates the pool. Mirror of ManaPool.pay (greedy generic payment).
    pub fn pay(&mut self, cost: &ManaCost) {
        if !self.can_pay(cost) {
            panic!(
                "Cannot pay {:?} from {:?} (+{} treasure)",
                cost, self.slots, self.treasures
            );
        }

        // Pay colored pips: own color first, then wildcard ('*' first, then Treasures).
        for (sym, need0) in cost {
            if sym == "generic" || sym == "X" {
                continue;
            }
            let i = match color_idx(sym) {
                Some(i) => i,
                None => continue,
            };
            let mut need = *need0;
            let have = self.slots[i];
            let use_ = have.min(need);
            self.slots[i] = have - use_;
            need -= use_;
            if need > 0 {
                self.spend_wild(need);
            }
        }

        // Generic: colorless C first, then most abundant color, then '*', then Treasures.
        let mut generic = cost.get("generic").copied().unwrap_or(0);
        while generic > 0 && self.slots[IDX_C] > 0 {
            self.slots[IDX_C] -= 1;
            generic -= 1;
        }
        while generic > 0 {
            // most abundant non-'*' color with v > 0 (deterministic: first max in scan order)
            let mut best: Option<usize> = None;
            let mut best_v = 0;
            for &i in &GENERIC_SCAN {
                let v = self.slots[i];
                if v > best_v {
                    best_v = v;
                    best = Some(i);
                }
            }
            if let Some(i) = best {
                self.slots[i] -= 1;
            } else if self.slots[IDX_STAR] > 0 {
                self.slots[IDX_STAR] -= 1;
            } else if self.treasures > 0 {
                self.treasures -= 1;
            } else {
                break;
            }
            generic -= 1;
        }
    }

    #[inline]
    fn spend_wild(&mut self, n: i64) {
        let mut n = n;
        let star = self.slots[IDX_STAR];
        let use_ = star.min(n);
        self.slots[IDX_STAR] = star - use_;
        n -= use_;
        if n > 0 {
            self.treasures -= n;
        }
    }
}

// --------------------------------------------------------------------------- //
// Battlefield permanents
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone)]
pub struct Permanent {
    pub name: String,
    pub copy_of: Option<String>,
    pub tapped: bool,
    pub summoning_sick: bool,
    pub is_token: bool,
}

impl Permanent {
    pub fn new(name: &str) -> Permanent {
        Permanent {
            name: name.to_string(),
            copy_of: None,
            tapped: false,
            summoning_sick: true,
            is_token: false,
        }
    }

    /// What the permanent functionally *is* (Sakashima-as-Krark keeps Sakashima's name).
    pub fn effective_name(&self) -> &str {
        self.copy_of.as_deref().unwrap_or(&self.name)
    }

    /// The CardDef this permanent functions as (copy_of if set, else its own name).
    pub fn functions_as<'a>(&self, reg: &'a Registry) -> &'a crate::cards::CardDef {
        reg.get(self.copy_of.as_deref().unwrap_or(&self.name))
    }
}

/// Helper: a Krark body on the battlefield (legend-safe via copy_of).
pub fn krark_body(name: &str, copy_of: Option<&str>, token: bool) -> Permanent {
    Permanent {
        name: name.to_string(),
        copy_of: copy_of.map(|s| s.to_string()),
        tapped: false,
        summoning_sick: false,
        is_token: token,
    }
}

// --------------------------------------------------------------------------- //
// The state
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone)]
pub struct GameState {
    pub library: Vec<String>,
    pub hand: Vec<String>,
    pub battlefield: Vec<Permanent>,
    pub graveyard: Vec<String>,
    pub exile: Vec<String>,
    pub exiled_play: Vec<String>,

    pub mana: ManaPool,
    pub storm_count: i64,
    pub turn: i64,
    pub is_my_turn: bool,

    pub opponent_life: Vec<i64>,
    pub opponent_library: Vec<i64>,
    pub opponent_hand: Vec<i64>,
    pub our_life: i64,

    pub infinite: HashSet<String>,
    pub game_result: Option<String>,

    /// Vivi Ornitier: power = +1/+1 counters = noncreature spells cast while it's been in play
    /// (persists across turns). Its {0} once-per-turn ability adds `vivi_power` U/R (modeled as `*`).
    pub vivi_power: i64,
    pub vivi_mana_used: bool,
}

impl Default for GameState {
    fn default() -> GameState {
        GameState {
            library: Vec::new(),
            hand: Vec::new(),
            battlefield: Vec::new(),
            graveyard: Vec::new(),
            exile: Vec::new(),
            exiled_play: Vec::new(),
            mana: ManaPool::new(),
            storm_count: 0,
            turn: 1,
            is_my_turn: true,
            opponent_life: vec![160],
            opponent_library: vec![99, 99, 99],
            opponent_hand: vec![5, 5, 5],
            our_life: STARTING_LIFE,
            infinite: HashSet::new(),
            game_result: None,
            vivi_power: 0,
            vivi_mana_used: false,
        }
    }
}

impl GameState {
    pub fn krark_bodies(&self, reg: &Registry) -> i64 {
        self.battlefield
            .iter()
            .filter(|p| p.functions_as(reg).is_krark_body)
            .count() as i64
    }

    /// Is a legendary creature in play? Mox Amber makes mana only if one is. The deck's relevant
    /// legendary creatures are the commanders — Krark, Sakashima, and clones copying Krark.
    pub fn has_legendary_creature(&self) -> bool {
        self.battlefield.iter().any(|p| {
            p.effective_name() == "Krark, the Thumbless"
                || p.name == "Sakashima of a Thousand Faces"
        })
    }

    /// Battlefield indices of untapped legendary creatures — Relic of Legends' second ability can
    /// tap each one for one mana of any color. This is free for this deck: no win line needs Krark
    /// bodies untapped (Krark enables flips by being in play; the combat kill is the Dualcaster
    /// token combo, not Krark beatdown), so idle legends are pure extra mana.
    pub fn untapped_legendary_creature_idxs(&self) -> Vec<usize> {
        self.battlefield
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.tapped && is_legendary_creature_name(p.effective_name()))
            .map(|(i, _)| i)
            .collect()
    }

    /// Count of mana Relic of Legends adds via its creature-tap ability right now (one per untapped
    /// legendary creature), or 0 if no Relic in play. Any-color.
    pub fn relic_legend_mana(&self) -> i64 {
        if self.battlefield.iter().any(|p| p.effective_name() == "Relic of Legends") {
            self.untapped_legendary_creature_idxs().len() as i64
        } else {
            0
        }
    }

    pub fn trigger_doublers(&self, reg: &Registry) -> i64 {
        self.battlefield
            .iter()
            .filter(|p| p.functions_as(reg).is_trigger_doubler)
            .count() as i64
    }

    /// bodies × (1 + doublers). Mirror of flips_per_cast.
    pub fn flips_per_cast(&self, reg: &Registry) -> i64 {
        self.krark_bodies(reg) * (1 + self.trigger_doublers(reg))
    }

    fn count_functioning(&self, name: &str) -> i64 {
        self.battlefield
            .iter()
            .filter(|p| p.effective_name() == name)
            .count() as i64
    }

    /// How many times `engine`'s value trigger fires per qualifying event (Veyran/Harmonic
    /// additive, each where it legally applies). Mirror of value_multiplier.
    pub fn value_multiplier(
        &self,
        engine: &crate::cards::CardDef,
        cast_is_instant_or_sorcery: bool,
    ) -> i64 {
        let mut m = 1;
        let cause = engine.trigger_cause.as_deref();
        let veyran_applies = matches!(cause, Some("is_cast_or_copy") | Some("is_cast"))
            || (cause == Some("spell_cast") && cast_is_instant_or_sorcery);
        if veyran_applies {
            m += self.count_functioning("Veyran, Voice of Duality");
        }
        if engine.is_shaman_or_wizard() {
            m += self.count_functioning("Harmonic Prodigy");
        }
        m
    }

    /// (index, CardDef) for every battlefield permanent that has a value/mana trigger.
    pub fn value_engines<'a>(&self, reg: &'a Registry) -> Vec<(usize, &'a crate::cards::CardDef)> {
        let mut out = Vec::new();
        for (i, p) in self.battlefield.iter().enumerate() {
            let f = p.functions_as(reg);
            if f.has_value_trigger() {
                out.push((i, f));
            }
        }
        out
    }

    pub fn has_krarks_thumb(&self) -> bool {
        self.battlefield.iter().any(|p| p.effective_name() == "Krark's Thumb")
    }

    pub fn has_vivi(&self) -> bool {
        self.battlefield.iter().any(|p| p.effective_name() == "Vivi Ornitier")
    }

    /// Mana still available from Vivi's once-per-turn {0} ability (= its power); 0 if used/absent.
    pub fn vivi_available_mana(&self) -> i64 {
        if self.has_vivi() && !self.vivi_mana_used && self.vivi_power > 0 {
            self.vivi_power
        } else {
            0
        }
    }

    /// Fire Vivi's {0} ability (once per turn): add `power` wildcard mana. Returns false when
    /// unavailable (no Vivi / already used / 0 power) so a payment retry-loop knows to stop.
    pub fn vivi_mana(&mut self) -> bool {
        if self.has_vivi() && !self.vivi_mana_used && self.vivi_power > 0 {
            self.mana.add("*", self.vivi_power);
            self.vivi_mana_used = true;
            true
        } else {
            false
        }
    }

    /// Single-flip win probability. Thumb -> 0.75 else 0.50.
    pub fn flip_p(&self) -> f64 {
        if self.has_krarks_thumb() {
            0.75
        } else {
            0.50
        }
    }

    pub fn defense_grid(&self) -> bool {
        self.battlefield.iter().any(|p| p.effective_name() == "Defense Grid")
    }

    pub fn controls_commander(&self) -> bool {
        self.battlefield
            .iter()
            .any(|p| COMMANDERS.contains(&p.name.as_str()))
    }

    /// Cost to cast `card_name` from hand right now (incl. Defense Grid +3 on opp turns,
    /// and the free-with-commander clause). Mirror of cast_cost.
    ///
    /// Returns a `Cow` so the common hot path (my turn / no Defense Grid) BORROWS the
    /// registry's immutable cost map with zero allocation; only the rare free-commander or
    /// opp-turn-Defense-Grid branches allocate an owned, adjusted copy. cast_cost runs on
    /// essentially every DFS node, so cutting this clone is a direct hot-path win.
    pub fn cast_cost<'a>(&self, reg: &'a Registry, card_name: &str) -> std::borrow::Cow<'a, ManaCost> {
        use std::borrow::Cow;
        let grid = !self.is_my_turn && self.defense_grid();
        // Baral, Chief of Compliance makes your instant/sorcery spells cost {1} less each (generic
        // only, never below 0). Clones of Baral stack. This was applied in SimGame::cast_cost but
        // NOT here — so the go-off/runaway math (do_cast / estimate_p_lethal / analyze_runaway, which
        // all route through this function) priced every I/S {1} too high whenever Baral was out.
        let baral = if reg.get(card_name).is_instant_or_sorcery() {
            self.count_functioning("Baral, Chief of Compliance")
        } else {
            0
        };
        if FREE_WITH_COMMANDER.contains(&card_name) && self.controls_commander() {
            let mut cost = HashMap::new();
            if grid {
                cost.insert("generic".to_string(), 3);
            }
            return Cow::Owned(cost);
        }
        let base = &reg.get(card_name).cost;
        if grid || baral > 0 {
            let mut cost = base.clone();
            if grid {
                *cost.entry("generic".to_string()).or_insert(0) += 3;
            }
            if baral > 0 {
                let g = cost.get("generic").copied().unwrap_or(0);
                cost.insert("generic".to_string(), (g - baral).max(0));
            }
            Cow::Owned(cost)
        } else {
            Cow::Borrowed(base)
        }
    }

    pub fn blue_devotion(&self, reg: &Registry) -> i64 {
        self.battlefield
            .iter()
            .map(|p| p.functions_as(reg).blue_pips())
            .sum()
    }

    pub fn has_permanent(&self, effective_name: &str) -> bool {
        self.battlefield.iter().any(|p| p.effective_name() == effective_name)
    }

    pub fn is_creature_on_bf(&self, reg: &Registry) -> bool {
        self.battlefield
            .iter()
            .any(|p| p.functions_as(reg).types.contains(&CardType::Creature))
    }
}
