//! The receiving end over `std::net`: fetch a device's `/stream` and hand
//! each JPEG to the caller — the Pi hub's record path, the host tooling, the
//! bridge's ingest.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use rusty_esp_video_core::esp_core::error::{Error, Result};
use rusty_esp_video_core::esp_core::time::Micros;
use rusty_esp_video_core::mjpeg_reader::Reader;

/// What the client saw.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PullStats {
    /// Parts delivered to the callback.
    pub frames: u64,
    /// Payload bytes delivered.
    pub bytes: u64,
    /// First and last timestamps carried by the parts, when present.
    pub first_timestamp: Option<Micros>,
    /// Last timestamp.
    pub last_timestamp: Option<Micros>,
}

/// Connect to `addr`, request `path`, and call `on_frame` for each JPEG until
/// `max_frames` have arrived or the server closes. `buf` must hold one whole
/// part.
pub fn pull_stream(
    addr: impl ToSocketAddrs,
    path: &str,
    max_frames: u64,
    buf: &mut [u8],
    mut on_frame: impl FnMut(&[u8], Option<Micros>) -> Result<()>,
) -> Result<PullStats> {
    let mut stream = TcpStream::connect(addr).map_err(|_| Error::Hardware)?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: janus\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .map_err(|_| Error::Hardware)?;
    let mut reader = Reader::new(buf);
    let mut stats = PullStats::default();
    let mut chunk = [0u8; 4096];
    while stats.frames < max_frames {
        let n = stream.read(&mut chunk).map_err(|_| Error::Hardware)?;
        if n == 0 {
            break;
        }
        reader.push(&chunk[..n])?;
        while let Some(part) = reader.next_part()? {
            on_frame(reader.part(&part), part.timestamp)?;
            stats.frames += 1;
            stats.bytes += part.len() as u64;
            if stats.first_timestamp.is_none() {
                stats.first_timestamp = part.timestamp;
            }
            stats.last_timestamp = part.timestamp;
            reader.release(&part);
            if stats.frames >= max_frames {
                break;
            }
        }
        if reader.is_done() {
            break;
        }
    }
    Ok(stats)
}
