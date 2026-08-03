//! Pinned TLS 1.3 verification for the board <-> server websocket.
//!
//! The board talks to exactly ONE server, so it pins that server's public key
//! instead of walking a PKI chain. That buys two things a certificate-authority
//! bundle can't:
//!
//! * **No CA bundle.** Nothing to ship, nothing to expire, ~0 flash.
//! * **No wall clock.** The board has no RTC and no NTP, so it cannot evaluate
//!   `notBefore`/`notAfter`. PKI validation without a clock is theatre: it would
//!   accept a certificate that expired years ago. A pin has no validity window,
//!   so there is nothing to fake.
//!
//! ## The pin
//!
//! ```text
//! pin = SHA-256( the certificate's DER-encoded SubjectPublicKeyInfo )
//! ```
//!
//! The whole `SubjectPublicKeyInfo` SEQUENCE — `AlgorithmIdentifier` plus the
//! `BIT STRING subjectPublicKey` — lifted verbatim out of the certificate.
//! NOT the bare 32-byte Ed25519 point, and NOT the hash of the whole
//! certificate (that would break on every reissue of the same key). For
//! Ed25519 the SPKI is always 44 bytes: `30 2a 30 05 06 03 2b 65 70 03 21 00`
//! followed by the 32-byte key. This matches `crates/server/src/tls.rs`
//! byte-for-byte; `gen-tls-cert --pin <cert.pem>` prints the same string.
//!
//! ## Two halves, and why both must be real
//!
//! [`PinnedVerifier`] implements the two hooks embedded-tls calls during the
//! handshake, and NEITHER may be a stub:
//!
//! * [`verify_certificate`](PinnedVerifier::verify_certificate) establishes
//!   **identity** — the presented certificate carries the key we expect.
//! * [`verify_signature`](PinnedVerifier::verify_signature) establishes
//!   **possession** — the peer actually holds the matching private key,
//!   because it signed this handshake's transcript with it.
//!
//! A certificate is public. Any on-path attacker can copy the server's and
//! present it verbatim. If `verify_signature` returned `Ok(())` the pin check
//! would stop exactly nobody: replay the captured certificate, finish the
//! ECDHE, done. The signature over the transcript is the only thing that
//! cannot be replayed, because the transcript is fresh every handshake.
//!
//! There is a third hook that embedded-tls does not provide, and this module
//! adds: [`PinnedVerifier::completed`]. embedded-tls only runs the verifier
//! when the server *sends* Certificate/CertificateVerify — a server that skips
//! straight to Finished is never verified at all, and an active attacker who
//! completed the ECDHE can produce a valid Finished. `completed()` is checked
//! after the handshake and turns "the verifier never ran" into a hard failure.
//!
//! Everything in this file is pure: no esp-hal, no allocator, no I/O. That is
//! deliberate — `tls-tests/` compiles this exact source for the host and runs
//! it against certificates minted by the same `rcgen` the server uses.

#![allow(dead_code)]

use ed25519_dalek::{Signature, VerifyingKey};
use embedded_tls::{
    Aes128GcmSha256, CertificateEntryRef, CertificateRef, CertificateVerifyRef, CryptoProvider,
    CryptoRngCore, SignatureScheme, TlsError, TlsVerifier,
};
use sha2::{Digest, Sha256};

/// A pin is a SHA-256 digest.
pub const PIN_LEN: usize = 32;

/// `id-Ed25519` (RFC 8410, OID 1.3.101.112) as it appears inside an
/// `AlgorithmIdentifier`: the OID, and — per RFC 8410 §3 — nothing else, since
/// the `parameters` field MUST be absent for Ed25519.
const ED25519_ALG_ID: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x70];

/// RFC 8446 §4.4.3: the context string that separates a server's
/// CertificateVerify signature from every other signature it might make.
const CERT_VERIFY_CONTEXT: &[u8] = b"TLS 1.3, server CertificateVerify\x00";

/// 64 pad bytes + 34 context bytes + a 32-byte SHA-256 transcript.
const CERT_VERIFY_MSG_LEN: usize = 64 + 34 + 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinError {
    /// The certificate is not well-formed DER, or not shaped like an X.509
    /// certificate.
    Der,
    /// The certificate's key is not Ed25519.
    NotEd25519,
    /// The certificate's SPKI hash matched neither compiled-in pin.
    NoPinMatch,
    /// The server presented no certificate, or a bare public key.
    NoCertificate,
    /// The peer signed with an algorithm we do not accept.
    Scheme,
    /// The CertificateVerify signature did not check out against the pinned key.
    Signature,
    /// CertificateVerify arrived without a preceding, accepted Certificate.
    Order,
}

impl PinError {
    pub fn as_str(self) -> &'static str {
        match self {
            PinError::Der => "certificate is not parseable DER",
            PinError::NotEd25519 => "certificate key is not Ed25519",
            PinError::NoPinMatch => "certificate key does not match a pinned key",
            PinError::NoCertificate => "server presented no X.509 certificate",
            PinError::Scheme => "unacceptable signature algorithm",
            PinError::Signature => "CertificateVerify signature is invalid",
            PinError::Order => "CertificateVerify without an accepted Certificate",
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal DER walk
// ---------------------------------------------------------------------------

/// Split one DER TLV off the front of `input`.
///
/// Returns `(tag, whole TLV including its header, content, bytes consumed)`.
/// Strict on purpose: definite lengths only, shortest-form lengths only, no
/// trailing slack. We are about to hash whatever we call "the SPKI", so a lax
/// parser that could be steered to a different byte range than the server's
/// parser would be a real problem.
fn tlv(input: &[u8]) -> Result<(u8, &[u8], &[u8], usize), PinError> {
    if input.len() < 2 {
        return Err(PinError::Der);
    }
    let tag = input[0];
    let first = input[1];
    let (len, header) = if first < 0x80 {
        (first as usize, 2)
    } else {
        let count = (first & 0x7f) as usize;
        // 0x80 is BER's indefinite length (illegal in DER); >3 length bytes
        // would mean a certificate larger than 16MB.
        if count == 0 || count > 3 || input.len() < 2 + count {
            return Err(PinError::Der);
        }
        // DER requires the shortest possible encoding: no leading zero byte,
        // and the long form only for lengths that don't fit the short form.
        if input[2] == 0 {
            return Err(PinError::Der);
        }
        let mut len = 0usize;
        let mut i = 0;
        while i < count {
            len = (len << 8) | input[2 + i] as usize;
            i += 1;
        }
        if len < 0x80 {
            return Err(PinError::Der);
        }
        (len, 2 + count)
    };
    let end = match header.checked_add(len) {
        Some(end) if end <= input.len() => end,
        _ => return Err(PinError::Der),
    };
    Ok((tag, &input[..end], &input[header..end], end))
}

/// Skip `n` TLVs, returning the remainder.
fn skip(mut input: &[u8], n: usize) -> Result<&[u8], PinError> {
    for _ in 0..n {
        let (_, _, _, used) = tlv(input)?;
        input = &input[used..];
    }
    Ok(input)
}

/// The certificate's DER `SubjectPublicKeyInfo`, verbatim — header and all.
///
/// ```text
/// Certificate     ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signatureValue }
/// TBSCertificate  ::= SEQUENCE { [0] version DEFAULT v1, serialNumber, signature,
///                                issuer, validity, subject, subjectPublicKeyInfo, ... }
/// ```
///
/// So: into the outer SEQUENCE, into the TBS SEQUENCE, skip the optional
/// explicit `[0] version`, then skip five fields (serial, signature, issuer,
/// validity, subject) and take the sixth.
pub fn spki_of_certificate(cert_der: &[u8]) -> Result<&[u8], PinError> {
    let (tag, whole, cert, _) = tlv(cert_der)?;
    if tag != 0x30 || whole.len() != cert_der.len() {
        // Not a SEQUENCE, or there are bytes after it. Either way this isn't a
        // certificate as TLS delivered it.
        return Err(PinError::Der);
    }
    let (tag, _, tbs, _) = tlv(cert)?;
    if tag != 0x30 {
        return Err(PinError::Der);
    }
    // `[0] EXPLICIT Version` is optional (absent means v1).
    let rest = if !tbs.is_empty() && tbs[0] == 0xa0 {
        skip(tbs, 1)?
    } else {
        tbs
    };
    let rest = skip(rest, 5)?;
    let (tag, spki, _, _) = tlv(rest)?;
    if tag != 0x30 {
        return Err(PinError::Der);
    }
    Ok(spki)
}

/// The 32-byte Ed25519 public key inside an SPKI.
///
/// ```text
/// SubjectPublicKeyInfo ::= SEQUENCE { algorithm AlgorithmIdentifier,
///                                     subjectPublicKey BIT STRING }
/// ```
///
/// The algorithm is checked even though the pin already covers these bytes:
/// the pin proves the SPKI is the one we expect, this proves we are reading it
/// the way we think we are, and it keeps a future non-Ed25519 pin from being
/// silently misinterpreted as an Ed25519 point.
pub fn ed25519_key_of_spki(spki: &[u8]) -> Result<[u8; 32], PinError> {
    let (tag, _, body, _) = tlv(spki)?;
    if tag != 0x30 {
        return Err(PinError::Der);
    }
    let (tag, _, alg, used) = tlv(body)?;
    if tag != 0x30 {
        return Err(PinError::Der);
    }
    if alg != ED25519_ALG_ID {
        return Err(PinError::NotEd25519);
    }
    let (tag, _, bits, used_bits) = tlv(&body[used..])?;
    if tag != 0x03 || used + used_bits != body.len() {
        return Err(PinError::Der);
    }
    // A BIT STRING's first content byte counts the unused trailing bits; a
    // whole number of key bytes means zero.
    if bits.len() != 33 || bits[0] != 0 {
        return Err(PinError::NotEd25519);
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bits[1..]);
    Ok(key)
}

/// `pin = SHA-256(SPKI DER)`.
pub fn spki_pin(spki: &[u8]) -> [u8; PIN_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(spki);
    let digest = hasher.finalize();
    let mut pin = [0u8; PIN_LEN];
    pin.copy_from_slice(&digest);
    pin
}

/// Compare two pins without an early exit.
///
/// A pin is public data, so a timing oracle here leaks nothing an attacker
/// couldn't read off the wire — but branch-free comparison of a secret-shaped
/// value costs nothing and removes the question. `black_box` is what actually
/// holds the line: without it LLVM is free to turn the accumulator back into a
/// short-circuiting `memcmp`.
fn pins_match(a: &[u8; PIN_LEN], b: &[u8; PIN_LEN]) -> bool {
    let mut diff = 0u8;
    for i in 0..PIN_LEN {
        diff |= a[i] ^ b[i];
    }
    core::hint::black_box(diff) == 0
}

/// Decode a 64-character hex pin at compile time.
///
/// Const-evaluated, so a typo in `config.rs` is a build error rather than a
/// board that silently refuses every server. Rejects the all-zero digest as
/// well: an "unused" successor slot must be `None`, never a pin that a
/// contrived SPKI could conceivably hash to.
pub const fn pin_from_hex(hex: &str) -> [u8; PIN_LEN] {
    let bytes = hex.as_bytes();
    assert!(bytes.len() == 64, "a pin is 64 hex characters (SHA-256)");
    let mut out = [0u8; PIN_LEN];
    let mut i = 0;
    let mut any_set = false;
    while i < PIN_LEN {
        let hi = hex_nibble(bytes[i * 2]);
        let lo = hex_nibble(bytes[i * 2 + 1]);
        out[i] = (hi << 4) | lo;
        if out[i] != 0 {
            any_set = true;
        }
        i += 1;
    }
    assert!(any_set, "an all-zero pin is not a pin; use None instead");
    out
}

const fn hex_nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("a pin is hexadecimal"),
    }
}

/// The bytes a TLS 1.3 server signs in CertificateVerify (RFC 8446 §4.4.3):
/// 64 spaces, the context string, a NUL, then the handshake transcript hash
/// through the Certificate message.
fn certificate_verify_message(transcript: &[u8]) -> Option<[u8; CERT_VERIFY_MSG_LEN]> {
    if transcript.len() != 32 {
        return None;
    }
    let mut msg = [0x20u8; CERT_VERIFY_MSG_LEN];
    msg[64..64 + CERT_VERIFY_CONTEXT.len()].copy_from_slice(CERT_VERIFY_CONTEXT);
    msg[64 + CERT_VERIFY_CONTEXT.len()..].copy_from_slice(transcript);
    Some(msg)
}

// ---------------------------------------------------------------------------
// The verifier
// ---------------------------------------------------------------------------

/// Server verification by public-key pin. See the module docs.
pub struct PinnedVerifier {
    /// Current pin, then the staged successor. Two from day one so a key
    /// rotation is a rollout (ship firmware trusting both, swap the server's
    /// key, ship firmware trusting only the new one) instead of a cliff where
    /// every board in the field drops off at the same instant.
    pins: [[u8; PIN_LEN]; 2],
    pin_count: usize,
    /// Key lifted out of the certificate that matched a pin. `verify_signature`
    /// uses THIS key, never one re-parsed from whatever arrives later.
    key: Option<[u8; 32]>,
    /// Handshake transcript as of the Certificate message.
    transcript: Option<Sha256>,
    cert_ok: bool,
    sig_ok: bool,
    /// Last failure, for a serial log line the handshake error can't carry.
    pub last_error: Option<PinError>,
}

impl PinnedVerifier {
    #[must_use]
    pub fn new(current: [u8; PIN_LEN], successor: Option<[u8; PIN_LEN]>) -> Self {
        let mut pins = [[0u8; PIN_LEN]; 2];
        pins[0] = current;
        let pin_count = match successor {
            Some(next) => {
                pins[1] = next;
                2
            }
            None => 1,
        };
        Self {
            pins,
            pin_count,
            key: None,
            transcript: None,
            cert_ok: false,
            sig_ok: false,
            last_error: None,
        }
    }

    /// Did BOTH halves actually run and pass?
    ///
    /// embedded-tls invokes the verifier only for the handshake messages the
    /// server chose to send (`connection.rs`: `ServerHandshake::Certificate` /
    /// `CertificateVerify` arms). A server that sends neither is never
    /// verified, and its Finished still checks out because Finished is keyed
    /// off the ECDHE secret — which an active attacker has. So "the handshake
    /// returned Ok" is NOT by itself proof of anything; this is.
    #[must_use]
    pub fn completed(&self) -> bool {
        self.cert_ok && self.sig_ok
    }

    fn fail(&mut self, err: PinError, tls: TlsError) -> TlsError {
        self.last_error = Some(err);
        tls
    }

    /// The identity half, factored out so the host tests can drive it with a
    /// plain certificate DER instead of a TLS handshake.
    ///
    /// On success the key is retained: `check_signature` verifies against the
    /// key from the certificate that matched a pin, never against one
    /// re-parsed from a later message.
    pub fn accept_certificate(&mut self, cert_der: &[u8]) -> Result<[u8; 32], PinError> {
        self.key = None;
        self.cert_ok = false;
        let spki = spki_of_certificate(cert_der)?;
        let pin = spki_pin(spki);
        let mut matched = false;
        for i in 0..self.pin_count {
            // No early exit: every configured pin is compared, every time.
            matched |= pins_match(&pin, &self.pins[i]);
        }
        if !matched {
            return Err(PinError::NoPinMatch);
        }
        let key = ed25519_key_of_spki(spki)?;
        self.key = Some(key);
        self.cert_ok = true;
        Ok(key)
    }

    /// The possession half, likewise factored out for the host tests.
    pub fn check_signature(
        &self,
        scheme: SignatureScheme,
        signature: &[u8],
        transcript: &[u8],
    ) -> Result<(), PinError> {
        // Ed25519 only. embedded-tls's default ClientHello also offers ECDSA,
        // and TlsConfig has no way to withdraw an offer, so the narrowing
        // happens here: our server's key is Ed25519, and a peer answering with
        // anything else is not our server.
        if scheme != SignatureScheme::Ed25519 {
            return Err(PinError::Scheme);
        }
        let key = self.key.ok_or(PinError::Order)?;
        let msg = certificate_verify_message(transcript).ok_or(PinError::Order)?;
        let vk = VerifyingKey::from_bytes(&key).map_err(|_| PinError::NotEd25519)?;
        let sig = Signature::from_slice(signature).map_err(|_| PinError::Signature)?;
        // `verify_strict` over `verify`: it rejects small-order and
        // non-canonical public keys, so signature validity can't hinge on
        // which Ed25519 implementation the peer used.
        vk.verify_strict(&msg, &sig).map_err(|_| PinError::Signature)
    }
}

impl TlsVerifier<Aes128GcmSha256> for PinnedVerifier {
    /// Recorded and ignored — deliberately.
    ///
    /// Hostname matching answers "does this certificate speak for the name I
    /// dialled", which is a question you ask a CA-issued certificate because a
    /// CA will sign for many names. A pin answers something stricter: this is
    /// THE key, not merely a key some authority vouched for. The SNI we send
    /// is still the real hostname (see `link.rs`), so the ClientHello looks
    /// ordinary on the wire.
    fn set_hostname_verification(&mut self, _hostname: &str) -> Result<(), TlsError> {
        Ok(())
    }

    fn verify_certificate(
        &mut self,
        transcript: &Sha256,
        cert: CertificateRef,
    ) -> Result<(), TlsError> {
        // TLS 1.3 puts the end-entity certificate first. Any intermediates
        // behind it are irrelevant to a pin: we are not building a chain, we
        // are recognising a key.
        let der = match cert.entries.first() {
            Some(CertificateEntryRef::X509(der)) => *der,
            _ => return Err(self.fail(PinError::NoCertificate, TlsError::InvalidCertificate)),
        };
        match self.accept_certificate(der) {
            Ok(_) => {
                self.transcript = Some(transcript.clone());
                Ok(())
            }
            Err(e) => Err(self.fail(e, TlsError::InvalidCertificate)),
        }
    }

    fn verify_signature(&mut self, verify: CertificateVerifyRef) -> Result<(), TlsError> {
        if !self.cert_ok {
            return Err(self.fail(PinError::Order, TlsError::InvalidCertificate));
        }
        // Taken, not borrowed: one CertificateVerify per handshake. A second
        // one finds no transcript and fails.
        let transcript = match self.transcript.take() {
            Some(t) => t.finalize(),
            None => return Err(self.fail(PinError::Order, TlsError::InvalidCertificate)),
        };
        match self.check_signature(verify.signature_scheme, verify.signature, &transcript) {
            Ok(()) => {
                self.sig_ok = true;
                Ok(())
            }
            Err(e) => {
                let tls = match e {
                    PinError::Scheme => TlsError::InvalidSignatureScheme,
                    _ => TlsError::InvalidSignature,
                };
                Err(self.fail(e, tls))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Crypto provider
// ---------------------------------------------------------------------------

/// Hands embedded-tls its RNG and our verifier.
///
/// Passed to `open()` as `&mut`, so the verifier survives the handshake and
/// [`PinnedVerifier::completed`] can be checked afterwards.
pub struct PinnedProvider<R> {
    rng: R,
    verifier: PinnedVerifier,
}

impl<R: CryptoRngCore> PinnedProvider<R> {
    pub fn new(rng: R, verifier: PinnedVerifier) -> Self {
        Self { rng, verifier }
    }

    #[must_use]
    pub fn verified(&self) -> bool {
        self.verifier.completed()
    }

    #[must_use]
    pub fn last_error(&self) -> Option<PinError> {
        self.verifier.last_error
    }
}

impl<R: CryptoRngCore> CryptoProvider for PinnedProvider<R> {
    type CipherSuite = Aes128GcmSha256;
    /// Unused: we never present a client certificate. The board authenticates
    /// at the application layer instead — a signed challenge over its own
    /// Ed25519 identity key (`ClientMsg::Auth`), which predates TLS here and
    /// keeps working unchanged.
    type Signature = [u8; 64];

    fn rng(&mut self) -> impl CryptoRngCore {
        &mut self.rng
    }

    fn verifier(&mut self) -> Result<&mut impl TlsVerifier<Self::CipherSuite>, TlsError> {
        Ok(&mut self.verifier)
    }
}
