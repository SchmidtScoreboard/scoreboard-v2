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
// One button, one hold, three thresholds — measured from the moment the press
// starts. The first HOLD_GRACE_MS are ignored so taps and double-taps never
// flash the bar. Then an amber bar fills toward the MENU threshold (3s opens
// the settings menu; inside the menu the shorter CLOSE threshold shuts it).
// KEEP holding past the menu threshold and the screen turns red: the raw
// factory reset at ERASE_HOLD_MS. That 15s super-hold is the escape hatch that
// must survive any bug in the menu logic — it lives in `tick`, below the menu
// state entirely, and wipes Wi-Fi credentials exactly as it always has.
pub const HOLD_GRACE_MS: u64 = 500;
pub const MENU_HOLD_MS: u64 = 3_000;
pub const MENU_CLOSE_HOLD_MS: u64 = 2_000;
pub const ERASE_HOLD_MS: u64 = 15_000;
// Nobody stands at the board for two minutes without touching the knob: the
// menu closes itself (device clock) so an interrupted visit can't park the
// panel on a settings list instead of scores.
pub const MENU_IDLE_TIMEOUT_MS: u64 = 120_000;
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

/// Host-reported facts for the WIFI INFO diagnostics screen. Everything is
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
    WifiInfo,
    ResetConfirm,
}

struct MenuState {
    screen: MenuScreen,
    selected: usize,      // root row (0..MENU_ITEM_COUNT)
    scroll: usize,        // wifi-info top line (the root's four rows all fit)
    erase_selected: bool, // reset-confirm: CANCEL is the default
    last_interaction: u64, // any knob activity; 120s of silence closes the menu
}

// Root menu rows, in owner-specified order.
const MENU_UPDATE: usize = 0;
const MENU_PAIR: usize = 1;
const MENU_WIFI: usize = 2;
const MENU_RESET: usize = 3;
const MENU_ITEM_COUNT: usize = 4;
// The wifi-info screen shows this many FONT_4X6 rows at once (5 × 6px + 1px
// top margin fills the 32px panel); the rest scroll.
const WIFI_INFO_VISIBLE: usize = 5;

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

    // the one-button hold machine (menu at 3s, raw factory reset at 15s)
    reset_active: bool,         // hold bar on screen (held past the grace window)
    reset_progress: u8,         // 0..=100 fill of the CURRENT phase's bar
    hold_erase_phase: bool,     // past 3s: the bar is red and runs toward the wipe
    hold_started_in_menu: bool, // decides the amber target: close (2s) vs open (3s)
    hold_fired: bool,           // this press's open/close action already ran
    reset_done: bool,           // erase complete — firmware wipes creds + reboots
    reset_done_at: Option<u64>, // for the simulator to auto-dismiss (device reboots first)

    // settings menu overlay (None = whatever the mode machine was doing)
    menu: Option<MenuState>,
    // Newer firmware the server says exists (`ServerMsg::UpdateAvailable`);
    // shown on the UPDATE row, and what makes selecting it send RequestUpdate.
    update_available: Option<String>,
    diag: Diagnostics, // host-fed rows for the WIFI INFO screen

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
            reset_active: false,
            reset_progress: 0,
            hold_erase_phase: false,
            hold_started_in_menu: false,
            hold_fired: false,
            reset_done: false,
            reset_done_at: None,
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

    /// Host-reported diagnostics for the WIFI INFO screen. Feed whenever the
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
        self.pressed_at = Some(now_ms);
        // Captured at press time: a hold that BEGINS inside the menu is a
        // close (2s), one that begins outside is an open (3s). Deciding at
        // fire time instead would make the 3s crossing re-open the menu the
        // same press just closed.
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
        let Some(p) = self.pressed_at.take() else { return };
        if let Some(m) = &mut self.menu {
            m.last_interaction = now_ms;
        }
        // A hold past the grace window was a hold, not a tap: clear the bar and
        // leave whatever thresholds already fired (menu open/close) in place.
        // Releasing anywhere in the red phase keeps everything un-erased — the
        // wipe only ever fires from `tick`, at the full 15s.
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

        // The hold machine. One press, timed from its start: amber bar to the
        // menu action (open at 3s from anywhere; close at 2s when the press
        // began inside the menu), then the RED phase running to the raw wipe
        // at 15s. This block is deliberately independent of the menu's own
        // logic — it reads only `pressed_at` and the flags stamped at press
        // time, so the 15s escape hatch works even if the menu is ever wrong.
        if let (Some(p), false) = (self.pressed_at, self.reset_done) {
            let held = now_ms.saturating_sub(p);
            // Fire the menu action exactly once per press, even if a slow host
            // tick jumps clean over the threshold.
            let menu_at = if self.hold_started_in_menu { MENU_CLOSE_HOLD_MS } else { MENU_HOLD_MS };
            if !self.hold_fired && held >= menu_at {
                self.hold_fired = true;
                if self.hold_started_in_menu {
                    // Close = drop the overlay. The mode machine underneath was
                    // never touched, so "restore the prior state" is a no-op.
                    self.menu = None;
                } else {
                    self.open_menu();
                }
            }
            if held >= HOLD_GRACE_MS {
                self.reset_active = true;
                if held >= MENU_HOLD_MS {
                    // Red phase: 3s..15s runs toward the wipe, clearly labeled
                    // so nobody erases their Wi-Fi reaching for the menu.
                    self.hold_erase_phase = true;
                    let fill = held - MENU_HOLD_MS;
                    let span = ERASE_HOLD_MS - MENU_HOLD_MS;
                    self.reset_progress = ((fill * 100) / span).min(100) as u8;
                    if held >= ERASE_HOLD_MS {
                        self.reset_done = true;
                        self.reset_progress = 100;
                        self.reset_done_at = Some(now_ms);
                    }
                } else {
                    // Amber phase: fill toward this press's menu action. A
                    // menu-origin press keeps showing the full bar from its 2s
                    // close until the red phase takes over at 3s — continuous
                    // feedback that the hold is still doing something.
                    self.hold_erase_phase = false;
                    let fill = held - HOLD_GRACE_MS;
                    let span = menu_at - HOLD_GRACE_MS;
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
        // someone walks over and presses again.
        if self.mode == Mode::Syncing {
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
    }

    fn apply(&mut self, g: Gesture) {
        // With the menu up, a tap is a SELECT and never reaches the sleep
        // toggle below — the modes don't exist while the overlay has the knob.
        if self.menu.is_some() {
            match g {
                Gesture::Single => self.menu_select(),
                // Reserved here too (see below) — two selects would be worse.
                Gesture::Double => {}
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
                }
            }
            // double press: deliberately SWALLOWED. It used to open the pairing
            // screen (that lives in the settings menu now), and it must not
            // fall through to two sleep toggles — a fumbled tap would flash the
            // panel off-and-on. The gesture stays reserved for a future use.
            Gesture::Double => {}
            // long press: factory reset -> setup
            Gesture::Long => self.enter_setup(),
        }
    }

    /// The post-wipe provisioning entry (what `Gesture::Long` injects).
    fn enter_setup(&mut self) {
        self.manifest = None;
        self.cache.clear();
        self.syncing_since = None;
        // Fresh provisioning always starts from the top of the handshake.
        self.setup_phase = SetupPhase::WaitingBluetooth;
        self.mode = Mode::Setup;
    }

    /// Open the settings menu (the 3s hold crossing). The mode machine is left
    /// exactly as it stands — the menu is an overlay, and dropping it later is
    /// the whole "restore the prior state" story.
    fn open_menu(&mut self) {
        if self.menu.is_none() {
            self.menu = Some(MenuState {
                screen: MenuScreen::Root,
                selected: 0,
                scroll: 0,
                erase_selected: false,
                last_interaction: self.clock,
            });
        }
    }

    /// Knob rotation while the menu is up: move the selection / scroll / flip
    /// the confirm toggle. Never brightness (see `encoder`).
    fn menu_rotate(&mut self, delta: i32) {
        let clock = self.clock;
        let Some(m) = &mut self.menu else { return };
        m.last_interaction = clock;
        match m.screen {
            MenuScreen::Root => {
                m.selected = (m.selected as i32 + delta).clamp(0, MENU_ITEM_COUNT as i32 - 1)
                    as usize;
            }
            // The code screen has nothing to scroll; the turn only counts as
            // presence (it keeps the 120s timeout from closing mid-pairing).
            MenuScreen::PairCode => {}
            MenuScreen::WifiInfo => {
                let max = self.wifi_info_lines().len().saturating_sub(WIFI_INFO_VISIBLE);
                let m = self.menu.as_mut().unwrap();
                m.scroll = (m.scroll as i32 + delta).clamp(0, max as i32) as usize;
            }
            // Direction picks the option outright (rather than toggling) so a
            // detent that double-reports can't land on ERASE by accident.
            MenuScreen::ResetConfirm => m.erase_selected = delta > 0,
        }
    }

    /// Tap while the menu is up.
    fn menu_select(&mut self) {
        let clock = self.clock;
        let Some(m) = &mut self.menu else { return };
        m.last_interaction = clock;
        match m.screen {
            MenuScreen::Root => match m.selected {
                MENU_UPDATE => {
                    // Armed only when the server has said a newer image exists.
                    // The outbox queues, so selecting while offline sends the
                    // request when the socket returns; with nothing offered the
                    // tap is a no-op (there is nothing to request).
                    if self.update_available.is_some() {
                        self.outbox.push(ClientMsg::RequestUpdate);
                    }
                }
                MENU_PAIR => m.screen = MenuScreen::PairCode,
                MENU_WIFI => {
                    m.screen = MenuScreen::WifiInfo;
                    m.scroll = 0;
                }
                MENU_RESET => {
                    m.screen = MenuScreen::ResetConfirm;
                    // CANCEL every visit — a stale ERASE from a backed-out
                    // attempt must never be one tap from firing.
                    m.erase_selected = false;
                }
                _ => {}
            },
            // The sub-screens tap back to the menu root.
            MenuScreen::PairCode | MenuScreen::WifiInfo => m.screen = MenuScreen::Root,
            MenuScreen::ResetConfirm => {
                if m.erase_selected {
                    // Same latch the 15s hold sets — one wipe path, not two.
                    self.menu = None;
                    self.reset_done = true;
                    self.reset_progress = 100;
                    self.reset_done_at = Some(clock);
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
                // The pairing screen the double-tap used to own, unchanged.
                MenuScreen::PairCode => self.screen_syncing(),
                MenuScreen::WifiInfo => self.screen_wifi_info(m),
                MenuScreen::ResetConfirm => self.screen_reset_confirm(m),
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
        let mut server_content = false;
        let mut buf = match self.mode {
            Mode::Booting => self.screen_boot(),
            Mode::Setup => self.screen_setup(),
            Mode::Syncing => self.screen_syncing(),
            Mode::Asleep => return vec![0u32; FRAME_PIXELS],
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
                    self.screen_running()
                } else {
                    self.screen_connecting()
                }
            }
        };
        // Brightness is applied exactly once, here — `local_brightness` tracks the
        // server's value while online (see `on_manifest`) and doubles as the local
        // fallback offline, so it dims every screen (boot/setup/sync/running).
        scale_buf(&mut buf, self.local_brightness);
        // Transient indicator for ~2s after a knob turn, drawn at full level so it
        // stays legible even when the screen itself is dimmed.
        if server_content && self.brightness_changed_at.is_some() {
            overlay_brightness_bar(&mut buf, self.local_brightness);
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

    /// The hold overlay, in its three phases: amber bar filling toward the
    /// menu action, the red KEEP-HOLDING-TO-ERASE run to 15s, and the green
    /// completion screen (the firmware reboots moments after that one).
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
                if self.hold_started_in_menu { "CLOSING MENU" } else { "HOLD FOR MENU" };
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

    /// Root settings list: a title row and four FONT_4X6 rows, selection as an
    /// inverse amber bar (black text on amber — the one style that stays
    /// legible at brightness 10).
    fn screen_menu_root(&self, m: &MenuState) -> Vec<u32> {
        let mut c = Canvas::new();
        let grey = Rgb888::new(0x88, 0x88, 0x88);
        let amber = Rgb888::new(0xFF, 0x88, 0x00);
        center_text(&mut c, "SETTINGS", 3, &FONT_4X6, Rgb888::new(0x33, 0xAA, 0xFF));
        // The UPDATE row carries a version: the offered one (with an up-arrow)
        // when the server said a newer image exists, otherwise what's running
        // — offline it simply shows the running version, no arrow, inert.
        let update_row = match (&self.update_available, &self.version) {
            (Some(v), _) => format!("UPDATE ^{v}"),
            (None, Some(v)) => format!("UPDATE {v}"),
            (None, None) => String::from("UPDATE"),
        };
        let items = [update_row.as_str(), "PAIR CODE", "WIFI INFO", "RESET BOARD"];
        for (i, label) in items.iter().enumerate() {
            let y = 8 + i as i32 * 6;
            if i == m.selected {
                Rectangle::new(Point::new(0, y - 1), Size::new(FRAME_WIDTH as u32, 7))
                    .into_styled(PrimitiveStyle::with_fill(amber))
                    .draw(&mut c)
                    .ok();
                left_text(&mut c, label, 1, y, Rgb888::BLACK);
            } else {
                left_text(&mut c, label, 1, y, grey);
            }
        }
        c.into_u32()
    }

    /// The rows the WIFI INFO screen scrolls through. Built on demand (also by
    /// `menu_rotate` for the scroll clamp) — the list is tiny.
    fn wifi_info_lines(&self) -> Vec<InfoLine> {
        let grey = Rgb888::new(0x88, 0x88, 0x88);
        let white = Rgb888::new(0xCC, 0xCC, 0xCC);
        let line = |text: String, color: Rgb888| InfoLine { text, color, bars: None };
        let d = &self.diag;
        let mut ls = Vec::new();
        ls.push(line(String::from("WIFI INFO"), Rgb888::new(0x33, 0xAA, 0xFF)));
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
    fn screen_wifi_info(&self, m: &MenuState) -> Vec<u32> {
        let lines = self.wifi_info_lines();
        let max_scroll = lines.len().saturating_sub(WIFI_INFO_VISIBLE);
        let scroll = m.scroll.min(max_scroll);
        let mut c = Canvas::new();
        for (row, l) in lines.iter().skip(scroll).take(WIFI_INFO_VISIBLE).enumerate() {
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

    /// RESET BOARD confirmation. CANCEL selects amber (the ordinary choice);
    /// ERASE selects RED — the same danger color as the super-hold phase, so
    /// the two roads to the wipe look like the same decision.
    fn screen_reset_confirm(&self, m: &MenuState) -> Vec<u32> {
        let mut c = Canvas::new();
        let grey = Rgb888::new(0x88, 0x88, 0x88);
        let red = Rgb888::new(0xFF, 0x55, 0x33);
        let amber = Rgb888::new(0xFF, 0x88, 0x00);
        center_text(&mut c, "RESET BOARD?", 4, &FONT_4X6, Rgb888::WHITE);
        center_text(&mut c, "ERASES WI-FI", 11, &FONT_4X6, grey);
        let bar_y = 21i32;
        let (cancel_fg, erase_fg) = if m.erase_selected {
            Rectangle::new(Point::new(34, bar_y), Size::new(28, 9))
                .into_styled(PrimitiveStyle::with_fill(red))
                .draw(&mut c)
                .ok();
            (grey, Rgb888::BLACK)
        } else {
            Rectangle::new(Point::new(2, bar_y), Size::new(28, 9))
                .into_styled(PrimitiveStyle::with_fill(amber))
                .draw(&mut c)
                .ok();
            (Rgb888::BLACK, red)
        };
        center_text_at(&mut c, "CANCEL", 16, bar_y + 4, &FONT_4X6, cancel_fg);
        center_text_at(&mut c, "ERASE", 48, bar_y + 4, &FONT_4X6, erase_fg);
        c.into_u32()
    }
}

/// One row of the WIFI INFO screen. `bars` marks the signal row, whose four
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
    // (x = 31.5): a 2x2 dot astride it, then a 6-wide and an 8-wide arc
    // built up from that base. The old 7-wide glyph was forced half a pixel
    // off-center, which kept the connect screen from ever being truly
    // symmetric no matter where the leg dots sat.
    #[rustfmt::skip]
    let px = [
        (28, 14), (29, 13), (30, 12), (31, 12), (32, 12), (33, 12), (34, 13), (35, 14), // outer arc
        (29, 16), (30, 15), (31, 15), (32, 15), (33, 15), (34, 16),                     // inner arc
        (31, 17), (32, 17), (31, 18), (32, 18),                                         // 2x2 dot
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

    /// DELIBERATE behavior change: double-press used to toggle the pairing
    /// screen (Mode::Syncing). The pair code lives in the settings menu now,
    /// and the gesture is a reserved NO-OP — it must not alias to two sleep
    /// toggles, online or off.
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

        // double press: swallowed everywhere — no sleep flicker, no pairing
        // screen, nothing queued for the server.
        for (i, online) in [(0u64, false), (1, true)] {
            let t0 = 2000 + i * 2000;
            b.set_online(online);
            b.take_outgoing(); // drop the ButtonPress noise from the setup above
            b.button_down(t0);
            b.button_up(t0 + 40);
            b.button_down(t0 + 120);
            b.button_up(t0 + 160);
            b.tick(t0 + 160 + DOUBLE_WINDOW_MS + 10);
            assert_eq!(b.mode(), Mode::Running, "double press is a no-op (online={online})");
            assert!(
                b.take_outgoing().is_empty(),
                "a swallowed double press sends nothing (online={online})"
            );
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
            rotation: vec![RotationEntry { frame_id: "tall".into(), dwell_ms: 10_000, scroll: false }],
            display_on: true,
            off_state: Default::default(),
            epoch: 0,
            brightness: 100,
        });
        b.tick(13_000);
        assert_eq!(top_row(&b), at_top, "no flag -> static top window");
    }

    /// Timings DELIBERATELY reworked with the settings menu: the wipe moved
    /// from ~10s to the 15s super-hold (the 3s crossing now opens the menu on
    /// the way). The latch semantics are unchanged.
    #[test]
    fn hold_fills_then_triggers_reset() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.button_down(0);
        b.tick(300); // within the grace window: nothing yet
        assert!(!b.reset_active());
        b.tick(800); // past grace (500): amber bar is filling toward the menu
        assert!(b.reset_active());
        assert!(!b.hold_erase_phase);
        assert!(!b.reset_triggered());
        b.tick(MENU_HOLD_MS + 500); // past 3s: menu opened, bar turned red
        assert!(b.menu_open(), "the 3s crossing opens the menu mid-hold");
        assert!(b.hold_erase_phase, "past 3s the hold runs toward the erase");
        assert!(!b.reset_triggered());
        b.tick(ERASE_HOLD_MS - 100); // just shy of 15s: still not erased
        assert!(!b.reset_triggered());
        b.tick(ERASE_HOLD_MS + 100); // the super-hold completes
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
        b.button_up(1200); // released before the menu threshold
        assert!(!b.reset_active());
        assert!(!b.reset_triggered());
        assert!(!b.menu_open(), "released before 3s: no menu either");
        // A long-ish hold no longer enters Setup (that's the app's job now).
        assert_eq!(b.mode(), Mode::Running);
    }

    // ---- settings-menu helpers: the three knob moves as the host delivers them ----

    /// Hold across the 3s menu threshold, then release. Returns a time safely
    /// after the release.
    fn hold_to_menu(b: &mut Board, t0: u64) -> u64 {
        b.button_down(t0);
        b.tick(t0 + MENU_HOLD_MS + 50);
        b.button_up(t0 + MENU_HOLD_MS + 100);
        t0 + MENU_HOLD_MS + 200
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

    /// The 3s hold opens the menu from every state, and closing (the 2s hold)
    /// restores each one EXACTLY — the mode machine never notices the visit.
    #[test]
    fn hold_opens_menu_everywhere_and_closing_restores_the_prior_state() {
        // Running.
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        let t = hold_to_menu(&mut b, 1000);
        assert!(b.menu_open());
        assert_eq!(b.mode(), Mode::Running, "the mode machine is untouched underneath");
        hold_to_close(&mut b, t);
        assert!(!b.menu_open());
        assert_eq!(b.mode(), Mode::Running);

        // Asleep stays asleep.
        let mut b = Board::new();
        b.mode = Mode::Asleep;
        b.tick(0);
        let t = hold_to_menu(&mut b, 1000);
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
        let t = hold_to_menu(&mut b, 1000);
        assert!(b.menu_open());
        let t = hold_to_close(&mut b, t);
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
        let t = hold_to_menu(&mut b, 1000);
        assert!(b.menu_open());
        hold_to_close(&mut b, t);
        assert_eq!(b.mode(), Mode::Setup, "still provisioning after the visit");
    }

    /// The 15s super-hold is the escape hatch: it must erase from Running and
    /// from a press that started INSIDE the menu (closing it on the way).
    #[test]
    fn super_hold_fires_the_reset_from_running_and_from_inside_the_menu() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        b.button_down(1000);
        b.tick(1000 + ERASE_HOLD_MS + 50);
        assert!(b.reset_triggered());

        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        let t = hold_to_menu(&mut b, 1000);
        assert!(b.menu_open());
        b.button_down(t);
        b.tick(t + MENU_CLOSE_HOLD_MS + 50);
        assert!(!b.menu_open(), "the 2s crossing closed the menu");
        b.tick(t + MENU_HOLD_MS + 500);
        assert!(!b.menu_open(), "a menu-origin hold must not reopen the menu at 3s");
        assert!(b.hold_erase_phase, "…it runs on toward the erase instead");
        b.tick(t + ERASE_HOLD_MS + 50);
        assert!(b.reset_triggered(), "15s from press start wipes, wherever it began");
    }

    /// Letting go anywhere in the red phase erases nothing and leaves the menu
    /// the hold opened at 3s — the wipe fires from `tick` at 15s, never on
    /// release.
    #[test]
    fn releasing_in_the_red_phase_keeps_the_menu_and_the_credentials() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        b.button_down(1000);
        b.tick(1000 + MENU_HOLD_MS + 2000); // deep in the red phase
        assert!(b.hold_erase_phase);
        assert!(b.menu_open());
        b.button_up(1000 + MENU_HOLD_MS + 2500);
        assert!(!b.reset_triggered(), "only the full 15s erases");
        assert!(b.menu_open(), "the menu the hold opened is still there");
        assert!(!b.reset_active(), "the red bar is gone");
    }

    /// Two minutes of an untouched knob closes an abandoned menu (device
    /// clock), and any interaction restarts the window.
    #[test]
    fn menu_times_out_after_two_minutes_of_no_interaction() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        let t = hold_to_menu(&mut b, 1000);
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
    #[test]
    fn rotate_moves_the_selection_and_clamps_and_scrolls() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_diagnostics(sim_diag());
        b.tick(0);
        let t = hold_to_menu(&mut b, 1000);
        b.tick(t);
        let top = b.framebuffer();
        b.encoder(-1);
        assert_eq!(b.framebuffer(), top, "selection clamps at the top");
        b.encoder(1);
        assert_ne!(b.framebuffer(), top, "rotation moves the selection");
        b.encoder(10);
        let last = b.framebuffer();
        b.encoder(1);
        assert_eq!(b.framebuffer(), last, "selection clamps at the bottom");

        // Into WIFI INFO (row 2) and scroll the list.
        b.encoder(-1);
        tap(&mut b, t + 1000);
        let page_top = b.framebuffer();
        b.encoder(-1);
        assert_eq!(b.framebuffer(), page_top, "scroll clamps at the top");
        b.encoder(1);
        assert_ne!(b.framebuffer(), page_top, "rotation scrolls the diagnostics");
        b.encoder(30);
        let bottom = b.framebuffer();
        b.encoder(1);
        assert_eq!(b.framebuffer(), bottom, "scroll clamps at the bottom");
        b.encoder(-30);
        assert_eq!(b.framebuffer(), page_top, "and scrolls back to the top");
    }

    /// RESET BOARD arms nothing by itself: CANCEL is the default every visit,
    /// backs out to the root, and only rotate-to-ERASE + tap runs the wipe.
    #[test]
    fn reset_confirm_defaults_to_cancel_and_cancel_backs_out() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        let t = hold_to_menu(&mut b, 1000);
        b.encoder(10); // RESET BOARD row
        let t = tap(&mut b, t + 500); // enter the confirm screen
        let cancel_screen = b.framebuffer();
        let t = tap(&mut b, t + 500); // tap the default (CANCEL)
        assert!(b.menu_open(), "CANCEL leaves the menu open");
        assert!(!b.reset_triggered(), "…and erases nothing");
        // Re-enter: still CANCEL by default (a backed-out ERASE doesn't stick).
        let t = tap(&mut b, t + 500);
        assert_eq!(b.framebuffer(), cancel_screen, "every visit starts on CANCEL");
        b.encoder(1); // flip to ERASE
        assert_ne!(b.framebuffer(), cancel_screen, "ERASE selection renders differently");
        tap(&mut b, t + 500);
        assert!(b.reset_triggered(), "ERASE + tap runs the one wipe path");
        assert!(!b.menu_open());
    }

    /// The UPDATE row: inert with nothing offered, one `RequestUpdate` in the
    /// outbox when the server has advertised a newer image — queued offline,
    /// shipped whenever the host next drains.
    #[test]
    fn selecting_update_queues_the_request() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        let t = hold_to_menu(&mut b, 1000);
        b.take_outgoing();
        let t = tap(&mut b, t); // UPDATE row is the default selection
        assert!(b.take_outgoing().is_empty(), "no offer, no message");

        b.set_update_available(Some("2026.9.9".into()));
        tap(&mut b, t);
        let out = b.take_outgoing();
        assert!(matches!(out.as_slice(), [ClientMsg::RequestUpdate]), "got {out:?}");
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
        let t = hold_to_menu(&mut b, 1000);
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

    /// PAIR CODE inside the menu is the very screen the double-press used to
    /// own — pixel for pixel — with `pairing_active` telling the host to
    /// announce a code, and tap / SetupDone both landing back on the root.
    #[test]
    fn menu_pair_code_reuses_the_syncing_screen_and_taps_back() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_online(true);
        b.tick(0);
        let t = hold_to_menu(&mut b, 1000);
        b.encoder(1); // PAIR CODE row
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

        // Back online: the server says the display is ON, so the panel lights up.
        b.set_online(true);
        b.on_manifest(mk(true));
        b.on_frame("a".into(), solid(255, 0, 0));
        assert_eq!(b.mode(), Mode::Running, "the manifest reasserts server ownership");
        b.tick(600);
        assert_eq!(b.framebuffer()[0], 0xFF0000, "content is back on the panel");

        // And a manifest that says OFF leaves it dark — via display_on, not mode.
        b.on_manifest(mk(false));
        assert_eq!(b.mode(), Mode::Running);
        assert!(b.framebuffer().iter().all(|&p| p == 0), "display_on: false is still black");
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
    /// UPDATED with the settings menu: the double-press entry is gone, so the
    /// test drives `Mode::Syncing` directly (a host-side entry) — the timeout
    /// machinery it guards is unchanged. The menu's own PAIR CODE screen rides
    /// the menu's 120s inactivity timeout instead.
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

        // Pair-code screen: same, spinner included. (Entered directly — the
        // double-press entry is gone; see `gestures_drive_mode`.)
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
        // feeds, so these dumps match what the sim window shows.
        let menu_board = || {
            let mut b = Board::new();
            b.mode = Mode::Running;
            b.set_version("2026.7.10".into());
            b.set_diagnostics(sim_diag());
            b.tick(0);
            b.button_down(100);
            b.tick(100 + MENU_HOLD_MS + 50);
            b.button_up(100 + MENU_HOLD_MS + 100);
            b
        };
        let tap_at = |b: &mut Board, t: u64| {
            b.button_down(t);
            b.button_up(t + 50);
            b.tick(t + 50 + DOUBLE_WINDOW_MS + 10);
        };
        // Root list: first row selected (with an update on offer), then last.
        let mut b = menu_board();
        b.set_update_available(Some("2026.8.1".into()));
        dump("menu_root", 4000, &mut b);
        b.encoder(3);
        dump("menu_root_last", 4100, &mut b);
        // WIFI INFO: top page, then scrolled to the bottom.
        let mut b = menu_board();
        b.encoder(2);
        tap_at(&mut b, 4000);
        dump("wifi_info", 5000, &mut b);
        b.encoder(30);
        dump("wifi_info_scrolled", 5100, &mut b);
        // RESET BOARD: both selections.
        let mut b = menu_board();
        b.encoder(3);
        tap_at(&mut b, 4000);
        dump("reset_confirm_cancel", 5000, &mut b);
        b.encoder(1);
        dump("reset_confirm_erase", 5100, &mut b);
        // The hold overlay: amber fill toward the menu, then the red erase run.
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.set_version("2026.7.10".into());
        b.tick(0);
        b.button_down(100);
        dump("hold_menu_fill", 2000, &mut b);
        dump("hold_erase_phase", 9000, &mut b);
    }
}

