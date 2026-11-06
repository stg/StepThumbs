use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};
use winreg::{RegKey, enums::*};
pub const EXTENSIONS: [&str; 4] = [".step", ".stp", ".stl", ".glb"];
pub const CLASS: &str = "{86F9B3AC-39AE-4EB1-AD06-819BE42B2019}";
pub const HANDLER: &str = "{E357FCCD-A995-4576-B01F-234630154E96}";
const STARTUP: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_NAME: &str = "StepThumbnail Preflight";
pub fn installed_executable() -> Option<std::path::PathBuf> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(format!(r"Software\Classes\CLSID\{CLASS}\InprocServer32"))
        .ok()
        .and_then(|key| key.get_value::<String, _>("").ok())
        .map(|dll| std::path::PathBuf::from(dll).with_file_name("StepThumbs.exe"))
}
#[derive(Clone, Debug)]
pub struct HookStatus {
    pub extension: String,
    pub provider: String,
    pub ours: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Backup {
    extension: String,
    had_value: bool,
    value: Option<String>,
    #[serde(default)]
    kind: String,
}
fn key(ext: &str) -> String {
    format!(r"Software\Classes\{ext}\shellex\{HANDLER}")
}
pub fn status(ext: &str) -> HookStatus {
    let root = RegKey::predef(HKEY_CLASSES_ROOT);
    let read = |p: String| {
        root.open_subkey(p)
            .ok()
            .and_then(|k| k.get_value::<String, _>("").ok())
    };
    let id = read(format!(r"{ext}\shellex\{HANDLER}"))
        .or_else(|| read(format!(r"SystemFileAssociations\{ext}\shellex\{HANDLER}")))
        .or_else(|| read(ext.into()).and_then(|p| read(format!(r"{p}\shellex\{HANDLER}"))));
    let ours = id.as_ref().is_some_and(|s| s.eq_ignore_ascii_case(CLASS));
    let provider = if ours {
        "StepThumbs installed".into()
    } else {
        match id {
            None => "No registered thumbnail provider".into(),
            Some(id) => {
                let name = read(format!(r"CLSID\{id}")).unwrap_or(id);
                if name.to_lowercase().contains("f3d") {
                    "F3D".into()
                } else {
                    name
                }
            }
        }
    };
    HookStatus {
        extension: ext.into(),
        provider,
        ours,
    }
}
pub fn register_component(dll: &Path) -> Result<()> {
    if !dll.is_file() {
        bail!("The Explorer component is missing.")
    }
    let root = RegKey::predef(HKEY_CURRENT_USER);
    let (class, _) = root.create_subkey(format!(r"Software\Classes\CLSID\{CLASS}"))?;
    class.set_value("", &"StepThumbs thumbnail provider")?;
    class.set_value("DisableProcessIsolation", &1u32)?;
    let (server, _) = class.create_subkey("InprocServer32")?;
    server.set_value("", &dll.to_string_lossy().as_ref())?;
    server.set_value("ThreadingModel", &"Apartment")?;
    Ok(())
}
pub fn install(selected: &[String], dll: &Path) -> Result<()> {
    install_with_notify(selected, dll, true)
}
pub fn install_with_notify(selected: &[String], dll: &Path, notify: bool) -> Result<()> {
    if selected.iter().any(|s| !EXTENSIONS.contains(&s.as_str())) {
        bail!("Unsupported extension")
    }
    register_component(dll)?;
    let path = crate::data_dir().join("previous-associations.json");
    let mut backups: Vec<Backup> = if path.exists() {
        {
            let bytes = fs::read(&path)?;
            serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes))?
        }
    } else {
        Vec::new()
    };
    let root = RegKey::predef(HKEY_CURRENT_USER);
    for ext in EXTENSIONS {
        let current = status(ext);
        if selected.iter().any(|v| v == ext) && !current.ours {
            if !backups.iter().any(|b| b.extension == ext) {
                let old = root
                    .open_subkey(key(ext))
                    .ok()
                    .and_then(|k| k.get_value::<String, _>("").ok());
                backups.push(Backup {
                    extension: ext.into(),
                    had_value: old.is_some(),
                    value: old,
                    kind: "String".into(),
                });
                crate::cache::atomic_write(&path, &serde_json::to_vec_pretty(&backups)?)?;
            }
            root.create_subkey(key(ext))?.0.set_value("", &CLASS)?;
        } else if !selected.iter().any(|v| v == ext) && current.ours {
            let Some(index) = backups.iter().position(|b| b.extension == ext) else {
                bail!("The previous provider for {ext} was not saved; it was left unchanged.")
            };
            let b = &backups[index];
            let current = root.open_subkey_with_flags(key(ext), KEY_WRITE)?;
            if b.had_value {
                current.set_value("", &b.value.as_deref().unwrap_or(""))?;
            } else {
                let _ = current.delete_value("");
            }
            backups.remove(index);
            crate::cache::atomic_write(&path, &serde_json::to_vec_pretty(&backups)?)?;
        }
    }
    if notify {
        unsafe {
            windows_sys::Win32::UI::Shell::SHChangeNotify(
                0x08000000,
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        }
    };
    Ok(())
}
pub fn startup(enabled: bool, exe: &Path) -> Result<()> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(STARTUP)?;
    if enabled {
        key.set_value(STARTUP_NAME, &format!("\"{}\" --background", exe.display()))?
    } else {
        let _ = key.delete_value(STARTUP_NAME);
    }
    Ok(())
}
pub fn startup_enabled() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(STARTUP)
        .ok()
        .and_then(|k| k.get_value::<String, _>(STARTUP_NAME).ok())
        .is_some()
}
