//! Encoder brightness persisted in flash so the board powers back on at the
//! level you left it. Stored in its OWN nvs sector (0xA000), separate from the
//! WiFi-credentials sector (0x9000) — esp-storage erases a whole sector per
//! write, so keeping them apart means a brightness write can never corrupt the
//! credentials, even on power loss mid-write.

use embedded_storage::{ReadStorage, Storage};
use esp_storage::FlashStorage;

const OFFSET: u32 = 0xA000; // nvs sector after creds (0x9000); within the nvs partition
const MAGIC: u8 = 0xB7;

/// A persisted brightness (10..=100), or None if never saved / invalid.
pub fn load(flash: &mut FlashStorage) -> Option<u8> {
    let mut buf = [0u8; 4];
    flash.read(OFFSET, &mut buf).ok()?;
    if buf[0] != MAGIC {
        return None;
    }
    let level = buf[1];
    (10..=100).contains(&level).then_some(level)
}

/// Persist the brightness. Erases a flash sector, so callers MUST debounce —
/// only write once the knob has settled, never on every detent.
pub fn save(flash: &mut FlashStorage, level: u8) -> bool {
    let buf = [MAGIC, level, 0, 0];
    flash.write(OFFSET, &buf).is_ok()
}
