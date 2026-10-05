//! The camera page's HTTP, with no socket in it.
//!
//! A chip serves three things: the page, the stream, and a refusal. What
//! those are — the request line that names them, the gate that admits them,
//! the bytes of each response — is protocol, and protocol does not care
//! whether the connection is `std::net` on ESP-IDF or embassy-net on bare
//! metal. So it lives here, over [`PacketSink`], and each transport keeps
//! only its own reading and writing: the `std` server in
//! `rusty_esp_video-esp::net`, and a Track B firmware over its async socket
//! (which formats a response into a [`SliceSink`](crate::sink::SliceSink)
//! and sends the slice).
//!
//! Nothing here allocates: a request is parsed in place, a path maps onto a
//! fixed set, and every response is written piecewise to the sink.

use rusty_esp_core::error::Result;

use crate::fmt_u64;
use crate::mjpeg_http::{Gate, TOKEN_PARAM};
use crate::sink::PacketSink;

/// The path that streams.
pub const STREAM_PATH: &str = "/stream";

/// Largest request head a server accepts.
pub const MAX_REQUEST_BYTES: usize = 2048;

/// The page: a full-screen `<img>` of the stream, and nothing else.
pub const INDEX_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>janus</title>\
<style>html,body{margin:0;background:#000;height:100%}img{display:block;max-width:100vw;max-height:100vh;margin:auto}</style>\
</head><body><img src=\"/stream\" alt=\"stream\"></body></html>";

/// Where a request points, out of the paths a chip serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    /// `/` or `/index.html`: the page.
    Index,
    /// `/stream`: the multipart JPEG stream.
    Stream,
    /// `/update`: a signed firmware image, `PUT` by the owner (X7).
    Update,
    /// `/setup`: the setup session over the device's own page (the setup
    /// protocol's section 11.2, the experiments plan's E7): `GET` is
    /// Discover, `POST` one message.
    Setup,
    /// Anything else: a `404`.
    Other,
}

/// What a request head turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// A `GET`, and whether the gate (if any) let it through: the token on
    /// the query string or in the `Cookie` header. Always admitted ungated.
    Get {
        /// Where it points.
        path: Path,
        /// Whether it may have what it asked for.
        admitted: bool,
    },
    /// A `PUT`, with the body's announced length when the head carried one;
    /// gated like a `GET`. Only `/update` takes one.
    Put {
        /// Where it points.
        path: Path,
        /// Whether it may do what it asked.
        admitted: bool,
        /// `Content-Length`, if the head had it.
        content_length: Option<u64>,
    },
    /// A `POST`, gated like a `GET`: only `/setup` takes one. Its body is
    /// `content_length` bytes behind the head.
    Post {
        /// Where it points.
        path: Path,
        /// Whether it may do what it asked.
        admitted: bool,
        /// `Content-Length`, if the head had it.
        content_length: Option<u64>,
        /// `X-Setup-Session`'s value, as sent, when it is 16 bytes long
        /// (the setup carrier checks that they are hex digits).
        setup_session: Option<[u8; 16]>,
    },
    /// Any other method: a `405`.
    Other,
}

/// Whether `head` holds a complete request head (the blank line has
/// arrived). A transport reads until this is true or [`MAX_REQUEST_BYTES`]
/// is full.
#[must_use]
pub fn head_complete(head: &[u8]) -> bool {
    blank_line(head, 0).is_some()
}

/// [`head_complete`] for a head that grows by reads: `seen` is the length
/// already checked (and found incomplete), so only the new bytes and the
/// three before them are searched (W8 of the optimization campaign). Same
/// answer as `head_complete(head)` whenever `head[..seen]` had no blank line.
#[must_use]
pub fn head_complete_from(head: &[u8], seen: usize) -> bool {
    blank_line(head, seen.saturating_sub(3)).is_some()
}

/// The first `\n` at or after `k`. On an ESP32-S3 with `pie-s3`, sixteen
/// bytes a test on the vector unit (`rusty_esp_dsp-esp`'s `find_byte`,
/// round 2 R12); elsewhere [`next_newline_portable`].
fn next_newline(head: &[u8], k: usize) -> Option<usize> {
    #[cfg(all(feature = "pie-s3", target_arch = "xtensa"))]
    {
        let i = rusty_esp_dsp_esp::pie_s3::find_byte(head, k, b'\n');
        (i < head.len()).then_some(i)
    }
    #[cfg(not(all(feature = "pie-s3", target_arch = "xtensa")))]
    {
        next_newline_portable(head, k)
    }
}

/// The first `\n` at or after `k`. Eight bytes at a time while none of
/// them is `\n` (a byte is `\n` exactly when `byte ^ 0x0A` is zero, so the
/// minimum of the eight XORs is non-zero), then byte by byte (W11).
#[cfg_attr(all(feature = "pie-s3", target_arch = "xtensa"), allow(dead_code))]
fn next_newline_portable(head: &[u8], mut k: usize) -> Option<usize> {
    while let Some(w) = head.get(k..k + 8) {
        let m = (w[0] ^ b'\n')
            .min(w[1] ^ b'\n')
            .min((w[2] ^ b'\n').min(w[3] ^ b'\n'))
            .min(
                (w[4] ^ b'\n')
                    .min(w[5] ^ b'\n')
                    .min((w[6] ^ b'\n').min(w[7] ^ b'\n')),
            );
        if m == 0 {
            break;
        }
        k += 8;
    }
    head[k..].iter().position(|&b| b == b'\n').map(|i| k + i)
}

/// The value (untrimmed) of the first header line, after the request line,
/// whose name before the first `:` is `name` once trimmed, ignoring ASCII
/// case. A line is split and trimmed only when its first byte could begin
/// `name` after trimming: `name`'s first letter in either case, or the first
/// byte of a Unicode `White_Space` character (ASCII `\t`..`\r` and space,
/// or the lead bytes `C2`, `E1`, `E2`, `E3` of the others). Any other line
/// is one the trimmed comparison rejects too (W9 of the optimization
/// campaign).
fn header<'t>(text: &'t str, name: &str) -> Option<&'t str> {
    let first = name.as_bytes().first()?.to_ascii_lowercase();
    text.lines()
        .skip(1)
        .filter(|l| {
            l.as_bytes().first().is_some_and(|&b| {
                b.to_ascii_lowercase() == first
                    || matches!(b, b'\t'..=b'\r' | b' ' | 0xC2 | 0xE1 | 0xE2 | 0xE3)
            })
        })
        .filter_map(|l| l.split_once(':'))
        .find(|(n, _)| n.trim().eq_ignore_ascii_case(name))
        .map(|(_, value)| value)
}

/// The offset just past the first `\r\n\r\n` that ends at or after
/// `from + 3`: found by its `\n`, checking the three bytes before it. The
/// slice comparison it replaced (`windows(4)` against the pattern) called
/// `memcmp` once per byte of the head (W7 of the optimization campaign).
fn blank_line(head: &[u8], from: usize) -> Option<usize> {
    #[cfg(all(feature = "pie-s3", target_arch = "xtensa"))]
    {
        // R13: the blank line is where `\n\r` occurs; the `\r` before the
        // pair and the `\n` after it finish the `\r\n\r\n`. A pair at
        // `i` is the pattern at `i - 1`, which must start at `from` or
        // later: the search starts at `from + 1`.
        let mut j = from + 1;
        loop {
            let i = rusty_esp_dsp_esp::pie_s3::find_pair(head, j, b'\n', b'\r');
            if i >= head.len() {
                return None;
            }
            if head[i - 1] == b'\r' && head.get(i + 2) == Some(&b'\n') {
                return Some(i + 3);
            }
            j = i + 1;
        }
    }
    #[cfg(not(all(feature = "pie-s3", target_arch = "xtensa")))]
    {
        blank_line_portable(head, from)
    }
}

/// [`blank_line`] off the vector unit: each newline found, the three bytes
/// before it checked (W7, W11). The oracle.
#[cfg_attr(all(feature = "pie-s3", target_arch = "xtensa"), allow(dead_code))]
fn blank_line_portable(head: &[u8], from: usize) -> Option<usize> {
    let mut k = from + 3;
    while k < head.len() {
        let at = next_newline(head, k)?;
        if head[at - 1] == b'\r' && head[at - 2] == b'\n' && head[at - 3] == b'\r' {
            return Some(at + 1);
        }
        k = at + 1;
    }
    None
}

/// Parse a complete request head. `None` is a `400`: not UTF-8, no request
/// line, or a request line without a method and a target.
#[must_use]
pub fn parse_request(head: &[u8], gate: Option<&Gate<'_>>) -> Option<Request> {
    // only the head: a PUT's body may already have arrived behind the blank
    // line, and it is bytes, not text (every update was a 400 until this)
    let end = body_offset(head).unwrap_or(head.len());
    let text = core::str::from_utf8(&head[..end]).ok()?;
    let line = text.lines().next()?;
    // An ASCII line splits the same on ASCII whitespace as on Unicode
    // whitespace (every Unicode space outside ASCII is a non-ASCII char),
    // and without a table lookup per char (round 2).
    let (method, target) = if line.is_ascii() {
        let mut parts = line.split_ascii_whitespace();
        (parts.next()?, parts.next()?)
    } else {
        let mut parts = line.split_whitespace();
        (parts.next()?, parts.next()?)
    };
    if method != "GET" && method != "PUT" && method != "POST" {
        return Some(Request::Other);
    }
    let admitted = match gate {
        None => true,
        // the cookie only when the query string has no token
        Some(gate) => gate.admits_with(target, || header(text, "cookie").map(str::trim)),
    };
    let path = match target.split('?').next().unwrap_or(target) {
        "/stream" => Path::Stream,
        "/" | "/index.html" => Path::Index,
        "/update" => Path::Update,
        "/setup" => Path::Setup,
        _ => Path::Other,
    };
    if method == "PUT" {
        let content_length =
            header(text, "content-length").and_then(|value| value.trim().parse().ok());
        return Some(Request::Put {
            path,
            admitted,
            content_length,
        });
    }
    if method == "POST" {
        let content_length =
            header(text, "content-length").and_then(|value| value.trim().parse().ok());
        let setup_session = header(text, "x-setup-session")
            .map(str::trim)
            .and_then(|v| <[u8; 16]>::try_from(v.as_bytes()).ok());
        return Some(Request::Post {
            path,
            admitted,
            content_length,
            setup_session,
        });
    }
    Some(Request::Get { path, admitted })
}

/// A binary response in full: the status line, the headers, `body` as
/// `application/octet-stream`. What `/setup` answers with: the setup
/// session's message, with the HTTP status its carrier gives it (200, or
/// the 4xx an `Error` maps to).
pub fn write_octets(sink: &mut impl PacketSink, code: u16, body: &[u8]) -> Result<()> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        409 => "Conflict",
        _ => "Error",
    };
    let mut num = [0u8; 20];
    sink.write(b"HTTP/1.1 ")?;
    let n = fmt_u64(u64::from(code), &mut num);
    sink.write(n.as_bytes())?;
    sink.write(b" ")?;
    sink.write(reason.as_bytes())?;
    sink.write(
        b"\r\nContent-Type: application/octet-stream\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: ",
    )?;
    let len = fmt_u64(body.len() as u64, &mut num);
    sink.write(len.as_bytes())?;
    sink.write(b"\r\n\r\n")?;
    sink.write(body)
}

/// Where the body starts in a head buffer that may already hold some of it:
/// the index just past the blank line, if the head is complete.
#[must_use]
pub fn body_offset(head: &[u8]) -> Option<usize> {
    blank_line(head, 0)
}

/// A plain-text response in full: the status line, the headers, `body`.
/// What `/update` answers with — what happened, in one line a person or a
/// runner reads (`committed <sha256>`, or the refusal's name).
pub fn write_plain(sink: &mut impl PacketSink, code: u16, reason: &str, body: &str) -> Result<()> {
    let mut num = [0u8; 20];
    sink.write(b"HTTP/1.1 ")?;
    let n = fmt_u64(u64::from(code), &mut num);
    sink.write(n.as_bytes())?;
    sink.write(b" ")?;
    sink.write(reason.as_bytes())?;
    sink.write(
        b"\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\nContent-Length: ",
    )?;
    let len = fmt_u64(body.len() as u64, &mut num);
    sink.write(len.as_bytes())?;
    sink.write(b"\r\n\r\n")?;
    sink.write(body.as_bytes())
}

/// The responses that are not the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The request head was malformed or too large.
    BadRequest,
    /// The server is gated and the request carried no token, or the wrong one.
    Forbidden,
    /// Not a path this chip serves.
    NotFound,
    /// Not a `GET`.
    MethodNotAllowed,
    /// The stream was asked for on a chip that has no camera to stream.
    NoCamera,
}

/// The page, with `Connection: close`. When the server is gated, the token
/// rides along as a cookie so the page's own `<img src="/stream">` passes.
pub fn write_index(sink: &mut impl PacketSink, token: Option<&str>) -> Result<()> {
    let mut num = [0u8; 20];
    let len = fmt_u64(INDEX_HTML.len() as u64, &mut num);
    sink.write(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n",
    )?;
    if let Some(token) = token {
        // the browser keeps it for this origin only
        sink.write(b"Set-Cookie: ")?;
        sink.write(TOKEN_PARAM.as_bytes())?;
        sink.write(b"=")?;
        sink.write(token.as_bytes())?;
        sink.write(b"; Path=/; SameSite=Strict; HttpOnly\r\n")?;
    }
    sink.write(b"Content-Length: ")?;
    sink.write(len.as_bytes())?;
    sink.write(b"\r\n\r\n")?;
    sink.write(INDEX_HTML.as_bytes())
}

/// A complete refusal: status line, headers, and a one-line body where a
/// person might read it.
pub fn write_status(sink: &mut impl PacketSink, status: Status) -> Result<()> {
    let (line, body): (&[u8], &[u8]) = match status {
        Status::BadRequest => (b"HTTP/1.1 400 Bad Request\r\n", b""),
        Status::Forbidden => (
            b"HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\n",
            b"403 Forbidden: this page needs its token, the URL printed at boot\n",
        ),
        Status::NotFound => (b"HTTP/1.1 404 Not Found\r\n", b""),
        Status::MethodNotAllowed => (b"HTTP/1.1 405 Method Not Allowed\r\nAllow: GET\r\n", b""),
        Status::NoCamera => (
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\n",
            b"503: this device has no camera to stream\n",
        ),
    };
    let mut num = [0u8; 20];
    let len = fmt_u64(body.len() as u64, &mut num);
    sink.write(line)?;
    sink.write(b"Connection: close\r\nContent-Length: ")?;
    sink.write(len.as_bytes())?;
    sink.write(b"\r\n\r\n")?;
    if !body.is_empty() {
        sink.write(body)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The header lookup W9 replaced: the oracle.
    fn header_scan<'t>(text: &'t str, name: &str) -> Option<&'t str> {
        text.lines()
            .skip(1)
            .filter_map(|l| l.split_once(':'))
            .find(|(n, _)| n.trim().eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    }

    #[test]
    fn the_filtered_header_lookup_answers_as_the_full_scan() {
        // names with every kind of padding, Unicode spaces included
        let pads = [
            "", " ", "\t", "\u{a0}", "\u{2003}", "\u{3000}", "\u{1680}", "\u{85}", "\u{b}",
        ];
        let names = [
            "Cookie",
            "cookie",
            "COOKIE",
            "Cookies",
            "Content-Length",
            "content-length",
            "X-Cookie",
            "",
            "C",
        ];
        let mut x = 0x2545_F491u32;
        let mut pick = |n: usize| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as usize) % n
        };
        for _ in 0..4000 {
            let mut text = std::string::String::from("GET / HTTP/1.1\r\n");
            for _ in 0..pick(6) {
                let (lead, name, tail) = (
                    pads[pick(pads.len())],
                    names[pick(names.len())],
                    pads[pick(pads.len())],
                );
                let colon = if pick(9) == 0 { "" } else { ":" };
                text.push_str(&std::format!(
                    "{lead}{name}{tail}{colon} v{}\r\n",
                    pick(100)
                ));
            }
            text.push_str("\r\n");
            for name in ["cookie", "content-length"] {
                assert_eq!(header(&text, name), header_scan(&text, name), "{text:?}");
            }
        }
    }

    /// The window search W7 replaced: the oracle.
    fn blank_line_windows(head: &[u8]) -> Option<usize> {
        head.windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| i + 4)
    }

    #[test]
    fn the_blank_line_is_found_where_the_window_search_found_it() {
        let mut x = 0x2545_F491u32;
        let alphabet = *b"\r\na: ";
        for len in 0..400usize {
            for _ in 0..8 {
                let head: std::vec::Vec<u8> = (0..len)
                    .map(|_| {
                        x ^= x << 13;
                        x ^= x >> 17;
                        x ^= x << 5;
                        alphabet[(x % 5) as usize]
                    })
                    .collect();
                assert_eq!(body_offset(&head), blank_line_windows(&head), "{head:?}");
                assert_eq!(head_complete(&head), blank_line_windows(&head).is_some());
                // read by read: the incremental check agrees at every length
                // while the prefix before it was incomplete
                let mut seen = 0usize;
                for end in 0..=head.len() {
                    if blank_line_windows(&head[..seen]).is_none() {
                        assert_eq!(
                            head_complete_from(&head[..end], seen),
                            blank_line_windows(&head[..end]).is_some(),
                            "{head:?} seen {seen} end {end}"
                        );
                    }
                    seen = end;
                }
            }
        }
    }

    use super::*;
    use crate::sink::SliceSink;

    /// `parse_request` before round 2 (eager cookie, Unicode split): the
    /// oracle for the lazy cookie and the ASCII split.
    fn parse_request_before(head: &[u8], gate: Option<&Gate<'_>>) -> Option<Request> {
        let end = body_offset(head).unwrap_or(head.len());
        let text = core::str::from_utf8(&head[..end]).ok()?;
        let line = text.lines().next()?;
        let mut parts = line.split_whitespace();
        let method = parts.next()?;
        let target = parts.next()?;
        if method != "GET" && method != "PUT" && method != "POST" {
            return Some(Request::Other);
        }
        let admitted = match gate {
            None => true,
            Some(gate) => {
                let cookie = header(text, "cookie").map(str::trim);
                gate.admits(target, cookie)
            }
        };
        let path = match target.split('?').next().unwrap_or(target) {
            "/stream" => Path::Stream,
            "/" | "/index.html" => Path::Index,
            "/update" => Path::Update,
            "/setup" => Path::Setup,
            _ => Path::Other,
        };
        if method == "PUT" {
            let content_length =
                header(text, "content-length").and_then(|value| value.trim().parse().ok());
            return Some(Request::Put {
                path,
                admitted,
                content_length,
            });
        }
        if method == "POST" {
            let content_length =
                header(text, "content-length").and_then(|value| value.trim().parse().ok());
            let setup_session = header(text, "x-setup-session")
                .map(str::trim)
                .and_then(|v| <[u8; 16]>::try_from(v.as_bytes()).ok());
            return Some(Request::Post {
                path,
                admitted,
                content_length,
                setup_session,
            });
        }
        Some(Request::Get { path, admitted })
    }

    #[test]
    fn the_lazy_cookie_and_ascii_split_answer_as_before() {
        let gate = Gate::new("7Ns3kQxYbVfR2pLmZ9wHdE");
        let methods = ["GET", "PUT", "POST", "get", ""];
        let gaps = [
            " ", "  ", "\t", " \t ", "\u{a0}", "\u{2003}", "\u{3000}", "\u{85}",
        ];
        let targets = [
            "/stream?t=7Ns3kQxYbVfR2pLmZ9wHdE",
            "/stream?t=wrongwrongwrongwrongwr",
            "/stream?a=1&t=7Ns3kQxYbVfR2pLmZ9wHdE",
            "/stream",
            "/",
            "/index.html?t=7Ns3kQxYbVfR2pLmZ9wHdE",
            "/update?t=7Ns3kQxYbVfR2pLmZ9wHdE",
            "/caf\u{e9}",
            "",
        ];
        let cookies = [
            "",
            "Cookie: t=7Ns3kQxYbVfR2pLmZ9wHdE\r\n",
            "Cookie: a=b; t=7Ns3kQxYbVfR2pLmZ9wHdE\r\n",
            "cookie:t=wrongwrongwrongwrongwr\r\n",
            "Cookie:\u{a0}t=7Ns3kQxYbVfR2pLmZ9wHdE\r\n",
        ];
        let lengths = ["", "Content-Length: 12\r\n", "content-length:  x\r\n"];
        let mut n = 0;
        for m in methods {
            for g in gaps {
                for t in targets {
                    for c in cookies {
                        for l in lengths {
                            let mut head = std::format!(
                                "{m}{g}{t}{g}HTTP/1.1\r\nHost: x\r\n{c}{l}Accept: */*\r\n\r\nBODY"
                            )
                            .into_bytes();
                            // a body that is not UTF-8, as an update's is
                            head.extend_from_slice(&[0xFF, 0xFE, 0x00]);
                            for gate in [None, Some(&gate)] {
                                assert_eq!(
                                    parse_request(&head, gate),
                                    parse_request_before(&head, gate),
                                    "{:?}",
                                    std::string::String::from_utf8_lossy(&head)
                                );
                                n += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(n > 4000);
    }

    fn parse(head: &str) -> Option<Request> {
        parse_request(head.as_bytes(), None)
    }

    #[test]
    fn a_head_is_complete_at_the_blank_line_and_not_before() {
        assert!(!head_complete(b"GET / HTTP/1.1\r\nHost: x\r\n"));
        assert!(head_complete(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"));
        assert!(head_complete(b"GET / HTTP/1.1\r\n\r\nbody"));
    }

    #[test]
    fn a_put_to_update_carries_its_length_and_is_gated_like_a_get() {
        assert_eq!(
            parse("PUT /update HTTP/1.1\r\nContent-Length: 1234\r\n\r\n"),
            Some(Request::Put {
                path: Path::Update,
                admitted: true,
                content_length: Some(1234),
            })
        );
        assert_eq!(
            parse("PUT /update HTTP/1.1\r\n\r\n"),
            Some(Request::Put {
                path: Path::Update,
                admitted: true,
                content_length: None,
            })
        );
        // a PUT anywhere else is still a PUT: the server says 404 or 405
        assert_eq!(
            parse("PUT / HTTP/1.1\r\n\r\n"),
            Some(Request::Put {
                path: Path::Index,
                admitted: true,
                content_length: None,
            })
        );
        assert_eq!(
            parse("GET /update HTTP/1.1\r\n\r\n"),
            Some(Request::Get {
                path: Path::Update,
                admitted: true
            })
        );
        let gate = Gate::new("7Ns3kQxYbVfR2pLmZ9wHdE");
        let gated = |head: &str| parse_request(head.as_bytes(), Some(&gate));
        assert_eq!(
            gated("PUT /update HTTP/1.1\r\n\r\n"),
            Some(Request::Put {
                path: Path::Update,
                admitted: false,
                content_length: None
            })
        );
        assert_eq!(
            gated("PUT /update?t=7Ns3kQxYbVfR2pLmZ9wHdE HTTP/1.1\r\n\r\n"),
            Some(Request::Put {
                path: Path::Update,
                admitted: true,
                content_length: None
            })
        );
        // the body may already be in the head buffer
        assert_eq!(body_offset(b"PUT /update HTTP/1.1\r\n\r\n{\"m\""), Some(24));
        assert_eq!(body_offset(b"PUT /update HTTP/1.1\r\n"), None);
        let mut buf = [0u8; 256];
        let mut sink = SliceSink::new(&mut buf);
        write_plain(&mut sink, 200, "OK", "committed abc\n").unwrap();
        let text = core::str::from_utf8(sink.written()).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(
            text.contains("Content-Length: 14\r\n\r\ncommitted abc\n"),
            "{text}"
        );
    }

    #[test]
    fn a_binary_body_behind_the_head_does_not_spoil_the_parse() {
        let head: &[u8] = b"PUT /update HTTP/1.1\r\nContent-Length: 4\r\n\r\n\xff\xfe\x00\x01";
        assert_eq!(
            parse_request(head, None),
            Some(Request::Put {
                path: Path::Update,
                admitted: true,
                content_length: Some(4),
            })
        );
    }

    #[test]
    fn the_three_paths_and_everything_else() {
        let get = |path| {
            Some(Request::Get {
                path,
                admitted: true,
            })
        };
        assert_eq!(parse("GET / HTTP/1.1\r\n\r\n"), get(Path::Index));
        assert_eq!(parse("GET /index.html HTTP/1.1\r\n\r\n"), get(Path::Index));
        assert_eq!(parse("GET /stream HTTP/1.1\r\n\r\n"), get(Path::Stream));
        assert_eq!(
            parse("GET /stream?t=abc HTTP/1.1\r\n\r\n"),
            get(Path::Stream)
        );
        assert_eq!(parse("GET /favicon.ico HTTP/1.1\r\n\r\n"), get(Path::Other));
        // a POST is parsed (only /setup takes one; the server refuses the rest)
        assert_eq!(
            parse("POST / HTTP/1.1\r\n\r\n"),
            Some(Request::Post {
                path: Path::Index,
                admitted: true,
                content_length: None,
                setup_session: None,
            })
        );
        assert_eq!(parse("DELETE / HTTP/1.1\r\n\r\n"), Some(Request::Other));
        assert_eq!(parse("\r\n\r\n"), None);
        assert_eq!(parse("GET\r\n\r\n"), None);
        assert_eq!(parse_request(b"GET /\xff HTTP/1.1\r\n\r\n", None), None);
    }

    #[test]
    fn the_gate_reads_the_query_string_or_the_cookie_header() {
        let gate = Gate::new("7Ns3kQxYbVfR2pLmZ9wHdE");
        let admitted = |head: &str| match parse_request(head.as_bytes(), Some(&gate)) {
            Some(Request::Get { admitted, .. }) => admitted,
            other => panic!("{other:?}"),
        };
        assert!(admitted("GET /?t=7Ns3kQxYbVfR2pLmZ9wHdE HTTP/1.1\r\n\r\n"));
        assert!(admitted(
            "GET /stream HTTP/1.1\r\nCookie: a=1; t=7Ns3kQxYbVfR2pLmZ9wHdE\r\n\r\n"
        ));
        assert!(admitted(
            "GET /stream HTTP/1.1\r\ncookie: t=7Ns3kQxYbVfR2pLmZ9wHdE\r\n\r\n"
        ));
        assert!(!admitted("GET /stream HTTP/1.1\r\n\r\n"));
        assert!(!admitted("GET /?t=wrong HTTP/1.1\r\n\r\n"));
        // The token in a header that is not Cookie does not count.
        assert!(!admitted(
            "GET / HTTP/1.1\r\nX-Token: t=7Ns3kQxYbVfR2pLmZ9wHdE\r\n\r\n"
        ));
    }

    fn render(f: impl FnOnce(&mut SliceSink<'_>) -> Result<()>) -> Vec<u8> {
        let mut buf = [0u8; 2048];
        let mut sink = SliceSink::new(&mut buf);
        f(&mut sink).unwrap();
        sink.written().to_vec()
    }

    #[test]
    fn the_page_response_is_exact_and_carries_the_token_only_when_gated() {
        let plain = render(|s| write_index(s, None));
        let text = core::str::from_utf8(&plain).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head.contains("Content-Type: text/html; charset=utf-8\r\n"));
        assert!(head.contains(&format!("Content-Length: {}", INDEX_HTML.len())));
        assert!(!head.contains("Set-Cookie"));
        assert_eq!(body, INDEX_HTML);

        let gated = render(|s| write_index(s, Some("7Ns3kQxYbVfR2pLmZ9wHdE")));
        let text = core::str::from_utf8(&gated).unwrap();
        assert!(text.contains(
            "Set-Cookie: t=7Ns3kQxYbVfR2pLmZ9wHdE; Path=/; SameSite=Strict; HttpOnly\r\n"
        ));
        assert!(text.ends_with(INDEX_HTML));
    }

    #[test]
    fn every_refusal_declares_its_length_and_closes() {
        for (status, code) in [
            (Status::BadRequest, "400"),
            (Status::Forbidden, "403"),
            (Status::NotFound, "404"),
            (Status::MethodNotAllowed, "405"),
            (Status::NoCamera, "503"),
        ] {
            let bytes = render(|s| write_status(s, status));
            let text = core::str::from_utf8(&bytes).unwrap();
            let (head, body) = text.split_once("\r\n\r\n").unwrap();
            assert!(
                head.starts_with(&format!("HTTP/1.1 {code} ")),
                "{code}: {head}"
            );
            assert!(head.contains("Connection: close\r\n"), "{code}");
            assert!(
                head.contains(&format!("Content-Length: {}", body.len())),
                "{code}: {head}"
            );
        }
        assert!(
            core::str::from_utf8(&render(|s| write_status(s, Status::MethodNotAllowed)))
                .unwrap()
                .contains("Allow: GET\r\n")
        );
    }

    #[test]
    fn a_setup_post_carries_its_session_and_length() {
        let head = b"POST /setup HTTP/1.1\r\nHost: 192.168.71.1\r\nContent-Type: application/octet-stream\r\nX-Setup-Session: 00112233445566aa\r\nContent-Length: 67\r\n\r\n\x01\x01";
        assert_eq!(
            parse_request(head, None),
            Some(Request::Post {
                path: Path::Setup,
                admitted: true,
                content_length: Some(67),
                setup_session: Some(*b"00112233445566aa"),
            })
        );
        // the body starts right behind the head, bytes and all
        assert_eq!(&head[body_offset(head).unwrap()..], b"\x01\x01");
        // GET /setup is Discover
        assert_eq!(
            parse_request(b"GET /setup HTTP/1.1\r\n\r\n", None),
            Some(Request::Get {
                path: Path::Setup,
                admitted: true
            })
        );
        // a session name of another length is not passed on
        let short = b"POST /setup HTTP/1.1\r\nX-Setup-Session: 0011\r\nContent-Length: 3\r\n\r\n";
        assert!(matches!(
            parse_request(short, None),
            Some(Request::Post {
                setup_session: None,
                ..
            })
        ));
        // a POST elsewhere is still a POST, which the server refuses
        assert!(matches!(
            parse_request(b"POST /stream HTTP/1.1\r\n\r\n", None),
            Some(Request::Post {
                path: Path::Stream,
                ..
            })
        ));
    }

    #[test]
    fn a_setup_answer_is_octets_with_its_status() {
        let mut buf = [0u8; 256];
        let mut sink = crate::sink::SliceSink::new(&mut buf);
        write_octets(&mut sink, 409, &[1, 0x7f, 3]).unwrap();
        let n = sink.len();
        let text = &buf[..n];
        assert!(text.starts_with(b"HTTP/1.1 409 Conflict\r\n"));
        let content_type = b"Content-Type: application/octet-stream";
        assert!(text.windows(content_type.len()).any(|w| w == content_type));
        assert!(text.ends_with(b"Content-Length: 3\r\n\r\n\x01\x7f\x03"));
    }
}
