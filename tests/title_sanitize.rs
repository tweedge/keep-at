//! Regression: remote catalog titles (arbitrary bytes from any Academic
//! Torrents registrant) must never reach a terminal raw - ANSI escapes,
//! OSC sequences, and newlines in titles are terminal injection.

use keep_at::humanize::sanitize_title;
use std::path::Path;

fn malicious_title() -> String {
    // OSC title rewrite + screen clear + SGR + newline-injected fake row.
    "\u{1b}]0;pwned\u{7}\u{1b}[2J\u{1b}[31mROOT-PWN\nFAKE-LINE injected by remote title\u{1b}[0m"
        .to_string()
}

#[test]
fn sanitize_title_strips_controls_keeps_text() {
    let clean = sanitize_title(&malicious_title());
    assert!(
        !clean.contains('\u{1b}'),
        "ANSI escapes stripped: {clean:?}"
    );
    assert!(!clean.contains('\n'), "newlines stripped: {clean:?}");
    assert!(!clean.contains('\u{7}'), "bell stripped: {clean:?}");
    assert!(clean.contains("ROOT-PWN"), "visible text survives");
    assert!(clean.contains("FAKE-LINE injected by remote title"));
    // Ordinary titles pass through untouched.
    assert_eq!(
        sanitize_title("A perfectly normal title (2024)"),
        "A perfectly normal title (2024)"
    );
    // Non-ASCII text is not control data and survives.
    assert_eq!(sanitize_title("Dataset – émigré"), "Dataset – émigré");
}

#[test]
fn catalog_ingest_sanitizes_titles() {
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel>
<item><title>{}</title><category>x</category><infohash>0123456789abcdef0123456789abcdef01234567</infohash><guid>g</guid><link>l</link><description>d</description><size>1000</size></item>
</channel></rss>"#,
        "&#x1b;]0;pwned&#x7;&#x1b;[2Jbad&#10;title"
    );
    let cat = keep_at::atcatalog::parse(xml.as_bytes()).expect("parse");
    assert_eq!(cat.items.len(), 1);
    let title = &cat.items[0].title;
    assert!(
        !title.contains('\u{1b}'),
        "escapes stripped at ingest: {title:?}"
    );
    assert!(
        !title.contains('\n'),
        "newlines stripped at ingest: {title:?}"
    );
}

#[test]
fn history_render_strips_remote_title() {
    let ev = keep_at::history::add_event(
        "0123456789abcdef0123456789abcdef01234567",
        &malicious_title(),
        1,
        1,
        Path::new("/mnt/d"),
        keep_at::history::Cause::Fill,
        1.0,
        0.5,
        1,
        "seed-scarcity roll succeeded",
        Vec::new(),
    );
    let lines = keep_at::history::render(&ev, false);
    let joined = lines.join("\n");
    assert!(
        !joined.contains('\u{1b}'),
        "render must not emit raw escapes: {joined:?}"
    );
    assert!(
        lines.iter().all(|l| l.lines().count() == 1),
        "render must not inject extra terminal lines: {lines:?}"
    );
}
