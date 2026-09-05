//! Engine: scan loop, rqbit session management, storage accounting,
//! stall eviction. Ported from internal/engine (Go), adapted to rqbit's
//! session API and plain on-disk storage (no compression backend).

pub mod ram;
pub mod scan;
pub mod session;
pub mod stats;
pub mod storage;
pub mod torrents;

pub use ram::{max_torrents_for_budget, per_torrent_ram, system_total_ram};
pub use scan::{Engine, LastScanStats, Options as EngineOptions};
pub use stats::runtime_stats_path;
pub use storage::{device_free_bytes, device_total_bytes, dir_size_bytes, resolve_all_limits};
pub use torrents::{find_torrent, torrent_output_dir, ManagedTorrentHandle};
