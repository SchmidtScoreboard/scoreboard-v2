//! This board's Ed25519 identity: a 32-byte secret seed persisted in flash. The
//! derived public key IS the `board_id` the server knows us by, so this is the most
//! important blob on the device — losing it unpairs every controller. Minted once
//! from the hardware RNG on first boot, then read back on every subsequent boot.
//!
//! Stored in its OWN nvs sector (0xB000), separate from creds (0x9000) and
//! brightness (0xA000): esp-storage erases a whole sector per write, so an isolated
//! sector means a brightness write — or a power loss mid-write — can never corrupt
//! the identity.

use embedded_storage::{ReadStorage, Storage};
use esp_hal::rng::Trng;
use esp_storage::FlashStorage;

const OFFSET: u32 = 0xB000; // own nvs sector, after creds (0x9000) + brightness (0xA000)
const MAGIC: u32 = 0x5742_4B31; // "WBK1"

/// Layout: magic(4) | secret(32). Returns the secret iff the magic matches.
fn load(flash: &mut FlashStorage) -> Option<[u8; 32]> {
    let mut buf = [0u8; 36];
    flash.read(OFFSET, &mut buf).ok()?;
    if u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) != MAGIC {
        return None;
    }
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&buf[4..36]);
    Some(secret)
}

/// Factory reset: destroy the identity so the NEXT boot mints a fresh seed —
/// a reset board IS a brand-new scoreboard, unrecognizable to the server and
/// to every previously paired phone (works fully offline; the online path
/// additionally tells the server to forget the old id). Full-record wipe, the
/// creds::clear lesson: a partial overwrite left recoverable bytes behind.
pub fn clear(flash: &mut FlashStorage) {
    const RECORD: usize = 36; // magic(4) + secret(32)
    let wipe = [0xFFu8; RECORD];
    debug_assert!(wipe.len() >= 36, "wipe must cover the whole record");
    let _ = flash.write(OFFSET, &wipe);
}

fn save(flash: &mut FlashStorage, secret: &[u8; 32]) {
    let mut buf = [0u8; 36];
    buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    buf[4..36].copy_from_slice(secret);
    flash.write(OFFSET, &buf).ok();
}

/// Load the persisted identity, or mint + persist a fresh one on first boot.
///
/// Takes a [`Trng`] rather than esp-hal's plain `Rng` on purpose. `Rng` is only
/// truly random while the RF subsystem or the SAR ADC is mixing physical noise
/// into the hardware generator; with neither up — which is exactly the state
/// this early in boot — it is documented as PSEUDO-random. That would make the
/// seed guessable, and this seed is minted once and read forever: a guessable
/// one is a permanently forgeable board identity. `Trng::try_new` only succeeds
/// while a `TrngSource` is alive, so the parameter type IS the proof that
/// physical entropy is switched on.
pub fn load_or_create(flash: &mut FlashStorage, rng: &Trng) -> [u8; 32] {
    if let Some(secret) = load(flash) {
        return secret;
    }
    let mut secret = [0u8; 32];
    rng.read(&mut secret);
    save(flash, &secret);
    secret
}
