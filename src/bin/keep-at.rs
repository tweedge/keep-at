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
        // World-readable log: operators tail it without root. Created with
        // an explicit 0o644 (ignoring umask), same policy as snapshots.
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o644)
            .open(path)
        {
            // Pre-existing log from an older/umask-restricted run: open
            // read access for everyone (best effort, never fatal).
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
            }
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
    // Restore default SIGPIPE (Rust's runtime ignores it) ONLY for short-lived
    // print-style commands, so `keep-at hosted-torrents | head` exits cleanly.
    // The daemon arms (run/start/service) must NEVER see SIG_DFL: every write
    // to a broken pipe or a closed stdout/stderr (systemd gives the daemon
    // journald stream sockets; journald restarts break them) would kill the
    // daemon instantly with SIGPIPE — silent, unloggable, and historically
    // indistinguishable from SIGKILL in the journal. Measured on mercury
    // 2026-09-11: std socket writes use MSG_NOSIGNAL (immune), but raw
    // pipe/journal-fd writes with SIG_DFL die with exit 141.
    // SIG_DFL is 0 (SIG_IGN is 1 - which makes writes return EPIPE and
    // println! panic; that was tried and is wrong).
    #[cfg(unix)]
    if restores_sigpipe_default(&cli.cmd) {
        libc_signal(13, 0); // SIGPIPE -> SIG_DFL
    }
    match cli.cmd {
        Command::Run(a) => {
            let cfg = cli::resolve(&a.common, &a.cfg)?;
            init_logging_to(cfg.debug, cfg.log_file.as_deref());
            cmd_run(cfg, a.common.config.clone()).await
        }
        Command::Start(a) => {
            let cfg = cli::resolve(&a.common, &a.cfg)?;
            init_logging(false);
            cmd_start(cfg, a.foreground).await
        }
        Command::Stop(a) => cmd_stop(&a),
        Command::Status(a) => keep_at::status::cmd_status(&a),
        Command::Logs(a) => keep_at::logs::cmd_logs(&a),
        Command::Service(s) => cmd_service(s).await,
        Command::NetworkStatus(a) => {
            init_logging(false);
            keep_at::census::cmd_network_status(&a).await
        }
        Command::HostedTorrents(a) => keep_at::hosted::cmd_hosted(&a),
        Command::DeathEvidence(a) => {
            let dir = cli::resolve_data_dir(&a)?;
            keep_at::forensics::print_death_evidence(&dir);
            Ok(())
        }
        Command::SelfUpdate(a) => cmd_self_update(a.beta).await,
        Command::Version => {
            println!("keep-at {}", keep_at::buildinfo::VERSION);
            Ok(())
        }
    }
}

async fn cmd_run(cfg: Config, config_path: Option<PathBuf>) -> Result<()> {
    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("creating data dir {}", cfg.data_dir.display()))?;
    // Record OUR pid (world-readable): status/stop work for flag-run,
    // systemd, and start-launched daemons alike. Previously only `start`
    // wrote a pid file — and wrote the parent's, stale from birth — so a
    // flag-run daemon reported "not running" whenever the /proc fallback
    // scan failed to reach it.
    keep_at::daemonctl::write_pid(&cfg.data_dir)?;
    // Split-secret migration (pre-0.8.11 configs): API key inline in an
    // owner-only config file. As the daemon user (the owner) rewrite the
    // config world-readable; the key lands in <data_dir>/api_key below.
    // Best effort: a failure leaves the legacy shape in place, never fatal.
    if let Some(p) = &config_path {
        if let Err(e) = Config::split_inline_secret(p, &cfg) {
            tracing::warn!("could not migrate config secrets ({}): {e:#}", p.display());
        }
    }
    // Key file is the runtime home of the API key (owner-only 0600), written
    // whenever a key is configured — flag runs included — so restarts via the
    // same config keep attribution without re-passing --api-key.
    if !cfg.api_key.is_empty() {
        if let Err(e) = cfg.write_api_key_file() {
            tracing::warn!("could not persist api key file: {e:#}");
        }
    }
    // Data-dir pointer: world-readable breadcrumb so `status` /
    // `hosted-torrents` as any user find this instance without reading the
    // config. Only written in a service context (/etc/keep-at exists).
    keep_at::service::write_data_dir_pointer(&cfg.data_dir);
    // Raise the fd soft limit before the session opens anything: a few
    // hundred held torrents (one fd per file each) plus peer sockets
    // otherwise exhaust low defaults (systemd's 1024 without LimitNOFILE).
    keep_at::fdlimit::raise_soft_limit();

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

    // Bind the query socket IMMEDIATELY in a booting state: `status` and
    // `hosted-torrents` answer truthfully from the first millisecond
    // ("starting up: resuming N held torrents") instead of reporting a
    // stale snapshot with a "may need a restart" warning for the whole
    // Engine::new window (minutes on slow hosts, 144-torrent resumes).
    let started_at = std::time::Instant::now();
    let storage: Vec<(std::path::PathBuf, u64)> = cfg
        .storage
        .iter()
        .map(|l| (l.path.clone(), l.limit_bytes()))
        .collect();
    let held = keep_at::state::State::load(&cfg.data_dir.join("state.json"))
        .map(|st| {
            st.all()
                .into_iter()
                .map(|t| keep_at::live::HeldTorrentView {
                    title: t.title,
                    info_hash: t.info_hash,
                    size_bytes: t.size_bytes,
                    progress_bytes: 0,
                    finished: false,
                    verifying: true,
                    last_known_seeders: t.last_known_seeders,
                })
                .collect()
        })
        .unwrap_or_default();
    let live = keep_at::live::LiveHandle::booting(started_at, storage, held);
    {
        let h = live.clone();
        let dir = cfg.data_dir.clone();
        tokio::spawn(async move {
            keep_at::live::serve(dir, h).await;
        });
    }

    // Kill forensics: catchable-signal death marks go to the log file
    // (SIGKILL can't be logged - that silence is itself the diagnostic);
    // the heartbeat task records RSS + cgroup state every 60s, and the
    // cgroup memory.events oom_kill counter is readable post-mortem.
    // Debug-kill-forensics build: shipped to find what is killing the
    // daemon on shared hosts.
    let log_path = cfg
        .log_file
        .clone()
        .unwrap_or_else(|| cfg.data_dir.join("keep-at.log"));
    // Belt and braces against SIGPIPE deaths (measured 2026-09-11, see main()):
    // whatever our ancestors set, the daemon itself pins SIGPIPE to ignored.
    // Socket writes are already immune (std sends with MSG_NOSIGNAL); this
    // covers pipes and inherited stderr/stdout fds.
    #[cfg(unix)]
    libc_signal(13, 1); // SIGPIPE -> SIG_IGN (broken-pipe writes return EPIPE)
                        // Redirect stdout+stderr into the log file: the daemon must never hold a
                        // pipe or journald stream fd (systemd hands out journal sockets; journald
                        // restarts break them, and ANY later write would EPIPE/SIGPIPE). Panic
                        // messages land in keep-at.log instead of vanishing with the process.
    #[cfg(unix)]
    redirect_stdio_to(&log_path);
    keep_at::forensics::install_signal_death_marks(&log_path);
    keep_at::forensics::install_panic_hook(&log_path);
    keep_at::forensics::heartbeat_task(cfg.data_dir.clone(), std::time::Instant::now());
    // Debug knob: KEEPAT_DEBUG_PANIC=1 schedules an intentional panic in a
    // background thread 20s after boot. Used to measure death handling
    // (panic hook -> log file, daemon must survive).
    #[cfg(unix)]
    if std::env::var_os("KEEPAT_DEBUG_PANIC").is_some() {
        tracing::info!("KEEPAT_DEBUG_PANIC: intentional panic scheduled in 20s");
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(20));
            panic!("KEEPAT_DEBUG_PANIC: intentional test panic");
        });
    }

    let started = std::time::Instant::now();
    let mut engine = keep_at::engine::Engine::new(cfg.clone()).await?;
    engine.attach_live(live);
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
    // Clean shutdown: drop our pid file so status doesn't report a dead
    // pid (SIGKILL'd daemons leave the liveness check to handle it).
    keep_at::daemonctl::remove_pid(&cfg.data_dir);
    tracing::info!("keep-at stopped");
    r
}

async fn cmd_start(cfg: Config, foreground: bool) -> Result<()> {
    if foreground || keep_at::daemonctl::is_containerized() {
        // Behave as run (foreground) inside containers or on request.
        // No config file path: `start` resolves flags into the saved config.
        return cmd_run(cfg, None).await;
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
    // The daemon records its own pid on boot (see cmd_run): this parent
    // exits in a moment, so writing here would leave a stale file.
    println!("keep-at started in the background (pid {pid})");
    Ok(())
}

fn cmd_stop(args: &keep_at::cli::CommonArgs) -> Result<()> {
    let dir = cli::resolve_data_dir(args)?;
    let mut st = keep_at::daemonctl::status(&dir);
    let explicit = args.data_dir.is_some() || args.config.is_some();
    if !st.running && !explicit {
        // Nothing at the resolved dir: same discovery fallback as bare
        // status — a flag-run daemon with a non-default data dir is still
        // stoppable, but ONLY for bare `stop` (an explicitly targeted stop
        // must never kill someone else's instance).
        if let Some((pid, other)) = keep_at::daemonctl::find_any_daemon() {
            if keep_at::daemonctl::pid_alive(pid) {
                st = keep_at::daemonctl::Status {
                    running: true,
                    pid: Some(pid),
                };
                println!(
                    "note: resolved data dir is {}, stopping the daemon using {} (pass --data-dir {} next time to skip this lookup)",
                    dir.display(),
                    other.display(),
                    other.display()
                );
            }
        }
    }
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
    fn signal(signum: i32, handler: usize) -> usize;
    fn dup2(oldfd: i32, newfd: i32) -> i32;
}

#[cfg(unix)]
fn libc_signal(signum: i32, handler: usize) -> usize {
    unsafe { signal(signum, handler) }
}

/// True for short-lived print-style commands where dying by SIGPIPE on a
/// closed downstream pipe (`... | head`) is the normal, expected behavior.
/// Never true for the daemon arms — their children inherit the disposition,
/// and a daemon with SIGPIPE at SIG_DFL dies silently on any write to a
/// broken pipe or closed journal stream (measured 2026-09-11).
#[cfg(unix)]
fn restores_sigpipe_default(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Status(_)
            | Command::Logs(_)
            | Command::HostedTorrents(_)
            | Command::NetworkStatus(_)
            | Command::Version
            | Command::DeathEvidence(_)
            | Command::Stop(_)
    )
}

/// Point fds 1 and 2 at `log_path` (append). The daemon then has no pipe or
/// journal-stream fds on its stdout/stderr, so no write anywhere can hit a
/// closed reader. Best effort: on failure the original fds stay (harmless —
/// SIGPIPE is already pinned to SIG_IGN above).
#[cfg(unix)]
fn redirect_stdio_to(log_path: &std::path::Path) {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o644)
        .open(log_path);
    if let Ok(fd) = f {
        let fd = std::os::unix::io::IntoRawFd::into_raw_fd(fd);
        unsafe {
            dup2(fd, 1);
            dup2(fd, 2);
            if fd > 2 {
                close(fd);
            }
        }
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn close(fd: i32) -> i32;
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
    // Never "update" sideways or backwards: the stable channel resolves
    // to the latest STABLE (x.y), which can lag a running beta (e.g.
    // running 0.9.1-beta while stable is 0.9). Suggest --beta instead of
    // downloading an older binary over a newer one.
    if !beta && version_older_or_equal(&latest, current) {
        println!(
            "latest stable is {latest}, but this binary is {current} (newer). Nothing to do — pass --beta to track prereleases."
        );
        return Ok(());
    }
    println!("updating keep-at {current} -> {latest}...");
    let exe = std::env::current_exe().context("locating keep-at executable")?;
    // Root-owned binary in /usr/local/bin or /usr/bin: a plain replace
    // fails with permission denied. Elevate (re-exec self, same policy as
    // `service install` — see service::elevate) so `self-update` works
    // without a manual sudo prefix, using the exact same argv.
    if let Err(e) = can_replace_file(&exe) {
        tracing::debug!("binary not replaceable in place ({e:#}); elevating");
        keep_at::service::elevate("replace the keep-at binary for self-update")?;
        // elevate() only returns when already root; re-check after it.
        can_replace_file(&exe).with_context(|| {
            format!(
                "binary {} still not replaceable after elevation",
                exe.display()
            )
        })?;
    }
    let _exe_dir: PathBuf = exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let new_version = keep_at::updater::apply(&http, &ua, &exe, beta).await?;
    println!("updated to {new_version}; restart keep-at to run it");
    Ok(())
}

/// Compare dotted version strings (leading 'v' stripped): true when `a` is
/// older than or equal to `b`. Non-numeric segments compare as 0. Used to
/// refuse downgrades on the stable channel (running beta vs lagging stable).
fn version_older_or_equal(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.trim_start_matches('v')
            .split('.')
            .map(|p| {
                p.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect()
    }
    let (pa, pb) = (parts(a), parts(b));
    let n = pa.len().max(pb.len());
    for i in 0..n {
        let (x, y) = (
            pa.get(i).copied().unwrap_or(0),
            pb.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x < y;
        }
    }
    true // equal
}

/// Can this process replace `exe` in place? Probes writability of the
/// binary's directory (where the atomic-replace temp file lands) without
/// writing anything lasting: if the directory isn't writable, self-update
/// would fail with permission denied, so the caller elevates first.
fn can_replace_file(exe: &std::path::Path) -> Result<()> {
    let dir = exe.parent().context("executable has no parent dir")?;
    // Simplest honest probe is attempting the temp file the updater itself
    // would write, then removing it. Suffix differs so a concurrent real
    // update never collides.
    let probe = dir.join(format!(".keep-at-writable-{}", std::process::id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("{} is not writable", dir.display())),
    }
}

#[cfg(test)]
mod self_update_tests {
    use super::version_older_or_equal;

    #[test]
    fn version_ordering() {
        assert!(version_older_or_equal("v0.7.2", "0.8.6"));
        assert!(version_older_or_equal("0.8.6", "0.8.6"));
        assert!(version_older_or_equal("v0.8.6", "v0.8.6-beta"));
        assert!(!version_older_or_equal("0.8.6", "v0.7.2"));
        assert!(!version_older_or_equal("v0.9.0", "0.8.6"));
        assert!(version_older_or_equal("1.2", "1.2.0"));
    }
}

#[cfg(test)]
mod sigpipe_disposition_tests {
    use super::{restores_sigpipe_default, Cli};
    use clap::Parser;

    fn cmd(argv: &[&str]) -> super::Command {
        Cli::try_parse_from(argv).unwrap().cmd
    }

    #[test]
    fn daemon_arms_never_restore_sigpipe_default() {
        assert!(!restores_sigpipe_default(&cmd(&[
            "keep-at",
            "run",
            "--storage-location",
            "/tmp/x",
        ])));
        assert!(!restores_sigpipe_default(&cmd(&["keep-at", "start"])));
        assert!(!restores_sigpipe_default(&cmd(&[
            "keep-at", "service", "install"
        ])));
    }

    #[test]
    fn print_arms_restore_sigpipe_default() {
        assert!(restores_sigpipe_default(&cmd(&["keep-at", "version"])));
        assert!(restores_sigpipe_default(&cmd(&["keep-at", "status"])));
        assert!(restores_sigpipe_default(&cmd(&["keep-at", "logs"])));
        assert!(restores_sigpipe_default(&cmd(&[
            "keep-at",
            "hosted-torrents"
        ])));
        assert!(restores_sigpipe_default(&cmd(&["keep-at", "stop"])));
    }
}
