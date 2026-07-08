//! Rich game model used for rendering (carries team colors + status), kept
//! separate from the lighter `GameSummary` sent to the app over HTTP.

use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::RgbColor;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GameStatus {
    Pregame,
    Active,
    Intermission,
    Final,
}

#[derive(Clone, Debug)]
pub struct TeamSide {
    pub abbrev: String,
    pub name: String, // short display name, uppercased at render time
    pub score: u32,
    pub primary: Rgb888,   // row background
    pub secondary: Rgb888, // accent bar + text
}

impl TeamSide {
    /// Placeholder side for leaderboard sports (golf/F1) — `abbrev` is the series
    /// tag ("PGA"/"LPGA"/"F1") used to match the favorite; the rest is unused.
    pub fn empty(abbrev: &str) -> Self {
        Self {
            abbrev: abbrev.into(),
            name: String::new(),
            score: 0,
            primary: Rgb888::BLACK,
            secondary: Rgb888::WHITE,
        }
    }
}

/// A ranked entry in a leaderboard sport (golfer/driver name + score/gap).
#[derive(Clone, Debug)]
pub struct LeaderRow {
    pub name: String,
    pub value: String,
}

/// Golf tournament / F1 race: an event name + ranked individuals (top few).
#[derive(Clone, Debug)]
pub struct Leaderboard {
    pub name: String,
    pub rows: Vec<LeaderRow>,
}

/// Championship/postseason context for a game, classified from ESPN's season
/// type + competition notes. `tier` is one of the sport's championship-option
/// ids (e.g. "final", "conf", "playoffs" — never "none") so the board's
/// big-games preference can be matched against it; `label` is the human name
/// shown on game cards ("Stanley Cup Final", "Playoffs").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Champ {
    pub tier: String,
    pub label: String,
}

/// Live in-game situation, rendered with the ORIGINAL scoreboard's sport-specific
/// bottom layouts (rust-scoreboard football.rs / baseball.rs / hockey.rs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Situation {
    /// NFL/CFB: down & distance + yardline + which side has the ball.
    Football {
        /// e.g. "3rd + 4" — ESPN shortDownDistanceText with "&" → "+" like the
        /// original server did (the 4x6 font has no ampersand glyph).
        down_string: String,
        /// e.g. "SEA 25" — ESPN possessionText, drawn verbatim.
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

#[derive(Clone, Debug)]
pub struct Game {
    pub id: String,
    pub sport_id: String,
    pub home: TeamSide,
    pub away: TeamSide,
    pub status: GameStatus,
    pub ordinal: String, // "2nd", "HALF", "" ...
    pub clock: String,   // "12:34" when active
    pub start_time: String,
    /// Set for leaderboard sports (golf, F1): an event name + ranked rows. `None`
    /// for normal two-team games (then home/away are the real teams).
    pub leaderboard: Option<Leaderboard>,
    /// Set when this is a postseason/championship game — drives the board's
    /// big-games preference and the app's game-card caption.
    pub champ: Option<Champ>,
    /// Session kind for multi-session events (F1: "race" / "quali" / "sprint" /
    /// "sprint-quali") — drives the per-league session subscription. `None` for
    /// ordinary games.
    pub session: Option<String>,
    /// Sport-specific live details (football down/possession, baseball count/
    /// bases, hockey power play), shown only while the game is Active.
    pub situation: Option<Situation>,
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
pub fn ensure_contrast(primary: Rgb888, secondary: Rgb888) -> Rgb888 {
    if contrast(primary, secondary) >= 3.0 {
        secondary
    } else if contrast(primary, Rgb888::WHITE) >= contrast(primary, Rgb888::BLACK) {
        Rgb888::WHITE
    } else {
        Rgb888::BLACK
    }
}
