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
    brightness_changed_at: Option<u64>, // transient brightness-bar overlay window

    // hold-to-reset (factory reset of the stored Wi-Fi credentials)
    reset_active: bool,         // bar on screen (held past the grace window)
    reset_progress: u8,         // 0..=100 fill
    reset_done: bool,           // fill complete — firmware wipes creds + reboots
    reset_done_at: Option<u64>, // for the simulator to auto-dismiss (device reboots first)

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
            brightness_changed_at: None,
            reset_active: false,
            reset_progress: 0,
            reset_done: false,
            reset_done_at: None,
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

    /// Server/app-controlled brightness from the latest manifest (what `SetBrightness`
    /// changes) — distinct from the local encoder knob. 100 until a manifest arrives.
    pub fn server_brightness(&self) -> u8 {
        self.manifest.as_ref().map_or(100, |m| m.brightness)
    }

    /// Display power from the latest manifest (what `SetPower` toggles).
    pub fn power(&self) -> bool {
        self.manifest.as_ref().map_or(true, |m| m.display_on)
    }

    /// Restore a persisted brightness on boot (clamped to the encoder range).
    pub fn set_brightness(&mut self, level: u8) {
        self.local_brightness = level.clamp(10, 100);
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
    pub fn set_online(&mut self, online: bool) {
        self.online = online;
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
    pub fn encoder(&mut self, delta: i32) {
        self.local_brightness = (self.local_brightness as i32 + delta * 10).clamp(10, 100) as u8;
        self.brightness_changed_at = Some(self.clock);
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
        self.manifest = Some(m);
        self.idx = 0;
        self.last_advance = self.clock;
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
                } else {
                    self.mode = if self.mode == Mode::Running {
                        Mode::Asleep
                    } else {
                        Mode::Running
                    };
                }
            }
            // double press: enter the syncing/setup phase
            Gesture::Double => self.mode = Mode::Syncing,
            // long press: factory reset -> setup
            Gesture::Long => {
                self.manifest = None;
                self.cache.clear();
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
        let mut buf = match self.mode {
            Mode::Booting => self.screen_boot(),
            Mode::Setup => self.screen_setup(),
            Mode::Syncing => self.screen_syncing(),
            Mode::Asleep => return vec![0u32; FRAME_PIXELS],
            Mode::Running => {
                if self.online {
                    self.screen_running()
                } else {
                    self.screen_reconnecting()
                }
            }
        };
        // The encoder brightness dims every live screen (boot/setup/sync/running);
        // the server "manifest" brightness is already folded into screen_running.
        scale_buf(&mut buf, self.local_brightness);
        // Transient indicator for ~2s after a knob turn, drawn at full level so it
        // stays legible even when the screen itself is dimmed.
        if self.brightness_changed_at.is_some() {
            overlay_brightness_bar(&mut buf, self.local_brightness);
        }
        buf
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
        // Server/manifest brightness only; the encoder brightness is applied to the
        // whole framebuffer in `framebuffer()` (so it also dims setup/boot/sync).
        let bright = m.brightness.min(100) as u32;
        let scale = |v: u8| ((v as u32 * bright) / 100) & 0xFF;
        for i in 0..FRAME_PIXELS {
            if 2 * i + 1 < px.len() {
                let c = u16::from_le_bytes([px[2 * i], px[2 * i + 1]]);
                let (r, g, b) = rgb565_to_888(c);
                buf[i] = (scale(r) << 16) | (scale(g) << 8) | scale(b);
            }
        }
        buf
    }

    fn screen_boot(&self) -> Vec<u32> {
        let mut c = Canvas::new();
        center_text(&mut c, "SCOREBOARD", 12, &FONT_5X8, Rgb888::new(0x33, 0xAA, 0xFF));
        spinner(&mut c, self.anim, 24);
        c.into_u32()
    }

    fn screen_setup(&self) -> Vec<u32> {
        let mut c = Canvas::new();
        center_text(&mut c, "SETUP", 6, &FONT_5X8, Rgb888::WHITE);
        center_text(&mut c, &self.setup_code, 18, &FONT_6X10, Rgb888::new(0xFF, 0xCC, 0x00));
        center_text(&mut c, "in the app", 28, &FONT_4X6, Rgb888::new(0x88, 0x88, 0x88));
        c.into_u32()
    }

    fn screen_syncing(&self) -> Vec<u32> {
        let mut c = Canvas::new();
        center_text(&mut c, "PAIR CODE", 6, &FONT_4X6, Rgb888::new(0x88, 0x88, 0x88));
        center_text(&mut c, &self.setup_code, 17, &FONT_6X10, Rgb888::new(0x33, 0xAA, 0xFF));
        spinner(&mut c, self.anim, 28);
        c.into_u32()
    }

    fn screen_reconnecting(&self) -> Vec<u32> {
        let mut c = Canvas::new();
        center_text(&mut c, "RECONNECTING", 12, &FONT_4X6, Rgb888::new(0xFF, 0x88, 0x00));
        spinner(&mut c, self.anim, 22);
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

/// Scale every pixel by `brightness` (10..=100) — the encoder's global dimmer.
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

        // double press -> syncing
        b.button_down(2000);
        b.button_up(2040);
        b.button_down(2120);
        b.button_up(2160);
        assert_eq!(b.mode(), Mode::Syncing);
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
}
