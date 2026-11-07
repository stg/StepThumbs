//! Opening an unavailable share can block in Windows. Keep it off the tray thread.
use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    sync::Arc,
    thread::JoinHandle,
};
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{Threading::*, WindowsProgramming::DRIVE_REMOTE},
};

pub struct Opened {
    pub file: OwnedHandle,
    pub remote: bool,
}
pub struct Attempt {
    pub root: usize,
    pub epoch: u64,
    event: Arc<OwnedHandle>,
    task: JoinHandle<io::Result<Opened>>,
}
impl Attempt {
    pub fn start(path: Vec<u16>, root: usize, epoch: u64) -> io::Result<Self> {
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let event = Arc::new(unsafe { OwnedHandle::from_raw_handle(event) });
        let done = event.clone();
        let task = std::thread::Builder::new()
            .name("watch reconnect".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                let result = unsafe {
                    let file = CreateFileW(
                        path.as_ptr(),
                        FILE_LIST_DIRECTORY,
                        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                        std::ptr::null(),
                        OPEN_EXISTING,
                        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                        std::ptr::null_mut(),
                    );
                    if file == INVALID_HANDLE_VALUE {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(Opened {
                            file: OwnedHandle::from_raw_handle(file),
                            remote: GetDriveTypeW(path.as_ptr()) == DRIVE_REMOTE,
                        })
                    }
                };
                unsafe {
                    SetEvent(done.as_raw_handle());
                }
                result
            })?;
        Ok(Self {
            root,
            epoch,
            event,
            task,
        })
    }
    pub fn event(&self) -> HANDLE {
        self.event.as_raw_handle()
    }
    pub fn finish(self) -> io::Result<Opened> {
        self.task
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("Folder watch opener panicked")))
    }
}
