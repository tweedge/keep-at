//! Broken-piece quarantine detection: watch per-torrent receive counters
//! for swarms that keep feeding us bytes which never pass hash validation
//! (a poisoned or mis-seeded swarm — e.g. a dataset whose seeders hold a
//! README that contradicts the registered metainfo). Such torrents loop
//! forever: request a piece, receive it, hash mismatch, disconnect, retry —
//! burning ~2-3 GiB/h of download while never completing. See
//! notes/DESIGN-broken-piece-quarantine.md for the incident and design.
//!
//! The signal needs no rqbit fork: every received chunk bumps the
//! per-torrent `fetched_bytes` wire counter *before* validation, and only
//! validated pieces bump `downloaded_and_checked_bytes`. So
//! `wasted = fetched - checked` is the discarded-bytes counter, observable
//! via `handle.stats().live.snapshot`.
//!
//! The rule is rate-independent: `wasted_since_progress` accumulates only
//! while `checked` is frozen and resets to zero the moment ANY verified
//! byte lands. A healthy slow torrent always lands verified bytes
//! eventually; in-flight bytes at a pass boundary are credited when their
//! piece completes; a broken loop accumulates forever. Combined with the
//! `min_windows` requirement (zero verified progress across consecutive
//! passes) this rides out transient stalls without false positives.

use std::collections::HashMap;

use crate::state;

/// The far-future cooldown for escalated repeat offenders (~10 years).
/// One constant, used by both the escalation branch and the
/// arithmetic-failure fallbacks.
pub fn far_future(now: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    // Checked: chrono's DateTime + TimeDelta PANICS on overflow (and this
    // is an abort build), while a checked failure here just means "use the
    // farthest representable instant" — the sentinel semantics either way.
    now.checked_add_signed(chrono::Duration::weeks(520))
        .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
}

/// The next registry entry after a trip. Extracted from the engine's trip
/// loop so the attempts/escalation arithmetic is unit-testable: the
/// off-by-one here decides whether a recoverable torrent is gated for
/// ~10 years (escalating one attempt early) or churns forever (never
/// escalating). Both calendar adds are checked — chrono's
/// `DateTime + TimeDelta` PANICS on overflow, and release is abort.
pub fn next_quarantine_entry(
    prev: Option<&state::Quarantine>,
    title: String,
    reason_builder: impl FnOnce(u32) -> String,
    trip: Trip,
    now: chrono::DateTime<chrono::Utc>,
    cooldown: chrono::Duration,
    max_retries: u32,
) -> state::Quarantine {
    let attempts = prev.map(|q| q.attempts).unwrap_or(0) + 1;
    let indefinite = far_future(now);
    let cooldown_until = if max_retries > 0 && attempts > max_retries {
        indefinite
    } else {
        now.checked_add_signed(cooldown).unwrap_or(indefinite)
    };
    state::Quarantine {
        title,
        reason: reason_builder(attempts),
        quarantined_at: now,
        cooldown_until,
        attempts,
        wasted_bytes: trip.wasted_bytes,
    }
}

/// Per-torrent watchdog state. In-memory only: restarts re-baseline from
/// the next pass (detection simply restarts its clock, which is fine at a
/// 30-min cadence).
#[derive(Debug, Clone, Copy)]
pub struct WatchState {
    last_checked: u64,
    last_fetched: u64,
    wasted_since_progress: u64,
    windows_without_progress: u32,
}

/// The detector's verdict when the quarantine rule trips.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trip {
    /// Discarded bytes accumulated since the last verified byte landed.
    pub wasted_bytes: u64,
    /// Consecutive zero-progress passes observed (>= min_windows).
    pub windows: u32,
}

#[derive(Debug, Default)]
pub struct Detector {
    watch: HashMap<String, WatchState>,
}

impl Detector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one torrent's counters for this watchdog pass. Returns
    /// `Some(Trip)` when the quarantine rule trips:
    ///
    /// - `wasted_since_progress >= discard_threshold`, AND
    /// - `windows_without_progress >= min_windows` (consecutive passes with
    ///   zero verified bytes).
    ///
    /// First observation of a hash only seeds the baseline. Any verified
    /// byte resets both accumulators. Counter regressions (torrent
    /// re-added after cooldown expiry: fresh session counters start at 0)
    /// re-baseline instead of producing nonsense deltas.
    pub fn observe(
        &mut self,
        info_hash: &str,
        checked: u64,
        fetched: u64,
        discard_threshold: u64,
        min_windows: u32,
    ) -> Option<Trip> {
        let ws = match self.watch.get_mut(info_hash) {
            None => {
                self.watch.insert(
                    info_hash.to_string(),
                    WatchState {
                        last_checked: checked,
                        last_fetched: fetched,
                        wasted_since_progress: 0,
                        windows_without_progress: 0,
                    },
                );
                return None;
            }
            Some(ws) => ws,
        };
        if checked < ws.last_checked || fetched < ws.last_fetched {
            *ws = WatchState {
                last_checked: checked,
                last_fetched: fetched,
                wasted_since_progress: 0,
                windows_without_progress: 0,
            };
            return None;
        }
        let checked_delta = checked - ws.last_checked;
        let fetched_delta = fetched - ws.last_fetched;
        ws.last_checked = checked;
        ws.last_fetched = fetched;
        if checked_delta > 0 {
            ws.wasted_since_progress = 0;
            ws.windows_without_progress = 0;
            return None;
        }
        ws.wasted_since_progress = ws.wasted_since_progress.saturating_add(fetched_delta);
        ws.windows_without_progress = ws.windows_without_progress.saturating_add(1);
        if ws.wasted_since_progress >= discard_threshold
            && ws.windows_without_progress >= min_windows.max(1)
        {
            Some(Trip {
                wasted_bytes: ws.wasted_since_progress,
                windows: ws.windows_without_progress,
            })
        } else {
            None
        }
    }

    /// Drop a torrent's watch state (removed, finished, or enforcement
    /// already handled it).
    pub fn forget(&mut self, info_hash: &str) {
        self.watch.remove(info_hash);
    }

    /// Drop every watch entry whose hash was NOT observed this pass. The
    /// pass sees the full session torrent list, so anything else (removed
    /// via swap, stall eviction, deleted-from-catalog, failed adds, census
    /// teardown) is gone from the session and its state is stale — pruning
    /// here keeps the map bounded at churn rate without every removal site
    /// having to remember a forget() call.
    pub fn retain_only(&mut self, observed: &std::collections::HashSet<String>) {
        self.watch.retain(|hash, _| observed.contains(hash));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn observe(d: &mut Detector, checked: u64, fetched: u64) -> Option<Trip> {
        d.observe("aa", checked, fetched, 64 * MIB, 2)
    }

    #[test]
    fn baseline_pass_never_trips() {
        let mut d = Detector::new();
        assert!(observe(&mut d, 0, 0).is_none());
        // A bit of waste in the second pass, below every threshold.
        assert!(observe(&mut d, 0, 10 * MIB).is_none());
    }

    #[test]
    fn trips_on_threshold_plus_windows() {
        let mut d = Detector::new();
        assert!(observe(&mut d, 0, 0).is_none());
        // 40 MiB wasted in pass 2 (below 64 MiB), 40 more in pass 3:
        // crosses the byte threshold but only with the 3rd observation,
        // which is the 2nd zero-progress window.
        assert!(observe(&mut d, 0, 40 * MIB).is_none());
        let trip = observe(&mut d, 0, 80 * MIB).expect("trips");
        assert_eq!(trip.wasted_bytes, 80 * MIB);
        assert_eq!(trip.windows, 2);
    }

    #[test]
    fn byte_threshold_without_windows_does_not_trip() {
        let mut d = Detector::new();
        assert!(observe(&mut d, 0, 0).is_none());
        // One window with massive waste: bytes crossed, window count not.
        assert!(observe(&mut d, 0, 200 * MIB).is_none());
    }

    #[test]
    fn verified_byte_resets_accumulators() {
        let mut d = Detector::new();
        assert!(observe(&mut d, 0, 0).is_none());
        assert!(observe(&mut d, 0, 50 * MIB).is_none());
        // A verified piece lands: both counters reset even though waste
        // was approaching the threshold.
        assert!(observe(&mut d, 50 * MIB, 90 * MIB).is_none());
        // Fresh accumulation starts from zero: 60 MiB of waste in the
        // first window after the reset stays under the 64 MiB threshold
        // (the pre-reset 50 MiB must not be counted toward it).
        assert!(observe(&mut d, 50 * MIB, 150 * MIB).is_none());
        // Second window past the reset: 60 + 64 = 124 MiB total waste
        // since the verified byte, 2 windows => trip. The trip's waste
        // count (124, not 174 = 50+124) proves the reset happened.
        let trip = observe(&mut d, 50 * MIB, 214 * MIB).expect("trips");
        assert_eq!(trip.wasted_bytes, 124 * MIB);
        assert_eq!(trip.windows, 2);
    }

    #[test]
    fn in_flight_bytes_are_not_waste_once_piece_completes() {
        let mut d = Detector::new();
        assert!(observe(&mut d, 0, 0).is_none());
        // A slow 32 MiB piece is mid-flight at the pass boundary.
        assert!(observe(&mut d, 0, 20 * MIB).is_none());
        // It completes next pass: checked jumps; the earlier fetched
        // bytes are credited, not re-counted as waste.
        assert!(observe(&mut d, 32 * MIB, 32 * MIB).is_none());
        assert!(observe(&mut d, 32 * MIB, 52 * MIB).is_none());
    }

    #[test]
    fn counter_regression_rebaselines() {
        let mut d = Detector::new();
        assert!(observe(&mut d, 0, 0).is_none());
        assert!(observe(&mut d, 0, 200 * MIB).is_none());
        // Torrent re-added after cooldown expiry: session counters start
        // over. Must re-baseline, not accumulate garbage.
        assert!(observe(&mut d, 0, 100 * MIB).is_none());
        assert!(observe(&mut d, 0, 60 * MIB).is_none());
    }

    #[test]
    fn forget_drops_state() {
        let mut d = Detector::new();
        assert!(observe(&mut d, 0, 0).is_none());
        assert!(observe(&mut d, 0, 200 * MIB).is_none());
        d.forget("aa");
        // Fresh baseline: no trip even with huge waste in one pass.
        assert!(observe(&mut d, 0, 500 * MIB).is_none());
    }

    #[test]
    fn retain_only_prunes_unobserved_hashes() {
        let mut d = Detector::new();
        // "aa" accumulates one window of waste.
        assert!(observe(&mut d, 0, 0).is_none());
        assert!(observe(&mut d, 0, 200 * MIB).is_none());
        // "bb" (a second, still-live torrent) also has one window.
        d.observe("bb", 0, 0, 64 * MIB, 2);
        assert!(d.observe("bb", 0, 200 * MIB, 64 * MIB, 2).is_none());
        // This pass observed only "aa" (e.g. "bb" was swapped out): its
        // watch state must go, so a future re-add re-baselines instead of
        // tripping on stale counters.
        let observed: std::collections::HashSet<String> = ["aa".to_string()].into_iter().collect();
        d.retain_only(&observed);
        // Re-added "bb" starts fresh: no trip despite huge single-pass waste.
        assert!(d.observe("bb", 0, 500 * MIB, 64 * MIB, 2).is_none());
        // "aa" kept its accumulated state: its next window pushes the
        // total waste (200 + 100 delta) past the threshold with 2 windows.
        let trip = d
            .observe("aa", 0, 300 * MIB, 64 * MIB, 2)
            .expect("aa keeps its state across prune");
        assert_eq!(trip.wasted_bytes, 300 * MIB);
    }

    #[test]
    fn single_window_mode_mins_to_one() {
        let mut d = Detector::new();
        d.observe("bb", 0, 0, 64 * MIB, 0);
        // min_windows 0 clamps to 1: a single pass past the byte threshold
        // trips.
        let trip = d
            .observe("bb", 0, 70 * MIB, 64 * MIB, 0)
            .expect("trips with clamped window count");
        assert_eq!(trip.windows, 1);
    }

    // ---- escalation math (next_quarantine_entry) ----

    fn fake_trip(wasted: u64) -> Trip {
        Trip {
            wasted_bytes: wasted,
            windows: 2,
        }
    }

    #[test]
    fn fresh_trip_starts_attempts_at_one_with_finite_cooldown() {
        let now = chrono::Utc::now();
        let cooldown = chrono::Duration::days(3);
        let q = next_quarantine_entry(
            None,
            "t".to_string(),
            |_| "r".to_string(),
            fake_trip(1),
            now,
            cooldown,
            0,
        );
        assert_eq!(q.attempts, 1);
        assert_eq!(q.cooldown_until, now + cooldown);
        assert_eq!(q.wasted_bytes, 1);
    }

    #[test]
    fn escalation_pasts_max_retries_becomes_indefinite() {
        let now = chrono::Utc::now();
        let prev = crate::state::Quarantine {
            title: "t".to_string(),
            reason: "r".to_string(),
            quarantined_at: now,
            cooldown_until: now,
            attempts: 1,
            wasted_bytes: 1,
        };
        let q = next_quarantine_entry(
            Some(&prev),
            "t".to_string(),
            |_| "r".to_string(),
            fake_trip(1),
            now,
            chrono::Duration::days(3),
            1,
        );
        assert_eq!(q.attempts, 2, "attempts accumulate across cycles");
        assert!(
            q.cooldown_until - now > chrono::Duration::weeks(500),
            "escalated cooldown ≈ 10y, got {:?}",
            q.cooldown_until - now
        );
    }

    #[test]
    fn escalation_boundary_is_strictly_greater() {
        // attempts == max_retries must STILL be finite: only a trip BEYOND
        // the configured retries locks the hash out. An off-by-one here
        // permanently gates a recoverable torrent one cycle early.
        let now = chrono::Utc::now();
        let cooldown = chrono::Duration::days(3);
        for prev_attempts in 0..=2u32 {
            let prev = crate::state::Quarantine {
                attempts: prev_attempts,
                ..same_quarantine(now)
            };
            let q = next_quarantine_entry(
                Some(&prev),
                "t".to_string(),
                |_| "r".to_string(),
                fake_trip(1),
                now,
                cooldown,
                2,
            );
            let expected_attempts = prev_attempts + 1;
            assert_eq!(q.attempts, expected_attempts);
            if expected_attempts > 2 {
                assert!(q.cooldown_until - now > chrono::Duration::weeks(500));
            } else {
                assert_eq!(q.cooldown_until, now + cooldown);
            }
        }
    }

    fn same_quarantine(now: chrono::DateTime<chrono::Utc>) -> crate::state::Quarantine {
        crate::state::Quarantine {
            title: "t".to_string(),
            reason: "r".to_string(),
            quarantined_at: now,
            cooldown_until: now,
            attempts: 0,
            wasted_bytes: 0,
        }
    }

    #[test]
    fn huge_cooldown_clamps_to_sentinel_not_zero() {
        // from_std failure must NOT degrade the cooldown to zero (that
        // silently inverts the operator's near-permanent quarantine into
        // immediate re-probe churn); the engine clamps to the sentinel
        // before calling here, but the checked add inside this fn must
        // also not panic on a cooldown that overflows the calendar.
        let now = chrono::Utc::now();
        let huge = chrono::Duration::weeks(520) + chrono::Duration::weeks(520);
        let q = next_quarantine_entry(
            None,
            "t".to_string(),
            |_| "r".to_string(),
            fake_trip(1),
            now,
            huge,
            0,
        );
        assert!(
            q.cooldown_until > now,
            "overflowed add must fall back to a future date, got {:?}",
            q.cooldown_until
        );
    }
}
