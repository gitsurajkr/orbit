//! Once-a-second counters for the stats lines both binaries print.

use std::time::{Duration, Instant};

/// Values (e.g. milliseconds) collected over one window; reports avg / p95 / max.
#[derive(Default)]
pub struct Dist {
    values: Vec<f64>,
}

impl Dist {
    pub fn add(&mut self, v: f64) {
        self.values.push(v);
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// "avg/p95/max" with one decimal, or "-" when empty. Clears the window.
    pub fn take(&mut self) -> String {
        if self.values.is_empty() {
            return "-".into();
        }
        self.values.sort_by(f64::total_cmp);
        let n = self.values.len();
        let avg = self.values.iter().sum::<f64>() / n as f64;
        let p95 = self.values[((n as f64 * 0.95).ceil() as usize).clamp(1, n) - 1];
        let max = self.values[n - 1];
        self.values.clear();
        format!("{avg:.1}/{p95:.1}/{max:.1}")
    }
}

/// Fires once per `period`, returning the exact elapsed seconds so rates stay correct even if
/// the caller checks a little late.
pub struct Ticker {
    last: Instant,
    period: Duration,
}

impl Ticker {
    pub fn new(period: Duration) -> Self {
        Ticker { last: Instant::now(), period }
    }

    pub fn tick(&mut self, now: Instant) -> Option<f64> {
        let el = now.duration_since(self.last);
        if el < self.period {
            return None;
        }
        self.last = now;
        Some(el.as_secs_f64())
    }
}

pub fn mbit_per_s(bytes: u64, secs: f64) -> f64 {
    bytes as f64 * 8.0 / 1e6 / secs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dist_summary() {
        let mut d = Dist::default();
        assert_eq!(d.take(), "-");
        for v in 1..=100 {
            d.add(v as f64);
        }
        assert_eq!(d.take(), "50.5/95.0/100.0");
        assert!(d.is_empty());
    }

    #[test]
    fn mbit() {
        assert_eq!(mbit_per_s(1_250_000, 1.0), 10.0);
    }
}
