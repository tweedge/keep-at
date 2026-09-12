//! CLI definition (clap) and config resolution from flags + file.
//! Every Config field has a flag; --config is optional; repeatable
//! --storage-location/--storage-limit pairs remove the need for a file.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::config::{self, Config, StorageLimit, StorageLocation};

#[derive(Debug, Parser)]
#[command(
    name = "keep-at",
    about = "a smart node that seeds Academic Torrents",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run in the foreground
    Run(RunArgs),
    /// Start as a background process
    Start(RunArgs),
    /// Stop the background process
    Stop(CommonArgs),
    /// Report whether keep-at is running
    Status(CommonArgs),
    /// Print keep-at's logs (follows by default; --all prints and exits)
    Logs(LogsArgs),
    /// Install/remove a systemd service (elevates automatically when needed)
    Service(ServiceArgs),
    /// Census the keep-at network (RAM/time-heavy, on demand)
    NetworkStatus(NetworkStatusArgs),
    /// List torrents this host holds and seeds
    HostedTorrents(CommonArgs),
    /// Update to the latest release
    SelfUpdate(SelfUpdateArgs),
    /// Triage a dead daemon: print surviving cgroup state + last heartbeat
    /// (called by the host watchdog on death, or manually post-mortem)
    TriageLastExit(CommonArgs),
    /// Print the version and exit
    Version,
}

#[derive(Debug, Args)]
pub struct CommonArgs {
    /// Path to a config file (optional)
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Directory for keep-at's own state (defaults to the config's or OS default)
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    /// Print the whole log and exit instead of following
    #[arg(long, default_value_t = false)]
    pub all: bool,
    /// How many lines to show before following (default: 50)
    #[arg(long, default_value_t = 20)]
    pub lines: usize,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    #[command(flatten)]
    pub cfg: ConfigArgs,
    /// Run in the foreground even for `start` (ignored for `run`)
    #[arg(long, default_value_t = false)]
    pub foreground: bool,
}

/// Every config setting as a flag. Storage is `--storage-location PATH`
/// plus `--storage-limit SIZE` pairs (repeatable); `--storage PATH` is the
/// single-location shorthand, and a bare `--storage-limit` fills the
/// default location.
///
/// Flags are grouped by purpose in --help (storage, network, selection,
/// limits, logging) rather than alphabetically, so the most-changed
/// settings come first.
#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// Storage location for torrent data (repeatable; pairs with --storage-limit)
    #[arg(long = "storage-location", help_heading = "Storage")]
    pub storage_location: Vec<PathBuf>,
    /// How much space to use, e.g. 500G, 2T, or 'max' (repeatable; pairs with --storage-location; a bare --storage-limit fills the default location)
    #[arg(long = "storage-limit", help_heading = "Storage")]
    pub storage_limit: Vec<String>,
    /// Single storage location shorthand (equivalent to one --storage-location)
    #[arg(long, help_heading = "Storage")]
    pub storage: Option<PathBuf>,
    /// BitTorrent listen port
    #[arg(long, help_heading = "Network")]
    pub port: Option<u16>,
    /// Max requests per second to Academic Torrents' own infrastructure
    #[arg(long, help_heading = "Network")]
    pub rate_limit: Option<f64>,
    /// Academic Torrents API key (uid=...;pass=...) - only sent to AT trackers
    #[arg(long, help_heading = "Network")]
    pub api_key: Option<String>,
    /// Anti-cascade base (0-1); lower backs off faster as more keep-at nodes join
    #[arg(long, help_heading = "Selection")]
    pub aggressiveness: Option<f64>,
    /// How many fewer seeds a candidate needs before displacing a held torrent
    #[arg(long, help_heading = "Selection")]
    pub min_seed_margin: Option<i32>,
    /// How often to rescan the Academic Torrents catalog (e.g. 168h)
    #[arg(long, value_parser = parse_duration, help_heading = "Selection")]
    pub scan_interval: Option<std::time::Duration>,
    /// Minimum torrent age before keep-at will download it
    #[arg(long, value_parser = parse_duration, help_heading = "Selection")]
    pub moderation_delay: Option<std::time::Duration>,
    /// How long a zero-seeder torrent with no progress can sit before removal (0 disables)
    #[arg(long, value_parser = parse_duration, help_heading = "Selection")]
    pub stall_eviction_timeout: Option<std::time::Duration>,
    /// Comma-separated keywords to block, matched against title and description
    #[arg(long, help_heading = "Selection")]
    pub keyword_blocklist: Option<String>,
    /// Keep seeding a torrent even if Academic Torrents removes it
    #[arg(long, help_heading = "Selection")]
    pub preserve_deleted_torrents: Option<bool>,
    /// Max RAM to plan around, e.g. 1G (default: 80% of system RAM)
    #[arg(long, help_heading = "Limits")]
    pub max_ram: Option<String>,
    /// Max upload speed across all torrents, e.g. 50M (default: unlimited)
    #[arg(long, help_heading = "Limits")]
    pub upload_rate_limit: Option<String>,
    /// Max download speed across all torrents, e.g. 20M (default: unlimited)
    #[arg(long, help_heading = "Limits")]
    pub download_rate_limit: Option<String>,
    /// Directory for keep-at's own state, logs, and cached metadata
    /// (duplicate of --data-dir on CommonArgs for convenience)
    #[arg(long = "cfg-data-dir", help_heading = "Logging")]
    pub cfg_data_dir: Option<PathBuf>,
    /// How often to log a summary; 0 disables periodic summaries
    #[arg(long, value_parser = parse_duration, help_heading = "Logging")]
    pub stats_interval: Option<std::time::Duration>,
    /// Verbose diagnostics
    #[arg(long, help_heading = "Logging")]
    pub debug: Option<bool>,
    /// Write logs to PATH instead of stdout (for background daemons)
    #[arg(long, help_heading = "Logging")]
    pub log_file: Option<PathBuf>,
}

fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    humantime::parse_duration(s).map_err(|e| e.to_string())
}

#[derive(Debug, Args)]
pub struct ServiceArgs {
    #[command(subcommand)]
    pub op: ServiceOp,
}

#[derive(Debug, Subcommand)]
pub enum ServiceOp {
    /// Install a systemd service (elevates automatically when needed)
    Install(Box<ServiceInstallArgs>),
    /// Remove the systemd service (elevates automatically when needed)
    Uninstall,
}

#[derive(Debug, Args)]
pub struct ServiceInstallArgs {
    /// User the systemd service runs as
    #[arg(long, default_value = "root")]
    pub user: String,
    #[command(flatten)]
    pub common: CommonArgs,
    #[command(flatten)]
    pub cfg: ConfigArgs,
}

#[derive(Debug, Args)]
pub struct NetworkStatusArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    /// How long to wait per torrent for peers while probing its swarm
    #[arg(long, value_parser = parse_duration)]
    pub probe_timeout: Option<std::time::Duration>,
    /// Academic Torrents API key to attribute census announces to your account
    #[arg(long)]
    pub api_key: Option<String>,
    /// Max requests per second to Academic Torrents' own infrastructure
    #[arg(long)]
    pub rate_limit: Option<f64>,
}

#[derive(Debug, Args)]
pub struct SelfUpdateArgs {
    /// Track development builds (x.y.z-beta) instead of stable releases (x.y)
    #[arg(long, default_value_t = false)]
    pub beta: bool,
}

/// Service config path when installed (pub for read-only commands' config
/// fallbacks; internal callers use it via this module).
pub fn service_config_if_present() -> Option<PathBuf> {
    let p = PathBuf::from(crate::service::CONFIG_PATH);
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

impl ConfigArgs {
    fn storage_flags_set(&self) -> bool {
        !self.storage_location.is_empty()
            || !self.storage_limit.is_empty()
            || self.storage.is_some()
    }

    /// Was any non-storage config flag explicitly passed?
    fn any_set(&self) -> bool {
        self.port.is_some()
            || self.cfg_data_dir.is_some()
            || self.aggressiveness.is_some()
            || self.min_seed_margin.is_some()
            || self.scan_interval.is_some()
            || self.moderation_delay.is_some()
            || self.rate_limit.is_some()
            || self.stall_eviction_timeout.is_some()
            || self.keyword_blocklist.is_some()
            || self.preserve_deleted_torrents.is_some()
            || self.max_ram.is_some()
            || self.api_key.is_some()
            || self.upload_rate_limit.is_some()
            || self.download_rate_limit.is_some()
            || self.stats_interval.is_some()
            || self.debug.is_some()
            || self.log_file.is_some()
    }

    fn apply_to(&self, cfg: &mut Config) -> Result<()> {
        if let Some(v) = self.port {
            cfg.port = v;
        }
        if let Some(v) = &self.cfg_data_dir {
            cfg.data_dir = v.clone();
        }
        if let Some(v) = self.aggressiveness {
            cfg.aggressiveness = v;
        }
        if let Some(v) = self.min_seed_margin {
            cfg.scan.min_seed_margin = v;
        }
        if let Some(v) = self.scan_interval {
            cfg.scan.interval = v;
        }
        if let Some(v) = self.moderation_delay {
            cfg.scan.moderation_delay = v;
        }
        if let Some(v) = self.rate_limit {
            cfg.scan.rate_limit_per_second = v;
        }
        if let Some(v) = self.stall_eviction_timeout {
            cfg.scan.stall_eviction_timeout = v;
        }
        if let Some(v) = &self.keyword_blocklist {
            cfg.keyword_blocklist = split_keywords(v);
        }
        if let Some(v) = self.preserve_deleted_torrents {
            cfg.preserve_deleted_torrents = v;
        }
        if let Some(v) = &self.max_ram {
            let bytes = config::parse_byte_size(v).with_context(|| "--max-ram")?;
            config::check_bandwidth_limit("--max-ram", bytes).with_context(|| "--max-ram")?;
            cfg.max_ram = bytes;
        }
        if let Some(v) = &self.api_key {
            cfg.api_key = v.clone();
        }
        if let Some(v) = &self.upload_rate_limit {
            let bytes = config::parse_byte_size(v).with_context(|| "--upload-rate-limit")?;
            config::check_bandwidth_limit("--upload-rate-limit", bytes)
                .with_context(|| "--upload-rate-limit")?;
            cfg.upload_rate_limit = bytes;
        }
        if let Some(v) = &self.download_rate_limit {
            let bytes = config::parse_byte_size(v).with_context(|| "--download-rate-limit")?;
            config::check_bandwidth_limit("--download-rate-limit", bytes)
                .with_context(|| "--download-rate-limit")?;
            cfg.download_rate_limit = bytes;
        }
        if let Some(v) = self.stats_interval {
            cfg.stats_interval = v;
        }
        if let Some(v) = self.debug {
            cfg.debug = v;
        }
        if let Some(v) = &self.log_file {
            cfg.log_file = Some(v.clone());
        }
        Ok(())
    }

    /// Build storage locations from flags. Combines --storage-location (with
    /// --storage-limit) and the --storage shorthand. A bare --storage-limit
    /// with no location fills the default storage location (documented in
    /// --storage-limit help); nothing here guesses *how much*, only *where*.
    fn storage_from_flags(&self) -> Result<Vec<StorageLocation>> {
        let mut locs: Vec<StorageLocation> = Vec::new();
        let mut limits = self.storage_limit.iter();
        for path in &self.storage_location {
            let raw = limits.next().with_context(|| {
                format!(
                    "--storage-location {} has no matching --storage-limit",
                    path.display()
                )
            })?;
            locs.push(StorageLocation {
                path: path.clone(),
                limit: StorageLimit::parse(raw)?,
            });
        }
        let leftover: Vec<String> = limits.cloned().collect();
        if let Some(path) = &self.storage {
            // --storage shorthand pairs with a single --storage-limit.
            let raw = match leftover.as_slice() {
                [single] => single.clone(),
                [] => bail!("--storage-limit is required (e.g. --storage-limit 500G, or --storage-limit max)"),
                _ => bail!("--storage can't be combined with multiple --storage-location/--storage-limit pairs; use --storage-location instead"),
            };
            locs.push(StorageLocation {
                path: path.clone(),
                limit: StorageLimit::parse(&raw)?,
            });
        } else if let [single] = leftover.as_slice() {
            // Bare --storage-limit: use the default storage location.
            locs.push(StorageLocation {
                path: crate::config::default_storage_location(),
                limit: StorageLimit::parse(single)?,
            });
        } else if !leftover.is_empty() {
            bail!("--storage-limit given without a matching --storage-location (pair each location with its own limit, or use --storage PATH --storage-limit SIZE)");
        }
        Ok(locs)
    }
}

pub fn split_keywords(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Resolve a full Config from --config and/or flags (run/start/service install).
pub fn resolve(common: &CommonArgs, args: &ConfigArgs) -> Result<Config> {
    let storage_set = args.storage_flags_set();

    let mut path = common.config.clone();
    let mut using_service_config = false;
    if path.is_none() && !storage_set && !args.any_set() {
        if let Some(p) = service_config_if_present() {
            path = Some(p);
            using_service_config = true;
        }
    }

    let mut cfg = Config::default();
    let mut file_loaded = false;
    if let Some(p) = &path {
        cfg = Config::load(p).map_err(|e| {
            if using_service_config {
                anyhow::anyhow!(
                    "found an installed service config at {} but couldn't load it: {e:#}",
                    p.display()
                )
            } else {
                e
            }
        })?;
        file_loaded = true;
    }

    args.apply_to(&mut cfg)?;
    // --data-dir on the command root wins over the config file.
    if let Some(d) = &common.data_dir {
        cfg.data_dir = d.clone();
    }

    if storage_set {
        if file_loaded {
            bail!("--storage-location/--storage-limit/--storage can't be combined with --config; edit {} instead", path.unwrap().display());
        }
        let locs = args.storage_from_flags()?;
        if locs.is_empty() {
            bail!("--storage-limit is required (e.g. --storage-limit 500G, or --storage-limit max to use a dedicated drive)");
        }
        cfg.storage = locs;
    } else if !file_loaded && cfg.storage.is_empty() {
        bail!("no storage configured; pass --storage-limit (e.g. --storage-limit 500G), --config, or install keep-at as a service first");
    }

    cfg.validate()?;
    Ok(cfg)
}

/// Config for commands that only need the data dir (status/stop/hosted).
///
/// Resolution order: --data-dir flag, --config file, then the service's
/// world-readable data-dir pointer (`/etc/keep-at/data_dir`), then the
/// service config file itself (legacy installs, pre-pointer), then the
/// default. The pointer exists precisely so a root-only legacy config never
/// blocks a read-only command run as a normal user.
pub fn resolve_data_dir(args: &CommonArgs) -> Result<PathBuf> {
    if let Some(d) = &args.data_dir {
        return Ok(d.clone());
    }
    if let Some(p) = &args.config {
        return Ok(Config::load(p)?.data_dir);
    }
    if let Some(d) = crate::service::read_data_dir_pointer() {
        return Ok(d);
    }
    if let Some(p) = service_config_if_present() {
        match Config::load(&p) {
            Ok(cfg) => return Ok(cfg.data_dir),
            Err(e) => anyhow::bail!(
                "{e:#}\nhint: this is an older install; run `sudo keep-at service install` once (or restart the daemon) to write {} and open the config up",
                crate::service::DATA_DIR_POINTER
            ),
        }
    }
    Ok(Config::default().data_dir)
}

/// Config for network-status: data dir + API key + rate limit, no storage.
pub fn resolve_census(args: &NetworkStatusArgs) -> Result<Config> {
    let mut path = args.common.config.clone();
    if path.is_none() {
        path = service_config_if_present();
    }
    let mut cfg = Config::default();
    if let Some(p) = path {
        cfg = Config::load(&p)?;
    }
    if let Some(d) = &args.common.data_dir {
        cfg.data_dir = d.clone();
    }
    if let Some(k) = &args.api_key {
        cfg.api_key = k.clone();
    }
    if let Some(r) = args.rate_limit {
        cfg.scan.rate_limit_per_second = r;
    }
    Ok(cfg)
}
