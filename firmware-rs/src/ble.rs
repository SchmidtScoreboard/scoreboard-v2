//! BLE WiFi provisioning (trouble-host GATT). Runs when flash has no credentials:
//! advertise as "Scoreboard Setup", accept a connection, receive "ssid\npassword"
//! written to the credentials characteristic, persist it to flash, then reboot into
//! WiFi mode. Test from a phone with nRF Connect (or the app) before iOS support.

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
    let mut ssid_v: Option<String> = None;
    let mut pass_v: Option<String> = None;
    let mut server_v: Option<String> = None;
    let clean = |b: &[u8]| core::str::from_utf8(b).ok().map(|s| String::from(s.trim_end_matches('\0')));
    loop {
        match conn.next().await {
            GattConnectionEvent::Disconnected { .. } => {
                // Phone left without finishing — back to "connect over Bluetooth".
                board.lock().await.set_setup_phase(SetupPhase::WaitingBluetooth);
                return;
            }
            GattConnectionEvent::Gatt { event } => {
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
                    }
                }
                if let Ok(reply) = event.accept() {
                    reply.send().await;
                }
                // Save + reboot once both SSID and password have been written
                // (the server characteristic is optional and must come first).
                if let (Some(s), Some(p)) = (&ssid_v, &pass_v) {
                    // "host:port" -> (host, port); malformed/absent = default.
                    let (host, port) = server_v
                        .as_deref()
                        .and_then(|sv| {
                            let (h, pt) = sv.rsplit_once(':')?;
                            Some((String::from(h), pt.parse::<u16>().ok()?))
                        })
                        .unwrap_or((String::new(), 0));
                    if !s.is_empty() && creds::save(&mut *flash.lock().await, s, p, &host, port) {
                        println!("[ble] saved creds for '{}' (server '{}') — rebooting", s, host);
                        // Show "WI-FI SAVED" for a beat before the reboot wipes
                        // the panel — otherwise the confirmation is never seen.
                        board.lock().await.set_setup_phase(SetupPhase::Saved);
                        Timer::after(Duration::from_millis(900)).await;
                        esp_hal::system::software_reset();
                    }
                }
            }
            _ => {}
        }
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
