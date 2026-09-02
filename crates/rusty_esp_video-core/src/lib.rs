#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
//! `rusty_esp_video-core` — the pure heart of `rusty_esp_video`.
//!
//! Espressif's video stack (`esp_video`, `esp_h264`, the CameraWebServer
//! stream, the ESP-WebRTC capture pipeline) remade as one seam and a set of
//! packetizers: a frame goes into a [`VideoEncoder`] and comes out as a
//! [`MediaPacket`]; a packetizer turns packets into bytes a browser, `rff`,
//! a Pi hub or the mesh can read; a [`PacketSink`] is wherever those bytes go.
//!
//! | Module | Holds |
//! |---|---|
//! | [`packet`] | [`MediaPacket`], [`Codec`] — the one packet type the family shares |
//! | [`annexb`] | H.264 Annex-B NAL unit scanning (start codes, IDR and AUD detection) |
//! | [`encoder`] | [`VideoEncoder`], [`EncoderConfig`], the zero-copy JPEG [`Passthrough`] |
//! | [`sink`] | [`PacketSink`] and the host/test sinks |
//! | [`source`] | [`PacketSource`] and [`EncodedSource`] — an `ImageSource` joined to a `VideoEncoder`, paced |
//! | [`mjpeg_http`] | `multipart/x-mixed-replace` — what a browser opens |
//! | [`mjpeg_reader`] | the receiving side: a streaming multipart parser over a caller buffer |
//! | [`rtp`] | RTP headers, RFC 6184 H.264 (single NAL + FU-A) and RFC 2435 JPEG payloaders |
//! | [`mpegts`] | an MPEG-2 transport stream mux (PAT, PMT, PES, PCR) for H.264 — what `rff -i udp://` reads today |
//! | [`udp`] | a tiny framing for raw datagrams with a matching reassembler |
//! | [`pacer`] | a frame-rate cap with drop counters |
//!
//! Rules (from the package plan): encoders write into caller buffers;
//! packetizers stream MTU-sized pieces into a sink without an intermediate
//! `Vec`; everything is testable on the host and gated against `rff` and
//! `ffmpeg` where a bitstream is produced.

#[cfg(feature = "alloc")]
extern crate alloc;

pub mod annexb;
pub mod encoder;
#[cfg(feature = "h264")]
pub mod h264;
pub mod mjpeg_http;
pub mod mjpeg_reader;
pub mod mpegts;
pub mod pacer;
pub mod packet;
pub mod policy;
pub mod rtp;
pub mod sink;
pub mod source;
pub mod udp;

pub use encoder::{EncoderConfig, Passthrough, VideoEncoder};
#[cfg(feature = "h264")]
pub use h264::H264;
pub use pacer::Pacer;
pub use packet::{Codec, MediaPacket};
pub use policy::{Choice, H264Path, Job, codec_for};
pub use rusty_esp_core as esp_core;
pub use sink::PacketSink;
pub use source::{EncodedSource, PacketSource};

/// The names a sketch or firmware wants in scope.
pub mod prelude {
    pub use crate::encoder::{EncoderConfig, Passthrough, VideoEncoder};
    pub use crate::mjpeg_http::Multipart;
    pub use crate::mpegts::Mux;
    pub use crate::pacer::Pacer;
    pub use crate::packet::{Codec, MediaPacket};
    pub use crate::sink::PacketSink;
    pub use crate::source::{EncodedSource, PacketSource};
    pub use rusty_esp_core::prelude::*;
}

/// Crate version, for capability manifests and logs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Write a decimal `u64` into `buf`; returns the digits as a `&str`. No `alloc`.
/// Public for the backend crate's HTTP responder.
pub fn fmt_u64_pub(v: u64, buf: &mut [u8; 20]) -> &str {
    fmt_u64(v, buf)
}

/// Write a decimal `u64` into `buf`; returns the digits as a `&str`. No `alloc`.
pub(crate) fn fmt_u64(mut v: u64, buf: &mut [u8; 20]) -> &str {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    // Only ASCII digits were written.
    core::str::from_utf8(&buf[i..]).unwrap_or("0")
}

#[cfg(test)]
mod tests {
    #[test]
    fn fmt_u64_edges() {
        let mut b = [0u8; 20];
        assert_eq!(super::fmt_u64(0, &mut b), "0");
        assert_eq!(super::fmt_u64(7, &mut b), "7");
        assert_eq!(super::fmt_u64(1234567890, &mut b), "1234567890");
        assert_eq!(super::fmt_u64(u64::MAX, &mut b), "18446744073709551615");
    }
}
