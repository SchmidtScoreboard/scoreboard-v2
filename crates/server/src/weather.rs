//! Weather for the off-mode screens: fetch, cache, and the vocabulary the
//! renderers draw from.
//!
//! Two key-free services, so a self-hoster needs no accounts and no secrets:
//! Open-Meteo for the forecast, Zippopotam to turn a US ZIP into coordinates.
//! A location may also be given as `lat,lon` directly, which is the escape
//! hatch for everyone outside the US.
//!
//! Nothing here blocks rendering. A refresh loop keeps a cache warm and the
//! renderers read whatever is in it — weather changes on the order of an hour,
//! so a board is never waiting on a network call to draw a frame.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often a location is re-fetched. Weather moves slowly and both services
/// are free; this is deliberately unhurried.
pub const REFRESH: Duration = Duration::from_secs(10 * 60);

/// How long a reading stays usable after its last successful fetch. Well past
/// `REFRESH`, so a service having a bad hour shows slightly stale weather
/// rather than an error — but not so long that a board sits on yesterday's.
const STALE_AFTER: Duration = Duration::from_secs(3 * 60 * 60);

#[derive(Clone, Debug, PartialEq)]
pub struct Day {
    /// Three-letter day name, already localized to the forecast's timezone.
    pub label: String,
    pub code: u16,
    pub hi: i32,
    pub lo: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Weather {
    pub temp: i32,
    /// WMO weather interpretation code — see `icon_for`.
    pub code: u16,
    /// False after sunset, which selects the moon icons.
    pub is_day: bool,
    pub hi: i32,
    pub lo: i32,
    /// Today first, then the next two days.
    pub days: Vec<Day>,
}

impl Weather {
    /// The fixed reading every preview renders, so the day-cached preview PNGs
    /// are byte-identical between fetches. Matches the mockups.
    pub fn sample() -> Self {
        Self {
            temp: 72,
            code: 2,
            is_day: true,
            hi: 78,
            lo: 61,
            days: vec![
                Day { label: "SUN".into(), code: 0, hi: 78, lo: 61 },
                Day { label: "MON".into(), code: 61, hi: 71, lo: 58 },
                Day { label: "TUE".into(), code: 2, hi: 83, lo: 64 },
            ],
        }
    }
}

/// What a weather screen should draw right now.
pub enum Reading {
    Ready(Weather),
    /// No location typed yet — the screen says so rather than showing a
    /// plausible-looking temperature for nowhere.
    NoLocation,
    /// A location is set but nothing has come back yet, or everything we have
    /// is too old to show.
    Unavailable,
}

#[derive(Default)]
pub struct Cache {
    entries: Mutex<HashMap<String, (Instant, Weather)>>,
}

impl Cache {
    pub fn get(&self, location: &str) -> Reading {
        if location.trim().is_empty() {
            return Reading::NoLocation;
        }
        match self.entries.lock().unwrap().get(location) {
            Some((at, w)) if at.elapsed() < STALE_AFTER => Reading::Ready(w.clone()),
            _ => Reading::Unavailable,
        }
    }

    fn put(&self, location: &str, w: Weather) {
        self.entries.lock().unwrap().insert(location.to_string(), (Instant::now(), w));
    }

    /// Whether this location is due a re-fetch.
    fn due(&self, location: &str) -> bool {
        match self.entries.lock().unwrap().get(location) {
            Some((at, _)) => at.elapsed() >= REFRESH,
            None => true,
        }
    }
}

/// Resolve a location string to coordinates. Accepts `lat,lon` directly, or a
/// US ZIP via Zippopotam — the ZIP is the friendly path, the pair is what makes
/// the feature usable outside the US without a second input field.
async fn coords(location: &str) -> Option<(f64, f64)> {
    let loc = location.trim();
    if let Some((a, b)) = loc.split_once(',') {
        if let (Ok(lat), Ok(lon)) = (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
            if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) {
                return Some((lat, lon));
            }
        }
    }
    let zip: String = loc.chars().filter(|c| c.is_ascii_digit()).collect();
    if zip.len() != 5 {
        return None;
    }
    let url = format!("https://api.zippopotam.us/us/{zip}");
    let v: serde_json::Value = reqwest::get(&url).await.ok()?.json().await.ok()?;
    let p = v.get("places")?.get(0)?;
    Some((
        p.get("latitude")?.as_str()?.parse().ok()?,
        p.get("longitude")?.as_str()?.parse().ok()?,
    ))
}

/// Day-of-week label for an ISO `YYYY-MM-DD`, in the forecast's own timezone.
fn day_label(iso: &str, index: usize) -> String {
    use chrono::Datelike;
    if index == 0 {
        return "TODAY".into();
    }
    match chrono::NaiveDate::parse_from_str(iso, "%Y-%m-%d") {
        Ok(d) => ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"]
            [d.weekday().num_days_from_monday() as usize]
            .to_string(),
        Err(_) => "--".into(),
    }
}

async fn fetch(location: &str) -> Option<Weather> {
    let (lat, lon) = coords(location).await?;
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat:.4}&longitude={lon:.4}\
         &current=temperature_2m,weather_code,is_day\
         &daily=weather_code,temperature_2m_max,temperature_2m_min\
         &temperature_unit=fahrenheit&timezone=auto&forecast_days=3"
    );
    let v: serde_json::Value = reqwest::get(&url).await.ok()?.json().await.ok()?;
    let cur = v.get("current")?;
    let daily = v.get("daily")?;
    let arr = |k: &str| daily.get(k).and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let (codes, maxes, mins, times) = (
        arr("weather_code"),
        arr("temperature_2m_max"),
        arr("temperature_2m_min"),
        arr("time"),
    );
    let days: Vec<Day> = (0..codes.len().min(3))
        .map(|i| Day {
            label: day_label(times.get(i).and_then(|t| t.as_str()).unwrap_or(""), i),
            code: codes[i].as_u64().unwrap_or(0) as u16,
            hi: maxes.get(i).and_then(|x| x.as_f64()).unwrap_or(0.0).round() as i32,
            lo: mins.get(i).and_then(|x| x.as_f64()).unwrap_or(0.0).round() as i32,
        })
        .collect();
    Some(Weather {
        temp: cur.get("temperature_2m")?.as_f64()?.round() as i32,
        code: cur.get("weather_code")?.as_u64().unwrap_or(0) as u16,
        // Missing `is_day` means daytime: showing a sun when we cannot tell is
        // a smaller error than showing a moon at noon.
        is_day: cur.get("is_day").and_then(|x| x.as_u64()).unwrap_or(1) == 1,
        hi: days.first().map(|d| d.hi).unwrap_or(0),
        lo: days.first().map(|d| d.lo).unwrap_or(0),
        days,
    })
}

/// Refresh every location currently in use, skipping ones still fresh.
/// Failures are logged and leave the previous reading in place.
pub async fn refresh_all(cache: &Arc<Cache>, locations: Vec<String>) {
    for loc in locations {
        if loc.trim().is_empty() || !cache.due(&loc) {
            continue;
        }
        match fetch(&loc).await {
            Some(w) => {
                tracing::info!("weather {loc}: {}F, code {}", w.temp, w.code);
                cache.put(&loc, w);
            }
            None => tracing::warn!("weather {loc}: fetch failed; keeping previous reading"),
        }
    }
}

/// Which of the drawn symbols a WMO code maps to. Many codes collapse: the
/// panel cannot show the difference between light and moderate rain, and
/// pretending otherwise would just be a different icon nobody can read.
pub fn icon_for(code: u16, is_day: bool) -> &'static str {
    match code {
        0 => {
            if is_day {
                "clear"
            } else {
                "clear-night"
            }
        }
        1 | 2 => {
            if is_day {
                "partly"
            } else {
                "partly-night"
            }
        }
        3 => "cloudy",
        45 | 48 => "fog",
        51 | 53 | 55 => "drizzle",
        56 | 57 | 66 | 67 => "sleet",
        71 | 73 | 75 | 77 | 85 | 86 => "snow",
        95 | 96 | 99 => "storm",
        _ => "rain",
    }
}

/// Short enough for the 64px panel in the small font — every string here is at
/// most 14 characters, which `condition_labels_fit` pins down.
pub fn condition_for(code: u16) -> &'static str {
    match code {
        0 => "CLEAR",
        1 => "MOSTLY CLEAR",
        2 => "PARTLY CLOUDY",
        3 => "OVERCAST",
        45 | 48 => "FOG",
        51 | 53 => "DRIZZLE",
        55 => "HEAVY DRIZZLE",
        56 | 57 => "FRZ DRIZZLE",
        61 => "LIGHT RAIN",
        63 => "RAIN",
        65 => "HEAVY RAIN",
        66 | 67 => "FREEZING RAIN",
        71 => "LIGHT SNOW",
        73 => "SNOW",
        75 => "HEAVY SNOW",
        77 => "SNOW GRAINS",
        80 | 81 => "SHOWERS",
        82 => "HEAVY SHOWERS",
        85 | 86 => "SNOW SHOWERS",
        95 => "THUNDERSTORM",
        96 | 99 => "THUNDERSTORM",
        _ => "WEATHER",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every WMO group Open-Meteo documents must land on a symbol we actually
    /// draw — an unmapped code would render a blank space where the sky is.
    #[test]
    fn every_wmo_code_maps_to_a_drawn_symbol() {
        const DRAWN: [&str; 11] = [
            "clear", "clear-night", "partly", "partly-night", "cloudy", "fog", "drizzle", "rain",
            "sleet", "snow", "storm",
        ];
        for code in 0u16..=100 {
            for is_day in [true, false] {
                let icon = icon_for(code, is_day);
                assert!(DRAWN.contains(&icon), "code {code} -> unknown symbol {icon:?}");
            }
        }
    }

    #[test]
    fn night_only_changes_the_two_symbols_that_have_a_night_form() {
        assert_eq!(icon_for(0, false), "clear-night");
        assert_eq!(icon_for(2, false), "partly-night");
        // Rain looks the same at midnight.
        assert_eq!(icon_for(63, false), icon_for(63, true));
    }

    /// The condition line is drawn in the small font across a 64px panel.
    #[test]
    fn condition_labels_fit_the_panel() {
        for code in 0u16..=100 {
            let s = condition_for(code);
            assert!(s.len() <= 14, "{code} -> {s:?} is {} chars", s.len());
            assert!(s.chars().all(|c| c.is_ascii_uppercase() || c == ' '), "{s:?}");
        }
    }

    #[test]
    fn a_lat_lon_pair_is_accepted_so_the_feature_is_not_us_only() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert_eq!(rt.block_on(coords("41.8858,-87.6229")), Some((41.8858, -87.6229)));
        assert_eq!(rt.block_on(coords(" 51.5,-0.12 ")), Some((51.5, -0.12)));
        // Out of range, and plainly not a location, are both rejected.
        assert_eq!(rt.block_on(coords("999,999")), None);
        assert_eq!(rt.block_on(coords("chicago")), None);
    }

    #[test]
    fn an_empty_location_reads_as_no_location_not_as_an_error() {
        let c = Cache::default();
        assert!(matches!(c.get(""), Reading::NoLocation));
        assert!(matches!(c.get("   "), Reading::NoLocation));
        assert!(matches!(c.get("60601"), Reading::Unavailable));
    }

    #[test]
    fn today_is_labelled_today_and_the_rest_by_weekday() {
        assert_eq!(day_label("2026-08-04", 0), "TODAY");
        assert_eq!(day_label("2026-08-05", 1), "WED");
        assert_eq!(day_label("2026-08-06", 2), "THU");
    }
}
