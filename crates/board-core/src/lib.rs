//! Sans-IO board (device) logic, shared by the simulator and the ESP32 firmware.
//! Owns: the websocket-driven frame cache + local rotation (Running mode), the
//! device-mode state machine (boot / run / sleep / setup / sync), encoder-button
//! gesture detection, and firmware-local screens (boot / setup / sync) drawn with
//! embedded-graphics. It performs no I/O — the host shuttles bytes and input.
//!
//! `no_std + alloc` so the ESP32 firmware can use it directly. Time is supplied by
//! the host as monotonic milliseconds (`Instant` doesn't exist on bare metal).

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use embedded_graphics::{
    draw_target::DrawTarget,
    geometry::{OriginDimensions, Size},
    mono_font::{
        ascii::{FONT_4X6, FONT_5X8, FONT_6X10},
        MonoFont, MonoTextStyle,
    },
    pixelcolor::Rgb888,
    prelude::*,
    primitives::{Line, PrimitiveStyle, Rectangle},
    text::{Alignment, Baseline, Text, TextStyleBuilder},
};
use scoreboard_protocol::{
    rgb565_to_888, ClientMsg, FrameId, Manifest, FRAME_HEIGHT, FRAME_PIXELS, FRAME_WIDTH,
};

const DOUBLE_WINDOW_MS: u64 = 400;
const BOOT_TIME_MS: u64 = 1500;
// One button. Taps: single = sleep/wake (or menu select); double = settings
// menu TOGGLE (open when closed, close when open). The HOLD is erase-only,
// timed from the moment the press starts: the first HOLD_GRACE_MS are ignored
// so taps and double-taps never flash the bar, then an amber bar fills toward
// MENU_CLOSE_HOLD_MS — the instant where a press that began INSIDE the menu
// closes it (outside the menu that crossing is just the warm-up ending, so
// every press runs the same timeline) — and past that the screen turns red:
// KEEP HOLDING TO ERASE, with the raw factory wipe at ERASE_HOLD_MS. 10s is
// the original pre-menu erase duration; the 15s variant existed only to make
// room for the hold's 3s open-the-menu stop, which double-tap replaced. The
// wipe is the escape hatch that must survive any bug in the menu logic — it
// lives in `tick`, below the menu state entirely, and wipes Wi-Fi
// credentials exactly as it always has.
pub const HOLD_GRACE_MS: u64 = 500;
pub const MENU_CLOSE_HOLD_MS: u64 = 2_000;
pub const ERASE_HOLD_MS: u64 = 10_000;
// Nobody stands at the board for two minutes without touching the knob: the
// menu closes itself (device clock) so an interrupted visit can't park the
// panel on a settings list instead of scores.
pub const MENU_IDLE_TIMEOUT_MS: u64 = 120_000;
// Sleep/wake fade: going dark (local tap-sleep, or a manifest with
// display_on: false — which the server only ships as the Blank off_state) and
// lighting back up ramp the panel's EFFECTIVE brightness across this window
// instead of snapping. Device clock, like every animation on this board; the
// stored user level never moves. Deliberately NO fade for the OTA blank, the
// reset overlay, or menu open/close — those must be instant.
pub const SLEEP_FADE_MS: u64 = 400;
// Offline grace: a dropped websocket usually re-establishes within a second or
// two — and a server DEPLOY takes 30-60s of reconnect failures. While there's
// content on screen, keep showing it (scores just pause updating); only admit
// the outage with the connect-progress screen once it has lasted this long.
// Field-tuned up from 5s: every deploy and proxy blip was flashing CONNECTING
// at people, which reads as "broken", not "briefly offline".
const OFFLINE_GRACE_MS: u64 = 45_000;
// The pair code is only useful while someone is standing at the board typing it
// into the app. An accidental double press would otherwise park the panel on it
// forever (the code screen shows no scores), so it returns to content on its own.
const SYNC_TIMEOUT_MS: u64 = 90_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Booting,
    Running,
    Asleep,
    Setup,
    Syncing,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Gesture {
    Single,
    Double,
    Long,
}

/// Where BLE provisioning stands, as reported by the firmware — the setup
/// screen names what it's waiting on instead of a one-size-fits-all "SETUP".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SetupPhase {
    WaitingBluetooth,
    WaitingCreds,
    Saved,
}

/// Host-reported facts for the DEBUG INFO diagnostics screen. Everything is
/// optional: the screen matters MOST when the board is offline and half-broken,
/// so each row renders "--" for what the host can't know rather than hiding.
/// (Uptime, online state, and firmware version come from the board itself.)
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Diagnostics {
    pub ssid: Option<String>,
    pub rssi_dbm: Option<i8>,
    pub ip: Option<[u8; 4]>,
    /// The server endpoint as "host:port" — what the board is dialing.
    pub server: Option<String>,
    /// A short prefix of the board id (the full pubkey never fits on 64px).
    pub board_id_prefix: Option<String>,
    pub free_heap_bytes: Option<u32>,
}

/// Which settings-menu screen is up. The menu is an OVERLAY, not a `Mode`:
/// the mode machine keeps running untouched underneath, which is what makes
/// "closing restores exactly the prior state" true by construction — an asleep
/// board is still asleep, a connect screen is still connecting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MenuScreen {
    Root,
    PairCode,
    DebugInfo,
    ResetConfirm,
    UpdateConfirm,
}

struct MenuState {
    screen: MenuScreen,
    selected: usize,      // root row (0..MENU_ITEM_COUNT)
    scroll: usize,        // debug-info top line (the root's five rows all fit)
    confirm_selected: bool, // confirm screens (reset/update): CANCEL is the default
    last_interaction: u64, // any knob activity; 120s of silence closes the menu
}

// Root menu rows, in owner-specified order. EXIT is first so the common case
// (peeked at the menu, done now) is the default selection plus one tap.
const MENU_EXIT: usize = 0;
const MENU_UPDATE: usize = 1;
const MENU_PAIR: usize = 2;
const MENU_DEBUG: usize = 3;
const MENU_RESET: usize = 4;
const MENU_ITEM_COUNT: usize = 5;
// The debug-info screen shows this many FONT_4X6 rows at once (5 × 6px + 1px
// top margin fills the 32px panel); the rest scroll.
const DEBUG_INFO_VISIBLE: usize = 5;
// Menu selection fills: deep blue for the ordinary selection, the deep-red
// member of the danger family for ERASE. Row text stays WHITE on top of them.
// Both values are picked for brightness 10, the worst case: the fill is still
// visibly colored (blue scales to (3,6,18), red to (18,3,1)) while the white
// text, at (25,25,25), stays the brightest thing on the row — verified
// numerically in `menu_selection_stays_legible_at_min_brightness` and by the
// `menu_root_dim10` screen dump.
const MENU_SELECT_BLUE: Rgb888 = Rgb888::new(0x22, 0x44, 0xBB);
const MENU_SELECT_RED: Rgb888 = Rgb888::new(0xBB, 0x22, 0x11);

pub struct Board {
    cache: BTreeMap<FrameId, Vec<u8>>,
    manifest: Option<Manifest>,
    last_good: Option<FrameId>, // last frame actually shown (smooth manifest swaps)
    idx: usize,
    last_advance: u64,

    mode: Mode,
    started: Option<u64>, // first-tick baseline for boot timing
    clock: u64,           // last tick time (ms), used when other inputs need "now"
    anim: u32,
    setup_code: String,

    // encoder-button gesture detection (all times in ms)
    pressed_at: Option<u64>,
    pending_single: Option<u64>,
    prev_release: Option<u64>,
    local_brightness: u8, // encoder-controlled, 10..=100
    online: bool,         // is the websocket currently connected
    offline_since: Option<u64>, // last loss of CONTACT (starts the offline grace window)
    wifi_up: bool,        // host-reported Wi-Fi link/DHCP state (connect screen)
    setup_phase: SetupPhase, // which BLE-provisioning stage the setup screen shows
    brightness_changed_at: Option<u64>, // transient brightness-bar overlay window
    brightness_pending: bool, // a turn the server hasn't heard yet (see `encoder`)
    syncing_since: Option<u64>, // when the pair-code screen went up (it times out)
    /// Countdown-bar anchor: (epoch, started_at). Set when a manifest carrying
    /// `countdown_ms` arrives with a NEW epoch; same-epoch re-pushes (a
    /// brightness change mid-notice) keep the anchor so the bar never restarts.
    countdown_anchor: Option<(u64, u64)>,
    /// Server-pushed "nobody is paired with you": keeps the pair-code screen
    /// up without the usual timeout until SetupDone or a user gesture — on a
    /// first startup the code IS the most useful thing the panel can show.
    sticky_pairing: bool,

    // the one-button erase hold (amber warm-up, red run, raw wipe at 10s)
    reset_active: bool,         // hold bar on screen (held past the grace window)
    reset_progress: u8,         // 0..=100 fill of the CURRENT phase's bar
    hold_erase_phase: bool,     // past 2s: the bar is red and runs toward the wipe
    hold_started_in_menu: bool, // menu-origin presses close the menu at the 2s mark
    hold_fired: bool,           // this press's menu-close already ran
    reset_done: bool,           // erase complete — firmware wipes creds + reboots
    reset_done_at: Option<u64>, // for the simulator to auto-dismiss (device reboots first)

    // Sleep/wake fade: was the panel deliberately dark at the last tick, and
    // when did that intent last flip (the ramp's zero point). A reversal
    // mid-fade restarts the ramp from the new flip — a sub-400ms double
    // toggle shows a small level jump, accepted for the state's simplicity.
    dark_intent: bool,
    fade_flip_at: Option<u64>,

    // settings menu overlay (None = whatever the mode machine was doing)
    menu: Option<MenuState>,
    // Newer firmware the server says exists (`ServerMsg::UpdateAvailable`);
    // shown on the UPDATE row, and what makes selecting it send RequestUpdate.
    update_available: Option<String>,
    diag: Diagnostics, // host-fed rows for the DEBUG INFO screen

    // OTA update in progress: Some(_) BLANKS the panel to black. Each flash
    // sector write stalls the flash-resident render task, darkening whatever's
    // lit — so any lit content strobes for the whole download. A black panel
    // has nothing to strobe. Progress is shown in the app now (server-observed),
    // so the stored value is vestigial: any Some renders identically black.
    updating: Option<u8>,
    // Firmware version shown on the boot screen (env!("CARGO_PKG_VERSION")).
    version: Option<String>,

    /// Messages for the host to send to the server (drained via `take_outgoing`).
    outbox: Vec<ClientMsg>,
}

impl Default for Board {
    fn default() -> Self {
        Self::new()
    }
}

impl Board {
    pub fn new() -> Self {
        Self {
            cache: BTreeMap::new(),
            manifest: None,
            last_good: None,
            idx: 0,
            last_advance: 0,
            mode: Mode::Booting,
            started: None,
            clock: 0,
            anim: 0,
            setup_code: String::from("000000"),
            pressed_at: None,
            pending_single: None,
            prev_release: None,
            local_brightness: 100,
            online: false,
            offline_since: None,
            wifi_up: false,
            setup_phase: SetupPhase::WaitingBluetooth,
            brightness_changed_at: None,
            brightness_pending: false,
            syncing_since: None,
            countdown_anchor: None,
            sticky_pairing: false,
            reset_active: false,
            reset_progress: 0,
            hold_erase_phase: false,
            hold_started_in_menu: false,
            hold_fired: false,
            reset_done: false,
            reset_done_at: None,
            dark_intent: false,
            fade_flip_at: None,
            menu: None,
            update_available: None,
            diag: Diagnostics::default(),
            updating: None,
            version: None,
            outbox: Vec::new(),
        }
    }

    /// Drain messages the board wants sent to the server (e.g. a button press).
    /// The host calls this after `tick` / input events and ships each over the WS.
    pub fn take_outgoing(&mut self) -> Vec<ClientMsg> {
        core::mem::take(&mut self.outbox)
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn brightness(&self) -> u8 {
        self.local_brightness
    }

    /// Display power from the latest manifest (what `SetPower` toggles).
    pub fn power(&self) -> bool {
        self.manifest.as_ref().map_or(true, |m| m.display_on)
    }

    /// Restore a persisted brightness on boot (clamped to the encoder range).
    pub fn set_brightness(&mut self, level: u8) {
        self.local_brightness = level.clamp(10, 100);
    }

    /// OTA update in flight. `Some(_)` BLANKS the panel to all-black, overriding
    /// every mode except hold-to-reset. The flash writes that stream the new
    /// image stall the render task sector-by-sector, so any LIT content would
    /// strobe for the whole download; black has nothing to strobe. Download
    /// progress is shown in the app now (server-observed), so the value passed
    /// is ignored for rendering — call `set_updating(Some(0))` once at the
    /// start. `None` restores the normal mode display (the failed-download
    /// case: the update must be a non-event).
    pub fn set_updating(&mut self, progress: Option<u8>) {
        self.updating = progress.map(|p| p.min(100));
    }

    /// Firmware version line for the boot screen (env!("CARGO_PKG_VERSION")).
    pub fn set_version(&mut self, v: String) {
        self.version = Some(v);
    }

    /// True while the hold-to-reset bar is on screen (held past the grace window).
    pub fn reset_active(&self) -> bool {
        self.reset_active
    }

    /// Newer firmware the server advertised (`ServerMsg::UpdateAvailable`).
    /// `Some(version)` puts an up-arrow + that version on the UPDATE row and
    /// arms it; `None` shows the running version and selecting is a no-op.
    pub fn set_update_available(&mut self, v: Option<String>) {
        self.update_available = v;
    }

    /// Host-reported diagnostics for the DEBUG INFO screen. Feed whenever the
    /// facts change (link up/down, DHCP, RSSI poll) — cheap either way.
    pub fn set_diagnostics(&mut self, d: Diagnostics) {
        self.diag = d;
    }

    /// Is the settings-menu overlay up (any of its screens)?
    pub fn menu_open(&self) -> bool {
        self.menu.is_some()
    }

    /// Should the host be announcing a pairing code to the server? True for
    /// the legacy Setup/Syncing modes AND the menu's PAIR CODE screen — the
    /// host used to infer this from `mode()`, which can't see the overlay.
    pub fn pairing_active(&self) -> bool {
        matches!(self.mode, Mode::Setup | Mode::Syncing)
            || self.menu.as_ref().is_some_and(|m| m.screen == MenuScreen::PairCode)
    }

    /// True once the hold-to-reset bar has filled — the firmware should wipe the
    /// stored Wi-Fi credentials and reboot into BLE provisioning.
    pub fn reset_triggered(&self) -> bool {
        self.reset_done
    }

    /// Host reports websocket connectivity; drives the reconnecting screen.
    /// The offline grace window (see `framebuffer`) is stamped here but only
    /// CLEARED by `on_manifest`: a socket is not freshness. A link that
    /// completes the handshake and dies before delivering anything would
    /// otherwise restart the window on every retry, holding an hour-old score
    /// on the panel as if it were live. Stamping only when the window isn't
    /// already running keeps a host that re-reports "offline" every poll (or
    /// flaps the socket) from extending it either.
    pub fn set_online(&mut self, online: bool) {
        if !online && self.offline_since.is_none() {
            self.offline_since = Some(self.clock);
        }
        self.online = online;
    }

    /// Host-reported Wi-Fi link/DHCP state; drives the connect-progress screen
    /// (which leg — Wi-Fi or server — is still coming up). Default false.
    pub fn set_wifi(&mut self, up: bool) {
        self.wifi_up = up;
    }

    /// Rendering hint for `Mode::Setup` (BLE provisioning). Default WaitingBluetooth.
    pub fn set_setup_phase(&mut self, p: SetupPhase) {
        self.setup_phase = p;
    }

    pub fn set_setup_code(&mut self, code: String) {
        self.setup_code = code;
    }

    pub fn setup_code(&self) -> String {
        self.setup_code.clone()
    }

    /// The board has been paired; leave the setup / sync screen. When the code
    /// was up via the menu's PAIR CODE screen, "leave" means back to the menu
    /// root — the person pairing is standing at the board, mid-menu-visit.
    pub fn setup_done(&mut self) {
        self.sticky_pairing = false;
        if matches!(self.mode, Mode::Setup | Mode::Syncing) {
            self.syncing_since = None;
            self.mode = Mode::Running;
        }
        if let Some(m) = &mut self.menu {
            if m.screen == MenuScreen::PairCode {
                m.screen = MenuScreen::Root;
                m.last_interaction = self.clock;
            }
        }
    }

    /// Server says nobody is paired with this board (a first startup after
    /// provisioning, or every controller was revoked): put the pair code up
    /// and KEEP it up — no timeout — until `setup_done` or a user gesture.
    /// The code on the panel is the whole onboarding path; making a brand-new
    /// owner discover double-tap -> PAIR CODE on their first boot was a wall.
    pub fn needs_pairing(&mut self) {
        // Never fight the user: an open menu or an active setup screen wins.
        if self.menu.is_some() || self.mode == Mode::Setup {
            return;
        }
        if self.mode == Mode::Running || self.mode == Mode::Booting {
            self.mode = Mode::Syncing;
            self.syncing_since = Some(self.clock);
            self.sticky_pairing = true;
        }
    }

    /// Trigger a gesture directly (used by the simulator control panel).
    pub fn gesture(&mut self, g: Gesture) {
        self.apply(g);
    }

    /// Encoder rotation -> brightness (mirrors the real device's encoder).
    /// While online the new level is reported to the server, which adopts it as
    /// the board's one brightness and echoes it back in the next manifest — the
    /// knob and the app's slider move the same value. Offline the dim applies
    /// locally at once and is REPORTED once the server can hear it (see
    /// `tick`), otherwise the first manifest after the reconnect would echo the
    /// server's stale level and undo the turn in front of the user.
    pub fn encoder(&mut self, delta: i32) {
        // With the menu up the knob NAVIGATES — brightness must not creep
        // while someone scrolls the wifi-info screen (and no DeviceBrightness
        // chatter goes out either; the server would adopt an accidental level).
        if self.menu.is_some() {
            self.menu_rotate(delta);
            return;
        }
        self.local_brightness = (self.local_brightness as i32 + delta * 10).clamp(10, 100) as u8;
        self.brightness_changed_at = Some(self.clock);
        if self.online && matches!(self.mode, Mode::Running) {
            self.outbox.push(ClientMsg::DeviceBrightness { level: self.local_brightness });
        } else {
            // A flag, not one queued message per click: the firmware's outbox
            // is 4 deep and drops on overflow, so ten clicks of queued
            // messages would lose the newest and leave the server on a level
            // the knob passed through. One message, the final level, on flush.
            self.brightness_pending = true;
        }
    }

    // ---- server content (Running mode) ----
    pub fn on_manifest(&mut self, m: Manifest) -> Option<ClientMsg> {
        let missing: Vec<FrameId> = m
            .rotation
            .iter()
            .map(|r| r.frame_id.clone())
            .filter(|id| !self.cache.contains_key(id))
            .collect();
        // Content is the only proof the server is still there, so freshness is
        // stamped HERE — a socket that reconnects and delivers nothing leaves
        // the grace window running (see `set_online`).
        self.offline_since = None;
        if self.mode == Mode::Booting && !m.rotation.is_empty() {
            self.mode = Mode::Running;
        }
        // A manifest is the server reasserting ownership of the panel: its
        // `display_on` decides darkness from here on. Without this, a sleep
        // toggled locally while offline survives the reconnect forever — the
        // panel stays black while the server (and the app's power toggle) both
        // read "on", and only a second physical press gets the board back.
        if self.mode == Mode::Asleep {
            self.mode = Mode::Running;
        }
        // The server owns brightness; adopt its value unless the knob was just
        // turned (the transient bar is still up) or a turn it hasn't heard yet
        // is waiting to be reported — a manifest racing either would
        // rubber-band the knob back to the level the user just left.
        if self.brightness_changed_at.is_none() && !self.brightness_pending {
            self.local_brightness = m.brightness.clamp(10, 100);
        }
        // A same-length rotation keeps the current slot and dwell timer —
        // content-addressed ids change on every score/clock tick, so resetting
        // here would restart at game 1 on every server poll and starve the
        // tail. Length is a heuristic: an equal-length lineup SWAP also keeps
        // position (the swapped-in game gets a partial first dwell) — accepted,
        // since frames carry no game identity to tell the two apart. A
        // different length is a genuinely new rotation: start from the top —
        // and so is a different EPOCH, which is how the server marks a program
        // change the length can't reveal (a 4-step notice sweep arriving over a
        // 4-game rotation must start at step one, not mid-bar).
        let same_shape = self.manifest.as_ref().is_some_and(|old| {
            !m.rotation.is_empty()
                && old.rotation.len() == m.rotation.len()
                && old.epoch == m.epoch
        });
        // The countdown bar anchors at the first manifest of an epoch: the
        // fill position derives from device time since THIS moment, so a
        // same-epoch re-push (brightness nudge mid-notice) can't restart it,
        // and a new notice (new epoch) always starts empty.
        match (m.countdown_ms, &self.countdown_anchor) {
            (Some(_), Some((e, _))) if *e == m.epoch => {}
            (Some(_), _) => self.countdown_anchor = Some((m.epoch, self.clock)),
            (None, _) => self.countdown_anchor = None,
        }
        self.manifest = Some(m);
        if !same_shape {
            self.idx = 0;
            self.last_advance = self.clock;
        }
        // Evict frames no longer referenced so the cache stays bounded on-device
        // (keep the current rotation + the last shown frame for a smooth swap).
        let keep: Vec<FrameId> = {
            let m = self.manifest.as_ref().unwrap();
            let mut k: Vec<FrameId> = m.rotation.iter().map(|r| r.frame_id.clone()).collect();
            if let Some(lg) = &self.last_good {
                k.push(lg.clone());
            }
            k
        };
        self.cache.retain(|id, _| keep.contains(id));
        if missing.is_empty() {
            None
        } else {
            Some(ClientMsg::NeedFrames { ids: missing })
        }
    }

    pub fn on_frame(&mut self, id: FrameId, rgb565_le: Vec<u8>) {
        self.cache.insert(id, rgb565_le);
    }

    // ---- raw encoder button from the host (seesaw / sim control) ----
    pub fn button_down(&mut self, now_ms: u64) {
        // Button events carry fresher time than the last tick — adopt it, or
        // anything stamped from `self.clock` in the gesture path (a menu
        // opened by this press, say) is timestamped in the past and can be
        // idle-closed by the very next tick. Invisible at the firmware's
        // 30fps tick cadence; board-core must not depend on that cadence.
        self.clock = self.clock.max(now_ms);
        self.pressed_at = Some(now_ms);
        // Captured at press time: only a hold that BEGINS inside the menu
        // closes it at the 2s mark (and labels the amber bar CLOSING MENU).
        // Deciding at fire time instead would let a press that outlives a
        // menu timeout close a menu the user opened mid-hold.
        self.hold_started_in_menu = self.menu.is_some();
        self.hold_fired = false;
        if let Some(m) = &mut self.menu {
            m.last_interaction = now_ms;
        }
        if !self.reset_done {
            self.reset_active = false;
            self.reset_progress = 0;
            self.hold_erase_phase = false;
        }
    }

    pub fn button_up(&mut self, now_ms: u64) {
        self.clock = self.clock.max(now_ms); // same freshness rule as button_down
        let Some(p) = self.pressed_at.take() else { return };
        if let Some(m) = &mut self.menu {
            m.last_interaction = now_ms;
        }
        // A hold past the grace window was a hold, not a tap: clear the bar and
        // leave whatever already fired (a menu-origin close) in place.
        // Releasing anywhere in the red phase keeps everything un-erased — the
        // wipe only ever fires from `tick`, at the full 10s.
        if now_ms.saturating_sub(p) >= HOLD_GRACE_MS {
            if !self.reset_done {
                self.reset_active = false;
                self.reset_progress = 0;
                self.hold_erase_phase = false;
            }
            return;
        }
        if let Some(prev) = self.prev_release.take() {
            if now_ms.saturating_sub(prev) <= DOUBLE_WINDOW_MS {
                self.pending_single = None;
                self.apply(Gesture::Double);
                return;
            }
        }
        self.prev_release = Some(now_ms);
        self.pending_single = Some(now_ms);
    }

    pub fn tick(&mut self, now_ms: u64) {
        self.clock = now_ms;
        // The brightness bar shows for ~2s after the last encoder turn.
        if let Some(t) = self.brightness_changed_at {
            if now_ms.saturating_sub(t) > 2000 {
                self.brightness_changed_at = None;
            }
        }
        if self.started.is_none() {
            self.started = Some(now_ms);
            self.last_advance = now_ms;
        }
        // Animation phase derives from WALL TIME, not the tick count, so spinner
        // speed is identical whether the host ticks at 30fps (firmware) or
        // redraws as fast as it can (simulator). 133ms/step = the cadence the
        // firmware's 30fps loop produced with the old per-tick counter.
        self.anim = (now_ms / 133) as u32;

        // The erase hold. One press, one timeline, wherever it starts: amber
        // warm-up bar to the 2s mark (where a press that BEGAN inside the
        // menu closes it), then the RED phase running to the raw wipe at 10s.
        // This block is deliberately independent of the menu's own logic — it
        // reads only `pressed_at` and the flags stamped at press time, so the
        // 10s escape hatch works even if the menu is ever wrong.
        if let (Some(p), false) = (self.pressed_at, self.reset_done) {
            let held = now_ms.saturating_sub(p);
            // Fire the menu-origin close exactly once per press, even if a
            // slow host tick jumps clean over the threshold.
            if self.hold_started_in_menu && !self.hold_fired && held >= MENU_CLOSE_HOLD_MS {
                self.hold_fired = true;
                // Close = drop the overlay. The mode machine underneath was
                // never touched, so "restore the prior state" is a no-op.
                self.menu = None;
            }
            if held >= HOLD_GRACE_MS {
                self.reset_active = true;
                if held >= MENU_CLOSE_HOLD_MS {
                    // Red phase: 2s..10s runs toward the wipe, clearly labeled
                    // so nobody erases their Wi-Fi closing the menu.
                    self.hold_erase_phase = true;
                    let fill = held - MENU_CLOSE_HOLD_MS;
                    let span = ERASE_HOLD_MS - MENU_CLOSE_HOLD_MS;
                    self.reset_progress = ((fill * 100) / span).min(100) as u8;
                    if held >= ERASE_HOLD_MS {
                        self.reset_done = true;
                        self.reset_progress = 100;
                        self.reset_done_at = Some(now_ms);
                    }
                } else {
                    // Amber warm-up: fills to the 2s mark. Sharing that mark
                    // with the menu-origin close keeps every press on the
                    // identical timeline — the red phase takes over at 2s
                    // whether or not a menu just closed under it.
                    self.hold_erase_phase = false;
                    let fill = held - HOLD_GRACE_MS;
                    let span = MENU_CLOSE_HOLD_MS - HOLD_GRACE_MS;
                    self.reset_progress = ((fill * 100) / span).min(100) as u8;
                }
            }
        }
        // The menu closes itself after two minutes of an untouched knob —
        // device clock, like every timeout on this board.
        if let Some(m) = &self.menu {
            if now_ms.saturating_sub(m.last_interaction) >= MENU_IDLE_TIMEOUT_MS {
                self.menu = None;
            }
        }
        // The simulator has no firmware to reboot it, so auto-dismiss the "complete"
        // screen after a few seconds (the real device reboots well before this).
        if let Some(t) = self.reset_done_at {
            if now_ms.saturating_sub(t) > 4000 {
                self.reset_active = false;
                self.reset_progress = 0;
                self.reset_done = false;
                self.reset_done_at = None;
            }
        }
        // resolve a single press once the double-press window passes
        if let Some(s) = self.pending_single {
            if now_ms.saturating_sub(s) > DOUBLE_WINDOW_MS {
                self.pending_single = None;
                self.prev_release = None;
                self.apply(Gesture::Single);
            }
        }
        if self.mode == Mode::Booting {
            if let Some(st) = self.started {
                if now_ms.saturating_sub(st) >= BOOT_TIME_MS {
                    self.mode = Mode::Running;
                }
            }
        }
        // The pair code outlives its usefulness the moment nobody's typing it
        // in: a stray double press must not cost the panel its scores until
        // someone walks over and presses again. EXCEPT on a server-flagged
        // unpaired board (`sticky_pairing`): there the code is the only thing
        // worth showing, and it stays until SetupDone or a gesture.
        if self.mode == Mode::Syncing && !self.sticky_pairing {
            if let Some(t) = self.syncing_since {
                if now_ms.saturating_sub(t) >= SYNC_TIMEOUT_MS {
                    self.syncing_since = None;
                    self.mode = Mode::Running;
                }
            }
        }
        // Report a knob turn the server missed (turned while offline, or on a
        // local screen) as soon as it can hear it — one message, final level.
        if self.brightness_pending && self.online && self.mode == Mode::Running {
            self.brightness_pending = false;
            self.outbox.push(ClientMsg::DeviceBrightness { level: self.local_brightness });
        }
        if self.mode == Mode::Running {
            let current_id = if let Some(m) = &self.manifest {
                if m.rotation.len() > 1 {
                    let dwell = m.rotation[self.idx].dwell_ms as u64;
                    if now_ms.saturating_sub(self.last_advance) >= dwell {
                        self.idx = (self.idx + 1) % m.rotation.len();
                        self.last_advance = now_ms;
                    }
                }
                m.rotation.get(self.idx).map(|e| e.frame_id.clone())
            } else {
                None
            };
            // Track the last frame we can actually show so a fresh manifest (new
            // ids not yet downloaded) keeps the previous frame up instead of blanking.
            if let Some(id) = current_id {
                if self.cache.contains_key(&id) {
                    self.last_good = Some(id);
                }
            }
        }
        // Sleep/wake fade: stamp the moment the deliberate-darkness intent
        // flips — AFTER the gesture/mode logic above, so a tap-sleep resolved
        // this very tick starts its ramp now, not one host frame late.
        // (`on_manifest` flips are picked up on the next tick; hosts tick at
        // ~30fps, so the ramp starts within a frame.)
        let dark = self.dark_intent_now();
        if dark != self.dark_intent {
            self.dark_intent = dark;
            self.fade_flip_at = Some(now_ms);
        }
    }

    fn apply(&mut self, g: Gesture) {
        // With the menu up, a tap is a SELECT and never reaches the sleep
        // toggle below — the modes don't exist while the overlay has the knob.
        if self.menu.is_some() {
            match g {
                Gesture::Single => self.menu_select(),
                // The toggle's other half: double-tap closes the menu from any
                // of its screens (the overlay drops; the mode underneath was
                // never touched).
                Gesture::Double => self.menu = None,
                // The post-wipe provisioning entry (firmware/sim injects it
                // after the credentials are gone); the menu has no say.
                Gesture::Long => {
                    self.menu = None;
                    self.enter_setup();
                }
            }
            return;
        }
        match g {
            // single press: while online + Running, the SERVER owns what a press
            // means (sleep / wake / "no games" notice) — report it and let the next
            // manifest change the screen. Offline (or on a setup/sync screen) fall
            // back to the local toggle so the knob is never a dead end.
            Gesture::Single => {
                if self.online && matches!(self.mode, Mode::Running) {
                    self.outbox.push(ClientMsg::ButtonPress);
                } else if matches!(self.mode, Mode::Running | Mode::Asleep) {
                    // Sleep toggle only makes sense between running and asleep;
                    // a press on the setup/pairing screens must NOT yank the
                    // board out of provisioning (the hold-to-reset still works
                    // everywhere — that's the guaranteed escape hatch).
                    self.mode = if self.mode == Mode::Running {
                        Mode::Asleep
                    } else {
                        Mode::Running
                    };
                    // Tell the server anyway. The local flip is what makes the
                    // knob work with no socket, but it's invisible to `power`
                    // until this lands — queued now, sent on reconnect, so the
                    // app's toggle and the panel agree again.
                    self.outbox.push(ClientMsg::ButtonPress);
                } else if self.mode == Mode::Syncing {
                    // Dismiss the pair-code screen — including the sticky
                    // server-initiated one: the owner tapping is the "I'll
                    // pair later" signal, and the code stays one double-tap
                    // away in the menu.
                    self.mode = Mode::Running;
                    self.syncing_since = None;
                    self.sticky_pairing = false;
                }
            }
            // double press: toggles the settings menu (open, since it's closed
            // here). This was a reserved no-op after the pairing screen moved
            // into the menu; the menu itself moved here from the old 3s hold,
            // which is erase-only now. Never reaches the sleep toggle — a
            // fumbled double tap must not flash the panel off-and-on.
            Gesture::Double => self.open_menu(),
            // long press: factory reset -> setup
            Gesture::Long => self.enter_setup(),
        }
    }

    /// The post-wipe provisioning entry (what `Gesture::Long` injects).
    fn enter_setup(&mut self) {
        self.manifest = None;
        self.cache.clear();
        self.syncing_since = None;
        self.sticky_pairing = false;
        // Fresh provisioning always starts from the top of the handshake.
        self.setup_phase = SetupPhase::WaitingBluetooth;
        self.mode = Mode::Setup;
    }

    /// Open the settings menu (the double-tap toggle). The mode machine is
    /// left exactly as it stands — the menu is an overlay, and dropping it
    /// later is the whole "restore the prior state" story.
    fn open_menu(&mut self) {
        if self.menu.is_none() {
            self.menu = Some(MenuState {
                screen: MenuScreen::Root,
                selected: 0,
                scroll: 0,
                confirm_selected: false,
                last_interaction: self.clock,
            });
        }
    }

    /// Knob rotation while the menu is up: move the selection / scroll / flip
    /// the confirm toggle. Never brightness (see `encoder`).
    ///
    /// DIRECTION — deliberately the INVERSE of the brightness mapping
    /// (`encoder(+1)` raises brightness; here `-delta` walks DOWN the list):
    /// the owner reports the same-sign mapping felt backwards at the physical
    /// knob. Pinned by `menu_selection_direction_is_pinned`.
    fn menu_rotate(&mut self, delta: i32) {
        let clock = self.clock;
        let Some(m) = &mut self.menu else { return };
        m.last_interaction = clock;
        match m.screen {
            MenuScreen::Root => {
                m.selected = (m.selected as i32 - delta).clamp(0, MENU_ITEM_COUNT as i32 - 1)
                    as usize;
            }
            // The code screen has nothing to scroll; the turn only counts as
            // presence (it keeps the 120s timeout from closing mid-pairing).
            MenuScreen::PairCode => {}
            MenuScreen::DebugInfo => {
                let max = self.debug_info_lines().len().saturating_sub(DEBUG_INFO_VISIBLE);
                let m = self.menu.as_mut().unwrap();
                m.scroll = (m.scroll as i32 - delta).clamp(0, max as i32) as usize;
            }
            // Direction picks the option outright (rather than toggling) so a
            // detent that double-reports can't land on the armed side by
            // accident. Same convention as the list: the down-the-list
            // direction moves the selection forward, to ERASE / UPDATE.
            MenuScreen::ResetConfirm | MenuScreen::UpdateConfirm => {
                m.confirm_selected = delta < 0
            }
        }
    }

    /// Tap while the menu is up.
    fn menu_select(&mut self) {
        let clock = self.clock;
        let Some(m) = &mut self.menu else { return };
        m.last_interaction = clock;
        match m.screen {
            MenuScreen::Root => match m.selected {
                // The default selection: one tap in, one tap out.
                MENU_EXIT => self.menu = None,
                MENU_UPDATE => {
                    // Armed only when the server has said a newer image exists;
                    // with nothing offered the row reads UP TO DATE and the tap
                    // is a no-op. The request itself sits behind a confirm
                    // screen: the panel goes DARK for the whole install, and a
                    // user who yanks power at that moment because the board
                    // "turned off" is the exact failure the warning prevents.
                    if self.update_available.is_some() {
                        m.screen = MenuScreen::UpdateConfirm;
                        // CANCEL every visit — same rule as the reset confirm.
                        m.confirm_selected = false;
                    }
                }
                MENU_PAIR => m.screen = MenuScreen::PairCode,
                MENU_DEBUG => {
                    m.screen = MenuScreen::DebugInfo;
                    m.scroll = 0;
                }
                MENU_RESET => {
                    m.screen = MenuScreen::ResetConfirm;
                    // CANCEL every visit — a stale ERASE from a backed-out
                    // attempt must never be one tap from firing.
                    m.confirm_selected = false;
                }
                _ => {}
            },
            // The sub-screens tap back to the menu root.
            MenuScreen::PairCode | MenuScreen::DebugInfo => m.screen = MenuScreen::Root,
            MenuScreen::ResetConfirm => {
                if m.confirm_selected {
                    // Same latch the 10s hold sets — one wipe path, not two.
                    self.menu = None;
                    self.reset_done = true;
                    self.reset_progress = 100;
                    self.reset_done_at = Some(clock);
                } else {
                    m.screen = MenuScreen::Root;
                }
            }
            MenuScreen::UpdateConfirm => {
                if m.confirm_selected {
                    // The outbox queues, so confirming while offline sends the
                    // request when the socket returns. Close the menu: the OTA
                    // blank takes over the panel the moment the offer lands.
                    self.outbox.push(ClientMsg::RequestUpdate);
                    self.menu = None;
                } else {
                    m.screen = MenuScreen::Root;
                }
            }
        }
    }

    /// Current display as a `FRAME_PIXELS`-long 0RGB buffer.
    pub fn framebuffer(&self) -> Vec<u32> {
        // Hold-to-reset takes over the whole screen, from any mode (even asleep).
        if self.reset_active || self.reset_done {
            let mut buf = self.screen_reset();
            scale_buf(&mut buf, self.local_brightness);
            return buf;
        }
        // An OTA update in flight BLANKS the panel, overriding every mode below
        // reset. The download's flash writes stall the render task per sector,
        // so any lit content strobes; black has nothing to strobe (and progress
        // now shows in the app, not here). Return pure black — no brightness
        // scaling needed, it's already dark. (Reset still wins — the escape
        // hatch is sacred, and stays fully lit even mid-download.)
        if self.updating.is_some() {
            return vec![0u32; FRAME_PIXELS];
        }
        // The settings menu suspends whatever the mode machine is showing.
        // Below updating on purpose: a menu lit during an OTA download would
        // strobe like any other content. No brightness bar here either — the
        // knob navigates, it doesn't dim (see `encoder`).
        if let Some(m) = &self.menu {
            let mut buf = match m.screen {
                MenuScreen::Root => self.screen_menu_root(m),
                // The pairing screen Mode::Syncing owns, unchanged.
                MenuScreen::PairCode => self.screen_syncing(),
                MenuScreen::DebugInfo => self.screen_debug_info(m),
                MenuScreen::ResetConfirm => self.screen_reset_confirm(m),
                MenuScreen::UpdateConfirm => self.screen_update_confirm(m),
            };
            scale_buf(&mut buf, self.local_brightness);
            return buf;
        }
        // Only server content gets the transient brightness bar. On the
        // firmware-local screens the bottom rows are the status LINE — measured
        // baselines: "FINDING SERVER" and "REBOOTING" fill rows 25-29, the
        // pair-code spinner 28-30 — so even a 4-row bar eats the word the user
        // is standing there reading. Those screens already report the level by
        // dimming whole (`scale_buf` below), which is feedback enough.
        // The sleep/wake fade multiplier for everything below (the overlays
        // above returned early on purpose — they snap).
        let fade = self.fade_factor();
        let mut server_content = false;
        let mut buf = match self.mode {
            Mode::Booting => self.screen_boot(),
            Mode::Setup => self.screen_setup(),
            Mode::Syncing => self.screen_syncing(),
            // Deliberate darkness. Once the fade lands this is a hard black;
            // during the SLEEP_FADE_MS goodbye the content that was up dims
            // out instead of snapping off.
            Mode::Asleep => {
                if fade == 0 {
                    return vec![0u32; FRAME_PIXELS];
                }
                self.screen_last_content()
            }
            Mode::Running => {
                // A dropped websocket often re-establishes within a second or
                // two; swapping screens instantly makes every blip flash
                // "CONNECTING / FINDING SERVER". While there's content to show,
                // hold it through the grace window and only admit the outage
                // once it's real. No content (fresh boot, nothing cached) means
                // nothing to hold — connect screen right away.
                let holding = self.has_content()
                    && self
                        .offline_since
                        .is_some_and(|t| self.clock.saturating_sub(t) < OFFLINE_GRACE_MS);
                // Power intent outlives the socket. A board the user turned OFF
                // must stay off through an ISP hiccup or a 30-60s server
                // deploy: an animated CONNECTING screen lighting up a dark
                // bedroom at 3am is worse than any outage it reports, and
                // nobody is standing there to read it anyway.
                let dark_by_intent = self.manifest.as_ref().is_some_and(|m| !m.display_on);
                if self.online || holding || dark_by_intent {
                    server_content = true;
                    if dark_by_intent && fade > 0 {
                        // A display_on:false manifest ships an EMPTY rotation,
                        // so the fade-out needs the last shown frame — not
                        // `screen_running`'s immediate black.
                        self.screen_last_content()
                    } else {
                        self.screen_running()
                    }
                } else {
                    self.screen_connecting()
                }
            }
        };
        // The notice countdown bar: the server ships ONE frame with the bar's
        // empty outline and `countdown_ms`; the fill is ours, on the device
        // clock, anchored per epoch — smooth at the render rate instead of the
        // old pre-rendered sweep's one chunky step per second. Geometry is in
        // LOCKSTEP with server render.rs `render_notice` (bar_x 4, bar_y 26,
        // 56 wide, 5 tall — interior 54x3 at 5,27), color with its fill.
        if server_content {
            if let (Some((_, t0)), Some(m)) = (&self.countdown_anchor, &self.manifest) {
                if let Some(total) = m.countdown_ms {
                    let elapsed = self.clock.saturating_sub(*t0);
                    let w = (elapsed.min(total as u64) * 54 / (total as u64).max(1)) as usize;
                    for y in 27..30usize {
                        for x in 5..(5 + w) {
                            buf[y * FRAME_WIDTH + x] = 0x00FF_AA00;
                        }
                    }
                }
            }
        }
        // Brightness is applied exactly once, here — `local_brightness` tracks the
        // server's value while online (see `on_manifest`) and doubles as the local
        // fallback offline, so it dims every screen (boot/setup/sync/running).
        scale_buf(&mut buf, self.local_brightness);
        // Transient indicator for ~2s after a knob turn, drawn at full level so it
        // stays legible even when the screen itself is dimmed.
        if server_content && self.brightness_changed_at.is_some() {
            overlay_brightness_bar(&mut buf, self.local_brightness);
        }
        // The fade multiplies the EFFECTIVE brightness on top of the user
        // level — `local_brightness` itself never moves, so the panel wakes
        // at exactly the level it slept at.
        if fade < 100 {
            scale_buf(&mut buf, fade);
        }
        buf
    }

    /// Is the panel DELIBERATELY dark — asleep locally, or the server's
    /// manifest says display off (which it only ever ships as the Blank
    /// off_state)? Overlays (menu, reset, OTA blank) are invisible to this on
    /// purpose: they snap, and must not restart the fade underneath.
    fn dark_intent_now(&self) -> bool {
        matches!(self.mode, Mode::Asleep)
            || (self.mode == Mode::Running
                && self.manifest.as_ref().is_some_and(|m| !m.display_on))
    }

    /// Effective-brightness multiplier (0..=100) for the sleep/wake fade: a
    /// linear device-clock ramp across SLEEP_FADE_MS from the last intent
    /// flip, landing exactly on 0 (dark) or 100 (lit). No flip recorded =
    /// settled at whatever the current intent is.
    fn fade_factor(&self) -> u8 {
        let Some(t) = self.fade_flip_at else {
            return if self.dark_intent { 0 } else { 100 };
        };
        let ramp = (self.clock.saturating_sub(t) * 100 / SLEEP_FADE_MS).min(100) as u8;
        if self.dark_intent {
            100 - ramp
        } else {
            ramp
        }
    }

    /// What the fade-out dims: the running content when the manifest still
    /// shows it (a local tap-sleep leaves the manifest lit underneath), else
    /// the last frame that made it to the panel — `last_good` survives cache
    /// eviction for exactly this hand-off. Tall frames show their top window;
    /// a 400ms goodbye doesn't need the scroll phase back.
    fn screen_last_content(&self) -> Vec<u32> {
        if self
            .manifest
            .as_ref()
            .is_some_and(|m| m.display_on && !m.rotation.is_empty())
        {
            return self.screen_running();
        }
        let mut buf = vec![0u32; FRAME_PIXELS];
        if let Some(px) = self.last_good.as_ref().and_then(|id| self.cache.get(id)) {
            blit_frame_window(&mut buf, px, 0);
        }
        buf
    }

    /// Does the board know what the server wants on the panel — a blittable
    /// frame, or deliberate darkness/idle? This is what the offline grace
    /// window holds on to. An intentionally dark panel (`display_on: false` —
    /// which the server always ships with an EMPTY rotation) counts: staying
    /// dark through a blip beats lighting up with the connect screen at 3am.
    /// Only a board with no manifest at all (fresh boot) has nothing to hold.
    fn has_content(&self) -> bool {
        let Some(m) = &self.manifest else { return false };
        if !m.display_on || m.rotation.is_empty() {
            return true; // deliberate blank — black IS the content
        }
        let entry = &m.rotation[self.idx.min(m.rotation.len() - 1)];
        self.cache.contains_key(&entry.frame_id)
            || self.last_good.as_ref().is_some_and(|id| self.cache.contains_key(id))
    }

    fn screen_running(&self) -> Vec<u32> {
        let mut buf = vec![0u32; FRAME_PIXELS];
        let Some(m) = &self.manifest else { return buf };
        if !m.display_on || m.rotation.is_empty() {
            return buf;
        }
        let entry = &m.rotation[self.idx.min(m.rotation.len() - 1)];
        let px = self
            .cache
            .get(&entry.frame_id)
            .or_else(|| self.last_good.as_ref().and_then(|id| self.cache.get(id)));
        let Some(px) = px else {
            return buf;
        };
        // Frames taller than the panel scroll: show a 32px window whose offset
        // runs one full pass per dwell, in fifths — hold, down, hold, up, hold
        // — so the rotation advances from a resting frame, never mid-glide.
        // The phase clock is time-in-this-entry, which same-shape manifest
        // refreshes deliberately preserve — a score update mid-scroll doesn't
        // yank the view back to the top.
        let frame_h = px.len() / (FRAME_WIDTH * 2);
        let row0 = if entry.scroll && frame_h > FRAME_HEIGHT {
            scroll_offset(
                self.clock.saturating_sub(self.last_advance),
                entry.dwell_ms as u64,
                frame_h - FRAME_HEIGHT,
            )
        } else {
            0
        };
        // No dimming here — brightness is applied once to the whole framebuffer
        // in `framebuffer()`. Scaling the frame by the manifest value here too
        // would square the dim (50% knob × 50% manifest = 25%).
        blit_frame_window(&mut buf, px, row0);
        buf
    }

    fn screen_boot(&self) -> Vec<u32> {
        let mut c = Canvas::new();
        center_text(&mut c, "SCOREBOARD", 12, &FONT_5X8, Rgb888::new(0x33, 0xAA, 0xFF));
        // Firmware version, small and grey under the title — the one place a
        // user can read the running version off the panel itself.
        if let Some(v) = &self.version {
            center_text(&mut c, v, 19, &FONT_4X6, Rgb888::new(0x88, 0x88, 0x88));
        }
        spinner(&mut c, self.anim, 24);
        c.into_u32()
    }

    /// BLE provisioning screens. No pairing code here — the board has no Wi-Fi
    /// (let alone a server that knows a code); it names which half of the
    /// handshake it's waiting on, animated so the panel never looks hung.
    fn screen_setup(&self) -> Vec<u32> {
        let mut c = Canvas::new();
        let grey = Rgb888::new(0x88, 0x88, 0x88);
        let blue = Rgb888::new(0x33, 0xAA, 0xFF);
        match self.setup_phase {
            SetupPhase::WaitingBluetooth => {
                center_text(&mut c, "SETUP", 6, &FONT_5X8, Rgb888::WHITE);
                center_text(&mut c, "WAITING FOR", 16, &FONT_4X6, grey);
                center_text(&mut c, "BLUETOOTH", 25, &FONT_4X6, blue);
                ellipsis(&mut c, 52, 25, self.anim, blue);
            }
            SetupPhase::WaitingCreds => {
                center_text(&mut c, "SETUP", 6, &FONT_5X8, Rgb888::WHITE);
                center_text(&mut c, "WAITING FOR", 16, &FONT_4X6, grey);
                center_text(&mut c, "WI-FI", 25, &FONT_4X6, blue);
                ellipsis(&mut c, 44, 25, self.anim, blue);
            }
            SetupPhase::Saved => {
                // On screen <1s — the firmware reboots right after; static is fine.
                center_text(&mut c, "WI-FI SAVED", 6, &FONT_4X6, Rgb888::new(0x33, 0xDD, 0x55));
                draw_checkmark(&mut c, FRAME_WIDTH as i32 / 2, 16);
                center_text(&mut c, "REBOOTING", 27, &FONT_4X6, grey);
            }
        }
        c.into_u32()
    }

    fn screen_syncing(&self) -> Vec<u32> {
        let mut c = Canvas::new();
        center_text(&mut c, "PAIR CODE", 6, &FONT_4X6, Rgb888::new(0x88, 0x88, 0x88));
        center_text(&mut c, &self.setup_code, 17, &FONT_6X10, Rgb888::new(0x33, 0xAA, 0xFF));
        spinner(&mut c, self.anim, 28);
        c.into_u32()
    }

    /// Switch-style connect progress: board -> router -> server nodes, with a
    /// dot walking whichever leg is still coming up. Covers both the first boot
    /// after provisioning and mid-session drops — a drop now says WHICH hop is
    /// down, which the old flat RECONNECTING never did.
    fn screen_connecting(&self) -> Vec<u32> {
        let mut c = Canvas::new();
        let amber = Rgb888::new(0xFF, 0x88, 0x00);
        let green = Rgb888::new(0x33, 0xDD, 0x55);
        let dim = Rgb888::new(0x33, 0x33, 0x33);
        let grey = Rgb888::new(0x88, 0x88, 0x88);
        center_text(&mut c, "CONNECTING", 4, &FONT_4X6, Rgb888::WHITE);
        draw_panel_node(&mut c);
        draw_wifi_node(&mut c, if self.wifi_up { green } else { grey });
        draw_server_node(&mut c);
        // Leg origins 14 and 37 put the ten dots at exact mirror positions
        // around the panel's center line (x = 31.5): 14,17,20,23,26 reflects
        // to 49,46,43,40,37. The old 15/38 crowded the Wi-Fi mark from the
        // left (margins 3-and-1 vs 2-and-1) and read visibly lopsided.
        if self.wifi_up {
            draw_leg(&mut c, 14, green, None);
            draw_leg(&mut c, 37, dim, Some(self.anim % 5));
            center_text(&mut c, "FINDING SERVER", 27, &FONT_4X6, amber);
        } else {
            draw_leg(&mut c, 14, dim, Some(self.anim % 5));
            draw_leg(&mut c, 37, dim, None);
            center_text(&mut c, "JOINING WI-FI", 27, &FONT_4X6, amber);
        }
        c.into_u32()
    }

    /// The hold overlay, in its three phases: the amber warm-up bar to 2s
    /// (labeled CLOSING MENU when the press began there), the red
    /// KEEP-HOLDING-TO-ERASE run to 10s, and the green completion screen
    /// (the firmware reboots moments after that one).
    fn screen_reset(&self) -> Vec<u32> {
        let mut c = Canvas::new();
        let grey = Rgb888::new(0x88, 0x88, 0x88);
        if self.reset_done {
            let green = Rgb888::new(0x33, 0xDD, 0x55);
            center_text(&mut c, "RESET COMPLETE", 6, &FONT_4X6, green);
            hold_bar(&mut c, 100, green);
            draw_checkmark(&mut c, FRAME_WIDTH as i32 / 2, 19);
        } else if self.hold_erase_phase {
            // Visibly NOT the menu bar: red, two-line warning, and an explicit
            // out — nobody should erase their Wi-Fi reaching for settings.
            let red = Rgb888::new(0xFF, 0x55, 0x33);
            center_text(&mut c, "KEEP HOLDING", 4, &FONT_4X6, red);
            center_text(&mut c, "TO ERASE WI-FI", 11, &FONT_4X6, red);
            hold_bar(&mut c, self.reset_progress, red);
            center_text(&mut c, "RELEASE TO KEEP", 28, &FONT_4X6, grey);
        } else {
            let amber = Rgb888::new(0xFF, 0x88, 0x00);
            let title =
                if self.hold_started_in_menu { "CLOSING MENU" } else { "HOLD TO ERASE" };
            center_text(&mut c, title, 6, &FONT_4X6, amber);
            hold_bar(&mut c, self.reset_progress, amber);
            // Version at the bottom — hold the knob a moment and release: the
            // quickest way to read what's running off the panel itself.
            if let Some(v) = &self.version {
                center_text(&mut c, v, 28, &FONT_4X6, grey);
            }
        }
        c.into_u32()
    }

    /// The UPDATE row's label: the offered version (with an up-arrow) when the
    /// server said a newer image exists — otherwise a plain UP TO DATE, which
    /// is also what the row IS: selecting it does nothing. (It used to show
    /// the running version, which read as a tappable "update to this" and
    /// confused everyone; the running version lives on DEBUG INFO's FW row.)
    fn update_row_label(&self) -> String {
        match &self.update_available {
            Some(v) => format!("UPDATE ^{v}"),
            None => String::from("UP TO DATE"),
        }
    }

    /// Root settings list: five FONT_4X6 rows (EXIT on top — no title row;
    /// five rows are exactly what 32px holds at a readable pitch). Every row's
    /// text stays WHITE; the selection is a filled deep-blue bar underneath —
    /// the old inverse style (dark text on amber) was hard to read while
    /// scrolling. The blue is picked to stay visibly blue, with the white
    /// text still brighter, at brightness 10 (see
    /// `menu_selection_stays_legible_at_min_brightness` and the screen dumps).
    fn screen_menu_root(&self, m: &MenuState) -> Vec<u32> {
        let mut c = Canvas::new();
        let update_row = self.update_row_label();
        let items =
            ["EXIT", update_row.as_str(), "PAIR CODE", "DEBUG INFO", "RESET BOARD"];
        for (i, label) in items.iter().enumerate() {
            let y = 1 + i as i32 * 6;
            if i == m.selected {
                Rectangle::new(Point::new(0, y - 1), Size::new(FRAME_WIDTH as u32, 7))
                    .into_styled(PrimitiveStyle::with_fill(MENU_SELECT_BLUE))
                    .draw(&mut c)
                    .ok();
            }
            left_text(&mut c, label, 1, y, Rgb888::WHITE);
        }
        c.into_u32()
    }

    /// The rows the DEBUG INFO screen scrolls through. Built on demand (also
    /// by `menu_rotate` for the scroll clamp) — the list is tiny.
    fn debug_info_lines(&self) -> Vec<InfoLine> {
        let grey = Rgb888::new(0x88, 0x88, 0x88);
        let white = Rgb888::new(0xCC, 0xCC, 0xCC);
        let line = |text: String, color: Rgb888| InfoLine { text, color, bars: None };
        let d = &self.diag;
        let mut ls = Vec::new();
        ls.push(line(String::from("DEBUG INFO"), Rgb888::new(0x33, 0xAA, 0xFF)));
        ls.push(line(
            format!("SSID {}", d.ssid.as_deref().unwrap_or("--")),
            white,
        ));
        // Signal row: dBm as text, plus the four ascending bars everyone can
        // read from across the room (drawn by the renderer, not glyphs).
        ls.push(match d.rssi_dbm {
            Some(r) => InfoLine {
                text: format!("SIG {r}dBm"),
                color: white,
                bars: Some(rssi_bars(r)),
            },
            None => line(String::from("SIG --"), grey),
        });
        ls.push(match d.ip {
            Some([a, b, cc, dd]) => line(format!("IP {a}.{b}.{cc}.{dd}"), white),
            None => line(String::from("IP --"), grey),
        });
        // The server endpoint can outrun 16 chars (a Railway hostname): give
        // it its own label row and wrap the value rather than truncating the
        // one fact a support call needs read back verbatim.
        match &d.server {
            Some(srv) if srv.len() > 12 => {
                ls.push(line(String::from("SRV"), grey));
                for chunk in srv.as_bytes().chunks(16) {
                    ls.push(line(String::from_utf8_lossy(chunk).into_owned(), white));
                }
            }
            Some(srv) => ls.push(line(format!("SRV {srv}"), white)),
            None => ls.push(line(String::from("SRV --"), grey)),
        }
        ls.push(if self.online {
            line(String::from("NET ONLINE"), Rgb888::new(0x33, 0xDD, 0x55))
        } else {
            line(String::from("NET OFFLINE"), Rgb888::new(0xFF, 0x55, 0x33))
        });
        ls.push(line(
            format!("ID {}", d.board_id_prefix.as_deref().unwrap_or("--")),
            white,
        ));
        // Uptime from the device clock — the one number that needs no host.
        let up = self.clock.saturating_sub(self.started.unwrap_or(self.clock));
        ls.push(line(format!("UP {}", fmt_uptime(up)), white));
        ls.push(match d.free_heap_bytes {
            Some(h) => line(format!("HEAP {h}"), white),
            None => line(String::from("HEAP --"), grey),
        });
        ls.push(line(
            format!("FW {}", self.version.as_deref().unwrap_or("--")),
            white,
        ));
        ls
    }

    /// Scrollable diagnostics: five rows at a time, chevrons at the right edge
    /// hinting at more above/below.
    fn screen_debug_info(&self, m: &MenuState) -> Vec<u32> {
        let lines = self.debug_info_lines();
        let max_scroll = lines.len().saturating_sub(DEBUG_INFO_VISIBLE);
        let scroll = m.scroll.min(max_scroll);
        let mut c = Canvas::new();
        for (row, l) in lines.iter().skip(scroll).take(DEBUG_INFO_VISIBLE).enumerate() {
            let y = 1 + row as i32 * 6;
            left_text(&mut c, &l.text, 1, y, l.color);
            if let Some(n) = l.bars {
                // Four ascending bars, right-aligned on the row; unlit ones
                // stay faintly visible so "one bar" reads as one-of-four.
                for i in 0..4i32 {
                    let h = 2 + i;
                    let color = if (i as u8) < n {
                        Rgb888::new(0x33, 0xDD, 0x55)
                    } else {
                        Rgb888::new(0x28, 0x28, 0x28)
                    };
                    Rectangle::new(
                        Point::new(50 + i * 3, y + 5 - h),
                        Size::new(2, h as u32),
                    )
                    .into_styled(PrimitiveStyle::with_fill(color))
                    .draw(&mut c)
                    .ok();
                }
            }
        }
        let hint = Rgb888::new(0x88, 0x88, 0x88);
        if scroll > 0 {
            for (x, y) in [(61, 0), (60, 1), (62, 1)] {
                Pixel(Point::new(x, y), hint).draw(&mut c).ok();
            }
        }
        if scroll < max_scroll {
            for (x, y) in [(61, 31), (60, 30), (62, 30)] {
                Pixel(Point::new(x, y), hint).draw(&mut c).ok();
            }
        }
        c.into_u32()
    }

    /// RESET BOARD confirmation. Same selection convention as the root list —
    /// text stays WHITE, the selection is the filled bar underneath: deep
    /// blue for CANCEL (the ordinary choice), deep RED for ERASE — the danger
    /// family the super-hold phase uses, so the two roads to the wipe look
    /// like the same decision.
    fn screen_reset_confirm(&self, m: &MenuState) -> Vec<u32> {
        let mut c = Canvas::new();
        let grey = Rgb888::new(0x88, 0x88, 0x88);
        center_text(&mut c, "RESET BOARD?", 4, &FONT_4X6, Rgb888::WHITE);
        center_text(&mut c, "ERASES WI-FI", 11, &FONT_4X6, grey);
        let bar_y = 21i32;
        let (x, fill) = if m.confirm_selected {
            (34, MENU_SELECT_RED)
        } else {
            (2, MENU_SELECT_BLUE)
        };
        Rectangle::new(Point::new(x, bar_y), Size::new(28, 9))
            .into_styled(PrimitiveStyle::with_fill(fill))
            .draw(&mut c)
            .ok();
        center_text_at(&mut c, "CANCEL", 16, bar_y + 4, &FONT_4X6, Rgb888::WHITE);
        center_text_at(&mut c, "ERASE", 48, bar_y + 4, &FONT_4X6, Rgb888::WHITE);
        c.into_u32()
    }
}

impl Board {
    fn screen_update_confirm(&self, m: &MenuState) -> Vec<u32> {
        let mut c = Canvas::new();
        let grey = Rgb888::new(0x88, 0x88, 0x88);
        let amber = Rgb888::new(0xFF, 0x88, 0x00);
        center_text(&mut c, "INSTALL UPDATE?", 2, &FONT_4X6, Rgb888::WHITE);
        // The version gets its own row: "INSTALL 2026.7.15?" is 72px on a
        // 64px panel.
        if let Some(v) = &self.update_available {
            let line = alloc::format!("^{v}");
            center_text(&mut c, &line, 9, &FONT_4X6, grey);
        }
        // THE warning this screen exists for: the panel goes dark for the
        // whole install, which reads as "it turned off" — and pulling power
        // mid-flash is the one way a customer can hurt an update.
        center_text(&mut c, "DO NOT UNPLUG", 15, &FONT_4X6, amber);
        let bar_y = 22i32;
        let x = if m.confirm_selected { 34 } else { 2 };
        Rectangle::new(Point::new(x, bar_y), Size::new(28, 9))
            .into_styled(PrimitiveStyle::with_fill(MENU_SELECT_BLUE))
            .draw(&mut c)
            .ok();
        center_text_at(&mut c, "CANCEL", 16, bar_y + 4, &FONT_4X6, Rgb888::WHITE);
        center_text_at(&mut c, "UPDATE", 48, bar_y + 4, &FONT_4X6, Rgb888::WHITE);
        c.into_u32()
    }
}

/// One row of the DEBUG INFO screen. `bars` marks the signal row, whose four
/// ascending bars the renderer draws graphically.
struct InfoLine {
    text: String,
    color: Rgb888,
    bars: Option<u8>,
}

/// Signal bars from RSSI, the usual thresholds: -55 and up is all four.
fn rssi_bars(dbm: i8) -> u8 {
    match dbm {
        d if d >= -55 => 4,
        d if d >= -67 => 3,
        d if d >= -75 => 2,
        d if d >= -85 => 1,
        _ => 0,
    }
}

/// Compact uptime for a 16-char row: the two largest units that matter.
fn fmt_uptime(ms: u64) -> String {
    let s = ms / 1000;
    let (d, h, m) = (s / 86_400, (s / 3600) % 24, (s / 60) % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m {}s", s % 60)
    }
}

/// The hold bar every phase of the hold overlay shares (outline + proportional
/// fill) — same geometry the original hold-to-reset bar used, so the visual
/// language carries over.
fn hold_bar(c: &mut Canvas, progress: u8, color: Rgb888) {
    let bar_x = 6i32;
    let bar_y = 15i32;
    let bar_w = FRAME_WIDTH as i32 - 12;
    let bar_h = 9i32;
    Rectangle::new(Point::new(bar_x, bar_y), Size::new(bar_w as u32, bar_h as u32))
        .into_styled(PrimitiveStyle::with_stroke(Rgb888::new(0x55, 0x55, 0x55), 1))
        .draw(c)
        .ok();
    let fill_w = (bar_w - 2) as u32 * progress as u32 / 100;
    if fill_w > 0 {
        Rectangle::new(Point::new(bar_x + 1, bar_y + 1), Size::new(fill_w, (bar_h - 2) as u32))
            .into_styled(PrimitiveStyle::with_fill(color))
            .draw(c)
            .ok();
    }
}

// ---- tiny canvas for the firmware-local screens ----
struct Canvas {
    buf: Vec<Rgb888>,
}
impl Canvas {
    fn new() -> Self {
        Self { buf: vec![Rgb888::BLACK; FRAME_PIXELS] }
    }
    fn into_u32(self) -> Vec<u32> {
        self.buf
            .iter()
            .map(|p| ((p.r() as u32) << 16) | ((p.g() as u32) << 8) | (p.b() as u32))
            .collect()
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

fn center_text<'a>(c: &mut Canvas, s: &str, y_center: i32, font: &'a MonoFont<'a>, color: Rgb888) {
    center_text_at(c, s, FRAME_WIDTH as i32 / 2, y_center, font, color);
}

fn center_text_at<'a>(
    c: &mut Canvas,
    s: &str,
    x_center: i32,
    y_center: i32,
    font: &'a MonoFont<'a>,
    color: Rgb888,
) {
    let style = TextStyleBuilder::new()
        .alignment(Alignment::Center)
        .baseline(Baseline::Middle)
        .build();
    Text::with_text_style(s, Point::new(x_center, y_center), MonoTextStyle::new(font, color), style)
        .draw(c)
        .ok();
}

/// Left-aligned FONT_4X6 row with its glyph tops at `y_top` — the menu and
/// diagnostics screens are lists, and lists read left-to-right.
fn left_text(c: &mut Canvas, s: &str, x: i32, y_top: i32, color: Rgb888) {
    let style = TextStyleBuilder::new()
        .alignment(Alignment::Left)
        .baseline(Baseline::Top)
        .build();
    Text::with_text_style(s, Point::new(x, y_top), MonoTextStyle::new(&FONT_4X6, color), style)
        .draw(c)
        .ok();
}

fn spinner(c: &mut Canvas, anim: u32, y: i32) {
    // `anim` already advances once per 133ms of wall time (see `tick`).
    let on = anim % 3;
    for i in 0..3i32 {
        let color = if i as u32 == on { Rgb888::WHITE } else { Rgb888::new(0x22, 0x22, 0x22) };
        Rectangle::new(Point::new(FRAME_WIDTH as i32 / 2 - 8 + i * 7, y), Size::new(3, 3))
            .into_styled(PrimitiveStyle::with_fill(color))
            .draw(c)
            .ok();
    }
}

/// Trailing "..." after a centered word: grows a dot every ~530ms (4 anim
/// steps) then wraps — enough motion to show the board isn't hung without the
/// word itself jittering around. `x0` is the column just past the word.
fn ellipsis(c: &mut Canvas, x0: i32, y_center: i32, anim: u32, color: Rgb888) {
    let n = (anim / 4) % 4;
    for i in 0..n as i32 {
        Rectangle::new(Point::new(x0 + i * 3, y_center + 1), Size::new(2, 2))
            .into_styled(PrimitiveStyle::with_fill(color))
            .draw(c)
            .ok();
    }
}

/// One leg of the connect-progress screen: 5 single-pixel dots from `x0`, 3px
/// apart, on the node row. `base` is the resting color (dim grey while the leg
/// is pending, green once it's up); `bright` walks a white dot along the leg.
fn draw_leg(c: &mut Canvas, x0: i32, base: Rgb888, bright: Option<u32>) {
    for i in 0..5u32 {
        let color = if Some(i) == bright { Rgb888::WHITE } else { base };
        Pixel(Point::new(x0 + i as i32 * 3, 16), color).draw(c).ok();
    }
}

/// Left connect-screen node: the board itself — a panel outline with a grid of
/// lit "LED" pixels so it reads as this very scoreboard.
fn draw_panel_node(c: &mut Canvas) {
    Rectangle::new(Point::new(3, 13), Size::new(9, 7))
        .into_styled(PrimitiveStyle::with_stroke(Rgb888::new(0x88, 0x88, 0x88), 1))
        .draw(c)
        .ok();
    for &(x, y) in &[(5, 15), (7, 15), (9, 15), (5, 17), (7, 17), (9, 17)] {
        Pixel(Point::new(x, y), Rgb888::new(0x33, 0xAA, 0xFF)).draw(c).ok();
    }
}

/// Middle connect-screen node: the classic dot-plus-arcs Wi-Fi mark, tinted
/// green once the link is up.
fn draw_wifi_node(c: &mut Canvas, color: Rgb888) {
    // Even-width mark, mirror-symmetric about the panel's center line
    // (x = 31.5): a 2x2 dot astride it, and two 2px-THICK arcs built up from
    // that base — 1px strokes read spindly next to the chunky dot. Each arc
    // is two vertically adjacent pixels per column, stepping down at the
    // ends; grown upward (to y=11) so the single-row gaps between dot, inner
    // arc, and outer arc survive the thickening.
    #[rustfmt::skip]
    let px = [
        // outer arc: 8 wide, 2px stroke
        (30, 11), (31, 11), (32, 11), (33, 11),
        (29, 12), (30, 12), (31, 12), (32, 12), (33, 12), (34, 12),
        (28, 13), (29, 13), (34, 13), (35, 13),
        (28, 14), (35, 14),
        // inner arc: 6 wide, 2px stroke
        (30, 14), (31, 14), (32, 14), (33, 14),
        (29, 15), (30, 15), (31, 15), (32, 15), (33, 15), (34, 15),
        (29, 16), (34, 16),
        // 2x2 dot
        (31, 17), (32, 17), (31, 18), (32, 18),
    ];
    for &(x, y) in &px {
        Pixel(Point::new(x, y), color).draw(c).ok();
    }
}

/// Right connect-screen node: a tiny server rack — outline, a shelf line, and
/// green indicator lights per unit.
fn draw_server_node(c: &mut Canvas) {
    let grey = Rgb888::new(0x88, 0x88, 0x88);
    Rectangle::new(Point::new(52, 13), Size::new(9, 7))
        .into_styled(PrimitiveStyle::with_stroke(grey, 1))
        .draw(c)
        .ok();
    Line::new(Point::new(53, 16), Point::new(59, 16))
        .into_styled(PrimitiveStyle::with_stroke(Rgb888::new(0x44, 0x44, 0x44), 1))
        .draw(c)
        .ok();
    for &(x, y) in &[(54, 14), (54, 18)] {
        Pixel(Point::new(x, y), Rgb888::new(0x33, 0xDD, 0x55)).draw(c).ok();
    }
}

/// Scale every pixel by `brightness` (10..=100) — the encoder's global dimmer.
/// Vertical scroll offset at `t` ms into a dwell: the dwell splits into five
/// equal phases — hold top, glide down, hold bottom, glide up, hold top — so
/// one full pass exactly fits the user's "Time per game" and the frame is at
/// rest when the rotation advances. Repeats (via modulo) for single-entry
/// rotations that never advance. Total for any input: a panic here is a
/// device reboot.
fn scroll_offset(t: u64, dwell_ms: u64, max_off: usize) -> usize {
    let phase = dwell_ms / 5;
    if phase == 0 || max_off == 0 {
        return 0;
    }
    let t = t % dwell_ms;
    match t / phase {
        0 => 0,                                                    // hold top
        1 => (max_off as u64 * (t - phase) / phase) as usize,      // glide down
        2 => max_off,                                              // hold bottom
        3 => max_off - (max_off as u64 * (t - 3 * phase) / phase) as usize, // glide up
        _ => 0,                                                    // hold top (+ dwell%5 remainder)
    }
}

/// Blit a panel-sized window of an RGB565-LE frame into a 0RGB buffer,
/// starting at source row `row0` (tall frames scroll by moving the window).
fn blit_frame_window(buf: &mut [u32], px: &[u8], row0: usize) {
    for y in 0..FRAME_HEIGHT {
        for x in 0..FRAME_WIDTH {
            let src = ((row0 + y) * FRAME_WIDTH + x) * 2;
            if src + 1 < px.len() {
                let c = u16::from_le_bytes([px[src], px[src + 1]]);
                let (r, g, b) = rgb565_to_888(c);
                buf[y * FRAME_WIDTH + x] = ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
            }
        }
    }
}

fn scale_buf(buf: &mut [u32], brightness: u8) {
    if brightness >= 100 {
        return;
    }
    let b = brightness as u32;
    for px in buf.iter_mut() {
        let r = ((*px >> 16) & 0xFF) * b / 100;
        let g = ((*px >> 8) & 0xFF) * b / 100;
        let bl = (*px & 0xFF) * b / 100;
        *px = (r << 16) | (g << 8) | bl;
    }
}

/// A white checkmark centered on `(cx, cy)` — shown when the reset bar fills.
fn draw_checkmark(c: &mut Canvas, cx: i32, cy: i32) {
    let style = PrimitiveStyle::with_stroke(Rgb888::WHITE, 2);
    Line::new(Point::new(cx - 5, cy), Point::new(cx - 1, cy + 4))
        .into_styled(style)
        .draw(c)
        .ok();
    Line::new(Point::new(cx - 1, cy + 4), Point::new(cx + 6, cy - 4))
        .into_styled(style)
        .draw(c)
        .ok();
}

/// Transient brightness bar across the bottom rows (amber fill proportional to
/// level). Four rows, not seven: it lands on a live game frame, and the taller
/// bar swallowed the score line on frames that use the full panel height.
fn overlay_brightness_bar(buf: &mut [u32], brightness: u8) {
    let bar_h = 4usize;
    let y0 = FRAME_HEIGHT - bar_h;
    let fill = (brightness as usize * FRAME_WIDTH) / 100;
    for y in y0..FRAME_HEIGHT {
        for x in 0..FRAME_WIDTH {
            // The unfilled track is deliberately lit, but keep it FAINT: at
            // 0x333333 on a panel at full brightness it reads as a band of
            // stuck-on pixels rather than an empty track.
            buf[y * FRAME_WIDTH + x] = if x < fill { 0x00FF_CC00 } else { 0x0018_1818 };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DELIBERATE behavior change (twice over): double-press once toggled the
    /// pairing screen, then became a reserved no-op — now it TOGGLES the
    /// settings menu. It must still never alias to two sleep toggles, and it
    /// must queue nothing for the server, online or off.
    #[test]
    fn gestures_drive_mode() {
        let mut b = Board::new();
        b.mode = Mode::Running;

        // single press -> sleep (resolved after the double window)
        b.button_down(0);
        b.button_up(50);
        b.tick(500);
        assert_eq!(b.mode(), Mode::Asleep);

        // single press -> wake
        b.button_down(1000);
        b.button_up(1050);
        b.tick(1500);
        assert_eq!(b.mode(), Mode::Running);

        // double press: toggles the menu — no sleep flicker, no pairing
        // screen, nothing queued for the server.
        for (i, online) in [(0u64, false), (1, true)] {
            let t0 = 3000 + i * 3000;
            b.set_online(online);
            b.take_outgoing(); // drop the ButtonPress noise from the setup above
            b.button_down(t0);
            b.button_up(t0 + 40);
            b.button_down(t0 + 120);
            b.button_up(t0 + 160);
            b.tick(t0 + 160 + DOUBLE_WINDOW_MS + 10);
            assert!(b.menu_open(), "double press opens the menu (online={online})");
            assert_eq!(b.mode(), Mode::Running, "the mode machine is untouched (online={online})");
            assert!(
                b.take_outgoing().is_empty(),
                "a double press sends nothing (online={online})"
            );
            // …and the second double-tap closes it again.
            b.button_down(t0 + 1000);
            b.button_up(t0 + 1040);
            b.button_down(t0 + 1120);
            b.button_up(t0 + 1160);
            b.tick(t0 + 1160 + DOUBLE_WINDOW_MS + 10);
            assert!(!b.menu_open(), "double press closes the menu (online={online})");
            assert_eq!(b.mode(), Mode::Running, "still no sleep toggle (online={online})");
            assert!(b.take_outgoing().is_empty(), "closing sends nothing (online={online})");
        }
    }

    /// Animations are a function of WALL TIME, not tick count — ticking faster
    /// (an uncapped simulator repaint loop) must not speed up the sync spinner.
    #[test]
    fn animation_is_frame_rate_independent() {
        let mut b = Board::new();
        b.mode = Mode::Syncing; // the animated-dots screen
        b.tick(1000);
        let once = b.framebuffer();
        // Hammer tick at the SAME wall time (as a fast host would within one frame).
        for _ in 0..500 {
            b.tick(1000);
        }
        assert_eq!(once, b.framebuffer(), "extra ticks must not advance the animation");
        // Advancing wall time by one spinner step (133ms) DOES move it.
        b.tick(1133);
        assert_ne!(once, b.framebuffer(), "wall time advances the animation");
    }

    /// While ONLINE, a single press is the server's call: it goes to the outbox as
    /// ButtonPress and the local mode doesn't change (the next manifest will).
    /// Offline keeps the local sleep/wake toggle (tested above) as a fallback.
    #[test]
    fn online_press_reports_to_server_instead_of_local_sleep() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.button_down(0);
        b.button_up(50);
        b.tick(500);
        assert_eq!(b.mode(), Mode::Running, "server decides; no local sleep");
        let out = b.take_outgoing();
        assert!(
            matches!(out.as_slice(), [ClientMsg::ButtonPress]),
            "press should be queued for the server"
        );
        assert!(b.take_outgoing().is_empty(), "outbox drains once");
    }

    /// There is ONE brightness. Online knob turns are reported to the server
    /// (which owns the value); offline turns dim immediately and wait for a
    /// socket rather than going out on a dead one.
    #[test]
    fn knob_reports_brightness_when_online() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.encoder(-2); // 100 -> 80
        assert_eq!(b.brightness(), 80);
        let out = b.take_outgoing();
        assert!(
            matches!(out.as_slice(), [ClientMsg::DeviceBrightness { level: 80 }]),
            "turn should be queued for the server, got {out:?}"
        );

        b.set_online(false);
        b.encoder(1);
        assert_eq!(b.brightness(), 90, "offline turns still dim locally");
        assert!(b.take_outgoing().is_empty(), "nothing to send while the socket is down");
    }

    /// A knob turned while offline is still the user's intent, not a local-only
    /// dim: the FINAL level (one message, however many clicks) reaches the
    /// outbox once the socket returns, and the reconnect manifest must not
    /// revert the panel to the server's stale level on the way.
    #[test]
    fn offline_knob_turn_reaches_the_server_on_reconnect() {
        let m = |bright: u8| Manifest {
            countdown_ms: None,
            rotation: Vec::new(),
            display_on: true,
            off_state: Default::default(),
            epoch: 0,
            brightness: bright,
        };
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        b.on_manifest(m(100));
        b.set_online(false);

        b.encoder(-1); // 100 -> 90
        b.encoder(-1); // -> 80
        b.encoder(-1); // -> 70
        b.tick(1000);
        assert!(b.take_outgoing().is_empty(), "nothing goes out while offline");

        // The socket returns and the server's first manifest still carries its
        // OLD level — adopting it here would undo the turn in front of the user.
        b.set_online(true);
        b.on_manifest(m(100));
        assert_eq!(b.brightness(), 70, "the pending turn outranks a stale echo");
        b.tick(2000);
        let out = b.take_outgoing();
        assert!(
            matches!(out.as_slice(), [ClientMsg::DeviceBrightness { level: 70 }]),
            "one message with the final level, got {out:?}"
        );

        // Reported once; the server now owns 70 and its echo is adopted again.
        b.tick(3000);
        assert!(b.take_outgoing().is_empty(), "the flush doesn't repeat every tick");
        b.tick(10_000); // knob bar expired
        b.on_manifest(m(40));
        assert_eq!(b.brightness(), 40, "adoption resumes once the server has heard us");
    }

    /// The manifest's brightness is adopted as the local level — except while the
    /// knob's transient bar is up, so an echo in flight can't rubber-band a turn.
    #[test]
    fn manifest_brightness_adopted_unless_knob_active() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        // Online: manifests only arrive over a live socket, and an ONLINE turn
        // is reported straight away (an offline one holds adoption off until it
        // has been — see `offline_knob_turn_reaches_the_server_on_reconnect`).
        b.set_online(true);
        let m = |bright: u8| Manifest {
            countdown_ms: None,
            rotation: Vec::new(),
            display_on: true,
            off_state: Default::default(),
            epoch: 0,
            brightness: bright,
        };
        b.on_manifest(m(40));
        assert_eq!(b.brightness(), 40, "server value adopted when idle");

        b.tick(1000);
        b.encoder(1); // 40 -> 50, bar up
        b.on_manifest(m(40)); // stale echo racing the turn
        assert_eq!(b.brightness(), 50, "mid-turn manifests must not rubber-band");

        b.tick(10_000); // bar expired
        b.on_manifest(m(70));
        assert_eq!(b.brightness(), 70, "adoption resumes once the knob settles");
    }

    /// A same-length manifest is the same lineup with refreshed pixels (scores
    /// changed), so the board keeps its rotation slot and dwell timer instead of
    /// restarting at game 1 on every server poll. A different length is a new
    /// rotation and starts from the top.
    #[test]
    fn same_shape_manifest_keeps_rotation_position() {
        use scoreboard_protocol::RotationEntry;
        let mk = |ids: &[&str]| Manifest {
            countdown_ms: None,
            rotation: ids
                .iter()
                .map(|i| RotationEntry { frame_id: (*i).into(), dwell_ms: 1000, scroll: false })
                .collect(),
            display_on: true,
            off_state: Default::default(),
            epoch: 0,
            brightness: 100,
        };
        // Solid-color frames; the colors survive the RGB565 round-trip exactly.
        let solid = |r: u8, g: u8, b: u8| -> Vec<u8> {
            let px = scoreboard_protocol::rgb888_to_565(r, g, b).to_le_bytes();
            (0..FRAME_PIXELS).flat_map(|_| px).collect()
        };
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        b.on_manifest(mk(&["a", "b", "c"]));
        b.on_frame("a".into(), solid(255, 0, 0));
        b.on_frame("b".into(), solid(0, 255, 0));
        b.on_frame("c".into(), solid(0, 0, 255));
        b.tick(1200); // dwell elapsed -> slot 1
        assert_eq!(b.framebuffer()[0], 0x00FF00, "should have advanced to slot 1");

        // Same-length refresh with new frame ids: slot and dwell timer kept.
        b.on_manifest(mk(&["d", "e", "f"]));
        b.on_frame("d".into(), solid(255, 0, 0));
        b.on_frame("e".into(), solid(255, 255, 255));
        b.on_frame("f".into(), solid(0, 0, 255));
        b.tick(1300); // dwell NOT elapsed since the slot-1 advance
        assert_eq!(b.framebuffer()[0], 0xFFFFFF, "slot 1 kept across a same-shape refresh");

        // Different length: genuinely new rotation, restart at the top.
        b.on_manifest(mk(&["g", "h"]));
        b.on_frame("g".into(), solid(0, 255, 0));
        b.on_frame("h".into(), solid(0, 0, 255));
        b.tick(1400);
        assert_eq!(b.framebuffer()[0], 0x00FF00, "new-shape rotation restarts at slot 0");
    }

    /// A frame taller than the panel scrolls one full pass per dwell, in
    /// fifths: hold, glide down, hold, glide up, hold — ending at rest so the
    /// advance never cuts mid-glide. Pure clock math: simulator and firmware
    /// animate identically.
    #[test]
    fn tall_frames_scroll_with_pingpong() {
        use scoreboard_protocol::RotationEntry;
        // Distinct solid color per SAMPLED row: red = (8y+8) mod 256. Rows past
        // 31 wrap, but the test only ever samples rows 0..=8 (max_off = 8).
        let tall_h = 40usize; // max_off = 8
        let px: Vec<u8> = (0..tall_h)
            .flat_map(|y| {
                let row = scoreboard_protocol::rgb888_to_565(((y * 8 + 8) % 256) as u8, 0, 0).to_le_bytes();
                core::iter::repeat(row).take(FRAME_WIDTH).flatten().collect::<Vec<u8>>()
            })
            .collect();
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        b.on_manifest(Manifest {
            countdown_ms: None,
            rotation: vec![RotationEntry {
                frame_id: "tall".into(),
                dwell_ms: 10_000, // fifths: 2s hold / 2s down / 2s hold / 2s up / 2s hold
                scroll: true,
            }],
            display_on: true,
            off_state: Default::default(),
            epoch: 0,
            brightness: 100,
        });
        b.on_frame("tall".into(), px);
        // Recover the row index from the red channel (>>3 strips the 565
        // round-trip's replicated low bits).
        let top_row = |b: &Board| ((b.framebuffer()[0] >> 16 & 0xFF) >> 3) as usize;

        b.tick(1_000); // mid top-hold
        let at_top = top_row(&b);
        b.tick(3_000); // glide down, halfway (max_off 8 -> offset 4)
        assert_eq!(top_row(&b), at_top + 4, "halfway down mid-glide");
        b.tick(5_000); // bottom hold
        assert_eq!(top_row(&b), at_top + 8, "held at the bottom");
        b.tick(7_000); // glide up, halfway
        assert_eq!(top_row(&b), at_top + 4, "halfway back up");
        b.tick(9_500); // final top hold: at rest before the advance
        assert_eq!(top_row(&b), at_top, "back at the top before the dwell ends");

        // Without the scroll flag a tall frame just shows its top window.
        b.on_manifest(Manifest {
            countdown_ms: None,
            rotation: vec![RotationEntry { frame_id: "tall".into(), dwell_ms: 10_000, scroll: false }],
            display_on: true,
            off_state: Default::default(),
            epoch: 0,
            brightness: 100,
        });
        b.tick(13_000);
        assert_eq!(top_row(&b), at_top, "no flag -> static top window");
    }

    /// Timings DELIBERATELY reworked again: the hold is erase-only now (the
    /// menu moved to double-tap), so the wipe is back at the original 10s —
    /// the 15s existed only to fit the hold's 3s open-the-menu stop. Amber
    /// warm-up to 2s, red run to 10s, and the hold NEVER opens the menu. The
    /// latch semantics are unchanged.
    #[test]
    fn hold_fills_then_triggers_reset() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.button_down(0);
        b.tick(300); // within the grace window: nothing yet
        assert!(!b.reset_active());
        b.tick(800); // past grace (500): amber warm-up bar is filling
        assert!(b.reset_active());
        assert!(!b.hold_erase_phase);
        assert!(!b.reset_triggered());
        b.tick(MENU_CLOSE_HOLD_MS + 500); // past 2s: the bar turned red
        assert!(!b.menu_open(), "an erase-only hold never opens the menu");
        assert!(b.hold_erase_phase, "past 2s the hold runs toward the erase");
        assert!(!b.reset_triggered());
        b.tick(ERASE_HOLD_MS - 100); // just shy of 10s: still not erased
        assert!(!b.reset_triggered());
        assert!(!b.menu_open(), "no menu at any point of the hold");
        b.tick(ERASE_HOLD_MS + 100); // the hold completes
        assert!(b.reset_triggered());
        // Releasing after completion keeps it latched (the firmware reboots us).
        b.button_up(ERASE_HOLD_MS + 200);
        assert!(b.reset_triggered());
    }

    #[test]
    fn releasing_early_cancels_reset() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.button_down(0);
        b.tick(900); // bar showing
        assert!(b.reset_active());
        b.button_up(1200); // released in the amber warm-up
        assert!(!b.reset_active());
        assert!(!b.reset_triggered());
        assert!(!b.menu_open(), "a hold never opens the menu (double-tap does)");
        // A long-ish hold no longer enters Setup (that's the app's job now).
        assert_eq!(b.mode(), Mode::Running);
    }

    // ---- settings-menu helpers: the knob moves as the host delivers them ----

    /// Double-tap (two quick press/release pairs inside the double window) —
    /// the menu toggle. Returns a time safely after the gesture resolved.
    /// The notice countdown bar is the DEVICE's animation: one server frame,
    /// smooth local fill anchored per epoch — a same-epoch re-push (brightness
    /// nudge mid-notice) must not restart it; a new epoch must.
    #[test]
    fn countdown_bar_fills_on_the_device_clock() {
        let mk = |epoch: u64| Manifest {
            rotation: vec![scoreboard_protocol::RotationEntry {
                frame_id: "n1".into(),
                dwell_ms: 6000,
                scroll: false,
            }],
            display_on: true,
            off_state: Default::default(),
            brightness: 100,
            epoch,
            countdown_ms: Some(4000),
        };
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        b.on_manifest(mk(7));
        b.on_frame("n1".into(), vec![0u8; scoreboard_protocol::FRAME_BYTES]);
        let lit = |b: &Board| {
            let f = b.framebuffer();
            (27..30).map(|y| (5..59).filter(|&x| f[y * FRAME_WIDTH + x] != 0).count()).max().unwrap()
        };
        b.tick(0);
        assert_eq!(lit(&b), 0, "bar starts empty");
        b.tick(2000);
        let half = lit(&b);
        assert!((25..=29).contains(&half), "half the notice, about half the bar: {half}");
        // Same-epoch re-push mid-notice: the anchor (and the fill) hold.
        b.on_manifest(mk(7));
        b.on_frame("n1".into(), vec![0u8; scoreboard_protocol::FRAME_BYTES]);
        b.tick(2100);
        assert!(lit(&b) >= half, "same-epoch re-push must not restart the fill");
        b.tick(4000);
        assert_eq!(lit(&b), 54, "lands exactly full");
        // A NEW notice (new epoch) starts from empty again.
        b.on_manifest(mk(8));
        b.on_frame("n1".into(), vec![0u8; scoreboard_protocol::FRAME_BYTES]);
        b.tick(4100);
        assert!(lit(&b) <= 2, "a new epoch re-anchors the bar");
    }

    /// First startup: the server flags an unpaired board and the pair code
    /// goes up unprompted, sticky past the normal timeout, until pairing
    /// completes or the owner taps it away.
    #[test]
    fn needs_pairing_shows_a_sticky_code_screen() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        b.needs_pairing();
        assert_eq!(b.mode(), Mode::Syncing, "code up unprompted");
        // Way past the normal pair-screen timeout: still up.
        b.tick(600_000);
        assert_eq!(b.mode(), Mode::Syncing, "sticky: no timeout while unpaired");
        // Pairing completes -> back to normal.
        b.setup_done();
        assert_eq!(b.mode(), Mode::Running);

        // The owner can also tap it away ("I'll pair later").
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        b.needs_pairing();
        tap(&mut b, 1000);
        assert_eq!(b.mode(), Mode::Running, "tap dismisses the sticky code");
        // And a dismissed screen does not come back on its own.
        b.tick(700_000);
        assert_eq!(b.mode(), Mode::Running);

        // The menu is never fought: needs_pairing while the menu is up defers.
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        double_tap(&mut b, 1000);
        b.needs_pairing();
        assert!(b.menu_open(), "menu wins");
        assert_eq!(b.mode(), Mode::Running, "no mode change underneath");
    }

    fn double_tap(b: &mut Board, t0: u64) -> u64 {
        b.button_down(t0);
        b.button_up(t0 + 40);
        b.button_down(t0 + 120);
        b.button_up(t0 + 160);
        b.tick(t0 + 200);
        t0 + 300
    }

    /// Hold across the 2s close threshold, then release.
    fn hold_to_close(b: &mut Board, t0: u64) -> u64 {
        b.button_down(t0);
        b.tick(t0 + MENU_CLOSE_HOLD_MS + 50);
        b.button_up(t0 + MENU_CLOSE_HOLD_MS + 100);
        t0 + MENU_CLOSE_HOLD_MS + 200
    }

    /// A quick tap, ticked past the double window so it resolves as Single.
    fn tap(b: &mut Board, t0: u64) -> u64 {
        b.button_down(t0);
        b.button_up(t0 + 50);
        b.tick(t0 + 50 + DOUBLE_WINDOW_MS + 10);
        t0 + 50 + DOUBLE_WINDOW_MS + 20
    }

    fn sim_diag() -> Diagnostics {
        Diagnostics {
            ssid: Some("SimNet".into()),
            rssi_dbm: Some(-55),
            ip: Some([192, 168, 1, 42]),
            server: Some("127.0.0.1:7878".into()),
            board_id_prefix: Some("ab12cd34".into()),
            free_heap_bytes: Some(123_456),
        }
    }

    /// Double-tap opens the menu from every state, and closing (double-tap
    /// again, or the 2s hold) restores each one EXACTLY — the mode machine
    /// never notices the visit.
    #[test]
    fn double_tap_opens_menu_everywhere_and_closing_restores_the_prior_state() {
        // Running; closed with the second double-tap.
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        let t = double_tap(&mut b, 1000);
        assert!(b.menu_open());
        assert_eq!(b.mode(), Mode::Running, "the mode machine is untouched underneath");
        double_tap(&mut b, t);
        assert!(!b.menu_open());
        assert_eq!(b.mode(), Mode::Running);

        // Asleep stays asleep; closed with the 2s hold this time.
        let mut b = Board::new();
        b.mode = Mode::Asleep;
        b.tick(0);
        let t = double_tap(&mut b, 1000);
        assert!(b.menu_open());
        assert!(b.framebuffer().iter().any(|&p| p != 0), "the menu lights an asleep panel");
        hold_to_close(&mut b, t);
        assert!(!b.menu_open());
        assert_eq!(b.mode(), Mode::Asleep, "closing leaves the board asleep");
        assert!(b.framebuffer().iter().all(|&p| p == 0), "asleep = dark again");

        // The connect screen (Running, offline, nothing cached): pixel-exact
        // restore, checked against a board that never saw the menu.
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        let t = double_tap(&mut b, 1000);
        assert!(b.menu_open());
        let t = double_tap(&mut b, t);
        assert!(!b.menu_open());
        b.tick(t);
        let mut reference = Board::new();
        reference.mode = Mode::Running;
        reference.tick(0);
        reference.tick(t);
        assert_eq!(b.framebuffer(), reference.framebuffer(), "connect screen exactly as before");

        // Provisioning: the setup screen survives a menu visit too (the
        // diagnostics matter exactly when someone is stuck here).
        let mut b = Board::new();
        b.tick(0);
        b.gesture(Gesture::Long);
        let t = double_tap(&mut b, 1000);
        assert!(b.menu_open());
        hold_to_close(&mut b, t);
        assert_eq!(b.mode(), Mode::Setup, "still provisioning after the visit");
    }

    /// The 10s hold is the escape hatch: it must erase from Running and from a
    /// press that started INSIDE the menu (closing it at 2s on the way — one
    /// press, one timeline).
    #[test]
    fn hold_fires_the_reset_from_running_and_from_inside_the_menu() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        b.button_down(1000);
        b.tick(1000 + ERASE_HOLD_MS + 50);
        assert!(b.reset_triggered());

        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        let t = double_tap(&mut b, 1000);
        assert!(b.menu_open());
        b.button_down(t);
        b.tick(t + MENU_CLOSE_HOLD_MS + 50);
        assert!(!b.menu_open(), "the 2s crossing closed the menu");
        b.tick(t + MENU_CLOSE_HOLD_MS + 1000);
        assert!(!b.menu_open(), "the continued hold never reopens the menu");
        assert!(b.hold_erase_phase, "…it runs on toward the erase instead");
        b.tick(t + ERASE_HOLD_MS + 50);
        assert!(b.reset_triggered(), "10s from press start wipes, wherever it began");
    }

    /// Letting go anywhere in the red phase erases nothing — the wipe fires
    /// from `tick` at 10s, never on release. A menu-origin press's 2s close
    /// sticks (releasing later must not reopen anything).
    #[test]
    fn releasing_in_the_red_phase_erases_nothing() {
        // Plain press from Running: red phase, release, nothing happened.
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        b.button_down(1000);
        b.tick(1000 + MENU_CLOSE_HOLD_MS + 2000); // deep in the red phase
        assert!(b.hold_erase_phase);
        assert!(!b.menu_open(), "an erase hold opens no menu");
        b.button_up(1000 + MENU_CLOSE_HOLD_MS + 2500);
        assert!(!b.reset_triggered(), "only the full 10s erases");
        assert!(!b.menu_open());
        assert!(!b.reset_active(), "the red bar is gone");
        assert_eq!(b.mode(), Mode::Running, "…and the board is exactly as it was");

        // Menu-origin press: the menu closed at 2s and stays closed.
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        let t = double_tap(&mut b, 1000);
        assert!(b.menu_open());
        b.button_down(t);
        b.tick(t + MENU_CLOSE_HOLD_MS + 2000);
        assert!(b.hold_erase_phase);
        b.button_up(t + MENU_CLOSE_HOLD_MS + 2500);
        assert!(!b.reset_triggered(), "only the full 10s erases");
        assert!(!b.menu_open(), "the 2s close sticks through the release");
        assert!(!b.reset_active());
    }

    /// Two minutes of an untouched knob closes an abandoned menu (device
    /// clock), and any interaction restarts the window.
    #[test]
    fn menu_times_out_after_two_minutes_of_no_interaction() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        let t = double_tap(&mut b, 1000);
        b.tick(t);
        b.encoder(1); // stamps the interaction clock at `t`
        b.tick(t + MENU_IDLE_TIMEOUT_MS - 100);
        assert!(b.menu_open(), "just inside the window");
        b.tick(t + MENU_IDLE_TIMEOUT_MS + 100);
        assert!(!b.menu_open(), "an abandoned menu closes itself");
        assert_eq!(b.mode(), Mode::Running);
    }

    /// The knob is the whole navigation story: selection moves and clamps on
    /// the root list, and scrolls (with clamps) on the diagnostics screen.
    /// Signs follow the owner-tuned inversion (see
    /// `menu_selection_direction_is_pinned`): NEGATIVE deltas walk down/scroll
    /// down.
    #[test]
    fn rotate_moves_the_selection_and_clamps_and_scrolls() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_diagnostics(sim_diag());
        b.tick(0);
        let t = double_tap(&mut b, 1000);
        b.tick(t);
        let top = b.framebuffer();
        b.encoder(1);
        assert_eq!(b.framebuffer(), top, "selection clamps at the top");
        b.encoder(-1);
        assert_ne!(b.framebuffer(), top, "rotation moves the selection");
        b.encoder(-10);
        let last = b.framebuffer();
        b.encoder(-1);
        assert_eq!(b.framebuffer(), last, "selection clamps at the bottom");

        // Up one to DEBUG INFO (row 3 of 0..=4) and scroll the list.
        b.encoder(1);
        tap(&mut b, t + 1000);
        let page_top = b.framebuffer();
        b.encoder(1);
        assert_eq!(b.framebuffer(), page_top, "scroll clamps at the top");
        b.encoder(-1);
        assert_ne!(b.framebuffer(), page_top, "rotation scrolls the diagnostics");
        b.encoder(-30);
        let bottom = b.framebuffer();
        b.encoder(-1);
        assert_eq!(b.framebuffer(), bottom, "scroll clamps at the bottom");
        b.encoder(30);
        assert_eq!(b.framebuffer(), page_top, "and scrolls back to the top");
    }

    /// Direction pin (owner-tuned): in the MENU the knob is deliberately the
    /// INVERSE of the brightness mapping — `encoder(+1)` raises brightness,
    /// but in the menu it moves the selection UP/back, and `encoder(-1)`
    /// walks DOWN the list. The owner reports the same-sign mapping felt
    /// backwards at the physical knob; this test exists so no refactor can
    /// silently flip it back.
    #[test]
    fn menu_selection_direction_is_pinned() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        // Outside the menu: +1 raises brightness (the reference direction).
        b.set_brightness(50);
        b.encoder(1);
        assert_eq!(b.brightness(), 60, "encoder(+1) raises brightness");

        let t = double_tap(&mut b, 1000);
        b.tick(t);
        // EXIT (row 0) is selected; -1 must move DOWN to UPDATE (row 1):
        // with an offer armed, selecting it opens the CONFIRM screen — which
        // proves where the selection sat (EXIT would have closed the menu).
        b.set_update_available(Some("9.9.9".into()));
        b.encoder(-1);
        tap(&mut b, t + 500);
        assert!(b.menu_open(), "encoder(-1) walked EXIT -> UPDATE (down the list)");
        assert!(
            b.take_outgoing().is_empty(),
            "the confirm screen, not the request, answers the first tap"
        );

        // Back out of the confirm (CANCEL is the default), then +1 walks back
        // UP to EXIT: the next tap closes the menu.
        b.gesture(Gesture::Single);
        b.encoder(1);
        b.gesture(Gesture::Single);
        assert!(!b.menu_open(), "encoder(+1) walked UPDATE -> EXIT (up the list)");
    }

    /// RESET BOARD arms nothing by itself: CANCEL is the default every visit,
    /// backs out to the root, and only rotate-to-ERASE + tap runs the wipe.
    #[test]
    fn reset_confirm_defaults_to_cancel_and_cancel_backs_out() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        let t = double_tap(&mut b, 1000);
        b.encoder(-10); // down to the RESET BOARD row (clamped at the bottom)
        let t = tap(&mut b, t + 500); // enter the confirm screen
        let cancel_screen = b.framebuffer();
        let t = tap(&mut b, t + 500); // tap the default (CANCEL)
        assert!(b.menu_open(), "CANCEL leaves the menu open");
        assert!(!b.reset_triggered(), "…and erases nothing");
        // Re-enter: still CANCEL by default (a backed-out ERASE doesn't stick).
        let t = tap(&mut b, t + 500);
        assert_eq!(b.framebuffer(), cancel_screen, "every visit starts on CANCEL");
        b.encoder(-1); // flip to ERASE (the down-the-list direction)
        assert_ne!(b.framebuffer(), cancel_screen, "ERASE selection renders differently");
        tap(&mut b, t + 500);
        assert!(b.reset_triggered(), "ERASE + tap runs the one wipe path");
        assert!(!b.menu_open());
    }

    /// The UPDATE row: inert with nothing offered (it reads UP TO DATE), one
    /// The update sits behind a confirm screen (the panel goes dark for the
    /// whole install — "DO NOT UNPLUG" must be read before anything happens):
    /// first tap opens the confirm with CANCEL armed, cancelling is free, and
    /// only rotate-to-UPDATE + tap queues `RequestUpdate` (offline-safe via
    /// the outbox) and closes the menu for the blank to take over.
    #[test]
    fn update_requires_a_second_confirmation() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        let t = double_tap(&mut b, 1000);
        b.take_outgoing();
        b.encoder(-1); // EXIT is the default selection; step down to UPDATE
        let t = tap(&mut b, t);
        assert!(b.menu_open(), "an inert UPDATE tap must not exit the menu");
        assert!(b.take_outgoing().is_empty(), "no offer, no confirm, no message");

        // With an offer: tap opens the confirm; nothing is queued yet.
        b.set_update_available(Some("2026.9.9".into()));
        let t = tap(&mut b, t);
        assert!(b.take_outgoing().is_empty(), "the confirm gate holds the request");

        // CANCEL is the default: a plain tap backs out to the root, queueless.
        let t = tap(&mut b, t);
        assert!(b.menu_open() && b.take_outgoing().is_empty(), "cancel is free");

        // Re-enter, arm UPDATE (down-the-list direction), confirm: the request
        // queues and the menu closes so the OTA blank owns the panel.
        let t = tap(&mut b, t);
        b.encoder(-1);
        tap(&mut b, t);
        let out = b.take_outgoing();
        assert!(matches!(out.as_slice(), [ClientMsg::RequestUpdate]), "got {out:?}");
        assert!(!b.menu_open(), "confirmed update closes the menu");

        // And the confirm re-defaults to CANCEL on every fresh visit.
        let t2 = double_tap(&mut b, 500_000);
        b.encoder(-1);
        let t2 = tap(&mut b, t2);
        tap(&mut b, t2);
        assert!(b.menu_open(), "a stale UPDATE selection must not survive re-entry");
    }

    /// The UPDATE row's two renderings: "UPDATE ^<version>" with an offer on
    /// the table, "UP TO DATE" without one — never the running version, which
    /// read as a tappable update and confused people.
    #[test]
    fn update_row_reads_up_to_date_without_an_offer() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_version("2026.7.11".into());
        b.tick(0);
        assert_eq!(b.update_row_label(), "UP TO DATE");
        b.set_update_available(Some("2026.9.9".into()));
        assert_eq!(b.update_row_label(), "UPDATE ^2026.9.9");

        // And the two states render visibly differently on the root screen.
        let t = double_tap(&mut b, 1000);
        b.tick(t);
        let offered = b.framebuffer();
        b.set_update_available(None);
        assert_ne!(b.framebuffer(), offered, "the row changes with the offer");
    }

    /// While the menu is up the knob navigates: brightness must not move, no
    /// DeviceBrightness goes out, and closing flushes no phantom level. The
    /// first turn AFTER closing dims again.
    #[test]
    fn brightness_does_not_move_while_the_menu_is_open() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        let t = double_tap(&mut b, 1000);
        b.take_outgoing();
        b.tick(t);
        b.encoder(-2);
        assert_eq!(b.brightness(), 100, "the knob navigates; it must not dim");
        b.tick(t + 100);
        assert!(b.take_outgoing().is_empty(), "and reports no DeviceBrightness");
        let t = hold_to_close(&mut b, t + 200);
        b.tick(t);
        assert!(b.take_outgoing().is_empty(), "closing flushes no phantom level");
        b.encoder(-1);
        assert_eq!(b.brightness(), 90, "the first turn after closing dims again");
        let out = b.take_outgoing();
        assert!(matches!(out.as_slice(), [ClientMsg::DeviceBrightness { level: 90 }]));
    }

    /// PAIR CODE inside the menu is the very screen `Mode::Syncing` owns —
    /// pixel for pixel — with `pairing_active` telling the host to announce a
    /// code, and tap / SetupDone both landing back on the root.
    #[test]
    fn menu_pair_code_reuses_the_syncing_screen_and_taps_back() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        let t = double_tap(&mut b, 1000);
        b.encoder(-2); // down from EXIT to the PAIR CODE row
        let t = tap(&mut b, t);
        assert!(b.pairing_active(), "the host should announce a code now");
        let mut reference = Board::new();
        reference.tick(0);
        reference.mode = Mode::Syncing;
        reference.tick(b.clock);
        assert_eq!(
            b.framebuffer(),
            reference.framebuffer(),
            "same screen the double-press used to own"
        );
        // Paired via the app: SetupDone lands back on the menu root.
        b.setup_done();
        assert!(b.menu_open(), "still mid-menu-visit");
        assert!(!b.pairing_active(), "pairing over, announcement off");
        // A plain tap backs out of the code screen too.
        let t = tap(&mut b, t + 500); // selection is still PAIR CODE: re-enter
        assert!(b.pairing_active());
        tap(&mut b, t + 500);
        assert!(!b.pairing_active());
        assert!(b.menu_open(), "tap returns to the menu, not out of it");
    }

    /// The three BLE-provisioning phases render distinct screens — the firmware
    /// flips the phase as the handshake progresses, and each step must be
    /// visible on the panel.
    #[test]
    fn setup_phases_render_distinct_screens() {
        let mut b = Board::new();
        b.tick(0);
        b.gesture(Gesture::Long);
        assert_eq!(b.mode(), Mode::Setup);
        let bt = b.framebuffer();
        b.set_setup_phase(SetupPhase::WaitingCreds);
        let creds = b.framebuffer();
        b.set_setup_phase(SetupPhase::Saved);
        let saved = b.framebuffer();
        assert_ne!(bt, creds, "BLUETOOTH vs WI-FI wait screens must differ");
        assert_ne!(creds, saved, "WI-FI wait vs saved screens must differ");
        assert_ne!(bt, saved, "BLUETOOTH wait vs saved screens must differ");
    }

    /// A board parked in provisioning must keep visibly moving (the growing
    /// ellipsis) so it never looks hung while the user fiddles with the app.
    #[test]
    fn setup_screen_animates() {
        let mut b = Board::new();
        b.tick(0);
        b.gesture(Gesture::Long);
        b.tick(1000);
        let before = b.framebuffer();
        b.tick(1500); // ~500ms later: the ellipsis has grown a dot
        assert_ne!(before, b.framebuffer(), "wall time advances the setup animation");
    }

    /// Offline in Running mode shows the connect-progress screen; the Wi-Fi and
    /// server legs render differently so the animation matches the label.
    #[test]
    fn connect_screen_distinguishes_wifi_from_server_leg() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(1000); // online defaults false -> connect screen
        b.set_wifi(false);
        let joining = b.framebuffer();
        b.set_wifi(true);
        let finding = b.framebuffer();
        assert_ne!(joining, finding, "wifi state must change the connect screen");
    }

    /// The traveling dot on the connect screen must keep moving — a frozen leg
    /// reads as a hung board, exactly what these screens exist to prevent.
    #[test]
    fn connect_screen_animates() {
        for wifi in [false, true] {
            let mut b = Board::new();
            b.mode = Mode::Running;
            b.set_wifi(wifi);
            b.tick(1000);
            let before = b.framebuffer();
            b.tick(1400); // ~400ms later: the leg dot has advanced a step
            assert_ne!(
                before,
                b.framebuffer(),
                "wall time advances the connect animation (wifi={wifi})"
            );
        }
    }

    /// A brief ws drop must not flash the connect screen: with content up, the
    /// board holds it through the ~5s grace window and only then falls back to
    /// connect-progress. Repeated offline reports from the host don't restart
    /// the window — it's timed from the true->false transition.
    #[test]
    fn offline_grace_holds_content_then_shows_connect() {
        use scoreboard_protocol::RotationEntry;
        let solid = |r: u8, g: u8, b: u8| -> Vec<u8> {
            let px = scoreboard_protocol::rgb888_to_565(r, g, b).to_le_bytes();
            (0..FRAME_PIXELS).flat_map(|_| px).collect()
        };
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        b.on_manifest(Manifest {
            countdown_ms: None,
            rotation: vec![RotationEntry { frame_id: "a".into(), dwell_ms: 1000, scroll: false }],
            display_on: true,
            off_state: Default::default(),
            epoch: 0,
            brightness: 100,
        });
        b.on_frame("a".into(), solid(255, 0, 0));
        b.tick(1000);
        let content = b.framebuffer();
        assert_eq!(content[0], 0xFF0000, "content frame up while online");

        // The connection drops: the content stays put through the window…
        b.set_online(false);
        assert_eq!(b.framebuffer(), content, "right after the drop: content held");
        b.tick(3000);
        assert_eq!(b.framebuffer(), content, "mid-window: still held");
        b.set_online(false); // host re-reports offline — must NOT extend the window
        // …right up to the edge (drop was at 1000; window is OFFLINE_GRACE_MS)…
        b.tick(1000 + OFFLINE_GRACE_MS - 100);
        assert_eq!(b.framebuffer(), content, "just inside the window: still held");
        // …and just past it the outage is real: connect-progress screen.
        b.tick(1000 + OFFLINE_GRACE_MS + 100);
        let mut reference = Board::new(); // never-online board = pure connect screen
        reference.mode = Mode::Running;
        reference.tick(1000 + OFFLINE_GRACE_MS + 100);
        assert_eq!(b.framebuffer(), reference.framebuffer(), "grace expired -> connect screen");
    }

    /// A deliberately dark panel (display_on: false — the server always ships
    /// that with an empty rotation) stays dark for as long as the outage lasts.
    /// Power intent is not a cache that expires: a board the user turned OFF
    /// must never light the bedroom up with the connect screen, whether the
    /// server is gone for two seconds or all night.
    #[test]
    fn power_off_survives_the_whole_outage() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        b.on_manifest(Manifest {
            countdown_ms: None,
            rotation: vec![],
            display_on: false,
            off_state: Default::default(),
            epoch: 0,
            brightness: 100,
        });
        b.tick(1000);
        let dark = b.framebuffer();
        assert!(dark.iter().all(|&p| p == 0), "asleep panel is black");
        b.set_online(false);
        b.tick(2000);
        assert_eq!(b.framebuffer(), dark, "blip mid-sleep: stays dark");
        b.tick(1000 + OFFLINE_GRACE_MS + 100);
        assert_eq!(b.framebuffer(), dark, "grace expired: an OFF board stays off");
        b.tick(6 * 3_600_000); // six hours of outage
        assert_eq!(b.framebuffer(), dark, "hours later: still off");
    }

    /// A same-LENGTH manifest with a different EPOCH is a different program
    /// (e.g. a 4-step notice sweep landing over a 4-game rotation): the board
    /// must restart at entry 0 instead of keeping its old slot mid-sweep.
    #[test]
    fn epoch_change_restarts_a_same_length_rotation() {
        use scoreboard_protocol::RotationEntry;
        let solid = |r: u8, g: u8, b: u8| -> Vec<u8> {
            let px = scoreboard_protocol::rgb888_to_565(r, g, b).to_le_bytes();
            (0..FRAME_PIXELS).flat_map(|_| px).collect()
        };
        let mk = |ids: &[&str], epoch: u64| Manifest {
            countdown_ms: None,
            rotation: ids
                .iter()
                .map(|id| RotationEntry { frame_id: (*id).into(), dwell_ms: 1000, scroll: false })
                .collect(),
            display_on: true,
            off_state: Default::default(),
            epoch,
            brightness: 100,
        };
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        b.on_manifest(mk(&["a", "b"], 0));
        b.on_frame("a".into(), solid(255, 0, 0));
        b.on_frame("b".into(), solid(0, 255, 0));
        b.tick(1500); // past the first dwell -> slot 1 (green)
        assert_eq!(b.framebuffer()[0], 0x00FF00, "advanced to slot 1");

        // Same length, same epoch: slot kept (the score-tick refresh case).
        b.on_manifest(mk(&["c", "d"], 0));
        b.on_frame("c".into(), solid(0, 0, 255));
        b.on_frame("d".into(), solid(255, 255, 255));
        assert_eq!(b.framebuffer()[0], 0xFFFFFF, "same epoch: slot 1 kept");

        // Same length, NEW epoch: a different program — restart at slot 0.
        b.on_manifest(mk(&["e", "f"], 7));
        b.on_frame("e".into(), solid(255, 255, 0));
        b.on_frame("f".into(), solid(0, 255, 255));
        assert_eq!(b.framebuffer()[0], 0xFFFF00, "new epoch: restarted at slot 0");
    }

    /// With nothing cached (fresh boot) there is no content to hold, so going
    /// offline shows the connect screen immediately — no blank grace window.
    #[test]
    fn offline_without_content_shows_connect_immediately() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(1000);
        b.set_online(false);
        let mut reference = Board::new();
        reference.mode = Mode::Running;
        reference.tick(1000);
        assert_eq!(
            b.framebuffer(),
            reference.framebuffer(),
            "no content: connect screen with no grace delay"
        );
    }

    /// An OTA update BLANKS the panel to all-black (a black panel can't strobe
    /// as each flash write stalls the render task): it overrides Running content
    /// at any progress value, and `None` clears it back to the exact frame.
    #[test]
    fn updating_blanks_panel_overrides_and_clears() {
        use scoreboard_protocol::RotationEntry;
        let solid = |r: u8, g: u8, b: u8| -> Vec<u8> {
            let px = scoreboard_protocol::rgb888_to_565(r, g, b).to_le_bytes();
            (0..FRAME_PIXELS).flat_map(|_| px).collect()
        };
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        b.on_manifest(Manifest {
            countdown_ms: None,
            rotation: vec![RotationEntry { frame_id: "a".into(), dwell_ms: 1000, scroll: false }],
            display_on: true,
            off_state: Default::default(),
            epoch: 0,
            brightness: 100,
        });
        b.on_frame("a".into(), solid(255, 0, 0));
        b.tick(100);
        let running = b.framebuffer();
        assert_eq!(running[0], 0xFF0000, "content up before the update");

        // Updating overrides the running content with an all-black frame.
        b.set_updating(Some(0));
        assert!(
            b.framebuffer().iter().all(|&p| p == 0),
            "updating blanks the panel to black, overriding Running content"
        );
        // The stored value is vestigial — the panel is black at any progress.
        b.set_updating(Some(50));
        assert!(b.framebuffer().iter().all(|&p| p == 0), "still black at 50%");
        b.set_updating(Some(100));
        assert!(b.framebuffer().iter().all(|&p| p == 0), "still black at 100%");

        // A failed download clears the overlay: back to the exact running frame.
        b.set_updating(None);
        assert_eq!(b.framebuffer(), running, "None restores the running content");
    }

    /// Updating blanks the panel and overrides every mode below reset — but
    /// hold-to-reset still WINS: the lit escape hatch must never be hidden, even
    /// mid-download.
    #[test]
    fn reset_overlay_beats_updating_blank() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        b.set_updating(Some(50));
        // Updating alone blanks the panel to black.
        assert!(
            b.framebuffer().iter().all(|&p| p == 0),
            "updating blanks the panel when no reset is held"
        );

        let mut reference = Board::new();
        reference.mode = Mode::Running;
        reference.tick(0);
        // Same hold on both boards; the one mid-update must render the (lit)
        // reset screen, identical to the board that isn't updating.
        b.button_down(0);
        reference.button_down(0);
        b.tick(900); // past the grace window: reset bar up
        reference.tick(900);
        assert!(b.reset_active());
        assert!(
            b.framebuffer().iter().any(|&p| p != 0),
            "hold-to-reset lights up over the updating blank"
        );
        assert_eq!(
            b.framebuffer(),
            reference.framebuffer(),
            "hold-to-reset renders over the updating blank, identical to no update"
        );
    }

    /// The boot screen carries the firmware version once the host reports it.
    #[test]
    fn boot_screen_shows_version() {
        let mut plain = Board::new();
        plain.tick(0);
        let without = plain.framebuffer();

        let mut b = Board::new();
        b.set_version("2026.7.0".into());
        b.tick(0);
        assert_eq!(b.mode(), Mode::Booting);
        assert_ne!(b.framebuffer(), without, "version line changes the boot screen");
    }

    /// A press with no socket sleeps the panel locally AND tells the server the
    /// moment there's a socket again — otherwise the board is dark while
    /// `cfg.power` reads true and the app's toggle is a confirmed no-op.
    #[test]
    fn offline_press_sleeps_locally_and_still_reports() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.button_down(0);
        b.button_up(50);
        b.tick(500);
        assert_eq!(b.mode(), Mode::Asleep, "offline press still works with no server");
        let out = b.take_outgoing();
        assert!(
            matches!(out.as_slice(), [ClientMsg::ButtonPress]),
            "the press must be queued so power converges on reconnect, got {out:?}"
        );
    }

    /// The server owns the panel: an arriving manifest ends a locally-toggled
    /// sleep, and its own `display_on` decides whether the board is dark. Without
    /// this the offline sleep survives every reconnect and only a second
    /// physical press recovers the board.
    #[test]
    fn manifest_ends_a_local_sleep() {
        use scoreboard_protocol::RotationEntry;
        let solid = |r: u8, g: u8, b: u8| -> Vec<u8> {
            let px = scoreboard_protocol::rgb888_to_565(r, g, b).to_le_bytes();
            (0..FRAME_PIXELS).flat_map(|_| px).collect()
        };
        let mk = |on: bool| Manifest {
            countdown_ms: None,
            rotation: if on {
                vec![RotationEntry { frame_id: "a".into(), dwell_ms: 1000, scroll: false }]
            } else {
                vec![]
            },
            display_on: on,
            off_state: Default::default(),
            epoch: 0,
            brightness: 100,
        };
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        b.button_down(0);
        b.button_up(50);
        b.tick(500);
        assert_eq!(b.mode(), Mode::Asleep);

        // Back online: the server says the display is ON, so the panel lights up
        // (through the wake fade — tick past it before sampling).
        b.set_online(true);
        b.on_manifest(mk(true));
        b.on_frame("a".into(), solid(255, 0, 0));
        assert_eq!(b.mode(), Mode::Running, "the manifest reasserts server ownership");
        b.tick(600);
        b.tick(600 + SLEEP_FADE_MS);
        assert_eq!(b.framebuffer()[0], 0xFF0000, "content is back on the panel");

        // And a manifest that says OFF leaves it dark — via display_on, not
        // mode (again after the fade-out has run its 400ms).
        b.on_manifest(mk(false));
        assert_eq!(b.mode(), Mode::Running);
        b.tick(1100 + SLEEP_FADE_MS);
        b.tick(1100 + 2 * SLEEP_FADE_MS);
        assert!(b.framebuffer().iter().all(|&p| p == 0), "display_on: false is still black");
    }

    /// Entering and leaving darkness FADES (~400ms, device clock) instead of
    /// snapping: a local tap-sleep ramps the lit content down monotonically
    /// and lands EXACTLY black; the wake ramps back up and lands EXACTLY on
    /// the content. The stored user brightness never moves.
    #[test]
    fn sleep_and_wake_fade_instead_of_snapping() {
        use scoreboard_protocol::RotationEntry;
        let solid = |r: u8, g: u8, b: u8| -> Vec<u8> {
            let px = scoreboard_protocol::rgb888_to_565(r, g, b).to_le_bytes();
            (0..FRAME_PIXELS).flat_map(|_| px).collect()
        };
        let lum = |fb: &[u32]| -> u64 {
            fb.iter().map(|&p| ((p >> 16 & 0xFF) + (p >> 8 & 0xFF) + (p & 0xFF)) as u64).sum()
        };
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        b.on_manifest(Manifest {
            countdown_ms: None,
            rotation: vec![RotationEntry { frame_id: "a".into(), dwell_ms: 60_000, scroll: false }],
            display_on: true,
            off_state: Default::default(),
            epoch: 0,
            brightness: 100,
        });
        b.on_frame("a".into(), solid(200, 40, 10));
        b.tick(1000);
        let content = b.framebuffer();
        assert!(lum(&content) > 0, "content is lit before the sleep");

        // OFFLINE tap-sleep is the local toggle (online presses defer to the
        // server); the offline grace holds the content underneath the fade.
        b.set_online(false);
        b.button_down(2000);
        b.button_up(2050);
        b.tick(2500); // the single resolves here -> Asleep; the ramp starts NOW
        assert_eq!(b.mode(), Mode::Asleep);
        let mut prev = lum(&b.framebuffer());
        assert_eq!(prev, lum(&content), "the ramp starts from the full content");
        for t in [2600u64, 2700, 2800, 2900] {
            b.tick(t);
            let l = lum(&b.framebuffer());
            assert!(l < prev, "luminance ramps down monotonically (t={t})");
            prev = l;
        }
        b.tick(2500 + SLEEP_FADE_MS);
        assert!(b.framebuffer().iter().all(|&p| p == 0), "lands exactly black at 400ms");
        b.tick(10_000);
        assert!(b.framebuffer().iter().all(|&p| p == 0), "…and stays black");
        assert_eq!(b.brightness(), 100, "the fade never touches the stored level");

        // Wake: ramps up from black and lands exactly on the old content.
        b.button_down(11_000);
        b.button_up(11_050);
        b.tick(11_500); // resolves -> Running; the ramp starts NOW
        assert_eq!(b.mode(), Mode::Running);
        let mut prev = lum(&b.framebuffer());
        assert_eq!(prev, 0, "the wake ramp starts from black");
        for t in [11_600u64, 11_700, 11_800, 11_900] {
            b.tick(t);
            let l = lum(&b.framebuffer());
            assert!(l > prev, "luminance ramps up monotonically (t={t})");
            prev = l;
        }
        b.tick(11_500 + SLEEP_FADE_MS);
        assert_eq!(b.framebuffer(), content, "lands exactly on the pre-sleep content");
    }

    /// The server-side sleep fades too: a display_on:false manifest ships an
    /// EMPTY rotation, so the fade-out dims the LAST SHOWN frame (`last_good`
    /// survives eviction for this) instead of snapping to black — and
    /// display_on:true fades back in to the exact content.
    #[test]
    fn manifest_power_off_fades_out_the_last_frame() {
        use scoreboard_protocol::RotationEntry;
        let solid = |r: u8, g: u8, b: u8| -> Vec<u8> {
            let px = scoreboard_protocol::rgb888_to_565(r, g, b).to_le_bytes();
            (0..FRAME_PIXELS).flat_map(|_| px).collect()
        };
        let lum = |fb: &[u32]| -> u64 {
            fb.iter().map(|&p| ((p >> 16 & 0xFF) + (p >> 8 & 0xFF) + (p & 0xFF)) as u64).sum()
        };
        let on = Manifest {
            countdown_ms: None,
            rotation: vec![RotationEntry { frame_id: "a".into(), dwell_ms: 60_000, scroll: false }],
            display_on: true,
            off_state: Default::default(),
            epoch: 0,
            brightness: 100,
        };
        let off = Manifest {
            countdown_ms: None,
            rotation: vec![],
            display_on: false,
            off_state: scoreboard_protocol::OffState::Blank,
            epoch: 0,
            brightness: 100,
        };
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        b.on_manifest(on.clone());
        b.on_frame("a".into(), solid(0, 180, 60));
        b.tick(1000);
        let content = b.framebuffer();

        b.on_manifest(off);
        b.tick(2000); // first tick after the flip: the ramp starts NOW
        assert_eq!(b.framebuffer(), content, "the last frame is still up at ramp start");
        b.tick(2200);
        let mid = lum(&b.framebuffer());
        assert!(mid > 0 && mid < lum(&content), "halfway: dimmed, not snapped");
        b.tick(2000 + SLEEP_FADE_MS);
        assert!(b.framebuffer().iter().all(|&p| p == 0), "lands exactly black");

        // Power back on: fade in, landing pixel-exact.
        b.on_manifest(on);
        b.tick(3000);
        assert!(b.framebuffer().iter().all(|&p| p == 0), "the wake ramp starts from black");
        b.tick(3200);
        let mid = lum(&b.framebuffer());
        assert!(mid > 0 && mid < lum(&content), "halfway back up");
        b.tick(3000 + SLEEP_FADE_MS);
        assert_eq!(b.framebuffer(), content, "lands exactly on the content");
    }

    /// Deliberately NO fade: the OTA blank, the reset overlay, and menu
    /// open/close snap — they're overlays, not sleep.
    #[test]
    fn ota_reset_and_menu_snap_without_fading() {
        use scoreboard_protocol::RotationEntry;
        let solid = |r: u8, g: u8, b: u8| -> Vec<u8> {
            let px = scoreboard_protocol::rgb888_to_565(r, g, b).to_le_bytes();
            (0..FRAME_PIXELS).flat_map(|_| px).collect()
        };
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        b.on_manifest(Manifest {
            countdown_ms: None,
            rotation: vec![RotationEntry { frame_id: "a".into(), dwell_ms: 60_000, scroll: false }],
            display_on: true,
            off_state: Default::default(),
            epoch: 0,
            brightness: 100,
        });
        b.on_frame("a".into(), solid(255, 0, 0));
        b.tick(1000);
        let content = b.framebuffer();

        // OTA blank: instant black, instant restore — no ramp either way.
        b.set_updating(Some(0));
        assert!(b.framebuffer().iter().all(|&p| p == 0), "OTA blank snaps to black");
        b.set_updating(None);
        assert_eq!(b.framebuffer(), content, "clearing the OTA blank snaps back");

        // Menu over an ASLEEP (settled-dark) board: instantly lit at the full
        // level, and instantly dark again on close.
        b.set_online(false);
        b.button_down(2000);
        b.button_up(2050);
        b.tick(2500); // -> Asleep
        b.tick(5000); // fade long since landed
        assert!(b.framebuffer().iter().all(|&p| p == 0));
        let t = double_tap(&mut b, 6000);
        assert!(b.menu_open());
        assert!(
            b.framebuffer().contains(&0x00FF_FFFF),
            "menu text renders at the FULL level instantly — no fade-in"
        );
        double_tap(&mut b, t);
        assert!(!b.menu_open());
        assert!(b.framebuffer().iter().all(|&p| p == 0), "closing snaps back to dark");

        // The reset overlay over the same dark board: instantly lit too.
        b.button_down(8000);
        b.tick(8900); // past the grace window: amber warm-up bar up
        assert!(b.reset_active());
        assert!(
            b.framebuffer().contains(&0x00FF_8800),
            "the hold bar renders at the FULL level instantly — no fade-in"
        );
    }

    /// The menu selection (deep-blue fill, white text everywhere) must stay
    /// readable at the darkest user level. At brightness 10 the selected row
    /// keeps visibly-blue fill pixels with the white text still brighter, and
    /// the reset-confirm screen keeps white text on both of its fills.
    #[test]
    fn menu_selection_stays_legible_at_min_brightness() {
        let white10 = {
            let c = 0xFFu32 * 10 / 100; // 25: each channel of white at 10%
            (c << 16) | (c << 8) | c
        };
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_brightness(10);
        b.tick(0);
        let t = double_tap(&mut b, 1000);
        let fb = b.framebuffer();
        // EXIT (row 0) is selected: its band is panel rows 0..=6.
        let band = &fb[..7 * FRAME_WIDTH];
        assert!(band.contains(&white10), "white text survives 10% brightness");
        let fill: Vec<u32> = band
            .iter()
            .copied()
            .filter(|&p| p != 0 && (p & 0xFF) > (p >> 16 & 0xFF))
            .collect();
        assert!(!fill.is_empty(), "the blue fill is still visible at 10%");
        let max_fill_channel = fill
            .iter()
            .flat_map(|&p| [p >> 16 & 0xFF, p >> 8 & 0xFF, p & 0xFF])
            .max()
            .unwrap();
        assert!(
            max_fill_channel < (white10 & 0xFF),
            "white text stays the brightest thing on the selected row"
        );

        // Reset confirm: white text over both the blue (CANCEL) and red
        // (ERASE) fills at 10%.
        b.encoder(-10); // RESET BOARD row
        tap(&mut b, t + 500);
        assert!(b.framebuffer().contains(&white10), "CANCEL side: white text at 10%");
        b.encoder(-1); // flip to ERASE
        let fb = b.framebuffer();
        assert!(fb.contains(&white10), "ERASE side: white text at 10%");
        let red_fill = fb
            .iter()
            .any(|&p| p != 0 && (p >> 16 & 0xFF) > (p & 0xFF) && (p >> 16 & 0xFF) > (p >> 8 & 0xFF));
        assert!(red_fill, "the red fill is still visible at 10%");
    }

    /// Freshness is stamped by CONTENT, not by the socket: a link that
    /// handshakes and dies before delivering a manifest must not restart the
    /// grace window, or an hour-old score renders as live forever. A reconnect
    /// that DOES deliver a manifest starts the clock over.
    #[test]
    fn reconnect_without_a_manifest_does_not_restart_the_grace_window() {
        use scoreboard_protocol::RotationEntry;
        let solid = |r: u8, g: u8, b: u8| -> Vec<u8> {
            let px = scoreboard_protocol::rgb888_to_565(r, g, b).to_le_bytes();
            (0..FRAME_PIXELS).flat_map(|_| px).collect()
        };
        let manifest = || Manifest {
            countdown_ms: None,
            rotation: vec![RotationEntry { frame_id: "a".into(), dwell_ms: 1000, scroll: false }],
            display_on: true,
            off_state: Default::default(),
            epoch: 0,
            brightness: 100,
        };
        let connect_screen = |t: u64| {
            let mut r = Board::new();
            r.mode = Mode::Running;
            r.tick(t);
            r.framebuffer()
        };
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        b.on_manifest(manifest());
        b.on_frame("a".into(), solid(255, 0, 0));
        b.tick(1000);
        let content = b.framebuffer();

        // Contact is lost at t=1000, then the socket flaps: each retry
        // handshakes and dies with nothing to show for it.
        b.set_online(false);
        for t in [10_000u64, 20_000, 30_000, 40_000] {
            b.tick(t);
            assert_eq!(b.framebuffer(), content, "inside the window: content held");
            b.set_online(true); // handshake completes…
            b.tick(t + 100);
            b.set_online(false); // …and dies before any manifest arrives
        }
        b.tick(1000 + OFFLINE_GRACE_MS + 100);
        assert_eq!(
            b.framebuffer(),
            connect_screen(1000 + OFFLINE_GRACE_MS + 100),
            "empty reconnects don't buy another 45s of stale scores"
        );

        // A reconnect that delivers content IS contact: the window restarts.
        b.set_online(true);
        b.on_manifest(manifest());
        b.tick(1000 + OFFLINE_GRACE_MS + 200);
        assert_eq!(b.framebuffer(), content, "fresh content is shown again");
        b.set_online(false);
        b.tick(2 * OFFLINE_GRACE_MS);
        assert_eq!(b.framebuffer(), content, "the window is timed from that manifest");
    }

    /// The pair code is only useful while someone is at the board typing it in.
    /// UPDATED with the settings menu: double-press toggles the MENU now (it
    /// no longer opens the pairing screen), so the test drives `Mode::Syncing`
    /// directly (a host-side entry) — the timeout machinery it guards is
    /// unchanged. The menu's own PAIR CODE screen rides the menu's 120s
    /// inactivity timeout instead.
    #[test]
    fn pair_code_screen_times_out() {
        let enter = |b: &mut Board| {
            b.syncing_since = Some(b.clock);
            b.mode = Mode::Syncing;
        };
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        enter(&mut b);
        b.tick(SYNC_TIMEOUT_MS - 1000);
        assert_eq!(b.mode(), Mode::Syncing, "still up while someone could be typing");
        b.tick(SYNC_TIMEOUT_MS + 100);
        assert_eq!(b.mode(), Mode::Running, "the code screen returns to content on its own");

        // The timer is per visit: a later entry gets its own full window.
        enter(&mut b);
        b.tick(SYNC_TIMEOUT_MS + 200);
        assert_eq!(b.mode(), Mode::Syncing, "the second visit starts its own clock");
        b.tick(2 * SYNC_TIMEOUT_MS + 300);
        assert_eq!(b.mode(), Mode::Running);
    }

    /// The brightness bar may cover pixels the SERVER drew; it must never cover
    /// the board's own status lines, whose text reaches row 29 (connect,
    /// "REBOOTING") and row 30 (the pair-code spinner). On content it stays in
    /// the bottom four rows.
    #[test]
    fn brightness_bar_spares_the_local_screens() {
        use scoreboard_protocol::RotationEntry;
        // Lit-pixel mask: brightness scaling dims every pixel, so compare which
        // pixels are lit rather than their exact color.
        let lit = |fb: &[u32]| -> Vec<bool> { fb.iter().map(|&p| p != 0).collect() };

        // Connect screen and setup screen: the turn must change nothing but the
        // overall dim — no bar, no erased status line.
        for setup in [false, true] {
            let mut b = Board::new();
            b.tick(0);
            if setup {
                b.gesture(Gesture::Long);
            } else {
                b.mode = Mode::Running;
                b.set_wifi(true);
            }
            b.tick(800);
            let before = lit(&b.framebuffer());
            b.encoder(-1);
            assert_eq!(lit(&b.framebuffer()), before, "no overlay on a local screen (setup={setup})");
        }

        // Pair-code screen: same, spinner included. (Entered directly —
        // double-press opens the settings menu, not this screen; see
        // `gestures_drive_mode`.)
        let mut b = Board::new();
        b.set_online(true);
        b.tick(0);
        b.mode = Mode::Syncing;
        b.tick(800);
        let before = lit(&b.framebuffer());
        b.encoder(-1);
        assert_eq!(lit(&b.framebuffer()), before, "the pair code keeps all 32 rows");

        // Server content: the bar DOES draw, confined to rows 28-31.
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        b.on_manifest(Manifest {
            countdown_ms: None,
            rotation: vec![RotationEntry { frame_id: "a".into(), dwell_ms: 10_000, scroll: false }],
            display_on: true,
            off_state: Default::default(),
            epoch: 0,
            brightness: 100,
        });
        let px = scoreboard_protocol::rgb888_to_565(255, 0, 0).to_le_bytes();
        b.on_frame("a".into(), (0..FRAME_PIXELS).flat_map(|_| px).collect());
        b.tick(100);
        b.encoder(-1);
        let with_bar = b.framebuffer();
        b.tick(4000); // same dim, bar expired: the difference IS the bar
        let without_bar = b.framebuffer();
        assert_eq!(
            with_bar[..28 * FRAME_WIDTH],
            without_bar[..28 * FRAME_WIDTH],
            "rows above 28 belong to the game"
        );
        assert_ne!(
            with_bar[28 * FRAME_WIDTH..],
            without_bar[28 * FRAME_WIDTH..],
            "the bar draws in the bottom four rows"
        );
    }

    /// Not a test — renders the new screens to raw RGB888 files for eyeballing.
    /// Run: cargo test -p board-core -- --ignored dump_screens
    #[test]
    #[ignore]
    fn dump_screens() {
        // Output dir override for whoever's eyeballing (default: system temp).
        let dir = std::env::var("BOARD_SCREEN_DUMP_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("board-screens"));
        let dir = dir.as_path();
        std::fs::create_dir_all(dir).unwrap();
        let dump = |name: &str, t: u64, b: &mut Board| {
            b.tick(t);
            let fb = b.framebuffer();
            let mut bytes = Vec::with_capacity(fb.len() * 3);
            for px in fb {
                bytes.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, px as u8]);
            }
            std::fs::write(dir.join(format!("{name}_{t}.rgb")), bytes).unwrap();
        };
        // 0ms and 800ms straddle both animation periods (ellipsis: 532ms/step,
        // leg dot: 665ms/lap), so each pair shows the motion.
        for t in [0u64, 800] {
            for (phase, name) in [
                (SetupPhase::WaitingBluetooth, "setup_bluetooth"),
                (SetupPhase::WaitingCreds, "setup_wifi_creds"),
                (SetupPhase::Saved, "setup_saved"),
            ] {
                let mut b = Board::new();
                b.tick(0);
                b.gesture(Gesture::Long);
                b.set_setup_phase(phase);
                dump(name, t, &mut b);
            }
            for (wifi, name) in [(false, "connect_wifi_down"), (true, "connect_wifi_up")] {
                let mut b = Board::new();
                b.mode = Mode::Running;
                b.set_wifi(wifi);
                dump(name, t, &mut b);
            }
        }

        // Settings menu + hold phases. Same diagnostics payload the simulator
        // feeds, so these dumps match what the sim window shows. The menu
        // opens on double-tap now; `gesture(Double)` is the sim's shortcut.
        let menu_board = || {
            let mut b = Board::new();
            b.mode = Mode::Running;
            b.set_version("2026.7.10".into());
            b.set_diagnostics(sim_diag());
            b.tick(0);
            b.gesture(Gesture::Double);
            b
        };
        let tap_at = |b: &mut Board, t: u64| {
            b.button_down(t);
            b.button_up(t + 50);
            b.tick(t + 50 + DOUBLE_WINDOW_MS + 10);
        };
        // Root list: EXIT on top (the default selection), an update on offer.
        let mut b = menu_board();
        b.set_update_available(Some("2026.8.1".into()));
        dump("menu_root", 4000, &mut b);
        // The UP TO DATE variant of the UPDATE row (no offer).
        let mut b = menu_board();
        dump("menu_root_up_to_date", 4000, &mut b);
        // Last row selected (down the list = encoder(-1), owner-tuned).
        let mut b = menu_board();
        b.encoder(-4);
        dump("menu_root_last", 4100, &mut b);
        // The selection colors at brightness 10 — the legibility worst case
        // the deep blue was picked for.
        let mut b = menu_board();
        b.set_brightness(10);
        dump("menu_root_dim10", 4200, &mut b);
        // DEBUG INFO: top page, then scrolled to the bottom.
        let mut b = menu_board();
        b.encoder(-3);
        tap_at(&mut b, 4000);
        dump("debug_info", 5000, &mut b);
        b.encoder(-30);
        dump("debug_info_scrolled", 5100, &mut b);
        // INSTALL UPDATE: the confirm behind the UPDATE row — version line,
        // the DO-NOT-UNPLUG warning, both selections.
        let mut b = menu_board();
        b.set_update_available(Some("2026.8.1".into()));
        b.encoder(-1);
        tap_at(&mut b, 4000);
        dump("update_confirm_cancel", 5000, &mut b);
        b.encoder(-1);
        dump("update_confirm_armed", 5100, &mut b);
        // RESET BOARD: both selections (white text, blue/red fills).
        let mut b = menu_board();
        b.encoder(-4);
        tap_at(&mut b, 4000);
        dump("reset_confirm_cancel", 5000, &mut b);
        b.encoder(-1);
        dump("reset_confirm_erase", 5100, &mut b);
        // The erase-only hold overlay: amber warm-up, then the red run to 10s.
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_version("2026.7.10".into());
        b.tick(0);
        b.button_down(100);
        dump("hold_erase_warmup", 1600, &mut b);
        dump("hold_erase_phase", 6000, &mut b);
    }
}

