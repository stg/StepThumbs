#![allow(unsafe_op_in_unsafe_fn)]
use crate::{Broker, Config, common};
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
    System::LibraryLoader::GetModuleHandleW,
    UI::{Shell::*, WindowsAndMessaging::*},
};
static BROKER: OnceLock<Arc<Broker>> = OnceLock::new();
static WINDOW: AtomicIsize = AtomicIsize::new(0);
static mut FONT: HFONT = null_mut();
static SNAPSHOT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static INITIAL_PAGE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const TRAY: u32 = WM_APP + 1;
const UPDATE: u32 = WM_APP + 2;
const SHOW: u32 = WM_APP + 3;
const QUIT: u32 = WM_APP + 4;
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
    if !h.is_null() {
        unsafe {
            PostMessageW(h, UPDATE, 0, 0);
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
    let window = CreateWindowExW(
        0,
        common::wide(kind).as_ptr(),
        common::wide(text).as_ptr(),
        WS_CHILD | WS_VISIBLE | style,
        x,
        y,
        w,
        h,
        parent,
        id as HMENU,
        GetModuleHandleW(null()),
        null(),
    );
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
    roots: HWND,
    exclude: HWND,
    settle: HWND,
    limit: HWND,
    startup: HWND,
    hooks: [HWND; 4],
    render_fields: Vec<HWND>,
    render_checks: Vec<HWND>,
    icon: HICON,
    task_icon: HICON,
}
static mut STATE: *mut Controls = null_mut();
unsafe fn page(index: usize) {
    if STATE.is_null() {
        return;
    }
    for (i, controls) in (*STATE).pages.iter().enumerate() {
        for h in controls {
            ShowWindow(*h, if i == index { SW_SHOW } else { SW_HIDE });
        }
    }
}
unsafe fn status() {
    if !STATE.is_null() {
        let s = BROKER.get().unwrap().status();
        let text = format!(
            "Model: {}\r\nScanning: {}\r\nQueued: {}  (preflight {})     GPU: {}\r\nRendered: {}     Failures: {}     Cache hits: {}\r\nCache: {} files, {:.1} / {} MiB\r\nModel bytes this session: {:.1} MiB\r\nProcess memory: {:.1} MiB     CPU time: {:.1} s",
            s["current"].as_str().unwrap_or("Idle"),
            s["scanning"].as_str().unwrap_or("Idle"),
            s["queued"],
            s["preflight_pending"],
            s["gpu"].as_str().unwrap_or("unknown"),
            s["success"],
            s["failures"],
            s["cache_hits"],
            s["cache_files"],
            s["cache_bytes"].as_u64().unwrap_or(0) as f64 / 1048576.0,
            s["cache_limit_mb"],
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
        settext(
            (*STATE).labels,
            &format!("{text}\r\n\r\nRecent activity:\r\n{recent}"),
        );
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
    control(hwnd, "BUTTON", text, 0, x, y, w, 27, id)
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
            let dpi = windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96);
            FONT = CreateFontW(
                -((9 * dpi as i32) / 72),
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                1,
                0,
                0,
                5,
                0,
                common::wide("Segoe UI").as_ptr(),
            );
            let cfg = Config::load().unwrap_or_default();
            let mut pages: [Vec<HWND>; 4] = Default::default();
            for (i, name) in ["Activity", "Settings", "File types", "Rendering"]
                .iter()
                .enumerate()
            {
                button(hwnd, name, 12 + i as i32 * 130, 10, 120, 10 + i);
            }
            let labels = control(hwnd, "STATIC", "", 0, 20, 60, 650, 310, 0);
            pages[0].push(labels);
            let mut add = |page: usize, h: HWND| {
                pages[page].push(h);
                h
            };
            add(
                1,
                control(hwnd, "STATIC", "Roots, one per line", 0, 20, 55, 300, 20, 0),
            );
            let roots = add(
                1,
                control(
                    hwnd,
                    "EDIT",
                    &cfg.roots.join("\r\n"),
                    WS_BORDER | (ES_MULTILINE as u32) | WS_VSCROLL,
                    20,
                    78,
                    300,
                    105,
                    0,
                ),
            );
            add(
                1,
                control(
                    hwnd,
                    "STATIC",
                    "Excluded paths, one per line",
                    0,
                    340,
                    55,
                    300,
                    20,
                    0,
                ),
            );
            let exclude = add(
                1,
                control(
                    hwnd,
                    "EDIT",
                    &cfg.exclude.join("\r\n"),
                    WS_BORDER | (ES_MULTILINE as u32) | WS_VSCROLL,
                    340,
                    78,
                    300,
                    105,
                    0,
                ),
            );
            add(
                1,
                control(hwnd, "STATIC", "Settle seconds", 0, 20, 202, 120, 20, 0),
            );
            let settle = add(
                1,
                control(
                    hwnd,
                    "EDIT",
                    &cfg.settle_seconds.to_string(),
                    WS_BORDER,
                    145,
                    200,
                    80,
                    24,
                    0,
                ),
            );
            add(
                1,
                control(hwnd, "STATIC", "Cache limit MiB", 0, 245, 202, 110, 20, 0),
            );
            let limit = add(
                1,
                control(
                    hwnd,
                    "EDIT",
                    &cfg.max_cache_mb.to_string(),
                    WS_BORDER,
                    360,
                    200,
                    100,
                    24,
                    0,
                ),
            );
            let startup = add(
                1,
                control(
                    hwnd,
                    "BUTTON",
                    "Start with Windows",
                    BS_AUTOCHECKBOX as u32,
                    20,
                    240,
                    250,
                    25,
                    0,
                ),
            );
            checked(startup, common::hooks::startup_enabled());
            add(1, button(hwnd, "Save settings", 20, 280, 150, 40));
            let mut hooks = [null_mut(); 4];
            for (i, ext) in common::hooks::EXTENSIONS.iter().enumerate() {
                let st = common::hooks::status(ext);
                hooks[i] = add(
                    2,
                    control(
                        hwnd,
                        "BUTTON",
                        &format!("{}    {}", ext, st.provider),
                        BS_AUTOCHECKBOX as u32,
                        20,
                        60 + i as i32 * 36,
                        600,
                        27,
                        0,
                    ),
                );
                let fresh = !common::data_dir()
                    .join("previous-associations.json")
                    .exists();
                checked(hooks[i], st.ours || (fresh && i < 2));
            }
            add(
                2,
                button(hwnd, "Install selected preview hooks", 20, 220, 235, 41),
            );
            let values = [
                ("Renderer (look/f3d)", cfg.renderer.clone()),
                ("Background colour", cfg.background.clone()),
                ("AO strength", cfg.ao_strength.to_string()),
                ("AO radius", cfg.ao_radius.to_string()),
                ("Edge darkness", cfg.edge_strength.to_string()),
                ("Edge width", cfg.edge_width.to_string()),
                ("Ambient brightness", cfg.ambient.to_string()),
                ("Light intensity", cfg.light_intensity.to_string()),
                ("Light direction", cfg.light_direction.clone()),
                ("Light colour", cfg.light_colour.clone()),
                ("Lighting preset", cfg.lighting_preset.clone()),
            ];
            let mut render_fields = Vec::new();
            for (i, (name, value)) in values.iter().enumerate() {
                let x = 20 + (i / 6) as i32 * 320;
                let y = 58 + (i % 6) as i32 * 38;
                add(3, control(hwnd, "STATIC", name, 0, x, y + 4, 155, 22, 0));
                render_fields.push(add(
                    3,
                    control(hwnd, "EDIT", value, WS_BORDER, x + 158, y, 140, 24, 0),
                ));
            }
            let mut render_checks = Vec::new();
            for (i, (name, value)) in [
                ("Transparent", cfg.transparent),
                ("Source colours", cfg.source_colours),
                ("Antialias", cfg.antialias),
                ("Ambient occlusion", cfg.ambient_occlusion),
                ("Edges", cfg.edges),
            ]
            .into_iter()
            .enumerate()
            {
                let x = 20 + (i % 3) as i32 * 210;
                let y = 300 + (i / 3) as i32 * 28;
                let h = add(
                    3,
                    control(
                        hwnd,
                        "BUTTON",
                        name,
                        BS_AUTOCHECKBOX as u32,
                        x,
                        y,
                        200,
                        24,
                        0,
                    ),
                );
                checked(h, value);
                render_checks.push(h);
            }
            add(3, button(hwnd, "Save rendering settings", 20, 360, 200, 42));
            add(
                3,
                control(
                    hwnd,
                    "STATIC",
                    "F3D uses background, source colours and antialiasing.",
                    0,
                    235,
                    366,
                    405,
                    22,
                    0,
                ),
            );
            let ico = tray_icon(hwnd);
            let task_icon = icon(include_bytes!("../assets/app.ico"), 32);
            SendMessageW(hwnd, WM_SETICON, 1, task_icon as isize);
            SendMessageW(hwnd, WM_SETICON, 0, task_icon as isize);
            STATE = Box::into_raw(Box::new(Controls {
                pages,
                labels,
                roots,
                exclude,
                settle,
                limit,
                startup,
                hooks,
                render_fields,
                render_checks,
                icon: ico,
                task_icon,
            }));
            if !SNAPSHOT.load(Ordering::Relaxed) {
                notify(hwnd, NIM_ADD, ico);
            }
            page(INITIAL_PAGE.load(Ordering::Relaxed));
            status();
            0
        }
        WM_COMMAND => {
            let id = (wp & 0xffff) as usize;
            match id {
                10..=13 => {
                    page(id - 10);
                    status();
                }
                40 => {
                    let s = &*STATE;
                    let result = (|| -> anyhow::Result<()> {
                        let mut cfg = Config::load()?;
                        cfg.roots = get(s.roots)
                            .lines()
                            .map(|v| v.trim().to_string())
                            .filter(|v| !v.is_empty())
                            .collect();
                        cfg.exclude = get(s.exclude)
                            .lines()
                            .map(|v| v.trim().to_string())
                            .filter(|v| !v.is_empty())
                            .collect();
                        cfg.settle_seconds = get(s.settle).parse()?;
                        cfg.max_cache_mb = get(s.limit).parse()?;
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
                    let s = &*STATE;
                    let result = (|| -> anyhow::Result<()> {
                        let mut cfg = Config::load()?;
                        let f = &s.render_fields;
                        let c = &s.render_checks;
                        cfg.renderer = get(f[0]).trim().to_lowercase();
                        cfg.background = get(f[1]);
                        cfg.ao_strength = get(f[2]).parse()?;
                        cfg.ao_radius = get(f[3]).parse()?;
                        cfg.edge_strength = get(f[4]).parse()?;
                        cfg.edge_width = get(f[5]).parse()?;
                        cfg.ambient = get(f[6]).parse()?;
                        cfg.light_intensity = get(f[7]).parse()?;
                        cfg.light_direction = get(f[8]);
                        cfg.light_colour = get(f[9]);
                        cfg.lighting_preset = get(f[10]);
                        cfg.transparent = check(c[0]);
                        cfg.source_colours = check(c[1]);
                        cfg.antialias = check(c[2]);
                        cfg.ambient_occlusion = check(c[3]);
                        cfg.edges = check(c[4]);
                        cfg.save()?;
                        Ok(())
                    })();
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
                    ShowWindow(hwnd, SW_SHOW);
                    SetForegroundWindow(hwnd);
                }
                104 => {
                    page(2);
                    status();
                    ShowWindow(hwnd, SW_SHOW);
                    SetForegroundWindow(hwnd);
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
        TRAY => {
            if lp as u32 == WM_LBUTTONUP {
                ShowWindow(hwnd, SW_SHOW);
                SetForegroundWindow(hwnd);
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
            if !STATE.is_null() {
                let new = tray_icon(hwnd);
                if !new.is_null() {
                    notify(hwnd, NIM_MODIFY, new);
                    DestroyIcon((*STATE).icon);
                    (*STATE).icon = new;
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_DPICHANGED => {
            let rect = &*(lp as *const RECT);
            SetWindowPos(
                hwnd,
                null_mut(),
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            if !STATE.is_null() {
                let new = tray_icon(hwnd);
                if !new.is_null() {
                    notify(hwnd, NIM_MODIFY, new);
                    DestroyIcon((*STATE).icon);
                    (*STATE).icon = new;
                }
            }
            0
        }
        QUIT => {
            DestroyWindow(hwnd);
            0
        }
        SHOW => {
            ShowWindow(hwnd, SW_SHOW);
            SetForegroundWindow(hwnd);
            status();
            0
        }
        UPDATE => {
            if IsWindowVisible(hwnd) != 0 {
                status();
            }
            0
        }
        WM_CLOSE => {
            ShowWindow(hwnd, SW_HIDE);
            0
        }
        WM_DESTROY => {
            if !STATE.is_null() {
                if !SNAPSHOT.load(Ordering::Relaxed) {
                    notify(hwnd, NIM_DELETE, (*STATE).icon);
                }
                DestroyIcon((*STATE).icon);
                DestroyIcon((*STATE).task_icon);
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
            hbrBackground: (COLOR_WINDOW + 1) as HBRUSH,
            ..std::mem::zeroed()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            common::wide("StepThumbs").as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
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
        if visible {
            ShowWindow(hwnd, SW_SHOW);
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        WINDOW.store(0, Ordering::Release);
    }
    Ok(())
}

pub fn snapshot(
    broker: Arc<Broker>,
    folder: &std::path::Path,
    filetypes_only: bool,
) -> anyhow::Result<()> {
    SNAPSHOT.store(true, Ordering::Relaxed);
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
            hbrBackground: (COLOR_WINDOW + 1) as HBRUSH,
            ..std::mem::zeroed()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            common::wide("StepThumbs").as_ptr(),
            WS_OVERLAPPEDWINDOW,
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
