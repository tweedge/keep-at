//! Rolling bandwidth history for the live query socket: bytes sent/received
//! per second over the past hour and past day.
//!
//! Design (option B): a span-carrying event log. Each tick appends ONE
//! entry covering exactly the elapsed span with the byte delta between the
//! two cumulative session counters — so the log is exact under any tick
//! cadence (sparse 30-min stats passes, bursty queries) with no background
//! timer. Rates sum window overlaps per query: microseconds over the entry
//! cap (1500), memory bounded at ~48 KB worst case, ~2 KB per day at the
//! default 30-min cadence. Live-only by design: the log lives in-process
//! and resets on restart, like the since-boot totals.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Entries fully older than this are pruned (day window + slack).
const DAY: Duration = Duration::from_secs(86_400);
/// Hard cap: query storms can't grow the log past this (~48 KB).
const MAX_ENTRIES: usize = 1500;

#[derive(Debug, Clone, Copy)]
struct Entry {
    /// When this entry's span began.
    start: Instant,
    /// How long the span lasted (time between the two ticks).
    span: Duration,
    up: u64,
    down: u64,
}

#[derive(Debug)]
pub struct RateTracker {
    entries: VecDeque<Entry>,
    last_up: u64,
    last_down: u64,
    last_tick: Instant,
    /// Session start: caps the effective window (no pretending to know
    /// rates from before this process existed).
    started: Instant,
}

impl RateTracker {
    pub fn new(started: Instant, up_total: u64, down_total: u64) -> RateTracker {
        RateTracker {
            entries: VecDeque::new(),
            last_up: up_total,
            last_down: down_total,
            last_tick: started,
            started,
        }
    }

    /// Record the transfer delta since the previous tick. Cheap (amortized
    /// O(1)); safe to call from any cadence. Counters are cumulative
    /// session totals (rqbit session counters).
    pub fn tick(&mut self, now: Instant, up_total: u64, down_total: u64) {
        let span = now.saturating_duration_since(self.last_tick);
        let up = up_total.saturating_sub(self.last_up);
        let down = down_total.saturating_sub(self.last_down);
        let start = self.last_tick;
        self.last_up = up_total;
        self.last_down = down_total;
        self.last_tick = now;
        if span.is_zero() {
            return;
        }
        if up > 0 || down > 0 || self.entries.is_empty() {
            // Zero-delta spans are only recorded while the log is empty (so
            // an idle boot still anchors the day window); afterwards they'd
            // just pad the deque.
            self.entries.push_back(Entry {
                start,
                span,
                up,
                down,
            });
        }
        // Prune entries fully outside the day window.
        while let Some(e) = self.entries.front() {
            if now.saturating_duration_since(e.start + e.span) > DAY {
                self.entries.pop_front();
            } else {
                break;
            }
        }
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }
    }

    /// Bytes per second sent within `window` ending at `now`. The
    /// denominator is the effective span: the window, capped at elapsed
    /// session time (a 5-minute-old session's hour rate is over 5 minutes,
    /// not extrapolated from nothing).
    pub fn rate_up(&self, now: Instant, window: Duration) -> f64 {
        self.rate(now, window).0
    }

    pub fn rate_down(&self, now: Instant, window: Duration) -> f64 {
        self.rate(now, window).1
    }

    fn rate(&self, now: Instant, window: Duration) -> (f64, f64) {
        let effective = window.min(now.saturating_duration_since(self.started));
        if effective.is_zero() {
            return (0.0, 0.0);
        }
        let win_start = now - effective;
        let mut up = 0u64;
        let mut down = 0u64;
        for e in &self.entries {
            let e_end = e.start + e.span;
            // Overlap of [e.start, e_end) with [win_start, now).
            let lo = if e.start > win_start {
                e.start
            } else {
                win_start
            };
            let hi = if e_end < now { e_end } else { now };
            if hi > lo {
                // Attribute the entry's bytes proportionally to the overlap
                // fraction of its span (exact when the entry sits fully
                // inside the window; interpolated when a long sparse entry
                // straddles the window edge).
                let frac =
                    hi.saturating_duration_since(lo).as_secs_f64() / e.span.as_secs_f64().max(1e-9);
                up = up.saturating_add((e.up as f64 * frac.min(1.0)) as u64);
                down = down.saturating_add((e.down as f64 * frac.min(1.0)) as u64);
            }
        }
        let secs = effective.as_secs_f64();
        (up as f64 / secs, down as f64 / secs)
    }

    /// Entry count (tests only; diagnostics read the log via rates).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> Instant {
        // Instant::now() is opaque; build a fake timeline by taking one
        // anchor and adding offsets.
        static ANCHOR: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let anchor = ANCHOR.get_or_init(Instant::now);
        anchor.checked_add(Duration::from_secs(secs)).unwrap()
    }

    #[test]
    fn steady_rate_over_hour() {
        let start = t(0);
        let mut tr = RateTracker::new(start, 0, 0);
        // 60 one-second ticks, 60 bytes up + 120 bytes down each.
        for i in 1..=60u64 {
            tr.tick(t(i), i * 60, i * 120);
        }
        let now = t(60);
        assert_eq!(tr.rate_up(now, Duration::from_secs(3600)), 60.0);
        assert_eq!(tr.rate_down(now, Duration::from_secs(3600)), 120.0);
        // Day window capped at elapsed: same numbers.
        assert_eq!(tr.rate_up(now, DAY), 60.0);
    }

    #[test]
    fn sparse_single_tick_exact() {
        let start = t(0);
        let mut tr = RateTracker::new(start, 0, 0);
        // One tick two hours in: 7200 up bytes over 7200s.
        tr.tick(t(7200), 7200, 0);
        let now = t(7200);
        assert_eq!(tr.rate_up(now, DAY), 1.0);
        assert_eq!(tr.rate_up(now, Duration::from_secs(3600)), 1.0);
        assert_eq!(tr.rate_down(now, DAY), 0.0);
    }

    #[test]
    fn hour_window_excludes_old_traffic() {
        let start = t(0);
        let mut tr = RateTracker::new(start, 0, 0);
        // Hour 0: 3600 bytes. Idle until hour 23 (zero-delta spans are not
        // recorded), then another 3600 bytes in one second.
        tr.tick(t(3600), 3600, 0);
        tr.tick(t(23 * 3600), 3600, 0);
        tr.tick(t(23 * 3600 + 1), 7200, 0);
        let now = t(23 * 3600 + 1);
        // Hour rate: the recent 3600 bytes over a 3600s window.
        assert_eq!(tr.rate_up(now, Duration::from_secs(3600)), 1.0);
        // Day rate: all 7200 bytes over 82801s elapsed.
        let day = tr.rate_up(now, DAY);
        assert!((day - 7200.0 / 82_801.0).abs() < 1e-9, "day rate {day}");
    }

    #[test]
    fn prunes_entries_older_than_a_day() {
        let start = t(0);
        let mut tr = RateTracker::new(start, 0, 0);
        tr.tick(t(1), 10, 10);
        tr.tick(t(2 * 86_400), 20, 20);
        assert!(tr.len() <= 2, "old entries pruned, len={}", tr.len());
        let now = t(2 * 86_400);
        // Entry 1 (10 bytes, 1s span) is >1 day old: pruned. Entry 2 spans
        // [1s, 172800s] carrying the 10-byte delta; half its span falls in
        // the day window, so half its bytes count: 5 bytes / 86400s. That
        // equals the entry's true average rate (10/172800) — proportional
        // attribution is exact for sparse spans.
        let got = tr.rate_up(now, DAY);
        assert!(
            (got - 5.0 / 86_400.0).abs() < 1e-9,
            "day rate {got} vs 5/86400"
        );
    }

    #[test]
    fn caps_entries_under_query_storm() {
        let start = t(0);
        let mut tr = RateTracker::new(start, 0, 0);
        for i in 1..=3000u64 {
            tr.tick(t(i), i, i);
        }
        assert_eq!(tr.len(), MAX_ENTRIES);
        let now = t(3000);
        // Capped at 1500 entries: seconds 1500..3000 survive, 1 byte each.
        // Effective window is the full 3000s elapsed, so 1500 bytes / 3000s.
        assert_eq!(tr.rate_up(now, DAY), 0.5);
    }

    #[test]
    fn zero_delta_idle_spans_anchor_the_window() {
        let start = t(0);
        let mut tr = RateTracker::new(start, 0, 0);
        // Idle for an hour, then 1 byte.
        tr.tick(t(3600), 1, 0);
        tr.tick(t(3601), 2, 0);
        let now = t(3601);
        // Hour rate: 1 byte over the last second span (3600-byte window,
        // effective span 3601s capped... elapsed = 3601 < 3600? No: 3601 >
        // 3600, so window = 3600 and the 1-byte entry falls inside it).
        assert_eq!(tr.rate_up(now, Duration::from_secs(3600)), 1.0 / 3600.0);
    }
}
