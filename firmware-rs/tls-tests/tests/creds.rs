//! Tests for the firmware's flash credential record (`firmware-rs/src/creds.rs`).
//!
//! Three things are being defended here:
//!
//! * **Compatibility.** A board in the field was provisioned by whatever
//!   firmware it shipped with. v1 and v2 records must keep loading exactly as
//!   they did, or a firmware update becomes a factory reset — the board comes
//!   back up on the setup screen with no Wi-Fi, and the owner has no idea why.
//!   The v1/v2 fixtures here are built BY HAND, not by today's `encode`, so
//!   this stays a test of the old bytes rather than a round trip with itself.
//! * **The pin is a provisioned fact.** A v3 record carries the server's TLS
//!   pin, and a pin that survives a save/load cycle wrong is a board that can
//!   never open a connection again — there is no server to fix it from.
//! * **No accidental pins.** An all-zero pin is "absent", never a value the
//!   verifier trusts, on both the read and the write side; and a malformed hex
//!   string is refused rather than stored as something.

use embedded_storage::{ReadStorage, Storage};
use tls_tests::creds::{self, MAGIC_V1, MAGIC_V2, MAGIC_V3, PIN_LEN, RECORD_LEN};

/// Where `creds.rs` keeps the record (`OFFSET`, private to the module).
const OFFSET: u32 = 0x9000;

/// A pin worth recognising when it comes back out.
const PIN_HEX: &str = "839f6daad00cae33c657239e4467d15ac920db3e095eb04bad31cc165c8a7e07";

// ---------------------------------------------------------------------------
// A fake flash
// ---------------------------------------------------------------------------

/// Erased flash, addressable at the same offsets the board uses.
///
/// `load`/`save`/`clear` are generic over `embedded_storage::Storage`, which is
/// exactly what `esp_storage::FlashStorage` implements — so these tests drive
/// the real functions, not a host-only reimplementation of them.
struct FakeFlash {
    data: Vec<u8>,
}

impl FakeFlash {
    fn new() -> Self {
        Self { data: vec![0xFF; OFFSET as usize + 4096] }
    }

    /// The raw record bytes, for assertions about the layout itself.
    fn record(&self) -> &[u8] {
        &self.data[OFFSET as usize..OFFSET as usize + RECORD_LEN]
    }

    /// Drop a hand-built record in at the offset.
    fn put(&mut self, bytes: &[u8]) {
        self.data[OFFSET as usize..OFFSET as usize + bytes.len()].copy_from_slice(bytes);
    }
}

impl ReadStorage for FakeFlash {
    type Error = ();

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), ()> {
        let at = offset as usize;
        bytes.copy_from_slice(self.data.get(at..at + bytes.len()).ok_or(())?);
        Ok(())
    }

    fn capacity(&self) -> usize {
        self.data.len()
    }
}

impl Storage for FakeFlash {
    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), ()> {
        let at = offset as usize;
        self.data.get_mut(at..at + bytes.len()).ok_or(())?.copy_from_slice(bytes);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Hand-built legacy records — the bytes older firmware actually wrote
// ---------------------------------------------------------------------------

fn v1_record(ssid: &str, pass: &str) -> Vec<u8> {
    let mut b = MAGIC_V1.to_le_bytes().to_vec();
    b.push(ssid.len() as u8);
    b.extend_from_slice(ssid.as_bytes());
    b.push(pass.len() as u8);
    b.extend_from_slice(pass.as_bytes());
    b.resize(RECORD_LEN, 0);
    b
}

fn v2_record(ssid: &str, pass: &str, host: &str, port: u16) -> Vec<u8> {
    let mut b = MAGIC_V2.to_le_bytes().to_vec();
    b.push(ssid.len() as u8);
    b.extend_from_slice(ssid.as_bytes());
    b.push(pass.len() as u8);
    b.extend_from_slice(pass.as_bytes());
    b.push(host.len() as u8);
    b.extend_from_slice(host.as_bytes());
    b.extend_from_slice(&port.to_le_bytes());
    b.resize(RECORD_LEN, 0);
    b
}

fn v3_record(ssid: &str, pass: &str, host: &str, port: u16, pin: &[u8; PIN_LEN]) -> Vec<u8> {
    let mut b = v2_record(ssid, pass, host, port);
    b.truncate(4 + 1 + ssid.len() + 1 + pass.len() + 1 + host.len() + 2);
    b[0..4].copy_from_slice(&MAGIC_V3.to_le_bytes());
    b.extend_from_slice(pin);
    b.resize(RECORD_LEN, 0);
    b
}

// ---------------------------------------------------------------------------
// Loading every layout a board might be carrying
// ---------------------------------------------------------------------------

#[test]
fn a_v1_record_loads_with_no_server_and_no_pin() {
    let mut flash = FakeFlash::new();
    flash.put(&v1_record("home-wifi", "hunter2"));

    let c = creds::load(&mut flash).expect("v1 records must keep loading");
    assert_eq!(c.ssid, "home-wifi");
    assert_eq!(c.pass, "hunter2");
    // Empty host = "use the compiled default", which is what v1 meant.
    assert_eq!(c.server_host, "");
    assert_eq!(c.server_port, 0);
    assert_eq!(c.server_pin, None, "a v1 record cannot carry a pin");
}

#[test]
fn a_v2_record_loads_its_server_and_no_pin() {
    let mut flash = FakeFlash::new();
    flash.put(&v2_record("home-wifi", "hunter2", "192.168.1.50", 7878));

    let c = creds::load(&mut flash).expect("v2 records must keep loading");
    assert_eq!(c.ssid, "home-wifi");
    assert_eq!(c.pass, "hunter2");
    assert_eq!(c.server_host, "192.168.1.50");
    assert_eq!(c.server_port, 7878);
    assert_eq!(c.server_pin, None, "a v2 record cannot carry a pin");
}

#[test]
fn a_v3_record_loads_its_pin() {
    let pin = creds::pin_from_hex_str(PIN_HEX).unwrap();
    let mut flash = FakeFlash::new();
    flash.put(&v3_record("home-wifi", "hunter2", "board.example.com", 4443, &pin));

    let c = creds::load(&mut flash).expect("v3 record");
    assert_eq!(c.ssid, "home-wifi");
    assert_eq!(c.pass, "hunter2");
    assert_eq!(c.server_host, "board.example.com");
    assert_eq!(c.server_port, 4443);
    assert_eq!(c.server_pin, Some(pin));
}

#[test]
fn an_unknown_magic_is_not_credentials() {
    let mut flash = FakeFlash::new();
    let mut junk = v2_record("home-wifi", "hunter2", "h", 1);
    junk[0..4].copy_from_slice(&0x5742_4339u32.to_le_bytes()); // "WBC9", from the future
    flash.put(&junk);
    assert!(creds::load(&mut flash).is_none());

    // And erased flash is not credentials either — the unprovisioned board.
    let mut blank = FakeFlash::new();
    assert!(creds::load(&mut blank).is_none());
}

// ---------------------------------------------------------------------------
// Saving
// ---------------------------------------------------------------------------

#[test]
fn a_provisioned_pin_round_trips_through_flash() {
    let pin = creds::pin_from_hex_str(PIN_HEX).unwrap();
    let mut flash = FakeFlash::new();

    assert!(creds::save(&mut flash, "home-wifi", "hunter2", "board.example.com", 4443, Some(&pin)));
    assert_eq!(
        flash.record()[0..4],
        MAGIC_V3.to_le_bytes(),
        "a record carrying a pin is v3"
    );

    let c = creds::load(&mut flash).expect("just saved");
    assert_eq!(c.ssid, "home-wifi");
    assert_eq!(c.pass, "hunter2");
    assert_eq!(c.server_host, "board.example.com");
    assert_eq!(c.server_port, 4443);
    assert_eq!(c.server_pin, Some(pin));
}

#[test]
fn saving_without_a_pin_still_writes_a_v2_record() {
    // The hosted-server case, which is most boards: the bytes are exactly what
    // they were before v3 existed, so an OTA revert to an older image still
    // finds credentials it understands instead of dropping into BLE setup.
    let mut flash = FakeFlash::new();
    assert!(creds::save(&mut flash, "home-wifi", "hunter2", "", 0, None));
    assert_eq!(flash.record()[0..4], MAGIC_V2.to_le_bytes());
    assert_eq!(flash.record(), &v2_record("home-wifi", "hunter2", "", 0)[..]);

    let c = creds::load(&mut flash).expect("just saved");
    assert_eq!(c.server_pin, None);
}

#[test]
fn an_over_long_field_is_refused_before_flash_is_touched() {
    let mut flash = FakeFlash::new();
    assert!(!creds::save(&mut flash, &"s".repeat(33), "p", "", 0, None));
    assert!(!creds::save(&mut flash, "s", &"p".repeat(65), "", 0, None));
    assert!(!creds::save(&mut flash, "s", "p", &"h".repeat(97), 0, None));
    // Nothing was written: the record is still erased.
    assert!(flash.record().iter().all(|&b| b == 0xFF));
    assert!(creds::load(&mut flash).is_none());
}

// ---------------------------------------------------------------------------
// An all-zero pin is absent, on both sides
// ---------------------------------------------------------------------------

#[test]
fn an_all_zero_stored_pin_reads_as_absent() {
    // A v3 record whose pin field is zeroed — a half-written record, or a
    // caller that thought zeroes meant "none". It must not become a pin.
    let mut flash = FakeFlash::new();
    flash.put(&v3_record("home-wifi", "hunter2", "board.example.com", 4443, &[0u8; PIN_LEN]));

    let c = creds::load(&mut flash).expect("the rest of the record is still valid");
    assert_eq!(c.server_host, "board.example.com", "the record still loads");
    assert_eq!(c.server_pin, None, "an all-zero pin is not a pin");
}

#[test]
fn an_all_zero_pin_is_never_written_as_one() {
    let mut flash = FakeFlash::new();
    assert!(creds::save(&mut flash, "home-wifi", "hunter2", "h", 1, Some(&[0u8; PIN_LEN])));
    assert_eq!(
        flash.record()[0..4],
        MAGIC_V2.to_le_bytes(),
        "zeroes are not a pin, so there is nothing for v3 to carry"
    );
    assert_eq!(creds::load(&mut flash).unwrap().server_pin, None);
}

// ---------------------------------------------------------------------------
// Hex parsing — what the app pastes into the BLE characteristic
// ---------------------------------------------------------------------------

#[test]
fn a_well_formed_pin_parses_in_either_case() {
    let lower = creds::pin_from_hex_str(PIN_HEX).expect("64 lowercase hex digits");
    let upper = creds::pin_from_hex_str(&PIN_HEX.to_uppercase()).expect("case is not meaningful");
    assert_eq!(lower, upper);
    // Same bytes the tls module's compile-time decoder produces.
    assert_eq!(lower, tls_tests::tls::pin_from_hex(PIN_HEX));
}

#[test]
fn malformed_hex_is_rejected_rather_than_stored() {
    let bad_pins: [String; 7] = [
        String::new(),                    // nothing pasted
        PIN_HEX[..63].to_string(),        // one digit short
        format!("{PIN_HEX}0"),            // one digit long
        format!("{}zz", &PIN_HEX[..62]),  // not hex
        format!("{}  ", &PIN_HEX[..62]),  // right length, trailing spaces
        format!("0x{}", &PIN_HEX[..62]),  // an 0x prefix is not part of a pin
        "0".repeat(64),                   // the all-zero digest is never a pin
    ];
    for bad in &bad_pins {
        assert_eq!(
            creds::pin_from_hex_str(bad),
            None,
            "{bad:?} is not a pin and must not be stored as one"
        );
    }

    // And a rejected parse is not a quiet fallback to "no pin": the BLE layer
    // refuses the whole save (see ble.rs), so what reaches `save` is only ever
    // a pin that parsed. Prove the shape it would have written is v2 — i.e.
    // that "no pin" and "a broken pin" are genuinely different values here.
    let mut flash = FakeFlash::new();
    assert!(creds::save(&mut flash, "s", "p", "h", 1, None));
    assert_eq!(flash.record()[0..4], MAGIC_V2.to_le_bytes());
}

// ---------------------------------------------------------------------------
// The record still fits, and `clear` still covers all of it
// ---------------------------------------------------------------------------

#[test]
fn the_record_fits_at_maximum_field_lengths() {
    // The bounds `save` enforces: 32-byte SSID, 64-byte password, 96-byte host.
    let ssid = "s".repeat(32);
    let pass = "p".repeat(64);
    let host = "h".repeat(96);
    let pin = creds::pin_from_hex_str(PIN_HEX).unwrap();

    let mut flash = FakeFlash::new();
    assert!(creds::save(&mut flash, &ssid, &pass, &host, 65535, Some(&pin)));

    // magic(4) + 1+32 + 1+64 + 1+96 + 2 + 32 = 233 <= RECORD_LEN (256).
    let payload = 4 + 1 + 32 + 1 + 64 + 1 + 96 + 2 + PIN_LEN;
    assert_eq!(payload, 233);
    assert!(payload <= RECORD_LEN);
    // The tail past the payload is untouched by the write, so the pin really
    // did land inside the record rather than past its end.
    assert!(flash.record()[payload..].iter().all(|&b| b == 0));

    let c = creds::load(&mut flash).expect("the biggest record still loads");
    assert_eq!(c.ssid, ssid);
    assert_eq!(c.pass, pass);
    assert_eq!(c.server_host, host);
    assert_eq!(c.server_port, 65535);
    assert_eq!(c.server_pin, Some(pin));
}

#[test]
fn clear_wipes_every_byte_the_biggest_record_can_write() {
    // Hold-to-reset is the ONLY way out of a wrong pin — a board pinned to a
    // key its server does not hold can never be reached — so what it wipes is
    // load-bearing, pin included.
    let pin = creds::pin_from_hex_str(PIN_HEX).unwrap();
    let mut flash = FakeFlash::new();
    assert!(creds::save(
        &mut flash,
        &"s".repeat(32),
        &"p".repeat(64),
        &"h".repeat(96),
        65535,
        Some(&pin)
    ));
    assert!(creds::load(&mut flash).is_some());

    creds::clear(&mut flash);
    assert!(creds::load(&mut flash).is_none(), "a cleared board goes back to setup");
    assert!(
        flash.record().iter().all(|&b| b == 0xFF),
        "the whole record — password and pin included — is gone, not just the magic"
    );
}
