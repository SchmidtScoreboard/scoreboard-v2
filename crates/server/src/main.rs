//! Local scoreboard server. Pulls live games from ESPN, stores per-board
//! preferences (favorite teams etc.), and renders a per-board rotation filtered
//! by those preferences. Talks to boards + controllers over a websocket.

mod alerts;
mod bdf;
mod espn;
mod model;
mod offmode;
mod laps;
mod openf1;
mod weather;
mod render;
mod serve;
mod team_names;
mod teams;
mod tls;
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

/// Reserved pairing code for App Store / Play Store review. Typed into the
/// normal pairing screen it pairs the controller to a server-side VIRTUAL demo
/// board — no device ever connects; the board exists only as a config that
/// `config_for` forces onto the demo slate. Documented in the stores' review
/// notes, nowhere user-facing. `register_pairing_code` refuses to index this
/// code for a real board, so a TRNG collision can't shadow the demo.
pub const DEMO_PAIR_CODE: &str = "776677";

/// Marker prefix for virtual demo board ids. Unforgeable from the device side:
/// a board id IS its Ed25519 pubkey and must sign the auth challenge, and
/// `demo-…` is not valid pubkey hex. (Demo GAME ids are also `demo-*` —
/// different namespace, the two never share a map.)
pub const DEMO_BOARD_PREFIX: &str = "demo-";

pub fn is_demo_board(board_id: &str) -> bool {
    board_id.starts_with(DEMO_BOARD_PREFIX)
}

/// Per-CONTROLLER demo board id, so two reviewers never share config. The
/// pubkey is 64 hex by the time any caller has it (it signed the challenge),
/// but slice defensively anyway.
pub fn demo_board_id(controller_pubkey: &str) -> String {
    let tag = controller_pubkey.get(..12).unwrap_or(controller_pubkey);
    format!("{DEMO_BOARD_PREFIX}{tag}")
}

#[derive(Clone)]
pub struct AppState {
    pub catalog: Arc<Catalog>,
    pub games: Arc<Mutex<Vec<Game>>>,
    /// False until the first poll has WRITTEN a slate. "Never fetched" and
    /// "fetched and empty" must be distinguishable: a board reconnecting into
    /// a freshly restarted server was pushed the idle screen out of boot
    /// emptiness, blinked to weather for the seconds until the first sweep
    /// landed, then blinked back — once per deploy, live games and all.
    pub slate_ready: Arc<std::sync::atomic::AtomicBool>,
    /// The synthetic slate that boards with `demo_data` read, regenerated every
    /// poll alongside the live one. Seeded at tick 0 so `/demo/*` and a board
    /// that flips the toggle before the first poll are never blank.
    pub demo: Arc<Mutex<Vec<Game>>>,
    /// Owns the demo generator's clocks, so the fake slate advances on its own
    /// schedule rather than restarting on every read.
    pub demo_source: Arc<espn::DemoSource>,
    /// Weather by location string, kept warm by `weather_loop`. Renderers read
    /// it and never wait on it: a board must not be held up drawing a frame
    /// while an HTTP call to a third party completes.
    pub weather: Arc<weather::Cache>,
    /// Liveness heartbeats, one per background loop.
    ///
    /// The outage that motivated these was not a crash: the poll loop wedged on
    /// a lock and the process kept running, answering nothing, logging nothing.
    /// A plain "is the port open" check would have caught THAT one, but not the
    /// general case of a loop dying while HTTP still serves — so each loop
    /// stamps its own beat and `/health` reports the ages.
    pub beats: Arc<Mutex<HashMap<&'static str, Instant>>>,
    /// Rendered frames by content id, stamped with the eviction generation they
    /// were minted in (so `evict_stale_frames` never drops a frame between its
    /// render and its manifest registration — that write pair isn't atomic).
    pub frames: Arc<Mutex<HashMap<FrameId, (u64, Vec<u8>)>>>,
    pub configs: Arc<Mutex<HashMap<String, BoardConfig>>>, // board_id -> prefs
    /// Boards whose stored config could not be parsed at startup. They are
    /// QUARANTINED: `persist_config` refuses to write for them, so a row this
    /// binary cannot read is never overwritten by the default it fell back to.
    ///
    /// Losing a config in memory is recoverable — roll the server back and the
    /// row is still there. Overwriting it is not, and that is what used to
    /// happen: a drop here handed out `BoardConfig::default()`, and the next
    /// command upserted that default over the user's name, favorites,
    /// brightness and timezone with no error anywhere.
    pub unparsed: Arc<Mutex<HashSet<String>>>,
    pub devices: Arc<Mutex<HashMap<String, UnboundedSender<Message>>>>, // board_id -> sink
    pub controllers: Arc<Mutex<HashMap<String, Vec<UnboundedSender<Message>>>>>, // board_id -> app sinks
    /// Live pairing codes: code -> (board_id, when the board registered it).
    /// The stamp expires the code (`PAIR_CODE_TTL`) — see `register_pairing_code`.
    pub pending: Arc<Mutex<HashMap<String, (String, Instant)>>>,
    /// OpenF1 alternate feed — Some only when the environment carries the
    /// personal-use credentials. See `crates/server/src/openf1.rs`.
    pub openf1: Arc<Option<openf1::OpenF1>>,
    /// The latest OpenF1 session rows, swapped into f1 frames by `board_view`
    /// for boards that opted in. None until the first successful fetch.
    pub openf1_rows: Arc<Mutex<Option<openf1::SessionRows>>>,
    pub allow: Arc<Mutex<HashMap<String, HashSet<String>>>>, // board_id -> controller ids
    db: Option<Arc<Mutex<Connection>>>,
    /// Pairing-attempt budget per controller pubkey. One shared bucket meant a
    /// single attacker's guessing spree locked every other phone out of pairing.
    pair_limiters: Arc<Mutex<HashMap<String, RateLimiter>>>,
    /// Backstop above the per-identity buckets (fresh pubkeys are free).
    pair_limiter: Arc<Mutex<RateLimiter>>,
    /// Wakes the poll loop when a client connects, so it fetches fresh games promptly
    /// instead of up to POLL_SECS later (the idle-skip otherwise leaves games stale).
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
    pub finished: Arc<Mutex<HashMap<String, Finish>>>,
    /// Recent device console lines per board, newest last. Only populated while
    /// a board has been asked to stream (`ServerMsg::SetDebugLog`), and bounded
    /// — this is a debugging window, not a log service, and a board left
    /// streaming must not grow the server without limit.
    pub debug_logs: Arc<Mutex<HashMap<String, std::collections::VecDeque<String>>>>,
    /// Capability tokens for reading those logs:
    /// nonce -> (board_id, session_ends, readable_until).
    ///
    /// The log endpoint is unauthenticated HTTP — there is no session to check —
    /// so the URL itself has to be the credential. A board id would be a poor
    /// one: it is long-lived, it is printed on the device's own console, and it
    /// is the board's PUBLIC KEY, which is not a secret in any other context.
    /// A nonce that dies with the debug window is.
    pub debug_tokens: Arc<Mutex<HashMap<String, (String, std::time::Instant, std::time::Instant)>>>,
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
    /// resolves it to scope the download to that one board.
    /// The nonce is deliberately UNSIGNED (only version/size/channel/bytes are
    /// signed — the board verifies the image regardless). Entries older than
    /// `FW_TOKEN_TTL` are swept lazily on every touch (offers the board never
    /// acted on, downloads that never finished). Un-tokenized GETs never enter
    /// this map and emit no progress.
    pub fw_download_tokens: Arc<Mutex<HashMap<String, (String, std::time::Instant)>>>,
    /// Where per-version user-facing patch notes live
    /// (`{FIRMWARE_DIR}/notes/{version}.txt` — see `get_firmware_notes`).
    /// Unlike the firmware dists these are NOT loaded at startup: they carry
    /// no integrity requirement, they're tiny, and they're fetched rarely, so
    /// a per-request read keeps them cache- and restart-ordering-free.
    pub fw_notes_dir: PathBuf,
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
}

impl AppState {
    #[cfg(test)]
    fn new_ephemeral() -> Self {
        let state = Self::new_inner(None, mock_catalog());
        // Tests exercise selection/rotation against a known slate — which also
        // means the slate is KNOWN, so the boot hold does not apply.
        *state.games.lock().unwrap() = espn::mock_games();
        state.slate_ready.store(true, std::sync::atomic::Ordering::Relaxed);
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
        let (mut configs, unparsed, allow) = match &db {
            Some(conn) => {
                let (cfgs, bad) = load_configs(conn);
                (cfgs, bad, load_pairings(conn))
            }
            None => (HashMap::new(), HashSet::new(), HashMap::new()),
        };
        sanitize_configs(&catalog, &mut configs);
        sanitize_off_modes(&mut configs);
        sanitize_screens(&mut configs);
        migrate_champ_options(&mut configs);
        let state = AppState {
            demo: Arc::new(Mutex::new(espn::demo_games(0, 0))),
            demo_source: Arc::new(espn::DemoSource::new()),
            weather: Arc::new(weather::Cache::default()),
            beats: Arc::new(Mutex::new(HashMap::new())),
            catalog: Arc::new(catalog),
            // Empty until the first real fetch — boards show their no-games
            // behavior rather than fake placeholder games (but see
            // `slate_ready`: until that fetch, manifests HOLD rather than
            // claim idleness).
            games: Arc::new(Mutex::new(Vec::new())),
            slate_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            frames: Arc::new(Mutex::new(HashMap::new())),
            configs: Arc::new(Mutex::new(configs)),
            unparsed: Arc::new(Mutex::new(unparsed)),
            devices: Arc::new(Mutex::new(HashMap::new())),
            controllers: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            openf1: Arc::new(openf1::OpenF1::from_env()),
            openf1_rows: Arc::new(Mutex::new(None)),
            allow: Arc::new(Mutex::new(allow)),
            db: db.map(|c| Arc::new(Mutex::new(c))),
            pair_limiters: Arc::new(Mutex::new(HashMap::new())),
            pair_limiter: Arc::new(Mutex::new(RateLimiter::new(
                PAIR_GLOBAL_BURST,
                PAIR_GLOBAL_REFILL_PER_SEC,
            ))),
            notices: Arc::new(Mutex::new(HashMap::new())),
            last_manifest: Arc::new(Mutex::new(HashMap::new())),
            frame_grace: Arc::new(Mutex::new(HashSet::new())),
            frame_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            finished: Arc::new(Mutex::new(HashMap::new())),
            debug_logs: Arc::new(Mutex::new(HashMap::new())),
            debug_tokens: Arc::new(Mutex::new(HashMap::new())),
            firmware: HashMap::new(),
            device_versions: Arc::new(Mutex::new(HashMap::new())),
            fw_download_tokens: Arc::new(Mutex::new(HashMap::new())),
            // Matches FIRMWARE_DIR's default; main() re-points it when the
            // env var is set (mirroring how `state.firmware` is filled in).
            fw_notes_dir: PathBuf::from("./firmware-dist/notes"),
        };
        // Live debug sessions survive a deploy; see DEBUG_SESSIONS_KEY.
        load_debug_sessions(&state);
        state
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
        let mut cfg = self
            .configs
            .lock()
            .unwrap()
            .get(board_id)
            .cloned()
            .unwrap_or_default();
        // A virtual demo board is demo_data by IDENTITY, not by toggle: a
        // reviewer flipping the app's demo switch must never strand it on the
        // (possibly empty) real slate. Forced here — the one choke point every
        // reader funnels through — so `with_slate` stays the only reader of
        // the flag itself.
        if is_demo_board(board_id) {
            cfg.demo_data = true;
        }
        cfg
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
        // Never write over a row this binary could not read — see `unparsed`.
        if self.unparsed.lock().unwrap().contains(board_id) {
            tracing::warn!("refusing to overwrite unreadable config for board {board_id}");
            return;
        }
        let Ok(json) = serde_json::to_string(cfg) else { return };
        let _ = db.lock().unwrap().execute(
            "INSERT INTO boards (pubkey, config) VALUES (?1, ?2)
             ON CONFLICT(pubkey) DO UPDATE SET config = excluded.config",
            rusqlite::params![board_id, json],
        );
    }

    /// Read a small durable value. `None` when unset, or when there is no DB —
    /// callers treat both the same, because "we cannot remember" and "there is
    /// nothing to remember" lead to the same conservative behaviour.
    pub fn kv_get(&self, k: &str) -> Option<String> {
        let db = self.db.as_ref()?;
        let conn = db.lock().unwrap();
        conn.query_row("SELECT v FROM kv WHERE k = ?1", rusqlite::params![k], |r| r.get(0)).ok()
    }

    /// Write a small durable value. Silently a no-op without a DB.
    pub fn kv_set(&self, k: &str, v: &str) {
        let Some(db) = &self.db else { return };
        let _ = db.lock().unwrap().execute(
            "INSERT INTO kv (k, v) VALUES (?1, ?2)
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            rusqlite::params![k, v],
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

    /// Forget a board ENTIRELY — the factory-reset contract: every pairing
    /// (memory + DB), the config (memory + DB), the reported version, any
    /// notice, and any pending pairing codes. The board also wiped its own
    /// identity, so the old id never returns; this keeps the tables from
    /// accumulating orphans and makes every phone's grant on it dead.
    pub fn forget_board(&self, board_id: &str) {
        self.allow.lock().unwrap().remove(board_id);
        self.configs.lock().unwrap().remove(board_id);
        self.device_versions.lock().unwrap().remove(board_id);
        self.notices.lock().unwrap().remove(board_id);
        self.pending.lock().unwrap().retain(|_, (b, _)| b != board_id);
        if let Some(db) = &self.db {
            let db = db.lock().unwrap();
            let _ = db.execute("DELETE FROM pairings WHERE board_pubkey = ?1", rusqlite::params![board_id]);
            let _ = db.execute("DELETE FROM boards WHERE pubkey = ?1", rusqlite::params![board_id]);
        }
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
    ///
    /// The stamp is REFRESHED for connected boards by `tick_once`. Without
    /// that, the TTL was measured from the board's last reconnect while the
    /// panel went on displaying the code forever: a board up for more than ten
    /// minutes showed a code the server had already forgotten, and typing it
    /// in answered "invalid or expired". The map still holds at most one code
    /// per board, so keeping a CONNECTED board's live costs no extra guessable
    /// space — and it dies ten minutes after the board goes away.
    pub fn register_pairing_code(&self, code: String, board_id: &str) {
        // A board whose TRNG happens to mint the reserved demo code (1-in-10⁶
        // per boot) must not be indexed under it: the Pair intercept would win
        // anyway and hand its owner the DEMO board instead of their own,
        // silently. Refused here, the collision is logged and the board is
        // simply unpairable until its next boot re-mints.
        if code == DEMO_PAIR_CODE {
            tracing::warn!("board '{board_id}' minted the reserved demo pairing code; not registering it");
            return;
        }
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
         );
         CREATE TABLE IF NOT EXISTS kv (
            k TEXT PRIMARY KEY,
            v TEXT NOT NULL
         );",
    )?;
    Ok(conn)
}

/// Drop config leftovers that no longer make sense against the catalog:
/// team favorites for leagues that have NO teams (PGA/LPGA/F1 became
/// option-subscription leagues, orphaning their old pseudo-teams). Sports
/// absent from the catalog are left untouched — the catalog may be the small
/// mock fallback, and deleting real preferences against it would be data loss.
/// Reset any board parked on an off mode this binary does not offer — a fork
/// that removed a mode, or a settings import from a different server. Without
/// this the board keeps a dangling id: `render_off_mode` falls back to the
/// clock so the panel is fine, but the app would show no row selected.
/// Fold each league's legacy single option into its round SET, once, at load.
///
/// Must run BEFORE anything reads the set: `board_prefs` treats an empty set as
/// "no championship preferences", which flips a board to UNCONFIGURED and sets
/// it sampling the entire live slate. A board that had picked "Selected +
/// playoffs" would suddenly be showing everything.
///
/// Idempotent, because the projection is `None` exactly when the set is empty:
/// a second pass finds a non-empty set and leaves it alone.
fn migrate_champ_options(configs: &mut HashMap<String, BoardConfig>) {
    for (board, cfg) in configs.iter_mut() {
        for sc in cfg.sports.iter_mut() {
            if !sc.championship_option_ids.is_empty() {
                continue;
            }
            let expanded = expand_legacy_champ(&sc.sport_id, sc.championship_option_id.as_deref());
            if expanded.is_empty() {
                continue;
            }
            tracing::info!(
                "board {board}: {} big games {:?} -> {:?}",
                sc.sport_id,
                sc.championship_option_id,
                expanded
            );
            sc.championship_option_id = legacy_champ_projection(&sc.sport_id, &expanded);
            sc.championship_option_ids = expanded;
        }
    }
}

fn sanitize_off_modes(configs: &mut HashMap<String, BoardConfig>) {
    for (board, cfg) in configs.iter_mut() {
        if let Some(id) = cfg.off_mode.clone() {
            // The four weather layouts became one mode with a `variant` field.
            // Migrate BEFORE deciding an id is unknown, or every board that had
            // picked one would silently revert to the clock.
            let migrated = offmode::migrate(&id, &mut cfg.off_mode_params);
            if migrated != id {
                tracing::info!("board {board}: off mode {id:?} -> {migrated:?}");
                cfg.off_mode = Some(migrated.clone());
            }
            if !offmode::offers(&migrated) {
                tracing::warn!("board {board}: off mode {migrated:?} is not offered here; reverting");
                cfg.off_mode = None;
            }
        }
    }
}

/// MAR-117 load pass, after `sanitize_off_modes`' legacy flat pass. In memory
/// only, like every load sanitize; idempotent.
///
/// - Every board: a stored mode "blank" (a picked dark idle screen, which
///   older servers accepted) becomes "off" — dark stays dark, the wake time is
///   kept, and the board stops holding a "mode" nobody can see or edit.
/// - A board that saved a list: normalize it against THIS binary's off modes
///   (a fork or a rollback can have changed them) and re-derive its legacy
///   twins. A held kind a rolled-back server wrote as a bare mode is
///   canonicalized to the instance it resolves to; a held screen that no
///   longer exists releases the board to Sports.
/// - Every board: timers no screen owns are dropped (`prune_screen_timers`) —
///   instance keys a rollback left behind, which a screen added later under
///   the same id would otherwise inherit.
fn sanitize_screens(configs: &mut HashMap<String, BoardConfig>) {
    let catalog = offmode::off_modes();
    for (board, cfg) in configs.iter_mut() {
        if cfg.mode == OFF_MODE_BLANK {
            tracing::info!("board {board}: held mode \"blank\" -> \"off\"");
            cfg.set_mode(MODE_OFF);
        }
        if !cfg.screens.is_empty() && !cfg.set_screens(cfg.screens.clone(), &catalog) {
            let held = cfg.held_screen_id().to_string();
            let canonical = match cfg.mode_screen {
                None => resolve_screen(&cfg.screens, &held).map(|s| (s.id.clone(), s.kind.clone())),
                Some(_) => None,
            };
            match canonical {
                Some((id, kind)) => {
                    tracing::info!("board {board}: held {held:?} -> screen {id:?}");
                    cfg.set_held_screen(&id, &kind);
                }
                None => {
                    tracing::warn!("board {board}: held screen {held:?} is not on its list; back to Sports");
                    release_to_scores(cfg);
                }
            }
        }
        cfg.prune_screen_timers(&catalog);
    }
}

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
/// Returns the parsed configs and the pubkeys whose stored JSON this binary
/// could NOT read. The second half matters as much as the first: an unreadable
/// row used to vanish silently, and the default that replaced it was then
/// persisted straight over the original. Callers quarantine the returned ids.
fn load_configs(conn: &Connection) -> (HashMap<String, BoardConfig>, HashSet<String>) {
    let mut out = HashMap::new();
    let mut bad = HashSet::new();
    let Ok(mut stmt) = conn.prepare("SELECT pubkey, config FROM boards") else {
        return (out, bad);
    };
    let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) else {
        return (out, bad);
    };
    for (pubkey, json) in rows.flatten() {
        match serde_json::from_str::<BoardConfig>(&json) {
            Ok(mut cfg) => {
                migrate_legacy_row(&mut cfg);
                out.insert(pubkey, cfg);
            }
            Err(e) => {
                // Loud, because the alternative is a board that silently comes
                // back factory-fresh after a deploy.
                tracing::error!("board {pubkey}: stored config is unreadable ({e}); \
                                 quarantining it — settings preserved on disk, \
                                 board runs on defaults until this is fixed");
                bad.insert(pubkey);
            }
        }
    }
    (out, bad)
}

/// A row written by a pre-mode binary while ASLEEP: power=false with `mode`
/// at its serde default. Unreachable for new writes (`set_mode` keeps the twin
/// in sync), so the combination uniquely identifies a legacy sleeping board —
/// map it to "off" and KEEP wake_at, so the sweep in `tick_once` honours the
/// wake time the old binary promised. Idempotent and restart-safe.
fn migrate_legacy_row(cfg: &mut BoardConfig) {
    if !cfg.power && cfg.mode == MODE_SCORES {
        cfg.mode = MODE_OFF.to_string();
    }
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
    // Prove the mail path end to end without waiting for an incident or for the
    // daily hour. Exits with a non-zero status when it fails, so it is usable
    // as a check rather than just a thing that prints.
    if std::env::args().any(|a| a == "--test-alert") {
        match alerts::Mailer::from_env() {
            None => {
                eprintln!(
                    "email alerts are not configured.\n\
                     Set RESEND_API_KEY and ALERT_EMAIL_TO (optionally ALERT_EMAIL_FROM).\n\
                     Run against the deployed service's variables with:\n\
                     \n    railway run -s live cargo run -p server -- --test-alert\n"
                );
                std::process::exit(1);
            }
            Some(m) => {
                println!("sending a test alert to {}...", m.recipient());
                match m
                    .send(
                        "OpenScoreboard: test alert",
                        "This is a test from `server --test-alert`.\n\n\
                         If you are reading it, alerting is wired up: you will get one \
                         email when a problem is confirmed, one when it clears, and a \
                         report once a day.\n",
                    )
                    .await
                {
                    Ok(()) => {
                        println!("sent. If it does not arrive, check spam, and check that");
                        println!("ALERT_EMAIL_FROM is on a domain verified with Resend.");
                    }
                    Err(e) => {
                        eprintln!("FAILED: {e}");
                        eprintln!(
                            "\nCommon causes:\n\
                             - 401/403: the API key is wrong or lacks sending access\n\
                             - 403 with 'domain is not verified': ALERT_EMAIL_FROM is not on\n\
                               a domain you have verified with Resend\n\
                             - 422: the from/to address is malformed"
                        );
                        std::process::exit(1);
                    }
                }
                return;
            }
        }
    }

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

    // Demo data is a PER-BOARD setting now (`BoardConfig::demo_data`), served
    // from a second slate this same process keeps beside the live one. There is
    // no process-wide demo mode and no separate demo deployment: one server,
    // one catalog, and a toggle in the app.
    let source: Arc<dyn SportsDataSource> = Arc::new(espn::EspnSource::new());
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
    // Off modes belong to THIS BINARY, not to the team data, so they are filled
    // in here — once, after whichever source produced the catalog.
    //
    // They used to be set by the fetch path only. A server whose ESPN fetch
    // failed fell back to the disk cache, whose JSON predates the field, and
    // `#[serde(default)]` quietly handed the app an EMPTY list: every board
    // showed only the two modes the app hardcodes as a fallback, and the real
    // ones were invisible. A value that depends on which code path built the
    // struct is the bug; this is the fix.
    let catalog = with_binary_owned(catalog);
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
    // Patch notes ride alongside the dists in the same FIRMWARE_DIR.
    state.fw_notes_dir = fw_dir.join("notes");
    tokio::spawn(poll_loop(state.clone(), source));
    tokio::spawn(weather_loop(state.clone()));
    // Email alerting is opt-in: with nothing configured the server simply does
    // not start these, and says so once rather than warning forever.
    match alerts::Mailer::from_env() {
        Some(_) => {
            tracing::info!("email alerts enabled");
            if let Some(m) = alerts::Mailer::from_env() {
                tokio::spawn(alerts::watchdog(state.clone(), m));
            }
            if let Some(m) = alerts::Mailer::from_env() {
                tokio::spawn(alerts::daily_report(state.clone(), m));
            }
        }
        None => tracing::info!(
            "email alerts off (set RESEND_API_KEY and ALERT_EMAIL_TO to enable)"
        ),
    }
    tokio::spawn(tick_loop(state.clone()));

    let app = Router::new()
        .route("/health", get(get_health))
        .route("/debug/:token", get(get_debug_log))
        .route("/catalog", get(get_catalog))
        .route("/render/:id", get(get_render))
        .route("/game/:id", get(get_game))
        .route("/sample/:variant", get(get_sample))
        .route("/offmode/:id", get(get_offmode))
        .route("/demo/games", get(get_demo_games))
        .route("/demo/render/:id", get(get_demo_render))
        // The app's troubleshooting-sheet images: real board screens rendered
        // server-side by board-core, so they can never drift from the device.
        .route("/guide/:name", get(get_guide))
        // Per-channel OTA routes: "/firmware/stable.bin" + "/firmware/stable/latest"
        // (and beta/dev). No un-channeled aliases — no OTA-capable board ever
        // shipped speaking the old flat paths.
        .route("/firmware/:chan/latest", get(get_firmware_latest))
        .route("/firmware/:file", get(get_firmware_bin))
        // Per-version user-facing patch notes. The static "notes" segment
        // outranks the :chan capture above, so this coexists cleanly.
        .route("/firmware/notes/:version", get(get_firmware_notes))
        .route("/ws", get(ws::ws_handler))
        .with_state(state);

    // A missing certificate degrades to plaintext rather than crash-looping the
    // service.
    let tls = tls::from_env();
    match &tls {
        Some(t) => tracing::info!(
            "TLS 1.3 available (board pin {}, max TLS record {} bytes on the wire)",
            t.pin,
            tls::MAX_RECORD_WIRE
        ),
        None => tracing::info!("no certificate configured — plaintext only"),
    }
    let tls = tls.map(|t| t.config);

    // Bind all interfaces so a real board on the LAN (or Railway's proxy) can
    // reach us; includes localhost, so the simulator/app on 127.0.0.1 still work.
    //
    // Railway reaches this container twice, and the two paths want OPPOSITE
    // policies (see serve.rs):
    //
    //   PORT                         the HTTPS edge's target port. Railway
    //                                terminates TLS out there and forwards
    //                                PLAINTEXT here, so this port must stay open.
    //   RAILWAY_TCP_APPLICATION_PORT the raw TCP proxy's target port. Bytes
    //                                arrive untouched from the public internet;
    //                                this is the board's port and it can demand
    //                                TLS.
    //
    // Both default to 7878 and today production has them EQUAL, which collapses
    // the two into one port that cannot enforce anything (rejecting plaintext
    // there would reject the app). That case is handled honestly — one listener,
    // PortRole::Shared — rather than faked. Splitting them is a Railway config
    // change, not a code change: point the domain at one port (`railway domain
    // --port`, or the edit icon beside the domain) and the TCP proxy at the
    // other, then set both variables to match.
    let edge_port = env_port("PORT", 7878);
    let raw_port = env_port("RAILWAY_TCP_APPLICATION_PORT", edge_port);

    if edge_port == raw_port {
        let addr = format!("0.0.0.0:{edge_port}");
        let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
        tracing::info!(
            "scoreboard server on {addr} — ONE port for both the HTTPS edge and the raw \
             TCP proxy, so plaintext cannot be refused there (PORT and \
             RAILWAY_TCP_APPLICATION_PORT are both {edge_port}); plaintext websocket \
             upgrades that did not come through the edge will be logged"
        );
        serve::serve(listener, app, tls, Default::default(), serve::PortRole::Shared).await;
        return;
    }

    let edge_addr = format!("0.0.0.0:{edge_port}");
    let raw_addr = format!("0.0.0.0:{raw_port}");
    let edge = tokio::net::TcpListener::bind(&edge_addr).await.unwrap();
    let raw = tokio::net::TcpListener::bind(&raw_addr).await.unwrap();
    tracing::info!(
        "scoreboard server on {edge_addr} (HTTPS edge, full plaintext router) and \
         {raw_addr} (raw TCP proxy: TLS required except the signed OTA image route)"
    );
    tokio::join!(
        serve::serve(edge, app.clone(), tls.clone(), Default::default(), serve::PortRole::Edge),
        serve::serve(raw, app, tls, Default::default(), serve::PortRole::Raw),
    );
}

/// A port from the environment, falling back to `default` when unset or not a
/// number. A typo in a Railway variable must not take the service down.
fn env_port(name: &str, default: u16) -> u16 {
    match std::env::var(name) {
        Ok(v) => v.parse().unwrap_or_else(|_| {
            tracing::warn!("{name}='{v}' is not a port number — using {default}");
            default
        }),
        Err(_) => default,
    }
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
/// OTA fetch, so stream it and retire the token when drained; absent -> a plain
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
fn tokenized_stream_body(
    app: AppState,
    nonce: String,
    bytes: axum::body::Bytes,
) -> axum::body::Body {
    struct St {
        app: AppState,
        nonce: String,
        bytes: axum::body::Bytes,
        offset: usize,
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
    let init = St { app, nonce, bytes, offset: 0 };
    let stream = futures_util::stream::unfold(init, |mut st| async move {
        let total = st.bytes.len();
        if st.offset >= total {
            return None; // St drops with the stream; its Drop reclaims the token
        }
        let end = (st.offset + FW_CHUNK).min(total);
        let chunk = st.bytes.slice(st.offset..end);
        st.offset = end;
        // Deliberately NO progress anywhere: bytes handed to this socket are
        // swallowed by the kernel send buffer and Railway's TCP proxy (they
        // read 100% while the board still writes flash), and device-reported
        // progress cost a concurrent socket the firmware's pool couldn't
        // hold. The app shows a plain spinner; the token still scopes the
        // download to one board and expires when the stream drains.
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
    // A token that resolves to a board makes this a single-use, board-scoped
    // OTA fetch (the token retires when the stream drains or aborts).
    match q.u.as_deref().and_then(|nonce| resolve_fw_token(&s, nonce).map(|b| (nonce.to_string(), b))) {
        Some((nonce, _board_id)) => {
            let len = fw.bytes.len();
            let body = tokenized_stream_body(s.clone(), nonce, fw.bytes.clone());
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

/// GET /firmware/notes/{version} — the user-facing patch notes for one
/// released firmware version, as plain-text "- " bullets (the app shows them
/// verbatim in its update sheet). Served from `{FIRMWARE_DIR}/notes/
/// {version}.txt`, read at request time — tiny files fetched rarely, so a
/// cache would only add invalidation problems. Plain-text 404 for a version
/// with no notes file.
///
/// SECURITY: {version} is a path segment that becomes a FILENAME. It is
/// validated to strictly ASCII digits and dots BEFORE any filesystem call —
/// anything else (traversal attempts, percent-encoded separators that axum
/// has already decoded by the time we see the string, null bytes) is a plain
/// 404, never a path lookup. The whitelist can't build a separator, and the
/// appended ".txt" means even an all-dots version is a plain filename.
async fn get_firmware_notes(
    State(s): State<AppState>,
    axum::extract::Path(version): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let not_found =
        || (axum::http::StatusCode::NOT_FOUND, "no notes for that version").into_response();
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return not_found();
    }
    match tokio::fs::read_to_string(s.fw_notes_dir.join(format!("{version}.txt"))).await {
        Ok(notes) => (
            [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            notes,
        )
            .into_response(),
        Err(_) => not_found(),
    }
}

/// A canned example frame for the app's spoiler-settings rows: the same game
/// rendered plain ("off"), spoiled-normal, or spoiled-strict, so the choice
/// shows exactly what the board will do. Variant is off|normal|strict.
/// The fixed demonstration game shared by the spoiler previews (`/sample`)
/// and the troubleshooting guide's "regular scores" screen (`/guide/game`):
/// one deterministic, obviously-generic frame, so both stay byte-stable.
fn sample_game() -> model::Game {
    let team = |abbrev: &str, name: &str, score: u32, p: &str, sec: &str| model::TeamSide {
        abbrev: abbrev.into(),
        name: name.into(),
        hits: None,
        errors: None,
        shots: None,
        shootout: None, aggregate: None,
        // Sample spoiler frame renders NHL (default Name style), so location is
        // unused; left empty. (Render agent: populate to preview Location here.)
        location: String::new(),
        score,
        primary: model::hex_to_rgb(p),
        secondary: model::ensure_contrast(model::hex_to_rgb(p), model::hex_to_rgb(sec)),
    };
    model::Game {
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
        series: None,
        session: None,
        situation: Some(model::Situation::Hockey {
            away_powerplay: false,
            home_powerplay: false,
            away_players: 3,
            home_players: 3,
        }),
        pregame: None,
    }
}

/// The two sides of a league's demonstration game: `(abbrev, mascot, location,
/// primary, secondary)` each, then the away and home score.
///
/// PER LEAGUE, not per sport. Sharing one fixture across a family previewed
/// NCAA football settings with the Packers and the Bears, offered "The Open" as
/// the example LPGA event, and showed Arsenal against PSG on the MLS screen —
/// on the screens whose entire job is "here is what YOUR league will look
/// like". The whole point of a preview is recognition.
///
/// Mascots are kept SHORT where the renderer might fall back: NHL and soccer
/// rows use the 5x8 font and `pair_style` drops to the abbreviation when a name
/// does not fit, which would make the Name and Abbrev previews pixel-identical.
struct SampleSides {
    away: (&'static str, &'static str, &'static str, &'static str, &'static str),
    home: (&'static str, &'static str, &'static str, &'static str, &'static str),
    scores: (u32, u32),
}

/// The fixture for each league. A league missing here falls back to its
/// family's shape via `sample_sides`, so a fork adding a league still previews
/// something rather than nothing.
fn sample_sides(sport_id: &str) -> SampleSides {
    let s = |away, home, scores| SampleSides { away, home, scores };
    match sport_id {
        // ---- hockey ----
        "nhl" => s(
            ("BOS", "BRUINS", "BOSTON", "FFB81C", "000000"),
            ("TOR", "LEAFS", "TORONTO", "00205B", "FFFFFF"),
            (3, 3),
        ),
        // Olympic hockey is countries, and ESPN gives it no colors at all — the
        // flag table is the same one the catalog falls back to.
        "olyhm" | "olyhw" => s(
            ("CAN", "CANADA", "CANADA", "D52B1E", "1D1D1B"),
            ("USA", "USA", "UNITED STATES", "0A3161", "B31942"),
            (2, 1),
        ),
        // ---- basketball ----
        "nba" => s(
            ("BOS", "CELTICS", "BOSTON", "007A33", "FFFFFF"),
            ("LAL", "LAKERS", "LOS ANGELES", "552583", "FDB927"),
            (88, 84),
        ),
        // Not the Aces, whose primary is pure black: the row fill IS the
        // primary, so a black one draws no band at all and the preview shows
        // half a card.
        "wnba" => s(
            ("SEA", "STORM", "SEATTLE", "2C5234", "FEE11A"),
            ("NY", "LIBERTY", "NEW YORK", "6ECEB2", "000000"),
            (79, 74),
        ),
        "cbb" => s(
            ("DUKE", "BLUE DEVILS", "DUKE", "001A57", "FFFFFF"),
            ("UNC", "TAR HEELS", "NORTH CAROLINA", "7BAFD4", "FFFFFF"),
            (71, 68),
        ),
        "wcbb" => s(
            ("UCONN", "HUSKIES", "CONNECTICUT", "000E2F", "FFFFFF"),
            ("SC", "GAMECOCKS", "SOUTH CAROLINA", "73000A", "000000"),
            (66, 63),
        ),
        "olybm" | "olybw" => s(
            ("USA", "USA", "UNITED STATES", "0A3161", "B31942"),
            ("FRA", "FRANCE", "FRANCE", "002395", "ED2939"),
            (87, 82),
        ),
        // ---- baseball ----
        "mlb" => s(
            ("DET", "TIGERS", "DETROIT", "0C2340", "FA4616"),
            ("CLE", "GUARDIANS", "CLEVELAND", "E31937", "0C2340"),
            (6, 4),
        ),
        "cbase" => s(
            ("LSU", "TIGERS", "LSU", "461D7C", "FDD023"),
            ("TENN", "VOLS", "TENNESSEE", "FF8200", "FFFFFF"),
            (5, 3),
        ),
        // ---- football ----
        "nfl" => s(
            ("GB", "PACKERS", "GREEN BAY", "203731", "FFB612"),
            ("CHI", "BEARS", "CHICAGO", "0B162A", "C83803"),
            (17, 24),
        ),
        "cfb" => s(
            ("MICH", "WOLVERINES", "MICHIGAN", "00274C", "FFCB05"),
            ("OSU", "BUCKEYES", "OHIO STATE", "BB0000", "666666"),
            (21, 27),
        ),
        // ---- soccer ----
        "mls" => s(
            ("LA", "GALAXY", "LOS ANGELES", "00245D", "FFD200"),
            ("SEA", "SOUNDERS", "SEATTLE", "5D9732", "005595"),
            (1, 1),
        ),
        // Arsenal red against Chelsea blue, not against Liverpool red: two
        // near-identical bands stacked on each other is a picture of one team.
        "epl" => s(
            ("ARS", "ARSENAL", "ARSENAL", "EF0107", "FFFFFF"),
            ("CHE", "CHELSEA", "CHELSEA", "034694", "FFFFFF"),
            (1, 1),
        ),
        "ucl" => s(
            ("RMA", "MADRID", "REAL MADRID", "FEBE10", "00529F"),
            ("PSG", "PSG", "PARIS", "004170", "E30613"),
            (1, 1),
        ),
        "wc" | "wwc" | "olysm" | "olysw" => s(
            ("ARG", "ARGENTINA", "ARGENTINA", "75AADB", "FFFFFF"),
            ("BRA", "BRAZIL", "BRAZIL", "FEDF00", "009B3A"),
            (1, 1),
        ),
        // Fallback: unknown leagues get a neutral pair rather than someone
        // else's teams.
        _ => s(
            ("BOS", "CELTICS", "BOSTON", "007A33", "FFFFFF"),
            ("LAL", "LAKERS", "LOS ANGELES", "552583", "FDB927"),
            (88, 84),
        ),
    }
}

/// The event a leaderboard league previews with. Golf's two tours do not share
/// majors — offering "The Open" as the LPGA example is showing someone a
/// tournament their tour does not play.
fn sample_leaderboard(sport_id: &str) -> (&'static str, Vec<(&'static str, &'static str)>) {
    match sport_id {
        "lpga" => (
            "US WOMENS OPEN",
            vec![("KO", "-11"), ("KORDA", "-9"), ("THITIKUL", "-7"), ("YIN", "-6")],
        ),
        "f1" => (
            "MONACO GP",
            vec![("VER", "LAP 42"), ("NOR", "+2.1"), ("LEC", "+5.8"), ("HAM", "+9.4")],
        ),
        _ => (
            "THE OPEN",
            vec![("SCHEFFLER", "-12"), ("MCILROY", "-9"), ("FLEETWOOD", "-8"), ("MORIKAWA", "-7")],
        ),
    }
}

/// A demonstration game for ONE league, so a settings preview shows something
/// the user recognises: hits and errors on a baseball final, shots on a live
/// NHL frame, a shootout on a soccer knockout.
///
/// Locations are populated here where `sample_game`'s are not, because the
/// name-style preview is exactly the thing that needs them — a Location preview
/// against an empty location would show a blank row and read as a bug.
fn sample_game_for(sport_id: &str, extras: bool, live: bool) -> model::Game {
    let side = |t: (&str, &str, &str, &str, &str), score: u32| model::TeamSide {
        abbrev: t.0.into(),
        name: t.1.into(),
        location: t.2.into(),
        score,
        primary: model::hex_to_rgb(t.3),
        secondary: model::ensure_contrast(model::hex_to_rgb(t.3), model::hex_to_rgb(t.4)),
        hits: None,
        errors: None,
        shots: None,
        shootout: None,
        aggregate: None,
    };
    let mut g = sample_game();
    g.sport_id = sport_id.into();
    let sides = sample_sides(sport_id);
    g.away = side(sides.away, sides.scores.0);
    g.home = side(sides.home, sides.scores.1);
    match sport_id {
        "mlb" | "cbase" => {
            // Final, because hits and errors are a post-game line.
            g.status = GameStatus::Final;
            g.ordinal = String::new();
            g.clock = String::new();
            g.situation = None;
            if extras {
                g.away.hits = Some(9);
                g.home.hits = Some(6);
                g.home.errors = Some(1);
            }
        }
        "nhl" | "olyhm" | "olyhw" => {
            // Live and even strength, so the shots have the strip to themselves
            // rather than losing it to the power-play badge.
            g.situation = Some(model::Situation::Hockey {
                away_powerplay: false,
                home_powerplay: false,
                away_players: 5,
                home_players: 5,
            });
            if extras {
                g.away.shots = Some(21);
                g.home.shots = Some(17);
            }
        }
        _ if espn::is_soccer(sport_id) => {
            g.status = GameStatus::Final;
            g.ordinal = String::new();
            g.clock = String::new();
            g.situation = None;
            if extras {
                g.away.shootout = Some(3);
                g.home.shootout = Some(4);
            }
        }
        "nfl" | "cfb" => {
            // A LIVE football game with the ball in play. The fixture used to
            // be a Final with no situation, so every football preview was a
            // quarter and two scores — none of the down-and-distance and field
            // position that is most of what a football frame draws, and the
            // thing someone is actually choosing between.
            g.status = GameStatus::Active;
            g.ordinal = "3RD".into();
            g.clock = String::new();
            g.situation = Some(model::Situation::Football {
                down_string: "3rd & 4".into(),
                ball_position: format!("{} 38", sides.home.0),
                home_possession: Some(true),
            });
        }
        "pga" | "lpga" | "f1" => {
            // A leaderboard, not a team game. `render_spoiled` already has a
            // path for these — title kept, rows dropped, progress centred — and
            // the old fixture never produced one, so golf and F1 previewed
            // their spoiler setting with a basketball frame.
            let (title, rows) = sample_leaderboard(sport_id);
            g.leaderboard = Some(model::Leaderboard { matchplay: None,
                // The F1 preview wears the lap bar: mid-race, so the
                // filled and unfilled halves both show.
                laps: (sport_id == "f1").then_some(model::LapProgress { completed: 42, total: 78 }),
                flag: None,
                stage: None,
                quali: None,
                name: title.into(),
                rows: rows
                    .into_iter()
                    .enumerate()
                    .map(|(i, (name, value))| model::LeaderRow {
                        name: name.into(),
                        value: value.into(),
                        // Golf rows carry mid-round thru values — UNEVEN ones
                        // (a 6 above a 14) so the "Holes played" preview also
                        // demonstrates that the column stays a column. F1 has
                        // no thru.
                        thru: (sport_id != "f1").then(|| ["14", "6", "9", "11"][i % 4].into()),
                        ..Default::default()
                    })
                    .collect(),
            });
            // Golf's round lives in `ordinal`, which is also what the "Show
            // round" preview switches on and off.
            g.ordinal = if sport_id == "f1" { String::new() } else { "R2".into() };
            g.status = GameStatus::Active;
            g.clock = String::new();
            g.situation = None;
        }
        _ => {
            // Basketball and anything else: a plain live game is enough to show
            // a name style or a spoiler, which is all those previews need.
            g.ordinal = "3RD".into();
            g.situation = None;
        }
    }
    // A spoiler preview needs a game IN PROGRESS. Normal mode keeps the period
    // as sanctioned progress and strict hides even that, so on a finished game
    // the two modes render identically and the choice looks like no choice.
    // The extras previews want the opposite for baseball and soccer, where the
    // line only appears once the game is over — so each preview asks for the
    // state its own setting actually bites in.
    if live && g.status == GameStatus::Final {
        g.status = GameStatus::Active;
        g.ordinal = match sport_id {
            "mlb" | "cbase" => "5TH".into(),
            _ if espn::is_soccer(sport_id) => "2ND".into(),
            _ => "3RD".into(),
        };
        g.clock = "7:12".into();
        // Extras that only draw on a final would be dead weight here.
        for side in [&mut g.away, &mut g.home] {
            side.hits = None;
            side.errors = None;
            side.shootout = None;
            side.aggregate = None;
        }
    }
    g
}

// GET /demo/games — the synthetic slate as JSON. The discovery route: it is
// how you learn the demo ids without owning a board. Deliberately NOT
// board-scoped — synthetic games carry no spoiler risk, and accepting `?board=`
// would imply a scoping guarantee this handler does not provide.
//
// `no-store`, unlike the day-cached `/guide`: the slate advances with the poll
// loop, so the bytes are not stable.
//
// (Plain comments: the demo handlers live elsewhere in the file, and as ///
// this block silently attached itself to `get_offmode` below, documenting the
// wrong route.)

/// GET /offmode/{id}.png — what an off mode looks like on the panel, for the
/// app's settings rows. Rendered at a FIXED time so the bytes are stable and
/// the day-long cache is safe (same rule as `/guide`).
///
/// 404 on an unknown id rather than falling back to a picture of something
/// else: the app treats a failed preview as "no image", which is honest, while
/// a wrong image silently misrepresents what the board will do.
async fn get_offmode(
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let id = id.strip_suffix(".png").unwrap_or(&id);
    if !offmode::offers(id) {
        return (axum::http::StatusCode::NOT_FOUND, format!("no off mode '{id}'")).into_response();
    }
    // Query params let each CHOICE preview itself ("?variant=forecast"), which
    // is what the settings screen shows beside its options. Anything not given
    // resolves to the descriptor's default rather than to empty — a preview of
    // a state the user can never actually be in would be a lie.
    let params: std::collections::BTreeMap<String, String> =
        q.into_iter().filter(|(k, _)| k != "board").collect();
    let frames =
        offmode::render_off_mode(
        id,
        &offmode::OffCtx {
            tz: None,
            params: &params,
            preview: true,
            // Fixed sample weather, for the same reason the preview clock is
            // pinned: two fetches of a day-cached PNG must be identical.
            weather: weather::Reading::Ready(weather::Weather::sample()),
        },
    );
    let Some(first) = frames.into_iter().next() else {
        // A mode that draws nothing declares no preview, so the app never asks.
        return (axum::http::StatusCode::NOT_FOUND, "off mode draws nothing").into_response();
    };
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/png"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        render_png(&first.px),
    )
        .into_response()
}

async fn get_demo_games(State(s): State<AppState>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let games = s.demo.lock().unwrap().clone();
    (
        [
            (axum::http::header::CONTENT_TYPE, "application/json"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        serde_json::to_string_pretty(&games).unwrap_or_else(|_| "[]".into()),
    )
        .into_response()
}

/// GET /demo/render/{id} — one demo game's 64x32 frame as a PNG, so the slate
/// can be eyeballed without pairing a board. Unknown id gets the same VISIBLE
/// "NO GAME" frame `get_render` uses rather than a 404, for the same reason:
/// the app's image-failure state is a black rectangle, indistinguishable from a
/// black frame.
async fn get_demo_render(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(s): State<AppState>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let id = id.strip_suffix(".png").unwrap_or(&id);
    let px = s.demo.lock().unwrap().iter().find(|g| g.id == id).map(|g| {
        render::render_frame(g, None, None, default_name_style(&g.sport_id)).1
    });
    let rgb565 = px.unwrap_or_else(|| {
        render::render_notice(
            &[("NO GAME", true, render::YELLOW_PUB), (id, false, render::GREY_PUB)],
            None,
        )
        .1
    });
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/png"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        render_png(&rgb565),
    )
        .into_response()
}

async fn get_sample(
    axum::extract::Path(variant): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let variant = variant.strip_suffix(".png").unwrap_or(&variant).to_string();
    // `?sport=` makes the example the user's own league rather than a generic
    // one — a Location preview is meaningless until you can see YOUR team's
    // city in it. Absent, it stays the original fixed NHL frame, so the
    // spoiler rows an older app asks for are byte-identical to before.
    let sport = q.get("sport").cloned();
    // `?extras=off` renders the same game with the box-score extras stripped,
    // which is the whole point of that setting's preview: the two frames differ
    // in exactly the line being switched.
    let extras = q.get("extras").map(|v| v != "off").unwrap_or(true);
    let spoiler = match variant.as_str() {
        "normal" => Some(SpoilerMode::Normal),
        "strict" => Some(SpoilerMode::Strict),
        _ => None,
    };
    // `?live=1` forces a game in progress. The spoiler rows pass it, because
    // Normal and Strict only differ while a game is running.
    let live = spoiler.is_some() || q.get("live").is_some_and(|v| v != "0");
    let mut g = match &sport {
        Some(id) => sample_game_for(id, extras, live),
        None => sample_game(),
    };
    // `?round=off` previews golf's "Show round" switch, which `board_view`
    // implements by clearing the ordinal — so the preview clears it the same
    // way rather than inventing a second mechanism.
    if q.get("round").is_some_and(|v| v == "off") {
        g.ordinal.clear();
    }
    // `?thru=off` previews the holes-played switch the same way.
    if q.get("thru").is_some_and(|v| v == "off") {
        if let Some(lb) = g.leaderboard.as_mut() {
            for row in &mut lb.rows {
                row.thru = None;
            }
        }
    }
    // `?style=` previews a name style without storing it. Falls back to the
    // league default rather than erroring on a style this binary does not know.
    let style = q
        .get("style")
        .and_then(|s| match s.as_str() {
            "location" => Some(NameStyle::Location),
            "name" => Some(NameStyle::Name),
            "abbrev" => Some(NameStyle::Abbrev),
            _ => None,
        })
        .unwrap_or_else(|| default_name_style(&g.sport_id));
    let (_, px) = render::render_frame(&g, spoiler, None, style);
    (
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        render_png(&px),
    )
        .into_response()
}

/// The one timestamp every guide screen is ticked to before capture. Fixed so
/// the animated bits (spinner, ellipsis, walking connect dot) land at the same
/// phase on every request — the app caches these for a day, and two fetches of
/// the same screen must be byte-identical.
const GUIDE_TICK_MS: u64 = 2000;

/// Drive a fresh board-core Board into the named troubleshooting state and
/// capture its framebuffer. This is the SAME state machine and renderer the
/// firmware runs — the guide images can't drift from what a real panel shows.
/// Unknown name -> None (the route 404s).
fn guide_screen(name: &str) -> Option<Vec<u32>> {
    use board_core::{Board, Gesture, SetupPhase};
    let mut b = Board::new();
    match name {
        // The connect-progress screen, on whichever leg is still down. Boot to
        // Running first (two ticks: the first only sets the boot-time
        // baseline); with no manifest cached there's nothing to hold through
        // the offline grace window, so the connect screen shows immediately.
        "joining-wifi" | "finding-server" => {
            b.tick(0);
            b.tick(GUIDE_TICK_MS);
            b.set_online(false);
            b.set_wifi(name == "finding-server");
        }
        // BLE provisioning, at its entry phase — what a factory-fresh (or
        // just-erased) board shows while it waits for the app to find it.
        "setup" => {
            b.gesture(Gesture::Long); // the provisioning entry -> Mode::Setup
            b.set_setup_phase(SetupPhase::WaitingBluetooth);
            b.tick(GUIDE_TICK_MS);
        }
        // The pairing screen, via the same server-flagged sticky path a
        // never-paired board takes. The code is a fixed, obviously-fake
        // 000000: these images are public and a real code must never be baked
        // into one.
        "pair-code" => {
            b.set_setup_code("000000".into());
            b.needs_pairing();
            b.tick(GUIDE_TICK_MS);
        }
        // Regular scores — the healthy state. Shown in the guide because a
        // board can look EXACTLY like this while the app says offline: the
        // offline grace holds the last content through brief drops while it
        // reconnects in the background. Rendered from the same fixed sample
        // game as the spoiler previews (server pipeline, not board-core —
        // game frames are the server's to draw).
        "game" => {
            let g = sample_game();
            let (_, rgb565) = render::render_frame(&g, None, None, default_name_style(&g.sport_id));
            return Some(rgb565_to_frame_u32(&rgb565));
        }
        // A dark panel: asleep, powered off, or unpowered. The guide entry's
        // text does the work; the image anchors "yes, fully black is a state".
        "black" => return Some(vec![0u32; FRAME_PIXELS]),
        _ => return None,
    }
    // Full brightness (Board::new's default): the app dims nothing.
    Some(b.framebuffer())
}

/// RGB565-LE frame bytes -> the 0RGB u32 words `board_png` takes.
fn rgb565_to_frame_u32(rgb565: &[u8]) -> Vec<u32> {
    rgb565
        .chunks_exact(2)
        .map(|c| {
            let (r, g, b) = scoreboard_protocol::rgb565_to_888(u16::from_le_bytes([c[0], c[1]]));
            ((r as u32) << 16) | ((g as u32) << 8) | b as u32
        })
        .collect()
}

/// Encode a board-core framebuffer (0RGB u32 words, 64x32) as PNG bytes at 1x.
/// Companion to `render_png`, which takes the server renderer's RGB565 frames.
fn board_png(px: &[u32]) -> Vec<u8> {
    let mut img = image::RgbImage::new(FRAME_WIDTH as u32, FRAME_HEIGHT as u32);
    for y in 0..FRAME_HEIGHT {
        for x in 0..FRAME_WIDTH {
            let p = px[y * FRAME_WIDTH + x];
            img.put_pixel(
                x as u32,
                y as u32,
                image::Rgb([(p >> 16) as u8, (p >> 8) as u8, p as u8]),
            );
        }
    }
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut buf, image::ImageFormat::Png)
        .ok();
    buf.into_inner()
}

/// GET /guide/{joining-wifi|finding-server|setup|pair-code}.png — the board's
/// firmware-local screens for the app's troubleshooting sheet ("if your board
/// looks like THIS, do THAT"), rendered by the real board-core renderer (see
/// `guide_screen`). No board scoping or auth: these are the screens every
/// board shows the world, so there's nothing to leak. Cached a day — they
/// change only with releases.
async fn get_guide(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let name = name.strip_suffix(".png").unwrap_or(&name);
    let Some(px) = guide_screen(name) else {
        return (axum::http::StatusCode::NOT_FOUND, "no guide screen by that name").into_response();
    };
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/png"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        board_png(&px),
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
    let tall = q.get("tall").is_some_and(|v| v == "1");
    // `?board=` scopes the render to that board's spoiler rules — without it a
    // protected game's card image would leak the score around the sanitized
    // summaries and frames.
    let cfg = q.get("board").map(|b| s.config_for(b));
    let px = with_slate(&s, cfg.as_ref(), |games| {
        games.iter().find(|g| g.id == id).map(|g| {
            let spoiler = cfg.as_ref().and_then(|c| spoiler_for(c, g));
            let tz = cfg.as_ref().and_then(board_tz);
            let style = cfg
                .as_ref()
                .map(|c| name_style_for(c, &g.sport_id))
                .unwrap_or_else(|| default_name_style(&g.sport_id));
            // The card image must be the frame the BOARD draws: preferences
            // and the data-source swap applied, not the raw feed game. The
            // app's tiles diverging from the panel is how "the preview does
            // not match my scoreboard" gets reported — because it didn't.
            let of1 = s.openf1_rows.lock().unwrap();
            let view = cfg
                .as_ref()
                .map(|c| board_view_with(c, g, of1.as_ref()))
                .unwrap_or(std::borrow::Cow::Borrowed(g));
            // `?tall=1`: the BOARD's frame — a deep leaderboard's whole strip
            // at this board's depth, which the app scrolls exactly as the
            // panel does. Everything else renders the same pixels either way.
            if tall {
                let rows_cap = cfg
                    .as_ref()
                    .and_then(|c| c.sports.iter().find(|sp| sp.sport_id == g.sport_id))
                    .and_then(|sp| sp.leaderboard_rows)
                    .map(|r| r as usize)
                    .unwrap_or(render::LEADERBOARD_DEFAULT_ROWS);
                render::render_board_frame(&view, rows_cap, spoiler, tz, style).1
            } else {
                render::render_frame(&view, spoiler, tz, style).1
            }
        })
    });
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
    let game = with_slate(&s, cfg.as_ref(), |games| games.iter().find(|g| g.id == id).cloned());
    let Some(game) = game else {
        tracing::info!("game {id} board={} -> UNKNOWN GAME", q.get("board").map(String::as_str).unwrap_or("-"));
        return (axum::http::StatusCode::NOT_FOUND, format!("no game '{id}'")).into_response();
    };
    // The debug window shows the game as the BOARD sees it: preferences
    // applied and, for a league with an alternate feed, that feed's rows.
    let of1 = s.openf1_rows.lock().unwrap();
    let active_openf1 =
        cfg.as_ref().is_some_and(|c| openf1_rows_for(c, &game, of1.as_ref()).is_some());
    let game = cfg
        .as_ref()
        .map(|c| board_view_with(c, &game, of1.as_ref()).into_owned())
        .unwrap_or(game);
    drop(of1);
    // `spoiler_for` already honors the one-off reveal list, so a revealed
    // game passes through unredacted like everywhere else.
    let spoiler = cfg.as_ref().and_then(|c| spoiler_for(c, &game));
    let (game, spoiled) = match spoiler {
        Some(mode) => (redact_game(&game, mode), true),
        None => (game, false),
    };
    let mut v = serde_json::to_value(&game).expect("game serializes");
    if game.sport_id == "f1" {
        // Which feed the user ASKED for, and which one actually produced the
        // rows above — they differ when the alternate is selected but its
        // data is stale or for another session.
        let configured = cfg
            .as_ref()
            .and_then(|c| c.sports.iter().find(|sp| sp.sport_id == "f1"))
            .and_then(|sp| sp.data_source.clone())
            .unwrap_or_else(|| "espn".into());
        let obj = v.as_object_mut().expect("game serializes to an object");
        obj.insert("data_source".into(), configured.into());
        obj.insert(
            "data_source_active".into(),
            (if active_openf1 { "openf1" } else { "espn" }).into(),
        );
    }
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
        // Match play's cup score and match states are results too.
        lb.matchplay = None;
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
/// One board's view of a game, after its per-league DISPLAY settings.
///
/// Not a spoiler pass — that is `to_summary_for`/`render_spoiled`, and it hides
/// results. This drops things the user simply asked not to see, so the panel and
/// the card agree without every renderer growing a parameter for each new
/// toggle. Borrowed unless a setting actually changes something, so the common
/// case allocates nothing.
pub(crate) fn board_view<'a>(cfg: &BoardConfig, g: &'a Game) -> std::borrow::Cow<'a, Game> {
    board_view_with(cfg, g, None)
}

/// The OpenF1 rows that would replace this game's, when the board opted in
/// AND the stored rows are for this session AND fresh. One gate, used by the
/// swap and by the /game debug keys, so the two can never disagree.
fn openf1_rows_for<'r>(
    cfg: &BoardConfig,
    g: &Game,
    openf1: Option<&'r openf1::SessionRows>,
) -> Option<&'r openf1::SessionRows> {
    let selected = g.sport_id == "f1"
        && cfg
            .sports
            .iter()
            .find(|s| s.sport_id == g.sport_id)
            .and_then(|s| s.data_source.as_deref())
            == Some("openf1");
    if !selected {
        return None;
    }
    openf1
        .filter(|r| r.fetched.elapsed() < Duration::from_secs(600))
        // Matched on the game's SESSION, not an id suffix —
        // "-sprint-quali" ends with "-quali" and would collide.
        .filter(|r| g.session.as_deref() == Some(r.session))
}

/// `board_view` plus the OpenF1 row swap: a board whose f1 league selected
/// the alternate source gets that feed's timing rows on the SAME game — ids,
/// title, and status stay ESPN's, so flipping the switch is a controlled
/// comparison. Rows older than ten minutes are ignored: a dead feed must
/// degrade to ESPN, never freeze a stale tower on the panel.
pub(crate) fn board_view_with<'a>(
    cfg: &BoardConfig,
    g: &'a Game,
    openf1: Option<&openf1::SessionRows>,
) -> std::borrow::Cow<'a, Game> {
    let league = cfg.sports.iter().find(|s| s.sport_id == g.sport_id);
    // Default ON: a leaderboard looks the same on Thursday as on Sunday, so the
    // round earns its corner until someone turns it off.
    let show_round = league.and_then(|s| s.show_round).unwrap_or(true);
    // Default ON: mid-round, -9 thru 7 and -9 thru 16 are different scores.
    let show_thru = league.and_then(|s| s.show_thru).unwrap_or(true);
    // Default ON for the same reason: the extras were what the panel drew
    // before anyone could turn them off, so absence has to mean "as before".
    let show_extras = league.and_then(|s| s.show_extras).unwrap_or(true);
    let drop_round = !show_round && !g.ordinal.is_empty();
    let drop_extras = !show_extras && game_has_extras(g);
    let drop_thru = !show_thru
        && g.leaderboard.as_ref().is_some_and(|lb| lb.rows.iter().any(|r| r.thru.is_some()));
    let openf1_swap = openf1_rows_for(cfg, g, openf1);
    if !drop_round && !drop_extras && !drop_thru && openf1_swap.is_none() {
        return std::borrow::Cow::Borrowed(g);
    }
    let mut hidden = g.clone();
    if let Some(swap) = openf1_swap {
        if let Some(lb) = hidden.leaderboard.as_mut() {
            lb.rows = swap.rows.clone();
            // Track status rides the same swap: red/yellow recolor the
            // title-row flags, and only for boards on this source.
            lb.flag = swap.flag.map(String::from);
            // So do the qualifying segment and its elimination zones — this
            // feed knows the segment exactly (race control says when each
            // starts), where ESPN's reading trails by minutes.
            lb.stage = swap.stage;
            lb.quali = swap.quali;
            // The lap bar too. ESPN's status is the authority on the race
            // being over: the live laps feed stops one short at the flag
            // (the lap in progress is never "completed") until OpenF1
            // publishes the classification, which can lag.
            lb.laps = swap.laps.map(|l| match g.status {
                GameStatus::Final => model::LapProgress { completed: l.total, total: l.total },
                _ => l,
            });
        }
    }
    if drop_round {
        hidden.ordinal.clear();
    }
    if drop_thru {
        // Clearing the DATA, same as the round: the renderer already draws
        // nothing (and returns the width to the name) when thru is None.
        if let Some(lb) = hidden.leaderboard.as_mut() {
            for row in &mut lb.rows {
                row.thru = None;
            }
        }
    }
    if drop_extras {
        // Clearing the DATA rather than passing a flag down: `strip_sub_line`
        // and `draw_hockey_situation` already draw nothing when these are None,
        // so the renderers stay pure functions of the Game and never learn that
        // a preference exists. It also means the toggle inherits spoiler safety
        // for free, because this is the same pass redaction runs in.
        for side in [&mut hidden.away, &mut hidden.home] {
            side.hits = None;
            side.errors = None;
            side.shots = None;
            side.shootout = None;
            side.aggregate = None;
        }
    }
    std::borrow::Cow::Owned(hidden)
}

/// Whether this game would draw any box-score extra, so `board_view` can keep
/// borrowing in the overwhelmingly common case where there is nothing to strip.
fn game_has_extras(g: &Game) -> bool {
    [&g.away, &g.home].into_iter().any(|s| {
        s.hits.is_some()
            || s.errors.is_some()
            || s.shots.is_some()
            || s.shootout.is_some()
            || s.aggregate.is_some()
    })
}

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
            // Match play has no leader row; its headline is the cup score.
            if let Some(mp) = &lb.matchplay {
                let [a, b] = &mp.teams;
                return format!(
                    "{} {} · {} {}",
                    a.abbrev,
                    model::cup_points(a.points),
                    b.abbrev,
                    model::cup_points(b.points)
                );
            }
            let leader = lb.rows.first().map(|r| format!("{} {}", r.name, r.value)).unwrap_or_default();
            // Golf's round leads the detail ("R3 · SCHEFFLER -6"). Without it a
            // tournament card reads identically on all four days. F1 has no
            // round — its `ordinal` stays empty — so nothing is prefixed there.
            match (g.ordinal.as_str(), leader.as_str()) {
                ("", l) => l.to_string(),
                (r, "") => r.to_string(),
                (r, l) => format!("{r} · {l}"),
            }
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
            scroll_dwell_ms: 0,
            pin_rows: 0,
            // Leaderboard sports DO have ESPN pages — golf a tournament
            // leaderboard, F1 a race result — they just aren't /gameId/ ones.
            espn_url: crate::espn::espn_game_url(&g.sport_id, &g.id),
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
        scroll_dwell_ms: 0,
        pin_rows: 0,
        espn_url: crate::espn::espn_game_url(&g.sport_id, &g.id),
        // Board-scoped; the browse path fills it in.
        queued: false,
    }
}

/// CAN this game's board rendering scroll? True for a leaderboard whose field
/// runs deeper than the static 4 rows — regardless of the league's configured
/// depth. This drives `GameSummary.scrolls`, i.e. whether the app offers its
/// league scroll toggle on the card; it must NOT consider the depth setting,
/// or turning scrolling off (depth = 4) would hide the very button that turns
/// it back on. The pregame/spoiler outs mirror `render_board_frame`'s static
/// forcing, so the toggle never appears on a game that can't scroll anyway.
pub(crate) fn game_scroll_capable(cfg: &BoardConfig, g: &Game) -> bool {
    if g.status == GameStatus::Pregame || spoiler_for(cfg, g).is_some() {
        return false;
    }
    // The renderer's own count: match play has no stroke-play rows, and a
    // count of `rows` alone never let a Presidents Cup card scroll.
    g.leaderboard.as_ref().is_some_and(|lb| render::leaderboard_row_count(lb) > 4)
}

/// How long one rotation entry holds this board's panel, in ms, for a frame
/// `frame_rows` tall: the league's speed override or the board's, snapped to
/// a preset (so a legacy 7s or 30s behaves as the preset its label says),
/// scaled by the frame's height when it scrolls (`screen_dwell_ms`: 64 rows
/// twice as long, 128 four times). Pass 32 for a screen that does not scroll.
pub(crate) fn entry_dwell_ms(cfg: &BoardConfig, sport_id: &str, frame_rows: usize) -> u32 {
    let secs = cfg
        .sports
        .iter()
        .find(|s| s.sport_id == sport_id)
        .and_then(|s| s.dwell_secs)
        .unwrap_or(cfg.dwell_secs);
    scoreboard_protocol::screen_dwell_ms(secs, frame_rows)
}

/// A league's leaderboard depth on this board (golf/F1 rows; 4 = static).
pub(crate) fn league_rows_cap(cfg: &BoardConfig, sport_id: &str) -> usize {
    cfg.sports
        .iter()
        .find(|sp| sp.sport_id == sport_id)
        .and_then(|sp| sp.leaderboard_rows)
        .map(|r| r as usize)
        .unwrap_or(render::LEADERBOARD_DEFAULT_ROWS)
}

/// Mark a board-scoped summary with how this board scrolls the game: the
/// flag the app's scroll toggle keys off, plus the dwell and pinned rows the
/// app's card needs to glide exactly as the panel does. The dwell depends on
/// the frame's height, so `frame_rows` is the board frame's height when the
/// caller already rendered it; None renders it here (only for a game that can
/// scroll — a handful of leaderboards, never the team-sport slate). A game
/// whose scrolling is turned off (depth 4) renders panel-sized and gets no
/// glide, though its toggle still shows.
pub(crate) fn mark_scroll(s: &mut GameSummary, cfg: &BoardConfig, g: &Game, frame_rows: Option<usize>) {
    s.scrolls = game_scroll_capable(cfg, g);
    if !s.scrolls {
        return;
    }
    let rows = frame_rows.unwrap_or_else(|| {
        let (_, px, _) = render::render_board_frame(
            g, league_rows_cap(cfg, &g.sport_id), None, board_tz(cfg), name_style_for(cfg, &g.sport_id),
        );
        px.len() / (scoreboard_protocol::FRAME_WIDTH * 2)
    });
    if rows > scoreboard_protocol::FRAME_HEIGHT {
        s.scroll_dwell_ms = entry_dwell_ms(cfg, &g.sport_id, rows);
        s.pin_rows = render::LEADERBOARD_PIN_ROWS;
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
/// THE demo/real branch — the only place in the server that reads `demo_data`.
/// Everything downstream of it (priority, rotation, spoilers, frames, manifests,
/// both push paths) is slate-agnostic, which is the point: one decision, not a
/// conditional threaded through twenty call sites.
///
/// Closure form rather than returning the slate, because every caller holds the
/// lock while iterating and a guard can't be returned across two mutexes.
/// Deadlock-checked: the closures call `board_shows` (locks `finished`) and
/// pure helpers — none of them re-lock `games` or `demo`.
/// The panel's width in pixels, for code that must fit text to it.
pub(crate) fn protocol_frame_width() -> i32 {
    scoreboard_protocol::FRAME_WIDTH as i32
}

pub(crate) fn with_slate<R>(state: &AppState, cfg: Option<&BoardConfig>, f: impl FnOnce(&[Game]) -> R) -> R {
    if cfg.is_some_and(|c| c.demo_data) {
        f(&state.demo.lock().unwrap())
    } else {
        f(&state.games.lock().unwrap())
    }
}

/// When a game finished, in both clocks.
///
/// Two clocks because they answer different questions and neither substitutes
/// for the other: the linger window has to survive a wall clock that jumps
/// (NTP, DST, a laptop waking up), and "Ended 9:47 PM" cannot be rendered from
/// a monotonic instant at all.
#[derive(Clone, Copy)]
pub(crate) struct Finish {
    /// Monotonic — how long the finished game keeps its board slot.
    pub at: Instant,
    /// Wall clock, for the app's "Ended …" row. Derived from the same evidence
    /// as `at`, so the two can never disagree about which moment they mean.
    pub wall: chrono::DateTime<chrono::Utc>,
}

pub(crate) fn record_finished_transitions(state: &AppState, incoming: &[Game], prev_poll: Instant) {
    // Both slates: `state.finished` is one id-keyed map serving both, so a
    // scan that saw only the live slate would let each poll evict the other's
    // stamps. `incoming` is likewise the union (see `poll_loop`).
    let was_live: HashSet<String> = state
        .games
        .lock()
        .unwrap()
        .iter()
        .chain(state.demo.lock().unwrap().iter())
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
            // The wall-clock twin of `ended`, from the same evidence rather than
            // from `now`: the feed's endDate when it had one, else back-dated by
            // how long ago that instant was. This is the ONLY end time most
            // sports ever get — ESPN's scoreboard feed carries no end timestamp
            // for a team game, just `completed: true` — so it is deliberately
            // conservative. Its error is bounded by the poll interval, and a
            // game that was already Final when this process started never gets
            // one at all, which is honest: we did not see it end.
            let wall = chrono::Duration::from_std(ended.elapsed())
                .ok()
                .and_then(|ago| now_utc.checked_sub_signed(ago))
                .unwrap_or(now_utc);
            fin.entry(g.id.clone()).or_insert(Finish { at: ended, wall });
            let left = FINAL_LINGER.saturating_sub(ended.elapsed());
            tracing::info!("game '{}' ended; lingering {}m", g.id, left.as_secs() / 60);
        }
    }
    // Keep the map bounded: drop stamps past the linger or for vanished games.
    fin.retain(|id, f| {
        f.at.elapsed() < FINAL_LINGER && incoming.iter().any(|g| &g.id == id)
    });
}

/// Fill in `end_time` on finished games from what the server watched happen.
///
/// Only ever ADDS one. A feed that carries its own endDate (golf tournaments,
/// F1 race weekends) is left alone — it knows better than our poll cadence does
/// — and a game we never saw end keeps `None` rather than being given a
/// plausible-looking guess.
pub(crate) fn stamp_observed_end_times(state: &AppState, slate: &mut [Game]) {
    let fin = state.finished.lock().unwrap();
    for g in slate.iter_mut() {
        if g.status == GameStatus::Final && g.end_time.is_none() {
            if let Some(f) = fin.get(&g.id) {
                g.end_time = Some(rfc3339_utc(f.wall));
            }
        }
    }
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

/// The board's configured IANA timezone, parsed, else the server-wide
/// `DEFAULT_TIMEZONE`. `None` (nothing set, or unparseable) falls back to the
/// server's local zone at each format site, so a local dev server keeps
/// rendering sensible times without any configuration.
/// The board's wall clock as ms since the Unix epoch with its zone offset
/// ALREADY APPLIED. The board has no tz database and no DST rules, so it is
/// sent an answer rather than the inputs — and a DST transition reaches it as
/// an ordinary re-anchor on the next tick.
pub(crate) fn board_local_epoch_ms(cfg: &BoardConfig) -> u64 {
    use chrono::Offset;
    let now = chrono::Utc::now();
    let offset_secs = match board_tz(cfg) {
        Some(tz) => now.with_timezone(&tz).offset().fix().local_minus_utc() as i64,
        None => chrono::Local::now().offset().fix().local_minus_utc() as i64,
    };
    (now.timestamp_millis() + offset_secs * 1000).max(0) as u64
}

/// Is this board inside its quiet window right now, and has the user not
/// overridden it?
///
/// DERIVED on every read — never written into `cfg.power`. Three reasons, all
/// visible in the code as it stands: `tick_once` sets `power = true`
/// unconditionally when a sleep timer lapses and would fight a written flag
/// every 30 seconds; the app's power toggle writes that same field, so a
/// written quiet flag would need a second bit recording who turned it off; and
/// after a restart a persisted `power = false` is indistinguishable from a
/// manual switch-off, so morning could not know whether to undo it.
pub(crate) fn quiet_now(cfg: &BoardConfig) -> bool {
    quiet_window_now(cfg) && !cfg.quiet_override
}

/// Whether the wall clock is inside the window, ignoring any override.
pub(crate) fn quiet_window_now(cfg: &BoardConfig) -> bool {
    let Some(q) = cfg.quiet_hours else { return false };
    use chrono::Timelike;
    let now = match board_tz(cfg) {
        Some(tz) => chrono::Utc::now().with_timezone(&tz).time(),
        None => chrono::Local::now().time(),
    };
    q.contains((now.hour() * 60 + now.minute()) as u16)
}

pub(crate) fn board_tz(cfg: &BoardConfig) -> Option<chrono_tz::Tz> {
    cfg.timezone
        .as_deref()
        .and_then(|s| s.parse().ok())
        .or_else(default_tz)
}

/// The zone a board with nothing configured renders in, from `DEFAULT_TIMEZONE`
/// (an IANA name like "America/Chicago").
///
/// This exists because the board stopped guessing. Boards used to IP-geolocate
/// themselves at boot over cleartext HTTP to a third party and report the
/// answer up as `ClientMsg::DeviceTimezone`; that call is gone from the
/// firmware, and the server cannot geolocate on its behalf — behind Railway's
/// TCP proxy the peer address is the PROXY (see [`classify_peer`]). Without
/// this, a board nobody has set a zone for would render in the server's local
/// zone, which on Railway is UTC.
///
/// A CONFIGURED default, deliberately, not a guess: an owner-set value is a
/// fact, and a wrong timezone is worse than an unset one. Read once — the
/// environment cannot change under a running process, and this is called from
/// every render.
fn default_tz() -> Option<chrono_tz::Tz> {
    static DEFAULT_TZ: std::sync::OnceLock<Option<chrono_tz::Tz>> = std::sync::OnceLock::new();
    *DEFAULT_TZ.get_or_init(|| {
        let raw = std::env::var("DEFAULT_TIMEZONE").ok()?;
        let parsed = parse_iana_tz(&raw);
        match parsed {
            Some(tz) => tracing::info!("boards with no configured zone render in {tz}"),
            None => tracing::warn!("DEFAULT_TIMEZONE='{raw}' is not an IANA zone name — ignored"),
        }
        parsed
    })
}

/// An IANA zone name ("America/Chicago") if it names a real zone. The one place
/// a zone string becomes a zone, so "unparseable is ignored" means the same
/// thing everywhere.
fn parse_iana_tz(raw: &str) -> Option<chrono_tz::Tz> {
    raw.parse().ok()
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
            .is_some_and(|f| f.at.elapsed() < FINAL_LINGER);
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



/// The board's league option for a game's sport, if one is set (never "none").
fn league_options<'a>(cfg: &'a BoardConfig, sport_id: &str) -> &'a [String] {
    cfg.sports
        .iter()
        .find(|s| s.sport_id == sport_id)
        .map(|s| s.championship_option_ids.as_slice())
        .unwrap_or(&[])
}

/// How strongly the league OPTION claims this game: Some(2) for a targeted match
/// (championship tier, golf major, F1 session), Some(3) for the "all" blanket,
/// None when the option doesn't cover it.
/// EXACT membership, deliberately. A checked round means that round and
/// nothing else — the old table let "conf" quietly claim the Final, which is
/// the hidden implication this replaced.
fn game_option_priority(g: &Game, cfg: &BoardConfig) -> Option<u8> {
    let opts = league_options(cfg, &g.sport_id);
    if opts.is_empty() {
        return None;
    }
    let key = match g.sport_id.as_str() {
        // F1 subscribes to session kinds; its games carry no championship tier.
        "f1" => g.session.as_deref(),
        _ => g.champ.as_ref().map(|c| c.tier.as_str()),
    };
    // A targeted round outranks the blanket, which is what orders the
    // ROTATION_CAP cut when someone has both.
    if key.is_some_and(|k| opts.iter().any(|o| o == k)) {
        return Some(2);
    }
    opts.iter().any(|o| o == "all").then_some(3)
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
    // A board with NOTHING chosen used to claim the entire slate. It reads as a
    // friendly default and is actually the opposite: a freshly paired board lit
    // up with twenty games the owner never asked for, every league at once, and
    // no way to tell that from a board they had configured. Nothing selected now
    // means nothing selected — `manifest_for_board` answers with the setup screen
    // instead, which says what to do about it.
    //
    // Demo data is the one exception: its whole job is to fill a panel that has
    // no preferences yet, and a demo board showing "add teams in the app" would
    // be a toggle that appears broken.
    if unconfigured && cfg.demo_data {
        return Some(4);
    }
    None
}

/// Device console lines kept per board. A few hundred covers the minutes around
/// a fault at the rate the firmware prints; past that the oldest go.
const DEBUG_LOG_CAP: usize = 400;

/// Mint a capability URL for reading one board's console, good for `secs`.
///
/// Random nonce from the same generator the pairing challenge uses, so guessing
/// one is not a realistic attack. Expires with the debug window rather than
/// living until someone remembers to revoke it.
/// How long a link keeps working after the board stops streaming.
///
/// The end of a window is exactly when you want to read it: the last thing the
/// board said before it went quiet is usually the thing you were waiting for.
/// A link that died with the session would throw that away at the worst moment.
/// The board is already silent by then, so this grace costs nothing but a
/// readable buffer.
const DEBUG_READ_GRACE: Duration = Duration::from_secs(10 * 60);

pub fn mint_debug_token(state: &AppState, board_id: &str, secs: u32) -> String {
    let nonce: String = {
        use rand::RngCore;
        let mut b = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut b);
        b.iter().map(|x| format!("{x:02x}")).collect()
    };
    let now = std::time::Instant::now();
    let session_ends = now + Duration::from_secs(u64::from(secs));
    let readable_until = session_ends + DEBUG_READ_GRACE;
    {
        let mut tokens = state.debug_tokens.lock().unwrap();
        tokens.retain(|_, (_, _, readable)| *readable > now);
        tokens.insert(nonce.clone(), (board_id.to_string(), session_ends, readable_until));
    }
    persist_debug_sessions(state);
    format!("/debug/{nonce}")
}

/// The durable copy of the live debug sessions — the key detail that makes
/// log streaming survive a DEPLOY.
///
/// Sessions used to live only in process memory (Instant deadlines cannot even
/// represent anything else), which was fine for the failure it was designed
/// around — a BOARD rebooting mid-session gets re-armed on reconnect — and
/// silently fatal for the other restart: every `railway up` while a session
/// ran destroyed the token, so the board reconnected to a server that had
/// never heard of the session, the re-arm found nothing, and the app kept
/// showing a live-looking link and countdown onto a stream that had stopped.
/// On a day with a dozen deploys the feature simply read as broken.
///
/// Stored as wall-clock seconds in the same kv table as the daily-report
/// marker; loaded at boot converted back to Instants. The buffered LINES are
/// deliberately not persisted — the re-armed board streams fresh ones within
/// seconds, and stale console lines presented as current would mislead.
const DEBUG_SESSIONS_KEY: &str = "debug_sessions";

pub(crate) fn persist_debug_sessions(state: &AppState) {
    let now_i = std::time::Instant::now();
    let now_u = unix_now();
    let rows: Vec<(String, String, u64, u64)> = state
        .debug_tokens
        .lock()
        .unwrap()
        .iter()
        .map(|(n, (b, ends, readable))| {
            (
                n.clone(),
                b.clone(),
                now_u + ends.saturating_duration_since(now_i).as_secs(),
                now_u + readable.saturating_duration_since(now_i).as_secs(),
            )
        })
        .collect();
    state.kv_set(DEBUG_SESSIONS_KEY, &serde_json::to_string(&rows).unwrap());
}

fn load_debug_sessions(state: &AppState) {
    let Some(json) = state.kv_get(DEBUG_SESSIONS_KEY) else { return };
    let Ok(rows) = serde_json::from_str::<Vec<(String, String, u64, u64)>>(&json) else { return };
    let now_i = std::time::Instant::now();
    let now_u = unix_now();
    let mut tokens = state.debug_tokens.lock().unwrap();
    for (nonce, board, ends_u, readable_u) in rows {
        if readable_u > now_u {
            tokens.insert(
                nonce,
                (
                    board,
                    now_i + Duration::from_secs(ends_u.saturating_sub(now_u)),
                    now_i + Duration::from_secs(readable_u - now_u),
                ),
            );
        }
    }
}

/// Seconds left on this board's debug session, if one is live.
///
/// The board deliberately forgets streaming when it reboots — a setting that
/// survived a power cycle could leave a board chattering forever. But the
/// SESSION outlives the board's memory of it: the window was requested from the
/// server and the token is still good, so a board that comes back mid-session
/// should be put back to work rather than quietly staying silent while the app
/// shows a live link. `device_loop` re-arms on every connect for exactly that.
pub fn debug_session_remaining(state: &AppState, board_id: &str) -> Option<u32> {
    let now = std::time::Instant::now();
    state
        .debug_tokens
        .lock()
        .unwrap()
        .values()
        // SESSION end, not the readable-until: the grace exists so a link
        // outlives the stream, not so the board keeps talking into it.
        .filter(|(b, ends, _)| b == board_id && *ends > now)
        .map(|(_, ends, _)| ends.duration_since(now).as_secs() as u32)
        .max()
}

/// The board a token reads, or None when it is unknown or expired.
fn resolve_debug_token(state: &AppState, nonce: &str) -> Option<String> {
    let mut tokens = state.debug_tokens.lock().unwrap();
    let now = std::time::Instant::now();
    tokens.retain(|_, (_, _, readable)| *readable > now);
    tokens.get(nonce).map(|(board, _, _)| board.clone())
}

/// Record console lines from a board, oldest-out past the cap.
pub fn record_debug_log(state: &AppState, board_id: &str, lines: Vec<String>) {
    let mut logs = state.debug_logs.lock().unwrap();
    let buf = logs.entry(board_id.to_string()).or_default();
    for l in lines {
        if buf.len() >= DEBUG_LOG_CAP {
            buf.pop_front();
        }
        buf.push_back(l);
    }
}

/// How many games fit in the board's rotation.
const ROTATION_CAP: usize = 20;

/// How many not-yet-started games ride along in the app's "Live and Upcoming"
/// list. The claim predicate is generous by design — an unconfigured board or a
/// league set to "Every match" claims the ENTIRE day's slate, which on a fall
/// Saturday is hundreds of games. Each one costs a render, a cache entry, and a
/// binary frame pushed down the app's socket on every poll, to produce a list
/// nobody would scroll. Ten of the soonest is a useful "what's next" without
/// turning a routine board-state push into a megabyte.
///
/// A game past the cap is simply absent — NOT `bumped`, which means something
/// else entirely (it qualified for the rotation and lost the slot).
const UPCOMING_CAP: usize = 10;

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
        .any(|s| !s.championship_option_ids.is_empty());
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
    with_slate(state, Some(cfg), |games| candidates_in(state, cfg, games))
}

fn candidates_in(state: &AppState, cfg: &BoardConfig, games: &[Game]) -> (Vec<Game>, Vec<Game>) {
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
        let hit = with_slate(state, Some(cfg), |games| {
            games.iter().find(|g| &g.id == fid && board_shows(state, g)).cloned()
        });
        if let Some(g) = hit {
            return vec![g];
        }
    }
    candidate_games(state, cfg)
}

/// The board's screen list as everything server-side should see it: the
/// stored list normalized against THIS binary's off modes, or — for a board
/// that never saved one — the list its legacy fields describe (MAR-117).
pub fn board_screens(cfg: &BoardConfig) -> Vec<ScreenConfig> {
    resolved_screens(cfg, &offmode::off_modes())
}

/// Every screen the board's knob picker offers, in the board's own order:
/// Sports, Off, then the user's screens (MAR-117, §0 Q2), labelled for the
/// knob (≤15 ASCII, unique). The idle "Nothing" screen is never a row — a
/// picked dark screen IS off, and two dark rows with different meanings is a
/// trap. The APP composes the same list from the same protocol functions; the
/// ids must agree with what `handle_select_mode` accepts, which
/// `the_picker_offers_what_select_mode_accepts` pins.
pub fn pickable_modes(cfg: &BoardConfig) -> Vec<ModeEntry> {
    let screens = board_screens(cfg);
    let labels = board_labels(&screens, &offmode::off_modes());
    screens.into_iter().zip(labels).map(|(s, label)| ModeEntry { id: s.id, label }).collect()
}

pub fn manifest_for_board(state: &AppState, board_id: &str) -> Manifest {
    // The picker data rides EVERY manifest — most importantly the dark ones,
    // because a board showing "off" is exactly the board whose picker matters.
    let mut m = manifest_inner(state, board_id);
    let cfg = state.config_for(board_id);
    // The held INSTANCE, so the knob's tick lands on the right row when a
    // kind appears twice.
    m.mode = cfg.held_screen_id().to_string();
    m.modes = pickable_modes(&cfg);
    m
}

fn manifest_inner(state: &AppState, board_id: &str) -> Manifest {
    let cfg = state.config_for(board_id);
    let tz = board_tz(&cfg);

    // Priority 0: quiet hours. Above BOTH games and notices, because the notice
    // path returns `display_on: true` regardless of power — a demo toggle or a
    // power splash would otherwise light the panel at 3am, mid-window.
    //
    // Quiet means DARK, not the off mode. "Quiet hours" that answer with a lit
    // clock at 1am have not done the thing they were turned on for; a board
    // that wants a clock overnight simply leaves the window off.
    //
    // There is no exception. "Wake for live events" used to carve one out for a
    // game that was genuinely on, and it made the window mean two things at
    // once: the setting could not be described without a clause, and a board
    // that lit up at 2am for a match had done exactly what it was told in a way
    // nobody could predict from the screen. `cfg.quiet_allow_events` is still
    // accepted on the wire so an older app's command is not an error, and is
    // deliberately not read here.
    if quiet_now(&cfg) {
        return dark_manifest(&cfg);
    }

    // Priority 1: live games — only in SCORES mode. A deliberately picked
    // clock/weather/off holds the panel; that a lit board can now miss a game
    // is the point of picking a mode, not a bug.
    if cfg.mode == MODE_SCORES {
        let chosen = chosen_games(state, &cfg);
        if !chosen.is_empty() {
            // Games override any lingering notice ("no games" message ends early).
            state.notices.lock().unwrap().remove(board_id);
            let mut rotation = Vec::new();
            for g in &chosen {
                let league = cfg.sports.iter().find(|s| s.sport_id == g.sport_id);
                // Leaderboard depth (golf/F1): 4 = static top-4, no scrolling;
                // up to the full field. Deep frames render TALL and the board
                // scrolls a 32px window over them, one full pass per dwell
                // (app cards keep the static 4-row frame). The app's
                // "Disable scrolling" is just a shortcut that sets this depth
                // to 4 — one knob, stored on the league, never pruned.
                let rows_cap = league
                    .and_then(|s| s.leaderboard_rows)
                    .map(|r| r as usize)
                    .unwrap_or(render::LEADERBOARD_DEFAULT_ROWS);
                let style = name_style_for(&cfg, &g.sport_id);
                let of1 = state.openf1_rows.lock().unwrap();
                let view = board_view_with(&cfg, g, of1.as_ref());
                let (id, px, scrolls) = render::render_board_frame(&view, rows_cap, spoiler_for(&cfg, g), tz, style);
                // The league's speed (else the board's), scaled by the
                // frame's height when it scrolls: the scroll pass syncs to it.
                let rows = if scrolls { px.len() / (scoreboard_protocol::FRAME_WIDTH * 2) } else { scoreboard_protocol::FRAME_HEIGHT };
                let dwell_ms = entry_dwell_ms(&cfg, &g.sport_id, rows);
                state.cache_frame(id.clone(), px);
                // Every scrolling frame is a leaderboard, and a leaderboard's
                // title rows hold still while the field glides under them.
                let pin_rows = if scrolls { render::LEADERBOARD_PIN_ROWS } else { 0 };
                rotation.push(RotationEntry { frame_id: id, dwell_ms, scroll: scrolls, pin_rows });
            }
            return Manifest {
        mode: String::new(),
        modes: Vec::new(),
                clock_hour24: false,
                // Games / notice path: never a device-drawn clock.
                local_clock: false,
                rotation,
                display_on: true,
                off_state: cfg.off_state,
                brightness: cfg.brightness,
                epoch: 0,
                countdown_ms: None,
            };
        }
    }

    // Priority 2: a transient notice ("SLEEPING…" splash / "NO GAMES…"). The whole
    // countdown bar is the BOARD's to animate (`Manifest.countdown_ms`): one
    // frame carrying the empty outlined bar, and the device fills it smoothly
    // on its own clock, anchored to this notice's epoch. The old pre-rendered
    // sweep cost ~4KB of board RAM per step, which capped a 4s splash at 4
    // visibly chunky jumps — and per-step pushes are network animation, which
    // this system forbids. The frame derives from the notice's fixed lines,
    // so a mid-notice re-render yields an IDENTICAL manifest and the dedup in
    // `push_board` leaves the board's anchor alone.
    {
        let mut notices = state.notices.lock().unwrap();
        if let Some(n) = notices.get(board_id) {
            if std::time::Instant::now() < n.until {
                let lines: Vec<(&str, bool, embedded_graphics::pixelcolor::Rgb888)> =
                    n.lines.iter().map(|(s, b, c)| (s.as_str(), *b, *c)).collect();
                let total_ms = n.until.duration_since(n.created).as_millis() as u32;
                // remaining=1.0 renders the outline with an empty interior —
                // the exact canvas the board's fill animates into.
                let (id, px) = render::render_notice(&lines, Some(1.0));
                state.cache_frame(id.clone(), px);
                // Dwell 2s past expiry: a single-entry rotation never advances,
                // and the expiry task's push swaps in the post-notice view
                // before the dwell could matter anyway.
                let rotation = vec![RotationEntry {
                    frame_id: id,
                    dwell_ms: total_ms + 2000,
                    scroll: false,
                    pin_rows: 0,
                }];
                return Manifest {
        mode: String::new(),
        modes: Vec::new(),
                    clock_hour24: false,
                    // Games / notice path: never a device-drawn clock.
                    local_clock: false,
                    rotation,
                    display_on: true,
                    off_state: cfg.off_state,
                    brightness: cfg.brightness,
                    epoch: n.epoch,
                    countdown_ms: Some(total_ms),
                };
            }
            notices.remove(board_id);
        }
    }

    // Priority 3: a paired board nobody has told what to follow.
    //
    // This is the resting state, not a timed notice — it stays until the board
    // has something to show, because the thing it is asking for is the thing
    // that ends it. Below the notice block so a SLEEPING splash still gets its
    // moment, and gated on scores mode: "off" must stay dark, and a
    // deliberately picked clock on an unconfigured board should show the clock.
    if cfg.mode == MODE_SCORES && board_prefs(&cfg).unconfigured {
        let (id, px) = render::render_notice(&SETUP_LINES, None);
        state.cache_frame(id.clone(), px);
        return Manifest {
        mode: String::new(),
        modes: Vec::new(),
            clock_hour24: false,
            local_clock: false,
            rotation: vec![RotationEntry { frame_id: id, dwell_ms: 60_000, scroll: false, pin_rows: 0 }],
            display_on: true,
            off_state: cfg.off_state,
            brightness: cfg.brightness,
            epoch: 0,
            countdown_ms: None,
        };
    }

    // Priority 4: mode "off" is dark. Not the idle screen — "what the panel
    // shows when there are no games" and "what it shows when you turned it
    // off" are different answers to different questions, and the split is the
    // one thing the old power model got right. The SLEEPING splash still runs
    // first (priority 2) and expires into darkness rather than the clock.
    if cfg.mode == MODE_OFF {
        return dark_manifest(&cfg);
    }

    // Priority 5: a PICKED screen — clock, weather, a fork's screen — held
    // persistently until its timer returns the board to scores. The same
    // renderer as the idle screen; the difference is only what chose it.
    //
    // Resolved against the board's list, so a held "weather.2" renders ITS
    // layout. A held id the list does not name (a legacy row, a fork-removed
    // mode, a stored "blank") renders exactly as it always did: by id, with
    // the flat params, an unknown id drawing the clock.
    if cfg.mode != MODE_SCORES {
        let screens = board_screens(&cfg);
        let frames = match resolve_screen(&screens, cfg.held_screen_id()) {
            Some(screen) => render_screen(state, &cfg, screen, tz),
            None => render_legacy_id(state, &cfg, tz, &cfg.mode),
        };
        return screen_manifest(state, &cfg, frames);
    }

    // Priority 6: scores mode with nothing to show — the configured idle
    // screen.
    idle_manifest(state, &cfg, tz)
}

/// What a board with no preferences shows. Kept beside the notice colours it
/// borrows, and short enough to fit 64px in the fonts `render_notice` uses —
/// `the_setup_screen_fits_the_panel` measures it rather than trusting the count.
const SETUP_LINES: [(&str, bool, embedded_graphics::pixelcolor::Rgb888); 3] = [
    ("ADD TEAMS", true, NOTICE_WHITE),
    ("IN THE APP", false, NOTICE_GREY),
    ("TO GET STARTED", false, NOTICE_GREY),
];

/// A deliberately dark panel. Distinct from `idle_manifest`, which draws the
/// configured off mode — during quiet hours there is nothing to draw.
fn dark_manifest(cfg: &BoardConfig) -> Manifest {
    Manifest {
        mode: String::new(),
        modes: Vec::new(),
        clock_hour24: false,
        rotation: Vec::new(),
        display_on: false,
        local_clock: false,
        off_state: OffState::Blank,
        brightness: cfg.brightness,
        epoch: 0,
        countdown_ms: None,
    }
}

/// The idle panel: the board's checked screens, or darkness.
///
/// - None checked: dark, exactly as a "blank" idle screen always was.
/// - One: exactly the manifest a single idle screen always produced (epoch 0,
///   the render's own dwell) — the deploy-day invariant rests on this, since
///   every board that never saved a list has at most one.
/// - Two or more: ONE rotation the board advances on its own clock, each
///   screen's first frame held for the board's idle timer. Its epoch is a
///   hash of the members and the timer, so a minute re-push (new clock frame
///   ids, same shape) keeps the board's slot and timer, while a change of
///   membership, order or timer — or arriving from games, a notice or a held
///   screen — starts over at the first screen. Param edits keep the epoch.
fn idle_manifest(state: &AppState, cfg: &BoardConfig, tz: Option<chrono_tz::Tz>) -> Manifest {
    let screens = board_screens(cfg);
    let members: Vec<&ScreenConfig> = screens.iter().filter(|s| s.idle).collect();
    if members.len() < 2 {
        let frames = match members.first() {
            Some(screen) => render_screen(state, cfg, screen, tz),
            None => Vec::new(),
        };
        return screen_manifest(state, cfg, frames);
    }
    let dwell_ms = cfg.idle_cycle_ms();
    let ids: Vec<&str> = members.iter().map(|s| s.id.as_str()).collect();
    let rotation: Vec<RotationEntry> = members
        .iter()
        .filter_map(|screen| {
            // One frame per screen: a fork returning several would otherwise
            // change the rotation's length (and reset the board) on a whim.
            let f = render_screen(state, cfg, screen, tz).into_iter().next()?;
            state.cache_frame(f.id.clone(), f.px);
            Some(RotationEntry { frame_id: f.id, dwell_ms, scroll: false, pin_rows: 0 })
        })
        .collect();
    Manifest {
        mode: String::new(),
        modes: Vec::new(),
        local_clock: false,
        clock_hour24: cfg.off_mode_params.get(offmode::HOUR24).map(String::as_str) == Some("1"),
        display_on: !rotation.is_empty(),
        off_state: if rotation.is_empty() { OffState::Blank } else { OffState::Clock },
        rotation,
        brightness: cfg.brightness,
        epoch: idle_epoch(&ids, dwell_ms),
        countdown_ms: None,
    }
}

/// What a screen instance renders with — protocol's `layered_params` against
/// this server's catalog, the same rule the app's settings page reads with.
fn layered_params(cfg: &BoardConfig, screen: &ScreenConfig) -> std::collections::BTreeMap<String, String> {
    scoreboard_protocol::layered_params(&cfg.off_mode_params, screen, &offmode::off_modes())
}

/// THE render seam for a screen instance: its KIND, never its id. An instance
/// id ("weather.2") passed to `offmode` would silently draw a clock — the
/// unknown-id fallback — and ship looking fine. Weather comes from the
/// board-wide location.
fn render_screen(
    state: &AppState,
    cfg: &BoardConfig,
    screen: &ScreenConfig,
    tz: Option<chrono_tz::Tz>,
) -> Vec<offmode::OffFrame> {
    let params = layered_params(cfg, screen);
    offmode::render_off_mode(
        &screen.kind,
        &offmode::OffCtx { tz, params: &params, preview: false, weather: board_weather(state, cfg) },
    )
}

/// A held id the board's list does not name, rendered the way every held mode
/// was before lists: by id, with the flat params. Only reachable for legacy
/// and fork-removed ids, which is why it may pass an id to `offmode`.
fn render_legacy_id(
    state: &AppState,
    cfg: &BoardConfig,
    tz: Option<chrono_tz::Tz>,
    id: &str,
) -> Vec<offmode::OffFrame> {
    offmode::render_off_mode(
        id,
        &offmode::OffCtx { tz, params: &cfg.off_mode_params, preview: false, weather: board_weather(state, cfg) },
    )
}

/// The weather reading for the board's (board-wide) location.
fn board_weather(state: &AppState, cfg: &BoardConfig) -> weather::Reading {
    state.weather.get(cfg.off_mode_params.get(offmode::LOCATION).map(String::as_str).unwrap_or(""))
}

/// One screen's frames as a manifest — the idle screen, or a screen held by
/// the picker.
///
/// Extracted so the `display_on` / `off_state` derivation exists once — quiet
/// hours, the idle path, and a picked mode must not be able to disagree
/// about it.
fn screen_manifest(state: &AppState, cfg: &BoardConfig, frames: Vec<offmode::OffFrame>) -> Manifest {
    let rotation: Vec<RotationEntry> = frames
        .into_iter()
        .map(|f| {
            state.cache_frame(f.id.clone(), f.px);
            RotationEntry { frame_id: f.id, dwell_ms: f.dwell_ms, scroll: false, pin_rows: 0 }
        })
        .collect();

    Manifest {
        mode: String::new(),
        modes: Vec::new(),
        // Only the idle path can be a local clock, and only when it drew
        // something. The rendered frame still ships alongside it.
        // Always false. The board used to draw its own clock from this, which
        // meant two renderers for one screen; it has one now. Kept on the wire
        // because firmware that predates the change still reads the field, and
        // false is exactly what we want to tell it.
        local_clock: false,
        // So the offline clock keeps the format the app chose.
        clock_hour24: cfg.off_mode_params.get(offmode::HOUR24).map(String::as_str) == Some("1"),
        display_on: !rotation.is_empty(),
        // DERIVED, not copied. `OffState` is a closed enum with no serde
        // default on the wire, and firmware drops an unparseable message
        // silently — so a board sent a novel value would freeze on its last
        // manifest forever. Boards learn only "dark or not".
        off_state: if rotation.is_empty() { OffState::Blank } else { OffState::Clock },
        rotation,
        brightness: cfg.brightness,
        epoch: 0,
        countdown_ms: None,
    }
}

/// Send the board its current manifest — skipped when identical to the last one
/// sent, so periodic refreshes don't reset the rotation position mid-dwell.
/// Re-anchor a board's wall clock. Cheap and idempotent, so it rides along
/// with the periodic tick: the board only needs an occasional correction —
/// it advances the clock itself between these — and a DST change lands as an
/// ordinary re-anchor rather than needing rules on a device with no tz data.
pub fn push_time(state: &AppState, board_id: &str) {
    let Some(tx) = state.devices.lock().unwrap().get(board_id).cloned() else { return };
    let cfg = state.config_for(board_id);
    let msg = ServerMsg::Time { local_epoch_ms: board_local_epoch_ms(&cfg) };
    let _ = tx.send(Message::Text(serde_json::to_string(&msg).unwrap()));
}

/// Re-push the manifest to every connected board.
///
/// The collect lands in a `let` on purpose, and this function exists so that
/// stays true in one place. A guard created in a `for` iterator expression
/// lives for the WHOLE loop body; `push_board` locks `devices` again; std's
/// Mutex is not reentrant. Written inline, this deadlocked the thread against
/// itself while it held the lock every websocket and HTTP handler needs — the
/// server did not crash or log, it just stopped answering.
/// Record that a background loop is alive. Called at the TOP of each pass, so
/// a loop that wedges mid-pass still goes stale.
/// Generous multiples of each loop's real period: 3 min poll, 10 min weather,
/// 30 s tick. Tight enough to catch a stall within minutes, loose enough that
/// one slow upstream fetch is not an alert. Shared by `/health` and the email
/// watchdog so the two can never disagree about what "stalled" means.
pub const LOOP_LIMITS: [(&str, u64); 3] = [("poll", 600), ("weather", 2400), ("tick", 180)];

pub fn beat(state: &AppState, loop_name: &'static str) {
    state.beats.lock().unwrap().insert(loop_name, Instant::now());
}

/// How long since each loop last checked in, plus the numbers worth graphing.
/// `ok` is false when any loop has missed several turns — the shape a stall
/// takes, whether it is a deadlock, a panic in a spawned task, or a hang in an
/// upstream call with no timeout.
pub fn health_report(state: &AppState) -> (bool, serde_json::Value) {
    // Generous multiples of each loop's real period: 3 min poll, 10 min
    // weather, 30 s tick. Tight enough to catch a stall within minutes, loose
    // enough that one slow upstream fetch is not an alert.
    let beats = state.beats.lock().unwrap().clone();
    let mut loops = serde_json::Map::new();
    let mut ok = true;
    for (name, limit) in LOOP_LIMITS {
        let age = beats.get(name).map(|t| t.elapsed().as_secs());
        // A loop that has never beaten is starting up, not stalled.
        let stale = age.is_some_and(|a| a > limit);
        ok &= !stale;
        loops.insert(
            name.to_string(),
            serde_json::json!({ "age_secs": age, "limit_secs": limit, "stale": stale }),
        );
    }
    let report = serde_json::json!({
        "ok": ok,
        "loops": loops,
        "boards_connected": state.devices.lock().unwrap().len(),
        "controllers_connected": state.controllers.lock().unwrap().len(),
        "boards_known": state.configs.lock().unwrap().len(),
        "games": state.games.lock().unwrap().len(),
        "frames_cached": state.frames.lock().unwrap().len(),
        "version": env!("CARGO_PKG_VERSION"),
    });
    (ok, report)
}

/// GET /debug/{board_id} — the device console this board has streamed, oldest
/// first, as plain text.
///
/// Plain text and not JSON because the only thing anyone does with it is read
/// it, usually through `curl`. Empty until the board is asked to stream (the
/// app's Debug logging toggle); a board that was never asked returns nothing
/// rather than 404, since "no lines yet" and "unknown board" look the same to
/// someone waiting for a fault to reproduce.
async fn get_debug_log(
    State(s): State<AppState>,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(board_id) = resolve_debug_token(&s, &token) else {
        // Same answer for expired and never-existed: a distinguishable "expired"
        // would confirm which tokens were once real.
        return (
            axum::http::StatusCode::NOT_FOUND,
            [(axum::http::header::CACHE_CONTROL, "no-store")],
            "no such debug session\n",
        )
            .into_response();
    };
    let body = s
        .debug_logs
        .lock()
        .unwrap()
        .get(&board_id)
        .map(|b| b.iter().cloned().collect::<Vec<_>>().join("\n"))
        .unwrap_or_default();
    (
        axum::http::StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

/// GET /health — liveness for an uptime monitor, and the few numbers worth
/// watching. Returns 503 when a loop has stalled, so a platform healthcheck
/// restarts the container instead of leaving it up and useless.
async fn get_health(State(s): State<AppState>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let (ok, report) = health_report(&s);
    let code = if ok {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        [
            (axum::http::header::CONTENT_TYPE, "application/json"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    )
        .into_response()
}

pub fn push_all_connected(state: &AppState) {
    let connected: Vec<String> = state.devices.lock().unwrap().keys().cloned().collect();
    for bid in connected {
        push_board(state, &bid);
    }
}

pub fn push_board(state: &AppState, board_id: &str) {
    // Boot hold: before the first sweep, a scores-mode board would be told
    // "idle" purely because the process is young. Send nothing instead — the
    // board keeps whatever it was showing through the restart (its offline
    // grace held the frame), and the first poll's push follows in seconds.
    // Every state that does NOT depend on the slate — setup, quiet, a notice,
    // a picked mode, off, demo (whose slate is seeded at boot) — pushes
    // normally.
    if !state.slate_ready.load(std::sync::atomic::Ordering::Relaxed) {
        let cfg = state.config_for(board_id);
        let would_claim_idle = cfg.mode == MODE_SCORES
            && !cfg.demo_data
            && !board_prefs(&cfg).unconfigured
            && !quiet_now(&cfg)
            && !state.notices.lock().unwrap().contains_key(board_id);
        if would_claim_idle {
            return;
        }
    }
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

/// Show a transient full-screen notice on one board. The manifest render
/// pre-bakes the WHOLE progress-bar sweep as one dwell-timed rotation (see
/// `manifest_for_board`), so the board advances the bar on its own clock with
/// zero network traffic per step. One task remains, solely to clear the notice
/// at expiry — its push re-renders, sees the notice lapsed, and swaps in the
/// post-notice view.
pub fn set_notice(state: &AppState, board_id: &str, lines: &[(&str, bool, embedded_graphics::pixelcolor::Rgb888)], secs: u64) {
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
/// retire it and push the post-notice view. A replaced or already-cleared
/// notice does nothing — a newer showing owns the screen and its own expiry
/// task pushes.
///
/// Notices no longer carry consequences (`then_sleep` died with the NO GAMES
/// countdown): a splash is only ever an announcement, and the mode it
/// announced was already applied before it was shown.
pub(crate) fn finish_notice(state: &AppState, board_id: &str, epoch: u64) {
    let mine = {
        let mut notices = state.notices.lock().unwrap();
        match notices.get(board_id) {
            Some(n) if n.epoch == epoch => {
                notices.remove(board_id);
                true
            }
            _ => false,
        }
    };
    if mine {
        push_board(state, board_id);
    }
}

/// What a pick (the knob's SelectMode, the app's SetMode) resolved to.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    /// "scores" or "off".
    Mode(&'static str),
    /// Hold this screen instance (`id`) of `kind`.
    Screen { id: String, kind: String },
}

/// Resolve a picked id against the board — ONE resolver for the knob and the
/// app, so they cannot disagree about what a pick means. `None` = ignore it.
///
/// - "scores" and "off" are themselves; "blank" is "off" (a picked dark
///   screen IS off, and holding "blank" left a dark panel with power=true
///   and a timer keyed by nothing anyone can edit).
/// - A board that never saved a list runs the pre-list path exactly: a
///   legacy "weather-x" migrates INTO the flat params (that is where an
///   unsaved board's layout lives) and any offered mode is held by kind.
/// - A board with a list resolves against it: the exact instance; else a
///   legacy "weather-x" names the instance of that kind showing layout x;
///   else the kind's first instance. A kind the user removed — a queued
///   offline knob pick, an old app's tile — is ignored, never guessed. A pick
///   never changes any screen's params.
pub fn resolve_pick(cfg: &mut BoardConfig, pick: &str) -> Option<Pick> {
    if pick == MODE_SCORES {
        return Some(Pick::Mode(MODE_SCORES));
    }
    if pick == MODE_OFF || pick == OFF_MODE_BLANK {
        return Some(Pick::Mode(MODE_OFF));
    }
    if cfg.screens.is_empty() {
        let kind = offmode::migrate(pick, &mut cfg.off_mode_params);
        if kind == OFF_MODE_BLANK {
            return Some(Pick::Mode(MODE_OFF));
        }
        return offmode::offers(&kind).then(|| Pick::Screen { id: kind.clone(), kind });
    }
    let screens = board_screens(cfg);
    if let Some(s) = screens.iter().find(|s| s.id == pick) {
        return Some(Pick::Screen { id: s.id.clone(), kind: s.kind.clone() });
    }
    let mut named = std::collections::BTreeMap::new();
    let kind = offmode::migrate(pick, &mut named);
    let catalog = offmode::off_modes();
    let wants = |s: &ScreenConfig| {
        named.iter().all(|(k, v)| {
            catalog.iter().find(|d| d.id == s.kind).is_some_and(|d| d.value(k, &s.params) == v.as_str())
        })
    };
    screens
        .iter()
        .filter(|s| s.kind == kind && !s.is_fixed())
        .find(|s| !named.is_empty() && wants(s))
        .or_else(|| screens.iter().find(|s| s.kind == kind && !s.is_fixed()))
        .map(|s| Pick::Screen { id: s.id.clone(), kind: s.kind.clone() })
}

/// Apply a resolved pick through the one gesture transition.
pub fn apply_pick(cfg: &mut BoardConfig, pick: &Pick) {
    match pick {
        Pick::Mode(mode) => apply_mode(cfg, mode),
        Pick::Screen { id, kind } => apply_screen(cfg, id, kind),
    }
}

/// Does this pick hold a screen whose reading must be fetched?
pub fn pick_needs_weather(pick: &Pick) -> bool {
    matches!(pick, Pick::Screen { kind, .. } if offmode::needs_location(kind))
}

/// Fetch weather now and re-push the board — a weather screen just chosen
/// should not stare at UNAVAILABLE until the refresh loop comes round.
/// No-op outside a tokio runtime (sync tests), the same rule as
/// `set_notice`'s expiry task.
pub fn refresh_weather_now(state: &AppState, board_id: &str) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let st = state.clone();
        let board = board_id.to_string();
        handle.spawn(async move {
            weather::refresh_all(&st.weather, weather_locations(&st)).await;
            push_board(&st, &board);
        });
    }
}

/// A screen picked on the board's own panel. The board offered ids from
/// `Manifest.modes`, but the socket is not a trust boundary the picker can
/// lean on — validate exactly like the app's SetMode (`resolve_pick`).
pub fn handle_select_mode(state: &AppState, board_id: &str, mode: &str) {
    let mut cfg = state.config_for(board_id);
    let Some(pick) = resolve_pick(&mut cfg, mode) else {
        tracing::warn!("board '{board_id}': picker sent unknown or removed screen {mode:?}, ignoring");
        // The knob may be showing a list that no longer exists (an offline
        // pick of a since-deleted screen): re-send the current one, even if
        // it is identical to what the dedup last sent.
        state.forget_last_manifest(board_id);
        push_board(state, board_id);
        return;
    };
    apply_pick(&mut cfg, &pick);
    state.configs.lock().unwrap().insert(board_id.to_string(), cfg.clone());
    state.persist_config(board_id, &cfg);
    handle_mode_change(state, board_id, &cfg);
    // Same immediate-fetch rule as the app's pick.
    if pick_needs_weather(&pick) {
        refresh_weather_now(state, board_id);
    }
    tracing::info!("board '{board_id}': picker -> {}", cfg.held_screen_id());
    push_board(state, board_id);
    push_board_state(state, board_id);
}

/// A single knob press from PRE-PICKER firmware. Stage-2 firmware opens a
/// local mode picker instead and never sends this — but this mapping is
/// permanent mixed-fleet surface, not scaffolding: a board that never updates
/// keeps a working button forever.
///
/// - inside the quiet window -> override for the rest of tonight. This arm is
///   FIRST and load-bearing: without it a night-time press falls through and
///   arms "off" against an already-dark panel, and the user presses twice.
/// - any lit mode -> "off", with off's configured timer — the old button's
///   sleep, minus the no-games countdown.
/// - "off" -> "scores". Waking to scores rather than the prior mode matches
///   what the legacy button always meant.
pub fn handle_button(state: &AppState, board_id: &str) {
    let mut cfg = state.config_for(board_id);
    if quiet_now(&cfg) {
        cfg.quiet_override = true;
        state.notices.lock().unwrap().remove(board_id);
        state.configs.lock().unwrap().insert(board_id.to_string(), cfg.clone());
        state.persist_config(board_id, &cfg);
        tracing::info!("board '{board_id}': button overrode quiet hours for the rest of the window");
        push_board(state, board_id);
        push_board_state(state, board_id);
        return;
    }

    let target = if cfg.mode == MODE_OFF { MODE_SCORES } else { MODE_OFF };
    apply_mode(&mut cfg, target);
    state.configs.lock().unwrap().insert(board_id.to_string(), cfg.clone());
    state.persist_config(board_id, &cfg);
    handle_mode_change(state, board_id, &cfg);
    tracing::info!("board '{board_id}': button -> {target}");
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

/// A board reported an IANA timezone — adopt it for this board's clock and
/// start-time labels. DST is automatic because the NAME is stored and the tz
/// database supplies the offset. Unparseable names are ignored.
///
/// LEGACY. Boards used to IP-geolocate themselves at boot with a cleartext GET
/// to a third party and report the answer here. That call is gone from the
/// firmware — it was the last unencrypted third-party request the board made —
/// so current boards never send this. It is kept because boards on older
/// firmware still do, and honouring them costs nothing. A current board's zone
/// comes from the app ([`Command::SetTimezone`]) or, failing that, from
/// `DEFAULT_TIMEZONE` ([`default_tz`]).
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

/// What a connection's socket peer address is worth, as an answer to "where is
/// this board?".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeerOrigin {
    /// A globally-routable unicast address: this really is the far end's own
    /// public IP, and geolocating it would describe where the board lives.
    Public,
    /// RFC1918, CGNAT (100.64/10), loopback, link-local, or otherwise
    /// non-global: an infrastructure hop or a LAN, NOT the board's location.
    /// Railway's TCP proxy lands here.
    Infrastructure,
}

/// Classify a socket peer address.
///
/// The board's own public IP is the one input a server-side geolocation would
/// need, and behind a Layer-4 proxy it is simply not present in the connection:
/// the proxy dials us from its own address and there is no `X-Forwarded-For` to
/// consult, because there is no HTTP layer to put it in. `Infrastructure` is
/// therefore a statement about what we CANNOT know, and the only correct
/// response to it is to change nothing.
pub(crate) fn classify_peer(ip: std::net::IpAddr) -> PeerOrigin {
    use std::net::IpAddr;
    let global = match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                // Carrier-grade NAT, 100.64.0.0/10 — what container platforms
                // (Railway included) hand their internal networks. `std` has no
                // stable predicate for it.
                || (o[0] == 100 && (64..128).contains(&o[1]))
                // 0.0.0.0/8 and 240.0.0.0/4 (reserved).
                || o[0] == 0
                || o[0] >= 240)
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // Unique-local fc00::/7 and link-local fe80::/10.
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] & 0xffc0) == 0xfe80)
        }
    };
    if global { PeerOrigin::Public } else { PeerOrigin::Infrastructure }
}

/// Say, once per device connection, what the socket actually reports about
/// where this board is — and what that means for its timezone.
///
/// This is a diagnostic, not a mechanism: it never changes a board's zone.
/// Server-side geolocation was the obvious replacement for the firmware's old
/// cleartext lookup, and it does not work on this topology — Railway's raw TCP
/// proxy is a Layer-4 relay, so the address we accept the board's connection
/// from is the proxy's, inside Railway's network. Rather than assert that from
/// documentation alone, the server prints what it sees on every board connect,
/// so `railway logs` settles it.
pub fn note_device_peer(board_id: &str, peer: Option<std::net::SocketAddr>) {
    match peer {
        Some(addr) => match classify_peer(addr.ip()) {
            PeerOrigin::Infrastructure => tracing::info!(
                "board '{board_id}' connected from {addr} — NOT a public address, so this is \
                 an infrastructure hop (Railway's TCP proxy) or a LAN, not the board. No \
                 timezone can be inferred from it; keeping the configured zone."
            ),
            PeerOrigin::Public => tracing::info!(
                "board '{board_id}' connected from public address {} — this deployment is \
                 reached directly, without a Layer-4 proxy in front of it",
                addr.ip()
            ),
        },
        // The listener stamps every connection it accepts, so this means the
        // socket did not come from `serve::serve` at all (a test harness).
        None => tracing::debug!("board '{board_id}' connected with no connection info"),
    }
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
    // Answer the DEVICE every time, changed or not: a board that reconnects
    // while its stale loop still holds the old entry (OTA reset, power blip)
    // reports an unchanged version but has a fresh menu with no answer — the
    // report doubles as the ask.
    push_update_available(state, board_id);
}

/// Tell the DEVICE what a `RequestUpdate` would install right now
/// (`ServerMsg::UpdateAvailable`; None = up to date) — the settings menu's
/// UPDATE row renders exactly this. Runs the same `offer_for` pick as
/// `board_state_msg` and `handle_update_firmware`, so the menu, the app's
/// Update button, and the actual offer can never disagree. No reported version
/// or an offline board is a quiet no-op: with no version there is no answer to
/// compute, and every (re)connect re-reports, which re-asks.
pub fn push_update_available(state: &AppState, board_id: &str) {
    let Some(running) = state.device_versions.lock().unwrap().get(board_id).cloned() else {
        return;
    };
    let channel = state.config_for(board_id).fw_channel;
    let version =
        offer_for(&state.firmware, channel, &running).map(|(_, fw)| fw.manifest.version.clone());
    if let Some(tx) = state.devices.lock().unwrap().get(board_id) {
        let _ = tx.send(Message::Text(
            serde_json::to_string(&ServerMsg::UpdateAvailable { version }).unwrap(),
        ));
    }
}

/// Mint a fresh one-time download token for `board_id` and return the
/// tokenized image path for a dist that lives in `chan`
/// (`/firmware/{chan}.bin?u={nonce}`). Sweeps expired tokens as it inserts.
/// The token scopes the streaming GET route to the
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
/// Put a config in a mode. The ONE place mode transitions happen — the app's
/// picker, the legacy SetPower alias, and the physical button all come
/// through here, so they cannot disagree about the side effects.
///
/// - Any pick during the quiet window sets the override — INCLUDING re-picking
///   the current mode. The 2am gesture is "answer me now", and a pick that
///   changed nothing visible would read as a dead button.
/// - A non-scores mode arms its configured timer (`mode_hours`, falling back
///   to `sleep_hours`; 0 = until changed). Scores clears it. The tick_once
///   sweep returns the board to scores when the timer lapses, so no board can
///   be forgotten on a mode.
pub fn apply_mode(cfg: &mut BoardConfig, mode: &str) {
    apply_screen(cfg, mode, mode);
}

/// `apply_mode` for a screen INSTANCE: hold `id` (of `kind`), timed by the
/// instance's own hours, else its kind's, else `sleep_hours`. A mode id is
/// its own kind, so `apply_mode` is exactly this.
pub fn apply_screen(cfg: &mut BoardConfig, id: &str, kind: &str) {
    if quiet_now(cfg) {
        cfg.quiet_override = true;
    }
    cfg.set_held_screen(id, kind);
    cfg.wake_at = if kind == MODE_SCORES {
        None
    } else {
        match cfg.hold_hours(id, kind).min(72) {
            0 => None,
            h => Some(unix_now() + h as u64 * 3600),
        }
    };
}

/// Back to Sports because the held screen stopped existing — removed from
/// the list, dropped by an import, or unreadable at load. NOT a gesture, so
/// unlike `apply_mode` it never sets the quiet override: deleting a screen at
/// 1am must leave a quiet board dark. The caller still runs
/// `handle_mode_change` (outside the config lock) for the idle handoff.
pub fn release_to_scores(cfg: &mut BoardConfig) {
    cfg.set_mode(MODE_SCORES);
    cfg.wake_at = None;
}

/// Does the board need its (board-wide) location fetched: is a checked idle
/// screen, or the held screen, one that declares a location?
pub fn board_needs_weather(cfg: &BoardConfig) -> bool {
    let screens = board_screens(cfg);
    if screens.iter().any(|s| s.idle && offmode::needs_location(&s.kind)) {
        return true;
    }
    if cfg.mode == MODE_SCORES || cfg.mode == MODE_OFF {
        return false;
    }
    match resolve_screen(&screens, cfg.held_screen_id()) {
        Some(s) => offmode::needs_location(&s.kind),
        None => offmode::needs_location(&cfg.mode),
    }
}

/// The location this board needs fetched, if any.
pub fn board_weather_location(cfg: &BoardConfig) -> Option<String> {
    if !board_needs_weather(cfg) {
        return None;
    }
    cfg.off_mode_params.get(offmode::LOCATION).filter(|l| !l.trim().is_empty()).cloned()
}

/// How long the idle-arrival announcement holds before the idle screen (or
/// darkness) takes over. Long enough to be read from across a room, short
/// enough that the idle screen is what the evening actually shows.
const IDLE_NOTICE_SECS: u64 = 120;

/// The feedback half of a mode change, outside the config lock: clear any
/// lingering splash, announce "off" — darkness alone doesn't say whether the
/// press registered or the panel died — and announce the idle HANDOFF when
/// scores mode has nothing to show. Without that last one, picking Scores on
/// a gameless evening flipped the panel straight to the weather screen, which
/// read as being in weather mode: the board looked like it ignored the pick.
pub fn handle_mode_change(state: &AppState, board_id: &str, cfg: &BoardConfig) {
    state.notices.lock().unwrap().remove(board_id);
    if cfg.mode == MODE_OFF {
        let caption = match cfg.mode_duration_hours(MODE_OFF).min(72) {
            0 => "UNTIL TURNED ON".to_string(),
            1 => "FOR 1 HOUR".to_string(),
            h => format!("FOR {h} HOURS"),
        };
        set_notice(state, board_id, &[("SLEEPING", true, NOTICE_WHITE), (&caption, false, NOTICE_GREY)], SPLASH_SECS);
    } else if cfg.mode == MODE_SCORES && chosen_games(state, cfg).is_empty() {
        idle_arrival_notice(state, board_id, cfg);
    }
}

/// "NO GAMES / WEATHER IN / 2 MINUTES" — the countdown bar underneath is the
/// device's own fill, like every notice. A game appearing mid-countdown
/// clears it (priority 1 always wins), and its expiry simply reveals the idle
/// screen it named.
fn idle_arrival_notice(state: &AppState, board_id: &str, cfg: &BoardConfig) {
    let idle = cfg.off_mode_id();
    let line2 = if offmode::draws_nothing(idle) {
        "SCREEN OFF IN".to_string()
    } else {
        let label = offmode::off_modes()
            .iter()
            .find(|m| m.id == idle)
            .map(|m| m.label.to_uppercase())
            .unwrap_or_else(|| idle.to_uppercase());
        // A fork's long label must not overrun the 64px line; the first ten
        // characters carry the identity.
        format!("{} IN", label.chars().take(10).collect::<String>())
    };
    set_notice(
        state,
        board_id,
        &[
            ("NO GAMES", true, NOTICE_WHITE),
            (&line2, false, NOTICE_GREY),
            ("2 MINUTES", false, NOTICE_GREY),
        ],
        IDLE_NOTICE_SECS,
    );
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
    // Union of both slates. Scoped to the live one, this would strip a demo
    // board's per-game spoiler hides on the very next tick and the hide would
    // silently un-stick.
    let ids: HashSet<String> = state
        .games
        .lock()
        .unwrap()
        .iter()
        .chain(state.demo.lock().unwrap().iter())
        .map(|g| g.id.clone())
        .collect();
    let stale: Vec<(String, BoardConfig)> = {
        let configs = state.configs.lock().unwrap();
        configs
            .iter()
            .filter(|(_, c)| {
                c.spoiler_game_ids.iter().any(|id| !ids.contains(id))
                    || c.revealed_game_ids.iter().any(|id| !ids.contains(id))
            })
            .map(|(b, c)| (b.clone(), c.clone()))
            .collect()
    };
    for (bid, mut cfg) in stale {
        cfg.spoiler_game_ids.retain(|id| ids.contains(id));
        cfg.revealed_game_ids.retain(|id| ids.contains(id));
        state.configs.lock().unwrap().insert(bid.clone(), cfg.clone());
        state.persist_config(&bid, &cfg);
    }
}

/// Every location a board is actually configured to show. Nothing else is
/// fetched — the services are free, but they are somebody else's.
pub fn weather_locations(state: &AppState) -> Vec<String> {
    let mut out: Vec<String> = state
        .configs
        .lock()
        .unwrap()
        .values()
        .filter_map(board_weather_location)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Keep the weather cache warm. Separate from `poll_loop` because the cadences
/// have nothing to do with each other: scores move by the minute, weather by
/// the hour.
pub async fn weather_loop(state: AppState) {
    loop {
        let locations = weather_locations(&state);
        beat(&state, "weather");
        if !locations.is_empty() {
            weather::refresh_all(&state.weather, locations).await;
            // A fresh reading can change every weather board's frame.
            push_all_connected(&state);
        }
        tokio::time::sleep(weather::REFRESH).await;
    }
}

pub fn tick_once(state: &AppState) {
    beat(state, "tick");
    refresh_pairing_codes(state);
    sweep_stale_pins(state);
    // Re-anchor every connected board's clock. The board runs its own clock
    // between these; this only corrects drift and carries DST over.
    let connected: Vec<String> = state.devices.lock().unwrap().keys().cloned().collect();
    for bid in &connected {
        push_time(state, bid);
    }

    // An override is spent when the window it overrode ends. Rides the pass
    // that already walks every config every 30s — no scheduler, no per-board
    // task, and no absolute instant to drift across a DST change.
    let expired: Vec<String> = state
        .configs
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, c)| c.quiet_override && !quiet_window_now(c))
        .map(|(id, _)| id.clone())
        .collect();
    for bid in expired {
        let mut cfg = state.config_for(&bid);
        cfg.quiet_override = false;
        state.configs.lock().unwrap().insert(bid.clone(), cfg.clone());
        state.persist_config(&bid, &cfg);
        push_board_state(state, &bid);
    }

    // Mode-timer check covers ALL configured boards (a board can be offline
    // when its timer lapses; the config should still flip so the app agrees).
    // This is the old sleep-timer wake sweep, generalized: `wake_at` now means
    // "when the current non-scores mode expires", so a row persisted mid-sleep
    // by the old binary gets its "waking at 7am" promise honoured by the same
    // code that returns a timed clock mode to scores.
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
        cfg.set_mode(MODE_SCORES);
        cfg.wake_at = None;
        state.configs.lock().unwrap().insert(bid.clone(), cfg.clone());
        state.persist_config(&bid, &cfg);
        // A timer expiring into a gameless evening gets the same idle handoff
        // announcement as a hand-picked Scores — one arrival, one behavior.
        if chosen_games(state, &cfg).is_empty() {
            idle_arrival_notice(state, &bid, &cfg);
        }
        push_board_state(state, &bid);
        tracing::info!("board '{bid}': mode timer lapsed -> scores");
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

/// Clear pins whose game is no longer showable.
///
/// A pin means "this game alone, while it's still live" — `chosen_games`'
/// own words — and the BOARD already honors that scope by falling back to
/// the rotation when the game goes stale. But the CONFIG kept the id
/// forever, and the app dims every other card while `focused_game_id` is
/// set: pin a race, let it finish, and an hour later the whole dashboard is
/// grey with nothing visibly pinned. The pin expires WITH the game now.
///
/// An EMPTY slate keeps pins: that is a feed hiccup, not a game ending, and
/// a pin must not be lost to a bad poll.
fn sweep_stale_pins(state: &AppState) {
    let board_ids: Vec<String> = state.configs.lock().unwrap().keys().cloned().collect();
    for bid in board_ids {
        let cfg = state.config_for(&bid);
        let Some(fid) = cfg.focused_game_id.clone() else { continue };
        let verdict = with_slate(state, Some(&cfg), |games| {
            if games.is_empty() {
                return None; // feed hiccup: no verdict
            }
            Some(games.iter().any(|g| g.id == fid && board_shows(state, g)))
        });
        if verdict == Some(false) {
            tracing::info!("pin on {fid} expired with its game (board {bid})");
            let mut cfg = cfg;
            cfg.focused_game_id = None;
            state.configs.lock().unwrap().insert(bid.clone(), cfg.clone());
            state.persist_config(&bid, &cfg);
            // The board re-renders its rotation and every app un-dims.
            push_board_state(state, &bid);
        }
    }
}

/// Keep a connected board's pairing code redeemable for as long as its panel
/// can show it.
///
/// The code is registered once, when the board connects (`EnterSetup`), and
/// the board displays it on demand from its menu — days later, if it has been
/// up that long. Expiring from the connect instant meant the panel and the
/// server disagreed about a code the user was looking at.
fn refresh_pairing_codes(state: &AppState) {
    let connected: std::collections::HashSet<String> =
        state.devices.lock().unwrap().keys().cloned().collect();
    if connected.is_empty() {
        return;
    }
    let now = Instant::now();
    for (_, (board_id, minted)) in state.pending.lock().unwrap().iter_mut() {
        if connected.contains(board_id) {
            *minted = now;
        }
    }
}

/// Carry a football game's last known possession across a poll that omits it.
///
/// ESPN drops `situation.possession` transiently between plays while
/// `possessionText` and the down keep coming. Without this, one such poll
/// flips the frame from the field bar to the old four-corner text layout for
/// up to POLL_SECS and back again — the layout was observed flapping
/// mid-game. The down and yard line stay the FEED's; only the one omitted
/// bit is bridged, and any stated possession always wins, so a turnover is
/// wrong for at most the gap the feed itself created.
fn bridge_football_possession(state: &AppState, games: &mut [Game]) {
    let old = state.games.lock().unwrap();
    for g in games.iter_mut() {
        let Some(model::Situation::Football { home_possession: hp @ None, .. }) = &mut g.situation
        else {
            continue;
        };
        if let Some(model::Situation::Football { home_possession: prev @ Some(_), .. }) =
            old.iter().find(|o| o.id == g.id).and_then(|o| o.situation.as_ref())
        {
            *hp = *prev;
        }
    }
}

/// Push fresh board state (now-showing list + config) to every app subscribed to
/// this board, pruning any whose socket has closed.
pub fn push_board_state(state: &AppState, board_id: &str) {
    let state_msg = board_state_msg(state, board_id);
    // Binary frames for the now-showing games, pushed right after the state so the
    // app renders them in place — the same content-addressed mechanism the board uses.
    //
    // DELIBERATELY RAW, and staying that way: the compression the DEVICE path
    // negotiates (`ClientMsg::NeedFrames { codecs }`) exists to buy heap back on
    // a 160KB microcontroller. A phone has memory to spare, app-core carries no
    // decoder, and controllers never advertise a codec — so this must keep
    // sending bare pixels with no codec byte. Don't "unify" the two paths.
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
                let of1 = state.openf1_rows.lock().unwrap();
                let view = board_view_with(&cfg, g, of1.as_ref());
                let mut s = to_summary_for(&view, spoiler, tz);
                // The pushed card frame IS the board's frame — same view,
                // same renderer, same content hash, so a now-showing game has
                // ONE frame in the cache and the card cannot diverge from the
                // panel. Both apps crop a deep leaderboard's tall strip to
                // its top 32 rows, which is exactly the panel's resting
                // window. (render_board_frame delegates to the static layout
                // for everything that isn't a deep leaderboard, so for team
                // sports these were already identical pixels.)
                let rows_cap = league_rows_cap(&cfg, &g.sport_id);
                let (fid, px, _) = render::render_board_frame(&view, rows_cap, spoiler, tz, name_style_for(&cfg, &g.sport_id));
                drop(of1);
                // The frame just rendered IS the panel's, so its height sets
                // the card's glide exactly as it sets the rotation's dwell.
                mark_scroll(&mut s, &cfg, g, Some(px.len() / (scoreboard_protocol::FRAME_WIDTH * 2)));
                state.cache_frame(fid.clone(), px);
                s.frame_id = fid;
                s
            })
            .collect();
        // Bumped games ride along flagged, without pre-rendered frames (the app
        // falls back to HTTP rendering for those cards).
        out.extend(bumped.iter().map(|g| {
            let of1 = state.openf1_rows.lock().unwrap();
            let mut s = to_summary_for(&board_view_with(&cfg, g, of1.as_ref()), spoiler_for(&cfg, g), tz);
            drop(of1);
            mark_scroll(&mut s, &cfg, g, None);
            s.bumped = true;
            s
        }));
        // …and then what's COMING. The beta tester opened the app hours before
        // her team played, saw an empty "Now showing", and concluded the board
        // was broken — nothing told her tonight's game was already claimed. So
        // the section is "Live and Upcoming" and the claimed-but-unstarted
        // games ride along with it, using the same predicate the browse feed
        // uses for its "coming to your scoreboard" badge.
        //
        // DISPLAY-ONLY: `chosen_games` / `manifest_for_board` never see this
        // list, so the board's rotation is byte-for-byte what it was.
        let prefs = board_prefs(&cfg);
        let mut upcoming: Vec<Game> = with_slate(state, Some(&cfg), |games| {
            // `game_queued` already requires `!board_shows`, so nothing here can
            // collide with the shown/bumped entries above; the id set only guards
            // against a duplicate arriving twice in one slate.
            let mut seen: HashSet<String> = HashSet::new();
            games
                .iter()
                // NOT-STARTED ONLY. `game_queued` is `!board_shows && claimed`,
                // and a FINAL nobody watched end also satisfies both (the
                // linger stamp only exists for transitions we observed, so the
                // day's earlier finals qualify). Without this guard the section
                // filled with completed games — sorted to the FRONT, since a
                // past start time is the "soonest" — showing final scores under
                // a "Scheduled for your scoreboard" badge. Worst on a virgin
                // board, where every game is claimed. `game_queued`'s other
                // caller is pre-filtered by `browse_shows_now`, which is why
                // this never surfaced there.
                .filter(|g| g.status == GameStatus::Pregame)
                .filter(|g| game_queued(state, &cfg, &prefs, g))
                .filter(|g| seen.insert(g.id.clone()))
                .cloned()
                .collect()
        });
        // Soonest first, so the cap below keeps the most imminent games. A start
        // time we couldn't parse sorts last — we can't claim it's imminent.
        upcoming.sort_by_key(|g| {
            let start = parse_espn_time(&g.start_time);
            (start.is_none(), start)
        });
        out.extend(upcoming.iter().take(UPCOMING_CAP).map(|g| {
            let spoiler = spoiler_for(&cfg, g);
            let of1 = state.openf1_rows.lock().unwrap();
            let mut s = to_summary_for(&board_view_with(&cfg, g, of1.as_ref()), spoiler, tz);
            drop(of1);
            mark_scroll(&mut s, &cfg, g, None);
            s.queued = true;
            // The app's "Live and Upcoming" section keys off this status to tell
            // "on the board right now" from "claimed, hasn't started". The raw
            // feed word is "pregame"; the app-facing word is "upcoming".
            s.status = "upcoming".into();
            // Real cached frame, like the shown entries: a pregame frame is the
            // matchup + start time, so there's no score to leak — and the card
            // shows the same art the board will when the puck drops. Rendered
            // from the VIEW for the same reason as the shown loop.
            let of1 = state.openf1_rows.lock().unwrap();
            let view = board_view_with(&cfg, g, of1.as_ref());
            let (fid, px) = render::render_frame(&view, spoiler, tz, name_style_for(&cfg, &g.sport_id));
            drop(of1);
            state.cache_frame(fid.clone(), px);
            s.frame_id = fid;
            s
        }));
        out
    };
    // A demo board has no device to connect, and "offline" would read as
    // broken to the reviewer it exists for — the server IS the board here.
    let online = is_demo_board(board_id) || state.devices.lock().unwrap().contains_key(board_id);
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
    let quiet_active = quiet_now(&cfg);
    // The RESOLVED screen list rides the copy sent to apps (never the stored
    // row): a non-empty list is how a new app knows this server speaks
    // screens, and an unsaved board's list is synthesized from its legacy
    // fields so the app draws exactly what the board does.
    let mut cfg = cfg;
    cfg.screens = board_screens(&cfg);
    ServerMsg::BoardState {
        board_id: board_id.to_string(),
        online,
        now_showing,
        // Derived here, beside the firmware fields, for the same reason: it is
        // what the config MEANS right now, not part of the config.
        quiet_active,
        config: cfg,
        firmware_version,
        firmware_update,
    }
}

/// Fetch OpenF1 rows on the poll tick — gated on credentials AND on some
/// board actually having selected the source, because the key is a paid
/// personal one and the default costs it nothing. Failures leave the last
/// rows in place; `board_view` ignores rows older than ten minutes, so a
/// dead feed degrades to ESPN rather than freezing.
async fn maybe_fetch_openf1(state: &AppState) {
    let Some(client) = state.openf1.as_ref() else { return };
    let wanted = state.configs.lock().unwrap().values().any(|cfg| {
        cfg.sports
            .iter()
            .any(|s| s.sport_id == "f1" && s.data_source.as_deref() == Some("openf1"))
    });
    if !wanted {
        return;
    }
    if let Some(rows) = client.fetch_latest().await {
        *state.openf1_rows.lock().unwrap() = Some(rows);
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
    // Sampled far faster than the poll period; see the due-check in the loop.
    const POLL_SAMPLE: Duration = Duration::from_secs(5);
    let mut interval = tokio::time::interval(POLL_SAMPLE);
    // When ESPN was last actually hit. `None` = never, so the first tick with a
    // client connected fetches immediately rather than waiting out a full period
    // (which is what an empty slate after a restart looked like).
    let mut last_fetch: Option<Instant> = None;
    // When the slate we're about to compare against was fetched. The idle skip
    // means this can be hours or days back, and `record_finished_transitions`
    // needs it to date an end it only just noticed. None = nothing fetched yet
    // (state.games is empty, so no transition can be observed anyway).
    let mut prev_poll: Option<Instant> = None;
    loop {
        // THE TIMER IS THE ONLY TRIGGER. Connecting does not fetch.
        //
        // It used to: a client connect notified this loop and it swept ESPN
        // immediately, so that a new arrival never looked at data up to
        // POLL_SECS old. That turns client churn into upstream load one for one.
        // A board reconnecting every ~40s on a flaky link had the server
        // sweeping twenty leagues each time — a 24s median against the
        // configured 180s, with a 2.7s floor — which is a good way to get 403'd,
        // and self-feeding besides: every extra sweep made the server slower to
        // answer the next reconnect.
        //
        // Nothing is lost by dropping it. A connecting board is handed the
        // current manifest by `push_board` before this loop would have heard
        // anything, so the fetch was never what put its screen up; it only ever
        // made the numbers fresher, and the next tick does that anyway.
        interval.tick().await;
        // Skip while nothing is listening — no client, no reason to pull.
        let idle = {
            let no_devices = state.devices.lock().unwrap().is_empty();
            let no_controllers = state.controllers.lock().unwrap().is_empty();
            no_devices && no_controllers
        };
        if idle {
            continue;
        }
        // Due yet? The gate is ELAPSED TIME since the last fetch, never a client
        // event, which is what keeps a reconnect from buying anyone a sweep.
        //
        // The check runs often and the gate is slow, deliberately. Ticking at
        // the poll period instead made fetching depend on a 180s-spaced instant
        // happening to land while a client was connected — and a board on a
        // flaky link is disconnected a good fraction of the time, so ticks fell
        // in the gaps and the slate went empty for minutes at a stretch. Sampling
        // often and fetching slowly decouples the two: the cadence is still one
        // sweep per `secs`, and a client present for any moment of that window
        // gets it.
        if last_fetch.is_some_and(|t| t.elapsed() < Duration::from_secs(secs)) {
            continue;
        }
        beat(&state, "poll");
        let fetched_at = Instant::now();
        last_fetch = Some(fetched_at);
        let mut games = source.fetch().await;
        maybe_fetch_openf1(&state).await;
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
        // The synthetic slate, refreshed on the same cadence and through the
        // same ingestion filter. Always generated: it costs no network and
        // microseconds of CPU, and a board that toggles mid-window (or a hit on
        // /demo/games) must never find it empty.
        let mut demo = state.demo_source.fetch().await;
        demo.retain(|g| ingest_keeps(g, now));
        // `state.finished` is ONE id-keyed map shared by both slates, so the
        // transition scan has to see both — otherwise each poll's retain would
        // evict the other slate's stamps. Ids can't collide: ESPN ids are all
        // digits, demo ids are `demo-*`.
        let mut both = games.clone();
        both.extend(demo.iter().cloned());
        // Stamp any live -> Final transitions before the old slate disappears,
        // dated to the slate they're measured against (not to now).
        record_finished_transitions(&state, &both, prev_poll.unwrap_or(fetched_at));
        prev_poll = Some(fetched_at);
        // Hand the observed finish time back to the slate, so the frames, the
        // app's cards and GET /game all read one value. Patched on the STORED
        // slate rather than in the summary builders because ESPN hands us a
        // fresh, end-time-less copy every poll — this is the one place the
        // server's own observation and the feed's data are merged.
        stamp_observed_end_times(&state, &mut games);
        stamp_observed_end_times(&state, &mut demo);
        bridge_football_possession(&state, &mut games);
        *state.demo.lock().unwrap() = demo;
        *state.games.lock().unwrap() = games;
        // Unconditionally, even when the sweep came back empty: a genuinely
        // gameless day should idle honestly. The flag distinguishes "never
        // fetched", nothing more.
        state.slate_ready.store(true, std::sync::atomic::Ordering::Relaxed);
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
///
/// The LABELS carry a load the ids don't: a beta tester read the old
/// "Off / All playoffs / Every match" as a master switch for the whole league
/// and assumed picking "Off" would blank their favorite teams too. It never
/// did — this setting is purely ADDITIVE to the teams you picked. So every
/// label now says so out loud: "Only selected teams" is the floor, and each
/// middle tier reads "Selected + <the extra games>". Nobody should have to
/// learn that from a support thread.
///
/// The ids are the contract (boards store them, `champ_option_covers` matches
/// on them) and must never change; only these strings may.
///
/// Leaderboard sports (golf, F1) are the exception: they have no teams to
/// select — you subscribe by event kind — so "selected teams" would be a lie
/// there and their labels stay event-shaped.
/// The rounds a league offers as INDEPENDENT checkboxes.
///
/// Every id here except `"all"` is a tier `classify_champ` can actually emit
/// for that sport — a checkbox for a tier nothing produces is a control that
/// does nothing, and `every_round_is_a_tier_the_classifier_emits` enforces it.
///
/// Nothing nests. Checking "Conference Finals" does not add the Final; the old
/// single-select did, invisibly, which is what made its labels unreadable.
pub(crate) fn championship_rounds_for(sport_id: &str) -> Vec<ChampionshipOption> {
    let opts: &[(&str, &str)] = match sport_id {
        "nhl" => &[
            ("final", "Stanley Cup Final"),
            ("conf", "Conference Finals"),
            ("playoffs", "Earlier playoff rounds"),
            ("all", "Every game"),
        ],
        "nba" => &[("final", "NBA Finals"), ("playoffs", "Earlier playoff rounds"), ("all", "Every game")],
        "wnba" => &[("final", "WNBA Finals"), ("playoffs", "Earlier playoff rounds"), ("all", "Every game")],
        "mlb" => &[("ws", "World Series"), ("playoffs", "Earlier playoff rounds"), ("all", "Every game")],
        "nfl" => &[("sb", "Super Bowl"), ("playoffs", "Earlier playoff rounds"), ("all", "Every game")],
        "cfb" => &[
            ("playoff", "Playoff games"),
            ("bowl", "Bowl games"),
            ("playoffs", "Other postseason"),
            ("all", "Every game"),
        ],
        // NOT "Conference tournaments": those are regular season to ESPN and
        // never get a tier at all. What "playoffs" actually catches here is the
        // rest of the postseason field — the NIT, the WBIT, the Crown.
        "cbb" | "wcbb" => &[
            ("tournament", "March Madness"),
            ("playoffs", "NIT & other postseason"),
            ("all", "Every game"),
        ],
        "mls" => &[("cup", "MLS Cup"), ("playoffs", "Earlier playoff rounds"), ("all", "Every match")],
        "wc" => &[
            ("final", "Final"),
            ("third", "Third-place match"),
            ("semi", "Semifinals"),
            ("quarter", "Quarterfinals"),
            ("knockout", "Round of 16"),
            ("all", "Every match"),
        ],
        "wwc" => &[
            ("final", "Final"),
            ("third", "Third-place match"),
            ("semi", "Semifinals"),
            ("quarter", "Quarterfinals"),
            ("knockout", "Round of 16"),
            ("all", "Every match"),
        ],
        // The Champions League has no third-place match; its "knockout" also
        // covers the knockout-round playoffs that feed the round of 16.
        "ucl" => &[
            ("final", "Final"),
            ("semi", "Semifinals"),
            ("quarter", "Quarterfinals"),
            ("knockout", "Knockout rounds"),
            ("all", "Every match"),
        ],
        // The Olympics award medals rather than lifting a trophy, and the medal
        // games are exactly what "big games" means for them. This replaces a
        // "Postseason" checkbox that could never match: ESPN files every
        // Olympic game, gold medal game included, as regular season.
        "olyhm" | "olyhw" | "olybm" | "olybw" => &[
            ("gold", "Gold medal game"),
            ("bronze", "Bronze medal game"),
            ("semi", "Semifinals"),
            ("quarter", "Quarterfinals"),
            ("all", "Every game"),
        ],
        "olysm" | "olysw" => &[
            ("gold", "Gold medal match"),
            ("bronze", "Bronze medal match"),
            ("semi", "Semifinals"),
            ("quarter", "Quarterfinals"),
            ("all", "Every match"),
        ],
        // Regionals, super regionals, the College World Series and its
        // championship series are four different ESPN season types; "playoffs"
        // is the road to Omaha and "cws" is Omaha itself.
        "cbase" => &[
            ("cws", "College World Series"),
            ("playoffs", "Regionals & supers"),
            ("all", "Every game"),
        ],
        "pga" | "lpga" => &[("major", "Majors"), ("all", "All events")],
        // F1 subscribes to session kinds, which never nested in the first place.
        "f1" => &[
            ("race", "Grands Prix"),
            ("quali", "Qualifying"),
            ("sprint", "Sprints"),
            ("sprint-quali", "Sprint qualifying"),
        ],
        // The Premier League has no postseason at all: eng.1 has exactly one
        // season type, so nothing here can ever be tagged and the blanket is
        // the only honest subscription. A lone option renders as a toggle.
        "epl" => &[("all", "Every match")],
        // A fork's league, about which we know only that the generic
        // postseason gate is the one tier the classifier could tag.
        _ => &[("playoffs", "Postseason"), ("all", "Every game")],
    };
    opts.iter().map(|(id, label)| ChampionshipOption { id: (*id).into(), label: (*label).into() }).collect()
}

pub(crate) fn championship_options_for(sport_id: &str) -> Vec<ChampionshipOption> {
    let opts: &[(&str, &str)] = match sport_id {
        "nhl" => &[
            ("none", "Only selected teams"),
            ("final", "Selected + Stanley Cup"),
            // "conf" covers the conference finals AND the cup beyond them.
            ("conf", "Selected + Conf Finals"),
            ("playoffs", "Selected + playoffs"),
            ("all", "Every match"),
        ],
        "nba" => &[
            ("none", "Only selected teams"),
            ("final", "Selected + NBA Finals"),
            ("playoffs", "Selected + playoffs"),
            ("all", "Every match"),
        ],
        "wnba" => &[
            ("none", "Only selected teams"),
            ("final", "Selected + WNBA Finals"),
            ("playoffs", "Selected + playoffs"),
            ("all", "Every match"),
        ],
        "mlb" => &[
            ("none", "Only selected teams"),
            ("ws", "Selected + World Series"),
            ("playoffs", "Selected + playoffs"),
            ("all", "Every match"),
        ],
        "nfl" => &[
            ("none", "Only selected teams"),
            ("sb", "Selected + Super Bowl"),
            ("playoffs", "Selected + playoffs"),
            ("all", "Every match"),
        ],
        "cfb" => &[
            ("none", "Only selected teams"),
            ("playoff", "Selected + CFP & bowls"),
            ("bowl", "Selected + bowl games"),
            ("all", "Every match"),
        ],
        "cbb" | "wcbb" => &[
            ("none", "Only selected teams"),
            ("tournament", "Selected + March Madness"),
            ("all", "Every match"),
        ],
        "mls" => &[
            ("none", "Only selected teams"),
            ("cup", "Selected + MLS Cup"),
            ("playoffs", "Selected + playoffs"),
            ("all", "Every match"),
        ],
        // EPL has no playoffs, so the only league-wide subscription is all of it.
        "epl" => &[("none", "Only selected teams"), ("all", "Every match")],
        // The seasonal tournaments' real subscriptions are medal games and
        // knockout rounds, which this single-select ladder cannot express — an
        // app old enough to be reading it predates those ids entirely. Offer it
        // the two rungs it can honour rather than a middle tier that would
        // expand to something it never showed the user.
        "olyhm" | "olyhw" | "olybm" | "olybw" | "olysm" | "olysw" | "wwc" | "ucl" => {
            &[("none", "Only selected teams"), ("all", "Every match")]
        }
        "cbase" => &[
            ("none", "Only selected teams"),
            ("cws", "Selected + CWS"),
            ("playoffs", "Selected + postseason"),
            ("all", "Every match"),
        ],
        // The league IS the World Cup, so "the Final" needs no further naming;
        // the tiers nest downward from it (semis pull the final along, etc.).
        "wc" => &[
            ("none", "Only selected teams"),
            ("final", "Selected + the Final"),
            ("semi", "Selected + semis & Final"),
            ("knockout", "Selected + knockout stage"),
            ("all", "Every match"),
        ],
        // Leaderboard sports subscribe by EVENT KIND, not by team — there are no
        // "selected teams" to add to, so "None" is the honest bottom rung.
        "pga" | "lpga" => &[("none", "None"), ("all", "All events"), ("majors", "Majors only")],
        "f1" => &[
            ("none", "None"),
            ("gp", "Grands Prix"),
            ("gp-quali", "GP + Qualifying"),
            ("gp-quali-sprint", "GP + Qualifying + Sprints"),
        ],
        _ => &[
            ("none", "Only selected teams"),
            ("playoffs", "Selected + playoffs"),
            ("all", "Every match"),
        ],
    };
    opts.iter().map(|(id, label)| champ(id, label)).collect()
}

/// The leaderboard "leagues" (PGA / LPGA / F1) have no teams — they're
/// subscribed via their event-kind options above.
pub(crate) fn leaderboard_sports() -> Vec<SportDescriptor> {
    // Name and family come from LEAGUE_IDENTITY rather than being repeated
    // here, so these three can't drift from the leagues fetched off ESPN.
    // `with_binary_owned` stamps them again anyway; this just keeps a catalog
    // meaningful before it reaches that step.
    let sport = |id: &str| {
        let (name, category, rank) = espn::league_identity(id).unwrap_or((id, "", 0));
        SportDescriptor {
                data_sources: Vec::new(),
            id: id.into(),
            name: name.into(),
            teams: vec![],
            championship_options: Vec::new(),
            championship_rounds: Vec::new(),
            category: category.into(),
            category_rank: rank,
            extras_label: String::new(),
        }
    };
    vec![sport("pga"), sport("lpga"), sport("f1")]
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
/// Stamp everything THIS BINARY owns onto a catalog, whatever its provenance.
///
/// Off modes, the legacy championship ladder, and the round list are all
/// properties of the code, not of the cached team data. They used to be set at
/// construction, which meant a server whose ESPN fetch failed fell back to the
/// disk cache and served whatever those lists looked like whenever it was
/// written — for off modes that shipped as an empty list and hid every mode.
/// One owner, applied after whichever source produced the catalog.
fn with_binary_owned(mut c: Catalog) -> Catalog {
    c.off_modes = offmode::off_modes();
    for s in c.sports.iter_mut() {
        s.championship_options = championship_options_for(&s.id);
        s.championship_rounds = championship_rounds_for(&s.id);
        // Display name and sport family belong here for the same reason the
        // lists do: a cached catalog written before a rename would otherwise
        // keep serving the old names, and an empty family would leave the app
        // with nothing to group by. A league this binary doesn't know keeps
        // whatever the catalog carried.
        if let Some((name, category, rank)) = espn::league_identity(&s.id) {
            s.name = name.into();
            s.category = category.into();
            s.category_rank = rank;
            s.extras_label = extras_label_for(&s.id).into();
        }
        // Stamped here, not in the fetcher, for the cached-catalog reason
        // above — and recomputed per boot, because the offer depends on this
        // process's ENVIRONMENT: a server without the OpenF1 key must not
        // keep advertising a source it can no longer serve.
        s.data_sources = data_sources_for(&s.id, openf1::OpenF1::from_env().is_some());
    }
    c
}

/// The feeds this server can offer a league. Only f1 has an alternate, and
/// only when the process carries OpenF1 credentials — the key is personal-use
/// and lives in the environment, so a server without it never offers the
/// choice and no app ever shows the switch. FIRST entry is the default.
fn data_sources_for(
    sport_id: &str,
    openf1_available: bool,
) -> Vec<scoreboard_protocol::DataSourceOption> {
    if sport_id == "f1" && openf1_available {
        vec![
            scoreboard_protocol::DataSourceOption { id: "espn".into(), label: "ESPN".into() },
            scoreboard_protocol::DataSourceOption { id: "openf1".into(), label: "OpenF1".into() },
        ]
    } else {
        Vec::new()
    }
}

/// What a league's box-score extras are called, or "" when it draws none.
///
/// Keyed off the family rather than the league id so a new competition inherits
/// the right switch: every soccer league draws shootouts and aggregates, every
/// hockey league draws shots. The strings name what disappears, because "Extra
/// details" would leave the user to discover which.
fn extras_label_for(sport_id: &str) -> &'static str {
    match sport_id {
        "mlb" | "cbase" => "Hits and errors",
        "nhl" | "olyhm" | "olyhw" => "Shots on goal",
        _ if espn::is_soccer(sport_id) => "Shootout and aggregate",
        // Football, basketball, golf and F1 draw no extras today. An empty
        // label is how the app knows not to offer a switch that would do
        // nothing — see `SportDescriptor::extras_label`.
        _ => "",
    }
}

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
                data_sources: Vec::new(),
        id: id.into(),
        name: name.into(),
        teams,
        // Empty for the same reason as the lists: `with_binary_owned` is the
        // single owner, and filling it here would hide a missing assembly step
        // in the tests written to catch one.
        championship_options: Vec::new(),
        championship_rounds: Vec::new(),
        category: String::new(),
        category_rank: 0,
        extras_label: String::new(),
    };
    Catalog {
        // Empty, like every other construction site — `with_off_modes` is the
        // single owner. Filling it here would hide a missing assembly step in
        // exactly the tests meant to catch one.
        off_modes: Vec::new(),
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
        hits: None,
        errors: None,
        shots: None,
        shootout: None, aggregate: None,
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
        series: None,
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
                down_string: "3rd & 4".into(),
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
            leaderboard: Some(model::Leaderboard { matchplay: None, laps: None, flag: None, stage: None, quali: None, name: "MASTERS".into(), rows: vec![
                model::LeaderRow { name: "SCHEFFLER".into(), value: "-8".into(), ..Default::default() },
                model::LeaderRow { name: "MCILROY".into(), value: "-6".into(), ..Default::default() },
                model::LeaderRow { name: "MORIKAWA".into(), value: "-5".into(), ..Default::default() },
                model::LeaderRow { name: "SCHAUFFELE".into(), value: "-3".into(), ..Default::default() },
                model::LeaderRow { name: "FLEETWOOD".into(), value: "-2".into(), ..Default::default() },
            ] }),
            champ: None,
            series: None,
            session: None,
            situation: None,
            pregame: None,
        }),
        ("f1", Game {
            id: "f".into(), sport_id: "f1".into(),
            away: model::TeamSide::empty("F1"), home: model::TeamSide::empty(""),
            status: GameStatus::Active, ordinal: String::new(), clock: String::new(), start_time: String::new(), end_time: None,
            leaderboard: Some(model::Leaderboard { matchplay: None, laps: Some(model::LapProgress { completed: 42, total: 78 }), flag: None, stage: None, quali: None, name: "MONACO GP".into(), rows: vec![
                model::LeaderRow { rank: "1".into(), name: "VER".into(), value: "LAP 42".into(), color: Some(model::hex_to_rgb("3671C6")), tire: Some('M'), thru: None },
                model::LeaderRow { rank: "2".into(), name: "NOR".into(), value: "+2.1".into(), color: Some(model::hex_to_rgb("FF8000")), tire: Some('M'), thru: None },
                model::LeaderRow { rank: "3".into(), name: "LEC".into(), value: "+5.8".into(), color: Some(model::hex_to_rgb("E8002D")), tire: Some('S'), thru: None },
                model::LeaderRow { rank: "4".into(), name: "PIA".into(), value: "+9.0".into(), color: Some(model::hex_to_rgb("FF8000")), tire: Some('H'), thru: None },
                model::LeaderRow { rank: "20".into(), name: "HAM".into(), value: "+12".into(), color: Some(model::hex_to_rgb("E8002D")), tire: Some('S'), thru: None },
            ] }),
            champ: None,
            series: None,
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
        configure_all(&state, "b1");
        handle_button(&state, "b1");
        let cfg = state.config_for("b1");
        assert!(!cfg.power, "press while showing games should sleep");
        let wake = cfg.wake_at.expect("wake timer set");
        let expect = unix_now() + 24 * 3600;
        assert!((wake as i64 - expect as i64).abs() <= 5, "default sleep is 24h");
        let m = manifest_for_board(&state, "b1");
        // The splash is a pre-baked bar sweep: one step per second of notice.
        assert!(m.display_on && m.rotation.len() == 1, "sleep splash notice shows");
        assert_eq!(m.countdown_ms, Some(SPLASH_SECS as u32 * 1000), "the bar is the board's to fill");

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

    /// Deep leaderboards get a scrolling manifest entry held for the board's
    /// speed scaled by the frame's height (64 rows twice as long, 128 four
    /// times: the board fits one full pass into it); team games stay static
    /// at the speed itself.
    #[test]
    fn manifest_scrolls_deep_leaderboards() {
        let state = AppState::new_ephemeral();
        configure_all(&state, "b1");
        *state.games.lock().unwrap() = espn::demo_games(0, 0);
        let m = manifest_for_board(&state, "b1");
        let dwell = scoreboard_protocol::snap_dwell_secs(state.config_for("b1").dwell_secs) * 1000;
        let scrolled: Vec<&RotationEntry> = m.rotation.iter().filter(|e| e.scroll).collect();
        assert!(!scrolled.is_empty(), "demo golf/F1 should scroll");
        for e in &scrolled {
            let px = state.frames.lock().unwrap().get(&e.frame_id).map(|(_, p)| p.clone()).unwrap();
            assert!(px.len() > FRAME_BYTES, "scroll entries carry tall frames");
            let rows = (px.len() / (scoreboard_protocol::FRAME_WIDTH * 2)) as u32;
            assert_eq!(e.dwell_ms, dwell * rows / 32, "a scrolling screen holds in proportion to its height");
        }
        for e in m.rotation.iter().filter(|e| !e.scroll) {
            let px = state.frames.lock().unwrap().get(&e.frame_id).map(|(_, p)| p.clone()).unwrap();
            assert_eq!(px.len(), FRAME_BYTES, "static entries stay panel-sized");
            assert_eq!(e.dwell_ms, dwell, "a static screen holds for the speed itself");
        }
    }

    /// END-TO-END scroll: the demo manifest, serialized exactly as the wire
    /// carries it, drives a real Board through a full dwell — the leaderboard
    /// window must actually MOVE. Guards the server/board contract that unit
    /// tests on either side can miss.
    #[test]
    fn board_scrolls_the_demo_leaderboard_through_the_wire() {
        let state = AppState::new_ephemeral();
        configure_all(&state, "b1");
        *state.games.lock().unwrap() = espn::demo_games(0, 0);
        push_board(&state, "b1");
        let json = state.last_manifest.lock().unwrap().get("b1").cloned().unwrap();
        // The wire round-trip the device performs.
        let ServerMsg::Manifest(m) = serde_json::from_str::<ServerMsg>(&json).unwrap() else {
            panic!("stored manifest should parse")
        };
        let scroll_idx = m.rotation.iter().position(|e| e.scroll).expect("a scrolling entry");
        let dwell = m.rotation[scroll_idx].dwell_ms as u64;
        // Every scrolling entry pins the leaderboard's title rows, and only
        // scrolling entries do (the field is omitted otherwise, so static
        // manifests stay byte-identical for older firmware).
        for e in &m.rotation {
            assert_eq!(e.pin_rows, if e.scroll { render::LEADERBOARD_PIN_ROWS } else { 0 }, "{}", e.frame_id);
        }
        assert!(json.contains("\"pin_rows\":8"), "the pin travels on the wire");

        let mut b = board_core::Board::new();
        b.gesture(board_core::Gesture::Long); // -> Setup (any non-boot mode)
        b.setup_done(); // -> Running? Setup stays; use manifest path instead
        b.set_online(true);
        b.tick(0);
        b.on_manifest(m.clone());
        // Feed every frame the manifest references, exactly as NeedFrames
        // would: the payload the server builds for the codecs a real board
        // advertises, not the raw cache entry. Compression rides this path too
        // — a tall leaderboard has to survive encode, wire, decode, and still
        // scroll.
        {
            let frames = state.frames.lock().unwrap();
            for e in &m.rotation {
                let (_, px) = frames.get(&e.frame_id).cloned().unwrap();
                let payload = encode_frame_payload(&px, board_core::SUPPORTED_FRAME_CODECS);
                // MOST frames compress, but a dither-heavy frame can RLE-lose
                // and legitimately fall back to raw-plus-header — the board
                // decodes that fine (it is the legacy path). The demo slate is
                // minute-driven, so which frames exist here depends on the
                // wall clock; asserting a ratio made this test fail only at
                // certain times of day, which is the worst kind of red. The
                // invariant is that the payload never BALLOONS.
                assert!(
                    payload.len() <= px.len() + 8,
                    "frame payload ballooned: {} raw={} payload={}",
                    e.frame_id, px.len(), payload.len()
                );
                b.on_frame(e.frame_id.clone(), payload);
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

    /// What the PANEL shows through one dwell of a scrolling leaderboard,
    /// drawn by the real board code from the real server frame and manifest
    /// entry: the pinned title holds while the field scrolls beneath it.
    /// Writes `<out>/<name>/<nnn>.png` snapshots every 250ms of a 10s dwell.
    ///
    ///   SCROLL_OUT=/tmp/scroll F1_SNAPSHOT=<capture snapshot dir> GOLF_JSON=<lpga leaderboard json> \
    ///     cargo test -p server dump_pinned_scroll -- --ignored
    #[test]
    #[ignore = "writes scroll previews; run with --ignored and SCROLL_OUT"]
    fn dump_pinned_scroll() {
        let out = std::path::PathBuf::from(std::env::var("SCROLL_OUT").expect("SCROLL_OUT"));
        let mut games: Vec<(&str, Game)> = Vec::new();
        if let Ok(dir) = std::env::var("F1_SNAPSHOT") {
            let g = espn::replay_f1_snapshot(&std::path::Path::new(&dir).join("espn"))
                .into_iter()
                .find(|g| g.session.as_deref() == Some("race"))
                .expect("a race game in the snapshot");
            games.push(("f1", g));
        }
        if let Ok(f) = std::env::var("GOLF_JSON") {
            let raw = std::fs::read_to_string(f).unwrap();
            games.push(("golf", espn::parse_feed("lpga", &raw).unwrap().remove(0)));
        }
        let cup = std::fs::read_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../design/golf-logs"))
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.file_name().is_some_and(|n| n == "espn-pga-leaderboard-presidents-cup-20260926T124348Z.json"))
            .unwrap();
        games.push(("matchplay", espn::parse_feed("pga", &std::fs::read_to_string(cup).unwrap()).unwrap().remove(0)));

        for (name, g) in games {
            let (id, px, scrolls) = render::render_board_frame(
                &g, render::LEADERBOARD_MAX_ROWS, None, Some(chrono_tz::America::Chicago), NameStyle::Name,
            );
            assert!(scrolls, "{name} should scroll");
            let m = Manifest {
                rotation: vec![RotationEntry { frame_id: id.clone(), dwell_ms: 10_000, scroll: true, pin_rows: render::LEADERBOARD_PIN_ROWS }],
                display_on: true,
                local_clock: false,
                clock_hour24: false,
                off_state: Default::default(),
                brightness: 100,
                epoch: 0,
                countdown_ms: None,
                mode: String::new(),
                modes: Vec::new(),
            };
            let mut b = board_core::Board::new();
            b.gesture(board_core::Gesture::Long);
            b.setup_done();
            b.set_online(true);
            b.tick(0);
            b.on_manifest(m);
            b.on_frame(id, encode_frame_payload(&px, board_core::SUPPORTED_FRAME_CODECS));
            let dir = out.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            for (n, t) in (0..=10_000u64).step_by(250).enumerate() {
                b.tick(t);
                let fb = b.framebuffer();
                let mut img = image::RgbImage::new(64 * 8, 32 * 8);
                for (i, p) in fb.iter().enumerate() {
                    let (x, y) = ((i % 64) as u32, (i / 64) as u32);
                    let rgb = [(p >> 16 & 0xFF) as u8, (p >> 8 & 0xFF) as u8, (p & 0xFF) as u8];
                    if rgb == [0, 0, 0] {
                        continue;
                    }
                    for dy in 1..7 {
                        for dx in 1..7 {
                            img.put_pixel(x * 8 + dx, y * 8 + dy, image::Rgb(rgb));
                        }
                    }
                }
                img.save(dir.join(format!("{n:03}.png"))).unwrap();
            }
        }
    }

    /// A now-showing leaderboard's summary tells the app how the panel
    /// scrolls it — the doubled dwell and the pinned title rows — and a team
    /// game's says nothing; `/render?tall=1` hands the app the tall frame.
    #[test]
    fn summaries_carry_the_boards_scroll_timing() {
        let state = AppState::new_ephemeral();
        configure_all(&state, "b1");
        *state.games.lock().unwrap() = espn::demo_games(0, 0);
        let cfg = state.config_for("b1");
        let games = state.games.lock().unwrap().clone();
        let deep = games.iter().find(|g| game_scroll_capable(&cfg, g)).expect("a deep demo leaderboard");
        let mut s = to_summary(deep, None);
        mark_scroll(&mut s, &cfg, deep, None);
        assert!(s.scrolls);
        // The dwell is the one the rotation uses: the speed scaled by the
        // board frame's height.
        let (_, px, _) = render::render_board_frame(deep, league_rows_cap(&cfg, &deep.sport_id), None, None, name_style_for(&cfg, &deep.sport_id));
        let rows = px.len() / (scoreboard_protocol::FRAME_WIDTH * 2);
        assert!(rows > 32);
        assert_eq!(s.scroll_dwell_ms, scoreboard_protocol::screen_dwell_ms(cfg.dwell_secs, rows));
        assert_eq!(s.pin_rows, render::LEADERBOARD_PIN_ROWS);
        // Scrolling turned off (depth 4): the toggle still shows, no glide.
        let mut off = cfg.clone();
        off.sports.retain(|sp| sp.sport_id != deep.sport_id);
        off.sports.push(SportConfig { sport_id: deep.sport_id.clone(), leaderboard_rows: Some(4), ..Default::default() });
        let mut o = to_summary(deep, None);
        mark_scroll(&mut o, &off, deep, None);
        assert!(o.scrolls, "the toggle offers to turn it back on");
        assert_eq!((o.scroll_dwell_ms, o.pin_rows), (0, 0), "a static frame does not glide");
        let team = games.iter().find(|g| g.leaderboard.is_none()).unwrap();
        let mut t = to_summary(team, None);
        mark_scroll(&mut t, &cfg, team, None);
        assert!(!t.scrolls);
        assert_eq!((t.scroll_dwell_ms, t.pin_rows), (0, 0), "nothing to glide");
        // The wire omits both for a static game.
        let json = serde_json::to_string(&t).unwrap();
        assert!(!json.contains("scroll_dwell_ms") && !json.contains("pin_rows"));
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
        configure_all(&state, "b1");
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
        // The board default, as a static screen holds it; a scrolling one
        // holds it twice as long.
        let rows_of = |e: &RotationEntry| {
            let frames = state.frames.lock().unwrap();
            frames.get(&e.frame_id).map(|(_, p)| p.len() / (scoreboard_protocol::FRAME_WIDTH * 2)).unwrap()
        };
        let default_for = |e: &RotationEntry| scoreboard_protocol::screen_dwell_ms(cfg.dwell_secs, rows_of(e));
        let mut saw_f1 = false;
        for (entry, g) in m.rotation.iter().zip(&chosen) {
            if g.sport_id == "f1" {
                // 30s is not a preset: it snaps to Slow (20s), scaled by the
                // tall frame's height for the scroll.
                assert_eq!(entry.dwell_ms, 20_000 * rows_of(entry) as u32 / 32, "override applies to F1, snapped and scaled");
                assert!(entry.scroll, "demo F1 is deep enough to scroll");
                saw_f1 = true;
            } else {
                assert_eq!(entry.dwell_ms, default_for(entry), "everything else keeps the default");
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
        assert!(m.rotation.iter().all(|e| e.dwell_ms == default_for(e)), "cleared override");
    }

    /// Leaderboard depth: 4 = the static panel layout (no scrolling); 20 = the
    /// full field as one tall frame; unset = the default 10.
    #[test]
    fn league_rows_control_leaderboard_depth() {
        let state = AppState::new_ephemeral();
        configure_all(&state, "b1");
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
        configure_all(&state, "b1");
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

    /// The board trusts `Manifest.modes` to know what it may offer; the
    /// server validates `SelectMode` against the board's own list. These are
    /// two separate code paths that must agree, or the picker shows a row the
    /// server then rejects — on an unsaved board AND on one whose list holds
    /// two screens of one kind.
    #[test]
    fn the_picker_offers_what_select_mode_accepts() {
        let state = AppState::new_ephemeral();
        let mut edited = BoardConfig::default();
        let list = vec![
            ScreenConfig { id: "weather".into(), kind: "weather".into(), params: Default::default(), idle: true },
            ScreenConfig {
                id: "weather.2".into(),
                kind: "weather".into(),
                params: [("variant".to_string(), "forecast".to_string())].into(),
                idle: false,
            },
            ScreenConfig { id: "clock".into(), kind: "clock".into(), params: Default::default(), idle: false },
        ];
        let _ = edited.set_screens(list, &offmode::off_modes());
        state.configs.lock().unwrap().insert("b2".into(), edited);
        for board in ["b1", "b2"] {
            let modes = pickable_modes(&state.config_for(board));
            for entry in &modes {
                handle_select_mode(&state, board, &entry.id);
                assert_eq!(state.config_for(board).held_screen_id(), entry.id, "{board}: {} must be selectable", entry.id);
                assert_eq!(manifest_for_board(&state, board).mode, entry.id, "{board}: the knob's tick lands on the pick");
            }
            // Sports first, Off second (MAR-117 §0 Q2), and no dark idle
            // screen masquerading as a second Off.
            assert_eq!(modes[0], ModeEntry { id: MODE_SCORES.into(), label: "SPORTS".into() });
            assert_eq!(modes[1], ModeEntry { id: MODE_OFF.into(), label: "OFF".into() });
            assert!(!modes.iter().any(|m| m.id == "blank"), "Nothing is not a mode");
        }
        let ids: Vec<String> = pickable_modes(&state.config_for("b2")).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["scores", "off", "weather", "weather.2", "clock"], "the board's own order");
    }

    /// The picker data rides EVERY manifest — most importantly the dark ones,
    /// because a board showing "off" is exactly the board whose picker
    /// matters.
    #[test]
    fn every_manifest_carries_the_picker() {
        let state = AppState::new_ephemeral();
        handle_select_mode(&state, "b1", MODE_OFF);
        // Clear the (correct) SLEEPING splash to reach the resting dark state.
        state.notices.lock().unwrap().remove("b1");
        let dark = manifest_for_board(&state, "b1");
        assert!(!dark.display_on, "precondition: this is the dark manifest");
        assert_eq!(dark.mode, MODE_OFF);
        assert!(!dark.modes.is_empty(), "the dark manifest still offers the picker");

        handle_select_mode(&state, "b1", "clock");
        let lit = manifest_for_board(&state, "b1");
        assert_eq!(lit.mode, "clock");
        assert_eq!(lit.modes, dark.modes, "one list, whatever is showing");
        assert_eq!(lit.modes, pickable_modes(&state.config_for("b1")), "and it is the board's own list");
        let labels: Vec<&str> = lit.modes.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, ["SPORTS", "OFF", "CLOCK", "WEATHER"]);
    }

    /// The device path validates like the app path: junk is ignored.
    #[test]
    fn select_mode_ignores_junk() {
        let state = AppState::new_ephemeral();
        handle_select_mode(&state, "b1", "clock");
        handle_select_mode(&state, "b1", "stocks");
        assert_eq!(state.config_for("b1").mode, "clock", "junk pick ignored");
    }

    /// Arriving in scores mode with nothing to show announces the idle
    /// handoff — "NO GAMES / WEATHER IN / 2 MINUTES" — instead of flipping
    /// straight to the idle screen, which read as being in the wrong mode.
    #[test]
    fn picking_scores_on_a_gameless_evening_announces_the_idle_handoff() {
        let state = AppState::new_ephemeral();
        state.games.lock().unwrap().clear();
        let mut cfg = watch_all();
        cfg.set_off_mode("weather", false);
        apply_mode(&mut cfg, MODE_SCORES);
        state.configs.lock().unwrap().insert("b1".into(), cfg.clone());
        handle_mode_change(&state, "b1", &cfg);
        let lines: Vec<String> = state.notices.lock().unwrap().get("b1").unwrap()
            .lines.iter().map(|(t, ..)| t.clone()).collect();
        assert_eq!(lines, ["NO GAMES", "WEATHER IN", "2 MINUTES"], "the handoff names the idle screen");
        let m = manifest_for_board(&state, "b1");
        assert!(m.display_on && m.countdown_ms == Some(IDLE_NOTICE_SECS as u32 * 1000));

        // A blank idle screen says what actually happens: the panel goes off.
        cfg.set_off_mode("blank", true);
        state.configs.lock().unwrap().insert("b1".into(), cfg.clone());
        handle_mode_change(&state, "b1", &cfg);
        let lines: Vec<String> = state.notices.lock().unwrap().get("b1").unwrap()
            .lines.iter().map(|(t, ..)| t.clone()).collect();
        assert_eq!(lines, ["NO GAMES", "SCREEN OFF IN", "2 MINUTES"]);

        // With games on the slate there is no handoff to announce.
        *state.games.lock().unwrap() = espn::mock_games();
        handle_mode_change(&state, "b1", &cfg);
        assert!(state.notices.lock().unwrap().get("b1").is_none(), "games mean no announcement");

        // And the announcement's expiry reveals the idle screen it named.
        state.games.lock().unwrap().clear();
        cfg.set_off_mode("weather", false);
        state.configs.lock().unwrap().insert("b1".into(), cfg.clone());
        handle_mode_change(&state, "b1", &cfg);
        let epoch = state.notices.lock().unwrap().get("b1").unwrap().epoch;
        finish_notice(&state, "b1", epoch);
        let m = manifest_for_board(&state, "b1");
        assert!(m.display_on, "the idle screen follows");
        assert!(m.countdown_ms.is_none(), "with no bar left over");
    }

    /// A code on a connected board's panel must stay redeemable. It is
    /// registered once at connect, but the panel shows it on demand days
    /// later — expiring from the connect instant made the panel and the
    /// server disagree about the code the user was reading.
    #[test]
    fn a_connected_boards_code_does_not_expire_under_it() {
        let state = AppState::new_ephemeral();
        state.register_pairing_code("424242".into(), "b1");
        // Age the registration past the TTL, as a board up for an hour does.
        {
            let mut pending = state.pending.lock().unwrap();
            let e = pending.get_mut("424242").unwrap();
            e.1 = std::time::Instant::now() - (PAIR_CODE_TTL + Duration::from_secs(60));
        }
        // Disconnected: it is genuinely stale and must not be redeemable.
        assert!(try_pair(&state, "424242", "ctrl").is_err(), "a gone board's code expires");

        // Connected: the sweep keeps it live, and it redeems.
        state.register_pairing_code("515151".into(), "b2");
        {
            let mut pending = state.pending.lock().unwrap();
            let e = pending.get_mut("515151").unwrap();
            e.1 = std::time::Instant::now() - (PAIR_CODE_TTL + Duration::from_secs(60));
        }
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        state.devices.lock().unwrap().insert("b2".into(), tx);
        refresh_pairing_codes(&state);
        assert_eq!(try_pair(&state, "515151", "ctrl").as_deref(), Ok("b2"));
    }

    /// A board reconnecting into a freshly restarted server must not be told
    /// "idle" out of boot emptiness — the source of a weather blink on every
    /// deploy with games live. Manifests HOLD until the first sweep lands;
    /// slate-independent states still push immediately.
    #[test]
    fn a_restarting_server_holds_manifests_until_the_first_sweep() {
        use std::sync::atomic::Ordering;
        let state = AppState::new_ephemeral();
        state.slate_ready.store(false, Ordering::Relaxed);
        state.games.lock().unwrap().clear();

        state.configs.lock().unwrap().insert("b1".into(), watch_all());
        push_board(&state, "b1");
        assert!(
            state.last_manifest.lock().unwrap().get("b1").is_none(),
            "a scores board gets NOTHING before the first sweep — never a false idle"
        );

        // Slate-independent state: a board in mode "off" is dark whatever the
        // slate says, so it pushes at boot.
        let mut off = watch_all();
        apply_mode(&mut off, MODE_OFF);
        state.configs.lock().unwrap().insert("b2".into(), off);
        push_board(&state, "b2");
        assert!(
            state.last_manifest.lock().unwrap().get("b2").is_some(),
            "slate-independent states push through the boot hold"
        );

        // The first sweep write releases the hold — even an EMPTY one, so a
        // genuinely gameless day still idles honestly.
        state.slate_ready.store(true, Ordering::Relaxed);
        push_board(&state, "b1");
        assert!(
            state.last_manifest.lock().unwrap().get("b1").is_some(),
            "after the sweep, the idle claim is honest"
        );
    }

    /// A live debug session must survive a DEPLOY: the board's re-arm on
    /// reconnect only works if the restarted server still knows the session,
    /// and the app's link only works if the same nonce still resolves.
    #[test]
    fn a_debug_session_survives_a_restart() {
        let path = std::env::temp_dir().join(format!("sb-dbg-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let url = {
            let state = AppState::new_inner(Some(open_db(&path).unwrap()), mock_catalog());
            mint_debug_token(&state, "b1", 600)
        };
        {
            let state = AppState::new_inner(Some(open_db(&path).unwrap()), mock_catalog());
            let left = debug_session_remaining(&state, "b1").expect("the session survives the deploy");
            assert!(left > 500 && left <= 600, "with its clock still roughly right, got {left}");
            let nonce = url.trim_start_matches("/debug/");
            assert_eq!(
                resolve_debug_token(&state, nonce).as_deref(),
                Some("b1"),
                "the app's existing link keeps resolving"
            );
            // Revocation is durable too.
            state.debug_tokens.lock().unwrap().clear();
            persist_debug_sessions(&state);
        }
        {
            let state = AppState::new_inner(Some(open_db(&path).unwrap()), mock_catalog());
            assert!(
                debug_session_remaining(&state, "b1").is_none(),
                "a revoked session stays revoked across the next deploy"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    /// A poll that omits possession must not flip the football frame back to
    /// the text layout: the last known possession bridges the gap, the feed's
    /// own statement always wins, and a game with no history stays honest.
    #[test]
    fn a_possession_less_poll_keeps_the_field_bar() {
        let state = AppState::new_ephemeral();
        let football = |id: &str, hp: Option<bool>| {
            let mut g = sample_game_for("nfl", false, true);
            g.id = id.into();
            if let Some(model::Situation::Football { home_possession, .. }) = &mut g.situation {
                *home_possession = hp;
            }
            g
        };
        *state.games.lock().unwrap() = vec![football("g1", Some(true)), football("g2", Some(false))];

        // Next poll: g1 lost its possession bit, g2 states a new one, g3 is
        // new and unknown.
        let mut next = vec![football("g1", None), football("g2", Some(true)), football("g3", None)];
        bridge_football_possession(&state, &mut next);
        let hp = |g: &Game| match &g.situation {
            Some(model::Situation::Football { home_possession, .. }) => *home_possession,
            _ => panic!("football expected"),
        };
        assert_eq!(hp(&next[0]), Some(true), "omission bridged from the last poll");
        assert_eq!(hp(&next[1]), Some(true), "a stated possession always wins");
        assert_eq!(hp(&next[2]), None, "no history stays honestly unknown");
    }

    /// The legacy button is a clean toggle now: any lit mode -> "off" (with
    /// off's timer), "off" -> "scores". No hidden third state, games or not.
    #[test]
    fn legacy_button_toggles_between_scores_and_off() {
        let state = AppState::new_ephemeral();
        state.games.lock().unwrap().clear();
        handle_button(&state, "b1");
        let cfg = state.config_for("b1");
        assert_eq!(cfg.mode, MODE_OFF, "press from scores means off, games or not");
        assert!(!cfg.power, "the legacy twin agrees");
        assert!(cfg.wake_at.is_some(), "off arms its configured timer");
        assert!(state.notices.lock().unwrap().contains_key("b1"), "and says so");

        handle_button(&state, "b1");
        let cfg = state.config_for("b1");
        assert_eq!(cfg.mode, MODE_SCORES, "press from off means scores");
        assert!(cfg.power);
        assert!(cfg.wake_at.is_none(), "scores runs on no timer");
        // This board is GAMELESS, so waking replaces the splash with the idle
        // handoff announcement rather than silence.
        let lines: Vec<String> = state.notices.lock().unwrap().get("b1").unwrap()
            .lines.iter().map(|(t, ..)| t.clone()).collect();
        assert_eq!(lines[0], "NO GAMES", "waking into nothing announces the handoff");
    }

    /// A press from a PICKED mode (sticky clock) also goes to off — the legacy
    /// button's meaning was always "make it dark", not "cycle".
    #[test]
    fn legacy_button_from_a_picked_mode_goes_dark() {
        let state = AppState::new_ephemeral();
        let mut cfg = BoardConfig::default();
        apply_mode(&mut cfg, "clock");
        state.configs.lock().unwrap().insert("b1".into(), cfg);
        handle_button(&state, "b1");
        assert_eq!(state.config_for("b1").mode, MODE_OFF);
    }

    /// Notice expiry retires the notice and nothing else — a splash is an
    /// announcement, never a consequence. The mode it announced was applied
    /// before it was shown.
    #[test]
    fn notice_expiry_changes_no_config() {
        let state = AppState::new_ephemeral();
        state.games.lock().unwrap().clear();
        handle_button(&state, "b1"); // -> off, with the SLEEPING splash up
        let before = state.config_for("b1");
        let epoch = state.notices.lock().unwrap().get("b1").unwrap().epoch;
        finish_notice(&state, "b1", epoch);
        assert!(!state.notices.lock().unwrap().contains_key("b1"), "notice retired");
        let after = state.config_for("b1");
        assert_eq!(after.mode, before.mode, "expiry must not touch the mode");
        assert_eq!(after.wake_at, before.wake_at);

        // A superseded expiry (notice already cleared/replaced) is a no-op.
        handle_button(&state, "b1"); // wake -> scores; splash cancelled
        handle_button(&state, "b1"); // off again -> fresh splash
        let epoch = state.notices.lock().unwrap().get("b1").unwrap().epoch;
        state.notices.lock().unwrap().remove("b1");
        finish_notice(&state, "b1", epoch);
        assert_eq!(state.config_for("b1").mode, MODE_OFF, "superseded expiry changes nothing");
    }

    /// The one place mode transitions happen, exercised directly.
    #[test]
    fn apply_mode_arms_the_configured_timer() {
        let mut cfg = BoardConfig::default();
        // A pick uses the mode's own configured hours...
        cfg.mode_hours.insert("clock".into(), 3);
        apply_mode(&mut cfg, "clock");
        assert_eq!(cfg.mode, "clock");
        assert!(cfg.power, "a lit mode keeps the legacy twin true");
        let until = cfg.wake_at.expect("clock runs on its timer");
        let expect = unix_now() + 3 * 3600;
        assert!(until.abs_diff(expect) <= 2, "3 hours from now");
        // ...an unconfigured mode falls back to sleep_hours...
        apply_mode(&mut cfg, MODE_OFF);
        let until = cfg.wake_at.expect("off falls back to sleep_hours");
        assert!(until.abs_diff(unix_now() + 24 * 3600) <= 2, "the 24h default");
        assert!(!cfg.power);
        // ...zero means until changed...
        cfg.mode_hours.insert("weather".into(), 0);
        apply_mode(&mut cfg, "weather");
        assert_eq!(cfg.wake_at, None, "0 = no timer");
        // ...and scores never has one.
        apply_mode(&mut cfg, MODE_SCORES);
        assert_eq!(cfg.wake_at, None);
        assert!(cfg.power);
    }

    /// Any pick during the quiet window sets the override — INCLUDING
    /// re-picking the current mode. The 2am gesture is "answer me now"; a pick
    /// that changed nothing visible would read as a dead button.
    #[test]
    fn any_mode_pick_during_quiet_hours_overrides_the_window() {
        let mut cfg = BoardConfig {
            quiet_hours: Some(window_around_now(&BoardConfig::default())),
            ..Default::default()
        };
        assert!(quiet_now(&cfg), "precondition: inside the window");
        apply_mode(&mut cfg, MODE_SCORES); // the current mode, re-picked
        assert!(cfg.quiet_override, "same-mode pick still answers");
        assert!(!quiet_now(&cfg), "and the panel lights for the rest of tonight");
    }

    /// The timer sweep returns a lapsed mode to scores — the same sweep that
    /// honours an old binary's persisted sleep, so both are covered at once.
    #[test]
    fn the_sweep_returns_a_lapsed_mode_to_scores() {
        let state = AppState::new_ephemeral();
        let mut cfg = BoardConfig::default();
        apply_mode(&mut cfg, "clock");
        cfg.wake_at = Some(unix_now() - 1); // timer already lapsed
        state.configs.lock().unwrap().insert("b1".into(), cfg);
        tick_once(&state);
        let after = state.config_for("b1");
        assert_eq!(after.mode, MODE_SCORES, "lapsed timer -> scores");
        assert!(after.power);
        assert_eq!(after.wake_at, None);
    }

    /// A row persisted by the OLD binary mid-sleep: power=false, defaulted
    /// mode, wake_at set. It must load as "off" WITH the wake time kept, so
    /// the sweep honours the "waking at 7am" the old binary promised.
    #[test]
    fn a_legacy_sleeping_row_wakes_when_promised() {
        let old = r#"{"brightness":80,"power":false,"sports":[],"wake_at":123}"#;
        let mut cfg: BoardConfig = serde_json::from_str(old).unwrap();
        migrate_legacy_row(&mut cfg);
        assert_eq!(cfg.mode, MODE_OFF, "a sleeping legacy row is an off-mode row");
        assert_eq!(cfg.wake_at, Some(123), "the promise survives the migration");
        // And a lit legacy row stays scores.
        let lit = r#"{"brightness":80,"power":true,"sports":[]}"#;
        let mut cfg: BoardConfig = serde_json::from_str(lit).unwrap();
        migrate_legacy_row(&mut cfg);
        assert_eq!(cfg.mode, MODE_SCORES);
    }

    /// A picked mode holds the panel over live games — that is what picking
    /// means — and "off" stays dark even with games on.
    #[test]
    fn a_picked_mode_outranks_games() {
        let state = AppState::new_ephemeral();
        let games = candidate_games(&state, &watch_all());
        assert!(!games.is_empty(), "precondition: the mock slate has games");
        let mut cfg = watch_all();
        apply_mode(&mut cfg, "clock");
        state.configs.lock().unwrap().insert("b1".into(), cfg.clone());
        let m = manifest_for_board(&state, "b1");
        assert!(m.display_on, "a picked clock is lit");
        // The clock is a single non-scrolling frame, not the games rotation.
        assert_eq!(m.rotation.len(), 1, "the clock, not the rotation");

        apply_mode(&mut cfg, MODE_OFF);
        state.configs.lock().unwrap().insert("b1".into(), cfg);
        let m = manifest_for_board(&state, "b1");
        assert!(!m.display_on, "off is dark even with games on");
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
                        championship_option_ids: Vec::new(),
                        dwell_secs: None,
                        leaderboard_rows: None,
                        show_round: None,
                show_thru: None,
                data_source: None,
                        show_extras: None,
                        spoiler: false,
                        spoiler_team_ids: vec![],
                        name_style: None,
                    },
                    SportConfig {
                        sport_id: "nhl".into(),
                        favorite_team_ids: vec!["nhl-bos".into()],
                        championship_option_id: None,
                        championship_option_ids: Vec::new(),
                        dwell_secs: None,
                        leaderboard_rows: None,
                        show_round: None,
                show_thru: None,
                data_source: None,
                        show_extras: None,
                        spoiler: false,
                        spoiler_team_ids: vec![],
                        name_style: None,
                    },
                    SportConfig {
                        sport_id: "golf".into(), // sport no longer in the catalog at all
                        favorite_team_ids: vec!["golf-pga".into()],
                        championship_option_id: None,
                        championship_option_ids: Vec::new(),
                        dwell_secs: None,
                        leaderboard_rows: None,
                        show_round: None,
                show_thru: None,
                data_source: None,
                        show_extras: None,
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
        let subscribed = |g: &Game, cfg: &BoardConfig| game_option_priority(g, cfg).is_some();
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
            series: None,
            session: session.map(Into::into),
            situation: None,
            pregame: None,
        };
        // A board subscribed to exactly these rounds.
        let cfg = |sport: &str, rounds: &[&str]| BoardConfig {
            sports: vec![SportConfig {
                sport_id: sport.into(),
                favorite_team_ids: vec![],
                championship_option_ids: rounds.iter().map(|r| r.to_string()).collect(),
                championship_option_id: legacy_champ_projection(
                    sport,
                    &rounds.iter().map(|r| r.to_string()).collect::<Vec<_>>(),
                ),
                dwell_secs: None,
                leaderboard_rows: None,
                show_round: None,
                show_thru: None,
                data_source: None,
                show_extras: None,
                spoiler: false,
                spoiler_team_ids: vec![],
                name_style: None,
            }],
            ..Default::default()
        };

        // Golf: "all" takes every event; "major" only the majors.
        assert!(subscribed(&mk("pga", None, None), &cfg("pga", &["all"])));
        assert!(!subscribed(&mk("pga", None, None), &cfg("pga", &["major"])));
        assert!(subscribed(&mk("pga", Some("major"), None), &cfg("pga", &["major"])));

        // F1 sessions are independent: a sprint subscription does not bring the race.
        assert!(subscribed(&mk("f1", None, Some("race")), &cfg("f1", &["race"])));
        assert!(!subscribed(&mk("f1", None, Some("quali")), &cfg("f1", &["race"])));
        assert!(subscribed(&mk("f1", None, Some("quali")), &cfg("f1", &["race", "quali"])));
        assert!(!subscribed(&mk("f1", None, Some("race")), &cfg("f1", &["sprint"])));

        // THE POINT OF THE CHANGE: nothing implies anything else. Under the old
        // ladder "semi" silently claimed the Final and "knockout" claimed every
        // round beyond it.
        assert!(subscribed(&mk("wc", Some("quarter"), None), &cfg("wc", &["quarter"])));
        assert!(!subscribed(&mk("wc", Some("final"), None), &cfg("wc", &["semi"])), "semi must NOT imply the final");
        assert!(!subscribed(&mk("wc", Some("quarter"), None), &cfg("wc", &["knockout"])), "knockout is its own round");
        assert!(subscribed(&mk("wc", Some("final"), None), &cfg("wc", &["semi", "final"])), "check both to get both");

        // NHL, the case from the ticket.
        assert!(!subscribed(&mk("nhl", Some("final"), None), &cfg("nhl", &["conf"])), "conf must NOT imply the cup");
        assert!(subscribed(&mk("nhl", Some("final"), None), &cfg("nhl", &["conf", "final"])));

        // An empty set is "only my teams", and never matches on its own.
        assert!(!subscribed(&mk("nhl", Some("final"), None), &cfg("nhl", &[])));

        // "all" still claims an untagged regular-season game, at the weaker
        // priority so a targeted round wins the rotation slot.
        assert_eq!(game_option_priority(&mk("nhl", None, None), &cfg("nhl", &["all"])), Some(3));
        assert_eq!(game_option_priority(&mk("nhl", Some("final"), None), &cfg("nhl", &["all", "final"])), Some(2));
    }

    /// Rotation slots go by priority — manual adds first, then favorites, then
    /// targeted options, then blankets — and everything past the cap is reported
    /// as bumped (except for unconfigured boards sampling the slate).
    #[test]
    fn rotation_cap_prioritizes_and_reports_bumped() {
        let state = AppState::new_ephemeral();
        configure_all(&state, "b1");
        let side = |abbrev: &str| TeamSide {
            abbrev: abbrev.into(),
            name: abbrev.into(),
            location: String::new(),
            score: 0,
            primary: model::hex_to_rgb("111111"),
            secondary: model::hex_to_rgb("ffffff"), hits: None, errors: None, shots: None, shootout: None, aggregate: None,
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
                series: None,
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
                championship_option_ids: vec!["all".into()],
                dwell_secs: None,
                leaderboard_rows: None,
                show_round: None,
                show_thru: None,
                data_source: None,
                show_extras: None,
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

        // A board following NOTHING chooses nothing — so there is nothing to
        // bump either. (It used to sample the whole slate silently, which is
        // what put twenty unasked-for games on a freshly paired panel.)
        let (shown, bumped) = candidates_with_bumped(&state, &BoardConfig::default());
        assert!(shown.is_empty(), "nothing selected means nothing selected");
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
            series: None,
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

        let cfg = watch_all(); // unconfigured -> whole slate
        let ids: Vec<String> = candidate_games(&state, &cfg).iter().map(|g| g.id.clone()).collect();
        assert!(ids.contains(&"g1".to_string()), "observed final lingers");
        assert!(!ids.contains(&"g2".to_string()), "arrived-final never lingers");

        // Age the stamp past the linger: the game drops off the board.
        state
            .finished
            .lock()
            .unwrap()
            .insert("g1".into(), Finish {
                at: std::time::Instant::now() - FINAL_LINGER,
                wall: chrono::Utc::now(),
            });
        let ids: Vec<String> = candidate_games(&state, &cfg).iter().map(|g| g.id.clone()).collect();
        assert!(!ids.contains(&"g1".to_string()), "linger expires after 30m");
    }

    /// A board that follows every league.
    ///
    /// `BoardConfig::default()` used to mean this implicitly: an unconfigured
    /// board sampled the whole slate, so any test about rotation mechanics got
    /// its games for free. Nothing selected now means nothing selected, so a
    /// test that wants games has to say so — which is also more honest about
    /// what it is exercising.
    pub(crate) fn watch_all() -> BoardConfig {
        let leagues = [
            "nhl", "nba", "wnba", "mlb", "nfl", "cfb", "cbb", "wcbb", "mls", "epl", "wc", "wwc",
            "ucl", "cbase", "olyhm", "olyhw", "olybm", "olybw", "olysm", "olysw", "pga", "lpga",
        ];
        BoardConfig {
            sports: leagues
                .iter()
                .map(|id| SportConfig {
                    sport_id: (*id).into(),
                    championship_option_ids: vec!["all".into()],
                    ..Default::default()
                })
                .chain(std::iter::once(SportConfig {
                    // F1 has no blanket option; its four session kinds ARE the
                    // whole schedule.
                    sport_id: "f1".into(),
                    championship_option_ids: vec![
                        "race".into(),
                        "quali".into(),
                        "sprint".into(),
                        "sprint-quali".into(),
                    ],
                    ..Default::default()
                }))
                .collect(),
            ..Default::default()
        }
    }

    /// Make a board follow every league, LEAVING its other settings alone.
    ///
    /// Merges rather than replaces: these tests set timezones, dwell overrides
    /// and off modes on the same board, and a helper that clobbered them would
    /// quietly turn the test into a different test.
    fn configure_all(state: &AppState, board_id: &str) {
        let mut cfg = state.config_for(board_id);
        cfg.sports = watch_all().sports;
        state.configs.lock().unwrap().insert(board_id.to_string(), cfg);
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
            series: None,
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

    /// Sleep and "nothing on" are different states and must draw differently.
    ///
    /// They shared one path, so pressing the knob at bedtime lit the clock you
    /// had configured for the no-games case. Off has to mean off.
    #[test]
    fn a_sleeping_board_is_dark_and_an_idle_one_is_not() {
        let state = AppState::new_ephemeral();
        state.games.lock().unwrap().clear(); // nothing to show either way
        let mut cfg = watch_all();
        cfg.off_mode = Some("clock".into());

        // Awake with nothing on: the configured off mode lights the panel.
        apply_mode(&mut cfg, MODE_SCORES);
        state.configs.lock().unwrap().insert("b1".into(), cfg.clone());
        let awake = manifest_for_board(&state, "b1");
        assert!(awake.display_on, "an idle board still draws its off mode");
        assert!(!awake.rotation.is_empty(), "and has something to draw");

        // Off: dark, whatever the idle screen says.
        apply_mode(&mut cfg, MODE_OFF);
        state.configs.lock().unwrap().insert("b1".into(), cfg);
        // Clear the SLEEPING splash so this tests the resting state, not the
        // notice that (deliberately) precedes it.
        state.notices.lock().unwrap().remove("b1");
        let asleep = manifest_for_board(&state, "b1");
        assert!(!asleep.display_on, "off means off");
        assert!(asleep.rotation.is_empty(), "nothing to draw while asleep");
    }

    /// A board that reboots mid-session must be put back to work, or the app
    /// shows a live link onto a board that has gone quiet.
    #[test]
    fn a_live_session_re_arms_a_board_that_comes_back() {
        let state = AppState::new_ephemeral();
        assert_eq!(debug_session_remaining(&state, "b1"), None, "no session, nothing to re-arm");

        mint_debug_token(&state, "b1", 600);
        let left = debug_session_remaining(&state, "b1").expect("session is live");
        assert!(left > 500 && left <= 600, "reports the time REMAINING, got {left}");

        // Scoped to its own board.
        assert_eq!(debug_session_remaining(&state, "b2"), None);

        // Expired sessions do not re-arm anything.
        let past = std::time::Instant::now();
        state
            .debug_tokens
            .lock()
            .unwrap()
            .insert("old".into(), ("b2".into(), past, past));
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(debug_session_remaining(&state, "b2"), None, "an expired window is over");

        // The LINK outlives the stream: once the session ends the board stops
        // being asked for more, but what it already said stays readable.
        let url = mint_debug_token(&state, "b3", 1);
        let token = url.rsplit('/').next().unwrap().to_string();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(debug_session_remaining(&state, "b3"), None, "the window is over");
        assert_eq!(
            resolve_debug_token(&state, &token).as_deref(),
            Some("b3"),
            "but the link still reads — the end of a window is when you want it"
        );

        // Switching off revokes, so a later reconnect stays quiet.
        state.debug_tokens.lock().unwrap().retain(|_, (b, _, _)| b != "b1");
        assert_eq!(debug_session_remaining(&state, "b1"), None);
    }

    /// End to end through the handler the app calls: mint, record, read, revoke.
    #[tokio::test]
    async fn the_debug_endpoint_serves_a_live_token_and_404s_a_dead_one() {
        use axum::extract::Path;
        let state = AppState::new_ephemeral();
        record_debug_log(&state, "b1", vec!["! [ws] reconnecting".into(), "[panel] stalled 90ms".into()]);
        let token = mint_debug_token(&state, "b1", 60).rsplit('/').next().unwrap().to_string();

        let resp = get_debug_log(State(state.clone()), Path(token.clone())).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert_eq!(
            resp.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
            "text/plain; charset=utf-8"
        );
        // Never cached: a log read from URLCache would quietly show a stale window.
        assert_eq!(resp.headers().get(axum::http::header::CACHE_CONTROL).unwrap(), "no-store");
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("[panel] stalled 90ms"), "serves what the board sent: {text:?}");
        assert!(text.starts_with("! [ws] reconnecting"), "oldest first, warnings marked");

        // The board id is not a key, however well-known it is.
        let resp = get_debug_log(State(state.clone()), Path("b1".into())).await;
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

        // Revoking (streaming off) kills the URL that was already handed out.
        state.debug_tokens.lock().unwrap().clear();
        let resp = get_debug_log(State(state), Path(token)).await;
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    /// The console can carry a board's SSID and — worse — its pairing code, so
    /// the read endpoint is a capability, not a guessable address.
    ///
    /// A board id would have been the obvious path segment and the wrong one:
    /// it is long-lived, the board prints it on its own console, and it is the
    /// board's PUBLIC KEY, which nothing else treats as secret.
    #[test]
    fn a_debug_log_is_only_readable_through_a_live_token() {
        let state = AppState::new_ephemeral();
        record_debug_log(&state, "board-1", vec!["[ws] connected".into()]);

        // A token resolves to exactly its board, and only while it lives.
        let url = mint_debug_token(&state, "board-1", 60);
        let token = url.rsplit('/').next().unwrap().to_string();
        assert_eq!(resolve_debug_token(&state, &token).as_deref(), Some("board-1"));
        assert_eq!(token.len(), 32, "128 bits of nonce, hex-encoded");

        // The board id itself is NOT a key.
        assert_eq!(resolve_debug_token(&state, "board-1"), None);
        assert_eq!(resolve_debug_token(&state, "not-a-token"), None);

        // A second board's token cannot read the first board's console.
        let other = mint_debug_token(&state, "board-2", 60);
        let other_token = other.rsplit('/').next().unwrap().to_string();
        assert_eq!(resolve_debug_token(&state, &other_token).as_deref(), Some("board-2"));

        // Expiry is enforced on read, not by a sweeper that might not have run.
        let past = std::time::Instant::now();
        state
            .debug_tokens
            .lock()
            .unwrap()
            .insert("stale".into(), ("board-1".into(), past, past));
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(resolve_debug_token(&state, "stale"), None, "an expired token is no token");
    }

    /// Connecting must never reach ESPN. Enforced structurally: nothing outside
    /// the poll loop's own timer may start a fetch.
    ///
    /// The old wake-on-connect made client churn into upstream load one for one
    /// — a board reconnecting every ~40s had the server sweeping twenty leagues
    /// each time. This is a source check rather than a behavioural one because
    /// the failure is a *reintroduction*: someone adding a "refresh now" path
    /// would not break any existing assertion, and the symptom (a 403 from ESPN,
    /// eventually) shows up nowhere near the change.
    #[test]
    fn nothing_but_the_timer_can_trigger_a_fetch() {
        let main_rs = include_str!("main.rs");
        let ws_rs = include_str!("ws.rs");

        // The notify handle that used to carry connect -> fetch is gone. Split
        // so this assertion doesn't match its own source.
        let gone = concat!("poll_", "wake");
        for (name, src) in [("main.rs", main_rs), ("ws.rs", ws_rs)] {
            assert!(
                !src.contains(gone),
                "{name} still references {gone} — connecting must not trigger a fetch"
            );
        }
        // And the loop awaits its interval directly, rather than selecting over
        // the timer and something else that could fire on demand.
        assert!(
            main_rs.contains("THE TIMER IS THE ONLY TRIGGER"),
            "the poll loop's trigger rule lost its marker comment"
        );
        let loop_body = main_rs
            .split("THE TIMER IS THE ONLY TRIGGER")
            .nth(1)
            .expect("marker present")
            .split("let fetched_at")
            .next()
            .expect("fetch follows the wait");
        assert!(
            loop_body.contains("interval.tick().await"),
            "the poll loop should await its timer directly"
        );
        assert!(
            !loop_body.contains("select!"),
            "the poll loop selects over something besides its timer again: {loop_body}"
        );
    }

    /// The setup screen is centered by measurement, so a line wider than the
    /// panel doesn't wrap or truncate — it silently draws off both edges.
    #[test]
    fn the_setup_screen_fits_the_panel() {
        for (text, big, _) in SETUP_LINES {
            let w = render::text_width(text, big);
            assert!(
                w <= scoreboard_protocol::FRAME_WIDTH as i32,
                "{text:?} is {w}px wide, the panel is {}px",
                scoreboard_protocol::FRAME_WIDTH
            );
        }
    }

    /// A freshly paired board must claim NOTHING. It used to claim the whole
    /// slate, so someone who had just finished pairing got twenty games across
    /// every league they had never asked for.
    #[test]
    fn a_board_with_no_preferences_shows_the_setup_screen_not_every_game() {
        let state = AppState::new_ephemeral();
        *state.games.lock().unwrap() =
            vec![live_game("g1"), live_game("g2"), live_game("g3")];
        let cfg = BoardConfig::default();
        assert!(board_prefs(&cfg).unconfigured, "precondition: nothing configured");
        assert!(
            candidate_games(&state, &cfg).is_empty(),
            "nothing selected means nothing selected"
        );

        // And the panel says what to do about it rather than going dark.
        state.configs.lock().unwrap().insert("b1".into(), cfg);
        let m = manifest_for_board(&state, "b1");
        assert!(m.display_on, "the setup screen is lit");
        assert_eq!(m.rotation.len(), 1, "one screen, not a rotation of games");

        // Favoriting one team ends it: real games, no setup screen.
        let mut cfg = BoardConfig::default();
        cfg.sports = vec![SportConfig {
            sport_id: "nhl".into(),
            favorite_team_ids: vec!["nhl-bos".into()],
            ..Default::default()
        }];
        assert!(!board_prefs(&cfg).unconfigured, "one favorite is configured");
    }

    /// A league can switch off the box-score extras, and only its own.
    ///
    /// The switch clears the DATA in `board_view` rather than passing a flag
    /// into the renderers: `strip_sub_line` and `draw_hockey_situation` already
    /// draw nothing when these are None, so they stay pure functions of the
    /// Game and never learn a preference exists. It also inherits spoiler
    /// safety, because this is the pass redaction already runs in.
    #[test]
    fn a_league_can_turn_off_its_box_score_extras() {
        let mut mlb = Game {
            id: "g".into(),
            sport_id: "mlb".into(),
            away: TeamSide::empty("DET"),
            home: TeamSide::empty("CLE"),
            status: GameStatus::Final,
            ordinal: String::new(),
            clock: String::new(),
            start_time: String::new(),
            end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            situation: None,
            pregame: None,
        };
        mlb.away.hits = Some(6);
        mlb.home.hits = Some(9);
        mlb.home.errors = Some(1);

        // Default: nothing configured, everything drawn, and NOT cloned.
        let bare = BoardConfig::default();
        let view = board_view(&bare, &mlb);
        assert!(matches!(view, std::borrow::Cow::Borrowed(_)), "the common case must not clone");
        assert_eq!(view.away.hits, Some(6));

        // Explicitly on is the same as absent.
        let on = BoardConfig {
            sports: vec![SportConfig {
                sport_id: "mlb".into(),
                show_extras: Some(true),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(board_view(&on, &mlb).away.hits, Some(6));

        // Off: every extra field cleared, on both sides.
        let off = BoardConfig {
            sports: vec![SportConfig {
                sport_id: "mlb".into(),
                show_extras: Some(false),
                ..Default::default()
            }],
            ..Default::default()
        };
        let view = board_view(&off, &mlb);
        assert_eq!(view.away.hits, None);
        assert_eq!(view.home.hits, None);
        assert_eq!(view.home.errors, None);
        // The score itself is untouched — this hides detail, not results.
        assert_eq!(view.home.score, mlb.home.score);

        // And it is per league: a hockey game on the same board keeps its shots.
        let mut nhl = mlb.clone();
        nhl.sport_id = "nhl".into();
        nhl.away.shots = Some(21);
        assert_eq!(board_view(&off, &nhl).away.shots, Some(21), "the mlb switch is mlb's alone");

        // Composes with the round switch rather than fighting it.
        let mut golf = mlb.clone();
        golf.sport_id = "pga".into();
        golf.ordinal = "R2".into();
        let both = BoardConfig {
            sports: vec![SportConfig {
                sport_id: "pga".into(),
                show_round: Some(false),
                show_extras: Some(false),
                ..Default::default()
            }],
            ..Default::default()
        };
        let view = board_view(&both, &golf);
        assert!(view.ordinal.is_empty());
        assert_eq!(view.away.hits, None);
    }

    /// Only leagues that draw extras offer the switch, and the label names what
    /// disappears — an empty string is how the app knows to show no row.
    #[test]
    fn the_extras_label_gates_and_names_the_switch() {
        assert_eq!(extras_label_for("mlb"), "Hits and errors");
        assert_eq!(extras_label_for("cbase"), "Hits and errors");
        assert_eq!(extras_label_for("nhl"), "Shots on goal");
        assert_eq!(extras_label_for("olyhm"), "Shots on goal");
        for id in ["mls", "epl", "ucl", "wc", "olysw"] {
            assert_eq!(extras_label_for(id), "Shootout and aggregate", "{id}");
        }
        // Leagues that draw none get no switch at all.
        for id in ["nfl", "cfb", "nba", "cbb", "pga", "f1", "made-up"] {
            assert!(extras_label_for(id).is_empty(), "{id} should offer no switch");
        }
    }

    /// Demo data is the deliberate exception — it exists to fill a panel that
    /// has no preferences yet, so it must keep sampling the slate.
    #[test]
    fn demo_data_still_fills_an_unconfigured_board() {
        let state = AppState::new_ephemeral();
        *state.demo.lock().unwrap() = vec![live_game("d1"), live_game("d2")];
        let cfg = BoardConfig { demo_data: true, ..Default::default() };
        assert!(board_prefs(&cfg).unconfigured);
        assert!(
            !candidate_games(&state, &cfg).is_empty(),
            "a demo toggle that shows nothing looks broken"
        );
    }

    /// Turning the round off has to reach BOTH surfaces, or the panel and the
    /// card disagree about the same game on the same board.
    #[test]
    fn the_round_indicator_toggle_reaches_the_panel_and_the_card() {
        let golf = Game {
            id: "401".into(),
            sport_id: "pga".into(),
            away: crate::model::TeamSide::empty("PGA"),
            home: crate::model::TeamSide::empty(""),
            status: GameStatus::Active,
            ordinal: "R3".into(),
            clock: String::new(),
            start_time: "2026-08-08T11:00Z".into(),
            end_time: None,
            leaderboard: Some(crate::model::Leaderboard { matchplay: None, laps: None,
                flag: None,
                stage: None,
                quali: None,
                name: "WYNDHAM".into(),
                rows: vec![crate::model::LeaderRow {
                    name: "HOSSLER".into(),
                    value: "-12".into(),
                    ..Default::default()
                }],
            }),
            champ: None,
            series: None,
            session: None,
            situation: None,
            pregame: None,
        };
        let cfg_with = |show: Option<bool>| BoardConfig {
            sports: vec![SportConfig { sport_id: "pga".into(), show_round: show, ..Default::default() }],
            ..Default::default()
        };

        // Default (nothing stored) shows it, as does an explicit yes.
        for cfg in [BoardConfig::default(), cfg_with(None), cfg_with(Some(true))] {
            let view = board_view(&cfg, &golf);
            assert_eq!(view.ordinal, "R3", "shown unless turned off");
            assert!(to_summary(&view, None).detail.starts_with("R3"), "card leads with the round");
        }

        // Off: gone from the game the renderer and the card builder both see.
        let cfg = cfg_with(Some(false));
        let view = board_view(&cfg, &golf);
        assert_eq!(view.ordinal, "", "the panel has nothing to draw");
        let detail = to_summary(&view, None).detail;
        assert!(!detail.contains("R3"), "the card must not still say it: {detail:?}");
        assert_eq!(detail, "HOSSLER -12", "and the leader is still there");

        // Per league: a different league on the same board is untouched, and
        // the common case doesn't clone.
        let f1 = Game { sport_id: "f1".into(), ordinal: String::new(), ..golf.clone() };
        assert!(matches!(board_view(&cfg, &f1), std::borrow::Cow::Borrowed(_)), "no needless clone");
    }

    /// The OpenF1 swap: rows replace ESPN's only when the board OPTED IN,
    /// only on the matching session, and only while fresh — and the offer
    /// itself exists only when the server carries the personal-use key.
    #[test]
    fn the_openf1_swap_is_opt_in_session_matched_and_fresh() {
        let f1 = Game {
            id: "600057441-quali".into(),
            sport_id: "f1".into(),
            away: crate::model::TeamSide::empty("F1"),
            home: crate::model::TeamSide::empty(""),
            status: GameStatus::Active,
            ordinal: String::new(),
            clock: String::new(),
            start_time: "2026-08-22T14:00Z".into(),
            end_time: None,
            leaderboard: Some(crate::model::Leaderboard { matchplay: None, laps: None,
                flag: None,
                stage: None,
                quali: None,
                name: "DUTCH GP Q".into(),
                rows: vec![crate::model::LeaderRow {
                    rank: "1".into(),
                    name: "PIA".into(),
                    value: String::new(),
                    ..Default::default()
                }],
            }),
            champ: None,
            series: None,
            session: Some("quali".into()),
            situation: None,
            pregame: None,
        };
        let rows = |session: &'static str, age: Duration| openf1::SessionRows {
            session,
            stage: None,
            quali: None,
            flag: None,
            laps: None,
            rows: vec![crate::model::LeaderRow {
                rank: "1".into(),
                name: "NOR".into(),
                value: String::new(),
                ..Default::default()
            }],
            fetched: Instant::now() - age,
        };
        let cfg_with = |source: Option<&str>| BoardConfig {
            sports: vec![SportConfig {
                sport_id: "f1".into(),
                data_source: source.map(String::from),
                ..Default::default()
            }],
            ..Default::default()
        };
        let name_of = |v: &Game| v.leaderboard.as_ref().unwrap().rows[0].name.clone();

        // Default (no selection): ESPN rows, no clone.
        let fresh = rows("quali", Duration::from_secs(0));
        let v = board_view_with(&cfg_with(None), &f1, Some(&fresh));
        assert!(matches!(v, std::borrow::Cow::Borrowed(_)), "default costs nothing");
        assert_eq!(name_of(&v), "PIA");

        // Opted in + matching session + fresh: swapped, flag included.
        let mut flagged = rows("quali", Duration::from_secs(0));
        flagged.flag = Some("red");
        let v = board_view_with(&cfg_with(Some("openf1")), &f1, Some(&flagged));
        assert_eq!(name_of(&v), "NOR", "the alternate feed's rows");
        assert_eq!(
            v.leaderboard.as_ref().unwrap().flag.as_deref(),
            Some("red"),
            "track status rides the swap"
        );

        // Wrong session: sprint-quali rows must NOT land on the quali game.
        let sprint = rows("sprint-quali", Duration::from_secs(0));
        let v = board_view_with(&cfg_with(Some("openf1")), &f1, Some(&sprint));
        assert_eq!(name_of(&v), "PIA", "session mismatch keeps ESPN rows");

        // Stale: a dead feed degrades to ESPN rather than freezing.
        let stale = rows("quali", Duration::from_secs(700));
        let v = board_view_with(&cfg_with(Some("openf1")), &f1, Some(&stale));
        assert_eq!(name_of(&v), "PIA", "stale rows are ignored");
    }

    /// Re-render a capture from `scripts/capture-f1.py` through the server's
    /// own code, both feeds, every snapshot:
    ///
    ///   F1_CAPTURE_DIR=<capture root or one snapshot> F1_FIXTURE_OUT=/tmp/f1 \
    ///     cargo test -p server render_f1_capture -- --ignored
    ///
    /// Per snapshot `<ts>`: `<ts>-espn.png` (the ESPN game as parsed from the
    /// captured responses), `<ts>-openf1.png` (that same game with the
    /// captured OpenF1 rows swapped in exactly as a board opted into OpenF1
    /// sees it), both as full-depth board frames, and `<ts>-summary.json`
    /// with each view's rows, flag and laps side by side.
    #[test]
    #[ignore = "renders a capture; run with --ignored and F1_CAPTURE_DIR"]
    fn render_f1_capture() {
        let root = std::path::PathBuf::from(std::env::var("F1_CAPTURE_DIR").expect("F1_CAPTURE_DIR"));
        let out = std::path::PathBuf::from(std::env::var("F1_FIXTURE_OUT").expect("F1_FIXTURE_OUT"));
        std::fs::create_dir_all(&out).unwrap();
        let mut snaps: Vec<std::path::PathBuf> = if root.join("meta.json").exists() {
            vec![root.clone()]
        } else {
            std::fs::read_dir(&root)
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.join("meta.json").exists())
                .collect()
        };
        snaps.sort();
        assert!(!snaps.is_empty(), "no snapshots under {}", root.display());
        let openf1_cfg = BoardConfig {
            sports: vec![SportConfig { sport_id: "f1".into(), data_source: Some("openf1".into()), ..Default::default() }],
            ..Default::default()
        };
        let summary = |g: &Game| {
            let lb = g.leaderboard.as_ref();
            serde_json::json!({
                "status": format!("{:?}", g.status),
                "title": lb.map(|l| l.name.clone()),
                "flag": lb.and_then(|l| l.flag.clone()),
                "laps": lb.and_then(|l| l.laps).map(|l| format!("{}/{}", l.completed, l.total)),
                "rows": lb.map(|l| l.rows.iter().map(|r| format!("{:>2} {} {} {}", r.rank, r.name, r.value, r.tire.map(String::from).unwrap_or_default())).collect::<Vec<_>>()),
            })
        };
        let draw = |g: &Game, path: &std::path::Path| {
            let (_, px, _) = render::render_board_frame(
                g, render::LEADERBOARD_MAX_ROWS, None, None, scoreboard_protocol::NameStyle::Name,
            );
            std::fs::write(path, render_png(&px)).unwrap();
        };
        let mut rendered = 0;
        for snap in &snaps {
            let meta: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(snap.join("meta.json")).unwrap()).unwrap();
            let at = meta["captured_at"]
                .as_str()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&chrono::Utc))
                .expect("meta.json captured_at");
            let session = meta["session"].as_str().unwrap_or("race");
            let name = snap.file_name().unwrap().to_string_lossy().to_string();
            let games = espn::replay_f1_snapshot(&snap.join("espn"));
            let Some(game) = games.into_iter().find(|g| g.session.as_deref() == Some(session)) else {
                eprintln!("{name}: no ESPN {session} game");
                continue;
            };
            let espn_view = board_view_with(&BoardConfig::default(), &game, None).into_owned();
            draw(&espn_view, &out.join(format!("{name}-espn.png")));
            let of1 = openf1::replay_snapshot(&snap.join("openf1"), at);
            let of1_view = of1.as_ref().map(|rows| board_view_with(&openf1_cfg, &game, Some(rows)).into_owned());
            if let Some(v) = &of1_view {
                draw(v, &out.join(format!("{name}-openf1.png")));
            }
            let s = serde_json::json!({
                "captured_at": at.to_rfc3339(),
                "espn": summary(&espn_view),
                "openf1": of1_view.as_ref().map(summary),
                "openf1_swapped": of1.as_ref().is_some_and(|r| r.session == session),
            });
            std::fs::write(out.join(format!("{name}-summary.json")), serde_json::to_vec_pretty(&s).unwrap()).unwrap();
            rendered += 1;
        }
        eprintln!("rendered {rendered} of {} snapshots into {}", snaps.len(), out.display());
    }

    /// The catalog offers the alternate source ONLY for f1 and ONLY when the
    /// server carries credentials — no key, no switch, anywhere.
    #[test]
    fn data_sources_are_offered_only_with_credentials() {
        assert!(data_sources_for("f1", false).is_empty(), "no key, no offer");
        assert!(data_sources_for("nhl", true).is_empty(), "only f1 has an alternate");
        let offered = data_sources_for("f1", true);
        assert_eq!(offered[0].id, "espn", "the FIRST entry is the default");
        assert_eq!(offered[1].id, "openf1");
    }

    /// A pin expires WITH its game: pinned race finishes and ages out, the
    /// pin clears, the config persists, and the app stops dimming a
    /// dashboard with nothing visibly pinned on it. An empty slate (feed
    /// hiccup) must never cost anyone their pin.
    #[test]
    fn a_pin_expires_with_its_game() {
        let state = AppState::new_ephemeral();
        let live = Game {
            id: "600057441-race".into(),
            sport_id: "f1".into(),
            away: crate::model::TeamSide::empty("F1"),
            home: crate::model::TeamSide::empty(""),
            status: GameStatus::Active,
            ordinal: String::new(),
            clock: String::new(),
            start_time: "2026-08-23T13:00Z".into(),
            end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: Some("race".into()),
            situation: None,
            pregame: None,
        };
        *state.games.lock().unwrap() = vec![live.clone()];
        let mut cfg = BoardConfig::default();
        cfg.focused_game_id = Some(live.id.clone());
        state.configs.lock().unwrap().insert("b1".into(), cfg);

        // Live: the pin holds.
        sweep_stale_pins(&state);
        assert_eq!(
            state.config_for("b1").focused_game_id.as_deref(),
            Some("600057441-race"),
            "a live pinned game keeps its pin"
        );

        // Feed hiccup: empty slate must not clear it.
        *state.games.lock().unwrap() = Vec::new();
        sweep_stale_pins(&state);
        assert!(state.config_for("b1").focused_game_id.is_some(), "no verdict from an empty slate");

        // Final, past its linger (no watched finish recorded): not showable.
        let done = Game { status: GameStatus::Final, ..live };
        *state.games.lock().unwrap() = vec![done];
        sweep_stale_pins(&state);
        assert!(
            state.config_for("b1").focused_game_id.is_none(),
            "the pin expires with the game"
        );
    }

    /// The holes-played column follows the same switch shape as the round:
    /// absent means shown, off clears the DATA so the renderer never learns a
    /// preference exists — and the untouched case must not clone.
    #[test]
    fn the_thru_toggle_clears_the_column_data() {
        let golf = Game {
            id: "402".into(),
            sport_id: "pga".into(),
            away: crate::model::TeamSide::empty("PGA"),
            home: crate::model::TeamSide::empty(""),
            status: GameStatus::Active,
            ordinal: "R3".into(),
            clock: String::new(),
            start_time: "2026-08-22T11:00Z".into(),
            end_time: None,
            leaderboard: Some(crate::model::Leaderboard { matchplay: None, laps: None,
                flag: None,
                stage: None,
                quali: None,
                name: "BMW".into(),
                rows: vec![
                    crate::model::LeaderRow {
                        name: "CLARK".into(),
                        value: "-11".into(),
                        thru: Some("6".into()),
                        ..Default::default()
                    },
                    crate::model::LeaderRow {
                        name: "SPAUN".into(),
                        value: "-9".into(),
                        thru: Some("7".into()),
                        ..Default::default()
                    },
                ],
            }),
            champ: None,
            series: None,
            session: None,
            situation: None,
            pregame: None,
        };
        let cfg_with = |show: Option<bool>| BoardConfig {
            sports: vec![SportConfig { sport_id: "pga".into(), show_thru: show, ..Default::default() }],
            ..Default::default()
        };

        // Default (nothing stored) shows it, as does an explicit yes — and
        // neither clones.
        for cfg in [BoardConfig::default(), cfg_with(None), cfg_with(Some(true))] {
            let view = board_view(&cfg, &golf);
            assert!(matches!(view, std::borrow::Cow::Borrowed(_)), "no needless clone");
            let rows = &view.leaderboard.as_ref().unwrap().rows;
            assert_eq!(rows[0].thru.as_deref(), Some("6"), "shown unless turned off");
        }

        // Off: the column data is simply gone; scores stay.
        let view = board_view(&cfg_with(Some(false)), &golf);
        let rows = &view.leaderboard.as_ref().unwrap().rows;
        assert!(rows.iter().all(|r| r.thru.is_none()), "no thru left to draw");
        assert_eq!(rows[0].value, "-11", "the score is untouched");

        // A finished round has no thru anywhere — turning the switch off then
        // must not clone either.
        let mut done = golf.clone();
        for r in &mut done.leaderboard.as_mut().unwrap().rows {
            r.thru = None;
        }
        assert!(
            matches!(board_view(&cfg_with(Some(false)), &done), std::borrow::Cow::Borrowed(_)),
            "nothing to strip, nothing to clone"
        );
    }

    /// A team sport gets no end time from ESPN — the scoreboard feed carries
    /// `completed: true` and nothing else — so the app's "Ended …" row stayed
    /// blank for every game that wasn't golf or F1. The server watched the game
    /// finish, though, so it can say when.
    #[test]
    fn a_finished_team_game_gets_the_end_time_the_server_observed() {
        let state = AppState::new_ephemeral();
        *state.games.lock().unwrap() = vec![live_game("g1")];
        let ended = Game { status: GameStatus::Final, ..live_game("g1") };
        assert_eq!(ended.end_time, None, "precondition: the feed gave us nothing");

        // Observed ending ten minutes ago.
        record_finished_transitions(
            &state,
            std::slice::from_ref(&ended),
            std::time::Instant::now() - Duration::from_secs(600),
        );
        let mut slate = vec![ended.clone()];
        stamp_observed_end_times(&state, &mut slate);

        let stamped = slate[0].end_time.as_deref().expect("an end time");
        let parsed = parse_espn_time(stamped).expect("normalized RFC3339");
        let ago = chrono::Utc::now().signed_duration_since(parsed).num_seconds();
        assert!(
            (540..=660).contains(&ago),
            "dated to when we saw it end (~600s ago), not to now; got {ago}s from {stamped}"
        );
    }

    /// The feed's own end time outranks ours, and a game we never watched finish
    /// gets no invented one.
    #[test]
    fn observed_end_times_never_overwrite_or_invent() {
        let state = AppState::new_ephemeral();
        *state.games.lock().unwrap() = vec![live_game("g1")];
        let ended = Game { status: GameStatus::Final, ..live_game("g1") };
        record_finished_transitions(&state, std::slice::from_ref(&ended), std::time::Instant::now());

        // A feed-supplied endDate (golf/F1) is left exactly as it was.
        let mut with_feed = vec![Game { end_time: Some("2026-08-09T04:00Z".into()), ..ended.clone() }];
        stamp_observed_end_times(&state, &mut with_feed);
        assert_eq!(with_feed[0].end_time.as_deref(), Some("2026-08-09T04:00Z"));

        // A game that was already Final when we started has no stamp, so no
        // end time — we did not see it end and will not guess.
        let unwatched = Game { status: GameStatus::Final, ..live_game("never-seen-live") };
        let mut slate = vec![unwatched];
        stamp_observed_end_times(&state, &mut slate);
        assert_eq!(slate[0].end_time, None, "no observation, no claim");
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
            series: None,
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

    // ---- "Live and Upcoming" (the app section that used to read "Now showing") ----

    /// An NHL game starting `hours` from now, `away` playing at ZZZ.
    fn scheduled_nhl(id: &str, away: &str, hours: i64) -> Game {
        use chrono::{Duration as D, Utc};
        Game {
            id: id.into(),
            sport_id: "nhl".into(),
            away: TeamSide::empty(away),
            home: TeamSide::empty("ZZZ"),
            status: GameStatus::Pregame,
            ordinal: String::new(),
            clock: String::new(),
            start_time: (Utc::now() + D::hours(hours)).format("%Y-%m-%dT%H:%MZ").to_string(),
            end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            situation: None,
            pregame: None,
        }
    }

    /// The same matchup, already underway.
    fn playing_nhl(id: &str, away: &str) -> Game {
        Game { id: id.into(), status: GameStatus::Active, ordinal: "1st".into(), ..scheduled_nhl(id, away, 0) }
    }

    /// Store a board under "b1" whose only preference is one favorite team.
    fn board_with_favorite(state: &AppState, team_key: &str) {
        let mut cfg = BoardConfig::default();
        cfg.sports.push(SportConfig {
            sport_id: "nhl".into(),
            favorite_team_ids: vec![team_key.into()],
            ..Default::default()
        });
        state.configs.lock().unwrap().insert("b1".into(), cfg);
    }

    fn now_showing_of(state: &AppState, board_id: &str) -> Vec<GameSummary> {
        match board_state_msg(state, board_id) {
            ServerMsg::BoardState { now_showing, .. } => now_showing,
            _ => panic!("expected BoardState"),
        }
    }

    /// The tester opened the app in the afternoon, saw an empty list, and
    /// assumed her board was broken — tonight's game was already claimed, the
    /// app just never said so. Claimed-but-unstarted games now ride along in
    /// the board state, flagged `queued` and carrying a real cached frame.
    #[test]
    fn board_state_lists_claimed_upcoming_games() {
        let state = AppState::new_ephemeral();
        let live = playing_nhl("live", "CHI");
        let tonight = scheduled_nhl("tonight", "CHI", 6);
        let stranger = scheduled_nhl("stranger", "TOR", 6);
        *state.games.lock().unwrap() = vec![live, tonight, stranger];
        board_with_favorite(&state, "nhl-chi");

        let now_showing = now_showing_of(&state, "b1");
        let find = |id: &str| now_showing.iter().find(|s| s.id == id);

        let live = find("live").expect("the live favorite is on the panel");
        assert!(!live.queued, "a game already rotating is HERE, not coming");
        assert!(!live.bumped);

        let tonight = find("tonight").expect("tonight's claimed game rides along");
        assert!(tonight.queued, "…flagged as upcoming");
        assert_eq!(tonight.status, "upcoming", "the app-facing word for not-yet-started");
        assert!(!tonight.bumped, "it never competed for a slot, so it lost nothing");
        assert!(
            !tonight.detail.is_empty(),
            "an upcoming card shows WHEN it starts, not a score: {:?}",
            tonight.detail
        );
        assert_eq!((tonight.away_score, tonight.home_score), (0, 0), "no score exists yet");

        // Both cards can render from the WS frame cache — no HTTP fallback.
        let frames = state.frames.lock().unwrap();
        for s in [live, tonight] {
            assert!(!s.frame_id.is_empty(), "{} has a frame id", s.id);
            assert!(frames.contains_key(&s.frame_id), "frame cached for {}", s.id);
        }

        assert!(find("stranger").is_none(), "a game no preference claims is on neither list");
    }

    /// An "Every match" league on a busy night claims the whole slate; the app
    /// gets the soonest `UPCOMING_CAP` and nothing more. A capped-out game is
    /// simply ABSENT — flagging it `bumped` would claim it lost a rotation
    /// slot, which never happened.
    #[test]
    fn upcoming_list_is_capped_to_the_soonest_games() {
        let state = AppState::new_ephemeral();
        // 16 claimed games, inserted in a deliberately unhelpful order (latest
        // first) so the cap can only pass by sorting on start time.
        let games: Vec<Game> = (1..=16)
            .rev()
            .map(|h| scheduled_nhl(&format!("g{h:02}"), "CHI", h))
            .collect();
        *state.games.lock().unwrap() = games;
        board_with_favorite(&state, "nhl-chi");

        let now_showing = now_showing_of(&state, "b1");
        let queued: Vec<&str> = now_showing.iter().filter(|s| s.queued).map(|s| s.id.as_str()).collect();
        assert_eq!(queued.len(), UPCOMING_CAP, "the cap holds");
        assert_eq!(
            queued,
            (1..=UPCOMING_CAP).map(|h| format!("g{h:02}")).collect::<Vec<_>>(),
            "soonest first, so the cap keeps the most imminent games"
        );
        assert!(
            now_showing.iter().all(|s| !s.bumped),
            "capped out is not bumped — nothing here ever held a rotation slot"
        );
        assert_eq!(now_showing.len(), UPCOMING_CAP, "nothing is live, so the list is upcoming-only");
    }

    /// The tallest frame the server can render must FIT the firmware's ws
    /// receive buffer, including the 64-byte frame id and the codec prefix.
    /// The buffer used to be exactly the pre-codec size (16448 = 64 + 128*128),
    /// so adding one codec byte made a 20-row leaderboard unreceivable: the
    /// reader rejects an oversized frame, which tears down the socket and
    /// reconnect-loops the board. Only boards that had just OTA'd would break.
    #[test]
    fn frame_wire_size_fits_the_firmware_buffer() {
        // Must track firmware-rs/src/main.rs's `frame` buffer.
        const FIRMWARE_WS_FRAME_BUF: usize = 16_512;
        // A 20-row leaderboard renders 128 rows of 64px RGB565.
        assert_eq!(render::LEADERBOARD_MAX_ROWS, 20, "the 128-row figure below assumes this");
        let payload_bytes = 128 * scoreboard_protocol::FRAME_ROW_BYTES;
        let wire = scoreboard_protocol::FRAME_ID_HEX_LEN + 1 /* codec */ + payload_bytes;
        assert!(
            wire <= FIRMWARE_WS_FRAME_BUF,
            "tallest frame is {wire} B on the wire, firmware buffer is {FIRMWARE_WS_FRAME_BUF} B \
             — raise the firmware buffer or lower LEADERBOARD_MAX_ROWS"
        );
    }

    /// A pre-codec server sends bare pixels — always a whole number of rows.
    /// `encode_frame_payload` must never produce that length, or a board that
    /// outlived its server (a rollback) cannot tell the two apart and decodes
    /// a zero first pixel byte as "RAW", shifting every pixel by one.
    #[test]
    fn encoded_payloads_are_never_mistakable_for_legacy_frames() {
        use scoreboard_protocol::{encode_frame_payload, frame_payload_is_legacy_raw};
        let codecs = [scoreboard_protocol::FRAME_CODEC_RAW, scoreboard_protocol::FRAME_CODEC_PAL_RLE];
        // Sweep real rendered frames plus adversarial synthetic ones.
        let mut cases: Vec<Vec<u8>> = Vec::new();
        let g = sample_game();
        cases.push(render::render_frame(&g, None, None, default_name_style(&g.sport_id)).1);
        for h in [1usize, 2, 32, 64, 128] {
            for pattern in 0..4u16 {
                let px: Vec<u8> = (0..h * 64)
                    .flat_map(|i| {
                        let v: u16 = match pattern {
                            0 => 0,
                            1 => (i % 2) as u16 * 0xFFFF,
                            2 => (i % 17) as u16 * 0x0801,
                            _ => (i as u16).wrapping_mul(2654),
                        };
                        v.to_le_bytes()
                    })
                    .collect();
                cases.push(px);
            }
        }
        for px in cases {
            let payload = encode_frame_payload(&px, &codecs);
            assert!(
                !frame_payload_is_legacy_raw(&payload),
                "payload of {} B is a whole number of rows — indistinguishable from a \
                 pre-codec server's bare pixels",
                payload.len()
            );
            assert_eq!(
                scoreboard_protocol::decode_frame_payload(&payload).as_deref(),
                Some(&px[..]),
                "round trip"
            );
        }
    }

    /// Every stock idle screen must COMPRESS. An idle rotation keeps all of its
    /// frames resident in the board's 24KB cache at once, and a frame that
    /// ships raw costs ~4KB against ~200B-2KB compressed — a few of those and
    /// the board evicts the next screen in the cycle and shows the previous one
    /// in its place. Before the encoder stepped off row-multiple lengths, the
    /// compact 24h clock shipped raw for 15% of the day (including 10:09).
    ///
    /// Clocks are swept over every minute; weather over a spread of
    /// conditions, day/night and temperatures.
    #[test]
    fn no_stock_idle_screen_ships_raw() {
        use scoreboard_protocol::{decode_frame_payload, encode_frame_payload, FRAME_CODEC_RAW};
        let codecs = board_core::SUPPORTED_FRAME_CODECS;
        let check = |label: &str, px: &[u8]| {
            let payload = encode_frame_payload(px, codecs);
            assert_ne!(payload[0], FRAME_CODEC_RAW, "{label} shipped raw ({} B)", payload.len());
            assert_eq!(decode_frame_payload(&payload).as_deref(), Some(px), "{label} round trip");
        };

        for (style, _) in render::CLOCK_STYLES {
            for h24 in [false, true] {
                for h in 0..24 {
                    for m in 0..60 {
                        let px = render::render_clock_styled(h, m, style, h24).1;
                        check(&format!("clock {style} h24={h24} {h:02}:{m:02}"), &px);
                    }
                }
            }
        }

        let sample = crate::weather::Weather::sample();
        let codes: [u16; 22] = [0, 1, 2, 3, 45, 51, 55, 56, 61, 63, 65, 66, 71, 73, 75, 77, 80, 82, 85, 95, 96, 99];
        let temps: [(i32, i32, i32); 6] =
            [(72, 78, 61), (-40, -31, -48), (-9, -2, -18), (7, 11, 1), (104, 112, 88), (118, 121, 101)];
        for &code in &codes {
            for is_day in [true, false] {
                for &(t, hi, lo) in &temps {
                    let mut w = sample.clone();
                    w.code = code;
                    w.is_day = is_day;
                    w.temp = t;
                    w.hi = hi;
                    w.lo = lo;
                    let at = format!("code={code} day={is_day} t={t}");
                    check(&format!("weather simple {at}"), &render::render_weather_simple(&w).1);
                    check(&format!("weather detailed {at}"), &render::render_weather_detailed(&w).1);
                    for h24 in [false, true] {
                        for (h, m) in [(10, 9), (11, 11), (12, 59), (0, 0), (23, 45)] {
                            let px = render::render_weather_clock(&w, h, m, h24).1;
                            check(&format!("weather clock {at} h24={h24} {h:02}:{m:02}"), &px);
                        }
                    }
                }
            }
        }
        for h24 in [false, true] {
            for h in 0..24 {
                for m in 0..60 {
                    let px = render::render_weather_clock(&sample, h, m, h24).1;
                    check(&format!("weather clock sample h24={h24} {h:02}:{m:02}"), &px);
                }
            }
        }
        let day_codes: [u16; 9] = [0, 2, 3, 45, 51, 56, 61, 71, 95];
        for &a in &day_codes {
            for &b in &day_codes {
                for &c in &day_codes {
                    for temps in [[(78, 61), (71, 58), (83, 64)], [(-2, -18), (-11, -27), (-8, -31)], [(104, 88), (112, 91), (109, 99)]] {
                        let mut w = sample.clone();
                        w.days = (0..3)
                            .map(|i| crate::weather::Day {
                                label: ["MON", "TUE", "WED"][i].into(),
                                code: [a, b, c][i],
                                hi: temps[i].0,
                                lo: temps[i].1,
                            })
                            .collect();
                        check(&format!("forecast {a},{b},{c} {temps:?}"), &render::render_weather_forecast(&w).1);
                    }
                }
            }
        }

        let params = std::collections::BTreeMap::new();
        for reading in [crate::weather::Reading::NoLocation, crate::weather::Reading::Unavailable] {
            let ctx = offmode::OffCtx { tz: None, params: &params, preview: false, weather: reading };
            for f in offmode::render_off_mode("weather", &ctx) {
                check("weather notice", &f.px);
            }
        }
    }

    /// A FINAL nobody watched end satisfies `game_queued` (not board_shows,
    /// still claimed), so without a pregame guard the "Live and Upcoming"
    /// section filled with COMPLETED games — sorted to the front, since their
    /// start times are the oldest — showing final scores under a "scheduled"
    /// badge. Worst on a virgin board, where every game is claimed.
    #[test]
    fn finished_games_never_appear_as_upcoming() {
        let state = AppState::new_ephemeral();
        configure_all(&state, "b1");
        let mut done = live_game("done");
        done.status = GameStatus::Final;
        done.away.score = 4;
        done.home.score = 1;
        done.start_time = "2020-01-01T00:00Z".into(); // long past: sorts first
        let mut soon = live_game("soon");
        soon.status = GameStatus::Pregame;
        soon.start_time = (chrono::Utc::now() + chrono::Duration::hours(6))
            .format("%Y-%m-%dT%H:%MZ")
            .to_string();
        *state.games.lock().unwrap() = vec![done, soon];

        // A virgin board claims everything (unconfigured -> priority Some(4)).
        let ServerMsg::BoardState { now_showing, .. } = board_state_msg(&state, "b1") else {
            panic!("expected BoardState");
        };
        assert!(
            !now_showing.iter().any(|g| g.id == "done"),
            "a completed game must never be listed as upcoming: {:?}",
            now_showing.iter().map(|g| (&g.id, &g.status)).collect::<Vec<_>>()
        );
        let up: Vec<&GameSummary> = now_showing.iter().filter(|g| g.queued).collect();
        assert_eq!(up.len(), 1, "only the not-yet-started game is upcoming");
        assert_eq!(up[0].id, "soon");
    }

    /// Upcoming games are DISPLAY-only: they exist so the app can say "your
    /// game is coming". The panel must rotate exactly what it rotated before.
    #[test]
    fn upcoming_games_do_not_change_the_board_rotation() {
        let state = AppState::new_ephemeral();
        let live = playing_nhl("live", "CHI");
        *state.games.lock().unwrap() = vec![live.clone()];
        board_with_favorite(&state, "nhl-chi");

        let frame_ids = |m: &Manifest| m.rotation.iter().map(|r| r.frame_id.clone()).collect::<Vec<_>>();
        let before = frame_ids(&manifest_for_board(&state, "b1"));
        assert_eq!(before.len(), 1, "precondition: one live game rotating");

        // Add a night's worth of claimed-but-unstarted games.
        {
            let mut games = state.games.lock().unwrap();
            games.extend((1..=5).map(|h| scheduled_nhl(&format!("later{h}"), "CHI", h)));
        }
        assert!(
            now_showing_of(&state, "b1").iter().any(|s| s.queued),
            "precondition: the app now sees upcoming games"
        );
        assert_eq!(frame_ids(&manifest_for_board(&state, "b1")), before, "the rotation is untouched");
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

    /// The summary a golf card is built from must actually CARRY the link —
    /// fixing the URL builder wasn't enough, because the leaderboard branch of
    /// `to_summary` returned early with a hardcoded None and never called it.
    #[test]
    fn leaderboard_summaries_carry_the_espn_link() {
        let mut g = live_game("401811960");
        g.sport_id = "pga".into();
        g.leaderboard = Some(crate::model::Leaderboard { matchplay: None, laps: None,
            flag: None,
            stage: None,
            quali: None,
            name: "ROCKET".into(),
            rows: Vec::new(),
        });
        let s = to_summary(&g, None);
        assert_eq!(
            s.espn_url.as_deref(),
            Some("https://www.espn.com/golf/leaderboard?tournamentId=401811960"),
            "the golf card's View on ESPN link"
        );

        // And a team game keeps its own form (live_game defaults to pga).
        let mut team = live_game("401881922");
        team.sport_id = "nhl".into();
        assert_eq!(
            to_summary(&team, None).espn_url.as_deref(),
            Some("https://www.espn.com/nhl/game/_/gameId/401881922")
        );
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
            series: None,
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
            series: None,
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
            series: None,
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
        let state = AppState::new_ephemeral();
        // The board must already CLAIM these games: AddGame on a game the board
        // does not follow is a manual EXTRA, which is a different assertion than
        // the un-exclude this test is about.
        configure_all(&state, "b1");
        let all = candidate_games(&state, &watch_all());
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
        let all = candidate_games(&state, &watch_all());
        assert!(all.len() >= 2, "mock slate should have several live games");

        let cfg = BoardConfig { focused_game_id: Some(all[0].id.clone()), ..watch_all() };
        state.configs.lock().unwrap().insert("b1".into(), cfg);

        let m = manifest_for_board(&state, "b1");
        assert_eq!(m.rotation.len(), 1, "the board shows only the pinned game");

        let ServerMsg::BoardState { now_showing, config, .. } = board_state_msg(&state, "b1") else {
            panic!("expected BoardState");
        };
        assert_eq!(now_showing.len(), all.len(), "the app still sees every candidate");
        assert_eq!(config.focused_game_id.as_deref(), Some(all[0].id.as_str()));
    }

    /// Picking "off" shows the "SLEEPING" splash; any other pick cancels it.
    #[test]
    fn mode_changes_drive_the_notice() {
        let state = AppState::new_ephemeral();
        // A CONFIGURED board with games on the slate: the scores pick below
        // must land in the rotation, not the gameless idle handoff.
        let mut cfg = watch_all();
        apply_mode(&mut cfg, MODE_OFF);
        state.configs.lock().unwrap().insert("b1".into(), cfg.clone());
        handle_mode_change(&state, "b1", &cfg);
        assert!(state.notices.lock().unwrap().contains_key("b1"), "off -> splash");
        let m = manifest_for_board(&state, "b1");
        assert!(m.display_on && m.rotation.len() == 1, "splash notice shows over the dark mode");
        assert_eq!(m.countdown_ms, Some(SPLASH_SECS as u32 * 1000));
        apply_mode(&mut cfg, MODE_SCORES);
        handle_mode_change(&state, "b1", &cfg);
        assert!(!state.notices.lock().unwrap().contains_key("b1"), "a lit pick cancels the splash");
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

    /// A notice manifest is ONE frame (the empty outlined bar) plus
    /// `countdown_ms` — the fill is the BOARD's device-clock animation. The
    /// old pre-rendered sweep cost ~4KB of board RAM per step, capping a 4s
    /// splash at 4 chunky jumps; this is the smooth replacement.
    #[test]
    fn notice_is_one_frame_plus_a_board_countdown() {
        let state = AppState::new_ephemeral();
        state.games.lock().unwrap().clear();
        handle_button(&state, "b1"); // press -> off, with the SLEEPING splash up
        let m = manifest_for_board(&state, "b1");

        assert_eq!(m.rotation.len(), 1, "one frame; the bar fill is device-side");
        assert_eq!(m.countdown_ms, Some(SPLASH_SECS as u32 * 1000));
        assert!(!m.rotation[0].scroll, "notices never scroll");
        // Dwell pads 2s past expiry: a single entry never advances, and the
        // expiry push swaps the screen before the pad could matter.
        assert_eq!(m.rotation[0].dwell_ms as u64, SPLASH_SECS * 1000 + 2000);

        // The frame ships the bar EMPTY (outline only): interior row is dark.
        let frames = state.frames.lock().unwrap();
        let (_, px) = frames.get(&m.rotation[0].frame_id).cloned().expect("notice frame cached");
        drop(frames);
        let y = FRAME_HEIGHT - 4; // interior fill row of the outlined box
        let lit = (1..FRAME_WIDTH - 1)
            .filter(|x| {
                let i = (y * FRAME_WIDTH + x) * 2;
                px[i] != 0 || px[i + 1] != 0
            })
            .count();
        assert_eq!(lit, 2, "outline verticals only — the interior arrives empty: {lit}");

        // A mid-notice re-render is identical, so push_board's dedup keeps the
        // board's countdown anchor (epoch unchanged, same frame id).
        let m2 = manifest_for_board(&state, "b1");
        assert_eq!(m.rotation[0].frame_id, m2.rotation[0].frame_id);
        assert_eq!(m.epoch, m2.epoch);
        assert_eq!(m.countdown_ms, m2.countdown_ms);
    }

    /// The 4s sleep splash: same single-frame + device-countdown shape.
    #[test]
    fn short_splash_is_one_frame_with_a_device_countdown() {
        let state = AppState::new_ephemeral();
        configure_all(&state, "b1");
        handle_button(&state, "b1"); // games showing -> sleep + 4s splash
        let m = manifest_for_board(&state, "b1");
        assert_eq!(m.rotation.len(), 1);
        assert_eq!(m.countdown_ms, Some(SPLASH_SECS as u32 * 1000));
        assert_eq!(m.rotation[0].dwell_ms as u64, SPLASH_SECS * 1000 + 2000, "dwell + anti-advance pad");
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
        // Configured and with games, so the wake press lands in the games
        // rotation (program 0) rather than the gameless idle handoff.
        state.configs.lock().unwrap().insert("b1".into(), watch_all());
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

    /// A board still reporting a zone (firmware 2026.7.21 and earlier) is
    /// honoured: valid names are adopted and persisted, junk is ignored, and
    /// re-reports of the same zone are no-ops (old firmware re-sends after
    /// every reconnect).
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

    /// The single question behind "can the server geolocate the board itself?".
    ///
    /// It cannot, on this deployment: Railway's raw TCP proxy is a Layer-4
    /// relay, so the address the board's connection arrives from belongs to
    /// Railway's internal network, and a Layer-4 stream carries no
    /// `X-Forwarded-For` to correct it with. Every shape that means "not the
    /// board's own address" must classify as `Infrastructure`, because that is
    /// the answer that makes the server keep the configured zone instead of
    /// inventing one.
    #[test]
    fn only_a_globally_routable_peer_could_locate_a_board() {
        use std::net::IpAddr;
        let c = |s: &str| classify_peer(s.parse::<IpAddr>().unwrap());

        // What a proxied/containerized deployment actually reports.
        assert_eq!(c("10.0.0.7"), PeerOrigin::Infrastructure, "RFC1918 /8");
        assert_eq!(c("172.17.0.2"), PeerOrigin::Infrastructure, "docker bridge");
        assert_eq!(c("192.168.1.44"), PeerOrigin::Infrastructure, "home LAN");
        assert_eq!(c("100.64.3.9"), PeerOrigin::Infrastructure, "carrier-grade NAT");
        assert_eq!(c("127.0.0.1"), PeerOrigin::Infrastructure, "loopback");
        assert_eq!(c("169.254.1.1"), PeerOrigin::Infrastructure, "link-local");
        assert_eq!(c("0.0.0.0"), PeerOrigin::Infrastructure);
        assert_eq!(c("::1"), PeerOrigin::Infrastructure, "v6 loopback");
        assert_eq!(c("fd00::1"), PeerOrigin::Infrastructure, "v6 unique-local");
        assert_eq!(c("fe80::1"), PeerOrigin::Infrastructure, "v6 link-local");

        // A real public address — only reachable when nothing proxies us.
        assert_eq!(c("8.8.8.8"), PeerOrigin::Public);
        assert_eq!(c("203.0.113.9"), PeerOrigin::Infrastructure, "TEST-NET-3 is documentation");
        assert_eq!(c("2606:4700::1111"), PeerOrigin::Public);
    }

    /// A board with no zone of its own falls back to the server-wide
    /// `DEFAULT_TIMEZONE`, and a configured board always outranks it. This is
    /// what keeps a brand-new board off UTC now that the firmware no longer
    /// geolocates itself — a value the owner SET, never a guess.
    #[test]
    fn configured_zone_outranks_the_server_default() {
        assert_eq!(parse_iana_tz("America/Denver"), Some(chrono_tz::America::Denver));
        assert_eq!(parse_iana_tz("Not/AZone"), None, "junk names never become a zone");
        assert_eq!(parse_iana_tz(""), None);

        // The board's own zone wins over any default.
        let cfg = BoardConfig { timezone: Some("Asia/Tokyo".into()), ..Default::default() };
        assert_eq!(board_tz(&cfg), Some(chrono_tz::Asia::Tokyo));

        // With DEFAULT_TIMEZONE unset (the test process never sets it), an
        // unconfigured board is still None — every format site then uses the
        // server's local zone, exactly as before this existed.
        assert_eq!(std::env::var("DEFAULT_TIMEZONE").ok(), None, "test env is clean");
        assert_eq!(board_tz(&BoardConfig::default()), None);

        // An unparseable stored zone is ignored rather than fatal.
        let junk = BoardConfig { timezone: Some("Not/AZone".into()), ..Default::default() };
        assert_eq!(board_tz(&junk), None);
    }

    /// The idle clock renders in the BOARD's timezone: two boards in different
    /// zones get different frames; SetTimezone round-trips through the config;
    /// an unparseable zone is ignored (the old zone stays).
    #[test]
    fn per_board_timezone_drives_the_clock() {
        let state = AppState::new_ephemeral();
        state.games.lock().unwrap().clear(); // idle -> off-state clock
        // Both boards must be configured: an unconfigured board answers with the
        // setup screen, and two setup screens are identical no matter the zone.
        configure_all(&state, "b1");
        configure_all(&state, "b2");

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

    /// The app's scroll toggle is a shortcut to the LEAGUE's leaderboard
    /// depth: 4 = static top-4 (no scrolling), None = default (10, scrolls).
    /// One knob, stored on the league like dwell — which is the fix for the
    /// old per-game list, whose entries were pruned whenever the event left
    /// the slate (every server restart, and golf between rounds).
    #[test]
    fn league_depth_is_the_scroll_toggle() {
        let state = AppState::new_ephemeral();
        configure_all(&state, "b1");
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

        // "Disable scrolling" = depth 4: the whole league goes static…
        crate::ws::apply_command(&state, "b1", Command::SetLeagueRows { sport_id: "f1".into(), rows: Some(4) });
        let m = manifest_for_board(&state, "b1");
        let e = entry_for(&m, "demo-f1");
        assert!(!e.scroll, "league at depth 4 must not scroll");
        assert_eq!(frame_h(&e), FRAME_HEIGHT, "static frame is panel-sized");
        // …while the OTHER leaderboard league (golf) is untouched.
        let golf = entry_for(&m, "demo-golf");
        assert!(golf.scroll, "the toggle is per-league, not global");

        // Summaries still advertise the game as scroll-CAPABLE while the
        // league is static — hiding the toggle would strand the user with no
        // way to re-enable it from the card.
        let ServerMsg::BoardState { now_showing, config, .. } = board_state_msg(&state, "b1") else {
            panic!("expected BoardState");
        };
        let f1 = now_showing.iter().find(|g| g.id == "demo-f1").unwrap();
        assert!(f1.scrolls, "deep leaderboards advertise the toggle even when static");
        let f1_cfg = config.sports.iter().find(|s| s.sport_id == "f1").unwrap();
        assert_eq!(f1_cfg.leaderboard_rows, Some(4), "the app reads the state off the depth");
        let team_game = now_showing.iter().find(|g| !g.home.is_empty()).unwrap();
        assert!(!team_game.scrolls, "team games never offer the toggle");

        // "Enable scrolling" = back to the default depth: the tall frame returns.
        crate::ws::apply_command(&state, "b1", Command::SetLeagueRows { sport_id: "f1".into(), rows: None });
        let m = manifest_for_board(&state, "b1");
        let e = entry_for(&m, "demo-f1");
        assert!(e.scroll && frame_h(&e) > FRAME_HEIGHT, "toggling back restores the scroll");
    }

    /// THE reported bug: the per-game no-scroll list was pruned against the
    /// live slate, so a server restart (empty slate until the first poll) or a
    /// tournament's overnight gap silently deleted the setting — and persisted
    /// the deletion. The league depth must survive both, like favorites do.
    #[test]
    fn scroll_setting_survives_an_empty_slate() {
        let state = AppState::new_ephemeral();
        *state.games.lock().unwrap() = espn::demo_games(0, 0);
        crate::ws::apply_command(&state, "b1", Command::SetLeagueRows { sport_id: "pga".into(), rows: Some(4) });

        // Server restart: the slate is empty until the first fetch. Prune runs.
        state.games.lock().unwrap().clear();
        prune_spoiler_lists(&state);
        tick_once(&state);

        let cfg = state.config_for("b1");
        let pga = cfg.sports.iter().find(|s| s.sport_id == "pga").unwrap();
        assert_eq!(pga.leaderboard_rows, Some(4), "scroll-off survives the restart window");

        // And ResetRotation clears rotation tweaks, NOT league settings.
        crate::ws::apply_command(&state, "b1", Command::ResetRotation);
        let cfg = state.config_for("b1");
        let pga = cfg.sports.iter().find(|s| s.sport_id == "pga").unwrap();
        assert_eq!(pga.leaderboard_rows, Some(4), "reset rotation keeps league scrolling");
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
        // POWERED ON with nothing to show — the state the off mode is actually
        // for. Sleep is dark now (see `a_sleeping_board_is_dark_and_an_idle_one_is_not`),
        // so `power: false` no longer reaches this path at all. `watch_all` because
        // an unconfigured board answers with the setup screen instead.
        let mut cfg = BoardConfig { power: true, ..watch_all() }; // default off_state = Clock
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
        configure_all(&s, "board-x");
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

    /// The headline requirement: the toggle swaps the GAMES and nothing else.
    #[test]
    fn demo_board_keeps_every_other_setting() {
        let state = AppState::new_ephemeral();
        let before = BoardConfig {
            brightness: 33,
            dwell_secs: 12,
            name: Some("Den".into()),
            timezone: Some("America/Chicago".into()),
            spoiler_all: true,
            off_state: OffState::Blank,
            ..Default::default()
        };
        state.configs.lock().unwrap().insert("b".into(), before.clone());
        crate::ws::apply_command(&state, "b", Command::SetDemoData { on: true });
        let after = state.config_for("b");
        assert!(after.demo_data, "the toggle took");
        assert_eq!(after.brightness, before.brightness);
        assert_eq!(after.dwell_secs, before.dwell_secs);
        assert_eq!(after.name, before.name);
        assert_eq!(after.timezone, before.timezone);
        assert_eq!(after.spoiler_all, before.spoiler_all);
        assert_eq!(after.off_state, before.off_state);
        assert_eq!(after.sports, before.sports);
    }

    /// A demo board reads the synthetic slate; a normal board is unaffected by
    /// the demo slate existing at all.
    #[test]
    fn demo_board_sees_the_synthetic_slate() {
        let state = AppState::new_ephemeral();
        *state.demo.lock().unwrap() = espn::demo_games(0, 0);
        let live_ids = with_slate(&state, Some(&BoardConfig::default()), |g| {
            g.iter().map(|g| g.id.clone()).collect::<Vec<_>>()
        });
        let demo_ids = with_slate(&state, Some(&BoardConfig { demo_data: true, ..Default::default() }), |g| {
            g.iter().map(|g| g.id.clone()).collect::<Vec<_>>()
        });
        assert!(!demo_ids.is_empty(), "the demo slate is populated");
        assert!(demo_ids.iter().all(|id| id.starts_with("demo-")), "{demo_ids:?}");
        assert!(live_ids.iter().all(|id| !id.starts_with("demo-")), "{live_ids:?}");
        // And no config at all means live, which is what the unscoped HTTP
        // routes rely on.
        let unscoped = with_slate(&state, None, |g| g.len());
        assert_eq!(unscoped, live_ids.len());
    }

    /// Regression: the id set was built from the live slate only, so a demo
    /// board's per-game spoiler hide was stripped on the next 30s tick and the
    /// hide silently un-stuck.
    #[test]
    fn prune_spoiler_lists_keeps_demo_ids() {
        let state = AppState::new_ephemeral();
        *state.demo.lock().unwrap() = espn::demo_games(0, 0);
        let demo_id = state.demo.lock().unwrap()[0].id.clone();
        state.configs.lock().unwrap().insert(
            "b".into(),
            BoardConfig {
                demo_data: true,
                spoiler_game_ids: vec![demo_id.clone()],
                ..Default::default()
            },
        );
        prune_spoiler_lists(&state);
        assert!(
            state.config_for("b").spoiler_game_ids.contains(&demo_id),
            "a demo game's spoiler hide must survive the prune"
        );
    }

    /// The data-loss guard. A row this binary cannot parse must survive on
    /// disk: the board falls back to defaults for the session, but the next
    /// command must NOT persist those defaults over the user's settings.
    #[test]
    fn an_unreadable_config_is_never_overwritten() {
        let path = std::env::temp_dir().join(format!("sb-quarantine-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let hostile = r#"{"brightness":"not-a-number","power":true,"sports":[]}"#;
        {
            let conn = open_db(&path).unwrap();
            conn.execute(
                "INSERT INTO boards (pubkey, config) VALUES (?1, ?2)",
                rusqlite::params!["board-x", hostile],
            )
            .unwrap();
        }
        {
            let state = AppState::new_inner(Some(open_db(&path).unwrap()), mock_catalog());
            assert!(state.unparsed.lock().unwrap().contains("board-x"), "quarantined");
            // The board still works, on defaults.
            assert_eq!(state.config_for("board-x").brightness, BoardConfig::default().brightness);
            // A command lands; the write must be refused.
            crate::ws::apply_command(&state, "board-x", Command::SetBrightness { level: 5 });
        }
        let conn = open_db(&path).unwrap();
        let stored: String = conn
            .query_row("SELECT config FROM boards WHERE pubkey = 'board-x'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, hostile, "the unreadable row must be preserved byte for byte");
        let _ = std::fs::remove_file(&path);
    }

    /// The daily report's "already sent today" marker survives a restart.
    ///
    /// It used to be a local variable, so "one a day" meant "one a day per
    /// PROCESS". Every deploy forgot it, and a deploy landing inside the send
    /// hour produced another report on the next five-minute tick — four arrived
    /// in 48 minutes on 2026-08-12, all of them saying OK. A monitoring channel
    /// that cries wolf is worse than no channel, because the next one is the
    /// one you ignore.
    #[test]
    fn the_daily_report_only_sends_once_across_restarts() {
        let path = std::env::temp_dir().join(format!("sb-daily-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let today = chrono::Utc::now().date_naive();
        // Send-hour logic, extracted: would this process send right now?
        let would_send = |last: Option<chrono::NaiveDate>| last != Some(today);

        {
            let state = AppState::new_inner(Some(open_db(&path).unwrap()), mock_catalog());
            let last = state.kv_get("daily_report_last_sent").and_then(|s| s.parse().ok());
            assert!(would_send(last), "a fresh day sends");
            state.kv_set("daily_report_last_sent", &today.to_string());
            // Same process, later tick: already sent.
            let last = state.kv_get("daily_report_last_sent").and_then(|s| s.parse().ok());
            assert!(!would_send(last), "the same process does not send twice");
        }
        // A restart — a deploy — must not forget.
        {
            let state = AppState::new_inner(Some(open_db(&path).unwrap()), mock_catalog());
            let last: Option<chrono::NaiveDate> =
                state.kv_get("daily_report_last_sent").and_then(|s| s.parse().ok());
            assert_eq!(last, Some(today), "the marker survived the restart");
            assert!(!would_send(last), "a redeploy inside the send hour stays quiet");
        }
        // Tomorrow sends again — the fix must not silence the report entirely.
        {
            let state = AppState::new_inner(Some(open_db(&path).unwrap()), mock_catalog());
            let last: Option<chrono::NaiveDate> =
                state.kv_get("daily_report_last_sent").and_then(|s| s.parse().ok());
            let tomorrow = today.succ_opt().unwrap();
            assert!(last != Some(tomorrow), "a new day is not suppressed");
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Without a DB the marker degrades to the old per-process behaviour, which
    /// is the right failure: a server that cannot remember should still report.
    #[test]
    fn the_daily_report_still_works_without_a_database() {
        let state = AppState::new_inner(None, mock_catalog());
        assert_eq!(state.kv_get("daily_report_last_sent"), None);
        state.kv_set("daily_report_last_sent", "2026-08-12"); // no-op, must not panic
        assert_eq!(state.kv_get("daily_report_last_sent"), None);
    }

    #[test]
    fn sqlite_round_trip() {
        // Persist a config + pairing, then reload a fresh AppState from the same DB.
        let path = std::env::temp_dir().join(format!("sb-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let state = AppState::new_inner(Some(open_db(&path).unwrap()), mock_catalog());
            let cfg = BoardConfig { brightness: 42, power: false, demo_data: true, ..Default::default() };
            state.persist_config("board-x", &cfg);
            state.persist_pairing("board-x", "ctrl-1");
        }
        {
            let state = AppState::new_inner(Some(open_db(&path).unwrap()), mock_catalog());
            let cfg = state.config_for("board-x");
            assert_eq!(cfg.brightness, 42);
            assert!(!cfg.power);
            assert!(cfg.demo_data, "demo mode must survive a restart");
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
            secondary: model::hex_to_rgb("FFFFFF"), hits: None, errors: None, shots: None, shootout: None, aggregate: None,
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
            series: None,
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
            championship_option_ids: vec!["conf".into(), "final".into()],
            championship_option_id: Some("conf".into()),
            dwell_secs: None,
            leaderboard_rows: None,
            show_round: None,
                show_thru: None,
                data_source: None,
            show_extras: None,
            spoiler: false,
            spoiler_team_ids: vec![],
            name_style: None,
        });
        // The "conf" tier covers the cup final but not an early round,
        // and the regular-season game isn't a championship game at all.
        let ids: Vec<String> = candidate_games(&state, &cfg).iter().map(|g| g.id.clone()).collect();
        assert_eq!(ids, ["cup"]);

        // Broaden to the "playoffs" tier -> the early round comes in too.
        cfg.sports[0].championship_option_ids =
            vec!["final".into(), "conf".into(), "playoffs".into()];
        let ids: Vec<String> = candidate_games(&state, &cfg).iter().map(|g| g.id.clone()).collect();
        assert_eq!(ids, ["cup", "rd1"]);

        // A one-off removal still beats the preference.
        cfg.excluded_game_ids.push("cup".into());
        let ids: Vec<String> = candidate_games(&state, &cfg).iter().map(|g| g.id.clone()).collect();
        assert_eq!(ids, ["rd1"]);
    }

    #[test]
    fn champ_option_coverage_matrix() {
        // The old nesting table survives only as the migration's expansion, so
        // that is where it is still pinned: an already-configured board must
        // end up subscribed to exactly what its single option used to cover.
        let set = |sport: &str, legacy: &str| expand_legacy_champ(sport, Some(legacy));
        assert_eq!(set("nhl", "playoffs"), ["final", "conf", "playoffs"]);
        assert_eq!(set("nhl", "conf"), ["conf", "final"]);
        assert_eq!(set("nhl", "final"), ["final"]);
        assert!(set("nhl", "none").is_empty());
        // CFB's options were containment-inverted: the CFP option covered the
        // bowls too, which is why it expands to both plus the generic tier.
        assert_eq!(set("cfb", "playoff"), ["playoff", "bowl", "playoffs"]);
        assert_eq!(set("cfb", "bowl"), ["bowl"]);
        // Golf's option id and its tier never matched; the expansion is where
        // that mismatch gets corrected.
        assert_eq!(set("pga", "majors"), ["major"]);
        // And the projection is the inverse, so migrating twice is a no-op.
        for (sport, legacy) in [("nhl", "conf"), ("cfb", "bowl"), ("pga", "majors"), ("f1", "gp-quali")] {
            let ids = set(sport, legacy);
            assert_eq!(
                legacy_champ_projection(sport, &ids).as_deref(),
                Some(legacy),
                "{sport}/{legacy} must project back to itself"
            );
        }
    }

    /// The tester read the old "Off / All playoffs / Every match" as a master
    /// switch for the whole league and thought "Off" would blank her teams. It
    /// is ADDITIVE, so every row has to say so — the floor keeps your teams,
    /// and each middle tier reads as an addition to them.
    #[test]
    fn championship_labels_read_as_additions_not_a_master_switch() {
        // Trailing entry: an unknown sport falls through to the generic set,
        // which has to read the same way.
        // Trailing "khl" is deliberately unknown: a fork's league falls through
        // to the generic set, which has to read the same way.
        for sport in ["nhl", "nba", "wnba", "mlb", "nfl", "cfb", "cbb", "wcbb", "mls", "epl", "wc", "wwc",
                      "ucl", "cbase", "olyhm", "olyhw", "olybm", "olybw", "olysm", "olysw", "khl"] {
            let opts = championship_options_for(sport);
            let none = opts.iter().find(|o| o.id == "none").expect("every league can opt out");
            assert_eq!(none.label, "Only selected teams", "{sport}: opting out still shows your teams");
            for o in opts.iter().filter(|o| o.id != "none" && o.id != "all") {
                assert!(
                    o.label.starts_with("Selected + "),
                    "{sport}/{}: {:?} must read as an addition to the teams you picked",
                    o.id,
                    o.label
                );
            }
            for o in opts.iter().filter(|o| o.id == "all") {
                assert_eq!(o.label, "Every match", "{sport}: the blanket option");
            }
        }

        // Golf and F1 have no teams to select — you subscribe by event kind, so
        // "selected teams" would be a lie there.
        for sport in ["pga", "lpga", "f1"] {
            let opts = championship_options_for(sport);
            assert_eq!(opts.iter().find(|o| o.id == "none").unwrap().label, "None", "{sport}");
            assert!(
                opts.iter().all(|o| !o.label.to_lowercase().contains("selected")),
                "{sport}: no teams exist to be 'selected'"
            );
        }

        // Boards STORE the option id, so relabeling must never disturb the set
        // or the order the app renders.
        let ids: Vec<String> = championship_options_for("nhl").iter().map(|o| o.id.clone()).collect();
        assert_eq!(ids, ["none", "final", "conf", "playoffs", "all"]);

        // Menu rows are one line on a phone; keep every label short enough to
        // land there instead of truncating to "Selected + March Madn…".
        for sport in ["nhl", "nba", "wnba", "mlb", "nfl", "cfb", "cbb", "wcbb", "mls", "epl", "wc", "wwc",
                      "ucl", "cbase", "olyhm", "olysw", "pga", "lpga", "f1"] {
            for o in championship_options_for(sport) {
                assert!(o.label.chars().count() <= 26, "{sport}/{}: {:?} is too long for a menu row", o.id, o.label);
            }
        }
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

    /// The reserved demo code never enters `pending` (a TRNG collision must
    /// not shadow the virtual board), and the demo identity forces demo data
    /// on a virgin config — the toggle can't strand a reviewer on the real
    /// slate.
    #[test]
    fn reserved_demo_code_never_registers() {
        let s = AppState::new_ephemeral();
        s.register_pairing_code(DEMO_PAIR_CODE.into(), "board-x");
        assert!(!s.pending.lock().unwrap().contains_key(DEMO_PAIR_CODE), "reserved code refused");
        assert!(try_pair(&s, DEMO_PAIR_CODE, "ctrl-1").is_err(), "nothing to consume via try_pair");

        assert!(is_demo_board("demo-abcdef012345"));
        assert!(!is_demo_board(&"ab".repeat(32)), "a real pubkey id is not demo");
        assert_eq!(demo_board_id(&"ab".repeat(32)), "demo-abababababab");
        assert!(s.config_for("demo-abcdef012345").demo_data, "demo forced by identity, no stored config needed");
        assert!(!s.config_for(&"ab".repeat(32)).demo_data, "real boards keep the stored default");
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

        // Following every league -> the whole live slate.
        assert_eq!(ids(&watch_all()).len(), 3);
        // Following nothing -> nothing. (This used to be the whole slate too,
        // which is what made a freshly paired board light up with every game.)
        assert!(ids(&BoardConfig::default()).is_empty());

        // Favorites: only matching games (BOS nhl -> g1). Starts from an EMPTY
        // config, not `watch_all()` — the point is what the favorite claims.
        let mut cfg = BoardConfig::default();
        cfg.sports.push(SportConfig {
            sport_id: "nhl".into(),
            favorite_team_ids: vec!["nhl-bos".into()],
            championship_option_id: None,
            championship_option_ids: Vec::new(),
            dwell_secs: None,
            leaderboard_rows: None,
            show_round: None,
                show_thru: None,
                data_source: None,
            show_extras: None,
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

    /// GET /firmware/notes/{version}: plain-text patch notes per released
    /// version — and a hostile {version} 404s BEFORE any filesystem access,
    /// because that segment becomes a filename. Runs over real HTTP so axum's
    /// percent-decoding is in the loop (a "..%2F" segment reaches the handler
    /// as "../"), and so registering the static "notes" segment alongside the
    /// two :capture /firmware routes is proven conflict-free (a matchit
    /// The demo routes: JSON discovery + a PNG per game, both serving the
    /// synthetic slate regardless of any board config, and both surviving an
    /// id nobody has.
    /// Minutes since local midnight for a board, so a window can be built
    /// around "now" and the test means the same thing at any hour.
    fn local_minute(cfg: &BoardConfig) -> u16 {
        use chrono::Timelike;
        let now = match board_tz(cfg) {
            Some(tz) => chrono::Utc::now().with_timezone(&tz).time(),
            None => chrono::Local::now().time(),
        };
        (now.hour() * 60 + now.minute()) as u16
    }

    pub(crate) fn window_around_now(cfg: &BoardConfig) -> QuietHours {
        let m = local_minute(cfg);
        QuietHours { start_min: (m + 1439) % 1440, end_min: (m + 60) % 1440 }
    }

    /// Quiet hours must beat the NOTICE path too. Notices return
    /// `display_on: true` regardless of power, so a demo toggle or a power
    /// splash would otherwise light a dark bedroom at 3am.
    #[test]
    fn quiet_hours_outrank_games_and_notices_alike() {
        let state = AppState::new_ephemeral();
        let cfg = BoardConfig {
            quiet_hours: Some(window_around_now(&BoardConfig::default())),
            ..Default::default()
        };
        state.configs.lock().unwrap().insert("b".into(), cfg.clone());
        assert!(quiet_now(&state.config_for("b")), "precondition: inside the window");

        // Games are live, and a notice is pending — neither may wake the panel.
        set_notice(&state, "b", &[("HELLO", true, NOTICE_WHITE)], 30);
        let m = manifest_for_board(&state, "b");
        assert!(
            m.rotation.iter().all(|r| !r.scroll) && m.rotation.len() <= 1,
            "a quiet board shows only its off mode"
        );
        assert!(m.countdown_ms.is_none(), "no notice countdown while quiet");
    }

    /// A stored `quiet_allow_events` must NOT light the panel. The setting is
    /// gone from the app, so a board that still carries `true` from an older
    /// build would wake for a game with nothing on any screen able to explain
    /// it and nothing able to turn it off.
    #[test]
    fn a_leftover_wake_for_live_events_no_longer_lights_the_panel() {
        let state = AppState::new_ephemeral();
        let cfg = BoardConfig {
            quiet_hours: Some(window_around_now(&BoardConfig::default())),
            quiet_allow_events: true,
            power: true,
            ..Default::default()
        };
        state.configs.lock().unwrap().insert("b".into(), cfg);
        *state.games.lock().unwrap() = vec![sample_game_for("nhl", true, true)];
        let m = manifest_for_board(&state, "b");
        assert!(!m.display_on, "quiet hours mean dark, with no exception");
    }

    /// The knob inside a window means "show me anyway", NOT "sleep 24 hours" —
    /// `power` is still true in a window, so without its own arm the press
    /// lands in the showing-games branch and arms the sleep timer.
    #[test]
    fn the_knob_during_quiet_hours_overrides_it_rather_than_arming_sleep() {
        let state = AppState::new_ephemeral();
        let cfg = BoardConfig {
            quiet_hours: Some(window_around_now(&BoardConfig::default())),
            ..Default::default()
        };
        state.configs.lock().unwrap().insert("b".into(), cfg);
        handle_button(&state, "b");
        let after = state.config_for("b");
        assert!(after.quiet_override, "the press overrides the window");
        assert!(after.power, "and must not power the board off");
        assert!(after.wake_at.is_none(), "no 24-hour sleep timer");
        assert!(!quiet_now(&after), "the board is awake for the rest of the window");
    }

    /// Equal bounds mean DISABLED. A board dark around the clock is the worst
    /// outcome this feature has available.
    #[test]
    fn a_degenerate_window_never_darkens_the_board() {
        let state = AppState::new_ephemeral();
        let m = local_minute(&BoardConfig::default());
        let cfg = BoardConfig {
            quiet_hours: Some(QuietHours { start_min: m, end_min: m }),
            ..Default::default()
        };
        state.configs.lock().unwrap().insert("b".into(), cfg);
        assert!(!quiet_now(&state.config_for("b")));
    }

    /// Boards configured before the four weather layouts became one mode must
    /// keep the layout they chose.
    #[test]
    fn an_old_weather_id_migrates_to_the_variant_it_named() {
        let mut configs = HashMap::new();
        configs.insert(
            "b".to_string(),
            BoardConfig { off_mode: Some("weather-forecast".into()), ..Default::default() },
        );
        sanitize_off_modes(&mut configs);
        let cfg = &configs["b"];
        assert_eq!(cfg.off_mode.as_deref(), Some("weather"));
        assert_eq!(cfg.off_mode_params.get("variant").map(String::as_str), Some("forecast"));
        assert_eq!(cfg.off_mode_id(), "weather");
    }

    /// Health must report a STALLED loop, not just a live process. The outage
    /// that motivated this was a wedged loop inside a process that was still
    /// running and still accepting connections.
    #[test]
    fn health_reports_a_stalled_loop_rather_than_just_a_live_process() {
        let state = AppState::new_ephemeral();
        // Nothing has beaten yet: starting up is not stalled.
        let (ok, _) = health_report(&state);
        assert!(ok, "a server that has not run a pass yet is not unhealthy");

        beat(&state, "poll");
        beat(&state, "tick");
        beat(&state, "weather");
        let (ok, report) = health_report(&state);
        assert!(ok);
        assert_eq!(report["loops"]["poll"]["stale"], false);
        assert_eq!(report["loops"]["poll"]["age_secs"], 0);

        // Wind one loop's beat back past its limit.
        let limit = report["loops"]["tick"]["limit_secs"].as_u64().unwrap();
        state.beats.lock().unwrap().insert(
            "tick",
            Instant::now() - std::time::Duration::from_secs(limit + 1),
        );
        let (ok, report) = health_report(&state);
        assert!(!ok, "a stalled loop must make the server report unhealthy");
        assert_eq!(report["loops"]["tick"]["stale"], true);
        assert_eq!(report["loops"]["poll"]["stale"], false, "and name WHICH loop");
    }

    /// Migration must preserve every already-configured board's behaviour
    /// exactly. This walks every legacy option the old UI could produce and
    /// asserts the expanded set claims the same games, at the same priority.
    #[test]
    fn migrating_a_legacy_option_keeps_the_same_games() {
        let mk = |sport: &str, tier: Option<&str>, session: Option<&str>| Game {
            id: "g".into(),
            sport_id: sport.into(),
            away: TeamSide::empty("X"),
            home: TeamSide::empty(""),
            status: GameStatus::Active,
            ordinal: String::new(),
            clock: String::new(),
            start_time: String::new(),
            end_time: None,
            leaderboard: None,
            champ: tier.map(|t| model::Champ { tier: t.into(), label: t.into() }),
            series: None,
            session: session.map(Into::into),
            situation: None,
            pregame: None,
        };
        // (sport, legacy option, tiers/sessions it USED to claim)
        let cases: &[(&str, &str, &[&str])] = &[
            ("nhl", "final", &["final"]),
            ("nhl", "conf", &["conf", "final"]),
            ("nhl", "playoffs", &["final", "conf", "playoffs"]),
            ("nba", "playoffs", &["final", "playoffs"]),
            ("mlb", "ws", &["ws"]),
            ("nfl", "sb", &["sb"]),
            ("cfb", "bowl", &["bowl"]),
            ("cbb", "tournament", &["tournament"]),
            ("mls", "cup", &["cup"]),
            ("wc", "semi", &["semi", "final"]),
            ("wc", "knockout", &["knockout", "quarter", "semi", "final"]),
        ];
        for (sport, legacy, claimed) in cases {
            let mut configs = HashMap::new();
            configs.insert(
                "b".to_string(),
                BoardConfig {
                    sports: vec![SportConfig {
                        sport_id: (*sport).into(),
                        championship_option_id: Some((*legacy).to_string()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            );
            migrate_champ_options(&mut configs);
            let cfg = &configs["b"];
            for tier in *claimed {
                assert_eq!(
                    game_option_priority(&mk(sport, Some(tier), None), cfg),
                    Some(2),
                    "{sport}/{legacy} used to claim tier {tier} and must still"
                );
            }
            // And a board that had chosen nothing stays unconfigured.
            assert!(!cfg.sports[0].championship_option_ids.is_empty());
        }
    }

    /// Running the migration twice must equal running it once — the projection
    /// is None exactly when the set is empty, which is what makes that true.
    #[test]
    fn migration_is_idempotent() {
        let build = || {
            let mut c = HashMap::new();
            c.insert(
                "b".to_string(),
                BoardConfig {
                    sports: vec![SportConfig {
                        sport_id: "nhl".into(),
                        championship_option_id: Some("conf".into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            );
            c
        };
        let mut once = build();
        migrate_champ_options(&mut once);
        let mut twice = build();
        migrate_champ_options(&mut twice);
        migrate_champ_options(&mut twice);
        assert_eq!(once["b"].sports[0].championship_option_ids, twice["b"].sports[0].championship_option_ids);
        assert_eq!(once["b"].sports[0].championship_option_id, twice["b"].sports[0].championship_option_id);
    }

    /// Every offered round must be a tier the classifier can actually produce,
    /// or the checkbox is a control that does nothing.
    #[test]
    fn every_round_is_reachable_or_is_the_blanket() {
        // EVERY league this binary knows, not a hand-kept subset — the seasonal
        // ones were the leagues whose options were broken, and a subset is how
        // that went unnoticed. (Reachability itself is proven against real feed
        // strings by `every_offered_round_is_proven_by_a_real_feed_string`.)
        for (sport, ..) in crate::espn::LEAGUE_IDENTITY {
            let rounds = championship_rounds_for(sport);
            assert!(!rounds.is_empty(), "{sport} offers nothing");
            for r in &rounds {
                assert!(!r.label.is_empty(), "{sport}/{} has no label", r.id);
                // Labels are standalone now: no "Selected + " prefix, because
                // each row is one round rather than a cumulative ladder.
                assert!(!r.label.contains("Selected"), "{sport}/{}: {:?}", r.id, r.label);
            }
            assert!(rounds.iter().any(|r| r.id == "all") || *sport == "f1", "{sport} needs a blanket");
        }
    }

    /// The single most important compatibility case: an ALREADY-INSTALLED app
    /// sends only the singular field on every favourites edit. Treating that as
    /// an empty set would silently wipe the user's rounds.
    #[test]
    fn an_old_app_editing_favorites_cannot_wipe_the_round_set() {
        let state = AppState::new_ephemeral();
        state.configs.lock().unwrap().insert("b".into(), BoardConfig::default());
        // New app picks two rounds.
        crate::ws::apply_command(
            &state,
            "b",
            Command::SetSportConfig {
                sport_id: "nhl".into(),
                favorite_team_ids: vec![],
                championship_option_id: None,
                championship_option_ids: Some(vec!["conf".into(), "final".into()]),
            },
        );
        assert_eq!(state.config_for("b").sports[0].championship_option_ids.len(), 2);

        // Old app edits favourites, sending ONLY the legacy field.
        crate::ws::apply_command(
            &state,
            "b",
            Command::SetSportConfig {
                sport_id: "nhl".into(),
                favorite_team_ids: vec!["nhl-bos".into()],
                championship_option_id: Some("conf".into()),
                championship_option_ids: None,
            },
        );
        let after = state.config_for("b");
        assert_eq!(
            after.sports[0].championship_option_ids,
            vec!["conf".to_string(), "final".to_string()],
            "an old app's single option must expand, not blank the set"
        );

        // A NEW app sending an explicit empty list DOES deselect everything.
        crate::ws::apply_command(
            &state,
            "b",
            Command::SetSportConfig {
                sport_id: "nhl".into(),
                favorite_team_ids: vec![],
                championship_option_id: None,
                championship_option_ids: Some(vec![]),
            },
        );
        assert!(state.config_for("b").sports[0].championship_option_ids.is_empty());
    }

    /// A deadlock, caught the only way a deadlock can be: by timing out.
    ///
    /// This took the live server down. Nothing crashed and nothing was logged;
    /// the process simply stopped answering, because the thread was waiting on
    /// a lock it already held while every handler queued behind it.
    #[test]
    fn pushing_to_every_connected_board_cannot_deadlock() {
        let state = AppState::new_ephemeral();
        // Two registered devices, so the loop body actually runs more than once.
        for id in ["board-a", "board-b"] {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            state.devices.lock().unwrap().insert(id.to_string(), tx);
            state.configs.lock().unwrap().insert(id.to_string(), BoardConfig::default());
        }
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            push_all_connected(&state);
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "push_all_connected deadlocked — it is holding `devices` while push_board relocks it"
        );
    }

    /// The regression that shipped: a catalog restored from the disk cache
    /// advertised NO off modes, so every app fell back to the two ids it
    /// hardcodes and the real modes were invisible. The cache file predates the
    /// field, `#[serde(default)]` made that silent, and the fetch path — the
    /// one that DID set them — was skipped because ESPN was erroring.
    ///
    /// Deserialize a cache-shaped blob with no `off_modes`, then assert the
    /// assembly step puts this binary's modes back.
    #[test]
    fn a_cached_catalog_still_advertises_this_binarys_off_modes() {
        let cached: Catalog =
            serde_json::from_str(r#"{"sports":[{"id":"nhl","name":"NHL","teams":[],"championship_options":[]}]}"#)
                .unwrap();
        assert!(cached.off_modes.is_empty(), "precondition: the cache carries none");

        let served = with_binary_owned(cached);
        assert_eq!(
            served.off_modes.len(),
            offmode::off_modes().len(),
            "the served catalog must offer every mode this binary renders"
        );
        for m in offmode::off_modes() {
            assert!(served.off_modes.iter().any(|d| d.id == m.id), "missing {}", m.id);
        }
    }

    /// The same trap, for the two fields the league reorganisation added.
    ///
    /// The blob below is shaped like a REAL cache file written by an older
    /// binary: no `category`, no `category_rank`, and a `name` from before the
    /// renames. On Railway the cache is a normal path, not an edge case — an
    /// ESPN outage at startup serves it verbatim — so a rename that isn't
    /// re-stamped here keeps shipping last month's names, and an unstamped
    /// category leaves the app with nothing to group by.
    #[test]
    fn a_cached_catalog_is_restamped_with_this_binarys_names_and_families() {
        let cached: Catalog = serde_json::from_str(
            r#"{"sports":[
                 {"id":"cbb","name":"Men's College Basketball","teams":[],"championship_options":[]},
                 {"id":"olyhm","name":"Olympic Hockey (M)","teams":[],"championship_options":[]},
                 {"id":"khl","name":"A Fork's League","teams":[],"championship_options":[]}
               ]}"#,
        )
        .unwrap();
        assert_eq!(cached.sports[0].category, "", "precondition: the cache carries no family");
        assert_eq!(cached.sports[0].category_rank, 0);

        let served = with_binary_owned(cached);
        let by_id = |id: &str| served.sports.iter().find(|s| s.id == id).expect(id).clone();

        let cbb = by_id("cbb");
        assert_eq!(cbb.name, "NCAAM", "the stale cached name must not survive");
        assert_eq!(cbb.category, "Basketball");
        assert!(cbb.category_rank >= 1, "real ranks are 1-based so serde's 0 can mean 'unranked'");

        let oly = by_id("olyhm");
        assert_eq!(oly.name, "Men's Olympic");
        assert_eq!(oly.category, "Hockey");

        // A league this binary has never heard of keeps its name and stays
        // unranked rather than being dropped or renamed to its id.
        let fork = by_id("khl");
        assert_eq!(fork.name, "A Fork's League");
        assert_eq!(fork.category, "");
        assert_eq!(fork.category_rank, 0);
    }

    /// Two leagues may share a display name (three share "Men's Olympic"), but
    /// never two in the same section — that is the only place they are listed
    /// together, and the app has nothing else to tell them apart by.
    #[test]
    fn league_names_are_unique_within_their_family() {
        let mut seen: std::collections::HashSet<(&str, &str)> = std::collections::HashSet::new();
        let mut ranks: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for (id, name, category, rank) in crate::espn::LEAGUE_IDENTITY {
            assert!(!name.is_empty(), "{id} has no name");
            assert!(!category.is_empty(), "{id} has no family");
            assert!(*rank >= 1, "{id}: ranks start at 1 so 0 can mean 'old server'");
            assert!(seen.insert((category, name)), "{id}: {name:?} is listed twice under {category:?}");
            // Ranks order leagues within a sport, so a tie would leave two rows
            // in an order that depends on the name and nothing else.
            assert!(ranks.insert(*rank), "{id}: rank {rank} is used twice");
        }
    }

    /// A sport's sections must not interleave: every league in one family has to
    /// rank before every league in the next, or the app — which ranks a section
    /// by its best-ranked league — would put a league under the wrong heading's
    /// worth of ordering.
    #[test]
    fn each_familys_ranks_form_one_contiguous_block() {
        let mut order: Vec<(&str, u32)> = crate::espn::LEAGUE_IDENTITY
            .iter()
            .map(|(_, _, category, rank)| (*category, *rank))
            .collect();
        order.sort_by_key(|(_, rank)| *rank);
        let mut seen_families: Vec<&str> = Vec::new();
        for (family, rank) in order {
            if seen_families.last() != Some(&family) {
                assert!(
                    !seen_families.contains(&family),
                    "{family} reappears at rank {rank} after another family — its ranks are not contiguous"
                );
                seen_families.push(family);
            }
        }
    }

    /// Nothing may construct a Catalog with off modes of its own — that is what
    /// let the bug above hide, because the mock set them and the cache didn't.
    #[test]
    fn catalog_construction_leaves_off_modes_to_the_assembly_step() {
        assert!(
            mock_catalog().off_modes.is_empty(),
            "mock_catalog must not pre-fill off modes, or tests stop exercising the assembly"
        );
    }

    /// The preview route the app's off-mode rows fetch.
    #[tokio::test]
    async fn offmode_preview_route_serves_declared_modes_only() {
        let app = Router::new().route("/offmode/:id", get(get_offmode));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let resp = reqwest::get(format!("http://{addr}/offmode/clock.png")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get(reqwest::header::CONTENT_TYPE).unwrap(), "image/png");
        let first = resp.bytes().await.unwrap();
        assert_eq!(&first[..8], b"\x89PNG\r\n\x1a\n");

        // Byte-stable: the app caches these for a day.
        let again = reqwest::get(format!("http://{addr}/offmode/clock.png")).await.unwrap();
        assert_eq!(again.bytes().await.unwrap(), first, "preview must not drift with the minute");

        // A mode that draws nothing declares no preview, and 404s if asked.
        assert_eq!(reqwest::get(format!("http://{addr}/offmode/blank.png")).await.unwrap().status(), 404);
        // An id this server does not offer 404s rather than showing something else.
        assert_eq!(reqwest::get(format!("http://{addr}/offmode/stocks")).await.unwrap().status(), 404);
    }

    /// A board parked on a mode this binary does not offer still lights up —
    /// and the manifest never carries anything but the closed legacy enum.
    #[test]
    fn a_forked_off_mode_never_reaches_the_board_as_an_unknown_value() {
        let state = AppState::new_ephemeral();
        state.games.lock().unwrap().clear(); // idle -> off-mode path
        let mut cfg = BoardConfig::default();
        cfg.set_off_mode("stocks", false); // a mode this server does not have
        state.configs.lock().unwrap().insert("b".into(), cfg);
        let m = manifest_for_board(&state, "b");
        assert!(!m.rotation.is_empty(), "an unknown mode falls back to the clock, not to darkness");
        assert_eq!(m.off_state, OffState::Clock);
        assert!(m.display_on);
    }

    #[tokio::test]
    async fn demo_routes_serve_the_synthetic_slate() {
        let state = AppState::new_ephemeral();
        *state.demo.lock().unwrap() = espn::demo_games(0, 0);
        let known = state.demo.lock().unwrap()[0].id.clone();
        let app = Router::new()
            .route("/demo/games", get(get_demo_games))
            .route("/demo/render/:id", get(get_demo_render))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let resp = reqwest::get(format!("http://{addr}/demo/games")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get(reqwest::header::CACHE_CONTROL).unwrap(), "no-store");
        // `Game` is serialize-only, so assert against the wire JSON directly.
        let games: Vec<serde_json::Value> = resp.json().await.unwrap();
        assert!(!games.is_empty(), "the slate must not be empty");
        assert!(
            games.iter().all(|g| g["id"].as_str().is_some_and(|id| id.starts_with("demo-"))),
            "every id is a demo id"
        );

        // A real demo game renders a PNG.
        let resp = reqwest::get(format!("http://{addr}/demo/render/{known}.png")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get(reqwest::header::CONTENT_TYPE).unwrap(), "image/png");
        let png = resp.bytes().await.unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "actually a PNG");

        // An unknown id gets a VISIBLE frame, not a 404 — a 404 renders as a
        // black rectangle in the app, indistinguishable from a black frame.
        let resp = reqwest::get(format!("http://{addr}/demo/render/nope")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get(reqwest::header::CONTENT_TYPE).unwrap(), "image/png");
    }

    /// conflict would panic at Router build).
    #[tokio::test]
    async fn firmware_notes_route_serves_and_rejects_traversal() {
        let base = std::env::temp_dir().join(format!("scoreboard-fw-notes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("notes")).unwrap();
        let body = "- Sharper dark pixels.\n- Factory reset now erases Wi-Fi details.\n";
        std::fs::write(base.join("notes").join("2026.7.5.txt"), body).unwrap();
        // Bait OUTSIDE the notes dir: reachable only by traversal.
        std::fs::write(base.join("evil.txt"), "secret").unwrap();

        let mut state = AppState::new_ephemeral();
        state.fw_notes_dir = base.join("notes");
        let handler_state = state.clone();
        let app = Router::new()
            .route("/firmware/:chan/latest", get(get_firmware_latest))
            .route("/firmware/:file", get(get_firmware_bin))
            .route("/firmware/notes/:version", get(get_firmware_notes))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let get_notes = |version: &str| {
            let url = format!("http://{addr}/firmware/notes/{version}");
            async move { reqwest::get(url).await.unwrap() }
        };

        // A released version with notes: exact body, contract content-type.
        let resp = get_notes("2026.7.5").await;
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
            "text/plain; charset=utf-8"
        );
        assert_eq!(resp.text().await.unwrap(), body);

        // A valid-shaped version nobody released: plain 404.
        assert_eq!(get_notes("2026.7.999").await.status(), 404);

        // Traversal via percent-encoded slashes: the url crate keeps "%2f" as
        // one segment, axum decodes it to "/", and the digits-and-dots check
        // 404s the result without touching the filesystem — the bait file one
        // level up must stay unreachable.
        for hostile in ["..%2Fevil", "..%2fevil", "2026.7.5%2f..%2f..%2fevil", "..%2Fevil.txt"] {
            let resp = get_notes(hostile).await;
            assert_eq!(resp.status(), 404, "hostile version {hostile:?} must 404");
            assert!(
                !resp.text().await.unwrap().contains("secret"),
                "traversal {hostile:?} must not read outside notes/"
            );
        }
        // Junk that isn't strictly [0-9.] is 404 too, per the contract.
        for junk in ["latest", "2026.7.5.txt", "v2026.7.5", "2026.7.5%00"] {
            assert_eq!(get_notes(junk).await.status(), 404, "junk version {junk:?}");
        }

        // Belt and braces below the router: a literal "../evil" handed
        // straight to the handler (no client-side url normalization in the
        // way) is rejected by validation, not by a failed filesystem lookup.
        for hostile in ["../evil", "..", "", "/etc/passwd", "2026.7.5/../../evil"] {
            let resp = get_firmware_notes(
                State(handler_state.clone()),
                axum::extract::Path(hostile.to_string()),
            )
            .await;
            assert_eq!(
                resp.status(),
                axum::http::StatusCode::NOT_FOUND,
                "handler must reject {hostile:?}"
            );
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    /// GET /guide/{name}.png: every troubleshooting screen serves a 200 PNG
    /// with the day-long cache header, the four bodies are pairwise DISTINCT
    /// (a drive-the-state bug — e.g. never leaving Booting, or wifi stuck one
    /// way — collapses two screens into the same image), and an unknown name
    /// is a plain 404.
    #[tokio::test]
    async fn guide_routes_serve_distinct_board_screens() {
        let app = Router::new().route("/guide/:name", get(get_guide));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let names = ["joining-wifi", "finding-server", "setup", "pair-code", "game", "black"];
        let mut bodies = Vec::new();
        for name in names {
            let resp = reqwest::get(format!("http://{addr}/guide/{name}.png")).await.unwrap();
            assert_eq!(resp.status(), 200, "guide screen {name:?}");
            assert_eq!(
                resp.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
                "image/png",
                "guide screen {name:?}"
            );
            // The contract the app relies on: these are static per release, so
            // clients may cache them for a day.
            assert_eq!(
                resp.headers().get(reqwest::header::CACHE_CONTROL).unwrap(),
                "public, max-age=86400",
                "guide screen {name:?}"
            );
            let body = resp.bytes().await.unwrap();
            assert!(
                body.starts_with(&[0x89, b'P', b'N', b'G']),
                "guide screen {name:?} must be a real PNG"
            );
            bodies.push((name, body));
        }
        for i in 0..bodies.len() {
            for j in (i + 1)..bodies.len() {
                assert_ne!(
                    bodies[i].1, bodies[j].1,
                    "guide screens {:?} and {:?} rendered identically — a state was mis-driven",
                    bodies[i].0, bodies[j].0
                );
            }
        }

        // Same screen twice -> byte-identical (the fixed tick makes the
        // animation phase deterministic; without it caching would be a lie).
        let again = reqwest::get(format!("http://{addr}/guide/setup.png"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(again, bodies[2].1, "guide screens must render deterministically");

        // Unknown names are plain 404s — including near-misses.
        for unknown in ["rebooting", "pair-code2", "", "joining-wifi.jpg"] {
            let resp = reqwest::get(format!("http://{addr}/guide/{unknown}.png")).await.unwrap();
            assert_eq!(resp.status(), 404, "unknown guide name {unknown:?}");
        }
    }
}

#[cfg(test)]
mod screens_tests;

#[cfg(test)]
mod sample_preview_tests {
    use super::*;

    /// Not an assertion — the football bar in its three knowledge states.
    /// `cargo test -p server dump_football_states -- --ignored`
    #[test]
    #[ignore]
    fn dump_football_states() {
        let dir = std::env::var("PREVIEW_DIR").unwrap_or_else(|_| "/tmp/previews".into());
        std::fs::create_dir_all(&dir).unwrap();
        let mut g = sample_game_for("nfl", true, true);
        let f = render::render_frame(&g, None, None, NameStyle::Name).1;
        save_png(&format!("{dir}/fb-full.png"), &f, 8);
        if let Some(model::Situation::Football { home_possession, .. }) = &mut g.situation {
            *home_possession = None;
        }
        let f = render::render_frame(&g, None, None, NameStyle::Name).1;
        save_png(&format!("{dir}/fb-no-possession.png"), &f, 8);
        if let Some(model::Situation::Football { ball_position, .. }) = &mut g.situation {
            *ball_position = String::new();
        }
        let f = render::render_frame(&g, None, None, NameStyle::Name).1;
        save_png(&format!("{dir}/fb-no-ball.png"), &f, 8);
    }

    /// Not an assertion — a way to LOOK at every league's fixture.
    /// `cargo test -p server dump_league_previews -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dump_league_previews() {
        let dir = std::env::var("PREVIEW_DIR").unwrap_or_else(|_| "/tmp/previews".into());
        std::fs::create_dir_all(&dir).unwrap();
        for (id, ..) in espn::LEAGUE_IDENTITY {
            for (tag, extras, live) in [("live", true, true), ("final", true, false)] {
                let g = sample_game_for(id, extras, live);
                let f = render::render_frame(&g, None, None, NameStyle::Name).1;
                save_png(&format!("{dir}/{id}-{tag}.png"), &f, 8);
            }
        }
        println!("wrote {dir}");
    }

    /// Every league previews with ITS OWN teams. The fixture used to be keyed
    /// by sport family, so the NCAA football settings screen demonstrated
    /// itself with the Packers and the Bears, and MLS with Arsenal.
    #[test]
    fn each_league_previews_with_teams_from_that_league() {
        // Leagues that share a family and must not share a fixture.
        for (a, b) in [
            ("nfl", "cfb"),
            ("mlb", "cbase"),
            ("nba", "cbb"),
            ("nba", "wnba"),
            ("cbb", "wcbb"),
            ("mls", "epl"),
            ("epl", "ucl"),
            // Golf and F1 are leaderboards with no team sides — the title
            // assertion below is what covers those.
        ] {
            let ga = sample_game_for(a, false, true);
            let gb = sample_game_for(b, false, true);
            assert_ne!(
                (ga.away.abbrev.clone(), ga.home.abbrev.clone()),
                (gb.away.abbrev.clone(), gb.home.abbrev.clone()),
                "{a} and {b} preview with the same teams"
            );
        }
        // ...and the leaderboard leagues do not share an event.
        let title = |id: &str| sample_game_for(id, false, true).leaderboard.unwrap().name;
        assert_ne!(title("pga"), title("lpga"), "LPGA previews a PGA event");
        assert_ne!(title("pga"), title("f1"));
    }

    /// A football preview has to show the part of a football frame that is
    /// football: down, distance and field position. It was a Final with no
    /// situation, so both previews were a quarter and two scores.
    #[test]
    fn football_previews_show_the_down_and_the_ball() {
        for id in ["nfl", "cfb"] {
            let g = sample_game_for(id, true, true);
            assert_eq!(g.status, GameStatus::Active, "{id}");
            match g.situation {
                Some(model::Situation::Football { ref down_string, ref home_possession, .. }) => {
                    assert!(!down_string.is_empty(), "{id} has no down string");
                    assert!(home_possession.is_some(), "{id} has no possession");
                }
                other => panic!("{id} previews with {other:?}, not a football situation"),
            }
            // And it must reach the panel: a situation the renderer ignores is
            // the same bug one step later.
            let with = render::render_frame(&g, None, None, NameStyle::Name).1;
            let mut bare = g.clone();
            bare.situation = None;
            let without = render::render_frame(&bare, None, None, NameStyle::Name).1;
            assert_ne!(with, without, "{id}: the football situation draws nothing");
        }
    }

    /// The previews must actually DIFFER, or the settings row is showing the
    /// user two identical pictures and asking them to choose.
    #[test]
    fn every_preview_pair_renders_a_different_frame() {
        let frame = |sport: &str, extras: bool, style: NameStyle, spoiler: Option<SpoilerMode>| {
            let g = sample_game_for(sport, extras, spoiler.is_some());
            render::render_frame(&g, spoiler, None, style).1
        };
        // Extras on vs off, for each league family that draws them.
        for sport in ["mlb", "nhl", "ucl"] {
            let on = frame(sport, true, default_name_style(sport), None);
            let off = frame(sport, false, default_name_style(sport), None);
            assert_ne!(on, off, "{sport}: the extras preview shows no difference");
        }
        // A league with no extras must render identically either way — the app
        // shows no switch there, and a difference would mean it should.
        for sport in ["nfl", "nba"] {
            let on = frame(sport, true, default_name_style(sport), None);
            let off = frame(sport, false, default_name_style(sport), None);
            assert_eq!(on, off, "{sport} draws extras but declares no label");
        }
        // The three name styles, per league.
        for sport in ["mlb", "nhl", "nba", "nfl"] {
            let loc = frame(sport, true, NameStyle::Location, None);
            let nam = frame(sport, true, NameStyle::Name, None);
            let abb = frame(sport, true, NameStyle::Abbrev, None);
            assert_ne!(loc, nam, "{sport}: Location and Name look the same");
            assert_ne!(nam, abb, "{sport}: Name and Abbrev look the same");
            assert_ne!(loc, abb, "{sport}: Location and Abbrev look the same");
        }
        // Golf's round switch, and that a leaderboard league previews its
        // spoiler as a LEADERBOARD rather than as somebody else's game.
        let golf = sample_game_for("pga", true, false);
        assert!(golf.leaderboard.is_some(), "golf must preview a leaderboard");
        assert_eq!(golf.ordinal, "R2", "the round is what Show round switches");
        let mut no_round = golf.clone();
        no_round.ordinal.clear();
        let style = default_name_style("pga");
        assert_ne!(
            render::render_frame(&golf, None, None, style).1,
            render::render_frame(&no_round, None, None, style).1,
            "the Show round preview shows no difference",
        );
        for sport in ["pga", "lpga", "f1"] {
            let g = sample_game_for(sport, true, true);
            assert!(g.leaderboard.is_some(), "{sport} must preview a leaderboard");
            let st = default_name_style(sport);
            let plain = render::render_frame(&g, None, None, st).1;
            let normal = render::render_frame(&g, Some(SpoilerMode::Normal), None, st).1;
            let strict = render::render_frame(&g, Some(SpoilerMode::Strict), None, st).1;
            assert_ne!(plain, normal, "{sport}: hiding results shows no difference");
            assert_ne!(normal, strict, "{sport}: strict looks like normal");
        }

        // Spoiler modes, per league.
        for sport in ["mlb", "nhl", "nba"] {
            let style = default_name_style(sport);
            let plain = frame(sport, true, style, None);
            let normal = frame(sport, true, style, Some(SpoilerMode::Normal));
            let strict = frame(sport, true, style, Some(SpoilerMode::Strict));
            assert_ne!(plain, normal, "{sport}: normal spoiler shows no difference");
            assert_ne!(normal, strict, "{sport}: strict looks like normal");
        }
    }

    /// Every league gets a sample with a location, or the Location preview
    /// draws a blank row and reads as a bug rather than a choice.
    #[test]
    fn every_sample_league_has_locations() {
        for (id, ..) in espn::LEAGUE_IDENTITY {
            // Leaderboard leagues have no teams — their frame is a field of
            // individuals — which is also why the app offers them no name-style
            // picker, so there is no Location preview to render.
            if matches!(*id, "pga" | "lpga" | "f1") {
                assert!(sample_game_for(id, true, false).leaderboard.is_some(), "{id}");
                continue;
            }
            let g = sample_game_for(id, true, false);
            assert!(!g.away.location.is_empty(), "{id} away has no location");
            assert!(!g.home.location.is_empty(), "{id} home has no location");
            assert!(!g.away.name.is_empty() && !g.away.abbrev.is_empty(), "{id}");
        }
    }
}


