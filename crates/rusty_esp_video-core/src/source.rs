//! Where packets come from.
//!
//! A [`PacketSource`] yields the next coded packet into caller memory. The one
//! implementation every J1 firmware uses is [`EncodedSource`]: an
//! `ImageSource` (a camera, or the test pattern) joined to a [`VideoEncoder`]
//! and paced. The server in the `-esp` crate pulls from a `PacketSource` and
//! knows nothing about cameras or codecs.

use rusty_esp_core::error::{Error, Result};
use rusty_esp_core::frame::{Frame, Geometry, Plane, Planes};
use rusty_esp_core::time::Micros;
use rusty_esp_image_core::source::ImageSource;

use crate::encoder::VideoEncoder;
use crate::pacer::Pacer;
use crate::packet::MediaPacket;

/// Something that produces coded packets on demand.
pub trait PacketSource {
    /// Produce the next packet, using `scratch` for whatever memory it needs.
    /// The packet borrows `scratch`.
    fn next_packet<'b>(&mut self, scratch: &'b mut [u8]) -> Result<MediaPacket<'b>>;
}

/// An `ImageSource` feeding a `VideoEncoder`, with a frame-rate cap.
///
/// `scratch` is split in two: the first `frame_bytes` hold the captured frame,
/// the rest is the encoder's output. For a JPEG passthrough the packet points
/// straight into the frame half, so no byte is copied.
#[derive(Debug)]
pub struct EncodedSource<S, E> {
    source: S,
    encoder: E,
    pacer: Pacer,
    frame_bytes: usize,
    /// Frames the pacer dropped since construction.
    pub dropped: u64,
    /// Packets produced since construction.
    pub produced: u64,
}

impl<S: ImageSource, E: VideoEncoder> EncodedSource<S, E> {
    /// Join `source` and `encoder`; `frame_bytes` of every scratch buffer are
    /// reserved for the captured frame (a JPEG source needs its largest
    /// expected frame; a raw source needs `Geometry::byte_len`). `fps_cap` of
    /// 0 sends every frame.
    pub fn new(source: S, encoder: E, frame_bytes: usize, fps_cap: u32) -> Result<Self> {
        if frame_bytes == 0 {
            return Err(Error::InvalidGeometry);
        }
        Ok(EncodedSource {
            source,
            encoder,
            pacer: Pacer::new(fps_cap),
            frame_bytes,
            dropped: 0,
            produced: 0,
        })
    }

    /// The pacer's counters.
    #[must_use]
    pub fn pacer(&self) -> &Pacer {
        &self.pacer
    }

    /// Take the parts back.
    pub fn into_parts(self) -> (S, E) {
        (self.source, self.encoder)
    }
}

impl<S: ImageSource, E: VideoEncoder> PacketSource for EncodedSource<S, E> {
    fn next_packet<'b>(&mut self, scratch: &'b mut [u8]) -> Result<MediaPacket<'b>> {
        if scratch.len() <= self.frame_bytes {
            return Err(Error::BufferTooSmall {
                needed: self.frame_bytes + 1,
            });
        }
        let (frame_buf, out) = scratch.split_at_mut(self.frame_bytes);
        let base = frame_buf.as_ptr() as usize;
        let base_len = frame_buf.len();
        // Capture with short reborrows until the pacer admits a frame; keep
        // only the admitted frame's shape (plain offsets), then rebuild the
        // view once with the caller's lifetime. A source that blocks per frame
        // paces itself; the test pattern does not, so this loop is bounded by
        // the pacer's interval in device time, never by wall time here.
        let shape = loop {
            let frame = self.source.grab(&mut *frame_buf)?;
            if self.pacer.admit(frame.timestamp) {
                break Shape::of(&frame, base, base_len)?;
            }
            self.dropped += 1;
        };
        let frame_buf: &'b [u8] = &*frame_buf;
        let frame = shape.rebuild(frame_buf)?;
        let packet = self.encoder.encode(&frame, out)?;
        self.produced += 1;
        Ok(packet)
    }
}

/// A frame's layout as offsets into a base buffer — no borrow.
#[derive(Debug, Clone, Copy)]
struct Shape {
    geometry: Geometry,
    timestamp: Micros,
    sequence: u32,
    kind: ShapeKind,
}

#[derive(Debug, Clone, Copy)]
enum ShapeKind {
    Packed {
        offset: usize,
        len: usize,
    },
    Planar {
        y: PlaneShape,
        u: PlaneShape,
        v: PlaneShape,
    },
}

#[derive(Debug, Clone, Copy)]
struct PlaneShape {
    offset: usize,
    len: usize,
    stride: usize,
}

fn plane_shape(plane: &Plane<'_>, base: usize, base_len: usize) -> Result<PlaneShape> {
    let start = plane.data.as_ptr() as usize;
    let offset = start.checked_sub(base).ok_or(Error::InvalidGeometry)?;
    if offset + plane.data.len() > base_len {
        return Err(Error::InvalidGeometry);
    }
    Ok(PlaneShape {
        offset,
        len: plane.data.len(),
        stride: plane.stride,
    })
}

impl Shape {
    /// Record where `frame`'s planes sit inside the buffer at `base`.
    fn of(frame: &Frame<'_>, base: usize, base_len: usize) -> Result<Self> {
        let kind = match frame.planes {
            Planes::Packed(data) => {
                let start = data.as_ptr() as usize;
                let offset = start.checked_sub(base).ok_or(Error::InvalidGeometry)?;
                if offset + data.len() > base_len {
                    return Err(Error::InvalidGeometry);
                }
                ShapeKind::Packed {
                    offset,
                    len: data.len(),
                }
            }
            Planes::Planar { y, u, v } => ShapeKind::Planar {
                y: plane_shape(&y, base, base_len)?,
                u: plane_shape(&u, base, base_len)?,
                v: plane_shape(&v, base, base_len)?,
            },
        };
        Ok(Shape {
            geometry: frame.geometry,
            timestamp: frame.timestamp,
            sequence: frame.sequence,
            kind,
        })
    }

    /// The frame again, over `buf` with the caller's lifetime.
    fn rebuild<'b>(&self, buf: &'b [u8]) -> Result<Frame<'b>> {
        match self.kind {
            ShapeKind::Packed { offset, len } => Frame::packed(
                self.geometry,
                self.timestamp,
                self.sequence,
                buf.get(offset..offset + len)
                    .ok_or(Error::InvalidGeometry)?,
            ),
            ShapeKind::Planar { y, u, v } => {
                let sl = |p: PlaneShape| {
                    buf.get(p.offset..p.offset + p.len)
                        .ok_or(Error::InvalidGeometry)
                };
                Frame::yuv420p(
                    self.geometry,
                    self.timestamp,
                    self.sequence,
                    (sl(y)?, y.stride),
                    (sl(u)?, u.stride),
                    (sl(v)?, v.stride),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{EncoderConfig, Passthrough};
    use crate::packet::Codec;
    use rusty_esp_core::frame::PixelFormat;
    use rusty_esp_image_core::source::TestPattern;

    /// An `ImageSource` that emits a fixed JPEG blob with advancing timestamps.
    struct FixedJpeg {
        geometry: Geometry,
        n: u32,
        step_micros: u64,
    }

    impl ImageSource for FixedJpeg {
        fn geometry(&self) -> Geometry {
            self.geometry
        }

        fn grab<'b>(&mut self, out: &'b mut [u8]) -> Result<Frame<'b>> {
            let bytes = [0xFF, 0xD8, 0x00, self.n as u8, 0xFF, 0xD9];
            out[..6].copy_from_slice(&bytes);
            let f = Frame::packed(
                self.geometry,
                Micros(u64::from(self.n) * self.step_micros),
                self.n,
                &out[..6],
            )?;
            self.n += 1;
            Ok(f)
        }
    }

    #[test]
    fn passthrough_source_paces_and_borrows_the_frame_half() {
        let g = Geometry::new(320, 240, PixelFormat::Jpeg).unwrap();
        let cam = FixedJpeg {
            geometry: g,
            n: 0,
            step_micros: 20_000,
        }; // 50 fps camera
        let mut enc = Passthrough::default();
        enc.configure(g, &EncoderConfig::default()).unwrap();
        let mut src = EncodedSource::new(cam, enc, 64, 10).unwrap(); // capped at 10 fps
        let mut scratch = [0u8; 65];
        let p0 = src.next_packet(&mut scratch).unwrap();
        assert_eq!(p0.codec, Codec::Jpeg);
        assert_eq!(p0.timestamp, Micros::ZERO);
        let p1 = src.next_packet(&mut scratch).unwrap();
        assert_eq!(p1.timestamp.as_millis(), 100, "every fifth camera frame");
        assert_eq!(src.dropped, 4);
        assert_eq!(src.produced, 2);
        let mut small = [0u8; 64];
        assert!(matches!(
            src.next_packet(&mut small),
            Err(Error::BufferTooSmall { needed: 65 })
        ));
    }

    #[test]
    fn raw_pattern_through_passthrough_is_refused_cleanly() {
        let g = Geometry::new(8, 8, PixelFormat::Gray8).unwrap();
        let pattern = TestPattern::new(g, 30).unwrap();
        let mut src = EncodedSource::new(pattern, Passthrough::default(), 64, 0).unwrap();
        let mut scratch = [0u8; 128];
        assert_eq!(
            src.next_packet(&mut scratch).err(),
            Some(Error::Unsupported)
        );
        assert_eq!(
            EncodedSource::new(
                TestPattern::new(g, 30).unwrap(),
                Passthrough::default(),
                0,
                0
            )
            .err(),
            Some(Error::InvalidGeometry)
        );
    }
}
