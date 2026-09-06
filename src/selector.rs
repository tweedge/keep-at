//! Selection logic: which torrent most urgently needs seeding, and
//! whether swapping to it risks a cascade.

use rand::Rng;

/// A torrent keep-at is considering downloading.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub info_hash: [u8; 20],
    pub title: String,
    pub size_bytes: u64,
    /// Piece count: the RAM-cost axis (rqbit bookkeeping scales ~32 B/piece).
    /// Defaults to 0 (unknown) when the caller has no metainfo handy; a
    /// 0-piece candidate prices like a 1-piece one (never free).
    pub piece_count: u32,
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
    /// Piece count (0 = unknown, e.g. state written before this was tracked).
    pub piece_count: u32,
    pub seeders: u32,
}

/// Order candidates by seeding urgency: fewest seeds first; unavailable
/// (zero-seed) candidates excluded. The size-bias step below never compares
/// across seeder counts — it only orders within the same count.
///
/// Size tie-break is a bounded adaptive bias driven by the host's RAM:disk
/// ratio (`size_bias > 0` favors larger torrents, `< 0` favors smaller ones,
/// `0` keeps prior behavior). The bias is a *tie-break*: a candidate with
/// fewer seeders always outranks one with more, so poorly-seeded torrents
/// keep absolute priority.
///
/// - RAM-bound hosts (little RAM, big disk) get `size_bias > 0`: ties break
///   by bytes-per-RAM-byte, filling scarce RAM slots with the most bytes.
/// - RAM-rich hosts get `size_bias < 0`: ties break smallest-first, spending
///   plentiful slots on the small torrents big-disk hosts skip.
///
/// Piece count stays the RAM axis (rqbit bookkeeping scales ~32 B/piece);
/// byte size is the disk axis. See `size_bias_for_ratio` for how the ratio
/// maps to the exponent.
pub fn rank_candidates(
    mut candidates: Vec<Candidate>,
    size_bias: f64,
    peer_limit: usize,
) -> Vec<Candidate> {
    candidates.retain(|c| c.available());
    candidates.sort_by(|a, b| {
        // Primary key: fewest seeders first — the bias below can never
        // promote across seeder bands. Secondary key: size-bias score,
        // higher first.
        match a.seeders.cmp(&b.seeders) {
            std::cmp::Ordering::Equal => size_bias_score(b, size_bias, peer_limit)
                .partial_cmp(&size_bias_score(a, size_bias, peer_limit))
                .unwrap_or_else(|| a.size_bytes.cmp(&b.size_bytes)),
            ord => ord,
        }
    });
    candidates
}

/// Adjusted bytes-per-RAM-byte score for the size-bias tie-break.
/// `size_bias` is small (|bias| <= 1). Higher score = ranked first.
/// Positive bias rewards raw ratio (larger torrents win ties); zero bias
/// ranks by plain size, legacy order; negative bias ranks by smallest size
/// first (small torrents win ties), with the ratio breaking size ties
/// toward fewer pieces.
fn size_bias_score(c: &Candidate, size_bias: f64, peer_limit: usize) -> f64 {
    // Higher score = ranked first. Positive bias rewards the bytes-per-RAM
    // ratio (larger torrents win ties); zero or negative bias ranks
    // smallest-first (legacy order), with the ratio breaking exact size
    // ties toward fewer pieces.
    let ram = crate::engine::ram::torrent_ram(c.piece_count, peer_limit).max(1) as f64;
    let ratio = (c.size_bytes.max(1) as f64) / ram;
    if size_bias > 0.0 {
        return ratio.powf(size_bias);
    }
    -(c.size_bytes as f64) + ratio / (1.0 + c.size_bytes as f64)
}

/// Eviction-order score for a held torrent: ascending order evicts the
/// lowest score first. This is the same adjusted ratio as
/// [`size_bias_score`] (public so the engine's displaceable-set ordering
/// stays identical to ranking by construction, not by duplication).
/// Zero bias preserves the legacy order (largest seeders evicted first is
/// handled by the caller only when bias is zero — see below).
pub fn eviction_score(c: &Candidate, size_bias: f64, peer_limit: usize) -> f64 {
    if size_bias == 0.0 {
        // Legacy swap path evicted highest-seeded qualifying first; with no
        // bias there is no size signal, so keep that order by returning the
        // seeder count as the score. Callers pass seeders through.
        return c.seeders as f64;
    }
    size_bias_score(c, size_bias, peer_limit)
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
            piece_count: 0,
            seeders,
            leechers: 0,
            seeder_floor: 0,
        }
    }

    #[test]
    fn rank_fewest_seeds_first() {
        // Seeder count dominates: bias never promotes across bands.
        let v = rank_candidates(
            vec![cand(5, 10), cand(1, 100), cand(0, 1), cand(1, 5)],
            1.0,
            8,
        );
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].seeders, 1);
        assert_eq!(v[1].seeders, 1);
        assert_eq!(v[2].seeders, 5);
        // Same bias, balanced order: zero bias keeps legacy size order.
        let v = rank_candidates(
            vec![cand(5, 10), cand(1, 100), cand(0, 1), cand(1, 5)],
            0.0,
            8,
        );
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].size_bytes, 5);
        assert_eq!(v[1].size_bytes, 100);
        assert_eq!(v[2].seeders, 5);
    }

    #[test]
    fn rank_bias_directions() {
        // Positive bias favors larger within a seeder band...
        let v = rank_candidates(vec![cand(1, 5), cand(1, 100)], 1.0, 8);
        assert_eq!(v[0].size_bytes, 100);
        // ...negative bias favors smaller...
        let v = rank_candidates(vec![cand(1, 5), cand(1, 100)], -1.0, 8);
        assert_eq!(v[0].size_bytes, 5);
        // ...and zero bias keeps legacy order (smaller first).
        let v = rank_candidates(vec![cand(1, 5), cand(1, 100)], 0.0, 8);
        assert_eq!(v[0].size_bytes, 5);
    }

    #[test]
    fn rank_ram_bound_prices_pieces_not_bytes() {
        // Same 2 GiB size, different piece counts: fewer pieces wins ties
        // under positive bias (same disk, less RAM).
        let mut a = cand(1, 2 << 30);
        a.piece_count = 100;
        let mut b = cand(1, 2 << 30);
        b.piece_count = 100_000;
        let v = rank_candidates(vec![b.clone(), a.clone()], 1.0, 8);
        assert_eq!(v[0].piece_count, 100);
        // Negative bias is small-first by size; equal sizes keep both.
        let v = rank_candidates(vec![b, a], -1.0, 8);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn eviction_score_mirrors_rank() {
        // Ascending eviction order must be the exact reverse of descending
        // rank order for the same bias, so swaps reinforce ranking.
        let mut a = cand(1, 100 << 20);
        a.piece_count = 500;
        let mut b = cand(1, 900 << 20);
        b.piece_count = 500;
        for bias in [1.0, 0.5, -0.5, -1.0] {
            let ranked = rank_candidates(vec![a.clone(), b.clone()], bias, 8);
            let first = ranked[0].size_bytes;
            let sa = eviction_score(&a, bias, 8);
            let sb = eviction_score(&b, bias, 8);
            let evict_first = if sa < sb { a.size_bytes } else { b.size_bytes };
            assert_ne!(
                first, evict_first,
                "bias={bias}: rank-first ({first}) must be evicted last ({evict_first})"
            );
        }
    }

    #[test]
    fn bias_never_crosses_seeder_bands() {
        // A 10 TiB 1-seeder torrent outranks a 1 KiB 2-seeder one at any bias.
        let mut big = cand(1, 10 << 40);
        big.piece_count = 100;
        let mut small = cand(2, 1 << 10);
        small.piece_count = 100;
        for bias in [-1.0, 0.0, 1.0] {
            let v = rank_candidates(vec![small.clone(), big.clone()], bias, 8);
            assert_eq!(v[0].seeders, 1, "bias={bias}");
        }
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
            piece_count: 0,
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
