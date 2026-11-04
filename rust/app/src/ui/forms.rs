use super::*;
use windows_sys::Win32::UI::Controls::*;

pub unsafe fn build(hwnd: HWND, cfg: &Config) -> Controls {
    let init = INITCOMMONCONTROLSEX {
        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_TAB_CLASSES,
    };
    InitCommonControlsEx(&init);
    let tabs = control(
        hwnd,
        "SysTabControl32",
        "",
        TCS_FIXEDWIDTH,
        10,
        8,
        626,
        25,
        9,
    );
    for (i, name) in ["Activity", "Settings", "File types", "Rendering"]
        .iter()
        .enumerate()
    {
        let mut name = common::wide(name);
        let item = TCITEMW {
            mask: TCIF_TEXT,
            pszText: name.as_mut_ptr(),
            ..std::mem::zeroed()
        };
        SendMessageW(tabs, TCM_INSERTITEMW, i, &item as *const _ as isize);
    }
    tab_size(tabs);
    let mut pages: [Vec<HWND>; 4] = Default::default();
    let mut add = |p: usize, h| {
        pages[p].push(h);
        h
    };
    let labels = add(0, control(hwnd, "STATIC", "", 0, 14, 46, 618, 110, 0));
    add(
        0,
        control(hwnd, "STATIC", "Recent activity", 0, 14, 161, 618, 18, 0),
    );
    let recent = add(
        0,
        control(
            hwnd,
            "EDIT",
            "",
            WS_BORDER
                | ES_MULTILINE as u32
                | ES_READONLY as u32
                | ES_AUTOHSCROLL as u32
                | WS_HSCROLL
                | WS_VSCROLL,
            14,
            181,
            618,
            138,
            0,
        ),
    );
    add(0, button(hwnd, "Open cache folder", 14, 333, 126, 105));
    add(0, button(hwnd, "Rescan folders", 148, 333, 108, 106));
    add(0, button(hwnd, "Retry failed", 264, 333, 96, 107));
    add(0, button(hwnd, "Clear cache…", 368, 333, 100, 108));
    let mut paths = Vec::new();
    for (i, (label, values)) in [
        ("Monitored folders", &cfg.roots),
        ("Excluded folders", &cfg.exclude),
    ]
    .into_iter()
    .enumerate()
    {
        let x = 14 + i as i32 * 318;
        add(1, control(hwnd, "STATIC", label, 0, x, 45, 300, 18, 0));
        let list = add(
            1,
            control(
                hwnd,
                "LISTBOX",
                "",
                WS_BORDER
                    | WS_VSCROLL
                    | WS_HSCROLL
                    | LBS_NOTIFY as u32
                    | LBS_NOINTEGRALHEIGHT as u32,
                x,
                65,
                300,
                123,
                50 + i,
            ),
        );
        for value in values {
            list_add(list, value);
        }
        add(1, button(hwnd, "Add…", x, 194, 70, 52 + i * 2));
        let remove = add(1, button(hwnd, "Remove", x + 76, 194, 70, 53 + i * 2));
        EnableWindow(remove, 0);
        paths.push((list, remove));
    }
    add(
        1,
        control(
            hwnd,
            "STATIC",
            "Exclusions affect preflight only; Explorer can still request these thumbnails.",
            0,
            14,
            226,
            618,
            18,
            0,
        ),
    );
    add(
        1,
        control(hwnd, "STATIC", "Background workers", 0, 14, 257, 126, 18, 0),
    );
    let workers = add(
        1,
        control(
            hwnd,
            "EDIT",
            &cfg.background_workers.to_string(),
            WS_BORDER | ES_NUMBER as u32,
            142,
            253,
            64,
            22,
            0,
        ),
    );
    add(
        1,
        control(hwnd, "STATIC", "Cache limit (MiB)", 0, 332, 257, 126, 18, 0),
    );
    let limit = add(
        1,
        control(
            hwnd,
            "EDIT",
            &cfg.max_cache_mb.to_string(),
            WS_BORDER | ES_NUMBER as u32,
            460,
            253,
            90,
            22,
            0,
        ),
    );
    add(
        1,
        control(
            hwnd,
            "STATIC",
            "Max model size (MiB)",
            0,
            14,
            285,
            126,
            18,
            0,
        ),
    );
    let max_file = add(
        1,
        control(
            hwnd,
            "EDIT",
            &cfg.max_file_mb.to_string(),
            WS_BORDER | ES_NUMBER as u32,
            142,
            281,
            64,
            22,
            0,
        ),
    );
    add(
        1,
        control(
            hwnd,
            "STATIC",
            "0 = unlimited for both size limits.",
            0,
            332,
            285,
            300,
            18,
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
            14,
            308,
            180,
            20,
            0,
        ),
    );
    checked(startup, common::hooks::startup_enabled());
    add(
        1,
        control(
            hwnd,
            "STATIC",
            "More workers use more RAM while preparing models.",
            0,
            214,
            310,
            418,
            18,
            0,
        ),
    );
    add(1, button(hwnd, "Save settings", 14, 333, 108, 40));
    let mut hooks = [null_mut(); 4];
    let fresh = !common::data_dir()
        .join("previous-associations.json")
        .exists();
    for (i, ext) in common::hooks::EXTENSIONS.iter().enumerate() {
        hooks[i] = add(
            2,
            control(
                hwnd,
                "BUTTON",
                &format!("{}    {}", ext, common::hooks::status(ext).provider),
                BS_AUTOCHECKBOX as u32,
                22,
                51 + i as i32 * 28,
                600,
                22,
                0,
            ),
        );
        checked(
            hooks[i],
            common::hooks::status(ext).ours || (fresh && i < 2),
        );
    }
    add(
        2,
        button(hwnd, "Install selected preview hooks", 22, 177, 218, 41),
    );
    let mut fields = vec![null_mut(); 11];
    let mut field_labels = vec![null_mut(); 11];
    let values = [
        cfg.renderer.clone(),
        cfg.background.clone(),
        cfg.ao_strength.to_string(),
        cfg.ao_radius.to_string(),
        cfg.edge_strength.to_string(),
        cfg.edge_width.to_string(),
        cfg.ambient.to_string(),
        cfg.light_intensity.to_string(),
        cfg.light_direction.clone(),
        cfg.light_colour.clone(),
        cfg.lighting_preset.clone(),
    ];
    let positions = [
        (0, "Renderer", 14, 48, 126, 142, 44, 198),
        (10, "Lighting", 14, 76, 126, 142, 72, 198),
        (1, "Background", 14, 108, 126, 142, 104, 88),
        (2, "Strength", 142, 164, 52, 197, 160, 47),
        (3, "Radius", 253, 164, 42, 295, 160, 45),
        (4, "Darkness", 142, 192, 52, 197, 188, 47),
        (5, "Width", 253, 192, 42, 295, 188, 45),
        (6, "Ambient brightness", 14, 224, 126, 142, 220, 58),
        (7, "Intensity", 214, 224, 64, 282, 220, 58),
        (8, "Light direction", 14, 252, 126, 142, 248, 198),
        (9, "Light colour", 14, 280, 126, 142, 276, 198),
    ];
    for (i, name, lx, ly, lw, x, y, w) in positions {
        field_labels[i] = add(3, control(hwnd, "STATIC", name, 0, lx, ly, lw, 18, 0));
        fields[i] = add(
            3,
            control(
                hwnd,
                if i == 0 || i == 10 {
                    "COMBOBOX"
                } else {
                    "EDIT"
                },
                if i == 0 || i == 10 { "" } else { &values[i] },
                if i == 0 || i == 10 {
                    CBS_DROPDOWNLIST as u32 | WS_VSCROLL
                } else {
                    WS_BORDER
                },
                x,
                y,
                w,
                if i == 0 || i == 10 { 180 } else { 22 },
                200 + i,
            ),
        );
    }
    combo(
        fields[0],
        &["Look", "F3D"],
        usize::from(cfg.renderer == "f3d"),
    );
    combo(
        fields[10],
        &["Directional", "Studio (5 lights)"],
        usize::from(cfg.lighting_preset == "f3d-match"),
    );
    let mut checks = Vec::new();
    for (i, (name, value, x, y, w)) in [
        ("Transparent", cfg.transparent, 240, 104, 100),
        ("Source colours", cfg.source_colours, 14, 132, 126),
        ("Antialiasing", cfg.antialias, 142, 132, 150),
        ("Ambient occlusion", cfg.ambient_occlusion, 14, 160, 126),
        ("Edges", cfg.edges, 14, 188, 126),
    ]
    .into_iter()
    .enumerate()
    {
        let h = add(
            3,
            control(
                hwnd,
                "BUTTON",
                name,
                BS_AUTOCHECKBOX as u32,
                x,
                y,
                w,
                22,
                230 + i,
            ),
        );
        checked(h, value);
        checks.push(h);
    }
    let render_hint = add(3, control(hwnd, "STATIC", "", 0, 14, 308, 330, 18, 0));
    add(3, button(hwnd, "Save rendering settings", 14, 333, 166, 42));
    let preview_box = add(
        3,
        control(hwnd, "STATIC", "", SS_OWNERDRAW, 362, 44, 270, 260, 80),
    );
    let preview_status = add(
        3,
        control(hwnd, "STATIC", "", SS_PATHELLIPSIS, 362, 310, 270, 18, 83),
    );
    add(3, button(hwnd, "Load preview file", 362, 333, 138, 81));
    add(3, button(hwnd, "Use default", 508, 333, 124, 82));
    let ico = tray_icon(hwnd);
    let task_icon = icon(include_bytes!("../../assets/app.ico"), 32);
    SendMessageW(hwnd, WM_SETICON, 1, task_icon as isize);
    SendMessageW(hwnd, WM_SETICON, 0, task_icon as isize);
    Controls {
        pages,
        tabs,
        labels,
        recent,
        roots: paths[0].0,
        exclude: paths[1].0,
        remove_paths: [paths[0].1, paths[1].1],
        limit,
        workers,
        max_file,
        startup,
        hooks,
        render_fields: fields,
        render_labels: field_labels,
        render_checks: checks,
        render_hint,
        preview_box,
        preview_status,
        preview: crate::preview::Preview::new(BROKER.get().unwrap().clone()),
        preview_path: None,
        preview_revision: 0,
        preview_pixels: None,
        icon: ico,
        task_icon,
    }
}
unsafe fn combo(h: HWND, values: &[&str], selected: usize) {
    for value in values {
        SendMessageW(h, CB_ADDSTRING, 0, common::wide(value).as_ptr() as isize);
    }
    SendMessageW(h, CB_SETCURSEL, selected, 0);
}
pub unsafe fn tab_size(tabs: HWND) {
    let width = layout::scale(100, layout::dpi()) as u32;
    let height = layout::scale(24, layout::dpi()) as u32;
    SendMessageW(tabs, TCM_SETITEMSIZE, 0, ((height << 16) | width) as isize);
}
pub unsafe fn list_add(h: HWND, value: &str) {
    if list_values(h).iter().any(|v| v.eq_ignore_ascii_case(value)) {
        return;
    }
    SendMessageW(h, LB_ADDSTRING, 0, common::wide(value).as_ptr() as isize);
    list_extent(h);
}
pub unsafe fn list_extent(h: HWND) {
    let dc = GetDC(h);
    let old = SelectObject(dc, FONT);
    let mut longest = 0;
    for value in list_values(h) {
        let value = common::wide(value);
        let mut size: SIZE = std::mem::zeroed();
        GetTextExtentPoint32W(
            dc,
            value.as_ptr(),
            value.len().saturating_sub(1) as i32,
            &mut size,
        );
        longest = longest.max(size.cx);
    }
    SelectObject(dc, old);
    ReleaseDC(h, dc);
    SendMessageW(
        h,
        LB_SETHORIZONTALEXTENT,
        (longest + layout::scale(12, layout::dpi())) as usize,
        0,
    );
}
pub unsafe fn list_values(h: HWND) -> Vec<String> {
    (0..SendMessageW(h, LB_GETCOUNT, 0, 0).max(0) as usize)
        .map(|i| {
            let n = SendMessageW(h, LB_GETTEXTLEN, i, 0).max(0) as usize;
            let mut b = vec![0u16; n + 1];
            SendMessageW(h, LB_GETTEXT, i, b.as_mut_ptr() as isize);
            String::from_utf16_lossy(&b[..n])
        })
        .collect()
}
pub unsafe fn enable_rendering() {
    let s = &*STATE;
    let f = &s.render_fields;
    let c = &s.render_checks;
    let look = SendMessageW(f[0], CB_GETCURSEL, 0, 0) == 0;
    let directional = SendMessageW(f[10], CB_GETCURSEL, 0, 0) == 0;
    for i in 0..11 {
        let enabled = match i {
            0 => true,
            1 => !check(c[0]),
            2 | 3 => look && check(c[3]),
            4 | 5 => look && check(c[4]),
            6 | 8 | 9 => look && directional,
            7 | 10 => look,
            _ => false,
        };
        EnableWindow(f[i], enabled as i32);
        EnableWindow(s.render_labels[i], enabled as i32);
    }
    for i in 3..5 {
        EnableWindow(c[i], look as i32);
    }
    settext(
        s.render_hint,
        if !look {
            "F3D: background, colours and antialiasing."
        } else if directional {
            "Directional: one adjustable light."
        } else {
            "Studio: five lights that follow the camera."
        },
    );
}
pub unsafe fn render_config() -> anyhow::Result<Config> {
    let s = &*STATE;
    let f = &s.render_fields;
    let c = &s.render_checks;
    let mut cfg = Config::load()?;
    cfg.renderer = if SendMessageW(f[0], CB_GETCURSEL, 0, 0) == 0 {
        "look"
    } else {
        "f3d"
    }
    .into();
    cfg.transparent = check(c[0]);
    cfg.source_colours = check(c[1]);
    cfg.antialias = check(c[2]);
    if !cfg.transparent {
        cfg.background = get(f[1]);
    }
    if cfg.renderer == "look" {
        cfg.lighting_preset = if SendMessageW(f[10], CB_GETCURSEL, 0, 0) == 0 {
            "technical"
        } else {
            "f3d-match"
        }
        .into();
        cfg.ambient_occlusion = check(c[3]);
        cfg.edges = check(c[4]);
        if cfg.ambient_occlusion {
            cfg.ao_strength = get(f[2]).parse()?;
            cfg.ao_radius = get(f[3]).parse()?;
        }
        if cfg.edges {
            cfg.edge_strength = get(f[4]).parse()?;
            cfg.edge_width = get(f[5]).parse()?;
        }
        cfg.light_intensity = get(f[7]).parse()?;
        if cfg.lighting_preset == "technical" {
            cfg.ambient = get(f[6]).parse()?;
            cfg.light_direction = get(f[8]);
            cfg.light_colour = get(f[9]);
        }
    }
    cfg.validate()?;
    Ok(cfg)
}
pub unsafe fn pick(hwnd: HWND, folder: bool) -> anyhow::Result<Option<String>> {
    use windows::{
        Win32::{
            Foundation::HWND as Owner,
            System::Com::*,
            UI::Shell::{
                Common::COMDLG_FILTERSPEC, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
                FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
            },
        },
        core::*,
    };
    CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
    struct Com;
    impl Drop for Com {
        fn drop(&mut self) {
            unsafe { CoUninitialize() }
        }
    }
    let _com = Com;
    let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
    dialog.SetOptions(
        dialog.GetOptions()?
            | FOS_FORCEFILESYSTEM
            | if folder {
                FOS_PICKFOLDERS
            } else {
                FOS_FILEMUSTEXIST
            },
    )?;
    dialog.SetTitle(if folder {
        w!("Choose folder")
    } else {
        w!("Load preview file")
    })?;
    if !folder {
        dialog.SetFileTypes(&[COMDLG_FILTERSPEC {
            pszName: w!("3D models (*.step;*.stp;*.stl;*.glb)"),
            pszSpec: w!("*.step;*.stp;*.stl;*.glb"),
        }])?;
    }
    if let Err(e) = dialog.Show(Some(Owner(hwnd))) {
        if e.code().0 as u32 == 0x800704c7 {
            return Ok(None);
        }
        return Err(e.into());
    }
    let path = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
    let text = path.to_string();
    CoTaskMemFree(Some(path.0 as _));
    Ok(Some(text?))
}
