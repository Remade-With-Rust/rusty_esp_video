//! V2 sender: colour bars → JPEG → RTP/JPEG (or raw Janus datagrams) to a
//! laptop, paced at a frame rate; with `--h264`, a moving YUV pattern →
//! `rusty_h264` behind the seam → RTP (RFC 6184) instead.
//!
//! ```sh
//! cargo run -p rusty_esp_video-esp --features std --release --example rtp_send -- 192.168.0.224:5004 10 600
//! cargo run -p rusty_esp_video-esp --features std --release --example rtp_send -- 192.168.0.224:5006 10 600 --raw
//! cargo run -p rusty_esp_video-esp --features std,h264 --release --example rtp_send -- 127.0.0.1:5004 15 30 --h264
//! ```
//!
//! Arguments: destination, frames per second (default 10), seconds to run
//! (default 0 = forever), `--raw` for the Janus datagram framing instead of
//! RTP, `--h264` for H.264 over RTP (payload type 96, the encoder's
//! Constrained Baseline stream), `--kbps N` to cap the JPEG stream (frames
//! that do not fit are dropped whole and counted). Everything below the
//! encoder is what the chip runs. The receivers: `rtp_recv` for JPEG,
//! `rff -i rtp://@:5004` (`?pt=26` or `?pt=96`; an rff older than
//! remade_ffmpeg_rs `de24a83` needs `0.0.0.0` in place of `@` on Windows) or
//! ffmpeg with an SDP for either.

use std::time::{Duration, Instant};

use rusty_esp_core::error::{Error, Result};
use rusty_esp_core::frame::{Geometry, PixelFormat};
use rusty_esp_core::time::Micros;
use rusty_esp_image_core::source::{ImageSource, TestPattern};
use rusty_esp_video_core::packet::{Codec, MediaPacket};
use rusty_esp_video_esp::udp_net::{RawUdpSender, RtpJpegSender};

const MTU: usize = 1200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Jpeg,
    Raw,
    H264,
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let dest = args.next().unwrap_or_else(|| "127.0.0.1:5004".to_string());
    let mut fps: u32 = 10;
    let mut secs: u64 = 0;
    let mut mode = Mode::Jpeg;
    let mut kbps: u32 = 0;
    let mut want_kbps = false;
    let mut positional = 0;
    for a in args {
        if want_kbps {
            kbps = a.parse().unwrap_or(0);
            want_kbps = false;
        } else if a == "--kbps" {
            want_kbps = true;
        } else if a == "--raw" {
            mode = Mode::Raw;
        } else if a == "--h264" {
            mode = Mode::H264;
        } else if positional == 0 {
            fps = a.parse().unwrap_or(10);
            positional += 1;
        } else {
            secs = a.parse().unwrap_or(0);
        }
    }
    let fps = fps.max(1);
    if mode == Mode::H264 {
        return h264::run(&dest, fps, secs);
    }
    let g = Geometry::new(320, 240, PixelFormat::Rgb888)?;
    let mut pattern = TestPattern::new(g, fps)?;
    let mut rgb = vec![0u8; g.byte_len().ok_or(Error::Unsupported)?];
    let mut jpeg = Vec::new();
    let raw = mode == Mode::Raw;
    let mut rtp = (!raw)
        .then(|| RtpJpegSender::bind("0.0.0.0:0", &dest, 0x4A41_4E55, MTU))
        .transpose()?
        .map(|s| {
            if kbps > 0 {
                s.with_budget(kbps, 500)
            } else {
                s
            }
        });
    let mut rawtx = raw
        .then(|| RawUdpSender::bind("0.0.0.0:0", &dest, MTU))
        .transpose()?
        .map(|s| {
            if kbps > 0 {
                s.with_budget(kbps, 500)
            } else {
                s
            }
        });
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
        "send {}s: frames={} dropped={} packets={} bytes={} ({:.2} fps, {:.1} kbit/s{})",
        started.elapsed().as_secs(),
        s.frames,
        s.dropped,
        s.packets,
        s.bytes,
        s.frames as f64 / started.elapsed().as_secs_f64(),
        s.bytes as f64 * 8.0 / 1000.0 / started.elapsed().as_secs_f64(),
        if kbps > 0 {
            format!(", cap {kbps}")
        } else {
            String::new()
        }
    );
    Ok(())
}

/// The H.264 arm: a moving planar pattern through `encoder::H264` (the chip
/// configuration, `rusty_h264` 0.14's borrowed-plane, write-in-place path)
/// and the RFC 6184 payloader, one datagram per RTP packet.
#[cfg(feature = "h264")]
mod h264 {
    use std::net::UdpSocket;

    use super::*;
    use rusty_esp_core::frame::{Frame, Plane, Planes};
    use rusty_esp_video_core::encoder::{EncoderConfig, VideoEncoder};
    use rusty_esp_video_core::h264::H264;
    use rusty_esp_video_core::rtp::{H264Payloader, Rtp};

    const W: u32 = 320;
    const H: u32 = 240;
    /// The dynamic payload type the receivers' SDP names.
    const PT: u8 = 96;

    /// Fill a planar 4:2:0 frame: a luma ramp with a bar that moves one
    /// macroblock per frame, chroma tinted by position — something the
    /// encoder has to work for, and a decoder can be seen to follow.
    fn paint(y: &mut [u8], u: &mut [u8], v: &mut [u8], n: u32) {
        let (w, h) = (W as usize, H as usize);
        let bar = ((n * 16) % W) as usize;
        for row in 0..h {
            for col in 0..w {
                let ramp = ((col * 200) / w + (row * 40) / h) as u8;
                let in_bar = col >= bar && col < bar + 32;
                y[row * w + col] = if in_bar { 235 } else { 16 + ramp };
            }
        }
        for row in 0..h / 2 {
            for col in 0..w / 2 {
                u[row * (w / 2) + col] = (64 + (col * 128) / (w / 2)) as u8;
                v[row * (w / 2) + col] = (64 + (row * 128) / (h / 2)) as u8;
            }
        }
    }

    pub fn run(dest: &str, fps: u32, secs: u64) -> Result<()> {
        let g = Geometry::new(W, H, PixelFormat::Yuv420p)?;
        let mut enc = H264::new();
        enc.configure(
            g,
            &EncoderConfig {
                fps: fps.min(255) as u8,
                gop: 15,
                ..EncoderConfig::default()
            },
        )?;
        let (w, h) = (W as usize, H as usize);
        let mut yp = vec![0u8; w * h];
        let mut up = vec![0u8; w * h / 4];
        let mut vp = vec![0u8; w * h / 4];
        let mut out = vec![0u8; w * h * 3 / 2];
        let mut scratch = vec![0u8; MTU];
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|_| Error::Hardware)?;
        socket.connect(dest).map_err(|_| Error::Hardware)?;
        let mut payloader = H264Payloader::new(Rtp::new(0x4A41_4E55, PT, 90_000, 1), MTU)?;
        eprintln!(
            "sending RTP/H.264 (pt {PT}) to {dest} at {fps} fps{}",
            if secs > 0 {
                format!(" for {secs} s")
            } else {
                String::new()
            }
        );
        let started = Instant::now();
        let interval = Duration::from_micros(1_000_000 / u64::from(fps));
        let mut n = 0u32;
        let (mut packets, mut bytes, mut keys) = (0u64, 0u64, 0u64);
        loop {
            let due = started + interval * n;
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            if secs > 0 && started.elapsed() >= Duration::from_secs(secs) {
                break;
            }
            paint(&mut yp, &mut up, &mut vp, n);
            let ts = Micros(started.elapsed().as_micros() as u64);
            let frame = Frame {
                geometry: g,
                timestamp: ts,
                sequence: n,
                planes: Planes::Planar {
                    y: Plane::new(&yp, w, h, w)?,
                    u: Plane::new(&up, w / 2, h / 2, w / 2)?,
                    v: Plane::new(&vp, w / 2, h / 2, w / 2)?,
                },
            };
            let packet = enc.encode(&frame, &mut out)?;
            keys += u64::from(packet.key);
            let sent = payloader.packetize(packet.data, ts, &mut scratch, |datagram| {
                socket.send(datagram).map_err(|_| Error::Hardware)?;
                bytes += datagram.len() as u64;
                Ok(())
            })?;
            packets += sent as u64;
            n += 1;
        }
        println!(
            "send {}s: frames={n} keys={keys} packets={packets} bytes={bytes} ({:.2} fps, {:.1} kbit/s)",
            started.elapsed().as_secs(),
            f64::from(n) / started.elapsed().as_secs_f64(),
            bytes as f64 * 8.0 / 1000.0 / started.elapsed().as_secs_f64(),
        );
        Ok(())
    }
}

#[cfg(not(feature = "h264"))]
mod h264 {
    use super::*;

    pub fn run(_dest: &str, _fps: u32, _secs: u64) -> Result<()> {
        eprintln!("--h264 needs `--features std,h264`");
        Err(Error::Unsupported)
    }
}
