//! WiFi credentials persisted in flash (so BLE provisioning survives reboots and
//! replaces the hardcoded config.rs). Stored as a small blob in the `nvs` partition
//! region; we own the format (not esp-idf NVS compatible, which is fine).

use alloc::string::{String, ToString};
use embedded_storage::{ReadStorage, Storage};
use esp_storage::FlashStorage;

const OFFSET: u32 = 0x9000; // start of the `nvs` partition (unused by our no_std app)
const MAGIC: u32 = 0x5742_4331; // "WBC1"

/// Layout: magic(4) | ssid_len(1) | ssid | pass_len(1) | pass
pub fn load(flash: &mut FlashStorage) -> Option<(String, String)> {
    let mut buf = [0u8; 128];
    flash.read(OFFSET, &mut buf).ok()?;
    if u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) != MAGIC {
        return None;
    }
    let slen = buf[4] as usize;
    let ssid = core::str::from_utf8(buf.get(5..5 + slen)?).ok()?.to_string();
    let p = 5 + slen;
    let plen = *buf.get(p)? as usize;
    let pass = core::str::from_utf8(buf.get(p + 1..p + 1 + plen)?).ok()?.to_string();
    Some((ssid, pass))
}

pub fn save(flash: &mut FlashStorage, ssid: &str, pass: &str) -> bool {
    if ssid.len() > 32 || pass.len() > 64 {
        return false;
    }
    let mut buf = [0u8; 128];
    buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    buf[4] = ssid.len() as u8;
    buf[5..5 + ssid.len()].copy_from_slice(ssid.as_bytes());
    let p = 5 + ssid.len();
    buf[p] = pass.len() as u8;
    buf[p + 1..p + 1 + pass.len()].copy_from_slice(pass.as_bytes());
    flash.write(OFFSET, &buf).is_ok()
}

/// Wipe the stored credentials (forces BLE provisioning on next boot).
#[allow(dead_code)]
pub fn clear(flash: &mut FlashStorage) {
    let buf = [0xFFu8; 8];
    flash.write(OFFSET, &buf).ok();
}
