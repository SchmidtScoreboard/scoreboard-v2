//! Tests for the firmware's pinned-TLS verifier (`firmware-rs/src/tls.rs`).
//!
//! Two things are being defended here, and they are different:
//!
//! * **Identity** — the pin is computed exactly the way the server computes it,
//!   over exactly the bytes the server hashes.
//! * **Possession** — a peer that presents the right certificate but cannot
//!   sign with its private key is rejected. That is the whole game: a
//!   certificate is public, so a pin check with a rubber-stamp signature check
//!   is defeated by replaying a captured certificate.

use ed25519_dalek::{Signer, SigningKey};
use rcgen::PublicKeyData as _;
use sha2::{Digest, Sha256};
use tls_tests::tls::{
    ed25519_key_of_spki, pin_from_hex, spki_of_certificate, spki_pin, PinError, PinnedVerifier,
};

// ---------------------------------------------------------------------------
// A fixed certificate, so the pin RULE is pinned and not merely self-consistent
// ---------------------------------------------------------------------------
//
// Minted with the same rcgen + PKCS_ED25519 recipe as
// `crates/server/src/tls.rs::generate_self_signed`. Everything else in this
// file generates fresh keys; this one exists so that a change to what "the
// pin" means shows up as a failing constant rather than a suite that happily
// agrees with itself.

const FIXTURE_CERT_HEX: &str = concat!(
    "308201273081daa00302010202147f87f2dde9a09309ba05a2df0c8db464d8988ec3300506032b65703020311e301c06",
    "035504030c1573636f7265626f61726420626f617264206c696e6b301e170d3230303130313030303030305a170d3335",
    "303130313030303030305a3020311e301c06035504030c1573636f7265626f61726420626f617264206c696e6b302a30",
    "0506032b65700321004ffa9f5623a286bce0c9fb714c9cfc0201a1137ff880fac62fd3156d5c74e71ca3263024302206",
    "03551d11041b3019821768617961627573612e70726f78792e726c77792e6e6574300506032b6570034100db8d1373bf",
    "84784b92b62b52adba69651d04ccde475fca63daed38b39ead5d8c6f672c6f3a933063e65ce6ca1faf865d3e314c9f13",
    "a257d783713f8736e48100",
);

/// SHA-256 of the fixture's DER SubjectPublicKeyInfo, lowercase hex.
const FIXTURE_PIN: &str = "839f6daad00cae33c657239e4467d15ac920db3e095eb04bad31cc165c8a7e07";

/// The fixture's 32-byte Ed25519 seed, lifted out of its PKCS#8 (the bytes
/// after the `04 22 04 20` OCTET-STRING-in-OCTET-STRING header).
const FIXTURE_SEED: &str = "5c3f0824fa3a218c73392b2b27611baa86c4607d042a9121bd19e8136eb39f14";

fn fixture_cert() -> Vec<u8> {
    hex::decode(FIXTURE_CERT_HEX).unwrap()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct Server {
    cert_der: Vec<u8>,
    signing_key: SigningKey,
    pin: [u8; 32],
}

/// Mint a self-signed Ed25519 certificate the way the server does, and hand
/// back the private key so the tests can play a server that really does hold
/// it — and one that doesn't.
fn mint(host: &str) -> Server {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    let pkcs8 = key.serialize_der();
    let seed = ed25519_seed_from_pkcs8(&pkcs8);

    let mut params = rcgen::CertificateParams::new(vec![host.to_string()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "scoreboard board link");
    let cert = params.self_signed(&key).unwrap();
    let cert_der = cert.der().to_vec();
    let pin: [u8; 32] = Sha256::digest(key.subject_public_key_info()).into();

    Server {
        cert_der,
        signing_key: SigningKey::from_bytes(&seed),
        pin,
    }
}

fn ed25519_seed_from_pkcs8(pkcs8: &[u8]) -> [u8; 32] {
    let marker = [0x04u8, 0x22, 0x04, 0x20];
    let at = pkcs8
        .windows(4)
        .position(|w| w == marker)
        .expect("PKCS#8 Ed25519 private key header");
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&pkcs8[at + 4..at + 36]);
    seed
}

/// The bytes RFC 8446 §4.4.3 says a TLS 1.3 server signs, built here from the
/// RFC rather than by calling the firmware's own helper — otherwise a bug in
/// that helper would sign and verify the same wrong thing and look fine.
fn certificate_verify_bytes(transcript: &[u8; 32]) -> Vec<u8> {
    let mut msg = vec![0x20u8; 64];
    msg.extend_from_slice(b"TLS 1.3, server CertificateVerify");
    msg.push(0x00);
    msg.extend_from_slice(transcript);
    msg
}

fn verifier_for(pin: [u8; 32]) -> PinnedVerifier {
    PinnedVerifier::new(pin, None)
}

// ---------------------------------------------------------------------------
// The pin: identity
// ---------------------------------------------------------------------------

#[test]
fn the_fixture_certificate_hashes_to_the_documented_pin() {
    let cert = fixture_cert();
    let spki = spki_of_certificate(&cert).unwrap();
    assert_eq!(hex::encode(spki_pin(spki)), FIXTURE_PIN);
}

#[test]
fn pin_is_the_spki_hash_not_the_certificate_hash() {
    let cert = fixture_cert();
    let spki = spki_of_certificate(&cert).unwrap();
    let whole_cert: [u8; 32] = Sha256::digest(&cert).into();

    assert_ne!(
        hex::encode(whole_cert),
        FIXTURE_PIN,
        "hashing the whole certificate would break on every reissue of the same key"
    );
    // And the SPKI really is a verbatim slice of the certificate, not
    // something re-encoded on the way past.
    assert!(cert.windows(spki.len()).any(|w| w == spki));
}

#[test]
fn our_der_walk_agrees_with_rcgens_own_spki_encoder() {
    // The server cross-checks its pin against `key.subject_public_key_info()`
    // before printing it. Same oracle, other end of the wire.
    for host in ["hayabusa.proxy.rlwy.net", "localhost", "192.168.4.45"] {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let params = rcgen::CertificateParams::new(vec![host.to_string()]).unwrap();
        let cert = params.self_signed(&key).unwrap();

        let ours = spki_of_certificate(cert.der()).unwrap();
        assert_eq!(ours, key.subject_public_key_info().as_slice(), "host {host}");
    }
}

#[test]
fn an_ed25519_spki_has_the_documented_44_byte_shape() {
    let cert = fixture_cert();
    let spki = spki_of_certificate(&cert).unwrap();
    assert_eq!(spki.len(), 44);
    assert_eq!(
        &spki[..12],
        &[0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00]
    );
}

#[test]
fn the_key_lifted_from_the_spki_is_the_signing_key() {
    let server = mint("board.example");
    let spki = spki_of_certificate(&server.cert_der).unwrap();
    let key = ed25519_key_of_spki(spki).unwrap();
    assert_eq!(key, server.signing_key.verifying_key().to_bytes());
    // And the last 32 bytes of the SPKI are exactly that key.
    assert_eq!(&spki[12..], &key[..]);
}

#[test]
fn the_pin_survives_reissuing_the_certificate_with_the_same_key() {
    // This is what makes a staged rotation possible at all: the pin follows
    // the KEY, so re-minting a certificate (new serial, new validity, new
    // SANs) does not strand a single board in the field.
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    let first = rcgen::CertificateParams::new(vec!["a.example".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let second = rcgen::CertificateParams::new(vec!["b.example".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();

    assert_ne!(first.der().as_ref(), second.der().as_ref());
    assert_eq!(
        spki_pin(spki_of_certificate(first.der()).unwrap()),
        spki_pin(spki_of_certificate(second.der()).unwrap())
    );
}

// ---------------------------------------------------------------------------
// The pin: acceptance and rejection
// ---------------------------------------------------------------------------

#[test]
fn the_pinned_certificate_is_accepted() {
    let server = mint("board.example");
    let mut v = verifier_for(server.pin);
    assert_eq!(
        v.accept_certificate(&server.cert_der).unwrap(),
        server.signing_key.verifying_key().to_bytes()
    );
}

#[test]
fn some_other_servers_certificate_is_rejected() {
    let ours = mint("board.example");
    let theirs = mint("board.example");
    assert_ne!(ours.pin, theirs.pin);

    let mut v = verifier_for(ours.pin);
    assert_eq!(
        v.accept_certificate(&theirs.cert_der),
        Err(PinError::NoPinMatch)
    );
}

#[test]
fn a_single_flipped_bit_in_the_pin_rejects() {
    let server = mint("board.example");
    let mut wrong = server.pin;
    wrong[31] ^= 0x01;
    let mut v = verifier_for(wrong);
    assert_eq!(
        v.accept_certificate(&server.cert_der),
        Err(PinError::NoPinMatch)
    );
}

#[test]
fn the_successor_pin_is_accepted_only_when_it_is_staged() {
    let current = mint("board.example");
    let successor = mint("board.example");

    // No rotation in flight: only the current key gets in.
    let mut solo = PinnedVerifier::new(current.pin, None);
    assert!(solo.accept_certificate(&current.cert_der).is_ok());
    assert_eq!(
        solo.accept_certificate(&successor.cert_der),
        Err(PinError::NoPinMatch)
    );

    // Rotation staged: both, so the server's key can be swapped under a fleet
    // that is already running.
    let mut staged = PinnedVerifier::new(current.pin, Some(successor.pin));
    assert!(staged.accept_certificate(&current.cert_der).is_ok());
    assert!(staged.accept_certificate(&successor.cert_der).is_ok());

    // A third key is still nobody.
    let stranger = mint("board.example");
    assert_eq!(
        staged.accept_certificate(&stranger.cert_der),
        Err(PinError::NoPinMatch)
    );
}

#[test]
fn a_rejected_certificate_leaves_no_key_behind() {
    // A failed second Certificate must not leave the verifier armed with the
    // key from a successful first one.
    let good = mint("board.example");
    let bad = mint("board.example");
    let mut v = verifier_for(good.pin);
    assert!(v.accept_certificate(&good.cert_der).is_ok());
    assert!(v.accept_certificate(&bad.cert_der).is_err());

    let transcript = [7u8; 32];
    let sig = good
        .signing_key
        .sign(&certificate_verify_bytes(&transcript));
    assert_eq!(
        v.check_signature(
            embedded_tls::SignatureScheme::Ed25519,
            &sig.to_bytes(),
            &transcript
        ),
        Err(PinError::Order),
        "the key from the accepted certificate must not survive a rejection"
    );
}

// ---------------------------------------------------------------------------
// Possession: the half that a pin alone cannot provide
// ---------------------------------------------------------------------------

#[test]
fn a_real_server_proves_possession() {
    let server = mint("board.example");
    let transcript = [0xABu8; 32];
    let sig = server
        .signing_key
        .sign(&certificate_verify_bytes(&transcript));

    let mut v = verifier_for(server.pin);
    v.accept_certificate(&server.cert_der).unwrap();
    assert_eq!(
        v.check_signature(
            embedded_tls::SignatureScheme::Ed25519,
            &sig.to_bytes(),
            &transcript
        ),
        Ok(())
    );
}

#[test]
fn replaying_a_captured_certificate_without_the_private_key_fails() {
    // THE attack the pin alone does not stop. A certificate is public: an
    // on-path attacker copies the server's verbatim, so `accept_certificate`
    // says yes. What the attacker cannot do is sign this handshake's
    // transcript. If `verify_signature` were a stub returning Ok, everything
    // below would pass and the whole scheme would be decoration.
    let server = mint("board.example");
    let attacker = SigningKey::from_bytes(&[0x42u8; 32]);
    let transcript = [0x11u8; 32];

    let mut v = verifier_for(server.pin);
    v.accept_certificate(&server.cert_der)
        .expect("the replayed certificate passes the pin check — that is the point");

    let forged = attacker.sign(&certificate_verify_bytes(&transcript));
    assert_eq!(
        v.check_signature(
            embedded_tls::SignatureScheme::Ed25519,
            &forged.to_bytes(),
            &transcript
        ),
        Err(PinError::Signature)
    );
}

#[test]
fn a_signature_from_an_earlier_handshake_does_not_replay() {
    // The transcript is fresh every handshake, which is precisely why a
    // recorded CertificateVerify is worthless.
    let server = mint("board.example");
    let old_transcript = [0x01u8; 32];
    let new_transcript = [0x02u8; 32];
    let recorded = server
        .signing_key
        .sign(&certificate_verify_bytes(&old_transcript));

    let mut v = verifier_for(server.pin);
    v.accept_certificate(&server.cert_der).unwrap();
    assert_eq!(
        v.check_signature(
            embedded_tls::SignatureScheme::Ed25519,
            &recorded.to_bytes(),
            &new_transcript
        ),
        Err(PinError::Signature)
    );
}

#[test]
fn the_signed_message_is_the_one_rfc_8446_specifies() {
    // A signature over the raw transcript, or over the context string without
    // its NUL, or with the wrong pad length, must all fail — otherwise our
    // construction differs from every real TLS 1.3 server's and no handshake
    // would ever complete.
    let server = mint("board.example");
    let transcript = [0x5Au8; 32];

    let mut wrong_shapes: Vec<Vec<u8>> = Vec::new();
    wrong_shapes.push(transcript.to_vec());
    {
        let mut m = vec![0x20u8; 64];
        m.extend_from_slice(b"TLS 1.3, server CertificateVerify");
        m.extend_from_slice(&transcript); // missing the NUL separator
        wrong_shapes.push(m);
    }
    {
        let mut m = vec![0x20u8; 63]; // one pad byte short
        m.extend_from_slice(b"TLS 1.3, server CertificateVerify");
        m.push(0);
        m.extend_from_slice(&transcript);
        wrong_shapes.push(m);
    }
    {
        let mut m = vec![0x20u8; 64];
        m.extend_from_slice(b"TLS 1.3, client CertificateVerify"); // client, not server
        m.push(0);
        m.extend_from_slice(&transcript);
        wrong_shapes.push(m);
    }

    let mut v = verifier_for(server.pin);
    v.accept_certificate(&server.cert_der).unwrap();
    for (i, shape) in wrong_shapes.iter().enumerate() {
        let sig = server.signing_key.sign(shape);
        assert_eq!(
            v.check_signature(
                embedded_tls::SignatureScheme::Ed25519,
                &sig.to_bytes(),
                &transcript
            ),
            Err(PinError::Signature),
            "wrong-shape message {i} was accepted"
        );
    }
}

#[test]
fn only_ed25519_signatures_are_accepted() {
    // embedded-tls's default ClientHello also offers ECDSA and TlsConfig gives
    // no way to withdraw the offer, so the narrowing has to happen in the
    // verifier: our server signs with Ed25519, and a peer answering with
    // anything else is not our server.
    let server = mint("board.example");
    let transcript = [0x33u8; 32];
    let sig = server
        .signing_key
        .sign(&certificate_verify_bytes(&transcript));

    let mut v = verifier_for(server.pin);
    v.accept_certificate(&server.cert_der).unwrap();
    for scheme in [
        embedded_tls::SignatureScheme::EcdsaSecp256r1Sha256,
        embedded_tls::SignatureScheme::EcdsaSecp384r1Sha384,
        embedded_tls::SignatureScheme::RsaPssRsaeSha256,
        embedded_tls::SignatureScheme::RsaPkcs1Sha256,
    ] {
        assert_eq!(
            v.check_signature(scheme, &sig.to_bytes(), &transcript),
            Err(PinError::Scheme),
            "{scheme:?} should not be accepted"
        );
    }
}

#[test]
fn a_certificate_verify_without_a_certificate_is_rejected() {
    let server = mint("board.example");
    let transcript = [0x77u8; 32];
    let sig = server
        .signing_key
        .sign(&certificate_verify_bytes(&transcript));

    // Never saw a Certificate: there is no key to check against, and a
    // verifier that shrugged here would accept anyone.
    let v = verifier_for(server.pin);
    assert_eq!(
        v.check_signature(
            embedded_tls::SignatureScheme::Ed25519,
            &sig.to_bytes(),
            &transcript
        ),
        Err(PinError::Order)
    );
}

#[test]
fn a_malformed_signature_is_rejected_rather_than_panicking() {
    let server = mint("board.example");
    let transcript = [0x99u8; 32];
    let mut v = verifier_for(server.pin);
    v.accept_certificate(&server.cert_der).unwrap();

    for bogus in [vec![], vec![0u8; 63], vec![0u8; 65], vec![0xFFu8; 64]] {
        assert!(v
            .check_signature(
                embedded_tls::SignatureScheme::Ed25519,
                &bogus,
                &transcript
            )
            .is_err());
    }
}

#[test]
fn completed_demands_both_halves() {
    // `open()` returning Ok is not proof of anything: embedded-tls only runs
    // the verifier for the handshake messages the server chose to send.
    let server = mint("board.example");
    let transcript = [0x21u8; 32];

    let v = verifier_for(server.pin);
    assert!(!v.completed(), "a fresh verifier has verified nothing");

    let mut v = verifier_for(server.pin);
    v.accept_certificate(&server.cert_der).unwrap();
    assert!(
        !v.completed(),
        "identity alone is not enough — the certificate is public"
    );

    // The TlsVerifier path is what flips the second flag; drive it through the
    // trait so the flag and the real handshake hook cannot diverge.
    use embedded_tls::{CertificateVerifyRef, SignatureScheme, TlsVerifier};
    let sig = server
        .signing_key
        .sign(&certificate_verify_bytes(&Sha256::digest(b"transcript").into()));
    let mut v = verifier_for(server.pin);
    v.accept_certificate(&server.cert_der).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(b"transcript");
    TlsVerifier::<embedded_tls::Aes128GcmSha256>::verify_certificate(
        &mut v,
        &hasher,
        certificate_message(&server.cert_der),
    )
    .unwrap();
    let sig_bytes = sig.to_bytes();
    TlsVerifier::<embedded_tls::Aes128GcmSha256>::verify_signature(
        &mut v,
        CertificateVerifyRef {
            signature_scheme: SignatureScheme::Ed25519,
            signature: &sig_bytes,
        },
    )
    .unwrap();
    assert!(v.completed());
    let _ = transcript;
}

fn certificate_message(der: &[u8]) -> embedded_tls::CertificateRef<'_> {
    let mut cert = embedded_tls::CertificateRef::with_context(&[]);
    cert.add(embedded_tls::CertificateEntryRef::X509(der)).unwrap();
    cert
}

// ---------------------------------------------------------------------------
// DER robustness — the parser runs on attacker-controlled bytes
// ---------------------------------------------------------------------------

#[test]
fn malformed_certificates_are_errors_not_panics() {
    let good = fixture_cert();
    let mut cases: Vec<Vec<u8>> = vec![
        vec![],
        vec![0x30],
        vec![0x30, 0x82],
        b"not a certificate at all".to_vec(),
        vec![0x30, 0x00],                   // empty SEQUENCE
        vec![0x02, 0x01, 0x00],             // an INTEGER, not a certificate
        vec![0x30, 0x80, 0x00, 0x00],       // BER indefinite length
        vec![0x30, 0x84, 0xff, 0xff, 0xff, 0xff], // absurd length
    ];
    // Every truncation of a real certificate.
    for cut in (1..good.len()).step_by(7) {
        cases.push(good[..cut].to_vec());
    }
    // Trailing slack after a valid certificate: DER has none, and accepting it
    // would mean hashing bytes some other parser wouldn't.
    let mut trailing = good.clone();
    trailing.push(0x00);
    cases.push(trailing);

    for (i, case) in cases.iter().enumerate() {
        assert!(
            spki_of_certificate(case).is_err(),
            "case {i} ({} bytes) should not parse",
            case.len()
        );
    }
    // The unmodified fixture still parses, so the loop above isn't vacuous.
    assert!(spki_of_certificate(&good).is_ok());
}

#[test]
fn a_non_ed25519_key_is_refused_even_if_its_pin_matched() {
    // Pins are opaque hashes; nothing stops someone pinning an ECDSA
    // certificate by mistake. Reading a P-256 point as an Ed25519 key would be
    // nonsense, so say so instead.
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let cert = rcgen::CertificateParams::new(vec!["board.example".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let spki = spki_of_certificate(cert.der()).unwrap();
    assert_eq!(ed25519_key_of_spki(spki), Err(PinError::NotEd25519));

    // ...and it is refused through the front door too, with a matching pin.
    let pin: [u8; 32] = Sha256::digest(key.subject_public_key_info()).into();
    let mut v = verifier_for(pin);
    assert_eq!(
        v.accept_certificate(cert.der()),
        Err(PinError::NotEd25519)
    );
}

// ---------------------------------------------------------------------------
// The compiled-in pin constants
// ---------------------------------------------------------------------------

#[test]
fn pin_from_hex_round_trips() {
    assert_eq!(hex::encode(pin_from_hex(FIXTURE_PIN)), FIXTURE_PIN);
    // Case-insensitive, since a pin pasted from another tool may be upper.
    assert_eq!(
        pin_from_hex(&FIXTURE_PIN.to_uppercase()),
        pin_from_hex(FIXTURE_PIN)
    );
}

#[test]
fn the_fixture_seed_really_is_the_fixture_certificates_key() {
    // Guards the fixture itself: if these constants ever drift apart the
    // signature tests above would be testing nothing.
    let seed: [u8; 32] = hex::decode(FIXTURE_SEED).unwrap().try_into().unwrap();
    let signing = SigningKey::from_bytes(&seed);
    let cert = fixture_cert();
    let spki = spki_of_certificate(&cert).unwrap();
    assert_eq!(
        ed25519_key_of_spki(spki).unwrap(),
        signing.verifying_key().to_bytes()
    );

    // End to end on the fixture, with the pin as a hex constant exactly as
    // `config.rs` holds it.
    let transcript = [0xC3u8; 32];
    let sig = signing.sign(&certificate_verify_bytes(&transcript));
    let mut v = PinnedVerifier::new(pin_from_hex(FIXTURE_PIN), None);
    v.accept_certificate(&fixture_cert()).unwrap();
    assert_eq!(
        v.check_signature(
            embedded_tls::SignatureScheme::Ed25519,
            &sig.to_bytes(),
            &transcript
        ),
        Ok(())
    );
}
