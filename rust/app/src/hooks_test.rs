use anyhow::{Result, bail};
use std::{path::Path, ptr::null_mut};
use winreg::{HKEY, RegKey, enums::*};
#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegOverridePredefKey(key: HKEY, new_key: HKEY) -> u32;
}
struct Overrides;
impl Drop for Overrides {
    fn drop(&mut self) {
        unsafe {
            RegOverridePredefKey(HKEY_CLASSES_ROOT, null_mut());
            RegOverridePredefKey(HKEY_CURRENT_USER, null_mut());
        }
    }
}
pub fn run(dll: &Path) -> Result<()> {
    let parent = RegKey::predef(HKEY_CURRENT_USER);
    let key_name = format!(
        r"Software\StepThumbsTest-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    let (root, _) = parent.create_subkey(&key_name)?;
    let (classes, _) = root.create_subkey(r"Software\Classes")?;
    let id = "{F3D0F3D0-7D37-4CCC-8999-4FDDDEBC5BE6}";
    for ext in [".step", ".stl"] {
        classes
            .create_subkey(format!(r"{ext}\shellex\{}", crate::common::hooks::HANDLER))?
            .0
            .set_value("", &id)?;
    }
    classes
        .create_subkey(format!(r"CLSID\{id}"))?
        .0
        .set_value("", &"F3D")?;
    let data = std::env::temp_dir().join(format!("StepThumbs-hook-test-{}", std::process::id()));
    std::fs::create_dir_all(&data)?;
    unsafe {
        std::env::set_var("STEPTHUMBNAIL_DATA_DIR", &data);
    }
    let result = (|| -> Result<()> {
        unsafe {
            if RegOverridePredefKey(HKEY_CURRENT_USER, root.raw_handle()) != 0 {
                bail!("Cannot isolate HKCU")
            };
            if RegOverridePredefKey(HKEY_CLASSES_ROOT, classes.raw_handle()) != 0 {
                RegOverridePredefKey(HKEY_CURRENT_USER, null_mut());
                bail!("Cannot isolate HKCR")
            }
        }
        let _guard = Overrides;
        if crate::common::hooks::status(".step").provider != "F3D" {
            bail!("Initial provider was not F3D")
        }
        crate::common::hooks::install_with_notify(&[".step".into()], dll, false)?;
        if !crate::common::hooks::status(".step").ours
            || crate::common::hooks::status(".stl").provider != "F3D"
        {
            bail!("Installation modified an unselected provider")
        }
        let backup: serde_json::Value =
            serde_json::from_slice(&std::fs::read(data.join("previous-associations.json"))?)?;
        if backup[0]["Value"].as_str() != Some(id) {
            bail!("Previous provider was not saved")
        }
        crate::common::hooks::install_with_notify(&[], dll, false)?;
        if crate::common::hooks::status(".step").provider != "F3D"
            || crate::common::hooks::status(".stl").provider != "F3D"
        {
            bail!("Provider restoration failed")
        }
        Ok(())
    })();
    drop(classes);
    drop(root);
    let _ = parent.delete_subkey_all(&key_name);
    result?;
    println!("Isolated hook backup, selection and restoration passed");
    Ok(())
}
