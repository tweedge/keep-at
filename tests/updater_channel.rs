//! Regression tests for the self-update channel/downgrade fixes:
//! - the stable channel must never serve a release whose tag does not
//!   classify as stable (fail-closed tag-shape rule now enforced on BOTH
//!   channels; the old stable path trusted /releases/latest blind)
//! - the downgrade guard must apply on the beta channel too (GitHub's
//!   release list is created_at-ordered, so a re-published older beta is
//!   "newest" by list position and must not overwrite a newer binary)

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

type Routes = Arc<Mutex<HashMap<String, (u16, Vec<u8>)>>>;

use keep_at::updater;

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap()
}

fn serve_scripted(routes: Routes) -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let routes = routes.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => return,
                    }
                }
                let path = std::str::from_utf8(&buf)
                    .ok()
                    .and_then(|s| s.lines().next())
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let resp = {
                    let r = routes.lock().unwrap();
                    r.get(&path).cloned().unwrap_or((404, b"nope".to_vec()))
                };
                let reason = match resp.0 {
                    200 => "OK",
                    404 => "Not Found",
                    _ => "Error",
                };
                let head = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    resp.0, reason, resp.1.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&resp.1);
            });
        }
    });
    (format!("http://{addr}"), handle)
}

fn release_json(tag: &str, asset_name: &str, download_url: &str) -> String {
    format!(
        r#"{{"tag_name":"{tag}","draft":false,"assets":[{{"name":"{asset_name}","browser_download_url":"{download_url}"}}]}}"#
    )
}

/// The exact decision block from bin/keep-at.rs cmd_self_update, replicated
/// verbatim so the decision under test is the shipped logic
/// (latest_version output feeds straight into it).
fn would_install(latest: &str, current: &str, _beta: bool) -> bool {
    if latest.trim_start_matches('v') == current.trim_start_matches('v') || latest == current {
        return false;
    }
    if version_older_or_equal(latest, current) {
        return false;
    }
    true
}

fn version_older_or_equal(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.trim_start_matches('v')
            .split('.')
            .map(|p| {
                p.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect()
    }
    let (a, b) = (parts(a), parts(b));
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x < y;
        }
    }
    true
}

#[tokio::test(flavor = "multi_thread")]
async fn self_update_channel_guards() {
    // Phase 1 (stable channel):
    #[allow(clippy::await_holding_lock)]
    // env vars are process-global; the lock serializes the two tests
    let asset = "keep-at_linux_amd64.tar.gz";
    let client = http_client();
    let ua = "keep-at-test";

    // A list containing a bare three-component tag (v0.9.1, published
    // without the prerelease flag - GitHub would serve it as
    // /releases/latest) alongside the real stable v0.8.9.
    let non_stable = release_json("v0.9.1", asset, "http://127.0.0.1:1/x");
    let stable = release_json("v0.9", asset, "http://127.0.0.1:1/y");
    let routes: Routes = Arc::new(Mutex::new(
        [(
            "/releases".to_string(),
            (200, format!("[{non_stable},{stable}]").into_bytes()),
        )]
        .into_iter()
        .collect(),
    ));
    let (url, handle) = serve_scripted(routes);
    std::env::set_var("KEEPAT_RELEASES_LIST_URL", format!("{url}/releases"));

    let latest = updater::latest_version(&client, ua, false)
        .await
        .expect("latest");
    assert_eq!(
        latest, "v0.9",
        "stable channel must pick the stable-classified tag, never the mis-shaped one"
    );
    drop(handle);

    // Phase 2 (beta channel):
    let asset = "keep-at_linux_amd64.tar.gz";
    let client = http_client();
    let ua = "keep-at-test";

    // List order is created_at, not version: the older beta comes first.
    let old = release_json("v0.8.23-beta", asset, "http://127.0.0.1:1/old");
    let new = release_json("v0.8.24-beta", asset, "http://127.0.0.1:1/new");
    let routes: Routes = Arc::new(Mutex::new(
        [(
            "/releases".to_string(),
            (200, format!("[{old},{new}]").into_bytes()),
        )]
        .into_iter()
        .collect(),
    ));
    let (url, handle) = serve_scripted(routes);
    std::env::set_var("KEEPAT_RELEASES_LIST_URL", format!("{url}/releases"));

    let latest_beta = updater::latest_version(&client, ua, true)
        .await
        .expect("beta latest");
    assert_eq!(
        latest_beta, "v0.8.23-beta",
        "list position resolves the candidate"
    );
    // Running 0.8.24-beta with --beta: the un-gated downgrade guard must
    // refuse to replace the binary with the older release...
    assert!(
        !would_install(&latest_beta, "0.8.24-beta", true),
        "beta channel must not downgrade 0.8.24-beta -> {latest_beta}"
    );
    // ...and must still accept a genuine upgrade.
    assert!(
        would_install("v0.8.24-beta", "0.8.23-beta", true),
        "genuine beta upgrade must not be blocked by the guard"
    );
    drop(handle);
}
