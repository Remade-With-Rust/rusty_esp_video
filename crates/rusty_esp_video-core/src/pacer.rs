//! A frame-rate cap with honest counters.
//!
//! Wi-Fi on an ESP32 cannot carry every frame a sensor produces at every
//! size; a stream needs a policy for which frames to drop, and a counter that
//! says how many it dropped. The pacer admits a frame when at least one
//! interval has elapsed since the last admitted one, and never blocks.

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

#[cfg(test)]
mod tests {
    use super::*;

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
