//! Persisted view of what keep-at holds. Plain JSON, atomic writes.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Torrent {
    pub info_hash: String,
    #[serde(default)]
    pub title: String,
    pub size_bytes: u64,
    pub storage_location: PathBuf,
    pub added_at: DateTime<Utc>,
    /// Piece count at add time (0 = unknown, written by older versions).
    /// Feeds the RAM model so swaps price held torrents without re-parsing.
    #[serde(default)]
    pub piece_count: u32,
    #[serde(default)]
    pub last_known_seeders: u32,
    /// Verified bytes present at the last progress observation (name is a
    /// legacy misnomer - it has stored byte counts, not piece counts, since
    /// the progress tracking moved to bytes). u64 so torrents past 4 GiB
    /// keep comparing exactly against progress_bytes; older state files
    /// hold u32-capped values and migrate on the next observation.
    #[serde(default)]
    pub completed_pieces: u64,
    #[serde(default)]
    pub last_progress_at: Option<DateTime<Utc>>,
    /// Last scan that saw this info_hash still listed in the AT catalog.
    /// None on state written by older versions (treated as "last confirmed
    /// at add time" by the vanished-eviction grace period). Set every scan
    /// the catalog still lists it; the deleted-torrent pass evicts only
    /// once it has stayed absent longer than the grace timeout.
    #[serde(default)]
    pub last_confirmed_in_catalog_at: Option<DateTime<Utc>>,
}

/// A torrent removed under the broken-piece quarantine. Persistent in
/// state.json so the removal survives scans and restarts: the selection
/// gate skips the hash while `now < cooldown_until`, and the cooldown
/// expiry doubles as the periodic re-probe (AT may fix their seeders' data,
/// in which case the re-add completes and the quarantine lifts).
/// See notes/DESIGN-broken-piece-quarantine.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quarantine {
    pub title: String,
    /// Plain-language trigger (discarded volume + zero-progress passes).
    pub reason: String,
    pub quarantined_at: DateTime<Utc>,
    /// When the hash becomes re-eligible for selection again. Far-future
    /// once `scan.quarantine_max_retries` is exceeded.
    pub cooldown_until: DateTime<Utc>,
    /// How many times this hash has been quarantined (re-probes included).
    pub attempts: u32,
    /// Discarded bytes observed at the latest trigger.
    pub wasted_bytes: u64,
}

/// Pure completion test for a quarantine re-probe: a torrent lifts only
/// when its verified bytes cover its registered size in full. Shared by
/// [`State::lift_completed`] and the watchdog pass's race catch in
/// `engine/scan.rs` (which batches lifts across a pass and cannot call the
/// state method per hash), so the rule is pinned in one place.
pub fn completion_lifts(checked: u64, size_bytes: u64) -> bool {
    size_bytes > 0 && checked >= size_bytes
}

/// One pending per-torrent mutation for a batched [`State::update_each`].
pub type TorrentUpdate = Box<dyn FnOnce(&mut Torrent)>;

pub struct State {
    path: PathBuf,
    torrents: HashMap<String, Torrent>,
    quarantined: HashMap<String, Quarantine>,
}

impl State {
    /// Load state; missing file => empty state (brand new install).
    pub fn load(path: &Path) -> Result<State> {
        let mut torrents = HashMap::new();
        let mut quarantined = HashMap::new();
        match std::fs::read(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
            Ok(data) => {
                let on_disk: Stored = serde_json::from_slice(&data)
                    .with_context(|| format!("parsing {}", path.display()))?;
                torrents = on_disk.torrents;
                quarantined = on_disk.quarantined;
            }
        }
        Ok(State {
            path: path.to_path_buf(),
            torrents,
            quarantined,
        })
    }

    pub fn all(&self) -> Vec<Torrent> {
        self.torrents.values().cloned().collect()
    }

    pub fn get(&self, info_hash_hex: &str) -> Option<&Torrent> {
        self.torrents.get(info_hash_hex)
    }

    pub fn put(&mut self, t: Torrent) -> Result<()> {
        self.torrents.insert(t.info_hash.clone(), t);
        self.save()
    }

    pub fn update(&mut self, info_hash_hex: &str, f: impl FnOnce(&mut Torrent)) -> Result<bool> {
        let changed = match self.torrents.get_mut(info_hash_hex) {
            Some(t) => {
                f(t);
                true
            }
            None => false,
        };
        if changed {
            self.save()?;
        }
        Ok(changed)
    }

    pub fn remove(&mut self, info_hash_hex: &str) -> Result<()> {
        self.torrents.remove(info_hash_hex);
        self.save()
    }

    /// Number of hashes currently in the quarantine registry (status line).
    pub fn quarantine_count(&self) -> usize {
        self.quarantined.len()
    }

    /// The registry's key set (lift discovery inside engine closures that
    /// cannot reach the registry directly).
    pub fn quarantined_keys(&self) -> std::collections::HashSet<String> {
        self.quarantined.keys().cloned().collect()
    }

    /// The quarantine entry for a hash, if any.
    pub fn quarantine_get(&self, info_hash_hex: &str) -> Option<Quarantine> {
        self.quarantined.get(info_hash_hex).cloned()
    }

    /// Insert or replace a quarantine entry and persist (registry-only
    /// write: the ghost-enforcement path has no held entry to drop).
    pub fn quarantine_put(&mut self, info_hash_hex: String, q: Quarantine) -> Result<()> {
        self.quarantined.insert(info_hash_hex, q);
        self.save()
    }

    /// Insert or replace a quarantine entry AND remove the held torrent in
    /// ONE atomic save. The trip path used to do two saves (put, then
    /// remove), leaving a kill/save-failure window where the hash existed
    /// in BOTH maps — a zombie that could never re-enter the session (no
    /// cached .torrent after the trip deleted it) and, with
    /// max_retries escalation, stay gated for the indefinite cooldown.
    pub fn quarantine_and_remove(&mut self, info_hash_hex: String, q: Quarantine) -> Result<()> {
        self.quarantined.insert(info_hash_hex.clone(), q);
        self.torrents.remove(&info_hash_hex);
        self.save()
    }

    /// Drop several quarantine entries in one save (lift/GC batches).
    /// Returns how many were actually removed.
    pub fn quarantine_remove_many(&mut self, hashes: &[String]) -> Result<usize> {
        let mut removed = 0;
        for hex in hashes {
            if self.quarantined.remove(hex).is_some() {
                removed += 1;
            }
        }
        if removed > 0 {
            self.save()?;
        }
        Ok(removed)
    }

    /// Drop a quarantine entry (cooldown lapsed or manual release). Returns
    /// whether an entry existed; persists only on change.
    pub fn quarantine_remove(&mut self, info_hash_hex: &str) -> Result<bool> {
        let existed = self.quarantined.remove(info_hash_hex).is_some();
        if existed {
            self.save()?;
        }
        Ok(existed)
    }

    /// Lift rule for a torrent that just produced verified bytes: the
    /// re-probe COMPLETED (all registered pieces validated), so the
    /// quarantine is over. The pure completion test lives in
    /// [`completion_lifts`] so the sweep's finished-branch lift and the
    /// watchdog pass's race catch share exactly the rule the unit tests
    /// pin.
    pub fn lift_completed(
        &mut self,
        info_hash_hex: &str,
        checked: u64,
        size_bytes: u64,
    ) -> Result<bool> {
        if !completion_lifts(checked, size_bytes) || !self.quarantined.contains_key(info_hash_hex) {
            return Ok(false);
        }
        self.quarantine_remove(info_hash_hex)
    }

    /// Apply one mutation per named torrent, then persist ONCE.
    ///
    /// Prefer this over a loop of [`State::update`] for a whole pass over the
    /// held set: `update` saves the full file (rewrite + fsync) per entry, so
    /// the catalog-confirmation and seeder-refresh loops used to rewrite
    /// `state.json` up to 2x|held| times per scan - on a Pi/SD node with
    /// ~900 holdings that is hundreds of full-file rewrites and fsyncs per
    /// scan. Same shape as [`update_progress_many`], kept generic so each
    /// call site keeps its own per-torrent mutation.
    ///
    /// Unknown hashes are ignored. Returns how many torrents were actually
    /// mutated; nothing is written when that is 0.
    pub fn update_each(&mut self, updates: Vec<(String, TorrentUpdate)>) -> Result<usize> {
        let mut changed = 0usize;
        for (hex, f) in updates {
            if let Some(t) = self.torrents.get_mut(&hex) {
                f(t);
                changed += 1;
            }
        }
        if changed > 0 {
            self.save()?;
        }
        Ok(changed)
    }

    /// Progress bookkeeping for a whole watchdog pass in ONE save: pairs
    /// of (hash, verified bytes). Only entries whose verified count grew
    /// (or that never had a progress stamp) are touched; no change → no
    /// save. The per-entry `State::update` version saves the full file
    /// per torrent, which made a mass-progress pass rewrite state.json
    /// hundreds of times.
    pub fn update_progress_many(
        &mut self,
        progress: &[(String, u64)],
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let mut changed = false;
        for (hex, checked) in progress {
            let Some(t) = self.torrents.get_mut(hex) else {
                continue;
            };
            if *checked > t.completed_pieces || t.last_progress_at.is_none() {
                t.completed_pieces = *checked;
                t.last_progress_at = Some(now);
                changed = true;
            }
        }
        if changed {
            self.save()?;
        }
        Ok(changed)
    }

    pub fn save(&self) -> Result<()> {
        let stored = Stored {
            torrents: self.torrents.clone(),
            quarantined: self.quarantined.clone(),
        };
        let data = serde_json::to_string_pretty(&stored).context("marshalling state")?;
        crate::config::atomic_write(&self.path, data.as_bytes())
    }

    /// Sum of nominal sizes in one storage location.
    pub fn bytes_used(&self, location: &Path) -> u64 {
        self.torrents
            .values()
            .filter(|t| t.storage_location == location)
            .map(|t| t.size_bytes)
            .sum()
    }

    /// Re-key held torrents to canonical storage-location spellings. A
    /// config relocation/spelling change used to orphan entries: bytes_used
    /// matches by raw path, so renamed entries under-counted their location
    /// and free_bytes over-reported, letting the node re-fill on top.
    /// Canonicalizes each held torrent's directory (when it exists) and
    /// matches it against the configured (already-canonical) locations.
    /// Returns true when anything changed (caller persists).
    pub fn rekey_storage_locations(&mut self, locations: &[PathBuf]) -> bool {
        let mut changed = false;
        for t in self.torrents.values_mut() {
            if locations.contains(&t.storage_location) {
                continue;
            }
            if let Ok(canon) = t.storage_location.canonicalize() {
                if locations.contains(&canon) && t.storage_location != canon {
                    t.storage_location = canon;
                    changed = true;
                }
            }
        }
        if changed {
            let _ = self.save();
        }
        changed
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    torrents: HashMap<String, Torrent>,
    /// Broken-piece quarantine registry (serde default: absent on state
    /// files written before the feature existed).
    #[serde(default)]
    quarantined: HashMap<String, Quarantine>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut st = State::load(&path).unwrap();
        assert!(st.all().is_empty());
        st.put(Torrent {
            info_hash: "ab".to_string(),
            title: "t".to_string(),
            size_bytes: 10,
            storage_location: PathBuf::from("/x"),
            added_at: Utc::now(),
            piece_count: 0,
            last_known_seeders: 0,
            completed_pieces: 0,
            last_progress_at: None,
            last_confirmed_in_catalog_at: None,
        })
        .unwrap();
        let st2 = State::load(&path).unwrap();
        assert_eq!(st2.all().len(), 1);
        assert_eq!(st2.bytes_used(Path::new("/x")), 10);
    }

    #[test]
    fn bytes_used_is_nominal_not_on_disk() {
        // Regression test for the production over-commit defect: free-space
        // accounting must price held NOMINAL sizes (what downloads will
        // eventually occupy), never on-disk actuals (sparse files lag by
        // orders of magnitude). bytes_used sums size_bytes verbatim.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut st = State::load(&path).unwrap();
        for (ih, size) in [("aa", 100u64), ("bb", 200u64)] {
            st.put(Torrent {
                info_hash: ih.to_string(),
                title: "t".to_string(),
                size_bytes: size,
                storage_location: PathBuf::from("/loc"),
                added_at: Utc::now(),
                piece_count: 0,
                last_known_seeders: 0,
                completed_pieces: 0,
                last_progress_at: None,
                last_confirmed_in_catalog_at: None,
            })
            .unwrap();
        }
        // 300 nominal regardless of what exists on disk (nothing does).
        assert_eq!(st.bytes_used(Path::new("/loc")), 300);
        assert_eq!(st.bytes_used(Path::new("/other")), 0);
    }

    #[test]
    fn quarantine_registry_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut st = State::load(&path).unwrap();
        assert_eq!(st.quarantine_count(), 0);
        let q = Quarantine {
            title: "NotaBug Code Dataset".to_string(),
            reason: "discarded 512 MiB across 2 zero-progress passes".to_string(),
            quarantined_at: Utc::now(),
            cooldown_until: Utc::now() + chrono::Duration::days(3),
            attempts: 1,
            wasted_bytes: 512 * 1024 * 1024,
        };
        st.quarantine_put("aa".to_string(), q.clone()).unwrap();
        assert_eq!(st.quarantine_count(), 1);
        assert_eq!(st.quarantine_get("aa").unwrap().attempts, 1);
        assert!(st.quarantine_get("bb").is_none());

        // Persistence: a fresh load sees the same registry.
        let st2 = State::load(&path).unwrap();
        assert_eq!(st2.quarantine_count(), 1);
        let got = st2.quarantine_get("aa").unwrap();
        assert_eq!(got.title, q.title);
        assert_eq!(got.wasted_bytes, q.wasted_bytes);
        assert_eq!(got.attempts, 1);

        // Held entries and quarantine entries coexist in one file.
        st.put(Torrent {
            info_hash: "cc".to_string(),
            title: "t".to_string(),
            size_bytes: 10,
            storage_location: PathBuf::from("/loc"),
            added_at: Utc::now(),
            piece_count: 0,
            last_known_seeders: 0,
            completed_pieces: 0,
            last_progress_at: None,
            last_confirmed_in_catalog_at: None,
        })
        .unwrap();
        let mut st3 = State::load(&path).unwrap();
        assert_eq!(st3.quarantine_count(), 1);
        assert_eq!(st3.all().len(), 1);

        // Removal drops exactly the one entry.
        assert!(st3.quarantine_remove("aa").unwrap());
        assert!(!st3.quarantine_remove("aa").unwrap());
        let st4 = State::load(&path).unwrap();
        assert_eq!(st4.quarantine_count(), 0);
        assert_eq!(st4.all().len(), 1);
    }

    #[test]
    fn legacy_state_without_quarantined_key_loads() {
        // Pre-quarantine state files carry no "quarantined" key; serde
        // default must fill it in and a save must add the key back without
        // touching the held set.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"torrents": {"dd": {"info_hash": "dd", "title": "old",
                 "size_bytes": 5, "storage_location": "/loc",
                 "added_at": "2026-09-01T00:00:00Z"}}}"#,
        )
        .unwrap();
        let st = State::load(&path).unwrap();
        assert_eq!(st.all().len(), 1);
        assert_eq!(st.quarantine_count(), 0);
    }

    #[test]
    fn lift_completed_requires_full_verification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut st = State::load(&path).unwrap();
        let q = Quarantine {
            title: "t".to_string(),
            reason: "r".to_string(),
            quarantined_at: Utc::now(),
            cooldown_until: Utc::now(),
            attempts: 1,
            wasted_bytes: 1,
        };
        st.quarantine_put("aa".to_string(), q).unwrap();
        // Partial verification (the NotaBug shape: size-1 bytes validated,
        // one poisoned piece left) must NOT lift.
        assert!(!st.lift_completed("aa", 99, 100).unwrap());
        assert_eq!(st.quarantine_count(), 1);
        // Full verification lifts.
        assert!(st.lift_completed("aa", 100, 100).unwrap());
        assert_eq!(st.quarantine_count(), 0);
        // Zero-size (untracked) hashes never lift via this rule.
        st.quarantine_put(
            "bb".to_string(),
            Quarantine {
                title: "t".to_string(),
                reason: "r".to_string(),
                quarantined_at: Utc::now(),
                cooldown_until: Utc::now(),
                attempts: 1,
                wasted_bytes: 1,
            },
        )
        .unwrap();
        assert!(!st.lift_completed("bb", 1000, 0).unwrap());
        // Not-quarantined hashes are a no-op returning false.
        assert!(!st.lift_completed("cc", 100, 100).unwrap());
        // The pure rule the watchdog pass's race catch calls directly:
        // same truth table as the state method above.
        assert!(!completion_lifts(99, 100));
        assert!(!completion_lifts(0, 100));
        assert!(!completion_lifts(1000, 0));
        assert!(completion_lifts(100, 100));
        assert!(completion_lifts(1000, 100));
    }

    #[test]
    fn update_progress_many_single_save_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut st = State::load(&path).unwrap();
        st.put(Torrent {
            info_hash: "aa".to_string(),
            title: "t".to_string(),
            size_bytes: 100,
            storage_location: PathBuf::from("/loc"),
            added_at: Utc::now(),
            piece_count: 0,
            last_known_seeders: 0,
            completed_pieces: 0,
            last_progress_at: None,
            last_confirmed_in_catalog_at: None,
        })
        .unwrap();
        let now = Utc::now();
        // Growth + first-stamp in one call; unrelated hashes ignored.
        let changed = st
            .update_progress_many(&[("aa".to_string(), 50), ("zz".to_string(), 999)], now)
            .unwrap();
        assert!(changed);
        let t = st.get("aa").unwrap();
        assert_eq!(t.completed_pieces, 50);
        assert_eq!(t.last_progress_at, Some(now));
        // No growth → no change, no save.
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!st
            .update_progress_many(&[("aa".to_string(), 50)], now)
            .unwrap());
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            mtime,
            "no-progress call must not rewrite the file"
        );
        // Regression (checked < current) must not move the clock backwards.
        assert!(!st
            .update_progress_many(&[("aa".to_string(), 10)], now)
            .unwrap());
        assert_eq!(st.get("aa").unwrap().completed_pieces, 50);
    }

    /// A whole pass over the held set must cost ONE write, not one write
    /// per torrent - the catalog-confirm and seeder-refresh loops run every
    /// scan and used to rewrite the full file up to 2x|held| times.
    #[test]
    fn update_each_persists_once_for_a_whole_pass() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut st = State::load(&path).unwrap();
        for hash in ["aa", "bb", "cc"] {
            st.put(Torrent {
                info_hash: hash.to_string(),
                title: format!("t-{hash}"),
                size_bytes: 100,
                storage_location: PathBuf::from("/loc"),
                added_at: Utc::now(),
                piece_count: 0,
                last_known_seeders: 0,
                completed_pieces: 0,
                last_progress_at: None,
                last_confirmed_in_catalog_at: None,
            })
            .unwrap();
        }
        let stamp = Utc::now();
        let n = st
            .update_each(vec![
                (
                    "aa".to_string(),
                    Box::new(|t: &mut Torrent| {
                        t.last_known_seeders = 7;
                    }) as Box<dyn FnOnce(&mut Torrent)>,
                ),
                (
                    "bb".to_string(),
                    Box::new(move |t: &mut Torrent| {
                        t.last_confirmed_in_catalog_at = Some(stamp);
                    }),
                ),
                // Unknown hash: ignored, not an error.
                (
                    "zz".to_string(),
                    Box::new(|t: &mut Torrent| {
                        t.last_known_seeders = 99;
                    }),
                ),
            ])
            .unwrap();
        assert_eq!(n, 2, "only the two known hashes mutate");
        assert_eq!(st.get("aa").unwrap().last_known_seeders, 7);
        assert_eq!(
            st.get("bb").unwrap().last_confirmed_in_catalog_at,
            Some(stamp)
        );
        assert_eq!(st.get("cc").unwrap().last_known_seeders, 0);
        // Reload: the single save carried both mutations.
        let reloaded = State::load(&path).unwrap();
        assert_eq!(reloaded.get("aa").unwrap().last_known_seeders, 7);
        assert_eq!(
            reloaded.get("bb").unwrap().last_confirmed_in_catalog_at,
            Some(stamp)
        );
        // Nothing mutated -> no write at all.
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(st.update_each(Vec::new()).unwrap(), 0);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            mtime,
            "an empty pass must not rewrite the file"
        );
    }

    #[test]
    fn quarantine_and_remove_is_atomic_across_maps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut st = State::load(&path).unwrap();
        st.put(Torrent {
            info_hash: "aa".to_string(),
            title: "t".to_string(),
            size_bytes: 10,
            storage_location: PathBuf::from("/loc"),
            added_at: Utc::now(),
            piece_count: 0,
            last_known_seeders: 0,
            completed_pieces: 0,
            last_progress_at: None,
            last_confirmed_in_catalog_at: None,
        })
        .unwrap();
        let q = Quarantine {
            title: "t".to_string(),
            reason: "r".to_string(),
            quarantined_at: Utc::now(),
            cooldown_until: Utc::now(),
            attempts: 1,
            wasted_bytes: 1,
        };
        st.quarantine_and_remove("aa".to_string(), q).unwrap();
        // Both mutations landed in ONE save: the hash is in the registry
        // and nowhere in the held set — on disk as well as in memory.
        let st2 = State::load(&path).unwrap();
        assert_eq!(st2.quarantine_count(), 1);
        assert!(st2.get("aa").is_none());
        assert_eq!(st2.all().len(), 0);
    }

    #[test]
    fn quarantine_remove_many_batch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut st = State::load(&path).unwrap();
        let q = Quarantine {
            title: "t".to_string(),
            reason: "r".to_string(),
            quarantined_at: Utc::now(),
            cooldown_until: Utc::now(),
            attempts: 1,
            wasted_bytes: 1,
        };
        for h in ["aa", "bb", "cc"] {
            st.quarantine_put(h.to_string(), q.clone()).unwrap();
        }
        let removed = st
            .quarantine_remove_many(&["aa".to_string(), "xx".to_string(), "bb".to_string()])
            .unwrap();
        assert_eq!(removed, 2);
        assert_eq!(st.quarantine_count(), 1);
        // Empty result → no save.
        assert_eq!(st.quarantine_remove_many(&["zz".to_string()]).unwrap(), 0);
    }
}
