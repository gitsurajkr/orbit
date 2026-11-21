//! Embeds the ORBIT icon into orbit.exe (shown in Explorer, the taskbar and Alt+Tab).

include!("src/icon.rs");

fn ico(sizes: &[u32]) -> Vec<u8> {
    // ICO with 32-bit BMP images (bottom-up BGRA + empty AND mask).
    let mut images = Vec::new();
    for &s in sizes {
        let rgba = orbit_icon_rgba(s);
        let mut img = Vec::new();
        let header: [u32; 10] = [40, s, s * 2, (1 | (32 << 16)), 0, 0, 0, 0, 0, 0];
        for (i, v) in header.iter().enumerate() {
            if i == 3 {
                img.extend_from_slice(&1u16.to_le_bytes());
                img.extend_from_slice(&32u16.to_le_bytes());
            } else {
                img.extend_from_slice(&v.to_le_bytes());
            }
        }
        for y in (0..s).rev() {
            for x in 0..s {
                let i = ((y * s + x) * 4) as usize;
                img.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
            }
        }
        let mask_row = s.div_ceil(32) * 4;
        img.extend(std::iter::repeat_n(0u8, (mask_row * s) as usize));
        images.push((s, img));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (s, img) in &images {
        let dim = if *s >= 256 { 0 } else { *s as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(img.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for (_, img) in images {
        out.extend_from_slice(&img);
    }
    out
}

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("orbit.ico");
    std::fs::write(&out, ico(&[16, 24, 32, 48, 64, 128, 256])).unwrap();
    let mut res = winresource::WindowsResource::new();
    res.set_icon(out.to_str().unwrap());
    res.set("ProductName", "ORBIT");
    res.set("FileDescription", "ORBIT remote desktop");
    if let Err(e) = res.compile() {
        println!("cargo:warning=icon not embedded: {e}");
    }
}
