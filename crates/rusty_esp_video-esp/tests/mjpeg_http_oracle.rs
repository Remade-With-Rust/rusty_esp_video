//! The J1 stream-path kill test on the host: the exact server the board will
//! run, fed by colour bars encoded to JPEG, read back by a Rust client and by
//! `ffmpeg`'s MJPEG-over-HTTP demuxer (what a browser does), with the frame
//! count checked.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::sync::mpsc;
use std::thread;

use rusty_esp_core::error::{Error, Result};
use rusty_esp_core::frame::{Frame, Geometry, PixelFormat};
use rusty_esp_image_core::jpeg;
use rusty_esp_image_core::source::{ImageSource, TestPattern};
use rusty_esp_video_core::encoder::{EncoderConfig, Passthrough, VideoEncoder};
use rusty_esp_video_core::source::EncodedSource;
use rusty_esp_video_esp::net::{MjpegHttpServer, ServeStats, Served};

const W: u32 = 160;
const H: u32 = 120;

/// Colour bars → JPEG, standing in for a sensor in JPEG mode. Not wall-clock
/// paced: the server must not depend on the source's timing.
struct JpegPattern {
    pattern: TestPattern,
    rgb: Vec<u8>,
    jpeg: Vec<u8>,
    seq: u32,
}

impl JpegPattern {
    fn new() -> Self {
        let g = Geometry::new(W, H, PixelFormat::Rgb888).unwrap();
        JpegPattern {
            pattern: TestPattern::new(g, 30).unwrap(),
            rgb: vec![0u8; g.byte_len().unwrap()],
            jpeg: Vec::new(),
            seq: 0,
        }
    }
}

impl ImageSource for JpegPattern {
    fn geometry(&self) -> Geometry {
        Geometry::new(W, H, PixelFormat::Jpeg).unwrap()
    }

    fn grab<'b>(&mut self, out: &'b mut [u8]) -> Result<Frame<'b>> {
        // Copy the metadata out so the raw frame's borrow of `rgb` ends here.
        let timestamp = self.pattern.grab(&mut self.rgb)?.timestamp;
        let geometry = self.geometry();
        self.jpeg.clear();
        let enc = rusty_jpeg::encode::Encoder::new(&mut self.jpeg, 75);
        enc.encode(
            &self.rgb,
            W as u16,
            H as u16,
            rusty_jpeg::encode::ColorType::Rgb,
        )
        .map_err(|_| Error::Hardware)?;
        out[..self.jpeg.len()].copy_from_slice(&self.jpeg);
        let f = Frame::packed(geometry, timestamp, self.seq, &out[..self.jpeg.len()])?;
        self.seq += 1;
        Ok(f)
    }
}

/// Start a server on an ephemeral port that serves exactly `connections`
/// connections, then reports its stats.
fn spawn_server(connections: usize) -> (String, mpsc::Receiver<(ServeStats, Vec<Served>)>) {
    let server = MjpegHttpServer::bind("127.0.0.1:0").unwrap();
    let addr = server.local_addr().unwrap().to_string();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let camera = JpegPattern::new();
        let mut enc = Passthrough::default();
        enc.configure(camera.geometry(), &EncoderConfig::default())
            .unwrap();
        let mut source = EncodedSource::new(camera, enc, 32 * 1024, 0).unwrap();
        let mut scratch = vec![0u8; 32 * 1024 + 1];
        let mut stats = ServeStats::default();
        let mut served = Vec::new();
        for _ in 0..connections {
            served.push(
                server
                    .serve_one(&mut source, &mut scratch, &mut stats)
                    .unwrap(),
            );
        }
        tx.send((stats, served)).unwrap();
    });
    (addr, rx)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[test]
fn rust_client_reads_valid_jpeg_parts_and_index_and_404() {
    let (addr, rx) = spawn_server(3);

    // 1. the stream: read the head and five parts, then hang up
    let mut s = TcpStream::connect(&addr).unwrap();
    s.write_all(b"GET /stream HTTP/1.1\r\nHost: janus\r\n\r\n")
        .unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut parts = 0;
    while parts < 5 {
        let n = s.read(&mut chunk).unwrap();
        assert!(n > 0, "server closed early");
        buf.extend_from_slice(&chunk[..n]);
        parts = buf.windows(13).filter(|w| *w == b"--janus-frame").count();
    }
    drop(s);
    let head_end = find(&buf, b"\r\n\r\n").unwrap();
    let head = std::str::from_utf8(&buf[..head_end]).unwrap();
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(
        head.contains("multipart/x-mixed-replace; boundary=janus-frame"),
        "{head}"
    );
    // every complete part is a JPEG of the right geometry
    let body = &buf[head_end + 4..];
    let mut i = 0;
    let mut checked = 0;
    while let Some(p) = find(&body[i..], b"--janus-frame\r\n") {
        let start = i + p + 15;
        let Some(h) = find(&body[start..], b"\r\n\r\n") else {
            break;
        };
        let headers = std::str::from_utf8(&body[start..start + h]).unwrap();
        let len: usize = headers
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        let data_start = start + h + 4;
        if body.len() < data_start + len {
            break;
        }
        let img = &body[data_start..data_start + len];
        let info = jpeg::probe(img).unwrap();
        assert_eq!((info.geometry.width, info.geometry.height), (W, H));
        assert_eq!(jpeg::find_eoi(img), Some(len), "part is exactly one JPEG");
        checked += 1;
        i = data_start + len;
    }
    assert!(checked >= 4, "checked {checked} parts");

    // 2. the index page
    let mut s = TcpStream::connect(&addr).unwrap();
    s.write_all(b"GET / HTTP/1.1\r\nHost: janus\r\n\r\n")
        .unwrap();
    let mut page = Vec::new();
    s.read_to_end(&mut page).unwrap();
    let page = String::from_utf8(page).unwrap();
    assert!(page.starts_with("HTTP/1.1 200 OK"));
    assert!(page.contains("text/html"));
    assert!(page.contains("<img src=\"/stream\""));

    // 3. a 404
    let mut s = TcpStream::connect(&addr).unwrap();
    s.write_all(b"GET /nope HTTP/1.1\r\nHost: janus\r\n\r\n")
        .unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    assert!(resp.starts_with(b"HTTP/1.1 404"));

    let (stats, served) = rx.recv().unwrap();
    assert_eq!(stats.connections, 3);
    assert_eq!(stats.streams, 1);
    assert!(
        matches!(served[0], Served::Stream { frames } if frames >= 5),
        "{:?}",
        served[0]
    );
    assert_eq!(served[1], Served::Index);
    assert_eq!(served[2], Served::NotFound);
    assert!(stats.frames >= 5);
}

#[test]
fn ffmpeg_reads_the_stream_as_mjpeg_and_counts_frames() {
    let (addr, rx) = spawn_server(1);
    let url = format!("http://{addr}/stream");
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-i",
            &url,
            "-frames:v",
            "8",
            "-f",
            "framecrc",
            "-",
        ])
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if std::env::var_os("JANUS_REQUIRE_FFPROBE").is_some() {
                panic!("ffmpeg is required and not on PATH");
            }
            eprintln!("ffmpeg not on PATH; external oracle skipped");
            // unblock the server thread
            let _ = TcpStream::connect(&addr).map(|mut s| s.write_all(b"GET /x HTTP/1.1\r\n\r\n"));
            return;
        }
        Err(e) => panic!("ffmpeg failed to start: {e}"),
    };
    assert!(
        out.status.success(),
        "ffmpeg: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "ffmpeg reported: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let frames = text.lines().filter(|l| l.starts_with("0,")).count();
    assert_eq!(frames, 8, "framecrc lines:\n{text}");
    let (stats, served) = rx.recv().unwrap();
    assert!(
        matches!(served[0], Served::Stream { frames } if frames >= 8),
        "{:?}",
        served[0]
    );
    eprintln!(
        "ffmpeg read 8 MJPEG frames; server pushed {} before the client hung up",
        stats.frames
    );
}
