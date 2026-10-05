//! Game, rotation, and mode RULES — the derivations both platforms must make
//! identically, moved down from the iOS view model so Kotlin never grows a
//! second copy. Same charter as `strings`: one place a rule can be wrong.
//!
//! Everything here is a pure function over owned uniffi values. The platform
//! keeps its own state and its own client calls; the core answers "given
//! these facts, what is true / what changes" — which is exactly the part
//! that drifted three times inside ONE platform before this module existed.

use std::collections::HashMap;

use crate::{UiGame, UiOffMode, UiOffModeField};

// ---- game text + status ----

/// Has this game NOT started? The two feeds spell it differently — missing
/// one spelling once put a dashed ring on the dashboard and a solid ring on
/// the same game in All Games.
#[uniffi::export]
pub fn game_not_started(status: String) -> bool {
    status == "upcoming" || status == "pregame"
}

/// THE team-id convention: `{sport}-{abbrev, lowercased}`. This string was
/// hand-spelled in three places in Swift; every favorites lookup rides on it.
#[uniffi::export]
pub fn team_id(sport_id: String, abbrev: String) -> String {
    format!("{sport_id}-{}", abbrev.to_lowercase())
}

/// A human title for a game card: "BOS @ TOR" for team games (abbreviations —
/// full names truncate at card width), or the event name for leaderboard
/// sports (golf/F1), whose `home` is empty. Event names arrive panel-style
/// ALL CAPS; the card reads better as "Monaco GP" — words of two letters or
/// fewer (GP, Q) stay caps.
#[uniffi::export]
pub fn game_title(away: String, home: String) -> String {
    if !home.is_empty() {
        return format!("{away} @ {home}");
    }
    away.split(' ')
        .map(|w| {
            // Short words stay caps ("GP", "US") — and so does any word with
            // no vowel: "BMW" and "CPKC" are initialisms, and "Bmw" reads as
            // a typo. Y counts as a vowel so "LYNX"-shaped words still fold.
            let vowelless =
                !w.chars().any(|c| matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u' | 'y'));
            if w.chars().count() <= 2 || vowelless {
                w.to_string()
            } else {
                let mut cs = w.chars();
                match cs.next() {
                    Some(f) => f.to_string() + &cs.as_str().to_lowercase(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a game involves one of the board's favorited teams.
#[uniffi::export]
pub fn involves_favorite(
    sport_id: String,
    away: String,
    home: String,
    favorite_team_ids: Vec<String>,
) -> bool {
    favorite_team_ids.contains(&team_id(sport_id.clone(), away))
        || favorite_team_ids.contains(&team_id(sport_id, home))
}

/// The favorited team that puts this game on the board, if one does —
/// returned as the ABBREV (away checked first); the platform resolves the
/// display name against its catalog.
#[uniffi::export]
pub fn favorite_abbrev(
    sport_id: String,
    away: String,
    home: String,
    favorite_team_ids: Vec<String>,
) -> Option<String> {
    [away, home]
        .into_iter()
        .find(|ab| favorite_team_ids.contains(&team_id(sport_id.clone(), ab.clone())))
}

/// Why a game is on the board: manually added, or via a favorite. None when
/// neither applies — a one-off REMOVED game has no active source even if a
/// preference would otherwise cover it (it reads "Not on scoreboard" until
/// re-added or Reset).
#[derive(uniffi::Enum, PartialEq, Debug, Clone, Copy)]
pub enum UiGameSource {
    Added,
    Favorite,
}

#[uniffi::export]
pub fn game_source(
    game_id: String,
    sport_id: String,
    away: String,
    home: String,
    excluded_game_ids: Vec<String>,
    extra_game_ids: Vec<String>,
    favorite_team_ids: Vec<String>,
) -> Option<UiGameSource> {
    if excluded_game_ids.contains(&game_id) {
        return None;
    }
    if extra_game_ids.contains(&game_id) {
        return Some(UiGameSource::Added);
    }
    if involves_favorite(sport_id, away, home.clone(), favorite_team_ids.clone()) {
        return Some(UiGameSource::Favorite);
    }
    // Leaderboard sports (golf/F1) have no home side and match by series tag
    // server-side; the summary carries the EVENT name, so team matching can't
    // see it — any favorite in that league counts.
    if home.is_empty() && !favorite_team_ids.is_empty() {
        return Some(UiGameSource::Favorite);
    }
    None
}

/// The "Live and Upcoming" panel's order: pinned game, then live, then
/// upcoming, with cap-bumped games trailing. Within a bucket the SERVER's
/// order is the actual rotation order — Rust's stable sort preserves it,
/// which is the tiebreak machinery Swift had to hand-roll.
#[uniffi::export]
pub fn displayed_now_showing(games: Vec<UiGame>, focused_game_id: Option<String>) -> Vec<UiGame> {
    let rank = |g: &UiGame| -> u8 {
        if Some(&g.id) == focused_game_id.as_ref() {
            0
        } else if g.bumped {
            3
        } else if g.status == "upcoming" {
            2
        } else {
            1
        }
    };
    let mut out = games;
    out.sort_by_key(rank);
    out
}

// ---- rotation membership + the card ring ----

/// The two one-off sets, after a mutation — returned together because the
/// rules move ids BETWEEN them.
#[derive(uniffi::Record, Clone)]
pub struct UiRotationSets {
    pub extra_game_ids: Vec<String>,
    pub excluded_game_ids: Vec<String>,
}

/// Add a game. Adding back a game the user had one-off REMOVED clears the
/// exclusion rather than creating an extra — that's what the server does,
/// and mirroring it exactly is what lets the card flip on the tap instead of
/// on the next push.
#[uniffi::export]
pub fn rotation_add(
    game_id: String,
    extra_game_ids: Vec<String>,
    excluded_game_ids: Vec<String>,
) -> UiRotationSets {
    let mut extra = extra_game_ids;
    let mut excluded = excluded_game_ids;
    if let Some(i) = excluded.iter().position(|g| *g == game_id) {
        excluded.remove(i);
    } else if !extra.contains(&game_id) {
        extra.push(game_id);
    }
    UiRotationSets { extra_game_ids: extra, excluded_game_ids: excluded }
}

/// Remove a game from the rotation regardless of WHY it's there: a one-off
/// add is un-added; a preference-driven game is EXCLUDED (League Preferences
/// stay intact — Reset brings exclusions back).
#[uniffi::export]
pub fn rotation_remove(
    game_id: String,
    extra_game_ids: Vec<String>,
    excluded_game_ids: Vec<String>,
) -> UiRotationSets {
    let mut extra = extra_game_ids;
    let mut excluded = excluded_game_ids;
    if let Some(i) = extra.iter().position(|g| *g == game_id) {
        extra.remove(i);
    } else if !excluded.contains(&game_id) {
        excluded.push(game_id);
    }
    UiRotationSets { extra_game_ids: extra, excluded_game_ids: excluded }
}

/// Whether Reset has anything to undo.
#[uniffi::export]
pub fn rotation_modified(
    focused_game_id: Option<String>,
    extra_game_ids: Vec<String>,
    excluded_game_ids: Vec<String>,
    spoiler_game_ids: Vec<String>,
    revealed_game_ids: Vec<String>,
) -> bool {
    focused_game_id.is_some()
        || !extra_game_ids.is_empty()
        || !excluded_game_ids.is_empty()
        || !spoiler_game_ids.is_empty()
        || !revealed_game_ids.is_empty()
}

/// Whether the board has CLAIMED this game right now — by preference (in the
/// board's own list and not cap-bumped) or by a one-off add. A one-off
/// REMOVAL wins over both: the exclusion applies optimistically, so the card
/// flips on the tap rather than on the server's next push.
#[uniffi::export]
pub fn rotation_is_showing(
    game_id: String,
    now_showing: Vec<UiGame>,
    extra_game_ids: Vec<String>,
    excluded_game_ids: Vec<String>,
) -> bool {
    if excluded_game_ids.contains(&game_id) {
        return false;
    }
    now_showing.iter().any(|g| g.id == game_id && !g.bumped) || extra_game_ids.contains(&game_id)
}

/// The ring a game's card wears — one source, so no two screens can disagree.
///
/// Deliberately ignores `UiGame.queued` for MEMBERSHIP: the server computes
/// that flag when it answers a Browse and nothing clears it locally, so a
/// removed game kept its dashed ring until the next fetch. It is consulted
/// only for claimed-ness of games past the board state's upcoming cap, where
/// the flag is stale in one harmless direction (it can't know about a local
/// removal — the exclusion check covers that).
#[derive(uniffi::Enum, PartialEq, Debug, Clone, Copy)]
pub enum UiCardHighlight {
    None,
    /// A pin overrides the rotation, so it outranks every other state.
    Pinned,
    /// Claimed but not started: the dashed ring.
    Queued,
    /// On the panel now: the solid ring.
    Included,
}

#[uniffi::export]
pub fn card_highlight(
    game: UiGame,
    focused_game_id: Option<String>,
    now_showing: Vec<UiGame>,
    extra_game_ids: Vec<String>,
    excluded_game_ids: Vec<String>,
) -> UiCardHighlight {
    if focused_game_id.as_ref() == Some(&game.id) {
        return UiCardHighlight::Pinned;
    }
    let claimed = rotation_is_showing(
        game.id.clone(),
        now_showing,
        extra_game_ids,
        excluded_game_ids.clone(),
    ) || (game.queued && !excluded_game_ids.contains(&game.id));
    if !claimed {
        return UiCardHighlight::None;
    }
    if game_not_started(game.status) { UiCardHighlight::Queued } else { UiCardHighlight::Included }
}

// ---- off-mode descriptor interpretation ----

/// Stored value, else the field's declared default, else empty. MUST agree
/// with `OffModeDescriptor::value` in the protocol crate — the server renders
/// from that one; `the_app_and_the_server_resolve_off_mode_values_identically`
/// makes the mirror machine-checked instead of a comment.
#[uniffi::export]
pub fn off_mode_value(mode: &UiOffMode, key: String, params: HashMap<String, String>) -> String {
    if let Some(v) = params.get(&key) {
        return v.clone();
    }
    mode.fields.iter().find(|f| f.key == key).map(|f| f.default.clone()).unwrap_or_default()
}

/// The fields to draw, honouring one level of conditional visibility against
/// the RESOLVED value — a condition on a defaulted field works before the
/// user has touched anything.
#[uniffi::export]
pub fn visible_off_mode_fields(
    mode: &UiOffMode,
    params: HashMap<String, String>,
) -> Vec<UiOffModeField> {
    mode.fields
        .iter()
        .filter(|f| match (&f.visible_when_key, &f.visible_when_value) {
            (Some(key), Some(want)) => {
                off_mode_value(mode, key.clone(), params.clone()) == *want
            }
            _ => true,
        })
        .cloned()
        .collect()
}

/// The preview for a mode: the selected choice's, if any visible choice field
/// declares one, else the mode's own. Every path comes from the DESCRIPTOR —
/// the app never builds a preview URL out of an id, or a forked server's
/// mode stops previewing.
#[uniffi::export]
pub fn current_preview_path(mode: &UiOffMode, params: HashMap<String, String>) -> Option<String> {
    for f in visible_off_mode_fields(mode, params.clone()) {
        if f.kind == "choice" {
            let picked = off_mode_value(mode, f.key.clone(), params.clone());
            if let Some(p) = f.options.iter().find(|o| o.value == picked).and_then(|o| o.preview.clone()) {
                return Some(p);
            }
        }
    }
    mode.preview.clone()
}

/// The optimistic mirror of the server's `apply_mode` timer rule: hours from
/// the mode's own setting, falling back to `sleep_hours`; "scores" and 0 run
/// on no timer; everything else expires `min(hours, 72)` from now. The 72 is
/// the server's clamp — if these disagreed, the app's countdown would lie
/// until the next push.
#[uniffi::export]
pub fn compute_wake_at(
    mode: String,
    mode_hours: HashMap<String, u32>,
    sleep_hours: u32,
    now_unix: u64,
) -> Option<u64> {
    if mode == "scores" {
        return None;
    }
    let hours = mode_hours.get(&mode).copied().unwrap_or(sleep_hours);
    match hours.min(72) {
        0 => None,
        h => Some(now_unix + u64::from(h) * 3600),
    }
}

// ---- league preferences ----

/// One league a board follows, resolved against the catalog: the league, the
/// favorited teams in USER ORDER, and the labels of the rounds it subscribes
/// to.
#[derive(uniffi::Record, Clone)]
pub struct UiLeaguePref {
    pub sport: crate::UiSport,
    pub teams: Vec<crate::UiTeam>,
    pub round_labels: Vec<String>,
    pub league_spoiler: bool,
    pub spoiler_team_ids: Vec<String>,
}

/// The leagues worth showing on the dashboard, in catalog order.
///
/// A league earns a card when it has favorites, subscribed rounds, or ANY
/// per-league override — a dwell or depth override alone still counts, or it
/// would be active but invisible (set from Add League without subscribing, or
/// left behind after the last favorite is removed).
///
/// Favorites come back in the order the config lists them (the user's own
/// drag order), not catalog order; unknown ids are dropped rather than
/// faked — configs carry orphans from before the leaderboard-league split.
#[uniffi::export]
pub fn league_prefs(
    config: crate::UiBoardConfig,
    sports: Vec<crate::UiSport>,
) -> Vec<UiLeaguePref> {
    let mut out = Vec::new();
    for sport in &sports {
        let Some(sc) = config.sports.iter().find(|s| s.sport_id == sport.id) else { continue };
        let teams: Vec<crate::UiTeam> = sc
            .favorite_team_ids
            .iter()
            .filter_map(|id| sport.teams.iter().find(|t| &t.id == id).cloned())
            .collect();
        let round_labels: Vec<String> = sport
            .championship_rounds
            .iter()
            .filter(|r| sc.championship_option_ids.contains(&r.id))
            .map(|r| r.label.clone())
            .collect();
        let has_override = sc.dwell_secs.is_some()
            || sc.leaderboard_rows.is_some()
            || sc.show_round.is_some()
            || sc.show_extras.is_some()
            || sc.name_style.is_some();
        if teams.is_empty()
            && round_labels.is_empty()
            && !has_override
            && !sc.spoiler
            && sc.spoiler_team_ids.is_empty()
        {
            continue;
        }
        out.push(UiLeaguePref {
            sport: sport.clone(),
            teams,
            round_labels,
            league_spoiler: sc.spoiler,
            spoiler_team_ids: sc.spoiler_team_ids.clone(),
        });
    }
    out
}

/// The blanket "everything in this league" option id. Subscribing to it
/// implies every round; the CONVERSE does not hold — inferring it once turned
/// a majors-only golf subscription into the entire PGA season and made
/// majors-only inexpressible.
pub const WHOLE_LEAGUE_OPTION: &str = "all";

/// Toggling one round of a league's subscription.
///
/// Picking the blanket option selects every round (that IS what it means).
/// Un-picking any single round only removes that round — it must NOT clear
/// the blanket implication for the others, which is the one-way rule above.
#[uniffi::export]
pub fn toggle_round(
    round_id: String,
    picked: Vec<String>,
    all_round_ids: Vec<String>,
) -> Vec<String> {
    let mut set: Vec<String> = picked;
    if set.contains(&round_id) {
        set.retain(|r| *r != round_id);
        return set;
    }
    if round_id == WHOLE_LEAGUE_OPTION {
        return all_round_ids;
    }
    set.push(round_id);
    // Keep the catalog's order rather than tap order, so the summary reads
    // the same however it was built.
    all_round_ids.into_iter().filter(|r| set.contains(r)).collect()
}

/// Leaderboard depth choices for a league: (rows, label). F1 calls the
/// deepest one "Full field" — a rule iOS was maintaining in three places.
#[uniffi::export]
pub fn leaderboard_depth_options(sport_id: String) -> Vec<UiDepthOption> {
    vec![
        UiDepthOption { rows: 4, label: "Top 4 — no scrolling".into() },
        UiDepthOption { rows: 10, label: "Top 10".into() },
        UiDepthOption {
            rows: 20,
            label: if sport_id == "f1" { "Full field".into() } else { "Top 20".into() },
        },
    ]
}

#[derive(uniffi::Record, Clone)]
pub struct UiDepthOption {
    pub rows: u32,
    pub label: String,
}

// ---- screen speed ----
//
// How long each screen holds the panel, as three named speeds rather than a
// seconds picker. Seconds stay the stored unit (old apps write them), and a
// stored value that is not a preset reads as the preset it snaps to — the
// server times the board by the same snap, so label and panel agree.

/// The speed choices, fastest first: Fast (10s), Standard (15s), Slow (20s).
/// A scrolling screen holds longer in proportion to its height (64 rows twice
/// its speed, 128 four times — the server's `screen_dwell_ms`); the labels
/// don't say so, because the choice is about pace, not arithmetic.
#[uniffi::export]
pub fn dwell_options() -> Vec<UiDwellOption> {
    use scoreboard_protocol::{DWELL_FAST_SECS, DWELL_SLOW_SECS, DWELL_STANDARD_SECS};
    vec![
        UiDwellOption { secs: DWELL_FAST_SECS, label: "Fast".into() },
        UiDwellOption { secs: DWELL_STANDARD_SECS, label: "Standard".into() },
        UiDwellOption { secs: DWELL_SLOW_SECS, label: "Slow".into() },
    ]
}

#[derive(uniffi::Record, Clone)]
pub struct UiDwellOption {
    pub secs: u32,
    pub label: String,
}

/// The option a stored value selects: its snapped preset. Compare
/// `dwell_selected(secs) == option.secs` to place the checkmark.
#[uniffi::export]
pub fn dwell_selected(secs: u32) -> u32 {
    scoreboard_protocol::snap_dwell_secs(secs)
}

/// A stored speed's name: "Fast", "Standard", "Slow".
#[uniffi::export]
pub fn dwell_label(secs: u32) -> String {
    let snapped = dwell_selected(secs);
    dwell_options().into_iter().find(|o| o.secs == snapped).map(|o| o.label).unwrap_or_default()
}

/// The setting's title, in Settings and in a league's editor.
#[uniffi::export]
pub fn dwell_title() -> String {
    "Screen speed".into()
}

/// A league's "use the board's speed" choice, naming what that is:
/// "Board default (Standard)".
#[uniffi::export]
pub fn league_dwell_default_label(board_secs: u32) -> String {
    format!("Board default ({})", dwell_label(board_secs))
}

/// A league override as the dashboard's override row says it: "Slow speed".
#[uniffi::export]
pub fn league_dwell_override_label(secs: u32) -> String {
    format!("{} speed", dwell_label(secs))
}

/// Which row of a scrolling frame the app's card shows at the top of its
/// window, `elapsed_ms` into the animation: the board's own scroll curve
/// (hold, glide down, hold, glide up, hold, one pass per `dwell_ms`), so the
/// card moves exactly as the panel does. `frame_rows` is the frame's height;
/// a panel-sized frame, or a dwell of 0, never moves.
#[uniffi::export]
pub fn scroll_window_offset(elapsed_ms: u64, dwell_ms: u32, frame_rows: u32) -> u32 {
    let max_off = frame_rows.saturating_sub(scoreboard_protocol::FRAME_HEIGHT as u32) as usize;
    scoreboard_protocol::scroll_offset(elapsed_ms, dwell_ms as u64, max_off) as u32
}

// ---- endpoints ----

/// The HTTP origin for a board's server, derived from its websocket URL:
/// drop a trailing `/ws`, then `wss` -> `https` and `ws` -> `http`.
///
/// The scheme mapping is the part to be careful with. Every preview, frame
/// and guide image the app renders is fetched from this origin, so a rewrite
/// that lands on `http` for a `wss` server silently downgrades all of it to
/// cleartext against a server that was speaking TLS — no error, just an
/// unencrypted fetch. One rule, one place, both platforms.
#[uniffi::export]
pub fn http_base(ws_url: String) -> String {
    let u = ws_url.strip_suffix("/ws").unwrap_or(&ws_url);
    if let Some(rest) = u.strip_prefix("wss://") {
        return format!("https://{rest}");
    }
    if let Some(rest) = u.strip_prefix("ws://") {
        return format!("http://{rest}");
    }
    u.to_string()
}

// ---- provisioning: server presets and the pin grammar ----

/// A server the app can point a board at.
#[derive(uniffi::Record, Clone)]
pub struct UiServerPreset {
    pub id: String,
    pub label: String,
    /// The websocket URL the PHONE uses. None for "my own server", which has
    /// no fixed address.
    pub url: Option<String>,
    /// The `host:port` the BOARD is given — deliberately NOT derived from the
    /// phone's URL for a hosted preset: the phone talks to Railway's HTTPS
    /// edge, and the board needs the raw TCP proxy it can complete a
    /// pinned-key TLS handshake against. Writing the edge to a board
    /// provisions an endpoint that answers nothing.
    pub board_host_port: Option<String>,
}

#[uniffi::export]
pub fn server_presets() -> Vec<UiServerPreset> {
    vec![
        UiServerPreset {
            id: "prod".into(),
            label: "Hosted server".into(),
            url: Some("wss://live-production-118c.up.railway.app/ws".into()),
            board_host_port: Some("hayabusa.proxy.rlwy.net:47958".into()),
        },
        UiServerPreset {
            id: "localhost".into(),
            label: "Localhost".into(),
            url: Some("ws://127.0.0.1:7878/ws".into()),
            board_host_port: Some("127.0.0.1:7878".into()),
        },
        UiServerPreset {
            id: "custom".into(),
            label: "My own server".into(),
            url: None,
            // A self-hosted server has exactly ONE address, so the board's
            // endpoint comes from the phone's URL rather than a second one.
            board_host_port: None,
        },
    ]
}

/// The `host:port` to write to a board's `server` characteristic, for the
/// websocket URL the phone is using. Empty means "keep the firmware default".
#[uniffi::export]
pub fn host_port_for_board(ws_url: String) -> String {
    let u = ws_url.trim();
    if let Some(pinned) =
        server_presets().into_iter().find(|p| p.url.as_deref() == Some(u)).and_then(|p| p.board_host_port)
    {
        return pinned;
    }
    let mut host = u;
    for p in ["ws://", "wss://", "http://", "https://"] {
        if let Some(rest) = host.strip_prefix(p) {
            host = rest;
            break;
        }
    }
    if let Some(slash) = host.find('/') {
        host = &host[..slash];
    }
    if host.is_empty() {
        return String::new();
    }
    if host.contains(':') { host.to_string() } else { format!("{host}:7878") }
}

/// The bare pin candidate inside whatever was pasted — NOT validated.
///
/// `gen-tls-cert` prints the pin on a labelled line, and it also lands in
/// config files and shell exports, so the clipboard arrives carrying any of
/// "pin: <hex>", "SERVER_PIN=<hex>", quotes, spaces, a trailing newline, or
/// upper case. None of that is part of the pin: hex contains no ':' or '=',
/// so everything through the last one is a label.
#[uniffi::export]
pub fn pin_candidate(raw: String) -> String {
    let s = match raw.rfind([':', '=']) {
        Some(i) => &raw[i + 1..],
        None => &raw[..],
    };
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
        .collect::<String>()
        .to_lowercase()
}

/// The canonical lowercase 64-hex pin, or None if the input is not one.
///
/// Deliberately the SAME three rules the firmware applies to the write
/// (`creds::pin_from_hex_str`): 64 hex digits, nothing else, never the
/// all-zero digest. Anything this accepts and the board does not is a
/// provisioning that fails on the far side — where there is no text field to
/// put the complaint in, and the only way back is a physical factory reset.
///
/// The alphabet is explicit rather than a library `is_hex_digit`: Swift's
/// `isHexDigit` also accepts full-width Unicode digits the firmware rejects,
/// which is exactly the class of divergence this function exists to prevent.
#[uniffi::export]
pub fn normalized_pin(raw: String) -> Option<String> {
    let c = pin_candidate(raw);
    let hex = |ch: char| "0123456789abcdef".contains(ch);
    if c.chars().count() == 64 && c.chars().all(hex) && c.chars().any(|ch| ch != '0') {
        Some(c)
    } else {
        None
    }
}

/// Why a non-empty pin field is unusable, in words for the person typing — or
/// None when it is good (or empty, which each screen phrases its own way).
#[uniffi::export]
pub fn pin_complaint(raw: String) -> Option<String> {
    let c = pin_candidate(raw.clone());
    if c.is_empty() || normalized_pin(raw).is_some() {
        return None;
    }
    if !c.chars().all(|ch| "0123456789abcdef".contains(ch)) {
        return Some("A pin is hexadecimal — digits and the letters a-f only.".into());
    }
    if c.chars().count() != 64 {
        return Some(format!("A pin is exactly 64 characters; this one has {}.", c.chars().count()));
    }
    Some("That's the all-zero placeholder, not a key — use the pin your server printed.".into())
}

/// One step of the BLE provisioning write sequence: which characteristic, and
/// the UTF-8 bytes to write.
///
/// ORDER IS LOAD-BEARING. The firmware saves and reboots the moment the
/// PASSWORD lands, so the optional pin and server must be written before it —
/// a pin written after the password never arrives, and the board refuses the
/// save as "custom host, no pin" (or worse, provisions against a server it
/// cannot handshake with). The firmware's own comment says so; this function
/// is the client half of that contract, shared so neither platform can get
/// the order wrong.
#[derive(uniffi::Record, Clone)]
pub struct UiProvisionStep {
    /// Characteristic UUID, uppercase.
    pub uuid: String,
    pub value: String,
}

/// The GATT ids, in one place both platforms read.
pub const BLE_SERVICE_UUID: &str = "21BA0000-5363-6F72-6562-6F6172640001";
pub const BLE_SSID_UUID: &str = "21BA0000-5363-6F72-6562-6F6172640002";
pub const BLE_PASS_UUID: &str = "21BA0000-5363-6F72-6562-6F6172640003";
pub const BLE_SERVER_UUID: &str = "21BA0000-5363-6F72-6562-6F6172640004";
pub const BLE_NETWORKS_UUID: &str = "21BA0000-5363-6F72-6562-6F6172640005";
pub const BLE_PIN_UUID: &str = "21BA0000-5363-6F72-6562-6F6172640006";

#[derive(uniffi::Record, Clone)]
pub struct UiBleContract {
    pub device_name: String,
    pub service: String,
    pub ssid: String,
    pub password: String,
    pub server: String,
    pub networks: String,
    pub pin: String,
}

#[uniffi::export]
pub fn ble_contract() -> UiBleContract {
    UiBleContract {
        // The firmware advertises its NAME but not the 128-bit service UUID,
        // so a client scans broadly and matches on this.
        device_name: "Scoreboard Setup".into(),
        service: BLE_SERVICE_UUID.into(),
        ssid: BLE_SSID_UUID.into(),
        password: BLE_PASS_UUID.into(),
        server: BLE_SERVER_UUID.into(),
        networks: BLE_NETWORKS_UUID.into(),
        pin: BLE_PIN_UUID.into(),
    }
}

/// The whole write sequence for one provisioning, or an error to show the
/// user. `pin_raw` may be anything pasted; empty means "use the built-in".
#[uniffi::export]
pub fn provision_steps(
    ssid: String,
    password: String,
    ws_url: String,
    pin_raw: String,
    board_has_pin_characteristic: bool,
    board_has_server_characteristic: bool,
) -> Result<Vec<UiProvisionStep>, ProvisionError> {
    if ssid.is_empty() {
        return Err(ProvisionError::Refused { why: "Pick a network first.".into() });
    }
    let mut steps = Vec::new();
    let host_port = host_port_for_board(ws_url);
    if !pin_raw.trim().is_empty() {
        let Some(pin) = normalized_pin(pin_raw.clone()) else {
            return Err(ProvisionError::Refused {
                why: pin_complaint(pin_raw)
                    .unwrap_or_else(|| "That server pin isn't 64 hex characters".into()),
            });
        };
        if !board_has_pin_characteristic {
            return Err(ProvisionError::Refused {
                why: "This board's firmware can't be given a server pin".into(),
            });
        }
        steps.push(UiProvisionStep { uuid: BLE_PIN_UUID.into(), value: pin });
    }
    // Old firmware without the characteristic keeps its compiled default.
    if board_has_server_characteristic && !host_port.is_empty() && host_port.len() <= 96 {
        steps.push(UiProvisionStep { uuid: BLE_SERVER_UUID.into(), value: host_port });
    }
    steps.push(UiProvisionStep { uuid: BLE_SSID_UUID.into(), value: ssid });
    // LAST, always: this write is what makes the board save and reboot.
    steps.push(UiProvisionStep { uuid: BLE_PASS_UUID.into(), value: password });
    Ok(steps)
}

#[derive(uniffi::Error, Debug, thiserror::Error)]
pub enum ProvisionError {
    #[error("{why}")]
    Refused { why: String },
}

/// The board's pre-scanned network list, as written to the networks
/// characteristic: UTF-8 SSIDs joined by '\n', strongest first, deduped,
/// zero-padded to a fixed size.
#[uniffi::export]
pub fn parse_networks(raw: Vec<u8>) -> Vec<String> {
    String::from_utf8_lossy(&raw)
        .trim_end_matches('\0')
        .split('\n')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

// ---- the mode picker ----

/// One tile of the Controls mode picker.
#[derive(uniffi::Record, Clone)]
pub struct UiPickableMode {
    pub id: String,
    pub label: String,
    /// Complete server path for the tile's preview; None draws a black panel
    /// (an all-black PNG cannot be told apart from a failed load).
    pub path: Option<String>,
}

/// What a board shows before the server's catalog arrives: enough descriptors
/// to render the idle-screen setting sanely offline. Labels must match the
/// server's ("Off", not "Nothing" — one darkness, one name).
#[uniffi::export]
pub fn fallback_off_modes() -> Vec<UiOffMode> {
    vec![
        UiOffMode {
            id: "clock".into(),
            label: "Clock".into(),
            description: "The time, in the scoreboard's timezone.".into(),
            preview: Some("/offmode/clock.png".into()),
            input: None,
            fields: vec![],
        },
        UiOffMode {
            id: "blank".into(),
            label: "Off".into(),
            description: "The panel stays dark.".into(),
            preview: None,
            input: None,
            fields: vec![],
        },
    ]
}

/// Every pickable mode, in display order: Scores first (the resting state),
/// the server's lit screens, Off last. The dark idle descriptor is excluded —
/// the Off entry IS that darkness, and two dark tiles with different meanings
/// is a trap.
#[uniffi::export]
pub fn pickable_modes(off_modes: Vec<UiOffMode>) -> Vec<UiPickableMode> {
    let mut out = vec![UiPickableMode {
        id: "scores".into(),
        label: "Scores".into(),
        path: Some("/sample/plain.png?live=1".into()),
    }];
    // Off comes second, matching the Screen settings page where Off leads
    // the idle-screen list — the two orderings must read as one system.
    out.push(UiPickableMode { id: "off".into(), label: "Off".into(), path: None });
    for m in off_modes {
        if m.preview.is_some() {
            out.push(UiPickableMode { id: m.id, label: m.label, path: m.preview });
        }
    }
    out
}

/// A mode preview showing THIS board's configuration: the stored params ride
/// along as a sorted, percent-encoded query. Each mode reads only its own
/// keys, so passing the whole map to every preview is harmless; sorted so the
/// URL is stable and the platform image cache can hit.
#[uniffi::export]
pub fn configured_preview(path: Option<String>, params: HashMap<String, String>) -> Option<String> {
    let path = path?;
    if !path.starts_with("/offmode/") || params.is_empty() {
        return Some(path);
    }
    let mut pairs: Vec<String> =
        params.iter().map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v))).collect();
    pairs.sort();
    let sep = if path.contains('?') { '&' } else { '?' };
    Some(format!("{path}{sep}{}", pairs.join("&")))
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Is the panel showing NO game? Counts only entries that are actually in
/// the board's rotation: an "upcoming" entry is claimed-but-unstarted (it
/// rides along in "Live and Upcoming" for the reader, flagged `queued`) and a
/// bumped one was pushed off by the cap — neither is on the glass. Passing
/// the raw list length instead put the ring on Scores while the panel sat
/// on the idle weather screen waiting for tonight's games.
///
/// `None` is "not known yet": the board's state has not arrived (an app
/// opening, a board just picked). That is NOT an empty panel — treating it as
/// one made the picker jump to the idle screen for a moment on every open,
/// then strand its rail there when the state landed. Unknown trusts the
/// board's configured mode.
#[uniffi::export]
pub fn panel_shows_nothing(now_showing: Option<Vec<UiGame>>) -> bool {
    now_showing.is_some_and(|games| !games.iter().any(|g| !g.queued && !g.bumped))
}

/// The ring follows the GLASS: what the panel is actually showing, as a
/// picker id. `now_showing_empty` is [`panel_shows_nothing`], not the list's
/// length. Quiet hours force Off (the window darkens the panel whatever
/// the mode says); scores mode with nothing on is showing the idle screen,
/// with the blank descriptor reading as Off.
#[uniffi::export]
pub fn effective_mode(
    mode: String,
    quiet_active: bool,
    now_showing_empty: bool,
    off_mode: String,
) -> String {
    if quiet_active {
        return "off".into();
    }
    if mode == "scores" && now_showing_empty {
        return if off_mode == "blank" { "off".into() } else { off_mode };
    }
    mode
}

/// A picker id's display label: Scores/Off for the two reserved ids, the
/// descriptor's label otherwise, capitalized id as the last resort.
#[uniffi::export]
pub fn mode_label(id: String, off_modes: Vec<UiOffMode>) -> String {
    match id.as_str() {
        "scores" => "Scores".into(),
        "off" => "Off".into(),
        _ => off_modes
            .into_iter()
            .find(|m| m.id == id)
            .map(|m| m.label)
            .unwrap_or_else(|| {
                let mut cs = id.chars();
                match cs.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + cs.as_str(),
                    None => id.clone(),
                }
            }),
    }
}

/// The knob's detents: steps of 10, floor 10, cap 100 — one scale for the
/// slider, the physical dial, and the server clamp.
#[uniffi::export]
pub fn snap_brightness(value: f64) -> u8 {
    ((value / 10.0).round() * 10.0).clamp(10.0, 100.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UiOffModeChoice;

    fn game(id: &str, status: &str, bumped: bool) -> UiGame {
        UiGame {
            id: id.into(),
            sport_id: "nhl".into(),
            away: "BOS".into(),
            home: "TOR".into(),
            away_score: 0,
            home_score: 0,
            status: status.into(),
            detail: String::new(),
            start_time: None,
            end_time: None,
            frame_id: String::new(),
            espn_url: None,
            bumped,
            champ: None,
            spoiled: false,
            scrolls: false,
            scroll_dwell_ms: 0,
            pin_rows: 0,
            queued: false,
        }
    }

    /// Three named speeds, and every stored value reads as one of them.
    #[test]
    fn speeds_are_named_and_stored_values_snap() {
        let labels: Vec<(u32, String)> = dwell_options().into_iter().map(|o| (o.secs, o.label)).collect();
        assert_eq!(labels, [(10, "Fast".to_string()), (15, "Standard".into()), (20, "Slow".into())]);
        assert_eq!(dwell_label(7), "Fast", "a legacy 7s reads as Fast");
        assert_eq!(dwell_label(30), "Slow", "a legacy 30s reads as Slow");
        assert_eq!(dwell_selected(30), 20);
        assert_eq!(league_dwell_default_label(15), "Board default (Standard)");
        assert_eq!(league_dwell_override_label(20), "Slow speed");
    }

    /// The card's window follows the board's curve; a panel-sized frame
    /// never moves.
    #[test]
    fn the_card_scrolls_on_the_boards_curve() {
        // 107 rows: 75 rows to travel. 30s dwell: 6s phases.
        assert_eq!(scroll_window_offset(3_000, 30_000, 107), 0, "holding at the top");
        assert_eq!(scroll_window_offset(9_000, 30_000, 107), 37, "halfway down");
        assert_eq!(scroll_window_offset(15_000, 30_000, 107), 75, "holding at the bottom");
        assert_eq!(scroll_window_offset(15_000, 30_000, 32), 0, "a panel-sized frame");
        assert_eq!(scroll_window_offset(15_000, 0, 107), 0, "no dwell");
    }

    /// Both feed spellings of "not started", pinned — missing one once split
    /// the ring style between two screens showing the same game.
    #[test]
    fn both_spellings_of_not_started_count() {
        assert!(game_not_started("upcoming".into()));
        assert!(game_not_started("pregame".into()));
        assert!(!game_not_started("live".into()));
        assert!(!game_not_started("final".into()));
    }

    #[test]
    fn titles_read_as_matchups_or_title_cased_events() {
        assert_eq!(game_title("BOS".into(), "TOR".into()), "BOS @ TOR");
        // Leaderboard events: ALL CAPS from the panel, title-cased for the
        // card, short words (GP, Q) kept caps.
        assert_eq!(game_title("MONACO GP".into(), String::new()), "Monaco GP");
        assert_eq!(game_title("THE OPEN".into(), String::new()), "The Open");
    }

    #[test]
    fn the_team_id_convention_is_lowercased() {
        assert_eq!(team_id("nhl".into(), "BOS".into()), "nhl-bos");
    }

    /// The panel order: pinned, live, upcoming, bumped — and within a bucket
    /// the server's own order survives (stable sort).
    #[test]
    fn now_showing_orders_pinned_live_upcoming_bumped() {
        let games = vec![
            game("up1", "upcoming", false),
            game("live1", "live", false),
            game("bumped", "live", true),
            game("pinned", "upcoming", false),
            game("live2", "live", false),
        ];
        let ordered = displayed_now_showing(games, Some("pinned".into()));
        let ids: Vec<&str> = ordered.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, ["pinned", "live1", "live2", "up1", "bumped"]);
    }

    /// Adding back a removed game clears the exclusion instead of creating an
    /// extra — the server's rule, mirrored so the card flips on the tap.
    #[test]
    fn rotation_transitions_move_ids_between_the_sets() {
        let s = rotation_add("g1".into(), vec![], vec!["g1".into()]);
        assert!(s.extra_game_ids.is_empty(), "re-add clears the exclusion, no extra");
        assert!(s.excluded_game_ids.is_empty());
        let s = rotation_add("g2".into(), vec![], vec![]);
        assert_eq!(s.extra_game_ids, ["g2"]);
        // Removing an added game un-adds; removing a preference game excludes.
        let s = rotation_remove("g2".into(), vec!["g2".into()], vec![]);
        assert!(s.extra_game_ids.is_empty() && s.excluded_game_ids.is_empty());
        let s = rotation_remove("g3".into(), vec![], vec![]);
        assert_eq!(s.excluded_game_ids, ["g3"]);
    }

    /// The ring's precedence: pin above all; exclusion kills a stale queued
    /// flag; claimed-but-unstarted is dashed, started is solid.
    #[test]
    fn the_card_ring_has_one_precedence() {
        let now = vec![game("live1", "live", false), game("b", "live", true)];
        let g = game("live1", "live", false);
        assert_eq!(
            card_highlight(g.clone(), Some("live1".into()), now.clone(), vec![], vec![]),
            UiCardHighlight::Pinned
        );
        assert_eq!(
            card_highlight(g.clone(), None, now.clone(), vec![], vec![]),
            UiCardHighlight::Included
        );
        // Bumped games are not claimed by membership.
        let b = game("b", "live", true);
        assert_eq!(card_highlight(b, None, now.clone(), vec![], vec![]), UiCardHighlight::None);
        // A browse-time queued flag claims — unless a local removal exists,
        // which must drop the ring instantly.
        let mut q = game("q", "upcoming", false);
        q.queued = true;
        assert_eq!(
            card_highlight(q.clone(), None, now.clone(), vec![], vec![]),
            UiCardHighlight::Queued
        );
        assert_eq!(
            card_highlight(q, None, now, vec![], vec!["q".into()]),
            UiCardHighlight::None
        );
    }

    /// The app-side resolver and the protocol crate's canonical one must
    /// agree — the server renders from the protocol's. This is the mirror the
    /// Swift comment could only ask for; here it is machine-checked.
    #[test]
    fn the_app_and_the_server_resolve_off_mode_values_identically() {
        use scoreboard_protocol::{FieldCondition, OffModeDescriptor, OffModeField};
        let proto = OffModeDescriptor {
            id: "weather".into(),
            label: "Weather".into(),
            description: String::new(),
            preview: None,
            input: None,
            fields: vec![
                OffModeField {
                    key: "variant".into(),
                    label: "Layout".into(),
                    kind: "choice".into(),
                    placeholder: String::new(),
                    max_len: 0,
                    default: "simple".into(),
                    options: vec![],
                    visible_when: None,
                    per_screen: false,
                },
                OffModeField {
                    key: "hour24".into(),
                    label: "24-hour".into(),
                    kind: "toggle".into(),
                    placeholder: String::new(),
                    max_len: 0,
                    default: "0".into(),
                    options: vec![],
                    visible_when: Some(FieldCondition { key: "variant".into(), equals: "clock".into() }),
                    per_screen: false,
                },
            ],
        };
        let ui = UiOffMode {
            id: proto.id.clone(),
            label: proto.label.clone(),
            description: String::new(),
            preview: None,
            input: None,
            fields: proto
                .fields
                .iter()
                .map(|f| UiOffModeField {
                    key: f.key.clone(),
                    label: f.label.clone(),
                    kind: f.kind.clone(),
                    placeholder: f.placeholder.clone(),
                    max_len: f.max_len,
                    default: f.default.clone(),
                    options: vec![],
                    visible_when_key: f.visible_when.as_ref().map(|c| c.key.clone()),
                    visible_when_value: f.visible_when.as_ref().map(|c| c.equals.clone()),
                })
                .collect(),
        };
        for params in [
            std::collections::BTreeMap::new(),
            [("variant".to_string(), "clock".to_string())].into_iter().collect(),
        ] {
            let hash: HashMap<String, String> = params.clone().into_iter().collect();
            for key in ["variant", "hour24", "missing"] {
                assert_eq!(
                    off_mode_value(&ui, key.into(), hash.clone()),
                    proto.value(key, &params),
                    "key {key} diverged"
                );
            }
            assert_eq!(
                visible_off_mode_fields(&ui, hash).len(),
                proto.visible_fields(&params).len(),
                "visibility diverged for {params:?}"
            );
        }
    }

    /// The optimistic timer mirror: same fallback, same zero rule, same 72h
    /// clamp the server applies.
    #[test]
    fn wake_at_mirrors_the_server_rule() {
        let hours: HashMap<String, u32> = [("clock".to_string(), 3u32)].into_iter().collect();
        assert_eq!(compute_wake_at("scores".into(), hours.clone(), 24, 1000), None);
        assert_eq!(compute_wake_at("clock".into(), hours.clone(), 24, 1000), Some(1000 + 3 * 3600));
        assert_eq!(compute_wake_at("off".into(), hours.clone(), 24, 1000), Some(1000 + 24 * 3600));
        let zero: HashMap<String, u32> = [("off".to_string(), 0u32)].into_iter().collect();
        assert_eq!(compute_wake_at("off".into(), zero, 24, 1000), None, "0 = until changed");
        let big: HashMap<String, u32> = [("off".to_string(), 99u32)].into_iter().collect();
        assert_eq!(compute_wake_at("off".into(), big, 24, 1000), Some(1000 + 72 * 3600), "clamped like the server");
    }

    /// "BMW CHAMPIONSHIP" must not become "Bmw Championship" — a word with
    /// no vowel is an initialism and keeps its caps (MAR-109). Y is a vowel
    /// here so pronounceable consonant-heavy names still fold.
    #[test]
    fn initialisms_keep_their_caps_in_event_titles() {
        assert_eq!(game_title("BMW CHAMPIONSHIP".into(), String::new()), "BMW Championship");
        assert_eq!(game_title("CPKC WOMENS OPEN".into(), String::new()), "CPKC Womens Open");
        assert_eq!(game_title("DUTCH GP".into(), String::new()), "Dutch GP");
        assert_eq!(game_title("MONACO GP".into(), String::new()), "Monaco GP");
    }

    #[test]
    fn the_picker_is_scores_then_off_no_dark_double() {
        let modes = vec![
            UiOffMode { id: "blank".into(), label: "Off".into(), description: String::new(), preview: None, input: None, fields: vec![] },
            UiOffMode { id: "clock".into(), label: "Clock".into(), description: String::new(), preview: Some("/offmode/clock.png".into()), input: None, fields: vec![] },
        ];
        let p = pickable_modes(modes);
        let ids: Vec<&str> = p.iter().map(|m| m.id.as_str()).collect();
        // Off second, matching the settings page where Off leads the idle
        // list (MAR-110); the blank descriptor still never becomes a second
        // dark tile.
        assert_eq!(ids, ["scores", "off", "clock"], "blank never becomes a second dark tile");
    }

    #[test]
    fn the_ring_follows_the_glass() {
        assert_eq!(effective_mode("scores".into(), true, false, "weather".into()), "off", "quiet wins");
        assert_eq!(effective_mode("scores".into(), false, true, "weather".into()), "weather");
        assert_eq!(effective_mode("scores".into(), false, true, "blank".into()), "off");
        assert_eq!(effective_mode("scores".into(), false, false, "weather".into()), "scores");
        assert_eq!(effective_mode("clock".into(), false, true, "weather".into()), "clock");
    }

    /// Upcoming and bumped entries ride along in "Live and Upcoming" but are
    /// not on the panel — two queued games and nothing live is an idle board.
    #[test]
    fn upcoming_games_do_not_light_the_scores_ring() {
        let mut queued = game("later", "upcoming", false);
        queued.queued = true;
        assert!(panel_shows_nothing(Some(vec![])));
        assert!(panel_shows_nothing(Some(vec![queued.clone(), game("cut", "live", true)])));
        assert!(!panel_shows_nothing(Some(vec![queued, game("on", "live", false)])));
        // Not known yet (the state has not arrived) is not an empty panel:
        // the picker keeps the configured mode instead of flashing idle.
        assert!(!panel_shows_nothing(None));
    }

    #[test]
    fn previews_carry_the_configuration_sorted_and_encoded() {
        let params: HashMap<String, String> =
            [("style".to_string(), "word".to_string()), ("loc".to_string(), "a b".to_string())]
                .into_iter()
                .collect();
        assert_eq!(
            configured_preview(Some("/offmode/clock.png".into()), params.clone()).as_deref(),
            Some("/offmode/clock.png?loc=a%20b&style=word")
        );
        // Non-offmode paths pass through untouched.
        assert_eq!(
            configured_preview(Some("/sample/plain.png?live=1".into()), params).as_deref(),
            Some("/sample/plain.png?live=1")
        );
    }

    /// The scheme mapping, pinned: a `wss` server must never resolve to an
    /// `http` origin, or every image fetch silently drops out of TLS.
    /// The one-way blanket rule: picking "all" selects every round, but
    /// un-picking one round must not clear the others — the inverse once
    /// turned a majors-only subscription into the whole season.
    #[test]
    fn the_blanket_option_implies_every_round_but_not_the_converse() {
        let all = vec!["all".to_string(), "majors".to_string(), "regular".to_string()];
        let picked = toggle_round("all".into(), vec![], all.clone());
        assert_eq!(picked, all, "the blanket option selects everything");
        // Un-pick ONE round: the rest survive, including the blanket entry —
        // no inference back the other way.
        let after = toggle_round("regular".into(), picked, all.clone());
        assert_eq!(after, ["all", "majors"], "one round off, the rest stay");
        // A single pick on an empty set is just that round.
        assert_eq!(toggle_round("majors".into(), vec![], all), ["majors"]);
    }

    #[test]
    fn depth_labels_name_f1s_full_field() {
        assert_eq!(leaderboard_depth_options("f1".into())[2].label, "Full field");
        assert_eq!(leaderboard_depth_options("pga".into())[2].label, "Top 20");
    }

    #[test]
    fn http_base_never_downgrades_a_tls_server() {
        assert_eq!(http_base("wss://live.example.com/ws".into()), "https://live.example.com");
        assert_eq!(http_base("ws://127.0.0.1:7878/ws".into()), "http://127.0.0.1:7878");
        // No /ws suffix, and an already-http origin, both pass through.
        assert_eq!(http_base("wss://live.example.com".into()), "https://live.example.com");
        assert_eq!(http_base("https://live.example.com".into()), "https://live.example.com");
        // The suffix rule is exact — a host ENDING in "ws" is not a suffix.
        assert_eq!(http_base("wss://news/ws".into()), "https://news");
    }

    #[test]
    fn brightness_snaps_to_the_knob_detents() {
        assert_eq!(snap_brightness(0.0), 10);
        assert_eq!(snap_brightness(44.9), 40);
        assert_eq!(snap_brightness(45.0), 50);
        assert_eq!(snap_brightness(250.0), 100);
    }

    /// The offline fallback descriptors track the server's labels — "Off",
    /// one darkness one name.
    #[test]
    fn fallback_descriptors_match_the_server_labels() {
        let f = fallback_off_modes();
        assert_eq!(f[1].id, "blank");
        assert_eq!(f[1].label, "Off");
        assert!(f[1].preview.is_none());
    }
}

#[cfg(test)]
mod provisioning_tests {
    use super::*;

    /// The pin grammar must match the FIRMWARE's, exactly. Anything the app
    /// accepts and the board refuses is a provisioning that fails where there
    /// is no text field to complain in — recoverable only by factory reset.
    #[test]
    fn the_pin_grammar_is_the_firmwares() {
        let good = "a".repeat(64);
        assert_eq!(normalized_pin(good.clone()), Some(good.clone()));
        // Labels, quotes, whitespace and case are packaging, not pin.
        assert_eq!(normalized_pin(format!("  SERVER_PIN=\"{}\"\n", good.to_uppercase())), Some(good.clone()));
        assert_eq!(normalized_pin(format!("pin: {good}")), Some(good.clone()));
        // The three refusals.
        assert_eq!(normalized_pin("0".repeat(64)), None, "the all-zero placeholder is not a key");
        assert_eq!(normalized_pin("a".repeat(63)), None, "63 is not 64");
        assert_eq!(normalized_pin(format!("{}g", "a".repeat(63))), None, "g is not hex");
        // Full-width digits are NOT hex — the firmware's parser rejects them,
        // and Swift's isHexDigit famously does not.
        assert_eq!(normalized_pin("１".repeat(64)), None, "full-width digits are refused");
    }

    #[test]
    fn pin_complaints_name_the_actual_problem() {
        assert_eq!(pin_complaint(String::new()), None, "empty is each screen's own phrasing");
        assert_eq!(pin_complaint("a".repeat(64)), None);
        assert!(pin_complaint("zz".into()).unwrap().contains("hexadecimal"));
        assert!(pin_complaint("abc".into()).unwrap().contains("64 characters"));
        assert!(pin_complaint("0".repeat(64)).unwrap().contains("all-zero"));
    }

    /// A hosted preset must send the board its TCP PROXY, never the HTTPS edge
    /// the phone uses — the edge answers nothing on the board's path.
    #[test]
    fn a_hosted_board_is_given_the_proxy_not_the_edge() {
        assert_eq!(
            host_port_for_board("wss://live-production-118c.up.railway.app/ws".into()),
            "hayabusa.proxy.rlwy.net:47958"
        );
        // A self-hosted server has one address, so it comes from the URL.
        assert_eq!(host_port_for_board("wss://my.server.example/ws".into()), "my.server.example:7878");
        assert_eq!(host_port_for_board("ws://10.0.0.5:9000/ws".into()), "10.0.0.5:9000");
        assert_eq!(host_port_for_board(String::new()), "");
    }

    /// The password write triggers the board's save-and-reboot, so it MUST be
    /// last: a pin or server written after it never arrives.
    #[test]
    fn the_password_is_always_the_last_write() {
        let steps = provision_steps(
            "home-wifi".into(),
            "hunter2".into(),
            "wss://my.server.example/ws".into(),
            "b".repeat(64),
            true,
            true,
        )
        .unwrap();
        let uuids: Vec<&str> = steps.iter().map(|s| s.uuid.as_str()).collect();
        assert_eq!(uuids, [BLE_PIN_UUID, BLE_SERVER_UUID, BLE_SSID_UUID, BLE_PASS_UUID]);
        assert_eq!(steps.last().unwrap().value, "hunter2");
    }

    /// Older firmware without the optional characteristics still provisions —
    /// it keeps its compiled default server.
    #[test]
    fn a_board_without_the_optional_characteristics_still_provisions() {
        let steps =
            provision_steps("net".into(), "pw".into(), "wss://x/ws".into(), String::new(), false, false)
                .unwrap();
        let uuids: Vec<&str> = steps.iter().map(|s| s.uuid.as_str()).collect();
        assert_eq!(uuids, [BLE_SSID_UUID, BLE_PASS_UUID]);
        // ...but a pin it cannot accept is refused BEFORE any write, rather
        // than half-provisioning the board.
        let err = provision_steps(
            "net".into(), "pw".into(), "wss://x/ws".into(), "c".repeat(64), false, true,
        );
        assert!(err.is_err(), "a pin for firmware that has no pin characteristic must refuse");
    }

    #[test]
    fn a_bad_pin_refuses_before_anything_is_written() {
        let err = provision_steps(
            "net".into(), "pw".into(), "wss://x/ws".into(), "nope".into(), true, true,
        );
        assert!(matches!(err, Err(ProvisionError::Refused { .. })));
    }

    #[test]
    fn networks_parse_from_the_padded_characteristic() {
        let mut raw = b"HomeWiFi\nNeighbour 5G\n".to_vec();
        raw.resize(180, 0);
        assert_eq!(parse_networks(raw), ["HomeWiFi", "Neighbour 5G"]);
        assert!(parse_networks(vec![0u8; 180]).is_empty());
    }
}
