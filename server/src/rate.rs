//! Adaptive bitrate: once a second, pick the encoder bitrate from what the connection shows.
//!
//! Signals (all measured on the server, no client report needed):
//! - RTT against the best RTT of the last 10 s: a growing queue somewhere on the path shows up
//!   as extra delay *before* packets start dropping, so this backs off early.
//! - packet loss (quinn's lost / sent packets in that second).
//! - "net wait": the fraction of the second the video loop spent waiting for quinn's send
//!   queue to drain (the encoder makes more than the path carries).
//!
//! Rules (first match wins), adapted from Annex's controller:
//!
//! | condition                                              | action                  |
//! |--------------------------------------------------------|-------------------------|
//! | RTT > 3×best + 100 ms, or net wait > 50 %              | ×0.5                    |
//! | loss > 10 % (an overflowing link: get under it now)    | ×0.5                    |
//! | loss > 5 %, loss two seconds running, RTT > 2×best +   | ×0.8                    |
//! | 15 ms, or net wait > 10 %                              |                         |
//! | one stray lost packet                                  | hold (burst, not a cap) |
//! | RTT > 1.5×best + 8 ms                                  | ×0.9                    |
//! | clean, but less than half the rate was used (idle)     | recover up to "proven"  |
//! | clean                                                  | climb (see below)       |
//!
//! RTT rules only count in busy seconds (an idle sender can't be the cause of a slower round
//! trip), and a loss percentage needs [`MIN_PACKETS_FOR_RATIO`] packets.
//!
//! "Proven" is the highest rate a busy second carried without trouble (the start rate counts),
//! lowered by trouble under load. Idle seconds climb back up to it (+10 % a second), so a cut
//! caused by noise on a calm desktop comes back, but never beyond what the path has carried.
//!
//! A session starts at [`START_BPS`] (or the configured maximum, if lower) and only climbs past
//! the proven rate on seconds that actually carried traffic. Idle seconds used to count as clean, so after a still
//! screen the rate sat at the maximum without ever having carried it; the next burst of screen
//! changes then overflowed a slow path (a Tailscale relay: 30-70 % loss, 4 s freezes).
//!
//! After a cut: wait [`HOLD`] clean seconds, then climb +10 % per second, but only up to 90 %
//! of the rate where trouble happened; above that, probe slowly (+1 % of max per second). The
//! trouble rate is forgotten after [`FORGET`] clean seconds, in case the path got better.
//!
//! FEC (Reed-Solomon parity, see proto/video.rs) follows loss: +5 points for each second with
//! any lost packet, -1 per clean second, between 10 and 40 %. The rate above is the total on
//! the wire; the encoder gets [`RateControl::video_bps`] = rate x 100 / (100 + FEC %).

use std::collections::VecDeque;
use std::time::Duration;

/// Never go below this (a 1080p desktop is still readable at 1 Mbit/s).
pub const MIN_BPS: u32 = 1_000_000;
/// Where a session starts: good quality on any path, and a few seconds of real traffic climb
/// from here to the configured maximum where the path allows it.
pub const START_BPS: u32 = 10_000_000;
/// A second that sent less than this fraction of the video rate is idle: it doesn't prove the rate.
const BUSY: f64 = 0.5;
/// Loss above this halves the rate at once.
const HEAVY_LOSS: f64 = 0.10;
/// Seconds after the last loss during which the path counts as lossy (see `recently_lossy`).
const LOSSY_FOR: u32 = 10;
/// Fewer packets than this in a second: no loss percentage (only "lost some, twice in a row").
const MIN_PACKETS_FOR_RATIO: u64 = 50;
/// Seconds of RTT history for the "best RTT" baseline.
const RTT_WINDOW: usize = 10;
/// Clean seconds to wait after a cut before climbing.
const HOLD: u32 = 3;
/// Clean seconds after which the rate where trouble happened stops capping the climb.
const FORGET: u32 = 30;
/// FEC parity as a percent of data shards: start, floor, ceiling, steps.
const FEC_START: u32 = 15;
const FEC_MIN: u32 = 10;
pub(crate) const FEC_MAX: u32 = 40;
const FEC_UP: u32 = 5;
const FEC_DOWN: u32 = 1;

/// One second of measurements.
#[derive(Clone, Copy, Debug)]
pub struct Report {
    pub rtt: Duration,
    pub sent_packets: u64,
    pub lost_packets: u64,
    /// Fraction (0..=1) of the second spent waiting for the send queue.
    pub net_wait: f64,
    /// Video bits actually sent in that second (before FEC).
    pub sent_bps: u64,
}

pub struct RateControl {
    bps: u32,
    max: u32,
    rtts: VecDeque<Duration>,
    lossy_prev: bool,
    hold: u32,
    trouble_at: Option<u32>,
    clean: u32,
    reason: &'static str,
    fec: u32,
    /// Seconds since the last second with a lost packet.
    since_loss: u32,
    /// Highest rate a busy second carried without trouble (the start rate counts), lowered by
    /// trouble under load. Idle seconds recover up to it, never beyond.
    proven: u32,
}

impl RateControl {
    /// Starts at [`START_BPS`] or `max_bps`, whichever is lower (the configured bitrate is the ceiling).
    pub fn new(max_bps: u32) -> Self {
        let max = max_bps.max(MIN_BPS);
        RateControl {
            bps: max.min(START_BPS),
            max,
            rtts: VecDeque::new(),
            lossy_prev: false,
            hold: 0,
            trouble_at: None,
            clean: 0,
            reason: "start",
            fec: FEC_START,
            since_loss: LOSSY_FOR,
            proven: max.min(START_BPS),
        }
    }

    /// A packet was lost in the last [`LOSSY_FOR`] seconds: keep keyframes small (a big one is
    /// likely to be lost again on such a path).
    pub fn recently_lossy(&self) -> bool {
        self.since_loss < LOSSY_FOR
    }

    /// Total bitrate on the wire (video + FEC parity).
    pub fn bps(&self) -> u32 {
        self.bps
    }

    /// FEC parity percent for [`proto::video::fragment`].
    pub fn fec_percent(&self) -> u32 {
        self.fec
    }

    /// Bitrate for the encoder: the wire rate minus room for FEC parity.
    pub fn video_bps(&self) -> u32 {
        (self.bps as u64 * 100 / (100 + self.fec) as u64) as u32
    }

    /// Why the rate is what it is, for stats: start, queue, heavy loss, loss, net wait, rtt,
    /// burst, idle, hold, climb, probe, max.
    pub fn reason(&self) -> &'static str {
        self.reason
    }

    /// Feed one second of measurements; returns the new bitrate.
    pub fn update(&mut self, r: &Report) -> u32 {
        self.rtts.push_back(r.rtt);
        if self.rtts.len() > RTT_WINDOW {
            self.rtts.pop_front();
        }
        let best = self.rtts.iter().min().copied().unwrap_or(r.rtt);
        let ms = Duration::from_millis;
        let loss = if r.sent_packets > 0 { r.lost_packets as f64 / r.sent_packets as f64 } else { 0.0 };
        let lossy = r.lost_packets > 0;

        // A loss *percentage* from a handful of packets means little (1 of 8 is "12 %").
        let loss = if r.sent_packets >= MIN_PACKETS_FOR_RATIO { loss } else { 0.0 };
        let idle = (r.sent_bps as f64) < self.video_bps() as f64 * BUSY;
        // RTT rules only count in busy seconds: sending little, a slower round trip can't be our
        // own queue, it's the path's jitter. (Counting it ratcheted the rate down to 1 Mbit/s on
        // a calm desktop, seen on the workstation: blurry, and idle seconds never climbed back.)
        let busy_rtt = |limit: Duration| !idle && r.rtt > limit;

        if r.net_wait > 0.5 {
            self.cut(0.5, "net wait", idle);
        } else if busy_rtt(best * 3 + ms(100)) {
            self.cut(0.5, "queue", idle);
        } else if loss > HEAVY_LOSS {
            self.cut(0.5, "heavy loss", idle);
        } else if loss > 0.05 || (lossy && self.lossy_prev) {
            self.cut(0.8, "loss", idle);
        } else if r.net_wait > 0.1 {
            self.cut(0.8, "net wait", idle);
        } else if busy_rtt(best * 2 + ms(15)) {
            self.cut(0.8, "rtt", idle);
        } else if lossy {
            self.reason = "burst"; // one stray loss: keep the rate, don't count as clean
        } else if busy_rtt(Duration::from_secs_f64(best.as_secs_f64() * 1.5 + 0.008)) {
            self.cut(0.9, "rtt", idle);
        } else if idle {
            self.recover();
        } else {
            // A busy second without trouble: this rate is proven to work.
            self.proven = self.proven.max(self.bps);
            self.climb();
        }
        self.lossy_prev = lossy;
        self.since_loss = if lossy { 0 } else { (self.since_loss + 1).min(LOSSY_FOR) };
        self.fec = if lossy { (self.fec + FEC_UP).min(FEC_MAX) } else { self.fec.saturating_sub(FEC_DOWN).max(FEC_MIN) };
        self.bps
    }

    /// `idle`: the trouble came in a second that sent little, so it says nothing about what the
    /// path carries under load: the proven rate stays (idle seconds may recover to it).
    fn cut(&mut self, factor: f64, reason: &'static str, idle: bool) {
        self.trouble_at = Some(self.bps);
        self.bps = ((self.bps as f64 * factor) as u32).max(MIN_BPS);
        if !idle {
            self.proven = self.proven.min(self.bps);
        }
        self.hold = HOLD;
        self.clean = 0;
        self.reason = reason;
    }

    /// An idle second: it proves nothing, so never above the proven rate; but up to it (+10 % a
    /// second), so a rate cut by noise comes back while the desktop is calm.
    fn recover(&mut self) {
        if self.hold > 0 {
            self.hold -= 1;
            self.reason = "hold";
        } else if self.bps < self.proven {
            self.bps = ((self.bps as f64 * 1.1) as u32).min(self.proven);
            self.reason = "recover";
        } else {
            self.reason = "idle";
        }
    }

    fn climb(&mut self) {
        self.clean += 1;
        if self.clean >= FORGET {
            self.trouble_at = None;
        }
        if self.hold > 0 {
            self.hold -= 1;
            self.reason = "hold";
            return;
        }
        let ceiling = self.trouble_at.map_or(self.max, |t| (t as f64 * 0.9) as u32).min(self.max);
        if self.bps < ceiling {
            self.bps = ((self.bps as f64 * 1.1) as u32).min(ceiling);
            self.reason = "climb";
        } else if self.bps < self.max {
            self.bps = (self.bps + self.max / 100).min(self.max);
            self.reason = "probe";
        } else {
            self.reason = "max";
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: u32 = 1_000_000;

    /// A second that used the whole rate (busy): only such seconds can prove a rate.
    fn rep(rtt_ms: u64, sent: u64, lost: u64, wait: f64) -> Report {
        Report { rtt: Duration::from_millis(rtt_ms), sent_packets: sent, lost_packets: lost, net_wait: wait, sent_bps: u64::MAX / 2 }
    }

    fn idle(rtt_ms: u64) -> Report {
        Report { sent_bps: 200_000, ..rep(rtt_ms, 50, 0, 0.0) }
    }

    /// Already at `max` (as if it had climbed there under load).
    fn warm(max: u32) -> RateControl {
        let mut r = RateControl::new(max);
        r.bps = max;
        r
    }

    fn clean(rtt_ms: u64) -> Report {
        rep(rtt_ms, 1000, 0, 0.0)
    }

    #[test]
    fn stays_at_max_on_a_clean_path() {
        let mut r = warm(20 * M);
        for _ in 0..20 {
            assert_eq!(r.update(&clean(90)), 20 * M);
        }
        assert_eq!(r.reason(), "max");
    }

    #[test]
    fn halves_when_a_queue_builds() {
        let mut r = warm(20 * M);
        r.update(&clean(90));
        assert_eq!(r.update(&clean(400)), 10 * M); // 400 > 3*90 + 100
        assert_eq!(r.reason(), "queue");
        assert_eq!(r.update(&rep(90, 1000, 0, 0.7)), 5 * M);
        assert_eq!(r.reason(), "net wait");
    }

    #[test]
    fn one_stray_loss_holds_two_in_a_row_cut() {
        let mut r = warm(20 * M);
        r.update(&clean(90));
        assert_eq!(r.update(&rep(90, 1000, 1, 0.0)), 20 * M);
        assert_eq!(r.reason(), "burst");
        assert_eq!(r.update(&rep(90, 1000, 1, 0.0)), 16 * M);
        assert_eq!(r.reason(), "loss");
    }

    #[test]
    fn heavy_loss_cuts_at_once() {
        let mut r = warm(20 * M);
        assert_eq!(r.update(&rep(90, 1000, 80, 0.0)), 16 * M);
    }

    #[test]
    fn creeping_rtt_backs_off_gently() {
        let mut r = warm(20 * M);
        r.update(&clean(90));
        assert_eq!(r.update(&clean(150)), 18 * M); // > 1.5*90+8, < 2*90+15
        assert_eq!(r.reason(), "rtt");
    }

    #[test]
    fn after_a_cut_holds_then_climbs_to_90_percent_of_trouble() {
        let mut r = warm(20 * M);
        r.update(&clean(90));
        r.update(&rep(90, 1000, 100, 0.0)); // 20 -> 16, trouble at 20
        for _ in 0..HOLD {
            assert_eq!(r.update(&clean(90)), 16 * M);
            assert_eq!(r.reason(), "hold");
        }
        assert_eq!(r.update(&clean(90)), 17_600_000);
        assert_eq!(r.update(&clean(90)), 18 * M); // capped at 0.9 * 20
        assert_eq!(r.update(&clean(90)), 18_200_000); // slow probe above it
        assert_eq!(r.reason(), "probe");
    }

    #[test]
    fn trouble_rate_is_forgotten_after_clean_period() {
        let mut r = warm(20 * M);
        r.update(&clean(90));
        r.update(&rep(90, 1000, 100, 0.0));
        for _ in 0..FORGET {
            r.update(&clean(90));
        }
        assert!(r.trouble_at.is_none());
    }

    #[test]
    fn never_below_minimum() {
        let mut r = warm(20 * M);
        for _ in 0..30 {
            r.update(&rep(90, 1000, 0, 0.9));
        }
        assert_eq!(r.bps(), MIN_BPS);
    }

    #[test]
    fn fec_follows_loss_within_bounds() {
        let mut r = warm(20 * M);
        assert_eq!(r.fec_percent(), 15);
        assert_eq!(r.video_bps(), 20 * M * 100 / 115);
        r.update(&rep(90, 1000, 1, 0.0));
        assert_eq!(r.fec_percent(), 20);
        for _ in 0..10 {
            r.update(&rep(90, 1000, 1, 0.0));
        }
        assert_eq!(r.fec_percent(), 40);
        for _ in 0..100 {
            r.update(&clean(90));
        }
        assert_eq!(r.fec_percent(), 10);
    }

    #[test]
    fn lan_jitter_is_not_congestion() {
        let mut r = warm(20 * M);
        for rtt in [1, 2, 1, 3, 2, 1, 4, 2] {
            assert_eq!(r.update(&clean(rtt)), 20 * M); // 4 ms < 1.5*1 + 8
        }
    }

    #[test]
    fn starts_at_start_rate_and_climbs_only_under_load() {
        let mut r = RateControl::new(20 * M);
        assert_eq!(r.bps(), START_BPS);
        assert_eq!(RateControl::new(4 * M).bps(), 4 * M, "a lower maximum is the start");
        for _ in 0..20 {
            r.update(&idle(90));
        }
        assert_eq!(r.bps(), START_BPS, "idle seconds prove nothing");
        assert_eq!(r.reason(), "idle");
        for _ in 0..20 {
            r.update(&clean(90));
        }
        assert_eq!(r.bps(), 20 * M, "busy clean seconds climb to the maximum");
    }

    #[test]
    fn a_still_screen_does_not_climb_back_after_trouble() {
        // The relay freeze: cut to ~5 Mbit/s by loss, then a still screen for a minute.
        let mut r = warm(20 * M);
        r.update(&rep(90, 1000, 300, 0.0));
        let after_cut = r.bps();
        for _ in 0..60 {
            r.update(&idle(90));
        }
        assert_eq!(r.bps(), after_cut, "must not sit at the maximum it never carried");
    }

    #[test]
    fn heavy_loss_halves_at_once() {
        let mut r = warm(20 * M);
        assert_eq!(r.update(&rep(90, 1000, 333, 0.0)), 10 * M);
        assert_eq!(r.reason(), "heavy loss");
        assert_eq!(r.update(&rep(90, 1000, 80, 0.0)), 8 * M, "5-10 % loss: the gentler cut");
    }

    #[test]
    fn lossy_for_a_while_after_a_loss() {
        let mut r = warm(20 * M);
        assert!(!r.recently_lossy());
        r.update(&rep(90, 1000, 1, 0.0));
        assert!(r.recently_lossy());
        for _ in 0..LOSSY_FOR - 1 {
            r.update(&clean(90));
            assert!(r.recently_lossy());
        }
        r.update(&clean(90));
        assert!(!r.recently_lossy());
    }

    #[test]
    fn rtt_jitter_on_a_calm_desktop_keeps_the_rate() {
        // The workstation log: idle desktop, round trip 54 -> 74 -> 137 ms, no loss. It used to
        // cut 9 -> 3 -> 1 Mbit/s and stay there.
        let mut r = RateControl::new(20 * M);
        for rtt in [55, 54, 74, 60, 137, 58, 72, 140, 56] {
            r.update(&idle(rtt));
        }
        assert_eq!(r.bps(), START_BPS);
        assert_eq!(r.reason(), "idle");
    }

    #[test]
    fn a_cut_from_noise_while_idle_recovers_to_the_proven_rate() {
        let mut r = RateControl::new(20 * M);
        let lossy_idle = Report { lost_packets: 1, ..idle(60) };
        r.update(&lossy_idle);
        r.update(&lossy_idle); // two lossy seconds in a row: cut
        assert!(r.bps() < START_BPS);
        for _ in 0..HOLD {
            r.update(&idle(60));
            assert_eq!(r.reason(), "hold");
        }
        for _ in 0..5 {
            r.update(&idle(60));
        }
        assert_eq!(r.bps(), START_BPS, "back to the proven start rate");
        for _ in 0..20 {
            r.update(&idle(60));
        }
        assert_eq!(r.bps(), START_BPS, "never beyond it while idle");
    }

    #[test]
    fn trouble_under_load_lowers_what_idle_may_recover_to() {
        // The relay freeze: loss while busy at 10 -> 5 Mbit/s; a still screen must not go back up.
        let mut r = RateControl::new(20 * M);
        r.update(&rep(60, 1000, 300, 0.0));
        let after = r.bps();
        for _ in 0..30 {
            r.update(&idle(60));
        }
        assert_eq!(r.bps(), after);
    }

    #[test]
    fn a_few_packets_make_no_loss_percentage() {
        let mut r = RateControl::new(20 * M);
        // 1 of 8 packets lost on an idle second: no "12 % loss" halving.
        r.update(&Report { sent_packets: 8, lost_packets: 1, ..idle(60) });
        assert_eq!(r.bps(), START_BPS);
    }
}
