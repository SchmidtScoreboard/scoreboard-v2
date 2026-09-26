//! Rich game model used for rendering (carries team colors + status), kept
//! separate from the lighter `GameSummary` sent to the app over HTTP.

use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::RgbColor;
use serde::{Serialize, Serializer};

/// Serialize an `Rgb888` as the 6-digit hex string it was parsed from
/// (`"FFB81C"`) — for the raw-data endpoint's JSON (the type has no serde
/// support of its own, and hex is what every other color field speaks).
fn ser_rgb<S: Serializer>(c: &Rgb888, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format!("{:02X}{:02X}{:02X}", c.r(), c.g(), c.b()))
}

fn ser_rgb_opt<S: Serializer>(c: &Option<Rgb888>, s: S) -> Result<S::Ok, S::Error> {
    match c {
        Some(c) => ser_rgb(c, s),
        None => s.serialize_none(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub enum GameStatus {
    Pregame,
    Active,
    Intermission,
    Final,
}

#[derive(Clone, Debug, Serialize)]
pub struct TeamSide {
    pub abbrev: String,
    pub name: String, // short mascot/display name, uppercased at render time
    /// City / college / country (ESPN `competitor.team.location`) — the string
    /// the `Location` name style renders (uppercased + shortened at draw time).
    /// Empty for leaderboard "teams" (golf tours / F1) that have no location.
    pub location: String,
    pub score: u32,
    #[serde(serialize_with = "ser_rgb")]
    pub primary: Rgb888, // row background
    #[serde(serialize_with = "ser_rgb")]
    pub secondary: Rgb888, // accent bar + text
    /// The other numbers a box score keeps beside the score, for the sports
    /// that keep them. All four ride in payloads the server already downloads
    /// and were parsed by nobody until now.
    ///
    /// `Option` rather than 0 throughout, because absent and zero are different
    /// facts and the difference is the interesting one: a game yet to start has
    /// no hit count, and a no-hitter has a real one worth drawing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hits: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<u32>,
    /// Shots on goal (NHL), from `api-web.nhle.com`'s score feed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shots: Option<u32>,
    /// Penalty-shootout goals (soccer knockouts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shootout: Option<u32>,
    /// Aggregate score over both legs of a two-leg tie. On a second leg the
    /// scoreline alone is the wrong number to read — a 1-0 loss can still put a
    /// side through — and this is the one that decides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<u32>,
}

impl TeamSide {
    /// Placeholder side for leaderboard sports (golf/F1) — `abbrev` is the series
    /// tag ("PGA"/"LPGA"/"F1") used to match the favorite; the rest is unused.
    pub fn empty(abbrev: &str) -> Self {
        Self {
            abbrev: abbrev.into(),
            name: String::new(),
            location: String::new(),
            score: 0,
            primary: Rgb888::BLACK,
            secondary: Rgb888::WHITE,
            hits: None,
            errors: None,
            shots: None,
            shootout: None,
            aggregate: None,
        }
    }
}

/// A ranked entry in a leaderboard sport (golfer/driver name + score/gap).
#[derive(Clone, Debug, Default, Serialize)]
pub struct LeaderRow {
    /// Position label drawn centered in a fixed two-digit column ("1".."20");
    /// empty = no rank column (golf keeps names at the left edge).
    pub rank: String,
    pub name: String,
    pub value: String,
    /// Team color for the rank + name (F1 broadcast style); None = white.
    #[serde(serialize_with = "ser_rgb_opt", skip_serializing_if = "Option::is_none")]
    pub color: Option<embedded_graphics::pixelcolor::Rgb888>,
    /// Holes played this round, for a golfer still on the course. `None` for a
    /// finished round, which is the common case and the one that should cost no
    /// pixels — the column appears only while it is telling you something.
    pub thru: Option<String>,
    /// Tire compound indicator drawn as a colored dot after the name:
    /// 'S'oft, 'M'edium, 'H'ard, 'I'ntermediate, 'W'et. ESPN's feed carries
    /// no tire data, so live rows stay None (the demo shows the treatment).
    pub tire: Option<char>,
}

/// Golf tournament / F1 race: an event name + ranked individuals (top few).
#[derive(Clone, Debug, Serialize)]
pub struct Leaderboard {
    pub name: String,
    /// Track status for a motorsport session: "red" or "yellow" recolors the
    /// title-row flags; None (the overwhelming default, and everything golf)
    /// draws the checkered pair. Serialized for the /game debug view.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
    /// Qualifying segment in progress (1..=3): the renderer appends it to
    /// the session letter in the title ("SPANISH GP Q" -> "SPANISH GP Q2").
    /// None outside a live qualifying session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<u8>,
    /// Elimination zones of the running qualifying segment; None for races,
    /// golf, and finished sessions (a classification has no danger zone).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quali: Option<QualiZones>,
    /// Race progress for the lap bar under the title: laps the leader has
    /// completed out of the scheduled distance. None outside race and
    /// sprint sessions, before the start, and on a circuit whose distance
    /// `laps.rs` does not know — no bar is better than a wrong one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub laps: Option<LapProgress>,
    pub rows: Vec<LeaderRow>,
}

/// Laps the leader has completed out of the race's scheduled total.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct LapProgress {
    pub completed: u32,
    pub total: u32,
}

/// Where the axe falls in a qualifying segment, by RANK. Ranks past `safe`
/// are in the elimination zone of the segment being run; ranks past `field`
/// were already eliminated in an earlier segment and are only along for the
/// classification. Q3 has `safe == field == 10`: nobody left to drop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct QualiZones {
    pub safe: u32,
    pub field: u32,
}

impl QualiZones {
    /// The zones for segment `stage` (1..=3) of an `n`-car field. The rules
    /// drop half the surplus over ten in each of Q1 and Q2 (22 cars: 6 and 6,
    /// leaving 16 then 10; 20 cars: 5 and 5). Fewer than ten cars is not a
    /// qualifying format this understands.
    pub fn for_field(n: usize, stage: u8) -> Option<Self> {
        if n < 10 {
            return None;
        }
        let n = n as u32;
        let cut1 = n - (n - 10) / 2;
        Some(match stage {
            1 => QualiZones { safe: cut1, field: n },
            2 => QualiZones { safe: 10, field: cut1 },
            _ => QualiZones { safe: 10, field: 10 },
        })
    }
}

/// Championship/postseason context for a game, classified from ESPN's season
/// type + competition notes. `tier` is one of the sport's championship-option
/// ids (e.g. "final", "conf", "playoffs" — never "none") so the board's
/// big-games preference can be matched against it; `label` is the human name
/// shown on game cards ("Stanley Cup Final", "Playoffs").
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Champ {
    pub tier: String,
    pub label: String,
}

/// Live in-game situation, rendered with the ORIGINAL scoreboard's sport-specific
/// bottom layouts (rust-scoreboard football.rs / baseball.rs / hockey.rs).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Situation {
    /// NFL/CFB: down & distance + yardline + which side has the ball.
    Football {
        /// e.g. "3rd & 4" — ESPN's shortDownDistanceText, unmodified. The
        /// original server rewrote "&" to "+" because its 4x6 font had no
        /// ampersand; both of ours ship one.
        down_string: String,
        /// e.g. "SEA 25" — ESPN possessionText. No longer drawn verbatim: it is
        /// the ball's position on the field bar, resolved against whoever has
        /// possession. See `render::yards_from_own_goal`.
        ball_position: String,
        /// Some(true)=home ball, Some(false)=away, None=unknown (no indicator).
        home_possession: Option<bool>,
    },
    /// MLB: the count, outs, runners on base, and which half of the inning.
    Baseball {
        balls: u8,
        strikes: u8,
        outs: u8,
        on_first: bool,
        on_second: bool,
        on_third: bool,
        is_inning_top: bool,
    },
    /// NHL: power-play flags (badge shows the abbrev of the team on the PP) or
    /// the skater counts when both sides are short-handed (e.g. "4-4").
    Hockey {
        away_powerplay: bool,
        home_powerplay: bool,
        away_players: u8,
        home_players: u8,
    },
}

/// Pregame context shown on the board while a game is scheduled: each team's
/// overall record replaces its (meaningless) 0 score in the team row, and the
/// national broadcast joins the start time in the bottom strip.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Pregame {
    pub away_record: String, // "47-42" (soccer: "4-2-8"); "" if unknown
    pub home_record: String,
    pub broadcast: String, // "ESPN" — national feed only; "" if none
}

#[derive(Clone, Debug, Serialize)]
pub struct Game {
    pub id: String,
    pub sport_id: String,
    pub home: TeamSide,
    pub away: TeamSide,
    pub status: GameStatus,
    pub ordinal: String, // "2nd", "HALF", "" ...
    pub clock: String,   // "12:34" when active
    pub start_time: String,
    /// Feed-format end timestamp, when the source carries one. ESPN only
    /// provides `endDate` for multi-day leaderboard events (golf tournaments,
    /// F1 race weekends) — `None` for ordinary games.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_time: Option<String>,
    /// Set for leaderboard sports (golf, F1): an event name + ranked rows. `None`
    /// for normal two-team games (then home/away are the real teams).
    pub leaderboard: Option<Leaderboard>,
    /// Set when this is a postseason/championship game — drives the board's
    /// big-games preference and the app's game-card caption.
    pub champ: Option<Champ>,
    /// A playoff series' state, already shortened for the panel: "WCF 3-2",
    /// "SERIES 2-1". Formatted at parse time because the two sources disagree
    /// about almost everything — the NHL keys its win counts by SEED and ESPN
    /// keys them by home/away — and the renderer should not have to know.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    /// Session kind for multi-session events (F1: "race" / "quali" / "sprint" /
    /// "sprint-quali") — drives the per-league session subscription. `None` for
    /// ordinary games.
    pub session: Option<String>,
    /// Sport-specific live details (football down/possession, baseball count/
    /// bases, hockey power play), shown only while the game is Active.
    pub situation: Option<Situation>,
    /// Records + national TV, shown only while the game is scheduled.
    pub pregame: Option<Pregame>,
}

pub fn hex_to_rgb(hex: &str) -> Rgb888 {
    let h = hex.trim().trim_start_matches('#');
    let n = u32::from_str_radix(h, 16).unwrap_or(0);
    Rgb888::new((n >> 16) as u8, (n >> 8) as u8, n as u8)
}

fn rel_lum(c: Rgb888) -> f32 {
    let f = |v: u8| {
        let s = v as f32 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
}

fn contrast(a: Rgb888, b: Rgb888) -> f32 {
    let (l1, l2) = (rel_lum(a), rel_lum(b));
    let (hi, lo) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
    (hi + 0.05) / (lo + 0.05)
}

/// Like the original backend: if the team's secondary color doesn't contrast
/// enough with its primary, fall back to white or black (whichever reads).
/// The threshold matches the original Python backend's `color.py` (3.5) — the
/// v2 port had lowered it to 3.0, which let genuinely hard-to-read secondaries
/// (a mid-tone accent on a mid-tone primary) survive on the panel.
pub fn ensure_contrast(primary: Rgb888, secondary: Rgb888) -> Rgb888 {
    if contrast(primary, secondary) >= 3.5 {
        secondary
    } else if contrast(primary, Rgb888::WHITE) >= contrast(primary, Rgb888::BLACK) {
        Rgb888::WHITE
    } else {
        Rgb888::BLACK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 22 cars (2026): six out in Q1, six in Q2, ten in Q3. 20 cars: five
    /// and five. Q3 has nobody left to drop.
    #[test]
    fn quali_zones_follow_the_field_size() {
        assert_eq!(QualiZones::for_field(22, 1), Some(QualiZones { safe: 16, field: 22 }));
        assert_eq!(QualiZones::for_field(22, 2), Some(QualiZones { safe: 10, field: 16 }));
        assert_eq!(QualiZones::for_field(22, 3), Some(QualiZones { safe: 10, field: 10 }));
        assert_eq!(QualiZones::for_field(20, 1), Some(QualiZones { safe: 15, field: 20 }));
        assert_eq!(QualiZones::for_field(20, 2), Some(QualiZones { safe: 10, field: 15 }));
        assert_eq!(QualiZones::for_field(3, 1), None, "not a qualifying format");
    }
}
