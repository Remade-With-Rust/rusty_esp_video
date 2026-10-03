//! A frame-rate cap and a byte budget, with honest counters.
//!
//! Wi-Fi on an ESP32 cannot carry every frame a sensor produces at every
//! size; a stream needs a policy for which frames to drop, and a counter that
//! says how many it dropped. [`Pacer`] holds the frame rate to a cap,
//! optionally letting a burst through early (a sensor that sends its
//! pictures in bursts, as the OV3660 does, loses half of them to a pacer
//! that only spaces frames); [`Budget`] admits a
//! frame when its bytes fit the bit-rate cap. Both drop a frame whole, never
//! blocking and never sending part of one: a receiver sees complete frames
//! at a lower rate, not corrupt frames at the sensor's rate.

use rusty_esp_core::time::Micros;

/// Admits at most `fps` frames per second.
///
/// The rule is the generic cell rate algorithm: each admitted frame moves a
/// due time one interval on, and a frame is admitted when it arrives no
/// earlier than `burst` intervals before its due time. With `burst` 0
/// ([`Pacer::new`]) that is one interval between admitted frames, exactly;
/// with `burst` k, up to k + 1 frames may pass back to back, and over any
/// stretch at most `fps` per second plus those k are admitted.
#[derive(Debug, Clone)]
pub struct Pacer {
    interval_micros: u64,
    tolerance_micros: u64,
    /// When the next frame is due at the capped rate.
    due: Option<u64>,
    /// Frames admitted.
    pub admitted: u64,
    /// Frames dropped.
    pub dropped: u64,
}

impl Pacer {
    /// A pacer capping at `fps`, one interval between admitted frames; 0
    /// admits everything.
    #[must_use]
    pub fn new(fps: u32) -> Self {
        Self::with_burst(fps, 0)
    }

    /// A pacer capping the rate at `fps` that lets up to `burst` frames
    /// through ahead of their due time: a burst from the sensor is kept, and
    /// the long-run rate is still the cap. 0 `fps` admits everything.
    #[must_use]
    pub fn with_burst(fps: u32, burst: u32) -> Self {
        let interval_micros = if fps == 0 {
            0
        } else {
            1_000_000 / u64::from(fps)
        };
        Pacer {
            interval_micros,
            tolerance_micros: interval_micros * u64::from(burst),
            due: None,
            admitted: 0,
            dropped: 0,
        }
    }

    /// Should a frame captured at `now` be sent?
    pub fn admit(&mut self, now: Micros) -> bool {
        let t = now.0;
        let ok = self.interval_micros == 0
            || self
                .due
                .is_none_or(|due| t.saturating_add(self.tolerance_micros) >= due);
        if ok {
            if self.interval_micros != 0 {
                let from = self.due.map_or(t, |due| due.max(t));
                self.due = Some(from.saturating_add(self.interval_micros));
            }
            self.admitted += 1;
        } else {
            self.dropped += 1;
        }
        ok
    }

    /// Forget the frames admitted so far (after a stream restart).
    pub fn reset(&mut self) {
        self.due = None;
    }
}

/// A bit-rate cap: a token bucket in bytes, refilled at `kbps` and holding
/// at most `burst` bytes, that admits a frame whole when its bytes fit and
/// drops it whole when they do not.
///
/// The policy is *drop the frame that does not fit*, not *send late*: a
/// late frame on a live stream is worth nothing, and a queue only turns
/// congestion into latency. Sizing: at 10 fps and 6 kB a JPEG stream is
/// 480 kbit/s; a 200 kbit/s cap admits about four frames in ten.
#[derive(Debug, Clone)]
pub struct Budget {
    bytes_per_second: u64,
    burst_bytes: u64,
    tokens: u64,
    last: Option<Micros>,
    /// Frames admitted.
    pub admitted: u64,
    /// Frames dropped for want of budget.
    pub dropped: u64,
    /// Bytes admitted.
    pub bytes_admitted: u64,
}

impl Budget {
    /// A budget of `kbps` kilobits per second with `burst_ms` of headroom
    /// (the largest frame must fit the burst or it never sends). `kbps` of
    /// 0 admits everything.
    #[must_use]
    pub fn new(kbps: u32, burst_ms: u32) -> Self {
        let bytes_per_second = u64::from(kbps) * 1000 / 8;
        let burst_bytes = bytes_per_second * u64::from(burst_ms.max(1)) / 1000;
        Budget {
            bytes_per_second,
            burst_bytes,
            tokens: burst_bytes,
            last: None,
            admitted: 0,
            dropped: 0,
            bytes_admitted: 0,
        }
    }

    /// Bytes the bucket can hold.
    #[must_use]
    pub const fn burst_bytes(&self) -> u64 {
        self.burst_bytes
    }

    /// Should a frame of `bytes` captured at `now` be sent?
    pub fn admit(&mut self, now: Micros, bytes: usize) -> bool {
        if self.bytes_per_second == 0 {
            self.admitted += 1;
            self.bytes_admitted += bytes as u64;
            return true;
        }
        if let Some(last) = self.last {
            let refill = (u128::from(now.since(last)) * u128::from(self.bytes_per_second)
                / 1_000_000) as u64;
            self.tokens = self.tokens.saturating_add(refill).min(self.burst_bytes);
        }
        self.last = Some(now);
        let need = bytes as u64;
        if need <= self.tokens {
            self.tokens -= need;
            self.admitted += 1;
            self.bytes_admitted += need;
            true
        } else {
            self.dropped += 1;
            false
        }
    }

    /// Full bucket, no history (after a stream restart).
    pub fn reset(&mut self) {
        self.tokens = self.burst_bytes;
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_budget_admits_whole_frames_under_the_cap_and_drops_the_rest() {
        // 200 kbit/s = 25 000 B/s; 6 000-byte frames at 10 fps = 60 000 B/s.
        let mut b = Budget::new(200, 500);
        assert_eq!(b.burst_bytes(), 12_500);
        let mut sent = 0u64;
        for i in 0..100u64 {
            if b.admit(Micros::from_millis(i * 100), 6_000) {
                sent += 1;
            }
        }
        assert_eq!(sent, b.admitted);
        assert_eq!(b.admitted + b.dropped, 100);
        // ten seconds of 25 000 B/s plus the burst is the ceiling
        assert!(
            b.bytes_admitted <= 25_000 * 10 + 12_500,
            "{}",
            b.bytes_admitted
        );
        // and the cap is used: at least a third of the frames went
        assert!(b.admitted >= 38 && b.admitted <= 44, "{}", b.admitted);
        // a frame larger than the burst never sends
        let mut tiny = Budget::new(8, 100); // 1 000 B/s, 100 B burst
        assert!(!tiny.admit(Micros::ZERO, 200));
        assert!(!tiny.admit(Micros::from_secs(10), 200));
        assert_eq!(tiny.dropped, 2);
        // zero admits everything
        let mut open = Budget::new(0, 0);
        assert!(open.admit(Micros::ZERO, 1 << 20));
        assert!(open.admit(Micros::ZERO, 1 << 20));
        // reset refills
        let mut r = Budget::new(80, 100); // 10 000 B/s, 1 000 B burst
        assert!(r.admit(Micros::ZERO, 1_000));
        assert!(!r.admit(Micros::ZERO, 1));
        r.reset();
        assert!(r.admit(Micros::ZERO, 1_000));
    }

    #[test]
    fn caps_and_counts() {
        let mut p = Pacer::new(10); // 100 ms
        let mut sent = 0;
        for i in 0..100u64 {
            if p.admit(Micros::from_millis(i * 25)) {
                sent += 1;
            }
        }
        assert_eq!(sent, 25);
        assert_eq!((p.admitted, p.dropped), (25, 75));
        let mut all = Pacer::new(0);
        assert!(all.admit(Micros::ZERO) && all.admit(Micros::ZERO));
    }

    /// The OV3660's shape: the pictures of one 180 ms output window arrive
    /// back to back, 36 ms of sensor time apart but a few ms apart on the
    /// wire. Spacing alone keeps one or two a window; a burst allowance
    /// keeps the cap's rate.
    #[test]
    fn a_burst_allowance_keeps_a_bursty_sensor_at_the_cap() {
        // 5 pictures every 180 ms, 4 ms apart: 27.8 fps in bursts
        let arrivals: Vec<u64> = (0..200u64).map(|i| (i / 5) * 180 + (i % 5) * 4).collect();
        let span_ms = arrivals[arrivals.len() - 1] - arrivals[0];
        let run = |mut p: Pacer| {
            let n = arrivals.iter().filter(|&&ms| p.admit(Micros::from_millis(ms))).count();
            (n, p)
        };
        let (spaced, _) = run(Pacer::new(15));
        let (bursty, p) = run(Pacer::with_burst(15, 3));
        // spacing alone: one frame per 180 ms window (the next is 4 ms on)
        // plus the odd second, far under the cap
        assert!(spaced * 1000 < 10 * span_ms as usize, "spaced {spaced} over {span_ms} ms");
        // the allowance: the cap's 15 fps over the run, never more than the
        // cap plus the burst
        let cap = (span_ms * 15).div_ceil(1000) as usize + 1;
        assert!(bursty >= cap - 2 && bursty <= cap + 3, "bursty {bursty}, cap {cap}");
        assert_eq!(p.admitted + p.dropped, arrivals.len() as u64);
    }

    #[test]
    fn a_burst_allowance_never_lets_the_long_run_rate_past_the_cap() {
        // a sensor at 60 fps, evenly spaced, for ten seconds
        for burst in [0u32, 1, 3, 8] {
            let mut p = Pacer::with_burst(15, burst);
            let n = (0..600u64).filter(|&i| p.admit(Micros(i * 16_667))).count() as u64;
            // at most the cap's 150 over the run, plus the burst at the start
            assert!(n <= 150 + u64::from(burst) + 1, "burst {burst}: {n}");
            assert!(n >= 149, "burst {burst}: {n}");
        }
    }

    #[test]
    fn no_allowance_is_the_old_spacing_exactly() {
        // the spacing rule: admitted iff an interval since the last admitted
        let times = [0u64, 50, 99, 100, 150, 201, 230, 301, 302, 400, 399, 500];
        let mut p = Pacer::new(10);
        let mut last: Option<u64> = None;
        for &ms in &times {
            let expect = last.is_none_or(|l| ms.saturating_sub(l) >= 100);
            if expect {
                last = Some(ms);
            }
            assert_eq!(p.admit(Micros::from_millis(ms)), expect, "at {ms} ms");
        }
    }
}
