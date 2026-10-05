//! The scan: catalog walk, per-candidate evaluation, incremental acting,
//! stall eviction, deleted-torrent removal.

use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use librqbit::{Api, Session};
use tokio::sync::Semaphore;

use crate::atcatalog::{self};
use crate::atkey;
use crate::attorrent::{self, SwarmCounts, TorrentMeta};
use crate::buildinfo;
use crate::config::Config;
use crate::engine::ram;
use crate::engine::session as rqsession;
use crate::engine::stats as engstats;
use crate::engine::storage;
use crate::engine::torrents as engtorrents;
use crate::filter::KeywordBlocklist;
use crate::history;
use crate::netstats::{self, Snapshot};
use crate::selector::{self, Candidate, Held};
use crate::state::{self, State};

use super::storage::dir_size_bytes;

pub const EVALUATE_CONCURRENCY: usize = 16;
pub const PROGRESS_SAVE_INTERVAL: Duration = Duration::from_secs(2);
pub const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const SCRAPE_TIMEOUT: Duration = Duration::from_secs(15);
/// Extra bytes reserved per torrent beyond nominal size (cached .torrent +
/// state overhead). Small, fixed, conservative.
pub const PER_TORRENT_SIZE_BUFFER: u64 = 256 * 1024;

pub struct Engine {
    cfg: Config,
    session: Arc<Session>,
    api: Api,
    state: State,
    swarm_cache: Arc<std::sync::Mutex<SwarmCache>>,
    blocklist: KeywordBlocklist,
    http: reqwest::Client,
    catalog: atcatalog::Fetcher,
    torrent_fetcher: attorrent::Fetcher,
    user_announce: String,
    user_announce_ipv6: String,
    max_torrents: usize,
    /// RAM budget in bytes (for per-candidate footprint pricing).
    ram_budget: u64,
    /// Adaptive size bias in [-1, +1] from the host's RAM:disk ratio
    /// (positive favors larger torrents in ties, negative smaller ones).
    /// Computed once at startup; disk limits are static per install.
    size_bias: f64,
    /// How long to wait after a tracker 429 before failing the candidate
    /// fast (production 60s). From Options; injectable so the backoff path
    /// runs fast in tests.
    scrape_backoff: Duration,
    started_at: std::time::Instant,
    rate: Arc<tokio::sync::Mutex<RateLimiter>>,
    evaluated_counts: tokio::sync::Mutex<Vec<u32>>,
    last_scan: tokio::sync::Mutex<Option<LastScanStats>>,
    /// Infohashes already fully downloaded the last time completions were
    /// reported. Diffed on every runtime-stats pass so each completion logs
    /// exactly one `download completed` line (see log_and_save_runtime).
    completed_seen: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Push handle for the live query socket (status/hosted-torrents read
    /// from the running daemon instead of stale files). None in tests and
    /// anywhere no server is wanted. Refreshed after every state mutation.
    live: Option<crate::live::LiveHandle>,
    /// Append-only record of additions, swaps, and removals
    /// (<data_dir>/history.jsonl, rendered by `keep-at history`).
    history: crate::history::Writer,
    /// Broken-piece watchdog state (per-torrent receive-counter deltas).
    /// In-memory; the persistent half of the feature is the quarantine
    /// registry inside state.json.
    detector: crate::engine::quarantine::Detector,
    /// Hash set of the most recent catalog fetch, kept for the quarantine
    /// registry GC: an entry may only be dropped when its hash is neither
    /// held, in the session, nor catalog-listed (a listed hash must keep
    /// its entry — and its attempts count — until the lapsed cooldown
    /// re-probe runs). None until the first catalog fetch of this boot
    /// completes; GC waits rather than guessing until then.
    last_catalog: Option<HashSet<String>>,
}

/// Counters from the most recent scan, for tests and status callers.
#[derive(Debug, Clone, Default)]
pub struct LastScanStats {
    pub eligible: u64,
    pub scrape_requests: u64,
    pub scrape_cached: u64,
    pub processed: u64,
    pub total: u64,
}

#[derive(Debug, Default)]
struct ScanStats {
    library_bytes: AtomicU64,
    metadata_fetched: AtomicU64,
    metadata_cached: AtomicU64,
    scrape_requests: AtomicU64,
    scrape_cached: AtomicU64,
    skipped_held: AtomicU64,
    skipped_blocked: AtomicU64,
    skipped_too_big: AtomicU64,
    skipped_age: AtomicU64,
    skipped_fetch_err: AtomicU64,
    skipped_scrape_err: AtomicU64,
    eligible: AtomicU64,
}

/// Simple token-bucket rate limiter (max N requests/sec to AT infrastructure).
struct RateLimiter {
    per_second: f64,
    next_allowed: tokio::time::Instant,
}

impl RateLimiter {
    async fn wait(&mut self) {
        // Defensive: validate() rejects non-positive/NaN, but a panic here
        // (from_secs_f64 on NaN) is abort-in-release, so never do the math
        // on a rate that is not a positive finite number.
        if self.per_second.is_nan() || self.per_second <= 0.0 {
            return;
        }
        let now = tokio::time::Instant::now();
        if now < self.next_allowed {
            tokio::time::sleep(self.next_allowed - now).await;
        }
        // try_from, not from: a rate of 1e-300 passes the NaN/positivity
        // guard but 1/rate overflows Duration (panic = abort in release);
        // validate() floors rates at 1e-6, so this clamp is unreachable
        // belt-and-braces.
        let gap = Duration::try_from_secs_f64(1.0 / self.per_second)
            .unwrap_or(Duration::from_secs(365 * 24 * 3600));
        self.next_allowed = tokio::time::Instant::now() + gap;
    }
}

/// Lightweight evaluated candidate: only what ranking needs. Full metadata
/// lives in torrent-cache on disk and is re-read only when acted on.
#[derive(Debug, Clone)]
struct Evaluated {
    title: String,
    info_hash: [u8; 20],
    size_bytes: u64,
    piece_count: u32,
    seeders: u32,
    leechers: u32,
}

fn add_into(dst: &ScanStats, src: &ScanStats) {
    dst.library_bytes
        .fetch_add(src.library_bytes.load(Ordering::Relaxed), Ordering::Relaxed);
    dst.metadata_fetched.fetch_add(
        src.metadata_fetched.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    dst.metadata_cached.fetch_add(
        src.metadata_cached.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    dst.scrape_requests.fetch_add(
        src.scrape_requests.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    dst.scrape_cached
        .fetch_add(src.scrape_cached.load(Ordering::Relaxed), Ordering::Relaxed);
    dst.skipped_held
        .fetch_add(src.skipped_held.load(Ordering::Relaxed), Ordering::Relaxed);
    dst.skipped_blocked.fetch_add(
        src.skipped_blocked.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    dst.skipped_too_big.fetch_add(
        src.skipped_too_big.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    dst.skipped_age
        .fetch_add(src.skipped_age.load(Ordering::Relaxed), Ordering::Relaxed);
    dst.skipped_fetch_err.fetch_add(
        src.skipped_fetch_err.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    dst.skipped_scrape_err.fetch_add(
        src.skipped_scrape_err.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    dst.eligible
        .fetch_add(src.eligible.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// Arc-owned evaluation context: everything evaluate_one_item needs, without
/// borrowing the Engine across spawned tasks.
/// Owned inputs for one evaluation walk. Lets the evaluation task run
/// without borrowing the Engine while the scan's consume loop acts.
/// The swarm cache is shared by Arc (not reloaded): inserts made by
/// evaluation workers are visible to the Engine immediately, and the Engine
/// saves it at scan end as before.
struct EvaluationDispatch {
    ctx: Arc<EvalCtx>,
    shutdown: tokio::sync::watch::Receiver<bool>,
    items: Vec<atcatalog::Item>,
    held_hashes: HashSet<String>,
    scan_started_at: DateTime<Utc>,
    total_candidates: u64,
    max_fittable: u64,
    blocklist: KeywordBlocklist,
    swarm_cache: Arc<std::sync::Mutex<SwarmCache>>,
    network_stats_path: PathBuf,
}

/// Drive one evaluation walk to completion, streaming each eligible
/// candidate over `emit` as it completes. Merges the shared stat counters
/// into `stats` at the end (atomics - safe across tasks).
async fn run_evaluation(
    dispatch: EvaluationDispatch,
    emit: tokio::sync::mpsc::UnboundedSender<Evaluated>,
    stats: Arc<ScanStats>,
) {
    let EvaluationDispatch {
        ctx,
        shutdown,
        items,
        held_hashes,
        scan_started_at,
        total_candidates,
        max_fittable,
        blocklist,
        swarm_cache: shared_cache,
        network_stats_path,
    } = dispatch;
    // `stats` is the caller-shared accumulator (Arc<ScanStats>, all-atomics).
    // Workers record into it directly; the pre-filter records locally and
    // merges at the end.
    let prefilter = ScanStats::default();

    let sem = Arc::new(Semaphore::new(EVALUATE_CONCURRENCY));
    let processed = Arc::new(AtomicU64::new(0));

    // Progress saver (same as before: persists processed counts every 2s).
    let stats_path = network_stats_path;
    let seeder_floor = netstats::load_snapshot(&stats_path)
        .map(|s| s.seeder_floor)
        .unwrap_or(0);
    let prog_processed = processed.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop2 = stop.clone();
    let progress_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(PROGRESS_SAVE_INTERVAL);
        loop {
            tick.tick().await;
            if stop2.load(Ordering::Relaxed) {
                break;
            }
            let _ = netstats::save_snapshot(
                &stats_path,
                &Snapshot {
                    scan_started_at: Some(scan_started_at),
                    scan_completed_at: None,
                    total_candidates,
                    processed_candidates: prog_processed.load(Ordering::Relaxed),
                    seeder_floor,
                },
            );
        }
    });

    // Pre-filter (cheap, synchronous): held / blocked / too-big never even
    // reach the worker pool. Skipped-held/blocked/too-big counters merge
    // into stats at the end via add_into below... they are recorded on
    // `stats` directly here (same task tree, no race beyond atomics).
    let mut pending: Vec<atcatalog::Item> = Vec::new();
    for item in &items {
        if *shutdown.borrow() {
            break;
        }
        match prefilter_reason(item, &held_hashes, &blocklist, max_fittable) {
            Prefilter::Held => prefilter.skipped_held.fetch_add(1, Ordering::Relaxed),
            Prefilter::Blocked => prefilter.skipped_blocked.fetch_add(1, Ordering::Relaxed),
            Prefilter::TooBig => prefilter.skipped_too_big.fetch_add(1, Ordering::Relaxed),
            Prefilter::Pending => {
                pending.push(item.clone());
                continue;
            }
        };
    }

    let tx2 = emit.clone();
    let mut handles = Vec::new();
    for chunk in pending.chunks(EVALUATE_CONCURRENCY * 4) {
        if *shutdown.borrow() {
            break;
        }
        let chunk = chunk.to_vec();
        let sem = sem.clone();
        let tx = tx2.clone();
        let processed = processed.clone();
        let ctx = ctx.clone();
        let shared_cache = shared_cache.clone();
        let stats_outer = stats.clone();
        handles.push(tokio::spawn(async move {
            let mut inner = Vec::new();
            for item in chunk {
                let permit = sem.clone().acquire_owned().await;
                let tx = tx.clone();
                let processed = processed.clone();
                let ctx = ctx.clone();
                let shared_cache = shared_cache.clone();
                let stats = stats_outer.clone();
                inner.push(tokio::spawn(async move {
                    let _permit = permit;
                    let r = evaluate_one_item(&ctx, &shared_cache, &item, &stats).await;
                    processed.fetch_add(1, Ordering::Relaxed);
                    if let Some(ev) = r {
                        let _ = tx.send(ev);
                    }
                }));
            }
            for h in inner {
                let _ = h.await;
            }
        }));
    }
    drop(tx2);
    // Dispatch tasks run detached: results stream to the caller over the
    // channel as they complete. Abandoned tasks on shutdown finish in the
    // background; their sends fail silently once the caller drops the
    // receiver. Await the dispatchers (not the per-item workers - those are
    // joined by their dispatcher) so the progress task and merges below
    // run after dispatch, while the caller keeps consuming.
    //
    // Shutdown-abort: don't await wedged dispatchers forever; the caller's
    // consume loop bails on shutdown independently, and drops the receiver.
    let mut sd = shutdown.clone();
    tokio::select! {
        _ = async {
            for h in handles {
                let _ = h.await;
            }
        } => {}
        _ = sd.changed() => {}
    }
    stop.store(true, Ordering::Relaxed);
    let _ = progress_task.await;
    drop(emit);
    // Merge pre-filter counters into the caller's scan stats (workers wrote
    // theirs directly into the shared accumulator).
    add_into(&stats, &prefilter);
    // No Engine-cache merge needed: workers inserted directly into the
    // shared Engine cache via Arc (visible immediately, saved at scan end).
}

#[derive(Clone)]
struct EvalCtx {
    data_dir: PathBuf,
    moderation_bypass: Duration,
    http: reqwest::Client,
    torrent_base_url: String,
    user_agent: String,
    rate: Arc<tokio::sync::Mutex<RateLimiter>>,
    shutdown: tokio::sync::watch::Receiver<bool>,
    /// 429 backoff (production 60s; ~0s in tests). Stored here so spawned
    /// evaluation workers see the same value as held-refresh.
    scrape_backoff: Duration,
}

/// Where a fetched `.torrent` is cached. Single spelling on purpose: the
/// engine, the spawned evaluation workers, and the census probe all have to
/// agree on this path, and three near-identical copies of it had already
/// spread across those callers.
pub fn torrent_cache_path(data_dir: &Path, info_hash_hex: &str) -> PathBuf {
    data_dir
        .join("torrent-cache")
        .join(format!("{info_hash_hex}.torrent"))
}

async fn eval_fetch_metadata(ctx: &EvalCtx, info_hash_hex: &str) -> Result<TorrentMeta> {
    let path = torrent_cache_path(&ctx.data_dir, info_hash_hex);
    if let Ok(data) = std::fs::read(&path) {
        if let Ok(md) = attorrent::parse_torrent_bytes(&data) {
            return Ok(md);
        }
    }
    ctx.rate.lock().await.wait().await;
    let fetcher = attorrent::Fetcher {
        base_url: ctx.torrent_base_url.clone(),
        user_agent: ctx.user_agent.clone(),
        client: ctx.http.clone(),
    };
    // fetch_torrent writes the body to the cache file itself; only the
    // lightweight TorrentMeta (no raw bytes) is returned and kept.
    let (md, _raw) = fetcher.fetch_torrent(info_hash_hex, Some(&path)).await?;
    Ok(md)
}

/// The shared scrape infrastructure: everything the tracker loop needs that
/// is identical across its callers (the evaluation workers and the
/// held-refresh pass). Bundled so [`scrape_swarm_cached`] stays about the
/// protocol rather than about plumbing.
struct ScrapeShared<'a> {
    cache: &'a std::sync::Mutex<SwarmCache>,
    rate: &'a Arc<tokio::sync::Mutex<RateLimiter>>,
    http: &'a reqwest::Client,
    shutdown: &'a tokio::sync::watch::Receiver<bool>,
    scrape_backoff: Duration,
}

/// Scrape trackers in order (cached first); AT hosts go through the shared
/// rate limiter.
///
/// THE scrape policy for this daemon, used by both the evaluation workers
/// and the held-refresh pass. It used to exist as two byte-identical copies
/// (`eval_scrape_swarm` and `Engine::scrape_swarm`) that had already drifted
/// in their log wording; the 429 handling here is the most delicate shared
/// behavior keep-at owns (it exists because AT throttled this project), and
/// any fix to it must land in one place.
async fn scrape_swarm_cached(
    shared: &ScrapeShared<'_>,
    trackers: &[String],
    info_hash: &[u8; 20],
    stats: &ScanStats,
) -> Result<SwarmCounts> {
    if let Ok(c) = shared.cache.lock() {
        if let Some(hit) = c.get(info_hash) {
            stats.scrape_cached.fetch_add(1, Ordering::Relaxed);
            return Ok(hit);
        }
    }
    let mut last_err: Option<anyhow::Error> = None;
    for tracker in trackers {
        if crate::atkey::is_at_tracker_url(tracker) {
            shared.rate.lock().await.wait().await;
        }
        if !tracker.starts_with("http://") && !tracker.starts_with("https://") {
            // UDP trackers (BEP 15 scrape) are not implemented - skip
            // quietly; the AT https tracker answers for AT content.
            continue;
        }
        stats.scrape_requests.fetch_add(1, Ordering::Relaxed);
        let call = tokio::time::timeout(
            SCRAPE_TIMEOUT,
            attorrent::scrape_http(
                shared.http,
                &buildinfo::scraper_user_agent(),
                tracker,
                info_hash,
            ),
        )
        .await;
        match call {
            Ok(Ok(c)) => {
                if let Ok(mut cc) = shared.cache.lock() {
                    cc.insert(*info_hash, c);
                }
                return Ok(c);
            }
            Ok(Err(e)) => {
                if is_rate_limited(&e) {
                    // AT is throttling us: back off (shutdown-aware) and fail
                    // fast so the scan stops burning the shared budget.
                    // Nothing is cached; the next scan retries these.
                    tracing::warn!("tracker rate-limited, backing off: {e:#}");
                    let mut sd = shared.shutdown.clone();
                    tokio::select! {
                        _ = tokio::time::sleep(shared.scrape_backoff) => {}
                        _ = sd.changed() => {
                            anyhow::bail!("scan interrupted by shutdown");
                        }
                    }
                    return Err(anyhow::anyhow!("tracker rate-limited"));
                }
                last_err = Some(e)
            }
            Err(_) => last_err = Some(anyhow::anyhow!("scrape timed out")),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no tracker returned scrape data")))
}

async fn eval_scrape_swarm(
    ctx: &EvalCtx,
    cache: &std::sync::Mutex<SwarmCache>,
    trackers: &[String],
    info_hash: &[u8; 20],
    stats: &ScanStats,
) -> Result<SwarmCounts> {
    let shared = ScrapeShared {
        cache,
        rate: &ctx.rate,
        http: &ctx.http,
        shutdown: &ctx.shutdown,
        scrape_backoff: ctx.scrape_backoff,
    };
    scrape_swarm_cached(&shared, trackers, info_hash, stats).await
}

/// True when an error looks like HTTP 429 / rate limiting from a tracker.
pub fn is_rate_limited(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}");
    s.contains("429") || s.to_lowercase().contains("too many requests")
}

async fn evaluate_one_item(
    ctx: &EvalCtx,
    cache: &std::sync::Mutex<SwarmCache>,
    item: &atcatalog::Item,
    stats: &ScanStats,
) -> Option<Evaluated> {
    let hex = hex::encode(item.info_hash);
    let md = match eval_fetch_metadata(ctx, &hex).await {
        Ok(md) => {
            stats.metadata_fetched.fetch_add(1, Ordering::Relaxed);
            md
        }
        Err(e) => {
            stats.skipped_fetch_err.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                "skipping candidate {}: could not fetch metadata: {e:#}",
                item.title
            );
            return None;
        }
    };

    // Age gate: creation date must be at least moderation_delay old;
    // unknown age => not eligible.
    match md.created_at.map(|t| {
        Utc::now()
            .signed_duration_since(t)
            .to_std()
            .unwrap_or(Duration::ZERO)
    }) {
        Some(age) if age >= ctx.moderation_bypass => {}
        _ => {
            stats.skipped_age.fetch_add(1, Ordering::Relaxed);
            return None;
        }
    }

    let swarm = match eval_scrape_swarm(ctx, cache, &md.trackers, &md.info_hash, stats).await {
        Ok(s) => s,
        Err(e) => {
            stats.skipped_scrape_err.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                "skipping candidate {}: could not scrape trackers: {e:#}",
                item.title
            );
            return None;
        }
    };

    stats.eligible.fetch_add(1, Ordering::Relaxed);
    tracing::debug!(
        "eligible candidate (title={} seeders={} leechers={} size={} pieces={})",
        item.title,
        swarm.seeders,
        swarm.leechers,
        crate::humanize::human_bytes(md.total_length as i64),
        md.piece_count,
    );
    Some(Evaluated {
        title: item.title.clone(),
        info_hash: md.info_hash,
        size_bytes: md.total_length,
        piece_count: md.piece_count,
        seeders: swarm.seeders,
        leechers: swarm.leechers,
    })
}

/// Constructor inputs that are not user-facing config - test seams for
/// overriding the catalog / AT base URLs.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub catalog_url: Option<String>,
    pub at_base_url: Option<String>,
    /// How long evaluation and held-refresh wait after a tracker 429 before
    /// failing the candidate fast. Production default 60s; tests set ~0s so
    /// the backoff path runs fast instead of being skipped or slow.
    /// None means the 60s production default.
    pub scrape_backoff: Option<Duration>,
}

impl Engine {
    pub async fn new(cfg: Config) -> Result<Engine> {
        Engine::new_with_options(cfg, Options::default()).await
    }

    pub async fn new_with_options(cfg: Config, opts: Options) -> Result<Engine> {
        cfg.validate()?;
        let cfg = storage::resolve_all_limits(&cfg)?;

        // Data dir: traversable by everyone (0o755 masked in) so any user's
        // `status`/`hosted-torrents` can reach the world-readable snapshots
        // inside — even when the daemon runs as root with a strict umask.
        // Repair pass: an existing dir keeps whatever it had, so fix it up.
        std::fs::create_dir_all(&cfg.data_dir)
            .with_context(|| format!("creating data dir {}", cfg.data_dir.display()))?;
        crate::config::ensure_shared_dirs(&cfg.data_dir);
        for loc in &cfg.storage {
            std::fs::create_dir_all(&loc.path)
                .with_context(|| format!("creating storage {}", loc.path.display()))?;
        }

        let mut state = State::load(&cfg.data_dir.join("state.json"))?;
        // Storage locations are canonicalized in resolve_all_limits; re-key
        // legacy state entries spelled under an old config spelling or
        // symlink so accounting matches the configured locations.
        let locations: Vec<PathBuf> = cfg.storage.iter().map(|l| l.path.clone()).collect();
        if state.rekey_storage_locations(&locations) {
            tracing::info!("re-keyed held torrents to canonical storage locations");
        }
        let swarm_cache =
            SwarmCache::load(&cfg.data_dir.join("scrape-cache.json"), cfg.scan.interval);

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(buildinfo::user_agent())
            .build()
            .context("building HTTP client")?;

        // API key -> per-user announce URL (non-fatal on failure).
        let (user_announce, user_announce_ipv6) = if !cfg.api_key.is_empty() {
            match atkey::resolve_user_announce(&http, &cfg.api_key).await {
                Ok((a, b)) => {
                    tracing::info!("Academic Torrents API key resolved; seeded torrents will be attributed to your account");
                    (a, b)
                }
                Err(e) => {
                    tracing::warn!("could not resolve Academic Torrents API key; seeded torrents won't be attributed to your account: {e:#}");
                    (String::new(), String::new())
                }
            }
        } else {
            (String::new(), String::new())
        };

        // RAM budget -> torrent-count cap.
        let system_total = ram::system_total_ram();
        let (budget, hard_cap, max_torrents) =
            ram::max_torrents_for_budget(system_total, cfg.max_ram);
        if system_total > 0 {
            if cfg.max_ram > 0 && cfg.max_ram > hard_cap {
                anyhow::bail!(
                    "max_ram {} exceeds the 80%-of-system hard cap of {} (system total {})",
                    crate::humanize::human_bytes(cfg.max_ram as i64),
                    crate::humanize::human_bytes(hard_cap as i64),
                    crate::humanize::human_bytes(system_total as i64),
                );
            }
        } else {
            tracing::warn!("could not measure system RAM; the RAM-driven torrent cap is disabled");
        }
        tracing::info!(
            "RAM budget: system {} hard-cap-80% {} budget {} peer-limit {} size-bias {:+.2} max-torrents {}",
            crate::humanize::human_bytes(system_total as i64),
            crate::humanize::human_bytes(hard_cap as i64),
            crate::humanize::human_bytes(budget as i64),
            ram::peer_limit_for_budget(budget),
            ram::size_bias_for_ratio(budget, disk_limit_total(&cfg)),
            max_torrents,
        );

        let session = rqsession::new_seeder_session(&cfg, budget).await?;
        let api = Api::new(session.clone(), None);

        let catalog = atcatalog::Fetcher {
            cache_path: cfg.data_dir.join("database.xml"),
            url: opts
                .catalog_url
                .clone()
                .unwrap_or_else(|| atcatalog::DEFAULT_URL.to_string()),
            user_agent: buildinfo::user_agent(),
            client: http.clone(),
        };
        let torrent_fetcher = attorrent::Fetcher {
            base_url: opts
                .at_base_url
                .clone()
                .unwrap_or_else(|| "https://academictorrents.com".to_string()),
            user_agent: buildinfo::user_agent(),
            client: http.clone(),
        };

        let size_bias = ram::size_bias_for_ratio(budget, disk_limit_total(&cfg));

        let history = crate::history::Writer::new(cfg.data_dir.join("history.jsonl"));

        let mut eng = Engine {
            cfg,
            session,
            api,
            state,
            swarm_cache: Arc::new(std::sync::Mutex::new(swarm_cache)),
            blocklist: KeywordBlocklist::new(Vec::new()),
            http,
            catalog,
            torrent_fetcher,
            user_announce,
            user_announce_ipv6,
            max_torrents,
            ram_budget: budget,
            size_bias,
            scrape_backoff: opts.scrape_backoff.unwrap_or(Duration::from_secs(60)),
            started_at: std::time::Instant::now(),
            rate: Arc::new(tokio::sync::Mutex::new(RateLimiter {
                per_second: 0.5,
                next_allowed: tokio::time::Instant::now(),
            })),
            evaluated_counts: tokio::sync::Mutex::new(Vec::new()),
            last_scan: tokio::sync::Mutex::new(None),
            completed_seen: std::sync::Mutex::new(std::collections::HashSet::new()),
            live: None,
            history,
            detector: crate::engine::quarantine::Detector::new(),
            last_catalog: None,
        };
        eng.blocklist = KeywordBlocklist::new(eng.cfg.keyword_blocklist.clone());
        {
            let mut r = eng.rate.lock().await;
            r.per_second = eng.cfg.scan.rate_limit_per_second;
        }

        eng.resume_held().await?;
        Ok(eng)
    }

    fn network_stats_path(&self) -> PathBuf {
        self.cfg.data_dir.join("network-stats.json")
    }

    /// Fallback socket startup for engines with no attached handle (direct
    /// `run()` in tests): builds a live handle from the existing session and
    /// serves it. The production path binds earlier via `attach_live` (see
    /// cmd_run) so the socket exists during boot; call
    /// [`Engine::ensure_live_server`](Self::ensure_live_server) instead of
    /// this in `run()` - an unconditional start here would overwrite the
    /// attached handle with one that can never bind the socket.
    fn start_live_server(&mut self) {
        let handle = self.test_live_handle();
        self.live = Some(handle.clone());
        let data_dir = self.cfg.data_dir.clone();
        tokio::spawn(async move {
            crate::live::serve(data_dir, handle).await;
        });
    }

    /// `run()`'s live-socket startup step: keep the handle attached by
    /// cmd_run (already bound and serving the booting view), and only fall
    /// back to a self-made server when nothing is attached.
    ///
    /// The fallback used to run unconditionally, overwriting `self.live`
    /// with a fresh handle whose server can never bind the socket (the
    /// attached one owns it). Every later state push landed on that dead
    /// handle, so the served held snapshot froze at boot state forever:
    /// observed on a 0.8.23 node as held stuck at 693 while state.json and
    /// the session had grown to 823 (the session-derived seeding/downloading
    /// counters stayed live because the attached handle shares the session).
    pub fn ensure_live_server(&mut self) {
        if self.live.is_some() {
            return;
        }
        self.start_live_server();
    }

    /// Attach the booting-phase live handle created by cmd_run and promote
    /// it to the live engine view (session + held snapshot + Seeding).
    pub fn attach_live(&mut self, handle: crate::live::LiveHandle) {
        handle.activate(
            self.session.clone(),
            self.api.clone(),
            self.state.quarantine_count(),
            snapshot_entries(&self.state),
        );
        self.live = Some(handle);
    }

    /// Push current state to the live handle (no-op when no server).
    fn push_live(&self) {
        if let Some(live) = &self.live {
            live.refresh(self.state.quarantine_count(), snapshot_entries(&self.state));
        }
    }

    /// Report what the Engine is doing to the live handle (no-op when none).
    fn set_activity(&self, a: crate::live::Activity) {
        if let Some(live) = &self.live {
            live.set_activity(a);
        }
    }

    fn torrent_cache_path(&self, info_hash_hex: &str) -> PathBuf {
        torrent_cache_path(&self.cfg.data_dir, info_hash_hex)
    }

    // ---- main loop (Run) ----

    /// Run scans on the scan-interval cadence until cancelled. First scan runs
    /// immediately unless a scan completed recently. Spawns the live query
    /// socket (`status`/`hosted-torrents` read here, not stale files) before
    /// the first scan; state pushes after every mutation keep it current.
    pub async fn run(&mut self, mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
        self.ensure_live_server();
        self.log_and_save_runtime("startup");

        // Periodic runtime stats tick on their own interval. The ticker is
        // created BEFORE the single run loop's first wait: a host sleeping
        // out a long scan delay (e.g. 336h with a recent completion) still
        // gets periodic stats passes and tracker feeds — previously the
        // ticker was created after the delay, so the whole sleep went
        // without stats and the persisted snapshot went stale for days.
        let stats_interval = self.cfg.stats_interval;
        let mut stats_tick = tokio::time::interval(stats_interval.max(Duration::from_secs(60)));
        if stats_interval <= Duration::ZERO {
            // Disabled: set a far-future interval so the arm never fires.
            stats_tick = tokio::time::interval(Duration::from_secs(3600 * 24 * 365));
        } else {
            // Consume the immediate first tick (startup already logged its
            // stats line above); periodic passes then fire from the single
            // run loop at the proper cadence.
            stats_tick.tick().await;
        }

        // Broken-piece watchdog on its own cadence (also disabled via 0).
        // Same arming pattern as stats_tick: armed in the single run-loop
        // select, so passes fire during both the post-boot wait and the
        // between-scan waits. Delay (not the default Burst) on purpose: a
        // burst of back-to-back catch-up passes after a long scan would
        // inflate the detector's zero-progress window count with sub-second
        // gaps, breaking the documented "two passes one interval apart"
        // guarantee. Delay reschedules one interval after the last pass.
        let quarantine_interval = self.cfg.scan.quarantine_check_interval;
        let quarantine_on = quarantine_interval > Duration::ZERO;
        let mut quarantine_tick =
            tokio::time::interval(quarantine_interval.max(Duration::from_secs(60)));
        quarantine_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        if !quarantine_on {
            quarantine_tick = tokio::time::interval(Duration::from_secs(3600 * 24 * 365));
        } else {
            quarantine_tick.tick().await;
        }

        // ONE loop, scans deadline-driven. The deadline derives from
        // delay_until_next_scan() (scan_completed_at + interval), so the
        // schedule is anchored to actual scan completions with no interval
        // ticker to drift, and the periodic arms exist in exactly one
        // place. The previous two-loop structure (bare-select wait, then a
        // tickered main loop) duplicated these arms, and both of its bugs
        // were of exactly that drift class: the ticker was once created
        // after the delay (whole sleep went without stats), and the bare
        // select ended the post-boot wait on the first stats tick
        // (observed live: Sep 21 + Sep 26 boots scanned ~30min in, days
        // early).
        let first_wait = self.delay_until_next_scan();
        let mut deadline = tokio::time::Instant::now() + first_wait;
        let mut first_scan = true;
        if first_wait > Duration::ZERO {
            tracing::info!(
                "next scan is not due yet; waiting {} instead of scanning immediately",
                crate::humanize::human_duration(first_wait)
            );
        }

        // NOTE: while a scan runs, the task is inside run_scan_logged and
        // this select is not live, so periodic ticks only fire between
        // scans. The watchdog arm uses MissedTickBehavior::Delay (no
        // catch-up bursts into the detector); stats_tick may burst, which
        // only costs duplicate stats lines. Post-scan saves below
        // guarantee fresh stats after every scan regardless.
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = tokio::time::sleep_until(deadline) => {
                    let kind = if first_scan { "initial" } else { "periodic" };
                    let completed_ok = self
                        .run_scan_logged(kind, shutdown.clone())
                        .await;
                    self.log_and_save_runtime(if first_scan {
                        "post-initial-scan"
                    } else {
                        "post-scan"
                    });
                    first_scan = false;
                    // A failed scan leaves scan_completed_at unset, which
                    // delay_until_next_scan reports as "due now" — pacing
                    // the retry by the full interval instead, so a
                    // persistently failing scan (AT outage) can never
                    // hot-loop. The 60s floor keeps degenerate
                    // `scan.interval: 0` configs paced like the old
                    // interval ticker did.
                    let wait = if completed_ok {
                        self.delay_until_next_scan()
                    } else {
                        self.cfg.scan.interval
                    };
                    deadline =
                        tokio::time::Instant::now() + wait.max(Duration::from_secs(60));
                }
                _ = stats_tick.tick(), if stats_interval > Duration::ZERO => {
                    self.log_and_save_runtime("periodic");
                }
                _ = quarantine_tick.tick(), if quarantine_on => {
                    self.quarantine_pass().await;
                }
            }
        }
        Ok(())
    }

    /// Run one scan, log duration/outcome, restore the Seeding activity
    /// state. Returns whether the scan COMPLETED (success) — the run loop
    /// paces failed-scan retries by the interval on this signal, because a
    /// failure leaves scan_completed_at unset (which reads as "due now").
    async fn run_scan_logged(
        &mut self,
        kind: &str,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> bool {
        self.set_activity(crate::live::Activity::Scanning);
        tracing::info!("scan starting (kind={kind})");
        let start = std::time::Instant::now();
        let result = self.scan_once_shutdown(shutdown).await;
        self.set_activity(crate::live::Activity::Seeding);
        match result {
            Ok(()) => {
                tracing::info!(
                    "scan completed (kind={kind}, duration={:?})",
                    start.elapsed()
                );
                true
            }
            Err(e) => {
                tracing::error!(
                    "scan failed (kind={kind}, duration={:?}): {e:#}",
                    start.elapsed()
                );
                false
            }
        }
    }

    fn delay_until_next_scan(&self) -> Duration {
        if self.cfg.scan.interval <= Duration::ZERO {
            return Duration::ZERO;
        }
        let snap = netstats::load_snapshot(&self.network_stats_path()).unwrap_or_default();
        match snap.scan_completed_at {
            Some(done) => {
                let elapsed = (Utc::now() - done).to_std().unwrap_or(Duration::ZERO);
                self.cfg
                    .scan
                    .interval
                    .checked_sub(elapsed)
                    .unwrap_or(Duration::ZERO)
            }
            None => Duration::ZERO,
        }
    }

    // ---- broken-piece quarantine ----

    /// One watchdog pass: read every live torrent's receive counters, feed
    /// the detector, refresh state.json progress bookkeeping, and remove +
    /// quarantine any torrent whose swarm keeps sending bytes that never
    /// pass hash validation. Runs on `scan.quarantine_check_interval`
    /// between scans (and inside the inter-scan sleep); scans refresh the
    /// same bookkeeping. See notes/DESIGN-broken-piece-quarantine.md.
    async fn quarantine_pass(&mut self) {
        if self.cfg.scan.quarantine_check_interval <= Duration::ZERO {
            return;
        }
        let threshold = self.cfg.scan.broken_piece_discard_bytes;
        let min_windows = self.cfg.scan.broken_piece_min_windows;
        let session = self.session.clone();
        // with_torrents takes an Fn closure, so mutable state rides in
        // Cell/RefCell (the established pattern here — see
        // log_and_save_runtime); the detector is swapped out wholesale.
        let detector = std::cell::RefCell::new(std::mem::take(&mut self.detector));
        let trips =
            std::cell::RefCell::new(Vec::<(String, crate::engine::quarantine::Trip)>::new());
        let progress = std::cell::RefCell::new(Vec::<(String, u64)>::new());
        let seen = std::cell::RefCell::new(std::collections::HashSet::<String>::new());
        // Registry snapshot for lift discovery inside the closure (self is
        // not reachable there); lifts are applied after the sweep.
        let quarantined_keys = self.state.quarantined_keys();
        let lifts = std::cell::RefCell::new(Vec::<String>::new());
        session.with_torrents(|it| {
            for (_, h) in it {
                let hex = h.info_hash().as_string();
                seen.borrow_mut().insert(hex.clone());
                let st = h.stats();
                if st.finished {
                    detector.borrow_mut().forget(&hex);
                    // A FINISHED torrent with a registry entry means its
                    // re-probe completed against the registered hashes:
                    // upstream fixed the data. Lift (applied below).
                    // Deliberately strict: only a COMPLETE re-probe lifts,
                    // so a mixed swarm (one poisoned piece among valid
                    // ones — the NotaBug shape) never lifts and its
                    // attempts keep accumulating for max_retries
                    // escalation.
                    if quarantined_keys.contains(&hex) {
                        lifts.borrow_mut().push(hex.clone());
                    }
                    continue;
                }
                // Paused/error/initializing torrents expose no live counters:
                // a mid-check torrent's counter movement is check reads, not
                // peer wire data, and must not count as waste.
                let Some(live) = st.live.as_ref() else {
                    continue;
                };
                let checked = st.progress_bytes;
                let fetched = live.snapshot.fetched_bytes;
                if let Some(trip) =
                    detector
                        .borrow_mut()
                        .observe(&hex, checked, fetched, threshold, min_windows)
                {
                    trips.borrow_mut().push((hex.clone(), trip));
                }
                if checked > 0 {
                    progress.borrow_mut().push((hex, checked));
                }
            }
        });
        self.detector = detector.into_inner();
        let progress = progress.into_inner();
        let trips = trips.into_inner();
        // Prune stale entries in the same breath: anything the session no
        // longer holds (swap displacement, stall/vanished evictions, failed
        // adds, census teardown) cannot trip again, and re-adding
        // re-baselines from zero anyway.
        let seen = seen.into_inner();
        self.detector.retain_only(&seen);
        // Progress bookkeeping (design gap #2): last_progress_at used to
        // freeze between scans, leaving the stall-eviction clock anchored
        // to scan times. ONE save for the whole pass (the per-entry
        // State::update version rewrote the full file per progressing
        // torrent — hundreds of saves on a mass-progress pass).
        let now = Utc::now();
        let mut lift_hexes: Vec<String> = lifts.into_inner();
        let quarantined_keys = self.state.quarantined_keys();
        for (hex, checked) in &progress {
            // Race catch for the sweep's finished branch: every piece
            // validated but rqbit's finished flag has not flipped yet.
            // Same completion rule, unit-tested as state::completion_lifts
            // (and via State::lift_completed).
            if quarantined_keys.contains(hex) {
                let size = self.state.get(hex).map(|t| t.size_bytes).unwrap_or(0);
                if state::completion_lifts(*checked, size) {
                    lift_hexes.push(hex.clone());
                }
            }
        }
        let changed = self
            .state
            .update_progress_many(&progress, now)
            .map_err(|e| tracing::error!("progress bookkeeping failed: {e:#}"))
            .unwrap_or(false);
        // Apply lifts discovered in the sweep AND the race catch: the
        // re-probe completed, so the cooldown is over. Attempts reset here
        // on purpose — the problem is RESOLVED (locally complete +
        // hash-valid); a later re-break starts a fresh cycle count.
        let mut lifted_any = false;
        match self.state.quarantine_remove_many(&lift_hexes) {
            Ok(0) => {}
            Ok(_) => {
                lifted_any = true;
                for hex in &lift_hexes {
                    let title = self
                        .state
                        .get(hex)
                        .map(|t| t.title.clone())
                        .unwrap_or_else(|| hex.clone());
                    tracing::info!(
                        "quarantine lifted for {title} ({hex}): re-probe completed (upstream data fixed)"
                    );
                }
            }
            Err(e) => tracing::error!("failed to lift quarantines: {e:#}"),
        }
        // Registry GC: an entry whose hash is in NEITHER the held set NOR
        // the session NOR the last fetched catalog can never be re-probed
        // (the gate consults catalog candidates at scan time, so a still
        // listed hash must keep its entry — and its attempts count — until
        // the lapsed-cooldown probe runs) — drop it so
        // delisted/stall-evicted/hand-deleted hashes don't accumulate
        // forever. Active-cooldown entries stay: the catalog may relist the
        // hash before the cooldown lapses. Until a catalog fetch has
        // completed this boot (last_catalog is None) GC waits entirely:
        // dropping listed hashes there would silently cancel the documented
        // re-probe and reset attempts escalation.
        let listed = self.last_catalog.as_ref();
        let mut gc: Vec<String> = self
            .state
            .quarantined_keys()
            .into_iter()
            .filter(|k| {
                !seen.contains(k)
                    && self.state.get(k).is_none()
                    && listed.is_some_and(|c| !c.contains(k))
                    && self
                        .state
                        .quarantine_get(k)
                        .map(|q| now >= q.cooldown_until)
                        .unwrap_or(false)
            })
            .collect();
        if !gc.is_empty() {
            gc.sort();
            match self.state.quarantine_remove_many(&gc) {
                Ok(n) if n > 0 => {
                    lifted_any = true;
                    tracing::info!(
                        "quarantine GC dropped {} orphaned entr{} (hash no longer held, listed, or in session)",
                        n,
                        if n == 1 { "y" } else { "ies" }
                    );
                }
                Err(e) => tracing::error!("quarantine GC failed: {e:#}"),
                Ok(_) => {}
            }
        }
        if trips.is_empty() {
            // A lift or GC still changes the live quarantined count — push
            // it before bailing, or `status` shows a stale count until the
            // next mutation (up to a full scan interval away). Idle passes
            // (no trips, no lifts, no GC, no progress) still write nothing.
            if lifted_any || changed {
                self.push_live();
            }
            return;
        }
        // Calendar math hardened: from_std failure clamps to the
        // indefinite sentinel, and every add is checked (chrono's DateTime
        // + TimeDelta PANICS on overflow — abort-in-release crash loop —
        // so a huge cooldown value must never reach a bare `now + d`).
        // validate() bounds the knob at 366 days, so both clamps are
        // unreachable belt-and-braces.
        let cooldown_chrono = chrono::Duration::from_std(self.cfg.scan.quarantine_cooldown)
            .unwrap_or_else(|_| chrono::Duration::weeks(520));
        let max_retries = self.cfg.scan.quarantine_max_retries;
        let now = Utc::now();
        for (hex, trip) in trips {
            let Some(held) = self.state.get(&hex).cloned() else {
                // Session-managed but UNTRACKED. Normally impossible, but
                // two real shapes leave exactly this: a state.put that
                // failed right after a successful add (data-dir device
                // full), and a session delete that failed during an
                // earlier trip (hash already gated in the registry, state
                // entry gone, session copy still live). Enforcement MUST
                // still happen — the torrent is live and burning — and
                // discarding here turned detection into a permanent
                // no-op. Enforce with a best-effort identity: no state
                // entry means no history event (no size/seeder facts),
                // and ERROR-level visibility since status can't show it.
                let prev = self.state.quarantine_get(&hex);
                let title = prev
                    .as_ref()
                    .map(|q| q.title.clone())
                    .unwrap_or_else(|| format!("untracked {hex}"));
                let q = crate::engine::quarantine::next_quarantine_entry(
                    prev.as_ref(),
                    title.clone(),
                    |attempts| {
                        format!(
                            "discarded {} across {} zero-progress pass{} (attempt {attempts}, untracked ghost)",
                            crate::humanize::human_bytes(trip.wasted_bytes as i64),
                            trip.windows,
                            if trip.windows == 1 { "" } else { "es" },
                        )
                    },
                    trip,
                    now,
                    cooldown_chrono,
                    max_retries,
                );
                let attempts = q.attempts;
                let reason = q.reason.clone();
                let cooldown_until = q.cooldown_until;
                tracing::error!(
                    "quarantined UNTRACKED torrent {} ({}): {}; removing and gating until {} (attempt {}, no state entry: history and status cannot track this)",
                    title,
                    hex,
                    reason,
                    cooldown_until.to_rfc3339(),
                    attempts
                );
                // rqbit's delete(true) removes the data itself; the
                // keep-at dir cleanup is skipped (location unknown).
                if let Err(e) =
                    engtorrents::remove_torrent(&self.session, &hex, std::path::Path::new("")).await
                {
                    tracing::error!("failed to remove untracked quarantined torrent {hex}: {e:#}");
                }
                let _ = std::fs::remove_file(self.torrent_cache_path(&hex));
                if let Err(e) = self.state.quarantine_put(hex.clone(), q) {
                    tracing::error!("failed to persist quarantine for {}: {e:#}", hex);
                }
                self.detector.forget(&hex);
                continue;
            };
            let attempts_prev = self.state.quarantine_get(&hex);
            let q = crate::engine::quarantine::next_quarantine_entry(
                attempts_prev.as_ref(),
                held.title.clone(),
                |attempts| {
                    format!(
                        "discarded {} across {} zero-progress pass{} (attempt {attempts})",
                        crate::humanize::human_bytes(trip.wasted_bytes as i64),
                        trip.windows,
                        if trip.windows == 1 { "" } else { "es" },
                    )
                },
                trip,
                now,
                cooldown_chrono,
                max_retries,
            );
            let attempts = q.attempts;
            let reason = q.reason.clone();
            let cooldown_until = q.cooldown_until;
            tracing::warn!(
                "quarantined {} ({}): {}; removed and re-probed after {} (cooldown until {})",
                held.title,
                hex,
                reason,
                crate::humanize::human_duration(self.cfg.scan.quarantine_cooldown),
                cooldown_until.to_rfc3339(),
            );
            let out_dir = engtorrents::torrent_output_dir(&held.storage_location, &hex);
            if let Err(e) = engtorrents::remove_torrent(&self.session, &hex, &out_dir).await {
                tracing::error!("failed to remove quarantined torrent {}: {e:#}", held.title);
            }
            let _ = std::fs::remove_file(self.torrent_cache_path(&hex));
            let q = state::Quarantine {
                title: held.title.clone(),
                reason: reason.clone(),
                quarantined_at: now,
                cooldown_until,
                attempts,
                wasted_bytes: trip.wasted_bytes,
            };
            // ONE atomic save replaces the old put-then-remove pair (kill
            // window between them used to persist the hash into BOTH
            // maps — a zombie that could never re-enter the session and,
            // with escalation, stayed gated for the indefinite cooldown).
            if let Err(e) = self.state.quarantine_and_remove(hex.clone(), q) {
                tracing::error!("failed to persist quarantine for {}: {e:#}", hex);
                // The in-memory mutation already happened; only the disk
                // lagged. Skip history — the ledger must not claim a drop
                // the authoritative file doesn't reflect yet.
                self.detector.forget(&hex);
                continue;
            }
            self.history.record(&history::remove_event(
                &hex,
                &held.title,
                held.size_bytes,
                held.last_known_seeders,
                &held.storage_location,
                history::Cause::Quarantined,
                &reason,
            ));
            self.detector.forget(&hex);
        }
        self.push_live();
    }

    // ---- one scan ----

    pub async fn scan_once(&mut self) -> Result<()> {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        self.scan_once_shutdown(rx).await
    }

    /// scan_once with an explicit shutdown signal. Long phases (held refresh,
    /// evaluation, acting) check it and abort promptly on SIGTERM/SIGINT.
    pub async fn scan_once_shutdown(
        &mut self,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let scan_started_at = Utc::now();
        let seeder_floor = netstats::load_snapshot(&self.network_stats_path())
            .map(|s| s.seeder_floor)
            .unwrap_or(0);
        netstats::save_snapshot(
            &self.network_stats_path(),
            &Snapshot {
                scan_started_at: Some(scan_started_at),
                scan_completed_at: None,
                total_candidates: 0,
                processed_candidates: 0,
                seeder_floor,
            },
        )?;

        let (catalog, _fresh) = self.catalog.load(self.cfg.scan.interval).await?;
        tracing::info!("catalog loaded (items={})", catalog.items.len());

        let mut items = catalog.items;
        shuffle(&mut items);

        let catalog_hashes: HashSet<String> =
            items.iter().map(|i| hex::encode(i.info_hash)).collect();
        // Remember the catalog set for the watchdog's registry GC: an
        // orphaned quarantine entry may only be dropped when its hash is
        // absent from this set (see quarantine_pass).
        self.last_catalog = Some(catalog_hashes.clone());
        let held = self.state.all();
        let held_hashes: HashSet<String> = held.iter().map(|h| h.info_hash.clone()).collect();

        // Catalog collapse guard: a fresh fetch that parses but lists far
        // fewer of our HELD torrents than we hold is a parse/schema accident
        // (e.g. AT renaming the infohash field silently skips every row, or
        // a well-formed feed of wrong rows), not a mass deletion - and the
        // eviction pass below deletes downloaded data along with each state
        // entry. Refuse it unless the fresh catalog still lists at least
        // catalog_collapse_percent% of the held set (default 30; 0 disables,
        // 100 is strictest). The check is the held CATALOG OVERLAP, not the
        // catalog's total item count: a same-volume catalog sharing zero
        // hashes with our holdings is exactly the accident this guard exists
        // for, and a volume check cannot see it. If a collapse is real,
        // raise the percent or set preserve_deleted_torrents - see
        // docs/RECOVERY.md for recovery after a wipe.
        let still_listed = catalog_hashes.intersection(&held_hashes).count();
        let collapse = !held.is_empty()
            && self.cfg.catalog_collapse_percent > 0
            && still_listed
                < (held.len() * self.cfg.catalog_collapse_percent as usize / 100).max(1);
        if collapse {
            tracing::warn!(
                "catalog collapse guard: fresh catalog still lists {} of {} held torrents \
                 (catalog_collapse_percent={}) - skipping removed-from-catalog eviction this scan; \
                 if the collapse is real, raise --catalog-collapse-percent (0 disables the guard) \
                 or set preserve_deleted_torrents - see docs/RECOVERY.md",
                still_listed,
                held.len(),
                self.cfg.catalog_collapse_percent,
            );
        } else {
            self.remove_deleted_torrents(&held, &catalog_hashes).await;
        }
        // Confirm the survivors: every held torrent the fresh catalog still
        // lists gets its vanished-eviction clock reset. Only unlisted ones
        // keep counting toward the deleted-torrent grace window. Batched
        // into one state write: this runs over the whole held set every
        // scan, and a per-torrent `State::update` rewrote + fsynced the
        // full state.json once per survivor.
        let now = Utc::now();
        let confirmations: Vec<(String, crate::state::TorrentUpdate)> = self
            .state
            .all()
            .into_iter()
            .filter(|h| {
                catalog_hashes.contains(&h.info_hash)
                    && h.last_confirmed_in_catalog_at
                        .map(|t| t < now)
                        .unwrap_or(true)
            })
            .map(|h| {
                (
                    h.info_hash.clone(),
                    Box::new(move |t: &mut state::Torrent| {
                        t.last_confirmed_in_catalog_at = Some(now);
                    }) as crate::state::TorrentUpdate,
                )
            })
            .collect();
        let confirmed_any = self.state.update_each(confirmations).unwrap_or(0) > 0;
        if confirmed_any {
            self.push_live();
        }
        self.refresh_held_seeder_counts(&shutdown).await;
        if *shutdown.borrow() {
            anyhow::bail!("scan interrupted by shutdown");
        }
        self.evict_stalled_torrents(&catalog_hashes).await;

        let max_fittable = self.max_fittable_size();
        // Same predicate as `total_candidates` below, so the two figures in
        // the "disqualified N oversized" / "total=M" log lines can't
        // contradict each other. (This loop used to check held + size only
        // and so counted blocklisted+oversized items that `count_pending`
        // excluded.)
        let mut too_big = 0u64;
        for item in &items {
            if prefilter_reason(item, &held_hashes, &self.blocklist, max_fittable)
                == Prefilter::TooBig
            {
                too_big += 1;
            }
        }
        let total_candidates = count_pending(&items, &held_hashes, &self.blocklist, max_fittable);
        netstats::save_snapshot(
            &self.network_stats_path(),
            &Snapshot {
                scan_started_at: Some(scan_started_at),
                scan_completed_at: None,
                total_candidates,
                processed_candidates: 0,
                seeder_floor,
            },
        )?;
        if too_big > 0 {
            tracing::info!(
                "disqualified {too_big} oversized candidates before scrape (max fittable {})",
                crate::humanize::human_bytes(max_fittable as i64)
            );
        }
        tracing::info!("starting scrape: fetching torrent metadata and tracker data for every pending catalog candidate ({total_candidates} total)");

        // Shared with the spawned evaluation task (all-atomics: Sync).
        let stats = Arc::new(ScanStats::default());
        let mut library_bytes = 0u64;
        for item in &items {
            library_bytes += item.size_bytes;
        }
        stats.library_bytes.store(library_bytes, Ordering::Relaxed);

        let scrape_started = std::time::Instant::now();
        // TRUE incremental acting: evaluation streams results over a channel
        // as each completes, and this loop acts on the top window every 16
        // arrivals - the most urgent torrents start seeding within minutes,
        // not after the whole catalog is walked.
        //
        // evaluate_candidates only needs shared (&self) state - internally
        // everything crossing spawn boundaries is Arc-owned (EvalCtx,
        // swarm_cache Mutex, rate Mutex) - but its future still borrows
        // &self, which conflicts with acting's &mut self. Resolve by
        // snapshotting the shared inputs into an owned dispatch bundle and
        // running evaluation as a free function on a spawned task; the
        // consume loop below keeps &mut self free for acting.
        let mut acted: HashSet<String> = HashSet::new();
        let mut held_count = self.state.all().len();
        let mut batch: Vec<Evaluated> = Vec::new();
        let mut processed = 0u64;
        let (emit_tx, mut emit_rx) = tokio::sync::mpsc::unbounded_channel::<Evaluated>();
        // Fresh floor accumulator for this scan. evaluated_counts is a
        // tokio Mutex (not Clone): wrap in an Arc for the consume loop.
        *self.evaluated_counts.lock().await = Vec::new();
        let floor_accum = Arc::new(tokio::sync::Mutex::new(Vec::<u32>::new()));
        let dispatch = self.evaluation_dispatch(
            &shutdown,
            items,
            held_hashes,
            scan_started_at,
            total_candidates,
            max_fittable,
        );
        let eval_stats = stats.clone();
        let mut eval_handle = tokio::spawn(async move {
            run_evaluation(dispatch, emit_tx, eval_stats).await;
        });
        let mut eval_done = false;
        loop {
            tokio::select! {
                biased;
                r = &mut eval_handle, if !eval_done => {
                    // Join the dispatch task (propagates panics); the walk
                    // is over, but drained results may still be queued.
                    let _ = r;
                    eval_done = true;
                }
                ev = emit_rx.recv() => {
                    match ev {
                        Some(ev) => {
                            // Floor accumulator (was previously filled from
                            // the buffered vec at the end).
                            floor_accum.lock().await.push(ev.seeders);
                            if *shutdown.borrow() {
                                anyhow::bail!("scan interrupted by shutdown");
                            }
                            batch.push(ev);
                            processed += 1;
                            if batch.len().is_multiple_of(EVALUATE_CONCURRENCY) {
                                let ram_bound = held_count >= self.max_torrents;
                                self.act_on_windowed(
                                    &batch,
                                    &mut acted,
                                    &mut held_count,
                                    ram_bound,
                                    seeder_floor,
                                    &stats,
                                )
                                .await;
                            }
                        }
                        None => {
                            // All senders dropped and channel drained. If the
                            // evaluation task also finished, the walk is over.
                            if eval_done {
                                break;
                            }
                            // Channel drained but dispatch still running
                            // (senders alive, nothing queued): briefly yield
                            // rather than busy-loop.
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                }
            }
            if eval_done {
                // Drain anything that arrived after the last recv.
                while let Ok(ev) = emit_rx.try_recv() {
                    floor_accum.lock().await.push(ev.seeders);
                    batch.push(ev);
                    processed += 1;
                    if batch.len().is_multiple_of(EVALUATE_CONCURRENCY) {
                        let ram_bound = held_count >= self.max_torrents;
                        self.act_on_windowed(
                            &batch,
                            &mut acted,
                            &mut held_count,
                            ram_bound,
                            seeder_floor,
                            &stats,
                        )
                        .await;
                    }
                }
                break;
            }
        }
        if *shutdown.borrow() {
            anyhow::bail!("scan interrupted by shutdown");
        }
        if !processed.is_multiple_of(EVALUATE_CONCURRENCY as u64) {
            let ram_bound = held_count >= self.max_torrents;
            self.act_on_windowed(
                &batch,
                &mut acted,
                &mut held_count,
                ram_bound,
                seeder_floor,
                &stats,
            )
            .await;
        }

        let counts: Vec<u32> = floor_accum.lock().await.clone();
        *self.evaluated_counts.lock().await = counts.clone();
        // The floor anchors the NEXT scan's seed-scarcity gate. A scan that
        // evaluated nothing new (everything already held, all skipped) has
        // no fresh seeder data: keep the persisted floor instead of
        // clobbering it with 0, which would silently turn the next scan's
        // gate conservative (floor 0 behaves as floor 1).
        let floor = if counts.is_empty() {
            seeder_floor
        } else {
            selector::seeder_floor(&counts)
        };
        tracing::info!(
            "scrape complete: available={} processed={} total={} elapsed={} library={} fetched={} cached-meta={} scrapes={} cached-scrapes={} eligible={} seeder-floor={}",
            batch.len(), processed, total_candidates,
            crate::humanize::human_duration(scrape_started.elapsed()),
            crate::humanize::human_bytes(library_bytes as i64),
            stats.metadata_fetched.load(Ordering::Relaxed),
            stats.metadata_cached.load(Ordering::Relaxed),
            stats.scrape_requests.load(Ordering::Relaxed),
            stats.scrape_cached.load(Ordering::Relaxed),
            stats.eligible.load(Ordering::Relaxed),
            floor,
        );
        netstats::save_snapshot(
            &self.network_stats_path(),
            &Snapshot {
                scan_started_at: Some(scan_started_at),
                scan_completed_at: Some(Utc::now()),
                total_candidates,
                processed_candidates: processed,
                seeder_floor: floor,
            },
        )?;
        *self.last_scan.lock().await = Some(LastScanStats {
            eligible: stats.eligible.load(Ordering::Relaxed),
            scrape_requests: stats.scrape_requests.load(Ordering::Relaxed),
            scrape_cached: stats.scrape_cached.load(Ordering::Relaxed),
            processed,
            total: total_candidates,
        });
        if let Ok(cache) = self.swarm_cache.lock() {
            cache.save()?;
        }
        Ok(())
    }

    /// Counters from the most recent scan (None before the first scan).
    pub async fn last_scan_stats(&self) -> Option<LastScanStats> {
        self.last_scan.lock().await.clone()
    }

    /// Held torrents snapshot (for tests / status paths without an Engine clone).
    pub fn held_torrents(&self) -> Vec<crate::state::Torrent> {
        self.state.all()
    }

    /// Live-query push handle over this Engine's real session + held set.
    /// Construction seam `run` uses internally; public so integration tests
    /// can serve a real socket without starting the main loop.
    pub fn test_live_handle(&self) -> crate::live::LiveHandle {
        let storage: Vec<(PathBuf, u64)> = self
            .cfg
            .storage
            .iter()
            .map(|l| (l.path.clone(), l.limit_bytes()))
            .collect();
        let snap = snapshot_entries(&self.state);
        crate::live::LiveHandle::for_tests(
            self.session.clone(),
            self.api.clone(),
            snap,
            self.started_at,
            storage,
        )
    }

    // ---- evaluation ----

    /// Evaluate every pending candidate concurrently; returns lightweight results.
    #[allow(clippy::too_many_arguments)]
    /// Evaluate every pending candidate concurrently, STREAMING results:
    /// each completed evaluation is sent over `emit` immediately, so the
    /// caller acts on the top window as results arrive instead of waiting
    /// for the whole walk. Returns the count of eligible (sent) candidates;
    /// seeder counts for the floor are accumulated on the Engine as they
    /// arrive (see evaluated_counts).
    /// Snapshot the shared inputs evaluation needs into an owned dispatch
    /// bundle, so the evaluation task runs without borrowing &self while
    /// the consume loop acts with &mut self.
    fn evaluation_dispatch(
        &self,
        shutdown: &tokio::sync::watch::Receiver<bool>,
        items: Vec<atcatalog::Item>,
        held_hashes: HashSet<String>,
        scan_started_at: DateTime<Utc>,
        total_candidates: u64,
        max_fittable: u64,
    ) -> EvaluationDispatch {
        EvaluationDispatch {
            ctx: Arc::new(EvalCtx {
                data_dir: self.cfg.data_dir.clone(),
                moderation_bypass: self.cfg.scan.moderation_delay,
                http: self.http.clone(),
                torrent_base_url: self.torrent_fetcher.base_url.clone(),
                user_agent: buildinfo::user_agent(),
                rate: self.rate.clone(),
                shutdown: shutdown.clone(),
                scrape_backoff: self.scrape_backoff,
            }),
            shutdown: shutdown.clone(),
            items,
            held_hashes,
            scan_started_at,
            total_candidates,
            max_fittable,
            blocklist: self.blocklist.clone(),
            swarm_cache: self.swarm_cache.clone(),
            network_stats_path: self.network_stats_path(),
        }
    }

    /// Fetch metadata from torrent-cache, else AT (rate-limited), caching to disk.
    /// Returns the lightweight TorrentMeta only; raw bytes stay on disk until add.
    async fn fetch_metadata(&self, info_hash_hex: &str) -> Result<TorrentMeta> {
        let path = self.torrent_cache_path(info_hash_hex);
        if let Ok(data) = std::fs::read(&path) {
            if let Ok(md) = attorrent::parse_torrent_bytes(&data) {
                return Ok(md);
            }
        }
        self.rate.lock().await.wait().await;
        let (md, _raw) = self
            .torrent_fetcher
            .fetch_torrent(info_hash_hex, Some(&path))
            .await?;
        Ok(md)
    }

    /// Scrape trackers in order (cached first); AT hosts go through the
    /// shared rate limiter. Thin adapter over the one shared
    /// [`scrape_swarm_cached`] the evaluation workers use, so the 429 policy
    /// cannot drift between the two callers.
    async fn scrape_swarm(
        &self,
        shutdown: &tokio::sync::watch::Receiver<bool>,
        trackers: &[String],
        info_hash: &[u8; 20],
        stats: &ScanStats,
    ) -> Result<SwarmCounts> {
        let shared = ScrapeShared {
            cache: &self.swarm_cache,
            rate: &self.rate,
            http: &self.http,
            shutdown,
            scrape_backoff: self.scrape_backoff,
        };
        scrape_swarm_cached(&shared, trackers, info_hash, stats).await
    }

    // ---- acting ----

    fn max_fittable_size(&self) -> u64 {
        self.cfg
            .storage
            .iter()
            .map(|l| l.limit_bytes())
            .max()
            .unwrap_or(0)
    }

    fn free_bytes(&self, loc: &crate::config::StorageLocation) -> u64 {
        // Nominal-plus-buffer accounting: price what's HELD (state nominal +
        // per-torrent buffer), not what's on disk. Downloads land sparse, so
        // on-disk actuals lag nominal by orders of magnitude early on; pricing
        // actuals would let the node over-commit the location many times over
        // (observed live: 4.17 TB nominal held against a 3.8 TB limit while
        // actual disk sat at 277 GB). A partially-downloaded torrent reserves
        // its full eventual footprint.
        let held_nominal = self.state.bytes_used(&loc.path);
        let held_buffers = self
            .state
            .all()
            .iter()
            .filter(|t| t.storage_location == loc.path)
            .count() as u64
            * PER_TORRENT_SIZE_BUFFER;
        let mut free = loc
            .limit_bytes()
            .saturating_sub(held_nominal.saturating_add(held_buffers));
        if let Ok(dev_free) = storage::device_free_bytes(&loc.path) {
            free = free.min(dev_free);
        }
        free
    }

    /// Nominal size + small fixed buffer (cached .torrent + state overhead).
    /// Plain on-disk accounting: no compression backend by design.
    fn size_needed(&self, nominal: u64) -> u64 {
        nominal.saturating_add(PER_TORRENT_SIZE_BUFFER)
    }

    async fn act_on_windowed(
        &mut self,
        evaluated: &[Evaluated],
        acted: &mut HashSet<String>,
        held_count: &mut usize,
        _ram_bound: bool,
        seeder_floor: u32,
        stats: &ScanStats,
    ) {
        {
            let mut sel: Vec<Candidate> = evaluated
                .iter()
                .map(|e| Candidate {
                    info_hash: e.info_hash,
                    title: e.title.clone(),
                    size_bytes: e.size_bytes,
                    piece_count: e.piece_count,
                    seeders: e.seeders,
                    leechers: e.leechers,
                    seeder_floor,
                })
                .collect();
            sel = selector::rank_candidates(
                sel,
                self.size_bias,
                ram::peer_limit_for_budget(self.ram_budget),
            );
            let window = self.max_torrents.min(sel.len());
            for c in sel.into_iter().take(window) {
                let key = hex::encode(c.info_hash);
                if acted.contains(&key) {
                    continue;
                }
                acted.insert(key);
                self.act_on_candidate(&c, held_count, seeder_floor, stats)
                    .await;
            }
        }
    }

    async fn act_on_candidate(
        &mut self,
        c: &Candidate,
        held_count: &mut usize,
        seeder_floor: u32,
        _stats: &ScanStats,
    ) {
        let hex = hex::encode(c.info_hash);
        // Quarantine gate: a hash under an active cooldown is not selectable
        // (free-space adds included — this is the single choke point for
        // both paths). A LAPSED entry passes through unchanged: the
        // re-probe runs like any candidate, and the entry stays in the
        // registry so its attempts count survives for max_retries
        // escalation. Only the watchdog lifts an entry — once the
        // re-probe COMPLETES against the registered hashes (upstream
        // fixed the data); if the swarm is still broken, it re-quarantines
        // after min_windows zero-progress passes.
        let (active_quarantine, quarantine_probe) = self.quarantine_gate(&hex);
        if let Some(q) = active_quarantine {
            tracing::debug!(
                "skipping quarantined candidate {} (attempt {}, until {}): {}",
                c.title,
                q.attempts,
                q.cooldown_until.to_rfc3339(),
                q.reason
            );
            return;
        }
        // A lapsed cooldown (quarantine_probe) re-probes the hash. The
        // probe bypasses the seed-scarcity roll: a broken-but-well-seeded
        // swarm (the NotaBug shape has live seeders by definition) would
        // otherwise face chance = aggressiveness^(seeders - floor) ≈ 0
        // and never actually re-enter the session — the lift could never
        // happen and the entry would sit lapsed forever while logs
        // claimed "re-eligible" every scan. The bypass is bounded twice
        // over: probes fill FREE SPACE only (never displace a healthy
        // held torrent — see below), and max_retries escalation locks
        // repeat offenders into indefinite cooldown.
        // Re-read metadata from cache (never kept in memory during evaluation).
        let md = match self.fetch_metadata(&hex).await {
            Ok(md) => md,
            Err(e) => {
                tracing::warn!("could not reload cached metadata for {}: {e:#}", c.title);
                return;
            }
        };
        let size_bytes = md.total_length;
        // Refresh the candidate's RAM price from authoritative metainfo: the
        // Evaluated snapshot may predate piece_count tracking (always set for
        // fresh evaluations, but cheap to re-derive here).
        let mut c = c.clone();
        c.piece_count = md.piece_count;

        // FD-aware admission: a many-file candidate that would push the
        // process past RLIMIT_NOFILE is skipped here — before opening
        // anything, before rolling, before touching state — so fd pressure
        // is orderly selection (this candidate waits for a roomier scan),
        // not a hard "Too many open files" add failure. The guard reads the
        // live headroom per candidate (cheap: one getrlimit + one /proc
        // readdir); unknown headroom admits.
        //
        // Pricing note: it charges one fd per file, which was exact when
        // storage held every file open for the torrent's lifetime and is
        // now an upper bound — `engine::pool_storage` serves IO through a
        // bounded LRU handle pool, so a many-file torrent typically uses far
        // fewer live fds than its file count. Deliberately conservative:
        // refusing a candidate costs coverage, whereas pricing too low
        // costs a hard add failure. See `fdlimit`'s module docs.
        if !crate::fdlimit::fits_in_headroom(md.file_count as u64, crate::fdlimit::fd_headroom()) {
            tracing::info!(
                "skipping candidate: needs {} file fds but only {} free (title={})",
                md.file_count,
                crate::fdlimit::fd_headroom().unwrap_or(u64::MAX),
                c.title
            );
            return;
        }

        let mut decision = selector::SwapDecision {
            should_swap: false,
            chance: 0.0,
            roll: 0.0,
            reason: "candidate not evaluated".to_string(),
        };

        // ONE seed-scarcity roll per evaluation, drawn before any path split
        // and shared by the fill and the swap path alike (DESIGN.md: "every
        // candidate in every scan gets its own independent chance"). Drawing
        // it per placement attempt would hand a candidate k independent rolls
        // across k storage locations (P(admit) = 1-(1-p)^k) - and would do so
        // on exactly the paths that skip the fill attempt entirely (torrent
        // cap reached, no free space, RAM headroom exhausted), where the old
        // per-try_add draw meant no roll had been consumed yet. A quarantine
        // re-probe bypasses the roll (roll < 0 beats any chance >= 0): the
        // probe bypass is bounded twice over - free space only (never
        // displaces, see below) and max_retries escalation.
        let mut rng = rand::thread_rng();
        let roll = if quarantine_probe {
            -1.0
        } else {
            selector::roll(&mut rng)
        };

        if *held_count < self.max_torrents {
            // Free-space fill still checks the RAM price: even below the
            // torrent-count cap, a candidate must fit the *remaining* RAM
            // budget, so a many-piece giant can't push RSS past the budget on
            // a host whose disk dwarfs its RAM (e.g. 512 MiB / 1 TB).
            let ram_headroom = self.ram_headroom_bytes();
            let ram_cost =
                ram::torrent_ram(c.piece_count, ram::peer_limit_for_budget(self.ram_budget));
            if ram_cost > ram_headroom {
                tracing::debug!(
                    "skipping free-space fill: candidate RAM cost {} exceeds headroom {} (title={})",
                    crate::humanize::human_bytes(ram_cost as i64),
                    crate::humanize::human_bytes(ram_headroom as i64),
                    c.title
                );
            } else {
                let free_space: Vec<u64> = self
                    .cfg
                    .storage
                    .iter()
                    .map(|l| self.free_bytes(l))
                    .collect();
                let space_needed: Vec<u64> = self
                    .cfg
                    .storage
                    .iter()
                    .map(|_| self.size_needed(size_bytes))
                    .collect();
                if let Some(idx) =
                    selector::choose_location(&free_space, &space_needed, selector::roll(&mut rng))
                {
                    let loc = self.cfg.storage[idx].path.clone();
                    let (added, d) = self
                        .try_add(&c, &md, size_bytes, &loc, &[], seeder_floor, roll)
                        .await;
                    decision = d;
                    if added {
                        *held_count += 1;
                        self.log_decision(&c, &decision);
                        return;
                    }
                }
            }
        } else {
            tracing::debug!(
                "skipping free-space fill: at RAM-driven torrent cap (title={})",
                c.title
            );
        }

        // DESIGN.md: "candidates that lose the [seed-scarcity] roll are
        // skipped entirely." The roll above is shared by both placement
        // paths, so a roll-rejected candidate is rejected everywhere - this
        // early return is the fast path for that, not the mechanism. Without
        // it a roll-rejected candidate would still walk the swap path and
        // displace strictly better-seeded held torrents while free space is
        // plentiful - swap exists for space/RAM pressure, gated by margin,
        // not as a scarcity-roll retry.
        if decision.seed_scarcity_blocked() {
            return;
        }

        // Re-probes fill FREE SPACE only. A probe is speculative — it is
        // expected to fail and re-quarantine — and try_swap DELETES the
        // displaced torrent's data, so letting a probe displace would trade
        // a healthy, better-seeded holding for a swarm about to be removed
        // again: one healthy torrent destroyed per cooldown, forever under
        // the unlimited (0) max_retries default. A full node's probe simply
        // waits for space to free up; the gate keeps blocking the hash in
        // the meantime and the registry entry (with its attempts count)
        // survives until the probe runs (or the hash delists).
        if quarantine_probe {
            tracing::debug!(
                "quarantine re-probe deferred: no free space (probes never displace held torrents) (title={})",
                c.title
            );
            return;
        }

        let (swapped, d, displaced_removed) = self
            .try_swap(
                &c,
                &md,
                size_bytes,
                *held_count >= self.max_torrents,
                seeder_floor,
                roll,
            )
            .await;
        if swapped {
            // A swap is 1-in / N-out. Keeping held_count in step with the
            // real set matters within the scan: it gates the free-space
            // fill path (`held_count < max_torrents`), so a drift upward
            // would make later candidates skip fill and swap needlessly
            // until the next scan re-derives the count from state.
            *held_count = held_count
                .saturating_add(1)
                .saturating_sub(displaced_removed);
        }
        if swapped || (!decision.seed_scarcity_blocked() && !d.reason.is_empty()) {
            self.log_decision(&c, &d);
        }
    }

    /// Remaining RAM budget in bytes given the current held set, priced
    /// with each held torrent's stored piece count (unknown counts price as
    /// typical). Saturates at 0 rather than going negative.
    fn ram_headroom_bytes(&self) -> u64 {
        let peer_limit = ram::peer_limit_for_budget(self.ram_budget);
        let mut used = 0u64;
        for h in self.state.all() {
            let pieces = if h.piece_count > 0 {
                h.piece_count
            } else {
                1024
            };
            used = used.saturating_add(ram::torrent_ram(pieces, peer_limit));
        }
        self.ram_budget.saturating_sub(used)
    }

    /// Quarantine lookup with lazy re-probe handling. Returns
    /// `(Some(active_entry), false)` when the hash must be skipped,
    /// `(None, true)` when the cooldown has LAPSED (the hash is re-eligible
    /// as a probe; the entry stays in the registry so its attempts count
    /// survives for max_retries escalation), `(None, false)` when never
    /// quarantined. The entry is lifted later, by `quarantine_pass`, once
    /// the re-probe completes against the registered hashes (upstream
    /// fixed the data).
    fn quarantine_gate(&mut self, info_hash_hex: &str) -> (Option<state::Quarantine>, bool) {
        let Some(q) = self.state.quarantine_get(info_hash_hex) else {
            return (None, false);
        };
        if Utc::now() >= q.cooldown_until {
            tracing::info!(
                "quarantine re-probe for {} ({}): cooldown lapsed after {} attempt(s); hash re-eligible",
                q.title,
                info_hash_hex,
                q.attempts
            );
            return (None, true);
        }
        (Some(q), false)
    }

    fn log_decision(&self, c: &Candidate, d: &selector::SwapDecision) {
        if d.seed_scarcity_blocked() {
            return; // routine outcome for well-seeded candidates: not logged.
        }
        tracing::info!(
            "evaluated candidate (title={} seeders={} should_swap={} reason={})",
            c.title,
            c.seeders,
            d.should_swap,
            d.reason
        );
    }

    #[allow(clippy::too_many_arguments)]
    async fn try_add(
        &mut self,
        c: &Candidate,
        md: &TorrentMeta,
        size_bytes: u64,
        location: &Path,
        displaced: &[Held],
        seeder_floor: u32,
        roll: f64,
    ) -> (bool, selector::SwapDecision) {
        // `roll` is drawn ONCE per evaluation by act_on_candidate and shared
        // by every placement attempt for this candidate, so a candidate can
        // never earn a second chance by being retried against another
        // location. A quarantine re-probe passes roll = -1.0 (beats any
        // chance >= 0): see act_on_candidate. The availability and margin
        // vetoes still apply — a probe with no live seed simply waits for
        // the swarm to gain one.
        let decision = selector::evaluate_swap(
            &selector::Candidate {
                info_hash: c.info_hash,
                title: c.title.clone(),
                size_bytes,
                piece_count: md.piece_count,
                seeders: c.seeders,
                leechers: c.leechers,
                seeder_floor,
            },
            displaced,
            self.cfg.scan.min_seed_margin,
            self.cfg.aggressiveness,
            roll,
        );
        if !decision.should_swap {
            tracing::debug!(
                "roll failed (title={} seeders={} chance={:.3} roll={:.3} reason={})",
                c.title,
                c.seeders,
                decision.chance,
                decision.roll,
                decision.reason
            );
            return (false, decision);
        }
        let out_dir = engtorrents::torrent_output_dir(location, &hex::encode(c.info_hash));
        if let Err(e) = std::fs::create_dir_all(&out_dir) {
            tracing::error!("failed to create output dir {}: {e:#}", out_dir.display());
            return (false, decision);
        }
        let tiers = tiers_of(&md.trackers);
        let keyed = atkey::at_trackers_only(tiers, &self.user_announce, &self.user_announce_ipv6);
        let hex_str = hex::encode(c.info_hash);
        if let Err(e) = engtorrents::add_torrent_bytes(
            &self.session,
            &hex_str,
            &out_dir,
            keyed,
            &self.torrent_cache_path(&hex_str),
            true,
        )
        .await
        {
            tracing::error!("failed to add candidate {}: {e:#}", c.title);
            return (false, decision);
        }
        if let Err(e) = self.state.put(state::Torrent {
            info_hash: hex::encode(c.info_hash),
            title: c.title.clone(),
            size_bytes,
            storage_location: location.to_path_buf(),
            added_at: Utc::now(),
            piece_count: md.piece_count,
            last_known_seeders: c.seeders,
            completed_pieces: 0,
            last_progress_at: Some(Utc::now()),
            last_confirmed_in_catalog_at: Some(Utc::now()),
        }) {
            tracing::error!("failed to persist state for {}: {e:#}", c.title);
        }
        let cause = if displaced.is_empty() {
            history::Cause::Fill
        } else {
            history::Cause::Swap
        };
        self.history.record(&history::add_event(
            &hex_str,
            &c.title,
            size_bytes,
            c.seeders,
            location,
            cause,
            decision.chance,
            decision.roll,
            seeder_floor,
            &decision.reason,
            displaced
                .iter()
                .map(|d| history::Displaced {
                    hash: hex::encode(d.info_hash),
                    title: d.title.clone(),
                    seeders: d.seeders,
                    size_bytes: d.size_bytes,
                })
                .collect(),
        ));
        self.push_live();
        (true, decision)
    }

    /// Swap in `c` at the best location that frees enough disk AND RAM
    /// (see `select_displaceable`), removing the displaced set after a
    /// successful add. One location attempt only (first that fits).
    ///
    /// Returns `(added, decision, displaced_removed)`: the last element is
    /// how many held torrents actually left state, so the caller can keep
    /// its running `held_count` honest (a swap is 1-in / N-out).
    async fn try_swap(
        &mut self,
        c: &Candidate,
        md: &TorrentMeta,
        size_bytes: u64,
        _ram_bound: bool,
        seeder_floor: u32,
        roll: f64,
    ) -> (bool, selector::SwapDecision, usize) {
        let held = self.state.all();
        let peer_limit = ram::peer_limit_for_budget(self.ram_budget);
        let ram_cost = ram::torrent_ram(md.piece_count, peer_limit);
        let mut by_location: HashMap<PathBuf, Vec<state::Torrent>> = HashMap::new();
        for h in held {
            by_location
                .entry(h.storage_location.clone())
                .or_default()
                .push(h);
        }
        let mut last = selector::SwapDecision {
            should_swap: false,
            chance: 0.0,
            roll: 0.0,
            reason: "no held torrent clears the seed margin against this candidate".to_string(),
        };
        // Deterministic location order.
        let mut locations: Vec<PathBuf> = by_location.keys().cloned().collect();
        locations.sort();
        tracing::debug!(
            "swap considered (title={} seeders={} locations={} held_total={})",
            c.title,
            c.seeders,
            locations.len(),
            by_location.values().map(|v| v.len()).sum::<usize>(),
        );
        for location in locations {
            let in_location = &by_location[&location];
            let size_needed = self.size_needed(size_bytes);
            let displaced = select_displaceable(
                in_location,
                c.seeders,
                size_needed,
                ram_cost,
                peer_limit,
                self.cfg.scan.min_seed_margin,
                self.size_bias,
            );
            let Some(displaced) = displaced else { continue };
            // Device-level gate: displacement frees only what the displaced
            // torrents ACTUALLY occupy on disk (a sparse held torrent frees
            // ~nothing), while the candidate writes its full footprint. The
            // nominal checks above know nothing about the device; without
            // this gate a full device admits a swap candidate that provably
            // cannot finish (its writes hit ENOSPC, which fatals the
            // torrent) - trading a live torrent for one that can't seed.
            let freed_actual: u64 = displaced
                .iter()
                .map(|h| {
                    storage::dir_size_bytes(&engtorrents::torrent_output_dir(
                        &h.storage_location,
                        &h.info_hash,
                    ))
                })
                .sum();
            let size_needed = self.size_needed(size_bytes);
            if let Some(reason) = device_gate_reason(
                storage::device_free_bytes(&location).ok(),
                freed_actual,
                size_needed,
            ) {
                tracing::warn!("swap skipped: {} (title={})", reason, c.title,);
                last = selector::SwapDecision {
                    should_swap: false,
                    chance: 0.0,
                    roll: 0.0,
                    reason,
                };
                continue;
            }
            let sel_held: Vec<Held> = displaced
                .iter()
                .map(|h| Held {
                    info_hash: decode_hash(&h.info_hash).unwrap_or([0u8; 20]),
                    title: h.title.clone(),
                    size_bytes: h.size_bytes,
                    piece_count: h.piece_count,
                    seeders: h.last_known_seeders,
                })
                .collect();
            // The seed-scarcity roll is shared with the fill path (one draw
            // per evaluation, threaded in from act_on_candidate), so a
            // candidate cannot earn extra chances by being retried against
            // more locations. Probes never reach here: act_on_candidate
            // routes them to free space only.
            let (ok, decision) = self
                .try_add(c, md, size_bytes, &location, &sel_held, seeder_floor, roll)
                .await;
            if ok {
                let mut removed = 0usize;
                for h in &displaced {
                    tracing::info!(
                        "swapped out displaced torrent (title={} seeders={} size={} for candidate={})",
                        h.title,
                        h.last_known_seeders,
                        crate::humanize::human_bytes(h.size_bytes as i64),
                        c.title,
                    );
                    let out_dir =
                        engtorrents::torrent_output_dir(&h.storage_location, &h.info_hash);
                    if let Err(e) =
                        engtorrents::remove_torrent(&self.session, &h.info_hash, &out_dir).await
                    {
                        tracing::error!("failed to remove displaced torrent {}: {e:#}", h.title);
                    }
                    let _ = std::fs::remove_file(self.torrent_cache_path(&h.info_hash));
                    if let Err(e) = self.state.remove(&h.info_hash) {
                        tracing::error!("failed to drop state for {}: {e:#}", h.title);
                    } else {
                        removed += 1;
                    }
                }
                if removed > 0 {
                    self.push_live();
                }
                return (true, decision, removed);
            } else {
                last = decision;
            }
        }
        (false, last, 0)
    }

    // ---- maintenance ----

    async fn remove_deleted_torrents(
        &mut self,
        held: &[state::Torrent],
        catalog_hashes: &HashSet<String>,
    ) {
        if self.cfg.preserve_deleted_torrents {
            return;
        }
        let timeout = self.cfg.scan.vanished_eviction_timeout;
        let now = Utc::now();
        let mut removed_any = false;
        for h in held {
            if catalog_hashes.contains(&h.info_hash) {
                continue;
            }
            // Grace window: a torrent genuinely removed from AT stays gone,
            // so it evicts once unlisted longer than the timeout. A catalog
            // hiccup lists it again within the window and the confirmation
            // stamp (set after this pass each scan) resets the clock.
            // Timeout 0 = no grace (the pre-grace next-scan behavior).
            // Legacy state without a stamp is priced from add time - always
            // past any real timeout, preserving the old next-scan eviction.
            let anchor = h.last_confirmed_in_catalog_at.unwrap_or(h.added_at);
            let Ok(absent_for) = (now - anchor).to_std() else {
                continue;
            };
            if absent_for < timeout {
                tracing::debug!(
                    "deferring removal of {}: no longer listed on Academic Torrents for {} (grace {})",
                    h.title,
                    crate::humanize::human_duration(absent_for),
                    crate::humanize::human_duration(timeout)
                );
                continue;
            }
            tracing::info!(
                "removing torrent no longer listed on Academic Torrents (title={}, absent for {})",
                h.title,
                crate::humanize::human_duration(absent_for)
            );
            let out_dir = engtorrents::torrent_output_dir(&h.storage_location, &h.info_hash);
            if let Err(e) = engtorrents::remove_torrent(&self.session, &h.info_hash, &out_dir).await
            {
                tracing::error!("failed to remove deleted torrent {}: {e:#}", h.title);
            }
            let _ = std::fs::remove_file(self.torrent_cache_path(&h.info_hash));
            if let Err(e) = self.state.remove(&h.info_hash) {
                tracing::error!("failed to drop state for {}: {e:#}", h.title);
            } else {
                removed_any = true;
                self.history.record(&history::remove_event(
                    &h.info_hash,
                    &h.title,
                    h.size_bytes,
                    h.last_known_seeders,
                    &h.storage_location,
                    history::Cause::DeletedFromCatalog,
                    "no longer listed on Academic Torrents",
                ));
            }
        }
        if removed_any {
            self.push_live();
        }
    }

    async fn refresh_held_seeder_counts(&mut self, shutdown: &tokio::sync::watch::Receiver<bool>) {
        let held = self.state.all();
        let total = held.len();
        // Collect the per-torrent updates during the async walk and apply
        // them in ONE state write at the end: this pass covers the whole
        // held set every scan, and per-entry `State::update` rewrote +
        // fsynced the full state.json once per torrent (up to |held| full
        // rewrites per scan - the exact anti-pattern `update_progress_many`
        // already fixed for the watchdog pass).
        let mut updates: Vec<(String, crate::state::TorrentUpdate)> = Vec::new();
        for (done, h) in held.into_iter().enumerate() {
            if *shutdown.borrow() {
                tracing::info!("held refresh interrupted by shutdown ({done}/{total})");
                break;
            }
            if done % 25 == 0 {
                tracing::info!("refreshing held seeder counts ({done}/{total})");
            }
            let hex = h.info_hash.clone();
            let md = match self.fetch_metadata(&hex).await {
                Ok(md) => md,
                Err(e) => {
                    tracing::warn!("could not refresh metadata for {}: {e:#}", h.title);
                    continue;
                }
            };
            let dummy = ScanStats::default();
            let hash = decode_hash(&hex).unwrap_or([0u8; 20]);
            match self
                .scrape_swarm(shutdown, &md.trackers, &hash, &dummy)
                .await
            {
                Ok(sw) => {
                    // Progress = verified bytes on disk; grows iff new data lands.
                    // completed_pieces is a u64 byte count now: a u32 cap used to
                    // saturate past 4 GiB, making every later refresh look like
                    // "progress" and freezing the stall clock at scan times.
                    let progress = self.held_progress_bytes(&h);
                    updates.push((
                        hex,
                        Box::new(move |t: &mut state::Torrent| {
                            t.last_known_seeders = sw.seeders;
                            if progress > t.completed_pieces || t.last_progress_at.is_none() {
                                t.completed_pieces = progress;
                                t.last_progress_at = Some(Utc::now());
                            }
                        }),
                    ));
                }
                Err(e) => tracing::warn!("could not scrape held torrent {}: {e:#}", h.title),
            }
        }
        let updated_any = self.state.update_each(updates).unwrap_or(0) > 0;
        if updated_any {
            self.push_live();
        }
    }

    /// Verified bytes on disk for a held torrent (rqbit stats when managed,
    /// else on-disk dir size as a conservative proxy).
    fn held_progress_bytes(&self, h: &state::Torrent) -> u64 {
        if let Ok(Some(handle)) = engtorrents::find_torrent(&self.session, &h.info_hash) {
            return handle.stats().progress_bytes;
        }
        dir_size_bytes(&engtorrents::torrent_output_dir(
            &h.storage_location,
            &h.info_hash,
        ))
    }

    async fn evict_stalled_torrents(&mut self, catalog_hashes: &HashSet<String>) {
        let timeout = self.cfg.scan.stall_eviction_timeout;
        if timeout <= Duration::ZERO {
            return;
        }
        let now = Utc::now();
        let mut removed_any = false;
        for h in self.state.all() {
            if !catalog_hashes.contains(&h.info_hash) {
                continue;
            }
            // CHEAP CHECK FIRST: almost every held torrent is healthy, so
            // judge on the state snapshot before touching the session.
            // (This used to run last, after two O(n) session sweeps per
            // held torrent per scan.) No progress stamp yet = still
            // settling; not past the timeout = not evictable either way.
            let Some(since) = h.last_progress_at.and_then(|t| (now - t).to_std().ok()) else {
                continue;
            };
            if since < timeout {
                continue;
            }
            // Past the timeout: only now pay for the session lookups (one
            // find_torrent serves all three checks below).
            let handle = engtorrents::find_torrent(&self.session, &h.info_hash)
                .ok()
                .flatten();
            // A torrent running its integrity check reads as "incomplete"
            // (progress = bytes the check has walked so far) even when the
            // data is all there — boot windows run hundreds of these. Skip
            // checks in flight; the NEXT scan judges them on settled state.
            let checking = handle
                .as_ref()
                .map(|t| {
                    t.with_state(|s| matches!(s, librqbit::ManagedTorrentState::Initializing(_)))
                })
                .unwrap_or(false);
            if checking {
                continue;
            }
            // Fully-present torrents are complete and healthy: their
            // progress clock froze at completion (nothing left to verify),
            // so the stall timeout must never apply to them. The progress
            // comparison alone can never satisfy torrents declaring
            // padding files (rqbit excludes padding from piece selection,
            // so progress tops out below the all-files size) — rqbit's own
            // finished flag is the exact complete signal for those.
            let finished = handle.as_ref().map(|t| t.stats().finished).unwrap_or(false);
            let progress = match &handle {
                Some(t) => t.stats().progress_bytes,
                // Not in the session: fall back to the on-disk dir size as
                // a conservative proxy (also the recovery path when the
                // data dir was deleted externally — 0 → judged incomplete).
                None => dir_size_bytes(&engtorrents::torrent_output_dir(
                    &h.storage_location,
                    &h.info_hash,
                )),
            };
            if finished || progress >= h.size_bytes {
                continue;
            }
            // Stall timeout applies to anything still INCOMPLETE regardless
            // of swarm depth: zero-seeders (the original rule) and
            // seeded-but-stuck swarms (e.g. broken-piece loops) alike.
            // Incomplete + alive swarm is the quarantine's territory well
            // before 90d; this is the long-tail backstop.
            let reason = if h.last_known_seeders == 0 {
                format!(
                    "zero seeders and no download progress for {}",
                    crate::humanize::human_duration(since)
                )
            } else {
                format!(
                    "no download progress for {} ({} seeders, incomplete)",
                    crate::humanize::human_duration(since),
                    h.last_known_seeders
                )
            };
            tracing::warn!("removing stalled torrent {}: {}", h.title, reason);
            let out_dir = engtorrents::torrent_output_dir(&h.storage_location, &h.info_hash);
            if let Err(e) = engtorrents::remove_torrent(&self.session, &h.info_hash, &out_dir).await
            {
                tracing::error!("failed to remove stalled torrent {}: {e:#}", h.title);
            }
            let _ = std::fs::remove_file(self.torrent_cache_path(&h.info_hash));
            if let Err(e) = self.state.remove(&h.info_hash) {
                tracing::error!("failed to drop state for {}: {e:#}", h.title);
            } else {
                removed_any = true;
                self.history.record(&history::remove_event(
                    &h.info_hash,
                    &h.title,
                    h.size_bytes,
                    h.last_known_seeders,
                    &h.storage_location,
                    history::Cause::Stalled,
                    &reason,
                ));
            }
        }
        if removed_any {
            self.push_live();
        }
    }

    async fn resume_held(&mut self) -> Result<()> {
        let held = self.state.all();
        tracing::info!("resuming {} held torrents", held.len());
        let total = held.len();
        for (done, h) in held.into_iter().enumerate() {
            if done % 25 == 0 {
                tracing::info!("resuming held torrents ({done}/{total})");
            }
            let out_dir = engtorrents::torrent_output_dir(&h.storage_location, &h.info_hash);
            // Parse-then-drop: raw bytes are freed before the blocking add,
            // so resume never holds more than one .torrent in memory.
            let md = match std::fs::read(self.torrent_cache_path(&h.info_hash))
                .ok()
                .and_then(|raw| attorrent::parse_torrent_bytes(&raw).ok())
            {
                Some(md) => md,
                None => {
                    tracing::warn!(
                        "skipping resume {}: could not load cached .torrent",
                        h.info_hash
                    );
                    continue;
                }
            };
            let tiers = tiers_of(&md.trackers);
            let keyed =
                atkey::at_trackers_only(tiers, &self.user_announce, &self.user_announce_ipv6);
            if let Err(e) = engtorrents::add_torrent_bytes(
                &self.session,
                &h.info_hash,
                &out_dir,
                keyed,
                &self.torrent_cache_path(&h.info_hash),
                false,
            )
            .await
            {
                tracing::warn!("skipping resume {}: {e:#}", h.info_hash);
            }
        }
        Ok(())
    }

    /// Log the runtime summary line and persist the snapshot file. The
    /// socket serves live numbers from the same inputs; the file stays as
    /// the offline fallback `status` reads when no daemon is running.
    fn log_and_save_runtime(&self, kind: &str) {
        // Cap the daemon's log file: it is written directly (stdout/stderr
        // are dup2'd into it, not streamed to journald), so nothing else
        // rotates it. Rewritten in place to its newest half when it
        // outgrows the cap; best-effort, and stdout runs are uncapped.
        if let Some(log_path) = &self.cfg.log_file {
            let _ = history::cap_text_file(log_path, history::MAX_LOG_BYTES);
        }
        // Advance the bandwidth history on the stats cadence so hour/day
        // rates stay exact even when nobody queries the socket for hours.
        if let Some(live) = &self.live {
            live.tick_tracker();
        }
        let held = self.state.all();
        let by_hash: std::collections::HashMap<String, &state::Torrent> =
            held.iter().map(|t| (t.info_hash.clone(), t)).collect();
        let seeding = std::cell::Cell::new(0usize);
        let finished_now = std::cell::RefCell::new(Vec::<String>::new());
        self.session.with_torrents(|it| {
            for (_, h) in it {
                if h.stats().finished {
                    seeding.set(seeding.get() + 1);
                    finished_now.borrow_mut().push(h.info_hash().as_string());
                }
            }
        });
        let seeding = seeding.get();
        // One info line per newly completed download (titles + sizes from
        // state; unknown hashes are session-managed but untracked, e.g.
        // census probes - those never log here).
        if let Ok(mut seen) = self.completed_seen.lock() {
            for hex in finished_now.borrow().iter() {
                if seen.insert(hex.clone()) {
                    match by_hash.get(hex) {
                        Some(t) => tracing::info!(
                            "download completed (title={} size={})",
                            t.title,
                            crate::humanize::human_bytes(t.size_bytes as i64),
                        ),
                        None => {
                            tracing::info!("download completed (infohash={} not in state)", hex)
                        }
                    }
                }
            }
            // Forget hashes no longer managed, so a re-added torrent logs
            // again on its next completion instead of staying silent.
            let live: std::collections::HashSet<String> =
                finished_now.borrow().iter().cloned().collect();
            seen.retain(|h| live.contains(h));
        }
        let locs: Vec<(PathBuf, u64)> = self
            .cfg
            .storage
            .iter()
            .map(|l| (l.path.clone(), l.limit_bytes()))
            .collect();
        let (used, limit) = engstats::disk_usage(&locs);
        // Committed = sum of held torrents' nominal sizes: what the node has
        // reserved out of the configured limit. Unlike the on-disk walk this
        // is instant and counts data still being checked/downloaded.
        let committed: u64 = held.iter().map(|t| t.size_bytes).sum();
        let s = engstats::collect(
            &self.api,
            self.started_at,
            held.len(),
            seeding.min(held.len()),
            self.state.quarantine_count(),
            used,
            limit,
            committed,
        );
        tracing::info!(
            "runtime stats (kind={kind} held={} seeding={} quarantined={} disk={}/{} committed={} up={} down={} peers={} rss={} uptime={}s)",
            s.held_torrents,
            s.seeding_torrents,
            s.quarantined_torrents,
            crate::humanize::human_bytes(s.disk_used_bytes as i64),
            crate::humanize::human_bytes(s.disk_limit_bytes as i64),
            crate::humanize::human_bytes(s.disk_committed_bytes as i64),
            crate::humanize::human_bytes(s.useful_bytes_uploaded as i64),
            crate::humanize::human_bytes(s.useful_bytes_downloaded as i64),
            s.active_peers,
            crate::humanize::human_bytes(s.process_rss_bytes as i64),
            s.uptime_seconds,
        );
        let _ = engstats::save(&self.cfg.data_dir, &s);
    }

    /// Graceful shutdown: stop the rqbit session.
    pub async fn close(&self) {
        rqsession::stop_session(&self.session, Duration::from_secs(10)).await;
    }
}

impl crate::config::StorageLocation {
    pub fn limit_bytes(&self) -> u64 {
        match self.limit {
            crate::config::StorageLimit::Bytes(b) => b,
            _ => 0,
        }
    }
}

/// Sum of resolved disk limits across storage locations. Must be called
/// after `resolve_all_limits`, so `limit: max` is already concrete bytes.
fn disk_limit_total(cfg: &Config) -> u64 {
    cfg.storage.iter().map(|l| l.limit_bytes()).sum()
}

/// Snapshot the held set into live-query entries (title/hash/size/seeders).
fn snapshot_entries(state: &State) -> Vec<crate::live::StateEntry> {
    state
        .all()
        .into_iter()
        .map(|t| crate::live::StateEntry {
            title: t.title,
            info_hash: t.info_hash,
            size_bytes: t.size_bytes,
            last_known_seeders: t.last_known_seeders,
        })
        .collect()
}

fn shuffle<T>(v: &mut [T]) {
    use rand::seq::SliceRandom;
    v.shuffle(&mut rand::thread_rng());
}

/// Why a catalog item is not a pending candidate.
///
/// Single spelling for the three sites that ask: `count_pending` (the
/// `total_candidates` figure the snapshot and progress percent read), the
/// evaluation prefilter, and the oversize log counter. They had already
/// drifted apart - the oversize counter forgot the blocklist check, so the
/// "disqualified N oversized candidates" line disagreed with the
/// `total=...` printed in the same log record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prefilter {
    Held,
    Blocked,
    TooBig,
    Pending,
}

fn prefilter_reason(
    item: &atcatalog::Item,
    held: &HashSet<String>,
    blocklist: &KeywordBlocklist,
    max_fittable: u64,
) -> Prefilter {
    if held.contains(&hex::encode(item.info_hash)) {
        return Prefilter::Held;
    }
    if blocklist.blocks(&item.title, &item.description).is_some() {
        return Prefilter::Blocked;
    }
    if max_fittable > 0 && item.size_bytes > max_fittable {
        return Prefilter::TooBig;
    }
    Prefilter::Pending
}

fn count_pending(
    items: &[atcatalog::Item],
    held: &HashSet<String>,
    blocklist: &KeywordBlocklist,
    max_fittable: u64,
) -> u64 {
    items
        .iter()
        .filter(|i| prefilter_reason(i, held, blocklist, max_fittable) == Prefilter::Pending)
        .count() as u64
}

fn tiers_of(flat: &[String]) -> Vec<Vec<String>> {
    // .torrent tier structure isn't preserved by parse; treat AT's tracker
    // (conventionally first) as its own tier implicitly via scrape order.
    // For announce filtering, one tier per URL is equivalent: filtering is
    // per-URL, order-preserving.
    flat.iter().map(|t| vec![t.clone()]).collect()
}

fn decode_hash(hex_str: &str) -> Result<[u8; 20]> {
    let b = hex::decode(hex_str.trim()).context("invalid infohash hex")?;
    if b.len() != 20 {
        anyhow::bail!("invalid infohash length");
    }
    let mut h = [0u8; 20];
    h.copy_from_slice(&b);
    Ok(h)
}

/// Greedy displaceable-set selection within one location. Sizes are
/// nominal bytes here (plain storage: nominal == on-disk up to the fixed
/// buffer, applied symmetrically on both sides).
///
/// The set must free enough disk AND enough RAM: freed RAM is the sum of the
/// displaced torrents' footprints (stored piece counts, unknown = typical),
/// and the swap proceeds only when freed RAM covers the candidate's cost.
/// This keeps a many-piece candidate from evicting one cheap torrent and
/// pushing RSS past the budget — the displaced set must price out.
///
/// Eviction order follows the adaptive size bias: a large-biased host evicts
/// its smallest bytes-per-RAM torrents first (they contribute the least disk
/// per RAM slot), a small-biased host evicts its largest first (freeing disk
/// is easy; the scarce resource is slots, so it keeps the many small
/// torrents that urgency ranking surfaced).
fn select_displaceable(
    in_location: &[state::Torrent],
    candidate_seeders: u32,
    size_needed: u64,
    ram_cost: u64,
    peer_limit: usize,
    min_seed_margin: i32,
    size_bias: f64,
) -> Option<Vec<state::Torrent>> {
    let mut qualifying: Vec<state::Torrent> = in_location
        .iter()
        .filter(|h| {
            (h.last_known_seeders as i64) - (min_seed_margin as i64) >= candidate_seeders as i64
        })
        .cloned()
        .collect();
    if qualifying.is_empty() {
        return None;
    }
    // Evict worst bytes-per-RAM first when bias favors large (positive),
    // best bytes-per-RAM first when bias favors small (negative): i.e. sort
    // ascending by the same adjusted score rank_candidates sorts descending.
    // Zero bias keeps the legacy order (highest-seeded qualifying first).
    qualifying.sort_by(|a, b| {
        let ca = Candidate {
            info_hash: [0u8; 20],
            title: String::new(),
            size_bytes: a.size_bytes,
            piece_count: a.piece_count,
            seeders: a.last_known_seeders,
            leechers: 0,
            seeder_floor: 0,
        };
        let cb = Candidate {
            info_hash: [0u8; 20],
            title: String::new(),
            size_bytes: b.size_bytes,
            piece_count: b.piece_count,
            seeders: b.last_known_seeders,
            leechers: 0,
            seeder_floor: 0,
        };
        crate::selector::eviction_score(&ca, size_bias, peer_limit)
            .partial_cmp(&crate::selector::eviction_score(&cb, size_bias, peer_limit))
            .unwrap_or_else(|| a.size_bytes.cmp(&b.size_bytes))
    });
    let mut chosen = Vec::new();
    let mut freed = 0u64;
    let mut freed_ram = 0u64;
    for h in qualifying {
        freed = freed.saturating_add(h.size_bytes);
        freed_ram = freed_ram.saturating_add(ram::torrent_ram(h.piece_count.max(1), peer_limit));
        chosen.push(h);
        if freed >= size_needed && freed_ram >= ram_cost {
            return Some(chosen);
        }
    }
    None
}

// ---- scrape cache (persisted per-torrent counts, TTL = scan interval) ----

#[derive(Debug, Default)]
struct SwarmCache {
    path: PathBuf,
    ttl: Duration,
    entries: HashMap<[u8; 20], CachedSwarm>,
}

#[derive(Debug, Clone)]
struct CachedSwarm {
    counts: SwarmCounts,
    at: DateTime<Utc>,
}

impl SwarmCache {
    fn load(path: &Path, ttl: Duration) -> SwarmCache {
        let mut c = SwarmCache {
            path: path.to_path_buf(),
            ttl,
            entries: HashMap::new(),
        };
        if let Ok(data) = std::fs::read(path) {
            if let Ok(stored) = serde_json::from_slice::<StoredCache>(&data) {
                for (hex, e) in stored.entries {
                    if let Ok(h) = decode_hash(&hex) {
                        c.entries.insert(
                            h,
                            CachedSwarm {
                                counts: SwarmCounts {
                                    seeders: e.seeders,
                                    leechers: e.leechers,
                                    completed: 0,
                                },
                                at: e.at,
                            },
                        );
                    }
                }
            }
        }
        c
    }

    fn get(&self, info_hash: &[u8; 20]) -> Option<SwarmCounts> {
        let e = self.entries.get(info_hash)?;
        if (Utc::now() - e.at).to_std().unwrap_or(Duration::MAX) > self.ttl {
            return None;
        }
        Some(e.counts)
    }

    fn insert(&mut self, info_hash: [u8; 20], counts: SwarmCounts) {
        self.entries.insert(
            info_hash,
            CachedSwarm {
                counts,
                at: Utc::now(),
            },
        );
    }

    fn save(&self) -> Result<()> {
        let mut entries = HashMap::new();
        for (h, e) in &self.entries {
            entries.insert(
                hex::encode(h),
                StoredEntry {
                    seeders: e.counts.seeders,
                    leechers: e.counts.leechers,
                    at: e.at,
                },
            );
        }
        let data = serde_json::to_string_pretty(&StoredCache { entries })?;
        crate::config::atomic_write(&self.path, data.as_bytes())?;
        Ok(())
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredCache {
    #[serde(default)]
    entries: HashMap<String, StoredEntry>,
}

/// Device-level swap gate: `None` admits, `Some(reason)` blocks. Device
/// free after displacement (the ACTUAL bytes the displaced torrents
/// occupied) must cover the candidate's full need - sparse displacement
/// frees almost nothing real. A missing device-free reading (statvfs
/// failure) admits: the nominal per-location checks still apply, and
/// refusing every swap on a statfs hiccup would stall the node.
fn device_gate_reason(
    dev_free: Option<u64>,
    freed_actual: u64,
    size_needed: u64,
) -> Option<String> {
    let dev_free = dev_free?;
    let after = dev_free.saturating_add(freed_actual);
    if after < size_needed {
        Some(format!(
            "device free space after displacement ({}) is below the candidate's need ({}) - displaced torrents were sparse",
            crate::humanize::human_bytes(after as i64),
            crate::humanize::human_bytes(size_needed as i64),
        ))
    } else {
        None
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredEntry {
    seeders: u32,
    leechers: u32,
    at: DateTime<Utc>,
}

#[cfg(test)]
mod prefilter_tests {
    use super::*;

    fn item(hash_byte: u8, title: &str, size: u64) -> atcatalog::Item {
        atcatalog::Item {
            title: title.to_string(),
            category: String::new(),
            info_hash: [hash_byte; 20],
            guid: String::new(),
            link: String::new(),
            description: String::new(),
            size_bytes: size,
        }
    }

    /// The three sites that ask "is this a pending candidate?" share one
    /// answer. The oversize counter used to spell the rule itself (held ->
    /// size, no blocklist), so it counted blocklisted+oversized items that
    /// `count_pending` excluded - the "disqualified N oversized" line and
    /// the `total=M` figure in the same log record could contradict.
    #[test]
    fn one_predicate_answers_for_every_caller() {
        let held: HashSet<String> = [hex::encode([1u8; 20])].into_iter().collect();
        let blocklist = KeywordBlocklist::new(vec!["blocked".to_string()]);

        let held_item = item(1, "fine", 10);
        let blocked_item = item(2, "a blocked title", 10);
        let blocked_and_big = item(3, "blocked and huge", u64::MAX);
        let too_big = item(4, "fine but huge", u64::MAX);
        let pending = item(5, "fine", 10);

        let all = [
            &held_item,
            &blocked_item,
            &blocked_and_big,
            &too_big,
            &pending,
        ];
        let max_fittable = 1_000u64;

        // The classification itself.
        for (i, want) in [
            (&held_item, Prefilter::Held),
            (&blocked_item, Prefilter::Blocked),
            (&blocked_and_big, Prefilter::Blocked),
            (&too_big, Prefilter::TooBig),
            (&pending, Prefilter::Pending),
        ] {
            assert_eq!(
                prefilter_reason(i, &held, &blocklist, max_fittable),
                want,
                "misclassified: {}",
                i.title
            );
        }

        // count_pending sees exactly the Pending one...
        let items: Vec<atcatalog::Item> = all.iter().map(|i| (*i).clone()).collect();
        assert_eq!(
            count_pending(&items, &held, &blocklist, max_fittable),
            1,
            "only the clean small item is a pending candidate"
        );
        // ...and the oversize counter sees only items whose ONLY disqualifier
        // is size - never a blocklisted one, however large.
        let too_big_count = items
            .iter()
            .filter(|i| prefilter_reason(i, &held, &blocklist, max_fittable) == Prefilter::TooBig)
            .count();
        assert_eq!(
            too_big_count, 1,
            "a blocklisted+oversized item must not be counted as oversized"
        );
    }
}

#[cfg(test)]
mod device_gate_tests {
    use super::device_gate_reason;

    #[test]
    fn blocks_when_device_free_after_displacement_cannot_fit_candidate() {
        // Sparse displaced torrent: freed actual ~0, device nearly full.
        let r = device_gate_reason(Some(1_000_000), 0, 60_262_144);
        assert!(r.is_some(), "gate must block: {r:?}");
    }

    #[test]
    fn admits_when_displacement_frees_enough_real_space() {
        assert!(device_gate_reason(Some(1_000_000), 100_000_000, 60_262_144).is_none());
        assert!(device_gate_reason(Some(60_262_144), 0, 60_262_144).is_none());
    }

    #[test]
    fn admits_on_missing_device_free_reading() {
        assert!(device_gate_reason(None, 0, 60_262_144).is_none());
    }

    #[test]
    fn saturates_instead_of_overflowing() {
        assert!(
            device_gate_reason(Some(u64::MAX), u64::MAX, 1).is_none(),
            "saturating add must not wrap to a small number"
        );
    }
}

#[cfg(test)]
mod rate_limit_tests {
    use super::*;

    /// A NaN rate limit used to panic RateLimiter::wait
    /// (Duration::from_secs_f64(1.0/NaN); panic=abort in release). The
    /// defensive predicate must keep it a no-op instead.
    #[tokio::test]
    async fn rate_limiter_wait_does_not_panic_on_nan() {
        let mut rl = RateLimiter {
            per_second: f64::NAN,
            next_allowed: tokio::time::Instant::now(),
        };
        let res = tokio::spawn(async move { rl.wait().await }).await;
        assert!(res.is_ok(), "wait must not panic on NaN: {res:?}");
    }

    #[test]
    fn validate_rejects_nan_rate_limit() {
        let scan: crate::config::ScanConfig =
            serde_yaml::from_str("rate_limit_per_second: .nan").unwrap();
        assert!(scan.rate_limit_per_second.is_nan());
        let cfg = crate::config::Config {
            storage: vec![crate::config::StorageLocation {
                path: "/tmp/keep-at-nan-check".into(),
                limit: crate::config::StorageLimit::Bytes(1 << 30),
            }],
            scan,
            ..crate::config::Config::default()
        };
        assert!(
            cfg.validate().is_err(),
            "NaN rate_limit_per_second must fail validation"
        );
    }
}
