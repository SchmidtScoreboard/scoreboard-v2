//! OpenF1 (openf1.org) as an ALTERNATE feed for the f1 league — richer than
//! ESPN's scoreboard (real team colours, tire compounds, per-segment quali
//! gaps that actually agree with the positions next to them), but behind a
//! personal-use API key, so it can never be the default:
//!
//! - Credentials come from the environment (`OPENF1_EMAIL` / `OPENF1_KEY`)
//!   and never travel on the wire. A server without them never offers the
//!   source in its catalog, so no app ever shows the switch.
//! - The catalog lists it as a second `data_source` for f1; a board opts in
//!   per league, and `board_view` swaps the timing rows at frame-build time.
//!   Game ids stay ESPN's, so flipping the switch compares the two feeds on
//!   the SAME game.
//! - Fetches ride the poll tick, and only while some connected board has
//!   opted in — the personal key's quota is nobody's to burn on defaults.
//!
//! Scope, deliberately: the LATEST session of the current meeting. That is
//! what a board is showing during a race weekend; older sessions of the
//! weekend keep their ESPN rows.

use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::model::LeaderRow;

pub struct OpenF1 {
    email: String,
    key: String,
    client: reqwest::Client,
    /// (bearer, expiry). Tokens live an hour; refreshed with 5 minutes left.
    token: tokio::sync::Mutex<Option<(String, Instant)>>,
}

/// The latest session's timing rows, tagged with which session they belong
/// to in the ESPN id scheme ("race" / "quali" / "sprint" / "sprint-quali").
pub struct SessionRows {
    pub session: &'static str,
    pub rows: Vec<LeaderRow>,
    pub fetched: Instant,
}

impl OpenF1 {
    /// Some(..) only when the environment carries credentials.
    pub fn from_env() -> Option<Self> {
        let email = std::env::var("OPENF1_EMAIL").ok().filter(|s| !s.is_empty())?;
        let key = std::env::var("OPENF1_KEY").ok().filter(|s| !s.is_empty())?;
        Some(Self {
            email,
            key,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("client"),
            token: tokio::sync::Mutex::new(None),
        })
    }

    async fn bearer(&self) -> Option<String> {
        let mut slot = self.token.lock().await;
        if let Some((tok, exp)) = slot.as_ref() {
            if *exp > Instant::now() + Duration::from_secs(300) {
                return Some(tok.clone());
            }
        }
        #[derive(Deserialize)]
        struct Token {
            access_token: String,
            /// The API sends this as the STRING "3600" — a Value accepts
            /// either spelling, and a missing/garbled one falls back to a
            /// short lifetime, which only costs an extra token request.
            #[serde(default)]
            expires_in: serde_json::Value,
        }
        let resp = self
            .client
            .post("https://api.openf1.org/token")
            .form(&[("username", self.email.as_str()), ("password", self.key.as_str())])
            .send()
            .await
            .map_err(|e| tracing::warn!("openf1 token: {e}"))
            .ok()?;
        let status = resp.status();
        let body = resp.text().await.ok()?;
        let t: Token = serde_json::from_str(&body)
            .map_err(|e| {
                tracing::warn!("openf1 token: HTTP {status}, parse {e} (body starts {:?})",
                    &body[..body.len().min(120)])
            })
            .ok()?;
        let secs = t
            .expires_in
            .as_u64()
            .or_else(|| t.expires_in.as_str().and_then(|v| v.parse().ok()))
            .unwrap_or(300)
            .max(60);
        let expiry = Instant::now() + Duration::from_secs(secs);
        let tok = t.access_token;
        *slot = Some((tok.clone(), expiry));
        Some(tok)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Option<T> {
        let tok = self.bearer().await?;
        let resp = self
            .client
            .get(url)
            .bearer_auth(tok)
            .send()
            .await
            .map_err(|e| tracing::warn!("openf1 {url}: {e}"))
            .ok()?;
        if !resp.status().is_success() {
            tracing::warn!("openf1 {}: HTTP {}", url, resp.status());
            return None;
        }
        // Deserialize failures LOG — this API evolves, and a silent None here
        // reads as "feed down" when the truth is "schema moved".
        let body = resp.text().await.ok()?;
        match serde_json::from_str(&body) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("openf1 {}: parse: {e} (body starts {:?})", url, &body[..body.len().min(120)]);
                None
            }
        }
    }

    /// The latest session's rows, or None on any failure — the caller keeps
    /// serving ESPN rows, which is the whole point of a backup source.
    pub async fn fetch_latest(&self) -> Option<SessionRows> {
        let sessions: Vec<Session> =
            self.get_json("https://api.openf1.org/v1/sessions?session_key=latest").await?;
        let session = sessions.into_iter().next()?;
        let kind = session_kind(&session.session_name)?;
        let key = session.session_key;

        let drivers: Vec<Driver> = self
            .get_json(&format!("https://api.openf1.org/v1/drivers?session_key={key}"))
            .await?;

        // Live order: the last position record per driver. Ended session:
        // session_result is the official classification (and for quali the
        // only coherent gap source — intervals are race-only).
        let ended = session
            .date_end
            .as_deref()
            .and_then(parse_rfc3339)
            .map(|end| end < chrono::Utc::now())
            .unwrap_or(false);

        let rows = if ended || kind == "quali" || kind == "sprint-quali" {
            let results: Vec<SessionResult> = self
                .get_json(&format!("https://api.openf1.org/v1/session_result?session_key={key}"))
                .await?;
            rows_from_results(&results, &drivers)
        } else {
            let positions: Vec<Position> = self
                .get_json(&format!("https://api.openf1.org/v1/position?session_key={key}"))
                .await?;
            let intervals: Vec<Interval> = self
                .get_json(&format!("https://api.openf1.org/v1/intervals?session_key={key}"))
                .await
                .unwrap_or_default();
            let stints: Vec<Stint> = self
                .get_json(&format!("https://api.openf1.org/v1/stints?session_key={key}"))
                .await
                .unwrap_or_default();
            rows_from_live(&positions, &intervals, &stints, &drivers)
        };
        if rows.is_empty() {
            return None;
        }
        tracing::info!("openf1: {} rows for {} (session {key})", rows.len(), kind);
        Some(SessionRows { session: kind, rows, fetched: Instant::now() })
    }
}

// ---- wire shapes (all fields optional-ish: this API evolves) ----

#[derive(Deserialize)]
struct Session {
    session_key: u64,
    #[serde(default)]
    session_name: String,
    #[serde(default)]
    date_end: Option<String>,
}
#[derive(Deserialize)]
struct Driver {
    driver_number: u32,
    #[serde(default)]
    name_acronym: String,
    #[serde(default)]
    team_colour: Option<String>,
}
#[derive(Deserialize)]
struct Position {
    driver_number: u32,
    position: u32,
    #[serde(default)]
    date: String,
}
#[derive(Deserialize)]
struct Interval {
    driver_number: u32,
    /// Number of seconds, or a string like "+1 LAP" for lapped cars, or null
    /// for the leader. serde_json::Value covers all three honestly.
    #[serde(default)]
    gap_to_leader: serde_json::Value,
    #[serde(default)]
    date: String,
}
#[derive(Deserialize)]
struct Stint {
    driver_number: u32,
    #[serde(default)]
    compound: Option<String>,
    #[serde(default)]
    lap_start: u32,
}
#[derive(Deserialize)]
struct SessionResult {
    driver_number: u32,
    position: u32,
    /// Per-segment for quali ([Q1, Q2, Q3]); scalar for races. The LAST
    /// segment a driver reached carries their final gap.
    #[serde(default)]
    gap_to_leader: serde_json::Value,
    #[serde(default)]
    dnf: bool,
    #[serde(default)]
    dns: bool,
    #[serde(default)]
    dsq: bool,
}

/// OpenF1 session_name → the session suffix ESPN game ids use.
fn session_kind(name: &str) -> Option<&'static str> {
    match name {
        "Race" => Some("race"),
        "Qualifying" => Some("quali"),
        "Sprint" => Some("sprint"),
        "Sprint Qualifying" | "Sprint Shootout" => Some("sprint-quali"),
        _ => None, // practice
    }
}

fn parse_rfc3339(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&chrono::Utc))
}

/// "+1.360" from 1.36; "+1L" from "+1 LAP"; blank for the leader / null.
fn format_gap(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            if f <= 0.0 { String::new() } else { format!("+{f:.3}") }
        }
        serde_json::Value::String(s) => {
            s.to_uppercase().replace(" LAPS", "L").replace(" LAP", "L")
        }
        _ => String::new(),
    }
}

fn driver_row(
    n: u32,
    drivers: &[Driver],
) -> (String, Option<embedded_graphics::pixelcolor::Rgb888>) {
    let d = drivers.iter().find(|d| d.driver_number == n);
    let code = d
        .map(|d| d.name_acronym.clone())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| format!("#{n}"));
    // OpenF1 carries each team's REAL colour ("F47600"), which beats the
    // hardcoded map the ESPN path uses.
    let color = d
        .and_then(|d| d.team_colour.as_deref())
        .filter(|h| h.len() == 6)
        .map(crate::model::hex_to_rgb);
    (code, color)
}

fn rows_from_results(results: &[SessionResult], drivers: &[Driver]) -> Vec<LeaderRow> {
    let mut sorted: Vec<&SessionResult> = results.iter().collect();
    sorted.sort_by_key(|r| r.position);
    sorted
        .iter()
        .take(crate::render::LEADERBOARD_MAX_ROWS)
        .map(|r| {
            let (name, color) = driver_row(r.driver_number, drivers);
            let value = if r.dsq {
                "DSQ".into()
            } else if r.dns {
                "DNS".into()
            } else if r.dnf {
                "DNF".into()
            } else {
                // Quali carries per-segment arrays; the last entry is the
                // final segment the driver reached. Races carry a scalar.
                match &r.gap_to_leader {
                    serde_json::Value::Array(a) => a.last().map(format_gap).unwrap_or_default(),
                    v => format_gap(v),
                }
            };
            LeaderRow { rank: r.position.to_string(), name, value, color, tire: None, thru: None }
        })
        .collect()
}

fn rows_from_live(
    positions: &[Position],
    intervals: &[Interval],
    stints: &[Stint],
    drivers: &[Driver],
) -> Vec<LeaderRow> {
    use std::collections::HashMap;
    // Last record per driver wins — the feeds are append-only time series.
    let mut pos: HashMap<u32, (&str, u32)> = HashMap::new();
    for p in positions {
        let e = pos.entry(p.driver_number).or_insert((p.date.as_str(), p.position));
        if p.date.as_str() >= e.0 {
            *e = (p.date.as_str(), p.position);
        }
    }
    let mut gap: HashMap<u32, (&str, &serde_json::Value)> = HashMap::new();
    for i in intervals {
        let e = gap.entry(i.driver_number).or_insert((i.date.as_str(), &i.gap_to_leader));
        if i.date.as_str() >= e.0 {
            *e = (i.date.as_str(), &i.gap_to_leader);
        }
    }
    let mut tire: HashMap<u32, (u32, Option<&str>)> = HashMap::new();
    for s in stints {
        let e = tire.entry(s.driver_number).or_insert((s.lap_start, s.compound.as_deref()));
        if s.lap_start >= e.0 {
            *e = (s.lap_start, s.compound.as_deref());
        }
    }

    let mut order: Vec<(u32, u32)> = pos.iter().map(|(n, (_, p))| (*p, *n)).collect();
    order.sort();
    order
        .into_iter()
        .take(crate::render::LEADERBOARD_MAX_ROWS)
        .map(|(p, n)| {
            let (name, color) = driver_row(n, drivers);
            let value = if p == 1 {
                String::new()
            } else {
                gap.get(&n).map(|(_, v)| format_gap(v)).unwrap_or_default()
            };
            LeaderRow {
                rank: p.to_string(),
                name,
                value,
                color,
                tire: tire.get(&n).and_then(|(_, c)| tire_code(c.unwrap_or(""))),
                thru: None,
            }
        })
        .collect()
}

/// OpenF1 compounds → the single-letter code the render's tire strip colors.
fn tire_code(compound: &str) -> Option<char> {
    match compound {
        "SOFT" => Some('S'),
        "MEDIUM" => Some('M'),
        "HARD" => Some('H'),
        "INTERMEDIATE" => Some('I'),
        "WET" => Some('W'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drivers() -> Vec<Driver> {
        serde_json::from_str(
            r#"[{"driver_number":1,"name_acronym":"NOR","team_colour":"F47600"},
                {"driver_number":63,"name_acronym":"RUS","team_colour":"00A19C"},
                {"driver_number":12,"name_acronym":"ANT","team_colour":"00A19C"}]"#,
        )
        .unwrap()
    }

    /// The session_result shape captured live during 2026 Dutch GP quali:
    /// per-segment arrays, where the LAST segment carries the final gap —
    /// the coherent quali gap ESPN cannot provide.
    #[test]
    fn quali_results_read_the_final_segment_gap() {
        let results: Vec<SessionResult> = serde_json::from_str(
            r#"[{"position":2,"driver_number":63,"gap_to_leader":[0.314,0.331,0.102],"dnf":false,"dns":false,"dsq":false},
                {"position":1,"driver_number":1,"gap_to_leader":[0.085,0.0,0.0],"dnf":false,"dns":false,"dsq":false},
                {"position":3,"driver_number":12,"gap_to_leader":[0.412,0.287,0.133],"dnf":true,"dns":false,"dsq":false}]"#,
        )
        .unwrap();
        let rows = rows_from_results(&results, &drivers());
        assert_eq!(rows[0].name, "NOR");
        assert_eq!(rows[0].value, "", "the leader's zero gap draws blank, not +0.000");
        assert_eq!(rows[1].name, "RUS");
        assert_eq!(rows[1].value, "+0.102", "final segment, not first");
        assert_eq!(rows[2].value, "DNF", "status labels beat numbers");
        assert_eq!(
            rows[0].color,
            Some(crate::model::hex_to_rgb("F47600")),
            "the FEED's team colour, not the hardcoded map"
        );
    }

    /// Live rows: latest record per driver wins, tires ride along, lapped
    /// cars keep their string gap tightened for the panel.
    #[test]
    fn live_rows_take_the_latest_record_per_driver() {
        let positions: Vec<Position> = serde_json::from_str(
            r#"[{"driver_number":1,"position":2,"date":"2026-08-23T13:00:00+00:00"},
                {"driver_number":1,"position":1,"date":"2026-08-23T13:05:00+00:00"},
                {"driver_number":63,"position":1,"date":"2026-08-23T13:00:00+00:00"},
                {"driver_number":63,"position":2,"date":"2026-08-23T13:05:00+00:00"},
                {"driver_number":12,"position":3,"date":"2026-08-23T13:04:00+00:00"}]"#,
        )
        .unwrap();
        let intervals: Vec<Interval> = serde_json::from_str(
            r#"[{"driver_number":63,"gap_to_leader":1.36,"date":"2026-08-23T13:05:00+00:00"},
                {"driver_number":12,"gap_to_leader":"+1 LAP","date":"2026-08-23T13:05:00+00:00"},
                {"driver_number":1,"gap_to_leader":null,"date":"2026-08-23T13:05:00+00:00"}]"#,
        )
        .unwrap();
        let stints: Vec<Stint> = serde_json::from_str(
            r#"[{"driver_number":1,"compound":"MEDIUM","lap_start":1},
                {"driver_number":1,"compound":"SOFT","lap_start":20},
                {"driver_number":63,"compound":"HARD","lap_start":15}]"#,
        )
        .unwrap();
        let rows = rows_from_live(&positions, &intervals, &stints, &drivers());
        assert_eq!(rows[0].name, "NOR", "latest position record wins");
        assert_eq!(rows[0].value, "", "leader gap blank");
        assert_eq!(rows[0].tire, Some('S'), "latest stint's compound");
        assert_eq!(rows[1].name, "RUS");
        assert_eq!(rows[1].value, "+1.360");
        assert_eq!(rows[1].tire, Some('H'));
        assert_eq!(rows[2].value, "+1L", "lapped cars tighten for 64px");
    }

    #[test]
    fn session_names_map_to_espn_id_suffixes() {
        assert_eq!(session_kind("Race"), Some("race"));
        assert_eq!(session_kind("Qualifying"), Some("quali"));
        assert_eq!(session_kind("Sprint"), Some("sprint"));
        assert_eq!(session_kind("Sprint Qualifying"), Some("sprint-quali"));
        assert_eq!(session_kind("Practice 1"), None, "practice never renders");
    }

    /// Runs only by hand (`cargo test -p server openf1_live -- --ignored`),
    /// with real credentials in the environment: proves the token flow and
    /// the whole fetch against the actual API.
    #[tokio::test]
    #[ignore]
    async fn openf1_live_fetch_round_trip() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let Some(client) = OpenF1::from_env() else {
            panic!("set OPENF1_EMAIL / OPENF1_KEY to run this");
        };
        let rows = client.fetch_latest().await.expect("fetch");
        assert!(!rows.rows.is_empty());
        eprintln!("session {}: {} rows; P1 {} {}", rows.session, rows.rows.len(), rows.rows[0].name, rows.rows[0].value);
    }
}
