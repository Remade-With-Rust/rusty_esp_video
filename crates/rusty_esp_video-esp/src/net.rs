//! The Track A stream server: `std::net` sockets and a minimal HTTP/1.1
//! responder for the MJPEG stream — the CameraWebServer, remade.
//!
//! Deliberately small: one connection at a time (a chip streams to one viewer
//! in v1), `GET /stream` answers with the multipart stream until the client
//! goes away, `GET /` answers with a page that shows it, everything else is
//! `404`. Requests are read into a fixed buffer; nothing here allocates per
//! frame.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::time::Duration;

use rusty_esp_video_core::esp_core::error::{Error, Result};
use rusty_esp_video_core::mjpeg_http::Multipart;
use rusty_esp_video_core::sink::PacketSink;
use rusty_esp_video_core::source::PacketSource;

/// A [`PacketSink`] over a TCP connection.
#[derive(Debug)]
pub struct TcpSink(pub TcpStream);

impl PacketSink for TcpSink {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.0.write_all(bytes).map_err(|_| Error::Hardware)
    }
}

/// The path that streams.
pub const STREAM_PATH: &str = "/stream";

/// Largest request head accepted.
pub const MAX_REQUEST_BYTES: usize = 2048;

const INDEX_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>janus</title>\
<style>html,body{margin:0;background:#000;height:100%}img{display:block;max-width:100vw;max-height:100vh;margin:auto}</style>\
</head><body><img src=\"/stream\" alt=\"stream\"></body></html>";

/// Counters the server keeps across connections.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ServeStats {
    /// Connections accepted.
    pub connections: u64,
    /// Stream requests served (to the end of the connection).
    pub streams: u64,
    /// Frames pushed across all streams.
    pub frames: u64,
    /// Requests answered with a non-stream response (index, 404, 405).
    pub other: u64,
}

/// What one connection turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Served {
    /// The multipart stream, ended by the client (or a source error), after
    /// this many frames.
    Stream {
        /// Frames pushed on this connection.
        frames: u64,
    },
    /// The index page.
    Index,
    /// A `404`.
    NotFound,
    /// A `405` (non-GET).
    MethodNotAllowed,
    /// The request head was malformed or too large; a `400` was sent.
    BadRequest,
}

/// The MJPEG HTTP server.
#[derive(Debug)]
pub struct MjpegHttpServer {
    listener: TcpListener,
    /// Read timeout for the request head.
    pub request_timeout: Duration,
}

impl MjpegHttpServer {
    /// Bind to `addr` (for example `0.0.0.0:80` on a chip, `127.0.0.1:0` in a test).
    pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
        Ok(MjpegHttpServer {
            listener: TcpListener::bind(addr)?,
            request_timeout: Duration::from_secs(5),
        })
    }

    /// Where the server listens.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept one connection and serve it to completion. For `/stream` that
    /// means pushing frames from `source` until the client disconnects or the
    /// source fails; a source failure is returned, a client disconnect is not.
    pub fn serve_one(
        &self,
        source: &mut impl PacketSource,
        scratch: &mut [u8],
        stats: &mut ServeStats,
    ) -> Result<Served> {
        let (stream, _) = self.listener.accept().map_err(|_| Error::Hardware)?;
        stats.connections += 1;
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(self.request_timeout));
        let mut stream = stream;
        let request = match read_request(&mut stream) {
            Ok(r) => r,
            Err(()) => {
                let _ = stream.write_all(
                    b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                );
                stats.other += 1;
                return Ok(Served::BadRequest);
            }
        };
        match request {
            Request::Get(path) if path == STREAM_PATH => {
                let mut mp = Multipart::new(TcpSink(stream));
                if mp.write_response_head().is_err() {
                    return Ok(Served::Stream { frames: 0 });
                }
                stats.streams += 1;
                let mut frames = 0u64;
                loop {
                    let packet = source.next_packet(scratch)?;
                    match mp.push(&packet) {
                        Ok(()) => {
                            frames += 1;
                            stats.frames += 1;
                        }
                        // The viewer closed the tab: not an error, the end.
                        Err(Error::Hardware) => break,
                        Err(e) => return Err(e),
                    }
                }
                Ok(Served::Stream { frames })
            }
            Request::Get(path) if path == "/" || path == "/index.html" => {
                let mut head = [0u8; 20];
                let len = rusty_esp_video_core::fmt_u64_pub(INDEX_HTML.len() as u64, &mut head);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: ");
                let _ = stream.write_all(len.as_bytes());
                let _ = stream.write_all(b"\r\n\r\n");
                let _ = stream.write_all(INDEX_HTML.as_bytes());
                stats.other += 1;
                Ok(Served::Index)
            }
            Request::Get(_) => {
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                );
                stats.other += 1;
                Ok(Served::NotFound)
            }
            Request::Other => {
                let _ = stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nAllow: GET\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
                stats.other += 1;
                Ok(Served::MethodNotAllowed)
            }
        }
    }

    /// Serve connections forever; returns only on a source error.
    pub fn serve(
        &self,
        source: &mut impl PacketSource,
        scratch: &mut [u8],
        stats: &mut ServeStats,
    ) -> Result<()> {
        loop {
            self.serve_one(source, scratch, stats)?;
        }
    }
}

enum Request<'a> {
    Get(&'a str),
    Other,
}

/// Read the request head into a fixed buffer and pick out method and path.
fn read_request(stream: &mut TcpStream) -> std::result::Result<Request<'static>, ()> {
    // The path is copied into a leaked-free static buffer? No: we return a
    // borrowed view over a thread-local scratch instead.
    thread_local! {
        static HEAD: std::cell::RefCell<[u8; MAX_REQUEST_BYTES]> = const { std::cell::RefCell::new([0; MAX_REQUEST_BYTES]) };
    }
    HEAD.with(|cell| {
        let mut head = cell.borrow_mut();
        let mut len = 0usize;
        loop {
            if len >= head.len() {
                return Err(());
            }
            let n = stream.read(&mut head[len..]).map_err(|_| ())?;
            if n == 0 {
                return Err(());
            }
            len += n;
            if head[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let text = core::str::from_utf8(&head[..len]).map_err(|_| ())?;
        let line = text.lines().next().ok_or(())?;
        let mut parts = line.split_whitespace();
        let method = parts.next().ok_or(())?;
        let path = parts.next().ok_or(())?;
        if method != "GET" {
            return Ok(Request::Other);
        }
        // Strip a query string; the path is short, so copy it into a static
        // pool of known paths rather than allocate.
        let path = path.split('?').next().unwrap_or(path);
        Ok(match path {
            "/stream" => Request::Get(STREAM_PATH),
            "/" => Request::Get("/"),
            "/index.html" => Request::Get("/index.html"),
            _ => Request::Get("/<other>"),
        })
    })
}
