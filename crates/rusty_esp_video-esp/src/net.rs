//! The Track A stream server: `std::net` sockets and a minimal HTTP/1.1
//! responder for the MJPEG stream — the CameraWebServer, remade.
//!
//! Deliberately small: one connection at a time (a chip streams to one viewer
//! in v1), `GET /stream` answers with the multipart stream until the client
//! goes away, `GET /` answers with a page that shows it, everything else is
//! `404`. Requests are read into a fixed buffer; nothing here allocates per
//! frame.
//!
//! The protocol — which request is which, what each response says — is
//! `rusty_esp_video_core::http`, shared with the Track B firmware that
//! serves the same page over embassy-net. This module is the `std` I/O
//! around it: the listener, the read loop, the TCP sink.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use rusty_esp_video_core::esp_core::error::{Error, Result};
use rusty_esp_video_core::http::{self, Path, Request, Status, StatusQuery};
pub use rusty_esp_video_core::http::{INDEX_HTML, MAX_REQUEST_BYTES, STREAM_PATH};
use rusty_esp_video_core::mjpeg_http::{Gate, Multipart};
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
    /// A `403`: the server is gated and the request carried no token, or the
    /// wrong one.
    Forbidden,
    /// The request head was malformed or too large; a `400` was sent.
    BadRequest,
    /// `/v1/status`, served.
    Status,
}

/// One connection [`MjpegHttpServer::accept_one`] took.
#[derive(Debug)]
pub enum Accepted {
    /// Answered in full: what it was.
    Answered(Served),
    /// An admitted `/stream`, not yet started: [`StreamConn::run`] it.
    Stream(StreamConn),
}

/// An admitted `/stream` connection, handed back to run anywhere: on the
/// accepting thread ([`MjpegHttpServer::serve_one`]) or on one of its own.
#[derive(Debug)]
pub struct StreamConn {
    sink: TcpSink,
}

impl StreamConn {
    /// Push frames from `source` until the viewer leaves (the end, not an
    /// error) or the source fails (returned).
    pub fn run(
        self,
        source: &mut impl PacketSource,
        scratch: &mut [u8],
        stats: &mut ServeStats,
    ) -> Result<Served> {
        let mut mp = Multipart::new(self.sink);
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
}

/// What `/v1/status` serves: called with the request's nonce (`None` for the
/// plain status), it returns the JSON body. The firmware's, so this server
/// names nothing a status holds.
pub type StatusHandler = Arc<dyn Fn(Option<[u8; 32]>) -> String + Send + Sync>;

/// The MJPEG HTTP server.
pub struct MjpegHttpServer {
    listener: TcpListener,
    /// `/v1/status`, when the firmware gives one.
    status: Option<StatusHandler>,
    /// Read timeout for the request head.
    pub request_timeout: Duration,
    /// The viewing token `/` and `/stream` require, if any.
    token: Option<String>,
}

impl core::fmt::Debug for MjpegHttpServer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MjpegHttpServer")
            .field("listener", &self.listener)
            .field("request_timeout", &self.request_timeout)
            .field("gated", &self.token.is_some())
            .field("status", &self.status.is_some())
            .finish()
    }
}

impl MjpegHttpServer {
    /// Bind to `addr` (for example `0.0.0.0:80` on a chip, `127.0.0.1:0` in a test).
    pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
        Ok(MjpegHttpServer {
            listener: TcpListener::bind(addr)?,
            request_timeout: Duration::from_secs(5),
            token: None,
            status: None,
        })
    }

    /// Serve `/v1/status` from `handler`, ungated: a home's hub reads it to
    /// find and check the device, and has no page token. Without it the
    /// path is a `404`.
    #[must_use]
    pub fn with_status(mut self, handler: StatusHandler) -> Self {
        self.status = Some(handler);
        self
    }

    /// Require `token` on `/` and `/stream` ([`Gate`]): `?t=<token>` on the
    /// URL, or the `t` cookie the index page sets when opened that way, so
    /// the page's own `<img src="/stream">` passes. `/v1/status` is never
    /// gated; everything else is `403`.
    #[must_use]
    pub fn gated(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// The token this server requires, if it is gated.
    #[must_use]
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
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
        match self.accept_one(stats)? {
            Accepted::Answered(served) => Ok(served),
            Accepted::Stream(conn) => conn.run(source, scratch, stats),
        }
    }

    /// Take the next connection and answer it, unless it is an admitted
    /// `/stream`: that comes back as a [`StreamConn`] for the caller to run
    /// wherever it likes, so a viewer with the page open never holds up
    /// `/v1/status`, the page or a refusal. [`serve_one`](Self::serve_one) is
    /// this with the stream run in place.
    pub fn accept_one(&self, stats: &mut ServeStats) -> Result<Accepted> {
        let (stream, _) = self.listener.accept().map_err(|_| Error::Hardware)?;
        stats.connections += 1;
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(self.request_timeout));
        let gate = self.token.as_deref().map(Gate::new);
        let mut sink = TcpSink(stream);
        let Some(request) = read_request(&mut sink.0, gate.as_ref()) else {
            let _ = http::write_status(&mut sink, Status::BadRequest);
            stats.other += 1;
            return Ok(Accepted::Answered(Served::BadRequest));
        };
        match request {
            // ungated, ahead of the gate: the hub has no page token
            Request::Get {
                path: Path::Status(query),
                ..
            } => {
                stats.other += 1;
                match (&self.status, query) {
                    (None, _) => {
                        let _ = http::write_status(&mut sink, Status::NotFound);
                        Ok(Accepted::Answered(Served::NotFound))
                    }
                    (Some(_), StatusQuery::Invalid) => {
                        let _ = http::write_status(&mut sink, Status::BadRequest);
                        Ok(Accepted::Answered(Served::BadRequest))
                    }
                    (Some(handler), query) => {
                        let nonce = match query {
                            StatusQuery::Nonce(n) => Some(n),
                            _ => None,
                        };
                        let body = handler(nonce);
                        let _ = http::write_json(&mut sink, 200, body.as_bytes());
                        Ok(Accepted::Answered(Served::Status))
                    }
                }
            }
            Request::Get {
                admitted: false, ..
            } => {
                let _ = http::write_status(&mut sink, Status::Forbidden);
                stats.other += 1;
                Ok(Accepted::Answered(Served::Forbidden))
            }
            Request::Get {
                path: Path::Stream, ..
            } => Ok(Accepted::Stream(StreamConn { sink })),
            Request::Get {
                path: Path::Index, ..
            } => {
                let _ = http::write_index(&mut sink, self.token.as_deref());
                stats.other += 1;
                Ok(Accepted::Answered(Served::Index))
            }
            // the setup page is a Track B cell's (E7)
            Request::Get {
                path: Path::Other | Path::Setup,
                ..
            } => {
                let _ = http::write_status(&mut sink, Status::NotFound);
                stats.other += 1;
                Ok(Accepted::Answered(Served::NotFound))
            }
            // this server serves GETs; a signed update (`PUT /update`) is the
            // Track B page's, and a Track A cell takes its updates elsewhere
            Request::Get {
                path: Path::Update, ..
            }
            | Request::Put { .. }
            | Request::Post { .. } => {
                let _ = http::write_status(&mut sink, Status::MethodNotAllowed);
                stats.other += 1;
                Ok(Accepted::Answered(Served::MethodNotAllowed))
            }
            Request::Other => {
                let _ = http::write_status(&mut sink, Status::MethodNotAllowed);
                stats.other += 1;
                Ok(Accepted::Answered(Served::MethodNotAllowed))
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

/// Read the request head into a fixed buffer and parse it. `None` is a
/// `400`: the head did not complete within [`MAX_REQUEST_BYTES`], the
/// connection ended first, or what arrived was not a request.
fn read_request(stream: &mut TcpStream, gate: Option<&Gate<'_>>) -> Option<Request> {
    let mut head = [0u8; MAX_REQUEST_BYTES];
    let mut len = 0usize;
    while !http::head_complete(&head[..len]) {
        if len >= head.len() {
            return None;
        }
        let n = stream.read(&mut head[len..]).ok()?;
        if n == 0 {
            return None;
        }
        len += n;
    }
    http::parse_request(&head[..len], gate)
}
