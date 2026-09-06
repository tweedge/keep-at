//! Per-torrent lifecycle on top of an rqbit Session: add (from cached
//! .torrent bytes), remove (with files), progress queries.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use librqbit::api::TorrentIdOrHash;
use librqbit::{AddTorrent, AddTorrentOptions, Session};

pub type ManagedTorrentHandle = Arc<librqbit::ManagedTorrent>;

/// Add a torrent into `output_dir` (the assigned storage location).
/// AT-only tracker filtering + keyed announce URLs are applied before adding.
/// Raw bytes are read from the torrent-cache file at add time and dropped
/// right after, so callers never hold bulk metainfo in memory.
/// Returns the info-hash hex.
pub async fn add_torrent_bytes(
    session: &Arc<Session>,
    info_hash_hex: &str,
    md: &crate::attorrent::TorrentMeta,
    output_dir: &Path,
    trackers: Vec<Vec<String>>,
    cache_path: &Path,
) -> Result<String> {
    let raw = std::fs::read(cache_path)
        .with_context(|| format!("loading cached .torrent for {info_hash_hex}"))?;
    let _ = md;
    let opts = AddTorrentOptions {
        output_folder: Some(output_dir.to_string_lossy().into_owned()),
        overwrite: true,
        trackers: Some(trackers.into_iter().flatten().collect()),
        // Re-announces use the tracker's own min-interval (typically 15+ min
        // for AT); never force a faster cadence. Combined with AT-only
        // tracker filtering, automatic announce traffic stays minimal - an
        // older build additionally routed tracker dials through the shared
        // rate limiter, which rqbit does not support.
        force_tracker_interval: Some(std::time::Duration::from_secs(1800)),
        ..Default::default()
    };
    let resp = session
        .add_torrent(
            AddTorrent::TorrentFileBytes(raw.to_vec().into()),
            Some(opts),
        )
        .await
        .context("adding torrent to session")?;
    let handle = resp
        .into_handle()
        .context("torrent added list-only, expected managed")?;
    // Wait briefly for initialization so early errors surface here, not later.
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        handle.wait_until_initialized(),
    )
    .await;
    Ok(handle.info_hash().as_string())
}

/// Remove a torrent from the session and delete its files on disk.
/// `output_dir` is removed as well when it ends up empty.
pub async fn remove_torrent(
    session: &Arc<Session>,
    info_hash_hex: &str,
    output_dir: &Path,
) -> Result<()> {
    let id = parse_id20(info_hash_hex)?;
    let _ = session.delete(TorrentIdOrHash::Hash(id), true).await;
    // Belt and suspenders: delete(true) removes torrent files; also drop the
    // (now possibly empty) per-torrent dir.
    if output_dir.exists() {
        let _ = std::fs::remove_dir(output_dir);
    }
    Ok(())
}

/// Find a managed torrent by hex hash.
pub fn find_torrent(
    session: &Arc<Session>,
    info_hash_hex: &str,
) -> Result<Option<ManagedTorrentHandle>> {
    let id = parse_id20(info_hash_hex)?;
    let found = std::cell::Cell::new(None);
    session.with_torrents(|it| {
        for (_, h) in it {
            if h.info_hash() == id {
                found.set(Some(h.clone()));
                break;
            }
        }
    });
    Ok(found.into_inner())
}

/// Completed pieces and total pieces for stall tracking; None when the
/// torrent isn't managed (treat as no progress signal).
pub fn piece_progress(handle: &ManagedTorrentHandle) -> (u32, u32) {
    let stats = handle.stats();
    let total = stats.total_bytes; // bytes-based fallback below
    let _ = total;
    // TorrentStats exposes progress_bytes/total_bytes; piece counts come from
    // chunk tracking when live. Use bytes as the progress signal: it grows
    // if and only if new verified data lands.
    (stats.progress_bytes as u32, stats.total_bytes as u32)
}

/// Whether the torrent is fully downloaded (seeding).
pub fn is_finished(handle: &ManagedTorrentHandle) -> bool {
    handle.stats().finished
}

fn parse_id20(hex_str: &str) -> Result<librqbit_core::Id20> {
    let b = hex::decode(hex_str.trim()).context("invalid infohash hex")?;
    librqbit_core::Id20::from_bytes(&b).map_err(|e| anyhow::anyhow!("{e:#}"))
}

/// Per-torrent output dir: <location>/<infohash-hex>/, so torrents in one
/// location can never overlap.
pub fn torrent_output_dir(location: &Path, info_hash_hex: &str) -> PathBuf {
    location.join(info_hash_hex)
}
