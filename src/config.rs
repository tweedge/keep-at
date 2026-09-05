//! Configuration: every setting keep-at runs with, loadable from an
//! optional YAML file and overridable field-by-field from CLI flags.
//! Ported from internal/config (Go).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 37550;
pub const DEFAULT_AGGRESSIVENESS: f64 = 0.6;
pub const DEFAULT_MIN_SEED_MARGIN: i32 = 2;
pub const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_secs(7 * 24 * 3600);
pub const DEFAULT_MODERATION_DELAY: Duration = Duration::from_secs(7 * 24 * 3600);
pub const DEFAULT_RATE_LIMIT_PER_SEC: f64 = 0.5;
pub const DEFAULT_STATS_INTERVAL: Duration = Duration::from_secs(30 * 60);
pub const DEFAULT_STALL_EVICTION_TIMEOUT: Duration = Duration::from_secs(14 * 24 * 3600);

/// Fraction of a device's total formatted capacity `limit: all` resolves to.
/// Dedicated data drives only - see the Go config.AllLimitFraction docs.
pub const ALL_LIMIT_FRACTION: f64 = 0.975;

/// Share of system RAM keep-at will ever plan around, regardless of --max-ram.
pub const SYSTEM_RAM_FRACTION_HARD_CAP: f64 = 0.8;

/// Legacy flat per-torrent estimate. Superseded by the measured model in
/// engine::ram (BASE + per-piece + per-peer at the budget's peer limit);
/// kept so the hard-cap log line and external callers still compile.
pub const PER_TORRENT_RAM_BASE: u64 = 1 << 20; // 1 MiB

fn default_scan_interval() -> Duration {
    DEFAULT_SCAN_INTERVAL
}
fn default_moderation_delay() -> Duration {
    DEFAULT_MODERATION_DELAY
}
fn default_stats_interval() -> Duration {
    DEFAULT_STATS_INTERVAL
}
fn default_stall_timeout() -> Duration {
    DEFAULT_STALL_EVICTION_TIMEOUT
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageLocation {
    pub path: PathBuf,
    #[serde(default, deserialize_with = "de_limit", serialize_with = "ser_limit")]
    pub limit: StorageLimit,
}

fn de_limit<'de, D>(d: D) -> std::result::Result<StorageLimit, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    StorageLimit::parse(&s).map_err(serde::de::Error::custom)
}

fn ser_limit<S>(l: &StorageLimit, s: S) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match l {
        StorageLimit::All => s.serialize_str("all"),
        StorageLimit::Bytes(b) => s.serialize_str(&format_byte_size(*b)),
        StorageLimit::Unset => s.serialize_str(""),
    }
}

/// A storage location's space limit: a byte count, or "all" (resolved at
/// startup to a safe fraction of the device's formatted capacity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorageLimit {
    /// No limit parsed yet (zero value of a fresh location - invalid).
    #[default]
    Unset,
    Bytes(u64),
    All,
}

impl StorageLimit {
    pub fn parse(s: &str) -> Result<Self> {
        let t = s.trim();
        if t.eq_ignore_ascii_case("all") {
            return Ok(StorageLimit::All);
        }
        Ok(StorageLimit::Bytes(parse_byte_size(t)?))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScanConfig {
    #[serde(
        default = "default_scan_interval",
        deserialize_with = "de_duration_secs_opt",
        serialize_with = "ser_duration_secs_opt",
        skip_serializing_if = "is_default_scan_interval"
    )]
    pub interval: Duration,
    #[serde(default = "default_rate")]
    pub rate_limit_per_second: f64,
    #[serde(default = "default_margin")]
    pub min_seed_margin: i32,
    #[serde(
        default = "default_moderation_delay",
        deserialize_with = "de_duration_secs_opt",
        serialize_with = "ser_duration_secs_opt",
        skip_serializing_if = "is_default_moderation_delay"
    )]
    pub moderation_delay: Duration,
    #[serde(
        default = "default_stall_timeout",
        deserialize_with = "de_duration_secs_opt",
        serialize_with = "ser_duration_secs_opt",
        skip_serializing_if = "is_default_stall_timeout"
    )]
    pub stall_eviction_timeout: Duration,
}

fn default_rate() -> f64 {
    DEFAULT_RATE_LIMIT_PER_SEC
}
fn default_margin() -> i32 {
    DEFAULT_MIN_SEED_MARGIN
}
fn is_default_scan_interval(d: &Duration) -> bool {
    *d == DEFAULT_SCAN_INTERVAL
}
fn is_default_moderation_delay(d: &Duration) -> bool {
    *d == DEFAULT_MODERATION_DELAY
}
fn is_default_stall_timeout(d: &Duration) -> bool {
    *d == DEFAULT_STALL_EVICTION_TIMEOUT
}

fn de_duration_secs_opt<'de, D>(d: D) -> std::result::Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Deserialize as _;
    let opt = Option::<u64>::deserialize(d)?;
    Ok(opt
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_SCAN_INTERVAL))
}

fn ser_duration_secs_opt<S>(d: &Duration, s: S) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    s.serialize_u64(d.as_secs())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default)]
    pub storage: Vec<StorageLocation>,
    #[serde(default)]
    pub scan: ScanConfig,
    #[serde(default = "default_aggr")]
    pub aggressiveness: f64,
    #[serde(default)]
    pub keyword_blocklist: Vec<String>,
    #[serde(default)]
    pub preserve_deleted_torrents: bool,
    /// 0 = use the 80%-of-system hard cap.
    #[serde(
        default,
        deserialize_with = "de_bytesize_opt",
        serialize_with = "ser_bytesize_opt"
    )]
    pub max_ram: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    #[serde(
        default,
        deserialize_with = "de_bytesize_opt",
        serialize_with = "ser_bytesize_opt"
    )]
    pub upload_rate_limit: u64,
    #[serde(
        default,
        deserialize_with = "de_bytesize_opt",
        serialize_with = "ser_bytesize_opt"
    )]
    pub download_rate_limit: u64,
    #[serde(
        default = "default_stats_interval",
        deserialize_with = "de_duration_secs_opt_stats",
        serialize_with = "ser_duration_secs_opt",
        skip_serializing_if = "is_default_stats_interval"
    )]
    pub stats_interval: Duration,
    #[serde(default)]
    pub debug: bool,
    /// Optional log file path. When set, daemon output goes here instead of
    /// stdout (useful for `start`ed daemons whose stdio is detached).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_file: Option<PathBuf>,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}
fn default_aggr() -> f64 {
    DEFAULT_AGGRESSIVENESS
}
fn is_default_stats_interval(d: &Duration) -> bool {
    *d == DEFAULT_STATS_INTERVAL
}
fn de_duration_secs_opt_stats<'de, D>(d: D) -> std::result::Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Deserialize as _;
    let opt = Option::<u64>::deserialize(d)?;
    Ok(opt
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_STATS_INTERVAL))
}

fn de_bytesize_opt<'de, D>(d: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Deserialize as _;
    let opt = Option::<String>::deserialize(d)?;
    match opt {
        None => Ok(0),
        Some(s) => parse_byte_size(&s).map_err(serde::de::Error::custom),
    }
}

fn ser_bytesize_opt<S>(v: &u64, s: S) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::Serialize as _;
    if *v == 0 {
        None::<String>.serialize(s)
    } else {
        Some(format_byte_size(*v)).serialize(s)
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: DEFAULT_PORT,
            data_dir: default_data_dir(),
            storage: Vec::new(),
            scan: ScanConfig {
                interval: DEFAULT_SCAN_INTERVAL,
                rate_limit_per_second: DEFAULT_RATE_LIMIT_PER_SEC,
                min_seed_margin: DEFAULT_MIN_SEED_MARGIN,
                moderation_delay: DEFAULT_MODERATION_DELAY,
                stall_eviction_timeout: DEFAULT_STALL_EVICTION_TIMEOUT,
            },
            aggressiveness: DEFAULT_AGGRESSIVENESS,
            keyword_blocklist: Vec::new(),
            preserve_deleted_torrents: false,
            max_ram: 0,
            api_key: String::new(),
            upload_rate_limit: 0,
            download_rate_limit: 0,
            stats_interval: DEFAULT_STATS_INTERVAL,
            debug: false,
            log_file: None,
        }
    }
}

impl Config {
    /// Load a config file, writing a starter one (and erroring) if missing -
    /// same contract as Go's config.Load.
    pub fn load(path: &Path) -> Result<Config> {
        let data = match std::fs::read(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                write_starter_config(path).with_context(|| {
                    format!(
                        "no config at {}, and failed to write a starter one",
                        path.display()
                    )
                })?;
                bail!(
                    "wrote a starter config to {}: set at least one storage location and limit, then run keep-at again",
                    path.display()
                );
            }
            Err(e) => return Err(e).with_context(|| format!("reading config {}", path.display())),
            Ok(d) => d,
        };
        let mut cfg: Config = serde_yaml::from_slice(&data)
            .with_context(|| format!("parsing config {}", path.display()))?;
        // Fill defaults for fields a partial YAML left at zero values.
        if cfg.port == 0 {
            cfg.port = DEFAULT_PORT;
        }
        if cfg.data_dir.as_os_str().is_empty() {
            cfg.data_dir = default_data_dir();
        }
        if cfg.aggressiveness == 0.0 {
            cfg.aggressiveness = DEFAULT_AGGRESSIVENESS;
        }
        if cfg.scan.rate_limit_per_second == 0.0 {
            cfg.scan.rate_limit_per_second = DEFAULT_RATE_LIMIT_PER_SEC;
        }
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let header = "# keep-at config. Edit this file directly and restart keep-at\n# (`systemctl restart keep-at`, or `keep-at service install` again)\n# to apply changes. Every field here also has a --flag equivalent\n# (`keep-at run --help`).\n\n";
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating directory for {}", path.display()))?;
        }
        let mut out = header.to_string();
        out.push_str(&serde_yaml::to_string(self).context("marshalling config")?);
        atomic_write(path, out.as_bytes())
    }

    pub fn validate(&self) -> Result<()> {
        if self.storage.is_empty() {
            bail!(
                "no storage locations configured; keep-at will not pick a default limit, set --storage-limit (and optionally --storage) or storage: in a config file"
            );
        }
        let mut seen = std::collections::HashSet::new();
        for loc in &self.storage {
            if loc.path.as_os_str().is_empty() {
                bail!("storage location has an empty path");
            }
            match loc.limit {
                StorageLimit::Unset | StorageLimit::Bytes(0) => {
                    bail!(
                        "storage location {} has no positive limit set",
                        loc.path.display()
                    )
                }
                _ => {}
            }
            if !seen.insert(loc.path.clone()) {
                bail!(
                    "storage location {} is listed more than once",
                    loc.path.display()
                );
            }
        }
        if !(self.aggressiveness > 0.0 && self.aggressiveness < 1.0) {
            bail!(
                "aggressiveness must be between 0 and 1 (exclusive), got {}",
                self.aggressiveness
            );
        }
        if self.scan.min_seed_margin < 0 {
            bail!("scan.min_seed_margin must not be negative");
        }
        if self.port == 0 {
            bail!("port {} is out of range", self.port);
        }
        if self.scan.rate_limit_per_second <= 0.0 {
            bail!("scan.rate_limit_per_second must be positive");
        }
        Ok(())
    }
}

fn write_starter_config(path: &Path) -> Result<()> {
    let header = "# keep-at starter config. This is entirely optional - every field here has\n# a --flag equivalent (run `keep-at run --help`). Reach for a config file\n# once you want more than one storage location, or don't want to repeat\n# flags every time.\n#\n# At minimum, set a real limit (e.g. 500G, 2T) below.\n#\n# To get credit for the torrents you seed, set api_key to your Academic\n# Torrents API key (https://academictorrents.com/my.php) - it's only sent\n# to AT's own trackers and never logged.\n\n";
    let cfg = Config {
        storage: vec![StorageLocation {
            path: default_storage_location(),
            limit: StorageLimit::Unset,
        }],
        ..Config::default()
    };
    let mut out = header.to_string();
    // Starter must show an empty limit field; serialize manually.
    out.push_str(&format!(
        "port: {}\ndata_dir: {}\nstorage:\n- path: {}\n  limit: \"\"\n",
        cfg.port,
        cfg.data_dir.display(),
        cfg.storage[0].path.display()
    ));
    atomic_write(path, out.as_bytes())
}

pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating directory for {}", path.display()))?;
        }
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("finalizing {}", path.display()))?;
    Ok(())
}

/// Parse "500G", "2T", "50M", "1024", "1.5G" into bytes (binary units).
pub fn parse_byte_size(s: &str) -> Result<u64> {
    let t = s.trim();
    if t.is_empty() {
        bail!("empty byte size");
    }
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, suffix) = t.split_at(split);
    let n: f64 = num
        .parse()
        .with_context(|| format!("invalid byte size {s:?}"))?;
    if n < 0.0 {
        bail!("byte size must not be negative: {s:?}");
    }
    let mult: f64 = match suffix.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "K" | "KB" | "KI" | "KIB" => 1024.0,
        "M" | "MB" | "MI" | "MIB" => 1024.0 * 1024.0,
        "G" | "GB" | "GI" | "GIB" => 1024.0 * 1024.0 * 1024.0,
        "T" | "TB" | "TI" | "TIB" => 1024.0_f64.powi(4),
        "P" | "PB" | "PI" | "PIB" => 1024.0_f64.powi(5),
        other => bail!("unknown byte-size suffix {other:?} in {s:?}"),
    };
    Ok((n * mult) as u64)
}

pub fn format_byte_size(n: u64) -> String {
    crate::humanize::human_bytes(n as i64).replace(' ', "")
}

pub fn default_data_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(".local/share/keep-at");
        }
    }
    PathBuf::from("/var/lib/keep-at")
}

pub fn default_storage_location() -> PathBuf {
    default_data_dir().join("storage")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_sizes() {
        assert_eq!(parse_byte_size("500G").unwrap(), 500 * 1024 * 1024 * 1024);
        assert_eq!(parse_byte_size("2T").unwrap(), 2 * 1024_u64.pow(4));
        assert_eq!(parse_byte_size("50M").unwrap(), 50 * 1024 * 1024);
        assert_eq!(parse_byte_size("1024").unwrap(), 1024);
        assert_eq!(
            parse_byte_size("1.5G").unwrap(),
            (1.5 * 1024.0_f64.powi(3)) as u64
        );
        assert!(parse_byte_size("10X").is_err());
        assert!(parse_byte_size("").is_err());
        assert_eq!(StorageLimit::parse("all").unwrap(), StorageLimit::All);
        assert_eq!(StorageLimit::parse("ALL").unwrap(), StorageLimit::All);
    }

    #[test]
    fn validate_rejects_empty_storage() {
        let cfg = Config::default();
        assert!(cfg.validate().is_err());
    }
}
