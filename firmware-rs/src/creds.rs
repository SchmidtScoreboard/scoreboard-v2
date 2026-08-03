//! WiFi credentials + server address + server TLS pin persisted in flash (BLE
//! provisioning writes them; hold-to-reset wipes them). Stored as a small blob
//! in the `nvs` partition region; we own the format (not esp-idf NVS
//! compatible, fine).
//!
//! The record codec ([`encode`] / [`decode`] / [`pin_from_hex_str`]) is pure —
//! no esp-hal, no flash, no I/O — and the flash entry points are generic over
//! [`Storage`] rather than tied to `esp_storage::FlashStorage`. That is what
//! lets `tls-tests/` compile THIS source for the host and drive it against a
//! fake flash, the same trick `src/tls.rs` uses. A record format that decides
//! whether a board can reach its server should not be testable only by
//! flashing one.

use alloc::string::{String, ToString};
use embedded_storage::Storage;

const OFFSET: u32 = 0x9000; // start of the `nvs` partition (unused by our no_std app)
pub const MAGIC_V1: u32 = 0x5742_4331; // "WBC1": ssid + pass only
pub const MAGIC_V2: u32 = 0x5742_4332; // "WBC2": + server host + port
pub const MAGIC_V3: u32 = 0x5742_4333; // "WBC3": + server TLS pin

/// Every `save` writes exactly this many bytes, whatever the field lengths, and
/// `load` reads the same span back. `clear` wipes all of it — see there.
pub const RECORD_LEN: usize = 256;

/// A pin is a SHA-256 digest (`tls::PIN_LEN`, restated here so the record
/// format does not depend on the `tls` feature being compiled in).
pub const PIN_LEN: usize = 32;

/// Field bounds, enforced by `save` before anything touches flash. They exist as
/// constants so the record-size invariant below can be stated against them.
const MAX_SSID: usize = 32;
const MAX_PASS: usize = 64;
const MAX_HOST: usize = 96;

/// The largest v2 record `save` can lay down: magic(4) | ssid_len(1) + ssid
/// | pass_len(1) + pass | host_len(1) + host | port(2) = 201 bytes.
const MAX_PAYLOAD_V2: usize = 4 + 1 + MAX_SSID + 1 + MAX_PASS + 1 + MAX_HOST + 2;

/// The largest v3 record: everything v2 lays down plus the raw 32-byte pin
/// appended after the port — 201 + 32 = 233 bytes, against a 256-byte record.
/// 23 bytes of headroom left; the assert below is what keeps a future field
/// from quietly spending more than that.
const MAX_PAYLOAD: usize = MAX_PAYLOAD_V2 + PIN_LEN;
const _: () = assert!(
    MAX_PAYLOAD <= RECORD_LEN,
    "the v3 record must fit the span save() writes and load() reads"
);

pub struct Creds {
    pub ssid: String,
    pub pass: String,
    /// Server host (DNS name or IP literal); empty = use the compiled default.
    pub server_host: String,
    pub server_port: u16,
    /// The server's TLS pin, provisioned alongside the host for self-hosters.
    /// `None` (a v1/v2 record, or a v3 record carrying an all-zero pin) = use
    /// the firmware's compiled-in pin — see `link.rs` for the selection rule.
    pub server_pin: Option<[u8; PIN_LEN]>,
}

/// Read the stored record, if there is one.
pub fn load(flash: &mut impl Storage) -> Option<Creds> {
    let mut buf = [0u8; RECORD_LEN];
    flash.read(OFFSET, &mut buf).ok()?;
    decode(&buf)
}

/// Parse a record.
///
/// ```text
/// v1: magic(4) | ssid_len(1) | ssid | pass_len(1) | pass
/// v2: …                                                  | host_len(1) | host | port(2 LE)
/// v3: …                                                                                    | pin(32)
/// ```
///
/// Every older layout keeps loading EXACTLY as it did: a board updated in the
/// field still has whatever its provisioning wrote, and a firmware update that
/// dropped a v1 or v2 record on the floor would send a working board back to
/// the setup screen. They simply carry no pin.
pub fn decode(buf: &[u8]) -> Option<Creds> {
    let magic = u32::from_le_bytes([*buf.first()?, *buf.get(1)?, *buf.get(2)?, *buf.get(3)?]);
    if magic != MAGIC_V1 && magic != MAGIC_V2 && magic != MAGIC_V3 {
        return None;
    }
    let slen = *buf.get(4)? as usize;
    let ssid = core::str::from_utf8(buf.get(5..5 + slen)?).ok()?.to_string();
    let p = 5 + slen;
    let plen = *buf.get(p)? as usize;
    let pass = core::str::from_utf8(buf.get(p + 1..p + 1 + plen)?).ok()?.to_string();
    let mut at = p + 1 + plen;
    let (server_host, server_port) = if magic == MAGIC_V1 {
        (String::new(), 0)
    } else {
        let hlen = *buf.get(at)? as usize;
        let host = core::str::from_utf8(buf.get(at + 1..at + 1 + hlen)?).ok()?.to_string();
        at += 1 + hlen;
        let port = u16::from_le_bytes([*buf.get(at)?, *buf.get(at + 1)?]);
        at += 2;
        (host, port)
    };
    let server_pin = if magic == MAGIC_V3 {
        let mut pin = [0u8; PIN_LEN];
        pin.copy_from_slice(buf.get(at..at + PIN_LEN)?);
        // An all-zero pin is "absent", never a pin. Same rule the compiled-in
        // pins follow (`tls::pin_from_hex` refuses the all-zero digest at
        // compile time): a zeroed field is what an erase or a half-written
        // record looks like, and it must not become a value the verifier trusts
        // — nor let a caller provision a pin of zeroes by accident.
        pin.iter().any(|&b| b != 0).then_some(pin)
    } else {
        None
    };
    Some(Creds { ssid, pass, server_host, server_port, server_pin })
}

/// Lay out a record. `None` = a field is over its bound, so nothing is written.
///
/// Writes v3 only when a pin is actually provisioned, v2 otherwise — the
/// hosted-server case (no pin) keeps producing exactly the bytes it produced
/// before this format existed. That matters for downgrades: an OTA revert to an
/// older image must still find credentials it can read, and only a self-hoster
/// — whose board an older image could not have talked to anyway, since the pin
/// is the whole point — ever gets a record the old code rejects.
pub fn encode(
    ssid: &str,
    pass: &str,
    host: &str,
    port: u16,
    pin: Option<&[u8; PIN_LEN]>,
) -> Option<[u8; RECORD_LEN]> {
    if ssid.len() > MAX_SSID || pass.len() > MAX_PASS || host.len() > MAX_HOST {
        return None;
    }
    // All-zero in, absent out — the reader's rule, applied before the write so
    // the two can never disagree about what is stored.
    let pin = pin.filter(|p| p.iter().any(|&b| b != 0));
    let mut buf = [0u8; RECORD_LEN];
    let magic = if pin.is_some() { MAGIC_V3 } else { MAGIC_V2 };
    buf[0..4].copy_from_slice(&magic.to_le_bytes());
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
    p += 2;
    if let Some(pin) = pin {
        buf[p..p + PIN_LEN].copy_from_slice(pin);
    }
    Some(buf)
}

pub fn save(
    flash: &mut impl Storage,
    ssid: &str,
    pass: &str,
    host: &str,
    port: u16,
    pin: Option<&[u8; PIN_LEN]>,
) -> bool {
    match encode(ssid, pass, host, port, pin) {
        Some(buf) => flash.write(OFFSET, &buf).is_ok(),
        None => false,
    }
}

/// Decode the 64-character hex form of a pin — what `gen-tls-cert` prints, and
/// what a human pastes into the app — into the 32 raw bytes the record stores.
///
/// The runtime twin of `tls::pin_from_hex`, which is `const fn` and *asserts*:
/// that one turns a typo in `config.rs` into a build error, this one turns a
/// typo in a text field into a rejected BLE write. Same three rules — exactly
/// 64 hex digits, nothing else, and never the all-zero digest.
pub fn pin_from_hex_str(hex: &str) -> Option<[u8; PIN_LEN]> {
    let bytes = hex.as_bytes();
    if bytes.len() != PIN_LEN * 2 {
        return None;
    }
    let mut out = [0u8; PIN_LEN];
    for (i, b) in out.iter_mut().enumerate() {
        *b = (hex_nibble(bytes[i * 2])? << 4) | hex_nibble(bytes[i * 2 + 1])?;
    }
    out.iter().any(|&b| b != 0).then_some(out)
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Wipe the stored credentials (forces BLE provisioning on next boot).
///
/// Wipes the WHOLE record, not just the magic. Clearing 8 bytes was enough to
/// make `load` reject the blob, but it left the SSID tail, `pass_len`, the full
/// PLAINTEXT Wi-Fi password, the server host and the port sitting in flash — a
/// "factory reset" before handing the board on, or before an RMA, has to mean
/// the password is gone. It is also free: esp-storage read-modify-erase-writes
/// the entire 4KB sector for any write inside it, so 256 bytes of 0xFF costs
/// exactly the same single erase the 8 bytes did.
///
/// This is also the ONLY way out of a wrong TLS pin: a board pinned to a key
/// its server does not hold can never reach a server, so no remote fix exists.
/// Hold-to-reset takes the pin with the rest of the record and returns the
/// board to BLE provisioning, where a correct one can be written.
#[allow(dead_code)]
pub fn clear(flash: &mut impl Storage) {
    const WIPE_LEN: usize = RECORD_LEN;
    // The wipe and the record can only drift apart if a new field pushes the
    // payload past the buffer both share, so that is what gets asserted: catch
    // it here rather than discover a surviving password after a factory reset.
// A compile-time check, not `debug_assert!`: that macro is compiled OUT of
    // the release image that actually ships, so it could only ever have caught
    // this on a host test run.
    const _: () = assert!(WIPE_LEN >= MAX_PAYLOAD, "the wipe must cover the whole record");
    let buf = [0xFFu8; WIPE_LEN];
    flash.write(OFFSET, &buf).ok();
}
