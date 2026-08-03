//! TLS for the board link: one long-lived self-signed certificate, pinned.
//!
//! The board talks to exactly ONE server, so it pins that server's public key
//! instead of validating a PKI chain. That buys two things an ESP32-S3 badly
//! wants: no CA bundle in flash, and no wall clock. The board has no RTC and no
//! NTP — it cannot check `notAfter`, so PKI validation would be theatre. A pin
//! is checkable with a SHA-256 and nothing else.
//!
//! # What exactly is pinned
//!
//! **`pin = lowercase hex SHA-256 of the certificate's DER-encoded
//! SubjectPublicKeyInfo`** — the whole `SubjectPublicKeyInfo` SEQUENCE
//! (`AlgorithmIdentifier` + `BIT STRING subjectPublicKey`), exactly the bytes
//! that appear inside the certificate, *not* the raw 32-byte Ed25519 point and
//! *not* the whole certificate. For Ed25519 that SPKI is always 44 bytes: the
//! fixed prefix `30 2a 30 05 06 03 2b 65 70 03 21 00` followed by the 32-byte
//! public key.
//!
//! This is the same value as HPKP/`--pinnedpubkey`'s "SPKI hash", so it can be
//! cross-checked with stock tooling:
//!
//! ```text
//! openssl x509 -in cert.pem -pubkey -noout \
//!   | openssl pkey -pubin -outform der | openssl dgst -sha256
//! ```
//!
//! Hashing the SPKI rather than the whole certificate means re-issuing the
//! certificate (new serial, new SANs, new dates) with the *same key* does not
//! change the pin, so a re-issue is not a fleet-wide firmware event.
//!
//! # Why both halves of the verifier matter
//!
//! `verify_certificate` (pin match) establishes IDENTITY: this is the key we
//! expect. `verify_signature` (the TLS 1.3 CertificateVerify check) establishes
//! POSSESSION: the peer actually holds the matching private key. A certificate
//! is public — anyone on-path can copy the server's and replay it — so a pin
//! check alone is defeated by replay. Both are required. The test verifier in
//! `serve.rs` implements exactly this pair, and is the model for the firmware's
//! `TlsVerifier`.

// Shared by two binaries — the server (which serves) and gen-tls-cert (which
// mints) — so each of them sees the other's half as unused. The dead code here
// is structural, not real.
#![allow(dead_code)]

use std::sync::Arc;

use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// The `max_fragment_size` handed to rustls.
///
/// rustls counts the 5-byte record header inside this number
/// (`msgs::fragmenter`: `Some(sz) => sz - PACKET_OVERHEAD`, `PACKET_OVERHEAD =
/// 1 + 2 + 2`), so the plaintext it will put in one record is
/// [`MAX_RECORD_PLAINTEXT`] and the bytes it will put on the wire for one
/// record is [`MAX_RECORD_WIRE`]. The board sizes its receive buffer against
/// these; an unbounded 16 KB record would not fit in its heap.
pub const MAX_FRAGMENT_SIZE: usize = 4096;

/// Bytes of the TLS record header (type + legacy version + length).
const RECORD_HEADER: usize = 5;

/// TLS 1.3 AEAD expansion: the inner content-type byte plus the 16-byte tag.
/// (No padding — rustls does not pad.)
const AEAD_EXPANSION: usize = 1 + 16;

/// Most plaintext bytes the server will put in a single TLS record: 4091.
pub const MAX_RECORD_PLAINTEXT: usize = MAX_FRAGMENT_SIZE - RECORD_HEADER;

/// Most ciphertext bytes a single record's body can hold: 4108. This is the
/// number the board's `read_record_buf` must accommodate — embedded-tls reads
/// the 5-byte header into its own stack buffer and only the body into the
/// caller's buffer.
pub const MAX_RECORD_BODY: usize = MAX_RECORD_PLAINTEXT + AEAD_EXPANSION;

/// Most bytes a single TLS record occupies on the wire, header included: 4113.
pub const MAX_RECORD_WIRE: usize = RECORD_HEADER + MAX_RECORD_BODY;

/// Railway secrets holding the PEM certificate and its PEM private key. Not
/// files in the repo (the key is a secret) and not minted at boot (a per-deploy
/// certificate would break every board's pin on every deploy).
pub const CERT_ENV: &str = "TLS_CERT_PEM";
pub const KEY_ENV: &str = "TLS_KEY_PEM";

/// A loaded, ready-to-serve TLS identity.
pub struct ServerTls {
    pub config: Arc<ServerConfig>,
    /// The pin the firmware must carry for this certificate. Logged at boot so
    /// the owner can eyeball it against what shipped.
    pub pin: String,
}

/// Load the certificate from the environment.
///
/// `None` means "serve plaintext only", and is deliberately not fatal: a
/// deploy that has lost its secret must still serve boards and the app rather
/// than crash-loop. Both absent is the normal pre-TLS state (info); anything
/// else is a misconfiguration (error) that still degrades instead of dying.
pub fn from_env() -> Option<ServerTls> {
    let cert = std::env::var(CERT_ENV).ok().filter(|v| !v.trim().is_empty());
    let key = std::env::var(KEY_ENV).ok().filter(|v| !v.trim().is_empty());
    match (cert, key) {
        (Some(cert), Some(key)) => match from_pem(&cert, &key) {
            Ok(tls) => Some(tls),
            Err(e) => {
                tracing::error!(
                    "{CERT_ENV}/{KEY_ENV} are set but unusable: {e} — serving PLAINTEXT ONLY; \
                     boards that require TLS will not connect"
                );
                None
            }
        },
        (None, None) => {
            tracing::info!(
                "{CERT_ENV}/{KEY_ENV} unset — serving plaintext only \
                 (mint one with `cargo run -p server --bin gen-tls-cert`)"
            );
            None
        }
        (c, _) => {
            let present = if c.is_some() { CERT_ENV } else { KEY_ENV };
            let missing = if c.is_some() { KEY_ENV } else { CERT_ENV };
            tracing::error!(
                "{present} is set but {missing} is not — serving PLAINTEXT ONLY"
            );
            None
        }
    }
}

/// Build a TLS 1.3 server config from a PEM certificate + PEM private key,
/// and compute the pin the boards need for it.
pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<ServerTls, String> {
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("certificate PEM did not parse: {e}"))?;
    let leaf = certs
        .first()
        .ok_or_else(|| "certificate PEM contains no CERTIFICATE block".to_string())?;
    // The pin covers the LEAF only — that is the certificate whose key signs
    // the handshake, and the only one a self-signed deployment has.
    let pin = spki_sha256_hex(leaf)?;

    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .map_err(|e| format!("private key PEM did not parse: {e}"))?
        .ok_or_else(|| "private key PEM contains no PRIVATE KEY block".to_string())?;

    // Pin the provider explicitly rather than leaning on rustls's process-wide
    // default: if a second provider ever enters the dependency tree, that
    // default becomes a runtime panic instead of a compile error.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| format!("TLS 1.3 unavailable in this build: {e}"))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("certificate and key do not form a usable identity: {e}"))?;
    // See MAX_FRAGMENT_SIZE: the board's receive buffer is sized to this.
    config.max_fragment_size = Some(MAX_FRAGMENT_SIZE);

    Ok(ServerTls { config: Arc::new(config), pin })
}

/// The pin: lowercase hex SHA-256 over the certificate's DER SubjectPublicKeyInfo.
///
/// `x509_parser` hands back the SPKI's original DER slice (`raw`), so this
/// hashes bytes lifted verbatim out of the certificate — the same bytes the
/// board's verifier will find at the same place in the same certificate.
pub fn spki_sha256_hex(cert_der: &[u8]) -> Result<String, String> {
    use x509_parser::prelude::FromDer;
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(cert_der)
        .map_err(|e| format!("certificate DER did not parse: {e}"))?;
    Ok(scoreboard_protocol::sha256_hex(cert.tbs_certificate.subject_pki.raw))
}

/// A freshly minted identity: what `gen-tls-cert` prints.
pub struct GeneratedCert {
    pub cert_pem: String,
    pub key_pem: String,
    pub cert_der: Vec<u8>,
    /// SHA-256 of the DER SubjectPublicKeyInfo, lowercase hex.
    pub pin: String,
}

/// Years of validity stamped into a freshly minted certificate. Long, because
/// rotating it is a two-release firmware dance (see docs/cli.md) and because
/// nothing in the trust decision reads the dates: the board has no clock, and
/// the pin is what it checks. The dates exist so `openssl`/`curl` and the
/// occasional browser poke behave sensibly.
const VALIDITY_YEARS: i32 = 10;

/// Mint a self-signed Ed25519 certificate.
///
/// Ed25519 because it is the one key type this whole system already has code
/// for on every side: rustls signs with it (`ring`), embedded-tls verifies it
/// behind its `ed25519` feature, and the firmware ALREADY links ed25519-dalek
/// for OTA image verification — so the board pays no new crypto code for the
/// handshake. (P-384 is the only other type both ends support; it would drag
/// the whole `p384` crate onto the device. embedded-tls has no P-256
/// certificate support, so P-256 is not an option despite being the ECDHE
/// group both ends use for key exchange.)
pub fn generate_self_signed(subject_alt_names: &[String]) -> Result<GeneratedCert, String> {
    use chrono::Datelike as _;

    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)
        .map_err(|e| format!("generating an Ed25519 key: {e}"))?;

    let mut params = rcgen::CertificateParams::new(subject_alt_names.to_vec())
        .map_err(|e| format!("bad subject alt name: {e}"))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "scoreboard board link");
    // notBefore well in the past: some clients (not ours — ours has no clock)
    // sit behind wrong RTCs, and a certificate that is not yet valid is a
    // uniquely annoying failure.
    params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    let today = chrono::Utc::now().date_naive();
    params.not_after = rcgen::date_time_ymd(
        today.year() + VALIDITY_YEARS,
        today.month() as u8,
        today.day() as u8,
    );

    let cert = params
        .self_signed(&key)
        .map_err(|e| format!("self-signing: {e}"))?;
    let cert_der = cert.der().to_vec();
    let pin = spki_sha256_hex(&cert_der)?;

    // Cross-check the pin against rcgen's own SPKI encoder before handing the
    // owner a value they will bake into firmware: if these two disagree, the
    // pin is wrong and no board would ever connect.
    use rcgen::PublicKeyData as _;
    let from_keypair = scoreboard_protocol::sha256_hex(&key.subject_public_key_info());
    if from_keypair != pin {
        return Err(format!(
            "self-check failed: SPKI hash from the certificate ({pin}) does not match the \
             key pair's SPKI hash ({from_keypair})"
        ));
    }

    Ok(GeneratedCert {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
        cert_der,
        pin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec!["localhost".to_string()]
    }

    /// The value `gen-tls-cert` prints is the value the server derives from the
    /// PEM it later loads — the two must not be able to drift, since the
    /// firmware is built against the first and talks to the second.
    #[test]
    fn printed_pin_matches_the_pin_derived_from_the_loaded_pem() {
        let gen = generate_self_signed(&names()).unwrap();
        assert_eq!(gen.pin.len(), 64, "pin is a 32-byte hash in hex: {}", gen.pin);
        assert!(
            gen.pin.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "pin is lowercase hex: {}",
            gen.pin
        );

        let loaded = from_pem(&gen.cert_pem, &gen.key_pem).unwrap();
        assert_eq!(loaded.pin, gen.pin);
    }

    /// The pin is the SPKI hash, NOT the whole-certificate hash. The firmware
    /// computes one specific thing; this nails down which.
    #[test]
    fn pin_is_the_spki_hash_not_the_certificate_hash() {
        let gen = generate_self_signed(&names()).unwrap();
        let whole_cert = scoreboard_protocol::sha256_hex(&gen.cert_der);
        assert_ne!(gen.pin, whole_cert);

        // The SPKI is a strict, contiguous substring of the certificate DER —
        // i.e. the pin covers bytes lifted verbatim out of what the board
        // receives on the wire, with nothing recomputed or re-encoded.
        use x509_parser::prelude::FromDer;
        let (_, parsed) =
            x509_parser::certificate::X509Certificate::from_der(&gen.cert_der).unwrap();
        let spki = parsed.tbs_certificate.subject_pki.raw;
        assert!(
            gen.cert_der.windows(spki.len()).any(|w| w == spki),
            "the hashed SPKI bytes appear verbatim inside the certificate DER"
        );
        assert_eq!(scoreboard_protocol::sha256_hex(spki), gen.pin);
    }

    /// Why SPKI and not the certificate: re-issuing with the same key keeps the
    /// pin. Different SANs (and so different DER) — same pin, no firmware ship.
    #[test]
    fn pin_survives_certificate_reissue_with_the_same_key() {
        use chrono::Datelike as _;

        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let issue = |sans: Vec<String>| {
            let mut params = rcgen::CertificateParams::new(sans).unwrap();
            params.not_before = rcgen::date_time_ymd(2020, 1, 1);
            params.not_after = rcgen::date_time_ymd(chrono::Utc::now().year() + 10, 1, 1);
            params.self_signed(&key).unwrap().der().to_vec()
        };

        let first = issue(vec!["localhost".into()]);
        let second = issue(vec!["localhost".into(), "board.example".into()]);
        assert_ne!(first, second, "the two certificates really are different");
        assert_eq!(spki_sha256_hex(&first).unwrap(), spki_sha256_hex(&second).unwrap());
    }

    /// A different key is a different pin — the pin actually identifies the key.
    #[test]
    fn a_new_key_is_a_new_pin() {
        let a = generate_self_signed(&names()).unwrap();
        let b = generate_self_signed(&names()).unwrap();
        assert_ne!(a.pin, b.pin);
    }

    /// Bad secrets are reported, not panicked on — `from_env` turns every one
    /// of these into "serve plaintext" rather than a crash-loop.
    #[test]
    fn broken_pem_is_an_error_not_a_panic() {
        let gen = generate_self_signed(&names()).unwrap();

        assert!(from_pem("not a certificate", &gen.key_pem).is_err());
        assert!(from_pem(&gen.cert_pem, "not a key").is_err());
        assert!(from_pem("", "").is_err());
        // A certificate paired with somebody else's key: rustls catches the
        // mismatch at load time rather than at handshake time.
        let other = generate_self_signed(&names()).unwrap();
        assert!(from_pem(&gen.cert_pem, &other.key_pem).is_err());

        assert!(spki_sha256_hex(b"\x30\x00 not a certificate").is_err());
    }

    /// rustls validates `max_fragment_size` when a connection is created, not
    /// when the config is built — so prove the chosen number is actually
    /// accepted rather than rejected on the first board that connects.
    #[test]
    fn the_record_cap_is_a_value_rustls_accepts() {
        let gen = generate_self_signed(&names()).unwrap();
        let tls = from_pem(&gen.cert_pem, &gen.key_pem).unwrap();
        assert_eq!(tls.config.max_fragment_size, Some(MAX_FRAGMENT_SIZE));
        rustls::ServerConnection::new(tls.config.clone())
            .expect("rustls accepts the configured max_fragment_size");

        // The arithmetic the firmware's buffer sizing depends on.
        assert_eq!(MAX_RECORD_PLAINTEXT, 4091);
        assert_eq!(MAX_RECORD_BODY, 4108);
        assert_eq!(MAX_RECORD_WIRE, 4113);
    }
}
