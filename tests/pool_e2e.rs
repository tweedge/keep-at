//! Pooled-storage end-to-end: a ~10 GB multi-file torrent transferred
//! between two rqbit sessions in this process — seeder on stock storage,
//! leecher through the bounded file-handle pool — over a stub tracker.
//!
//! Pins the EMFILE fix at scale: the leecher's pool must stay bounded
//! (well under the file count), all files must hash-check, and the
//! transferred bytes must match exactly. Heavy: writes ~20 GB total
//! (seeder + leecher) to the system temp dir; run deliberately, not in CI.
//!
//! `KEEPAT_POOL_E2E=1 cargo test --test pool_e2e -- --nocapture`
//! (override size with KEEPAT_POOL_E2E_GB, default 10)

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use librqbit::storage::StorageFactoryExt;

const PIECE_LEN: usize = 4 * 1024 * 1024; // 4 MiB pieces
const FILE_COUNT: usize = 8;

fn run_tracker(seeder_addr: Arc<Mutex<Option<SocketAddr>>>) -> String {
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

/// Content of file i: its index byte repeated (deterministic, verifiable,
/// files differ from each other so cross-file confusion is detectable).
fn file_byte(i: usize) -> u8 {
    (i % 251) as u8
}

fn leecher_session_options(port: u16) -> librqbit::SessionOptions {
    librqbit::SessionOptions {
        listen: Some(librqbit::ListenerOptions {
            listen_addr: SocketAddr::from(([127, 0, 0, 1], port)),
            ..Default::default()
        }),
        dht: None,
        disable_local_service_discovery: true,
        default_storage_factory: Some(
            keep_at::engine::pool_storage::PooledStorageFactory::default().boxed(),
        ),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ten_gb_multi_file_transfer_through_pool() {
    if std::env::var("KEEPAT_POOL_E2E").as_deref() != Ok("1") {
        eprintln!("skipped: set KEEPAT_POOL_E2E=1 to run the 10GB pooled-storage e2e");
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let gb: u64 = std::env::var("KEEPAT_POOL_E2E_GB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    // Pin the pool below the file count so any eager-open regression
    // (stock rqbit behavior) fails the bounded assertion immediately.
    std::env::set_var("KEEPAT_FD_POOL_CAP", "4");

    let file_size: u64 = gb * (1 << 30) / FILE_COUNT as u64;
    let file_sizes: Vec<u64> = (0..FILE_COUNT).map(|_| file_size).collect();
    let total = file_size * FILE_COUNT as u64;

    let tmp = std::env::temp_dir().join(format!("keepat-pool-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    // Unwind-safe cleanup: any assert/unwrap failure removes the ~2x-dataset
    // scratch tree instead of leaking it (the pid suffix would otherwise
    // accumulate trees across failed runs).
    struct Cleanup<'a>(&'a std::path::Path);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0);
        }
    }
    let _cleanup = Cleanup(&tmp);
    let seed_root = tmp.join("seed");
    let leech_root = tmp.join("leech");
    std::fs::create_dir_all(&leech_root).unwrap();

    let seeder_addr: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
    let tracker_base = run_tracker(seeder_addr.clone());
    let tracker_url = format!("{tracker_base}/announce");

    eprintln!(
        "writing {:.1} GiB of seed content...",
        total as f64 / (1 << 30) as f64
    );
    let t0 = std::time::Instant::now();
    let raw = write_seed_and_torrent(&seed_root, &tracker_url, &file_sizes);
    let md = keep_at::attorrent::parse_torrent_bytes(&raw).expect("e2e torrent parses");
    let ih = md.info_hash;
    eprintln!("torrent built in {:?}", t0.elapsed());

    // Register the leech output dir with the pooled factory BEFORE the add.
    // Must be exactly the output_folder the leecher add passes (rqbit hides
    // output_folder from factories; the factory resolves via this registry).
    keep_at::engine::pool_storage::register_torrent(ih, leech_root.clone(), true);

    let base_opts = |port: u16| librqbit::SessionOptions {
        listen: Some(librqbit::ListenerOptions {
            listen_addr: SocketAddr::from(([127, 0, 0, 1], port)),
            ..Default::default()
        }),
        dht: None,
        disable_local_service_discovery: true,
        ..Default::default()
    };

    // Seeder: stock storage, hashes the pre-written files in place.
    let seeder = librqbit::Session::new_with_opts(tmp.join("ses-seed"), base_opts(0))
        .await
        .expect("seeder session");
    let resp = seeder
        .add_torrent(
            librqbit::AddTorrent::TorrentFileBytes(raw.clone().into()),
            Some(librqbit::AddTorrentOptions {
                output_folder: Some(seed_root.to_string_lossy().into_owned()),
                overwrite: true,
                ..Default::default()
            }),
        )
        .await
        .expect("seeder add");
    let sh = resp.into_handle().expect("seeder managed");
    let listen_port = seeder.listen_addr().expect("seeder listen addr").port();
    *seeder_addr.lock().unwrap() = Some(SocketAddr::from(([127, 0, 0, 1], listen_port)));
    tokio::time::timeout(std::time::Duration::from_secs(30 * 60), async {
        while !sh.stats().finished {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("seeder hash check within 30min");
    eprintln!("seeder verified {:.1} GiB", total as f64 / (1 << 30) as f64);

    // Leecher: pooled storage, downloads everything.
    let leecher =
        librqbit::Session::new_with_opts(tmp.join("ses-leech"), leecher_session_options(0))
            .await
            .expect("leecher session");
    let lh = leecher
        .add_torrent(
            librqbit::AddTorrent::TorrentFileBytes(raw.into()),
            Some(librqbit::AddTorrentOptions {
                output_folder: Some(leech_root.to_string_lossy().into_owned()),
                overwrite: true,
                ..Default::default()
            }),
        )
        .await
        .expect("leecher add")
        .into_handle()
        .expect("leecher managed");

    let pool = keep_at::engine::pool_storage::FilePool::global();
    let done = tokio::time::timeout(std::time::Duration::from_secs(30 * 60), async {
        loop {
            let (_, _, live, cap) = pool.stats();
            assert!(live <= cap, "pool exceeded its cap: {live} > {cap}");
            if lh.stats().finished {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    })
    .await;
    assert!(done.is_ok(), "leecher finished within 30min");
    eprintln!("leecher finished in {:?}", t0.elapsed());

    // Byte-for-byte verification of every file through the pool's output.
    for (i, size) in file_sizes.iter().enumerate() {
        let path = leech_root.join("pool-e2e").join(format!("file_{i}.bin"));
        let m = std::fs::metadata(&path).unwrap();
        assert_eq!(m.len(), *size, "file {i} length");
        let mut f = std::fs::File::open(&path).unwrap();
        let mut fi = 0u64;
        let mut buf = vec![0u8; 4 << 20];
        while fi < *size {
            let n = buf.len().min((*size - fi) as usize);
            use std::io::Read;
            f.read_exact(&mut buf[..n]).unwrap();
            assert!(
                buf[..n].iter().all(|&b| b == file_byte(i)),
                "file {i} content mismatch at offset {fi}"
            );
            fi += n as u64;
        }
    }
    eprintln!("all {FILE_COUNT} files verified byte-for-byte");

    let (opens, evictions, live, cap) = pool.stats();
    eprintln!("pool: {opens} opens, {evictions} evictions, {live} live handles, cap {cap}");
    assert!(live <= cap, "pool stayed bounded");
    assert!(evictions > 0, "cap below file count must force eviction");

    let _ = seeder.stop().await;
    let _ = leecher.stop().await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Write the seed content files and build a multi-file .torrent with REAL
/// piece hashes over the concatenated stream (pieces cross file
/// boundaries, like any real multi-file torrent).
fn write_seed_and_torrent(
    seed_root: &std::path::Path,
    tracker_url: &str,
    file_sizes: &[u64],
) -> Vec<u8> {
    use sha1::Digest;
    use std::io::Write;
    let torrent_name = "pool-e2e";
    std::fs::create_dir_all(seed_root.join(torrent_name)).unwrap();

    let mut info = Vec::new();
    info.push(b'd');
    info.extend_from_slice(b"5:filesl");
    for (i, size) in file_sizes.iter().enumerate() {
        info.push(b'd');
        info.extend_from_slice(&format!("6:lengthi{}e", size).into_bytes());
        let fname = format!("file_{i}.bin");
        // rqbit joins output_folder + raw path components (it does NOT
        // prepend the torrent name), so the name is the first component.
        info.extend_from_slice(
            &format!("4:pathl8:pool-e2e{}:{}e", fname.len(), fname).into_bytes(),
        );
        info.push(b'e');
    }
    info.push(b'e');
    info.extend_from_slice(&format!("4:name{}:{torrent_name}", torrent_name.len()).into_bytes());
    info.extend_from_slice(&format!("12:piece lengthi{}e", PIECE_LEN).into_bytes());

    let mut handles: Vec<(std::fs::File, u64, u64)> = file_sizes
        .iter()
        .enumerate()
        .map(|(i, size)| {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(seed_root.join(torrent_name).join(format!("file_{i}.bin")))
                .unwrap();
            (f, *size, 0u64)
        })
        .collect();

    let total: u64 = file_sizes.iter().sum();
    let mut pieces: Vec<u8> = Vec::new();
    let mut piece_buf = vec![0u8; PIECE_LEN];
    let mut pos = 0usize;
    let mut fi = 0usize;
    let mut copied: u64 = 0;
    while copied < total {
        let left_in_file = handles[fi].1 - handles[fi].2;
        let left_in_piece = PIECE_LEN - pos;
        let n = (left_in_file.min(left_in_piece as u64)) as usize;
        let byte = file_byte(fi);
        let chunk = vec![byte; n];
        handles[fi].0.write_all(&chunk).unwrap();
        piece_buf[pos..pos + n].copy_from_slice(&chunk);
        handles[fi].2 += n as u64;
        copied += n as u64;
        pos += n;
        if handles[fi].2 == handles[fi].1 {
            fi += 1;
        }
        if pos == PIECE_LEN || copied == total {
            let mut h = sha1::Sha1::new();
            h.update(&piece_buf[..pos]);
            pieces.extend_from_slice(&h.finalize());
            pos = 0;
        }
    }
    info.extend_from_slice(&format!("6:pieces{}:", pieces.len()).into_bytes());
    info.extend_from_slice(&pieces);
    info.push(b'e');

    let mut raw = vec![b'd'];
    raw.extend_from_slice(&format!("8:announce{}:", tracker_url.len()).into_bytes());
    raw.extend_from_slice(tracker_url.as_bytes());
    raw.extend_from_slice(b"4:info");
    raw.extend_from_slice(&info);
    raw.push(b'e');
    raw
}
