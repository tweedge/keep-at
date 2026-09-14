//! Append-only history of what the daemon did to its holdings, plus the
//! `keep-at history` command that renders it.
//!
//! Every addition (fill or swap, with the seed-scarcity statistics that
//! admitted the torrent), every swap's displaced set, and every drop
//! (stalled zero-seeder eviction, vanished-from-catalog removal) is
//! recorded as one JSON line in `<data_dir>/history.jsonl`. Boot resume
//! re-adds are deliberately NOT recorded — they re-materialize holdings
//! that were already recorded when first acquired.
//!
//! The file is JSON Lines with careful handling on both sides:
//! - writes are single `serde_json` lines (embedded newlines are escaped),
//!   appended O_APPEND and flushed per event; failures are best-effort and
//!   never block scanning,
//! - the file is rotated to `history.jsonl.1` on the write that would push
//!   it past [`MAX_HISTORY_BYTES`] (one previous generation kept), so it is
//!   always size-bounded,
//! - reads skip unparseable lines (a torn tail after a crash, or lines from
//!   a newer format) and report how many they skipped.
//!
//! `keep-at history` renders the events human-readably, tail + follow by
//! default (like `logs`), color-coded: additions green, swaps (and what
//! they displaced) yellow, drops red. Colors are used only when stdout is
//! a terminal and `NO_COLOR` is unset.

use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cli::HistoryArgs;

/// Size cap for `history.jsonl`. The write that would exceed it rotates the
/// file to `history.jsonl.1` first, so history covers roughly two caps.
pub const MAX_HISTORY_BYTES: u64 = 5 * 1024 * 1024;

/// Size cap for the daemon's own log file (`keep-at.log`). The daemon
/// writes its log directly (stdout/stderr are dup2'd into the file, not
/// streamed to journald), so nothing else rotates it; [`cap_text_file`]
/// rewrites it in place to its newest half when it outgrows this.
pub const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// Where the holdings history lives for a data dir.
pub fn history_path(data_dir: &Path) -> PathBuf {
    data_dir.join("history.jsonl")
}

/// Why a torrent entered or left the holding set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// Free space was available; the seed-scarcity gate admitted it.
    Fill,
    /// It displaced one or more held torrents (see Add::displaced).
    Swap,
    /// Zero seeders and no piece progress past the stall timeout.
    Stalled,
    /// No longer listed on Academic Torrents.
    DeletedFromCatalog,
}

impl Cause {
    fn label(&self) -> &'static str {
        match self {
            Cause::Fill => "fill",
            Cause::Swap => "swap",
            Cause::Stalled => "stalled",
            Cause::DeletedFromCatalog => "deleted-from-catalog",
        }
    }
}

/// One torrent displaced by a swap, embedded in the winner's Add event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Displaced {
    pub hash: String,
    pub title: String,
    pub seeders: u32,
    pub size_bytes: u64,
}

/// One history record. `event` is the JSON discriminator tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Add {
        ts: String,
        hash: String,
        title: String,
        size_bytes: u64,
        seeders: u32,
        location: PathBuf,
        cause: Cause,
        /// Seed-scarcity admission statistics: `chance` =
        /// aggressiveness^(seeders - seeder_floor), `roll` = the uniform
        /// draw that was compared against it, `reason` = the selector's
        /// plain-language explanation.
        chance: f64,
        roll: f64,
        seeder_floor: u32,
        reason: String,
        /// Torrents this add displaced (swap adds only; omitted for fills).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        displaced: Vec<Displaced>,
    },
    Remove {
        ts: String,
        hash: String,
        title: String,
        size_bytes: u64,
        seeders: u32,
        location: PathBuf,
        cause: Cause,
        reason: String,
    },
}

fn now_ts() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Build an Add event (timestamped now).
#[allow(clippy::too_many_arguments)]
pub fn add_event(
    hash: &str,
    title: &str,
    size_bytes: u64,
    seeders: u32,
    location: &Path,
    cause: Cause,
    chance: f64,
    roll: f64,
    seeder_floor: u32,
    reason: &str,
    displaced: Vec<Displaced>,
) -> Event {
    Event::Add {
        ts: now_ts(),
        hash: hash.to_string(),
        title: title.to_string(),
        size_bytes,
        seeders,
        location: location.to_path_buf(),
        cause,
        chance,
        roll,
        seeder_floor,
        reason: reason.to_string(),
        displaced,
    }
}

/// Build a Remove event (timestamped now).
pub fn remove_event(
    hash: &str,
    title: &str,
    size_bytes: u64,
    seeders: u32,
    location: &Path,
    cause: Cause,
    reason: &str,
) -> Event {
    Event::Remove {
        ts: now_ts(),
        hash: hash.to_string(),
        title: title.to_string(),
        size_bytes,
        seeders,
        location: location.to_path_buf(),
        cause,
        reason: reason.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Writer (daemon side)
// ---------------------------------------------------------------------------

/// Appends events to `history.jsonl`, rotating at the size cap. Single
/// writer (the scan task) — no locking needed. All failures are silent:
/// history must never take the daemon down.
pub struct Writer {
    path: PathBuf,
    cap: u64,
    file: Option<std::fs::File>,
}

impl Writer {
    pub fn new(path: PathBuf) -> Writer {
        let cap = MAX_HISTORY_BYTES;
        let file = Self::open_append(&path).ok();
        Writer { path, cap, file }
    }

    /// Test seam: a writer with a tiny cap so rotation is exercisable.
    pub fn with_cap(path: PathBuf, cap: u64) -> Writer {
        let file = Self::open_append(&path).ok();
        Writer { path, cap, file }
    }

    fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;
        // World-readable like every other file in the data dir.
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o644)
            .open(path)
    }

    pub fn record(&mut self, ev: &Event) {
        let Ok(mut line) = serde_json::to_string(ev) else {
            return;
        };
        line.push('\n');
        // Rotate on the write that would cross the cap, so the live file
        // stays bounded and .1 holds the previous generation.
        if let Ok(md) = std::fs::metadata(&self.path) {
            if md.len().saturating_add(line.len() as u64) > self.cap {
                self.rotate();
            }
        }
        self.write_line(&line);
    }

    fn write_line(&mut self, line: &str) {
        // One retry through a fresh handle: the file may have been removed
        // (data dir cleanup) or the handle may have gone stale.
        for _ in 0..2 {
            if self.file.is_none() {
                self.file = Self::open_append(&self.path).ok();
            }
            match self.file.as_mut() {
                Some(f) => {
                    if f.write_all(line.as_bytes()).and_then(|_| f.flush()).is_ok() {
                        return;
                    }
                    self.file = None;
                }
                None => return, // unwritable data dir: drop the event
            }
        }
    }

    fn rotate(&mut self) {
        self.file = None;
        let mut old = self.path.as_os_str().to_os_string();
        old.push(".1");
        let _ = std::fs::rename(&self.path, PathBuf::from(old));
        self.file = Self::open_append(&self.path).ok();
    }
}

/// Cap a plain-text log file in place at `max_bytes`: when it is larger,
/// rewrite the same inode down to its newest `max_bytes/2`, cut at a line
/// boundary. In-place on purpose: the daemon's stdout/stderr (dup2'd
/// O_APPEND) and the forensics death-mark fd keep pointing at the live log,
/// so renaming underneath them would silently strand future writes in the
/// rotated file. The write-then-truncate ordering keeps the tail intact
/// across a crash mid-cap (at worst stale bytes past the new tail, trimmed
/// by the next pass); a line appended by a concurrent O_APPEND writer
/// inside the read->truncate window can be lost - the check runs on the
/// stats cadence, so that window is rare and the worst case is one old log
/// line.
pub fn cap_text_file(path: &Path, max_bytes: u64) -> std::io::Result<bool> {
    let len = std::fs::metadata(path)?.len();
    if len <= max_bytes {
        return Ok(false);
    }
    let keep = (max_bytes / 2).max(1);
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    f.seek(std::io::SeekFrom::Start(len.saturating_sub(keep)))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    // Resume at a line boundary so the capped log never starts mid-line.
    let cut = buf.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
    // Write the kept tail at offset 0 FIRST, then truncate to its length:
    // the source region never overlaps the destination (keep <= len/2, so a
    // forward copy cannot clobber unread source), so a kill between the
    // write and the set_len leaves the tail intact with stale bytes past it
    // that the next cap pass trims. The previous order (set_len(0) then
    // write) erased the ENTIRE log - death marks and panic evidence
    // included - at exactly the moment forensics matter.
    f.seek(std::io::SeekFrom::Start(0))?;
    let tail = &buf[cut..];
    f.write_all(tail)?;
    f.set_len(tail.len() as u64)?;
    let _ = f.sync_all();
    Ok(true)
}

// ---------------------------------------------------------------------------
// Reader (CLI side)
// ---------------------------------------------------------------------------

/// Parse the whole history file: `(events, unparseable lines skipped)`.
/// Unparseable lines are torn writes (crash mid-append) or lines from a
/// newer format; skipping keeps `history` working across both.
pub fn read_events(path: &Path) -> (Vec<Event>, usize) {
    let Ok(data) = std::fs::read(path) else {
        return (Vec::new(), 0);
    };
    parse_lines(&data)
}

fn parse_lines(data: &[u8]) -> (Vec<Event>, usize) {
    let mut events = Vec::new();
    let mut skipped = 0usize;
    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<Event>(line) {
            Ok(ev) => events.push(ev),
            Err(_) => skipped += 1,
        }
    }
    (events, skipped)
}

const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

fn paint(color: bool, code: &str, s: String) -> String {
    if color {
        format!("{code}{s}{RESET}")
    } else {
        s
    }
}

fn fmt_ts(ts: &str) -> String {
    DateTime::parse_from_rfc3339(ts)
        .map(|t| {
            t.with_timezone(&Utc)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| ts.to_string())
}

/// Render one event as one or more display lines (a swap's displaced set
/// gets a line each, under the winner).
pub fn render(ev: &Event, color: bool) -> Vec<String> {
    match ev {
        Event::Add {
            ts,
            hash,
            title,
            size_bytes,
            seeders,
            location,
            cause,
            chance,
            roll,
            seeder_floor,
            reason,
            displaced,
        } => {
            let ts = fmt_ts(ts);
            let code = if matches!(cause, Cause::Swap) {
                YELLOW
            } else {
                GREEN
            };
            let verb = if matches!(cause, Cause::Swap) {
                "swapped in "
            } else {
                "added     "
            };
            let sanitized_title = crate::humanize::sanitize_title(title);
            let mut out = vec![paint(
                color,
                code,
                format!(
                    "{ts} {verb}{sanitized_title} ({}, {} seeders) at {} — {}: {reason} \
                     (chance {chance:.2}, roll {roll:.2}, seeder floor {seeder_floor}) [{}]",
                    crate::humanize::human_bytes(*size_bytes as i64),
                    seeders,
                    location.display(),
                    cause.label(),
                    &hash[..hash.len().min(8)],
                ),
            )];
            for d in displaced {
                out.push(paint(
                    color,
                    code,
                    format!(
                        "{ts}   displaced {} ({}, {} seeders) [{}]",
                        crate::humanize::sanitize_title(&d.title),
                        crate::humanize::human_bytes(d.size_bytes as i64),
                        d.seeders,
                        &d.hash[..d.hash.len().min(8)],
                    ),
                ));
            }
            out
        }
        Event::Remove {
            ts,
            hash,
            title,
            size_bytes,
            seeders,
            location,
            cause,
            reason,
        } => {
            let ts = fmt_ts(ts);
            let sanitized_title = crate::humanize::sanitize_title(title);
            vec![paint(
                color,
                RED,
                format!(
                    "{ts} dropped   {sanitized_title} ({}, {} seeders) from {} — {}: {reason} [{}]",
                    crate::humanize::human_bytes(*size_bytes as i64),
                    seeders,
                    location.display(),
                    cause.label(),
                    &hash[..hash.len().min(8)],
                ),
            )]
        }
    }
}

// ---------------------------------------------------------------------------
// `keep-at history` command
// ---------------------------------------------------------------------------

/// The `keep-at history` command: tail + follow by default, `--all` prints
/// everything and exits. Same file-following philosophy as `logs` (500ms
/// polling, complete lines only, survives the file being replaced) but each
/// complete line is parsed and rendered as an event.
pub fn cmd_history(args: &HistoryArgs) -> Result<()> {
    let dir = crate::cli::resolve_data_dir(&args.common)?;
    crate::config::ensure_shared_dirs(&dir);
    let path = history_path(&dir);
    let color = std::io::IsTerminal::is_terminal(&std::io::stdout())
        && std::env::var_os("NO_COLOR").is_none();
    if !path.exists() {
        anyhow::bail!(
            "no history at {}\nthe daemon records additions, swaps, and removals there once it starts changing what it holds",
            path.display()
        );
    }

    let data = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let complete_len = data.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let (events, skipped) = parse_lines(&data[..complete_len]);
    if skipped > 0 {
        eprintln!(
            "# note: skipped {skipped} unparseable line(s) in {} (torn write or newer format)",
            path.display()
        );
    }
    if args.all {
        for ev in &events {
            for line in render(ev, color) {
                println!("{line}");
            }
        }
        return Ok(());
    }

    // Tail: the last `lines` events, oldest first.
    for ev in events.iter().rev().take(args.lines).rev() {
        for line in render(ev, color) {
            println!("{line}");
        }
    }
    if args.no_follow {
        return Ok(());
    }
    let mut pos = complete_len as u64;
    let mut last_id: Option<(u64, u64)> = None;
    loop {
        let meta = std::fs::metadata(&path).ok();
        if let Some(m) = &meta {
            use std::os::unix::fs::MetadataExt as _;
            let id = (m.dev(), m.ino());
            let len = m.len();
            // tail -F semantics: Writer::rotate renames the live file and
            // starts a fresh generation underneath a running follow (new
            // inode), and any shrink - manual truncation, a cap - would
            // trip the length check. The stale offset would otherwise stall
            // until the new file regrows past it, then skip its first `pos`
            // bytes. Reset to the new head instead.
            if len < pos || last_id.is_some_and(|prev| prev != id) {
                pos = 0;
                println!("--- history rotated; following the new generation ---");
                use std::io::Write;
                std::io::stdout().flush().ok();
            }
            last_id = Some(id);
            if len > pos {
                let mut f = std::fs::File::open(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                f.seek(std::io::SeekFrom::Start(pos))?;
                let mut buf = Vec::new();
                f.read_to_end(&mut buf).unwrap_or(0);
                // Hold back a partial trailing line: only complete lines parse.
                let complete = match buf.iter().rposition(|&b| b == b'\n') {
                    Some(i) => &buf[..=i],
                    None => &[][..],
                };
                for line in complete.split(|&b| b == b'\n') {
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(ev) = serde_json::from_slice::<Event>(line) {
                        for out in render(&ev, color) {
                            println!("{out}");
                        }
                    }
                }
                use std::io::Write;
                std::io::stdout().flush().ok();
                pos += complete.len() as u64;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_add(cause: Cause) -> Event {
        add_event(
            "0123456789abcdef0123456789abcdef01234567",
            "Dataset X",
            2_100_000,
            2,
            Path::new("/mnt/disk1/keep-at"),
            cause,
            0.44,
            0.31,
            3,
            "seed-scarcity roll succeeded",
            vec![Displaced {
                hash: "fedcba9876543210fedcba9876543210fedcba98".to_string(),
                title: "Dataset W".to_string(),
                seeders: 5,
                size_bytes: 800_000,
            }],
        )
    }

    #[test]
    fn add_serializes_as_one_line_without_displaced_for_fill() {
        let ev = add_event(
            "0123456789abcdef0123456789abcdef01234567",
            "Dataset X",
            1,
            1,
            Path::new("/mnt/d"),
            Cause::Fill,
            1.0,
            0.5,
            1,
            "seed-scarcity roll succeeded",
            Vec::new(),
        );
        let line = serde_json::to_string(&ev).unwrap();
        assert!(!line.contains('\n'));
        assert!(line.starts_with(r#"{"event":"add""#));
        assert!(!line.contains("displaced"), "fill adds omit the field");
        let back: Event = serde_json::from_str(&line).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn writer_rotates_at_cap_and_keeps_one_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        // Cap 1 byte: every write rotates first, so the live file always
        // holds exactly the newest event and .1 exactly the previous one.
        let mut w = Writer::with_cap(path.clone(), 1);
        for i in 0..4 {
            w.record(&add_event(
                &format!("{i:040}"),
                &format!("title-{i}"),
                1,
                1,
                Path::new("/mnt/d"),
                Cause::Fill,
                1.0,
                0.5,
                1,
                "seed-scarcity roll succeeded",
                Vec::new(),
            ));
        }
        let rotated = dir.path().join("history.jsonl.1");
        assert!(rotated.exists(), "rotation produced a .1 file");
        let (events, skipped) = read_events(&path);
        assert_eq!(skipped, 0);
        assert_eq!(events.len(), 1, "live file holds only the newest event");
        assert_eq!(event_title(&events[0]), "title-3");
        let (old_events, _) = read_events(&rotated);
        assert_eq!(old_events.len(), 1, ".1 holds the previous generation");
        assert_eq!(event_title(&old_events[0]), "title-2");
    }

    fn event_title(ev: &Event) -> &str {
        match ev {
            Event::Add { title, .. } | Event::Remove { title, .. } => title,
        }
    }

    #[test]
    fn reader_skips_torn_and_unparseable_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let ev = sample_add(Cause::Fill);
        let good = serde_json::to_string(&ev).unwrap();
        let body = format!("{good}\n{{\"event\":\"future-kind\",x:1\n{{\"half\":\n");
        std::fs::write(&path, body).unwrap();
        let (events, skipped) = read_events(&path);
        assert_eq!(events.len(), 1);
        assert_eq!(skipped, 2);
    }

    #[test]
    fn render_color_codes_by_cause() {
        let fill = sample_add(Cause::Fill);
        let swap = sample_add(Cause::Swap);
        let drop = remove_event(
            "0123456789abcdef0123456789abcdef01234567",
            "Dataset Y",
            1,
            0,
            Path::new("/mnt/disk1/keep-at"),
            Cause::Stalled,
            "zero seeders and no download progress for 14 days",
        );
        assert!(render(&fill, false)[0].contains("added     Dataset X"));
        assert!(!render(&fill, false)[0].contains('\x1b'));
        assert!(render(&fill, true)[0].starts_with(GREEN));
        assert!(render(&swap, true)[0].starts_with(YELLOW));
        assert!(render(&swap, true).len() == 2, "displaced gets a line");
        assert!(render(&swap, true)[1].contains("displaced Dataset W"));
        let drop_lines = render(&drop, true);
        assert!(drop_lines[0].starts_with(RED));
        assert!(drop_lines[0].contains("stalled: zero seeders"));
        assert!(render(&drop, false)[0].contains("dropped   Dataset Y"));
    }

    #[test]
    fn cap_text_file_keeps_tail_from_line_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let mut body = String::new();
        for i in 0..100 {
            body.push_str(&format!("line {i:03} of the log ----------------\n"));
        }
        std::fs::write(&path, &body).unwrap();
        assert!(cap_text_file(&path, 1024).unwrap());
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.len() <= 512, "capped to the kept half");
        assert!(after.starts_with("line "), "starts at a line boundary");
        assert!(body.ends_with(&after), "kept the newest content");
        assert!(!cap_text_file(&path, 1024).unwrap(), "no-op under cap");
    }
}
