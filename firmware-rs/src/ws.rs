//! Minimal no_std WebSocket client over an embassy-net TcpSocket — just enough for
//! the device protocol (a text-JSON + binary-frame channel). Client frames are
//! masked per RFC 6455; control frames (ping/close) are handled inline.

use alloc::vec::Vec;
use embedded_io_async::{Read, Write};

pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_PING: u8 = 0x9;

/// Send the HTTP Upgrade and confirm a `101` response.
pub async fn handshake<S: Read + Write>(sock: &mut S, host: &str) -> Result<(), ()> {
    let mut req = Vec::new();
    req.extend_from_slice(b"GET /ws HTTP/1.1\r\nHost: ");
    req.extend_from_slice(host.as_bytes());
    req.extend_from_slice(
        b"\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
          Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
          Sec-WebSocket-Version: 13\r\n\r\n",
    );
    sock.write_all(&req).await.map_err(|_| ())?;
    // MUST flush before reading the response. Over TLS `write_all` only fills
    // the record buffer — without this the request never leaves the board and
    // both ends sit waiting for the other. Over a plain socket it's a no-op.
    sock.flush().await.map_err(|_| ())?;

    // Read one byte at a time up to the end of headers so we never consume into
    // the first WebSocket frame (the server sends Hello right after the 101).
    let mut buf = [0u8; 512];
    let mut got = 0;
    loop {
        let mut b = [0u8; 1];
        if sock.read(&mut b).await.map_err(|_| ())? == 0 {
            return Err(());
        }
        buf[got] = b[0];
        got += 1;
        if got >= 4 && &buf[got - 4..got] == b"\r\n\r\n" {
            return if is_switching_protocols(&buf[..got]) { Ok(()) } else { Err(()) };
        }
        if got == buf.len() {
            return Err(());
        }
    }
}

/// Did the server actually switch protocols? Only the STATUS LINE may answer
/// that. Searching the whole header block for "101" said yes to any response
/// carrying those three bytes anywhere — `Content-Length: 101`, a `Date` with
/// 10:1x, an ETag, a redirect body length — so an error page could be mistaken
/// for a live socket, and every "frame" parsed afterwards would be garbage from
/// an HTML body.
fn is_switching_protocols(head: &[u8]) -> bool {
    let end = head.windows(2).position(|w| w == b"\r\n").unwrap_or(head.len());
    let status = &head[..end];
    status.starts_with(b"HTTP/1.1 101") || status.starts_with(b"HTTP/1.0 101")
}

/// Frame reader whose ENTIRE parse state lives on `self`, not in the future.
/// The device loop races `read` against the outbox in a `select`; when the
/// outbox wins, the read future is dropped mid-frame. A plain `async fn` loses
/// whatever header/payload bytes it had already consumed and the stream
/// desyncs — every later frame parses as garbage, and the failure is silent
/// (bogus opcodes are ignored) until the socket times out. Here a dropped
/// read resumes exactly where the stream left off on the next call.
///
/// `read` never writes: a cancelled inline pong would tear the OUTGOING
/// stream the same way. Pings are returned to the caller (`OP_PING`), whose
/// post-select writes can't be cancelled.
pub struct FrameReader {
    /// [0..2 base header][0/2/8 extended length][4 mask if masked].
    hdr: [u8; 14],
    hdr_got: usize,
    hdr_need: usize,
    payload_got: usize,
    payload_need: Option<usize>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self { hdr: [0; 14], hdr_got: 0, hdr_need: 2, payload_got: 0, payload_need: None }
    }

    fn reset(&mut self) {
        self.hdr_got = 0;
        self.hdr_need = 2;
        self.payload_got = 0;
        self.payload_need = None;
    }

    /// Read the next data frame (text/binary/ping) into `buf`, returning
    /// `(opcode, len)`. Pong is swallowed; close/error returns `Err`.
    /// Cancellation-safe: progress is recorded synchronously after every
    /// completed socket read, so dropping the future never loses bytes.
    pub async fn read<S: Read>(&mut self, sock: &mut S, buf: &mut [u8]) -> Result<(u8, usize), ()> {
        loop {
            while self.payload_need.is_none() {
                if self.hdr_got < self.hdr_need {
                    let n = sock
                        .read(&mut self.hdr[self.hdr_got..self.hdr_need])
                        .await
                        .map_err(|_| ())?;
                    if n == 0 {
                        return Err(());
                    }
                    self.hdr_got += n;
                    continue;
                }
                // Base header complete: grow to cover extended length + mask,
                // or parse the payload length once everything is in hand.
                let masked = self.hdr[1] & 0x80 != 0;
                let l7 = (self.hdr[1] & 0x7F) as usize;
                let ext = match l7 {
                    126 => 2,
                    127 => 8,
                    _ => 0,
                };
                let full = 2 + ext + if masked { 4 } else { 0 };
                if self.hdr_need < full {
                    self.hdr_need = full;
                    continue;
                }
                let len = match l7 {
                    126 => u16::from_be_bytes([self.hdr[2], self.hdr[3]]) as usize,
                    127 => u64::from_be_bytes(self.hdr[2..10].try_into().unwrap()) as usize,
                    _ => l7,
                };
                if len > buf.len() {
                    return Err(()); // frame larger than our buffer
                }
                self.payload_need = Some(len);
            }
            let len = self.payload_need.unwrap_or(0);
            while self.payload_got < len {
                let n = sock.read(&mut buf[self.payload_got..len]).await.map_err(|_| ())?;
                if n == 0 {
                    return Err(());
                }
                self.payload_got += n;
            }
            // Frame complete — unmask, reset, dispatch.
            let opcode = self.hdr[0] & 0x0F;
            let masked = self.hdr[1] & 0x80 != 0;
            if masked {
                let ext = match (self.hdr[1] & 0x7F) as usize {
                    126 => 2,
                    127 => 8,
                    _ => 0,
                };
                let mask = [
                    self.hdr[2 + ext],
                    self.hdr[3 + ext],
                    self.hdr[4 + ext],
                    self.hdr[5 + ext],
                ];
                for i in 0..len {
                    buf[i] ^= mask[i % 4];
                }
            }
            self.reset();
            match opcode {
                0xA => {}                // pong, ignore
                0x8 => return Err(()),   // close
                _ => return Ok((opcode, len)),
            }
        }
    }
}

/// Answer a ping (the caller passes the ping's payload back).
pub async fn write_pong<S: Write>(sock: &mut S, payload: &[u8]) -> Result<(), ()> {
    write_frame(sock, 0xA, &payload[..payload.len().min(125)]).await
}

pub async fn write_text<S: Write>(sock: &mut S, data: &[u8]) -> Result<(), ()> {
    write_frame(sock, OP_TEXT, data).await
}

async fn write_frame<S: Write>(sock: &mut S, opcode: u8, data: &[u8]) -> Result<(), ()> {
    let mask = [0x21u8, 0x43, 0x65, 0x87];
    let mut frame = Vec::with_capacity(data.len() + 14);
    frame.push(0x80 | opcode);
    let len = data.len();
    if len < 126 {
        frame.push(0x80 | len as u8);
    } else if len < 65536 {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    for (i, b) in data.iter().enumerate() {
        frame.push(b ^ mask[i % 4]);
    }
    sock.write_all(&frame).await.map_err(|_| ())?;
    // One websocket frame, one flush: the TLS record layer buffers writes and
    // only emits a record when told to, so a frame left unflushed is a frame
    // the server never sees. No-op on a plain socket.
    sock.flush().await.map_err(|_| ())
}
