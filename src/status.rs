//! `keep-at status`: running state + runtime summary.
//! Ported from cmd/keep-at/status.go (Go).

use anyhow::Result;

use crate::cli::CommonArgs;
use crate::daemonctl;
use crate::humanize;
use crate::netstats;

pub fn cmd_status(args: &CommonArgs) -> Result<()> {
    let dir = crate::cli::resolve_data_dir(args)?;

    let st = daemonctl::status(&dir);
    if st.running {
        match st.pid {
            Some(pid) => println!("keep-at is running (pid {pid})"),
            None => println!("keep-at is running in the foreground, not as a service"),
        }
    } else {
        println!("keep-at is not running");
    }

    let rs = netstats::load_runtime(&dir.join("runtime-stats.json"))?;
    if rs.collected_at.is_none() {
        return Ok(());
    }

    println!(
        "runtime stats ({} uptime, uptime {}):",
        rs.collected_at.map(format_time).unwrap_or_default(),
        humanize::human_duration(rs.uptime())
    );
    println!(
        "  torrents: {} held, {} seeding, {} downloading",
        rs.held_torrents, rs.seeding_torrents, rs.downloading_torrents
    );
    if rs.disk_limit_bytes > 0 {
        println!(
            "  disk: {} used of {} configured ({:.1}%)",
            humanize::human_bytes(rs.disk_used_bytes as i64),
            humanize::human_bytes(rs.disk_limit_bytes as i64),
            rs.disk_used_pct()
        );
    }
    println!(
        "  useful upload since boot: {} (total network {})",
        humanize::human_bytes(rs.useful_bytes_uploaded as i64),
        humanize::human_bytes(rs.total_bytes_uploaded as i64)
    );
    println!(
        "  useful download since boot: {} (total network {})",
        humanize::human_bytes(rs.useful_bytes_downloaded as i64),
        humanize::human_bytes(rs.total_bytes_downloaded as i64)
    );
    println!(
        "  avg upload since boot: {}",
        humanize::human_bits_per_sec(rs.upload_bits_per_sec())
    );
    println!(
        "  avg download since boot: {}",
        humanize::human_bits_per_sec(rs.download_bits_per_sec())
    );
    println!("  active peers: {}", rs.active_peers);
    if rs.process_rss_bytes > 0 {
        println!(
            "  memory: {} RSS",
            humanize::human_bytes(rs.process_rss_bytes as i64)
        );
    }
    Ok(())
}

fn format_time(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}
