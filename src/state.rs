//! Persisted view of what keep-at holds. Ported from internal/state
//! (Go). Plain JSON, atomic writes.

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
    #[serde(default)]
    pub last_known_seeders: u32,
    #[serde(default)]
    pub completed_pieces: u32,
    #[serde(default)]
    pub last_progress_at: Option<DateTime<Utc>>,
}

pub struct State {
    path: PathBuf,
    torrents: HashMap<String, Torrent>,
}

impl State {
    /// Load state; missing file => empty state (brand new install).
    pub fn load(path: &Path) -> Result<State> {
        let mut torrents = HashMap::new();
        match std::fs::read(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
            Ok(data) => {
                let on_disk: Stored = serde_json::from_slice(&data)
                    .with_context(|| format!("parsing {}", path.display()))?;
                torrents = on_disk.torrents;
            }
        }
        Ok(State {
            path: path.to_path_buf(),
            torrents,
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

    pub fn save(&self) -> Result<()> {
        let stored = Stored {
            torrents: self.torrents.clone(),
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
}

#[derive(Debug, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    torrents: HashMap<String, Torrent>,
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
            last_known_seeders: 0,
            completed_pieces: 0,
            last_progress_at: None,
        })
        .unwrap();
        let st2 = State::load(&path).unwrap();
        assert_eq!(st2.all().len(), 1);
        assert_eq!(st2.bytes_used(Path::new("/x")), 10);
    }
}
