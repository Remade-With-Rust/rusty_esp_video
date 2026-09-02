//! The one packet type the media packages share.
//!
//! A [`MediaPacket`] is a borrowed view over coded bytes plus the three facts
//! every transport needs: what codec, whether it is a sync point, and when it
//! was captured on the device's monotonic clock. It is promoted to
//! `rusty_esp_core` the day the audio package needs the same shape.

use rusty_esp_core::pcm::PcmFormat;
use rusty_esp_core::time::Micros;

/// The coding of a packet's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Codec {
    /// One complete JPEG/JFIF image.
    Jpeg,
    /// One H.264 access unit in Annex-B byte-stream form (start codes present).
    H264,
    /// Raw interleaved PCM in the given format.
    Pcm(PcmFormat),
}

impl Codec {
    /// The MIME type a browser or an HTTP header wants.
    #[must_use]
    pub const fn mime(self) -> &'static str {
        match self {
            Codec::Jpeg => "image/jpeg",
            Codec::H264 => "video/H264",
            Codec::Pcm(_) => "audio/L16",
        }
    }

    /// Stable wire tag.
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Codec::Jpeg => 1,
            Codec::H264 => 2,
            Codec::Pcm(_) => 3,
        }
    }
}

/// A coded media packet over borrowed bytes.
#[derive(Debug, Clone, Copy)]
pub struct MediaPacket<'a> {
    /// The coding.
    pub codec: Codec,
    /// True for a random-access point (a JPEG always is; an H.264 IDR is).
    pub key: bool,
    /// Capture instant on the device's monotonic clock.
    pub timestamp: Micros,
    /// The coded bytes.
    pub data: &'a [u8],
}

impl<'a> MediaPacket<'a> {
    /// A packet.
    #[must_use]
    pub const fn new(codec: Codec, key: bool, timestamp: Micros, data: &'a [u8]) -> Self {
        MediaPacket {
            codec,
            key,
            timestamp,
            data,
        }
    }

    /// Length of the coded bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.data.len()
    }

    /// True when there are no bytes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}
