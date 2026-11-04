use anyhow::Result;
use std::{
    fs,
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub fn directory() -> PathBuf {
    crate::data_dir().join("cache-v1")
}
pub fn identity(path: &Path, config: &crate::Config) -> Result<String> {
    let metadata = fs::metadata(path)?;
    let key = format!(
        "{}|{}|{}|{}",
        crate::path_key(path),
        metadata.file_size(),
        metadata.last_write_time(),
        config.profile()
    );
    Ok(blake3::hash(key.as_bytes()).to_hex().to_string())
}
pub fn image_path(path: &Path, config: &crate::Config) -> Result<PathBuf> {
    Ok(directory().join(identity(path, config)? + ".png"))
}
pub fn find(path: &Path, config: &crate::Config) -> Option<PathBuf> {
    let image = image_path(path, config).ok()?;
    fs::metadata(&image)
        .ok()
        .filter(|m| m.len() > 0)
        .map(|_| image)
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temporary = path.with_extension(format!(
        "{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    fs::write(&temporary, bytes)?;
    unsafe {
        if windows_sys::Win32::Storage::FileSystem::MoveFileExW(
            crate::wide(&temporary).as_ptr(),
            crate::wide(path).as_ptr(),
            0x1 | 0x8,
        ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}
fn own_png(path: &Path) -> bool {
    path.file_name()
        .and_then(|v| v.to_str())
        .is_some_and(|name| {
            name.len() == 68
                && name.ends_with(".png")
                && name[..64].bytes().all(|b| b.is_ascii_hexdigit())
        })
}
pub fn inventory() -> (u64, u64) {
    let mut count = 0;
    let mut bytes = 0;
    if let Ok(entries) = fs::read_dir(directory()) {
        for f in entries.flatten() {
            if own_png(&f.path()) {
                if let Ok(m) = f.metadata() {
                    count += 1;
                    bytes += m.len()
                }
            }
        }
    }
    (count, bytes)
}
pub fn prune(limit_mb: u64) {
    if limit_mb == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(directory()) else {
        return;
    };
    let mut files = entries
        .flatten()
        .filter_map(|f| {
            if !own_png(&f.path()) {
                return None;
            }
            let m = f.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), f.path()))
        })
        .collect::<Vec<_>>();
    let mut bytes = files.iter().map(|x| x.1).sum::<u64>();
    files.sort_by_key(|x| x.0);
    for (_, length, path) in files {
        if bytes <= limit_mb * 1024 * 1024 {
            break;
        }
        if fs::remove_file(path).is_ok() {
            bytes -= length
        }
    }
}

/// Tracks additions between prunes, so preflight does not enumerate the cache for every model.
pub fn record_and_prune(limit_mb: u64, added: u64) {
    use std::sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    };
    static ESTIMATE: OnceLock<AtomicU64> = OnceLock::new();
    static PRUNE: Mutex<()> = Mutex::new(());
    if limit_mb == 0 {
        return;
    }
    let value = if let Some(v) = ESTIMATE.get() {
        v.fetch_add(added, Ordering::Relaxed).saturating_add(added)
    } else {
        let measured = inventory().1;
        if ESTIMATE.set(AtomicU64::new(measured)).is_ok() {
            measured
        } else {
            ESTIMATE
                .get()
                .unwrap()
                .fetch_add(added, Ordering::Relaxed)
                .saturating_add(added)
        }
    };
    let limit = limit_mb.saturating_mul(1024 * 1024);
    if value > limit {
        let _guard = PRUNE.lock().unwrap();
        if ESTIMATE.get().unwrap().load(Ordering::Relaxed) > limit {
            prune(limit_mb);
            ESTIMATE
                .get()
                .unwrap()
                .store(inventory().1, Ordering::Relaxed);
        }
    }
}
