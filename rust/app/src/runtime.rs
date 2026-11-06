//! The engine lives only while work or its settings window need it.
use crate::{Broker, common};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::SYNCHRONIZE,
    System::{SystemInformation::GetTickCount64, Threading::*},
};

static OWNER: AtomicU32 = AtomicU32::new(0);
static COLD: AtomicBool = AtomicBool::new(true);
static VISIBLE: AtomicBool = AtomicBool::new(false);
static STOPPING: AtomicBool = AtomicBool::new(false);
static FAILED: AtomicBool = AtomicBool::new(false);
static INITIAL_PAUSE: AtomicBool = AtomicBool::new(false);
static LAST: AtomicU64 = AtomicU64::new(0);
static ACTIVITY_EVENT: AtomicIsize = AtomicIsize::new(0);
pub fn init(owner: u32, cold: bool) {
    OWNER.store(owner, Ordering::Release);
    COLD.store(cold, Ordering::Release);
    if let Ok((header, _)) = common::host::request(common::host::STATUS, 0) {
        INITIAL_PAUSE.store(header[4] & 1 == 0, Ordering::Release);
    }
    unsafe {
        ACTIVITY_EVENT.store(
            CreateEventW(std::ptr::null(), 0, 1, std::ptr::null()) as isize,
            Ordering::Release,
        );
        LAST.store(GetTickCount64(), Ordering::Release);
    }
}
pub fn initially_paused() -> bool {
    INITIAL_PAUSE.load(Ordering::Acquire)
}
pub fn enabled() -> bool {
    OWNER.load(Ordering::Acquire) != 0
}
pub fn cold() -> bool {
    COLD.load(Ordering::Acquire)
}
pub fn host_memory() -> u64 {
    if enabled() {
        common::host::process_memory(OWNER.load(Ordering::Acquire)) as u64
    } else {
        0
    }
}
pub fn owner() -> u32 {
    OWNER.load(Ordering::Acquire)
}
pub fn stopping() -> bool {
    STOPPING.load(Ordering::Acquire)
}
pub fn failed() -> bool {
    FAILED.load(Ordering::Acquire)
}
pub fn fail(b: Arc<Broker>) {
    FAILED.store(true, Ordering::Release);
    stop(b);
}
pub fn visible(value: bool) {
    VISIBLE.store(value, Ordering::Release);
    if value {
        touch();
    } else {
        changed();
    }
}
pub fn touch() {
    if enabled() {
        unsafe {
            LAST.store(GetTickCount64(), Ordering::Release);
        }
        changed();
    }
}
pub fn changed() {
    if enabled() {
        unsafe {
            SetEvent(ACTIVITY_EVENT.load(Ordering::Acquire) as HANDLE);
        }
    }
}
pub fn restore(b: &Broker) {
    let Ok(bytes) = std::fs::read(common::data_dir().join("engine-session.json")) else {
        return;
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return;
    };
    for (key, counter) in [
        ("success", &b.state.success),
        ("failures", &b.state.failures),
        ("hits", &b.state.hits),
        ("source_bytes", &b.state.source_bytes),
        ("promotions", &b.state.promotions),
        ("scans_started", &b.state.scans_started),
        ("journal_passes", &b.state.journal_passes),
    ] {
        if let Some(n) = v[key].as_u64() {
            counter.store(n, Ordering::Relaxed);
        }
    }
    if let Ok(recent) = serde_json::from_value(v["recent"].clone()) {
        *b.state.recent.lock().unwrap() = recent;
    }
}
pub fn persist(b: &Broker) {
    if !enabled() {
        return;
    }
    let s = &b.state;
    let session = serde_json::json!({"success":s.success.load(Ordering::Relaxed),"failures":s.failures.load(Ordering::Relaxed),"hits":s.hits.load(Ordering::Relaxed),"source_bytes":s.source_bytes.load(Ordering::Relaxed),"promotions":s.promotions.load(Ordering::Relaxed),"scans_started":s.scans_started.load(Ordering::Relaxed),"journal_passes":s.journal_passes.load(Ordering::Relaxed),"recent":&*s.recent.lock().unwrap()});
    let _ = common::cache::atomic_write(
        &common::data_dir().join("engine-session.json"),
        &serde_json::to_vec(&session).unwrap(),
    );
    let status = common::ipc::Response::status(b.status());
    if let Some(status) = status.status {
        let _ = common::cache::atomic_write(
            &common::data_dir().join("idle-status.json"),
            &serde_json::to_vec(&status).unwrap(),
        );
    }
    if let Err(e) = crate::cache_index::export_host_hints() {
        crate::cache_index::log(&e);
    }
}
fn stop(b: Arc<Broker>) {
    if STOPPING.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(move || {
        b.drain();
        persist(&b);
        crate::ui::quit();
    });
}
fn busy(b: &Broker) -> bool {
    VISIBLE.load(Ordering::Acquire)
        || !crate::monitor::ready()
        || crate::cache_index::usage().is_none()
        || !b.state.jobs.lock().unwrap().is_empty()
        || (!*b.state.paused.lock().unwrap() && b.state.scanning.lock().unwrap().is_some())
        || (!*b.state.paused.lock().unwrap()
            && b.state.preflight_pending.load(Ordering::Acquire) != 0)
}
pub fn start(b: Arc<Broker>) {
    if !enabled() {
        return;
    }
    let events = b.clone();
    std::thread::spawn(move || unsafe {
        let owner = OWNER.load(Ordering::Acquire);
        let wake = OpenEventW(
            SYNCHRONIZE,
            0,
            common::wide(common::host::event_name(owner, "wake")).as_ptr(),
        );
        let ready = OpenEventW(
            EVENT_MODIFY_STATE,
            0,
            common::wide(common::host::event_name(owner, "ready")).as_ptr(),
        );
        if wake.is_null() || ready.is_null() {
            fail(events);
            return;
        }
        SetEvent(ready);
        CloseHandle(ready);
        while WaitForSingleObject(wake, INFINITE) == WAIT_OBJECT_0 {
            if stopping() {
                break;
            }
            loop {
                let Ok((header, payload)) = common::host::request(common::host::TAKE, 0) else {
                    fail(events.clone());
                    break;
                };
                let paused = header[4] & 1 == 0;
                let pause_changed = {
                    let mut p = events.state.paused.lock().unwrap();
                    if *p != paused {
                        *p = paused;
                        events.state.resumed.notify_all();
                        true
                    } else {
                        false
                    }
                };
                if pause_changed {
                    crate::monitor::wake();
                    crate::ui::update();
                }
                if let Err(error) = crate::monitor::ingest(&payload, header[14]) {
                    crate::cache_index::log(&error);
                    fail(events.clone());
                    break;
                }
                let Ok((ack, _)) = common::host::request(common::host::ACK, header[15]) else {
                    fail(events.clone());
                    break;
                };
                if ack[2] == 2 {
                    continue;
                }
                if header[28] == 255 {
                    stop(events.clone());
                    break;
                }
                if header[28] > 0 {
                    touch();
                    crate::ui::show_page((header[28] - 1) as usize);
                }
                changed();
                break;
            }
        }
        CloseHandle(wake);
    });
    std::thread::spawn(move || {
        let idle = std::env::var("STEPTHUMBS_ENGINE_IDLE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_secs(60))
            .clamp(Duration::from_secs(1), Duration::from_secs(600));
        let idle = idle.as_millis() as u64;
        loop {
            if stopping() {
                return;
            }
            let remaining = unsafe {
                LAST.load(Ordering::Acquire)
                    .saturating_add(idle)
                    .saturating_sub(GetTickCount64())
            };
            if remaining == 0
                && !busy(&b)
                && common::host::request(common::host::SLEEP, std::process::id() as u64)
                    .is_ok_and(|(h, _)| h[2] == 0)
            {
                stop(b);
                return;
            }
            unsafe {
                WaitForSingleObject(
                    ACTIVITY_EVENT.load(Ordering::Acquire) as HANDLE,
                    if remaining == 0 {
                        INFINITE
                    } else {
                        remaining.min(u32::MAX as u64 - 1) as u32
                    },
                );
            }
        }
    });
}
