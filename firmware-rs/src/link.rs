//! The device link: the TCP socket to the server, wrapped in pinned TLS.
//!
//! `Link` is what the websocket code in `ws.rs` reads and writes. It is the
//! same type either way; the `tls` cargo feature decides whether bytes pass
//! through [`embedded_tls`] on the way. There is deliberately NO plaintext
//! fallback at runtime: port 7878 still answers plain `ws://`, so "try TLS,
//! fall back on failure" would be a downgrade an on-path attacker triggers by
//! breaking one handshake. A TLS build requires TLS; a failure is just another
//! connection failure and the outer loop retries with its existing backoff.
//!
//! # Cancellation, and why the socket is split
//!
//! The device loop races the socket read against the outbox, and whichever
//! loses is dropped mid-flight. `TcpSocket::read` survives that — it consumes
//! nothing until it returns. `TlsConnection::read` does NOT:
//!
//! * `record_reader::next_record_header` accumulates the 5-byte record header
//!   in a stack local. Dropped after 3 of 5 bytes, those 3 bytes are gone and
//!   every later record is misframed.
//! * `record_reader::advance` keeps its byte count on the `RecordReader`, but
//!   the `RecordHeader` it is filling against is a local. Dropped mid-body,
//!   the body bytes are kept and the header is lost, so the next call reads
//!   five body bytes and calls them a header.
//!
//! TLS authenticates every record, so the failure is loud (a MAC failure and a
//! reconnect) rather than silent — but it would fire whenever a knob press
//! landed between the TCP segments of a record, which is often.
//!
//! So the TLS read future is never cancelled. Instead the loop waits on
//! [`Halves::wait_readable`] — which consumes nothing and is safe to drop —
//! and only then commits to a read. That needs the socket's read half to stay
//! reachable while `TlsConnection` owns the socket, which is what [`Halves`]
//! is: `TcpSocket::split` into a reader and a writer, each behind its own
//! `RefCell`. [`SockIo`] borrows the reader for reads and the writer for
//! writes, so the two never contend, and the loop can take a shared borrow of
//! the reader to await readability while no read is in flight.
//!
//! `ws::FrameReader` keeps its parse state on `self` regardless. That
//! invariant is now belt AND braces: nothing cancels it, and if anything ever
//! does it still resumes mid-frame.

use core::cell::RefCell;
use embassy_net::tcp::{TcpReader, TcpSocket, TcpWriter};
use embedded_io_async::{ErrorType, Read, Write};

#[cfg(feature = "tls")]
use crate::config;
#[cfg(feature = "tls")]
use crate::tls::{pin_from_hex, PinnedProvider, PinnedVerifier};
#[cfg(feature = "tls")]
use alloc::vec::Vec;
#[cfg(feature = "tls")]
use embedded_tls::{Aes128GcmSha256, FlushPolicy, TlsConfig, TlsConnection, TlsContext};

/// The server caps its TLS records at 4096 bytes *including* the 5-byte
/// header (`rustls` subtracts `PACKET_OVERHEAD` from `max_fragment_size`), so
/// a record body is at most 4108 bytes on the wire and 4091 bytes of
/// plaintext. Measured, not assumed: `serve::tests::
/// outgoing_records_respect_the_documented_cap` on the server asserts the
/// largest record is exactly 4113 bytes and fails if the cap moves either way.
///
/// 4096 + `TLS_RECORD_OVERHEAD` gives the read buffer room for the body plus
/// embedded-tls's framing slack.
#[cfg(feature = "tls")]
pub const TLS_READ_BUF: usize = 4096 + embedded_tls::TLS_RECORD_OVERHEAD;

/// Outgoing records. Only the client's own traffic goes through here — a
/// ClientHello (~250 bytes) and websocket frames carrying small JSON messages
/// — and anything longer is simply split across records, so this is a
/// throughput knob, not a limit.
#[cfg(feature = "tls")]
pub const TLS_WRITE_BUF: usize = 1024;

/// Decrypted bytes waiting for `ws::FrameReader`.
///
/// Sized to hold one record's plaintext whole (4091 bytes), which is what
/// makes "this buffer is empty" mean "embedded-tls is holding nothing either".
/// The loop leans on that: it is only safe to park on socket readability when
/// no decrypted bytes are stranded inside the TLS layer, or a frame that has
/// already arrived would sit undelivered until the server's next 20s ping.
/// [`Link::idle`] carries a second, cheaper check in case the server's cap
/// ever grows past this.
#[cfg(feature = "tls")]
pub const TLS_PLAIN_BUF: usize = 4096;

/// The compiled-in pins, decoded as `const` items so `pin_from_hex` runs
/// during compilation.
///
/// This is the difference between a typo in `config.rs` being a build error
/// and being a board that boots, panics in the connect loop, reboots, and
/// burns an OTA revert strike every time. `pin_from_hex` asserts length, hex,
/// and not-all-zero; in const position those asserts fail the build.
///
/// They are the DEFAULT, not the only possibility: a board provisioned with a
/// pin of its own (a self-hoster's) uses that instead — see [`Link::open`].
#[cfg(feature = "tls")]
const PIN_CURRENT: [u8; 32] = pin_from_hex(config::SERVER_PIN);
#[cfg(feature = "tls")]
const PIN_NEXT: Option<[u8; 32]> = match config::SERVER_PIN_NEXT {
    Some(next) => Some(pin_from_hex(next)),
    None => None,
};

/// The three TLS buffers, allocated ONCE for the life of the board.
///
/// embedded-tls takes caller-provided buffers and never allocates. Re-taking
/// ~9KB from the heap on every reconnect — and the Railway proxy recycles the
/// socket every 5-15 minutes — would carve the heap up around the long-lived
/// frame cache. So they are minted before the connect loop and reborrowed by
/// each session.
#[cfg(feature = "tls")]
pub struct TlsBuffers {
    read: Vec<u8>,
    write: Vec<u8>,
    plain: Vec<u8>,
}

#[cfg(feature = "tls")]
impl TlsBuffers {
    #[must_use]
    pub fn new() -> Self {
        Self {
            read: alloc::vec![0u8; TLS_READ_BUF],
            write: alloc::vec![0u8; TLS_WRITE_BUF],
            plain: alloc::vec![0u8; TLS_PLAIN_BUF],
        }
    }

    /// Total heap held for TLS, for the boot log.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.read.len() + self.write.len() + self.plain.len()
    }
}

/// Nothing to allocate without TLS; the type still exists so `main.rs` has one
/// shape.
#[cfg(not(feature = "tls"))]
pub struct TlsBuffers;

#[cfg(not(feature = "tls"))]
impl TlsBuffers {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
    #[must_use]
    pub fn bytes(&self) -> usize {
        0
    }
}

impl Default for TlsBuffers {
    fn default() -> Self {
        Self::new()
    }
}

/// A connected socket split into halves the rest of this module can hold
/// independently.
///
/// The split is what lets the device loop await readability on the read half
/// while `TlsConnection` owns the socket. Each half sits behind its own
/// `RefCell` rather than one shared cell so that a write can never collide
/// with the loop's readability wait — and if the read discipline is ever
/// broken, `RefCell` panics loudly instead of quietly aliasing.
pub struct Halves<'s> {
    rx: RefCell<TcpReader<'s>>,
    tx: RefCell<TcpWriter<'s>>,
}

impl<'s> Halves<'s> {
    pub fn new(sock: &'s mut TcpSocket<'_>) -> Self {
        let (rx, tx) = sock.split();
        Self {
            rx: RefCell::new(rx),
            tx: RefCell::new(tx),
        }
    }

    /// Wait until the socket has bytes to read, without consuming any.
    ///
    /// Cancellation-safe by construction: it reads nothing, so dropping it
    /// loses nothing. This is the ONLY thing the device loop is allowed to
    /// race against the outbox.
    pub async fn wait_readable(&self) {
        let rx = self.rx.borrow();
        rx.wait_read_ready().await;
    }
}

/// The `embedded_io_async` face of a split socket: reads take the reader half,
/// writes take the writer half.
#[derive(Clone, Copy)]
pub struct SockIo<'a, 's> {
    rx: &'a RefCell<TcpReader<'s>>,
    tx: &'a RefCell<TcpWriter<'s>>,
}

impl ErrorType for SockIo<'_, '_> {
    type Error = embassy_net::tcp::Error;
}

impl Read for SockIo<'_, '_> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let mut rx = self.rx.borrow_mut();
        rx.read(buf).await
    }
}

impl Write for SockIo<'_, '_> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let mut tx = self.tx.borrow_mut();
        tx.write(buf).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        // Deliberately not `TcpWriter::flush`, which waits for the peer to ACK.
        // Handing bytes to smoltcp's transmit queue is what "sent" means here;
        // waiting for an ACK would put a round trip inside every websocket
        // write. The TLS layer runs `FlushPolicy::Relaxed` for the same reason.
        Ok(())
    }
}

/// The hardware RNG, wearing rand_core 0.6's `CryptoRng` marker.
///
/// esp-hal implements `RngCore` for `Rng` but not `CryptoRng`, and
/// embedded-tls needs both. The marker is honest here: `Rng` is a true random
/// source whenever the RF subsystem is feeding it noise, and this only ever
/// runs from the device task, which exists solely because Wi-Fi is up. (This
/// is the same rule `main.rs` follows when it hands the SAR ADC back before
/// `wifi::new` — from that point on the radio itself is the entropy source.)
/// The bytes drawn here are the TLS 1.3 client random and the ephemeral
/// P-256 key, both of which need real entropy.
#[cfg(feature = "tls")]
#[derive(Clone, Copy, Default)]
pub struct HwRng(esp_hal::rng::Rng);

#[cfg(feature = "tls")]
impl rand_core::RngCore for HwRng {
    fn next_u32(&mut self) -> u32 {
        self.0.random()
    }

    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0u8; 8];
        self.0.read(&mut bytes);
        u64::from_le_bytes(bytes)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.0.read(dest);
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.0.read(dest);
        Ok(())
    }
}

#[cfg(feature = "tls")]
impl rand_core::CryptoRng for HwRng {}

/// What went wrong bringing a link up, in a form worth printing to serial.
/// (Nothing constructs these in a `--no-default-features` build — there is no
/// handshake to fail.)
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(feature = "tls"), allow(dead_code))]
pub enum LinkError {
    /// The TLS handshake failed. Carries the pin/signature diagnosis when the
    /// verifier is the one that rejected it.
    Handshake(&'static str),
    /// The handshake "succeeded" but the verifier never ran both halves.
    Unverified,
}

impl LinkError {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkError::Handshake(why) => why,
            LinkError::Unverified => "server skipped certificate verification",
        }
    }
}

/// Why the handshake failed, for the serial log, when the failure was NOT our
/// verifier rejecting the server (that case reports the pin diagnosis instead).
///
/// `InsufficientSpace` is spelled out because it is the one failure whose cause
/// is entirely on our side of the wire: it means a record did not fit
/// [`TLS_READ_BUF`], i.e. the server raised its fragment cap past what this
/// firmware was built for. Without a name for it that shows up on a board as
/// "the display stopped updating".
#[cfg(feature = "tls")]
fn why(e: embedded_tls::TlsError) -> &'static str {
    use embedded_tls::TlsError as E;
    match e {
        E::ConnectionClosed => "server closed the connection during the handshake",
        E::HandshakeAborted(..) => "server aborted the handshake with an alert",
        E::AbortHandshake(..) => "we aborted the handshake with an alert",
        E::Io(_) | E::IoError => "socket error during the handshake",
        E::InsufficientSpace => "a TLS record was larger than the read buffer",
        E::InvalidCipherSuite => "no cipher suite in common with the server",
        E::InvalidSignatureScheme => "server signed with an algorithm we do not accept",
        E::InvalidSignature => "handshake signature did not verify",
        E::InvalidCertificate => "server certificate rejected",
        E::InvalidKeyShare => "server key share is unusable",
        _ => "tls handshake failed",
    }
}

// ---------------------------------------------------------------------------
// TLS link
// ---------------------------------------------------------------------------

#[cfg(feature = "tls")]
pub struct Link<'a, 's> {
    tls: TlsConnection<'a, SockIo<'a, 's>, Aes128GcmSha256>,
    /// Decrypted bytes not yet handed to `ws::FrameReader`.
    plain: &'a mut [u8],
    head: usize,
    tail: usize,
    /// The last refill filled `plain` exactly, so embedded-tls may still be
    /// holding decrypted bytes. Cannot happen while the server honours its
    /// 4091-byte plaintext cap; if it ever does, this keeps the link live (the
    /// loop stops parking on socket readability) at the cost of the outbox
    /// waiting for the current frame.
    brimmed: bool,
}

#[cfg(feature = "tls")]
impl<'a, 's> Link<'a, 's> {
    /// Bring up TLS 1.3 with pinned server verification.
    ///
    /// `server_name` is sent as SNI so the ClientHello looks like any other —
    /// it plays no part in verification, which is entirely the pin.
    ///
    /// # Which pin
    ///
    /// `provisioned_pin` is the pin written over BLE alongside the host and
    /// port (`creds.rs`), and when it is present it is the ONLY pin trusted.
    /// It does not join the compiled-in pair, it replaces it: someone running
    /// their own server has pointed this board at their own certificate, and a
    /// board that also kept trusting the hosted server's key would accept that
    /// server — or anyone holding its certificate — on a connection its owner
    /// believes only they can answer. Narrowing to one pin is the whole point.
    ///
    /// With no provisioned pin the board is a hosted-server board and uses the
    /// compiled-in current + successor pair exactly as before, so the staged
    /// key rotation in docs/cli.md is untouched.
    pub async fn open(
        halves: &'a Halves<'s>,
        bufs: &'a mut TlsBuffers,
        server_name: &str,
        provisioned_pin: Option<[u8; 32]>,
    ) -> Result<Self, LinkError> {
        let io = SockIo {
            rx: &halves.rx,
            tx: &halves.tx,
        };
        let mut tls = TlsConnection::new(io, &mut bufs.read[..], &mut bufs.write[..]);
        // Relaxed: close the record and hand it to smoltcp. Strict (the
        // default) would block each write on a TCP ACK.
        tls.set_flush_policy(FlushPolicy::Relaxed);

        let config = TlsConfig::new().with_server_name(server_name);
        // No successor slot for a provisioned pin: rotation for a self-hosted
        // server is re-provisioning it, not a staged fleet rollout.
        let verifier = match provisioned_pin {
            Some(pin) => PinnedVerifier::new(pin, None),
            None => PinnedVerifier::new(PIN_CURRENT, PIN_NEXT),
        };
        let mut provider = PinnedProvider::new(HwRng::default(), verifier);

        let outcome = tls.open(TlsContext::new(&config, &mut provider)).await;
        if let Err(e) = outcome {
            // Our verifier's own diagnosis beats embedded-tls's error, which
            // flattens every rejection into InvalidCertificate — "the pin
            // didn't match" and "the signature didn't verify" are very
            // different problems to be staring at over a serial cable.
            return Err(LinkError::Handshake(
                provider.last_error().map_or_else(|| why(e), crate::tls::PinError::as_str),
            ));
        }
        // `open` returning Ok is NOT proof the server was verified: embedded-tls
        // runs the verifier only for the handshake messages the server chose to
        // send, and Finished alone is satisfiable by anyone who completed the
        // ECDHE. Demand that both halves actually ran.
        if !provider.verified() {
            return Err(LinkError::Unverified);
        }

        Ok(Self {
            tls,
            plain: &mut bufs.plain[..],
            head: 0,
            tail: 0,
            brimmed: false,
        })
    }

    /// Is it safe to park on socket readability?
    ///
    /// True when no decrypted plaintext is stranded anywhere: not in our
    /// staging buffer, and (given the server's record cap) not inside
    /// embedded-tls either. The device loop only races the outbox when this
    /// holds, so it never cancels a TLS read.
    #[must_use]
    pub fn idle(&self) -> bool {
        self.head == self.tail && !self.brimmed
    }
}

#[cfg(feature = "tls")]
impl ErrorType for Link<'_, '_> {
    type Error = embedded_tls::TlsError;
}

#[cfg(feature = "tls")]
impl Read for Link<'_, '_> {
    async fn read(&mut self, out: &mut [u8]) -> Result<usize, Self::Error> {
        if self.head == self.tail {
            self.head = 0;
            self.tail = 0;
            let n = self.tls.read(&mut self.plain[..]).await?;
            if n == 0 {
                return Ok(0);
            }
            self.tail = n;
            self.brimmed = n == self.plain.len();
        }
        let n = core::cmp::min(out.len(), self.tail - self.head);
        out[..n].copy_from_slice(&self.plain[self.head..self.head + n]);
        self.head += n;
        Ok(n)
    }
}

#[cfg(feature = "tls")]
impl Write for Link<'_, '_> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.tls.write(buf).await
    }

    /// Required, not optional: `TlsConnection::write` only buffers. Without
    /// this the websocket frame would sit in the record buffer and both ends
    /// would wait for each other.
    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.tls.flush().await
    }
}

// ---------------------------------------------------------------------------
// Plaintext link (`--no-default-features`, bring-up only)
// ---------------------------------------------------------------------------

#[cfg(not(feature = "tls"))]
pub struct Link<'a, 's> {
    io: SockIo<'a, 's>,
}

#[cfg(not(feature = "tls"))]
impl<'a, 's> Link<'a, 's> {
    pub async fn open(
        halves: &'a Halves<'s>,
        _bufs: &'a mut TlsBuffers,
        _server_name: &str,
        _provisioned_pin: Option<[u8; 32]>,
    ) -> Result<Self, LinkError> {
        Ok(Self {
            io: SockIo {
                rx: &halves.rx,
                tx: &halves.tx,
            },
        })
    }

    /// Nothing buffers above the socket, so parking on readability is always
    /// safe here.
    #[must_use]
    pub fn idle(&self) -> bool {
        true
    }
}

#[cfg(not(feature = "tls"))]
impl ErrorType for Link<'_, '_> {
    type Error = embassy_net::tcp::Error;
}

#[cfg(not(feature = "tls"))]
impl Read for Link<'_, '_> {
    async fn read(&mut self, out: &mut [u8]) -> Result<usize, Self::Error> {
        self.io.read(out).await
    }
}

#[cfg(not(feature = "tls"))]
impl Write for Link<'_, '_> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.io.write(buf).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.io.flush().await
    }
}
