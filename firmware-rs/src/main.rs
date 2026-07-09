//! Firmware (Gate 2b-2) — live games on the panel, all Rust.
//! WiFi + WebSocket feed board-core (`device` task); a `render` task blits
//! board-core's framebuffer to the HUB75 panel via esp-hub75. board-core is the
//! single source of truth, shared with an embassy mutex. `cargo run --release`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use board_core::{Board, Gesture};
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Runner, Stack, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Timer};
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
use esp_radio::wifi::{sta::StationConfig, Config, Interface, WifiController};
use scoreboard_protocol::{
    decode_frame, pubkey_hex, sign_hex, ClientMsg, Role, ServerMsg, FRAME_PIXELS, FRAME_WIDTH,
};
use esp_storage::FlashStorage;
use seesaw::Encoder;

mod ble;
mod brightness;
mod config;
mod creds;
mod identity;
mod seesaw;
mod ws;

esp_bootloader_esp_idf::esp_app_desc!();

const ROWS: usize = 32;
const COLS: usize = 64;
const NROWS: usize = compute_rows(ROWS);
const PLANES: usize = 7;
type FBType = DmaFrameBuffer<NROWS, COLS, PLANES>;
type SharedBoard = Mutex<CriticalSectionRawMutex, Board>;

/// Board-generated messages (e.g. knob press -> ButtonPress) on their way to the
/// server. The render task drains board-core's outbox into here; the ws task
/// selects on it alongside the socket read.
static OUTBOX: embassy_sync::channel::Channel<CriticalSectionRawMutex, ClientMsg, 4> =
    embassy_sync::channel::Channel::new();

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
    let hub75 = Hub75::new_async(p.LCD_CAM, pins, p.DMA_CH0, tx_descriptors, Rate::from_mhz(20))
        .expect("hub75");
    let fb_a = mk_static!(FBType, FBType::new());
    let fb_b = mk_static!(FBType, FBType::new());

    // Load persisted state BEFORE the first render, so the board never flashes at
    // full brightness on boot — it powers on at the level you left it.
    let mut flash = FlashStorage::new(p.FLASH);
    let stored_creds = creds::load(&mut flash);
    if let Some(level) = brightness::load(&mut flash) {
        board.lock().await.set_brightness(level);
        println!("[bright] restored {}%", level);
    }

    // The render task always runs — it shows games (WiFi mode) or the setup screen
    // (BLE mode), whatever board-core's current mode is.
    spawner.spawn(render(hub75, fb_a, fb_b, board).unwrap());

    match stored_creds {
        // Provisioned: connect WiFi, run the device protocol + encoder.
        Some((ssid, pass)) => {
            println!("[creds] loaded from flash; connecting to '{}'…", ssid);
            // Mint-or-load this board's Ed25519 identity; its pubkey is the board_id.
            let mut id_rng = Rng::new();
            let secret = identity::load_or_create(&mut flash, &mut id_rng);
            let board_id = pubkey_hex(&secret);
            println!("[id] board {}", board_id);
            let (mut controller, interfaces) =
                esp_radio::wifi::new(p.WIFI, Default::default()).unwrap();
            let station = Config::Station(
                StationConfig::default()
                    .with_ssid(ssid.as_str())
                    .with_password(pass),
            );
            controller.set_config(&station).unwrap();

            let net_config = embassy_net::Config::dhcpv4(Default::default());
            let rng = Rng::new();
            let seed = ((rng.random() as u64) << 32) | rng.random() as u64;
            let (stack, runner) = embassy_net::new(
                interfaces.station,
                net_config,
                mk_static!(StackResources<3>, StackResources::<3>::new()),
                seed,
            );

            spawner.spawn(connection(controller).unwrap());
            spawner.spawn(net_task(runner).unwrap());
            spawner.spawn(device(stack, board, board_id, secret).unwrap());

            // encoder (seesaw over I2C / STEMMA QT: SDA=16, SCL=17)
            let i2c = I2c::new(p.I2C0, I2cConfig::default())
                .expect("i2c")
                .with_sda(p.GPIO16)
                .with_scl(p.GPIO17);
            spawner.spawn(encoder_task(Encoder::new(i2c), board, true).unwrap());
            // Owns flash in WiFi mode: persists brightness + handles hold-to-reset.
            spawner.spawn(flash_task(flash, board).unwrap());
        }
        // Unprovisioned: show the setup screen + provision over BLE, then reboot.
        None => {
            println!("[creds] none — starting BLE provisioning");
            board.lock().await.gesture(Gesture::Long); // -> Setup screen
            // The encoder still dims the panel during setup (rotation only — there's
            // no screen to navigate to, so the button is suppressed).
            let i2c = I2c::new(p.I2C0, I2cConfig::default())
                .expect("i2c")
                .with_sda(p.GPIO16)
                .with_scl(p.GPIO17);
            spawner.spawn(encoder_task(Encoder::new(i2c), board, false).unwrap());
            ble::provision(p.BT, &mut flash).await;
        }
    }

    loop {
        Timer::after(Duration::from_secs(30)).await;
    }
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

        // Re-sample board-core at ~30fps into the cached frame.
        if last_update.elapsed() >= Duration::from_millis(33) {
            let mut b = board.lock().await;
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
async fn device(stack: Stack<'static>, board: &'static SharedBoard, board_id: String, secret: [u8; 32]) {
    let mut rx = alloc::vec![0u8; 2048];
    let mut tx = alloc::vec![0u8; 2048];
    // Sized for tall scrolling leaderboard frames (64-byte id + 128 bytes/row;
    // 16448 fits frames up to 128px — SCROLL_MAX_ROWS needs 68px).
    let mut frame = alloc::vec![0u8; 16448];

    loop {
        Timer::after(Duration::from_secs(1)).await;
        if stack.config_v4().is_none() {
            continue;
        }
        let mut sock = TcpSocket::new(stack, &mut rx[..], &mut tx[..]);
        sock.set_timeout(Some(Duration::from_secs(45)));

        println!("[ws] connecting to {}:{}", config::SERVER_IP, config::SERVER_PORT);
        if sock.connect((config::SERVER_IP, config::SERVER_PORT)).await.is_err() {
            println!("[ws] tcp connect failed; retry");
            Timer::after(Duration::from_secs(2)).await;
            continue;
        }
        let host = format!("{}:{}", config::SERVER_IP, config::SERVER_PORT);
        if ws::handshake(&mut sock, &host).await.is_err() {
            println!("[ws] handshake failed; retry");
            Timer::after(Duration::from_secs(2)).await;
            continue;
        }
        println!("[ws] connected");
        board.lock().await.set_online(true);
        // Parse state lives OUTSIDE the select so a read cancelled by an outbox
        // message resumes mid-frame instead of desyncing the stream.
        let mut reader = ws::FrameReader::new();

        loop {
            // Full-duplex on one socket: wait on the next server frame OR an
            // outgoing board message.
            use embassy_futures::select::{select, Either};
            let event = select(reader.read(&mut sock, &mut frame), OUTBOX.receive()).await;
            let read = match event {
                Either::Second(msg) => {
                    let _ = ws::write_text(&mut sock, &serde_json::to_vec(&msg).unwrap()).await;
                    continue;
                }
                Either::First(r) => r,
            };
            match read {
                Ok((ws::OP_TEXT, n)) => match serde_json::from_slice::<ServerMsg>(&frame[..n]) {
                    Ok(ServerMsg::Hello { challenge }) => {
                        let auth = ClientMsg::Auth {
                            role: Role::Device,
                            pubkey: board_id.clone(),
                            sig: sign_hex(&secret, challenge.as_bytes()),
                        };
                        let _ = ws::write_text(&mut sock, &serde_json::to_vec(&auth).unwrap()).await;
                        println!("[ws] authed");
                    }
                    Ok(ServerMsg::Manifest(m)) => {
                        let need = board.lock().await.on_manifest(m);
                        if let Some(req) = need {
                            let _ = ws::write_text(&mut sock, &serde_json::to_vec(&req).unwrap()).await;
                        }
                    }
                    _ => {}
                },
                Ok((ws::OP_BINARY, n)) => {
                    if let Some((id, px)) = decode_frame(&frame[..n]) {
                        board.lock().await.on_frame(id, px.to_vec());
                    }
                }
                Ok((ws::OP_PING, n)) => {
                    // Answered here (not inside the reader): these writes are
                    // post-select, so they can't be cancelled mid-frame.
                    let _ = ws::write_pong(&mut sock, &frame[..n]).await;
                }
                Ok(_) => {}
                Err(_) => {
                    println!("[ws] disconnected; reconnecting");
                    board.lock().await.set_online(false);
                    break;
                }
            }
        }
    }
}

/// Poll the seesaw encoder: rotation -> brightness, push -> board-core gesture.
#[embassy_executor::task]
async fn encoder_task(mut enc: Encoder, board: &'static SharedBoard, handle_button: bool) {
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
        // Suppressed during BLE setup — there's no screen to navigate to there.
        if handle_button {
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
}

/// Owns the flash peripheral in WiFi mode. Two jobs: (1) persist encoder brightness
/// once it settles (~2s after the last turn, so we don't wear flash on every
/// detent); (2) on hold-to-reset, wipe the Wi-Fi credentials and reboot into BLE
/// provisioning.
#[embassy_executor::task]
async fn flash_task(mut flash: FlashStorage<'static>, board: &'static SharedBoard) {
    let mut saved = board.lock().await.brightness();
    let mut target = saved;
    let mut dirty_at: Option<Instant> = None;
    loop {
        Timer::after(Duration::from_millis(400)).await;
        let (cur, reset) = {
            let b = board.lock().await;
            (b.brightness(), b.reset_triggered())
        };
        // Hold-to-reset: let the "reset complete" checkmark show for a beat, then
        // wipe creds + reboot — which comes back up in BLE provisioning mode.
        if reset {
            println!("[reset] clearing Wi-Fi credentials, rebooting into setup");
            Timer::after(Duration::from_millis(1000)).await;
            creds::clear(&mut flash);
            esp_hal::system::software_reset();
        }
        // Brightness persistence (debounced).
        if cur != target {
            target = cur;
            dirty_at = Some(Instant::now()); // restart the settle timer on each change
        }
        if let Some(t) = dirty_at {
            if t.elapsed() >= Duration::from_secs(2) {
                if target != saved && brightness::save(&mut flash, target) {
                    println!("[bright] persisted {}%", target);
                    saved = target;
                }
                dirty_at = None;
            }
        }
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    loop {
        match controller.connect_async().await {
            Ok(info) => {
                println!("[wifi] connected: {:?}", info);
                let _ = controller.wait_for_disconnect_async().await;
                println!("[wifi] disconnected; retrying");
            }
            Err(e) => {
                println!("[wifi] connect failed: {:?}", e);
                Timer::after(Duration::from_secs(3)).await;
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}
