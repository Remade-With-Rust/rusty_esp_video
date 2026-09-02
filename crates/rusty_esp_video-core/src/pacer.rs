//! A frame-rate cap and a byte budget, with honest counters.
//!
//! Wi-Fi on an ESP32 cannot carry every frame a sensor produces at every
//! size; a stream needs a policy for which frames to drop, and a counter that
//! says how many it dropped. [`Pacer`] admits a frame when at least one
//! interval has elapsed since the last admitted one; [`Budget`] admits a
//! frame when its bytes fit the bit-rate cap. Both drop a frame whole, never
//! blocking and never sending part of one: a receiver sees complete frames
//! at a lower rate, not corrupt frames at the sensor's rate.

use rusty_esp_core::time::Micros;

/// Admits at most `fps` frames per second.
#[derive(Debug, Clone)]
pub struct Pacer {
    interval_micros: u64,
    last: Option<Micros>,
    /// Frames admitted.
    pub admitted: u64,
    /// Frames dropped.
    pub dropped: u64,
}

impl Pacer {
    /// A pacer capping at `fps`; 0 admits everything.
    #[must_use]
    pub fn new(fps: u32) -> Self {
        Pacer {
            interval_micros: if fps == 0 {
                0
            } else {
                1_000_000 / u64::from(fps)
            },
            last: None,
            admitted: 0,
            dropped: 0,
        }
    }

    /// Should a frame captured at `now` be sent?
    pub fn admit(&mut self, now: Micros) -> bool {
        let ok = match self.last {
            None => true,
            Some(last) => now.since(last) >= self.interval_micros,
        };
        if ok {
            self.last = Some(now);
            self.admitted += 1;
        } else {
            self.dropped += 1;
        }
        ok
    }

    /// Forget the last admitted frame (after a stream restart).
    pub fn reset(&mut self) {
        self.last = None;
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
}
