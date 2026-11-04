use std::{
    ptr::null,
    sync::atomic::{AtomicU64, Ordering},
};
use windows_sys::Win32::{Foundation::*, System::Memory::*};
fn name() -> String {
    format!(
        "Local\\StepThumbnailStats-{}",
        &blake3::hash(crate::data_dir().to_string_lossy().as_bytes()).to_hex()[..20]
    )
}
pub struct SharedStats {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}
unsafe impl Send for SharedStats {}
unsafe impl Sync for SharedStats {}
impl SharedStats {
    pub fn create() -> Option<Self> {
        unsafe {
            let h = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                null(),
                PAGE_READWRITE,
                0,
                8,
                crate::wide(name()).as_ptr(),
            );
            if h.is_null() {
                return None;
            }
            let view = MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, 8);
            if view.Value.is_null() {
                CloseHandle(h);
                return None;
            }
            Some(Self { handle: h, view })
        }
    }
    pub fn hits(&self) -> u64 {
        unsafe { (*(self.view.Value as *const AtomicU64)).load(Ordering::Relaxed) }
    }
}
impl Drop for SharedStats {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(self.view);
            CloseHandle(self.handle);
        }
    }
}
pub fn hit() {
    unsafe {
        let h = OpenFileMappingW(FILE_MAP_WRITE, 0, crate::wide(name()).as_ptr());
        if h.is_null() {
            return;
        }
        let view = MapViewOfFile(h, FILE_MAP_WRITE, 0, 0, 8);
        if !view.Value.is_null() {
            (*(view.Value as *const AtomicU64)).fetch_add(1, Ordering::Relaxed);
            UnmapViewOfFile(view);
        }
        CloseHandle(h);
    }
}
