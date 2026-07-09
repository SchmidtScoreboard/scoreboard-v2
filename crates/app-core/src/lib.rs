//! `app-core`: the shared, non-UI brain of the iOS/Android apps, exported to
//! Swift/Kotlin via uniffi. Owns the websocket to the server, authenticates as a
//! controller, subscribes to a board, fetches the catalog, and sends commands
//! (power, brightness, per-sport favorites). State arrives via `ClientListener`.

uniffi::setup_scaffolding!();

use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use scoreboard_protocol::{
    decode_frame, pubkey_hex, rgb565_to_888, sign_hex, BoardConfig, Catalog, ClientMsg, Command,
    GameSummary, OffState, Role, ServerMsg, SportConfig as PSportConfig,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

// ---- uniffi-exported data the UI renders ----
#[derive(uniffi::Record, Clone)]
pub struct UiTeam {
    pub id: String,
    pub name: String,
    pub abbrev: String,
    /// Primary brand color as 6-digit hex (no `#`), or `None` when unknown.
    pub color: Option<String>,
    /// Secondary brand color as 6-digit hex, or `None` when unknown.
    pub alt_color: Option<String>,
}
#[derive(uniffi::Record, Clone)]
pub struct UiChampOption {
    pub id: String,
    pub label: String,
}
#[derive(uniffi::Record, Clone)]
pub struct UiSport {
    pub id: String,
    pub name: String,
    pub teams: Vec<UiTeam>,
    pub championship_options: Vec<UiChampOption>,
}
#[derive(uniffi::Record, Clone)]
pub struct UiSportConfig {
    pub sport_id: String,
    pub favorite_team_ids: Vec<String>,
    pub championship_option_id: Option<String>,
    /// Per-league "time per event" override (seconds); None = board default.
    pub dwell_secs: Option<u32>,
    /// Leaderboard depth for golf/F1 (4 = static top-4 .. 20 = full field).
    pub leaderboard_rows: Option<u32>,
}
/// What the panel shows while off / idle.
#[derive(uniffi::Enum, Clone, Copy, PartialEq, Eq)]
pub enum UiOffState {
    Blank,
    Clock,
}
/// One board's full server-side config, pushed with every BoardState.
#[derive(uniffi::Record, Clone)]
pub struct UiBoardConfig {
    pub sports: Vec<UiSportConfig>,
    pub brightness: u8,
    pub power: bool,
    pub extra_game_ids: Vec<String>,
    /// Preference-driven games one-off removed from the rotation.
    pub excluded_game_ids: Vec<String>,
    pub focused_game_id: Option<String>,
    pub name: Option<String>,
    pub off_state: UiOffState,
    pub sleep_hours: u32,
    /// Seconds each game holds the panel before the rotation advances.
    pub dwell_secs: u32,
    /// Unix seconds when a button-sleep auto-wakes (None when not sleeping).
    pub wake_at: Option<u64>,
}
#[derive(uniffi::Record, Clone)]
pub struct UiGame {
    pub id: String,
    pub sport_id: String,
    pub away: String,
    pub home: String,
    pub away_score: u32,
    pub home_score: u32,
    pub status: String,
    pub detail: String,
    /// Content hash of this game's rendered frame; look it up among the frames pushed
    /// via `on_frame` to draw the card.
    pub frame_id: String,
    /// True when the game qualifies for the board but lost its rotation slot to
    /// the cap — show it grayed with an offer to add it manually.
    pub bumped: bool,
    /// Championship tier label when this is a postseason/championship game
    /// ("Stanley Cup Final", "Playoffs"); nil for regular-season games.
    pub champ: Option<String>,
}

/// Implemented on the Swift/Kotlin side to receive pushed state.
#[uniffi::export(callback_interface)]
pub trait ClientListener: Send + Sync {
    fn on_connection(&self, connected: bool);
    fn on_board_state(&self, board_id: String, now_showing: Vec<UiGame>, online: bool);
    fn on_catalog(&self, sports: Vec<UiSport>);
    fn on_config(&self, board_id: String, config: UiBoardConfig);
    fn on_games(&self, games: Vec<UiGame>);
    /// A rendered frame arrived (pushed over the WS). `rgba` is 64×32 RGBA8 bytes.
    fn on_frame(&self, frame_id: String, rgba: Vec<u8>);
    fn on_paired(&self, board_id: String);
    fn on_pair_failed(&self, reason: String);
}

enum Out {
    Command(String, Command), // (board_id, command)
    GetCatalog,
    GetGames,
    Pair(String),
}

#[derive(uniffi::Object)]
pub struct ScoreboardClient {
    server_url: String,
    /// Every board this controller is paired with (it can watch/control several).
    boards: Arc<Mutex<Vec<String>>>,
    secret: [u8; 32],
    rt: Runtime,
    listener: Arc<Mutex<Option<Box<dyn ClientListener>>>>,
    tx: mpsc::UnboundedSender<Out>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<Out>>>,
}

#[uniffi::export]
impl ScoreboardClient {
    /// `secret_key_hex` is this install's persisted 32-byte Ed25519 seed (hex).
    /// Pass `None` on first launch to mint a fresh identity; read it back out via
    /// `secret_key_hex()` and store it (Keychain) so the identity survives.
    #[uniffi::constructor]
    pub fn new(server_url: String, board_ids: Vec<String>, secret_key_hex: Option<String>) -> Arc<Self> {
        let secret = secret_key_hex
            .and_then(|h| hex::decode(h).ok())
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
            .unwrap_or_else(|| {
                let mut s = [0u8; 32];
                rand::rngs::OsRng.fill_bytes(&mut s);
                s
            });
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (tx, rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            server_url,
            boards: Arc::new(Mutex::new(board_ids)),
            secret,
            rt,
            listener: Arc::new(Mutex::new(None)),
            tx,
            rx: Mutex::new(Some(rx)),
        })
    }

    pub fn set_listener(&self, listener: Box<dyn ClientListener>) {
        *self.listener.lock().unwrap() = Some(listener);
    }

    pub fn connect(&self) {
        let Some(mut rx) = self.rx.lock().unwrap().take() else {
            return; // already connected
        };
        let url = self.server_url.clone();
        let boards = self.boards.clone();
        let listener = self.listener.clone();
        let secret = self.secret;

        self.rt.spawn(async move {
            let notify_conn = |connected: bool| {
                if let Some(l) = listener.lock().unwrap().as_ref() {
                    l.on_connection(connected);
                }
            };
            // Exponential backoff with jitter: retries keep the app self-healing
            // (server restart, board power cycle, app launched offline) without
            // stampeding the server — 1s, 2s, 4s … capped at 30s, plus 0–500ms of
            // random jitter so a fleet of apps doesn't reconnect in lockstep.
            let mut delay = Duration::from_secs(1);
            macro_rules! backoff {
                () => {{
                    let jitter = Duration::from_millis(rand::Rng::gen_range(&mut rand::rngs::OsRng, 0..500));
                    tokio::time::sleep(delay + jitter).await;
                    delay = (delay * 2).min(Duration::from_secs(30));
                }};
            }

            'reconnect: loop {
                // Bound each attempt so an unroutable host can't stall a cycle.
                let ws = match tokio::time::timeout(Duration::from_secs(10), connect_async(&url)).await {
                    Ok(Ok((ws, _))) => ws,
                    _ => {
                        notify_conn(false);
                        backoff!();
                        continue 'reconnect;
                    }
                };
                let (mut write, mut read) = ws.split();

                // Wait for Hello { challenge }.
                let challenge = loop {
                    match read.next().await {
                        Some(Ok(Message::Text(t))) => {
                            if let Ok(ServerMsg::Hello { challenge }) =
                                serde_json::from_str::<ServerMsg>(&t)
                            {
                                break challenge;
                            }
                        }
                        Some(Ok(_)) => {} // ignore non-text frames pre-Hello
                        _ => {
                            notify_conn(false);
                            backoff!();
                            continue 'reconnect;
                        }
                    }
                };

                // Authenticate as a controller: sign the challenge with our Ed25519
                // secret. Our pubkey IS our identity to the server.
                let auth = ClientMsg::Auth {
                    role: Role::Controller,
                    pubkey: pubkey_hex(&secret),
                    sig: sign_hex(&secret, challenge.as_bytes()),
                };
                let _ = write.send(Message::Text(serde_json::to_string(&auth).unwrap())).await;
                // Re-subscribe to every board we've paired with (empty until first
                // pair), and re-pull the catalog — it may have changed across a
                // server restart, and a launch-while-offline never got one at all.
                {
                    let bs = boards.lock().unwrap().clone();
                    if !bs.is_empty() {
                        let sub = ClientMsg::Subscribe { board_ids: bs };
                        let _ = write.send(Message::Text(serde_json::to_string(&sub).unwrap())).await;
                    }
                }
                let _ = write
                    .send(Message::Text(serde_json::to_string(&ClientMsg::GetCatalog).unwrap()))
                    .await;
                notify_conn(true);
                delay = Duration::from_secs(1); // healthy connection resets the backoff

                // Periodic pings surface silently-dead sockets (Wi-Fi hops, sleep)
                // instead of hanging on a read forever.
                let mut ping = tokio::time::interval(Duration::from_secs(30));
                ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                ping.reset(); // don't fire immediately

                let mut req_id: u64 = 0;
                loop {
                    tokio::select! {
                    _ = ping.tick() => {
                        if write.send(Message::Ping(Vec::new())).await.is_err() {
                            notify_conn(false);
                            backoff!();
                            continue 'reconnect;
                        }
                    }
                    incoming = read.next() => {
                        match incoming {
                            Some(Ok(Message::Text(t))) => {
                                if let Ok(sm) = serde_json::from_str::<ServerMsg>(&t) {
                                    match sm {
                                        ServerMsg::Paired { board_id: bid } => {
                                            // Add to our set (dedup) and re-subscribe to all boards.
                                            let all = {
                                                let mut bs = boards.lock().unwrap();
                                                if !bs.contains(&bid) { bs.push(bid.clone()); }
                                                bs.clone()
                                            };
                                            let sub = ClientMsg::Subscribe { board_ids: all };
                                            let _ = write.send(Message::Text(serde_json::to_string(&sub).unwrap())).await;
                                            if let Some(l) = listener.lock().unwrap().as_ref() { l.on_paired(bid); }
                                        }
                                        ServerMsg::PairFailed { reason } => {
                                            if let Some(l) = listener.lock().unwrap().as_ref() { l.on_pair_failed(reason); }
                                        }
                                        ServerMsg::Catalog(cat) => {
                                            if let Some(l) = listener.lock().unwrap().as_ref() { l.on_catalog(to_ui_sports(cat)); }
                                        }
                                        ServerMsg::Games { games } => {
                                            if let Some(l) = listener.lock().unwrap().as_ref() { l.on_games(to_ui_games(games)); }
                                        }
                                        ServerMsg::BoardState { board_id: bid, online, now_showing, config } => {
                                            if let Some(l) = listener.lock().unwrap().as_ref() {
                                                l.on_board_state(bid.clone(), to_ui_games(now_showing), online);
                                                l.on_config(bid, to_ui_board_config(config));
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            Some(Ok(Message::Binary(b))) => {
                                if let Some((fid, px)) = decode_frame(&b) {
                                    let rgba = rgb565_to_rgba(px);
                                    if let Some(l) = listener.lock().unwrap().as_ref() {
                                        l.on_frame(fid, rgba);
                                    }
                                }
                            }
                            Some(Ok(_)) => {}
                            _ => {
                                // Connection lost: report it and retry with backoff.
                                notify_conn(false);
                                backoff!();
                                continue 'reconnect;
                            }
                        }
                    }
                    out = rx.recv() => {
                        match out {
                            Some(Out::Command(bid, c)) => {
                                req_id += 1;
                                let m = ClientMsg::Command { req_id, board_id: bid, cmd: c };
                                let _ = write.send(Message::Text(serde_json::to_string(&m).unwrap())).await;
                            }
                            Some(Out::GetCatalog) => {
                                let _ = write.send(Message::Text(serde_json::to_string(&ClientMsg::GetCatalog).unwrap())).await;
                            }
                            Some(Out::GetGames) => {
                                let _ = write.send(Message::Text(serde_json::to_string(&ClientMsg::GetGames).unwrap())).await;
                            }
                            Some(Out::Pair(code)) => {
                                let m = ClientMsg::Pair { code };
                                let _ = write.send(Message::Text(serde_json::to_string(&m).unwrap())).await;
                            }
                            // The client object was dropped — end the task for good.
                            None => break 'reconnect,
                        }
                    }
                }
                }
            }
        });
    }

    pub fn request_catalog(&self) {
        let _ = self.tx.send(Out::GetCatalog);
    }

    /// Pair with whichever board is currently showing this code.
    pub fn pair(&self, code: String) {
        let _ = self.tx.send(Out::Pair(code));
    }

    /// Every board this controller is paired with. Persist these to re-subscribe on
    /// the next launch.
    pub fn board_ids(&self) -> Vec<String> {
        self.boards.lock().unwrap().clone()
    }

    /// This controller's Ed25519 public key (hex) — its identity to the server.
    pub fn pubkey(&self) -> String {
        pubkey_hex(&self.secret)
    }

    /// The 32-byte secret seed (hex). Persist in the Keychain and pass back to
    /// `new` next launch so this identity (and its pairings) survive.
    pub fn secret_key_hex(&self) -> String {
        hex::encode(self.secret)
    }

    pub fn set_power(&self, board_id: String, on: bool) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetPower { on }));
    }

    /// Keep the board off with no auto-wake (cancels a running sleep timer).
    pub fn clear_wake_timer(&self, board_id: String) {
        let _ = self.tx.send(Out::Command(board_id, Command::ClearWakeTimer));
    }

    pub fn set_brightness(&self, board_id: String, level: u8) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetBrightness { level }));
    }

    pub fn set_sport_config(
        &self,
        board_id: String,
        sport_id: String,
        favorite_team_ids: Vec<String>,
        championship_option_id: Option<String>,
    ) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetSportConfig {
            sport_id,
            favorite_team_ids,
            championship_option_id,
        }));
    }

    /// Rename a board (persisted server-side, shown in the board switcher).
    pub fn rename(&self, board_id: String, name: String) {
        let _ = self.tx.send(Out::Command(board_id, Command::Rename { name }));
    }

    /// What the panel shows while off / idle (blank or a server-rendered clock).
    pub fn set_off_state(&self, board_id: String, off_state: UiOffState) {
        let off = match off_state {
            UiOffState::Blank => OffState::Blank,
            UiOffState::Clock => OffState::Clock,
        };
        let _ = self.tx.send(Out::Command(board_id, Command::SetOffState { off_state: off }));
    }

    /// How long a knob-press sleep lasts before the board wakes itself.
    pub fn set_sleep_hours(&self, board_id: String, hours: u32) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetSleepHours { hours }));
    }

    /// Seconds each game holds the panel before the rotation advances.
    /// Per-league "time per event" override; None reverts to the board default.
    pub fn set_league_dwell(&self, board_id: String, sport_id: String, secs: Option<u32>) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetLeagueDwell { sport_id, secs }));
    }

    /// Leaderboard depth for golf/F1; None reverts to the default (top 10).
    pub fn set_league_rows(&self, board_id: String, sport_id: String, rows: Option<u32>) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetLeagueRows { sport_id, rows }));
    }

    pub fn set_dwell(&self, board_id: String, secs: u32) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetDwell { secs }));
    }

    /// Ask the server for all live games (to browse + add to a board).
    pub fn request_games(&self) {
        let _ = self.tx.send(Out::GetGames);
    }

    /// Pin a board to a single game (overrides the rotation).
    pub fn focus_game(&self, board_id: String, game_id: String) {
        let _ = self.tx.send(Out::Command(board_id, Command::FocusGame { game_id }));
    }

    /// Clear a focus, returning to the normal rotation.
    pub fn clear_focus(&self, board_id: String) {
        let _ = self.tx.send(Out::Command(board_id, Command::ClearFocus));
    }

    /// Add a game to a board's rotation (beyond favorites).
    pub fn add_game(&self, board_id: String, game_id: String) {
        let _ = self.tx.send(Out::Command(board_id, Command::AddGame { game_id }));
    }

    /// Remove a game from a board's rotation (un-adds an extra, or one-off
    /// excludes a preference-driven game without touching the preference).
    pub fn remove_game(&self, board_id: String, game_id: String) {
        let _ = self.tx.send(Out::Command(board_id, Command::RemoveGame { game_id }));
    }

    /// Back to the preference-driven rotation: clears the pin, one-off additions,
    /// and one-off exclusions in a single step.
    pub fn reset_rotation(&self, board_id: String) {
        let _ = self.tx.send(Out::Command(board_id, Command::ResetRotation));
    }
}

fn to_ui_sports(cat: Catalog) -> Vec<UiSport> {
    cat.sports
        .into_iter()
        .map(|s| UiSport {
            id: s.id,
            name: s.name,
            teams: s
                .teams
                .into_iter()
                .map(|t| UiTeam {
                    id: t.id,
                    name: t.name,
                    abbrev: t.abbrev,
                    color: t.color,
                    alt_color: t.alt_color,
                })
                .collect(),
            championship_options: s
                .championship_options
                .into_iter()
                .map(|c| UiChampOption { id: c.id, label: c.label })
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod wss_tests {
    use super::*;
    use std::sync::mpsc;

    struct Probe {
        tx: std::sync::Mutex<mpsc::Sender<String>>,
    }
    impl ClientListener for Probe {
        fn on_connection(&self, connected: bool) {
            let _ = self.tx.lock().unwrap().send(format!("conn={connected}"));
        }
        fn on_board_state(&self, _: String, _: Vec<UiGame>, _: bool) {}
        fn on_catalog(&self, _: Vec<UiSport>) {}
        fn on_config(&self, _: String, _: UiBoardConfig) {}
        fn on_games(&self, games: Vec<UiGame>) {
            let _ = self.tx.lock().unwrap().send(format!("games={}", games.len()));
        }
        fn on_frame(&self, _: String, _: Vec<u8>) {}
        fn on_paired(&self, _: String) {}
        fn on_pair_failed(&self, _: String) {}
    }

    /// The order-of-operations regression: the app launches while the server is
    /// DOWN, the server comes up later, and the client must connect on its own
    /// (backoff retry) — the old code gave up after one failed attempt.
    /// Loopback-only; no network needed.
    #[test]
    fn reconnects_after_server_comes_up_late() {
        // Reserve a port, then free it so the client's first attempts fail.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };
        let (tx, rx) = mpsc::channel();
        let client = ScoreboardClient::new(format!("ws://127.0.0.1:{port}/ws"), vec![], None);
        client.set_listener(Box::new(Probe { tx: std::sync::Mutex::new(tx) }));
        client.connect();

        // Let at least one connection attempt fail before the server exists.
        std::thread::sleep(Duration::from_millis(1500));

        // NOW bring up a minimal mock server that speaks the Hello handshake.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.spawn(async move {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else { return };
                    let hello = serde_json::to_string(&ServerMsg::Hello { challenge: "c".into() }).unwrap();
                    let _ = ws.send(Message::Text(hello)).await;
                    while let Some(Ok(_)) = ws.next().await {} // drain until close
                });
            }
        });

        // The client should reconnect within a few backoff cycles (1s+2s+4s…).
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut connected = false;
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(m) if m == "conn=true" => {
                    connected = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(connected, "client must retry and connect once the server appears");
    }

    /// Exercises the exact controller path the iOS app uses: connect over wss to the
    /// live Railway dummy, then browse. `#[ignore]` (network) — run explicitly:
    /// `cargo test -p app-core -- --ignored --nocapture wss`
    #[test]
    #[ignore]
    fn connects_over_wss_and_browses() {
        let (tx, rx) = mpsc::channel();
        let url = std::env::var("SB_TEST_URL")
            .unwrap_or_else(|_| "wss://dummy-production-2e6f.up.railway.app/ws".into());
        let client = ScoreboardClient::new(url, vec![], None);
        client.set_listener(Box::new(Probe { tx: std::sync::Mutex::new(tx) }));
        client.connect();
        std::thread::sleep(std::time::Duration::from_secs(4));
        client.request_games();
        std::thread::sleep(std::time::Duration::from_secs(4));
        let msgs: Vec<String> = rx.try_iter().collect();
        println!("listener messages: {msgs:?}");
        assert!(msgs.iter().any(|m| m == "conn=true"), "expected to connect over wss");
        assert!(
            msgs.iter().any(|m| m.starts_with("games=") && m != "games=0"),
            "expected to receive live games"
        );
    }
}

fn to_ui_configs(sports: Vec<PSportConfig>) -> Vec<UiSportConfig> {
    sports
        .into_iter()
        .map(|s| UiSportConfig {
            sport_id: s.sport_id,
            favorite_team_ids: s.favorite_team_ids,
            championship_option_id: s.championship_option_id,
            dwell_secs: s.dwell_secs,
            leaderboard_rows: s.leaderboard_rows,
        })
        .collect()
}

fn to_ui_board_config(c: BoardConfig) -> UiBoardConfig {
    UiBoardConfig {
        sports: to_ui_configs(c.sports),
        brightness: c.brightness,
        power: c.power,
        extra_game_ids: c.extra_game_ids,
        excluded_game_ids: c.excluded_game_ids,
        focused_game_id: c.focused_game_id,
        name: c.name,
        off_state: match c.off_state {
            OffState::Blank => UiOffState::Blank,
            OffState::Clock => UiOffState::Clock,
        },
        sleep_hours: c.sleep_hours,
        dwell_secs: c.dwell_secs,
        wake_at: c.wake_at,
    }
}

fn to_ui_games(games: Vec<GameSummary>) -> Vec<UiGame> {
    games
        .into_iter()
        .map(|g| UiGame {
            id: g.id,
            sport_id: g.sport_id,
            away: g.away,
            home: g.home,
            away_score: g.away_score,
            home_score: g.home_score,
            status: g.status,
            detail: g.detail,
            frame_id: g.frame_id,
            bumped: g.bumped,
            champ: g.champ,
        })
        .collect()
}

/// Convert a 64×32 RGB565-LE frame to RGBA8 bytes for the UI.
fn rgb565_to_rgba(rgb565: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgb565.len() / 2 * 4);
    for px in rgb565.chunks_exact(2) {
        let c = u16::from_le_bytes([px[0], px[1]]);
        let (r, g, b) = rgb565_to_888(c);
        out.extend_from_slice(&[r, g, b, 255]);
    }
    out
}
