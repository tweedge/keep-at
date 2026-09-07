//! Engine: scan loop, rqbit session management, storage accounting,
//! stall eviction, and plain on-disk storage.

pub mod ram;
pub mod scan;
pub mod session;
pub mod stats;
pub mod storage;
pub mod torrents;

pub use ram::{
    max_torrents_for_budget, peer_limit_for_budget, size_bias_for_ratio, system_total_ram,
    torrent_ram,
};
pub use scan::{Engine, LastScanStats, Options as EngineOptions};
pub use storage::{device_free_bytes, device_total_bytes, dir_size_bytes, resolve_all_limits};
pub use torrents::{find_torrent, torrent_output_dir, ManagedTorrentHandle};
