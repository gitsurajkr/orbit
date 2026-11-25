//! Notification-area (tray) icon, so ORBIT can run in the background without a taskbar button.
//!
//! - Left click: bring the ORBIT window up. Right click: menu (Open ORBIT / Quit ORBIT).
//! - Hovering shows a status tooltip ([`Tray::set_tip`]).
//!
//! The icon lives on its own thread with a hidden window that receives the icon's messages.
//! "In the tray" means the main window is minimized *and* marked as a tool window, which keeps
//! it off the taskbar; minimized is the state ORBIT already used in the background, so the app
//! keeps running exactly as before (sharing, noticing a closed viewer, update checks).
//! Showing/hiding goes through this thread, never from inside the UI's own frame callback.

use std::sync::Mutex;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW};
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1; // icon clicked
const WM_TIP: u32 = WM_APP + 2; // tooltip text changed
const WM_SHOW_MAIN: u32 = WM_APP + 3;
const WM_HIDE_MAIN: u32 = WM_APP + 4;
const ID_OPEN: usize = 1;
const ID_QUIT: usize = 2;
const ID_LOGS: usize = 3;

static TIP: Mutex<String> = Mutex::new(String::new());
static TASKBAR_CREATED: Mutex<u32> = Mutex::new(0);

pub struct Tray {
    /// The tray thread's hidden window (an HWND, kept as isize so `Tray` is Send).
    hwnd: isize,
}

impl Tray {
    /// Adds the icon. None if Windows refused (ORBIT then falls back to the taskbar).
    pub fn start() -> Option<Tray> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new().name("tray".into()).spawn(move || unsafe { run(tx) }).ok()?;
        rx.recv().ok().flatten().map(|hwnd| Tray { hwnd })
    }

    fn post(&self, msg: u32) {
        unsafe {
            let _ = PostMessageW(HWND(self.hwnd as _), msg, WPARAM(0), LPARAM(0));
        }
    }

    /// Tooltip shown when hovering the icon (max 127 characters).
    pub fn set_tip(&self, tip: &str) {
        let mut t = TIP.lock().unwrap();
        if *t != tip {
            *t = tip.to_string();
            drop(t);
            self.post(WM_TIP);
        }
    }

    /// Brings the ORBIT window up from the tray.
    pub fn show_main(&self) {
        self.post(WM_SHOW_MAIN);
    }

    /// Sends the ORBIT window to the tray.
    pub fn hide_main(&self) {
        self.post(WM_HIDE_MAIN);
    }

    /// Removes the icon (on exit; otherwise it lingers until hovered).
    pub fn remove(&self) {
        self.post(WM_CLOSE);
    }
}

/// The launcher's window ("ORBIT"; the viewer's title is "ORBIT — ...", the tray's "ORBIT tray").
pub fn main_window() -> Option<HWND> {
    unsafe { FindWindowW(None, w!("ORBIT")).ok().filter(|h| !h.is_invalid()) }
}

/// Restores `hwnd` from the tray (or minimized), with a taskbar button, in front.
/// Also used by a second launcher process to bring the running one up.
pub fn show_window(hwnd: HWND) {
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if ex & WS_EX_TOOLWINDOW.0 as isize != 0 {
            let _ = ShowWindow(hwnd, SW_HIDE); // the taskbar only notices style changes on hidden windows
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !(WS_EX_TOOLWINDOW.0 as isize));
        }
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = SetForegroundWindow(hwnd);
    }
}

fn hide_window(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_TOOLWINDOW.0 as isize);
        let _ = ShowWindow(hwnd, SW_SHOWMINNOACTIVE);
    }
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() };
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    unsafe {
        let small = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        // Resource 1 is the ORBIT icon build.rs embeds (winresource's default id).
        nid.hIcon = match LoadImageW(hinst, PCWSTR(1 as *const u16), IMAGE_ICON, small.0, small.1, LR_DEFAULTCOLOR) {
            Ok(h) => HICON(h.0),
            Err(_) => LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        };
    }
    let tip = TIP.lock().unwrap();
    let tip = if tip.is_empty() { "ORBIT" } else { tip.as_str() };
    for (dst, src) in nid.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
        *dst = src;
    }
    nid
}

unsafe fn run(tx: std::sync::mpsc::Sender<Option<isize>>) {
    let hinst = GetModuleHandleW(None).unwrap_or_default();
    let class = w!("ORBIT_TRAY");
    let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: class, ..Default::default() };
    RegisterClassW(&wc);
    // A normal (never shown) top-level window: message-only windows miss "TaskbarCreated".
    let hwnd = match CreateWindowExW(WINDOW_EX_STYLE(0), class, w!("ORBIT tray"), WINDOW_STYLE(0), 0, 0, 0, 0, None, None, hinst, None) {
        Ok(h) => h,
        Err(_) => {
            let _ = tx.send(None);
            return;
        }
    };
    *TASKBAR_CREATED.lock().unwrap() = RegisterWindowMessageW(w!("TaskbarCreated"));
    if !Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd)).as_bool() {
        let _ = DestroyWindow(hwnd);
        let _ = tx.send(None);
        return;
    }
    let _ = tx.send(Some(hwnd.0 as isize));
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

unsafe fn menu(hwnd: HWND) {
    let Ok(m) = CreatePopupMenu() else { return };
    let _ = AppendMenuW(m, MF_STRING, ID_OPEN, w!("Open ORBIT"));
    let _ = AppendMenuW(m, MF_STRING, ID_LOGS, w!("Open logs folder"));
    let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(m, MF_STRING, ID_QUIT, w!("Quit ORBIT"));
    let mut p = POINT::default();
    let _ = GetCursorPos(&mut p);
    // Without this the menu doesn't close when clicking elsewhere (documented Windows quirk).
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(m, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RIGHTALIGN, p.x, p.y, 0, hwnd, None);
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(m);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            match (lp.0 as u32) & 0xFFFF {
                WM_LBUTTONUP => {
                    if let Some(h) = main_window() {
                        show_window(h);
                    }
                }
                WM_RBUTTONUP => menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_TIP => {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &icon_data(hwnd));
            LRESULT(0)
        }
        WM_SHOW_MAIN => {
            if let Some(h) = main_window() {
                show_window(h);
            }
            LRESULT(0)
        }
        WM_HIDE_MAIN => {
            if let Some(h) = main_window() {
                hide_window(h);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match wp.0 & 0xFFFF {
                ID_OPEN => {
                    if let Some(h) = main_window() {
                        show_window(h);
                    }
                }
                ID_LOGS => crate::logfile::open_folder(),
                ID_QUIT => {
                    // Same as closing the window for real: eframe exits, on_exit stops sharing.
                    if let Some(h) = main_window() {
                        let _ = PostMessageW(h, WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
            PostQuitMessage(0);
            LRESULT(0)
        }
        m if m != 0 && m == *TASKBAR_CREATED.lock().unwrap() => {
            // Explorer restarted: the icon is gone, add it again.
            let _ = Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd));
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
