//! `server team-names`: the offline "notify me for correction" report.
//!
//! For every TEAM-sport league it prints each team's resolved (shortened)
//! display in every available [`NameStyle`], measures the rendered pixel width —
//! in the SAME 5x8/4x6 fonts + optical kerning the board draws with — against the
//! team-row's available space (the frame minus the accent stripe minus a minimum
//! two-digit score column), and collects an OVERFLOW section listing every
//! (league, team, style, width) that STILL doesn't fit after shortening. Golf
//! last-names and F1 three-letter codes are covered from the live leaderboard
//! feeds. Runs headless and prints a clean report to stdout.

use crate::espn::{CatalogTeamRaw, EspnSource};
use crate::render;
use scoreboard_protocol::{default_name_style, name_style_options, NameStyle};

/// The label a league gives a style ("City" / "College" / "Country" …), or "—"
/// if the style isn't offered for that league.
fn style_label(sport_id: &str, style: NameStyle) -> &'static str {
    name_style_options(sport_id)
        .iter()
        .find(|(s, _)| *s == style)
        .map(|(_, l)| *l)
        .unwrap_or("—")
}

/// The on-board display for a team under `style`, from the raw catalog fields —
/// identical to what `render::draw_team_box` draws: the mascot Name goes through
/// `teams::short_name`, the Location through `teams::shorten_location`, and every
/// style is uppercased.
fn display_for(sport_id: &str, t: &CatalogTeamRaw, style: NameStyle) -> String {
    match style {
        NameStyle::Location => crate::teams::shorten_location(&t.location).to_ascii_uppercase(),
        NameStyle::Name => {
            crate::teams::short_name(sport_id, &t.abbrev, &t.short_display_name).to_ascii_uppercase()
        }
        NameStyle::Abbrev => t.abbrev.to_ascii_uppercase(),
    }
}

pub async fn run() {
    let source = EspnSource::new();
    println!("=== team-names report ===");
    println!("widths are rendered pixels (board fonts + kerning); a team row's");
    println!("`avail` reserves a minimum two-digit score column. `!` = overflows.\n");

    // (league, style label, display, width, avail, tag) — printed together at the end.
    let mut overflow: Vec<String> = Vec::new();

    // ---- team sports ----
    let catalog = source.fetch_team_catalog_raw().await;
    for (sport_id, sport_name, teams) in &catalog {
        let opts = name_style_options(sport_id);
        let def = default_name_style(sport_id);
        println!(
            "--- {sport_name} [{sport_id}] default style: {} ---",
            style_label(sport_id, def)
        );
        if teams.is_empty() {
            println!("  (no teams fetched — ESPN unreachable? skipping)\n");
            continue;
        }
        let mut sorted: Vec<&CatalogTeamRaw> = teams.iter().collect();
        sorted.sort_by(|a, b| a.abbrev.cmp(&b.abbrev));
        for t in sorted {
            let mut cells = Vec::new();
            for (style, label) in opts {
                let display = display_for(sport_id, t, *style);
                let (w, avail) = render::team_name_metrics(sport_id, &display);
                let flag = if w > avail { "!" } else { "" };
                cells.push(format!("{label}={display:?} {w}/{avail}px{flag}"));
                if w > avail {
                    overflow.push(format!(
                        "{sport_id:<5} {label:<12} {display:<18} {w}px > {avail}px  (abbrev {})",
                        t.abbrev
                    ));
                }
            }
            println!("  {:<6} {}", t.abbrev, cells.join("   "));
        }
        println!();
    }

    // ---- leaderboard sports (golf last names, F1 three-letter codes) ----
    for feed in ["pga", "lpga", "f1"] {
        let parsed = match source.fetch_feed_raw(feed).await {
            Ok(raw) => crate::espn::parse_feed(feed, &raw),
            Err(e) => Err(e),
        };
        match parsed {
            Ok(games) => {
                // (name, value, has_rank) — F1 rows carry a rank column, golf don't.
                let rows: Vec<(String, String, bool)> = games
                    .iter()
                    .filter_map(|g| g.leaderboard.as_ref())
                    .flat_map(|lb| {
                        lb.rows
                            .iter()
                            .map(|r| (r.name.clone(), r.value.clone(), !r.rank.is_empty()))
                    })
                    .collect();
                println!("--- {feed} leaderboard names ({} rows) ---", rows.len());
                if rows.is_empty() {
                    println!("  (no live event — nothing to measure)");
                }
                for (name, value, has_rank) in &rows {
                    let (w, avail) = render::leaderboard_name_metrics(name, value, *has_rank);
                    let flag = if w > avail { "!" } else { "" };
                    println!("  {name:<14} value={value:<8} {w}/{avail}px{flag}");
                    if w > avail {
                        overflow.push(format!(
                            "{feed:<5} {:<12} {name:<18} {w}px > {avail}px  (value {value})",
                            "leaderboard"
                        ));
                    }
                }
                println!();
            }
            Err(e) => println!("--- {feed} --- fetch/parse failed: {e}\n"),
        }
    }

    // ---- OVERFLOW section LAST, so `tail` catches it ----
    println!("=== OVERFLOW: names that STILL don't fit after shortening ===");
    println!("(hand-add a curated short name / location override for each)\n");
    if overflow.is_empty() {
        println!("  none — every team fits its row in every available style");
    } else {
        for line in &overflow {
            println!("  {line}");
        }
        println!("\n  {} overflowing (league, team, style) entries", overflow.len());
    }
}
