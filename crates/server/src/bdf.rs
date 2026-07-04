//! Minimal BDF bitmap-font loader so the server renders text with the ORIGINAL
//! project's exact glyphs (X11 `4x6` / `5x8`) instead of the embedded-graphics
//! look-alikes (whose 'N' etc. read wrong). Only ASCII glyphs are kept, and the
//! fonts are <= 8px wide so each bitmap row is a single byte (MSB = leftmost px).

use std::collections::HashMap;
use std::sync::OnceLock;

pub struct Glyph {
    pub w: i32,
    pub h: i32,
    pub xoff: i32,
    pub yoff: i32,
    pub rows: Vec<u8>, // one byte per row, MSB = leftmost pixel
}

pub struct BdfFont {
    glyphs: HashMap<char, Glyph>,
}

impl BdfFont {
    pub fn glyph(&self, ch: char) -> Option<&Glyph> {
        self.glyphs.get(&ch)
    }
}

fn parse(src: &str) -> BdfFont {
    let mut glyphs = HashMap::new();
    let mut lines = src.lines();
    while let Some(line) = lines.next() {
        if !line.starts_with("STARTCHAR") {
            continue;
        }
        let mut encoding: i32 = -1;
        let mut bbx = (0i32, 0i32, 0i32, 0i32);
        let mut rows: Vec<u8> = Vec::new();
        for l in lines.by_ref() {
            if let Some(rest) = l.strip_prefix("ENCODING ") {
                encoding = rest.trim().parse().unwrap_or(-1);
            } else if let Some(rest) = l.strip_prefix("BBX ") {
                let n: Vec<i32> = rest.split_whitespace().filter_map(|s| s.parse().ok()).collect();
                if n.len() == 4 {
                    bbx = (n[0], n[1], n[2], n[3]);
                }
            } else if l == "BITMAP" {
                for bl in lines.by_ref() {
                    if bl.starts_with("ENDCHAR") {
                        break;
                    }
                    if let Ok(b) = u8::from_str_radix(bl.trim().get(0..2).unwrap_or("00"), 16) {
                        rows.push(b);
                    }
                }
                break;
            }
        }
        if (0..128).contains(&encoding) {
            if let Some(ch) = char::from_u32(encoding as u32) {
                glyphs.insert(ch, Glyph { w: bbx.0, h: bbx.1, xoff: bbx.2, yoff: bbx.3, rows });
            }
        }
    }
    BdfFont { glyphs }
}

static FONT_4X6: OnceLock<BdfFont> = OnceLock::new();
static FONT_5X8: OnceLock<BdfFont> = OnceLock::new();

pub fn font_4x6() -> &'static BdfFont {
    FONT_4X6.get_or_init(|| parse(include_str!("../fonts/4x6.bdf")))
}
pub fn font_5x8() -> &'static BdfFont {
    FONT_5X8.get_or_init(|| parse(include_str!("../fonts/5x8.bdf")))
}
