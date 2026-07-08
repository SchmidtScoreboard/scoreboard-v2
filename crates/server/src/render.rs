//! Server-side rendering of a game into a 64x32 RGB565 frame, reproducing the
//! ORIGINAL scoreboard layout pixel-for-pixel: two team-colored rows (secondary
//! accent stripe + name left, score right) and a bottom status line.
//!
//! Matching the original (rust-scoreboard) exactly means:
//!  - box height = effective glyph height + 2*spacing  (5x8 glyph = 6px, 4x6 = 5px),
//!  - text drawn on its ALPHABETIC baseline at `y_offset + spacing + glyph_height`
//!    (so the glyph has equal `spacing` margin top and bottom = vertically centered),
//!  - per-character kerning overrides Mark used (5x8: I,T->4 Y->6 ; 4x6: N->5).

use crate::model::{Game, GameStatus, TeamSide};
use embedded_graphics::{
    draw_target::DrawTarget,
    geometry::{OriginDimensions, Size},
    pixelcolor::Rgb888,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
};
use scoreboard_protocol::{
    frame_id, rgb888_to_565, FrameId, FRAME_BYTES, FRAME_HEIGHT, FRAME_PIXELS, FRAME_WIDTH,
};

const WHITE: Rgb888 = Rgb888::new(255, 255, 255);
const YELLOW: Rgb888 = Rgb888::new(255, 255, 0);
const GREY: Rgb888 = Rgb888::new(160, 160, 160);
const ACCENT_W: u32 = 2;
const NAME_X: i32 = ACCENT_W as i32 + 3; // 5
const SCORE_RIGHT: i32 = 61; // 64 - 3

struct Canvas {
    buf: Vec<Rgb888>,
}
impl Canvas {
    fn new() -> Self {
        Self {
            buf: vec![Rgb888::BLACK; FRAME_PIXELS],
        }
    }
    fn to_rgb565(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(FRAME_BYTES);
        for p in &self.buf {
            v.extend_from_slice(&rgb888_to_565(p.r(), p.g(), p.b()).to_le_bytes());
        }
        v
    }
}
impl OriginDimensions for Canvas {
    fn size(&self) -> Size {
        Size::new(FRAME_WIDTH as u32, FRAME_HEIGHT as u32)
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
            if p.x >= 0 && p.y >= 0 && (p.x as usize) < FRAME_WIDTH && (p.y as usize) < FRAME_HEIGHT
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
                if x >= 0 && y >= 0 && (x as usize) < FRAME_WIDTH && (y as usize) < FRAME_HEIGHT {
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

fn draw_team_box(c: &mut Canvas, team: &TeamSide, y_offset: i32, lay: &Layout) {
    let bh = box_h(lay);
    fill(c, 0, y_offset, FRAME_WIDTH as u32, bh as u32, team.primary);
    fill(c, 0, y_offset, ACCENT_W, bh as u32, team.secondary);
    let by = centered_baseline(y_offset, bh, lay.big);
    draw_text(c, &team.name.to_ascii_uppercase(), NAME_X, by, lay.big, team.secondary);
    draw_text_right(c, &team.score.to_string(), SCORE_RIGHT, by, lay.big, team.secondary);
}

fn status_for(g: &Game) -> (String, Rgb888) {
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
            crate::start_time_label(&g.start_time).unwrap_or_else(|| "PREGAME".to_string()),
            GREY,
        ),
    }
}

pub fn render_game(g: &Game) -> (FrameId, Vec<u8>) {
    if g.leaderboard.is_some() {
        return render_leaderboard(g);
    }
    let mut c = Canvas::new();
    let lay = layout_for(&g.sport_id);

    draw_team_box(&mut c, &g.away, 0, &lay);
    draw_team_box(&mut c, &g.home, box_h(&lay), &lay);

    // Status line centered (both axes) in the strip below the two team rows.
    let (s, color) = status_for(g);
    let strip_top = 2 * box_h(&lay);
    let by = centered_baseline(strip_top, FRAME_HEIGHT as i32 - strip_top, lay.status_big);
    let sx = (FRAME_WIDTH as i32 - text_width(&s, lay.status_big)) / 2;
    draw_text(&mut c, &s, sx, by, lay.status_big, color);

    let px = c.to_rgb565();
    (frame_id(&px), px)
}

/// Golf / F1 leaderboard (ported from the original `src/golf.rs`): event name
/// centered up top (green for golf, red for F1), then up to 4 name/value rows.
fn render_leaderboard(g: &Game) -> (FrameId, Vec<u8>) {
    let mut c = Canvas::new();
    let lb = g.leaderboard.as_ref().unwrap();
    let accent = if g.sport_id == "f1" {
        Rgb888::new(225, 6, 0) // F1 red
    } else {
        Rgb888::new(52, 162, 35) // golf green
    };
    draw_text(&mut c, &lb.name, 32 - text_width(&lb.name, false) / 2, 5, false, accent);

    if matches!(g.status, GameStatus::Pregame) {
        let (s, _) = status_for(g);
        let by = centered_baseline(8, FRAME_HEIGHT as i32 - 8, true);
        draw_text(&mut c, &s, 32 - text_width(&s, true) / 2, by, true, WHITE);
    } else {
        // Match the original golf.rs density: 4x6 rows on a 6px pitch, so the title
        // + 4 rows = 5 rows fit the 32px panel (the original fit 5, not 4). The whole
        // block sits one pixel down from the top so the gap is even (1px) top + bottom.
        let mut by = 12;
        for row in lb.rows.iter().take(5) {
            let name_max_right = 63 - text_width(&row.value, false) - 2;
            draw_text_clip(&mut c, &row.name, 1, by, name_max_right, false, WHITE);
            draw_text_right(&mut c, &row.value, 63, by, false, accent);
            by += 6;
        }
    }

    let px = c.to_rgb565();
    (frame_id(&px), px)
}

/// Render a centered multi-line notice (e.g. "SLEEPING / FOR 24 HOURS").
/// Each line is (text, big_font, color); lines are stacked with a 2px gap and the
/// whole block is centered vertically. Lines wider than the panel are clipped by
/// the canvas — keep big lines ≤ ~10 chars and small ones ≤ ~12.
///
/// `remaining` (0.0..=1.0) draws a depleting progress bar along the bottom row
/// showing how long until the notice goes away; `None` omits it (e.g. the clock).
pub fn render_notice(lines: &[(&str, bool, Rgb888)], remaining: Option<f32>) -> (FrameId, Vec<u8>) {
    let mut c = Canvas::new();
    let line_h = |big: bool| if big { 6 } else { 5 };
    let gap = 2;
    // Reserve the bottom rows for the bar so the text block centers above it.
    let content_h = if remaining.is_some() { FRAME_HEIGHT as i32 - 3 } else { FRAME_HEIGHT as i32 };
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
        let w = (rem.clamp(0.0, 1.0) * FRAME_WIDTH as f32).round() as u32;
        if w > 0 {
            fill(&mut c, 0, FRAME_HEIGHT as i32 - 1, w, 1, GREY);
        }
    }
    let px = c.to_rgb565();
    (frame_id(&px), px)
}

/// The idle clock: big 12-hour time centered, small AM/PM below. Rendered
/// server-side (the board is a dumb display), re-pushed each minute while shown.
pub fn render_clock(hour24: u32, minute: u32) -> (FrameId, Vec<u8>) {
    let (h12, ampm) = match hour24 % 24 {
        0 => (12, "AM"),
        h if h < 12 => (h, "AM"),
        12 => (12, "PM"),
        h => (h - 12, "PM"),
    };
    let time = format!("{}:{:02}", h12, minute);
    render_notice(&[(&time, true, WHITE), (ampm, false, GREY)], None)
}

#[cfg(test)]
mod kern_tests {
    use super::*;

    /// The team-game status line (period/clock) must be horizontally centered in the
    /// bottom strip — it used to be left-aligned at NAME_X.
    #[test]
    fn team_status_line_is_centered() {
        use crate::model::{Game, GameStatus, TeamSide};
        let side = |name: &str| TeamSide {
            abbrev: "X".into(),
            name: name.into(),
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
            start_time: String::new(),
            leaderboard: None,
            session: None,
            champ: None,
        };
        let (_, px) = render_game(&g);
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
            start_time: String::new(),
            leaderboard: Some(Leaderboard {
                name: "MONACO GP".into(),
                rows: vec![
                    LeaderRow { name: "VERSTAPPEN".into(), value: "L42".into() },
                    LeaderRow { name: "NORRIS".into(), value: "+2.1".into() },
                    LeaderRow { name: "LECLERC".into(), value: "+5.8".into() },
                    LeaderRow { name: "PIASTRI".into(), value: "+3.0".into() },
                ],
            }),
            champ: None,
            session: Some("race".into()),
        };
        let (_, px) = render_leaderboard(&g); // 64x32 RGB565-LE, 2 bytes/pixel
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
}
