//! resolver.rs — port of resolver.py. The Krark cast resolver: analyze_cast (exact EV),
//! the per-card EFFECTS, ETB tutors, and resolve_cast_sample (one playout step).

use crate::cards::{CardType, ManaCost, Registry};
use crate::game_state::{krark_body, GameState, Permanent};
use crate::wishlist;
use rand::Rng;
use std::collections::HashMap;

pub const MAX_FLIPS: i64 = 40;
const QUASI_BODY_CAP: i64 = 4;
const QUASI_TOKEN_CAP: i64 = 5;

pub const STORM_SPELLS: &[&str] = &["Grapeshot", "Brain Freeze", "Flusterstorm"];
const DAMAGE_SPELLS: &[&str] = &["Grapeshot", "Gut Shot"];

// --------------------------------------------------------------------------- //
// Pre-cast analysis (exact, no sampling)
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, Default)]
pub struct CastAnalysis {
    pub card: String,
    pub flips: i64,
    pub p: f64,
    pub p_resolve: f64,
    pub p_return: f64,
    pub e_copies: f64,
    pub e_storm_copies: i64,
    pub e_effect_resolutions: f64,
    pub e_storm_after: i64,
    pub e_draws: f64,
    pub e_treasures: f64,
    pub e_mana: HashMap<String, f64>,
    pub e_damage: f64,
    pub wins_pmf: Vec<f64>,
    pub notes: Vec<String>,
}

fn comb(n: i64, k: i64) -> f64 {
    if k < 0 || k > n {
        return 0.0;
    }
    let mut r = 1.0f64;
    let k = k.min(n - k);
    for i in 0..k {
        r = r * (n - i) as f64 / (i + 1) as f64;
    }
    r
}

fn binom_pmf(n: i64, p: f64) -> Vec<f64> {
    (0..=n)
        .map(|k| comb(n, k) * p.powi(k as i32) * (1.0 - p).powi((n - k) as i32))
        .collect()
}

pub fn analyze_cast(state: &GameState, reg: &Registry, card_name: &str) -> CastAnalysis {
    let cdef = reg.get(card_name);
    if !cdef.is_instant_or_sorcery() {
        panic!("{card_name} is not an instant/sorcery; Krark won't flip for it.");
    }

    // Step Through is a Wizardcycle (activated ability), not a Krark-flipping cast: estimate it as
    // a single non-flipping tutor resolution.
    if card_name == "Step Through" {
        return CastAnalysis {
            card: card_name.to_string(),
            flips: 0,
            p: state.flip_p(),
            p_resolve: 1.0,
            p_return: 0.0,
            e_effect_resolutions: 1.0,
            wins_pmf: vec![1.0],
            ..Default::default()
        };
    }

    let f = state.flips_per_cast(reg).min(MAX_FLIPS);
    let p = state.flip_p();
    let mut notes: Vec<String> = Vec::new();
    if f == 0 {
        notes.push("No Krark body in play — casting triggers no flips (one normal cast).".into());
    }

    let storm_copies = if STORM_SPELLS.contains(&card_name) {
        state.storm_count
    } else {
        0
    };
    let pmf = if f > 0 { binom_pmf(f, p) } else { vec![1.0] };
    let e_wins = f as f64 * p;
    // Krark's Thumb lets you CHOOSE each flip (flip two coins, keep one), so you deliberately LOSE
    // one flip to return the spell to hand and keep the loop alive, winning the rest for copies.
    // Continuation only fails if every trigger rolls two heads (no flip can be made a loss): 0.25^f.
    // Without the Thumb the spell resolves (loop ends) only when all flips naturally win: p^f (p=0.5).
    let p_resolve = if f > 0 {
        if state.has_krarks_thumb() { 0.25_f64.powi(f as i32) } else { p.powi(f as i32) }
    } else {
        1.0
    };
    let p_return = 1.0 - p_resolve;
    let e_copies = e_wins;

    let e_effect_resolutions =
        (if f > 0 { e_wins + p_resolve } else { 1.0 }) + storm_copies as f64;
    let e_cast_copy_events = 1.0 + e_wins + storm_copies as f64;

    let mut e_draws = 0.0;
    let mut e_treasures = 0.0;
    let mut e_damage = 0.0;
    let mut e_mana: HashMap<String, f64> = HashMap::new();

    for (idx, eng) in state.value_engines(reg) {
        let mult = state.value_multiplier(eng, true) as f64;
        let eff = state.battlefield[idx].effective_name();
        if crate::cards::is_verify(eff) {
            notes.push(format!("{eff}: output NOT certified; treat as estimate."));
        }
        let cause = eng.trigger_cause.as_deref();
        let events = match cause {
            Some("is_cast_or_copy") => e_cast_copy_events,
            Some("is_cast") | Some("spell_cast") => 1.0,
            Some("coin_flip_win") => e_wins,
            _ => 0.0,
        };
        e_draws += eng.draw_per_trigger as f64 * mult * events;
        e_treasures += (eng.treasure_per_trigger + eng.treasure_per_flip_win) as f64 * mult * events;
        e_damage += eng.damage_per_trigger as f64 * mult * events;
        for (col, amt) in &eng.mana_per_trigger {
            *e_mana.entry(col.clone()).or_insert(0.0) += *amt as f64 * mult * events;
        }
    }

    if DAMAGE_SPELLS.contains(&card_name) {
        e_damage += e_effect_resolutions;
        notes.push(format!(
            "{card_name}: E[total bolts] = {e_effect_resolutions:.2} ({storm_copies} storm + {e_wins:.2} Krark + {p_resolve:.3} original)."
        ));
    }

    CastAnalysis {
        card: card_name.to_string(),
        flips: f,
        p,
        p_resolve,
        p_return,
        e_copies,
        e_storm_copies: storm_copies,
        e_effect_resolutions,
        e_storm_after: state.storm_count + 1,
        e_draws,
        e_treasures,
        e_mana,
        e_damage,
        wins_pmf: pmf,
        notes,
    }
}

// --------------------------------------------------------------------------- //
// Effects support
// --------------------------------------------------------------------------- //

/// "untap up to N lands" cap per resolution. Mirror of UNTAP_LANDS.
fn untap_cap(card: &str) -> i64 {
    match card {
        "Frantic Search" => 3,
        "Snap" => 2,
        _ => 0,
    }
}

pub fn untap_mana(state: &GameState, reg: &Registry, card: &str) -> i64 {
    let cap = untap_cap(card);
    if cap == 0 {
        return 0;
    }
    // "Untap up to N lands" only refunds lands that are actually TAPPED (re-tapping them for a
    // second use). Real lands only — treasures/rocks aren't untapped by these spells.
    let n_tapped = state
        .battlefield
        .iter()
        .filter(|p| p.tapped && reg.get(&p.name).types.contains(&CardType::Land))
        .count() as i64;
    cap.min(n_tapped)
}

/// Mana actually refunded by "untap up to `cap` lands": pick up to `cap` TAPPED lands and sum
/// their production in REAL colors (basics give their color, dual/any-color lands give '*').
/// Requires the lands to be present and tapped — with too few lands the refund is partial, so
/// the spell can go mana-negative on its own (the user's ruling: Snap/Frantic need the lands).
pub fn untap_lands_mana(state: &GameState, reg: &Registry, cap: i64) -> ManaCost {
    let mut out: ManaCost = HashMap::new();
    let mut taken = 0i64;
    for p in &state.battlefield {
        if taken >= cap {
            break;
        }
        if !p.tapped || !reg.get(&p.name).types.contains(&CardType::Land) {
            continue;
        }
        if let Some((_, produced)) = crate::tables::mana_source(p.effective_name()) {
            for (k, v) in &produced {
                *out.entry(k.clone()).or_insert(0) += v;
            }
            taken += 1;
        }
    }
    out
}


const MANA_ROCK_NAMES: &[&str] = &[
    "Sol Ring", "Mana Vault", "Chrome Mox", "Mox Amber", "Mox Diamond", "Lotus Petal",
    "Arcane Signet", "Springleaf Drum", "Relic of Legends",
];

fn is_source(reg: &Registry, name: &str) -> bool {
    reg.get(name).types.contains(&CardType::Land) || MANA_ROCK_NAMES.contains(&name)
}

/// Sort key for what to DISCARD (lowest = pitch first). Mirror of discard_rank.
pub fn discard_rank(state: &GameState, reg: &Registry, card: &str) -> f64 {
    // Win-cons / combo pieces rank above everything, but ORDERED (not a flat INFINITY) so a FORCED
    // discard (Frantic Search etc. when the hand is all keepers) sheds the least-critical protected
    // card -- a redundant combo piece -- and NEVER the singleton Thassa's Oracle we're digging to.
    // (The dig used to pitch its own Oracle into the graveyard and then deck out -> brick.)
    let protect = match card {
        "Thassa's Oracle" => 1e9,
        "Grapeshot" | "Brain Freeze" => 1e8,
        "Underworld Breach" => 5e7,
        "Dualcaster Mage" | "Twinflame" | "Heat Shimmer" | "Gale, Waterdeep Prodigy" => 1e7,
        _ => 0.0,
    };
    if protect > 0.0 {
        return protect;
    }
    let val = wishlist::card_value(state, reg, card, false);
    let mut redundant = 0.0;
    if state
        .battlefield
        .iter()
        .any(|p| p.effective_name() == card || p.name == card)
    {
        redundant += 5.0;
    }
    if is_source(reg, card) {
        let sources = state
            .battlefield
            .iter()
            .filter(|p| is_source(reg, p.effective_name()))
            .count() as i64
            + state.hand.iter().filter(|c| is_source(reg, c)).count() as i64;
        if sources > 4 {
            redundant += 3.0 + 0.5 * (sources - 4) as f64;
        }
    }
    val - redundant
}

/// Live finishers: with a storm engine assembling, these must never be pitched — stranding one in
/// the graveyard with no recursion left is how the deck deck-outs mid-loop.
pub fn is_finisher(card: &str) -> bool {
    matches!(card, "Grapeshot" | "Brain Freeze" | "Underworld Breach")
}

pub fn pitch_worst(state: &mut GameState, reg: &Registry, k: i64) {
    // Mirror of SimGame::discard_to_hand_size (the end-of-turn discard) and mulligan bottoming: shed
    // the worst card by discard_rank, one at a time. discard_rank keeps win-cons/combo pieces on top
    // (Oracle highest), so a forced discard never throws the payoff while any lesser card exists.
    // (No filter/fallback: the old fallback pitched a protected card once the hand was all keepers.)
    // With a Krark body on board (storm engine live) NEVER pitch a finisher — keep fewer cards than
    // strand the kill (the all-keepers FS loop used to pitch Grapeshot/Brain Freeze and deck out).
    let protect_finishers = state.krark_bodies(reg) >= 1;
    for _ in 0..k {
        if state.hand.is_empty() {
            return;
        }
        let worst = state
            .hand
            .iter()
            .filter(|c| !(protect_finishers && is_finisher(c)))
            .min_by(|a, b| {
                discard_rank(state, reg, a)
                    .partial_cmp(&discard_rank(state, reg, b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .cloned();
        let worst = match worst {
            Some(w) => w,
            None => return, // hand is all finishers/essential — don't strand them
        };
        if let Some(pos) = state.hand.iter().position(|c| *c == worst) {
            state.hand.remove(pos);
        }
        state.graveyard.push(worst);
    }
}

// --------------------------------------------------------------------------- //
// ETB tutors
// --------------------------------------------------------------------------- //

/// If `name` is a tutor creature entering, fetch the best wishlist card matching its filter.
/// Returns the fetched card, or None. Mirror of apply_etb / ETB_TUTORS.
pub fn apply_etb(state: &mut GameState, reg: &Registry, name: &str) -> Option<String> {
    match name {
        "Spellseeker" => wishlist::tutor(state, reg, |c| {
            let cd = reg.get(c);
            cd.is_instant_or_sorcery() && cd.mana_value <= 2
        }),
        "Imperial Recruiter" => {
            wishlist::tutor(state, reg, |c| reg.get(c).types.contains(&CardType::Creature))
        }
        "Okaun, Eye of Chaos" => {
            wishlist::tutor(state, reg, |c| c == "Zndrsplt, Eye of Wisdom")
        }
        "Zndrsplt, Eye of Wisdom" => {
            wishlist::tutor(state, reg, |c| c == "Okaun, Eye of Chaos")
        }
        _ => None,
    }
}

// --------------------------------------------------------------------------- //
// Quasiduplicate target selection
// --------------------------------------------------------------------------- //

/// Develop value of casting Quasiduplicate now. Mirror of resolver.quasi_value.
pub fn quasi_value(state: &GameState, reg: &Registry, resolutions: f64) -> f64 {
    match quasi_target(state, reg) {
        None => -1.0,
        Some(t) if t == "Krark, the Thumbless" => resolutions * 3.0,
        Some(_) => resolutions * 2.0,
    }
}

pub fn quasi_target(state: &GameState, reg: &Registry) -> Option<String> {
    if state.battlefield.iter().filter(|p| p.is_token).count() as i64 >= QUASI_TOKEN_CAP {
        return None;
    }
    let on_bf: Vec<&str> = state.battlefield.iter().map(|p| p.effective_name()).collect();
    let payoff_acc = ["Grapeshot", "Thassa's Oracle", "Brain Freeze"].iter().any(|pf| {
        state.hand.iter().any(|c| c == pf)
            || state.graveyard.iter().any(|c| c == pf)
            || state.has_permanent(pf)
    });
    if !payoff_acc {
        for t in ["Imperial Recruiter", "Spellseeker"] {
            if on_bf.contains(&t) {
                return Some(t.to_string());
            }
        }
    }
    if on_bf.contains(&"Krark, the Thumbless") && state.krark_bodies(reg) < QUASI_BODY_CAP {
        return Some("Krark, the Thumbless".to_string());
    }
    None
}

// --------------------------------------------------------------------------- //
// Effects dispatch — port of the @effect handlers
// --------------------------------------------------------------------------- //

/// Choices passed to an effect (subset of the Python dict that the effects read).
#[derive(Default, Clone)]
pub struct Choices {
    pub target: Option<String>, // Brain Freeze: "self" | "opponents"
}

/// Index of the FIRST entry with the smallest positive value (matches Python's
/// `min((j for j,l in enumerate(xs) if l>0), key=lambda j: xs[j])` tie-break: first wins).
fn first_min_positive(xs: &[i64]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (j, &l) in xs.iter().enumerate() {
        if l > 0 && (best.is_none() || l < xs[best.unwrap()]) {
            best = Some(j);
        }
    }
    best
}

fn pop_top(state: &mut GameState) -> Option<String> {
    if state.library.is_empty() {
        None
    } else {
        Some(state.library.remove(0))
    }
}

/// Draw the best (by card_value) of the top `k` cards — models look-at-top selection: Ponder /
/// Brainstorm (reorder top 3, draw best), Preordain (scry 2 + draw), Serum Visions (draw + scry 2),
/// Opt (scry 1 + draw). The non-chosen looked-at cards stay on top; the dig-toward-best is the value
/// (a plain draw would take a random top card instead).
fn dig_best(state: &mut GameState, reg: &Registry, k: usize) {
    if state.library.is_empty() {
        return;
    }
    let look = k.min(state.library.len());
    let top: Vec<String> = state.library.iter().take(look).cloned().collect();
    let keep = wishlist::best(state, reg, &top, 1, false)[0].clone();
    if let Some(pos) = state.library.iter().position(|c| *c == keep) {
        state.library.remove(pos);
    }
    state.hand.push(keep);
}

/// Serum Visions: draw a card, THEN scry. The immediate draw is the blind top card (no selection —
/// this is what separates it from Preordain, whose scry happens before the draw); the scry only
/// improves the NEXT draw, modeled by promoting the best of the next `scry_n` cards to the top.
fn draw_then_scry(state: &mut GameState, reg: &Registry, scry_n: usize) {
    if state.library.is_empty() {
        return;
    }
    let drawn = state.library.remove(0); // blind top draw, before the scry
    state.hand.push(drawn);
    let look = scry_n.min(state.library.len());
    if look <= 1 {
        return;
    }
    let top: Vec<String> = state.library.iter().take(look).cloned().collect();
    let best = wishlist::best(state, reg, &top, 1, false)[0].clone();
    if let Some(pos) = state.library.iter().take(look).position(|c| *c == best) {
        if pos != 0 {
            let b = state.library.remove(pos);
            state.library.insert(0, b); // best of the scryed cards is drawn next
        }
    }
}

/// Consider: surveil 1 then draw. Draw the best of the top `k`; any looked-at card ABOVE the kept one
/// is binned to the graveyard (the surveil), which also fuels Underworld Breach / Gale recursion.
fn dig_best_surveil(state: &mut GameState, reg: &Registry, k: usize) {
    if state.library.is_empty() {
        return;
    }
    let look = k.min(state.library.len());
    let top: Vec<String> = state.library.iter().take(look).cloned().collect();
    let keep = wishlist::best(state, reg, &top, 1, false)[0].clone();
    let keep_pos = state.library.iter().position(|c| *c == keep).unwrap_or(0);
    let binned: Vec<String> = state.library.drain(0..keep_pos).collect();
    state.graveyard.extend(binned);
    if !state.library.is_empty() {
        let drawn = state.library.remove(0);
        state.hand.push(drawn);
    }
}

/// Run the per-card effect `n` total instances (resolutions + storm copies). `rng` is used by
/// Gamble's random discard. Returns true if the card had a scripted effect.
/// Does `name`'s ETB tutor still have a legal target in the library?
fn etb_has_target(state: &GameState, reg: &Registry, name: &str) -> bool {
    match name {
        "Spellseeker" => state
            .library
            .iter()
            .any(|c| reg.get(c).is_instant_or_sorcery() && reg.get(c).mana_value <= 2),
        "Imperial Recruiter" => state
            .library
            .iter()
            .any(|c| reg.get(c).types.contains(&CardType::Creature)),
        _ => false,
    }
}

/// Snap "return target creature to hand": the value play is bouncing your OWN spent ETB tutor and
/// recasting it for another trigger. Modeled as the net effect (pay the recast cost + re-fire the
/// ETB) for the best on-board tutor that still has a target. GATED on clear SURPLUS mana so it can
/// never spend mana a kill needs. The body is treated as staying in play (it doesn't attack here).
fn snap_bounce_recast(state: &mut GameState, reg: &Registry) {
    for name in ["Spellseeker", "Imperial Recruiter"] {
        if !state.battlefield.iter().any(|p| p.effective_name() == name) {
            continue;
        }
        let cost = reg.get(name).cost.clone();
        let cost_total: i64 = cost.values().sum();
        if state.mana.total() < cost_total + 2 {
            continue; // keep a buffer; never starve the kill for a tutor
        }
        if !state.mana.can_pay(&cost) || !etb_has_target(state, reg, name) {
            continue;
        }
        state.mana.pay(&cost);
        apply_etb(state, reg, name);
        return; // one bounce-recast per Snap resolution
    }
}

fn run_effect<R: Rng + ?Sized>(
    state: &mut GameState,
    reg: &Registry,
    card: &str,
    n: i64,
    choices: &Choices,
    rng: &mut R,
) -> bool {
    match card {
        "Ponder" | "Brainstorm" | "Preordain" => {
            // Ponder/Brainstorm reorder the top 3 and draw the best; Preordain scrys 2 BEFORE drawing,
            // so its draw is selected too — all modeled as "dig best of top 3".
            for _ in 0..n {
                dig_best(state, reg, 3);
            }
            true
        }
        "Serum Visions" => {
            // draw a card, THEN scry 2 — the draw is blind; the scry improves the next draw.
            for _ in 0..n {
                draw_then_scry(state, reg, 2);
            }
            true
        }
        "Opt" => {
            // scry 1, draw a card: draw the better of the top 2.
            for _ in 0..n {
                dig_best(state, reg, 2);
            }
            true
        }
        "Consider" => {
            // surveil 1, draw a card: draw the better of the top 2; the rejected top card goes to
            // the graveyard (fuels Underworld Breach / Gale).
            for _ in 0..n {
                dig_best_surveil(state, reg, 2);
            }
            true
        }
        "Frantic Search" => {
            let cap = untap_cap("Frantic Search");
            for _ in 0..n {
                // refund the real colors of up to `cap` tapped lands (each copy untaps anew)
                let refund = untap_lands_mana(state, reg, cap);
                state.mana.add_cost(&refund);
                let take = 2.min(state.library.len());
                for _ in 0..take {
                    let c = state.library.remove(0);
                    state.hand.push(c);
                }
                let k = 2.min(state.hand.len()) as i64;
                pitch_worst(state, reg, k);
            }
            true
        }
        "Snap" => {
            let cap = untap_cap("Snap");
            for _ in 0..n {
                let refund = untap_lands_mana(state, reg, cap);
                state.mana.add_cost(&refund);
                // bounce a spent ETB tutor and recast it for another trigger (surplus-gated)
                snap_bounce_recast(state, reg);
            }
            true
        }
        "Gamble" => {
            // Each resolution searches up a card, then discards one at RANDOM. With multiple
            // resolutions (Krark/Storm copies), pick the best n cards up front and fetch them
            // LEAST-important first so the most important is added LAST — it then sits in hand
            // for the fewest subsequent random discards. wishlist::best is best-first, so rev().
            let want = (n.max(0) as usize).min(state.library.len());
            let picks = wishlist::best(state, reg, &state.library.clone(), want, true);
            for fetched in picks.into_iter().rev() {
                match state.library.iter().position(|c| *c == fetched) {
                    Some(pos) => {
                        state.library.remove(pos);
                    }
                    None => continue,
                }
                state.hand.push(fetched);
                if !state.hand.is_empty() {
                    let i = rng.gen_range(0..state.hand.len());
                    let pitched = state.hand.remove(i);
                    state.graveyard.push(pitched);
                }
            }
            true
        }
        "Pyretic Ritual" | "Desperate Ritual" => {
            state.mana.add("R", 3 * n);
            true
        }
        "Rite of Flame" => {
            state.mana.add("R", 2 * n);
            true
        }
        // Genuinely selectionless draw-1 cantrips: Peek/Probe look at an opponent's hand (irrelevant
        // in goldfish), the red ones just draw. Scry/surveil cantrips are handled above.
        "Gitaxian Probe" | "Peek" | "Borne Upon a Wind"
        | "Overmaster" | "Expedite" | "Might of the Meek"
        | "Crimson Wisps" | "Renegade Tactics" | "Accelerate" => {
            for _ in 0..n {
                if let Some(c) = pop_top(state) {
                    state.hand.push(c);
                }
            }
            true
        }
        "Brightstone Ritual" => {
            // Add {R} per Goblin on the battlefield; in this deck goblin-count == Krark bodies
            // (Krark + any clone copying him). Scales with the board the engine builds up.
            state.mana.add("R", state.krark_bodies(reg) * n);
            true
        }
        "Heroes' Hangout" => {
            // Date Night mode: exile top two, keep the better one to play (exiled_play),
            // the other is exiled face-down (unplayable). Each copy digs two anew.
            for _ in 0..n {
                let take = 2.min(state.library.len());
                if take == 0 {
                    break;
                }
                let mut drawn: Vec<String> = Vec::new();
                for _ in 0..take {
                    drawn.push(state.library.remove(0));
                }
                let keep = wishlist::best(state, reg, &drawn, 1, false)[0].clone();
                if let Some(pos) = drawn.iter().position(|c| *c == keep) {
                    drawn.remove(pos);
                }
                state.exiled_play.push(keep);
                for c in drawn {
                    state.exile.push(c);
                }
            }
            true
        }
        "Mystical Tutor" => {
            // Search for the best instant/sorcery and put it on TOP of the library (NOT into hand) —
            // the next draw retrieves it. Does NOT reduce the library, so it sets up a draw rather
            // than digging. The spell shuffles before placing on top, so Krark copies / repeated
            // casts do NOT stack: each just re-seats the single best I/S on top (n is irrelevant).
            let cands: Vec<String> = state
                .library
                .iter()
                .filter(|c| reg.get(c).is_instant_or_sorcery())
                .cloned()
                .collect();
            if let Some(best) = wishlist::best(state, reg, &cands, 1, true).first().cloned() {
                if let Some(pos) = state.library.iter().position(|c| *c == best) {
                    let card = state.library.remove(pos);
                    state.library.insert(0, card);
                }
            }
            true
        }
        "Step Through" => {
            // Wizardcycling: fetch the best library Wizard to hand. Krark is the commander, so it
            // is never in the library and can't be found. (Modeled as a {2} Wizard tutor; note the
            // real cycle is an ability and wouldn't trigger Krark/magecraft — minor over-credit.)
            const WIZARDS: &[&str] = &[
                "Dualcaster Mage", "Veyran, Voice of Duality", "Vivi Ornitier",
                "Archmage Emeritus", "Snapcaster Mage", "Gale, Waterdeep Prodigy", "Spellseeker",
            ];
            for _ in 0..n {
                if wishlist::tutor(state, reg, |c| WIZARDS.contains(&c)).is_none() {
                    break;
                }
            }
            true
        }
        "Strike It Rich" => {
            state.mana.treasures += n;
            true
        }
        "Jeska's Will" => {
            let max_hand = state.opponent_hand.iter().copied().max().unwrap_or(0);
            state.mana.add("R", max_hand * n);
            let k = (3 * n).min(state.library.len() as i64);
            for _ in 0..k {
                let c = state.library.remove(0);
                state.exiled_play.push(c);
            }
            true
        }
        "Grapeshot" | "Gut Shot" => {
            let mut d = n;
            let mut life = state.opponent_life.clone();
            while d > 0 && life.iter().any(|l| *l > 0) {
                let j = first_min_positive(&life).unwrap();
                let take = d.min(life[j]);
                life[j] -= take;
                d -= take;
                if life[j] > 0 {
                    break;
                }
            }
            state.opponent_life = life;
            true
        }
        "Brain Freeze" => {
            let mill = 3 * n;
            if choices.target.as_deref() == Some("self") {
                let take = mill.min(state.library.len() as i64);
                for _ in 0..take {
                    let c = state.library.remove(0);
                    state.graveyard.push(c);
                }
            } else {
                let mut libs = state.opponent_library.clone();
                let mut m = mill;
                while m > 0 && libs.iter().any(|l| *l > 0) {
                    let j = first_min_positive(&libs).unwrap();
                    let take = m.min(libs[j]);
                    libs[j] -= take;
                    m -= take;
                    if libs[j] > 0 {
                        break;
                    }
                }
                state.opponent_library = libs;
            }
            true
        }
        "Thassa's Oracle" => true, // creature ETB; win decided by predicate
        "Quasiduplicate" => {
            for _ in 0..n {
                let tgt = match quasi_target(state, reg) {
                    Some(t) => t,
                    None => break,
                };
                if tgt == "Krark, the Thumbless" {
                    state.battlefield.push(krark_body(
                        "Sakashima of a Thousand Faces",
                        Some("Krark, the Thumbless"),
                        true,
                    ));
                } else {
                    let mut p = Permanent::new(&tgt);
                    p.is_token = true;
                    p.summoning_sick = true;
                    state.battlefield.push(p);
                    apply_etb(state, reg, &tgt);
                }
            }
            true
        }
        _ => false,
    }
}

// --------------------------------------------------------------------------- //
// Sampling resolver (one playout step)
// --------------------------------------------------------------------------- //

pub struct ResolveLog {
    pub flips: i64,
    pub wins: i64,
    pub storm_copies: i64,
    pub resolutions: i64,
    pub triggers: Vec<(String, i64)>,
    pub warnings: Vec<String>,
}

/// Mutates `state` in place: put spell on stack, flip, resolve Storm + Krark copies + value
/// triggers, run the effect, return/resolve the original. Mirror of resolve_cast_sample with
/// copy=False (caller owns the state). `forced_wins`: if Some, fixes the won-flip count.
pub fn resolve_cast_sample<R: Rng + ?Sized>(
    state: &mut GameState,
    reg: &Registry,
    card_name: &str,
    rng: &mut R,
    choices: &Choices,
    forced_wins: Option<i64>,
) -> ResolveLog {
    // Step Through is used via Wizardcycling — an ACTIVATED ABILITY, not a spell cast. It must not
    // flip Krark, trigger magecraft, or add to storm. Just discard it (the caller does that) and
    // tutor a Wizard.
    if card_name == "Step Through" {
        run_effect(state, reg, card_name, 1, choices, rng);
        return ResolveLog {
            flips: 0,
            wins: 0,
            storm_copies: 0,
            resolutions: 1,
            triggers: Vec::new(),
            warnings: Vec::new(),
        };
    }
    let f = state.flips_per_cast(reg).min(MAX_FLIPS);
    let p = state.flip_p();
    let storm_prior = state.storm_count;
    let storm_copies = if STORM_SPELLS.contains(&card_name) {
        storm_prior
    } else {
        0
    };
    state.storm_count += 1;
    // Vivi Ornitier gets a +1/+1 counter on each noncreature CAST (this fn only runs for I/S casts,
    // which are noncreature; copies aren't "cast" so they don't count). Power persists across turns.
    if state.has_vivi() {
        state.vivi_power += 1;
    }

    // Krark's Thumb: each Krark trigger flips TWO coins and you keep one, so you steer each flip.
    // The player's goal decides the steer: to keep a free loop alive you reserve one flip as a LOSS
    // (return spell to hand) and win the rest (copies); to bank the spell's own effect you win out.
    // Continuing beats resolving when a recast is worth more than the spell resolving now: with f>=2
    // a won copy still resolves the effect at least once, and with f==1 it only pays if a cast/copy/
    // flip-win value engine fires per cast. Without an engine at f==1 we resolve (win) instead.
    let thumb = state.has_krarks_thumb();
    let has_cast_engine = thumb
        && state.value_engines(reg).iter().any(|(_, e)| {
            matches!(
                e.trigger_cause.as_deref(),
                Some("is_cast") | Some("is_cast_or_copy") | Some("coin_flip_win")
            )
        });
    let wins = match forced_wins {
        Some(fw) => fw.max(0).min(f),
        None if thumb && f > 0 => {
            // Roll two coins per trigger; classify which outcomes a trigger can be steered to.
            let mut both_heads = 0i64; // forced WIN  (can't be made a loss)
            let mut both_tails = 0i64; // forced LOSS (can't be made a win)
            for _ in 0..f {
                let a = rng.gen::<bool>();
                let b = rng.gen::<bool>();
                match (a, b) {
                    (true, true) => both_heads += 1,
                    (false, false) => both_tails += 1,
                    _ => {} // "either": free choice
                }
            }
            let eithers = f - both_heads - both_tails;
            let want_continue = f >= 2 || has_cast_engine;
            if want_continue && (both_tails + eithers) >= 1 {
                // Reserve ONE flip as the loss (a forced-loss is free; else spend an "either"),
                // win every other steerable flip.
                if both_tails >= 1 { both_heads + eithers } else { both_heads + eithers - 1 }
            } else {
                // Resolve: win every flip we can (loop ends, spell's effect banked).
                both_heads + eithers
            }
        }
        None => (0..f).filter(|_| rng.gen::<f64>() < p).count() as i64,
    };
    let all_won = f == 0 || wins == f;
    let resolutions = if f > 0 {
        wins + if all_won { 1 } else { 0 }
    } else {
        1
    };
    let magecraft_events = 1 + wins + storm_copies;

    let mut log = ResolveLog {
        flips: f,
        wins,
        storm_copies,
        resolutions,
        triggers: Vec::new(),
        warnings: Vec::new(),
    };

    // value engines (collect deltas first to avoid borrow conflicts)
    let engines: Vec<(usize, ManaCost, i64, i64, i64, i64, Option<String>, i64, String)> = state
        .value_engines(reg)
        .into_iter()
        .map(|(idx, eng)| {
            (
                idx,
                eng.mana_per_trigger.clone(),
                eng.draw_per_trigger,
                eng.treasure_per_trigger,
                eng.treasure_per_flip_win,
                eng.damage_per_trigger,
                eng.trigger_cause.clone(),
                state.value_multiplier(eng, true),
                state.battlefield[idx].effective_name().to_string(),
            )
        })
        .collect();

    for (_idx, mana_per, draw_per, treas_per, treas_flip, dmg_per, cause, mult, eff_name) in engines
    {
        let events = match cause.as_deref() {
            Some("is_cast_or_copy") => magecraft_events,
            Some("is_cast") | Some("spell_cast") => 1,
            Some("coin_flip_win") => wins,
            _ => 0,
        };
        let fires = events * mult;
        if draw_per != 0 {
            let n = (draw_per * fires).min(state.library.len() as i64);
            for _ in 0..n {
                let c = state.library.remove(0);
                state.hand.push(c);
            }
        }
        if treas_per != 0 || treas_flip != 0 {
            state.mana.treasures += (treas_per + treas_flip) * fires;
        }
        for (col, amt) in &mana_per {
            state.mana.add(col, *amt * fires);
        }
        if dmg_per != 0 && fires != 0 {
            let mut d = dmg_per * fires;
            let mut life = state.opponent_life.clone();
            while d > 0 && life.iter().any(|l| *l > 0) {
                let j = first_min_positive(&life).unwrap();
                let take = d.min(life[j]);
                life[j] -= take;
                d -= take;
            }
            state.opponent_life = life;
        }
        if fires != 0 {
            log.triggers.push((eff_name, fires));
        }
    }

    let total_instances = resolutions + storm_copies;
    let scripted = run_effect(state, reg, card_name, total_instances, choices, rng);
    if !scripted {
        log.warnings.push(format!("effect for {card_name} not scripted"));
    }

    if f == 0 || all_won {
        state.graveyard.push(card_name.to_string());
    } else {
        state.hand.push(card_name.to_string());
    }
    log
}

// --------------------------------------------------------------------------- //
// selftest — mirrors the Python module __main__ asserts
// --------------------------------------------------------------------------- //

pub fn selftest(reg: &Registry) {
    use crate::game_state::krark_body as kb;
    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::from_seed([7u8; 32]);

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    // ---- value-engine EV (P(resolve)=0.125, E[draws]=7.5) ----
    {
        let mut s = GameState {
            library: vec!["Island".to_string(); 40],
            hand: vec!["Ponder".to_string()],
            ..Default::default()
        };
        s.battlefield = vec![
            kb("Krark, the Thumbless", None, false),
            Permanent { summoning_sick: false, ..Permanent::new("Archmage Emeritus") },
            Permanent { summoning_sick: false, ..Permanent::new("Storm-Kiln Artist") },
            Permanent { summoning_sick: false, ..Permanent::new("Veyran, Voice of Duality") },
            Permanent { summoning_sick: false, ..Permanent::new("Harmonic Prodigy") },
        ];
        let a = analyze_cast(&s, reg, "Ponder");
        assert!(approx(a.p_resolve, 0.125) && approx(a.e_draws, 7.5), "Ponder EV: p_resolve={} e_draws={}", a.p_resolve, a.e_draws);
        println!("[ok] value-engine EV (P(resolve)=0.125, E[draws]=7.5)");
    }

    // ---- Grapeshot storm math: 9 prior, 1 Krark + both doublers (F=3) ----
    {
        let mut g = GameState {
            storm_count: 9,
            hand: vec!["Grapeshot".to_string()],
            opponent_life: vec![40, 40, 40],
            ..Default::default()
        };
        g.battlefield = vec![
            kb("Krark, the Thumbless", None, false),
            Permanent { summoning_sick: false, ..Permanent::new("Veyran, Voice of Duality") },
            Permanent { summoning_sick: false, ..Permanent::new("Harmonic Prodigy") },
        ];
        let ag = analyze_cast(&g, reg, "Grapeshot");
        let expect = 9.0 + 3.0 * 0.5 + 0.5f64.powi(3);
        assert!(approx(ag.e_damage, expect), "Grapeshot e_damage={} expect={}", ag.e_damage, expect);
        println!("[ok] Grapeshot E[bolts]={:.3} = 9 storm + 1.5 Krark + 0.125 original", ag.e_damage);
    }

    // ---- deterministic storm kill: no Krark, 9 prior, opps at 3 ----
    {
        let mut g2 = GameState {
            storm_count: 9,
            hand: vec!["Grapeshot".to_string()],
            opponent_life: vec![3, 3, 3],
            ..Default::default()
        };
        resolve_cast_sample(&mut g2, reg, "Grapeshot", &mut rng, &Choices::default(), None);
        assert!(g2.opponent_life.iter().all(|l| *l <= 0), "life {:?}", g2.opponent_life);
        println!("[ok] Grapeshot (storm 9, no Krark) deals 10 -> kills 3x3 table: {:?}", g2.opponent_life);
    }

    // ---- Brain Freeze self-mill feeds Thoracle ----
    {
        let mut bf = GameState {
            storm_count: 5,
            library: vec!["Island".to_string(); 18],
            hand: vec!["Brain Freeze".to_string()],
            ..Default::default()
        };
        bf.battlefield = vec![Permanent { summoning_sick: false, ..Permanent::new("Thassa's Oracle") }];
        let before = bf.library.len();
        let ch = Choices { target: Some("self".to_string()) };
        resolve_cast_sample(&mut bf, reg, "Brain Freeze", &mut rng, &ch, None);
        let dev = bf.blue_devotion(reg);
        println!(
            "[ok] Brain Freeze self-mill: library {} -> {}; devotion {} -> Thoracle {}",
            before,
            bf.library.len(),
            dev,
            if bf.library.len() as i64 <= dev { "LETHAL" } else { "pending" }
        );
    }

    // ---- INVARIANT: copies are not cast, so storm stays 1 ----
    {
        let mut inv = GameState {
            library: vec!["Island".to_string(); 200],
            storm_count: 0,
            hand: vec!["Ponder".to_string()],
            ..Default::default()
        };
        inv.battlefield = vec![
            kb("Krark, the Thumbless", None, false),
            kb("Sakashima of a Thousand Faces", Some("Krark, the Thumbless"), false),
            Permanent { summoning_sick: false, ..Permanent::new("Veyran, Voice of Duality") },
            Permanent { summoning_sick: false, ..Permanent::new("Harmonic Prodigy") },
        ];
        assert_eq!(inv.flips_per_cast(reg), 6);
        let mut r2 = rand::rngs::StdRng::from_seed([7u8; 32]);
        let log = resolve_cast_sample(&mut inv, reg, "Ponder", &mut r2, &Choices::default(), None);
        assert_eq!(inv.storm_count, 1, "storm should be 1 cast, not 1+copies");
        inv.hand.push("Grapeshot".to_string());
        let ag2 = analyze_cast(&inv, reg, "Grapeshot");
        assert_eq!(ag2.e_storm_copies, 1, "next Grapeshot sees only prior casts");
        println!("[ok] INVARIANT: Ponder made {} Krark copies, storm still = {}; next Grapeshot sees {} storm copy", log.wins, inv.storm_count, ag2.e_storm_copies);
    }

    // ---- flip math fidelity: E[copies] ~ F*p over many samples ----
    {
        let mut s = GameState {
            library: vec!["Island".to_string(); 5000],
            hand: vec!["Ponder".to_string()],
            ..Default::default()
        };
        s.battlefield = vec![
            kb("Krark, the Thumbless", None, false),
            kb("Sakashima of a Thousand Faces", Some("Krark, the Thumbless"), false),
        ]; // 2 bodies, no doublers -> F=2, p=0.5 -> E[copies]=1.0
        let mut total_wins = 0i64;
        let trials = 200_000;
        let mut r3 = rand::rngs::StdRng::from_seed([42u8; 32]);
        for _ in 0..trials {
            let mut c = s.clone();
            let log = resolve_cast_sample(&mut c, reg, "Ponder", &mut r3, &Choices::default(), None);
            total_wins += log.wins;
        }
        let e_copies = total_wins as f64 / trials as f64;
        assert!((e_copies - 1.0).abs() < 0.02, "E[copies]={e_copies} (expect ~1.0)");
        println!("[ok] flip math: 2 bodies p=0.5 -> sampled E[copies]={e_copies:.4} (expect 1.0 = F*p)");
    }

    println!("\nResolver Phase-2 selftest passed.");
}
