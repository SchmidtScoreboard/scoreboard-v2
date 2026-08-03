//! The listener: one port, two protocols.
//!
//! Port 7878 cannot become TLS-only. Boards already in the field speak plain
//! `ws://` to it, and Railway's HTTPS edge terminates the app's TLS and
//! forwards *plaintext* HTTP to this same port. So instead of choosing, the
//! listener looks at the first byte of every accepted connection: `0x16` is the
//! TLS `handshake` ContentType (RFC 8446 §5.1), and no HTTP request line can
//! start with it (0x16 is SYN, not a token character). `0x16` gets a rustls
//! wrap; everything else goes to the plaintext path untouched.
//!
//! The look is a real `MSG_PEEK` — the byte stays in the socket buffer, so
//! whichever path takes over reads the stream from byte zero and neither hyper
//! nor rustls can tell it was inspected.
//!
//! Everything past the sniff — the hyper auto (h1+h2) builder, upgrades on, one
//! task per connection — is what `axum::serve` does, so the plaintext path
//! behaves exactly as it did before TLS existed.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// TLS record ContentType `handshake` — the first byte of a ClientHello.
const TLS_HANDSHAKE: u8 = 0x16;

/// How long a connection may stay silent before we give up on it.
///
/// A connection that opens and says nothing (or says `0x16` and then stalls
/// mid-handshake) must not pin a task forever. It can never wedge the *accept
/// loop* — every connection is sniffed inside its own task — but an unbounded
/// wait would still let a trickle of dead sockets accumulate.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// Waiting for the first byte to arrive.
    pub sniff: Duration,
    /// Completing the TLS handshake once we have seen `0x16`.
    pub handshake: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        // Generous on the sniff — some clients pre-open a socket speculatively
        // and send nothing for a while, and dropping those early would just
        // make them reconnect. Tighter on the handshake: the board's TLS 1.3
        // handshake is a second or two of software crypto, so 30s of silence
        // mid-flight means the peer is gone.
        Self { sniff: Duration::from_secs(60), handshake: Duration::from_secs(30) }
    }
}

/// Accept forever, sniffing each connection and serving `app` over whichever
/// transport it turns out to be. `tls` of `None` means no certificate is
/// configured: plaintext still works, TLS attempts are refused.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    tls: Option<Arc<rustls::ServerConfig>>,
    timeouts: Timeouts,
) {
    let acceptor = tls.map(TlsAcceptor::from);
    /// Did this accept fail because THIS connection died, rather than because
    /// the process is out of resources? Mirrors axum::serve's classification.
    fn is_connection_error(e: &std::io::Error) -> bool {
        use std::io::ErrorKind::*;
        matches!(e.kind(), ConnectionRefused | ConnectionAborted | ConnectionReset)
    }

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(conn) => conn,
            // Two very different failures wear the same Err. A per-connection
            // error (ECONNABORTED, ECONNRESET — the peer left between SYN and
            // accept) is transient and the next accept succeeds, so yield and
            // move on. RESOURCE exhaustion (EMFILE/ENFILE) is not: accept
            // fails instantly and forever until an fd frees, and yielding
            // spins a whole core, floods the log, and starves the very tasks
            // that would release those fds. Sleep on those — same distinction
            // axum::serve draws, for the same reason.
            Err(e) => {
                if is_connection_error(&e) {
                    tokio::task::yield_now().await;
                } else {
                    tracing::warn!("accept failed: {e}; backing off");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                continue;
            }
        };
        let app = app.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            // Sniffing happens HERE, inside the per-connection task, so a peer
            // that connects and then says nothing costs one idle task and
            // never a moment of the accept loop. MSG_PEEK leaves the byte in
            // the socket buffer: whoever serves the connection next reads it
            // from byte zero.
            let mut probe = [0u8; 1];
            let first = match tokio::time::timeout(timeouts.sniff, stream.peek(&mut probe)).await {
                Ok(Ok(0)) => return, // hung up before saying anything
                Ok(Ok(_)) => probe[0],
                Ok(Err(e)) => {
                    tracing::debug!("{peer}: sniff failed: {e}");
                    return;
                }
                Err(_) => {
                    tracing::debug!("{peer}: silent for {:?}, dropping", timeouts.sniff);
                    return;
                }
            };

            if first != TLS_HANDSHAKE {
                serve_conn(TokioIo::new(stream), app).await;
                return;
            }

            let Some(acceptor) = acceptor else {
                tracing::warn!(
                    "{peer}: offered TLS but no certificate is configured — refusing \
                     (set TLS_CERT_PEM/TLS_KEY_PEM)"
                );
                return;
            };
            match tokio::time::timeout(timeouts.handshake, acceptor.accept(stream)).await {
                Ok(Ok(stream)) => serve_conn(TokioIo::new(stream), app).await,
                Ok(Err(e)) => tracing::debug!("{peer}: TLS handshake failed: {e}"),
                Err(_) => tracing::debug!("{peer}: TLS handshake stalled, dropping"),
            }
        });
    }
}

/// Hand one established connection — plaintext or TLS, the code is identical —
/// to hyper. `serve_connection_with_upgrades` is what makes websockets work.
async fn serve_conn<I>(io: I, app: Router)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    // Errors here are almost always "the client vanished without finishing a
    // request", which axum::serve also swallows.
    // `.timer()` is load-bearing, not decoration: hyper's h1_header_read_timeout
    // (30s by default) is documented as requiring a Timer, and without one it
    // never fires. A connection that sends a single non-TLS byte and then goes
    // silent satisfies the sniff timeout instantly and would otherwise hold an
    // fd and a task forever, for the price of one write(2). (axum::serve sets
    // no timer either — this makes the sniffing path strictly better than what
    // it replaced, rather than merely equivalent.)
    let mut builder = Builder::new(TokioExecutor::new());
    // The timer lives on the per-protocol sub-builders; setting it on http1
    // is what activates hyper's 30s header-read timeout.
    // 10s, not hyper's default 30: a real client sends its headers in the
    // first packet or two, so anything slower is a stalled or hostile socket
    // squatting an fd. Also keeps the regression test below quick.
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(10));
    builder.http2().timer(TokioTimer::new());
    let _ = builder
        .serve_connection_with_upgrades(io, TowerToHyperService::new(app))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls;

    use tokio::net::TcpStream;

    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};

    use axum::routing::get;
    use futures_util::{SinkExt, StreamExt};
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};
    use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};

    /// Body big enough to force the fragmenter to do its job several times over.
    const BIG_LEN: usize = 100_000;

    fn test_router() -> Router {
        Router::new()
            .route("/hello", get(|| async { "hello from the board link" }))
            .route("/big", get(|| async { "x".repeat(BIG_LEN) }))
            .route(
                "/ws",
                get(|up: axum::extract::WebSocketUpgrade| async move {
                    up.on_upgrade(|mut sock| async move {
                        while let Some(Ok(msg)) = sock.recv().await {
                            if let axum::extract::ws::Message::Text(t) = msg {
                                let _ = sock
                                    .send(axum::extract::ws::Message::Text(format!("echo:{t}")))
                                    .await;
                            }
                        }
                    })
                }),
            )
    }

    /// Spin up the sniffing listener on an ephemeral port.
    async fn spawn(
        tls: Option<Arc<rustls::ServerConfig>>,
        timeouts: Timeouts,
    ) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, test_router(), tls, timeouts));
        addr
    }

    fn cert() -> tls::GeneratedCert {
        tls::generate_self_signed(&["localhost".to_string()]).unwrap()
    }

    // ---- a client that verifies the way the firmware will ----

    /// The board's verifier, in rustls terms. This is the design under test:
    ///
    /// * `verify_server_cert` = embedded-tls's `verify_certificate` — IDENTITY.
    ///   Hash the presented certificate's SPKI, accept it only if it is one of
    ///   the pins we carry.
    /// * `verify_tls13_signature` = embedded-tls's `verify_signature` —
    ///   POSSESSION. Check the CertificateVerify signature for real. Without
    ///   this, the pin check is defeated by replaying a captured certificate,
    ///   which is public.
    #[derive(Debug)]
    struct PinnedVerifier {
        pins: Vec<String>,
        provider: Arc<rustls::crypto::CryptoProvider>,
        /// Set when the possession half actually ran, so a test can prove it is
        /// not quietly skipped.
        signature_checked: Arc<std::sync::atomic::AtomicBool>,
    }

    impl PinnedVerifier {
        fn new(pins: Vec<String>) -> (Arc<Self>, Arc<std::sync::atomic::AtomicBool>) {
            let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let me = Arc::new(Self {
                pins,
                provider: Arc::new(rustls::crypto::ring::default_provider()),
                signature_checked: flag.clone(),
            });
            (me, flag)
        }
    }

    impl ServerCertVerifier for PinnedVerifier {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            let got = tls::spki_sha256_hex(end_entity).map_err(rustls::Error::General)?;
            if self.pins.iter().any(|p| *p == got) {
                Ok(ServerCertVerified::assertion())
            } else {
                Err(rustls::Error::General(format!("pin mismatch: {got}")))
            }
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            // TLS 1.3 only; reaching this would itself be the bug.
            Err(rustls::Error::General("TLS 1.2 is not offered".into()))
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            self.signature_checked
                .store(true, std::sync::atomic::Ordering::SeqCst);
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.provider
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    fn pinned_client(pins: Vec<String>) -> (Arc<rustls::ClientConfig>, Arc<std::sync::atomic::AtomicBool>) {
        let (verifier, flag) = PinnedVerifier::new(pins);
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        (Arc::new(config), flag)
    }

    /// Records every byte the server sends, so a test can walk the raw TLS
    /// record framing afterwards.
    #[derive(Debug)]
    struct Tee<S> {
        inner: S,
        seen: Arc<Mutex<Vec<u8>>>,
    }

    impl<S: AsyncRead + Unpin> AsyncRead for Tee<S> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let before = buf.filled().len();
            let r = Pin::new(&mut self.inner).poll_read(cx, buf);
            if let Poll::Ready(Ok(())) = &r {
                self.seen.lock().unwrap().extend_from_slice(&buf.filled()[before..]);
            }
            r
        }
    }

    impl<S: AsyncWrite + Unpin> AsyncWrite for Tee<S> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }
        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }
        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    /// Open a pinned TLS connection to `addr`, returning the stream and the
    /// tee of everything the server sent.
    async fn tls_connect(
        addr: std::net::SocketAddr,
        config: Arc<rustls::ClientConfig>,
    ) -> std::io::Result<(
        tokio_rustls::client::TlsStream<Tee<TcpStream>>,
        Arc<Mutex<Vec<u8>>>,
    )> {
        let tcp = TcpStream::connect(addr).await?;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let tee = Tee { inner: tcp, seen: seen.clone() };
        let name = ServerName::try_from("localhost").unwrap();
        let s = tokio_rustls::TlsConnector::from(config).connect(name, tee).await?;
        Ok((s, seen))
    }

    /// Minimal HTTP/1.1 GET over an established stream; returns the whole
    /// response (headers included) as bytes.
    async fn http_get<S: AsyncRead + AsyncWrite + Unpin>(
        stream: &mut S,
        path: &str,
    ) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).await.unwrap();
        out
    }

    /// A raw HTTP response with the one header that legitimately differs
    /// between two responses dropped, so "byte-for-byte" comparisons compare
    /// what they mean to (hyper stamps `date:` at second resolution, and two
    /// requests can straddle a tick).
    fn without_date(raw: &[u8]) -> Vec<u8> {
        String::from_utf8_lossy(raw)
            .split_inclusive("\r\n")
            .filter(|line| !line.to_ascii_lowercase().starts_with("date:"))
            .collect::<String>()
            .into_bytes()
    }

    // ---- guard rails ----

    /// Plaintext HTTP is untouched: same status, same headers, same body as a
    /// stock `axum::serve` on the same router.
    #[tokio::test]
    async fn plaintext_http_still_works() {
        let sniffing = spawn(None, Timeouts::default()).await;

        let reference = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a = l.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(l, test_router()).await.unwrap() });
            a
        };

        let mut via_sniff = TcpStream::connect(sniffing).await.unwrap();
        let mut via_axum = TcpStream::connect(reference).await.unwrap();
        let a = http_get(&mut via_sniff, "/hello").await;
        let b = http_get(&mut via_axum, "/hello").await;
        assert_eq!(
            without_date(&a),
            without_date(&b),
            "the sniffing listener answers byte-for-byte like axum::serve"
        );
        assert!(String::from_utf8_lossy(&a).contains("hello from the board link"));
        // The dropped header really was there, i.e. the comparison above is not
        // quietly comparing two stripped-to-nothing strings.
        assert!(String::from_utf8_lossy(&a).to_ascii_lowercase().contains("date:"));
    }

    /// Plaintext websockets — what every board in the field speaks today —
    /// still upgrade and echo.
    #[tokio::test]
    async fn plaintext_websocket_upgrade_still_works() {
        let addr = spawn(None, Timeouts::default()).await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        ws.send(tokio_tungstenite::tungstenite::Message::Text("hi".into()))
            .await
            .unwrap();
        let got = ws.next().await.unwrap().unwrap();
        assert_eq!(got.into_text().unwrap(), "echo:hi");
    }

    /// A pinned TLS client completes the handshake and gets the same bytes off
    /// the same routes — and the possession half of the verifier really ran.
    #[tokio::test]
    async fn tls_serves_the_same_routes_as_plaintext() {
        let gen = cert();
        let tls = tls::from_pem(&gen.cert_pem, &gen.key_pem).unwrap();
        assert_eq!(tls.pin, gen.pin);
        let addr = spawn(Some(tls.config), Timeouts::default()).await;

        let (client, signature_checked) = pinned_client(vec![gen.pin.clone()]);
        let (mut stream, _) = tls_connect(addr, client).await.unwrap();
        // TLS 1.3 on the wire, not a downgrade: embedded-tls speaks nothing else.
        assert_eq!(
            stream.get_ref().1.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_3)
        );
        let over_tls = http_get(&mut stream, "/hello").await;

        let mut plain = TcpStream::connect(addr).await.unwrap();
        let over_plain = http_get(&mut plain, "/hello").await;

        assert_eq!(
            without_date(&over_tls),
            without_date(&over_plain),
            "same router, same response, either transport"
        );
        assert!(
            signature_checked.load(std::sync::atomic::Ordering::SeqCst),
            "the CertificateVerify signature was actually verified — a pin check \
             alone would be defeated by replaying the (public) certificate"
        );
    }

    /// The board's real path: websocket over TLS.
    #[tokio::test]
    async fn tls_websocket_upgrade_works() {
        let gen = cert();
        let tls = tls::from_pem(&gen.cert_pem, &gen.key_pem).unwrap();
        let addr = spawn(Some(tls.config), Timeouts::default()).await;

        let (client, _) = pinned_client(vec![gen.pin.clone()]);
        let (stream, _) = tls_connect(addr, client).await.unwrap();
        let (mut ws, _) = tokio_tungstenite::client_async(format!("ws://localhost/ws"), stream)
            .await
            .unwrap();
        ws.send(tokio_tungstenite::tungstenite::Message::Text("board".into()))
            .await
            .unwrap();
        let got = ws.next().await.unwrap().unwrap();
        assert_eq!(got.into_text().unwrap(), "echo:board");
    }

    /// Two pins are supported from day one so a key rotation is a staged
    /// rollout: firmware carrying {current, successor} keeps working across the
    /// server-side swap, in both directions.
    #[tokio::test]
    async fn either_of_two_pins_is_accepted() {
        let current = cert();
        let successor = cert();
        let both = vec![current.pin.clone(), successor.pin.clone()];

        for gen in [&current, &successor] {
            let tls = tls::from_pem(&gen.cert_pem, &gen.key_pem).unwrap();
            let addr = spawn(Some(tls.config), Timeouts::default()).await;
            let (client, _) = pinned_client(both.clone());
            let (mut stream, _) = tls_connect(addr, client).await.unwrap();
            let body = http_get(&mut stream, "/hello").await;
            assert!(String::from_utf8_lossy(&body).contains("hello from the board link"));
        }
    }

    /// The pin is load-bearing: a client carrying the wrong one cannot connect,
    /// even though the server presents a perfectly well-formed certificate.
    #[tokio::test]
    async fn a_wrong_pin_fails_the_handshake() {
        let gen = cert();
        let tls = tls::from_pem(&gen.cert_pem, &gen.key_pem).unwrap();
        let addr = spawn(Some(tls.config), Timeouts::default()).await;

        let (client, _) = pinned_client(vec!["00".repeat(32)]);
        let err = tls_connect(addr, client).await.expect_err("handshake must fail");
        assert!(
            err.to_string().contains("pin mismatch"),
            "failed for the pin reason: {err}"
        );
    }

    /// No certificate configured: plaintext keeps working and a TLS attempt is
    /// refused rather than hanging or taking the process down.
    #[tokio::test]
    async fn tls_attempt_without_a_certificate_is_refused_and_plaintext_survives() {
        let addr = spawn(None, Timeouts::default()).await;

        let (client, _) = pinned_client(vec!["00".repeat(32)]);
        assert!(tls_connect(addr, client).await.is_err());

        let mut plain = TcpStream::connect(addr).await.unwrap();
        let body = http_get(&mut plain, "/hello").await;
        assert!(String::from_utf8_lossy(&body).contains("hello from the board link"));
    }

    /// A connection that sends ONE non-TLS byte and then goes silent used to
    /// be the hole in the timeout story: it satisfies the sniff timeout
    /// instantly, and hyper's header-read timeout is inert without a Timer, so
    /// the socket and its task were held forever for the cost of one write(2).
    /// The `.timer()` on the protocol builders is what closes it.
    #[tokio::test]
    async fn one_byte_then_silence_is_eventually_dropped() {
        use tokio::io::AsyncReadExt;
        let addr = spawn(None, Timeouts::default()).await;
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        // 'G' as in GET — routes to the plaintext path, then nothing follows.
        sock.write_all(b"G").await.unwrap();

        // hyper's header-read timeout is 30s by default and only runs when a
        // Timer is installed, so this both proves the timer is wired and bounds
        // how long a silent client can squat. A read of 0 / an error means the
        // server hung up on us.
        let closed = tokio::time::timeout(Duration::from_secs(25), async {
            let mut buf = [0u8; 1];
            loop {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => return true,
                    Ok(_) => continue,
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(closed, "a one-byte-then-silent connection must be timed out, not held forever");
    }

    /// A connection that sends one byte and then stalls must not wedge the
    /// accept loop, and must not be held forever either.
    #[tokio::test]
    async fn a_stalled_connection_does_not_wedge_the_accept_loop() {
        let gen = cert();
        let tls = tls::from_pem(&gen.cert_pem, &gen.key_pem).unwrap();
        let short = Timeouts {
            sniff: Duration::from_millis(200),
            handshake: Duration::from_millis(200),
        };
        let addr = spawn(Some(tls.config), short).await;

        // (a) says 0x16 and then nothing — a TLS handshake that never arrives.
        let mut stalled_tls = TcpStream::connect(addr).await.unwrap();
        stalled_tls.write_all(&[0x16]).await.unwrap();
        stalled_tls.flush().await.unwrap();
        // (b) says nothing at all.
        let silent = TcpStream::connect(addr).await.unwrap();

        // The loop is still accepting and answering, right now, with both of
        // those sockets still open.
        let serve_while_stalled = tokio::time::timeout(Duration::from_secs(5), async {
            let mut plain = TcpStream::connect(addr).await.unwrap();
            http_get(&mut plain, "/hello").await
        })
        .await
        .expect("the accept loop kept serving while two connections stalled");
        assert!(String::from_utf8_lossy(&serve_while_stalled).contains("hello from the board link"));

        // And both stalled sockets are eventually dropped by the server rather
        // than parked forever: read returns EOF once the timeout fires.
        for (name, mut sock) in [("tls", stalled_tls), ("silent", silent)] {
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; 1];
            let n = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf))
                .await
                .unwrap_or_else(|_| panic!("{name}: server never closed the stalled connection"))
                .unwrap_or(0);
            assert_eq!(n, 0, "{name}: server closed the stalled connection");
        }
    }

    /// The record cap is real on the wire. Walks the raw TLS framing the server
    /// produced for a 100 KB body and checks every record against the number
    /// documented in `tls.rs` — this is the value the firmware sizes its
    /// receive buffer against, so it is worth measuring rather than asserting.
    #[tokio::test]
    async fn outgoing_records_respect_the_documented_cap() {
        let gen = cert();
        let tls = tls::from_pem(&gen.cert_pem, &gen.key_pem).unwrap();
        let addr = spawn(Some(tls.config), Timeouts::default()).await;

        let (client, _) = pinned_client(vec![gen.pin.clone()]);
        let (mut stream, seen) = tls_connect(addr, client).await.unwrap();
        let body = http_get(&mut stream, "/big").await;
        assert!(
            String::from_utf8_lossy(&body).contains(&"x".repeat(1000)),
            "the big body really came through"
        );

        let raw = seen.lock().unwrap().clone();
        let mut idx = 0usize;
        let mut records = 0usize;
        let mut largest = 0usize;
        while idx + 5 <= raw.len() {
            let len = u16::from_be_bytes([raw[idx + 3], raw[idx + 4]]) as usize;
            let wire = 5 + len;
            if idx + wire > raw.len() {
                break; // trailing partial record
            }
            records += 1;
            largest = largest.max(wire);
            idx += wire;
        }

        assert!(records > 20, "a 100 KB body fragments into many records, saw {records}");
        // Exactly the cap, not merely under it: a 100 KB body fills records to
        // the brim, so this is the real worst case a board will ever see and
        // the number its receive buffer is sized against. Equality also means
        // the test fails if the cap is ever loosened OR tightened without the
        // firmware being told.
        assert_eq!(
            largest,
            tls::MAX_RECORD_WIRE,
            "largest TLS record on the wire was {largest} bytes; the documented cap the \
             firmware sizes its receive buffer against is {}",
            tls::MAX_RECORD_WIRE
        );
    }
}
