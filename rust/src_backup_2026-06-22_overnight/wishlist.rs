//! wishlist.rs — port of wishlist.py. What the deck most wants to draw/fetch given the board.
//! Used by selection (Ponder/Brainstorm), tutors (Gamble/ETB), the mulligan and discard.

use crate::cards::Registry;
use crate::game_state::GameState;

const PAYOFFS: &[&str] = &["Thassa's Oracle", "Grapeshot", "Brain Freeze"];
const BODIES: &[&str] = &[
    "Krark, the Thumbless", "Sakashima of a Thousand Faces", "Glasspool Mimic", "Phantasmal Image",
];
const DOUBLERS: &[&str] = &["Veyran, Voice of Duality", "Harmonic Prodigy"];
const DRAW_ENGINES: &[&str] = &["Archmage Emeritus"];
const MANA_ENGINES: &[&str] = &["Storm-Kiln Artist", "Birgi, God of Storytelling"];
const FAST_MANA: &[&str] = &[
    "Sol Ring", "Mox Diamond", "Chrome Mox", "Lotus Petal", "Arcane Signet", "Springleaf Drum",
    "Lion's Eye Diamond", "Mana Vault", "Mox Amber", "Relic of Legends",
];
const RITUALS: &[&str] = &["Pyretic Ritual", "Desperate Ritual", "Strike It Rich", "Rite of Flame"];

const COMBO_DUALCASTER: &str = "Dualcaster Mage";
const COMBO_SHIMMERS: &[&str] = &["Twinflame", "Heat Shimmer"];

const ENGINE_EXTRA: &[&str] = &[
    "Krark's Thumb", "Baral, Chief of Compliance", "Underworld Breach",
    "Lion's Eye Diamond", "Jeska's Will", "Brainstorm", "Ponder", "Frantic Search",
    "Gamble", "Pyretic Ritual", "Desperate Ritual", "Strike It Rich",
    "Gitaxian Probe", "Peek", "Borne Upon a Wind", "Rite of Flame",
    "Opt", "Consider", "Serum Visions", "Preordain",
];

pub fn is_body(name: &str) -> bool {
    BODIES.contains(&name)
}

const SINK_PERMS_EXTRA: &[&str] = &[
    "Krark's Thumb", "Baral, Chief of Compliance", "Urabrask", "Tavern Scoundrel",
    "Vivi Ornitier", "Valley Floodcaller", "Okaun, Eye of Chaos", "Zndrsplt, Eye of Wisdom",
];

/// Mirror of loops._SINK_PERMS = _BODIES | _DOUBLERS | _DRAW_ENGINES | _MANA_ENGINES | extra.
pub fn is_sink_perm(name: &str) -> bool {
    BODIES.contains(&name)
        || DOUBLERS.contains(&name)
        || DRAW_ENGINES.contains(&name)
        || MANA_ENGINES.contains(&name)
        || SINK_PERMS_EXTRA.contains(&name)
}

pub fn is_payoff(name: &str) -> bool {
    PAYOFFS.contains(&name)
}

/// Mirror of sim._ACTION's wishlist._ENGINE component (in_engine).
pub fn in_engine_pub(name: &str) -> bool {
    in_engine(name)
}

fn in_engine(name: &str) -> bool {
    BODIES.contains(&name)
        || DOUBLERS.contains(&name)
        || DRAW_ENGINES.contains(&name)
        || MANA_ENGINES.contains(&name)
        || PAYOFFS.contains(&name)
        || ENGINE_EXTRA.contains(&name)
}

fn have(s: &GameState, name: &str) -> bool {
    s.hand.iter().any(|c| c == name) || s.has_permanent(name)
}

pub fn payoff_accessible(s: &GameState) -> bool {
    PAYOFFS.iter().any(|p| {
        s.hand.iter().any(|c| c == p) || s.has_permanent(p) || s.graveyard.iter().any(|c| c == p)
    })
}

fn brain_freeze_ready(s: &GameState, reg: &Registry) -> bool {
    s.krark_bodies(reg) >= 2
        && (s.has_permanent("Archmage Emeritus")
            || s.has_permanent("Storm-Kiln Artist")
            || s.has_permanent("Birgi, God of Storytelling"))
}

fn combo_ready(s: &GameState, reg: &Registry) -> bool {
    s.krark_bodies(reg) >= 2
        && (MANA_ENGINES.iter().any(|m| s.has_permanent(m))
            || DRAW_ENGINES.iter().any(|d| s.has_permanent(d))
            || s.has_permanent("Urabrask")
            || s.has_permanent("Tavern Scoundrel")
            || s.has_permanent("Underworld Breach"))
}

fn ready_to_finish(s: &GameState, reg: &Registry, name: &str) -> bool {
    if name == "Brain Freeze" {
        brain_freeze_ready(s, reg)
    } else {
        combo_ready(s, reg)
    }
}

/// Higher = more wanted now. Mirror of wishlist.card_value.
pub fn card_value(s: &GameState, reg: &Registry, name: &str, for_tutor: bool) -> f64 {
    let mut score = 0.0;
    let is_b = is_body(name);
    let accessible = payoff_accessible(s);

    let have_dc = have(s, COMBO_DUALCASTER);
    let have_shimmer = COMBO_SHIMMERS.iter().any(|sh| have(s, sh));
    if name == COMBO_DUALCASTER && have_shimmer && !have_dc {
        score += 120.0;
    }
    if COMBO_SHIMMERS.contains(&name) && have_dc && !have_shimmer {
        score += 120.0;
    }

    let bodies = s.krark_bodies(reg);
    if is_b && (1..2).contains(&bodies) {
        score += 80.0;
    }
    if DOUBLERS.contains(&name) && s.trigger_doublers(reg) == 0 {
        score += 60.0;
    }
    if name == "Krark's Thumb" && !s.has_permanent("Krark's Thumb") {
        score += 50.0;
    }
    if DRAW_ENGINES.contains(&name) && !DRAW_ENGINES.iter().any(|d| s.has_permanent(d)) {
        score += 50.0;
    }
    if MANA_ENGINES.contains(&name) && !MANA_ENGINES.iter().any(|m| s.has_permanent(m)) {
        score += 40.0;
    }
    if name == "Jeska's Will" {
        score += if bodies >= 1 { 60.0 } else { 35.0 };
    } else if FAST_MANA.contains(&name) {
        score += 35.0;
    } else if RITUALS.contains(&name) {
        score += 20.0;
    }

    if PAYOFFS.contains(&name) {
        if !for_tutor {
            score += if name == "Thassa's Oracle" { 45.0 } else { 15.0 };
        }
        if !accessible && ready_to_finish(s, reg, name) {
            score += 100.0;
        }
    }

    if in_engine(name) && !PAYOFFS.contains(&name) {
        score += 10.0;
        if is_b {
            score += 5.0;
        }
    }
    score
}

/// The k highest-value cards from `pool`, best first. Stable like Python's sorted
/// (ties keep original order). Mirror of wishlist.best.
pub fn best(s: &GameState, reg: &Registry, pool: &[String], k: usize, for_tutor: bool) -> Vec<String> {
    let mut idx: Vec<usize> = (0..pool.len()).collect();
    // sort by -value, stable
    idx.sort_by(|&a, &b| {
        let va = card_value(s, reg, &pool[a], for_tutor);
        let vb = card_value(s, reg, &pool[b], for_tutor);
        vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal)
    });
    idx.into_iter().take(k).map(|i| pool[i].clone()).collect()
}

/// Move the highest-value library card matching `predicate` into hand. Returns the fetched
/// card name, or None. Mirror of wishlist.tutor.
pub fn tutor<F: Fn(&str) -> bool>(s: &mut GameState, reg: &Registry, predicate: F) -> Option<String> {
    let matches: Vec<String> = s.library.iter().filter(|c| predicate(c)).cloned().collect();
    if matches.is_empty() {
        return None;
    }
    let fetched = best(s, reg, &matches, 1, true)[0].clone();
    if let Some(pos) = s.library.iter().position(|c| *c == fetched) {
        s.library.remove(pos);
    }
    s.hand.push(fetched.clone());
    Some(fetched)
}
