//! Watches this PC's clipboard and writes the other PC's copies into it.
//!
//! A hidden message-only window on its own thread is told about every change
//! (`AddClipboardFormatListener`). Writes from the other PC are done on that same thread, so it
//! can tell them apart from the user's own copies:
//! - after writing, it notes Windows' clipboard sequence number; notifications up to that
//!   number are its own write, not a copy to send back (no ping-pong between the PCs);
//! - it also remembers a hash of what came from the other PC, so a clipboard manager that
//!   re-copies the same content doesn't echo it either.

use crate::image;
use anyhow::{bail, Context, Result};
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Largest text copied (bytes of UTF-8).
pub const MAX_TEXT: usize = 16 << 20;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Clip {
    Text(String),
    /// A PNG file's bytes.
    Png(Vec<u8>),
    /// Files / folders copied in Explorer (full paths on this PC).
    Files(Vec<std::path::PathBuf>),
}

impl Clip {
    fn hash64(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }
}

const WM_APPLY: u32 = WM_APP + 1;

type OnCopy = Box<dyn Fn(Clip) + Send>;

struct State {
    on_copy: OnCopy,
    inbox: Arc<Mutex<Vec<Clip>>>,
    /// Sequence number right after our own last write.
    own_seq: u32,
    /// Hash of the last content that came from the other PC (or was sent to it).
    last_hash: Option<u64>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Hands content from the other PC to the watcher thread. Cheap to clone, usable anywhere.
#[derive(Clone)]
pub struct Setter {
    hwnd: isize,
    inbox: Arc<Mutex<Vec<Clip>>>,
}

impl Setter {
    pub fn set(&self, c: Clip) {
        self.inbox.lock().unwrap().push(c);
        unsafe {
            let _ = PostMessageW(HWND(self.hwnd as _), WM_APPLY, WPARAM(0), LPARAM(0));
        }
    }
}

/// The running watcher; stops when dropped.
pub struct Watcher {
    setter: Setter,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    /// Starts watching; `on_copy` is called (on the watcher thread) for every copy the user
    /// makes on this PC.
    pub fn start(on_copy: impl Fn(Clip) + Send + 'static) -> Result<Watcher> {
        let inbox = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = std::sync::mpsc::channel::<Result<isize, String>>();
        let ib = inbox.clone();
        let on_copy: OnCopy = Box::new(on_copy);
        let thread = std::thread::Builder::new().name("clipboard".into()).spawn(move || unsafe {
            let hwnd = match create_window() {
                Ok(h) => h,
                Err(e) => {
                    let _ = tx.send(Err(format!("{e:#}")));
                    return;
                }
            };
            STATE.with(|s| *s.borrow_mut() = Some(State { on_copy, inbox: ib, own_seq: GetClipboardSequenceNumber(), last_hash: None }));
            if let Err(e) = AddClipboardFormatListener(hwnd) {
                let _ = tx.send(Err(format!("AddClipboardFormatListener: {e}")));
                let _ = DestroyWindow(hwnd);
                return;
            }
            let _ = tx.send(Ok(hwnd.0 as isize));
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                DispatchMessageW(&msg);
            }
            STATE.with(|s| *s.borrow_mut() = None);
        })?;
        let hwnd = rx.recv().context("clipboard thread ended")?.map_err(anyhow::Error::msg)?;
        Ok(Watcher { setter: Setter { hwnd, inbox }, thread: Some(thread) })
    }

    pub fn setter(&self) -> Setter {
        self.setter.clone()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(HWND(self.setter.hwnd as _), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

unsafe fn create_window() -> Result<HWND> {
    let instance: HINSTANCE = GetModuleHandleW(None)?.into();
    let class = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: instance, lpszClassName: w!("orbit-clipboard"), ..Default::default() };
    RegisterClassW(&class); // fails harmlessly if already registered
    Ok(CreateWindowExW(WINDOW_EX_STYLE::default(), w!("orbit-clipboard"), w!(""), WINDOW_STYLE::default(), 0, 0, 0, 0, HWND_MESSAGE, None, instance, None)?)
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CLIPBOARDUPDATE => {
                STATE.with(|s| {
                    let mut s = s.borrow_mut();
                    let Some(s) = s.as_mut() else { return };
                    let seq = GetClipboardSequenceNumber();
                    if seq.wrapping_sub(s.own_seq) as i32 <= 0 {
                        return; // our own write
                    }
                    match read(hwnd) {
                        Ok(Some(c)) => {
                            let h = c.hash64();
                            if s.last_hash != Some(h) {
                                s.last_hash = Some(h);
                                (s.on_copy)(c);
                            }
                        }
                        Ok(None) => {}
                        Err(e) => eprintln!("clipboard: {e:#}"),
                    }
                });
                LRESULT(0)
            }
            WM_APPLY => {
                STATE.with(|s| {
                    let mut s = s.borrow_mut();
                    let Some(s) = s.as_mut() else { return };
                    let newest = s.inbox.lock().unwrap().drain(..).last();
                    if let Some(c) = newest {
                        let h = c.hash64();
                        match write(hwnd, &c) {
                            Ok(()) => s.last_hash = Some(h),
                            Err(e) => eprintln!("clipboard: {e:#}"),
                        }
                        s.own_seq = GetClipboardSequenceNumber();
                    }
                });
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = RemoveClipboardFormatListener(hwnd);
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// Opens the clipboard, retrying briefly: another app may hold it for a moment.
struct Open;
impl Open {
    unsafe fn new(hwnd: HWND) -> Result<Open> {
        for _ in 0..10 {
            if OpenClipboard(hwnd).is_ok() {
                return Ok(Open);
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        bail!("the clipboard is busy (another app holds it)")
    }
}
impl Drop for Open {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

unsafe fn global_bytes(h: HANDLE) -> Option<Vec<u8>> {
    let g = HGLOBAL(h.0);
    let p = GlobalLock(g) as *const u8;
    if p.is_null() {
        return None;
    }
    let v = std::slice::from_raw_parts(p, GlobalSize(g)).to_vec();
    let _ = GlobalUnlock(g);
    Some(v)
}

fn png_format() -> u32 {
    unsafe { RegisterClipboardFormatW(w!("PNG")) }
}

/// Files if Explorer copied some, else text, else an image (PNG as provided by the app, or
/// converted), else None.
unsafe fn read(hwnd: HWND) -> Result<Option<Clip>> {
    let _open = Open::new(hwnd)?;
    if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() {
        let h = HDROP(GetClipboardData(CF_HDROP.0 as u32)?.0);
        let n = DragQueryFileW(h, u32::MAX, None);
        let mut paths = Vec::new();
        for i in 0..n {
            let len = DragQueryFileW(h, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(h, i, Some(&mut buf));
            paths.push(std::path::PathBuf::from(String::from_utf16_lossy(&buf[..len])));
        }
        return Ok((!paths.is_empty()).then_some(Clip::Files(paths)));
    }
    if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_ok() {
        let h = GetClipboardData(CF_UNICODETEXT.0 as u32)?;
        let Some(b) = global_bytes(h) else { return Ok(None) };
        let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
        let text = String::from_utf16_lossy(&units);
        if text.is_empty() || text.len() > MAX_TEXT {
            return Ok(None);
        }
        return Ok(Some(Clip::Text(text)));
    }
    let png = png_format();
    if IsClipboardFormatAvailable(png).is_ok() {
        if let Some(b) = GetClipboardData(png).ok().and_then(|h| global_bytes(h)) {
            if image::decode_png(&b).is_ok() {
                return Ok(Some(Clip::Png(b)));
            }
        }
    }
    for f in [CF_DIBV5, CF_DIB] {
        if IsClipboardFormatAvailable(f.0 as u32).is_ok() {
            if let Some(b) = GetClipboardData(f.0 as u32).ok().and_then(|h| global_bytes(h)) {
                let img = image::dib_to_rgba(&b)?;
                return Ok(Some(Clip::Png(image::encode_png(&img)?)));
            }
        }
    }
    Ok(None) // files, or a format we don't carry
}

unsafe fn put(format: u32, bytes: &[u8]) -> Result<()> {
    let g = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
    let p = GlobalLock(g) as *mut u8;
    if p.is_null() {
        let _ = GlobalFree(g);
        bail!("GlobalLock failed");
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    let _ = GlobalUnlock(g);
    if let Err(e) = SetClipboardData(format, HANDLE(g.0)) {
        let _ = GlobalFree(g); // still ours when SetClipboardData fails
        return Err(e.into());
    }
    Ok(())
}

unsafe fn write(hwnd: HWND, c: &Clip) -> Result<()> {
    // Decode before taking the clipboard, so it's held only briefly.
    let dib = match c {
        Clip::Png(b) => Some(image::rgba_to_dib(&image::decode_png(b)?)),
        _ => None,
    };
    let _open = Open::new(hwnd)?;
    EmptyClipboard()?;
    match c {
        Clip::Text(t) => {
            let mut b: Vec<u8> = t.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
            b.extend_from_slice(&[0, 0]);
            put(CF_UNICODETEXT.0 as u32, &b)?;
        }
        Clip::Png(b) => {
            put(png_format(), b)?; // keeps transparency for apps that read PNG
            put(CF_DIB.0 as u32, dib.as_deref().unwrap_or_default())?;
        }
        Clip::Files(paths) => {
            // DROPFILES header (file list at offset 20, wide chars), then NUL-separated paths
            // ending in an extra NUL: what Explorer pastes.
            let mut b = Vec::new();
            for v in [20u32, 0, 0, 0, 1] {
                b.extend_from_slice(&v.to_le_bytes());
            }
            for p in paths {
                b.extend(p.as_os_str().to_string_lossy().encode_utf16().chain([0]).flat_map(|u| u.to_le_bytes()));
            }
            b.extend_from_slice(&[0, 0]);
            put(CF_HDROP.0 as u32, &b)?;
            put(RegisterClipboardFormatW(w!("Preferred DropEffect")), &1u32.to_le_bytes())?; // copy, not move
        }
    }
    Ok(())
}
