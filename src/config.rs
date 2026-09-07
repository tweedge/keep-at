//! Configuration: every setting keep-at runs with, loadable from an
//! optional YAML file and overridable field-by-field from CLI flags.

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

/// Fraction of a device's total formatted capacity `limit: max` resolves to.
/// Dedicated data drives only.
pub const ALL_LIMIT_FRACTION: f64 = 0.975;

/// Minimum storage limit: below this is certainly a units mistake (e.g.
/// meaning megabytes but typing a bare number, or a misplaced decimal), not
/// a real allocation. keep-at refuses to run with less.
pub const MIN_STORAGE_LIMIT: u64 = 100 * 1024 * 1024; // 100 MiB

/// Below this, a storage limit is probably a units mistake worth warning
/// about (but not refusing): a GiB-scale node holds almost nothing.
pub const STORAGE_LIMIT_WARN: u64 = 1024 * 1024 * 1024; // 1 GiB

/// Minimum bandwidth limit: below this is certainly a units mistake, not a
/// real cap (100 KiB/s cannot usefully seed). keep-at refuses to run slower.
pub const MIN_BANDWIDTH_LIMIT: u64 = 100 * 1024; // 100 KiB/s

/// Below this, a bandwidth limit is probably a units mistake worth warning
/// about (but not refusing): sub-MiB/s caps make downloads take days.
pub const BANDWIDTH_LIMIT_WARN: u64 = 1024 * 1024; // 1 MiB/s

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
        StorageLimit::All => s.serialize_str("max"),
        StorageLimit::Bytes(b) => s.serialize_str(&format_byte_size(*b)),
        StorageLimit::Unset => s.serialize_str(""),
    }
}

/// A storage location's space limit: a byte count, or "max" (resolved at
/// startup to a safe fraction of the device's formatted capacity).
/// "all" is accepted as a deprecated alias for "max".
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
        if t.eq_ignore_ascii_case("max") || t.eq_ignore_ascii_case("all") {
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
    /// Load a config file, writing a starter one (and erroring) if missing.
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
        // Configs may carry the API key (passkey-equivalent): owner-only.
        atomic_write_mode(path, out.as_bytes(), 0o600)
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
            check_storage_limit(&loc.path, loc.limit)?;
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
        check_bandwidth_limit("max_ram", self.max_ram)?;
        check_bandwidth_limit("upload_rate_limit", self.upload_rate_limit)?;
        check_bandwidth_limit("download_rate_limit", self.download_rate_limit)?;
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
    atomic_write_mode(path, data, 0o644)
}

/// Atomic write with an explicit file mode, ignoring umask. Read-only
/// operations (`status`, `hosted-torrents`, `network-status` against cached
/// files) must work for any local user, so snapshots, caches, and state
/// files are world-readable (0o644) while the daemon keeps sole write
/// access. Secrets (API keys in configs) use 0o600 via this same helper —
/// see `save`.
pub fn atomic_write_mode(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            // Directories need +x for traversal: 0o755 regardless of umask,
            // applied to every level create_dir_all makes (existing levels
            // keep their modes — see ensure_shared_dirs below for repair).
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o755)
                .create(dir)
                .with_context(|| format!("creating directory for {}", path.display()))?;
        }
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    // OpenOptions with explicit mode: umask cannot strip what we set here
    // (mode applies at creation; umask only masks it — and 0o644/0o600
    // survive every sane umask: 022, 027, even 077 for the public files?
    // NO — umask 077 WOULD strip group/other. So set permissions explicitly
    // after writing, which ignores umask entirely.)
    std::fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting mode on {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("finalizing {}", path.display()))?;
    // rename preserves the temp file's mode; belt-and-suspenders in case a
    // platform ever copies instead of renaming.
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    Ok(())
}

/// Walk up from `dir`, ensuring every existing level is at least
/// owner-rwx + group/other rx (0o755 masked in, never stripped), so a data
/// dir created earlier under a restrictive umask (or by root's 077 default)
/// still lets other users traverse to the world-readable snapshots inside.
/// Stops at filesystem boundaries it can't change (errors ignored past the
/// first failure — best effort, never fatal).
pub fn ensure_shared_dirs(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut cur = Some(dir);
    while let Some(d) = cur {
        match std::fs::metadata(d) {
            Ok(m) => {
                let mode = m.permissions().mode() & 0o777;
                let fixed = mode | 0o755;
                if fixed != mode {
                    let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(fixed));
                }
            }
            Err(_) => break,
        }
        cur = d.parent().filter(|p| !p.as_os_str().is_empty());
    }
}

/// Parse "500G", "2T", "50M", "1024", "1.5G" into bytes (binary units).
/// Rejects empty input, negatives, and unknown suffixes. The error for an
/// unknown suffix names the valid suffixes AND the `max` keyword, because
/// the most common failure is typing a limit like "500" (bare bytes, almost
/// certainly meant as gigabytes) or a typo of "max" — either way the
/// operator needs to see the keyword exists.
pub fn parse_byte_size(s: &str) -> Result<u64> {
    let t = s.trim();
    if t.is_empty() {
        bail!("empty byte size (e.g. 500G, 2T, or max for a dedicated drive)");
    }
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, suffix) = t.split_at(split);
    let n: f64 = num.parse().with_context(|| {
        format!("invalid byte size {s:?} (e.g. 500G, 2T, or max for a dedicated drive)")
    })?;
    if n < 0.0 {
        bail!("byte size must not be negative: {s:?}");
    }
    if num.is_empty() {
        bail!("byte size {s:?} has no number (e.g. 500G, 2T, or max for a dedicated drive)");
    }
    let mult: f64 = match suffix.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "K" | "KB" | "KI" | "KIB" => 1024.0,
        "M" | "MB" | "MI" | "MIB" => 1024.0 * 1024.0,
        "G" | "GB" | "GI" | "GIB" => 1024.0 * 1024.0 * 1024.0,
        "T" | "TB" | "TI" | "TIB" => 1024.0_f64.powi(4),
        "P" | "PB" | "PI" | "PIB" => 1024.0_f64.powi(5),
        other => bail!("unknown byte-size suffix {other:?} in {s:?} (use K/M/G/T/P, a plain byte count, or max for a dedicated drive)"),
    };
    Ok((n * mult) as u64)
}

/// Check a parsed storage limit against the sanity floors. Rejects
/// absurdly small limits (<100M, certainly a units mistake); warns on
/// merely small ones (<1G). `max` (All) always passes.
pub fn check_storage_limit(path: &std::path::Path, limit: StorageLimit) -> Result<()> {
    let bytes = match limit {
        StorageLimit::All | StorageLimit::Unset => return Ok(()),
        StorageLimit::Bytes(b) => b,
    };
    if bytes < MIN_STORAGE_LIMIT {
        anyhow::bail!(
            "storage location {} limit {} is under 100M — certainly a units mistake (did you mean gigabytes?). Use max for a dedicated drive, or a limit of at least 100M",
            path.display(),
            format_byte_size(bytes),
        );
    }
    if bytes < STORAGE_LIMIT_WARN {
        tracing::warn!(
            "storage location {} limit {} is under 1G — it will hold almost nothing; use max for a dedicated drive if that's what you meant",
            path.display(),
            format_byte_size(bytes),
        );
    }
    Ok(())
}

/// Check a parsed bandwidth limit (upload/download/max-ram take the same
/// byte-size syntax). `0` means unlimited and always passes. Rejects <100K,
/// warns <1M.
pub fn check_bandwidth_limit(flag: &str, bytes: u64) -> Result<()> {
    if bytes == 0 {
        return Ok(());
    }
    if bytes < MIN_BANDWIDTH_LIMIT {
        anyhow::bail!(
            "{flag} {} is under 100K/s — certainly a units mistake (did you mean megabytes?). Use 0 for unlimited, or at least 100K",
            format_byte_size(bytes),
        );
    }
    if bytes < BANDWIDTH_LIMIT_WARN {
        tracing::warn!(
            "{flag} {} is under 1M/s — downloads will take days at this cap",
            format_byte_size(bytes),
        );
    }
    Ok(())
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
        assert!(parse_byte_size("G").is_err());
        assert_eq!(StorageLimit::parse("all").unwrap(), StorageLimit::All);
        assert_eq!(StorageLimit::parse("ALL").unwrap(), StorageLimit::All);
        // "max" is the documented keyword; "all" stays as a deprecated alias.
        assert_eq!(StorageLimit::parse("max").unwrap(), StorageLimit::All);
        assert_eq!(StorageLimit::parse("MAX").unwrap(), StorageLimit::All);
    }

    #[test]
    fn storage_limit_floors() {
        use std::path::Path;
        let p = Path::new("/mnt/d1");
        // Under 100M rejected.
        assert!(check_storage_limit(p, StorageLimit::Bytes(50 * 1024 * 1024)).is_err());
        assert!(check_storage_limit(p, StorageLimit::Bytes(99)).is_err());
        // 100M..1G passes (warns, not asserted here).
        assert!(check_storage_limit(p, StorageLimit::Bytes(500 * 1024 * 1024)).is_ok());
        assert!(check_storage_limit(p, StorageLimit::Bytes(2 * 1024 * 1024 * 1024)).is_ok());
        // max always passes.
        assert!(check_storage_limit(p, StorageLimit::All).is_ok());
    }

    #[test]
    fn bandwidth_limit_floors() {
        // 0 = unlimited always passes.
        assert!(check_bandwidth_limit("--upload-rate-limit", 0).is_ok());
        // Under 100K rejected.
        assert!(check_bandwidth_limit("--upload-rate-limit", 50 * 1024).is_err());
        assert!(check_bandwidth_limit("--download-rate-limit", 99).is_err());
        // 100K..1M and above pass (warns, not asserted here).
        assert!(check_bandwidth_limit("--upload-rate-limit", 500 * 1024).is_ok());
        assert!(check_bandwidth_limit("--download-rate-limit", 50 * 1024 * 1024).is_ok());
    }

    #[test]
    fn validate_rejects_empty_storage() {
        let cfg = Config::default();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn shared_files_world_readable_despite_umask() {
        use std::os::unix::fs::PermissionsExt;
        // Simulate a root-like restrictive umask: even under 077, snapshot
        // writes must land world-readable (status/hosted-torrents run as
        // any user against a possibly-root-owned data dir).
        let dir = std::env::temp_dir().join(format!("keep-at-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: umask is process-global; tests run threads sharing it.
        // Save, set 077, restore immediately after the writes under test.
        let old = libc_umask(0o077);
        let p = dir.join("sub").join("runtime-stats.json");
        atomic_write(&p, b"{}").unwrap();
        ensure_shared_dirs(&dir.join("sub"));
        libc_umask(old);
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "snapshot world-readable under umask 077");
        let dmode = std::fs::metadata(dir.join("sub"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dmode & 0o755, 0o755, "dirs traversable by everyone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn configs_stay_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("keep-at-cfgmode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config {
            storage: vec![StorageLocation {
                path: dir.join("storage"),
                limit: StorageLimit::Bytes(200 * 1024 * 1024),
            }],
            api_key: "uid=1;pass=secret".to_string(),
            ..Config::default()
        };
        let path = dir.join("keep-at.yaml");
        cfg.save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config with possible API key is owner-only");
        let _ = std::fs::remove_dir_all(&dir);
    }

    unsafe extern "C" {
        fn umask(mask: u32) -> u32;
    }

    // `unsafe_op_in_unsafe_fn` style: the wrapper is safe (umask only sets
    // the process mask and returns the old one), so callers need no block.
    fn libc_umask(mask: u32) -> u32 {
        unsafe { umask(mask) }
    }
}
