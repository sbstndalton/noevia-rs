//! A minimal synchronous RFC 6455 WebSocket client: the opening handshake, masked client frames,
//! text and continuation frames, ping, pong and close. Server frames must be unmasked.

use base64::Engine as _;
use sha1::{Digest, Sha1};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// The largest message (or frame) accepted from the browser.
pub(crate) const MAX_MESSAGE: usize = 64 << 20;
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_HEAD: usize = 16 * 1024;

pub(crate) const CONTINUATION: u8 = 0x0;
pub(crate) const TEXT: u8 = 0x1;
pub(crate) const BINARY: u8 = 0x2;
pub(crate) const CLOSE: u8 = 0x8;
pub(crate) const PING: u8 = 0x9;
pub(crate) const PONG: u8 = 0xA;

pub(crate) fn bad(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// `Sec-WebSocket-Accept` for a `Sec-WebSocket-Key` (RFC 6455 section 4.2.2).
pub(crate) fn accept_key(key: &str) -> String {
    let mut hash = Sha1::new();
    hash.update(key.as_bytes());
    hash.update(GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hash.finalize())
}

pub(crate) fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(out)
}

/// One final client frame, masked with `mask`.
pub(crate) fn encode(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let len = payload.len();
    let mut out = Vec::with_capacity(len.saturating_add(14));
    out.push(0x80 | opcode);
    if len < 126 {
        out.push(0x80 | len as u8);
    } else if let Ok(short) = u16::try_from(len) {
        out.push(0x80 | 126);
        out.extend_from_slice(&short.to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    out.extend(payload.iter().zip(mask.iter().cycle()).map(|(b, m)| b ^ m));
    out
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub fin: bool,
    pub opcode: u8,
    pub masked: bool,
    pub payload: Vec<u8>,
    /// Longer than the cap: its payload was read and dropped.
    pub oversize: bool,
}

/// One frame (unmasked if it was masked). A data frame longer than `max` is read and dropped
/// (`oversize`), so the connection stays in step.
pub(crate) fn read_frame(r: &mut impl Read, max: usize) -> io::Result<Frame> {
    let mut head = [0u8; 2];
    r.read_exact(&mut head)?;
    let [b0, b1] = head;
    if b0 & 0x70 != 0 {
        return Err(bad("WebSocket frame with reserved bits set"));
    }
    let (fin, opcode, masked) = (b0 & 0x80 != 0, b0 & 0x0F, b1 & 0x80 != 0);
    let len = match b1 & 0x7F {
        126 => {
            let mut b = [0u8; 2];
            r.read_exact(&mut b)?;
            u64::from(u16::from_be_bytes(b))
        }
        127 => {
            let mut b = [0u8; 8];
            r.read_exact(&mut b)?;
            u64::from_be_bytes(b)
        }
        n => u64::from(n),
    };
    if opcode >= CLOSE && (len > 125 || !fin) {
        return Err(bad("Malformed WebSocket control frame"));
    }
    let mut mask = [0u8; 4];
    if masked {
        r.read_exact(&mut mask)?;
    }
    let Some(len) = usize::try_from(len).ok().filter(|l| *l <= max) else {
        let dropped = io::copy(&mut r.take(len), &mut io::sink())?;
        if dropped < len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        return Ok(Frame {
            fin,
            opcode,
            masked,
            payload: Vec::new(),
            oversize: true,
        });
    };
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    if masked {
        for (b, m) in payload.iter_mut().zip(mask.iter().cycle()) {
            *b ^= m;
        }
    }
    Ok(Frame {
        fin,
        opcode,
        masked,
        payload,
        oversize: false,
    })
}

/// What the browser sent.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Incoming {
    Text(String),
    Ping(Vec<u8>),
    Close,
    /// A message over the cap, read and dropped (its caller, if any, meets its deadline).
    Oversize,
}

/// Reassembles fragmented messages; control frames may arrive between fragments.
#[derive(Default)]
pub(crate) struct MessageReader {
    partial: Option<(u8, Vec<u8>)>,
    /// Dropping the rest of an oversize fragmented message.
    skipping: bool,
}

impl MessageReader {
    pub(crate) fn next(&mut self, r: &mut impl Read, max: usize) -> io::Result<Incoming> {
        loop {
            let frame = read_frame(r, max)?;
            if frame.masked {
                return Err(bad("Masked WebSocket frame from the server"));
            }
            let (opcode, payload) = match frame.opcode {
                PING => return Ok(Incoming::Ping(frame.payload)),
                PONG => continue,
                CLOSE => return Ok(Incoming::Close),
                TEXT | BINARY if self.partial.is_none() && !self.skipping => {
                    if frame.oversize {
                        self.skipping = !frame.fin;
                        if frame.fin {
                            return Ok(Incoming::Oversize);
                        }
                        continue;
                    }
                    (frame.opcode, frame.payload)
                }
                CONTINUATION if self.skipping => {
                    if frame.fin {
                        self.skipping = false;
                        return Ok(Incoming::Oversize);
                    }
                    continue;
                }
                CONTINUATION => {
                    let (opcode, mut acc) = self
                        .partial
                        .take()
                        .ok_or_else(|| bad("Unexpected continuation frame"))?;
                    if frame.oversize || acc.len().saturating_add(frame.payload.len()) > max {
                        self.skipping = !frame.fin;
                        if frame.fin {
                            return Ok(Incoming::Oversize);
                        }
                        continue;
                    }
                    acc.extend_from_slice(&frame.payload);
                    (opcode, acc)
                }
                _ => return Err(bad("Unexpected WebSocket frame")),
            };
            if !frame.fin {
                self.partial = Some((opcode, payload));
            } else if opcode == TEXT {
                return String::from_utf8(payload)
                    .map(Incoming::Text)
                    .map_err(|_| bad("WebSocket text is not UTF-8"));
            }
            // A whole binary message: the protocol never sends one; skip it.
        }
    }
}

/// A stream whose reads and writes all end by one deadline (the handshake, `/json/version`).
pub(crate) struct Bounded<'a> {
    pub stream: &'a mut TcpStream,
    pub until: Instant,
}

impl Bounded<'_> {
    fn left(&self) -> io::Result<Duration> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the browser engine did not answer in time",
            ));
        }
        Ok(left)
    }
}

impl Read for Bounded<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.left()?;
        self.stream.set_read_timeout(Some(left))?;
        self.stream.read(buf)
    }
}

impl Write for Bounded<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let left = self.left()?;
        self.stream.set_write_timeout(Some(left))?;
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// An HTTP response head (up to the blank line), read byte by byte so nothing after it is consumed.
pub(crate) fn read_head(r: &mut impl Read) -> io::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEAD {
            return Err(bad("HTTP response head too large"));
        }
        r.read_exact(&mut byte)?;
        head.extend_from_slice(&byte);
    }
    String::from_utf8(head).map_err(|_| bad("HTTP response head is not UTF-8"))
}

/// `name: value` of a response head, case-insensitively.
pub(crate) fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.split("\r\n").skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then_some(v.trim())
    })
}

pub(crate) fn status(head: &str) -> Option<u16> {
    head.split(' ').nth(1)?.parse().ok()
}

/// The opening handshake (no Origin header).
pub(crate) fn handshake(
    stream: &mut (impl Read + Write),
    host: &str,
    path: &str,
) -> io::Result<()> {
    let key = base64::engine::general_purpose::STANDARD.encode(random::<16>()?);
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(request.as_bytes())?;
    let head = read_head(stream)?;
    if status(&head) != Some(101) {
        let line = head.split("\r\n").next().unwrap_or("");
        return Err(bad(format!(
            "The browser engine refused the WebSocket upgrade: {line}"
        )));
    }
    if header(&head, "sec-websocket-accept") != Some(accept_key(&key).as_str()) {
        return Err(bad(
            "The browser engine answered the WebSocket upgrade with a wrong accept key",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn server_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![if fin { 0x80 } else { 0 } | opcode];
        let len = payload.len();
        if len < 126 {
            out.push(len as u8);
        } else if len <= 0xFFFF {
            out.push(126);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            out.push(127);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn accept_key_matches_rfc_example() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn client_frames_are_masked_with_each_length_form() {
        let mask = [0x37, 0xFA, 0x21, 0x3D];
        // RFC 6455 section 5.7: a masked "Hello".
        assert_eq!(
            encode(TEXT, b"Hello", mask),
            [0x81, 0x85, 0x37, 0xFA, 0x21, 0x3D, 0x7F, 0x9F, 0x4D, 0x51, 0x58]
        );
        for (len, header_len) in [(0, 6), (125, 6), (126, 8), (65_535, 8), (65_536, 14)] {
            let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let bytes = encode(TEXT, &payload, mask);
            assert_eq!(bytes.len(), len + header_len, "{len}");
            let frame = read_frame(&mut bytes.as_slice(), MAX_MESSAGE).unwrap();
            assert_eq!((frame.fin, frame.opcode, frame.masked), (true, TEXT, true));
            assert_eq!(frame.payload, payload, "{len}");
        }
        assert_eq!(encode(TEXT, &[0; 126], mask)[1], 0x80 | 126);
        assert_eq!(encode(TEXT, &[0; 65_536], mask)[1], 0x80 | 127);
    }

    #[test]
    fn server_lengths_and_cap() {
        for len in [0, 125, 126, 65_535, 65_536] {
            let payload = vec![b'x'; len];
            let bytes = server_frame(true, TEXT, &payload);
            let mut reader = MessageReader::default();
            assert_eq!(
                reader.next(&mut bytes.as_slice(), MAX_MESSAGE).unwrap(),
                Incoming::Text("x".repeat(len))
            );
        }
        // An oversize message is read past and dropped; the next one arrives intact.
        let mut bytes = server_frame(true, TEXT, &[b'x'; 200]);
        bytes.extend(server_frame(true, TEXT, b"after"));
        let mut r = bytes.as_slice();
        let mut reader = MessageReader::default();
        assert_eq!(reader.next(&mut r, 100).unwrap(), Incoming::Oversize);
        assert_eq!(
            reader.next(&mut r, 100).unwrap(),
            Incoming::Text("after".into())
        );
        // A declared 64-bit length past the cap allocates nothing; the stream ends inside it.
        let mut huge = vec![0x81, 127];
        huge.extend_from_slice(&u64::MAX.to_be_bytes());
        huge.extend_from_slice(b"some");
        assert!(read_frame(&mut huge.as_slice(), MAX_MESSAGE).is_err());
    }

    #[test]
    fn fragments_with_control_frames_between() {
        let mut bytes = server_frame(false, TEXT, b"{\"id\":");
        bytes.extend(server_frame(true, PING, b"hi"));
        bytes.extend(server_frame(false, CONTINUATION, b"1,"));
        bytes.extend(server_frame(true, PONG, b""));
        bytes.extend(server_frame(true, CONTINUATION, b"\"result\":{}}"));
        bytes.extend(server_frame(true, CLOSE, &1000u16.to_be_bytes()));
        let mut r = bytes.as_slice();
        let mut reader = MessageReader::default();
        assert_eq!(
            reader.next(&mut r, MAX_MESSAGE).unwrap(),
            Incoming::Ping(b"hi".to_vec())
        );
        assert_eq!(
            reader.next(&mut r, MAX_MESSAGE).unwrap(),
            Incoming::Text("{\"id\":1,\"result\":{}}".into())
        );
        assert_eq!(reader.next(&mut r, MAX_MESSAGE).unwrap(), Incoming::Close);
        // Fragments together past the cap are dropped, whichever fragment crosses it.
        let mut bytes = server_frame(false, TEXT, &[b'a'; 60]);
        bytes.extend(server_frame(false, CONTINUATION, &[b'a'; 60]));
        bytes.extend(server_frame(true, CONTINUATION, &[b'a'; 60]));
        bytes.extend(server_frame(false, TEXT, &[b'b'; 200]));
        bytes.extend(server_frame(true, PING, b""));
        bytes.extend(server_frame(true, CONTINUATION, b"b"));
        bytes.extend(server_frame(true, TEXT, b"next"));
        let mut r = bytes.as_slice();
        let mut reader = MessageReader::default();
        assert_eq!(reader.next(&mut r, 100).unwrap(), Incoming::Oversize);
        assert_eq!(
            reader.next(&mut r, 100).unwrap(),
            Incoming::Ping(Vec::new())
        );
        assert_eq!(reader.next(&mut r, 100).unwrap(), Incoming::Oversize);
        assert_eq!(
            reader.next(&mut r, 100).unwrap(),
            Incoming::Text("next".into())
        );
    }

    #[test]
    fn refusals() {
        let cases: Vec<Vec<u8>> = vec![
            encode(TEXT, b"masked", [1, 2, 3, 4]),
            server_frame(true, CONTINUATION, b"orphan"),
            server_frame(false, PING, b""),
            vec![0x81 | 0x40, 0],
            server_frame(true, TEXT, &[0xFF, 0xFE]),
            server_frame(true, 0x3, b""),
        ];
        for bytes in cases {
            assert!(
                MessageReader::default()
                    .next(&mut bytes.as_slice(), MAX_MESSAGE)
                    .is_err(),
                "{bytes:?}"
            );
        }
        let mut text_in_text = server_frame(false, TEXT, b"a");
        text_in_text.extend(server_frame(true, TEXT, b"b"));
        assert!(MessageReader::default()
            .next(&mut text_in_text.as_slice(), MAX_MESSAGE)
            .is_err());
    }

    #[test]
    fn handshake_checks_the_accept_key() {
        struct Pipe {
            sent: Vec<u8>,
            answer: Option<std::io::Cursor<Vec<u8>>>,
            wrong: bool,
        }
        impl Read for Pipe {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.answer.is_none() {
                    let text = String::from_utf8(self.sent.clone()).unwrap();
                    let key = header(&text, "sec-websocket-key").unwrap().to_string();
                    let accept = if self.wrong {
                        accept_key("other")
                    } else {
                        accept_key(&key)
                    };
                    let reply = format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n");
                    self.answer = Some(std::io::Cursor::new(reply.into_bytes()));
                }
                self.answer.as_mut().unwrap().read(buf)
            }
        }
        impl Write for Pipe {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.sent.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut ok = Pipe {
            sent: Vec::new(),
            answer: None,
            wrong: false,
        };
        handshake(&mut ok, "127.0.0.1:9222", "/devtools/browser/qa").unwrap();
        let sent = String::from_utf8(ok.sent).unwrap();
        assert!(
            sent.starts_with("GET /devtools/browser/qa HTTP/1.1\r\nHost: 127.0.0.1:9222\r\n"),
            "{sent}"
        );
        assert!(!sent.to_ascii_lowercase().contains("origin:"));
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(header(&sent, "Sec-WebSocket-Key").unwrap())
                .unwrap()
                .len(),
            16
        );
        let mut wrong = Pipe {
            sent: Vec::new(),
            answer: None,
            wrong: true,
        };
        assert!(handshake(&mut wrong, "127.0.0.1:9222", "/").is_err());
    }
}
