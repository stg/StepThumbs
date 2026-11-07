use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    os::windows::io::FromRawHandle,
    path::Path,
    time::{Duration, Instant},
};
use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*, System::Pipes::*};

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    Thumbnail { path: String },
    Show,
    Shutdown,
    Status,
    Pause { paused: bool },
}
#[derive(Serialize, Deserialize, Debug)]
pub struct Response {
    pub image: Option<String>,
    pub error: Option<String>,
    pub status: Option<serde_json::Value>,
}
impl Response {
    pub fn image(path: &Path) -> Self {
        Self {
            image: Some(path.to_string_lossy().into()),
            error: None,
            status: None,
        }
    }
    pub fn error(e: impl ToString) -> Self {
        Self {
            image: None,
            error: Some(crate::brief_error(&e.to_string())),
            status: None,
        }
    }
    pub fn ok() -> Self {
        Self {
            image: None,
            error: None,
            status: None,
        }
    }
    pub fn status(status: serde_json::Value) -> Self {
        let mut response = Self {
            image: None,
            error: None,
            status: Some(status),
        };
        while serde_json::to_vec(&response).map_or(true, |v| v.len() > 60_000) {
            let status = response.status.as_mut().unwrap();
            status["details_truncated"] = true.into();
            if ["recent", "current_files"].iter().any(|field| {
                status[*field]
                    .as_array_mut()
                    .is_some_and(|items| items.pop().is_some())
            }) {
                continue;
            }
            status["current"] = serde_json::Value::Null;
            status["scanning"] = serde_json::Value::Null;
            if serde_json::to_vec(&response).map_or(true, |v| v.len() > 60_000) {
                return Self::error("Status details exceed the message limit");
            }
        }
        response
    }
}
pub fn pipe_name() -> String {
    format!(
        r"\\.\pipe\StepThumbnail-{}",
        &blake3::hash(crate::data_dir().to_string_lossy().as_bytes()).to_hex()[..20]
    )
}
pub fn engine_pipe_name() -> String {
    format!("{}.Engine", pipe_name())
}
pub fn write<T: Serialize>(file: &mut File, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > 65536 {
        bail!("IPC message too large")
    };
    file.write_all(&(bytes.len() as u32).to_le_bytes())?;
    file.write_all(&bytes)?;
    file.flush()?;
    Ok(())
}
pub fn read<T: serde::de::DeserializeOwned>(file: &mut File) -> Result<T> {
    let mut length = [0; 4];
    file.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > 65536 {
        bail!("IPC message too large")
    };
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}
pub fn exists() -> bool {
    exists_at(&pipe_name())
}
fn exists_at(name: &str) -> bool {
    unsafe {
        WaitNamedPipeW(crate::wide(name).as_ptr(), 500) != 0 || GetLastError() == ERROR_SEM_TIMEOUT
    }
}
pub fn call(request: &Request, exe: Option<&Path>) -> Result<Response> {
    call_at(request, exe, &pipe_name())
}
pub fn call_engine(request: &Request) -> Result<Response> {
    call_at(request, None, &engine_pipe_name())
}
fn call_at(request: &Request, exe: Option<&Path>, name: &str) -> Result<Response> {
    let until = Instant::now() + Duration::from_secs(120);
    match request {
        Request::Status if !exists_at(name) => {
            if let Some(status) = crate::host::idle_status_until(until) {
                return Ok(Response::status(status));
            }
        }
        Request::Show => {
            if crate::host::ensure_until(crate::host::SHOW, exe, until).is_ok() {
                return Ok(Response::ok());
            }
        }
        Request::Pause { paused } => {
            if let Ok((h, _)) = crate::host::request_until(
                crate::host::STATUS,
                0,
                until.min(Instant::now() + Duration::from_secs(4)),
            ) {
                if (*paused) == (h[4] & 1 != 0) {
                    crate::host::request_until(
                        crate::host::TOGGLE,
                        0,
                        until.min(Instant::now() + Duration::from_secs(4)),
                    )?;
                }
                return Ok(Response::ok());
            }
        }
        Request::Shutdown => {
            if crate::host::request_until(
                crate::host::QUIT,
                0,
                until.min(Instant::now() + Duration::from_secs(4)),
            )
            .is_ok()
            {
                return Ok(Response::ok());
            }
        }
        _ => {}
    }
    let name = crate::wide(name);
    let mut wake_sent = false;
    loop {
        if Instant::now() >= until {
            bail!("StepThumbs did not respond");
        }
        let mut pipe = match crate::pipe_io::Pipe::open(&name) {
            Ok(pipe) => pipe,
            Err(error) => {
                if matches!(request, Request::Status) {
                    if let Some(status) = crate::host::idle_status_until(until) {
                        return Ok(Response::status(status));
                    }
                }
                let code = error.raw_os_error().unwrap_or_default() as u32;
                if !matches!(
                    code,
                    ERROR_PIPE_BUSY | ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND
                ) {
                    return Err(error.into());
                }
                if Instant::now() >= until {
                    bail!("StepThumbs did not respond");
                }
                if code != ERROR_PIPE_BUSY && !wake_sent {
                    crate::host::ensure_until(crate::host::WAKE, exe, until)?;
                    wake_sent = true;
                }
                unsafe {
                    WaitNamedPipeW(
                        name.as_ptr(),
                        until
                            .saturating_duration_since(Instant::now())
                            .as_millis()
                            .clamp(1, 100) as u32,
                    );
                }
                std::thread::sleep(
                    Duration::from_millis(10).min(until.saturating_duration_since(Instant::now())),
                );
                continue;
            }
        };
        let response = exchange(&mut pipe, request, until);
        match response {
            Ok(v) if v.error.as_deref() != Some("Engine is restarting") => return Ok(v),
            Ok(_) => {
                if matches!(request, Request::Status) {
                    if let Some(status) = crate::host::idle_status_until(until) {
                        return Ok(Response::status(status));
                    }
                }
            }
            Err(e) => {
                let retry = e.downcast_ref::<std::io::Error>().is_some_and(|v| {
                    matches!(
                        v.kind(),
                        std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionReset
                    )
                });
                if !retry {
                    return Err(e);
                }
            }
        }
        if Instant::now() >= until {
            bail!("StepThumbs did not respond");
        }
        wake_sent = false;
        std::thread::sleep(
            Duration::from_millis(20).min(until.saturating_duration_since(Instant::now())),
        );
    }
}
fn exchange(
    pipe: &mut crate::pipe_io::Pipe,
    request: &Request,
    until: Instant,
) -> Result<Response> {
    let bytes = serde_json::to_vec(request)?;
    if bytes.len() > 65536 {
        bail!("IPC message too large");
    }
    pipe.write_all(&(bytes.len() as u32).to_le_bytes(), until)?;
    pipe.write_all(&bytes, until)?;
    let mut length = [0; 4];
    pipe.read_exact(&mut length, until)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > 65536 {
        bail!("IPC message too large");
    }
    let mut bytes = vec![0; length];
    pipe.read_exact(&mut bytes, until)?;
    Ok(serde_json::from_slice(&bytes)?)
}
pub fn create_pipe(name: &str, overlapped: bool, buffer: u32) -> Result<HANDLE> {
    // Build the owner ACL directly; no SDDL parser is loaded into the tray.
    use windows_sys::Win32::{
        Security::*,
        System::{SystemServices::*, Threading::*},
    };
    #[repr(align(8))]
    struct Aligned([u8; 256]);
    let mut token = std::ptr::null_mut();
    let mut user = Aligned([0; 256]);
    let mut needed = 0;
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let ok = GetTokenInformation(
            token,
            TokenUser,
            user.0.as_mut_ptr().cast(),
            256,
            &mut needed,
        );
        CloseHandle(token);
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    let mut acl = Aligned([0; 256]);
    let mut descriptor: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    unsafe {
        let sid = (*(user.0.as_ptr().cast::<TOKEN_USER>())).User.Sid;
        if InitializeAcl(acl.0.as_mut_ptr().cast(), 256, ACL_REVISION) == 0
            || AddAccessAllowedAce(acl.0.as_mut_ptr().cast(), ACL_REVISION, GENERIC_ALL, sid) == 0
            || InitializeSecurityDescriptor(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                SECURITY_DESCRIPTOR_REVISION,
            ) == 0
            || SetSecurityDescriptorDacl(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                1,
                acl.0.as_ptr().cast(),
                0,
            ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    let security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
        bInheritHandle: 0,
    };
    let handle = unsafe {
        CreateNamedPipeW(
            crate::wide(name).as_ptr(),
            PIPE_ACCESS_DUPLEX | if overlapped { FILE_FLAG_OVERLAPPED } else { 0 },
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            32,
            buffer,
            buffer,
            0,
            &security,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(handle)
}

pub fn accept_engine() -> Result<File> {
    let handle = create_pipe(&engine_pipe_name(), false, 65536)?;
    let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
    if connected == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
        unsafe { CloseHandle(handle) };
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipe_io::Pipe;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        thread,
    };

    fn pair(server: impl FnOnce(&mut File) + Send + 'static) -> (Pipe, thread::JoinHandle<()>) {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            r"\\.\pipe\StepThumbs.IpcTest.{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let handle = create_pipe(&name, false, 4096).unwrap() as usize;
        let worker = thread::spawn(move || unsafe {
            let handle = handle as HANDLE;
            let ok = ConnectNamedPipe(handle, std::ptr::null_mut());
            assert!(ok != 0 || GetLastError() == ERROR_PIPE_CONNECTED);
            let mut file = File::from_raw_handle(handle);
            server(&mut file);
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let pipe = loop {
            match Pipe::open(&crate::wide(&name)) {
                Ok(pipe) => break pipe,
                Err(_) if Instant::now() < deadline => unsafe {
                    WaitNamedPipeW(crate::wide(&name).as_ptr(), 10);
                },
                Err(error) => panic!("{error}"),
            }
        };
        (pipe, worker)
    }
    fn timeout_after_partial_reply(reply: &'static [u8]) {
        let (mut pipe, worker) = pair(move |file| {
            let _: Request = read(file).unwrap();
            file.write_all(reply).unwrap();
            let _ = file.read(&mut [0u8; 1]); // keep the peer open until cancellation
        });
        let started = Instant::now();
        let error = exchange(
            &mut pipe,
            &Request::Status,
            started + Duration::from_millis(150),
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(pipe);
        worker.join().unwrap();
    }
    #[test]
    fn partial_reply_header_obeys_deadline() {
        timeout_after_partial_reply(&[20]);
    }
    #[test]
    fn partial_reply_body_obeys_deadline() {
        timeout_after_partial_reply(&[20, 0, 0, 0, b'{']);
    }
    #[test]
    fn blocked_pipe_write_obeys_deadline() {
        let (release, wait) = std::sync::mpsc::channel();
        let (mut pipe, worker) = pair(move |_| {
            let _ = wait.recv();
        });
        let started = Instant::now();
        let error = exchange(
            &mut pipe,
            &Request::Thumbnail {
                path: "a".repeat(60_000),
            },
            started + Duration::from_millis(150),
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(pipe);
        release.send(()).unwrap();
        worker.join().unwrap();
    }
    #[test]
    fn fragmented_large_response_is_read_completely() {
        let expected = "a".repeat(50_000);
        let image = expected.clone();
        let (mut pipe, worker) = pair(move |file| {
            let _: Request = read(file).unwrap();
            let bytes = serde_json::to_vec(&Response::image(Path::new(&image))).unwrap();
            file.write_all(&(bytes.len() as u32).to_le_bytes()).unwrap();
            for piece in bytes.chunks(113) {
                file.write_all(piece).unwrap();
            }
        });
        let result = exchange(
            &mut pipe,
            &Request::Status,
            Instant::now() + Duration::from_secs(3),
        )
        .unwrap();
        assert_eq!(result.image.as_deref(), Some(expected.as_str()));
        drop(pipe);
        worker.join().unwrap();
    }
}
