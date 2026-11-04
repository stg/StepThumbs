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
