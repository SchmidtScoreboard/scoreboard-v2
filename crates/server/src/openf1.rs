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
    /// Live qualifying: the segment being run (1..=3), read exactly from
    /// race control's session-status stream. None otherwise.
    pub stage: Option<u8>,
    /// Live qualifying: the segment's elimination zones. None otherwise.
    pub quali: Option<crate::model::QualiZones>,
    /// Current track status: "red" / "yellow" recolor the frame's title-row
    /// flags. None = green-or-nothing, which draws the checkered pair.
    pub flag: Option<&'static str>,
    /// Race and sprint: the leader's completed laps out of the circuit's
    /// scheduled distance, for the lap bar. None when the distance is not
    /// known or the session is not a race.
    pub laps: Option<crate::model::LapProgress>,
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

        // Prefer the official classification for ended sessions and quali —
        // but session_result 404s until OpenF1 PUBLISHES it, which can lag
        // the chequered flag by a while (and `ended` flips at the PLANNED
        // end time, which a red flag blows straight through). So each path
        // falls back to the other: live timing keeps flowing after the
        // flag, and a session that ended long ago has results by now.
        let is_quali = kind == "quali" || kind == "sprint-quali";
        let results_first = ended || is_quali;
        let from_results = |results: Vec<SessionResult>, drivers: &[Driver]| {
            let r = rows_from_results(&results, drivers);
            (!r.is_empty()).then_some(r)
        };
        let mut rows = None;
        // Laps the leader has completed, from the live laps feed. A race the
        // official classification covers is over: every lap is done.
        let mut laps_done: Option<u32> = None;
        if results_first {
            if let Some(results) = self
                .get_json(&format!("https://api.openf1.org/v1/session_result?session_key={key}"))
                .await
            {
                rows = from_results(results, &drivers);
                laps_done = rows.as_ref().map(|_| u32::MAX);
            }
        }
        // Race control serves two readers: the track flag below, and a live
        // qualifying's segment boundaries. Fetched once, up front.
        let msgs: Vec<RaceControl> = if ended && !is_quali {
            Vec::new()
        } else {
            self.get_json(&format!("https://api.openf1.org/v1/race_control?session_key={key}"))
                .await
                .unwrap_or_default()
        };
        let (mut stage, mut zones) = (None, None);
        if rows.is_none() {
            let positions: Vec<Position> = self
                .get_json(&format!("https://api.openf1.org/v1/position?session_key={key}"))
                .await
                .unwrap_or_default();
            let stints: Vec<Stint> = self
                .get_json(&format!("https://api.openf1.org/v1/stints?session_key={key}"))
                .await
                .unwrap_or_default();
            if is_quali {
                // Intervals are race-only; a live qualifying's numbers come
                // from the laps themselves, segmented by race control.
                let laps: Vec<Lap> = self
                    .get_json(&format!("https://api.openf1.org/v1/laps?session_key={key}"))
                    .await
                    .unwrap_or_default();
                let (live, st, z) = quali_rows_live(&positions, &laps, &msgs, &stints, &drivers);
                if !live.is_empty() {
                    rows = Some(live);
                    stage = Some(st);
                    zones = z;
                }
            } else {
                let intervals: Vec<Interval> = self
                    .get_json(&format!("https://api.openf1.org/v1/intervals?session_key={key}"))
                    .await
                    .unwrap_or_default();
                let live = rows_from_live(&positions, &intervals, &stints, &drivers);
                if !live.is_empty() {
                    rows = Some(live);
                }
                // The leader's laps alone (one small request): the lap being
                // run already has a record, so completed = the highest
                // number less one.
                if let Some(leader) = leader_number(&positions) {
                    let laps: Vec<Lap> = self
                        .get_json(&format!(
                            "https://api.openf1.org/v1/laps?session_key={key}&driver_number={leader}"
                        ))
                        .await
                        .unwrap_or_default();
                    laps_done = Some(laps_completed(&laps));
                }
            }
        }
        if rows.is_none() && !results_first {
            if let Some(results) = self
                .get_json(&format!("https://api.openf1.org/v1/session_result?session_key={key}"))
                .await
            {
                rows = from_results(results, &drivers);
                laps_done = rows.as_ref().map(|_| u32::MAX);
            }
        }
        let rows = rows.unwrap_or_default();
        if rows.is_empty() {
            return None;
        }
        // Track status only matters while the session runs — a finished
        // session's chequered flag is just the checkered glyphs.
        let flag = if ended { None } else { track_flag(&msgs) };
        let laps = match kind {
            "race" | "sprint" => crate::laps::total_laps(&session.circuit_short_name, kind == "sprint")
                .map(|total| crate::model::LapProgress { completed: laps_done.unwrap_or(0).min(total), total }),
            _ => None,
        };
        tracing::info!(
            "openf1: {} rows for {} (session {key}, flag {flag:?}, stage {stage:?}, laps {laps:?})",
            rows.len(),
            kind
        );
        Some(SessionRows { session: kind, rows, stage, quali: zones, flag, laps, fetched: Instant::now() })
    }
}

/// Replay `fetch_latest` over one captured OpenF1 folder (`sessions.json`,
/// `drivers.json`, `position.json`, … as `scripts/capture-f1.py` writes them),
/// as if the poll had run at `at`. A MIRROR of `fetch_latest`'s precedence —
/// classification first for quali and ended sessions, live timing otherwise,
/// each falling back to the other, no flag once ended — with the network
/// replaced by files. Keep the two in step: a replay that disagrees with the
/// server is worse than none.
///
/// Two capture-shaped differences, both harmless to the rows: `intervals.json`
/// holds the last three minutes rather than the whole session (every running
/// car reports every few seconds, so the latest-per-driver is the same), and
/// for a race `laps.json` holds only the leader's laps, which is all the
/// server fetches too.
#[cfg(test)]
pub(crate) fn replay_snapshot(dir: &std::path::Path, at: chrono::DateTime<chrono::Utc>) -> Option<SessionRows> {
    fn load<T: serde::de::DeserializeOwned>(dir: &std::path::Path, name: &str) -> Vec<T> {
        std::fs::read_to_string(dir.join(format!("{name}.json")))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }
    let session = load::<Session>(dir, "sessions").into_iter().next()?;
    let kind = session_kind(&session.session_name)?;
    let drivers: Vec<Driver> = load(dir, "drivers");
    let ended = session.date_end.as_deref().and_then(parse_rfc3339).map(|end| end < at).unwrap_or(false);
    let is_quali = kind == "quali" || kind == "sprint-quali";
    let results_first = ended || is_quali;
    let from_results = || {
        let r = rows_from_results(&load::<SessionResult>(dir, "session_result"), &drivers);
        (!r.is_empty()).then_some(r)
    };
    let mut rows = None;
    let mut laps_done: Option<u32> = None;
    if results_first {
        rows = from_results();
        laps_done = rows.as_ref().map(|_| u32::MAX);
    }
    let msgs: Vec<RaceControl> = if ended && !is_quali { Vec::new() } else { load(dir, "race_control") };
    let (mut stage, mut zones) = (None, None);
    if rows.is_none() {
        let positions: Vec<Position> = load(dir, "position");
        let stints: Vec<Stint> = load(dir, "stints");
        if is_quali {
            let laps: Vec<Lap> = load(dir, "laps");
            let (live, st, z) = quali_rows_live(&positions, &laps, &msgs, &stints, &drivers);
            if !live.is_empty() {
                rows = Some(live);
                stage = Some(st);
                zones = z;
            }
        } else {
            let intervals: Vec<Interval> = load(dir, "intervals");
            let live = rows_from_live(&positions, &intervals, &stints, &drivers);
            if !live.is_empty() {
                rows = Some(live);
            }
            if leader_number(&positions).is_some() {
                laps_done = Some(laps_completed(&load::<Lap>(dir, "laps")));
            }
        }
    }
    if rows.is_none() && !results_first {
        rows = from_results();
        laps_done = rows.as_ref().map(|_| u32::MAX);
    }
    let rows = rows.filter(|r| !r.is_empty())?;
    let flag = if ended { None } else { track_flag(&msgs) };
    let laps = match kind {
        "race" | "sprint" => crate::laps::total_laps(&session.circuit_short_name, kind == "sprint")
            .map(|total| crate::model::LapProgress { completed: laps_done.unwrap_or(0).min(total), total }),
        _ => None,
    };
    Some(SessionRows { session: kind, rows, stage, quali: zones, flag, laps, fetched: Instant::now() })
}

// ---- wire shapes (all fields optional-ish: this API evolves) ----

#[derive(Deserialize)]
struct Session {
    session_key: u64,
    #[serde(default)]
    session_name: String,
    #[serde(default)]
    date_end: Option<String>,
    /// "Madring", "Monte Carlo": keys the scheduled lap count.
    #[serde(default)]
    circuit_short_name: String,
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
    /// Seconds to the CAR AHEAD — the broadcast tower's number, and the one
    /// the panel shows for a live race: it says who is actually racing whom,
    /// where gap-to-leader turns the midfield into large meaningless numbers.
    /// A number, or a string like "+1 LAP" for lapped cars, or null for the
    /// leader. serde_json::Value covers all three honestly.
    #[serde(default)]
    interval: serde_json::Value,
    #[serde(default)]
    date: String,
}
#[derive(Deserialize)]
struct Lap {
    driver_number: u32,
    #[serde(default)]
    lap_number: u32,
    /// Seconds; null for a lap still being run.
    #[serde(default)]
    lap_duration: Option<f64>,
    #[serde(default)]
    date_start: Option<String>,
    /// The lap that leaves the pits — never a flying lap.
    #[serde(default)]
    is_pit_out_lap: bool,
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
    /// NULL for retired cars that were not classified — the 2026 Dutch GP's
    /// official result carried five of them, and a strict u32 here failed
    /// the WHOLE array's parse, locking the fallback to live timing forever.
    #[serde(default)]
    position: Option<u32>,
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

#[derive(Deserialize, Clone)]
struct RaceControl {
    #[serde(default)]
    category: String,
    #[serde(default)]
    flag: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    message: String,
    #[serde(default)]
    date: String,
}

/// The track status a session is under, from the race-control stream.
///
/// Learned from the REAL stream during the 2026 Dutch GP (red-flagged five
/// minutes in): a red flag arrives as category "Other" with the message
/// "RED FLAG - RACE SUSPENDED" and `flag: None`, alongside SessionStatus
/// "SESSION ABORTED" — and a "TRACK CLEAR" flag message follows minutes
/// later while the race is STILL suspended (marshals clearing debris is not
/// a restart). So:
///
/// - RED holds until a SESSION-level resume (SessionStatus RESUMED/STARTED
///   or a green-flag message), never a mere track-clear.
/// - YELLOW = safety car / VSC deployed and not yet ended, or a TRACK-scope
///   yellow not yet cleared. Sector yellows are local waving, not session
///   state, and are deliberately ignored.
fn track_flag(msgs: &[RaceControl]) -> Option<&'static str> {
    let mut last_red = "";
    let mut last_session_green = "";
    let mut last_sc_on = "";
    let mut last_sc_off = "";
    let mut last_track_yellow = "";
    let mut last_track_clear = "";
    for m in msgs {
        let d = m.date.as_str();
        let up = m.message.to_uppercase();
        let is_flag = m.category == "Flag";
        let track = m.scope.as_deref() == Some("Track");
        if (is_flag && m.flag.as_deref() == Some("RED"))
            || up.contains("RED FLAG")
            || (m.category == "SessionStatus" && (up.contains("ABORT") || up.contains("SUSPEND")))
        {
            if d > last_red {
                last_red = d;
            }
        }
        if (m.category == "SessionStatus"
            && (up.contains("RESUM") || up.contains("START") || up.contains("GREEN")))
            || up.contains("GREEN LIGHT")
            || up.contains("GREEN FLAG")
            || (is_flag && track && m.flag.as_deref() == Some("GREEN"))
        {
            if d > last_session_green {
                last_session_green = d;
            }
        }
        // Safety car signals arrive BOTH as a SafetyCar category and as plain
        // "Other" messages — the 2026 Dutch GP restarted behind the SC with
        // nothing but "SAFETY CAR LIGHTS ON" (category Other). "RACE WILL
        // RESUME AT .." is an announcement made DURING the stoppage, so it is
        // deliberately not a resume signal.
        let sc_end = up.contains("IN THIS LAP")
            || up.contains("ENDING")
            || up.contains("LIGHTS OFF")
            // The green flag IS the restart — it ends the SC period too.
            || (is_flag && track && m.flag.as_deref() == Some("GREEN"));
        let sc_start = up.contains("SAFETY CAR DEPLOYED")
            || up.contains("VIRTUAL SAFETY CAR DEPLOYED")
            || up.contains("SAFETY CAR LIGHTS ON")
            || (m.category == "SafetyCar" && !sc_end);
        if sc_end {
            if d > last_sc_off {
                last_sc_off = d;
            }
        } else if sc_start {
            if d > last_sc_on {
                last_sc_on = d;
            }
        }
        if is_flag && track {
            match m.flag.as_deref() {
                Some("YELLOW") | Some("DOUBLE YELLOW") => {
                    if d > last_track_yellow {
                        last_track_yellow = d;
                    }
                }
                Some("CLEAR") | Some("GREEN") => {
                    if d > last_track_clear {
                        last_track_clear = d;
                    }
                }
                _ => {}
            }
        }
    }
    // A safety car LEADING CARS means the race is running — under yellow,
    // but running. That is how a red actually ends when the restart is
    // behind the SC (no session-resume message ever arrives).
    let red_over = [last_session_green, last_sc_on].into_iter().max().unwrap_or("");
    if !last_red.is_empty() && last_red > red_over {
        return Some("red");
    }
    // A session-level green ends the SC period and track yellows too: the
    // Dutch GP's SC phase closed with nothing but "SESSION STARTED" (the
    // rolling restart) — no IN-THIS-LAP, no lights-off, no green flag. The
    // same rule retires the pre-race "SAFETY CAR LIGHTS ON" that precedes
    // the formation lap, which otherwise painted the start yellow.
    if !last_sc_on.is_empty() && last_sc_on > last_sc_off && last_sc_on > last_session_green {
        return Some("yellow");
    }
    if !last_track_yellow.is_empty()
        && last_track_yellow > last_track_clear
        && last_track_yellow > last_session_green
    {
        return Some("yellow");
    }
    None
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
            // Hundredths — thousandths read as noise at four pixels a digit.
            if f <= 0.0 { String::new() } else { format!("+{f:.2}") }
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
    // Unclassified retirements (position null) sort to the bottom, where the
    // broadcast puts them; the depth cap usually truncates them anyway.
    sorted.sort_by_key(|r| r.position.unwrap_or(u32::MAX));
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
            let rank = r.position.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
            LeaderRow { rank, name, value, color, tire: None, thru: None }
        })
        .collect()
}

/// Who leads: the driver whose LATEST position record says 1.
fn leader_number(positions: &[Position]) -> Option<u32> {
    use std::collections::HashMap;
    let mut pos: HashMap<u32, (&str, u32)> = HashMap::new();
    for p in positions {
        let e = pos.entry(p.driver_number).or_insert((p.date.as_str(), p.position));
        if p.date.as_str() >= e.0 {
            *e = (p.date.as_str(), p.position);
        }
    }
    pos.iter().find(|(_, (_, p))| *p == 1).map(|(n, _)| *n)
}

/// Laps a driver has completed, from their lap records: the feed opens a
/// record when a lap STARTS (the quali capture's out-laps carried sector
/// times and no duration), so the highest lap number is the one being run.
fn laps_completed(laps: &[Lap]) -> u32 {
    laps.iter().map(|l| l.lap_number).max().map_or(0, |m| m.saturating_sub(1))
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
        let e = gap.entry(i.driver_number).or_insert((i.date.as_str(), &i.interval));
        if i.date.as_str() >= e.0 {
            *e = (i.date.as_str(), &i.interval);
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

/// When each qualifying segment began, from race control's session-status
/// stream. A segment is a SESSION STARTED that follows a SESSION FINISHED
/// (or is the first): a restart after a red flag also arrives as STARTED,
/// and must not count as Q2. Dates are RFC3339 in one fixed format, so the
/// strings sort as times.
fn segment_starts(msgs: &[RaceControl]) -> Vec<String> {
    let mut status: Vec<&RaceControl> = msgs.iter().filter(|m| m.category == "SessionStatus").collect();
    status.sort_by(|a, b| a.date.cmp(&b.date));
    let mut starts = Vec::new();
    let mut finished = true;
    for m in status {
        let up = m.message.to_uppercase();
        if up.contains("FINISH") {
            finished = true;
        } else if up.contains("START") && finished {
            starts.push(m.date.clone());
            finished = false;
        }
    }
    starts
}

/// Lap times race control has deleted (track limits), as (car, milliseconds):
/// "CAR 41 (LIN) TIME 1:35.149 DELETED - TRACK LIMITS AT TURN 17 LAP 3".
/// The laps feed keeps the lap as driven, so this is the only way to know
/// it no longer counts. A "LAP DELETED" without a time was never a
/// flying lap and needs nothing.
fn deleted_laps(msgs: &[RaceControl]) -> std::collections::HashSet<(u32, i64)> {
    msgs.iter()
        .filter(|m| m.message.contains("DELETED"))
        .filter_map(|m| {
            let car: u32 = m.message.strip_prefix("CAR ")?.split(' ').next()?.parse().ok()?;
            let (_, rest) = m.message.split_once("TIME ")?;
            let ms = parse_lap_time(rest.split(' ').next()?)?;
            Some((car, ms))
        })
        .collect()
}

/// "1:35.149" -> 95149 (milliseconds). Integer so lap durations from the
/// laps feed (a float) can be matched exactly after rounding.
fn parse_lap_time(t: &str) -> Option<i64> {
    let (m, s) = t.split_once(':')?;
    let m: i64 = m.parse().ok()?;
    let s: f64 = s.parse().ok()?;
    Some(m * 60_000 + (s * 1000.0).round() as i64)
}

/// 95.312 -> "1:35.31": a lap time in hundredths, like every other timing
/// on the panel.
fn format_lap(secs: f64) -> String {
    let m = (secs / 60.0).floor();
    format!("{}:{:05.2}", m as u32, secs - m * 60.0)
}

/// A live qualifying tower: order from the position feed (the timing
/// screen), numbers from the laps. Each driver's best lap in the running
/// segment sets the column — the leader's as a lap time, the rest as the
/// gap to it — and a driver yet to set one this segment draws blank rather
/// than last segment's number (see the ESPN path for why that misleads).
/// Drivers already eliminated show the best lap of the last segment they
/// ran, which the renderer greys; the position feed keeps them at the
/// bottom. Deleted laps are removed before any of this.
///
/// Returns the rows, the segment (1..=3), and its elimination zones.
fn quali_rows_live(
    positions: &[Position],
    laps: &[Lap],
    msgs: &[RaceControl],
    stints: &[Stint],
    drivers: &[Driver],
) -> (Vec<LeaderRow>, u8, Option<crate::model::QualiZones>) {
    use std::collections::HashMap;
    let starts = segment_starts(msgs);
    let stage = starts.len().clamp(1, 3) as u8;
    let segment_of = |date: &str| -> usize {
        starts.iter().filter(|s| s.as_str() <= date).count().saturating_sub(1).min(2)
    };
    let deleted = deleted_laps(msgs);
    // Only FLYING laps count. The feed times every lap, including the
    // out-lap (flagged) and the in-lap (not flagged — but the driver's next
    // lap is a pit-out, which is how it is recognised): a driver whose only
    // Q2 laps were out and in showed "+20.63" here, where the timing screen
    // had no time for them at all. What survives is then held to 107% of
    // the segment's fastest lap — the FIA's own qualifying threshold — so a
    // cool-down lap with no pit stop after it cannot pose as a best either.
    let pit_out: std::collections::HashSet<(u32, u32)> =
        laps.iter().filter(|l| l.is_pit_out_lap).map(|l| (l.driver_number, l.lap_number)).collect();
    let mut best: HashMap<u32, [Option<f64>; 3]> = HashMap::new();
    let mut fastest: [Option<f64>; 3] = [None; 3];
    for lap in laps {
        let (Some(d), Some(ds)) = (lap.lap_duration, lap.date_start.as_deref()) else { continue };
        if d <= 0.0
            || lap.is_pit_out_lap
            || pit_out.contains(&(lap.driver_number, lap.lap_number + 1))
            || deleted.contains(&(lap.driver_number, (d * 1000.0).round() as i64))
        {
            continue;
        }
        let seg = segment_of(ds);
        let slot = &mut best.entry(lap.driver_number).or_default()[seg];
        if slot.is_none_or(|b| d < b) {
            *slot = Some(d);
        }
        if fastest[seg].is_none_or(|f| d < f) {
            fastest[seg] = Some(d);
        }
    }
    for b in best.values_mut() {
        for (seg, slot) in b.iter_mut().enumerate() {
            if let (Some(d), Some(f)) = (*slot, fastest[seg]) {
                if d > f * 1.07 {
                    *slot = None;
                }
            }
        }
    }

    let mut pos: HashMap<u32, (&str, u32)> = HashMap::new();
    for p in positions {
        let e = pos.entry(p.driver_number).or_insert((p.date.as_str(), p.position));
        if p.date.as_str() >= e.0 {
            *e = (p.date.as_str(), p.position);
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
    let zones = crate::model::QualiZones::for_field(order.len(), stage);
    let cur = (stage - 1) as usize;
    let leader_best = order.first().and_then(|(_, n)| best.get(n).and_then(|b| b[cur]));
    let rows = order
        .into_iter()
        .take(crate::render::LEADERBOARD_MAX_ROWS)
        .map(|(p, n)| {
            let (name, color) = driver_row(n, drivers);
            let b = best.get(&n);
            let out = zones.is_some_and(|z| p > z.field);
            let value = if out {
                b.and_then(|b| b.iter().rev().flatten().next().copied()).map(format_lap).unwrap_or_default()
            } else {
                match (b.and_then(|b| b[cur]), leader_best) {
                    (Some(mine), _) if p == 1 => format_lap(mine),
                    (Some(mine), Some(lead)) if mine >= lead => format!("+{:.2}", mine - lead),
                    _ => String::new(),
                }
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
        .collect();
    (rows, stage, zones)
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
        assert_eq!(rows[1].value, "+0.10", "final segment, not first, in hundredths");
        assert_eq!(rows[2].value, "DNF", "status labels beat numbers");
        assert_eq!(
            rows[0].color,
            Some(crate::model::hex_to_rgb("F47600")),
            "the FEED's team colour, not the hardcoded map"
        );
    }

    /// The 2026 Dutch GP's OFFICIAL classification, verbatim: unclassified
    /// retirements carry position NULL — a strict integer failed the whole
    /// array and locked the results path out permanently.
    #[test]
    fn null_positions_in_the_official_classification_parse_and_sink() {
        let results: Vec<SessionResult> = serde_json::from_str(
            r#"[{"position":null,"driver_number":77,"dnf":true,"dns":false,"dsq":false,"gap_to_leader":null},
                {"position":1,"driver_number":1,"dnf":false,"dns":false,"dsq":false,"gap_to_leader":0},
                {"position":2,"driver_number":12,"dnf":false,"dns":false,"dsq":false,"gap_to_leader":11.536},
                {"position":17,"driver_number":23,"dnf":true,"dns":false,"dsq":false,"gap_to_leader":null}]"#,
        )
        .unwrap();
        let rows = rows_from_results(&results, &[]);
        assert_eq!(rows[0].rank, "1");
        assert_eq!(rows[1].value, "+11.54", "gap to the winner, hundredths");
        assert_eq!(rows[2].rank, "17");
        assert_eq!(rows[2].value, "DNF", "a CLASSIFIED retirement keeps its position");
        assert_eq!(rows[3].rank, "-", "an unclassified one sinks with no number");
        assert_eq!(rows[3].value, "DNF");
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
            r#"[{"driver_number":63,"gap_to_leader":50.038,"interval":1.36,"date":"2026-08-23T13:05:00+00:00"},
                {"driver_number":12,"gap_to_leader":"+1 LAP","interval":"+1 LAP","date":"2026-08-23T13:05:00+00:00"},
                {"driver_number":1,"gap_to_leader":null,"interval":null,"date":"2026-08-23T13:05:00+00:00"}]"#,
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
        assert_eq!(rows[1].value, "+1.36", "the INTERVAL to the car ahead, in hundredths");
        assert_eq!(rows[1].tire, Some('H'));
        assert_eq!(rows[2].value, "+1L", "lapped cars tighten for 64px");
    }

    /// Q2 of the 2026 Spanish GP, shapes verbatim: segments come from the
    /// session-status stream (a red-flag restart is NOT a new segment), the
    /// leader shows a lap time and the rest the gap to it, a driver without
    /// a lap this segment draws blank, and a deleted time is not a best lap.
    #[test]
    fn live_quali_rows_come_from_laps_segmented_by_race_control() {
        let msgs: Vec<RaceControl> = serde_json::from_str(
            r#"[{"category":"SessionStatus","message":"SESSION STARTED","date":"2026-09-12T14:00:00+00:00","flag":null,"scope":null},
                {"category":"SessionStatus","message":"SESSION ABORTED","date":"2026-09-12T14:06:00+00:00","flag":null,"scope":null},
                {"category":"SessionStatus","message":"SESSION STARTED","date":"2026-09-12T14:12:00+00:00","flag":null,"scope":null},
                {"category":"SessionStatus","message":"SESSION FINISHED","date":"2026-09-12T14:18:00+00:00","flag":null,"scope":null},
                {"category":"Other","message":"CAR 63 (RUS) TIME 1:33.000 DELETED - TRACK LIMITS AT TURN 6 LAP 9 16:16:33","date":"2026-09-12T14:30:00+00:00","flag":null,"scope":null},
                {"category":"SessionStatus","message":"SESSION STARTED","date":"2026-09-12T14:25:00+00:00","flag":null,"scope":null}]"#,
        )
        .unwrap();
        assert_eq!(segment_starts(&msgs).len(), 2, "the restart at 14:12 is still Q1");
        let laps: Vec<Lap> = serde_json::from_str(
            r#"[{"driver_number":1,"lap_number":1,"lap_duration":95.5,"date_start":"2026-09-12T14:03:00+00:00"},
                {"driver_number":1,"lap_number":2,"lap_duration":94.812,"date_start":"2026-09-12T14:28:00+00:00"},
                {"driver_number":1,"lap_number":3,"lap_duration":null,"date_start":"2026-09-12T14:30:00+00:00"},
                {"driver_number":63,"lap_number":1,"lap_duration":95.9,"date_start":"2026-09-12T14:04:00+00:00"},
                {"driver_number":63,"lap_number":2,"lap_duration":95.1,"date_start":"2026-09-12T14:29:00+00:00"},
                {"driver_number":63,"lap_number":3,"lap_duration":93.0,"date_start":"2026-09-12T14:31:00+00:00"},
                {"driver_number":12,"lap_number":1,"lap_duration":96.2,"date_start":"2026-09-12T14:05:00+00:00"},
                {"driver_number":12,"lap_number":7,"lap_duration":469.8,"date_start":"2026-09-12T14:25:34+00:00","is_pit_out_lap":true},
                {"driver_number":12,"lap_number":8,"lap_duration":113.19,"date_start":"2026-09-12T14:27:45+00:00"},
                {"driver_number":12,"lap_number":9,"lap_duration":492.4,"date_start":"2026-09-12T14:35:53+00:00","is_pit_out_lap":true},
                {"driver_number":12,"lap_number":10,"lap_duration":110.0,"date_start":"2026-09-12T14:37:52+00:00"}]"#,
        )
        .unwrap();
        let positions: Vec<Position> = serde_json::from_str(
            r#"[{"driver_number":1,"position":1,"date":"2026-09-12T14:28:30+00:00"},
                {"driver_number":63,"position":2,"date":"2026-09-12T14:29:30+00:00"},
                {"driver_number":12,"position":3,"date":"2026-09-12T14:25:00+00:00"}]"#,
        )
        .unwrap();
        let (rows, stage, zones) = quali_rows_live(&positions, &laps, &msgs, &[], &drivers());
        assert_eq!(stage, 2);
        assert!(zones.is_none(), "three cars is no qualifying format");
        assert_eq!(rows[0].name, "NOR");
        assert_eq!(rows[0].value, "1:34.81", "the leader's best lap THIS segment");
        assert_eq!(rows[1].name, "RUS");
        assert_eq!(rows[1].value, "+0.29", "gap to that lap; the deleted 1:33.0 does not count");
        assert_eq!(
            rows[2].value, "",
            "out-lap, in-lap (the pit-out follows it) and a cool-down lap outside 107% are not times: blank"
        );

        assert_eq!(deleted_laps(&msgs).into_iter().next(), Some((63, 93_000)));
        assert_eq!(format_lap(95.312), "1:35.31");
        assert_eq!(format_lap(60.0), "1:00.00");
    }

    /// With a full field the zones follow the segment, and an eliminated
    /// driver's column holds the lap of the last segment they ran.
    #[test]
    fn live_quali_marks_the_eliminated_with_their_last_lap() {
        let mut positions = Vec::new();
        let mut laps = Vec::new();
        for n in 1..=20u32 {
            positions.push(Position { driver_number: n, position: n, date: "2026-09-12T14:30:00+00:00".into() });
            let lap = |k: u32, d: f64, at: &str| Lap {
                driver_number: n,
                lap_number: k,
                lap_duration: Some(d),
                date_start: Some(at.into()),
                is_pit_out_lap: false,
            };
            laps.push(lap(1, 90.0 + n as f64 * 0.1, "2026-09-12T14:05:00+00:00"));
            if n <= 15 {
                laps.push(lap(2, 89.0 + n as f64 * 0.1, "2026-09-12T14:30:00+00:00"));
            }
        }
        let msgs: Vec<RaceControl> = serde_json::from_str(
            r#"[{"category":"SessionStatus","message":"SESSION STARTED","date":"2026-09-12T14:00:00+00:00","flag":null,"scope":null},
                {"category":"SessionStatus","message":"SESSION FINISHED","date":"2026-09-12T14:18:00+00:00","flag":null,"scope":null},
                {"category":"SessionStatus","message":"SESSION STARTED","date":"2026-09-12T14:25:00+00:00","flag":null,"scope":null}]"#,
        )
        .unwrap();
        let (rows, stage, zones) = quali_rows_live(&positions, &laps, &msgs, &[], &[]);
        assert_eq!(stage, 2);
        assert_eq!(zones, Some(crate::model::QualiZones { safe: 10, field: 15 }));
        assert_eq!(rows[0].value, "1:29.10");
        assert_eq!(rows[14].value, "+1.40", "P15 is still in the field: a Q2 gap");
        assert_eq!(rows[15].value, "1:31.60", "P16 went out in Q1: that lap, for the renderer to grey");
    }

    /// Render a CAPTURED OpenF1 snapshot (drivers/position/laps/race_control/
    /// stints JSON as `design/f1-logs/live-openf1/<ts>/` or the one-shot
    /// `design/f1-logs/openf1-<name>-<ts>.json` files hold them) to a PNG at
    /// the board's full row depth:
    ///
    ///   OPENF1_FIXTURE_DIR=... OPENF1_FIXTURE_TS=20260912T143721Z \
    ///   F1_FIXTURE_OUT=/tmp/f1 cargo test -p server render_openf1_fixture -- --ignored
    ///
    /// With OPENF1_FIXTURE_TS set, files are read as `<dir>/openf1-<name>-<ts>.json`;
    /// without it, as `<dir>/<name>.json`.
    #[test]
    #[ignore]
    fn render_openf1_fixture() {
        let dir = std::env::var("OPENF1_FIXTURE_DIR").expect("OPENF1_FIXTURE_DIR");
        let ts = std::env::var("OPENF1_FIXTURE_TS").ok();
        let out = std::env::var("F1_FIXTURE_OUT").unwrap_or_else(|_| dir.clone());
        let load = |name: &str| -> String {
            let path = match &ts {
                Some(ts) => format!("{dir}/openf1-{name}-{ts}.json"),
                None => format!("{dir}/{name}.json"),
            };
            std::fs::read_to_string(&path).unwrap_or_else(|_| "[]".into())
        };
        let drivers: Vec<Driver> = serde_json::from_str(&load("drivers")).unwrap_or_default();
        let positions: Vec<Position> = serde_json::from_str(&load("position")).unwrap_or_default();
        let laps: Vec<Lap> = serde_json::from_str(&load("laps")).unwrap_or_default();
        let msgs: Vec<RaceControl> = serde_json::from_str(&load("race_control")).unwrap_or_default();
        let stints: Vec<Stint> = serde_json::from_str(&load("stints")).unwrap_or_default();
        let (rows, stage, zones) = quali_rows_live(&positions, &laps, &msgs, &stints, &drivers);
        let g = crate::model::Game {
            id: "openf1-quali".into(),
            sport_id: "f1".into(),
            away: crate::model::TeamSide::empty("F1"),
            home: crate::model::TeamSide::empty(""),
            status: crate::model::GameStatus::Active,
            ordinal: String::new(),
            clock: String::new(),
            start_time: String::new(),
            end_time: None,
            leaderboard: Some(crate::model::Leaderboard { matchplay: None, laps: None,
                name: "SPANISH GP Q".into(),
                flag: track_flag(&msgs).map(String::from),
                stage: Some(stage),
                quali: zones,
                rows,
            }),
            champ: None,
            series: None,
            session: Some("quali".into()),
            situation: None,
            pregame: None,
        };
        let (_, px, _) = crate::render::render_board_frame(
            &g, crate::render::LEADERBOARD_MAX_ROWS, None, None, scoreboard_protocol::NameStyle::Name,
        );
        std::fs::create_dir_all(&out).unwrap();
        let path = format!("{out}/openf1-quali-{}.png", ts.as_deref().unwrap_or("snapshot"));
        std::fs::write(&path, crate::render_png(&px)).unwrap();
        eprintln!("wrote {path} (stage {stage}, zones {zones:?})");
    }

    /// MAR-116: the leader is whoever's LATEST position record says 1, and
    /// their completed laps are one less than the lap record being run.
    #[test]
    fn lap_progress_from_the_leaders_laps() {
        let positions: Vec<Position> = serde_json::from_str(
            r#"[{"driver_number":16,"position":2,"date":"2026-09-13T13:01:00+00:00"},
                {"driver_number":12,"position":1,"date":"2026-09-13T13:01:00+00:00"},
                {"driver_number":16,"position":1,"date":"2026-09-13T13:20:00+00:00"},
                {"driver_number":12,"position":2,"date":"2026-09-13T13:20:00+00:00"}]"#,
        )
        .unwrap();
        assert_eq!(leader_number(&positions), Some(16), "LEC took the lead at 13:20");
        let laps: Vec<Lap> = serde_json::from_str(
            r#"[{"driver_number":16,"lap_number":21,"lap_duration":95.1,"date_start":"2026-09-13T13:38:00+00:00"},
                {"driver_number":16,"lap_number":22,"lap_duration":95.3,"date_start":"2026-09-13T13:39:35+00:00"},
                {"driver_number":16,"lap_number":23,"lap_duration":null,"date_start":"2026-09-13T13:41:10+00:00"}]"#,
        )
        .unwrap();
        assert_eq!(laps_completed(&laps), 22, "lap 23 is being run");
        assert_eq!(laps_completed(&[]), 0);
    }

    #[test]
    fn session_names_map_to_espn_id_suffixes() {
        assert_eq!(session_kind("Race"), Some("race"));
        assert_eq!(session_kind("Qualifying"), Some("quali"));
        assert_eq!(session_kind("Sprint"), Some("sprint"));
        assert_eq!(session_kind("Sprint Qualifying"), Some("sprint-quali"));
        assert_eq!(session_kind("Practice 1"), None, "practice never renders");
    }

    /// The exact sequence the 2026 Dutch GP produced live while this was
    /// being written: session start, red flag five minutes in (spelled as an
    /// "Other" message + "SESSION ABORTED", flag field None), then a TRACK
    /// CLEAR that must NOT lift the red, because the race is still suspended.
    #[test]
    fn a_red_flag_survives_a_track_clear() {
        let msgs: Vec<RaceControl> = serde_json::from_str(
            r#"[{"category":"SessionStatus","message":"SESSION STARTED","date":"2026-08-23T13:00:00","flag":null,"scope":null},
                {"category":"SessionStatus","message":"SESSION ABORTED","date":"2026-08-23T13:05:27","flag":null,"scope":null},
                {"category":"Other","message":"RED FLAG - RACE SUSPENDED","date":"2026-08-23T13:05:28","flag":null,"scope":null},
                {"category":"Flag","flag":"CLEAR","scope":"Track","message":"TRACK CLEAR","date":"2026-08-23T13:08:00"},
                {"category":"Flag","flag":"DOUBLE YELLOW","scope":"Sector","message":"DOUBLE YELLOW IN TRACK SECTOR 18","date":"2026-08-23T13:08:01"}]"#,
        )
        .unwrap();
        assert_eq!(track_flag(&msgs), Some("red"), "track-clear is marshals, not a restart");

        // The restart lifts it.
        let mut resumed = msgs.clone();
        resumed.push(serde_json::from_str(
            r#"{"category":"SessionStatus","message":"SESSION RESUMED","date":"2026-08-23T13:40:00","flag":null,"scope":null}"#,
        ).unwrap());
        assert_eq!(track_flag(&resumed), None, "a resume clears the red");
    }

    /// The 2026 Dutch GP restart, verbatim: the red ended with nothing but
    /// "SAFETY CAR LIGHTS ON" (category Other) — no session resume ever came.
    /// A safety car leading cars means running-under-yellow, not stopped;
    /// and the "RACE WILL RESUME AT .." announcement during the stoppage
    /// must not count as the resume.
    #[test]
    fn a_red_flag_ends_when_the_safety_car_leads_the_restart() {
        let msgs: Vec<RaceControl> = serde_json::from_str(
            r#"[{"category":"SessionStatus","message":"SESSION STARTED","date":"2026-08-23T13:00:00","flag":null,"scope":null},
                {"category":"Other","message":"RED FLAG - RACE SUSPENDED","date":"2026-08-23T13:05:28","flag":null,"scope":null},
                {"category":"Other","message":"RACE WILL RESUME AT 15:33","date":"2026-08-23T13:22:29","flag":null,"scope":null},
                {"category":"Other","message":"SAFETY CAR LIGHTS ON","date":"2026-08-23T13:28:11","flag":null,"scope":null}]"#,
        )
        .unwrap();
        assert_eq!(track_flag(&msgs), Some("yellow"), "SC restart: yellow, not red");
        // ...and the SC phase ended with NOTHING but a SESSION STARTED (the
        // rolling restart, verbatim from the stream at 13:33:00).
        let mut restarted = msgs.clone();
        restarted.push(serde_json::from_str(
            r#"{"category":"SessionStatus","message":"SESSION STARTED","date":"2026-08-23T13:33:00","flag":null,"scope":null}"#,
        ).unwrap());
        assert_eq!(track_flag(&restarted), None, "a session start ends the SC yellow");
        // The pre-race shape: SC lights on BEFORE the start must not paint
        // the start yellow (12:55 lights-on, 13:03 session start, verbatim).
        let prerace: Vec<RaceControl> = serde_json::from_str(
            r#"[{"category":"Other","message":"SAFETY CAR LIGHTS ON","date":"2026-08-23T12:55:02","flag":null,"scope":null},
                {"category":"SessionStatus","message":"SESSION STARTED","date":"2026-08-23T13:03:28","flag":null,"scope":null}]"#,
        )
        .unwrap();
        assert_eq!(track_flag(&prerace), None, "the formation-lap SC is not a race yellow");
        // Announcement alone changes nothing.
        assert_eq!(track_flag(&msgs[..3]), Some("red"), "a scheduled resume is still a stoppage");
        // Green flag ends the SC period.
        let mut green = msgs.clone();
        green.push(serde_json::from_str(
            r#"{"category":"Flag","flag":"GREEN","scope":"Track","message":"GREEN FLAG","date":"2026-08-23T13:31:00"}"#,
        ).unwrap());
        assert_eq!(track_flag(&green), None, "green flag: racing");
    }

    #[test]
    fn safety_car_and_track_yellows_read_as_yellow_sector_yellows_do_not() {
        let sc: Vec<RaceControl> = serde_json::from_str(
            r#"[{"category":"SessionStatus","message":"SESSION STARTED","date":"T1","flag":null,"scope":null},
                {"category":"SafetyCar","message":"SAFETY CAR DEPLOYED","date":"T2","flag":null,"scope":null}]"#,
        )
        .unwrap();
        assert_eq!(track_flag(&sc), Some("yellow"));
        let mut ended = sc.clone();
        ended.push(serde_json::from_str(
            r#"{"category":"SafetyCar","message":"SAFETY CAR IN THIS LAP","date":"T3","flag":null,"scope":null}"#,
        ).unwrap());
        assert_eq!(track_flag(&ended), None, "SC coming in ends the yellow");

        // The VSC, verbatim from the stream (14:44:35): category SafetyCar,
        // message just "VSC DEPLOYED" — caught by the category, not the text.
        let vsc: Vec<RaceControl> = serde_json::from_str(
            r#"[{"category":"SessionStatus","message":"SESSION STARTED","date":"T1","flag":null,"scope":null},
                {"category":"SafetyCar","message":"VSC DEPLOYED","date":"T2","flag":null,"scope":null}]"#,
        )
        .unwrap();
        assert_eq!(track_flag(&vsc), Some("yellow"), "a VSC is a yellow");
        // ...and its end, verbatim (14:48:41): "VSC ENDING", then TRACK CLEAR.
        let mut vsc_over = vsc.clone();
        vsc_over.push(serde_json::from_str(
            r#"{"category":"SafetyCar","message":"VSC ENDING","date":"T3","flag":null,"scope":null}"#,
        ).unwrap());
        assert_eq!(track_flag(&vsc_over), None, "VSC ENDING clears the yellow");

        let sector: Vec<RaceControl> = serde_json::from_str(
            r#"[{"category":"Flag","flag":"DOUBLE YELLOW","scope":"Sector","message":"DOUBLE YELLOW IN TRACK SECTOR 4","date":"T1"}]"#,
        )
        .unwrap();
        assert_eq!(track_flag(&sector), None, "a sector yellow is local waving, not session state");
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
