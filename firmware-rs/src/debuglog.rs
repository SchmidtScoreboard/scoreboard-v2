//! The device console, mirrored into a small in-memory ring so it can leave over
//! the socket instead of only over USB.
//!
//! Why this exists: the interesting failures happen on a board sitting on a
//! shelf in someone's living room, and the only way to read one was to carry it
//! to a laptop and plug in a cable — which reboots it, and so destroys the state
//! you were trying to look at.
//!
//! # Two levels, and why
//!
//! * [`dwarn!`] is ALWAYS buffered, whether or not anyone is listening. Faults
//!   are not repeatable on demand; by the time you think to turn logging on, the
//!   interesting line has already scrolled past. So the board keeps a running
//!   backlog of the bad news and hands it over the moment someone asks.
//! * [`dlog!`] is the chatty level and is only buffered while streaming is on.
//!   It formats nothing when it is off, which matters because it sits on
//!   per-frame paths.
//!
//! When the ring is full it evicts the oldest CHATTY line first, so a flood of
//! routine output can never push the warnings out from under itself.
//!
//! # What must never go in here
//!
//! `println!` is UART-only; `dlog!`/`dwarn!` leave the building. That difference
//! is the access-control boundary, so secrets stay on `println!`: the Wi-Fi SSID
//! and — especially — the pairing code, which is all anyone needs to attach a
//! phone to the board. There is no redaction pass to forget to run; a line is
//! remote-readable only if someone typed the remote-readable macro.
//!
//! # Expiry
//!
//! Streaming carries a deadline and switches itself off. Left on indefinitely it
//! would cost the board work nobody is reading and keep a window open on the
//! server for no reason.

use alloc::collections::VecDeque;
use alloc::string::String;
use core::cell::RefCell;
use critical_section::Mutex;

/// Total lines held. These are short tagged lines; a hundred covers the seconds
/// around a stall at the rate the firmware prints.
const LINES: usize = 96;
/// Longest line kept. Longer ones are truncated rather than dropped — the front
/// of a line carries the tag and the numbers, which is the part worth having.
const LINE_CAP: usize = 160;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    /// Buffered always.
    Warn,
    /// Buffered only while streaming.
    Info,
}

struct Ring {
    lines: VecDeque<(Level, String)>,
    /// Lines dropped because the ring was full and nobody had drained it.
    dropped: u32,
    /// Whether the server has asked for the chatty level. Off at boot, always.
    on: bool,
    /// Device-clock millis when streaming switches itself off; `None` = not on.
    until_ms: Option<u64>,
}

static RING: Mutex<RefCell<Ring>> = Mutex::new(RefCell::new(Ring {
    lines: VecDeque::new(),
    dropped: 0,
    on: false,
    until_ms: None,
}));

/// Start streaming for `secs`, or stop.
///
/// Does NOT clear on the way on: the whole point of the always-buffered warning
/// level is that the backlog is waiting when you ask. Turning it OFF does clear,
/// so the next session starts clean and nothing lingers unread.
pub fn set_enabled(on: bool, secs: u32, now_ms: u64) {
    critical_section::with(|cs| {
        let mut r = RING.borrow_ref_mut(cs);
        r.on = on;
        if on {
            r.until_ms = Some(now_ms + u64::from(secs) * 1000);
        } else {
            r.until_ms = None;
            r.lines.clear();
            r.dropped = 0;
        }
    });
}

/// Switch streaming off once its window has passed. Returns true on the tick
/// that expires it, so the caller can say so out loud exactly once.
pub fn expire_if_due(now_ms: u64) -> bool {
    critical_section::with(|cs| {
        let mut r = RING.borrow_ref_mut(cs);
        match r.until_ms {
            Some(deadline) if now_ms >= deadline => {
                r.on = false;
                r.until_ms = None;
                r.lines.clear();
                r.dropped = 0;
                true
            }
            _ => false,
        }
    })
}

pub fn enabled() -> bool {
    critical_section::with(|cs| RING.borrow_ref(cs).on)
}

fn push_level(level: Level, mut line: String) {
    critical_section::with(|cs| {
        let mut r = RING.borrow_ref_mut(cs);
        if level == Level::Info && !r.on {
            return;
        }
        if line.len() > LINE_CAP {
            line.truncate(LINE_CAP);
        }
        if r.lines.len() >= LINES {
            // Evict the oldest CHATTY line, so routine output can never crowd
            // out the warnings that are the reason this buffer exists.
            let victim = r.lines.iter().position(|(l, _)| *l == Level::Info).unwrap_or(0);
            let _ = r.lines.remove(victim);
            r.dropped = r.dropped.saturating_add(1);
        }
        r.lines.push_back((level, line));
    });
}

/// Chatty level. Only reaches the ring while streaming.
pub fn push(line: String) {
    push_level(Level::Info, line);
}

/// Always buffered, streaming or not.
pub fn push_warn(line: String) {
    push_level(Level::Warn, line);
}

/// Everything buffered, oldest first, plus how many lines were lost since the
/// last drain. Warnings are prefixed so the level survives the trip.
pub fn drain() -> (alloc::vec::Vec<String>, u32) {
    critical_section::with(|cs| {
        let mut r = RING.borrow_ref_mut(cs);
        let lines = r
            .lines
            .drain(..)
            .map(|(level, l)| match level {
                Level::Warn => {
                    let mut s = String::from("! ");
                    s.push_str(&l);
                    s
                }
                Level::Info => l,
            })
            .collect();
        (lines, core::mem::replace(&mut r.dropped, 0))
    })
}

/// Is there anything worth sending? Cheap enough for the ws loop to ask often.
pub fn pending() -> bool {
    critical_section::with(|cs| !RING.borrow_ref(cs).lines.is_empty())
}

/// `println!` that is also readable remotely while streaming is on.
///
/// The `enabled()` check comes first so a board nobody is debugging never
/// formats a string it will not keep — this sits on per-frame paths.
#[macro_export]
macro_rules! dlog {
    ($($arg:tt)*) => {{
        esp_println::println!($($arg)*);
        if $crate::debuglog::enabled() {
            $crate::debuglog::push(alloc::format!($($arg)*));
        }
    }};
}

/// `println!` for bad news: buffered ALWAYS, so the backlog is already waiting
/// when someone turns streaming on after the fact.
#[macro_export]
macro_rules! dwarn {
    ($($arg:tt)*) => {{
        esp_println::println!($($arg)*);
        $crate::debuglog::push_warn(alloc::format!($($arg)*));
    }};
}
