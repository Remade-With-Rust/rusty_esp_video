//! RTP: the fixed header, payloaders for H.264 (RFC 6184, single NAL unit
//! and FU-A fragmentation) and JPEG (RFC 2435), and the JPEG depayloader that
//! turns RFC 2435 packets back into a JPEG file in a caller buffer.
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
                0xC4 => {
                    // DHT: RFC 2435 carries no Huffman tables; the receiver
                    // regenerates the Annex K set, so the scan must use it.
                    let mut s = seg;
                    while !s.is_empty() {
                        if s.len() < 17 {
                            return Err(Error::InvalidFormat);
                        }
                        let (codelens, expected): (&[u8], &huffman::Table) = (
                            &s[1..17],
                            huffman::table_for(s[0]).ok_or(Error::Unsupported)?,
                        );
                        let nsym: usize = codelens.iter().map(|&c| usize::from(c)).sum();
                        let symbols = s.get(17..17 + nsym).ok_or(Error::InvalidFormat)?;
                        if codelens != expected.codelens || symbols != expected.symbols {
                            return Err(Error::Unsupported);
                        }
                        s = &s[17 + nsym..];
                    }
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

/// RFC 2435 Appendix B: the ITU-T T.81 Annex K Huffman tables. A JPEG over
/// RTP carries no DHT; the receiver writes these, so the sender's scan must
/// have been coded with them (an OV2640 does; `rusty_jpeg` does unless asked
/// for optimised tables).
pub mod huffman {
    /// One Huffman table: 16 code-length counts and the symbols in code order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Table {
        /// DHT class (0 = DC, 1 = AC) and destination id, as the `Tc|Th` byte.
        pub class_id: u8,
        /// Number of codes of each length 1..=16.
        pub codelens: &'static [u8; 16],
        /// The symbols, in code order.
        pub symbols: &'static [u8],
    }

    /// Luminance DC.
    pub const LUM_DC: Table = Table {
        class_id: 0x00,
        codelens: &[0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
        symbols: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
    };
    /// Luminance AC.
    pub const LUM_AC: Table = Table {
        class_id: 0x10,
        codelens: &[0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d],
        symbols: &[
            0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51,
            0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1,
            0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18,
            0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39,
            0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57,
            0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75,
            0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92,
            0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
            0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
            0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8,
            0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2,
            0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
        ],
    };
    /// Chrominance DC.
    pub const CHM_DC: Table = Table {
        class_id: 0x01,
        codelens: &[0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0],
        symbols: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
    };
    /// Chrominance AC.
    pub const CHM_AC: Table = Table {
        class_id: 0x11,
        codelens: &[0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77],
        symbols: &[
            0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07,
            0x61, 0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09,
            0x23, 0x33, 0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25,
            0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38,
            0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56,
            0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74,
            0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
            0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
            0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba,
            0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6,
            0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2,
            0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
        ],
    };
    /// All four, in the order the receiver writes them.
    pub const ALL: [Table; 4] = [LUM_DC, LUM_AC, CHM_DC, CHM_AC];

    /// The table a DHT `Tc|Th` byte selects, if it is one of the four.
    #[must_use]
    pub fn table_for(class_id: u8) -> Option<&'static Table> {
        ALL.iter().find(|t| t.class_id == class_id)
    }
}

/// Zigzag scan order: entry `i` is the natural (row-major) index of the
/// `i`-th coefficient in scan order. A DQT lists its 64 values in this order.
const ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// T.81 Table K.1, natural order (RFC 2435 Appendix A lists it this way).
const LUMA_QUANT: [u8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
/// T.81 Table K.2, natural order.
const CHROMA_QUANT: [u8; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

/// RFC 2435 Appendix A: the quantization tables a `Q` in `1..=127` stands
/// for, in the zigzag order a DQT uses (the RFC's listing is natural order;
/// the receivers that matter, ffmpeg among them, apply the zigzag).
#[must_use]
pub fn default_quant_tables(q: u8) -> [[u8; 64]; 2] {
    let q = u32::from(q.clamp(1, 99));
    let factor = if q < 50 { 5000 / q } else { 200 - q * 2 };
    let scale = |base: &[u8; 64]| {
        let mut out = [0u8; 64];
        for (i, slot) in out.iter_mut().enumerate() {
            let v = (u32::from(base[usize::from(ZIGZAG[i])]) * factor + 50) / 100;
            *slot = v.clamp(1, 255) as u8;
        }
        out
    };
    [scale(&LUMA_QUANT), scale(&CHROMA_QUANT)]
}

/// Bytes [`write_jpeg_headers`] emits for `qtables` tables, with or without a
/// DRI segment.
#[must_use]
pub const fn jpeg_header_len(qtables: usize, restart: bool) -> usize {
    // SOI + DQT + SOF0 + four DHTs + optional DRI + SOS
    2 + (4 + 65 * qtables) + 19 + (33 + 183 + 33 + 183) + if restart { 6 } else { 0 } + 14
}

/// Write everything a baseline JPEG needs before its scan: SOI, DQT, SOF0,
/// the four Annex K DHTs, DRI when `restart_interval` is set, and SOS. The
/// scan bytes then follow, and `FF D9` ends the file. `type_` is the RFC 2435
/// type without the restart bit (0 = 4:2:2, 1 = 4:2:0). Returns the length.
pub fn write_jpeg_headers(
    out: &mut [u8],
    width: u16,
    height: u16,
    type_: u8,
    restart_interval: Option<u16>,
    qtables: &[[u8; 64]],
) -> Result<usize> {
    if qtables.is_empty() || qtables.len() > 2 || type_ > 1 {
        return Err(Error::InvalidFormat);
    }
    let needed = jpeg_header_len(qtables.len(), restart_interval.is_some());
    if out.len() < needed {
        return Err(Error::BufferTooSmall { needed });
    }
    let mut p = 0usize;
    let mut put = |bytes: &[u8]| {
        out[p..p + bytes.len()].copy_from_slice(bytes);
        p += bytes.len();
    };
    put(&[0xFF, 0xD8]);
    // DQT
    put(&[0xFF, 0xDB]);
    put(&((2 + 65 * qtables.len()) as u16).to_be_bytes());
    for (id, t) in qtables.iter().enumerate() {
        put(&[id as u8]);
        put(t);
    }
    // SOF0: 8-bit, three components, chroma on table 1 when there is one
    let hv = if type_ == 0 { 0x21 } else { 0x22 };
    let ctab = u8::from(qtables.len() > 1);
    put(&[0xFF, 0xC0, 0, 17, 8]);
    put(&height.to_be_bytes());
    put(&width.to_be_bytes());
    put(&[3, 1, hv, 0, 2, 0x11, ctab, 3, 0x11, ctab]);
    // DHT x4
    for t in &huffman::ALL {
        put(&[0xFF, 0xC4]);
        put(&((2 + 1 + 16 + t.symbols.len()) as u16).to_be_bytes());
        put(&[t.class_id]);
        put(t.codelens);
        put(t.symbols);
    }
    if let Some(ri) = restart_interval {
        put(&[0xFF, 0xDD, 0, 4]);
        put(&ri.to_be_bytes());
    }
    // SOS
    put(&[0xFF, 0xDA, 0, 12, 3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);
    debug_assert_eq!(p, needed);
    Ok(p)
}

/// Bytes a [`JpegDepayloader`] keeps free in front of the scan for the
/// regenerated headers.
pub const HEADER_RESERVE: usize = 1024;

/// What one pushed packet did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depayload {
    /// A fragment of the frame in progress.
    Fragment,
    /// The frame is complete: [`JpegDepayloader::jpeg`] holds `len` bytes.
    Frame {
        /// JPEG length.
        len: usize,
        /// RTP timestamp (90 kHz).
        timestamp: u32,
        /// Width in pixels.
        width: u16,
        /// Height in pixels.
        height: u16,
    },
    /// A fragment was lost; the frame it belonged to is gone and the next
    /// frame start resynchronises.
    Dropped,
}

/// RFC 2435 receiver: rebuilds one JPEG at a time into a caller buffer.
///
/// The scan is written straight into the buffer after [`HEADER_RESERVE`]
/// bytes; on the marker packet the headers go in front of it and `FF D9`
/// after, so a complete frame is never copied. Loss is reported, not guessed:
/// a sequence gap or an offset that is not the next byte drops the frame.
#[derive(Debug)]
pub struct JpegDepayloader<'m> {
    buf: &'m mut [u8],
    scan_len: usize,
    in_frame: bool,
    timestamp: u32,
    width: u16,
    height: u16,
    type_: u8,
    restart_interval: Option<u16>,
    qtables: [[u8; 64]; 2],
    qcount: usize,
    last_seq: Option<u16>,
    jpeg: Option<(usize, usize)>,
    /// Complete frames delivered.
    pub frames: u32,
    /// Frames abandoned because a fragment never arrived.
    pub dropped: u32,
    /// Packets missing by sequence number.
    pub lost: u32,
}

impl<'m> JpegDepayloader<'m> {
    /// Over `buf`, which must hold [`HEADER_RESERVE`] plus the largest scan
    /// plus two bytes.
    pub fn new(buf: &'m mut [u8]) -> Result<Self> {
        if buf.len() < HEADER_RESERVE + 2 {
            return Err(Error::BufferTooSmall {
                needed: HEADER_RESERVE + 2,
            });
        }
        Ok(JpegDepayloader {
            buf,
            scan_len: 0,
            in_frame: false,
            timestamp: 0,
            width: 0,
            height: 0,
            type_: 0,
            restart_interval: None,
            qtables: [[0; 64]; 2],
            qcount: 0,
            last_seq: None,
            jpeg: None,
            frames: 0,
            dropped: 0,
            lost: 0,
        })
    }

    /// The last complete JPEG, until the next packet is pushed.
    #[must_use]
    pub fn jpeg(&self) -> &[u8] {
        match self.jpeg {
            Some((a, b)) => &self.buf[a..b],
            None => &[],
        }
    }

    /// Push one RTP packet (header included).
    pub fn push(&mut self, packet: &[u8]) -> Result<Depayload> {
        let (h, payload) = Header::parse(packet)?;
        if h.payload_type != PT_JPEG {
            return Err(Error::InvalidFormat);
        }
        self.jpeg = None;
        let mut gap = 0u16;
        if let Some(last) = self.last_seq {
            gap = h.seq.wrapping_sub(last).wrapping_sub(1);
            if gap != 0 && gap < 0x8000 {
                self.lost += u32::from(gap);
            } else if gap >= 0x8000 {
                gap = 0; // reordered or duplicate: not a loss
            }
        }
        self.last_seq = Some(h.seq);
        if payload.len() < 8 {
            return Err(Error::InvalidFormat);
        }
        let off = (usize::from(payload[1]) << 16)
            | (usize::from(payload[2]) << 8)
            | usize::from(payload[3]);
        let type_ = payload[4];
        let q = payload[5];
        let width = u16::from(payload[6]) * 8;
        let height = u16::from(payload[7]) * 8;
        if width == 0 || height == 0 || (type_ & 0x3F) > 1 {
            return Err(Error::Unsupported);
        }
        let mut p = 8usize;
        let mut restart = None;
        if type_ >= 64 {
            let r = payload.get(p..p + 4).ok_or(Error::InvalidFormat)?;
            restart = Some(u16::from_be_bytes([r[0], r[1]]));
            p += 4;
        }
        if off == 0 {
            if self.in_frame && self.scan_len > 0 {
                self.dropped += 1;
            }
            self.in_frame = true;
            self.scan_len = 0;
            self.timestamp = h.timestamp;
            self.width = width;
            self.height = height;
            self.type_ = type_ & 0x3F;
            self.restart_interval = restart;
            if q >= 128 {
                let qh = payload.get(p..p + 4).ok_or(Error::InvalidFormat)?;
                let precision = qh[1];
                let len = usize::from(u16::from_be_bytes([qh[2], qh[3]]));
                p += 4;
                match len {
                    0 => {
                        // Q = 255 with no tables: the previous frame's apply.
                        if self.qcount == 0 {
                            return Err(Error::InvalidFormat);
                        }
                    }
                    64 | 128 => {
                        if precision != 0 {
                            return Err(Error::Unsupported); // 16-bit tables
                        }
                        let tables = payload.get(p..p + len).ok_or(Error::InvalidFormat)?;
                        self.qcount = len / 64;
                        for (t, chunk) in self.qtables.iter_mut().zip(tables.chunks(64)) {
                            t.copy_from_slice(chunk);
                        }
                        p += len;
                    }
                    _ => return Err(Error::InvalidFormat),
                }
            } else if q == 0 {
                return Err(Error::InvalidFormat);
            } else {
                self.qtables = default_quant_tables(q);
                self.qcount = 2;
            }
        } else {
            if !self.in_frame {
                // Joined mid-frame; wait for the next frame start.
                return Ok(Depayload::Fragment);
            }
            if gap != 0 || off != self.scan_len || h.timestamp != self.timestamp {
                self.in_frame = false;
                self.dropped += 1;
                return Ok(Depayload::Dropped);
            }
        }
        let data = &payload[p..];
        let start = HEADER_RESERVE + self.scan_len;
        let end = start + data.len();
        if end + 2 > self.buf.len() {
            self.in_frame = false;
            self.dropped += 1;
            return Err(Error::BufferTooSmall { needed: end + 2 });
        }
        self.buf[start..end].copy_from_slice(data);
        self.scan_len += data.len();
        if !h.marker {
            return Ok(Depayload::Fragment);
        }
        let hlen = jpeg_header_len(self.qcount, self.restart_interval.is_some());
        let head = HEADER_RESERVE - hlen;
        write_jpeg_headers(
            &mut self.buf[head..HEADER_RESERVE],
            self.width,
            self.height,
            self.type_,
            self.restart_interval,
            &self.qtables[..self.qcount],
        )?;
        let eoi = HEADER_RESERVE + self.scan_len;
        self.buf[eoi] = 0xFF;
        self.buf[eoi + 1] = 0xD9;
        self.in_frame = false;
        self.frames += 1;
        self.jpeg = Some((head, eoi + 2));
        Ok(Depayload::Frame {
            len: eoi + 2 - head,
            timestamp: self.timestamp,
            width: self.width,
            height: self.height,
        })
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

    fn house_jpeg(w: u16, h: u16, optimised: bool) -> std::vec::Vec<u8> {
        let rgb: std::vec::Vec<u8> = (0..(w as usize * h as usize * 3))
            .map(|i| ((i * 7) ^ (i / 3)) as u8)
            .collect();
        let mut jpeg = std::vec::Vec::new();
        let mut enc = rusty_jpeg::encode::Encoder::new(&mut jpeg, 75);
        enc.set_optimized_huffman_tables(optimised);
        enc.encode(&rgb, w, h, rusty_jpeg::encode::ColorType::Rgb)
            .unwrap();
        jpeg
    }

    fn segments(jpeg: &[u8], marker: u8) -> std::vec::Vec<std::vec::Vec<u8>> {
        let mut out = std::vec::Vec::new();
        let mut i = 2;
        while i + 4 <= jpeg.len() && jpeg[i] == 0xFF {
            let m = jpeg[i + 1];
            if m == 0xDA {
                break;
            }
            let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
            if m == marker {
                out.push(jpeg[i + 4..i + 2 + len].to_vec());
            }
            i += 2 + len;
        }
        out
    }

    fn decode(jpeg: &[u8]) -> (u16, u16, std::vec::Vec<u8>) {
        let mut d = rusty_jpeg::Decoder::new(jpeg);
        let px = d.decode().unwrap();
        let info = d.info().unwrap();
        (info.width, info.height, px)
    }

    #[test]
    fn the_annex_k_tables_are_what_the_house_encoder_writes() {
        // rusty_jpeg is an independent transcription of T.81 Annex K: its DHT
        // segments must equal the constants the receiver regenerates.
        let jpeg = house_jpeg(64, 32, false);
        let mut seen = 0;
        for seg in segments(&jpeg, 0xC4) {
            let mut s = &seg[..];
            while !s.is_empty() {
                let t = huffman::table_for(s[0]).expect("one of the four");
                let nsym: usize = s[1..17].iter().map(|&c| usize::from(c)).sum();
                assert_eq!(&s[1..17], t.codelens, "codelens of {:#x}", s[0]);
                assert_eq!(&s[17..17 + nsym], t.symbols, "symbols of {:#x}", s[0]);
                s = &s[17 + nsym..];
                seen += 1;
            }
        }
        assert_eq!(seen, 4);
        for t in &huffman::ALL {
            let n: usize = t.codelens.iter().map(|&c| usize::from(c)).sum();
            assert_eq!(n, t.symbols.len());
        }
    }

    #[test]
    fn optimised_huffman_tables_are_refused_at_the_sender() {
        let jpeg = house_jpeg(64, 32, true);
        assert_eq!(JpegScan::parse(&jpeg).err(), Some(Error::Unsupported));
    }

    #[test]
    fn default_quant_tables_follow_the_rfc_in_zigzag_order() {
        let [l, c] = default_quant_tables(50);
        // Q = 50 is the unscaled Annex K set; ffmpeg's rtpdec_jpeg lists the
        // luma table pre-zigzagged and begins 16, 11, 12, 14, 12, 10, 16, 14.
        assert_eq!(&l[..8], &[16, 11, 12, 14, 12, 10, 16, 14]);
        assert_eq!(l[0], LUMA_QUANT[0]);
        assert_eq!(l[2], LUMA_QUANT[8]);
        assert_eq!(c[0], 17);
        assert_eq!(c[63], 99);
        // Q clamps to 99 (factor 2): 16 scales to 1, the largest entry to 2.
        let [l100, _] = default_quant_tables(100);
        assert_eq!(l100[0], 1);
        assert!(l100.iter().all(|&v| v <= 2), "{l100:?}");
        let [l1, _] = default_quant_tables(1);
        assert_eq!(l1[63], 255, "Q 1 clamps to 255");
        let [l25, _] = default_quant_tables(25);
        assert_eq!(l25[0], 32, "Q 25 is factor 200: (16 * 200 + 50) / 100");
    }

    #[test]
    fn jpeg_round_trips_through_the_depayloader_pixel_for_pixel() {
        let jpeg = house_jpeg(64, 32, false);
        let info = JpegScan::parse(&jpeg).unwrap();
        let mut p = JpegPayloader::new(9, 65530, 300).unwrap();
        let mut scratch = [0u8; 300];
        let mut pkts: std::vec::Vec<std::vec::Vec<u8>> = std::vec::Vec::new();
        p.packetize(&jpeg, Micros::from_millis(40), &mut scratch, |b| {
            pkts.push(b.to_vec());
            Ok(())
        })
        .unwrap();
        assert!(pkts.len() > 2, "{} packets", pkts.len());
        let mut buf = std::vec![0u8; HEADER_RESERVE + jpeg.len() + 2];
        let mut d = JpegDepayloader::new(&mut buf).unwrap();
        let mut result = None;
        for (k, pk) in pkts.iter().enumerate() {
            let r = d.push(pk).unwrap();
            if k + 1 < pkts.len() {
                assert_eq!(r, Depayload::Fragment);
            } else {
                result = Some(r);
            }
        }
        let Some(Depayload::Frame {
            len,
            timestamp,
            width,
            height,
        }) = result
        else {
            panic!("no frame: {result:?}");
        };
        assert_eq!((width, height), (64, 32));
        assert_eq!(timestamp, 3600);
        assert_eq!(d.frames, 1);
        assert_eq!((d.dropped, d.lost), (0, 0));
        let regen = d.jpeg().to_vec();
        assert_eq!(regen.len(), len);
        // Same scan, same tables, same DHTs; only the header layout differs.
        let back = JpegScan::parse(&regen).unwrap();
        assert_eq!(back.scan, info.scan);
        assert_eq!(
            back.qtables[..usize::from(info.qtable_count)],
            info.qtables[..usize::from(info.qtable_count)]
        );
        assert_eq!(
            (back.width, back.height, back.type_),
            (info.width, info.height, info.type_)
        );
        assert_eq!(
            segments(&regen, 0xC4).concat(),
            segments(&jpeg, 0xC4).concat()
        );
        // And the pixels: the house decoder sees the same image.
        assert_eq!(decode(&regen), decode(&jpeg));
    }

    #[test]
    fn a_lost_fragment_drops_the_frame_and_the_next_start_resyncs() {
        let jpeg = house_jpeg(64, 32, false);
        let mut p = JpegPayloader::new(9, 0, 300).unwrap();
        let mut scratch = [0u8; 300];
        let mut frame = |ts: u64| {
            let mut pkts: std::vec::Vec<std::vec::Vec<u8>> = std::vec::Vec::new();
            p.packetize(&jpeg, Micros::from_millis(ts), &mut scratch, |b| {
                pkts.push(b.to_vec());
                Ok(())
            })
            .unwrap();
            pkts
        };
        let f1 = frame(40);
        let f2 = frame(80);
        let mut buf = std::vec![0u8; HEADER_RESERVE + jpeg.len() + 2];
        let mut d = JpegDepayloader::new(&mut buf).unwrap();
        // frame 1 with its second packet missing
        assert_eq!(d.push(&f1[0]).unwrap(), Depayload::Fragment);
        assert_eq!(d.push(&f1[2]).unwrap(), Depayload::Dropped);
        assert_eq!(d.lost, 1);
        assert_eq!(d.dropped, 1);
        // the tail of frame 1 is ignored, frame 2 arrives whole
        for pk in &f1[3..] {
            assert_eq!(d.push(pk).unwrap(), Depayload::Fragment);
        }
        let mut last = Depayload::Fragment;
        for pk in &f2 {
            last = d.push(pk).unwrap();
        }
        assert!(
            matches!(
                last,
                Depayload::Frame {
                    timestamp: 7200,
                    ..
                }
            ),
            "{last:?}"
        );
        assert_eq!(d.frames, 1);
        assert_eq!(decode(d.jpeg()), decode(&jpeg));
    }

    #[test]
    fn headers_from_a_q_factor_decode_too() {
        // A sender using Q < 128 sends no tables; the receiver's defaults
        // must make a JPEG the house decoder accepts.
        let mut out = [0u8; 1024];
        let tables = default_quant_tables(75);
        let n = write_jpeg_headers(&mut out, 16, 8, 1, None, &tables).unwrap();
        assert_eq!(n, jpeg_header_len(2, false));
        let mut d = rusty_jpeg::Decoder::new(&out[..n]);
        d.read_info().unwrap();
        let info = d.info().unwrap();
        assert_eq!((info.width, info.height), (16, 8));
        let with_dri = write_jpeg_headers(&mut out, 16, 8, 0, Some(4), &tables[..1]).unwrap();
        assert_eq!(with_dri, jpeg_header_len(1, true));
        assert!(write_jpeg_headers(&mut [0u8; 100], 16, 8, 0, None, &tables).is_err());
    }
}
