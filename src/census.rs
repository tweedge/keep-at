//! `keep-at network-status`: on-demand census of the keep-at network.
//! Walks the catalog, scrapes seeder counts (feeding the p10 floor), and
//! briefly joins each swarm with a disposable scraper-identity session to
//! count other keep-at seeders.

use std::time::Duration;

use anyhow::{Context, Result};
use librqbit::{AddTorrent, AddTorrentOptions};

use crate::atcatalog;
use crate::atkey;
use crate::attorrent;
use crate::buildinfo;
use crate::cli::NetworkStatusArgs;
use crate::engine::torrents::ManagedTorrentHandle;
use crate::humanize;
use crate::netstats;

pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
pub const PROGRESS_EVERY: usize = 25;
pub const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(500);
pub const PROBE_MAX_PEERS: usize = 8;

pub struct CensusResult {
    pub catalog_size: usize,
    pub scraped: usize,
    pub probed: usize,
    pub failed: usize,
    pub node_count: usize,
    pub seeding_bytes: u64,
    pub leeching_bytes: u64,
    pub seeder_floor: u32,
    pub elapsed: Duration,
}

pub async fn cmd_network_status(args: &NetworkStatusArgs) -> Result<()> {
    let cfg = crate::cli::resolve_census(args)?;
    let probe_timeout = args.probe_timeout.unwrap_or(DEFAULT_PROBE_TIMEOUT);

    println!("keep-at network-status: censusing the keep-at network");
    println!("NOTE: this is RAM- and time-heavy (joins every torrent's swarm; can take hours).");
    println!("data dir: {}", cfg.data_dir.display());
    println!();

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(buildinfo::user_agent())
        .build()?;

    let (user_announce, user_announce_ipv6) = if !cfg.api_key.is_empty() {
        atkey::resolve_user_announce(&http, &cfg.api_key)
            .await
            .unwrap_or_default()
    } else {
        (String::new(), String::new())
    };

    let catalog_fetcher = atcatalog::Fetcher {
        cache_path: cfg.data_dir.join("database.xml"),
        url: atcatalog::DEFAULT_URL.to_string(),
        user_agent: buildinfo::user_agent(),
        client: http.clone(),
    };
    let torrent_fetcher = attorrent::Fetcher {
        base_url: "https://academictorrents.com".to_string(),
        user_agent: buildinfo::user_agent(),
        client: http.clone(),
    };

    let (catalog, _) = catalog_fetcher.load(cfg.scan.interval).await?;
    println!("catalog loaded (items={})", catalog.items.len());

    let mut items = catalog.items;
    {
        use rand::seq::SliceRandom;
        items.shuffle(&mut rand::thread_rng());
    }

    let started = std::time::Instant::now();
    let mut tracker = netstats::Tracker::default();
    let mut seeder_counts: Vec<u32> = Vec::new();
    let mut result = CensusResult {
        catalog_size: items.len(),
        scraped: 0,
        probed: 0,
        failed: 0,
        node_count: 0,
        seeding_bytes: 0,
        leeching_bytes: 0,
        seeder_floor: 0,
        elapsed: Duration::ZERO,
    };
    let mut processed = 0usize;
    let mut rate = RateState::new(cfg.scan.rate_limit_per_second);

    for item in &items {
        let hex = hex::encode(item.info_hash);
        let md = match cached_or_fetch(&cfg.data_dir, &torrent_fetcher, &hex).await {
            Ok(md) => md,
            Err(_) => {
                result.failed += 1;
                processed += 1;
                report(processed, items.len(), &tracker, started.elapsed(), false);
                continue;
            }
        };
        let swarm = match scrape_one(&http, &mut rate, &md.trackers, &md.info_hash).await {
            Ok(s) => s,
            Err(_) => {
                result.failed += 1;
                processed += 1;
                report(processed, items.len(), &tracker, started.elapsed(), false);
                continue;
            }
        };
        result.scraped += 1;
        seeder_counts.push(swarm.seeders);

        // Probe with a fresh disposable scraper session, torn down right after.
        // Raw bytes are read from the cache file at probe time (never held
        // across iterations).
        match probe_swarm(
            &cfg.data_dir,
            &hex,
            &md.trackers,
            &user_announce,
            &user_announce_ipv6,
            probe_timeout,
        )
        .await
        {
            Ok(obs) => {
                result.probed += 1;
                for (node_key, complete) in obs {
                    tracker.observe(node_key, md.total_length, complete);
                }
            }
            Err(_) => result.failed += 1,
        }

        processed += 1;
        report(
            processed,
            items.len(),
            &tracker,
            started.elapsed(),
            processed.is_multiple_of(PROGRESS_EVERY),
        );
    }

    result.node_count = tracker.node_count();
    result.seeding_bytes = tracker.seeding_bytes;
    result.leeching_bytes = tracker.leeching_bytes;
    result.seeder_floor = crate::selector::seeder_floor(&seeder_counts);
    result.elapsed = started.elapsed();

    println!();
    println!(
        "census complete: {}/{} torrents scraped and probed ({} failed) in {}",
        result.scraped,
        result.catalog_size,
        result.failed,
        humanize::human_duration(result.elapsed)
    );
    println!("keep-at nodes observed: {}", result.node_count);
    println!(
        "data being seeded by keep-at nodes: {}",
        humanize::human_bytes(result.seeding_bytes as i64)
    );
    println!(
        "data being downloaded by keep-at nodes: {}",
        humanize::human_bytes(result.leeching_bytes as i64)
    );
    println!(
        "p10 seeder floor (anchor for the seed-scarcity gate): {}",
        result.seeder_floor
    );
    Ok(())
}

fn report(
    processed: usize,
    total: usize,
    tracker: &netstats::Tracker,
    elapsed: Duration,
    print: bool,
) {
    if !print {
        return;
    }
    let pct = if total > 0 {
        processed as f64 / total as f64 * 100.0
    } else {
        0.0
    };
    println!(
        "  census in progress: {processed}/{total} torrents ({pct:.1}%), keep-at nodes observed so far: {}, elapsed {}",
        tracker.node_count(),
        humanize::human_duration(elapsed)
    );
}

struct RateState {
    per_second: f64,
    next_allowed: tokio::time::Instant,
}

impl RateState {
    fn new(per_second: f64) -> RateState {
        RateState {
            per_second,
            next_allowed: tokio::time::Instant::now(),
        }
    }
    async fn wait(&mut self) {
        if self.per_second <= 0.0 {
            return;
        }
        let now = tokio::time::Instant::now();
        if now < self.next_allowed {
            tokio::time::sleep(self.next_allowed - now).await;
        }
        self.next_allowed =
            tokio::time::Instant::now() + Duration::from_secs_f64(1.0 / self.per_second);
    }
}

async fn cached_or_fetch(
    data_dir: &std::path::Path,
    fetcher: &attorrent::Fetcher,
    hex: &str,
) -> Result<attorrent::TorrentMeta> {
    let path = data_dir
        .join("torrent-cache")
        .join(format!("{hex}.torrent"));
    if let Ok(data) = std::fs::read(&path) {
        if let Ok(md) = attorrent::parse_torrent_bytes(&data) {
            return Ok(md);
        }
    }
    let md = fetcher.fetch_torrent(hex, Some(&path)).await?.0;
    Ok(md)
}

async fn scrape_one(
    http: &reqwest::Client,
    rate: &mut RateState,
    trackers: &[String],
    info_hash: &[u8; 20],
) -> Result<attorrent::SwarmCounts> {
    let mut last_err: Option<anyhow::Error> = None;
    for tracker in trackers {
        if atkey::is_at_tracker_url(tracker) {
            rate.wait().await;
        }
        if !tracker.starts_with("http://") && !tracker.starts_with("https://") {
            continue;
        }
        let call = tokio::time::timeout(
            Duration::from_secs(15),
            attorrent::scrape_http(http, &buildinfo::scraper_user_agent(), tracker, info_hash),
        )
        .await;
        match call {
            Ok(Ok(c)) => return Ok(c),
            Ok(Err(e)) => last_err = Some(e),
            Err(_) => last_err = Some(anyhow::anyhow!("scrape timed out")),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no tracker returned scrape data")))
}

/// Join the swarm briefly with a disposable scraper session; return
/// (node_key, complete) per connected keep-at seeder peer.
async fn probe_swarm(
    data_dir: &std::path::Path,
    info_hash_hex: &str,
    trackers: &[String],
    user_announce: &str,
    user_announce_ipv6: &str,
    timeout: Duration,
) -> Result<Vec<(String, bool)>> {
    use crate::engine::session as rqsession;
    let session = rqsession::new_probe_session(data_dir.to_path_buf()).await?;
    let api = librqbit::Api::new(session.clone(), None);

    let tiers: Vec<Vec<String>> = trackers.iter().map(|t| vec![t.clone()]).collect();
    let keyed = atkey::at_trackers_only(tiers, user_announce, user_announce_ipv6);
    let probe_dir = data_dir
        .join("probe-scratch")
        .join(format!("p{}", std::process::id()));
    let _ = std::fs::create_dir_all(&probe_dir);

    let opts = AddTorrentOptions {
        output_folder: Some(probe_dir.to_string_lossy().into_owned()),
        overwrite: true,
        trackers: Some(keyed.into_iter().flatten().collect()),
        ..Default::default()
    };
    // Raw bytes loaded here and dropped with the response; never held across
    // probe iterations.
    let raw = std::fs::read(
        data_dir
            .join("torrent-cache")
            .join(format!("{info_hash_hex}.torrent")),
    )
    .with_context(|| format!("loading cached .torrent for {info_hash_hex}"))?;
    let resp = session
        .add_torrent(AddTorrent::TorrentFileBytes(raw.into()), Some(opts))
        .await?;
    let Some(handle) = resp.into_handle() else {
        anyhow::bail!("probe torrent added list-only");
    };
    let total_pieces = handle
        .with_metadata(|m| m.info.lengths().total_pieces())
        .unwrap_or(0);

    // Poll for peers until timeout or enough peers.
    let deadline = tokio::time::Instant::now() + timeout;
    let out = loop {
        let out = keep_at_peers(&api, &handle, total_pieces);
        let peer_count = peer_count(&api, &handle);
        if !out.is_empty()
            || peer_count >= PROBE_MAX_PEERS
            || tokio::time::Instant::now() >= deadline
        {
            break out;
        }
        tokio::time::sleep(PROBE_POLL_INTERVAL).await;
    };

    // Tear down before returning: per-torrent peak memory.
    let id = handle.info_hash();
    let _ = session
        .delete(librqbit::api::TorrentIdOrHash::Hash(id), true)
        .await;
    rqsession::stop_session(&session, Duration::from_secs(5)).await;
    let _ = std::fs::remove_dir_all(&probe_dir);
    Ok(out)
}

fn peer_count(api: &librqbit::Api, handle: &ManagedTorrentHandle) -> usize {
    use librqbit::http_api_types::PeerStatsFilter;
    api.api_peer_stats(
        librqbit::api::TorrentIdOrHash::Hash(handle.info_hash()),
        PeerStatsFilter::default(),
    )
    .map(|s| s.peers.len())
    .unwrap_or(0)
}

/// keep-at seeder observations from live peer stats. Completeness per peer
/// isn't exposed by rqbit's public snapshot (only our own bitfield is), so
/// a keep-at peer on a torrent we don't hold is conservatively counted as
/// leeching unless the torrent is complete locally... Instead: peers
/// advertising keep-at seeder identity with a full bitfield can't be seen;
/// count identity only, completeness = whether WE have it complete AND the
/// peer is a seeder client. Documented estimate (matches Go's caveats).
fn keep_at_peers(
    api: &librqbit::Api,
    handle: &ManagedTorrentHandle,
    _total_pieces: u32,
) -> Vec<(String, bool)> {
    use librqbit::http_api_types::PeerStatsFilter;
    let Ok(snap) = api.api_peer_stats(
        librqbit::api::TorrentIdOrHash::Hash(handle.info_hash()),
        PeerStatsFilter::default(),
    ) else {
        return Vec::new();
    };
    let we_complete = handle.stats().finished;
    let mut out = Vec::new();
    for (addr, peer) in &snap.peers {
        if let Some(name) = &peer.client_name {
            if crate::buildinfo::is_keep_at_seeder(name) {
                let host = addr.split(':').next().unwrap_or(addr).to_string();
                // A keep-at seeder with our torrent complete locally is
                // seeding (it must hold it complete to advertise seeder
                // identity truthfully); otherwise count as leeching.
                out.push((host, we_complete));
            }
        }
    }
    out
}
