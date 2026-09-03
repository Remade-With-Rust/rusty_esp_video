//! `rusty_h264` behind the [`VideoEncoder`] seam: H.264 Baseline from the
//! house encoder, configured the way a chip wants it.
//!
//! This is the J5 encoder, on `rusty_h264` 0.14's chip API. The crate is
//! `no_std` + `alloc` without its `std` feature (`libm` carries the float
//! math), so this wrapper runs on the host, on Track A (ESP-IDF) and on the
//! bare-metal track alike; the `h264` feature needs only `alloc`.
//!
//! ## What the chip configuration is
//!
//! [`H264::configure`] takes the seam's [`EncoderConfig`] (bitrate, fps, GOP,
//! quality) and starts from `EncoderConfig::baseline` upstream — the one
//! constructor both this crate and `rff -profile baseline -preset fast`
//! select, so host and device produce the same bytes:
//!
//! - **Constrained Baseline, CAVLC**, no 8×8 transform, no B-frames, one
//!   reference frame — the profile every hardware and browser decoder
//!   accepts, and the cheapest to produce.
//! - **`Preset::Fast`**: SAD mode decision, integer-pel motion, 16×16 only;
//!   the half-pel plane cache is never built.
//! - **No lookahead, no scene cut, no mb-tree**: `encode` returns one access
//!   unit per input frame, so latency is one frame and there is nothing to
//!   `flush` except at end of stream.
//! - **Fixed GOP** = the seam's `gop` (`min_keyint` = `gop_size`), so key
//!   frames land where the packetizer and a late joiner expect them; and
//!   [`VideoEncoder::request_keyframe`] makes the *next* picture an IDR
//!   without a fresh encoder (rate control and the frame counter survive).
//! - `quality` 0–100 maps to QP 40–16 (`qp = 40 - quality * 24 / 100`) when
//!   `bitrate_kbps` is 0; otherwise the encoder's average-bitrate control
//!   takes `bitrate_kbps` and `fps`.
//!
//! ## Memory
//!
//! The camera's planes are **borrowed**: the seam's [`Frame`] becomes a
//! `YuvPlanes` view with its strides, and `Encoder::encode_into` writes the
//! access unit **in place** into the packetizer's buffer — no copy of the
//! frame, no `Vec` for the NALs. A buffer too small for the access unit is
//! reported with the exact size it needed (the picture is lost, the encoder
//! stays in step); size it for the worst case, an IDR at low QP, which can
//! approach `width * height * 3 / 2`.

use rusty_esp_core::error::Result;
use rusty_esp_core::{Error, Frame, Geometry, PixelFormat, Planes};
use rusty_h264::{EncodeError, Encoder, EncoderConfig as H264Config, YuvPlanes};

use crate::annexb::contains_idr;
use crate::encoder::{EncoderConfig, VideoEncoder};
use crate::packet::{Codec, MediaPacket};

/// The house H.264 encoder behind the [`VideoEncoder`] seam.
pub struct H264 {
    geometry: Option<Geometry>,
    config: EncoderConfig,
    encoder: Option<Encoder>,
    frames: u32,
}

impl core::fmt::Debug for H264 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("H264")
            .field("geometry", &self.geometry)
            .field("config", &self.config)
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

impl Default for H264 {
    fn default() -> Self {
        Self::new()
    }
}

impl H264 {
    /// An unconfigured encoder; call [`VideoEncoder::configure`] first.
    #[must_use]
    pub fn new() -> Self {
        H264 {
            geometry: None,
            config: EncoderConfig::default(),
            encoder: None,
            frames: 0,
        }
    }

    /// Frames encoded since `configure`.
    #[must_use]
    pub const fn frames(&self) -> u32 {
        self.frames
    }

    /// The `rusty_h264` configuration for `geometry` under `config`: the
    /// upstream `baseline` constructor (the chip profile described in the
    /// module docs) with the seam's GOP, rate and quality on top.
    #[must_use]
    pub fn chip_config(geometry: Geometry, config: &EncoderConfig) -> H264Config {
        let mut cfg = H264Config::baseline(geometry.width as usize, geometry.height as usize);
        let gop = if config.gop == 0 {
            1
        } else {
            u32::from(config.gop)
        };
        cfg.gop_size = gop;
        cfg.min_keyint = gop;
        cfg.framerate = f32::from(config.fps.max(1));
        cfg.bitrate = config.bitrate_kbps.saturating_mul(1000);
        cfg.qp = qp_for_quality(config.quality);
        cfg
    }

    /// End of stream: whatever the encoder still holds. With no lookahead
    /// this is empty unless a frame is mid-flight; call it before tearing
    /// down a stream so a packetizer sees every access unit.
    pub fn flush(&mut self, out: &mut [u8]) -> Result<usize> {
        let Some(enc) = self.encoder.as_mut() else {
            return Ok(0);
        };
        enc.flush_into(out).map_err(map_err)
    }
}

/// `quality` 0–100 → QP 40–16, linear.
#[must_use]
pub const fn qp_for_quality(quality: u8) -> u8 {
    let q = if quality > 100 { 100 } else { quality } as u32;
    (40 - q * 24 / 100) as u8
}

fn map_err(e: EncodeError) -> Error {
    match e {
        EncodeError::BufferTooSmall { needed } => Error::BufferTooSmall { needed },
        EncodeError::FrameMismatch => Error::InvalidGeometry,
        _ => Error::Unsupported,
    }
}

/// The seam's planar frame as the encoder's borrowed view, strides and all.
fn planes_of<'a>(frame: &Frame<'a>) -> Result<YuvPlanes<'a>> {
    let Planes::Planar { y, u, v } = &frame.planes else {
        return Err(Error::Unsupported);
    };
    let Geometry { width, height, .. } = frame.geometry;
    YuvPlanes::new(
        width as usize,
        height as usize,
        y.data,
        u.data,
        v.data,
        y.stride,
        u.stride.max(v.stride),
    )
    .ok_or(Error::InvalidGeometry)
}

impl VideoEncoder for H264 {
    fn codec(&self) -> Codec {
        Codec::H264
    }

    fn configure(&mut self, geometry: Geometry, config: &EncoderConfig) -> Result<()> {
        if geometry.format != PixelFormat::Yuv420p {
            return Err(Error::Unsupported);
        }
        if geometry.width % 2 != 0 || geometry.height % 2 != 0 || geometry.width == 0 {
            return Err(Error::InvalidGeometry);
        }
        let enc = Encoder::new(Self::chip_config(geometry, config)).map_err(map_err)?;
        self.geometry = Some(geometry);
        self.config = *config;
        self.frames = 0;
        self.encoder = Some(enc);
        Ok(())
    }

    fn encode<'b>(&mut self, frame: &Frame<'b>, out: &'b mut [u8]) -> Result<MediaPacket<'b>> {
        let geometry = self.geometry.ok_or(Error::Unsupported)?;
        if frame.geometry.width != geometry.width || frame.geometry.height != geometry.height {
            return Err(Error::InvalidGeometry);
        }
        let enc = self.encoder.as_mut().ok_or(Error::Unsupported)?;
        let planes = planes_of(frame)?;
        let n = enc.encode_into(&planes, out).map_err(map_err)?;
        self.frames = self.frames.wrapping_add(1);
        let (data, _) = out.split_at_mut(n);
        let key = contains_idr(data);
        Ok(MediaPacket::new(Codec::H264, key, frame.timestamp, data))
    }

    fn request_keyframe(&mut self) {
        if let Some(enc) = self.encoder.as_mut() {
            enc.request_keyframe();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_h264::{Preset, Profile};

    #[test]
    fn quality_maps_to_a_sane_qp_range() {
        assert_eq!(qp_for_quality(0), 40);
        assert_eq!(qp_for_quality(100), 16);
        assert_eq!(qp_for_quality(80), 21);
        assert_eq!(qp_for_quality(200), 16);
    }

    #[test]
    fn chip_config_is_upstream_baseline_with_the_seams_gop_on_top() {
        let g = Geometry::new(320, 240, PixelFormat::Yuv420p).unwrap();
        let c = H264::chip_config(g, &EncoderConfig::default());
        assert_eq!(c.profile, Profile::ConstrainedBaseline);
        assert!(!c.cabac);
        assert!(!c.transform_8x8);
        assert!(!c.mbtree);
        assert_eq!(c.bframes, 0);
        assert_eq!(c.lookahead, 0);
        assert_eq!(c.scenecut, 0);
        assert_eq!(c.num_ref_frames, 1);
        assert_eq!(c.gop_size, 15);
        assert_eq!(c.min_keyint, 15);
        assert_eq!(c.preset, Preset::Fast);
    }
}
