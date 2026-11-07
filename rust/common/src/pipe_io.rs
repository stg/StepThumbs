//! Cancellable named-pipe I/O. Every transfer shares the caller's deadline.
use std::{
    io,
    mem::zeroed,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::{null, null_mut},
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{IO::*, Threading::*},
};

pub(crate) struct Pipe {
    handle: OwnedHandle,
    event: OwnedHandle,
}
impl Pipe {
    pub fn open(name: &[u16]) -> io::Result<Self> {
        unsafe {
            let handle = CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                null_mut(),
            );
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let handle = OwnedHandle::from_raw_handle(handle);
            let event = CreateEventW(null(), 1, 0, null());
            if event.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                handle,
                event: OwnedHandle::from_raw_handle(event),
            })
        }
    }
    fn transfer(
        &mut self,
        bytes: *mut u8,
        length: usize,
        read: bool,
        until: Instant,
    ) -> io::Result<usize> {
        let remaining = until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timed_out());
        }
        unsafe {
            let handle = self.handle.as_raw_handle();
            let event = self.event.as_raw_handle();
            ResetEvent(event);
            let mut io: OVERLAPPED = zeroed();
            io.hEvent = event;
            let length = length.min(u32::MAX as usize) as u32;
            let ok = if read {
                ReadFile(handle, bytes.cast(), length, null_mut(), &mut io)
            } else {
                WriteFile(handle, bytes.cast(), length, null_mut(), &mut io)
            };
            if ok == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                    return Err(error);
                }
                let wait = WaitForSingleObject(
                    event,
                    remaining.as_millis().clamp(1, u32::MAX as u128 - 1) as u32,
                );
                if wait != WAIT_OBJECT_0 {
                    let error = if wait == WAIT_TIMEOUT {
                        timed_out()
                    } else {
                        io::Error::last_os_error()
                    };
                    // OVERLAPPED and its buffer must remain alive until Windows
                    // acknowledges cancellation, including completion races.
                    CancelIoEx(handle, &io);
                    let mut transferred = 0;
                    GetOverlappedResult(handle, &io, &mut transferred, 1);
                    return Err(error);
                }
            }
            let mut transferred = 0;
            if GetOverlappedResult(handle, &io, &mut transferred, 0) == 0 {
                return Err(io::Error::last_os_error());
            }
            if transferred == 0 {
                return Err(io::Error::from(if read {
                    io::ErrorKind::UnexpectedEof
                } else {
                    io::ErrorKind::WriteZero
                }));
            }
            Ok(transferred as usize)
        }
    }
    pub fn read_exact(&mut self, mut bytes: &mut [u8], until: Instant) -> io::Result<()> {
        while !bytes.is_empty() {
            let count = self.transfer(bytes.as_mut_ptr(), bytes.len(), true, until)?;
            bytes = &mut bytes[count..];
        }
        Ok(())
    }
    pub fn write_all(&mut self, mut bytes: &[u8], until: Instant) -> io::Result<()> {
        while !bytes.is_empty() {
            let count = self.transfer(bytes.as_ptr().cast_mut(), bytes.len(), false, until)?;
            bytes = &bytes[count..];
        }
        Ok(())
    }
}
fn timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "StepThumbs did not respond before the IPC deadline",
    )
}
