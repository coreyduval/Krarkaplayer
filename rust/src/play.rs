//! Interactive "play against the Krarkaplayer" mode — a text REPL.
//!
//! Unlike the goldfish modes (sweep/audit/diag) the bot here faces a *human* opponent who can
//! interact: between the bot's turns you may destroy / exile / bounce its permanents, and during its
//! turn you may counter the spells worth countering. The bot fights back with its own counters —
//! and because every counter is an instant, **Krark flips for it** (win → copy / extra charge; lose
//! → return to hand to recast), so on a real go-off turn it can throw a wall of counters at you.
//!
//! The bot still chooses its plays with the full optimizer (`play_turn`/`develop`/planner). The
//! interaction lives only on `SimGame`'s real-turn methods, which the optimizer's clone-based
//! rollouts never call — so sweep/diag stay byte-identical.

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::cards::Registry;
use crate::game_state::GameState;
use crate::planner;
use crate::sim::{Removal, SimGame};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

// ── narration sink ────────────────────────────────────────────────────────────
// The interactive narration (taunts, counter-wars, flips, kills) is shared between the terminal REPL
// and the web UI. In the terminal it prints to stdout; under the web server it's captured into a
// buffer the browser polls. `narrate()` routes to whichever is active.

pub static WEB_ACTIVE: AtomicBool = AtomicBool::new(false);
pub static WEB_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Emit one narration line — to the web log when serving, else to stdout.
pub fn narrate(s: String) {
    if WEB_ACTIVE.load(Ordering::Relaxed) {
        WEB_LOG.lock().unwrap().push(s);
    } else {
        println!("{s}");
    }
}

// ── structured view for the web UI ────────────────────────────────────────────

pub struct PermView {
    pub idx: usize,
    pub name: String,
    pub copy_of: Option<String>,
    pub is_land: bool,
}

/// A snapshot of the bot's public board state for the browser panel.
pub struct GameView {
    pub turn: i64,
    pub lands: usize,
    pub treasures: i64,
    pub our_life: i64,
    pub opp_life: i64,
    pub board: Vec<PermView>,
    pub hand: Vec<String>,
}

pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_str_array(xs: &[String]) -> String {
    let items: Vec<String> = xs.iter().map(|s| json_str(s)).collect();
    format!("[{}]", items.join(","))
}

impl GameView {
    pub fn to_json(&self) -> String {
        let board: Vec<String> = self
            .board
            .iter()
            .map(|p| {
                format!(
                    "{{\"idx\":{},\"name\":{},\"copy_of\":{},\"is_land\":{}}}",
                    p.idx,
                    json_str(&p.name),
                    match &p.copy_of {
                        Some(t) => json_str(t),
                        None => "null".to_string(),
                    },
                    p.is_land
                )
            })
            .collect();
        format!(
            "{{\"turn\":{},\"lands\":{},\"treasures\":{},\"our_life\":{},\"opp_life\":{},\"board\":[{}],\"hand\":{}}}",
            self.turn,
            self.lands,
            self.treasures,
            self.our_life,
            self.opp_life,
            board.join(","),
            json_str_array(&self.hand),
        )
    }
}

// ── opponent interface ───────────────────────────────────────────────────────

/// The human opponent's answer when the bot casts a worth-countering spell.
pub enum Response {
    Resolve,
    Counter(String),
}

/// A live opponent the bot consults during its turn. The bot's *decision* engine never sees this —
/// only real-board execution does, via `SimGame`.
pub trait Opponent {
    /// The bot is casting (or has put on the stack) `obj`. Let it resolve, or counter it?
    fn respond(&mut self, obj: &str, fighting_back: bool) -> Response;
    /// Has the opponent latched "no responses, run the turn out"?
    fn auto_pass(&self) -> bool;
    /// Called at the start of each bot turn to clear the per-turn auto-pass latch.
    fn begin_turn(&mut self);
}

/// Reads the opponent's responses from stdin. `auto` latches on `pass` (or EOF) so the rest of the
/// turn runs with no further prompts.
pub struct StdinOpponent {
    auto: bool,
}

impl StdinOpponent {
    pub fn new() -> StdinOpponent {
        StdinOpponent { auto: false }
    }
}

impl Opponent for StdinOpponent {
    fn respond(&mut self, obj: &str, fighting_back: bool) -> Response {
        if self.auto {
            return Response::Resolve;
        }
        let prompt = if fighting_back {
            format!("  [Krark answers with {obj}] counter it? (Enter=let it resolve · counter [card] · pass): ")
        } else {
            format!("  [Krark casts {obj}] respond? (Enter=let it resolve · counter [card] · pass): ")
        };
        loop {
            match prompt_line(&prompt) {
                None => {
                    self.auto = true;
                    return Response::Resolve;
                }
                Some(s) => {
                    let s = s.trim();
                    let low = s.to_ascii_lowercase();
                    if s.is_empty() || low == "ok" || low == "resolve" || low == "y" || low == "yes" {
                        return Response::Resolve;
                    }
                    if low == "pass" || low == "go" || low == "done" || low == "no responses" {
                        self.auto = true;
                        return Response::Resolve;
                    }
                    if low == "counter" || low == "c" || low.starts_with("counter ") {
                        let with = s.splitn(2, char::is_whitespace).nth(1).unwrap_or("").trim().to_string();
                        return Response::Counter(with);
                    }
                    println!("  ?? Enter = let it resolve · `counter [card]` = counter it · `pass` = no more responses this turn");
                }
            }
        }
    }
    fn auto_pass(&self) -> bool {
        self.auto
    }
    fn begin_turn(&mut self) {
        self.auto = false;
    }
}

// ── what's worth countering, and what the bot fights back with ────────────────

/// Impactful spells the bot pauses on (payoffs, tutors, engines, clones/commanders, combo pieces,
/// counters). Trivial casts — cantrips, rituals, mana rocks, lands, the flips themselves — resolve
/// with no prompt.
const WORTH_COUNTERING: &[&str] = &[
    // payoffs
    "Grapeshot", "Brain Freeze",
    // combo enablers
    "Dualcaster Mage", "Twinflame", "Molten Duplication", "Heat Shimmer", "Heat Shimmer II", "Underworld Breach",
    // value engines
    "Storm-Kiln Artist", "Birgi, God of Storytelling", "Veyran, Voice of Duality",
    "Vivi Ornitier", "Urabrask", "Archmage Emeritus", "Harmonic Prodigy", "Tavern Scoundrel",
    "Valley Floodcaller", "Gale, Waterdeep Prodigy", "Roaming Throne", "Electro, Assaulting Battery",
    // tutors / card advantage spikes
    "Mystical Tutor", "Gamble", "Spellseeker", "Imperial Recruiter", "Step Through", "Jeska's Will",
    // commanders / clones
    "Krark, the Thumbless", "Sakashima of a Thousand Faces", "Glasspool Mimic",
    "Phantasmal Image", "Phyrexian Metamorph", "Mockingbird",
    // counters
    "Force of Will", "Pact of Negation", "Fierce Guardianship", "Flusterstorm",
    "An Offer You Can't Refuse", "Deflecting Swat",
];

/// Counters the bot fights back with.
const BOT_COUNTERS: &[&str] = &[
    "Force of Will", "Pact of Negation", "Fierce Guardianship", "Flusterstorm",
    "An Offer You Can't Refuse", "Deflecting Swat",
];

/// Counters the bot can always cast (free / alt cost), so mana is never the limiter on them.
const FREE_BOT_COUNTERS: &[&str] = &[
    "Force of Will",        // pitch a blue card + 1 life
    "Pact of Negation",     // free now, owes {3}{U}{U} next upkeep
    "Fierce Guardianship",  // free with a commander
    "Deflecting Swat",      // free with a commander
];

pub fn worth_countering(card: &str, _reg: &Registry) -> bool {
    WORTH_COUNTERING.contains(&card)
}

fn counter_is_free(name: &str) -> bool {
    FREE_BOT_COUNTERS.contains(&name)
}

/// First counter in hand the bot can pay for (free ones always; paid ones need a spare mana).
fn pick_counter(state: &GameState) -> Option<String> {
    for c in &state.hand {
        if BOT_COUNTERS.contains(&c.as_str()) && (counter_is_free(c) || state.mana.total() >= 1) {
            return Some(c.clone());
        }
    }
    None
}

fn pay_counter(state: &mut GameState, name: &str) {
    if counter_is_free(name) {
        if name == "Force of Will" {
            state.our_life -= 1; // pitch life
        }
        return;
    }
    let cost: HashMap<String, i64> = HashMap::from([("generic".to_string(), 1)]);
    if state.mana.can_pay(&cost) {
        state.mana.pay(&cost);
    }
}

// ── explicit Krark flips (interactive: show each result) ──────────────────────

/// Resolve `f` Krark flips explicitly (the bot steers toward winning, since for a counter both a win
/// — a copy/charge — and a loss — return-to-hand — keep the wall going). Returns the win count.
fn explicit_flips(f: i64, thumb: bool, rng: &mut StdRng) -> i64 {
    let mut wins = 0;
    for _ in 0..f {
        let won = if thumb {
            rng.gen::<bool>() || rng.gen::<bool>() // flip two, keep a head if either is one
        } else {
            rng.gen::<bool>()
        };
        if won {
            wins += 1;
        }
    }
    wins
}

// ── counter-war ───────────────────────────────────────────────────────────────

/// Top-level hook: the bot is casting `spell` (already gated to worth-countering). Returns true if it
/// ends up COUNTERED (fizzles), false if it resolves. Mutates `state` for any counters the bot spends.
pub fn handle_cast(
    state: &mut GameState,
    reg: &Registry,
    opp: &mut dyn Opponent,
    rng: &mut StdRng,
    spell: &str,
) -> bool {
    if !worth_countering(spell, reg) || opp.auto_pass() {
        return false;
    }
    match opp.respond(spell, false) {
        Response::Resolve => false,
        Response::Counter(with) => {
            let label = if with.is_empty() { "a counterspell".to_string() } else { with };
            narrate(format!("  OPP     : counters {spell} with {label}!"));
            if counter_war(state, reg, opp, rng, spell) {
                narrate(format!("  >> {spell} RESOLVES anyway!  {}", taunt(rng, true)));
                false
            } else {
                narrate(format!("  >> {spell} is COUNTERED.  {}", taunt(rng, false)));
                true
            }
        }
    }
}

/// The bot has assembled lethal and is going for the kill. Give the opponent one window to stop it;
/// the bot fights back with its full counter wall (floated go-off mana, Krark flipping for each
/// counter). Returns true if the kill goes through, false if the opponent stops it this turn.
pub fn defend_kill(
    state: &mut GameState,
    reg: &Registry,
    opp: &mut dyn Opponent,
    rng: &mut StdRng,
    kill_desc: &str,
) -> bool {
    if opp.auto_pass() {
        return true;
    }
    narrate(format!("  KRARK   : HERE IT COMES!! I'm going for the kill -> {kill_desc}!  Try an' stop me!"));
    match opp.respond(&format!("THE KILL ({kill_desc})"), false) {
        Response::Resolve => true,
        Response::Counter(with) => {
            let label = if with.is_empty() { "a counterspell".to_string() } else { with };
            narrate(format!("  OPP     : tries to stop the kill with {label}!"));
            if counter_war(state, reg, opp, rng, "the kill") {
                narrate(format!("  >> the kill RESOLVES — KRARK WINS!  {}", taunt(rng, true)));
                true
            } else {
                narrate(format!("  >> the kill is COUNTERED — Krark is stopped this turn.  {}", taunt(rng, false)));
                false
            }
        }
    }
}

/// Resolve the LIFO counter-war over `protect` (which the opponent just countered). Returns true if
/// `protect` survives to resolve.
fn counter_war(
    state: &mut GameState,
    reg: &Registry,
    opp: &mut dyn Opponent,
    rng: &mut StdRng,
    _protect: &str,
) -> bool {
    let mut charges: i64 = 0; // spare counter copies banked from won flips
    let mut guard = 0;
    loop {
        guard += 1;
        if guard > 200 {
            return true; // pathological loop — bot's wall wins
        }
        let bot_obj: String;
        if charges > 0 {
            charges -= 1;
            bot_obj = "a copied counter".to_string();
            narrate(format!("  KRARK   : nuh-uh! a copy counters your counter! ({charges} copies still banked)"));
        } else {
            match pick_counter(state) {
                None => return false, // no answer left — protect dies
                Some(cn) => {
                    if let Some(p) = state.hand.iter().position(|c| *c == cn) {
                        state.hand.remove(p);
                    }
                    pay_counter(state, &cn);
                    let f = state.flips_per_cast(reg);
                    let thumb = state.has_krarks_thumb();
                    if f == 0 {
                        state.graveyard.push(cn.clone());
                        narrate(format!("  KRARK   : I counter your counter with {cn}!"));
                    } else {
                        let wins = explicit_flips(f, thumb, rng);
                        charges += wins;
                        let bounced = wins < f;
                        if bounced {
                            state.hand.push(cn.clone()); // Krark returns it — recast it again!
                        } else {
                            state.graveyard.push(cn.clone());
                        }
                        let extra = if bounced {
                            format!(" — and {cn} bounces back to my hand, hee hee!")
                        } else {
                            String::new()
                        };
                        narrate(format!(
                            "  KRARK   : {cn}! Krark flips {f} coin(s) -> {wins} win(s) = {wins} copies{extra}"
                        ));
                    }
                    bot_obj = cn;
                }
            }
        }
        match opp.respond(&bot_obj, true) {
            Response::Resolve => return true, // bot's counter resolves → opp's counter dies → protect lives
            Response::Counter(_) => {
                narrate(format!("  OPP     : counters the bot's {bot_obj}!"));
                // loop: bot answers again (with a banked copy or a fresh counter)
            }
        }
    }
}

// ── goblin trash-talk ─────────────────────────────────────────────────────────

const TAUNTS: &[&str] = &[
    "HEYYY! No touchy my stuff! I'm gonna sic {c} on you, nyah nyah nyah!",
    "Booo! Stinky cheater! {c} says you smell like a wet goblin sock!",
    "Grrrr! You'll be SUPER sorry when I untap {c}!! >:(",
    "Waaaah! That's not faaaair! {c} was my favorite and now I'm MAD!",
    "Pbbbt! Go ahead, take it! I gots like a MILLION {c} back here, dummy!",
    "You poked the goblin! Now {c} pokes you BACK! teehee hee!",
    "Mean! Mean! MEAN! {c} is writing your name in the BIG book of losers!",
    "Oh you wanna play rough?? {c} eats your interaction for BREAKFAST, silly!",
    "*stomps little feet* I'm TELLING {c}!! You're in biiiig trouble now!",
    "hahaha you think THAT stops me? {c} says hi from your nightmares, weenie!",
    "Rude! Super duper rude! Me an' {c} are gonna flip SO many coins at your face!",
    "Nyeh! Keep it up and Krark flips TWICE (thanks {c}) and copies my whole tantrum!",
];

/// Lines for when the bot WINS a counter-war (gloating).
const WIN_TAUNTS: &[&str] = &[
    "Heehee! {c} says NICE TRY, loser!",
    "Pbbbbt! Your little counter got countered by my counter! {c} is laughing at you!",
    "Told ya! {c} an' me got a WHOLE pile more where that came from!",
    "Flip flip flip! {c} copies go BRRRR! You can't stop the goblin!",
];

/// Lines for when the bot's spell finally DOES get countered (sulking, but still cocky).
const LOSE_TAUNTS: &[&str] = &[
    "Hmmph! Fine! I got like fifty more {c} anyway, see if I care!",
    "Boooo you big meanie. {c} is gonna remember this.",
    "Ok ok you got one. But {c} says the goblin always wins in the end!",
];

const SCARY_CARDS: &[&str] = &[
    "Krark, the Thumbless", "Grapeshot", "Brain Freeze", "Storm-Kiln Artist",
    "Birgi, God of Storytelling", "Vivi Ornitier", "Urabrask", "Dualcaster Mage",
    "Tavern Scoundrel", "Lion's Eye Diamond", "Mana Vault", "Krark's Thumb",
    "Underworld Breach", "Jeska's Will", "Pact of Negation", "Force of Will",
    "Sakashima of a Thousand Faces",
];

fn one_taunt(rng: &mut StdRng, pool: &[&str]) -> String {
    let t = pool[rng.gen_range(0..pool.len())];
    let c = SCARY_CARDS[rng.gen_range(0..SCARY_CARDS.len())];
    t.replace("{c}", c)
}

/// A taunt body (no prefix) — for inline use after a result line.
fn taunt(rng: &mut StdRng, bot_won: bool) -> String {
    one_taunt(rng, if bot_won { WIN_TAUNTS } else { LOSE_TAUNTS })
}

/// A full goblin line for removal / generic gloating.
pub fn goblin_line(rng: &mut StdRng) -> String {
    format!("  KRARK   : {}", one_taunt(rng, TAUNTS))
}

// ── stdin helper ──────────────────────────────────────────────────────────────

/// Print a prompt and read one line. Returns None on EOF (piped input exhausted).
fn prompt_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut s = String::new();
    match io::stdin().read_line(&mut s) {
        Ok(0) => None,
        Ok(_) => Some(s),
        Err(_) => None,
    }
}

// ── between-turns removal window ──────────────────────────────────────────────

fn print_removable(game: &SimGame) {
    let perms = game.play_perms();
    if perms.is_empty() {
        println!("  (the Krarkaplayer has no permanents on board)");
        return;
    }
    println!("  The Krarkaplayer's permanents:");
    for (idx, name, copy_of, is_land) in perms {
        let tag = if is_land { " (land)" } else { "" };
        let cp = match copy_of {
            Some(t) => format!(" [=copy of {t}]"),
            None => String::new(),
        };
        println!("    [{idx}] {name}{tag}{cp}");
    }
}

fn removal_window(game: &mut SimGame, rng: &mut StdRng) {
    println!("\n--- BETWEEN TURNS — your move ---");
    print_removable(game);
    println!("  Commands: destroy <#> | exile <#> | bounce <#> | (blank) = pass");
    loop {
        let line = match prompt_line("  you> ") {
            Some(l) => l,
            None => break,
        };
        let line = line.trim();
        if line.is_empty() || line.eq_ignore_ascii_case("done") || line.eq_ignore_ascii_case("pass") {
            break;
        }
        let mut parts = line.split_whitespace();
        let verb = parts.next().unwrap_or("");
        let kind = match verb.to_ascii_lowercase().as_str() {
            "destroy" | "d" | "kill" => Removal::Destroy,
            "exile" | "e" => Removal::Exile,
            "bounce" | "b" | "return" => Removal::Bounce,
            _ => {
                println!("  ?? try: destroy <#> | exile <#> | bounce <#> | (blank) to pass");
                continue;
            }
        };
        let idx: Option<usize> = parts.next().and_then(|s| s.parse().ok());
        let idx = match idx {
            Some(i) => i,
            None => {
                println!("  ?? need a permanent number, e.g. `{verb} 0`");
                continue;
            }
        };
        match game.remove_permanent(idx, kind) {
            Some(name) => {
                let verb_past = match kind {
                    Removal::Destroy => "destroyed",
                    Removal::Exile => "exiled",
                    Removal::Bounce => "bounced",
                };
                println!("  >> you {verb_past} {name}.");
                println!("{}", goblin_line(rng));
                print_removable(game);
            }
            None => println!("  ?? no permanent at [{idx}]."),
        }
    }
}

// ── driver ────────────────────────────────────────────────────────────────────

/// Entry point for `krarksim play`.
pub fn run_play(reg: &Registry, deck: &[String], seed: u64, luck: u64, max_turns: i64, fast_mull: bool) {
    let mut game = SimGame::new(reg, deck, seed, 0.95, fast_mull);
    game.set_dev_seed(seed.wrapping_mul(1_000_003).wrapping_add(luck));
    game.set_send_gate(0.20);
    game.verbose = true;
    game.set_opponent(Box::new(StdinOpponent::new()));
    let mut taunt_rng = StdRng::seed_from_u64(seed ^ 0x90B1_180B_90B1_180B);

    println!("=== PLAY AGAINST THE KRARKAPLAYER — seed={seed} luck={luck} ===");
    println!("(You're a 4-player pod's worth of inert life — but you get to interact.");
    println!(" Between its turns: destroy/exile/bounce its permanents. During its turn: counter the");
    println!(" spells worth countering. It fights back, flips Krark for its counters, and gloats.)\n");
    game.print_opening();

    let det = planner::DeterministicKillSearch::default();
    let mut prob = planner::ProbabilisticPlanner {
        mc_sims: 80,
        max_first: 2,
        rollout_steps: 20,
        ..Default::default()
    };

    let mut win_turn = 0i64;
    let mut won = false;
    for t in 1..=max_turns {
        if t > 1 {
            removal_window(&mut game, &mut taunt_rng);
        }
        if game.play_turn(&det, &mut prob).is_some() {
            win_turn = t;
            won = true;
            break;
        }
        if game.dead {
            break;
        }
    }

    println!();
    if won {
        println!("=== THE KRARKAPLAYER WINS — turn {win_turn} ===");
        println!("{}", goblin_line(&mut taunt_rng));
    } else if game.dead {
        println!("=== THE KRARKAPLAYER DIED (fizzled a fatal go-off) ===");
    } else {
        println!("=== NO WIN in {max_turns} turns (the goldfish bricked) ===");
    }
    game.print_zone_inspection();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game_state::{ManaPool, Permanent};
    use std::fs;

    fn reg() -> Registry {
        let text = fs::read_to_string("../krarkashima.txt").expect("krarkashima.txt");
        Registry::load(&text)
    }

    /// Opponent that replays a fixed script of responses.
    struct Scripted {
        first: Vec<Response>, // for the initial respond(spell, false)
        war: Vec<Response>,   // for the fight-back respond(.., true) calls, in order
        fi: usize,
        wi: usize,
    }
    impl Opponent for Scripted {
        fn respond(&mut self, _obj: &str, fighting_back: bool) -> Response {
            let (v, i) = if fighting_back {
                (&self.war, &mut self.wi)
            } else {
                (&self.first, &mut self.fi)
            };
            let r = v.get(*i);
            *i += 1;
            match r {
                Some(Response::Resolve) => Response::Resolve,
                Some(Response::Counter(s)) => Response::Counter(s.clone()),
                None => Response::Resolve, // ran out of script → let it resolve
            }
        }
        fn auto_pass(&self) -> bool {
            false
        }
        fn begin_turn(&mut self) {}
    }

    fn krark(name: &str) -> Permanent {
        Permanent {
            name: name.to_string(),
            copy_of: None,
            tapped: false,
            summoning_sick: false,
            is_token: false,
            temporary: false,
            imprint: None,
        }
    }

    fn base_state() -> GameState {
        let mut s = GameState::default();
        s.battlefield.push(krark("Krark, the Thumbless"));
        s.battlefield.push(krark("Krark's Thumb"));
        s.mana = ManaPool::new();
        s.mana.treasures = 20;
        s
    }

    #[test]
    fn fight_back_saves_the_spell() {
        let reg = reg();
        let mut rng = StdRng::seed_from_u64(42);
        let mut s = base_state();
        s.hand = vec!["Force of Will".to_string(), "Pact of Negation".to_string()];
        // Opponent counters the spell, then counters the bot's first counter, then gives up.
        let mut opp = Scripted {
            first: vec![Response::Counter(String::new())],
            war: vec![Response::Counter(String::new()), Response::Resolve],
            fi: 0,
            wi: 0,
        };
        let countered = handle_cast(&mut s, &reg, &mut opp, &mut rng, "Grapeshot");
        assert!(!countered, "bot should win the counter-war and resolve Grapeshot");
        // It should have spent at least one counter fighting back.
        assert!(
            s.hand.len() < 2 || s.graveyard.iter().any(|c| BOT_COUNTERS.contains(&c.as_str())),
            "bot should have used a counter"
        );
    }

    #[test]
    fn no_counters_means_fizzle() {
        let reg = reg();
        let mut rng = StdRng::seed_from_u64(1);
        let mut s = base_state();
        s.hand = vec!["Ponder".to_string()]; // no counters to fight back with
        let mut opp = Scripted {
            first: vec![Response::Counter(String::new())],
            war: vec![],
            fi: 0,
            wi: 0,
        };
        let countered = handle_cast(&mut s, &reg, &mut opp, &mut rng, "Grapeshot");
        assert!(countered, "with no counters in hand the spell must be countered");
    }

    #[test]
    fn kill_resolves_when_opponent_passes() {
        let reg = reg();
        let mut rng = StdRng::seed_from_u64(3);
        let mut s = base_state();
        let mut opp = Scripted { first: vec![Response::Resolve], war: vec![], fi: 0, wi: 0 };
        assert!(defend_kill(&mut s, &reg, &mut opp, &mut rng, "test kill"));
    }

    #[test]
    fn kill_is_stopped_when_bot_cannot_answer() {
        let reg = reg();
        let mut rng = StdRng::seed_from_u64(3);
        let mut s = base_state();
        s.hand = vec!["Ponder".to_string()]; // no counters to defend with
        let mut opp = Scripted {
            first: vec![Response::Counter(String::new())],
            war: vec![],
            fi: 0,
            wi: 0,
        };
        assert!(!defend_kill(&mut s, &reg, &mut opp, &mut rng, "test kill"));
    }

    #[test]
    fn trivial_casts_never_prompt() {
        let reg = reg();
        let mut rng = StdRng::seed_from_u64(1);
        let mut s = base_state();
        // A cantrip is not worth countering → handle_cast resolves it without consulting the opponent.
        let mut opp = Scripted { first: vec![], war: vec![], fi: 0, wi: 0 };
        assert!(!handle_cast(&mut s, &reg, &mut opp, &mut rng, "Ponder"));
        assert_eq!(opp.fi, 0, "opponent must not be consulted for a trivial cast");
    }
}
