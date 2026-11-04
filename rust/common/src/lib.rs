pub mod cache;
pub mod config;
pub mod hooks;
pub mod ipc;
pub use config::Config;
pub fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    value.as_ref().encode_wide().chain(Some(0)).collect()
}
pub fn data_dir() -> std::path::PathBuf {
    std::env::var_os("STEPTHUMBNAIL_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap())
                .join("StepThumbnail")
        })
}
pub fn path_key(path: &std::path::Path) -> String {
    path.to_string_lossy().trim_end_matches('\\').to_lowercase()
}
pub fn below(path: &std::path::Path, root: &std::path::Path) -> bool {
    let p = path_key(path);
    let r = path_key(root);
    p == r || p.starts_with(&(r + "\\"))
}
pub mod shared_stats;
