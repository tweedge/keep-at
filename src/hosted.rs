//! `keep-at hosted-torrents`: list held torrents with live progress.
//!
//! Live-first: when the daemon is running, per-torrent progress comes
//! straight from the session (verified bytes + finished flag) — a
//! just-added sparse torrent correctly shows downloading with real
//! progress, never "fully downloaded instantly". Otherwise falls back to
//! state + on-disk sizes.

use anyhow::Result;

use crate::cli::CommonArgs;
use crate::engine::storage::dir_size_bytes;
use crate::engine::torrents::torrent_output_dir;
use crate::humanize;
use crate::live;
use crate::state::State;

pub fn cmd_hosted(args: &CommonArgs) -> Result<()> {
    let dir = crate::cli::resolve_data_dir(args)?;
    // Same traversal repair as status (read-only op, any user).
    crate::config::ensure_shared_dirs(&dir);

    // Live daemon: authoritative per-torrent progress.
    if let Some(live::Response::Held(view)) = live::query(&dir, &live::Request::Held) {
        if view.torrents.is_empty() {
            println!("keep-at is not holding any torrents");
            return Ok(());
        }
        for t in &view.torrents {
            println!("{}", t.title);
            println!(
                "  link:        https://academictorrents.com/details/{}",
                t.info_hash
            );
            println!(
                "  status:      {}",
                if t.finished { "seeding" } else { "downloading" }
            );
            println!(
                "  space:       {} present (torrent is {})",
                humanize::human_bytes(t.progress_bytes.min(t.size_bytes) as i64),
                humanize::human_bytes(t.size_bytes as i64)
            );
            println!("  last scrape: {} seeders", t.last_known_seeders);
        }
        return Ok(());
    }

    // Offline fallback: state + on-disk heuristic (unchanged).
    let st = State::load(&dir.join("state.json"))?;
    let held = st.all();
    if held.is_empty() {
        println!("keep-at is not holding any torrents");
        return Ok(());
    }

    let mut rows = held;
    rows.sort_by(|a, b| a.title.cmp(&b.title));
    for t in rows {
        let out_dir = torrent_output_dir(&t.storage_location, &t.info_hash);
        let on_disk = dir_size_bytes(&out_dir);
        // Seeding heuristic without a live session: fully present when the
        // completed-bytes marker says so, else compare on-disk to nominal.
        // Plain storage writes sparse, so on-disk < nominal while downloading.
        let seeding = on_disk >= t.size_bytes && t.size_bytes > 0;
        println!("{}", t.title);
        println!(
            "  link:        https://academictorrents.com/details/{}",
            t.info_hash
        );
        println!(
            "  status:      {}",
            if seeding { "seeding" } else { "downloading" }
        );
        println!(
            "  space:       {} on disk (torrent is {})",
            humanize::human_bytes(on_disk as i64),
            humanize::human_bytes(t.size_bytes as i64)
        );
        println!("  last scrape: {} seeders", t.last_known_seeders);
    }
    Ok(())
}
