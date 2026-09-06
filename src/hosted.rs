//! `keep-at hosted-torrents`: list held torrents from state + disk.
//! Works whether or not keep-at is running. Plain storage: seeding ==
//! fully downloaded (output dir holds the full nominal size).

use anyhow::Result;

use crate::cli::CommonArgs;
use crate::engine::storage::dir_size_bytes;
use crate::engine::torrents::torrent_output_dir;
use crate::humanize;
use crate::state::State;

pub fn cmd_hosted(args: &CommonArgs) -> Result<()> {
    let dir = crate::cli::resolve_data_dir(args)?;
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
