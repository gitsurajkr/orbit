//! Clipboard images: Windows' DIB format ⇄ PNG (what travels: a 4K screenshot is ~33 MB as
//! a DIB and usually 1–3 MB as PNG).

use anyhow::{bail, ensure, Context, Result};

/// Largest image accepted, in pixels (e.g. 8192 × 8192).
const MAX_PIXELS: u64 = 64 << 20;

/// An 8-bit RGBA image, top-down rows.
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

/// Reads a packed DIB (`CF_DIB` / `CF_DIBV5`: header, optional masks/palette, pixels).
/// Handles the 24- and 32-bit uncompressed images that screenshots and apps put there.
pub fn dib_to_rgba(dib: &[u8]) -> Result<Rgba> {
    ensure!(dib.len() >= 40, "DIB too short");
    let header = u32_at(dib, 0) as usize;
    ensure!(header >= 40 && header <= dib.len(), "bad DIB header size {header}");
    let w = u32_at(dib, 4) as i32;
    let h = u32_at(dib, 8) as i32;
    let bpp = u16_at(dib, 14);
    let compression = u32_at(dib, 16);
    let colors_used = u32_at(dib, 32) as usize;
    const BI_RGB: u32 = 0;
    const BI_BITFIELDS: u32 = 3;
    ensure!(w > 0 && h != 0, "bad DIB size {w}x{h}");
    ensure!(bpp == 24 || bpp == 32, "unsupported DIB: {bpp} bits per pixel");
    ensure!(compression == BI_RGB || compression == BI_BITFIELDS, "unsupported DIB compression {compression}");
    let (width, height) = (w as u32, h.unsigned_abs());
    ensure!(width as u64 * height as u64 <= MAX_PIXELS, "image too big");
    // A 40-byte header with BI_BITFIELDS is followed by three color masks; bigger headers
    // contain them. Assumed to be the standard BGR order (what Windows and apps use).
    let masks = if compression == BI_BITFIELDS && header == 40 { 12 } else { 0 };
    let offset = header + masks + colors_used * 4;
    let stride = (width as usize * bpp as usize).div_ceil(32) * 4;
    ensure!(dib.len() >= offset + stride * height as usize, "DIB pixels cut short");
    let bottom_up = h > 0;
    let step = bpp as usize / 8;
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    let mut any_alpha = false;
    for y in 0..height as usize {
        let src_y = if bottom_up { height as usize - 1 - y } else { y };
        let row = &dib[offset + src_y * stride..];
        for x in 0..width as usize {
            let s = &row[x * step..x * step + step];
            let d = &mut pixels[(y * width as usize + x) * 4..][..4];
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = if step == 4 { s[3] } else { 255 };
            any_alpha |= d[3] != 0;
        }
    }
    // Most 32-bit DIBs leave alpha at 0 meaning "opaque".
    if !any_alpha {
        pixels.chunks_exact_mut(4).for_each(|p| p[3] = 255);
    }
    Ok(Rgba { width, height, pixels })
}

/// A packed 32-bit bottom-up DIB (BITMAPINFOHEADER + BGRA pixels) for `CF_DIB`.
pub fn rgba_to_dib(img: &Rgba) -> Vec<u8> {
    let (w, h) = (img.width as usize, img.height as usize);
    let mut out = Vec::with_capacity(40 + w * h * 4);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes()); // positive: bottom-up, the most compatible
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&((w * h * 4) as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]); // resolution, colors used/important
    for y in (0..h).rev() {
        for p in img.pixels[y * w * 4..(y + 1) * w * 4].chunks_exact(4) {
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    out
}

pub fn encode_png(img: &Rgba) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut e = png::Encoder::new(&mut out, img.width, img.height);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        e.set_compression(png::Compression::Fast);
        let mut w = e.write_header()?;
        w.write_image_data(&img.pixels)?;
    }
    Ok(out)
}

pub fn decode_png(bytes: &[u8]) -> Result<Rgba> {
    let mut d = png::Decoder::new(std::io::Cursor::new(bytes));
    d.set_transformations(png::Transformations::normalize_to_color8());
    let mut r = d.read_info().context("not a PNG")?;
    let (w, h) = (r.info().width, r.info().height);
    ensure!(w as u64 * h as u64 <= MAX_PIXELS, "image too big");
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf)?;
    let n = w as usize * h as usize;
    let mut pixels = Vec::with_capacity(n * 4);
    match info.color_type {
        png::ColorType::Rgba => pixels.extend_from_slice(&buf[..n * 4]),
        png::ColorType::Rgb => buf[..n * 3].chunks_exact(3).for_each(|p| pixels.extend_from_slice(&[p[0], p[1], p[2], 255])),
        png::ColorType::GrayscaleAlpha => buf[..n * 2].chunks_exact(2).for_each(|p| pixels.extend_from_slice(&[p[0], p[0], p[0], p[1]])),
        png::ColorType::Grayscale => buf[..n].iter().for_each(|&g| pixels.extend_from_slice(&[g, g, g, 255])),
        other => bail!("unexpected PNG color type {other:?}"),
    }
    Ok(Rgba { width: w, height: h, pixels })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Rgba {
        // 3x2, distinct pixels, one half-transparent
        let pixels = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 10, 20, 30, 128, 40, 50, 60, 255, 70, 80, 90, 255];
        Rgba { width: 3, height: 2, pixels }
    }

    #[test]
    fn dib_round_trip() {
        let img = sample();
        let back = dib_to_rgba(&rgba_to_dib(&img)).unwrap();
        assert_eq!((back.width, back.height), (3, 2));
        assert_eq!(back.pixels, img.pixels);
    }

    #[test]
    fn png_round_trip() {
        let img = sample();
        let back = decode_png(&encode_png(&img).unwrap()).unwrap();
        assert_eq!(back.pixels, img.pixels);
    }

    #[test]
    fn top_down_24_bit_dib_with_zero_alpha_rule() {
        // 2x1, 24-bit, top-down (negative height); rows padded to 4 bytes.
        let mut d = vec![0u8; 40];
        d[0..4].copy_from_slice(&40u32.to_le_bytes());
        d[4..8].copy_from_slice(&2i32.to_le_bytes());
        d[8..12].copy_from_slice(&(-1i32).to_le_bytes());
        d[12..14].copy_from_slice(&1u16.to_le_bytes());
        d[14..16].copy_from_slice(&24u16.to_le_bytes());
        d.extend_from_slice(&[1, 2, 3, 4, 5, 6, 0, 0]); // BGR BGR + padding
        let img = dib_to_rgba(&d).unwrap();
        assert_eq!(img.pixels, vec![3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(dib_to_rgba(&[0; 10]).is_err());
        assert!(decode_png(b"not a png").is_err());
    }
}
