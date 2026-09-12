//! Filesystem helpers: directory sizes, device free/total, atomic writes.
//! Linux-only (statvfs via libc): the migration targets Linux exclusively.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::{Config, StorageLimit, ALL_LIMIT_FRACTION};

/// Sum of on-disk (allocated) bytes under dir, recursively. Symlinks not
/// followed. Allocated means the blocks the filesystem actually assigned
/// (`st_blocks` × 512) — for plain sparse files that is what the host's
/// quota charges, and it lags nominal size while pieces are still missing.
/// Use state nominal sums when eventual-footprint accounting is wanted
/// (committed storage); see `state::State::bytes_used`.
pub fn dir_size_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for e in entries.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(p),
                Ok(ft) if ft.is_file() => {
                    if let Ok(m) = e.metadata() {
                        // Apparent size (m.len()) counts sparse holes the
                        // device never allocated — on a partially-downloaded
                        // torrent that reported ~1.4 TiB of holes as "used"
                        // (4.18 TB apparent vs 2.78 TB allocated vs the
                        // host's quota view). blocks()*512 is what du -s and
                        // every quota meter report.
                        total = total.saturating_add(m.blocks().saturating_mul(512));
                    }
                }
                _ => {}
            }
        }
    }
    total
}

fn statvfs_of(path: &Path) -> Result<libc_statvfs_t> {
    std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| anyhow::anyhow!("non-UTF8 storage path"))?;
    let mut st: libc_statvfs_t = unsafe { std::mem::zeroed() };
    // SAFETY: statvfs writes a struct statvfs into st on success.
    let rc = libc_statvfs_fn(c.as_ptr(), &mut st);
    if rc != 0 {
        anyhow::bail!(
            "statvfs({}) failed: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(st)
}

// Minimal statvfs binding (avoids adding a libc dependency).
#[repr(C)]
struct libc_statvfs_t {
    f_bsize: u64,
    f_frsize: u64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_favail: u64,
    f_fsid: u64,
    f_flag: u64,
    f_namemax: u64,
    __pad: [u64; 6],
}

unsafe extern "C" {
    fn statvfs(path: *const std::ffi::c_char, buf: *mut libc_statvfs_t) -> i32;
}

fn libc_statvfs_fn(path: *const std::ffi::c_char, buf: *mut libc_statvfs_t) -> i32 {
    // SAFETY: delegates to libc statvfs with a valid path pointer and buffer.
    unsafe { statvfs(path, buf) }
}

/// Bytes available to an unprivileged user on the device holding path.
pub fn device_free_bytes(path: &Path) -> Result<u64> {
    let st = statvfs_of(path)?;
    Ok(st.f_bavail.saturating_mul(st.f_frsize))
}

/// Total formatted capacity of the device holding path.
pub fn device_total_bytes(path: &Path) -> Result<u64> {
    let st = statvfs_of(path)?;
    Ok(st.f_blocks.saturating_mul(st.f_frsize))
}

/// Resolve `limit: max` locations to concrete byte counts (safe fraction of
/// the device's total formatted capacity). The caller's original keeps "max".
pub fn resolve_all_limits(cfg: &Config) -> Result<Config> {
    let mut out = cfg.clone();
    for loc in &mut out.storage {
        if loc.limit == StorageLimit::All {
            let total = device_total_bytes(&loc.path).with_context(|| {
                format!(
                    "resolving `limit: max` for {}: cannot stat device",
                    loc.path.display()
                )
            })?;
            loc.limit = StorageLimit::Bytes((total as f64 * ALL_LIMIT_FRACTION) as u64);
            tracing::info!(
                "resolved `limit: max` for {} to {}",
                loc.path.display(),
                crate::humanize::human_bytes(loc.limit_bytes() as i64),
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{Seek as _, Write as _};

    /// dir_size_bytes must report ALLOCATED bytes, not apparent size: keep-at
    /// writes sparse (rqbit truncates files to full length and fills pieces
    /// as they download), and a partially-downloaded torrent's apparent size
    /// is its full nominal footprint. Reporting apparent size made `used`
    /// equal `committed` (both nominal), hiding quota consumption — observed
    /// on mercury: 4.18 TB apparent vs 2.78 TB allocated vs the host's quota
    /// view (3.90 TB − 1.12 TB free = 2.78 TB).
    #[test]
    fn dir_size_is_allocated_not_apparent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("storage");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sparse.bin");

        // One sparse file: length 1_000_000 bytes with only the final byte
        // written (everything before it is an unallocated hole).
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&file)
            .unwrap();
        f.set_len(1_000_000).unwrap();
        f.seek(std::io::SeekFrom::End(-1)).unwrap();
        f.write_all(&[0xAA]).unwrap();
        drop(f);

        let apparent = std::fs::metadata(&file).unwrap().len();
        assert_eq!(apparent, 1_000_000, "apparent size covers the hole");
        let on_disk = dir_size_bytes(&dir);
        assert!(
            on_disk < apparent / 100,
            "allocated ({on_disk}) must exclude the sparse hole (apparent {apparent})"
        );
        assert!(on_disk > 0, "the written byte's block must count");

        // A fully-written file: allocated >= nominal (block rounding up).
        let full = dir.join("full.bin");
        std::fs::write(&full, vec![0xBB; 100_000]).unwrap();
        let on_disk = dir_size_bytes(&dir);
        assert!(
            on_disk >= 100_000,
            "a fully-written file's allocation must cover its nominal size ({on_disk})"
        );
    }
}
