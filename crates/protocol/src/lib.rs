//! Shared wire protocol + data model for the scoreboard system.
//!
//! Used by the server, the board firmware/simulator, and (later, via uniffi)
//! the app core. This is the single source of truth for messages and types.
//! `no_std + alloc` so the ESP32 firmware can depend on it.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ---- display geometry ----
pub const FRAME_WIDTH: usize = 64;
pub const FRAME_HEIGHT: usize = 32;
pub const FRAME_PIXELS: usize = FRAME_WIDTH * FRAME_HEIGHT;
/// Frames travel as raw little-endian RGB565 (2 bytes/pixel).
pub const FRAME_BYTES: usize = FRAME_PIXELS * 2;

/// Content address of a frame = hex sha256 of its RGB565 bytes.
pub type FrameId = String;

pub fn frame_id(rgb565_le: &[u8]) -> FrameId {
    let mut h = Sha256::new();
    h.update(rgb565_le);
    hex::encode(h.finalize())
}

// ---- pixel helpers ----
pub fn rgb888_to_565(r: u8, g: u8, b: u8) -> u16 {
    (((r as u16) & 0xF8) << 8) | (((g as u16) & 0xFC) << 3) | ((b as u16) >> 3)
}

pub fn rgb565_to_888(c: u16) -> (u8, u8, u8) {
    let r = ((c >> 11) & 0x1F) as u8;
    let g = ((c >> 5) & 0x3F) as u8;
    let b = (c & 0x1F) as u8;
    ((r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2))
}

// ---- catalog / data model (served over HTTP) ----
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Team {
    pub id: String,
    pub league_id: String,
    pub name: String,
    pub abbrev: String,
    /// Primary team color as a 6-digit hex string (no `#`), when known. `None`
    /// for mock/leaderboard "teams" (golf tours, F1) that have no brand color.
    #[serde(default)]
    pub color: Option<String>,
    /// Secondary/alternate team color (hex), when known.
    #[serde(default)]
    pub alt_color: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChampionshipOption {
    pub id: String,
    pub label: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SportDescriptor {
    pub id: String,
    pub name: String,
    pub teams: Vec<Team>,
    pub championship_options: Vec<ChampionshipOption>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Catalog {
    pub sports: Vec<SportDescriptor>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GameSummary {
    pub id: String,
    pub sport_id: String,
    pub away: String,
    pub home: String,
    pub away_score: u32,
    pub home_score: u32,
    pub status: String, // "live" | "upcoming" | "final"
    pub detail: String, // e.g. "P2 12:34" or "7:00 PM"
    /// Normalized RFC3339 UTC start time ("2026-06-28T13:00:00Z"); None when
    /// the feed's timestamp was absent or unparseable. NOT a spoiler — even
    /// strict spoiler mode shows the matchup + time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    /// Normalized RFC3339 UTC end time. Present mainly for multi-day events
    /// (golf tournaments, F1 race weekends) whose feed carries an endDate;
    /// None for ordinary games. Not a spoiler either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time: Option<String>,
    /// Content hash of this game's rendered frame ("" if not rendered). The frame
    /// is pushed as a binary WS message, so cards update without HTTP polling.
    #[serde(default)]
    pub frame_id: FrameId,
    /// True when this game QUALIFIES for the board but lost its rotation slot to
    /// the cap — the app shows it grayed with an offer to add it manually
    /// (manual adds take top priority).
    #[serde(default)]
    pub bumped: bool,
    /// Championship tier label when this is a postseason/championship game
    /// ("Stanley Cup Final", "Playoffs"); None for regular-season games.
    /// Drives the app's big-game caption on game cards.
    #[serde(default)]
    pub champ: Option<String>,
    /// True when this summary was sanitized by spoiler protection: scores are
    /// zeroed and `detail` carries only spoiler-safe status.
    #[serde(default)]
    pub spoiled: bool,
    /// True when this game's BOARD rendering scrolls (a leaderboard field
    /// deeper than the static 4 rows, at an effective league depth > 4) —
    /// the app offers its per-game scroll toggle only for these.
    #[serde(default)]
    pub scrolls: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Schedule {
    pub live: Vec<GameSummary>,
    pub upcoming: Vec<GameSummary>,
}

// ---- display plan ----
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum OffState {
    Blank,
    #[default]
    Clock,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// How spoiler-protected games render on the board and in app summaries.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SpoilerMode {
    /// Matchup + start time only; a FINAL marker once the game ends.
    Strict,
    /// Adds basic progress (period / quarter / lap count) — still no scores.
    #[default]
    Normal,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RotationEntry {
    pub frame_id: FrameId,
    pub dwell_ms: u32,
    /// Frames taller than the panel scroll vertically, synced to the dwell in
    /// fifths: hold at the top, glide down, hold at the bottom, glide back,
    /// hold at the top — one full pass per dwell, ending at rest so the
    /// rotation advance never cuts a scroll mid-glide. (At the 10s default:
    /// 2s per phase.) Omitted when false, so static manifests read identically.
    #[serde(default, skip_serializing_if = "is_false")]
    pub scroll: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Manifest {
    pub rotation: Vec<RotationEntry>,
    pub display_on: bool,
    pub off_state: OffState,
    pub brightness: u8, // 10..=100, applied device-side (one value: knob and app share it)
    /// Program identity. The board keeps its rotation slot across same-length
    /// refreshes (content-addressed ids change every score tick) — but that
    /// heuristic can't tell a 4-game rotation from a 4-step notice sweep. A
    /// changed epoch says "different program: start from the top". 0 = the
    /// games rotation; notices mint a fresh nonzero value per showing.
    #[serde(default)]
    pub epoch: u64,
}

// ---- per-board config ----
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct SportConfig {
    pub sport_id: String,
    pub favorite_team_ids: Vec<String>,
    pub championship_option_id: Option<String>,
    /// Per-league "time per event" override (seconds); None = the board's
    /// default dwell. Lets F1 scroll for 30s while baseball flashes by in 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dwell_secs: Option<u32>,
    /// Leaderboard depth for golf/F1 (rows shown on the board): 4 = static
    /// top-4 (no scrolling), up to 20 = the full field. None = default (10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaderboard_rows: Option<u32>,
    /// Spoiler-protect every game in this league.
    #[serde(default, skip_serializing_if = "is_false")]
    pub spoiler: bool,
    /// Spoiler-protect games involving these teams (team ids).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spoiler_team_ids: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BoardConfig {
    pub brightness: u8,
    pub power: bool,
    pub sports: Vec<SportConfig>,
    /// Games manually added to the rotation (beyond favorites), by game id.
    #[serde(default)]
    pub extra_game_ids: Vec<String>,
    /// Games one-off removed from the rotation (a favorite/big-game entry the user
    /// swiped away for now) — the underlying preferences stay intact.
    #[serde(default)]
    pub excluded_game_ids: Vec<String>,
    /// If set, the board shows ONLY this game (overrides the rotation).
    #[serde(default)]
    pub focused_game_id: Option<String>,
    /// User-given display name for this board (set via `Command::Rename`), shown in
    /// the app's board switcher. `None` until named.
    #[serde(default)]
    pub name: Option<String>,
    /// What the panel shows while the display is off / idle (server-rendered).
    #[serde(default)]
    pub off_state: OffState,
    /// How long a button-press sleep lasts before the board wakes itself.
    #[serde(default = "default_sleep_hours")]
    pub sleep_hours: u32,
    /// Seconds each game holds the panel before the rotation advances.
    #[serde(default = "default_dwell_secs")]
    pub dwell_secs: u32,
    /// If sleeping via the button: unix seconds when the server turns power back on.
    #[serde(default)]
    pub wake_at: Option<u64>,
    /// How spoiler-protected games render (strict = matchup + time only).
    #[serde(default)]
    pub spoiler_mode: SpoilerMode,
    /// Spoiler-protect EVERY game on this board.
    #[serde(default)]
    pub spoiler_all: bool,
    /// One-off spoiler-protected games (the app's "Hide spoilers" button).
    #[serde(default)]
    pub spoiler_game_ids: Vec<String>,
    /// One-off revealed games — exceptions that beat every protection rule
    /// (the app's "Reveal" button on a protected game).
    #[serde(default)]
    pub revealed_game_ids: Vec<String>,
    /// Leaderboard games (golf/F1) whose scrolling the user turned OFF: they
    /// render as the static top-4 frame regardless of the league depth setting.
    #[serde(default)]
    pub no_scroll_game_ids: Vec<String>,
    /// Board-local IANA timezone name ("America/New_York"); None = server-local.
    /// Set automatically by the app (`Command::SetTimezone`) so the idle clock
    /// and game start-time labels track where the BOARD lives.
    #[serde(default)]
    pub timezone: Option<String>,
}

fn default_sleep_hours() -> u32 {
    24
}

fn default_dwell_secs() -> u32 {
    10
}

fn default_brightness() -> u8 {
    80
}

/// Version stamped into every settings export (`scoreboard_settings` field).
pub const PORTABLE_SETTINGS_VERSION: u32 = 1;

/// The DURABLE half of a `BoardConfig`, as a portable blob: the preferences a
/// user would want to carry to another scoreboard (possibly on a different
/// server). Deliberately excludes board identity (`name`), live state
/// (`power`/`wake_at`), and per-game one-offs (extra/excluded/focused/spoiler
/// game ids — those ids expire with the games they point at).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PortableSettings {
    /// Format marker + version. Named `scoreboard_settings` so an exported blob
    /// is self-identifying; always `PORTABLE_SETTINGS_VERSION` on export. The
    /// serde default (0) makes arbitrary JSON parse but fail the version check.
    #[serde(rename = "scoreboard_settings", default)]
    pub version: u32,
    #[serde(default = "default_brightness")]
    pub brightness: u8,
    /// Seconds each game holds the panel before the rotation advances.
    #[serde(default = "default_dwell_secs")]
    pub dwell_secs: u32,
    /// How long a button-press sleep lasts before the board wakes itself.
    #[serde(default = "default_sleep_hours")]
    pub sleep_hours: u32,
    /// What the panel shows while the display is off / idle.
    #[serde(default)]
    pub off_state: OffState,
    /// How spoiler-protected games render (strict = matchup + time only).
    #[serde(default)]
    pub spoiler_mode: SpoilerMode,
    /// Spoiler-protect EVERY game on the board.
    #[serde(default)]
    pub spoiler_all: bool,
    /// Per-league preferences: favorites, big games, dwell/rows overrides,
    /// league + team spoiler rules.
    #[serde(default)]
    pub sports: Vec<SportConfig>,
    /// Board-local IANA timezone. Optional in the blob: absent/None means
    /// "keep the target board's timezone" — boards live in places, so an old
    /// export must never erase a zone the board already knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

impl From<&BoardConfig> for PortableSettings {
    fn from(cfg: &BoardConfig) -> Self {
        Self {
            version: PORTABLE_SETTINGS_VERSION,
            brightness: cfg.brightness,
            dwell_secs: cfg.dwell_secs,
            sleep_hours: cfg.sleep_hours,
            off_state: cfg.off_state,
            spoiler_mode: cfg.spoiler_mode,
            spoiler_all: cfg.spoiler_all,
            sports: cfg.sports.clone(),
            timezone: cfg.timezone.clone(),
        }
    }
}

impl PortableSettings {
    /// Overwrite exactly the durable preferences on `cfg` — nothing else. The
    /// board keeps its identity (name), live state (power/wake), and per-game
    /// rotation/spoiler one-offs.
    pub fn apply_to(&self, cfg: &mut BoardConfig) {
        cfg.brightness = self.brightness;
        cfg.dwell_secs = self.dwell_secs;
        cfg.sleep_hours = self.sleep_hours;
        cfg.off_state = self.off_state;
        cfg.spoiler_mode = self.spoiler_mode;
        cfg.spoiler_all = self.spoiler_all;
        cfg.sports = self.sports.clone();
        // Timezone travels only when the export carried one — an old (or
        // hand-trimmed) blob must not move a board's clock.
        if let Some(tz) = &self.timezone {
            cfg.timezone = Some(tz.clone());
        }
    }
}

impl Default for BoardConfig {
    fn default() -> Self {
        Self {
            brightness: 80,
            power: true,
            sports: Vec::new(),
            extra_game_ids: Vec::new(),
            excluded_game_ids: Vec::new(),
            focused_game_id: None,
            name: None,
            off_state: OffState::default(),
            sleep_hours: default_sleep_hours(),
            dwell_secs: default_dwell_secs(),
            wake_at: None,
            spoiler_mode: SpoilerMode::default(),
            spoiler_all: false,
            spoiler_game_ids: Vec::new(),
            revealed_game_ids: Vec::new(),
            no_scroll_game_ids: Vec::new(),
            timezone: None,
        }
    }
}

// ---- controller commands ----
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum Command {
    SetPower { on: bool },
    SetBrightness { level: u8 },
    SetSportConfig {
        sport_id: String,
        favorite_team_ids: Vec<String>,
        championship_option_id: Option<String>,
    },
    AddGame { game_id: String },
    RemoveGame { game_id: String },
    FocusGame { game_id: String },
    ClearFocus,
    /// Back to the preference-driven rotation: clears the pin, one-off additions,
    /// and one-off exclusions in a single step.
    ResetRotation,
    /// What the panel shows while off / idle (blank or a server-rendered clock).
    SetOffState { off_state: OffState },
    /// How long a button-press sleep lasts (hours) before auto-wake.
    SetSleepHours { hours: u32 },
    /// Stay asleep with no auto-wake: cancels a running sleep timer without
    /// turning the display back on ("off until I say so").
    ClearWakeTimer,
    /// Seconds each game holds the panel before the rotation advances.
    SetDwell { secs: u32 },
    /// Per-league dwell override; None reverts the league to the board default.
    SetLeagueDwell { sport_id: String, secs: Option<u32> },
    /// Leaderboard depth for golf/F1 (4 = static top-4 .. 20 = full field);
    /// None reverts to the default (10).
    SetLeagueRows { sport_id: String, rows: Option<u32> },
    /// How spoiler-protected games render (strict vs normal).
    SetSpoilerMode { mode: SpoilerMode },
    /// Spoiler-protect every game on the board.
    SetSpoilerAll { on: bool },
    /// Spoiler-protect a whole league.
    SetLeagueSpoiler { sport_id: String, on: bool },
    /// Spoiler-protect one team's games.
    SetTeamSpoiler { sport_id: String, team_id: String, on: bool },
    /// Per-game toggle: hidden=true protects one game; hidden=false reveals it
    /// even when team/league/board rules would protect it.
    SetGameSpoiler { game_id: String, hidden: bool },
    /// Per-game scroll toggle for leaderboard games (golf/F1): scroll=false
    /// pins the game to the static top-4 frame; scroll=true restores the
    /// league depth setting.
    SetGameScroll { game_id: String, scroll: bool },
    /// Board-local IANA timezone ("America/New_York") — drives the idle clock
    /// and game start-time labels. Unparseable names are ignored.
    SetTimezone { tz: String },
    Rename { name: String },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Role {
    Device,
    Controller,
}

// ---- websocket messages (JSON over text frames) ----
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "t")]
pub enum ClientMsg {
    Auth { role: Role, pubkey: String, sig: String },
    NeedFrames { ids: Vec<FrameId> },
    DeviceStatus { current_frame_id: Option<FrameId> },
    Subscribe { board_ids: Vec<String> },
    Command { req_id: u64, board_id: String, cmd: Command },
    GetCatalog,
    // controller asks for all live games (to browse + add to a board)
    GetGames {
        /// Scope summaries to a board so its spoiler rules apply. Absent from
        /// old clients (unit-variant JSON) — deserializes as None.
        #[serde(default)]
        board_id: Option<String>,
    },
    // device announces it's in setup showing this pairing code
    EnterSetup { code: String },
    // controller asks to pair with whichever board is showing this code
    Pair { code: String },
    // device reports a single knob press — the server decides sleep/wake/notice
    ButtonPress,
    // device reports a knob turn — the server adopts this as the board's one
    // brightness (the same value SetBrightness writes and the manifest echoes)
    DeviceBrightness { level: u8 },
    // device reports its self-discovered IANA timezone (IP-geolocated at boot)
    // — the server adopts it for this board's clock and start-time labels, so
    // the zone tracks where the BOARD lives with no phone involved (and DST is
    // automatic: the zone NAME doesn't change, only its offset does)
    DeviceTimezone { tz: String },
    // controller asks for this board's durable settings as a portable JSON blob
    // (a `PortableSettings`), answered by `ServerMsg::SettingsExport`
    ExportSettings { board_id: String },
    // controller applies a previously exported blob to this board, answered by
    // `ServerMsg::ImportResult`
    ImportSettings { board_id: String, json: String },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "t")]
pub enum ServerMsg {
    Hello { challenge: String },
    AuthOk,
    Manifest(Manifest),
    Catalog(Catalog),
    // struct variant (NOT a newtype) — internally-tagged enums can't serialize a
    // newtype wrapping a sequence, which would panic on every browse request.
    Games { games: Vec<GameSummary> },
    BoardState {
        board_id: String,
        online: bool,
        now_showing: Vec<GameSummary>,
        config: BoardConfig,
    },
    CommandAck {
        req_id: u64,
        ok: bool,
        error: Option<String>,
    },
    // pairing results (to a controller)
    Paired { board_id: String },
    PairFailed { reason: String },
    // to a device: you've been paired, leave setup
    SetupDone,
    // this board's durable settings as a portable JSON blob (pretty-printed
    // `PortableSettings`) — the reply to `ClientMsg::ExportSettings`
    SettingsExport { board_id: String, json: String },
    // outcome of `ClientMsg::ImportSettings` (None = applied successfully)
    ImportResult { board_id: String, error: Option<String> },
}

// ---- binary frame framing ----
// A frame is sent as a binary websocket message:
//   [ 64 bytes ASCII hex frame_id ][ RGB565-LE pixels ]
pub const FRAME_ID_HEX_LEN: usize = 64;

pub fn encode_frame(id: &str, rgb565_le: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(FRAME_ID_HEX_LEN + rgb565_le.len());
    v.extend_from_slice(id.as_bytes());
    v.extend_from_slice(rgb565_le);
    v
}

pub fn decode_frame(buf: &[u8]) -> Option<(FrameId, &[u8])> {
    if buf.len() < FRAME_ID_HEX_LEN {
        return None;
    }
    let id = core::str::from_utf8(&buf[..FRAME_ID_HEX_LEN]).ok()?.to_string();
    Some((id, &buf[FRAME_ID_HEX_LEN..]))
}

// ---- Ed25519 identity (shared by server, app-core, firmware) ----
// A client's identity IS its public key. On connect the server sends a random
// challenge; the client signs it with its 32-byte secret seed; the server verifies
// the signature against the claimed pubkey. The 32-byte secret is generated once
// per device/app-install (platform RNG) and persisted.

/// Public key (hex) derived from a 32-byte Ed25519 secret seed.
pub fn pubkey_hex(secret: &[u8; 32]) -> String {
    hex::encode(SigningKey::from_bytes(secret).verifying_key().to_bytes())
}

/// Sign `msg` with the secret seed; returns the 64-byte signature as hex.
pub fn sign_hex(secret: &[u8; 32], msg: &[u8]) -> String {
    hex::encode(SigningKey::from_bytes(secret).sign(msg).to_bytes())
}

/// True iff `sig_hex` is `pubkey_hex`'s Ed25519 signature over `msg`.
pub fn verify_hex(pubkey_hex: &str, msg: &[u8], sig_hex: &str) -> bool {
    let Ok(pk_bytes) = hex::decode(pubkey_hex) else { return false };
    let Ok(pk): Result<[u8; 32], _> = pk_bytes.try_into() else { return false };
    let Ok(vk) = VerifyingKey::from_bytes(&pk) else { return false };
    let Ok(sig_bytes) = hex::decode(sig_hex) else { return false };
    let Ok(sig): Result<[u8; 64], _> = sig_bytes.try_into() else { return false };
    vk.verify(msg, &Signature::from_bytes(&sig)).is_ok()
}

#[cfg(test)]
mod ed25519_tests {
    use super::*;
    #[test]
    fn sign_verify_roundtrip() {
        let secret = [7u8; 32];
        let pk = pubkey_hex(&secret);
        let sig = sign_hex(&secret, b"challenge-123");
        assert!(verify_hex(&pk, b"challenge-123", &sig));
        assert!(!verify_hex(&pk, b"different-msg", &sig)); // wrong message
        assert!(!verify_hex(&pk, b"challenge-123", "deadbeef")); // malformed sig
        assert!(!verify_hex(&pubkey_hex(&[8u8; 32]), b"challenge-123", &sig)); // wrong key
    }
}
