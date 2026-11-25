//! Watches this PC's mouse pointer shape so the viewer can give its own pointer the same shape.
//!
//! The video never contains the pointer (Desktop Duplication leaves it out), and the viewer
//! uses its local pointer so it moves with zero lag. What's missing is only the *shape*: the
//! I-beam over text, resize arrows on window edges, the hand over links. (A pointer the remote
//! PC hides keeps its last shape here: see `poll`.)
//!
//! [`CursorWatch::poll`] reads `GetCursorInfo` (cheap; called every ~16 ms) and only produces a
//! message when the shape changed:
//! - Windows' standard cursors are system-wide shared handles, so they're recognized by handle
//!   and sent as a 1-byte kind.
//! - Anything else (an app's own pointer) is read back as a BGRA image, sent once, and
//!   afterwards referred to by a hash of its pixels.

use proto::ctrl::{cursor, Ctrl};
use std::collections::HashSet;
use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub struct CursorWatch {
    standard: Vec<(isize, u8)>,
    /// Last (handle, showing) seen, to report changes only.
    last: Option<(isize, bool)>,
    /// Custom image keys the client already has.
    sent: HashSet<u32>,
}

impl Default for CursorWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorWatch {
    pub fn new() -> CursorWatch {
        let standard = cursor::STANDARD
            .iter()
            .enumerate()
            .filter_map(|(i, &id)| unsafe { LoadCursorW(None, PCWSTR(id as usize as *const u16)) }.ok().map(|h| (h.0 as isize, i as u8 + 1)))
            .collect();
        CursorWatch { standard, last: None, sent: HashSet::new() }
    }

    /// Appends the messages needed to show the current pointer, if it changed since last time.
    pub fn poll(&mut self, out: &mut Vec<Ctrl>) {
        let mut ci = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
        if unsafe { GetCursorInfo(&mut ci) }.is_err() {
            return; // e.g. the secure desktop is up; keep the last shape
        }
        // A pointer Windows reports as hidden keeps its last shape in the viewer: the viewer's
        // pointer is never hidden. Windows hides its cursor whenever no mouse is plugged in (a
        // PC controlled remotely often has none), and SM_MOUSEPRESENT can't tell (virtual mouse
        // drivers make it almost always 1), so hiding it made the pointer vanish for good.
        let showing = ci.flags.0 & CURSOR_SHOWING.0 != 0;
        let h = ci.hCursor.0 as isize;
        if !showing || h == 0 {
            return;
        }
        let state = (h, showing);
        if self.last == Some(state) {
            return;
        }
        self.last = Some(state);
        if let Some(&(_, kind)) = self.standard.iter().find(|(sh, _)| *sh == h) {
            out.push(Ctrl::Cursor { kind, key: 0 });
            return;
        }
        match read_image(ci.hCursor) {
            Some(img) => {
                let key = cursor::image_key(img.width, img.height, img.hot_x, img.hot_y, &img.bgra);
                if self.sent.insert(key) {
                    out.push(Ctrl::CursorImage { key, hot_x: img.hot_x, hot_y: img.hot_y, width: img.width, height: img.height, bgra: img.bgra });
                }
                out.push(Ctrl::Cursor { kind: cursor::CUSTOM, key });
            }
            None => out.push(Ctrl::Cursor { kind: 1, key: 0 }), // arrow
        }
    }
}

pub struct Image {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    /// Top-down BGRA, straight alpha.
    pub bgra: Vec<u8>,
}

/// Reads a bitmap as top-down 32-bit BGRA rows.
unsafe fn dib_bits(hdc: HDC, bmp: HBITMAP, w: i32, h: i32) -> Option<Vec<u8>> {
    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let lines = GetDIBits(hdc, bmp, 0, h as u32, Some(buf.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS);
    (lines == h).then_some(buf)
}

/// The pointer's pixels. Color pointers keep their alpha (or get it from the mask if they have
/// none); old black-and-white pointers are converted, with "invert screen" pixels drawn black.
pub fn read_image(hc: HCURSOR) -> Option<Image> {
    unsafe {
        let mut ii = ICONINFO::default();
        GetIconInfo(HICON(hc.0), &mut ii).ok()?;
        let result = (|| {
            let mut bm = BITMAP::default();
            if GetObjectW(ii.hbmMask, std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) == 0 {
                return None;
            }
            let color = !ii.hbmColor.is_invalid();
            let (w, h) = (bm.bmWidth, if color { bm.bmHeight } else { bm.bmHeight / 2 });
            if w <= 0 || h <= 0 || w > cursor::MAX_SIZE as i32 || h > cursor::MAX_SIZE as i32 {
                return None;
            }
            let hdc = GetDC(None);
            let img = (|| {
                // Mask rows: for color pointers the AND mask alone; for monochrome ones the AND
                // mask on top of the XOR mask (2h rows). A set mask bit reads back as white.
                let mask = dib_bits(hdc, ii.hbmMask, w, if color { h } else { 2 * h })?;
                let set = |buf: &[u8], i: usize| buf[i * 4] != 0;
                let n = (w * h) as usize;
                if color {
                    let mut px = dib_bits(hdc, ii.hbmColor, w, h)?;
                    if px.chunks_exact(4).all(|p| p[3] == 0) {
                        for i in 0..n {
                            px[i * 4 + 3] = if set(&mask, i) { 0 } else { 255 };
                        }
                    }
                    Some(px)
                } else {
                    let mut px = vec![0u8; n * 4];
                    for i in 0..n {
                        let (and, xor) = (set(&mask, i), set(&mask, n + i));
                        let v = match (and, xor) {
                            (false, false) => [0, 0, 0, 255],
                            (false, true) => [255, 255, 255, 255],
                            (true, false) => [0, 0, 0, 0],
                            (true, true) => [0, 0, 0, 255], // "invert": black shows on most backgrounds
                        };
                        px[i * 4..i * 4 + 4].copy_from_slice(&v);
                    }
                    Some(px)
                }
            })();
            ReleaseDC(None, hdc);
            Some(Image { width: w as u16, height: h as u16, hot_x: ii.xHotspot as u16, hot_y: ii.yHotspot as u16, bgra: img? })
        })();
        let _ = DeleteObject(ii.hbmMask);
        if !ii.hbmColor.is_invalid() {
            let _ = DeleteObject(ii.hbmColor);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_every_standard_cursor_as_an_image() {
        // Not what poll() sends for these (they go by kind), but they cover both color and
        // black-and-white pointers.
        for &id in &cursor::STANDARD {
            let h = unsafe { LoadCursorW(None, PCWSTR(id as usize as *const u16)) }.unwrap();
            let img = read_image(h).unwrap_or_else(|| panic!("cursor {id}"));
            assert_eq!(img.bgra.len(), img.width as usize * img.height as usize * 4);
            assert!(img.bgra.chunks_exact(4).any(|p| p[3] == 255), "cursor {id} fully transparent");
            assert!(img.bgra.chunks_exact(4).any(|p| p[3] == 0), "cursor {id} fully opaque");
        }
    }

    #[test]
    fn poll_reports_only_changes() {
        let mut w = CursorWatch::new();
        let mut out = Vec::new();
        w.poll(&mut out);
        // The first poll describes the pointer, unless Windows has it hidden right now (e.g. while
        // someone types), in which case nothing is sent by design.
        out.clear();
        w.poll(&mut out);
        assert!(out.is_empty(), "nothing changed, nothing sent: {out:?}");
    }
}
