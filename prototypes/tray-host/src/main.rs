#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(not(feature = "std"), no_main)]
#![windows_subsystem = "windows"]
#![allow(unsafe_op_in_unsafe_fn)]

//! Isolated memory/lifecycle experiment; does not install hooks or replace StepThumbs.
//! Build here with `cargo build --release`. Default: tray, one overlapped pipe,
//! recursive fixed-drive notifications, and a reusable stub worker (60 s idle exit).
//! `--watch PATH` / `--exclude PATH` repeat; `--no-auto` only queues changes.
//! `--engine-command CMDLINE` launches a real engine instead of the lifecycle stub.
//! `--no-watch`, `--no-tray`, `--worker-idle-ms N` are measurement controls.
//! Comparison features: std, resident-shell, png-icon, segment-heap; --no-default-features
//! also enables text input infrastructure, which this host never needs.
//!
//! Pipe: \\.\pipe\StepThumbs.HostProbe.<PID>, current-user ACL, local clients only.
//! Request: LE u32 magic 0x53544850, u32 opcode, u64 argument. Reply: 32 LE u64s
//! (see snapshot), then payload. Opcodes: status=1, quit=2, wake=3, take=4,
//! acknowledge generation=5, toggle auto=6, kill worker=7, menu probe=8.
//! TAKE records: u32 byte size, u16 root, u16 directory action (0=file), UTF-16 path.
//! Files need a metadata probe; directory records need only targeted prefix work.
//! A reconciliation mask means notifications overflowed, not a routine root scan.
//! The engine must persist checkpoints and acknowledge only after durable acceptance.
//! This prototype intentionally has no model index, SQLite, render code, or settings UI.

use core::{
    mem::{offset_of, size_of, zeroed},
    ptr::{addr_of_mut, null, null_mut},
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};
use windows_sys::Win32::{
    Foundation::*,
    Globalization::*,
    Security::*,
    Storage::FileSystem::*,
    System::{
        Diagnostics::Debug::*, Environment::*, IO::*, JobObjects::*, LibraryLoader::*, Memory::*,
        Performance::*, Pipes::*, Registry::*, SystemInformation::*, SystemServices::*,
        Threading::*, WindowsProgramming::*,
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
const MAX_ROOTS: usize = 8;
const MAX_EXCLUDES: usize = 16;
const QUIT: u32 = 1;
const WAKE: u32 = 2;
const PAUSE: u32 = 4;
const REFRESH: u32 = 8;
const READD: u32 = 16;
const MENU: u32 = 32;
const CLOSE_MENU: u32 = 64;
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
    buffer: Block<WATCH_BYTES>,
}
impl Watch {
    const EMPTY: Self = Self {
        path: Text::NONE,
        file: INVALID_HANDLE_VALUE,
        event: null_mut(),
        io: unsafe { zeroed() },
        buffer: Block([0; WATCH_BYTES]),
    };
}
struct Host {
    window: HWND,
    icon: HICON,
    roots: usize,
    excludes: usize,
    pool: [u16; 4096],
    pool_used: usize,
    exclude: [Text; MAX_EXCLUDES],
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
    worker_idle: u32,
    executable: Text,
    engine: Text,
    no_tray: bool,
    no_watch: bool,
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
}
static mut HOST: Host = Host {
    window: null_mut(),
    icon: null_mut(),
    roots: 0,
    excludes: 0,
    pool: [0; 4096],
    pool_used: 0,
    exclude: [Text::NONE; MAX_EXCLUDES],
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
    worker_idle: 60000,
    executable: Text::NONE,
    engine: Text::NONE,
    no_tray: false,
    no_watch: false,
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
};
unsafe fn state() -> &'static mut Host {
    &mut *addr_of_mut!(HOST)
}
unsafe fn length(p: *const u16) -> usize {
    let mut n = 0;
    while n < 32768 && *p.add(n) != 0 {
        n += 1;
    }
    n
}
unsafe fn equals(p: *const u16, s: &str) -> bool {
    let b = s.as_bytes();
    for (i, v) in b.iter().enumerate() {
        if *p.add(i) != *v as u16 {
            return false;
        }
    }
    *p.add(b.len()) == 0
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

// Windows CRT quoting rules, without CommandLineToArgvW/Shell32 or an allocator.
unsafe fn argument(line: *const u16, position: &mut usize, out: *mut u16) -> bool {
    while *line.add(*position) == 32 || *line.add(*position) == 9 {
        *position += 1;
    }
    if *line.add(*position) == 0 {
        return false;
    }
    let mut quoted = false;
    let mut n = 0;
    loop {
        let c = *line.add(*position);
        if c == 0 || (!quoted && (c == 32 || c == 9)) {
            break;
        }
        if c == 92 {
            let mut slashes = 0;
            while *line.add(*position) == 92 {
                slashes += 1;
                *position += 1;
            }
            if *line.add(*position) == 34 {
                for _ in 0..slashes / 2 {
                    *out.add(n) = 92;
                    n += 1;
                }
                if slashes % 2 == 1 {
                    *out.add(n) = 34;
                    n += 1;
                } else {
                    quoted = !quoted;
                }
                *position += 1;
            } else {
                for _ in 0..slashes {
                    *out.add(n) = 92;
                    n += 1;
                }
            }
        } else if c == 34 {
            quoted = !quoted;
            *position += 1;
        } else {
            *out.add(n) = c;
            n += 1;
            *position += 1;
        }
        if n >= 32767 {
            fatal(2);
        }
    }
    *out.add(n) = 0;
    true
}
unsafe fn number(p: *const u16) -> u32 {
    let mut n = 0u32;
    let mut i = 0;
    while *p.add(i) != 0 {
        let v = *p.add(i);
        if !(48..=57).contains(&v) {
            fatal(2);
        }
        n = n
            .checked_mul(10)
            .and_then(|n| n.checked_add((v - 48) as u32))
            .unwrap_or_else(|| fatal(2));
        i += 1;
    }
    n
}
unsafe fn configure(s: &mut Host) -> Option<(u32, u32)> {
    let line = GetCommandLineW();
    let mut at = 0;
    argument(line, &mut at, s.scratch.as_mut_ptr());
    while argument(line, &mut at, s.scratch.as_mut_ptr()) {
        let p = s.scratch.as_ptr();
        if equals(p, "--worker-stub") {
            if !argument(line, &mut at, s.scratch.as_mut_ptr()) {
                fatal(2);
            }
            let owner = number(s.scratch.as_ptr());
            if !argument(line, &mut at, s.scratch.as_mut_ptr()) {
                fatal(2);
            }
            return Some((owner, number(s.scratch.as_ptr())));
        }
        let kind = if equals(p, "--watch") {
            1
        } else if equals(p, "--exclude") {
            2
        } else if equals(p, "--engine-command") {
            3
        } else if equals(p, "--worker-idle-ms") {
            4
        } else {
            0
        };
        if kind != 0 {
            if !argument(line, &mut at, s.scratch.as_mut_ptr()) {
                fatal(2);
            }
            if kind == 4 {
                s.worker_idle = number(s.scratch.as_ptr()).clamp(100, 600000);
                continue;
            }
            let t = copy_text(s, s.scratch.as_ptr(), length(s.scratch.as_ptr()));
            match kind {
                1 => {
                    if s.roots == MAX_ROOTS {
                        fatal(2);
                    }
                    s.watch[s.roots].path = t;
                    s.roots += 1;
                }
                2 => {
                    if s.excludes == MAX_EXCLUDES {
                        fatal(2);
                    }
                    s.exclude[s.excludes] = t;
                    s.excludes += 1;
                }
                _ => s.engine = t,
            };
        } else if equals(p, "--no-auto") {
            AUTO.store(0, Ordering::Relaxed);
        } else if equals(p, "--no-tray") {
            s.no_tray = true;
        } else if equals(p, "--no-watch") {
            s.no_watch = true;
        } else {
            fatal(2);
        }
    }
    if s.roots == 0 && !s.no_watch {
        let drives = GetLogicalDrives();
        for drive in 0..26 {
            if drives & (1 << drive) == 0 {
                continue;
            }
            let root = [65 + drive as u16, 58, 92, 0];
            if GetDriveTypeW(root.as_ptr()) == DRIVE_FIXED {
                let t = copy_text(s, root.as_ptr(), 3);
                s.watch[s.roots].path = t;
                s.roots += 1;
                if s.roots == MAX_ROOTS {
                    break;
                }
            }
        }
    }
    let n = GetModuleFileNameW(null_mut(), s.scratch.as_mut_ptr(), s.scratch.len() as u32) as usize;
    if n == 0 || n >= s.scratch.len() {
        fatal(3);
    }
    s.executable = copy_text(s, s.scratch.as_ptr(), n);
    None
}

#[cfg(not(feature = "resident-shell"))]
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
    let tip = b"StepThumbs Host Prototype";
    for (i, b) in tip.iter().enumerate() {
        data.szTip[i] = *b as u16;
    }
    #[cfg(feature = "resident-shell")]
    {
        windows_sys::Win32::UI::Shell::Shell_NotifyIconW(action, &data);
    }
    #[cfg(not(feature = "resident-shell"))]
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
        #[cfg(feature = "png-icon")]
        const B: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/", $theme, "-", $size, ".png"));
        #[cfg(not(feature = "png-icon"))]
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
    AppendMenuW(menu, MF_STRING, 10, w!("Wake worker (prototype)"));
    AppendMenuW(menu, MF_STRING, 11, w!("Configure (prototype worker)"));
    AppendMenuW(
        menu,
        MF_STRING,
        12,
        if AUTO.load(Ordering::Relaxed) != 0 {
            w!("Pause filesystem work")
        } else {
            w!("Resume filesystem work")
        },
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, 13, w!("Exit prototype"));
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
                ACTIONS.fetch_or(WAKE, Ordering::Relaxed);
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
                    10 | 11 => WAKE,
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
    let mut pipe_name = [0u16; 96];
    name(
        pipe_name.as_mut_ptr(),
        "\\\\.\\pipe\\StepThumbs.HostProbe.",
        s.pid,
        "",
    );
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
    let flags = (AUTO.load(Ordering::Relaxed) as u64)
        | (((!s.worker.is_null()) as u64) << 1)
        | ((cfg!(feature = "std") as u64) << 2)
        | ((cfg!(feature = "resident-shell") as u64) << 3)
        | ((cfg!(feature = "png-icon") as u64) << 4)
        | ((cfg!(feature = "segment-heap") as u64) << 5);
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
        (!MENU_HANDLE.load(Ordering::Relaxed).is_null()) as u64,
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

unsafe fn arm(w: &mut Watch) {
    ResetEvent(w.event);
    w.io = zeroed();
    w.io.hEvent = w.event;
    if ReadDirectoryChangesExW(
        w.file,
        w.buffer.0.as_mut_ptr().cast(),
        WATCH_BYTES as u32,
        1,
        FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE,
        null_mut(),
        &mut w.io,
        None,
        ReadDirectoryNotifyExtendedInformation,
    ) == 0
    {
        fatal(6);
    }
}
unsafe fn setup_watches(s: &mut Host) {
    for i in 0..s.roots {
        let path = text(s, s.watch[i].path);
        let file = CreateFileW(
            path,
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            null_mut(),
        );
        if file == INVALID_HANDLE_VALUE {
            fatal(6);
        }
        let event = CreateEventW(null(), 1, 0, null());
        if event.is_null() {
            fatal(6);
        }
        s.watch[i].file = file;
        s.watch[i].event = event;
        arm(&mut s.watch[i]);
    }
}
unsafe fn excluded(s: &Host, path: *const u16, len: usize) -> bool {
    for t in &s.exclude[..s.excludes] {
        if t.len > len {
            continue;
        }
        if CompareStringOrdinal(path, t.len as i32, text(s, *t), t.len as i32, 1) == CSTR_EQUAL
            && (t.len == len || *path.add(t.len) == 92 || *text(s, *t).add(t.len - 1) == 92)
        {
            return true;
        }
    }
    false
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
    let mut bytes = 0;
    let w = &mut s.watch[index];
    s.batches += 1;
    let good = GetOverlappedResult(w.file, &w.io, &mut bytes, 0) != 0;
    if !good || bytes == 0 {
        s.overflows += 1;
        s.dirty |= 1 << index;
        s.generation += 1;
        arm(w);
        return true;
    }
    let base = w.buffer.0.as_ptr();
    let count = bytes as usize;
    let mut at = 0;
    let mut work = false;
    let header = offset_of!(FILE_NOTIFY_EXTENDED_INFORMATION, FileName);
    let root = s.watch[index].path;
    while at + header <= count {
        let p = base.add(at);
        let field = |offset| core::ptr::read_unaligned(p.add(offset).cast::<u32>());
        let next = field(0) as usize;
        let action = field(4);
        let attributes = field(offset_of!(FILE_NOTIFY_EXTENDED_INFORMATION, FileAttributes));
        let len = field(offset_of!(FILE_NOTIFY_EXTENDED_INFORMATION, FileNameLength)) as usize;
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
        let directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        // Child changes already have their own records. A directory's last-write
        // notification must not wake an engine or trigger a subtree scan.
        if (directory && action == FILE_ACTION_MODIFIED) || (!directory && !model(file, n)) {
            s.ignored += 1;
        } else {
            let separator = if *text(s, root).add(root.len - 1) == 92 {
                0
            } else {
                1
            };
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
                if excluded(s, dest, total) {
                    s.ignored += 1;
                } else {
                    if directory {
                        s.directories += 1;
                    } else {
                        s.models += 1;
                    }
                    enqueue(s, index, dest, total, if directory { action } else { 0 });
                    work = true;
                }
            }
        }
        if next == 0 {
            break;
        }
        if next < header + len || at + next > count {
            s.dirty |= 1 << index;
            s.generation = s.generation.wrapping_add(1);
            s.overflows += 1;
            work = true;
            break;
        }
        at += next;
    }
    arm(&mut s.watch[index]);
    work
}

unsafe fn launch(s: &mut Host) {
    if !s.worker.is_null() {
        if !s.wake.is_null() {
            SetEvent(s.wake);
        }
        return;
    }
    let mut event_name = [0u16; 96];
    name(
        event_name.as_mut_ptr(),
        "Local\\StepThumbs.HostProbe.",
        s.pid,
        ".wake",
    );
    s.wake = CreateEventW(null(), 0, 1, event_name.as_ptr());
    name(
        event_name.as_mut_ptr(),
        "Local\\StepThumbs.HostProbe.",
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
    if s.engine.len != 0 {
        core::ptr::copy_nonoverlapping(text(s, s.engine), s.scratch.as_mut_ptr(), s.engine.len + 1);
    } else {
        let mut n = 0;
        *s.scratch.as_mut_ptr().add(n) = 34;
        n += 1;
        core::ptr::copy_nonoverlapping(
            text(s, s.executable),
            s.scratch.as_mut_ptr().add(n),
            s.executable.len,
        );
        n += s.executable.len;
        *s.scratch.as_mut_ptr().add(n) = 34;
        n += 1;
        for b in b" --worker-stub " {
            *s.scratch.as_mut_ptr().add(n) = *b as u16;
            n += 1;
        }
        name(s.scratch.as_mut_ptr().add(n), "", s.pid, " ");
        n = length(s.scratch.as_ptr());
        name(s.scratch.as_mut_ptr().add(n), "", s.worker_idle, "");
    }
    let mut startup: STARTUPINFOW = zeroed();
    startup.cb = size_of::<STARTUPINFOW>() as u32;
    let mut process: PROCESS_INFORMATION = zeroed();
    let start = ticks();
    if CreateProcessW(
        null(),
        s.scratch.as_mut_ptr(),
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
    if s.engine.len != 0 {
        CloseHandle(s.ready);
        s.ready = null_mut();
    }
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
}

unsafe fn request(owner: u32, op: u32, arg: u64, header: &mut [u64; 32]) -> bool {
    let mut pipe_name = [0u16; 96];
    name(
        pipe_name.as_mut_ptr(),
        "\\\\.\\pipe\\StepThumbs.HostProbe.",
        owner,
        "",
    );
    WaitNamedPipeW(pipe_name.as_ptr(), 2000);
    let pipe = CreateFileW(
        pipe_name.as_ptr(),
        GENERIC_READ | GENERIC_WRITE,
        0,
        null(),
        OPEN_EXISTING,
        0,
        null_mut(),
    );
    if pipe == INVALID_HANDLE_VALUE {
        return false;
    }
    let command = [MAGIC | ((op as u64) << 32), arg];
    let mut n = 0;
    let mut ok = WriteFile(pipe, command.as_ptr().cast(), 16, &mut n, null_mut()) != 0 && n == 16;
    let mut filled = 0;
    while ok && filled < HEADER {
        ok = ReadFile(
            pipe,
            (header.as_mut_ptr() as *mut u8).add(filled),
            (HEADER - filled) as u32,
            &mut n,
            null_mut(),
        ) != 0
            && n > 0;
        filled += n as usize;
    }
    if ok {
        let mut left = header[26] as usize;
        let mut buffer = [0u8; 256];
        while left > 0 {
            let bytes = left.min(buffer.len());
            if ReadFile(pipe, buffer.as_mut_ptr(), bytes as u32, &mut n, null_mut()) == 0 || n == 0
            {
                ok = false;
                break;
            }
            left -= n as usize;
        }
    }
    CloseHandle(pipe);
    ok && header[0] == MAGIC
}
unsafe fn stub(owner: u32, idle: u32) -> u32 {
    let mut event = [0u16; 96];
    name(
        event.as_mut_ptr(),
        "Local\\StepThumbs.HostProbe.",
        owner,
        ".wake",
    );
    let wake = OpenEventW(SYNCHRONIZE, 0, event.as_ptr());
    name(
        event.as_mut_ptr(),
        "Local\\StepThumbs.HostProbe.",
        owner,
        ".ready",
    );
    let ready = OpenEventW(EVENT_MODIFY_STATE, 0, event.as_ptr());
    if wake.is_null() || ready.is_null() {
        return 8;
    }
    SetEvent(ready);
    CloseHandle(ready);
    let mut header = [0u64; 32];
    while WaitForSingleObject(wake, idle) == WAIT_OBJECT_0 {
        if request(owner, 4, 0, &mut header) {
            let generation = header[15];
            request(owner, 5, generation, &mut header);
        }
    }
    CloseHandle(wake);
    0
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
        if w.file != INVALID_HANDLE_VALUE {
            CancelIoEx(w.file, &w.io);
            let mut n = 0;
            GetOverlappedResult(w.file, &w.io, &mut n, 1);
            CloseHandle(w.file);
            CloseHandle(w.event);
        }
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
    let s = state();
    if let Some((owner, idle)) = configure(s) {
        return stub(owner, idle);
    }
    #[cfg(feature = "disable-ime")]
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
        lpszClassName: w!("StepThumbsHostPrototype"),
        ..zeroed()
    };
    if RegisterClassW(&class) == 0 {
        fatal(3);
    }
    s.window = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        w!("StepThumbsHostPrototype"),
        w!("StepThumbs Host Prototype"),
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
    setup_watches(s);
    loop {
        let actions = ACTIONS.swap(0, Ordering::Relaxed);
        if actions & QUIT != 0 {
            break;
        }
        if actions & CLOSE_MENU != 0 {
            let menu = MENU_HANDLE.swap(null_mut(), Ordering::Relaxed);
            if !menu.is_null() {
                DestroyMenu(menu);
            }
        }
        if actions & PAUSE != 0 {
            AUTO.fetch_xor(1, Ordering::Relaxed);
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
        let mut handles = [null_mut(); 12];
        let mut kinds = [0u32; 12];
        let mut n = 0;
        handles[n] = s.pipe_event;
        n += 1;
        for i in 0..s.roots {
            handles[n] = s.watch[i].event;
            kinds[n] = 1 + i as u32;
            n += 1;
        }
        if !s.worker.is_null() {
            handles[n] = s.worker;
            kinds[n] = 20;
            n += 1;
        }
        if !s.ready.is_null() {
            handles[n] = s.ready;
            kinds[n] = 21;
            n += 1;
        }
        let timeout = if s.deadline == 0 {
            INFINITE
        } else {
            s.deadline.saturating_sub(GetTickCount64()).min(3000) as u32
        };
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
            disconnect(s);
        } else if result == WAIT_FAILED {
            fatal(9);
        } else if result < n as u32 {
            match kinds[result as usize] {
                0 => pipe_event(s),
                20 => worker_exit(s),
                21 => {
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
        if work && AUTO.load(Ordering::Relaxed) != 0 {
            launch(s);
        }
    }
    cleanup(s);
    0
}

#[cfg(feature = "std")]
fn main() {
    unsafe {
        ExitProcess(main_host());
    }
}
#[cfg(not(feature = "std"))]
#[unsafe(no_mangle)]
/// # Safety
/// Called once by the Windows executable loader, on the initial process thread.
pub unsafe extern "system" fn host_entry() -> ! {
    let code = main_host();
    ExitProcess(code)
}
#[cfg(not(feature = "std"))]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    unsafe { ExitProcess(101) }
}

// No CRT: volatile byte operations prevent recursive LLVM memory intrinsics.
#[cfg(not(feature = "std"))]
#[unsafe(no_mangle)]
/// # Safety
/// `dst` must be writable for `n` bytes, as for the C memory intrinsic.
pub unsafe extern "C" fn memset(
    dst: *mut core::ffi::c_void,
    value: i32,
    n: usize,
) -> *mut core::ffi::c_void {
    let bytes = dst.cast::<u8>();
    for i in 0..n {
        bytes.add(i).write_volatile(value as u8);
    }
    dst
}
#[cfg(not(feature = "std"))]
#[unsafe(no_mangle)]
/// # Safety
/// Both regions must be valid for `n` bytes and must not overlap.
pub unsafe extern "C" fn memcpy(
    dst: *mut core::ffi::c_void,
    src: *const core::ffi::c_void,
    n: usize,
) -> *mut core::ffi::c_void {
    let (out, input) = (dst.cast::<u8>(), src.cast::<u8>());
    for i in 0..n {
        out.add(i).write_volatile(input.add(i).read_volatile());
    }
    dst
}
#[cfg(not(feature = "std"))]
#[unsafe(no_mangle)]
/// # Safety
/// Both regions must be valid for `n` bytes; overlap is permitted.
pub unsafe extern "C" fn memmove(
    dst: *mut core::ffi::c_void,
    src: *const core::ffi::c_void,
    n: usize,
) -> *mut core::ffi::c_void {
    if (dst as usize) < src as usize {
        memcpy(dst, src, n);
    } else {
        let (out, input) = (dst.cast::<u8>(), src.cast::<u8>());
        for i in (0..n).rev() {
            out.add(i).write_volatile(input.add(i).read_volatile());
        }
    }
    dst
}
#[cfg(not(feature = "std"))]
#[unsafe(no_mangle)]
/// # Safety
/// Both regions must be readable for `n` bytes.
pub unsafe extern "C" fn memcmp(
    a: *const core::ffi::c_void,
    b: *const core::ffi::c_void,
    n: usize,
) -> i32 {
    let (a, b) = (a.cast::<u8>(), b.cast::<u8>());
    for i in 0..n {
        let (a, b) = (a.add(i).read_volatile(), b.add(i).read_volatile());
        if a != b {
            return a as i32 - b as i32;
        }
    }
    0
}
