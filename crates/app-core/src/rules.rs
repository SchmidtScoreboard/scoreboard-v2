//! Game, rotation, and mode RULES — the derivations both platforms must make
//! identically, moved down from the iOS view model so Kotlin never grows a
//! second copy. Same charter as `strings`: one place a rule can be wrong.
//!
//! Everything here is a pure function over owned uniffi values. The platform
//! keeps its own state and its own client calls; the core answers "given
//! these facts, what is true / what changes" — which is exactly the part
//! that drifted three times inside ONE platform before this module existed.

use std::collections::{BTreeMap, HashMap};

use scoreboard_protocol::{
    BoardConfig, FieldCondition, OffModeChoice, OffModeDescriptor, OffModeField, OffModeInput,
    ScreenConfig, IDLE_CYCLE_SECS, MAX_SCREENS, MODE_OFF, MODE_SCORES, OFF_LABEL, OFF_MODE_BLANK,
    OFF_MODE_CLOCK, SCORES_LABEL,
};

use crate::{UiGame, UiOffMode, UiOffModeChoice, UiOffModeField, UiOffModeInput, UiScreenConfig};

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
/// fewer (GP, Q) stay caps, and so do initialisms.
#[uniffi::export]
pub fn game_title(away: String, home: String) -> String {
    if !home.is_empty() {
        return format!("{away} @ {home}");
    }
    away.split(' ')
        .enumerate()
        .map(|(i, w)| {
            // Initialisms WITH a vowel, which the vowel rule below cannot
            // tell from words: "Pga Champ", "Lpga Shanghai", "Aig Women's
            // Open" and "Cme Group Tour" were all on the 2026 calendars.
            if matches!(w, "PGA" | "LPGA" | "AIG" | "CME" | "ISCO") {
                return w.to_string();
            }
            match w {
                "FEDEX" => return "FedEx".to_string(),
                // Saint, not an initialism: no vowel, but "ST." is not "BMW".
                "ST." => return "St.".to_string(),
                _ => {}
            }
            // A connector mid-title is lower case: "Bank of Utah", not the
            // two-letter rule's "Bank OF Utah".
            if i > 0 && matches!(w, "OF" | "AT" | "IN" | "BY") {
                return w.to_lowercase();
            }
            // Short words stay caps ("GP", "US") — and so does any word with
            // no vowel: "BMW" and "CPKC" are initialisms, and "Bmw" reads as
            // a typo. Y counts as a vowel so "LYNX"-shaped words still fold.
            let vowelless =
                !w.chars().any(|c| matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u' | 'y'));
            if w.chars().count() <= 2 || vowelless {
                w.to_string()
            } else {
                // Each part of a hyphenated word: "Q-School", not "Q-school".
                w.split('-')
                    .map(|part| {
                        let mut cs = part.chars();
                        match cs.next() {
                            Some(f) => f.to_string() + &cs.as_str().to_lowercase(),
                            None => String::new(),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("-")
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

/// The optimistic mirror of the server's `apply_mode` hold, for picking
/// screen `id` (an instance id, or "off" for Off): Sports runs on no timer;
/// every other pick, Off included, holds for the protocol's `HOLD_HOURS`
/// (24) from `now_unix`. There is no per-screen timer — if these disagreed,
/// the app's countdown would lie until the next push.
#[uniffi::export]
pub fn compute_wake_at(id: String, now_unix: u64) -> Option<u64> {
    if id.is_empty() || id == MODE_SCORES {
        return None;
    }
    Some(now_unix + u64::from(scoreboard_protocol::HOLD_HOURS) * 3600)
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

/// What a board shows before the server's catalog arrives: the stock
/// server's descriptors, in its order, so a list synthesized in that window
/// already reads like the real one — with Weather present, a weather-idle
/// board shows Weather checked rather than a Clock the fallback made up.
/// Labels must match the server's ("Off", not "Nothing" — one darkness, one
/// name). No fields: settings pages fill in when the catalog lands, and
/// writes made before then keep every param they carry (see the mirrors,
/// which read this list exactly like an empty one).
/// Prefer `off_modes_or_fallback`, which applies this only when needed.
#[uniffi::export]
pub fn fallback_off_modes() -> Vec<UiOffMode> {
    let m = |id: &str, label: &str, description: &str, preview: Option<&str>| UiOffMode {
        id: id.into(),
        label: label.into(),
        description: description.into(),
        preview: preview.map(Into::into),
        input: None,
        fields: vec![],
    };
    vec![
        m(OFF_MODE_BLANK, "Off", "The panel stays dark.", None),
        m(OFF_MODE_CLOCK, "Clock", "The time, in the scoreboard's timezone.", Some("/offmode/clock.png")),
        m("weather", "Weather", "Conditions where the scoreboard lives.", Some("/offmode/weather.png")),
    ]
}

/// The off modes to work from: the server's declared list, or
/// [`fallback_off_modes`] while it has declared none (the catalog has not
/// arrived). Both platforms read descriptors through this, so neither has a
/// cold-start gap the other lacks.
#[uniffi::export]
pub fn off_modes_or_fallback(declared: Vec<UiOffMode>) -> Vec<UiOffMode> {
    if declared.is_empty() { fallback_off_modes() } else { declared }
}

/// LEGACY picker tiles (one per descriptor kind). The rail is built from
/// [`screen_list`] now, which knows instances; kept for older callers.
/// Every pickable mode, in display order: Sports first (the resting state),
/// the server's lit screens, Off last. The dark idle descriptor is excluded —
/// the Off entry IS that darkness, and two dark tiles with different meanings
/// is a trap.
#[uniffi::export]
pub fn pickable_modes(off_modes: Vec<UiOffMode>) -> Vec<UiPickableMode> {
    let mut out = vec![UiPickableMode {
        id: MODE_SCORES.into(),
        label: SCORES_LABEL.into(),
        path: Some(SPORTS_PREVIEW.into()),
    }];
    // Off comes second, matching the Screen settings page where Off leads
    // the idle-screen list — the two orderings must read as one system.
    out.push(UiPickableMode { id: MODE_OFF.into(), label: OFF_LABEL.into(), path: None });
    for m in off_modes {
        if m.preview.is_some() {
            out.push(UiPickableMode { id: m.id, label: m.label, path: m.preview });
        }
    }
    out
}

/// A preview path carrying a configuration: `params` ride along as a sorted,
/// percent-encoded query, sorted so the URL is stable and the platform image
/// cache can hit.
///
/// The CALLER chooses the params, and passing a whole map is no longer
/// harmless: two screens of one kind must not share a URL (or a cached
/// image), a key must never appear twice (`path` from a choice already
/// carries `?variant=`), and a typed location must not ride into an on-disk
/// URL cache. [`screen_list`] builds every screen's preview correctly — the
/// descriptor's own path, its declared non-text keys, the screen's per-screen
/// values over the board-wide ones. Use it rather than calling this.
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

/// A DESCRIPTOR id's display label: Sports/Off for the reserved ids, the
/// descriptor's label otherwise, capitalized id as the last resort. Knows
/// nothing of instances — label a screen or a held mode with
/// [`screen_label_for`].
#[uniffi::export]
pub fn mode_label(id: String, off_modes: Vec<UiOffMode>) -> String {
    match id.as_str() {
        MODE_SCORES => SCORES_LABEL.into(),
        MODE_OFF | OFF_MODE_BLANK => OFF_LABEL.into(),
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

// ---- screens (MAR-117) ----
//
// A board's screens are a LIST of instances: Sports, Off, then the user's
// screens in their order, some of them checked to show when nothing is on.
// Every rule below wraps the protocol's own function — the server runs the
// same one — so an optimistic edit shows exactly what the server will store.
// The platforms hold the raw `UiScreenConfig` list, transform it only through
// these mirrors, send it with `set_screens`, and draw rows from
// `screen_list`. No platform reads an id or a kind to decide anything.
//
// INPUT CONTRACT: every function here taking `screens: Vec<UiScreenConfig>`
// wants the RESOLVED list — `resolved_screen_configs(config.screens,
// off_mode, off_mode_params, off_modes)` — not the raw `config.screens`,
// which is EMPTY on a server without lists. (`screen_list` takes the raw
// fields and resolves itself.) The destructive paths are guarded anyway: a
// mirror returns an empty input unchanged, `held_after` never releases
// against one, and `set_screens` sends nothing for a list that cannot be a
// board's.

/// Sports' preview tile: a sample live game. Not a descriptor path — Sports
/// is not an off mode.
const SPORTS_PREVIEW: &str = "/sample/plain.png?live=1";

pub(crate) fn to_screen_config(s: UiScreenConfig) -> ScreenConfig {
    ScreenConfig { id: s.id, kind: s.kind, params: s.params.into_iter().collect(), idle: s.idle }
}

pub(crate) fn to_ui_screen_config(s: ScreenConfig) -> UiScreenConfig {
    UiScreenConfig { id: s.id, kind: s.kind, params: s.params.into_iter().collect(), idle: s.idle }
}

fn to_configs(screens: Vec<UiScreenConfig>) -> Vec<ScreenConfig> {
    screens.into_iter().map(to_screen_config).collect()
}

fn to_ui_configs(screens: Vec<ScreenConfig>) -> Vec<UiScreenConfig> {
    screens.into_iter().map(to_ui_screen_config).collect()
}

/// The protocol descriptor a `UiOffMode` came from — lossless for everything
/// the protocol's screen rules read (ids, labels, previews, fields with their
/// kinds, keys, defaults, options, visibility and `per_screen`), so the core
/// can run THE server function rather than a copy of it.
pub(crate) fn to_descriptor(m: &UiOffMode) -> OffModeDescriptor {
    OffModeDescriptor {
        id: m.id.clone(),
        label: m.label.clone(),
        description: m.description.clone(),
        preview: m.preview.clone(),
        input: m.input.as_ref().map(|i: &UiOffModeInput| OffModeInput {
            key: i.key.clone(),
            label: i.label.clone(),
            placeholder: i.placeholder.clone(),
            max_len: i.max_len,
        }),
        fields: m
            .fields
            .iter()
            .map(|f| OffModeField {
                key: f.key.clone(),
                label: f.label.clone(),
                kind: f.kind.clone(),
                placeholder: f.placeholder.clone(),
                max_len: f.max_len,
                default: f.default.clone(),
                options: f
                    .options
                    .iter()
                    .map(|o: &UiOffModeChoice| OffModeChoice {
                        value: o.value.clone(),
                        label: o.label.clone(),
                        preview: o.preview.clone(),
                        screen_label: o.screen_label.clone(),
                    })
                    .collect(),
                visible_when: match (&f.visible_when_key, &f.visible_when_value) {
                    (Some(key), Some(equals)) => Some(FieldCondition { key: key.clone(), equals: equals.clone() }),
                    _ => None,
                },
                per_screen: f.per_screen,
            })
            .collect(),
    }
}

/// The catalog to READ with: the server's, or the fallback before it lands.
fn catalog(off_modes: Vec<UiOffMode>) -> Vec<OffModeDescriptor> {
    off_modes_or_fallback(off_modes).iter().map(to_descriptor).collect()
}

/// Is `off_modes` the stand-in for a catalog that has not arrived — empty,
/// or exactly what [`off_modes_or_fallback`] substitutes for empty? A
/// platform that reads through `off_modes_or_fallback` everywhere hands the
/// mirrors the fallback, and that must mean the same thing as handing them
/// nothing. (A real server's catalog differs: the stock one declares fields,
/// and a fork's has its own ids.)
fn is_stand_in(off_modes: &[UiOffMode]) -> bool {
    let fb = fallback_off_modes();
    off_modes.is_empty()
        || (off_modes.len() == fb.len()
            && off_modes.iter().zip(&fb).all(|(a, b)| a.id == b.id && a.fields.is_empty() && a.input.is_none()))
}

/// The catalog a mirror normalizes against. With the server's catalog in
/// hand, exactly it — the mirror then equals what the server stores. Before
/// it arrives (`off_modes` empty, or the fallback that stands in for it —
/// [`is_stand_in`]), the fallback made LENIENT: a kind it does not know, and
/// a param key it does not declare, are kept as they are rather than
/// dropped, because the list goes to the server, and an edit made in that
/// window must not delete a fork's screen or a layout the fallback simply
/// cannot describe. The server's own normalize still has the last word.
fn write_catalog(screens: &[ScreenConfig], off_modes: Vec<UiOffMode>) -> Vec<OffModeDescriptor> {
    if !is_stand_in(&off_modes) {
        return off_modes.iter().map(to_descriptor).collect();
    }
    let mut cat = catalog(vec![]);
    for s in screens.iter().filter(|s| !s.is_fixed() && s.kind != OFF_MODE_BLANK) {
        let i = match cat.iter().position(|d| d.id == s.kind) {
            Some(i) => i,
            None => {
                cat.push(OffModeDescriptor {
                    id: s.kind.clone(),
                    label: s.kind.clone(),
                    description: String::new(),
                    preview: Some(String::new()),
                    input: None,
                    fields: vec![],
                });
                cat.len() - 1
            }
        };
        let d = &mut cat[i];
        for key in s.params.keys() {
            if !d.per_screen(key) {
                // An unknown field kind validates anything (a fork's field).
                d.fields.push(OffModeField {
                    key: key.clone(),
                    label: key.clone(),
                    kind: String::new(),
                    placeholder: String::new(),
                    max_len: 0,
                    default: String::new(),
                    options: vec![],
                    visible_when: None,
                    per_screen: true,
                });
            }
        }
    }
    cat
}

/// The list as the server has it — verbatim when it sent one, else
/// synthesized from the legacy fields by the protocol's `resolved_screens`,
/// which is how the board itself behaves on a server without lists.
fn resolve_list(
    screens: Vec<UiScreenConfig>,
    off_mode: String,
    off_mode_params: HashMap<String, String>,
    cat: &[OffModeDescriptor],
) -> Vec<ScreenConfig> {
    if !screens.is_empty() {
        return to_configs(screens);
    }
    let mut cfg = BoardConfig::default();
    if !off_mode.is_empty() {
        cfg.off_mode = Some(off_mode);
    }
    cfg.off_mode_params = off_mode_params.into_iter().collect();
    scoreboard_protocol::resolved_screens(&cfg, cat)
}

/// The board's screen list to EDIT: `UiBoardConfig.screens` when the server
/// sent one, else the list a server without lists is running, synthesized
/// from `off_mode`/`off_mode_params`. Feed this — never the raw, possibly
/// empty field — to the mirrors (`screens_with_idle`, ...), `ring` and
/// `screen_label_for`.
#[uniffi::export]
pub fn resolved_screen_configs(
    screens: Vec<UiScreenConfig>,
    off_mode: String,
    off_mode_params: HashMap<String, String>,
    off_modes: Vec<UiOffMode>,
) -> Vec<UiScreenConfig> {
    to_ui_configs(resolve_list(screens, off_mode, off_mode_params, &catalog(off_modes)))
}

/// One row of the Screens list and one tile of the dashboard rail.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct UiScreen {
    /// The instance id — key rows and tiles by it, and pass it to
    /// `set_mode` and the mirrors.
    pub id: String,
    /// The descriptor id (or "scores"/"off"). For looking up the descriptor
    /// whose fields the screen's page draws; never for branching.
    pub kind: String,
    /// Distinct across the list (protocol `screen_labels`).
    pub label: String,
    /// Complete server path for the preview; None draws a black panel (an
    /// all-black PNG cannot be told apart from a failed load).
    pub preview: Option<String>,
    /// Checked: shown when nothing is on.
    pub idle: bool,
    /// Sports or Off: pinned first and second, never removed, reordered or
    /// checked. Render them apart from the movable rows.
    pub fixed: bool,
    /// Has a page (fields, the idle checkmark, Remove): every user screen.
    /// False for Sports and Off, which have nothing to set — draw them with
    /// no chevron and no navigation.
    pub has_settings: bool,
    /// Offer Remove (a swipe, a menu item, the page's button): a user's
    /// screen on a server that stores lists. False for Sports and Off, and
    /// for every row of a list synthesized for a server without lists (or
    /// before the board's config arrives) — removing there cannot land.
    pub removable: bool,
}

/// The params a screen's preview carries: the keys its descriptor declares,
/// minus text fields (a typed location would ride into the URL cache for
/// nothing — previews render sample weather), with the screen's own values
/// over the board-wide ones. A per-screen key is NEVER taken from the flat
/// map: that holds the first instance's value as a mirror for old apps, and
/// reading it would give a defaulted sibling the other screen's layout.
fn preview_params(
    d: &OffModeDescriptor,
    flat: &HashMap<String, String>,
    own: &BTreeMap<String, String>,
) -> HashMap<String, String> {
    let shown = |k: &str| d.fields.iter().any(|f| f.key == k && f.kind != "text");
    let mut out: HashMap<String, String> = flat
        .iter()
        .filter(|(k, _)| shown(k) && !d.per_screen(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (k, v) in own {
        if shown(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// Every screen to draw, in the board's order: Sports, Off, then the user's
/// screens, each with its label, preview and flags. Takes only the fields a
/// platform keeps per board (never a whole `UiBoardConfig`) and works before
/// the catalog arrives and against a server without lists, which it
/// synthesizes for.
#[uniffi::export]
pub fn screen_list(
    screens: Vec<UiScreenConfig>,
    off_mode: String,
    off_mode_params: HashMap<String, String>,
    off_modes: Vec<UiOffMode>,
) -> Vec<UiScreen> {
    let cat = catalog(off_modes);
    // A server that stores lists always sends one (Sports and Off at least).
    let listed = !screens.is_empty();
    let list = resolve_list(screens, off_mode, off_mode_params.clone(), &cat);
    let labels = scoreboard_protocol::screen_labels(&list, &cat);
    list.into_iter()
        .zip(labels)
        .map(|(s, label)| {
            let preview = match s.kind.as_str() {
                MODE_SCORES => Some(SPORTS_PREVIEW.to_string()),
                MODE_OFF => None,
                _ => cat.iter().find(|d| d.id == s.kind).and_then(|d| {
                    configured_preview(d.preview.clone(), preview_params(d, &off_mode_params, &s.params))
                }),
            };
            UiScreen {
                fixed: s.is_fixed(),
                removable: listed && !s.is_fixed(),
                has_settings: !s.is_fixed(),
                idle: s.idle,
                id: s.id,
                kind: s.kind,
                label,
                preview,
            }
        })
        .collect()
}

/// The params screen `id` RENDERS with — protocol's `layered_params`: the
/// board-wide map minus every key the screen's descriptor marks
/// `per_screen`, then the screen's own values. Pass this, never a hand-made
/// overlay, to `visible_off_mode_fields` and `off_mode_value` on a screen's
/// page. The naive overlay (board map, then the screen's) is wrong: the
/// board map holds ONE instance's layout as a mirror for older apps, so a
/// defaulted sibling would read — and tick — the other screen's layout.
///
/// An id not in `screens` (Sports, Off, a list from a server without lists)
/// gets the board map as it is: such a server declares no per-screen keys,
/// so its map IS what it renders with.
#[uniffi::export]
pub fn screen_params(
    id: String,
    screens: Vec<UiScreenConfig>,
    off_mode_params: HashMap<String, String>,
    off_modes: Vec<UiOffMode>,
) -> HashMap<String, String> {
    let Some(s) = screens.into_iter().find(|s| s.id == id) else { return off_mode_params };
    let flat: BTreeMap<String, String> = off_mode_params.into_iter().collect();
    scoreboard_protocol::layered_params(&flat, &to_screen_config(s), &catalog(off_modes)).into_iter().collect()
}

/// Run one edit on the list, then THE normalize the server runs on save.
///
/// An EMPTY input is returned as it is. A list the server sent always holds
/// Sports and Off, so an empty one is `UiBoardConfig.screens` from a server
/// without lists, passed raw instead of through `resolved_screen_configs`;
/// normalizing it would yield a bare [Sports, Off] — "nothing checked" — and
/// `set_screens` would turn that board's idle screen dark.
fn mirror(
    screens: Vec<UiScreenConfig>,
    off_modes: Vec<UiOffMode>,
    edit: impl FnOnce(&mut Vec<ScreenConfig>),
) -> Vec<ScreenConfig> {
    if screens.is_empty() {
        return vec![];
    }
    let mut list = to_configs(screens);
    let cat = write_catalog(&list, off_modes);
    edit(&mut list);
    scoreboard_protocol::normalize_screens(list, &cat)
}

/// What [`screens_added`] produced: the new list, and the new screen's id —
/// empty when it did not survive (the list was full, or the server does not
/// offer the kind).
#[derive(uniffi::Record, Clone, Debug)]
pub struct UiScreensAdded {
    pub screens: Vec<UiScreenConfig>,
    pub id: String,
}

/// Add a screen of `kind` with a preset's `params` (from `addable_screens`)
/// at the end of the list, unchecked. The id is minted by the protocol's
/// `mint_screen_id`, the function the server mints with.
#[uniffi::export]
pub fn screens_added(
    screens: Vec<UiScreenConfig>,
    kind: String,
    params: HashMap<String, String>,
    off_modes: Vec<UiOffMode>,
) -> UiScreensAdded {
    let mut id = String::new();
    let list = mirror(screens, off_modes, |list| {
        id = scoreboard_protocol::mint_screen_id(&kind, list.iter().map(|s| s.id.as_str()));
        list.push(ScreenConfig { id: id.clone(), kind, params: params.into_iter().collect(), idle: false });
    });
    if !list.iter().any(|s| s.id == id) {
        id.clear();
    }
    UiScreensAdded { screens: to_ui_configs(list), id }
}

/// The board after [`screens_removed`]: every field the server's remove
/// changes, to apply to the platform's record in one go.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct UiScreensRemoved {
    pub screens: Vec<UiScreenConfig>,
    /// The held screen: unchanged, or "scores" when the removed screen was
    /// the one held (the server's release).
    pub mode: String,
    /// The `power` twin of `mode` ([`mode_power`]).
    pub power: bool,
    /// Cleared by a release; otherwise as it was.
    pub wake_at: Option<u64>,
}

/// Remove screen `id` — the server's whole `SetScreens` remove, mirrored:
/// the list normalized, and the board released to Sports (and its wake
/// timer cleared) when `mode` was the removed screen — `release_to_scores`,
/// never a quiet override. Sports and Off cannot be removed. Send `screens`
/// with `set_screens`.
#[uniffi::export]
pub fn screens_removed(
    screens: Vec<UiScreenConfig>,
    id: String,
    mode: String,
    wake_at: Option<u64>,
    off_modes: Vec<UiOffMode>,
) -> UiScreensRemoved {
    if screens.is_empty() {
        // Not a list (see `mirror`): change nothing.
        return UiScreensRemoved { screens, power: mode_power(mode.clone()), mode, wake_at };
    }
    let list = mirror(screens, off_modes, |list| list.retain(|s| s.is_fixed() || s.id != id));
    let screens = to_ui_configs(list);
    let held = held_after(mode.clone(), screens.clone());
    let wake_at = if held == mode { wake_at } else { None };
    UiScreensRemoved { power: mode_power(held.clone()), mode: held, wake_at, screens }
}

/// Reorder by ID — the order the user left the movable rows in (Swift's
/// `onMove` and a Kotlin drag count indices differently; ids do not). Ids it
/// does not know are ignored, screens it omits keep their relative order
/// after the named ones, and Sports and Off stay first and second wherever
/// they were named.
#[uniffi::export]
pub fn screens_with_order(
    screens: Vec<UiScreenConfig>,
    ids: Vec<String>,
    off_modes: Vec<UiOffMode>,
) -> Vec<UiScreenConfig> {
    to_ui_configs(mirror(screens, off_modes, |list| {
        let mut rest = std::mem::take(list);
        for id in &ids {
            if let Some(i) = rest.iter().position(|s| &s.id == id) {
                list.push(rest.remove(i));
            }
        }
        list.append(&mut rest);
    }))
}

/// Check or uncheck screen `id` for showing when nothing is on (multi-select:
/// the rest stay as they are). Sports and Off cannot be checked
/// ([`can_set_idle`]).
#[uniffi::export]
pub fn screens_with_idle(
    screens: Vec<UiScreenConfig>,
    id: String,
    on: bool,
    off_modes: Vec<UiOffMode>,
) -> Vec<UiScreenConfig> {
    to_ui_configs(mirror(screens, off_modes, |list| {
        for s in list.iter_mut().filter(|s| s.id == id && !s.is_fixed()) {
            s.idle = on;
        }
    }))
}

/// The single-select form of [`screens_with_idle`], for a server without
/// lists ([`idle_single_select`]): checking `id` makes it the ONLY checked
/// screen; unchecking it leaves none (the board stays dark when idle). That
/// is all such a server can store — `set_screens` routes it to `SetOffMode`.
#[uniffi::export]
pub fn screens_with_sole_idle(
    screens: Vec<UiScreenConfig>,
    id: String,
    on: bool,
    off_modes: Vec<UiOffMode>,
) -> Vec<UiScreenConfig> {
    to_ui_configs(mirror(screens, off_modes, |list| {
        for s in list.iter_mut().filter(|s| !s.is_fixed()) {
            s.idle = on && s.id == id;
        }
    }))
}

/// Set one PER-SCREEN value (`UiOffModeField.per_screen`) of screen `id`. An
/// empty value removes the key — the field's default — as the server does; a
/// value the descriptor would refuse is dropped by the normalize, as the
/// server's is. Board-wide fields go through `set_off_mode_param` instead.
#[uniffi::export]
pub fn screens_with_param(
    screens: Vec<UiScreenConfig>,
    id: String,
    key: String,
    value: String,
    off_modes: Vec<UiOffMode>,
) -> Vec<UiScreenConfig> {
    to_ui_configs(mirror(screens, off_modes, |list| {
        for s in list.iter_mut().filter(|s| s.id == id && !s.is_fixed()) {
            if value.is_empty() {
                s.params.remove(&key);
            } else {
                s.params.insert(key.clone(), value.clone());
            }
        }
    }))
}

/// The held screen once the list is `screens`: `held_id` while it is still
/// listed — Sports and Off always are — else "scores", the server's release.
/// EXACT id match (the protocol's `held_screen_listed`): removing a held
/// first instance releases the board even while a sibling of its kind stays.
/// [`screens_removed`] already applies it (with the wake timer and power a
/// release changes); this is the rule on its own, for checking a held id
/// against a list pushed from elsewhere.
#[uniffi::export]
pub fn held_after(held_id: String, screens: Vec<UiScreenConfig>) -> String {
    if screens.is_empty() {
        // No list to judge by (see `mirror`): never a release.
        return held_id;
    }
    let mut cfg = BoardConfig::default();
    cfg.set_mode(&held_id);
    if cfg.held_screen_listed(&to_configs(screens)) { held_id } else { MODE_SCORES.into() }
}

/// May another screen be added? (The server keeps at most `MAX_SCREENS`
/// rows, Sports and Off included.) When false, show [`crate::strings::screen_cap_note`].
/// The Add control itself is gated by [`add_screen_note`], which also knows
/// when the chooser would be empty.
#[uniffi::export]
pub fn can_add_screen(screens: Vec<UiScreenConfig>) -> bool {
    screens.len() < MAX_SCREENS
}

/// Why Add Screen is unavailable, or None when it is available. The list is
/// full ([`crate::strings::screen_cap_note`]), or every screen the server
/// offers is already on it ([`crate::strings::nothing_to_add_note`]), or the
/// catalog has not arrived yet ([`crate::strings::screens_loading_note`]). Both
/// platforms disable Add exactly when this is Some, and show the sentence.
#[uniffi::export]
pub fn add_screen_note(screens: Vec<UiScreenConfig>, off_modes: Vec<UiOffMode>) -> Option<String> {
    if !can_add_screen(screens.clone()) {
        return Some(crate::strings::screen_cap_note());
    }
    // Before the catalog lands (the board's state arrives first on every
    // connect) the stand-in knows only the clock, which would read as
    // "nothing left to add".
    if is_stand_in(&off_modes) {
        return Some(crate::strings::screens_loading_note());
    }
    if addable_screens(screens, off_modes).is_empty() {
        return Some(crate::strings::nothing_to_add_note());
    }
    None
}

/// May screen `id` be checked or unchecked? Not Sports or Off, and not an id
/// the list does not have.
#[uniffi::export]
pub fn can_set_idle(screens: Vec<UiScreenConfig>, id: String) -> bool {
    screens.into_iter().map(to_screen_config).any(|s| s.id == id && !s.is_fixed())
}

/// Can screens be added, removed and reordered, and the idle timer set? Only
/// on a server that understands lists; otherwise hide those controls and
/// show [`crate::strings::update_server_note`].
#[uniffi::export]
pub fn screens_editable(screens_supported: bool) -> bool {
    screens_supported
}

/// Do the checkmarks act as single-select ([`screens_with_sole_idle`])? On a
/// server without lists, which stores exactly one idle screen.
#[uniffi::export]
pub fn idle_single_select(screens_supported: bool) -> bool {
    !screens_supported
}

/// The off mode a server without lists stores for `screens`: the first
/// checked screen's kind, else "blank" (nothing checked = dark when idle).
/// Exactly what `set_screens` sends there as `SetOffMode`.
pub(crate) fn legacy_off_mode(screens: &[UiScreenConfig]) -> String {
    screens
        .iter()
        .find(|s| s.idle && s.kind != MODE_SCORES && s.kind != MODE_OFF)
        .map_or_else(|| OFF_MODE_BLANK.to_string(), |s| s.kind.clone())
}

/// What an optimistic screens edit writes into the board's record — the
/// fields `set_screens` will actually change on the server. None = leave
/// that field as it is.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct UiScreensWrite {
    pub screens: Option<Vec<UiScreenConfig>>,
    pub off_mode: Option<String>,
}

/// Apply this, never `screens` directly, before `set_screens(screens)`.
///
/// On a server that stores lists, the list itself. On one without lists the
/// record's `screens` must STAY EMPTY — empty is how every rule here knows
/// such a server (rows are not removable, the mirrors leave it alone) — and
/// what changes is `off_mode`, the single idle screen that server stores.
/// The resolved list then re-synthesizes from it, so the tick still shows.
/// Nothing at all for an empty list (`set_screens` sends nothing then).
#[uniffi::export]
pub fn screens_write(screens_supported: bool, screens: Vec<UiScreenConfig>) -> UiScreensWrite {
    let nothing = UiScreensWrite { screens: None, off_mode: None };
    if screens.is_empty() {
        return nothing;
    }
    if screens_supported {
        return UiScreensWrite { screens: Some(screens), off_mode: None };
    }
    if screens.iter().all(|s| s.kind == MODE_SCORES || s.kind == MODE_OFF) {
        return nothing;
    }
    UiScreensWrite { screens: None, off_mode: Some(legacy_off_mode(&screens)) }
}

/// One entry of the Add Screen sheet.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct UiAddableScreen {
    /// Pass with `params` to `screens_added`.
    pub kind: String,
    pub params: HashMap<String, String>,
    pub label: String,
    pub description: String,
    pub preview: Option<String>,
}

/// The first per-screen choice field whose options carry previews — the
/// field that tells one screen of a kind from another (a layout, a style).
fn preset_field(d: &OffModeDescriptor) -> Option<&OffModeField> {
    d.fields
        .iter()
        .find(|f| f.per_screen && f.kind == "choice" && f.options.iter().any(|o| o.preview.is_some()))
}

/// What can be added, grouped by kind in catalog order: one preset per
/// layout or style for a kind that has them (Big Weather, Forecast, Word
/// Clock ...), else one per kind. A preset already on the list — a screen of
/// its kind whose resolved layout matches, defaults included — is left out,
/// since two identical screens are only confusing.
#[uniffi::export]
pub fn addable_screens(screens: Vec<UiScreenConfig>, off_modes: Vec<UiOffMode>) -> Vec<UiAddableScreen> {
    let list = to_configs(screens);
    let mut out = Vec::new();
    for d in catalog(off_modes).iter().filter(|d| d.lit()) {
        match preset_field(d) {
            Some(f) => {
                for o in &f.options {
                    let present = list.iter().any(|s| s.kind == d.id && d.value(&f.key, &s.params) == o.value);
                    if present {
                        continue;
                    }
                    let params: HashMap<String, String> = [(f.key.clone(), o.value.clone())].into_iter().collect();
                    out.push(UiAddableScreen {
                        kind: d.id.clone(),
                        label: match o.screen_label.as_deref().filter(|l| !l.is_empty()) {
                            Some(l) => l.to_string(),
                            None => format!("{} · {}", d.label, o.label),
                        },
                        description: d.description.clone(),
                        preview: o.preview.clone().or_else(|| configured_preview(d.preview.clone(), params.clone())),
                        params,
                    });
                }
            }
            None => {
                if list.iter().any(|s| s.kind == d.id) {
                    continue;
                }
                out.push(UiAddableScreen {
                    kind: d.id.clone(),
                    params: HashMap::new(),
                    label: d.label.clone(),
                    description: d.description.clone(),
                    preview: d.preview.clone(),
                });
            }
        }
    }
    out
}

/// One section of the Add Screen chooser: a kind's presets under the
/// descriptor's name, with its description.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct UiAddableGroup {
    /// A stable key for the section (the descriptor id). Never for branching.
    pub kind: String,
    /// The section's heading: the descriptor's label ("Weather").
    pub label: String,
    /// The descriptor's description, once per section.
    pub description: String,
    pub screens: Vec<UiAddableScreen>,
}

/// [`addable_screens`], grouped for the chooser: one group per kind in
/// catalog order, each headed by its descriptor's label. Both platforms draw
/// these sections — the same headings, the same description per kind.
#[uniffi::export]
pub fn addable_screen_groups(screens: Vec<UiScreenConfig>, off_modes: Vec<UiOffMode>) -> Vec<UiAddableGroup> {
    let cat = catalog(off_modes.clone());
    let mut out: Vec<UiAddableGroup> = Vec::new();
    for a in addable_screens(screens, off_modes) {
        match out.last_mut() {
            Some(g) if g.kind == a.kind => g.screens.push(a),
            _ => {
                let d = cat.iter().find(|d| d.id == a.kind);
                out.push(UiAddableGroup {
                    kind: a.kind.clone(),
                    label: d.map_or_else(|| a.kind.clone(), |d| d.label.clone()),
                    description: d.map_or_else(|| a.description.clone(), |d| d.description.clone()),
                    screens: vec![a],
                });
            }
        }
    }
    out
}

/// Which rail tile wears the ring, the one to scroll to, and when to ask
/// again.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct UiRing {
    /// The ringed screen id — at most one: exactly the screen on the glass.
    /// EMPTY when the board idles through two or more screens and the
    /// server has not said when that rotation began (an older server, or the
    /// moment before its push arrives): ring nothing rather than guess.
    pub ids: Vec<String>,
    /// The single tile to scroll into view: `ids[0]`, else the first
    /// checked screen.
    pub scroll_target: String,
    /// Unix ms at which the board moves to its next idle screen: call `ring`
    /// again then. None when the ring cannot change on its own.
    pub next_change_ms: Option<u64>,
}

/// The ring follows the GLASS: exactly the screen the panel is showing.
/// `panel_shows_nothing` is [`panel_shows_nothing`], not the list's length;
/// `screens` is [`resolved_screen_configs`]; `idle_cycle_secs` and
/// `idle_since_ms` are the board's (`UiBoardConfig`); `now_ms` is unix ms.
///
/// - Quiet hours ring Off (the window darkens the panel whatever the mode
///   says); Off rings Off; a held screen rings itself.
/// - Sports with something on rings Sports.
/// - Sports with nothing on rings Off when no screen is checked, the one
///   screen when one is, and with two or more the one the board's own clock
///   has up: member `floor((now - since) / dwell) % n` of the checked
///   screens in list order — the board restarts its rotation at the first
///   screen at `idle_since_ms` and holds each for the snapped idle timer.
///   `next_change_ms` is the next boundary; re-evaluate then. Without
///   `idle_since_ms` it rings nothing (see [`UiRing::ids`]).
///
/// An empty `mode` (no config yet) is Sports, the resting state — so a
/// platform's default record can hold "" rather than spelling an id.
#[uniffi::export]
pub fn ring(
    mode: String,
    quiet_active: bool,
    panel_shows_nothing: bool,
    screens: Vec<UiScreenConfig>,
    idle_cycle_secs: u32,
    idle_since_ms: Option<u64>,
    now_ms: u64,
) -> UiRing {
    let one = |id: String| UiRing { ids: vec![id.clone()], scroll_target: id, next_change_ms: None };
    if quiet_active || mode == MODE_OFF || mode == OFF_MODE_BLANK {
        return one(MODE_OFF.into());
    }
    if !mode.is_empty() && mode != MODE_SCORES {
        return one(mode);
    }
    if !panel_shows_nothing {
        return one(MODE_SCORES.into());
    }
    let idle: Vec<String> = screens.into_iter().filter(|s| s.idle && s.kind != MODE_SCORES && s.kind != MODE_OFF).map(|s| s.id).collect();
    match (idle.as_slice(), idle_since_ms) {
        ([], _) => one(MODE_OFF.into()),
        ([only], _) => one(only.clone()),
        (_, None) => UiRing { ids: vec![], scroll_target: idle[0].clone(), next_change_ms: None },
        (_, Some(since)) => {
            let dwell = u64::from(scoreboard_protocol::snap_idle_cycle_secs(idle_cycle_secs)) * 1000;
            let steps = now_ms.saturating_sub(since) / dwell;
            let id = idle[(steps % idle.len() as u64) as usize].clone();
            UiRing { ids: vec![id.clone()], scroll_target: id, next_change_ms: Some(since + (steps + 1) * dwell) }
        }
    }
}

/// The `power` twin of a mode: lit unless it is Off — the protocol's
/// `set_mode` rule, for an optimistic mode change.
#[uniffi::export]
pub fn mode_power(mode: String) -> bool {
    let mut cfg = BoardConfig::default();
    cfg.set_mode(&mode);
    cfg.power
}

/// Sports' id — the resting mode — for a platform record's default and for
/// `set_mode` when a control returns the board to Sports. Never compare a
/// mode against it on the platform; ask [`mode_footer_shown`] or [`ring`].
#[uniffi::export]
pub fn sports_id() -> String {
    MODE_SCORES.into()
}

/// Does the held-screen footer show for `mode`? Whenever something other
/// than Sports holds the panel. Not for Sports, and not for "" (the board's
/// config has not arrived). Its sentence is `mode_until_changed` or
/// `mode_back_prefix` + duration + `mode_back_suffix`, labelled by
/// [`screen_label_for`].
#[uniffi::export]
pub fn mode_footer_shown(mode: String) -> bool {
    !mode.is_empty() && mode != MODE_SCORES
}

/// The label for a screen or held-mode id (footers, the held tile): its
/// row's label in `screens` ([`resolved_screen_configs`]), a legacy kind id
/// resolving to its first instance; else Sports/Off/the descriptor label.
#[uniffi::export]
pub fn screen_label_for(id: String, screens: Vec<UiScreenConfig>, off_modes: Vec<UiOffMode>) -> String {
    let cat = catalog(off_modes.clone());
    let list = to_configs(screens);
    let index = list.iter().position(|s| s.id == id).or_else(|| {
        let r = scoreboard_protocol::resolve_screen(&list, &id)?;
        list.iter().position(|s| s.id == r.id)
    });
    match index {
        Some(i) => scoreboard_protocol::screen_label(&list, i, &cat),
        None => mode_label(id, off_modes_or_fallback(off_modes)),
    }
}

// ---- the idle timer ----
//
// How long each checked screen holds the panel while the board idles through
// two or more. Same shape as screen speed: seconds are stored, presets are
// the protocol's, and a stored value that is not a preset reads as the one
// it snaps to — the server times the board by the same snap.

/// One choice of the idle timer.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct UiIdleCycleOption {
    pub secs: u32,
    pub label: String,
}

fn idle_cycle_name(secs: u32) -> String {
    match secs {
        3600 => "1 hour".into(),
        s if s % 3600 == 0 => format!("{} hours", s / 3600),
        s => format!("{} minutes", s / 60),
    }
}

/// The idle timer's choices, shortest first: "2 minutes" ... "1 hour".
#[uniffi::export]
pub fn idle_cycle_options() -> Vec<UiIdleCycleOption> {
    IDLE_CYCLE_SECS.iter().map(|&secs| UiIdleCycleOption { secs, label: idle_cycle_name(secs) }).collect()
}

/// The option a stored value selects: its snapped preset. Compare
/// `idle_cycle_selected(secs) == option.secs` to place the checkmark.
#[uniffi::export]
pub fn idle_cycle_selected(secs: u32) -> u32 {
    scoreboard_protocol::snap_idle_cycle_secs(secs)
}

/// A stored idle timer's name: "5 minutes".
#[uniffi::export]
pub fn idle_cycle_label(secs: u32) -> String {
    idle_cycle_name(idle_cycle_selected(secs))
}

/// The idle timer's title.
#[uniffi::export]
pub fn idle_cycle_title() -> String {
    "Switch screens every".into()
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    per_screen: f.per_screen,
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

    /// The optimistic hold mirror: the server's one 24-hour hold for every
    /// pick but Sports, Off and instances included.
    #[test]
    fn wake_at_mirrors_the_server_rule() {
        assert_eq!(compute_wake_at("scores".into(), 1000), None);
        assert_eq!(compute_wake_at(String::new(), 1000), None, "no mode yet is Sports");
        for id in ["clock", "clock.2", "weather.3", "off", "blank"] {
            assert_eq!(compute_wake_at(id.into(), 1000), Some(1000 + 24 * 3600), "{id}");
        }
        assert_eq!(scoreboard_protocol::HOLD_HOURS, 24);
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
        // Initialisms that have a vowel, from the 2026 PGA and LPGA calendars.
        for (panel, card) in [
            ("PGA CHAMP", "PGA Champ"),
            ("LPGA SHANGHAI", "LPGA Shanghai"),
            ("AIG WOMEN'S OPEN", "AIG Women's Open"),
            ("CME GROUP TOUR", "CME Group Tour"),
            ("ISCO CHAMP", "ISCO Champ"),
            ("FEDEX ST. JUDE", "FedEx St. Jude"),
            ("BANK OF UTAH", "Bank of Utah"),
            ("TOURN OF CHAMPS", "Tourn of Champs"),
            ("TOUR Q-SCHOOL", "Tour Q-School"),
            ("US GP", "US GP"),
            ("SAO PAULO GP", "Sao Paulo GP"),
        ] {
            assert_eq!(game_title(panel.into(), String::new()), card, "{panel}");
        }
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

    /// The ring follows the glass: quiet rings Off; a held screen rings
    /// itself; Sports with nothing on rings 0 → Off, 1 → that screen, 2+ →
    /// the one the board's clock has up (nothing, scrolling to the first,
    /// until the server says when the rotation began); Sports otherwise
    /// rings Sports. Never more than one tile.
    #[test]
    fn the_ring_follows_the_glass() {
        let sc = |id: &str, kind: &str, idle: bool| UiScreenConfig { id: id.into(), kind: kind.into(), params: HashMap::new(), idle };
        let none = vec![sc("scores", "scores", false), sc("off", "off", false), sc("clock", "clock", false), sc("weather", "weather", false)];
        let one = vec![sc("scores", "scores", false), sc("off", "off", false), sc("clock", "clock", false), sc("weather", "weather", true)];
        let two = vec![sc("scores", "scores", false), sc("off", "off", false), sc("clock", "clock", true), sc("weather.2", "weather", true)];
        let r = |mode: &str, quiet: bool, nothing: bool, l: &Vec<UiScreenConfig>| {
            let r = ring(mode.into(), quiet, nothing, l.clone(), 300, Some(1_000), 1_000);
            assert_eq!(r.scroll_target, r.ids[0], "the scroll target is the ringed tile");
            assert!(r.ids.len() == 1, "one tile, never several");
            if l.iter().filter(|s| s.idle).count() < 2 || !nothing || quiet || mode != "scores" {
                assert_eq!(r.next_change_ms, None, "nothing changes on its own");
            }
            r.ids
        };
        assert_eq!(r("scores", true, true, &two), ["off"], "quiet wins");
        assert_eq!(r("weather.2", true, false, &two), ["off"], "quiet wins over a held screen too");
        assert_eq!(r("scores", false, true, &none), ["off"], "nothing checked: the panel is dark");
        assert_eq!(r("scores", false, true, &one), ["weather"]);
        assert_eq!(r("scores", false, true, &two), ["clock"], "the rotation's first screen at its start");
        assert_eq!(r("scores", false, false, &two), ["scores"]);
        assert_eq!(r("weather.2", false, true, &two), ["weather.2"], "a held screen rings itself");
        assert_eq!(r("off", false, false, &two), ["off"]);
        assert_eq!(r("blank", false, false, &two), ["off"], "a stored blank is dark like Off");

        // Two or more checked and no anchor (an older server, or before its
        // push): ring nothing rather than guess, but still scroll somewhere.
        let unknown = ring("scores".into(), false, true, two.clone(), 300, None, 5_000);
        assert_eq!(unknown, UiRing { ids: vec![], scroll_target: "clock".into(), next_change_ms: None });
        // One checked needs no anchor.
        assert_eq!(ring("scores".into(), false, true, one, 300, None, 5_000).ids, ["weather"]);
    }

    /// The idle ring's arithmetic is the board's: the rotation restarts at
    /// its first checked screen at `idle_since_ms`, each screen holds for the
    /// SNAPPED idle timer, and member floor((now - since) / dwell) % n is up —
    /// the same formula the server's test proves a real board follows. Each
    /// answer says when to ask again: the next boundary.
    #[test]
    fn the_idle_ring_counts_slots_from_the_anchor() {
        let sc = |id: &str, kind: &str, idle: bool| UiScreenConfig { id: id.into(), kind: kind.into(), params: HashMap::new(), idle };
        let list = vec![
            sc("scores", "scores", false),
            sc("off", "off", false),
            sc("clock", "clock", true),
            sc("weather", "weather", false),
            sc("weather.2", "weather", true),
            sc("clock.2", "clock", true),
        ];
        let checked = ["clock", "weather.2", "clock.2"];
        let since = 1_790_000_000_000u64;
        // 400 s is stored; the board times by the 300 s preset it snaps to.
        let dwell = 300_000u64;
        let at = |now: u64| ring("scores".into(), false, true, list.clone(), 400, Some(since), now);
        for now in (since..since + 2 * 3 * dwell + dwell).step_by(17_000) {
            let r = at(now);
            let k = ((now - since) / dwell % 3) as usize;
            assert_eq!(r.ids, [checked[k]], "t+{}ms", now - since);
            assert_eq!(r.scroll_target, checked[k]);
            assert_eq!(r.next_change_ms, Some(since + ((now - since) / dwell + 1) * dwell));
        }
        // Exactly at a boundary the next screen is up; one ms before, not.
        assert_eq!(at(since + dwell - 1).ids, ["clock"]);
        assert_eq!(at(since + dwell).ids, ["weather.2"]);
        assert_eq!(at(since + 3 * dwell).ids, ["clock"], "and round again");
        // A phone clock behind the server's: the first screen, until the anchor.
        let early = at(since - 5_000);
        assert_eq!((early.ids, early.next_change_ms), (vec!["clock".to_string()], Some(since + dwell)));
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

    /// The offline fallback descriptors track the stock server's ids, order
    /// and labels — "Off", one darkness one name — and apply only while the
    /// server has declared nothing.
    #[test]
    fn fallback_descriptors_match_the_server_labels() {
        let f = fallback_off_modes();
        let ids: Vec<&str> = f.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["blank", "clock", "weather"], "the stock server's catalog order");
        assert_eq!(f[0].label, "Off");
        assert!(f[0].preview.is_none());
        assert!(f[1].preview.is_some() && f[2].preview.is_some(), "the lit ones preview");
        assert_eq!(off_modes_or_fallback(vec![]).len(), 3);
        let declared = vec![f[1].clone()];
        assert_eq!(off_modes_or_fallback(declared).len(), 1, "a declared catalog is used as is");
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

#[cfg(test)]
mod screens_tests {
    use super::*;
    use scoreboard_protocol::normalize_screens;

    fn choice(value: &str, label: &str, preview: &str, screen_label: Option<&str>) -> OffModeChoice {
        OffModeChoice {
            value: value.into(),
            label: label.into(),
            preview: Some(preview.into()),
            screen_label: screen_label.map(Into::into),
        }
    }

    fn field(key: &str, kind: &str, default: &str, per_screen: bool, options: Vec<OffModeChoice>) -> OffModeField {
        OffModeField {
            key: key.into(),
            label: key.into(),
            kind: kind.into(),
            placeholder: String::new(),
            max_len: if kind == "text" { 32 } else { 0 },
            default: default.into(),
            options,
            visible_when: None,
            per_screen,
        }
    }

    fn mode(id: &str, label: &str, preview: Option<&str>, fields: Vec<OffModeField>) -> OffModeDescriptor {
        OffModeDescriptor {
            id: id.into(),
            label: label.into(),
            description: format!("{label} description"),
            preview: preview.map(Into::into),
            input: None,
            fields,
        }
    }

    /// The stock catalog as the server will declare it once layouts and
    /// styles are per-screen, plus a fieldless-preset kind (stocks).
    fn proto_catalog() -> Vec<OffModeDescriptor> {
        let mut hour24 = field("hour24", "toggle", "0", false, vec![]);
        hour24.visible_when = Some(FieldCondition { key: "style".into(), equals: "classic".into() });
        let mut weather = mode(
            "weather",
            "Weather",
            Some("/offmode/weather.png"),
            vec![
                field("location", "text", "", false, vec![]),
                field(
                    "variant",
                    "choice",
                    "detailed",
                    true,
                    vec![
                        choice("simple", "Simple", "/offmode/weather.png?variant=simple", Some("Big Weather")),
                        choice("detailed", "Detailed", "/offmode/weather.png?variant=detailed", None),
                        choice("forecast", "Forecast", "/offmode/weather.png?variant=forecast", Some("Forecast")),
                    ],
                ),
            ],
        );
        weather.input = Some(OffModeInput {
            key: "location".into(),
            label: "Location".into(),
            placeholder: "ZIP or lat,lon".into(),
            max_len: 32,
        });
        vec![
            mode("blank", "Off", None, vec![]),
            mode(
                "clock",
                "Clock",
                Some("/offmode/clock.png"),
                vec![
                    hour24,
                    field(
                        "style",
                        "choice",
                        "classic",
                        true,
                        vec![
                            choice("classic", "Classic", "/offmode/clock.png?style=classic", None),
                            choice("word", "Word", "/offmode/clock.png?style=word", Some("Word Clock")),
                        ],
                    ),
                ],
            ),
            weather,
            mode("stocks", "Stocks", Some("/offmode/stocks.png"), vec![field("tickers", "text", "SPY", false, vec![])]),
        ]
    }

    fn ui_catalog() -> Vec<UiOffMode> {
        crate::to_ui_off_modes(&proto_catalog())
    }

    fn sc(id: &str, kind: &str, params: &[(&str, &str)], idle: bool) -> UiScreenConfig {
        UiScreenConfig {
            id: id.into(),
            kind: kind.into(),
            params: params.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            idle,
        }
    }

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// Sports, Off, Clock, Weather (idle), Weather · Forecast, Stocks.
    fn base() -> Vec<UiScreenConfig> {
        vec![
            sc("scores", "scores", &[], false),
            sc("off", "off", &[], false),
            sc("clock", "clock", &[], false),
            sc("weather", "weather", &[], true),
            sc("weather.2", "weather", &[("variant", "forecast")], false),
            sc("stocks", "stocks", &[], false),
        ]
    }

    fn ids(list: &[UiScreenConfig]) -> Vec<&str> {
        list.iter().map(|s| s.id.as_str()).collect()
    }

    /// What the server stores for the same edit: THE protocol normalize.
    fn server(list: Vec<UiScreenConfig>) -> Vec<UiScreenConfig> {
        to_ui_configs(normalize_screens(to_configs(list), &proto_catalog()))
    }

    /// The converters lose nothing the protocol rules read: a descriptor
    /// survives protocol → app → protocol, and a screen survives both ways.
    #[test]
    fn descriptors_and_screens_round_trip_through_the_app_types() {
        for d in proto_catalog() {
            let back = to_descriptor(&crate::to_ui_off_modes(std::slice::from_ref(&d))[0]);
            assert_eq!(serde_json::to_value(&back).unwrap(), serde_json::to_value(&d).unwrap(), "{} diverged", d.id);
        }
        let s = sc("weather.2", "weather", &[("variant", "forecast")], true);
        assert_eq!(to_ui_screen_config(to_screen_config(s.clone())), s);
    }

    /// Every optimistic transition is exactly what the server stores: the
    /// same edit run through the protocol's normalize, with the catalog the
    /// server declared.
    #[test]
    fn every_mirror_equals_the_server_normalize() {
        let cat = ui_catalog();

        // Add: minted after the existing instances, unchecked, at the end.
        let added = screens_added(base(), "clock".into(), map(&[("style", "word")]), cat.clone());
        assert_eq!(added.id, "clock.2");
        let mut want = base();
        want.push(sc("clock.2", "clock", &[("style", "word")], false));
        assert_eq!(added.screens, server(want));
        assert!(!added.screens.last().unwrap().idle);

        // Remove: gone; Sports and Off are not removable.
        let rm = |list: Vec<UiScreenConfig>, id: &str| {
            screens_removed(list, id.into(), "scores".into(), None, cat.clone()).screens
        };
        let removed = rm(base(), "weather");
        let want: Vec<UiScreenConfig> = base().into_iter().filter(|s| s.id != "weather").collect();
        assert_eq!(removed, server(want));
        assert_eq!(rm(base(), "off"), server(base()), "Off stays");

        // Reorder by id: unknown ids ignored, omitted ones appended in their
        // order, Sports/Off pinned however they were named.
        let ordered = screens_with_order(
            base(),
            vec!["stocks".into(), "nope".into(), "off".into(), "weather.2".into()],
            cat.clone(),
        );
        assert_eq!(ids(&ordered), ["scores", "off", "stocks", "weather.2", "clock", "weather"]);
        let b = base();
        let manual = vec![b[5].clone(), b[1].clone(), b[4].clone(), b[0].clone(), b[2].clone(), b[3].clone()];
        assert_eq!(ordered, server(manual));

        // Idle: multi-select keeps the others; fixed rows never check.
        let idled = screens_with_idle(base(), "clock".into(), true, cat.clone());
        let mut want = base();
        want[2].idle = true;
        assert_eq!(idled, server(want));
        assert_eq!(idled.iter().filter(|s| s.idle).count(), 2);
        assert_eq!(screens_with_idle(base(), "off".into(), true, cat.clone()), server(base()), "Off never checks");
        let unchecked = screens_with_idle(base(), "weather".into(), false, cat.clone());
        assert!(unchecked.iter().all(|s| !s.idle), "unchecking the last leaves none — the board goes dark when idle");

        // Param: set, empty removes (the default), a refused value drops.
        let set = screens_with_param(base(), "weather".into(), "variant".into(), "simple".into(), cat.clone());
        let mut want = base();
        want[3].params = map(&[("variant", "simple")]);
        assert_eq!(set, server(want));
        let cleared = screens_with_param(base(), "weather.2".into(), "variant".into(), String::new(), cat.clone());
        assert!(cleared[4].params.is_empty(), "an empty value removes the key, as the server does");
        let bad = screens_with_param(base(), "weather".into(), "variant".into(), "radar".into(), cat.clone());
        assert!(bad[3].params.is_empty(), "a value the descriptor refuses is not stored");
        let wide = screens_with_param(base(), "weather".into(), "location".into(), "10001".into(), cat.clone());
        assert!(wide[3].params.is_empty(), "a board-wide key is never a screen's");
    }

    /// The add mirror refuses exactly what the server does: past the cap the
    /// screen does not survive and no id comes back.
    #[test]
    fn adding_past_the_cap_returns_no_id() {
        let cat = ui_catalog();
        let mut list = base();
        while can_add_screen(list.clone()) {
            list = screens_added(list, "clock".into(), HashMap::new(), cat.clone()).screens;
        }
        assert_eq!(list.len(), MAX_SCREENS);
        let past = screens_added(list.clone(), "clock".into(), HashMap::new(), cat);
        assert_eq!(past.id, "");
        assert_eq!(past.screens, list);
    }

    /// Single-select for a server without lists: checking one unchecks the
    /// rest; unchecking it leaves none.
    #[test]
    fn sole_idle_is_single_select() {
        let cat = ui_catalog();
        let one = screens_with_sole_idle(base(), "clock".into(), true, cat.clone());
        assert_eq!(one.iter().filter(|s| s.idle).map(|s| s.id.as_str()).collect::<Vec<_>>(), ["clock"]);
        let none = screens_with_sole_idle(one, "clock".into(), false, cat);
        assert!(none.iter().all(|s| !s.idle));
    }

    /// Before the catalog arrives a mirror must not delete what the fallback
    /// cannot describe — a fork's screen, a layout — because the result is
    /// sent to the server.
    #[test]
    fn a_mirror_before_the_catalog_keeps_what_it_cannot_describe() {
        let mut list = base();
        list.push(sc("radar", "radar", &[("zoom", "3")], false));
        let out = screens_with_idle(list, "radar".into(), true, vec![]);
        let radar = out.iter().find(|s| s.id == "radar").expect("the fork's screen survives");
        assert!(radar.idle);
        assert_eq!(radar.params, map(&[("zoom", "3")]));
        assert_eq!(out[4].params, map(&[("variant", "forecast")]), "the layout survives");
    }

    /// A platform that reads every catalog through `off_modes_or_fallback`
    /// hands the mirrors the FALLBACK before the catalog lands; that must
    /// mean exactly what handing them nothing means — no layout, no fork's
    /// screen lost. (iOS did this; Android passes the raw, empty list.)
    #[test]
    fn the_fallback_catalog_reads_as_no_catalog() {
        let mut list = base();
        list.push(sc("radar", "radar", &[("zoom", "3")], false));
        list[2].params = map(&[("style", "word")]);
        let fb = off_modes_or_fallback(vec![]);
        let pairs = |m: Vec<UiOffMode>| {
            vec![
                screens_with_idle(list.clone(), "radar".into(), true, m.clone()),
                screens_with_sole_idle(list.clone(), "clock".into(), true, m.clone()),
                screens_with_order(list.clone(), vec!["weather.2".into(), "clock".into()], m.clone()),
                screens_with_param(list.clone(), "weather".into(), "variant".into(), "simple".into(), m.clone()),
                screens_added(list.clone(), "clock".into(), HashMap::new(), m.clone()).screens,
                screens_removed(list.clone(), "stocks".into(), "scores".into(), None, m).screens,
            ]
        };
        let with_fallback = pairs(fb);
        assert_eq!(with_fallback, pairs(vec![]));
        let order = &with_fallback[2];
        assert_eq!(ids(order)[..4], ["scores", "off", "weather.2", "clock"]);
        assert_eq!(order[2].params, map(&[("variant", "forecast")]), "the layout survives a reorder");
        assert_eq!(order[3].params, map(&[("style", "word")]), "the style survives a reorder");
        assert!(order.iter().any(|s| s.id == "radar" && s.params == map(&[("zoom", "3")])), "the fork's screen survives");
        // The server's own catalog is still read strictly.
        let strict = screens_with_order(list.clone(), vec![], ui_catalog());
        assert!(!strict.iter().any(|s| s.id == "radar"));
    }

    /// Add is offered exactly when it can land, and says why when not —
    /// full, or nothing the server offers is missing from the list.
    #[test]
    fn add_screen_says_why_it_is_unavailable() {
        assert_eq!(add_screen_note(base(), ui_catalog()), None);
        let full: Vec<UiScreenConfig> = base()
            .into_iter()
            .chain((2..=5).map(|n| sc(&format!("stocks.{n}"), "stocks", &[], false)))
            .collect();
        assert_eq!(full.len(), MAX_SCREENS);
        assert_eq!(add_screen_note(full, ui_catalog()), Some(crate::strings::screen_cap_note()));
        let every = vec![
            sc("scores", "scores", &[], false),
            sc("off", "off", &[], false),
            sc("clock", "clock", &[], false),
            sc("clock.2", "clock", &[("style", "word")], false),
            sc("weather", "weather", &[("variant", "simple")], false),
            sc("weather.2", "weather", &[], false),
            sc("weather.3", "weather", &[("variant", "forecast")], false),
            sc("stocks", "stocks", &[], false),
        ];
        assert!(addable_screens(every.clone(), ui_catalog()).is_empty());
        assert_eq!(add_screen_note(every, ui_catalog()), Some(crate::strings::nothing_to_add_note()));
    }

    /// The chooser's sections come from the core: the descriptor's label as
    /// the heading, its description once, the presets in catalog order.
    #[test]
    fn the_add_chooser_is_grouped_by_the_core() {
        let groups = addable_screen_groups(base(), ui_catalog());
        let heads: Vec<(&str, &str, usize)> =
            groups.iter().map(|g| (g.label.as_str(), g.description.as_str(), g.screens.len())).collect();
        assert_eq!(heads, [("Clock", "Clock description", 1), ("Weather", "Weather description", 1)]);
        let flat: Vec<UiAddableScreen> = groups.into_iter().flat_map(|g| g.screens).collect();
        assert_eq!(flat, addable_screens(base(), ui_catalog()));
    }

    /// Remove is offered for a user's screen on a server that stores lists —
    /// never for Sports or Off, nor for a list synthesized for an old server
    /// (or for a board whose config has not arrived).
    #[test]
    fn only_a_stored_users_screen_is_removable() {
        let rows = screen_list(base(), String::new(), HashMap::new(), ui_catalog());
        let got: Vec<(&str, bool)> = rows.iter().map(|s| (s.id.as_str(), s.removable)).collect();
        assert_eq!(
            got,
            [("scores", false), ("off", false), ("clock", true), ("weather", true), ("weather.2", true), ("stocks", true)]
        );
        let old = screen_list(vec![], "weather".into(), HashMap::new(), ui_catalog());
        assert!(old.iter().all(|s| !s.removable));
        let unknown = screen_list(vec![], String::new(), HashMap::new(), vec![]);
        assert!(unknown.iter().all(|s| !s.removable));
    }

    /// The held screen after an edit — the server's release, mirrored: the
    /// same answer as the protocol's `set_screens`.
    #[test]
    fn held_after_mirrors_the_servers_release() {
        let cat = ui_catalog();
        let removed = screens_removed(base(), "weather".into(), "scores".into(), None, cat).screens;
        for (held, kind) in [("weather", "weather"), ("weather.2", "weather"), ("clock", "clock"), ("scores", "scores"), ("off", "off")] {
            let mut cfg = BoardConfig::default();
            cfg.set_held_screen(held, kind);
            let listed = cfg.set_screens(to_configs(removed.clone()), &proto_catalog());
            let want = if listed { held.to_string() } else { "scores".to_string() };
            assert_eq!(held_after(held.into(), removed.clone()), want, "held {held}");
        }
        assert_eq!(
            held_after("weather".into(), removed),
            "scores",
            "a removed first instance releases even while its sibling stays"
        );
        assert_eq!(held_after("blank".into(), base()), "blank", "a stored blank is dark, not released");
    }

    /// A list synthesized from the legacy fields exactly as the protocol
    /// does, the layout kept, blank = none checked. NOTE: this runs a NEW
    /// catalog (`per_screen` declared) over legacy fields — a board that
    /// never saved a list on a current server. A real server without lists
    /// declares no `per_screen`; `an_old_server_keeps_its_layout_board_wide`
    /// covers that.
    #[test]
    fn an_old_server_gets_a_synthesized_list() {
        let flat = map(&[("variant", "forecast"), ("location", "10001")]);
        let list = screen_list(vec![], "weather".into(), flat.clone(), ui_catalog());
        let got: Vec<(&str, bool)> = list.iter().map(|s| (s.id.as_str(), s.idle)).collect();
        assert_eq!(got, [("scores", false), ("off", false), ("clock", false), ("weather", true), ("stocks", false)]);
        let w = &list[3];
        assert_eq!(w.label, "Forecast", "named by its layout, even alone (MAR-119)");
        assert_eq!(w.preview.as_deref(), Some("/offmode/weather.png?variant=forecast"), "the layout is kept, the ZIP is not");
        let raw = resolved_screen_configs(vec![], "weather".into(), flat.clone(), ui_catalog());
        assert_eq!(raw[3].params, map(&[("variant", "forecast")]));

        let dark = screen_list(vec![], "blank".into(), flat.clone(), ui_catalog());
        assert!(dark.iter().all(|s| !s.idle), "blank: nothing checked");

        // Before the catalog arrives the fallback already reads right.
        let early = screen_list(vec![], "weather".into(), flat, vec![]);
        let got: Vec<(&str, bool)> = early.iter().map(|s| (s.id.as_str(), s.idle)).collect();
        assert_eq!(got, [("scores", false), ("off", false), ("clock", false), ("weather", true)]);
    }

    /// Rows: Sports and Off fixed and pinned, labels distinct, a server's
    /// list taken verbatim, Sports/Off previews fixed, every row but Sports
    /// has a page.
    #[test]
    fn screen_rows_carry_labels_previews_and_flags() {
        let flat = map(&[("variant", "forecast"), ("location", "10001"), ("hour24", "1")]);
        let list = screen_list(base(), "weather".into(), flat, ui_catalog());
        let labels: Vec<&str> = list.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["Sports", "Off", "Clock", "Weather · Detailed", "Forecast", "Stocks"]);
        assert_eq!(list[0].preview.as_deref(), Some("/sample/plain.png?live=1"));
        assert_eq!(list[1].preview, None, "Off draws black on the platform");
        assert!(list[0].fixed && list[1].fixed && !list[2].fixed);
        assert!(!list[0].has_settings && !list[1].has_settings, "Sports and Off have no page");
        assert!(list[2..].iter().all(|s| s.has_settings), "every user screen has one");
        assert_eq!(list[2].preview.as_deref(), Some("/offmode/clock.png?hour24=1"), "board-wide 24-hour rides along");
        assert_eq!(list[5].preview.as_deref(), Some("/offmode/stocks.png"), "a text field never rides");
    }

    /// Two weather screens: two distinct canonical URLs, no duplicate keys,
    /// no location — and a defaulted instance does NOT pick up its sibling's
    /// layout from the flat mirror.
    #[test]
    fn two_weather_screens_preview_distinctly() {
        let screens = vec![
            sc("scores", "scores", &[], false),
            sc("off", "off", &[], false),
            sc("weather", "weather", &[], false),
            sc("weather.2", "weather", &[("variant", "forecast")], true),
        ];
        // The server mirrors the idle member's layout into the flat map.
        let flat = map(&[("variant", "forecast"), ("location", "Springfield, IL")]);
        let list = screen_list(screens, "weather".into(), flat, ui_catalog());
        let a = list[2].preview.clone().unwrap();
        let b = list[3].preview.clone().unwrap();
        assert_eq!(a, "/offmode/weather.png");
        assert_eq!(b, "/offmode/weather.png?variant=forecast");
        for url in [&a, &b] {
            assert!(!url.contains("location"), "{url}");
            assert!(url.matches("variant=").count() <= 1, "{url}");
        }
    }

    /// Presets: one per layout or style, server labels where declared,
    /// "{Mode} · {Option}" otherwise, previews verbatim, catalog order — and
    /// what the board already has is left out, defaults resolved.
    #[test]
    fn addable_presets_hide_what_is_already_there() {
        let list = vec![
            sc("scores", "scores", &[], false),
            sc("off", "off", &[], false),
            sc("clock", "clock", &[], false),
            sc("weather", "weather", &[], true),
        ];
        let add = addable_screens(list, ui_catalog());
        let got: Vec<(&str, &str)> = add.iter().map(|a| (a.kind.as_str(), a.label.as_str())).collect();
        assert_eq!(
            got,
            [("clock", "Word Clock"), ("weather", "Big Weather"), ("weather", "Forecast"), ("stocks", "Stocks")],
            "Classic and Detailed are the defaults the board already shows"
        );
        assert_eq!(add[1].params, map(&[("variant", "simple")]));
        assert_eq!(add[1].preview.as_deref(), Some("/offmode/weather.png?variant=simple"));
        assert_eq!(add[1].description, "Weather description");
        assert!(add[3].params.is_empty());
        assert_eq!(add[3].preview.as_deref(), Some("/offmode/stocks.png"));

        let full = addable_screens(base(), ui_catalog());
        let labels: Vec<&str> = full.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(labels, ["Word Clock", "Big Weather"], "Forecast and Stocks are on the board");
        // An unlabelled option composes its name.
        let mut cat = proto_catalog();
        cat[1].fields[1].options[1].screen_label = None;
        let add = addable_screens(base(), crate::to_ui_off_modes(&cat));
        assert_eq!(add[0].label, "Clock · Word");
    }

    #[test]
    fn predicates_gate_what_the_server_would_refuse() {
        assert!(can_add_screen(base()));
        assert!(!can_set_idle(base(), "scores".into()));
        assert!(!can_set_idle(base(), "off".into()));
        assert!(!can_set_idle(base(), "nope".into()));
        assert!(can_set_idle(base(), "weather.2".into()));
        assert!(screens_editable(true) && !screens_editable(false));
        assert!(idle_single_select(false) && !idle_single_select(true));
        assert!(mode_power("weather.2".into()) && mode_power("scores".into()) && !mode_power("off".into()));
    }

    /// Footer labels: an instance's row label, a legacy kind id resolving to
    /// its first instance, Sports/Off fixed, and a sane last resort.
    #[test]
    fn labels_for_held_ids() {
        let l = |id: &str| screen_label_for(id.into(), base(), ui_catalog());
        assert_eq!(l("weather.2"), "Forecast");
        assert_eq!(l("weather"), "Weather · Detailed");
        assert_eq!(l("scores"), "Sports");
        assert_eq!(l("off"), "Off");
        assert_eq!(l("blank"), "Off");
        let legacy = vec![sc("scores", "scores", &[], false), sc("off", "off", &[], false), sc("weather.3", "weather", &[], false)];
        assert_eq!(screen_label_for("weather".into(), legacy, ui_catalog()), "Weather", "a kind id resolves to its first instance");
        assert_eq!(l("mystery"), "Mystery");
        assert_eq!(mode_label("scores".into(), vec![]), "Sports");
    }

    /// Six named timers, and every stored value reads as one of them.
    #[test]
    fn idle_timers_are_named_and_stored_values_snap() {
        let o: Vec<(u32, String)> = idle_cycle_options().into_iter().map(|o| (o.secs, o.label)).collect();
        assert_eq!(
            o,
            [
                (120, "2 minutes".to_string()),
                (300, "5 minutes".into()),
                (600, "10 minutes".into()),
                (900, "15 minutes".into()),
                (1800, "30 minutes".into()),
                (3600, "1 hour".into()),
            ]
        );
        assert_eq!(idle_cycle_selected(0), 120, "never zero");
        assert_eq!(idle_cycle_selected(400), 300);
        assert_eq!(idle_cycle_selected(99_999), 3600);
        assert_eq!(idle_cycle_label(450), "10 minutes", "a midpoint goes to the longer preset");
        assert_eq!(idle_cycle_title(), "Switch screens every");
    }

    /// A screen's page reads the params the server RENDERS it with
    /// (`layered_params`): the board map minus the descriptor's per-screen
    /// keys, then the screen's own. The board map mirrors the idle Word
    /// clock's style, so the naive overlay would show the defaulted Clock as
    /// Word and hide its 24-hour toggle.
    #[test]
    fn a_screens_page_reads_what_the_server_renders() {
        let cat = ui_catalog();
        let list = vec![
            sc("scores", "scores", &[], false),
            sc("off", "off", &[], false),
            sc("clock", "clock", &[], false),
            sc("clock.2", "clock", &[("style", "word")], true),
        ];
        // What the server stores: the flat map mirrors the idle member.
        let mut cfg = BoardConfig::default();
        cfg.off_mode_params = [("hour24".to_string(), "1".to_string())].into_iter().collect();
        let _ = cfg.set_screens(to_configs(list.clone()), &proto_catalog());
        let flat: HashMap<String, String> = cfg.off_mode_params.clone().into_iter().collect();
        assert_eq!(flat.get("style").map(String::as_str), Some("word"), "the precondition: a mirrored style");

        let clock = cat.iter().find(|m| m.id == "clock").unwrap();
        let d = to_descriptor(clock);
        let first = screen_params("clock".into(), list.clone(), flat.clone(), cat.clone());
        let second = screen_params("clock.2".into(), list.clone(), flat.clone(), cat.clone());
        for (params, style) in [(&first, "classic"), (&second, "word")] {
            let bt: BTreeMap<String, String> = params.clone().into_iter().collect();
            assert_eq!(off_mode_value(clock, "style".into(), params.clone()), style);
            assert_eq!(d.value("style", &bt), style, "the protocol reads the same");
            assert_eq!(params.get("hour24").map(String::as_str), Some("1"), "board-wide values ride along");
        }
        let keys = |p: &HashMap<String, String>| -> Vec<String> {
            visible_off_mode_fields(clock, p.clone()).into_iter().map(|f| f.key).collect()
        };
        assert_eq!(keys(&first), ["hour24", "style"], "the Classic clock offers 24-hour time");
        assert_eq!(keys(&second), ["style"]);
        // Sports, Off and an unknown id read the board map as it is.
        assert_eq!(screen_params("off".into(), list.clone(), flat.clone(), cat.clone()), flat);
        assert_eq!(screen_params("nope".into(), list, flat.clone(), cat), flat);
    }

    /// Removing a screen is the server's whole remove in one record: the
    /// list, and the release when it was held (mode, power, wake timer —
    /// never more).
    #[test]
    fn removing_a_screen_mirrors_the_servers_release() {
        let cat = ui_catalog();
        let r = screens_removed(base(), "weather.2".into(), "weather.2".into(), Some(1000), cat.clone());
        let mut cfg = BoardConfig::default();
        cfg.set_held_screen("weather.2", "weather");
        assert!(!cfg.set_screens(to_configs(r.screens.clone()), &proto_catalog()), "the protocol releases too");
        assert_eq!((r.mode.as_str(), r.power, r.wake_at), ("scores", true, None), "released to Sports");
        assert!(!r.screens.iter().any(|s| s.id == "weather.2"));

        let r = screens_removed(base(), "clock".into(), "weather.2".into(), Some(1000), cat.clone());
        assert_eq!((r.mode.as_str(), r.power, r.wake_at), ("weather.2", true, Some(1000)), "not held: untouched");

        let r = screens_removed(base(), "weather".into(), "off".into(), Some(1000), cat);
        assert_eq!((r.mode.as_str(), r.power, r.wake_at), ("off", false, Some(1000)), "Off stays dark");
    }

    /// The optimistic write is exactly what `set_screens` changes on the
    /// server. Against a server without lists the record's `screens` stays
    /// EMPTY (so no row turns removable and no mirror runs on a stand-in
    /// list) and `off_mode` carries the change, as `SetOffMode` will.
    #[test]
    fn an_optimistic_write_changes_what_the_server_will() {
        let configs = resolved_screen_configs(vec![], "clock".into(), HashMap::new(), ui_catalog());
        let checked = screens_with_sole_idle(configs.clone(), "weather".into(), true, ui_catalog());
        let w = screens_write(false, checked.clone());
        assert_eq!((w.screens, w.off_mode.as_deref()), (None, Some("weather")));
        let rows = screen_list(vec![], w.off_mode.unwrap(), HashMap::new(), ui_catalog());
        assert!(rows.iter().all(|r| !r.removable), "still a server without lists");
        assert!(rows.iter().any(|r| r.id == "weather" && r.idle), "and the tick shows");

        let none = screens_with_sole_idle(configs, "clock".into(), false, ui_catalog());
        assert_eq!(screens_write(false, none).off_mode.as_deref(), Some("blank"), "nothing checked: dark");

        let w = screens_write(true, checked.clone());
        assert_eq!((w.screens, w.off_mode), (Some(checked), None));
        assert_eq!(screens_write(true, vec![]), UiScreensWrite { screens: None, off_mode: None });
    }

    /// Before the catalog lands, Add says so instead of "nothing to add".
    #[test]
    fn add_waits_for_the_catalog() {
        let configs = resolved_screen_configs(vec![], "clock".into(), HashMap::new(), vec![]);
        assert_eq!(add_screen_note(configs.clone(), vec![]), Some(crate::strings::screens_loading_note()));
        assert_eq!(add_screen_note(configs.clone(), off_modes_or_fallback(vec![])), Some(crate::strings::screens_loading_note()));
        assert_eq!(add_screen_note(configs, ui_catalog()), None);
    }

    /// The raw `UiBoardConfig.screens` of a server without lists is EMPTY.
    /// Fed to a mirror by mistake it must change nothing and send nothing —
    /// never a [Sports, Off] list routed to `SetOffMode{"blank"}`, which
    /// would turn that board's idle screen dark.
    #[test]
    fn an_empty_list_is_never_a_dark_write() {
        let cat = ui_catalog();
        let json = |cmds: Vec<scoreboard_protocol::Command>| -> Vec<String> {
            cmds.iter().map(|c| serde_json::to_string(c).unwrap()).collect()
        };
        for out in [
            screens_with_idle(vec![], "clock".into(), true, cat.clone()),
            screens_with_sole_idle(vec![], "clock".into(), true, cat.clone()),
            screens_with_order(vec![], vec!["clock".into()], cat.clone()),
            screens_with_param(vec![], "clock".into(), "style".into(), "word".into(), cat.clone()),
            screens_added(vec![], "clock".into(), HashMap::new(), cat.clone()).screens,
            screens_removed(vec![], "clock".into(), "scores".into(), None, cat.clone()).screens,
        ] {
            assert!(out.is_empty(), "{out:?}");
            assert!(crate::screens_commands(false, out.clone()).is_empty());
            assert!(crate::screens_commands(true, out).is_empty(), "SetScreens{{[]}} would wipe the list");
        }
        let pinned = vec![sc("scores", "scores", &[], false), sc("off", "off", &[], false)];
        assert!(json(crate::screens_commands(false, pinned.clone())).is_empty(), "no lit screen: not an old server's list");
        assert_eq!(json(crate::screens_commands(true, pinned)).len(), 1, "a new server may hold only Sports and Off");
        assert_eq!(held_after("weather".into(), vec![]), "weather", "no list, no release");
        let r = screens_removed(vec![], "weather".into(), "weather".into(), Some(9), cat);
        assert_eq!((r.mode.as_str(), r.wake_at), ("weather", Some(9)));
    }

    /// A REAL server without lists: its descriptors declare no `per_screen`,
    /// so the synthesized screens carry no params, the layout lives in the
    /// board map (and the preview reads it there), and the single-select
    /// edit routes to exactly one `SetOffMode`.
    #[test]
    fn an_old_server_keeps_its_layout_board_wide() {
        let mut old = proto_catalog();
        for d in &mut old {
            for f in &mut d.fields {
                f.per_screen = false;
                for o in &mut f.options {
                    o.screen_label = None;
                }
            }
        }
        let cat = crate::to_ui_off_modes(&old);
        let flat = map(&[("variant", "forecast"), ("location", "10001"), ("hour24", "1")]);
        let list = resolved_screen_configs(vec![], "weather".into(), flat.clone(), cat.clone());
        assert_eq!(ids(&list), ["scores", "off", "clock", "weather", "stocks"]);
        assert!(list.iter().all(|s| s.params.is_empty()), "no per-screen keys to copy");
        assert_eq!(list.iter().filter(|s| s.idle).map(|s| s.id.as_str()).collect::<Vec<_>>(), ["weather"]);

        let rows = screen_list(vec![], "weather".into(), flat.clone(), cat.clone());
        assert_eq!(rows[3].preview.as_deref(), Some("/offmode/weather.png?variant=forecast"), "the flat layout, no ZIP");
        assert_eq!(screen_params("weather".into(), list.clone(), flat.clone(), cat.clone()), flat, "the board map IS the render");

        // A layout edit is not a per-screen value here; the page sends it with
        // set_off_mode_param, and the mirror stores nothing on the screen.
        let edited = screens_with_param(list.clone(), "weather".into(), "variant".into(), "simple".into(), cat.clone());
        assert!(edited.iter().all(|s| s.params.is_empty()));

        let json = |cmds: Vec<scoreboard_protocol::Command>| -> Vec<String> {
            cmds.iter().map(|c| serde_json::to_string(c).unwrap()).collect()
        };
        let clock = screens_with_sole_idle(list.clone(), "clock".into(), true, cat.clone());
        assert_eq!(json(crate::screens_commands(false, clock)), [r#"{"type":"SetOffMode","id":"clock"}"#]);
        let dark = screens_with_sole_idle(list, "weather".into(), false, cat);
        assert_eq!(json(crate::screens_commands(false, dark)), [r#"{"type":"SetOffMode","id":"blank"}"#], "unchecking is a real dark write");
    }

    /// Before the config lands the mode is "": Sports, no footer. The footer
    /// shows for anything else that holds the panel.
    #[test]
    fn the_footer_and_ring_know_sports_without_a_literal() {
        assert_eq!(sports_id(), "scores");
        assert!(!mode_footer_shown(String::new()));
        assert!(!mode_footer_shown(sports_id()));
        for m in ["off", "clock", "weather.2", "blank"] {
            assert!(mode_footer_shown(m.into()), "{m}");
        }
        assert_eq!(ring(String::new(), false, false, base(), 300, None, 0).ids, ["scores"], "unknown mode rings Sports");
        assert_eq!(ring(String::new(), false, true, base(), 300, None, 0).ids, ["weather"], "and idles like Sports");
    }
}
