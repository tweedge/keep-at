//! FD-aware admission: file count rides the metadata path end to end,
//! and the unit template carries a LimitNOFILE that survives systemd's
//! 1024 default.
//!
//! Asserts through the public surface only: parsed `file_count` on single
//! vs multi-file metainfo (via the shared harness builders), the pure
//! `fits_in_headroom` math, and the service unit template text.

mod common;

use common::Fixture;

#[test]
fn parse_exposes_fd_cost() {
    // Single-file: 1 fd. Multi-file: one fd per file.
    let single = common::torrent_bytes(
        &Fixture::new("fd-one", 3000, 1),
        "http://127.0.0.1:9/announce",
    );
    let md = keep_at::attorrent::parse_torrent_bytes(&single).expect("single parses");
    assert_eq!(md.file_count, 1, "single-file torrent costs 1 fd");

    let multi = common::torrent_bytes(
        &Fixture::multi_file("fd-many", 3000, 1, 7),
        "http://127.0.0.1:9/announce",
    );
    let md = keep_at::attorrent::parse_torrent_bytes(&multi).expect("multi parses");
    assert_eq!(md.file_count, 7, "7-file torrent costs 7 fds");
    assert_eq!(md.total_length, 3000, "content size splits, not multiplies");
}

#[test]
fn guard_admits_small_refuses_huge() {
    use keep_at::fdlimit::{fits_in_headroom, SOCKET_RESERVE};
    // Ordinary torrent against a healthy daemon: admits.
    assert!(fits_in_headroom(10, Some(50_000)));
    // Many-file imagery set against an exhausted host: refuses.
    assert!(!fits_in_headroom(20_000, Some(SOCKET_RESERVE + 100)));
    // Unknown headroom (/proc unreadable) never refuses.
    assert!(fits_in_headroom(u64::MAX, None));
}

#[test]
fn live_headroom_is_coherent() {
    // Queries the real rlimit + /proc/self/fd: soft/headroom/open agree
    // within the readdir's own transient fd.
    let Some((soft, hard)) = keep_at::fdlimit::current_limits() else {
        return; // non-Linux CI: nothing to assert.
    };
    assert!(soft > 0 && soft <= hard, "soft={soft} hard={hard}");
    let open = keep_at::fdlimit::open_count().expect("fd count readable");
    let headroom = keep_at::fdlimit::fd_headroom().expect("headroom readable");
    assert!(
        soft.saturating_sub(headroom).abs_diff(open) <= 4,
        "soft={soft} headroom={headroom} open={open}"
    );
}

#[test]
fn unit_template_raises_nofile() {
    // The systemd service must not inherit systemd's 1024 default: a few
    // hundred held torrents (one fd per file each) plus peer sockets
    // exhaust that on any real node (observed live: EMFILE on add).
    let src = include_str!("../src/service.rs");
    assert!(
        src.contains("LimitNOFILE=65536"),
        "unit template must set LimitNOFILE"
    );
    assert_eq!(
        keep_at::fdlimit::TARGET_SOFT_LIMIT,
        65536,
        "startup raise target must match the unit file"
    );
}
