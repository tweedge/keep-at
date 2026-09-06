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
    swarm_cache: std::sync::Mutex<SwarmCache>,
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
    started_at: std::time::Instant,
    rate: Arc<tokio::sync::Mutex<RateLimiter>>,
    evaluated_counts: tokio::sync::Mutex<Vec<u32>>,
    last_scan: tokio::sync::Mutex<Option<LastScanStats>>,
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
        if self.per_second <= 0.0 {
            return;
        }
        let now = tokio::time::Instant::now();
        if now < self.next_allowed {
            tokio::time::sleep(self.next_allowed - now).await;
        }
        let gap = Duration::from_secs_f64(1.0 / self.per_second);
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
#[derive(Clone)]
struct EvalCtx {
    data_dir: PathBuf,
    moderation_bypass: Duration,
    http: reqwest::Client,
    torrent_base_url: String,
    user_agent: String,
    rate: Arc<tokio::sync::Mutex<RateLimiter>>,
    shutdown: tokio::sync::watch::Receiver<bool>,
}

fn eval_torrent_cache_path(data_dir: &Path, info_hash_hex: &str) -> PathBuf {
    data_dir
        .join("torrent-cache")
        .join(format!("{info_hash_hex}.torrent"))
}

async fn eval_fetch_metadata(ctx: &EvalCtx, info_hash_hex: &str) -> Result<TorrentMeta> {
    let path = eval_torrent_cache_path(&ctx.data_dir, info_hash_hex);
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

async fn eval_scrape_swarm(
    ctx: &EvalCtx,
    cache: &std::sync::Mutex<SwarmCache>,
    trackers: &[String],
    info_hash: &[u8; 20],
    stats: &ScanStats,
) -> Result<SwarmCounts> {
    if let Ok(c) = cache.lock() {
        if let Some(hit) = c.get(info_hash) {
            stats.scrape_cached.fetch_add(1, Ordering::Relaxed);
            return Ok(hit);
        }
    }
    let mut last_err: Option<anyhow::Error> = None;
    for tracker in trackers {
        if crate::atkey::is_at_tracker_url(tracker) {
            ctx.rate.lock().await.wait().await;
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
                &ctx.http,
                &buildinfo::scraper_user_agent(),
                tracker,
                info_hash,
            ),
        )
        .await;
        match call {
            Ok(Ok(c)) => {
                if let Ok(mut cc) = cache.lock() {
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
                    let mut sd = ctx.shutdown.clone();
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(60)) => {}
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

/// True when an error looks like HTTP 429 / rate limiting from a tracker.
fn is_rate_limited(e: &anyhow::Error) -> bool {
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
}

impl Engine {
    pub async fn new(cfg: Config) -> Result<Engine> {
        Engine::new_with_options(cfg, Options::default()).await
    }

    pub async fn new_with_options(cfg: Config, opts: Options) -> Result<Engine> {
        cfg.validate()?;
        let cfg = storage::resolve_all_limits(&cfg)?;

        std::fs::create_dir_all(&cfg.data_dir)
            .with_context(|| format!("creating data dir {}", cfg.data_dir.display()))?;
        for loc in &cfg.storage {
            std::fs::create_dir_all(&loc.path)
                .with_context(|| format!("creating storage {}", loc.path.display()))?;
        }

        let state = State::load(&cfg.data_dir.join("state.json"))?;
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

        let mut eng = Engine {
            cfg,
            session,
            api,
            state,
            swarm_cache: std::sync::Mutex::new(swarm_cache),
            blocklist: KeywordBlocklist::new(Vec::new()),
            http,
            catalog,
            torrent_fetcher,
            user_announce,
            user_announce_ipv6,
            max_torrents,
            ram_budget: budget,
            size_bias,
            started_at: std::time::Instant::now(),
            rate: Arc::new(tokio::sync::Mutex::new(RateLimiter {
                per_second: 0.5,
                next_allowed: tokio::time::Instant::now(),
            })),
            evaluated_counts: tokio::sync::Mutex::new(Vec::new()),
            last_scan: tokio::sync::Mutex::new(None),
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

    fn torrent_cache_path(&self, info_hash_hex: &str) -> PathBuf {
        self.cfg
            .data_dir
            .join("torrent-cache")
            .join(format!("{info_hash_hex}.torrent"))
    }

    // ---- main loop (Run) ----

    /// Run scans on the scan-interval cadence until cancelled. First scan runs
    /// immediately unless a scan completed recently.
    pub async fn run(&mut self, mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
        self.log_and_save_runtime("startup");

        if self.delay_until_next_scan() > Duration::ZERO {
            let d = self.delay_until_next_scan();
            tracing::info!(
                "next scan is not due yet; waiting {} instead of scanning immediately",
                crate::humanize::human_duration(d)
            );
            tokio::select! {
                _ = tokio::time::sleep(d) => {}
                _ = shutdown.changed() => return Ok(()),
            }
        }

        // Periodic runtime stats run on their own task so they keep ticking
        // during long scans. The task owns an mpsc sender; each tick it asks
        // the main loop (via select) to collect+save - collection needs &self
        // (session/state), so it must happen on this task, not the spawned one.
        let stats_interval = self.cfg.stats_interval;
        let mut scan_interval =
            tokio::time::interval(self.cfg.scan.interval.max(Duration::from_secs(60)));

        self.run_scan_logged("initial", shutdown.clone()).await;
        self.log_and_save_runtime("post-initial-scan");

        // Dedicated periodic stats timer. NOTE: while a scan runs, this task
        // is inside run_scan_logged and the select below is not live, so
        // periodic ticks only fire between scans. Post-scan saves (above)
        // guarantee fresh stats after every scan regardless.
        let mut stats_tick = tokio::time::interval(stats_interval.max(Duration::from_secs(60)));
        if stats_interval <= Duration::ZERO {
            // Disabled: set a far-future interval so the arm never fires.
            stats_tick = tokio::time::interval(Duration::from_secs(3600 * 24 * 365));
        }

        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = scan_interval.tick() => {
                    self.run_scan_logged("periodic", shutdown.clone()).await;
                    self.log_and_save_runtime("post-scan");
                }
                _ = stats_tick.tick(), if stats_interval > Duration::ZERO => { self.log_and_save_runtime("periodic"); }
            }
        }
        Ok(())
    }

    async fn run_scan_logged(&mut self, kind: &str, shutdown: tokio::sync::watch::Receiver<bool>) {
        tracing::info!("scan starting (kind={kind})");
        let start = std::time::Instant::now();
        match self.scan_once_shutdown(shutdown).await {
            Ok(()) => tracing::info!(
                "scan completed (kind={kind}, duration={:?})",
                start.elapsed()
            ),
            Err(e) => tracing::error!(
                "scan failed (kind={kind}, duration={:?}): {e:#}",
                start.elapsed()
            ),
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
        let held = self.state.all();
        let held_hashes: HashSet<String> = held.iter().map(|h| h.info_hash.clone()).collect();

        self.remove_deleted_torrents(&held, &catalog_hashes).await;
        self.refresh_held_seeder_counts(&shutdown).await;
        if *shutdown.borrow() {
            anyhow::bail!("scan interrupted by shutdown");
        }
        self.evict_stalled_torrents(&catalog_hashes).await;

        let max_fittable = self.max_fittable_size();
        let mut too_big = 0u64;
        for item in &items {
            if held_hashes.contains(&hex::encode(item.info_hash)) {
                continue;
            }
            if max_fittable > 0 && item.size_bytes > max_fittable {
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

        let stats = ScanStats::default();
        let mut library_bytes = 0u64;
        for item in &items {
            library_bytes += item.size_bytes;
        }
        stats.library_bytes.store(library_bytes, Ordering::Relaxed);

        let scrape_started = std::time::Instant::now();
        // Incremental acting: evaluate the walk, then act over its output in
        // arrival-sized batches to preserve the windowing behavior below.
        // (evaluate_candidates buffers the whole walk before returning;
        // true streaming is future work - the batching still bounds each
        // re-rank to EVALUATE_CONCURRENCY arrivals plus a final flush.)
        let mut acted: HashSet<String> = HashSet::new();
        let mut held_count = self.state.all().len();
        let mut batch: Vec<Evaluated> = Vec::new();
        let mut processed = 0u64;
        let evaluated_all = self
            .evaluate_candidates(
                &shutdown,
                &items,
                &held_hashes,
                scan_started_at,
                total_candidates,
                &stats,
                max_fittable,
            )
            .await;
        if *shutdown.borrow() {
            anyhow::bail!("scan interrupted by shutdown");
        }
        // NOTE: evaluate_candidates currently buffers the whole walk before
        // returning (streaming is a TODO); act incrementally over its output
        // in arrival-sized batches to preserve the windowing behavior.
        for ev in evaluated_all {
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

        let counts: Vec<u32> = self.evaluated_counts.lock().await.clone();
        let floor = selector::seeder_floor(&counts);
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

    // ---- evaluation ----

    /// Evaluate every pending candidate concurrently; returns lightweight results.
    #[allow(clippy::too_many_arguments)]
    async fn evaluate_candidates(
        &self,
        shutdown: &tokio::sync::watch::Receiver<bool>,
        items: &[atcatalog::Item],
        held_hashes: &HashSet<String>,
        scan_started_at: DateTime<Utc>,
        total_candidates: u64,
        stats: &ScanStats,
        max_fittable: u64,
    ) -> Vec<Evaluated> {
        // Share stats + swarm cache with spawned tasks via Arcs (atomics +
        // mutex are Sync; references cannot cross spawn boundaries).
        let shared = Arc::new(ScanStats {
            library_bytes: AtomicU64::new(stats.library_bytes.load(Ordering::Relaxed)),
            metadata_fetched: AtomicU64::new(stats.metadata_fetched.load(Ordering::Relaxed)),
            metadata_cached: AtomicU64::new(stats.metadata_cached.load(Ordering::Relaxed)),
            scrape_requests: AtomicU64::new(stats.scrape_requests.load(Ordering::Relaxed)),
            scrape_cached: AtomicU64::new(stats.scrape_cached.load(Ordering::Relaxed)),
            skipped_held: AtomicU64::new(stats.skipped_held.load(Ordering::Relaxed)),
            skipped_blocked: AtomicU64::new(stats.skipped_blocked.load(Ordering::Relaxed)),
            skipped_too_big: AtomicU64::new(stats.skipped_too_big.load(Ordering::Relaxed)),
            skipped_age: AtomicU64::new(stats.skipped_age.load(Ordering::Relaxed)),
            skipped_fetch_err: AtomicU64::new(stats.skipped_fetch_err.load(Ordering::Relaxed)),
            skipped_scrape_err: AtomicU64::new(stats.skipped_scrape_err.load(Ordering::Relaxed)),
            eligible: AtomicU64::new(stats.eligible.load(Ordering::Relaxed)),
        });
        // Share the swarm cache with spawned tasks via a standalone Arc.
        // Snapshot current entries into a new shareable cache, then write
        // back afterwards (evaluation only inserts).
        let shared_cache = Arc::new(std::sync::Mutex::new(SwarmCache::load(
            &self.cfg.data_dir.join("scrape-cache.json"),
            self.cfg.scan.interval,
        )));
        let sem = Arc::new(Semaphore::new(EVALUATE_CONCURRENCY));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Evaluated>();
        let processed = Arc::new(AtomicU64::new(0));
        let min_age = self.cfg.scan.moderation_delay;

        // Progress saver.
        let stats_path = self.network_stats_path();
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

        let mut pending: Vec<atcatalog::Item> = Vec::new();
        for item in items {
            if *shutdown.borrow() {
                break;
            }
            let hex = hex::encode(item.info_hash);
            if held_hashes.contains(&hex) {
                stats.skipped_held.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if self
                .blocklist
                .blocks(&item.title, &item.description)
                .is_some()
            {
                stats.skipped_blocked.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if max_fittable > 0 && item.size_bytes > max_fittable {
                stats.skipped_too_big.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            pending.push(item.clone());
        }

        // Evaluate with bounded concurrency. evaluate_one only needs shared
        // state (&self methods used are all &self), so wrap the needed inputs
        // in an Arc-owned context instead of borrowing self across spawns.
        let ctx = Arc::new(EvalCtx {
            data_dir: self.cfg.data_dir.clone(),
            moderation_bypass: min_age,
            http: self.http.clone(),
            torrent_base_url: self.torrent_fetcher.base_url.clone(),
            user_agent: buildinfo::user_agent(),
            rate: self.rate.clone(),
            shutdown: shutdown.clone(),
        });
        let tx2 = tx.clone();
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
            let stats_outer = shared.clone();
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
        for h in handles {
            let _ = h.await;
        }
        stop.store(true, Ordering::Relaxed);
        let _ = progress_task.await;
        drop(tx);
        // Merge Arc stats back into the caller's counters.
        add_into(stats, &shared);
        // Merge newly scraped entries back into the Engine cache.
        if let (Ok(shared), Ok(mut own)) = (shared_cache.lock(), self.swarm_cache.lock()) {
            for (h, e) in &shared.entries {
                own.entries.entry(*h).or_insert_with(|| e.clone());
            }
        }

        let mut out = Vec::new();
        // Drain whatever arrived. recv() ends when all senders drop; on a
        // shutdown-abandoned dispatch some senders may linger in wedged
        // tasks, so bound the drain rather than waiting forever.
        let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(ev)) => out.push(ev),
                _ => break,
            }
        }
        // Remember seeder counts for the floor computation.
        *self.evaluated_counts.lock().await = out.iter().map(|e| e.seeders).collect();
        out
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
    /// shared rate limiter.
    async fn scrape_swarm(
        &self,
        shutdown: &tokio::sync::watch::Receiver<bool>,
        trackers: &[String],
        info_hash: &[u8; 20],
        stats: &ScanStats,
    ) -> Result<SwarmCounts> {
        if let Ok(cache) = self.swarm_cache.lock() {
            if let Some(c) = cache.get(info_hash) {
                stats.scrape_cached.fetch_add(1, Ordering::Relaxed);
                return Ok(c);
            }
        }
        let mut last_err: Option<anyhow::Error> = None;
        for tracker in trackers {
            if crate::atkey::is_at_tracker_url(tracker) {
                self.rate.lock().await.wait().await;
            }
            if !tracker.starts_with("http://") && !tracker.starts_with("https://") {
                // UDP trackers (BEP 15 scrape) are not implemented; skip quietly.
                continue;
            }
            stats.scrape_requests.fetch_add(1, Ordering::Relaxed);
            let call = tokio::time::timeout(
                SCRAPE_TIMEOUT,
                attorrent::scrape_http(
                    &self.http,
                    &buildinfo::scraper_user_agent(),
                    tracker,
                    info_hash,
                ),
            )
            .await;
            match call {
                Ok(Ok(c)) => {
                    if let Ok(mut cache) = self.swarm_cache.lock() {
                        cache.insert(*info_hash, c);
                    }
                    return Ok(c);
                }
                Ok(Err(e)) => {
                    if is_rate_limited(&e) {
                        tracing::warn!(
                            "tracker rate-limited during held refresh, backing off: {e:#}"
                        );
                        let mut sd = shutdown.clone();
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(60)) => {}
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
        let on_disk = dir_size_bytes(&loc.path);
        let mut free = loc.limit_bytes().saturating_sub(on_disk);
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

        let mut decision = selector::SwapDecision {
            should_swap: false,
            chance: 0.0,
            roll: 0.0,
            reason: "candidate not evaluated".to_string(),
        };
        let mut rng = rand::thread_rng();

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
                        .try_add(&c, &md, size_bytes, &loc, &[], seeder_floor)
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

        let (swapped, d) = self
            .try_swap(
                &c,
                &md,
                size_bytes,
                *held_count >= self.max_torrents,
                seeder_floor,
            )
            .await;
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

    async fn try_add(
        &mut self,
        c: &Candidate,
        md: &TorrentMeta,
        size_bytes: u64,
        location: &Path,
        displaced: &[Held],
        seeder_floor: u32,
    ) -> (bool, selector::SwapDecision) {
        let mut rng = rand::thread_rng();
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
            selector::roll(&mut rng),
        );
        if !decision.should_swap {
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
            md,
            &out_dir,
            keyed,
            &self.torrent_cache_path(&hex_str),
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
        }) {
            tracing::error!("failed to persist state for {}: {e:#}", c.title);
        }
        (true, decision)
    }

    async fn try_swap(
        &mut self,
        c: &Candidate,
        md: &TorrentMeta,
        size_bytes: u64,
        _ram_bound: bool,
        seeder_floor: u32,
    ) -> (bool, selector::SwapDecision) {
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
            let (ok, decision) = self
                .try_add(c, md, size_bytes, &location, &sel_held, seeder_floor)
                .await;
            if ok {
                for h in &displaced {
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
                    }
                }
                return (true, decision);
            } else {
                last = decision;
            }
        }
        (false, last)
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
        for h in held {
            if catalog_hashes.contains(&h.info_hash) {
                continue;
            }
            tracing::info!(
                "removing torrent no longer listed on Academic Torrents (title={})",
                h.title
            );
            let out_dir = engtorrents::torrent_output_dir(&h.storage_location, &h.info_hash);
            if let Err(e) = engtorrents::remove_torrent(&self.session, &h.info_hash, &out_dir).await
            {
                tracing::error!("failed to remove deleted torrent {}: {e:#}", h.title);
            }
            let _ = std::fs::remove_file(self.torrent_cache_path(&h.info_hash));
            if let Err(e) = self.state.remove(&h.info_hash) {
                tracing::error!("failed to drop state for {}: {e:#}", h.title);
            }
        }
    }

    async fn refresh_held_seeder_counts(&mut self, shutdown: &tokio::sync::watch::Receiver<bool>) {
        let held = self.state.all();
        let total = held.len();
        for (done, h) in held.into_iter().enumerate() {
            if *shutdown.borrow() {
                tracing::info!("held refresh interrupted by shutdown ({done}/{total})");
                break;
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
                    let progress = self.held_progress_bytes(&h);
                    let _ = self.state.update(&hex, |t| {
                        t.last_known_seeders = sw.seeders;
                        if progress > t.completed_pieces as u64 || t.last_progress_at.is_none() {
                            t.completed_pieces = progress.min(u32::MAX as u64) as u32;
                            t.last_progress_at = Some(Utc::now());
                        }
                    });
                }
                Err(e) => tracing::warn!("could not scrape held torrent {}: {e:#}", h.title),
            }
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
        for h in self.state.all() {
            if !catalog_hashes.contains(&h.info_hash) {
                continue;
            }
            if h.last_known_seeders > 0 {
                continue;
            }
            let Some(since) = h.last_progress_at.and_then(|t| (now - t).to_std().ok()) else {
                continue;
            };
            if since < timeout {
                continue;
            }
            tracing::warn!(
                "removing stalled torrent {}: zero seeders and no download progress for {}",
                h.title,
                crate::humanize::human_duration(since)
            );
            let out_dir = engtorrents::torrent_output_dir(&h.storage_location, &h.info_hash);
            if let Err(e) = engtorrents::remove_torrent(&self.session, &h.info_hash, &out_dir).await
            {
                tracing::error!("failed to remove stalled torrent {}: {e:#}", h.title);
            }
            let _ = std::fs::remove_file(self.torrent_cache_path(&h.info_hash));
            if let Err(e) = self.state.remove(&h.info_hash) {
                tracing::error!("failed to drop state for {}: {e:#}", h.title);
            }
        }
    }

    async fn resume_held(&mut self) -> Result<()> {
        let held = self.state.all();
        tracing::info!("resuming {} held torrents", held.len());
        for h in held {
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
                &md,
                &out_dir,
                keyed,
                &self.torrent_cache_path(&h.info_hash),
            )
            .await
            {
                tracing::warn!("skipping resume {}: {e:#}", h.info_hash);
            }
        }
        Ok(())
    }

    fn log_and_save_runtime(&self, kind: &str) {
        let held = self.state.all();
        let seeding = std::cell::Cell::new(0usize);
        self.session.with_torrents(|it| {
            for (_, h) in it {
                if h.stats().finished {
                    seeding.set(seeding.get() + 1);
                }
            }
        });
        let seeding = seeding.get();
        let locs: Vec<(PathBuf, u64)> = self
            .cfg
            .storage
            .iter()
            .map(|l| (l.path.clone(), l.limit_bytes()))
            .collect();
        let (used, limit) = engstats::disk_usage(&locs);
        let s = engstats::collect(
            &self.api,
            self.started_at,
            held.len(),
            seeding.min(held.len()),
            used,
            limit,
        );
        tracing::info!(
            "runtime stats (kind={kind} held={} seeding={} disk={}/{} up={} down={} peers={} rss={} uptime={}s)",
            s.held_torrents,
            s.seeding_torrents,
            crate::humanize::human_bytes(s.disk_used_bytes as i64),
            crate::humanize::human_bytes(s.disk_limit_bytes as i64),
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
/// after `resolve_all_limits`, so `limit: all` is already concrete bytes.
fn disk_limit_total(cfg: &Config) -> u64 {
    cfg.storage.iter().map(|l| l.limit_bytes()).sum()
}

fn shuffle<T>(v: &mut [T]) {
    use rand::seq::SliceRandom;
    v.shuffle(&mut rand::thread_rng());
}

fn count_pending(
    items: &[atcatalog::Item],
    held: &HashSet<String>,
    blocklist: &KeywordBlocklist,
    max_fittable: u64,
) -> u64 {
    let mut n = 0u64;
    for item in items {
        if held.contains(&hex::encode(item.info_hash)) {
            continue;
        }
        if blocklist.blocks(&item.title, &item.description).is_some() {
            continue;
        }
        if max_fittable > 0 && item.size_bytes > max_fittable {
            continue;
        }
        n += 1;
    }
    n
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

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredEntry {
    seeders: u32,
    leechers: u32,
    at: DateTime<Utc>,
}
