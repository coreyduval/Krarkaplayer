//! win.rs — port of win.py. The win predicate (resolution + state wins) and loss check.

use crate::cards::{CardType, Registry};
use crate::game_state::GameState;

#[derive(Debug, Clone, Default)]
pub struct WinResult {
    pub won: bool,
    pub wtype: String, // "thoracle" | "grapeshot" | "brain_freeze_mill" | "combat" | "LOSS" | ""
    pub detail: String,
}

impl WinResult {
    fn no() -> WinResult {
        WinResult { won: false, wtype: String::new(), detail: String::new() }
    }
    fn new(won: bool, wtype: &str, detail: String) -> WinResult {
        WinResult { won, wtype: wtype.to_string(), detail }
    }
}

// --------------------------------------------------------------------------- //
// Resolution-time wins
// --------------------------------------------------------------------------- //

/// Call when `resolving` (a card name) finishes resolving, after its effect is applied.
pub fn check_resolution_win(state: &GameState, reg: &Registry, resolving: &str) -> WinResult {
    match resolving {
        "Thassa's Oracle" => thoracle(state, reg),
        "Grapeshot" => {
            let total_damage = state.storm_count + 1;
            burn_lethal(state, total_damage, "Grapeshot")
        }
        "Brain Freeze" => {
            let mill_each = 3 * (state.storm_count + 1);
            mill_table(state, mill_each, "Brain Freeze")
        }
        _ => WinResult::no(),
    }
}

fn thoracle(state: &GameState, reg: &Registry) -> WinResult {
    let dev = state.blue_devotion(reg);
    let lib = state.library.len() as i64;
    if lib <= dev {
        WinResult::new(
            true,
            "thoracle",
            format!("Thassa's Oracle resolves: blue devotion {dev} >= library {lib}."),
        )
    } else {
        WinResult::new(
            false,
            "thoracle",
            format!(
                "NOT lethal: library {lib} > devotion {dev}. Need {} more mill/draw before Thoracle is safe.",
                lib - dev
            ),
        )
    }
}

fn burn_lethal(state: &GameState, damage: i64, source: &str) -> WinResult {
    let living: Vec<i64> = state.opponent_life.iter().copied().filter(|l| *l > 0).collect();
    let needed: i64 = living.iter().sum();
    if damage >= needed && !living.is_empty() {
        WinResult::new(true, "grapeshot", format!("{source}: {damage} damage >= {needed} total opponent life."))
    } else {
        WinResult::new(false, "grapeshot", format!("NOT lethal: {source} {damage} dmg < {needed} total opponent life."))
    }
}

fn mill_table(state: &GameState, mill_each: i64, source: &str) -> WinResult {
    let libs: Vec<i64> = state.opponent_library.iter().copied().filter(|l| *l > 0).collect();
    if libs.is_empty() {
        return WinResult::new(true, "brain_freeze_mill", format!("{source}: all opponents already decked."));
    }
    let max_lib = *libs.iter().max().unwrap();
    let sum_lib: i64 = libs.iter().sum();
    if mill_each >= max_lib && mill_each * libs.len() as i64 >= sum_lib {
        WinResult::new(true, "brain_freeze_mill", format!("{source}: {mill_each} mill per target decks the table."))
    } else {
        WinResult::new(false, "brain_freeze_mill", format!("NOT lethal: {source} {mill_each} mill per target insufficient."))
    }
}

// --------------------------------------------------------------------------- //
// State-based wins
// --------------------------------------------------------------------------- //

pub fn check_state_win(state: &GameState, reg: &Registry) -> WinResult {
    if state.opponent_life.iter().all(|l| *l <= 0) {
        return WinResult::new(true, "combat", "Opponent life pool reduced to 0 (160 total damage).".into());
    }
    if state.opponent_library.iter().all(|l| *l <= 0) {
        return WinResult::new(true, "brain_freeze_mill", "All opponents decked.".into());
    }

    let inf = &state.infinite;

    if inf.contains("hasty_attackers") && has_unbounded_attacker(state, reg) {
        return WinResult::new(true, "combat", "Infinite hasty attackers (Dualcaster/Twinflame) — lethal combat.".into());
    }

    if payoff_accessible(state, reg, "Grapeshot") {
        return WinResult::new(true, "grapeshot", "Grapeshot accessible with an unbounded red-mana/storm loop.".into());
    }

    if payoff_accessible(state, reg, "Brain Freeze") {
        return WinResult::new(true, "brain_freeze_mill", "Brain Freeze accessible with an infinite storm/blue-mana loop.".into());
    }

    if payoff_accessible(state, reg, "Thassa's Oracle") {
        let draw_or_mill = inf.contains("draw") || inf.contains("mill");
        if draw_or_mill || (state.library.len() as i64) <= state.blue_devotion(reg) {
            return WinResult::new(
                true,
                "thoracle",
                "Thassa's Oracle castable and library empties (draw/mill loop) or is already <= devotion.".into(),
            );
        }
    }

    WinResult::no()
}

const GRAPESHOT_LOOPS: &[&str] = &["mana_R", "mana_any", "storm"];
const BRAINFREEZE_LOOPS: &[&str] = &["storm", "mana_U", "mana_any"];
const THORACLE_LOOPS: &[&str] = &["mana_R", "mana_U", "mana_any", "storm", "draw", "mill"];

fn inf_intersects(state: &GameState, tags: &[&str]) -> bool {
    tags.iter().any(|t| state.infinite.contains(*t))
}

/// A graveyard card castable via Underworld Breach (any nonland) or Gale (any I/S).
pub fn yard_reachable(state: &GameState, reg: &Registry, name: &str) -> bool {
    if !state.graveyard.iter().any(|c| c == name) {
        return false;
    }
    let cd = reg.get(name);
    if state.has_permanent("Underworld Breach") && !cd.types.contains(&CardType::Land) {
        return true;
    }
    if state.has_permanent("Gale, Waterdeep Prodigy") && cd.is_instant_or_sorcery() {
        return true;
    }
    false
}

pub fn payoff_accessible(state: &GameState, reg: &Registry, name: &str) -> bool {
    let in_hand = state.hand.iter().any(|c| c == name);
    let in_gy = state.graveyard.iter().any(|c| c == name);
    let in_exile = state.exiled_play.iter().any(|c| c == name);
    let yard = yard_reachable(state, reg, name);
    let on_bf = state.has_permanent(name);

    match name {
        "Grapeshot" => (in_hand || in_gy || in_exile) && inf_intersects(state, GRAPESHOT_LOOPS),
        "Brain Freeze" => (in_hand || in_gy || in_exile) && inf_intersects(state, BRAINFREEZE_LOOPS),
        "Thassa's Oracle" => {
            (in_hand || in_exile || on_bf || yard) && inf_intersects(state, THORACLE_LOOPS)
        }
        _ => on_bf || in_hand || in_exile || yard,
    }
}

fn has_unbounded_attacker(state: &GameState, reg: &Registry) -> bool {
    if state.infinite.contains("hasty_attackers") {
        return true;
    }
    state.battlefield.iter().any(|p| {
        p.functions_as(reg).types.contains(&CardType::Creature) && !p.summoning_sick
    })
}

// --------------------------------------------------------------------------- //
// Dispatcher + loss
// --------------------------------------------------------------------------- //

/// Single entry. Pass `resolving=Some(name)` at a resolution event; None for a static check.
pub fn evaluate_win(state: &GameState, reg: &Registry, resolving: Option<&str>) -> WinResult {
    if let Some(name) = resolving {
        let r = check_resolution_win(state, reg, name);
        if r.won {
            return r;
        }
    }
    check_state_win(state, reg)
}

pub fn check_loss(state: &GameState, reg: &Registry) -> WinResult {
    if state.library.is_empty() && !payoff_accessible(state, reg, "Thassa's Oracle") {
        return WinResult::new(true, "LOSS", "Library empty with no Thassa's Oracle to cash out.".into());
    }
    WinResult::no()
}

// --------------------------------------------------------------------------- //
// selftest — mirrors win.py __main__
// --------------------------------------------------------------------------- //

pub fn selftest(reg: &Registry) {
    use crate::game_state::{krark_body, Permanent};
    use std::collections::HashSet;

    // flip-count fidelity
    {
        let s = GameState {
            battlefield: vec![
                krark_body("Krark, the Thumbless", None, false),
                Permanent { summoning_sick: false, ..Permanent::new("Veyran, Voice of Duality") },
                Permanent { summoning_sick: false, ..Permanent::new("Harmonic Prodigy") },
            ],
            ..Default::default()
        };
        assert_eq!(s.krark_bodies(reg), 1);
        assert_eq!(s.trigger_doublers(reg), 2);
        assert_eq!(s.flips_per_cast(reg), 3);
        println!("[ok] win: 1 body + Veyran + Harmonic -> {} flips (expect 3)", s.flips_per_cast(reg));
    }

    // Thoracle empty library wins
    {
        let s = GameState {
            library: vec![],
            battlefield: vec![Permanent { summoning_sick: false, ..Permanent::new("Thassa's Oracle") }],
            ..Default::default()
        };
        let r = check_resolution_win(&s, reg, "Thassa's Oracle");
        assert!(r.won, "{:?}", r);
        println!("[ok] win: Thoracle, empty library, devotion {} -> WIN", s.blue_devotion(reg));
    }

    // Thoracle library 5 > devotion 2 -> not lethal
    {
        let s = GameState {
            library: vec!["Island".into(); 5],
            battlefield: vec![Permanent { summoning_sick: false, ..Permanent::new("Thassa's Oracle") }],
            ..Default::default()
        };
        let r = check_resolution_win(&s, reg, "Thassa's Oracle");
        assert!(!r.won, "{:?}", r);
        println!("[ok] win: Thoracle, library 5 > devotion 2 -> NOT lethal");
    }

    // infinite storm with no payoff -> not a win
    {
        let mut s = GameState::default();
        s.infinite = HashSet::from(["storm".to_string()]);
        assert!(!check_state_win(&s, reg).won, "infinite storm alone must NOT win");
        println!("[ok] win: infinite storm with no payoff -> NOT a win");
    }

    // infinite storm + Grapeshot in hand -> win
    {
        let mut s = GameState {
            hand: vec!["Grapeshot".into()],
            ..Default::default()
        };
        s.infinite = HashSet::from(["storm".to_string()]);
        let r = check_state_win(&s, reg);
        assert!(r.won && r.wtype == "grapeshot", "{:?}", r);
        println!("[ok] win: infinite storm + Grapeshot -> WIN");
    }

    // infinite hasty attackers -> combat win
    {
        let mut s = GameState::default();
        s.infinite = HashSet::from(["hasty_attackers".to_string()]);
        let r = check_state_win(&s, reg);
        assert!(r.won && r.wtype == "combat", "{:?}", r);
        println!("[ok] win: infinite hasty attackers -> WIN (combat)");
    }

    // deck-out loss
    {
        let s = GameState { library: vec![], ..Default::default() };
        let r = check_loss(&s, reg);
        assert!(r.won, "{:?}", r);
        println!("[ok] win: empty library, no Thoracle -> LOSS flagged");
    }

    println!("Win-predicate selftest passed.");
}
