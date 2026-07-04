//! Local scoreboard server. Pulls live games from ESPN, stores per-board
//! preferences (favorite teams etc.), and renders a per-board rotation filtered
//! by those preferences. Talks to boards + controllers over a websocket.

mod bdf;
mod espn;
mod model;
mod render;
mod teams;
mod ws;

use axum::{extract::State, routing::get, Json, Router};
use espn::SportsDataSource;
use model::{Game, GameStatus};
use scoreboard_protocol::*;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use rusqlite::Connection;
use tokio::sync::mpsc::UnboundedSender;

use axum::extract::ws::Message;

/// Token-bucket rate limiter — throttles pairing-code guesses so the 6-digit code
/// can't be brute-forced (a connected client could otherwise try codes for free).
struct RateLimiter {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl RateLimiter {
    fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self { tokens: capacity, capacity, refill_per_sec, last: Instant::now() }
    }
    /// Consume a token; returns false when the bucket is empty (reject the attempt).
    fn allow(&mut self) -> bool {
        let now = Instant::now();
        let gained = now.duration_since(self.last).as_secs_f64() * self.refill_per_sec;
        self.tokens = (self.tokens + gained).min(self.capacity);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub catalog: Arc<Catalog>,
    pub games: Arc<Mutex<Vec<Game>>>,
    pub frames: Arc<Mutex<HashMap<FrameId, Vec<u8>>>>,
    pub configs: Arc<Mutex<HashMap<String, BoardConfig>>>, // board_id -> prefs
    pub devices: Arc<Mutex<HashMap<String, UnboundedSender<Message>>>>, // board_id -> sink
    pub controllers: Arc<Mutex<HashMap<String, Vec<UnboundedSender<Message>>>>>, // board_id -> app sinks
    pub pending: Arc<Mutex<HashMap<String, String>>>, // pairing code -> board_id
    pub allow: Arc<Mutex<HashMap<String, HashSet<String>>>>, // board_id -> controller ids
    db: Option<Arc<Mutex<Connection>>>,
    pair_limiter: Arc<Mutex<RateLimiter>>,
    /// Wakes the poll loop when a client connects, so it fetches fresh games promptly
    /// instead of up to POLL_SECS later (the idle-skip otherwise leaves games stale).
    poll_wake: Arc<tokio::sync::Notify>,
    /// Transient full-screen notices per board ("SLEEPING…", "NO GAMES…"). Memory
    /// only — they're seconds-to-minutes long and don't need to survive a restart.
    pub notices: Arc<Mutex<HashMap<String, Notice>>>,
    /// Last manifest JSON sent to each device; identical re-pushes are skipped so a
    /// refresh doesn't needlessly reset the board's rotation position.
    last_manifest: Arc<Mutex<HashMap<String, String>>>,
}

/// A transient full-screen message for one board. Stored as text so each manifest
/// render can redraw it with a live progress bar (depleting toward `until`).
#[derive(Clone)]
pub struct Notice {
    pub lines: Vec<(String, bool, embedded_graphics::pixelcolor::Rgb888)>,
    pub created: std::time::Instant,
    pub until: std::time::Instant,
}

impl AppState {
    #[cfg(test)]
    fn new_ephemeral() -> Self {
        Self::new_inner(None, mock_catalog())
    }

    fn new_with_catalog(catalog: Catalog) -> Self {
        // On Railway, STATE_DIR points at a mounted volume so pairings/configs
        // survive redeploys; locally it's unset and we use the working directory.
        let dir = std::env::var("STATE_DIR").unwrap_or_else(|_| ".".into());
        let path = PathBuf::from(dir).join("scoreboard.db");
        let db = match open_db(&path) {
            Ok(conn) => Some(conn),
            Err(e) => {
                tracing::error!("state DB {} unavailable ({e}); running without persistence", path.display());
                None
            }
        };
        Self::new_inner(db, catalog)
    }

    fn new_inner(db: Option<Connection>, catalog: Catalog) -> Self {
        let (configs, allow) = match &db {
            Some(conn) => (load_configs(conn), load_pairings(conn)),
            None => (HashMap::new(), HashMap::new()),
        };
        AppState {
            catalog: Arc::new(catalog),
            games: Arc::new(Mutex::new(espn::mock_games())),
            frames: Arc::new(Mutex::new(HashMap::new())),
            configs: Arc::new(Mutex::new(configs)),
            devices: Arc::new(Mutex::new(HashMap::new())),
            controllers: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            allow: Arc::new(Mutex::new(allow)),
            db: db.map(|c| Arc::new(Mutex::new(c))),
            pair_limiter: Arc::new(Mutex::new(RateLimiter::new(10.0, 1.0))),
            poll_wake: Arc::new(tokio::sync::Notify::new()),
            notices: Arc::new(Mutex::new(HashMap::new())),
            last_manifest: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Drop the manifest-dedup entry for a board (used when it reconnects, so the
    /// fresh connection always receives an initial manifest).
    pub fn forget_last_manifest(&self, board_id: &str) {
        self.last_manifest.lock().unwrap().remove(board_id);
    }

    pub fn config_for(&self, board_id: &str) -> BoardConfig {
        self.configs
            .lock()
            .unwrap()
            .get(board_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Is this controller authorized to view/command this board?
    pub fn is_allowed(&self, board_id: &str, controller: &str) -> bool {
        self.allow
            .lock()
            .unwrap()
            .get(board_id)
            .is_some_and(|s| s.contains(controller))
    }

    /// Upsert one board's config (after a controller changes it). Best-effort —
    /// SQLite gives the atomicity + durability the JSON file couldn't.
    pub fn persist_config(&self, board_id: &str, cfg: &BoardConfig) {
        let Some(db) = &self.db else { return };
        let Ok(json) = serde_json::to_string(cfg) else { return };
        let _ = db.lock().unwrap().execute(
            "INSERT INTO boards (pubkey, config) VALUES (?1, ?2)
             ON CONFLICT(pubkey) DO UPDATE SET config = excluded.config",
            rusqlite::params![board_id, json],
        );
    }

    /// Record a new pairing (controller authorized for a board).
    pub fn persist_pairing(&self, board_id: &str, controller: &str) {
        let Some(db) = &self.db else { return };
        let _ = db.lock().unwrap().execute(
            "INSERT OR IGNORE INTO pairings (board_pubkey, controller_pubkey) VALUES (?1, ?2)",
            rusqlite::params![board_id, controller],
        );
    }
}

/// Open (creating if needed) the SQLite store and ensure the schema exists.
fn open_db(path: &std::path::Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS boards (
            pubkey TEXT PRIMARY KEY,
            config TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS pairings (
            board_pubkey      TEXT NOT NULL,
            controller_pubkey TEXT NOT NULL,
            PRIMARY KEY (board_pubkey, controller_pubkey)
         );",
    )?;
    Ok(conn)
}

/// Load every persisted board config (board pubkey -> config).
fn load_configs(conn: &Connection) -> HashMap<String, BoardConfig> {
    let mut out = HashMap::new();
    let Ok(mut stmt) = conn.prepare("SELECT pubkey, config FROM boards") else { return out };
    let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) else {
        return out;
    };
    for (pubkey, json) in rows.flatten() {
        if let Ok(cfg) = serde_json::from_str::<BoardConfig>(&json) {
            out.insert(pubkey, cfg);
        }
    }
    out
}

/// Load the pairing allow-list (board pubkey -> set of controller pubkeys).
fn load_pairings(conn: &Connection) -> HashMap<String, HashSet<String>> {
    let mut out: HashMap<String, HashSet<String>> = HashMap::new();
    let Ok(mut stmt) = conn.prepare("SELECT board_pubkey, controller_pubkey FROM pairings") else {
        return out;
    };
    let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) else {
        return out;
    };
    for (board, controller) in rows.flatten() {
        out.entry(board).or_default().insert(controller);
    }
    out
}

/// Resolve a pairing code to its board and authorize the controller.
/// The code is single-use (consumed on success). Returns the board_id or a reason.
pub fn try_pair(state: &AppState, code: &str, controller: &str) -> Result<String, String> {
    let board_id = state
        .pending
        .lock()
        .unwrap()
        .remove(code)
        .ok_or_else(|| "invalid or expired code".to_string())?;
    state
        .allow
        .lock()
        .unwrap()
        .entry(board_id.clone())
        .or_default()
        .insert(controller.to_string());
    state.persist_pairing(&board_id, controller);
    Ok(board_id)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    if std::env::args().any(|a| a == "--preview") {
        preview();
        return;
    }

    // `--demo` flag or DEMO env (the dummy Railway service just sets DEMO=1).
    let demo = std::env::args().any(|a| a == "--demo") || std::env::var("DEMO").is_ok();
    let source: Arc<dyn SportsDataSource> = if demo {
        tracing::info!("DEMO MODE: serving synthetic live games");
        Arc::new(espn::DemoSource::new())
    } else {
        Arc::new(espn::EspnSource::new())
    };
    // Team rosters come from the data source's /teams endpoint — the FULL league
    // roster, independent of which games are live. In live mode we cache the last
    // good catalog to disk, so an ESPN outage at startup still serves the full list
    // instead of the tiny mock. Demo mode keeps its synthetic catalog and never
    // touches the cache.
    let catalog = match source.fetch_catalog().await {
        Some(c) => {
            tracing::info!("catalog: {} sports from source", c.sports.len());
            if !demo {
                cache_catalog(&c);
            }
            c
        }
        None if !demo => load_cached_catalog().unwrap_or_else(|| {
            tracing::warn!("catalog: source unavailable and no cache yet, using mock");
            mock_catalog()
        }),
        None => {
            tracing::warn!("catalog: team fetch unavailable, using mock");
            mock_catalog()
        }
    };
    let state = AppState::new_with_catalog(catalog);
    tokio::spawn(poll_loop(state.clone(), source));
    tokio::spawn(tick_loop(state.clone()));

    let app = Router::new()
        .route("/catalog", get(get_catalog))
        .route("/schedule", get(get_schedule))
        .route("/render/:id", get(get_render))
        .route("/ws", get(ws::ws_handler))
        .with_state(state);

    // Bind all interfaces so a real board on the LAN (or Railway's proxy) can reach
    // us; includes localhost, so the simulator/app on 127.0.0.1 still work. Railway
    // injects $PORT; locally we default to 7878.
    let port = std::env::var("PORT").unwrap_or_else(|_| "7878".into());
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    tracing::info!("scoreboard server on {addr}");
    axum::serve(listener, app).await.unwrap();
}

async fn get_catalog(State(s): State<AppState>) -> Json<Catalog> {
    Json((*s.catalog).clone())
}

/// Render a game to a PNG — the app's "actual rendered content" cards fetch this.
/// `id` may carry a trailing `.png`. 64x32 source; the app scales it up crisply.
async fn get_render(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(s): State<AppState>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let id = id.strip_suffix(".png").unwrap_or(&id);
    let px = {
        let games = s.games.lock().unwrap();
        games.iter().find(|g| g.id == id).map(|g| render::render_game(g).1)
    };
    match px {
        Some(rgb565) => (
            [(axum::http::header::CONTENT_TYPE, "image/png")],
            render_png(&rgb565),
        )
            .into_response(),
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

async fn get_schedule(State(s): State<AppState>) -> Json<Schedule> {
    let games = s.games.lock().unwrap();
    let mut live = Vec::new();
    let mut upcoming = Vec::new();
    for g in games.iter() {
        let summary = to_summary(g);
        match g.status {
            GameStatus::Active | GameStatus::Intermission => live.push(summary),
            _ => upcoming.push(summary),
        }
    }
    Json(Schedule { live, upcoming })
}

pub(crate) fn to_summary(g: &Game) -> GameSummary {
    // Leaderboard sports (golf/F1): no away/home — show the event + leader.
    if let Some(lb) = &g.leaderboard {
        let detail = lb
            .rows
            .first()
            .map(|r| format!("{} {}", r.name, r.value))
            .unwrap_or_default();
        return GameSummary {
            id: g.id.clone(),
            sport_id: g.sport_id.clone(),
            away: lb.name.clone(),
            home: String::new(),
            away_score: 0,
            home_score: 0,
            status: format!("{:?}", g.status).to_lowercase(),
            detail,
            frame_id: String::new(),
            champ: g.champ.as_ref().map(|c| c.label.clone()),
        };
    }
    let detail = if g.clock.is_empty() {
        g.ordinal.clone()
    } else {
        format!("{} {}", g.ordinal, g.clock)
    };
    GameSummary {
        id: g.id.clone(),
        sport_id: g.sport_id.clone(),
        away: g.away.abbrev.clone(),
        home: g.home.abbrev.clone(),
        away_score: g.away.score,
        home_score: g.home.score,
        status: format!("{:?}", g.status).to_lowercase(),
        detail,
        frame_id: String::new(),
        champ: g.champ.as_ref().map(|c| c.label.clone()),
    }
}

// ---- per-board selection + rendering ----
pub(crate) fn is_live(g: &Game) -> bool {
    matches!(g.status, GameStatus::Active | GameStatus::Intermission)
}

/// Stable per-sport team key shared by the catalog and games: "{sport}-{abbrev}".
pub(crate) fn team_key(sport_id: &str, abbrev: &str) -> String {
    format!("{}-{}", sport_id, abbrev.to_lowercase())
}

fn game_matches(g: &Game, favs: &HashSet<String>) -> bool {
    favs.contains(&team_key(&g.sport_id, &g.away.abbrev))
        || favs.contains(&team_key(&g.sport_id, &g.home.abbrev))
}

/// Whether a board's big-games option covers a game's championship tier.
/// Tier ids come from `championship_options_for` / `classify_champ` and are
/// per-sport; broader options cover the more specific tiers above them
/// (e.g. NHL "conf" covers conference finals AND the cup final).
fn champ_option_covers(sport_id: &str, option_id: &str, tier: &str) -> bool {
    match (sport_id, option_id) {
        (_, "none") => false,
        // "All playoffs" covers every championship tier for the sport.
        (_, "playoffs") => true,
        ("nhl", "conf") => tier == "conf" || tier == "final",
        ("nhl", "final") => tier == "final",
        ("nba", "final") => tier == "final",
        ("mlb", "ws") => tier == "ws",
        ("nfl", "sb") => tier == "sb",
        // "Playoff + Bowls" covers both; "Bowl Games" covers just the bowls.
        ("cfb", "playoff") => true,
        ("cfb", "bowl") => tier == "bowl",
        ("cbb" | "wcbb", "tournament") => tier == "tournament",
        _ => option_id == tier,
    }
}

/// Whether the board's per-sport big-games preference wants this game.
fn game_matches_champ(g: &Game, cfg: &BoardConfig) -> bool {
    let Some(champ) = &g.champ else { return false };
    cfg.sports
        .iter()
        .find(|s| s.sport_id == g.sport_id)
        .and_then(|s| s.championship_option_id.as_deref())
        .map(|opt| champ_option_covers(&g.sport_id, opt, &champ.tier))
        .unwrap_or(false)
}

/// What the BOARD displays right now: the pinned game alone while a live focus is
/// set, otherwise the full candidate rotation.
fn chosen_games(state: &AppState, cfg: &BoardConfig) -> Vec<Game> {
    // Focus override: show ONLY this game while it's still live.
    if let Some(fid) = &cfg.focused_game_id {
        let games = state.games.lock().unwrap();
        if let Some(g) = games.iter().find(|g| &g.id == fid && is_live(g)) {
            return vec![g.clone()];
        }
    }
    candidate_games(state, cfg)
}

/// Games this board WANTS (favorites + big games + manual adds — or the whole live
/// slate for an unconfigured board), IGNORING any pin. The app's "now showing"
/// list uses this so a pinned board still shows the rest of the rotation (grayed
/// out client-side) instead of collapsing to a single card.
fn candidate_games(state: &AppState, cfg: &BoardConfig) -> Vec<Game> {
    let games = state.games.lock().unwrap();
    let favs: HashSet<String> = cfg
        .sports
        .iter()
        .flat_map(|s| s.favorite_team_ids.iter().cloned())
        .collect();
    let has_champ_prefs = cfg
        .sports
        .iter()
        .any(|s| s.championship_option_id.as_deref().is_some_and(|o| o != "none"));
    // Favorites' live games + big games matching the per-sport championship
    // option + any manually-added games. An unconfigured board (no favorites,
    // no big-games option, no extras) shows the whole live slate so it isn't
    // blank.
    let unconfigured = favs.is_empty() && cfg.extra_game_ids.is_empty() && !has_champ_prefs;
    let mut out: Vec<Game> = Vec::new();
    for g in games.iter().filter(|g| is_live(g)) {
        let wanted = (unconfigured
            || game_matches(g, &favs)
            || game_matches_champ(g, cfg)
            || cfg.extra_game_ids.contains(&g.id))
            && !cfg.excluded_game_ids.contains(&g.id);
        if wanted && !out.iter().any(|x| x.id == g.id) {
            out.push(g.clone());
            if out.len() >= 8 {
                break;
            }
        }
    }
    out
}

pub fn manifest_for_board(state: &AppState, board_id: &str) -> Manifest {
    let cfg = state.config_for(board_id);

    // Priority 1: live games (only while powered on).
    if cfg.power {
        let chosen = chosen_games(state, &cfg);
        if !chosen.is_empty() {
            // Games override any lingering notice ("no games" message ends early).
            state.notices.lock().unwrap().remove(board_id);
            let mut rotation = Vec::new();
            let mut frames = state.frames.lock().unwrap();
            for g in &chosen {
                let (id, px) = render::render_game(g);
                frames.insert(id.clone(), px);
                rotation.push(RotationEntry { frame_id: id, dwell_ms: 7000 });
            }
            return Manifest {
                rotation,
                display_on: true,
                off_state: cfg.off_state,
                brightness: cfg.brightness,
            };
        }
    }

    // Priority 2: a transient notice ("SLEEPING…" splash / "NO GAMES…"), rendered
    // fresh so its bottom progress bar reflects the time left before it goes away.
    {
        let mut notices = state.notices.lock().unwrap();
        if let Some(n) = notices.get(board_id) {
            let now = std::time::Instant::now();
            if now < n.until {
                let total = n.until.duration_since(n.created).as_secs_f32();
                let remaining = n.until.duration_since(now).as_secs_f32();
                let frac = if total > 0.0 { remaining / total } else { 0.0 };
                let lines: Vec<(&str, bool, embedded_graphics::pixelcolor::Rgb888)> =
                    n.lines.iter().map(|(s, b, c)| (s.as_str(), *b, *c)).collect();
                let (id, px) = render::render_notice(&lines, Some(frac));
                state.frames.lock().unwrap().insert(id.clone(), px);
                return Manifest {
                    rotation: vec![RotationEntry { frame_id: id, dwell_ms: 60_000 }],
                    display_on: true,
                    off_state: cfg.off_state,
                    brightness: cfg.brightness,
                };
            }
            notices.remove(board_id);
        }
    }

    // Priority 3: idle / asleep — the configured off-state (server-rendered clock
    // or a blank panel). Applies both to power-off and powered-on-but-no-games.
    let rotation = if cfg.off_state == OffState::Clock {
        let now = chrono::Local::now();
        use chrono::Timelike;
        let (id, px) = render::render_clock(now.hour(), now.minute());
        state.frames.lock().unwrap().insert(id.clone(), px);
        vec![RotationEntry { frame_id: id, dwell_ms: 60_000 }]
    } else {
        vec![]
    };
    Manifest {
        display_on: !rotation.is_empty(),
        rotation,
        off_state: cfg.off_state,
        brightness: cfg.brightness,
    }
}

/// Send the board its current manifest — skipped when identical to the last one
/// sent, so periodic refreshes don't reset the rotation position mid-dwell.
pub fn push_board(state: &AppState, board_id: &str) {
    let manifest = manifest_for_board(state, board_id);
    let json = serde_json::to_string(&ServerMsg::Manifest(manifest)).unwrap();
    {
        let mut last = state.last_manifest.lock().unwrap();
        if last.get(board_id) == Some(&json) {
            return;
        }
        last.insert(board_id.to_string(), json.clone());
    }
    if let Some(tx) = state.devices.lock().unwrap().get(board_id) {
        let _ = tx.send(Message::Text(json));
    }
}

const NOTICE_WHITE: embedded_graphics::pixelcolor::Rgb888 =
    embedded_graphics::pixelcolor::Rgb888::new(255, 255, 255);
const NOTICE_GREY: embedded_graphics::pixelcolor::Rgb888 =
    embedded_graphics::pixelcolor::Rgb888::new(160, 160, 160);
/// How long the "SLEEPING…" splash stays up before the off-state takes over.
const SPLASH_SECS: u64 = 4;
const NO_GAMES_SECS: u64 = 5 * 60;

/// Show a transient full-screen notice on one board, then spawn a repush loop that
/// advances its bottom progress bar and clears it on expiry. The repush cadence
/// scales with duration (one push ≈ per bar pixel), so a 4s splash updates ~4×/s
/// while the 5-minute notice only pushes every few seconds.
pub fn set_notice(state: &AppState, board_id: &str, lines: &[(&str, bool, embedded_graphics::pixelcolor::Rgb888)], secs: u64) {
    let now = std::time::Instant::now();
    let until = now + Duration::from_secs(secs);
    state.notices.lock().unwrap().insert(
        board_id.to_string(),
        Notice {
            lines: lines.iter().map(|(s, b, c)| (s.to_string(), *b, *c)).collect(),
            created: now,
            until,
        },
    );
    // (No-op outside a tokio runtime, i.e. in sync tests — the tick loop covers it.)
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let (state, board_id) = (state.clone(), board_id.to_string());
        let step = Duration::from_millis(((secs * 1000) / FRAME_WIDTH as u64).clamp(250, 10_000));
        handle.spawn(async move {
            loop {
                tokio::time::sleep(step).await;
                push_board(&state, &board_id);
                if std::time::Instant::now() >= until {
                    break; // final push above already swapped to the post-notice view
                }
            }
        });
    }
}

/// A single knob press, reported by the board. The server owns what it means:
/// - asleep (power off) -> wake; if nothing is live, show the "no games" notice
/// - showing games      -> sleep for `sleep_hours` (with a brief splash saying so)
/// - idle (no games)    -> show the "no games" notice for 5 minutes
pub fn handle_button(state: &AppState, board_id: &str) {
    let mut cfg = state.config_for(board_id);
    let has_games = !candidate_games(state, &cfg).is_empty();

    if !cfg.power {
        // Wake up.
        cfg.power = true;
        cfg.wake_at = None;
        if !has_games {
            set_notice(state, board_id, &[("NO GAMES", true, NOTICE_WHITE), ("SLEEPING IN", false, NOTICE_GREY), ("5 MINUTES", false, NOTICE_GREY)], NO_GAMES_SECS);
        }
        tracing::info!("board '{board_id}': button -> wake");
    } else if has_games {
        // Sleep for the configured duration.
        let hours = cfg.sleep_hours.max(1);
        cfg.power = false;
        cfg.wake_at = Some(unix_now() + hours as u64 * 3600);
        let caption = if hours == 1 { "FOR 1 HOUR".to_string() } else { format!("FOR {hours} HOURS") };
        set_notice(state, board_id, &[("SLEEPING", true, NOTICE_WHITE), (&caption, false, NOTICE_GREY)], SPLASH_SECS);
        tracing::info!("board '{board_id}': button -> sleep {hours}h");
    } else {
        // Powered on but idle: show why the panel is dark, then re-sleep.
        set_notice(state, board_id, &[("NO GAMES", true, NOTICE_WHITE), ("SLEEPING IN", false, NOTICE_GREY), ("5 MINUTES", false, NOTICE_GREY)], NO_GAMES_SECS);
        tracing::info!("board '{board_id}': button -> no-games notice");
    }

    state.configs.lock().unwrap().insert(board_id.to_string(), cfg.clone());
    state.persist_config(board_id, &cfg);
    push_board(state, board_id);
    push_board_state(state, board_id);
}

/// The app toggled power. Mirror the physical button's feedback: powering off
/// shows the "SLEEPING" splash (indefinite sleep — no duration line); powering on
/// cancels any active notice so the games/off-state appear immediately.
pub fn handle_app_power(state: &AppState, board_id: &str, on: bool) {
    if on {
        state.notices.lock().unwrap().remove(board_id);
    } else {
        set_notice(state, board_id, &[("SLEEPING", true, NOTICE_WHITE)], SPLASH_SECS);
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One housekeeping pass: wake any boards whose sleep timer lapsed, and re-push
/// manifests to connected boards so the idle clock advances. The identical-manifest
/// dedup in `push_board` makes the blanket re-push free for boards showing games.
pub fn tick_once(state: &AppState) {
    // Wake-timer check covers ALL configured boards (a board can be offline when
    // its timer lapses; the config should still flip so the app shows it awake).
    let lapsed: Vec<String> = state
        .configs
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, c)| c.wake_at.is_some_and(|w| unix_now() >= w))
        .map(|(id, _)| id.clone())
        .collect();
    for bid in lapsed {
        let mut cfg = state.config_for(&bid);
        cfg.power = true;
        cfg.wake_at = None;
        state.configs.lock().unwrap().insert(bid.clone(), cfg.clone());
        state.persist_config(&bid, &cfg);
        push_board_state(state, &bid);
        tracing::info!("board '{bid}': sleep timer lapsed -> waking");
    }
    let board_ids: Vec<String> = state.devices.lock().unwrap().keys().cloned().collect();
    for bid in board_ids {
        push_board(state, &bid);
    }
}

async fn tick_loop(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    loop {
        interval.tick().await;
        tick_once(&state);
    }
}

/// Push fresh board state (now-showing list + config) to every app subscribed to
/// this board, pruning any whose socket has closed.
pub fn push_board_state(state: &AppState, board_id: &str) {
    let state_msg = board_state_msg(state, board_id);
    // Binary frames for the now-showing games, pushed right after the state so the
    // app renders them in place — the same content-addressed mechanism the board uses.
    let frame_msgs: Vec<Message> = {
        let cache = state.frames.lock().unwrap();
        match &state_msg {
            ServerMsg::BoardState { now_showing, .. } => now_showing
                .iter()
                .filter(|g| !g.frame_id.is_empty())
                .filter_map(|g| cache.get(&g.frame_id).map(|px| Message::Binary(encode_frame(&g.frame_id, px))))
                .collect(),
            _ => Vec::new(),
        }
    };
    let json = Message::Text(serde_json::to_string(&state_msg).unwrap());
    let mut controllers = state.controllers.lock().unwrap();
    if let Some(subs) = controllers.get_mut(board_id) {
        subs.retain(|tx| {
            if tx.send(json.clone()).is_err() {
                return false;
            }
            frame_msgs.iter().all(|f| tx.send(f.clone()).is_ok())
        });
        if subs.is_empty() {
            controllers.remove(board_id);
        }
    }
}

pub fn board_state_msg(state: &AppState, board_id: &str) -> ServerMsg {
    let cfg = state.config_for(board_id);
    // Render each now-showing game, cache its frame, and tag the summary with the
    // frame id so the controller can pull the frame over the WS (no HTTP).
    // Deliberately IGNORES a pin: the app needs the whole rotation so it can gray
    // out the unpinned games (the pin itself travels in config.focused_game_id).
    let now_showing: Vec<GameSummary> = {
        let chosen = candidate_games(state, &cfg);
        let mut frames = state.frames.lock().unwrap();
        chosen
            .iter()
            .map(|g| {
                let mut s = to_summary(g);
                let (fid, px) = render::render_game(g);
                frames.insert(fid.clone(), px);
                s.frame_id = fid;
                s
            })
            .collect()
    };
    let online = state.devices.lock().unwrap().contains_key(board_id);
    ServerMsg::BoardState {
        board_id: board_id.to_string(),
        online,
        now_showing,
        config: cfg,
    }
}

async fn poll_loop(state: AppState, source: Arc<dyn SportsDataSource>) {
    // Poll cadence is configurable (default 3 min); set POLL_SECS to override.
    let secs: u64 = std::env::var("POLL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&s| s > 0)
        .unwrap_or(180);
    tracing::info!("poll loop: every {secs}s, skipped while nothing is connected");
    let mut interval = tokio::time::interval(Duration::from_secs(secs));
    loop {
        // Wake on the timer OR as soon as a client connects (fresh games promptly).
        let woke_on_connect = tokio::select! {
            _ = interval.tick() => false,
            _ = state.poll_wake.notified() => true,
        };
        // A plain timer tick skips while idle; a just-connected client ALWAYS triggers
        // a fetch — it isn't in the registries yet (it may be browsing before/without
        // ever subscribing), so the idle check would wrongly skip it.
        if !woke_on_connect {
            let no_devices = state.devices.lock().unwrap().is_empty();
            let no_controllers = state.controllers.lock().unwrap().is_empty();
            if no_devices && no_controllers {
                continue;
            }
        }
        let mut games = source.fetch().await;
        if games.is_empty() {
            tracing::warn!("no games from source; using mock games");
            games = espn::mock_games();
        }
        let n = games.len();
        *state.games.lock().unwrap() = games;
        let board_ids: Vec<String> = state.devices.lock().unwrap().keys().cloned().collect();
        for bid in &board_ids {
            push_board(&state, bid);
        }
        // Refresh the now-showing list + scores in every subscribed app.
        let controller_boards: Vec<String> = state.controllers.lock().unwrap().keys().cloned().collect();
        for bid in &controller_boards {
            push_board_state(&state, bid);
        }
        tracing::info!("fetched {n} games; refreshed {} board(s)", board_ids.len());
    }
}

// ---- mock catalog (team list for the app; real ESPN catalog is a later step) ----
fn team(sport: &str, abbrev: &str, name: &str) -> Team {
    Team {
        id: team_key(sport, abbrev),
        league_id: sport.into(),
        name: name.into(),
        abbrev: abbrev.into(),
        // Leaderboard "teams" (golf tours, F1) carry no brand color; ESPN + the
        // colored mock teams below do.
        color: None,
        alt_color: None,
    }
}

/// Like `team`, but with brand colors (hex, no `#`) — so demo/offline mode shows
/// the same team-colored swatches the live ESPN catalog provides.
fn team_c(sport: &str, abbrev: &str, name: &str, color: &str, alt: &str) -> Team {
    Team {
        color: Some(color.into()),
        alt_color: Some(alt.into()),
        ..team(sport, abbrev, name)
    }
}

fn champ(id: &str, label: &str) -> ChampionshipOption {
    ChampionshipOption { id: id.into(), label: label.into() }
}

/// Per-sport "big games" groupings (app-domain, not from ESPN). Shared by the
/// mock catalog and the live ESPN catalog.
pub(crate) fn championship_options_for(sport_id: &str) -> Vec<ChampionshipOption> {
    let opts: &[(&str, &str)] = match sport_id {
        "nhl" => &[
            ("none", "Off"),
            ("final", "Stanley Cup Final"),
            ("conf", "Conference Finals+"),
            ("playoffs", "All playoffs"),
        ],
        "nba" => &[("none", "Off"), ("final", "NBA Finals"), ("playoffs", "All playoffs")],
        "mlb" => &[("none", "Off"), ("ws", "World Series"), ("playoffs", "All playoffs")],
        "nfl" => &[("none", "Off"), ("sb", "Super Bowl"), ("playoffs", "All playoffs")],
        "golf" | "f1" => &[], // leaderboard sports: just favorite the tour/series
        _ => &[("none", "Off"), ("playoffs", "All playoffs")],
    };
    opts.iter().map(|(id, label)| champ(id, label)).collect()
}

/// Golf as one sport with favoritable PGA + LPGA "teams" — reuses the favorites
/// pipeline (favorite a tour to show its live tournaments).
pub(crate) fn golf_sport() -> SportDescriptor {
    SportDescriptor {
        id: "golf".into(),
        name: "Golf".into(),
        teams: vec![team("golf", "PGA", "PGA Tour"), team("golf", "LPGA", "LPGA Tour")],
        championship_options: vec![],
    }
}

/// Formula 1 as one sport with a single favoritable "Formula 1" entry.
pub(crate) fn f1_sport() -> SportDescriptor {
    SportDescriptor {
        id: "f1".into(),
        name: "Formula 1".into(),
        teams: vec![team("f1", "F1", "Formula 1")],
        championship_options: vec![],
    }
}

/// Where the last-good live catalog is cached (alongside the SQLite DB, so it
/// survives restarts and — on Railway — redeploys via the mounted STATE_DIR).
fn catalog_cache_path() -> PathBuf {
    let dir = std::env::var("STATE_DIR").unwrap_or_else(|_| ".".into());
    PathBuf::from(dir).join("catalog.json")
}

/// Persist a freshly-fetched catalog so a later ESPN outage can still serve the
/// full team list. Best-effort — a write failure just means we fall back to mock.
fn cache_catalog(catalog: &Catalog) {
    let path = catalog_cache_path();
    match serde_json::to_vec(catalog) {
        Ok(bytes) => match std::fs::write(&path, bytes) {
            Ok(()) => tracing::info!("catalog: cached {} sports to {}", catalog.sports.len(), path.display()),
            Err(e) => tracing::warn!("catalog cache write {}: {e}", path.display()),
        },
        Err(e) => tracing::warn!("catalog serialize: {e}"),
    }
}

/// Load the catalog cached by a previous successful fetch, if one exists.
fn load_cached_catalog() -> Option<Catalog> {
    let path = catalog_cache_path();
    let bytes = std::fs::read(&path).ok()?;
    match serde_json::from_slice::<Catalog>(&bytes) {
        Ok(c) => {
            tracing::info!("catalog: loaded {} sports from cache {}", c.sports.len(), path.display());
            Some(c)
        }
        Err(e) => {
            tracing::warn!("catalog cache parse {}: {e}", path.display());
            None
        }
    }
}

fn mock_catalog() -> Catalog {
    let sport = |id: &str, name: &str, teams: Vec<Team>| SportDescriptor {
        id: id.into(),
        name: name.into(),
        teams,
        championship_options: championship_options_for(id),
    };
    Catalog {
        sports: vec![
            sport(
                "nhl",
                "NHL",
                vec![
                    team_c("nhl", "BOS", "Bruins", "FFB81C", "000000"),
                    team_c("nhl", "TOR", "Maple Leafs", "00205B", "FFFFFF"),
                    team_c("nhl", "NYR", "Rangers", "0038A8", "CE1126"),
                    team_c("nhl", "EDM", "Oilers", "041E42", "FF4C00"),
                    team_c("nhl", "COL", "Avalanche", "6F263D", "236192"),
                    team_c("nhl", "VGK", "Golden Knights", "B4975A", "333F42"),
                ],
            ),
            sport(
                "nba",
                "NBA",
                vec![
                    team_c("nba", "BOS", "Celtics", "007A33", "FFFFFF"),
                    team_c("nba", "LAL", "Lakers", "552583", "FDB927"),
                    team_c("nba", "GSW", "Warriors", "1D428A", "FFC72C"),
                    team_c("nba", "DEN", "Nuggets", "0E2240", "FEC524"),
                    team_c("nba", "MIA", "Heat", "98002E", "F9A01B"),
                ],
            ),
            sport(
                "mlb",
                "MLB",
                vec![
                    team_c("mlb", "NYY", "Yankees", "003087", "E4002C"),
                    team_c("mlb", "BOS", "Red Sox", "BD3039", "0C2340"),
                    team_c("mlb", "LAD", "Dodgers", "005A9C", "EF3E42"),
                    team_c("mlb", "CHC", "Cubs", "0E3386", "CC3433"),
                    team_c("mlb", "HOU", "Astros", "002D62", "EB6E1F"),
                ],
            ),
            sport(
                "nfl",
                "NFL",
                vec![
                    team_c("nfl", "NE", "Patriots", "002244", "C60C30"),
                    team_c("nfl", "BUF", "Bills", "00338D", "C60C30"),
                    team_c("nfl", "KC", "Chiefs", "E31837", "FFB81C"),
                    team_c("nfl", "DAL", "Cowboys", "003594", "869397"),
                    team_c("nfl", "SF", "49ers", "AA0000", "B3995D"),
                ],
            ),
            golf_sport(),
            f1_sport(),
        ],
    }
}

// ---- `server --preview`: render sample games to scaled PNGs for eyeballing ----
fn preview() {
    use model::{Game, GameStatus, TeamSide};
    let dir = "/tmp/sb_preview";
    std::fs::create_dir_all(dir).ok();
    let mk = |abbrev: &str, name: &str, score: u32, p: &str, s: &str| TeamSide {
        abbrev: abbrev.into(),
        name: name.into(),
        score,
        primary: model::hex_to_rgb(p),
        secondary: model::ensure_contrast(model::hex_to_rgb(p), model::hex_to_rgb(s)),
    };
    let g = |id: &str, sport: &str, away, home, status, ordinal: &str, clock: &str| Game {
        id: id.into(),
        sport_id: sport.into(),
        away,
        home,
        status,
        ordinal: ordinal.into(),
        clock: clock.into(),
        start_time: String::new(),
        leaderboard: None,
        champ: None,
    };
    let samples: Vec<(&str, Game)> = vec![
        // Tagged as a championship game so the app's big-game caption is
        // exercisable in demo mode (out of playoff season there are no live
        // postseason games to test against).
        ("nhl_active", {
            let mut game = g("1", "nhl", mk("BOS", "BRUINS", 2, "FFB81C", "000000"), mk("TOR", "LEAFS", 1, "00205B", "FFFFFF"), GameStatus::Active, "2nd", "12:34");
            game.champ = Some(model::Champ { tier: "final".into(), label: "Stanley Cup Final".into() });
            game
        }),
        ("nba_active", g("2", "nba", mk("BOS", "CELTICS", 88, "007A33", "FFFFFF"), mk("LAL", "LAKERS", 84, "552583", "FDB927"), GameStatus::Active, "4th", "5:20")),
        ("nhl_final", g("5", "nhl", mk("BOS", "BRUINS", 4, "FFB81C", "000000"), mk("TOR", "LEAFS", 2, "00205B", "FFFFFF"), GameStatus::Final, "", "")),
        // Kerning torture tile (5x8): LT / AV pairs that leave holes with fixed advance.
        ("kern_5x8", g("9", "nhl", mk("TB", "BOLTS", 3, "002868", "FFFFFF"), mk("LV", "GRAVY", 2, "C8102E", "FFFFFF"), GameStatus::Active, "1st", "8:00")),
        ("golf", Game {
            id: "g".into(), sport_id: "golf".into(),
            away: model::TeamSide::empty("PGA"), home: model::TeamSide::empty(""),
            status: GameStatus::Active, ordinal: String::new(), clock: String::new(), start_time: String::new(),
            leaderboard: Some(model::Leaderboard { name: "MASTERS".into(), rows: vec![
                model::LeaderRow { name: "SCHEFFLER".into(), value: "-8".into() },
                model::LeaderRow { name: "MCILROY".into(), value: "-6".into() },
                model::LeaderRow { name: "MORIKAWA".into(), value: "-5".into() },
                model::LeaderRow { name: "SCHAUFFELE".into(), value: "-3".into() },
                model::LeaderRow { name: "FLEETWOOD".into(), value: "-2".into() },
            ] }),
            champ: None,
        }),
        ("f1", Game {
            id: "f".into(), sport_id: "f1".into(),
            away: model::TeamSide::empty("F1"), home: model::TeamSide::empty(""),
            status: GameStatus::Active, ordinal: String::new(), clock: String::new(), start_time: String::new(),
            leaderboard: Some(model::Leaderboard { name: "MONACO GP".into(), rows: vec![
                model::LeaderRow { name: "VERSTAPPEN".into(), value: "LAP 42".into() },
                model::LeaderRow { name: "NORRIS".into(), value: "+2.1".into() },
                model::LeaderRow { name: "LECLERC".into(), value: "+5.8".into() },
                model::LeaderRow { name: "PIASTRI".into(), value: "+9.0".into() },
                model::LeaderRow { name: "HAMILTON".into(), value: "+12".into() },
            ] }),
            champ: None,
        }),
    ];
    for (name, game) in &samples {
        let (_, px) = render::render_game(game);
        let path = format!("{dir}/{name}.png");
        save_png(&path, &px, 12);
        println!("wrote {path}");
    }
}

fn rgb565_to_image(rgb565: &[u8], scale: u32) -> image::RgbImage {
    let mut img = image::RgbImage::new(FRAME_WIDTH as u32 * scale, FRAME_HEIGHT as u32 * scale);
    for y in 0..FRAME_HEIGHT {
        for x in 0..FRAME_WIDTH {
            let i = (y * FRAME_WIDTH + x) * 2;
            let c = u16::from_le_bytes([rgb565[i], rgb565[i + 1]]);
            let (r, gr, b) = rgb565_to_888(c);
            for dy in 0..scale {
                for dx in 0..scale {
                    img.put_pixel(x as u32 * scale + dx, y as u32 * scale + dy, image::Rgb([r, gr, b]));
                }
            }
        }
    }
    img
}

fn save_png(path: &str, rgb565: &[u8], scale: u32) {
    rgb565_to_image(rgb565, scale).save(path).ok();
}

/// Encode a rendered frame as PNG bytes (64x32 source; the app upscales it crisply).
fn render_png(rgb565: &[u8]) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(rgb565_to_image(rgb565, 1))
        .write_to(&mut buf, image::ImageFormat::Png)
        .ok();
    buf.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{Game, GameStatus, TeamSide};

    /// Button while showing games -> sleep (power off, wake timer, splash notice);
    /// button again -> wake, games return, notice cleared.
    #[test]
    fn button_sleep_wake_cycle() {
        let state = AppState::new_ephemeral(); // mock games incl. live ones
        handle_button(&state, "b1");
        let cfg = state.config_for("b1");
        assert!(!cfg.power, "press while showing games should sleep");
        let wake = cfg.wake_at.expect("wake timer set");
        let expect = unix_now() + 24 * 3600;
        assert!((wake as i64 - expect as i64).abs() <= 5, "default sleep is 24h");
        let m = manifest_for_board(&state, "b1");
        assert!(m.display_on && m.rotation.len() == 1, "sleep splash notice shows");

        handle_button(&state, "b1");
        let cfg = state.config_for("b1");
        assert!(cfg.power && cfg.wake_at.is_none(), "second press wakes");
        let m = manifest_for_board(&state, "b1");
        assert!(!m.rotation.is_empty(), "games back after wake");
        assert!(!state.notices.lock().unwrap().contains_key("b1"), "games clear the notice");
    }

    /// Button while powered on but idle (no live games) -> the "no games" notice,
    /// without turning the board off.
    #[test]
    fn button_with_no_games_shows_notice() {
        let state = AppState::new_ephemeral();
        state.games.lock().unwrap().clear();
        handle_button(&state, "b1");
        let cfg = state.config_for("b1");
        assert!(cfg.power, "no-games press must not sleep the board");
        assert!(state.notices.lock().unwrap().contains_key("b1"));
        let m = manifest_for_board(&state, "b1");
        assert!(m.display_on && m.rotation.len() == 1, "notice frame shows");
    }

    /// Removing a preference-driven game EXCLUDES it (preferences untouched);
    /// adding it back un-excludes; ResetRotation restores everything at once.
    #[test]
    fn exclusion_add_and_reset_cycle() {
        let state = AppState::new_ephemeral(); // unconfigured -> whole live slate
        let all = candidate_games(&state, &BoardConfig::default());
        assert!(all.len() >= 2);
        let victim = all[0].id.clone();

        // Remove (a non-extra game) -> excluded, dropped from the rotation.
        crate::ws::apply_command(&state, "b1", Command::RemoveGame { game_id: victim.clone() });
        let cfg = state.config_for("b1");
        assert!(cfg.excluded_game_ids.contains(&victim));
        assert!(!candidate_games(&state, &cfg).iter().any(|g| g.id == victim));

        // Add it back -> un-excluded (NOT turned into an extra).
        crate::ws::apply_command(&state, "b1", Command::AddGame { game_id: victim.clone() });
        let cfg = state.config_for("b1");
        assert!(cfg.excluded_game_ids.is_empty() && cfg.extra_game_ids.is_empty());
        assert!(candidate_games(&state, &cfg).iter().any(|g| g.id == victim));

        // Exclude again + pin something, then ResetRotation clears the lot.
        crate::ws::apply_command(&state, "b1", Command::RemoveGame { game_id: victim.clone() });
        crate::ws::apply_command(&state, "b1", Command::FocusGame { game_id: all[1].id.clone() });
        crate::ws::apply_command(&state, "b1", Command::ResetRotation);
        let cfg = state.config_for("b1");
        assert!(cfg.focused_game_id.is_none());
        assert!(cfg.extra_game_ids.is_empty() && cfg.excluded_game_ids.is_empty());
    }

    /// Pinning a game narrows the BOARD to one frame, but the app's now-showing
    /// list must still contain the whole rotation (it grays out the unpinned ones).
    #[test]
    fn pinned_board_still_reports_full_rotation_to_app() {
        let state = AppState::new_ephemeral(); // unconfigured -> whole live slate
        let all = candidate_games(&state, &BoardConfig::default());
        assert!(all.len() >= 2, "mock slate should have several live games");

        let cfg = BoardConfig { focused_game_id: Some(all[0].id.clone()), ..Default::default() };
        state.configs.lock().unwrap().insert("b1".into(), cfg);

        let m = manifest_for_board(&state, "b1");
        assert_eq!(m.rotation.len(), 1, "the board shows only the pinned game");

        let ServerMsg::BoardState { now_showing, config, .. } = board_state_msg(&state, "b1") else {
            panic!("expected BoardState");
        };
        assert_eq!(now_showing.len(), all.len(), "the app still sees every candidate");
        assert_eq!(config.focused_game_id.as_deref(), Some(all[0].id.as_str()));
    }

    /// App power-off shows the "SLEEPING" splash; power-on cancels it immediately.
    #[test]
    fn app_power_toggle_drives_notice() {
        let state = AppState::new_ephemeral();
        handle_app_power(&state, "b1", false);
        assert!(state.notices.lock().unwrap().contains_key("b1"), "power off -> splash");
        // Config isn't flipped here (apply_command does that), so force it off to
        // check the manifest shows the notice rather than the off-state.
        let cfg = BoardConfig { power: false, ..Default::default() };
        state.configs.lock().unwrap().insert("b1".into(), cfg);
        let m = manifest_for_board(&state, "b1");
        assert!(m.display_on && m.rotation.len() == 1, "splash frame shows");
        handle_app_power(&state, "b1", true);
        assert!(!state.notices.lock().unwrap().contains_key("b1"), "power on cancels the notice");
    }

    /// The notice frame carries a depleting progress bar: as time passes the render
    /// changes (bar shrinks), so successive manifests differ.
    #[test]
    fn notice_progress_bar_depletes() {
        let full = render::render_notice(&[("SLEEPING", true, NOTICE_WHITE)], Some(1.0));
        let half = render::render_notice(&[("SLEEPING", true, NOTICE_WHITE)], Some(0.5));
        let none = render::render_notice(&[("SLEEPING", true, NOTICE_WHITE)], Some(0.0));
        assert_ne!(full.0, half.0, "bar width must change the frame");
        assert_ne!(half.0, none.0);
        // The bottom row's lit pixel count tracks the remaining fraction.
        let lit_bottom = |px: &[u8]| {
            (0..scoreboard_protocol::FRAME_WIDTH)
                .filter(|x| {
                    let i = ((scoreboard_protocol::FRAME_HEIGHT - 1) * scoreboard_protocol::FRAME_WIDTH + x) * 2;
                    px[i] != 0 || px[i + 1] != 0
                })
                .count()
        };
        assert_eq!(lit_bottom(&full.1), 64);
        assert_eq!(lit_bottom(&half.1), 32);
        assert_eq!(lit_bottom(&none.1), 0);
    }

    /// A lapsed sleep timer flips power back on at the next housekeeping tick.
    #[test]
    fn lapsed_sleep_timer_wakes() {
        let state = AppState::new_ephemeral();
        let cfg = BoardConfig { power: false, wake_at: Some(unix_now() - 10), ..Default::default() };
        state.configs.lock().unwrap().insert("b1".into(), cfg);
        tick_once(&state);
        let cfg = state.config_for("b1");
        assert!(cfg.power && cfg.wake_at.is_none(), "timer lapse wakes the board");
    }

    /// Off/idle presentation follows the configured off-state: Clock renders a
    /// server-side clock frame; Blank turns the display off with nothing to show.
    #[test]
    fn off_state_clock_vs_blank() {
        let state = AppState::new_ephemeral();
        state.games.lock().unwrap().clear();
        let mut cfg = BoardConfig { power: false, ..Default::default() }; // default off_state = Clock
        state.configs.lock().unwrap().insert("b1".into(), cfg.clone());
        let m = manifest_for_board(&state, "b1");
        assert!(m.display_on && m.rotation.len() == 1, "clock frame while off");
        assert!(state.frames.lock().unwrap().contains_key(&m.rotation[0].frame_id));

        cfg.off_state = OffState::Blank;
        state.configs.lock().unwrap().insert("b1".into(), cfg);
        let m = manifest_for_board(&state, "b1");
        assert!(!m.display_on && m.rotation.is_empty(), "blank = display off");
    }

    #[test]
    fn games_message_serializes() {
        // Internally-tagged ServerMsg can't serialize a newtype variant wrapping a
        // sequence, so Games must stay a STRUCT variant — this used to panic the
        // connection task on every browse (GetGames) request.
        let json = serde_json::to_string(&ServerMsg::Games { games: vec![] }).unwrap();
        assert!(matches!(
            serde_json::from_str::<ServerMsg>(&json),
            Ok(ServerMsg::Games { .. })
        ));
    }

    #[test]
    fn now_showing_carries_cached_frame_ids() {
        // Each now-showing game must carry a frame id whose frame is cached, so the
        // controller can pull it over the WS (the no-HTTP "now showing" path).
        let s = AppState::new_ephemeral();
        let ServerMsg::BoardState { now_showing, .. } = board_state_msg(&s, "board-x") else {
            panic!("expected BoardState");
        };
        assert!(!now_showing.is_empty(), "mock games should be showing");
        let frames = s.frames.lock().unwrap();
        for g in &now_showing {
            assert!(!g.frame_id.is_empty(), "game {} has a frame id", g.id);
            assert!(frames.contains_key(&g.frame_id), "frame cached for {}", g.id);
        }
    }

    #[test]
    fn sqlite_round_trip() {
        // Persist a config + pairing, then reload a fresh AppState from the same DB.
        let path = std::env::temp_dir().join(format!("sb-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let state = AppState::new_inner(Some(open_db(&path).unwrap()), mock_catalog());
            let cfg = BoardConfig { brightness: 42, power: false, ..Default::default() };
            state.persist_config("board-x", &cfg);
            state.persist_pairing("board-x", "ctrl-1");
        }
        {
            let state = AppState::new_inner(Some(open_db(&path).unwrap()), mock_catalog());
            let cfg = state.config_for("board-x");
            assert_eq!(cfg.brightness, 42);
            assert!(!cfg.power);
            assert!(state.is_allowed("board-x", "ctrl-1"));
            assert!(!state.is_allowed("board-x", "ctrl-2"));
        }
        let _ = std::fs::remove_file(&path);
    }

    fn ts(abbrev: &str) -> TeamSide {
        TeamSide {
            abbrev: abbrev.into(),
            name: abbrev.into(),
            score: 0,
            primary: model::hex_to_rgb("000000"),
            secondary: model::hex_to_rgb("FFFFFF"),
        }
    }
    fn game(sport: &str, away: &str, home: &str) -> Game {
        Game {
            id: "x".into(),
            sport_id: sport.into(),
            away: ts(away),
            home: ts(home),
            status: GameStatus::Active,
            ordinal: String::new(),
            clock: String::new(),
            start_time: String::new(),
            leaderboard: None,
            champ: None,
        }
    }

    /// A big-games preference pulls in exactly the games its tier covers —
    /// and counts as "configured", so the whole-slate fallback stays off.
    #[test]
    fn champ_pref_selects_covered_games() {
        let state = AppState::new_ephemeral();
        {
            let mut games = state.games.lock().unwrap();
            games.clear();
            let mut cup = game("nhl", "FLA", "EDM");
            cup.id = "cup".into();
            cup.champ = Some(model::Champ { tier: "final".into(), label: "Stanley Cup Final".into() });
            let mut rd1 = game("nhl", "DAL", "COL");
            rd1.id = "rd1".into();
            rd1.champ = Some(model::Champ { tier: "playoffs".into(), label: "Playoffs".into() });
            let mut reg = game("nhl", "BOS", "TOR");
            reg.id = "reg".into();
            games.extend([cup, rd1, reg]);
        }

        let mut cfg = BoardConfig::default();
        cfg.sports.push(SportConfig {
            sport_id: "nhl".into(),
            favorite_team_ids: vec![],
            championship_option_id: Some("conf".into()),
        });
        // "Conference Finals+" covers the cup final but not an early round,
        // and the regular-season game isn't a championship game at all.
        let ids: Vec<String> = candidate_games(&state, &cfg).iter().map(|g| g.id.clone()).collect();
        assert_eq!(ids, ["cup"]);

        // Broaden to "All playoffs" -> the early round comes in too.
        cfg.sports[0].championship_option_id = Some("playoffs".into());
        let ids: Vec<String> = candidate_games(&state, &cfg).iter().map(|g| g.id.clone()).collect();
        assert_eq!(ids, ["cup", "rd1"]);

        // A one-off removal still beats the preference.
        cfg.excluded_game_ids.push("cup".into());
        let ids: Vec<String> = candidate_games(&state, &cfg).iter().map(|g| g.id.clone()).collect();
        assert_eq!(ids, ["rd1"]);
    }

    #[test]
    fn champ_option_coverage_matrix() {
        assert!(champ_option_covers("nhl", "playoffs", "playoffs"));
        assert!(champ_option_covers("nhl", "playoffs", "final"));
        assert!(champ_option_covers("nhl", "conf", "final"));
        assert!(champ_option_covers("nhl", "conf", "conf"));
        assert!(!champ_option_covers("nhl", "conf", "playoffs"));
        assert!(!champ_option_covers("nhl", "final", "conf"));
        assert!(!champ_option_covers("nhl", "none", "final"));
        // CFB's options are containment-inverted: "Playoff + Bowls" covers
        // both tiers, "Bowl Games" only the bowls.
        assert!(champ_option_covers("cfb", "playoff", "bowl"));
        assert!(champ_option_covers("cfb", "playoff", "playoff"));
        assert!(!champ_option_covers("cfb", "bowl", "playoff"));
    }

    #[test]
    fn pairing_rendezvous() {
        let s = AppState::new_ephemeral();
        s.pending.lock().unwrap().insert("123456".into(), "board-x".into());
        // correct code pairs + authorizes the controller
        assert_eq!(try_pair(&s, "123456", "ctrl-1").unwrap(), "board-x");
        assert!(s.allow.lock().unwrap().get("board-x").unwrap().contains("ctrl-1"));
        // the code is single-use
        assert!(try_pair(&s, "123456", "ctrl-1").is_err());
        // an unknown code fails
        assert!(try_pair(&s, "999999", "ctrl-2").is_err());
    }

    #[test]
    fn favorites_match_by_sport_and_abbrev() {
        let favs: HashSet<String> = ["nhl-bos".to_string()].into_iter().collect();
        assert!(game_matches(&game("nhl", "BOS", "TOR"), &favs)); // away
        assert!(game_matches(&game("nhl", "NYR", "BOS"), &favs)); // home
        assert!(!game_matches(&game("nba", "BOS", "LAL"), &favs)); // same abbrev, different sport
        assert!(!game_matches(&game("nhl", "NYR", "TOR"), &favs)); // unrelated
    }

    #[test]
    fn focus_and_extras_drive_rotation() {
        let s = AppState::new_ephemeral();
        let mut g1 = game("nhl", "BOS", "TOR");
        g1.id = "g1".into();
        let mut g2 = game("nba", "LAL", "GSW");
        g2.id = "g2".into();
        let mut g3 = game("mlb", "NYY", "DET");
        g3.id = "g3".into();
        *s.games.lock().unwrap() = vec![g1, g2, g3];
        let ids = |c: &BoardConfig| -> Vec<String> {
            chosen_games(&s, c).iter().map(|g| g.id.clone()).collect()
        };

        // No favorites, no extras -> the whole live slate.
        assert_eq!(ids(&BoardConfig::default()).len(), 3);

        // Favorites: only matching games (BOS nhl -> g1).
        let mut cfg = BoardConfig::default();
        cfg.sports.push(SportConfig {
            sport_id: "nhl".into(),
            favorite_team_ids: vec!["nhl-bos".into()],
            championship_option_id: None,
        });
        assert_eq!(ids(&cfg), vec!["g1".to_string()]);

        // + a manually-added extra game -> g1 and g2.
        cfg.extra_game_ids.push("g2".into());
        let got = ids(&cfg);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&"g1".to_string()) && got.contains(&"g2".to_string()));

        // Focus overrides everything -> only g3.
        cfg.focused_game_id = Some("g3".into());
        assert_eq!(ids(&cfg), vec!["g3".to_string()]);
    }
}
