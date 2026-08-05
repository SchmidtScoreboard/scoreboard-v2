//! Off modes: what the panel shows while the scoreboard is idle or off.
//!
//! # This file is the fork seam
//!
//! Off modes are DECLARED to the app as data (`Catalog::off_modes`), not
//! hardcoded in it. To add one — stocks, a transit board, a photo of the dog —
//! you add an entry to [`off_modes`] and an arm to [`render_off_mode`]. That is
//! the whole change. No protocol version bump, no app rebuild, no firmware
//! release: the stock iOS app will list your mode, show its preview, and let
//! people pick it, because it never learns what any `id` means.
//!
//! Two rules keep that true, and both are easy to break:
//!
//! 1. **The descriptor carries every string the app needs.** Label, subtitle,
//!    the input's placeholder. The moment anything reads `if id == "weather"`,
//!    on either side, forks start needing app releases again. (The league list
//!    is dynamic in exactly this way and the app still switches on league ids
//!    in about eight places — that is the failure mode to avoid.)
//!
//! 2. **A new id never reaches the fleet.** `Manifest::off_state` is a closed
//!    enum with no serde default, and firmware drops an unparseable message
//!    silently, so a board that received a novel value would freeze on its last
//!    manifest forever. Boards are sent frames and a Blank/Clock flag; the id
//!    lives only between the app, the server, and the database.

use scoreboard_protocol::{FrameId, OffModeDescriptor, OffModeInput};
use std::collections::BTreeMap;

use crate::render;

/// A rendered idle screen: the content-addressed frame plus how long it holds.
pub struct OffFrame {
    pub id: FrameId,
    pub px: Vec<u8>,
    pub dwell_ms: u32,
}

/// True ONLY for the plain clock, which the board can draw itself once it has
/// been told the time. The server still renders and ships the frame — old
/// firmware blits it, and so does a board that hasn't received a `Time` yet —
/// but a board that can draw it locally keeps ticking through an outage
/// instead of freezing on a picture of a minute that has passed.
///
/// Deliberately an EXACT match, not "anything that isn't blank". The
/// clock+weather screen also shows a time, but the board has no weather and
/// would draw a bare clock straight over it.
pub fn is_local_clock(id: &str) -> bool {
    id == "clock"
}

/// Everything a mode is allowed to depend on. Add fields here rather than
/// reaching into `AppState` — it is what keeps modes renderable from the
/// preview route, which has no board and no config.
pub struct OffCtx<'a> {
    /// The board's timezone, already resolved (falls back to the server's).
    pub tz: Option<chrono_tz::Tz>,
    /// Values the user typed for this mode's declared input.
    pub params: &'a BTreeMap<String, String>,
    /// True when rendering for `/offmode/{id}.png`. Modes must produce a
    /// STABLE image in this state — the app caches previews for a day, and two
    /// fetches have to be byte-identical.
    pub preview: bool,
    /// What the weather cache holds for this board's location.
    pub weather: crate::weather::Reading,
}

/// The wall time every preview renders at. Fixed for the same reason
/// `GUIDE_TICK_MS` is: a live minute would pin whatever value happened to be
/// current into the app's URL cache, and read as a bug besides.
const PREVIEW_TIME: (u32, u32) = (10, 9);

/// The off modes this server offers, in menu order.
pub fn off_modes() -> Vec<OffModeDescriptor> {
    vec![
        OffModeDescriptor {
            id: "clock".into(),
            label: "Clock".into(),
            description: "The time, in the scoreboard's timezone.".into(),
            preview: Some("/offmode/clock.png".into()),
            input: None,
        },
        OffModeDescriptor {
            id: "blank".into(),
            label: "Nothing".into(),
            description: "The panel stays dark.".into(),
            // No preview: an all-black PNG is indistinguishable from a failed
            // load, so the app draws the black panel itself.
            preview: None,
            input: None,
        },
        weather_mode("weather-simple", "Simple", "The temperature, and what it's doing."),
        weather_mode(
            "weather-detailed",
            "Detailed",
            "Temperature, today's high and low, and conditions.",
        ),
        weather_mode("weather-forecast", "Forecast", "Today and the next two days."),
        weather_mode("weather-clock", "Clock + Weather", "The time, with the weather below it."),
    ]
}

/// The four weather screens differ only in layout, so they share a descriptor
/// shape — including the one text input all of them need.
fn weather_mode(id: &str, label: &str, description: &str) -> OffModeDescriptor {
    OffModeDescriptor {
        id: id.into(),
        label: label.into(),
        description: description.into(),
        preview: Some(format!("/offmode/{id}.png")),
        input: Some(OffModeInput {
            key: LOCATION.into(),
            label: "Location".into(),
            // A ZIP is the friendly path; the pair is what keeps this usable
            // outside the US without a second field.
            placeholder: "ZIP or lat,lon".into(),
            max_len: 32,
        }),
    }
}

/// The key every weather screen reads its location from.
pub const LOCATION: &str = "location";

/// Whether a mode leaves the panel dark — the one bit the legacy `OffState`
/// enum could express, and what `BoardConfig::set_off_mode` records.
pub fn draws_nothing(id: &str) -> bool {
    render_off_mode(
        id,
        &OffCtx {
            tz: None,
            params: &BTreeMap::new(),
            preview: true,
            weather: crate::weather::Reading::Ready(crate::weather::Weather::sample()),
        },
    )
    .is_empty()
}

/// True when this server offers `id`. Commands are validated against it: with
/// open string ids, an unvalidated assignment would let a typo park a board on
/// a mode that renders nothing forever.
pub fn offers(id: &str) -> bool {
    off_modes().iter().any(|m| m.id == id)
}

/// The rotation an off mode wants. Empty means a dark panel.
///
/// An unknown id falls back to the CLOCK, never to blank: a black panel is
/// indistinguishable from a broken one, and this is reachable from a config
/// that names a mode a fork removed.
pub fn render_off_mode(id: &str, cx: &OffCtx) -> Vec<OffFrame> {
    match id {
        "blank" => vec![],
        "weather-simple" | "weather-detailed" | "weather-forecast" | "weather-clock" => {
            let w = match &cx.weather {
                crate::weather::Reading::Ready(w) => w.clone(),
                // Say which of the two problems it is. "No location" is
                // something the user can fix in the app; "unavailable" is ours.
                crate::weather::Reading::NoLocation => {
                    return vec![notice("WEATHER", "SET A LOCATION")];
                }
                crate::weather::Reading::Unavailable => {
                    return vec![notice("WEATHER", "UNAVAILABLE")];
                }
            };
            let (fid, px) = match id {
                "weather-simple" => render::render_weather_simple(&w),
                "weather-detailed" => render::render_weather_detailed(&w),
                "weather-forecast" => render::render_weather_forecast(&w),
                _ => {
                    let (h, m) = if cx.preview { PREVIEW_TIME } else { now_hm(cx.tz) };
                    render::render_weather_clock(&w, h, m)
                }
            };
            // A minute, because the clock+weather screen shows one. The others
            // change far more slowly, but one cadence is simpler than four and
            // an unchanged frame re-pushes as the same content hash anyway.
            vec![OffFrame { id: fid, px, dwell_ms: 60_000 }]
        }
        // Add your mode here. Anything that produces a 64x32 RGB565 frame
        // works; `render::render_notice` is the shortest path to text.
        _ => {
            let (hour, minute) = if cx.preview {
                PREVIEW_TIME
            } else {
                now_hm(cx.tz)
            };
            let (id, px) = render::render_clock(hour, minute);
            // A minute is the smallest unit the clock shows, so re-pushing
            // faster would burn frames to redraw identical pixels.
            vec![OffFrame { id, px, dwell_ms: 60_000 }]
        }
    }
}

fn notice(title: &str, detail: &str) -> OffFrame {
    let (id, px) = render::render_notice(
        &[(title, true, render::WHITE_PUB), (detail, false, render::GREY_PUB)],
        None,
    );
    OffFrame { id, px, dwell_ms: 60_000 }
}

/// Board-local wall time: the configured zone, else the server's local zone
/// (which on Railway is UTC — hence per-board timezones).
fn now_hm(tz: Option<chrono_tz::Tz>) -> (u32, u32) {
    use chrono::Timelike;
    match tz {
        Some(tz) => {
            let now = chrono::Utc::now().with_timezone(&tz);
            (now.hour(), now.minute())
        }
        None => {
            let now = chrono::Local::now();
            (now.hour(), now.minute())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> crate::weather::Reading {
        crate::weather::Reading::Ready(crate::weather::Weather::sample())
    }

    /// The app renders these blind, so anything missing here is a blank row on
    /// someone's phone.
    #[test]
    fn every_declared_mode_is_complete_and_renderable() {
        let params = BTreeMap::new();
        for m in off_modes() {
            assert!(!m.id.is_empty(), "id");
            assert!(!m.label.is_empty(), "{}: label", m.id);
            assert!(!m.description.is_empty(), "{}: description", m.id);
            let frames = render_off_mode(&m.id, &OffCtx { tz: None, params: &params, preview: true, weather: sample() });
            // A mode declares a preview if and only if it draws something.
            assert_eq!(
                m.preview.is_some(),
                !frames.is_empty(),
                "{}: preview presence must match whether it renders",
                m.id
            );
            assert!(offers(&m.id));
        }
    }

    /// Reachable whenever a fork removes a mode a board is still parked on.
    #[test]
    fn an_unknown_mode_falls_back_to_the_clock_not_to_darkness() {
        let params = BTreeMap::new();
        let frames = render_off_mode("stocks", &OffCtx { tz: None, params: &params, preview: true, weather: sample() });
        assert_eq!(frames.len(), 1, "an unknown mode must still light the panel");
        let clock = render_off_mode("clock", &OffCtx { tz: None, params: &params, preview: true, weather: sample() });
        assert_eq!(frames[0].id, clock[0].id, "and it falls back to the clock");
    }

    /// The app caches previews for a day; two fetches must match.
    #[test]
    fn previews_are_byte_stable() {
        let params = BTreeMap::new();
        let once = render_off_mode("clock", &OffCtx { tz: None, params: &params, preview: true, weather: sample() });
        let twice = render_off_mode("clock", &OffCtx { tz: None, params: &params, preview: true, weather: sample() });
        assert_eq!(once[0].id, twice[0].id);
        assert_eq!(once[0].px, twice[0].px);
    }

    #[test]
    fn draws_nothing_matches_the_declaration() {
        assert!(draws_nothing("blank"));
        assert!(!draws_nothing("clock"));
    }
}
