//! V2 receiver: the laptop end of `rtp_send`. Reassembles RTP/JPEG (or raw
//! Janus datagrams), counts what was lost, and can write every frame to a
//! directory.
//!
//! ```sh
//! cargo run -p rusty_esp_video-esp --features std --release --example rtp_recv -- 0.0.0.0:5004 600 out/
//! cargo run -p rusty_esp_video-esp --features std --release --example rtp_recv -- 0.0.0.0:5006 600 --raw
//! ```
//!
//! Arguments: bind address, seconds to run (default 10), an optional output
//! directory, `--raw` for the Janus datagram framing.

use std::net::UdpSocket;
use std::path::PathBuf;
use std::time::Duration;

use rusty_esp_core::error::{Error, Result};
use rusty_esp_video_esp::udp_net::{receive_raw, receive_rtp_jpeg, Until};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let bind = args.next().unwrap_or_else(|| "0.0.0.0:5004".to_string());
    let mut secs: u64 = 10;
    let mut out: Option<PathBuf> = None;
    let mut raw = false;
    let mut positional = 0;
    for a in args {
        if a == "--raw" {
            raw = true;
        } else if positional == 0 {
            secs = a.parse().unwrap_or(10);
            positional += 1;
        } else {
            out = Some(PathBuf::from(a));
        }
    }
    if let Some(d) = &out {
        std::fs::create_dir_all(d).map_err(|_| Error::Hardware)?;
    }
    let socket = UdpSocket::bind(&bind).map_err(|_| Error::Hardware)?;
    eprintln!(
        "receiving {} on {bind} for {secs} s{}",
        if raw { "raw datagrams" } else { "RTP/JPEG" },
        out.as_ref()
            .map(|d| format!(", frames to {}", d.display()))
            .unwrap_or_default()
    );
    let until = Until {
        frames: 0,
        for_at_most: Duration::from_secs(secs),
    };
    let mut buf = vec![0u8; 512 * 1024];
    let mut written = 0u64;
    let mut largest = 0usize;
    let mut smallest = usize::MAX;
    let mut n = 0u64;
    let mut write = |bytes: &[u8]| {
        largest = largest.max(bytes.len());
        smallest = smallest.min(bytes.len());
        if let Some(d) = &out {
            if std::fs::write(d.join(format!("frame-{n:06}.jpg")), bytes).is_ok() {
                written += 1;
            }
        }
        n += 1;
        if n % 100 == 0 {
            eprintln!("{n} frames");
        }
    };
    let stats = if raw {
        receive_raw(&socket, &mut buf, until, |_h, bytes| write(bytes))?
    } else {
        receive_rtp_jpeg(&socket, &mut buf, until, |jpeg, _ts, _w, _h| write(jpeg))?
    };
    println!(
        "recv {secs}s: packets={} frames={} lost={} dropped={} bad={} bytes={} written={} frame_bytes={}..{} ({:.2} fps)",
        stats.packets,
        stats.frames,
        stats.lost,
        stats.dropped,
        stats.bad,
        stats.bytes,
        written,
        if smallest == usize::MAX { 0 } else { smallest },
        largest,
        stats.fps()
    );
    Ok(())
}
