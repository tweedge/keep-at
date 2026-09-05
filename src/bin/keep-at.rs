use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use keep_at::cli::{self, Cli, Command};
use keep_at::config::Config;

fn init_logging(debug: bool) {
    init_logging_to(debug, None);
}

fn init_logging_to(debug: bool, log_file: Option<&std::path::Path>) {
    let filter = if debug { "debug" } else { "info" };
    let env = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter));
    if let Some(path) = log_file {
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            tracing_subscriber::fmt()
                .with_env_filter(env)
                .with_target(false)
                .compact()
                .with_writer(std::sync::Mutex::new(f))
                .init();
            return;
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(env)
        .with_target(false)
        .compact()
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Command::Run(a) => {
            let cfg = cli::resolve(&a.common, &a.cfg)?;
            init_logging_to(cfg.debug, cfg.log_file.as_deref());
            cmd_run(cfg).await
        }
        Command::Start(a) => {
            let cfg = cli::resolve(&a.common, &a.cfg)?;
            init_logging(false);
            cmd_start(cfg, a.foreground).await
        }
        Command::Stop(a) => cmd_stop(&a),
        Command::Status(a) => keep_at::status::cmd_status(&a),
        Command::Service(s) => cmd_service(s).await,
        Command::NetworkStatus(a) => {
            init_logging(false);
            keep_at::census::cmd_network_status(&a).await
        }
        Command::HostedTorrents(a) => keep_at::hosted::cmd_hosted(&a),
        Command::SelfUpdate(a) => cmd_self_update(a.beta).await,
        Command::Version => {
            println!("keep-at {}", keep_at::buildinfo::VERSION);
            Ok(())
        }
    }
}

async fn cmd_run(cfg: Config) -> Result<()> {
    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("creating data dir {}", cfg.data_dir.display()))?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    {
        let tx = shutdown_tx.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("received SIGINT, shutting down");
            let _ = tx.send(true);
        });
    }
    #[cfg(unix)]
    {
        let tx = shutdown_tx.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            if let Ok(mut sig) = signal(SignalKind::terminate()) {
                sig.recv().await;
                tracing::info!("received SIGTERM, shutting down");
                let _ = tx.send(true);
            }
        });
    }

    let started = std::time::Instant::now();
    let mut engine = keep_at::engine::Engine::new(cfg.clone()).await?;
    tracing::info!(
        "keep-at started (port={} data_dir={} startup_time={:?})",
        cfg.port,
        cfg.data_dir.display(),
        started.elapsed()
    );
    // Bounded total runtime after a signal: run the engine with a shutdown
    // deadline (mirrors TimeoutStopSec=30). The engine checks the watch
    // channel between phases, but spawned workers can wedge (60s backoff,
    // hung fetch); the deadline guarantees the process exits regardless.
    // State on disk is already consistent (atomic writes).
    let mut shutdown_rx2 = shutdown_tx.subscribe();
    let r = tokio::select! {
        r = engine.run(shutdown_rx) => r,
        _ = async {
            // Wait for any signal, then give the engine 20s to wind down.
            let _ = shutdown_rx2.wait_for(|v| *v).await;
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        } => {
            tracing::warn!("shutdown deadline exceeded, exiting");
            Ok(())
        }
    };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine.close()).await;
    tracing::info!("keep-at stopped");
    r
}

async fn cmd_start(cfg: Config, foreground: bool) -> Result<()> {
    if foreground || keep_at::daemonctl::is_containerized() {
        // Behave as run (foreground) inside containers or on request.
        return cmd_run(cfg).await;
    }
    // Resolve + validate now so a bad config fails here, not in the daemon.
    let mut cfg = cfg;
    if cfg.log_file.is_none() {
        cfg.log_file = Some(cfg.data_dir.join("keep-at.log"));
    }
    let data_dir = cfg.data_dir.clone();
    let tmp_path = data_dir.join("config.resolved.yaml");
    cfg.save(&tmp_path)?;
    let exe = std::env::current_exe().context("locating keep-at executable")?;
    // The daemon runs `run --config <resolved>` exactly: --data-dir must NOT
    // be passed alongside --config (resolve() rejects storage flags with a
    // file, and --data-dir would shadow the file's value). status/stop match
    // the daemon via its --config argv (see find_foreground).
    let pid = keep_at::daemonctl::setsid_spawn(
        &exe,
        &[
            "run".to_string(),
            "--config".to_string(),
            tmp_path.to_string_lossy().into_owned(),
        ],
    )?;
    // Record the daemon PID for stop/status.
    std::fs::write(keep_at::daemonctl::pid_path(&data_dir), format!("{pid}\n"))?;
    println!("keep-at started in the background (pid {pid})");
    Ok(())
}

fn cmd_stop(args: &keep_at::cli::CommonArgs) -> Result<()> {
    let dir = cli::resolve_data_dir(args)?;
    let st = keep_at::daemonctl::status(&dir);
    let pid = match (st.running, st.pid) {
        (true, Some(pid)) => pid,
        (true, None) => {
            anyhow::bail!("keep-at is running in the foreground; stop it directly (Ctrl-C / kill)");
        }
        (false, _) => {
            println!("keep-at is not running");
            return Ok(());
        }
    };
    send_sigterm(pid)?;
    // Wait up to 30s for exit (mirrors TimeoutStopSec=30).
    for _ in 0..60 {
        if !keep_at::daemonctl::pid_alive(pid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    if keep_at::daemonctl::pid_alive(pid) {
        anyhow::bail!("keep-at (pid {pid}) did not stop within 30s");
    }
    keep_at::daemonctl::remove_pid(&dir);
    println!("keep-at stopped");
    Ok(())
}

#[cfg(unix)]
fn send_sigterm(pid: u32) -> Result<()> {
    // SAFETY: kill with SIGTERM; ESRCH (no such process) is fine.
    let rc = libc_kill(pid as i32, 15);
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::NotFound {
            anyhow::bail!("signalling pid {pid}: {e}");
        }
    }
    Ok(())
}

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
fn libc_kill(pid: i32, sig: i32) -> i32 {
    unsafe { kill(pid, sig) }
}

async fn cmd_service(s: keep_at::cli::ServiceArgs) -> Result<()> {
    match s.op {
        keep_at::cli::ServiceOp::Install(a) => {
            let cfg = cli::resolve(&a.common, &a.cfg)?;
            let exe = std::env::current_exe().context("locating keep-at executable")?;
            keep_at::service::install(&keep_at::service::InstallOpts {
                exec_path: exe.to_string_lossy().into_owned(),
                config: cfg,
                user: a.user,
            })?;
            println!(
                "keep-at service installed and started, config at {}",
                keep_at::service::CONFIG_PATH
            );
            Ok(())
        }
        keep_at::cli::ServiceOp::Uninstall => {
            keep_at::service::uninstall()?;
            println!(
                "keep-at service uninstalled (config left in place at {})",
                keep_at::service::CONFIG_PATH
            );
            Ok(())
        }
    }
}

async fn cmd_self_update(beta: bool) -> Result<()> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let ua = keep_at::buildinfo::user_agent();
    let current = keep_at::buildinfo::VERSION;
    let latest = keep_at::updater::latest_version(&http, &ua, beta).await?;
    if latest.trim_start_matches('v') == current.trim_start_matches('v') || latest == current {
        println!("keep-at is already up to date ({current})");
        return Ok(());
    }
    println!("updating keep-at {current} -> {latest}...");
    let exe = std::env::current_exe().context("locating keep-at executable")?;
    let _exe_dir: PathBuf = exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let new_version = keep_at::updater::apply(&http, &ua, &exe, beta).await?;
    println!("updated to {new_version}; restart keep-at to run it");
    Ok(())
}
