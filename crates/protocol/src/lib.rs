//! Shared wire protocol + data model for the scoreboard system.
//!
//! Used by the server, the board firmware/simulator, and (later, via uniffi)
//! the app core. This is the single source of truth for messages and types.
//! `no_std + alloc` so the ESP32 firmware can depend on it.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::collections::BTreeMap;
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

/// One off/idle behavior the SERVER offers, declared as data so a forked server
/// can add an off mode and the stock app picks it up with no rebuild.
///
/// The app must NEVER recognize an `id`. Every string it needs to draw the row
/// lives in this struct — the moment something reads `if id == "weather"`, the
/// fork-without-an-app-release property is gone. Order is menu order.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OffModeDescriptor {
    /// Stable contract. "clock" and "blank" are reserved.
    pub id: String,
    pub label: String,
    /// Row subtitle — the one-line explanation of what the panel will show.
    pub description: String,
    /// Path of the preview PNG ("/offmode/clock.png"). `None` means this mode
    /// draws nothing: the app fills a black rectangle locally rather than
    /// fetching an all-black image it cannot distinguish from a failed load.
    #[serde(default)]
    pub preview: Option<String>,
    /// LEGACY, and still emitted: an app built before `fields` existed reads
    /// only this, so a server that dropped it would take the location box away
    /// from every installed copy. Filled with the first text field.
    ///
    /// AT MOST ONE free-text input (a ZIP for weather, tickers for stocks).
    /// Deliberately not a field-schema: a field-KIND enum would itself be a
    /// closed enum in the uniffi bindings, which is the exact cliff this whole
    /// design exists to avoid. A mode needing two inputs, a picker, or
    /// validation feedback needs an app release — that is the stated cost.
    #[serde(default)]
    pub input: Option<OffModeInput>,
    /// The mode's settings screen, in display order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<OffModeField>,
}

impl OffModeDescriptor {
    /// Stored value, else the declared default, else empty.
    ///
    /// THE resolution order, and the reason it lives in the protocol crate:
    /// the server renders from it and the app displays from it, so if they
    /// disagreed about what an unset field means the panel and the phone would
    /// show different things with nothing to point at.
    pub fn value<'a>(&'a self, key: &str, params: &'a BTreeMap<String, String>) -> &'a str {
        if let Some(v) = params.get(key) {
            return v;
        }
        self.fields
            .iter()
            .find(|f| f.key == key)
            .map(|f| f.default.as_str())
            .unwrap_or("")
    }

    /// Whether a field shows, given the RESOLVED value of the key it watches.
    pub fn visible(&self, field: &OffModeField, params: &BTreeMap<String, String>) -> bool {
        match &field.visible_when {
            None => true,
            Some(c) => self.value(&c.key, params) == c.equals,
        }
    }

    pub fn visible_fields(&self, params: &BTreeMap<String, String>) -> Vec<&OffModeField> {
        self.fields.iter().filter(|f| self.visible(f, params)).collect()
    }

    /// Every declared field resolved — what a renderer reads.
    pub fn resolved(&self, params: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        self.fields
            .iter()
            .map(|f| (f.key.clone(), self.value(&f.key, params).to_string()))
            .collect()
    }

    /// Is `value` legal for the declared field? Rejects anything undeclared,
    /// so a hand-edited import cannot smuggle keys into the stored map.
    pub fn validate(&self, key: &str, value: &str) -> Result<(), &'static str> {
        let Some(f) = self.fields.iter().find(|f| f.key == key) else {
            return Err("no such setting");
        };
        match f.kind.as_str() {
            // CHARACTERS, not bytes. The app clamps with `String.prefix`, which
            // counts characters; bounding bytes here meant the two disagreed on
            // anything non-ASCII and the server silently rejected what the app
            // had already accepted.
            "text" => {
                if f.max_len > 0 && value.chars().count() > f.max_len as usize {
                    Err("too long")
                } else {
                    Ok(())
                }
            }
            "toggle" => {
                if value == "0" || value == "1" {
                    Ok(())
                } else {
                    Err("a toggle is \"0\" or \"1\"")
                }
            }
            "choice" => {
                if f.options.iter().any(|o| o.value == value) {
                    Ok(())
                } else {
                    Err("not one of the offered choices")
                }
            }
            // An unknown kind is a FORK'S field this binary does not implement.
            // Store it untouched rather than rejecting: the server that
            // declared it is the one that will read it.
            _ => Ok(()),
        }
    }
}

/// One control on a mode's settings screen.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OffModeField {
    /// Key into `BoardConfig::off_mode_params`.
    pub key: String,
    pub label: String,
    /// `"text"`, `"toggle"`, or `"choice"`.
    ///
    /// A STRING, never an enum. A closed enum here is the cliff this whole
    /// architecture exists to avoid: the generated Swift ends its decode in
    /// `unexpectedEnumCase`, and that read runs inside the single call that
    /// decodes the WHOLE config — one unknown discriminant and the board's
    /// entire settings blob is discarded. An app that meets a kind it does not
    /// know SKIPS that field, and `default` keeps the mode working meanwhile.
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub placeholder: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub max_len: u32,
    /// What the field means when nothing is stored. Never written into
    /// `off_mode_params` — storing defaults would freeze them at the value the
    /// server happened to have when the user first opened the screen.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default: String,
    /// The choices, for `kind == "choice"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<OffModeChoice>,
    /// Show this field only while another field holds a given value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_when: Option<FieldCondition>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OffModeChoice {
    pub value: String,
    pub label: String,
    /// Preview for THIS choice. Descriptor-carried, so the app never has to
    /// build a URL out of an id it is not allowed to understand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// One level of conditional visibility — not a rule engine. Anything needing
/// nesting is a sign the mode wants two screens, not a cleverer condition.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FieldCondition {
    pub key: String,
    pub equals: String,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OffModeInput {
    /// Key into `BoardConfig::off_mode_params`.
    pub key: String,
    pub label: String,
    pub placeholder: String,
    pub max_len: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SportDescriptor {
    pub id: String,
    pub name: String,
    pub teams: Vec<Team>,
    /// LEGACY, frozen: the single-select ladder an already-installed app
    /// renders. Kept so old builds keep a working picker.
    pub championship_options: Vec<ChampionshipOption>,
    /// The independently selectable rounds. Each is exactly one thing, and none
    /// implies another.
    #[serde(default)]
    pub championship_rounds: Vec<ChampionshipOption>,
    /// Sport family this league is listed under ("Hockey", "Soccer"), as a
    /// DISPLAY string.
    ///
    /// A STRING, never an enum — the same rule as `OffModeField::kind`, for the
    /// same reason: the generated Swift decodes an unknown enum case into
    /// `unexpectedEnumCase`, and that read happens inside the single lift for
    /// the WHOLE league catalog. One family a fork invented would blank every
    /// league in the app rather than just its own.
    ///
    /// The server declares this so a fork can add a league and have it land in
    /// the right section without shipping a new app.
    #[serde(default)]
    pub category: String,
    /// Sort rank for the family section; ties break on league name. Real ranks
    /// start at 1, so the serde default of 0 marks "a server too old to say"
    /// and sorts last instead of first.
    #[serde(default)]
    pub category_rank: u32,
    /// What this league's box-score extras are CALLED, or empty when it draws
    /// none. Doubles as the gate and the label: an empty string means the app
    /// shows no toggle, and a non-empty one is the toggle's title.
    ///
    /// Server-declared and a STRING for the same reason as `category` — a fork
    /// adding a league gets a correctly-named switch without shipping an app,
    /// and there is no enum to decode into `unexpectedEnumCase` and blank the
    /// whole catalog. It also lets the switch say what it hides ("Hits and
    /// errors") rather than something generic.
    #[serde(default)]
    pub extras_label: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Catalog {
    pub sports: Vec<SportDescriptor>,
    /// The off modes this server offers. `#[serde(default)]` so an app talking
    /// to a server that predates them sees an empty list and falls back.
    #[serde(default)]
    pub off_modes: Vec<OffModeDescriptor>,
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
    /// This board's preferences already claim the game (favorite team, big-game
    /// option, league blanket, or a manual add) but it is NOT on the panel yet
    /// — it hasn't started. The app marks these in All Games so you can see
    /// what's coming without hunting through the slate. Board-scoped: only set
    /// when the browse request named a board.
    #[serde(default, skip_serializing_if = "is_false")]
    pub queued: bool,
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
    /// Draw the clock ON THE DEVICE rather than blitting the rotation.
    ///
    /// The rotation still carries a server-rendered clock frame: firmware that
    /// predates local clocks ignores this flag and blits that frame as it
    /// always has, and a board that hasn't been told the time yet falls back to
    /// it. An explicit flag rather than sniffing the rotation's shape, because
    /// "one frame with a 60s dwell" is not a contract.
    #[serde(default, skip_serializing_if = "is_false")]
    pub local_clock: bool,
    /// Draw the device-local clock in 24-hour time.
    ///
    /// Sent so the OFFLINE clock matches the format chosen in the app. Only
    /// the format travels, not the style: the board draws one layout, and a
    /// board that is offline is already showing a different screen than the one
    /// it shows connected — the struck-out Wi-Fi mark says so.
    #[serde(default, skip_serializing_if = "is_false")]
    pub clock_hour24: bool,
    pub off_state: OffState,
    pub brightness: u8, // 10..=100, applied device-side (one value: knob and app share it)
    /// Program identity. The board keeps its rotation slot across same-length
    /// refreshes (content-addressed ids change every score tick) — but that
    /// heuristic can't tell a 4-game rotation from a notice. A changed epoch
    /// says "different program: start from the top". 0 = the games rotation;
    /// notices mint a fresh nonzero value per showing.
    #[serde(default)]
    pub epoch: u64,
    /// Countdown-bar duration for a notice: the DEVICE fills the bar smoothly
    /// on its own clock over this many ms, anchored at the first manifest of
    /// this `epoch` (same-epoch re-pushes — a brightness change mid-notice —
    /// don't restart it). The frame arrives with the bar's outline empty; the
    /// board owns the fill. Same division of labor as `scroll`: pre-rendering
    /// the sweep as rotation steps cost ~4KB of board RAM per step, capping a
    /// 4s splash at 4 chunky jumps, and per-step pushes are network animation,
    /// which this system forbids. None = no bar. Old firmware ignores the
    /// field and shows the static outline — a graceful downgrade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub countdown_ms: Option<u32>,
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
    /// LEGACY, and frozen. The single "big games" option this league used to
    /// carry, kept load-bearing for two audiences: an OLDER SERVER binary (a
    /// rollback) and an ALREADY-INSTALLED app, which renders this one id and
    /// knows nothing about the set. Derived from `championship_option_ids` on
    /// every write — the broadest selected round — and never read once the list
    /// is present.
    ///
    /// Deliberately keeps NO serde default and NO skip: it is always written,
    /// as `null` when None, so an old reader always finds the key.
    pub championship_option_id: Option<String>,
    /// The rounds this league subscribes to, as EXACT ids.
    ///
    /// Each entry means that round and nothing else. The old single option
    /// nested — picking "conf" quietly pulled in the Final beyond it — which is
    /// what made the labels unreadable ("Selected + Conf Finals") and the
    /// behaviour unguessable. Empty = only your selected teams.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub championship_option_ids: Vec<String>,
    /// Per-league "time per event" override (seconds); None = the board's
    /// default dwell. Lets F1 scroll for 30s while baseball flashes by in 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dwell_secs: Option<u32>,
    /// Leaderboard depth for golf/F1 (rows shown on the board): 4 = static
    /// top-4 (no scrolling), up to 20 = the full field. None = default (10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaderboard_rows: Option<u32>,
    /// Whether a golf tournament shows which round it is on. `None` = the
    /// default, which is ON: a leaderboard alone looks the same on Thursday as
    /// on Sunday, so the round is worth its corner of the panel until someone
    /// says otherwise. Only golf reads this — no other league has rounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_round: Option<bool>,
    /// Whether this league draws the box-score extras beside the score: hits
    /// and errors on a baseball final, shots on goal on a live NHL frame, a
    /// shootout or aggregate score on a soccer knockout.
    ///
    /// `None` = the default, which is ON. Absence meaning "show" is the whole
    /// compatibility story: an older app never sends the key and gets exactly
    /// what it has always drawn, and an older server that has never heard of it
    /// ignores it the same way.
    ///
    /// One flag rather than one per kind, because the kinds are already
    /// partitioned by league — hits reach only baseball, shots only hockey,
    /// shootouts only soccer — so per-league IS per-kind, and a finer control
    /// would offer a distinction nobody can act on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_extras: Option<bool>,
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
    /// LEGACY, and frozen permanently. Kept load-bearing for two audiences: an
    /// OLDER SERVER binary (a rollback) and an ALREADY-INSTALLED app whose
    /// uniffi enum is closed and would abort the whole config decode on an
    /// unknown value. Derived from `off_mode` on every write — Blank when the
    /// mode draws nothing, Clock otherwise — and never read once `off_mode` is
    /// set. This is precisely why a forked off mode cannot make anyone's
    /// BoardConfig fail to parse.
    #[serde(default)]
    pub off_state: OffState,
    /// The server-declared off mode (`Catalog::off_modes[].id`). None = a row
    /// written before this field existed; `off_mode_id()` falls back.
    #[serde(default)]
    pub off_mode: Option<String>,
    /// Per-mode input values, keyed by `OffModeInput::key`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub off_mode_params: BTreeMap<String, String>,
    /// What the panel is DOING, as one open string: "scores" (the resting
    /// state — game rotation, idle screen when nothing is on), "off" (dark),
    /// or any server-declared off-mode id ("clock", "weather", a fork's
    /// screen) held persistently.
    ///
    /// A string, never an enum: the same compatibility rule as `off_mode`. An
    /// old binary ignores the field entirely and keeps reading `power`, which
    /// is why `power` is written as a derived twin on every mode write — see
    /// `set_mode`, and the invariant test beside the forked-off-mode one.
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Per-mode timer durations in HOURS, keyed by mode id ("off", "clock",
    /// …). Picking a non-scores mode runs it for its configured duration and
    /// then the board returns to scores on its own — the auto-wake property
    /// the old sleep timer had, generalized. 0 = until changed (no timer).
    /// A mode with no entry falls back to `sleep_hours`, which keeps an old
    /// app's SetSleepHours meaningful.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mode_hours: BTreeMap<String, u32>,
    /// How long a button-press sleep lasts before the board wakes itself.
    /// LEGACY as a concept (superseded by `mode_hours`) but load-bearing as
    /// the fallback duration and as the field old apps still write.
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
    /// Board-local IANA timezone name ("America/New_York"); None = server-local.
    /// Set automatically by the app (`Command::SetTimezone`) so the idle clock
    /// and game start-time labels track where the BOARD lives.
    #[serde(default)]
    pub timezone: Option<String>,
    /// A nightly window during which the board sleeps. None = never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_hours: Option<QuietHours>,
    /// Let live games light the panel during the window.
    ///
    /// The point of quiet hours is a dark bedroom, not a missed race. With
    /// this on, a game that is actually happening still shows; when nothing is
    /// on, the panel stays dark rather than falling back to the clock.
    #[serde(default, skip_serializing_if = "is_false")]
    pub quiet_allow_events: bool,
    /// LIVE state: the user asked to see the board anyway, for the rest of
    /// tonight's window.
    ///
    /// A bool, cleared when the window ends — deliberately not "override until
    /// T". The window is wall-clock, so any absolute instant stored against it
    /// would drift an hour twice a year, which is the exact failure evaluating
    /// in wall-clock avoids.
    #[serde(default, skip_serializing_if = "is_false")]
    pub quiet_override: bool,
    /// Firmware release channel this board follows (`Command::SetFwChannel`).
    /// Durable per-board config, but deliberately NOT part of
    /// `PortableSettings` — see the note there.
    #[serde(default)]
    pub fw_channel: FwChannel,
    /// Serve this board the synthetic slate instead of live games — the whole
    /// demo set, on the same poll cadence. Everything else about the board
    /// (favorites, dwell, name styles, off state, timezone, spoilers) is
    /// untouched: the toggle swaps the GAMES, not the settings.
    ///
    /// `#[serde(default)]` is load-bearing, not decoration: `load_configs`
    /// drops a row whose JSON fails to parse WITHOUT logging, so a field with
    /// no default would silently factory-reset every stored board on deploy.
    #[serde(default, skip_serializing_if = "is_false")]
    pub demo_data: bool,
}

fn default_sleep_hours() -> u32 {
    24
}

fn default_mode() -> String {
    MODE_SCORES.to_string()
}

/// The two mode ids that are NOT off-mode ids, reserved by the server's
/// SetMode validation. Everything else in the mode namespace is an off-mode
/// catalog id.
pub const MODE_SCORES: &str = "scores";
pub const MODE_OFF: &str = "off";

fn default_dwell_secs() -> u32 {
    10
}

fn default_brightness() -> u8 {
    80
}

/// Version stamped into every settings export (`scoreboard_settings` field).
/// A nightly window during which the scoreboard sleeps, in minutes since the
/// BOARD's local midnight. Half-open `[start, end)`.
///
/// `start == end` means DISABLED, never "always". An off-by-one that darkens a
/// board around the clock is the worst thing this feature could do, so the
/// degenerate case fails safe.
///
/// Minutes-since-local-midnight, not an instant: a window is a wall-clock
/// intent ("quiet after 10pm"), and any absolute timestamp derived from it
/// slides an hour twice a year.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuietHours {
    pub start_min: u16,
    pub end_min: u16,
}

impl QuietHours {
    pub fn enabled(&self) -> bool {
        self.start_min != self.end_min
    }

    /// Is `minute_of_day` inside the window? Midnight crossing is one branch
    /// rather than a date calculation — which is the whole point of storing
    /// minutes since local midnight.
    pub fn contains(&self, minute_of_day: u16) -> bool {
        if !self.enabled() {
            return false;
        }
        if self.start_min < self.end_min {
            self.start_min <= minute_of_day && minute_of_day < self.end_min
        } else {
            minute_of_day >= self.start_min || minute_of_day < self.end_min
        }
    }
}

/// Expand a legacy single option into the exact set of rounds it covered.
///
/// Derived literally from the old `champ_option_covers` / `f1_option_covers`
/// tables, so every already-configured board keeps the behaviour it had.
///
/// The wildcards (`playoffs`, and cfb's `playoff`) are ENUMERATED here rather
/// than carried across as a wildcard member. That is a deliberate behaviour
/// change with a cost: those ids used to absorb any tier added later, and now
/// they will not — a new tier must also be added to the round list, and to any
/// board holding the sport's full set. The alternative keeps exactly the hidden
/// implication this change exists to remove.
pub fn expand_legacy_champ(sport_id: &str, legacy: Option<&str>) -> Vec<String> {
    let ids: &[&str] = match (sport_id, legacy) {
        (_, None) | (_, Some("none")) => &[],
        (_, Some("all")) => &["all"],
        ("nhl", Some("final")) => &["final"],
        ("nhl", Some("conf")) => &["conf", "final"],
        ("nhl", Some("playoffs")) => &["final", "conf", "playoffs"],
        ("nba" | "wnba", Some("final")) => &["final"],
        ("nba" | "wnba", Some("playoffs")) => &["final", "playoffs"],
        ("mlb", Some("ws")) => &["ws"],
        ("mlb", Some("playoffs")) => &["ws", "playoffs"],
        ("nfl", Some("sb")) => &["sb"],
        ("nfl", Some("playoffs")) => &["sb", "playoffs"],
        // The CFP option covered bowls too, and so did "playoffs".
        ("cfb", Some("playoff")) | ("cfb", Some("playoffs")) => &["playoff", "bowl", "playoffs"],
        ("cfb", Some("bowl")) => &["bowl"],
        ("cbb" | "wcbb", Some("tournament")) => &["tournament"],
        ("cbb" | "wcbb", Some("playoffs")) => &["tournament", "playoffs"],
        ("mls", Some("cup")) => &["cup"],
        ("mls", Some("playoffs")) => &["cup", "playoffs"],
        // The Women's World Cup runs the same rounds as the men's, so it shares
        // the ladder; the Champions League has no third-place match.
        ("wc" | "wwc", Some("final")) => &["final"],
        ("wc" | "wwc", Some("semi")) => &["semi", "final"],
        // "third" joined the ladder when the classifier learned ESPN spells the
        // bronze-medal game "3rd-place"; the old knockout rung covered every
        // match after the group stage, so it covers that one too.
        ("wc" | "wwc", Some("knockout")) | ("wc" | "wwc", Some("playoffs")) => {
            &["knockout", "quarter", "semi", "third", "final"]
        }
        ("ucl", Some("final")) => &["final"],
        ("ucl", Some("semi")) => &["semi", "final"],
        ("ucl", Some("knockout")) | ("ucl", Some("playoffs")) => {
            &["knockout", "quarter", "semi", "final"]
        }
        ("cbase", Some("cws")) => &["cws"],
        ("cbase", Some("playoffs")) => &["cws", "playoffs"],
        // The Olympic medal ladder, in the same order `legacy_champ_projection`
        // ranks it, so projecting a set and expanding it again returns the set
        // rather than dropping half of it. Wanting the bronze game implies
        // wanting the gold one.
        ("olyhm" | "olyhw" | "olybm" | "olybw" | "olysm" | "olysw", Some("gold")) => &["gold"],
        ("olyhm" | "olyhw" | "olybm" | "olybw" | "olysm" | "olysw", Some("bronze")) => {
            &["bronze", "gold"]
        }
        ("olyhm" | "olyhw" | "olybm" | "olybw" | "olysm" | "olysw", Some("semi")) => {
            &["semi", "bronze", "gold"]
        }
        ("olyhm" | "olyhw" | "olybm" | "olybw" | "olysm" | "olysw", Some("quarter")) => {
            &["quarter", "semi", "bronze", "gold"]
        }
        // The option id was "majors"; the tier is "major". They never matched,
        // so the old option only ever worked through the wildcard.
        ("pga" | "lpga", Some("majors")) => &["major"],
        ("f1", Some("gp")) => &["race"],
        ("f1", Some("gp-quali")) => &["race", "quali"],
        ("f1", Some("gp-quali-sprint")) => &["race", "quali", "sprint", "sprint-quali"],
        (_, Some("playoffs")) => &["playoffs"],
        // Preserves the old `_ => option_id == tier` fallthrough for anything
        // unrecognized, including a fork's.
        (_, Some(other)) => {
            let mut v = Vec::new();
            v.push(other.to_string());
            return v;
        }
    };
    ids.iter().map(|s| s.to_string()).collect()
}

/// The single legacy id that most nearly means `ids`, for readers that only
/// understand one.
///
/// `None` exactly when the set is empty — that equivalence is what makes
/// re-running the migration a no-op.
pub fn legacy_champ_projection(sport_id: &str, ids: &[String]) -> Option<String> {
    if ids.is_empty() {
        return None;
    }
    let has = |x: &str| ids.iter().any(|i| i == x);
    if has("all") {
        return Some("all".to_string());
    }
    // Broadest first: the inverse of the expansion above.
    let broadest: &[&str] = match sport_id {
        "nhl" => &["playoffs", "conf", "final"],
        "wc" | "wwc" => &["knockout", "quarter", "semi", "third", "final"],
        "ucl" => &["knockout", "quarter", "semi", "final"],
        "f1" => &["sprint", "quali", "race"],
        "cfb" => &["playoff", "bowl"],
        "pga" | "lpga" => &["major"],
        "cbase" => &["playoffs", "cws"],
        // The Olympic tournaments: bronze is the broader of the two medal
        // games, since a viewer who wants it wants the gold one as well.
        "olyhm" | "olyhw" | "olybm" | "olybw" | "olysm" | "olysw" => {
            &["quarter", "semi", "bronze", "gold"]
        }
        _ => &["playoffs", "final", "ws", "sb", "cup", "tournament"],
    };
    let pick = broadest.iter().find(|x| has(x)).copied();
    Some(match (sport_id, pick) {
        ("pga" | "lpga", Some("major")) => "majors".to_string(),
        ("f1", Some("sprint")) => "gp-quali-sprint".to_string(),
        ("f1", Some("quali")) => "gp-quali".to_string(),
        ("f1", Some("race")) => "gp".to_string(),
        ("wc", Some("quarter")) | ("wc", Some("third")) => "knockout".to_string(),
        // NOTHING may project a narrow round set onto "all". These leagues only
        // ever offer an old app "none" or "all", and projecting to the blanket
        // looks harmless — until you follow what an old app DOES with the value:
        // it echoes the legacy field back on the next favourites edit, `ws.rs`
        // finds no `championship_option_ids`, and `expand_legacy_champ` turns
        // "all" into the whole league. Someone following only the gold medal
        // game would have had every Olympic game pushed onto their board by
        // swiping away a favourite team. Falling through to the generic arm
        // below names the round instead, which round-trips through the
        // `(_, Some(other))` wildcard untouched.
        (_, Some(p)) => p.to_string(),
        // A set of only forked/unknown ids: name the first, so a non-empty set
        // never projects to None and break the invariant above.
        (_, None) => ids[0].clone(),
    })
}

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
///
/// `demo_data` is excluded for the same shape of reason, more sharply:
/// importing settings must never put a real scoreboard into demo mode. Fake
/// scores on a panel someone trusts is exactly the failure the honest
/// empty-slate rule in `poll_loop` exists to prevent.
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
    /// Per-mode timer durations — durable preferences, like `sleep_hours`.
    /// The current MODE stays out of the blob for the same reason power and
    /// wake_at do: it is live state, and an import that yanked a board into
    /// "off" would be the demo_data-shaped failure those exclusions prevent.
    /// `#[serde(default)]`, and NO version bump: the import check is exact
    /// equality, so a bump would kill every existing export.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mode_hours: BTreeMap<String, u32>,
    /// What the panel shows while the display is off / idle. Legacy twin of
    /// `off_mode`, kept so an export stays readable by an older binary.
    #[serde(default)]
    pub off_state: OffState,
    /// The declared off mode. Absent in blobs exported before this existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_mode: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub off_mode_params: BTreeMap<String, String>,
    /// The nightly quiet window — a durable preference, like `sleep_hours`.
    /// `quiet_override` is live state and deliberately stays behind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_hours: Option<QuietHours>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub quiet_allow_events: bool,
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
            mode_hours: cfg.mode_hours.clone(),
            off_state: cfg.off_state,
            off_mode: cfg.off_mode.clone(),
            off_mode_params: cfg.off_mode_params.clone(),
            quiet_hours: cfg.quiet_hours,
            quiet_allow_events: cfg.quiet_allow_events,
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
        cfg.mode_hours = self.mode_hours.clone();
        cfg.off_state = self.off_state;
        // An export from a FORKED server can name a mode this one does not
        // offer. Carry it across anyway and let the server re-clamp it against
        // its own catalog — the same shape as the brightness/dwell re-clamp.
        cfg.off_mode = self.off_mode.clone();
        cfg.off_mode_params = self.off_mode_params.clone();
        cfg.quiet_hours = self.quiet_hours;
        cfg.quiet_allow_events = self.quiet_allow_events;
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

impl BoardConfig {
    /// The off mode to render: the declared id, else the legacy enum. Every
    /// server-side reader goes through this, so a config written before
    /// `off_mode` existed keeps behaving exactly as it did.
    pub fn off_mode_id(&self) -> &str {
        self.off_mode.as_deref().unwrap_or(match self.off_state {
            OffState::Blank => "blank",
            OffState::Clock => "clock",
        })
    }

    /// Write BOTH fields. `draws_nothing` says whether this mode leaves the
    /// panel dark, which is all the legacy enum was ever able to express.
    pub fn set_off_mode(&mut self, id: &str, draws_nothing: bool) {
        self.off_mode = Some(id.to_string());
        self.off_state = if draws_nothing { OffState::Blank } else { OffState::Clock };
    }

    /// Write the mode AND its legacy twin. `power` has no serde default — it
    /// is one of the three fields every old parser REQUIRES — so it keeps
    /// being written forever, derived: lit unless the mode is "off". Dropping
    /// it from serialization would make a rolled-back server quarantine every
    /// stored board and a fielded app fail its whole BoardState decode.
    ///
    /// Same twin pattern as `set_off_mode`/`off_state`, for the same reason.
    pub fn set_mode(&mut self, mode: &str) {
        self.mode = mode.to_string();
        self.power = mode != MODE_OFF;
    }

    /// A mode's timer, in hours. 0 = until changed.
    pub fn mode_duration_hours(&self, mode: &str) -> u32 {
        self.mode_hours.get(mode).copied().unwrap_or(self.sleep_hours)
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
            off_mode: None,
            off_mode_params: BTreeMap::new(),
            quiet_hours: None,
            quiet_allow_events: false,
            quiet_override: false,
            mode: default_mode(),
            mode_hours: BTreeMap::new(),
            sleep_hours: default_sleep_hours(),
            dwell_secs: default_dwell_secs(),
            wake_at: None,
            spoiler_mode: SpoilerMode::default(),
            spoiler_all: false,
            spoiler_game_ids: Vec::new(),
            revealed_game_ids: Vec::new(),
            timezone: None,
            fw_channel: FwChannel::default(),
            demo_data: false,
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
        /// LEGACY: what an already-installed app still sends. Folded into the
        /// set ONLY when `championship_option_ids` is absent.
        #[serde(default)]
        championship_option_id: Option<String>,
        /// `Option`, not a plain `Vec`, and this is the sharpest compatibility
        /// hazard in the change: an old app sends only the singular field on
        /// every favourites edit, and a bare `#[serde(default)] Vec` would
        /// parse that as an EMPTY SET and silently wipe the user's rounds.
        /// `None` = the sender never knew the field; `Some([])` = deselect all.
        #[serde(default)]
        championship_option_ids: Option<Vec<String>>,
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
    /// Put the board in a MODE: "scores", "off", or an off-mode id held
    /// persistently. Non-scores modes run for their configured duration
    /// (`SetModeHours`) and then return to scores. Replaces SetPower, which
    /// stays as an accepted alias for already-installed apps.
    SetMode { mode: String },
    /// Configure a mode's timer, in hours. 0 = until changed.
    SetModeHours { mode: String, hours: u32 },
    /// How long a button-press sleep lasts (hours) before auto-wake.
    /// LEGACY alias: the fallback duration for modes `mode_hours` misses.
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
    /// Golf's round indicator on/off for one league. `None` reverts to the
    /// default (shown).
    SetLeagueShowRound { sport_id: String, show: Option<bool> },
    /// Box-score extras for a league (baseball H/E, NHL shots, soccer
    /// shootout/aggregate). `None` reverts to the default, which is shown.
    SetLeagueShowExtras { sport_id: String, show: Option<bool> },
    /// Ask the board to stream (or stop streaming) its console to the server
    /// for `minutes`. DEBUG builds of the app only — see
    /// `ServerMsg::SetDebugLog`.
    SetDebugLog { on: bool, minutes: u32 },
    /// Per-league team-name display style (city/college/country vs mascot vs
    /// abbreviation); None reverts the league to its default
    /// ([`default_name_style`]).
    SetLeagueNameStyle { sport_id: String, style: Option<NameStyle> },
    /// How spoiler-protected games render (strict vs normal).
    SetSpoilerMode { mode: SpoilerMode },
    /// Spoiler-protect every game on the board.
    SetSpoilerAll { on: bool },
    /// Swap this board between live games and the synthetic demo slate.
    SetDemoData { on: bool },
    /// Set the nightly quiet window (minutes since board-local midnight).
    /// `start_min == end_min` clears it.
    SetQuietHours { start_min: u16, end_min: u16 },
    ClearQuietHours,
    /// Whether live games may light the panel during the quiet window.
    SetQuietAllowEvents { on: bool },
    /// Pick a server-declared off mode by id. The server validates it against
    /// its own catalog and ignores an id it does not offer.
    SetOffMode { id: String },
    /// Set one off-mode input value (`OffModeInput::key`).
    SetOffModeParam { key: String, value: String },
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
    NeedFrames {
        ids: Vec<FrameId>,
        /// Frame payload codecs THIS CLIENT can decode ([`FRAME_CODEC_RAW`],
        /// [`FRAME_CODEC_PAL_RLE`], ...). The sender advertises its own
        /// decoder, so the answer never depends on guessing a firmware
        /// version — a board that just OTA'd is described by the request it
        /// sends, not by what the server last heard about it. Absent or empty
        /// (old firmware, the app) means RAW ONLY; the server may always
        /// answer with raw regardless of what's listed here.
        #[serde(default)]
        codecs: Vec<u8>,
    },
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
    // device announces it is factory-resetting RIGHT NOW (best-effort, sent
    // in the second before the wipe+reboot when a socket happens to be up):
    // the server deletes everything it knows about the board — pairings,
    // config, version — so it returns as a brand-new scoreboard. The device
    // also wipes its own identity seed, so even an OFFLINE reset comes back
    // under a fresh board_id; this message just keeps the server's tables
    // from accumulating orphans and unpairs the old id from every phone.
    FactoryReset,
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
    // device asks for whatever its channel would currently offer — the exact
    // pick the app's Update button runs. Update offers were app-initiated
    // only; the on-device settings menu needs to ask without a phone in the
    // room. Nothing newer hosted is a quiet no-op server-side (no offer, no
    // error) — the answer the menu can DISPLAY already arrived as
    // `ServerMsg::UpdateAvailable`.
    RequestUpdate,
    // controller (DEBUG firmware picker) asks for every firmware image the
    // server hosts, across ALL channels regardless of board enrollment —
    // answered by `ServerMsg::FirmwareList`
    ListFirmware,
    /// Device console lines, batched, sent only while the server has asked for
    /// them (`ServerMsg::SetDebugLog`). The board prints these to its UART
    /// regardless; this is the same text taking the socket instead, so a board
    /// nowhere near a USB cable can still be read.
    DebugLog { lines: Vec<String> },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "t")]
pub enum ServerMsg {
    Hello { challenge: String },
    AuthOk,
    /// The board's local wall clock, as milliseconds since the Unix epoch with
    /// the board's timezone offset ALREADY APPLIED. The board has no RTC and no
    /// tz database, so it gets an answer, not the inputs to compute one — and
    /// DST transitions arrive as an ordinary update rather than needing rules
    /// on device.
    ///
    /// This SETS the time; it does not drive the display. The board anchors it
    /// to its own monotonic clock and advances it itself, so the panel keeps
    /// ticking with the network unplugged.
    ///
    /// Safe to send to old firmware: an unknown variant fails to deserialize
    /// and is dropped by the catch-all arm in the message loop.
    Time { local_epoch_ms: u64 },
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
        /// True while the board is inside its quiet window. DERIVED per push
        /// and never stored — the window is the durable thing, this is just
        /// what it means right now.
        #[serde(default, skip_serializing_if = "is_false")]
        quiet_active: bool,
    },
    CommandAck {
        req_id: u64,
        ok: bool,
        error: Option<String>,
        /// Where to read this board's console, for a `SetDebugLog` that turned
        /// it on. Carries a single-use-window token rather than the board id —
        /// see `debug_tokens` — so the URL is the capability and it expires
        /// with the window. Absent for every other command.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        debug_url: Option<String>,
    },
    // pairing results (to a controller)
    Paired { board_id: String },
    PairFailed { reason: String },
    // to a device: you've been paired, leave setup
    SetupDone,
    // server tells a freshly-authed device that NO controller is paired with
    // it (first startup after provisioning, or every pairing was revoked):
    // the board puts its pair code up, sticky, until SetupDone or a gesture.
    // The code itself was already registered — devices send EnterSetup on
    // every connect — so this is pure presentation. Old firmware drops the
    // unknown variant harmlessly and keeps its old behavior (code in the
    // menu only).
    NeedsPairing,
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
    // reply to `ClientMsg::ListFirmware`: every firmware image the server hosts,
    // one per channel that has a current dist (the DEBUG picker's full menu).
    FirmwareList { images: Vec<FwImage> },
    // to a DEVICE: the version a `RequestUpdate` would install right now;
    // None = up to date. The None answer is a real push, not silence — the
    // on-device menu must be able to tell "up to date" from "never heard
    // back". DISPLAY-ONLY: the actual offer stays the signed `UpdateFirmware`
    // message above (this carries no size/sig/path, so it can never install
    // anything). Pushed after every `DeviceVersion` report (each reconnect
    // re-reports, so the report doubles as the ask) and again when a channel
    // switch changes the answer for a connected board. Old firmware fails to
    // parse the unknown variant and drops the message — that must remain
    // harmless, so the server never waits on any reaction to it.
    /// Start or stop streaming the device console to the server, for `secs`.
    ///
    /// Off by default, never persisted, and self-limiting: the board switches
    /// itself off when the window passes. Indefinite streaming would cost a
    /// board work nobody is reading and hold a window open on the server for no
    /// reason — so the window is required, not optional.
    SetDebugLog { on: bool, secs: u32 },
    UpdateAvailable {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
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
//   [ 64 bytes ASCII hex frame_id ][ payload ]
// The payload is produced by [`encode_frame_payload`] and is self-describing:
// its first byte names the codec, so the framing below never has to know
// whether the pixels are raw or compressed.
pub const FRAME_ID_HEX_LEN: usize = 64;

pub fn encode_frame(id: &str, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(FRAME_ID_HEX_LEN + payload.len());
    v.extend_from_slice(id.as_bytes());
    v.extend_from_slice(payload);
    v
}

pub fn decode_frame(buf: &[u8]) -> Option<(FrameId, &[u8])> {
    if buf.len() < FRAME_ID_HEX_LEN {
        return None;
    }
    let id = core::str::from_utf8(&buf[..FRAME_ID_HEX_LEN]).ok()?.to_string();
    Some((id, &buf[FRAME_ID_HEX_LEN..]))
}

// ---- frame payload codecs ----
//
// A 64x32 frame is 4096 bytes raw, and the device caches up to 20 of them —
// ~80KB of a 160KB heap that the WiFi radio shares. Rendered frames are flat
// synthetic graphics (3-8 distinct colors each), so a 16-color palette at one
// index per pixel plus run-length coding shrinks them ~5x. Crucially the
// DECODER needs no sliding window and no scratch buffer: just the palette and
// the output. That is why this beats deflate here, which compresses better but
// wants a 32KB inflate dictionary — more heap than the frames it saves.

/// Uncompressed payload: the RGB565-LE pixels verbatim after the codec byte.
/// Bytes per pixel row (64 px * 2 B). Load-bearing beyond geometry: it is the
/// modulus that separates a legacy bare-pixel payload from a self-describing one.
pub const FRAME_ROW_BYTES: usize = FRAME_WIDTH * 2;

/// Is this binary-frame payload from a PRE-CODEC server — i.e. bare RGB565
/// pixels with no leading codec byte?
///
/// Servers that predate the codec send exactly `height * FRAME_ROW_BYTES`
/// bytes, always a whole number of rows. `encode_frame_payload` guarantees it
/// never emits that length (raw payloads are `1 + height * FRAME_ROW_BYTES`,
/// and compressed candidates landing on a row multiple are skipped), so the
/// length alone is a reliable discriminator.
///
/// This exists because a board can outlive the server that taught it the
/// codec: firmware dists ship inside the server image and the downgrade guard
/// refuses older versions, so rolling the server back leaves updated boards
/// talking to a server that only speaks bare pixels. Without this check the
/// board reads the first PIXEL byte as a codec — a zero byte decodes as RAW
/// and silently shifts every pixel by one, which is worse than an error.
pub fn frame_payload_is_legacy_raw(payload: &[u8]) -> bool {
    !payload.is_empty() && payload.len() % FRAME_ROW_BYTES == 0
}

pub const FRAME_CODEC_RAW: u8 = 0;
/// 16-color palette + run-length coded indices (see [`encode_frame_payload`]).
pub const FRAME_CODEC_PAL_RLE: u8 = 1;

/// Palette entries a [`FRAME_CODEC_PAL_RLE`] payload may carry.
pub const MAX_PALETTE_COLORS: usize = 16;

/// Largest frame either codec will produce or accept, in bytes: 64 wide by 256
/// rows tall. Frames are variable height — a 20-row leaderboard renders 128
/// rows and scrolls — so this is generous headroom over anything the renderer
/// makes, while still bounding what a corrupt or hostile payload can make a
/// 160KB-heap device allocate.
pub const MAX_FRAME_BYTES: usize = FRAME_WIDTH * 256 * 2;

/// Encode pixels into a self-describing payload: `[codec u8][data...]`.
///
/// `accepted` lists the codecs the RECIPIENT can decode (see
/// [`ClientMsg::NeedFrames`]); every accepted codec is tried and the SMALLEST
/// result wins. [`FRAME_CODEC_RAW`] is always in play and always the fallback,
/// so an empty `accepted` (old firmware, the app) yields raw.
///
/// # `FRAME_CODEC_PAL_RLE` layout
///
/// ```text
/// [0]                      codec byte = FRAME_CODEC_PAL_RLE
/// [1]                      color_count, 1..=16
/// [2 .. 2+2*color_count]   palette: color_count * u16 little-endian RGB565
/// [2+2*color_count ..]     RLE pairs: [run_len u8, 1..=255][palette_index u8]
/// ```
///
/// There is no length field: the decoder derives the pixel count by summing
/// run lengths, and refuses anything summing past [`MAX_FRAME_BYTES`]. Runs
/// longer than 255 pixels are split across consecutive pairs.
///
/// The encoder falls back to raw when the frame has more than
/// [`MAX_PALETTE_COLORS`] distinct colors, when it is not a whole number of
/// pixels or is larger than [`MAX_FRAME_BYTES`], and — the case that matters —
/// whenever the encoded form would not be strictly smaller than the raw
/// payload. Alternating single-pixel runs EXPAND under RLE; that must never go
/// on the wire.
pub fn encode_frame_payload(rgb565_le: &[u8], accepted: &[u8]) -> Vec<u8> {
    // The payload raw would produce; nothing bigger than this may ship.
    let raw_len = rgb565_le.len() + 1;
    let mut best: Option<Vec<u8>> = None;

    for &codec in accepted {
        let candidate = match codec {
            FRAME_CODEC_PAL_RLE => encode_pal_rle(rgb565_le),
            // RAW is the fallback below; unknown codecs are simply not spoken.
            _ => None,
        };
        let Some(candidate) = candidate else { continue };
        if candidate.len() >= raw_len {
            continue; // never ship an expansion (or a wash)
        }
        // A payload whose length is a whole number of pixel rows is
        // INDISTINGUISHABLE from what a pre-codec server sends (bare pixels,
        // always height * FRAME_ROW_BYTES). Skip those lengths so the receiver
        // can tell the two apart by length alone — see `frame_payload_is_legacy_raw`.
        // Costs compression on ~1/128 of frames; buys rollback safety.
        if candidate.len() % FRAME_ROW_BYTES == 0 {
            continue;
        }
        let better = match &best {
            Some(b) => candidate.len() < b.len(),
            None => true,
        };
        if better {
            best = Some(candidate);
        }
    }

    best.unwrap_or_else(|| {
        let mut v = Vec::with_capacity(raw_len);
        v.push(FRAME_CODEC_RAW);
        v.extend_from_slice(rgb565_le);
        v
    })
}

/// Inverse of [`encode_frame_payload`]. `None` on malformed input, an unknown
/// codec, or a length mismatch (a truncated palette, a dangling half-pair, a
/// run pointing outside the palette, or a total exceeding
/// [`MAX_FRAME_BYTES`]).
pub fn decode_frame_payload(payload: &[u8]) -> Option<Vec<u8>> {
    let (&codec, rest) = payload.split_first()?;
    match codec {
        FRAME_CODEC_RAW => Some(rest.to_vec()),
        FRAME_CODEC_PAL_RLE => decode_pal_rle(rest),
        _ => None,
    }
}

/// Build a palette+RLE payload, or `None` when this frame can't use the codec.
fn encode_pal_rle(rgb565_le: &[u8]) -> Option<Vec<u8>> {
    // Whole pixels only, non-empty, and no bigger than the decoder accepts —
    // otherwise the round trip wouldn't hold.
    if rgb565_le.is_empty()
        || !rgb565_le.chunks_exact(2).remainder().is_empty()
        || rgb565_le.len() > MAX_FRAME_BYTES
    {
        return None;
    }

    let mut palette: Vec<u16> = Vec::new();
    let mut body: Vec<u8> = Vec::new();
    let mut run_index: u8 = 0;
    let mut run_len: u8 = 0;

    for px in rgb565_le.chunks_exact(2) {
        let color = u16::from_le_bytes([px[0], px[1]]);
        let index = match palette.iter().position(|&c| c == color) {
            Some(i) => i as u8,
            None => {
                if palette.len() == MAX_PALETTE_COLORS {
                    return None; // too many colors for a 4bpp palette
                }
                palette.push(color);
                (palette.len() - 1) as u8
            }
        };

        if run_len > 0 && index == run_index && run_len < 255 {
            run_len += 1;
        } else {
            if run_len > 0 {
                body.push(run_len);
                body.push(run_index);
            }
            run_index = index;
            run_len = 1;
        }

        // Already lost to raw and it can only get worse — stop paying for it.
        if body.len() > rgb565_le.len() {
            return None;
        }
    }
    if run_len > 0 {
        body.push(run_len);
        body.push(run_index);
    }

    let mut out = Vec::with_capacity(2 + palette.len() * 2 + body.len());
    out.push(FRAME_CODEC_PAL_RLE);
    out.push(palette.len() as u8);
    for color in &palette {
        out.extend_from_slice(&color.to_le_bytes());
    }
    out.extend_from_slice(&body);
    Some(out)
}

/// Decode the bytes AFTER the [`FRAME_CODEC_PAL_RLE`] codec byte.
fn decode_pal_rle(body: &[u8]) -> Option<Vec<u8>> {
    let (&count, rest) = body.split_first()?;
    let count = count as usize;
    if count == 0 || count > MAX_PALETTE_COLORS {
        return None;
    }
    if rest.len() < count * 2 {
        return None; // truncated palette
    }
    let (palette_bytes, runs) = rest.split_at(count * 2);
    if runs.is_empty() || !runs.chunks_exact(2).remainder().is_empty() {
        return None; // no runs at all, or a dangling half-pair
    }

    // Validate and total the runs BEFORE allocating: on a 160KB heap a corrupt
    // payload must not be able to ask for an unbounded buffer.
    let mut total: usize = 0;
    for pair in runs.chunks_exact(2) {
        let (len, index) = (pair[0] as usize, pair[1] as usize);
        if len == 0 || index >= count {
            return None;
        }
        total += len * 2;
        if total > MAX_FRAME_BYTES {
            return None;
        }
    }

    let palette: Vec<u16> =
        palette_bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();

    let mut out = Vec::with_capacity(total);
    for pair in runs.chunks_exact(2) {
        let color = palette[pair[1] as usize].to_le_bytes();
        for _ in 0..pair[0] {
            out.extend_from_slice(&color);
        }
    }
    Some(out)
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

    fn descriptor() -> OffModeDescriptor {
        OffModeDescriptor {
            id: "weather".into(),
            label: "Weather".into(),
            description: "".into(),
            preview: None,
            input: None,
            fields: vec![
                OffModeField {
                    key: "location".into(), label: "Location".into(), kind: "text".into(),
                    placeholder: "ZIP".into(), max_len: 5, default: String::new(),
                    options: vec![], visible_when: None,
                },
                OffModeField {
                    key: "variant".into(), label: "Layout".into(), kind: "choice".into(),
                    placeholder: String::new(), max_len: 0, default: "simple".into(),
                    options: vec![
                        OffModeChoice { value: "simple".into(), label: "Simple".into(), preview: None },
                        OffModeChoice { value: "clock".into(), label: "Clock".into(), preview: None },
                    ],
                    visible_when: None,
                },
                OffModeField {
                    key: "hour24".into(), label: "24-hour".into(), kind: "toggle".into(),
                    placeholder: String::new(), max_len: 0, default: "0".into(),
                    options: vec![],
                    visible_when: Some(FieldCondition { key: "variant".into(), equals: "clock".into() }),
                },
            ],
        }
    }

    /// Defaults must NOT be written into the stored map. Storing them would
    /// freeze a field at whatever the server happened to offer the first time
    /// someone opened the screen, so changing a default could never reach
    /// anyone who had visited it.
    #[test]
    fn an_unset_field_resolves_to_its_declared_default() {
        let d = descriptor();
        let empty = BTreeMap::new();
        assert_eq!(d.value("variant", &empty), "simple");
        assert_eq!(d.value("location", &empty), "");
        assert_eq!(d.value("nonexistent", &empty), "");
        let mut set = BTreeMap::new();
        set.insert("variant".into(), "clock".to_string());
        assert_eq!(d.value("variant", &set), "clock");
    }

    /// The conditional field in the brief: the 24-hour toggle exists only while
    /// the clock layout is chosen — including when that choice is the DEFAULT
    /// rather than something stored.
    #[test]
    fn a_conditional_field_follows_the_resolved_value_not_just_the_stored_one() {
        let d = descriptor();
        let mut p = BTreeMap::new();
        assert_eq!(d.visible_fields(&p).len(), 2, "hour24 hidden under the default layout");
        p.insert("variant".into(), "clock".to_string());
        assert_eq!(d.visible_fields(&p).len(), 3, "and appears once clock is picked");
        p.insert("variant".into(), "simple".to_string());
        assert_eq!(d.visible_fields(&p).len(), 2);
    }

    #[test]
    fn validation_rejects_what_the_app_could_never_have_sent() {
        let d = descriptor();
        assert!(d.validate("variant", "simple").is_ok());
        assert!(d.validate("variant", "banana").is_err(), "not an offered choice");
        assert!(d.validate("hour24", "1").is_ok());
        assert!(d.validate("hour24", "yes").is_err(), "a toggle is 0 or 1");
        assert!(d.validate("nope", "x").is_err(), "undeclared keys never land in the map");
        // Characters, not bytes: the app clamps with String.prefix.
        assert!(d.validate("location", "60601").is_ok());
        assert!(d.validate("location", "606011").is_err());
        assert!(d.validate("location", "\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}").is_ok(), "5 chars, 10 bytes");
    }

    /// A window is a wall-clock intent, so the degenerate and wrapping cases
    /// are the ones worth pinning.
    #[test]
    fn a_quiet_window_wraps_midnight_and_fails_safe_when_degenerate() {
        let night = QuietHours { start_min: 22 * 60, end_min: 7 * 60 };
        assert!(night.contains(23 * 60));
        assert!(night.contains(3 * 60));
        assert!(night.contains(22 * 60), "inclusive at the start");
        assert!(!night.contains(7 * 60), "exclusive at the end");
        assert!(!night.contains(12 * 60));

        let day = QuietHours { start_min: 9 * 60, end_min: 17 * 60 };
        assert!(day.contains(12 * 60));
        assert!(!day.contains(3 * 60));

        // start == end is DISABLED. A board dark 24/7 is the worst outcome
        // available here, so the degenerate case must never mean "always".
        let degenerate = QuietHours { start_min: 600, end_min: 600 };
        assert!(!degenerate.enabled());
        for m in [0u16, 599, 600, 601, 1439] {
            assert!(!degenerate.contains(m), "minute {m} must not be quiet");
        }
    }

    /// The window travels between boards; the override does not.
    #[test]
    fn portable_settings_carry_the_window_but_not_the_override() {
        let source = BoardConfig {
            quiet_hours: Some(QuietHours { start_min: 1320, end_min: 420 }),
            quiet_override: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&PortableSettings::from(&source)).unwrap();
        assert!(!json.contains("override"), "live state must not travel: {json}");
        let mut target = BoardConfig::default();
        serde_json::from_str::<PortableSettings>(&json).unwrap().apply_to(&mut target);
        assert_eq!(target.quiet_hours, source.quiet_hours);
        assert!(!target.quiet_override);
    }

    /// CalVer is YYYY.M.P, so a month rollover RESETS the patch — and the
    /// comparison has to order that correctly, or a corrected version reads as
    /// a downgrade and no board takes it.
    ///
    /// Not hypothetical: releases kept climbing 2026.7.x for four days into
    /// August before anyone noticed the month had moved.
    #[test]
    fn a_month_rollover_outranks_a_high_patch_in_the_old_month() {
        assert!(version_newer("2026.8.0", "2026.7.23"));
        assert!(!version_newer("2026.7.24", "2026.8.0"));
        assert!(version_newer("2027.1.0", "2026.12.9"), "and a year rollover too");
    }

    /// Every league whose rounds this repo knows, for the two invariants below.
    const CHAMP_LEAGUES: &[&str] = &[
        "nhl", "nba", "wnba", "mlb", "nfl", "cfb", "cbb", "wcbb", "mls", "epl", "wc", "wwc", "ucl",
        "cbase", "olyhm", "olyhw", "olybm", "olybw", "olysm", "olysw", "pga", "lpga", "f1", "khl",
    ];

    /// The one that bit: a NARROW round set must never project onto the blanket.
    ///
    /// An old app is handed this projection in every board push and echoes it
    /// back on the next favourites edit, at which point `expand_legacy_champ`
    /// turns it into a subscription. Projecting "gold medal game" to "all"
    /// therefore meant swiping away a favourite team could put every game in the
    /// league on the board — a one-way trip, since the old app never knew the
    /// narrow set existed.
    #[test]
    fn no_narrow_round_set_projects_onto_the_blanket() {
        for sport in CHAMP_LEAGUES {
            for round in ["gold", "bronze", "semi", "quarter", "third", "final", "cws", "playoffs", "conf"] {
                let set = vec![round.to_string()];
                let projected = legacy_champ_projection(sport, &set);
                assert_ne!(
                    projected.as_deref(),
                    Some("all"),
                    "{sport}: following only {round:?} must not read as the whole league to an old app"
                );
                assert!(projected.is_some(), "{sport}/{round}: a non-empty set must project to something");
            }
        }
        // The blanket itself still projects to the blanket.
        assert_eq!(legacy_champ_projection("olyhm", &["all".to_string()]).as_deref(), Some("all"));
    }

    /// Expanding a projection must not quietly drop rounds the user picked.
    ///
    /// `expand(project(set))` is allowed to be BROADER than `set` — the legacy
    /// ladder is coarser than the checkboxes, and that widening is the old
    /// design's documented cost — but it may never lose a round, which would
    /// silently unsubscribe someone.
    #[test]
    fn expanding_a_projection_never_loses_a_round() {
        let cases: &[(&str, &[&str])] = &[
            ("olyhm", &["gold"]),
            ("olyhm", &["gold", "bronze"]),
            ("olyhm", &["gold", "bronze", "semi", "quarter"]),
            ("olysw", &["bronze", "gold"]),
            ("cbase", &["cws"]),
            ("cbase", &["cws", "playoffs"]),
            ("wc", &["third"]),
            ("wc", &["knockout", "quarter", "semi", "third", "final"]),
            ("wwc", &["final"]),
            ("ucl", &["semi", "final"]),
            ("nhl", &["final"]),
            ("f1", &["race"]),
        ];
        for (sport, ids) in cases {
            let set: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
            let projected = legacy_champ_projection(sport, &set).expect("non-empty projects");
            let back = expand_legacy_champ(sport, Some(&projected));
            for id in ids.iter() {
                assert!(
                    back.iter().any(|b| b == id),
                    "{sport}: {set:?} projected to {projected:?} and expanded to {back:?}, losing {id:?}"
                );
            }
        }
    }

    /// A config written before off modes existed must keep behaving exactly as
    /// it did — the legacy enum is the fallback, not a default guess.
    #[test]
    fn a_config_without_off_mode_falls_back_to_the_legacy_enum() {
        let clock: BoardConfig =
            serde_json::from_str(r#"{"brightness":80,"power":true,"sports":[],"off_state":"Clock"}"#)
                .unwrap();
        assert_eq!(clock.off_mode_id(), "clock");
        let blank: BoardConfig =
            serde_json::from_str(r#"{"brightness":80,"power":true,"sports":[],"off_state":"Blank"}"#)
                .unwrap();
        assert_eq!(blank.off_mode_id(), "blank");
    }

    /// The fleet-safety invariant. `Manifest::off_state` is a closed enum with
    /// no serde default, and firmware drops an unparseable message silently —
    /// so a forked mode id must never be able to reach a board. Writing the
    /// mode always writes the legacy twin, and the twin only ever says "dark"
    /// or "lit".
    #[test]
    fn setting_a_forked_mode_still_leaves_a_legacy_value_a_board_can_parse() {
        let mut cfg = BoardConfig::default();
        cfg.set_off_mode("stocks", false);
        assert_eq!(cfg.off_mode_id(), "stocks");
        assert_eq!(cfg.off_state, OffState::Clock, "a lit mode reads as Clock to old readers");
        let json = serde_json::to_string(&cfg).unwrap();
        // An OLD binary parsing this must succeed and see a sane off_state.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["off_state"], "Clock");

        cfg.set_off_mode("blank", true);
        assert_eq!(cfg.off_state, OffState::Blank, "a dark mode reads as Blank");
    }

    /// The power twin invariant, in the style of the forked-off-mode test
    /// above: EVERY mode write must leave a `power` bool an old parser can
    /// read, truthful about whether the panel is lit. `power` has no serde
    /// default — it is required — so a serialization that dropped it would
    /// quarantine every stored board on rollback.
    #[test]
    fn every_mode_write_leaves_a_power_bool_old_parsers_accept() {
        let mut cfg = BoardConfig::default();
        for (mode, lit) in [(MODE_OFF, false), ("clock", true), ("weather", true), (MODE_SCORES, true)] {
            cfg.set_mode(mode);
            assert_eq!(cfg.power, lit, "{mode}: power must say whether the panel is lit");
            let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
            assert_eq!(v["power"], lit, "{mode}: power must be IN the json, not defaulted");
        }
    }

    /// A row written by an old binary has no `mode` field: it must parse, and
    /// it must default to scores — the migration of sleeping rows is the
    /// server's job at load, not serde's.
    #[test]
    fn an_old_row_without_a_mode_parses_as_scores() {
        let old = r#"{"brightness":80,"power":false,"sports":[],"wake_at":123}"#;
        let cfg: BoardConfig = serde_json::from_str(old).unwrap();
        assert_eq!(cfg.mode, MODE_SCORES);
        assert!(!cfg.power);
        assert_eq!(cfg.wake_at, Some(123));
    }

    /// Timer durations are durable preferences and travel with an export; the
    /// current MODE is live state and must not.
    #[test]
    fn portable_settings_carry_durations_but_never_the_mode() {
        let mut source = BoardConfig::default();
        source.mode_hours.insert("clock".into(), 3);
        source.set_mode(MODE_OFF);
        let blob = PortableSettings::from(&source);
        let mut target = BoardConfig::default();
        blob.apply_to(&mut target);
        assert_eq!(target.mode_hours.get("clock"), Some(&3));
        assert_eq!(target.mode, MODE_SCORES, "an import must not change what the board is doing");
        assert!(target.power);
        // And the blob's json carries no mode key at all.
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&blob).unwrap()).unwrap();
        assert!(v.get("mode").is_none());
    }

    /// Off-mode selection travels with an export, so cloning a setup onto a
    /// second board carries the idle screen too.
    #[test]
    fn portable_settings_carry_the_off_mode() {
        let mut source = BoardConfig::default();
        source.set_off_mode("weather", false);
        source.off_mode_params.insert("zip".into(), "60601".into());
        let json = serde_json::to_string(&PortableSettings::from(&source)).unwrap();
        let mut target = BoardConfig::default();
        serde_json::from_str::<PortableSettings>(&json).unwrap().apply_to(&mut target);
        assert_eq!(target.off_mode_id(), "weather");
        assert_eq!(target.off_mode_params.get("zip").map(String::as_str), Some("60601"));
    }

    /// The guard against a silent config wipe. `load_configs` drops a row whose
    /// JSON fails to parse WITHOUT logging, so a `demo_data` with no serde
    /// default would make every board stored before this field shipped fail to
    /// parse — and each one would come back as a factory-fresh default, losing
    /// its name, favorites, brightness and timezone. Deploy-time data loss with
    /// no error message.
    #[test]
    fn config_missing_demo_data_defaults_to_false() {
        let cfg: BoardConfig =
            serde_json::from_str("{\"brightness\":80,\"power\":true,\"sports\":[]}").unwrap();
        assert!(!cfg.demo_data, "a board stored before demo_data existed must load as live");
    }

    /// A board that never touches the toggle must serialize byte-identically to
    /// before the field existed, so rolling the server back is clean.
    #[test]
    fn demo_data_off_is_not_written_to_disk() {
        let json = serde_json::to_string(&BoardConfig::default()).unwrap();
        assert!(!json.contains("demo"), "demo_data leaked into a default config: {json}");
    }

    /// Importing settings must never put a real scoreboard into demo mode.
    #[test]
    fn portable_settings_do_not_move_demo_data() {
        let source = BoardConfig { demo_data: true, brightness: 40, ..Default::default() };
        let json = serde_json::to_string(&PortableSettings::from(&source)).unwrap();
        assert!(!json.contains("demo"), "demo_data leaked into the export: {json}");

        let mut target = BoardConfig::default();
        let parsed: PortableSettings = serde_json::from_str(&json).unwrap();
        parsed.apply_to(&mut target);
        assert_eq!(target.brightness, 40, "durable prefs DO transfer");
        assert!(!target.demo_data, "a real board must not be flipped into demo by an import");

        // And the reverse: importing onto a demo board must not silently kick it
        // back to live data either.
        let mut demo_board = BoardConfig { demo_data: true, ..Default::default() };
        let live_blob = serde_json::to_string(&PortableSettings::from(&BoardConfig::default())).unwrap();
        serde_json::from_str::<PortableSettings>(&live_blob).unwrap().apply_to(&mut demo_board);
        assert!(demo_board.demo_data, "an import must not clear demo mode either");
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

    /// The on-device menu's ask + answer round-trip. `UpdateAvailable`'s None
    /// ("up to date") omits the field entirely on the wire, and a version-less
    /// message parses back to None — the skip/default pair must stay symmetric
    /// or an up-to-date answer would fail to parse on the board.
    #[test]
    fn request_update_and_update_available_round_trip() {
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(
                &serde_json::to_string(&ClientMsg::RequestUpdate).unwrap()
            )
            .unwrap(),
            ClientMsg::RequestUpdate
        ));

        let some = ServerMsg::UpdateAvailable { version: Some("2026.8.0".into()) };
        match serde_json::from_str::<ServerMsg>(&serde_json::to_string(&some).unwrap()).unwrap() {
            ServerMsg::UpdateAvailable { version } => {
                assert_eq!(version.as_deref(), Some("2026.8.0"))
            }
            other => panic!("expected UpdateAvailable, got {other:?}"),
        }

        let none = ServerMsg::UpdateAvailable { version: None };
        let json = serde_json::to_string(&none).unwrap();
        assert_eq!(json, "{\"t\":\"UpdateAvailable\"}", "None is skipped on the wire");
        match serde_json::from_str::<ServerMsg>(&json).unwrap() {
            ServerMsg::UpdateAvailable { version } => assert_eq!(version, None),
            other => panic!("expected UpdateAvailable, got {other:?}"),
        }
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
mod frame_codec_tests {
    use super::*;

    const BLACK: u16 = 0x0000;
    const WHITE: u16 = 0xFFFF;
    const RED: u16 = 0xF800;
    const GREEN: u16 = 0x07E0;
    const BLUE: u16 = 0x001F;
    const YELLOW: u16 = 0xFFE0;

    const PAL: &[u8] = &[FRAME_CODEC_PAL_RLE];

    /// Pixels -> RGB565-LE bytes.
    fn bytes(px: &[u16]) -> Vec<u8> {
        px.iter().flat_map(|c| c.to_le_bytes()).collect()
    }

    /// A frame shaped like what the renderer actually produces: mostly black,
    /// a handful of flat colors, plus a band of text-like per-pixel detail so
    /// this isn't just a few giant runs.
    fn realistic_frame() -> Vec<u8> {
        let mut px = vec![BLACK; FRAME_PIXELS];
        for y in 0..FRAME_HEIGHT {
            for x in 0..FRAME_WIDTH {
                let c = if y < 2 {
                    RED // away team's color rule
                } else if y >= FRAME_HEIGHT - 2 {
                    BLUE // home team's color rule
                } else if (6..14).contains(&y) && (4..28).contains(&x) {
                    WHITE // away score block
                } else if (6..14).contains(&y) && (36..60).contains(&x) {
                    YELLOW // home score block
                } else if (18..24).contains(&y) {
                    // text-like: short, irregular runs from a cheap hash
                    let h = (x * 7 + y * 13) % 5;
                    if h == 0 {
                        WHITE
                    } else if h == 1 {
                        GREEN
                    } else {
                        BLACK
                    }
                } else {
                    BLACK
                };
                px[y * FRAME_WIDTH + x] = c;
            }
        }
        bytes(&px)
    }

    /// Every encode is reversible, and never larger than raw.
    fn assert_round_trip(pixels: &[u8], accepted: &[u8]) -> Vec<u8> {
        let payload = encode_frame_payload(pixels, accepted);
        assert_eq!(
            decode_frame_payload(&payload).as_deref(),
            Some(pixels),
            "round trip failed for codec {}",
            payload[0]
        );
        assert!(
            payload.len() <= pixels.len() + 1,
            "payload {} exceeded raw {}",
            payload.len(),
            pixels.len() + 1
        );
        payload
    }

    #[test]
    fn realistic_frame_round_trips_and_shrinks() {
        let px = realistic_frame();
        assert_eq!(px.len(), FRAME_BYTES);

        let payload = assert_round_trip(&px, PAL);
        assert_eq!(payload[0], FRAME_CODEC_PAL_RLE, "flat graphics must compress");
        assert_eq!(payload[1] as usize, 6, "black/white/red/green/blue/yellow");
        assert!(
            payload.len() * 2 < FRAME_BYTES,
            "expected a real win, got {} bytes",
            payload.len()
        );

        // The same pixels with no negotiated codec stay raw.
        let raw = assert_round_trip(&px, &[]);
        assert_eq!(raw[0], FRAME_CODEC_RAW);
        assert_eq!(raw.len(), FRAME_BYTES + 1);
    }

    #[test]
    fn single_color_frame_is_tiny() {
        let px = bytes(&[BLACK; FRAME_PIXELS]);
        let payload = assert_round_trip(&px, PAL);
        assert_eq!(payload[0], FRAME_CODEC_PAL_RLE);
        assert_eq!(payload[1], 1, "one palette entry");
        // 2048 pixels = 8 full 255-runs + a 8-pixel remainder = 9 pairs.
        assert_eq!(payload.len(), 2 + 2 + 9 * 2);
        assert_eq!(&payload[..4], &[FRAME_CODEC_PAL_RLE, 1, 0x00, 0x00]);
    }

    #[test]
    fn sixteen_colors_compress_seventeen_falls_back() {
        // 16 colors, each in one long block.
        let mut px: Vec<u16> = Vec::new();
        for i in 0..16u16 {
            px.resize(px.len() + FRAME_PIXELS / 16, i * 0x1111);
        }
        let payload = assert_round_trip(&bytes(&px), PAL);
        assert_eq!(payload[0], FRAME_CODEC_PAL_RLE);
        assert_eq!(payload[1], 16, "a full palette is still legal");

        // One more distinct color and the palette can't hold it.
        let mut px17 = px.clone();
        px17.truncate(FRAME_PIXELS - 64);
        px17.resize(FRAME_PIXELS, 0xABCD);
        let payload = assert_round_trip(&bytes(&px17), PAL);
        assert_eq!(payload[0], FRAME_CODEC_RAW, "17 colors must fall back to raw");
        assert_eq!(payload.len(), FRAME_BYTES + 1);
    }

    /// The pathological case: alternating pixels turn every run into 2 bytes
    /// for 2 bytes of pixels, so RLE EXPANDS. Shipping that would make the
    /// compression a regression on the worst frame instead of the average one.
    #[test]
    fn alternating_pixels_never_expand() {
        let px: Vec<u16> =
            (0..FRAME_PIXELS).map(|i| if i % 2 == 0 { BLACK } else { WHITE }).collect();
        let payload = assert_round_trip(&bytes(&px), PAL);
        assert_eq!(payload[0], FRAME_CODEC_RAW);
        assert_eq!(payload.len(), FRAME_BYTES + 1, "exactly raw, not one byte more");

        // One pixel per run is exactly the break-even point (a 2-byte pair per
        // 2-byte pixel), which the palette header then pushes over. Two-pixel
        // runs are the first size that pays, and it must take that win.
        let px2: Vec<u16> =
            (0..FRAME_PIXELS).map(|i| if (i / 2) % 2 == 0 { BLACK } else { WHITE }).collect();
        let payload = assert_round_trip(&bytes(&px2), PAL);
        assert_eq!(payload[0], FRAME_CODEC_PAL_RLE);
        assert_eq!(payload.len(), 2 + 4 + (FRAME_PIXELS / 2) * 2);
    }

    #[test]
    fn runs_longer_than_255_split() {
        // 300 red then 300 blue: 255+45 and 255+45.
        let mut px = vec![RED; 300];
        px.resize(600, BLUE);
        let payload = assert_round_trip(&bytes(&px), PAL);
        assert_eq!(
            payload,
            vec![
                FRAME_CODEC_PAL_RLE,
                2,
                0x00,
                0xF8, // RED  LE
                0x1F,
                0x00, // BLUE LE
                255,
                0,
                45,
                0,
                255,
                1,
                45,
                1,
            ]
        );

        // And a run exactly 255 long doesn't sprout an empty second pair.
        let px = bytes(&[GREEN; 255]);
        let payload = assert_round_trip(&px, PAL);
        assert_eq!(payload.len(), 2 + 2 + 2);
        assert_eq!(payload[4], 255);
    }

    /// Frames are variable height — a deep leaderboard renders tall and
    /// scrolls. The codec must not assume 64x32 anywhere.
    #[test]
    fn tall_scrolling_frame_round_trips() {
        let height = 128; // a 20-row leaderboard
        let mut px = vec![BLACK; FRAME_WIDTH * height];
        for y in 0..height {
            for x in 0..FRAME_WIDTH {
                if x < 3 {
                    px[y * FRAME_WIDTH + x] = if y % 6 < 3 { GREEN } else { RED };
                } else if (2..10).contains(&(y % 6)) && (8..40).contains(&x) {
                    px[y * FRAME_WIDTH + x] = WHITE;
                }
            }
        }
        let px = bytes(&px);
        assert_eq!(px.len(), FRAME_BYTES * 4);
        let payload = assert_round_trip(&px, PAL);
        assert_eq!(payload[0], FRAME_CODEC_PAL_RLE);
        assert!(payload.len() < px.len() / 2);
    }

    #[test]
    fn empty_input_is_raw_and_round_trips() {
        let payload = assert_round_trip(&[], PAL);
        assert_eq!(payload, vec![FRAME_CODEC_RAW]);
        assert_eq!(decode_frame_payload(&payload), Some(Vec::new()));
    }

    #[test]
    fn odd_length_input_is_raw() {
        // Not a whole number of pixels: raw carries it verbatim.
        let px = vec![0xABu8; 4095];
        let payload = assert_round_trip(&px, PAL);
        assert_eq!(payload[0], FRAME_CODEC_RAW);
    }

    #[test]
    fn no_accepted_codecs_always_yields_raw() {
        for px in [realistic_frame(), bytes(&[BLACK; 64]), Vec::new(), vec![1u8, 2, 3]] {
            let payload = assert_round_trip(&px, &[]);
            assert_eq!(payload[0], FRAME_CODEC_RAW);
            assert_eq!(payload.len(), px.len() + 1);
        }
        // An unknown codec id is not a licence to invent one either.
        let px = realistic_frame();
        assert_eq!(encode_frame_payload(&px, &[7, 200])[0], FRAME_CODEC_RAW);
        // RAW listed explicitly is fine, and still raw.
        assert_eq!(encode_frame_payload(&px, &[FRAME_CODEC_RAW])[0], FRAME_CODEC_RAW);
        // Unknown codecs alongside a known one don't disturb the pick.
        assert_eq!(
            encode_frame_payload(&px, &[9, FRAME_CODEC_PAL_RLE, 3])[0],
            FRAME_CODEC_PAL_RLE
        );
    }

    #[test]
    fn malformed_payloads_are_rejected() {
        assert_eq!(decode_frame_payload(&[]), None, "empty payload");
        assert_eq!(decode_frame_payload(&[2]), None, "unknown codec byte");
        assert_eq!(decode_frame_payload(&[255, 1, 2, 3]), None, "unknown codec byte");
        assert_eq!(decode_frame_payload(&[FRAME_CODEC_PAL_RLE]), None, "no color count");
        assert_eq!(decode_frame_payload(&[FRAME_CODEC_PAL_RLE, 0]), None, "zero colors");
        assert_eq!(
            decode_frame_payload(&[FRAME_CODEC_PAL_RLE, 17, 0, 0]),
            None,
            "palette larger than 16"
        );
        assert_eq!(
            decode_frame_payload(&[FRAME_CODEC_PAL_RLE, 2, 0, 0, 0xFF]),
            None,
            "truncated palette"
        );
        assert_eq!(
            decode_frame_payload(&[FRAME_CODEC_PAL_RLE, 1, 0, 0]),
            None,
            "palette but no runs"
        );
        assert_eq!(
            decode_frame_payload(&[FRAME_CODEC_PAL_RLE, 1, 0, 0, 4]),
            None,
            "dangling half-pair"
        );
        assert_eq!(
            decode_frame_payload(&[FRAME_CODEC_PAL_RLE, 1, 0, 0, 0, 0]),
            None,
            "zero-length run"
        );
        assert_eq!(
            decode_frame_payload(&[FRAME_CODEC_PAL_RLE, 1, 0, 0, 4, 1]),
            None,
            "run indexes past the palette"
        );

        // Truncating a real payload mid-palette or mid-pair is caught too.
        let good = encode_frame_payload(&realistic_frame(), PAL);
        assert_eq!(good[0], FRAME_CODEC_PAL_RLE);
        assert_eq!(decode_frame_payload(&good[..5]), None, "cut inside the palette");
        assert_eq!(decode_frame_payload(&good[..good.len() - 1]), None, "cut mid-pair");
    }

    /// A tiny payload can claim an enormous frame — 2 bytes of runs per 510
    /// bytes of output. On a 160KB heap that has to be refused, not allocated.
    #[test]
    fn oversized_run_totals_are_refused() {
        let mut bomb = vec![FRAME_CODEC_PAL_RLE, 1, 0xFF, 0xFF];
        for _ in 0..200 {
            bomb.extend_from_slice(&[255, 0]); // 200 * 255 px = 102,000 bytes
        }
        assert!(bomb.len() < 512, "the payload itself is small: {}", bomb.len());
        assert_eq!(decode_frame_payload(&bomb), None);

        // Exactly at the cap is fine; one pixel past it is not.
        let runs = MAX_FRAME_BYTES / 2; // pixels
        let mut at_cap = vec![FRAME_CODEC_PAL_RLE, 1, 0x00, 0x00];
        let mut left = runs;
        while left > 0 {
            let n = left.min(255);
            at_cap.extend_from_slice(&[n as u8, 0]);
            left -= n;
        }
        let decoded = decode_frame_payload(&at_cap).expect("a cap-sized frame decodes");
        assert_eq!(decoded.len(), MAX_FRAME_BYTES);

        let mut over = at_cap.clone();
        over.extend_from_slice(&[1, 0]);
        assert_eq!(decode_frame_payload(&over), None, "one pixel past the cap");
    }

    /// Frames bigger than the decoder's cap never get compressed, so encode
    /// and decode can't disagree about what is acceptable.
    #[test]
    fn frames_past_the_cap_stay_raw() {
        let px = bytes(&vec![BLUE; MAX_FRAME_BYTES]); // 2x the cap in bytes
        assert!(px.len() > MAX_FRAME_BYTES);
        let payload = assert_round_trip(&px, PAL);
        assert_eq!(payload[0], FRAME_CODEC_RAW);
    }

    /// The binary websocket framing is unchanged in shape: the payload just
    /// carries a codec byte now.
    #[test]
    fn wire_framing_carries_the_payload_unchanged() {
        let px = realistic_frame();
        let id = frame_id(&px);
        let payload = encode_frame_payload(&px, PAL);
        let msg = encode_frame(&id, &payload);

        let (got_id, got_payload) = decode_frame(&msg).expect("framing decodes");
        assert_eq!(got_id, id);
        assert_eq!(got_payload, &payload[..]);
        // The id is still the hash of the PIXELS, not of the payload.
        assert_eq!(frame_id(&decode_frame_payload(got_payload).unwrap()), id);
    }

    #[test]
    fn need_frames_negotiates_codecs() {
        let msg = ClientMsg::NeedFrames {
            ids: vec!["abc".into()],
            codecs: vec![FRAME_CODEC_RAW, FRAME_CODEC_PAL_RLE],
        };
        let json = serde_json::to_string(&msg).unwrap();
        match serde_json::from_str::<ClientMsg>(&json).unwrap() {
            ClientMsg::NeedFrames { ids, codecs } => {
                assert_eq!(ids, vec!["abc".to_string()]);
                assert_eq!(codecs, vec![FRAME_CODEC_RAW, FRAME_CODEC_PAL_RLE]);
            }
            other => panic!("expected NeedFrames, got {other:?}"),
        }

        // Old firmware / the app: no `codecs` field at all means RAW ONLY.
        let old: ClientMsg =
            serde_json::from_str("{\"t\":\"NeedFrames\",\"ids\":[\"abc\"]}").unwrap();
        match old {
            ClientMsg::NeedFrames { codecs, .. } => {
                assert!(codecs.is_empty());
                assert_eq!(encode_frame_payload(&realistic_frame(), &codecs)[0], FRAME_CODEC_RAW);
            }
            other => panic!("expected NeedFrames, got {other:?}"),
        }
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
