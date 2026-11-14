//! Bakes the code-drawn icon into the .exe so Explorer and the taskbar show it.

use std::path::PathBuf;

include!("src/icon.rs");

/// A classic .ico: one 32-bit BMP image per size.
fn ico(sizes: &[usize]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&n| {
            let rgba = icon_rgba(n);
            let mask_row = ((n + 31) / 32) * 4;
            let mut img = Vec::with_capacity(40 + n * n * 4 + mask_row * n);
            img.extend_from_slice(&40u32.to_le_bytes());
            img.extend_from_slice(&(n as i32).to_le_bytes());
            img.extend_from_slice(&(2 * n as i32).to_le_bytes()); // colour + mask
            img.extend_from_slice(&1u16.to_le_bytes());
            img.extend_from_slice(&32u16.to_le_bytes());
            img.extend_from_slice(&[0u8; 24]);
            for y in (0..n).rev() {
                for x in 0..n {
                    let i = (y * n + x) * 4;
                    img.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
                }
            }
            img.resize(img.len() + mask_row * n, 0);
            img
        })
        .collect();

    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len();
    for (&n, img) in sizes.iter().zip(&images) {
        let dim = if n >= 256 { 0 } else { n as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(img.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += img.len();
    }
    for img in images {
        out.extend_from_slice(&img);
    }
    out
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let path = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("salvage.ico");
    std::fs::write(&path, ico(&[16, 24, 32, 48, 64, 256])).expect("write icon");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(&path.to_string_lossy());
    if let Err(e) = res.compile() {
        println!("cargo:warning=icona non incorporata nell'exe: {e}");
    }
}
