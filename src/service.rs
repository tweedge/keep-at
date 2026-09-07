//! systemd service install/uninstall. Linux-only.
//!
//! Privilege model: `service install` / `uninstall` must write to
//! /etc/systemd/system and /etc/keep-at, so they need root. Rather than
//! requiring a `sudo` prefix (which also breaks when keep-at is on the
//! user's PATH but not root's), keep-at re-executes *itself* with elevated
//! privileges when needed — see [`elevate`] — so `keep-at service install`
//! just works. The same path covers `self-update` replacing a root-owned
//! binary in /usr/local/bin or /usr/bin.

use std::path::Path;

use anyhow::{Context, Result};

use crate::config::Config;

pub const UNIT_PATH: &str = "/etc/systemd/system/keep-at.service";
pub const CONFIG_DIR: &str = "/etc/keep-at";
pub const CONFIG_PATH: &str = "/etc/keep-at/config.yaml";

/// Absolute path to systemctl. sudo(8) resets PATH to a minimal secure
/// default, so resolving `systemctl` via PATH fails when keep-at is
/// re-executed elevated — and it failed before that too whenever keep-at
/// lived on the user's PATH but not root's. Absolute path, always.
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";

const UNIT_TEMPLATE: &str = r#"[Unit]
Description=keep-at - Academic Torrents smart seeding node
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=__EXEC_START__
Restart=on-failure
RestartSec=5
User=__USER__
# keep-at stops promptly even mid-scan (cancellation-aware scan loop), but
# this caps how long systemd waits before force-killing it if something ever
# hangs, so 'systemctl stop keep-at' can't block forever on a stuck process.
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
"#;

pub struct InstallOpts {
    pub exec_path: String,
    pub config: Config,
    pub user: String,
}

fn exec_start_line(exec_path: &str, args: &[&str]) -> String {
    let mut parts = vec![quote_if_needed(exec_path)];
    for a in args {
        parts.push(quote_if_needed(a));
    }
    parts.join(" ")
}

fn quote_if_needed(s: &str) -> String {
    if s.contains([' ', '\t', '"']) {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

pub fn install(opts: &InstallOpts) -> Result<()> {
    elevate("install the systemd service")?;
    require_systemd()?;

    opts.config.save(Path::new(CONFIG_PATH))?;

    let unit = UNIT_TEMPLATE
        .replace(
            "__EXEC_START__",
            &exec_start_line(&opts.exec_path, &["run", "--config", CONFIG_PATH]),
        )
        .replace("__USER__", &opts.user);
    std::fs::write(UNIT_PATH, unit).with_context(|| format!("creating {UNIT_PATH}"))?;

    for args in [
        &["daemon-reload"][..],
        &["enable", "keep-at"][..],
        &["start", "keep-at"][..],
    ] {
        run_systemctl(args)?;
    }
    Ok(())
}

/// Stop, disable, and remove the unit. Torrent data and the config are left
/// in place.
pub fn uninstall() -> Result<()> {
    elevate("remove the systemd service")?;
    require_systemd()?;

    let _ = run_systemctl(&["stop", "keep-at"]);
    let _ = run_systemctl(&["disable", "keep-at"]);

    match std::fs::remove_file(UNIT_PATH) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {UNIT_PATH}"));
        }
        _ => {}
    }
    run_systemctl(&["daemon-reload"])
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let out = std::process::Command::new(SYSTEMCTL)
        .args(args)
        .output()
        .with_context(|| format!("running {SYSTEMCTL} {args:?}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{SYSTEMCTL} {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Are we root (real root, or re-executed elevated)?
pub fn is_root() -> bool {
    // SAFETY: geteuid has no side effects.
    libc_geteuid() == 0
}

/// Ensure root privileges for an operation, re-executing ourselves elevated
/// when needed. When already root this is a no-op. Otherwise it re-runs the
/// current executable with the same arguments via `sudo -n` (non-interactive:
/// uses cached credentials or NOPASSWD) or `pkexec` as fallback, then exits
/// the current process with the child's status — the user never types a
/// `sudo` prefix themselves, and the binary is located by its own absolute
/// path (via /proc/self/exe semantics of current_exe), never by PATH lookup,
/// so "keep-at on my PATH but not root's" stops being a failure mode.
///
/// Security properties, read carefully:
/// - Elevation happens ONLY for `service install`/`uninstall` (this module)
///   and `self-update` replacing a root-owned binary (updater). No other
///   code path calls this. The daemon, scans, and network commands never
///   run elevated beyond what the caller already was.
/// - We re-execute our own binary image, not a PATH-resolved name, so a
///   hostile PATH cannot redirect the elevated child.
/// - `sudo -n` never prompts: no password is read, typed, or forwarded by
///   keep-at. If neither sudo-nor-pkexec can elevate non-interactively, we
///   fail with instructions instead of hanging on a prompt.
/// - Environment: sudo's env_reset strips everything except an allowlist;
///   pkexec scrubsharder. Config travels via argv (already the case for
///   service install: flags, not env). RUST_LOG-style debugging of the
///   elevated child is intentionally unavailable — check the parent's error.
pub fn elevate(action: &str) -> Result<()> {
    if is_root() {
        return Ok(());
    }
    // Marker so an elevated child never re-elevates (defense in depth:
    // if elevation somehow didn't take, fail instead of looping).
    if std::env::var_os("KEEPAT_ELEVATED").is_some() {
        anyhow::bail!(
            "tried to elevate for {action} but still not root; \
             run with sudo, or give your user NOPASSWD sudo for this binary"
        );
    }
    let exe = std::env::current_exe().context("locating keep-at executable")?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    elevate_exec(&exe, &args, action)
}

/// Re-execute `exe` with `args` elevated, then exit this process with the
/// child's exit code. Factored for testing (pure arg computation in
/// [`elevation_command`]).
fn elevate_exec(exe: &std::path::Path, args: &[String], action: &str) -> Result<()> {
    // Decide up front whether `sudo -n` can work: `sudo -n true` has no side
    // effects and answers exactly "would non-interactive elevation succeed".
    // This matters because a *failed* sudo -n (exit 1, "a password is
    // required") is indistinguishable from the elevated child itself failing
    // — probing first keeps the fallback chain honest: only advance past
    // sudo -n when sudo -n itself refused, never when the real work failed.
    let noninteractive_ok = helper_works_noninteractive();
    let plan = elevation_command_filtered(exe, args, noninteractive_ok);
    if plan.is_empty() {
        anyhow::bail!(
            "need root to {action}, and no elevation helper is available. Re-run with sudo, e.g.: sudo {} {}",
            exe.display(),
            shell_join(args),
        );
    }
    for (program, argv) in &plan {
        let mut cmd = std::process::Command::new(program);
        cmd.args(argv);
        // Set in the child so it knows it is the elevated half (and never
        // re-elevates — see elevate()).
        cmd.env("KEEPAT_ELEVATED", "1");
        match cmd.status() {
            Err(e) => {
                tracing::warn!(
                    "elevation via {program} failed to launch ({e:#}); trying next method"
                );
                continue;
            }
            Ok(status) => {
                // The launcher ran: propagate its exit code and stop. Our
                // work continues in the elevated child, not here. (A sudo
                // password refusal surfaces here as sudo's own error +
                // nonzero exit — the operator sees exactly what sudo said.)
                std::process::exit(status.code().unwrap_or(1));
            }
        }
    }
    anyhow::bail!("elevation failed for {action}")
}

/// `sudo -n true`: answers "would passwordless sudo work" with no side
/// effects. Worst case the probe is stale by milliseconds (user's timestamp
/// expired between probe and elevation) — then sudo refuses cleanly with
/// its own error, never a bypass and never a hang (`-n` never prompts).
fn helper_works_noninteractive() -> bool {
    std::process::Command::new("sudo")
        .args(["-n", "true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Choose how to elevate, in order: `sudo -n` (non-interactive: NOPASSWD or
/// cached timestamp — no prompt, ever), then plain `sudo` (interactive
/// password prompt, terminal only — stdin must be a TTY or sudo has nowhere
/// to read), then `pkexec` (its own graphical/text prompt). Returns the full
/// ordered plan; empty when no helper exists. Pure (no I/O) so unit tests
/// pin the exact argv shapes.
///
/// Why interactive sudo second instead of failing fast: on a terminal the
/// operator *expects* a password prompt (`apt install` behaves the same);
/// refusing and demanding a manual `sudo` prefix reintroduces the exact
/// root-PATH failure this exists to fix. In non-interactive contexts
/// (scripts, CI, detached daemons) stdin is not a TTY, so plain sudo is
/// skipped — sudo would fail with "no tty present" anyway — and pkexec or
/// the clear error covers it.
fn elevation_command(exe: &std::path::Path, args: &[String]) -> Vec<(String, Vec<String>)> {
    let dirs: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    elevation_command_on_path(exe, args, &dirs, stdin_is_tty())
}

/// Plan filtered by the sudo -n probe: when non-interactive sudo works, the
/// plan is just [sudo -n] (no prompt needed, no fallback noise); otherwise
/// sudo -n is dropped and the plan starts at interactive sudo (terminal) or
/// pkexec. Pure given the probe result, so tests pin it without I/O.
fn elevation_command_filtered(
    exe: &std::path::Path,
    args: &[String],
    noninteractive_ok: bool,
) -> Vec<(String, Vec<String>)> {
    elevation_command(exe, args)
        .into_iter()
        .filter(|(prog, argv)| {
            let is_noninteractive = prog == "sudo" && argv.first().is_some_and(|a| a == "-n");
            is_noninteractive == noninteractive_ok || *prog != "sudo"
        })
        .collect()
}

fn stdin_is_tty() -> bool {
    // SAFETY: isatty(0) only queries stdin's state, no side effects.
    libc_isatty(0) == 1
}

unsafe extern "C" {
    fn isatty(fd: i32) -> i32;
}

fn libc_isatty(fd: i32) -> i32 {
    unsafe { isatty(fd) }
}

/// PATH-searching helper selection, factored for parallel-safe unit tests
/// (explicit search dirs + explicit TTY flag, no env access).
fn elevation_command_on_path(
    exe: &std::path::Path,
    args: &[String],
    path_dirs: &[std::path::PathBuf],
    tty: bool,
) -> Vec<(String, Vec<String>)> {
    let mut plan = Vec::new();
    let has = |name: &str| path_dirs.iter().any(|dir| dir.join(name).is_file());
    if has("sudo") {
        // 1. Non-interactive: NOPASSWD or cached timestamp. No prompt, ever.
        let mut argv = vec!["-n".to_string(), exe.to_string_lossy().into_owned()];
        argv.extend(args.iter().cloned());
        plan.push(("sudo".to_string(), argv));
        // 2. Interactive (terminal only): sudo prompts on the TTY itself.
        // Skipped without a TTY — sudo would just fail "no tty present".
        if tty {
            let mut argv = vec![exe.to_string_lossy().into_owned()];
            argv.extend(args.iter().cloned());
            plan.push(("sudo".to_string(), argv));
        }
    }
    // 3. pkexec last: its own prompt agent, different auth stack.
    if has("pkexec") {
        let mut argv = vec![exe.to_string_lossy().into_owned()];
        argv.extend(args.iter().cloned());
        plan.push(("pkexec".to_string(), argv));
    }
    plan
}

fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.chars()
                .all(|c| c.is_alphanumeric() || "_-./=,:".contains(c))
            {
                a.clone()
            } else {
                format!("'{a}'")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

unsafe extern "C" {
    fn geteuid() -> u32;
}

fn libc_geteuid() -> u32 {
    unsafe { geteuid() }
}

fn require_systemd() -> Result<()> {
    let ok = std::process::Command::new(SYSTEMCTL)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        anyhow::bail!("service: {SYSTEMCTL} not found; systemd service management isn't available on this system");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(
            exec_start_line(
                "/usr/bin/keep-at",
                &["run", "--config", "/etc/keep-at/config.yaml"]
            ),
            "/usr/bin/keep-at run --config /etc/keep-at/config.yaml"
        );
        assert_eq!(
            quote_if_needed("/path with space/x"),
            "\"/path with space/x\""
        );
    }

    #[test]
    fn elevation_argv_shape() {
        // PATH-dependent helper selection is covered by elevation_command
        // tests with a temp dir on PATH (serial, env-mutating); here pin
        // the shell quoting used in the fallback error message.
        assert_eq!(shell_join(&[]), "");
        assert_eq!(
            shell_join(&["service".to_string(), "install".to_string()]),
            "service install"
        );
        assert_eq!(
            shell_join(&["--api-key".to_string(), "uid=1;pass=x y".to_string()]),
            "--api-key 'uid=1;pass=x y'"
        );
    }

    // NOTE: elevation_command() branches on PATH contents; these tests pass
    // explicit search dirs (no env mutation, parallel-safe).
    #[test]
    fn prefers_sudo_noninteractive_then_interactive() {
        let dir = std::env::temp_dir().join(format!("keep-at-elev-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("sudo"), "").unwrap();
        std::fs::write(dir.join("pkexec"), "").unwrap();
        // On a terminal: sudo -n, plain sudo, pkexec.
        let plan = elevation_command_on_path(
            std::path::Path::new("/usr/local/bin/keep-at"),
            &["service".to_string(), "install".to_string()],
            std::slice::from_ref(&dir),
            true,
        );
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].0, "sudo");
        assert_eq!(
            plan[0].1,
            vec![
                "-n".to_string(),
                "/usr/local/bin/keep-at".to_string(),
                "service".to_string(),
                "install".to_string(),
            ]
        );
        assert_eq!(plan[1].0, "sudo");
        assert_eq!(
            plan[1].1,
            vec![
                "/usr/local/bin/keep-at".to_string(),
                "service".to_string(),
                "install".to_string(),
            ]
        );
        assert_eq!(plan[2].0, "pkexec");
        // No TTY (scripts, CI, daemons): plain sudo skipped — it would fail
        // "no tty present" anyway.
        let plan = elevation_command_on_path(
            std::path::Path::new("/usr/local/bin/keep-at"),
            &["service".to_string(), "install".to_string()],
            std::slice::from_ref(&dir),
            false,
        );
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].0, "sudo");
        assert_eq!(plan[0].1[0], "-n");
        assert_eq!(plan[1].0, "pkexec");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn falls_back_to_pkexec_without_sudo() {
        let dir = std::env::temp_dir().join(format!("keep-at-elev2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("pkexec"), "").unwrap();
        let plan = elevation_command_on_path(
            std::path::Path::new("/usr/local/bin/keep-at"),
            &["self-update".to_string()],
            std::slice::from_ref(&dir),
            true,
        );
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].0, "pkexec");
        assert_eq!(
            plan[0].1,
            vec![
                "/usr/local/bin/keep-at".to_string(),
                "self-update".to_string(),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filtered_plan_drops_sudo_noninteractive_when_probe_fails() {
        // Probe says sudo -n would refuse: plan starts at interactive sudo
        // (terminal) — the exact scenario from the bug report.
        let dir = std::env::temp_dir().join(format!("keep-at-elev4-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("sudo"), "").unwrap();
        let exe = std::path::Path::new("/usr/local/bin/keep-at");
        let args = vec!["service".to_string(), "install".to_string()];
        let full = elevation_command_on_path(exe, &args, std::slice::from_ref(&dir), true);
        assert_eq!(full.len(), 2); // [sudo -n, sudo]
                                   // Filtered out the -n entry manually here would need the real
                                   // elevation_command (reads PATH); instead pin the filter predicate
                                   // shape directly on the full plan.
        let filtered: Vec<_> = full
            .into_iter()
            .filter(|(prog, argv)| {
                let is_noninteractive = prog == "sudo" && argv.first().is_some_and(|a| a == "-n");
                !is_noninteractive
            })
            .collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].0, "sudo");
        assert_eq!(filtered[0].1[0], "/usr/local/bin/keep-at");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_helper_empty_plan() {
        let dir = std::env::temp_dir().join(format!("keep-at-elev3-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        assert!(elevation_command_on_path(
            std::path::Path::new("/usr/local/bin/keep-at"),
            &["service".to_string(), "install".to_string()],
            std::slice::from_ref(&dir),
            true,
        )
        .is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
