//! Minimal no_std WebSocket client over an embassy-net TcpSocket — just enough for
//! the device protocol (a text-JSON + binary-frame channel). Client frames are
//! masked per RFC 6455; control frames (ping/close) are handled inline.

use alloc::vec::Vec;
use embedded_io_async::{Read, Write};

pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;

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
            return if buf[..got].windows(3).any(|w| w == b"101") { Ok(()) } else { Err(()) };
        }
        if got == buf.len() {
            return Err(());
        }
    }
}

async fn read_exact<S: Read>(sock: &mut S, buf: &mut [u8]) -> Result<(), ()> {
    let mut got = 0;
    while got < buf.len() {
        let n = sock.read(&mut buf[got..]).await.map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        got += n;
    }
    Ok(())
}

/// Read the next data frame (text/binary) into `buf`, returning `(opcode, len)`.
/// Ping is answered with pong; close/error returns `Err`.
pub async fn read_frame<S: Read + Write>(sock: &mut S, buf: &mut [u8]) -> Result<(u8, usize), ()> {
    loop {
        let mut hdr = [0u8; 2];
        read_exact(sock, &mut hdr).await?;
        let opcode = hdr[0] & 0x0F;
        let masked = hdr[1] & 0x80 != 0;
        let mut len = (hdr[1] & 0x7F) as usize;
        if len == 126 {
            let mut e = [0u8; 2];
            read_exact(sock, &mut e).await?;
            len = u16::from_be_bytes(e) as usize;
        } else if len == 127 {
            let mut e = [0u8; 8];
            read_exact(sock, &mut e).await?;
            len = u64::from_be_bytes(e) as usize;
        }
        let mut mask = [0u8; 4];
        if masked {
            read_exact(sock, &mut mask).await?;
        }
        if len > buf.len() {
            return Err(()); // frame larger than our buffer
        }
        read_exact(sock, &mut buf[..len]).await?;
        if masked {
            for i in 0..len {
                buf[i] ^= mask[i % 4];
            }
        }
        match opcode {
            0x9 => write_frame(sock, 0xA, &buf[..len.min(125)]).await?, // ping -> pong
            0xA => {}                                                   // pong, ignore
            0x8 => return Err(()),                                      // close
            _ => return Ok((opcode, len)),
        }
    }
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
    sock.write_all(&frame).await.map_err(|_| ())
}
