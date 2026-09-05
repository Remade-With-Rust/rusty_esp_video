//! `multipart/x-mixed-replace` — the CameraWebServer stream, remade.
//!
//! Every browser opens it; so does the Pi hub. The response is one HTTP
//! header followed by an unbounded sequence of parts, each a complete JPEG
//! with its own `Content-Length`. Nothing here allocates: header numbers are
//! formatted into a stack buffer and every piece goes straight to the sink.

use rusty_esp_core::error::{Error, Result};

use crate::fmt_u64;
use crate::packet::{Codec, MediaPacket};
use crate::sink::PacketSink;

/// The boundary string used unless the caller picks another.
pub const DEFAULT_BOUNDARY: &str = "janus-frame";

/// A multipart JPEG stream writer.
#[derive(Debug)]
pub struct Multipart<S> {
    sink: S,
    boundary: &'static str,
    frames: u64,
}

impl<S: PacketSink> Multipart<S> {
    /// Stream into `sink` with [`DEFAULT_BOUNDARY`].
    pub fn new(sink: S) -> Self {
        Self::with_boundary(sink, DEFAULT_BOUNDARY)
    }

    /// Stream into `sink` with a custom boundary (ASCII, no CR/LF/`"`).
    pub fn with_boundary(sink: S, boundary: &'static str) -> Self {
        Multipart {
            sink,
            boundary,
            frames: 0,
        }
    }

    /// Frames pushed so far.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Give the sink back.
    pub fn into_sink(self) -> S {
        self.sink
    }

    /// Write the HTTP response head. Call once, before the first frame, when
    /// the sink is a raw TCP connection; skip it when an HTTP server owns the
    /// head.
    pub fn write_response_head(&mut self) -> Result<()> {
        self.sink.write(b"HTTP/1.1 200 OK\r\n")?;
        self.sink
            .write(b"Content-Type: multipart/x-mixed-replace; boundary=")?;
        self.sink.write(self.boundary.as_bytes())?;
        self.sink
            .write(b"\r\nCache-Control: no-cache, no-store\r\n")?;
        self.sink.write(b"Pragma: no-cache\r\n")?;
        self.sink.write(b"Connection: close\r\n")?;
        self.sink.write(b"Access-Control-Allow-Origin: *\r\n\r\n")
    }

    /// The value for a `Content-Type` header when an HTTP server owns the head.
    #[must_use]
    pub fn content_type(&self) -> &'static str {
        match self.boundary {
            DEFAULT_BOUNDARY => "multipart/x-mixed-replace; boundary=janus-frame",
            _ => "multipart/x-mixed-replace",
        }
    }

    /// Write one JPEG part. Non-JPEG packets are refused.
    pub fn push(&mut self, packet: &MediaPacket<'_>) -> Result<()> {
        if packet.codec != Codec::Jpeg || packet.is_empty() {
            return Err(Error::Unsupported);
        }
        let mut num = [0u8; 20];
        self.sink.write(b"--")?;
        self.sink.write(self.boundary.as_bytes())?;
        self.sink
            .write(b"\r\nContent-Type: image/jpeg\r\nContent-Length: ")?;
        self.sink
            .write(fmt_u64(packet.len() as u64, &mut num).as_bytes())?;
        self.sink.write(b"\r\nX-Timestamp: ")?;
        self.sink
            .write(fmt_u64(packet.timestamp.0, &mut num).as_bytes())?;
        self.sink.write(b"\r\n\r\n")?;
        self.sink.write(packet.data)?;
        self.sink.write(b"\r\n")?;
        self.frames += 1;
        Ok(())
    }
}

/// The query parameter and cookie name a viewing token travels under.
pub const TOKEN_PARAM: &str = "t";

/// A viewing token the page and the stream require.
///
/// The camera page is served on a network the owner does not control the
/// members of, so it is not open to whoever finds the address: a request
/// passes when it carries the token as `?t=<token>` on its target, or as a
/// `t=<token>` cookie the page set when it was opened with the token. The
/// token is minted once on the device (16 random bytes, base58) and printed
/// at boot as the URL to open. Comparison is constant-time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gate<'a> {
    token: &'a str,
}

impl<'a> Gate<'a> {
    /// A gate that admits `token`.
    #[must_use]
    pub const fn new(token: &'a str) -> Self {
        Gate { token }
    }

    /// The token this gate admits.
    #[must_use]
    pub const fn token(&self) -> &'a str {
        self.token
    }

    /// Whether a request for `target` (the path with any query string),
    /// carrying `cookie` (the `Cookie` header's value, if any), may pass.
    #[must_use]
    pub fn admits(&self, target: &str, cookie: Option<&str>) -> bool {
        let presented = query_param(target, TOKEN_PARAM)
            .or_else(|| cookie.and_then(|c| cookie_value(c, TOKEN_PARAM)));
        presented.is_some_and(|p| eq_constant_time(p.as_bytes(), self.token.as_bytes()))
    }
}

/// The value of `name` in `target`'s query string (`/path?a=1&name=v`), if
/// present. No percent-decoding: a base58 token needs none.
#[must_use]
pub fn query_param<'t>(target: &'t str, name: &str) -> Option<&'t str> {
    let (_, query) = target.split_once('?')?;
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// The value of cookie `name` in a `Cookie` header value (`a=1; name=v`).
#[must_use]
pub fn cookie_value<'c>(header: &'c str, name: &str) -> Option<&'c str> {
    header
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// Equal without an early exit on the first differing byte, so a wrong token
/// takes the same time whatever prefix it shares with the right one.
#[must_use]
pub fn eq_constant_time(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_takes_the_token_from_the_query_or_the_cookie_and_nowhere_else() {
        let gate = Gate::new("7Ns3kQxYbVfR2pLmZ9wHdE");
        assert!(gate.admits("/?t=7Ns3kQxYbVfR2pLmZ9wHdE", None));
        assert!(gate.admits("/stream?t=7Ns3kQxYbVfR2pLmZ9wHdE", None));
        assert!(gate.admits("/?x=1&t=7Ns3kQxYbVfR2pLmZ9wHdE&y=2", None));
        assert!(gate.admits("/stream", Some("t=7Ns3kQxYbVfR2pLmZ9wHdE")));
        assert!(gate.admits("/stream", Some("theme=dark; t=7Ns3kQxYbVfR2pLmZ9wHdE")));
        assert!(gate.admits("/stream", Some("t=7Ns3kQxYbVfR2pLmZ9wHdE;theme=dark")));
        // without, wrong, a prefix, a longer one, the name only, another cookie
        assert!(!gate.admits("/", None));
        assert!(!gate.admits("/stream", None));
        assert!(!gate.admits("/?t=7Ns3kQxYbVfR2pLmZ9wHdF", None));
        assert!(!gate.admits("/?t=7Ns3kQxYbVfR2pLmZ9wHd", None));
        assert!(!gate.admits("/?t=7Ns3kQxYbVfR2pLmZ9wHdE1", None));
        assert!(!gate.admits("/?t=", None));
        assert!(!gate.admits("/?token=7Ns3kQxYbVfR2pLmZ9wHdE", None));
        assert!(!gate.admits("/stream", Some("tt=7Ns3kQxYbVfR2pLmZ9wHdE")));
        assert!(!gate.admits("/stream", Some("theme=7Ns3kQxYbVfR2pLmZ9wHdE")));
        // the query is what is looked at first, either way round
        assert!(gate.admits("/?t=7Ns3kQxYbVfR2pLmZ9wHdE", Some("t=stale")));
        assert!(!gate.admits("/?t=stale", Some("t=7Ns3kQxYbVfR2pLmZ9wHdE")));
        assert_eq!(gate.token(), "7Ns3kQxYbVfR2pLmZ9wHdE");
    }

    #[test]
    fn query_and_cookie_parsing() {
        assert_eq!(query_param("/?t=abc", "t"), Some("abc"));
        assert_eq!(query_param("/stream?a=1&t=abc", "t"), Some("abc"));
        assert_eq!(query_param("/stream", "t"), None);
        assert_eq!(query_param("/?t", "t"), None);
        assert_eq!(query_param("/?t=", "t"), Some(""));
        assert_eq!(cookie_value("t=abc", "t"), Some("abc"));
        assert_eq!(cookie_value("a=1; t=abc; b=2", "t"), Some("abc"));
        assert_eq!(cookie_value("a=1", "t"), None);
        assert_eq!(cookie_value("", "t"), None);
    }

    #[test]
    fn constant_time_equality_is_equality() {
        assert!(eq_constant_time(b"", b""));
        assert!(eq_constant_time(b"abc", b"abc"));
        assert!(!eq_constant_time(b"abc", b"abd"));
        assert!(!eq_constant_time(b"abc", b"ab"));
        assert!(!eq_constant_time(b"", b"a"));
    }
    use crate::sink::SliceSink;
    use rusty_esp_core::time::Micros;

    /// A browser-shaped parser: splits on the boundary, reads Content-Length,
    /// returns the bodies.
    fn parse_parts<'a>(stream: &'a [u8], boundary: &str) -> std::vec::Vec<(&'a [u8], u64)> {
        let s = std::str::from_utf8(stream).unwrap_or("");
        let _ = s;
        let mut parts = std::vec::Vec::new();
        let marker = std::format!("--{boundary}\r\n");
        let mut i = 0;
        while let Some(p) = find(&stream[i..], marker.as_bytes()) {
            let start = i + p + marker.len();
            let hdr_end = start + find(&stream[start..], b"\r\n\r\n").unwrap();
            let headers = std::str::from_utf8(&stream[start..hdr_end]).unwrap();
            let mut len = 0usize;
            let mut ts = 0u64;
            for line in headers.split("\r\n") {
                if let Some(v) = line.strip_prefix("Content-Length: ") {
                    len = v.parse().unwrap();
                }
                if let Some(v) = line.strip_prefix("X-Timestamp: ") {
                    ts = v.parse().unwrap();
                }
                if let Some(v) = line.strip_prefix("Content-Type: ") {
                    assert_eq!(v, "image/jpeg");
                }
            }
            let body_start = hdr_end + 4;
            let body = &stream[body_start..body_start + len];
            assert_eq!(&stream[body_start + len..body_start + len + 2], b"\r\n");
            parts.push((body, ts));
            i = body_start + len + 2;
        }
        parts
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    #[test]
    fn head_and_parts_parse_like_a_browser() {
        let mut buf = [0u8; 1024];
        let mut mp = Multipart::new(SliceSink::new(&mut buf));
        mp.write_response_head().unwrap();
        let a = [0xFFu8, 0xD8, 1, 2, 3, 0xFF, 0xD9];
        let b = [0xFFu8, 0xD8, 9, 8, 7, 6, 5, 4, 0xFF, 0xD9];
        mp.push(&MediaPacket::new(
            Codec::Jpeg,
            true,
            Micros::from_millis(100),
            &a,
        ))
        .unwrap();
        mp.push(&MediaPacket::new(
            Codec::Jpeg,
            true,
            Micros::from_millis(200),
            &b,
        ))
        .unwrap();
        assert_eq!(mp.frames(), 2);
        let sink = mp.into_sink();
        let out = sink.written();
        let head_end = find(out, b"\r\n\r\n").unwrap();
        let head = std::str::from_utf8(&out[..head_end]).unwrap();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head.contains("multipart/x-mixed-replace; boundary=janus-frame"));
        let parts = parse_parts(&out[head_end + 4..], DEFAULT_BOUNDARY);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], (&a[..], 100_000));
        assert_eq!(parts[1], (&b[..], 200_000));
    }

    #[test]
    fn refuses_non_jpeg_and_overflow() {
        let mut buf = [0u8; 64];
        let mut mp = Multipart::new(SliceSink::new(&mut buf));
        let h264 = [0u8, 0, 0, 1, 0x65];
        assert_eq!(
            mp.push(&MediaPacket::new(Codec::H264, true, Micros::ZERO, &h264)),
            Err(Error::Unsupported)
        );
        let big = [0xFFu8; 100];
        assert!(matches!(
            mp.push(&MediaPacket::new(Codec::Jpeg, true, Micros::ZERO, &big)),
            Err(Error::BufferTooSmall { .. })
        ));
    }
}
