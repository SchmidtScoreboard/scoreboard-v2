//! Sports data behind a swappable `SportsDataSource` trait. `EspnSource` pulls
//! live games from ESPN's public scoreboard API (ported from the original
//! Python backend); `MockSource` provides offline fallback games.

use crate::model::{ensure_contrast, hex_to_rgb, Champ, Game, GameStatus, Leaderboard, LeaderRow, TeamSide};
use async_trait::async_trait;
use scoreboard_protocol::{Catalog, SportDescriptor, Team};
use serde::Deserialize;
use std::sync::atomic::{AtomicU64, Ordering};

#[async_trait]
pub trait SportsDataSource: Send + Sync {
    async fn fetch(&self) -> Vec<Game>;
    /// Full team catalog (real rosters). Default None → caller uses the mock list.
    async fn fetch_catalog(&self) -> Option<Catalog> {
        None
    }
}

struct League {
    sport_id: &'static str,
    sport: &'static str,
    league: &'static str,
    name: &'static str,        // catalog display name
    games_query: &'static str, // appended to the scoreboard URL (e.g. "?groups=50")
}

pub struct EspnSource {
    client: reqwest::Client,
    leagues: Vec<League>,
}

impl EspnSource {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            leagues: vec![
                League { sport_id: "nhl", sport: "hockey", league: "nhl", name: "NHL", games_query: "" },
                League { sport_id: "nba", sport: "basketball", league: "nba", name: "NBA", games_query: "" },
                League { sport_id: "mlb", sport: "baseball", league: "mlb", name: "MLB", games_query: "" },
                League { sport_id: "nfl", sport: "football", league: "nfl", name: "NFL", games_query: "" },
                League { sport_id: "cfb", sport: "football", league: "college-football", name: "College Football", games_query: "" },
                League { sport_id: "cbb", sport: "basketball", league: "mens-college-basketball", name: "Men's College Basketball", games_query: "?groups=50" },
                League { sport_id: "wcbb", sport: "basketball", league: "womens-college-basketball", name: "Women's College Basketball", games_query: "?groups=50" },
                League { sport_id: "wnba", sport: "basketball", league: "wnba", name: "WNBA", games_query: "" },
            ],
        }
    }
}

#[async_trait]
impl SportsDataSource for EspnSource {
    async fn fetch(&self) -> Vec<Game> {
        let mut out = Vec::new();
        for lg in &self.leagues {
            let url = format!(
                "https://site.api.espn.com/apis/site/v2/sports/{}/{}/scoreboard{}",
                lg.sport, lg.league, lg.games_query
            );
            match self.client.get(&url).send().await {
                Ok(resp) => match resp.json::<Scoreboard>().await {
                    Ok(sb) => {
                        let n = sb.events.len();
                        for ev in sb.events {
                            if let Some(g) = parse_event(lg.sport_id, ev) {
                                out.push(g);
                            }
                        }
                        tracing::info!("espn {}: {} events", lg.league, n);
                    }
                    Err(e) => tracing::warn!("espn parse {}: {e}", lg.league),
                },
                Err(e) => tracing::warn!("espn fetch {}: {e}", lg.league),
            }
        }
        out.extend(self.fetch_golf().await);
        out.extend(self.fetch_f1().await);
        out
    }

    async fn fetch_catalog(&self) -> Option<Catalog> {
        let mut sports = Vec::new();
        for lg in &self.leagues {
            let url = format!(
                "https://site.api.espn.com/apis/site/v2/sports/{}/{}/teams?limit=500",
                lg.sport, lg.league
            );
            let mut teams: Vec<Team> = match self.client.get(&url).send().await {
                Ok(resp) => match resp.json::<TeamsResponse>().await {
                    Ok(tr) => tr
                        .sports
                        .into_iter()
                        .flat_map(|s| s.leagues)
                        .flat_map(|l| l.teams)
                        .filter_map(|w| {
                            let t = w.team;
                            let abbrev = t.abbreviation;
                            if abbrev.is_empty() {
                                return None;
                            }
                            let name = if t.display_name.is_empty() {
                                t.short_display_name
                            } else {
                                t.display_name
                            };
                            // ESPN gives "" when a color is absent — normalize to None.
                            let hex = |s: String| (!s.is_empty()).then_some(s);
                            Some(Team {
                                id: crate::team_key(lg.sport_id, &abbrev),
                                league_id: lg.sport_id.into(),
                                name,
                                abbrev,
                                color: hex(t.color),
                                alt_color: hex(t.alternate_color),
                            })
                        })
                        .collect(),
                    Err(e) => {
                        tracing::warn!("espn teams parse {}: {e}", lg.league);
                        Vec::new()
                    }
                },
                Err(e) => {
                    tracing::warn!("espn teams fetch {}: {e}", lg.league);
                    Vec::new()
                }
            };
            if teams.is_empty() {
                continue;
            }
            teams.sort_by(|a, b| a.name.cmp(&b.name));
            tracing::info!("espn teams {}: {}", lg.league, teams.len());
            sports.push(SportDescriptor {
                id: lg.sport_id.into(),
                name: lg.name.into(),
                teams,
                championship_options: crate::championship_options_for(lg.sport_id),
            });
        }
        if sports.is_empty() {
            return None; // ESPN team fetch unavailable -> caller falls back to mock_catalog
        }
        // Leaderboard sports (golf PGA/LPGA + F1) aren't ESPN team lists — add them.
        sports.push(crate::golf_sport());
        sports.push(crate::f1_sport());
        Some(Catalog { sports })
    }
}

// ---- golf + F1 (leaderboard sports) ----
impl EspnSource {
    async fn fetch_golf(&self) -> Vec<Game> {
        let mut out = Vec::new();
        for tour in ["pga", "lpga"] {
            let url =
                format!("https://site.api.espn.com/apis/site/v2/sports/golf/leaderboard?league={tour}");
            let events = match self.client.get(&url).send().await {
                Ok(r) => match r.json::<LeaderboardResponse>().await {
                    Ok(g) => g.events,
                    Err(e) => {
                        tracing::warn!("golf parse {tour}: {e}");
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!("golf fetch {tour}: {e}");
                    continue;
                }
            };
            let tag = tour.to_uppercase();
            for ev in events {
                if let Some(g) = parse_golf(&tag, ev) {
                    out.push(g);
                }
            }
        }
        out
    }

    async fn fetch_f1(&self) -> Vec<Game> {
        let url = "https://site.api.espn.com/apis/site/v2/sports/racing/f1/scoreboard";
        let events = match self.client.get(url).send().await {
            Ok(r) => match r.json::<LeaderboardResponse>().await {
                Ok(g) => g.events,
                Err(e) => {
                    tracing::warn!("f1 parse: {e}");
                    return Vec::new();
                }
            },
            Err(e) => {
                tracing::warn!("f1 fetch: {e}");
                return Vec::new();
            }
        };
        events.into_iter().filter_map(parse_f1).collect()
    }
}

/// A leaderboard "event" (especially an F1 race weekend) can carry several
/// competitions — practice, qualifying, sprint, the race. ESPN lists them in
/// chronological order, so `.next()` grabs the *first* (often a finished practice
/// session) rather than what's actually happening. Pick the most relevant: one in
/// progress, else the soonest upcoming, else the most recent finished.
fn pick_leader_comp(comps: Vec<LeaderComp>) -> Option<LeaderComp> {
    fn category(name: &str) -> u8 {
        match map_status(name) {
            Some(GameStatus::Active) | Some(GameStatus::Intermission) => 2, // live
            Some(GameStatus::Pregame) => 1,                                  // upcoming
            _ => 0,                                                          // finished/unknown
        }
    }
    comps.into_iter().max_by(|a, b| {
        let (ca, cb) = (category(&a.status.type_.name), category(&b.status.type_.name));
        if ca != cb {
            ca.cmp(&cb) // live > upcoming > finished
        } else if ca == 1 {
            b.date.cmp(&a.date) // upcoming: soonest (earliest date) wins
        } else {
            a.date.cmp(&b.date) // live/finished: latest date wins
        }
    })
}

fn parse_golf(tag: &str, ev: LeaderEvent) -> Option<Game> {
    let comp = pick_leader_comp(ev.competitions)?;
    let status = map_status(&comp.status.type_.name).unwrap_or(GameStatus::Pregame);
    // Top 4 by current position (ties allowed), name = last name, score = "+5"/"E".
    let mut ranked: Vec<(i64, LeaderRow)> = comp
        .competitors
        .into_iter()
        .filter_map(|c| {
            let pos = c.status.as_ref()?.position.as_ref()?.id;
            if pos <= 0 || pos > 4 {
                return None;
            }
            let name = golfer_last_name(&c.athlete.as_ref()?.display_name);
            let score = c
                .statistics
                .first()
                .map(|s| s.display_value.clone())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "E".into());
            Some((pos, LeaderRow { name, value: score }))
        })
        .collect();
    ranked.sort_by_key(|(p, _)| *p);
    let rows = ranked.into_iter().take(4).map(|(_, r)| r).collect();
    Some(Game {
        id: ev.id,
        sport_id: "golf".into(),
        away: TeamSide::empty(tag),
        home: TeamSide::empty(""),
        status,
        ordinal: String::new(),
        clock: String::new(),
        start_time: comp.date,
        leaderboard: Some(Leaderboard { name: clean_golf_name(&ev.short_name), rows }),
        champ: None,
    })
}

fn parse_f1(ev: LeaderEvent) -> Option<Game> {
    let comp = pick_leader_comp(ev.competitions)?;
    let status = map_status(&comp.status.type_.name).unwrap_or(GameStatus::Pregame);
    let mut drivers: Vec<(i64, LeaderRow)> = comp
        .competitors
        .into_iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let name = f1_last_name(&c.athlete.as_ref()?.display_name);
            let order = if c.order > 0 { c.order } else { i as i64 + 1 };
            let value = c
                .statistics
                .first()
                .map(|s| s.display_value.clone())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| format!("P{order}"));
            Some((order, LeaderRow { name, value }))
        })
        .collect();
    drivers.sort_by_key(|(o, _)| *o);
    let rows = drivers.into_iter().take(4).map(|(_, r)| r).collect();
    Some(Game {
        id: ev.id,
        sport_id: "f1".into(),
        away: TeamSide::empty("F1"),
        home: TeamSide::empty(""),
        status,
        ordinal: String::new(),
        clock: String::new(),
        start_time: comp.date,
        leaderboard: Some(Leaderboard { name: clean_f1_name(&ev.short_name), rows }),
        champ: None,
    })
}

fn golfer_last_name(full: &str) -> String {
    let up = full.to_uppercase();
    let mut parts: Vec<&str> = up.split_whitespace().collect();
    let suffixes = ["JR.", "JR", "SR", "SR.", "II", "III", "IV", "V", "VI"];
    while parts.len() > 1 && suffixes.contains(parts.last().unwrap()) {
        parts.pop();
    }
    parts.last().copied().unwrap_or("").chars().filter(|c| c.is_alphanumeric()).collect()
}

fn f1_last_name(full: &str) -> String {
    full.to_uppercase()
        .split_whitespace()
        .last()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// Shorten a tournament `shortName` to fit the panel (ported from `golf.py`).
fn clean_golf_name(short: &str) -> String {
    let up = short.to_uppercase();
    let mapped = golf_name_map(&up).map(String::from).unwrap_or(up);
    let pre = mapped.split("PRES").next().unwrap_or(&mapped).trim().to_string();
    let mut words: Vec<&str> = pre.split_whitespace().collect();
    for filler in ["TOURNAMENT", "CHAMPIONSHIP", "CHALLENGE", "CLASSIC", "INVITATIONAL"] {
        if let Some(idx) = words.iter().position(|w| *w == filler) {
            words.truncate(idx);
        }
    }
    if words.first() == Some(&"THE") {
        words.remove(0);
    }
    if words
        .first()
        .map_or(false, |w| w.chars().all(|c| c.is_ascii_digit()) || *w == "AT&T" || *w == "WGC-FEDEX")
    {
        words.remove(0);
    }
    if let Some(idx) = words.iter().position(|w| *w == "OF") {
        words.truncate(idx);
    }
    let name = words.join(" ");
    if words.first() == Some(&"MASTERS") {
        format!("THE {name}")
    } else {
        name
    }
}

fn golf_name_map(name: &str) -> Option<&'static str> {
    Some(match name {
        "SHRINERS CHILDREN'S OPEN" => "SHRINERS OPEN",
        "BUTTERFIELD BERMUDA CHAMPIONSHIP" => "BERMUDA CHAMP",
        "WORLD WIDE TECHNOLOGY CHAMPIONSHIP AT MAYAKOBA"
        | "WORLD WIDE TECHNOLOGY CHAMPIONSHIP" => "WWT CHAMP",
        "FARMERS INSURANCE OPEN" => "FARMERS OPEN",
        "SONY OPEN IN HAWAII" => "SONY OPEN",
        "AT&T PEBBLE BEACH PRO-AM" => "PEBBLE BEACH",
        "WASTE MANAGEMENT PHOENIX OPEN" => "WM PHOENIX",
        "VALERO TEXAS OPEN" => "VALERO OPEN",
        "RBC CANADIAN OPEN" => "RBC CANADIAN",
        "GENESIS SCOTTISH OPEN" => "SCOTTISH OPEN",
        "CADENCE BANK HOUSTON OPEN" | "TEXAS CHILDREN'S HOUSTON OPEN" => "HOUSTON OPEN",
        "MEXICO OPEN AT VIDANTA" => "MEXICO OPEN",
        "THE CJ CUP BYRON NELSON" => "BYRON NELSON",
        "ONEFLIGHT MYRTLE BEACH CLASSIC" => "MYRTLE BEACH",
        _ => return None,
    })
}

/// Shorten an F1 race `shortName` (drop leading sponsor words until it fits).
fn clean_f1_name(short: &str) -> String {
    let up = short.to_uppercase();
    let mut words: Vec<&str> = up.split_whitespace().collect();
    while words.len() > 2 && words.join(" ").chars().count() > 13 {
        words.remove(0);
    }
    words.join(" ")
}

/// ESPN encodes some numeric ids as strings ("72") — accept either.
fn de_flexible_i64<'de, D>(d: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StrOrInt {
        Int(i64),
        Str(String),
    }
    Ok(match StrOrInt::deserialize(d)? {
        StrOrInt::Int(i) => i,
        StrOrInt::Str(s) => s.parse().unwrap_or(0),
    })
}

// ---- leaderboard JSON (golf leaderboard + racing scoreboard share a shape) ----
#[derive(Deserialize)]
struct LeaderboardResponse {
    #[serde(default)]
    events: Vec<LeaderEvent>,
}
#[derive(Deserialize)]
struct LeaderEvent {
    #[serde(default)]
    id: String,
    #[serde(default, rename = "shortName")]
    short_name: String,
    #[serde(default)]
    competitions: Vec<LeaderComp>,
}
#[derive(Deserialize)]
struct LeaderComp {
    #[serde(default)]
    date: String,
    status: Status,
    #[serde(default)]
    competitors: Vec<LeaderCompetitor>,
}
#[derive(Deserialize)]
struct LeaderCompetitor {
    #[serde(default, deserialize_with = "de_flexible_i64")]
    order: i64,
    athlete: Option<LeaderAthlete>,
    status: Option<LeaderCompStatus>,
    #[serde(default)]
    statistics: Vec<LeaderStat>,
}
#[derive(Deserialize)]
struct LeaderAthlete {
    #[serde(default, rename = "displayName")]
    display_name: String,
}
#[derive(Deserialize)]
struct LeaderCompStatus {
    position: Option<LeaderPosition>,
}
#[derive(Deserialize)]
struct LeaderPosition {
    #[serde(default, deserialize_with = "de_flexible_i64")]
    id: i64,
}
#[derive(Deserialize)]
struct LeaderStat {
    #[serde(default, rename = "displayValue")]
    display_value: String,
}

/// Offline fallback source (swappable with `EspnSource` via the trait).
#[allow(dead_code)]
/// Fixed two-game slate for tests (`AppState::new_ephemeral`) — a known
/// rotation to exercise selection/exclusion logic against. Never served to
/// real boards: production starts with an empty slate and stays empty when
/// the source has nothing, rather than showing made-up scores.
#[cfg(test)]
pub fn mock_games() -> Vec<Game> {
    let bruins = TeamSide {
        abbrev: "BOS".into(), name: "BRUINS".into(), score: 2,
        primary: hex_to_rgb("FFB81C"), secondary: hex_to_rgb("000000"),
    };
    let leafs = TeamSide {
        abbrev: "TOR".into(), name: "LEAFS".into(), score: 1,
        primary: hex_to_rgb("00205B"), secondary: hex_to_rgb("FFFFFF"),
    };
    let celtics = TeamSide {
        abbrev: "BOS".into(), name: "CELTICS".into(), score: 88,
        primary: hex_to_rgb("007A33"), secondary: hex_to_rgb("FFFFFF"),
    };
    let lakers = TeamSide {
        abbrev: "LAL".into(), name: "LAKERS".into(), score: 84,
        primary: hex_to_rgb("552583"), secondary: hex_to_rgb("FDB927"),
    };
    vec![
        Game {
            id: "mock-nhl".into(), sport_id: "nhl".into(), away: bruins, home: leafs,
            status: GameStatus::Active, ordinal: "2nd".into(), clock: "12:34".into(),
            start_time: String::new(),
            leaderboard: None,
            // Tagged so the app's big-game caption + selection are exercisable
            // offline (outside playoff season there are no live postseason
            // games to test against).
            champ: Some(Champ { tier: "final".into(), label: "Stanley Cup Final".into() }),
        },
        Game {
            id: "mock-nba".into(), sport_id: "nba".into(), away: celtics, home: lakers,
            status: GameStatus::Active, ordinal: "4th".into(), clock: "5:20".into(),
            start_time: String::new(),
            leaderboard: None,
            champ: None,
        },
    ]
}

/// Demo source: synthetic live games with ticking scores/clocks (for `--demo`).
pub struct DemoSource {
    tick: AtomicU64,
    /// Wall-clock baseline for the scripted "game that ends" — the tick counter
    /// depends on poll cadence, so it can't time a five-minute story.
    started: std::time::Instant,
}

impl DemoSource {
    pub fn new() -> Self {
        Self { tick: AtomicU64::new(0), started: std::time::Instant::now() }
    }
}

#[async_trait]
impl SportsDataSource for DemoSource {
    async fn fetch(&self) -> Vec<Game> {
        demo_games(self.tick.fetch_add(1, Ordering::Relaxed), self.started.elapsed().as_secs())
    }
}

fn demo_games(n: u64, elapsed_secs: u64) -> Vec<Game> {
    // The scripted ending: live for the first five minutes of the server's life,
    // then FINAL — exercises the observed live->Final transition and the
    // half-hour linger without waiting for a real game to end.
    let ending_live = elapsed_secs < 5 * 60;
    let team = |abbrev: &str, name: &str, score: u32, p: &str, s: &str| TeamSide {
        abbrev: abbrev.into(),
        name: name.into(),
        score,
        primary: hex_to_rgb(p),
        secondary: ensure_contrast(hex_to_rgb(p), hex_to_rgb(s)),
    };
    let clock = format!("{}:{:02}", 19 - (n % 20), (n * 7) % 60);
    let period = |base: u64| {
        match ((n / 4) + base) % 4 + 1 {
            1 => "1st",
            2 => "2nd",
            3 => "3rd",
            _ => "4th",
        }
        .to_string()
    };
    vec![
        // The two NHL demo games carry different championship tiers so both
        // trophy-caption variants (and the big-games selection) are
        // exercisable in demo mode.
        Game {
            id: "demo-nhl".into(), sport_id: "nhl".into(),
            away: team("BOS", "BRUINS", (n / 3 % 6) as u32, "FFB81C", "000000"),
            home: team("TOR", "LEAFS", (n / 5 % 5) as u32, "00205B", "FFFFFF"),
            status: GameStatus::Active, ordinal: period(0), clock: clock.clone(), start_time: String::new(),
            leaderboard: None,
            champ: Some(Champ { tier: "final".into(), label: "Stanley Cup Final".into() }),
        },
        Game {
            id: "demo-nhl-2".into(), sport_id: "nhl".into(),
            away: team("COL", "AVALANCHE", (n / 4 % 5) as u32, "6F263D", "236192"),
            home: team("VGK", "KNIGHTS", (n / 6 % 4) as u32, "B4975A", "333F42"),
            status: GameStatus::Active, ordinal: period(2), clock: clock.clone(), start_time: String::new(),
            leaderboard: None,
            champ: Some(Champ { tier: "conf".into(), label: "Conference Finals".into() }),
        },
        Game {
            id: "demo-nba".into(), sport_id: "nba".into(),
            away: team("BOS", "CELTICS", (80 + n % 30) as u32, "007A33", "FFFFFF"),
            home: team("LAL", "LAKERS", (78 + n % 28) as u32, "552583", "FDB927"),
            status: GameStatus::Active, ordinal: period(1), clock: clock.clone(), start_time: String::new(),
            leaderboard: None,
            champ: None,
        },
        Game {
            id: "demo-nba-2".into(), sport_id: "nba".into(),
            away: team("GSW", "WARRIORS", (85 + n % 25) as u32, "1D428A", "FFC72C"),
            home: team("DEN", "NUGGETS", (88 + n % 22) as u32, "0E2240", "FEC524"),
            status: GameStatus::Active, ordinal: period(3), clock: clock.clone(), start_time: String::new(),
            leaderboard: None,
            champ: None,
        },
        Game {
            id: "demo-nba-3".into(), sport_id: "nba".into(),
            away: team("MIA", "HEAT", (70 + n % 26) as u32, "98002E", "F9A01B"),
            home: team("NYK", "KNICKS", (72 + n % 24) as u32, "006BB6", "F58426"),
            status: GameStatus::Active, ordinal: period(0), clock: clock.clone(), start_time: String::new(),
            leaderboard: None,
            champ: None,
        },
        Game {
            id: "demo-mlb".into(), sport_id: "mlb".into(),
            away: team("NYY", "YANKEES", (n / 7 % 9) as u32, "132448", "C4CED3"),
            home: team("BOS", "RED SOX", (n / 6 % 9) as u32, "BD3039", "FFFFFF"),
            status: GameStatus::Active, ordinal: period(2), clock: String::new(), start_time: String::new(),
            leaderboard: None,
            champ: None,
        },
        Game {
            id: "demo-mlb-2".into(), sport_id: "mlb".into(),
            away: team("LAD", "DODGERS", (n / 5 % 8) as u32, "005A9C", "EF3E42"),
            home: team("CHC", "CUBS", (n / 8 % 7) as u32, "0E3386", "CC3433"),
            status: GameStatus::Active, ordinal: period(1), clock: String::new(), start_time: String::new(),
            leaderboard: None,
            champ: None,
        },
        Game {
            id: "demo-mlb-ending".into(), sport_id: "mlb".into(),
            away: team("HOU", "ASTROS", 5, "002D62", "EB6E1F"),
            home: team("SEA", "MARINERS", 4, "0C2C56", "005C5C"),
            status: if ending_live { GameStatus::Active } else { GameStatus::Final },
            ordinal: if ending_live { "9th".into() } else { String::new() },
            clock: String::new(), start_time: String::new(),
            leaderboard: None,
            champ: None,
        },
        Game {
            id: "demo-nfl".into(), sport_id: "nfl".into(),
            away: team("NE", "PATRIOTS", (n / 9 % 35) as u32, "002244", "C60C30"),
            home: team("BUF", "BILLS", (n / 8 % 35) as u32, "00338D", "FFFFFF"),
            status: GameStatus::Active, ordinal: period(3), clock, start_time: String::new(),
            leaderboard: None,
            champ: None,
        },
        Game {
            id: "demo-golf".into(), sport_id: "golf".into(),
            away: TeamSide::empty("PGA"), home: TeamSide::empty(""),
            status: GameStatus::Active, ordinal: String::new(), clock: String::new(), start_time: String::new(),
            leaderboard: Some(Leaderboard { name: "MASTERS".into(), rows: vec![
                LeaderRow { name: "SCHEFFLER".into(), value: "-8".into() },
                LeaderRow { name: "MCILROY".into(), value: "-6".into() },
                LeaderRow { name: "MORIKAWA".into(), value: "-5".into() },
                LeaderRow { name: "SCHAUFFELE".into(), value: "-3".into() },
            ] }),
            champ: None,
        },
        Game {
            id: "demo-golf-2".into(), sport_id: "golf".into(),
            away: TeamSide::empty("LPGA"), home: TeamSide::empty(""),
            status: GameStatus::Active, ordinal: String::new(), clock: String::new(), start_time: String::new(),
            leaderboard: Some(Leaderboard { name: "US OPEN".into(), rows: vec![
                LeaderRow { name: "KORDA".into(), value: "-7".into() },
                LeaderRow { name: "THITIKUL".into(), value: "-5".into() },
                LeaderRow { name: "VU".into(), value: "-4".into() },
                LeaderRow { name: "SAIGO".into(), value: "-2".into() },
            ] }),
            champ: None,
        },
        Game {
            id: "demo-f1".into(), sport_id: "f1".into(),
            away: TeamSide::empty("F1"), home: TeamSide::empty(""),
            status: GameStatus::Active, ordinal: String::new(), clock: String::new(), start_time: String::new(),
            leaderboard: Some(Leaderboard { name: "MONACO GP".into(), rows: vec![
                LeaderRow { name: "VERSTAPPEN".into(), value: "LAP 42".into() },
                LeaderRow { name: "NORRIS".into(), value: "+2.1".into() },
                LeaderRow { name: "LECLERC".into(), value: "+5.8".into() },
                LeaderRow { name: "PIASTRI".into(), value: "+9.0".into() },
            ] }),
            champ: None,
        },
        // Upcoming games: one inside the 30-min pregame window (appears on the
        // board with its start time), one hours out (View-all browse only).
        Game {
            id: "demo-nhl-soon".into(), sport_id: "nhl".into(),
            away: team("NYR", "RANGERS", 0, "0038A8", "CE1126"),
            home: team("EDM", "OILERS", 0, "041E42", "FF4C00"),
            status: GameStatus::Pregame, ordinal: String::new(), clock: String::new(),
            start_time: (chrono::Utc::now() + chrono::Duration::minutes(20)).format("%Y-%m-%dT%H:%MZ").to_string(),
            leaderboard: None,
            champ: None,
        },
        Game {
            id: "demo-nba-tonight".into(), sport_id: "nba".into(),
            away: team("MIL", "BUCKS", 0, "00471B", "EEE1C6"),
            home: team("PHX", "SUNS", 0, "1D1160", "E56020"),
            status: GameStatus::Pregame, ordinal: String::new(), clock: String::new(),
            start_time: (chrono::Utc::now() + chrono::Duration::hours(3)).format("%Y-%m-%dT%H:%MZ").to_string(),
            leaderboard: None,
            champ: None,
        },
    ]
}

// ---- ESPN JSON (subset) ----
#[derive(Deserialize)]
struct Scoreboard {
    #[serde(default)]
    events: Vec<Event>,
}
#[derive(Deserialize)]
struct Event {
    #[serde(default)]
    competitions: Vec<Competition>,
    /// Season context ("type": 2 = regular season, 3 = postseason). Drives the
    /// championship classification for the big-games preference.
    season: Option<EventSeason>,
}
#[derive(Deserialize)]
struct EventSeason {
    #[serde(default, rename = "type")]
    type_: i64,
    #[serde(default)]
    slug: String,
}
#[derive(Deserialize)]
struct Competition {
    #[serde(default)]
    id: String,
    #[serde(default)]
    date: String,
    #[serde(default)]
    competitors: Vec<Competitor>,
    status: Status,
    /// Round context, e.g. {"headline": "Stanley Cup Final - Game 3"}. Refines
    /// the postseason tier (final vs conference finals vs early rounds).
    #[serde(default)]
    notes: Vec<CompNote>,
}
#[derive(Deserialize)]
struct CompNote {
    #[serde(default)]
    headline: String,
}
#[derive(Deserialize)]
struct Status {
    #[serde(rename = "type")]
    type_: StatusType,
    #[serde(default)]
    period: i64,
    #[serde(default, rename = "displayClock")]
    display_clock: String,
}
#[derive(Deserialize)]
struct StatusType {
    #[serde(default)]
    name: String,
}
#[derive(Deserialize)]
struct Competitor {
    #[serde(default)]
    score: String,
    #[serde(default, rename = "homeAway")]
    home_away: String,
    team: EspnTeam,
}
#[derive(Deserialize)]
struct EspnTeam {
    #[serde(default)]
    abbreviation: String,
    #[serde(default, rename = "shortDisplayName")]
    short_display_name: String,
    #[serde(default)]
    color: String,
    #[serde(default, rename = "alternateColor")]
    alternate_color: String,
}

// ---- ESPN teams endpoint JSON (subset) ----
#[derive(Deserialize)]
struct TeamsResponse {
    #[serde(default)]
    sports: Vec<TeamsSport>,
}
#[derive(Deserialize)]
struct TeamsSport {
    #[serde(default)]
    leagues: Vec<TeamsLeague>,
}
#[derive(Deserialize)]
struct TeamsLeague {
    #[serde(default)]
    teams: Vec<TeamWrap>,
}
#[derive(Deserialize)]
struct TeamWrap {
    team: CatalogTeam,
}
#[derive(Deserialize)]
struct CatalogTeam {
    #[serde(default)]
    abbreviation: String,
    #[serde(default, rename = "displayName")]
    display_name: String,
    #[serde(default, rename = "shortDisplayName")]
    short_display_name: String,
    #[serde(default)]
    color: String,
    #[serde(default, rename = "alternateColor")]
    alternate_color: String,
}

fn parse_event(sport_id: &str, ev: Event) -> Option<Game> {
    let season = ev.season;
    let comp = ev.competitions.into_iter().next()?;
    let status = map_status(&comp.status.type_.name)?;
    let champ = classify_champ(
        sport_id,
        season.as_ref(),
        &comp.notes.iter().map(|n| n.headline.as_str()).collect::<Vec<_>>().join(" | "),
    );

    let mut home = None;
    let mut away = None;
    for c in comp.competitors {
        let primary = hex_to_rgb(if c.team.color.is_empty() { "222222" } else { &c.team.color });
        let secondary = ensure_contrast(
            primary,
            hex_to_rgb(if c.team.alternate_color.is_empty() { "FFFFFF" } else { &c.team.alternate_color }),
        );
        let name = crate::teams::short_name(sport_id, &c.team.abbreviation, &c.team.short_display_name);
        let side = TeamSide {
            abbrev: c.team.abbreviation.clone(),
            name,
            score: c.score.parse().unwrap_or(0),
            primary,
            secondary,
        };
        match c.home_away.as_str() {
            "home" => home = Some(side),
            "away" => away = Some(side),
            _ => {}
        }
    }

    let ordinal = ordinal_for(&comp.status, status);
    let clock = if status == GameStatus::Active {
        comp.status.display_clock.clone()
    } else {
        String::new()
    };

    Some(Game {
        id: comp.id,
        sport_id: sport_id.into(),
        home: home?,
        away: away?,
        status,
        ordinal,
        clock,
        start_time: comp.date,
        leaderboard: None,
        champ,
    })
}

/// Classify a game's championship tier from ESPN's season type + the
/// competition notes headline. The tier vocabulary matches the sport's
/// championship options (`championship_options_for`), so the board's
/// big-games preference can be checked against it directly. Headline
/// keywords beat the season flag (they're the more specific signal); a
/// postseason game with no recognizable headline falls back to the
/// sport's broadest tier. Regular-season games return None.
fn classify_champ(sport_id: &str, season: Option<&EventSeason>, headline: &str) -> Option<Champ> {
    let postseason = season.map(|s| s.type_ == 3 || s.slug == "post-season").unwrap_or(false);
    let h = headline.to_lowercase();
    let tag = |tier: &str, label: &str| Some(Champ { tier: tier.into(), label: label.into() });

    match sport_id {
        "nhl" => {
            if h.contains("stanley cup final") {
                return tag("final", "Stanley Cup Final");
            }
            if h.contains("conference final") {
                return tag("conf", "Conference Finals");
            }
        }
        "nba" => {
            if h.contains("nba finals") {
                return tag("final", "NBA Finals");
            }
        }
        "wnba" => {
            if h.contains("wnba finals") {
                return tag("playoffs", "WNBA Finals");
            }
        }
        "mlb" => {
            if h.contains("world series") {
                return tag("ws", "World Series");
            }
        }
        "nfl" => {
            if h.contains("super bowl") {
                return tag("sb", "Super Bowl");
            }
        }
        "cfb" => {
            if h.contains("college football playoff") || h.contains("cfp") {
                return tag("playoff", "CFP");
            }
            if h.contains("bowl") {
                return tag("bowl", "Bowl Game");
            }
        }
        "cbb" | "wcbb" => {
            if h.contains("ncaa") && h.contains("tournament") {
                return tag("tournament", "March Madness");
            }
        }
        _ => {}
    }
    if postseason {
        return tag("playoffs", "Playoffs");
    }
    None
}

fn map_status(name: &str) -> Option<GameStatus> {
    Some(match name {
        "STATUS_SCHEDULED" => GameStatus::Pregame,
        "STATUS_IN_PROGRESS" => GameStatus::Active,
        "STATUS_HALFTIME" | "STATUS_END_PERIOD" | "STATUS_DELAYED" | "STATUS_SUSPENDED" => {
            GameStatus::Intermission
        }
        "STATUS_FINAL" | "STATUS_PLAY_COMPLETE" => GameStatus::Final,
        "STATUS_POSTPONED" | "STATUS_CANCELED" => return None,
        _ => GameStatus::Pregame,
    })
}

fn ordinal(n: i64) -> String {
    let suffix = match (n % 100, n % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

fn ordinal_for(status: &Status, gs: GameStatus) -> String {
    if status.type_.name == "STATUS_HALFTIME" {
        return "HALF".into();
    }
    if status.period <= 0 {
        return String::new();
    }
    let o = ordinal(status.period);
    if gs == GameStatus::Intermission {
        format!("{o} INT")
    } else {
        o
    }
}

#[cfg(test)]
mod leader_tests {
    use super::*;

    /// An F1 weekend lists practice/quali (finished) before the race. We must show
    /// the in-progress race, not the first (finished) session.
    #[test]
    fn f1_picks_in_progress_race_not_finished_practice() {
        let json = r#"{
            "id": "evt1",
            "shortName": "Lenovo Austrian GP",
            "competitions": [
                {"date":"2026-06-26T11:30Z","status":{"type":{"name":"STATUS_FINAL"}},
                 "competitors":[{"order":1,"athlete":{"displayName":"Practice Driver"}}]},
                {"date":"2026-06-28T13:00Z","status":{"type":{"name":"STATUS_IN_PROGRESS"}},
                 "competitors":[
                    {"order":1,"athlete":{"displayName":"Max Verstappen"}},
                    {"order":2,"athlete":{"displayName":"Lando Norris"}}
                 ]}
            ]
        }"#;
        let ev: LeaderEvent = serde_json::from_str(json).expect("deserialize event");
        let g = parse_f1(ev).expect("parse_f1 should yield a game");
        assert_eq!(g.status, GameStatus::Active, "should pick the in-progress race");
        assert_eq!(g.sport_id, "f1");
        assert_eq!(g.away.abbrev, "F1");
        let lb = g.leaderboard.expect("leaderboard present");
        assert_eq!(
            lb.rows.first().map(|r| r.name.as_str()),
            Some("VERSTAPPEN"),
            "leaderboard rows should come from the race, not the practice session"
        );
    }

    /// When nothing is live, fall back to the soonest upcoming session.
    #[test]
    fn f1_falls_back_to_next_upcoming_when_none_live() {
        let json = r#"{
            "id": "evt2",
            "shortName": "Test GP",
            "competitions": [
                {"date":"2026-06-26T11:30Z","status":{"type":{"name":"STATUS_FINAL"}},
                 "competitors":[{"order":1,"athlete":{"displayName":"Past Driver"}}]},
                {"date":"2026-06-28T13:00Z","status":{"type":{"name":"STATUS_SCHEDULED"}},
                 "competitors":[{"order":1,"athlete":{"displayName":"Future Driver"}}]}
            ]
        }"#;
        let ev: LeaderEvent = serde_json::from_str(json).expect("deserialize event");
        let g = parse_f1(ev).expect("parse_f1 should yield a game");
        assert_eq!(g.status, GameStatus::Pregame, "should pick the upcoming session");
        assert_eq!(
            g.leaderboard.unwrap().rows.first().map(|r| r.name.clone()),
            Some("DRIVER".to_string()),
            "should come from the upcoming session"
        );
    }
}

#[cfg(test)]
mod champ_tests {
    use super::*;

    fn season(type_: i64) -> EventSeason {
        EventSeason { type_, slug: String::new() }
    }

    #[test]
    fn regular_season_game_is_not_champ() {
        assert_eq!(classify_champ("nhl", Some(&season(2)), ""), None);
        assert_eq!(classify_champ("nhl", None, ""), None);
    }

    #[test]
    fn nhl_headline_refines_tier() {
        let cup = classify_champ("nhl", Some(&season(3)), "Stanley Cup Final - Game 3").unwrap();
        assert_eq!((cup.tier.as_str(), cup.label.as_str()), ("final", "Stanley Cup Final"));

        let conf = classify_champ("nhl", Some(&season(3)), "West Conference Finals - Game 5").unwrap();
        assert_eq!((conf.tier.as_str(), conf.label.as_str()), ("conf", "Conference Finals"));

        // Postseason with an unrecognized round headline -> broadest tier.
        let rd1 = classify_champ("nhl", Some(&season(3)), "1st Round - Game 2").unwrap();
        assert_eq!((rd1.tier.as_str(), rd1.label.as_str()), ("playoffs", "Playoffs"));
    }

    #[test]
    fn headline_beats_missing_season_flag() {
        // A cup-final headline is decisive even if the season block is absent.
        let cup = classify_champ("nhl", None, "Stanley Cup Final - Game 1").unwrap();
        assert_eq!(cup.tier, "final");
    }

    #[test]
    fn other_sports_top_tiers() {
        assert_eq!(classify_champ("nba", Some(&season(3)), "NBA Finals - Game 7").unwrap().tier, "final");
        assert_eq!(classify_champ("mlb", Some(&season(3)), "World Series - Game 4").unwrap().tier, "ws");
        assert_eq!(classify_champ("nfl", Some(&season(3)), "Super Bowl LXI").unwrap().tier, "sb");
        // Generic sport: postseason -> playoffs.
        assert_eq!(classify_champ("wnba", Some(&season(3)), "").unwrap().tier, "playoffs");
    }

    #[test]
    fn parse_event_attaches_champ() {
        let json = r#"{
            "season": {"type": 3, "slug": "post-season"},
            "competitions": [{
                "id": "401",
                "date": "2026-06-10T00:00Z",
                "notes": [{"headline": "Stanley Cup Final - Game 2"}],
                "status": {"type": {"name": "STATUS_IN_PROGRESS"}, "period": 2, "displayClock": "12:34"},
                "competitors": [
                    {"homeAway": "home", "score": "1", "team": {"abbreviation": "EDM", "shortDisplayName": "Oilers", "color": "041E42", "alternateColor": "FF4C00"}},
                    {"homeAway": "away", "score": "2", "team": {"abbreviation": "FLA", "shortDisplayName": "Panthers", "color": "C8102E", "alternateColor": "B9975B"}}
                ]
            }]
        }"#;
        let ev: Event = serde_json::from_str(json).expect("deserialize event");
        let g = parse_event("nhl", ev).expect("parse_event should yield a game");
        let champ = g.champ.expect("champ should be classified");
        assert_eq!(champ.tier, "final");
        assert_eq!(champ.label, "Stanley Cup Final");
    }
}
