//! tests.rs — Phase-2 unit tests mirroring the Python modules' asserts.

use crate::cards::{parse_mana_cost, Registry};
use crate::game_state::{krark_body, plan_phyrexian, GameState, ManaPool, Permanent, LIFE_FLOOR};
use crate::resolver::analyze_cast;
use std::collections::HashMap;
use std::fs;

fn reg() -> Registry {
    // tests run with cwd = rust/, so the file is one level up.
    let text = fs::read_to_string("../krarkashima.txt").expect("krarkashima.txt");
    Registry::load(&text)
}

fn cost(pairs: &[(&str, i64)]) -> HashMap<String, i64> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

#[test]
fn parse_costs() {
    assert_eq!(parse_mana_cost("{1}{R}"), cost(&[("generic", 1), ("R", 1)]));
    assert_eq!(parse_mana_cost("{2}{U}{U}"), cost(&[("generic", 2), ("U", 2)]));
    assert_eq!(parse_mana_cost("{0}"), cost(&[("generic", 0)]));
    assert_eq!(parse_mana_cost("Land"), HashMap::new());
    // phyrexian pip is excluded from the mana cost (paid with life / colored mana at cast time)
    assert_eq!(parse_mana_cost("{U/P}"), HashMap::new());
    assert_eq!(parse_mana_cost("{R/P}"), HashMap::new());
}

#[test]
fn phyrexian_life_and_mana() {
    let r = reg();
    // Gut Shot / Gitaxian Probe: no mana in `cost`, one Phyrexian pip recorded by color.
    let gs = r.get("Gut Shot");
    assert_eq!(gs.cost, HashMap::new());
    assert_eq!(gs.phyrexian, cost(&[("R", 1)]));
    let gp = r.get("Gitaxian Probe");
    assert_eq!(gp.cost, HashMap::new());
    assert_eq!(gp.phyrexian, cost(&[("U", 1)]));

    // High life: pay the pip with 2 life, no extra mana.
    let (extra, life) = plan_phyrexian(&gp.phyrexian, 40);
    assert_eq!(life, 2);
    assert!(extra.is_empty());

    // Life at the floor: can't safely pay life, route the pip to its colored mana.
    let (extra, life) = plan_phyrexian(&gp.phyrexian, LIFE_FLOOR);
    assert_eq!(life, 0);
    assert_eq!(extra, cost(&[("U", 1)]));
}

#[test]
fn manapool_can_pay_and_pay() {
    // colored pip paid by treasure (wildcard)
    let mut mp = ManaPool::from_pairs(&[], 2);
    assert!(mp.can_pay(&cost(&[("U", 2)])));
    mp.pay(&cost(&[("U", 2)]));
    assert_eq!(mp.treasures, 0);

    // generic paid from C first, then most-abundant color, keeps scarce U
    let mut mp = ManaPool::from_pairs(&[("C", 1), ("R", 3), ("U", 1)], 0);
    mp.pay(&cost(&[("generic", 3), ("U", 1)])); // pay UU... no, U then 3 generic
    // U pip consumed the single U; generic 3 paid from C(1)+R(2) -> R left = 1
    assert_eq!(mp.get("U"), 0);
    assert_eq!(mp.get("C"), 0);
    assert_eq!(mp.get("R"), 1);
}

#[test]
fn manapool_treasures_spent_last() {
    // generic should drain floating color before treasures
    let mut mp = ManaPool::from_pairs(&[("R", 2)], 3);
    mp.pay(&cost(&[("generic", 2)]));
    assert_eq!(mp.treasures, 3, "treasures banked; floating R spent first");
    assert_eq!(mp.get("R"), 0);
}

#[test]
fn cast_cost_basic() {
    let r = reg();
    let s = GameState::default();
    // Krark {1}{R}
    assert_eq!(*s.cast_cost(&r, "Krark, the Thumbless"), cost(&[("generic", 1), ("R", 1)]));
    // Free-with-commander only when a commander is on the battlefield
    assert_eq!(
        *s.cast_cost(&r, "Fierce Guardianship"),
        r.get("Fierce Guardianship").cost.clone()
    );
    let mut s2 = GameState::default();
    s2.battlefield.push(krark_body("Krark, the Thumbless", None, false));
    assert!(s2.cast_cost(&r, "Fierce Guardianship").is_empty());
}

#[test]
fn flip_math() {
    let r = reg();
    let mut s = GameState::default();
    s.battlefield = vec![
        krark_body("Krark, the Thumbless", None, false),
        Permanent { summoning_sick: false, ..Permanent::new("Veyran, Voice of Duality") },
        Permanent { summoning_sick: false, ..Permanent::new("Harmonic Prodigy") },
    ];
    assert_eq!(s.krark_bodies(&r), 1);
    assert_eq!(s.trigger_doublers(&r), 2);
    assert_eq!(s.flips_per_cast(&r), 3);
    assert!((s.flip_p() - 0.5).abs() < 1e-9);
    s.battlefield.push(Permanent { summoning_sick: false, ..Permanent::new("Krark's Thumb") });
    assert!((s.flip_p() - 0.75).abs() < 1e-9);

    // 2 bodies + both doublers -> 6
    let mut s2 = GameState::default();
    s2.battlefield = vec![
        krark_body("Krark, the Thumbless", None, false),
        krark_body("Sakashima of a Thousand Faces", Some("Krark, the Thumbless"), false),
        Permanent { summoning_sick: false, ..Permanent::new("Veyran, Voice of Duality") },
        Permanent { summoning_sick: false, ..Permanent::new("Harmonic Prodigy") },
    ];
    assert_eq!(s2.krark_bodies(&r), 2);
    assert_eq!(s2.flips_per_cast(&r), 6);
}

#[test]
fn blue_devotion_uses_function() {
    let r = reg();
    let mut s = GameState::default();
    // Sakashima copying Krark contributes Krark's pips (0 blue), not {3}{U}
    s.battlefield.push(krark_body("Sakashima of a Thousand Faces", Some("Krark, the Thumbless"), false));
    assert_eq!(s.blue_devotion(&r), 0);
    // bare Thassa's Oracle is {U}{U} -> 2 devotion
    s.battlefield.push(Permanent::new("Thassa's Oracle"));
    assert_eq!(s.blue_devotion(&r), 2);
}

#[test]
fn analyze_grapeshot_storm() {
    let r = reg();
    let mut g = GameState {
        storm_count: 9,
        hand: vec!["Grapeshot".to_string()],
        opponent_life: vec![40, 40, 40],
        ..Default::default()
    };
    g.battlefield = vec![
        krark_body("Krark, the Thumbless", None, false),
        Permanent { summoning_sick: false, ..Permanent::new("Veyran, Voice of Duality") },
        Permanent { summoning_sick: false, ..Permanent::new("Harmonic Prodigy") },
    ];
    let a = analyze_cast(&g, &r, "Grapeshot");
    let expect = 9.0 + 3.0 * 0.5 + 0.5f64.powi(3);
    assert!((a.e_damage - expect).abs() < 1e-9);
    assert_eq!(a.e_storm_copies, 9);
    assert_eq!(a.flips, 3);
}

#[test]
fn deck_is_98() {
    let r = reg();
    let deck = crate::build_deck(&r, 8, 12);
    assert_eq!(deck.len(), 98);
    assert_eq!(deck.iter().filter(|c| *c == "Island").count(), 8);
    assert_eq!(deck.iter().filter(|c| *c == "Mountain").count(), 12);
}
