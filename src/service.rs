//! systemd service install/uninstall. Linux-only.
//! Ported from internal/service (Go).

use std::path::Path;

use anyhow::{Context, Result};

use crate::config::Config;

pub const UNIT_PATH: &str = "/etc/systemd/system/keep-at.service";
pub const CONFIG_DIR: &str = "/etc/keep-at";
pub const CONFIG_PATH: &str = "/etc/keep-at/config.yaml";

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
    require_root()?;
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
    require_root()?;
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
    let out = std::process::Command::new("systemctl")
        .args(args)
        .output()
        .with_context(|| format!("running systemctl {args:?}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "systemctl {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

fn require_root() -> Result<()> {
    // SAFETY: geteuid has no side effects.
    let euid = libc_geteuid();
    if euid != 0 {
        anyhow::bail!("service: must be run as root (try sudo)");
    }
    Ok(())
}

unsafe extern "C" {
    fn geteuid() -> u32;
}

fn libc_geteuid() -> u32 {
    unsafe { geteuid() }
}

fn require_systemd() -> Result<()> {
    let ok = std::process::Command::new("systemctl")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        anyhow::bail!("service: systemctl not found; systemd service management isn't available on this system");
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
}
