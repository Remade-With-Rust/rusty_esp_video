//! RTP: the fixed header, and payloaders for H.264 (RFC 6184, single NAL unit
//! and FU-A fragmentation) and JPEG (RFC 2435).
//!
//! Every payloader writes MTU-sized packets into a caller scratch buffer and
//! hands each to an `emit` closure — a UDP socket send, a QUIC datagram, a
//! test's `Vec`.

use rusty_esp_core::error::{Error, Result};
use rusty_esp_core::time::Micros;

use crate::annexb::nal_units;

/// Fixed RTP header length (no CSRC, no extension).
pub const HEADER_LEN: usize = 12;

/// The 90 kHz clock video payload types use.
pub const VIDEO_CLOCK_HZ: u32 = 90_000;

/// Static payload type for JPEG (RFC 3551).
pub const PT_JPEG: u8 = 26;

/// A dynamic payload type commonly used for H.264.
pub const PT_H264_DYNAMIC: u8 = 96;

/// Sequence, timestamp and identity of one RTP stream.
#[derive(Debug, Clone)]
pub struct Rtp {
    /// Synchronisation source.
    pub ssrc: u32,
    /// Payload type (7 bits).
    pub payload_type: u8,
    /// Clock rate of the timestamp field.
    pub clock_hz: u32,
    seq: u16,
}

impl Rtp {
    /// A stream with the given identity; the sequence starts at `first_seq`.
    #[must_use]
    pub fn new(ssrc: u32, payload_type: u8, clock_hz: u32, first_seq: u16) -> Self {
        Rtp {
            ssrc,
            payload_type: payload_type & 0x7F,
            clock_hz,
            seq: first_seq,
        }
    }

    /// The next sequence number (the value the next packet will carry).
    #[must_use]
    pub fn seq(&self) -> u16 {
        self.seq
    }

    /// Convert a device timestamp to this stream's clock, wrapping.
    #[must_use]
    pub fn timestamp(&self, micros: Micros) -> u32 {
        ((u128::from(micros.0) * u128::from(self.clock_hz)) / 1_000_000) as u32
    }

    /// Write a header for the next packet into `out[..12]` and advance the sequence.
    pub fn write_header(&mut self, out: &mut [u8], marker: bool, timestamp: u32) -> Result<()> {
        if out.len() < HEADER_LEN {
            return Err(Error::BufferTooSmall { needed: HEADER_LEN });
        }
        out[0] = 0x80; // V=2, P=0, X=0, CC=0
        out[1] = self.payload_type | if marker { 0x80 } else { 0 };
        out[2..4].copy_from_slice(&self.seq.to_be_bytes());
        out[4..8].copy_from_slice(&timestamp.to_be_bytes());
        out[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        self.seq = self.seq.wrapping_add(1);
        Ok(())
    }
}

/// A parsed RTP header (for tests and the host receiver).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Marker bit.
    pub marker: bool,
    /// Payload type.
    pub payload_type: u8,
    /// Sequence number.
    pub seq: u16,
    /// Timestamp.
    pub timestamp: u32,
    /// SSRC.
    pub ssrc: u32,
}

impl Header {
    /// Parse a fixed header; returns it and the payload.
    pub fn parse(packet: &[u8]) -> Result<(Header, &[u8])> {
        if packet.len() < HEADER_LEN || packet[0] >> 6 != 2 {
            return Err(Error::InvalidFormat);
        }
        let cc = (packet[0] & 0x0F) as usize;
        let ext = packet[0] & 0x10 != 0;
        let mut off = HEADER_LEN + 4 * cc;
        if ext {
            let len = u16::from_be_bytes([packet[off + 2], packet[off + 3]]) as usize;
            off += 4 + 4 * len;
        }
        if packet.len() < off {
            return Err(Error::InvalidFormat);
        }
        Ok((
            Header {
                marker: packet[1] & 0x80 != 0,
                payload_type: packet[1] & 0x7F,
                seq: u16::from_be_bytes([packet[2], packet[3]]),
                timestamp: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
                ssrc: u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
            },
            &packet[off..],
        ))
    }
}

/// RFC 6184 payloader: single NAL unit packets, FU-A when a NAL exceeds the MTU.
#[derive(Debug, Clone)]
pub struct H264Payloader {
    /// The stream.
    pub rtp: Rtp,
    mtu: usize,
}

/// FU-A NAL unit type.
const FU_A: u8 = 28;

impl H264Payloader {
    /// Packets of at most `mtu` bytes including the RTP header.
    pub fn new(rtp: Rtp, mtu: usize) -> Result<Self> {
        if mtu < HEADER_LEN + 3 {
            return Err(Error::InvalidFormat);
        }
        Ok(H264Payloader { rtp, mtu })
    }

    /// Packetize one Annex-B access unit captured at `timestamp`. `scratch`
    /// must hold `mtu` bytes. Returns the packet count. The marker bit is set
    /// on the last packet of the access unit.
    pub fn packetize(
        &mut self,
        access_unit: &[u8],
        timestamp: Micros,
        scratch: &mut [u8],
        mut emit: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<usize> {
        if scratch.len() < self.mtu {
            return Err(Error::BufferTooSmall { needed: self.mtu });
        }
        let ts = self.rtp.timestamp(timestamp);
        let nals: usize = nal_units(access_unit).count();
        if nals == 0 {
            return Err(Error::InvalidFormat);
        }
        let single_max = self.mtu - HEADER_LEN;
        let frag_max = self.mtu - HEADER_LEN - 2;
        let mut count = 0usize;
        for (i, nal) in nal_units(access_unit).enumerate() {
            let last_nal = i + 1 == nals;
            if nal.len() <= single_max {
                self.rtp.write_header(scratch, last_nal, ts)?;
                scratch[HEADER_LEN..HEADER_LEN + nal.len()].copy_from_slice(nal);
                emit(&scratch[..HEADER_LEN + nal.len()])?;
                count += 1;
            } else {
                let indicator = (nal[0] & 0xE0) | FU_A;
                let nal_type = nal[0] & 0x1F;
                let body = &nal[1..];
                let mut off = 0usize;
                while off < body.len() {
                    let take = (body.len() - off).min(frag_max);
                    let start = off == 0;
                    let end = off + take == body.len();
                    self.rtp.write_header(scratch, last_nal && end, ts)?;
                    scratch[HEADER_LEN] = indicator;
                    scratch[HEADER_LEN + 1] =
                        nal_type | if start { 0x80 } else { 0 } | if end { 0x40 } else { 0 };
                    scratch[HEADER_LEN + 2..HEADER_LEN + 2 + take]
                        .copy_from_slice(&body[off..off + take]);
                    emit(&scratch[..HEADER_LEN + 2 + take])?;
                    count += 1;
                    off += take;
                }
            }
        }
        Ok(count)
    }
}

/// What RFC 2435 needs to know about a baseline JPEG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JpegScan<'a> {
    /// Width in pixels (multiple of 8, at most 2040).
    pub width: u16,
    /// Height in pixels (multiple of 8, at most 2040).
    pub height: u16,
    /// RFC 2435 type: 0 for 4:2:2, 1 for 4:2:0; +64 when restart markers are used.
    pub type_: u8,
    /// Restart interval from a DRI segment, when present.
    pub restart_interval: Option<u16>,
    /// Quantization tables, 64 bytes each in zigzag order, up to two.
    pub qtables: [&'a [u8]; 2],
    /// Number of quantization tables present.
    pub qtable_count: u8,
    /// The entropy-coded scan data (after the SOS header, before EOI).
    pub scan: &'a [u8],
}

impl<'a> JpegScan<'a> {
    /// Pick apart a baseline JPEG. Progressive, 12-bit, 16-bit quantization
    /// tables, non-8-multiple sizes and exotic sampling are refused: RFC 2435
    /// cannot carry them.
    pub fn parse(jpeg: &'a [u8]) -> Result<Self> {
        if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
            return Err(Error::InvalidFormat);
        }
        let mut i = 2usize;
        let mut qtables: [&[u8]; 2] = [&[], &[]];
        let mut qcount = 0u8;
        let mut dims: Option<(u16, u16, u8)> = None; // width, height, type
        let mut restart = None;
        loop {
            while jpeg.get(i) == Some(&0xFF) {
                i += 1;
            }
            let &marker = jpeg.get(i).ok_or(Error::InvalidFormat)?;
            i += 1;
            if matches!(marker, 0x01 | 0xD0..=0xD7) {
                continue;
            }
            let len = usize::from(u16::from_be_bytes([
                *jpeg.get(i).ok_or(Error::InvalidFormat)?,
                *jpeg.get(i + 1).ok_or(Error::InvalidFormat)?,
            ]));
            if len < 2 {
                return Err(Error::InvalidFormat);
            }
            let seg = jpeg.get(i + 2..i + len).ok_or(Error::InvalidFormat)?;
            match marker {
                0xDB => {
                    // DQT: one or more (Pq|Tq, 64 bytes) tables.
                    let mut s = seg;
                    while !s.is_empty() {
                        let pq = s[0] >> 4;
                        let tq = (s[0] & 0x0F) as usize;
                        if pq != 0 || s.len() < 65 || tq > 1 {
                            return Err(Error::Unsupported);
                        }
                        qtables[tq] = &s[1..65];
                        qcount = qcount.max(tq as u8 + 1);
                        s = &s[65..];
                    }
                }
                0xC0 | 0xC1 => {
                    if seg.len() < 6 || seg[0] != 8 || seg[5] != 3 {
                        return Err(Error::Unsupported);
                    }
                    let h = u16::from_be_bytes([seg[1], seg[2]]);
                    let w = u16::from_be_bytes([seg[3], seg[4]]);
                    if w == 0 || h == 0 || w % 8 != 0 || h % 8 != 0 || w > 2040 || h > 2040 {
                        return Err(Error::Unsupported);
                    }
                    // component 0 sampling factors
                    let hv = *seg.get(7).ok_or(Error::InvalidFormat)?;
                    let type_ = match hv {
                        0x21 => 0, // 2x1: 4:2:2
                        0x22 => 1, // 2x2: 4:2:0
                        _ => return Err(Error::Unsupported),
                    };
                    dims = Some((w, h, type_));
                }
                0xC2..=0xCF if marker != 0xC4 && marker != 0xC8 && marker != 0xCC => {
                    return Err(Error::Unsupported); // progressive / lossless / hierarchical
                }
                0xDD => {
                    if seg.len() < 2 {
                        return Err(Error::InvalidFormat);
                    }
                    restart = Some(u16::from_be_bytes([seg[0], seg[1]]));
                }
                0xDA => {
                    let (w, h, mut type_) = dims.ok_or(Error::InvalidFormat)?;
                    if qcount == 0 {
                        return Err(Error::InvalidFormat);
                    }
                    if restart.is_some() {
                        type_ += 64;
                    }
                    let scan_start = i + len;
                    // Scan data ends at EOI; scan back for FF D9.
                    let mut end = jpeg.len();
                    while end >= 2 && !(jpeg[end - 2] == 0xFF && jpeg[end - 1] == 0xD9) {
                        end -= 1;
                    }
                    if end < 2 || end - 2 < scan_start {
                        return Err(Error::InvalidFormat);
                    }
                    return Ok(JpegScan {
                        width: w,
                        height: h,
                        type_,
                        restart_interval: restart,
                        qtables,
                        qtable_count: qcount,
                        scan: &jpeg[scan_start..end - 2],
                    });
                }
                0xD9 => return Err(Error::InvalidFormat),
                _ => {}
            }
            i += len;
        }
    }
}

/// RFC 2435 payloader. Quantization tables travel in the first packet of
/// each frame (`Q = 255`).
#[derive(Debug, Clone)]
pub struct JpegPayloader {
    /// The stream (payload type 26, 90 kHz).
    pub rtp: Rtp,
    mtu: usize,
}

impl JpegPayloader {
    /// Packets of at most `mtu` bytes including the RTP header.
    pub fn new(ssrc: u32, first_seq: u16, mtu: usize) -> Result<Self> {
        // header 12 + main 8 + restart 4 + qtable header 4 + two tables 128 + one byte
        if mtu < HEADER_LEN + 8 + 4 + 4 + 128 + 1 {
            return Err(Error::InvalidFormat);
        }
        Ok(JpegPayloader {
            rtp: Rtp::new(ssrc, PT_JPEG, VIDEO_CLOCK_HZ, first_seq),
            mtu,
        })
    }

    /// Packetize one JPEG captured at `timestamp`. Returns the packet count.
    pub fn packetize(
        &mut self,
        jpeg: &[u8],
        timestamp: Micros,
        scratch: &mut [u8],
        mut emit: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<usize> {
        if scratch.len() < self.mtu {
            return Err(Error::BufferTooSmall { needed: self.mtu });
        }
        let info = JpegScan::parse(jpeg)?;
        let ts = self.rtp.timestamp(timestamp);
        let mut off = 0usize;
        let mut count = 0usize;
        while off < info.scan.len() {
            let mut p = HEADER_LEN;
            // main header
            scratch[p] = 0; // type-specific
            let o = off as u32;
            scratch[p + 1] = (o >> 16) as u8;
            scratch[p + 2] = (o >> 8) as u8;
            scratch[p + 3] = o as u8;
            scratch[p + 4] = info.type_;
            scratch[p + 5] = 255; // Q: tables in-band
            scratch[p + 6] = (info.width / 8) as u8;
            scratch[p + 7] = (info.height / 8) as u8;
            p += 8;
            if let Some(ri) = info.restart_interval {
                scratch[p..p + 2].copy_from_slice(&ri.to_be_bytes());
                scratch[p + 2..p + 4].copy_from_slice(&0xFFFFu16.to_be_bytes()); // F=1, L=1, count=0x3FFF
                p += 4;
            }
            if off == 0 {
                let n = usize::from(info.qtable_count);
                scratch[p] = 0; // MBZ
                scratch[p + 1] = 0; // precision: 8-bit tables
                scratch[p + 2..p + 4].copy_from_slice(&((64 * n) as u16).to_be_bytes());
                p += 4;
                for t in &info.qtables[..n] {
                    scratch[p..p + 64].copy_from_slice(t);
                    p += 64;
                }
            }
            let room = self.mtu - p;
            let take = (info.scan.len() - off).min(room);
            let last = off + take == info.scan.len();
            self.rtp.write_header(scratch, last, ts)?;
            scratch[p..p + take].copy_from_slice(&info.scan[off..off + take]);
            emit(&scratch[..p + take])?;
            count += 1;
            off += take;
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annexb::AUD_NAL;

    #[test]
    fn header_round_trip_and_timestamp_clock() {
        let mut r = Rtp::new(0xDEADBEEF, PT_H264_DYNAMIC, VIDEO_CLOCK_HZ, 65535);
        assert_eq!(r.timestamp(Micros::from_millis(1000)), 90_000);
        let mut buf = [0u8; 12];
        r.write_header(&mut buf, true, 90_000).unwrap();
        let (h, rest) = Header::parse(&buf).unwrap();
        assert!(rest.is_empty());
        assert_eq!(h.seq, 65535);
        assert!(h.marker);
        assert_eq!(h.payload_type, 96);
        assert_eq!(h.ssrc, 0xDEADBEEF);
        assert_eq!(r.seq(), 0, "wrapped");
    }

    #[test]
    fn h264_single_and_fragmented() {
        let mut au = AUD_NAL.to_vec();
        au.extend_from_slice(&[0, 0, 0, 1, 0x67, 1, 2, 3]); // SPS
        au.extend_from_slice(&[0, 0, 0, 1, 0x65]);
        au.extend(core::iter::repeat_n(0xABu8, 3000)); // big IDR slice
        let mut p = H264Payloader::new(Rtp::new(1, 96, 90_000, 0), 1200).unwrap();
        let mut scratch = [0u8; 1200];
        let mut pkts: std::vec::Vec<std::vec::Vec<u8>> = std::vec::Vec::new();
        let n = p
            .packetize(&au, Micros::from_millis(40), &mut scratch, |b| {
                pkts.push(b.to_vec());
                Ok(())
            })
            .unwrap();
        // AUD (single), SPS (single), IDR 3001 bytes -> body 3000 over 1186-byte fragments = 3 FU-A
        assert_eq!(n, 5);
        let (h0, pl0) = Header::parse(&pkts[0]).unwrap();
        assert_eq!(pl0, &[0x09, 0xF0]);
        assert!(!h0.marker);
        assert_eq!(h0.timestamp, 3600);
        let (_, pl2) = Header::parse(&pkts[2]).unwrap();
        assert_eq!(pl2[0], (0x65 & 0xE0) | 28, "FU indicator keeps NRI");
        assert_eq!(pl2[1], 0x80 | 5, "start bit + type");
        let (h4, pl4) = Header::parse(&pkts[4]).unwrap();
        assert!(h4.marker, "marker on the last packet of the AU");
        assert_eq!(pl4[1], 0x40 | 5, "end bit + type");
        // reassemble the fragments and compare to the NAL body
        let mut body = std::vec::Vec::new();
        for pk in &pkts[2..] {
            let (_, pl) = Header::parse(pk).unwrap();
            body.extend_from_slice(&pl[2..]);
        }
        assert_eq!(body.len(), 3000);
        assert!(body.iter().all(|&b| b == 0xAB));
        let seqs: std::vec::Vec<u16> = pkts
            .iter()
            .map(|p| Header::parse(p).unwrap().0.seq)
            .collect();
        assert_eq!(seqs, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn jpeg_payloader_on_a_house_encoded_image() {
        // A real baseline JPEG from rusty_jpeg, 64x32 RGB.
        let (w, h) = (64u16, 32u16);
        let rgb: std::vec::Vec<u8> = (0..(w as usize * h as usize * 3))
            .map(|i| (i * 7 % 256) as u8)
            .collect();
        let mut jpeg = std::vec::Vec::new();
        let enc = rusty_jpeg::encode::Encoder::new(&mut jpeg, 75);
        enc.encode(&rgb, w, h, rusty_jpeg::encode::ColorType::Rgb)
            .unwrap();

        let info = JpegScan::parse(&jpeg).unwrap();
        assert_eq!((info.width, info.height), (64, 32));
        assert!(info.type_ == 0 || info.type_ == 1, "type {}", info.type_);
        assert!(info.qtable_count >= 1);
        assert!(!info.scan.is_empty());

        let mut p = JpegPayloader::new(7, 100, 400).unwrap();
        let mut scratch = [0u8; 400];
        let mut pkts: std::vec::Vec<std::vec::Vec<u8>> = std::vec::Vec::new();
        let n = p
            .packetize(&jpeg, Micros::from_millis(10), &mut scratch, |b| {
                pkts.push(b.to_vec());
                Ok(())
            })
            .unwrap();
        assert_eq!(n, pkts.len());
        // walk the packets: offsets contiguous, tables only in the first, marker last
        let mut expected_off = 0u32;
        let mut total = 0usize;
        for (k, pk) in pkts.iter().enumerate() {
            let (hdr, pl) = Header::parse(pk).unwrap();
            assert_eq!(hdr.payload_type, PT_JPEG);
            assert_eq!(hdr.marker, k + 1 == pkts.len());
            let off = (u32::from(pl[1]) << 16) | (u32::from(pl[2]) << 8) | u32::from(pl[3]);
            assert_eq!(off, expected_off);
            assert_eq!(pl[4], info.type_);
            assert_eq!(pl[5], 255);
            assert_eq!((pl[6], pl[7]), (8, 4));
            let mut p = 8;
            if k == 0 {
                let len = u16::from_be_bytes([pl[p + 2], pl[p + 3]]) as usize;
                assert_eq!(len, 64 * usize::from(info.qtable_count));
                assert_eq!(&pl[p + 4..p + 4 + 64], info.qtables[0]);
                p += 4 + len;
            }
            let payload = &pl[p..];
            assert_eq!(
                payload,
                &info.scan[off as usize..off as usize + payload.len()]
            );
            expected_off += payload.len() as u32;
            total += payload.len();
        }
        assert_eq!(total, info.scan.len());
        // progressive is refused
        let mut prog = std::vec::Vec::new();
        let mut enc = rusty_jpeg::encode::Encoder::new(&mut prog, 75);
        enc.set_progressive(true);
        enc.encode(&rgb, w, h, rusty_jpeg::encode::ColorType::Rgb)
            .unwrap();
        assert_eq!(JpegScan::parse(&prog).err(), Some(Error::Unsupported));
    }
}
