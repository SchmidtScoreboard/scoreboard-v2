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
    /// When we WATCHED each game transition to Final (ESPN has no end time, so we
    /// observe it at poll granularity). Drives the post-game linger; memory-only —
    /// a restart mid-linger just ends the linger early.
    pub finished: Arc<Mutex<HashMap<String, std::time::Instant>>>,
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
        let state = Self::new_inner(None, mock_catalog());
        // Tests exercise selection/rotation against a known slate.
        *state.games.lock().unwrap() = espn::mock_games();
        state
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
        let (mut configs, allow) = match &db {
            Some(conn) => (load_configs(conn), load_pairings(conn)),
            None => (HashMap::new(), HashMap::new()),
        };
        sanitize_configs(&catalog, &mut configs);
        AppState {
            catalog: Arc::new(catalog),
            // Empty until the first real fetch — boards show their no-games
            // behavior rather than fake placeholder games.
            games: Arc::new(Mutex::new(Vec::new())),
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
            finished: Arc::new(Mutex::new(HashMap::new())),
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

/// Drop config leftovers that no longer make sense against the catalog:
/// team favorites for leagues that have NO teams (PGA/LPGA/F1 became
/// option-subscription leagues, orphaning their old pseudo-teams). Sports
/// absent from the catalog are left untouched — the catalog may be the small
/// mock fallback, and deleting real preferences against it would be data loss.
fn sanitize_configs(catalog: &Catalog, configs: &mut HashMap<String, BoardConfig>) {
    let teamless: HashSet<&str> = catalog
        .sports
        .iter()
        .filter(|s| s.teams.is_empty())
        .map(|s| s.id.as_str())
        .collect();
    for cfg in configs.values_mut() {
        for s in cfg.sports.iter_mut() {
            if teamless.contains(s.sport_id.as_str()) && !s.favorite_team_ids.is_empty() {
                tracing::info!("dropping orphaned favorites for teamless league '{}'", s.sport_id);
                s.favorite_team_ids.clear();
            }
        }
    }
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

    // One-shot: fetch the full ESPN catalog, write the disk cache, exit. Seeds
    // demo mode (which never contacts ESPN) with the real team lists.
    if std::env::args().any(|a| a == "--fetch-catalog") {
        match espn::EspnSource::new().fetch_catalog().await {
            Some(c) => {
                cache_catalog(&c);
                let teams: usize = c.sports.iter().map(|s| s.teams.len()).sum();
                println!("cached {} sports / {} teams -> {}", c.sports.len(), teams, catalog_cache_path().display());
            }
            None => {
                eprintln!("catalog fetch failed (ESPN unreachable?); cache unchanged");
                std::process::exit(1);
            }
        }
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
    // roster, independent of which games are live. Live mode caches the last good
    // catalog to disk; demo mode (whose source never fetches) reads that same
    // cache, so it serves the full team list WITHOUT ever contacting ESPN.
    // Seed or refresh the cache with `server --fetch-catalog`.
    let catalog = match source.fetch_catalog().await {
        Some(c) => {
            tracing::info!("catalog: {} sports from source", c.sports.len());
            cache_catalog(&c);
            c
        }
        None => load_cached_catalog().unwrap_or_else(|| {
            tracing::warn!(
                "catalog: no cache yet, using mock (run `server --fetch-catalog` to seed the full list)"
            );
            mock_catalog()
        }),
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
    // Scheduled games: the useful detail is WHEN they start ("7:05 PM"), for
    // both team games and leaderboard events.
    let pregame_detail = || {
        (g.status == GameStatus::Pregame)
            .then(|| start_time_label(&g.start_time))
            .flatten()
    };
    // Leaderboard sports (golf/F1): no away/home — show the event + leader.
    if let Some(lb) = &g.leaderboard {
        let detail = pregame_detail().unwrap_or_else(|| {
            lb.rows
                .first()
                .map(|r| format!("{} {}", r.name, r.value))
                .unwrap_or_default()
        });
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
            bumped: false,
            champ: g.champ.as_ref().map(|c| c.label.clone()),
        };
    }
    let detail = pregame_detail().unwrap_or_else(|| {
        if g.clock.is_empty() {
            g.ordinal.clone()
        } else {
            format!("{} {}", g.ordinal, g.clock)
        }
    });
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
        bumped: false,
        champ: g.champ.as_ref().map(|c| c.label.clone()),
    }
}

// ---- per-board selection + rendering ----
pub(crate) fn is_live(g: &Game) -> bool {
    matches!(g.status, GameStatus::Active | GameStatus::Intermission)
}

/// How early a scheduled game may appear on the BOARD (matches the original
/// scoreboard: live games, plus anything starting within half an hour).
const PREGAME_WINDOW_MINS: i64 = 30;

/// How long a just-ended game keeps showing its FINAL frame on the board.
const FINAL_LINGER: Duration = Duration::from_secs(30 * 60);

/// Called with each fresh slate BEFORE it replaces `state.games`: any game that
/// was live in the old slate and is Final in the new one gets an end stamp NOW.
/// Games that arrive already-Final (server restart, day-old finals in the feed)
/// are never stamped — only an observed transition starts a linger.
pub(crate) fn record_finished_transitions(state: &AppState, incoming: &[Game]) {
    let was_live: HashSet<String> = state
        .games
        .lock()
        .unwrap()
        .iter()
        .filter(|g| is_live(g))
        .map(|g| g.id.clone())
        .collect();
    let mut fin = state.finished.lock().unwrap();
    for g in incoming {
        if g.status == GameStatus::Final && was_live.contains(&g.id) {
            fin.entry(g.id.clone()).or_insert_with(std::time::Instant::now);
            tracing::info!("game '{}' ended; lingering {}m", g.id, FINAL_LINGER.as_secs() / 60);
        }
    }
    // Keep the map bounded: drop stamps past the linger or for vanished games.
    fin.retain(|id, t| {
        t.elapsed() < FINAL_LINGER && incoming.iter().any(|g| &g.id == id)
    });
}

/// Parse ESPN's start-time format ("2026-06-28T13:00Z", sometimes with seconds).
pub(crate) fn parse_espn_time(iso: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::{DateTime, NaiveDateTime, Utc};
    if let Ok(t) = DateTime::parse_from_rfc3339(iso) {
        return Some(t.with_timezone(&Utc));
    }
    for fmt in ["%Y-%m-%dT%H:%MZ", "%Y-%m-%dT%H:%M:%SZ"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(iso, fmt) {
            return Some(t.and_utc());
        }
    }
    None
}

/// The game's local start time as frame/caption text ("7:05 PM"), if parseable.
pub(crate) fn start_time_label(iso: &str) -> Option<String> {
    let t = parse_espn_time(iso)?;
    Some(t.with_timezone(&chrono::Local).format("%-I:%M %p").to_string())
}

/// Should the BOARD show this game? Live always; scheduled only inside the
/// pregame window. (The app's browse feed is broader — see `browse_shows`.)
fn board_shows_at(g: &Game, now: chrono::DateTime<chrono::Utc>) -> bool {
    if is_live(g) {
        return true;
    }
    if g.status != GameStatus::Pregame {
        return false;
    }
    match parse_espn_time(&g.start_time) {
        // Within the window, including games slightly past their scheduled start
        // that ESPN hasn't flipped to in-progress yet.
        Some(start) => (start - now).num_minutes() <= PREGAME_WINDOW_MINS,
        None => false,
    }
}

/// State-aware board filter: the pure time-window logic, plus the post-game
/// linger (a Final whose end we watched keeps showing for `FINAL_LINGER`).
pub(crate) fn board_shows(state: &AppState, g: &Game) -> bool {
    if g.status == GameStatus::Final {
        return state
            .finished
            .lock()
            .unwrap()
            .get(&g.id)
            .is_some_and(|t| t.elapsed() < FINAL_LINGER);
    }
    board_shows_at(g, chrono::Utc::now())
}

/// Should the app's "View all" browse feed show this game? Live plus ALL of the
/// slate's upcoming games (so you can queue up tonight's game this morning).
pub(crate) fn browse_shows(g: &Game) -> bool {
    is_live(g) || g.status == GameStatus::Pregame
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
        ("wnba", "final") => tier == "final",
        ("mlb", "ws") => tier == "ws",
        ("nfl", "sb") => tier == "sb",
        // "Playoff + Bowls" covers both; "Bowl Games" covers just the bowls.
        ("cfb", "playoff") => true,
        ("cfb", "bowl") => tier == "bowl",
        ("cbb" | "wcbb", "tournament") => tier == "tournament",
        ("mls", "cup") => tier == "cup",
        // World Cup tiers nest: final ⊂ semis ⊂ the knockout stage.
        ("wc", "final") => tier == "final",
        ("wc", "semi") => matches!(tier, "final" | "semi"),
        ("wc", "knockout") => matches!(tier, "final" | "semi" | "quarter" | "knockout"),
        // Golf: "majors" covers major-tier events (the "all" option is handled in
        // `game_option_priority`, since regular events carry no champ tag).
        ("pga" | "lpga", "majors") => tier == "major",
        _ => option_id == tier,
    }
}

/// Which F1 sessions a subscription level includes.
fn f1_option_covers(option_id: &str, session: &str) -> bool {
    match option_id {
        "gp" => session == "race",
        "gp-quali" => matches!(session, "race" | "quali"),
        // Sprints bring their own qualifying session along.
        "gp-quali-sprint" => matches!(session, "race" | "quali" | "sprint" | "sprint-quali"),
        _ => false,
    }
}

/// The board's league option for a game's sport, if one is set (never "none").
fn league_option<'a>(cfg: &'a BoardConfig, sport_id: &str) -> Option<&'a str> {
    cfg.sports
        .iter()
        .find(|s| s.sport_id == sport_id)
        .and_then(|s| s.championship_option_id.as_deref())
        .filter(|o| *o != "none")
}

/// How strongly the league OPTION claims this game: Some(2) for a targeted match
/// (championship tier, golf major, F1 session), Some(3) for the "all" blanket,
/// None when the option doesn't cover it.
fn game_option_priority(g: &Game, cfg: &BoardConfig) -> Option<u8> {
    let opt = league_option(cfg, &g.sport_id)?;
    // "all" is the universal blanket: every event/match in the league.
    if opt == "all" {
        return Some(3);
    }
    let covered = match g.sport_id.as_str() {
        // F1 subscribes to session kinds, not championships.
        "f1" => g.session.as_deref().is_some_and(|s| f1_option_covers(opt, s)),
        // Everything else keys off the game's championship tier.
        _ => g
            .champ
            .as_ref()
            .is_some_and(|c| champ_option_covers(&g.sport_id, opt, &c.tier)),
    };
    covered.then_some(2)
}

/// Rotation slots are finite; when more games qualify than fit, the strongest
/// claims win. 0 = manually added (an explicit "I want THIS game"), 1 = favorite
/// team, 2 = targeted league option (big game / major / session), 3 = league
/// blanket ("Every match"), 4 = unconfigured board showing the whole slate.
fn game_priority(g: &Game, cfg: &BoardConfig, favs: &HashSet<String>, unconfigured: bool) -> Option<u8> {
    if cfg.excluded_game_ids.contains(&g.id) {
        return None;
    }
    if cfg.extra_game_ids.contains(&g.id) {
        return Some(0);
    }
    if game_matches(g, favs) {
        return Some(1);
    }
    if let Some(p) = game_option_priority(g, cfg) {
        return Some(p);
    }
    if unconfigured {
        return Some(4);
    }
    None
}

/// How many games fit in the board's rotation.
const ROTATION_CAP: usize = 20;

/// All games this board WANTS, split into (shown, bumped): priority-ordered, the
/// first `ROTATION_CAP` get rotation slots; the rest are reported to the app as
/// bumped so the user can promote them (manual adds outrank everything).
/// Unconfigured boards sample the slate without reporting bumps — nothing there
/// was chosen, so nothing was "lost".
fn candidates_with_bumped(state: &AppState, cfg: &BoardConfig) -> (Vec<Game>, Vec<Game>) {
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
    let unconfigured = favs.is_empty() && cfg.extra_game_ids.is_empty() && !has_champ_prefs;

    let mut seen: HashSet<String> = HashSet::new();
    let mut wanted: Vec<(u8, usize, Game)> = Vec::new();
    for (i, g) in games.iter().enumerate().filter(|(_, g)| board_shows(state, g)) {
        if !seen.insert(g.id.clone()) {
            continue;
        }
        if let Some(p) = game_priority(g, cfg, &favs, unconfigured) {
            wanted.push((p, i, g.clone()));
        }
    }
    wanted.sort_by_key(|(p, i, _)| (*p, *i)); // priority, then feed order (stable)
    let mut shown: Vec<Game> = Vec::new();
    let mut bumped: Vec<Game> = Vec::new();
    for (_, _, g) in wanted {
        if shown.len() < ROTATION_CAP {
            shown.push(g);
        } else if !unconfigured {
            bumped.push(g);
        }
    }
    (shown, bumped)
}

/// Games this board's rotation shows (favorites + options + manual adds — or the
/// whole live slate for an unconfigured board), IGNORING any pin.
fn candidate_games(state: &AppState, cfg: &BoardConfig) -> Vec<Game> {
    candidates_with_bumped(state, cfg).0
}

/// What the BOARD displays right now: the pinned game alone while a live focus is
/// set, otherwise the full candidate rotation.
fn chosen_games(state: &AppState, cfg: &BoardConfig) -> Vec<Game> {
    // Focus override: show ONLY this game while it's still live.
    if let Some(fid) = &cfg.focused_game_id {
        let games = state.games.lock().unwrap();
        if let Some(g) = games.iter().find(|g| &g.id == fid && board_shows(state, g)) {
            return vec![g.clone()];
        }
    }
    candidate_games(state, cfg)
}

pub fn manifest_for_board(state: &AppState, board_id: &str) -> Manifest {
    let cfg = state.config_for(board_id);

    // Priority 1: live games (only while powered on).
    if cfg.power {
        let chosen = chosen_games(state, &cfg);
        if !chosen.is_empty() {
            // Games override any lingering notice ("no games" message ends early).
            state.notices.lock().unwrap().remove(board_id);
            let dwell_ms = cfg.dwell_secs.clamp(3, 60) * 1000;
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

/// The device knob turned. There is ONE brightness per board — the knob writes
/// the same config field the app's slider does; the next manifest echoes it back
/// to the device and the app sees it in the board state.
pub fn handle_device_brightness(state: &AppState, board_id: &str, level: u8) {
    let mut cfg = state.config_for(board_id);
    let level = level.clamp(10, 100);
    if cfg.brightness == level {
        return;
    }
    cfg.brightness = level;
    state.configs.lock().unwrap().insert(board_id.to_string(), cfg.clone());
    state.persist_config(board_id, &cfg);
    push_board(state, board_id);
    push_board_state(state, board_id);
}

/// The app toggled power. Same behavior as the physical button: powering off is
/// a timed sleep (`apply_command` armed `wake_at`), so the splash shows the same
/// duration line; powering on cancels any active notice so the games/off-state
/// appear immediately.
pub fn handle_app_power(state: &AppState, board_id: &str, on: bool) {
    if on {
        state.notices.lock().unwrap().remove(board_id);
    } else {
        let hours = state.config_for(board_id).sleep_hours.max(1);
        let caption = if hours == 1 { "FOR 1 HOUR".to_string() } else { format!("FOR {hours} HOURS") };
        set_notice(state, board_id, &[("SLEEPING", true, NOTICE_WHITE), (&caption, false, NOTICE_GREY)], SPLASH_SECS);
    }
}

pub(crate) fn unix_now() -> u64 {
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
        let (shown, bumped) = candidates_with_bumped(state, &cfg);
        let mut out: Vec<GameSummary> = shown
            .iter()
            .map(|g| {
                let mut s = to_summary(g);
                let (fid, px) = render::render_game(g);
                frames.insert(fid.clone(), px);
                s.frame_id = fid;
                s
            })
            .collect();
        // Bumped games ride along flagged, without pre-rendered frames (the app
        // falls back to HTTP rendering for those cards).
        out.extend(bumped.iter().map(|g| {
            let mut s = to_summary(g);
            s.bumped = true;
            s
        }));
        out
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
        let games = source.fetch().await;
        if games.is_empty() {
            // Honest empty slate: boards fall through to their no-games
            // behavior. (This used to substitute fake mock games, which
            // put made-up scores on a real scoreboard.)
            tracing::warn!("no games from source; showing empty slate");
        }
        let n = games.len();
        // Stamp any live -> Final transitions before the old slate disappears.
        record_finished_transitions(&state, &games);
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
            ("all", "Every match"),
        ],
        "nba" => &[("none", "Off"), ("final", "NBA Finals"), ("playoffs", "All playoffs"), ("all", "Every match")],
        "wnba" => &[("none", "Off"), ("final", "WNBA Finals"), ("playoffs", "All playoffs"), ("all", "Every match")],
        "mlb" => &[("none", "Off"), ("ws", "World Series"), ("playoffs", "All playoffs"), ("all", "Every match")],
        "nfl" => &[("none", "Off"), ("sb", "Super Bowl"), ("playoffs", "All playoffs"), ("all", "Every match")],
        "cfb" => &[("none", "Off"), ("playoff", "CFP + bowls"), ("bowl", "Bowl games only"), ("all", "Every match")],
        "cbb" | "wcbb" => &[("none", "Off"), ("tournament", "March Madness"), ("all", "Every match")],
        "mls" => &[("none", "Off"), ("cup", "MLS Cup"), ("playoffs", "All playoffs"), ("all", "Every match")],
        // EPL has no playoffs, so the only league-wide subscription is all of it.
        "epl" => &[("none", "Off"), ("all", "Every match")],
        "wc" => &[
            ("none", "Off"),
            ("final", "World Cup Final"),
            ("semi", "Semifinals +"),
            ("knockout", "Knockout stage"),
            ("all", "Every match"),
        ],
        // Leaderboard sports subscribe by EVENT KIND, not by team.
        "pga" | "lpga" => &[("none", "Off"), ("all", "All events"), ("majors", "Majors only")],
        "f1" => &[
            ("none", "Off"),
            ("gp", "Grands Prix"),
            ("gp-quali", "GP + Qualifying"),
            ("gp-quali-sprint", "GP + Qualifying + Sprints"),
        ],
        _ => &[("none", "Off"), ("playoffs", "All playoffs"), ("all", "Every match")],
    };
    opts.iter().map(|(id, label)| champ(id, label)).collect()
}

/// The leaderboard "leagues" (PGA / LPGA / F1) have no teams — they're
/// subscribed via their event-kind options above.
pub(crate) fn leaderboard_sports() -> Vec<SportDescriptor> {
    let sport = |id: &str, name: &str| SportDescriptor {
        id: id.into(),
        name: name.into(),
        teams: vec![],
        championship_options: championship_options_for(id),
    };
    vec![sport("pga", "PGA Tour"), sport("lpga", "LPGA Tour"), sport("f1", "Formula 1")]
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
        ]
        .into_iter()
        .chain(leaderboard_sports())
        .collect(),
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
        session: None,
        situation: None,
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
        // The three situational layouts ported from the original scoreboard.
        ("nfl_situation", {
            let mut game = g("s1", "nfl", mk("SF", "49ERS", 14, "AA0000", "B3995D"), mk("SEA", "SEAHAWKS", 17, "002244", "69BE28"), GameStatus::Active, "3rd", "8:12");
            game.situation = Some(model::Situation::Football {
                down_string: "3rd + 4".into(),
                ball_position: "SEA 25".into(),
                home_possession: Some(false),
            });
            game
        }),
        ("mlb_situation", {
            let mut game = g("s2", "mlb", mk("HOU", "ASTROS", 2, "002D62", "EB6E1F"), mk("SEA", "MARINERS", 4, "0C2C56", "005C5C"), GameStatus::Active, "5th", "");
            game.situation = Some(model::Situation::Baseball {
                balls: 3, strikes: 2, outs: 2,
                on_first: true, on_second: true, on_third: false,
                is_inning_top: true,
            });
            game
        }),
        ("nhl_powerplay", {
            let mut game = g("s3", "nhl", mk("DAL", "STARS", 1, "006341", "A2AAAD"), mk("STL", "BLUES", 2, "002F87", "FCB514"), GameStatus::Active, "2nd", "14:02");
            game.situation = Some(model::Situation::Hockey {
                away_powerplay: true, home_powerplay: false,
                away_players: 5, home_players: 4,
            });
            game
        }),
        ("nhl_final", g("5", "nhl", mk("BOS", "BRUINS", 4, "FFB81C", "000000"), mk("TOR", "LEAFS", 2, "00205B", "FFFFFF"), GameStatus::Final, "", "")),
        // Kerning torture tile (5x8): LT / AV pairs that leave holes with fixed advance.
        ("kern_5x8", g("9", "nhl", mk("TB", "BOLTS", 3, "002868", "FFFFFF"), mk("LV", "GRAVY", 2, "C8102E", "FFFFFF"), GameStatus::Active, "1st", "8:00")),
        ("golf", Game {
            id: "g".into(), sport_id: "pga".into(),
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
            session: None,
            situation: None,
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
            session: None,
            situation: None,
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

    /// App power-off arms the SAME timed sleep the button does (wake_at from
    /// sleep_hours + the "SLEEPING FOR N HOURS" splash); power-on cancels it.
    #[test]
    fn app_power_off_arms_the_sleep_timer() {
        let state = AppState::new_ephemeral();
        crate::ws::apply_command(&state, "b1", Command::SetSleepHours { hours: 2 });
        crate::ws::apply_command(&state, "b1", Command::SetPower { on: false });
        let cfg = state.config_for("b1");
        assert!(!cfg.power);
        let wake = cfg.wake_at.expect("app sleep sets a wake timer like the button");
        let expect = unix_now() + 2 * 3600;
        assert!((wake as i64 - expect as i64).abs() <= 5, "timer = sleep_hours");
        assert!(state.notices.lock().unwrap().contains_key("b1"), "sleep splash shows");

        crate::ws::apply_command(&state, "b1", Command::SetPower { on: true });
        let cfg = state.config_for("b1");
        assert!(cfg.power && cfg.wake_at.is_none(), "power-on cancels the timer");

        // "Off until I say so": cancel the timer while staying asleep.
        crate::ws::apply_command(&state, "b1", Command::SetPower { on: false });
        crate::ws::apply_command(&state, "b1", Command::ClearWakeTimer);
        let cfg = state.config_for("b1");
        assert!(!cfg.power && cfg.wake_at.is_none(), "indefinite off: asleep, no timer");
    }

    /// A knob turn writes the SAME brightness the app's slider does — one value,
    /// persisted and echoed in the manifest, clamped to the knob's 10..=100 range.
    #[test]
    fn device_knob_sets_the_one_brightness() {
        let state = AppState::new_ephemeral();
        handle_device_brightness(&state, "b1", 40);
        assert_eq!(state.config_for("b1").brightness, 40);
        assert_eq!(manifest_for_board(&state, "b1").brightness, 40, "manifest echoes it");

        handle_device_brightness(&state, "b1", 0); // below the knob floor
        assert_eq!(state.config_for("b1").brightness, 10, "clamped to 10..=100");
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

    /// Loading configs drops team favorites for leagues the catalog says have no
    /// teams (the old golf/F1 pseudo-teams), but never touches teamed leagues or
    /// sports missing from the catalog (which may just be the mock fallback).
    #[test]
    fn orphaned_teamless_favorites_are_dropped_on_load() {
        let mut configs = HashMap::new();
        configs.insert(
            "b1".to_string(),
            BoardConfig {
                sports: vec![
                    SportConfig {
                        sport_id: "f1".into(),
                        favorite_team_ids: vec!["f1-f1".into()], // orphan from before the split
                        championship_option_id: Some("gp".into()),
                    },
                    SportConfig {
                        sport_id: "nhl".into(),
                        favorite_team_ids: vec!["nhl-bos".into()],
                        championship_option_id: None,
                    },
                    SportConfig {
                        sport_id: "golf".into(), // sport no longer in the catalog at all
                        favorite_team_ids: vec!["golf-pga".into()],
                        championship_option_id: None,
                    },
                ],
                ..Default::default()
            },
        );
        sanitize_configs(&mock_catalog(), &mut configs);
        let cfg = &configs["b1"];
        let by_id = |id: &str| cfg.sports.iter().find(|s| s.sport_id == id).unwrap();
        assert!(by_id("f1").favorite_team_ids.is_empty(), "teamless league favorites dropped");
        assert_eq!(by_id("f1").championship_option_id.as_deref(), Some("gp"), "option kept");
        assert_eq!(by_id("nhl").favorite_team_ids, vec!["nhl-bos".to_string()], "teamed league untouched");
        assert_eq!(by_id("golf").favorite_team_ids, vec!["golf-pga".to_string()], "absent sport left alone");
    }

    /// The per-league subscription options: golf all/majors, F1 session levels,
    /// World Cup rounds + "every match".
    #[test]
    fn league_option_subscriptions() {
        // Test-readability shim over `game_option_priority`.
        let game_matches_league_option =
            |g: &Game, cfg: &BoardConfig| game_option_priority(g, cfg).is_some();
        let mk = |sport: &str, champ_tier: Option<&str>, session: Option<&str>| Game {
            id: "g".into(),
            sport_id: sport.into(),
            away: TeamSide::empty("X"),
            home: TeamSide::empty(""),
            status: GameStatus::Active,
            ordinal: String::new(),
            clock: String::new(),
            start_time: String::new(),
            leaderboard: None,
            champ: champ_tier.map(|t| model::Champ { tier: t.into(), label: t.into() }),
            session: session.map(Into::into),
            situation: None,
        };
        let cfg = |sport: &str, opt: &str| BoardConfig {
            sports: vec![SportConfig {
                sport_id: sport.into(),
                favorite_team_ids: vec![],
                championship_option_id: Some(opt.into()),
            }],
            ..Default::default()
        };

        // Golf: "all" shows everything; "majors" only major-tier events.
        assert!(game_matches_league_option(&mk("pga", None, None), &cfg("pga", "all")));
        assert!(!game_matches_league_option(&mk("pga", None, None), &cfg("pga", "majors")));
        assert!(game_matches_league_option(&mk("pga", Some("major"), None), &cfg("pga", "majors")));

        // F1 session levels nest race < +quali < +sprints.
        assert!(game_matches_league_option(&mk("f1", None, Some("race")), &cfg("f1", "gp")));
        assert!(!game_matches_league_option(&mk("f1", None, Some("quali")), &cfg("f1", "gp")));
        assert!(game_matches_league_option(&mk("f1", None, Some("quali")), &cfg("f1", "gp-quali")));
        assert!(!game_matches_league_option(&mk("f1", None, Some("sprint")), &cfg("f1", "gp-quali")));
        assert!(game_matches_league_option(&mk("f1", None, Some("sprint-quali")), &cfg("f1", "gp-quali-sprint")));

        // World Cup: knockout covers the rounds; group stage only via "all".
        assert!(game_matches_league_option(&mk("wc", Some("quarter"), None), &cfg("wc", "knockout")));
        assert!(!game_matches_league_option(&mk("wc", Some("knockout"), None), &cfg("wc", "semi")));
        assert!(game_matches_league_option(&mk("wc", Some("final"), None), &cfg("wc", "semi")));
        assert!(!game_matches_league_option(&mk("wc", None, None), &cfg("wc", "knockout")));
        assert!(game_matches_league_option(&mk("wc", None, None), &cfg("wc", "all")));

        // WNBA Finals-only now exists.
        assert!(game_matches_league_option(&mk("wnba", Some("final"), None), &cfg("wnba", "final")));
        assert!(!game_matches_league_option(&mk("wnba", Some("playoffs"), None), &cfg("wnba", "final")));

        // "Every match" is a universal blanket, champ tag or not.
        assert!(game_matches_league_option(&mk("nhl", None, None), &cfg("nhl", "all")));
        assert!(game_matches_league_option(&mk("epl", None, None), &cfg("epl", "all")));
        assert!(!game_matches_league_option(&mk("nhl", None, None), &cfg("nhl", "playoffs")));
    }

    /// Rotation slots go by priority — manual adds first, then favorites, then
    /// targeted options, then blankets — and everything past the cap is reported
    /// as bumped (except for unconfigured boards sampling the slate).
    #[test]
    fn rotation_cap_prioritizes_and_reports_bumped() {
        let state = AppState::new_ephemeral();
        let side = |abbrev: &str| TeamSide {
            abbrev: abbrev.into(),
            name: abbrev.into(),
            score: 0,
            primary: model::hex_to_rgb("111111"),
            secondary: model::hex_to_rgb("ffffff"),
        };
        // 25 blanket-covered NHL games, then one favorite game and one manual add
        // at the END of the feed (weakest feed position, strongest claims).
        let mut games: Vec<Game> = (0..25)
            .map(|i| Game {
                id: format!("blanket-{i}"),
                sport_id: "nhl".into(),
                away: side("AAA"),
                home: side("BBB"),
                status: GameStatus::Active,
                ordinal: "1st".into(),
                clock: "5:00".into(),
                start_time: String::new(),
                leaderboard: None,
                champ: None,
                session: None,
                situation: None,
            })
            .collect();
        games.push(Game { id: "fav-game".into(), away: side("FAV"), ..games[0].clone() });
        games.push(Game { id: "added-game".into(), away: side("ZZZ"), ..games[0].clone() });
        *state.games.lock().unwrap() = games;

        let cfg = BoardConfig {
            sports: vec![SportConfig {
                sport_id: "nhl".into(),
                favorite_team_ids: vec!["nhl-fav".into()],
                championship_option_id: Some("all".into()),
            }],
            extra_game_ids: vec!["added-game".into()],
            ..Default::default()
        };
        let (shown, bumped) = candidates_with_bumped(&state, &cfg);
        assert_eq!(shown.len(), 20, "cap is 20");
        assert_eq!(shown[0].id, "added-game", "manual add outranks everything");
        assert_eq!(shown[1].id, "fav-game", "favorite outranks the blanket");
        assert_eq!(bumped.len(), 7, "25 + 2 wanted, 20 shown");
        assert!(bumped.iter().all(|g| g.id.starts_with("blanket-")), "only blanket games bumped");

        // The app sees the bumped games flagged in now_showing.
        state.configs.lock().unwrap().insert("b1".into(), cfg);
        let ServerMsg::BoardState { now_showing, .. } = board_state_msg(&state, "b1") else {
            panic!("expected BoardState");
        };
        assert_eq!(now_showing.len(), 27);
        assert_eq!(now_showing.iter().filter(|g| g.bumped).count(), 7);
        assert!(now_showing.iter().filter(|g| g.bumped).all(|g| g.frame_id.is_empty()));

        // Unconfigured boards sample the slate silently — no bump reports.
        let (shown, bumped) = candidates_with_bumped(&state, &BoardConfig::default());
        assert_eq!(shown.len(), 20);
        assert!(bumped.is_empty(), "nothing was chosen, so nothing was lost");
    }

    /// A game whose end we observed lingers on the board for 30 minutes; one that
    /// arrives already-Final (restart, stale feed) never lingers; the linger expires.
    #[test]
    fn finals_linger_half_hour_after_observed_end() {
        let state = AppState::new_ephemeral();
        let live = Game {
            id: "g1".into(),
            sport_id: "nhl".into(),
            away: TeamSide::empty("A"),
            home: TeamSide::empty("B"),
            status: GameStatus::Active,
            ordinal: "3rd".into(),
            clock: "1:00".into(),
            start_time: String::new(),
            leaderboard: None,
            champ: None,
            session: None,
            situation: None,
        };
        *state.games.lock().unwrap() = vec![live.clone()];

        // Poll observes the transition to Final.
        let ended = Game { status: GameStatus::Final, ordinal: String::new(), clock: String::new(), ..live };
        let already_final = Game { id: "g2".into(), ..ended.clone() };
        let slate = vec![ended, already_final];
        record_finished_transitions(&state, &slate);
        *state.games.lock().unwrap() = slate;

        let cfg = BoardConfig::default(); // unconfigured -> whole slate
        let ids: Vec<String> = candidate_games(&state, &cfg).iter().map(|g| g.id.clone()).collect();
        assert!(ids.contains(&"g1".to_string()), "observed final lingers");
        assert!(!ids.contains(&"g2".to_string()), "arrived-final never lingers");

        // Age the stamp past the linger: the game drops off the board.
        state
            .finished
            .lock()
            .unwrap()
            .insert("g1".into(), std::time::Instant::now() - FINAL_LINGER);
        let ids: Vec<String> = candidate_games(&state, &cfg).iter().map(|g| g.id.clone()).collect();
        assert!(!ids.contains(&"g1".to_string()), "linger expires after 30m");
    }

    /// The board shows live games plus scheduled ones inside the 30-minute
    /// pregame window; the browse feed shows ALL upcoming games.
    #[test]
    fn pregame_window_gates_board_but_not_browse() {
        use chrono::{Duration as D, Utc};
        let now = Utc::now();
        let mk = |mins_from_now: i64| Game {
            id: "g".into(),
            sport_id: "nhl".into(),
            away: TeamSide::empty("A"),
            home: TeamSide::empty("B"),
            status: GameStatus::Pregame,
            ordinal: String::new(),
            clock: String::new(),
            start_time: (now + D::minutes(mins_from_now)).format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            leaderboard: None,
            champ: None,
            session: None,
            situation: None,
        };
        assert!(board_shows_at(&mk(10), now), "starts in 10 min -> on the board");
        assert!(board_shows_at(&mk(-5), now), "past scheduled start, not yet live -> keep showing");
        assert!(!board_shows_at(&mk(120), now), "starts in 2h -> NOT on the board");
        assert!(browse_shows(&mk(120)), "…but browsable in View all");
        // Unparseable start time: never board-shown early.
        let mut odd = mk(10);
        odd.start_time = "not-a-time".into();
        assert!(!board_shows_at(&odd, now));
    }

    /// ESPN timestamps (with and without seconds) parse; labels render local time.
    #[test]
    fn espn_time_parsing() {
        assert!(parse_espn_time("2026-06-28T13:00Z").is_some());
        assert!(parse_espn_time("2026-06-28T13:00:00Z").is_some());
        assert!(parse_espn_time("garbage").is_none());
        assert!(start_time_label("2026-06-28T13:00Z").is_some());
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
            session: None,
            situation: None,
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
