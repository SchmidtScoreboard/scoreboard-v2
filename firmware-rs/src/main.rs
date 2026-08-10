//! Firmware (Gate 2b-2) — live games on the panel, all Rust.
//! WiFi + WebSocket feed board-core (`device` task); a `render` task blits
//! board-core's framebuffer to the HUB75 panel via esp-hub75. board-core is the
//! single source of truth, shared with an embassy mutex. `cargo run --release`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use board_core::{Board, Diagnostics, Gesture};
use core::sync::atomic::{AtomicI32, Ordering};
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Runner, Stack, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use embedded_graphics::{pixelcolor::Rgb888, prelude::*, Pixel};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::Pin;
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::rng::Rng;
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hub75::{
    framebuffer::{bitplane::plain::DmaFrameBuffer, compute_rows},
    Hub75, Hub75Pins16,
};
use esp_println::println;
use esp_radio::wifi::{
    ap::AccessPointInfo, scan::ScanConfig, sta::StationConfig, Config, Interface, WifiController,
};
use scoreboard_protocol::{
    decode_frame, pubkey_hex, sign_hex, ClientMsg, Role, ServerMsg, FRAME_PIXELS, FRAME_WIDTH,
};
use esp_storage::FlashStorage;
use seesaw::Encoder;

mod ble;
mod brightness;
mod config;
mod creds;
mod debuglog;
mod fw_pubkey;
mod identity;
mod link;
mod ota;
mod seesaw;
#[cfg(feature = "tls")]
mod tls;
mod ws;

esp_bootloader_esp_idf::esp_app_desc!();

/// esp-backtrace prints the panic, then calls this instead of halting (the
/// `custom-halt` feature). Halting would strand an OTA'd image forever — no
/// halted CPU runs a revert watchdog. Rebooting instead feeds ota::boot_check's
/// three-strike counter, so a panic-looping update reverts itself.
#[unsafe(no_mangle)]
extern "Rust" fn custom_halt() -> ! {
    // Let the panic text drain the UART before the reset takes it.
    for _ in 0..4_000_000 {
        core::hint::spin_loop();
    }
    esp_hal::system::software_reset()
}

const ROWS: usize = 32;
const COLS: usize = 64;
const NROWS: usize = compute_rows(ROWS);
const PLANES: usize = 7;
type FBType = DmaFrameBuffer<NROWS, COLS, PLANES>;
pub type SharedBoard = Mutex<CriticalSectionRawMutex, Board>;

/// Board-generated messages (e.g. knob press -> ButtonPress) on their way to the
/// server. The render task drains board-core's outbox into here; the ws task
/// selects on it alongside the socket read.
static OUTBOX: embassy_sync::channel::Channel<CriticalSectionRawMutex, ClientMsg, 4> =
    embassy_sync::channel::Channel::new();

/// Latest station RSSI (dBm), published by the `connection` task — the only
/// task that can reach the `WifiController`, which `rssi()` lives on — and
/// read by `diagnostics_task` for the menu's WIFI INFO screen. An atomic
/// letterbox instead of sharing the controller: `connect_async` holds the
/// controller across long awaits, so a mutex would starve the 5s poll.
/// `i32::MIN` = unknown (not associated, or the query failed).
const RSSI_UNKNOWN: i32 = i32::MIN;
static RSSI_DBM: AtomicI32 = AtomicI32::new(RSSI_UNKNOWN);

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    esp_alloc::heap_allocator!(size: 96 * 1024);
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);

    let timg0 = TimerGroup::new(p.TIMG0);
    let sw_int = SoftwareInterruptControl::new(p.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    // OTA self-check FIRST — before the HUB75/radio/I2C bring-up that a broken
    // update could panic in. Together with the rebooting panic handler, this is
    // the brick-proofing: every boot of an unconfirmed image lands here and
    // counts a strike before anything riskier runs (see ota::boot_check).
    let mut flash = FlashStorage::new(p.FLASH);
    let fw_unconfirmed = ota::boot_check(&mut flash);

    // board-core state, shared by the device + render tasks
    let board: &'static SharedBoard = mk_static!(SharedBoard, Mutex::new(Board::new()));

    // ---- HUB75 panel ----
    let tx_descriptors = esp_hub75::hub75_dma_descriptors!(FBType);
    let pins = Hub75Pins16 {
        red1: p.GPIO42.degrade(),
        grn1: p.GPIO41.degrade(),
        blu1: p.GPIO40.degrade(),
        red2: p.GPIO38.degrade(),
        grn2: p.GPIO39.degrade(),
        blu2: p.GPIO37.degrade(),
        addr0: p.GPIO45.degrade(),
        addr1: p.GPIO36.degrade(),
        addr2: p.GPIO48.degrade(),
        addr3: p.GPIO35.degrade(),
        addr4: p.GPIO21.degrade(),
        blank: p.GPIO14.degrade(),
        clock: p.GPIO2.degrade(),
        latch: p.GPIO47.degrade(),
    };
    // 10MHz, not 20: the driver blanks the panel for exactly ONE clock cycle
    // around each row-address change and latch (hub75-framebuffer's
    // BLANKING_DELAY). At 20MHz that window is 50ns — too short for the row
    // drivers to switch off and the address lines to settle, so the previous
    // row bleeds onto the next and "black" pixels glow faintly (worst next to
    // a bright full-width row, e.g. the brightness bar). Halving the clock
    // doubles the blanking window and the data settling time; one 64x32 panel
    // has refresh headroom to spare at 10MHz.
    let hub75 = Hub75::new_async(p.LCD_CAM, pins, p.DMA_CH0, tx_descriptors, Rate::from_mhz(10))
        .expect("hub75");
    let fb_a = mk_static!(FBType, FBType::new());
    let fb_b = mk_static!(FBType, FBType::new());

    // Load persisted state BEFORE the first render, so the board never flashes at
    // full brightness on boot — it powers on at the level you left it.
    let stored_creds = creds::load(&mut flash);
    if let Some(level) = brightness::load(&mut flash) {
        board.lock().await.set_brightness(level);
        println!("[bright] restored {}%", level);
    }
    // The running firmware version, on the boot screen and reported to the
    // server after every auth (which offers updates when we're behind).
    board.lock().await.set_version(String::from(env!("CARGO_PKG_VERSION")));
    println!("[fw] version {}", env!("CARGO_PKG_VERSION"));

    // Flash moves behind a mutex from here on: brightness persistence,
    // hold-to-reset, BLE credential saves, and the OTA updater all lock it per
    // short operation, so no path can starve another (reset must ALWAYS work,
    // including mid-download).
    let flash: &'static ota::SharedFlash = mk_static!(ota::SharedFlash, Mutex::new(flash));
    // An unconfirmed image must prove itself (one ws auth) within five minutes
    // or the watchdog reverts to the previous slot. Armed ONLY on the
    // provisioned path: BLE provisioning has no Wi-Fi and no server, so the
    // auth that clears the watchdog is impossible there by construction — a
    // board that hold-to-resets right after an update would sit in setup and
    // silently throw away a perfectly good image five minutes later. An update
    // that breaks credential loading is left to the three-strike boot counter
    // in ota::boot_check instead (every power cycle of an unconfirmed image
    // counts a strike, and the rebooting panic handler turns a crashing image
    // into strikes on its own). See ota.rs for the whole strategy.
    if fw_unconfirmed && stored_creds.is_some() {
        spawner.spawn(ota::revert_watchdog(flash).unwrap());
    }

    // The render task always runs — it shows games (WiFi mode) or the setup screen
    // (BLE mode), whatever board-core's current mode is.
    spawner.spawn(render(hub75, fb_a, fb_b, board).unwrap());

    // Encoder BEFORE the WiFi/BLE split: the button must be able to reset the
    // board in EVERY state it can be in, and one construction serves both
    // paths below. (seesaw over I2C / STEMMA QT: SDA=16, SCL=17)
    let i2c = I2c::new(p.I2C0, I2cConfig::default())
        .expect("i2c")
        .with_sda(p.GPIO16)
        .with_scl(p.GPIO17);
    spawner.spawn(encoder_task(Encoder::new(i2c), board).unwrap());

    match stored_creds {
        // Provisioned: connect WiFi, run the device protocol + encoder.
        Some(creds) => {
            println!("[creds] loaded from flash; connecting to '{}'…", creds.ssid);
            // Physical entropy, switched on before anything mints a secret.
            // esp-hal's plain `Rng` is only a TRUE random source while the RF
            // subsystem or the SAR ADC is feeding it noise, and NEITHER is up
            // here — the radio starts further down. `load_or_create` mints this
            // board's permanent Ed25519 seed on first boot, and that seed IS the
            // board_id: written once, read on every boot after. A pseudo-random
            // one would be a forgeable board identity for the life of the board.
            let trng_source = esp_hal::rng::TrngSource::new(p.RNG, p.ADC1);
            // `try_new` only fails with no live `TrngSource`; one is right above,
            // so this is an invariant, not a runtime condition.
            let trng = esp_hal::rng::Trng::try_new().expect("TrngSource constructed above");
            let secret = identity::load_or_create(&mut *flash.lock().await, &trng);
            let board_id = pubkey_hex(&secret);
            println!("[id] board {}", board_id);

            // Fresh 6-digit pairing code per boot: shown on the settings
            // menu's PAIR CODE screen (hold 3s -> menu; double-press is
            // reserved now) and registered with the server via EnterSetup so
            // the app's code entry actually resolves to this board.
            let code = format!("{:06}", trng.random() % 1_000_000);
            board.lock().await.set_setup_code(code.clone());
            println!("[pair] code {code}");

            // Hand the SAR ADC back BEFORE the radio comes up. Espressif's
            // equivalent (bootloader_random_enable) is documented as needing to
            // be disabled before Wi-Fi/BT or ADC init — it leaves the SAR ADC
            // force-powered in a sampling mode the PHY calibration also touches.
            // Nothing is lost: from wifi::new onward the RF subsystem is itself
            // the hardware RNG's entropy source. `trng` first — dropping the
            // source while a `Trng` is outstanding panics by design.
            drop(trng);
            drop(trng_source);

            // Server: BLE-provisioned host:port, else the compiled default.
            let (server_host, server_port) = if creds.server_host.is_empty() {
                (String::from(config::SERVER_HOST), config::SERVER_PORT)
            } else {
                (creds.server_host.clone(), creds.server_port)
            };
            // …and its TLS pin, provisioned the same way. `None` = trust the
            // compiled-in pin(s), which is every board talking to the hosted
            // server; `Some` is a self-hoster's own certificate and REPLACES
            // them (see link::Link::open).
            let server_pin = creds.server_pin;
            println!("[ws] server {server_host}:{server_port}");

            let (mut controller, interfaces) =
                esp_radio::wifi::new(p.WIFI, Default::default()).unwrap();
            let station = Config::Station(
                StationConfig::default()
                    .with_ssid(creds.ssid.as_str())
                    .with_password(creds.pass.clone()),
            );
            controller.set_config(&station).unwrap();

            let net_config = embassy_net::Config::dhcpv4(Default::default());
            let rng = Rng::new();
            let seed = ((rng.random() as u64) << 32) | rng.random() as u64;
            // FIVE socket slots, and the count is load-bearing: DHCP, DNS, the
            // websocket, and the OTA download socket — which runs CONCURRENTLY
            // with the websocket since 2026.7.6 (the ws stays open so the board
            // can report download progress), plus one spare. At <3> the pool
            // held exactly one TCP slot, so the download's TcpSocket::new
            // panicked in smoltcp ("socket set full") the instant an update
            // was accepted: the board rebooted on the spot, on every attempt,
            // and OTA was unusable until a USB flash. If a socket is ever
            // added, grow this — smoltcp's failure mode is a panic, not an Err.
            let (stack, runner) = embassy_net::new(
                interfaces.station,
                net_config,
                mk_static!(StackResources<5>, StackResources::<5>::new()),
                seed,
            );

            spawner.spawn(connection(controller).unwrap());
            spawner.spawn(net_task(runner).unwrap());
            // Facts for the menu's WIFI INFO screen, refreshed forever. The
            // strings are cloned here because `device` consumes the originals;
            // the id prefix is 8 chars to match what the screen can show (and
            // what the simulator feeds — same look on both).
            spawner.spawn(
                diagnostics_task(
                    stack,
                    board,
                    creds.ssid.clone(),
                    format!("{server_host}:{server_port}"),
                    board_id.chars().take(8).collect(),
                )
                .unwrap(),
            );
            spawner.spawn(
                device(
                    stack, board, board_id, secret, code, server_host, server_port, server_pin,
                    flash, fw_unconfirmed,
                )
                .unwrap(),
            );
            // Owns flash in WiFi mode: persists brightness + handles hold-to-reset.
            // (The encoder task itself is up already — spawned above the split.)
            spawner.spawn(flash_task(flash, board).unwrap());
        }
        // Unprovisioned: show the setup screen + provision over BLE, then reboot.
        None => {
            println!("[creds] none — starting BLE provisioning");
            board.lock().await.gesture(Gesture::Long); // -> Setup screen
            // Button stays live in BLE mode too — the board must NEVER be in a
            // state where hold-to-reset doesn't work. (Single press is inert on
            // the setup screen; board-core guards it. The encoder task itself
            // is up already — spawned above the split.)
            spawner.spawn(setup_reset_task(board, flash).unwrap());

            // Scan for nearby networks BEFORE BLE starts (no coex feature — the
            // two radios must not run at once). wifi::new() itself starts the
            // controller in station mode, which is all scan_async needs. A failed
            // scan just means an empty list: provisioning must never block on it.
            let scanned = match esp_radio::wifi::new(p.WIFI, Default::default()) {
                Ok((mut controller, interfaces)) => {
                    let aps = match controller.scan_async(&ScanConfig::default().with_max(12)).await
                    {
                        Ok(aps) => aps,
                        Err(e) => {
                            println!("[scan] scan failed: {e:?} — provisioning without a list");
                            Vec::new()
                        }
                    };
                    // Deinit Wi-Fi (WifiController::drop -> wifi_deinit) before
                    // the BleConnector claims the radio.
                    drop(interfaces);
                    drop(controller);
                    aps
                }
                Err(e) => {
                    println!("[scan] wifi init failed: {e:?} — provisioning without a list");
                    Vec::new()
                }
            };
            let networks = networks_payload(scanned);
            println!("[scan] offering networks: {}", networks.replace('\n', ", "));
            ble::provision(p.BT, flash, board, networks).await;
        }
    }

    loop {
        Timer::after(Duration::from_secs(30)).await;
    }
}

/// The BLE `networks` payload: UTF-8 SSIDs joined by '\n', strongest signal
/// first, deduped, no empty/hidden names, truncated at a whole-SSID boundary to
/// <=180 bytes so the value fits one ATT read at iOS's typical negotiated MTU.
fn networks_payload(mut aps: Vec<AccessPointInfo>) -> String {
    const MAX: usize = 180;
    aps.sort_unstable_by(|a, b| b.signal_strength.cmp(&a.signal_strength));
    let mut out = String::new();
    for ap in &aps {
        let ssid = ap.ssid.as_str();
        // Control chars would corrupt the record format ('\n' IS the separator,
        // and the app splits on it) — a beacon can carry arbitrary bytes.
        if ssid.is_empty()
            || ssid.chars().any(|c| c.is_control())
            || out.split('\n').any(|s| s == ssid)
        {
            continue;
        }
        let sep = usize::from(!out.is_empty());
        if out.len() + sep + ssid.len() > MAX {
            break;
        }
        if sep == 1 {
            out.push('\n');
        }
        out.push_str(ssid);
    }
    out
}

/// Double-buffered panel refresh: DMA `front` to the panel while blitting `back`,
/// so the panel is *always* being driven (no dark gap during the blit = no flicker).
/// board-core is re-sampled ~30fps; the cached frame is re-blitted every loop so
/// both buffers stay in sync.
#[embassy_executor::task]
async fn render(
    mut hub75: Hub75<'static, esp_hal::Async>,
    mut front: &'static mut FBType,
    mut back: &'static mut FBType,
    board: &'static SharedBoard,
) {
    let mut last_update = Instant::now();
    let mut buf = alloc::vec![0u32; FRAME_PIXELS];
    // Anything longer than this between panel passes is user-visible as a flash.
    // Two frame intervals: one missed pass is noise, a run of them is the bug.
    const STALL_REPORT: Duration = Duration::from_millis(66);
    let mut last_pass = Instant::now();
    loop {
        // Kick off the DMA for `front`; it streams to the panel for the whole loop.
        let mut xfer = match hub75.render(&*front) {
            Ok(x) => x,
            Err((_, h)) => {
                hub75 = h;
                Timer::after(Duration::from_millis(5)).await;
                continue;
            }
        };

        // A HUB75 panel emits light ONLY while this loop is driving it — there
        // is no framebuffer holding the image between passes. So any gap here is
        // a visibly black panel, and a gap is exactly what a cooperative
        // executor produces when some other task runs long without yielding.
        // Report them: a stall is invisible in a log that only prints events.
        let since_pass = last_pass.elapsed();
        if since_pass > STALL_REPORT {
            crate::dlog!("[panel] refresh stalled {}ms", since_pass.as_millis());
        }
        last_pass = Instant::now();

        // Re-sample board-core at ~30fps into the cached frame.
        if last_update.elapsed() >= Duration::from_millis(33) {
            let lock_wait = Instant::now();
            let mut b = board.lock().await;
            let waited = lock_wait.elapsed();
            if waited > STALL_REPORT {
                // Distinguishes "someone held the board mutex" from "the
                // executor was busy elsewhere" — different fixes.
                crate::dlog!("[panel] waited {}ms for the board lock", waited.as_millis());
            }
            b.tick(Instant::now().as_millis());
            buf.copy_from_slice(&b.framebuffer());
            let outgoing = b.take_outgoing();
            drop(b);
            // Queue board-generated messages for the ws task (dropped if it's full
            // or the socket is down — a press can simply be pressed again).
            for m in outgoing {
                let _ = OUTBOX.try_send(m);
            }
            last_update = Instant::now();
        }
        // Blit the cached frame into `back` (overlaps the DMA on `front`).
        let pixels = (0..FRAME_PIXELS).map(|i| {
            let c = buf[i];
            Pixel(
                Point::new((i % FRAME_WIDTH) as i32, (i / FRAME_WIDTH) as i32),
                Rgb888::new((c >> 16) as u8, (c >> 8) as u8, c as u8),
            )
        });
        let _ = back.draw_iter(pixels);

        let _ = xfer.wait_for_done().await;
        let (_, h) = xfer.wait();
        hub75 = h;
        core::mem::swap(&mut front, &mut back);
    }
}

/// WebSocket device-protocol loop feeding board-core.
#[embassy_executor::task]
#[allow(clippy::too_many_arguments)]
async fn device(
    stack: Stack<'static>,
    board: &'static SharedBoard,
    board_id: String,
    secret: [u8; 32],
    pair_code: String,
    server_host: String,
    server_port: u16,
    // The BLE-provisioned server pin, or `None` for the compiled-in default.
    server_pin: Option<[u8; 32]>,
    flash: &'static ota::SharedFlash,
    fw_unconfirmed: bool,
) {
    let mut rx = alloc::vec![0u8; 2048];
    let mut tx = alloc::vec![0u8; 2048];
    // Sized for tall scrolling leaderboard frames: 64-byte id + 1 codec byte +
    // 128 bytes/row. A 20-row leaderboard renders 128px = 16384 payload bytes,
    // so the wire frame is 16449 — the old 16448 fit the pre-codec format
    // EXACTLY, and the codec byte pushed it one over, which the reader rejects
    // as oversized and turns into a reconnect loop. 16512 restores slack.
    let mut frame = alloc::vec![0u8; 16512];
    // Last server IP that carried a live websocket. The Railway proxy drops
    // the socket every few minutes, so reconnects are routine — re-running
    // DNS on every pass added 2-5s of outage for an answer that almost never
    // changes. Cleared whenever the cached address fails to produce a working
    // websocket (a Railway redeploy CAN move the server), so a stale entry
    // costs exactly one failed pass before fresh DNS takes over.
    let mut cached_ip: Option<core::net::Ipv4Addr> = None;
    // TLS record + plaintext staging buffers, minted ONCE. embedded-tls never
    // allocates; it works out of buffers the caller hands it, and re-taking
    // ~9KB on every reconnect (the Railway proxy recycles the socket every
    // 5-15 minutes) would fragment the heap around the long-lived frame cache.
    let mut link_bufs = link::TlsBuffers::new();
    // Which key this board will actually accept, on the line right above the
    // first handshake — the one thing worth knowing when a self-hosted board
    // won't connect. cfg'd, not `if bytes() > 0`: `config::SERVER_PIN` has no
    // default and panics during const evaluation when it isn't set, so a
    // plaintext `--no-default-features` bring-up build must not name it at all.
    #[cfg(feature = "tls")]
    {
        let shown = match &server_pin {
            Some(p) => format!("{:02x}{:02x}{:02x}{:02x} (provisioned)", p[0], p[1], p[2], p[3]),
            None => format!("{} (built-in)", &config::SERVER_PIN[..8]),
        };
        println!("[tls] pinned to {shown}… ({} bytes of buffers)", link_bufs.bytes());
    }
    #[cfg(not(feature = "tls"))]
    println!(
        "[tls] DISABLED — plaintext bring-up build ({} bytes of buffers)",
        link_bufs.bytes()
    );

    // How long a join may keep failing before the panel stops saying "JOINING
    // WI-FI" and starts offering a way out. Long enough to cover a slow DHCP
    // lease or an AP still coming up after a power cut; short enough that
    // nobody stares at a spinner wondering whether the board is broken.
    const WIFI_STUCK_AFTER: Duration = Duration::from_secs(45);
    let mut wifi_down_since: Option<Instant> = None;

    loop {
        Timer::after(Duration::from_secs(1)).await;
        // Report the Wi-Fi/DHCP leg every pass — board-core's connect screen
        // shows JOINING WI-FI vs FINDING SERVER off this.
        let wifi_up = stack.config_v4().is_some();
        board.lock().await.set_wifi(wifi_up);
        if !wifi_up {
            // A wrong password looks EXACTLY like a slow join: the radio finds
            // the AP, fails the handshake, and retries forever behind an
            // identical screen. After the timeout, offer re-provisioning —
            // `set_wifi(true)` retires the offer if the network comes back.
            let since = *wifi_down_since.get_or_insert(Instant::now());
            if since.elapsed() >= WIFI_STUCK_AFTER {
                board.lock().await.set_wifi_stuck(true);
            }
            continue;
        }
        wifi_down_since = None;
        // NOTHING runs between Wi-Fi coming up and the server connection below.
        // This is where the boot path used to IP-geolocate the board's timezone
        // off a cleartext GET to a third party, up to three times, each capped
        // at 3s — the board's own DNS + connect + read in front of the one thing
        // it exists for. The zone is the SERVER's business now (it renders every
        // frame, clock and start-time label included), so the board no longer
        // has one to discover, and this pass goes straight to the socket.
        //
        // Resolve the host: IP literal (the LAN dev server) parses directly;
        // otherwise prefer the cached last-good IP and only hit DNS when
        // there isn't one (first pass, or the cache was invalidated below).
        let ip: core::net::Ipv4Addr = match server_host.parse() {
            Ok(ip) => ip,
            Err(_) => match cached_ip {
                Some(ip) => ip,
                None => {
                    match stack.dns_query(&server_host, embassy_net::dns::DnsQueryType::A).await {
                        Ok(addrs) if !addrs.is_empty() => match addrs[0] {
                            embassy_net::IpAddress::Ipv4(v4) => v4,
                        },
                        _ => {
                            println!("[ws] dns failed for {server_host}; retry");
                            Timer::after(Duration::from_secs(5)).await;
                            continue;
                        }
                    }
                }
            },
        };
        let mut sock = TcpSocket::new(stack, &mut rx[..], &mut tx[..]);
        // 25s = one server ping interval (20s, spawn_keepalive in
        // crates/server/src/ws.rs) + 5s margin. The Railway TCP proxy recycles
        // this connection every 5-15 min WITHOUT telling the board: the server
        // sees the error instantly, the board sees only silence and sits out
        // this whole timeout before reconnecting. A healthy link is never
        // silent longer than 20s (the board pongs, the server pings), so one
        // missed ping window + margin is proof the socket is dead. At the old
        // 45s the visible outage ran ~45-70s; this cuts detection to ~25-30s.
        sock.set_timeout(Some(Duration::from_secs(25)));

        println!("[ws] connecting to {server_host} ({ip}:{server_port})");
        if sock.connect((ip, server_port)).await.is_err() {
            // Never retry a possibly-stale address forever: dropping the
            // cache makes the next pass re-resolve, which is what recovers
            // when a Railway redeploy hands the server a new IP.
            println!("[ws] tcp connect failed; retry");
            cached_ip = None;
            Timer::after(Duration::from_secs(2)).await;
            continue;
        }
        let host = format!("{server_host}:{server_port}");
        // Set when the server orders a firmware update: the ws loop ends and the
        // download takes over this task's socket buffers (see below).
        let mut pending_update: Option<ota::UpdateJob> = None;
        // Everything that borrows the socket lives inside this block. The OTA
        // download below reuses this task's rx/tx buffers, so the link — and
        // the reader/writer halves it was built on — must be gone before
        // `sock` is.
        let established = 'session: {
            let halves = link::Halves::new(&mut sock);
            // TLS 1.3 with the server's public key pinned (src/tls.rs), or a
            // bare passthrough in a `--no-default-features` bring-up build.
            let mut link = match link::Link::open(&halves, &mut link_bufs, &server_host, server_pin).await {
                Ok(link) => link,
                Err(e) => {
                    // A pin mismatch, a forged signature, or a server that
                    // never proved possession at all. There is NO fallback to
                    // plaintext: 7878 still answers plain ws://, so falling
                    // back would hand any on-path attacker a downgrade for the
                    // price of breaking one handshake. This is simply a failed
                    // pass, and the retry below is the same one a refused TCP
                    // connection gets.
                    println!("[tls] {}; retry", e.as_str());
                    break 'session false;
                }
            };
            if ws::handshake(&mut link, &host).await.is_err() {
                // The transport came up but the endpoint isn't (or is no
                // longer) our server — a recycled IP can accept connections
                // yet fail here, so this must invalidate the cache too or
                // we'd loop on it.
                println!("[ws] handshake failed; retry");
                break 'session false;
            }
            println!("[ws] connected");
            board.lock().await.set_online(true);
            // Parse state lives OUTSIDE the read call so a frame that arrives in
            // pieces resumes mid-frame instead of desyncing the stream.
            let mut reader = ws::FrameReader::new();

            loop {
                // Full-duplex on one socket, but the outbox may only interrupt us
                // HERE — where nothing is half-read.
                //
                // `link.idle()` means no decrypted bytes are stranded above the
                // socket, and `wait_readable` consumes nothing, so dropping either
                // future loses nothing. Racing the read itself (which is what this
                // loop used to do) would strand a partial TLS record: embedded-tls
                // accumulates the 5-byte record header in a stack local, so a
                // cancelled read misframes everything after it. See link.rs.
                if link.idle() {
                    use embassy_futures::select::{select, Either};
                    // The 30s cap is not belt-and-braces, it is load-bearing:
                    // `wait_read_ready` parks until bytes ARRIVE, and a socket
                    // the peer has closed never delivers any — smoltcp reports
                    // "not readable", never "dead" — so without this the loop
                    // would wait on a corpse forever and the board would sit
                    // ONLINE showing stale scores. The old code noticed because
                    // it was inside a `read`, which does return an error. 30s
                    // rests on the same fact as the 25s socket timeout above: a
                    // healthy link is never silent for 20s, because the server
                    // pings and the board pongs.
                    let event = with_timeout(
                        Duration::from_secs(30),
                        select(halves.wait_readable(), OUTBOX.receive()),
                    )
                    .await;
                    match event {
                        Err(_) => {
                            println!("[ws] silent for 30s; reconnecting");
                            board.lock().await.set_online(false);
                            break;
                        }
                        Ok(Either::First(())) => {}
                        Ok(Either::Second(msg)) => {
                            let _ =
                                ws::write_text(&mut link, &serde_json::to_vec(&msg).unwrap()).await;
                            continue;
                        }
                    }
                    // Ship whatever the console produced while we were waiting.
                    // Here, not on a timer: this is the one point in the loop
                    // where nothing is half-read and a write is safe.
                    if crate::debuglog::enabled() {
                        let (mut lines, dropped) = crate::debuglog::drain();
                        if dropped > 0 {
                            lines.insert(0, alloc::format!("[dbg] ... {dropped} line(s) dropped"));
                        }
                        if !lines.is_empty() {
                            let msg = ClientMsg::DebugLog { lines };
                            let _ = ws::write_text(&mut link, &serde_json::to_vec(&msg).unwrap()).await;
                        }
                    }
                    {
                    }
                }
                match reader.read(&mut link, &mut frame).await {
                    Ok((ws::OP_TEXT, n)) => match serde_json::from_slice::<ServerMsg>(&frame[..n]) {
                        Ok(ServerMsg::Hello { challenge }) => {
                            let auth = ClientMsg::Auth {
                                role: Role::Device,
                                pubkey: board_id.clone(),
                                sig: sign_hex(&secret, challenge.as_bytes()),
                            };
                            let _ = ws::write_text(&mut link, &serde_json::to_vec(&auth).unwrap()).await;
                            // Register the pairing code on every connect so the
                            // app's code entry always resolves to this board.
                            let setup = ClientMsg::EnterSetup { code: pair_code.clone() };
                            let _ = ws::write_text(&mut link, &serde_json::to_vec(&setup).unwrap()).await;
                            // No `DeviceTimezone` here any more: the board has no
                            // zone of its own to report (see the boot loop above).
                            // The server still ACCEPTS the message — boards on
                            // older firmware keep sending it — so this is a
                            // firmware-side removal only, not a protocol change.
                            //
                            // Report the running firmware version — the server
                            // echoes it into BoardState and offers the hosted
                            // image when we're behind.
                            let ver = ClientMsg::DeviceVersion {
                                version: String::from(env!("CARGO_PKG_VERSION")),
                            };
                            let _ = ws::write_text(&mut link, &serde_json::to_vec(&ver).unwrap()).await;
                            println!("[ws] authed (pair code {pair_code})");
                        }
                        Ok(ServerMsg::AuthOk) => {
                            // Only OUR server can complete the signed auth, so the
                            // IP cache is written here — not after the handshake,
                            // which any generic websocket listener passes. A
                            // recycled IP that 101s but never auths would
                            // otherwise trap every future pass on the stale
                            // address with DNS never re-queried.
                            cached_ip = Some(ip);
                            // First contact with the server: if this boot is a fresh
                            // OTA image, this is the proof it works — persist Valid
                            // so the revert watchdog stands down.
                            if fw_unconfirmed {
                                ota::confirm_running_image(flash).await;
                            }
                        }
                        // Debug logging on/off. Deliberately not persisted:
                        // a board that reboots comes back quiet.
                        Ok(ServerMsg::SetDebugLog { on }) => {
                            crate::debuglog::set_enabled(on);
                            println!("[dbg] remote logging {}", if on { "ON" } else { "off" });
                        }
                        Ok(ServerMsg::SetupDone) => {
                            // Paired: leave the pairing screen if it's up.
                            board.lock().await.setup_done();
                        }
                        Ok(ServerMsg::NeedsPairing) => {
                            // Nobody is paired with this board: show the code
                            // unprompted (sticky until paired or dismissed).
                            board.lock().await.needs_pairing();
                        }
                        // The board has no RTC. This is its only clock source:
                        // anchor it, then advance it from the monotonic timer.
                        Ok(ServerMsg::Time { local_epoch_ms }) => {
                            board.lock().await.set_time(local_epoch_ms);
                        }
                        Ok(ServerMsg::Manifest(m)) => {
                            let need = board.lock().await.on_manifest(m);
                            if let Some(req) = need {
                                let _ = ws::write_text(&mut link, &serde_json::to_vec(&req).unwrap()).await;
                            }
                        }
                        Ok(ServerMsg::UpdateAvailable { version }) => {
                            // Arms (Some) or clears (None) the menu's UPDATE row.
                            // Selecting an armed row queues RequestUpdate in
                            // board-core's outbox, which drains through OUTBOX to
                            // this socket like any other board message — and the
                            // channel outlives a disconnect, so a tap made offline
                            // sends on the next connect instead of vanishing.
                            board.lock().await.set_update_available(version);
                        }
                        Ok(ServerMsg::UpdateFirmware { version, size, sha256, sig, path, channel }) => {
                            // Leave the ws loop: the download reuses this task's
                            // socket buffers (the connection can't survive that,
                            // and the server's keepalive stops anyway). No progress
                            // is reported anywhere — the app shows a spinner, and
                            // the whole download runs well under a minute.
                            println!("[ota] server offers {version} ({channel}) at {path} ({size} bytes)");
                            pending_update = Some(ota::UpdateJob { version, size, sha256, sig, path, channel });
                            break;
                        }
                        _ => {}
                    },
                    Ok((ws::OP_BINARY, n)) => {
                        if let Some((id, px)) = decode_frame(&frame[..n]) {
                            board.lock().await.on_frame(id, px.to_vec());
                        }
                    }
                    Ok((ws::OP_PING, n)) => {
                        // Answered here (not inside the reader): this write happens
                        // after the read returned, so it can't be cancelled
                        // mid-frame.
                        let _ = ws::write_pong(&mut link, &frame[..n]).await;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        println!("[ws] disconnected; reconnecting");
                        board.lock().await.set_online(false);
                        break;
                    }
                }
            }
            true
        };

        // The websocket is done. If the server ordered an update, run the
        // download HERE rather than in a task of its own: this task owns the
        // socket buffers, and the ws socket is dropped first so they're free.
        // The ws socket is dropped FIRST: the download reuses this task's
        // rx/tx buffers, and one-TCP-at-a-time keeps the socket-pool math
        // trivial (see the StackResources comment — the pool's failure mode
        // is a panic).
        drop(sock);
        if !established {
            // Never came up: a TLS or websocket handshake failure means this
            // address didn't produce a working link, so re-resolve next pass
            // and back off exactly as a refused connection does. This is the
            // whole "no fallback" story — a board that can't authenticate the
            // server just keeps trying, and if the reason is a broken OTA
            // image, `ota::revert_watchdog` rolls it back after five minutes.
            cached_ip = None;
            Timer::after(Duration::from_secs(2)).await;
            continue;
        }
        if let Some(job) = pending_update.take() {
            board.lock().await.set_online(false);
            match ota::download_and_install(
                stack, &mut rx, &mut tx, ip, server_port, &host, &job, flash, board,
            )
            .await
            {
                Ok(()) => {
                    // Keep the panel black for a beat (it already is — the
                    // update blanks it), then reboot into the new slot. The new
                    // image confirms itself after its first auth (or reverts —
                    // see ota.rs).
                    println!("[ota] {} installed; rebooting into the new image", job.version);
                    board.lock().await.set_updating(Some(100));
                    Timer::after(Duration::from_millis(1500)).await;
                    esp_hal::system::software_reset();
                }
                Err(e) => {
                    // A failed update is a NON-EVENT: clear the overlay and let
                    // the outer loop reconnect as if nothing happened — the old
                    // image never stopped running and otadata never moved.
                    println!("[ota] update failed: {e}; resuming normal operation");
                    board.lock().await.set_updating(None);
                }
            }
        }
    }
}

/// Poll the seesaw encoder: rotation -> brightness, push -> board-core gesture.
/// The button is live in EVERY mode — hold-to-reset must always work.
#[embassy_executor::task]
async fn encoder_task(mut enc: Encoder, board: &'static SharedBoard) {
    if enc.init() {
        println!("[enc] seesaw ready");
    } else {
        println!("[enc] seesaw not found (running without encoder)");
    }
    let mut last_pos = enc.position().await;
    let mut was_pressed = false;
    loop {
        Timer::after(Duration::from_millis(20)).await;
        if let Some(pos) = enc.position().await {
            if let Some(lp) = last_pos {
                if pos != lp {
                    board.lock().await.encoder(pos - lp);
                }
            }
            last_pos = Some(pos);
        }
        // The render task's tick() resolves single/double/long from these events.
        let pressed = enc.pressed().await;
        if pressed != was_pressed {
            let now_ms = Instant::now().as_millis();
            let mut b = board.lock().await;
            if pressed {
                b.button_down(now_ms);
            } else {
                b.button_up(now_ms);
            }
            was_pressed = pressed;
        }
    }
}

/// Flash housekeeping in WiFi mode. Two jobs: (1) persist encoder brightness
/// once it settles (~2s after the last turn, so we don't wear flash on every
/// detent); (2) on hold-to-reset, wipe the Wi-Fi credentials and reboot into BLE
/// provisioning. The flash itself is SHARED (embassy mutex) with the OTA
/// updater; both sides lock per short operation, so hold-to-reset keeps working
/// throughout a firmware download.
#[embassy_executor::task]
async fn flash_task(flash: &'static ota::SharedFlash, board: &'static SharedBoard) {
    let mut saved = board.lock().await.brightness();
    let mut target = saved;
    let mut dirty_at: Option<Instant> = None;
    loop {
        Timer::after(Duration::from_millis(400)).await;
        let (cur, reset, reprovision) = {
            let b = board.lock().await;
            (b.brightness(), b.reset_triggered(), b.reprovision_triggered())
        };
        // The Wi-Fi escape hatch: forget the NETWORK and come back up in BLE
        // provisioning, keeping the identity and therefore the pairing. A
        // factory reset would also work, but it un-pairs the board from every
        // phone to fix a mistyped password, which is not a trade anyone would
        // choose if the panel offered them the choice.
        if reprovision {
            println!("[wifi] user asked to re-provision; clearing creds and rebooting");
            {
                let mut f = flash.lock().await;
                creds::clear(&mut *f);
            }
            Timer::after(Duration::from_millis(200)).await;
            esp_hal::system::software_reset();
        }
        // Hold-to-reset: let the "reset complete" checkmark show for a beat, then
        // wipe creds + reboot — which comes back up in BLE provisioning mode.
        if reset {
            // Full factory wipe: credentials, the identity seed (the next
            // boot mints a fresh one — a reset board is a brand-new
            // scoreboard, unpaired from every phone), and the stored
            // brightness. The 1s beat lets RESET COMPLETE show AND gives the
            // ws task its window to flush the best-effort FactoryReset the
            // latch queued, so the server forgets the old id too.
            println!("[reset] full factory wipe, rebooting into setup");
            Timer::after(Duration::from_millis(1000)).await;
            {
                let mut f = flash.lock().await;
                creds::clear(&mut *f);
                identity::clear(&mut f);
                brightness::clear(&mut f);
            }
            esp_hal::system::software_reset();
        }
        // Brightness persistence (debounced).
        if cur != target {
            target = cur;
            dirty_at = Some(Instant::now()); // restart the settle timer on each change
        }
        if let Some(t) = dirty_at {
            if t.elapsed() >= Duration::from_secs(2) {
                if target != saved && brightness::save(&mut *flash.lock().await, target) {
                    println!("[bright] persisted {}%", target);
                    saved = target;
                }
                dirty_at = None;
            }
        }
    }
}

/// Hold-to-reset watcher for BLE provisioning mode (flash_task owns the reset
/// in WiFi mode). No creds exist in this mode, but the identity and brightness
/// might — a reset must produce a brand-new scoreboard from EVERY state, or
/// "reset then hand it to a friend" leaks the old pairing identity. The
/// GESTURE must work everywhere, or the board can get stuck un-resettable.
#[embassy_executor::task]
async fn setup_reset_task(board: &'static SharedBoard, flash: &'static ota::SharedFlash) {
    loop {
        Timer::after(Duration::from_millis(400)).await;
        if board.lock().await.reset_triggered() {
            println!("[reset] full factory wipe from setup mode, rebooting");
            Timer::after(Duration::from_millis(1000)).await;
            {
                let mut f = flash.lock().await;
                creds::clear(&mut *f);
                identity::clear(&mut f);
                brightness::clear(&mut f);
            }
            esp_hal::system::software_reset();
        }
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    loop {
        match controller.connect_async().await {
            Ok(info) => {
                println!("[wifi] connected: {:?}", info);
                // While associated, publish the RSSI every ~5s for the WIFI
                // INFO screen (esp-radio's `WifiController::rssi`, valid only
                // in station mode after connect — exactly this window). The
                // timeout-and-retry shape is safe against a race: if the
                // disconnect event fires while we're outside the wait,
                // re-creating `wait_for_disconnect_async` returns
                // `Err(NotConnected)` immediately rather than waiting forever.
                loop {
                    let dbm = controller.rssi().unwrap_or(RSSI_UNKNOWN);
                    RSSI_DBM.store(dbm, Ordering::Relaxed);
                    // Ok = the wait completed (disconnected, or already found
                    // not-connected); Err = the 5s timeout — still up, poll on.
                    if with_timeout(
                        Duration::from_secs(5),
                        controller.wait_for_disconnect_async(),
                    )
                    .await
                    .is_ok()
                    {
                        break;
                    }
                }
                // A stale reading would lie on the diagnostics screen right
                // when someone's debugging a drop — unknown beats wrong.
                RSSI_DBM.store(RSSI_UNKNOWN, Ordering::Relaxed);
                println!("[wifi] disconnected; retrying");
            }
            Err(e) => {
                println!("[wifi] connect failed: {:?}", e);
                Timer::after(Duration::from_secs(3)).await;
            }
        }
    }
}

/// Feed board-core the facts for the settings menu's WIFI INFO screen, every
/// ~5s, forever. This screen matters MOST when the board is offline and
/// half-broken, so everything here is local: SSID from the stored creds, RSSI
/// from the `connection` task's letterbox, IP from the embassy-net stack, the
/// server endpoint we dial, and free heap from esp-alloc. Costs three small
/// String clones a pass — nothing that can bloat over a season of uptime.
#[embassy_executor::task]
async fn diagnostics_task(
    stack: Stack<'static>,
    board: &'static SharedBoard,
    ssid: String,
    server: String,
    board_id_prefix: String,
) {
    loop {
        let rssi = RSSI_DBM.load(Ordering::Relaxed);
        let d = Diagnostics {
            ssid: Some(ssid.clone()),
            // The C API reports i32 but real 802.11 RSSI lives in -100..0;
            // clamp rather than truncate in case the radio ever reports junk.
            rssi_dbm: (rssi != RSSI_UNKNOWN)
                .then(|| rssi.clamp(i8::MIN as i32, i8::MAX as i32) as i8),
            ip: stack.config_v4().map(|c| c.address.address().octets()),
            server: Some(server.clone()),
            board_id_prefix: Some(board_id_prefix.clone()),
            free_heap_bytes: Some(esp_alloc::HEAP.free() as u32),
        };
        board.lock().await.set_diagnostics(d);
        Timer::after(Duration::from_secs(5)).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}
