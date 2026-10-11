//! This board's Ed25519 identity: a 32-byte secret seed persisted in flash. The
//! derived public key IS the `board_id` the server knows us by, so this is the most
//! important blob on the device — losing it unpairs every controller.
//!
//! Two ways to get one (MAR-90). An AUTHORIZED board has its identity written
//! at flash time (`bless-board`), together with the owner's blessing that lets
//! it use the hosted server. Any other board mints its own from the hardware
//! RNG on its first boot after provisioning. The record layout lives in
//! `scoreboard_protocol::IdentityRecord`, shared with the tool that mints it.
//!
//! Stored in its OWN nvs sector (0xB000), separate from creds (0x9000) and
//! brightness (0xA000): esp-storage erases a whole sector per write, so an isolated
//! sector means a brightness write — or a power loss mid-write — can never corrupt
//! the identity.

use embedded_storage::{ReadStorage, Storage};
use esp_hal::rng::Trng;
use esp_storage::FlashStorage;
use scoreboard_protocol::{IdentityRecord, IDENTITY_RECORD_LEN};

const OFFSET: u32 = 0xB000; // own nvs sector, after creds (0x9000) + brightness (0xA000)

/// An authorized board's offline factory reset, remembered until it can tell
/// the server (`take_forget_pending`). Its own sector: written only on reset
/// and cleared once, so it can never tear the identity record.
const FORGET_OFFSET: u32 = 0xD000; // after the OTA attempt counter (0xC000)
const FORGET_MAGIC: u32 = 0x5742_4631; // "WBF1"

fn load(flash: &mut FlashStorage) -> Option<IdentityRecord> {
    let mut buf = [0u8; IDENTITY_RECORD_LEN];
    flash.read(OFFSET, &mut buf).ok()?;
    IdentityRecord::decode(&buf)
}

/// Does this board carry an owner's blessing? Decides what a factory reset
/// may destroy (see `factory_reset`).
pub fn is_blessed(flash: &mut FlashStorage) -> bool {
    load(flash).is_some_and(|r| r.blessing.is_some())
}

/// Factory reset's share of the identity.
///
/// A board that minted its own identity destroys it, so the NEXT boot mints
/// a fresh seed: a reset board IS a brand-new scoreboard, unrecognizable to
/// the server and to every previously paired phone (works fully offline; the
/// online path additionally tells the server to forget the old id).
/// Full-record wipe, the creds::clear lesson: a partial overwrite left
/// recoverable bytes behind.
///
/// An AUTHORIZED board keeps its identity. It is the authorization, and
/// wiping it would quietly turn the board off for good on the hosted server.
/// It still has to come back unpaired from every phone, so it records the
/// reset and tells the server on its next connect (FactoryReset after
/// AuthOk), whether or not this reset happened online.
pub fn factory_reset(flash: &mut FlashStorage) {
    if is_blessed(flash) {
        let _ = flash.write(FORGET_OFFSET, &FORGET_MAGIC.to_le_bytes());
    } else {
        let wipe = [0xFFu8; IDENTITY_RECORD_LEN];
        let _ = flash.write(OFFSET, &wipe);
    }
}

/// True (once) when a reset is waiting to be reported to the server. The
/// caller sends FactoryReset and then calls `clear_forget_pending`.
pub fn forget_pending(flash: &mut FlashStorage) -> bool {
    let mut buf = [0u8; 4];
    flash.read(FORGET_OFFSET, &mut buf).is_ok() && u32::from_le_bytes(buf) == FORGET_MAGIC
}

pub fn clear_forget_pending(flash: &mut FlashStorage) {
    let _ = flash.write(FORGET_OFFSET, &[0xFFu8; 4]);
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
pub fn load_or_create(flash: &mut FlashStorage, rng: &Trng) -> IdentityRecord {
    if let Some(record) = load(flash) {
        return record;
    }
    let mut secret = [0u8; 32];
    rng.read(&mut secret);
    let record = IdentityRecord { secret, blessing: None };
    flash.write(OFFSET, &record.encode()).ok();
    record
}
