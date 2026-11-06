//! Small control protocol shared by the resident tray and the disposable engine.
use anyhow::{Result, bail};
use std::{
    fs::File,
    io::{Read, Write},
    os::windows::{io::FromRawHandle, process::CommandExt},
    path::Path,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{Pipes::*, ProcessStatus::*, Threading::*},
};

pub const MAGIC: u64 = 0x53544850;
pub const STATUS: u32 = 1;
pub const QUIT: u32 = 2;
pub const WAKE: u32 = 3;
pub const TAKE: u32 = 4;
pub const ACK: u32 = 5;
pub const TOGGLE: u32 = 6;
pub const SHOW: u32 = 9;
pub const CONFIGURE: u32 = 10;
pub const SLEEP: u32 = 11;
pub const RELOAD: u32 = 12;
pub const HINTS: u32 = 13;
pub const BLOOM_BYTES: usize = 8192;

pub fn pipe_name() -> String {
    format!("{}.Host", crate::ipc::pipe_name())
}
pub fn event_name(owner: u32, kind: &str) -> String {
    format!("Local\\StepThumbs.Host.{owner}.{kind}")
}
pub fn watch_roots(config: &crate::Config) -> Vec<std::path::PathBuf> {
    let mut roots = Vec::new();
    let drives = unsafe { GetLogicalDrives() };
    for drive in 0..26 {
        if drives & (1 << drive) != 0 {
            let path = std::path::PathBuf::from(format!("{}:\\", (b'A' + drive) as char));
            if unsafe { GetDriveTypeW(crate::wide(&path).as_ptr()) } == 3 {
                roots.push(path);
            }
        }
    }
    for root in &config.roots {
        let root = std::path::PathBuf::from(root);
        if !roots.iter().any(|p| crate::below(&root, p)) {
            roots.push(root);
        }
    }
    if let Ok(data) = std::fs::read(crate::data_dir().join("host-extra-roots.json")) {
        if let Ok(extra) = serde_json::from_slice::<Vec<std::path::PathBuf>>(&data) {
            for root in extra {
                if !roots.iter().any(|p| crate::below(&root, p)) {
                    roots.push(root);
                }
            }
        }
    }
    roots
}
pub fn track_source(path: &Path) -> Result<()> {
    let raw = path.to_string_lossy();
    let root = if raw.as_bytes().get(1) == Some(&b':') {
        let root = std::path::PathBuf::from(format!("{}\\", &raw[..2]));
        if unsafe { GetDriveTypeW(crate::wide(&root).as_ptr()) } == 3 {
            return Ok(());
        }
        root
    } else {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        parent.to_owned()
    };
    let file = crate::data_dir().join("host-extra-roots.json");
    let mut roots = std::fs::read(&file)
        .ok()
        .and_then(|v| serde_json::from_slice::<Vec<std::path::PathBuf>>(&v).ok())
        .unwrap_or_default();
    if roots.iter().any(|p| crate::below(&root, p)) {
        return Ok(());
    }
    roots.retain(|p| !crate::below(p, &root));
    roots.push(root);
    roots.sort();
    crate::cache::atomic_write(&file, &serde_json::to_vec(&roots)?)?;
    let _ = request(HINTS, 0);
    Ok(())
}
pub fn request(op: u32, arg: u64) -> Result<([u64; 32], Vec<u8>)> {
    let until = Instant::now() + Duration::from_secs(4);
    let name = crate::wide(pipe_name());
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
        if error != ERROR_PIPE_BUSY || Instant::now() >= until {
            return Err(std::io::Error::from_raw_os_error(error as i32).into());
        }
        unsafe {
            WaitNamedPipeW(name.as_ptr(), 100);
        }
    };
    let mut pipe = unsafe { File::from_raw_handle(handle) };
    pipe.write_all(&(MAGIC as u32).to_le_bytes())?;
    pipe.write_all(&op.to_le_bytes())?;
    pipe.write_all(&arg.to_le_bytes())?;
    let mut raw = [0u8; 256];
    pipe.read_exact(&mut raw)?;
    let header =
        std::array::from_fn(|i| u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().unwrap()));
    if header[0] != MAGIC || header[1] != 1 || header[26] > 65536 {
        bail!("Invalid tray response");
    }
    let mut payload = vec![0; header[26] as usize];
    pipe.read_exact(&mut payload)?;
    Ok((header, payload))
}
pub fn ensure(op: u32, exe: Option<&Path>) -> Result<()> {
    if request(op, 0).is_ok() {
        return Ok(());
    }
    let Some(exe) = exe else {
        bail!("StepThumbs tray is not running");
    };
    std::process::Command::new(exe)
        .arg("--wake")
        .creation_flags(0x08000000)
        .spawn()?;
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        if request(op, 0).is_ok() {
            return Ok(());
        }
        if Instant::now() >= until {
            bail!("StepThumbs tray did not start");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
pub fn idle_status() -> Option<serde_json::Value> {
    let (header, _) = request(STATUS, 0).ok()?;
    let mut value = std::fs::read(crate::data_dir().join("idle-status.json"))
        .ok()
        .and_then(|v| serde_json::from_slice::<serde_json::Value>(&v).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let memory = process_memory(header[3] as u32);
    let engine_memory = if header[16] != 0 {
        process_memory(header[16] as u32)
    } else {
        0
    };
    value["engine"] = if header[16] == 0 {
        "asleep".into()
    } else {
        "running".into()
    };
    value["host_pid"] = header[3].into();
    value["engine_pid"] = header[16].into();
    value["memory_bytes"] = (memory + engine_memory).into();
    value["host_memory_bytes"] = memory.into();
    value["paused"] = (header[4] & 1 == 0).into();
    value["host_pending"] = header[12].into();
    value["engine_starts"] = header[17].into();
    value["watch_errors"] = header[30].into();
    if let Some(stats) = crate::shared_stats::SharedStats::create() {
        let direct = stats.hits();
        let total = value["cache_hits"].as_u64().unwrap_or(0);
        let previous = value["dll_cache_hits"].as_u64().unwrap_or(0);
        value["cache_hits"] = (total.saturating_sub(previous) + direct).into();
        value["dll_cache_hits"] = direct.into();
    }
    Some(value)
}
pub fn process_memory(pid: u32) -> usize {
    let mut memory = 0;
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
        if !handle.is_null() {
            let mut info: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            info.cb = std::mem::size_of_val(&info) as u32;
            if K32GetProcessMemoryInfo(handle, &mut info, info.cb) != 0 {
                memory = info.WorkingSetSize;
            }
            CloseHandle(handle);
        }
    }
    memory
}
fn bloom_hash(key: &str) -> u64 {
    key.bytes().fold(0xcbf29ce484222325, |n, b| {
        (n ^ b as u64).wrapping_mul(0x100000001b3)
    })
}
pub fn bloom_add(bits: &mut [u8], key: &str) {
    let hash = bloom_hash(key);
    for shift in [0, 21, 42] {
        let bit = ((hash >> shift) as usize) % (bits.len() * 8);
        bits[bit / 8] |= 1 << (bit % 8);
    }
}
pub fn bloom_contains(bits: &[u8], key: &str) -> bool {
    let hash = bloom_hash(key);
    [0, 21, 42].into_iter().all(|shift| {
        let bit = ((hash >> shift) as usize) % (bits.len() * 8);
        bits[bit / 8] & (1 << (bit % 8)) != 0
    })
}
