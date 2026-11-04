use crate::common;
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    path::Path,
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    UI::{HiDpi::*, WindowsAndMessaging::*},
};

pub const STYLE: u32 = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
pub const WIDTH: i32 = 668;
pub const HEIGHT: i32 = 410;
thread_local! {
    static DPI: Cell<u32> = const { Cell::new(96) };
    static CONTROLS: RefCell<Vec<(HWND,[i32;4])>> = const { RefCell::new(Vec::new()) };
    static PRESENTED: Cell<bool> = const { Cell::new(false) };
    static CLIENT_HEIGHT: Cell<i32> = const { Cell::new(HEIGHT) };
}
pub fn dpi() -> u32 {
    DPI.get()
}
pub fn scale(value: i32, dpi: u32) -> i32 {
    ((value as i64 * dpi as i64 + 48) / 96) as i32
}
pub fn rect(values: [i32; 4]) -> [i32; 4] {
    values.map(|v| scale(v, dpi()))
}
pub fn init(dpi: u32, height: i32) {
    DPI.set(dpi.max(96));
    CLIENT_HEIGHT.set(height);
    CONTROLS.with(|v| v.borrow_mut().clear());
    PRESENTED.set(false);
}
pub fn add(hwnd: HWND, rect: [i32; 4]) {
    CONTROLS.with(|v| v.borrow_mut().push((hwnd, rect)));
}
pub unsafe fn font() -> HFONT {
    unsafe {
        CreateFontW(
            -scale(12, dpi()),
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            0,
            0,
            CLEARTYPE_QUALITY as u32,
            0,
            common::wide("Segoe UI").as_ptr(),
        )
    }
}
pub unsafe fn size(ex_style: u32) -> (i32, i32) {
    let mut r = RECT {
        left: 0,
        top: 0,
        right: scale(WIDTH, dpi()),
        bottom: scale(CLIENT_HEIGHT.get(), dpi()),
    };
    unsafe {
        AdjustWindowRectExForDpi(&mut r, STYLE, 0, ex_style, dpi());
    }
    (r.right - r.left, r.bottom - r.top)
}
pub unsafe fn change_dpi(dpi: u32, font: HFONT) {
    DPI.set(dpi.max(96));
    CONTROLS.with(|v| {
        for (hwnd, logical) in v.borrow().iter() {
            let [x, y, w, h] = rect(*logical);
            unsafe {
                SetWindowPos(
                    *hwnd,
                    std::ptr::null_mut(),
                    x,
                    y,
                    w,
                    h,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW,
                );
                SendMessageW(*hwnd, WM_SETFONT, font as usize, 0);
            }
        }
    });
}
pub fn set_dpi(dpi: u32) {
    DPI.set(dpi.max(96));
}

#[derive(Serialize, Deserialize)]
struct Position {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    work_left: i32,
    work_top: i32,
    dpi: u32,
}
fn saved(path: &Path) -> Option<Position> {
    let value: Position = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (value.dpi >= 96
        && value.dpi <= 768
        && value.right > value.left
        && value.bottom > value.top
        && [
            value.left,
            value.top,
            value.right,
            value.bottom,
            value.work_left,
            value.work_top,
        ]
        .iter()
        .all(|v| v.unsigned_abs() < 1_000_000))
    .then_some(value)
}
unsafe fn work(monitor: HMONITOR) -> RECT {
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    unsafe {
        GetMonitorInfoW(monitor, &mut info);
    }
    info.rcWork
}
unsafe fn active_monitor(hwnd: HWND) -> HMONITOR {
    unsafe {
        let foreground = GetForegroundWindow();
        if !foreground.is_null() && foreground != hwnd && IsWindowVisible(foreground) != 0 {
            return MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST);
        }
        let mut point = POINT { x: 0, y: 0 };
        GetCursorPos(&mut point);
        MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST)
    }
}
pub unsafe fn initial_position() -> (i32, i32) {
    let area = unsafe { work(active_monitor(std::ptr::null_mut())) };
    (
        area.left + (area.right - area.left - WIDTH) / 2,
        area.top + (area.bottom - area.top - HEIGHT) / 2,
    )
}
fn clamp_position(area: RECT, x: i32, y: i32, w: i32, h: i32) -> (i32, i32) {
    (
        x.clamp(area.left, (area.right - w).max(area.left)),
        y.clamp(area.top, (area.bottom - h).max(area.top)),
    )
}
pub unsafe fn show(hwnd: HWND) {
    unsafe {
        if IsWindowVisible(hwnd) == 0 {
            let stored = saved(&common::data_dir().join("window-position.json"));
            let monitor = stored
                .as_ref()
                .map(|p| {
                    MonitorFromRect(
                        &RECT {
                            left: p.left,
                            top: p.top,
                            right: p.right,
                            bottom: p.bottom,
                        },
                        MONITOR_DEFAULTTONULL,
                    )
                })
                .filter(|m| !m.is_null());
            let selected = monitor.unwrap_or_else(|| active_monitor(hwnd));
            let area = work(selected);
            // Establish the destination monitor while hidden, then use that
            // window's actual DPI. Never reposition after ShowWindow.
            let target = stored.as_ref().filter(|_| monitor.is_some());
            let (x, y) = target.map(|p| (p.left, p.top)).unwrap_or_else(|| {
                (
                    area.left + (area.right - area.left - WIDTH) / 2,
                    area.top + (area.bottom - area.top - HEIGHT) / 2,
                )
            });
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            let actual = GetDpiForWindow(hwnd).max(96);
            DPI.set(actual);
            let (w, h) = size(0);
            let (x, y) = target
                .map(|p| {
                    (
                        area.left
                            + ((p.left - p.work_left) as i64 * actual as i64 / p.dpi as i64) as i32,
                        area.top
                            + ((p.top - p.work_top) as i64 * actual as i64 / p.dpi as i64) as i32,
                    )
                })
                .unwrap_or_else(|| {
                    (
                        area.left + (area.right - area.left - w) / 2,
                        area.top + (area.bottom - area.top - h) / 2,
                    )
                });
            let (x, y) = clamp_position(area, x, y, w, h);
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                x,
                y,
                w,
                h,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        PRESENTED.set(true);
        ShowWindow(
            hwnd,
            if IsIconic(hwnd) != 0 {
                SW_RESTORE
            } else {
                SW_SHOW
            },
        );
        SetForegroundWindow(hwnd);
    }
}
pub unsafe fn save(hwnd: HWND) {
    if !PRESENTED.get() {
        return;
    }
    unsafe {
        if IsIconic(hwnd) != 0 {
            return;
        }
        let mut r: RECT = std::mem::zeroed();
        GetWindowRect(hwnd, &mut r);
        let area = work(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST));
        let position = Position {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
            work_left: area.left,
            work_top: area.top,
            dpi: dpi(),
        };
        if let Ok(bytes) = serde_json::to_vec(&position) {
            let _ = common::cache::atomic_write(
                &common::data_dir().join("window-position.json"),
                &bytes,
            );
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dpi_scaling_is_always_from_the_original_layout() {
        for (dpi, expected) in [(96, 235), (120, 294), (144, 353), (192, 470)] {
            assert_eq!(scale(235, dpi), expected);
        }
    }
    #[test]
    fn disconnected_display_positions_are_clamped_into_work_area() {
        let r = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1040,
        };
        assert_eq!(clamp_position(r, 1000, 1200, 690, 460), (-690, 580));
    }
}
