//! Live read-only queries against the running daemon over a Unix socket.
//!
//! `status` and `hosted-torrents` used to read periodic snapshot files
//! (`runtime-stats.json`) plus on-disk dir sizes. Snapshots go stale between
//! writes (up to `stats_interval`), and the dir-size heuristic misreports a
//! just-added sparse torrent as fully downloaded. The running Engine serves
//! the truth here instead.
//!
//! Transport: `<data_dir>/keep-at.sock`, mode 0o666 (any local user can
//! query; same-host-only by construction — no TCP port, no auth problem).
//! Protocol: one JSON request line, one JSON response line. Two read-only
//! verbs, no mutation surface:
//! - `{"op":"runtime"}` -> [`RuntimeView`] (instantaneous counters)
//! - `{"op":"held"}` -> [`HeldView`] (per-torrent live progress)
//!
//! The handle is TWO-PHASE: `LiveHandle::booting` binds the socket at the
//! very start of `run` — before `Engine::new` finishes resuming torrents,
//! which on slow hosts takes minutes — and serves a booting view built from
//! `state.json`. `activate` promotes it to the live engine once the session
//! exists. Without this, `status` reported "may need a restart" for the
//! whole boot window: the pid file existed while the socket didn't.
//!
//! Clients connect with a short timeout; any failure means "not running"
//! and the caller falls back to the file path unchanged.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// What the Engine is doing right now. Surfaced by `status` as a
/// human-readable state line so a booting node isn't mistaken for a broken
/// one and 135 "downloading" torrents aren't mistaken for actual transfers
/// (during boot they are being integrity-checked, not downloaded).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    /// Engine::new is still resuming held torrents (socket serves booting
    /// views built from state.json).
    Booting,
    /// Inter-scan sleep: holding and seeding, nothing else running.
    Seeding,
    /// A catalog scan is in progress (fetch/scrape/evaluate/refresh).
    Scanning,
}

impl Activity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Activity::Booting => "booting",
            Activity::Seeding => "seeding",
            Activity::Scanning => "scanning",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeView {
    /// True while the Engine is still resuming torrents (socket bound, but
    /// the session isn't serving yet). `status` shows a starting-up line
    /// instead of implying a stale or broken daemon.
    pub booting: bool,
    pub activity: Activity,
    /// Torrents currently running an initial integrity check (rqbit
    /// Initializing state). High during boot, 0 in steady state.
    pub checks_in_progress: usize,
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
    /// True while the torrent's initial integrity check is running
    /// (rqbit Initializing state) or the daemon is still booting — the
    /// display should say "verifying", not "downloading".
    pub verifying: bool,
    pub last_known_seeders: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeldView {
    /// True while the daemon is booting: per-torrent progress is not yet
    /// known (sessions still initializing), every unfinished torrent shows
    /// as verifying.
    pub booting: bool,
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
///
/// Absent/refused connections are the normal not-running case and stay
/// silent; anything else (connected but the write/read/parse failed) is
/// logged at warn, because a running daemon that doesn't answer is exactly
/// what used to hide as silent fallbacks.
pub fn query(data_dir: &Path, req: &Request) -> Option<Response> {
    let path = socket_path(data_dir);
    let mut stream = match std::os::unix::net::UnixStream::connect(&path) {
        Ok(s) => s,
        // The daemon genuinely isn't there: normal, never logged.
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                || e.kind() == std::io::ErrorKind::ConnectionRefused =>
        {
            return None;
        }
        Err(e) => {
            tracing::warn!("live query: connect to {} failed: {e}", path.display());
            return None;
        }
    };
    if stream
        .set_read_timeout(Some(QUERY_TIMEOUT))
        .and_then(|_| stream.set_write_timeout(Some(QUERY_TIMEOUT)))
        .is_err()
    {
        tracing::warn!("live query: could not set timeouts on {}", path.display());
        return None;
    }
    let mut line = serde_json::to_string(req).ok()?;
    line.push('\n');
    use std::io::{BufRead, Write};
    if let Err(e) = stream.write_all(line.as_bytes()).and_then(|_| stream.flush()) {
        tracing::warn!(
            "live query: {} accepted the connection but the write failed: {e}",
            path.display()
        );
        return None;
    }
    let mut reader = std::io::BufReader::new(&stream);
    let mut resp = String::new();
    if let Err(e) = reader.read_line(&mut resp) {
        tracing::warn!("live query: {} read failed: {e}", path.display());
        return None;
    }
    if resp.trim().is_empty() {
        // EOF or blank line: the daemon closed the connection without
        // answering — the signature of a server-side query bug.
        tracing::warn!(
            "live query: {} closed the connection without answering",
            path.display()
        );
        return None;
    }
    match serde_json::from_str(&resp) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(
                "live query: {} returned unparseable data: {e} (line: {:.200})",
                path.display(),
                resp.trim()
            );
            None
        }
    }
}

/// How long `serve` keeps retrying the bind before giving up (transient
/// races with a dying previous generation; the serve task also runs on the
/// shared runtime and can be starved until well after boot).
const BIND_RETRY_WINDOW: Duration = Duration::from_secs(30);
/// Sleep between bind attempts. One connect+bind syscall pair per attempt.
const BIND_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Serve forever on `data_dir/keep-at.sock`, answering from `handle`.
/// Runs until the process exits (spawned task, never joined). A stale
/// socket file from a dead daemon is unlinked first (liveness is checked
/// by attempting a connect: refusal = stale). Bind failures are retried
/// for [`BIND_RETRY_WINDOW`]: a single failure used to permanently disable
/// live queries for the whole daemon lifetime (observed once — the bind
/// lost a race around a previous generation's socket).
pub async fn serve(data_dir: PathBuf, handle: LiveHandle) {
    let path = socket_path(&data_dir);
    // Stale socket: connect fails => previous owner dead => unlink.
    if path.exists() && std::os::unix::net::UnixStream::connect(&path).is_err() {
        let _ = std::fs::remove_file(&path);
    }
    let deadline = Instant::now() + BIND_RETRY_WINDOW;
    let listener = loop {
        match tokio::net::UnixListener::bind(&path) {
            Ok(l) => break l,
            Err(e) if Instant::now() < deadline => {
                // Present-but-unowned socket (previous owner died since the
                // probe above, or lost the unlink race): unlink and retry.
                if std::os::unix::net::UnixStream::connect(&path).is_err() {
                    let _ = std::fs::remove_file(&path);
                }
                tracing::debug!("live query socket bind retry ({}): {e:#}", path.display());
                tokio::time::sleep(BIND_RETRY_INTERVAL).await;
            }
            Err(e) => {
                tracing::warn!(
                    "live query socket unavailable ({}): {e:#}; status/hosted-torrents fall back to files",
                    path.display()
                );
                return;
            }
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
    // Async I/O with a real timeout. Do NOT convert to a std socket and
    // block: into_std() hands back an fd still in O_NONBLOCK mode, so a
    // std read issued before the client's request bytes arrive returns
    // EAGAIN (not "wait") and the connection is dropped instantly — every
    // query that lost the write-vs-read race failed at ~0s with EPIPE/EOF
    // on the client side (observed ~50% of status calls on a
    // CPU-constrained host). Blocking I/O would also hold a runtime worker
    // for up to the full timeout on a slow client, stalling the daemon.
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut stream = stream;
    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    tokio::time::timeout(QUERY_TIMEOUT, reader.read_line(&mut line))
        .await
        .context("reading request (client timed out)")?
        .context("reading request")?;
    let req: Request = serde_json::from_str(line.trim()).context("parsing request")?;
    let resp: Response = match req {
        Request::Runtime => Response::Runtime(handle.runtime_view()),
        Request::Held => Response::Held(handle.held_view()),
    };
    let mut out = serde_json::to_string(&resp).context("marshalling response")?;
    out.push('\n');
    tokio::time::timeout(QUERY_TIMEOUT, writer.write_all(out.as_bytes()))
        .await
        .context("writing response (client timed out)")??;
    writer.flush().await.context("flushing response")?;
    Ok(())
}

/// Everything the live (post-boot) view needs from the Engine.
struct LiveState {
    session: Arc<librqbit::Session>,
    api: librqbit::Api,
    tracker: Arc<std::sync::Mutex<crate::bandwidth::RateTracker>>,
    started_at: Instant,
    storage: Vec<(PathBuf, u64)>,
    /// Held-torrent snapshot pushed by the Engine after every mutation.
    held: std::sync::Mutex<Vec<StateEntry>>,
}

/// Read-only view into the running Engine, shared with the socket server
/// task. Two-phase: Booting from `LiveHandle::booting` (socket bound before
/// the Engine exists), promoted by `activate` once the session is up.
#[derive(Clone)]
pub struct LiveHandle {
    inner: Arc<parking_lot::RwLock<HandleInner>>,
    activity: Arc<std::sync::Mutex<Activity>>,
}

enum HandleInner {
    Booting {
        started_at: Instant,
        storage: Vec<(PathBuf, u64)>,
        held: Vec<HeldTorrentView>,
    },
    Live(Box<LiveState>),
}

impl LiveHandle {
    /// Booting-phase handle: binds immediately, serves held-torrent truth
    /// from `state.json` (progress unknown, everything verifying) until
    /// `activate` promotes the view.
    pub fn booting(
        started_at: Instant,
        storage: Vec<(PathBuf, u64)>,
        held: Vec<HeldTorrentView>,
    ) -> LiveHandle {
        LiveHandle {
            inner: Arc::new(parking_lot::RwLock::new(HandleInner::Booting {
                started_at,
                storage,
                held,
            })),
            activity: Arc::new(std::sync::Mutex::new(Activity::Booting)),
        }
    }

    /// Live-phase handle directly (seam used by Engine fallbacks and tests):
    /// same as booting + activate.
    pub fn for_tests(
        session: Arc<librqbit::Session>,
        api: librqbit::Api,
        torrents: Vec<StateEntry>,
        started_at: Instant,
        storage: Vec<(PathBuf, u64)>,
    ) -> LiveHandle {
        let h = LiveHandle::booting(started_at, storage, Vec::new());
        h.activate(session, api, torrents);
        h
    }

    /// Promote to the live engine view. Called once, after the session
    /// exists; later queries serve real per-torrent progress.
    pub fn activate(
        &self,
        session: Arc<librqbit::Session>,
        api: librqbit::Api,
        torrents: Vec<StateEntry>,
    ) {
        let (started_at, storage) = {
            let inner = self.inner.write();
            match &*inner {
                HandleInner::Booting {
                    started_at,
                    storage,
                    ..
                } => (*started_at, storage.clone()),
                HandleInner::Live(_) => return, // already live
            }
        };
        // Baseline counters now (session counters are cumulative since
        // session creation; the tracker only needs deltas from here on).
        let snap = api.api_session_stats();
        let tracker = Arc::new(std::sync::Mutex::new(crate::bandwidth::RateTracker::new(
            started_at,
            snap.counters.uploaded_bytes,
            snap.counters.fetched_bytes,
        )));
        *self.inner.write() = HandleInner::Live(Box::new(LiveState {
            session,
            api,
            tracker,
            started_at,
            storage,
            held: std::sync::Mutex::new(torrents),
        }));
        self.set_activity(Activity::Seeding);
    }

    /// Replace the held-torrent snapshot (called by the Engine after every
    /// state mutation). During booting the list is static; refresh still
    /// merges (keeping verifying=true) in case state changes early.
    pub fn refresh(&self, torrents: Vec<StateEntry>) {
        let mut inner = self.inner.write();
        match &mut *inner {
            HandleInner::Booting { held, .. } => {
                *held = torrents
                    .into_iter()
                    .map(|t| HeldTorrentView {
                        title: t.title,
                        info_hash: t.info_hash,
                        size_bytes: t.size_bytes,
                        progress_bytes: 0,
                        finished: false,
                        verifying: true,
                        last_known_seeders: t.last_known_seeders,
                    })
                    .collect();
            }
            HandleInner::Live(live) => {
                *live.held.lock().unwrap() = torrents;
            }
        }
    }

    /// Set what the Engine is doing (Booting/Seeding/Scanning).
    pub fn set_activity(&self, a: Activity) {
        *self.activity.lock().unwrap() = a;
    }

    /// Advance the bandwidth history with the current session counters.
    /// Called on the periodic stats cadence and from every runtime query —
    /// the event log is exact under any cadence. No-op while booting.
    pub fn tick_tracker(&self) {
        let (api, tracker) = {
            let inner = self.inner.read();
            match &*inner {
                HandleInner::Live(live) => (live.api.clone(), live.tracker.clone()),
                HandleInner::Booting { .. } => return,
            }
        };
        let sess = api.api_session_stats();
        let now = Instant::now();
        let Ok(mut tr) = tracker.try_lock() else {
            return;
        };
        tr.tick(
            now,
            sess.counters.uploaded_bytes,
            sess.counters.fetched_bytes,
        );
    }

    fn runtime_view(&self) -> RuntimeView {
        let activity = *self.activity.lock().unwrap();
        // Snapshot what's needed, then drop the inner lock.
        enum Phase {
            Booting {
                held_len: usize,
                storage: Vec<(PathBuf, u64)>,
            },
            Live {
                session: Arc<librqbit::Session>,
                api: librqbit::Api,
                tracker: Arc<std::sync::Mutex<crate::bandwidth::RateTracker>>,
                storage: Vec<(PathBuf, u64)>,
                held_len: usize,
            },
        }
        let (phase, started_at) = {
            let inner = self.inner.read();
            let started = match &*inner {
                HandleInner::Booting { started_at, .. } => *started_at,
                HandleInner::Live(live) => live.started_at,
            };
            let phase = match &*inner {
                HandleInner::Booting { held, storage, .. } => Phase::Booting {
                    held_len: held.len(),
                    storage: storage.clone(),
                },
                HandleInner::Live(live) => Phase::Live {
                    held_len: live.held.lock().unwrap().len(),
                    session: live.session.clone(),
                    api: live.api.clone(),
                    tracker: live.tracker.clone(),
                    storage: live.storage.clone(),
                },
            };
            (phase, started)
        };

        let (seeding, downloading, checks, totals, peers) = match &phase {
            Phase::Live { session, api, .. } => {
                let seeding = std::cell::Cell::new(0usize);
                let checks = std::cell::Cell::new(0usize);
                let downloading = std::cell::Cell::new(0usize);
                session.with_torrents(|it| {
                    for (_, h) in it {
                        let finished = h.stats().finished;
                        let initializing = h.with_state(|s| {
                            matches!(s, librqbit::ManagedTorrentState::Initializing(_))
                        });
                        if finished {
                            seeding.set(seeding.get() + 1);
                        } else if initializing {
                            checks.set(checks.get() + 1);
                        } else {
                            downloading.set(downloading.get() + 1);
                        }
                    }
                });
                let sess = api.api_session_stats();
                (
                    seeding.get(),
                    downloading.get(),
                    checks.get(),
                    Some((sess.counters.uploaded_bytes, sess.counters.fetched_bytes)),
                    sess.peers.live as usize,
                )
            }
            Phase::Booting { held_len, .. } => (0, *held_len, 0, None, 0),
        };

        // Tick before reading: this query is also a sample point.
        self.tick_tracker();
        let (up_hour, down_hour, up_day, down_day) = match &phase {
            Phase::Live { tracker, .. } => {
                let now = Instant::now();
                match tracker.try_lock() {
                    Ok(tr) => (
                        tr.rate_up(now, Duration::from_secs(3600)),
                        tr.rate_down(now, Duration::from_secs(3600)),
                        tr.rate_up(now, Duration::from_secs(86_400)),
                        tr.rate_down(now, Duration::from_secs(86_400)),
                    ),
                    Err(_) => (0.0, 0.0, 0.0, 0.0),
                }
            }
            Phase::Booting { .. } => (0.0, 0.0, 0.0, 0.0),
        };

        let (up_total, down_total) = totals.unwrap_or((0, 0));
        let (used, limit) = match &phase {
            Phase::Live { storage, .. } => crate::engine::stats::disk_usage(storage),
            Phase::Booting { storage, .. } => crate::engine::stats::disk_usage(storage),
        };
        let booting = matches!(phase, Phase::Booting { .. });
        let held_len = match &phase {
            Phase::Booting { held_len, .. } => *held_len,
            Phase::Live { held_len, .. } => *held_len,
        };
        RuntimeView {
            booting,
            activity,
            checks_in_progress: checks,
            uptime_seconds: started_at.elapsed().as_secs(),
            held_torrents: held_len,
            seeding_torrents: seeding,
            downloading_torrents: downloading,
            disk_used_bytes: used,
            disk_limit_bytes: limit,
            total_bytes_uploaded: up_total,
            total_bytes_downloaded: down_total,
            upload_bps_hour: up_hour,
            download_bps_hour: down_hour,
            upload_bps_day: up_day,
            download_bps_day: down_day,
            active_peers: peers,
            process_rss_bytes: crate::engine::stats::process_rss_bytes(),
        }
    }

    fn held_view(&self) -> HeldView {
        let inner = self.inner.read();
        match &*inner {
            HandleInner::Booting { held, .. } => HeldView {
                booting: true,
                torrents: held.clone(),
            },
            HandleInner::Live(live) => {
                // Live progress + per-torrent state, keyed by infohash.
                let live_map = std::cell::RefCell::new(std::collections::HashMap::new());
                live.session.with_torrents(|it| {
                    for (_, h) in it {
                        let st = h.stats();
                        let verifying = h.with_state(|s| {
                            matches!(s, librqbit::ManagedTorrentState::Initializing(_))
                        });
                        live_map.borrow_mut().insert(
                            h.info_hash().as_string(),
                            (st.progress_bytes, st.finished, verifying),
                        );
                    }
                });
                let live_map = live_map.borrow();
                let held = live.held.lock().unwrap();
                let mut torrents: Vec<HeldTorrentView> = held
                    .iter()
                    .map(|t| {
                        let (progress, finished, verifying) = live_map
                            .get(&t.info_hash)
                            .copied()
                            .unwrap_or((0, false, false));
                        HeldTorrentView {
                            title: t.title.clone(),
                            info_hash: t.info_hash.clone(),
                            size_bytes: t.size_bytes,
                            progress_bytes: progress,
                            finished,
                            verifying,
                            last_known_seeders: t.last_known_seeders,
                        }
                    })
                    .collect();
                torrents.sort_by(|a, b| a.title.cmp(&b.title));
                HeldView {
                    booting: false,
                    torrents,
                }
            }
        }
    }
}

/// Snapshot the held set into live-query entries (title/hash/size/seeders).
pub fn snapshot_entries(state: &crate::state::State) -> Vec<StateEntry> {
    state
        .all()
        .into_iter()
        .map(|t| StateEntry {
            title: t.title,
            info_hash: t.info_hash,
            size_bytes: t.size_bytes,
            last_known_seeders: t.last_known_seeders,
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct StateEntry {
    pub title: String,
    pub info_hash: String,
    pub size_bytes: u64,
    pub last_known_seeders: u32,
}
