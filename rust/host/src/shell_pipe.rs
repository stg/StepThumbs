//! The public shell endpoint stays available while the engine sleeps. The UI
//! thread accepts overlapped requests; only active requests get a relay thread.
use std::{
    mem::zeroed,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::{null, null_mut},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

// Reserve the listening instance and one transient hand-off slot out of 32.
const MAX_RELAYS: usize = 30;
struct Relays {
    active: AtomicUsize,
    changed: OwnedHandle,
}
struct Relay {
    pipe: Option<OwnedHandle>,
    state: Arc<Relays>,
}
impl Drop for Relay {
    fn drop(&mut self) {
        // Release the pipe instance before announcing that its slot is free.
        drop(self.pipe.take());
        self.state.active.fetch_sub(1, Ordering::AcqRel);
        unsafe {
            SetEvent(self.state.changed.as_raw_handle());
        }
    }
}
use stepthumbnail_common as common;
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{IO::*, Pipes::*, SystemInformation::GetTickCount64, Threading::*},
};

pub struct Listener {
    pipe: HANDLE,
    pub event: HANDLE,
    io: OVERLAPPED,
    phase: u8,
    pending: bool,
    header: [u8; 4],
    body: Vec<u8>,
    filled: usize,
    pub deadline: u64,
    relays: Arc<Relays>,
}
impl Listener {
    pub unsafe fn new() -> anyhow::Result<Box<Self>> {
        let changed = unsafe { CreateEventW(null(), 1, 0, null()) };
        anyhow::ensure!(!changed.is_null(), "Cannot create shell capacity event");
        let mut listener = Box::new(Self {
            pipe: INVALID_HANDLE_VALUE,
            event: unsafe { CreateEventW(null(), 1, 0, null()) },
            io: unsafe { zeroed() },
            phase: 0,
            pending: false,
            header: [0; 4],
            body: Vec::new(),
            filled: 0,
            deadline: 0,
            relays: Arc::new(Relays {
                active: AtomicUsize::new(0),
                changed: unsafe { OwnedHandle::from_raw_handle(changed) },
            }),
        });
        anyhow::ensure!(!listener.event.is_null(), "Cannot create shell pipe event");
        unsafe {
            listener.connect()?;
        }
        Ok(listener)
    }
    unsafe fn close_pipe(&mut self) {
        unsafe {
            if self.pipe != INVALID_HANDLE_VALUE {
                if self.pending {
                    CancelIoEx(self.pipe, &self.io);
                    let mut n = 0;
                    GetOverlappedResult(self.pipe, &self.io, &mut n, 1);
                }
                CloseHandle(self.pipe);
                self.pipe = INVALID_HANDLE_VALUE;
            }
            self.pending = false;
        }
    }
    unsafe fn reset_io(&mut self) {
        unsafe {
            ResetEvent(self.event);
            self.io = zeroed();
            self.io.hEvent = self.event;
        }
    }
    unsafe fn connect(&mut self) -> anyhow::Result<()> {
        unsafe {
            self.close_pipe();
            self.reset_io();
            self.body = Vec::new();
            self.filled = 0;
            self.phase = 0;
            self.deadline = 0;
            self.pipe = common::ipc::create_pipe(&common::ipc::pipe_name(), true, 4096)?;
            let ok = ConnectNamedPipe(self.pipe, &mut self.io);
            let error = if ok == 0 { GetLastError() } else { 0 };
            self.pending = error == ERROR_IO_PENDING;
            if ok != 0 || error == ERROR_PIPE_CONNECTED {
                SetEvent(self.event);
            } else {
                anyhow::ensure!(self.pending, "Cannot listen on shell pipe: {error}");
            }
            Ok(())
        }
    }
    unsafe fn read(&mut self) -> anyhow::Result<()> {
        unsafe {
            self.reset_io();
            let (buffer, length) = if self.phase == 1 {
                (self.header.as_mut_ptr(), 4)
            } else {
                (self.body.as_mut_ptr(), self.body.len())
            };
            let ok = ReadFile(
                self.pipe,
                buffer.add(self.filled).cast(),
                (length - self.filled) as u32,
                null_mut(),
                &mut self.io,
            );
            let error = if ok == 0 { GetLastError() } else { 0 };
            self.pending = error == ERROR_IO_PENDING;
            if ok == 0 && !self.pending {
                self.connect()?;
            }
            Ok(())
        }
    }
    pub unsafe fn event(&mut self) -> anyhow::Result<()> {
        unsafe {
            if self.phase == 0 {
                self.pending = false;
                self.phase = 1;
                self.deadline = GetTickCount64() + 3000;
                return self.read();
            }
            let mut n = 0;
            let ok = GetOverlappedResult(self.pipe, &self.io, &mut n, 0);
            self.pending = false;
            if ok == 0 || n == 0 {
                return self.connect();
            }
            self.filled += n as usize;
            if self.phase == 1 {
                if self.filled < 4 {
                    return self.read();
                }
                let length = u32::from_le_bytes(self.header) as usize;
                if length == 0 || length > 65536 {
                    return self.connect();
                }
                self.body = vec![0; length];
                self.filled = 0;
                self.phase = 2;
                return self.read();
            }
            if self.filled < self.body.len() {
                return self.read();
            }
            // Hold this complete request while all relay slots are occupied.
            // Further callers wait for a pipe instance, within their deadline.
            self.phase = 3;
            self.deadline = 0;
            self.reset_io();
            self.dispatch()
        }
    }
    pub fn capacity_event(&self) -> HANDLE {
        self.relays.changed.as_raw_handle()
    }
    #[cfg(test)]
    #[allow(dead_code)] // Only the isolated integration test uses this probe.
    pub fn waiting_for_capacity(&self) -> bool {
        self.phase == 3 && self.relays.active.load(Ordering::Acquire) >= MAX_RELAYS
    }
    pub unsafe fn capacity_changed(&mut self) -> anyhow::Result<()> {
        unsafe {
            ResetEvent(self.capacity_event());
            if self.phase == 3 {
                self.dispatch()?;
            }
            Ok(())
        }
    }
    unsafe fn dispatch(&mut self) -> anyhow::Result<()> {
        unsafe {
            if self.relays.active.load(Ordering::Acquire) >= MAX_RELAYS {
                return Ok(());
            }
            self.relays.active.fetch_add(1, Ordering::AcqRel);
            let relay = Relay {
                pipe: Some(OwnedHandle::from_raw_handle(self.pipe)),
                state: self.relays.clone(),
            };
            self.pipe = INVALID_HANDLE_VALUE;
            let body = std::mem::take(&mut self.body);
            self.connect()?;
            std::thread::Builder::new()
                .name("shell request".into())
                .stack_size(128 * 1024)
                .spawn(move || {
                    let request = serde_json::from_slice::<common::ipc::Request>(&body);
                    let quitting = matches!(&request, Ok(common::ipc::Request::Shutdown));
                    let response = match request {
                        Ok(common::ipc::Request::Thumbnail { path }) => {
                            common::ipc::call_engine(&common::ipc::Request::Thumbnail { path })
                        }
                        Ok(common::ipc::Request::Status) => {
                            common::ipc::call_engine(&common::ipc::Request::Status)
                        }
                        Ok(common::ipc::Request::Shutdown) => Ok(common::ipc::Response::ok()),
                        Ok(request) => common::ipc::call(&request, None),
                        Err(e) => Ok(common::ipc::Response::error(e)),
                    }
                    .unwrap_or_else(common::ipc::Response::error);
                    let _ = write(relay.pipe.as_ref().unwrap().as_raw_handle(), &response);
                    if quitting {
                        let _ = common::host::request(common::host::QUIT, 0);
                    }
                    drop(relay);
                })?;
            Ok(())
        }
    }
    pub unsafe fn expire(&mut self) -> anyhow::Result<()> {
        unsafe {
            if self.deadline != 0 && GetTickCount64() >= self.deadline {
                self.connect()?;
            }
            Ok(())
        }
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        unsafe {
            self.close_pipe();
            CloseHandle(self.event);
        }
    }
}

fn write(handle: HANDLE, response: &common::ipc::Response) -> anyhow::Result<()> {
    unsafe {
        let data = serde_json::to_vec(response)?;
        anyhow::ensure!(data.len() <= 65536, "Response exceeds shell message limit");
        let mut bytes = (data.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&data);
        let event = CreateEventW(null(), 1, 0, null());
        anyhow::ensure!(!event.is_null(), "Cannot create reply event");
        let mut offset = 0;
        while offset < bytes.len() {
            ResetEvent(event);
            let mut io: OVERLAPPED = zeroed();
            io.hEvent = event;
            let ok = WriteFile(
                handle,
                bytes.as_ptr().add(offset).cast(),
                (bytes.len() - offset) as u32,
                null_mut(),
                &mut io,
            );
            let error = if ok == 0 { GetLastError() } else { 0 };
            if ok == 0 && error != ERROR_IO_PENDING {
                break;
            }
            let mut n = 0;
            if error == ERROR_IO_PENDING && WaitForSingleObject(event, 5000) != WAIT_OBJECT_0 {
                CancelIoEx(handle, &io);
                GetOverlappedResult(handle, &io, &mut n, 1);
                break;
            }
            if GetOverlappedResult(handle, &io, &mut n, 0) == 0 || n == 0 {
                break;
            }
            offset += n as usize;
        }
        CloseHandle(event);
        anyhow::ensure!(offset == bytes.len(), "Shell reply did not finish");
        Ok(())
    }
}
