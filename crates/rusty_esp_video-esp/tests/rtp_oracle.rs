//! V2 oracles: our RTP/JPEG against ffmpeg's, both directions, and the raw
//! datagram path over loopback.
//!
//! ffmpeg is the external oracle (RFC 2435 in `rtpdec_jpeg` / `rtpenc_jpeg`)
//! and runs when it is on the PATH; set `JANUS_REQUIRE_FFMPEG=1` to make its
//! absence a failure, as CI on a box with ffmpeg should.

use std::io::Read;
use std::net::UdpSocket;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rusty_esp_core::frame::{Geometry, PixelFormat};
use rusty_esp_core::time::Micros;
use rusty_esp_image_core::jpeg;
use rusty_esp_image_core::source::{ImageSource, TestPattern};
use rusty_esp_video_core::packet::{Codec, MediaPacket};
use rusty_esp_video_esp::udp_net::{
    receive_raw, receive_rtp_jpeg, RawUdpSender, RtpJpegSender, Until,
};

const W: u32 = 320;
const H: u32 = 240;

/// The two ffmpeg tests each bind RTP ports; one at a time keeps them apart.
static FFMPEG: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn ffmpeg_available() -> bool {
    match Command::new("ffmpeg").arg("-version").output() {
        Ok(o) if o.status.success() => true,
        _ => {
            if std::env::var_os("JANUS_REQUIRE_FFMPEG").is_some() {
                panic!("ffmpeg is required (JANUS_REQUIRE_FFMPEG is set) and not on the PATH");
            }
            eprintln!("ffmpeg not on the PATH; oracle skipped");
            false
        }
    }
}

/// `n` colour-bar frames as JPEGs, the way the sender example makes them.
fn pattern_jpegs(n: usize) -> Vec<Vec<u8>> {
    let g = Geometry::new(W, H, PixelFormat::Rgb888).unwrap();
    let mut pattern = TestPattern::new(g, 10).unwrap();
    let mut rgb = vec![0u8; g.byte_len().unwrap()];
    (0..n)
        .map(|_| {
            pattern.grab(&mut rgb).unwrap();
            let mut jpeg = Vec::new();
            let enc = rusty_jpeg::encode::Encoder::new(&mut jpeg, 80);
            enc.encode(&rgb, W as u16, H as u16, rusty_jpeg::encode::ColorType::Rgb)
                .unwrap();
            jpeg
        })
        .collect()
}

fn free_udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("janus-rtp-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// ffmpeg's RFC 2435 receiver reassembles what our payloader sends and
/// decodes ten frames of it.
#[test]
fn ffmpeg_reassembles_and_decodes_our_rtp_jpeg() {
    if !ffmpeg_available() {
        return;
    }
    let _serial = FFMPEG.lock().unwrap_or_else(|e| e.into_inner());
    let port = free_udp_port();
    let dir = temp_dir("sdp");
    let sdp = dir.join("janus.sdp");
    std::fs::write(
        &sdp,
        format!(
            "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=janus\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=video {port} RTP/AVP 26\r\na=rtpmap:26 JPEG/90000\r\n"
        ),
    )
    .unwrap();
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-protocol_whitelist",
            "file,rtp,udp",
            "-i",
        ])
        .arg(&sdp)
        .args(["-frames:v", "10", "-f", "framecrc", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ffmpeg");
    // Send colour bars at 10 fps until ffmpeg has what it wants.
    let stop = Arc::new(AtomicBool::new(false));
    let sender = {
        let stop = stop.clone();
        thread::spawn(move || {
            let frames = pattern_jpegs(20);
            let mut tx =
                RtpJpegSender::bind("127.0.0.1:0", ("127.0.0.1", port), 0x1234, 1200).unwrap();
            let started = Instant::now();
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) && started.elapsed() < Duration::from_secs(40) {
                let jpeg = &frames[(i as usize) % frames.len()];
                tx.send_frame(jpeg, Micros(i * 100_000)).unwrap();
                i += 1;
                thread::sleep(Duration::from_millis(100));
            }
            tx.stats
        })
    };
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    let status = child.wait().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    stop.store(true, Ordering::Relaxed);
    let sent = sender.join().unwrap();
    let frames = stdout.lines().filter(|l| l.starts_with("0,")).count();
    assert!(status.success(), "ffmpeg exit {status}: {stderr}");
    assert_eq!(frames, 10, "framecrc lines:\n{stdout}\nstderr:\n{stderr}");
    assert!(stderr.trim().is_empty(), "ffmpeg complained: {stderr}");
    assert!(sent.frames >= 10, "sent {} frames", sent.frames);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Our depayloader rebuilds what ffmpeg's RFC 2435 packetiser sends, and the
/// result is a JPEG both ffmpeg and the house decoder read.
#[test]
fn our_receiver_rebuilds_ffmpegs_rtp_jpeg() {
    if !ffmpeg_available() {
        return;
    }
    let _serial = FFMPEG.lock().unwrap_or_else(|e| e.into_inner());
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={W}x{H}:rate=10"),
            "-frames:v",
            "30",
            "-pix_fmt",
            "yuvj420p",
            "-c:v",
            "mjpeg",
            "-huffman",
            "default",
            "-f",
            "rtp",
            &format!("rtp://127.0.0.1:{port}"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ffmpeg");
    let mut buf = vec![0u8; 512 * 1024];
    let mut first: Option<Vec<u8>> = None;
    let mut dims = Vec::new();
    let stats = receive_rtp_jpeg(
        &socket,
        &mut buf,
        Until {
            frames: 20,
            for_at_most: Duration::from_secs(20),
        },
        |jpeg, _ts, w, h| {
            dims.push((w, h));
            first.get_or_insert_with(|| jpeg.to_vec());
        },
    )
    .unwrap();
    let _ = child.kill();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let _ = child.wait();
    assert!(stats.frames >= 15, "{stats:?}\nffmpeg: {stderr}");
    assert_eq!(
        (stats.lost, stats.dropped, stats.bad),
        (0, 0, 0),
        "{stats:?}"
    );
    assert!(dims.iter().all(|&d| d == (W as u16, H as u16)), "{dims:?}");
    let jpeg = first.expect("a frame");
    let info = jpeg::probe(&jpeg).unwrap();
    assert_eq!((info.geometry.width, info.geometry.height), (W, H));
    let mut d = rusty_jpeg::Decoder::new(&jpeg[..]);
    let px = d
        .decode()
        .expect("house decoder reads the regenerated JPEG");
    assert_eq!(px.len(), (W * H * 3) as usize);
    let dir = temp_dir("rx");
    let path = dir.join("first.jpg");
    std::fs::write(&path, &jpeg).unwrap();
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&path)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Our own two ends, pixel for pixel: what the receiver writes decodes to the
/// same image as what the sender was given.
#[test]
fn our_two_ends_agree_pixel_for_pixel() {
    let frames = pattern_jpegs(3);
    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut tx = RtpJpegSender::bind("127.0.0.1:0", rx.local_addr().unwrap(), 7, 700).unwrap();
    for (i, f) in frames.iter().enumerate() {
        tx.send_frame(f, Micros(i as u64 * 100_000)).unwrap();
    }
    let mut buf = vec![0u8; 256 * 1024];
    let mut got = Vec::new();
    let stats = receive_rtp_jpeg(
        &rx,
        &mut buf,
        Until {
            frames: 3,
            for_at_most: Duration::from_secs(5),
        },
        |jpeg, ts, _w, _h| got.push((ts, jpeg.to_vec())),
    )
    .unwrap();
    assert_eq!(stats.frames, 3);
    assert_eq!(stats.packets, tx.stats.packets);
    assert_eq!((stats.lost, stats.dropped, stats.bad), (0, 0, 0));
    for (i, (ts, regen)) in got.iter().enumerate() {
        assert_eq!(*ts, (i as u32) * 9000, "90 kHz timestamp of frame {i}");
        let a = rusty_jpeg::Decoder::new(&frames[i][..]).decode().unwrap();
        let b = rusty_jpeg::Decoder::new(&regen[..]).decode().unwrap();
        assert!(a == b, "frame {i} decodes differently after the wire");
    }
}

/// The raw Janus datagram path over loopback, byte for byte.
#[test]
fn raw_datagrams_round_trip_with_the_janus_header() {
    let frames = pattern_jpegs(4);
    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut tx = RawUdpSender::bind("127.0.0.1:0", rx.local_addr().unwrap(), 1200).unwrap();
    for (i, f) in frames.iter().enumerate() {
        tx.send_packet(&MediaPacket::new(
            Codec::Jpeg,
            i == 0,
            Micros(i as u64 * 100_000),
            f,
        ))
        .unwrap();
    }
    let mut buf = vec![0u8; 256 * 1024];
    let mut got = Vec::new();
    let stats = receive_raw(
        &rx,
        &mut buf,
        Until {
            frames: 4,
            for_at_most: Duration::from_secs(5),
        },
        |h, bytes| got.push((h.seq, h.timestamp, h.key, h.codec, bytes.to_vec())),
    )
    .unwrap();
    assert_eq!(stats.frames, 4);
    assert_eq!(stats.packets, tx.stats.packets);
    assert_eq!((stats.lost, stats.bad), (0, 0));
    for (i, (seq, ts, key, codec, bytes)) in got.iter().enumerate() {
        assert_eq!(*seq as usize, i);
        assert_eq!(ts.0, i as u64 * 100_000);
        assert_eq!(*key, i == 0);
        assert_eq!(*codec, Codec::Jpeg.tag());
        assert_eq!(bytes, &frames[i]);
    }
}
