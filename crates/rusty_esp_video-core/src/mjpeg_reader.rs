//! The receiving side of [`crate::mjpeg_http`]: a streaming parser for a
//! `multipart/x-mixed-replace` JPEG stream over a caller-owned buffer.
//!
//! This is what the Pi hub, the host recorder and the mesh bridge use to take
//! a device's stream apart again. It never allocates: bytes are pushed into
//! the reader's buffer, complete parts are reported as ranges, and the caller
//! releases them when done.

use rusty_esp_core::error::{Error, Result};
use rusty_esp_core::time::Micros;

/// Longest boundary token accepted (RFC 2046 allows 70).
pub const MAX_BOUNDARY_LEN: usize = 70;

/// A complete part sitting in the reader's buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartInfo {
    start: usize,
    end: usize,
    /// The part's `X-Timestamp`, when present.
    pub timestamp: Option<Micros>,
    /// Bytes consumed by this part including its headers and trailing CRLF.
    consumed: usize,
}

impl PartInfo {
    /// Length of the part's body.
    #[must_use]
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// True when the body is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Nothing seen yet: decide whether a response head is present.
    Start,
    /// Inside the HTTP response head.
    Head,
    /// Between parts.
    Parts,
    /// The stream ended (`--boundary--`).
    Done,
}

/// A streaming multipart reader.
#[derive(Debug)]
pub struct Reader<'m> {
    buf: &'m mut [u8],
    len: usize,
    state: State,
    boundary: [u8; MAX_BOUNDARY_LEN],
    boundary_len: usize,
    /// Parts returned so far.
    pub parts: u64,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

impl<'m> Reader<'m> {
    /// A reader over `buf`, which must hold at least one whole part (headers
    /// plus the largest JPEG expected).
    pub fn new(buf: &'m mut [u8]) -> Self {
        Reader {
            buf,
            len: 0,
            state: State::Start,
            boundary: [0; MAX_BOUNDARY_LEN],
            boundary_len: 0,
            parts: 0,
        }
    }

    /// Append received bytes.
    pub fn push(&mut self, bytes: &[u8]) -> Result<()> {
        let end = self.len + bytes.len();
        if end > self.buf.len() {
            return Err(Error::BufferTooSmall { needed: end });
        }
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }

    /// True once the stream has terminated.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.state == State::Done
    }

    /// The boundary learned from the head or the first delimiter.
    #[must_use]
    pub fn boundary(&self) -> &[u8] {
        &self.boundary[..self.boundary_len]
    }

    /// The body bytes of a part reported by [`Self::next_part`].
    #[must_use]
    pub fn part(&self, info: &PartInfo) -> &[u8] {
        &self.buf[info.start..info.end]
    }

    /// Drop a part (and everything before it) from the buffer.
    pub fn release(&mut self, info: &PartInfo) {
        let n = info.consumed.min(self.len);
        self.buf.copy_within(n..self.len, 0);
        self.len -= n;
    }

    /// Try to parse the next complete part from the buffered bytes.
    pub fn next_part(&mut self) -> Result<Option<PartInfo>> {
        loop {
            match self.state {
                State::Start => {
                    if self.len < 5 {
                        return Ok(None);
                    }
                    self.state = if &self.buf[..5] == b"HTTP/" {
                        State::Head
                    } else {
                        State::Parts
                    };
                }
                State::Head => {
                    let Some(end) = find(&self.buf[..self.len], b"\r\n\r\n") else {
                        return Ok(None);
                    };
                    let head = &self.buf[..end];
                    if let Some(b) = find(head, b"boundary=") {
                        let rest = &head[b + 9..];
                        let stop = rest
                            .iter()
                            .position(|&c| c == b'\r' || c == b';' || c == b' ' || c == b'"')
                            .unwrap_or(rest.len());
                        let token = &rest[..stop];
                        let token = token.strip_prefix(b"\"").unwrap_or(token);
                        if token.is_empty() || token.len() > MAX_BOUNDARY_LEN {
                            return Err(Error::InvalidFormat);
                        }
                        self.boundary[..token.len()].copy_from_slice(token);
                        self.boundary_len = token.len();
                    }
                    let consumed = end + 4;
                    self.buf.copy_within(consumed..self.len, 0);
                    self.len -= consumed;
                    self.state = State::Parts;
                }
                State::Parts => return self.parse_part(),
                State::Done => return Ok(None),
            }
        }
    }

    fn parse_part(&mut self) -> Result<Option<PartInfo>> {
        let data = &self.buf[..self.len];
        // Skip leading CRLFs between parts.
        let mut i = 0;
        while data.get(i) == Some(&b'\r') || data.get(i) == Some(&b'\n') {
            i += 1;
        }
        if data.len() < i + 2 {
            return Ok(None);
        }
        if &data[i..i + 2] != b"--" {
            return Err(Error::InvalidFormat);
        }
        let Some(line_end) = find(&data[i..], b"\r\n") else {
            return Ok(None);
        };
        let line = &data[i + 2..i + line_end];
        if self.boundary_len == 0 {
            // Learn the boundary from the first delimiter.
            let token = line.strip_suffix(b"--").unwrap_or(line);
            if token.is_empty() || token.len() > MAX_BOUNDARY_LEN {
                return Err(Error::InvalidFormat);
            }
            self.boundary[..token.len()].copy_from_slice(token);
            self.boundary_len = token.len();
        }
        let boundary = &self.boundary[..self.boundary_len];
        if line == boundary {
            // a part follows
        } else if line.strip_suffix(b"--") == Some(boundary) {
            self.state = State::Done;
            return Ok(None);
        } else {
            return Err(Error::InvalidFormat);
        }
        let headers_start = i + line_end + 2;
        let Some(h_end) = find(&data[headers_start..], b"\r\n\r\n") else {
            return Ok(None);
        };
        let headers = &data[headers_start..headers_start + h_end];
        let mut content_length: Option<usize> = None;
        let mut timestamp: Option<Micros> = None;
        for header in headers.split(|&c| c == b'\n') {
            let header = header.strip_suffix(b"\r").unwrap_or(header);
            if let Some(v) = strip_header(header, b"content-length:") {
                content_length = Some(parse_usize(v).ok_or(Error::InvalidFormat)?);
            } else if let Some(v) = strip_header(header, b"x-timestamp:") {
                timestamp = parse_usize(v).map(|n| Micros(n as u64));
            }
        }
        let length = content_length.ok_or(Error::Unsupported)?;
        let body_start = headers_start + h_end + 4;
        let body_end = body_start + length;
        // body plus the CRLF that closes the part
        if data.len() < body_end + 2 {
            if body_end + 2 > self.buf.len() {
                return Err(Error::BufferTooSmall {
                    needed: body_end + 2,
                });
            }
            return Ok(None);
        }
        self.parts += 1;
        Ok(Some(PartInfo {
            start: body_start,
            end: body_end,
            timestamp,
            consumed: body_end + 2,
        }))
    }
}

/// Case-insensitive header match; returns the trimmed value.
fn strip_header<'a>(line: &'a [u8], name_lower: &[u8]) -> Option<&'a [u8]> {
    if line.len() < name_lower.len() {
        return None;
    }
    let (name, rest) = line.split_at(name_lower.len());
    if !name.eq_ignore_ascii_case(name_lower) {
        return None;
    }
    let mut v = rest;
    while let Some((&b' ', r)) = v.split_first() {
        v = r;
    }
    Some(v)
}

fn parse_usize(v: &[u8]) -> Option<usize> {
    if v.is_empty() {
        return None;
    }
    let mut n: usize = 0;
    for &c in v {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(usize::from(c - b'0'))?;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mjpeg_http::Multipart;
    use crate::packet::{Codec, MediaPacket};
    use crate::sink::SliceSink;

    fn stream_with_head(frames: &[&[u8]]) -> std::vec::Vec<u8> {
        let mut buf = [0u8; 2048];
        let mut mp = Multipart::new(SliceSink::new(&mut buf));
        mp.write_response_head().unwrap();
        for (i, f) in frames.iter().enumerate() {
            mp.push(&MediaPacket::new(
                Codec::Jpeg,
                true,
                Micros::from_millis(i as u64 * 40),
                f,
            ))
            .unwrap();
        }
        mp.into_sink().written().to_vec()
    }

    #[test]
    fn round_trips_the_writer_output_in_one_push() {
        let a = [0xFFu8, 0xD8, 1, 2, 0xFF, 0xD9];
        let b = [0xFFu8, 0xD8, 3, 4, 5, 0xFF, 0xD9];
        let stream = stream_with_head(&[&a, &b]);
        let mut mem = [0u8; 2048];
        let mut r = Reader::new(&mut mem);
        r.push(&stream).unwrap();
        let p = r.next_part().unwrap().unwrap();
        assert_eq!(r.boundary(), b"janus-frame");
        assert_eq!(r.part(&p), &a);
        assert_eq!(p.timestamp, Some(Micros::ZERO));
        r.release(&p);
        let p = r.next_part().unwrap().unwrap();
        assert_eq!(r.part(&p), &b);
        assert_eq!(p.timestamp, Some(Micros::from_millis(40)));
        r.release(&p);
        assert!(r.next_part().unwrap().is_none());
        assert_eq!(r.parts, 2);
    }

    #[test]
    fn handles_arbitrary_chunking_and_no_head() {
        let frames: std::vec::Vec<std::vec::Vec<u8>> = (0..5u8)
            .map(|i| {
                let mut f = std::vec![0xFF, 0xD8];
                f.extend(std::iter::repeat_n(i, 10 + i as usize * 7));
                f.extend_from_slice(&[0xFF, 0xD9]);
                f
            })
            .collect();
        let refs: std::vec::Vec<&[u8]> = frames.iter().map(|f| f.as_slice()).collect();
        let full = stream_with_head(&refs);
        // strip the response head: the reader learns the boundary from the first delimiter
        let head_end = find(&full, b"\r\n\r\n").unwrap() + 4;
        let body = &full[head_end..];
        for chunk in [1usize, 3, 7, 64, 1000] {
            let mut mem = [0u8; 2048];
            let mut r = Reader::new(&mut mem);
            let mut got = std::vec::Vec::new();
            for piece in body.chunks(chunk) {
                r.push(piece).unwrap();
                while let Some(p) = r.next_part().unwrap() {
                    got.push(r.part(&p).to_vec());
                    r.release(&p);
                }
            }
            assert_eq!(got, frames, "chunk size {chunk}");
        }
    }

    #[test]
    fn terminator_and_errors() {
        let mut mem = [0u8; 256];
        let mut r = Reader::new(&mut mem);
        r.push(b"--janus-frame--\r\n").unwrap();
        assert!(r.next_part().unwrap().is_none());
        assert!(r.is_done());

        let mut mem = [0u8; 64];
        let mut r = Reader::new(&mut mem);
        r.push(b"--x\r\nContent-Type: image/jpeg\r\n\r\n").unwrap();
        assert_eq!(
            r.next_part(),
            Err(Error::Unsupported),
            "Content-Length is required"
        );

        let mut mem = [0u8; 64];
        let mut r = Reader::new(&mut mem);
        r.push(b"--x\r\nContent-Length: 500\r\n\r\n").unwrap();
        assert!(matches!(r.next_part(), Err(Error::BufferTooSmall { .. })));

        let mut mem = [0u8; 64];
        let mut r = Reader::new(&mut mem);
        r.push(b"garbage\r\n").unwrap();
        assert_eq!(r.next_part(), Err(Error::InvalidFormat));
    }
}
