#[path = "src/logo.rs"]
#[allow(dead_code)]
mod logo;

use std::path::PathBuf;

fn main() {
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent-dark".into()),
    )
    .expect("compiling the Slint UI");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = out.join("molehill.ico");
    std::fs::write(&ico, build_ico(&[16, 20, 24, 32, 40, 48, 64, 256])).unwrap();
    println!("cargo:rerun-if-changed=src/logo.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon(ico.to_str().unwrap())
            .set("ProductName", "Molehill")
            .set("FileDescription", "Molehill VPN")
            .set("LegalCopyright", "Copyright © Shota Giorgobiani. MIT License.")
            // winit sets per-monitor DPI awareness at runtime too; the
            // manifest makes it explicit, and lets us run as a plain user.
            .set_manifest(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges>
    <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
  </requestedPrivileges></security></trustInfo>
  <application xmlns="urn:schemas-microsoft-com:asm.v3"><windowsSettings>
    <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
  </windowsSettings></application>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1"><application>
    <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
  </application></compatibility>
</assembly>"#,
            )
            .compile()
            .expect("embedding the Windows resources");
    }
}

/// A multi-size .ico with 32-bit BMP entries.
fn build_ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| bmp_entry(s, &logo::render_app_icon(s))).collect();
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&s, img) in sizes.iter().zip(&images) {
        let dim = if s >= 256 { 0 } else { s as u8 };
        ico.extend_from_slice(&[dim, dim, 0, 0, 1, 0, 32, 0]);
        ico.extend_from_slice(&(img.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in images {
        ico.extend_from_slice(&img);
    }
    ico
}

fn bmp_entry(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mask_row = size.div_ceil(32) * 4;
    let mut v = Vec::new();
    v.extend_from_slice(&40u32.to_le_bytes());
    v.extend_from_slice(&(size as i32).to_le_bytes());
    v.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&32u16.to_le_bytes());
    v.extend_from_slice(&[0u8; 24]);
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            v.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    v.extend(std::iter::repeat_n(0u8, (mask_row * size) as usize));
    v
}
