//! An MPEG-2 transport stream mux for H.264 — the container `rff -i udp://`
//! and `ffmpeg` read today, so a device stream can be verified end to end
//! with the tools the house already trusts.
//!
//! One program, one video elementary stream on PID `0x100` (also the PCR
//! PID), PAT and PMT before every key frame and at least every
//! [`PSI_INTERVAL_PACKETS`] packets, PES with PTS only (no B-frames on a
//! chip), an access unit delimiter inserted when the encoder did not, PCR on
//! the first packet of every access unit, stuffing through the adaptation
//! field. Nothing here allocates: packets are assembled in a 188-byte stack
//! buffer and streamed to the sink.

use rusty_esp_core::error::{Error, Result};
use rusty_esp_core::time::Micros;

use crate::annexb::{AUD_NAL, contains_idr, starts_with_aud};
use crate::packet::{Codec, MediaPacket};
use crate::sink::PacketSink;

/// Transport packet length.
pub const PACKET_LEN: usize = 188;

/// Program association table PID.
pub const PID_PAT: u16 = 0x0000;

/// Program map table PID.
pub const PID_PMT: u16 = 0x1000;

/// The video elementary stream (and PCR) PID.
pub const PID_VIDEO: u16 = 0x0100;

/// Stream type for H.264 (ITU-T Rec. H.222.0).
pub const STREAM_TYPE_H264: u8 = 0x1B;

/// PES stream id for the first video stream.
pub const STREAM_ID_VIDEO: u8 = 0xE0;

/// Emit PAT + PMT at least this often, counted in transport packets.
pub const PSI_INTERVAL_PACKETS: u32 = 64;

/// PCR lags PTS by this many 90 kHz ticks (300 ms), so decoders always see
/// a PTS in the future.
const PCR_DELAY_TICKS: u64 = 27_000;

const SYNC: u8 = 0x47;

/// The mux.
#[derive(Debug)]
pub struct Mux<S> {
    sink: S,
    cc: [u8; 3], // PAT, PMT, video
    packets: u64,
    since_psi: u32,
    psi_written: u64,
    started: bool,
}

#[derive(Clone, Copy)]
enum Pid {
    Pat,
    Pmt,
    Video,
}

impl Pid {
    fn number(self) -> u16 {
        match self {
            Pid::Pat => PID_PAT,
            Pid::Pmt => PID_PMT,
            Pid::Video => PID_VIDEO,
        }
    }

    fn index(self) -> usize {
        match self {
            Pid::Pat => 0,
            Pid::Pmt => 1,
            Pid::Video => 2,
        }
    }
}

/// CRC-32/MPEG-2: polynomial 0x04C11DB7, init all ones, no reflection, no final xor.
#[must_use]
pub fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// A 90 kHz presentation timestamp from a device timestamp, 33 bits.
#[must_use]
pub fn pts_90k(micros: Micros) -> u64 {
    (micros.0 * 9 / 100) & 0x1_FFFF_FFFF
}

impl<S: PacketSink> Mux<S> {
    /// Mux into `sink`.
    pub fn new(sink: S) -> Self {
        Mux {
            sink,
            cc: [0; 3],
            packets: 0,
            since_psi: 0,
            psi_written: 0,
            started: false,
        }
    }

    /// Transport packets written.
    #[must_use]
    pub fn packets(&self) -> u64 {
        self.packets
    }

    /// PAT+PMT pairs written.
    #[must_use]
    pub fn psi_written(&self) -> u64 {
        self.psi_written
    }

    /// Give the sink back.
    pub fn into_sink(self) -> S {
        self.sink
    }

    /// Mux one H.264 access unit (Annex-B).
    pub fn push(&mut self, packet: &MediaPacket<'_>) -> Result<()> {
        if packet.codec != Codec::H264 || packet.is_empty() {
            return Err(Error::Unsupported);
        }
        let key = packet.key || contains_idr(packet.data);
        if !self.started || key || self.since_psi >= PSI_INTERVAL_PACKETS {
            self.write_psi()?;
            self.started = true;
        }
        let pts = pts_90k(packet.timestamp);
        let pcr = pts.saturating_sub(PCR_DELAY_TICKS);
        let pes_header = pes_header(pts);
        let aud: &[u8] = if starts_with_aud(packet.data) {
            &[]
        } else {
            &AUD_NAL
        };
        let mut reader = Chunks::new([&pes_header, aud, packet.data]);
        let mut buf = [0u8; PACKET_LEN];
        let mut first = true;
        while reader.remaining() > 0 {
            let remaining = reader.remaining();
            let (pusi, af_len_field) = if first {
                // adaptation field with PCR (7 bytes) plus stuffing if the AU is short
                let l = (183 - remaining.min(176)).max(7);
                (true, Some(l))
            } else if remaining >= 184 {
                (false, None)
            } else {
                (false, Some(183 - remaining))
            };
            self.write_header(&mut buf, Pid::Video, pusi, af_len_field.is_some());
            let mut p = 4;
            if let Some(l) = af_len_field {
                buf[p] = l as u8;
                p += 1;
                if l >= 1 {
                    let mut flags = 0u8;
                    if first {
                        flags |= 0x10; // PCR
                        if key {
                            flags |= 0x40; // random access indicator
                        }
                    }
                    buf[p] = flags;
                    p += 1;
                    if first {
                        write_pcr(&mut buf[p..p + 6], pcr);
                        p += 6;
                    }
                    let used = if first { 7 } else { 1 };
                    for b in &mut buf[p..p + (l - used)] {
                        *b = 0xFF;
                    }
                    p += l - used;
                }
            }
            let take = PACKET_LEN - p;
            reader.read(&mut buf[p..p + take]);
            self.emit(&buf)?;
            first = false;
        }
        Ok(())
    }

    fn write_header(&mut self, buf: &mut [u8; PACKET_LEN], pid: Pid, pusi: bool, adaptation: bool) {
        let cc = &mut self.cc[pid.index()];
        buf[0] = SYNC;
        let pidn = pid.number();
        buf[1] = ((pidn >> 8) as u8 & 0x1F) | if pusi { 0x40 } else { 0 };
        buf[2] = pidn as u8;
        buf[3] = (if adaptation { 0x30 } else { 0x10 }) | (*cc & 0x0F);
        *cc = (*cc + 1) & 0x0F;
    }

    fn emit(&mut self, buf: &[u8; PACKET_LEN]) -> Result<()> {
        self.sink.write(buf)?;
        self.packets += 1;
        self.since_psi += 1;
        Ok(())
    }

    fn write_psi(&mut self) -> Result<()> {
        let mut buf = [0xFFu8; PACKET_LEN];
        // PAT
        self.write_header(&mut buf, Pid::Pat, true, false);
        buf[4] = 0; // pointer field
        let n = write_pat(&mut buf[5..]);
        for b in &mut buf[5 + n..] {
            *b = 0xFF;
        }
        self.emit(&buf)?;
        // PMT
        let mut buf = [0xFFu8; PACKET_LEN];
        self.write_header(&mut buf, Pid::Pmt, true, false);
        buf[4] = 0;
        let n = write_pmt(&mut buf[5..]);
        for b in &mut buf[5 + n..] {
            *b = 0xFF;
        }
        self.emit(&buf)?;
        self.since_psi = 0;
        self.psi_written += 1;
        Ok(())
    }
}

fn write_pat(out: &mut [u8]) -> usize {
    let section: [u8; 12] = [
        0x00, // table_id
        0xB0,
        0x0D, // syntax=1, length=13
        0x00,
        0x01, // transport_stream_id
        0xC1, // version 0, current
        0x00,
        0x00, // section 0 of 0
        0x00,
        0x01, // program 1
        0xE0 | ((PID_PMT >> 8) as u8 & 0x1F),
        PID_PMT as u8,
    ];
    out[..12].copy_from_slice(&section);
    out[12..16].copy_from_slice(&crc32_mpeg2(&section).to_be_bytes());
    16
}

fn write_pmt(out: &mut [u8]) -> usize {
    let section: [u8; 17] = [
        0x02, // table_id
        0xB0,
        0x12, // syntax=1, length=18
        0x00,
        0x01, // program 1
        0xC1, // version 0, current
        0x00,
        0x00, // section 0 of 0
        0xE0 | ((PID_VIDEO >> 8) as u8 & 0x1F),
        PID_VIDEO as u8, // PCR PID
        0xF0,
        0x00, // program_info_length 0
        STREAM_TYPE_H264,
        0xE0 | ((PID_VIDEO >> 8) as u8 & 0x1F),
        PID_VIDEO as u8,
        0xF0,
        0x00, // ES_info_length 0
    ];
    out[..17].copy_from_slice(&section);
    out[17..21].copy_from_slice(&crc32_mpeg2(&section).to_be_bytes());
    21
}

/// A 14-byte PES header: start code, stream id, unbounded length, flags,
/// PTS only.
fn pes_header(pts: u64) -> [u8; 14] {
    [
        0x00,
        0x00,
        0x01,
        STREAM_ID_VIDEO,
        0x00,
        0x00, // PES_packet_length: unbounded for video
        0x84, // '10', no scrambling, priority 0, data_alignment 1
        0x80, // PTS present, DTS absent
        0x05, // header data length
        0x20 | (((pts >> 29) & 0x0E) as u8) | 0x01,
        ((pts >> 22) & 0xFF) as u8,
        (((pts >> 14) & 0xFE) as u8) | 0x01,
        ((pts >> 7) & 0xFF) as u8,
        (((pts << 1) & 0xFE) as u8) | 0x01,
    ]
}

fn write_pcr(out: &mut [u8], base: u64) {
    let base = base & 0x1_FFFF_FFFF;
    out[0] = (base >> 25) as u8;
    out[1] = (base >> 17) as u8;
    out[2] = (base >> 9) as u8;
    out[3] = (base >> 1) as u8;
    out[4] = (((base & 1) as u8) << 7) | 0x7E; // reserved bits, extension high bit 0
    out[5] = 0x00; // extension low byte
}

/// Reads sequentially across up to three slices without copying them together.
struct Chunks<'a> {
    parts: [&'a [u8]; 3],
    idx: usize,
    off: usize,
}

impl<'a> Chunks<'a> {
    fn new(parts: [&'a [u8]; 3]) -> Self {
        Chunks {
            parts,
            idx: 0,
            off: 0,
        }
    }

    fn remaining(&self) -> usize {
        let mut n = self.parts[self.idx..]
            .iter()
            .map(|p| p.len())
            .sum::<usize>();
        n -= self.off;
        n
    }

    fn read(&mut self, out: &mut [u8]) {
        let mut written = 0;
        while written < out.len() && self.idx < self.parts.len() {
            let part = self.parts[self.idx];
            let avail = part.len() - self.off;
            if avail == 0 {
                self.idx += 1;
                self.off = 0;
                continue;
            }
            let take = avail.min(out.len() - written);
            out[written..written + take].copy_from_slice(&part[self.off..self.off + take]);
            written += take;
            self.off += take;
        }
    }
}

/// A test- and host-side demuxer: enough of a TS reader to check the mux.
#[cfg(feature = "alloc")]
pub mod demux {
    use alloc::vec::Vec;

    use super::{PACKET_LEN, PID_PAT, PID_PMT, PID_VIDEO, SYNC};

    /// What a transport stream said about itself.
    #[derive(Debug, Default, Clone, PartialEq, Eq)]
    pub struct Report {
        /// Access units (PES payloads) recovered from the video PID, in order.
        pub access_units: Vec<Vec<u8>>,
        /// PTS values in 90 kHz, one per access unit.
        pub pts: Vec<u64>,
        /// Stream type declared in the PMT for the video PID.
        pub stream_type: Option<u8>,
        /// PCR values seen, in 90 kHz base ticks.
        pub pcr: Vec<u64>,
        /// Continuity-counter errors per PID (PAT, PMT, video).
        pub cc_errors: [u32; 3],
        /// Transport packets seen.
        pub packets: usize,
    }

    /// Parse a whole stream.
    pub fn parse(ts: &[u8]) -> Result<Report, &'static str> {
        if ts.len() % PACKET_LEN != 0 {
            return Err("length is not a multiple of 188");
        }
        let mut r = Report::default();
        let mut expect_cc: [Option<u8>; 3] = [None; 3];
        let mut current: Option<(Vec<u8>, u64)> = None;
        for pkt in ts.chunks_exact(PACKET_LEN) {
            r.packets += 1;
            if pkt[0] != SYNC {
                return Err("bad sync byte");
            }
            let pusi = pkt[1] & 0x40 != 0;
            let pid = (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2]);
            let afc = (pkt[3] >> 4) & 0x3;
            let cc = pkt[3] & 0x0F;
            let idx = match pid {
                PID_PAT => 0,
                PID_PMT => 1,
                PID_VIDEO => 2,
                _ => return Err("unexpected pid"),
            };
            if let Some(e) = expect_cc[idx] {
                if e != cc {
                    r.cc_errors[idx] += 1;
                }
            }
            expect_cc[idx] = Some((cc + 1) & 0x0F);
            let mut p = 4;
            if afc & 0x2 != 0 {
                let l = pkt[4] as usize;
                if l >= 7 && pkt[5] & 0x10 != 0 {
                    let b = &pkt[6..12];
                    let base = (u64::from(b[0]) << 25)
                        | (u64::from(b[1]) << 17)
                        | (u64::from(b[2]) << 9)
                        | (u64::from(b[3]) << 1)
                        | u64::from(b[4] >> 7);
                    r.pcr.push(base);
                }
                p += 1 + l;
            }
            if afc & 0x1 == 0 {
                continue;
            }
            let payload = &pkt[p..];
            match pid {
                PID_PMT if pusi => {
                    let sec = &payload[1 + payload[0] as usize..];
                    // table_id(1) len(2) prog(2) ver(1) sec(2) pcr(2) pil(2) then ES loop
                    let pil = (usize::from(sec[10] & 0x0F) << 8) | usize::from(sec[11]);
                    let es = &sec[12 + pil..];
                    let es_pid = (u16::from(es[1] & 0x1F) << 8) | u16::from(es[2]);
                    if es_pid == PID_VIDEO {
                        r.stream_type = Some(es[0]);
                    }
                }
                PID_VIDEO => {
                    if pusi {
                        if let Some((au, pts)) = current.take() {
                            r.access_units.push(au);
                            r.pts.push(pts);
                        }
                        if payload.len() < 9 || payload[..4] != [0, 0, 1, 0xE0] {
                            return Err("bad PES start");
                        }
                        let hdl = payload[8] as usize;
                        let pts = if payload[7] & 0x80 != 0 {
                            let b = &payload[9..14];
                            (u64::from(b[0] & 0x0E) << 29)
                                | (u64::from(b[1]) << 22)
                                | (u64::from(b[2] & 0xFE) << 14)
                                | (u64::from(b[3]) << 7)
                                | u64::from(b[4] >> 1)
                        } else {
                            0
                        };
                        current = Some((payload[9 + hdl..].to_vec(), pts));
                    } else if let Some((au, _)) = current.as_mut() {
                        au.extend_from_slice(payload);
                    }
                }
                _ => {}
            }
        }
        if let Some((au, pts)) = current.take() {
            r.access_units.push(au);
            r.pts.push(pts);
        }
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annexb::AUD_NAL;

    #[test]
    fn crc_matches_the_reference_vector() {
        // CRC-32/MPEG-2 check value for "123456789" is 0x0376E6E7.
        assert_eq!(crc32_mpeg2(b"123456789"), 0x0376_E6E7);
    }

    #[test]
    fn pts_and_pes_encoding() {
        assert_eq!(pts_90k(Micros::from_millis(1000)), 90_000);
        let h = pes_header(90_000);
        assert_eq!(&h[..4], &[0, 0, 1, 0xE0]);
        assert_eq!(h[8], 5);
        // decode the PTS back
        let b = &h[9..14];
        let pts = (u64::from(b[0] & 0x0E) << 29)
            | (u64::from(b[1]) << 22)
            | (u64::from(b[2] & 0xFE) << 14)
            | (u64::from(b[3]) << 7)
            | u64::from(b[4] >> 1);
        assert_eq!(pts, 90_000);
        assert_eq!(b[0] & 0xF1, 0x21, "marker bits");
    }

    #[test]
    fn mux_round_trips_through_the_demuxer() {
        let mut ts: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        let mut mux = Mux::new(&mut ts);
        // three synthetic AUs: a big keyframe, a tiny P, a medium P
        let idr: alloc::vec::Vec<u8> = [
            0u8, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xCE, 0, 0, 0, 1, 0x65,
        ]
        .iter()
        .copied()
        .chain(core::iter::repeat_n(0x5Au8, 1000))
        .collect();
        let p1: alloc::vec::Vec<u8> = [0u8, 0, 0, 1, 0x41, 1, 2, 3].to_vec();
        let p2: alloc::vec::Vec<u8> = [0u8, 0, 0, 1, 0x41]
            .iter()
            .copied()
            .chain(core::iter::repeat_n(0x3Cu8, 300))
            .collect();
        let aus = [(&idr, true, 0u64), (&p1, false, 40), (&p2, false, 80)];
        for (au, key, ms) in &aus {
            mux.push(&MediaPacket::new(
                Codec::H264,
                *key,
                Micros::from_millis(*ms),
                au,
            ))
            .unwrap();
        }
        let packets = mux.packets();
        assert_eq!(mux.psi_written(), 1, "PSI once: one keyframe, few packets");
        let _ = mux.into_sink();
        assert_eq!(ts.len() % PACKET_LEN, 0);
        assert_eq!(ts.len() / PACKET_LEN, packets as usize);

        let r = demux::parse(&ts).unwrap();
        assert_eq!(r.stream_type, Some(STREAM_TYPE_H264));
        assert_eq!(r.cc_errors, [0, 0, 0]);
        assert_eq!(r.access_units.len(), 3);
        for (i, (au, _, ms)) in aus.iter().enumerate() {
            let mut expected = AUD_NAL.to_vec();
            expected.extend_from_slice(au);
            assert_eq!(
                r.access_units[i], expected,
                "AU {i} byte-identical after AUD insertion"
            );
            assert_eq!(r.pts[i], pts_90k(Micros::from_millis(*ms)));
        }
        assert_eq!(r.pcr.len(), 3, "one PCR per access unit");
        assert!(r.pcr[0] <= r.pts[0]);

        // an AU that already carries an AUD is not given a second one
        let mut ts2: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        let mut mux = Mux::new(&mut ts2);
        let mut with_aud = AUD_NAL.to_vec();
        with_aud.extend_from_slice(&p1);
        mux.push(&MediaPacket::new(
            Codec::H264,
            true,
            Micros::ZERO,
            &with_aud,
        ))
        .unwrap();
        let _ = mux.into_sink();
        let r = demux::parse(&ts2).unwrap();
        assert_eq!(r.access_units[0], with_aud);

        // wrong codec refused
        let mut ts3: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        let mut mux = Mux::new(&mut ts3);
        assert_eq!(
            mux.push(&MediaPacket::new(
                Codec::Jpeg,
                true,
                Micros::ZERO,
                &[0xFF, 0xD8]
            )),
            Err(Error::Unsupported)
        );
    }

    #[test]
    fn psi_repeats_on_the_interval() {
        let mut ts: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        let mut mux = Mux::new(&mut ts);
        let p: alloc::vec::Vec<u8> = [0u8, 0, 0, 1, 0x41]
            .iter()
            .copied()
            .chain(core::iter::repeat_n(0x11u8, 20_000))
            .collect();
        mux.push(&MediaPacket::new(Codec::H264, true, Micros::ZERO, &p))
            .unwrap();
        mux.push(&MediaPacket::new(
            Codec::H264,
            false,
            Micros::from_millis(40),
            &p,
        ))
        .unwrap();
        assert_eq!(
            mux.psi_written(),
            2,
            "second AU crossed the packet interval"
        );
    }
}
