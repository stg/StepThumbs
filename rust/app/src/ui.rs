#![allow(unsafe_op_in_unsafe_fn)]
use crate::{Broker, Config, common};
mod forms;
mod layout;
use forms::*;
use std::{
    ptr::{null, null_mut},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicIsize, Ordering},
    },
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::{
        LibraryLoader::GetModuleHandleW,
        SystemServices::{SS_OWNERDRAW, SS_PATHELLIPSIS},
    },
    UI::{
        Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled},
        Shell::*,
        WindowsAndMessaging::*,
    },
};
static BROKER: OnceLock<Arc<Broker>> = OnceLock::new();
static WINDOW: AtomicIsize = AtomicIsize::new(0);
static QUIT_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static UPDATE_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static mut FONT: HFONT = null_mut();
static mut TRAY_ICON: HICON = null_mut();
static mut APP_ICON: HICON = null_mut();
static SNAPSHOT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static INITIAL_PAGE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static SNAPSHOT_DPI: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(96);
static COMPACT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const TRAY: u32 = WM_APP + 1;
const UPDATE: u32 = WM_APP + 2;
const SHOW: u32 = WM_APP + 3;
const QUIT: u32 = WM_APP + 4;
const PREVIEW_READY: u32 = WM_APP + 5;
pub fn preview_ready() {
    let hwnd = WINDOW.load(Ordering::Acquire) as HWND;
    if !hwnd.is_null() {
        unsafe {
            PostMessageW(hwnd, PREVIEW_READY, 0, 0);
        }
    }
}
fn restart(page: usize) {
    let broker = BROKER.get().unwrap().clone();
    std::thread::spawn(move || {
        broker.drain();
        use std::os::windows::process::CommandExt;
        if let Ok(exe) = std::env::current_exe() {
            if std::process::Command::new(exe)
                .args([
                    "--wait-for",
                    &std::process::id().to_string(),
                    &page.to_string(),
                ])
                .creation_flags(0x08000000)
                .spawn()
                .is_ok()
            {
                quit();
            }
        }
    });
}
pub fn quit() {
    QUIT_REQUESTED.store(true, Ordering::Release);
    let h = WINDOW.load(Ordering::Acquire) as HWND;
    if !h.is_null() {
        unsafe {
            PostMessageW(h, QUIT, 0, 0);
        }
    }
}
pub fn show() {
    let h = WINDOW.load(Ordering::Acquire) as HWND;
    if !h.is_null() {
        unsafe {
            PostMessageW(h, SHOW, 0, 0);
        }
    }
}
pub fn update() {
    let h = WINDOW.load(Ordering::Acquire) as HWND;
    if !h.is_null() && !UPDATE_PENDING.swap(true, Ordering::AcqRel) {
        unsafe {
            if PostMessageW(h, UPDATE, 0, 0) == 0 {
                UPDATE_PENDING.store(false, Ordering::Release);
            }
        }
    }
}
unsafe fn control(
    parent: HWND,
    kind: &str,
    text: &str,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: usize,
) -> HWND {
    let logical = [x, y, w, h];
    let [x, y, w, h] = layout::rect(logical);
    let window = CreateWindowExW(
        0,
        common::wide(kind).as_ptr(),
        common::wide(text).as_ptr(),
        WS_CHILD
            | WS_VISIBLE
            | style
            | if matches!(
                kind,
                "BUTTON" | "EDIT" | "COMBOBOX" | "LISTBOX" | "SysTabControl32"
            ) {
                WS_TABSTOP
            } else {
                0
            },
        x,
        y,
        w,
        h,
        parent,
        id as HMENU,
        GetModuleHandleW(null()),
        null(),
    );
    layout::add(window, logical);
    if !FONT.is_null() {
        SendMessageW(window, WM_SETFONT, FONT as usize, 1);
    }
    window
}
unsafe fn settext(h: HWND, text: &str) {
    SetWindowTextW(h, common::wide(text).as_ptr());
}
unsafe fn get(h: HWND) -> String {
    let n = GetWindowTextLengthW(h).max(0) as usize;
    let mut buf = vec![0u16; n + 1];
    GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
    String::from_utf16_lossy(&buf[..n])
}
unsafe fn check(h: HWND) -> bool {
    SendMessageW(h, BM_GETCHECK, 0, 0) == 1isize
}
unsafe fn checked(h: HWND, v: bool) {
    SendMessageW(h, BM_SETCHECK, if v { 1usize } else { 0usize }, 0);
}
struct Controls {
    pages: [Vec<HWND>; 4],
    labels: HWND,
    recent: HWND,
    roots: HWND,
    exclude: HWND,
    tabs: HWND,
    remove_paths: [HWND; 2],
    limit: HWND,
    workers: HWND,
    max_file: HWND,
    startup: HWND,
    hooks: [HWND; 4],
    render_fields: Vec<HWND>,
    render_checks: Vec<HWND>,
    render_labels: Vec<HWND>,
    render_hint: HWND,
    preview_box: HWND,
    preview_status: HWND,
    preview: crate::preview::Preview,
    preview_path: Option<std::path::PathBuf>,
    preview_revision: u64,
    preview_pixels: Option<Vec<u8>>,
}
static mut STATE: *mut Controls = null_mut();
unsafe fn initialize_controls(hwnd: HWND) {
    if !STATE.is_null() {
        return;
    }
    FONT = layout::font();
    let cfg = Config::load().unwrap_or_default();
    STATE = Box::into_raw(Box::new(forms::build(hwnd, &cfg)));
    enable_rendering();
    page(INITIAL_PAGE.load(Ordering::Relaxed));
    status();
}
unsafe fn page(index: usize) {
    if STATE.is_null() {
        return;
    }
    for (i, controls) in (*STATE).pages.iter().enumerate() {
        for h in controls {
            ShowWindow(*h, if i == index { SW_SHOW } else { SW_HIDE });
        }
    }
    SendMessageW(
        (*STATE).tabs,
        windows_sys::Win32::UI::Controls::TCM_SETCURSEL,
        index,
        0,
    );
    let hwnd = GetParent((*STATE).tabs);
    if index == 3 {
        schedule_preview(hwnd);
    } else {
        release_preview(hwnd);
    }
}
unsafe fn schedule_preview(hwnd: HWND) {
    if !STATE.is_null() && IsWindowVisible((*STATE).preview_box) != 0 {
        SetTimer(hwnd, 1, 200, None);
    }
}
unsafe fn release_preview(hwnd: HWND) {
    KillTimer(hwnd, 1);
    if !STATE.is_null() {
        (*STATE).preview.release();
        (*STATE).preview_revision = 0;
        (*STATE).preview_pixels = None;
    }
}
unsafe fn begin_preview() {
    let config = render_config();
    let s = &mut *STATE;
    match config {
        Ok(cfg) => {
            let path = s
                .preview_path
                .clone()
                .unwrap_or_else(crate::preview::default_path);
            settext(s.preview_status, "Updating preview…");
            s.preview_revision = s.preview.request(path, cfg);
        }
        Err(_) => {
            s.preview.invalidate();
            s.preview_revision = 0;
            settext(
                s.preview_status,
                "Enter valid values to update the preview.",
            );
        }
    }
}
unsafe fn accept_preview() {
    let s = &mut *STATE;
    if let Some(reply) = s.preview.take() {
        if reply.revision != s.preview_revision {
            return;
        }
        match reply.result {
            Ok(image) => {
                // Composite transparent pixels onto a subtle checkerboard for display.
                let mut pixels = Vec::with_capacity(512 * 512 * 4);
                for (x, y, pixel) in image.enumerate_pixels() {
                    let bg = if (x / 20 + y / 20) % 2 == 0 {
                        255u32
                    } else {
                        240u32
                    };
                    let a = pixel[3] as u32;
                    for c in [2, 1, 0] {
                        pixels.push(((pixel[c] as u32 * a + bg * (255 - a) + 127) / 255) as u8);
                    }
                    pixels.push(255);
                }
                s.preview_pixels = Some(pixels);
                settext(
                    s.preview_status,
                    s.preview_path
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .and_then(|s| s.to_str())
                        .unwrap_or("Default preview"),
                );
            }
            Err(error) => {
                s.preview_pixels = None;
                settext(s.preview_status, &error);
            }
        }
        InvalidateRect(s.preview_box, null(), 0);
    }
}
unsafe fn draw_preview(draw: &windows_sys::Win32::UI::Controls::DRAWITEMSTRUCT) {
    let mut r = draw.rcItem;
    FillRect(draw.hDC, &r, GetSysColorBrush(COLOR_WINDOW));
    FrameRect(draw.hDC, &r, GetSysColorBrush(COLOR_3DSHADOW));
    if STATE.is_null() {
        return;
    }
    if let Some(pixels) = &(*STATE).preview_pixels {
        r.left += 1;
        r.top += 1;
        r.right -= 1;
        r.bottom -= 1;
        let side = (r.right - r.left).min(r.bottom - r.top);
        let x = r.left + (r.right - r.left - side) / 2;
        let y = r.top + (r.bottom - r.top - side) / 2;
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = 512;
        info.bmiHeader.biHeight = -512;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        SetStretchBltMode(draw.hDC, HALFTONE);
        SetBrushOrgEx(draw.hDC, 0, 0, null_mut());
        StretchDIBits(
            draw.hDC,
            x,
            y,
            side,
            side,
            0,
            0,
            512,
            512,
            pixels.as_ptr() as _,
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
    }
}
unsafe fn status() {
    if !STATE.is_null() {
        let s = BROKER.get().unwrap().status();
        let cache = if s["cache_ready"].as_bool().unwrap_or(false) {
            format!(
                "Cache: {} images, {:.1} MiB / {}",
                s["cache_files"],
                s["cache_bytes"].as_u64().unwrap_or(0) as f64 / 1048576.0,
                if s["cache_limit_mb"].as_u64() == Some(0) {
                    "unlimited".to_string()
                } else {
                    format!("{} MiB", s["cache_limit_mb"])
                }
            )
        } else {
            "Cache: counting existing thumbnails".to_owned()
        };
        let text = format!(
            "Preflight: {}     GPU: {}\r\nQueued: {}  (preflight {})     Deferred: {}\r\nRendered: {}     Failures: {}     Cache hits: {}\r\n{}\r\nModel bytes this session: {:.1} MiB\r\nProcess memory: {:.1} MiB     CPU time: {:.1} s",
            if s["paused"].as_bool().unwrap_or(false) {
                "Paused"
            } else if s["scanning"].is_string() {
                "Scanning"
            } else if s["queued"].as_u64().unwrap_or(0) > 0 {
                "Preparing"
            } else if s["preflight_pending"].as_u64().unwrap_or(0) > 0 {
                "Waiting to retry"
            } else {
                "Idle"
            },
            s["gpu"].as_str().unwrap_or("unknown"),
            s["queued"],
            s["preflight_pending"],
            s["preflight_failed"],
            s["success"],
            s["failures"],
            s["cache_hits"],
            cache,
            s["source_bytes"].as_u64().unwrap_or(0) as f64 / 1048576.0,
            s["memory_bytes"].as_u64().unwrap_or(0) as f64 / 1048576.0,
            s["cpu_seconds"].as_f64().unwrap_or(0.0)
        );
        let recent = s["recent"]
            .as_array()
            .map(|r| {
                r.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join("\r\n")
            })
            .unwrap_or_default();
        settext((*STATE).labels, &text);
        if get((*STATE).recent) != recent {
            settext((*STATE).recent, &recent);
        }
    }
}
unsafe fn icon(bytes: &[u8], desired: i32) -> HICON {
    if bytes.len() < 22 {
        return null_mut();
    }
    let n = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let mut chosen = None;
    for i in 0..n {
        let at = 6 + i * 16;
        if at + 16 > bytes.len() {
            break;
        }
        let width = if bytes[at] == 0 {
            256
        } else {
            bytes[at] as usize
        };
        let size = u32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap()) as usize;
        let offset = u32::from_le_bytes(bytes[at + 12..at + 16].try_into().unwrap()) as usize;
        if offset + size <= bytes.len()
            && chosen.is_none_or(|(distance, _, _)| width.abs_diff(desired as usize) < distance)
        {
            chosen = Some((width.abs_diff(desired as usize), offset, size))
        }
    }
    if let Some((_, offset, size)) = chosen {
        CreateIconFromResourceEx(
            bytes[offset..].as_ptr(),
            size as u32,
            1,
            0x30000,
            desired,
            desired,
            0,
        )
    } else {
        null_mut()
    }
}
unsafe fn tray_icon(hwnd: HWND) -> HICON {
    let light = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .ok()
        .and_then(|k| k.get_value::<u32, _>("SystemUsesLightTheme").ok())
        .unwrap_or(0)
        != 0;
    let dpi = windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd);
    let size = windows_sys::Win32::UI::HiDpi::GetSystemMetricsForDpi(SM_CXSMICON, dpi);
    icon(
        if light {
            include_bytes!("../assets/tray-light.ico")
        } else {
            include_bytes!("../assets/tray-dark.ico")
        },
        size.max(16),
    )
}
unsafe fn notify(hwnd: HWND, action: u32, ico: HICON) {
    let mut n: NOTIFYICONDATAW = std::mem::zeroed();
    n.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    n.hWnd = hwnd;
    n.uID = 1;
    n.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    n.uCallbackMessage = TRAY;
    n.hIcon = ico;
    let tip = common::wide("StepThumbs");
    n.szTip[..tip.len()].copy_from_slice(&tip);
    Shell_NotifyIconW(action, &n);
}
unsafe fn button(hwnd: HWND, text: &str, x: i32, y: i32, w: i32, id: usize) -> HWND {
    control(hwnd, "BUTTON", text, 0, x, y, w, 24, id)
}
unsafe extern "system" fn window(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        window_inner(hwnd, msg, wp, lp)
    }))
    .unwrap_or_else(|_| {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(common::data_dir().join("activity.log"))
        {
            let _ = writeln!(f, "A window operation panicked");
        }
        if msg == WM_CREATE { -1 } else { 0 }
    })
}
unsafe fn window_inner(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let dpi = if SNAPSHOT.load(Ordering::Relaxed) {
                SNAPSHOT_DPI.load(Ordering::Relaxed)
            } else {
                windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96)
            };
            layout::init(
                dpi,
                if COMPACT.load(Ordering::Relaxed) {
                    256
                } else {
                    layout::HEIGHT
                },
            );
            TRAY_ICON = tray_icon(hwnd);
            APP_ICON = icon(include_bytes!("../assets/app.ico"), 32);
            SendMessageW(hwnd, WM_SETICON, 1, APP_ICON as isize);
            SendMessageW(hwnd, WM_SETICON, 0, APP_ICON as isize);
            if !SNAPSHOT.load(Ordering::Relaxed) {
                notify(hwnd, NIM_ADD, TRAY_ICON);
            } else {
                initialize_controls(hwnd);
            }
            0
        }
        WM_COMMAND => {
            let id = (wp & 0xffff) as usize;
            // Child controls can notify the parent before their construction
            // has returned and STATE has been assigned.
            if STATE.is_null() && lp != 0 {
                return 0;
            }
            if STATE.is_null() && !matches!(id, 101 | 102 | 103 | 105) {
                initialize_controls(hwnd);
            }
            let notification = (wp >> 16) as u32;
            match id {
                200..=210 | 230..=234 if notification == EN_CHANGE || notification == CBN_SELCHANGE || notification == BN_CLICKED => {
                    enable_rendering();
                    schedule_preview(hwnd);
                }
                50..=51 => {
                    let s=&*STATE;
                    let list=if id==50 {s.roots}else{s.exclude};
                    EnableWindow(s.remove_paths[id-50], (SendMessageW(list,LB_GETCURSEL,0,0)>=0) as i32);
                }
                52 | 54 => {
                    match pick(hwnd,true) {
                        Ok(Some(path)) => list_add(if id==52 {(*STATE).roots}else{(*STATE).exclude},&path),
                        Ok(None) => {},
                        Err(e) => { MessageBoxW(hwnd, common::wide(common::brief_error(&e)).as_ptr(),common::wide("Choose folder").as_ptr(),MB_OK|MB_ICONERROR); }
                    }
                }
                53 | 55 => {
                    let list=if id==53 {(*STATE).roots}else{(*STATE).exclude};
                    let index=SendMessageW(list,LB_GETCURSEL,0,0);
                    if index>=0 { SendMessageW(list,LB_DELETESTRING,index as usize,0); }
                    EnableWindow((*STATE).remove_paths[(id-53)/2],0);
                }
                81 => {
                    match pick(hwnd,false) {
                        Ok(Some(path)) => { (*STATE).preview_path=Some(path.into()); schedule_preview(hwnd); }
                        Ok(None) => {},
                        Err(e) => { MessageBoxW(hwnd,common::wide(common::brief_error(&e)).as_ptr(),common::wide("Load preview file").as_ptr(),MB_OK|MB_ICONERROR); }
                    }
                }
                82 => { (*STATE).preview_path=None; schedule_preview(hwnd); }
                106 => crate::monitor::rescan(),
                107 => crate::monitor::retry_failed(),
                108 => {
                    if MessageBoxW(hwnd,common::wide("Delete StepThumbs’ cached thumbnails?\r\n\r\nThey will be rendered again when requested or on the next rescan.").as_ptr(),common::wide("Clear thumbnail cache").as_ptr(),MB_YESNO|MB_ICONWARNING|MB_DEFBUTTON2)==IDYES {
                        std::thread::spawn(|| { if let Err(e)=crate::cache_index::clear_images(){crate::cache_index::log(&e);} update(); });
                    }
                }
                10..=13 => {
                    page(id - 10);
                    status();
                }
                40 => {
                    let s = &*STATE;
                    let result = (|| -> anyhow::Result<()> {
                        let mut cfg = Config::load()?;
                        cfg.roots = list_values(s.roots);
                        cfg.exclude = list_values(s.exclude);
                        cfg.max_cache_mb = get(s.limit).parse()?;
                        cfg.background_workers = get(s.workers).parse()?;
                        cfg.max_file_mb = get(s.max_file).parse()?;
                        cfg.start_with_windows = check(s.startup);
                        cfg.save()?;
                        common::hooks::startup(cfg.start_with_windows, &std::env::current_exe()?)?;
                        Ok(())
                    })();
                    if let Err(e) = result {
                        MessageBoxW(
                            hwnd,
                            common::wide(format!("{e:#}")).as_ptr(),
                            common::wide("Settings").as_ptr(),
                            MB_OK | MB_ICONERROR,
                        );
                    } else {
                        restart(1);
                    }
                }
                41 => {
                    let s = &*STATE;
                    let selected = common::hooks::EXTENSIONS
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| check(s.hooks[*i]))
                        .map(|(_, x)| x.to_string())
                        .collect::<Vec<_>>();
                    let changes = selected
                        .iter()
                        .filter(|x| !common::hooks::status(x).ours)
                        .map(|x| format!("{}: {}", x, common::hooks::status(x).provider))
                        .collect::<Vec<_>>();
                    let text = format!(
                        "Replace these thumbnail providers?\r\n\r\n{}\r\n\r\nThe previous registration is saved and can be restored. Unchecked StepThumbs formats will be restored.",
                        if changes.is_empty() {
                            "No new providers".into()
                        } else {
                            changes.join("\r\n")
                        }
                    );
                    if MessageBoxW(
                        hwnd,
                        common::wide(text).as_ptr(),
                        common::wide("Confirm thumbnail providers").as_ptr(),
                        MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
                    ) == IDYES
                    {
                        let dll = std::env::current_exe()
                            .unwrap()
                            .with_file_name("StepThumbsShell.dll");
                        let result = common::hooks::install(&selected, &dll).and_then(|_| {
                            let mut c = Config::load()?;
                            c.extensions = selected;
                            c.save()
                        });
                        if let Err(e) = result {
                            MessageBoxW(
                                hwnd,
                                common::wide(format!("{e:#}")).as_ptr(),
                                common::wide("File types").as_ptr(),
                                MB_OK | MB_ICONERROR,
                            );
                        } else {
                            restart(2);
                        }
                    }
                }
                42 => {
                    let result = render_config().and_then(|cfg| cfg.save());
                    if let Err(e) = result {
                        MessageBoxW(
                            hwnd,
                            common::wide(format!("{e:#}")).as_ptr(),
                            common::wide("Rendering").as_ptr(),
                            MB_OK | MB_ICONERROR,
                        );
                    } else {
                        restart(3);
                    }
                }
                100 => {
                    page(0);
                    status();
                    layout::show(hwnd);
                }
                104 => {
                    page(2);
                    status();
                    layout::show(hwnd);
                }
                105 => {
                    let path = common::cache::directory();
                    let _ = std::fs::create_dir_all(&path);
                    ShellExecuteW(
                        hwnd,
                        common::wide("open").as_ptr(),
                        common::wide(path).as_ptr(),
                        null(),
                        null(),
                        SW_SHOW,
                    );
                }
                103 => {
                    let log = common::data_dir().join("activity.log");
                    let _ = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log);
                    ShellExecuteW(
                        hwnd,
                        common::wide("open").as_ptr(),
                        common::wide(log).as_ptr(),
                        null(),
                        null(),
                        SW_SHOW,
                    );
                }
                102 => {
                    BROKER.get().unwrap().toggle_pause();
                }
                101 => {
                    let b = BROKER.get().unwrap().clone();
                    std::thread::spawn(move || {
                        b.drain();
                        crate::ui::quit();
                    });
                }
                _ => {}
            }
            0
        }
        WM_NOTIFY => {
            use windows_sys::Win32::UI::Controls::*;
            let notification = &*(lp as *const NMHDR);
            if !STATE.is_null() && notification.idFrom == 9 && notification.code == TCN_SELCHANGE {
                page(SendMessageW((*STATE).tabs, TCM_GETCURSEL, 0, 0) as usize);
                status();
            }
            0
        }
        WM_DRAWITEM if wp == 80 => {
            draw_preview(&*(lp as *const windows_sys::Win32::UI::Controls::DRAWITEMSTRUCT));
            1
        }
        WM_TIMER if wp == 1 => {
            KillTimer(hwnd, 1);
            begin_preview();
            0
        }
        PREVIEW_READY => {
            accept_preview();
            0
        }
        TRAY => {
            if lp as u32 == WM_LBUTTONUP {
                initialize_controls(hwnd);
                layout::show(hwnd);
                schedule_preview(hwnd);
                status();
            } else if lp as u32 == WM_RBUTTONUP {
                let menu = CreatePopupMenu();
                AppendMenuW(menu, MF_STRING, 100, common::wide("Activity").as_ptr());
                AppendMenuW(menu, MF_STRING, 104, common::wide("Configure").as_ptr());
                let paused = *BROKER.get().unwrap().state.paused.lock().unwrap();
                AppendMenuW(
                    menu,
                    MF_STRING,
                    102,
                    common::wide(if paused {
                        "Resume preflight"
                    } else {
                        "Pause preflight"
                    })
                    .as_ptr(),
                );
                AppendMenuW(menu, MF_STRING, 103, common::wide("Open log").as_ptr());
                AppendMenuW(menu, MF_STRING, 101, common::wide("Exit").as_ptr());
                let mut p = POINT { x: 0, y: 0 };
                GetCursorPos(&mut p);
                SetForegroundWindow(hwnd);
                TrackPopupMenu(menu, TPM_RIGHTBUTTON, p.x, p.y, 0, hwnd, null());
                DestroyMenu(menu);
            }
            0
        }
        WM_SETTINGCHANGE | WM_THEMECHANGED => {
            let new = tray_icon(hwnd);
            if !new.is_null() {
                notify(hwnd, NIM_MODIFY, new);
                DestroyIcon(TRAY_ICON);
                TRAY_ICON = new;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_DPICHANGED => {
            let rect = &*(lp as *const RECT);
            layout::set_dpi((wp as u32 & 0xffff).max(96));
            let old_font = FONT;
            if !STATE.is_null() {
                FONT = layout::font();
                layout::change_dpi(layout::dpi(), FONT);
                tab_size((*STATE).tabs);
                list_extent((*STATE).roots);
                list_extent((*STATE).exclude);
            }
            if !old_font.is_null() {
                DeleteObject(old_font);
            }
            let (width, height) = layout::size(if SNAPSHOT.load(Ordering::Relaxed) {
                WS_EX_TOOLWINDOW
            } else {
                0
            });
            SetWindowPos(
                hwnd,
                null_mut(),
                rect.left,
                rect.top,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            let new = tray_icon(hwnd);
            if !new.is_null() {
                notify(hwnd, NIM_MODIFY, new);
                DestroyIcon(TRAY_ICON);
                TRAY_ICON = new;
            }
            RedrawWindow(
                hwnd,
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_FRAME,
            );
            0
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            SetBkMode(wp as HDC, TRANSPARENT as i32);
            SetTextColor(
                wp as HDC,
                GetSysColor(if IsWindowEnabled(lp as HWND) != 0 {
                    COLOR_BTNTEXT
                } else {
                    COLOR_GRAYTEXT
                }),
            );
            SetBkColor(wp as HDC, GetSysColor(COLOR_BTNFACE));
            GetSysColorBrush(COLOR_BTNFACE) as LRESULT
        }
        WM_EXITSIZEMOVE => {
            if !SNAPSHOT.load(Ordering::Relaxed) {
                layout::save(hwnd);
            }
            0
        }
        QUIT => {
            if !SNAPSHOT.load(Ordering::Relaxed) {
                layout::save(hwnd);
            }
            DestroyWindow(hwnd);
            0
        }
        SHOW => {
            initialize_controls(hwnd);
            layout::show(hwnd);
            schedule_preview(hwnd);
            status();
            0
        }
        UPDATE => {
            UPDATE_PENDING.store(false, Ordering::Release);
            if IsWindowVisible(hwnd) != 0 {
                status();
            }
            0
        }
        WM_CLOSE => {
            if !SNAPSHOT.load(Ordering::Relaxed) {
                layout::save(hwnd);
            }
            release_preview(hwnd);
            ShowWindow(hwnd, SW_HIDE);
            0
        }
        WM_DESTROY => {
            if !SNAPSHOT.load(Ordering::Relaxed) {
                notify(hwnd, NIM_DELETE, TRAY_ICON);
            }
            DestroyIcon(TRAY_ICON);
            DestroyIcon(APP_ICON);
            TRAY_ICON = null_mut();
            APP_ICON = null_mut();
            if !STATE.is_null() {
                drop(Box::from_raw(STATE));
                STATE = null_mut();
            }
            if !FONT.is_null() {
                DeleteObject(FONT);
                FONT = null_mut();
            }
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
pub fn run(broker: Arc<Broker>, visible: bool, initial_page: usize) -> anyhow::Result<()> {
    INITIAL_PAGE.store(initial_page, Ordering::Relaxed);
    BROKER
        .set(broker)
        .map_err(|_| anyhow::anyhow!("UI already running"))?;
    unsafe {
        use windows_sys::Win32::System::Threading::*;
        SetPriorityClass(GetCurrentProcess(), NORMAL_PRIORITY_CLASS);
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_NORMAL);
        windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
        let inst = GetModuleHandleW(null());
        let class = common::wide("StepThumbsRustWindow");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window),
            hInstance: inst,
            lpszClassName: class.as_ptr(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_BTNFACE + 1) as HBRUSH,
            ..std::mem::zeroed()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let (x, y) = layout::initial_position();
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            common::wide("StepThumbs").as_ptr(),
            layout::STYLE,
            x,
            y,
            690,
            460,
            null_mut(),
            null_mut(),
            inst,
            null(),
        );
        if hwnd.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        WINDOW.store(hwnd as isize, Ordering::Release);
        if QUIT_REQUESTED.load(Ordering::Acquire) {
            PostMessageW(hwnd, QUIT, 0, 0);
        } else if visible {
            initialize_controls(hwnd);
            layout::show(hwnd);
            schedule_preview(hwnd);
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        WINDOW.store(0, Ordering::Release);
    }
    Ok(())
}

pub fn snapshot(
    broker: Arc<Broker>,
    folder: &std::path::Path,
    filetypes_only: bool,
    dpi: u32,
) -> anyhow::Result<()> {
    SNAPSHOT.store(true, Ordering::Relaxed);
    SNAPSHOT_DPI.store(dpi, Ordering::Relaxed);
    COMPACT.store(filetypes_only, Ordering::Relaxed);
    BROKER
        .set(broker)
        .map_err(|_| anyhow::anyhow!("UI already running"))?;
    std::fs::create_dir_all(folder)?;
    unsafe {
        let instance = GetModuleHandleW(null());
        let class = common::wide("StepThumbsSnapshotWindow");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_BTNFACE + 1) as HBRUSH,
            ..std::mem::zeroed()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            common::wide("StepThumbs").as_ptr(),
            layout::STYLE,
            -32000,
            -32000,
            690,
            if filetypes_only { 310 } else { 460 },
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        WINDOW.store(hwnd as isize, Ordering::Release);
        let (width, height) = layout::size(WS_EX_TOOLWINDOW);
        SetWindowPos(
            hwnd,
            null_mut(),
            -32000,
            -32000,
            width,
            height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let mut rect: RECT = std::mem::zeroed();
        GetWindowRect(hwnd, &mut rect);
        let width = (rect.right - rect.left) as u32;
        let height = (rect.bottom - rect.top) as u32;
        let dc = CreateCompatibleDC(null_mut());
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width as i32;
        info.bmiHeader.biHeight = -(height as i32);
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        let mut bits = null_mut();
        let bmp = CreateDIBSection(null_mut(), &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if dc.is_null() || bmp.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        let old = SelectObject(dc, bmp);
        let pages = if filetypes_only { 2..3 } else { 0..4 };
        for i in pages {
            page(i);
            if i == 3 {
                KillTimer(hwnd, 1);
                begin_preview();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(35);
                while std::time::Instant::now() < deadline
                    && get((*STATE).preview_status).starts_with("Updating preview")
                {
                    let mut message: MSG = std::mem::zeroed();
                    while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            InvalidateRect(hwnd, null(), 1);
            UpdateWindow(hwnd);
            if windows_sys::Win32::Storage::Xps::PrintWindow(hwnd, dc, 0) == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let src = std::slice::from_raw_parts(bits as *const u8, (width * height * 4) as usize);
            let mut rgba = vec![0u8; src.len()];
            for (a, b) in src.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
                b.copy_from_slice(&[a[2], a[1], a[0], 255]);
            }
            image::save_buffer(
                folder.join(format!("tab-{i}.png")),
                &rgba,
                width,
                height,
                image::ColorType::Rgba8,
            )?;
        }
        SelectObject(dc, old);
        DeleteObject(bmp);
        DeleteDC(dc);
        DestroyWindow(hwnd);
    }
    Ok(())
}
