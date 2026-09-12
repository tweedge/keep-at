//! rqbit session construction and torrent lifecycle helpers.
//! One Session (seeder identity) for the node; the census builds its own
//! short-lived sessions with the scraper identity.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use librqbit::limits::LimitsConfig;
use librqbit::{ConnectionOptions, ListenerOptions, Session, SessionOptions};

use crate::buildinfo;
use crate::config::Config;

/// Listener options shared by the seeder and probe sessions: TCP bind on
/// `addr`, plus the upstream select!-panic workaround (raised pending-
/// handshake cap — see the comment inside). Factored for the regression test.
pub fn listener_options(addr: SocketAddr, ipv4_only: bool) -> ListenerOptions {
    ListenerOptions {
        listen_addr: addr,
        ipv4_only,
        // Upstream bug workaround (observed 2026-09-12, SIGABRT during a
        // 4TB-library boot): rqbit's listener task select!s between
        // accept (disabled when pending handshakes >= cap) and handshake
        // completion matched as `Some(Ok(..))`. A failed handshake check
        // (torrent still Initializing past the 5s live-wait, routine on
        // big boots) fails that pattern; if the pending queue is also at
        // cap at that instant, ALL branches are disabled and the select!
        // panics — with panic=abort that kills the whole daemon, and the
        // watchdog restart can re-enter the same window (crash loop).
        // Still unfixed upstream as of librqbit 9.0.1 / rqbit master
        // 2026-09. Raising the cap to usize::MAX keeps the accept branch
        // permanently enabled, so the select! can never go all-disabled;
        // the queue is bounded in practice by connection rate and each
        // entry lives at most the 5s handshake-check timeout.
        max_pending_incoming_handshake_checks: usize::MAX,
        ..ListenerOptions::default()
    }
}

/// Build the node's main rqbit session: TCP-only listener on cfg.port, DHT
/// on, RAM-scaled per-torrent peer limit, global up/down rate limits,
/// keep-at seeder identity for tracker User-Agent and extended handshake.
///
/// The peer limit scales with the RAM budget (see peer_limit_for_budget):
/// peer buffers are the dominant per-torrent RAM term, so small hosts trade
/// per-torrent swarm speed for more held torrents.
pub async fn new_seeder_session(cfg: &Config, ram_budget: u64) -> Result<Arc<Session>> {
    let ratelimits = session_rate_limits(cfg);

    // Default output folder is unused (every torrent passes output_folder
    // explicitly); point it at the data dir so nothing lands somewhere odd.
    //
    // Socket strategy: dualstack first (IPv6 + IPv4 on `[::]`), IPv4-only
    // fallback when the dualstack bind collides. On some hosts (observed:
    // shared-seedbox Gentoo) a just-released dualstack socket lingers and
    // re-binding `[::]` fails with EADDRINUSE while 0.0.0.0 succeeds - so a
    // watchdog restart right after a stop would die without the fallback.
    // IPv6 stays fully supported: the fallback only triggers when dualstack
    // actually fails, and session creation retries either way (below), so a
    // transient collision resolves on the next attempt.
    // (SessionOptions isn't Clone, so this is a small closure rebuild.)
    let build_opts = |ipv4_only: bool| SessionOptions {
        listen: Some(listener_options(
            SocketAddr::from(([0, 0, 0, 0], cfg.port)),
            ipv4_only,
        )),
        connect: Some(ConnectionOptions {
            enable_tcp: true,
            ..ConnectionOptions::default()
        }),
        ratelimits,
        peer_limit: Some(crate::engine::ram::peer_limit_for_budget(ram_budget)),
        disable_local_service_discovery: true,
        client_name_and_version: Some(buildinfo::seeder_user_agent()),
        peer_id: Some(peer_id_from_prefix()),
        ipv4_only,
        ..Default::default()
    };
    let mut last_err = anyhow::anyhow!("session creation never attempted");
    // Attempt 1: dualstack. Attempts 2-3: IPv4-only fallback, then one more
    // dualstack try in case the residue cleared (preserves IPv6 whenever the
    // collision was transient).
    for (attempt, (ipv4_only, mode)) in [
        (false, "dualstack"),
        (true, "ipv4-only"),
        (false, "dualstack"),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, pair)| (i + 1, pair))
    {
        match Session::new_with_opts(cfg.data_dir.clone(), build_opts(ipv4_only)).await {
            Ok(session) => {
                if ipv4_only {
                    tracing::warn!(
                        "dualstack socket bind failed; running IPv4-only this boot (retry dualstack on next restart)"
                    );
                }
                return Ok(session);
            }
            Err(e) => {
                last_err = e.context(format!(
                    "creating rqbit session (attempt {attempt}/3, {mode})"
                ));
                if attempt < 3 {
                    tracing::warn!("{last_err:#}; retrying in 10s");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        }
    }
    Err(last_err)
}

/// Build a short-lived census probe session with the scraper identity,
/// ephemeral listen port, DHT off (tracker-only discovery is enough to
/// answer "who else is in this swarm"), and no rate limits of its own.
pub async fn new_probe_session(data_dir: PathBuf) -> Result<Arc<Session>> {
    let opts = SessionOptions {
        listen: Some(listener_options(SocketAddr::from(([0, 0, 0, 0], 0)), false)),
        dht: None,
        peer_limit: Some(8),
        disable_local_service_discovery: true,
        client_name_and_version: Some(buildinfo::scraper_user_agent()),
        peer_id: Some(peer_id_from_prefix()),
        ..Default::default()
    };

    Session::new_with_opts(data_dir, opts)
        .await
        .context("creating probe session")
}

fn nonzero_u32(v: u64) -> Option<std::num::NonZeroU32> {
    u32::try_from(v).ok().and_then(std::num::NonZeroU32::new)
}

/// Resolve the effective session rate limits for a config: `0` (unset)
/// means unlimited (None); anything else caps that direction session-wide.
/// Values above u32::MAX saturate to unlimited rather than wrapping -
/// no sane link exceeds 4 GiB/s, and a wrapped tiny limit would
/// mysteriously stall all transfers.
pub fn session_rate_limits(cfg: &Config) -> librqbit::limits::LimitsConfig {
    LimitsConfig {
        upload_bps: nonzero_u32(cfg.upload_rate_limit),
        download_bps: nonzero_u32(cfg.download_rate_limit),
    }
}

fn peer_id_from_prefix() -> librqbit_core::Id20 {
    let mut rng = rand::thread_rng();
    let id = buildinfo::make_peer_id(&mut rng);
    librqbit_core::Id20::new(id)
}

/// Graceful shutdown with a bounded wait.
pub async fn stop_session(session: &Arc<Session>, timeout: Duration) {
    let _ = tokio::time::timeout(timeout, session.stop()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, StorageLimit, StorageLocation};

    /// Regression: the pending-handshake cap must be raised above any
    /// reachable value so rqbit's listener select! (which disables its
    /// accept branch at the cap and panics when a failed handshake then
    /// leaves no enabled branch — observed as a SIGABRT daemon death on a
    /// 4TB-library boot, 2026-09-12) can never go all-disabled. Upstream
    /// still matches handshake results as `Some(Ok(..))` with no else.
    #[test]
    fn listener_pending_cap_never_disables_accept() {
        let opts = listener_options(SocketAddr::from(([0, 0, 0, 0], 37550)), false);
        assert_eq!(
            opts.max_pending_incoming_handshake_checks,
            usize::MAX,
            "accept branch must stay enabled at any pending-queue length"
        );
        let probe = listener_options(SocketAddr::from(([0, 0, 0, 0], 0)), false);
        assert_eq!(probe.max_pending_incoming_handshake_checks, usize::MAX);
    }

    fn cfg_with_limits(up: u64, down: u64) -> Config {
        let mut cfg = Config {
            storage: vec![StorageLocation {
                path: "/tmp/keep-at-test-storage".into(),
                limit: StorageLimit::Bytes(1 << 30),
            }],
            upload_rate_limit: up,
            download_rate_limit: down,
            ..Config::default()
        };
        cfg.scan.interval = Duration::from_secs(60);
        cfg
    }

    #[test]
    fn unlimited_by_default() {
        let lim = session_rate_limits(&cfg_with_limits(0, 0));
        assert_eq!(lim.upload_bps, None);
        assert_eq!(lim.download_bps, None);
    }

    #[test]
    fn limits_pass_through() {
        let lim = session_rate_limits(&cfg_with_limits(50 * 1024 * 1024, 20 * 1024 * 1024));
        assert_eq!(lim.upload_bps.map(|v| v.get()), Some(50 * 1024 * 1024));
        assert_eq!(lim.download_bps.map(|v| v.get()), Some(20 * 1024 * 1024));
    }

    #[test]
    fn overflow_saturates_to_unlimited() {
        let lim = session_rate_limits(&cfg_with_limits(u64::MAX, u64::MAX));
        assert_eq!(lim.upload_bps, None);
        assert_eq!(lim.download_bps, None);
    }

    #[tokio::test]
    async fn session_reports_configured_limits() {
        // End-to-end: a real session exposes exactly the configured caps via
        // its live Limits handle (the same handle the peer data path
        // acquires through on every chunk).
        let cfg = cfg_with_limits(1024 * 1024, 2 * 1024 * 1024);
        let dir = std::env::temp_dir().join(format!("keep-at-limit-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let session = Session::new_with_opts(
            dir.clone(),
            SessionOptions {
                ratelimits: session_rate_limits(&cfg),
                dht: None,
                disable_local_service_discovery: true,
                ..Default::default()
            },
        )
        .await
        .expect("session builds");
        assert_eq!(
            session.ratelimits.get_upload_bps().map(|v| v.get()),
            Some(1024 * 1024)
        );
        assert_eq!(
            session.ratelimits.get_download_bps().map(|v| v.get()),
            Some(2 * 1024 * 1024)
        );
        stop_session(&session, Duration::from_secs(5)).await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
