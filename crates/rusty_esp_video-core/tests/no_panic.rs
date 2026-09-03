//! The robustness gate: every parser fed from a socket returns an error on
//! bad input; it never panics. Random datagrams and streams from an LCG,
//! under `catch_unwind` so a failure names the parser and prints the input.

use std::panic::{AssertUnwindSafe, catch_unwind};

use rusty_esp_video_core::annexb;
use rusty_esp_video_core::mpegts::demux;
use rusty_esp_video_core::rtp::{self, JpegDepayloader, JpegScan};
use rusty_esp_video_core::udp::{self, Reassembler};

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn bytes(&mut self, max_len: usize) -> Vec<u8> {
        let n = self.below(max_len + 1);
        (0..n).map(|_| (self.next() >> 56) as u8).collect()
    }

    /// Bytes with a bias toward the values framing looks for.
    fn framed(&mut self, max_len: usize) -> Vec<u8> {
        let mut v = self.bytes(max_len);
        for b in v.iter_mut() {
            match self.below(8) {
                0 => *b = 0x00,
                1 => *b = 0x01,
                2 => *b = 0xFF,
                3 => *b = 0x47,
                _ => {}
            }
        }
        v
    }
}

fn check<R>(name: &str, input: &[u8], f: impl FnOnce() -> R) {
    if catch_unwind(AssertUnwindSafe(f)).is_err() {
        let hex: String = input.iter().take(256).map(|b| format!("{b:02x}")).collect();
        panic!("{name} panicked on {} bytes: {hex}", input.len());
    }
}

#[test]
fn rtp_parsers_never_panic() {
    let mut rng = Lcg(0x71D0_0001);
    let mut buf = vec![0u8; rtp::HEADER_RESERVE + 64 * 1024];
    let mut depay = JpegDepayloader::new(&mut buf).unwrap();
    for i in 0..20_000 {
        let packet = if i % 2 == 0 {
            rng.bytes(1500)
        } else {
            let mut p = rng.framed(1500);
            if p.len() >= 12 {
                p[0] = 0x80; // RTP v2, the shape a real packet has
                p[1] = 26 | if i % 4 == 0 { 0x80 } else { 0 };
            }
            p
        };
        check("rtp::Header::parse", &packet, || {
            rtp::Header::parse(&packet).map(|(h, rest)| (h.timestamp, rest.len()))
        });
        check("JpegDepayloader::push", &packet, || {
            let _ = depay.push(&packet);
        });
        check("JpegScan::parse", &packet, || {
            JpegScan::parse(&packet).map(|_| ())
        });
    }
}

#[test]
fn udp_framing_never_panics() {
    let mut rng = Lcg(0x71D0_0002);
    let mut buf = vec![0u8; 256 * 1024];
    let mut reasm = Reassembler::new(&mut buf);
    for _ in 0..30_000 {
        let datagram = rng.framed(1500);
        check("udp::Header::parse", &datagram, || {
            udp::Header::parse(&datagram).map(|_| ())
        });
        check("Reassembler::push", &datagram, || {
            let _ = reasm.push(&datagram);
        });
    }
}

#[test]
fn annexb_and_ts_never_panic() {
    let mut rng = Lcg(0x71D0_0003);
    for i in 0..5_000 {
        let stream = rng.framed(4_000);
        check("annexb iterators", &stream, || {
            let _ = annexb::nal_units(&stream).count();
            let _ = annexb::nal_spans(&stream).count();
            let _ = annexb::access_units(&stream).count();
            let _ = annexb::contains_idr(&stream);
            let _ = annexb::starts_with_aud(&stream);
            for nal in annexb::nal_units(&stream) {
                let _ = annexb::nal_unit_type(nal);
                let _ = annexb::is_vcl(nal);
                let _ = annexb::is_first_slice(nal);
            }
        });
        // transport stream: whole packets with sync bytes, and torn ones
        let mut ts = stream.clone();
        if i % 2 == 0 {
            for k in (0..ts.len()).step_by(188) {
                ts[k] = 0x47;
            }
        }
        check("mpegts::demux::parse", &ts, || {
            demux::parse(&ts).map(|r| r.packets)
        });
    }
}
