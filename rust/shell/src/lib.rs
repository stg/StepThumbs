#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]
use std::{
    ffi::c_void,
    mem::size_of,
    path::PathBuf,
    ptr::null_mut,
    sync::atomic::{AtomicI32, AtomicU32, Ordering},
};
use stepthumbnail_common as common;
use windows_sys::{
    Win32::{Graphics::Gdi::*, System::LibraryLoader::*},
    core::GUID,
};
const OK: i32 = 0;
const FAIL: i32 = 0x80004005u32 as i32;
const NOIF: i32 = 0x80004002u32 as i32;
const POINTER: i32 = 0x80004003u32 as i32;
const INVALID: i32 = 0x80070057u32 as i32;
const UNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
const FACTORY: GUID = GUID::from_u128(0x00000001_0000_0000_c000_000000000046);
const INIT: GUID = GUID::from_u128(0xb7d14566_0509_4cce_a71f_0a554233bd9b);
const THUMB: GUID = GUID::from_u128(0xe357fccd_a995_4576_b01f_234630154e96);
const CLASS: GUID = GUID::from_u128(0x86f9b3ac_39ae_4eb1_ad06_819be42b2019);
unsafe fn eq(a: *const GUID, b: &GUID) -> bool {
    let a = &*a;
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}
static OBJECTS: AtomicI32 = AtomicI32::new(0);
#[repr(C)]
struct Interface {
    vt: *const Vtable,
    owner: *mut Provider,
}
#[repr(C)]
struct Vtable {
    query: unsafe extern "system" fn(*mut Interface, *const GUID, *mut *mut c_void) -> i32,
    add: unsafe extern "system" fn(*mut Interface) -> u32,
    release: unsafe extern "system" fn(*mut Interface) -> u32,
    method: *const (),
}
#[repr(C)]
struct Provider {
    init: Interface,
    thumb: Interface,
    refs: AtomicU32,
    path: Option<PathBuf>,
}
unsafe extern "system" fn query(
    this: *mut Interface,
    id: *const GUID,
    out: *mut *mut c_void,
) -> i32 {
    if this.is_null() || id.is_null() || out.is_null() {
        return POINTER;
    }
    *out = null_mut();
    let p = (*this).owner;
    if eq(id, &UNKNOWN) || eq(id, &INIT) {
        *out = &mut (*p).init as *mut _ as _
    } else if eq(id, &THUMB) {
        *out = &mut (*p).thumb as *mut _ as _
    } else {
        return NOIF;
    }
    add(this);
    OK
}
unsafe extern "system" fn add(this: *mut Interface) -> u32 {
    (*(*this).owner).refs.fetch_add(1, Ordering::Relaxed) + 1
}
unsafe extern "system" fn release(this: *mut Interface) -> u32 {
    let p = (*this).owner;
    let n = (*p).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if n == 0 {
        drop(Box::from_raw(p));
        OBJECTS.fetch_sub(1, Ordering::SeqCst);
    }
    n
}
unsafe extern "system" fn initialize(this: *mut Interface, path: *const u16, _mode: u32) -> i32 {
    std::panic::catch_unwind(|| {
        if path.is_null() {
            return INVALID;
        }
        let p = &mut *(*this).owner;
        if p.path.is_some() {
            return 0x800704dfu32 as i32;
        }
        let mut len = 0;
        while *path.add(len) != 0 {
            len += 1;
            if len > 32767 {
                return INVALID;
            }
        }
        use std::os::windows::ffi::OsStringExt;
        p.path = Some(PathBuf::from(std::ffi::OsString::from_wide(
            std::slice::from_raw_parts(path, len),
        )));
        OK
    })
    .unwrap_or(FAIL)
}
unsafe fn exe_path() -> Option<PathBuf> {
    let mut module = null_mut();
    if GetModuleHandleExW(0x6, DllCanUnloadNow as *const () as *const u16, &mut module) == 0 {
        return None;
    }
    let mut buf = vec![0u16; 32768];
    let n = GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    use std::os::windows::ffi::OsStringExt;
    let peer =
        PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])).with_file_name("StepThumbs.exe");
    if let Some(installed) = common::hooks::installed_executable() {
        let family =
            |path: &std::path::Path| path.parent().and_then(|p| p.parent()).map(common::path_key);
        if family(&peer) == family(&installed) {
            return Some(installed);
        }
    }
    Some(peer)
}
unsafe fn make_bitmap(path: &std::path::Path, size: u32) -> anyhow::Result<HBITMAP> {
    let side = size.min(512);
    let mut source = image::open(path)?.to_rgba8();
    for pixel in source.pixels_mut() {
        let alpha = pixel[3] as u32;
        for channel in &mut pixel.0[..3] {
            *channel = ((*channel as u32 * alpha + 127) / 255) as u8;
        }
    }
    let image = image::imageops::resize(&source, side, side, image::imageops::FilterType::Lanczos3);
    let mut info: BITMAPINFO = std::mem::zeroed();
    info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = side as i32;
    info.bmiHeader.biHeight = -(side as i32);
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    let mut bits = null_mut();
    let bitmap = CreateDIBSection(null_mut(), &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    if bitmap.is_null() {
        anyhow::bail!("CreateDIBSection failed")
    };
    let dst = std::slice::from_raw_parts_mut(bits as *mut u8, (side * side * 4) as usize);
    for (src, d) in image.as_raw().chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        let a = src[3];
        d[0] = src[2].min(a);
        d[1] = src[1].min(a);
        d[2] = src[0].min(a);
        d[3] = a;
    }
    Ok(bitmap)
}
unsafe extern "system" fn thumbnail(
    this: *mut Interface,
    size: u32,
    out: *mut HBITMAP,
    alpha: *mut i32,
) -> i32 {
    std::panic::catch_unwind(|| {
        if out.is_null() || alpha.is_null() {
            return POINTER;
        }
        *out = null_mut();
        let Some(path) = &(*(*this).owner).path else {
            return INVALID;
        };
        if size == 0 {
            return INVALID;
        }
        let result = (|| -> anyhow::Result<HBITMAP> {
            let config = common::Config::load()?;
            if let Some(cached) = common::cache::find(path, &config) {
                match make_bitmap(&cached, size) {
                    Ok(bitmap) => {
                        common::shared_stats::hit();
                        return Ok(bitmap);
                    }
                    Err(error) => {
                        if image::open(&cached).is_err() {
                            std::fs::remove_file(&cached)?;
                        } else {
                            return Err(error);
                        }
                    }
                }
            }
            let response = common::ipc::call(
                &common::ipc::Request::Thumbnail {
                    path: path.to_string_lossy().into(),
                },
                exe_path().as_deref(),
            )?;
            let image = PathBuf::from(
                response
                    .image
                    .ok_or_else(|| anyhow::anyhow!(response.error.unwrap_or_default()))?,
            );
            make_bitmap(&image, size)
        })();
        match result {
            Ok(b) => {
                *out = b;
                *alpha = 2;
                OK
            }
            Err(e) => {
                if std::env::var_os("STEPTHUMBNAIL_DEBUG").is_some() {
                    eprintln!("StepThumbs: {e:#}");
                }
                FAIL
            }
        }
    })
    .unwrap_or(FAIL)
}
unsafe impl Sync for Vtable {}
static INIT_V: Vtable = Vtable {
    query,
    add,
    release,
    method: initialize as *const (),
};
static THUMB_V: Vtable = Vtable {
    query,
    add,
    release,
    method: thumbnail as *const (),
};
#[repr(C)]
struct Factory {
    vt: *const FactoryV,
    refs: AtomicU32,
}
#[repr(C)]
struct FactoryV {
    query: unsafe extern "system" fn(*mut Factory, *const GUID, *mut *mut c_void) -> i32,
    add: unsafe extern "system" fn(*mut Factory) -> u32,
    release: unsafe extern "system" fn(*mut Factory) -> u32,
    create:
        unsafe extern "system" fn(*mut Factory, *mut c_void, *const GUID, *mut *mut c_void) -> i32,
    lock: unsafe extern "system" fn(*mut Factory, i32) -> i32,
}
unsafe extern "system" fn fq(this: *mut Factory, id: *const GUID, out: *mut *mut c_void) -> i32 {
    if out.is_null() || id.is_null() {
        return POINTER;
    }
    *out = null_mut();
    if !eq(id, &UNKNOWN) && !eq(id, &FACTORY) {
        return NOIF;
    }
    *out = this as _;
    fa(this);
    OK
}
unsafe extern "system" fn fa(this: *mut Factory) -> u32 {
    (*this).refs.fetch_add(1, Ordering::Relaxed) + 1
}
unsafe extern "system" fn fr(this: *mut Factory) -> u32 {
    let n = (*this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if n == 0 {
        drop(Box::from_raw(this));
        OBJECTS.fetch_sub(1, Ordering::SeqCst);
    }
    n
}
unsafe extern "system" fn create(
    _this: *mut Factory,
    outer: *mut c_void,
    id: *const GUID,
    out: *mut *mut c_void,
) -> i32 {
    std::panic::catch_unwind(|| {
        if out.is_null() {
            return POINTER;
        }
        *out = null_mut();
        if !outer.is_null() {
            return 0x80040110u32 as i32;
        }
        let p = Box::into_raw(Box::new(Provider {
            init: Interface {
                vt: &INIT_V,
                owner: null_mut(),
            },
            thumb: Interface {
                vt: &THUMB_V,
                owner: null_mut(),
            },
            refs: AtomicU32::new(1),
            path: None,
        }));
        (*p).init.owner = p;
        (*p).thumb.owner = p;
        OBJECTS.fetch_add(1, Ordering::SeqCst);
        let hr = query(&mut (*p).init, id, out);
        release(&mut (*p).init);
        hr
    })
    .unwrap_or(FAIL)
}
unsafe extern "system" fn lock(_: *mut Factory, b: i32) -> i32 {
    if b != 0 {
        OBJECTS.fetch_add(1, Ordering::SeqCst);
    } else {
        OBJECTS.fetch_sub(1, Ordering::SeqCst);
    }
    OK
}
static FACTORY_V: FactoryV = FactoryV {
    query: fq,
    add: fa,
    release: fr,
    create,
    lock,
};
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    class: *const GUID,
    id: *const GUID,
    out: *mut *mut c_void,
) -> i32 {
    std::panic::catch_unwind(|| {
        if class.is_null() || out.is_null() {
            return POINTER;
        }
        *out = null_mut();
        if !eq(class, &CLASS) {
            return 0x80040111u32 as i32;
        }
        let f = Box::into_raw(Box::new(Factory {
            vt: &FACTORY_V,
            refs: AtomicU32::new(1),
        }));
        OBJECTS.fetch_add(1, Ordering::SeqCst);
        let hr = fq(f, id, out);
        fr(f);
        hr
    })
    .unwrap_or(FAIL)
}
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> i32 {
    if OBJECTS.load(Ordering::SeqCst) == 0 {
        OK
    } else {
        1
    }
}
