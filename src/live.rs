//! Live read-only queries against the running daemon over a Unix socket.
//!
//! `status` and `hosted-torrents` used to read periodic snapshot files
//! (`runtime-stats.json`) plus on-disk dir sizes. Snapshots go stale between
//! writes (up to `stats_interval`), and the dir-size heuristic misreports a
//! just-added sparse torrent as fully downloaded. The running Engine already
//! holds the truth in-process (session handles + state), so it serves it
//! directly here instead.
//!
//! Transport: `<data_dir>/keep-at.sock`, mode 0o666 (any local user can
//! query; same-host-only by construction — no TCP port, no auth problem).
//! Protocol: one JSON request line, one JSON response line. Two read-only
//! verbs, no mutation surface:
//! - `{"op":"runtime"}` -> [`RuntimeView`] (instantaneous counters)
//! - `{"op":"held"}` -> [`HeldView`] (per-torrent live progress)
//!
//! Clients connect with a short timeout; any failure means "not running"
//! and the caller falls back to the file path unchanged.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Socket filename inside the data dir.
pub const SOCKET_NAME: &str = "keep-at.sock";

/// Client timeout: the daemon answers from in-memory state, so anything
/// slower means wedged or gone — fall back to files.
pub const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SOCKET_NAME)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    Runtime,
    Held,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeView {
    pub uptime_seconds: u64,
    pub held_torrents: usize,
    pub seeding_torrents: usize,
    pub downloading_torrents: usize,
    pub disk_used_bytes: u64,
    pub disk_limit_bytes: u64,
    pub total_bytes_uploaded: u64,
    pub total_bytes_downloaded: u64,
    /// Bytes per second over the past hour (rolling, live-only).
    pub upload_bps_hour: f64,
    pub download_bps_hour: f64,
    /// Bytes per second over the past day (rolling, live-only).
    pub upload_bps_day: f64,
    pub download_bps_day: f64,
    pub active_peers: usize,
    pub process_rss_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeldTorrentView {
    pub title: String,
    pub info_hash: String,
    pub size_bytes: u64,
    /// Verified bytes present (rqbit progress_bytes for managed torrents).
    pub progress_bytes: u64,
    /// True when the session reports the torrent complete.
    pub finished: bool,
    pub last_known_seeders: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeldView {
    pub torrents: Vec<HeldTorrentView>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "ok", content = "data")]
pub enum Response {
    #[serde(rename = "runtime")]
    Runtime(RuntimeView),
    #[serde(rename = "held")]
    Held(HeldView),
    #[serde(rename = "error")]
    Error(String),
}

/// Query the running daemon. Ok(None) = no live daemon (socket absent,
/// refused, wedged, or garbage) — caller falls back to files.
pub fn query(data_dir: &Path, req: &Request) -> Option<Response> {
    let path = socket_path(data_dir);
    let mut stream = std::os::unix::net::UnixStream::connect(&path).ok()?;
    stream
        .set_read_timeout(Some(QUERY_TIMEOUT))
        .and_then(|_| stream.set_write_timeout(Some(QUERY_TIMEOUT)))
        .ok()?;
    let mut line = serde_json::to_string(req).ok()?;
    line.push('\n');
    use std::io::Write;
    stream.write_all(line.as_bytes()).ok()?;
    stream.flush().ok()?;
    let mut reader = std::io::BufReader::new(&stream);
    use std::io::BufRead;
    let mut resp = String::new();
    reader.read_line(&mut resp).ok()?;
    if resp.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&resp).ok()
}

/// Serve forever on `data_dir/keep-at.sock`, answering from `handle`.
/// Runs until the process exits (spawned task, never joined). A stale
/// socket file from a dead daemon is unlinked first (liveness is checked
/// by attempting a connect: refusal = stale).
pub async fn serve(data_dir: PathBuf, handle: LiveHandle) {
    let path = socket_path(&data_dir);
    // Stale socket: connect fails => previous owner dead => unlink.
    if path.exists() && std::os::unix::net::UnixStream::connect(&path).is_err() {
        let _ = std::fs::remove_file(&path);
    }
    let listener = match tokio::net::UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(
                "live query socket unavailable ({}): {e:#}; status/hosted-torrents fall back to files",
                path.display()
            );
            return;
        }
    };
    // Any local user queries (status/hosted-torrents run as anyone).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666));
    }
    tracing::info!("live query socket listening ({})", path.display());
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let handle = handle.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_one(stream, &handle).await {
                tracing::debug!("live query failed: {e:#}");
            }
        });
    }
}

async fn serve_one(stream: tokio::net::UnixStream, handle: &LiveHandle) -> Result<()> {
    let stream = stream.into_std().context("into std stream")?;
    stream.set_read_timeout(Some(QUERY_TIMEOUT))?;
    stream.set_write_timeout(Some(QUERY_TIMEOUT))?;
    let mut reader = std::io::BufReader::new(&stream);
    use std::io::{BufRead, Write};
    let mut line = String::new();
    reader.read_line(&mut line).context("reading request")?;
    let req: Request = serde_json::from_str(line.trim()).context("parsing request")?;
    let resp: Response = match req {
        Request::Runtime => Response::Runtime(handle.runtime_view()),
        Request::Held => Response::Held(handle.held_view()),
    };
    let mut out = serde_json::to_string(&resp).context("marshalling response")?;
    out.push('\n');
    (&stream)
        .write_all(out.as_bytes())
        .context("writing response")?;
    (&stream).flush().context("flushing response")?;
    Ok(())
}

/// Read-only view into the running Engine, shared with the socket server
/// task. The Engine pushes a fresh snapshot after every state mutation
/// (put/update/remove); the server reads it without touching the scan.
/// Session/API stay behind their original Arcs (both Clone + Send + Sync).
#[derive(Clone)]
pub struct LiveHandle {
    session: std::sync::Arc<librqbit::Session>,
    api: librqbit::Api,
    state: std::sync::Arc<parking_lot::RwLock<StateSnapshot>>,
    started_at: std::time::Instant,
    storage: Vec<(PathBuf, u64)>,
    /// Rolling bandwidth history (hour/day rates). Shared with the socket
    /// server clones; internal Mutex, sub-microsecond critical sections.
    tracker: std::sync::Arc<std::sync::Mutex<crate::bandwidth::RateTracker>>,
}

/// State snapshot pushed by the Engine after every mutation. Plain data:
/// no locks held across await points on the Engine side, no scan blocking
/// on the server side.
#[derive(Debug, Clone, Default)]
pub struct StateSnapshot {
    pub torrents: Vec<StateEntry>,
}

#[derive(Debug, Clone)]
pub struct StateEntry {
    pub title: String,
    pub info_hash: String,
    pub size_bytes: u64,
    pub last_known_seeders: u32,
}

impl LiveHandle {
    pub fn new(
        session: std::sync::Arc<librqbit::Session>,
        api: librqbit::Api,
        torrents: Vec<StateEntry>,
        started_at: std::time::Instant,
        storage: Vec<(PathBuf, u64)>,
    ) -> LiveHandle {
        // Baseline counters at handle creation (session near-zero at boot).
        let snap = api.api_session_stats();
        let tracker = crate::bandwidth::RateTracker::new(
            started_at,
            snap.counters.uploaded_bytes,
            snap.counters.fetched_bytes,
        );
        LiveHandle {
            session,
            api,
            state: std::sync::Arc::new(parking_lot::RwLock::new(StateSnapshot { torrents })),
            started_at,
            storage,
            tracker: std::sync::Arc::new(std::sync::Mutex::new(tracker)),
        }
    }

    /// Advance the bandwidth history with the current session counters.
    /// Called on the periodic stats cadence and from every runtime query —
    /// the event log is exact under any cadence.
    pub fn tick_tracker(&self) {
        let sess = self.api.api_session_stats();
        let now = std::time::Instant::now();
        if let Ok(mut tr) = self.tracker.try_lock() {
            tr.tick(
                now,
                sess.counters.uploaded_bytes,
                sess.counters.fetched_bytes,
            );
        }
    }

    /// Refresh after a state mutation. Called by the Engine (the sole
    /// writer); cheap clone of the held list.
    pub fn refresh(&self, torrents: Vec<StateEntry>) {
        *self.state.write() = StateSnapshot { torrents };
    }

    fn runtime_view(&self) -> RuntimeView {
        let snap = self.state.read();
        let held = snap.torrents.len();
        let seeding = std::cell::Cell::new(0usize);
        self.session.with_torrents(|it| {
            for (_, h) in it {
                if h.stats().finished {
                    seeding.set(seeding.get() + 1);
                }
            }
        });
        let seeding = seeding.get().min(held);
        let (used, limit) = crate::engine::stats::disk_usage(&self.storage);
        // Tick before reading: this query is also a sample point.
        self.tick_tracker();
        let (up_hour, down_hour, up_day, down_day) = {
            let now = std::time::Instant::now();
            match self.tracker.try_lock() {
                Ok(tr) => (
                    tr.rate_up(now, std::time::Duration::from_secs(3600)),
                    tr.rate_down(now, std::time::Duration::from_secs(3600)),
                    tr.rate_up(now, std::time::Duration::from_secs(86_400)),
                    tr.rate_down(now, std::time::Duration::from_secs(86_400)),
                ),
                Err(_) => (0.0, 0.0, 0.0, 0.0),
            }
        };
        let sess = self.api.api_session_stats();
        RuntimeView {
            uptime_seconds: self.started_at.elapsed().as_secs(),
            held_torrents: held,
            seeding_torrents: seeding,
            downloading_torrents: held.saturating_sub(seeding),
            disk_used_bytes: used,
            disk_limit_bytes: limit,
            total_bytes_uploaded: sess.counters.uploaded_bytes,
            total_bytes_downloaded: sess.counters.fetched_bytes,
            upload_bps_hour: up_hour,
            download_bps_hour: down_hour,
            upload_bps_day: up_day,
            download_bps_day: down_day,
            active_peers: sess.peers.live as usize,
            process_rss_bytes: crate::engine::stats::process_rss_bytes(),
        }
    }

    fn held_view(&self) -> HeldView {
        let snap = self.state.read();
        // Live progress per managed handle, keyed by infohash.
        let live = std::cell::RefCell::new(std::collections::HashMap::new());
        self.session.with_torrents(|it| {
            for (_, h) in it {
                let st = h.stats();
                live.borrow_mut()
                    .insert(h.info_hash().as_string(), (st.progress_bytes, st.finished));
            }
        });
        let live = live.borrow();
        let mut torrents: Vec<HeldTorrentView> = snap
            .torrents
            .iter()
            .map(|t| {
                let (progress, finished) = live.get(&t.info_hash).copied().unwrap_or((0, false));
                HeldTorrentView {
                    title: t.title.clone(),
                    info_hash: t.info_hash.clone(),
                    size_bytes: t.size_bytes,
                    progress_bytes: progress,
                    finished,
                    last_known_seeders: t.last_known_seeders,
                }
            })
            .collect();
        torrents.sort_by(|a, b| a.title.cmp(&b.title));
        HeldView { torrents }
    }
}
