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
    match request {
        Request::Status if !exists_at(name) => {
            if let Some(status) = crate::host::idle_status() {
                return Ok(Response::status(status));
            }
        }
        Request::Show => {
            if crate::host::ensure(crate::host::SHOW, exe).is_ok() {
                return Ok(Response::ok());
            }
        }
        Request::Pause { paused } => {
            if let Ok((h, _)) = crate::host::request(crate::host::STATUS, 0) {
                if (*paused) == (h[4] & 1 != 0) {
                    crate::host::request(crate::host::TOGGLE, 0)?;
                }
                return Ok(Response::ok());
            }
        }
        Request::Shutdown => {
            if crate::host::request(crate::host::QUIT, 0).is_ok() {
                return Ok(Response::ok());
            }
        }
        _ => {}
    }
    let name = crate::wide(name);
    let until = Instant::now() + Duration::from_secs(120);
    let mut wake_sent = false;
    loop {
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            if matches!(request, Request::Status) {
                if let Some(status) = crate::host::idle_status() {
                    return Ok(Response::status(status));
                }
            }
            let error = unsafe { GetLastError() };
            if !matches!(
                error,
                ERROR_PIPE_BUSY | ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND
            ) {
                return Err(std::io::Error::from_raw_os_error(error as i32).into());
            }
            if Instant::now() >= until {
                bail!("StepThumbs did not respond");
            }
            if error != ERROR_PIPE_BUSY && !wake_sent {
                crate::host::ensure(crate::host::WAKE, exe)?;
                wake_sent = true;
            }
            unsafe {
                WaitNamedPipeW(name.as_ptr(), 100);
            }
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let mut pipe = unsafe { File::from_raw_handle(handle) };
        let response = write(&mut pipe, request).and_then(|()| read::<Response>(&mut pipe));
        match response {
            Ok(v) if v.error.as_deref() != Some("Engine is restarting") => return Ok(v),
            Ok(_) => {
                if matches!(request, Request::Status) {
                    if let Some(status) = crate::host::idle_status() {
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
        std::thread::sleep(Duration::from_millis(20));
    }
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
