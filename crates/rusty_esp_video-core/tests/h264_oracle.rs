//! The J5 kill test, host half: the `H264` encoder behind the `VideoEncoder`
//! seam produces a Constrained Baseline stream at QVGA that muxes to MPEG-TS,
//! decodes in `ffmpeg`/`ffprobe` with the right picture count, and
//! round-trips through the house decoder — one access unit per input frame,
//! key frames exactly where the GOP says.
//!
//! Also prints the per-frame encode time on this host (`--release
//! -- --nocapture`), which the ledger records as the **host** baseline the
//! S3 number will be compared against. Run:
//!
//! `cargo test -p rusty_esp_video-core --features h264 --release --test h264_oracle -- --nocapture`
#![cfg(feature = "h264")]

use std::process::Command;
use std::time::Instant;

use rusty_esp_core::time::Micros;
use rusty_esp_core::{Frame, Geometry, PixelFormat, Plane, Planes};
use rusty_esp_video_core::annexb::contains_idr;
use rusty_esp_video_core::mpegts::{Mux, STREAM_TYPE_H264, demux};
use rusty_esp_video_core::{Codec, EncoderConfig, H264, MediaPacket, VideoEncoder};
use rusty_h264::Decoder;

const W: u32 = 320;
const H: u32 = 240;
const FRAMES: usize = 30;
const GOP: u16 = 15;
const FPS: u8 = 15;
const FRAME_MICROS: u64 = 1_000_000 / FPS as u64;

/// One I420 frame: a bright square moving four columns per frame over a
/// gradient background, so there is real motion and real texture.
fn i420(n: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (w, h) = (W as usize, H as usize);
    let mut y = vec![0u8; w * h];
    for row in 0..h {
        for col in 0..w {
            y[row * w + col] = (16 + ((row + col) % 160)) as u8;
        }
    }
    let x0 = (n * 4) % (w - 32);
    let y0 = 40 + (n * 2) % 100;
    for row in y0..y0 + 32 {
        for col in x0..x0 + 32 {
            y[row * w + col] = 235;
        }
    }
    let (cw, ch) = (w / 2, h / 2);
    let u = vec![128u8; cw * ch];
    let v = vec![110u8; cw * ch];
    (y, u, v)
}

fn frame<'a>(n: usize, y: &'a [u8], u: &'a [u8], v: &'a [u8]) -> Frame<'a> {
    let (w, h) = (W as usize, H as usize);
    Frame {
        geometry: Geometry::new(W, H, PixelFormat::Yuv420p).unwrap(),
        timestamp: Micros(n as u64 * FRAME_MICROS),
        sequence: n as u32,
        planes: Planes::Planar {
            y: Plane::new(y, w, h, w).unwrap(),
            u: Plane::new(u, w / 2, h / 2, w / 2).unwrap(),
            v: Plane::new(v, w / 2, h / 2, w / 2).unwrap(),
        },
    }
}

struct Encoded {
    aus: Vec<(Vec<u8>, bool)>,
    micros: Vec<u64>,
}

fn encode_all() -> Encoded {
    let mut enc = H264::new();
    let geometry = Geometry::new(W, H, PixelFormat::Yuv420p).unwrap();
    enc.configure(
        geometry,
        &EncoderConfig {
            bitrate_kbps: 0,
            fps: FPS,
            gop: GOP,
            quality: 80,
        },
    )
    .unwrap();
    let mut out = vec![0u8; 256 * 1024];
    let mut aus = Vec::new();
    let mut micros = Vec::new();
    for n in 0..FRAMES {
        let (y, u, v) = i420(n);
        let f = frame(n, &y, &u, &v);
        let t = Instant::now();
        let pkt = enc.encode(&f, &mut out).expect("encode");
        micros.push(t.elapsed().as_micros() as u64);
        assert_eq!(pkt.codec, Codec::H264);
        assert!(
            !pkt.data.is_empty(),
            "frame {n} produced no bytes: no lookahead buffering allowed"
        );
        aus.push((pkt.data.to_vec(), pkt.key));
    }
    let mut tail = vec![0u8; 64 * 1024];
    let n = enc.flush(&mut tail).unwrap();
    assert_eq!(n, 0, "nothing should be buffered with lookahead 0");
    assert_eq!(enc.frames(), FRAMES as u32);
    Encoded { aus, micros }
}

#[test]
fn one_access_unit_per_frame_with_key_frames_on_the_gop() {
    let e = encode_all();
    assert_eq!(e.aus.len(), FRAMES);
    for (n, (au, key)) in e.aus.iter().enumerate() {
        assert_eq!(
            contains_idr(au),
            *key,
            "frame {n}: key flag matches the bytes"
        );
        let expect_key = n % GOP as usize == 0;
        assert_eq!(*key, expect_key, "frame {n}: IDR exactly on the GOP");
    }
    let mut sorted = e.micros.clone();
    sorted.sort_unstable();
    let total: u64 = e.micros.iter().sum();
    println!(
        "h264 host baseline: {W}x{H} Constrained Baseline CAVLC, Fast, GOP {GOP}, {FRAMES} frames: \
         min {} us, median {} us, max {} us per frame; {} bytes total ({} B/frame); build {}",
        sorted[0],
        sorted[sorted.len() / 2],
        sorted[sorted.len() - 1],
        e.aus.iter().map(|(a, _)| a.len()).sum::<usize>(),
        e.aus.iter().map(|(a, _)| a.len()).sum::<usize>() / FRAMES,
        if cfg!(debug_assertions) {
            "debug (timing meaningless)"
        } else {
            "release"
        },
    );
    let _ = total;
}

#[test]
fn request_keyframe_yields_an_idr_on_the_next_frame() {
    let mut enc = H264::new();
    enc.configure(
        Geometry::new(W, H, PixelFormat::Yuv420p).unwrap(),
        &EncoderConfig {
            bitrate_kbps: 0,
            fps: FPS,
            gop: 250,
            quality: 60,
        },
    )
    .unwrap();
    let mut out = vec![0u8; 256 * 1024];
    let mut keys = Vec::new();
    for n in 0..6 {
        if n == 3 {
            enc.request_keyframe();
        }
        let (y, u, v) = i420(n);
        let f = frame(n, &y, &u, &v);
        keys.push(enc.encode(&f, &mut out).unwrap().key);
    }
    assert_eq!(keys, [true, false, false, true, false, false]);
}

#[test]
fn wrong_pixel_format_or_odd_geometry_is_refused() {
    let mut enc = H264::new();
    let cfg = EncoderConfig::default();
    assert!(
        enc.configure(Geometry::new(W, H, PixelFormat::Rgb565).unwrap(), &cfg)
            .is_err()
    );
    // The core refuses an odd width for 4:2:0 before the encoder ever sees it.
    assert!(Geometry::new(321, H, PixelFormat::Yuv420p).is_err());
    let mut small = [0u8; 16];
    enc.configure(Geometry::new(W, H, PixelFormat::Yuv420p).unwrap(), &cfg)
        .unwrap();
    let (y, u, v) = i420(0);
    let f = frame(0, &y, &u, &v);
    assert!(matches!(
        enc.encode(&f, &mut small),
        Err(rusty_esp_core::Error::BufferTooSmall { .. })
    ));
}

#[test]
fn the_stream_muxes_to_ts_decodes_in_the_house_decoder_and_in_ffprobe() {
    let e = encode_all();
    let mut ts = Vec::new();
    {
        let mut mux = Mux::new(&mut ts);
        for (i, (au, key)) in e.aus.iter().enumerate() {
            let pkt = MediaPacket::new(Codec::H264, *key, Micros(i as u64 * FRAME_MICROS), au);
            mux.push(&pkt).unwrap();
        }
    }
    assert!(!ts.is_empty());

    // The demuxer gives back one access unit per picture, and the house
    // decoder decodes them (it may hold the last picture until a flush, as the
    // V0 oracle allows).
    let report = demux::parse(&ts).expect("well-formed transport stream");
    assert_eq!(report.stream_type, Some(STREAM_TYPE_H264));
    assert_eq!(report.cc_errors, [0, 0, 0]);
    assert_eq!(report.access_units.len(), FRAMES);
    let mut dec = Decoder::new();
    let mut decoded = 0;
    for au in &report.access_units {
        if dec.decode(au).expect("house decoder").is_some() {
            decoded += 1;
        }
    }
    assert!(decoded >= FRAMES - 1, "house decoder pictures: {decoded}");

    // ffprobe, when present, is the external oracle.
    let dir = std::env::temp_dir().join("janus-h264-oracle");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("qvga.ts");
    std::fs::write(&path, &ts).unwrap();
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,profile,width,height,nb_read_frames",
            "-of",
            "csv=p=0",
        ])
        .arg(&path)
        .output();
    match probe {
        Ok(o) if o.status.success() => {
            let line = String::from_utf8_lossy(&o.stdout).trim().to_string();
            println!("ffprobe: {line}");
            assert!(line.starts_with("h264,"), "{line}");
            assert!(line.contains("Constrained Baseline"), "profile: {line}");
            assert!(line.ends_with(&format!(",{W},{H},{FRAMES}")), "{line}");
            let dec = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args(["-f", "null", "-"])
                .output()
                .unwrap();
            assert!(dec.status.success());
            assert!(
                dec.stderr.is_empty(),
                "ffmpeg errors: {}",
                String::from_utf8_lossy(&dec.stderr)
            );
        }
        _ => {
            assert!(
                std::env::var_os("JANUS_REQUIRE_FFPROBE").is_none(),
                "ffprobe missing but JANUS_REQUIRE_FFPROBE is set"
            );
            eprintln!("ffprobe not found: external oracle skipped");
        }
    }
}
