//! V2 on the host: RTP/JPEG (RFC 2435) and raw Janus datagrams over
//! `std::net::UdpSocket`.
//!
//! The senders are the code the chip runs under ESP-IDF's `std::net`; the
//! receivers are the laptop end, with the loss counters the kill test asks
//! for. Both sides use the core's payloader / depayloader and framer /
//! reassembler unchanged, so a packet made here and a packet made on a board
//! are the same bytes.

use std::net::{ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use rusty_esp_video_core::esp_core::error::{Error, Result};
use rusty_esp_video_core::esp_core::time::Micros;
use rusty_esp_video_core::pacer::Budget;
use rusty_esp_video_core::packet::MediaPacket;
use rusty_esp_video_core::rtp::{Depayload, JpegDepayloader, JpegPayloader, HEADER_RESERVE};
use rusty_esp_video_core::udp::{self, Framer, Reassembler, Reassembly};

/// What a sender has done so far.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TxStats {
    /// Frames (or packets, for the raw sender) handed in.
    pub frames: u64,
    /// Datagrams sent.
    pub packets: u64,
    /// Bytes on the wire, headers included.
    pub bytes: u64,
    /// Frames dropped whole by the byte budget.
    pub dropped: u64,
}

/// RTP/JPEG sender: one `send_frame` per JPEG, MTU-sized packets to one
/// destination.
#[derive(Debug)]
pub struct RtpJpegSender {
    socket: UdpSocket,
    payloader: JpegPayloader,
    scratch: Vec<u8>,
    budget: Option<Budget>,
    /// Running totals.
    pub stats: TxStats,
}

impl RtpJpegSender {
    /// Bind `local`, connect to `dest`, packets of at most `mtu` bytes.
    pub fn bind(
        local: impl ToSocketAddrs,
        dest: impl ToSocketAddrs,
        ssrc: u32,
        mtu: usize,
    ) -> Result<Self> {
        let socket = UdpSocket::bind(local).map_err(|_| Error::Hardware)?;
        socket.connect(dest).map_err(|_| Error::Hardware)?;
        Ok(RtpJpegSender {
            socket,
            payloader: JpegPayloader::new(ssrc, (ssrc & 0xFFFF) as u16, mtu)?,
            scratch: vec![0u8; mtu],
            budget: None,
            stats: TxStats::default(),
        })
    }

    /// Cap the stream at `kbps` with `burst_ms` of headroom: a frame that
    /// does not fit is dropped whole and counted (see
    /// [`Budget`]). The budget counts wire bytes, headers included.
    #[must_use]
    pub fn with_budget(mut self, kbps: u32, burst_ms: u32) -> Self {
        self.budget = Some(Budget::new(kbps, burst_ms));
        self
    }

    /// The budget's own counters, when there is one.
    #[must_use]
    pub fn budget(&self) -> Option<&Budget> {
        self.budget.as_ref()
    }

    /// Wire bytes `payload` will take at this MTU, headers included.
    fn wire_len(&self, payload: usize) -> usize {
        let room = self
            .scratch
            .len()
            .saturating_sub(rusty_esp_video_core::rtp::HEADER_LEN + 8);
        let packets = payload.div_ceil(room.max(1));
        payload + packets * (rusty_esp_video_core::rtp::HEADER_LEN + 8) + 4 + 128
    }

    /// Send one JPEG captured at `timestamp`. Returns the packet count, 0
    /// when the budget dropped the frame.
    pub fn send_frame(&mut self, jpeg: &[u8], timestamp: Micros) -> Result<usize> {
        let wire = self.wire_len(jpeg.len());
        if let Some(b) = self.budget.as_mut() {
            if !b.admit(timestamp, wire) {
                self.stats.dropped += 1;
                return Ok(0);
            }
        }
        let socket = &self.socket;
        let stats = &mut self.stats;
        let n = self
            .payloader
            .packetize(jpeg, timestamp, &mut self.scratch, |pkt| {
                socket.send(pkt).map_err(|_| Error::Hardware)?;
                stats.packets += 1;
                stats.bytes += pkt.len() as u64;
                Ok(())
            })?;
        self.stats.frames += 1;
        Ok(n)
    }

    /// The bound local address.
    pub fn local_addr(&self) -> Result<std::net::SocketAddr> {
        self.socket.local_addr().map_err(|_| Error::Hardware)
    }
}

/// Raw Janus datagram sender: one `send_packet` per [`MediaPacket`], split
/// into MTU-sized datagrams with the 28-byte header.
#[derive(Debug)]
pub struct RawUdpSender {
    socket: UdpSocket,
    framer: Framer,
    scratch: Vec<u8>,
    budget: Option<Budget>,
    /// Running totals.
    pub stats: TxStats,
}

impl RawUdpSender {
    /// Bind `local`, connect to `dest`, datagrams of at most `mtu` bytes.
    pub fn bind(local: impl ToSocketAddrs, dest: impl ToSocketAddrs, mtu: usize) -> Result<Self> {
        let socket = UdpSocket::bind(local).map_err(|_| Error::Hardware)?;
        socket.connect(dest).map_err(|_| Error::Hardware)?;
        Ok(RawUdpSender {
            socket,
            framer: Framer::new(mtu)?,
            scratch: vec![0u8; mtu],
            budget: None,
            stats: TxStats::default(),
        })
    }

    /// Cap the stream at `kbps` with `burst_ms` of headroom; see
    /// [`RtpJpegSender::with_budget`].
    #[must_use]
    pub fn with_budget(mut self, kbps: u32, burst_ms: u32) -> Self {
        self.budget = Some(Budget::new(kbps, burst_ms));
        self
    }

    /// The budget's own counters, when there is one.
    #[must_use]
    pub fn budget(&self) -> Option<&Budget> {
        self.budget.as_ref()
    }

    /// Send one packet. Returns the datagram count, 0 when the budget
    /// dropped it.
    pub fn send_packet(&mut self, packet: &MediaPacket<'_>) -> Result<usize> {
        let room = self.scratch.len().saturating_sub(udp::HEADER_LEN).max(1);
        let wire = packet.len() + packet.len().div_ceil(room) * udp::HEADER_LEN;
        if let Some(b) = self.budget.as_mut() {
            if !b.admit(packet.timestamp, wire) {
                self.stats.dropped += 1;
                return Ok(0);
            }
        }
        let socket = &self.socket;
        let stats = &mut self.stats;
        let n = self.framer.frame(packet, &mut self.scratch, |d| {
            socket.send(d).map_err(|_| Error::Hardware)?;
            stats.packets += 1;
            stats.bytes += d.len() as u64;
            Ok(())
        })?;
        self.stats.frames += 1;
        Ok(n)
    }
}

/// What a receiver saw.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RxStats {
    /// Datagrams received.
    pub packets: u64,
    /// Bytes received, headers included.
    pub bytes: u64,
    /// Complete frames (RTP) or complete packets (raw) delivered.
    pub frames: u64,
    /// Datagrams missing by sequence (RTP) or packets abandoned for a missing
    /// fragment (raw).
    pub lost: u64,
    /// Frames abandoned because a fragment never arrived (RTP only).
    pub dropped: u64,
    /// Datagrams that did not parse.
    pub bad: u64,
    /// Wall micros since the receiver started, at the first and last frame.
    pub first_frame_at: Option<u64>,
    /// Last frame.
    pub last_frame_at: Option<u64>,
}

impl RxStats {
    /// Frames per second between the first and last frame.
    #[must_use]
    pub fn fps(&self) -> f64 {
        match (self.first_frame_at, self.last_frame_at) {
            (Some(a), Some(b)) if b > a && self.frames > 1 => {
                (self.frames - 1) as f64 / ((b - a) as f64 / 1e6)
            }
            _ => 0.0,
        }
    }

    fn frame_at(&mut self, started: Instant) {
        let now = started.elapsed().as_micros() as u64;
        self.first_frame_at.get_or_insert(now);
        self.last_frame_at = Some(now);
        self.frames += 1;
    }
}

/// When a receiver stops.
#[derive(Debug, Clone, Copy)]
pub struct Until {
    /// Stop after this many frames (0 = no limit).
    pub frames: u64,
    /// Stop after this long.
    pub for_at_most: Duration,
}

const DATAGRAM_MAX: usize = 65_536;
const POLL: Duration = Duration::from_millis(250);

/// Receive RTP/JPEG on `socket` until `until`, calling `on_frame(jpeg,
/// rtp_timestamp, width, height)` for each complete frame. `buf` must hold
/// [`HEADER_RESERVE`] plus the largest scan plus two bytes.
pub fn receive_rtp_jpeg(
    socket: &UdpSocket,
    buf: &mut [u8],
    until: Until,
    mut on_frame: impl FnMut(&[u8], u32, u16, u16),
) -> Result<RxStats> {
    if buf.len() < HEADER_RESERVE + 2 {
        return Err(Error::BufferTooSmall {
            needed: HEADER_RESERVE + 2,
        });
    }
    socket
        .set_read_timeout(Some(POLL))
        .map_err(|_| Error::Hardware)?;
    let started = Instant::now();
    let mut stats = RxStats::default();
    let mut d = JpegDepayloader::new(buf)?;
    let mut datagram = vec![0u8; DATAGRAM_MAX];
    while started.elapsed() < until.for_at_most
        && (until.frames == 0 || stats.frames < until.frames)
    {
        let n = match socket.recv(&mut datagram) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(_) => return Err(Error::Hardware),
        };
        stats.packets += 1;
        stats.bytes += n as u64;
        match d.push(&datagram[..n]) {
            Ok(Depayload::Frame {
                timestamp,
                width,
                height,
                ..
            }) => {
                stats.frame_at(started);
                on_frame(d.jpeg(), timestamp, width, height);
            }
            Ok(_) => {}
            Err(_) => stats.bad += 1,
        }
    }
    stats.lost = u64::from(d.lost);
    stats.dropped = u64::from(d.dropped);
    Ok(stats)
}

/// Receive raw Janus datagrams on `socket` until `until`, calling
/// `on_packet(header, bytes)` for each complete packet. `buf` must hold the
/// largest packet.
pub fn receive_raw(
    socket: &UdpSocket,
    buf: &mut [u8],
    until: Until,
    mut on_packet: impl FnMut(&udp::Header, &[u8]),
) -> Result<RxStats> {
    socket
        .set_read_timeout(Some(POLL))
        .map_err(|_| Error::Hardware)?;
    let started = Instant::now();
    let mut stats = RxStats::default();
    let mut r = Reassembler::new(buf);
    let mut datagram = vec![0u8; DATAGRAM_MAX];
    while started.elapsed() < until.for_at_most
        && (until.frames == 0 || stats.frames < until.frames)
    {
        let n = match socket.recv(&mut datagram) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(_) => return Err(Error::Hardware),
        };
        stats.packets += 1;
        stats.bytes += n as u64;
        match r.push(&datagram[..n]) {
            Ok(Reassembly::Complete(h)) => {
                stats.frame_at(started);
                on_packet(&h, r.packet(&h));
            }
            Ok(_) => {}
            Err(_) => stats.bad += 1,
        }
    }
    stats.lost = r.lost;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_esp_video_core::packet::Codec;

    #[test]
    fn a_budgeted_sender_drops_whole_frames_and_the_receiver_sees_only_whole_ones() {
        // 40 packets of 6 000 bytes every 100 ms of device time = 480 kbit/s
        // against a 200 kbit/s cap with half a second of burst. The receiver
        // runs concurrently: seventeen 6 kB packets sent back to back would
        // overflow a loopback socket's buffer before a late receiver drained it.
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dest = rx.local_addr().unwrap();
        let payload: Vec<u8> = (0..6_000u32).map(|i| (i % 253) as u8).collect();
        let expected = payload.clone();
        let receiver = std::thread::spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            let mut got = Vec::new();
            let stats = receive_raw(
                &rx,
                &mut buf,
                Until {
                    frames: 0,
                    for_at_most: Duration::from_millis(1500),
                },
                |h, bytes| {
                    assert_eq!(bytes, &expected[..], "whole frames only");
                    got.push(h.timestamp.0 / 100_000);
                },
            )
            .unwrap();
            (stats, got)
        });
        let mut tx = RawUdpSender::bind("127.0.0.1:0", dest, 1200)
            .unwrap()
            .with_budget(200, 500);
        let mut sent = Vec::new();
        for n in 0..40u64 {
            let p = MediaPacket::new(Codec::Jpeg, true, Micros(n * 100_000), &payload);
            if tx.send_packet(&p).unwrap() > 0 {
                sent.push(n);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(tx.stats.frames + tx.stats.dropped, 40);
        assert_eq!(tx.stats.frames as usize, sent.len());
        assert!(tx.stats.dropped >= 20, "{:?}", tx.stats);
        let b = tx.budget().unwrap();
        // four seconds of device time at 25 000 B/s plus the burst bound the bytes admitted
        assert!(
            b.bytes_admitted <= 25_000 * 4 + 12_500,
            "{}",
            b.bytes_admitted
        );
        let (stats, got) = receiver.join().unwrap();
        assert_eq!(stats.frames as usize, sent.len());
        assert_eq!((stats.lost, stats.bad), (0, 0));
        assert_eq!(got, sent, "exactly the admitted frames, in order");
    }

    #[test]
    fn raw_sender_and_receiver_agree_over_loopback() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dest = rx.local_addr().unwrap();
        let mut tx = RawUdpSender::bind("127.0.0.1:0", dest, 200).unwrap();
        let payload: Vec<u8> = (0..1500u32).map(|i| (i % 251) as u8).collect();
        for n in 0..5u64 {
            let p = MediaPacket::new(Codec::Jpeg, n == 0, Micros(n * 33_333), &payload);
            assert_eq!(tx.send_packet(&p).unwrap(), 9);
        }
        assert_eq!(tx.stats.frames, 5);
        assert_eq!(tx.stats.packets, 45);
        let mut buf = vec![0u8; 4096];
        let mut got = Vec::new();
        let stats = receive_raw(
            &rx,
            &mut buf,
            Until {
                frames: 5,
                for_at_most: Duration::from_secs(5),
            },
            |h, bytes| got.push((h.seq, h.timestamp, h.key, bytes.to_vec())),
        )
        .unwrap();
        assert_eq!(stats.frames, 5);
        assert_eq!(stats.packets, 45);
        assert_eq!((stats.lost, stats.bad), (0, 0));
        assert_eq!(got.len(), 5);
        for (n, (seq, ts, key, bytes)) in got.iter().enumerate() {
            assert_eq!(*seq as u64, n as u64);
            assert_eq!(ts.0, n as u64 * 33_333);
            assert_eq!(*key, n == 0);
            assert_eq!(bytes, &payload);
        }
    }
}
