//! Local two-session byte transfer: one rqbit session seeds known bytes,
//! another downloads them over a stub tracker that answers announces with
//! compact peers. Asserts the transferred bytes match.
//!
//! This de-risks rqbit itself (the riskiest dependency) at exactly one cut
//! point — raw session byte movement — kept separate from Engine acting
//! tests so each suite asserts one thing.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use common::{torrent_bytes, torrent_info_hash, Fixture};

fn run_tracker(
    seeder_addr: Arc<Mutex<Option<SocketAddr>>>,
    announces: Arc<Mutex<usize>>,
) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind tracker");
    let addr = listener.local_addr().expect("tracker addr");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            use std::io::{Read, Write};
            let mut s = stream;
            let mut buf = vec![0u8; 65536];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();
            let payload: Vec<u8> = if path.starts_with("/announce") {
                *announces.lock().unwrap() += 1;
                // Compact IPv4 peers: 6 bytes each from the seeder addr.
                let mut peers = Vec::new();
                if let Some(SocketAddr::V4(v4)) = *seeder_addr.lock().unwrap() {
                    peers.extend_from_slice(&v4.ip().octets());
                    peers.extend_from_slice(&v4.port().to_be_bytes());
                }
                let mut body = format!("d8:intervali1800e5:peers{}:", peers.len()).into_bytes();
                body.extend_from_slice(&peers);
                body.push(b'e');
                body
            } else {
                b"d5:filesdee".to_vec()
            };
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = s.write_all(header.as_bytes());
            let _ = s.write_all(&payload);
        }
    });
    format!("http://{addr}")
}

/// Build `.torrent` bytes like the shared generator, but with REAL piece
/// hashes of `content` so a session holding those files counts as a seed.
/// (The shared generator emits zero hashes: fine for metadata-only Engine
/// tests, useless for byte transfer.)
fn torrent_bytes_with_pieces(fx: &Fixture, tracker_url: &str, content: &[u8]) -> Vec<u8> {
    use sha1::Digest;
    let piece_len = fx.piece_len.max(1) as usize;
    let mut pieces = Vec::new();
    for chunk in content.chunks(piece_len) {
        let mut h = sha1::Sha1::new();
        h.update(chunk);
        pieces.extend_from_slice(&h.finalize());
    }
    // Same layout as common::torrent_bytes, with real hashes spliced in.
    let safe_title: String = fx
        .title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("{safe_title}.bin");
    let mut info = vec![b'd'];
    let mut field = |k: &[u8], raw_value: Vec<u8>| {
        info.extend_from_slice(format!("{}:", k.len()).as_bytes());
        info.extend_from_slice(k);
        info.extend_from_slice(&raw_value);
    };
    // Values here are already bencoded forms (int / prefixed string).
    field(b"length", format!("i{}e", fx.size).into_bytes());
    field(b"name", {
        let mut v = format!("{}:", name.len()).into_bytes();
        v.extend_from_slice(name.as_bytes());
        v
    });
    field(
        b"piece length",
        format!("i{}e", fx.piece_len.max(1)).into_bytes(),
    );
    field(b"pieces", {
        let mut v = format!("{}:", pieces.len()).into_bytes();
        v.extend_from_slice(&pieces);
        v
    });
    info.push(b'e');
    let mut real = vec![b'd'];
    let mut rf = |k: &[u8], s: &[u8]| {
        real.extend_from_slice(format!("{}:", k.len()).as_bytes());
        real.extend_from_slice(k);
        real.extend_from_slice(format!("{}:", s.len()).as_bytes());
        real.extend_from_slice(s);
    };
    rf(b"announce", tracker_url.as_bytes());
    real.extend_from_slice(b"4:info");
    real.extend_from_slice(&info);
    real.push(b'e');
    real
}

fn session_options(port: u16) -> librqbit::SessionOptions {
    librqbit::SessionOptions {
        listen: Some(librqbit::ListenerOptions {
            listen_addr: SocketAddr::from(([127, 0, 0, 1], port)),
            ..Default::default()
        }),
        dht: None,
        disable_local_service_discovery: true,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_session_byte_transfer() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    // 4 pieces of 16 KiB: small enough to be fast, multi-piece enough to
    // exercise the piece path (not just a single-chunk degenerate).
    let fx = Fixture {
        title: "transfer".to_string(),
        size: 64 * 1024,
        piece_len: 16 * 1024,
        seeders: 0,
        leechers: 0,
        age: None,
    };
    let seeder_addr: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
    let announces: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let tracker_base = run_tracker(seeder_addr.clone(), announces.clone());
    let tracker_url = format!("{tracker_base}/announce");
    let raw = torrent_bytes(&fx, &tracker_url);
    let hex = torrent_info_hash(&raw);

    let base = tempfile::tempdir().unwrap().keep();
    let seed_dir = base.join("seed");
    let leech_dir = base.join("leech");
    std::fs::create_dir_all(&seed_dir).unwrap();
    std::fs::create_dir_all(&leech_dir).unwrap();
    // Seeder content: deterministic bytes (not zeros — zeros could mask a
    // broken transfer that yields a sparse file).
    let content: Vec<u8> = (0..fx.size).map(|i| (i % 251) as u8).collect();
    std::fs::write(seed_dir.join("transfer.bin"), &content).unwrap();

    // Rebuild the .torrent with REAL piece hashes of the content (the shared
    // fixture generator emits zero hashes, fine for metadata-only Engine
    // tests, but a seeder must hash-match its files to count as complete).
    let raw = torrent_bytes_with_pieces(&fx, &tracker_url, &content);
    let _hex = torrent_info_hash(&raw);

    // Seeder session: add with overwrite=true so it hashes existing files
    // in place (overwrite=false means "create, fail if present" here).
    let seeder = librqbit::Session::new_with_opts(base.join("ses-seed"), session_options(0))
        .await
        .expect("seeder session");
    let resp = seeder
        .add_torrent(
            librqbit::AddTorrent::TorrentFileBytes(raw.clone().into()),
            Some(librqbit::AddTorrentOptions {
                output_folder: Some(seed_dir.to_string_lossy().into_owned()),
                overwrite: true,
                ..Default::default()
            }),
        )
        .await
        .expect("seeder add");
    let handle = resp.into_handle().expect("seeder managed");
    // Publish the seeder's listen addr to the tracker stub.
    let listen_port = seeder
        .listen_addr()
        .map(|a| a.port())
        .unwrap_or_else(|| panic!("seeder listen addr"));
    *seeder_addr.lock().unwrap() = Some(SocketAddr::from(([127, 0, 0, 1], listen_port)));
    // Wait for the initial hash check to finish (async): have == total.
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if handle.stats().finished {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("seeder hashes existing files within 60s");
    assert!(
        handle.stats().finished,
        "seeder has the full content after hashing existing files"
    );

    // Leecher session: empty dir, same torrent, tracker points at seeder.
    let leecher = librqbit::Session::new_with_opts(base.join("ses-leech"), session_options(0))
        .await
        .expect("leecher session");
    let resp = leecher
        .add_torrent(
            librqbit::AddTorrent::TorrentFileBytes(raw.into()),
            Some(librqbit::AddTorrentOptions {
                output_folder: Some(leech_dir.to_string_lossy().into_owned()),
                overwrite: true,
                ..Default::default()
            }),
        )
        .await
        .expect("leecher add");
    let lh = resp.into_handle().expect("leecher managed");

    // Wait for completion (generous: local loopback, tiny torrent).
    let done = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            if lh.stats().finished {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    })
    .await;
    assert!(done.is_ok(), "leecher finished within 120s (hex {hex})");

    let got = std::fs::read(leech_dir.join("transfer.bin")).expect("leecher file");
    assert_eq!(got.len(), content.len(), "same length");
    assert_eq!(got, content, "transferred bytes match exactly");
    assert!(
        *announces.lock().unwrap() > 0,
        "tracker served at least one announce"
    );

    let _ = seeder.stop().await;
    let _ = leecher.stop().await;
}
