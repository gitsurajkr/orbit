//! Turns the server's pointer-shape messages into Windows cursors for the viewer window.
//! Standard shapes use this PC's own cursors (so they match its theme and size); custom ones
//! are built from the image the server sent, once, and cached by its key.

use proto::ctrl::{cursor, Ctrl};
use std::collections::HashMap;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub struct Cursors {
    custom: HashMap<u32, HCURSOR>,
}

impl Cursors {
    pub fn new() -> Cursors {
        Cursors { custom: HashMap::new() }
    }

    /// The cursor to show after message `m` (a null handle hides the pointer), or `None` if `m`
    /// doesn't change what's shown.
    pub fn handle(&mut self, m: Ctrl) -> Option<HCURSOR> {
        match m {
            Ctrl::CursorImage { key, hot_x, hot_y, width, height, bgra } => {
                if let std::collections::hash_map::Entry::Vacant(e) = self.custom.entry(key) {
                    match make_cursor(width, height, hot_x, hot_y, &bgra) {
                        Some(c) => {
                            e.insert(c);
                        }
                        None => eprintln!("couldn't build the remote pointer ({width}x{height})"),
                    }
                }
                None
            }
            // Never hide the pointer: older ORBIT versions sent "hidden" whenever the remote PC
            // had no mouse plugged in, which made the pointer vanish. Keep the last shape.
            Ctrl::Cursor { kind: cursor::HIDDEN, .. } => None,
            Ctrl::Cursor { kind: cursor::CUSTOM, key } => Some(self.custom.get(&key).copied().unwrap_or_else(arrow)),
            Ctrl::Cursor { kind, .. } => {
                let id = cursor::STANDARD.get(kind as usize - 1).copied().unwrap_or(cursor::STANDARD[0]);
                Some(unsafe { LoadCursorW(None, PCWSTR(id as usize as *const u16)) }.unwrap_or_else(|_| arrow()))
            }
            _ => None,
        }
    }
}

impl Drop for Cursors {
    fn drop(&mut self) {
        for c in self.custom.values() {
            unsafe {
                let _ = DestroyCursor(*c);
            }
        }
    }
}

fn arrow() -> HCURSOR {
    unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default()
}

/// Builds a cursor from top-down BGRA with straight alpha.
fn make_cursor(w: u16, h: u16, hot_x: u16, hot_y: u16, bgra: &[u8]) -> Option<HCURSOR> {
    if w == 0 || h == 0 || bgra.len() != w as usize * h as usize * 4 {
        return None;
    }
    unsafe {
        // A 32-bit DIB section with an alpha channel: Windows then uses the alpha, not the mask.
        let header = BITMAPV5HEADER {
            bV5Size: std::mem::size_of::<BITMAPV5HEADER>() as u32,
            bV5Width: w as i32,
            bV5Height: -(h as i32),
            bV5Planes: 1,
            bV5BitCount: 32,
            bV5Compression: BI_BITFIELDS,
            bV5RedMask: 0x00FF_0000,
            bV5GreenMask: 0x0000_FF00,
            bV5BlueMask: 0x0000_00FF,
            bV5AlphaMask: 0xFF00_0000,
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(None, &header as *const _ as *const BITMAPINFO, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, bgra.len());
        let mask = CreateBitmap(w as i32, h as i32, 1, 1, None);
        let ii = ICONINFO { fIcon: false.into(), xHotspot: hot_x as u32, yHotspot: hot_y as u32, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&ii);
        let _ = DeleteObject(color);
        let _ = DeleteObject(mask);
        icon.ok().map(|i| HCURSOR(i.0))
    }
}

/// Shows `c` right away if the pointer is over `hwnd`'s client area (otherwise the next
/// WM_SETCURSOR picks it up).
pub fn apply_now(hwnd: HWND, c: HCURSOR) {
    unsafe {
        let mut p = windows::Win32::Foundation::POINT::default();
        if GetCursorPos(&mut p).is_err() || WindowFromPoint(p) != hwnd {
            return;
        }
        let _ = ScreenToClient(hwnd, &mut p);
        let mut r = windows::Win32::Foundation::RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        if p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom {
            SetCursor(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_custom_cursor_and_falls_back_to_arrow() {
        let mut c = Cursors::new();
        let img = Ctrl::CursorImage { key: 7, hot_x: 1, hot_y: 1, width: 4, height: 4, bgra: vec![200; 64] };
        assert_eq!(c.handle(img), None);
        let custom = c.handle(Ctrl::Cursor { kind: cursor::CUSTOM, key: 7 }).unwrap();
        assert!(!custom.is_invalid() && custom != arrow());
        assert_eq!(c.handle(Ctrl::Cursor { kind: cursor::CUSTOM, key: 8 }), Some(arrow()), "unknown key");
        assert_eq!(c.handle(Ctrl::Cursor { kind: cursor::HIDDEN, key: 0 }), None, "never hides the pointer");
        assert_eq!(c.handle(Ctrl::Cursor { kind: 2, key: 0 }), Some(unsafe { LoadCursorW(None, IDC_IBEAM) }.unwrap()));
        assert_eq!(c.handle(Ctrl::Cursor { kind: 200, key: 0 }), Some(arrow()), "unknown kind");
    }
}
