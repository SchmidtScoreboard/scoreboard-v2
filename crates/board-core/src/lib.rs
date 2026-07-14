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
// Hold-to-reset: ignore the first RESET_GRACE_MS (so taps / double-press don't
// trigger it), then fill a bar over RESET_FILL_MS — a deliberate ~10s hold so it's
// never accidental and the user clearly sees the press is doing something. At the
// end the firmware wipes the Wi-Fi credentials and reboots into BLE provisioning.
const RESET_GRACE_MS: u64 = 500;
const RESET_FILL_MS: u64 = 9500;
// Offline grace: a dropped websocket usually re-establishes within a second or
// two — and a server DEPLOY takes 30-60s of reconnect failures. While there's
// content on screen, keep showing it (scores just pause updating); only admit
// the outage with the connect-progress screen once it has lasted this long.
// Field-tuned up from 5s: every deploy and proxy blip was flashing CONNECTING
// at people, which reads as "broken", not "briefly offline".
const OFFLINE_GRACE_MS: u64 = 45_000;

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
    offline_since: Option<u64>, // when it dropped (starts the offline grace window)
    wifi_up: bool,        // host-reported Wi-Fi link/DHCP state (connect screen)
    setup_phase: SetupPhase, // which BLE-provisioning stage the setup screen shows
    brightness_changed_at: Option<u64>, // transient brightness-bar overlay window

    // hold-to-reset (factory reset of the stored Wi-Fi credentials)
    reset_active: bool,         // bar on screen (held past the grace window)
    reset_progress: u8,         // 0..=100 fill
    reset_done: bool,           // fill complete — firmware wipes creds + reboots
    reset_done_at: Option<u64>, // for the simulator to auto-dismiss (device reboots first)

    // OTA update in progress: Some(pct) = full-screen updating overlay. Progress
    // is DEVICE-known (bytes the firmware has downloaded), so this is pure local
    // rendering — no animation or progress ever rides the network.
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
            reset_active: false,
            reset_progress: 0,
            reset_done: false,
            reset_done_at: None,
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

    /// OTA update progress. `Some(pct)` puts up (or refreshes) the full-screen
    /// updating overlay — "UPDATING" + progress bar + "DO NOT UNPLUG" — which
    /// overrides every mode except hold-to-reset (mid-flash-write is the one
    /// moment the user must not think the board hung and pull the plug).
    /// `None` clears it back to whatever the mode would normally show (the
    /// failed-download case: the update must be a non-event).
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

    /// True once the hold-to-reset bar has filled — the firmware should wipe the
    /// stored Wi-Fi credentials and reboot into BLE provisioning.
    pub fn reset_triggered(&self) -> bool {
        self.reset_done
    }

    /// Host reports websocket connectivity; drives the reconnecting screen.
    /// A drop starts the offline grace window (see `framebuffer`) — timed from
    /// the true->false transition only, so a host that re-reports "offline"
    /// every poll can't keep extending the window forever.
    pub fn set_online(&mut self, online: bool) {
        if online {
            self.offline_since = None;
        } else if self.online {
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

    /// The board has been paired; leave the setup / sync screen.
    pub fn setup_done(&mut self) {
        if matches!(self.mode, Mode::Setup | Mode::Syncing) {
            self.mode = Mode::Running;
        }
    }

    /// Trigger a gesture directly (used by the simulator control panel).
    pub fn gesture(&mut self, g: Gesture) {
        self.apply(g);
    }

    /// Encoder rotation -> brightness (mirrors the real device's encoder).
    /// While online the new level is reported to the server, which adopts it as
    /// the board's one brightness and echoes it back in the next manifest — the
    /// knob and the app's slider move the same value. Offline it stays a purely
    /// local dim so the knob is never a dead end.
    pub fn encoder(&mut self, delta: i32) {
        self.local_brightness = (self.local_brightness as i32 + delta * 10).clamp(10, 100) as u8;
        self.brightness_changed_at = Some(self.clock);
        if self.online && matches!(self.mode, Mode::Running) {
            self.outbox.push(ClientMsg::DeviceBrightness { level: self.local_brightness });
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
        if self.mode == Mode::Booting && !m.rotation.is_empty() {
            self.mode = Mode::Running;
        }
        // The server owns brightness; adopt its value unless the knob was just
        // turned (the transient bar is still up) — a manifest racing a turn in
        // flight would rubber-band the knob.
        if self.brightness_changed_at.is_none() {
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
        if !self.reset_done {
            self.reset_active = false;
            self.reset_progress = 0;
        }
    }

    pub fn button_up(&mut self, now_ms: u64) {
        let Some(p) = self.pressed_at.take() else { return };
        // A hold past the grace window was a reset attempt; if it didn't complete,
        // cancel the bar — and don't also treat it as a tap gesture.
        if now_ms.saturating_sub(p) >= RESET_GRACE_MS {
            if !self.reset_done {
                self.reset_active = false;
                self.reset_progress = 0;
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

        // hold-to-reset: after the grace window, fill a bar; when full the firmware
        // wipes Wi-Fi creds + reboots. (Replaces the old long-press-to-setup.)
        if let (Some(p), false) = (self.pressed_at, self.reset_done) {
            let held = now_ms.saturating_sub(p);
            if held >= RESET_GRACE_MS {
                self.reset_active = true;
                let fill = held - RESET_GRACE_MS;
                self.reset_progress = ((fill * 100) / RESET_FILL_MS).min(100) as u8;
                if fill >= RESET_FILL_MS {
                    self.reset_done = true;
                    self.reset_progress = 100;
                    self.reset_done_at = Some(now_ms);
                }
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
                }
            }
            // double press: show the pairing code — but only while online.
            // Offline (or in BLE provisioning) the server doesn't know any
            // code, so the screen would just be a dead end. A second double
            // press closes the code screen again (even if the connection
            // dropped while it was up — leaving must never be gated).
            Gesture::Double => {
                if self.mode == Mode::Syncing {
                    self.mode = Mode::Running;
                } else if self.online {
                    self.mode = Mode::Syncing;
                }
            }
            // long press: factory reset -> setup
            Gesture::Long => {
                self.manifest = None;
                self.cache.clear();
                // Fresh provisioning always starts from the top of the handshake.
                self.setup_phase = SetupPhase::WaitingBluetooth;
                self.mode = Mode::Setup;
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
        // An OTA update in flight overrides every mode BELOW reset: the one
        // state where the user must not conclude the board hung and unplug it
        // mid-flash-write. (Reset still wins — the escape hatch is sacred.)
        if let Some(pct) = self.updating {
            let mut buf = self.screen_updating(pct);
            scale_buf(&mut buf, self.local_brightness);
            return buf;
        }
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
                if self.online || holding {
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
        if self.brightness_changed_at.is_some() {
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

    /// OTA in progress: amber title, the hold-to-reset-style outlined bar
    /// filled to `pct`, and the warning that matters. Progress comes from the
    /// firmware's own download counter — device-local, like every animation.
    fn screen_updating(&self, pct: u8) -> Vec<u32> {
        let mut c = Canvas::new();
        let amber = Rgb888::new(0xFF, 0x88, 0x00);
        center_text(&mut c, "UPDATING", 5, &FONT_5X8, amber);

        // Same outlined-bar language as hold-to-reset (proportional fill).
        let bar_x = 6i32;
        let bar_y = 13i32;
        let bar_w = FRAME_WIDTH as i32 - 12;
        let bar_h = 8i32;
        Rectangle::new(Point::new(bar_x, bar_y), Size::new(bar_w as u32, bar_h as u32))
            .into_styled(PrimitiveStyle::with_stroke(Rgb888::new(0x55, 0x55, 0x55), 1))
            .draw(&mut c)
            .ok();
        let fill_w = (bar_w - 2) as u32 * pct.min(100) as u32 / 100;
        if fill_w > 0 {
            Rectangle::new(
                Point::new(bar_x + 1, bar_y + 1),
                Size::new(fill_w, (bar_h - 2) as u32),
            )
            .into_styled(PrimitiveStyle::with_fill(amber))
            .draw(&mut c)
            .ok();
        }
        center_text(&mut c, "DO NOT UNPLUG", 27, &FONT_4X6, Rgb888::new(0x88, 0x88, 0x88));
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
        if self.wifi_up {
            draw_leg(&mut c, 15, green, None);
            draw_leg(&mut c, 38, dim, Some(self.anim % 5));
            center_text(&mut c, "FINDING SERVER", 27, &FONT_4X6, amber);
        } else {
            draw_leg(&mut c, 15, dim, Some(self.anim % 5));
            draw_leg(&mut c, 38, dim, None);
            center_text(&mut c, "JOINING WI-FI", 27, &FONT_4X6, amber);
        }
        c.into_u32()
    }

    fn screen_reset(&self) -> Vec<u32> {
        let mut c = Canvas::new();
        let (title, color) = if self.reset_done {
            ("RESET COMPLETE", Rgb888::new(0x33, 0xDD, 0x55)) // green
        } else {
            ("HOLD TO RESET", Rgb888::new(0xFF, 0x55, 0x33)) // red
        };
        center_text(&mut c, title, 6, &FONT_4X6, color);

        // Progress bar (outline + proportional fill).
        let bar_x = 6i32;
        let bar_y = 15i32;
        let bar_w = FRAME_WIDTH as i32 - 12;
        let bar_h = 9i32;
        Rectangle::new(Point::new(bar_x, bar_y), Size::new(bar_w as u32, bar_h as u32))
            .into_styled(PrimitiveStyle::with_stroke(Rgb888::new(0x55, 0x55, 0x55), 1))
            .draw(&mut c)
            .ok();
        let fill_w = (bar_w - 2) as u32 * self.reset_progress as u32 / 100;
        if fill_w > 0 {
            Rectangle::new(
                Point::new(bar_x + 1, bar_y + 1),
                Size::new(fill_w, (bar_h - 2) as u32),
            )
            .into_styled(PrimitiveStyle::with_fill(color))
            .draw(&mut c)
            .ok();
        }
        if self.reset_done {
            draw_checkmark(&mut c, FRAME_WIDTH as i32 / 2, bar_y + bar_h / 2);
        }
        // Version at the bottom — with OTA updates, the reset screen is the
        // handiest place to read what's running: hold the knob a moment,
        // release before the bar fills.
        if let Some(v) = &self.version {
            center_text(&mut c, v, 28, &FONT_4X6, Rgb888::new(0x88, 0x88, 0x88));
        }
        c.into_u32()
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
    let style = TextStyleBuilder::new()
        .alignment(Alignment::Center)
        .baseline(Baseline::Middle)
        .build();
    Text::with_text_style(s, Point::new(FRAME_WIDTH as i32 / 2, y_center), MonoTextStyle::new(font, color), style)
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
    #[rustfmt::skip]
    let px = [
        (29, 14), (30, 13), (31, 12), (32, 12), (33, 12), (34, 13), (35, 14), // outer arc
        (30, 16), (31, 15), (32, 15), (33, 15), (34, 16),                     // inner arc
        (32, 18),                                                             // dot
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

/// Transient brightness bar across the bottom rows (amber fill proportional to level).
fn overlay_brightness_bar(buf: &mut [u32], brightness: u8) {
    let bar_h = 7usize;
    let y0 = FRAME_HEIGHT - bar_h;
    let fill = (brightness as usize * FRAME_WIDTH) / 100;
    for y in y0..FRAME_HEIGHT {
        for x in 0..FRAME_WIDTH {
            buf[y * FRAME_WIDTH + x] = if x < fill { 0x00FF_CC00 } else { 0x0033_3333 };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // double press OFFLINE: inert — the pairing code means nothing until
        // the server has been told about it.
        b.button_down(2000);
        b.button_up(2040);
        b.button_down(2120);
        b.button_up(2160);
        assert_eq!(b.mode(), Mode::Running, "offline double-press is locked out");

        // Online, the same gesture opens the pairing screen.
        b.set_online(true);
        b.button_down(3000);
        b.button_up(3040);
        b.button_down(3120);
        b.button_up(3160);
        assert_eq!(b.mode(), Mode::Syncing);

        // A second double press closes it again…
        b.button_down(4000);
        b.button_up(4040);
        b.button_down(4120);
        b.button_up(4160);
        assert_eq!(b.mode(), Mode::Running, "double-press toggles the code screen off");

        // …and closing works even if the connection dropped while it was up.
        b.button_down(5000);
        b.button_up(5040);
        b.button_down(5120);
        b.button_up(5160);
        assert_eq!(b.mode(), Mode::Syncing);
        b.set_online(false);
        b.button_down(6000);
        b.button_up(6040);
        b.button_down(6120);
        b.button_up(6160);
        assert_eq!(b.mode(), Mode::Running, "leaving the code screen is never gated");
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
    /// (which owns the value); offline turns stay local and silent.
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
        assert!(b.take_outgoing().is_empty(), "offline turns are not queued");
    }

    /// The manifest's brightness is adopted as the local level — except while the
    /// knob's transient bar is up, so an echo in flight can't rubber-band a turn.
    #[test]
    fn manifest_brightness_adopted_unless_knob_active() {
        let mut b = Board::new();
        b.mode = Mode::Running;
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

    #[test]
    fn hold_fills_then_triggers_reset() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.button_down(0);
        b.tick(300); // within the grace window: nothing yet
        assert!(!b.reset_active());
        b.tick(800); // past grace (500): bar is filling
        assert!(b.reset_active());
        assert!(!b.reset_triggered());
        b.tick(11000); // past grace + fill (~10s): complete
        assert!(b.reset_triggered());
        // Releasing after completion keeps it latched (the firmware reboots us).
        b.button_up(3100);
        assert!(b.reset_triggered());
    }

    #[test]
    fn releasing_early_cancels_reset() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.button_down(0);
        b.tick(900); // bar showing
        assert!(b.reset_active());
        b.button_up(1200); // released before it fills
        assert!(!b.reset_active());
        assert!(!b.reset_triggered());
        // A long-ish hold no longer enters Setup (that's the app's job now).
        assert_eq!(b.mode(), Mode::Running);
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
    /// that with an empty rotation) is content too: a ws blip at 3am must not
    /// light the bedroom up with the connect screen.
    #[test]
    fn offline_grace_holds_a_deliberately_dark_panel() {
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
        assert_ne!(b.framebuffer(), dark, "a real outage still surfaces eventually");
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

    /// The OTA updating screen: distinct at 0/50/100 (the bar visibly fills),
    /// overrides Running content, and `None` clears it back exactly.
    #[test]
    fn updating_screen_progresses_overrides_and_clears() {
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

        b.set_updating(Some(0));
        let at0 = b.framebuffer();
        assert_ne!(at0, running, "updating overlay overrides Running content");
        b.set_updating(Some(50));
        let at50 = b.framebuffer();
        b.set_updating(Some(100));
        let at100 = b.framebuffer();
        assert_ne!(at0, at50, "0% vs 50% must differ (bar fills)");
        assert_ne!(at50, at100, "50% vs 100% must differ (bar fills)");
        assert_ne!(at0, at100, "0% vs 100% must differ (bar fills)");

        // A failed download clears the overlay: back to the exact running frame.
        b.set_updating(None);
        assert_eq!(b.framebuffer(), running, "None restores the running content");
    }

    /// Updating overrides every mode below reset — but hold-to-reset still wins:
    /// the escape hatch must never be hidden, even mid-download.
    #[test]
    fn reset_overlay_beats_updating_screen() {
        let mut b = Board::new();
        b.mode = Mode::Running;
        b.tick(0);
        b.set_updating(Some(50));
        let mut reference = Board::new();
        reference.mode = Mode::Running;
        reference.tick(0);
        // Same hold on both boards; the one mid-update must render identically.
        b.button_down(0);
        reference.button_down(0);
        b.tick(900); // past the grace window: reset bar up
        reference.tick(900);
        assert!(b.reset_active());
        assert_eq!(
            b.framebuffer(),
            reference.framebuffer(),
            "hold-to-reset renders over the updating screen"
        );

        // Asleep is overridden too ("every mode except reset").
        let mut asleep = Board::new();
        asleep.mode = Mode::Asleep;
        asleep.tick(0);
        assert!(asleep.framebuffer().iter().all(|&p| p == 0));
        asleep.set_updating(Some(50));
        assert!(
            asleep.framebuffer().iter().any(|&p| p != 0),
            "updating overlay lights up even an asleep panel"
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
    }
}
