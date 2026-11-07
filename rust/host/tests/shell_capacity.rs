//! Exercise the actual listener against a stalled, then recovering engine.
#![allow(unsafe_op_in_unsafe_fn)]
use std::{
    fs::File,
    io::{Read, Write},
    os::windows::io::FromRawHandle,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use stepthumbnail_common as common;
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{Pipes::*, Threading::*},
};
#[path = "../src/shell_pipe.rs"]
mod shell_pipe;

#[test]
fn concurrent_requests_wait_for_capacity_and_then_complete() {
    // Child-only configuration avoids changing the test runner's environment.
    let data = std::env::temp_dir().join(format!(
        "StepThumbs-capacity-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&data).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "capacity_probe_child",
            "--ignored",
            "--nocapture",
        ])
        .env("STEPTHUMBNAIL_DATA_DIR", &data)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("capacity probe timed out");
        }
        thread::sleep(Duration::from_millis(20));
    }
    std::fs::remove_dir(data).unwrap();
}

#[test]
#[ignore = "Run in the isolated child process"]
fn capacity_probe_child() {
    assert!(
        std::env::var("STEPTHUMBNAIL_DATA_DIR")
            .unwrap()
            .contains("StepThumbs-capacity-test-")
    );
    let held = Arc::new(AtomicUsize::new(0));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let (held_engine, release_engine) = (held.clone(), release.clone());
    let (engine_ready, ready) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut first = Some(engine_ready);
        loop {
            let handle =
                common::ipc::create_pipe(&common::ipc::engine_pipe_name(), false, 4096).unwrap();
            if let Some(ready) = first.take() {
                ready.send(()).unwrap();
            }
            let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
            assert!(connected != 0 || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED);
            let mut file = unsafe { File::from_raw_handle(handle) };
            let _: common::ipc::Request = common::ipc::read(&mut file).unwrap();
            let (held, release) = (held_engine.clone(), release_engine.clone());
            thread::spawn(move || {
                held.fetch_add(1, Ordering::SeqCst);
                let (go, ready) = &*release;
                let mut go = go.lock().unwrap();
                while !*go {
                    go = ready.wait(go).unwrap();
                }
                drop(go);
                common::ipc::write(&mut file, &common::ipc::Response::ok()).unwrap();
            });
        }
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut listener = unsafe { shell_pipe::Listener::new().unwrap() };
    let complete = Arc::new(AtomicUsize::new(0));
    let mut clients = Vec::new();
    for _ in 0..40 {
        let complete = complete.clone();
        clients.push(thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let handle = loop {
                let handle = unsafe {
                    CreateFileW(
                        common::wide(common::ipc::pipe_name()).as_ptr(),
                        GENERIC_READ | GENERIC_WRITE,
                        0,
                        std::ptr::null(),
                        OPEN_EXISTING,
                        0,
                        std::ptr::null_mut(),
                    )
                };
                if handle != INVALID_HANDLE_VALUE {
                    break handle;
                }
                assert!(Instant::now() < deadline, "client connection timed out");
                unsafe {
                    WaitNamedPipeW(common::wide(common::ipc::pipe_name()).as_ptr(), 100);
                }
            };
            let mut file = unsafe { File::from_raw_handle(handle) };
            let bytes = serde_json::to_vec(&common::ipc::Request::Status).unwrap();
            file.write_all(&(bytes.len() as u32).to_le_bytes()).unwrap();
            file.write_all(&bytes).unwrap();
            let mut length = [0; 4];
            file.read_exact(&mut length).unwrap();
            let mut bytes = vec![0; u32::from_le_bytes(length) as usize];
            file.read_exact(&mut bytes).unwrap();
            let response: common::ipc::Response = serde_json::from_slice(&bytes).unwrap();
            assert!(response.error.is_none(), "{:?}", response.error);
            complete.fetch_add(1, Ordering::SeqCst);
        }));
    }
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut hit_capacity = false;
    while complete.load(Ordering::SeqCst) < 40 {
        assert!(Instant::now() < deadline, "requests did not drain");
        if held.load(Ordering::SeqCst) >= 30 && listener.waiting_for_capacity() && !hit_capacity {
            // More callers are pending than the native pipe instance limit.
            hit_capacity = true;
            *release.0.lock().unwrap() = true;
            release.1.notify_all();
        }
        let handles = [listener.event, listener.capacity_event()];
        match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, 100) } {
            WAIT_OBJECT_0 => unsafe {
                listener.event().unwrap();
            },
            value if value == WAIT_OBJECT_0 + 1 => unsafe {
                listener.capacity_changed().unwrap();
            },
            WAIT_TIMEOUT => unsafe {
                listener.expire().unwrap();
            },
            other => panic!("wait failed: {other}"),
        }
    }
    assert!(hit_capacity);
    for client in clients {
        client.join().unwrap();
    }
    println!("40 concurrent requests completed after capacity recovery");
}
