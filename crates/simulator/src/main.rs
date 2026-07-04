//! Board simulator + test rig. Runs the shared `board-core` against the real
//! server over a websocket and renders the 64x32 panel in an egui window, with a
//! separate control window: an encoder (rotate/press), gesture buttons, and
//! failure-injection toggles for exercising the setup flow without hardware.
//!
//! `--headless` runs the websocket/board pipeline with no GUI (prints frame stats).

use board_core::{Board, Gesture, Mode};
use eframe::egui;
use futures_util::{SinkExt, StreamExt};
use scoreboard_protocol::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const SERVER_WS: &str = "ws://127.0.0.1:7878/ws";
const BOARD_ID: &str = "sim-board-01";

/// Failure toggles the (future) setup-flow simulation will read.
#[derive(Default)]
struct Failures {
    wifi_fail: bool,
    bad_code: bool,
    server_unreachable: bool,
    ble_fail: bool,
}

/// Parse `--server <url>`, `--board <id>`, `--headless` (defaults = the consts).
fn parse_args() -> (String, String, bool) {
    let mut server = SERVER_WS.to_string();
    let mut board = BOARD_ID.to_string();
    let mut headless = false;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--headless" => headless = true,
            "--server" => { i += 1; server = args.get(i).cloned().unwrap_or(server); }
            "--board" => { i += 1; board = args.get(i).cloned().unwrap_or(board); }
            _ => {}
        }
        i += 1;
    }
    (server, board, headless)
}

fn main() -> eframe::Result<()> {
    let (server, board_id, headless) = parse_args();
    println!("simulator: server={server} board={board_id}");
    let board = Arc::new(Mutex::new(Board::new()));
    let failures = Arc::new(Mutex::new(Failures::default()));

    // Websocket client feeds the shared board on its own runtime/thread.
    let board_ws = board.clone();
    let failures_ws = failures.clone();
    let server_ws = server.clone();
    let board_id_ws = board_id.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(run_client(board_ws, failures_ws, server_ws, board_id_ws));
    });

    if headless {
        // No GUI: tick the board and print frame stats for a while.
        let start = Instant::now();
        for _ in 0..80 {
            let lit = {
                let mut b = board.lock().unwrap();
                b.tick(start.elapsed().as_millis() as u64);
                b.framebuffer().iter().filter(|&&p| p != 0).count()
            };
            println!("headless: mode={:?} lit={}", board.lock().unwrap().mode(), lit);
            std::thread::sleep(Duration::from_millis(200));
        }
        return Ok(());
    }

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Scoreboard Simulator")
            .with_inner_size([960.0, 440.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Scoreboard Simulator",
        opts,
        Box::new(move |_cc| Ok(Box::new(SimApp { board, failures, tex: None, start: Instant::now(), holding_reset: false }))),
    )
}

struct SimApp {
    board: Arc<Mutex<Board>>,
    failures: Arc<Mutex<Failures>>,
    tex: Option<egui::TextureHandle>,
    start: Instant,
    holding_reset: bool,
}

fn fb_to_image(fb: &[u32]) -> egui::ColorImage {
    let mut rgba = Vec::with_capacity(FRAME_PIXELS * 4);
    for &c in fb {
        rgba.extend_from_slice(&[(c >> 16) as u8, (c >> 8) as u8, c as u8, 255]);
    }
    egui::ColorImage::from_rgba_unmultiplied([FRAME_WIDTH, FRAME_HEIGHT], &rgba)
}

impl eframe::App for SimApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // keyboard shortcuts
        let now_ms = self.start.elapsed().as_millis() as u64;
        ctx.input(|i| {
            let mut b = self.board.lock().unwrap();
            if i.key_pressed(egui::Key::ArrowRight) { b.encoder(1); }
            if i.key_pressed(egui::Key::ArrowLeft) { b.encoder(-1); }
            if i.key_pressed(egui::Key::Space) { b.gesture(Gesture::Single); }
            if i.key_pressed(egui::Key::D) { b.gesture(Gesture::Double); }
            if i.key_pressed(egui::Key::L) { b.gesture(Gesture::Long); }
            // Hold R to preview the factory-reset animation (simulates a button hold).
            let r_down = i.key_down(egui::Key::R);
            if r_down && !self.holding_reset { b.button_down(now_ms); self.holding_reset = true; }
            if !r_down && self.holding_reset { b.button_up(now_ms); self.holding_reset = false; }
        });

        // tick + grab the framebuffer + status (incl. server/app-controlled values)
        let (fb, mode, knob, app_bright, power) = {
            let mut b = self.board.lock().unwrap();
            b.tick(now_ms);
            (b.framebuffer(), b.mode(), b.brightness(), b.server_brightness(), b.power())
        };
        let img = fb_to_image(&fb);
        let tex = self
            .tex
            .get_or_insert_with(|| ctx.load_texture("panel", img.clone(), egui::TextureOptions::NEAREST));
        tex.set(img, egui::TextureOptions::NEAREST);
        let tex_id = tex.id();

        // Controls live in a right side-panel of the SAME window (single-window sim).
        egui::SidePanel::right("controls").resizable(false).default_width(280.0).show(ctx, |ui| {
            ui.heading("Encoder");
            ui.horizontal(|ui| {
                if ui.button("◀ rotate −").clicked() { self.board.lock().unwrap().encoder(-1); }
                if ui.button("rotate + ▶").clicked() { self.board.lock().unwrap().encoder(1); }
            });
            if ui.button("Press  (sleep / wake)").clicked() { self.board.lock().unwrap().gesture(Gesture::Single); }
            if ui.button("Double-press  (sync)").clicked() { self.board.lock().unwrap().gesture(Gesture::Double); }
            if ui.button("Long-press  (setup screen)").clicked() { self.board.lock().unwrap().gesture(Gesture::Long); }
            ui.small("Hold R to factory-reset (fills a bar, then wipes Wi-Fi)");

            ui.separator();
            ui.heading("Simulate failures");
            let mut f = self.failures.lock().unwrap();
            ui.label("Board goes offline:");
            ui.checkbox(&mut f.wifi_fail, "WiFi connect fails");
            ui.checkbox(&mut f.server_unreachable, "Server unreachable");
            ui.label("Pairing fails (set before double-press):");
            ui.checkbox(&mut f.bad_code, "Bad pairing code");
            ui.checkbox(&mut f.ble_fail, "BLE handshake fails");

            ui.separator();
            ui.small("Keys: ←/→ rotate · Space press · D double · L setup · hold R reset");
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Panel (64×32)");
            let size = egui::vec2(FRAME_WIDTH as f32, FRAME_HEIGHT as f32) * 10.0;
            ui.image(egui::load::SizedTexture::new(tex_id, size));
            ui.label(format!(
                "mode: {mode:?} · power: {} · brightness: {app_bright} · knob: {knob}",
                if power { "on" } else { "off" }
            ));
        });

        // Keep animating, but at ~30fps (the real panel's refresh) instead of
        // repainting flat-out — an uncapped repaint loop pins a whole core and
        // made time-based animations look like a blur.
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

/// Deterministic dev identity: seed the Ed25519 key from the `--board` string so the
/// board_id (= pubkey) is stable across runs with no key file. A real device mints a
/// random key in flash; the simulator doesn't need that secrecy.
fn dev_secret(board_id: &str) -> [u8; 32] {
    let mut secret = [0u8; 32];
    let bytes = board_id.as_bytes();
    let n = bytes.len().min(32);
    secret[..n].copy_from_slice(&bytes[..n]);
    secret
}

async fn run_client(
    board: Arc<Mutex<Board>>,
    failures: Arc<Mutex<Failures>>,
    server: String,
    board_id: String,
) {
    let secret = dev_secret(&board_id);
    let mut prev_setup = false;
    loop {
        // Simulated "no path to the server" (WiFi down / server unreachable).
        if { let f = failures.lock().unwrap(); f.wifi_fail || f.server_unreachable } {
            board.lock().unwrap().set_online(false);
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        match connect_async(&server).await {
            Ok((ws, _)) => {
                println!("connected to {server}");
                board.lock().unwrap().set_online(true);
                let (mut write, mut read) = ws.split();
                let mut poll = tokio::time::interval(Duration::from_millis(200));

                // Re-announce an in-progress setup after a (re)connect.
                {
                    let b = board.lock().unwrap();
                    if matches!(b.mode(), Mode::Setup | Mode::Syncing) {
                        let code = b.setup_code();
                        drop(b);
                        let m = ClientMsg::EnterSetup { code };
                        let _ = write.send(Message::Text(serde_json::to_string(&m).unwrap())).await;
                        prev_setup = true;
                    }
                }

                loop {
                    tokio::select! {
                        msg = read.next() => match msg {
                            Some(Ok(Message::Text(t))) => {
                                if let Ok(sm) = serde_json::from_str::<ServerMsg>(&t) {
                                    match sm {
                                        ServerMsg::Hello { challenge } => {
                                            let auth = ClientMsg::Auth {
                                                role: Role::Device,
                                                pubkey: pubkey_hex(&secret),
                                                sig: sign_hex(&secret, challenge.as_bytes()),
                                            };
                                            let _ = write
                                                .send(Message::Text(serde_json::to_string(&auth).unwrap()))
                                                .await;
                                        }
                                        ServerMsg::Manifest(m) => {
                                            let need = board.lock().unwrap().on_manifest(m);
                                            if let Some(n) = need {
                                                let _ = write
                                                    .send(Message::Text(serde_json::to_string(&n).unwrap()))
                                                    .await;
                                            }
                                        }
                                        ServerMsg::SetupDone => {
                                            println!("paired — leaving setup");
                                            board.lock().unwrap().setup_done();
                                            prev_setup = false;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            Some(Ok(Message::Binary(b))) => {
                                if let Some((id, px)) = decode_frame(&b) {
                                    board.lock().unwrap().on_frame(id, px.to_vec());
                                }
                            }
                            Some(Ok(_)) => {}
                            _ => break, // closed or error
                        },
                        _ = poll.tick() => {
                            // Drop the connection if the user toggled us offline.
                            if { let f = failures.lock().unwrap(); f.wifi_fail || f.server_unreachable } {
                                break;
                            }
                            // Detect entering setup/sync; announce a pairing code.
                            let new_code = {
                                let mut b = board.lock().unwrap();
                                let in_setup = matches!(b.mode(), Mode::Setup | Mode::Syncing);
                                let nc = if in_setup && !prev_setup {
                                    let code = gen_code();
                                    b.set_setup_code(code.clone());
                                    // "Bad code" / "BLE handshake fails": announce a code that
                                    // won't match what's shown, so the app's Pair fails.
                                    let pair_fail = { let f = failures.lock().unwrap(); f.bad_code || f.ble_fail };
                                    Some(if pair_fail { wrong_code(&code) } else { code })
                                } else {
                                    None
                                };
                                prev_setup = in_setup;
                                nc
                            };
                            if let Some(code) = new_code {
                                println!("announcing setup code {code}");
                                let m = ClientMsg::EnterSetup { code };
                                let _ = write.send(Message::Text(serde_json::to_string(&m).unwrap())).await;
                            }
                            // Ship board-generated messages (e.g. knob press -> ButtonPress).
                            let outgoing = board.lock().unwrap().take_outgoing();
                            for m in outgoing {
                                let _ = write.send(Message::Text(serde_json::to_string(&m).unwrap())).await;
                            }
                        }
                    }
                }
                board.lock().unwrap().set_online(false);
                eprintln!("disconnected; reconnecting in 2s…");
            }
            Err(e) => {
                board.lock().unwrap().set_online(false);
                eprintln!("connect failed: {e}; retrying in 2s…");
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// A code guaranteed to differ from `code` (simulates a bad/failed pairing handshake).
fn wrong_code(code: &str) -> String {
    let n: u32 = code.parse().unwrap_or(0);
    format!("{:06}", (n + 1) % 1_000_000)
}

/// A short, non-cryptographic 6-digit pairing code (friends-and-family scale).
fn gen_code() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{:06}", n % 1_000_000)
}
