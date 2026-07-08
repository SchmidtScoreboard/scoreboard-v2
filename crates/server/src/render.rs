//! Server-side rendering of a game into a 64x32 RGB565 frame, reproducing the
//! ORIGINAL scoreboard layout pixel-for-pixel: two team-colored rows (secondary
//! accent stripe + name left, score right) and a bottom status line.
//!
//! Matching the original (rust-scoreboard) exactly means:
//!  - box height = effective glyph height + 2*spacing  (5x8 glyph = 6px, 4x6 = 5px),
//!  - text drawn on its ALPHABETIC baseline at `y_offset + spacing + glyph_height`
//!    (so the glyph has equal `spacing` margin top and bottom = vertically centered),
//!  - per-character kerning overrides Mark used (5x8: I,T->4 Y->6 ; 4x6: N->5).

use crate::model::{Game, GameStatus, Situation, TeamSide};
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
                if x >= 0 && y >= 0 && (x as usize) < FRAME_WIDTH && (y as usize) < FRAME_HEIGHT {
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

/// `accent_w` is normally ACCENT_W; football widens the possessing team's
/// stripe to 8 (the original's possession cue), which also shifts the name.
fn draw_team_box(c: &mut Canvas, team: &TeamSide, y_offset: i32, lay: &Layout, accent_w: u32) {
    let bh = box_h(lay);
    fill(c, 0, y_offset, FRAME_WIDTH as u32, bh as u32, team.primary);
    // The original's rectangle x-endpoints were inclusive, so its width-8
    // possession stripe lit 9 columns — the ball sprite (x 1..=7) sits framed
    // with a 1px margin each side. The plain 2px stripe stays v2's look.
    let stripe = if accent_w > ACCENT_W { accent_w + 1 } else { accent_w };
    fill(c, 0, y_offset, stripe, bh as u32, team.secondary);
    let by = centered_baseline(y_offset, bh, lay.big);
    draw_text(c, &team.name.to_ascii_uppercase(), accent_w as i32 + 3, by, lay.big, team.secondary);
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

    // Football possession part 1: the side with the ball gets an 8px accent
    // stripe (its name shifts right with it), the other keeps the normal 2px.
    let (away_accent, home_accent) = match &g.situation {
        Some(Situation::Football { home_possession: Some(home_ball), .. }) => {
            if *home_ball { (ACCENT_W, 8) } else { (8, ACCENT_W) }
        }
        _ => (ACCENT_W, ACCENT_W),
    };
    draw_team_box(&mut c, &g.away, 0, &lay, away_accent);
    draw_team_box(&mut c, &g.home, box_h(&lay), &lay, home_accent);

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
            // Status line centered (both axes) in the strip below the two team rows.
            let (s, color) = status_for(g);
            let strip_top = 2 * box_h(&lay);
            let by = centered_baseline(strip_top, FRAME_HEIGHT as i32 - strip_top, lay.status_big);
            let sx = (FRAME_WIDTH as i32 - text_width(&s, lay.status_big)) / 2;
            draw_text(&mut c, &s, sx, by, lay.status_big, color);
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
            situation: None,
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
            situation: None,
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
            start_time: String::new(),
            leaderboard: None,
            champ: None,
            session: None,
            situation: Some(sit),
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
        let (_, px) = render_game(&g);

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
        let (_, px) = render_game(&g);
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
        let (_, px) = render_game(&g);
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

    /// Even strength (5v5, no flags) shows no badge — the centered status line
    /// stays, so the panel doesn't flash between layouts needlessly.
    #[test]
    fn hockey_even_strength_keeps_centered_status() {
        let g = sit_game("nhl", "2nd", "14:02", Situation::Hockey {
            away_powerplay: false, home_powerplay: false,
            away_players: 5, home_players: 5,
        });
        let (_, px) = render_game(&g);
        let yellow = c565(YELLOW);
        for y in 20..32 {
            for x in 0..FRAME_WIDTH as i32 {
                assert_ne!(px565(&px, x, y), yellow, "no badge at ({x},{y})");
            }
        }
    }
}
