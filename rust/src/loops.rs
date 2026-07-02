//! loops.rs — port of loops.py. Runaway / semi-infinite analysis, MC estimators,
//! develop scoring, loop detection.

use crate::cards::{CardType, Registry};
use crate::game_state::{krark_body, plan_phyrexian, GameState, Permanent};
use crate::resolver::{analyze_cast, resolve_cast_sample, untap_mana, Choices, ResolveLog, STORM_SPELLS};
use crate::tables::{is_mana_source, mana_source, SrcMode};
use crate::wishlist;
use crate::win;
use rand::Rng;
use std::collections::HashSet;

pub const DEV_PAYOFFS: &[&str] = &["Grapeshot", "Brain Freeze"];

// Aggressive cantrips: develop_score stops charging the mana cost of CANTRIP_LOOP cards (draw /
// card-selection). Rationale — the deck is mana-saturated (~17% of dev-turn mana floats away unused),
// so the opportunity cost of a cantrip's mana is ~0; a "mana-neutral" dig (pay 1, draw 1) is actually
// positive selection progress and should fire. Temporary-mana rituals are untouched — they hit the
// pure_mana branch and stay binned with the combo pieces. Default ON (A/B at 1200x8: TTK 7.21->7.12,
// P90 11->10, win% flat 99.7->99.6); opt out with --no-aggro-cantrips.
pub static AGGRO_CANTRIPS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
fn aggro_cantrips() -> bool {
    *AGGRO_CANTRIPS.get().unwrap_or(&true)
}

// Pre-Krark digging: credit a cantrip's selection value even with NO Krark body in play (no copies),
// so the pilot digs toward Krark/combo with spare mana on early turns. Without this the dig branch is
// gated on flips_per_cast>=1, so a bare cantrip scores ~mana-neutral and never fires pre-Krark. Layers
// on top of aggro_cantrips (which zeroes the cost so the dig reads positive). Default off (A/B gated).
pub static PRE_KRARK_DIG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
fn pre_krark_dig() -> bool {
    *PRE_KRARK_DIG.get().unwrap_or(&false)
}

// per-resolution red mana the spell's own effect makes.
fn spell_red_per_resolution(card: &str) -> i64 {
    match card {
        "Pyretic Ritual" | "Desperate Ritual" => 3,
        "Rite of Flame" => 2,
        _ => 0,
    }
}
fn spell_generic_per_resolution(card: &str) -> i64 {
    match card {
        "Strike It Rich" => 1,
        // Countering your own already-triggered spell makes you its controller → 2 Treasures.
        "An Offer You Can't Refuse" => 2,
        _ => 0,
    }
}

fn library_reduction(card: &str) -> i64 {
    match card {
        "Brain Freeze" => 3,
        "Frantic Search" => 2,
        // Heroes' Hangout impulse-digs the top two (plays the better one).
        "Heroes' Hangout" => 2,
        "Brainstorm" | "Ponder" | "Gamble" | "Gitaxian Probe" | "Peek" | "Borne Upon a Wind"
        | "Opt" | "Consider" | "Serum Visions" | "Preordain"
        // Red {R}: draw-1 cantrips — same dig value as Opt/Ponder; were missing so the planner
        // scored them negative and refused to fire them for dig.
        | "Overmaster" | "Expedite" | "Might of the Meek"
        | "Crimson Wisps" | "Renegade Tactics" | "Accelerate" => 1,
        _ => 0,
    }
}

const BURN_WEIGHT: f64 = 1.5;
const DIG_WEIGHT: f64 = 1.0;
const TREASURE_BANK_WEIGHT: f64 = 0.5;

const TREASURE_SPELLS: &[&str] = &["Strike It Rich", "An Offer You Can't Refuse"];
const BURN_ENGINES: &[&str] = &["Urabrask", "Vivi Ornitier"];

const PAYOFF_ONLY: &[&str] = &["Grapeshot"];

const MANA_POSITIVE_LOOP: &[&str] = &[
    "Jeska's Will", "Rite of Flame", "Pyretic Ritual", "Desperate Ritual", "Strike It Rich",
];
const CANTRIP_LOOP: &[&str] = &[
    "Brainstorm", "Ponder", "Gitaxian Probe", "Peek", "Frantic Search", "Snap",
    "Borne Upon a Wind", "Opt", "Consider", "Serum Visions", "Preordain",
    "Overmaster", "Expedite", "Might of the Meek", "Heroes' Hangout",
    "Crimson Wisps", "Renegade Tactics", "Accelerate",
    // NOTE: Gamble is deliberately NOT here. It's a tutor with a RANDOM-discard cost, not a free
    // loop cantrip — looping it repeatedly random-discards key cards (Grapeshot, doublers). Gamble is
    // valued as a one-shot tutor in develop_score instead: Gamble once for the best piece, then cast it.
];
// Loopable counters/instants: cast for magecraft/storm value off a per-cast engine. Free ones
// (Pact / Fierce Guardianship / Deflecting Swat / Mogg Salvage) loop for 0 mana; the {U} ones
// (Flusterstorm / An Offer / Cyclonic Rift) need blue, covered by treasure-per-cast. Pact has a
// {3}{U}{U}-next-upkeep death-tax, so a fizzled Pact loop is fatal (handled in sim::declare).
const MAGECRAFT_FUEL: &[&str] = &[
    "Flusterstorm",
    "Deflecting Swat",
    "Fierce Guardianship",
    "Pact of Negation",
    "An Offer You Can't Refuse",
    "Mogg Salvage",
    "Cyclonic Rift",
];
pub fn is_magecraft_fuel(name: &str) -> bool {
    MAGECRAFT_FUEL.contains(&name)
}

const CAST_VALUE_ENGINES: &[&str] = &[
    "Storm-Kiln Artist", "Archmage Emeritus", "Birgi, God of Storytelling", "Urabrask",
    "Tavern Scoundrel", "Vivi Ornitier", "Electro, Assaulting Battery",
];

// --------------------------------------------------------------------------- //
// Breach / Gale graveyard recursion
// --------------------------------------------------------------------------- //

pub fn gy_fuel(s: &GameState, exclude: Option<&str>) -> i64 {
    let mut n = s.graveyard.len() as i64;
    if let Some(ex) = exclude {
        if s.graveyard.iter().any(|c| c == ex) {
            n -= 1;
        }
    }
    n
}

/// Underworld Breach is an enchantment with no triggers, so it is NOT an engine permanent and never
/// gets deployed on its own. It's a combo finisher: once on the battlefield, `can_escape` lets you
/// re-cast a storm payoff (Grapeshot / Brain Freeze) from the graveyard by exiling 3 other gy cards
/// each. Breach sacrifices itself at end of turn, so it's only worth casting when the escape line can
/// go off THIS turn — a payoff already sits in the graveyard, the graveyard is deep enough to pay the
/// exile costs, and a Krark body is out to storm the escaped payoff to lethal. Gating on those keeps
/// the pilot from wasting it speculatively; the kill search then verifies the actual lethal line.
pub fn breach_line_live(s: &GameState, reg: &Registry) -> bool {
    if !s.hand.iter().any(|c| c == "Underworld Breach") {
        return false;
    }
    let payoff_in_gy = s
        .graveyard
        .iter()
        .any(|c| matches!(c.as_str(), "Grapeshot" | "Brain Freeze"));
    payoff_in_gy && s.graveyard.len() >= 7 && s.krark_bodies(reg) >= 1
}

pub fn can_escape(s: &GameState, reg: &Registry, card: &str) -> bool {
    if !s.has_permanent("Underworld Breach") || !s.graveyard.iter().any(|c| c == card) {
        return false;
    }
    if reg.get(card).types.contains(&CardType::Land) {
        return false;
    }
    gy_fuel(s, Some(card)) >= 3
}

pub fn can_gale_recast(s: &GameState, reg: &Registry, card: &str) -> bool {
    if !s.has_permanent("Gale, Waterdeep Prodigy") || !s.graveyard.iter().any(|c| c == card) {
        return false;
    }
    let cd = reg.get(card);
    if !cd.is_instant_or_sorcery() {
        return false;
    }
    let trigger = if cd.types.contains(&CardType::Instant) {
        CardType::Sorcery
    } else {
        CardType::Instant
    };
    s.hand.iter().any(|h| reg.get(h).types.contains(&trigger))
}

pub fn crack_led(s: &mut GameState) -> bool {
    // LED can be cracked from the battlefield, OR cast ({0}) from hand and cracked the same turn
    // (Breach combo: dump the hand into the graveyard to give it escape, and make 3 mana).
    if let Some(i) = s.battlefield.iter().position(|p| p.effective_name() == "Lion's Eye Diamond") {
        s.battlefield.remove(i);
    } else if let Some(i) = s.hand.iter().position(|c| c == "Lion's Eye Diamond") {
        s.hand.remove(i);
    } else {
        return false;
    }
    // cost: "Discard your hand, Sacrifice LED" -> add 3 mana of one color (modeled as wildcard).
    let hand: Vec<String> = std::mem::take(&mut s.hand);
    s.graveyard.extend(hand);
    s.graveyard.push("Lion's Eye Diamond".to_string());
    s.mana.add("*", 3);
    true
}


/// Exile `k` cards from the graveyard as Breach's escape cost. Never the `protect` set (cards
/// needed for THIS go-off); among the rest, exile the LEAST valuable first — same priority as the
/// mulligan/discard logic (resolver::discard_rank: payoffs/combo are ∞-protected, dead cards go
/// first, so we don't bin a ritual or payoff we'd rather re-escape).
pub fn exile_fuel(s: &mut GameState, reg: &Registry, k: i64, protect: &[String]) {
    let rank = |s: &GameState, i: usize| crate::resolver::discard_rank(s, reg, &s.graveyard[i]);
    // candidates outside the protect set, worst (lowest discard_rank) first
    let mut order: Vec<usize> = (0..s.graveyard.len())
        .filter(|&i| !protect.contains(&s.graveyard[i]))
        .collect();
    order.sort_by(|&a, &b| rank(s, a).partial_cmp(&rank(s, b)).unwrap_or(std::cmp::Ordering::Equal));
    let mut to_remove: Vec<usize> = order.into_iter().take(k as usize).collect();
    // not enough non-protected fuel? forced to exile protected cards too (still worst-first)
    if (to_remove.len() as i64) < k {
        let mut rest: Vec<usize> = (0..s.graveyard.len()).filter(|i| !to_remove.contains(i)).collect();
        rest.sort_by(|&a, &b| rank(s, a).partial_cmp(&rank(s, b)).unwrap_or(std::cmp::Ordering::Equal));
        let need = k as usize - to_remove.len();
        to_remove.extend(rest.into_iter().take(need));
    }
    // remove highest index first so earlier indices stay valid
    to_remove.sort_unstable_by(|a, b| b.cmp(a));
    for i in to_remove {
        s.graveyard.remove(i);
    }
}

pub fn breach_led_mana(s: &mut GameState, reg: &Registry, protect: &[String]) -> bool {
    if !can_escape(s, reg, "Lion's Eye Diamond") {
        return false;
    }
    if let Some(pos) = s.graveyard.iter().position(|c| c == "Lion's Eye Diamond") {
        s.graveyard.remove(pos);
    }
    exile_fuel(s, reg, 3, protect);
    s.mana.add("*", 3);
    s.graveyard.push("Lion's Eye Diamond".to_string());
    true
}

pub fn castable_now(s: &GameState, reg: &Registry, name: &str) -> bool {
    s.hand.iter().any(|c| c == name)
        || s.exiled_play.iter().any(|c| c == name)
        || can_escape(s, reg, name)
        || can_gale_recast(s, reg, name)
}

// --------------------------------------------------------------------------- //
// Color conversion
// --------------------------------------------------------------------------- //

fn convert_available(s: &GameState, reg: &Registry, color: &str, need: i64) -> bool {
    let pool = &s.mana;
    let mut have = pool.get(color)
        + pool.get("*")
        + s.mana.treasures;
    if have >= need {
        return true;
    }
    if s.flips_per_cast(reg) >= 1
        && pool.get("R") >= 2
        && TREASURE_SPELLS.iter().any(|c| castable_now(s, reg, c))
    {
        return true;
    }
    let mut spare = s.mana.total() - have;
    for p in &s.battlefield {
        if p.tapped {
            continue;
        }
        // Mox Amber is dead without a legendary creature in play.
        if p.effective_name() == "Mox Amber" && !s.has_legendary_creature() {
            continue;
        }
        if let Some((mode, produced)) = mana_source(p.effective_name()) {
            if matches!(mode, SrcMode::Tap | SrcMode::Sac) {
                have += produced.get("*").copied().unwrap_or(0) + produced.get(color).copied().unwrap_or(0);
            } else if mode == SrcMode::LifeRepeat {
                // Treasonous Ogre: life→mana battery (bounded by the life floor) is real convertible mana.
                let n = ((s.our_life - crate::game_state::LIFE_FLOOR)
                    / crate::tables::LIFE_REPEAT_COST)
                    .max(0);
                have += produced.get(color).copied().unwrap_or(0) * n;
            }
        }
    }
    // Relic of Legends' creature-tap ability: each idle legendary creature is one any-color mana.
    have += s.relic_legend_mana();
    // Vivi Ornitier's once-per-turn {0} ability: add its power (= noncreature spells cast) as U/R.
    have += s.vivi_available_mana();
    // dedup-preserving-order over hand
    let mut seen: HashSet<&str> = HashSet::new();
    for name in &s.hand {
        if !seen.insert(name.as_str()) {
            continue;
        }
        if let Some((_, produced)) = mana_source(name) {
            let yield_ = produced.get("*").copied().unwrap_or(0) + produced.get(color).copied().unwrap_or(0);
            let cost: i64 = s.cast_cost(reg, name).values().sum();
            if yield_ > 0 && spare >= cost {
                have += yield_;
                spare -= cost;
            }
        }
    }
    have >= need
}

// --------------------------------------------------------------------------- //
// Winning payoff
// --------------------------------------------------------------------------- //

/// Return Some(label) of a payoff lethal RIGHT NOW, else None.
pub fn winning_payoff(s: &GameState, reg: &Registry, payoffs: &[&str], need_life: i64) -> Option<String> {
    if need_life > 0 && s.opponent_life.iter().all(|l| *l <= 0) {
        return Some("burn".to_string());
    }
    if s.opponent_library.iter().all(|l| *l <= 0) {
        return Some("mill".to_string());
    }
    if payoffs.contains(&"Grapeshot") && s.storm_count + 1 >= need_life {
        // Full cost, not just the red pip: Grapeshot is {1}{R} (2 mana). convert_available folds in
        // colored/wildcard/treasure/tap/hand sources; requiring the whole cost in-color is conservative
        // (won't over-declare) — the storm kill floats plenty of red/Treasures by the time it fires.
        let cost: i64 = s.cast_cost(reg, "Grapeshot").values().sum();
        if castable_now(s, reg, "Grapeshot") && convert_available(s, reg, "R", cost) {
            return Some("Grapeshot".to_string());
        }
    }
    if payoffs.contains(&"Brain Freeze") {
        let mill_each = 3 * (s.storm_count + 1);
        let libs: Vec<i64> = s.opponent_library.iter().copied().filter(|l| *l > 0).collect();
        let cost: i64 = s.cast_cost(reg, "Brain Freeze").values().sum(); // {1}{U} = 2, not just the U pip
        if !libs.is_empty()
            && mill_each >= *libs.iter().max().unwrap()
            && castable_now(s, reg, "Brain Freeze")
            && convert_available(s, reg, "U", cost)
        {
            return Some("Brain Freeze".to_string());
        }
    }
    // Dualcaster Mage + Twinflame/Molten Duplication = infinite hasty attackers (Dualcaster has flash:
    // cast a shimmer, flash Dualcaster in response copying it, the token Dualcaster re-copies the
    // shimmer, ad infinitum). The deterministic kill search catches this via detect_loops, but the
    // ROLLOUT only consults winning_payoff — so without this it keeps digging (and self-mills the
    // combo away) on a board that's already lethal. Mirror detect_loops's confirmed condition.
    if dualcaster_shimmer_lethal(s, reg) || krark_shimmer_lethal(s, reg) {
        return Some("combat".to_string());
    }
    // Krark + Flare of Duplication = infinite magecraft -> infinite mana/storm (Storm-Kiln) -> burn.
    if flare_magecraft_lethal(s, reg) {
        return Some("Grapeshot".to_string());
    }
    None
}

/// True if Krark + Flare of Duplication can fire a reliable kill now. A Krark win copies the Flare
/// spell; aim the copy at the original Flare still on the stack (copying doesn't consume it), so each
/// resolution spawns another Flare copy = infinite MAGECRAFT. Note: copies are NOT cast, so the loop
/// raises magecraft, NOT storm. Seeding it just needs to WIN ONE flip (P(win >=1) = 1 - 1/2^n over n
/// Krark triggers) — reliable with Krark's Thumb OR enough triggers (>=3 flips ~= 87.5%), the same
/// bar the krark-shimmer line uses; no Thumb required. Two payoff routes turn the unbounded magecraft
/// into a kill:
///  - Storm-Kiln Artist: Treasure per magecraft = infinite MANA, which fuels Krark recasts (each
///    recast a real cast -> +1 storm) to pump a castable Grapeshot / Brain Freeze to lethal.
///  - Archmage Emeritus: a card per magecraft = draw the whole library, so the finisher only needs
///    to EXIST (incl. still in library) — the loop draws it plus the mana to cast it.
fn flare_magecraft_lethal(s: &GameState, reg: &Registry) -> bool {
    if !(s.has_krarks_thumb() || s.flips_per_cast(reg) >= 3) {
        return false;
    }
    // Castable this turn: hand, Jeska's-Will exile, or a graveyard escape.
    let castable = |name: &str| {
        s.hand.iter().any(|c| c == name)
            || s.exiled_play.iter().any(|c| c == name)
            || (s.graveyard.iter().any(|c| c == name) && can_escape(s, reg, name))
    };
    if !castable("Flare of Duplication")
        || !s.mana.can_pay(&s.cast_cost(reg, "Flare of Duplication"))
    {
        return false;
    }
    let finisher_castable = castable("Grapeshot") || castable("Brain Freeze");
    // Archmage draws the whole library, so a finisher anywhere reachable (incl. library) is enough.
    let finisher_reachable = finisher_castable
        || ["Grapeshot", "Brain Freeze"].iter().any(|f| s.library.iter().any(|c| c == f));
    (s.has_permanent("Storm-Kiln Artist") && finisher_castable)
        || (s.has_permanent("Archmage Emeritus") && finisher_reachable)
}

/// True if the Krark + Sakashima + Twinflame/Molten Duplication engine makes INFINITE hasty Krarks now:
/// cast a shimmer at Krark and flip for one LOSS (Krark returns the shimmer to hand) + wins (each
/// copy a token Krark). Sakashima's legend-rule break lets the tokens stick; a renewable mana source
/// — a per-cast engine, or a loopable ritual in hand (the Krark return-trick loops it for net mana) —
/// refunds the {1}{R} recast. Krark's Thumb isn't required; it only makes the 1-loss steer reliable,
/// and the split self-corrects as the army grows. Army grows unbounded -> lethal combat.
fn krark_shimmer_lethal(s: &GameState, reg: &Registry) -> bool {
    let on_board = s.has_sakashima_break();
    // Deployable path is flag-gated: `s.sakashima_cmd` is true ONLY under `--sak-deploy` (a command-
    // zone Sakashima exposed by build_state); it is always false otherwise, so with the flag off every
    // branch below collapses to the original on-board behavior — byte-identical baseline.
    let deployable = s.sakashima_cmd && !on_board;
    if !on_board && !deployable {
        return false;
    }
    // Reliability gate (not a piece requirement): the loop hinges on steering each cast to a 1-loss/
    // rest-win split. Krark's Thumb makes that near-certain; without it, >=3 FLIPS per cast already
    // keeps each cast >=87% to continue and the split self-corrects as the army grows — so treat it as
    // lethal at Thumb OR >=3 flips. Flips (bodies × (1+doublers)), not raw bodies: 2 bodies + Roaming
    // Throne = 4 flips is strictly more reliable than 3 bodies bare (audit 2026-07-01, seed 428).
    // Deployable path: accept >=1 body — with Sakashima + renewable mana the hasty-Krark army grows
    // ~E[wins]/cast in expectation and self-corrects, and a goldfish turn has no disruption, so even a
    // slow-growing army reaches lethal. This only fires once the (real) mana check below passes.
    let bodies_ok = if on_board {
        s.has_krarks_thumb() || s.flips_per_cast(reg) >= 3
    } else {
        s.krark_bodies(reg) >= 1
    };
    if !bodies_ok {
        return false;
    }
    // Renewable mana to refund the {1}{R} shimmer recast each loop. Storm-Kiln (Treasure per
    // magecraft — copies count) and Tavern Scoundrel (2 Treasures per flip-win) make the recast free
    // at one copy. Birgi / Urabrask add only {R} per CAST, so one is net -1 — but the shimmer copies
    // a CREATURE, so you steer one win to copy Birgi/Urabrask itself → two → +2 red/cast = free, then
    // pile up token Krarks. So any one engine piece + a few Krarks bootstraps it. A loopable ritual in
    // hand loops for net mana via the Krark return-trick.
    let mut sustain = ["Storm-Kiln Artist", "Tavern Scoundrel", "Birgi, God of Storytelling", "Urabrask"]
        .iter()
        .any(|e| s.has_permanent(e))
        || ["Brightstone Ritual", "Pyretic Ritual", "Desperate Ritual", "Rite of Flame"]
            .iter()
            .any(|r| s.hand.iter().any(|c| c == r));
    // Deployable path: Sakashima enters as a copy of Krark, so the army starts at 2 bodies (never 1)
    // and grows ~geometrically — each cast's Krark triggers copy the shimmer into more token Krarks,
    // so flips scale with the army and it reaches lethal in ~7-11 casts. The only real yard risk (all
    // flips win -> shimmer resolves to the graveyard instead of returning to hand, P=0.5^flips) is
    // front-loaded on the FIRST cast. Declaring P=1.0 is honest only if that cast-1 yard is negligible
    // or can be CONTINUED past:
    //   - post-deploy flips >= 4  -> cast-1 yard <= 6% and geometric growth carries it (~93%+), or
    //   - Krark's Thumb           -> never yards (steer to exactly one loss), or
    //   - Underworld Breach in play -> re-escape the yarded shimmer from the graveyard, or
    //   - a SECOND shimmer accessible -> switch to it (army already bigger, so it's ~safe).
    // A 2-body / no-doubler / single-shimmer / no-Breach line is only ~70% -> not booked as certain.
    if deployable {
        let post_flips = (s.krark_bodies(reg) + 1) * (1 + s.trigger_doublers(reg));
        let shimmers = crate::cards::SHIMMERS
            .iter()
            .filter(|sh| {
                s.hand.iter().any(|c| &c.as_str() == *sh)
                    || s.exiled_play.iter().any(|c| &c.as_str() == *sh)
                    || can_escape(s, reg, sh)
            })
            .count();
        let reliable = post_flips >= 4
            || s.has_krarks_thumb()
            || s.has_permanent("Underworld Breach")
            || shimmers >= 2;
        if !reliable {
            return false;
        }
        // Renewable mana to fund the ~10 {1}{R} shimmer recasts: a loopable, net-positive Jeska's Will
        // (returns to hand on a lost flip, +~3 mana/cast) or LED re-escaped via Breach both sustain it.
        if !sustain {
            sustain = castable_now(s, reg, "Jeska's Will") || can_escape(s, reg, "Lion's Eye Diamond");
        }
    }
    // Shimmer access + the one-time Sakashima deploy cost, shared by the finite-mana pump below and
    // the final castability check.
    let sak_cost: crate::cards::ManaCost = if deployable {
        s.cast_cost(reg, "Sakashima of a Thousand Faces").into_owned()
    } else {
        crate::cards::ManaCost::new()
    };
    let access = |name: &str| {
        s.hand.iter().any(|c| c == name)
            || s.exiled_play.iter().any(|c| c == name)
            || (deployable && can_escape(s, reg, name))
    };
    if !sustain {
        // Finite-mana pump (audit 2026-07-01, seed 428): with NO renewable engine, a big-enough
        // BANKED pool still goes lethal — each recast's flip wins are token KRARKS (bodies!), so
        // flips scale with the army and it grows ~1.5-2x per cast. Casts are bounded by pool mana;
        // simulate the discounted expected pump and book the kill only when the army's combat power
        // clears the pod's combined life before the mana runs out, AND the loop is reliable (same
        // menu as the deployable gate: cast-1 yard is the front-loaded risk).
        let mc_total = |c: &crate::cards::ManaCost| -> i64 {
            c.iter().filter(|(k, _)| k.as_str() != "X").map(|(_, v)| *v).sum()
        };
        let doub = 1 + s.trigger_doublers(reg);
        let bodies0 = s.krark_bodies(reg) + if deployable { 1 } else { 0 };
        let flips0 = bodies0 * doub;
        let thumb = s.has_krarks_thumb();
        let n_shimmers = crate::cards::SHIMMERS.iter().filter(|sh| access(sh)).count();
        if !(thumb || flips0 >= 4 || s.has_permanent("Underworld Breach") || n_shimmers >= 2) {
            return false;
        }
        // Cheapest accessible shimmer funds the recasts (each needs one R pip: cap casts by
        // red-capable mana — R + wildcard + Treasures — minus one the deploy's generic may eat).
        let recast = crate::cards::SHIMMERS
            .iter()
            .filter(|sh| access(sh))
            .map(|sh| mc_total(s.cast_cost(reg, sh).as_ref()))
            .min()
            .unwrap_or(i64::MAX);
        if recast == i64::MAX {
            return false;
        }
        let pool = s.mana.total() - mc_total(&sak_cost);
        let red_avail = s.mana.slots[3] + s.mana.slots[6] + s.mana.treasures
            - if deployable { 1 } else { 0 };
        let max_casts = (pool / recast.max(1)).min(red_avail).max(0);
        // Expected pump, conservatively discounted: no Thumb -> 0.45 wins/flip (under the true 0.5);
        // Thumb -> steer to exactly one loss (flips-1 wins). Token Krarks have power 1 and haste;
        // lethal = the pod's combined remaining life.
        let lethal = s.opponent_life.iter().sum::<i64>() as f64;
        let mut bodies = bodies0 as f64;
        let mut power = 0.0;
        let mut dead = false;
        for _ in 0..max_casts {
            let flips = bodies * doub as f64;
            let wins = if thumb { (flips - 1.0).max(0.0) } else { flips * 0.45 };
            power += wins;
            bodies += wins;
            if power >= lethal {
                dead = true;
                break;
            }
        }
        if !dead {
            return false;
        }
    }
    // The shimmer must be castable from hand / Jeska-exile (or, on the deployable path, escapable via
    // Breach), and the pool must cover the first shimmer cast PLUS the one-time Sakashima {3}{U} deploy.
    crate::cards::SHIMMERS.iter().any(|sh| {
        access(sh) && s.mana.can_pay(&add_costs(&[&sak_cost, s.cast_cost(reg, sh).as_ref()]))
    })
}

/// True if Dualcaster Mage + a Twinflame/Molten Duplication can fire the infinite-hasty-attackers combo
/// right now: two Dualcaster bodies already (loop established), or Dualcaster in hand with a shimmer
/// in hand (or escapable) and mana for both. Lean inline mirror of detect_loops's confirmed branch
/// for the rollout's hot path (full Gale/Breach routes are re-verified by the commit-time solve).
fn dualcaster_shimmer_lethal(s: &GameState, reg: &Registry) -> bool {
    if s.battlefield.iter().filter(|p| p.effective_name() == "Dualcaster Mage").count() >= 2 {
        return true;
    }
    // Accessible to cast this turn: hand, Jeska's-Will exile ("you may play it" -> exiled_play), or
    // a graveyard escape. exiled_play matters because a deep go-off casts Jeska's Will repeatedly and
    // can strand a combo piece there — castable, but invisible to a hand-only check.
    let access = |name: &str| {
        s.hand.iter().any(|c| c == name)
            || s.exiled_play.iter().any(|c| c == name)
            || (s.graveyard.iter().any(|c| c == name) && can_escape(s, reg, name))
    };
    if !access("Dualcaster Mage") {
        return false;
    }
    let dc_cost = s.cast_cost(reg, "Dualcaster Mage");
    crate::cards::SHIMMERS.iter().any(|sh| {
        access(sh) && {
            let sh_cost = s.cast_cost(reg, sh);
            s.mana.can_pay(&add_costs(&[sh_cost.as_ref(), dc_cost.as_ref()]))
        }
    })
}

// --------------------------------------------------------------------------- //
// _do_cast / _cast_source
// --------------------------------------------------------------------------- //

/// Cast `card` from `source` ('hand' | 'escape' | 'gale'), paying mana (refuel via LED).
/// Returns Some((new_state, log)) or None on mana ruin. Works on a private clone.
pub fn do_cast<R: Rng + ?Sized>(
    s: &GameState,
    reg: &Registry,
    card: &str,
    source: &str,
    rng: &mut R,
    payoffs: &[&str],
) -> Option<(GameState, ResolveLog)> {
    let mut s = s.clone();
    let mut keep: Vec<String> = payoffs.iter().map(|p| p.to_string()).collect();
    keep.push(card.to_string());
    // Phyrexian pips: pay 2 life each by default, or route to colored mana when life is low.
    let (extra_mana, life_pay) = plan_phyrexian(&reg.get(card).phyrexian, s.our_life);
    let mut cost = s.cast_cost(reg, card).into_owned();
    for (k, v) in &extra_mana {
        *cost.entry(k.clone()).or_insert(0) += v;
    }
    // A flashback / jump-start recast pays its graveyard cost, not the card's normal cost.
    if source == "flashback" {
        if let Some((fb, _)) = flashback_cost(card) {
            cost = fb;
        }
    }

    if source == "escape" || source == "gale" || source == "flashback" {
        if let Some(pos) = s.graveyard.iter().position(|c| c == card) {
            s.graveyard.remove(pos);
        }
    }
    while !s.mana.can_pay(&cost) && (breach_led_mana(&mut s, reg, &keep) || s.vivi_mana()) {}
    if !s.mana.can_pay(&cost) {
        return None;
    }
    s.mana.pay(&cost);
    s.our_life -= life_pay;
    if source == "hand" {
        if let Some(pos) = s.hand.iter().position(|c| c == card) {
            s.hand.remove(pos);
        }
    } else if source == "escape" {
        exile_fuel(&mut s, reg, 3, &keep);
    } else if source == "flashback" {
        // jump-start additionally costs discarding a card
        if let Some((_, true)) = flashback_cost(card) {
            crate::resolver::pitch_worst(&mut s, reg, 1);
        }
    }

    let mut choices = Choices::default();
    if card == "Brain Freeze" {
        // Self-mill (own library -> graveyard) is only correct for the Underworld Breach line, where
        // it feeds escape fuel + storm. Otherwise self-milling is too greedy — it buries undrawn combo
        // pieces (e.g. a Twinflame the go-off still needs). Default to milling OPPONENTS (toward a
        // mill-kill); only self-mill when Underworld Breach is in play or in hand.
        let breach = s.has_permanent("Underworld Breach")
            || s.hand.iter().any(|c| c == "Underworld Breach");
        choices.target = Some(if breach { "self".into() } else { "opponents".into() });
    }
    let log = resolve_cast_sample(&mut s, reg, card, rng, &choices, None);
    if source == "gale" {
        if let Some(pos) = s.graveyard.iter().position(|c| c == card) {
            s.graveyard.remove(pos);
            s.exile.push(card.to_string());
        }
    } else if source == "flashback" {
        // Flashback / jump-start EXILE when the spell would leave the stack: whether it resolved
        // (now in the graveyard) OR Krark lost the flip and returned it (now in hand). Either way it
        // goes to exile — it cannot loop or return to hand. [user rules note]
        if let Some(pos) = s.hand.iter().position(|c| c == card) {
            s.hand.remove(pos);
        } else if let Some(pos) = s.graveyard.iter().position(|c| c == card) {
            s.graveyard.remove(pos);
        }
        s.exile.push(card.to_string());
    }
    Some((s, log))
}

/// Native flashback / jump-start recast: (graveyard cost, needs-a-card-to-discard). Both keywords
/// EXILE the card when it would leave the stack — including a Krark return-to-hand — so unlike an
/// escape/hand cast, a flashback cast can't loop or go back to hand. Handled in `do_cast`.
fn flashback_cost(card: &str) -> Option<(std::collections::HashMap<String, i64>, bool)> {
    let mk = |pairs: &[(&str, i64)]| -> std::collections::HashMap<String, i64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    };
    match card {
        "Strike It Rich" => Some((mk(&[("generic", 2), ("R", 1)]), false)), // Flashback {2}{R}
        "Quasiduplicate" => Some((mk(&[("generic", 1), ("U", 2)]), true)),  // Jump-start (+discard)
        _ => None,
    }
}

pub fn can_flashback(s: &GameState, reg: &Registry, card: &str) -> bool {
    let needs_discard = match flashback_cost(card) {
        Some((_, d)) => d,
        None => return false,
    };
    s.graveyard.iter().any(|c| c == card)
        && !reg.get(card).types.contains(&CardType::Land)
        && !(needs_discard && s.hand.is_empty()) // jump-start needs a card to pitch
}

pub fn cast_source(s: &GameState, reg: &Registry, card: &str) -> Option<String> {
    if s.hand.iter().any(|c| c == card) {
        return Some("hand".to_string());
    }
    if can_gale_recast(s, reg, card) {
        return Some("gale".to_string());
    }
    if can_escape(s, reg, card) {
        return Some("escape".to_string());
    }
    // Flashback/jump-start last: it's a one-shot (exiles), so prefer a loopable escape when Breach is out.
    if can_flashback(s, reg, card) {
        return Some("flashback".to_string());
    }
    None
}

fn has_burn_engine(s: &GameState) -> bool {
    BURN_ENGINES.iter().any(|n| s.has_permanent(n))
}

/// Gut Shot is free (Phyrexian) and deals 1 damage per resolution. With a Krark flip engine
/// each cast is a fresh burst and a fully-lost cast returns it to hand for a free recast, so it
/// loops like a burn engine — credit its per-cast damage even without Urabrask/Vivi.
fn loopable_burn_finisher(s: &GameState, reg: &Registry, card: &str) -> bool {
    card == "Gut Shot"
        && s.flips_per_cast(reg) >= 1
        && s.cast_cost(reg, card).values().sum::<i64>() == 0
}

// --------------------------------------------------------------------------- //
// estimate_p_lethal
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, Default)]
pub struct LethalEstimate {
    pub p_win: f64,
    pub by_payoff: Vec<(String, f64)>,
    pub p_deckout_no_win: f64,
    pub mean_chain_len: f64,
    pub need_life_for_grapeshot: i64,
}

pub fn estimate_p_lethal<R: Rng + ?Sized>(
    state: &GameState,
    reg: &Registry,
    engine_card: &str,
    payoffs: &[&str],
    n_sims: i64,
    max_iters: i64,
    rng: &mut R,
    decision_threshold: Option<f64>,
) -> LethalEstimate {
    use std::collections::HashMap;
    let mut wins: HashMap<String, i64> = HashMap::new();
    let mut any_wins = 0i64;
    let mut deckouts = 0i64;
    let mut chain_total = 0i64;
    let mut chain_n = 0i64;
    let need_life: i64 = state.opponent_life.iter().copied().filter(|l| *l > 0).sum();
    let need = decision_threshold.map(|t| t * n_sims as f64);

    let mut ran = 0i64;
    for _ in 0..n_sims {
        let mut s = state.clone();
        if s.has_permanent("Underworld Breach") {
            crack_led(&mut s);
        }
        let mut iters = 0i64;
        let mut won_with = winning_payoff(&s, reg, payoffs, need_life);
        while won_with.is_none() && iters < max_iters {
            let source = match cast_source(&s, reg, engine_card) {
                Some(src) => src,
                None => break,
            };
            match do_cast(&s, reg, engine_card, &source, rng, payoffs) {
                Some((ns, _)) => s = ns,
                None => break,
            }
            iters += 1;
            won_with = winning_payoff(&s, reg, payoffs, need_life);
        }
        if won_with.is_none() && iters >= max_iters && has_burn_engine(&s)
            && s.opponent_life.iter().any(|l| *l > 0)
        {
            won_with = Some("burn".to_string());
        }
        chain_total += iters;
        chain_n += 1;
        if let Some(w) = won_with {
            *wins.entry(w).or_insert(0) += 1;
            any_wins += 1;
        } else if s.library.is_empty() {
            deckouts += 1;
        }
        ran += 1;
        if let Some(need) = need {
            if any_wins as f64 >= need {
                break;
            }
            if any_wins as f64 + ((n_sims - ran) as f64) < need {
                break;
            }
        }
    }

    let r = ran.max(1) as f64;
    let by_payoff: Vec<(String, f64)> =
        wins.into_iter().filter(|(_, v)| *v > 0).map(|(k, v)| (k, v as f64 / r)).collect();
    LethalEstimate {
        p_win: if ran > 0 { any_wins as f64 / r } else { 0.0 },
        by_payoff,
        p_deckout_no_win: if ran > 0 { deckouts as f64 / r } else { 0.0 },
        mean_chain_len: if chain_n > 0 { chain_total as f64 / chain_n as f64 } else { 0.0 },
        need_life_for_grapeshot: need_life,
    }
}

// --------------------------------------------------------------------------- //
// develop scoring
// --------------------------------------------------------------------------- //

pub fn develop_candidates(s: &GameState, reg: &Registry) -> Vec<(String, String)> {
    let has_creature = s
        .battlefield
        .iter()
        .any(|p| p.functions_as(reg).types.contains(&CardType::Creature));
    let ok = |c: &str| -> bool {
        let cd = reg.get(c);
        if !cd.is_instant_or_sorcery() {
            return false;
        }
        // PAYOFF_ONLY (Grapeshot) are normally one-shot payoffs, not develop/rollout casts.
        // EXCEPTION — Grapeshot LOOPS with Krark's Thumb + a body: you steer one flip to a LOSS to
        // return it to hand, so each recast deals (resolutions + storm) damage and comes back. Then
        // it's a loopable burn engine, and the rollout can ride the loop to lethal at a far lower
        // per-cast storm than winning_payoff's single-cast check requires. The Thumb gate ensures the
        // return is steerable (it can't be stranded in the graveyard).
        let grapeshot_loops = c == "Grapeshot" && s.has_krarks_thumb() && s.krark_bodies(reg) >= 1;
        if PAYOFF_ONLY.contains(&c) && !grapeshot_loops {
            return false;
        }
        if MAGECRAFT_FUEL.contains(&c) {
            // A counter (or Cyclonic Rift) needs a legal TARGET — a spell on the stack. In a solitaire
            // goldfish there are no opponent spells, and the counters can't bootstrap targets from
            // EACH OTHER: a copy that wins its Krark flip resolves before it can be re-countered, so
            // they consume rather than sustain. So the loop needs (a) >=2 counters AND (b) a NON-counter
            // spell to SEED the stack — a storm payoff / cantrip / ritual whose Krark copies become the
            // perpetual targets the counter points at while it bounces around for magecraft value.
            // [fix 2026-06-23: dropped single-counter-into-empty-stack. fix 2026-06-27: + seed source.]
            let two_counters =
                s.hand.iter().filter(|h| MAGECRAFT_FUEL.contains(&h.as_str())).count() >= 2;
            let has_seed = s.hand.iter().any(|h| {
                reg.get(h).is_instant_or_sorcery() && !MAGECRAFT_FUEL.contains(&h.as_str())
            });
            if !two_counters || !has_seed {
                return false;
            }
            // The loop only sustains if the RIGHT-color mana is there each cast. Free fuel needs
            // none; the {U} fuel needs blue, which treasures (any-color, made per cast by the
            // engine) can convert to. A red-only engine (Birgi/rituals) can't fuel a blue counter.
            let u_need = cd.blue_pips();
            if u_need > 0 && !convert_available(s, reg, "U", u_need) {
                return false;
            }
            return true;
        }
        crate::cards::castable_in_solitaire(c, has_creature)
    };

    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for c in &s.hand {
        if !seen.contains(c) && ok(c) {
            out.push((c.clone(), "hand".to_string()));
            seen.insert(c.clone());
        }
    }
    // set(graveyard) — order from a set is arbitrary in Python; we use a stable dedup.
    let mut gy_seen: HashSet<&str> = HashSet::new();
    for c in &s.graveyard {
        if !gy_seen.insert(c.as_str()) {
            continue;
        }
        if seen.contains(c) || !ok(c) {
            continue;
        }
        let src = if can_gale_recast(s, reg, c) {
            Some("gale")
        } else if can_escape(s, reg, c) {
            Some("escape")
        } else {
            None
        };
        if let Some(src) = src {
            out.push((c.clone(), src.to_string()));
            seen.insert(c.clone());
        }
    }
    out
}

fn finish_progress(
    s: &GameState,
    reg: &Registry,
    card: &str,
    e_damage: f64,
    e_effect_resolutions: f64,
) -> f64 {
    let need_life: i64 = s.opponent_life.iter().copied().filter(|l| *l > 0).sum();
    if e_damage > 0.0 && need_life > 0 {
        if has_burn_engine(s) || e_damage >= need_life as f64 || loopable_burn_finisher(s, reg, card) {
            return e_damage.min(need_life as f64) * BURN_WEIGHT;
        }
        return 0.0;
    }
    // Cantrips dig for value once a Krark body is out (flips multiply each draw). Before that they
    // normally score 0 — held for the go-off, where they draw many instead of one. EXCEPTION: genuine
    // mana-screw (no body AND <2 mana sources in play) — then durdling is worse than spending a
    // cantrip to find a land, so credit the dig to climb out of the screw.
    let screwed = s.flips_per_cast(reg) < 1
        && s.battlefield
            .iter()
            .filter(|p| crate::tables::mana_source(p.effective_name()).is_some())
            .count()
            < 2
        // ...and no land/rock in hand to fix it naturally. Otherwise it's just a normal early turn
        // (you'll add mana next turn), and the cantrip is worth more held for the go-off.
        && !s.hand.iter().any(|c| crate::tables::mana_source(c).is_some());
    if CANTRIP_LOOP.contains(&card) && (s.flips_per_cast(reg) >= 1 || pre_krark_dig() || screwed) {
        return library_reduction(card) as f64 * e_effect_resolutions * DIG_WEIGHT;
    }
    0.0
}

const SINK_PAYOFFS: &[&str] = &["Grapeshot", "Brain Freeze"];

fn sink_perm(name: &str) -> bool {
    // wishlist._BODIES | _DOUBLERS | _DRAW_ENGINES | _MANA_ENGINES | extra set
    wishlist::is_sink_perm(name)
}

fn has_mana_sink(s: &GameState) -> bool {
    if SINK_PAYOFFS
        .iter()
        .any(|pf| s.hand.iter().any(|c| c == pf) || s.graveyard.iter().any(|c| c == pf) || s.has_permanent(pf))
    {
        return true;
    }
    if has_burn_engine(s) {
        return true;
    }
    s.hand.iter().any(|c| sink_perm(c))
}

pub fn develop_score(s: &GameState, reg: &Registry, card: &str) -> f64 {
    let a = analyze_cast(s, reg, card);
    if card == "Quasiduplicate" {
        return crate::resolver::quasi_value(s, reg, a.e_effect_resolutions);
    }
    if crate::cards::SHIMMERS.contains(&card) {
        return crate::resolver::shimmer_value(s, reg, a.e_effect_resolutions);
    }
    if card == "Gamble" {
        // tutor: best card it can find minus expected random discard loss
        let best = s
            .library
            .iter()
            .map(|c| wishlist::card_value(s, reg, c, true))
            .fold(0.0f64, f64::max);
        let avg_loss = if s.hand.is_empty() {
            0.0
        } else {
            s.hand.iter().map(|c| wishlist::card_value(s, reg, c, false)).sum::<f64>() / s.hand.len() as f64
        };
        let discards = a.e_effect_resolutions.max(1.0);
        return (best - avg_loss * discards) / 20.0 + a.e_draws;
    }
    if card == "Mystical Tutor" {
        // Puts the best I/S on TOP of the library (not in hand). Value = how much it improves the
        // NEXT draw over whatever is currently on top, plus the engine draws from the copies. Once
        // the best card is already on top, best - top = 0, so re-casting scores ~0 and the pilot
        // won't loop it pointlessly (the spell shuffles, so a looped tutor just re-seats the same
        // card). Without this it scored negative and never fired a 92%-meta-staple tutor.
        let best = s
            .library
            .iter()
            .filter(|c| reg.get(c).is_instant_or_sorcery())
            .map(|c| wishlist::card_value(s, reg, c, true))
            .fold(0.0f64, f64::max);
        let top = s
            .library
            .first()
            .map(|c| wishlist::card_value(s, reg, c, true))
            .unwrap_or(0.0);
        return (best - top).max(0.0) / 20.0 + a.e_draws;
    }
    if card == "Step Through" {
        // Wizardcycle: fetch the best library Wizard (single tutor, no Krark trigger).
        const WIZARDS: &[&str] = &[
            "Dualcaster Mage", "Veyran, Voice of Duality", "Vivi Ornitier", "Archmage Emeritus",
            "Snapcaster Mage", "Gale, Waterdeep Prodigy", "Spellseeker",
        ];
        let best = s
            .library
            .iter()
            .filter(|c| WIZARDS.contains(&c.as_str()))
            .map(|c| wishlist::card_value(s, reg, c, true))
            .fold(0.0f64, f64::max);
        return best / 20.0;
    }
    let mut mana: f64 = a.e_mana.values().sum::<f64>() + a.e_treasures;
    let mut own = (spell_red_per_resolution(card)
        + spell_generic_per_resolution(card)
        + untap_mana(s, reg, card)) as f64;
    if card == "Jeska's Will" {
        own += s.opponent_hand.iter().copied().max().unwrap_or(0) as f64;
    }
    // Brightstone Ritual makes {R} per Goblin = per Krark body — a scaling ritual that's mana-positive
    // with >=2 bodies (and snowballs with more). Without this it reads as a 0-mana card and gets pitched.
    if card == "Brightstone Ritual" {
        own += s.krark_bodies(reg) as f64;
    }
    mana += own * a.e_effect_resolutions;
    let mut cost: i64 = s.cast_cost(reg, card).iter().filter(|(k, _)| k.as_str() != "X").map(|(_, v)| *v).sum();
    // Mana-saturated deck: a cantrip's mana would otherwise be wasted, so don't let its cost cancel
    // its card-selection value — charge it 0 and let the dig fire. Rituals never reach here (pure_mana).
    if aggro_cantrips() && CANTRIP_LOOP.contains(&card) {
        cost = 0;
    }
    let finish = finish_progress(s, reg, card, a.e_damage, a.e_effect_resolutions);
    let treasures_made = a.e_treasures
        + if TREASURE_SPELLS.contains(&card) { a.e_effect_resolutions } else { 0.0 };
    let pure_mana = a.e_draws == 0.0
        && finish == 0.0
        && library_reduction(card) == 0
        && a.e_damage == 0.0;
    if pure_mana && !has_mana_sink(s) {
        return if treasures_made > 0.0 { treasures_made * TREASURE_BANK_WEIGHT } else { -1.0 };
    }
    // Among equal-dig cantrips (cost waived above), prefer the CHEAPER one: casting {0} before
    // {1}{U} keeps the loop mana-positive (a treasure engine refunds the cheaper cast in full),
    // and saves the surplus for the next play. Small coefficient — a tiebreak that never reorders
    // the coarse dig-value tiers (e_draws ~1 each).
    let cheap_tiebreak = if aggro_cantrips() && CANTRIP_LOOP.contains(&card) {
        reg.get(card).mana_value as f64 * 0.15
    } else {
        0.0
    };
    (mana - cost as f64) + a.e_draws + finish - cheap_tiebreak
}

pub fn max_draws(s: &GameState, reg: &Registry, card: &str) -> f64 {
    let cd = reg.get(card);
    if !cd.is_instant_or_sorcery() {
        return 0.0;
    }
    let f = s.flips_per_cast(reg);
    let storm = if STORM_SPELLS.contains(&card) { s.storm_count } else { 0 };
    let max_copies = if f > 0 { 1 + f + storm } else { 1 + storm };
    let mut total = 0.0f64;
    for (_idx, eng) in s.value_engines(reg) {
        let mult = s.value_multiplier(eng, true);
        let events = match eng.trigger_cause.as_deref() {
            Some("is_cast_or_copy") => max_copies,
            Some("is_cast") | Some("spell_cast") => 1,
            Some("coin_flip_win") => f,
            _ => 0,
        };
        total += (eng.draw_per_trigger * mult * events) as f64;
    }
    total += (library_reduction(card) * max_copies) as f64;
    total
}

// --------------------------------------------------------------------------- //
// rollout
// --------------------------------------------------------------------------- //

/// One-line trace of a single cast (verbose diag only). `tag` distinguishes go-off ("FLIP") from
/// develop ("DEV ") casts.
pub fn trace_cast_line(tag: &str, step: i64, card: &str, log: &ResolveLog, pre: &GameState, post: &GameState) -> String {
    // Flip outcome right after the card name: (heads won / coins flipped). Krark's Thumb flips two
    // coins per body and keeps one, so a 3-body cast is "(W/6)".
    let flip = if log.flips > 0 {
        format!(" ({}/{})", log.wins, log.flips)
    } else {
        String::new()
    };
    let res = if log.flips > 0 {
        format!("{} resolutions", log.resolutions)
    } else {
        "1 resolution".to_string()
    };
    // Effect deltas (pre -> post): what the spell actually did.
    let mut fx: Vec<String> = Vec::new();
    // Cards that ENTERED hand this cast (draws + hand-tutors like Gamble), by name.
    let mut pre_ct: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
    for c in &pre.hand {
        *pre_ct.entry(c.as_str()).or_insert(0) += 1;
    }
    let mut added: Vec<String> = Vec::new();
    for c in &post.hand {
        let e = pre_ct.entry(c.as_str()).or_insert(0);
        if *e > 0 {
            *e -= 1;
        } else {
            added.push(c.clone());
        }
    }
    if !added.is_empty() {
        let shown = if added.len() > 10 {
            format!("{}, +{} more", added[..10].join(", "), added.len() - 10)
        } else {
            added.join(", ")
        };
        fx.push(format!("drew [{shown}]"));
    }
    // Library reduction beyond what entered hand = impulse-exiled / self-milled this cast.
    let lib_drop = pre.library.len() as i64 - post.library.len() as i64;
    let impulse = lib_drop - added.len() as i64;
    if impulse > 0 {
        fx.push(format!("impulsed/milled-self {impulse}"));
    }
    let live = |g: &GameState| g.opponent_life.iter().filter(|l| **l > 0).sum::<i64>();
    let dmg = live(pre) - live(post);
    if dmg > 0 {
        fx.push(format!("dealt {dmg}"));
    }
    let milled = pre.opponent_library.iter().sum::<i64>() - post.opponent_library.iter().sum::<i64>();
    if milled > 0 {
        fx.push(format!("milled {milled}"));
    }
    let dtreas = post.mana.treasures - pre.mana.treasures;
    if dtreas > 0 {
        fx.push(format!("+{dtreas} treasure"));
    }
    if log.storm_copies > 0 {
        fx.push(format!("+{} storm-copies", log.storm_copies));
    }
    for (k, v) in &log.triggers {
        if *v != 0 {
            fx.push(format!("{k} x{v}"));
        }
    }
    let fx_s = if fx.is_empty() { "(no carryover)".to_string() } else { fx.join(", ") };
    format!(
        "    {tag}{step:>2}: {card}{flip}\n           -> {res}  |  {fx_s}  |  mana left: {} {} (storm {})  |  opp {} life",
        post.mana.total(),
        crate::sim::fmt_pool(&post.mana),
        post.storm_count,
        live(post)
    )
}

pub fn rollout_from<R: Rng + ?Sized>(
    state: GameState,
    reg: &Registry,
    payoffs: &[&str],
    need_life: i64,
    rng: &mut R,
    max_steps: i64,
    trace: bool,
) -> Option<String> {
    // Takes `state` by value: callers already own a fresh, throwaway state (the result of the
    // first cast), so we move it in and skip a deep GameState clone per rollout.
    use std::collections::HashMap;
    let mut s = state;
    let mut score_cache: HashMap<String, f64> = HashMap::new();
    let mut ran_full = true;
    let mut step = 1i64;
    for _ in 0..max_steps {
        if let Some(w) = winning_payoff(&s, reg, payoffs, need_life) {
            return Some(w);
        }
        let mut cands = develop_candidates(&s, reg);
        if cands.is_empty() {
            ran_full = false;
            break;
        }
        // sort by develop_score desc (stable)
        for (c, _) in &cands {
            score_cache.entry(c.clone()).or_insert_with(|| develop_score(&s, reg, c));
        }
        cands.sort_by(|a, b| {
            let sa = *score_cache.get(&a.0).unwrap();
            let sb = *score_cache.get(&b.0).unwrap();
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        if trace {
            // Instrumentation: show the greedy's ranked options each step — score, and ',X' if it
            // can't currently pay for it. Reveals whether the policy is picking the right card and
            // whether mana is gating the better plays.
            let ranked: Vec<String> = cands
                .iter()
                .take(8)
                .map(|(c, _)| {
                    let sc = *score_cache.get(c).unwrap_or(&0.0);
                    let pay = if cast_source(&s, reg, c).is_some() { "" } else { ",X" };
                    format!("{c}({sc:+.2}{pay})")
                })
                .collect();
            println!(
                "    [step{step}] mana={} storm={} opp={} | options: {}",
                s.mana.total(),
                s.storm_count,
                s.opponent_life.iter().sum::<i64>(),
                ranked.join(", ")
            );
        }
        let mut nxt: Option<GameState> = None;
        for (card, source) in &cands {
            if let Some((ns, log)) = do_cast(&s, reg, card, source, rng, payoffs) {
                if trace {
                    step += 1;
                    println!("{}", trace_cast_line("FLIP", step, card, &log, &s, &ns));
                }
                nxt = Some(ns);
                break;
            }
        }
        match nxt {
            Some(ns) => s = ns,
            None => {
                ran_full = false;
                break;
            }
        }
        if s.library.is_empty() && winning_payoff(&s, reg, payoffs, need_life).is_none() {
            ran_full = false;
            break;
        }
        score_cache.clear(); // board changed (do_cast may add bodies); recompute like Python?
        // NOTE: Python caches per-state s (re-created each loop via the closure capturing s);
        // since s changes each iteration the cache is effectively per-iteration. Clearing matches.
    }
    if ran_full {
        if has_burn_engine(&s) && s.opponent_life.iter().any(|l| *l > 0) {
            return Some("burn".to_string());
        }
    }
    winning_payoff(&s, reg, payoffs, need_life)
}

#[derive(Debug, Clone, Default)]
pub struct RolloutEstimate {
    pub p_win: f64,
    pub by_payoff: Vec<(String, f64)>,
}

pub fn rollout_estimate<R: Rng + ?Sized>(
    state: &GameState,
    reg: &Registry,
    first: (&str, &str),
    payoffs: &[&str],
    n_sims: i64,
    max_steps: i64,
    rng: &mut R,
    decision_threshold: Option<f64>,
) -> RolloutEstimate {
    use std::collections::HashMap;
    let mut wins: HashMap<String, i64> = HashMap::new();
    let mut any_wins = 0i64;
    let need_life: i64 = state.opponent_life.iter().copied().filter(|l| *l > 0).sum();
    let (fcard, fsrc) = first;
    let need = decision_threshold.map(|t| t * n_sims as f64);

    // The first-cast probe (winning_payoff / cast_source / do_cast) is read-only on the base
    // state, so we avoid the per-sim deep clone of `state` entirely in the common (no-Breach)
    // case. Only Underworld Breach needs a mutable owned copy (crack_led mutates it).
    let breach = state.has_permanent("Underworld Breach");
    let mut ran = 0i64;
    for _ in 0..n_sims {
        let breach_state: Option<GameState> = if breach {
            let mut owned = state.clone();
            crack_led(&mut owned);
            Some(owned)
        } else {
            None
        };
        let s: &GameState = breach_state.as_ref().unwrap_or(state);
        let mut won = winning_payoff(s, reg, payoffs, need_life);
        if won.is_none() {
            let src = cast_source(s, reg, fcard);
            let ns = match src {
                Some(src) => do_cast(s, reg, fcard, &src, rng, payoffs).map(|(ns, _)| ns),
                None => None,
            };
            let _ = fsrc;
            match ns {
                Some(ns) => {
                    won = rollout_from(ns, reg, payoffs, need_life, rng, max_steps - 1, false);
                }
                None => {
                    ran += 1;
                    if let Some(need) = need {
                        if any_wins as f64 + ((n_sims - ran) as f64) < need {
                            break;
                        }
                    }
                    continue;
                }
            }
        }
        if let Some(w) = won {
            *wins.entry(w).or_insert(0) += 1;
            any_wins += 1;
        }
        ran += 1;
        if let Some(need) = need {
            if any_wins as f64 >= need {
                break;
            }
            if any_wins as f64 + ((n_sims - ran) as f64) < need {
                break;
            }
        }
    }
    let r = ran.max(1) as f64;
    let by_payoff: Vec<(String, f64)> =
        wins.into_iter().filter(|(_, v)| *v > 0).map(|(k, v)| (k, v as f64 / r)).collect();
    RolloutEstimate {
        p_win: if ran > 0 { any_wins as f64 / r } else { 0.0 },
        by_payoff,
    }
}

// --------------------------------------------------------------------------- //
// prove_go_off
// --------------------------------------------------------------------------- //

pub fn prove_go_off<R: Rng + ?Sized>(
    base: &GameState,
    reg: &Registry,
    first: (&str, &str),
    loop_line: bool,
    rng: &mut R,
    payoffs: &[&str],
    max_steps: i64,
    max_iters: i64,
    trace: bool,
) -> bool {
    let need_life: i64 = base.opponent_life.iter().copied().filter(|l| *l > 0).sum();
    let mut s = base.clone();
    if trace {
        println!(
            "  GO-OFF  : {} Krark bodies, {} flips/cast (p={:.2}); opponents at {} combined life. Open: cast {}.",
            s.krark_bodies(reg), s.flips_per_cast(reg), s.flip_p(), need_life, first.0
        );
    }
    if s.has_permanent("Underworld Breach") {
        crack_led(&mut s);
        if trace {
            println!("    crack Lion's Eye Diamond for mana (Underworld Breach line)");
        }
    }
    if winning_payoff(&s, reg, payoffs, need_life).is_some() {
        return true;
    }
    let (card, src0) = first;
    if loop_line {
        let mut iters = 0i64;
        let mut won = None;
        while won.is_none() && iters < max_iters {
            let src = match cast_source(&s, reg, card) {
                Some(src) => src,
                None => break,
            };
            match do_cast(&s, reg, card, &src, rng, payoffs) {
                Some((ns, log)) => {
                    if trace {
                        println!("{}", trace_cast_line("FLIP", iters + 1, card, &log, &s, &ns));
                    }
                    s = ns;
                }
                None => break,
            }
            iters += 1;
            won = winning_payoff(&s, reg, payoffs, need_life);
        }
        if won.is_none() && iters >= max_iters && has_burn_engine(&s)
            && s.opponent_life.iter().any(|l| *l > 0)
        {
            won = Some("burn".to_string());
        }
        if trace {
            println!("    => {} after {iters} casts.", won.clone().unwrap_or_else(|| "no win".into()));
        }
        return won.is_some();
    }
    // develop line
    let src = cast_source(&s, reg, card).unwrap_or_else(|| src0.to_string());
    match do_cast(&s, reg, card, &src, rng, payoffs) {
        Some((ns, log)) => {
            if trace {
                println!("{}", trace_cast_line("FLIP", 1, card, &log, &s, &ns));
            }
            let w = rollout_from(ns, reg, payoffs, need_life, rng, max_steps - 1, trace);
            if trace {
                println!("    => {}.", w.clone().unwrap_or_else(|| "no win".into()));
            }
            w.is_some()
        }
        None => false,
    }
}

// --------------------------------------------------------------------------- //
// analyze_runaway
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone)]
pub struct RunawayAssessment {
    pub card: String,
    pub kind: String,
    pub flips: i64,
    pub p_return: f64,
    pub e_chain_len: f64,
    pub e_net_mana_per_cast: f64,
    pub e_net_cards_per_cast: f64,
    pub e_total_mana: f64,
    pub e_total_cards: f64,
}

pub fn analyze_runaway(state: &GameState, reg: &Registry, card_name: &str) -> RunawayAssessment {
    let a = analyze_cast(state, reg, card_name);
    let f = a.flips;
    let eng_mana = a.e_mana.values().sum::<f64>() + a.e_treasures;
    let mut own_red = spell_red_per_resolution(card_name);
    let own_gen = spell_generic_per_resolution(card_name) + untap_mana(state, reg, card_name);
    if card_name == "Jeska's Will" {
        own_red = state.opponent_hand.iter().copied().max().unwrap_or(0);
    }
    if card_name == "Brightstone Ritual" {
        own_red = state.krark_bodies(reg); // {R} per Goblin = per Krark body
    }
    let own_mana = (own_red + own_gen) as f64 * a.e_effect_resolutions;
    let cast_cost: i64 = state.cast_cost(reg, card_name).iter().filter(|(k, _)| k.as_str() != "X").map(|(_, v)| *v).sum();
    let e_net_mana = eng_mana + own_mana - cast_cost as f64;
    let e_net_cards = a.e_draws - 1.0;
    let e_chain = if a.p_resolve > 0.0 { 1.0 / a.p_resolve } else { f64::INFINITY };
    let e_total_mana = e_net_mana * e_chain;
    let e_total_cards = e_net_cards * e_chain;

    let kind = if f == 0 {
        "NONE"
    } else if e_net_mana > 0.0 && a.p_return > 0.5 {
        "MANA_RUNAWAY"
    } else if e_net_cards > 0.0 && a.p_return > 0.5 {
        "DRAW_RUNAWAY"
    } else if e_net_mana > 0.0 || e_net_cards > 0.0 {
        "POSITIVE_BUT_BOUNDED"
    } else {
        "NONE"
    };
    RunawayAssessment {
        card: card_name.to_string(),
        kind: kind.to_string(),
        flips: f,
        p_return: a.p_return,
        e_chain_len: e_chain,
        e_net_mana_per_cast: e_net_mana,
        e_net_cards_per_cast: e_net_cards,
        e_total_mana,
        e_total_cards,
    }
}

// --------------------------------------------------------------------------- //
// Loop detection
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, Default)]
pub struct LoopReport {
    pub confirmed: HashSet<String>,
    pub reasons: Vec<String>,
    pub candidates: Vec<(HashSet<String>, String, String)>,
}

fn engine_tags(state: &GameState) -> HashSet<String> {
    let mut tags = HashSet::new();
    if state.has_permanent("Archmage Emeritus") {
        tags.insert("draw".to_string());
    }
    if state.has_permanent("Storm-Kiln Artist") {
        tags.insert("mana_any".to_string());
    }
    tags
}

fn add_costs(costs: &[&crate::cards::ManaCost]) -> crate::cards::ManaCost {
    let mut out = crate::cards::ManaCost::new();
    for c in costs {
        for (k, v) in *c {
            *out.entry(k.clone()).or_insert(0) += *v;
        }
    }
    out
}

fn cheapest_instant_in_hand(state: &GameState, reg: &Registry) -> Option<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut best: Option<(i64, String)> = None;
    for c in &state.hand {
        if !seen.insert(c.as_str()) {
            continue;
        }
        if reg.get(c).types.contains(&CardType::Instant) {
            let total: i64 = state.cast_cost(reg, c).values().sum();
            if best.as_ref().map(|(b, _)| total < *b).unwrap_or(true) {
                best = Some((total, c.clone()));
            }
        }
    }
    best.map(|(_, c)| c)
}

/// Returns (shimmer, total_cost, route).
fn shimmer_start(state: &GameState, reg: &Registry) -> (Option<String>, crate::cards::ManaCost, String) {
    let mut best: (Option<String>, crate::cards::ManaCost, String) =
        (None, crate::cards::ManaCost::new(), String::new());
    let cost_total = |c: &crate::cards::ManaCost| -> i64 { c.values().sum() };

    let gale_play = state.has_permanent("Gale, Waterdeep Prodigy");
    let gale_hand = state.hand.iter().any(|c| c == "Gale, Waterdeep Prodigy");
    let trig = cheapest_instant_in_hand(state, reg);

    let mut consider = |sh: &str, cost: crate::cards::ManaCost, route: &str| {
        if best.0.is_none() || cost_total(&cost) < cost_total(&best.1) {
            best = (Some(sh.to_string()), cost, route.to_string());
        }
    };

    for sh in crate::cards::SHIMMERS.iter().copied() {
        if state.hand.iter().any(|c| c == sh) {
            consider(sh, state.cast_cost(reg, sh).into_owned(), "in hand");
            continue;
        }
        if !state.graveyard.iter().any(|c| c == sh) {
            continue;
        }
        if can_escape(state, reg, sh) {
            consider(sh, state.cast_cost(reg, sh).into_owned(), "escaped from graveyard via Underworld Breach");
        }
        if (gale_play || gale_hand) && trig.is_some() {
            let t = trig.as_ref().unwrap();
            let (sh_c, t_c) = (state.cast_cost(reg, sh), state.cast_cost(reg, t));
            let mut c = add_costs(&[sh_c.as_ref(), t_c.as_ref()]);
            let mut route = format!("recast from graveyard via Gale (trigger: cast {t})");
            if !gale_play {
                let gale_c = state.cast_cost(reg, "Gale, Waterdeep Prodigy");
                c = add_costs(&[&c, gale_c.as_ref()]);
                route += " after casting Gale";
            }
            consider(sh, c, &route);
        }
    }
    best
}

pub fn detect_loops(state: &GameState, reg: &Registry) -> LoopReport {
    let mut confirmed: HashSet<String> = HashSet::new();
    let mut reasons: Vec<String> = Vec::new();
    let mut candidates: Vec<(HashSet<String>, String, String)> = Vec::new();

    let dc_bodies = state.battlefield.iter().filter(|p| p.effective_name() == "Dualcaster Mage").count();
    let (shimmer, shimmer_cost, route) = shimmer_start(state, reg);
    let pieces = state.hand.iter().any(|c| c == "Dualcaster Mage") && shimmer.is_some();
    let combined = if pieces {
        add_costs(&[&shimmer_cost, state.cast_cost(reg, "Dualcaster Mage").as_ref()])
    } else {
        crate::cards::ManaCost::new()
    };
    if dc_bodies >= 2 {
        confirmed.insert("hasty_attackers".to_string());
        confirmed.extend(engine_tags(state));
        reasons.push("Multiple Dualcaster Mage bodies present — loop already established.".to_string());
    } else if pieces && state.mana.can_pay(&combined) {
        confirmed.insert("hasty_attackers".to_string());
        confirmed.extend(engine_tags(state));
        let sh = shimmer.clone().unwrap();
        reasons.push(format!(
            "{sh} ({route}) + Dualcaster Mage with mana for both — infinite hasty attackers."
        ));
    } else if pieces {
        let sh = shimmer.clone().unwrap();
        let mut tags = HashSet::new();
        tags.insert("hasty_attackers".to_string());
        candidates.push((
            tags,
            format!("{sh} ({route}) + Dualcaster Mage but not enough mana to start both."),
            format!("need {:?} to initiate; current pool {:?}.", combined, state.mana.slots),
        ));
    }

    // Krark + Sakashima(break) + a shimmer in hand + a renewable mana source = infinite hasty Krarks
    // (steer each shimmer cast to one loss -> returns to hand, wins -> token Krarks; the break keeps
    // them, the mana refunds the recast). Same condition as winning_payoff's krark_shimmer_lethal.
    let kss_sustain = ["Storm-Kiln Artist", "Birgi, God of Storytelling", "Tavern Scoundrel"]
        .iter()
        .any(|e| state.has_permanent(e))
        || ["Brightstone Ritual", "Pyretic Ritual", "Desperate Ritual", "Rite of Flame"]
            .iter()
            .any(|r| state.hand.iter().any(|c| c == r));
    let kss_shimmer = crate::cards::SHIMMERS.iter().find(|sh| {
        (state.hand.iter().any(|c| &c.as_str() == *sh) || state.exiled_play.iter().any(|c| &c.as_str() == *sh))
            && state.mana.can_pay(&state.cast_cost(reg, sh))
    });
    if state.has_sakashima_break()
        && (state.has_krarks_thumb() || state.flips_per_cast(reg) >= 3)
        && kss_sustain
    {
        if let Some(sh) = kss_shimmer {
            confirmed.insert("hasty_attackers".to_string());
            confirmed.extend(engine_tags(state));
            reasons.push(format!(
                "{sh} + Krark + Sakashima break + renewable mana — steer each cast to 1 loss (returns it) + wins (token Krarks): infinite hasty Krarks."
            ));
        }
    }

    let breach = state.has_permanent("Underworld Breach") || state.hand.iter().any(|c| c == "Underworld Breach");
    let led = state.has_permanent("Lion's Eye Diamond")
        || state.hand.iter().any(|c| c == "Lion's Eye Diamond")
        || state.graveyard.iter().any(|c| c == "Lion's Eye Diamond");
    let bfreeze = state.graveyard.iter().any(|c| c == "Brain Freeze") || state.hand.iter().any(|c| c == "Brain Freeze");
    if breach && led && bfreeze {
        let tags: HashSet<String> = ["storm", "mill", "mana_any"].iter().map(|s| s.to_string()).collect();
        candidates.push((
            tags,
            "Underworld Breach + Lion's Eye Diamond + Brain Freeze — escape Brain Freeze off LED mana for storm + self-mill.".to_string(),
            "sustainability depends on graveyard fuel; verify with estimate_p_lethal.".to_string(),
        ));
    }

    LoopReport { confirmed, reasons, candidates }
}

pub fn apply_loops(state: &mut GameState, reg: &Registry) -> LoopReport {
    let report = detect_loops(state, reg);
    if !report.confirmed.is_empty() {
        for t in &report.confirmed {
            state.infinite.insert(t.clone());
        }
    }
    report
}

// --------------------------------------------------------------------------- //
// selftest — mirrors loops.py __main__
// --------------------------------------------------------------------------- //

pub fn selftest(reg: &Registry) {
    use rand::SeedableRng;

    let board = || -> Vec<Permanent> {
        vec![
            krark_body("Krark, the Thumbless", None, false),
            krark_body("Sakashima of a Thousand Faces", Some("Krark, the Thumbless"), false),
            Permanent { summoning_sick: false, ..Permanent::new("Veyran, Voice of Duality") },
            Permanent { summoning_sick: false, ..Permanent::new("Harmonic Prodigy") },
            Permanent { summoning_sick: false, ..Permanent::new("Archmage Emeritus") },
            Permanent { summoning_sick: false, ..Permanent::new("Storm-Kiln Artist") },
        ]
    };

    // analyze_runaway: Jeska's Will -> MANA_RUNAWAY
    {
        let s = GameState {
            library: vec!["Island".into(); 60],
            hand: vec!["Jeska's Will".into()],
            battlefield: board(),
            ..Default::default()
        };
        let ra = analyze_runaway(&s, reg, "Jeska's Will");
        assert_eq!(ra.kind, "MANA_RUNAWAY", "kind={}", ra.kind);
        assert!(ra.e_net_mana_per_cast > 0.0);
        println!("[ok] loops: analyze_runaway Jeska's Will -> {} (net mana/cast {:.2})", ra.kind, ra.e_net_mana_per_cast);
    }

    // Krark's Thumb: choosing to LOSE one flip keeps a low-body loop alive, so p_return follows the
    // choice (1 - 0.25^f), not the naive better-coin (1 - 0.75^f) which at f=1 wrongly read 0.25 < 0.5
    // and de-classed the engine. One Krark + Thumb -> f=1 -> p_return must be ~0.75.
    {
        let s = GameState {
            library: vec!["Island".into(); 40],
            hand: vec!["Brainstorm".into()],
            battlefield: vec![
                krark_body("Krark, the Thumbless", None, false),
                Permanent { summoning_sick: false, ..Permanent::new("Krark's Thumb") },
                Permanent { summoning_sick: false, ..Permanent::new("Storm-Kiln Artist") },
            ],
            ..Default::default()
        };
        assert!(s.has_krarks_thumb());
        assert_eq!(s.flips_per_cast(reg), 1, "one Krark body -> f=1");
        let ra = analyze_runaway(&s, reg, "Brainstorm");
        assert!((ra.p_return - 0.75).abs() < 0.01, "Thumb f=1 p_return={} (expected 0.75)", ra.p_return);
        println!("[ok] loops: Krark's Thumb f=1 -> p_return={:.2} (choose-to-lose keeps the loop alive)", ra.p_return);
    }

    // estimate_p_lethal: Jeska + payoffs in hand -> high p_win
    {
        let mut s = GameState {
            library: vec!["Island".into(); 40],
            hand: vec!["Jeska's Will".into(), "Grapeshot".into()],
            battlefield: board(),
            opponent_life: vec![10, 10, 10],
            ..Default::default()
        };
        s.mana.add("R", 1);
        s.mana.add("C", 2);
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        let res = estimate_p_lethal(&s, reg, "Jeska's Will", DEV_PAYOFFS, 1500, 80, &mut rng, None);
        assert!(res.p_win > 0.5, "p_win={}", res.p_win);
        println!("[ok] loops: estimate_p_lethal Jeska chain p_win={:.3} mean_chain={:.1}", res.p_win, res.mean_chain_len);
    }

    // contrast: no payoff -> p_win == 0
    {
        let mut s = GameState {
            library: vec!["Island".into(); 40],
            hand: vec!["Jeska's Will".into()],
            battlefield: board(),
            opponent_life: vec![40, 40, 40],
            ..Default::default()
        };
        s.mana.add("R", 1);
        s.mana.add("C", 2);
        let mut rng = rand::rngs::StdRng::seed_from_u64(2);
        let res = estimate_p_lethal(&s, reg, "Jeska's Will", DEV_PAYOFFS, 800, 80, &mut rng, None);
        assert!(res.p_win == 0.0, "p_win={} (expect 0)", res.p_win);
        println!("[ok] loops: no payoff -> p_win=0.000 (mana/draw alone is not a win)");
    }

    // loop detector: Twinflame + Dualcaster with mana -> confirmed
    {
        let mut lp = GameState {
            library: vec!["Island".into(); 40],
            hand: vec!["Twinflame".into(), "Dualcaster Mage".into()],
            battlefield: vec![
                krark_body("Krark, the Thumbless", None, false),
                Permanent { summoning_sick: false, ..Permanent::new("Archmage Emeritus") },
            ],
            ..Default::default()
        };
        lp.mana.add("R", 3);
        lp.mana.add("C", 2);
        let rep = apply_loops(&mut lp, reg);
        assert!(lp.infinite.contains("hasty_attackers") && lp.infinite.contains("draw"), "{:?}", lp.infinite);
        assert_eq!(win::evaluate_win(&lp, reg, None).wtype, "combat");
        println!("[ok] loops: Twinflame+Dualcaster+mana -> confirmed {:?}", rep.confirmed);
    }

    // same pieces no mana -> candidate only
    {
        let lp2 = GameState {
            hand: vec!["Twinflame".into(), "Dualcaster Mage".into()],
            ..Default::default()
        };
        let rep2 = detect_loops(&lp2, reg);
        assert!(rep2.confirmed.is_empty() && !rep2.candidates.is_empty());
        println!("[ok] loops: no mana -> not confirmed, flagged as candidate");
    }

    // Breach + LED + Brain Freeze -> candidate
    {
        let lp3 = GameState {
            battlefield: vec![Permanent { summoning_sick: false, ..Permanent::new("Underworld Breach") }],
            hand: vec!["Lion's Eye Diamond".into()],
            graveyard: vec!["Brain Freeze".into()],
            ..Default::default()
        };
        let rep3 = detect_loops(&lp3, reg);
        assert!(rep3.confirmed.is_empty() && rep3.candidates.iter().any(|c| c.1.contains("Breach")));
        println!("[ok] loops: Breach+LED+Brain Freeze -> candidate, not asserted");
    }

    // Sakashima-deploy lever (`--sak-deploy`): a command-zone Sakashima + Krark + an accessible
    // shimmer + the Jeska's-Will mana engine + mana for {3}{U}+shimmer is recognized as a combat kill
    // ONLY when build_state has exposed the deployable Sakashima (sakashima_cmd). It ALSO requires
    // Underworld Breach IN PLAY so a first-cast yarded shimmer (P=0.5^flips) can be recovered — without
    // it, declaring P=1.0 would be dishonest. Flag off -> sakashima_cmd false -> not lethal.
    {
        let mut s = GameState {
            library: vec!["Island".into(); 40],
            hand: vec!["Molten Duplication".into(), "Jeska's Will".into()],
            battlefield: vec![
                krark_body("Krark, the Thumbless", None, false),
                Permanent { summoning_sick: false, ..Permanent::new("Underworld Breach") },
            ],
            opponent_life: vec![160],
            ..Default::default()
        };
        s.mana.add("*", 8); // wildcard covers Sakashima {3}{U} + the shimmer cast
        assert!(
            winning_payoff(&s, reg, DEV_PAYOFFS, 160).is_none(),
            "flag off (sakashima_cmd=false) must NOT recognize the deploy line"
        );
        s.sakashima_cmd = true;
        assert_eq!(
            winning_payoff(&s, reg, DEV_PAYOFFS, 160).as_deref(),
            Some("combat"),
            "deployable Sakashima + Krark + shimmer + Jeska + Breach-in-play + mana -> combat"
        );
        // Recoverability gate: strip Breach from play -> a yarded shimmer is stranded -> NOT lethal.
        s.battlefield.retain(|p| p.name != "Underworld Breach");
        assert!(
            winning_payoff(&s, reg, DEV_PAYOFFS, 160).is_none(),
            "no Breach in play -> yarded shimmer unrecoverable -> must NOT claim a kill"
        );
        println!("[ok] loops: --sak-deploy kill needs Breach-in-play recovery (inert when off / no Breach)");
    }

    println!("loops selftest passed.");
}
