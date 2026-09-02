//! Raw datagram framing — the Pi-hub UDP recipe, made precise.
//!
//! A packet is split into MTU-sized datagrams, each with a 24-byte header:
//!
//! ```text
//! "JNS1"  magic (4)
//! seq      u32  per-packet sequence (all fragments of a packet share it)
//! ts_us    u64  capture timestamp, device monotonic microseconds
//! total    u32  total packet length
//! offset   u32  this fragment's offset
//! codec    u8   Codec::tag
//! flags    u8   bit0 key frame
//! reserved u16
//! ```
//!
//! [`Reassembler`] on the receiving side rebuilds one packet at a time into a
//! caller buffer and reports gaps instead of guessing.

use rusty_esp_core::error::{Error, Result};
use rusty_esp_core::pcm::{PcmFormat, SampleFormat};
use rusty_esp_core::time::Micros;

use crate::packet::{Codec, MediaPacket};

/// Header length.
pub const HEADER_LEN: usize = 28;

const MAGIC: &[u8; 4] = b"JNS1";

/// Splits packets into datagrams.
#[derive(Debug, Clone)]
pub struct Framer {
    mtu: usize,
    seq: u32,
}

impl Framer {
    /// A framer for datagrams of at most `mtu` bytes (header included).
    pub fn new(mtu: usize) -> Result<Self> {
        if mtu <= HEADER_LEN {
            return Err(Error::InvalidFormat);
        }
        Ok(Framer { mtu, seq: 0 })
    }

    /// Emit every datagram of `packet` through `emit`, using `scratch` (at
    /// least `mtu` bytes) to assemble each one. Returns the datagram count.
    pub fn frame(
        &mut self,
        packet: &MediaPacket<'_>,
        scratch: &mut [u8],
        mut emit: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<usize> {
        if scratch.len() < self.mtu {
            return Err(Error::BufferTooSmall { needed: self.mtu });
        }
        let total = u32::try_from(packet.len()).map_err(|_| Error::InvalidFormat)?;
        let payload_max = self.mtu - HEADER_LEN;
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        let mut offset = 0usize;
        let mut count = 0usize;
        loop {
            let take = (packet.len() - offset).min(payload_max);
            let h = &mut scratch[..HEADER_LEN];
            h[0..4].copy_from_slice(MAGIC);
            h[4..8].copy_from_slice(&seq.to_be_bytes());
            h[8..16].copy_from_slice(&packet.timestamp.0.to_be_bytes());
            h[16..20].copy_from_slice(&total.to_be_bytes());
            h[20..24].copy_from_slice(&(offset as u32).to_be_bytes());
            h[24] = packet.codec.tag();
            h[25] = u8::from(packet.key);
            h[26..28].copy_from_slice(&[0, 0]);
            scratch[HEADER_LEN..HEADER_LEN + take]
                .copy_from_slice(&packet.data[offset..offset + take]);
            emit(&scratch[..HEADER_LEN + take])?;
            count += 1;
            offset += take;
            if offset >= packet.len() {
                break;
            }
        }
        Ok(count)
    }
}

/// A parsed datagram header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Packet sequence.
    pub seq: u32,
    /// Capture timestamp.
    pub timestamp: Micros,
    /// Total packet length.
    pub total: u32,
    /// Fragment offset.
    pub offset: u32,
    /// Codec tag.
    pub codec: u8,
    /// Key-frame flag.
    pub key: bool,
}

impl Header {
    /// Parse a datagram's header.
    pub fn parse(datagram: &[u8]) -> Result<Self> {
        if datagram.len() < HEADER_LEN || &datagram[0..4] != MAGIC {
            return Err(Error::InvalidFormat);
        }
        let d = datagram;
        let u32_at = |i: usize| u32::from_be_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);
        let ts = u64::from_be_bytes([d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15]]);
        Ok(Header {
            seq: u32_at(4),
            timestamp: Micros(ts),
            total: u32_at(16),
            offset: u32_at(20),
            codec: d[24],
            key: d[25] & 1 == 1,
        })
    }

    /// The codec, when the tag is known.
    #[must_use]
    pub fn codec(&self) -> Option<Codec> {
        match self.codec {
            1 => Some(Codec::Jpeg),
            2 => Some(Codec::H264),
            // PCM parameters are not carried by the tag; callers know their format.
            3 => Some(Codec::Pcm(PcmFormat {
                sample_rate_hz: 16_000,
                channels: 1,
                sample: SampleFormat::I16,
            })),
            _ => None,
        }
    }
}

/// What the reassembler says after each datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reassembly {
    /// More fragments of the current packet are expected.
    Partial,
    /// The packet is complete; its bytes are in the buffer.
    Complete(Header),
    /// A new packet started before the previous one completed; the previous
    /// one was dropped. The new fragment has been accepted.
    Gap {
        /// Sequence of the packet that was lost.
        lost_seq: u32,
    },
}

/// Rebuilds one packet at a time into a caller buffer.
#[derive(Debug)]
pub struct Reassembler<'m> {
    buf: &'m mut [u8],
    current: Option<Header>,
    received: usize,
    /// Packets completed.
    pub complete: u64,
    /// Packets abandoned because a newer one started.
    pub lost: u64,
}

impl<'m> Reassembler<'m> {
    /// A reassembler over `buf`; packets larger than it are refused.
    pub fn new(buf: &'m mut [u8]) -> Self {
        Reassembler {
            buf,
            current: None,
            received: 0,
            complete: 0,
            lost: 0,
        }
    }

    /// Feed one datagram.
    pub fn push(&mut self, datagram: &[u8]) -> Result<Reassembly> {
        let h = Header::parse(datagram)?;
        let payload = &datagram[HEADER_LEN..];
        let total = h.total as usize;
        if total > self.buf.len() {
            return Err(Error::BufferTooSmall { needed: total });
        }
        let end = h.offset as usize + payload.len();
        if end > total {
            return Err(Error::InvalidFormat);
        }
        let mut gap = None;
        match self.current {
            Some(cur) if cur.seq == h.seq => {}
            Some(cur) => {
                self.lost += 1;
                gap = Some(cur.seq);
                self.current = Some(h);
                self.received = 0;
            }
            None => {
                self.current = Some(h);
                self.received = 0;
            }
        }
        self.buf[h.offset as usize..end].copy_from_slice(payload);
        self.received += payload.len();
        if self.received >= total {
            self.current = None;
            self.complete += 1;
            return Ok(Reassembly::Complete(h));
        }
        Ok(gap.map_or(Reassembly::Partial, |lost_seq| Reassembly::Gap { lost_seq }))
    }

    /// The bytes of the last completed packet (valid right after `Complete`).
    #[must_use]
    pub fn packet(&self, header: &Header) -> &[u8] {
        &self.buf[..header.total as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_and_reassemble_round_trip() {
        let data: std::vec::Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let pkt = MediaPacket::new(Codec::Jpeg, true, Micros::from_millis(42), &data);
        let mut framer = Framer::new(300).unwrap();
        let mut scratch = [0u8; 300];
        let mut datagrams: std::vec::Vec<std::vec::Vec<u8>> = std::vec::Vec::new();
        let n = framer
            .frame(&pkt, &mut scratch, |d| {
                datagrams.push(d.to_vec());
                Ok(())
            })
            .unwrap();
        assert_eq!(n, 4); // 272 payload bytes each: 272*3 + 184
        assert_eq!(datagrams.len(), 4);
        let mut buf = [0u8; 2048];
        let mut r = Reassembler::new(&mut buf);
        for d in &datagrams[..3] {
            assert_eq!(r.push(d).unwrap(), Reassembly::Partial);
        }
        let done = r.push(&datagrams[3]).unwrap();
        let Reassembly::Complete(h) = done else {
            panic!("expected completion")
        };
        assert_eq!(h.seq, 0);
        assert_eq!(h.timestamp.as_millis(), 42);
        assert!(h.key);
        assert_eq!(h.codec(), Some(Codec::Jpeg));
        assert_eq!(r.packet(&h), &data[..]);
        assert_eq!(r.complete, 1);
    }

    #[test]
    fn gap_is_reported_and_bad_headers_refused() {
        let a = [1u8; 500];
        let b = [2u8; 500];
        let mut framer = Framer::new(300).unwrap();
        let mut scratch = [0u8; 300];
        let mut ds = std::vec::Vec::new();
        for (i, d) in [&a, &b].iter().enumerate() {
            let pkt = MediaPacket::new(Codec::H264, i == 0, Micros::ZERO, *d);
            framer
                .frame(&pkt, &mut scratch, |x| {
                    ds.push(x.to_vec());
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(ds.len(), 4);
        let mut buf = [0u8; 1024];
        let mut r = Reassembler::new(&mut buf);
        assert_eq!(r.push(&ds[0]).unwrap(), Reassembly::Partial); // a, first half
        // lose ds[1]; b arrives
        assert_eq!(r.push(&ds[2]).unwrap(), Reassembly::Gap { lost_seq: 0 });
        assert!(matches!(r.push(&ds[3]).unwrap(), Reassembly::Complete(h) if h.seq == 1));
        assert_eq!((r.lost, r.complete), (1, 1));
        assert_eq!(Header::parse(b"nope"), Err(Error::InvalidFormat));
        assert_eq!(Framer::new(HEADER_LEN).err(), Some(Error::InvalidFormat));
        let mut small = [0u8; 10];
        let mut r2 = Reassembler::new(&mut small);
        assert!(matches!(
            r2.push(&ds[0]),
            Err(Error::BufferTooSmall { needed: 500 })
        ));
    }
}
