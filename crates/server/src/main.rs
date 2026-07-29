//! Local scoreboard server. Pulls live games from ESPN, stores per-board
//! preferences (favorite teams etc.), and renders a per-board rotation filtered
//! by those preferences. Talks to boards + controllers over a websocket.

mod bdf;
mod espn;
mod model;
mod render;
mod team_names;
mod teams;
mod tui;
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
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Is this bucket back at capacity? A full bucket holds no history, so the
    /// per-controller map can drop it instead of remembering every identity
    /// that ever tried a code.
    fn is_full(&mut self) -> bool {
        self.refill();
        self.tokens >= self.capacity
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let gained = now.duration_since(self.last).as_secs_f64() * self.refill_per_sec;
        self.tokens = (self.tokens + gained).min(self.capacity);
        self.last = now;
    }
}

/// Per-controller pairing budget: a short burst, refilling slowly. Sized so a
/// human retyping a mistyped code never notices it.
const PAIR_BURST: f64 = 10.0;
const PAIR_REFILL_PER_SEC: f64 = 1.0;

/// Whole-server pairing ceiling. Pubkeys are free to mint, so the per-identity
/// buckets alone can't bound total guessing load — but this is set far above
/// any honest simultaneous-pairing burst, so exhausting it takes a deliberate
/// attack rather than a busy evening.
const PAIR_GLOBAL_BURST: f64 = 60.0;
const PAIR_GLOBAL_REFILL_PER_SEC: f64 = 5.0;

/// How long a pairing code shown on a board stays valid. The board leaves setup
/// long before this; without an expiry every code a board ever displayed stayed
/// live forever, so the guessable space grew with every setup session.
const PAIR_CODE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone)]
pub struct AppState {
    pub catalog: Arc<Catalog>,
    pub games: Arc<Mutex<Vec<Game>>>,
    /// Rendered frames by content id, stamped with the eviction generation they
    /// were minted in (so `evict_stale_frames` never drops a frame between its
    /// render and its manifest registration — that write pair isn't atomic).
    pub frames: Arc<Mutex<HashMap<FrameId, (u64, Vec<u8>)>>>,
    pub configs: Arc<Mutex<HashMap<String, BoardConfig>>>, // board_id -> prefs
    pub devices: Arc<Mutex<HashMap<String, UnboundedSender<Message>>>>, // board_id -> sink
    pub controllers: Arc<Mutex<HashMap<String, Vec<UnboundedSender<Message>>>>>, // board_id -> app sinks
    /// Live pairing codes: code -> (board_id, when the board registered it).
    /// The stamp expires the code (`PAIR_CODE_TTL`) — see `register_pairing_code`.
    pub pending: Arc<Mutex<HashMap<String, (String, Instant)>>>,
    pub allow: Arc<Mutex<HashMap<String, HashSet<String>>>>, // board_id -> controller ids
    db: Option<Arc<Mutex<Connection>>>,
    /// Pairing-attempt budget per controller pubkey. One shared bucket meant a
    /// single attacker's guessing spree locked every other phone out of pairing.
    pair_limiters: Arc<Mutex<HashMap<String, RateLimiter>>>,
    /// Backstop above the per-identity buckets (fresh pubkeys are free).
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
    /// Frame ids kept by the previous eviction pass — one grace generation so a
    /// device still fetching the manifest we just replaced can't miss its frames.
    frame_grace: Arc<Mutex<HashSet<FrameId>>>,
    /// Current eviction generation; every cached frame is stamped with it.
    frame_gen: Arc<std::sync::atomic::AtomicU64>,
    /// When we WATCHED each game transition to Final (ESPN has no end time, so we
    /// observe it at poll granularity). Drives the post-game linger; memory-only —
    /// a restart mid-linger just ends the linger early.
    pub finished: Arc<Mutex<HashMap<String, std::time::Instant>>>,
    /// The OTA firmware images this server hosts, one per release channel
    /// (loaded once from FIRMWARE_DIR/{stable,beta,dev} at startup, each
    /// integrity-checked). An absent channel is simply off: its routes 404 and
    /// it never enters an offer. Empty map = OTA updates off entirely.
    pub firmware: HashMap<FwChannel, Arc<FirmwareDist>>,
    /// Firmware version each connected board last reported
    /// (`ClientMsg::DeviceVersion`). Volatile on purpose — devices re-send on
    /// every connect, and the entry is dropped on disconnect (BoardState says
    /// None while offline).
    pub device_versions: Arc<Mutex<HashMap<String, String>>>,
    /// One-time OTA download tokens: a fresh hex nonce (minted per firmware
    /// offer) → the board_id it was minted for + when. The offer's download
    /// path carries `?u={nonce}`; the streaming `/firmware/{chan}.bin` GET
    /// resolves it to know whose controllers should see `FirmwareProgress`.
    /// The nonce is deliberately UNSIGNED (only version/size/channel/bytes are
    /// signed — the board verifies the image regardless). Entries older than
    /// `FW_TOKEN_TTL` are swept lazily on every touch (offers the board never
    /// acted on, downloads that never finished). Un-tokenized GETs never enter
    /// this map and emit no progress.
    pub fw_download_tokens: Arc<Mutex<HashMap<String, (String, std::time::Instant)>>>,
}

/// The signed metadata half of an OTA dist — exactly the firmware.json that
/// `sign-firmware` writes and GET /firmware/{channel}/latest serves.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct FirmwareManifest {
    pub version: String,
    /// Release channel this dist was signed FOR ("stable"/"beta"/"dev") — part
    /// of the signed message, and checked against the directory it was loaded
    /// from so a dist can't be smuggled between channels by a file move.
    pub channel: String,
    pub size: u32,
    /// Hex sha256 of the raw image bytes.
    pub sha256: String,
    /// Hex Ed25519 signature over "scoreboard-fw:{version}:{size}:{channel}:"
    /// + the raw image bytes (verified on the board against its baked-in
    /// FW_PUBKEY_HEX; the server just relays it).
    pub sig: String,
}

/// A hosted firmware image: manifest + the bytes it describes. Only ever
/// constructed by `load_firmware_dist`, which refuses a manifest/image pair
/// that doesn't match — a served image always has the advertised size + hash.
#[derive(Clone, Debug)]
pub struct FirmwareDist {
    pub manifest: FirmwareManifest,
    /// The raw image; `Bytes` so every download is a cheap refcount clone.
    pub bytes: axum::body::Bytes,
}

/// Load the OTA dist from `dir` (firmware.json + firmware.bin).
/// - Nothing there at all -> Ok(None): the feature is quietly off.
/// - A dist that doesn't hold together (unreadable/unparseable manifest,
///   missing image, size or sha256 mismatch) -> Err: the caller must treat it
///   as a hard startup error rather than ever serving a broken image.
pub fn load_firmware_dist(
    dir: &std::path::Path,
    expected_channel: Option<&str>,
) -> Result<Option<FirmwareDist>, String> {
    let json_path = dir.join("firmware.json");
    let bin_path = dir.join("firmware.bin");
    if !json_path.exists() && !bin_path.exists() {
        return Ok(None); // no dist published — feature off
    }
    let json = std::fs::read_to_string(&json_path)
        .map_err(|e| format!("reading {}: {e}", json_path.display()))?;
    let manifest: FirmwareManifest =
        serde_json::from_str(&json).map_err(|e| format!("parsing {}: {e}", json_path.display()))?;
    let bytes = std::fs::read(&bin_path).map_err(|e| format!("reading {}: {e}", bin_path.display()))?;
    if bytes.len() != manifest.size as usize {
        return Err(format!(
            "{} is {} bytes but firmware.json says {}",
            bin_path.display(),
            bytes.len(),
            manifest.size
        ));
    }
    let sha = sha256_hex(&bytes);
    if sha != manifest.sha256 {
        return Err(format!(
            "sha256 mismatch: {} hashes to {sha} but firmware.json says {}",
            bin_path.display(),
            manifest.sha256
        ));
    }
    // The channel is inside the signed message, so a dist moved (or copied)
    // into another channel's directory would fail on-device anyway — but fail
    // it HERE, loudly, at startup rather than on a customer's wall.
    if let Some(expected) = expected_channel {
        if manifest.channel != expected {
            return Err(format!(
                "{} was signed for channel {:?} but is hosted as {expected:?} — dists cannot move between channels",
                json_path.display(),
                manifest.channel,
            ));
        }
    }
    Ok(Some(FirmwareDist { manifest, bytes: bytes.into() }))
}

/// Load every release channel's dist from `base` (FIRMWARE_DIR):
/// `{base}/stable`, `{base}/beta`, `{base}/dev`. Channels are independent —
/// an absent dir just leaves that channel off — but any channel that IS
/// present must hold together (`load_firmware_dist`'s checks), and flat
/// pre-channel files at `base` itself are refused rather than silently
/// unserved: both are hard startup errors.
pub fn load_firmware_dists(
    base: &std::path::Path,
) -> Result<HashMap<FwChannel, Arc<FirmwareDist>>, String> {
    if base.join("firmware.json").exists() || base.join("firmware.bin").exists() {
        return Err(format!(
            "{} holds a flat (pre-channel) dist — move firmware.bin/firmware.json into {}",
            base.display(),
            base.join("stable").display()
        ));
    }
    let mut dists = HashMap::new();
    for chan in FwChannel::ALL {
        let dir = base.join(chan.as_str());
        if let Some(dist) =
            load_firmware_dist(&dir, Some(chan.as_str())).map_err(|e| format!("{} channel: {e}", chan.as_str()))?
        {
            dists.insert(chan, Arc::new(dist));
        }
    }
    Ok(dists)
}

/// THE update-offer computation, shared verbatim by `board_state_msg` (which
/// lights the app's Update button) and `handle_update_firmware` (which acts on
/// the press) — one implementation so the two can never drift.
///
/// Among the hosted dists in the board's channel's allowed set (stable ⊆ beta
/// ⊆ dev — see `FwChannel::allowed`), pick the newest by CalVer, and offer it
/// only when strictly newer than the version the board reports running.
/// Returns the winning dist's OWN channel too: the download path is where the
/// dist lives, not what the board is enrolled in (a beta board whose best
/// offer is the stable image downloads /firmware/stable.bin).
pub fn offer_for<'a>(
    dists: &'a HashMap<FwChannel, Arc<FirmwareDist>>,
    channel: FwChannel,
    running: &str,
) -> Option<(FwChannel, &'a Arc<FirmwareDist>)> {
    channel
        .allowed()
        .iter()
        .filter_map(|c| dists.get(c).map(|fw| (*c, fw)))
        // Unparseable versions sort as None (< every Some) and are then thrown
        // out by `version_newer` anyway — junk can never win an offer. Ties
        // are impossible: release-firmware.sh refuses a version that already
        // exists in ANY channel.
        .max_by_key(|(_, fw)| parse_calver(&fw.manifest.version))
        .filter(|(_, fw)| version_newer(&fw.manifest.version, running))
}

/// A transient full-screen message for one board. Stored as text so each manifest
/// render can redraw it with a live progress bar (depleting toward `until`).
#[derive(Clone)]
pub struct Notice {
    pub lines: Vec<(String, bool, embedded_graphics::pixelcolor::Rgb888)>,
    pub created: std::time::Instant,
    pub until: std::time::Instant,
    /// Program identity for the manifest (see `Manifest::epoch`): minted fresh
    /// per showing, so the board restarts the sweep even when the step count
    /// matches the outgoing rotation's length — and so a RE-SET notice (second
    /// knob press mid-notice) yields a different manifest the dedup can't
    /// swallow, restarting the bar with the new expiry.
    pub epoch: u64,
    /// When the countdown ends, put the board to sleep (the NO GAMES notice
    /// SAYS "sleeping in 10 seconds" — this makes it true). Guarded by epoch:
    /// a notice that was replaced or cleared never sleeps the board late.
    pub then_sleep: bool,
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
            pair_limiters: Arc::new(Mutex::new(HashMap::new())),
            pair_limiter: Arc::new(Mutex::new(RateLimiter::new(
                PAIR_GLOBAL_BURST,
                PAIR_GLOBAL_REFILL_PER_SEC,
            ))),
            poll_wake: Arc::new(tokio::sync::Notify::new()),
            notices: Arc::new(Mutex::new(HashMap::new())),
            last_manifest: Arc::new(Mutex::new(HashMap::new())),
            frame_grace: Arc::new(Mutex::new(HashSet::new())),
            frame_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            finished: Arc::new(Mutex::new(HashMap::new())),
            firmware: HashMap::new(),
            device_versions: Arc::new(Mutex::new(HashMap::new())),
            fw_download_tokens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Cache a rendered frame, stamped with the current eviction generation.
    pub fn cache_frame(&self, id: FrameId, px: Vec<u8>) {
        let gen = self.frame_gen.load(std::sync::atomic::Ordering::Relaxed);
        self.frames.lock().unwrap().insert(id, (gen, px));
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

    /// Forget a pairing durably (mirrors `persist_pairing`). Without the DB
    /// delete a revoked grant came straight back on the next server start.
    fn forget_pairing(&self, board_id: &str, controller: &str) {
        let Some(db) = &self.db else { return };
        let _ = db.lock().unwrap().execute(
            "DELETE FROM pairings WHERE board_pubkey = ?1 AND controller_pubkey = ?2",
            rusqlite::params![board_id, controller],
        );
    }

    /// Revoke ONE controller's grant on a board, in memory and on disk. Returns
    /// whether that grant existed — unpairing a board you were never paired
    /// with is a no-op, not an error. The caller supplies the controller from
    /// the signed connection, so nobody can revoke someone else's phone.
    pub fn unpair(&self, board_id: &str, controller: &str) -> bool {
        let removed = {
            let mut allow = self.allow.lock().unwrap();
            match allow.get_mut(board_id) {
                Some(set) => {
                    let removed = set.remove(controller);
                    if set.is_empty() {
                        allow.remove(board_id);
                    }
                    removed
                }
                None => false,
            }
        };
        if removed {
            self.forget_pairing(board_id, controller);
        }
        removed
    }

    /// Publish a board's current pairing code. Codes expire (`PAIR_CODE_TTL`),
    /// swept lazily here and on every pair attempt so no timer task is needed;
    /// and a board's OLD codes die the moment it shows a new one — what's on
    /// the panel is the only code that works.
    pub fn register_pairing_code(&self, code: String, board_id: &str) {
        let now = Instant::now();
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, (bid, minted)| {
            now.duration_since(*minted) < PAIR_CODE_TTL && bid.as_str() != board_id
        });
        pending.insert(code, (board_id.to_string(), now));
    }

    /// Spend one pairing attempt for this controller. Per-identity budget first,
    /// and a rejected attempt never touches the global bucket — otherwise an
    /// attacker draining its own would still drain everyone else's.
    pub fn allow_pair_attempt(&self, controller: &str) -> bool {
        {
            let mut limiters = self.pair_limiters.lock().unwrap();
            // Buckets that have refilled to full are indistinguishable from
            // fresh ones, so drop them — the map can't grow with every pubkey
            // that ever guessed.
            limiters.retain(|k, l| k == controller || !l.is_full());
            let ok = limiters
                .entry(controller.to_string())
                .or_insert_with(|| RateLimiter::new(PAIR_BURST, PAIR_REFILL_PER_SEC))
                .allow();
            if !ok {
                return false;
            }
        }
        self.pair_limiter.lock().unwrap().allow()
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
    let board_id = {
        let now = Instant::now();
        let mut pending = state.pending.lock().unwrap();
        // Sweep before the lookup, so an expired code can never be redeemed
        // (and the map stays bounded without a background task).
        pending.retain(|_, (_, minted)| now.duration_since(*minted) < PAIR_CODE_TTL);
        pending.remove(code).map(|(board_id, _)| board_id)
    }
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

    // Pipeline debugger: interactive TUI, or headless `tui --feed X --dump`.
    if std::env::args().nth(1).as_deref() == Some("tui") {
        let args: Vec<String> = std::env::args().collect();
        tui::run(&args);
        return;
    }

    // Headless overflow report: for every league, each team's resolved name in
    // each available style, measured against the row width, plus an OVERFLOW
    // section for anything that still doesn't fit. The primary "notify me for
    // correction" tool (live renders also warn, deduped, on real truncation).
    if std::env::args().nth(1).as_deref() == Some("team-names") {
        team_names::run().await;
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
    let mut state = AppState::new_with_catalog(catalog);
    // OTA firmware dists, one per channel: an absent channel is quietly off;
    // any present-but-broken channel refuses startup (a board must never
    // download an image that won't verify).
    let fw_dir = PathBuf::from(std::env::var("FIRMWARE_DIR").unwrap_or_else(|_| "./firmware-dist".into()));
    state.firmware = match load_firmware_dists(&fw_dir) {
        Ok(dists) => {
            if dists.is_empty() {
                tracing::info!("no firmware dists in {} — OTA updates off", fw_dir.display());
            }
            for chan in FwChannel::ALL {
                if let Some(dist) = dists.get(&chan) {
                    tracing::info!(
                        "hosting {} firmware {} ({} bytes) from {}",
                        chan.as_str(),
                        dist.manifest.version,
                        dist.manifest.size,
                        fw_dir.join(chan.as_str()).display()
                    );
                }
            }
            dists
        }
        Err(e) => {
            eprintln!("error: firmware dist in {} is broken: {e}", fw_dir.display());
            std::process::exit(1);
        }
    };
    tokio::spawn(poll_loop(state.clone(), source));
    tokio::spawn(tick_loop(state.clone()));

    let app = Router::new()
        .route("/catalog", get(get_catalog))
        .route("/render/:id", get(get_render))
        .route("/game/:id", get(get_game))
        .route("/sample/:variant", get(get_sample))
        // Per-channel OTA routes: "/firmware/stable.bin" + "/firmware/stable/latest"
        // (and beta/dev). No un-channeled aliases — no OTA-capable board ever
        // shipped speaking the old flat paths.
        .route("/firmware/:chan/latest", get(get_firmware_latest))
        .route("/firmware/:file", get(get_firmware_bin))
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

/// GET /firmware/{stable|beta|dev}/latest — that channel's firmware.json
/// (version/size/sha256/sig). Plain-text 404 for an unknown channel name or a
/// channel with no dist hosted.
async fn get_firmware_latest(
    State(s): State<AppState>,
    axum::extract::Path(chan): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match FwChannel::from_name(&chan).and_then(|c| s.firmware.get(&c)) {
        Some(fw) => Json(fw.manifest.clone()).into_response(),
        None => (axum::http::StatusCode::NOT_FOUND, "no firmware hosted for that channel").into_response(),
    }
}

/// How long a minted OTA download token stays valid: long enough for a board
/// to receive the offer, reconnect its HTTP fetch, and stream the whole image;
/// swept lazily so a token an offer never used (or a download that never
/// finished) can't accumulate.
const FW_TOKEN_TTL: Duration = Duration::from_secs(600);

/// Image bytes handed to the board's fetch per stream step. TCP backpressure
/// paces `unfold` to the board's real consumption, so this is the granularity
/// at which server-observed download progress advances.
const FW_CHUNK: usize = 16 * 1024;

/// Query string of `GET /firmware/{chan}.bin`. `u` is the one-time download
/// token an offer's path carries (`?u=…`): present -> this is a board's real
/// OTA fetch, so stream it and push `FirmwareProgress`; absent -> a plain
/// download (curl, tests), served whole with no progress.
#[derive(serde::Deserialize)]
struct FwDownloadQuery {
    #[serde(default)]
    u: Option<String>,
}

/// Resolve a download token to the board_id it was minted for, sweeping any
/// expired tokens first. Does NOT remove the token — the stream drops it on
/// completion (an early-dropped download leaves it for the lazy sweep).
fn resolve_fw_token(state: &AppState, nonce: &str) -> Option<String> {
    let mut tokens = state.fw_download_tokens.lock().unwrap();
    let now = Instant::now();
    tokens.retain(|_, (_, created)| now.duration_since(*created) < FW_TOKEN_TTL);
    tokens.get(nonce).map(|(board_id, _)| board_id.clone())
}

/// A streamed image body that reports server-observed download progress. Emits
/// the SAME `bytes` in ~`FW_CHUNK` slices (byte-identical to the plain path —
/// the board verifies sha256 + Ed25519), and after each slice pushes
/// `FirmwareProgress{board_id, pct}` to that board's subscribed controllers
/// when pct advanced by ≥2 (or reached 100). When the stream drains it drops
/// the one-time token.
fn progress_stream_body(
    app: AppState,
    board_id: String,
    nonce: String,
    bytes: axum::body::Bytes,
) -> axum::body::Body {
    struct St {
        app: AppState,
        board_id: String,
        nonce: String,
        bytes: axum::body::Bytes,
        offset: usize,
        last_pct: u8,
    }
    // Single-use token cleanup rides on Drop, NOT on the stream being polled to
    // its terminal None: with an explicit Content-Length, hyper stops polling
    // once it has sent `len` bytes and never asks for the end-of-stream item.
    // Drop fires whether the body completes OR the board aborts mid-download
    // (connection closed) — either way the nonce is reclaimed promptly.
    impl Drop for St {
        fn drop(&mut self) {
            self.app.fw_download_tokens.lock().unwrap().remove(&self.nonce);
        }
    }
    let init = St { app, board_id, nonce, bytes, offset: 0, last_pct: 0 };
    let stream = futures_util::stream::unfold(init, |mut st| async move {
        let total = st.bytes.len();
        if st.offset >= total {
            return None; // St drops with the stream; its Drop reclaims the token
        }
        let end = (st.offset + FW_CHUNK).min(total);
        let chunk = st.bytes.slice(st.offset..end);
        st.offset = end;
        // Deliberately NO progress push here. Bytes handed to this socket are
        // swallowed by the kernel send buffer and Railway's TCP proxy, so they
        // reach 100% while the board is still writing flash — the app's bar
        // jumped straight to full. The DEVICE reports its own progress over
        // the websocket it now keeps open (ClientMsg::DeviceUpdateProgress);
        // the token still scopes the download to one board and expires it.
        Some((Ok::<_, std::convert::Infallible>(chunk), st))
    });
    axum::body::Body::from_stream(stream)
}

/// GET /firmware/{stable|beta|dev}.bin — that channel's raw image bytes, the
/// path `ServerMsg::UpdateFirmware` points boards at. Plain HTTP is fine: the
/// board verifies size + sha256 + Ed25519 signature itself, so the transport
/// needs no trust. Plain-text 404 for anything that isn't a hosted channel's
/// "<channel>.bin". A valid `?u=` token turns this into a progress-reporting
/// stream (see `progress_stream_body`); an absent/unknown/expired token serves
/// the full body exactly as before.
async fn get_firmware_bin(
    State(s): State<AppState>,
    axum::extract::Path(file): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<FwDownloadQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let chan = file.strip_suffix(".bin").and_then(FwChannel::from_name);
    let Some(fw) = chan.and_then(|c| s.firmware.get(&c)) else {
        return (axum::http::StatusCode::NOT_FOUND, "no firmware hosted for that channel").into_response();
    };
    // A token that resolves to a board makes this a monitored OTA fetch.
    match q.u.as_deref().and_then(|nonce| resolve_fw_token(&s, nonce).map(|b| (nonce.to_string(), b))) {
        Some((nonce, board_id)) => {
            let len = fw.bytes.len();
            let body = progress_stream_body(s.clone(), board_id, nonce, fw.bytes.clone());
            // CRITICAL: an unknown-length streamed body makes hyper frame the
            // response as `transfer-encoding: chunked` — but the board's bare
            // HTTP client (firmware-rs/src/ota.rs) does NOT decode chunking; it
            // treats every post-header byte as raw image, so the chunk-size
            // lines would be hashed into the image and every OTA would fail
            // sha256. An explicit Content-Length makes hyper use identity
            // framing (raw bytes, no chunking), byte-identical to the plain
            // branch below. (Tests that go through reqwest hide this — it
            // decodes chunked transparently — so there is a raw-bytes test.)
            (
                [
                    (axum::http::header::CONTENT_TYPE, "application/octet-stream".to_string()),
                    (axum::http::header::CONTENT_LENGTH, len.to_string()),
                ],
                body,
            )
                .into_response()
        }
        None => {
            let headers = [(axum::http::header::CONTENT_TYPE, "application/octet-stream")];
            (headers, fw.bytes.clone()).into_response()
        }
    }
}

/// A canned example frame for the app's spoiler-settings rows: the same game
/// rendered plain ("off"), spoiled-normal, or spoiled-strict, so the choice
/// shows exactly what the board will do. Variant is off|normal|strict.
async fn get_sample(
    axum::extract::Path(variant): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let variant = variant.strip_suffix(".png").unwrap_or(&variant).to_string();
    let team = |abbrev: &str, name: &str, score: u32, p: &str, sec: &str| model::TeamSide {
        abbrev: abbrev.into(),
        name: name.into(),
        // Sample spoiler frame renders NHL (default Name style), so location is
        // unused; left empty. (Render agent: populate to preview Location here.)
        location: String::new(),
        score,
        primary: model::hex_to_rgb(p),
        secondary: model::ensure_contrast(model::hex_to_rgb(p), model::hex_to_rgb(sec)),
    };
    let g = model::Game {
        id: "sample".into(),
        sport_id: "nhl".into(),
        away: team("CHI", "BLACKHAWKS", 3, "CF0A2C", "FFD100"),
        home: team("STL", "BLUES", 3, "002F87", "FCB514"),
        // Overtime, tied — exactly the state someone doesn't want spoiled.
        // Normal mode still shows "OT"; strict hides even that (LIVE NOW).
        status: GameStatus::Active,
        ordinal: "OT".into(),
        clock: "2:41".into(),
        start_time: "2026-07-08T23:00Z".into(),
        end_time: None,
        leaderboard: None,
        champ: None,
        session: None,
        situation: Some(model::Situation::Hockey {
            away_powerplay: false,
            home_powerplay: false,
            away_players: 3,
            home_players: 3,
        }),
        pregame: None,
    };
    let spoiler = match variant.as_str() {
        "normal" => Some(SpoilerMode::Normal),
        "strict" => Some(SpoilerMode::Strict),
        _ => None,
    };
    let (_, px) = render::render_frame(&g, spoiler, None, default_name_style(&g.sport_id));
    (
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        render_png(&px),
    )
        .into_response()
}

/// Render a game to a PNG — the app's "actual rendered content" cards fetch this.
/// `id` may carry a trailing `.png`. 64x32 source; the app scales it up crisply.
async fn get_render(
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
    State(s): State<AppState>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let id = id.strip_suffix(".png").unwrap_or(&id);
    // `?board=` scopes the render to that board's spoiler rules — without it a
    // protected game's card image would leak the score around the sanitized
    // summaries and frames.
    let cfg = q.get("board").map(|b| s.config_for(b));
    let px = {
        let games = s.games.lock().unwrap();
        games.iter().find(|g| g.id == id).map(|g| {
            let spoiler = cfg.as_ref().and_then(|c| spoiler_for(c, g));
            let tz = cfg.as_ref().and_then(board_tz);
            let style = cfg
                .as_ref()
                .map(|c| name_style_for(c, &g.sport_id))
                .unwrap_or_else(|| default_name_style(&g.sport_id));
            render::render_frame(g, spoiler, tz, style).1
        })
    };
    tracing::info!(
        "render {id} board={} -> {}",
        q.get("board").map(String::as_str).unwrap_or("-"),
        if px.is_some() { "ok" } else { "UNKNOWN GAME" }
    );
    // Unknown ids get a VISIBLE error frame, not a 404: the app's image
    // failure state is a black rectangle — indistinguishable from a black
    // frame — which made a stale-id bug look like a rendering bug.
    let rgb565 = px.unwrap_or_else(|| {
        render::render_notice(
            &[("NO GAME", true, render::YELLOW_PUB), (id, false, render::GREY_PUB)],
            None,
        )
        .1
    });
    (
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        render_png(&rgb565),
    )
        .into_response()
}

/// The full processed `Game` as pretty JSON — the raw-data companion to
/// `get_render`'s PNG. `?board=` scopes the response to that board's spoiler
/// rules exactly like `get_render` does: without it a protected game's raw
/// data would leak the score around the sanitized summaries and frames.
/// Protected games are served REDACTED (see `redact_game`) with a visible
/// `"spoiled": true`; unknown ids are a plain-text 404 (unlike the render
/// route there's no image-failure ambiguity to work around here).
async fn get_game(
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
    State(s): State<AppState>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let cfg = q.get("board").map(|b| s.config_for(b));
    let game = s.games.lock().unwrap().iter().find(|g| g.id == id).cloned();
    let Some(game) = game else {
        tracing::info!("game {id} board={} -> UNKNOWN GAME", q.get("board").map(String::as_str).unwrap_or("-"));
        return (axum::http::StatusCode::NOT_FOUND, format!("no game '{id}'")).into_response();
    };
    // `spoiler_for` already honors the one-off reveal list, so a revealed
    // game passes through unredacted like everywhere else.
    let spoiler = cfg.as_ref().and_then(|c| spoiler_for(c, &game));
    let (game, spoiled) = match spoiler {
        Some(mode) => (redact_game(&game, mode), true),
        None => (game, false),
    };
    let mut v = serde_json::to_value(&game).expect("game serializes");
    if spoiled {
        // Flag the redaction in the payload itself, like GameSummary::spoiled.
        v.as_object_mut()
            .expect("game serializes to an object")
            .insert("spoiled".into(), true.into());
    }
    tracing::info!(
        "game {id} board={} -> {}",
        q.get("board").map(String::as_str).unwrap_or("-"),
        if spoiled { "spoiled" } else { "ok" }
    );
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::to_string_pretty(&v).expect("game serializes"),
    )
        .into_response()
}

/// A spoiler-safe clone of a game for the raw-data endpoint — as strict as
/// `to_summary_for` and `render_spoiled` (the fields those sanitize are the
/// fields cleared here): scores zeroed, live situation dropped, clock
/// emptied, leaderboard rows emptied (the matchup / event name is not a
/// spoiler). Normal mode keeps the ordinal as sanctioned basic progress;
/// Strict hides even that.
pub(crate) fn redact_game(g: &Game, mode: SpoilerMode) -> Game {
    let mut r = g.clone();
    r.away.score = 0;
    r.home.score = 0;
    r.clock = String::new();
    r.situation = None;
    if let Some(lb) = &mut r.leaderboard {
        lb.rows.clear();
    }
    // Normal mode may show progress (period/quarter/lap) — but only while the
    // game is IN progress. A Final's lingering ordinal ("OT", "10th") is a
    // result: it says the game went past regulation, which spoiler_detail and
    // render_spoiled both deliberately collapse to a bare FINAL.
    if mode == SpoilerMode::Strict || g.status == GameStatus::Final {
        r.ordinal = String::new();
    }
    r
}

/// Board-scoped summary: spoiler protection zeroes the scores and swaps the
/// detail for spoiler-safe status, so protected results can't leak through
/// the app's data model even if the UI mishandled them.
pub(crate) fn to_summary_for(g: &Game, spoiler: Option<SpoilerMode>, tz: Option<chrono_tz::Tz>) -> GameSummary {
    let mut s = to_summary(g, tz);
    if let Some(mode) = spoiler {
        s.away_score = 0;
        s.home_score = 0;
        s.detail = spoiler_detail(g, mode, tz);
        s.spoiled = true;
    }
    s
}

pub(crate) fn to_summary(g: &Game, tz: Option<chrono_tz::Tz>) -> GameSummary {
    // Scheduled games: the useful detail is WHEN they start ("7:05 PM"), for
    // both team games and leaderboard events.
    let pregame_detail = || {
        (g.status == GameStatus::Pregame)
            .then(|| {
                if starts_on_a_day(&g.sport_id) {
                    start_day_label(&g.start_time)
                } else {
                    start_time_label(&g.start_time, tz)
                }
            })
            .flatten()
    };
    // Normalized machine-readable timestamps (RFC3339 UTC) alongside the
    // human `detail` label. Times are NOT spoilers — even strict mode shows
    // the matchup + time — so the spoiler pass in `to_summary_for` leaves
    // these untouched.
    let start_time = parse_espn_time(&g.start_time).map(rfc3339_utc);
    let end_time = g.end_time.as_deref().and_then(parse_espn_time).map(rfc3339_utc);
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
            start_time,
            end_time,
            frame_id: String::new(),
            bumped: false,
            champ: g.champ.as_ref().map(|c| c.label.clone()),
            spoiled: false,
            scrolls: false,
            // Leaderboard sports (golf/F1) have no per-game ESPN page.
            espn_url: None,
            // Board-scoped; the browse path fills it in.
            queued: false,
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
        start_time,
        end_time,
        frame_id: String::new(),
        bumped: false,
        champ: g.champ.as_ref().map(|c| c.label.clone()),
        spoiled: false,
        scrolls: false,
        espn_url: crate::espn::espn_game_url(&g.sport_id, &g.id),
        // Board-scoped; the browse path fills it in.
        queued: false,
    }
}

/// Would this game's BOARD rendering scroll (before any per-game opt-out)?
/// Mirrors `render_board_frame`'s `deep` predicate exactly — including the
/// pregame and spoiler outs that force a static frame regardless of depth —
/// so the app never offers a scroll toggle on a game that can't scroll.
/// Membership in `no_scroll_game_ids` travels separately (in the config the
/// app already receives) and picks the toggle's label.
pub(crate) fn game_scrolls(cfg: &BoardConfig, g: &Game) -> bool {
    if g.status == GameStatus::Pregame || spoiler_for(cfg, g).is_some() {
        return false;
    }
    let deep_field = g.leaderboard.as_ref().is_some_and(|lb| lb.rows.len() > 4);
    let rows = cfg
        .sports
        .iter()
        .find(|s| s.sport_id == g.sport_id)
        .and_then(|s| s.leaderboard_rows)
        .map(|r| r as usize)
        .unwrap_or(render::LEADERBOARD_DEFAULT_ROWS);
    deep_field && rows > 4
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
/// was live in the old slate and is Final in the new one gets an end stamp.
/// Games that arrive already-Final (server restart, day-old finals in the feed)
/// are never stamped — only an observed transition starts a linger.
///
/// `prev_poll` is when we last saw the slate we're comparing against, and it —
/// NOT `now` — is the stamp: the linger means "we watched it end", so it can
/// never be fresher than the observation gap. The poll loop skips fetching
/// while nothing is connected, so the old slate can be arbitrarily stale: a
/// board connecting on Monday compared Monday's feed against Sunday's frozen
/// slate and re-lingered a tournament that had ended two days earlier. Stamped
/// at the earliest moment the game could have ended, that transition is already
/// expired and the game is dropped, while a normal 3-minute gap still lingers
/// its ~27 remaining minutes.
pub(crate) fn record_finished_transitions(state: &AppState, incoming: &[Game], prev_poll: Instant) {
    let was_live: HashSet<String> = state
        .games
        .lock()
        .unwrap()
        .iter()
        .filter(|g| is_live(g))
        .map(|g| g.id.clone())
        .collect();
    let now_utc = chrono::Utc::now();
    let mut fin = state.finished.lock().unwrap();
    for g in incoming {
        if g.status == GameStatus::Final && was_live.contains(&g.id) {
            // When did it ACTUALLY end? The feed knows for multi-day events
            // (golf/F1 carry endDate) — use that. Otherwise the best evidence
            // is the previous poll: the game was still live then, so it ended
            // somewhere in that gap, and the gap's start is the conservative
            // choice. Never `now`: a slate frozen while nothing was connected
            // would otherwise re-linger a tournament that ended days ago.
            let ended = g
                .end_time
                .as_deref()
                .and_then(parse_espn_time)
                .and_then(|end| {
                    // Feed time -> our monotonic clock, only for a PAST end
                    // (a future/clock-skewed endDate falls back to prev_poll).
                    let ago = now_utc.signed_duration_since(end).to_std().ok()?;
                    Instant::now().checked_sub(ago)
                })
                .unwrap_or(prev_poll);
            fin.entry(g.id.clone()).or_insert(ended);
            let left = FINAL_LINGER.saturating_sub(ended.elapsed());
            tracing::info!("game '{}' ended; lingering {}m", g.id, left.as_secs() / 60);
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

/// Canonical RFC3339 UTC text ("2026-06-28T13:00:00Z") for app-facing
/// timestamps — one normalized shape regardless of the feed's quirks.
pub(crate) fn rfc3339_utc(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The board's configured IANA timezone, parsed. `None` (unset or unparseable)
/// falls back to the server's local zone at each format site, so a local dev
/// server keeps rendering sensible times without any configuration.
pub(crate) fn board_tz(cfg: &BoardConfig) -> Option<chrono_tz::Tz> {
    cfg.timezone.as_deref().and_then(|s| s.parse().ok())
}

/// The resolved team-name display style for `sport_id` on this board: the
/// league's `SportConfig.name_style` override, else the league default
/// ([`default_name_style`]). Threaded into the renderer's `draw_team_box`.
pub(crate) fn name_style_for(cfg: &BoardConfig, sport_id: &str) -> NameStyle {
    cfg.sports
        .iter()
        .find(|s| s.sport_id == sport_id)
        .and_then(|s| s.name_style)
        .unwrap_or_else(|| default_name_style(sport_id))
}

/// The game's board-local start time as frame/caption text ("7:05 PM"), if
/// parseable. `tz` is the BOARD's zone; `None` = the server's local zone.
pub(crate) fn start_time_label(iso: &str, tz: Option<chrono_tz::Tz>) -> Option<String> {
    let t = parse_espn_time(iso)?;
    const FMT: &str = "%-I:%M %p";
    Some(match tz {
        Some(tz) => t.with_timezone(&tz).format(FMT).to_string(),
        None => t.with_timezone(&chrono::Local).format(FMT).to_string(),
    })
}

/// Multi-day events (golf tournaments) carry a DATE, not a tee time: ESPN
/// stamps them at midnight Eastern of the opening day, so the Rocket Classic
/// that tees off Thursday 6am reads as `2026-07-30T04:00Z`. Rendering that as
/// a clock time is wrong twice — the number is a boundary nobody plays at, and
/// west of Eastern it lands on the PREVIOUS day ("11:00 PM tonight" for a
/// Thursday-morning start). So show the day instead, resolved in Eastern where
/// the boundary was drawn.
pub(crate) fn start_day_label(iso: &str) -> Option<String> {
    let t = parse_espn_time(iso)?;
    let et = t.with_timezone(&chrono_tz::America::New_York);
    let today = chrono::Utc::now().with_timezone(&chrono_tz::America::New_York).date_naive();
    let days = (et.date_naive() - today).num_days();
    Some(match days {
        0 => "TODAY".to_string(),
        1 => "TOMORROW".to_string(),
        // Inside the week a weekday reads instantly; past that, a date does.
        2..=6 => et.format("%a").to_string().to_uppercase(),
        _ => et.format("%b %-d").to_string().to_uppercase(),
    })
}

/// Does this game's start timestamp mean a DAY rather than a moment? True for
/// golf, whose feed dates are midnight-Eastern boundaries (see
/// `start_day_label`). F1 sessions carry real session times, so they don't.
pub(crate) fn starts_on_a_day(sport_id: &str) -> bool {
    matches!(sport_id, "pga" | "lpga")
}

/// How far ahead a scheduled game may start and still enter `state.games`.
/// 24h ≈ "starting today" without timezone hair: tonight and tomorrow morning
/// stay; next week's slate never enters state at all.
const INGEST_HORIZON: chrono::Duration = chrono::Duration::hours(24);

/// Ingestion gate: should a fetched game enter `state.games` at all? Anything
/// NOT Pregame is kept — live and intermission obviously, Finals because the
/// linger rules own their exit, and multi-day golf events whose (past) start
/// date no longer matters once play began. A Pregame game must start within
/// `INGEST_HORIZON`; an UNPARSEABLE start time is kept, so a feed quirk can
/// never blank a league.
pub(crate) fn ingest_keeps(g: &Game, now: chrono::DateTime<chrono::Utc>) -> bool {
    if g.status != GameStatus::Pregame {
        return true;
    }
    match parse_espn_time(&g.start_time) {
        Some(start) => start - now <= INGEST_HORIZON,
        None => true,
    }
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
/// Time-only — the state-aware [`browse_shows_now`] is what the app is served.
pub(crate) fn browse_shows(g: &Game) -> bool {
    is_live(g) || g.status == GameStatus::Pregame
}

/// The browse feed the app actually gets: the upcoming/live window above, plus
/// anything the board is showing right now. Whatever is on the panel MUST be
/// listable — a lingering Final showed up on the board and in "Now showing"
/// but was missing from View All, so the user could see a game they couldn't
/// find or remove.
pub(crate) fn browse_shows_now(state: &AppState, g: &Game) -> bool {
    browse_shows(g) || board_shows(state, g)
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

/// A board's selection rules, derived once and reused per game — resolving the
/// favorites set for every game in a 300-game slate is pure waste.
pub(crate) struct Prefs {
    favs: HashSet<String>,
    /// Nothing chosen at all: the board samples the whole live slate, so every
    /// game "qualifies" and nothing was ever lost to the cap.
    unconfigured: bool,
}

pub(crate) fn board_prefs(cfg: &BoardConfig) -> Prefs {
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
    Prefs { favs, unconfigured }
}

/// Will this game join the rotation once it starts? True when the board's
/// preferences already claim it but it isn't on the panel yet — which is
/// exactly "upcoming, and coming to your scoreboard". Anything already showing
/// (live, or a pregame inside the lead-in window) is not queued: it's HERE.
pub(crate) fn game_queued(state: &AppState, cfg: &BoardConfig, prefs: &Prefs, g: &Game) -> bool {
    !board_shows(state, g) && game_priority(g, cfg, &prefs.favs, prefs.unconfigured).is_some()
}

/// All games this board WANTS, split into (shown, bumped): priority-ordered, the
/// first `ROTATION_CAP` get rotation slots; the rest are reported to the app as
/// bumped so the user can promote them (manual adds outrank everything).
/// Unconfigured boards sample the slate without reporting bumps — nothing there
/// was chosen, so nothing was "lost".
fn candidates_with_bumped(state: &AppState, cfg: &BoardConfig) -> (Vec<Game>, Vec<Game>) {
    let games = state.games.lock().unwrap();
    let Prefs { favs, unconfigured } = board_prefs(cfg);

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
    let tz = board_tz(&cfg);

    // Priority 1: live games (only while powered on).
    if cfg.power {
        let chosen = chosen_games(state, &cfg);
        if !chosen.is_empty() {
            // Games override any lingering notice ("no games" message ends early).
            state.notices.lock().unwrap().remove(board_id);
            let base_dwell_ms = cfg.dwell_secs.clamp(3, 60) * 1000;
            let mut rotation = Vec::new();
            for g in &chosen {
                let league = cfg.sports.iter().find(|s| s.sport_id == g.sport_id);
                // Per-league "time per event" override, else the board default
                // — the scroll pass syncs to whichever applies, so a 30s F1
                // override buys a slower, more readable leaderboard glide.
                let dwell_ms = league
                    .and_then(|s| s.dwell_secs)
                    .map(|s| s.clamp(3, 60) * 1000)
                    .unwrap_or(base_dwell_ms);
                // Leaderboard depth (golf/F1): 4 = static top-4, no scrolling;
                // up to the full field. Deep frames render TALL and the board
                // scrolls a 32px window over them, one full pass per dwell
                // (app cards keep the static 4-row frame). A per-game scroll
                // opt-out overrides the league depth for THIS game only —
                // rows_cap = 4 IS the static layout.
                let rows_cap = if cfg.no_scroll_game_ids.contains(&g.id) {
                    4
                } else {
                    league
                        .and_then(|s| s.leaderboard_rows)
                        .map(|r| r as usize)
                        .unwrap_or(render::LEADERBOARD_DEFAULT_ROWS)
                };
                let style = name_style_for(&cfg, &g.sport_id);
                let (id, px, scrolls) = render::render_board_frame(g, rows_cap, spoiler_for(&cfg, g), tz, style);
                state.cache_frame(id.clone(), px);
                rotation.push(RotationEntry { frame_id: id, dwell_ms, scroll: scrolls });
            }
            return Manifest {
                rotation,
                display_on: true,
                off_state: cfg.off_state,
                brightness: cfg.brightness,
                epoch: 0,
            };
        }
    }

    // Priority 2: a transient notice ("SLEEPING…" splash / "NO GAMES…"). The whole
    // progress-bar sweep is pre-rendered as ONE rotation the board steps through on
    // its own clock — N frames at descending bar fractions, each dwelling secs/N —
    // so no network push rides any step (the old per-step repush loop made the
    // sleep splash visibly stutter). The fractions derive from created/until, not
    // "now", so a mid-notice re-render yields an IDENTICAL manifest and the dedup
    // in `push_board` keeps the board's rotation position.
    {
        let mut notices = state.notices.lock().unwrap();
        if let Some(n) = notices.get(board_id) {
            if std::time::Instant::now() < n.until {
                let lines: Vec<(&str, bool, embedded_graphics::pixelcolor::Rgb888)> =
                    n.lines.iter().map(|(s, b, c)| (s.as_str(), *b, *c)).collect();
                let secs = n.until.duration_since(n.created).as_secs().max(1);
                // The board caches every rotation frame in RAM (~4KB each), so
                // cap the sweep at 12 steps; short notices step once a second.
                // (FRAME_WIDTH bounds it too: finer steps than bar pixels can't
                // render distinctly.)
                let steps = secs.min(12).min(FRAME_WIDTH as u64) as usize;
                let step_ms = (secs * 1000 / steps as u64) as u32;
                let rotation = (0..steps)
                    .map(|i| {
                        // Bar fraction at the START of step i: full, then evenly
                        // down to 1/steps — every width distinct, so each step
                        // mints its own frame id.
                        let frac = (steps - i) as f32 / steps as f32;
                        let (id, px) = render::render_notice(&lines, Some(frac));
                        state.cache_frame(id.clone(), px);
                        // After the last entry the board would WRAP back to the
                        // full bar — a jump backward no fraction ordering can
                        // hide. Instead the last entry dwells 2s past expiry:
                        // the expiry task's push swaps in the post-notice view
                        // before the wrap can ever show.
                        let dwell_ms = if i + 1 == steps { step_ms + 2000 } else { step_ms };
                        RotationEntry { frame_id: id, dwell_ms, scroll: false }
                    })
                    .collect();
                return Manifest {
                    rotation,
                    display_on: true,
                    off_state: cfg.off_state,
                    brightness: cfg.brightness,
                    epoch: n.epoch,
                };
            }
            notices.remove(board_id);
        }
    }

    // Priority 3: idle / asleep — the configured off-state (server-rendered clock
    // or a blank panel). Applies both to power-off and powered-on-but-no-games.
    let rotation = if cfg.off_state == OffState::Clock {
        use chrono::Timelike;
        // Board-local wall time: the configured IANA zone, else the server's
        // local zone (which on Railway is UTC — hence per-board timezones).
        let (hour, minute) = match tz {
            Some(tz) => {
                let now = chrono::Utc::now().with_timezone(&tz);
                (now.hour(), now.minute())
            }
            None => {
                let now = chrono::Local::now();
                (now.hour(), now.minute())
            }
        };
        let (id, px) = render::render_clock(hour, minute);
        state.cache_frame(id.clone(), px);
        vec![RotationEntry { frame_id: id, dwell_ms: 60_000, scroll: false }]
    } else {
        vec![]
    };
    Manifest {
        display_on: !rotation.is_empty(),
        rotation,
        off_state: cfg.off_state,
        brightness: cfg.brightness,
        epoch: 0,
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
const NO_GAMES_SECS: u64 = 10;

/// Show a transient full-screen notice on one board. The manifest render
/// pre-bakes the WHOLE progress-bar sweep as one dwell-timed rotation (see
/// `manifest_for_board`), so the board advances the bar on its own clock with
/// zero network traffic per step. One task remains, solely to clear the notice
/// at expiry — its push re-renders, sees the notice lapsed, and swaps in the
/// post-notice view.
pub fn set_notice(state: &AppState, board_id: &str, lines: &[(&str, bool, embedded_graphics::pixelcolor::Rgb888)], secs: u64, then_sleep: bool) {
    // Nonzero and unique per showing (0 = the games rotation).
    static NOTICE_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let now = std::time::Instant::now();
    let until = now + Duration::from_secs(secs);
    let epoch = NOTICE_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    state.notices.lock().unwrap().insert(
        board_id.to_string(),
        Notice {
            lines: lines.iter().map(|(s, b, c)| (s.to_string(), *b, *c)).collect(),
            created: now,
            until,
            epoch,
            then_sleep,
        },
    );
    // (No-op outside a tokio runtime, i.e. in sync tests — the tick loop covers it.)
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let (state, board_id) = (state.clone(), board_id.to_string());
        handle.spawn(async move {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            finish_notice(&state, &board_id, epoch);
        });
    }
}

/// Notice countdown expiry: if OUR notice (by epoch) is still the active one,
/// retire it — and if it promised sleep ("NO GAMES / SLEEPING IN…"), actually
/// put the board to sleep with the standard timed wake. A replaced or already
/// -cleared notice does nothing: a newer showing owns the screen, and a game
/// that appeared mid-countdown (priority 1 clears the notice) must not have
/// the panel yanked dark 10 seconds later.
pub(crate) fn finish_notice(state: &AppState, board_id: &str, epoch: u64) {
    let mine = {
        let mut notices = state.notices.lock().unwrap();
        match notices.get(board_id) {
            Some(n) if n.epoch == epoch => {
                let sleep = n.then_sleep;
                notices.remove(board_id);
                Some(sleep)
            }
            _ => None,
        }
    };
    match mine {
        Some(true) => {
            let mut cfg = state.config_for(board_id);
            cfg.power = false;
            cfg.wake_at = Some(unix_now() + cfg.sleep_hours.max(1) as u64 * 3600);
            state.configs.lock().unwrap().insert(board_id.to_string(), cfg.clone());
            state.persist_config(board_id, &cfg);
            tracing::info!("board '{board_id}': no-games countdown lapsed -> sleep");
            push_board(state, board_id);
            push_board_state(state, board_id);
        }
        Some(false) => push_board(state, board_id),
        // Replaced/cleared: the newer showing's own expiry task pushes.
        None => {}
    }
}

/// A single knob press, reported by the board. The server owns what it means:
/// - asleep (power off) -> wake; if nothing is live, show the "no games" notice
/// - showing games      -> sleep for `sleep_hours` (with a brief splash saying so)
/// - idle (no games)    -> show the "no games" notice for 5 minutes
pub fn handle_button(state: &AppState, board_id: &str) {
    let mut cfg = state.config_for(board_id);
    let has_games = !candidate_games(state, &cfg).is_empty();

    // A press DURING the no-games countdown means "yes, sleep now" — don't
    // make the user wait out the bar they were just shown.
    let counting_down = state
        .notices
        .lock()
        .unwrap()
        .get(board_id)
        .is_some_and(|n| n.then_sleep && std::time::Instant::now() < n.until);
    if counting_down {
        state.notices.lock().unwrap().remove(board_id);
        cfg.power = false;
        cfg.wake_at = Some(unix_now() + cfg.sleep_hours.max(1) as u64 * 3600);
        state.configs.lock().unwrap().insert(board_id.to_string(), cfg.clone());
        state.persist_config(board_id, &cfg);
        tracing::info!("board '{board_id}': button skipped the no-games countdown -> sleep");
        push_board(state, board_id);
        push_board_state(state, board_id);
        return;
    }

    if !cfg.power {
        // Wake up.
        cfg.power = true;
        cfg.wake_at = None;
        if !has_games {
            set_notice(state, board_id, &[("NO GAMES", true, NOTICE_WHITE), ("SLEEPING IN", false, NOTICE_GREY), ("10 SECONDS", false, NOTICE_GREY)], NO_GAMES_SECS, true);
        }
        tracing::info!("board '{board_id}': button -> wake");
    } else if has_games {
        // Sleep for the configured duration.
        let hours = cfg.sleep_hours.max(1);
        cfg.power = false;
        cfg.wake_at = Some(unix_now() + hours as u64 * 3600);
        let caption = if hours == 1 { "FOR 1 HOUR".to_string() } else { format!("FOR {hours} HOURS") };
        set_notice(state, board_id, &[("SLEEPING", true, NOTICE_WHITE), (&caption, false, NOTICE_GREY)], SPLASH_SECS, false);
        tracing::info!("board '{board_id}': button -> sleep {hours}h");
    } else {
        // Powered on but idle: show why the panel is dark, then re-sleep.
        set_notice(state, board_id, &[("NO GAMES", true, NOTICE_WHITE), ("SLEEPING IN", false, NOTICE_GREY), ("10 SECONDS", false, NOTICE_GREY)], NO_GAMES_SECS, true);
        tracing::info!("board '{board_id}': button -> no-games notice");
    }

    state.configs.lock().unwrap().insert(board_id.to_string(), cfg.clone());
    state.persist_config(board_id, &cfg);
    push_board(state, board_id);
    push_board_state(state, board_id);
}

/// Whether a board spoiler-protects this game, and in which display mode.
/// Precedence: a one-off REVEAL beats everything; then any protection rule
/// (one-off hide, protect-all, league, team) engages the board's mode.
pub fn spoiler_for(cfg: &BoardConfig, g: &Game) -> Option<SpoilerMode> {
    if cfg.revealed_game_ids.contains(&g.id) {
        return None;
    }
    let league = cfg.sports.iter().find(|s| s.sport_id == g.sport_id);
    let team_hit = league.is_some_and(|s| {
        s.spoiler_team_ids.iter().any(|tid| {
            *tid == format!("{}-{}", g.sport_id, g.away.abbrev.to_lowercase())
                || *tid == format!("{}-{}", g.sport_id, g.home.abbrev.to_lowercase())
        })
    });
    let protected = cfg.spoiler_game_ids.contains(&g.id)
        || cfg.spoiler_all
        || league.is_some_and(|s| s.spoiler)
        || team_hit;
    protected.then_some(cfg.spoiler_mode)
}

/// The spoiler-safe replacement for a summary's `detail` line.
fn spoiler_detail(g: &Game, mode: SpoilerMode, tz: Option<chrono_tz::Tz>) -> String {
    match g.status {
        GameStatus::Final => "FINAL".into(),
        GameStatus::Pregame => start_time_label(&g.start_time, tz).unwrap_or_default(),
        _ => match mode {
            SpoilerMode::Strict => start_time_label(&g.start_time, tz)
                .map(|t| format!("Started {t}"))
                .unwrap_or_else(|| "LIVE".into()),
            SpoilerMode::Normal => {
                // Basic progress only: period/quarter for team games, the lap
                // count for F1 (never names/gaps), plain LIVE otherwise.
                if let Some(lb) = &g.leaderboard {
                    lb.rows
                        .first()
                        .filter(|r| r.value.starts_with("LAP"))
                        .map(|r| r.value.clone())
                        .unwrap_or_else(|| "LIVE".into())
                } else if g.ordinal.is_empty() {
                    "LIVE".into()
                } else {
                    g.ordinal.clone()
                }
            }
        },
    }
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

/// The board reported its self-discovered IANA timezone (IP-geolocated at
/// boot) — adopt it for this board's clock and start-time labels. The zone
/// tracks where the BOARD lives, needs no phone, and self-corrects on the
/// next boot after a move; DST is automatic because the NAME is stored and
/// the tz database supplies the offset. Unparseable names are ignored.
pub fn handle_device_timezone(state: &AppState, board_id: &str, tz: &str) {
    if tz.parse::<chrono_tz::Tz>().is_err() {
        tracing::warn!("board '{board_id}' reported unparseable timezone '{tz}' — ignored");
        return;
    }
    let mut cfg = state.config_for(board_id);
    if cfg.timezone.as_deref() == Some(tz) {
        return;
    }
    tracing::info!("board '{board_id}' timezone -> {tz}");
    cfg.timezone = Some(tz.to_string());
    state.configs.lock().unwrap().insert(board_id.to_string(), cfg.clone());
    state.persist_config(board_id, &cfg);
    push_board(state, board_id);
    push_board_state(state, board_id);
}

/// The board reported its running firmware version (once, after auth). Record
/// it and — only when that changes what apps would see — re-push board state,
/// which carries `firmware_version` and the update offer computed from it.
pub fn handle_device_version(state: &AppState, board_id: &str, version: String) {
    let changed = {
        let mut versions = state.device_versions.lock().unwrap();
        if versions.get(board_id) == Some(&version) {
            false
        } else {
            tracing::info!("board '{board_id}' runs firmware {version}");
            versions.insert(board_id.to_string(), version);
            true
        }
    };
    if changed {
        push_board_state(state, board_id);
    }
}

/// Mint a fresh one-time download token for `board_id` and return the
/// tokenized image path for a dist that lives in `chan`
/// (`/firmware/{chan}.bin?u={nonce}`). Sweeps expired tokens as it inserts.
/// The token lets the streaming GET route push `FirmwareProgress` to the
/// board's subscribed controllers; it is NOT part of the signed message.
fn mint_fw_offer_path(state: &AppState, board_id: &str, chan: FwChannel) -> String {
    let mut tokens = state.fw_download_tokens.lock().unwrap();
    let now = Instant::now();
    tokens.retain(|_, (_, created)| now.duration_since(*created) < FW_TOKEN_TTL);
    // 16 hex chars of RNG — collision-free in practice for the handful of
    // concurrent OTAs a server ever runs, and unguessable enough that a token
    // can't be forged (it only gates progress reporting, never the bytes).
    let nonce = format!("{:016x}", rand::random::<u64>());
    tokens.insert(nonce.clone(), (board_id.to_string(), now));
    format!("/firmware/{}.bin?u={}", chan.as_str(), nonce)
}

/// Build the `ServerMsg::UpdateFirmware` offer for `fw` (which lives in
/// `chan`), targeting `board_id`: version/size/sha256/sig come straight from
/// the dist's manifest; the download path is freshly tokenized. Shared by the
/// newest-allowed `UpdateFirmware` flow and the channel-targeted
/// `InstallFirmware` flow so the two never drift.
fn build_fw_offer(state: &AppState, board_id: &str, chan: FwChannel, fw: &FirmwareDist) -> ServerMsg {
    ServerMsg::UpdateFirmware {
        version: fw.manifest.version.clone(),
        size: fw.manifest.size,
        sha256: fw.manifest.sha256.clone(),
        sig: fw.manifest.sig.clone(),
        path: mint_fw_offer_path(state, board_id, chan),
        channel: chan.as_str().to_string(),
    }
}

/// The app pressed Update. Re-run the SAME pick `board_state_msg` advertised
/// (`offer_for`: the board's channel, its reported version, the hosted dists)
/// and relay that dist to the board's device socket as
/// `ServerMsg::UpdateFirmware` — the board downloads `path` from us and
/// verifies the image against the other fields. No offerable dist, no reported
/// version, or an offline board is a logged no-op (nothing to send, or nobody
/// to send it to).
pub fn handle_update_firmware(state: &AppState, board_id: &str) {
    let channel = state.config_for(board_id).fw_channel;
    let Some(running) = state.device_versions.lock().unwrap().get(board_id).cloned() else {
        tracing::info!("update requested for '{board_id}' but it has not reported a version — ignored");
        return;
    };
    let Some((chan, fw)) = offer_for(&state.firmware, channel, &running) else {
        tracing::info!(
            "update requested for '{board_id}' ({} channel, running {running}) but nothing newer is hosted — ignored",
            channel.as_str()
        );
        return;
    };
    let version = fw.manifest.version.clone();
    let msg = build_fw_offer(state, board_id, chan, fw);
    match state.devices.lock().unwrap().get(board_id) {
        Some(tx) => {
            let _ = tx.send(Message::Text(serde_json::to_string(&msg).unwrap()));
            tracing::info!("offered {} firmware {version} to board '{board_id}'", chan.as_str());
        }
        None => tracing::info!("update requested for offline board '{board_id}' — ignored"),
    }
}

/// The DEBUG picker asked to install a SPECIFIC channel's current dist
/// (`Command::InstallFirmware`). Relay that channel's dist to the board's
/// device socket as a `ServerMsg::UpdateFirmware` offer regardless of version —
/// the on-device same-or-newer guard decides whether it takes, so a same/older
/// dist still SENDS (a deliberate reinstall). No dist hosted for that channel,
/// or an offline board, is a logged no-op.
pub fn handle_install_firmware(state: &AppState, board_id: &str, channel: FwChannel) {
    let Some(fw) = state.firmware.get(&channel) else {
        tracing::info!(
            "install requested for '{board_id}' on {} but no dist is hosted there — ignored",
            channel.as_str()
        );
        return;
    };
    let version = fw.manifest.version.clone();
    let msg = build_fw_offer(state, board_id, channel, fw);
    match state.devices.lock().unwrap().get(board_id) {
        Some(tx) => {
            let _ = tx.send(Message::Text(serde_json::to_string(&msg).unwrap()));
            tracing::info!("install: offered {} firmware {version} to board '{board_id}'", channel.as_str());
        }
        None => tracing::info!("install requested for offline board '{board_id}' — ignored"),
    }
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
        set_notice(state, board_id, &[("SLEEPING", true, NOTICE_WHITE), (&caption, false, NOTICE_GREY)], SPLASH_SECS, false);
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
/// Drop per-game one-offs (spoiler hides/reveals, scroll opt-outs) whose games
/// left the slate — ids are per-event, so without this the persisted lists
/// grow forever.
pub fn prune_spoiler_lists(state: &AppState) {
    let ids: HashSet<String> = state.games.lock().unwrap().iter().map(|g| g.id.clone()).collect();
    let stale: Vec<(String, BoardConfig)> = {
        let configs = state.configs.lock().unwrap();
        configs
            .iter()
            .filter(|(_, c)| {
                c.spoiler_game_ids.iter().any(|id| !ids.contains(id))
                    || c.revealed_game_ids.iter().any(|id| !ids.contains(id))
                    || c.no_scroll_game_ids.iter().any(|id| !ids.contains(id))
            })
            .map(|(b, c)| (b.clone(), c.clone()))
            .collect()
    };
    for (bid, mut cfg) in stale {
        cfg.spoiler_game_ids.retain(|id| ids.contains(id));
        cfg.revealed_game_ids.retain(|id| ids.contains(id));
        cfg.no_scroll_game_ids.retain(|id| ids.contains(id));
        state.configs.lock().unwrap().insert(bid.clone(), cfg.clone());
        state.persist_config(&bid, &cfg);
    }
}

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
    evict_stale_frames(state);
    prune_spoiler_lists(state);
}

/// Frames are content-addressed and every score/clock/situation change mints new
/// ids, so the cache only grows — evict what no board can ask for anymore.
/// Devices only request ids from their CURRENT manifest (all kept via
/// `last_manifest`); one grace generation covers a device still fetching the
/// manifest we just replaced. Everything else is re-rendered on demand.
pub fn evict_stale_frames(state: &AppState) {
    let mut keep: HashSet<FrameId> = HashSet::new();
    for json in state.last_manifest.lock().unwrap().values() {
        if let Ok(ServerMsg::Manifest(m)) = serde_json::from_str::<ServerMsg>(json) {
            keep.extend(m.rotation.into_iter().map(|r| r.frame_id));
        }
    }
    let mut grace = state.frame_grace.lock().unwrap();
    // Bump the generation first (returns the old one): frames minted during the
    // era that just ended survive this pass unconditionally, closing the race
    // where a frame is rendered but its manifest isn't registered yet — by the
    // NEXT pass (30s), registration (microseconds) has long since happened.
    let prev_gen = state.frame_gen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut frames = state.frames.lock().unwrap();
    frames.retain(|id, (gen, _)| keep.contains(id) || grace.contains(id) || *gen >= prev_gen);
    *grace = keep;
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
                .filter_map(|g| cache.get(&g.frame_id).map(|(_, px)| Message::Binary(encode_frame(&g.frame_id, px))))
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

/// Push a firmware-download progress tick to every app subscribed to this
/// board, pruning any whose socket has closed — the same controller routing
/// `push_board_state` uses. Called from the streaming `/firmware/{chan}.bin`
/// handler as it hands bytes to the board's HTTP fetch (the device's WS is
/// dropped during the download, so progress is SERVER-observed).
pub fn push_firmware_progress(state: &AppState, board_id: &str, pct: u8) {
    let msg = ServerMsg::FirmwareProgress { board_id: board_id.to_string(), pct };
    let json = Message::Text(serde_json::to_string(&msg).unwrap());
    let mut controllers = state.controllers.lock().unwrap();
    if let Some(subs) = controllers.get_mut(board_id) {
        subs.retain(|tx| tx.send(json.clone()).is_ok());
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
    let tz = board_tz(&cfg);
    let now_showing: Vec<GameSummary> = {
        let (shown, bumped) = candidates_with_bumped(state, &cfg);
        let mut out: Vec<GameSummary> = shown
            .iter()
            .map(|g| {
                let spoiler = spoiler_for(&cfg, g);
                let mut s = to_summary_for(g, spoiler, tz);
                s.scrolls = game_scrolls(&cfg, g);
                let (fid, px) = render::render_frame(g, spoiler, tz, name_style_for(&cfg, &g.sport_id));
                state.cache_frame(fid.clone(), px);
                s.frame_id = fid;
                s
            })
            .collect();
        // Bumped games ride along flagged, without pre-rendered frames (the app
        // falls back to HTTP rendering for those cards).
        out.extend(bumped.iter().map(|g| {
            let mut s = to_summary_for(g, spoiler_for(&cfg, g), tz);
            s.scrolls = game_scrolls(&cfg, g);
            s.bumped = true;
            s
        }));
        out
    };
    let online = state.devices.lock().unwrap().contains_key(board_id);
    // What the connected device reported (dropped on disconnect, so offline
    // boards read None) and the version the app may offer as an update —
    // `offer_for` picks the newest dist among the board's channel's allowed
    // set, only when strictly newer by CalVer than the report. Junk versions
    // on either side simply mean no offer.
    let firmware_version = state.device_versions.lock().unwrap().get(board_id).cloned();
    let firmware_update = firmware_version
        .as_deref()
        .and_then(|cur| offer_for(&state.firmware, cfg.fw_channel, cur))
        .map(|(_, fw)| fw.manifest.version.clone());
    ServerMsg::BoardState {
        board_id: board_id.to_string(),
        online,
        now_showing,
        config: cfg,
        firmware_version,
        firmware_update,
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
    // When the slate we're about to compare against was fetched. The idle skip
    // means this can be hours or days back, and `record_finished_transitions`
    // needs it to date an end it only just noticed. None = nothing fetched yet
    // (state.games is empty, so no transition can be observed anyway).
    let mut prev_poll: Option<Instant> = None;
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
        let fetched_at = Instant::now();
        let mut games = source.fetch().await;
        // Ingestion filter: far-future scheduled games (a GP two weeks out,
        // week-one CFB) never enter state — no render work, no browse clutter.
        // The demo source flows through here too; its slate stays within the
        // horizon by construction (see demo_slate_survives_ingestion_filter).
        let fetched = games.len();
        let now = chrono::Utc::now();
        games.retain(|g| ingest_keeps(g, now));
        let dropped = fetched - games.len();
        if dropped > 0 {
            tracing::info!("ingestion: dropped {dropped} game(s) starting >24h out");
        }
        if games.is_empty() {
            // Honest empty slate: boards fall through to their no-games
            // behavior. (This used to substitute fake mock games, which
            // put made-up scores on a real scoreboard.)
            tracing::warn!("no games from source; showing empty slate");
        }
        let n = games.len();
        // Stamp any live -> Final transitions before the old slate disappears,
        // dated to the slate they're measured against (not to now).
        record_finished_transitions(&state, &games, prev_poll.unwrap_or(fetched_at));
        prev_poll = Some(fetched_at);
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
        // --preview sample frames; location unused by default styles. (Render
        // agent: populate to eyeball the Location style.)
        location: String::new(),
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
        start_time: String::new(), end_time: None,
        leaderboard: None,
        champ: None,
        session: None,
        situation: None,
        pregame: None,
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
        ("mlb_pregame", {
            let mut game = g("s4", "mlb", mk("HOU", "ASTROS", 0, "002D62", "EB6E1F"), mk("SEA", "MARINERS", 0, "0C2C56", "005C5C"), GameStatus::Pregame, "", "");
            game.start_time = "2026-07-08T02:10Z".into();
            game.pregame = Some(model::Pregame {
                away_record: "48-25".into(),
                home_record: "51-22".into(),
                broadcast: "ESPN".into(),
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
            status: GameStatus::Active, ordinal: String::new(), clock: String::new(), start_time: String::new(), end_time: None,
            leaderboard: Some(model::Leaderboard { name: "MASTERS".into(), rows: vec![
                model::LeaderRow { name: "SCHEFFLER".into(), value: "-8".into(), ..Default::default() },
                model::LeaderRow { name: "MCILROY".into(), value: "-6".into(), ..Default::default() },
                model::LeaderRow { name: "MORIKAWA".into(), value: "-5".into(), ..Default::default() },
                model::LeaderRow { name: "SCHAUFFELE".into(), value: "-3".into(), ..Default::default() },
                model::LeaderRow { name: "FLEETWOOD".into(), value: "-2".into(), ..Default::default() },
            ] }),
            champ: None,
            session: None,
            situation: None,
            pregame: None,
        }),
        ("f1", Game {
            id: "f".into(), sport_id: "f1".into(),
            away: model::TeamSide::empty("F1"), home: model::TeamSide::empty(""),
            status: GameStatus::Active, ordinal: String::new(), clock: String::new(), start_time: String::new(), end_time: None,
            leaderboard: Some(model::Leaderboard { name: "MONACO GP".into(), rows: vec![
                model::LeaderRow { rank: "1".into(), name: "VER".into(), value: "LAP 42".into(), color: Some(model::hex_to_rgb("3671C6")), tire: Some('M') },
                model::LeaderRow { rank: "2".into(), name: "NOR".into(), value: "+2.1".into(), color: Some(model::hex_to_rgb("FF8000")), tire: Some('M') },
                model::LeaderRow { rank: "3".into(), name: "LEC".into(), value: "+5.8".into(), color: Some(model::hex_to_rgb("E8002D")), tire: Some('S') },
                model::LeaderRow { rank: "4".into(), name: "PIA".into(), value: "+9.0".into(), color: Some(model::hex_to_rgb("FF8000")), tire: Some('H') },
                model::LeaderRow { rank: "20".into(), name: "HAM".into(), value: "+12".into(), color: Some(model::hex_to_rgb("E8002D")), tire: Some('S') },
            ] }),
            champ: None,
            session: None,
            situation: None,
            pregame: None,
        }),
    ];
    for (name, game) in &samples {
        let (_, px) = render::render_game(game, None, default_name_style(&game.sport_id));
        let path = format!("{dir}/{name}.png");
        save_png(&path, &px, 12);
        println!("wrote {path}");
    }
    // The board-facing tall leaderboard (app cards keep the 4-row static frame).
    if let Some(golf) = espn::demo_games(0, 0).into_iter().find(|g| g.id == "demo-golf") {
        let (_, px, scrolls) = render::render_board_frame(&golf, render::LEADERBOARD_DEFAULT_ROWS, None, None, default_name_style(&golf.sport_id));
        assert!(scrolls, "demo golf should be deep enough to scroll");
        let path = format!("{dir}/pga_scroll_tall.png");
        save_png(&path, &px, 12);
        println!("wrote {path}");
    }
}

fn rgb565_to_image(rgb565: &[u8], scale: u32) -> image::RgbImage {
    // Height from the payload: tall scrolling frames preview at full height.
    let height = rgb565.len() / (FRAME_WIDTH * 2);
    let mut img = image::RgbImage::new(FRAME_WIDTH as u32 * scale, height as u32 * scale);
    for y in 0..height {
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

pub(crate) fn save_png(path: &str, rgb565: &[u8], scale: u32) {
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
        // The splash is a pre-baked bar sweep: one step per second of notice.
        assert!(m.display_on && m.rotation.len() == SPLASH_SECS as usize, "sleep splash notice shows");

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

    /// Deep leaderboards get a scrolling manifest entry; the dwell stays the
    /// user's "Time per game" (the board fits one full pass INTO it), and team
    /// games stay static.
    #[test]
    fn manifest_scrolls_deep_leaderboards() {
        let state = AppState::new_ephemeral();
        *state.games.lock().unwrap() = espn::demo_games(0, 0);
        let m = manifest_for_board(&state, "b1");
        let dwell = state.config_for("b1").dwell_secs * 1000;
        let scrolled: Vec<&RotationEntry> = m.rotation.iter().filter(|e| e.scroll).collect();
        assert!(!scrolled.is_empty(), "demo golf/F1 should scroll");
        for e in &scrolled {
            let px = state.frames.lock().unwrap().get(&e.frame_id).map(|(_, p)| p.clone()).unwrap();
            assert!(px.len() > FRAME_BYTES, "scroll entries carry tall frames");
            assert_eq!(e.dwell_ms, dwell, "scroll syncs to Time per game, not the reverse");
        }
        for e in m.rotation.iter().filter(|e| !e.scroll) {
            let px = state.frames.lock().unwrap().get(&e.frame_id).map(|(_, p)| p.clone()).unwrap();
            assert_eq!(px.len(), FRAME_BYTES, "static entries stay panel-sized");
        }
    }

    /// END-TO-END scroll: the demo manifest, serialized exactly as the wire
    /// carries it, drives a real Board through a full dwell — the leaderboard
    /// window must actually MOVE. Guards the server/board contract that unit
    /// tests on either side can miss.
    #[test]
    fn board_scrolls_the_demo_leaderboard_through_the_wire() {
        let state = AppState::new_ephemeral();
        *state.games.lock().unwrap() = espn::demo_games(0, 0);
        push_board(&state, "b1");
        let json = state.last_manifest.lock().unwrap().get("b1").cloned().unwrap();
        // The wire round-trip the device performs.
        let ServerMsg::Manifest(m) = serde_json::from_str::<ServerMsg>(&json).unwrap() else {
            panic!("stored manifest should parse")
        };
        let scroll_idx = m.rotation.iter().position(|e| e.scroll).expect("a scrolling entry");
        let dwell = m.rotation[scroll_idx].dwell_ms as u64;

        let mut b = board_core::Board::new();
        b.gesture(board_core::Gesture::Long); // -> Setup (any non-boot mode)
        b.setup_done(); // -> Running? Setup stays; use manifest path instead
        b.set_online(true);
        b.tick(0);
        b.on_manifest(m.clone());
        // Feed every frame the manifest references, exactly as NeedFrames would.
        {
            let frames = state.frames.lock().unwrap();
            for e in &m.rotation {
                let (_, px) = frames.get(&e.frame_id).cloned().unwrap();
                b.on_frame(e.frame_id.clone(), px);
            }
        }
        // Walk to the scrolling entry: advance happens at each entry's dwell.
        let mut now = 0u64;
        for e in m.rotation.iter().take(scroll_idx) {
            now += e.dwell_ms as u64;
            b.tick(now);
        }
        // Mid top-hold vs mid glide-down: the visible window must differ.
        b.tick(now + dwell / 10); // t = 10% of dwell: inside the first fifth (hold)
        let held = b.framebuffer();
        b.tick(now + dwell * 3 / 10); // t = 30%: halfway through the glide
        let gliding = b.framebuffer();
        assert_ne!(held, gliding, "scroll window should have moved mid-glide");
    }

    /// Spoiler precedence: a one-off reveal beats every rule; team, league,
    /// board-wide, and one-off hides all engage the board's display mode; and
    /// sanitized summaries carry no scores in any mode.
    #[test]
    fn spoiler_rules_and_sanitized_summaries() {
        let state = AppState::new_ephemeral();
        let g = state.games.lock().unwrap().iter().find(|g| g.status == GameStatus::Active && g.leaderboard.is_none()).unwrap().clone();
        let mut cfg = BoardConfig::default();
        assert!(spoiler_for(&cfg, &g).is_none(), "nothing protected by default");

        // Board-wide.
        cfg.spoiler_all = true;
        assert_eq!(spoiler_for(&cfg, &g), Some(SpoilerMode::Normal));
        cfg.spoiler_mode = SpoilerMode::Strict;
        assert_eq!(spoiler_for(&cfg, &g), Some(SpoilerMode::Strict));

        // Reveal beats everything.
        cfg.revealed_game_ids.push(g.id.clone());
        assert!(spoiler_for(&cfg, &g).is_none(), "reveal wins over protect-all");
        cfg.revealed_game_ids.clear();
        cfg.spoiler_all = false;

        // League + team rules.
        cfg.sports.push(SportConfig { sport_id: g.sport_id.clone(), spoiler: true, ..Default::default() });
        assert!(spoiler_for(&cfg, &g).is_some(), "league rule engages");
        cfg.sports[0].spoiler = false;
        cfg.sports[0].spoiler_team_ids =
            vec![format!("{}-{}", g.sport_id, g.away.abbrev.to_lowercase())];
        assert!(spoiler_for(&cfg, &g).is_some(), "team rule engages");
        cfg.sports[0].spoiler_team_ids.clear();
        assert!(spoiler_for(&cfg, &g).is_none());

        // One-off hide.
        cfg.spoiler_game_ids.push(g.id.clone());
        assert!(spoiler_for(&cfg, &g).is_some());

        // Sanitized summary: no scores, no live detail, flagged.
        let s = to_summary_for(&g, Some(SpoilerMode::Normal), None);
        assert!(s.spoiled);
        assert_eq!((s.away_score, s.home_score), (0, 0));
        assert_eq!(s.detail, g.ordinal, "normal mode keeps period only");
        let s = to_summary_for(&g, Some(SpoilerMode::Strict), None);
        assert!(!s.detail.contains(&g.clock), "strict never leaks the clock");
    }

    /// Spoiler-protected games render without scores in the manifest AND the
    /// board-state frames; deep leaderboards collapse to a static frame.
    #[test]
    fn spoiled_manifest_hides_scores_and_scrolling() {
        let state = AppState::new_ephemeral();
        *state.games.lock().unwrap() = espn::demo_games(0, 0);
        crate::ws::apply_command(&state, "b1", Command::SetSpoilerAll { on: true });
        let m = manifest_for_board(&state, "b1");
        assert!(m.rotation.iter().all(|e| !e.scroll), "no leaderboard scrolls while spoiled");
        for e in &m.rotation {
            let len = state.frames.lock().unwrap().get(&e.frame_id).map(|(_, p)| p.len()).unwrap();
            assert_eq!(len, FRAME_BYTES, "every spoiled frame is panel-sized");
        }
        // The reveal exception restores the game's normal frame.
        let live_id = state.games.lock().unwrap().iter().find(|g| g.status == GameStatus::Active).unwrap().id.clone();
        crate::ws::apply_command(&state, "b1", Command::SetGameSpoiler { game_id: live_id.clone(), hidden: false });
        let cfg = state.config_for("b1");
        let g = state.games.lock().unwrap().iter().find(|g| g.id == live_id).unwrap().clone();
        assert!(spoiler_for(&cfg, &g).is_none(), "revealed game escapes protect-all");
    }

    /// A per-league dwell override retimes ONLY that league's entries; others
    /// keep the board default. Scrolling frames sync to whichever applies.
    #[test]
    fn league_dwell_override_retimes_that_league_only() {
        let state = AppState::new_ephemeral();
        *state.games.lock().unwrap() = espn::demo_games(0, 0);
        crate::ws::apply_command(&state, "b1", Command::SetLeagueDwell {
            sport_id: "f1".into(),
            secs: Some(30),
        });
        let m = manifest_for_board(&state, "b1");
        let games = state.games.lock().unwrap().clone();
        let cfg = state.config_for("b1");
        let chosen = chosen_games(&state, &cfg);
        assert_eq!(m.rotation.len(), chosen.len());
        let mut saw_f1 = false;
        for (entry, g) in m.rotation.iter().zip(&chosen) {
            if g.sport_id == "f1" {
                assert_eq!(entry.dwell_ms, 30_000, "override applies to F1");
                assert!(entry.scroll, "demo F1 is deep enough to scroll");
                saw_f1 = true;
            } else {
                assert_eq!(entry.dwell_ms, 10_000, "everything else keeps the default");
            }
        }
        assert!(saw_f1, "demo slate should include F1");
        drop(games);

        // None reverts to the board default.
        crate::ws::apply_command(&state, "b1", Command::SetLeagueDwell {
            sport_id: "f1".into(),
            secs: None,
        });
        let m = manifest_for_board(&state, "b1");
        assert!(m.rotation.iter().all(|e| e.dwell_ms == 10_000), "cleared override");
    }

    /// Leaderboard depth: 4 = the static panel layout (no scrolling); 20 = the
    /// full field as one tall frame; unset = the default 10.
    #[test]
    fn league_rows_control_leaderboard_depth() {
        let state = AppState::new_ephemeral();
        *state.games.lock().unwrap() = espn::demo_games(0, 0);
        let f1_entry = |m: &Manifest, state: &AppState| {
            let cfg = state.config_for("b1");
            let chosen = chosen_games(state, &cfg);
            m.rotation
                .iter()
                .zip(&chosen)
                .find(|(_, g)| g.id == "demo-f1")
                .map(|(e, _)| e.clone())
                .expect("demo F1 race in rotation")
        };
        let frame_h = |state: &AppState, e: &RotationEntry| {
            state.frames.lock().unwrap().get(&e.frame_id).map(|(_, p)| p.len() / (FRAME_WIDTH * 2)).unwrap()
        };

        // Default: top 10 (68px tall, scrolling).
        let m = manifest_for_board(&state, "b1");
        let e = f1_entry(&m, &state);
        assert!(e.scroll);
        assert_eq!(frame_h(&state, &e), 12 + 9 * 6 + 2);

        // Top 4: the static panel frame, no scroll.
        crate::ws::apply_command(&state, "b1", Command::SetLeagueRows { sport_id: "f1".into(), rows: Some(4) });
        let m = manifest_for_board(&state, "b1");
        let e = f1_entry(&m, &state);
        assert!(!e.scroll, "top-4 never scrolls");
        assert_eq!(frame_h(&state, &e), 32);

        // Full field: 20 rows = 128px, the firmware buffer's exact ceiling.
        crate::ws::apply_command(&state, "b1", Command::SetLeagueRows { sport_id: "f1".into(), rows: Some(20) });
        let m = manifest_for_board(&state, "b1");
        let e = f1_entry(&m, &state);
        assert!(e.scroll);
        assert_eq!(frame_h(&state, &e), 128);
        assert!(frame_h(&state, &e) * FRAME_WIDTH * 2 + 64 <= 16448, "fits the firmware frame buffer");
    }

    /// The frame cache stays bounded: ids no current manifest references are
    /// dropped after one grace generation (a device may still be fetching the
    /// manifest we just replaced), while live ids always survive.
    #[test]
    fn stale_frames_are_evicted_after_grace() {
        let state = AppState::new_ephemeral();
        push_board(&state, "b1"); // renders + caches the current rotation
        evict_stale_frames(&state); // grace <- current ids
        let old_ids: Vec<FrameId> = state.frames.lock().unwrap().keys().cloned().collect();
        assert!(!old_ids.is_empty(), "manifest should have cached frames");

        // A score change mints a new frame id for that game on the next push.
        state.games.lock().unwrap()[0].away.score += 100;
        push_board(&state, "b1");
        let live_ids: Vec<FrameId> = {
            let last = state.last_manifest.lock().unwrap();
            let json = last.get("b1").expect("manifest recorded");
            match serde_json::from_str::<ServerMsg>(json) {
                Ok(ServerMsg::Manifest(m)) => m.rotation.into_iter().map(|r| r.frame_id).collect(),
                _ => panic!("stored manifest should parse"),
            }
        };
        let replaced: Vec<&FrameId> = old_ids.iter().filter(|id| !live_ids.contains(id)).collect();
        assert!(!replaced.is_empty(), "the score bump should replace a frame id");

        // First pass: replaced ids survive on grace; live ids obviously kept.
        evict_stale_frames(&state);
        {
            let frames = state.frames.lock().unwrap();
            for id in &replaced {
                assert!(frames.contains_key(*id), "grace keeps last generation");
            }
        }
        // Second pass: grace expired — replaced ids gone, live ids kept.
        evict_stale_frames(&state);
        let frames = state.frames.lock().unwrap();
        for id in &replaced {
            assert!(!frames.contains_key(*id), "stale frame should be evicted");
        }
        for id in &live_ids {
            assert!(frames.contains_key(id), "current manifest frames must survive");
        }
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
        // One dwell-timed step per second of countdown.
        assert!(m.display_on && m.rotation.len() == NO_GAMES_SECS as usize, "notice rotation shows");
        assert!(
            state.notices.lock().unwrap().get("b1").unwrap().then_sleep,
            "the countdown notice re-sleeps the board on expiry"
        );
    }

    /// A second press DURING the no-games countdown sleeps immediately — the
    /// user shouldn't have to wait out the bar they were just shown.
    #[test]
    fn second_press_skips_the_no_games_countdown() {
        let state = AppState::new_ephemeral();
        state.games.lock().unwrap().clear();
        handle_button(&state, "b1"); // countdown up, power on
        assert!(state.config_for("b1").power);
        handle_button(&state, "b1"); // "yes, sleep now"
        let cfg = state.config_for("b1");
        assert!(!cfg.power, "second press sleeps without waiting");
        assert!(cfg.wake_at.is_some(), "the standard timed wake is armed");
        assert!(
            !state.notices.lock().unwrap().contains_key("b1"),
            "countdown cancelled, straight to the off-state"
        );
    }

    /// The countdown's expiry actually sleeps the board — but only if OUR
    /// notice (by epoch) is still the active one. A cleared/replaced notice
    /// (a game appeared mid-countdown) must never yank the panel dark late.
    #[test]
    fn countdown_expiry_sleeps_unless_superseded() {
        let state = AppState::new_ephemeral();
        state.games.lock().unwrap().clear();
        handle_button(&state, "b1");
        let epoch = state.notices.lock().unwrap().get("b1").unwrap().epoch;
        finish_notice(&state, "b1", epoch);
        let cfg = state.config_for("b1");
        assert!(!cfg.power, "expiry honors the promise: board is asleep");
        assert!(cfg.wake_at.is_some());

        // Round 2: the notice is cleared before expiry (as when games appear).
        handle_button(&state, "b1"); // wake -> new countdown
        let epoch = state.notices.lock().unwrap().get("b1").unwrap().epoch;
        state.notices.lock().unwrap().remove("b1");
        finish_notice(&state, "b1", epoch);
        assert!(state.config_for("b1").power, "a superseded countdown never sleeps the board");
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
                        dwell_secs: None,
                        leaderboard_rows: None,
                        spoiler: false,
                        spoiler_team_ids: vec![],
                        name_style: None,
                    },
                    SportConfig {
                        sport_id: "nhl".into(),
                        favorite_team_ids: vec!["nhl-bos".into()],
                        championship_option_id: None,
                        dwell_secs: None,
                        leaderboard_rows: None,
                        spoiler: false,
                        spoiler_team_ids: vec![],
                        name_style: None,
                    },
                    SportConfig {
                        sport_id: "golf".into(), // sport no longer in the catalog at all
                        favorite_team_ids: vec!["golf-pga".into()],
                        championship_option_id: None,
                        dwell_secs: None,
                        leaderboard_rows: None,
                        spoiler: false,
                        spoiler_team_ids: vec![],
                        name_style: None,
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
            start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: champ_tier.map(|t| model::Champ { tier: t.into(), label: t.into() }),
            session: session.map(Into::into),
            situation: None,
            pregame: None,
        };
        let cfg = |sport: &str, opt: &str| BoardConfig {
            sports: vec![SportConfig {
                sport_id: sport.into(),
                favorite_team_ids: vec![],
                championship_option_id: Some(opt.into()),
                dwell_secs: None,
                leaderboard_rows: None,
                spoiler: false,
                spoiler_team_ids: vec![],
                name_style: None,
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
            location: String::new(),
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
                start_time: String::new(), end_time: None,
                leaderboard: None,
                champ: None,
                session: None,
                situation: None,
                pregame: None,
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
                dwell_secs: None,
                leaderboard_rows: None,
                spoiler: false,
                spoiler_team_ids: vec![],
                name_style: None,
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
            start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            session: None,
            situation: None,
            pregame: None,
        };
        *state.games.lock().unwrap() = vec![live.clone()];

        // Poll observes the transition to Final, three minutes after the slate
        // it's comparing against (a normal cadence).
        let ended = Game { status: GameStatus::Final, ordinal: String::new(), clock: String::new(), ..live };
        let already_final = Game { id: "g2".into(), ..ended.clone() };
        let slate = vec![ended, already_final];
        let prev_poll = std::time::Instant::now() - Duration::from_secs(180);
        record_finished_transitions(&state, &slate, prev_poll);
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

    /// A live game (in whatever state the previous slate left it) for the
    /// linger tests below.
    fn live_game(id: &str) -> Game {
        Game {
            id: id.into(),
            sport_id: "pga".into(),
            away: TeamSide::empty("A"),
            home: TeamSide::empty("B"),
            status: GameStatus::Active,
            ordinal: String::new(),
            clock: String::new(),
            start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            session: None,
            situation: None,
            pregame: None,
        }
    }

    /// The reported bug: the poll loop skips fetching while nothing is
    /// connected, so a board connecting on Monday hands `record_finished_
    /// transitions` a frozen weekend slate. Dating the end to the previous
    /// observation (not to now) means an old gap produces an already-expired
    /// stamp — a tournament that ended Sunday must not reappear Monday — while
    /// a normal poll gap still lingers.
    #[test]
    fn stale_previous_slate_does_not_mint_a_fresh_linger() {
        // Fresh gap: we really did watch it end, so it lingers.
        let state = AppState::new_ephemeral();
        *state.games.lock().unwrap() = vec![live_game("g1")];
        let ended = Game { status: GameStatus::Final, ..live_game("g1") };
        record_finished_transitions(
            &state,
            std::slice::from_ref(&ended),
            std::time::Instant::now() - Duration::from_secs(180),
        );
        *state.games.lock().unwrap() = vec![ended.clone()];
        assert!(board_shows(&state, &ended), "3 minutes ago -> lingers");

        // Same transition, weekend-old slate: the end we're "observing" is two
        // days stale, so the linger is over before it starts.
        let state = AppState::new_ephemeral();
        *state.games.lock().unwrap() = vec![live_game("g1")];
        record_finished_transitions(
            &state,
            std::slice::from_ref(&ended),
            std::time::Instant::now() - Duration::from_secs(48 * 3600),
        );
        *state.games.lock().unwrap() = vec![ended.clone()];
        assert!(!board_shows(&state, &ended), "ended before we stopped watching -> no linger");
        assert!(
            state.finished.lock().unwrap().is_empty(),
            "the expired stamp is swept, not left to accumulate"
        );
    }

    /// Board and browse must never disagree: a Final the board is lingering is
    /// visible on the panel and in "Now showing", so View All has to list it —
    /// otherwise the user sees a game they can't find or remove.
    #[test]
    fn browse_lists_everything_the_board_shows() {
        let state = AppState::new_ephemeral();
        *state.games.lock().unwrap() = vec![live_game("g1")];
        let ended = Game { status: GameStatus::Final, ..live_game("g1") };
        record_finished_transitions(&state, std::slice::from_ref(&ended), std::time::Instant::now());
        *state.games.lock().unwrap() = vec![ended.clone()];

        assert!(board_shows(&state, &ended), "precondition: the board is lingering it");
        assert!(!browse_shows(&ended), "the time-only rule alone would hide it");
        assert!(browse_shows_now(&state, &ended), "…so the browse feed must add it back");

        // A Final nobody watched end is on neither list.
        let cold = Game { status: GameStatus::Final, ..live_game("g2") };
        assert!(!board_shows(&state, &cold));
        assert!(!browse_shows_now(&state, &cold));
    }

    /// "Coming to your scoreboard": a game the board's preferences already
    /// claim, that just hasn't started yet. Anything already on the panel is
    /// not queued (it's here), and a game no preference claims never is.
    #[test]
    fn queued_marks_upcoming_games_the_board_will_show() {
        use chrono::{Duration as D, Utc};
        let scheduled = |id: &str, abbrev: &str, hours: i64| Game {
            id: id.into(),
            sport_id: "nhl".into(),
            away: TeamSide { abbrev: abbrev.into(), ..TeamSide::empty(abbrev) },
            home: TeamSide::empty("ZZZ"),
            status: GameStatus::Pregame,
            ordinal: String::new(),
            clock: String::new(),
            start_time: (Utc::now() + D::hours(hours)).format("%Y-%m-%dT%H:%MZ").to_string(),
            end_time: None,
            leaderboard: None,
            champ: None,
            session: None,
            situation: None,
            pregame: None,
        };
        let state = AppState::new_ephemeral();
        // Tonight's Blackhawks game, a stranger's game, and a live favorite.
        let mine = scheduled("mine", "CHI", 6);
        let theirs = scheduled("theirs", "TOR", 6);
        let now_playing = Game { away: TeamSide { abbrev: "CHI".into(), ..TeamSide::empty("CHI") }, ..live_game("live") };
        *state.games.lock().unwrap() =
            vec![mine.clone(), theirs.clone(), now_playing.clone()];

        let mut cfg = BoardConfig::default();
        cfg.sports.push(SportConfig {
            sport_id: "nhl".into(),
            favorite_team_ids: vec!["nhl-chi".into()],
            ..Default::default()
        });
        let prefs = board_prefs(&cfg);

        assert!(game_queued(&state, &cfg, &prefs, &mine), "a favorite's upcoming game is coming");
        assert!(!game_queued(&state, &cfg, &prefs, &theirs), "a game no preference claims is not");
        assert!(
            !game_queued(&state, &cfg, &prefs, &now_playing),
            "a game already on the panel is here, not coming"
        );
    }

    /// Golf's feed timestamp is a midnight-Eastern DATE boundary, not a tee
    /// time — the exact case that showed a Thursday-morning tournament as
    /// "11:00 PM" the night before on a Central board.
    #[test]
    fn golf_starts_render_as_a_day_not_a_clock_time() {
        use chrono::{Duration as D, Utc};
        // Midnight ET on a day two from now = 04:00Z, the shape ESPN sends.
        let iso = (Utc::now() + D::days(2))
            .with_timezone(&chrono_tz::America::New_York)
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(chrono_tz::America::New_York)
            .unwrap()
            .with_timezone(&Utc)
            .format("%Y-%m-%dT%H:%MZ")
            .to_string();

        assert!(starts_on_a_day("pga") && starts_on_a_day("lpga"));
        assert!(!starts_on_a_day("nhl"), "team games have real start times");
        assert!(!starts_on_a_day("f1"), "F1 sessions have real session times");

        // A weekday, not a time — and NOT shifted a day earlier by the board's
        // western timezone, which is what produced the bogus "11:00 PM".
        let label = start_day_label(&iso).expect("parses");
        assert!(!label.contains(':'), "a day label, not a clock: {label}");
        let chicago = start_time_label(&iso, Some(chrono_tz::America::Chicago)).unwrap();
        assert!(chicago.contains("11:00 PM"), "precondition: the old label was {chicago}");

        // Near days read as words.
        let tomorrow = (Utc::now() + D::days(1))
            .with_timezone(&chrono_tz::America::New_York)
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(chrono_tz::America::New_York)
            .unwrap()
            .with_timezone(&Utc)
            .format("%Y-%m-%dT%H:%MZ")
            .to_string();
        assert_eq!(start_day_label(&tomorrow).as_deref(), Some("TOMORROW"));
    }

    /// Leaderboard sports have ESPN pages too — just not /gameId/ ones.
    #[test]
    fn leaderboard_sports_get_espn_links() {
        use crate::espn::espn_game_url;
        assert_eq!(
            espn_game_url("pga", "401811960").as_deref(),
            Some("https://www.espn.com/golf/leaderboard?tournamentId=401811960")
        );
        assert_eq!(
            espn_game_url("lpga", "123").as_deref(),
            Some("https://www.espn.com/golf/leaderboard?tournamentId=123")
        );
        // F1 games are per-session; the results page wants the bare event id.
        assert_eq!(
            espn_game_url("f1", "600057440-race").as_deref(),
            Some("https://www.espn.com/racing/results/_/raceId/600057440")
        );
        assert_eq!(espn_game_url("pga", "demo-golf"), None, "demo ids still have no page");
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
            end_time: None,
            leaderboard: None,
            champ: None,
            session: None,
            situation: None,
            pregame: None,
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

    /// The ingestion filter drops only far-future Pregame games: anything
    /// underway or final stays (multi-day golf, linger-owned Finals), and an
    /// unparseable start time is kept so a feed quirk can't blank a league.
    #[test]
    fn ingestion_drops_only_far_future_pregames() {
        use chrono::{Duration as D, Utc};
        let now = Utc::now();
        let mk = |status: GameStatus, mins_from_now: i64| Game {
            id: "g".into(),
            sport_id: "cfb".into(),
            away: TeamSide::empty("A"),
            home: TeamSide::empty("B"),
            status,
            ordinal: String::new(),
            clock: String::new(),
            start_time: (now + D::minutes(mins_from_now)).format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            end_time: None,
            leaderboard: None,
            champ: None,
            session: None,
            situation: None,
            pregame: None,
        };
        // Pregame two weeks out (the Belgian GP problem): never enters state.
        assert!(!ingest_keeps(&mk(GameStatus::Pregame, 14 * 24 * 60), now), "2 weeks out -> dropped");
        // Pregame tonight: kept.
        assert!(ingest_keeps(&mk(GameStatus::Pregame, 3 * 60), now), "3h out -> kept");
        // Active with a start date two days AGO: a multi-day golf event, kept.
        assert!(ingest_keeps(&mk(GameStatus::Active, -2 * 24 * 60), now), "underway -> kept regardless of start");
        // Final: the linger rules own its exit, ingestion never drops it.
        assert!(ingest_keeps(&mk(GameStatus::Final, -3 * 60), now), "final -> kept");
        // Intermission is live too.
        assert!(ingest_keeps(&mk(GameStatus::Intermission, -60), now), "intermission -> kept");
        // Unparseable start time: KEPT — a feed quirk must not blank a league.
        let mut odd = mk(GameStatus::Pregame, 0);
        odd.start_time = "not-a-time".into();
        assert!(ingest_keeps(&odd, now), "unparseable start -> kept");
    }

    /// ESPN timestamps (with and without seconds) parse; labels render local time.
    #[test]
    fn espn_time_parsing() {
        assert!(parse_espn_time("2026-06-28T13:00Z").is_some());
        assert!(parse_espn_time("2026-06-28T13:00:00Z").is_some());
        assert!(parse_espn_time("garbage").is_none());
        assert!(start_time_label("2026-06-28T13:00Z", None).is_some());
        // Board timezones move the label: 23:00Z is 7 PM in New York (EDT) and
        // 8 AM the next morning in Tokyo.
        let ny: chrono_tz::Tz = "America/New_York".parse().unwrap();
        let tokyo: chrono_tz::Tz = "Asia/Tokyo".parse().unwrap();
        assert_eq!(start_time_label("2026-06-28T23:00Z", Some(ny)).as_deref(), Some("7:00 PM"));
        assert_eq!(start_time_label("2026-06-28T23:00Z", Some(tokyo)).as_deref(), Some("8:00 AM"));
    }

    /// Summaries carry machine-readable RFC3339 UTC timestamps alongside the
    /// human detail label: start_time normalized from ESPN's format, end_time
    /// only when the feed carried one (multi-day golf/F1 events), and neither
    /// is stripped by spoiler protection (times are not spoilers).
    #[test]
    fn summaries_carry_normalized_timestamps() {
        let mut g = Game {
            id: "g".into(),
            sport_id: "nhl".into(),
            away: TeamSide::empty("A"),
            home: TeamSide::empty("B"),
            status: GameStatus::Active,
            ordinal: "2nd".into(),
            clock: "5:00".into(),
            start_time: "2026-06-28T13:00Z".into(),
            end_time: None,
            leaderboard: None,
            champ: None,
            session: None,
            situation: None,
            pregame: None,
        };
        let s = to_summary(&g, None);
        assert_eq!(s.start_time.as_deref(), Some("2026-06-28T13:00:00Z"));
        assert_eq!(s.end_time, None, "no feed end date -> None");
        // The normalized value is genuinely RFC3339.
        chrono::DateTime::parse_from_rfc3339(s.start_time.as_deref().unwrap()).unwrap();

        // A multi-day event's end date is normalized the same way.
        g.end_time = Some("2026-06-30T22:00Z".into());
        let s = to_summary(&g, None);
        assert_eq!(s.end_time.as_deref(), Some("2026-06-30T22:00:00Z"));

        // Times survive spoiler sanitization in BOTH modes — even strict mode
        // shows matchup + time.
        for mode in [SpoilerMode::Normal, SpoilerMode::Strict] {
            let s = to_summary_for(&g, Some(mode), None);
            assert!(s.spoiled);
            assert_eq!(s.start_time.as_deref(), Some("2026-06-28T13:00:00Z"));
            assert_eq!(s.end_time.as_deref(), Some("2026-06-30T22:00:00Z"));
        }

        // Unparseable feed text -> None, never garbage.
        g.start_time = "not-a-time".into();
        g.end_time = Some("also-not-a-time".into());
        let s = to_summary(&g, None);
        assert_eq!((s.start_time, s.end_time), (None, None));
    }

    /// GET /game/{id}: the full processed game as pretty JSON; unknown ids
    /// are a plain-text 404.
    #[tokio::test]
    async fn game_endpoint_serves_pretty_json_and_404s() {
        use axum::extract::{Path, Query};
        let state = AppState::new_ephemeral(); // mock slate incl. "mock-nhl" (BOS @ TOR)
        let resp = get_game(Path("mock-nhl".into()), Query(HashMap::new()), State(state.clone())).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert_eq!(
            resp.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains('\n'), "pretty-printed, not a single line");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["away"]["abbrev"], "BOS", "carries the away team");
        assert_eq!(v["away"]["score"], 2, "unscoped request shows the score");
        assert!(v.get("spoiled").is_none(), "unprotected game carries no spoiled flag");

        let resp = get_game(Path("no-such-game".into()), Query(HashMap::new()), State(state)).await;
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    /// The spoiler rule is leak-proof through the raw-data path too: a
    /// protected board's request gets zeroed scores, no situation/clock, and
    /// an EMPTY leaderboard, flagged "spoiled": true — while the same id
    /// without a board (or after a reveal) serves the full data.
    #[tokio::test]
    async fn game_endpoint_redacts_spoiler_protected_games() {
        use axum::extract::{Path, Query};
        let state = AppState::new_ephemeral();
        // Tick 7: demo-nhl has nonzero scores + a hockey situation; demo-golf
        // has a populated leaderboard.
        *state.games.lock().unwrap() = espn::demo_games(7, 0);
        crate::ws::apply_command(&state, "b1", Command::SetSpoilerAll { on: true });

        let fetch = |id: &str, board: Option<&str>| {
            let state = state.clone();
            let id = id.to_string();
            let q: HashMap<String, String> = board
                .map(|b| HashMap::from([("board".to_string(), b.to_string())]))
                .unwrap_or_default();
            async move {
                let resp = get_game(Path(id), Query(q), State(state)).await;
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()
            }
        };

        // Protected: zeroed scores, no live details, flagged.
        let v = fetch("demo-nhl", Some("b1")).await;
        assert_eq!(v["spoiled"], true);
        assert_eq!((v["away"]["score"].as_u64(), v["home"]["score"].as_u64()), (Some(0), Some(0)));
        assert!(v["situation"].is_null(), "situation carries the power play — redacted");
        assert_eq!(v["clock"], "", "the clock is a result detail");
        assert!(!v["ordinal"].as_str().unwrap().is_empty(), "normal mode keeps basic progress");

        // Protected leaderboard: the event name stays, the rows go.
        let v = fetch("demo-golf", Some("b1")).await;
        assert_eq!(v["spoiled"], true);
        assert_eq!(v["leaderboard"]["name"], "MASTERS");
        assert_eq!(v["leaderboard"]["rows"].as_array().map(Vec::len), Some(0));

        // A FINAL's ordinal is a result ("OT" says the game went past
        // regulation) — Normal mode must hide it once the game ends, exactly
        // like spoiler_detail and render_spoiled collapse finals to "FINAL".
        // (ESPN leaves period at its last value on finals, so parse_event
        // yields ordinal "OT" for an overtime final — force that shape here.)
        {
            let mut games = state.games.lock().unwrap();
            let g = games.iter_mut().find(|g| g.id == "demo-nhl").unwrap();
            g.status = GameStatus::Final;
            g.ordinal = "OT".into();
        }
        let v = fetch("demo-nhl", Some("b1")).await;
        assert_eq!(v["ordinal"], "", "a final's ordinal is a result, not progress");

        // Strict mode hides even the in-progress ordinal.
        crate::ws::apply_command(&state, "b1", Command::SetSpoilerMode { mode: SpoilerMode::Strict });
        let v = fetch("demo-nhl", Some("b1")).await;
        assert_eq!(v["ordinal"], "", "strict hides basic progress too");

        // The SAME id with no board scope serves the full data.
        let v = fetch("demo-nhl", None).await;
        assert!(v["away"]["score"].as_u64().unwrap() > 0);
        assert!(!v["situation"].is_null());
        assert!(v.get("spoiled").is_none());
        let v = fetch("demo-golf", None).await;
        assert!(v["leaderboard"]["rows"].as_array().map(Vec::len).unwrap() > 0);

        // A one-off reveal beats the board-wide rule here like everywhere else.
        crate::ws::apply_command(&state, "b1", Command::SetGameSpoiler { game_id: "demo-nhl".into(), hidden: false });
        let v = fetch("demo-nhl", Some("b1")).await;
        assert!(v["away"]["score"].as_u64().unwrap() > 0, "revealed game escapes redaction");
        assert!(v.get("spoiled").is_none());
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
        assert!(m.display_on && m.rotation.len() == SPLASH_SECS as usize, "splash sweep shows");
        handle_app_power(&state, "b1", true);
        assert!(!state.notices.lock().unwrap().contains_key("b1"), "power on cancels the notice");
    }

    /// The notice frame carries a depleting progress bar: as time passes the render
    /// changes (bar shrinks), so successive manifests differ.
    #[test]
    fn notice_progress_bar_fills_toward_the_event() {
        let full = render::render_notice(&[("SLEEPING", true, NOTICE_WHITE)], Some(1.0));
        let half = render::render_notice(&[("SLEEPING", true, NOTICE_WHITE)], Some(0.5));
        let none = render::render_notice(&[("SLEEPING", true, NOTICE_WHITE)], Some(0.0));
        assert_ne!(full.0, half.0, "bar width must change the frame");
        assert_ne!(half.0, none.0);
        // Reset-button styling: fill GROWS as `remaining` shrinks. Measure a
        // row inside the outlined box (box top at FRAME_HEIGHT-6).
        let lit_in_bar = |px: &[u8]| {
            let y = scoreboard_protocol::FRAME_HEIGHT - 4; // interior fill row
            (0..scoreboard_protocol::FRAME_WIDTH)
                .filter(|x| {
                    let i = (y * scoreboard_protocol::FRAME_WIDTH + x) * 2;
                    px[i] != 0 || px[i + 1] != 0
                })
                .count()
        };
        let (f, h, n) = (lit_in_bar(&full.1), lit_in_bar(&half.1), lit_in_bar(&none.1));
        assert!(f < h && h < n, "fill grows toward the event: {f} < {h} < {n}");
        assert_eq!(f, 2, "all-remaining shows just the outline edges");
        assert_eq!(n, scoreboard_protocol::FRAME_WIDTH - 8, "none-remaining fills the box");
    }

    /// A notice is ONE pre-baked dwell-timed rotation the board steps through on
    /// its own clock (no per-step network pushes): N distinct frames whose bar
    /// sweeps down, dwells covering the whole duration, the last entry padded
    /// past expiry so the wrap back to a full bar never shows.
    #[test]
    fn notice_renders_as_one_dwell_timed_rotation() {
        let state = AppState::new_ephemeral();
        state.games.lock().unwrap().clear();
        handle_button(&state, "b1"); // idle press -> the 10-second "NO GAMES" countdown
        let m = manifest_for_board(&state, "b1");

        let steps = m.rotation.len();
        assert_eq!(steps, NO_GAMES_SECS as usize, "one 1s step per countdown second");
        assert!(m.rotation.iter().all(|e| !e.scroll), "notices never scroll");
        // Distinct content-addressed frames: every step draws a different bar.
        let ids: HashSet<&FrameId> = m.rotation.iter().map(|e| &e.frame_id).collect();
        assert_eq!(ids.len(), steps);

        // Dwells cover the whole duration; the FINAL entry carries the +2s pad.
        let step_ms = NO_GAMES_SECS * 1000 / steps as u64;
        for e in &m.rotation[..steps - 1] {
            assert_eq!(e.dwell_ms as u64, step_ms);
        }
        assert_eq!(m.rotation[steps - 1].dwell_ms as u64, step_ms + 2000, "last entry padded past expiry");
        let total: u64 = m.rotation.iter().map(|e| e.dwell_ms as u64).sum();
        assert_eq!(total, NO_GAMES_SECS * 1000 + 2000);

        // The bar advances (fill strictly GROWS) across entries — reset style.
        let lit = |id: &FrameId| {
            let frames = state.frames.lock().unwrap();
            let (_, px) = frames.get(id).cloned().expect("sweep frame cached");
            let y = FRAME_HEIGHT - 4; // interior fill row of the outlined box
            (0..FRAME_WIDTH)
                .filter(|x| {
                    let i = (y * FRAME_WIDTH + x) * 2;
                    px[i] != 0 || px[i + 1] != 0
                })
                .count()
        };
        let widths: Vec<usize> = m.rotation.iter().map(|e| lit(&e.frame_id)).collect();
        assert!(widths.windows(2).all(|w| w[1] > w[0]), "bar must fill: {widths:?}");

        // A mid-notice re-render is byte-identical, so push_board's dedup keeps
        // the board's rotation position — the fix for the stuttering splash.
        let m2 = manifest_for_board(&state, "b1");
        assert_eq!(
            serde_json::to_string(&ServerMsg::Manifest(m)).unwrap(),
            serde_json::to_string(&ServerMsg::Manifest(m2)).unwrap()
        );
    }

    /// A short splash gets one bar step per second (4s -> 4 entries of ~1s).
    #[test]
    fn short_splash_steps_once_per_second() {
        let state = AppState::new_ephemeral();
        handle_button(&state, "b1"); // games showing -> sleep + 4s splash
        let m = manifest_for_board(&state, "b1");
        assert_eq!(m.rotation.len(), SPLASH_SECS as usize);
        for e in &m.rotation[..m.rotation.len() - 1] {
            assert_eq!(e.dwell_ms, 1000);
        }
        assert_eq!(m.rotation.last().unwrap().dwell_ms, 3000, "1s + the 2s anti-wrap pad");
    }

    /// Notices carry a fresh nonzero manifest epoch per SHOWING: the board
    /// restarts the sweep even when its step count matches the outgoing games
    /// rotation's length (the same-shape heuristic can't tell them apart), and
    /// a re-set notice mid-showing yields a DIFFERENT manifest the dedup can't
    /// swallow — the bar restarts with the new expiry. Games manifests are
    /// always program 0.
    #[test]
    fn notice_epochs_mark_a_new_program_per_showing() {
        let state = AppState::new_ephemeral();
        handle_button(&state, "b1"); // games showing -> sleep + splash
        let first = manifest_for_board(&state, "b1");
        assert_ne!(first.epoch, 0, "a notice is not the games program");

        handle_button(&state, "b1"); // wake -> back to the games rotation
        assert_eq!(manifest_for_board(&state, "b1").epoch, 0, "games = program 0");

        handle_button(&state, "b1"); // sleep again -> a NEW showing of the splash
        let second = manifest_for_board(&state, "b1");
        assert_ne!(second.epoch, 0);
        assert_ne!(first.epoch, second.epoch, "each showing mints its own epoch");
    }

    /// The board reports its own IP-geolocated zone: valid names are adopted
    /// and persisted, junk is ignored, and re-reports of the same zone are
    /// no-ops (the firmware re-sends after every reconnect).
    #[test]
    fn device_reported_timezone_is_adopted_with_validation() {
        let state = AppState::new_ephemeral();
        handle_device_timezone(&state, "b1", "America/Chicago");
        assert_eq!(state.config_for("b1").timezone.as_deref(), Some("America/Chicago"));

        handle_device_timezone(&state, "b1", "Not/AZone");
        assert_eq!(
            state.config_for("b1").timezone.as_deref(),
            Some("America/Chicago"),
            "junk from a spoofed geolocation reply must not clobber the zone"
        );

        handle_device_timezone(&state, "b1", "Asia/Tokyo");
        assert_eq!(state.config_for("b1").timezone.as_deref(), Some("Asia/Tokyo"));
    }

    /// The idle clock renders in the BOARD's timezone: two boards in different
    /// zones get different frames; SetTimezone round-trips through the config;
    /// an unparseable zone is ignored (the old zone stays).
    #[test]
    fn per_board_timezone_drives_the_clock() {
        let state = AppState::new_ephemeral();
        state.games.lock().unwrap().clear(); // idle -> off-state clock

        crate::ws::apply_command(&state, "b1", Command::SetTimezone { tz: "America/New_York".into() });
        crate::ws::apply_command(&state, "b2", Command::SetTimezone { tz: "Asia/Tokyo".into() });
        assert_eq!(state.config_for("b1").timezone.as_deref(), Some("America/New_York"));
        assert_eq!(state.config_for("b2").timezone.as_deref(), Some("Asia/Tokyo"));

        // New York and Tokyo are 13–14h apart — their wall clocks never agree,
        // so the rendered clock frames must differ.
        let m1 = manifest_for_board(&state, "b1");
        let m2 = manifest_for_board(&state, "b2");
        assert_eq!(m1.rotation.len(), 1);
        assert_ne!(m1.rotation[0].frame_id, m2.rotation[0].frame_id, "clocks track board zones");

        // Garbage is ignored: the board keeps its zone.
        crate::ws::apply_command(&state, "b1", Command::SetTimezone { tz: "Not/AZone".into() });
        assert_eq!(state.config_for("b1").timezone.as_deref(), Some("America/New_York"));
    }

    /// Timezone rides the portable settings blob — but ONLY when the export
    /// carried one, so an old blob can't erase where a board lives.
    #[test]
    fn portable_settings_move_timezone_only_when_present() {
        let src = BoardConfig { timezone: Some("America/Denver".into()), ..Default::default() };
        let blob = PortableSettings::from(&src);
        let mut dst = BoardConfig { timezone: Some("Asia/Tokyo".into()), ..Default::default() };
        blob.apply_to(&mut dst);
        assert_eq!(dst.timezone.as_deref(), Some("America/Denver"), "export with a zone applies it");

        // A pre-timezone blob deserializes with timezone=None and leaves it alone.
        let old: PortableSettings = serde_json::from_str(r#"{"scoreboard_settings":1}"#).unwrap();
        assert!(old.timezone.is_none());
        let mut dst = BoardConfig { timezone: Some("Asia/Tokyo".into()), ..Default::default() };
        old.apply_to(&mut dst);
        assert_eq!(dst.timezone.as_deref(), Some("Asia/Tokyo"), "old blobs keep the board's zone");
    }

    /// The per-game scroll toggle: turning a deep leaderboard off renders the
    /// static top-4 panel frame (scroll=false) while the rest of the league
    /// keeps its depth; toggling back restores the tall scroll; ResetRotation
    /// clears the list; the summaries advertise which games CAN scroll.
    #[test]
    fn per_game_scroll_toggle() {
        let state = AppState::new_ephemeral();
        *state.games.lock().unwrap() = espn::demo_games(0, 0);
        let entry_for = |m: &Manifest, id: &str| {
            let cfg = state.config_for("b1");
            let chosen = chosen_games(&state, &cfg);
            m.rotation
                .iter()
                .zip(&chosen)
                .find(|(_, g)| g.id == id)
                .map(|(e, _)| e.clone())
                .expect("game in rotation")
        };
        let frame_h = |e: &RotationEntry| {
            state.frames.lock().unwrap().get(&e.frame_id).map(|(_, p)| p.len() / (FRAME_WIDTH * 2)).unwrap()
        };

        // Baseline: demo F1 scrolls at the default depth.
        let m = manifest_for_board(&state, "b1");
        let e = entry_for(&m, "demo-f1");
        assert!(e.scroll, "demo F1 scrolls by default");
        assert!(frame_h(&e) > FRAME_HEIGHT);

        // Toggle off: THIS game renders the static top-4 frame…
        crate::ws::apply_command(&state, "b1", Command::SetGameScroll { game_id: "demo-f1".into(), scroll: false });
        assert_eq!(state.config_for("b1").no_scroll_game_ids, vec!["demo-f1".to_string()]);
        let m = manifest_for_board(&state, "b1");
        let e = entry_for(&m, "demo-f1");
        assert!(!e.scroll, "opted-out game must not scroll");
        assert_eq!(frame_h(&e), FRAME_HEIGHT, "static frame is panel-sized");
        // …while the other deep leaderboard (demo golf) still scrolls.
        let golf = entry_for(&m, "demo-golf");
        assert!(golf.scroll, "the opt-out is per-game, not per-league");

        // Summaries still advertise the game as scroll-capable (the app keys
        // the toggle's LABEL off config.no_scroll_game_ids).
        let ServerMsg::BoardState { now_showing, config, .. } = board_state_msg(&state, "b1") else {
            panic!("expected BoardState");
        };
        let f1 = now_showing.iter().find(|g| g.id == "demo-f1").unwrap();
        assert!(f1.scrolls, "deep leaderboards advertise the toggle");
        assert!(config.no_scroll_game_ids.contains(&"demo-f1".to_string()));
        let team_game = now_showing.iter().find(|g| !g.home.is_empty()).unwrap();
        assert!(!team_game.scrolls, "team games never offer the toggle");

        // Toggle back on: the tall scrolling frame returns.
        crate::ws::apply_command(&state, "b1", Command::SetGameScroll { game_id: "demo-f1".into(), scroll: true });
        assert!(state.config_for("b1").no_scroll_game_ids.is_empty());
        let m = manifest_for_board(&state, "b1");
        let e = entry_for(&m, "demo-f1");
        assert!(e.scroll && frame_h(&e) > FRAME_HEIGHT, "toggling back restores the scroll");

        // ResetRotation clears the list like the other per-game tweaks.
        crate::ws::apply_command(&state, "b1", Command::SetGameScroll { game_id: "demo-f1".into(), scroll: false });
        crate::ws::apply_command(&state, "b1", Command::ResetRotation);
        assert!(state.config_for("b1").no_scroll_game_ids.is_empty());
    }

    /// Ended/vanished games fall out of no_scroll_game_ids with the other
    /// per-game lists, so persisted configs stay bounded.
    #[test]
    fn no_scroll_list_is_pruned_with_the_slate() {
        let state = AppState::new_ephemeral();
        *state.games.lock().unwrap() = espn::demo_games(0, 0);
        crate::ws::apply_command(&state, "b1", Command::SetGameScroll { game_id: "demo-f1".into(), scroll: false });
        crate::ws::apply_command(&state, "b1", Command::SetGameScroll { game_id: "gone-game".into(), scroll: false });
        prune_spoiler_lists(&state);
        assert_eq!(state.config_for("b1").no_scroll_game_ids, vec!["demo-f1".to_string()]);
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
            location: String::new(),
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
            start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            session: None,
            situation: None,
            pregame: None,
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
            dwell_secs: None,
            leaderboard_rows: None,
            spoiler: false,
            spoiler_team_ids: vec![],
            name_style: None,
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
        s.register_pairing_code("123456".into(), "board-x");
        // correct code pairs + authorizes the controller
        assert_eq!(try_pair(&s, "123456", "ctrl-1").unwrap(), "board-x");
        assert!(s.allow.lock().unwrap().get("board-x").unwrap().contains("ctrl-1"));
        // the code is single-use
        assert!(try_pair(&s, "123456", "ctrl-1").is_err());
        // an unknown code fails
        assert!(try_pair(&s, "999999", "ctrl-2").is_err());
    }

    /// Codes are short-lived and singular per board: an old one can't be
    /// redeemed, and showing a new code retires whatever was on the panel.
    #[test]
    fn pairing_codes_expire_and_supersede() {
        let s = AppState::new_ephemeral();

        // A code from a setup session ten minutes ago is dead.
        s.pending
            .lock()
            .unwrap()
            .insert("111111".into(), ("board-x".into(), Instant::now() - PAIR_CODE_TTL));
        assert!(try_pair(&s, "111111", "ctrl-1").is_err(), "expired code can't pair");

        // Re-entering setup replaces this board's code; the old one stops working.
        s.register_pairing_code("222222".into(), "board-x");
        s.register_pairing_code("333333".into(), "board-x");
        assert!(try_pair(&s, "222222", "ctrl-1").is_err(), "superseded code can't pair");

        // Another board's live code is untouched by all of that, and the
        // current code still pairs.
        s.register_pairing_code("444444".into(), "board-y");
        assert_eq!(try_pair(&s, "333333", "ctrl-1").unwrap(), "board-x");
        assert_eq!(try_pair(&s, "444444", "ctrl-1").unwrap(), "board-y");
    }

    /// Unpairing revokes exactly one controller's grant, durably.
    #[test]
    fn unpair_revokes_only_the_asking_controller() {
        let path = std::env::temp_dir().join(format!("sb-unpair-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let s = AppState::new_inner(Some(open_db(&path).unwrap()), mock_catalog());
            s.register_pairing_code("123456".into(), "board-x");
            try_pair(&s, "123456", "ctrl-1").unwrap();
            s.register_pairing_code("654321".into(), "board-x");
            try_pair(&s, "654321", "ctrl-2").unwrap();

            // No-ops: a board you're not paired with, and someone else's grant.
            assert!(!s.unpair("board-y", "ctrl-1"), "never paired with board-y");
            assert!(!s.unpair("board-x", "ctrl-3"), "not a paired controller");
            assert!(s.is_allowed("board-x", "ctrl-1"));

            // The real thing: ctrl-1 hands its grant back. Commands from it are
            // refused afterwards (`is_allowed` is the gate every command path
            // checks); the other phone keeps working.
            assert!(s.unpair("board-x", "ctrl-1"));
            assert!(!s.is_allowed("board-x", "ctrl-1"), "revoked controller is refused");
            assert!(s.is_allowed("board-x", "ctrl-2"), "the other phone is untouched");
        }
        {
            // …and it stays revoked across a restart: the DB row is gone, not
            // just the in-memory entry.
            let s = AppState::new_inner(Some(open_db(&path).unwrap()), mock_catalog());
            assert!(!s.is_allowed("board-x", "ctrl-1"));
            assert!(s.is_allowed("board-x", "ctrl-2"));
        }
        let _ = std::fs::remove_file(&path);
    }

    /// The pairing throttle is per identity: one guesser can drain its own
    /// budget without locking anyone else out of pairing.
    #[test]
    fn pair_throttle_is_per_controller() {
        let s = AppState::new_ephemeral();
        let mut spent = 0;
        while s.allow_pair_attempt("attacker") {
            spent += 1;
            assert!(spent <= PAIR_BURST as usize, "bucket must empty within its capacity");
        }
        assert_eq!(spent, PAIR_BURST as usize);
        assert!(!s.allow_pair_attempt("attacker"), "stays empty");
        assert!(s.allow_pair_attempt("someone-else"), "a real phone can still pair");
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
            dwell_secs: None,
            leaderboard_rows: None,
            spoiler: false,
            spoiler_team_ids: vec![],
            name_style: None,
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

    // ---- OTA firmware hosting ----

    /// Build a valid dist dir (image + matching firmware.json) under a unique
    /// temp path. Returns (dir, image bytes). Sig is any hex string — the
    /// server relays it opaquely; only the BOARD verifies it.
    fn write_dist(tag: &str, version: &str, channel: &str) -> (PathBuf, Vec<u8>) {
        let dir = std::env::temp_dir().join(format!("scoreboard-fw-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let image = b"\xE9\x00firmware image bytes".to_vec();
        std::fs::write(dir.join("firmware.bin"), &image).unwrap();
        let json = serde_json::json!({
            "version": version,
            "channel": channel,
            "size": image.len(),
            "sha256": sha256_hex(&image),
            "sig": "ab".repeat(64),
        });
        std::fs::write(dir.join("firmware.json"), json.to_string()).unwrap();
        (dir, image)
    }

    /// The update decision is the pinned CalVer tuple compare: strictly newer
    /// offers, equal/older doesn't, junk on either side never does.
    #[test]
    fn calver_update_compare() {
        assert!(version_newer("2026.7.1", "2026.7.0"));
        assert!(!version_newer("2026.7.0", "2026.7.1"));
        assert!(version_newer("2027.1.0", "2026.12.0"));
        assert!(!version_newer("2026.12.0", "2027.1.0"));
        assert!(!version_newer("2026.7.0", "2026.7.0")); // equal = no offer
        assert!(!version_newer("junk", "2026.7.0"));
        assert!(!version_newer("2026.8.0", "junk"));
        assert!(!version_newer("2026.8", "2026.7.0")); // not three parts
    }

    /// An absent dist quietly disables the feature; a dist whose image doesn't
    /// match its manifest (size or sha256) is a load ERROR — the caller treats
    /// that as fatal at startup so a broken image is never served.
    #[test]
    fn firmware_dist_load_is_integrity_checked() {
        // Nothing there at all -> feature off, not an error.
        let missing = std::env::temp_dir().join(format!("scoreboard-fw-test-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        assert!(matches!(load_firmware_dist(&missing, None), Ok(None)));

        // A valid dist loads with the manifest and bytes intact.
        let (dir, image) = write_dist("valid", "2026.7.0", "stable");
        let dist = load_firmware_dist(&dir, None).unwrap().expect("dist present");
        assert_eq!(dist.manifest.version, "2026.7.0");
        assert_eq!(dist.manifest.size as usize, image.len());
        assert_eq!(dist.manifest.sha256, sha256_hex(&image));
        assert_eq!(&dist.bytes[..], &image[..]);

        // Corrupt the image AFTER signing: sha mismatch -> hard error.
        std::fs::write(dir.join("firmware.bin"), b"tampered bytes!!!!!!!").unwrap();
        let err = load_firmware_dist(&dir, None).unwrap_err();
        assert!(err.contains("sha256") || err.contains("bytes"), "names the mismatch: {err}");

        // Wrong length (manifest size no longer matches) -> hard error too.
        std::fs::write(dir.join("firmware.bin"), b"short").unwrap();
        assert!(load_firmware_dist(&dir, None).is_err());

        // Half a dist (manifest without image) is broken, not absent.
        std::fs::remove_file(dir.join("firmware.bin")).unwrap();
        assert!(load_firmware_dist(&dir, None).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// GET /firmware/{channel}/latest serves that channel's manifest JSON and
    /// /firmware/{channel}.bin its raw image, straight from an
    /// integrity-checked temp dist; absent channels and unknown names 404.
    #[tokio::test]
    async fn firmware_routes_round_trip() {
        let (dir, image) = write_dist("routes", "2026.7.0", "stable");
        let mut state = AppState::new_ephemeral();
        state
            .firmware
            .insert(FwChannel::Stable, Arc::new(load_firmware_dist(&dir, None).unwrap().unwrap()));

        let latest = |s: &AppState, chan: &str| {
            get_firmware_latest(State(s.clone()), axum::extract::Path(chan.to_string()))
        };
        let bin = |s: &AppState, file: &str| {
            get_firmware_bin(
                State(s.clone()),
                axum::extract::Path(file.to_string()),
                axum::extract::Query(FwDownloadQuery { u: None }),
            )
        };

        let resp = latest(&state, "stable").await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert_eq!(
            resp.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let m: FirmwareManifest = serde_json::from_slice(&body).unwrap();
        assert_eq!(m.version, "2026.7.0");
        assert_eq!(m.size as usize, image.len());
        assert_eq!(m.sha256, sha256_hex(&image));
        assert_eq!(m.sig, "ab".repeat(64));

        let resp = bin(&state, "stable.bin").await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert_eq!(
            resp.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        assert_eq!(&body[..], &image[..], "served image is byte-identical");

        // A channel with nothing hosted 404s — even though stable is live.
        assert_eq!(latest(&state, "beta").await.status(), axum::http::StatusCode::NOT_FOUND);
        assert_eq!(bin(&state, "beta.bin").await.status(), axum::http::StatusCode::NOT_FOUND);
        assert_eq!(bin(&state, "dev.bin").await.status(), axum::http::StatusCode::NOT_FOUND);
        // Unknown channel names / malformed files 404 too.
        assert_eq!(latest(&state, "nightly").await.status(), axum::http::StatusCode::NOT_FOUND);
        assert_eq!(bin(&state, "nightly.bin").await.status(), axum::http::StatusCode::NOT_FOUND);
        assert_eq!(bin(&state, "stable").await.status(), axum::http::StatusCode::NOT_FOUND);
        assert_eq!(bin(&state, "stable.json").await.status(), axum::http::StatusCode::NOT_FOUND);

        // No dists hosted at all -> plain 404s.
        let bare = AppState::new_ephemeral();
        assert_eq!(latest(&bare, "stable").await.status(), axum::http::StatusCode::NOT_FOUND);
        assert_eq!(bin(&bare, "stable.bin").await.status(), axum::http::StatusCode::NOT_FOUND);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two per-channel firmware routes coexist in one Router (a matchit
    /// conflict would panic right here at registration) and dispatch correctly
    /// over real HTTP: "/firmware/stable.bin" reaches the bin handler,
    /// "/firmware/stable/latest" the manifest handler.
    #[tokio::test]
    async fn firmware_routes_register_and_dispatch() {
        let (dir, image) = write_dist("dispatch", "2026.7.0", "stable");
        let mut state = AppState::new_ephemeral();
        state
            .firmware
            .insert(FwChannel::Stable, Arc::new(load_firmware_dist(&dir, None).unwrap().unwrap()));
        let app = Router::new()
            .route("/firmware/:chan/latest", get(get_firmware_latest))
            .route("/firmware/:file", get(get_firmware_bin))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let bin = reqwest::get(format!("http://{addr}/firmware/stable.bin")).await.unwrap();
        assert_eq!(bin.status(), 200);
        assert_eq!(bin.bytes().await.unwrap().as_ref(), &image[..]);

        let latest = reqwest::get(format!("http://{addr}/firmware/stable/latest")).await.unwrap();
        assert_eq!(latest.status(), 200);
        let m: FirmwareManifest = latest.json().await.unwrap();
        assert_eq!(m.version, "2026.7.0");

        let absent = reqwest::get(format!("http://{addr}/firmware/beta.bin")).await.unwrap();
        assert_eq!(absent.status(), 404);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Multi-channel loading: absent channels are off, present channels load,
    /// and ONE corrupt channel is a hard error even with a valid channel
    /// alongside (never start half-broken). The old flat layout is refused
    /// loudly rather than silently unserved.
    #[test]
    fn firmware_dists_load_per_channel() {
        let base = std::env::temp_dir()
            .join(format!("scoreboard-fw-test-channels-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        // Empty base dir: every channel off, not an error.
        assert!(load_firmware_dists(&base).unwrap().is_empty());

        // stable + dev hosted, beta absent -> exactly those two load.
        for (chan, version) in [("stable", "2026.7.0"), ("dev", "2026.8.0")] {
            let (dir, _) = write_dist(&format!("channels-{chan}"), version, chan);
            std::fs::rename(&dir, base.join(chan)).unwrap();
        }
        let dists = load_firmware_dists(&base).unwrap();
        assert_eq!(dists.len(), 2);
        assert_eq!(dists.get(&FwChannel::Stable).unwrap().manifest.version, "2026.7.0");
        assert_eq!(dists.get(&FwChannel::Dev).unwrap().manifest.version, "2026.8.0");
        assert!(!dists.contains_key(&FwChannel::Beta));

        // Corrupt ONE channel (dev's image no longer matches its manifest):
        // the whole load fails, valid stable notwithstanding.
        std::fs::write(base.join("dev").join("firmware.bin"), b"tampered!!!").unwrap();
        let err = load_firmware_dists(&base).unwrap_err();
        assert!(err.contains("dev"), "error names the broken channel: {err}");

        // A dist signed for one channel hosted in another's directory is
        // refused at startup (its signature binds the channel; it could never
        // verify on-device as the channel it's masquerading in).
        let _ = std::fs::remove_dir_all(base.join("dev"));
        let (moved, _) = write_dist("channels-moved", "2026.9.0", "dev");
        std::fs::rename(&moved, base.join("beta")).unwrap();
        let err = load_firmware_dists(&base).unwrap_err();
        assert!(
            err.contains("cannot move between channels"),
            "cross-channel move refused: {err}"
        );
        let _ = std::fs::remove_dir_all(base.join("beta"));

        // Flat pre-channel files at the base are a refusal, not a silent skip.
        let _ = std::fs::remove_dir_all(&base);
        let (flat, _) = write_dist("channels-flat", "2026.7.0", "stable");
        let err = load_firmware_dists(&flat).unwrap_err();
        assert!(err.contains("flat"), "error explains the layout problem: {err}");
        let _ = std::fs::remove_dir_all(&flat);
    }

    /// The pinned offer matrix, exhaustively: each channel sees only its
    /// allowed set (stable ⊆ beta ⊆ dev), the NEWEST allowed dist wins, and
    /// nothing is offered unless that pick is strictly newer than what the
    /// board runs.
    #[test]
    fn firmware_offer_matrix() {
        use FwChannel::*;
        let dist = |version: &str| {
            let image = format!("\u{e9}img {version}").into_bytes();
            Arc::new(FirmwareDist {
                manifest: FirmwareManifest {
                    version: version.into(),
                    // The offer path/channel come from the MAP KEY, not this
                    // field (which only load-time validation reads) — the
                    // matrix test doesn't care what it says.
                    channel: String::new(),
                    size: image.len() as u32,
                    sha256: sha256_hex(&image),
                    sig: "ab".repeat(64),
                },
                bytes: image.into(),
            })
        };
        let host = |entries: &[(FwChannel, &str)]| -> HashMap<FwChannel, Arc<FirmwareDist>> {
            entries.iter().map(|(c, v)| (*c, dist(v))).collect()
        };
        // offer() -> Some((dist channel, dist version)) for compact asserts.
        let offer = |dists: &HashMap<FwChannel, Arc<FirmwareDist>>, chan, running: &str| {
            offer_for(dists, chan, running).map(|(c, fw)| (c, fw.manifest.version.clone()))
        };

        // Newer dists on every channel: each board channel picks the newest
        // dist it is ALLOWED to see — stable never sees beta/dev.
        let all = host(&[(Stable, "2026.7.0"), (Beta, "2026.8.0"), (Dev, "2026.9.0")]);
        assert_eq!(offer(&all, Stable, "2026.6.0"), Some((Stable, "2026.7.0".into())));
        assert_eq!(offer(&all, Beta, "2026.6.0"), Some((Beta, "2026.8.0".into())));
        assert_eq!(offer(&all, Dev, "2026.6.0"), Some((Dev, "2026.9.0".into())));

        // Running >= the best allowed pick: no offer — even when a channel the
        // board can NOT see hosts something newer.
        assert_eq!(offer(&all, Stable, "2026.7.0"), None, "beta/dev newer, but invisible");
        assert_eq!(offer(&all, Beta, "2026.8.0"), None, "dev newer, but invisible");
        assert_eq!(offer(&all, Dev, "2026.9.0"), None);
        assert_eq!(offer(&all, Dev, "2026.10.0"), None, "running ahead of everything");

        // Newest-wins WITHIN the allowed set: when stable outruns beta, the
        // beta/dev boards are offered the stable dist (and its stable path).
        let stable_ahead = host(&[(Stable, "2026.9.0"), (Beta, "2026.8.0"), (Dev, "2026.7.0")]);
        assert_eq!(offer(&stable_ahead, Beta, "2026.8.0"), Some((Stable, "2026.9.0".into())));
        assert_eq!(offer(&stable_ahead, Dev, "2026.7.0"), Some((Stable, "2026.9.0".into())));
        assert_eq!(offer(&stable_ahead, Stable, "2026.8.0"), Some((Stable, "2026.9.0".into())));

        // Absent channels drop out of the pick: only a dev dist hosted means
        // stable/beta boards get nothing, however stale they are.
        let dev_only = host(&[(Dev, "2026.9.0")]);
        assert_eq!(offer(&dev_only, Stable, "2026.1.0"), None);
        assert_eq!(offer(&dev_only, Beta, "2026.1.0"), None);
        assert_eq!(offer(&dev_only, Dev, "2026.1.0"), Some((Dev, "2026.9.0".into())));

        // No dists at all / junk versions: never an offer.
        assert_eq!(offer(&HashMap::new(), Dev, "2026.1.0"), None);
        assert_eq!(offer(&all, Dev, "junk"), None, "unparseable running version");
        let junk_dist = host(&[(Stable, "not-calver")]);
        assert_eq!(offer(&junk_dist, Stable, "2026.1.0"), None, "unparseable dist version");
    }
}
