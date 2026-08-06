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
                League { sport_id: "nhl", sport: "hockey", league: "nhl", name: "NHL", games_query: "" },
                League { sport_id: "nba", sport: "basketball", league: "nba", name: "NBA", games_query: "" },
                League { sport_id: "mlb", sport: "baseball", league: "mlb", name: "MLB", games_query: "" },
                League { sport_id: "nfl", sport: "football", league: "nfl", name: "NFL", games_query: "" },
                League { sport_id: "cfb", sport: "football", league: "college-football", name: "College Football", games_query: "" },
                League { sport_id: "cbb", sport: "basketball", league: "mens-college-basketball", name: "Men's College Basketball", games_query: "?groups=50" },
                League { sport_id: "wcbb", sport: "basketball", league: "womens-college-basketball", name: "Women's College Basketball", games_query: "?groups=50" },
                League { sport_id: "wnba", sport: "basketball", league: "wnba", name: "WNBA", games_query: "" },
                // Tournaments that only exist some of the time. They cost one
                // request each per poll and sit dormant out of season — the
                // ingestion filter drops anything more than 24h out, so an
                // idle league contributes nothing to the slate.
                //
                // Every slug here was verified against ESPN before being added;
                // a wrong one is a warning line every poll forever.
                League { sport_id: "olyhm", sport: "hockey", league: "olympics-mens-ice-hockey", name: "Olympic Hockey (M)", games_query: "" },
                League { sport_id: "olyhw", sport: "hockey", league: "olympics-womens-ice-hockey", name: "Olympic Hockey (W)", games_query: "" },
                League { sport_id: "olybm", sport: "basketball", league: "mens-olympics-basketball", name: "Olympic Basketball (M)", games_query: "" },
                League { sport_id: "olybw", sport: "basketball", league: "womens-olympics-basketball", name: "Olympic Basketball (W)", games_query: "" },
                League { sport_id: "olysm", sport: "soccer", league: "fifa.olympics", name: "Olympic Soccer (M)", games_query: "" },
                League { sport_id: "olysw", sport: "soccer", league: "fifa.w.olympics", name: "Olympic Soccer (W)", games_query: "" },
                League { sport_id: "wwc", sport: "soccer", league: "fifa.wwc", name: "Women's World Cup", games_query: "" },
                League { sport_id: "ucl", sport: "soccer", league: "uefa.champions", name: "UEFA Champions League", games_query: "" },
                League { sport_id: "cbase", sport: "baseball", league: "college-baseball", name: "NCAA Baseball", games_query: "" },
                League { sport_id: "mls", sport: "soccer", league: "usa.1", name: "MLS", games_query: "" },
                League { sport_id: "epl", sport: "soccer", league: "eng.1", name: "Premier League", games_query: "" },
                // The World Cup is its own ESPN "league" of national teams;
                // favoriting a country works exactly like favoriting a club.
                League { sport_id: "wc", sport: "soccer", league: "fifa.world", name: "World Cup", games_query: "" },
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
    ("cfb", "College Football"),
    ("cbb", "Men's College Basketball"),
    ("wcbb", "Women's College Basketball"),
    ("wnba", "WNBA"),
    ("mls", "MLS"),
    ("epl", "Premier League"),
    ("wc", "World Cup"),
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
        "pga" | "lpga" => {
            let r: LeaderboardResponse = serde_json::from_str(raw).map_err(|e| e.to_string())?;
            Ok(r.events.into_iter().filter_map(|ev| parse_golf(feed, ev)).collect())
        }
        "f1" => {
            let r: LeaderboardResponse = serde_json::from_str(raw).map_err(|e| e.to_string())?;
            Ok(r.events.into_iter().flat_map(parse_f1).collect())
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
                // Left empty: `with_binary_owned` stamps both lists at startup,
                // so their contents cannot depend on whether the catalog came
                // from here or from the disk cache.
                championship_options: Vec::new(),
                championship_rounds: Vec::new(),
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
            out.push((lg.sport_id, lg.name, teams));
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
            for ev in events {
                if let Some(g) = parse_golf(tour, ev) {
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
        events.into_iter().flat_map(parse_f1).collect()
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
    let status = map_status(&comp.status.type_.name).unwrap_or(GameStatus::Pregame);
    // The whole field by current position (ties allowed), name = last name,
    // score = "+5"/"E". This used to cut at position 4 — a leftover from when
    // the board only ever drew four static rows — which silently capped every
    // deep-field setting: the `take` below could never reach 20 because the
    // filter had already thrown the rest away. It also made the row count
    // JUMP AROUND, because position is a RANK and ties share one: four
    // players at positions 1,2,3,3 gave four rows, while a five-way tie for
    // third gave seven, with nothing changed in the app.
    // Read before the field is consumed below.
    let tee_off = first_tee_time(&comp);
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
            Some((pos, LeaderRow { name, value: score, ..Default::default() }))
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
        ordinal: String::new(),
        clock: String::new(),
        // The FIRST tee time, not the event date — see `LeaderCompStatus::tee_time`.
        // Falls back to the event date when the field has no tee times yet
        // (ESPN publishes them a day or two out), which is no worse than before.
        start_time: tee_off.unwrap_or(comp.date),
        // Tournaments run for days — surface when this one wraps up.
        end_time: (!ev.end_date.is_empty()).then(|| ev.end_date.clone()),
        leaderboard: Some(Leaderboard { name, rows }),
        champ,
        session: None,
        situation: None,
        pregame: None,
    })
}

/// The earliest tee time in the field, or None when ESPN has not published them.
///
/// Golf's event `date` is a DATE, not a time — midnight local on day one, with
/// `timeValid: false` alongside it. The board's half-hour pre-game window
/// applied to that put a Thursday tournament on the panel at 10:30pm Wednesday.
///
/// Racing needs no equivalent: F1 competitions carry a real per-session `date`
/// with `timeValid: true`, which `f1_session_game` already uses.
fn first_tee_time(comp: &LeaderComp) -> Option<String> {
    comp.competitors
        .iter()
        .filter_map(|c| c.status.as_ref())
        .map(|s| s.tee_time.as_str())
        .filter(|t| !t.is_empty())
        // RFC3339 in a fixed UTC format, so lexical order IS chronological.
        .min()
        .map(str::to_string)
}

/// One F1 event is a whole race weekend; each session the user can subscribe to
/// (race / qualifying / sprint / sprint qualifying) becomes its OWN game with its
/// own schedule, status, and standings. Practice sessions are never shown.
fn parse_f1(ev: LeaderEvent) -> Vec<Game> {
    let base = clean_f1_name(&ev.short_name);
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
            let status = map_status(&comp.status.type_.name).unwrap_or(GameStatus::Pregame);
            let mut drivers: Vec<(i64, LeaderRow)> = comp
                .competitors
                .into_iter()
                .enumerate()
                .filter_map(|(i, c)| {
                    let code = f1_code(&c.athlete.as_ref()?.display_name);
                    let order = if c.order > 0 { c.order } else { i as i64 + 1 };
                    let value = c
                        .statistics
                        .first()
                        .map(|s| s.display_value.clone())
                        .filter(|v| !v.is_empty())
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
                    }))
                })
                .collect();
            drivers.sort_by_key(|(o, _)| *o);
            let rows = drivers.into_iter().take(crate::render::LEADERBOARD_MAX_ROWS).map(|(_, r)| r).collect();
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
                leaderboard: Some(Leaderboard { name, rows }),
                champ: None,
                session: Some(session.into()),
                situation: None,
                pregame: None,
            })
        })
        .collect()
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
        .filter(|c| c.is_ascii_alphabetic())
        .take(3)
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
    fit_words(&short.to_uppercase(), 13)
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
    #[serde(default)]
    events: Vec<LeaderEvent>,
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
}
#[derive(Deserialize)]
struct LeaderComp {
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
    /// When this player actually tees off.
    ///
    /// The event's own `date` is NOT a start time for golf: ESPN reports
    /// midnight local on day one and sets `timeValid: false` to say so. Taken
    /// literally it put a tournament on the board at 10:30pm the night before,
    /// via the half-hour pre-game window. The earliest of these is the real
    /// start of play.
    #[serde(default, rename = "teeTime")]
    tee_time: String,
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
        primary: hex_to_rgb("FFB81C"), secondary: hex_to_rgb("000000"),
    };
    let leafs = TeamSide {
        abbrev: "TOR".into(), name: "LEAFS".into(), location: "TORONTO".into(), score: 1,
        primary: hex_to_rgb("00205B"), secondary: hex_to_rgb("FFFFFF"),
    };
    let celtics = TeamSide {
        abbrev: "BOS".into(), name: "CELTICS".into(), location: "BOSTON".into(), score: 88,
        primary: hex_to_rgb("007A33"), secondary: hex_to_rgb("FFFFFF"),
    };
    let lakers = TeamSide {
        abbrev: "LAL".into(), name: "LAKERS".into(), location: "LOS ANGELES".into(), score: 84,
        primary: hex_to_rgb("552583"), secondary: hex_to_rgb("FDB927"),
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
            session: None,
            situation: Some(Situation::Football {
                down_string: match n % 4 {
                    0 => "1st + 10", 1 => "2nd + 7", 2 => "3rd + 4", _ => "4th + 1",
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
            session: None,
            situation: Some(Situation::Football {
                down_string: match (n / 2) % 4 {
                    0 => "1st + 10", 1 => "2nd + 6", 2 => "3rd + 2", _ => "4th + 8",
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
            session: None,
            situation: None,
            pregame: None,
        },
        Game {
            id: "demo-golf".into(), sport_id: "pga".into(),
            away: TeamSide::empty("PGA"), home: TeamSide::empty(""),
            status: GameStatus::Active, ordinal: String::new(), clock: String::new(), start_time: String::new(), end_time: None,
            // Ten deep so the board's scrolling leaderboard shows in demo mode.
            leaderboard: Some(Leaderboard { name: "MASTERS".into(), rows: vec![
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
            session: None,
            situation: None,
            pregame: None,
        },
        Game {
            id: "demo-golf-2".into(), sport_id: "lpga".into(),
            away: TeamSide::empty("LPGA"), home: TeamSide::empty(""),
            status: GameStatus::Active, ordinal: String::new(), clock: String::new(), start_time: String::new(), end_time: None,
            leaderboard: Some(Leaderboard { name: "US OPEN".into(), rows: vec![
                LeaderRow { name: "KORDA".into(), value: "-7".into(), ..Default::default() },
                LeaderRow { name: "THITIKUL".into(), value: "-5".into(), ..Default::default() },
                LeaderRow { name: "VU".into(), value: "-4".into(), ..Default::default() },
                LeaderRow { name: "SAIGO".into(), value: "-2".into(), ..Default::default() },
            ] }),
            champ: Some(Champ { tier: "major".into(), label: "U.S. Women's Open".into() }),
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
            leaderboard: Some(Leaderboard { name: "MONACO GP".into(), rows: vec![
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
            leaderboard: Some(Leaderboard { name: "BRITISH GP Q".into(), rows: vec![
                LeaderRow { rank: "1".into(), name: "NOR".into(), value: "1:24.8".into(), color: f1_team_color("NOR"), tire: None },
                LeaderRow { rank: "2".into(), name: "VER".into(), value: "+0.093".into(), color: f1_team_color("VER"), tire: None },
                LeaderRow { rank: "3".into(), name: "HAM".into(), value: "+0.210".into(), color: f1_team_color("HAM"), tire: None },
                LeaderRow { rank: "4".into(), name: "RUS".into(), value: "+0.286".into(), color: f1_team_color("RUS"), tire: None },
                LeaderRow { rank: "5".into(), name: "PIA".into(), value: "+0.334".into(), color: f1_team_color("PIA"), tire: None },
                LeaderRow { rank: "6".into(), name: "ALO".into(), value: "+0.412".into(), color: f1_team_color("ALO"), tire: None },
                LeaderRow { rank: "7".into(), name: "LEC".into(), value: "+0.501".into(), color: f1_team_color("LEC"), tire: None },
                LeaderRow { rank: "8".into(), name: "HUL".into(), value: "+0.688".into(), color: f1_team_color("HUL"), tire: None },
                LeaderRow { rank: "9".into(), name: "ALB".into(), value: "+0.719".into(), color: f1_team_color("ALB"), tire: None },
                LeaderRow { rank: "10".into(), name: "GAS".into(), value: "+0.854".into(), color: f1_team_color("GAS"), tire: None },
            ] }),
            champ: None,
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
    /// e.g. "Top 5th" — the original server derived baseball's inning half from
    /// this ("Top" substring check), so we do too.
    #[serde(default, rename = "shortDetail")]
    short_detail: String,
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
    #[serde(rename = "awayTeam")]
    away_team: NhlTeamRef,
    #[serde(rename = "homeTeam")]
    home_team: NhlTeamRef,
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
    let status = map_status(&comp.status.type_.name)?;
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

    let mut ordinal = ordinal_for(&comp.status, status);
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
                // "&" → "+" like the original (the 4x6 font has no ampersand).
                down_string: s.short_down_distance_text.replace('&', "+"),
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
                return tag("final", "WNBA Finals");
            }
        }
        "mls" => {
            if h.contains("mls cup") {
                return tag("cup", "MLS Cup");
            }
        }
        // World Cup rounds arrive via the season slug ("round-of-16",
        // "quarterfinals", …) rather than headlines. Group-stage games get no
        // champ tag — the "Every match" option covers them separately.
        "wc" => {
            let slug = season.map(|s| s.slug.as_str()).unwrap_or("");
            if slug.contains("semifinal") {
                return tag("semi", "World Cup Semifinal");
            }
            if slug.contains("quarterfinal") {
                return tag("quarter", "World Cup Quarterfinal");
            }
            if slug.contains("third-place") || slug.contains("round-of") || slug.contains("knockout") {
                return tag("knockout", "World Cup Knockout");
            }
            if slug.contains("final") {
                return tag("final", "World Cup Final");
            }
            return None; // group stage / unknown: never generic-postseason
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
        // Soccer runs through halves, extra time, and shootouts as distinct states.
        "STATUS_IN_PROGRESS" | "STATUS_FIRST_HALF" | "STATUS_SECOND_HALF"
        | "STATUS_OVERTIME" | "STATUS_EXTRA_TIME" | "STATUS_SHOOTOUT" => GameStatus::Active,
        "STATUS_HALFTIME" | "STATUS_END_PERIOD" | "STATUS_DELAYED" | "STATUS_SUSPENDED"
        | "STATUS_HALFTIME_ET" | "STATUS_END_OF_EXTRATIME" => GameStatus::Intermission,
        "STATUS_FINAL" | "STATUS_PLAY_COMPLETE" | "STATUS_FULL_TIME" | "STATUS_FINAL_PEN"
        | "STATUS_FINAL_AET" => GameStatus::Final,
        "STATUS_POSTPONED" | "STATUS_CANCELED" | "STATUS_ABANDONED" => return None,
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
        let games = parse_f1(ev);
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
        let games = parse_f1(ev);
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
                down_string: "3rd + 4".into(), // "&" replaced like the original
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
