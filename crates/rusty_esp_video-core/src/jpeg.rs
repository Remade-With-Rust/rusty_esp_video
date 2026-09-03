//! The software JPEG encoder behind the [`VideoEncoder`] seam: a raw sensor
//! frame (YUYV as the sensor delivers it, RGB, gray) becomes one JPEG packet
//! through `rusty_jpeg` on the chip — for sensors without a JPEG mode, and
//! for the raw modes a model path wants while a browser still gets MJPEG.
//!
//! Every packet is a key frame; [`VideoEncoder::request_keyframe`] has
//! nothing to do. The JPEG is written straight into the packetizer's buffer
//! (`rusty_esp_image-core::jpeg::encode`); size it by
//! [`max_bytes`], which is the raw frame plus headers and always enough.

use rusty_esp_core::error::Result;
use rusty_esp_core::{Error, Frame, Geometry, PixelFormat};
use rusty_esp_image_core::jpeg::encode;

use crate::encoder::{EncoderConfig, VideoEncoder};
use crate::packet::{Codec, MediaPacket};

/// The buffer size that always holds a JPEG of `geometry` at any quality.
#[must_use]
pub fn max_bytes(geometry: &Geometry) -> usize {
    encode::max_bytes(geometry)
}

/// The house JPEG encoder behind the [`VideoEncoder`] seam.
#[derive(Debug, Default, Clone)]
pub struct SoftJpeg {
    geometry: Option<Geometry>,
    quality: u8,
    frames: u32,
}

impl SoftJpeg {
    /// An unconfigured encoder; call [`VideoEncoder::configure`] first.
    #[must_use]
    pub const fn new() -> Self {
        SoftJpeg {
            geometry: None,
            quality: 80,
            frames: 0,
        }
    }

    /// Frames encoded since `configure`.
    #[must_use]
    pub const fn frames(&self) -> u32 {
        self.frames
    }

    /// The JPEG quality in use (the seam's `quality`, 0 meaning 80).
    #[must_use]
    pub const fn quality(&self) -> u8 {
        self.quality
    }
}

impl VideoEncoder for SoftJpeg {
    fn codec(&self) -> Codec {
        Codec::Jpeg
    }

    fn configure(&mut self, geometry: Geometry, config: &EncoderConfig) -> Result<()> {
        if !matches!(
            geometry.format,
            PixelFormat::Yuyv422
                | PixelFormat::Rgb888
                | PixelFormat::Bgr888
                | PixelFormat::Rgba8888
                | PixelFormat::Gray8
        ) {
            return Err(Error::Unsupported);
        }
        if geometry.width == 0
            || geometry.height == 0
            || geometry.width > 65_535
            || geometry.height > 65_535
        {
            return Err(Error::InvalidGeometry);
        }
        self.geometry = Some(geometry);
        self.quality = if config.quality == 0 {
            80
        } else {
            config.quality.min(100)
        };
        self.frames = 0;
        Ok(())
    }

    fn encode<'b>(&mut self, frame: &Frame<'b>, out: &'b mut [u8]) -> Result<MediaPacket<'b>> {
        let geometry = self.geometry.ok_or(Error::Unsupported)?;
        if frame.geometry != geometry {
            return Err(Error::InvalidGeometry);
        }
        let n = encode::encode_frame(frame, self.quality, out)?;
        self.frames = self.frames.wrapping_add(1);
        let (data, _) = out.split_at_mut(n);
        Ok(MediaPacket::new(Codec::Jpeg, true, frame.timestamp, data))
    }

    fn request_keyframe(&mut self) {}
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use crate::rtp::JpegScan;
    use rusty_esp_image_core::jpeg::probe;
    use rusty_esp_image_core::source::{ImageSource, TestPattern};

    #[test]
    fn colour_bars_become_a_baseline_jpeg_the_packetizer_accepts() {
        let g = Geometry::new(160, 120, PixelFormat::Rgb888).unwrap();
        let mut pattern = TestPattern::new(g, 10).unwrap();
        let mut rgb = vec![0u8; g.byte_len().unwrap()];
        let mut enc = SoftJpeg::new();
        enc.configure(g, &EncoderConfig::default()).unwrap();
        let mut out = vec![0u8; max_bytes(&g)];
        let frame = pattern.grab(&mut rgb).unwrap();
        let packet = enc.encode(&frame, &mut out).unwrap();
        assert_eq!(packet.codec, Codec::Jpeg);
        assert!(packet.key);
        let info = probe(packet.data).unwrap();
        assert_eq!((info.geometry.width, info.geometry.height), (160, 120));
        JpegScan::parse(packet.data).expect("baseline with standard tables");
        assert_eq!(enc.frames(), 1);
        assert!(
            packet.data.len() < g.byte_len().unwrap(),
            "smaller than raw"
        );
    }

    #[test]
    fn a_yuyv_frame_is_coded_as_the_sensor_delivered_it() {
        let g = Geometry::new(64, 32, PixelFormat::Yuyv422).unwrap();
        let mut yuyv = vec![0u8; g.byte_len().unwrap()];
        for (i, px) in yuyv.chunks_exact_mut(4).enumerate() {
            let y = (i % 32 * 8) as u8;
            px.copy_from_slice(&[y, 128, y, 128]);
        }
        let frame = Frame::packed(g, rusty_esp_core::time::Micros(0), 0, &yuyv).unwrap();
        let mut enc = SoftJpeg::new();
        enc.configure(g, &EncoderConfig::default()).unwrap();
        let mut out = vec![0u8; max_bytes(&g)];
        let packet = enc.encode(&frame, &mut out).unwrap();
        let info = probe(packet.data).unwrap();
        assert_eq!((info.geometry.width, info.geometry.height), (64, 32));
        let mut small = [0u8; 64];
        assert!(matches!(
            enc.encode(&frame, &mut small),
            Err(Error::BufferTooSmall { .. })
        ));
    }

    #[test]
    fn a_planar_or_coded_frame_is_refused() {
        let g = Geometry::new(64, 32, PixelFormat::Yuv420p).unwrap();
        let mut enc = SoftJpeg::new();
        assert_eq!(
            enc.configure(g, &EncoderConfig::default()),
            Err(Error::Unsupported)
        );
        let g = Geometry::new(64, 32, PixelFormat::Jpeg).unwrap();
        assert_eq!(
            enc.configure(g, &EncoderConfig::default()),
            Err(Error::Unsupported)
        );
    }
}
