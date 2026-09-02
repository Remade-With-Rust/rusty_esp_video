//! The J1 stream path on a laptop: colour bars → JPEG → `/stream`.
//!
//! ```sh
//! cargo run -p rusty_esp_video-esp --features std --example mjpeg_server -- 127.0.0.1:8080
//! # then open http://127.0.0.1:8080/ in a browser, or:
//! ffmpeg -i http://127.0.0.1:8080/stream -frames:v 30 -f null -
//! ```
//!
//! Everything below the JPEG encoder is the code the XIAO ESP32-S3 Sense runs;
//! on the board the encoder is the sensor.

use rusty_esp_core::error::{Error, Result};
use rusty_esp_core::frame::{Frame, Geometry, PixelFormat};
use rusty_esp_core::time::Micros;
use rusty_esp_image_core::source::{ImageSource, TestPattern};
use rusty_esp_video_core::encoder::{EncoderConfig, Passthrough, VideoEncoder};
use rusty_esp_video_core::source::EncodedSource;
use rusty_esp_video_esp::net::{MjpegHttpServer, ServeStats};

/// The test pattern, JPEG-encoded on the host by `rusty_jpeg`: stands in for a
/// sensor in JPEG mode.
struct JpegPattern {
    pattern: TestPattern,
    rgb: Vec<u8>,
    jpeg: Vec<u8>,
    quality: u8,
    frame_micros: u64,
    seq: u32,
    started: std::time::Instant,
}

impl JpegPattern {
    fn new(width: u32, height: u32, fps: u32, quality: u8) -> Result<Self> {
        let g = Geometry::new(width, height, PixelFormat::Rgb888)?;
        Ok(JpegPattern {
            pattern: TestPattern::new(g, fps)?,
            rgb: vec![0u8; g.byte_len().ok_or(Error::Unsupported)?],
            jpeg: Vec::new(),
            quality,
            frame_micros: 1_000_000 / u64::from(fps),
            seq: 0,
            started: std::time::Instant::now(),
        })
    }
}

impl ImageSource for JpegPattern {
    fn geometry(&self) -> Geometry {
        let g = self.pattern.geometry();
        Geometry::new(g.width, g.height, PixelFormat::Jpeg).expect("same dimensions")
    }

    fn grab<'b>(&mut self, out: &'b mut [u8]) -> Result<Frame<'b>> {
        // Pace like a real sensor: one frame per interval of wall time.
        let due = self.started
            + std::time::Duration::from_micros(self.frame_micros * u64::from(self.seq));
        if let Some(wait) = due.checked_duration_since(std::time::Instant::now()) {
            std::thread::sleep(wait);
        }
        let (w, h) = {
            let raw = self.pattern.grab(&mut self.rgb)?;
            (raw.geometry.width as u16, raw.geometry.height as u16)
        };
        let geometry = self.geometry();
        self.jpeg.clear();
        let enc = rusty_jpeg::encode::Encoder::new(&mut self.jpeg, self.quality);
        enc.encode(&self.rgb, w, h, rusty_jpeg::encode::ColorType::Rgb)
            .map_err(|_| Error::Hardware)?;
        if out.len() < self.jpeg.len() {
            return Err(Error::BufferTooSmall {
                needed: self.jpeg.len(),
            });
        }
        out[..self.jpeg.len()].copy_from_slice(&self.jpeg);
        let ts = Micros(self.started.elapsed().as_micros() as u64);
        let frame = Frame::packed(geometry, ts, self.seq, &out[..self.jpeg.len()])?;
        self.seq = self.seq.wrapping_add(1);
        Ok(frame)
    }
}

fn main() -> Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());
    let server = MjpegHttpServer::bind(&addr).map_err(|_| Error::Hardware)?;
    let local = server.local_addr().map_err(|_| Error::Hardware)?;
    eprintln!("serving http://{local}/  (stream at http://{local}/stream)");

    let camera = JpegPattern::new(320, 240, 15, 80)?;
    let mut enc = Passthrough::default();
    enc.configure(camera.geometry(), &EncoderConfig::default())?;
    let mut source = EncodedSource::new(camera, enc, 64 * 1024, 0)?;
    let mut scratch = vec![0u8; 64 * 1024 + 1];
    let mut stats = ServeStats::default();
    loop {
        let served = server.serve_one(&mut source, &mut scratch, &mut stats)?;
        eprintln!("{served:?}  totals: {stats:?}");
    }
}
