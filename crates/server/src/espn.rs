//! Sports data behind a swappable `SportsDataSource` trait. `EspnSource` pulls
//! live games from ESPN's public scoreboard API (ported from the original
//! Python backend); `MockSource` provides offline fallback games.

use crate::model::{ensure_contrast, hex_to_rgb, Champ, Game, GameStatus, Leaderboard, LeaderRow, Pregame, Situation, TeamSide};
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

/// How this server identifies itself to ESPN. Honest by default — it names the
/// software, not a browser — and overridable so a fork can point at its own
/// project instead.
fn espn_user_agent() -> String {
    std::env::var("ESPN_USER_AGENT").unwrap_or_else(|_| {
        format!("scoreboard/{} (+https://github.com)", env!("CARGO_PKG_VERSION"))
    })
}

struct League {
    sport_id: &'static str,
    sport: &'static str,
    league: &'static str,
    games_query: &'static str, // appended to the scoreboard URL (e.g. "?groups=50")
}

/// Display name and sport family for every league this binary knows:
/// `(sport_id, name, family, family rank)`.
///
/// The single owner of both strings. `League` above carries only ESPN routing —
/// its `sport` field ("hockey", "basketball") is a URL path segment, not a
/// label — and `with_binary_owned` stamps the name and family from here onto
/// whichever catalog we ended up serving. That matters because the disk cache
/// is a normal path, not an edge case: a rename would otherwise keep shipping
/// last month's league names for as long as ESPN stayed unreachable.
///
/// The names deliberately DON'T repeat their family, because the app renders
/// them grouped under it — "Men's Olympic" under Hockey, not "Olympic Hockey
/// (M)" under Hockey. That also means three leagues share the name "Men's
/// Olympic" and two share "NCAA": unique within a section, which is the only
/// place they are ever listed together.
///
/// The rank orders leagues WITHIN a sport as well as the sports themselves —
/// the app sorts on it before falling back to the name, and ranks a section by
/// its best-ranked league. Alphabetical order alone put "Men's Olympic" above
/// "NHL" in Hockey, which reads like a bug: a tournament that runs for two
/// weeks every four years should not outrank the league that runs all winter.
/// So the everyday league leads each sport, then the other professional ones,
/// then college, then the Olympics.
///
/// Ranks start at 1. Zero is what serde hands an old server's catalog, and the
/// app sorts that last rather than first. Gaps are deliberate — a fork can slot
/// a league in without renumbering.
pub(crate) const LEAGUE_IDENTITY: &[(&str, &str, &str, u32)] = &[
    ("nhl", "NHL", "Hockey", 10),
    ("olyhm", "Men's Olympic", "Hockey", 15),
    ("olyhw", "Women's Olympic", "Hockey", 16),
    ("nba", "NBA", "Basketball", 20),
    ("wnba", "WNBA", "Basketball", 21),
    ("cbb", "NCAAM", "Basketball", 22),
    ("wcbb", "NCAAW", "Basketball", 23),
    ("olybm", "Men's Olympic", "Basketball", 25),
    ("olybw", "Women's Olympic", "Basketball", 26),
    ("mlb", "MLB", "Baseball", 30),
    ("cbase", "NCAA", "Baseball", 31),
    ("nfl", "NFL", "Football", 40),
    ("cfb", "NCAA", "Football", 41),
    ("mls", "MLS", "Soccer", 50),
    ("epl", "Premier League", "Soccer", 51),
    ("ucl", "UEFA Champions League", "Soccer", 52),
    ("wc", "Men's World Cup", "Soccer", 55),
    ("wwc", "Women's World Cup", "Soccer", 56),
    ("olysm", "Men's Olympic", "Soccer", 57),
    ("olysw", "Women's Olympic", "Soccer", 58),
    ("pga", "PGA Tour", "Golf", 60),
    ("lpga", "LPGA Tour", "Golf", 61),
    ("f1", "Formula 1", "Motorsport", 70),
];

/// `(name, family, family rank)` for a league, or `None` for one this binary
/// has never heard of (a fork's). Unknown leagues keep whatever name the
/// catalog carried and get no family — the app buckets those on their own
/// rather than dropping them.
pub(crate) fn league_identity(sport_id: &str) -> Option<(&'static str, &'static str, u32)> {
    LEAGUE_IDENTITY.iter().find(|(id, ..)| *id == sport_id).map(|(_, n, c, r)| (*n, *c, *r))
}

/// Soccer, by the league registry rather than a hand-kept list of ids — a
/// competition added to `LEAGUE_IDENTITY` is covered without a second edit
/// somewhere else remembering to agree.
pub(crate) fn is_soccer(sport_id: &str) -> bool {
    league_identity(sport_id).is_some_and(|(_, family, _)| family == "Soccer")
}

/// The league display name, falling back to the id for a fork's league.
pub(crate) fn league_name(sport_id: &'static str) -> &'static str {
    league_identity(sport_id).map(|(n, _, _)| n).unwrap_or(sport_id)
}

pub struct EspnSource {
    client: reqwest::Client,
    leagues: Vec<League>,
    /// Final gaps for FINISHED F1 race sessions, keyed by competition id →
    /// (competitor id → gap). A finished race's gaps never change, and
    /// without this every poll would re-fan-out 20 requests for the rest of
    /// the weekend the event stays on ESPN's scoreboard.
    f1_gap_cache: tokio::sync::Mutex<std::collections::HashMap<String, std::collections::HashMap<String, F1Timing>>>,
}

impl EspnSource {
    pub fn new() -> Self {
        Self {
            f1_gap_cache: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            // A hard timeout so one hung upstream (ESPN or the NHL API) can't
            // stall the poll loop — a lost cycle beats a frozen slate.
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                // ESPN's edge refuses requests with NO User-Agent — reqwest
                // sends none by default — and answers 403 with an HTML page.
                // `.json()` then fails as "error decoding response body", which
                // says nothing about the actual cause; the feeds went dark for
                // hours looking like a parser bug.
                //
                // The URL in the comment is not decoration: a bare token
                // ("scoreboard-server") and an unresolvable host are both still
                // refused. Override with ESPN_USER_AGENT if you fork this.
                .user_agent(espn_user_agent())
                .build()
                .expect("reqwest client"),
            leagues: vec![
                League { sport_id: "nhl", sport: "hockey", league: "nhl", games_query: "" },
                League { sport_id: "nba", sport: "basketball", league: "nba", games_query: "" },
                League { sport_id: "mlb", sport: "baseball", league: "mlb", games_query: "" },
                League { sport_id: "nfl", sport: "football", league: "nfl", games_query: "" },
                // Without `groups` ESPN serves only games with a Top 25 team, so a
                // favorite that is unranked never reached the slate (Oregon at
                // Illinois, 2026-10-24). 80 is all of FBS, as 50 is all of D-I below.
                League { sport_id: "cfb", sport: "football", league: "college-football", games_query: "?groups=80" },
                League { sport_id: "cbb", sport: "basketball", league: "mens-college-basketball", games_query: "?groups=50" },
                League { sport_id: "wcbb", sport: "basketball", league: "womens-college-basketball", games_query: "?groups=50" },
                League { sport_id: "wnba", sport: "basketball", league: "wnba", games_query: "" },
                // Tournaments that only exist some of the time. They cost one
                // request each per poll and sit dormant out of season — the
                // ingestion filter drops anything more than 24h out, so an
                // idle league contributes nothing to the slate.
                //
                // Every slug here was verified against ESPN before being added;
                // a wrong one is a warning line every poll forever.
                League { sport_id: "olyhm", sport: "hockey", league: "olympics-mens-ice-hockey", games_query: "" },
                League { sport_id: "olyhw", sport: "hockey", league: "olympics-womens-ice-hockey", games_query: "" },
                League { sport_id: "olybm", sport: "basketball", league: "mens-olympics-basketball", games_query: "" },
                League { sport_id: "olybw", sport: "basketball", league: "womens-olympics-basketball", games_query: "" },
                League { sport_id: "olysm", sport: "soccer", league: "fifa.olympics", games_query: "" },
                League { sport_id: "olysw", sport: "soccer", league: "fifa.w.olympics", games_query: "" },
                League { sport_id: "wwc", sport: "soccer", league: "fifa.wwc", games_query: "" },
                League { sport_id: "ucl", sport: "soccer", league: "uefa.champions", games_query: "" },
                League { sport_id: "cbase", sport: "baseball", league: "college-baseball", games_query: "" },
                League { sport_id: "mls", sport: "soccer", league: "usa.1", games_query: "" },
                League { sport_id: "epl", sport: "soccer", league: "eng.1", games_query: "" },
                // The World Cup is its own ESPN "league" of national teams;
                // favoriting a country works exactly like favoriting a club.
                League { sport_id: "wc", sport: "soccer", league: "fifa.world", games_query: "" },
            ],
        }
    }
}

/// Deep link to ESPN's full human-readable game page for a game (the app's
/// "View on ESPN" button). `None` unless the id is a bare numeric ESPN event
/// id (excludes demo games like "demo-nhl" and F1's "{id}-{session}") in a
/// sport that HAS a game page. Team sports use `/game/`; soccer uses
/// `/soccer/match/`; leaderboard sports (golf/F1) have no per-game page.
/// URL formats verified live against espn.com.
pub fn espn_game_url(sport_id: &str, game_id: &str) -> Option<String> {
    // ESPN event ids are numeric; anything else (demo ids, F1 session suffixes)
    // can't form a valid page.
    // F1 games are one-per-session ("{event}-race"); the results page keys on
    // the bare event id, so strip the suffix before the all-digits check.
    let game_id = match game_id.split_once('-') {
        Some((base, _)) if sport_id == "f1" => base,
        _ => game_id,
    };
    if game_id.is_empty() || !game_id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Leaderboard sports have pages too — they just aren't /gameId/ pages.
    // (Verified live: golf 200s on the tournamentId query form, F1 on the
    // /racing/results raceId form.)
    match sport_id {
        "pga" | "lpga" => {
            return Some(format!(
                "https://www.espn.com/golf/leaderboard?tournamentId={game_id}"
            ))
        }
        "f1" => return Some(format!("https://www.espn.com/racing/results/_/raceId/{game_id}")),
        _ => {}
    }
    // (web path segment, page kind) per sport. Web paths differ from the API
    // league slugs — soccer collapses usa.1/eng.1/fifa.world to "soccer/match".
    let (path, kind) = match sport_id {
        "nhl" | "nba" | "mlb" | "nfl" | "wnba" => (sport_id, "game"),
        "cfb" => ("college-football", "game"),
        "cbb" => ("mens-college-basketball", "game"),
        "wcbb" => ("womens-college-basketball", "game"),
        "mls" | "epl" | "wc" => ("soccer", "match"),
        _ => return None,
    };
    Some(format!("https://www.espn.com/{path}/{kind}/_/gameId/{game_id}"))
}

/// Every raw feed the debug TUI can fetch/inspect: the team-league
/// scoreboards, the golf tours, F1, and the NHL situation aux feed.
pub(crate) const FEEDS: &[(&str, &str)] = &[
    ("nhl", "NHL"),
    ("nba", "NBA"),
    ("mlb", "MLB"),
    ("nfl", "NFL"),
    ("cfb", "NCAA Football"),
    ("cbb", "NCAAM"),
    ("wcbb", "NCAAW"),
    ("wnba", "WNBA"),
    ("mls", "MLS"),
    ("epl", "Premier League"),
    ("wc", "Men's World Cup"),
    ("pga", "PGA Tour"),
    ("lpga", "LPGA"),
    ("f1", "Formula 1"),
    ("nhl-aux", "NHL situations (api-web.nhle.com)"),
    ("demo", "Demo slate (generated)"),
];

impl EspnSource {
    /// The URL behind a feed id (None for "demo", which is generated).
    pub(crate) fn feed_url(&self, feed: &str) -> Option<String> {
        match feed {
            "pga" | "lpga" => Some(format!(
                "https://site.api.espn.com/apis/site/v2/sports/golf/leaderboard?league={feed}"
            )),
            "f1" => Some("https://site.api.espn.com/apis/site/v2/sports/racing/f1/scoreboard".into()),
            "nhl-aux" => Some("https://api-web.nhle.com/v1/score/now".into()),
            _ => self.leagues.iter().find(|l| l.sport_id == feed).map(|lg| {
                format!(
                    "https://site.api.espn.com/apis/site/v2/sports/{}/{}/scoreboard{}",
                    lg.sport, lg.league, lg.games_query
                )
            }),
        }
    }

    /// Fetch one feed's raw JSON body (the debug tool's capture path).
    pub(crate) async fn fetch_feed_raw(&self, feed: &str) -> Result<String, String> {
        let url = self.feed_url(feed).ok_or_else(|| format!("unknown feed '{feed}'"))?;
        let resp = self.client.get(&url).send().await.map_err(|e| e.to_string())?;
        resp.text().await.map_err(|e| e.to_string())
    }
}

/// Parse one feed's raw JSON with EXACTLY the server's parse functions —
/// what the TUI shows as "processed" is what the poll loop would produce.
pub(crate) fn parse_feed(feed: &str, raw: &str) -> Result<Vec<Game>, String> {
    match feed {
        "demo" => Ok(demo_games(0, 0)),
        "nhl-aux" => Err("nhl-aux is a situation overlay, not a game feed".into()),
        "pga" | "lpga" => golf_games(feed, raw),
        "f1" => {
            let r: LeaderboardResponse = serde_json::from_str(raw).map_err(|e| e.to_string())?;
            // The debug/TUI path shows the scoreboard feed as-is; the
            // per-driver gap fan-out belongs to the live poll only.
            let no_timing = std::collections::HashMap::new();
            Ok(r.events.into_iter().flat_map(|ev| parse_f1(ev, &no_timing)).collect())
        }
        _ => {
            let sport_id = FEEDS
                .iter()
                .find(|(id, _)| *id == feed)
                .map(|(id, _)| *id)
                .ok_or_else(|| format!("unknown feed '{feed}'"))?;
            let sb: Scoreboard = serde_json::from_str(raw).map_err(|e| e.to_string())?;
            Ok(sb.events.into_iter().filter_map(|ev| parse_event(sport_id, ev)).collect())
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
                Ok(resp) if !resp.status().is_success() => {
                    tracing::warn!("espn {}: HTTP {}", lg.league, resp.status());
                }
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
        self.attach_nhl_situations(&mut out).await;
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
                            // ESPN's Olympic hockey rosters shout two of their
                            // twelve country names and none of the others.
                            let name = crate::teams::unshout(&name);
                            // ESPN gives "" when a color is absent — normalize to None.
                            let hex = |s: String| (!s.is_empty()).then_some(s);
                            // ...and gives none at all for Olympic hockey, so
                            // those countries fall back to their flag.
                            let flag = crate::teams::flag_colors(&abbrev);
                            Some(Team {
                                id: crate::team_key(lg.sport_id, &abbrev),
                                league_id: lg.sport_id.into(),
                                name,
                                color: hex(t.color).or_else(|| flag.map(|(c, _)| c.to_string())),
                                alt_color: hex(t.alternate_color)
                                    .or_else(|| flag.map(|(_, c)| c.to_string())),
                                abbrev,
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
                data_sources: Vec::new(),
                id: lg.sport_id.into(),
                // Left empty, like the lists below: `with_binary_owned` stamps
                // the name and family from LEAGUE_IDENTITY at startup, so none
                // of them can depend on whether the catalog came from here or
                // from the disk cache.
                name: String::new(),
                teams,
                championship_options: Vec::new(),
                championship_rounds: Vec::new(),
                category: String::new(),
                category_rank: 0,
                extras_label: String::new(),
            });
        }
        if sports.is_empty() {
            return None; // ESPN team fetch unavailable -> caller falls back to mock_catalog
        }
        // Leaderboard sports (PGA / LPGA / F1) aren't ESPN team lists — add them.
        sports.extend(crate::leaderboard_sports());
        // Left empty on purpose: `with_off_modes` fills this in at startup, so the
        // value cannot depend on whether the catalog came from here or from the
        // disk cache.
        Some(Catalog { sports, off_modes: Vec::new() })
    }
}

/// One team's raw name fields for the `server team-names` overflow report:
/// `short_display_name` (fed to `teams::short_name` for the Name style),
/// `abbrev`, and `location` (the Location style's source). Kept separate from
/// the protocol `Team` (which carries no location) so the report can measure
/// every style without a protocol change.
pub(crate) struct CatalogTeamRaw {
    /// ESPN `name` (mascot) — the `Name` style's source in the report.
    pub mascot: String,
    pub abbrev: String,
    pub location: String,
}

impl EspnSource {
    /// Fetch every TEAM-sport league's roster with the raw fields the overflow
    /// report needs. Returns `(sport_id, display name, teams)` per league in the
    /// configured order; a league whose fetch/parse fails yields an empty list
    /// (the report notes it) rather than aborting the whole run.
    pub(crate) async fn fetch_team_catalog_raw(&self) -> Vec<(&'static str, &'static str, Vec<CatalogTeamRaw>)> {
        let mut out = Vec::new();
        for lg in &self.leagues {
            let url = format!(
                "https://site.api.espn.com/apis/site/v2/sports/{}/{}/teams?limit=500",
                lg.sport, lg.league
            );
            let teams: Vec<CatalogTeamRaw> = match self.client.get(&url).send().await {
                Ok(resp) => match resp.json::<TeamsResponse>().await {
                    Ok(tr) => tr
                        .sports
                        .into_iter()
                        .flat_map(|s| s.leagues)
                        .flat_map(|l| l.teams)
                        .filter_map(|w| {
                            let t = w.team;
                            if t.abbreviation.is_empty() {
                                return None;
                            }
                            let mascot = if t.name.is_empty() { t.short_display_name } else { t.name };
                            Some(CatalogTeamRaw {
                                mascot,
                                abbrev: t.abbreviation,
                                location: t.location,
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
            out.push((lg.sport_id, league_name(lg.sport_id), teams));
        }
        out
    }
}

// ---- golf + F1 (leaderboard sports) ----
impl EspnSource {
    async fn fetch_golf(&self) -> Vec<Game> {
        let mut out = Vec::new();
        // PGA and LPGA are separate leagues in the catalog ("pga"/"lpga"), each
        // subscribable to all events or majors only.
        for tour in ["pga", "lpga"] {
            let url =
                format!("https://site.api.espn.com/apis/site/v2/sports/golf/leaderboard?league={tour}");
            let body = match self.client.get(&url).send().await {
                Ok(r) => match r.text().await {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!("golf fetch {tour}: {e}");
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!("golf fetch {tour}: {e}");
                    continue;
                }
            };
            match golf_games(tour, &body) {
                Ok(games) => out.extend(games),
                Err(e) => tracing::warn!("golf parse {tour}: {e}"),
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
        let mut out = Vec::new();
        for ev in events {
            let timing = self.f1_timing_for_event(&ev).await;
            out.extend(parse_f1(ev, &timing));
        }
        out
    }

    /// Live timing for an event's sessions, keyed (competition id,
    /// competitor id).
    ///
    /// The scoreboard inlines an empty `statistics` for every session, but
    /// the same data exists per-competitor in ESPN's core API — one request
    /// per driver, which is why this is bounded to the rows the board can
    /// draw and to sessions where the numbers mean something.
    ///
    /// Qualifying is the interesting case. ESPN resets `gapToLeader` per
    /// Q1/Q2/Q3 segment while the scoreboard `order` spans the whole hour
    /// (and lags by a minute), so joined naively its own site shows P8
    /// nearer than P2 mid-session. The SAME resource also carries the
    /// timing-screen `position`, which is what the gaps are relative to,
    /// and the per-segment lap times ESPN fills in as drivers drop out —
    /// together they are enough to draw a coherent tower (see `parse_f1`).
    ///
    /// Failures degrade to the blank value column this render always had.
    async fn f1_timing_for_event(
        &self,
        ev: &LeaderEvent,
    ) -> std::collections::HashMap<(String, String), F1Timing> {
        let mut out = std::collections::HashMap::new();
        for comp in &ev.competitions {
            if !f1_session_wants_timing(comp) {
                continue;
            }
            let done = matches!(map_status(&comp.status.type_), Some(GameStatus::Final));
            // A finished session's numbers are frozen — serve the cache, fetch once.
            if done {
                if let Some(cached) = self.f1_gap_cache.lock().await.get(&comp.id) {
                    for (cid, t) in cached {
                        out.insert((comp.id.clone(), cid.clone()), t.clone());
                    }
                    continue;
                }
            }
            // Top rows only, in running order — the panel never draws more.
            let mut ids: Vec<(i64, String)> = comp
                .competitors
                .iter()
                .filter(|c| !c.id.is_empty())
                .map(|c| (c.order, c.id.clone()))
                .collect();
            ids.sort();
            ids.truncate(crate::render::LEADERBOARD_MAX_ROWS);

            let fetched = futures_util::future::join_all(ids.into_iter().map(|(_, cid)| {
                let url = format!(
                    "https://sports.core.api.espn.com/v2/sports/racing/leagues/f1/events/{}/competitions/{}/competitors/{}/statistics?lang=en&region=us",
                    ev.id, comp.id, cid
                );
                let client = self.client.clone();
                async move {
                    let stats: CoreStatistics = client.get(&url).send().await.ok()?.json().await.ok()?;
                    Some((cid, f1_timing(&stats)))
                }
            }))
            .await;

            let mut session: std::collections::HashMap<String, F1Timing> = std::collections::HashMap::new();
            for (cid, t) in fetched.into_iter().flatten() {
                session.insert(cid, t);
            }
            if !session.is_empty() {
                tracing::info!("f1 timing: {} rows for session {}", session.len(), comp.id);
                for (cid, t) in &session {
                    out.insert((comp.id.clone(), cid.clone()), t.clone());
                }
                // Cache live results too: if the one poll that sees Final
                // hits a network blip, the session still keeps its last numbers.
                self.f1_gap_cache.lock().await.insert(comp.id.clone(), session);
            }
        }
        out
    }
}

/// Which sessions earn the per-driver timing fan-out: every session the
/// board can show (races, sprints, qualifying, sprint qualifying), live or
/// finished. Practice never, and pregame never (there is nothing to fetch
/// yet).
fn f1_session_wants_timing(comp: &LeaderComp) -> bool {
    let shown = matches!(
        comp.type_.as_ref().map(|t| t.abbreviation.as_str()),
        Some("Race") | Some("SR") | Some("Qual") | Some("SS")
    );
    shown
        && !comp.id.is_empty()
        && matches!(
            map_status(&comp.status.type_),
            Some(GameStatus::Active | GameStatus::Intermission | GameStatus::Final)
        )
}

/// One driver's slice of the core statistics resource, as the render needs it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct F1Timing {
    /// "+0.29" — gap to the leader of the CURRENT segment (quali) or the
    /// race; None for the leader and for a driver ESPN has nothing on.
    pub gap: Option<String>,
    /// The timing-screen position. Observed live (2026 Spanish GP quali) to
    /// lead the scoreboard `order` by about a minute and to be the ordering
    /// `gap` is relative to.
    pub position: Option<i64>,
    /// Per-segment qualifying lap times ("1:35.312"), indexed Q1..Q3. ESPN
    /// fills a driver's time for the segment they were eliminated in, and
    /// only after the segment ends — so "any Q1 time in the field" reads as
    /// "Q1 is over". Empty for a segment the driver never ran or that is
    /// still running.
    pub qual_time: [Option<String>; 3],
}

/// The core API statistics resource: categories of name/displayValue stats.
#[derive(Deserialize)]
struct CoreStatistics {
    #[serde(default)]
    splits: Option<CoreSplits>,
}
#[derive(Deserialize)]
struct CoreSplits {
    #[serde(default)]
    categories: Vec<CoreCategory>,
}
#[derive(Deserialize)]
struct CoreCategory {
    #[serde(default)]
    stats: Vec<CoreStat>,
}
#[derive(Deserialize)]
struct CoreStat {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "displayValue")]
    display_value: String,
}

/// Rewrite gap-to-leader values as intervals to the car ahead, in place.
/// "+d.ddd" strings only; anything else ("+2L", "DNF") is left alone and
/// skipped by the subtraction. Hundredths — thousandths read as noise at
/// four pixels per digit.
fn derive_intervals(rows: &mut [LeaderRow]) {
    let mut prev = 0.0_f64; // the leader's gap to itself
    for row in rows.iter_mut() {
        let Some(g) = row.value.strip_prefix('+').and_then(|v| v.parse::<f64>().ok()) else {
            continue;
        };
        row.value = format!("+{:.2}", (g - prev).max(0.0));
        prev = g;
    }
}

/// Everything the tower needs from one driver's statistics resource.
fn f1_timing(stats: &CoreStatistics) -> F1Timing {
    let mut t = F1Timing { gap: extract_gap(stats), ..Default::default() };
    let stat = |name: &str| -> Option<String> {
        let categories = &stats.splits.as_ref()?.categories;
        categories
            .iter()
            .flat_map(|c| c.stats.iter())
            .find(|s| s.name == name)
            .map(|s| s.display_value.clone())
            .filter(|v| !v.is_empty())
    };
    t.position = stat("position").and_then(|v| v.parse().ok()).filter(|p| *p > 0);
    for (i, name) in ["qual1TimeMS", "qual2TimeMS", "qual3TimeMS"].into_iter().enumerate() {
        // A segment not run (or not over) reads "0.000".
        t.qual_time[i] = stat(name).filter(|v| v != "0.000" && v != "0");
    }
    t
}

/// The gap the timing tower shows. Field names verified live during the 2026
/// Dutch GP: qualifying carries `gapToLeader`; the race is expected to carry
/// the same or `behindTime`/`behindLaps` (ESPN's older racing vocabulary), so
/// all three are accepted in that order. The leader carries none and keeps a
/// blank column. "+1 Lap" style values are tightened to "+1L" for the panel.
fn extract_gap(stats: &CoreStatistics) -> Option<String> {
    let categories = &stats.splits.as_ref()?.categories;
    for wanted in ["gapToLeader", "behindTime", "behindLaps"] {
        for cat in categories {
            for st in &cat.stats {
                if st.name == wanted && !st.display_value.is_empty() && st.display_value != "0" && st.display_value != "0.000" {
                    let v = st.display_value.replace(" Laps", "L").replace(" Lap", "L");
                    // Hundredths: "+1.804" -> "+1.80". Non-numeric strings
                    // ("+2L") pass through.
                    if let Some(n) = v.strip_prefix('+').and_then(|x| x.parse::<f64>().ok()) {
                        return Some(format!("+{n:.2}"));
                    }
                    return Some(v);
                }
            }
        }
    }
    None
}

/// A qualifying lap time as the panel draws it: "1:35.312" -> "1:35.31",
/// hundredths like every other timing on the board. Anything that is not
/// m:ss.fff passes through untouched.
pub(crate) fn fit_lap_time(t: &str) -> String {
    let Some((m, sec)) = t.split_once(':') else { return t.to_string() };
    match (m.parse::<u32>(), sec.parse::<f64>()) {
        (Ok(m), Ok(s)) => format!("{m}:{s:05.2}"),
        _ => t.to_string(),
    }
}

/// A leaderboard "event" (especially an F1 race weekend) can carry several
/// competitions — practice, qualifying, sprint, the race. ESPN lists them in
/// chronological order, so `.next()` grabs the *first* (often a finished practice
/// session) rather than what's actually happening. Pick the most relevant: one in
/// progress, else the soonest upcoming, else the most recent finished.
fn pick_leader_comp(comps: Vec<LeaderComp>) -> Option<LeaderComp> {
    fn category(t: &StatusType) -> u8 {
        match map_status(t) {
            Some(GameStatus::Active) | Some(GameStatus::Intermission) => 2, // live
            Some(GameStatus::Pregame) => 1,                                  // upcoming
            _ => 0,                                                          // finished/unknown
        }
    }
    comps.into_iter().max_by(|a, b| {
        let (ca, cb) = (category(&a.status.type_), category(&b.status.type_));
        if ca != cb {
            ca.cmp(&cb) // live > upcoming > finished
        } else if ca == 1 {
            b.date.cmp(&a.date) // upcoming: soonest (earliest date) wins
        } else {
            a.date.cmp(&b.date) // live/finished: latest date wins
        }
    })
}

/// The tours' majors, matched against ESPN event names (lowercased).
fn golf_major_label(sport_id: &str, event_name: &str) -> Option<&'static str> {
    let n = event_name.to_lowercase();
    let majors: &[(&str, &str)] = match sport_id {
        "pga" => &[
            ("masters", "The Masters"),
            ("pga championship", "PGA Championship"),
            ("u.s. open", "U.S. Open"),
            ("us open", "U.S. Open"),
            ("open championship", "The Open"),
        ],
        "lpga" => &[
            ("chevron", "Chevron Championship"),
            ("u.s. women's open", "U.S. Women's Open"),
            ("us women's open", "U.S. Women's Open"),
            ("women's pga", "Women's PGA"),
            ("kpmg", "Women's PGA"),
            ("evian", "Evian Championship"),
            ("women's open", "Women's Open"),
            ("aig", "Women's Open"),
        ],
        _ => &[],
    };
    majors.iter().find(|(key, _)| n.contains(key)).map(|(_, label)| *label)
}

fn parse_golf(sport_id: &str, ev: LeaderEvent) -> Option<Game> {
    let comp = pick_leader_comp(ev.competitions)?;
    // `?`, not a Pregame fallback: a tournament that is suspended for the day
    // (or canceled) has no business on the board until the feed says play is on
    // again. Falling back to Pregame would not help — the tee times we'd date it
    // by are the ones already played.
    let status = map_status(&comp.status.type_)?;
    // The whole field by current position (ties allowed), name = last name,
    // score = "+5"/"E". This used to cut at position 4 — a leftover from when
    // the board only ever drew four static rows — which silently capped every
    // deep-field setting: the `take` below could never reach 20 because the
    // filter had already thrown the rest away. It also made the row count
    // JUMP AROUND, because position is a RANK and ties share one: four
    // players at positions 1,2,3,3 gave four rows, while a five-way tie for
    // third gave seven, with nothing changed in the app.
    // Both read before the field is consumed below.
    let tee_off = first_tee_time(&comp);
    let round = golf_round(&comp);
    let mut ranked: Vec<(i64, LeaderRow)> = comp
        .competitors
        .into_iter()
        .filter_map(|c| {
            let pos = c.status.as_ref()?.position.as_ref()?.id;
            // pos <= 0 is "not started / withdrawn", which stays filtered.
            if pos <= 0 {
                return None;
            }
            let name = golfer_last_name(&c.athlete.as_ref()?.display_name);
            let score = c
                .statistics
                .first()
                .map(|s| s.display_value.clone())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "E".into());
            // Holes played, but only while that is news. A finished round
            // reports 18 and every row would read "18" — a column of noise
            // charged against the golfer's name. Mid-round it is the one thing
            // the leaderboard cannot otherwise tell you: -4 through 6 and -4
            // after 18 are not the same afternoon.
            let thru = c
                .status
                .as_ref()
                .and_then(|s| s.thru)
                .filter(|t| (1..=17).contains(t))
                .map(|t| t.to_string());
            Some((pos, LeaderRow { name, value: score, thru, ..Default::default() }))
        })
        .collect();
    ranked.sort_by_key(|(p, _)| *p);
    // Keep enough of the field for the board's scrolling frame (it shows 4
    // at a time and glides through the rest).
    let rows = ranked.into_iter().take(crate::render::LEADERBOARD_MAX_ROWS).map(|(_, r)| r).collect();
    let name = clean_golf_name(&ev.short_name);
    let champ = golf_major_label(sport_id, &ev.short_name)
        .map(|label| Champ { tier: "major".into(), label: label.into() });
    Some(Game {
        id: ev.id,
        sport_id: sport_id.into(),
        away: TeamSide::empty(&sport_id.to_uppercase()),
        home: TeamSide::empty(""),
        status,
        // Golf's "period" IS the tournament round, so it belongs in the field
        // every other sport keeps its period in. Without it a golf card could
        // not say which of four days you were looking at — the leaderboard
        // alone looks the same on Thursday as on Sunday.
        ordinal: round,
        clock: String::new(),
        // The FIRST tee time, not the event date — see `LeaderCompStatus::tee_time`.
        // Falls back to the event date when the field has no tee times yet
        // (ESPN publishes them a day or two out), which is no worse than before.
        start_time: tee_off.unwrap_or(comp.date),
        // Tournaments run for days — surface when this one wraps up.
        end_time: (!ev.end_date.is_empty()).then(|| ev.end_date.clone()),
        leaderboard: Some(Leaderboard { matchplay: None, laps: None, name, flag: None, stage: None, quali: None, rows }),
        champ,
        series: None,
        session: None,
        situation: None,
        pregame: None,
    })
}

/// The tournament round as a board-sized label ("R3"), empty when the feed has
/// no round yet (a tournament that hasn't started reports period 0).
///
/// Rounds beyond the fourth exist — a Monday finish after a weather week still
/// reports its own period — so this formats whatever the feed says rather than
/// assuming the usual four.
fn golf_round(comp: &LeaderComp) -> String {
    match comp.status.period {
        n if n >= 1 => format!("R{n}"),
        _ => String::new(),
    }
}

/// The earliest tee time in the field, or None when ESPN has not published them.
///
/// Golf's event `date` is a DATE, not a time — midnight local on day one, with
/// `timeValid: false` alongside it. The board's half-hour pre-game window
/// applied to that put a Thursday tournament on the panel at 10:30pm Wednesday.
///
/// Racing needs no equivalent: F1 competitions carry a real per-session `date`
/// with `timeValid: true`, which `f1_session_game` already uses.
///
/// This is a per-ROUND start, which is what makes a multi-day tournament work as
/// one card: on Saturday it returns Saturday's first tee, not Thursday's. Two
/// things keep it that way. ESPN only publishes `teeTime` for players still
/// active in the current round — a golfer who missed the cut has none, which is
/// why the field can report mixed `period`s while every tee time belongs to the
/// same round. And the round filter below makes that explicit rather than
/// incidental, so a feed that one day starts carrying finished rounds' times
/// can't drag the start back to a day already played.
fn first_tee_time(comp: &LeaderComp) -> Option<String> {
    let round = comp.status.period;
    comp.competitors
        .iter()
        .filter_map(|c| c.status.as_ref())
        // period 0 means the feed didn't say; take those rather than none.
        .filter(|s| round < 1 || s.period < 1 || s.period == round)
        .map(|s| s.tee_time.as_str())
        .filter(|t| !t.is_empty())
        // RFC3339 in a fixed UTC format, so lexical order IS chronological.
        .min()
        .map(str::to_string)
}

/// Replay one captured ESPN folder (`scoreboard.json` plus a
/// `stats-<competitor>.json` per driver, as `scripts/capture-f1.py` and the
/// earlier quali captures write them) through the live parser: the games the
/// server would have built from exactly those responses.
#[cfg(test)]
pub(crate) fn replay_f1_snapshot(dir: &std::path::Path) -> Vec<Game> {
    use std::collections::HashMap;
    let Ok(raw) = std::fs::read_to_string(dir.join("scoreboard.json")) else { return Vec::new() };
    let Ok(r) = serde_json::from_str::<LeaderboardResponse>(&raw) else { return Vec::new() };
    let mut timing = HashMap::new();
    for ev in &r.events {
        for comp in &ev.competitions {
            for c in &comp.competitors {
                let Ok(raw) = std::fs::read_to_string(dir.join(format!("stats-{}.json", c.id))) else { continue };
                let Ok(stats) = serde_json::from_str::<CoreStatistics>(&raw) else { continue };
                timing.insert((comp.id.clone(), c.id.clone()), f1_timing(&stats));
            }
        }
    }
    r.events.into_iter().flat_map(|ev| parse_f1(ev, &timing)).collect()
}

/// Every game in one golf leaderboard response: stroke-play events through
/// `parse_golf`, team match play through `parse_matchplay`. The poll and the
/// debug tools both come through here.
fn golf_games(tour: &str, raw: &str) -> Result<Vec<Game>, String> {
    let r: LeaderboardResponse = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    let mut games: Vec<Game> = r.events.into_iter().filter_map(|ev| parse_golf(tour, ev)).collect();
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        events: Vec<serde_json::Value>,
    }
    let raw: Raw = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    for v in raw.events.into_iter().filter(is_match_play) {
        match serde_json::from_value::<MpEvent>(v) {
            Ok(ev) => games.extend(parse_matchplay(tour, ev)),
            Err(e) => tracing::warn!("match play event skipped: {e}"),
        }
    }
    Ok(games)
}

/// Team match play (Presidents Cup, Ryder Cup). ESPN nests these events'
/// competitions BY SESSION — `[[cup], [Thursday's matches], …]` — where a
/// stroke-play event has one flat competition; that nesting is the tell.
fn is_match_play(ev: &serde_json::Value) -> bool {
    ev.get("competitions")
        .and_then(|c| c.as_array())
        .is_some_and(|c| c.iter().any(|x| x.is_array()))
}

#[derive(Deserialize)]
struct MpEvent {
    #[serde(default)]
    id: String,
    #[serde(default, rename = "shortName")]
    short_name: String,
    #[serde(default, rename = "endDate")]
    end_date: String,
    #[serde(default)]
    competitions: Vec<Vec<MpComp>>,
}
#[derive(Deserialize)]
struct MpComp {
    #[serde(default)]
    date: String,
    /// "Saturday Morning Four-Balls"; "tournament" on the cup itself.
    #[serde(default)]
    description: String,
    status: Status,
    /// "tournament" (the cup), "fourball", "foursome", "singles".
    #[serde(default, rename = "type")]
    kind: Option<MpType>,
    #[serde(default)]
    notes: Vec<MpNote>,
    #[serde(default)]
    competitors: Vec<MpCompetitor>,
}
#[derive(Deserialize)]
struct MpType {
    #[serde(default)]
    text: String,
}
#[derive(Deserialize)]
struct MpNote {
    #[serde(default)]
    text: String,
}
#[derive(Deserialize)]
struct MpCompetitor {
    team: Option<MpTeam>,
    /// An object here ({value, displayValue, winner}); the scoreboard
    /// endpoint sends a bare string for the same thing. Value takes both.
    #[serde(default)]
    score: serde_json::Value,
    status: Option<MpCompStatus>,
    /// A pair's players (Four-Balls, Foursomes).
    #[serde(default)]
    roster: Vec<MpRosterEntry>,
    /// A singles player: ESPN puts the one athlete on the competitor itself
    /// and leaves `roster` empty.
    athlete: Option<MpAthlete>,
}
#[derive(Deserialize)]
struct MpTeam {
    #[serde(default)]
    abbreviation: String,
}
#[derive(Deserialize)]
struct MpCompStatus {
    #[serde(default)]
    thru: Option<i64>,
    #[serde(default, rename = "teeTime")]
    tee_time: String,
}
#[derive(Deserialize)]
struct MpRosterEntry {
    athlete: Option<MpAthlete>,
}
#[derive(Deserialize)]
struct MpAthlete {
    #[serde(default, rename = "lastName")]
    last_name: String,
    #[serde(default, rename = "displayName")]
    display_name: String,
}

impl MpCompetitor {
    fn abbrev(&self) -> &str {
        self.team.as_ref().map(|t| t.abbreviation.as_str()).unwrap_or("")
    }
    fn score_text(&self) -> String {
        match &self.score {
            serde_json::Value::String(s) => s.clone(),
            v => v.get("displayValue").and_then(|d| d.as_str()).unwrap_or("").to_string(),
        }
    }
    fn score_value(&self) -> Option<f64> {
        self.score.get("value").and_then(|v| v.as_f64())
    }
    fn winner(&self) -> bool {
        self.score.get("winner").and_then(|v| v.as_bool()).unwrap_or(false)
    }
}

/// "1 Up" -> "1UP", "3 & 2" -> "3+2", "AS" stays: golf's notation, packed
/// for the panel. The numbers stay golf's (3 up with 2 to play); the "&"
/// becomes "+" because the 4x6 ampersand is a squiggle that reads as an 8
/// ("6&4" looked like "684").
fn match_notation(s: &str) -> String {
    s.to_uppercase().split_whitespace().collect::<Vec<_>>().join("").replace('&', "+")
}

/// The panel colour for a cup team. The feed carries none (both `color`
/// fields are null), so these are ours: USA red; the International and
/// European sides blue.
fn cup_team_color(abbrev: &str) -> embedded_graphics::pixelcolor::Rgb888 {
    match abbrev {
        "USA" => hex_to_rgb("E0283C"),
        "INTL" | "EUR" => hex_to_rgb("2F6FE0"),
        _ => hex_to_rgb("C0C0C0"),
    }
}

/// A team match-play event as one game: the cup score plus the matches of
/// ONE session — the live one; else the latest with results (between
/// sessions, and after the last, the scores just made are the news); else
/// the next one scheduled (before the first tee shot).
fn parse_matchplay(sport_id: &str, ev: MpEvent) -> Option<Game> {
    use crate::model::{CupTeam, Match, MatchPlay, MatchState};
    let is_cup = |c: &MpComp| c.kind.as_ref().is_some_and(|k| k.text == "tournament");
    let comps: Vec<&MpComp> = ev.competitions.iter().flatten().collect();
    let cup = comps.iter().copied().find(|c| is_cup(c))?;
    if cup.competitors.len() != 2 {
        return None;
    }
    let team = |i: usize| {
        let c = &cup.competitors[i];
        CupTeam {
            abbrev: c.abbrev().to_string(),
            points: c.score_value().unwrap_or(0.0) as f32,
            color: cup_team_color(c.abbrev()),
        }
    };
    let teams = [team(0), team(1)];
    let side = |abbrev: &str| teams.iter().position(|t| t.abbrev == abbrev);
    // "FIRST TEAM TO 15.5 POINTS WINS THE PRESIDENTS CUP"
    let target = cup.notes.iter().find_map(|n| {
        let up = n.text.to_uppercase();
        let rest = up.split(" TO ").nth(1)?;
        let num: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        (!num.is_empty()).then_some(num)
    });

    let sessions: Vec<&Vec<MpComp>> =
        ev.competitions.iter().filter(|g| !g.is_empty() && !g.iter().any(|c| is_cup(c))).collect();
    let state_of = |c: &MpComp| map_status(&c.status.type_);
    let live = |g: &&Vec<MpComp>| g.iter().any(|c| matches!(state_of(c), Some(GameStatus::Active | GameStatus::Intermission)));
    let played = |g: &&Vec<MpComp>| g.iter().any(|c| matches!(state_of(c), Some(GameStatus::Final)));
    let session: &Vec<MpComp> = sessions
        .iter()
        .copied()
        .find(live)
        .or_else(|| sessions.iter().rev().copied().find(played))
        .or_else(|| sessions.first().copied())?;

    let matches: Vec<Match> = session
        .iter()
        .filter_map(|m| {
            let mut players: [Vec<String>; 2] = [Vec::new(), Vec::new()];
            let mut by_side: [Option<&MpCompetitor>; 2] = [None, None];
            for c in &m.competitors {
                let i = side(c.abbrev())?;
                by_side[i] = Some(c);
                // A pair's roster, else the singles player on the competitor.
                let mut athletes: Vec<&MpAthlete> = c.roster.iter().filter_map(|r| r.athlete.as_ref()).collect();
                if athletes.is_empty() {
                    athletes.extend(c.athlete.as_ref());
                }
                let lasts: Vec<String> = athletes.iter().map(|a| a.last_name.to_uppercase()).collect();
                players[i] = athletes
                    .iter()
                    .zip(&lasts)
                    .map(|(a, last)| {
                        // Tom Kim and Si Woo Kim, partners: "T KIM" / "S KIM".
                        if lasts.iter().filter(|l| *l == last).count() > 1 {
                            let initial = a.display_name.chars().next().unwrap_or(' ');
                            format!("{initial} {last}")
                        } else {
                            last.clone()
                        }
                    })
                    .collect();
            }
            let [Some(a), Some(b)] = by_side else { return None };
            let thru = [a, b]
                .iter()
                .filter_map(|c| c.status.as_ref().and_then(|s| s.thru))
                .max()
                .unwrap_or(0)
                .max(0) as u32;
            let state = match state_of(m)? {
                GameStatus::Pregame => MatchState::Scheduled {
                    tee: [a, b]
                        .iter()
                        .filter_map(|c| c.status.as_ref())
                        .map(|s| s.tee_time.clone())
                        .find(|t| !t.is_empty())
                        .or_else(|| (!m.date.is_empty()).then(|| m.date.clone())),
                },
                GameStatus::Final => {
                    let winner = [a, b].iter().position(|c| c.winner());
                    let result = winner
                        .map(|w| match_notation(&[a, b][w].score_text()))
                        .filter(|r| !r.is_empty())
                        .unwrap_or_else(|| "HALVED".into());
                    MatchState::Final { winner, result }
                }
                _ => {
                    // The leader's side carries "2 Up"; the other is blank.
                    // "AS" on both is all square, and "-" before the first
                    // hole is finished reads the same way.
                    let leader = [a, b].iter().position(|c| match_notation(&c.score_text()).ends_with("UP"));
                    let margin = leader.map(|l| match_notation(&[a, b][l].score_text())).unwrap_or_else(|| "AS".into());
                    MatchState::Live { leader, margin, thru }
                }
            };
            Some(Match { players, state })
        })
        .collect();
    if matches.is_empty() {
        return None;
    }
    let status = map_status(&cup.status.type_)?;
    let format = session.first().and_then(|c| c.kind.as_ref()).map(|k| k.text.clone()).unwrap_or_default();
    let first_tee = matches
        .iter()
        .filter_map(|m| match &m.state {
            MatchState::Scheduled { tee } => tee.clone(),
            _ => None,
        })
        .min();
    Some(Game {
        id: ev.id,
        sport_id: sport_id.into(),
        away: TeamSide::empty(&sport_id.to_uppercase()),
        home: TeamSide::empty(""),
        status,
        ordinal: String::new(),
        clock: String::new(),
        start_time: first_tee.unwrap_or_else(|| cup.date.clone()),
        end_time: (!ev.end_date.is_empty()).then_some(ev.end_date),
        leaderboard: Some(Leaderboard {
            matchplay: Some(MatchPlay {
                teams,
                target,
                session: session.first().map(|c| c.description.clone()).unwrap_or_default(),
                format,
                matches,
            }),
            laps: None,
            flag: None,
            stage: None,
            quali: None,
            // Not `clean_golf_name`: it cuts at "PRES" (for "pres. by"
            // sponsors) and would leave the Presidents Cup with no name.
            name: ev.short_name.to_uppercase(),
            rows: Vec::new(),
        }),
        champ: None,
        series: None,
        session: None,
        situation: None,
        pregame: None,
    })
}

/// One F1 event is a whole race weekend; each session the user can subscribe to
/// (race / qualifying / sprint / sprint qualifying) becomes its OWN game with its
/// own schedule, status, and standings. Practice sessions are never shown.
fn parse_f1(
    ev: LeaderEvent,
    timing: &std::collections::HashMap<(String, String), F1Timing>,
) -> Vec<Game> {
    let base = clean_f1_name(&ev.short_name);
    let circuit = ev.circuit.map(|c| c.full_name).unwrap_or_default();
    ev.competitions
        .into_iter()
        .filter_map(|comp| {
            // (session id, frame-title suffix)
            let (session, suffix) = match comp.type_.as_ref().map(|t| t.abbreviation.as_str()) {
                Some("Race") => ("race", ""),
                Some("Qual") => ("quali", " Q"),
                Some("SR") => ("sprint", " S"),
                Some("SS") => ("sprint-quali", " SQ"),
                _ => return None, // practice / unknown
            };
            let status = map_status(&comp.status.type_)?;
            let live = matches!(status, GameStatus::Active | GameStatus::Intermission);
            let is_quali = matches!(session, "quali" | "sprint-quali");
            // The lap bar: ESPN's status `period` is the leader's lap count
            // (it climbs through a session and reads the full distance at
            // Final), and the total is a per-circuit constant the feed does
            // not carry. Race and sprint only, once the lights have gone
            // out; a circuit the table does not know draws no bar.
            let laps = match session {
                "race" | "sprint" if !matches!(status, GameStatus::Pregame) => {
                    crate::laps::total_laps(&circuit, session == "sprint").map(|total| {
                        let completed = u32::try_from(comp.status.period).unwrap_or(0).min(total);
                        crate::model::LapProgress { completed, total }
                    })
                }
                _ => None,
            };
            // (sort key, row, the driver's timing)
            let mut drivers: Vec<(i64, LeaderRow, F1Timing)> = comp
                .competitors
                .into_iter()
                .enumerate()
                .filter_map(|(i, c)| {
                    let code = f1_code(&c.athlete.as_ref()?.display_name);
                    let order = if c.order > 0 { c.order } else { i as i64 + 1 };
                    let t = timing.get(&(comp.id.clone(), c.id.clone())).cloned().unwrap_or_default();
                    // The scoreboard's inline statistics have been empty for
                    // every session all weekend; the per-driver core fetch is
                    // where gaps actually live. Inline stays as fallback in
                    // case ESPN ever starts populating it.
                    let value = t
                        .gap
                        .clone()
                        .or_else(|| {
                            c.statistics
                                .first()
                                .map(|s| s.display_value.clone())
                                .filter(|v| !v.is_empty())
                        })
                        .unwrap_or_default();
                    // Broadcast timing-tower style: rank centered in a fixed
                    // column, three-letter code in the team's color, gap/lap
                    // right-aligned. (No tire data in ESPN's feed.)
                    Some((order, LeaderRow {
                        rank: order.to_string(),
                        color: f1_team_color(&code),
                        name: code,
                        value,
                        tire: None,
                        thru: None,
                    }, t))
                })
                .collect();
            // A LIVE qualifying tower orders by the timing screen, which the
            // gaps are relative to and which the scoreboard order trails by
            // a minute. Only when ESPN gave every driver one — a half-keyed
            // sort would interleave two orderings.
            let by_position = is_quali && live && !drivers.is_empty()
                && drivers.iter().all(|(_, _, t)| t.position.is_some());
            if by_position {
                for (key, row, t) in drivers.iter_mut() {
                    *key = t.position.unwrap_or(*key);
                    row.rank = key.to_string();
                }
            }
            drivers.sort_by_key(|(o, _, _)| *o);
            let field_size = drivers.len();
            let (mut stage, mut zones) = (None, None);
            if is_quali {
                // No timing (fan-out failed, or the debug path) means no
                // stage either: "Q1" would be a guess dressed as a fact.
                stage = by_position.then(|| quali_stage(drivers.iter().map(|(_, _, t)| t)));
                zones = stage.and_then(|st| crate::model::QualiZones::for_field(field_size, st));
                quali_values(&mut drivers, zones, live);
            }
            let mut rows: Vec<LeaderRow> =
                drivers.into_iter().take(crate::render::LEADERBOARD_MAX_ROWS).map(|(_, r, _)| r).collect();
            // Race sessions show the INTERVAL to the car ahead, like the
            // broadcast tower — ESPN only carries gap-to-leader, but adjacent
            // gaps subtract cleanly (ESPN stamps the whole field in one
            // batch). Lapped cars keep their string and drop out of the
            // arithmetic. LIVE races only: a finished classification reads
            // as gaps to the winner, which is what ESPN already carries.
            if matches!(session, "race" | "sprint") && live {
                derive_intervals(&mut rows);
            }
            let name = fit_words(&format!("{base}{suffix}"), 13);
            Some(Game {
                id: format!("{}-{}", ev.id, session),
                sport_id: "f1".into(),
                away: TeamSide::empty("F1"),
                home: TeamSide::empty(""),
                status,
                ordinal: String::new(),
                clock: String::new(),
                start_time: comp.date.clone(),
                // ESPN's endDate is EVENT-level (when the whole weekend ends
                // — i.e. after the race). It's the race's end, not quali's,
                // so only the race session carries it.
                end_time: (session == "race" && !ev.end_date.is_empty())
                    .then(|| ev.end_date.clone()),
                leaderboard: Some(Leaderboard { matchplay: None, laps, name, flag: None, stage, quali: zones, rows }),
                champ: None,
                series: None,
                session: Some(session.into()),
                situation: None,
                pregame: None,
            })
        })
        .collect()
}

/// Which qualifying segment is running, from what ESPN has published: a
/// driver's lap time for a segment appears once that segment is OVER (the
/// eliminated carry theirs; observed live, 2026 Spanish GP), so any Q1 time
/// in the field means Q2, any Q2 time means Q3. ESPN publishes those with
/// some lag, so this can trail the real segment change by minutes — late
/// beats early: an early flip would paint the wrong six drivers red while
/// their segment still runs.
fn quali_stage<'a>(timing: impl Iterator<Item = &'a F1Timing>) -> u8 {
    let mut stage = 1;
    for t in timing {
        if t.qual_time[1].is_some() {
            stage = 3;
        } else if t.qual_time[0].is_some() {
            stage = stage.max(2);
        }
    }
    stage
}

/// The value column of a qualifying tower, in place. Rows are in rank order.
///
/// LIVE, in the running segment's field: ESPN's gap to the segment leader —
/// but a driver yet to set a time this segment still carries LAST segment's
/// gap, which ESPN sorts to the bottom of the field, so a gap smaller than
/// the one above it is stale and draws blank. Already eliminated: the lap
/// time that put them out, once ESPN has published it, else the segment gap
/// they finished on — either way a closed book, and the renderer greys it.
/// FINISHED: everyone's lap time from the last segment they ran, which is
/// the one column that reads coherently across Q1/Q2/Q3 (the per-segment
/// gaps are each relative to a different leader).
fn quali_values(
    drivers: &mut [(i64, LeaderRow, F1Timing)],
    zones: Option<crate::model::QualiZones>,
    live: bool,
) {
    let last_time = |t: &F1Timing| t.qual_time.iter().rev().flatten().next().map(|v| fit_lap_time(v));
    if !live || zones.is_none() {
        for (_, row, t) in drivers.iter_mut() {
            row.value = if live { row.value.clone() } else { last_time(t).unwrap_or_default() };
        }
        return;
    }
    let z = zones.unwrap();
    let mut prev = 0.0_f64;
    for (rank, row, t) in drivers.iter_mut() {
        let rank = *rank as u32;
        if rank > z.field {
            row.value = last_time(t).unwrap_or_else(|| row.value.clone());
            continue;
        }
        if rank == 1 {
            row.value.clear();
            continue;
        }
        match row.value.strip_prefix('+').and_then(|v| v.parse::<f64>().ok()) {
            Some(g) if g >= prev => prev = g,
            _ => row.value.clear(),
        }
    }
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

/// Team colors by driver code, per the broadcast timing tower. One table per
/// season (silly-season moves = one-line edits); unknown codes render white,
/// so a lineup change degrades gracefully instead of mis-coloring.
fn f1_team_color(code: &str) -> Option<embedded_graphics::pixelcolor::Rgb888> {
    use crate::model::hex_to_rgb;
    let hex = match code {
        "VER" | "TSU" => "3671C6",         // Red Bull
        "LEC" | "HAM" => "E8002D",         // Ferrari
        "RUS" | "ANT" => "27F4D2",         // Mercedes
        "NOR" | "PIA" => "FF8000",         // McLaren
        "ALO" | "STR" => "229971",         // Aston Martin
        "GAS" | "COL" => "0093CC",         // Alpine
        "ALB" | "SAI" => "64C4FF",         // Williams
        "LAW" | "HAD" => "6692FF",         // Racing Bulls
        "HUL" | "BOR" => "52E252",         // Sauber
        "OCO" | "BEA" => "B6BABD",         // Haas
        _ => return None,
    };
    Some(hex_to_rgb(hex))
}

/// Official-style broadcast code: the surname's first three letters
/// ("Verstappen" -> VER, "Hamilton" -> HAM) — matches the FIA code for the
/// whole current grid. Historical exceptions (Mick Schumacher was MSC, not
/// SCH) would need an override map here if one ever reaches the feed.
fn f1_code(display_name: &str) -> String {
    f1_last_name(display_name)
        .chars()
        .map(fold_diacritic)
        .filter(|c| c.is_ascii_alphabetic())
        .take(3)
        .collect()
}

/// Accented Latin capitals to their base letter, so Hülkenberg codes as HUL
/// and Pérez as PER (the FIA codes) rather than HLK / PRZ — which is what
/// dropping the non-ASCII letter produced, and what left both drivers'
/// rows white on the panel because the color table never matched.
fn fold_diacritic(c: char) -> char {
    match c {
        'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => 'A',
        'Ç' => 'C',
        'È' | 'É' | 'Ê' | 'Ë' => 'E',
        'Ì' | 'Í' | 'Î' | 'Ï' => 'I',
        'Ñ' => 'N',
        'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' => 'O',
        'Ù' | 'Ú' | 'Û' | 'Ü' => 'U',
        'Ý' => 'Y',
        _ => c,
    }
}

/// Shorten a tournament `shortName` to fit the panel (ported from `golf.py`).
fn clean_golf_name(short: &str) -> String {
    let up = short.to_uppercase();
    let mapped = golf_name_map(&up).map(String::from).unwrap_or(up);
    let mut words: Vec<&str> = mapped.split_whitespace().collect();
    // "pres. by <sponsor>" goes — as a WORD. Cutting at the letters "PRES"
    // also cut inside EXPRESS ("THE AMERICAN EX") and left the Presidents Cup
    // with no name at all.
    if let Some(idx) = words.iter().position(|w| matches!(*w, "PRES" | "PRES." | "PRESENTED")) {
        words.truncate(idx);
    }
    let mut filler_cut = None;
    for filler in ["TOURNAMENT", "CHAMPIONSHIP", "CHALLENGE", "CLASSIC", "INVITATIONAL"] {
        if let Some(idx) = words.iter().position(|w| *w == filler) {
            words.truncate(idx);
            filler_cut = Some(filler);
        }
    }
    let had_the = words.first() == Some(&"THE");
    if had_the {
        words.remove(0);
    }
    if words
        .first()
        .map_or(false, |w| w.chars().all(|c| c.is_ascii_digit()) || *w == "AT&T" || *w == "WGC-FEDEX")
    {
        words.remove(0);
    }
    let mut of_tail = Vec::new();
    if let Some(idx) = words.iter().position(|w| *w == "OF") {
        of_tail = words.split_off(idx);
    }
    // One short word left is a fragment, not a tournament: the PGA
    // Championship drew "PGA", The Open "OPEN", the Bank of Utah "BANK", and
    // the BMW, TOUR and ISCO Championships one bare word each. Put back the
    // last thing cut, in its shortest form.
    if words.len() == 1 && words[0].chars().count() <= 4 {
        if !of_tail.is_empty() {
            words.extend(of_tail);
        } else if let Some(filler) = filler_cut {
            words.push(if filler == "CHAMPIONSHIP" { "CHAMP" } else { filler });
        } else if had_the {
            words.insert(0, "THE");
        }
    }
    let name = words.join(" ");
    let name = if words.first() == Some(&"MASTERS") {
        format!("THE {name}")
    } else {
        name
    };
    // Fit the panel, the same way F1 already does. Without this a long name was
    // centered at a NEGATIVE x and clipped at BOTH ends — "The Standard
    // Portland Classic" shortened to "STANDARD PORTLAND", 17 characters against
    // a 16-character panel, and drew as "TANDARD PORTLAN". 18 of the 81 events
    // on the 2026 PGA and LPGA calendars overflowed, so this was the common
    // case for a long title rather than a rare one.
    //
    // Leading words go first because that is where golf keeps its sponsors:
    // "STANDARD PORTLAND" -> "PORTLAND" names the tournament, and dropping from
    // the other end would not. `fit_words` keeps a two-word tail whatever
    // happens, so a name is never reduced to a single sponsor.
    fit_words_px(&name, crate::protocol_frame_width())
}

/// Fit `text` to `max_px` for the panel, preferring whole words to fragments.
///
/// Pixel width rather than a character count, because the 4x6 font is NOT
/// monospaced — `N` is five columns where every other glyph is four — so
/// sixteen characters is 64px for "PORTLAND CLASSIC" and 65px for "PUERTO RICO
/// OPEN".
///
/// Three moves, in order, because each is lossier than the one before:
///
/// 1. **Drop punctuation.** "U.S. WOMEN'S OPEN" is 65px and "US WOMENS OPEN" is
///    56px — every word survives and nobody misreads it. This alone rescues
///    both Women's Opens, which otherwise each collapsed to "WOMEN'S OPEN" and
///    so named each other.
/// 2. **Drop leading words**, which is where golf keeps its sponsors:
///    "STANDARD PORTLAND" -> "PORTLAND". Never leaves a result starting with a
///    preposition — "RIVIERA MAYA OPEN AT MAYAKOBA" must not become "AT
///    MAYAKOBA" — and will go down to a single word, because one whole word
///    names a tournament better than two chopped ones. The first version of
///    this kept a two-word tail and drew "STANDARD PORTLA".
/// 3. **Truncate**, only when a lone word is still too wide. Reaching here means
///    the name was going to be unsatisfying whatever we did.
fn fit_words_px(text: &str, max_px: i32) -> String {
    let w = |t: &str| crate::render::text_width(t, false);
    let up = text.to_uppercase();
    if w(&up) <= max_px {
        return up;
    }
    let stripped: String = up.chars().filter(|c| *c != '.' && *c != '\'').collect();
    if w(&stripped) <= max_px {
        return stripped;
    }
    // A tail starting with a preposition reads as a fragment of a sentence
    // rather than the name of anything.
    let leading_junk = |t: &str| {
        matches!(t.split(' ').next().unwrap_or(""), "AT" | "IN" | "OF" | "BY" | "THE" | "AND")
    };
    let words: Vec<&str> = stripped.split_whitespace().collect();
    for start in 1..words.len() {
        let cand = words[start..].join(" ");
        if w(&cand) <= max_px && !leading_junk(&cand) {
            return cand;
        }
    }
    let mut last = words.last().copied().unwrap_or("").to_string();
    while !last.is_empty() && w(&last) > max_px {
        last.pop();
    }
    last
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
        // These four survive the generic fit but read badly out the other side,
        // which is what this table is for. Without them: "Puerto Rico Open"
        // becomes "RICO OPEN" (a different place), the ANNIKA becomes
        // "PELICAN" (the venue, not the event), and the women's Scottish Open
        // becomes "SCOTTISH OPEN" — identical to the men's, on a board that can
        // show both in the same week.
        "PUERTO RICO OPEN" => "PUERTO RICO",
        // AMERICAN EXPRESS is 65px (one N too many); EXPRESS alone names a card.
        "THE AMERICAN EXPRESS" => "THE AMEX",
        "THE ANNIKA DRIVEN BY GAINBRIDGE AT PELICAN" => "THE ANNIKA",
        "ISPS HANDA WOMEN'S SCOTTISH OPEN" => "WOMENS SCOTTISH",
        "CPKC WOMEN'S OPEN" => "CPKC OPEN",
        _ => return None,
    })
}

/// Shorten an F1 race `shortName` (drop leading sponsor words until it fits).
/// The race title from an F1 `shortName` ("MSC Cruises United States GP").
///
/// ESPN puts the sponsor first and the place right before "GP", so the place
/// is what to keep. Dropping leading words until the name fit — what this did
/// before — cut every two-word place in half ("STATES GP", "CITY GP",
/// "ARABIAN GP") and kept the tail of "Bahrain GP in Malaysia" ("IN
/// MALAYSIA"). Two-word places are named here; anything else keeps its last
/// word, and anything after "GP" goes. Accents fold: the 4x6 font draws
/// "SAO", not "SÃO".
fn clean_f1_name(short: &str) -> String {
    let up: String = short.to_uppercase().chars().map(fold_diacritic).collect();
    let words: Vec<&str> = up.split_whitespace().collect();
    let place = match words.iter().position(|w| *w == "GP").map(|gp| &words[..gp]) {
        Some([.., "UNITED", "STATES"]) => "US",
        Some([.., "SAUDI", "ARABIAN"]) => "SAUDI",
        Some([.., "MEXICO", "CITY"]) => "MEXICO",
        Some([.., "EMILIA", "ROMAGNA"]) => "IMOLA",
        Some([.., "SAO", "PAULO"]) => "SAO PAULO",
        Some([.., "LAS", "VEGAS"]) => "LAS VEGAS",
        Some([.., "ABU", "DHABI"]) => "ABU DHABI",
        // "BARCELONA-CATALUNYA" -> "BARCELONA".
        Some([.., last]) => last.split('-').next().unwrap_or(last),
        // No "GP" in it: not a shape this knows, so just make it fit.
        _ => return fit_words(&up, 13),
    };
    fit_words(&format!("{place} GP"), 13)
}

/// Fit `text` to `max` chars for the 64px frame by dropping LEADING words (they
/// carry sponsors: "PIRELLI BRITISH GP" -> "BRITISH GP"); hard-truncate if a
/// two-word tail still doesn't fit.
fn fit_words(text: &str, max: usize) -> String {
    let up = text.to_uppercase();
    let mut words: Vec<&str> = up.split_whitespace().collect();
    while words.len() > 2 && words.join(" ").chars().count() > max {
        words.remove(0);
    }
    let joined = words.join(" ");
    if joined.chars().count() > max {
        joined.chars().take(max).collect()
    } else {
        joined
    }
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
    /// Decoded ONE EVENT AT A TIME: an event whose shape this parser does not
    /// know is logged and skipped instead of failing the whole response. The
    /// 2026 Presidents Cup (match play, with competitions nested by session)
    /// used to take down the entire PGA feed this way, every poll, for the
    /// whole week — along with any ordinary event sharing its response.
    #[serde(default, deserialize_with = "de_events_one_by_one")]
    events: Vec<LeaderEvent>,
}

/// See `LeaderboardResponse::events`.
fn de_events_one_by_one<'de, D>(d: D) -> Result<Vec<LeaderEvent>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Option<Vec<serde_json::Value>> = Option::deserialize(d)?;
    Ok(raw
        .unwrap_or_default()
        .into_iter()
        // Match play has its own parser (`golf_games`); not an error here.
        .filter(|v| !is_match_play(v))
        .filter_map(|v| {
            let label = || {
                format!(
                    "{} ({})",
                    v.get("id").and_then(|x| x.as_str()).unwrap_or("?"),
                    v.get("shortName").and_then(|x| x.as_str()).unwrap_or("?")
                )
            };
            match serde_json::from_value::<LeaderEvent>(v.clone()) {
                Ok(ev) => Some(ev),
                Err(e) => {
                    tracing::warn!("leaderboard event {} skipped: {e}", label());
                    None
                }
            }
        })
        .collect())
}
#[derive(Deserialize)]
struct LeaderEvent {
    #[serde(default)]
    id: String,
    #[serde(default, rename = "shortName")]
    short_name: String,
    /// When the event ENDS — golf tournaments and F1 weekends span days, and
    /// ESPN carries `endDate` on these leaderboard events (team-sport
    /// scoreboard events have no such field).
    #[serde(default, rename = "endDate")]
    end_date: String,
    #[serde(default)]
    competitions: Vec<LeaderComp>,
    /// F1 only: the venue, whose name keys the scheduled lap count.
    #[serde(default)]
    circuit: Option<LeaderCircuit>,
}
#[derive(Deserialize)]
struct LeaderCircuit {
    #[serde(default, rename = "fullName")]
    full_name: String,
}
#[derive(Deserialize)]
struct LeaderComp {
    #[serde(default)]
    id: String,
    #[serde(default)]
    date: String,
    status: Status,
    #[serde(default)]
    competitors: Vec<LeaderCompetitor>,
    /// Session kind for racing weekends ("Race", "Qual", "SR", "SS", "FP1"…).
    #[serde(default, rename = "type")]
    type_: Option<LeaderCompType>,
}

#[derive(Deserialize)]
struct LeaderCompType {
    #[serde(default)]
    abbreviation: String,
}
#[derive(Deserialize)]
struct LeaderCompetitor {
    #[serde(default)]
    id: String,
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
    /// Holes played in the current round. 18 for a finished round and 0 before
    /// a player tees off; the mid-round 1..17 shape is what the panel wants and
    /// is the one value that could NOT be observed out of season, so the
    /// renderer treats anything outside 1..=17 as "say nothing".
    #[serde(default)]
    thru: Option<i64>,
    /// When this player actually tees off.
    ///
    /// The event's own `date` is NOT a start time for golf: ESPN reports
    /// midnight local on day one and sets `timeValid: false` to say so. Taken
    /// literally it put a tournament on the board at 10:30pm the night before,
    /// via the half-hour pre-game window. The earliest of these is the real
    /// start of play.
    #[serde(default, rename = "teeTime")]
    tee_time: String,
    /// Which round this player's `tee_time` belongs to. The field can hold a
    /// MIX — a golfer who missed the cut stays on the round they were cut in —
    /// so `first_tee_time` matches it against the competition's round rather
    /// than trusting every entry to be current.
    #[serde(default)]
    period: i64,
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
        abbrev: "BOS".into(), name: "BRUINS".into(), location: "BOSTON".into(), score: 2,
        primary: hex_to_rgb("FFB81C"), secondary: hex_to_rgb("000000"), hits: None, errors: None, shots: None, shootout: None, aggregate: None,
    };
    let leafs = TeamSide {
        abbrev: "TOR".into(), name: "LEAFS".into(), location: "TORONTO".into(), score: 1,
        primary: hex_to_rgb("00205B"), secondary: hex_to_rgb("FFFFFF"), hits: None, errors: None, shots: None, shootout: None, aggregate: None,
    };
    let celtics = TeamSide {
        abbrev: "BOS".into(), name: "CELTICS".into(), location: "BOSTON".into(), score: 88,
        primary: hex_to_rgb("007A33"), secondary: hex_to_rgb("FFFFFF"), hits: None, errors: None, shots: None, shootout: None, aggregate: None,
    };
    let lakers = TeamSide {
        abbrev: "LAL".into(), name: "LAKERS".into(), location: "LOS ANGELES".into(), score: 84,
        primary: hex_to_rgb("552583"), secondary: hex_to_rgb("FDB927"), hits: None, errors: None, shots: None, shootout: None, aggregate: None,
    };
    vec![
        Game {
            id: "mock-nhl".into(), sport_id: "nhl".into(), away: bruins, home: leafs,
            status: GameStatus::Active, ordinal: "2nd".into(), clock: "12:34".into(),
            start_time: String::new(), end_time: None,
            leaderboard: None,
            // Tagged so the app's big-game caption + selection are exercisable
            // offline (outside playoff season there are no live postseason
            // games to test against).
            champ: Some(Champ { tier: "final".into(), label: "Stanley Cup Final".into() }),
            series: None,
            session: None,
            situation: None,
            pregame: None,
        },
        Game {
            id: "mock-nba".into(), sport_id: "nba".into(), away: celtics, home: lakers,
            status: GameStatus::Active, ordinal: "4th".into(), clock: "5:20".into(),
            start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            situation: None,
            pregame: None,
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

/// Demo timing-tower row: rank + code + team color + tire compound.
fn f1_row(rank: u32, code: &str, value: &str, tire: char) -> LeaderRow {
    LeaderRow {
        rank: rank.to_string(),
        name: code.into(),
        value: value.into(),
        color: f1_team_color(code),
        tire: Some(tire),
        thru: None,
    }
}

/// City / college / country for a demo team's abbrev — so the Location name
/// style renders a real place in `--demo` mode. Unknown abbrevs get "" (the
/// Location style then falls back to whatever the renderer does with an empty
/// string, same as a leaderboard side).
fn demo_location(abbrev: &str) -> &'static str {
    match abbrev {
        "BOS" => "BOSTON",
        "TOR" => "TORONTO",
        "COL" => "COLORADO",
        "VGK" => "VEGAS",
        "DET" => "DETROIT",
        "CHI" => "CHICAGO",
        "LAL" | "LAD" | "LAFC" => "LOS ANGELES",
        "GSW" => "GOLDEN STATE",
        "DEN" => "DENVER",
        "MIA" => "MIAMI",
        "NYK" | "NYY" | "NYL" | "NYR" => "NEW YORK",
        "CHC" => "CHICAGO",
        "HOU" => "HOUSTON",
        "SEA" => "SEATTLE",
        "NE" => "NEW ENGLAND",
        "BUF" => "BUFFALO",
        "OSU" => "OHIO STATE",
        "MICH" => "MICHIGAN",
        "LVA" => "LAS VEGAS",
        "ARS" => "LONDON",
        "MCI" => "MANCHESTER",
        "USA" => "USA",
        "FRA" => "FRANCE",
        "EDM" => "EDMONTON",
        "MIL" => "MILWAUKEE",
        "PHX" => "PHOENIX",
        _ => "",
    }
}

pub(crate) fn demo_games(n: u64, elapsed_secs: u64) -> Vec<Game> {
    // The scripted ending: live for the first five minutes of the server's life,
    // then FINAL — exercises the observed live->Final transition and the
    // half-hour linger without waiting for a real game to end.
    let ending_live = elapsed_secs < 5 * 60;
    // Locations come from a per-abbrev lookup so the Location ("City"/"College"/
    // "Country") name style has something real to show in demo mode — without
    // threading a city through every call site below.
    let team = |abbrev: &str, name: &str, score: u32, p: &str, s: &str| TeamSide {
        abbrev: abbrev.into(),
        name: name.into(),
        location: demo_location(abbrev).into(),
        hits: None,
        errors: None,
        shots: None,
        shootout: None, aggregate: None,
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
            status: GameStatus::Active, ordinal: period(0), clock: clock.clone(), start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: Some(Champ { tier: "final".into(), label: "Stanley Cup Final".into() }),
            series: None,
            session: None,
            // Cycle the strength states so the power-play badge (abbrev or
            // skater counts) is visible in demo mode.
            situation: Some(match (n / 6) % 4 {
                0 => Situation::Hockey { away_powerplay: true, home_powerplay: false, away_players: 5, home_players: 4 },
                1 => Situation::Hockey { away_powerplay: false, home_powerplay: true, away_players: 4, home_players: 5 },
                2 => Situation::Hockey { away_powerplay: false, home_powerplay: false, away_players: 4, home_players: 4 },
                _ => Situation::Hockey { away_powerplay: false, home_powerplay: false, away_players: 5, home_players: 5 },
            }),
            pregame: None,
        },
        Game {
            id: "demo-nhl-2".into(), sport_id: "nhl".into(),
            away: team("COL", "AVALANCHE", (n / 4 % 5) as u32, "6F263D", "236192"),
            home: team("VGK", "KNIGHTS", (n / 6 % 4) as u32, "B4975A", "333F42"),
            status: GameStatus::Active, ordinal: period(2), clock: clock.clone(), start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: Some(Champ { tier: "conf".into(), label: "Conference Finals".into() }),
            series: None,
            session: None,
            situation: None,
            pregame: None,
        },
        // Overtime: 3-on-3 shows the skater-count badge next to the "OT"
        // ordinal (regular 5v5 draws no badge), cycling through a power play.
        Game {
            id: "demo-nhl-ot".into(), sport_id: "nhl".into(),
            away: team("DET", "RED WINGS", 2, "CE1126", "FFFFFF"),
            home: team("CHI", "BLACKHAWKS", 2, "CF0A2C", "FFD100"),
            status: GameStatus::Active, ordinal: "OT".into(), clock: clock.clone(), start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            situation: Some(match (n / 10) % 3 {
                0 => Situation::Hockey { away_powerplay: false, home_powerplay: false, away_players: 3, home_players: 3 },
                1 => Situation::Hockey { away_powerplay: true, home_powerplay: false, away_players: 4, home_players: 3 },
                _ => Situation::Hockey { away_powerplay: false, home_powerplay: true, away_players: 3, home_players: 4 },
            }),
            pregame: None,
        },
        Game {
            id: "demo-nba".into(), sport_id: "nba".into(),
            away: team("BOS", "CELTICS", (80 + n % 30) as u32, "007A33", "FFFFFF"),
            home: team("LAL", "LAKERS", (78 + n % 28) as u32, "552583", "FDB927"),
            status: GameStatus::Active, ordinal: period(1), clock: clock.clone(), start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            situation: None,
            pregame: None,
        },
        Game {
            id: "demo-nba-2".into(), sport_id: "nba".into(),
            away: team("GSW", "WARRIORS", (85 + n % 25) as u32, "1D428A", "FFC72C"),
            home: team("DEN", "NUGGETS", (88 + n % 22) as u32, "0E2240", "FEC524"),
            status: GameStatus::Active, ordinal: period(3), clock: clock.clone(), start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            situation: None,
            pregame: None,
        },
        Game {
            id: "demo-nba-3".into(), sport_id: "nba".into(),
            away: team("MIA", "HEAT", (70 + n % 26) as u32, "98002E", "F9A01B"),
            home: team("NYK", "KNICKS", (72 + n % 24) as u32, "006BB6", "F58426"),
            status: GameStatus::Active, ordinal: period(0), clock: clock.clone(), start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            situation: None,
            pregame: None,
        },
        Game {
            id: "demo-mlb".into(), sport_id: "mlb".into(),
            away: team("NYY", "YANKEES", (n / 7 % 9) as u32, "132448", "C4CED3"),
            home: team("BOS", "RED SOX", (n / 6 % 9) as u32, "BD3039", "FFFFFF"),
            status: GameStatus::Active, ordinal: period(2), clock: String::new(), start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            situation: Some(Situation::Baseball {
                balls: (n % 4) as u8,
                strikes: (n % 3) as u8,
                outs: ((n / 3) % 3) as u8,
                on_first: n % 2 == 0,
                on_second: (n / 2) % 2 == 0,
                on_third: (n / 4) % 2 == 0,
                is_inning_top: (n / 8) % 2 == 0,
            }),
            pregame: None,
        },
        Game {
            id: "demo-mlb-2".into(), sport_id: "mlb".into(),
            away: team("LAD", "DODGERS", (n / 5 % 8) as u32, "005A9C", "EF3E42"),
            home: team("CHC", "CUBS", (n / 8 % 7) as u32, "0E3386", "CC3433"),
            status: GameStatus::Active, ordinal: period(1), clock: String::new(), start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            // Static max-drama frame next to demo-mlb's animated one: bases
            // loaded, full count, two out (every diamond and square filled).
            situation: Some(Situation::Baseball {
                balls: 3, strikes: 2, outs: 2,
                on_first: true, on_second: true, on_third: true,
                is_inning_top: false,
            }),
            pregame: None,
        },
        Game {
            id: "demo-mlb-ending".into(), sport_id: "mlb".into(),
            away: team("HOU", "ASTROS", 5, "002D62", "EB6E1F"),
            home: team("SEA", "MARINERS", 4, "0C2C56", "005C5C"),
            status: if ending_live { GameStatus::Active } else { GameStatus::Final },
            ordinal: if ending_live { "9th".into() } else { String::new() },
            clock: String::new(), start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            situation: None,
            pregame: None,
        },
        Game {
            id: "demo-nfl".into(), sport_id: "nfl".into(),
            away: team("NE", "PATRIOTS", (n / 9 % 35) as u32, "002244", "C60C30"),
            home: team("BUF", "BILLS", (n / 8 % 35) as u32, "00338D", "FFFFFF"),
            status: GameStatus::Active, ordinal: period(3), clock: clock.clone(), start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            situation: Some(Situation::Football {
                // Ampersands, like the feed sends and the panel now draws — the
                // demo is what the app previews, so a "+" here would show a
                // frame no real game produces.
                down_string: match n % 4 {
                    0 => "1st & 10", 1 => "2nd & 7", 2 => "3rd & 4", _ => "4th & 1",
                }.into(),
                ball_position: if (n / 8) % 2 == 0 {
                    format!("NE {}", 20 + n % 30)
                } else {
                    format!("BUF {}", 20 + n % 30)
                },
                home_possession: Some((n / 8) % 2 == 1),
            }),
            pregame: None,
        },
        Game {
            id: "demo-cfb".into(), sport_id: "cfb".into(),
            away: team("OSU", "BUCKEYES", (n / 7 % 28) as u32, "BB0000", "666666"),
            home: team("MICH", "WOLVERINES", (n / 6 % 31) as u32, "00274C", "FFCB05"),
            status: GameStatus::Active, ordinal: period(1), clock: clock.clone(), start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            situation: Some(Situation::Football {
                down_string: match (n / 2) % 4 {
                    0 => "1st & 10", 1 => "2nd & 6", 2 => "3rd & 2", _ => "4th & 8",
                }.into(),
                ball_position: if (n / 5) % 2 == 0 {
                    format!("OSU {}", 15 + n % 35)
                } else {
                    format!("MICH {}", 15 + n % 35)
                },
                home_possession: Some((n / 5) % 2 == 1),
            }),
            pregame: None,
        },
        Game {
            id: "demo-wnba".into(), sport_id: "wnba".into(),
            away: team("LVA", "ACES", (n / 6 % 22) as u32, "CE0E2D", "000000"),
            home: team("NYL", "LIBERTY", (n / 7 % 24) as u32, "6ECEB2", "000000"),
            status: GameStatus::Active, ordinal: period(2), clock: clock.clone(), start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            situation: None,
            pregame: None,
        },
        Game {
            id: "demo-mls".into(), sport_id: "mls".into(),
            away: team("LAFC", "LAFC", (n / 6 % 4) as u32, "000000", "C39E6D"),
            home: team("MIA", "INTER MIAMI", (n / 4 % 4) as u32, "F7B5CD", "231F20"),
            status: GameStatus::Active, ordinal: "2nd".into(), clock: clock.clone(), start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            situation: None,
            pregame: None,
        },
        Game {
            id: "demo-epl".into(), sport_id: "epl".into(),
            away: team("ARS", "ARSENAL", (n / 5 % 4) as u32, "EF0107", "FFFFFF"),
            home: team("MCI", "MAN CITY", (n / 7 % 4) as u32, "6CABDD", "FFFFFF"),
            status: GameStatus::Active, ordinal: "1st".into(), clock: clock.clone(), start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            situation: None,
            pregame: None,
        },
        Game {
            id: "demo-wc".into(), sport_id: "wc".into(),
            away: team("USA", "USA", (n / 8 % 3) as u32, "002868", "BF0A30"),
            home: team("FRA", "FRANCE", (n / 9 % 3) as u32, "21304D", "FFFFFF"),
            status: GameStatus::Active, ordinal: "2nd".into(), clock, start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            situation: None,
            pregame: None,
        },
        Game {
            id: "demo-golf".into(), sport_id: "pga".into(),
            away: TeamSide::empty("PGA"), home: TeamSide::empty(""),
            status: GameStatus::Active, ordinal: String::new(), clock: String::new(), start_time: String::new(), end_time: None,
            // Ten deep so the board's scrolling leaderboard shows in demo mode.
            leaderboard: Some(Leaderboard { matchplay: None, laps: None, flag: None, stage: None, quali: None, name: "MASTERS".into(), rows: vec![
                LeaderRow { name: "SCHEFFLER".into(), value: "-8".into(), ..Default::default() },
                LeaderRow { name: "MCILROY".into(), value: "-6".into(), ..Default::default() },
                LeaderRow { name: "MORIKAWA".into(), value: "-5".into(), ..Default::default() },
                LeaderRow { name: "SCHAUFFELE".into(), value: "-3".into(), ..Default::default() },
                LeaderRow { name: "RAHM".into(), value: "-2".into(), ..Default::default() },
                LeaderRow { name: "FLEETWOOD".into(), value: "-2".into(), ..Default::default() },
                LeaderRow { name: "HOVLAND".into(), value: "-1".into(), ..Default::default() },
                LeaderRow { name: "THOMAS".into(), value: "E".into(), ..Default::default() },
                LeaderRow { name: "SPIETH".into(), value: "+1".into(), ..Default::default() },
                LeaderRow { name: "DAY".into(), value: "+2".into(), ..Default::default() },
            ] }),
            champ: Some(Champ { tier: "major".into(), label: "The Masters".into() }),
            series: None,
            session: None,
            situation: None,
            pregame: None,
        },
        Game {
            id: "demo-golf-2".into(), sport_id: "lpga".into(),
            away: TeamSide::empty("LPGA"), home: TeamSide::empty(""),
            status: GameStatus::Active, ordinal: String::new(), clock: String::new(), start_time: String::new(), end_time: None,
            leaderboard: Some(Leaderboard { matchplay: None, laps: None, flag: None, stage: None, quali: None, name: "US OPEN".into(), rows: vec![
                LeaderRow { name: "KORDA".into(), value: "-7".into(), ..Default::default() },
                LeaderRow { name: "THITIKUL".into(), value: "-5".into(), ..Default::default() },
                LeaderRow { name: "VU".into(), value: "-4".into(), ..Default::default() },
                LeaderRow { name: "SAIGO".into(), value: "-2".into(), ..Default::default() },
            ] }),
            champ: Some(Champ { tier: "major".into(), label: "U.S. Women's Open".into() }),
            series: None,
            session: None,
            situation: None,
            pregame: None,
        },
        Game {
            id: "demo-f1".into(), sport_id: "f1".into(),
            away: TeamSide::empty("F1"), home: TeamSide::empty(""),
            status: GameStatus::Active, ordinal: String::new(), clock: String::new(), start_time: String::new(), end_time: None,
            // Full broadcast treatment: team colors + tire compounds (live
            // ESPN rows carry no tire data, so the demo is where dots show).
            leaderboard: Some(Leaderboard { matchplay: None, laps: None, flag: None, stage: None, quali: None, name: "MONACO GP".into(), rows: vec![
                f1_row(1, "VER", "LAP 42", 'M'),
                f1_row(2, "NOR", "+2.1", 'M'),
                f1_row(3, "LEC", "+5.8", 'S'),
                f1_row(4, "PIA", "+9.0", 'M'),
                f1_row(5, "RUS", "+12.4", 'H'),
                f1_row(6, "HAM", "+15.9", 'S'),
                f1_row(7, "ALO", "+21.2", 'H'),
                f1_row(8, "SAI", "+24.7", 'M'),
                f1_row(9, "GAS", "+31.0", 'I'),
                f1_row(10, "ALB", "+38.5", 'W'),
                f1_row(11, "HUL", "+42.0", 'H'),
                f1_row(12, "OCO", "+44.6", 'M'),
                f1_row(13, "STR", "+48.1", 'H'),
                f1_row(14, "TSU", "+52.7", 'M'),
                f1_row(15, "LAW", "+55.3", 'S'),
                f1_row(16, "HAD", "+58.9", 'M'),
                f1_row(17, "BEA", "+1:04", 'H'),
                f1_row(18, "ANT", "+1:09", 'S'),
                f1_row(19, "BOR", "+1:15", 'M'),
                f1_row(20, "COL", "+1:22", 'W'),
            ] }),
            champ: None,
            series: None,
            session: Some("race".into()),
            situation: None,
            pregame: None,
        },
        // Upcoming games: one inside the 30-min pregame window (appears on the
        // board with its start time), one hours out (View-all browse only).
        // The same tower WITHOUT tire data — exactly what live ESPN rows look
        // like (no tire feed): team colors, no chip column, timings at the edge.
        Game {
            id: "demo-f1-quali".into(), sport_id: "f1".into(),
            away: TeamSide::empty("F1"), home: TeamSide::empty(""),
            status: GameStatus::Active, ordinal: String::new(), clock: String::new(), start_time: String::new(), end_time: None,
            leaderboard: Some(Leaderboard { matchplay: None, laps: None, flag: None, stage: None, quali: None, name: "BRITISH GP Q".into(), rows: vec![
                LeaderRow { rank: "1".into(), name: "NOR".into(), value: "1:24.8".into(), color: f1_team_color("NOR"), tire: None, thru: None },
                LeaderRow { rank: "2".into(), name: "VER".into(), value: "+0.093".into(), color: f1_team_color("VER"), tire: None, thru: None },
                LeaderRow { rank: "3".into(), name: "HAM".into(), value: "+0.210".into(), color: f1_team_color("HAM"), tire: None, thru: None },
                LeaderRow { rank: "4".into(), name: "RUS".into(), value: "+0.286".into(), color: f1_team_color("RUS"), tire: None, thru: None },
                LeaderRow { rank: "5".into(), name: "PIA".into(), value: "+0.334".into(), color: f1_team_color("PIA"), tire: None, thru: None },
                LeaderRow { rank: "6".into(), name: "ALO".into(), value: "+0.412".into(), color: f1_team_color("ALO"), tire: None, thru: None },
                LeaderRow { rank: "7".into(), name: "LEC".into(), value: "+0.501".into(), color: f1_team_color("LEC"), tire: None, thru: None },
                LeaderRow { rank: "8".into(), name: "HUL".into(), value: "+0.688".into(), color: f1_team_color("HUL"), tire: None, thru: None },
                LeaderRow { rank: "9".into(), name: "ALB".into(), value: "+0.719".into(), color: f1_team_color("ALB"), tire: None, thru: None },
                LeaderRow { rank: "10".into(), name: "GAS".into(), value: "+0.854".into(), color: f1_team_color("GAS"), tire: None, thru: None },
            ] }),
            champ: None,
            series: None,
            session: Some("quali".into()),
            situation: None,
            pregame: None,
        },
        Game {
            id: "demo-nhl-soon".into(), sport_id: "nhl".into(),
            away: team("NYR", "RANGERS", 0, "0038A8", "CE1126"),
            home: team("EDM", "OILERS", 0, "041E42", "FF4C00"),
            status: GameStatus::Pregame, ordinal: String::new(), clock: String::new(),
            start_time: (chrono::Utc::now() + chrono::Duration::minutes(20)).format("%Y-%m-%dT%H:%MZ").to_string(),
            end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            situation: None,
            pregame: Some(Pregame {
                away_record: "48-25".into(),
                home_record: "51-22".into(),
                broadcast: "ESPN".into(),
            }),
        },
        Game {
            id: "demo-nba-tonight".into(), sport_id: "nba".into(),
            away: team("MIL", "BUCKS", 0, "00471B", "EEE1C6"),
            home: team("PHX", "SUNS", 0, "1D1160", "E56020"),
            status: GameStatus::Pregame, ordinal: String::new(), clock: String::new(),
            start_time: (chrono::Utc::now() + chrono::Duration::hours(3)).format("%Y-%m-%dT%H:%MZ").to_string(),
            end_time: None,
            leaderboard: None,
            champ: None,
            series: None,
            session: None,
            situation: None,
            // Records with NO national broadcast — exercises the records-only
            // variant (single centered start-time line, records in the rows).
            pregame: Some(Pregame {
                away_record: "39-19".into(),
                home_record: "35-23".into(),
                broadcast: String::new(),
            }),
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
    /// Live in-game details: football down/distance/possession, baseball
    /// count/outs/runners. Only present while the game is in progress.
    #[serde(default)]
    situation: Option<EspnSituation>,
    /// TV listings by market; only the "national" entry is shown (regional
    /// feeds differ per viewer, so naming one would usually be wrong).
    #[serde(default)]
    broadcasts: Vec<EspnBroadcast>,
    /// Playoff series state. The key is ABSENT outside a series — including on
    /// play-in games, which are postseason but not a series — so its presence
    /// is the gate, not the season type.
    #[serde(default)]
    series: Option<EspnSeries>,
}
#[derive(Deserialize)]
struct EspnSeries {
    /// Same order as `competitors`: HOME first, away second. Verified across 13
    /// playoff games; it is NOT sorted leader-first.
    #[serde(default)]
    competitors: Vec<EspnSeriesCompetitor>,
}
#[derive(Deserialize)]
struct EspnSeriesCompetitor {
    #[serde(default)]
    wins: u32,
}
/// ESPN `competitions[0].situation` (subset) — the same fields the original
/// scoreboard's Python server read (football_v2.py / baseball_v2.py).
#[derive(Deserialize, Default)]
struct EspnSituation {
    #[serde(default, rename = "shortDownDistanceText")]
    short_down_distance_text: String,
    #[serde(default, rename = "possessionText")]
    possession_text: String,
    /// Team id of the side with the ball (matches Competitor.id).
    #[serde(default)]
    possession: Option<String>,
    #[serde(default)]
    balls: u8,
    #[serde(default)]
    strikes: u8,
    #[serde(default)]
    outs: u8,
    #[serde(default, rename = "onFirst")]
    on_first: bool,
    #[serde(default, rename = "onSecond")]
    on_second: bool,
    #[serde(default, rename = "onThird")]
    on_third: bool,
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
    /// "pre" | "in" | "post". ESPN's own verdict on whether play is happening,
    /// and the only field that separates a pause *within* a game from one that
    /// ends the day — see `map_status`.
    #[serde(default)]
    state: String,
    /// e.g. "Top 5th" — the original server derived baseball's inning half from
    /// this ("Top" substring check), so we do too.
    #[serde(default, rename = "shortDetail")]
    short_detail: String,
    /// ESPN's own short name for a period past regulation: "OT", "2OT", "SO".
    /// Absent on every game that ended in regulation, so its presence IS the
    /// signal.
    ///
    /// Worth having rather than deriving from the period number, because the
    /// period number is ambiguous exactly where it matters. An NHL game at
    /// period 5 is a shootout in the regular season and double overtime in the
    /// playoffs — over five sampled dates that is 7 games ESPN calls "SO" that
    /// arithmetic would have called "2OT".
    #[serde(default, rename = "altDetail")]
    alt_detail: Option<String>,
}
#[derive(Deserialize)]
struct Competitor {
    /// Competitor id == team id; football possession is matched against this.
    #[serde(default)]
    id: String,
    #[serde(default)]
    score: String,
    #[serde(default, rename = "homeAway")]
    home_away: String,
    team: EspnTeam,
    /// Season records; the overall one has type "total" (summary "47-42").
    #[serde(default)]
    records: Vec<EspnRecord>,
    /// Baseball's other two box-score columns, sitting beside `score` in every
    /// payload we already download. Optional because absent is not zero: a game
    /// that has not started has no hit count, and a no-hitter has a real one.
    #[serde(default)]
    hits: Option<u32>,
    #[serde(default)]
    errors: Option<u32>,
    /// Penalty shootout, on soccer knockouts only. Without it a shootout final
    /// renders as the draw it was after 120 minutes and never says who went
    /// through.
    #[serde(default, rename = "shootoutScore")]
    shootout_score: Option<u32>,
    /// Aggregate over both legs of a knockout tie. A JSON FLOAT where the score
    /// beside it is a JSON STRING, and the key is absent entirely — not null,
    /// not zero — on first legs and on every league match.
    #[serde(default, rename = "aggregateScore")]
    aggregate_score: Option<f64>,
}
#[derive(Deserialize)]
struct EspnRecord {
    #[serde(default, rename = "type")]
    type_: String,
    #[serde(default)]
    summary: String,
}
#[derive(Deserialize)]
struct EspnBroadcast {
    #[serde(default)]
    market: String,
    #[serde(default)]
    names: Vec<String>,
}
#[derive(Deserialize)]
struct EspnTeam {
    #[serde(default)]
    abbreviation: String,
    #[serde(default, rename = "shortDisplayName")]
    short_display_name: String,
    /// The team's NICKNAME / mascot ("Fighting Illini", "Bruins", "Buckeyes")
    /// — ESPN's `name` field, distinct from `shortDisplayName` which is the
    /// SCHOOL/city for college ("Illinois"). Drives the `Name` style.
    #[serde(default)]
    name: String,
    /// City / college / country ("Boston", "Michigan", "France") — the string
    /// the `Location` name style renders. ESPN provides this on every
    /// competitor's team.
    #[serde(default)]
    location: String,
    #[serde(default)]
    color: String,
    #[serde(default, rename = "alternateColor")]
    alternate_color: String,
}

// ---- NHL power plays (api-web.nhle.com) ----
// ESPN's hockey scoreboard has no power-play info, so — exactly like the
// original scoreboard's server (hockey.py) — the live strength state comes from
// the NHL's own API: `powerPlay` + `numSkaters` on each side of the gamecenter
// boxscore. Games are matched to ESPN's by team abbreviation.

#[derive(Deserialize)]
struct NhlScoreNow {
    #[serde(default)]
    games: Vec<NhlScoreGame>,
}
#[derive(Deserialize)]
struct NhlScoreGame {
    id: i64,
    #[serde(default, rename = "gameState")]
    game_state: String,
    /// Present only on playoff games (gameType 3), absent entirely otherwise.
    #[serde(default, rename = "seriesStatus")]
    series_status: Option<NhlSeriesStatus>,
    #[serde(rename = "awayTeam")]
    away_team: NhlTeamRef,
    #[serde(rename = "homeTeam")]
    home_team: NhlTeamRef,
}
/// The NHL's series block. Win counts are keyed by SEED rather than by
/// home/away, so the abbrevs have to be matched to work out which number
/// belongs to which side of the board — the top seed is frequently the away
/// team.
#[derive(Deserialize)]
struct NhlSeriesStatus {
    #[serde(default, rename = "seriesAbbrev")]
    series_abbrev: String,
    #[serde(default, rename = "topSeedTeamAbbrev")]
    top_seed_abbrev: String,
    #[serde(default, rename = "topSeedWins")]
    top_seed_wins: u32,
    #[serde(default, rename = "bottomSeedWins")]
    bottom_seed_wins: u32,
}
#[derive(Deserialize)]
struct NhlTeamRef {
    #[serde(default)]
    abbrev: String,
    /// Present on boxscore teams while live; absent otherwise (like the
    /// original's `.get("powerPlay", False)`).
    #[serde(default, rename = "powerPlay")]
    power_play: bool,
    /// i64 so a malformed value can't abort the whole boxscore parse; clamped
    /// to u8 range on use (the original clamped negatives to 0 the same way).
    #[serde(default, rename = "numSkaters")]
    num_skaters: i64,
    /// Shots on goal, which sits next to `score` in the score feed and needed
    /// nothing but this line to reach the panel. Absent before puck drop, which
    /// is why it is optional rather than a defaulted 0.
    #[serde(default)]
    sog: Option<u32>,
}
#[derive(Deserialize)]
struct NhlBoxscore {
    #[serde(rename = "awayTeam")]
    away_team: NhlTeamRef,
    #[serde(rename = "homeTeam")]
    home_team: NhlTeamRef,
}

/// ESPN and the NHL API disagree on a few team abbreviations — normalize to
/// the NHL's, both for matching the two feeds and for the power-play badge
/// text (the original always displayed NHL-style abbrevs: TBL, not TB).
pub(crate) fn nhl_abbrev(a: &str) -> String {
    match a.to_ascii_uppercase().as_str() {
        "NJ" => "NJD".into(),
        "TB" => "TBL".into(),
        "LA" => "LAK".into(),
        "SJ" => "SJS".into(),
        "UTAH" => "UTA".into(),
        other => other.into(),
    }
}

impl EspnSource {
    /// Attach `Situation::Hockey` to live NHL games from the NHL API's
    /// boxscores. Any failure just means no power-play info this cycle.
    async fn attach_nhl_situations(&self, games: &mut [Game]) {
        if !games.iter().any(|g| g.sport_id == "nhl" && g.status == GameStatus::Active) {
            return;
        }
        let score_now: NhlScoreNow = match self
            .client
            .get("https://api-web.nhle.com/v1/score/now")
            .send()
            .await
            .and_then(|r| r.error_for_status())
        {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("nhl score/now parse: {e}");
                    return;
                }
            },
            Err(e) => {
                tracing::warn!("nhl score/now fetch: {e}");
                return;
            }
        };
        // Shots come off the score feed itself, so every game in it has them
        // for free — no per-game boxscore fetch, unlike the power-play state
        // below.
        let mut shots_by_teams = std::collections::HashMap::new();
        let mut series_by_teams = std::collections::HashMap::new();
        for g in score_now.games.iter() {
            let key = (nhl_abbrev(&g.away_team.abbrev), nhl_abbrev(&g.home_team.abbrev));
            shots_by_teams.insert(key.clone(), (g.away_team.sog, g.home_team.sog));
            // The wins are keyed by SEED, and the top seed is often the away
            // team, so they have to be matched by abbrev rather than assumed.
            if let Some(ss) = &g.series_status {
                let top = nhl_abbrev(&ss.top_seed_abbrev);
                let (away_w, home_w) = if top == key.0 {
                    (ss.top_seed_wins, ss.bottom_seed_wins)
                } else {
                    (ss.bottom_seed_wins, ss.top_seed_wins)
                };
                if !ss.series_abbrev.is_empty() {
                    series_by_teams
                        .insert(key.clone(), format!("{} {away_w}-{home_w}", ss.series_abbrev));
                }
            }
        }
        // (away, home) abbrev pair -> hockey situation, for live games only.
        let mut by_teams = std::collections::HashMap::new();
        for g in score_now.games.iter().filter(|g| matches!(g.game_state.as_str(), "LIVE" | "CRIT")) {
            let url = format!("https://api-web.nhle.com/v1/gamecenter/{}/boxscore", g.id);
            let bs: NhlBoxscore = match self.client.get(&url).send().await {
                Ok(r) => match r.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("nhl boxscore parse {}: {e}", g.id);
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!("nhl boxscore fetch {}: {e}", g.id);
                    continue;
                }
            };
            by_teams.insert(
                (nhl_abbrev(&g.away_team.abbrev), nhl_abbrev(&g.home_team.abbrev)),
                Situation::Hockey {
                    away_powerplay: bs.away_team.power_play,
                    home_powerplay: bs.home_team.power_play,
                    away_players: bs.away_team.num_skaters.clamp(0, 255) as u8,
                    home_players: bs.home_team.num_skaters.clamp(0, 255) as u8,
                },
            );
        }
        for g in games.iter_mut().filter(|g| g.sport_id == "nhl" && g.status == GameStatus::Active) {
            let key = (nhl_abbrev(&g.away.abbrev), nhl_abbrev(&g.home.abbrev));
            if let Some(sit) = by_teams.get(&key) {
                g.situation = Some(sit.clone());
            }
            if let Some((away, home)) = shots_by_teams.get(&key) {
                g.away.shots = *away;
                g.home.shots = *home;
            }
            if let Some(s) = series_by_teams.get(&key) {
                g.series = Some(s.clone());
            }
        }
    }
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
    /// Mascot/nickname ("Fighting Illini", "Bruins") — the `Name` style's
    /// source. ESPN's `name`, distinct from the school/city `shortDisplayName`.
    #[serde(default)]
    name: String,
    /// City / college / country — the `Location` name style's source (and the
    /// `server team-names` overflow report's).
    #[serde(default)]
    location: String,
    #[serde(default)]
    color: String,
    #[serde(default, rename = "alternateColor")]
    alternate_color: String,
}

fn parse_event(sport_id: &str, ev: Event) -> Option<Game> {
    let season = ev.season;
    let comp = ev.competitions.into_iter().next()?;
    let status = map_status(&comp.status.type_)?;
    let champ = classify_champ(
        sport_id,
        season.as_ref(),
        &comp.notes.iter().map(|n| n.headline.as_str()).collect::<Vec<_>>().join(" | "),
    );

    let mut home = None;
    let mut away = None;
    let mut home_id = String::new();
    let (mut home_record, mut away_record) = (String::new(), String::new());
    let overall = |c: &Competitor| {
        c.records
            .iter()
            .find(|r| r.type_ == "total")
            .or_else(|| c.records.first())
            .map(|r| r.summary.clone())
            .unwrap_or_default()
    };
    for c in comp.competitors {
        let primary = hex_to_rgb(if c.team.color.is_empty() { "222222" } else { &c.team.color });
        let secondary = ensure_contrast(
            primary,
            hex_to_rgb(if c.team.alternate_color.is_empty() { "FFFFFF" } else { &c.team.alternate_color }),
        );
        // The `Name` style is the mascot (ESPN `name`: "Fighting Illini"), not
        // the school/city (`shortDisplayName`: "Illinois"). Fall back to
        // shortDisplayName only if the feed omitted the mascot.
        let mascot = if c.team.name.is_empty() { &c.team.short_display_name } else { &c.team.name };
        let name = crate::teams::short_name(sport_id, &c.team.abbreviation, mascot);
        let side = TeamSide {
            abbrev: c.team.abbreviation.clone(),
            name,
            location: c.team.location.clone(),
            score: c.score.parse().unwrap_or(0),
            primary,
            secondary,
            hits: c.hits,
            errors: c.errors,
            // ESPN sends this as a JSON float on a two-leg tie and omits the
            // key entirely everywhere else.
            aggregate: c.aggregate_score.map(|v| v.round().max(0.0) as u32),
            // Filled in later from the NHL's own feed; ESPN has no shot count.
            shots: None,
            shootout: c.shootout_score,
        };
        match c.home_away.as_str() {
            "home" => {
                home_record = overall(&c);
                home = Some(side);
                home_id = c.id;
            }
            "away" => {
                away_record = overall(&c);
                away = Some(side);
            }
            _ => {}
        }
    }

    // Playoff series, from the wins ESPN keeps alongside the game. `series` is
    // ordered HOME FIRST, matching `competitors`, and is absent outside a
    // series — including on play-in games, which is why presence is the gate.
    let series = comp.series.as_ref().and_then(|s| {
        let (h, a) = (s.competitors.first()?.wins, s.competitors.get(1)?.wins);
        // Before game one there is nothing to report, and that state was never
        // observed, so it is excluded rather than rendered as "SERIES 0-0".
        (h + a > 0).then(|| format!("SERIES {a}-{h}"))
    });
    let mut ordinal = ordinal_for(&comp.status, status, sport_id);
    // The original called every extra NHL period "OT" (never "4th"/"5th").
    if sport_id == "nhl" && comp.status.period > 3 {
        ordinal = if status == GameStatus::Intermission { "OT INT".into() } else { "OT".into() };
    }
    let clock = if status == GameStatus::Active {
        comp.status.display_clock.clone()
    } else {
        String::new()
    };

    // Sport-specific live details — same ESPN fields the original scoreboard's
    // server read. Only attached while in progress (ESPN keeps a stale situation
    // object on finished games).
    // A live game with no situation object still gets the sport layout with
    // zeroed fields — the original's server defaulted every field the same way,
    // so the panel never flips between layouts mid-game when ESPN omits it.
    let empty_situation = EspnSituation::default();
    let situation = match (status, sport_id) {
        (GameStatus::Active, "nfl" | "cfb") => {
            let s = comp.situation.as_ref().unwrap_or(&empty_situation);
            Some(Situation::Football {
                // The original rewrote "&" to "+" because its 4x6 font had no
                // ampersand. Ours does — both BDFs ship an inked `ampersand`
                // glyph at the same 4px advance as the "+" that replaced it, so
                // the substitution cost a correct character and bought nothing.
                down_string: s.short_down_distance_text.clone(),
                ball_position: s.possession_text.clone(),
                home_possession: s.possession.as_ref().map(|p| *p == home_id),
            })
        }
        (GameStatus::Active, "mlb") => {
            let s = comp.situation.as_ref().unwrap_or(&empty_situation);
            Some(Situation::Baseball {
                balls: s.balls,
                strikes: s.strikes,
                outs: s.outs,
                on_first: s.on_first,
                on_second: s.on_second,
                on_third: s.on_third,
                is_inning_top: comp.status.type_.short_detail.contains("Top"),
            })
        }
        _ => None,
    };

    // Scheduled games trade their meaningless 0-0 score line for the records,
    // plus the national broadcast under the start time. National only — a
    // regional feed's name is usually wrong for whoever is looking at the board.
    let pregame = (status == GameStatus::Pregame).then(|| {
        let broadcast = comp
            .broadcasts
            .iter()
            .find(|b| b.market == "national")
            .and_then(|b| b.names.first())
            .map(|n| n.to_uppercase().chars().take(15).collect())
            .unwrap_or_default();
        Pregame { away_record, home_record, broadcast }
    })
    .filter(|p| *p != Pregame::default());

    Some(Game {
        id: comp.id,
        sport_id: sport_id.into(),
        home: home?,
        away: away?,
        status,
        ordinal,
        clock,
        start_time: comp.date,
        // Team-sport scoreboard events carry no endDate (games end within
        // hours; only multi-day leaderboard events get one).
        end_time: None,
        leaderboard: None,
        champ,
        series,
        session: None,
        situation,
        pregame,
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

    let slug = season.map(|s| s.slug.as_str()).unwrap_or("");

    match sport_id {
        "nhl" => {
            if h.contains("stanley cup final") {
                return tag("final", "Stanley Cup Final");
            }
            // ESPN does not write "conference final" — the feed says "East
            // Final - Game 3" / "West Final - Game 1". Matching only the formal
            // name made this checkbox dead for four straight postseasons: every
            // conference final fell through to the generic "playoffs" tier, so
            // a user who asked for conference finals got nothing and a user who
            // asked for earlier rounds got them anyway.
            if h.contains("conference final") || h.contains("east final") || h.contains("west final") {
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
                return tag("final", "WNBA Finals");
            }
        }
        // MLS never reports a postseason season type and never writes "MLS Cup"
        // in a headline: the round is in the SLUG ("mls-cup",
        // "eastern-conference-playoffs---semifinals"). Both of this league's
        // checkboxes were therefore dead. Order matters — the conference finals
        // are ".…playoffs---final", so the cup has to be settled first.
        "mls" => {
            if h.contains("mls cup") || slug.contains("mls-cup") {
                return tag("cup", "MLS Cup");
            }
            if slug.contains("playoffs") {
                return tag("playoffs", "MLS Playoffs");
            }
        }
        // The soccer tournaments all put the round in the season slug rather
        // than a headline, and they share one vocabulary.
        "wc" | "wwc" | "ucl" | "olysm" | "olysw" => {
            // Group stage / league phase gets no tag; "every match" covers it.
            return soccer_round(slug).map(|(tier, label)| Champ { tier: tier.into(), label: label.into() });
        }
        // Olympic hockey and basketball report EVERY game as season type 2,
        // "regular-season" — including the Gold Medal Game. The round exists
        // only in the headline ("Milano Cortina 2026 Men's Hockey - Gold Medal
        // Game"), which is why the generic "Postseason" checkbox these leagues
        // used to offer could never match a single game.
        "olyhm" | "olyhw" | "olybm" | "olybw" => {
            if h.contains("gold medal") {
                return tag("gold", "Gold Medal Game");
            }
            if h.contains("bronze medal") {
                return tag("bronze", "Bronze Medal Game");
            }
            if h.contains("semifinal") {
                return tag("semi", "Semifinal");
            }
            if h.contains("quarterfinal") {
                return tag("quarter", "Quarterfinal");
            }
            return None; // preliminary round / qualifications
        }
        // College baseball's postseason runs across FOUR season types: regionals
        // (3), super regionals (4), the College World Series (5) and the CWS
        // championship series (6). Only type 3 passes the generic postseason
        // gate, so "Postseason" used to mean "regionals" and silently dropped
        // the College World Series itself.
        "cbase" => {
            if slug.contains("world-series") || slug.contains("championship-series") {
                return tag("cws", "College World Series");
            }
            if slug.contains("regional") {
                return tag("playoffs", "NCAA Regionals");
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
            // ESPN files the Pro Bowl as season type 3, so the postseason
            // fallback below would call an all-star exhibition a playoff game
            // and put AFC vs NFC in front of anyone following "earlier playoff
            // rounds". Untagged, it still reaches a board via "Every game",
            // which is the honest subscription for a game that decides nothing.
            if h.contains("pro bowl") {
                return None;
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
        // ESPN writes the NCAA tournament as "Men's Basketball Championship -
        // South Region - 1st Round" and (through 2025) "NCAA Women's
        // Championship - Regional 1 in Spokane - 1st Round". Neither contains
        // the word "tournament", so requiring it made March Madness — the
        // single most likely thing anyone subscribes to — unmatchable: 0 of 626
        // events in 2026, and none in any season checked back to 2019.
        //
        // Both halves of the test are load-bearing:
        //  * the postseason gate excludes the CONFERENCE tournaments, whose
        //    headlines also say "Championship" ("Phillips 66 Big 12
        //    Championship") but which ESPN files as regular season;
        //  * the wording excludes the NIT, WBIT and "College Basketball Crown
        //    Championship Game", which do share season type 3.
        "cbb" | "wcbb" => {
            let ncaa_tournament = h.contains("basketball championship")
                || (h.contains("ncaa") && h.contains("championship"));
            if postseason && ncaa_tournament {
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

/// The round a soccer tournament's season slug names, shared by the World Cup,
/// the Women's World Cup, the Champions League and Olympic soccer.
///
/// Order is not stylistic: "quarterfinals" and "semifinals" both END in
/// "final", so the specific rounds have to be settled before the Final is.
fn soccer_round(slug: &str) -> Option<(&'static str, &'static str)> {
    // Olympic soccer decides medals rather than a trophy.
    if slug.contains("gold-medal") {
        return Some(("gold", "Gold Medal Match"));
    }
    if slug.contains("bronze-medal") {
        return Some(("bronze", "Bronze Medal Match"));
    }
    // ESPN spells this "3rd-place" (World Cup) and "3rd-place-match" (2022).
    // The old code tested for "third-place" and matched neither, so the
    // bronze-medal game of a World Cup went untagged.
    if slug.contains("3rd-place") || slug.contains("third-place") {
        return Some(("third", "Third-Place Match"));
    }
    if slug.contains("semifinal") {
        return Some(("semi", "Semifinal"));
    }
    if slug.contains("quarterfinal") {
        return Some(("quarter", "Quarterfinal"));
    }
    // "round-of-16" plus the Champions League's "knockout-round-playoffs".
    if slug.contains("round-of") || slug.contains("knockout") {
        return Some(("knockout", "Knockout Round"));
    }
    if slug.contains("final") {
        return Some(("final", "Final"));
    }
    None
}

/// `None` means "don't put this on the board at all".
///
/// The name alone is not enough. STATUS_SUSPENDED covers both a rain delay that
/// resumes in twenty minutes and a golf round called for darkness that resumes
/// TOMORROW MORNING — and we were calling both of them live, so a suspended
/// tournament sat on the panel all night. ESPN distinguishes them in `state`:
/// a pause within the game keeps `state: "in"`, while play stopping for the day
/// flips it to `"post"`. So anything ESPN has moved to "post" without also
/// naming it finished has stopped without finishing: drop it until the feed
/// puts it back in play.
fn map_status(t: &StatusType) -> Option<GameStatus> {
    let status = match t.name.as_str() {
        "STATUS_SCHEDULED" => GameStatus::Pregame,
        // Soccer runs through halves, extra time, and shootouts as distinct states.
        "STATUS_IN_PROGRESS" | "STATUS_FIRST_HALF" | "STATUS_SECOND_HALF"
        | "STATUS_OVERTIME" | "STATUS_EXTRA_TIME" | "STATUS_SHOOTOUT" => GameStatus::Active,
        "STATUS_HALFTIME" | "STATUS_END_PERIOD" | "STATUS_DELAYED" | "STATUS_SUSPENDED"
        | "STATUS_HALFTIME_ET" | "STATUS_END_OF_EXTRATIME" => GameStatus::Intermission,
        // SESSION_COMPLETE is ESPN's transitional post-chequered racing
        // status (state still "in"!) before the official STATUS_FINAL —
        // observed live at the 2026 Dutch GP, where the catch-all turned a
        // just-finished race into a PREGAME frame.
        "STATUS_FINAL" | "STATUS_PLAY_COMPLETE" | "STATUS_FULL_TIME" | "STATUS_FINAL_PEN"
        | "STATUS_FINAL_AET" | "STATUS_SESSION_COMPLETE" => GameStatus::Final,
        "STATUS_POSTPONED" | "STATUS_CANCELED" | "STATUS_ABANDONED" => return None,
        _ => GameStatus::Pregame,
    };
    if t.state == "post" && status != GameStatus::Final {
        return None;
    }
    Some(status)
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

/// The label for a period past regulation, from the period number alone.
///
/// Only used when ESPN does not send `altDetail`, which so far means live games
/// — every final that went past regulation carries it. `None` means "no idea,
/// print the plain ordinal", and NHL past the first overtime is deliberately
/// one of those: period 5 is a shootout in the regular season and 2OT in the
/// playoffs, and the period number cannot tell you which.
fn overtime_label(sport_id: &str, period: i64) -> Option<String> {
    let regulation = match sport_id {
        "nhl" | "olyhm" | "olyhw" => 3,
        "nba" | "wnba" | "olybm" | "olybw" | "nfl" | "cfb" => 4,
        "cbb" | "wcbb" => 2, // halves
        // Baseball counts innings forever: a 12th is a 12th, not a 3rd OT.
        _ if is_soccer(sport_id) => 2,
        _ => return None,
    };
    let extra = period - regulation;
    if extra <= 0 {
        return None;
    }
    if is_soccer(sport_id) {
        // Two extra-time halves, then the shootout.
        return Some(if extra >= 3 { "PENS".into() } else { "ET".into() });
    }
    match (sport_id, extra) {
        ("nhl" | "olyhm" | "olyhw", 1) => Some("OT".into()),
        // Ambiguous: SO or 2OT depending on the season. Let `altDetail` say.
        ("nhl" | "olyhm" | "olyhw", _) => None,
        (_, 1) => Some("OT".into()),
        (_, n) => Some(format!("{n}OT")),
    }
}

fn ordinal_for(status: &Status, gs: GameStatus, sport_id: &str) -> String {
    if status.type_.name == "STATUS_HALFTIME" {
        return "HALF".into();
    }
    if status.period <= 0 {
        return String::new();
    }
    // A game past regulation used to print the raw period: NBA overtime read
    // "5TH", college basketball "3RD", and a soccer shootout "5TH". ESPN's own
    // label first, then the period arithmetic, then the ordinal it always was.
    let o = status
        .type_
        .alt_detail
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_uppercase())
        .or_else(|| overtime_label(sport_id, status.period))
        .unwrap_or_else(|| ordinal(status.period));
    if gs == GameStatus::Intermission {
        format!("{o} INT")
    } else {
        o
    }
}

#[cfg(test)]
mod espn_url_tests {
    use super::espn_game_url;

    #[test]
    fn builds_game_pages_per_sport_and_skips_the_rest() {
        // Team sports -> /game/; verified-live formats.
        assert_eq!(
            espn_game_url("nhl", "401881922").as_deref(),
            Some("https://www.espn.com/nhl/game/_/gameId/401881922")
        );
        assert_eq!(
            espn_game_url("cfb", "401856766").as_deref(),
            Some("https://www.espn.com/college-football/game/_/gameId/401856766")
        );
        assert_eq!(
            espn_game_url("cbb", "123").as_deref(),
            Some("https://www.espn.com/mens-college-basketball/game/_/gameId/123")
        );
        // Soccer -> /soccer/match/, regardless of the league.
        assert_eq!(
            espn_game_url("mls", "761685").as_deref(),
            Some("https://www.espn.com/soccer/match/_/gameId/761685")
        );
        assert_eq!(
            espn_game_url("epl", "761685").as_deref(),
            Some("https://www.espn.com/soccer/match/_/gameId/761685")
        );
        // Leaderboard sports DO have pages — just not /gameId/ ones (see
        // `leaderboard_sports_get_espn_links` in main.rs for those forms).
        assert!(espn_game_url("f1", "401881922-race").is_some());
        assert!(espn_game_url("pga", "401580328").is_some());
        // Non-numeric ids (demo games) still can't form a valid link.
        assert_eq!(espn_game_url("nhl", "demo-nhl"), None);
        assert_eq!(espn_game_url("pga", "demo-golf"), None);
        assert_eq!(espn_game_url("nhl", ""), None);
        assert_eq!(espn_game_url("unknown", "123"), None);
    }
}

#[cfg(test)]
mod leader_tests {
    use std::collections::HashMap;

    /// The gap extraction reads the exact JSON shape ESPN's core API served
    /// during the 2026 Dutch GP (captured live). The leader carries no gap
    /// stat and must stay blank; "Lap" spellings tighten for the panel.
    #[test]
    fn f1_gap_extraction_matches_the_core_api_shape() {
        let trailing: CoreStatistics = serde_json::from_str(
            r#"{"splits":{"categories":[{"stats":[
                {"name":"lapsCompleted","displayValue":"15"},
                {"name":"place","displayValue":"3"},
                {"name":"gapToLeader","displayValue":"+0.287"},
                {"name":"lastUpdated","displayValue":"2026-08-22T14:49:06Z"}
            ]}]}}"#,
        )
        .unwrap();
        assert_eq!(extract_gap(&trailing).as_deref(), Some("+0.29"), "hundredths on the panel");

        let leader: CoreStatistics = serde_json::from_str(
            r#"{"splits":{"categories":[{"stats":[
                {"name":"lapsCompleted","displayValue":"16"},
                {"name":"place","displayValue":"1"}
            ]}]}}"#,
        )
        .unwrap();
        assert_eq!(extract_gap(&leader), None, "the leader keeps a blank column");

        let lapped: CoreStatistics = serde_json::from_str(
            r#"{"splits":{"categories":[{"stats":[{"name":"behindLaps","displayValue":"+2 Laps"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(extract_gap(&lapped).as_deref(), Some("+2L"), "lap gaps tighten for 64px");

        let zeroed: CoreStatistics = serde_json::from_str(
            r#"{"splits":{"categories":[{"stats":[{"name":"gapToLeader","displayValue":"0.000"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(extract_gap(&zeroed), None, "a zero gap is the leader, not data");
    }

    /// Only RACES get the per-driver fan-out — the grand prix and the sprint,
    /// live or final. Qualifying is excluded on purpose: ESPN resets its gaps
    /// per Q1/Q2/Q3 segment while positions span the hour, so the numbers
    /// contradict the order they sit next to.
    #[test]
    fn f1_timing_fanout_covers_every_shown_session() {
        let comp = |abbr: &str, status: &str| -> LeaderComp {
            serde_json::from_str(&format!(
                r#"{{"id":"401839097","type":{{"abbreviation":"{abbr}"}},
                    "status":{{"type":{{"name":"{status}"}}}},"competitors":[]}}"#,
            ))
            .unwrap()
        };
        assert!(f1_session_wants_timing(&comp("Race", "STATUS_IN_PROGRESS")));
        assert!(f1_session_wants_timing(&comp("Race", "STATUS_FINAL")));
        assert!(f1_session_wants_timing(&comp("SR", "STATUS_IN_PROGRESS")), "a sprint is a race");
        assert!(!f1_session_wants_timing(&comp("Race", "STATUS_SCHEDULED")), "nothing to fetch pregame");
        assert!(f1_session_wants_timing(&comp("Qual", "STATUS_IN_PROGRESS")), "quali: position + segment times");
        assert!(f1_session_wants_timing(&comp("SS", "STATUS_IN_PROGRESS")), "sprint quali too");
        assert!(!f1_session_wants_timing(&comp("FP1", "STATUS_IN_PROGRESS")), "practice never renders");
    }

    /// The segment reads from what ESPN has PUBLISHED: a driver's Q1 time
    /// appears once they are out of Q1, so any Q1 time means Q2 is on.
    #[test]
    fn quali_stage_reads_from_published_segment_times() {
        let t = |q1: Option<&str>, q2: Option<&str>| F1Timing {
            qual_time: [q1.map(String::from), q2.map(String::from), None],
            ..Default::default()
        };
        assert_eq!(quali_stage([t(None, None), t(None, None)].iter()), 1);
        assert_eq!(quali_stage([t(None, None), t(Some("1:35.312"), None)].iter()), 2);
        assert_eq!(quali_stage([t(Some("1:35.312"), None), t(None, Some("1:34.001"))].iter()), 3);
        assert_eq!(quali_stage(std::iter::empty()), 1, "nothing published yet: Q1");
    }

    /// Q2 of the 2026 Spanish GP, verbatim shapes: the field's gaps are to
    /// the Q2 leader, a driver without a Q2 lap still carries a SMALLER Q1
    /// gap (blanked), and the Q1-eliminated carry the lap time that put
    /// them out (kept — the renderer greys it).
    #[test]
    fn live_quali_values_blank_stale_gaps_and_keep_out_times() {
        let mk = |rank: i64, gap: Option<&str>, q1: Option<&str>| {
            (
                rank,
                LeaderRow { rank: rank.to_string(), value: gap.unwrap_or("").into(), ..Default::default() },
                F1Timing {
                    gap: gap.map(String::from),
                    position: Some(rank),
                    qual_time: [q1.map(String::from), None, None],
                },
            )
        };
        let mut drivers = vec![
            mk(1, None, None),
            mk(2, Some("+0.16"), None),
            mk(3, Some("+2.98"), None),
            mk(4, Some("+0.75"), None), // Q1 gap: no Q2 lap yet
            mk(5, Some("+3.10"), None), // back on Q2 gaps (leader-relative)
            mk(6, Some("+2.10"), Some("1:35.312")), // out in Q1
            mk(7, Some("+2.18"), None), // out in Q1, time not published yet
        ];
        // A five-car "field" so the cut lands inside the fixture: safe 3, field 5.
        let zones = Some(crate::model::QualiZones { safe: 3, field: 5 });
        quali_values(&mut drivers, zones, true);
        let v: Vec<&str> = drivers.iter().map(|(_, r, _)| r.value.as_str()).collect();
        assert_eq!(v, ["", "+0.16", "+2.98", "", "+3.10", "1:35.31", "+2.18"]);

        // Finished: lap times from the last segment each driver ran.
        let mut done = vec![mk(1, None, Some("1:34.100")), mk(2, Some("+0.30"), None)];
        quali_values(&mut done, None, false);
        assert_eq!(done[0].1.value, "1:34.10");
        assert_eq!(done[1].1.value, "", "nothing published: blank, not a stale gap");
    }

    /// A live quali orders by the timing screen (which the gaps are relative
    /// to), carries the segment and its zones, and falls back to the
    /// scoreboard order when the fan-out gave nothing.
    #[test]
    fn live_quali_orders_by_timing_position_and_carries_the_segment() {
        let ev = || -> LeaderEvent {
            serde_json::from_str(
                r#"{"id":"600057443","shortName":"Spanish GP","competitions":[{
                    "id":"401839106",
                    "type":{"abbreviation":"Qual"},
                    "status":{"type":{"name":"STATUS_IN_PROGRESS"}},
                    "competitors":[
                        {"id":"4665","order":1,"athlete":{"displayName":"Max Verstappen"}},
                        {"id":"5503","order":2,"athlete":{"displayName":"George Russell"}},
                        {"id":"868","order":3,"athlete":{"displayName":"Lewis Hamilton"}}
                    ]}]}"#,
            )
            .unwrap()
        };
        let mut timing = HashMap::new();
        let t = |gap: Option<&str>, pos: i64, q1: Option<&str>| F1Timing {
            gap: gap.map(String::from),
            position: Some(pos),
            qual_time: [q1.map(String::from), None, None],
        };
        timing.insert(("401839106".into(), "4665".into()), t(None, 1, None));
        timing.insert(("401839106".into(), "5503".into()), t(Some("+0.29"), 3, None));
        timing.insert(("401839106".into(), "868".into()), t(Some("+0.15"), 2, Some("1:35.000")));
        let g = parse_f1(ev(), &timing).remove(0);
        let lb = g.leaderboard.unwrap();
        let names: Vec<&str> = lb.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["VER", "HAM", "RUS"], "timing position, not scoreboard order");
        assert_eq!(lb.rows[1].rank, "2", "rank follows the timing position");
        assert_eq!(lb.stage, Some(2), "a published Q1 time means Q2");
        assert_eq!(lb.name, "SPANISH GP Q", "the digit is the renderer's to add");
        assert!(lb.quali.is_none(), "three cars is no qualifying format");

        let g = parse_f1(ev(), &HashMap::new()).remove(0);
        let lb = g.leaderboard.unwrap();
        let names: Vec<&str> = lb.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["VER", "RUS", "HAM"], "no timing: scoreboard order");
        assert_eq!(lb.stage, None, "no timing: no segment claimed");
    }

    /// The statistics resource, 2026 Spanish GP Q2 verbatim for a driver
    /// out in Q1: position, gap, and the Q1 time all come through.
    #[test]
    fn f1_timing_reads_position_and_segment_times() {
        let stats: CoreStatistics = serde_json::from_str(
            r#"{"splits":{"categories":[
                {"name":"general","stats":[
                    {"name":"place","displayValue":"17"},
                    {"name":"qual1TimeMS","displayValue":"1:35.312"},
                    {"name":"qual2TimeMS","displayValue":"0.000"},
                    {"name":"qual3TimeMS","displayValue":"0.000"}]},
                {"name":"gapToLeader","stats":[
                    {"name":"position","displayValue":"17"},
                    {"name":"gapToLeader","displayValue":"+2.101"}]}]}}"#,
        )
        .unwrap();
        let t = f1_timing(&stats);
        assert_eq!(t.gap.as_deref(), Some("+2.10"));
        assert_eq!(t.position, Some(17));
        assert_eq!(t.qual_time, [Some("1:35.312".to_string()), None, None]);
        assert_eq!(fit_lap_time("1:35.312"), "1:35.31");
        assert_eq!(fit_lap_time("DNF"), "DNF", "non-times pass through");
    }

    /// Accented surnames code as the FIA does: HUL and PER, not HLK / PRZ.
    #[test]
    fn driver_codes_fold_diacritics() {
        assert_eq!(f1_code("Nico Hülkenberg"), "HUL");
        assert_eq!(f1_code("Sergio Pérez"), "PER");
        assert_eq!(f1_code("Max Verstappen"), "VER");
        assert!(f1_team_color("HUL").is_some(), "and therefore get their team colour");
    }

    /// Render a CAPTURED ESPN snapshot (the scoreboard plus one statistics
    /// file per driver, as `design/f1-logs/live/<ts>/` holds them) to PNGs,
    /// one per session, at the board's full row depth — the way to see what
    /// a live session looked like without a server or a session in progress:
    ///
    ///   F1_FIXTURE_DIR=design/f1-logs/live/20260912T144011Z \
    ///   F1_FIXTURE_OUT=/tmp/f1 cargo test -p server render_f1_fixture -- --ignored
    #[test]
    #[ignore]
    fn render_f1_fixture() {
        let dir = std::env::var("F1_FIXTURE_DIR").expect("F1_FIXTURE_DIR");
        let out = std::env::var("F1_FIXTURE_OUT").unwrap_or_else(|_| dir.clone());
        std::fs::create_dir_all(&out).unwrap();
        let games = replay_f1_snapshot(std::path::Path::new(&dir));
        assert!(!games.is_empty(), "no F1 games in {dir}/scoreboard.json");
        for g in games {
            let (_, px, _) = crate::render::render_board_frame(
                &g, crate::render::LEADERBOARD_MAX_ROWS, None, None, scoreboard_protocol::NameStyle::Name,
            );
            let path = format!("{out}/{}.png", g.id);
            std::fs::write(&path, crate::render_png(&px)).unwrap();
            eprintln!("wrote {path} ({})", g.leaderboard.as_ref().map(|l| l.name.as_str()).unwrap_or(""));
        }
    }

    /// ESPN's post-chequered racing status, observed live: SESSION_COMPLETE
    /// with state still "in". Unmapped it fell to the Pregame catch-all and
    /// a finished race rendered as a matchup with a start time.
    #[test]
    fn a_completed_session_is_final_not_pregame() {
        let t: StatusType = serde_json::from_str(r#"{"name":"STATUS_SESSION_COMPLETE"}"#).unwrap();
        assert!(matches!(map_status(&t), Some(GameStatus::Final)));
    }

    /// ESPN carries only gap-to-leader, so race rows derive the interval to
    /// the car ahead by subtracting adjacent gaps. Lapped cars keep their
    /// string and drop out of the arithmetic.
    #[test]
    fn espn_race_rows_derive_intervals_from_adjacent_gaps() {
        let row = |value: &str| LeaderRow { value: value.into(), ..Default::default() };
        let mut rows = vec![row(""), row("+1.804"), row("+4.799"), row("+2L"), row("+30.100")];
        derive_intervals(&mut rows);
        assert_eq!(rows[0].value, "", "leader stays blank");
        assert_eq!(rows[1].value, "+1.80", "P2's interval IS its gap");
        assert_eq!(rows[2].value, "+3.00", "P3: 4.799 - 1.804, hundredths");
        assert_eq!(rows[3].value, "+2L", "lapped string untouched");
        assert_eq!(rows[4].value, "+25.30", "the car behind a lapped one subtracts the last numeric gap");
    }

    /// parse_f1 joins fetched gaps onto rows by (competition, competitor) id;
    /// drivers the fetch missed keep the blank value they always had.
    #[test]
    fn f1_rows_carry_fetched_gaps() {
        let ev: LeaderEvent = serde_json::from_str(
            r#"{"id":"600057441","shortName":"Dutch GP","competitions":[{
                "id":"401839097",
                "type":{"abbreviation":"Race"},
                "status":{"type":{"name":"STATUS_IN_PROGRESS"}},
                "competitors":[
                    {"id":"4665","order":1,"athlete":{"displayName":"Max Verstappen"}},
                    {"id":"5829","order":2,"athlete":{"displayName":"Kimi Antonelli"}},
                    {"id":"4472","order":3,"athlete":{"displayName":"Lando Norris"}}
                ]}]}"#,
        )
        .unwrap();
        let mut gaps = HashMap::new();
        gaps.insert(
            ("401839097".to_string(), "5829".to_string()),
            F1Timing { gap: Some("+0.29".into()), ..Default::default() },
        );
        let games = parse_f1(ev, &gaps);
        assert_eq!(games.len(), 1);
        let rows = &games[0].leaderboard.as_ref().unwrap().rows;
        assert_eq!(rows[0].value, "", "leader blank");
        assert_eq!(rows[1].value, "+0.29", "fetched gap lands on the right driver");
        assert_eq!(rows[2].value, "", "a missed fetch degrades to blank, not an error");
    }

    use super::*;

    /// An F1 weekend fans out into per-session games (practice skipped), each
    /// with its own id, status, schedule, and standings.
    #[test]
    fn f1_emits_one_game_per_subscribable_session() {
        let json = r#"{
            "id": "evt1",
            "shortName": "Pirelli British GP",
            "competitions": [
                {"date":"2026-07-03T11:30Z","status":{"type":{"name":"STATUS_FINAL"}},
                 "type":{"abbreviation":"FP1"},
                 "competitors":[{"order":1,"athlete":{"displayName":"Practice Driver"}}]},
                {"date":"2026-07-04T15:00Z","status":{"type":{"name":"STATUS_FINAL"}},
                 "type":{"abbreviation":"Qual"},
                 "competitors":[{"order":1,"athlete":{"displayName":"Lando Norris"}}]},
                {"date":"2026-07-05T14:00Z","status":{"type":{"name":"STATUS_IN_PROGRESS"}},
                 "type":{"abbreviation":"Race"},
                 "competitors":[
                    {"order":1,"athlete":{"displayName":"Max Verstappen"}},
                    {"order":2,"athlete":{"displayName":"Lando Norris"}}
                 ]}
            ]
        }"#;
        let ev: LeaderEvent = serde_json::from_str(json).expect("deserialize event");
        let games = parse_f1(ev, &HashMap::new());
        assert_eq!(games.len(), 2, "practice is skipped; quali + race emitted");

        let quali = games.iter().find(|g| g.session.as_deref() == Some("quali")).unwrap();
        assert_eq!(quali.id, "evt1-quali");
        assert_eq!(quali.status, GameStatus::Final);
        assert!(quali.leaderboard.as_ref().unwrap().name.ends_with(" Q"), "quali title suffixed");

        let race = games.iter().find(|g| g.session.as_deref() == Some("race")).unwrap();
        assert_eq!(race.id, "evt1-race");
        assert_eq!(race.status, GameStatus::Active);
        assert_eq!(race.leaderboard.as_ref().unwrap().name, "BRITISH GP");
        // Broadcast style: rank column + three-letter code + team color.
        let rows = &race.leaderboard.as_ref().unwrap().rows;
        assert_eq!(rows.first().map(|r| (r.rank.as_str(), r.name.as_str())), Some(("1", "VER")));
        assert_eq!(rows.get(1).map(|r| r.name.as_str()), Some("NOR"));
        assert!(rows.first().unwrap().color.is_some(), "VER maps to a team color");
    }

    /// MAR-116: a race or sprint at a known circuit carries the leader's
    /// lap count (status `period`) against the scheduled distance; quali
    /// and a race that has not started carry nothing, and so does a
    /// circuit the table does not know.
    #[test]
    fn f1_race_and_sprint_carry_lap_progress() {
        use crate::model::LapProgress;
        let json = r#"{
            "id": "evt2",
            "shortName": "Pirelli British GP",
            "circuit": {"fullName": "Silverstone Circuit"},
            "competitions": [
                {"date":"2026-07-04T15:00Z","status":{"type":{"name":"STATUS_IN_PROGRESS"},"period":9},
                 "type":{"abbreviation":"Qual"},
                 "competitors":[{"order":1,"athlete":{"displayName":"Lando Norris"}}]},
                {"date":"2026-07-04T11:00Z","status":{"type":{"name":"STATUS_FINAL"},"period":17},
                 "type":{"abbreviation":"SR"},
                 "competitors":[{"order":1,"athlete":{"displayName":"Lando Norris"}}]},
                {"date":"2026-07-05T14:00Z","status":{"type":{"name":"STATUS_IN_PROGRESS"},"period":23},
                 "type":{"abbreviation":"Race"},
                 "competitors":[{"order":1,"athlete":{"displayName":"Max Verstappen"}}]}
            ]
        }"#;
        let ev: LeaderEvent = serde_json::from_str(json).unwrap();
        let games = parse_f1(ev, &HashMap::new());
        let laps = |s: &str| games.iter().find(|g| g.session.as_deref() == Some(s)).unwrap().leaderboard.as_ref().unwrap().laps;
        assert_eq!(laps("race"), Some(LapProgress { completed: 23, total: 52 }));
        assert_eq!(laps("sprint"), Some(LapProgress { completed: 17, total: 17 }));
        assert_eq!(laps("quali"), None, "qualifying has no distance");

        // Not started: period 0 and a scheduled status — nothing to draw yet.
        let pre = json.replace(r#""name":"STATUS_IN_PROGRESS"},"period":23"#, r#""name":"STATUS_SCHEDULED"},"period":0"#);
        let ev: LeaderEvent = serde_json::from_str(&pre).unwrap();
        let games = parse_f1(ev, &HashMap::new());
        let race = games.iter().find(|g| g.session.as_deref() == Some("race")).unwrap();
        assert_eq!(race.leaderboard.as_ref().unwrap().laps, None);

        // An unknown circuit: no bar rather than a guess.
        let odd = json.replace("Silverstone Circuit", "Brands Hatch");
        let ev: LeaderEvent = serde_json::from_str(&odd).unwrap();
        let games = parse_f1(ev, &HashMap::new());
        let race = games.iter().find(|g| g.session.as_deref() == Some("race")).unwrap();
        assert_eq!(race.leaderboard.as_ref().unwrap().laps, None);
    }

    /// Multi-day leaderboard events carry ESPN's event-level endDate as
    /// end_time: golf tournaments always; F1 weekends on the RACE session only
    /// (the weekend ends when the race does — quali doesn't run to Sunday).
    /// Events without one (and team-sport games, which have no such field)
    /// stay None.
    #[test]
    fn leaderboard_events_carry_end_dates() {
        // Shape verified against the live feeds: golf/F1 events have
        // "endDate"; team-sport scoreboard events do not.
        let golf = r#"{
            "id": "401",
            "shortName": "Travelers Championship",
            "endDate": "2026-07-12T04:00Z",
            "competitions": [
                {"date":"2026-07-09T04:00Z","status":{"type":{"name":"STATUS_IN_PROGRESS"}},
                 "competitors":[{"order":1,"athlete":{"displayName":"Scottie Scheffler"},
                                 "status":{"position":{"id":1}},
                                 "statistics":[{"displayValue":"-8"}]}]}
            ]
        }"#;
        let ev: LeaderEvent = serde_json::from_str(golf).unwrap();
        let g = parse_golf("pga", ev).unwrap();
        assert_eq!(g.end_time.as_deref(), Some("2026-07-12T04:00Z"));
        assert_eq!(g.start_time, "2026-07-09T04:00Z");

        // No endDate in the feed -> None (never an empty string).
        let ev: LeaderEvent =
            serde_json::from_str(&golf.replace("\"endDate\": \"2026-07-12T04:00Z\",", "")).unwrap();
        assert_eq!(parse_golf("pga", ev).unwrap().end_time, None);

        let f1 = r#"{
            "id": "evt1",
            "shortName": "British GP",
            "endDate": "2026-07-19T13:00Z",
            "competitions": [
                {"date":"2026-07-18T15:00Z","status":{"type":{"name":"STATUS_FINAL"}},
                 "type":{"abbreviation":"Qual"},
                 "competitors":[{"order":1,"athlete":{"displayName":"Lando Norris"}}]},
                {"date":"2026-07-19T14:00Z","status":{"type":{"name":"STATUS_SCHEDULED"}},
                 "type":{"abbreviation":"Race"},
                 "competitors":[]}
            ]
        }"#;
        let ev: LeaderEvent = serde_json::from_str(f1).unwrap();
        let games = parse_f1(ev, &HashMap::new());
        let race = games.iter().find(|g| g.session.as_deref() == Some("race")).unwrap();
        assert_eq!(race.end_time.as_deref(), Some("2026-07-19T13:00Z"));
        let quali = games.iter().find(|g| g.session.as_deref() == Some("quali")).unwrap();
        assert_eq!(quali.end_time, None, "the weekend end is not quali's end");
    }

    /// Golf majors get the "major" champ tier; regular tour stops don't.
    #[test]
    fn golf_majors_are_classified() {
        assert_eq!(golf_major_label("pga", "The Masters"), Some("The Masters"));
        assert_eq!(golf_major_label("pga", "Travelers Championship"), None);
        assert_eq!(golf_major_label("lpga", "KPMG Women's PGA Championship"), Some("Women's PGA"));
        assert_eq!(golf_major_label("lpga", "Portland Classic"), None);
    }

    /// World Cup rounds classify from the season slug; group stage stays untagged.
    #[test]
    fn world_cup_rounds_classify_from_slug() {
        let season = |slug: &str| EventSeason { type_: 13800, slug: slug.into() };
        assert_eq!(classify_champ("wc", Some(&season("round-of-16")), "").unwrap().tier, "knockout");
        assert_eq!(classify_champ("wc", Some(&season("quarterfinals")), "").unwrap().tier, "quarter");
        assert_eq!(classify_champ("wc", Some(&season("semifinals")), "").unwrap().tier, "semi");
        assert_eq!(classify_champ("wc", Some(&season("final")), "").unwrap().tier, "final");
        assert_eq!(classify_champ("wc", Some(&season("group-stage")), ""), None);
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

    /// VERBATIM strings from ESPN's live feeds, each with the tier it must
    /// produce. `None` means "no tier — only the blanket option catches it".
    ///
    /// Everything here is copied out of a real response, not written by hand,
    /// because hand-written fixtures are what let five checkboxes ship dead.
    /// The NHL test below used to assert on "West Conference Finals - Game 5",
    /// a headline ESPN has never sent; the real feed says "West Final - Game 1",
    /// so the Conference Finals option matched nothing for four straight
    /// postseasons while its test stayed green.
    ///
    /// (sport_id, season type, season slug, notes headline, expected tier)
    const REAL_FEED_CASES: &[(&str, i64, &str, &str, Option<&str>)] = &[
        // --- NHL: the feed names the conference by side, not by "conference".
        ("nhl", 3, "post-season", "West Final - Game 1", Some("conf")),
        ("nhl", 3, "post-season", "East Final - Game 3", Some("conf")),
        ("nhl", 3, "post-season", "East 2nd Round - Game 5", Some("playoffs")),
        ("nhl", 3, "post-season", "Stanley Cup Final - Game 3", Some("final")),
        // --- The leagues that already worked, pinned so they keep working.
        ("nba", 3, "post-season", "NBA Finals - Game 7", Some("final")),
        ("nba", 3, "post-season", "West Finals - Game 1", Some("playoffs")),
        ("nba", 2, "regular-season", "", None),
        ("wnba", 3, "post-season", "WNBA Finals - Game 4", Some("final")),
        ("wnba", 3, "post-season", "WNBA Semifinals - Game 2", Some("playoffs")),
        ("mlb", 3, "post-season", "World Series - Game 5", Some("ws")),
        ("mlb", 3, "post-season", "ALWC - Game 1", Some("playoffs")),
        ("nfl", 3, "post-season", "Super Bowl LIX", Some("sb")),
        ("nfl", 3, "post-season", "AFC Wild Card Playoffs", Some("playoffs")),
        // Season type 3, but an exhibition — the one NFL type-3 event that is
        // not a playoff game.
        ("nfl", 3, "post-season", "Pro Bowl Games", None),
        ("cfb", 3, "post-season", "College Football Playoff First Round Presented by Allstate", Some("playoff")),
        ("cfb", 3, "post-season", "IS4S Salute to Veterans Bowl", Some("bowl")),
        ("cfb", 3, "post-season", "FCS Championship - Semifinals", Some("playoffs")),
        // Conference championship games are regular season to ESPN, so only the
        // blanket option ever catches them.
        ("cfb", 2, "regular-season", "Big Ten Championship", None),
        // The Premier League has no postseason season type at all.
        ("epl", 12654, "2024-25-english-premier-league", "", None),
        // --- College basketball: the NCAA tournament never says "tournament",
        // and three other type-3 events must NOT be mistaken for it.
        ("cbb", 3, "post-season", "Men's Basketball Championship - South Region - 1st Round", Some("tournament")),
        ("cbb", 3, "post-season", "Men's Basketball Championship - Final Four", Some("tournament")),
        ("cbb", 3, "post-season", "Men's Basketball Championship - National Championship", Some("tournament")),
        ("cbb", 3, "post-season", "NIT - 1st Round", Some("playoffs")),
        ("cbb", 3, "post-season", "College Basketball Crown - 1st Round", Some("playoffs")),
        // Says "Championship", is not the NCAA tournament.
        ("cbb", 3, "post-season", "College Basketball Crown Championship Game", Some("playoffs")),
        // Conference tournaments are REGULAR season and also say "Championship".
        ("cbb", 2, "regular-season", "Phillips 66 Big 12 Championship - 1st Round", None),
        ("cbb", 2, "regular-season", "T. Rowe Price ACC Tournament - 2nd Round", None),
        ("cbb", 2, "regular-season", "MAAC Championship - Quarterfinal", None),
        // Women's wording changed in 2026; both spellings must land.
        ("wcbb", 3, "post-season", "NCAA Women's Championship - Regional 1 in Spokane - 1st Round", Some("tournament")),
        ("wcbb", 3, "post-season", "NCAA Women's Basketball Championship - Final Four", Some("tournament")),
        ("wcbb", 3, "post-season", "Women's NIT - Semifinal", Some("playoffs")),
        ("wcbb", 3, "post-season", "WBIT - Quarterfinal", Some("playoffs")),
        // --- MLS: no postseason season type, no "MLS Cup" headline. All slug.
        ("mls", 13119, "mls-cup", "", Some("cup")),
        ("mls", 13121, "eastern-conference-playoffs---final", "", Some("playoffs")),
        ("mls", 13122, "western-conference-playoffs---semifinals", "", Some("playoffs")),
        ("mls", 13133, "eastern-conference-playoffs---wild-card", "", Some("playoffs")),
        ("mls", 13481, "2025-mls-regular-season", "", None),
        // --- Men's World Cup: "3rd-place-match", which "third-place" missed.
        ("wc", 10948, "final", "", Some("final")),
        ("wc", 10949, "3rd-place-match", "", Some("third")),
        ("wc", 10950, "semifinals", "", Some("semi")),
        ("wc", 10951, "quarterfinals", "", Some("quarter")),
        ("wc", 10952, "round-of-16", "", Some("knockout")),
        ("wc", 10953, "group-stage", "", None),
        // --- Women's World Cup: same vocabulary, "3rd-place" without "-match".
        ("wwc", 10632, "final", "", Some("final")),
        ("wwc", 10633, "3rd-place", "", Some("third")),
        ("wwc", 10634, "semifinals", "", Some("semi")),
        ("wwc", 10635, "quarterfinals", "", Some("quarter")),
        ("wwc", 10636, "round-of-16", "", Some("knockout")),
        ("wwc", 10637, "group-stage", "", None),
        // --- Champions League: league phase untagged, two knockout spellings.
        ("ucl", 12881, "final", "", Some("final")),
        ("ucl", 12882, "semifinals", "", Some("semi")),
        ("ucl", 12883, "quarterfinals", "", Some("quarter")),
        ("ucl", 12884, "round-of-16", "", Some("knockout")),
        ("ucl", 12886, "knockout-round-playoffs", "", Some("knockout")),
        ("ucl", 12885, "league-phase", "", None),
        // --- Olympic soccer: medals live in the slug.
        ("olysm", 12488, "gold-medal-match", "", Some("gold")),
        ("olysm", 12489, "bronze-medal-match", "", Some("bronze")),
        ("olysm", 12490, "semifinals", "", Some("semi")),
        ("olysm", 12491, "quarterfinals", "", Some("quarter")),
        ("olysm", 12492, "group-stage", "", None),
        ("olysw", 12483, "gold-medal-match", "", Some("gold")),
        ("olysw", 12484, "bronze-medal-match", "", Some("bronze")),
        ("olysw", 12485, "semifinals", "", Some("semi")),
        ("olysw", 12486, "quarterfinals", "", Some("quarter")),
        ("olysw", 12487, "group-stage", "", None),
        // --- Olympic hockey / basketball: EVERY game is "regular-season", so
        // the round is only ever in the headline.
        ("olyhm", 2, "regular-season", "Milano Cortina 2026 Men's Hockey - Gold Medal Game", Some("gold")),
        ("olyhm", 2, "regular-season", "Milano Cortina 2026 Men's Hockey - Bronze Medal Game", Some("bronze")),
        ("olyhm", 2, "regular-season", "Milano Cortina 2026 Men's Hockey - Semifinal", Some("semi")),
        ("olyhm", 2, "regular-season", "Milano Cortina 2026 Men's Hockey - Quarterfinal", Some("quarter")),
        ("olyhm", 2, "regular-season", "Milano Cortina 2026 Men's Hockey - Preliminary Round - Group A", None),
        ("olyhm", 2, "regular-season", "Milano Cortina 2026 Men's Hockey - Qualifications", None),
        ("olyhw", 2, "regular-season", "Milano Cortina 2026 Women's Hockey - Gold Medal Game", Some("gold")),
        ("olyhw", 2, "regular-season", "Milano Cortina 2026 Women's Hockey - Bronze Medal Game", Some("bronze")),
        ("olyhw", 2, "regular-season", "Milano Cortina 2026 Women's Hockey - Semifinal", Some("semi")),
        ("olyhw", 2, "regular-season", "Milano Cortina 2026 Women's Hockey - Quarterfinal", Some("quarter")),
        ("olyhw", 2, "regular-season", "Milano Cortina 2026 Women's Hockey - Preliminary Round - Group B", None),
        ("olybm", 2, "regular-season", "2024 Olympic Men's Basketball - Gold Medal Game", Some("gold")),
        ("olybm", 2, "regular-season", "2024 Olympic Men's Basketball - Bronze Medal Game", Some("bronze")),
        ("olybm", 2, "regular-season", "2024 Olympic Men's Basketball - Semifinal", Some("semi")),
        ("olybm", 2, "regular-season", "2024 Olympic Men's Basketball - Quarterfinal", Some("quarter")),
        ("olybm", 2, "regular-season", "2024 Olympic Men's Basketball - Group B", None),
        ("olybw", 2, "regular-season", "2024 Olympic Women's Basketball - Gold Medal Game", Some("gold")),
        ("olybw", 2, "regular-season", "2024 Olympic Women's Basketball - Bronze Medal Game", Some("bronze")),
        ("olybw", 2, "regular-season", "2024 Olympic Women's Basketball - Semifinal", Some("semi")),
        ("olybw", 2, "regular-season", "2024 Olympic Women's Basketball - Quarterfinal", Some("quarter")),
        ("olybw", 2, "regular-season", "2024 Olympic Women's Basketball - Group C", None),
        // --- College baseball: four season types, only one of them type 3.
        ("cbase", 3, "regionals", "", Some("playoffs")),
        ("cbase", 4, "super-regionals", "", Some("playoffs")),
        ("cbase", 5, "world-series", "", Some("cws")),
        ("cbase", 6, "championship-series", "", Some("cws")),
        ("cbase", 2, "regular-season", "", None),
    ];

    /// Every captured string classifies the way the option list promises.
    #[test]
    fn real_feed_strings_classify_as_advertised() {
        for (sport, type_, slug, headline, want) in REAL_FEED_CASES {
            let s = EventSeason { type_: *type_, slug: (*slug).into() };
            let got = classify_champ(sport, Some(&s), headline);
            let got_tier = got.as_ref().map(|c| c.tier.as_str());
            assert_eq!(
                got_tier, *want,
                "{sport} / season {type_} {slug:?} / {headline:?}: got {got_tier:?}, want {want:?}"
            );
        }
    }

    /// The invariant that actually bites: a league may not OFFER a round the
    /// classifier has never been shown producing from a real feed string.
    ///
    /// The old version of this check only inspected labels, so every dead
    /// control passed it. Adding an option now requires adding the feed string
    /// that proves it fires — which is the only evidence that matters.
    #[test]
    fn every_offered_round_is_proven_by_a_real_feed_string() {
        for (sport_id, ..) in crate::espn::LEAGUE_IDENTITY {
            // Golf tiers come from tournament names and F1 from session ids;
            // neither goes through classify_champ.
            if matches!(*sport_id, "pga" | "lpga" | "f1") {
                continue;
            }
            let proven: std::collections::HashSet<&str> = REAL_FEED_CASES
                .iter()
                .filter(|(s, ..)| s == sport_id)
                .filter_map(|(_, _, _, _, tier)| *tier)
                .collect();
            for round in crate::championship_rounds_for(sport_id) {
                if round.id == "all" {
                    continue; // the blanket matches by definition, not by tier
                }
                assert!(
                    proven.contains(round.id.as_str()),
                    "{sport_id} offers {:?} ({:?}) but no real feed string in \
                     REAL_FEED_CASES produces that tier — either the classifier \
                     cannot emit it (a dead checkbox) or the case is missing",
                    round.id,
                    round.label
                );
            }
        }
    }

    #[test]
    fn nhl_headline_refines_tier() {
        let cup = classify_champ("nhl", Some(&season(3)), "Stanley Cup Final - Game 3").unwrap();
        assert_eq!((cup.tier.as_str(), cup.label.as_str()), ("final", "Stanley Cup Final"));

        let conf = classify_champ("nhl", Some(&season(3)), "West Final - Game 5").unwrap();
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

    /// Football situations read the same ESPN fields the original server did:
    /// shortDownDistanceText ("&"->"+"), possessionText, and possession matched
    /// against the HOME competitor id.
    #[test]
    fn parse_event_attaches_football_situation() {
        let json = |possession: &str| format!(r#"{{
            "competitions": [{{
                "id": "401",
                "date": "2026-10-04T20:00Z",
                "status": {{"type": {{"name": "STATUS_IN_PROGRESS"}}, "period": 3, "displayClock": "8:12"}},
                "situation": {{
                    "shortDownDistanceText": "3rd & 4",
                    "possessionText": "SEA 25",
                    "possession": "{possession}"
                }},
                "competitors": [
                    {{"id": "12", "homeAway": "home", "score": "17", "team": {{"abbreviation": "SEA", "shortDisplayName": "Seahawks"}}}},
                    {{"id": "25", "homeAway": "away", "score": "14", "team": {{"abbreviation": "SF", "shortDisplayName": "49ers"}}}}
                ]
            }}]
        }}"#);
        let ev: Event = serde_json::from_str(&json("12")).unwrap();
        let g = parse_event("nfl", ev).unwrap();
        assert_eq!(
            g.situation,
            Some(Situation::Football {
                down_string: "3rd & 4".into(), // ESPN's own text, ampersand and all
                ball_position: "SEA 25".into(),
                home_possession: Some(true),
            })
        );

        let ev: Event = serde_json::from_str(&json("25")).unwrap();
        let g = parse_event("cfb", ev).unwrap();
        assert!(matches!(
            g.situation,
            Some(Situation::Football { home_possession: Some(false), .. })
        ));

        // Non-football sports never get a football situation from these fields.
        let ev: Event = serde_json::from_str(&json("12")).unwrap();
        assert_eq!(parse_event("nba", ev).unwrap().situation, None);
    }

    /// Baseball: count/outs/runners from situation, inning half from the
    /// "Top ..." shortDetail substring (exactly the original's check).
    #[test]
    fn parse_event_attaches_baseball_situation() {
        let json = |detail: &str, name: &str| format!(r#"{{
            "competitions": [{{
                "id": "401",
                "date": "2026-07-07T23:05Z",
                "status": {{"type": {{"name": "{name}", "shortDetail": "{detail}"}}, "period": 5}},
                "situation": {{
                    "balls": 3, "strikes": 2, "outs": 1,
                    "onFirst": true, "onSecond": false, "onThird": true
                }},
                "competitors": [
                    {{"id": "10", "homeAway": "home", "score": "4", "team": {{"abbreviation": "SEA", "shortDisplayName": "Mariners"}}}},
                    {{"id": "11", "homeAway": "away", "score": "2", "team": {{"abbreviation": "HOU", "shortDisplayName": "Astros"}}}}
                ]
            }}]
        }}"#);
        let ev: Event = serde_json::from_str(&json("Top 5th", "STATUS_IN_PROGRESS")).unwrap();
        let g = parse_event("mlb", ev).unwrap();
        assert_eq!(
            g.situation,
            Some(Situation::Baseball {
                balls: 3, strikes: 2, outs: 1,
                on_first: true, on_second: false, on_third: true,
                is_inning_top: true,
            })
        );

        let ev: Event = serde_json::from_str(&json("Bot 5th", "STATUS_IN_PROGRESS")).unwrap();
        assert!(matches!(
            parse_event("mlb", ev).unwrap().situation,
            Some(Situation::Baseball { is_inning_top: false, .. })
        ));

        // ESPN keeps a stale situation object on finished games — ignore it.
        let ev: Event = serde_json::from_str(&json("Final", "STATUS_FINAL")).unwrap();
        assert_eq!(parse_event("mlb", ev).unwrap().situation, None);
    }

    /// A live game with NO situation object still gets the sport layout with
    /// zeroed fields — the original's server defaulted the same way, so the
    /// panel never flips between layouts when ESPN transiently omits it.
    #[test]
    fn live_game_without_situation_object_gets_zeroed_layout() {
        let json = |sport: &str, league_status: &str| format!(r#"{{
            "competitions": [{{
                "id": "401",
                "date": "2026-10-04T20:00Z",
                "status": {{"type": {{"name": "{league_status}", "shortDetail": "Bot 3rd"}}, "period": 3, "displayClock": "8:12"}},
                "competitors": [
                    {{"id": "12", "homeAway": "home", "score": "7", "team": {{"abbreviation": "SEA", "shortDisplayName": "{sport} Home"}}}},
                    {{"id": "25", "homeAway": "away", "score": "3", "team": {{"abbreviation": "SF", "shortDisplayName": "{sport} Away"}}}}
                ]
            }}]
        }}"#);
        let ev: Event = serde_json::from_str(&json("nfl", "STATUS_IN_PROGRESS")).unwrap();
        assert_eq!(
            parse_event("nfl", ev).unwrap().situation,
            Some(Situation::Football {
                down_string: String::new(),
                ball_position: String::new(),
                home_possession: None,
            })
        );
        let ev: Event = serde_json::from_str(&json("mlb", "STATUS_IN_PROGRESS")).unwrap();
        assert_eq!(
            parse_event("mlb", ev).unwrap().situation,
            Some(Situation::Baseball {
                balls: 0, strikes: 0, outs: 0,
                on_first: false, on_second: false, on_third: false,
                is_inning_top: false,
            })
        );
        // Pregame stays layout-free.
        let ev: Event = serde_json::from_str(&json("nfl", "STATUS_SCHEDULED")).unwrap();
        assert_eq!(parse_event("nfl", ev).unwrap().situation, None);
    }

    /// The original called every extra NHL period "OT", never "4th".
    #[test]
    fn nhl_overtime_ordinal_is_ot() {
        let json = |period: u32, name: &str| format!(r#"{{
            "competitions": [{{
                "id": "401",
                "date": "2026-06-10T00:00Z",
                "status": {{"type": {{"name": "{name}"}}, "period": {period}, "displayClock": "3:33"}},
                "competitors": [
                    {{"id": "1", "homeAway": "home", "score": "2", "team": {{"abbreviation": "EDM", "shortDisplayName": "Oilers"}}}},
                    {{"id": "2", "homeAway": "away", "score": "2", "team": {{"abbreviation": "FLA", "shortDisplayName": "Panthers"}}}}
                ]
            }}]
        }}"#);
        let ev: Event = serde_json::from_str(&json(4, "STATUS_IN_PROGRESS")).unwrap();
        assert_eq!(parse_event("nhl", ev).unwrap().ordinal, "OT");
        let ev: Event = serde_json::from_str(&json(5, "STATUS_IN_PROGRESS")).unwrap();
        assert_eq!(parse_event("nhl", ev).unwrap().ordinal, "OT", "2OT/SO also reads OT");
        // Regulation periods unchanged; other sports unaffected.
        let ev: Event = serde_json::from_str(&json(3, "STATUS_IN_PROGRESS")).unwrap();
        assert_eq!(parse_event("nhl", ev).unwrap().ordinal, "3rd");
        let ev: Event = serde_json::from_str(&json(4, "STATUS_IN_PROGRESS")).unwrap();
        assert_eq!(parse_event("nba", ev).unwrap().ordinal, "4th");
    }

    /// The demo slate must exercise every board feature: all three situational
    /// layouts (football on both leagues), the OT badge case, and both pregame
    /// variants — so a demo run shows everything without waiting on real games.
    /// The reported bug: a Thursday tournament appeared on the panel at 10:30pm
    /// WEDNESDAY.
    ///
    /// ESPN gives golf an event date of midnight LOCAL on day one — 04:00Z for
    /// a US event — and flags `timeValid: false` to say the time means nothing.
    /// The board's half-hour pre-game window applied to that opens at 03:30Z,
    /// which is 10:30pm the previous evening in Central time. The first tee
    /// time is the real start.
    #[test]
    fn golf_starts_at_the_first_tee_time_not_at_midnight() {
        let body = r#"{"events":[{"id":"401","shortName":"Wyndham","endDate":"2026-08-09T04:00Z",
          "competitions":[{"date":"2026-08-06T04:00Z","status":{"type":{"state":"pre","completed":false}},
          "competitors":[
            {"order":1,"athlete":{"displayName":"A"},"status":{"position":{"id":1},"teeTime":"2026-08-06T18:22Z"},"statistics":[]},
            {"order":2,"athlete":{"displayName":"B"},"status":{"position":{"id":2},"teeTime":"2026-08-06T10:50Z"},"statistics":[]},
            {"order":3,"athlete":{"displayName":"C"},"status":{"position":{"id":3},"teeTime":"2026-08-06T12:05Z"},"statistics":[]}
          ]}]}]}"#;
        let resp: LeaderboardResponse = serde_json::from_str(body).unwrap();
        let ev = resp.events.into_iter().next().unwrap();
        let g = parse_golf("pga", ev).expect("a game");
        assert_eq!(g.start_time, "2026-08-06T10:50Z", "the EARLIEST tee time, not the event date");
        assert_ne!(g.start_time, "2026-08-06T04:00Z", "midnight is what put it on the panel a day early");
    }

    /// The reported bug: the Wyndham was called for the day and stayed on the
    /// board, still labelled live, all evening.
    ///
    /// A golf round called for darkness comes back as STATUS_SUSPENDED, which
    /// we mapped to Intermission — and Intermission counts as live, with no
    /// time bound on it. So the tournament sat there until play resumed the
    /// next morning. ESPN had already said it wasn't in play: `state: "post"`.
    #[test]
    fn a_round_suspended_for_the_day_leaves_the_board() {
        let body = r#"{"events":[{"id":"401","shortName":"Wyndham","endDate":"2026-08-09T04:00Z",
          "competitions":[{"date":"2026-08-06T04:00Z",
          "status":{"period":1,"type":{"name":"STATUS_SUSPENDED","state":"post","detail":"Round 1 - Suspended"}},
          "competitors":[
            {"order":1,"athlete":{"displayName":"A"},"status":{"position":{"id":1},"teeTime":"2026-08-06T10:50Z","thru":18},"statistics":[]},
            {"order":2,"athlete":{"displayName":"B"},"status":{"position":{"id":2},"teeTime":"2026-08-06T11:12Z","thru":16},"statistics":[]}
          ]}]}]}"#;
        let resp: LeaderboardResponse = serde_json::from_str(body).unwrap();
        let ev = resp.events.into_iter().next().unwrap();
        assert!(parse_golf("pga", ev).is_none(), "play is done for the day — the panel must drop it");
    }

    /// A period past regulation says so, instead of counting.
    ///
    /// The board printed the raw period number, so NBA overtime read "5TH",
    /// college basketball "3RD" (it plays halves), NHL overtime "4TH", and a
    /// soccer penalty shootout "5TH" — a label that is not wrong so much as
    /// meaningless, since nobody calls it that.
    ///
    /// ESPN's own `altDetail` leads because the period number is genuinely
    /// ambiguous where it matters most: an NHL game at period 5 is a shootout
    /// in the regular season and double overtime in the playoffs. Over five
    /// sampled 2025-26 dates that is 7 games ESPN calls "SO" that arithmetic
    /// would have called "2OT". So arithmetic is the FALLBACK, for the live
    /// games where `altDetail` has not been observed, and it declines to answer
    /// exactly where it cannot know.
    #[test]
    fn overtime_says_overtime_instead_of_counting_periods() {
        let st = |period: i64, alt: Option<&str>| Status {
            type_: StatusType {
                name: "STATUS_FINAL".into(),
                state: "post".into(),
                short_detail: String::new(),
                alt_detail: alt.map(|s| s.to_string()),
            },
            period,
            display_clock: String::new(),
        };
        let say = |sport: &str, period: i64, alt: Option<&str>| {
            ordinal_for(&st(period, alt), GameStatus::Final, sport)
        };
        // ESPN's label wins wherever it is present — including the one case
        // arithmetic gets backwards.
        assert_eq!(say("nhl", 5, Some("SO")), "SO", "a regular-season shootout");
        assert_eq!(say("nhl", 5, Some("2OT")), "2OT", "the same period in the playoffs");
        assert_eq!(say("nhl", 4, Some("OT")), "OT");
        assert_eq!(say("nba", 5, Some("OT")), "OT");
        // Without it, the period number answers where it safely can.
        assert_eq!(say("nba", 5, None), "OT");
        assert_eq!(say("nba", 6, None), "2OT");
        assert_eq!(say("cbb", 3, None), "OT", "college basketball plays halves");
        assert_eq!(say("nfl", 5, None), "OT");
        assert_eq!(say("nhl", 4, None), "OT", "the first NHL overtime is unambiguous");
        assert_eq!(say("nhl", 5, None), "5th", "SO or 2OT — refuses to guess");
        // Soccer: two extra-time halves, then the shootout.
        assert_eq!(say("epl", 3, None), "ET");
        assert_eq!(say("ucl", 4, None), "ET");
        assert_eq!(say("ucl", 5, None), "PENS");
        // Regulation is untouched, and baseball counts innings forever.
        for (sport, period, want) in
            [("nba", 4, "4th"), ("nhl", 3, "3rd"), ("cbb", 2, "2nd"), ("mlb", 12, "12th")]
        {
            assert_eq!(say(sport, period, None), want, "{sport} period {period}");
        }
        // Intermission still appends, and halftime still wins outright.
        assert_eq!(ordinal_for(&st(4, Some("OT")), GameStatus::Intermission, "nhl"), "OT INT");
        let mut half = st(2, None);
        half.type_.name = "STATUS_HALFTIME".into();
        assert_eq!(ordinal_for(&half, GameStatus::Active, "cbb"), "HALF");
    }

    /// The other half of that: a pause ESPN keeps at `state: "in"` is a delay
    /// *within* the game. Those still belong on the board — dropping a rain
    /// delay would blank the panel mid-game and bring it back minutes later.
    #[test]
    fn a_delay_within_a_game_stays_live() {
        let t = |name: &str, state: &str| StatusType {
            name: name.into(),
            state: state.into(),
            short_detail: String::new(),
            alt_detail: None,
        };
        assert_eq!(map_status(&t("STATUS_DELAYED", "in")), Some(GameStatus::Intermission));
        assert_eq!(map_status(&t("STATUS_HALFTIME", "in")), Some(GameStatus::Intermission));
        assert_eq!(map_status(&t("STATUS_SUSPENDED", "in")), Some(GameStatus::Intermission));
        // Finished is finished — "post" must not swallow the final score.
        assert_eq!(map_status(&t("STATUS_FINAL", "post")), Some(GameStatus::Final));
        assert_eq!(map_status(&t("STATUS_PLAY_COMPLETE", "post")), Some(GameStatus::Final));
        // A feed that omits `state` behaves exactly as it did before.
        assert_eq!(map_status(&t("STATUS_IN_PROGRESS", "")), Some(GameStatus::Active));
    }

    /// A tournament's round comes from the competition's period, and the start
    /// advances with it — Saturday's card opens on Saturday's first tee, not on
    /// the one played on Thursday.
    ///
    /// The mixed `period`s below are real: at round 3 of the 2026 Wyndham the
    /// live feed reported 83 competitors on period 3, 63 on period 2 and one on
    /// period 1 (players cut, or stopped by the round-1 suspension). ESPN
    /// publishes no `teeTime` for those, but the round filter means it would not
    /// matter if it started to.
    #[test]
    fn golf_reports_the_round_and_that_rounds_first_tee() {
        let body = r#"{"events":[{"id":"401","shortName":"Wyndham","endDate":"2026-08-09T04:00Z",
          "competitions":[{"date":"2026-08-06T04:00Z",
          "status":{"period":3,"type":{"name":"STATUS_IN_PROGRESS","state":"in","detail":"Round 3 - In Progress"}},
          "competitors":[
            {"order":1,"athlete":{"displayName":"A"},"status":{"position":{"id":1},"period":3,"teeTime":"2026-08-08T16:10Z"},"statistics":[]},
            {"order":2,"athlete":{"displayName":"B"},"status":{"position":{"id":2},"period":3,"teeTime":"2026-08-08T11:00Z"},"statistics":[]},
            {"order":3,"athlete":{"displayName":"C"},"status":{"position":{"id":3},"period":2,"teeTime":"2026-08-07T12:40Z"},"statistics":[]}
          ]}]}]}"#;
        let resp: LeaderboardResponse = serde_json::from_str(body).unwrap();
        let g = parse_golf("pga", resp.events.into_iter().next().unwrap()).expect("a game");
        assert_eq!(g.ordinal, "R3", "the card has to say which day of the tournament this is");
        assert_eq!(
            g.start_time, "2026-08-08T11:00Z",
            "the earliest tee of THIS round; a cut player's round-2 time must not drag it back a day"
        );
    }

    /// A tournament ESPN hasn't started yet reports period 0, and inventing "R0"
    /// would be worse than saying nothing.
    #[test]
    fn a_tournament_that_has_not_started_has_no_round() {
        let body = r#"{"events":[{"id":"402","shortName":"Open","endDate":"",
          "competitions":[{"date":"2026-09-01T04:00Z","status":{"period":0,"type":{"state":"pre"}},
          "competitors":[{"order":1,"athlete":{"displayName":"A"},"status":{"position":{"id":1},"teeTime":"2026-09-03T12:00Z"},"statistics":[]}]}]}]}"#;
        let resp: LeaderboardResponse = serde_json::from_str(body).unwrap();
        let g = parse_golf("pga", resp.events.into_iter().next().unwrap()).expect("a game");
        assert_eq!(g.ordinal, "", "no round yet");
        // The round filter must not swallow the tee time when nobody has a period.
        assert_eq!(g.start_time, "2026-09-03T12:00Z");
    }

    /// No tee times published yet (ESPN posts them a day or two out) must fall
    /// back to the old behaviour rather than to no start time at all.
    #[test]
    fn golf_without_tee_times_falls_back_to_the_event_date() {
        let body = r#"{"events":[{"id":"402","shortName":"Open","endDate":"",
          "competitions":[{"date":"2026-09-01T04:00Z","status":{"type":{"state":"pre","completed":false}},
          "competitors":[{"order":1,"athlete":{"displayName":"A"},"status":{"position":{"id":1}},"statistics":[]}]}]}]}"#;
        let resp: LeaderboardResponse = serde_json::from_str(body).unwrap();
        let ev = resp.events.into_iter().next().unwrap();
        let g = parse_golf("pga", ev).expect("a game");
        assert_eq!(g.start_time, "2026-09-01T04:00Z");
    }

    #[test]
    fn demo_covers_the_feature_matrix() {
        let games = demo_games(0, 0);
        let has = |f: &dyn Fn(&Game) -> bool| games.iter().any(f);
        assert!(has(&|g| g.sport_id == "nfl" && matches!(g.situation, Some(Situation::Football { .. }))), "nfl situation");
        assert!(has(&|g| g.sport_id == "cfb" && matches!(g.situation, Some(Situation::Football { .. }))), "cfb situation");
        assert!(has(&|g| g.sport_id == "mlb" && matches!(g.situation, Some(Situation::Baseball { .. }))), "mlb situation");
        assert!(has(&|g| g.sport_id == "nhl" && matches!(g.situation, Some(Situation::Hockey { .. }))), "nhl situation");
        assert!(has(&|g| matches!(&g.situation, Some(Situation::Baseball { on_first: true, on_second: true, on_third: true, .. }))), "bases loaded");
        assert!(
            has(&|g| g.ordinal == "OT"
                && matches!(g.situation, Some(Situation::Hockey { away_players: 3, home_players: 3, .. }))),
            "3-on-3 overtime badge"
        );
        assert!(has(&|g| g.pregame.as_ref().is_some_and(|p| !p.broadcast.is_empty())), "pregame with TV");
        assert!(
            has(&|g| g.pregame.as_ref().is_some_and(|p| p.broadcast.is_empty() && !p.away_record.is_empty())),
            "pregame records-only"
        );
        assert!(
            has(&|g| g.leaderboard.as_ref().is_some_and(|lb| lb.rows.len() > 4)),
            "a leaderboard deep enough to scroll"
        );
        assert!(
            has(&|g| g.leaderboard.as_ref().is_some_and(|lb| lb.rows.iter().any(|r| r.tire.is_some()))),
            "a timing tower WITH tire chips"
        );
        assert!(
            has(&|g| g.sport_id == "f1"
                && g.leaderboard.as_ref().is_some_and(|lb|
                    !lb.rows.is_empty() && lb.rows.iter().all(|r| r.tire.is_none()))),
            "an F1 tower WITHOUT tire data (the live-ESPN look)"
        );
    }

    /// The demo source flows through the same poll-loop ingestion filter as
    /// live ESPN, so every demo game — including the synthetic Pregame pair
    /// (+20m / +3h start times) — must pass it, at tick 0 and after the
    /// scripted ending has flipped to Final. If a demo game ever grows a
    /// far-future start time, this catches it before `--demo` goes blank.
    #[test]
    fn demo_slate_survives_ingestion_filter() {
        let now = chrono::Utc::now();
        for (n, elapsed) in [(0, 0), (7, 10 * 60)] {
            let games = demo_games(n, elapsed);
            assert!(!games.is_empty());
            for g in &games {
                assert!(
                    crate::ingest_keeps(g, now),
                    "demo game '{}' (tick {n}) must survive the ingestion filter",
                    g.id
                );
            }
        }
    }

    /// One event the parser cannot read must not take the rest of the
    /// response with it: the Presidents Cup's nested competitions used to
    /// fail the WHOLE PGA feed, so an ordinary event beside it vanished too.
    #[test]
    fn one_unreadable_golf_event_does_not_sink_the_feed() {
        let body = r#"{"events":[
          {"id":"401824815","shortName":"Presidents Cup",
           "competitions":[[{"id":"12093","status":{"type":{"name":"STATUS_IN_PROGRESS"}},"competitors":[]}]]},
          {"id":"401","shortName":"Wyndham","competitions":[{"date":"2026-08-06T04:00Z",
           "status":{"period":3,"type":{"name":"STATUS_IN_PROGRESS","state":"in"}},
           "competitors":[{"order":1,"athlete":{"displayName":"A Player"},"status":{"position":{"id":1}},"statistics":[]}]}]}
        ]}"#;
        let games = parse_feed("pga", body).expect("the response still parses");
        assert_eq!(games.len(), 1, "the ordinary event survives");
        assert_eq!(games[0].id, "401");
    }

    /// The 2026 Presidents Cup, verbatim from ESPN's golf leaderboard feed
    /// on the Saturday morning (design/golf-logs): team match play with its
    /// competitions nested by session. It becomes ONE game carrying the cup
    /// score and the live session's matches, keyed to the cup's teams.
    #[test]
    fn presidents_cup_parses_as_match_play() {
        use crate::model::MatchState;
        let raw = include_str!("../../../design/golf-logs/espn-pga-leaderboard-presidents-cup-20260926T124348Z.json");
        let games = parse_feed("pga", raw).expect("parses");
        assert_eq!(games.len(), 1, "one game for the event");
        let g = &games[0];
        let lb = g.leaderboard.as_ref().unwrap();
        assert_eq!(lb.name, "PRESIDENTS CUP", "not cut at PRES like a sponsor");
        assert!(lb.rows.is_empty(), "no stroke-play rows");
        let mp = lb.matchplay.as_ref().expect("match play");
        assert_eq!((mp.teams[0].abbrev.as_str(), mp.teams[1].abbrev.as_str()), ("USA", "INTL"));
        assert_eq!(mp.target.as_deref(), Some("15.5"));
        assert_eq!(mp.session, "Saturday Morning Four-Balls", "the live session");
        assert_eq!(mp.format, "fourball");
        assert_eq!(mp.matches.len(), 4);
        // Keyed to the CUP's sides: USA first even where the match lists
        // INTL as home.
        let m = &mp.matches[0];
        assert_eq!(m.players, [vec!["THOMAS".to_string(), "KOIVUN".into()], vec!["IM".into(), "LEE".into()]]);
        assert_eq!(m.state, MatchState::Live { leader: Some(0), margin: "1UP".into(), thru: 2 });
        // All square, and the Kims told apart by initial.
        assert_eq!(mp.matches[1].players[1], vec!["S KIM".to_string(), "T KIM".into()]);
        assert_eq!(mp.matches[1].state, MatchState::Live { leader: None, margin: "AS".into(), thru: 1 });
        // Not yet teed off: the tee time rides along.
        assert_eq!(mp.matches[3].state, MatchState::Scheduled { tee: Some("2026-09-26T12:56Z".into()) });
        assert_eq!(g.status, GameStatus::Active);
        assert_eq!(g.start_time, "2026-09-26T12:56Z", "the next tee time");
    }

    /// A finished match's result keeps golf's numbers with a readable
    /// separator: ESPN's "6 & 4" is "6+4" on the panel.
    #[test]
    fn match_results_use_a_plus_not_an_ampersand() {
        assert_eq!(match_notation("6 & 4"), "6+4");
        assert_eq!(match_notation("2 & 1"), "2+1");
        assert_eq!(match_notation("1 Up"), "1UP");
        assert_eq!(match_notation("AS"), "AS");
    }

    /// Sunday singles, verbatim from ESPN (design/golf-logs): each
    /// competitor carries its player as `athlete` with an EMPTY roster, which
    /// the pairs-only parser read as no players at all — every singles row
    /// blank on the board.
    #[test]
    fn presidents_cup_singles_name_their_players() {
        let raw = include_str!("../../../design/golf-logs/espn-pga-leaderboard-presidents-cup-singles-20260927T1630Z.json");
        let g = parse_feed("pga", raw).expect("parses").remove(0);
        let mp = g.leaderboard.as_ref().unwrap().matchplay.as_ref().unwrap();
        assert_eq!(mp.session, "Sunday Singles");
        assert_eq!(mp.matches.len(), 12);
        for m in &mp.matches {
            assert_eq!((m.players[0].len(), m.players[1].len()), (1, 1), "one player a side");
        }
        assert_eq!(mp.matches[0].players[0], vec!["YOUNG".to_string()], "USA's first singles player");
    }

    /// The debug TUI's parse path is the server's parse path: parse_feed
    /// handles every feed id, errors gracefully on garbage, and the demo feed
    /// generates without any raw input.
    #[test]
    fn parse_feed_covers_every_feed() {
        assert!(!parse_feed("demo", "").unwrap().is_empty());
        assert!(parse_feed("nhl", "not json").is_err());
        assert!(parse_feed("bogus-feed", "{}").is_err());
        assert!(parse_feed("nhl-aux", "{}").is_err(), "aux feed is not a game feed");
        // A minimal team-league scoreboard parses through parse_event.
        let sb = r#"{"events":[{"competitions":[{"id":"1","date":"2026-10-04T20:00Z",
            "status":{"type":{"name":"STATUS_SCHEDULED"},"period":0},
            "competitors":[
              {"id":"1","homeAway":"home","score":"0","team":{"abbreviation":"TOR","shortDisplayName":"Maple Leafs"}},
              {"id":"2","homeAway":"away","score":"0","team":{"abbreviation":"BOS","shortDisplayName":"Bruins"}}
            ]}]}]}"#;
        let games = parse_feed("nhl", sb).unwrap();
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].away.abbrev, "BOS");
    }

    /// Scheduled games carry each side's overall record and the NATIONAL
    /// broadcast (regional feeds are wrong for most viewers); live games don't.
    #[test]
    fn pregame_records_and_national_broadcast() {
        let json = |status: &str| format!(r#"{{
            "competitions": [{{
                "id": "401",
                "date": "2026-10-04T20:00Z",
                "status": {{"type": {{"name": "{status}"}}, "period": 0}},
                "broadcasts": [
                    {{"market": "away", "names": ["Marquee Sports Net"]}},
                    {{"market": "national", "names": ["ESPN Unlmtd", "MLB.TV"]}}
                ],
                "competitors": [
                    {{"id": "10", "homeAway": "home", "score": "0",
                      "records": [{{"type": "total", "summary": "51-22"}}, {{"type": "home", "summary": "23-8"}}],
                      "team": {{"abbreviation": "SEA", "shortDisplayName": "Mariners"}}}},
                    {{"id": "11", "homeAway": "away", "score": "0",
                      "records": [{{"type": "total", "summary": "48-25"}}],
                      "team": {{"abbreviation": "HOU", "shortDisplayName": "Astros"}}}}
                ]
            }}]
        }}"#);
        let ev: Event = serde_json::from_str(&json("STATUS_SCHEDULED")).unwrap();
        let p = parse_event("mlb", ev).unwrap().pregame.expect("pregame attached");
        assert_eq!(p.away_record, "48-25");
        assert_eq!(p.home_record, "51-22", "overall record, not the home split");
        assert_eq!(p.broadcast, "ESPN UNLMTD", "national market, not regional");

        // Live games show the situation layouts, never pregame chrome.
        let ev: Event = serde_json::from_str(&json("STATUS_IN_PROGRESS")).unwrap();
        assert!(parse_event("mlb", ev).unwrap().pregame.is_none());
    }

    /// The ESPN<->NHL abbreviation bridge for power-play matching.
    #[test]
    fn nhl_abbrev_aliases() {
        for (espn, nhl) in [("NJ", "NJD"), ("TB", "TBL"), ("LA", "LAK"), ("SJ", "SJS"), ("UTAH", "UTA")] {
            assert_eq!(nhl_abbrev(espn), nhl);
        }
        // Already-NHL abbrevs pass through (matching is idempotent).
        for same in ["BOS", "TOR", "NJD", "TBL", "VGK"] {
            assert_eq!(nhl_abbrev(same), same);
        }
    }
}

#[cfg(test)]
mod golf_title_tests {
    use super::*;

    /// No tournament title is ever wider than the panel.
    ///
    /// `render_leaderboard_rows` centres the title at `32 - width/2`, so a name
    /// wider than 64px gets a NEGATIVE x and is clipped at BOTH ends — the panel
    /// showed "TANDARD PORTLAN" for the Standard Portland Classic. It was not a
    /// rare case: 18 of the 81 events on the 2026 PGA and LPGA calendars
    /// overflowed, including two majors.
    ///
    /// Names sampled from those calendars, weighted to the ones that broke.
    #[test]
    fn a_tournament_title_always_fits_the_panel() {
        let names = [
            "The Standard Portland Classic",
            "The ANNIKA driven by Gainbridge at Pelican",
            "ISPS HANDA Women's Scottish Open",
            "Riviera Maya Open at Mayakoba",
            "VidantaWorld Mexico Open",
            "Hilton Grand Vacations Tournament of Champions",
            "Fortinet Founders Cup",
            "Mizuho Americas Open",
            "Honda LPGA Thailand",
            "Walmart NW Arkansas Championship pres. by P&G",
            "Buick LPGA Shanghai",
            "HSBC Women's World Championship",
            "Corales Puntacana Championship",
            "Kroger Queen City Championship pres by P&G",
            "CPKC Women's Open",
            "PGA TOUR Q-School presented by Korn Ferry",
            "Puerto Rico Open",
            "U.S. Women's Open pres. by Ally",
            // And a spread of ones that already fit, which must not change.
            "Masters Tournament",
            "FedEx St. Jude Championship",
            "Genesis Scottish Open",
            "AT&T Pebble Beach Pro-Am",
            "3M Open",
        ];
        for n in names {
            let cleaned = clean_golf_name(n);
            let w = crate::render::text_width(&cleaned, false);
            assert!(w <= crate::protocol_frame_width(), "{n:?} -> {cleaned:?} is {w}px");
            assert!(!cleaned.is_empty(), "{n:?} shortened to nothing");
            assert!(!cleaned.ends_with(' '), "{n:?} -> {cleaned:?} has a trailing space");
        }
        // The established shortenings still hold — this must not have quietly
        // rewritten names that were already right.
        assert_eq!(clean_golf_name("Masters Tournament"), "THE MASTERS");
        assert_eq!(clean_golf_name("Genesis Scottish Open"), "SCOTTISH OPEN");
        assert_eq!(clean_golf_name("AT&T Pebble Beach Pro-Am"), "PEBBLE BEACH");
        assert_eq!(clean_golf_name("FedEx St. Jude Championship"), "FEDEX ST. JUDE");
        // Leading words go first, because that is where golf keeps its sponsors.
        assert_eq!(clean_golf_name("Buick LPGA Shanghai"), "LPGA SHANGHAI");
        assert_eq!(clean_golf_name("Fortinet Founders Cup"), "FOUNDERS CUP");

        // Whole words, never fragments. The first version of the fit kept a
        // two-word tail and drew "STANDARD PORTLA".
        assert_eq!(clean_golf_name("The Standard Portland Classic"), "PORTLAND");
        assert_eq!(clean_golf_name("Corales Puntacana Championship"), "PUNTACANA");
        // Punctuation goes before any word does — every word survives here.
        assert_eq!(clean_golf_name("U.S. Women's Open pres. by Ally"), "US WOMENS OPEN");
        // A tail may not begin with a preposition: not "AT MAYAKOBA".
        assert_eq!(clean_golf_name("Riviera Maya Open at Mayakoba"), "MAYAKOBA");
        // Two tournaments that would otherwise render identically in the same
        // week must not: the men's Scottish Open and the women's.
        assert_ne!(
            clean_golf_name("ISPS HANDA Women's Scottish Open"),
            clean_golf_name("Genesis Scottish Open"),
        );
        // And a place is not renamed to a different place.
        assert_eq!(clean_golf_name("Puerto Rico Open"), "PUERTO RICO");
    }

    /// Cutting the sponsor or a filler word never leaves a bare fragment.
    ///
    /// From an audit of every 2026 PGA and LPGA event: "pres. by" was cut at
    /// the LETTERS, so EXPRESS lost its tail and the Presidents Cup its whole
    /// name; and dropping "Championship" left one short word — two majors
    /// among them ("PGA", "OPEN").
    #[test]
    fn a_tournament_title_is_never_a_fragment() {
        for (name, want) in [
            ("The American Express", "THE AMEX"),
            ("Presidents Cup", "PRESIDENTS CUP"),
            ("PGA Championship", "PGA CHAMP"),
            ("The Open", "THE OPEN"),
            ("BMW Championship", "BMW CHAMP"),
            ("TOUR Championship", "TOUR CHAMP"),
            ("ISCO Championship", "ISCO CHAMP"),
            ("The RSM Classic", "RSM CLASSIC"),
            ("Bank of Utah Championship", "BANK OF UTAH"),
            ("Ford Championship pres. by Wild Horse Pass", "FORD CHAMP"),
            ("Dow Championship", "DOW CHAMP"),
            ("FM Championship", "FM CHAMP"),
            // A whole word after the cut is a name, and stays as it was.
            ("Wyndham Championship", "WYNDHAM"),
            ("The Chevron Championship", "CHEVRON"),
            ("BMW Ladies Championship", "BMW LADIES"),
            ("Zurich Classic of New Orleans", "ZURICH"),
            ("the Memorial Tournament pres. by Workday", "MEMORIAL"),
        ] {
            assert_eq!(clean_golf_name(name), want, "{name:?}");
        }
    }
}

#[cfg(test)]
mod f1_title_tests {
    use super::*;

    /// Every race on the 2026 calendar, as ESPN's `shortName` spells it,
    /// reads as its place.
    ///
    /// The old rule kept the last 13 characters' worth of words, which cut
    /// two-word places in half — the US Grand Prix drew "STATES GP" — and kept
    /// "IN MALAYSIA" from "Bahrain GP in Malaysia".
    #[test]
    fn every_2026_grand_prix_reads_as_its_place() {
        for (short, want) in [
            ("Qatar Airways Australian GP", "AUSTRALIAN GP"),
            ("Heineken Chinese GP", "CHINESE GP"),
            ("Aramco Japanese GP", "JAPANESE GP"),
            ("Gulf Air Bahrain GP", "BAHRAIN GP"),
            ("STC Saudi Arabian GP", "SAUDI GP"),
            ("Crypto.com Miami GP", "MIAMI GP"),
            ("Lenovo Canadian GP", "CANADIAN GP"),
            ("Monaco GP", "MONACO GP"),
            ("MSC Cruises Barcelona-Catalunya GP", "BARCELONA GP"),
            ("Lenovo Austrian GP", "AUSTRIAN GP"),
            ("Pirelli British GP", "BRITISH GP"),
            ("Moët & Chandon Belgian GP", "BELGIAN GP"),
            ("AWS Hungarian GP", "HUNGARIAN GP"),
            ("Heineken Dutch GP", "DUTCH GP"),
            ("Pirelli Italian GP", "ITALIAN GP"),
            ("Tag Heuer Spanish GP", "SPANISH GP"),
            ("Qatar Airways Azerbaijan GP", "AZERBAIJAN GP"),
            ("Gulf Air Bahrain GP in Malaysia", "BAHRAIN GP"),
            ("Singapore Airlines Singapore GP", "SINGAPORE GP"),
            ("MSC Cruises United States GP", "US GP"),
            ("Mexico City GP", "MEXICO GP"),
            ("MSC Cruises São Paulo GP", "SAO PAULO GP"),
            ("Heineken Las Vegas GP", "LAS VEGAS GP"),
            ("Qatar Airways Qatar GP", "QATAR GP"),
            ("Etihad Airways Abu Dhabi GP", "ABU DHABI GP"),
        ] {
            let got = clean_f1_name(short);
            assert_eq!(got, want, "{short:?}");
            // Every session title the parser builds from it fits the panel.
            // 65, not 64: `text_width` counts the last glyph's blank advance
            // column, so "AZERBAIJAN GP SQ" measures 65 and inks exactly 64.
            for suffix in ["", " Q", " S", " SQ"] {
                let title = format!("{got}{suffix}");
                assert!(crate::render::text_width(&title, false) <= 65, "{title:?} is too wide");
            }
        }
    }
}
