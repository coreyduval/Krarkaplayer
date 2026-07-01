//! planner.rs — port of planner.py. Search layer: actions, deterministic kill search,
//! probabilistic planner, solve().

use crate::cards::{CardType, Registry};
use crate::game_state::{GameState, Permanent};
use crate::loops;
use crate::resolver::{analyze_cast, apply_etb, resolve_cast_sample, Choices};
use crate::tables::{life_per_tap, mana_source, SrcMode};
use crate::win;
use rand::Rng;
use std::collections::HashSet;

/// Experiment flag (--ritual-prelude): let the planner fire a sorcery-speed ritual to power out a
/// payoff a turn early. Default off. See best_line's ritual-prelude branch + sim::declare gate.
pub static RITUAL_PRELUDE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
fn ritual_prelude_on() -> bool {
    *RITUAL_PRELUDE.get().unwrap_or(&false)
}
/// Cheap red rituals usable as a mana prelude. Net-positive only with a Krark flip win (lose-flip
/// returns the spell to hand for 0), so the prelude is inherently probabilistic.
pub const PRELUDE_RITUALS: &[&str] = &["Pyretic Ritual", "Desperate Ritual", "Rite of Flame"];
/// True if a committed line's first cast is a pure-mana ritual prelude — its fizzle is benign
/// (the payoff is never committed; the ritual just bounces back to hand), so it gates at send_gate.
pub fn first_is_prelude_ritual(line: &Line) -> bool {
    line.first.as_ref().map_or(false, |(c, _)| PRELUDE_RITUALS.contains(&c.as_str()))
}

// --------------------------------------------------------------------------- //
// Actions / Line
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone)]
pub struct Action {
    pub kind: String,        // "cast" | "cast_perm" | "activate" | "pass"
    pub card: Option<String>,
    pub idx: Option<usize>,  // for activate
    pub target: Option<String>, // Brain Freeze choice
}

impl Action {
    pub fn cast(card: &str) -> Action {
        Action { kind: "cast".into(), card: Some(card.into()), idx: None, target: None }
    }
    pub fn cast_target(card: &str, target: &str) -> Action {
        Action { kind: "cast".into(), card: Some(card.into()), idx: None, target: Some(target.into()) }
    }
    pub fn cast_perm(card: &str) -> Action {
        Action { kind: "cast_perm".into(), card: Some(card.into()), idx: None, target: None }
    }
    pub fn activate(card: &str, idx: usize) -> Action {
        Action { kind: "activate".into(), card: Some(card.into()), idx: Some(idx), target: None }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Line {
    pub actions: Vec<Action>,
    pub kind: String,
    pub p_win: f64,
    pub detail: String,
    pub base: Option<GameState>,
    pub first: Option<(String, String)>,
    pub loop_line: bool,
    /// Human-readable step-by-step execution of a deterministic kill (combo walkthrough). Empty for
    /// probabilistic lines (those get the cast-by-cast go-off trace instead).
    pub walkthrough: Vec<String>,
}

// --------------------------------------------------------------------------- //
// Mana ability application
// --------------------------------------------------------------------------- //

fn has_other_untapped_creature(state: &GameState, reg: &Registry, exclude_idx: usize) -> bool {
    state.battlefield.iter().enumerate().any(|(i, p)| {
        i != exclude_idx && p.functions_as(reg).types.contains(&CardType::Creature) && !p.tapped
    })
}

pub fn tap_out(state: &GameState) -> GameState {
    let mut s = state.clone();
    let legend = s.has_legendary_creature();
    for i in 0..s.battlefield.len() {
        let nm = s.battlefield[i].effective_name();
        if nm == "Mox Amber" && !legend {
            continue; // dead without a legendary creature
        }
        if let Some((mode, produced)) = s.battlefield[i].mana_produced() {
            if mode == SrcMode::Tap && !s.battlefield[i].tapped {
                let life = life_per_tap(nm);
                s.battlefield[i].tapped = true;
                s.mana.add_cost(&produced);
                s.our_life -= life;
            } else if mode == SrcMode::LifeRepeat {
                // Treasonous Ogre: drain the full life→mana battery (down to the floor) — the go-off
                // wants max mana. Repeatable, so never marked tapped.
                let n = ((s.our_life - crate::game_state::LIFE_FLOOR)
                    / crate::tables::LIFE_REPEAT_COST)
                    .max(0);
                for (k, v) in &produced {
                    s.mana.add(k, v * n);
                }
                s.our_life -= crate::tables::LIFE_REPEAT_COST * n;
            }
        }
    }
    // Relic of Legends' second ability: tap each idle legendary creature for one any-color mana.
    // Free here (no win line needs Krark bodies untapped), so harvest them all.
    if s.battlefield.iter().any(|p| p.effective_name() == "Relic of Legends") {
        for i in s.untapped_legendary_creature_idxs() {
            s.battlefield[i].tapped = true;
            s.mana.add("*", 1);
        }
    }
    s
}

pub fn apply_mana_ability_reg(s: &mut GameState, reg: &Registry, idx: usize) {
    let name = s.battlefield[idx].effective_name().to_string();
    let (mode, produced) = s.battlefield[idx].mana_produced().unwrap();
    if mode == SrcMode::LifeRepeat {
        // Treasonous Ogre: drain the full battery, charge life, never tap (repeatable).
        let n = ((s.our_life - crate::game_state::LIFE_FLOOR) / crate::tables::LIFE_REPEAT_COST).max(0);
        for (k, v) in &produced {
            s.mana.add(k, v * n);
        }
        s.our_life -= crate::tables::LIFE_REPEAT_COST * n;
        return;
    }
    match mode {
        SrcMode::Tap => s.battlefield[idx].tapped = true,
        SrcMode::TapCreature => {
            s.battlefield[idx].tapped = true;
            for j in 0..s.battlefield.len() {
                if j != idx
                    && s.battlefield[j].functions_as(reg).types.contains(&CardType::Creature)
                    && !s.battlefield[j].tapped
                {
                    s.battlefield[j].tapped = true;
                    break;
                }
            }
        }
        SrcMode::Sac => {
            let p = s.battlefield.remove(idx);
            s.graveyard.push(p.name);
        }
        SrcMode::SacHand => {
            let p = s.battlefield.remove(idx);
            s.graveyard.push(p.name);
            let hand: Vec<String> = std::mem::take(&mut s.hand);
            s.graveyard.extend(hand);
        }
        SrcMode::LifeRepeat => unreachable!("LifeRepeat handled by early return above"),
    }
    s.mana.add_cost(&produced);
    s.our_life -= life_per_tap(&name);
}

pub fn apply_perm_cast(s: &mut GameState, reg: &Registry, card: &str) {
    if let Some(pos) = s.hand.iter().position(|c| c == card) {
        s.hand.remove(pos);
    }
    let cost = s.cast_cost(reg, card);
    s.mana.pay(&cost);
    let mut perm = Permanent::new(card);
    perm.summoning_sick = true;
    if card == "Sakashima of a Thousand Faces"
        && s.battlefield.iter().any(|p| p.effective_name() == "Krark, the Thumbless")
    {
        perm.copy_of = Some("Krark, the Thumbless".to_string());
    }
    s.battlefield.push(perm);
    apply_etb(s, reg, card);
}

// --------------------------------------------------------------------------- //
// Engine permanent helpers
// --------------------------------------------------------------------------- //

pub fn is_engine_permanent(reg: &Registry, name: &str) -> bool {
    let cd = reg.get(name);
    if !cd.is_permanent() || cd.types.contains(&CardType::Land) {
        return false;
    }
    if name == "Krark's Thumb" {
        return true;
    }
    cd.is_krark_body
        || cd.clones_sakashima_safe
        || cd.is_trigger_doubler
        || cd.draw_per_trigger != 0
        || cd.treasure_per_trigger != 0
        || !cd.mana_per_trigger.is_empty()
        || cd.damage_per_trigger != 0
        || cd.treasure_per_flip_win != 0
}

/// Deploy every affordable engine permanent from hand in place. Returns deployed names.
pub fn deploy_engine_perms(s: &mut GameState, reg: &Registry) -> Vec<String> {
    let mut deployed = Vec::new();
    loop {
        // candidates: engine perms or mana sources in hand, non-land
        let mut cands: Vec<String> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for nm in &s.hand {
            if !seen.insert(nm.as_str()) {
                continue;
            }
            let cd = reg.get(nm);
            if (is_engine_permanent(reg, nm) || mana_source(nm).is_some())
                && !cd.types.contains(&CardType::Land)
            {
                cands.push(nm.clone());
            }
        }
        // Underworld Breach isn't an engine permanent (no triggers), but cast it when the escape
        // combo is live (payoff in graveyard + depth + a Krark body) so the kill search can ride the
        // graveyard escapes. It self-sacs EOT, so breach_line_live keeps it from being wasted.
        if crate::loops::breach_line_live(s, reg) && !cands.iter().any(|c| c == "Underworld Breach") {
            cands.push("Underworld Breach".to_string());
        }
        // mana rocks first, then cheapest engine piece
        cands.sort_by(|a, b| {
            let ka = (mana_source(a).is_none(), s.cast_cost(reg, a).values().sum::<i64>());
            let kb = (mana_source(b).is_none(), s.cast_cost(reg, b).values().sum::<i64>());
            ka.cmp(&kb)
        });
        let mut did = false;
        for nm in &cands {
            if !s.mana.can_pay(&s.cast_cost(reg, nm)) {
                continue;
            }
            // Chrome Mox needs a nonland/nonartifact card to imprint (exile); skip if none.
            let chrome_imp = if nm == "Chrome Mox" {
                match s.hand.iter().find(|c| {
                    let cd = reg.get(c);
                    c.as_str() != "Chrome Mox" && !cd.is_land() && !cd.is_artifact()
                }) {
                    Some(t) => Some(t.clone()),
                    None => continue,
                }
            } else {
                None
            };
            // Mox Diamond's ETB requires discarding a land card (or it's put into the graveyard);
            // with no land in hand it can't stay in play, so it's not a free mana source.
            let mox_pitch = if nm == "Mox Diamond" {
                match s.hand.iter().find(|c| c.as_str() != "Mox Diamond" && reg.get(c).is_land()) {
                    Some(t) => Some(t.clone()),
                    None => continue,
                }
            } else {
                None
            };
            apply_perm_cast(s, reg, nm);
            if let Some(t) = chrome_imp {
                if let Some(pos) = s.hand.iter().position(|c| *c == t) {
                    s.hand.remove(pos);
                }
            }
            if let Some(t) = mox_pitch {
                if let Some(pos) = s.hand.iter().position(|c| *c == t) {
                    s.hand.remove(pos);
                }
                s.graveyard.push(t);
            }
            deployed.push(nm.clone());
            // a fresh tap source taps for mana now (Mox Amber only with a legend in play)
            if let Some((SrcMode::Tap, produced)) = mana_source(nm) {
                if !(nm == "Mox Amber" && !s.has_legendary_creature()) {
                    for j in 0..s.battlefield.len() {
                        if s.battlefield[j].effective_name() == nm && !s.battlefield[j].tapped {
                            s.battlefield[j].tapped = true;
                            s.mana.add_cost(&produced);
                            break;
                        }
                    }
                }
            }
            did = true;
            break;
        }
        if !did {
            break;
        }
    }
    deployed
}

pub fn deploy_engine_base(state: &GameState, reg: &Registry) -> (GameState, Vec<Action>) {
    let mut s = tap_out(state);
    let names = deploy_engine_perms(&mut s, reg);
    let actions = names.iter().map(|n| Action::cast_perm(n)).collect();
    (s, actions)
}

// --------------------------------------------------------------------------- //
// enumerate_actions / apply_deterministic
// --------------------------------------------------------------------------- //

pub fn enumerate_actions(state: &GameState, reg: &Registry) -> Vec<Action> {
    let mut acts = Vec::new();
    for (i, p) in state.battlefield.iter().enumerate() {
        let nm = p.effective_name();
        if nm == "Mox Amber" && !state.has_legendary_creature() {
            continue; // dead without a legendary creature
        }
        if let Some((mode, _)) = mana_source(nm) {
            if matches!(mode, SrcMode::Tap | SrcMode::TapCreature) && p.tapped {
                continue;
            }
            if mode == SrcMode::TapCreature && !has_other_untapped_creature(state, reg, i) {
                continue;
            }
            acts.push(Action::activate(nm, i));
        }
    }
    let has_creature = state
        .battlefield
        .iter()
        .any(|p| p.functions_as(reg).types.contains(&CardType::Creature));
    let mut seen: HashSet<&str> = HashSet::new();
    for nm in &state.hand {
        if !seen.insert(nm.as_str()) {
            continue;
        }
        let cdef = reg.get(nm);
        let cost = state.cast_cost(reg, nm);
        if !state.mana.can_pay(&cost) {
            continue;
        }
        if cdef.is_instant_or_sorcery() {
            if !crate::cards::castable_in_solitaire(nm, has_creature) {
                continue;
            }
            if nm == "Brain Freeze" {
                acts.push(Action::cast_target(nm, "self"));
                acts.push(Action::cast_target(nm, "opponents"));
            } else {
                acts.push(Action::cast(nm));
            }
        } else if cdef.is_permanent() && !cdef.types.contains(&CardType::Land) {
            acts.push(Action::cast_perm(nm));
        }
    }
    acts
}

/// Successor for a luck-free choice (I/S cast forces 0 Krark wins).
pub fn apply_deterministic(state: &GameState, reg: &Registry, action: &Action) -> GameState {
    let mut s = state.clone();
    match action.kind.as_str() {
        "activate" => {
            apply_mana_ability_reg(&mut s, reg, action.idx.unwrap());
            s
        }
        "cast_perm" => {
            apply_perm_cast(&mut s, reg, action.card.as_ref().unwrap());
            s
        }
        "cast" => {
            let card = action.card.as_ref().unwrap();
            if let Some(pos) = s.hand.iter().position(|c| c == card) {
                s.hand.remove(pos);
            }
            let cost = s.cast_cost(reg, card);
            s.mana.pay(&cost);
            let mut choices = Choices::default();
            choices.target = action.target.clone();
            // forced_wins = 0; rng unused for forced wins
            let mut rng = rand::rngs::StdRng::from_seed_const();
            resolve_cast_sample(&mut s, reg, card, &mut rng, &choices, Some(0));
            s
        }
        _ => s,
    }
}

// Helper trait for a const-seeded RNG (forced wins path never reads it).
trait FromSeedConst {
    fn from_seed_const() -> Self;
}
impl FromSeedConst for rand::rngs::StdRng {
    fn from_seed_const() -> Self {
        use rand::SeedableRng;
        rand::rngs::StdRng::from_seed([0u8; 32])
    }
}

// --------------------------------------------------------------------------- //
// terminal_value
// --------------------------------------------------------------------------- //

pub fn terminal_value(state: &mut GameState, reg: &Registry) -> Option<f64> {
    loops::apply_loops(state, reg);
    if win::evaluate_win(state, reg, None).won {
        return Some(1.0);
    }
    if win::check_loss(state, reg).won {
        return Some(0.0);
    }
    None
}

// --------------------------------------------------------------------------- //
// Deterministic kill search
// --------------------------------------------------------------------------- //

const STORM_SPELLS: &[&str] = &["Grapeshot", "Brain Freeze", "Flusterstorm"];

fn deterministic_useful(state: &GameState, reg: &Registry, action: &Action) -> bool {
    if action.kind != "cast" {
        return true;
    }
    let card = action.card.as_ref().unwrap();
    let cdef = reg.get(card);
    if cdef.is_instant_or_sorcery() && !STORM_SPELLS.contains(&card.as_str()) && state.flips_per_cast(reg) > 0 {
        return false;
    }
    true
}

pub struct DeterministicKillSearch {
    pub max_depth: i64,
    pub node_budget: i64,
}

impl Default for DeterministicKillSearch {
    fn default() -> Self {
        DeterministicKillSearch { max_depth: 12, node_budget: 30000 }
    }
}

impl DeterministicKillSearch {
    pub fn find_kill(&self, state: &GameState, reg: &Registry) -> Option<Line> {
        let mut visited: HashSet<u64> = HashSet::new();
        let mut budget = self.node_budget;
        self.dfs(&state.clone(), reg, Vec::new(), self.max_depth, &mut visited, &mut budget)
    }

    fn dfs(
        &self,
        s: &GameState,
        reg: &Registry,
        line: Vec<Action>,
        depth: i64,
        visited: &mut HashSet<u64>,
        budget: &mut i64,
    ) -> Option<Line> {
        if *budget <= 0 {
            return None;
        }
        *budget -= 1;
        let mut sc = s.clone();
        let tv = terminal_value(&mut sc, reg);
        if tv == Some(1.0) {
            let w = win::evaluate_win(&sc, reg, None);
            return Some(Line {
                actions: line,
                kind: "deterministic".into(),
                p_win: 1.0,
                detail: kill_detail(&sc, reg, &w.detail),
                walkthrough: kill_walkthrough(&sc, reg),
                ..Default::default()
            });
        }
        if tv == Some(0.0) || depth == 0 {
            return None;
        }
        let key = canonical_hash(&sc);
        if visited.contains(&key) {
            return None;
        }
        visited.insert(key);
        for action in enumerate_actions(&sc, reg) {
            if !deterministic_useful(&sc, reg, &action) {
                continue;
            }
            let succ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                apply_deterministic(&sc, reg, &action)
            }));
            let succ = match succ {
                Ok(s2) => s2,
                Err(_) => continue,
            };
            let mut nl = line.clone();
            nl.push(action);
            if let Some(got) = self.dfs(&succ, reg, nl, depth - 1, visited, budget) {
                return Some(got);
            }
        }
        None
    }
}

fn kill_detail(state: &GameState, reg: &Registry, win_detail: &str) -> String {
    let mut sc = state.clone();
    let rep = loops::apply_loops(&mut sc, reg);
    if !rep.confirmed.is_empty() && !rep.reasons.is_empty() {
        rep.reasons.join("; ")
    } else if !win_detail.is_empty() {
        win_detail.to_string()
    } else {
        "guaranteed lethal".to_string()
    }
}

/// Step-by-step execution narration for a deterministic kill — identifies which combo the won state
/// holds and walks through how it loops to lethal. Pure exposition for the diag (the engine proves
/// lethality structurally; this explains the line a human would actually execute).
fn kill_walkthrough(s: &GameState, reg: &Registry) -> Vec<String> {
    let access = |n: &str| {
        s.hand.iter().any(|c| c == n)
            || s.exiled_play.iter().any(|c| c == n)
            || s.graveyard.iter().any(|c| c == n)
    };
    let shimmer = crate::cards::SHIMMERS
        .iter()
        .copied()
        .find(|sh| access(sh));
    let bodies = s.krark_bodies(reg);
    let flips = s.flips_per_cast(reg).max(1);
    let dualcasters = s
        .battlefield
        .iter()
        .filter(|p| p.effective_name() == "Dualcaster Mage")
        .count();
    let mut w = Vec::new();

    // 1) Dualcaster Mage + shimmer -> infinite hasty Dualcaster tokens.
    if dualcasters >= 2 || (access("Dualcaster Mage") && shimmer.is_some()) {
        let sh = shimmer.unwrap_or("Twinflame");
        w.push(format!("Combo: Dualcaster Mage + {sh} → infinite hasty attackers (combat)"));
        w.push(format!("  1. Cast {sh} targeting a creature you control."));
        w.push(format!("  2. In response, flash in Dualcaster Mage — its ETB copies the {sh} spell."));
        w.push(format!("  3. Aim the copy at Dualcaster Mage → a hasty token Dualcaster, whose ETB copies {sh} again."));
        w.push("  4. Repeat → unbounded hasty Dualcaster tokens → swing for ≥160 → lethal.".into());
        return w;
    }
    // 2) Krark + Sakashima legend-break + shimmer -> infinite hasty Krarks (the namesake).
    if shimmer.is_some() && s.has_sakashima_break() && bodies >= 1 {
        let sh = shimmer.unwrap();
        let steer = if s.has_krarks_thumb() {
            "Krark's Thumb lets you CHOOSE each flip: take exactly 1 loss, win the rest"
        } else {
            "steer to exactly 1 loss, win the rest (reliable at these trigger counts)"
        };
        w.push(format!("Combo: Krark + Sakashima legend-break + {sh} → infinite hasty Krarks (combat)"));
        w.push(format!("  1. Cast {sh} ({{1}}{{R}}) copying Krark — your {flips} Krark triggers flip {flips} coins."));
        w.push(format!("  2. {steer}: the loss returns {sh} to hand (recast it), each win is a token copy of Krark (hasty)."));
        w.push("  3. Sakashima's legend-rule break lets every token Krark stick on the battlefield.".into());
        w.push("  4. A renewable mana source refunds the {1}{R}, so the army grows each iteration, mana-neutral.".into());
        w.push("  → arbitrarily many hasty Krarks → swing for ≥160 → lethal.".into());
        return w;
    }
    // 3) Krark + Flare of Duplication -> infinite magecraft.
    if access("Flare of Duplication")
        && (s.has_permanent("Storm-Kiln Artist") || s.has_permanent("Archmage Emeritus"))
    {
        let engine = if s.has_permanent("Storm-Kiln Artist") {
            "Storm-Kiln Artist makes a Treasure per magecraft → infinite MANA"
        } else {
            "Archmage Emeritus draws a card per magecraft → draw your library"
        };
        w.push("Combo: Krark + Flare of Duplication → infinite magecraft".to_string());
        w.push(format!("  1. Cast Flare of Duplication ({{1}}{{R}}{{R}}); win one Krark flip to copy it ({flips} triggers)."));
        w.push("  2. Aim the copy at the original Flare on the stack — copying doesn't consume it, so it spawns another Flare copy.".into());
        w.push("  3. Each copy is a magecraft trigger (copies are NOT cast → magecraft, not storm) → unbounded magecraft.".into());
        w.push(format!("  4. {engine}; recast a spell via Krark to build storm, then Grapeshot / Brain Freeze closes → lethal."));
        return w;
    }
    // 4) Grapeshot storm burn.
    if access("Grapeshot") {
        w.push("Combo: Grapeshot storm burn".to_string());
        w.push(format!("  1. Loop cheap spells with Krark ({flips} triggers/cast: win=copy, lose=return to recast) to pile up storm + magecraft mana."));
        w.push("  2. Cast Grapeshot — it copies once per spell cast this turn (storm) → ≥160 damage across the pod → lethal.".into());
        return w;
    }
    // 5) Brain Freeze mill.
    if access("Brain Freeze") {
        w.push("Combo: Brain Freeze mill".to_string());
        w.push(format!("  1. Build storm by looping spells with Krark ({flips} triggers/cast)."));
        w.push("  2. Cast Brain Freeze — mills 3× storm per opponent → decks the pod → lethal.".into());
        return w;
    }
    // 6) Vivi / Urabrask 3-per-cast burn.
    if s.has_permanent("Vivi Ornitier") || s.has_permanent("Urabrask") {
        let src = if s.has_permanent("Vivi Ornitier") { "Vivi Ornitier" } else { "Urabrask" };
        w.push(format!("Combo: {src} burn"));
        w.push(format!("  1. Each instant/sorcery cast deals damage via {src}; Krark copies multiply it ({flips} triggers/cast)."));
        w.push("  2. Loop cheap spells (win=copy, lose=return) → damage accumulates past 160 → lethal.".into());
        return w;
    }
    w // empty -> caller falls back to the one-line detail
}

// canonical hash: a structural digest sufficient for memoization (order-sensitive library/stack,
// multiset elsewhere). We hash a stable string.
fn canonical_hash(s: &GameState) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.library.hash(&mut h);
    let mut hand = s.hand.clone();
    hand.sort();
    hand.hash(&mut h);
    let mut bf: Vec<(String, Option<String>, bool, bool, bool)> = s
        .battlefield
        .iter()
        .map(|p| (p.name.clone(), p.copy_of.clone(), p.tapped, p.summoning_sick, p.is_token))
        .collect();
    bf.sort();
    bf.hash(&mut h);
    let mut gy = s.graveyard.clone();
    gy.sort();
    gy.hash(&mut h);
    let mut ex = s.exiled_play.clone();
    ex.sort();
    ex.hash(&mut h);
    let pool: Vec<(&str, i64)> = s.mana.iter().collect();
    pool.hash(&mut h);
    s.mana.treasures.hash(&mut h);
    s.storm_count.hash(&mut h);
    s.opponent_life.hash(&mut h);
    s.opponent_library.hash(&mut h);
    let mut inf: Vec<&String> = s.infinite.iter().collect();
    inf.sort();
    inf.hash(&mut h);
    h.finish()
}

// --------------------------------------------------------------------------- //
// Probabilistic planner
// --------------------------------------------------------------------------- //

pub struct ProbabilisticPlanner {
    pub max_depth: i64,
    pub mc_sims: i64,
    pub max_first: usize,
    pub rollout_steps: i64,
    pub decision_threshold: Option<f64>,
}

impl Default for ProbabilisticPlanner {
    fn default() -> Self {
        ProbabilisticPlanner {
            max_depth: 8,
            mc_sims: 1500,
            max_first: 3,
            rollout_steps: 40,
            decision_threshold: None,
        }
    }
}

impl ProbabilisticPlanner {
    pub fn best_line<R: Rng + ?Sized>(
        &self,
        state: &GameState,
        reg: &Registry,
        payoffs: &[&str],
        rng: &mut R,
    ) -> Line {
        let mut best = Line {
            kind: "probabilistic".into(),
            p_win: 0.0,
            detail: "no winning line found".into(),
            ..Default::default()
        };
        let thr = self.decision_threshold;
        let fired = |best: &Line| thr.map(|t| best.p_win >= t).unwrap_or(false);

        // evaluate: develop candidates rollout
        let mut evaluate = |best: &mut Line, base: &GameState, prefix: &[Action], label: &str, rng: &mut R| {
            let mut cands = loops::develop_candidates(base, reg);
            cands.sort_by(|a, b| {
                let sa = loops::develop_score(base, reg, &a.0);
                let sb = loops::develop_score(base, reg, &b.0);
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            });
            for (card, source) in cands.into_iter().take(self.max_first) {
                if fired(best) {
                    return;
                }
                let est = loops::rollout_estimate(
                    base,
                    reg,
                    (&card, &source),
                    payoffs,
                    self.mc_sims,
                    self.rollout_steps,
                    rng,
                    thr,
                );
                if est.p_win > best.p_win {
                    let winning = est
                        .by_payoff
                        .iter()
                        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
                        .map(|(k, _)| k.clone())
                        .unwrap_or_else(|| "loop".into());
                    let mut actions = prefix.to_vec();
                    actions.push(Action::cast(&card));
                    *best = Line {
                        actions,
                        kind: "probabilistic".into(),
                        p_win: est.p_win,
                        detail: format!("{label}{card} -> {winning} (P={:.3})", est.p_win),
                        base: Some(base.clone()),
                        first: Some((card.clone(), source.clone())),
                        loop_line: false,
                        walkthrough: Vec::new(),
                    };
                }
            }
        };

        let mut eval_loops = |best: &mut Line, base: &GameState, prefix: &[Action], label: &str, rng: &mut R| {
            if base.krark_bodies(reg) < 1 {
                return;
            }
            let payoff_here = payoffs.iter().any(|pf| {
                base.hand.iter().any(|c| c == pf) || base.graveyard.iter().any(|c| c == pf) || base.has_permanent(pf)
            });
            if !payoff_here && !loops_has_burn_engine(base) {
                return;
            }
            let mut engines: Vec<String> = Vec::new();
            for pf in payoffs {
                if base.hand.iter().any(|c| c == pf) || loops::can_escape(base, reg, pf) {
                    engines.push(pf.to_string());
                }
            }
            let mut devs: Vec<String> = loops::develop_candidates(base, reg).into_iter().map(|(c, _)| c).collect();
            devs.sort_by(|a, b| {
                let sa = loops::develop_score(base, reg, a);
                let sb = loops::develop_score(base, reg, b);
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            });
            for c in devs.into_iter().take(2) {
                if !engines.contains(&c) {
                    engines.push(c);
                }
            }
            let n = 60.max(self.mc_sims / 2);
            for card in engines {
                if fired(best) {
                    return;
                }
                let est = loops::estimate_p_lethal(base, reg, &card, payoffs, n, 80, rng, thr);
                if est.p_win > best.p_win {
                    let won = est
                        .by_payoff
                        .iter()
                        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
                        .map(|(k, _)| k.clone())
                        .unwrap_or_else(|| "loop".into());
                    let src = if base.hand.iter().any(|c| c == &card) { "hand" } else { "escape" };
                    let mut actions = prefix.to_vec();
                    actions.push(Action::cast(&card));
                    *best = Line {
                        actions,
                        kind: "probabilistic".into(),
                        p_win: est.p_win,
                        detail: format!("{label}loop {card} -> {won} (P={:.3})", est.p_win),
                        base: Some(base.clone()),
                        first: Some((card.clone(), src.to_string())),
                        loop_line: true,
                        walkthrough: Vec::new(),
                    };
                }
            }
        };

        // develop off current board (tap out)
        let dev_base = tap_out(state);
        evaluate(&mut best, &dev_base, &[], "develop ", rng);
        if !fired(&best) {
            eval_loops(&mut best, &dev_base, &[], "develop ", rng);
        }
        if fired(&best) {
            return best;
        }

        // Underworld Breach combo turn
        let mut breach_base: Option<GameState> = None;
        let mut deployed = false;
        if state.has_permanent("Underworld Breach") {
            breach_base = Some(tap_out(state));
        } else if state.hand.iter().any(|c| c == "Underworld Breach") {
            let mut s = tap_out(state);
            let bcost = s.cast_cost(reg, "Underworld Breach");
            if s.mana.can_pay(&bcost) {
                s.mana.pay(&bcost);
                if let Some(pos) = s.hand.iter().position(|c| c == "Underworld Breach") {
                    s.hand.remove(pos);
                }
                let mut p = Permanent::new("Underworld Breach");
                p.summoning_sick = true;
                s.battlefield.push(p);
                breach_base = Some(s);
                deployed = true;
            }
        }
        if let Some(bb) = breach_base {
            let payoff_here = payoffs.iter().any(|pf| {
                bb.hand.iter().any(|c| c == pf) || bb.graveyard.iter().any(|c| c == pf) || bb.has_permanent(pf)
            });
            // LED (battlefield OR hand) manufactures fuel: cracking it discards the hand into the
            // graveyard, so the Breach line is live even with an empty yard.
            let led_avail = bb.has_permanent("Lion's Eye Diamond")
                || bb.hand.iter().any(|c| c == "Lion's Eye Diamond");
            let enough_fuel = loops::gy_fuel(&bb, None) >= 4 || led_avail;
            if payoff_here && enough_fuel {
                let prefix: Vec<Action> = if deployed { vec![Action::cast("Underworld Breach")] } else { vec![] };
                let lbl = if deployed { "deploy Breach + " } else { "Breach: " };
                evaluate(&mut best, &bb, &prefix, lbl, rng);
                eval_loops(&mut best, &bb, &prefix, lbl, rng);
            }
        }

        // Engine-deploy combo turn
        if !fired(&best) && state.hand.iter().any(|nm| is_engine_permanent(reg, nm)) {
            let (eng_base, deploy_acts) = deploy_engine_base(state, reg);
            if !deploy_acts.is_empty() {
                evaluate(&mut best, &eng_base, &deploy_acts, "deploy engine + ", rng);
                if !fired(&best) {
                    eval_loops(&mut best, &eng_base, &deploy_acts, "deploy engine + ", rng);
                }
            }
        }

        // Ritual prelude (--ritual-prelude): with a Krark body online, try firing a sorcery-speed
        // ritual as the FIRST cast to float enough mana for a payoff this turn (e.g. land-light
        // Jeska's Will). rollout_estimate casts the ritual through the resolver, so its mc_sims
        // average over the flip's win (RRR[+copy]) and whiff (0 mana, card back to hand) — the
        // returned p_win already prices the flip risk. The fizzle is benign (see first_is_prelude_
        // ritual + sim::declare's gate), so a low-odds attempt is "free" under the clone model.
        if ritual_prelude_on() && !fired(&best) && state.krark_bodies(reg) >= 1 {
            let dev_base = tap_out(state);
            let mut seen: HashSet<&str> = HashSet::new();
            for r in &dev_base.hand {
                if !PRELUDE_RITUALS.contains(&r.as_str()) || !seen.insert(r.as_str()) {
                    continue;
                }
                if !dev_base.mana.can_pay(&dev_base.cast_cost(reg, r)) {
                    continue;
                }
                if fired(&best) {
                    break;
                }
                let est = loops::rollout_estimate(
                    &dev_base,
                    reg,
                    (r, "hand"),
                    payoffs,
                    self.mc_sims,
                    self.rollout_steps,
                    rng,
                    thr,
                );
                if est.p_win > best.p_win {
                    let won = est
                        .by_payoff
                        .iter()
                        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
                        .map(|(k, _)| k.clone())
                        .unwrap_or_else(|| "loop".into());
                    best = Line {
                        actions: vec![Action::cast(r)],
                        kind: "probabilistic".into(),
                        p_win: est.p_win,
                        detail: format!("ritual {r} -> {won} (P={:.3})", est.p_win),
                        base: Some(dev_base.clone()),
                        first: Some((r.clone(), "hand".to_string())),
                        loop_line: false,
                        walkthrough: Vec::new(),
                    };
                }
            }
        }

        best
    }
}

fn loops_has_burn_engine(s: &GameState) -> bool {
    ["Urabrask", "Vivi Ornitier"].iter().any(|n| s.has_permanent(n))
}

// --------------------------------------------------------------------------- //
// solve
// --------------------------------------------------------------------------- //

pub fn solve<R: Rng + ?Sized>(
    state: &GameState,
    reg: &Registry,
    payoffs: &[&str],
    det: &DeterministicKillSearch,
    prob: &ProbabilisticPlanner,
    rng: &mut R,
) -> Line {
    let mut s = state.clone();

    let mut sc = s.clone();
    if terminal_value(&mut sc, reg) == Some(1.0) {
        let w = win::evaluate_win(&sc, reg, None);
        let d = if w.detail.is_empty() { "already lethal".to_string() } else { w.detail.clone() };
        return Line {
            kind: "deterministic".into(),
            p_win: 1.0,
            detail: kill_detail(&sc, reg, &d),
            ..Default::default()
        };
    }

    let report = loops::apply_loops(&mut s, reg);
    if !report.confirmed.is_empty() && win::evaluate_win(&s, reg, None).won {
        let w = win::evaluate_win(&s, reg, None);
        return Line {
            kind: "deterministic".into(),
            p_win: 1.0,
            detail: kill_detail(&s, reg, &w.detail),
            ..Default::default()
        };
    }

    if let Some(kill) = det.find_kill(&s, reg) {
        return kill;
    }

    prob.best_line(&s, reg, payoffs, rng)
}

// --------------------------------------------------------------------------- //
// selftest — mirrors planner.py __main__
// --------------------------------------------------------------------------- //

pub fn selftest(reg: &Registry) {
    use crate::game_state::krark_body;
    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::seed_from_u64(0);
    let det = DeterministicKillSearch::default();
    let prob = ProbabilisticPlanner::default();

    // 1) Twinflame + Dualcaster in hand with mana -> deterministic KILL
    {
        let mut s = GameState {
            library: vec!["Island".into(); 40],
            hand: vec!["Twinflame".into(), "Dualcaster Mage".into()],
            battlefield: vec![krark_body("Krark, the Thumbless", None, false)],
            ..Default::default()
        };
        s.mana.add("R", 3);
        s.mana.add("C", 2);
        let line = solve(&s, reg, loops::DEV_PAYOFFS, &det, &prob, &mut rng);
        assert!(line.kind == "deterministic" && line.p_win == 1.0, "{:?}", line);
        println!("[ok] planner 1) deterministic KILL: {}", line.detail);
    }

    // 2) Jeska runaway + payoffs -> probabilistic win > 0.5
    {
        let mut s = GameState {
            library: vec!["Island".into(); 40],
            hand: vec!["Jeska's Will".into(), "Grapeshot".into()],
            battlefield: vec![
                krark_body("Krark, the Thumbless", None, false),
                krark_body("Sakashima of a Thousand Faces", Some("Krark, the Thumbless"), false),
                Permanent { summoning_sick: false, ..Permanent::new("Veyran, Voice of Duality") },
                Permanent { summoning_sick: false, ..Permanent::new("Harmonic Prodigy") },
                Permanent { summoning_sick: false, ..Permanent::new("Archmage Emeritus") },
                Permanent { summoning_sick: false, ..Permanent::new("Storm-Kiln Artist") },
            ],
            ..Default::default()
        };
        s.mana.add("R", 1);
        s.mana.add("C", 2);
        let line = solve(&s, reg, loops::DEV_PAYOFFS, &det, &prob, &mut rng);
        assert!(line.kind == "probabilistic" && line.p_win > 0.5, "p_win={} kind={}", line.p_win, line.kind);
        println!("[ok] planner 2) probabilistic win P={:.3}: {}", line.p_win, line.detail);
    }

    // 3) nothing assembled -> no win
    {
        let s = GameState {
            library: vec!["Island".into(); 40],
            hand: vec!["Ponder".into()],
            battlefield: vec![krark_body("Krark, the Thumbless", None, false)],
            ..Default::default()
        };
        let line = solve(&s, reg, loops::DEV_PAYOFFS, &det, &prob, &mut rng);
        assert!(line.p_win == 0.0, "p_win={}", line.p_win);
        println!("[ok] planner 3) nothing assembled -> P(win)=0.000");
    }

    println!("planner selftest passed.");
}
