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

/// Build the node's main rqbit session: TCP-only listener on cfg.port, DHT
/// on, RAM-scaled per-torrent peer limit, global up/down rate limits,
/// keep-at seeder identity for tracker User-Agent and extended handshake.
///
/// The peer limit scales with the RAM budget (see peer_limit_for_budget):
/// peer buffers are the dominant per-torrent RAM term, so small hosts trade
/// per-torrent swarm speed for more held torrents.
pub async fn new_seeder_session(cfg: &Config, ram_budget: u64) -> Result<Arc<Session>> {
    let ratelimits = LimitsConfig {
        upload_bps: nonzero_u32(cfg.upload_rate_limit),
        download_bps: nonzero_u32(cfg.download_rate_limit),
    };

    // Default output folder is unused (every torrent passes output_folder
    // explicitly); point it at the data dir so nothing lands somewhere odd.
    //
    // Session creation binds listen + DHT sockets, which can transiently
    // collide right after a previous instance released them. Retry a few
    // times with gaps before giving up, so a watchdog restart (or a fast
    // manual stop/start) doesn't die on TIME_WAIT-style residue.
    // (SessionOptions isn't Clone, so this is a small closure rebuild.)
    let build_opts = || SessionOptions {
        listen: Some(ListenerOptions {
            listen_addr: SocketAddr::from(([0, 0, 0, 0], cfg.port)),
            ..ListenerOptions::default()
        }),
        connect: Some(ConnectionOptions {
            enable_tcp: true,
            ..ConnectionOptions::default()
        }),
        ratelimits,
        peer_limit: Some(crate::engine::ram::peer_limit_for_budget(ram_budget)),
        disable_local_service_discovery: true,
        client_name_and_version: Some(buildinfo::seeder_user_agent()),
        peer_id: Some(peer_id_from_prefix()),
        // IPv4-only sockets: on some hosts (observed: shared-seedbox
        // Gentoo) the dualstack `[::]` UDP bind for DHT collides with a
        // just-released socket and restart dies with EADDRINUSE, while a
        // plain 0.0.0.0 bind succeeds. DHT over IPv4 loses no AT
        // connectivity (trackers + IPv4 peers dominate).
        ipv4_only: true,
        ..Default::default()
    };
    let mut last_err = anyhow::anyhow!("session creation never attempted");
    for attempt in 1..=3 {
        match Session::new_with_opts(cfg.data_dir.clone(), build_opts()).await {
            Ok(session) => return Ok(session),
            Err(e) => {
                last_err = e.context(format!("creating rqbit session (attempt {attempt}/3)"));
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
    let listen = ListenerOptions {
        listen_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
        ..ListenerOptions::default()
    };

    let opts = SessionOptions {
        listen: Some(listen),
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

fn peer_id_from_prefix() -> librqbit_core::Id20 {
    let mut rng = rand::thread_rng();
    let id = buildinfo::make_peer_id(&mut rng);
    librqbit_core::Id20::new(id)
}

/// Graceful shutdown with a bounded wait.
pub async fn stop_session(session: &Arc<Session>, timeout: Duration) {
    let _ = tokio::time::timeout(timeout, session.stop()).await;
}
