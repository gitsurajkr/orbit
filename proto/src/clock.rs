//! Each side keeps its own monotonic clock ([`now_us`]: microseconds since the process started).
//! The client estimates `offset = server_clock - client_clock` with ping/pong, NTP-style:
//!
//! ```text
//! client sends Ping at c0 ──► server stamps s ──► client gets Pong at c1
//! offset ≈ s - (c0 + c1) / 2        (exact if both directions take equally long)
//! ```
//!
//! Only the sample with the smallest round trip among the recent ones is trusted: a small
//! round trip leaves little room for the two directions to differ. With the offset, the client
//! turns a frame's `capture_us` into its own clock and measures true capture-to-screen latency.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

/// Microseconds since this process first called `now_us`.
pub fn now_us() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    rtt_us: u64,
    offset_us: i64,
}

pub struct ClockSync {
    samples: VecDeque<Sample>,
}

const KEEP: usize = 16;

impl Default for ClockSync {
    fn default() -> Self {
        ClockSync { samples: VecDeque::with_capacity(KEEP) }
    }
}

impl ClockSync {
    /// `c0`, `c1`: client clock at send and receive; `s`: server clock in the Pong.
    pub fn add(&mut self, c0: u64, s: u64, c1: u64) {
        if c1 < c0 {
            return;
        }
        let mid = (c0 + c1) / 2;
        if self.samples.len() == KEEP {
            self.samples.pop_front();
        }
        self.samples.push_back(Sample { rtt_us: c1 - c0, offset_us: s as i64 - mid as i64 });
    }

    fn best(&self) -> Option<Sample> {
        self.samples.iter().copied().min_by_key(|s| s.rtt_us)
    }

    pub fn offset_us(&self) -> Option<i64> {
        self.best().map(|s| s.offset_us)
    }

    /// Round trip of the latest ping (what the network is doing now).
    pub fn last_rtt_us(&self) -> Option<u64> {
        self.samples.back().map(|s| s.rtt_us)
    }

    /// Converts a server timestamp to the client's clock.
    pub fn to_client(&self, server_us: u64) -> Option<i64> {
        self.offset_us().map(|o| server_us as i64 - o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symmetric_path_gives_exact_offset() {
        let mut c = ClockSync::default();
        // server clock is 1_000_000 ahead; 10 ms each way
        c.add(5_000, 1_000_000 + 15_000, 25_000);
        assert_eq!(c.offset_us(), Some(1_000_000));
        assert_eq!(c.to_client(1_000_000 + 40_000), Some(40_000));
    }

    #[test]
    fn smallest_round_trip_wins() {
        let mut c = ClockSync::default();
        c.add(0, 500 + 90_000, 100_000); // slow, skewed sample
        c.add(200_000, 500 + 202_000, 204_000); // fast sample: offset 500
        c.add(300_000, 500 + 310_000, 380_000);
        assert_eq!(c.offset_us(), Some(500));
        assert_eq!(c.last_rtt_us(), Some(80_000));
    }

    #[test]
    fn negative_offset_and_bad_sample() {
        let mut c = ClockSync::default();
        c.add(10, 0, 5); // receive before send: ignored
        assert_eq!(c.offset_us(), None);
        c.add(1_000_000, 100, 1_000_200);
        assert_eq!(c.offset_us(), Some(100 - 1_000_100));
    }
}
