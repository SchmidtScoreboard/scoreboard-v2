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
            if w.chars().count() <= 2 {
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
    for m in off_modes {
        if m.preview.is_some() {
            out.push(UiPickableMode { id: m.id, label: m.label, path: m.preview });
        }
    }
    out.push(UiPickableMode { id: "off".into(), label: "Off".into(), path: None });
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

/// The ring follows the GLASS: what the panel is actually showing, as a
/// picker id. Quiet hours force Off (the window darkens the panel whatever
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
            queued: false,
        }
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

    #[test]
    fn the_picker_is_scores_first_off_last_no_dark_double() {
        let modes = vec![
            UiOffMode { id: "blank".into(), label: "Off".into(), description: String::new(), preview: None, input: None, fields: vec![] },
            UiOffMode { id: "clock".into(), label: "Clock".into(), description: String::new(), preview: Some("/offmode/clock.png".into()), input: None, fields: vec![] },
        ];
        let p = pickable_modes(modes);
        let ids: Vec<&str> = p.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["scores", "clock", "off"], "blank never becomes a second dark tile");
    }

    #[test]
    fn the_ring_follows_the_glass() {
        assert_eq!(effective_mode("scores".into(), true, false, "weather".into()), "off", "quiet wins");
        assert_eq!(effective_mode("scores".into(), false, true, "weather".into()), "weather");
        assert_eq!(effective_mode("scores".into(), false, true, "blank".into()), "off");
        assert_eq!(effective_mode("scores".into(), false, false, "weather".into()), "scores");
        assert_eq!(effective_mode("clock".into(), false, true, "weather".into()), "clock");
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
