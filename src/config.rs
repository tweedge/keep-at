//! Configuration: every setting keep-at runs with, loadable from an
//! optional YAML file and overridable field-by-field from CLI flags.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 37550;
pub const DEFAULT_AGGRESSIVENESS: f64 = 0.6;
pub const DEFAULT_MIN_SEED_MARGIN: i32 = 4;
/// Catalog collapse guard threshold (percent of held; see Config).
pub const DEFAULT_CATALOG_COLLAPSE_PERCENT: u32 = 30;
pub const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_secs(14 * 24 * 3600);
pub const DEFAULT_MODERATION_DELAY: Duration = Duration::from_secs(7 * 24 * 3600);
pub const DEFAULT_RATE_LIMIT_PER_SEC: f64 = 0.5;
pub const DEFAULT_STATS_INTERVAL: Duration = Duration::from_secs(30 * 60);
pub const DEFAULT_STALL_EVICTION_TIMEOUT: Duration = Duration::from_secs(90 * 24 * 3600);
/// Grace period for held torrents that vanished from the AT catalog: how
/// long a torrent must stay unlisted before the deleted-torrent pass
/// removes it. Real removals are permanent, so they evict after the window;
/// catalog hiccups (partial fetches, schema changes, AT-side outages) list
/// the torrent again well within it and the entry recovers untouched.
pub const DEFAULT_VANISHED_EVICTION_TIMEOUT: Duration = Duration::from_secs(90 * 24 * 3600);
/// Broken-piece watchdog cadence (0 = disabled).
pub const DEFAULT_QUARANTINE_CHECK_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Discarded-download volume (received-but-never-validated bytes) that
/// marks a torrent's swarm as broken.
pub const DEFAULT_BROKEN_PIECE_DISCARD_BYTES: u64 = 256 * 1024 * 1024;
/// Consecutive watchdog passes with zero verified progress required on top
/// of the discard volume (rides out transient stalls).
pub const DEFAULT_BROKEN_PIECE_MIN_WINDOWS: u32 = 2;
/// How long a quarantined hash stays un-selectable before the next re-probe.
pub const DEFAULT_QUARANTINE_COOLDOWN: Duration = Duration::from_secs(3 * 24 * 3600);
/// Re-probes before a quarantine becomes permanent (0 = unlimited).
pub const DEFAULT_QUARANTINE_MAX_RETRIES: u32 = 0;

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

/// Filename (inside the data dir) holding the Academic Torrents API key,
/// owner-only (0o600). The config file itself carries no secrets so it can
/// stay world-readable for `status`/`hosted-torrents` as any user.
pub const API_KEY_FILE: &str = "api_key";

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
fn default_vanished_timeout() -> Duration {
    DEFAULT_VANISHED_EVICTION_TIMEOUT
}
fn is_default_vanished_timeout(d: &Duration) -> bool {
    *d == DEFAULT_VANISHED_EVICTION_TIMEOUT
}

fn default_quarantine_check_interval() -> Duration {
    DEFAULT_QUARANTINE_CHECK_INTERVAL
}
fn is_default_quarantine_check_interval(d: &Duration) -> bool {
    *d == DEFAULT_QUARANTINE_CHECK_INTERVAL
}
fn default_broken_piece_discard_bytes() -> u64 {
    DEFAULT_BROKEN_PIECE_DISCARD_BYTES
}
fn is_default_broken_piece_discard_bytes(v: &u64) -> bool {
    *v == DEFAULT_BROKEN_PIECE_DISCARD_BYTES
}
fn default_broken_piece_min_windows() -> u32 {
    DEFAULT_BROKEN_PIECE_MIN_WINDOWS
}
fn is_default_broken_piece_min_windows(v: &u32) -> bool {
    *v == DEFAULT_BROKEN_PIECE_MIN_WINDOWS
}
fn default_quarantine_cooldown() -> Duration {
    DEFAULT_QUARANTINE_COOLDOWN
}
fn is_default_quarantine_cooldown(d: &Duration) -> bool {
    *d == DEFAULT_QUARANTINE_COOLDOWN
}
fn default_quarantine_max_retries() -> u32 {
    DEFAULT_QUARANTINE_MAX_RETRIES
}
fn is_default_quarantine_max_retries(v: &u32) -> bool {
    *v == DEFAULT_QUARANTINE_MAX_RETRIES
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanConfig {
    #[serde(
        default = "default_scan_interval",
        deserialize_with = "de_scan_interval",
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
        deserialize_with = "de_moderation_delay",
        serialize_with = "ser_duration_secs_opt",
        skip_serializing_if = "is_default_moderation_delay"
    )]
    pub moderation_delay: Duration,
    #[serde(
        default = "default_stall_timeout",
        deserialize_with = "de_stall_timeout",
        serialize_with = "ser_duration_secs_opt",
        skip_serializing_if = "is_default_stall_timeout"
    )]
    pub stall_eviction_timeout: Duration,
    #[serde(
        default = "default_vanished_timeout",
        deserialize_with = "de_vanished_timeout",
        serialize_with = "ser_duration_secs_opt",
        skip_serializing_if = "is_default_vanished_timeout"
    )]
    pub vanished_eviction_timeout: Duration,
    /// How often the broken-piece watchdog inspects per-torrent receive
    /// counters (0 = disabled). Torrents whose wire bytes never pass hash
    /// validation (poisoned/mis-seeded swarms) get removed and quarantined
    /// for `quarantine_cooldown` instead of looping forever.
    #[serde(
        default = "default_quarantine_check_interval",
        deserialize_with = "de_quarantine_check",
        serialize_with = "ser_duration_secs_opt",
        skip_serializing_if = "is_default_quarantine_check_interval"
    )]
    pub quarantine_check_interval: Duration,
    /// Discarded bytes (received minus validated) accumulated with zero
    /// verified progress before a torrent's swarm is declared broken.
    #[serde(
        default = "default_broken_piece_discard_bytes",
        skip_serializing_if = "is_default_broken_piece_discard_bytes"
    )]
    pub broken_piece_discard_bytes: u64,
    /// Consecutive zero-progress watchdog passes required on top of the
    /// discard volume.
    #[serde(
        default = "default_broken_piece_min_windows",
        skip_serializing_if = "is_default_broken_piece_min_windows"
    )]
    pub broken_piece_min_windows: u32,
    /// Re-probe interval for quarantined hashes: after this long, the
    /// selection gate admits the hash again (a fresh probe; if the swarm
    /// is still broken the watchdog re-quarantines within one interval).
    #[serde(
        default = "default_quarantine_cooldown",
        deserialize_with = "de_quarantine_cooldown",
        serialize_with = "ser_duration_secs_opt",
        skip_serializing_if = "is_default_quarantine_cooldown"
    )]
    pub quarantine_cooldown: Duration,
    /// Quarantine cycles tolerated before the cooldown becomes indefinite
    /// (0 = unlimited; raise only for catalog entries that stay broken for
    /// many months).
    #[serde(
        default = "default_quarantine_max_retries",
        skip_serializing_if = "is_default_quarantine_max_retries"
    )]
    pub quarantine_max_retries: u32,
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

/// Core for the per-field Duration deserializers below: an explicit `null`
/// (serde_json/yaml) falls back to the FIELD's own default, not some other
/// field's. A field's absence is handled separately by its `default = ..`
/// attribute; only `null` reaches here.
///
/// History: a single shared helper used to fall back to
/// `DEFAULT_SCAN_INTERVAL` for every field it served, so
/// `stall_eviction_timeout: null` silently meant 14d (the pre-2026-09-26
/// default) instead of the documented 90d. Each field now has a thin
/// wrapper; fix fallbacks here, in exactly one place.
fn de_duration_secs<'de, D: serde::Deserializer<'de>>(
    d: D,
    fallback: Duration,
) -> std::result::Result<Duration, D::Error> {
    use serde::de::Deserialize as _;
    Ok(Option::<u64>::deserialize(d)?
        .map(Duration::from_secs)
        .unwrap_or(fallback))
}

macro_rules! de_secs_wrapper {
    ($name:ident, $fallback:expr) => {
        fn $name<'de, D: serde::Deserializer<'de>>(
            d: D,
        ) -> std::result::Result<Duration, D::Error> {
            de_duration_secs(d, $fallback)
        }
    };
}

de_secs_wrapper!(de_scan_interval, DEFAULT_SCAN_INTERVAL);
de_secs_wrapper!(de_moderation_delay, DEFAULT_MODERATION_DELAY);
de_secs_wrapper!(de_stall_timeout, DEFAULT_STALL_EVICTION_TIMEOUT);
de_secs_wrapper!(de_vanished_timeout, DEFAULT_VANISHED_EVICTION_TIMEOUT);
de_secs_wrapper!(de_quarantine_check, DEFAULT_QUARANTINE_CHECK_INTERVAL);
de_secs_wrapper!(de_quarantine_cooldown, DEFAULT_QUARANTINE_COOLDOWN);
de_secs_wrapper!(de_stats_interval, DEFAULT_STATS_INTERVAL);

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
    /// Catalog collapse guard (percent): refuse the removed-from-catalog
    /// eviction pass when a fresh catalog lists fewer than this percent of
    /// the held set - a mass deletion is far more likely a parse/schema
    /// accident than reality, and that pass deletes downloaded data with the
    /// state entries. 0 disables the guard; 100 is the strictest.
    #[serde(default = "default_catalog_collapse_percent")]
    pub catalog_collapse_percent: u32,
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
        deserialize_with = "de_stats_interval",
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
fn default_catalog_collapse_percent() -> u32 {
    DEFAULT_CATALOG_COLLAPSE_PERCENT
}
fn is_default_stats_interval(d: &Duration) -> bool {
    *d == DEFAULT_STATS_INTERVAL
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

impl Default for ScanConfig {
    /// Manual, not derived: a config file (or `#[serde(default)]` field)
    /// missing the whole `scan` section must land on the documented
    /// defaults, not zero values. A derived Default used to give
    /// `interval = 0` here (60s-floored continuous scans) — a latent
    /// hazard the quarantine validation surfaced.
    fn default() -> Self {
        ScanConfig {
            interval: DEFAULT_SCAN_INTERVAL,
            rate_limit_per_second: DEFAULT_RATE_LIMIT_PER_SEC,
            min_seed_margin: DEFAULT_MIN_SEED_MARGIN,
            moderation_delay: DEFAULT_MODERATION_DELAY,
            stall_eviction_timeout: DEFAULT_STALL_EVICTION_TIMEOUT,
            vanished_eviction_timeout: DEFAULT_VANISHED_EVICTION_TIMEOUT,
            quarantine_check_interval: DEFAULT_QUARANTINE_CHECK_INTERVAL,
            broken_piece_discard_bytes: DEFAULT_BROKEN_PIECE_DISCARD_BYTES,
            broken_piece_min_windows: DEFAULT_BROKEN_PIECE_MIN_WINDOWS,
            quarantine_cooldown: DEFAULT_QUARANTINE_COOLDOWN,
            quarantine_max_retries: DEFAULT_QUARANTINE_MAX_RETRIES,
        }
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
                vanished_eviction_timeout: DEFAULT_VANISHED_EVICTION_TIMEOUT,
                quarantine_check_interval: DEFAULT_QUARANTINE_CHECK_INTERVAL,
                broken_piece_discard_bytes: DEFAULT_BROKEN_PIECE_DISCARD_BYTES,
                broken_piece_min_windows: DEFAULT_BROKEN_PIECE_MIN_WINDOWS,
                quarantine_cooldown: DEFAULT_QUARANTINE_COOLDOWN,
                quarantine_max_retries: DEFAULT_QUARANTINE_MAX_RETRIES,
            },
            aggressiveness: DEFAULT_AGGRESSIVENESS,
            keyword_blocklist: Vec::new(),
            preserve_deleted_torrents: false,
            catalog_collapse_percent: DEFAULT_CATALOG_COLLAPSE_PERCENT,
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
    ///
    /// The API key no longer lives in the file: it is merged from
    /// `<data_dir>/api_key` (owner-only) when present. A legacy inline
    /// `api_key:` field still parses for back-compat; the key file wins.
    pub fn load(path: &Path) -> Result<Config> {
        Self::load_inner(path, true)
    }

    /// Load a config file without ever writing anything: for read-only
    /// commands (status/stop/logs/history/hosted) and /proc cmdline probes.
    /// A missing config is an error, never a starter write — the probe must
    /// not materialize configs for paths that merely appear in another
    /// process's argv (and a concurrent starter-generating `run` would race
    /// it: whoever writes second parses a starter instead of reporting it).
    pub fn load_readonly(path: &Path) -> Result<Config> {
        Self::load_inner(path, false)
    }

    fn load_inner(path: &Path, write_starter: bool) -> Result<Config> {
        let data = match std::fs::read(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !write_starter {
                    bail!("no config at {}", path.display());
                }
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
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                bail!(
                    "reading config {} denied: {e}. The config of an older keep-at install is root-only; run `sudo keep-at service install` once (or restart the daemon) to migrate it world-readable",
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
        // API key from the secret file (authoritative at runtime). Unreadable
        // (non-owner) => keep whatever the config field said, never fatal —
        // an anonymous node is a supported mode.
        cfg.merge_key_file();
        cfg.validate()?;
        Ok(cfg)
    }

    /// Save the config world-readable (nothing secret in it anymore) and,
    /// when an API key is set, write it owner-only to
    /// `<data_dir>/api_key` instead of the config file.
    pub fn save(&self, path: &Path) -> Result<()> {
        let header = "# keep-at config. Edit this file directly and restart keep-at\n# (`systemctl restart keep-at`, or `keep-at service install` again)\n# to apply changes. Every field here also has a --flag equivalent\n# (`keep-at run --help`).\n#\n# The Academic Torrents API key is NOT stored here: it lives in\n# <data_dir>/api_key (owner-only, 0600), so this file can stay\n# world-readable for `status`/`hosted-torrents` run as any user.\n# Set it with --api-key or write the file directly.\n\n";
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating directory for {}", path.display()))?;
        }
        let mut out = header.to_string();
        // Persist the key file FIRST: if its write fails (data dir full -
        // keep-at fills disks by design - or read-only), the config on disk
        // must still carry the inline secret so the key survives. Writing
        // the keyless config first used to lose the key permanently on any
        // key-file failure: config rewritten, key nowhere, node silently
        // anonymous after reload.
        if !self.api_key.is_empty() {
            self.write_api_key_file()?;
        }
        // Never serialize the key: it is not a config-file field anymore.
        let mut public = self.clone();
        public.api_key = String::new();
        out.push_str(&serde_yaml::to_string(&public).context("marshalling config")?);
        atomic_write_mode(path, out.as_bytes(), 0o644)?;
        Ok(())
    }

    /// Persist the API key owner-only under the data dir. Best effort from
    /// callers that must not fail over it (startup), strict from save().
    pub fn write_api_key_file(&self) -> Result<()> {
        let p = self.data_dir.join(API_KEY_FILE);
        atomic_write_mode(&p, format!("{}\n", self.api_key).as_bytes(), 0o600)
            .with_context(|| format!("writing {}", p.display()))
    }

    /// One-time migration of a pre-split config: when the API key is
    /// embedded in the file, rewrite the config world-readable without it
    /// (the daemon writes the key file separately at startup). When the key
    /// is already absent but the file is owner-only (legacy mode), widen it.
    /// No-op for current-format files. Call as the daemon user (owner).
    pub fn split_inline_secret(path: &Path, cfg: &Config) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let mode = match std::fs::metadata(path) {
            Ok(m) => m.permissions().mode() & 0o777,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("stating {}", path.display())),
        };
        if cfg.api_key.is_empty() {
            if mode & 0o077 == 0 {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
                    .with_context(|| format!("widening {}", path.display()))?;
            }
            return Ok(());
        }
        // save() writes the config keyless + world-readable; the key file
        // itself is written at startup (write_api_key_file).
        cfg.save(path)
    }

    /// Merge the `<data_dir>/api_key` secret file into this config when
    /// present and the config field is still empty. Called from every path
    /// that assembles a runtime config (file loads merge it inside
    /// `load_inner`; flag-only runs call this after the data dir resolves)
    /// so attribution works without a `--api-key` flag on every invocation.
    /// Precedence: an explicit `--api-key` flag beats the file, which beats
    /// nothing. Unreadable (non-owner) keeps whatever the field said, never
    /// fatal - an anonymous node is a supported mode.
    pub fn merge_key_file(&mut self) {
        if !self.api_key.is_empty() {
            return;
        }
        if let Ok(k) = std::fs::read_to_string(self.data_dir.join(API_KEY_FILE)) {
            let k = k.trim();
            if !k.is_empty() {
                self.api_key = k.to_string();
            }
        }
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
            // Dedup on the best-effort canonical path: symlink aliases of
            // one directory must count once (the authoritative alias check
            // also runs in resolve_all_limits on the daemon path).
            let key = loc.path.canonicalize().unwrap_or_else(|_| loc.path.clone());
            if !seen.insert(key) {
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
        if self.catalog_collapse_percent > 100 {
            bail!(
                "catalog_collapse_percent must be 0-100, got {}",
                self.catalog_collapse_percent
            );
        }
        if self.scan.min_seed_margin < 0 {
            bail!("scan.min_seed_margin must not be negative");
        }
        if self.scan.broken_piece_min_windows == 0 {
            bail!(
                "scan.broken_piece_min_windows must be at least 1 (0 windows \
                 would quarantine on discard volume alone)"
            );
        }
        if self.scan.broken_piece_discard_bytes == 0 {
            bail!(
                "scan.broken_piece_discard_bytes must be at least 1 byte (0 \
                 would quarantine any live torrent with no verified bytes)"
            );
        }
        if self.scan.quarantine_check_interval > Duration::ZERO
            && self.scan.quarantine_check_interval < Duration::from_secs(60)
        {
            bail!(
                "scan.quarantine_check_interval must be 0 (disabled) or at least 60s, got {:?}",
                self.scan.quarantine_check_interval
            );
        }
        if self.port == 0 {
            bail!("port {} is out of range", self.port);
        }
        if self.scan.rate_limit_per_second.is_nan() || self.scan.rate_limit_per_second <= 0.0 {
            // !(x > 0.0), not x <= 0.0: NaN compares false against both and
            // would otherwise sail through validation into
            // Duration::from_secs_f64(1.0/NaN), which panics - and the
            // release profile is panic=abort, so that is a boot loop.
            bail!("scan.rate_limit_per_second must be a positive finite number");
        }
        if self.scan.rate_limit_per_second < MIN_RATE_LIMIT_PER_SECOND {
            bail!(
                "scan.rate_limit_per_second must be at least {} (one request every \
                 {}s at that rate would wedge a scan for months) - got {}",
                MIN_RATE_LIMIT_PER_SECOND,
                (1.0 / MIN_RATE_LIMIT_PER_SECOND) as u64,
                self.scan.rate_limit_per_second
            );
        }
        for (name, d) in [
            ("scan.interval", self.scan.interval),
            ("scan.moderation_delay", self.scan.moderation_delay),
            (
                "scan.stall_eviction_timeout",
                self.scan.stall_eviction_timeout,
            ),
            (
                "scan.vanished_eviction_timeout",
                self.scan.vanished_eviction_timeout,
            ),
            (
                "scan.quarantine_check_interval",
                self.scan.quarantine_check_interval,
            ),
            ("scan.quarantine_cooldown", self.scan.quarantine_cooldown),
            ("stats_interval", self.stats_interval),
        ] {
            check_duration_knob(name, d)?;
        }
        check_bandwidth_limit("max_ram", self.max_ram)?;
        check_bandwidth_limit("upload_rate_limit", self.upload_rate_limit)?;
        check_bandwidth_limit("download_rate_limit", self.download_rate_limit)?;
        Ok(())
    }
}

fn write_starter_config(path: &Path) -> Result<()> {
    let header = "# keep-at starter config. This is entirely optional - every field here has\n# a --flag equivalent (run `keep-at run --help`). Reach for a config file\n# once you want more than one storage location, or don't want to repeat\n# flags every time.\n#\n# At minimum, set a real limit (e.g. 500G, 2T) below.\n#\n# To get credit for the torrents you seed, set an Academic Torrents API key\n# (https://academictorrents.com/my.php) via --api-key or <data_dir>/api_key\n# (owner-only file) - it's only sent to AT's own trackers and never logged.\n\n";
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
    // Flush data blocks BEFORE the rename: without fsync the rename can
    // commit while the temp file's blocks are still unordered on disk, so
    // power loss can leave the destination zero-length or truncated —
    // and state.json is load-fatal (a corrupt one boot-loops the daemon
    // under the watchdog). Cost: one fsync per save (state.json saves are
    // seconds apart at most, not per-request). Opened READ-WRITE on
    // purpose: Windows refuses FlushFileBuffers on a read-only handle, so
    // `File::open` here would hard-fail every state save there.
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&tmp)
        .and_then(|f| f.sync_all())
        .with_context(|| format!("syncing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("finalizing {}", path.display()))?;
    // Persist the rename itself so the old-content window cannot survive
    // a crash either. Best-effort: some filesystems (network mounts)
    // reject directory fsyncs.
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    // rename preserves the temp file's mode; belt-and-suspenders in case a
    // platform ever copies instead of renaming.
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    Ok(())
}

/// Ensure the data-dir subtree is traversable: chmod `dir` itself to at
/// least owner-rwx + group/other rx (0o755 masked in, never stripped), so a
/// data dir created earlier under a restrictive umask (or by root's 077
/// default) still lets other users reach the world-readable snapshots
/// inside. Best effort, never fatal.
///
/// Deliberately scoped to the data dir ONLY: the previous version walked up
/// to `/` OR-ing 0o755 into every owned ancestor, which silently widened
/// private directories that merely happen to contain the data dir - a
/// `--data-dir /home/alice/private/kat` turned `$HOME` itself
/// world-traversable as a side effect of running `keep-at status`.
pub fn ensure_shared_dirs(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(m) = std::fs::metadata(dir) {
        let mode = m.permissions().mode() & 0o777;
        let fixed = mode | 0o755;
        if fixed != mode {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(fixed));
        }
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
    let bytes = n * mult;
    // f64→u64 casts SATURATE at u64::MAX (rustc ≥1.45): an input like
    // "999999999T" would silently become ~18 EB and neuter every limit
    // and threshold it feeds. Reject instead of saturating.
    if bytes >= u64::MAX as f64 {
        bail!("byte size {s:?} overflows (max is 18446744073709551615 bytes)");
    }
    Ok(bytes as u64)
}

/// Upper bound for every Duration knob. The scheduler and calendar
/// arithmetic these feed overflow beyond huge values (std Instant
/// checked_add panics past u64 seconds; chrono DateTime maxes at year
/// 262143), and a release panic is abort → watchdog boot loop. 366 days
/// is far beyond any sane cadence (the scan default is 14d) and matches
/// the 1-year internal "disabled" sentinel the tickers use.
const MAX_DURATION_KNOB: Duration = Duration::from_secs(366 * 24 * 3600);

/// Lower bound for the catalog request rate: below this, one request
/// every `1/rate` seconds wedges a multi-thousand-request scan for
/// months while activity reads "Scanning".
pub const MIN_RATE_LIMIT_PER_SECOND: f64 = 1e-6;

fn check_duration_knob(name: &str, d: Duration) -> Result<()> {
    if d > Duration::ZERO && d > MAX_DURATION_KNOB {
        bail!(
            "{name} must be 0 (disabled) or at most 366 days, got {}s",
            d.as_secs()
        );
    }
    Ok(())
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
    fn load_readonly_never_writes_a_starter() {
        let dir = std::env::temp_dir().join(format!("keep-at-cfgro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("missing.yaml");

        // Read-only load: a missing config is an error and nothing appears.
        let err = Config::load_readonly(&path).unwrap_err();
        assert!(!path.exists(), "read-only load must not write a starter");
        assert!(err.to_string().contains("no config at"), "err: {err:#}");

        // The starter-writing load still writes (and says so).
        let err2 = Config::load(&path).unwrap_err();
        assert!(err2.to_string().contains("wrote a starter config"));
        assert!(path.exists(), "starter written by the mutating load");

        // An existing file parses identically through both.
        std::fs::write(&path, "port: 41234\nstorage:\n- path: /tmp\n  limit: 1G\n").unwrap();
        assert_eq!(
            Config::load_readonly(&path).unwrap().port,
            Config::load(&path).unwrap().port
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn secrets_split_from_world_readable_config() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("keep-at-cfgmode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = Config {
            data_dir: dir.clone(),
            storage: vec![StorageLocation {
                path: dir.join("storage"),
                limit: StorageLimit::Bytes(200 * 1024 * 1024),
            }],
            api_key: "uid=1;pass=secret".to_string(),
            ..Config::default()
        };
        let path = dir.join("keep-at.yaml");
        cfg.save(&path).unwrap();
        // Config: world-readable, no secret inside.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "config without secrets is world-readable");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("pass=secret"), "key never in config file");
        // Key: owner-only beside the state.
        let kp = dir.join(API_KEY_FILE);
        let kmode = std::fs::metadata(&kp).unwrap().permissions().mode() & 0o777;
        assert_eq!(kmode, 0o600, "api_key file owner-only");
        assert_eq!(
            std::fs::read_to_string(&kp).unwrap().trim(),
            "uid=1;pass=secret"
        );
        // Round trip: load merges the key file back.
        let reloaded = Config::load(&path).unwrap();
        assert_eq!(reloaded.api_key, "uid=1;pass=secret");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_inline_secret_migrates_legacy_config() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("keep-at-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Legacy shape: owner-only file, api_key inline.
        let path = dir.join("config.yaml");
        let legacy = format!(
            "port: {}\ndata_dir: {}\nstorage:\n- path: {}/storage\n  limit: 200M\napi_key: uid=9;pass=hush\n",
            DEFAULT_PORT,
            dir.display(),
            dir.display()
        );
        std::fs::write(&path, legacy).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.api_key, "uid=9;pass=hush", "legacy inline key parses");
        Config::split_inline_secret(&path, &cfg).unwrap();
        // Config now world-readable, key gone from the file, key file written.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "migrated config world-readable");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("pass=hush"));
        let kp = dir.join(API_KEY_FILE);
        let kmode = std::fs::metadata(&kp).unwrap().permissions().mode() & 0o777;
        assert_eq!(kmode, 0o600);
        // Second run: no inline key, file already 0644 — no-op, still 0644.
        let cfg2 = Config::load(&path).unwrap();
        assert_eq!(cfg2.api_key, "uid=9;pass=hush", "key file merged on load");
        Config::split_inline_secret(&path, &cfg2).unwrap();
        let mode2 = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode2, 0o644);
        // Owner-only config with no inline key: widened, content untouched.
        let bare = dir.join("bare.yaml");
        std::fs::write(
            &bare,
            format!(
                "port: {}\ndata_dir: {}\nstorage:\n- path: {}/s\n  limit: 200M\n",
                DEFAULT_PORT,
                dir.display(),
                dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bare, std::fs::Permissions::from_mode(0o600)).unwrap();
        let cfg3 = Config::load(&bare).unwrap();
        Config::split_inline_secret(&bare, &cfg3).unwrap();
        let bmode = std::fs::metadata(&bare).unwrap().permissions().mode() & 0o777;
        assert_eq!(bmode, 0o644, "keyless legacy config widened in place");
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

    #[test]
    fn explicit_null_durations_fall_back_to_own_defaults() {
        // Regression for the shared-deserializer bug: `null` used to fall
        // back to DEFAULT_SCAN_INTERVAL (14d) for every Duration field, so
        // `stall_eviction_timeout: null` silently meant 14d, not the
        // documented 90d. Absence must land on the same defaults as null.
        let dir = std::env::temp_dir().join(format!("keep-at-cfgnull-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nulls.yaml");
        std::fs::write(
            &path,
            format!(
                "port: {}\ndata_dir: {}\nstorage:\n- path: {}/s\n  limit: 200M\nscan:\n  interval: null\n  moderation_delay: null\n  stall_eviction_timeout: null\n  vanished_eviction_timeout: null\n  quarantine_check_interval: null\n  quarantine_cooldown: null\n",
                DEFAULT_PORT,
                dir.display(),
                dir.display()
            ),
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.scan.interval, DEFAULT_SCAN_INTERVAL);
        assert_eq!(cfg.scan.moderation_delay, DEFAULT_MODERATION_DELAY);
        assert_eq!(
            cfg.scan.stall_eviction_timeout,
            DEFAULT_STALL_EVICTION_TIMEOUT
        );
        assert_eq!(
            cfg.scan.vanished_eviction_timeout,
            DEFAULT_VANISHED_EVICTION_TIMEOUT
        );
        assert_eq!(
            cfg.scan.quarantine_check_interval,
            DEFAULT_QUARANTINE_CHECK_INTERVAL
        );
        assert_eq!(cfg.scan.quarantine_cooldown, DEFAULT_QUARANTINE_COOLDOWN);
        assert_eq!(cfg.stats_interval, DEFAULT_STATS_INTERVAL);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_zero_durations_stay_zero() {
        // 0 is meaningful (disable): it must survive null-fallback logic.
        let dir = std::env::temp_dir().join(format!("keep-at-cfgzero-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("zeros.yaml");
        std::fs::write(
            &path,
            format!(
                "port: {}\ndata_dir: {}\nstorage:\n- path: {}/s\n  limit: 200M\nscan:\n  stall_eviction_timeout: 0\n  vanished_eviction_timeout: 0\n  quarantine_check_interval: 0\n",
                DEFAULT_PORT,
                dir.display(),
                dir.display()
            ),
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.scan.stall_eviction_timeout, Duration::ZERO);
        assert_eq!(cfg.scan.vanished_eviction_timeout, Duration::ZERO);
        assert_eq!(cfg.scan.quarantine_check_interval, Duration::ZERO);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
