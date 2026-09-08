//! `keep-at status`: running state + runtime summary.
//!
//! Live-first: when the daemon is running, numbers come straight from it
//! over the query socket (instantaneous, never stale). Otherwise — or when
//! the socket is unreachable — falls back to the persisted snapshot files.

use anyhow::Result;

use crate::cli::CommonArgs;
use crate::daemonctl;
use crate::humanize;
use crate::live;
use crate::netstats;

pub fn cmd_status(args: &CommonArgs) -> Result<()> {
    let dir = crate::cli::resolve_data_dir(args)?;
    // Best-effort repair: a root-owned data dir from an older/umask-strict
    // run may block traversal for this user. Repair only helps when WE own
    // an ancestor (no-op otherwise — never an error).
    crate::config::ensure_shared_dirs(&dir);

    let st = daemonctl::status(&dir);
    // Ask the daemon once: the answer reconciles the running line below
    // (socket works => daemon lives, even if pid-file/proc detection failed)
    // and decides live-vs-fallback stats.
    let live = live::query(&dir, &live::Request::Runtime);
    if st.running {
        match st.pid {
            Some(pid) => println!("keep-at is running (pid {pid})"),
            None => println!("keep-at is running in the foreground, not as a service"),
        }
    } else if live.is_some() {
        println!("keep-at is running (pid file missing or stale; live socket answered)");
    } else {
        println!("keep-at is not running");
    }

    // Live daemon answers from in-process state; files are the offline path.
    if let Some(live::Response::Runtime(v)) = live {
        print_live(&v);
        return Ok(());
    }
    if st.running {
        // A daemon is up but didn't answer: either a pre-socket binary
        // (upgraded keep-at, not yet restarted) or an unreadable socket.
        // Say so explicitly — silently showing a stale snapshot reads as
        // live data and has confused operators before.
        println!(
            "  (live stats unavailable — daemon may need a restart after upgrading; showing the last persisted snapshot)"
        );
    }
    print_snapshot(&netstats::load_runtime(&dir.join("runtime-stats.json"))?);
    Ok(())
}

fn print_live(v: &live::RuntimeView) {
    println!(
        "runtime stats (live, uptime {}):",
        humanize::human_duration(std::time::Duration::from_secs(v.uptime_seconds))
    );
    print_numbers(
        v.held_torrents,
        v.seeding_torrents,
        v.downloading_torrents,
        v.disk_used_bytes,
        v.disk_limit_bytes,
        v.useful_bytes_uploaded,
        v.total_bytes_uploaded,
        v.useful_bytes_downloaded,
        v.total_bytes_downloaded,
        v.uptime_seconds,
        v.active_peers,
        v.process_rss_bytes,
    );
}

fn print_snapshot(rs: &netstats::RuntimeStats) {
    if rs.collected_at.is_none() {
        return;
    }
    println!(
        "runtime stats (snapshot from {}, uptime {}):",
        rs.collected_at.map(format_time).unwrap_or_default(),
        humanize::human_duration(rs.uptime())
    );
    print_numbers(
        rs.held_torrents,
        rs.seeding_torrents,
        rs.downloading_torrents,
        rs.disk_used_bytes,
        rs.disk_limit_bytes,
        rs.useful_bytes_uploaded,
        rs.total_bytes_uploaded,
        rs.useful_bytes_downloaded,
        rs.total_bytes_downloaded,
        rs.uptime_seconds,
        rs.active_peers,
        rs.process_rss_bytes,
    );
}

#[allow(clippy::too_many_arguments)]
fn print_numbers(
    held: usize,
    seeding: usize,
    downloading: usize,
    disk_used: u64,
    disk_limit: u64,
    up_useful: u64,
    up_total: u64,
    down_useful: u64,
    down_total: u64,
    uptime_secs: u64,
    peers: usize,
    rss: u64,
) {
    println!("  torrents: {held} held, {seeding} seeding, {downloading} downloading");
    if disk_limit > 0 {
        let pct = if disk_limit > 0 {
            (disk_used as f64 / disk_limit as f64 * 100.0).min(100.0)
        } else {
            0.0
        };
        println!(
            "  disk: {} used of {} configured ({:.1}%)",
            humanize::human_bytes(disk_used as i64),
            humanize::human_bytes(disk_limit as i64),
            pct
        );
    }
    println!(
        "  useful upload since boot: {} (total network {})",
        humanize::human_bytes(up_useful as i64),
        humanize::human_bytes(up_total as i64)
    );
    println!(
        "  useful download since boot: {} (total network {})",
        humanize::human_bytes(down_useful as i64),
        humanize::human_bytes(down_total as i64)
    );
    println!(
        "  avg upload since boot: {}",
        humanize::human_bits_per_sec(bits_per_sec(up_total, uptime_secs))
    );
    println!(
        "  avg download since boot: {}",
        humanize::human_bits_per_sec(bits_per_sec(down_total, uptime_secs))
    );
    println!("  active peers: {peers}");
    if rss > 0 {
        println!("  memory: {} RSS", humanize::human_bytes(rss as i64));
    }
}

fn bits_per_sec(total_bytes: u64, uptime_secs: u64) -> f64 {
    if uptime_secs == 0 {
        return 0.0;
    }
    total_bytes as f64 * 8.0 / uptime_secs as f64
}

fn format_time(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}
