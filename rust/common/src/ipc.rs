use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    os::windows::{io::FromRawHandle, process::CommandExt},
    path::Path,
    process::Command,
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
    unsafe {
        WaitNamedPipeW(crate::wide(pipe_name()).as_ptr(), 500) != 0
            || GetLastError() == ERROR_SEM_TIMEOUT
    }
}
pub fn call(request: &Request, exe: Option<&Path>) -> Result<Response> {
    let name = crate::wide(pipe_name());
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut launched = false;
    let handle = loop {
        let h = unsafe {
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
        if h != INVALID_HANDLE_VALUE {
            break h;
        }
        let error = unsafe { GetLastError() };
        if error != ERROR_PIPE_BUSY
            && error != ERROR_FILE_NOT_FOUND
            && error != ERROR_PATH_NOT_FOUND
        {
            return Err(std::io::Error::from_raw_os_error(error as i32).into());
        }
        if !launched && error != ERROR_PIPE_BUSY {
            if let Some(exe) = exe {
                Command::new(exe)
                    .arg("--background")
                    .creation_flags(0x08000000)
                    .spawn()?;
            }
            launched = true;
        }
        if Instant::now() > deadline {
            bail!("StepThumbs did not respond")
        }
        unsafe { WaitNamedPipeW(name.as_ptr(), 200) };
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut pipe = unsafe { File::from_raw_handle(handle) };
    write(&mut pipe, request)?;
    read(&mut pipe)
}
pub fn accept() -> Result<File> {
    // Owner-only DACL and local clients only. No network listener is created.
    let mut descriptor = std::ptr::null_mut();
    let text = crate::wide("D:P(A;;GA;;;SY)(A;;GA;;;OW)");
    let ok = unsafe {
        windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW(text.as_ptr(),1,&mut descriptor,std::ptr::null_mut())
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let security = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let handle = unsafe {
        CreateNamedPipeW(
            crate::wide(pipe_name()).as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            32,
            65536,
            65536,
            0,
            &security,
        )
    };
    unsafe { LocalFree(descriptor) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
    if connected == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
        unsafe { CloseHandle(handle) };
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}
