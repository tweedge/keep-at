//! Human-readable byte / duration formatting. Ported from
//! internal/engine/humanize.go and internal/netstats helpers (Go).

const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

/// Format a byte count like "500 GiB", "10.4 MiB".
pub fn human_bytes(n: i64) -> String {
    if n < 0 {
        return format!("-{}", human_bytes(-n));
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Format bits-per-second like "12.3 Mbit/s".
pub fn human_bits_per_sec(bps: f64) -> String {
    const UNITS: [&str; 5] = ["bit/s", "Kbit/s", "Mbit/s", "Gbit/s", "Tbit/s"];
    let mut v = bps;
    let mut u = 0;
    while v >= 1000.0 && u + 1 < UNITS.len() {
        v /= 1000.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

/// Compact duration like "4h19m", "90s", "3d2h".
pub fn human_duration(d: std::time::Duration) -> String {
    let mut secs = d.as_secs();
    if secs == 0 {
        return format!("{}ms", d.as_millis());
    }
    let mut out = String::new();
    let parts = [(86400, "d"), (3600, "h"), (60, "m"), (1, "s")];
    for (len, suffix) in parts {
        let q = secs / len;
        if q > 0 || !out.is_empty() {
            out.push_str(&format!("{q}{suffix}"));
        }
        secs %= len;
    }
    if out.is_empty() {
        out.push_str("0s");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn bytes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(10 * 1024 * 1024), "10.0 MiB");
    }

    #[test]
    fn durations() {
        assert_eq!(human_duration(Duration::from_secs(90)), "1m30s");
        assert_eq!(
            human_duration(Duration::from_secs(4 * 3600 + 19 * 60)),
            "4h19m0s"
        );
    }
}
