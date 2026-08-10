//! The device console, mirrored into a small in-memory ring so it can leave over
//! the socket instead of only over USB.
//!
//! Why this exists: the interesting failures happen on a board sitting on a
//! shelf in someone's living room, and the only way to read one was to carry it
//! to a laptop and plug in a cable — which reboots it, and so destroys the state
//! you were trying to look at. `dlog!` prints exactly like `println!` and
//! additionally keeps the line here; the ws task drains it to the server while
//! debug logging is on.
//!
//! Deliberately bounded and lossy. A ring that blocks, grows without limit, or
//! backs up behind a dead socket would turn a diagnostic into an outage — so the
//! oldest line goes when it is full, and the loss is COUNTED, because a log with
//! a silent hole in it is worse than one that admits the hole.

use alloc::collections::VecDeque;
use alloc::string::String;
use core::cell::RefCell;
use critical_section::Mutex;

/// How many lines the ring holds. These are short tagged lines ("[ws] …"); a
/// few dozen covers the seconds around a stall, which is what anyone is ever
/// looking at.
const LINES: usize = 64;
/// Longest line kept. Longer ones are truncated rather than dropped — the front
/// of a line carries the tag and the numbers, which is the part worth having.
const LINE_CAP: usize = 160;

struct Ring {
    lines: VecDeque<String>,
    /// Lines dropped because the ring was full and nobody had drained it.
    dropped: u32,
    /// Whether the server has asked for these. Off at boot, always.
    on: bool,
}

static RING: Mutex<RefCell<Ring>> = Mutex::new(RefCell::new(Ring {
    lines: VecDeque::new(),
    dropped: 0,
    on: false,
}));

/// Turn streaming on or off. Clearing on the way ON means a session starts at
/// the moment you asked rather than with whatever the last one left behind.
pub fn set_enabled(on: bool) {
    critical_section::with(|cs| {
        let mut r = RING.borrow_ref_mut(cs);
        if on && !r.on {
            r.lines.clear();
            r.dropped = 0;
        }
        r.on = on;
    });
}

pub fn enabled() -> bool {
    critical_section::with(|cs| RING.borrow_ref(cs).on)
}

/// Record one line. Only ever called when `enabled()` (see `dlog!`), so a board
/// nobody is debugging never formats a string it will not keep.
pub fn push(line: String) {
    critical_section::with(|cs| {
        let mut r = RING.borrow_ref_mut(cs);
        if !r.on {
            return;
        }
        let mut line = line;
        if line.len() > LINE_CAP {
            line.truncate(LINE_CAP);
        }
        if r.lines.len() >= LINES {
            let _ = r.lines.pop_front();
            r.dropped = r.dropped.saturating_add(1);
        }
        r.lines.push_back(line);
    });
}

/// Everything buffered, oldest first, plus how many lines were lost since the
/// last drain. Empty when there is nothing to send.
pub fn drain() -> (alloc::vec::Vec<String>, u32) {
    critical_section::with(|cs| {
        let mut r = RING.borrow_ref_mut(cs);
        let lines: alloc::vec::Vec<String> = r.lines.drain(..).collect();
        (lines, core::mem::replace(&mut r.dropped, 0))
    })
}

/// `println!` that is also readable remotely.
///
/// The `enabled()` check comes first so the formatting cost only exists while
/// someone is actually watching — this macro replaces `println!` on hot paths
/// (every frame, every tick), where an unconditional `format!` would be its own
/// performance bug.
#[macro_export]
macro_rules! dlog {
    ($($arg:tt)*) => {{
        esp_println::println!($($arg)*);
        if $crate::debuglog::enabled() {
            $crate::debuglog::push(alloc::format!($($arg)*));
        }
    }};
}
