//! `keep-at logs`: print or follow the daemon's log file.
//!
//! Follow is the default (`tail -f` semantics: start at the last N lines,
//! poll for growth, keep printing). `--all` prints everything and exits.
//!
//! Where the log lives: `<data_dir>/keep-at.log` — the daemon writes there
//! by default (start-launched daemons always; flag-run daemons unless
//! `--log-file` points elsewhere). A custom `--log-file` from the resolved
//! config wins. When no log file exists at all (e.g. a foreground `run`
//! without `--log-file` writes to stdout, which is lost), the command says
//! so instead of following an empty file.
//!
//! Follow uses plain file polling (500ms) rather than inotify: keep-at's
//! log appends continuously, the file never rotates or shrinks in place,
//! and polling survives the log file being replaced (new daemon boot
//! truncates) — the reader just keeps following the path. Log lines are
//! already emitted newline-terminated by the writer, so reads are plain
//! chunked reads; partial trailing lines are carried over to the next poll.

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::cli::CommonArgs;

/// Resolve the log file path for this instance: the config's `log_file`
/// when set, else `<data_dir>/keep-at.log` (the daemon's default).
pub fn resolve_log_path(args: &CommonArgs) -> Result<PathBuf> {
    let dir = crate::cli::resolve_data_dir(args)?;
    crate::config::ensure_shared_dirs(&dir);
    // Config's log_file (when resolvable) beats the data-dir default; a
    // permission-denied config read falls through to the default silently —
    // `logs` is a read-only command and must work for any user.
    if let Some(p) = &args.config {
        if let Ok(cfg) = crate::config::Config::load(p) {
            if let Some(l) = &cfg.log_file {
                return Ok(l.clone());
            }
        }
    }
    if let Some(cfg) = service_config_snapshot() {
        if let Some(l) = &cfg.log_file {
            return Ok(l.clone());
        }
    }
    Ok(dir.join("keep-at.log"))
}

pub fn cmd_logs(args: &crate::cli::LogsArgs) -> Result<()> {
    let path = resolve_log_path(&args.common)?;
    if !path.exists() {
        anyhow::bail!(
            "no log file at {}\nthe daemon writes {} when started in the background (`keep-at start` or the service); a foreground `keep-at run` without --log-file logs to stdout only",
            path.display(),
            path.display()
        );
    }

    if args.all {
        let data = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        use std::io::Write;
        std::io::stdout().write_all(&data).context("writing log")?;
        return Ok(());
    }

    // Follow: print the last `lines` lines, then stream new bytes as they
    // land. Partial trailing lines are carried to the next poll (a line is
    // complete only once it ends with \n).
    let mut pos = print_tail(&path, args.lines)?;
    loop {
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(pos);
        if len > pos {
            let mut f = std::fs::File::open(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            use std::io::Seek;
            f.seek(std::io::SeekFrom::Start(pos))?;
            let mut buf = String::new();
            f.read_to_string(&mut buf).unwrap_or(0);
            // Hold back a partial trailing line: only print up to the last \n.
            let emit = match buf.rfind('\n') {
                Some(i) => &buf[..=i],
                None => "",
            };
            if !emit.is_empty() {
                use std::io::Write;
                print!("{emit}");
                std::io::stdout().flush().context("flushing log output")?;
            }
            let consumed = if emit.is_empty() { 0 } else { emit.len() };
            pos += consumed as u64;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// Print the last `lines` complete lines of `path`; returns the byte offset
/// just past the last complete line (the follow position).
fn print_tail(path: &std::path::Path, lines: usize) -> Result<u64> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = String::from_utf8_lossy(&data);
    let complete_len = match text.rfind('\n') {
        Some(i) => i + 1,
        None => 0,
    };
    let complete = &text[..complete_len];
    let start = if lines == 0 {
        complete_len
    } else {
        // Count back `lines` newlines; point at the char after the (lines-th
        // from the end) newline so exactly `lines` lines print.
        let mut idx = complete_len;
        let mut seen = 0usize;
        for (off, b) in complete.as_bytes().iter().enumerate().rev() {
            if *b == b'\n' {
                seen += 1;
                if seen == lines {
                    idx = off + 1;
                    break;
                }
            }
        }
        if seen < lines {
            0
        } else {
            idx
        }
    };
    use std::io::Write;
    print!("{}", &complete[start..]);
    std::io::stdout().flush().context("flushing log output")?;
    Ok(complete_len as u64)
}

/// Best-effort load of the service config (for log-path resolution).
/// Read-only commands must never fail on it: permission errors fall
/// through to the data-dir default.
fn service_config_snapshot() -> Option<crate::config::Config> {
    let p = crate::cli::service_config_if_present()?;
    crate::config::Config::load(&p).ok()
}
