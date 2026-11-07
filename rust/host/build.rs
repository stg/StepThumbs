use std::{env, fs, io::Cursor, path::PathBuf};

fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let resource = root.join("assets/app.res");
    println!("cargo:rerun-if-changed={}", resource.display());
    println!("cargo:rustc-link-arg={}", resource.display());
    for theme in ["dark", "light"] {
        let source = root.join(format!("../app/assets/tray-{theme}.ico"));
        println!("cargo:rerun-if-changed={}", source.display());
        let ico = fs::read(source).unwrap();
        let count = u16::from_le_bytes(ico[4..6].try_into().unwrap()) as usize;
        for i in 0..count {
            let at = 6 + i * 16;
            let side = ico[at] as usize;
            let size = u32::from_le_bytes(ico[at + 8..at + 12].try_into().unwrap()) as usize;
            let offset = u32::from_le_bytes(ico[at + 12..at + 16].try_into().unwrap()) as usize;
            let image = &ico[offset..offset + size];
            let decoder = png::Decoder::new(Cursor::new(image));
            let mut reader = decoder.read_info().unwrap();
            let mut rgba = vec![0; reader.output_buffer_size().unwrap()];
            let info = reader.next_frame(&mut rgba).unwrap();
            assert_eq!(info.color_type, png::ColorType::Rgba);
            assert_eq!(info.width as usize, side);
            let mask_stride = side.div_ceil(32) * 4;
            let mut dib = vec![0u8; 40 + side * side * 4 + mask_stride * side];
            for (at, n) in [
                (0, 40u32),
                (4, side as u32),
                (8, (side * 2) as u32),
                (20, (side * side * 4) as u32),
            ] {
                dib[at..at + 4].copy_from_slice(&n.to_le_bytes());
            }
            dib[12..14].copy_from_slice(&1u16.to_le_bytes());
            dib[14..16].copy_from_slice(&32u16.to_le_bytes());
            for y in 0..side {
                for x in 0..side {
                    let src = (y * side + x) * 4;
                    let dst = 40 + ((side - 1 - y) * side + x) * 4;
                    dib[dst..dst + 4].copy_from_slice(&[
                        rgba[src + 2],
                        rgba[src + 1],
                        rgba[src],
                        rgba[src + 3],
                    ]);
                    if rgba[src + 3] == 0 {
                        dib[40 + side * side * 4 + (side - 1 - y) * mask_stride + x / 8] |=
                            0x80 >> (x % 8);
                    }
                }
            }
            fs::write(out.join(format!("{theme}-{side}.dib")), dib).unwrap();
        }
    }
    println!("cargo:rustc-link-arg-bin=StepThumbs=/SUBSYSTEM:WINDOWS");
    println!("cargo:rustc-link-arg=/STACK:262144,4096");
    println!("cargo:rustc-link-arg=/DYNAMICBASE");
    println!("cargo:rustc-link-arg=/NXCOMPAT");
    println!("cargo:rustc-link-arg=/HIGHENTROPYVA");
    println!("cargo:rustc-link-arg=/OPT:REF,ICF");
    let manifest = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
<trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges><requestedExecutionLevel level="asInvoker" uiAccess="false"/></requestedPrivileges></security></trustInfo>
<application xmlns="urn:schemas-microsoft-com:asm.v3"><windowsSettings><dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness></windowsSettings></application>
</assembly>"#;
    let manifest_path = out.join("host.manifest");
    fs::write(&manifest_path, manifest).unwrap();
    println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg=/MANIFESTINPUT:{}",
        manifest_path.display()
    );
}
