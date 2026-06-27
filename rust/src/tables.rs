//! tables.rs — shared static tables (mana sources) referenced by loops + planner.
//! Mirror of planner.MANA_SOURCES.

use crate::cards::ManaCost;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SrcMode {
    Tap,
    TapCreature,
    Sac,
    SacHand,
}

/// (mode, produced-mana). Mirror of planner.MANA_SOURCES.
pub fn mana_source(name: &str) -> Option<(SrcMode, ManaCost)> {
    let mk = |pairs: &[(&str, i64)]| -> ManaCost {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    };
    let (mode, produced): (SrcMode, ManaCost) = match name {
        "Sol Ring" => (SrcMode::Tap, mk(&[("C", 2)])),
        "Ancient Tomb" => (SrcMode::Tap, mk(&[("C", 2)])),
        "Great Furnace" => (SrcMode::Tap, mk(&[("R", 1)])),
        "Otawara, Soaring City" => (SrcMode::Tap, mk(&[("U", 1)])),
        "Seat of the Synod" => (SrcMode::Tap, mk(&[("U", 1)])),
        "Island" => (SrcMode::Tap, mk(&[("U", 1)])),
        "Mountain" => (SrcMode::Tap, mk(&[("R", 1)])),
        "Command Tower" => (SrcMode::Tap, mk(&[("*", 1)])),
        "Arcane Signet" => (SrcMode::Tap, mk(&[("*", 1)])),
        "Talisman of Creativity" => (SrcMode::Tap, mk(&[("*", 1)])),
        "Shivan Reef" => (SrcMode::Tap, mk(&[("*", 1)])),
        "Sulfur Falls" => (SrcMode::Tap, mk(&[("*", 1)])),
        "Mox Diamond" => (SrcMode::Tap, mk(&[("*", 1)])),
        "Chrome Mox" => (SrcMode::Tap, mk(&[("*", 1)])),
        "Springleaf Drum" => (SrcMode::TapCreature, mk(&[("*", 1)])),
        "Lotus Petal" => (SrcMode::Sac, mk(&[("*", 1)])),
        "Simian Spirit Guide" => (SrcMode::Sac, mk(&[("R", 1)])),
        "Lion's Eye Diamond" => (SrcMode::SacHand, mk(&[("*", 3)])),
        "Volcanic Island" => (SrcMode::Tap, mk(&[("*", 1)])),
        "Fiery Islet" => (SrcMode::Tap, mk(&[("*", 1)])),
        // Steam Vents: shock dual; sim always pays the 2-life shock to enter untapped (charged once
        // at fetch/ETB time, NOT per-tap), then taps for U/R (modeled '*').
        "Steam Vents" => (SrcMode::Tap, mk(&[("*", 1)])),
        "Mana Confluence" => (SrcMode::Tap, mk(&[("*", 1)])),
        "Mana Vault" => (SrcMode::Tap, mk(&[("C", 3)])),
        "Grim Monolith" => (SrcMode::Tap, mk(&[("C", 3)])),
        "Mox Amber" => (SrcMode::Tap, mk(&[("*", 1)])),
        "Relic of Legends" => (SrcMode::Tap, mk(&[("*", 1)])),
        _ => return None,
    };
    Some((mode, produced))
}

pub fn is_mana_source(name: &str) -> bool {
    mana_source(name).is_some()
}

/// Life paid (damage taken) each time this source is tapped for mana.
pub fn life_per_tap(name: &str) -> i64 {
    match name {
        "Ancient Tomb" => 2,
        "Mana Confluence" => 1,
        // Shivan Reef deals 1 only for colored mana; the sim always treats its output as '*'
        // (colored), so it always costs 1 here.
        "Shivan Reef" => 1,
        // Talisman of Creativity: 1 damage when tapped for {U}/{R} (sim treats output as '*').
        "Talisman of Creativity" => 1,
        // Fiery Islet (horizon land): 1 life per colored tap, like a painland.
        "Fiery Islet" => 1,
        _ => 0,
    }
}
