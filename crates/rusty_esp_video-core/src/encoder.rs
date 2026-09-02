//! The encoder seam.
//!
//! One trait for every way a frame becomes a packet: the sensor's own JPEG
//! passed through, `rusty_h264` in software (V3), the ESP32-P4 hardware
//! encoders (V4). The output always borrows caller memory; for the passthrough
//! that memory is the frame itself, so a JPEG camera stream costs no copy.

use rusty_esp_core::error::{Error, Result};
use rusty_esp_core::frame::{Frame, Geometry, PixelFormat};

use crate::packet::{Codec, MediaPacket};

/// What a firmware asks an encoder for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Target bitrate in kilobits per second; 0 for constant quality.
    pub bitrate_kbps: u32,
    /// Target frame rate.
    pub fps: u8,
    /// Frames between key frames; 0 for intra-only.
    pub gop: u16,
    /// Codec-specific quality, 0–100 (JPEG: 100 is best).
    pub quality: u8,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        EncoderConfig {
            bitrate_kbps: 0,
            fps: 15,
            gop: 15,
            quality: 80,
        }
    }
}

/// Turns frames into coded packets.
pub trait VideoEncoder {
    /// The codec this encoder emits.
    fn codec(&self) -> Codec;

    /// Prepare for frames of `geometry` under `config`.
    fn configure(&mut self, geometry: Geometry, config: &EncoderConfig) -> Result<()>;

    /// Encode `frame` into `out` (or, for a passthrough, straight from the
    /// frame). The packet borrows whichever memory it points at.
    fn encode<'b>(&mut self, frame: &Frame<'b>, out: &'b mut [u8]) -> Result<MediaPacket<'b>>;

    /// Make the next packet a random-access point.
    fn request_keyframe(&mut self);
}

/// The v1 encoder: a JPEG frame from the sensor is already a packet.
#[derive(Debug, Default, Clone)]
pub struct Passthrough {
    geometry: Option<Geometry>,
}

impl VideoEncoder for Passthrough {
    fn codec(&self) -> Codec {
        Codec::Jpeg
    }

    fn configure(&mut self, geometry: Geometry, _config: &EncoderConfig) -> Result<()> {
        if geometry.format != PixelFormat::Jpeg {
            return Err(Error::Unsupported);
        }
        self.geometry = Some(geometry);
        Ok(())
    }

    fn encode<'b>(&mut self, frame: &Frame<'b>, _out: &'b mut [u8]) -> Result<MediaPacket<'b>> {
        let data = frame.coded().ok_or(Error::Unsupported)?;
        Ok(MediaPacket::new(Codec::Jpeg, true, frame.timestamp, data))
    }

    fn request_keyframe(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_esp_core::time::Micros;

    #[test]
    fn passthrough_is_zero_copy_and_jpeg_only() {
        let g = Geometry::new(320, 240, PixelFormat::Jpeg).unwrap();
        let mut enc = Passthrough::default();
        enc.configure(g, &EncoderConfig::default()).unwrap();
        let jpeg = [0xFF, 0xD8, 0x00, 0x11, 0xFF, 0xD9];
        let frame = Frame::packed(g, Micros::from_millis(5), 3, &jpeg).unwrap();
        let mut scratch = [0u8; 0];
        let pkt = enc.encode(&frame, &mut scratch).unwrap();
        assert_eq!(pkt.codec, Codec::Jpeg);
        assert!(pkt.key);
        assert_eq!(pkt.timestamp.as_millis(), 5);
        assert!(core::ptr::eq(pkt.data.as_ptr(), jpeg.as_ptr()), "no copy");

        let raw = Geometry::new(2, 2, PixelFormat::Gray8).unwrap();
        assert_eq!(
            enc.configure(raw, &EncoderConfig::default()),
            Err(Error::Unsupported)
        );
        let f = Frame::packed(raw, Micros::ZERO, 0, &[0u8; 4]).unwrap();
        assert_eq!(enc.encode(&f, &mut scratch).err(), Some(Error::Unsupported));
    }
}
