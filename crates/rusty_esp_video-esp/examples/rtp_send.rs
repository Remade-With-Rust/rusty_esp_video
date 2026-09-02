//! V2 sender: colour bars → JPEG → RTP/JPEG (or raw Janus datagrams) to a
//! laptop, paced at a frame rate.
//!
//! ```sh
//! cargo run -p rusty_esp_video-esp --features std --release --example rtp_send -- 192.168.0.224:5004 10 600
//! cargo run -p rusty_esp_video-esp --features std --release --example rtp_send -- 192.168.0.224:5006 10 600 --raw
//! ```
//!
//! Arguments: destination, frames per second (default 10), seconds to run
//! (default 0 = forever), `--raw` for the Janus datagram framing instead of
//! RTP. Everything below the JPEG encoder is what the chip runs.

use std::time::{Duration, Instant};

use rusty_esp_core::error::{Error, Result};
use rusty_esp_core::frame::{Geometry, PixelFormat};
use rusty_esp_core::time::Micros;
use rusty_esp_image_core::source::{ImageSource, TestPattern};
use rusty_esp_video_core::packet::{Codec, MediaPacket};
use rusty_esp_video_esp::udp_net::{RawUdpSender, RtpJpegSender};

const MTU: usize = 1200;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let dest = args.next().unwrap_or_else(|| "127.0.0.1:5004".to_string());
    let mut fps: u32 = 10;
    let mut secs: u64 = 0;
    let mut raw = false;
    let mut positional = 0;
    for a in args {
        if a == "--raw" {
            raw = true;
        } else if positional == 0 {
            fps = a.parse().unwrap_or(10);
            positional += 1;
        } else {
            secs = a.parse().unwrap_or(0);
        }
    }
    let fps = fps.max(1);
    let g = Geometry::new(320, 240, PixelFormat::Rgb888)?;
    let mut pattern = TestPattern::new(g, fps)?;
    let mut rgb = vec![0u8; g.byte_len().ok_or(Error::Unsupported)?];
    let mut jpeg = Vec::new();
    let mut rtp = (!raw)
        .then(|| RtpJpegSender::bind("0.0.0.0:0", &dest, 0x4A41_4E55, MTU))
        .transpose()?;
    let mut rawtx = raw
        .then(|| RawUdpSender::bind("0.0.0.0:0", &dest, MTU))
        .transpose()?;
    eprintln!(
        "sending {} to {dest} at {fps} fps{}",
        if raw { "raw datagrams" } else { "RTP/JPEG" },
        if secs > 0 {
            format!(" for {secs} s")
        } else {
            String::new()
        }
    );
    let started = Instant::now();
    let interval = Duration::from_micros(1_000_000 / u64::from(fps));
    let mut seq = 0u64;
    let mut last_report = Instant::now();
    loop {
        let due = started + interval * (seq as u32);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        if secs > 0 && started.elapsed() >= Duration::from_secs(secs) {
            break;
        }
        let (w, h) = {
            let frame = pattern.grab(&mut rgb)?;
            (frame.geometry.width as u16, frame.geometry.height as u16)
        };
        jpeg.clear();
        let enc = rusty_jpeg::encode::Encoder::new(&mut jpeg, 80);
        enc.encode(&rgb, w, h, rusty_jpeg::encode::ColorType::Rgb)
            .map_err(|_| Error::Hardware)?;
        let ts = Micros(started.elapsed().as_micros() as u64);
        if let Some(tx) = rtp.as_mut() {
            tx.send_frame(&jpeg, ts)?;
        }
        if let Some(tx) = rawtx.as_mut() {
            tx.send_packet(&MediaPacket::new(Codec::Jpeg, true, ts, &jpeg))?;
        }
        seq += 1;
        if last_report.elapsed() >= Duration::from_secs(10) {
            let s = rtp
                .as_ref()
                .map(|t| t.stats)
                .or(rawtx.as_ref().map(|t| t.stats))
                .unwrap_or_default();
            eprintln!(
                "{:>5} s: frames={} packets={} bytes={} ({:.1} fps, {} B this frame)",
                started.elapsed().as_secs(),
                s.frames,
                s.packets,
                s.bytes,
                s.frames as f64 / started.elapsed().as_secs_f64(),
                jpeg.len()
            );
            last_report = Instant::now();
        }
    }
    let s = rtp
        .as_ref()
        .map(|t| t.stats)
        .or(rawtx.as_ref().map(|t| t.stats))
        .unwrap_or_default();
    println!(
        "send {}s: frames={} packets={} bytes={} ({:.2} fps)",
        started.elapsed().as_secs(),
        s.frames,
        s.packets,
        s.bytes,
        s.frames as f64 / started.elapsed().as_secs_f64()
    );
    Ok(())
}
