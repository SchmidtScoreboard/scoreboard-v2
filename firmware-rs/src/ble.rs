//! BLE WiFi provisioning (trouble-host GATT). Runs when flash has no credentials:
//! advertise as "Scoreboard Setup", accept a connection, receive "ssid\npassword"
//! written to the credentials characteristic, persist it to flash, then reboot into
//! WiFi mode. Test from a phone with nRF Connect (or the app) before iOS support.

use crate::config;
use crate::creds;
use crate::SharedBoard;
use alloc::string::String;
use board_core::SetupPhase;
use bt_hci::controller::ExternalController;
use embassy_futures::join::join;
use embassy_time::{Duration, Timer};
use esp_hal::peripherals::BT;
use esp_println::println;
use esp_radio::ble::controller::BleConnector;
use trouble_host::prelude::*;

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 2;

#[gatt_server]
struct Server {
    prov: Provisioning,
}

/// Custom 128-bit UUIDs for the provisioning service + characteristic.
#[gatt_service(uuid = "21ba0000-5363-6f72-6562-6f6172640001")]
struct Provisioning {
    /// WiFi SSID (UTF-8). Write this, then the password — provisioning saves + reboots.
    #[characteristic(uuid = "21ba0000-5363-6f72-6562-6f6172640002", write, value = [0u8; 64])]
    ssid: [u8; 64],
    /// WiFi password (UTF-8).
    #[characteristic(uuid = "21ba0000-5363-6f72-6562-6f6172640003", write, value = [0u8; 64])]
    password: [u8; 64],
    /// Server "host:port" (UTF-8, optional — write BEFORE the password, which
    /// triggers the save). Empty = the firmware's compiled default server.
    #[characteristic(uuid = "21ba0000-5363-6f72-6562-6f6172640004", write, value = [0u8; 96])]
    server: [u8; 96],
    /// Nearby Wi-Fi networks, scanned before advertising: UTF-8 SSIDs joined by
    /// '\n', strongest first, deduped, zero-padded to the fixed size. 180 bytes
    /// keeps the whole value inside one ATT read at iOS's typical MTU (185).
    #[characteristic(uuid = "21ba0000-5363-6f72-6562-6f6172640005", read, value = [0u8; 180])]
    networks: [u8; 180],
    /// Server TLS pin: the 64 hex characters `gen-tls-cert` prints (UTF-8,
    /// optional — write BEFORE the password, like `server`). This is what makes
    /// self-hosting possible: a board pointed at your own server must trust
    /// YOUR certificate, and the compiled-in pin is only a default. Absent =
    /// keep that default. Write-only, and a malformed value is REFUSED at the
    /// ATT layer rather than stored (see `gatt_loop`).
    #[characteristic(uuid = "21ba0000-5363-6f72-6562-6f6172640006", write, value = [0u8; 64])]
    pin: [u8; 64],
}

pub async fn provision(
    bt: BT<'static>,
    flash: &'static crate::ota::SharedFlash,
    board: &'static SharedBoard,
    networks: String,
) -> ! {
    let transport = BleConnector::new(bt, Default::default()).unwrap();
    // 20 HCI command slots — matches the trouble-host/esp reference examples.
    // (1 slot stalls the multi-command advertising setup.)
    let controller = ExternalController::<_, 20>::new(transport);
    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
    // Fresh random address EVERY boot: iOS caches the GATT table per address,
    // so a fixed one makes phones that met an older firmware silently miss any
    // characteristic added since (there's no app-side way to flush the cache).
    // A new address = a new device = a fresh discovery. Top two bits must be
    // 11 for a valid static random address (the array is little-endian).
    let rng = esp_hal::rng::Rng::new();
    let mut addr = [0u8; 6];
    for b in addr.iter_mut() {
        *b = rng.random() as u8;
    }
    addr[5] |= 0b1100_0000;
    let address = Address::random(addr);
    let stack = trouble_host::new(controller, &mut resources).set_random_address(address);
    let Host { mut peripheral, runner, .. } = stack.build();
    println!("[ble] stack built");

    let server = Server::new_with_config(GapConfig::Peripheral(PeripheralConfig {
        name: "Scoreboard Setup",
        appearance: &appearance::power_device::GENERIC_POWER_DEVICE,
    }))
    .unwrap();

    // Publish the pre-scanned network list on the read characteristic
    // (zero-padded to its fixed 180-byte size).
    let mut nets = [0u8; 180];
    let n = networks.len().min(nets.len());
    nets[..n].copy_from_slice(&networks.as_bytes()[..n]);
    if let Err(e) = server.prov.networks.set(&server, &nets) {
        println!("[ble] networks characteristic set failed: {:?}", e);
    }

    println!("[ble] advertising 'Scoreboard Setup' — write SSID + password chars to provision");
    let _ = join(ble_task(runner), async {
        loop {
            match advertise("Scoreboard Setup", &mut peripheral, &server).await {
                Ok(conn) => {
                    // Phone attached: the setup screen switches to "waiting for
                    // Wi-Fi details".
                    board.lock().await.set_setup_phase(SetupPhase::WaitingCreds);
                    gatt_loop(&server, &conn, flash, board).await;
                }
                Err(_) => println!("[ble] advertise error"),
            }
        }
    })
    .await;
    loop {}
}

async fn ble_task<C: Controller, P: PacketPool>(mut runner: Runner<'_, C, P>) {
    loop {
        if let Err(e) = runner.run().await {
            println!("[ble] runner error: {:?}", e);
            return;
        }
    }
}

async fn gatt_loop<P: PacketPool>(
    server: &Server<'_>,
    conn: &GattConnection<'_, '_, P>,
    flash: &'static crate::ota::SharedFlash,
    board: &'static SharedBoard,
) {
    let ssid_h = server.prov.ssid;
    let pass_h = server.prov.password;
    let server_h = server.prov.server;
    let pin_h = server.prov.pin;
    let mut ssid_v: Option<String> = None;
    let mut pass_v: Option<String> = None;
    let mut server_v: Option<String> = None;
    let mut pin_v: Option<[u8; creds::PIN_LEN]> = None;
    // A pin arrived and did not parse. Latched, and it BLOCKS the save below:
    // saving without it would look like success while the board quietly kept
    // the compiled-in pin, which for a self-hoster means a board that reboots,
    // dials their server, and rejects it forever with no clue why. A later
    // well-formed write clears the latch.
    let mut pin_bad = false;
    let clean = |b: &[u8]| core::str::from_utf8(b).ok().map(|s| String::from(s.trim_end_matches('\0')));
    loop {
        match conn.next().await {
            GattConnectionEvent::Disconnected { .. } => {
                // Phone left without finishing — back to "connect over Bluetooth".
                board.lock().await.set_setup_phase(SetupPhase::WaitingBluetooth);
                return;
            }
            GattConnectionEvent::Gatt { event } => {
                // Set when the written value is unusable, so the write is
                // answered with an ATT error instead of an ACK — the app sees
                // the failure on the characteristic that caused it, which is
                // the only place a 64x32 panel could never explain it.
                let mut refuse = None;
                if let GattEvent::Write(e) = &event {
                    if e.handle() == ssid_h.handle {
                        ssid_v = clean(e.data());
                        println!("[ble] ssid received");
                    } else if e.handle() == pass_h.handle {
                        pass_v = clean(e.data());
                        println!("[ble] password received");
                    } else if e.handle() == server_h.handle {
                        server_v = clean(e.data());
                        println!("[ble] server received");
                    } else if e.handle() == pin_h.handle {
                        match parse_pin(e.data()) {
                            PinWrite::Pin(p) => {
                                pin_v = Some(p);
                                pin_bad = false;
                                println!("[ble] server pin received ({:02x}{:02x}…)", p[0], p[1]);
                            }
                            PinWrite::Cleared => {
                                pin_v = None;
                                pin_bad = false;
                                println!("[ble] server pin cleared — using the built-in pin");
                            }
                            PinWrite::Malformed => {
                                // Never store a broken pin, and never fall back
                                // to the built-in one behind the user's back.
                                pin_v = None;
                                pin_bad = true;
                                refuse = Some(AttErrorCode::VALUE_NOT_ALLOWED);
                                println!(
                                    "[ble] server pin REJECTED ({}B) — expected 64 hex characters \
                                     from `gen-tls-cert`, not all-zero",
                                    e.data().len()
                                );
                            }
                        }
                    }
                }
                let reply = match refuse {
                    Some(code) => event.reject(code),
                    None => event.accept(),
                };
                if let Ok(reply) = reply {
                    reply.send().await;
                }
                // Save + reboot once both SSID and password have been written
                // (the server and pin characteristics are optional and must
                // come first).
                if let (Some(s), Some(p)) = (&ssid_v, &pass_v) {
                    // A pin was offered and was garbage: refuse the whole save
                    // rather than silently provision a board that cannot reach
                    // the server it was just pointed at. The panel stays on
                    // "WAITING FOR WI-FI" — which is the truth — and rewriting
                    // the characteristic retries from here.
                    if pin_bad {
                        println!("[ble] creds NOT saved — the server pin was malformed; rewrite it");
                        continue;
                    }
                    // "host:port" -> (host, port); malformed/absent = default.
                    let (host, port) = server_v
                        .as_deref()
                        .and_then(|sv| {
                            let (h, pt) = sv.rsplit_once(':')?;
                            Some((String::from(h), pt.parse::<u16>().ok()?))
                        })
                        .unwrap_or((String::new(), 0));
                    // A PIN AND A HOST ARE INSEPARABLE. The built-in pin belongs
                    // to the built-in server and to nothing else, so every
                    // mismatched pairing below is a board that dials one server
                    // while trusting another's key — a handshake that can never
                    // succeed, on a board that looks like it provisioned fine.
                    // Cheap to catch here, and the only exit otherwise is
                    // hold-to-reset by someone who has no idea why.
                    //
                    // This also covers a client that writes the characteristics
                    // in the wrong ORDER: the password triggers this save, so a
                    // pin written after it never arrives, and that lands here as
                    // "custom host, no pin" rather than silently saving a record
                    // missing the pin. (The app chains its writes correctly;
                    // third-party GATT clients following the docs may not.)
                    let custom_host = !host.is_empty()
                        && (host != config::SERVER_HOST || port != config::SERVER_PORT);
                    if pin_v.is_some() && host.is_empty() {
                        println!(
                            "[ble] creds NOT saved — a server pin was given with no server \
                             address; write the address too (or omit the pin to use the default)"
                        );
                        continue;
                    }
                    if custom_host && pin_v.is_none() {
                        println!(
                            "[ble] creds NOT saved — server '{host}' is not the built-in one and \
                             no pin was given; that board could never complete a TLS handshake. \
                             Write the pin characteristic BEFORE the password."
                        );
                        continue;
                    }
                    if !s.is_empty()
                        && creds::save(
                            &mut *flash.lock().await,
                            s,
                            p,
                            &host,
                            port,
                            pin_v.as_ref(),
                        )
                    {
                        println!(
                            "[ble] saved creds for '{}' (server '{}', {} pin) — rebooting",
                            s,
                            host,
                            if pin_v.is_some() { "provisioned" } else { "built-in" }
                        );
                        // Show "WI-FI SAVED" for a beat before the reboot wipes
                        // the panel — otherwise the confirmation is never seen.
                        board.lock().await.set_setup_phase(SetupPhase::Saved);
                        Timer::after(Duration::from_millis(900)).await;
                        esp_hal::system::software_reset();
                    }
                    // Only reachable when the save was REFUSED — the success arm
                    // above reboots. A refusal used to be silent, which is the
                    // worst possible failure here: the phone's write was ACKed at
                    // the GATT layer, so the app declares success while the board
                    // has nothing in flash and will come back up unprovisioned.
                    // Say exactly why on the console, and deliberately leave the
                    // panel in WaitingCreds — "still waiting for Wi-Fi details" is
                    // the truth, and a rewritten characteristic retries from here.
                    println!(
                        "[ble] creds REJECTED (ssid {}B, pass {}B, host {}B; flash record allows 32/64/96, ssid must be non-empty) — still waiting",
                        s.len(),
                        p.len(),
                        host.len()
                    );
                }
            }
            _ => {}
        }
    }
}

/// What a write to the pin characteristic meant.
enum PinWrite {
    /// 64 hex characters that decode to a usable pin.
    Pin([u8; creds::PIN_LEN]),
    /// Empty: no pin, keep the firmware's compiled-in default. Exactly what an
    /// empty `server` value means — and what an app that writes every
    /// characteristic unconditionally sends for a hosted board, which must not
    /// be mistaken for a broken pin.
    Cleared,
    /// Anything else. Refused, never stored.
    Malformed,
}

/// Read the pin characteristic's value.
///
/// Tolerant about the wrapper — the value is zero-padded to its fixed size on
/// the wire, and a pasted string arrives with whatever whitespace came along —
/// and strict about the content: exactly 64 hex digits, never the all-zero
/// digest. `creds::pin_from_hex_str` holds a provisioned pin to the same rules
/// `tls::pin_from_hex` holds the compiled-in ones to, minus the const assert.
fn parse_pin(raw: &[u8]) -> PinWrite {
    let Ok(s) = core::str::from_utf8(raw) else {
        return PinWrite::Malformed;
    };
    let s = s.trim_matches(|c: char| c == '\0' || c.is_ascii_whitespace());
    if s.is_empty() {
        return PinWrite::Cleared;
    }
    match creds::pin_from_hex_str(s) {
        Some(pin) => PinWrite::Pin(pin),
        None => PinWrite::Malformed,
    }
}

async fn advertise<'a, 'b, C: Controller>(
    name: &'a str,
    peripheral: &mut Peripheral<'a, C, DefaultPacketPool>,
    server: &'b Server<'a>,
) -> Result<GattConnection<'a, 'b, DefaultPacketPool>, BleHostError<C::Error>> {
    let mut adv_data = [0u8; 31];
    let len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::CompleteLocalName(name.as_bytes()),
        ],
        &mut adv_data,
    )?;
    let advertiser = peripheral
        .advertise(
            &Default::default(),
            Advertisement::ConnectableScannableUndirected {
                adv_data: &adv_data[..len],
                scan_data: &[],
            },
        )
        .await?;
    println!("[ble] advertiser ACTIVE — should be visible now");
    let conn = advertiser.accept().await?.with_attribute_server(server)?;
    println!("[ble] phone connected");
    Ok(conn)
}
