//! The V0 kill test: a transport stream muxed on the host from a real H.264
//! elementary stream decodes in `ffmpeg`/`ffprobe` with the right frame count,
//! and round-trips through the house decoder.
//!
//! The fixture is made by `rusty_h264` (the house encoder, scalar build), so
//! the whole chain is pure Rust; `ffprobe` is the external oracle and runs
//! when it is on the PATH (set `JANUS_REQUIRE_FFPROBE=1` to make its absence
//! a failure, as CI on a box with ffmpeg should).

use std::process::Command;

use rusty_esp_core::time::Micros;
use rusty_esp_video_core::annexb::{access_units, contains_idr};
use rusty_esp_video_core::mpegts::{Mux, STREAM_TYPE_H264, demux};
use rusty_esp_video_core::{Codec, MediaPacket};
use rusty_h264::{Decoder, Encoder, EncoderConfig, YuvFrame};

const W: usize = 64;
const H: usize = 48;
const FRAMES: usize = 12;
const FPS_MICROS: u64 = 40_000; // 25 fps

/// Encode `FRAMES` frames of a moving pattern; returns one Annex-B access
/// unit per picture with its key flag. The encoder may buffer and return
/// several pictures at once, so the concatenated output is split.
fn fixture() -> Vec<(Vec<u8>, bool)> {
    let mut enc = Encoder::new(EncoderConfig::new(W as _, H as _)).expect("encoder config");
    let mut stream = Vec::new();
    for n in 0..FRAMES {
        let mut frame = YuvFrame::black(W, H);
        // a bright square that moves three columns per frame
        let x0 = (n * 3) % (W - 16);
        for y in 8..24usize {
            for x in x0..x0 + 16 {
                frame.y[y * W + x] = 235;
            }
        }
        stream.extend_from_slice(&enc.encode(&frame));
    }
    stream.extend_from_slice(&enc.flush());
    access_units(&stream)
        .map(|au| (au.to_vec(), contains_idr(au)))
        .collect()
}

fn mux_fixture(aus: &[(Vec<u8>, bool)]) -> Vec<u8> {
    let mut ts = Vec::new();
    let mut mux = Mux::new(&mut ts);
    for (i, (au, key)) in aus.iter().enumerate() {
        let pkt = MediaPacket::new(Codec::H264, *key, Micros(i as u64 * FPS_MICROS), au);
        mux.push(&pkt).unwrap();
    }
    ts
}

fn decode_all(aus: impl Iterator<Item = impl AsRef<[u8]>>) -> usize {
    let mut dec = Decoder::new();
    let mut decoded = 0;
    for au in aus {
        if dec.decode(au.as_ref()).expect("decodes").is_some() {
            decoded += 1;
        }
    }
    decoded
}

#[test]
fn fixture_is_one_access_unit_per_picture_and_the_house_decoder_accepts_it() {
    let aus = fixture();
    assert_eq!(aus.len(), FRAMES, "one access unit per encoded picture");
    assert!(aus[0].1, "first access unit is an IDR");
    let decoded = decode_all(aus.iter().map(|(au, _)| au));
    assert!(
        decoded >= FRAMES - 1,
        "decoded {decoded} pictures of {FRAMES}"
    );
}

#[test]
fn ts_round_trips_byte_identical_and_decodes_with_the_house_decoder() {
    let aus = fixture();
    let ts = mux_fixture(&aus);
    let report = demux::parse(&ts).expect("well-formed transport stream");
    assert_eq!(report.stream_type, Some(STREAM_TYPE_H264));
    assert_eq!(report.cc_errors, [0, 0, 0]);
    assert_eq!(report.access_units.len(), aus.len());
    assert_eq!(report.pcr.len(), aus.len(), "one PCR per access unit");
    for (i, au) in report.access_units.iter().enumerate() {
        // AUD prepended, then the original bytes
        assert!(au.ends_with(&aus[i].0), "AU {i} carries the original bytes");
        assert_eq!(report.pts[i], (i as u64 * FPS_MICROS) * 9 / 100);
    }
    let decoded = decode_all(report.access_units.iter());
    assert!(
        decoded >= aus.len() - 1,
        "decoded {decoded} of {}",
        aus.len()
    );
}

#[test]
fn ffprobe_counts_the_frames_and_ffmpeg_decodes_cleanly() {
    let aus = fixture();
    let ts = mux_fixture(&aus);
    let dir = std::env::temp_dir().join("rusty_esp_video_ts_oracle");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("fixture.ts");
    std::fs::write(&path, &ts).unwrap();

    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            "stream=codec_name,width,height,nb_read_frames",
            "-of",
            "csv=p=0",
        ])
        .arg(&path)
        .output();
    let out = match probe {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if std::env::var_os("JANUS_REQUIRE_FFPROBE").is_some() {
                panic!("ffprobe is required and not on PATH");
            }
            eprintln!("ffprobe not on PATH; external oracle skipped");
            return;
        }
        Err(e) => panic!("ffprobe failed to start: {e}"),
    };
    assert!(
        out.status.success(),
        "ffprobe: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let raw = String::from_utf8_lossy(&out.stdout).to_string();
    eprintln!("ffprobe says: {raw:?}");
    // First line: codec_name,width,height,nb_read_frames (ffprobe may append
    // further section lines; only the stream line matters).
    let text = raw.lines().next().unwrap_or("").trim().to_string();
    let fields: Vec<&str> = text
        .split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .collect();
    assert!(fields.len() >= 4, "unexpected ffprobe output: {text}");
    assert_eq!(fields[0], "h264", "{text}");
    assert_eq!(fields[1], W.to_string(), "{text}");
    assert_eq!(fields[2], H.to_string(), "{text}");
    let frames: usize = fields[3]
        .parse()
        .unwrap_or_else(|e| panic!("frame count {:?}: {e} ({text})", fields[3]));
    assert_eq!(
        frames,
        aus.len(),
        "ffprobe counted {frames} frames; muxed {}",
        aus.len()
    );

    let dec = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&path)
        .args(["-f", "null", "-"])
        .output()
        .expect("ffmpeg runs");
    assert!(dec.status.success());
    assert!(
        dec.stderr.is_empty(),
        "ffmpeg reported: {}",
        String::from_utf8_lossy(&dec.stderr)
    );
    eprintln!(
        "transport packets: {}; bytes: {}; access units: {}",
        ts.len() / 188,
        ts.len(),
        aus.len()
    );
}
