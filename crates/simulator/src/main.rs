//! Board simulator + test rig. Runs the shared `board-core` against the real
//! server over a websocket and renders the 64x32 panel in an egui window, with a
//! separate control window: an encoder (rotate/press), gesture buttons, and
//! failure-injection toggles for exercising the setup flow without hardware.
//!
//! `--headless` runs the websocket/board pipeline with no GUI (prints frame stats).

use board_core::{Board, Diagnostics, Gesture, SetupPhase};
use eframe::egui;
use futures_util::{SinkExt, StreamExt};
use scoreboard_protocol::*;
use std::sync::atomic::{AtomicBool, Ordering};
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

struct Args {
    server: String,
    board: String,
    headless: bool,
    /// IANA zone reported to the server as this "device's" location. The
    /// firmware IP-geolocates one; the sim's closest analogue is the host's.
    tz: String,
}

/// Parse `--server <url>`, `--board <id>`, `--tz <zone>`, `--headless`
/// (defaults = the consts / the host timezone).
fn parse_args() -> Args {
    let mut a = Args {
        server: SERVER_WS.to_string(),
        board: BOARD_ID.to_string(),
        headless: false,
        tz: host_timezone(),
    };
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--headless" => a.headless = true,
            "--server" => { i += 1; a.server = args.get(i).cloned().unwrap_or(a.server); }
            "--board" => { i += 1; a.board = args.get(i).cloned().unwrap_or(a.board); }
            "--tz" => { i += 1; a.tz = args.get(i).cloned().unwrap_or(a.tz); }
            _ => {}
        }
        i += 1;
    }
    a
}

/// The host's IANA zone. Without one the server renders this board's clock and
/// start times in ITS zone, so every time on the panel differs from what the
/// same manifest would show on hardware — the sim stops being a preview.
fn host_timezone() -> String {
    if let Ok(tz) = std::env::var("TZ") {
        if !tz.is_empty() {
            return tz;
        }
    }
    // macOS and Linux both symlink /etc/localtime into the zoneinfo tree; the
    // components after `zoneinfo` are the IANA name (America/Los_Angeles).
    if let Ok(path) = std::fs::read_link("/etc/localtime") {
        let parts: Vec<String> =
            path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        if let Some(i) = parts.iter().position(|p| p == "zoneinfo") {
            let name = parts[i + 1..].join("/");
            if !name.is_empty() {
                return name;
            }
        }
    }
    "UTC".to_string()
}

/// "ws://127.0.0.1:7878/ws" -> "127.0.0.1:7878" (the DEBUG INFO screen's SRV row).
fn server_host_port(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split('/').next().unwrap_or(rest).to_string()
}

/// A board exactly as a fresh boot would produce it: version line + the fake
/// diagnostics the DEBUG INFO screen shows. The server row is the REAL target
/// URL, so the menu stays honest across server switches.
fn fresh_board(server: &str, board_id: &str) -> Board {
    let mut b = Board::new();
    // Same version line the firmware puts on its boot screen.
    b.set_version(env!("CARGO_PKG_VERSION").to_string());
    // Plausible fake diagnostics for the settings menu's DEBUG INFO screen —
    // the sim has no radio, but the screen should look like the real thing.
    b.set_diagnostics(Diagnostics {
        ssid: Some("SimNet".into()),
        rssi_dbm: Some(-55),
        ip: Some([192, 168, 1, 42]),
        server: Some(server_host_port(server)),
        board_id_prefix: Some(board_id.chars().take(8).collect()),
        free_heap_bytes: Some(123_456),
    });
    b
}

/// Start (or restart) the websocket client on its own runtime/thread, aimed at
/// `server`. Returns the thread's liveness flag: clearing it retires the loop
/// within one poll tick, which is how the server picker swaps targets without
/// relaunching the process — the sim equivalent of rebooting the board onto a
/// different server.
fn spawn_client(
    board: Arc<Mutex<Board>>,
    failures: Arc<Mutex<Failures>>,
    server: String,
    board_id: String,
    tz: String,
) -> Arc<AtomicBool> {
    let alive = Arc::new(AtomicBool::new(true));
    let alive_ws = alive.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(run_client(board, failures, server, board_id, tz, alive_ws));
    });
    alive
}

fn main() -> eframe::Result<()> {
    let Args { server, board: board_id, headless, tz } = parse_args();
    println!("simulator: server={server} board={board_id} tz={tz}");
    // The Wi-Fi leg is driven by the client loop (mirroring the firmware's
    // per-pass link check), so the "WiFi connect fails" toggle renders the
    // Wi-Fi hop down instead of blaming the server.
    let board = Arc::new(Mutex::new(fresh_board(&server, &board_id)));
    let failures = Arc::new(Mutex::new(Failures::default()));
    let client_alive = spawn_client(
        board.clone(),
        failures.clone(),
        server.clone(),
        board_id.clone(),
        tz.clone(),
    );

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
        Box::new(move |_cc| {
            Ok(Box::new(SimApp {
                board,
                failures,
                board_id,
                tz,
                custom_url: if server_preset_label(&server).is_none() { server.clone() } else { String::new() },
                server,
                client_alive,
                tex: None,
                start: Instant::now(),
                holding_reset: false,
                holding_knob: false,
                scroll_accum: 0.0,
                offer_update: false,
                updating: false,
                setup_phase: SetupPhase::WaitingBluetooth,
            }))
        }),
    )
}

/// The same fixed targets the iOS app's server picker offers, so the two
/// tools speak about the same worlds. Returns the preset label for a URL, or
/// None when it's custom.
const SERVER_PRESETS: &[(&str, &str)] = &[
    ("Localhost", "ws://127.0.0.1:7878/ws"),
    ("Prod", "ws://hayabusa.proxy.rlwy.net:47958/ws"),
    // The demo deployment is gone: demo data is a per-board toggle on the live
    // server now, so there is no separate URL to preset.
];

fn server_preset_label(url: &str) -> Option<&'static str> {
    SERVER_PRESETS.iter().find(|(_, u)| *u == url).map(|(l, _)| *l)
}

struct SimApp {
    board: Arc<Mutex<Board>>,
    failures: Arc<Mutex<Failures>>,
    /// Everything a server switch needs to relaunch the client: identity and
    /// zone are per-process, the URL and liveness flag are per-connection.
    board_id: String,
    tz: String,
    server: String,
    client_alive: Arc<AtomicBool>,
    /// The custom-URL text box (seeded from --server when it isn't a preset).
    custom_url: String,
    tex: Option<egui::TextureHandle>,
    start: Instant,
    holding_reset: bool,
    /// Mouse held on the "Hold knob" button (drives the same press/release
    /// path as the R key, so the erase hold works pointer-only too).
    holding_knob: bool,
    /// Mouse-wheel remainder: the wheel is the sim's encoder, ~24px a detent.
    scroll_accum: f32,
    /// Pretend the server advertised a newer firmware (arms the UPDATE row).
    offer_update: bool,
    /// OTA in flight (blanks the panel) — the firmware sets this from the
    /// download task, which the sim has no equivalent of.
    updating: bool,
    /// Which BLE-provisioning stage the setup screen shows. The firmware walks
    /// these as the handshake progresses; here they're a selector, or the
    /// WaitingCreds/Saved screens are unreachable in the sim.
    setup_phase: SetupPhase,
}

fn fb_to_image(fb: &[u32]) -> egui::ColorImage {
    let mut rgba = Vec::with_capacity(FRAME_PIXELS * 4);
    for &c in fb {
        rgba.extend_from_slice(&[(c >> 16) as u8, (c >> 8) as u8, c as u8, 255]);
    }
    egui::ColorImage::from_rgba_unmultiplied([FRAME_WIDTH, FRAME_HEIGHT], &rgba)
}

impl SimApp {
    /// Point the sim at a different server: retire the current client thread,
    /// reset the board as a reboot would (fresh connect screen, fresh
    /// diagnostics with the new target), and launch a new client. The old
    /// thread notices its liveness flag within one poll tick; until then it
    /// writes into the board we just reset, which the new connection
    /// immediately overwrites — the same benign overlap a real reboot has.
    fn restart_with(&mut self, url: String) {
        if url == self.server {
            return;
        }
        println!("switching server -> {url}");
        self.client_alive.store(false, Ordering::Release);
        *self.board.lock().unwrap() = fresh_board(&url, &self.board_id);
        self.client_alive = spawn_client(
            self.board.clone(),
            self.failures.clone(),
            url.clone(),
            self.board_id.clone(),
            self.tz.clone(),
        );
        self.server = url;
    }
}

impl eframe::App for SimApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // keyboard shortcuts
        let now_ms = self.start.elapsed().as_millis() as u64;
        ctx.input(|i| {
            let mut b = self.board.lock().unwrap();
            if i.key_pressed(egui::Key::ArrowRight) { b.encoder(1); }
            if i.key_pressed(egui::Key::ArrowLeft) { b.encoder(-1); }
            // Mouse wheel = the encoder, ~24px a detent. Wheel-down sends
            // encoder(-1): board-core's menu walks DOWN the list on negative
            // deltas (the owner-tuned direction flip), so wheel-down still
            // matches list intuition — and outside the menu it dims, volume
            // knob style.
            self.scroll_accum += i.raw_scroll_delta.y;
            while self.scroll_accum <= -24.0 { b.encoder(-1); self.scroll_accum += 24.0; }
            while self.scroll_accum >= 24.0 { b.encoder(1); self.scroll_accum -= 24.0; }
            if i.key_pressed(egui::Key::Space) { b.gesture(Gesture::Single); }
            // Double-tap toggles the settings menu (open when closed, close
            // when open).
            if i.key_pressed(egui::Key::D) { b.gesture(Gesture::Double); }
            // A long press restarts provisioning, and board-core rewinds the
            // phase with it — keep the selector honest.
            if i.key_pressed(egui::Key::L) { b.gesture(Gesture::Long); self.setup_phase = SetupPhase::WaitingBluetooth; }
            // Hold R for the erase-only hold ladder: amber warm-up bar, red
            // KEEP-HOLDING phase at 2s, factory reset at 10s (simulates the
            // physical hold; the menu lives on double-tap now).
            let r_down = i.key_down(egui::Key::R);
            if r_down && !self.holding_reset { b.button_down(now_ms); self.holding_reset = true; }
            if !r_down && self.holding_reset { b.button_up(now_ms); self.holding_reset = false; }
        });

        // tick + grab the framebuffer + status (incl. server/app-controlled values)
        let (fb, mode, brightness, power, menu) = {
            let mut b = self.board.lock().unwrap();
            b.tick(now_ms);
            (b.framebuffer(), b.mode(), b.brightness(), b.power(), b.menu_open())
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
            if ui.button("Press  (sleep / wake · menu select)").clicked() { self.board.lock().unwrap().gesture(Gesture::Single); }
            if ui.button("Double-press  (open / close menu)").clicked() { self.board.lock().unwrap().gesture(Gesture::Double); }
            if ui.button("Long-press  (setup screen)").clicked() {
                self.board.lock().unwrap().gesture(Gesture::Long);
                self.setup_phase = SetupPhase::WaitingBluetooth;
            }
            // A real press-and-HOLD for the mouse: down starts the erase-only
            // hold (red KEEP-HOLDING phase at 2s, wipe at 10s), up releases.
            let hold_btn = ui.button("HOLD me  (erase at 10s)");
            let down = hold_btn.is_pointer_button_down_on();
            if down && !self.holding_knob { self.board.lock().unwrap().button_down(now_ms); self.holding_knob = true; }
            if !down && self.holding_knob { self.board.lock().unwrap().button_up(now_ms); self.holding_knob = false; }
            ui.small("or hold R · in the menu: wheel/← → moves, Space selects, hold ~2s closes");

            ui.separator();
            ui.heading("Server");
            // Mirrors the app's picker; selecting relaunches the client at the
            // new target, board rebooting-style (fresh connect screen).
            let current_label =
                server_preset_label(&self.server).unwrap_or("Custom").to_string();
            let mut picked: Option<String> = None;
            egui::ComboBox::from_id_salt("server-picker")
                .selected_text(&current_label)
                .show_ui(ui, |ui| {
                    for (label, url) in SERVER_PRESETS {
                        if ui
                            .selectable_label(self.server == *url, *label)
                            .clicked()
                        {
                            picked = Some((*url).to_string());
                        }
                    }
                });
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.custom_url)
                        .hint_text("ws://host:port/ws")
                        .desired_width(180.0),
                );
                let url = self.custom_url.trim().to_string();
                if ui
                    .add_enabled(
                        !url.is_empty() && url != self.server,
                        egui::Button::new("Connect"),
                    )
                    .clicked()
                {
                    picked = Some(url);
                }
            });
            ui.small(format!("connected target: {}", self.server));
            if let Some(url) = picked {
                self.restart_with(url);
            }

            ui.separator();
            ui.heading("Device state");
            // Firmware-driven states the sim has no source for: without these
            // the OTA blank and two of the three setup screens can only be seen
            // on hardware.
            if ui.checkbox(&mut self.updating, "OTA updating (panel blanks)").changed() {
                let p = if self.updating { Some(0) } else { None };
                self.board.lock().unwrap().set_updating(p);
            }
            // Arms the menu's UPDATE row without a real hosted image (the
            // server's UpdateAvailable push also drives this when connected).
            if ui.checkbox(&mut self.offer_update, "Server offers newer firmware").changed() {
                let v = if self.offer_update { Some("2026.99.0".to_string()) } else { None };
                self.board.lock().unwrap().set_update_available(v);
            }
            ui.label("Setup phase (long-press to see it):");
            let mut phase = self.setup_phase;
            ui.horizontal(|ui| {
                ui.radio_value(&mut phase, SetupPhase::WaitingBluetooth, "Bluetooth");
                ui.radio_value(&mut phase, SetupPhase::WaitingCreds, "Wi-Fi");
                ui.radio_value(&mut phase, SetupPhase::Saved, "Saved");
            });
            if phase != self.setup_phase {
                self.setup_phase = phase;
                self.board.lock().unwrap().set_setup_phase(phase);
            }

            ui.separator();
            ui.heading("Simulate failures");
            let mut f = self.failures.lock().unwrap();
            ui.label("Board goes offline:");
            ui.checkbox(&mut f.wifi_fail, "WiFi connect fails");
            ui.checkbox(&mut f.server_unreachable, "Server unreachable");
            ui.label("Pairing fails (set before opening PAIR CODE):");
            ui.checkbox(&mut f.bad_code, "Bad pairing code");
            ui.checkbox(&mut f.ble_fail, "BLE handshake fails");

            ui.separator();
            ui.small("Keys: ←/→/wheel rotate · Space press · D double (menu) · L setup · hold R erase");
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Panel (64×32)");
            let size = egui::vec2(FRAME_WIDTH as f32, FRAME_HEIGHT as f32) * 10.0;
            ui.image(egui::load::SizedTexture::new(tex_id, size));
            ui.label(format!(
                "mode: {mode:?} · power: {} · brightness: {brightness}{}",
                if power { "on" } else { "off" },
                if menu { " · MENU" } else { "" }
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
    tz: String,
    alive: Arc<AtomicBool>,
) {
    let secret = dev_secret(&board_id);
    let mut prev_setup = false;
    loop {
        // Retired by a server switch: stop touching the (already reset) board.
        if !alive.load(Ordering::Acquire) {
            return;
        }
        // Mirror the firmware's per-pass link check (`stack.config_v4()`): the
        // Wi-Fi leg is up unless the toggle says it failed. Hard-coding it up
        // made "WiFi connect fails" render the SERVER leg down — the one screen
        // the toggle exists to exercise was the one it couldn't produce.
        let (wifi_fail, server_down) = {
            let f = failures.lock().unwrap();
            (f.wifi_fail, f.server_unreachable)
        };
        board.lock().unwrap().set_wifi(!wifi_fail);
        // Simulated "no path to the server" (WiFi down / server unreachable).
        if wifi_fail || server_down {
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
                // `pairing_active` (not `mode()`): the code screen can be up
                // via the settings menu, which the mode machine can't see.
                {
                    let b = board.lock().unwrap();
                    if b.pairing_active() {
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
                                            // Same post-auth reports the firmware sends. The
                                            // zone is the load-bearing one: without it the
                                            // server renders this board's clock and start
                                            // times in ITS timezone, so the sim previews
                                            // frames hardware would never show.
                                            for m in [
                                                ClientMsg::DeviceTimezone { tz: tz.clone() },
                                                ClientMsg::DeviceVersion {
                                                    version: env!("CARGO_PKG_VERSION").to_string(),
                                                },
                                            ] {
                                                let _ = write
                                                    .send(Message::Text(serde_json::to_string(&m).unwrap()))
                                                    .await;
                                            }
                                        }
                                        // Without this the simulator never
                                        // exercises the local clock path.
                                        ServerMsg::Time { local_epoch_ms } => {
                                            board.lock().unwrap().set_time(local_epoch_ms);
                                        }
                                        ServerMsg::Manifest(m) => {
                                            let need = board.lock().unwrap().on_manifest(m);
                                            if let Some(n) = need {
                                                let _ = write
                                                    .send(Message::Text(serde_json::to_string(&n).unwrap()))
                                                    .await;
                                            }
                                        }
                                        ServerMsg::NeedsPairing => {
                                            board.lock().unwrap().needs_pairing();
                                        }
                                        ServerMsg::SetupDone => {
                                            println!("paired — leaving setup");
                                            board.lock().unwrap().setup_done();
                                            prev_setup = false;
                                        }
                                        // Arms (or clears) the settings menu's
                                        // UPDATE row — display only; the signed
                                        // offer still runs the real OTA.
                                        ServerMsg::UpdateAvailable { version } => {
                                            board.lock().unwrap().set_update_available(version);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            Some(Ok(Message::Binary(b))) => {
                                // The payload is self-describing (`[codec][data]`,
                                // usually palette+RLE) and goes to board-core
                                // untouched — exactly what the firmware does.
                                // Decoding is board-core's, once per frame change,
                                // so the sim exercises the real device path.
                                if let Some((id, payload)) = decode_frame(&b) {
                                    board.lock().unwrap().on_frame(id, payload.to_vec());
                                }
                            }
                            Some(Ok(_)) => {}
                            _ => break, // closed or error
                        },
                        _ = poll.tick() => {
                            // Retired by a server switch: drop the connection
                            // promptly instead of on the next read.
                            if !alive.load(Ordering::Acquire) {
                                return;
                            }
                            // Keep the link state live while connected too, so
                            // flipping the Wi-Fi toggle changes which hop the
                            // connect screen blames on the very next frame.
                            let (wifi_fail, server_down) = {
                                let f = failures.lock().unwrap();
                                (f.wifi_fail, f.server_unreachable)
                            };
                            board.lock().unwrap().set_wifi(!wifi_fail);
                            // Drop the connection if the user toggled us offline.
                            if wifi_fail || server_down {
                                break;
                            }
                            // Detect entering setup/sync (incl. the menu's PAIR
                            // CODE screen); announce a pairing code.
                            let new_code = {
                                let mut b = board.lock().unwrap();
                                let in_setup = b.pairing_active();
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
