//! Bounded file-handle pool for rqbit's storage layer.
//!
//! Stock rqbit (`FilesystemStorage`) opens every file of every torrent in
//! read/write mode at add time and keeps all handles open until the torrent
//! is removed — one fd per library file, forever. On file-heavy libraries
//! (a 29k-file dataset was 44% of mercury's 65,536-fd ceiling on its own)
//! that exhausts the process fd limit: EMFILE on everything, dead listener,
//! dead status. Other clients solved this decades ago with a bounded pool —
//! libtorrent's `file_pool_size` (default 40) and Transmission's
//! `tr_open_files` (LRU, max 32) open/close around each IO. This module
//! plugs the same design into rqbit's public `StorageFactory`:
//!
//! - `init` only creates sparse files (open → `set_len` → close): the
//!   correct footprint, zero fds held afterward.
//! - Every read/write gets its handle from a process-wide LRU pool capped
//!   at `min(4096, hard_limit - 2048)` open fds. `pread`/`pwrite` never
//!   move the cursor, so one fd serves concurrent positioned IO from many
//!   threads with no per-file locking; handles are `Arc<File>` so an
//!   in-flight IO survives eviction (the fd closes only when the last
//!   reference drops — no close-vs-use race).
//! - Pause/resume (`TorrentStorage::take`) is free: storages are
//!   stateless path views; fds age out via the LRU.
//!
//! One upstream quirk shapes the API: `ManagedTorrentShared.options`
//! (output_folder, allow_overwrite) is `pub(crate)` in librqbit, invisible
//! to external factories. The factory therefore resolves paths through
//! [`register_torrent`], which the engine's add/remove paths maintain,
//! keyed by info-hash (public on `shared`).
//!
//! Pool counters are observable via `FilePool::stats()`.

use std::collections::HashMap;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use librqbit::storage::{BoxStorageFactory, StorageFactory, StorageFactoryExt, TorrentStorage};
use librqbit::{ManagedTorrentShared, TorrentMetadata};

use crate::fdlimit;

/// Pool ceiling and the fd reserve left for sockets/pipes/epoll between
/// the pool cap and the process hard limit.
const POOL_CAP_MAX: u64 = 4096;
const FD_RESERVE: u64 = 2048;

const EMFILE: i32 = 24; // libc::EMFILE; literal to avoid a direct libc dep

// ---------------------------------------------------------------------------
// Per-torrent path registry (upstream hides output_folder from factories)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Registered {
    output_folder: PathBuf,
    overwrite: bool,
}

fn registry() -> &'static Mutex<HashMap<[u8; 20], Registered>> {
    static REG: OnceLock<Mutex<HashMap<[u8; 20], Registered>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Publish a torrent's output folder so the pooled factory can resolve
/// paths (upstream passes only the info-hash to factories). Called on the
/// add path before `session.add_torrent`.
pub fn register_torrent(info_hash: [u8; 20], output_folder: PathBuf, overwrite: bool) {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            info_hash,
            Registered {
                output_folder,
                overwrite,
            },
        );
}

/// Drop a torrent's registry entry. Called when the engine removes the
/// torrent. A stale entry is harmless (path strings), but removal keeps
/// the map honest across swap cycles.
pub fn unregister_torrent(info_hash: [u8; 20]) {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&info_hash);
}

fn lookup(info_hash: [u8; 20]) -> Result<Registered> {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&info_hash)
        .cloned()
        .context("torrent not registered for pooled storage")
}

// ---------------------------------------------------------------------------
// The pool
// ---------------------------------------------------------------------------

/// One shared LRU pool. Keyed by `(storage uid, file id)`; values pair the
/// `Arc<File>` (in-flight IO survives eviction) with the resolved path so
/// deletions can sweep cached handles for files they unlink.
pub struct FilePool {
    inner: Mutex<PoolInner>,
    stamp: AtomicU64,
    cap: usize,
    opens: AtomicU64,
    evictions: AtomicU64,
}

type PoolKey = (u64, usize);

#[derive(Default)]
struct PoolInner {
    map: HashMap<PoolKey, (Arc<std::fs::File>, PathBuf)>,
    stamps: HashMap<PoolKey, u64>,
}

impl PoolInner {
    fn evict(&mut self, key: PoolKey) {
        self.map.remove(&key);
        self.stamps.remove(&key);
    }

    /// Evict every handle whose cached path matches — O(cap) sweep, only
    /// called from deletion paths. Needed because rqbit's delete fallback
    /// for errored torrents mints a fresh storage uid (create() without
    /// init), so uid-keyed eviction would miss the entries actually
    /// cached under the original uid.
    fn evict_by_path(&mut self, path: &Path) {
        let victims: Vec<PoolKey> = self
            .map
            .iter()
            .filter(|(_, (_, p))| p == path)
            .map(|(k, _)| *k)
            .collect();
        for k in victims {
            self.evict(k);
        }
    }
}

impl FilePool {
    /// Build a pool with an explicit cap (floored at 1; `global` applies
    /// the production floor).
    pub fn with_cap(cap: usize) -> Arc<Self> {
        Arc::new(Self {
            cap: cap.max(1),
            inner: Mutex::new(PoolInner::default()),
            stamp: AtomicU64::new(0),
            opens: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        })
    }

    /// Process-wide pool, sized once from the process fd HARD limit
    /// (`current_limits()` returns `(soft, hard)`; the hard limit is the
    /// true ceiling keep-at's soft raise targets): `min(POOL_CAP_MAX,
    /// hard - FD_RESERVE)`. Sizing from the soft limit instead would
    /// silently yield a tiny pool on hosts where the best-effort
    /// `raise_soft_limit()` fails (soft stays 1024 → cap 16 → constant
    /// thrash). `KEEPAT_FD_POOL_CAP` overrides explicitly (clamped to
    /// `[1, hard]`), mostly for tests. Later calls return the same pool.
    pub fn global() -> Arc<Self> {
        static GLOBAL: OnceLock<Arc<FilePool>> = OnceLock::new();
        GLOBAL
            .get_or_init(|| {
                let (_, hard) = fdlimit::current_limits().unwrap_or((0, POOL_CAP_MAX + FD_RESERVE));
                let cap = std::env::var("KEEPAT_FD_POOL_CAP")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|v| v.clamp(1, hard)) // explicit, no production floor
                    .unwrap_or_else(|| hard.saturating_sub(FD_RESERVE).min(POOL_CAP_MAX))
                    .max(1) as usize;
                if std::env::var("KEEPAT_FD_POOL_CAP").is_err() && cap < POOL_CAP_MAX as usize {
                    tracing::warn!(
                        "file-handle pool capped below {POOL_CAP_MAX} (hard limit {hard}); \
                         rlimit raise may have failed"
                    );
                }
                tracing::info!("file-handle pool capped at {cap} open fds (hard limit {hard})");
                Self::with_cap(cap)
            })
            .clone()
    }

    /// (opens issued, evictions performed, live handles, cap)
    pub fn stats(&self) -> (u64, u64, usize, usize) {
        let inner = self.locked();
        (
            self.opens.load(Ordering::Relaxed),
            self.evictions.load(Ordering::Relaxed),
            inner.map.len(),
            self.cap,
        )
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, PoolInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn next_stamp(&self) -> u64 {
        self.stamp.fetch_add(1, Ordering::Relaxed)
    }

    fn open_rw(path: &Path) -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
    }

    /// Open (or reuse) the handle for `key`. The open happens under the
    /// lock to dedupe concurrent misses for the same file; opens are
    /// microseconds and pool IO is chunk-granular, so this does not
    /// serialize meaningful work. On EMFILE (something else is eating
    /// fds), shed a quarter of the pool and retry the open exactly once;
    /// a persistent EMFILE returns the error — rqbit surfaces storage
    /// errors as orderly per-torrent/peer failures (read errors drop the
    /// peer, write errors fatal the torrent), never a process wedge.
    fn get_or_open(&self, key: PoolKey, path: &Path) -> std::io::Result<Arc<std::fs::File>> {
        let mut inner = self.locked();
        if let Some((f, _)) = inner.map.get(&key) {
            let f = f.clone();
            let stamp = self.next_stamp();
            inner.stamps.insert(key, stamp);
            return Ok(f);
        }
        match Self::open_rw(path) {
            Ok(f) => {
                self.opens.fetch_add(1, Ordering::Relaxed);
                self.insert(&mut inner, key, Arc::new(f), path);
                return Ok(inner.map.get(&key).expect("just inserted").0.clone());
            }
            Err(e) if e.raw_os_error() != Some(EMFILE) => return Err(e),
            Err(_) => {
                // EMFILE: shed a quarter of the pool, retry exactly once,
                // then give up with the error. Eviction frees fds only when
                // no in-flight IO holds a clone — if the exhaustion is
                // caused by non-pool fds, retrying forever would just spin
                // on this mutex, blocking ALL IO daemon-wide.
                let shed = (self.cap / 4).max(1);
                for _ in 0..shed {
                    let Some(victim) = inner
                        .stamps
                        .iter()
                        .min_by_key(|(_, s)| **s)
                        .map(|(k, _)| *k)
                    else {
                        break;
                    };
                    inner.evict(victim);
                    self.evictions.fetch_add(1, Ordering::Relaxed);
                }
                tracing::warn!("EMFILE: fd pool shed {shed} handles; retrying open once");
            }
        }
        match Self::open_rw(path) {
            Ok(f) => {
                self.opens.fetch_add(1, Ordering::Relaxed);
                self.insert(&mut inner, key, Arc::new(f), path);
                Ok(inner.map.get(&key).expect("just inserted").0.clone())
            }
            Err(e) => Err(e),
        }
    }

    fn insert(&self, inner: &mut PoolInner, key: PoolKey, f: Arc<std::fs::File>, path: &Path) {
        while inner.map.len() >= self.cap {
            let Some(victim) = inner
                .stamps
                .iter()
                .min_by_key(|(_, s)| **s)
                .map(|(k, _)| *k)
            else {
                break;
            };
            if victim == key {
                break; // degenerate cap==1: keep the newest
            }
            inner.evict(victim);
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
        let stamp = self.next_stamp();
        inner.stamps.insert(key, stamp);
        inner.map.insert(key, (f, path.to_path_buf()));
    }

    /// Evict cached handles for `path` (deletion support).
    pub fn evict_path(&self, path: &Path) {
        self.locked().evict_by_path(path);
    }
}

// ---------------------------------------------------------------------------
// Per-torrent storage view
// ---------------------------------------------------------------------------

/// A per-torrent storage view: resolves `(uid, file id)` → path and
/// routes every IO through the shared pool. Holds no fds itself, so
/// rqbit's `take()` (pause) is a stateless clone.
///
/// `uid` is a process-unique instance id, NOT rqbit's torrent id — that
/// one is per-session, so two sessions in one process (the transfer test's
/// seeder/leecher, or seeder + probe sessions) would share pool keys and
/// read each other's cached files. Caught by the transfer test: the
/// leecher "had" every piece via the seeder's handles and downloaded
/// nothing.
pub struct PooledStorage {
    pool: Arc<FilePool>,
    uid: u64,
    output_folder: PathBuf,
    overwrite: bool,
    /// file id → relative path (`None` for padding entries, which rqbit
    /// never reads or writes — its stock storage also dummies those).
    paths: Vec<Option<PathBuf>>,
}

/// Monotonic per-storage-instance id source for pool keys.
fn next_uid() -> u64 {
    static UID: AtomicU64 = AtomicU64::new(0);
    UID.fetch_add(1, Ordering::Relaxed)
}

impl PooledStorage {
    fn handle(&self, file_id: usize) -> Result<Arc<std::fs::File>> {
        let rel = self
            .paths
            .get(file_id)
            .and_then(|p| p.as_ref())
            .with_context(|| format!("no such file id {file_id} in storage {}", self.uid))?;
        let full = self.output_folder.join(rel);
        self.pool
            .get_or_open((self.uid, file_id), &full)
            .with_context(|| format!("opening {full:?} through the fd pool"))
    }
}

impl TorrentStorage for PooledStorage {
    fn init(&mut self, shared: &ManagedTorrentShared, metadata: &TorrentMetadata) -> Result<()> {
        let mut paths = Vec::with_capacity(metadata.file_infos.len());
        for fi in &metadata.file_infos {
            if fi.attrs.padding {
                paths.push(None);
                continue;
            }
            let full = self.output_folder.join(&fi.relative_filename);
            std::fs::create_dir_all(
                full.parent()
                    .with_context(|| format!("no parent for {full:?}"))?,
            )?;
            let f = if self.overwrite {
                // open-or-create WITHOUT truncating: the file may already
                // hold verified data (resume, re-add after restart).
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .mode(0o644)
                    .open(&full)
                    .with_context(|| format!("opening {full:?} read/write"))?
            } else {
                // Upstream stock behavior: create_new fails if present.
                // create_new does not combine with read(true), so two calls.
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o644)
                    .open(&full)
                    .with_context(|| format!("creating {full:?} (no overwrite)"))?;
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&full)
                    .with_context(|| format!("reopening {full:?} read/write"))?
            };
            drop(f);
            // Lengths are deliberately NOT set here: stock rqbit creates
            // 0-byte files and fixes lengths only after the initial hash
            // check (initializing.rs ensure_file_length loop) — which
            // PooledStorage implements via pooled set_len. Setting lengths
            // here would turn every fresh add's check into a full sparse-
            // hole zero-scan of the whole library.
            paths.push(Some(fi.relative_filename.clone()));
        }
        let _ = shared; // upstream hides options; registry provided the paths
        self.paths = paths;
        Ok(())
    }

    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> Result<()> {
        let f = self.handle(file_id)?;
        f.read_exact_at(buf, offset)
            .with_context(|| format!("pread {offset} len {} id {file_id}", buf.len()))
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> Result<()> {
        let f = self.handle(file_id)?;
        f.write_all_at(buf, offset)
            .with_context(|| format!("pwrite {offset} len {} id {file_id}", buf.len()))
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> Result<()> {
        // Pooled: open, truncate, close. set_len on a sparse file extends
        // without allocating blocks, preserving the sparse footprint.
        let f = self.handle(file_id)?;
        f.set_len(length)
            .with_context(|| format!("set_len {length} id {file_id}"))
    }

    fn take(&self) -> Result<Box<dyn TorrentStorage>> {
        // Stateless: a fresh view over the same paths sharing the pool.
        // The old object is dropped by rqbit; fds age out via the LRU.
        Ok(Box::new(PooledStorage {
            pool: self.pool.clone(),
            uid: self.uid,
            output_folder: self.output_folder.clone(),
            overwrite: self.overwrite,
            paths: self.paths.clone(),
        }))
    }

    fn remove_file(&self, _file_id: usize, filename: &Path) -> Result<()> {
        let full = self.output_folder.join(filename);
        std::fs::remove_file(&full).with_context(|| format!("removing {full:?}"))?;
        // Sweep cached handles by path, not uid: rqbit's delete fallback for
        // errored torrents creates a fresh uid, so uid-keyed eviction here
        // would miss the entries actually cached under the original uid and
        // pin the unlinked inodes open.
        self.pool.evict_path(&full);
        Ok(())
    }

    fn remove_directory_if_empty(&self, path: &Path) -> Result<()> {
        let full = self.output_folder.join(path);
        if !full.is_dir() {
            bail!("cannot remove dir: {full:?} is not a directory")
        }
        if std::fs::read_dir(&full)?.next().is_none() {
            std::fs::remove_dir(&full).with_context(|| format!("removing {full:?}"))?;
        } else {
            tracing::debug!("did not remove {full:?} as it was not empty");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Factory wired into `SessionOptions::default_storage_factory`; hands
/// every torrent a [`PooledStorage`] view over the process-wide pool.
#[derive(Clone)]
pub struct PooledStorageFactory {
    pool: Arc<FilePool>,
}

impl PooledStorageFactory {
    pub fn new(pool: Arc<FilePool>) -> Self {
        Self { pool }
    }
}

impl Default for PooledStorageFactory {
    fn default() -> Self {
        Self::new(FilePool::global())
    }
}

impl StorageFactory for PooledStorageFactory {
    type Storage = PooledStorage;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        _metadata: &TorrentMetadata,
    ) -> Result<Self::Storage> {
        let reg = lookup(shared.info_hash.0)?;
        Ok(PooledStorage {
            pool: self.pool.clone(),
            uid: next_uid(),
            output_folder: reg.output_folder,
            overwrite: reg.overwrite,
            paths: Vec::new(),
        })
    }

    fn clone_box(&self) -> BoxStorageFactory {
        self.clone().boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage_with_files(pool: Arc<FilePool>, dir: &Path, names: &[&str]) -> PooledStorage {
        PooledStorage {
            pool,
            uid: 1,
            output_folder: dir.to_path_buf(),
            overwrite: true,
            paths: names.iter().map(|n| Some(PathBuf::from(n))).collect(),
        }
    }

    /// A file opened through the pool reads back what was written through
    /// it; positioned IO leaves no shared cursor state.
    #[test]
    fn roundtrip_through_pool() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.bin");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let pool = FilePool::with_cap(8);
        let f = pool.get_or_open((1, 0), &path).unwrap();
        f.write_all_at(&[0xAA; 100], 0).unwrap();
        let mut buf = [0u8; 100];
        f.read_exact_at(&mut buf, 0).unwrap();
        assert!(buf.iter().all(|&b| b == 0xAA));
        // Non-overlapping positioned IO at another offset.
        f.write_all_at(&[0xBB; 4], 4000).unwrap();
        let mut tail = [0u8; 4];
        f.read_exact_at(&mut tail, 4000).unwrap();
        assert_eq!(tail, [0xBB; 4]);
        assert_eq!(pool.stats().2, 1, "one live handle");
    }

    /// LRU eviction: over cap, the least-recently-touched handle closes
    /// (pool size stays at cap) and the victim is re-openable.
    #[test]
    fn lru_eviction_bounds_pool() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = FilePool::with_cap(2);
        let mut files = Vec::new();
        for i in 0..4 {
            let p = tmp.path().join(format!("f{i}.bin"));
            std::fs::write(&p, vec![i as u8; 16]).unwrap();
            files.push((i, p));
        }
        for (i, p) in &files {
            pool.get_or_open((1, *i), p).unwrap();
        }
        assert_eq!(pool.stats().2, 2, "pool capped at 2");
        assert!(pool.stats().1 >= 2, "at least two evictions happened");
        // f0 was evicted (opened first, never re-touched): reopen and
        // verify data integrity through the fresh handle.
        let f = pool.get_or_open((1, 0), &files[0].1).unwrap();
        let mut buf = [0u8; 16];
        f.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(buf, [0u8; 16]);
        assert_eq!(pool.stats().2, 2, "still bounded after reopen");
    }

    /// Re-touching refreshes recency: the just-used entry survives while
    /// the stale one is evicted.
    #[test]
    fn lru_prefers_evicting_stale_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let mk = |name: &str| {
            let p = tmp.path().join(name);
            std::fs::write(&p, vec![0u8; 8]).unwrap();
            p
        };
        let a = mk("a.bin");
        let b = mk("b.bin");
        let c = mk("c.bin");
        let pool = FilePool::with_cap(2);
        pool.get_or_open((1, 0), &a).unwrap();
        pool.get_or_open((1, 1), &b).unwrap();
        // Touch a: b becomes the victim when c arrives, not a.
        pool.get_or_open((1, 0), &a).unwrap();
        pool.get_or_open((1, 2), &c).unwrap();
        let (opens, evictions, live, _) = pool.stats();
        assert_eq!(live, 2);
        assert_eq!(evictions, 1, "only b evicted (a was re-touched)");
        assert_eq!(opens, 3, "three distinct files opened once each");
    }

    /// IO through the storage must survive eviction: write via id 0, push
    /// other files through the pool to force eviction, then read id 0
    /// back — handle() transparently reopens with correct content.
    #[test]
    fn io_across_eviction_is_transparent() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = FilePool::with_cap(1);
        let st = storage_with_files(pool.clone(), tmp.path(), &["a.bin", "b.bin"]);
        std::fs::write(tmp.path().join("a.bin"), vec![0u8; 1024]).unwrap();
        std::fs::write(tmp.path().join("b.bin"), vec![0u8; 1024]).unwrap();
        st.pwrite_all(0, 0, &[0x42; 512]).unwrap();
        // Touch b (id 1) — with cap 1 this evicts a's handle.
        st.pwrite_all(1, 0, &[0x11; 512]).unwrap();
        // Read a back: reopened from disk through the pool.
        let mut buf = [0u8; 512];
        st.pread_exact(0, 0, &mut buf).unwrap();
        assert_eq!(buf, [0x42; 512]);
        assert_eq!(pool.stats().2, 1, "bounded at cap=1");
    }

    /// take() (pause semantics) returns a fully working fresh view; both
    /// objects keep functioning against the same files.
    #[test]
    fn take_returns_working_view() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = FilePool::with_cap(4);
        let st = storage_with_files(pool, tmp.path(), &["f.bin"]);
        std::fs::write(tmp.path().join("f.bin"), vec![0u8; 64]).unwrap();
        let taken = st.take().unwrap();
        taken.pwrite_all(0, 0, &[0x77; 32]).unwrap();
        st.pread_exact(0, 0, &mut [0u8; 32][..]).unwrap();
        let mut buf = [0u8; 32];
        taken.pread_exact(0, 0, &mut buf).unwrap();
        assert_eq!(buf, [0x77; 32]);
    }

    /// Unknown file ids error cleanly (bad id, padding-style dummies).
    #[test]
    fn io_on_unknown_file_id_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let st = storage_with_files(FilePool::with_cap(4), tmp.path(), &["f.bin"]);
        assert!(st.pread_exact(9, 0, &mut [0u8; 1]).is_err());
    }

    /// remove_file deletes the file and drops the cached handle.
    #[test]
    fn remove_file_deletes_and_evicts() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = FilePool::with_cap(4);
        let st = storage_with_files(pool.clone(), tmp.path(), &["gone.bin"]);
        std::fs::write(tmp.path().join("gone.bin"), vec![0u8; 32]).unwrap();
        st.pwrite_all(0, 0, &[0; 8]).unwrap();
        assert_eq!(pool.stats().2, 1);
        st.remove_file(0, Path::new("gone.bin")).unwrap();
        assert!(!tmp.path().join("gone.bin").exists());
        assert_eq!(pool.stats().2, 0, "handle evicted on remove");
    }

    /// remove_directory_if_empty removes only empty leaf dirs.
    #[test]
    fn remove_directory_if_empty_behavior() {
        let tmp = tempfile::tempdir().unwrap();
        let st = storage_with_files(FilePool::with_cap(4), tmp.path(), &["d/x.bin"]);
        std::fs::create_dir_all(tmp.path().join("d")).unwrap();
        st.remove_directory_if_empty(Path::new("d")).unwrap();
        assert!(!tmp.path().join("d").exists());
        // Non-empty dir: left alone.
        std::fs::create_dir_all(tmp.path().join("e")).unwrap();
        std::fs::write(tmp.path().join("e/f"), b"x").unwrap();
        assert!(st.remove_directory_if_empty(Path::new("e")).is_ok());
        assert!(tmp.path().join("e").exists(), "non-empty dir survives");
        // Missing dir: error.
        assert!(st.remove_directory_if_empty(Path::new("nope")).is_err());
    }

    /// Concurrent positioned IO on one shared handle: preads while a
    /// pwrite loop runs, no corruption, no deadlock.
    #[test]
    fn concurrent_positioned_io_is_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("shared.bin");
        std::fs::write(&path, vec![0u8; 8192]).unwrap();
        let pool = FilePool::with_cap(4);
        let f = pool.get_or_open((1, 0), &path).unwrap();
        let mut handles = Vec::new();
        for t in 0..4u8 {
            let f = f.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    let off = u64::from(t) * 1024;
                    f.write_all_at(&[t; 256], off).unwrap();
                    let mut buf = [0u8; 256];
                    f.read_exact_at(&mut buf, off).unwrap();
                    assert!(buf.iter().all(|&b| b == t));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    /// Two storages in different sessions that happen to share rqbit's
    /// per-session torrent id must NOT share pool handles: uid-keyed keys
    /// keep their caches isolated. (The transfer test caught the original
    /// (torrent_id, file) keying: the leecher "had" all pieces via the
    /// seeder's cached fds and downloaded nothing.)
    #[test]
    fn same_rqbittid_different_storages_never_share_handles() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_a = tmp.path().join("seed");
        let dir_b = tmp.path().join("leech");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        std::fs::write(dir_a.join("f.bin"), vec![0xAA; 64]).unwrap();
        std::fs::write(dir_b.join("f.bin"), vec![0u8; 64]).unwrap();
        let pool = FilePool::with_cap(8);
        let a = PooledStorage {
            pool: pool.clone(),
            uid: 1000,
            output_folder: dir_a,
            overwrite: true,
            paths: vec![Some(PathBuf::from("f.bin"))],
        };
        // Both storages would be torrent id 0 in their own sessions —
        // but they carry distinct uids, so handles are distinct.
        let b = PooledStorage {
            pool: pool.clone(),
            uid: 101,
            output_folder: dir_b,
            overwrite: true,
            paths: vec![Some(PathBuf::from("f.bin"))],
        };
        let mut buf = [0u8; 64];
        a.pread_exact(0, 0, &mut buf).unwrap();
        assert_eq!(buf, [0xAA; 64], "seeder reads its own file");
        b.pread_exact(0, 0, &mut buf).unwrap();
        assert_eq!(
            buf, [0u8; 64],
            "leecher reads its own sparse file, not the seeder's"
        );
        assert_eq!(pool.stats().2, 2, "two distinct handles cached");
    }

    /// Registry: register → lookup works; unregister removes.
    #[test]
    fn registry_roundtrip() {
        let ih = [9u8; 20];
        register_torrent(ih, PathBuf::from("/tmp/x"), true);
        let reg = lookup(ih).unwrap();
        assert_eq!(reg.output_folder, PathBuf::from("/tmp/x"));
        assert!(reg.overwrite);
        unregister_torrent(ih);
        assert!(lookup(ih).is_err());
    }
}
