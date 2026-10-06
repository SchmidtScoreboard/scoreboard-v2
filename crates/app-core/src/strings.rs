//! User-facing copy and the derivation rules for user-facing text.
//!
//! # Why this module exists
//!
//! The Android app (MAR-104) must say the same things the iOS app says, and
//! two hand-maintained copies of the same sentence WILL drift. Copy that both
//! platforms need lives here, exposed through uniffi, so there is one place a
//! sentence can be wrong.
//!
//! The highest-churn text never lands here at all — league names, round
//! labels, off-mode descriptors and their fields all travel from the SERVER,
//! which is the same one-source rule applied a layer up.
//!
//! What deliberately stays on the platform: layout-bound labels (a section
//! header is part of a screen, not shared prose) and anything the platform
//! must FORMAT — times, dates, plurals via the platform's own locale
//! machinery. The core supplies strings and rules; the platform renders them.

/// One screen of the troubleshooting guide: the panel state it names, and
/// what to do about it. `slug` keys the server's `/guide/{slug}.png` preview.
#[derive(uniffi::Record, Clone)]
pub struct GuideCopy {
    pub slug: String,
    pub title: String,
    pub advice: String,
}

/// The troubleshooting guide, in likelihood order: the healthy-looking states
/// first, then the connect legs, then first-time setup.
#[uniffi::export]
pub fn troubleshooting_guides() -> Vec<GuideCopy> {
    let g = |slug: &str, title: &str, advice: &str| GuideCopy {
        slug: slug.into(),
        title: title.into(),
        advice: advice.into(),
    };
    vec![
        g(
            "game",
            "Showing scores as usual",
            "The panel keeps the latest scores up through brief network drops while it \
             reconnects in the background — so it can look perfectly normal while the app \
             says offline. Give it a minute: it either recovers quietly or switches to a \
             CONNECTING screen.",
        ),
        g(
            "black",
            "Completely dark",
            "The board is off, in quiet hours, or not getting power. Tap the knob and pick \
             Sports to wake it. If it stays dark, check that it's plugged in and that the \
             power adapter supplies 5V\u{2009}\u{2393}\u{2009}3A — the one from the box. \
             Weaker adapters can't run the panel.",
        ),
        g(
            "joining-wifi",
            "Joining Wi-Fi",
            "The board can't reach your Wi-Fi: the router may be down, the password may have \
             changed, or the board moved to a new network. If it persists, hold the knob for \
             about 10 seconds to factory reset the board, then set it up again.",
        ),
        g(
            "finding-server",
            "Finding the server",
            "Wi-Fi is fine but the internet or the scoreboard service isn't reachable. Check \
             your internet connection — the service may also be briefly restarting. The board \
             reconnects on its own.",
        ),
        g(
            "setup",
            "Waiting for setup",
            "The board is waiting to be set up over Bluetooth: open the app and tap Add \
             Scoreboard.",
        ),
        g(
            "pair-code",
            "Ready to pair",
            "The board is on Wi-Fi and ready to join the app: tap Add Scoreboard and type in \
             the code shown on the panel.",
        ),
    ]
}

#[uniffi::export]
pub fn troubleshooting_intro() -> String {
    "Match what the scoreboard's panel shows right now:".into()
}

#[uniffi::export]
pub fn troubleshooting_footer() -> String {
    "Whatever the panel shows, power-cycling is a safe first step: unplug the scoreboard \
     and plug it back in. It always recovers on its own once your network is back."
        .into()
}

/// The Spoilers screen's footer: the one place the full set of spoiler scopes
/// is stated, because they live on three different screens.
#[uniffi::export]
pub fn spoiler_explainer() -> String {
    "Spoiler protection hides scores and shows only the current period or a live \
     indicator. Spoiler protection can be turned on for all leagues, specific leagues, \
     individual teams, or individual games."
        .into()
}

/// The demo-games info card: the one setting whose consequence you cannot see
/// from its row — the scores go fake, and nothing on the panel says so.
#[uniffi::export]
pub fn demo_games_explainer() -> String {
    "Fills this scoreboard with a made-up slate of games — every sport and every layout \
     at once — so you can see what it does out of season or before your teams are \
     playing. The scores are NOT real.\n\nIt stays on until you turn it off; it will not \
     time out on its own. Nothing else about the scoreboard changes, and your favorites \
     and settings are waiting when you switch back.\n\nThe setting lives on the server, \
     so you can turn it off from any paired phone even if the scoreboard is unplugged."
        .into()
}

// ---- screens (MAR-117) ----
//
// The Screens page's copy, the held-mode footer and the settings transfer.
// Section headers ("Screens") stay on the platform per the charter above; the
// sentences live here.

/// What Sports is called everywhere — the rail, the Screens list, footers.
/// The protocol's name, which the board's knob also uses (uppercased).
#[uniffi::export]
pub fn sports_label() -> String {
    scoreboard_protocol::SCORES_LABEL.into()
}

#[uniffi::export]
pub fn add_screen_label() -> String {
    "Add Screen".into()
}

#[uniffi::export]
pub fn remove_screen_label() -> String {
    "Remove Screen".into()
}

/// The per-screen checkmark: part of what shows when nothing is on.
#[uniffi::export]
pub fn idle_toggle_label() -> String {
    "Show when nothing is on".into()
}

/// The Screens list's footer, in three runs so the idle timer can sit
/// inline as a tappable link: `before`, then `link` (when present), then
/// `after`. Concatenated, it is one sentence.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct UiIdleExplainer {
    pub before: String,
    /// "every 5 minutes": tapping it opens the idle-timer picker
    /// (`idle_cycle_title`, `idle_cycle_options`, committed with
    /// `set_idle_cycle`). None when there is nothing to pick — fewer than two
    /// checked screens, or a server that cannot store the timer — and the
    /// whole sentence is then plain text.
    pub link: Option<String>,
    pub after: String,
}

/// What the board does when nothing you follow is playing — the Screens
/// list's footer — for the rows `screen_list` returned and the board's idle
/// timer: dark with none checked, the one screen with one, and the rotation
/// with two or more, whose timer is the link when `editable`
/// (`screens_editable`). Where screens are not editable, show
/// `update_server_note` after it.
#[uniffi::export]
pub fn idle_explainer_parts(
    screens: Vec<crate::rules::UiScreen>,
    idle_cycle_secs: u32,
    editable: bool,
) -> UiIdleExplainer {
    const LEAD: &str = "When nothing you follow is playing, the board";
    let plain = |text: String| UiIdleExplainer { before: text, link: None, after: String::new() };
    let idle: Vec<&crate::rules::UiScreen> = screens.iter().filter(|s| s.idle && !s.fixed).collect();
    match idle.as_slice() {
        [] => plain(format!("{LEAD} stays dark.")),
        [one] => plain(format!("{LEAD} shows {}.", one.label)),
        _ => {
            let every = match crate::rules::idle_cycle_selected(idle_cycle_secs) {
                3600 => "every hour".to_string(),
                secs => format!("every {}", crate::rules::idle_cycle_label(secs)),
            };
            let before = format!("{LEAD} switches between the checked screens ");
            if editable {
                UiIdleExplainer { before, link: Some(every), after: ".".into() }
            } else {
                plain(format!("{before}{every}."))
            }
        }
    }
}

/// Shown in place of Add Screen when `can_add_screen` is false.
#[uniffi::export]
pub fn screen_cap_note() -> String {
    // Sports and Off are rows too, but not "your" screens.
    format!("You can have up to {} screens.", scoreboard_protocol::MAX_SCREENS - 2)
}

/// Shown in place of Add Screen when every screen the server offers is
/// already on the list (`add_screen_note`).
#[uniffi::export]
pub fn nothing_to_add_note() -> String {
    "Every screen this server offers is already on your list.".into()
}

/// Shown in place of Add Screen until the server's list of screens has
/// arrived (`add_screen_note`) — until then there is nothing true to offer.
#[uniffi::export]
pub fn screens_loading_note() -> String {
    "Loading this server's screens…".into()
}

/// Shown when `screens_editable` is false: the server predates screen lists.
#[uniffi::export]
pub fn update_server_note() -> String {
    "Update your server to add, remove, or reorder screens.".into()
}

/// The footer while a held screen runs with no timer: "Clock until changed."
#[uniffi::export]
pub fn mode_until_changed(label: String) -> String {
    format!("{label} until changed.")
}

/// The start of the footer while a held screen runs on its timer: "Clock —
/// back to Sports in ". The platform appends its own formatted duration
/// (relative time is locale formatting), then [`mode_back_suffix`].
#[uniffi::export]
pub fn mode_back_prefix(label: String) -> String {
    format!("{label} — back to {} in ", scoreboard_protocol::SCORES_LABEL)
}

/// The end of that footer, after the duration.
#[uniffi::export]
pub fn mode_back_suffix() -> String {
    ".".into()
}

/// The footer's button: hold the screen until changed (`clear_wake_timer`).
#[uniffi::export]
pub fn keep_label() -> String {
    "Keep".into()
}

/// The dashboard line while quiet hours darken the panel: "Quiet hours —
/// the screen is off until 7:00 AM." `end_time` is the window's end in the
/// reader's own clock format (locale formatting stays on the platform).
#[uniffi::export]
pub fn quiet_footer(end_time: String) -> String {
    format!("Quiet hours — the screen is off until {end_time}.")
}

/// The alert when Import settings finds nothing on the clipboard.
#[uniffi::export]
pub fn import_empty_title() -> String {
    "Nothing to import".into()
}

#[uniffi::export]
pub fn import_empty_message() -> String {
    "Copy an export from another scoreboard first (Export settings → Copy), then come \
     back and tap Import settings."
        .into()
}

/// The export sheet's summary of what travels.
#[uniffi::export]
pub fn export_sheet_summary() -> String {
    "League preferences, screens, spoiler rules, brightness, and timing. On the other \
     scoreboard, tap Import settings with this on the clipboard."
        .into()
}

/// The Transfer settings section's footer while connected.
#[uniffi::export]
pub fn export_footer() -> String {
    "Exports league preferences, screens, spoiler rules, brightness, and timing for \
     another scoreboard."
        .into()
}

/// The Transfer settings section's footer while disconnected.
#[uniffi::export]
pub fn export_unavailable_note() -> String {
    "Unavailable while disconnected from this scoreboard's server.".into()
}

// ---- derivation rules for text both platforms must build identically ----

use crate::UiSport;

/// One section of the league list: the category name and its league ids in
/// display order. Ids, not whole sports — the platform maps them back onto
/// whatever element type its screen holds (catalog entries, preference rows).
#[derive(uniffi::Record, Clone)]
pub struct UiLeagueGroup {
    pub category: String,
    pub sport_ids: Vec<String>,
}

fn category_of(s: &UiSport) -> String {
    if s.category.is_empty() { "Other leagues".into() } else { s.category.clone() }
}

/// Rank 0 is what an older server's catalog decodes to; it sorts LAST, not
/// first.
fn effective_rank(r: u32) -> u32 {
    if r == 0 { u32::MAX } else { r }
}

/// Group the catalog into sport sections, sections and leagues both in the
/// server's declared order.
///
/// Built from the groups that actually EXIST rather than by intersecting with
/// a list this build knows: a hardcoded section order silently drops any
/// league whose section it never heard of. A section ranks by its
/// best-ranked league, so section order comes from the same declaration as
/// league order; ties break alphabetically at both levels.
#[uniffi::export]
pub fn group_leagues(sports: Vec<UiSport>) -> Vec<UiLeagueGroup> {
    let mut sorted = sports;
    sorted.sort_by(|a, b| {
        effective_rank(a.category_rank)
            .cmp(&effective_rank(b.category_rank))
            .then_with(|| category_of(a).cmp(&category_of(b)))
            .then_with(|| a.name.cmp(&b.name))
    });
    let mut out: Vec<UiLeagueGroup> = Vec::new();
    for s in &sorted {
        let cat = category_of(s);
        match out.iter_mut().find(|g| g.category == cat) {
            Some(g) => g.sport_ids.push(s.id.clone()),
            None => out.push(UiLeagueGroup { category: cat, sport_ids: vec![s.id.clone()] }),
        }
    }
    out
}

/// "Conference Finals +2": name one round, count the rest — four labels
/// joined by separators is not readable at row width. None when nothing is
/// picked.
#[uniffi::export]
pub fn picked_rounds_label(picked_labels: Vec<String>) -> Option<String> {
    let first = picked_labels.first()?;
    Some(if picked_labels.len() == 1 {
        first.clone()
    } else {
        format!("{first} +{}", picked_labels.len() - 1)
    })
}

/// A league's subscription in one line — "2 teams · MLS Cup" — or None when
/// nothing is configured. Counts only favorites that still resolve against
/// the catalog: configs can carry orphans (the old "Formula 1" pseudo-team
/// from before the leaderboard-league split).
#[uniffi::export]
pub fn subscription_summary(
    sport: &UiSport,
    favorite_team_ids: Vec<String>,
    picked_round_ids: Vec<String>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let n = sport.teams.iter().filter(|t| favorite_team_ids.contains(&t.id)).count();
    match n {
        0 => {}
        1 => parts.push("1 team".into()),
        n => parts.push(format!("{n} teams")),
    }
    let labels: Vec<String> = sport
        .championship_rounds
        .iter()
        .filter(|r| picked_round_ids.contains(&r.id))
        .map(|r| r.label.clone())
        .collect();
    if let Some(l) = picked_rounds_label(labels) {
        parts.push(l);
    }
    if parts.is_empty() { None } else { Some(parts.join(" · ")) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{UiChampOption, UiTeam};

    fn sport(id: &str, name: &str, cat: &str, rank: u32) -> UiSport {
        UiSport {
            id: id.into(),
            name: name.into(),
            teams: Vec::new(),
            championship_options: Vec::new(),
            championship_rounds: Vec::new(),
            category: cat.into(),
            category_rank: rank,
            data_sources: Vec::new(),
            extras_label: String::new(),
        }
    }

    /// The ordering rules, pinned: rank orders sections and leagues, rank 0
    /// sorts last, an empty category becomes "Other leagues" at the end.
    #[test]
    fn sections_and_leagues_follow_the_declared_order() {
        let groups = group_leagues(vec![
            sport("cbb", "NCAAM", "Basketball", 22),
            sport("mystery", "Fork League", "", 0),
            sport("nhl", "NHL", "Hockey", 10),
            sport("nba", "NBA", "Basketball", 20),
        ]);
        let cats: Vec<&str> = groups.iter().map(|g| g.category.as_str()).collect();
        assert_eq!(cats, ["Hockey", "Basketball", "Other leagues"]);
        assert_eq!(groups[1].sport_ids, ["nba", "cbb"], "leagues rank within their section");
        assert_eq!(groups[2].sport_ids, ["mystery"], "rank 0 sorts last, never first");
    }

    #[test]
    fn summary_counts_only_favorites_the_catalog_still_has() {
        let mut s = sport("f1", "Formula 1", "Motorsport", 70);
        s.teams = vec![UiTeam {
            id: "f1-ver".into(),
            name: "Verstappen".into(),
            abbrev: "VER".into(),
            color: None,
            alt_color: None,
        }];
        s.championship_rounds = vec![
            UiChampOption { id: "gp".into(), label: "Grands Prix".into() },
            UiChampOption { id: "sprint".into(), label: "Sprints".into() },
        ];
        // One resolvable favorite, one orphan from an old config.
        let summary = subscription_summary(
            &s,
            vec!["f1-ver".into(), "f1-f1".into()],
            vec!["gp".into(), "sprint".into()],
        );
        assert_eq!(summary.as_deref(), Some("1 team · Grands Prix +1"));
        assert_eq!(subscription_summary(&s, Vec::new(), Vec::new()), None);
    }

    #[test]
    fn rounds_label_names_one_and_counts_the_rest() {
        assert_eq!(picked_rounds_label(Vec::new()), None);
        assert_eq!(picked_rounds_label(vec!["Playoffs".into()]).as_deref(), Some("Playoffs"));
        assert_eq!(
            picked_rounds_label(vec!["Conference Finals".into(), "a".into(), "b".into()]).as_deref(),
            Some("Conference Finals +2")
        );
    }

    fn screen(id: &str, label: &str, idle: bool, fixed: bool) -> crate::rules::UiScreen {
        crate::rules::UiScreen {
            id: id.into(),
            kind: id.into(),
            label: label.into(),
            preview: None,
            idle,
            fixed,
            has_settings: true,
            removable: !fixed,
        }
    }

    /// 0, 1 and 2+ checked screens each say what the board will do; with
    /// two or more the timer is the link — only where the server can store
    /// it — and the three runs always read as one sentence.
    #[test]
    fn the_idle_explainer_counts_the_checked_screens() {
        let whole = |e: &UiIdleExplainer| format!("{}{}{}", e.before, e.link.clone().unwrap_or_default(), e.after);
        let mut rows = vec![screen("scores", "Sports", false, true), screen("off", "Off", false, true)];
        for editable in [true, false] {
            let e = idle_explainer_parts(rows.clone(), 300, editable);
            assert_eq!(e.link, None, "nothing to pick");
            assert_eq!(whole(&e), "When nothing you follow is playing, the board stays dark.");
        }
        rows.push(screen("weather.2", "Forecast", true, false));
        let e = idle_explainer_parts(rows.clone(), 300, true);
        assert_eq!(e.link, None, "one screen has no timer");
        assert_eq!(whole(&e), "When nothing you follow is playing, the board shows Forecast.");
        rows.push(screen("clock", "Clock", true, false));
        let e = idle_explainer_parts(rows.clone(), 300, true);
        assert_eq!(
            e,
            UiIdleExplainer {
                before: "When nothing you follow is playing, the board switches between the checked screens ".into(),
                link: Some("every 5 minutes".into()),
                after: ".".into(),
            }
        );
        assert_eq!(idle_explainer_parts(rows.clone(), 3600, true).link.as_deref(), Some("every hour"));
        assert_eq!(idle_explainer_parts(rows.clone(), 400, true).link.as_deref(), Some("every 5 minutes"), "snapped");
        let old = idle_explainer_parts(rows, 300, false);
        assert_eq!(old.link, None, "a server without lists cannot store the timer: plain text");
        assert_eq!(whole(&old), whole(&e), "the same sentence either way");
    }

    #[test]
    fn screens_copy_names_sports_and_the_cap() {
        assert_eq!(sports_label(), "Sports");
        assert_eq!(screen_cap_note(), "You can have up to 8 screens.");
        assert_eq!(nothing_to_add_note(), "Every screen this server offers is already on your list.");
        assert_eq!(mode_until_changed("Clock".into()), "Clock until changed.");
        assert_eq!(mode_back_prefix("Clock".into()), "Clock — back to Sports in ");
        assert!(export_footer().contains("screens") && export_sheet_summary().contains("screens"));
        let black = troubleshooting_guides().into_iter().find(|g| g.slug == "black").unwrap();
        assert!(black.advice.contains("pick Sports") && !black.advice.contains("Scores"));
    }

    /// The Controls card and the Transfer section's remaining sentences,
    /// moved down from both platforms word for word.
    #[test]
    fn quiet_and_import_copy_live_in_the_core() {
        assert_eq!(quiet_footer("7:00 AM".into()), "Quiet hours — the screen is off until 7:00 AM.");
        assert_eq!(import_empty_title(), "Nothing to import");
        assert_eq!(
            import_empty_message(),
            "Copy an export from another scoreboard first (Export settings → Copy), then come back and \
             tap Import settings."
        );
    }

    /// The guide slugs are a server contract (`/guide/{slug}.png`) — pin them.
    #[test]
    fn guide_slugs_match_the_server_routes() {
        let slugs: Vec<String> = troubleshooting_guides().into_iter().map(|g| g.slug).collect();
        assert_eq!(slugs, ["game", "black", "joining-wifi", "finding-server", "setup", "pair-code"]);
    }
}
