//! `rusty_h264` behind the [`VideoEncoder`] seam: H.264 Baseline from the
//! house encoder, configured the way a chip wants it.
//!
//! This is the J5 encoder. Today it needs `std` — `rusty_h264` 0.12 is a
//! host crate (threads, `std::env`, `Vec` everywhere) — so the feature that
//! enables it implies `std` and the wrapper runs on the host and on Track A
//! (ESP-IDF, `std`). The upstream `no_std` pass moves the same wrapper down
//! the ladder unchanged: nothing here depends on anything but the encoder's
//! public API, which is the point of writing it now.
//!
//! ## What the chip configuration is
//!
//! [`H264::configure`] takes the seam's [`EncoderConfig`] (bitrate, fps, GOP,
//! quality) and builds a `rusty_h264` configuration a small CPU can run and
//! any decoder can read:
//!
//! - **Constrained Baseline, CAVLC**, no 8×8 transform, no B-frames, one
//!   reference frame — the profile every hardware and browser decoder
//!   accepts, and the cheapest to produce. (`rusty_h264` defaults to High +
//!   CABAC and selects Baseline through an environment variable; a chip has
//!   no environment, so the fields are set explicitly.)
//! - **`Preset::Fast`**: SAD mode decision, integer-pel motion, 16×16 only.
//! - **No lookahead, no scene cut**: `encode` returns one access unit per
//!   input frame, so latency is one frame and there is nothing to `flush`
//!   except at end of stream. The default configuration buffers 40 frames
//!   for mb-tree; a live camera cannot.
//! - **Fixed GOP** = the seam's `gop` (`min_keyint` = `gop_size`), so key
//!   frames land where the packetizer and a late joiner expect them.
//! - `quality` 0–100 maps to QP 40–16 (`qp = 40 - quality * 24 / 100`) when
//!   `bitrate_kbps` is 0; otherwise the encoder's average-bitrate control
//!   takes `bitrate_kbps` and `fps`.
//!
//! ## Memory
//!
//! `rusty_h264` takes an owned `YuvFrame` (three `Vec<u8>`) and returns an
//! owned `Vec<u8>`; the wrapper copies the borrowed planes in and the bytes
//! out. That is one QVGA frame (115 KB) each way per picture — acceptable on
//! a PSRAM board and the reason the mission plan's V3 names a borrowed-frame
//! input path as an upstream item.

use rusty_esp_core::error::Result;
use rusty_esp_core::{Error, Frame, Geometry, PixelFormat, Planes};
use rusty_h264::{ChromaFormat, Encoder, EncoderConfig as H264Config, Preset, Profile, YuvFrame};

use crate::annexb::contains_idr;
use crate::encoder::{EncoderConfig, VideoEncoder};
use crate::packet::{Codec, MediaPacket};

/// The house H.264 encoder behind the [`VideoEncoder`] seam.
pub struct H264 {
    geometry: Option<Geometry>,
    config: EncoderConfig,
    encoder: Option<Encoder>,
    frame: YuvFrame,
    force_idr: bool,
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
            frame: YuvFrame::black(16, 16),
            force_idr: false,
            frames: 0,
        }
    }

    /// Frames encoded since `configure`.
    #[must_use]
    pub const fn frames(&self) -> u32 {
        self.frames
    }

    /// The `rusty_h264` configuration for `geometry` under `config`: the
    /// chip profile described in the module docs.
    #[must_use]
    pub fn chip_config(geometry: Geometry, config: &EncoderConfig) -> H264Config {
        let mut cfg = H264Config::new(geometry.width as usize, geometry.height as usize);
        cfg.profile = Profile::ConstrainedBaseline;
        cfg.chroma = ChromaFormat::Yuv420;
        cfg.cabac = false;
        cfg.transform_8x8 = false;
        cfg.bframes = 0;
        cfg.num_ref_frames = 1;
        cfg.preset = Preset::Fast;
        cfg.lookahead = 0;
        cfg.scenecut = 0;
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

    /// Build (or rebuild, after `request_keyframe`) the underlying encoder.
    fn start(&mut self) -> Result<()> {
        let geometry = self.geometry.ok_or(Error::Unsupported)?;
        let cfg = Self::chip_config(geometry, &self.config);
        let enc = Encoder::new(cfg).map_err(|_| Error::Unsupported)?;
        self.encoder = Some(enc);
        self.force_idr = false;
        Ok(())
    }

    /// End of stream: whatever the encoder still holds. With no lookahead
    /// this is empty unless a frame is mid-flight; call it before tearing
    /// down a stream so a packetizer sees every access unit.
    pub fn flush(&mut self, out: &mut [u8]) -> Result<usize> {
        let Some(enc) = self.encoder.as_mut() else {
            return Ok(0);
        };
        let bytes = enc.flush();
        copy_out(&bytes, out)
    }
}

/// `quality` 0–100 → QP 40–16, linear.
#[must_use]
pub const fn qp_for_quality(quality: u8) -> u8 {
    let q = if quality > 100 { 100 } else { quality } as u32;
    (40 - q * 24 / 100) as u8
}

fn copy_out(bytes: &[u8], out: &mut [u8]) -> Result<usize> {
    if out.len() < bytes.len() {
        return Err(Error::BufferTooSmall {
            needed: bytes.len(),
        });
    }
    out[..bytes.len()].copy_from_slice(bytes);
    Ok(bytes.len())
}

/// Copy a borrowed planar frame into the encoder's owned frame.
fn load_frame(dst: &mut YuvFrame, frame: &Frame<'_>) -> Result<()> {
    let Geometry { width, height, .. } = frame.geometry;
    let (w, h) = (width as usize, height as usize);
    let Planes::Planar { y, u, v } = &frame.planes else {
        return Err(Error::Unsupported);
    };
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    if dst.width != w || dst.height != h {
        *dst = YuvFrame::black(w, h);
    }
    copy_plane(&mut dst.y[..], y, w, h)?;
    copy_plane(&mut dst.u[..], u, cw, ch)?;
    copy_plane(&mut dst.v[..], v, cw, ch)?;
    Ok(())
}

fn copy_plane(dst: &mut [u8], src: &rusty_esp_core::Plane<'_>, w: usize, h: usize) -> Result<()> {
    if dst.len() < w * h {
        return Err(Error::InvalidGeometry);
    }
    for row in 0..h {
        let line = src.row(row).ok_or(Error::InvalidGeometry)?;
        let line = line.get(..w).ok_or(Error::InvalidGeometry)?;
        dst[row * w..(row + 1) * w].copy_from_slice(line);
    }
    Ok(())
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
        self.geometry = Some(geometry);
        self.config = *config;
        self.frames = 0;
        self.frame = YuvFrame::black(geometry.width as usize, geometry.height as usize);
        self.start()
    }

    fn encode<'b>(&mut self, frame: &Frame<'b>, out: &'b mut [u8]) -> Result<MediaPacket<'b>> {
        let geometry = self.geometry.ok_or(Error::Unsupported)?;
        if frame.geometry.width != geometry.width || frame.geometry.height != geometry.height {
            return Err(Error::InvalidGeometry);
        }
        if self.force_idr || self.encoder.is_none() {
            // `rusty_h264` has no keyframe request; a fresh encoder starts
            // with an IDR, which is what the caller asked for.
            self.start()?;
        }
        load_frame(&mut self.frame, frame)?;
        let enc = self.encoder.as_mut().ok_or(Error::Unsupported)?;
        let bytes = enc.encode(&self.frame);
        let n = copy_out(&bytes, out)?;
        self.frames = self.frames.wrapping_add(1);
        let (data, _) = out.split_at_mut(n);
        let key = contains_idr(data);
        Ok(MediaPacket::new(Codec::H264, key, frame.timestamp, data))
    }

    fn request_keyframe(&mut self) {
        self.force_idr = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_maps_to_a_sane_qp_range() {
        assert_eq!(qp_for_quality(0), 40);
        assert_eq!(qp_for_quality(100), 16);
        assert_eq!(qp_for_quality(80), 21);
        assert_eq!(qp_for_quality(200), 16);
    }

    #[test]
    fn chip_config_is_constrained_baseline_cavlc_with_no_lookahead() {
        let g = Geometry::new(320, 240, PixelFormat::Yuv420p).unwrap();
        let c = H264::chip_config(g, &EncoderConfig::default());
        assert_eq!(c.profile, Profile::ConstrainedBaseline);
        assert!(!c.cabac);
        assert!(!c.transform_8x8);
        assert_eq!(c.bframes, 0);
        assert_eq!(c.lookahead, 0);
        assert_eq!(c.num_ref_frames, 1);
        assert_eq!(c.gop_size, 15);
        assert_eq!(c.min_keyint, 15);
        assert_eq!(c.preset, Preset::Fast);
    }
}
