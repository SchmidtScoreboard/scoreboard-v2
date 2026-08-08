//! `app-core`: the shared, non-UI brain of the iOS/Android apps, exported to
//! Swift/Kotlin via uniffi. Owns the websocket to the server, authenticates as a
//! controller, subscribes to a board, fetches the catalog, and sends commands
//! (power, brightness, per-sport favorites). State arrives via `ClientListener`.

uniffi::setup_scaffolding!();

use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use scoreboard_protocol::{
    OffModeDescriptor,
    decode_frame, default_name_style as p_default_name_style,
    name_style_options as p_name_style_options, pubkey_hex, rgb565_to_888, sign_hex, BoardConfig,
    Catalog, ClientMsg, Command, FwChannel, FwImage, GameSummary, NameStyle, Role,
    ServerMsg, SpoilerMode, SportConfig as PSportConfig,
};
use std::collections::HashMap;
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
/// One off mode the server offers. Every string the row needs lives here.
#[derive(uniffi::Record, Clone)]
pub struct UiOffMode {
    pub id: String,
    pub label: String,
    pub description: String,
    /// Path of the preview PNG on the SAME server, or nil when the mode draws
    /// nothing (the app fills black locally rather than fetching a black PNG
    /// it cannot tell apart from a failed load).
    pub preview: Option<String>,
    pub input: Option<UiOffModeInput>,
    /// The mode's settings screen, in display order.
    pub fields: Vec<UiOffModeField>,
}

/// One control on a mode's settings screen.
#[derive(uniffi::Record, Clone)]
pub struct UiOffModeField {
    pub key: String,
    pub label: String,
    /// "text" | "toggle" | "choice" — a STRING, never an enum. A closed enum
    /// here would abort the whole config decode the first time a forked server
    /// named a kind this build has not heard of. Unknown kinds are SKIPPED.
    pub kind: String,
    pub placeholder: String,
    pub max_len: u32,
    /// What the field means when nothing is stored.
    pub default: String,
    pub options: Vec<UiOffModeChoice>,
    /// Show only while `visible_when_key` holds `visible_when_value`. Two flat
    /// Options rather than a nested record: uniffi records nest fine, but the
    /// Swift is markedly simpler and this is never more than one condition.
    pub visible_when_key: Option<String>,
    pub visible_when_value: Option<String>,
}

#[derive(uniffi::Record, Clone)]
pub struct UiOffModeChoice {
    pub value: String,
    pub label: String,
    pub preview: Option<String>,
}

#[derive(uniffi::Record, Clone)]
pub struct UiOffModeInput {
    pub key: String,
    pub label: String,
    pub placeholder: String,
    pub max_len: u32,
}

#[derive(uniffi::Record, Clone)]
pub struct UiSport {
    pub id: String,
    pub name: String,
    pub teams: Vec<UiTeam>,
    /// LEGACY single-select ladder, still carried for older builds.
    pub championship_options: Vec<UiChampOption>,
    /// The independently selectable rounds — what the app should render.
    pub championship_rounds: Vec<UiChampOption>,
    /// Sport family section this league is listed under ("Hockey"). A STRING,
    /// never an enum — see `UiBoardConfig::off_mode`. Empty from a server too
    /// old to declare one.
    pub category: String,
    /// Section sort rank, 1-based. Zero means unranked and sorts last.
    pub category_rank: u32,
}
/// How a league renders team names on the board (protocol `NameStyle`).
/// Location = city/college/country, Name = mascot/team name, Abbrev = the
/// abbreviation. Resolved per-board against the league default when unset.
#[derive(uniffi::Enum, Clone, Copy, PartialEq, Eq)]
pub enum UiNameStyle {
    Location,
    Name,
    Abbrev,
}
/// One entry in a league's "Names" picker: a `style` plus its league-specific
/// `label` ("College"/"City"/"Country", "Team name", "Abbreviation"). Comes
/// from `name_style_options(sport_id)` in menu order; empty for leaderboard
/// sports (golf/F1), which the app hides the picker for.
#[derive(uniffi::Record, Clone)]
pub struct UiNameStyleOption {
    pub style: UiNameStyle,
    pub label: String,
}
#[derive(uniffi::Record, Clone)]
pub struct UiSportConfig {
    pub sport_id: String,
    pub favorite_team_ids: Vec<String>,
    /// The rounds this league subscribes to. Each is exactly one thing; none
    /// implies another.
    pub championship_option_ids: Vec<String>,
    /// Per-league "time per event" override (seconds); None = board default.
    pub dwell_secs: Option<u32>,
    /// Leaderboard depth for golf/F1 (4 = static top-4 .. 20 = full field).
    pub leaderboard_rows: Option<u32>,
    /// Per-league team-name display style; None = the league default
    /// (`default_name_style`). Location = city/college/country, Name = mascot,
    /// Abbrev = abbreviation.
    pub name_style: Option<UiNameStyle>,
    /// Spoiler-protect every game in this league.
    pub spoiler: bool,
    /// Spoiler-protect games involving these teams.
    pub spoiler_team_ids: Vec<String>,
}
/// What the panel shows while off / idle.
/// Firmware release channel a board follows (protocol `FwChannel`). Channels
/// are cumulative toward Dev: a beta board may also be offered stable builds
/// when stable is newer — newest version wins.
#[derive(uniffi::Enum, Clone, Copy, PartialEq, Eq)]
pub enum UiFwChannel {
    Stable,
    Beta,
    Dev,
}
/// One firmware image the server currently hosts (protocol `FwImage`), as
/// listed for the DEBUG firmware picker. `channel` is a lowercase route name
/// ("stable"/"beta"/"dev"); `version` is CalVer ("2026.7.3").
#[derive(uniffi::Record, Clone)]
pub struct UiFwImage {
    pub channel: String,
    pub version: String,
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
    /// Id of the server-declared off mode (`UiOffMode::id`). A STRING, never an
    /// enum: a closed enum here would throw `unexpectedEnumCase` in the
    /// generated Swift and abort the whole config decode the first time a
    /// forked server named a mode this build had never heard of.
    pub off_mode: String,
    /// Values the user typed for the selected mode's declared input.
    pub off_mode_params: HashMap<String, String>,
    pub sleep_hours: u32,
    /// Seconds each game holds the panel before the rotation advances.
    pub dwell_secs: u32,
    /// Unix seconds when a button-sleep auto-wakes (None when not sleeping).
    pub wake_at: Option<u64>,
    /// Spoiler display: true = strict (matchup + time only), false = normal.
    pub strict_spoilers: bool,
    /// Spoiler-protect every game on this board.
    pub spoiler_all: bool,
    /// One-off spoiler-hidden games (cleared by Reset, pruned as games end).
    pub spoiler_game_ids: Vec<String>,
    /// One-off revealed games (exceptions that beat every protection rule).
    pub revealed_game_ids: Vec<String>,
    /// Board-local IANA timezone name ("America/New_York"); nil = server-local.
    pub timezone: Option<String>,
    /// Firmware release channel this board follows (`set_fw_channel`).
    pub fw_channel: UiFwChannel,
    /// Serve this board a synthetic slate instead of live games. Only the
    /// GAMES change — every other setting on the board is untouched.
    pub demo_data: bool,
    /// The nightly quiet window in minutes since the board's local midnight,
    /// or nil when there is none. Two flat fields rather than a record, so the
    /// "unset" case is one nil check in Swift.
    pub quiet_start_min: Option<u16>,
    pub quiet_end_min: Option<u16>,
    /// Let live games light the panel during the window.
    pub quiet_allow_events: bool,
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
    /// Normalized RFC3339 UTC start time ("2026-06-28T13:00:00Z"); nil when
    /// the feed's timestamp was absent or unparseable. Never stripped by
    /// spoiler protection — times are not spoilers.
    pub start_time: Option<String>,
    /// Normalized RFC3339 UTC end time; present mainly for multi-day events
    /// (golf tournaments, F1 race weekends). nil for ordinary games.
    pub end_time: Option<String>,
    /// Content hash of this game's rendered frame; look it up among the frames pushed
    /// via `on_frame` to draw the card.
    pub frame_id: String,
    /// True when the game qualifies for the board but lost its rotation slot to
    /// the cap — show it grayed with an offer to add it manually.
    pub bumped: bool,
    /// Championship tier label when this is a postseason/championship game
    /// ("Stanley Cup Final", "Playoffs"); nil for regular-season games.
    pub champ: Option<String>,
    /// True when spoiler protection sanitized this game (scores zeroed).
    pub spoiled: bool,
    /// True when this game's board rendering scrolls (deep golf/F1 leaderboard)
    /// — the card menu offers its scroll toggle only for these.
    pub scrolls: bool,
    /// ESPN game-page URL for the "View on ESPN" menu item; nil when none can
    /// be formed (demo games, golf/F1 leaderboards).
    pub espn_url: Option<String>,
    /// The board's preferences already claim this game, but it hasn't started
    /// — All Games marks it so you can see what's coming to the panel.
    pub queued: bool,
}

/// Implemented on the Swift/Kotlin side to receive pushed state.
#[uniffi::export(callback_interface)]
pub trait ClientListener: Send + Sync {
    fn on_connection(&self, connected: bool);
    fn on_board_state(&self, board_id: String, now_showing: Vec<UiGame>, online: bool);
    fn on_catalog(&self, sports: Vec<UiSport>);
    /// The off modes THIS server offers, in menu order. Delivered with the
    /// catalog. The app renders them generically — it must never branch on an
    /// id, or forks start needing app releases again.
    fn on_off_modes(&self, modes: Vec<UiOffMode>);
    /// True while the board is inside its quiet window right now. Derived by
    /// the server per push — the window is the setting, this is what it means.
    fn on_quiet_active(&self, board_id: String, active: bool);
    fn on_config(&self, board_id: String, config: UiBoardConfig);
    fn on_games(&self, games: Vec<UiGame>);
    /// A rendered frame arrived (pushed over the WS). `rgba` is 64×32 RGBA8 bytes.
    fn on_frame(&self, frame_id: String, rgba: Vec<u8>);
    fn on_paired(&self, board_id: String);
    fn on_pair_failed(&self, reason: String);
    /// A board's exported settings arrived (the reply to `export_settings`);
    /// `json` is the portable blob to share/copy.
    fn on_settings_export(&self, board_id: String, json: String);
    /// Outcome of `import_settings`: `None` = applied, `Some` = readable error.
    fn on_import_result(&self, board_id: String, error: Option<String>);
    /// Firmware versions, delivered with every board state push: `version` is
    /// what the device last reported on this connection (None while offline or
    /// pre-report); `update_available` is the server-hosted image's version,
    /// present ONLY when it is strictly newer than `version` — show an Update
    /// button exactly when this is Some.
    fn on_board_version(&self, board_id: String, version: Option<String>, update_available: Option<String>);
    /// The full firmware menu the server hosts (reply to `list_firmware`) —
    /// one image per channel with a current dist, across ALL channels
    /// regardless of any board's enrollment. Global to the server, not per
    /// board. Drives the DEBUG firmware picker.
    fn on_firmware_list(&self, images: Vec<UiFwImage>);
}

enum Out {
    Command(String, Command), // (board_id, command)
    GetCatalog,
    GetGames(Option<String>),
    Pair(String),
    /// Give up this controller's grant on a board (server-side revoke).
    Unpair(String),
    /// Re-send Subscribe with the CURRENT board list (after a removal).
    Resubscribe,
    /// Ask for a board's durable settings as a portable JSON blob.
    ExportSettings(String),
    /// Apply a previously exported blob to a board: (board_id, json).
    ImportSettings(String, String),
    /// Ask the server for the full firmware menu (DEBUG picker).
    ListFirmware,
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
                                            let modes = to_ui_off_modes(&cat.off_modes);
                                            if let Some(l) = listener.lock().unwrap().as_ref() {
                                                l.on_off_modes(modes);
                                                l.on_catalog(to_ui_sports(cat));
                                            }
                                        }
                                        ServerMsg::Games { games } => {
                                            if let Some(l) = listener.lock().unwrap().as_ref() { l.on_games(to_ui_games(games)); }
                                        }
                                        ServerMsg::BoardState { board_id: bid, online, now_showing, config, firmware_version, firmware_update, quiet_active } => {
                                            if let Some(l) = listener.lock().unwrap().as_ref() {
                                                l.on_board_state(bid.clone(), to_ui_games(now_showing), online);
                                                l.on_config(bid.clone(), to_ui_board_config(config));
                                                l.on_quiet_active(bid.clone(), quiet_active);
                                                l.on_board_version(bid, firmware_version, firmware_update);
                                            }
                                        }
                                        ServerMsg::SettingsExport { board_id: bid, json } => {
                                            if let Some(l) = listener.lock().unwrap().as_ref() { l.on_settings_export(bid, json); }
                                        }
                                        ServerMsg::ImportResult { board_id: bid, error } => {
                                            if let Some(l) = listener.lock().unwrap().as_ref() { l.on_import_result(bid, error); }
                                        }
                                        ServerMsg::FirmwareList { images } => {
                                            if let Some(l) = listener.lock().unwrap().as_ref() { l.on_firmware_list(to_ui_fw_images(images)); }
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
                            Some(Out::GetGames(board_id)) => {
                                let m = ClientMsg::GetGames { board_id };
                                let _ = write.send(Message::Text(serde_json::to_string(&m).unwrap())).await;
                            }
                            Some(Out::Pair(code)) => {
                                let m = ClientMsg::Pair { code };
                                let _ = write.send(Message::Text(serde_json::to_string(&m).unwrap())).await;
                            }
                            Some(Out::Unpair(bid)) => {
                                let m = ClientMsg::Unpair { board_id: bid };
                                let _ = write.send(Message::Text(serde_json::to_string(&m).unwrap())).await;
                            }
                            Some(Out::Resubscribe) => {
                                let sub = ClientMsg::Subscribe { board_ids: boards.lock().unwrap().clone() };
                                let _ = write.send(Message::Text(serde_json::to_string(&sub).unwrap())).await;
                            }
                            Some(Out::ExportSettings(bid)) => {
                                let m = ClientMsg::ExportSettings { board_id: bid };
                                let _ = write.send(Message::Text(serde_json::to_string(&m).unwrap())).await;
                            }
                            Some(Out::ImportSettings(bid, json)) => {
                                let m = ClientMsg::ImportSettings { board_id: bid, json };
                                let _ = write.send(Message::Text(serde_json::to_string(&m).unwrap())).await;
                            }
                            Some(Out::ListFirmware) => {
                                let _ = write.send(Message::Text(serde_json::to_string(&ClientMsg::ListFirmware).unwrap())).await;
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

    /// Forget a board: drop it from the subscription set and tell the server,
    /// so its pushes stop. Without this, "Remove this scoreboard" was app-UI
    /// only — the still-subscribed connection kept receiving board state and
    /// the next push resurrected the board.
    ///
    /// This forgets locally but keeps the server-side grant. To also hand back
    /// the pairing (so this phone can no longer command the board at all), use
    /// [`Self::unpair_board`].
    pub fn remove_board(&self, board_id: String) {
        self.boards.lock().unwrap().retain(|b| b != &board_id);
        let _ = self.tx.send(Out::Resubscribe);
    }

    /// Remove a board AND revoke this app's pairing on the server. The grant
    /// used to be permanent — re-adding the board later means pairing again
    /// with a fresh code, which is what "Remove this scoreboard" implies.
    /// Only this controller's own grant is revoked; other paired phones keep
    /// theirs.
    pub fn unpair_board(&self, board_id: String) {
        // Revoke before resubscribing, so the server has already dropped the
        // grant when the new (shorter) subscription set arrives.
        let _ = self.tx.send(Out::Unpair(board_id.clone()));
        self.remove_board(board_id);
    }

    /// Keep the board off with no auto-wake (cancels a running sleep timer).
    pub fn clear_wake_timer(&self, board_id: String) {
        let _ = self.tx.send(Out::Command(board_id, Command::ClearWakeTimer));
    }

    pub fn set_brightness(&self, board_id: String, level: u8) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetBrightness { level }));
    }

    /// Set a league's favourites and the rounds it subscribes to.
    ///
    /// `championship_option_ids` is the whole selection, sent as `Some` even
    /// when empty — the server reads absent as "an older app that never knew
    /// about rounds" and would expand the legacy field instead of deselecting.
    pub fn set_sport_config(
        &self,
        board_id: String,
        sport_id: String,
        favorite_team_ids: Vec<String>,
        championship_option_ids: Vec<String>,
    ) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetSportConfig {
            sport_id,
            favorite_team_ids,
            championship_option_id: None,
            championship_option_ids: Some(championship_option_ids),
        }));
    }

    /// Rename a board (persisted server-side, shown in the board switcher).
    pub fn rename(&self, board_id: String, name: String) {
        let _ = self.tx.send(Out::Command(board_id, Command::Rename { name }));
    }

    /// What the panel shows while off / idle (blank or a server-rendered clock).
    /// Pick a server-declared off mode. The id comes from `on_off_modes`; the
    /// server validates it and ignores one it does not offer.
    pub fn set_off_mode(&self, board_id: String, id: String) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetOffMode { id }));
    }

    /// Set the selected mode's input value. An empty string clears it.
    pub fn set_off_mode_param(&self, board_id: String, key: String, value: String) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetOffModeParam { key, value }));
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

    /// Per-league team-name display style (Location/Name/Abbrev); None reverts
    /// to the league default (`default_name_style`). Only meaningful for the
    /// styles a league offers via `name_style_options`.
    pub fn set_league_name_style(
        &self,
        board_id: String,
        sport_id: String,
        style: Option<UiNameStyle>,
    ) {
        let style = style.map(NameStyle::from);
        let _ = self
            .tx
            .send(Out::Command(board_id, Command::SetLeagueNameStyle { sport_id, style }));
    }

    pub fn set_dwell(&self, board_id: String, secs: u32) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetDwell { secs }));
    }

    /// Ask the server for all live games (to browse + add to a board).
    /// Browse all live/upcoming games. Pass the viewing board's id so its
    /// spoiler rules sanitize the summaries.
    pub fn request_games(&self, board_id: Option<String>) {
        let _ = self.tx.send(Out::GetGames(board_id));
    }

    /// How spoiler-protected games render: strict = matchup + time only.
    pub fn set_spoiler_mode(&self, board_id: String, strict: bool) {
        let mode = if strict { SpoilerMode::Strict } else { SpoilerMode::Normal };
        let _ = self.tx.send(Out::Command(board_id, Command::SetSpoilerMode { mode }));
    }

    /// Spoiler-protect every game on the board.
    pub fn set_spoiler_all(&self, board_id: String, on: bool) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetSpoilerAll { on }));
    }

    /// Set the nightly quiet window, in minutes since the board's local
    /// midnight. Equal values clear it.
    pub fn set_quiet_hours(&self, board_id: String, start_min: u16, end_min: u16) {
        let _ = self
            .tx
            .send(Out::Command(board_id, Command::SetQuietHours { start_min, end_min }));
    }

    /// Let live games light the panel during the quiet window.
    pub fn set_quiet_allow_events(&self, board_id: String, on: bool) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetQuietAllowEvents { on }));
    }

    pub fn clear_quiet_hours(&self, board_id: String) {
        let _ = self.tx.send(Out::Command(board_id, Command::ClearQuietHours));
    }

    /// Serve this board synthetic demo games instead of live ones. Persisted
    /// server-side and per-board, so it survives a restart and can be cleared
    /// from any paired phone even while the board is offline.
    pub fn set_demo_data(&self, board_id: String, on: bool) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetDemoData { on }));
    }

    /// Spoiler-protect a whole league.
    pub fn set_league_spoiler(&self, board_id: String, sport_id: String, on: bool) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetLeagueSpoiler { sport_id, on }));
    }

    /// Spoiler-protect one team's games.
    pub fn set_team_spoiler(&self, board_id: String, sport_id: String, team_id: String, on: bool) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetTeamSpoiler { sport_id, team_id, on }));
    }

    /// Per-game toggle: hidden=true protects; hidden=false reveals even when
    /// team/league/board rules protect it.
    pub fn set_game_spoiler(&self, board_id: String, game_id: String, hidden: bool) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetGameSpoiler { game_id, hidden }));
    }

    /// Board-local IANA timezone ("America/New_York") — drives the board's
    /// idle clock and start-time labels. Invalid names are ignored server-side.
    pub fn set_timezone(&self, board_id: String, tz: String) {
        let _ = self.tx.send(Out::Command(board_id, Command::SetTimezone { tz }));
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

    /// Ask for a board's durable settings (leagues, spoilers, timing) as a
    /// portable JSON blob; the reply arrives via `on_settings_export`.
    pub fn export_settings(&self, board_id: String) {
        let _ = self.tx.send(Out::ExportSettings(board_id));
    }

    /// Apply a previously exported settings blob to a board (possibly on a
    /// different server); the outcome arrives via `on_import_result`.
    pub fn import_settings(&self, board_id: String, json: String) {
        let _ = self.tx.send(Out::ImportSettings(board_id, json));
    }

    /// Tell a board to download, verify, and install the server-hosted
    /// firmware image (the one advertised via `on_board_version`'s
    /// `update_available`). The board goes offline while it flashes and
    /// reboots (~a minute); the fresh version report on reconnect is the
    /// confirmation.
    pub fn update_firmware(&self, board_id: String) {
        let _ = self.tx.send(Out::Command(board_id, Command::UpdateFirmware));
    }

    /// Which firmware release channel this board follows. Beta/dev boards may
    /// still be offered a stable build when it's the newest one available.
    pub fn set_fw_channel(&self, board_id: String, channel: UiFwChannel) {
        let channel = match channel {
            UiFwChannel::Stable => FwChannel::Stable,
            UiFwChannel::Beta => FwChannel::Beta,
            UiFwChannel::Dev => FwChannel::Dev,
        };
        let _ = self.tx.send(Out::Command(board_id, Command::SetFwChannel { channel }));
    }

    /// Ask the server for the full firmware menu (every channel with a hosted
    /// dist, regardless of board enrollment). The reply arrives via
    /// `on_firmware_list`. Backs the DEBUG firmware picker.
    pub fn list_firmware(&self) {
        let _ = self.tx.send(Out::ListFirmware);
    }

    /// Tell a board to install a SPECIFIC channel's current dist (the DEBUG
    /// picker / "reinstall current" flow). The on-device same-or-newer guard
    /// still governs whether it takes, so installing an older channel's dist is
    /// a no-op on the board.
    pub fn install_firmware(&self, board_id: String, channel: UiFwChannel) {
        let channel = match channel {
            UiFwChannel::Stable => FwChannel::Stable,
            UiFwChannel::Beta => FwChannel::Beta,
            UiFwChannel::Dev => FwChannel::Dev,
        };
        let _ = self.tx.send(Out::Command(board_id, Command::InstallFirmware { channel }));
    }
}

fn to_ui_off_modes(modes: &[OffModeDescriptor]) -> Vec<UiOffMode> {
    modes
        .iter()
        .map(|m| UiOffMode {
            id: m.id.clone(),
            label: m.label.clone(),
            description: m.description.clone(),
            preview: m.preview.clone(),
            input: m.input.as_ref().map(|i| UiOffModeInput {
                key: i.key.clone(),
                label: i.label.clone(),
                placeholder: i.placeholder.clone(),
                max_len: i.max_len,
            }),
            fields: m
                .fields
                .iter()
                .map(|f| UiOffModeField {
                    key: f.key.clone(),
                    label: f.label.clone(),
                    kind: f.kind.clone(),
                    placeholder: f.placeholder.clone(),
                    max_len: f.max_len,
                    default: f.default.clone(),
                    options: f
                        .options
                        .iter()
                        .map(|o| UiOffModeChoice {
                            value: o.value.clone(),
                            label: o.label.clone(),
                            preview: o.preview.clone(),
                        })
                        .collect(),
                    visible_when_key: f.visible_when.as_ref().map(|c| c.key.clone()),
                    visible_when_value: f.visible_when.as_ref().map(|c| c.equals.clone()),
                })
                .collect(),
        })
        .collect()
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
            championship_rounds: s
                .championship_rounds
                .into_iter()
                .map(|c| UiChampOption { id: c.id, label: c.label })
                .collect(),
            category: s.category,
            category_rank: s.category_rank,
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
        fn on_off_modes(&self, _: Vec<UiOffMode>) {}
        fn on_quiet_active(&self, _: String, _: bool) {}
        fn on_config(&self, _: String, _: UiBoardConfig) {}
        fn on_games(&self, games: Vec<UiGame>) {
            let _ = self.tx.lock().unwrap().send(format!("games={}", games.len()));
        }
        fn on_frame(&self, _: String, _: Vec<u8>) {}
        fn on_paired(&self, _: String) {}
        fn on_pair_failed(&self, _: String) {}
        fn on_settings_export(&self, _: String, _: String) {}
        fn on_import_result(&self, _: String, _: Option<String>) {}
        fn on_board_version(&self, _: String, _: Option<String>, _: Option<String>) {}
        fn on_firmware_list(&self, _: Vec<UiFwImage>) {}
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

    /// Exercises the exact controller path the iOS app uses: connect to the
    /// Railway dummy TCP proxy, then browse. `#[ignore]` (network) — run explicitly:
    /// `cargo test -p app-core -- --ignored --nocapture wss`
    #[test]
    #[ignore]
    fn connects_over_wss_and_browses() {
        let (tx, rx) = mpsc::channel();
        let url = std::env::var("SB_TEST_URL")
            .unwrap_or_else(|_| "wss://live-production-118c.up.railway.app/ws".into());
        let client = ScoreboardClient::new(url, vec![], None);
        client.set_listener(Box::new(Probe { tx: std::sync::Mutex::new(tx) }));
        client.connect();
        std::thread::sleep(std::time::Duration::from_secs(4));
        client.request_games(None);
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
            championship_option_ids: s.championship_option_ids,
            dwell_secs: s.dwell_secs,
            leaderboard_rows: s.leaderboard_rows,
            name_style: s.name_style.map(UiNameStyle::from),
            spoiler: s.spoiler,
            spoiler_team_ids: s.spoiler_team_ids,
        })
        .collect()
}

impl From<NameStyle> for UiNameStyle {
    fn from(s: NameStyle) -> Self {
        match s {
            NameStyle::Location => UiNameStyle::Location,
            NameStyle::Name => UiNameStyle::Name,
            NameStyle::Abbrev => UiNameStyle::Abbrev,
        }
    }
}
impl From<UiNameStyle> for NameStyle {
    fn from(s: UiNameStyle) -> Self {
        match s {
            UiNameStyle::Location => NameStyle::Location,
            UiNameStyle::Name => NameStyle::Name,
            UiNameStyle::Abbrev => NameStyle::Abbrev,
        }
    }
}

/// The name-style choices a league offers, in menu order, each with its
/// league-specific label. Wraps protocol `name_style_options`. Empty for
/// leaderboard sports (golf/F1) and unknown leagues — the app hides the
/// "Names" picker when this is empty.
#[uniffi::export]
pub fn name_style_options(sport_id: String) -> Vec<UiNameStyleOption> {
    p_name_style_options(&sport_id)
        .iter()
        .map(|(style, label)| UiNameStyleOption {
            style: UiNameStyle::from(*style),
            label: (*label).to_string(),
        })
        .collect()
}

/// The style a league falls back to when its `name_style` override is unset.
/// Wraps protocol `default_name_style` so the app can preselect it in the
/// "Names" picker. Location for college/World Cup, Name otherwise.
#[uniffi::export]
pub fn default_name_style(sport_id: String) -> UiNameStyle {
    UiNameStyle::from(p_default_name_style(&sport_id))
}

fn to_ui_board_config(c: BoardConfig) -> UiBoardConfig {
    // Resolved before the literal below moves fields out of `c`.
    let off_mode = c.off_mode_id().to_string();
    let off_mode_params: HashMap<String, String> =
        c.off_mode_params.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    UiBoardConfig {
        sports: to_ui_configs(c.sports),
        brightness: c.brightness,
        power: c.power,
        extra_game_ids: c.extra_game_ids,
        excluded_game_ids: c.excluded_game_ids,
        focused_game_id: c.focused_game_id,
        name: c.name,
        off_mode,
        off_mode_params,
        sleep_hours: c.sleep_hours,
        dwell_secs: c.dwell_secs,
        wake_at: c.wake_at,
        strict_spoilers: c.spoiler_mode == SpoilerMode::Strict,
        spoiler_all: c.spoiler_all,
        spoiler_game_ids: c.spoiler_game_ids,
        revealed_game_ids: c.revealed_game_ids,
        timezone: c.timezone,
        fw_channel: match c.fw_channel {
            FwChannel::Stable => UiFwChannel::Stable,
            FwChannel::Beta => UiFwChannel::Beta,
            FwChannel::Dev => UiFwChannel::Dev,
        },
        demo_data: c.demo_data,
        quiet_start_min: c.quiet_hours.map(|q| q.start_min),
        quiet_end_min: c.quiet_hours.map(|q| q.end_min),
        quiet_allow_events: c.quiet_allow_events,
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
            start_time: g.start_time,
            end_time: g.end_time,
            frame_id: g.frame_id,
            bumped: g.bumped,
            champ: g.champ,
            spoiled: g.spoiled,
            scrolls: g.scrolls,
            espn_url: g.espn_url,
            queued: g.queued,
        })
        .collect()
}

fn to_ui_fw_images(images: Vec<FwImage>) -> Vec<UiFwImage> {
    images
        .into_iter()
        .map(|i| UiFwImage { channel: i.channel, version: i.version })
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
