//! Server-side rendering of a game into a 64x32 RGB565 frame, reproducing the
//! ORIGINAL scoreboard layout pixel-for-pixel: two team-colored rows (secondary
//! accent stripe + name left, score right) and a bottom status line.
//!
//! Matching the original (rust-scoreboard) exactly means:
//!  - box height = effective glyph height + 2*spacing  (5x8 glyph = 6px, 4x6 = 5px),
//!  - text drawn on its ALPHABETIC baseline at `y_offset + spacing + glyph_height`
//!    (so the glyph has equal `spacing` margin top and bottom = vertically centered),
//!  - per-character kerning overrides Mark used (5x8: I,T->4 Y->6 ; 4x6: N->5).

use crate::model::{ensure_contrast, Game, GameStatus, Situation, TeamSide};
use scoreboard_protocol::{NameStyle, SpoilerMode};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use embedded_graphics::{
    draw_target::DrawTarget,
    geometry::{OriginDimensions, Size},
    pixelcolor::Rgb888,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
};
use scoreboard_protocol::{
    frame_id, rgb888_to_565, FrameId, FRAME_HEIGHT, FRAME_WIDTH,
};

const WHITE: Rgb888 = Rgb888::new(255, 255, 255);
const YELLOW: Rgb888 = Rgb888::new(255, 255, 0);
const GREY: Rgb888 = Rgb888::new(160, 160, 160);
/// Public aliases for callers outside this module (diagnostic frames).
pub const YELLOW_PUB: Rgb888 = YELLOW;
pub const GREY_PUB: Rgb888 = GREY;
pub const WHITE_PUB: Rgb888 = WHITE;
const ACCENT_W: u32 = 2;
const SCORE_RIGHT: i32 = 61; // 64 - 3

struct Canvas {
    buf: Vec<Rgb888>,
    height: usize,
}
impl Canvas {
    fn new() -> Self {
        Self::with_height(FRAME_HEIGHT)
    }
    /// Taller-than-panel canvases back the scrolling leaderboard frames — the
    /// board shows a 32px window and glides it up and down the frame.
    fn with_height(height: usize) -> Self {
        Self {
            buf: vec![Rgb888::BLACK; FRAME_WIDTH * height],
            height,
        }
    }
    fn to_rgb565(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.buf.len() * 2);
        for p in &self.buf {
            v.extend_from_slice(&rgb888_to_565(p.r(), p.g(), p.b()).to_le_bytes());
        }
        v
    }
}
impl OriginDimensions for Canvas {
    fn size(&self) -> Size {
        Size::new(FRAME_WIDTH as u32, self.height as u32)
    }
}
impl DrawTarget for Canvas {
    type Color = Rgb888;
    type Error = core::convert::Infallible;
    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(p, c) in pixels {
            if p.x >= 0 && p.y >= 0 && (p.x as usize) < FRAME_WIDTH && (p.y as usize) < self.height
            {
                self.buf[p.y as usize * FRAME_WIDTH + p.x as usize] = c;
            }
        }
        Ok(())
    }
}

fn fill(c: &mut Canvas, x: i32, y: i32, w: u32, h: u32, color: Rgb888) {
    Rectangle::new(Point::new(x, y), Size::new(w, h))
        .into_styled(PrimitiveStyle::with_fill(color))
        .draw(c)
        .ok();
}

/// Per-character advance width — replicates Mark's original kerning overrides.
fn advance(ch: char, big: bool) -> i32 {
    if big {
        match ch {
            'I' | 'T' => 4,
            'Y' => 6,
            _ => 5,
        }
    } else {
        match ch {
            'N' => 5,
            _ => 4,
        }
    }
}

/// Total advance of `s` with optical kerning — sum of the kerned pair-advances plus
/// the final glyph's own cell (so right-alignment lands correctly).
fn text_width(s: &str, big: bool) -> i32 {
    let mut w = 0;
    let mut prev: Option<char> = None;
    for ch in s.chars() {
        if let Some(p) = prev {
            w += kerned_advance(p, ch, big);
        }
        prev = Some(ch);
    }
    if let Some(p) = prev {
        w += advance(p, big);
    }
    w
}

/// Measured cap height of the eg fonts (FONT_5X8 caps = 6px, FONT_4X6 = 5px).
fn cap_h(big: bool) -> i32 {
    if big {
        6
    } else {
        5
    }
}

/// Alphabetic baseline that vertically centers caps in the region `[top, top+h)`.
/// eg draws caps with their bottom *on* the baseline, hence the `- 1`.
fn centered_baseline(top: i32, h: i32, big: bool) -> i32 {
    let ch = cap_h(big);
    top + (h - ch) / 2 + ch - 1
}

/// Draw one BDF glyph with its baseline at `by` and origin (left edge) at `px`.
fn draw_glyph(c: &mut Canvas, g: &crate::bdf::Glyph, px: i32, by: i32, color: Rgb888) {
    for (row, &byte) in g.rows.iter().enumerate() {
        for col in 0..g.w {
            if (byte >> (7 - col)) & 1 == 1 {
                let x = px + g.xoff + col;
                let y = by - g.yoff - g.h + 1 + row as i32;
                // Clip to the CANVAS height — tall scrolling frames draw past
                // the panel's 32 rows.
                if x >= 0 && y >= 0 && (x as usize) < FRAME_WIDTH && (y as usize) < c.height {
                    c.buf[y as usize * FRAME_WIDTH + x as usize] = color;
                }
            }
        }
    }
}

/// Target whitespace (px) between adjacent glyph ink at their tightest facing row.
/// Optical kerning trims any *excess* beyond this; it never tightens past it and
/// never adds space, so the verified per-sport spacing changes only where a pair
/// (LT, AV, LY…) was leaving an obvious hole.
const TARGET_GAP: i32 = 1;

/// Ink extent (leftmost, rightmost columns relative to the glyph ORIGIN, so it
/// includes `xoff`) on the baseline-relative row `rrel` (= screen_y − baseline), or
/// None if that row is blank. Inverse of `draw_glyph`'s row mapping.
fn glyph_ink_row(g: &crate::bdf::Glyph, rrel: i32) -> Option<(i32, i32)> {
    let row = rrel + g.yoff + g.h - 1;
    if row < 0 || row >= g.h {
        return None;
    }
    let byte = g.rows[row as usize];
    let (mut lo, mut hi): (Option<i32>, Option<i32>) = (None, None);
    for col in 0..g.w {
        if (byte >> (7 - col)) & 1 == 1 {
            lo = lo.or(Some(col));
            hi = Some(col);
        }
    }
    Some((g.xoff + lo?, g.xoff + hi?))
}

/// How far `cur` can slide toward `prev` (placed at `base`) before their ink would
/// be closer than `TARGET_GAP`. 0 for spaces / missing glyphs. This IS the kerning
/// rule — read from the bitmaps, so it covers every pair and both font sizes with no
/// hand-built table. `cur`'s leftmost ink on a row is measured against `prev`'s
/// rightmost ink on that row AND its vertical neighbors, so DIAGONALLY adjacent
/// pixels keep their gap too (otherwise pairs like "FS" touch at a corner).
fn kern(prev: char, cur: char, big: bool, base: i32) -> i32 {
    let font = if big { crate::bdf::font_5x8() } else { crate::bdf::font_4x6() };
    let (Some(pg), Some(cg)) = (font.glyph(prev), font.glyph(cur)) else {
        return 0;
    };
    let mut min_gap: Option<i32> = None;
    for rrel in -8..=2 {
        let Some((cl, _)) = glyph_ink_row(cg, rrel) else {
            continue;
        };
        // prev's rightmost ink within one row of this one (catches diagonals)
        let pr = [rrel - 1, rrel, rrel + 1]
            .into_iter()
            .filter_map(|r| glyph_ink_row(pg, r).map(|(_, right)| right))
            .max();
        if let Some(pr) = pr {
            let gap = base + cl - pr - 1; // empty columns between the facing ink edges
            min_gap = Some(min_gap.map_or(gap, |m| m.min(gap)));
        }
    }
    match min_gap {
        Some(m) if m > TARGET_GAP => m - TARGET_GAP,
        _ => 0,
    }
}

/// Base advance of `prev`, less the optical kern toward `cur`.
fn kerned_advance(prev: char, cur: char, big: bool) -> i32 {
    advance(prev, big) - kern(prev, cur, big, advance(prev, big))
}

/// Draw text on its alphabetic baseline at `by`, left edge at `x`, with optical
/// kerning between adjacent glyphs. Uses the ORIGINAL BDF glyphs.
fn draw_text(c: &mut Canvas, s: &str, x: i32, by: i32, big: bool, color: Rgb888) {
    let font = if big { crate::bdf::font_5x8() } else { crate::bdf::font_4x6() };
    let mut cx = x;
    let mut prev: Option<char> = None;
    for ch in s.chars() {
        if let Some(p) = prev {
            cx += kerned_advance(p, ch, big);
        }
        if let Some(g) = font.glyph(ch) {
            draw_glyph(c, g, cx, by, color);
        }
        prev = Some(ch);
    }
}

/// Draw text right-aligned so its right edge sits at `right`.
fn draw_text_right(c: &mut Canvas, s: &str, right: i32, by: i32, big: bool, color: Rgb888) {
    draw_text(c, s, right - text_width(s, big), by, big, color);
}

/// Like `draw_text` but stops before `max_right`, so a long name doesn't run into
/// a right-aligned value (used by the leaderboard rows).
fn draw_text_clip(c: &mut Canvas, s: &str, x: i32, by: i32, max_right: i32, big: bool, color: Rgb888) {
    let font = if big { crate::bdf::font_5x8() } else { crate::bdf::font_4x6() };
    let mut cx = x;
    let mut prev: Option<char> = None;
    for ch in s.chars() {
        if let Some(p) = prev {
            cx += kerned_advance(p, ch, big);
        }
        if cx + advance(ch, big) > max_right {
            break;
        }
        if let Some(g) = font.glyph(ch) {
            draw_glyph(c, g, cx, by, color);
        }
        prev = Some(ch);
    }
}

// ---- situational pixel art, decoded from the ORIGINAL assets/*.png ----
// Each sprite is (rows, width); bit (width-1-dx) of a row = pixel at dx. Unset
// bits are transparent (the original PNGs' alpha), so sprites overlay cleanly.

/// assets/football.png - 7x5 solid ball, recolored to the possessing team.
const FOOTBALL: (&[u8], i32) = (&[0b0011100, 0b0111110, 0b1111111, 0b0111110, 0b0011100], 7);
/// assets/filled_base.png / empty_base.png - 7x7 diamond, runner on/off.
const BASE_FILLED: (&[u8], i32) = (
    &[0b0001000, 0b0011100, 0b0111110, 0b1111111, 0b0111110, 0b0011100, 0b0001000],
    7,
);
const BASE_EMPTY: (&[u8], i32) = (
    &[0b0001000, 0b0010100, 0b0100010, 0b1000001, 0b0100010, 0b0010100, 0b0001000],
    7,
);
/// assets/small_arrow.png - 5x3 (points down); the top-of-inning arrow is the
/// vertical flip, exactly like the original's `flip_vertical()`.
const ARROW_DOWN: (&[u8], i32) = (&[0b11111, 0b01110, 0b00100], 5);
const ARROW_UP: (&[u8], i32) = (&[0b00100, 0b01110, 0b11111], 5);
/// assets/filled_square.png / empty_square.png - 3x3 out markers.
const SQUARE_FILLED: (&[u8], i32) = (&[0b111, 0b111, 0b111], 3);
const SQUARE_EMPTY: (&[u8], i32) = (&[0b111, 0b101, 0b111], 3);

fn draw_sprite(c: &mut Canvas, sprite: (&[u8], i32), x0: i32, y0: i32, color: Rgb888) {
    let (rows, w) = sprite;
    for (dy, &row) in rows.iter().enumerate() {
        for dx in 0..w {
            if (row >> (w - 1 - dx)) & 1 == 1 {
                let (x, y) = (x0 + dx, y0 + dy as i32);
                if x >= 0 && y >= 0 && (x as usize) < FRAME_WIDTH && (y as usize) < c.height {
                    c.buf[y as usize * FRAME_WIDTH + x as usize] = color;
                }
            }
        }
    }
}

/// The original's `Font::get_text_dimensions` width: a plain sum of fixed
/// per-char advances, no optical kerning. Baseball's ordinal-position quirk
/// (shift left in extra innings) was computed with THIS measure, so it is here.
fn naive_width_4x6(s: &str) -> i32 {
    s.chars().map(|ch| advance(ch, false)).sum()
}

struct Layout {
    glyph_h: i32, // cap height of the row font: 5x8 -> 6, 4x6 -> 5
    spacing: i32,
    big: bool,        // 5x8 for the team rows?
    status_big: bool, // 5x8 for the status line?
}

fn layout_for(sport: &str) -> Layout {
    match sport {
        "nhl" => Layout { glyph_h: 6, spacing: 2, big: true, status_big: true },
        "nba" | "wnba" | "cbb" | "wcbb" => Layout { glyph_h: 5, spacing: 2, big: false, status_big: false },
        // mlb + nfl + cfb: 4x6 rows, 1px spacing
        _ => Layout { glyph_h: 5, spacing: 1, big: false, status_big: false },
    }
}

fn box_h(lay: &Layout) -> i32 {
    lay.glyph_h + 2 * lay.spacing
}

/// The on-board display string for a team under `style`: the raw
/// location/name/abbrev, shortened (location only — the mascot `name` is already
/// shortened at parse time) and uppercased.
fn team_display(team: &TeamSide, style: NameStyle) -> String {
    // shorten_location already ASCII-folds; fold the others too — the board
    // font is ASCII-only, so an accented mascot ("Peñarol") would blank out.
    match style {
        NameStyle::Location => crate::teams::shorten_location(&team.location).to_ascii_uppercase(),
        NameStyle::Name => crate::teams::ascii_fold(&team.name).to_ascii_uppercase(),
        NameStyle::Abbrev => crate::teams::ascii_fold(&team.abbrev).to_ascii_uppercase(),
    }
}

/// `accent_w` is normally ACCENT_W; football widens the possessing team's
/// stripe to 8 (the original's possession cue), which also shifts the name.
/// `right_text` is the score — or the team's record while the game is
/// scheduled, when a 0-0 line would say nothing. `style` picks which of the
/// team's three name strings is drawn (per-board / per-league resolved).
fn draw_team_box(c: &mut Canvas, sport_id: &str, team: &TeamSide, style: NameStyle, y_offset: i32, lay: &Layout, accent_w: u32, right_text: &str) {
    let bh = box_h(lay);
    fill(c, 0, y_offset, FRAME_WIDTH as u32, bh as u32, team.primary);
    // The original's rectangle x-endpoints were inclusive, so its width-8
    // possession stripe lit 9 columns — the ball sprite (x 1..=7) sits framed
    // with a 1px margin each side. The plain 2px stripe stays v2's look.
    let stripe = if accent_w > ACCENT_W { accent_w + 1 } else { accent_w };
    fill(c, 0, y_offset, stripe, bh as u32, team.secondary);
    let by = centered_baseline(y_offset, bh, lay.big);
    let right_x = SCORE_RIGHT - text_width(right_text, lay.big);
    let name_x = accent_w as i32 + 3;
    let max_right = right_x - 2;
    // Resolve the styled name, then GUARANTEE it's readable: if the chosen
    // style (after shortening) still overflows the row, fall back to the
    // abbreviation, which always fits — a clipped half-name is worse than a
    // recognizable abbrev. The overflow is still reported (deduped here, and
    // exhaustively by `server team-names`) so a long name can be given a
    // curated short form rather than silently dropping to its abbrev.
    // The style already accounts for BOTH teams (see `pair_style`), so no
    // per-team fallback happens here — that is what made one row show an
    // abbreviation next to the other's full name. `draw_text_clip` still
    // guards the rare case where even an abbreviation is too wide.
    let _ = sport_id;
    let display = team_display(team, style);
    // Clip the name against the right text — records are wider than scores.
    draw_text_clip(c, &display, name_x, by, max_right, lay.big, team.secondary);
    draw_text(c, right_text, right_x, by, lay.big, team.secondary);
}

/// Pixels available for a team's NAME on one row — the same arithmetic
/// `draw_team_box` uses, factored out so the fit decision and the drawing can
/// never disagree about how much room there is.
fn name_avail(lay: &Layout, accent_w: u32, right_text: &str) -> i32 {
    let right_x = SCORE_RIGHT - text_width(right_text, lay.big);
    let name_x = accent_w as i32 + 3;
    (right_x - 2) - name_x
}

/// The style BOTH teams will be drawn with.
///
/// Resolved once for the pair, because the fallback is a property of the
/// matchup rather than of one team: deciding per team produced lines like
/// "CHI @ CARDINALS", where one name happened to fit and the other didn't.
/// If EITHER team overflows under the requested style, both drop to their
/// abbreviations together, so the row always compares like with like.
fn pair_style(sport_id: &str, away: &TeamSide, home: &TeamSide, style: NameStyle, lay: &Layout, avail: i32) -> NameStyle {
    if style == NameStyle::Abbrev {
        return style;
    }
    for team in [away, home] {
        let styled = team_display(team, style);
        let w = text_width(&styled, lay.big);
        if w > avail {
            warn_overflow(sport_id, style, &styled, w, avail);
            // Only worth switching if the abbreviation actually helps.
            let abbrev = team_display(team, NameStyle::Abbrev);
            if text_width(&abbrev, lay.big) <= avail {
                return NameStyle::Abbrev;
            }
        }
    }
    style
}

/// Deduped overflow warning (per league / style / display string), so a team
/// that always overflows logs once, not every render tick.
fn warn_overflow(sport_id: &str, style: NameStyle, shown: &str, width: i32, avail: i32) {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let key = format!("{sport_id}|{style:?}|{shown}");
    let mut set = WARNED.get_or_init(|| Mutex::new(HashSet::new())).lock().unwrap();
    if set.insert(key) {
        tracing::warn!(
            "team-name overflow: league={} style={:?} name={:?} width={}px avail={}px (truncated) — run `server team-names` for the full report",
            sport_id, style, shown, width, avail
        );
    }
}

/// Team-row name metrics for `sport`'s layout: `(rendered width, available
/// width)`. The name overflows (and `draw_team_box` clips it) when
/// `width > avail`. `avail` reserves a minimum two-digit score column, matching
/// `draw_team_box`. Used by the `server team-names` overflow report so it and
/// the renderer measure identically.
pub fn team_name_metrics(sport: &str, display: &str) -> (i32, i32) {
    let lay = layout_for(sport);
    let width = text_width(display, lay.big);
    let name_x = ACCENT_W as i32 + 3;
    let right_x = SCORE_RIGHT - text_width("00", lay.big);
    let avail = (right_x - 2) - name_x;
    (width, avail)
}

/// Leaderboard-row name metrics (golf / F1), mirroring `render_leaderboard_rows`'
/// name clip: an optional two-digit rank column on the left, the value
/// right-aligned at x=63. `(rendered width, available width)`; overflow when
/// `width > avail`.
pub fn leaderboard_name_metrics(name: &str, value: &str, has_rank: bool) -> (i32, i32) {
    let width = text_width(name, false);
    let value_right = 63;
    let name_max_right = value_right - text_width(value, false) - 2;
    let name_x = if has_rank { text_width("20", false) + 3 } else { 1 };
    (width, name_max_right - name_x)
}

/// `tz` is the BOARD's timezone for start-time labels (None = server-local).
fn status_for(g: &Game, tz: Option<chrono_tz::Tz>) -> (String, Rgb888) {
    match g.status {
        GameStatus::Final => ("FINAL".to_string(), YELLOW),
        GameStatus::Active => (format!("{} {}", g.ordinal, g.clock).trim().to_string(), WHITE),
        GameStatus::Intermission => (
            if g.ordinal.is_empty() { "INT".to_string() } else { g.ordinal.clone() },
            WHITE,
        ),
        // Scheduled games show WHEN they start (like the original scoreboard);
        // "PREGAME" only if ESPN's timestamp doesn't parse.
        GameStatus::Pregame => (
            if crate::starts_on_a_day(&g.sport_id) {
                crate::start_day_label(&g.start_time).unwrap_or_else(|| "PREGAME".to_string())
            } else {
                crate::start_time_label(&g.start_time, tz).unwrap_or_else(|| "PREGAME".to_string())
            },
            GREY,
        ),
    }
}

/// Board-scoped render: spoiler protection hides scores, situations, and
/// leaderboard rows. `None` = the normal frame. Pregame frames are already
/// spoiler-free (no result exists yet), so they always render normally.
/// `tz` localizes start-time labels to the board's zone.
pub fn render_frame(g: &Game, spoiler: Option<SpoilerMode>, tz: Option<chrono_tz::Tz>, style: NameStyle) -> (FrameId, Vec<u8>) {
    match spoiler {
        Some(mode) if g.status != GameStatus::Pregame => render_spoiled(g, mode, style),
        _ => render_game(g, tz, style),
    }
}

/// The spoiler-protected frame: matchup stays (team rows / event title), the
/// result goes. Strict shows only when the game started (and FINAL when done);
/// Normal adds basic progress — period/quarter for team games, the lap count
/// for F1 — never scores, situations, or leaderboard rows.
fn render_spoiled(g: &Game, mode: SpoilerMode, style: NameStyle) -> (FrameId, Vec<u8>) {
    let (text, color) = match g.status {
        GameStatus::Final => ("FINAL".to_string(), YELLOW),
        _ => match mode {
            // "STARTED 6:00 PM" overflows the panel; LIVE NOW says enough.
            SpoilerMode::Strict => ("LIVE NOW".into(), GREY),
            SpoilerMode::Normal => {
                if let Some(lb) = &g.leaderboard {
                    // Lap count is sanctioned basic progress; anything richer
                    // (positions, gaps) is a spoiler.
                    let lap = lb.rows.first().filter(|r| r.value.starts_with("LAP"));
                    (lap.map(|r| r.value.clone()).unwrap_or_else(|| "LIVE".into()), WHITE)
                } else if g.ordinal.is_empty() {
                    ("LIVE".into(), WHITE)
                } else {
                    (g.ordinal.clone(), WHITE)
                }
            }
        },
    };

    if let Some(lb) = &g.leaderboard {
        // Event title (F1 keeps its chrome), no rows, centered status.
        let mut c = Canvas::new();
        let f1 = g.sport_id == "f1";
        let accent = if f1 { Rgb888::new(225, 6, 0) } else { Rgb888::new(52, 162, 35) };
        let title_color = if f1 { WHITE } else { accent };
        let title_w = text_width(&lb.name, false);
        let tx = 32 - title_w / 2;
        draw_text(&mut c, &lb.name, tx, 5, false, title_color);
        if f1 && tx >= 11 && tx + title_w <= 53 {
            checkered_flag(&mut c, 1);
            checkered_flag(&mut c, 55);
        }
        let by = centered_baseline(8, FRAME_HEIGHT as i32 - 8, true);
        draw_text(&mut c, &text, 32 - text_width(&text, true) / 2, by, true, color);
        let px = c.to_rgb565();
        return (frame_id(&px), px);
    }

    // Team game: rows keep colors and names — the matchup is not a spoiler —
    // but the score slots stay empty and no situation layout is drawn.
    let mut c = Canvas::new();
    let lay = layout_for(&g.sport_id);
    let style = pair_style(&g.sport_id, &g.away, &g.home, style, &lay, name_avail(&lay, ACCENT_W, ""));
    draw_team_box(&mut c, &g.sport_id, &g.away, style, 0, &lay, ACCENT_W, "");
    draw_team_box(&mut c, &g.sport_id, &g.home, style, box_h(&lay), &lay, ACCENT_W, "");
    let strip_top = 2 * box_h(&lay);
    let by = centered_baseline(strip_top, FRAME_HEIGHT as i32 - strip_top, lay.status_big);
    draw_text(&mut c, &text, (FRAME_WIDTH as i32 - text_width(&text, lay.status_big)) / 2, by, lay.status_big, color);
    let px = c.to_rgb565();
    (frame_id(&px), px)
}

pub fn render_game(g: &Game, tz: Option<chrono_tz::Tz>, style: NameStyle) -> (FrameId, Vec<u8>) {
    if g.leaderboard.is_some() {
        return render_leaderboard(g, tz);
    }
    let mut c = Canvas::new();
    let lay = layout_for(&g.sport_id);

    // Football possession part 1: the side with the ball gets an 8px accent
    // stripe (its name shifts right with it), the other keeps the normal 2px.
    let (away_accent, home_accent) = match &g.situation {
        Some(Situation::Football { home_possession: Some(home_ball), .. }) => {
            if *home_ball { (ACCENT_W, 8) } else { (8, ACCENT_W) }
        }
        _ => (ACCENT_W, ACCENT_W),
    };
    let (away_right, home_right) = match &g.pregame {
        Some(p) if g.status == GameStatus::Pregame => (
            if p.away_record.is_empty() { g.away.score.to_string() } else { p.away_record.clone() },
            if p.home_record.is_empty() { g.home.score.to_string() } else { p.home_record.clone() },
        ),
        _ => (g.away.score.to_string(), g.home.score.to_string()),
    };
    // Bound the fit by the WIDER right text and the WIDER accent stripe, so
    // the shared decision holds for whichever row is tighter.
    let widest_right = if text_width(&home_right, lay.big) > text_width(&away_right, lay.big) {
        &home_right
    } else {
        &away_right
    };
    let style = pair_style(
        &g.sport_id,
        &g.away,
        &g.home,
        style,
        &lay,
        name_avail(&lay, away_accent.max(home_accent), widest_right),
    );
    draw_team_box(&mut c, &g.sport_id, &g.away, style, 0, &lay, away_accent, &away_right);
    draw_team_box(&mut c, &g.sport_id, &g.home, style, box_h(&lay), &lay, home_accent, &home_right);

    // Live situational layouts reproduce the ORIGINAL per-sport screens; every
    // other state keeps the centered status line.
    match &g.situation {
        Some(sit @ Situation::Football { .. }) if g.status == GameStatus::Active => {
            draw_football_situation(&mut c, g, sit);
        }
        Some(sit @ Situation::Baseball { .. }) if g.status == GameStatus::Active => {
            draw_baseball_situation(&mut c, g, sit);
        }
        Some(sit @ Situation::Hockey { .. })
            if g.status == GameStatus::Active && hockey_badge(g, sit).is_some() =>
        {
            draw_hockey_situation(&mut c, g, &hockey_badge(g, sit).unwrap());
        }
        _ => {
            let (s, color) = status_for(g, tz);
            let strip_top = 2 * box_h(&lay);
            let strip_h = FRAME_HEIGHT as i32 - strip_top;
            let broadcast = g
                .pregame
                .as_ref()
                .filter(|_| g.status == GameStatus::Pregame)
                .map(|p| p.broadcast.as_str())
                .unwrap_or("");
            if broadcast.is_empty() {
                // Status line centered (both axes) in the strip below the team rows.
                let by = centered_baseline(strip_top, strip_h, lay.status_big);
                let sx = (FRAME_WIDTH as i32 - text_width(&s, lay.status_big)) / 2;
                draw_text(&mut c, &s, sx, by, lay.status_big, color);
            } else {
                // Scheduled with a national broadcast: start time over the
                // channel, the two-line block centered in the strip. The 1px
                // gap lets the pair fit even the NHL layout's 12px strip.
                let (h1, h2, gap) = (cap_h(lay.status_big), cap_h(false), 1);
                let top = strip_top + (strip_h - (h1 + gap + h2)) / 2;
                let by1 = centered_baseline(top, h1, lay.status_big);
                draw_text(&mut c, &s, (FRAME_WIDTH as i32 - text_width(&s, lay.status_big)) / 2, by1, lay.status_big, color);
                let by2 = centered_baseline(top + h1 + gap, h2, false);
                draw_text(&mut c, broadcast, (FRAME_WIDTH as i32 - text_width(broadcast, false)) / 2, by2, false, GREY);
            }
        }
    }

    let px = c.to_rgb565();
    (frame_id(&px), px)
}

/// Original football.rs bottom layout: two 4x6 rows — clock top-left, down &
/// distance top-right, period bottom-left, yardline bottom-right — plus the
/// team-colored football next to the possessing team's row.
fn draw_football_situation(c: &mut Canvas, g: &Game, sit: &Situation) {
    let Situation::Football { down_string, ball_position, home_possession } = sit else {
        return;
    };
    // Original rows: top at y=17, bottom at y=25. The original's draw_text put
    // cap bottoms at y-1; ours puts them ON the baseline, hence the -1 (ink
    // occupies rows 17..=21 and 25..=29, exactly as before).
    draw_text(c, &g.clock, 2, 21, false, WHITE);
    draw_text_right(c, down_string, 63, 21, false, WHITE);
    draw_text(c, &g.ordinal, 2, 29, false, WHITE);
    draw_text_right(c, ball_position, 63, 29, false, WHITE);
    // Possession part 2: the ball sprite inside the widened accent stripe,
    // recolored to the possessing team's PRIMARY color (original replace_color).
    if let Some(home_ball) = home_possession {
        let (y, color) = if *home_ball { (8, g.home.primary) } else { (1, g.away.primary) };
        draw_sprite(c, FOOTBALL, 1, y, color);
    }
}

/// Original baseball.rs bottom layout: 5x8 inning ordinal + half-inning arrow
/// on the left, the bases diamond in the middle, balls-strikes over the three
/// out squares on the right.
fn draw_baseball_situation(c: &mut Canvas, g: &Game, sit: &Situation) {
    let Situation::Baseball { balls, strikes, outs, on_first, on_second, on_third, is_inning_top } =
        sit
    else {
        return;
    };
    // The original measured the ordinal with the 4x6 width table but DREW it in
    // 5x8 — both quirks kept. Wide ordinals (10th+) shift left so the bases
    // diamond (left edge x=29) isn't overrun.
    let w4 = naive_width_4x6(&g.ordinal);
    let x_off = if w4 < 16 { 5 } else { 26 - (w4 + 7) };
    // Original baseline 26 with ink above it -> ours is 25 (ink rows 20..=25).
    draw_text(c, &g.ordinal, x_off, 25, true, WHITE);
    let arrow_x = w4 + x_off + 4;
    if *is_inning_top {
        draw_sprite(c, ARROW_UP, arrow_x, 20, WHITE);
    } else {
        draw_sprite(c, ARROW_DOWN, arrow_x, 23, WHITE);
    }

    let bs = format!("{balls}-{strikes}");
    // Fixed-advance width like the original measured — the out squares anchor
    // to this too, so the column can't wobble as the count changes.
    let bs_x = 61 - naive_width_4x6(&bs);
    // Ink rows 18..=22 — one clear row above the out squares, like the original.
    draw_text(c, &bs, bs_x, 22, false, WHITE);
    for i in 0..3i32 {
        let sq = if (*outs as i32) > i { SQUARE_FILLED } else { SQUARE_EMPTY };
        draw_sprite(c, sq, bs_x + i * 4, 24, WHITE);
    }

    draw_sprite(c, if *on_third { BASE_FILLED } else { BASE_EMPTY }, 29, 22, WHITE);
    draw_sprite(c, if *on_second { BASE_FILLED } else { BASE_EMPTY }, 34, 17, WHITE);
    draw_sprite(c, if *on_first { BASE_FILLED } else { BASE_EMPTY }, 39, 22, WHITE);
}

/// What the original hockey.rs put in the yellow badge: the abbrev of the team
/// on the power play, or the skater counts ("4-4") when both sides are short.
fn hockey_badge(g: &Game, sit: &Situation) -> Option<String> {
    let Situation::Hockey { away_powerplay, home_powerplay, away_players, home_players } = sit
    else {
        return None;
    };
    if *away_powerplay {
        Some(crate::espn::nhl_abbrev(&g.away.abbrev))
    } else if *home_powerplay {
        Some(crate::espn::nhl_abbrev(&g.home.abbrev))
    } else if (2..=4).contains(away_players) && (2..=4).contains(home_players) {
        Some(format!("{away_players}-{home_players}"))
    } else {
        None
    }
}

/// Original hockey.rs power-play layout: period ordinal left-aligned at x=5 and
/// a yellow badge with black text hugging the right edge (rows 21..=30).
fn draw_hockey_situation(c: &mut Canvas, g: &Game, msg: &str) {
    // Original baseline 29 with ink above it -> ours is 28 (ink rows 23..=28,
    // centered in the badge rows 21..=30 with a 2px margin each side).
    draw_text(c, &g.ordinal, 5, 28, true, WHITE);
    let w = text_width(msg, true);
    let right_point = FRAME_WIDTH as i32 - w - 4;
    fill(c, right_point, 21, (w + 3) as u32, 10, YELLOW);
    draw_text(c, msg, right_point + 2, 28, true, Rgb888::BLACK);
}

/// Golf / F1 leaderboard (ported from the original `src/golf.rs`): event name
/// centered up top (green for golf, red for F1), then up to 4 name/value rows.
fn render_leaderboard(g: &Game, tz: Option<chrono_tz::Tz>) -> (FrameId, Vec<u8>) {
    render_leaderboard_rows(g, 4, tz)
}

/// Default and maximum leaderboard depth (rows) on the board. The max is
/// pinned by the firmware's ws frame buffer (16512 bytes): a 20-row
/// leaderboard is 128px tall = 16384 payload bytes, plus the 64-byte frame id
/// and the 1-byte codec prefix = 16449 on the wire. `frame_wire_size_fits_the_firmware_buffer`
/// asserts the whole sum — the buffer used to fit the pre-codec format to the
/// byte, and adding the codec prefix put it one over.
pub const LEADERBOARD_DEFAULT_ROWS: usize = 10;
pub const LEADERBOARD_MAX_ROWS: usize = 20;

/// The frame the PHYSICAL BOARD shows for a game. Same as `render_game`
/// except deep leaderboard fields render TALL (up to `rows_cap` rows on the
/// same 6px pitch) for the board to scroll through; returns whether it
/// scrolls. A cap of 4 is the static panel layout — no scrolling. App cards
/// and HTTP renders keep the static 4-row `render_game`.
pub fn render_board_frame(g: &Game, rows_cap: usize, spoiler: Option<SpoilerMode>, tz: Option<chrono_tz::Tz>, style: NameStyle) -> (FrameId, Vec<u8>, bool) {
    // Spoiler-protected leaderboards hide their rows entirely — nothing tall
    // to scroll, and the panel-sized spoiled frame is the whole story.
    if spoiler.is_some() && g.status != GameStatus::Pregame {
        let (id, px) = render_frame(g, spoiler, tz, style);
        return (id, px, false);
    }
    let rows_cap = rows_cap.clamp(4, LEADERBOARD_MAX_ROWS);
    let deep = rows_cap > 4
        && g.status != GameStatus::Pregame
        && g.leaderboard.as_ref().is_some_and(|lb| lb.rows.len() > 4);
    if !deep {
        let (id, px) = render_game(g, tz, style);
        return (id, px, false);
    }
    let (id, px) = render_leaderboard_rows(g, rows_cap, tz);
    (id, px, true)
}

fn render_leaderboard_rows(g: &Game, max_rows: usize, tz: Option<chrono_tz::Tz>) -> (FrameId, Vec<u8>) {
    let lb = g.leaderboard.as_ref().unwrap();
    // Title baseline 5 + rows on a 6px pitch from baseline 12 (1px gap top and
    // bottom): 4 rows land exactly on the 32px panel; more rows grow the frame
    // by 6px each and the board scrolls the window.
    let rows = lb.rows.len().min(max_rows);
    let height = if rows > 4 { 12 + (rows as i32 - 1) * 6 + 2 } else { FRAME_HEIGHT as i32 };
    let mut c = Canvas::with_height(height as usize);
    let f1 = g.sport_id == "f1";
    let accent = if f1 {
        Rgb888::new(225, 6, 0) // F1 red
    } else {
        Rgb888::new(52, 162, 35) // golf green
    };
    // F1 wears race-day chrome: white event title between checkered flags.
    let title_color = if f1 { WHITE } else { accent };
    let title_w = text_width(&lb.name, false);
    let tx = 32 - title_w / 2;
    draw_text(&mut c, &lb.name, tx, 5, false, title_color);
    if f1 {
        // Flags only when the title clears them by 2px on both sides — a long
        // title ("BRITISH GP Q") otherwise collides and reads as garbage.
        let (flag_w, gap) = (8, 2);
        if tx >= 1 + flag_w + gap && tx + title_w <= 55 - gap {
            checkered_flag(&mut c, 1);
            checkered_flag(&mut c, 55);
        }
    }

    if matches!(g.status, GameStatus::Pregame) {
        let (s, _) = status_for(g, tz);
        let by = centered_baseline(8, FRAME_HEIGHT as i32 - 8, true);
        draw_text(&mut c, &s, 32 - text_width(&s, true) / 2, by, true, WHITE);
    } else {
        // Match the original golf.rs density: 4x6 rows on a 6px pitch, so the title
        // + 4 rows = 5 rows fit the 32px panel (the original fit 5, not 4). The whole
        // block sits one pixel down from the top so the gap is even (1px) top + bottom.
        let mut by = 12;
        for row in lb.rows.iter().take(max_rows.max(5)) {
            // Leaderboard rows draw the team/driver color as ink on the BLACK
            // panel — a dark team color (deep blue, maroon) would be unreadable,
            // so force it to white when it doesn't clear black by enough. The
            // team-row path already runs colors through ensure_contrast; this
            // brings the F1/golf tower to parity.
            let ink = row.color.map(|c| ensure_contrast(Rgb888::BLACK, c)).unwrap_or(WHITE);
            // Tire compound: a 1px strip on the far right edge, full row pitch
            // so consecutive rows read as one continuous color column.
            let value_right = if let Some(t) = row.tire {
                fill(&mut c, 63, by - 4, 1, 6, tire_color(t));
                61
            } else {
                63
            };
            let name_max_right = value_right - text_width(&row.value, false) - 2;
            let name_x = if row.rank.is_empty() {
                1
            } else {
                // Broadcast timing tower: the rank sits centered in a fixed
                // two-digit column so every code starts at the same x and the
                // timings line up.
                let col_w = text_width("20", false);
                let rx = (col_w - text_width(&row.rank, false)) / 2;
                draw_text(&mut c, &row.rank, rx, by, false, ink);
                col_w + 3
            };
            // A golfer last-name / driver code that runs into the value gets
            // clipped — surface it (deduped) like the team-row overflow.
            if name_x + text_width(&row.name, false) > name_max_right {
                warn_overflow(&g.sport_id, NameStyle::Name, &row.name, text_width(&row.name, false), name_max_right - name_x);
            }
            draw_text_clip(&mut c, &row.name, name_x, by, name_max_right, false, ink);
            // The timing wears the team color too (contrast-guarded like the
            // name); golf (no row color) keeps its green accent values.
            let value_ink = row.color.map(|c| ensure_contrast(Rgb888::BLACK, c)).unwrap_or(accent);
            draw_text_right(&mut c, &row.value, value_right, by, false, value_ink);
            by += 6;
        }
    }

    let px = c.to_rgb565();
    (frame_id(&px), px)
}

/// Pirelli compound colors for the timing-tower tire chip.
fn tire_color(compound: char) -> Rgb888 {
    match compound.to_ascii_uppercase() {
        'S' => Rgb888::new(0xDA, 0x29, 0x1C), // soft: red
        'M' => Rgb888::new(0xFF, 0xD1, 0x00), // medium: yellow
        'H' => Rgb888::new(0xF0, 0xF0, 0xF0), // hard: white
        'I' => Rgb888::new(0x39, 0xB5, 0x4A), // intermediate: green
        _ => Rgb888::new(0x00, 0x67, 0xAD),   // wet: blue
    }
}

/// An 8x6 checkered flag (2x2 checks) for the F1 title row corners. Starts at
/// row 1, preserving the layout's 1px top gap; the bottom row dips just below
/// the title's baseline into empty space.
fn checkered_flag(c: &mut Canvas, x0: i32) {
    for cy in 0..3i32 {
        for cx in 0..4i32 {
            if (cx + cy) % 2 == 0 {
                fill(c, x0 + cx * 2, 1 + cy * 2, 2, 2, WHITE);
            }
        }
    }
}

/// Render a centered multi-line notice (e.g. "SLEEPING / FOR 24 HOURS").
/// Each line is (text, big_font, color); lines are stacked with a 2px gap and the
/// whole block is centered vertically. Lines wider than the panel are clipped by
/// the canvas — keep big lines ≤ ~10 chars and small ones ≤ ~12.
///
/// `remaining` (0.0..=1.0) draws a countdown bar along the bottom — the
/// hold-to-reset styling (outlined box, fill GROWING toward the event: full
/// bar = it happens now); `None` omits it (e.g. the clock).
pub fn render_notice(lines: &[(&str, bool, Rgb888)], remaining: Option<f32>) -> (FrameId, Vec<u8>) {
    let mut c = Canvas::new();
    let line_h = |big: bool| if big { 6 } else { 5 };
    let gap = 2;
    // Reserve the bottom rows for the bar so the text block centers above it.
    let content_h = if remaining.is_some() { FRAME_HEIGHT as i32 - 7 } else { FRAME_HEIGHT as i32 };
    let total: i32 = lines.iter().map(|(_, b, _)| line_h(*b)).sum::<i32>()
        + gap * (lines.len().saturating_sub(1)) as i32;
    let mut y = (content_h - total) / 2;
    for (text, big, color) in lines {
        let h = line_h(*big);
        let by = centered_baseline(y, h, *big);
        let x = (FRAME_WIDTH as i32 - text_width(text, *big)) / 2;
        draw_text(&mut c, text, x, by, *big, *color);
        y += h + gap;
    }
    if let Some(rem) = remaining {
        let bar_x = 4i32;
        let bar_h = 5u32;
        let bar_y = FRAME_HEIGHT as i32 - bar_h as i32 - 1;
        let bar_w = FRAME_WIDTH as u32 - 8;
        let outline = Rgb888::new(0x55, 0x55, 0x55);
        fill(&mut c, bar_x, bar_y, bar_w, 1, outline);
        fill(&mut c, bar_x, bar_y + bar_h as i32 - 1, bar_w, 1, outline);
        fill(&mut c, bar_x, bar_y, 1, bar_h, outline);
        fill(&mut c, bar_x + bar_w as i32 - 1, bar_y, 1, bar_h, outline);
        let done = 1.0 - rem.clamp(0.0, 1.0);
        let w = (done * (bar_w - 2) as f32).round() as u32;
        if w > 0 {
            fill(&mut c, bar_x + 1, bar_y + 1, w, bar_h - 2, Rgb888::new(0xFF, 0xAA, 0x00));
        }
    }
    let px = c.to_rgb565();
    (frame_id(&px), px)
}

/// The idle clock: big 12-hour time centered, small AM/PM below. Rendered
/// server-side (the board is a dumb display), re-pushed each minute while shown.
/// Split a 24-hour clock into what the panel should show.
fn clock_parts(hour24: u32, minute: u32, h24: bool) -> (String, Option<&'static str>) {
    if h24 {
        return (format!("{:02}:{:02}", hour24 % 24, minute), None);
    }
    let (h12, ampm) = match hour24 % 24 {
        0 => (12, "AM"),
        h if h < 12 => (h, "AM"),
        12 => (12, "PM"),
        h => (h - 12, "PM"),
    };
    (format!("{}:{:02}", h12, minute), Some(ampm))
}

/// The idle clock.
///
/// `style` is one of `classic`, `large`, `compact`; anything else falls back to
/// `classic` rather than to a blank panel, for the same reason an unknown off
/// mode does.
pub fn render_clock_styled(hour24: u32, minute: u32, style: &str, h24: bool) -> (FrameId, Vec<u8>) {
    let (time, ampm) = clock_parts(hour24, minute, h24);
    match style {
        // Two lines of large type filling the panel, the meridiem riding the
        // bottom edge of the digits rather than floating at their middle —
        // there it reads as part of the number.
        "large" => {
            let mut c = Canvas::new();
            let gap = 3;
            let ampm_w = ampm.map(|a| gap + text_width(a, false)).unwrap_or(0);
            let width = wx_w(&time, true, 2) + ampm_w;
            let x0 = (FRAME_WIDTH as i32 - width) / 2;
            let span = wx_text(&mut c, &time, x0, 15, true, 2, WHITE);
            if let Some(a) = ampm {
                wx_text(&mut c, a, x0 + wx_w(&time, true, 2) + gap, span.1 - 2, false, 1, GREY);
            }
            let px = c.to_rgb565();
            (frame_id(&px), px)
        }
        // The smallest legible clock: least light in a dark room, which is the
        // whole reason to pick it.
        "compact" => {
            let mut c = Canvas::new();
            let gap = 2;
            let ampm_w = ampm.map(|a| gap + text_width(a, false)).unwrap_or(0);
            let width = text_width(&time, false) + ampm_w;
            let x0 = (FRAME_WIDTH as i32 - width) / 2;
            wx_text(&mut c, &time, x0, 16, false, 1, WHITE);
            if let Some(a) = ampm {
                wx_text(&mut c, a, x0 + text_width(&time, false) + gap, 16, false, 1, GREY);
            }
            let px = c.to_rgb565();
            (frame_id(&px), px)
        }
        _ => match ampm {
            Some(a) => render_notice(&[(&time, true, WHITE), (a, false, GREY)], None),
            None => render_notice(&[(&time, true, WHITE)], None),
        },
    }
}

/// The styles this binary draws, in menu order — the descriptor is built from
/// this, so a style can never be offered that `render_clock_styled` cannot draw.
pub const CLOCK_STYLES: [(&str, &str); 3] =
    [("classic", "Classic"), ("large", "Large"), ("compact", "Compact")];

#[cfg(test)]
mod kern_tests {
    use super::*;

    /// Both rows must use the SAME name style. Deciding per team produced
    /// lines like "CHI @ CARDINALS" — one name happened to fit, the other
    /// didn't, and the row compared an abbreviation against a mascot.
    #[test]
    fn a_matchup_never_mixes_abbreviation_with_full_name() {
        let lay = layout_for("nfl");
        let avail = name_avail(&lay, ACCENT_W, "24");
        let side = |name: &str, abbrev: &str| TeamSide {
            abbrev: abbrev.into(),
            name: name.into(),
            location: name.into(),
            score: 0,
            primary: Rgb888::new(0, 0, 0),
            secondary: Rgb888::WHITE,
        };
        // One short name that fits, one long one that cannot.
        let short = side("BEARS", "CHI");
        let long = side("CARDINALSCARDINALS", "ARI");
        assert!(text_width("BEARS", lay.big) <= avail, "precondition: the short name fits");
        assert!(
            text_width("CARDINALSCARDINALS", lay.big) > avail,
            "precondition: the long name does not"
        );

        // Whichever side is long, the PAIR drops to abbreviations together.
        for (a, h) in [(&short, &long), (&long, &short)] {
            let chosen = pair_style("nfl", a, h, NameStyle::Name, &lay, avail);
            assert_eq!(
                chosen,
                NameStyle::Abbrev,
                "one team overflowing must move BOTH to abbreviations"
            );
        }

        // And when both fit, neither is needlessly abbreviated.
        assert_eq!(
            pair_style("nfl", &short, &short, NameStyle::Name, &lay, avail),
            NameStyle::Name
        );
    }

    /// Accented names must RENDER, not leave a hole. The BDF loader used to
    /// discard every glyph above ASCII while parsing fonts that ship ~900 and
    /// ~1400 of them, so a golfer named Høgard came out as "H GARD" — the
    /// renderer drew a blank where the lookup failed. Golf and F1 fields are
    /// full of these.
    #[test]
    fn accented_names_render_instead_of_leaving_a_gap() {
        for big in [false, true] {
            let font = if big { crate::bdf::font_5x8() } else { crate::bdf::font_4x6() };
            for ch in "ØÉÑÜÅÖÇÍÁÈ".chars() {
                assert!(
                    font.glyph(ch).is_some(),
                    "{ch:?} has no glyph in the {} font",
                    if big { "5x8" } else { "4x6" }
                );
            }
            // And the drawn result differs from the same string with the
            // accented letter dropped — i.e. real ink, not a silent skip.
            let draw = |t: &str| {
                let mut c = Canvas::new();
                draw_text(&mut c, t, 0, 6, big, Rgb888::WHITE);
                c.to_rgb565()
            };
            assert_ne!(draw("HØGARD"), draw("HGARD"), "the accented glyph drew nothing (big={big})");
        }
    }

    /// The team-game status line (period/clock) must be horizontally centered in the
    /// bottom strip — it used to be left-aligned at NAME_X.
    #[test]
    fn team_status_line_is_centered() {
        use crate::model::{Game, GameStatus, TeamSide};
        let side = |name: &str| TeamSide {
            abbrev: "X".into(),
            name: name.into(),
            location: String::new(),
            score: 0,
            primary: crate::model::hex_to_rgb("003087"),
            secondary: crate::model::hex_to_rgb("ffffff"),
        };
        let g = Game {
            id: "t".into(),
            sport_id: "mlb".into(),
            away: side("YANKEES"),
            home: side("RED SOX"),
            status: GameStatus::Active,
            ordinal: "3rd".into(),
            clock: String::new(),
            start_time: String::new(), end_time: None,
            leaderboard: None,
            session: None,
            situation: None,
            pregame: None,
            champ: None,
        };
        let (_, px) = render_game(&g, None, NameStyle::Name);
        // Scan the bottom strip (clear of the two team boxes) for the status text.
        let (mut left, mut right, mut any) = (FRAME_WIDTH, 0usize, false);
        for y in 24..FRAME_HEIGHT {
            for x in 0..FRAME_WIDTH {
                let i = (y * FRAME_WIDTH + x) * 2;
                if px[i] != 0 || px[i + 1] != 0 {
                    any = true;
                    left = left.min(x);
                    right = right.max(x);
                }
            }
        }
        assert!(any, "status strip should have lit pixels");
        let center = (left + right) as f32 / 2.0;
        assert!((center - 31.5).abs() <= 2.0, "status not centered: center={center} (left={left}, right={right})");
    }

    /// The leaderboard block must sit with an even 1px gap above the title and below
    /// the last row (it used to be 2px top / 0px bottom).
    #[test]
    fn leaderboard_one_pixel_gap_top_and_bottom() {
        use crate::model::{Game, GameStatus, Leaderboard, LeaderRow, TeamSide};
        let g = Game {
            id: "t".into(),
            sport_id: "f1".into(),
            home: TeamSide::empty(""),
            away: TeamSide::empty("F1"),
            status: GameStatus::Active,
            ordinal: String::new(),
            clock: String::new(),
            start_time: String::new(), end_time: None,
            leaderboard: Some(Leaderboard {
                name: "MONACO GP".into(),
                rows: vec![
                    LeaderRow { name: "VERSTAPPEN".into(), value: "L42".into(), ..Default::default() },
                    LeaderRow { name: "NORRIS".into(), value: "+2.1".into(), ..Default::default() },
                    LeaderRow { name: "LECLERC".into(), value: "+5.8".into(), ..Default::default() },
                    LeaderRow { name: "PIASTRI".into(), value: "+3.0".into(), ..Default::default() },
                ],
            }),
            champ: None,
            session: Some("race".into()),
            situation: None,
            pregame: None,
        };
        let (_, px) = render_leaderboard(&g, None); // 64x32 RGB565-LE, 2 bytes/pixel
        let row_lit = |y: usize| {
            (0..FRAME_WIDTH).any(|x| {
                let i = (y * FRAME_WIDTH + x) * 2;
                px[i] != 0 || px[i + 1] != 0
            })
        };
        let top = (0..FRAME_HEIGHT).find(|&y| row_lit(y)).unwrap();
        let bottom = (0..FRAME_HEIGHT).rev().find(|&y| row_lit(y)).unwrap();
        assert_eq!(top, 1, "top gap should be 1px (first lit row = 1)");
        assert_eq!(FRAME_HEIGHT - 1 - bottom, 1, "bottom gap should be 1px (last lit row was {bottom})");
    }

    #[test]
    fn optical_kern_tightens_open_pairs_only() {
        let big = true;
        // "LT" leaves an obvious hole with fixed advance -> T must be pulled in.
        assert!(kern('L', 'T', big, advance('L', big)) > 0, "LT should kern");
        assert!(text_width("LT", big) < advance('L', big) + advance('T', big));
        // Kerning never ADDS space: a string is never wider than its naive cell sum.
        let naive: i32 = "GRAVY".chars().map(|c| advance(c, big)).sum();
        assert!(text_width("GRAVY", big) <= naive);
    }

    // ---- situational layouts (ported from the original football/baseball/hockey.rs) ----

    fn px565(px: &[u8], x: i32, y: i32) -> u16 {
        let i = (y as usize * FRAME_WIDTH + x as usize) * 2;
        u16::from_le_bytes([px[i], px[i + 1]])
    }
    fn c565(c: Rgb888) -> u16 {
        rgb888_to_565(c.r(), c.g(), c.b())
    }

    fn sit_game(sport: &str, ordinal: &str, clock: &str, sit: Situation) -> Game {
        use crate::model::hex_to_rgb;
        let side = |abbrev: &str, name: &str, p: &str, s: &str| TeamSide {
            abbrev: abbrev.into(),
            name: name.into(),
            location: String::new(),
            score: 3,
            primary: hex_to_rgb(p),
            secondary: hex_to_rgb(s),
        };
        Game {
            id: "t".into(),
            sport_id: sport.into(),
            away: side("SF", "49ERS", "AA0000", "FFFFFF"),
            home: side("SEA", "SEAHAWKS", "002244", "69BE28"),
            status: GameStatus::Active,
            ordinal: ordinal.into(),
            clock: clock.into(),
            start_time: String::new(), end_time: None,
            leaderboard: None,
            champ: None,
            session: None,
            situation: Some(sit),
            pregame: None,
        }
    }

    /// Football: possession widens the accent stripe to 8 and puts the ball
    /// sprite (possessing team's primary) at the original (1, 1)/(1, 8); the
    /// four text corners occupy the original rows (baselines 22 and 30).
    #[test]
    fn football_situation_matches_original_layout() {
        let g = sit_game("nfl", "3rd", "8:12", Situation::Football {
            down_string: "3rd + 4".into(),
            ball_position: "SEA 25".into(),
            home_possession: Some(false), // away has the ball
        });
        let (_, px) = render_game(&g, None, NameStyle::Name);

        let away_primary = c565(g.away.primary);
        let away_secondary = c565(g.away.secondary);
        let home_primary = c565(g.home.primary);
        // Away accent stripe lights 9 columns like the original (x=8 secondary),
        // framing the 7px ball sprite with a 1px margin on each side.
        assert_eq!(px565(&px, 8, 5), away_secondary, "away accent should widen to 9 columns");
        assert_eq!(px565(&px, 9, 5), c565(g.away.primary), "stripe ends at x=8");
        // …home stripe stays 2 (x=7 is the primary row fill).
        assert_eq!(px565(&px, 7, 10), home_primary, "home accent should stay 2");
        // Football sprite: full middle row (y=1+2) in the AWAY PRIMARY color,
        // sitting on the secondary stripe so it reads as the ball.
        for x in 1..=7 {
            assert_eq!(px565(&px, x, 3), away_primary, "football row at x={x}");
        }
        // The four text corners: white ink in the original rows.
        let white = c565(WHITE);
        let lit_white_in = |x0: i32, x1: i32, y0: i32, y1: i32| {
            (y0..=y1).any(|y| (x0..=x1).any(|x| px565(&px, x, y) == white))
        };
        assert!(lit_white_in(2, 30, 17, 21), "clock top-left");
        assert!(lit_white_in(33, 63, 17, 21), "down & distance top-right");
        assert!(lit_white_in(2, 30, 25, 29), "ordinal bottom-left");
        assert!(lit_white_in(33, 63, 25, 29), "yardline bottom-right");
    }

    /// Baseball: bases diamond at the original (29,22)/(34,17)/(39,22), the
    /// balls-strikes count right-aligned to 61 with the out squares under it,
    /// and the half-inning arrow after the (4x6-measured) ordinal.
    #[test]
    fn baseball_situation_matches_original_layout() {
        let g = sit_game("mlb", "3rd", "", Situation::Baseball {
            balls: 3, strikes: 2, outs: 2,
            on_first: false, on_second: true, on_third: false,
            is_inning_top: true,
        });
        let (_, px) = render_game(&g, None, NameStyle::Name);
        let white = c565(WHITE);

        // Second base (runner on): filled diamond centered at (34+3, 17+3).
        assert_eq!(px565(&px, 37, 20), white, "second base filled center");
        // First/third empty: outline tip lit, center dark.
        assert_eq!(px565(&px, 42, 22), white, "first base top tip");
        assert_eq!(px565(&px, 42, 25), 0, "first base hollow center");
        assert_eq!(px565(&px, 32, 22), white, "third base top tip");
        assert_eq!(px565(&px, 32, 25), 0, "third base hollow center");

        // Top-of-inning arrow: ordinal "3rd" measures 12 in fixed 4x6 -> x_off 5,
        // arrow at x=21, up-arrow apex at (23, 20), full row at y=22.
        assert_eq!(px565(&px, 23, 20), white, "arrow apex");
        assert_eq!(px565(&px, 21, 22), white, "arrow base left");
        assert_eq!(px565(&px, 25, 22), white, "arrow base right");

        // Balls-strikes right-aligned to 61; outs squares directly below on a
        // 4px pitch: first two filled (outs=2), third an empty ring.
        let bs_x = 61 - text_width("3-2", false);
        assert!((18..=22).any(|y| (bs_x..=61).any(|x| px565(&px, x, y) == white)), "count text");
        // One clear row between the count's ink and the out squares (original gap).
        assert!((bs_x..=61).all(|x| px565(&px, x, 23) == 0), "gap row above outs");
        assert_eq!(px565(&px, bs_x, 24), white, "out 1 filled");
        assert_eq!(px565(&px, bs_x + 4 + 1, 25), white, "out 2 filled center");
        assert_eq!(px565(&px, bs_x + 8 + 1, 25), 0, "out 3 ring center empty");
        assert_eq!(px565(&px, bs_x + 8, 24), white, "out 3 ring edge");

        // Ordinal drawn in the big font on the left (baseline 26).
        assert!((20..=25).any(|y| (5..=17).any(|x| px565(&px, x, y) == white)), "ordinal");
    }

    /// Hockey: a power play puts the ordinal at x=5 and a yellow badge with the
    /// short-handed-against abbrev in black, hugging the right edge (rows 21-30).
    #[test]
    fn hockey_powerplay_badge_matches_original_layout() {
        let g = sit_game("nhl", "2nd", "14:02", Situation::Hockey {
            away_powerplay: true, home_powerplay: false,
            away_players: 5, home_players: 4,
        });
        let (_, px) = render_game(&g, None, NameStyle::Name);
        let yellow = c565(YELLOW);
        let white = c565(WHITE);

        let w = text_width("SF", true); // away abbrev on the badge
        let right_point = FRAME_WIDTH as i32 - w - 4;
        // Badge corners yellow; the column left of it is untouched background.
        assert_eq!(px565(&px, right_point, 21), yellow, "badge top-left");
        assert_eq!(px565(&px, right_point + w + 2, 30), yellow, "badge bottom-right");
        assert_eq!(px565(&px, right_point - 1, 25), 0, "left of badge is background");
        // The abbrev is black ink INSIDE the badge (some badge pixels unlit-black).
        let black_inside = (22..=29).any(|y| {
            (right_point + 2..right_point + 2 + w).any(|x| px565(&px, x, y) == 0)
        });
        assert!(black_inside, "abbrev should punch black pixels through the badge");
        // Ordinal left-aligned at x=5 (not centered) while the badge shows.
        assert!((23..=28).any(|y| (5..=20).any(|x| px565(&px, x, y) == white)), "ordinal at left");
    }

    /// The spoiled frame keeps the matchup but drops the score: the team rows'
    /// score slots stay dark, situations vanish, and the strip shows only the
    /// sanctioned status (FINAL / period / start time).
    #[test]
    fn spoiled_frames_hide_scores_and_situations() {
        use scoreboard_protocol::SpoilerMode;
        let g = sit_game("mlb", "Top 7th", "", Situation::Baseball {
            balls: 3, strikes: 2, outs: 2,
            on_first: true, on_second: true, on_third: true,
            is_inning_top: true,
        });
        let (_, normal) = render_frame(&g, None, None, NameStyle::Name);
        let (_, spoiled) = render_frame(&g, Some(SpoilerMode::Normal), None, NameStyle::Name);
        assert_ne!(normal, spoiled);
        // Score digits draw in the team's SECONDARY color, right-aligned around
        // x 56..=61; the row's primary-color background fill is lit either way,
        // so the check is for secondary-colored ink specifically.
        let away_ink = c565(g.away.secondary);
        let score_ink = |px: &[u8], ys: core::ops::Range<i32>| {
            ys.clone().any(|y| (56..=61).any(|x| px565(px, x, y) == away_ink))
        };
        assert!(score_ink(&normal, 1..6), "normal frame shows the score");
        assert!(!score_ink(&spoiled, 1..6), "spoiled frame hides the away score");
        // Normal mode keeps the period in the strip.
        let strip_lit = (16..32).any(|y| (0..64).any(|x| px565(&spoiled, x, y) != 0));
        assert!(strip_lit, "normal spoiler mode still shows the period");
        // Strict mode differs from normal (start time instead of period).
        let (_, strict) = render_frame(&g, Some(SpoilerMode::Strict), None, NameStyle::Name);
        assert_ne!(spoiled, strict);
        // Finals show FINAL even when spoiled.
        let mut done = g.clone();
        done.status = GameStatus::Final;
        done.situation = None;
        let (_, final_px) = render_frame(&done, Some(SpoilerMode::Strict), None, NameStyle::Name);
        let final_strip_lit = (16..32).any(|y| (0..64).any(|x| px565(&final_px, x, y) != 0));
        assert!(final_strip_lit, "FINAL marker renders");
    }

    /// Checkered flags yield to long titles: they render only when the title
    /// clears both corners, so "BRITISH GP Q" never collides with the chrome.
    #[test]
    fn flags_yield_to_long_titles() {
        use crate::model::{Leaderboard, LeaderRow};
        let mut g = sit_game("f1", "", "", Situation::Football {
            down_string: String::new(), ball_position: String::new(), home_possession: None,
        });
        g.situation = None;
        g.home = TeamSide::empty("");
        let lb = |name: &str| Some(Leaderboard {
            name: name.into(),
            rows: vec![LeaderRow { name: "VER".into(), value: "+1".into(), ..Default::default() }],
        });
        // Corner cell (1..=2, 1..=2) is the flag's first white check.
        let corner_lit = |px: &[u8]| (1..=2).any(|y| (1..=2).any(|x| px565(px, x, y) != 0));

        g.leaderboard = lb("MONACO GP");
        let (_, px) = render_game(&g, None, NameStyle::Name);
        assert!(corner_lit(&px), "short title keeps its flags");

        g.leaderboard = lb("BRITISH GP Q");
        let (_, px) = render_game(&g, None, NameStyle::Name);
        assert!(!corner_lit(&px), "long title takes the corners instead");
    }

    /// Timing-tower alignment: ranks sit centered in a fixed two-digit column,
    /// so the three-letter codes start at the same x for "1" and "20" and the
    /// right-aligned timings line up.
    #[test]
    fn f1_rank_column_aligns_codes() {
        use crate::model::{Leaderboard, LeaderRow};
        let row = |rank: &str| LeaderRow {
            rank: rank.into(),
            name: "VER".into(),
            value: "+1.0".into(),
            color: None,
            tire: Some('S'),
        };
        let mut g = sit_game("f1", "", "", Situation::Football {
            down_string: String::new(), ball_position: String::new(), home_possession: None,
        });
        g.situation = None;
        g.home = TeamSide::empty("");
        g.leaderboard = Some(Leaderboard { name: "GP".into(), rows: vec![row("1"), row("20")] });
        let (_, px) = render_game(&g, None, NameStyle::Name);

        // Leftmost lit pixel past the rank column, per row band (baselines 12, 18).
        let code_start = |band: core::ops::RangeInclusive<i32>| {
            (10..40)
                .find(|&x| band.clone().any(|y| px565(&px, x, y) != 0))
                .expect("code drawn")
        };
        assert_eq!(
            code_start(7..=12),
            code_start(13..=18),
            "codes must start at the same x for one- and two-digit ranks"
        );
    }

    /// Deep leaderboards render tall for the board (6px pitch past the 32px
    /// panel); four or fewer rows stay static, and the app-facing render_game
    /// is always static.
    #[test]
    fn deep_leaderboards_render_tall_for_the_board() {
        use crate::model::{Leaderboard, LeaderRow};
        let mut g = sit_game("pga", "", "", Situation::Football {
            down_string: String::new(), ball_position: String::new(), home_possession: None,
        });
        g.situation = None;
        g.home = TeamSide::empty("");
        let rows = |n: usize| Some(Leaderboard {
            name: "OPEN".into(),
            rows: (0..n).map(|i| LeaderRow { name: format!("PLAYER{i}"), value: format!("-{i}"), ..Default::default() }).collect(),
        });

        g.leaderboard = rows(10);
        let (_, px, scrolls) = render_board_frame(&g, LEADERBOARD_DEFAULT_ROWS, None, None, NameStyle::Name);
        assert!(scrolls);
        let h = px.len() / (64 * 2);
        assert_eq!(h, 12 + 9 * 6 + 2, "10 rows on the 6px pitch");
        // The BOTTOM rows must actually be drawn (draw_glyph once clipped the
        // canvas at the panel height, silently blanking everything past row 31).
        let lit_below_panel = (32..h).any(|y| (0..64).any(|x| {
            let i = (y * 64 + x) * 2;
            u16::from_le_bytes([px[i], px[i + 1]]) != 0
        }));
        assert!(lit_below_panel, "rows 5..10 render past the panel bound");
        let (_, px_static) = render_game(&g, None, NameStyle::Name);
        assert_eq!(px_static.len() / (64 * 2), 32, "app frame stays panel-sized");

        g.leaderboard = rows(4);
        let (_, px, scrolls) = render_board_frame(&g, LEADERBOARD_DEFAULT_ROWS, None, None, NameStyle::Name);
        assert!(!scrolls, "a shallow field has nothing to scroll");
        assert_eq!(px.len() / (64 * 2), 32);
    }

    /// Scheduled games: records replace the 0-0 scores in the team rows and the
    /// national broadcast sits under the start time (two centered bands).
    #[test]
    fn pregame_shows_records_and_broadcast() {
        use crate::model::Pregame;
        let mut g = sit_game("nfl", "", "", Situation::Football {
            down_string: String::new(), ball_position: String::new(), home_possession: None,
        });
        g.situation = None;
        g.status = GameStatus::Pregame;
        g.start_time = "2026-10-04T20:00Z".into();
        g.pregame = Some(Pregame {
            away_record: "10-2".into(),
            home_record: "9-3".into(),
            broadcast: "ESPN".into(),
        });
        let (_, px) = render_game(&g, None, NameStyle::Name);

        // The record is wider than the 0 it replaces: the away row has
        // secondary-colored ink starting at 61 - width("10-2").
        let rx = 61 - text_width("10-2", false);
        let away_secondary = c565(g.away.secondary);
        assert!((1..=5).any(|y| (rx..=61).any(|x| px565(&px, x, y) == away_secondary)), "away record drawn");
        // A bare pregame frame (no records) puts only the 4px-wide "0" there.
        let plain = { let mut p = g.clone(); p.pregame = None; render_game(&p, None, NameStyle::Name).1 };
        assert!((1..=5).all(|y| (rx..rx + 6).all(|x| px565(&plain, x, y) != away_secondary || x >= 61 - 4)),
            "without records the slot holds just the 0");

        // Bottom strip: two grey text bands (time, then channel) with a gap row.
        let row_lit = |buf: &[u8], y: i32| (0..64).any(|x| px565(buf, x, y) != 0 && y >= 14);
        let bands = {
            let mut n = 0; let mut prev = false;
            for y in 14..32 {
                let lit = row_lit(&px, y);
                if lit && !prev { n += 1; }
                prev = lit;
            }
            n
        };
        assert_eq!(bands, 2, "start time + broadcast bands");
        let plain_bands = {
            let mut n = 0; let mut prev = false;
            for y in 14..32 {
                let lit = row_lit(&plain, y);
                if lit && !prev { n += 1; }
                prev = lit;
            }
            n
        };
        assert_eq!(plain_bands, 1, "no broadcast -> single centered line");
    }

    /// Even strength (5v5, no flags) shows no badge — the centered status line
    /// stays, so the panel doesn't flash between layouts needlessly.
    #[test]
    fn hockey_even_strength_keeps_centered_status() {
        let g = sit_game("nhl", "2nd", "14:02", Situation::Hockey {
            away_powerplay: false, home_powerplay: false,
            away_players: 5, home_players: 5,
        });
        let (_, px) = render_game(&g, None, NameStyle::Name);
        let yellow = c565(YELLOW);
        for y in 20..32 {
            for x in 0..FRAME_WIDTH as i32 {
                assert_ne!(px565(&px, x, y), yellow, "no badge at ({x},{y})");
            }
        }
    }

    /// The rendered frame draws the string the NameStyle selects — Location →
    /// team.location, Name → team.name, Abbrev → team.abbrev — and the per-league
    /// DEFAULT resolves correctly (cfb/wc → Location, nhl → Name).
    #[test]
    fn name_style_picks_the_right_string() {
        use crate::model::{hex_to_rgb, Game, GameStatus, TeamSide};
        use scoreboard_protocol::default_name_style;
        let side = |loc: &str, name: &str, abbrev: &str| TeamSide {
            abbrev: abbrev.into(),
            name: name.into(),
            location: loc.into(),
            score: 1,
            primary: hex_to_rgb("003087"),
            secondary: hex_to_rgb("ffffff"),
        };
        let game = |sport: &str| Game {
            id: "t".into(),
            sport_id: sport.into(),
            away: side("MICHIGAN", "WOLVERINES", "MICH"),
            home: side("OHIO", "BUCKEYES", "OSU"),
            status: GameStatus::Final,
            ordinal: String::new(),
            clock: String::new(),
            start_time: String::new(),
            end_time: None,
            leaderboard: None,
            session: None,
            situation: None,
            pregame: None,
            champ: None,
        };

        // The three styles yield three different frames — the chosen string
        // really changes what's drawn (location/name/abbrev all differ here).
        let g = game("cfb");
        let loc = render_game(&g, None, NameStyle::Location).1;
        let name = render_game(&g, None, NameStyle::Name).1;
        let abbrev = render_game(&g, None, NameStyle::Abbrev).1;
        assert_ne!(loc, name, "Location vs Name differ");
        assert_ne!(name, abbrev, "Name vs Abbrev differ");
        assert_ne!(loc, abbrev, "Location vs Abbrev differ");

        // cfb defaults to Location.
        assert_eq!(
            render_game(&game("cfb"), None, default_name_style("cfb")).1,
            loc,
            "cfb default resolves to Location",
        );
        // wc defaults to Location too.
        assert_eq!(
            render_game(&game("wc"), None, default_name_style("wc")).1,
            render_game(&game("wc"), None, NameStyle::Location).1,
            "wc default resolves to Location",
        );
        // nhl defaults to Name (the mascot), not Location.
        let nhl_default = render_game(&game("nhl"), None, default_name_style("nhl")).1;
        assert_eq!(
            nhl_default,
            render_game(&game("nhl"), None, NameStyle::Name).1,
            "nhl default resolves to Name",
        );
        assert_ne!(
            nhl_default,
            render_game(&game("nhl"), None, NameStyle::Location).1,
            "nhl default is not Location",
        );
    }

    /// The width check flags names that overflow their row (and `draw_team_box`
    /// truncates), while short ones fit. `avail` depends only on the layout, not
    /// the string.
    #[test]
    fn overflow_detected_by_width_check() {
        let (w_long, avail) = team_name_metrics("cfb", "WESTERN KENTUCKY");
        assert!(w_long > avail, "long location overflows: width {w_long} > avail {avail}");
        let (w_short, avail2) = team_name_metrics("cfb", "UM");
        assert!(w_short <= avail2, "short abbrev fits: width {w_short} <= avail {avail2}");
        assert_eq!(avail, avail2, "avail is layout-only, independent of the string");

        // Leaderboard names measure against the value column; a positive control
        // (a real driver code) fits comfortably.
        let (vw, vavail) = leaderboard_name_metrics("VER", "+1.0", true);
        assert!(vw <= vavail, "an F1 code fits: {vw} <= {vavail}");
    }
}

// ---- weather off-mode screens ----------------------------------------------
//
// Four layouts over one icon set. The icons are drawn from string art so the
// shapes stay editable; every one has an EVEN ink height, because the
// temperature beside them is 12 rows and its optical centre falls on a
// half-pixel — an odd-height icon lands half a row off, which is exactly the
// misalignment these layouts were rebuilt to fix. `icons_are_well_formed`
// enforces it.

const WX_CLEAR: [&str; 13] = [
    "      S      ",
    "  S       S  ",
    "    SSSSS    ",
    "   SSSSSSS   ",
    "  SSSSSSSSS  ",
    "S SSSSSSSSS S",
    "S SSSSSSSSS S",
    "  SSSSSSSSS  ",
    "   SSSSSSS   ",
    "    SSSSS    ",
    "  S       S  ",
    "      S      ",
    "             ",
];
const WX_PARTLY: [&str; 13] = [
    "   S         ",
    " S   S       ",
    "  SSS        ",
    "S SSS S      ",
    "  SSS        ",
    " S   S       ",
    "             ",
    "       CCCC  ",
    "   CCCCCCCCC ",
    "  CCCCCCCCCCC",
    " CCCCCCCCCCCC",
    "  CCCCCCCCCC ",
    "             ",
];
const WX_CLOUDY: [&str; 13] = [
    "  DDDDD      ",
    " DDDDDDDDD   ",
    "DDDDDDDDDDD  ",
    " DDDDDDDDD   ",
    "             ",
    "             ",
    "       CCCC  ",
    "   CCCCCCCCC ",
    "  CCCCCCCCCCC",
    " CCCCCCCCCCCC",
    " CCCCCCCCCCCC",
    "  CCCCCCCCCC ",
    "             ",
];
const WX_FOG: [&str; 13] = [
    " FFFFFFFFFFF ",
    "             ",
    "             ",
    "FFFFFFFFFFFFF",
    "             ",
    "             ",
    "  FFFFFFFFF  ",
    "             ",
    "             ",
    " FFFFFFFFFFF ",
    "             ",
    "             ",
    "             ",
];
const WX_DRIZZLE: [&str; 13] = [
    "      CCCC   ",
    "   CCCCCCCCC ",
    "  CCCCCCCCCCC",
    " CCCCCCCCCCCC",
    " CCCCCCCCCCCC",
    "  CCCCCCCCCC ",
    "             ",
    "   R   R   R ",
    "             ",
    "  R   R   R  ",
    "             ",
    "             ",
    "             ",
];
const WX_RAIN: [&str; 13] = [
    "      CCCC   ",
    "   CCCCCCCCC ",
    "  CCCCCCCCCCC",
    " CCCCCCCCCCCC",
    " CCCCCCCCCCCC",
    "  CCCCCCCCCC ",
    "             ",
    "   R   R   R ",
    "  R   R   R  ",
    " R   R   R   ",
    "             ",
    "             ",
    "             ",
];
const WX_SLEET: [&str; 13] = [
    "      CCCC   ",
    "   CCCCCCCCC ",
    "  CCCCCCCCCCC",
    " CCCCCCCCCCCC",
    " CCCCCCCCCCCC",
    "  CCCCCCCCCC ",
    "             ",
    "   R   R   R ",
    "             ",
    "  W   W   W  ",
    "             ",
    "             ",
    "             ",
];
const WX_SNOW: [&str; 13] = [
    "      CCCC   ",
    "   CCCCCCCCC ",
    "  CCCCCCCCCCC",
    " CCCCCCCCCCCC",
    " CCCCCCCCCCCC",
    "  CCCCCCCCCC ",
    "             ",
    " W  W  W  W  ",
    "             ",
    "   W  W  W   ",
    "             ",
    " W  W  W  W  ",
    "             ",
];
const WX_STORM: [&str; 13] = [
    "      CCCC   ",
    "   CCCCCCCCC ",
    "  CCCCCCCCCCC",
    " CCCCCCCCCCCC",
    "  CCCCCCCCCC ",
    "       BB    ",
    "      BB     ",
    "     BB      ",
    "   BBBBBB    ",
    "      BB     ",
    "     BB      ",
    "    BB       ",
    "             ",
];
const WX_CLEAR_NIGHT: [&str; 13] = [
    "    MMMM     ",
    "  MMM        ",
    "  MM         ",
    " MM          ",
    " MM          ",
    " MM          ",
    " MM          ",
    "  MM         ",
    "  MMM        ",
    "    MMMM     ",
    "             ",
    "             ",
    "             ",
];
const WX_PARTLY_NIGHT: [&str; 13] = [
    " MMM         ",
    "MM           ",
    "MM           ",
    "MM           ",
    "MM           ",
    " MMM         ",
    "             ",
    "       CCCC  ",
    "   CCCCCCCCC ",
    "  CCCCCCCCCCC",
    " CCCCCCCCCCCC",
    "  CCCCCCCCCC ",
    "             ",
];

const WX_CLEAR9: [&str; 9] = [
    "    S    ",
    "         ",
    "   SSS   ",
    "  SSSSS  ",
    "S SSSSS S",
    "  SSSSS  ",
    "   SSS   ",
    "    S    ",
    "         ",
];
const WX_PARTLY9: [&str; 9] = [
    " SSS     ",
    "SSSSS    ",
    " SSS     ",
    "         ",
    "     CCC ",
    "   CCCCCC",
    "  CCCCCCC",
    "   CCCCC ",
    "         ",
];
const WX_CLOUDY9: [&str; 9] = [
    " DDDD    ",
    "DDDDDD   ",
    " DDDD    ",
    "         ",
    "   CCC   ",
    " CCCCCCC ",
    "CCCCCCCCC",
    " CCCCCCC ",
    "         ",
];
const WX_FOG9: [&str; 9] = [
    " FFFFFFF ",
    "         ",
    "FFFFFFFFF",
    "         ",
    "         ",
    " FFFFFFF ",
    "         ",
    "  FFFFF  ",
    "         ",
];
const WX_DRIZZLE9: [&str; 9] = [
    "   CCC   ",
    " CCCCCCC ",
    "CCCCCCCCC",
    " CCCCCCC ",
    "         ",
    "  R   R  ",
    "         ",
    " R   R   ",
    "         ",
];
const WX_RAIN9: [&str; 9] = [
    "   CCC   ",
    " CCCCCCC ",
    "CCCCCCCCC",
    " CCCCCCC ",
    "         ",
    "  R  R  R",
    " R  R  R ",
    "R  R  R  ",
    "         ",
];
const WX_SLEET9: [&str; 9] = [
    "   CCC   ",
    " CCCCCCC ",
    "CCCCCCCCC",
    " CCCCCCC ",
    "         ",
    "  R   R  ",
    "         ",
    " W   W   ",
    "         ",
];
const WX_SNOW9: [&str; 9] = [
    "   CCC   ",
    " CCCCCCC ",
    "CCCCCCCCC",
    " CCCCCCC ",
    "         ",
    " W  W  W ",
    "         ",
    "  W  W   ",
    "         ",
];
const WX_STORM9: [&str; 9] = [
    "  CCCCC  ",
    " CCCCCCC ",
    "CCCCCCCCC",
    "    BB   ",
    "   BB    ",
    " BBBBB   ",
    "   BB    ",
    "  BB     ",
    "         ",
];
const WX_CLEAR_NIGHT9: [&str; 9] = [
    "  MMM    ",
    " MM      ",
    "MM       ",
    "MM       ",
    "MM       ",
    "MM       ",
    " MM      ",
    "  MMM    ",
    "         ",
];
const WX_PARTLY_NIGHT9: [&str; 9] = [
    " MM      ",
    "MM       ",
    "MM       ",
    " MM      ",
    "    CCC  ",
    "  CCCCCC ",
    " CCCCCCCC",
    "  CCCCCC ",
    "         ",
];

/// The art for a symbol name, at the size a layout needs. Unknown names fall
/// back to the plain cloud rather than to nothing: a blank sky reads as a
/// broken screen, which is the same rule the off-mode dispatch follows.
fn wx_art(name: &str, small: bool) -> &'static [&'static str] {
    match (name, small) {
        ("clear", false) => &WX_CLEAR,
        ("clear", true) => &WX_CLEAR9,
        ("partly", false) => &WX_PARTLY,
        ("partly", true) => &WX_PARTLY9,
        ("fog", false) => &WX_FOG,
        ("fog", true) => &WX_FOG9,
        ("drizzle", false) => &WX_DRIZZLE,
        ("drizzle", true) => &WX_DRIZZLE9,
        ("rain", false) => &WX_RAIN,
        ("rain", true) => &WX_RAIN9,
        ("sleet", false) => &WX_SLEET,
        ("sleet", true) => &WX_SLEET9,
        ("snow", false) => &WX_SNOW,
        ("snow", true) => &WX_SNOW9,
        ("storm", false) => &WX_STORM,
        ("storm", true) => &WX_STORM9,
        ("clear-night", false) => &WX_CLEAR_NIGHT,
        ("clear-night", true) => &WX_CLEAR_NIGHT9,
        ("partly-night", false) => &WX_PARTLY_NIGHT,
        ("partly-night", true) => &WX_PARTLY_NIGHT9,
        (_, false) => &WX_CLOUDY,
        (_, true) => &WX_CLOUDY9,
    }
}

fn wx_color(ch: char) -> Option<Rgb888> {
    Some(match ch {
        'S' => Rgb888::new(255, 200, 40),
        'M' => Rgb888::new(225, 225, 170),
        'C' => Rgb888::new(185, 198, 215),
        'D' => Rgb888::new(105, 115, 130),
        'R' => Rgb888::new(80, 160, 255),
        'W' => Rgb888::new(235, 245, 255),
        'F' => Rgb888::new(150, 158, 170),
        'B' => Rgb888::new(255, 225, 60),
        _ => return None,
    })
}

/// Offset that centres an ink span `[lo, hi]` on `cy`.
///
/// `div_euclid` (floor), NOT `/` (truncate toward zero). Text ink extents are
/// NEGATIVE (measured up from the baseline) and icon extents POSITIVE, so plain
/// division rounds the two in opposite directions — which put every icon
/// exactly one pixel below the number beside it.
fn wx_center_off(lo: i32, hi: i32, cy: i32) -> i32 {
    cy - (lo + hi).div_euclid(2)
}

fn wx_bbox(art: &[&str]) -> (i32, i32, i32, i32) {
    let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for (r, row) in art.iter().enumerate() {
        for (col, ch) in row.chars().enumerate() {
            if wx_color(ch).is_some() {
                x0 = x0.min(col as i32);
                x1 = x1.max(col as i32);
                y0 = y0.min(r as i32);
                y1 = y1.max(r as i32);
            }
        }
    }
    (x0, y0, x1, y1)
}

/// Draw an icon with its INK centred on (cx, cy) — optical centring, not
/// bounding-box centring, so swapping one symbol for another never shifts a
/// layout. Returns the ink row span.
fn wx_icon(c: &mut Canvas, art: &[&str], cx: i32, cy: i32) -> (i32, i32) {
    let (x0, y0, x1, y1) = wx_bbox(art);
    let (ox, oy) = (wx_center_off(x0, x1, cx), wx_center_off(y0, y1, cy));
    for (r, row) in art.iter().enumerate() {
        for (col, ch) in row.chars().enumerate() {
            if let Some(color) = wx_color(ch) {
                let (px, py) = (ox + col as i32, oy + r as i32);
                if px >= 0 && py >= 0 && (px as usize) < FRAME_WIDTH && (py as usize) < c.height {
                    c.buf[py as usize * FRAME_WIDTH + px as usize] = color;
                }
            }
        }
    }
    (oy + y0, oy + y1)
}

fn wx_icon_w(art: &[&str]) -> i32 {
    let (x0, _, x1, _) = wx_bbox(art);
    x1 - x0 + 1
}

/// `draw_text` at an integer scale, for a temperature readable across a room.
fn wx_draw_scaled(c: &mut Canvas, s: &str, x: i32, by: i32, big: bool, scale: i32, color: Rgb888) {
    let font = if big { crate::bdf::font_5x8() } else { crate::bdf::font_4x6() };
    let mut cx = x;
    let mut prev: Option<char> = None;
    for ch in s.chars() {
        if let Some(p) = prev {
            cx += kerned_advance(p, ch, big) * scale;
        }
        if let Some(g) = font.glyph(ch) {
            for (row, &byte) in g.rows.iter().enumerate() {
                for col in 0..g.w {
                    if (byte >> (7 - col)) & 1 == 1 {
                        let dx = g.xoff + col;
                        let dy = row as i32 - g.yoff - g.h + 1;
                        fill(c, cx + dx * scale, by + dy * scale, scale as u32, scale as u32, color);
                    }
                }
            }
        }
        prev = Some(ch);
    }
}

/// Vertical ink extent of a run drawn at baseline 0.
fn wx_ink_rows(s: &str, big: bool, scale: i32) -> (i32, i32) {
    let font = if big { crate::bdf::font_5x8() } else { crate::bdf::font_4x6() };
    let (mut top, mut bot) = (i32::MAX, i32::MIN);
    for ch in s.chars() {
        if let Some(g) = font.glyph(ch) {
            for (row, &byte) in g.rows.iter().enumerate() {
                if byte == 0 {
                    continue;
                }
                let dy = row as i32 - g.yoff - g.h + 1;
                top = top.min(dy * scale);
                bot = bot.max(dy * scale + scale - 1);
            }
        }
    }
    if top == i32::MAX {
        (0, 0)
    } else {
        (top, bot)
    }
}

/// Draw text centred on row `cy` by its INK, so a big number and a small one in
/// the same band line up optically instead of sharing a baseline that makes
/// their different cap heights look crooked. Returns the ink row span.
fn wx_text(c: &mut Canvas, s: &str, x: i32, cy: i32, big: bool, scale: i32, color: Rgb888) -> (i32, i32) {
    let (t, b) = wx_ink_rows(s, big, scale);
    let by = wx_center_off(t, b, cy);
    wx_draw_scaled(c, s, x, by, big, scale, color);
    (by + t, by + b)
}

fn wx_w(s: &str, big: bool, scale: i32) -> i32 {
    text_width(s, big) * scale
}

const WX_HOT: Rgb888 = Rgb888::new(255, 150, 90);
const WX_COLD: Rgb888 = Rgb888::new(110, 180, 255);
const WX_DIM: Rgb888 = Rgb888::new(120, 120, 120);

fn wx_temp(t: i32) -> String {
    format!("{t}\u{b0}")
}

/// Icon + temperature on one centre line, and nothing else.
pub fn render_weather_simple(w: &crate::weather::Weather) -> (FrameId, Vec<u8>) {
    let mut c = Canvas::new();
    let art = wx_art(crate::weather::icon_for(w.code, w.is_day), false);
    let temp = wx_temp(w.temp);
    let gap = 5;
    let group = wx_icon_w(art) + gap + wx_w(&temp, true, 2);
    let gx = (FRAME_WIDTH as i32 - group) / 2;
    let cy = 15;
    wx_icon(&mut c, art, gx + wx_icon_w(art) / 2, cy);
    wx_text(&mut c, &temp, gx + wx_icon_w(art) + gap, cy, true, 2, WHITE);
    let px = c.to_rgb565();
    (frame_id(&px), px)
}

/// Icon, temperature, the day's range, and the condition in words. No rule
/// between the two blocks: the three whitespace bands ARE the structure, and
/// `weather_detailed_spaces_its_bands_evenly` holds them equal.
pub fn render_weather_detailed(w: &crate::weather::Weather) -> (FrameId, Vec<u8>) {
    let mut c = Canvas::new();
    let art = wx_art(crate::weather::icon_for(w.code, w.is_day), false);
    let temp = wx_temp(w.temp);
    let top_cy = 10;
    let t_span = wx_text(&mut c, &temp, 17, top_cy, true, 2, WHITE);
    wx_icon(&mut c, art, 8, top_cy);
    // Anchored to the temperature's MEASURED span, so the pair occupies exactly
    // the same rows whatever the glyphs turn out to be.
    let (hi, lo) = (w.hi.to_string(), w.lo.to_string());
    wx_text(&mut c, &hi, 61 - text_width(&hi, false), t_span.0 + 2, false, 1, WX_HOT);
    wx_text(&mut c, &lo, 61 - text_width(&lo, false), t_span.1 - 2, false, 1, WX_COLD);
    let cond = crate::weather::condition_for(w.code);
    wx_text(&mut c, cond, (FRAME_WIDTH as i32 - text_width(cond, false)) / 2, 24, false, 1, WX_DIM);
    let px = c.to_rgb565();
    (frame_id(&px), px)
}

/// Three days across, in equal columns.
pub fn render_weather_forecast(w: &crate::weather::Weather) -> (FrameId, Vec<u8>) {
    let mut c = Canvas::new();
    for (i, day) in w.days.iter().take(3).enumerate() {
        let cx = i as i32 * 22 + 10;
        let tint = if i == 0 { WHITE } else { WX_DIM };
        // "TODAY" is as wide as the column; the short form always fits.
        let label = if day.label.len() > 3 { &day.label[..3] } else { &day.label };
        wx_text(&mut c, label, cx - text_width(label, false) / 2, 3, false, 1, tint);
        // Daily codes have no day/night sense — a forecast is for the daytime.
        wx_icon(&mut c, wx_art(crate::weather::icon_for(day.code, true), true), cx, 12);
        let (hi, lo) = (day.hi.to_string(), day.lo.to_string());
        wx_text(&mut c, &hi, cx - text_width(&hi, false) / 2, 22, false, 1, WX_HOT);
        wx_text(&mut c, &lo, cx - text_width(&lo, false) / 2, 28, false, 1, WX_COLD);
        if i > 0 {
            fill(&mut c, cx - 11, 1, 1, 30, Rgb888::new(30, 30, 30));
        }
    }
    let px = c.to_rgb565();
    (frame_id(&px), px)
}

/// The clock, with weather sharing the panel underneath it.
pub fn render_weather_clock(
    w: &crate::weather::Weather,
    hour24: u32,
    minute: u32,
    h24: bool,
) -> (FrameId, Vec<u8>) {
    let mut c = Canvas::new();
    let (time, meridiem) = clock_parts(hour24, minute, h24);
    let gap = 3;
    let meridiem_w = meridiem.map(|m| gap + text_width(m, false)).unwrap_or(0);
    let width = wx_w(&time, true, 2) + meridiem_w;
    let x0 = (FRAME_WIDTH as i32 - width) / 2;
    let clock_span = wx_text(&mut c, &time, x0, 9, true, 2, WHITE);
    // The meridiem rides the BOTTOM of the digits: floating at mid-height it
    // reads as part of the number rather than as an annotation.
    if let Some(m) = meridiem {
        wx_text(&mut c, m, x0 + wx_w(&time, true, 2) + gap, clock_span.1 - 2, false, 1, WX_DIM);
    }

    let art = wx_art(crate::weather::icon_for(w.code, w.is_day), true);
    let scy = 23;
    wx_icon(&mut c, art, 6, scy);
    let temp = wx_temp(w.temp);
    wx_text(&mut c, &temp, 14, scy, true, 1, WHITE);
    let (hi, lo) = (w.hi.to_string(), w.lo.to_string());
    wx_text(&mut c, &hi, 49 - text_width(&hi, true), scy, true, 1, WX_HOT);
    wx_text(&mut c, &lo, 63 - text_width(&lo, true), scy, true, 1, WX_COLD);
    let px = c.to_rgb565();
    (frame_id(&px), px)
}

#[cfg(test)]
mod wx_tests {
    use super::*;

    /// Every symbol must have BOTH sizes, rows of the right width, and an EVEN
    /// ink height. The parity is the load-bearing part: the temperature beside
    /// these is 12 rows, so its optical centre falls on a half-pixel, and an
    /// odd-height icon lands half a row off — the exact misalignment these
    /// layouts were rebuilt to remove.
    #[test]
    fn icons_are_well_formed() {
        const NAMES: [&str; 11] = [
            "clear", "clear-night", "partly", "partly-night", "cloudy", "fog", "drizzle", "rain",
            "sleet", "snow", "storm",
        ];
        for name in NAMES {
            for (small, w) in [(false, 13usize), (true, 9usize)] {
                let art = wx_art(name, small);
                for (i, row) in art.iter().enumerate() {
                    assert_eq!(row.chars().count(), w, "{name} ({w}px) row {i}");
                }
                let (x0, y0, x1, y1) = wx_bbox(art);
                assert!(x0 <= x1 && y0 <= y1, "{name} ({w}px) has no ink");
                assert_eq!((y1 - y0 + 1) % 2, 0, "{name} ({w}px) ink height must be even");
            }
        }
    }

    /// A symbol swap must never knock the temperature off its line.
    ///
    /// The invariant is the shared CENTRE, not an identical span: symbols are
    /// legitimately different heights (fog is 10 ink rows, the sun 12). Because
    /// every height is even, the centres coincide exactly — compared doubled,
    /// so there is no rounding to argue about.
    #[test]
    fn the_temperature_holds_its_line_across_every_condition() {
        let mut centers = std::collections::HashSet::new();
        for code in [0u16, 2, 3, 45, 51, 63, 66, 71, 95] {
            let w = crate::weather::Weather { code, ..crate::weather::Weather::sample() };
            let mut c = Canvas::new();
            let art = wx_art(crate::weather::icon_for(w.code, true), false);
            let temp = wx_temp(w.temp);
            let icon = wx_icon(&mut c, art, 8, 10);
            let text = wx_text(&mut c, &temp, 17, 10, true, 2, WHITE);
            assert_eq!(
                icon.0 + icon.1,
                text.0 + text.1,
                "code {code}: icon and temperature must share a centre line"
            );
            centers.insert(icon.0 + icon.1);
        }
        assert_eq!(centers.len(), 1, "every condition centres on the same line");
    }

    /// Three digits and a minus sign both happen. Neither may run off the panel.
    #[test]
    fn extreme_temperatures_stay_on_the_panel() {
        for (t, hi, lo) in [(-40, -20, -45), (105, 112, 88), (0, 5, -9)] {
            let w = crate::weather::Weather { temp: t, hi, lo, ..crate::weather::Weather::sample() };
            for (name, (_, px)) in [
                ("simple", render_weather_simple(&w)),
                ("detailed", render_weather_detailed(&w)),
                ("forecast", render_weather_forecast(&w)),
                ("clock", render_weather_clock(&w, 13, 5, false)),
            ] {
                assert_eq!(px.len(), FRAME_WIDTH * FRAME_HEIGHT * 2, "{name} at {t}");
                assert!(px.iter().any(|&b| b != 0), "{name} at {t} rendered an empty panel");
            }
        }
    }

    /// Different weather has to produce different pixels — a layout that
    /// ignored its input would still pass every geometry check above.
    #[test]
    fn the_screens_actually_reflect_the_weather() {
        let a = crate::weather::Weather::sample();
        let b = crate::weather::Weather { temp: 12, code: 71, hi: 20, lo: 4, ..a.clone() };
        assert_ne!(render_weather_simple(&a).0, render_weather_simple(&b).0);
        assert_ne!(render_weather_detailed(&a).0, render_weather_detailed(&b).0);
        assert_ne!(render_weather_clock(&a, 9, 0, false).0, render_weather_clock(&b, 9, 0, false).0);
        // And the clock half moves on its own.
        assert_ne!(render_weather_clock(&a, 9, 0, false).0, render_weather_clock(&a, 9, 1, false).0);
    }
}

#[cfg(test)]
mod wx_dump {
    use super::*;
    #[test]
    #[ignore = "writes weather previews; run with --ignored"]
    fn dump() {
        let w = crate::weather::Weather::sample();
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../mockups");
        std::fs::create_dir_all(&out).unwrap();
        let shots: [(&str, (FrameId, Vec<u8>)); 4] = [
            ("simple", render_weather_simple(&w)),
            ("detailed", render_weather_detailed(&w)),
            ("forecast", render_weather_forecast(&w)),
            ("clock", render_weather_clock(&w, 22, 9, false)),
        ];
        for (name, (_, px)) in shots {
            let mut img = image::RgbImage::new(FRAME_WIDTH as u32 * 8, FRAME_HEIGHT as u32 * 8);
            for y in 0..FRAME_HEIGHT {
                for x in 0..FRAME_WIDTH {
                    let v = u16::from_le_bytes([px[(y * FRAME_WIDTH + x) * 2], px[(y * FRAME_WIDTH + x) * 2 + 1]]);
                    let (r, g, b) = (
                        ((v >> 11) as u8) << 3, (((v >> 5) & 0x3F) as u8) << 2, ((v & 0x1F) as u8) << 3,
                    );
                    if r == 0 && g == 0 && b == 0 { continue; }
                    for dy in 0..6u32 { for dx in 0..6u32 {
                        img.put_pixel(x as u32 * 8 + 1 + dx, y as u32 * 8 + 1 + dy, image::Rgb([r, g, b]));
                    }}
                }
            }
            img.save(out.join(format!("wx-{name}.png"))).unwrap();
        }
    }
}
