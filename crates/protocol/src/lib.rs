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
    sha256_hex(rgb565_le)
}

/// Hex sha256 of arbitrary bytes (frame ids, firmware images).
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
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
    /// Deep link to ESPN's full human-readable game page ("View on ESPN"),
    /// when one can be formed — a numeric ESPN event id in a sport with a game
    /// page. None for demo games, leaderboard sports (golf/F1), and anything
    /// without a resolvable page. Spoiler-safe: it's just a URL, no result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub espn_url: Option<String>,
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

// ---- per-league team-name display style ----
/// How a team's name is rendered on the board for a given league. Resolved
/// per-board: a league's `SportConfig.name_style`, falling back to
/// [`default_name_style`] when that is `None`. A `TeamSide` carries all three
/// strings (location / name / abbrev) so any style can be drawn from one game.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum NameStyle {
    /// City / college / country — `TeamSide.location`.
    Location,
    /// Mascot / team name — `TeamSide.name` (the default for pro clubs).
    #[default]
    Name,
    /// Team abbreviation — `TeamSide.abbrev`.
    Abbrev,
}

/// The name-style choices a league offers, as `(style, LABEL)` pairs in menu
/// order. Empty for leaderboard sports (golf/F1 — individual fields, no
/// per-team style). Shared by the server (render + overflow report) and the
/// app (the per-league "Names" picker), so both speak the same menu.
pub fn name_style_options(sport_id: &str) -> &'static [(NameStyle, &'static str)] {
    use NameStyle::*;
    match sport_id {
        // College: the LOCATION is the identity ("Michigan"); the mascot
        // ("Wolverines") is optional. Default is Location.
        "cfb" | "cbb" | "wcbb" => {
            &[(Location, "College"), (Name, "Team name"), (Abbrev, "Abbreviation")]
        }
        // World Cup national teams have no mascot — country or abbreviation only.
        "wc" => &[(Location, "Country"), (Abbrev, "Abbreviation")],
        // Other team sports: pro clubs, which default to the mascot.
        "nhl" | "nba" | "mlb" | "nfl" | "wnba" | "mls" | "epl" => {
            &[(Location, "City"), (Name, "Team name"), (Abbrev, "Abbreviation")]
        }
        // Leaderboard sports (f1/pga/lpga) and anything unknown: no per-team
        // style — individual leaderboards, not two-team rows.
        _ => &[],
    }
}

/// The default name style for a league, used when `SportConfig.name_style` is
/// `None`. College + World Cup identify by location; every other league
/// (including the leaderboard sports, where it is unused) defaults to Name.
pub fn default_name_style(sport_id: &str) -> NameStyle {
    match sport_id {
        "cfb" | "cbb" | "wcbb" | "wc" => NameStyle::Location,
        _ => NameStyle::Name,
    }
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
    /// Per-league team-name display style; None = the league default
    /// ([`default_name_style`]). Location = city/college/country, Name =
    /// mascot, Abbrev = abbreviation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_style: Option<NameStyle>,
    /// Spoiler-protect every game in this league.
    #[serde(default, skip_serializing_if = "is_false")]
    pub spoiler: bool,
    /// Spoiler-protect games involving these teams (team ids).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spoiler_team_ids: Vec<String>,
}

/// Which firmware release channel a board follows. Channels are cumulative
/// toward Dev: a board may always be offered anything from the channels BELOW
/// its own (stable ⊆ beta ⊆ dev), newest CalVer wins — so a beta board whose
/// beta build is older than the current stable release gets the stable one.
/// All channels share ONE CalVer space (`scripts/release-firmware.sh` refuses
/// a version that exists in any channel), so a version string identifies a
/// unique image regardless of where it was published.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum FwChannel {
    #[default]
    Stable,
    Beta,
    Dev,
}

impl FwChannel {
    /// Every channel, in stable-first order.
    pub const ALL: [FwChannel; 3] = [FwChannel::Stable, FwChannel::Beta, FwChannel::Dev];

    /// The dist-directory / route-segment name ("stable" | "beta" | "dev").
    pub fn as_str(self) -> &'static str {
        match self {
            FwChannel::Stable => "stable",
            FwChannel::Beta => "beta",
            FwChannel::Dev => "dev",
        }
    }

    /// Inverse of [`as_str`](Self::as_str) — used to parse route segments.
    pub fn from_name(s: &str) -> Option<FwChannel> {
        match s {
            "stable" => Some(FwChannel::Stable),
            "beta" => Some(FwChannel::Beta),
            "dev" => Some(FwChannel::Dev),
            _ => None,
        }
    }

    /// The offer matrix: which channels' dists a board on this channel may be
    /// offered. Stable -> {stable}; Beta -> {stable, beta}; Dev -> all three.
    pub fn allowed(self) -> &'static [FwChannel] {
        match self {
            FwChannel::Stable => &[FwChannel::Stable],
            FwChannel::Beta => &[FwChannel::Stable, FwChannel::Beta],
            FwChannel::Dev => &[FwChannel::Stable, FwChannel::Beta, FwChannel::Dev],
        }
    }
}

/// One firmware image the server currently hosts, as listed for the DEBUG
/// firmware picker (`ServerMsg::FirmwareList`). Every channel the server has a
/// dist for is listed regardless of any board's enrollment — the picker shows
/// the full menu and lets the on-device same-or-newer guard decide what takes.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FwImage {
    /// The channel this dist lives in ("stable" | "beta" | "dev").
    pub channel: String,
    /// The dist's CalVer version ("2026.7.3").
    pub version: String,
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
    /// Firmware release channel this board follows (`Command::SetFwChannel`).
    /// Durable per-board config, but deliberately NOT part of
    /// `PortableSettings` — see the note there.
    #[serde(default)]
    pub fw_channel: FwChannel,
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
///
/// `fw_channel` is durable per-board config but is ALSO deliberately excluded:
/// importing a friend's settings must never enroll a board in beta/dev
/// firmware — release-channel membership is a per-board decision, not a
/// portable preference.
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
            fw_channel: FwChannel::default(),
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
    /// Per-league team-name display style (city/college/country vs mascot vs
    /// abbreviation); None reverts the league to its default
    /// ([`default_name_style`]).
    SetLeagueNameStyle { sport_id: String, style: Option<NameStyle> },
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
    /// Which firmware release channel this board follows. Takes effect
    /// immediately: the server persists it and re-pushes `BoardState`, whose
    /// `firmware_update` offer is recomputed against the new channel's
    /// allowed dists.
    SetFwChannel { channel: FwChannel },
    /// Tell this board to download, verify, and install the firmware image the
    /// server is hosting (the one advertised via `BoardState::firmware_update`).
    UpdateFirmware,
    /// Install a SPECIFIC channel's current dist (the DEBUG firmware picker /
    /// "reinstall current" flow). The server sends that channel's dist as a
    /// `ServerMsg::UpdateFirmware` offer, exactly like `UpdateFirmware` but
    /// targeting the named channel instead of the newest allowed pick — the
    /// on-device same-or-newer guard still governs whether it takes (so an
    /// older channel's dist is a no-op on the board). No-op if that channel
    /// has no hosted dist.
    InstallFirmware { channel: FwChannel },
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
    // controller gives up its own grant on a board ("Remove this scoreboard").
    // The server revokes only the SENDING controller's pairing — the identity
    // comes from the signed handshake, so this can't touch anyone else's.
    // Without it a grant was permanent: removal was app-UI only and the board
    // came back on the next launch.
    Unpair { board_id: String },
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
    // device reports its running firmware version (env!("CARGO_PKG_VERSION")),
    // sent once after auth — the server echoes it into `BoardState` and uses it
    // to decide whether to offer the hosted image as an update
    DeviceVersion { version: String },
    // controller (DEBUG firmware picker) asks for every firmware image the
    // server hosts, across ALL channels regardless of board enrollment —
    // answered by `ServerMsg::FirmwareList`
    ListFirmware,
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
        /// Firmware version the device last reported on this connection
        /// (`ClientMsg::DeviceVersion`); None while offline or pre-report.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        firmware_version: Option<String>,
        /// Version of the server-hosted firmware image, present ONLY when it is
        /// strictly newer (CalVer compare) than `firmware_version` — the app
        /// shows its Update button exactly when this is Some.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        firmware_update: Option<String>,
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
    // to a device: download + install this firmware image. The device GETs
    // `path` (e.g. "/firmware/beta.bin") from http://{server_host}:{server_port}
    // (the server it is already connected to), checks the byte count against
    // `size`, verifies `sha256` (hex, over the raw image bytes) and `sig` (hex
    // Ed25519 signature over the raw image bytes, against the baked-in
    // FW_PUBKEY_HEX), writes the inactive OTA slot, and reboots into it. The
    // path points at the CHANNEL the picked dist lives in (a beta board whose
    // best offer is the stable image is sent the stable path).
    UpdateFirmware {
        version: String,
        size: u32,
        sha256: String,
        sig: String,
        path: String,
        /// The channel the picked dist lives in ("stable"/"beta"/"dev"). Bound
        /// into the Ed25519 signature alongside version and size, so a dev
        /// image can never verify under a relabeled offer.
        channel: String,
    },
    // to controllers subscribed to `board_id`: how far the board's in-flight
    // firmware download has gotten (0..=100). The device's ws is DROPPED during
    // the download, so this is SERVER-OBSERVED — the server counts the bytes it
    // hands the board's HTTP fetch (TCP backpressure paces that to the board's
    // real consumption) and pushes progress as it streams. Cleared (implicitly)
    // when the stream ends: the board reboots into the new image and re-pushes
    // BoardState, or a failure just stops the updates.
    FirmwareProgress { board_id: String, pct: u8 },
    // reply to `ClientMsg::ListFirmware`: every firmware image the server hosts,
    // one per channel that has a current dist (the DEBUG picker's full menu).
    FirmwareList { images: Vec<FwImage> },
}

// ---- firmware versions (CalVer "YYYY.M.P") ----

/// Parse a CalVer firmware version ("2026.7.0") into its three numeric parts.
/// None when the string isn't exactly three '.'-separated u32s.
pub fn parse_calver(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    let c = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((a, b, c))
}

/// True iff `candidate` is strictly newer than `current`: both parse as CalVer
/// and compare greater as a (year, month, patch) tuple. Unparseable versions
/// are never newer (either side failing to parse means "don't offer/install").
pub fn version_newer(candidate: &str, current: &str) -> bool {
    match (parse_calver(candidate), parse_calver(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
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
mod calver_tests {
    use super::*;
    #[test]
    fn parses_calver() {
        assert_eq!(parse_calver("2026.7.0"), Some((2026, 7, 0)));
        assert_eq!(parse_calver("2026.12.34"), Some((2026, 12, 34)));
        assert_eq!(parse_calver("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_calver("2026.7"), None); // too few parts
        assert_eq!(parse_calver("2026.7.0.1"), None); // too many parts
        assert_eq!(parse_calver("2026.7.x"), None); // non-numeric
        assert_eq!(parse_calver(""), None);
    }

    #[test]
    fn newer_is_strict_tuple_compare() {
        assert!(version_newer("2026.7.1", "2026.7.0"));
        assert!(version_newer("2026.8.0", "2026.7.9"));
        assert!(version_newer("2027.1.0", "2026.12.9"));
        // NUMERIC compare, not string compare: 10 > 9.
        assert!(version_newer("2026.10.0", "2026.9.0"));
        assert!(!version_newer("2026.7.0", "2026.7.0")); // equal = not newer
        assert!(!version_newer("2026.6.9", "2026.7.0")); // older
        // Unparseable on either side = never newer.
        assert!(!version_newer("garbage", "2026.7.0"));
        assert!(!version_newer("2026.7.0", "garbage"));
    }
}

#[cfg(test)]
mod fw_channel_tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for chan in FwChannel::ALL {
            assert_eq!(FwChannel::from_name(chan.as_str()), Some(chan));
        }
        assert_eq!(FwChannel::from_name("nightly"), None);
        assert_eq!(FwChannel::from_name("Stable"), None); // route names are lowercase
        assert_eq!(FwChannel::default(), FwChannel::Stable);
    }

    #[test]
    fn offer_matrix_allowed_sets() {
        assert_eq!(FwChannel::Stable.allowed(), &[FwChannel::Stable]);
        assert_eq!(FwChannel::Beta.allowed(), &[FwChannel::Stable, FwChannel::Beta]);
        assert_eq!(
            FwChannel::Dev.allowed(),
            &[FwChannel::Stable, FwChannel::Beta, FwChannel::Dev]
        );
    }

    /// An old (pre-channel) persisted config deserializes onto Stable.
    #[test]
    fn config_missing_channel_defaults_to_stable() {
        let cfg: BoardConfig = serde_json::from_str("{\"brightness\":80,\"power\":true,\"sports\":[]}").unwrap();
        assert_eq!(cfg.fw_channel, FwChannel::Stable);
    }

    /// fw_channel is durable config but deliberately NOT portable: exporting a
    /// beta board's settings and importing them elsewhere must not enroll the
    /// target board in beta — and the export must not leak the channel at all.
    #[test]
    fn portable_settings_do_not_move_fw_channel() {
        let mut source = BoardConfig { fw_channel: FwChannel::Beta, ..Default::default() };
        source.brightness = 40;
        let blob = PortableSettings::from(&source);
        let json = serde_json::to_string(&blob).unwrap();
        assert!(!json.contains("channel"), "channel leaked into the export: {json}");

        let mut target = BoardConfig::default();
        assert_eq!(target.fw_channel, FwChannel::Stable);
        let parsed: PortableSettings = serde_json::from_str(&json).unwrap();
        parsed.apply_to(&mut target);
        assert_eq!(target.brightness, 40, "durable prefs DO transfer");
        assert_eq!(target.fw_channel, FwChannel::Stable, "channel must not transfer");

        // And the other direction: importing a stable board's settings onto a
        // dev board must not knock the dev board off its channel either.
        let mut dev_board = BoardConfig { fw_channel: FwChannel::Dev, ..Default::default() };
        parsed.apply_to(&mut dev_board);
        assert_eq!(dev_board.fw_channel, FwChannel::Dev);
    }
}

#[cfg(test)]
mod firmware_msg_tests {
    use super::*;

    /// The DEBUG firmware picker / progress messages round-trip through serde.
    /// (`FirmwareList` is a struct variant wrapping a Vec — the internally
    /// tagged enum can't serialize a NEWTYPE around a sequence, which is why
    /// `Games`/`FirmwareList` are struct variants; this pins that.)
    #[test]
    fn firmware_messages_round_trip() {
        let list = ServerMsg::FirmwareList {
            images: vec![
                FwImage { channel: "stable".into(), version: "2026.7.0".into() },
                FwImage { channel: "dev".into(), version: "2026.7.3".into() },
            ],
        };
        let json = serde_json::to_string(&list).unwrap();
        match serde_json::from_str::<ServerMsg>(&json).unwrap() {
            ServerMsg::FirmwareList { images } => {
                assert_eq!(images.len(), 2);
                assert_eq!(images[1].channel, "dev");
                assert_eq!(images[1].version, "2026.7.3");
            }
            other => panic!("expected FirmwareList, got {other:?}"),
        }

        let prog = ServerMsg::FirmwareProgress { board_id: "b1".into(), pct: 42 };
        let json = serde_json::to_string(&prog).unwrap();
        assert!(matches!(
            serde_json::from_str::<ServerMsg>(&json).unwrap(),
            ServerMsg::FirmwareProgress { board_id, pct } if board_id == "b1" && pct == 42
        ));

        // The controller request + the channel-targeted install command.
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(
                &serde_json::to_string(&ClientMsg::ListFirmware).unwrap()
            )
            .unwrap(),
            ClientMsg::ListFirmware
        ));
        let install = Command::InstallFirmware { channel: FwChannel::Beta };
        assert!(matches!(
            serde_json::from_str::<Command>(&serde_json::to_string(&install).unwrap()).unwrap(),
            Command::InstallFirmware { channel: FwChannel::Beta }
        ));
    }
}

#[cfg(test)]
mod name_style_tests {
    use super::*;

    #[test]
    fn defaults_per_league() {
        // College identifies by location.
        assert_eq!(default_name_style("cfb"), NameStyle::Location);
        assert_eq!(default_name_style("cbb"), NameStyle::Location);
        assert_eq!(default_name_style("wcbb"), NameStyle::Location);
        // World Cup national teams: default to the country (Location).
        assert_eq!(default_name_style("wc"), NameStyle::Location);
        // Pro clubs default to the mascot.
        assert_eq!(default_name_style("nhl"), NameStyle::Name);
        assert_eq!(default_name_style("mlb"), NameStyle::Name);
        assert_eq!(default_name_style("epl"), NameStyle::Name);
        // Leaderboard sports: unused, but Name by contract.
        assert_eq!(default_name_style("f1"), NameStyle::Name);
        assert_eq!(default_name_style("pga"), NameStyle::Name);
        assert_eq!(default_name_style("lpga"), NameStyle::Name);
        // The enum's own Default is Name.
        assert_eq!(NameStyle::default(), NameStyle::Name);
    }

    #[test]
    fn options_per_league() {
        // College offers all three, Location first, with the "College" label.
        let college = name_style_options("cfb");
        assert_eq!(college.len(), 3);
        assert_eq!(college[0], (NameStyle::Location, "College"));
        assert_eq!(
            college.iter().map(|(_, l)| *l).collect::<Vec<_>>(),
            vec!["College", "Team name", "Abbreviation"]
        );
        // cbb/wcbb share the college table.
        assert_eq!(name_style_options("cbb"), college);
        assert_eq!(name_style_options("wcbb"), college);

        // World Cup has NO Name option (countries have no mascot).
        let wc = name_style_options("wc");
        assert_eq!(wc.len(), 2);
        assert_eq!(wc[0], (NameStyle::Location, "Country"));
        assert_eq!(wc[1], (NameStyle::Abbrev, "Abbreviation"));
        assert!(!wc.iter().any(|(s, _)| *s == NameStyle::Name), "no mascot for nations");

        // Pro club: City / Team name / Abbreviation.
        let nhl = name_style_options("nhl");
        assert_eq!(nhl.len(), 3);
        assert_eq!(nhl[0], (NameStyle::Location, "City"));

        // Leaderboard sports have no per-team style at all.
        assert!(name_style_options("f1").is_empty());
        assert!(name_style_options("pga").is_empty());
        assert!(name_style_options("lpga").is_empty());
        // Unknown leagues degrade to empty too.
        assert!(name_style_options("zzz").is_empty());
    }

    #[test]
    fn serde_snake_case_and_skip_none() {
        // The wire form is snake_case.
        assert_eq!(serde_json::to_string(&NameStyle::Location).unwrap(), "\"location\"");
        assert_eq!(serde_json::to_string(&NameStyle::Name).unwrap(), "\"name\"");
        assert_eq!(serde_json::to_string(&NameStyle::Abbrev).unwrap(), "\"abbrev\"");

        // None name_style is omitted from a serialized SportConfig...
        let cfg = SportConfig { sport_id: "nhl".into(), ..Default::default() };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("name_style"), "None must be skipped: {json}");

        // ...and Some round-trips.
        let cfg2 =
            SportConfig { sport_id: "cfb".into(), name_style: Some(NameStyle::Abbrev), ..Default::default() };
        let round: SportConfig = serde_json::from_str(&serde_json::to_string(&cfg2).unwrap()).unwrap();
        assert_eq!(round.name_style, Some(NameStyle::Abbrev));

        // A pre-name-style persisted config deserializes with name_style None.
        let old: SportConfig =
            serde_json::from_str("{\"sport_id\":\"nhl\",\"favorite_team_ids\":[],\"championship_option_id\":null}")
                .unwrap();
        assert_eq!(old.name_style, None);
    }

    #[test]
    fn command_round_trips() {
        let cmd = Command::SetLeagueNameStyle {
            sport_id: "wc".into(),
            style: Some(NameStyle::Location),
        };
        assert!(matches!(
            serde_json::from_str::<Command>(&serde_json::to_string(&cmd).unwrap()).unwrap(),
            Command::SetLeagueNameStyle { style: Some(NameStyle::Location), .. }
        ));
        // None (revert to default) round-trips too.
        let revert = Command::SetLeagueNameStyle { sport_id: "cfb".into(), style: None };
        assert!(matches!(
            serde_json::from_str::<Command>(&serde_json::to_string(&revert).unwrap()).unwrap(),
            Command::SetLeagueNameStyle { style: None, .. }
        ));
    }
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
