#![windows_subsystem = "windows"]
#![allow(unsafe_op_in_unsafe_fn)]

//! Resident tray, overlapped control pipe and filesystem notifications.
//! Rendering, indexing and settings live in StepThumbsEngine.exe.

use core::{
    mem::{offset_of, size_of, zeroed},
    ptr::{addr_of_mut, null, null_mut},
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};
use stepthumbnail_common as common;
mod shell_pipe;
mod watch_open;
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    Storage::FileSystem::*,
    System::{
        Diagnostics::Debug::*, IO::*, JobObjects::*, LibraryLoader::*, Memory::*, Performance::*,
        Pipes::*, Registry::*, SystemInformation::*, SystemServices::*, Threading::*,
    },
    UI::{
        HiDpi::*,
        Shell::{NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW},
        WindowsAndMessaging::*,
    },
};

macro_rules! w {
    ($s:literal) => {{
        const B: &[u8] = $s.as_bytes();
        const W: [u16; B.len() + 1] = {
            let mut a = [0; B.len() + 1];
            let mut i = 0;
            while i < B.len() {
                a[i] = B[i] as u16;
                i += 1;
            }
            a
        };
        W.as_ptr()
    }};
}
const MAGIC: u64 = 0x53544850;
const HEADER: usize = 32 * 8;
const WATCH_BYTES: usize = 16 * 1024;
const QUEUE_BYTES: usize = 64 * 1024;
const MAX_ROOTS: usize = 32;
const QUIT: u32 = 1;
const WAKE: u32 = 2;
const PAUSE: u32 = 4;
const REFRESH: u32 = 8;
const READD: u32 = 16;
const MENU: u32 = 32;
const CLOSE_MENU: u32 = 64;
const OPEN_ACTIVITY: u32 = 128;
const OPEN_CONFIG: u32 = 256;
const RELOAD_CONFIG: u32 = 512;
static ACTIONS: AtomicU32 = AtomicU32::new(0);
static MENU_HANDLE: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(null_mut());
static MENU_MS: AtomicU32 = AtomicU32::new(80);
static TASKBAR: AtomicU32 = AtomicU32::new(0);
static AUTO: AtomicU32 = AtomicU32::new(1);
#[derive(Clone, Copy)]
struct Text {
    at: usize,
    len: usize,
}
impl Text {
    const NONE: Self = Self { at: 0, len: 0 };
}
#[repr(align(8))]
struct Block<const N: usize>([u8; N]);
struct Watch {
    path: Text,
    file: HANDLE,
    event: HANDLE,
    io: OVERLAPPED,
    buffer: *mut u8,
    extended: bool,
    remote: bool,
    pending: bool,
    retry_at: u64,
    retry_failures: usize,
}
impl Watch {
    const EMPTY: Self = Self {
        path: Text::NONE,
        file: INVALID_HANDLE_VALUE,
        event: null_mut(),
        io: unsafe { zeroed() },
        buffer: null_mut(),
        extended: true,
        remote: false,
        pending: false,
        retry_at: 0,
        retry_failures: 0,
    };
}
struct Host {
    config: Option<common::Config>,
    bloom: [u8; common::host::BLOOM_BYTES],
    pending_page: u64,
    next_page: u64,
    stopping: bool,
    quitting: bool,
    demand: bool,
    cleanup_demand: bool,
    force_cold: bool,
    shutdown_at: u64,
    restart_at: u64,
    crash_count: usize,
    window: HWND,
    icon: HICON,
    roots: usize,
    pool: [u16; 4096],
    pool_used: usize,
    watch: [Watch; MAX_ROOTS],
    scratch: [u16; 32768],
    pipe: HANDLE,
    pipe_event: HANDLE,
    pipe_io: OVERLAPPED,
    pipe_phase: u32,
    pipe_pending: bool,
    pipe_fill: usize,
    pipe_input: [u64; 2],
    reply: [u64; 32],
    reply_ptr: *mut u8,
    reply_len: usize,
    reply_fill: usize,
    reply_alloc: bool,
    deadline: u64,
    quit_after_write: bool,
    queue: *mut u8,
    queue_len: usize,
    queue_count: u64,
    dirty: u64,
    generation: u64,
    peak_queue: u64,
    worker: HANDLE,
    job: HANDLE,
    wake: HANDLE,
    ready: HANDLE,
    worker_pid: u32,
    worker_since: i64,
    no_tray: bool,
    pid: u32,
    frequency: i64,
    batches: u64,
    records: u64,
    models: u64,
    directories: u64,
    ignored: u64,
    overflows: u64,
    pipe_requests: u64,
    launches: u64,
    ready_us: u64,
    launch_us: u64,
    exits: u64,
    exit_code: u32,
    worker_life_us: u64,
    wakeups: u64,
    ack: u64,
    watch_errors: u64,
    watch_epoch: u64,
    watch_open: Option<watch_open::Attempt>,
}
static mut HOST: Host = Host {
    config: None,
    bloom: [0; common::host::BLOOM_BYTES],
    pending_page: 0,
    next_page: 0,
    stopping: false,
    quitting: false,
    demand: false,
    cleanup_demand: false,
    force_cold: true,
    shutdown_at: 0,
    restart_at: 0,
    crash_count: 0,
    window: null_mut(),
    icon: null_mut(),
    roots: 0,
    pool: [0; 4096],
    pool_used: 0,
    watch: [const { Watch::EMPTY }; MAX_ROOTS],
    scratch: [0; 32768],
    pipe: INVALID_HANDLE_VALUE,
    pipe_event: null_mut(),
    pipe_io: unsafe { zeroed() },
    pipe_phase: 0,
    pipe_pending: false,
    pipe_fill: 0,
    pipe_input: [0; 2],
    reply: [0; 32],
    reply_ptr: null_mut(),
    reply_len: 0,
    reply_fill: 0,
    reply_alloc: false,
    deadline: 0,
    quit_after_write: false,
    queue: null_mut(),
    queue_len: 0,
    queue_count: 0,
    dirty: 0,
    generation: 0,
    peak_queue: 0,
    worker: null_mut(),
    job: null_mut(),
    wake: null_mut(),
    ready: null_mut(),
    worker_pid: 0,
    worker_since: 0,
    no_tray: false,
    pid: 0,
    frequency: 1,
    batches: 0,
    records: 0,
    models: 0,
    directories: 0,
    ignored: 0,
    overflows: 0,
    pipe_requests: 0,
    launches: 0,
    ready_us: 0,
    launch_us: 0,
    exits: 0,
    exit_code: 0,
    worker_life_us: 0,
    wakeups: 0,
    ack: 0,
    watch_errors: 0,
    watch_epoch: 0,
    watch_open: None,
};
unsafe fn state() -> &'static mut Host {
    &mut *addr_of_mut!(HOST)
}
unsafe fn copy_text(s: &mut Host, p: *const u16, n: usize) -> Text {
    if s.pool_used + n + 1 > s.pool.len() {
        fatal(2);
    }
    let at = s.pool_used;
    core::ptr::copy_nonoverlapping(p, s.pool.as_mut_ptr().add(at), n);
    s.pool[at + n] = 0;
    s.pool_used += n + 1;
    Text { at, len: n }
}
unsafe fn text(s: &Host, t: Text) -> *const u16 {
    s.pool.as_ptr().add(t.at)
}
unsafe fn fatal(code: u32) -> ! {
    ExitProcess(code)
}
fn lower(c: u16) -> u16 {
    if (65..=90).contains(&c) { c + 32 } else { c }
}
unsafe fn ticks() -> i64 {
    let mut n = 0;
    QueryPerformanceCounter(&mut n);
    n
}
fn micros(s: &Host, delta: i64) -> u64 {
    (delta.max(0) as u64).saturating_mul(1_000_000) / (s.frequency as u64)
}
unsafe fn name(buf: *mut u16, prefix: &str, pid: u32, suffix: &str) {
    let mut n = 0;
    for b in prefix.bytes() {
        *buf.add(n) = b as u16;
        n += 1;
    }
    let mut digits = [0u16; 10];
    let mut count = 0;
    let mut v = pid;
    loop {
        digits[count] = (v % 10) as u16 + 48;
        count += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    while count > 0 {
        count -= 1;
        *buf.add(n) = digits[count];
        n += 1;
    }
    for b in suffix.bytes() {
        *buf.add(n) = b as u16;
        n += 1;
    }
    *buf.add(n) = 0;
}

unsafe fn configure(s: &mut Host) {
    s.config = Some(match common::Config::load() {
        Ok(v) => v,
        Err(e) => {
            MessageBoxW(
                null_mut(),
                common::wide(format!("Settings could not be loaded: {e:#}")).as_ptr(),
                w!("StepThumbs"),
                MB_OK | MB_ICONERROR,
            );
            fatal(2)
        }
    });
    s.bloom = std::fs::read(common::data_dir().join("host-directories.bin"))
        .ok()
        .and_then(|v| v.try_into().ok())
        .unwrap_or([0; common::host::BLOOM_BYTES]);
    let paths = common::host::watch_roots(s.config.as_ref().unwrap());
    for path in paths {
        if s.roots == MAX_ROOTS {
            fatal(2);
        }
        let wide = common::wide(path);
        let path = copy_text(s, wide.as_ptr(), wide.len() - 1);
        s.watch[s.roots].path = path;
        s.roots += 1;
    }
}

type Notify = unsafe extern "system" fn(u32, *const NOTIFYICONDATAW) -> BOOL;
unsafe fn notify(s: &Host, action: u32) {
    if s.no_tray {
        return;
    }
    let mut data: NOTIFYICONDATAW = zeroed();
    data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = s.window;
    data.uID = 1;
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = WM_APP + 1;
    data.hIcon = s.icon;
    let tip = b"StepThumbs";
    for (i, b) in tip.iter().enumerate() {
        data.szTip[i] = *b as u16;
    }
    {
        let module = LoadLibraryExW(w!("shell32.dll"), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
        if !module.is_null() {
            if let Some(function) = GetProcAddress(module, c"Shell_NotifyIconW".as_ptr().cast()) {
                let f: Notify = core::mem::transmute(function);
                f(action, &data);
            }
            FreeLibrary(module);
        }
    }
}
macro_rules! image_bytes {
    ($theme:literal,$size:literal) => {{
        const B: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/", $theme, "-", $size, ".dib"));
        B
    }};
}
unsafe fn load_icon(s: &Host) -> HICON {
    let mut light = 0u32;
    let mut bytes = 4;
    RegGetValueW(
        HKEY_CURRENT_USER,
        w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
        w!("SystemUsesLightTheme"),
        RRF_RT_REG_DWORD,
        null_mut(),
        (&mut light as *mut u32).cast(),
        &mut bytes,
    );
    let size = GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForWindow(s.window).max(96)).max(16);
    let image = match (light != 0, size) {
        (false, 0..=16) => image_bytes!("dark", "16"),
        (true, 0..=16) => image_bytes!("light", "16"),
        (false, 17..=20) => image_bytes!("dark", "20"),
        (true, 17..=20) => image_bytes!("light", "20"),
        (false, 21..=24) => image_bytes!("dark", "24"),
        (true, 21..=24) => image_bytes!("light", "24"),
        (false, 25..=32) => image_bytes!("dark", "32"),
        (true, 25..=32) => image_bytes!("light", "32"),
        (false, 33..=40) => image_bytes!("dark", "40"),
        (true, 33..=40) => image_bytes!("light", "40"),
        (false, 41..=48) => image_bytes!("dark", "48"),
        (true, 41..=48) => image_bytes!("light", "48"),
        (false, _) => image_bytes!("dark", "64"),
        (true, _) => image_bytes!("light", "64"),
    };
    CreateIconFromResourceEx(
        image.as_ptr(),
        image.len() as u32,
        1,
        0x30000,
        size,
        size,
        0,
    )
}
unsafe fn menu(window: HWND, test: bool) {
    let previous = MENU_HANDLE.swap(null_mut(), Ordering::Relaxed);
    if !previous.is_null() {
        EndMenu();
        DestroyMenu(previous);
    }
    let menu = CreatePopupMenu();
    let info = MENUINFO {
        cbSize: size_of::<MENUINFO>() as u32,
        fMask: MIM_STYLE,
        dwStyle: MNS_MODELESS,
        ..zeroed()
    };
    // Keep the I/O event loop running while the native tray menu is open.
    SetMenuInfo(menu, &info);
    AppendMenuW(menu, MF_STRING, 10, w!("Activity"));
    AppendMenuW(menu, MF_STRING, 11, w!("Configure"));
    AppendMenuW(
        menu,
        MF_STRING,
        12,
        if AUTO.load(Ordering::Relaxed) != 0 {
            w!("Pause preflight")
        } else {
            w!("Resume preflight")
        },
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, 14, w!("Open log"));
    AppendMenuW(menu, MF_STRING, 13, w!("Exit"));
    let mut p: POINT = zeroed();
    GetCursorPos(&mut p);
    SetForegroundWindow(window);
    if test {
        SetTimer(window, 1, MENU_MS.load(Ordering::Relaxed), None);
    }
    MENU_HANDLE.store(menu, Ordering::Relaxed);
    TrackPopupMenuEx(menu, TPM_RIGHTBUTTON, p.x, p.y, window, null());
    PostMessageW(window, WM_NULL, 0, 0);
}
unsafe extern "system" fn window(window: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == TASKBAR.load(Ordering::Relaxed) && msg != 0 {
        ACTIONS.fetch_or(READD, Ordering::Relaxed);
        return 0;
    }
    match msg {
        m if m == WM_APP + 1 => {
            if lp as u32 == WM_RBUTTONUP {
                menu(window, false);
            } else if lp as u32 == WM_LBUTTONUP {
                ACTIONS.fetch_or(OPEN_ACTIVITY, Ordering::Relaxed);
            }
            0
        }
        WM_THEMECHANGED | WM_SETTINGCHANGE | WM_DPICHANGED => {
            ACTIONS.fetch_or(REFRESH, Ordering::Relaxed);
            0
        }
        WM_TIMER if wp == 1 => {
            KillTimer(window, 1);
            EndMenu();
            0
        }
        WM_UNINITMENUPOPUP => {
            KillTimer(window, 1);
            ACTIONS.fetch_or(CLOSE_MENU, Ordering::Relaxed);
            0
        }
        WM_COMMAND => {
            ACTIONS.fetch_or(
                match wp as u32 & 0xffff {
                    10 => OPEN_ACTIVITY,
                    11 => OPEN_CONFIG,
                    14 => {
                        use std::os::windows::process::CommandExt;
                        std::process::Command::new("explorer.exe")
                            .arg(common::data_dir().join("activity.log"))
                            .creation_flags(CREATE_NO_WINDOW)
                            .spawn()
                            .ok();
                        0
                    }
                    12 => PAUSE,
                    13 => QUIT,
                    _ => 0,
                },
                Ordering::Relaxed,
            );
            0
        }
        WM_CLOSE | WM_ENDSESSION => {
            ACTIONS.fetch_or(QUIT, Ordering::Relaxed);
            0
        }
        WM_QUERYENDSESSION => 1,
        _ => DefWindowProcW(window, msg, wp, lp),
    }
}

unsafe fn open_pipe(s: &mut Host) {
    let mut token = null_mut();
    if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
        fatal(4);
    }
    let mut user = Block([0u8; 256]);
    let mut needed = 0;
    let ok = GetTokenInformation(
        token,
        TokenUser,
        user.0.as_mut_ptr().cast(),
        256,
        &mut needed,
    );
    CloseHandle(token);
    if ok == 0 {
        fatal(4);
    }
    let sid = (*(user.0.as_ptr().cast::<TOKEN_USER>())).User.Sid;
    let mut acl = Block([0u8; 256]);
    let mut descriptor: SECURITY_DESCRIPTOR = zeroed();
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
        fatal(4);
    }
    let security = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
        bInheritHandle: 0,
    };
    let pipe_name = common::wide(common::host::pipe_name());
    s.pipe = CreateNamedPipeW(
        pipe_name.as_ptr(),
        PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
        1,
        4096,
        4096,
        0,
        &security,
    );
    if s.pipe == INVALID_HANDLE_VALUE {
        fatal(4);
    }
    s.pipe_event = CreateEventW(null(), 1, 0, null());
    if s.pipe_event.is_null() {
        fatal(4);
    }
    connect(s);
}
unsafe fn connect(s: &mut Host) {
    ResetEvent(s.pipe_event);
    s.pipe_io = zeroed();
    s.pipe_io.hEvent = s.pipe_event;
    s.pipe_phase = 0;
    s.pipe_fill = 0;
    s.deadline = 0;
    let ok = ConnectNamedPipe(s.pipe, &mut s.pipe_io);
    let error = if ok == 0 { GetLastError() } else { 0 };
    s.pipe_pending = error == ERROR_IO_PENDING;
    if ok != 0 || error == ERROR_PIPE_CONNECTED {
        SetEvent(s.pipe_event);
    } else if error != ERROR_IO_PENDING {
        fatal(4);
    }
}
unsafe fn free_reply(s: &mut Host) {
    if s.reply_alloc {
        VirtualFree(s.reply_ptr.cast(), 0, MEM_RELEASE);
    }
    s.reply_alloc = false;
    s.reply_ptr = null_mut();
}
unsafe fn disconnect(s: &mut Host) {
    if s.pipe_pending {
        CancelIoEx(s.pipe, &s.pipe_io);
        let mut n = 0;
        GetOverlappedResult(s.pipe, &s.pipe_io, &mut n, 1);
    }
    free_reply(s);
    DisconnectNamedPipe(s.pipe);
    connect(s);
}
unsafe fn read_request(s: &mut Host) {
    ResetEvent(s.pipe_event);
    s.pipe_io = zeroed();
    s.pipe_io.hEvent = s.pipe_event;
    s.pipe_phase = 1;
    let p = (s.pipe_input.as_mut_ptr() as *mut u8).add(s.pipe_fill);
    let ok = ReadFile(
        s.pipe,
        p,
        (16 - s.pipe_fill) as u32,
        null_mut(),
        &mut s.pipe_io,
    );
    let error = if ok == 0 { GetLastError() } else { 0 };
    s.pipe_pending = error == ERROR_IO_PENDING;
    if ok == 0 && !s.pipe_pending {
        disconnect(s);
    }
}
unsafe fn write_reply(s: &mut Host) {
    ResetEvent(s.pipe_event);
    s.pipe_io = zeroed();
    s.pipe_io.hEvent = s.pipe_event;
    s.pipe_phase = 2;
    let ok = WriteFile(
        s.pipe,
        s.reply_ptr.add(s.reply_fill),
        (s.reply_len - s.reply_fill) as u32,
        null_mut(),
        &mut s.pipe_io,
    );
    let error = if ok == 0 { GetLastError() } else { 0 };
    s.pipe_pending = error == ERROR_IO_PENDING;
    if ok == 0 && !s.pipe_pending {
        disconnect(s);
    }
}
unsafe fn finish_client(s: &mut Host) {
    let quit = s.quit_after_write;
    s.quit_after_write = false;
    disconnect(s);
    if quit {
        ACTIONS.fetch_or(QUIT, Ordering::Relaxed);
    }
}
unsafe fn await_client_close(s: &mut Host) {
    // DisconnectNamedPipe discards unread output. Await EOF asynchronously
    // instead of blocking the window thread in FlushFileBuffers.
    free_reply(s);
    ResetEvent(s.pipe_event);
    s.pipe_io = zeroed();
    s.pipe_io.hEvent = s.pipe_event;
    s.pipe_phase = 3;
    let ok = ReadFile(
        s.pipe,
        s.pipe_input.as_mut_ptr().cast(),
        1,
        null_mut(),
        &mut s.pipe_io,
    );
    let error = if ok == 0 { GetLastError() } else { 0 };
    s.pipe_pending = error == ERROR_IO_PENDING;
    if ok == 0 && !s.pipe_pending {
        finish_client(s);
    }
}
unsafe fn clear_queue(s: &mut Host) {
    if !s.queue.is_null() {
        VirtualFree(s.queue.cast(), 0, MEM_RELEASE);
        s.queue = null_mut();
    }
    s.queue_len = 0;
    s.queue_count = 0;
    s.dirty = 0;
}
unsafe fn snapshot(s: &mut Host, error: u64, payload: usize) {
    let flags =
        (AUTO.load(Ordering::Relaxed) as u64) | (((!s.worker.is_null()) as u64) << 1) | (1 << 2); // Standard-library host; retained for the diagnostic protocol.
    s.reply = [
        MAGIC,
        1,
        error,
        s.pid as u64,
        flags,
        s.roots as u64,
        s.batches,
        s.records,
        s.models,
        s.directories,
        s.ignored,
        s.overflows,
        s.queue_count,
        s.queue_len as u64,
        s.dirty,
        s.generation,
        s.worker_pid as u64,
        s.launches,
        s.ready_us,
        s.exits,
        s.exit_code as u64,
        s.wakeups,
        s.pipe_requests,
        s.launch_us,
        s.worker_life_us,
        HEADER as u64,
        payload as u64,
        WATCH_BYTES as u64,
        s.pending_page,
        s.ack,
        s.watch_errors,
        s.peak_queue,
    ];
}
unsafe fn reply(s: &mut Host) {
    let command = s.pipe_input[0];
    let op = (command >> 32) as u32;
    let arg = s.pipe_input[1];
    let mut error = 0;
    let mut payload = 0;
    if command as u32 != MAGIC as u32 {
        error = 1;
    } else {
        match op {
            1 => {}
            2 => s.quit_after_write = true,
            3 => launch(s),
            4 => payload = s.queue_len,
            5 => {
                if arg == s.generation {
                    clear_queue(s);
                    s.pending_page = 0;
                    s.ack = arg;
                } else {
                    error = 2;
                    if !s.wake.is_null() {
                        SetEvent(s.wake);
                    }
                }
            }
            6 => {
                AUTO.fetch_xor(1, Ordering::Relaxed);
                if AUTO.load(Ordering::Relaxed) != 0 {
                    launch(s);
                } else if !s.wake.is_null() {
                    SetEvent(s.wake);
                }
            }
            7 => {
                if !s.worker.is_null() {
                    TerminateJobObject(s.job, 0);
                }
            }
            8 => {
                MENU_MS.store(
                    if arg == 0 {
                        80
                    } else {
                        arg.clamp(50, 5000) as u32
                    },
                    Ordering::Relaxed,
                );
                ACTIONS.fetch_or(MENU, Ordering::Relaxed);
            }
            9 => queue_page(s, 1),
            10 => queue_page(s, 3),
            11 => {
                if arg == s.worker_pid as u64
                    && s.queue_count == 0
                    && s.dirty == 0
                    && s.pending_page == 0
                    && !s.quitting
                {
                    s.stopping = true;
                } else {
                    error = 4;
                }
            }
            13 => {
                ACTIONS.fetch_or(RELOAD_CONFIG, Ordering::Relaxed);
            }
            12 => {
                s.next_page = arg;
                s.demand = true;
                ACTIONS.fetch_or(RELOAD_CONFIG, Ordering::Relaxed);
            }
            _ => error = 3,
        }
    }
    snapshot(s, error, payload);
    s.reply_fill = 0;
    s.reply_len = HEADER + payload;
    if payload > 0 {
        s.reply_ptr = VirtualAlloc(
            null(),
            s.reply_len,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
        .cast();
        if s.reply_ptr.is_null() {
            fatal(5);
        }
        s.reply_alloc = true;
        core::ptr::copy_nonoverlapping(s.reply.as_ptr().cast::<u8>(), s.reply_ptr, HEADER);
        core::ptr::copy_nonoverlapping(s.queue, s.reply_ptr.add(HEADER), payload);
    } else {
        s.reply_ptr = s.reply.as_mut_ptr().cast();
    }
    write_reply(s);
}
unsafe fn pipe_event(s: &mut Host) {
    let mut n = 0;
    if s.pipe_phase == 0 {
        s.pipe_pending = false;
        s.deadline = GetTickCount64() + 3000;
        read_request(s);
        return;
    }
    let ok = GetOverlappedResult(s.pipe, &s.pipe_io, &mut n, 0);
    s.pipe_pending = false;
    if ok == 0 {
        if s.pipe_phase == 3 {
            finish_client(s);
        } else {
            disconnect(s);
        }
        return;
    }
    if s.pipe_phase == 3 {
        finish_client(s);
        return;
    }
    if s.pipe_phase == 1 {
        if n == 0 {
            disconnect(s);
            return;
        }
        s.pipe_fill += n as usize;
        if s.pipe_fill < 16 {
            read_request(s);
        } else {
            s.pipe_requests += 1;
            reply(s);
        }
    } else {
        s.reply_fill += n as usize;
        if s.reply_fill < s.reply_len {
            write_reply(s);
        } else {
            await_client_close(s);
        }
    }
}

unsafe fn arm(w: &mut Watch) -> bool {
    ResetEvent(w.event);
    w.io = zeroed();
    w.io.hEvent = w.event;
    let filter = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_SIZE;
    let mut ok = if w.extended {
        ReadDirectoryChangesExW(
            w.file,
            w.buffer.cast(),
            WATCH_BYTES as u32,
            1,
            filter,
            null_mut(),
            &mut w.io,
            None,
            ReadDirectoryNotifyExtendedInformation,
        )
    } else {
        0
    };
    if ok == 0
        && w.extended
        && matches!(
            GetLastError(),
            ERROR_INVALID_PARAMETER | ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED
        )
    {
        w.extended = false;
    }
    if !w.extended {
        ok = ReadDirectoryChangesW(
            w.file,
            w.buffer.cast(),
            WATCH_BYTES as u32,
            1,
            filter,
            null_mut(),
            &mut w.io,
            None,
        );
    }
    w.pending = ok != 0;
    w.pending
}
unsafe fn stop_watch(w: &mut Watch) {
    if w.file != INVALID_HANDLE_VALUE {
        if w.pending {
            CancelIoEx(w.file, &w.io);
            let mut n = 0;
            GetOverlappedResult(w.file, &w.io, &mut n, 1);
        }
        CloseHandle(w.file);
        w.file = INVALID_HANDLE_VALUE;
    }
    if !w.event.is_null() {
        CloseHandle(w.event);
        w.event = null_mut();
    }
    if !w.buffer.is_null() {
        VirtualFree(w.buffer.cast(), 0, MEM_RELEASE);
        w.buffer = null_mut();
    }
    w.pending = false;
}
unsafe fn setup_watches(s: &mut Host) {
    for i in 0..s.roots {
        s.watch[i].retry_at = GetTickCount64().max(1);
        s.watch[i].retry_failures = 0;
    }
}
unsafe fn failed_watch(s: &mut Host, index: usize) {
    s.watch_errors += 1;
    let watch = &mut s.watch[index];
    stop_watch(watch);
    let delay = [1000, 2000, 5000, 15000, 30000][watch.retry_failures.min(4)];
    watch.retry_failures = watch.retry_failures.saturating_add(1);
    watch.retry_at = GetTickCount64().saturating_add(delay);
}
fn next_watch_retry(s: &Host) -> Option<u64> {
    if s.quitting || s.watch_open.is_some() {
        return None;
    }
    s.watch[..s.roots]
        .iter()
        .filter_map(|w| (w.retry_at != 0).then_some(w.retry_at))
        .min()
}
unsafe fn begin_watch_retry(s: &mut Host) {
    let now = GetTickCount64();
    let Some(at) = next_watch_retry(s).filter(|at| *at <= now) else {
        return;
    };
    let index = s.watch[..s.roots]
        .iter()
        .position(|w| w.retry_at == at)
        .unwrap();
    let root = s.watch[index].path;
    let path = core::slice::from_raw_parts(text(s, root), root.len + 1).to_vec();
    match watch_open::Attempt::start(path, index, s.watch_epoch) {
        Ok(attempt) => s.watch_open = Some(attempt),
        Err(_) => failed_watch(s, index),
    }
}
unsafe fn finish_watch_retry(s: &mut Host) -> bool {
    let Some(attempt) = s.watch_open.take() else {
        return false;
    };
    let (epoch, index) = (attempt.epoch, attempt.root);
    let opened = attempt.finish();
    if epoch != s.watch_epoch || index >= s.roots {
        return false;
    }
    let Ok(opened) = opened else {
        failed_watch(s, index);
        return false;
    };
    use std::os::windows::io::IntoRawHandle;
    let watch = &mut s.watch[index];
    watch.file = opened.file.into_raw_handle();
    watch.remote = opened.remote;
    watch.extended = true;
    watch.event = CreateEventW(null(), 1, 0, null());
    watch.buffer = VirtualAlloc(
        null(),
        WATCH_BYTES,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_READWRITE,
    )
    .cast();
    if watch.event.is_null() || watch.buffer.is_null() {
        fatal(6);
    }
    if !arm(watch) {
        failed_watch(s, index);
        return false;
    }
    watch.retry_at = 0;
    watch.retry_failures = 0;
    // A journal catch-up or affected-root scan covers the disconnected interval.
    s.dirty |= 1 << index;
    s.generation = s.generation.wrapping_add(1);
    s.cleanup_demand = true;
    true
}
unsafe fn queue_page(s: &mut Host, page: u64) {
    if s.quitting {
        return;
    }
    s.pending_page = page;
    s.generation = s.generation.wrapping_add(1);
    launch(s);
}
unsafe fn reload_config(s: &mut Host) {
    if let Ok(bits) = std::fs::read(common::data_dir().join("host-directories.bin")) {
        if let Ok(bits) = bits.try_into() {
            s.bloom = bits;
        }
    }
    let Ok(config) = common::Config::load() else {
        return;
    };
    let changed = s
        .config
        .as_ref()
        .is_none_or(|old| serde_json::to_vec(old).ok() != serde_json::to_vec(&config).ok());
    let roots = common::host::watch_roots(&config);
    let roots_changed = roots.len() != s.roots
        || roots.iter().enumerate().any(|(i, p)| {
            let old = String::from_utf16_lossy(core::slice::from_raw_parts(
                text(s, s.watch[i].path),
                s.watch[i].path.len,
            ));
            !std::path::Path::new(&old).eq(p)
        });
    s.config = Some(config);
    if roots_changed {
        s.watch_epoch = s.watch_epoch.wrapping_add(1);
        for watch in &mut s.watch[..s.roots] {
            stop_watch(watch);
        }
        s.roots = 0;
        s.pool_used = 0;
        configure(s);
        setup_watches(s);
        s.dirty = (1u64 << s.roots) - 1;
        s.generation = s.generation.wrapping_add(1);
        if s.worker.is_null() {
            if AUTO.load(Ordering::Relaxed) != 0 {
                launch(s);
            }
        } else {
            SetEvent(s.wake);
        }
    }
    if !changed {
        return;
    }
    if s.worker.is_null() {
        launch(s);
    }
}
unsafe fn model(path: *const u16, len: usize) -> bool {
    for suffix in [b".step" as &[u8], b".stp", b".stl", b".glb"] {
        if len < suffix.len() {
            continue;
        }
        let mut equal = true;
        for (i, b) in suffix.iter().enumerate() {
            if lower(*path.add(len - suffix.len() + i)) != *b as u16 {
                equal = false;
                break;
            }
        }
        if equal {
            return true;
        }
    }
    false
}
unsafe fn enqueue(s: &mut Host, root: usize, path: *const u16, len: usize, kind: u32) {
    s.generation = s.generation.wrapping_add(1);
    let bytes = 8 + len * 2;
    let mut offset = 0;
    while offset < s.queue_len {
        let n = core::ptr::read_unaligned(s.queue.add(offset).cast::<u32>()) as usize;
        if n == bytes
            && core::slice::from_raw_parts(s.queue.add(offset + 8), n - 8)
                == core::slice::from_raw_parts(path.cast::<u8>(), len * 2)
        {
            return;
        }
        offset += n;
    }
    if s.queue_len + bytes > QUEUE_BYTES {
        s.overflows += 1;
        s.dirty |= 1 << root;
        return;
    }
    if s.queue.is_null() {
        s.queue = VirtualAlloc(
            null(),
            QUEUE_BYTES,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
        .cast();
        if s.queue.is_null() {
            s.dirty |= 1 << root;
            return;
        }
    }
    let p = s.queue.add(s.queue_len);
    core::ptr::write_unaligned(p.cast::<u32>(), bytes as u32);
    core::ptr::write_unaligned(p.add(4).cast::<u32>(), (root as u32) | kind << 16);
    core::ptr::copy_nonoverlapping(path.cast::<u8>(), p.add(8), len * 2);
    s.queue_len += bytes;
    s.queue_count += 1;
    s.peak_queue = s.peak_queue.max(s.queue_len as u64);
}
unsafe fn watch_event(s: &mut Host, index: usize) -> bool {
    s.batches += 1;
    let mut bytes = 0;
    let w = &mut s.watch[index];
    let good = GetOverlappedResult(w.file, &w.io, &mut bytes, 0) != 0;
    w.pending = false;
    if !good || bytes == 0 {
        s.cleanup_demand = true;
        s.overflows += 1;
        s.dirty |= 1 << index;
        s.generation = s.generation.wrapping_add(1);
        if !arm(w) {
            failed_watch(s, index);
        }
        return true;
    }
    let base = w.buffer;
    let extended = w.extended;
    let remote = w.remote;
    let count = bytes as usize;
    let mut at = 0;
    let mut work = false;
    let header = if extended {
        offset_of!(FILE_NOTIFY_EXTENDED_INFORMATION, FileName)
    } else {
        12
    };
    let root = s.watch[index].path;
    while at + header <= count {
        let p = base.add(at);
        let field = |offset| core::ptr::read_unaligned(p.add(offset).cast::<u32>());
        let next = field(0) as usize;
        let action = field(4);
        let attributes = if extended {
            field(offset_of!(FILE_NOTIFY_EXTENDED_INFORMATION, FileAttributes))
        } else {
            0
        };
        let len = field(if extended {
            offset_of!(FILE_NOTIFY_EXTENDED_INFORMATION, FileNameLength)
        } else {
            8
        }) as usize;
        if !len.is_multiple_of(2) || at + header + len > count {
            s.overflows += 1;
            s.dirty |= 1 << index;
            s.generation = s.generation.wrapping_add(1);
            work = true;
            break;
        }
        s.records += 1;
        let file = p.add(header).cast::<u16>();
        let n = len / 2;
        let is_model = model(file, n);
        let deleted = matches!(action, FILE_ACTION_REMOVED | FILE_ACTION_RENAMED_OLD_NAME);
        let special = [
            b"settings.json" as &[u8],
            b"host-directories.bin",
            b"host-extra-roots.json",
        ]
        .iter()
        .any(|suffix| {
            n >= suffix.len()
                && suffix
                    .iter()
                    .enumerate()
                    .all(|(i, b)| lower(*file.add(n - suffix.len() + i)) == *b as u16)
        });
        let candidate = (attributes & FILE_ATTRIBUTE_DIRECTORY != 0
            && action != FILE_ACTION_MODIFIED)
            || is_model
            || special
            || !extended && action != FILE_ACTION_MODIFIED;
        if candidate {
            let separator = usize::from(*text(s, root).add(root.len - 1) != 92);
            let total = root.len + separator + n;
            if total + 1 > s.scratch.len() {
                s.dirty |= 1 << index;
                s.generation = s.generation.wrapping_add(1);
                work = true;
            } else {
                let dest = s.scratch.as_mut_ptr();
                core::ptr::copy_nonoverlapping(text(s, root), dest, root.len);
                if separator != 0 {
                    *dest.add(root.len) = 92;
                }
                core::ptr::copy_nonoverlapping(file, dest.add(root.len + separator), n);
                *dest.add(total) = 0;
                let path = std::path::PathBuf::from(String::from_utf16_lossy(
                    core::slice::from_raw_parts(dest, total),
                ));
                let key = common::path_key(&path);
                let data = common::data_dir();
                let tracked = !s.worker.is_null() || common::host::bloom_contains(&s.bloom, &key);
                let directory = if extended {
                    attributes & FILE_ATTRIBUTE_DIRECTORY != 0
                } else if deleted {
                    !is_model && tracked
                } else {
                    GetFileAttributesW(dest) & FILE_ATTRIBUTE_DIRECTORY != 0
                };
                let cfg = s.config.as_ref().unwrap();
                let wanted = if common::below(&path, &data) {
                    if path == data.join("settings.json")
                        || path == data.join("host-directories.bin")
                        || path == data.join("host-extra-roots.json")
                    {
                        ACTIONS.fetch_or(RELOAD_CONFIG, Ordering::Relaxed);
                    }
                    false
                } else if directory {
                    if deleted {
                        tracked
                    } else {
                        cfg.included(&path)
                            && (action == FILE_ACTION_RENAMED_NEW_NAME
                                || remote && action == FILE_ACTION_ADDED)
                    }
                } else if is_model {
                    if deleted {
                        tracked
                    } else {
                        cfg.included(&path) && cfg.supports(&path)
                    }
                } else {
                    false
                };
                if wanted {
                    if deleted {
                        s.cleanup_demand = true;
                    }
                    if directory {
                        s.directories += 1;
                    } else {
                        s.models += 1;
                    }
                    enqueue(s, index, dest, total, u32::from(directory));
                    work = true;
                } else {
                    s.ignored += 1;
                }
            }
        } else {
            s.ignored += 1;
        }
        if next == 0 {
            break;
        }
        if next < header + len || at + next > count {
            s.dirty |= 1 << index;
            s.overflows += 1;
            s.generation = s.generation.wrapping_add(1);
            work = true;
            break;
        }
        at += next;
    }
    if !arm(&mut s.watch[index]) {
        failed_watch(s, index);
    }
    work
}
unsafe fn launch(s: &mut Host) {
    if s.quitting {
        return;
    }
    if !s.worker.is_null() {
        if s.stopping {
            s.demand = true;
            return;
        }
        if !s.wake.is_null() {
            SetEvent(s.wake);
        }
        return;
    }
    if s.restart_at > GetTickCount64() {
        return;
    }
    s.restart_at = 0;
    let mut event_name = [0u16; 96];
    name(
        event_name.as_mut_ptr(),
        "Local\\StepThumbs.Host.",
        s.pid,
        ".wake",
    );
    s.wake = CreateEventW(null(), 0, 1, event_name.as_ptr());
    name(
        event_name.as_mut_ptr(),
        "Local\\StepThumbs.Host.",
        s.pid,
        ".ready",
    );
    s.ready = CreateEventW(null(), 1, 0, event_name.as_ptr());
    if s.wake.is_null() || s.ready.is_null() {
        fatal(7);
    }
    s.job = CreateJobObjectW(null(), null());
    if s.job.is_null() {
        fatal(7);
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if SetInformationJobObject(
        s.job,
        JobObjectExtendedLimitInformation,
        (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
        size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
    ) == 0
    {
        fatal(7);
    }
    let executable = std::env::current_exe()
        .unwrap()
        .with_file_name("StepThumbsEngine.exe");
    let mut command = common::wide(format!(
        "\"{}\" --engine {} {}",
        executable.display(),
        s.pid,
        u8::from(s.force_cold)
    ));
    s.force_cold = false;
    s.stopping = false;
    s.demand = false;
    let mut startup: STARTUPINFOW = zeroed();
    startup.cb = size_of::<STARTUPINFOW>() as u32;
    let mut process: PROCESS_INFORMATION = zeroed();
    let start = ticks();
    if CreateProcessW(
        null(),
        command.as_mut_ptr(),
        null(),
        null(),
        0,
        CREATE_NO_WINDOW | CREATE_SUSPENDED,
        null(),
        null(),
        &startup,
        &mut process,
    ) == 0
    {
        fatal(7);
    }
    if AssignProcessToJobObject(s.job, process.hProcess) == 0 {
        TerminateProcess(process.hProcess, 1);
        fatal(7);
    }
    s.worker = process.hProcess;
    s.worker_pid = process.dwProcessId;
    s.worker_since = start;
    s.launch_us = micros(s, ticks() - start);
    s.ready_us = 0;
    s.launches += 1;
    ResumeThread(process.hThread);
    CloseHandle(process.hThread);
}
unsafe fn worker_exit(s: &mut Host) {
    GetExitCodeProcess(s.worker, &mut s.exit_code);
    s.worker_life_us = micros(s, ticks() - s.worker_since);
    for handle in [s.worker, s.job, s.wake, s.ready] {
        if !handle.is_null() {
            CloseHandle(handle);
        }
    }
    s.worker = null_mut();
    s.job = null_mut();
    s.wake = null_mut();
    s.ready = null_mut();
    s.worker_pid = 0;
    s.exits += 1;
    s.stopping = false;
    if s.exit_code != 0 {
        s.force_cold = true;
        s.demand = true;
        s.restart_at = GetTickCount64() + [1000, 2000, 5000, 15000, 30000][s.crash_count.min(4)];
        s.crash_count = s.crash_count.saturating_add(1);
    } else {
        s.crash_count = 0;
        s.restart_at = 0;
    }
    if let Ok(bits) = std::fs::read(common::data_dir().join("host-directories.bin")) {
        if let Ok(bits) = bits.try_into() {
            s.bloom = bits;
        }
    }
    if s.next_page != 0 {
        s.pending_page = s.next_page;
        s.next_page = 0;
        s.generation = s.generation.wrapping_add(1);
    }
    if !s.quitting
        && (s.demand
            || s.pending_page != 0
            || AUTO.load(Ordering::Relaxed) != 0 && (s.queue_count != 0 || s.dirty != 0))
    {
        launch(s);
    }
}

unsafe fn cleanup(s: &mut Host) {
    let menu = MENU_HANDLE.swap(null_mut(), Ordering::Relaxed);
    if !menu.is_null() {
        EndMenu();
        DestroyMenu(menu);
    }
    notify(s, NIM_DELETE);
    if !s.icon.is_null() {
        DestroyIcon(s.icon);
    }
    if s.pipe != INVALID_HANDLE_VALUE {
        if s.pipe_pending {
            CancelIoEx(s.pipe, &s.pipe_io);
            let mut n = 0;
            GetOverlappedResult(s.pipe, &s.pipe_io, &mut n, 1);
        }
        CloseHandle(s.pipe);
        CloseHandle(s.pipe_event);
    }
    free_reply(s);
    for w in &mut s.watch[..s.roots] {
        stop_watch(w);
    }
    clear_queue(s);
    if !s.job.is_null() {
        CloseHandle(s.job);
    }
    for h in [s.worker, s.ready, s.wake] {
        if !h.is_null() {
            CloseHandle(h);
        }
    }
    DestroyWindow(s.window);
}
unsafe fn main_host() -> u32 {
    SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
    // Keep the counter alive while the engine sleeps. DLL hits only increment
    // shared memory; they never send a message or wake this process.
    let _stats = common::shared_stats::SharedStats::create();
    let s = state();
    configure(s);
    {
        // The host has no text entry. Settings live in the engine process.
        let module = LoadLibraryExW(w!("imm32.dll"), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
        if !module.is_null() {
            if let Some(function) = GetProcAddress(module, c"ImmDisableIME".as_ptr().cast()) {
                let disable: unsafe extern "system" fn(u32) -> BOOL =
                    core::mem::transmute(function);
                disable(u32::MAX);
            }
            FreeLibrary(module);
        }
    }
    s.pid = GetCurrentProcessId();
    QueryPerformanceFrequency(&mut s.frequency);
    let instance = GetModuleHandleW(null());
    let class = WNDCLASSW {
        lpfnWndProc: Some(window),
        hInstance: instance,
        lpszClassName: w!("StepThumbsHostWindow"),
        ..zeroed()
    };
    if RegisterClassW(&class) == 0 {
        fatal(3);
    }
    s.window = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        w!("StepThumbsHostWindow"),
        w!("StepThumbs"),
        WS_POPUP,
        0,
        0,
        0,
        0,
        null_mut(),
        null_mut(),
        instance,
        null(),
    );
    if s.window.is_null() {
        fatal(3);
    }
    TASKBAR.store(
        RegisterWindowMessageW(w!("TaskbarCreated")),
        Ordering::Relaxed,
    );
    if !s.no_tray {
        s.icon = load_icon(s);
        if s.icon.is_null() {
            fatal(3);
        }
        notify(s, NIM_ADD);
    }
    open_pipe(s);
    let mut shell = shell_pipe::Listener::new().unwrap_or_else(|_| fatal(4));
    setup_watches(s);
    launch(s);
    loop {
        // Never let a busy volume starve tray messages.
        let mut message: MSG = zeroed();
        for _ in 0..32 {
            if PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) == 0 {
                break;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        let actions = ACTIONS.swap(0, Ordering::Relaxed);
        if actions & QUIT != 0 {
            s.quitting = true;
            s.shutdown_at = GetTickCount64() + 120000;
            if !s.worker.is_null() {
                s.pending_page = 255;
                s.generation = s.generation.wrapping_add(1);
                SetEvent(s.wake);
            }
        }
        if s.quitting && s.worker.is_null() {
            break;
        }
        if actions & OPEN_ACTIVITY != 0 {
            queue_page(s, 1);
        }
        if actions & OPEN_CONFIG != 0 {
            queue_page(s, 3);
        }
        if actions & RELOAD_CONFIG != 0 {
            reload_config(s);
        }
        if actions & CLOSE_MENU != 0 {
            let menu = MENU_HANDLE.swap(null_mut(), Ordering::Relaxed);
            if !menu.is_null() {
                DestroyMenu(menu);
            }
        }
        if actions & PAUSE != 0 {
            AUTO.fetch_xor(1, Ordering::Relaxed);
            if AUTO.load(Ordering::Relaxed) != 0 {
                launch(s);
            } else if !s.wake.is_null() {
                SetEvent(s.wake);
            }
        }
        if actions & WAKE != 0 {
            launch(s);
        }
        if actions & MENU != 0 {
            menu(s.window, true);
        }
        if actions & (REFRESH | READD) != 0 && !s.no_tray {
            let icon = load_icon(s);
            if !icon.is_null() {
                DestroyIcon(s.icon);
                s.icon = icon;
                notify(
                    s,
                    if actions & READD != 0 {
                        NIM_ADD
                    } else {
                        NIM_MODIFY
                    },
                );
            }
        }
        // Enforce deadlines even if a busy volume keeps a wait handle signalled.
        if shell.expire().is_err() {
            fatal(4);
        }
        if s.deadline != 0 && GetTickCount64() >= s.deadline {
            disconnect(s);
        }
        begin_watch_retry(s);
        let mut handles = [null_mut(); MAX_ROOTS + 6];
        let mut kinds = [0u32; MAX_ROOTS + 6];
        let mut n = 0;
        handles[n] = s.pipe_event;
        n += 1;
        handles[n] = shell.event;
        kinds[n] = 200;
        n += 1;
        handles[n] = shell.capacity_event();
        kinds[n] = 201;
        n += 1;
        if let Some(attempt) = &s.watch_open {
            handles[n] = attempt.event();
            kinds[n] = 202;
            n += 1;
        }
        for i in 0..s.roots {
            if s.watch[i].file != INVALID_HANDLE_VALUE {
                handles[n] = s.watch[i].event;
                kinds[n] = 1 + i as u32;
                n += 1;
            }
        }
        if !s.worker.is_null() {
            handles[n] = s.worker;
            kinds[n] = 100;
            n += 1;
        }
        if !s.ready.is_null() {
            handles[n] = s.ready;
            kinds[n] = 101;
            n += 1;
        }
        let mut timeout = if s.deadline == 0 {
            INFINITE
        } else {
            s.deadline.saturating_sub(GetTickCount64()).min(3000) as u32
        };
        if s.quitting {
            timeout = timeout.min(s.shutdown_at.saturating_sub(GetTickCount64()) as u32);
        }
        if shell.deadline != 0 {
            timeout = timeout.min(shell.deadline.saturating_sub(GetTickCount64()) as u32);
        }
        if s.worker.is_null() && s.restart_at != 0 && !s.quitting {
            timeout = timeout.min(s.restart_at.saturating_sub(GetTickCount64()) as u32);
        }
        if let Some(at) = next_watch_retry(s) {
            timeout =
                timeout.min(at.saturating_sub(GetTickCount64()).min(u32::MAX as u64 - 1) as u32);
        }
        let result = MsgWaitForMultipleObjectsEx(
            n as u32,
            handles.as_ptr(),
            timeout,
            QS_ALLINPUT,
            MWMO_INPUTAVAILABLE,
        );
        s.wakeups += 1;
        let mut work = false;
        if result == WAIT_TIMEOUT {
            if !s.quitting
                && s.worker.is_null()
                && s.restart_at != 0
                && GetTickCount64() >= s.restart_at
            {
                launch(s);
            }
            if shell.expire().is_err() {
                fatal(4);
            }
            if s.quitting && GetTickCount64() >= s.shutdown_at {
                break;
            }
            if s.deadline != 0 && GetTickCount64() >= s.deadline {
                disconnect(s);
            }
        } else if result == WAIT_FAILED {
            fatal(9);
        } else if result < n as u32 {
            match kinds[result as usize] {
                0 => pipe_event(s),
                100 => worker_exit(s),
                200 => {
                    if shell.event().is_err() {
                        fatal(4);
                    }
                }
                201 => {
                    if shell.capacity_changed().is_err() {
                        fatal(4);
                    }
                }
                202 => work = finish_watch_retry(s),
                101 => {
                    s.ready_us = micros(s, ticks() - s.worker_since);
                    CloseHandle(s.ready);
                    s.ready = null_mut();
                }
                k => work = watch_event(s, (k - 1) as usize),
            }
        } else {
            let mut msg: MSG = zeroed();
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if work && (AUTO.load(Ordering::Relaxed) != 0 || s.cleanup_demand) {
            s.cleanup_demand = false;
            launch(s);
        }
    }
    drop(shell);
    cleanup(s);
    0
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let arg = args.first().map(String::as_str).unwrap_or("");
    let exe = std::env::current_exe().unwrap();
    if matches!(
        arg,
        "--render-test"
            | "--snapshot"
            | "--snapshot-filetypes"
            | "--promotion-test"
            | "--hooks-self-test"
            | "--restore-hooks"
    ) {
        use std::os::windows::process::CommandExt;
        let code = std::process::Command::new(exe.with_file_name("StepThumbsEngine.exe"))
            .args(&args)
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map(|v| v.code().unwrap_or(1))
            .unwrap_or(1);
        std::process::exit(code);
    }
    if arg == "--status" {
        unsafe {
            windows_sys::Win32::System::Console::AttachConsole(u32::MAX);
        }
        match common::ipc::call(&common::ipc::Request::Status, None) {
            Ok(v) => println!("{}", serde_json::to_string_pretty(&v.status).unwrap()),
            Err(e) => {
                eprintln!("{e:#}");
                std::process::exit(1);
            }
        }
        return;
    }
    let op = match arg {
        "--background" => common::host::STATUS,
        "--wake" => common::host::WAKE,
        "--shutdown" => common::host::QUIT,
        "--pause" | "--resume" => common::host::TOGGLE,
        "--file-types" => common::host::CONFIGURE,
        _ => common::host::SHOW,
    };
    if let Ok((header, _)) = common::host::request(common::host::STATUS, 0) {
        if (arg == "--pause" && header[4] & 1 == 0) || (arg == "--resume" && header[4] & 1 != 0) {
            return;
        }
        let _ = common::host::request(op, 0);
        if op == common::host::QUIT {
            unsafe {
                let process = OpenProcess(SYNCHRONIZE, 0, header[3] as u32);
                if !process.is_null() {
                    WaitForSingleObject(process, 125000);
                    CloseHandle(process);
                }
            }
        }
        return;
    }
    if arg == "--shutdown" {
        if common::ipc::exists() {
            let _ = common::ipc::call(&common::ipc::Request::Shutdown, None);
        }
        return;
    }
    unsafe {
        let mutex = CreateMutexW(
            null(),
            0,
            common::wide(format!(
                "Local\\{}",
                common::host::pipe_name().replace('\\', "_")
            ))
            .as_ptr(),
        );
        if mutex.is_null() {
            fatal(2);
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = common::host::ensure(op, None);
            return;
        }
        // A provider already loaded by Explorer may still use the old public
        // pipe. Retire its broker before taking over that endpoint.
        if common::ipc::exists() {
            let _ = common::ipc::call(&common::ipc::Request::Shutdown, None);
        }
        if arg == "--pause" {
            AUTO.store(0, Ordering::Relaxed);
        }
        let s = state();
        s.pending_page = if op == common::host::SHOW {
            1
        } else if op == common::host::CONFIGURE {
            3
        } else {
            0
        };
        let result = main_host();
        CloseHandle(mutex);
        std::process::exit(result as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_watch_recovers_and_removes_its_retry_timer() {
        let folder = std::env::temp_dir().join(format!(
            "StepThumbs-watch-recovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(!folder.exists());
        unsafe {
            // This executable has one host-state test; no tray or renderer runs.
            let s = state();
            let path = common::wide(&folder);
            s.watch[0].path = copy_text(s, path.as_ptr(), path.len() - 1);
            s.roots = 1;
            let config = common::Config {
                roots: vec![folder.to_string_lossy().into_owned()],
                exclude: Vec::new(),
                ..Default::default()
            };
            s.config = Some(config);
            setup_watches(s);
            begin_watch_retry(s);
            assert_eq!(
                WaitForSingleObject(s.watch_open.as_ref().unwrap().event(), 5000),
                WAIT_OBJECT_0
            );
            assert!(!finish_watch_retry(s));
            assert_eq!(s.watch[0].file, INVALID_HANDLE_VALUE);
            assert_eq!(s.watch[0].retry_failures, 1);
            assert!(next_watch_retry(s).unwrap() > GetTickCount64());

            std::fs::create_dir(&folder).unwrap();
            s.watch[0].retry_at = GetTickCount64().max(1);
            begin_watch_retry(s);
            assert_eq!(
                WaitForSingleObject(s.watch_open.as_ref().unwrap().event(), 5000),
                WAIT_OBJECT_0
            );
            assert!(finish_watch_retry(s));
            assert_ne!(s.watch[0].file, INVALID_HANDLE_VALUE);
            assert_eq!(s.watch[0].retry_failures, 0);
            assert_eq!(next_watch_retry(s), None);
            assert_eq!(s.dirty, 1); // missed changes require reconciliation

            let model = folder.join("new.step");
            std::fs::write(&model, b"filesystem notification test").unwrap();
            assert_eq!(WaitForSingleObject(s.watch[0].event, 5000), WAIT_OBJECT_0);
            assert!(watch_event(s, 0));
            assert!(s.queue_count > 0);
            stop_watch(&mut s.watch[0]);
            clear_queue(s);
            std::fs::remove_file(model).unwrap();
            std::fs::remove_dir(folder).unwrap();
        }
    }
}
