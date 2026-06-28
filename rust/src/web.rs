//! Web UI for the interactive "play against the Krarkaplayer" mode.
//!
//! Architecture: the game loop runs on a worker thread driving a real `SimGame` (full optimizer,
//! `verbose=false` — the web log uses the structured narration sink in `play.rs`). A `WebOpponent`
//! implements the `Opponent` trait by blocking on a channel until the browser POSTs a response. A
//! tiny_http server (main thread) exposes the state (board snapshot + narration log + current
//! prompt) over `GET /api/state` and forwards the player's actions over `POST /api/input`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use rand::rngs::StdRng;
use rand::SeedableRng;

use crate::cards::Registry;
use crate::planner;
use crate::play::{self, goblin_line, json_str, narrate, GameView, Opponent, Response, WEB_ACTIVE, WEB_LOG};
use crate::sim::{Removal, SimGame};

use tiny_http::{Header, Method, Response as HttpResponse, Server};

/// One message from the browser to the game worker.
enum ClientMsg {
    Respond(Response),
    Pass,
    Remove(usize, Removal),
    DoneRemoving,
    NewGame(u64),
}

/// State the server reads to answer `GET /api/state`.
#[derive(Default)]
struct Shared {
    awaiting: &'static str, // "none" | "response" | "removal" | "over"
    prompt: String,
    fighting_back: bool,
    result: String,
    view_json: String,
}

/// Opponent backed by the browser: publishes the prompt, then blocks for the player's input.
struct WebOpponent {
    shared: Arc<Mutex<Shared>>,
    rx: Arc<Mutex<Receiver<ClientMsg>>>,
    auto: AtomicBool,
}

impl Opponent for WebOpponent {
    fn respond(&mut self, obj: &str, fighting_back: bool) -> Response {
        if self.auto.load(Ordering::Relaxed) {
            return Response::Resolve;
        }
        {
            let mut sh = self.shared.lock().unwrap();
            sh.awaiting = "response";
            sh.prompt = obj.to_string();
            sh.fighting_back = fighting_back;
        }
        let r = loop {
            let msg = self.rx.lock().unwrap().recv();
            match msg {
                Ok(ClientMsg::Respond(r)) => break r,
                Ok(ClientMsg::Pass) => {
                    self.auto.store(true, Ordering::Relaxed);
                    break Response::Resolve;
                }
                Ok(_) => continue, // ignore removal/newgame while awaiting a response
                Err(_) => break Response::Resolve, // browser gone
            }
        };
        {
            let mut sh = self.shared.lock().unwrap();
            sh.awaiting = "none";
            sh.prompt.clear();
            sh.fighting_back = false;
        }
        r
    }
    fn auto_pass(&self) -> bool {
        self.auto.load(Ordering::Relaxed)
    }
    fn begin_turn(&mut self) {
        self.auto.store(false, Ordering::Relaxed);
    }
}

fn publish_view(shared: &Arc<Mutex<Shared>>, view: &GameView) {
    shared.lock().unwrap().view_json = view.to_json();
}

/// Between-turns removal window, driven by the browser instead of stdin.
fn web_removal_window(
    game: &mut SimGame,
    shared: &Arc<Mutex<Shared>>,
    rx: &Arc<Mutex<Receiver<ClientMsg>>>,
    taunt_rng: &mut StdRng,
) -> bool {
    shared.lock().unwrap().awaiting = "removal";
    publish_view(shared, &game.game_view());
    loop {
        let msg = rx.lock().unwrap().recv();
        match msg {
            Ok(ClientMsg::Remove(idx, kind)) => {
                if let Some(name) = game.remove_permanent(idx, kind) {
                    let verb = match kind {
                        Removal::Destroy => "destroyed",
                        Removal::Exile => "exiled",
                        Removal::Bounce => "bounced",
                    };
                    narrate(format!("  YOU     : {verb} {name}."));
                    narrate(goblin_line(taunt_rng));
                    publish_view(shared, &game.game_view());
                } else {
                    narrate(format!("  (no permanent at [{idx}])"));
                }
            }
            Ok(ClientMsg::DoneRemoving) => break,
            Ok(ClientMsg::NewGame(_)) => return false, // abandon this game, restart
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    shared.lock().unwrap().awaiting = "none";
    true
}

/// The game worker: plays games back-to-back, restarting on a NewGame message.
fn run_web_game(
    reg: Registry,
    deck: Vec<String>,
    mut seed: u64,
    shared: Arc<Mutex<Shared>>,
    rx: Arc<Mutex<Receiver<ClientMsg>>>,
) {
    loop {
        WEB_LOG.lock().unwrap().clear();
        {
            let mut sh = shared.lock().unwrap();
            sh.awaiting = "none";
            sh.result.clear();
            sh.prompt.clear();
        }
        let mut taunt_rng = StdRng::seed_from_u64(seed ^ 0x90B1_180B_90B1_180B);
        narrate("=== PLAY AGAINST THE KRARKAPLAYER ===".to_string());
        narrate("(Between its turns: destroy/exile/bounce its permanents. During its turn: counter".to_string());
        narrate(" the spells worth countering. It fights back, flips Krark for its counters, and gloats.)".to_string());

        let mut game = SimGame::new(&reg, &deck, seed, 0.95, true);
        game.set_dev_seed(seed.wrapping_mul(1_000_003));
        game.set_send_gate(0.20);
        game.verbose = false;
        game.set_opponent(Box::new(WebOpponent {
            shared: shared.clone(),
            rx: rx.clone(),
            auto: AtomicBool::new(false),
        }));

        let view = game.game_view();
        narrate(format!("  OPENING : {}", view.hand.join(", ")));
        publish_view(&shared, &view);

        let det = planner::DeterministicKillSearch::default();
        let mut prob = planner::ProbabilisticPlanner {
            mc_sims: 80,
            max_first: 2,
            rollout_steps: 20,
            ..Default::default()
        };

        let mut won = false;
        let mut win_turn = 0i64;
        let mut abandoned = false;
        for t in 1..=12i64 {
            if t > 1 && !web_removal_window(&mut game, &shared, &rx, &mut taunt_rng) {
                abandoned = true;
                break;
            }
            narrate(format!("──────── TURN {t} ────────"));
            if game.play_turn(&det, &mut prob).is_some() {
                won = true;
                win_turn = t;
                break;
            }
            publish_view(&shared, &game.game_view());
            if game.dead {
                break;
            }
        }

        if !abandoned {
            let result = if won {
                format!("THE KRARKAPLAYER WINS — turn {win_turn}!")
            } else if game.dead {
                "THE KRARKAPLAYER DIED (fizzled a fatal go-off)".to_string()
            } else {
                "NO WIN in 12 turns (the goldfish bricked)".to_string()
            };
            narrate(format!("=== {result} ==="));
            narrate(goblin_line(&mut taunt_rng));
            publish_view(&shared, &game.game_view());
            {
                let mut sh = shared.lock().unwrap();
                sh.awaiting = "over";
                sh.result = result;
            }
            // Wait for a New Game.
            loop {
                match rx.lock().unwrap().recv() {
                    Ok(ClientMsg::NewGame(s)) => {
                        seed = s;
                        break;
                    }
                    Ok(_) => continue,
                    Err(_) => return,
                }
            }
        } else {
            // abandoned mid-game by a New Game request; the message carried the new seed.
            // Pull it (it was a NewGame consumed in the removal window? no — we returned before consuming).
            seed = seed.wrapping_add(1);
        }
    }
}

/// Parse a `POST /api/input` body (a simple `verb[:arg[:arg]]` line) into a worker message.
fn parse_input(body: &str) -> Option<ClientMsg> {
    let body = body.trim();
    let mut parts = body.split(':');
    let verb = parts.next().unwrap_or("");
    match verb {
        "resolve" => Some(ClientMsg::Respond(Response::Resolve)),
        "counter" => {
            let card = parts.next().unwrap_or("").trim().to_string();
            Some(ClientMsg::Respond(Response::Counter(card)))
        }
        "pass" => Some(ClientMsg::Pass),
        "done" => Some(ClientMsg::DoneRemoving),
        "remove" => {
            let idx: usize = parts.next()?.trim().parse().ok()?;
            let kind = match parts.next()?.trim() {
                "destroy" => Removal::Destroy,
                "exile" => Removal::Exile,
                "bounce" => Removal::Bounce,
                _ => return None,
            };
            Some(ClientMsg::Remove(idx, kind))
        }
        "newgame" => {
            let seed: u64 = parts.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            Some(ClientMsg::NewGame(seed))
        }
        _ => None,
    }
}

fn state_json(shared: &Arc<Mutex<Shared>>, since: usize) -> String {
    let log = WEB_LOG.lock().unwrap();
    let total = log.len();
    let start = since.min(total);
    let new_lines: Vec<String> = log[start..].iter().map(|l| json_str(l)).collect();
    drop(log);
    let sh = shared.lock().unwrap();
    let view = if sh.view_json.is_empty() { "null" } else { sh.view_json.as_str() };
    format!(
        "{{\"awaiting\":{},\"prompt\":{},\"fighting_back\":{},\"result\":{},\"log_len\":{},\"log\":[{}],\"view\":{}}}",
        json_str(sh.awaiting),
        json_str(&sh.prompt),
        sh.fighting_back,
        json_str(&sh.result),
        total,
        new_lines.join(","),
        view,
    )
}

const INDEX_HTML: &str = include_str!("play_ui.html");

fn header(ct: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], ct.as_bytes()).unwrap()
}

/// Entry point for `krarksim serve`.
pub fn serve(reg: Registry, deck: Vec<String>, port: u16, seed: u64) {
    WEB_ACTIVE.store(true, Ordering::Relaxed);
    let shared = Arc::new(Mutex::new(Shared::default()));
    let (tx, rx): (Sender<ClientMsg>, Receiver<ClientMsg>) = mpsc::channel();
    let rx = Arc::new(Mutex::new(rx));

    let sh_worker = shared.clone();
    let rx_worker = rx.clone();
    thread::spawn(move || run_web_game(reg, deck, seed, sh_worker, rx_worker));

    let server = match Server::http(("0.0.0.0", port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("could not bind port {port}: {e}");
            return;
        }
    };
    println!("Krarkaplayer web UI running at  http://localhost:{port}   (Ctrl-C to stop)");

    for mut req in server.incoming_requests() {
        let url = req.url().to_string();
        let method = req.method().clone();
        if method == Method::Get && (url == "/" || url.starts_with("/index")) {
            let _ = req.respond(HttpResponse::from_string(INDEX_HTML).with_header(header("text/html; charset=utf-8")));
        } else if method == Method::Get && url.starts_with("/api/state") {
            let since = url
                .split_once("since=")
                .and_then(|(_, s)| s.split('&').next())
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            let body = state_json(&shared, since);
            let _ = req.respond(HttpResponse::from_string(body).with_header(header("application/json")));
        } else if method == Method::Post && url.starts_with("/api/input") {
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);
            if let Some(msg) = parse_input(&body) {
                let _ = tx.send(msg);
            }
            let _ = req.respond(HttpResponse::from_string("{\"ok\":true}").with_header(header("application/json")));
        } else {
            let _ = req.respond(HttpResponse::from_string("not found").with_status_code(404));
        }
    }
}
