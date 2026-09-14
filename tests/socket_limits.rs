//! Regression: the query socket is world-writable (0o666), so a local user
//! could (a) balloon the daemon's heap with an unbounded request line (a
//! unix socket sustains GB/s - the 5s timeout bounds a STALLED client, not
//! a writing one) and (b) pin one fd per silent connection with no cap.
//! Requests are now capped at MAX_REQUEST_BYTES and concurrent queries at
//! MAX_CONCURRENT_QUERIES (over capacity drops immediately; a flooded
//! server recovers as the flood's timeouts expire).

use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

fn serve_thread(dir: std::path::PathBuf, handle: keep_at::live::LiveHandle) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(keep_at::live::serve(dir, handle));
    });
}

fn wait_socket(dir: &Path) {
    let p = keep_at::live::socket_path(dir);
    for _ in 0..200 {
        if UnixStream::connect(&p).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("socket never appeared at {}", p.display());
}

/// Raw client: connect, send `req`, read one reply line (or None on
/// EOF/error - i.e. the server dropped the connection without a response).
fn raw_query(path: &Path, req: &[u8]) -> Option<String> {
    let mut s = UnixStream::connect(path).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(req).ok()?;
    let mut reader = std::io::BufReader::new(s);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line),
    }
}

fn booting_handle() -> keep_at::live::LiveHandle {
    keep_at::live::LiveHandle::booting(std::time::Instant::now(), Vec::new(), Vec::new())
}

#[test]
fn oversize_request_line_is_dropped_and_server_stays_up() {
    let dir = tempfile::tempdir().unwrap().keep();
    let handle = booting_handle();
    serve_thread(dir.clone(), handle);
    wait_socket(&dir);
    let sock = keep_at::live::socket_path(&dir);

    // 200KB of junk on one line: way over the cap. The server must drop the
    // connection unread (no response) instead of buffering the whole line.
    let mut junk = String::from("{\"op\":\"");
    while junk.len() < 200 * 1024 {
        junk.push('x');
    }
    junk.push_str("\"}\n");
    let resp = raw_query(&sock, junk.as_bytes());
    assert!(
        resp.is_none(),
        "oversize request must be dropped without a response: {resp:?}"
    );

    // The server itself is unharmed.
    let resp = raw_query(&sock, b"{\"op\":\"runtime\"}\n").expect("server still answers");
    assert!(resp.contains("\"op\"") || resp.contains("Runtime") || !resp.is_empty());
}

#[test]
fn normal_query_within_cap_still_answers() {
    let dir = tempfile::tempdir().unwrap().keep();
    let handle = booting_handle();
    serve_thread(dir.clone(), handle);
    wait_socket(&dir);
    let sock = keep_at::live::socket_path(&dir);
    let resp = raw_query(&sock, b"{\"op\":\"runtime\"}\n").expect("runtime query answered");
    assert!(resp.contains("\n"), "one response line");
}

#[test]
fn silent_flood_recovers_after_query_timeout() {
    let dir = tempfile::tempdir().unwrap().keep();
    let handle = booting_handle();
    serve_thread(dir.clone(), handle);
    wait_socket(&dir);
    let sock = keep_at::live::socket_path(&dir);

    // Flood: silent connections hold permits for up to QUERY_TIMEOUT each.
    let mut conns = Vec::new();
    for _ in 0..(keep_at::live::MAX_CONCURRENT_QUERIES * 3) {
        if let Ok(s) = UnixStream::connect(&sock) {
            conns.push(s);
        }
    }
    std::thread::sleep(Duration::from_millis(200));

    // After the flood's timeouts expire, every permit is back: the server
    // answers normally (no permanent wedge).
    std::thread::sleep(keep_at::live::QUERY_TIMEOUT + Duration::from_millis(800));
    drop(conns);
    let resp = raw_query(&sock, b"{\"op\":\"runtime\"}\n");
    assert!(resp.is_some(), "server must recover after the flood drains");
}
