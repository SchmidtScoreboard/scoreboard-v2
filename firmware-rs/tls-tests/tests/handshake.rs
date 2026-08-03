//! End-to-end: the firmware's verifier completing a real TLS 1.3 handshake
//! against a real rustls server configured exactly like the deployed one.
//!
//! The unit tests in `pinning.rs` prove the pin arithmetic and the signature
//! check in isolation. They cannot prove the thing that actually breaks
//! first — that the verifier is wired into embedded-tls's state machine
//! correctly, that the transcript it clones is the one RFC 8446 says the
//! server signed, and that a rustls server with an Ed25519 certificate and a
//! 4096-byte fragment cap will negotiate something this client can finish.
//!
//! So: same rcgen recipe as `crates/server/src/tls.rs::generate_self_signed`,
//! same rustls builder (`ring` provider, TLS 1.3 only, `max_fragment_size =
//! Some(4096)`), same `PinnedVerifier` the board runs — compiled from
//! `../../src/tls.rs` — over a loopback socket.
//!
//! embedded-tls's blocking API is used rather than the async one purely to
//! keep a tokio runtime out of the test; the connection state machine,
//! `CryptoProvider` plumbing and verifier hooks are the same code.

use embedded_tls::blocking::TlsConnection;
use embedded_tls::{Aes128GcmSha256, TlsConfig, TlsContext};
use rcgen::PublicKeyData as _;
use sha2::{Digest, Sha256};
use embedded_io::Write as _;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use tls_tests::tls::{PinError, PinnedProvider, PinnedVerifier};

/// Mirrors `crates/server/src/tls.rs`: the board's read buffer is sized to it.
const MAX_FRAGMENT_SIZE: usize = 4096;

// ---------------------------------------------------------------------------
// Test plumbing
// ---------------------------------------------------------------------------

/// `std::net::TcpStream` wearing embedded-io's blocking traits.
struct StdSock(TcpStream);

#[derive(Debug)]
struct SockError(std::io::ErrorKind);

impl std::fmt::Display for SockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl std::error::Error for SockError {}

impl embedded_io::Error for SockError {
    fn kind(&self) -> embedded_io::ErrorKind {
        embedded_io::ErrorKind::Other
    }
}

impl embedded_io::ErrorType for StdSock {
    type Error = SockError;
}

impl embedded_io::Read for StdSock {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.0.read(buf).map_err(|e| SockError(e.kind()))
    }
}

impl embedded_io::Write for StdSock {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.0.write(buf).map_err(|e| SockError(e.kind()))
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.0.flush().map_err(|e| SockError(e.kind()))
    }
}

/// `ring`'s system RNG, which is what the board's hardware RNG stands in for.
struct TestRng(ring::rand::SystemRandom);

impl TestRng {
    fn new() -> Self {
        Self(ring::rand::SystemRandom::new())
    }
}

impl rand_core::RngCore for TestRng {
    fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.fill_bytes(&mut b);
        u32::from_le_bytes(b)
    }
    fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill_bytes(&mut b);
        u64::from_le_bytes(b)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        use ring::rand::SecureRandom as _;
        self.0.fill(dest).expect("system rng");
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl rand_core::CryptoRng for TestRng {}

struct Server {
    port: u16,
    pin: [u8; 32],
    handle: std::thread::JoinHandle<Option<Vec<u8>>>,
}

/// A rustls TLS 1.3 server holding an Ed25519 identity, built the way
/// `crates/server/src/tls.rs::from_pem` builds the real one. It echoes back
/// whatever the client sends, uppercased, and returns what it received.
fn start_server(reply_bytes: usize) -> Server {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    let pin: [u8; 32] = Sha256::digest(key.subject_public_key_info()).into();

    let mut params = rcgen::CertificateParams::new(vec!["board.example".to_string()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "scoreboard board link");
    let cert = params.self_signed(&key).unwrap();

    let certs = vec![cert.der().clone()];
    let private = rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into());

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, private)
        .unwrap();
    config.max_fragment_size = Some(MAX_FRAGMENT_SIZE);
    let config = Arc::new(config);

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().ok()?;
        // A client that walks away mid-handshake must not wedge the suite.
        sock.set_read_timeout(Some(std::time::Duration::from_secs(10))).ok()?;
        let mut conn = rustls::ServerConnection::new(config).ok()?;
        // Complete the handshake; a client that rejects our certificate makes
        // this fail, which is exactly what the negative test wants to see.
        conn.complete_io(&mut sock).ok()?;
        assert_eq!(conn.protocol_version(), Some(rustls::ProtocolVersion::TLSv1_3));

        let mut tls = rustls::Stream::new(&mut conn, &mut sock);
        let mut got = vec![0u8; 64];
        let n = tls.read(&mut got).ok()?;
        got.truncate(n);

        // Answer with `reply_bytes` of deterministic data, so the client can
        // check that a payload spanning several capped records reassembles.
        let reply: Vec<u8> = (0..reply_bytes).map(|i| (i % 251) as u8).collect();
        tls.write_all(&reply).ok()?;
        tls.flush().ok()?;
        Some(got)
    });

    Server { port, pin, handle }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn a_pinned_handshake_completes_against_a_real_rustls_server() {
    let server = start_server(100 * 1024);

    let sock = StdSock(TcpStream::connect(("127.0.0.1", server.port)).unwrap());
    // Same sizes the firmware allocates (see link.rs).
    let mut read_buf = vec![0u8; MAX_FRAGMENT_SIZE + embedded_tls::TLS_RECORD_OVERHEAD];
    let mut write_buf = vec![0u8; 1024];
    let mut tls: TlsConnection<'_, StdSock, Aes128GcmSha256> =
        TlsConnection::new(sock, &mut read_buf, &mut write_buf);

    let config = TlsConfig::new().with_server_name("board.example");
    let mut provider = PinnedProvider::new(
        TestRng::new(),
        PinnedVerifier::new(server.pin, None),
    );

    tls.open(TlsContext::new(&config, &mut provider))
        .expect("handshake with the pinned server");

    // Both halves ran. Identity AND possession — this is the assertion that
    // would fail if `verify_signature` were ever quietly stubbed out, because
    // a stub sets no flag.
    assert!(
        provider.verified(),
        "the handshake completed without the verifier running both halves"
    );
    assert_eq!(provider.last_error(), None);

    tls.write_all(b"hello from the board").unwrap();
    tls.flush().unwrap();

    // Read the whole reply. The server caps plaintext at 4091 bytes per
    // record, so 100KB arrives as ~25 records through a 4224-byte buffer —
    // the firmware's exact buffer arithmetic, exercised.
    let mut got = Vec::new();
    let mut chunk = vec![0u8; MAX_FRAGMENT_SIZE];
    while got.len() < 100 * 1024 {
        let n = tls.read(&mut chunk).unwrap();
        assert_ne!(n, 0, "server closed early");
        got.extend_from_slice(&chunk[..n]);
    }
    let expected: Vec<u8> = (0..100 * 1024).map(|i| (i % 251) as u8).collect();
    assert_eq!(got, expected);

    assert_eq!(
        server.handle.join().unwrap().as_deref(),
        Some(&b"hello from the board"[..])
    );
}

#[test]
fn one_record_never_exceeds_the_buffer_the_firmware_allocates() {
    // The read buffer is sized off the server's documented cap. If the server
    // ever raised `max_fragment_size` past it, embedded-tls would fail with
    // InsufficientSpace rather than anything diagnosable — so prove the
    // arithmetic holds against a real 100KB response before trusting it on a
    // board where the failure looks like "the display stopped updating".
    let server = start_server(100 * 1024);
    let sock = StdSock(TcpStream::connect(("127.0.0.1", server.port)).unwrap());

    // Exactly the firmware's TLS_READ_BUF, no slack added.
    let mut read_buf = vec![0u8; MAX_FRAGMENT_SIZE + embedded_tls::TLS_RECORD_OVERHEAD];
    let mut write_buf = vec![0u8; 1024];
    let mut tls: TlsConnection<'_, StdSock, Aes128GcmSha256> =
        TlsConnection::new(sock, &mut read_buf, &mut write_buf);
    let config = TlsConfig::new().with_server_name("board.example");
    let mut provider =
        PinnedProvider::new(TestRng::new(), PinnedVerifier::new(server.pin, None));
    tls.open(TlsContext::new(&config, &mut provider)).unwrap();

    tls.write_all(b"go").unwrap();
    tls.flush().unwrap();

    let mut total = 0usize;
    let mut chunk = vec![0u8; 8192];
    while total < 100 * 1024 {
        let n = tls.read(&mut chunk).unwrap();
        assert_ne!(n, 0);
        // One `read` yields at most one record's plaintext, and the firmware's
        // 4096-byte staging buffer is sized on the promise that it is never
        // more than 4091.
        assert!(
            n <= MAX_FRAGMENT_SIZE - 5,
            "a record carried {n} bytes of plaintext; the board stages 4096"
        );
        total += n;
    }
    let _ = server.handle.join();
}

#[test]
fn the_wrong_pin_fails_the_handshake_and_there_is_nothing_to_fall_back_to() {
    let server = start_server(16);

    let sock = StdSock(TcpStream::connect(("127.0.0.1", server.port)).unwrap());
    let mut read_buf = vec![0u8; MAX_FRAGMENT_SIZE + embedded_tls::TLS_RECORD_OVERHEAD];
    let mut write_buf = vec![0u8; 1024];
    let mut tls: TlsConnection<'_, StdSock, Aes128GcmSha256> =
        TlsConnection::new(sock, &mut read_buf, &mut write_buf);

    // A pin for some other key entirely.
    let mut wrong = server.pin;
    wrong[0] ^= 0xFF;

    let config = TlsConfig::new().with_server_name("board.example");
    let mut provider =
        PinnedProvider::new(TestRng::new(), PinnedVerifier::new(wrong, None));

    let result = tls.open(TlsContext::new(&config, &mut provider));
    assert!(result.is_err(), "a mismatched pin must abort the handshake");
    assert!(!provider.verified());
    assert_eq!(
        provider.last_error(),
        Some(PinError::NoPinMatch),
        "and the board should be able to say WHY on the serial log"
    );

    // What the board does next is drop the socket and retry the whole
    // connection — it never reopens it in plaintext. Dropping here is that,
    // and it lets the server thread observe the hang-up.
    drop(tls);

    // The server never got a request: the client refused it before saying
    // anything an application would care about.
    assert_eq!(server.handle.join().unwrap(), None);
}

#[test]
fn a_staged_successor_pin_connects_to_the_server_that_already_rotated() {
    // The rollout in the middle: firmware trusting {old, new} meets a server
    // that has already swapped to the new key.
    let server = start_server(16);
    let mut old_pin = server.pin;
    old_pin[7] ^= 0x5A;

    let sock = StdSock(TcpStream::connect(("127.0.0.1", server.port)).unwrap());
    let mut read_buf = vec![0u8; MAX_FRAGMENT_SIZE + embedded_tls::TLS_RECORD_OVERHEAD];
    let mut write_buf = vec![0u8; 1024];
    let mut tls: TlsConnection<'_, StdSock, Aes128GcmSha256> =
        TlsConnection::new(sock, &mut read_buf, &mut write_buf);

    let config = TlsConfig::new().with_server_name("board.example");
    let mut provider = PinnedProvider::new(
        TestRng::new(),
        PinnedVerifier::new(old_pin, Some(server.pin)),
    );

    tls.open(TlsContext::new(&config, &mut provider))
        .expect("the successor pin should carry the connection");
    assert!(provider.verified());

    tls.write_all(b"rotated").unwrap();
    tls.flush().unwrap();
    assert_eq!(
        server.handle.join().unwrap().as_deref(),
        Some(&b"rotated"[..])
    );
}
