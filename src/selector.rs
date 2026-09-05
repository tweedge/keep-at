//! Selection logic: which torrent most urgently needs seeding, and
//! whether swapping to it risks a cascade. Ported from internal/selector
//! (Go), preserving the exact formulas.

use rand::Rng;

/// A torrent keep-at is considering downloading.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub info_hash: [u8; 20],
    pub title: String,
    pub size_bytes: u64,
    pub seeders: u32,
    pub leechers: u32,
    /// p10 seeder floor from the last completed scan. 0 (no completed scan)
    /// falls back to the original aggressiveness^(seeders-1) behavior.
    pub seeder_floor: u32,
}

impl Candidate {
    /// Availability bar: at least one live seed.
    pub fn available(&self) -> bool {
        self.seeders >= 1
    }
}

/// A torrent keep-at currently holds and seeds.
#[derive(Debug, Clone)]
pub struct Held {
    pub info_hash: [u8; 20],
    pub title: String,
    pub size_bytes: u64,
    pub seeders: u32,
}

/// Order candidates by seeding urgency: fewest seeds first; unavailable
/// (zero-seed) candidates excluded. Size tie-break prefers smaller normally
/// (cheaper to try first), larger when RAM-bound (bytes per RAM slot).
pub fn rank_candidates(mut candidates: Vec<Candidate>, ram_bound: bool) -> Vec<Candidate> {
    candidates.retain(|c| c.available());
    candidates.sort_by(|a, b| {
        a.seeders.cmp(&b.seeders).then(if ram_bound {
            b.size_bytes.cmp(&a.size_bytes)
        } else {
            a.size_bytes.cmp(&b.size_bytes)
        })
    });
    candidates
}

/// n: the probability keep-at proceeds with a candidate given its seeder
/// count relative to the catalog's p10 floor:
/// n = aggressiveness ^ max(0, seeders - floor).
pub fn selection_chance(aggressiveness: f64, seeders: u32, seeder_floor: u32) -> f64 {
    let seeders = seeders.max(1);
    let floor = seeder_floor.max(1);
    let exponent = seeders.saturating_sub(floor);
    aggressiveness.powi(exponent as i32)
}

/// Nearest-rank p10 over positive seeder counts; 0 when nothing is seeded.
pub fn seeder_floor(seed_counts: &[u32]) -> u32 {
    let mut seeded: Vec<u32> = seed_counts.iter().copied().filter(|&s| s > 0).collect();
    if seeded.is_empty() {
        return 0;
    }
    seeded.sort_unstable();
    let rank = seeded.len().div_ceil(10); // ceil(n/10)
    seeded[rank.saturating_sub(1)]
}

/// Whether the candidate beats every displaced torrent by the margin.
/// Empty displaced list always passes (free-space fill, nothing to beat).
pub fn meets_seed_margin(candidate_seeders: u32, displaced: &[Held], min_seed_margin: i32) -> bool {
    if displaced.is_empty() {
        return true;
    }
    let min_displaced = displaced.iter().map(|d| d.seeders).min().unwrap_or(0);
    (candidate_seeders as i64) <= (min_displaced as i64) - (min_seed_margin as i64)
}

#[derive(Debug, Clone)]
pub struct SwapDecision {
    pub should_swap: bool,
    pub chance: f64,
    pub roll: f64,
    pub reason: String,
}

pub const REASON_SEED_SCARCITY_ROLL_FAILED: &str =
    "seed-scarcity roll failed; candidate already has enough seeders";

impl SwapDecision {
    pub fn seed_scarcity_blocked(&self) -> bool {
        self.reason == REASON_SEED_SCARCITY_ROLL_FAILED
    }
}

/// Decide whether to download `candidate`, optionally displacing `displaced`.
/// Swaps when roll < n. `roll` is a caller-supplied uniform [0,1) draw so
/// this stays deterministic to test.
pub fn evaluate_swap(
    candidate: &Candidate,
    displaced: &[Held],
    min_seed_margin: i32,
    aggressiveness: f64,
    roll: f64,
) -> SwapDecision {
    if !candidate.available() {
        return SwapDecision {
            should_swap: false,
            chance: 0.0,
            roll,
            reason: "candidate has no live seed".to_string(),
        };
    }
    if !meets_seed_margin(candidate.seeders, displaced, min_seed_margin) {
        return SwapDecision {
            should_swap: false,
            chance: 0.0,
            roll,
            reason: "candidate does not beat displaced torrents by the required margin".to_string(),
        };
    }
    let chance = selection_chance(aggressiveness, candidate.seeders, candidate.seeder_floor);
    let should_swap = roll < chance;
    SwapDecision {
        should_swap,
        chance,
        roll,
        reason: if should_swap {
            "seed-scarcity roll succeeded".to_string()
        } else {
            REASON_SEED_SCARCITY_ROLL_FAILED.to_string()
        },
    }
}

/// Weighted choice of storage location index by free-space weights.
/// `free[i]` must be >= `need[i]` to be eligible. Deterministic in `roll`.
pub fn choose_location(free: &[u64], need: &[u64], roll: f64) -> Option<usize> {
    let mut eligible: Vec<(usize, u64)> = free
        .iter()
        .zip(need.iter())
        .enumerate()
        .filter(|(_, (&f, &n))| f >= n)
        .map(|(i, (&f, _))| (i, f))
        .collect();
    if eligible.is_empty() {
        return None;
    }
    eligible.sort_by_key(|(i, _)| *i);
    let total: f64 = eligible.iter().map(|(_, f)| *f as f64).sum();
    let mut target = roll * total;
    for (i, f) in &eligible {
        target -= *f as f64;
        if target < 0.0 {
            return Some(*i);
        }
    }
    // Float rounding: fall back to the last candidate.
    eligible.last().map(|(i, _)| *i)
}

/// Draw a uniform [0,1) roll.
pub fn roll(rng: &mut impl Rng) -> f64 {
    rng.gen_range(0.0..1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(seeders: u32, size: u64) -> Candidate {
        Candidate {
            info_hash: [0u8; 20],
            title: String::new(),
            size_bytes: size,
            seeders,
            leechers: 0,
            seeder_floor: 0,
        }
    }

    #[test]
    fn rank_fewest_seeds_first() {
        let v = rank_candidates(
            vec![cand(5, 10), cand(1, 100), cand(0, 1), cand(1, 5)],
            false,
        );
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].size_bytes, 5);
        assert_eq!(v[1].size_bytes, 100);
        assert_eq!(v[2].seeders, 5);
    }

    #[test]
    fn rank_ram_bound_prefers_larger() {
        let v = rank_candidates(vec![cand(1, 5), cand(1, 100)], true);
        assert_eq!(v[0].size_bytes, 100);
    }

    #[test]
    fn chance_math() {
        // floor 0/1 -> aggressiveness^(seeders-1)
        assert!((selection_chance(0.6, 1, 0) - 1.0).abs() < 1e-12);
        assert!((selection_chance(0.6, 2, 0) - 0.6).abs() < 1e-12);
        // at/below floor -> 1.0
        assert!((selection_chance(0.6, 3, 5) - 1.0).abs() < 1e-12);
        assert!((selection_chance(0.6, 5, 5) - 1.0).abs() < 1e-12);
        assert!((selection_chance(0.6, 6, 5) - 0.6).abs() < 1e-12);
    }

    #[test]
    fn floor_p10() {
        assert_eq!(seeder_floor(&[]), 0);
        assert_eq!(seeder_floor(&[0, 0]), 0);
        // 10 values: rank ceil(10/10)=1 -> min
        assert_eq!(seeder_floor(&[1, 1, 2, 3, 4, 5, 6, 7, 8, 9]), 1);
        // 20 values 1..=20: rank 2 -> 2
        let v: Vec<u32> = (1..=20).collect();
        assert_eq!(seeder_floor(&v), 2);
    }

    #[test]
    fn swap_gates() {
        let c = cand(1, 10);
        let d = evaluate_swap(&c, &[], 2, 0.6, 0.5);
        assert!(d.should_swap);
        let d = evaluate_swap(&c, &[], 2, 0.6, 0.999);
        assert!(d.should_swap); // chance 1.0: any roll < 1 passes
        let c2 = cand(3, 10);
        let d = evaluate_swap(&c2, &[], 2, 0.6, 0.5);
        assert!(!d.should_swap);
        assert!(d.seed_scarcity_blocked());
        // margin
        let held = vec![Held {
            info_hash: [1u8; 20],
            title: String::new(),
            size_bytes: 5,
            seeders: 4,
        }];
        let d = evaluate_swap(&cand(3, 10), &held, 2, 0.6, 0.0);
        assert!(!d.should_swap);
        assert!(!d.seed_scarcity_blocked());
        let d = evaluate_swap(&cand(2, 10), &held, 2, 0.6, 0.0);
        assert!(d.should_swap);
    }

    #[test]
    fn location_choice_weighted() {
        // Only index 1 fits.
        assert_eq!(choose_location(&[10, 100, 5], &[50, 50, 50], 0.0), Some(1));
        assert_eq!(choose_location(&[10, 5], &[50, 50], 0.0), None);
        // Proportional: roll 0 picks first eligible after sort.
        assert_eq!(choose_location(&[100, 100], &[10, 10], 0.0), Some(0));
        assert_eq!(choose_location(&[100, 100], &[10, 10], 0.999), Some(1));
    }
}
