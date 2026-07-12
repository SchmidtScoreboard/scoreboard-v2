//! WiFi credentials + server address persisted in flash (BLE provisioning
//! writes them; hold-to-reset wipes them). Stored as a small blob in the `nvs`
//! partition region; we own the format (not esp-idf NVS compatible, fine).

use alloc::string::{String, ToString};
use embedded_storage::{ReadStorage, Storage};
use esp_storage::FlashStorage;

const OFFSET: u32 = 0x9000; // start of the `nvs` partition (unused by our no_std app)
const MAGIC_V1: u32 = 0x5742_4331; // "WBC1": ssid + pass only
const MAGIC_V2: u32 = 0x5742_4332; // "WBC2": + server host + port

pub struct Creds {
    pub ssid: String,
    pub pass: String,
    /// Server host (DNS name or IP literal); empty = use the compiled default.
    pub server_host: String,
    pub server_port: u16,
}

/// Layout v2: magic(4) | ssid_len(1) | ssid | pass_len(1) | pass
///            | host_len(1) | host | port(2 LE)
/// v1 blobs (pre-server-provisioning boards) load with an empty host.
pub fn load(flash: &mut FlashStorage) -> Option<Creds> {
    let mut buf = [0u8; 256];
    flash.read(OFFSET, &mut buf).ok()?;
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != MAGIC_V1 && magic != MAGIC_V2 {
        return None;
    }
    let slen = buf[4] as usize;
    let ssid = core::str::from_utf8(buf.get(5..5 + slen)?).ok()?.to_string();
    let p = 5 + slen;
    let plen = *buf.get(p)? as usize;
    let pass = core::str::from_utf8(buf.get(p + 1..p + 1 + plen)?).ok()?.to_string();
    let (server_host, server_port) = if magic == MAGIC_V2 {
        let h = p + 1 + plen;
        let hlen = *buf.get(h)? as usize;
        let host = core::str::from_utf8(buf.get(h + 1..h + 1 + hlen)?).ok()?.to_string();
        let q = h + 1 + hlen;
        let port = u16::from_le_bytes([*buf.get(q)?, *buf.get(q + 1)?]);
        (host, port)
    } else {
        (String::new(), 0)
    };
    Some(Creds { ssid, pass, server_host, server_port })
}

pub fn save(flash: &mut FlashStorage, ssid: &str, pass: &str, host: &str, port: u16) -> bool {
    if ssid.len() > 32 || pass.len() > 64 || host.len() > 96 {
        return false;
    }
    let mut buf = [0u8; 256];
    buf[0..4].copy_from_slice(&MAGIC_V2.to_le_bytes());
    buf[4] = ssid.len() as u8;
    buf[5..5 + ssid.len()].copy_from_slice(ssid.as_bytes());
    let mut p = 5 + ssid.len();
    buf[p] = pass.len() as u8;
    buf[p + 1..p + 1 + pass.len()].copy_from_slice(pass.as_bytes());
    p += 1 + pass.len();
    buf[p] = host.len() as u8;
    buf[p + 1..p + 1 + host.len()].copy_from_slice(host.as_bytes());
    p += 1 + host.len();
    buf[p..p + 2].copy_from_slice(&port.to_le_bytes());
    flash.write(OFFSET, &buf).is_ok()
}

/// Wipe the stored credentials (forces BLE provisioning on next boot).
#[allow(dead_code)]
pub fn clear(flash: &mut FlashStorage) {
    let buf = [0xFFu8; 8];
    flash.write(OFFSET, &buf).ok();
}
