//! Record a device's MJPEG stream to disk — the Pi hub's record path
//! (pi-mission H3) without `rff`, which cannot read MJPEG over HTTP yet.
//!
//! ```sh
//! cargo run -p rusty_esp_video-esp --features std --example mjpeg_record -- 192.168.1.50:80 out.mjpeg 300
//! ffprobe -f mjpeg -count_frames -show_entries stream=nb_read_frames -of csv=p=0 out.mjpeg
//! ```
//!
//! The output is a raw concatenation of JPEGs, which ffmpeg reads as `-f mjpeg`.

use std::io::Write;

use rusty_esp_core::error::{Error, Result};
use rusty_esp_video_esp::client::pull_stream;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:8080".to_string());
    let out_path = args.next().unwrap_or_else(|| "out.mjpeg".to_string());
    let frames: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(100);

    let mut out = std::fs::File::create(&out_path).map_err(|_| Error::Hardware)?;
    let mut buf = vec![0u8; 512 * 1024];
    let started = std::time::Instant::now();
    let stats = pull_stream(&addr, "/stream", frames, &mut buf, |jpeg, _ts| {
        out.write_all(jpeg).map_err(|_| Error::Hardware)
    })?;
    let secs = started.elapsed().as_secs_f32().max(0.001);
    eprintln!(
        "recorded {} frames ({} bytes) to {out_path} in {secs:.1}s = {:.1} fps; device timestamps {:?}..{:?}",
        stats.frames,
        stats.bytes,
        stats.frames as f32 / secs,
        stats.first_timestamp,
        stats.last_timestamp
    );
    Ok(())
}
