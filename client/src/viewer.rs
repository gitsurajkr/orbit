//! The viewer window: connects, decodes, presents, forwards input.

use crate::{cursor, decode, keyhook, net, render};
use anyhow::{Context, Result};
use proto::clock::now_us;
use proto::ctrl::{Ctrl, MouseMove};
use proto::stats::{mbit_per_s, Dist, Ticker};
use proto::video::Frame;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::net::ToSocketAddrs;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use windows::core::{w, HSTRING};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateEventW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// What the window procedure needs to forward input. Lives on the window thread only.
struct InputSink {
    conn: quinn::Connection,
    ctrl: tokio::sync::mpsc::UnboundedSender<Ctrl>,
    video_w: u32,
    video_h: u32,
    win_w: u32,
    win_h: u32,
    move_seq: u32,
    keys: HashSet<(u16, bool)>,
    buttons: u8,
    resized: Option<(u32, u32)>,
    view_only: bool,
    fullscreen: Option<(WINDOW_STYLE, RECT)>,
}

thread_local! {
    static SINK: RefCell<Option<InputSink>> = const { RefCell::new(None) };
    /// The remote pointer's shape for our client area (null = hidden); `None` until the server
    /// says, and always in view-only mode (the local pointer doesn't stand for the remote one).
    static CURSOR: Cell<Option<HCURSOR>> = const { Cell::new(None) };
}

/// Connects to `server` (IP or name, optional `:port`) and runs the viewer window until it's
/// closed or the connection ends.
pub fn run_viewer(server: &str, view_only: bool, opts: &net::ConnectOptions) -> Result<()> {
    let server = server.trim();
    let addr_str = if server.contains(':') { server.to_string() } else { format!("{server}:{}", proto::DEFAULT_PORT) };
    let server = addr_str
        .to_socket_addrs()
        .with_context(|| format!("can't find '{addr_str}': check the Tailscale IP or name"))?
        .next()
        .with_context(|| format!("can't resolve {addr_str}"))?;
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // Another library may have set up COM on this thread already; either mode works for MF.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    }

    let wake = net::WakeEvent(unsafe { CreateEventW(None, false, false, None)? });
    let (frames_tx, frames_rx) = mpsc::channel::<Frame>();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    println!("connecting to {server} ...");
    let session = rt.block_on(net::connect(server, opts, frames_tx, wake))?;
    println!("connected: remote screen {}x{} @ {} fps cap", session.width, session.height, session.fps);

    let hwnd = create_window(session.width, session.height)?;
    let (cw, ch) = client_size(hwnd);
    let mut renderer = render::Renderer::new(hwnd, cw, ch)?;
    let mut decoder = decode::Decoder::new(&renderer.device)?;
    println!("display: tearing {} | Ctrl+Alt+Enter toggles fullscreen", if renderer.tearing() { "on (no vsync wait)" } else { "unavailable (vsync)" });
    SINK.with(|s| {
        *s.borrow_mut() = Some(InputSink {
            conn: session.conn.clone(),
            ctrl: session.ctrl.clone(),
            video_w: session.width,
            video_h: session.height,
            win_w: cw,
            win_h: ch,
            move_seq: 0,
            keys: HashSet::new(),
            buttons: 0,
            resized: None,
            view_only,
            fullscreen: None,
        })
    });

    // Win / Alt+Tab / Ctrl+Esc go to the remote PC while this window has focus.
    let _hook = if view_only {
        None
    } else {
        match keyhook::KeyHook::install(hwnd, forward_key) {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("note: can't forward Win / Alt+Tab ({e}); they stay on this PC");
                None
            }
        }
    };

    let mut cursors = cursor::Cursors::new();
    let mut decoded = Vec::new();
    let mut shown: Option<decode::Decoded> = None;
    let mut ticker = Ticker::new(Duration::from_secs(1));
    let mut presented = 0u64;
    let (mut latency, mut decode_ms, mut wire_ms) = (Dist::default(), Dist::default(), Dist::default());
    let mut prev = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let path = proto::tailscale::PathWatcher::start(server.ip());

    'main: loop {
        unsafe {
            MsgWaitForMultipleObjects(Some(&[wake.0]), false, 250, QS_ALLINPUT);
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    break 'main;
                }
                DispatchMessageW(&msg); // no TranslateMessage: we send scan codes, not characters
            }
        }
        if let Some(reason) = session.conn.close_reason() {
            println!("disconnected: {reason}");
            break;
        }
        while let Ok(m) = session.cursor.try_recv() {
            if let (false, Some(c)) = (view_only, cursors.handle(m)) {
                CURSOR.with(|cur| cur.set(Some(c)));
                cursor::apply_now(hwnd, c);
            }
        }
        if let Some((w, h)) = SINK.with(|s| s.borrow_mut().as_mut().and_then(|s| s.resized.take())) {
            renderer.resize(w, h)?;
        }

        // Decode everything that arrived (each delta frame is needed as a reference for the
        // next), but present only the newest.
        let mut newest: Option<(u64, Instant)> = None;
        while let Ok(f) = frames_rx.try_recv() {
            let t = Instant::now();
            decoded.clear();
            match decoder.decode(&f.data, f.capture_us, &mut decoded) {
                Ok(()) => {
                    decode_ms.add(t.elapsed().as_secs_f64() * 1e3);
                    wire_ms.add((f.complete_at - f.first_at).as_secs_f64() * 1e3);
                    if let Some(d) = decoded.pop() {
                        shown = Some(d);
                        newest = Some((f.capture_us, f.complete_at));
                    }
                }
                Err(e) => {
                    eprintln!("decode error: {e:#}; requesting a keyframe");
                    session.decoder_failed.store(true, Ordering::Release);
                }
            }
        }
        if let (Some((capture_us, _)), Some(d)) = (newest, shown.as_ref()) {
            renderer.present(d, session.width, session.height)?;
            presented += 1;
            if let Some(cap) = session.clock.lock().unwrap().to_client(capture_us) {
                latency.add((now_us() as i64 - cap) as f64 / 1e3);
            }
        }

        if let Some(secs) = ticker.tick(Instant::now()) {
            let st = &session.stats;
            let now = (
                st.bytes.load(Ordering::Relaxed),
                st.frames.load(Ordering::Relaxed),
                st.lost.load(Ordering::Relaxed),
                st.skipped.load(Ordering::Relaxed),
                st.keyframe_requests.load(Ordering::Relaxed),
                st.recovered.load(Ordering::Relaxed),
            );
            let rtt = session.clock.lock().unwrap().last_rtt_us().map_or(f64::NAN, |r| r as f64 / 1e3);
            let line = format!(
                "fps {:5.1} (recv {:5.1}) | {:6.2} Mbit/s | latency ms {}{} | decode ms {} | frame spread ms {} | rtt {:.1} | lost {} skip {} keyreq {} fec-fixed {}",
                presented as f64 / secs,
                (now.1 - prev.1) as f64 / secs,
                mbit_per_s(now.0 - prev.0, secs),
                latency.take(),
                path.suffix(" | "),
                decode_ms.take(),
                wire_ms.take(),
                rtt,
                now.2 - prev.2,
                now.3 - prev.3,
                now.4 - prev.4,
                now.5 - prev.5,
            );
            println!("{line}");
            unsafe {
                let _ = SetWindowTextW(hwnd, &HSTRING::from(format!("ORBIT — {}", &line[..line.find(" | decode").unwrap_or(line.len())])));
            }
            prev = now;
            presented = 0;
        }
    }
    session.conn.close(0u32.into(), b"bye");
    Ok(())
}

/// A key the keyboard hook took away from Windows: send it like any other key.
fn forward_key(scan: u16, extended: bool, down: bool) {
    SINK.with(|cell| {
        if let Ok(mut s) = cell.try_borrow_mut() {
            if let Some(s) = s.as_mut() {
                s.key(scan, extended, down);
            }
        }
    });
}

fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut r = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut r);
    }
    ((r.right - r.left).max(1) as u32, (r.bottom - r.top).max(1) as u32)
}

fn create_window(video_w: u32, video_h: u32) -> Result<HWND> {
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: w!("orbit"),
            ..Default::default()
        };
        RegisterClassW(&class);
        // Fit the remote screen into 90% of this monitor's work area, keeping its shape.
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY), &mut mi);
        let (aw, ah) = ((mi.rcWork.right - mi.rcWork.left) as u32 * 9 / 10, (mi.rcWork.bottom - mi.rcWork.top) as u32 * 9 / 10);
        let r = render::fit(video_w, video_h, aw.min(video_w), ah.min(video_h));
        let style = WS_OVERLAPPEDWINDOW | WS_VISIBLE;
        let mut wr = RECT { left: 0, top: 0, right: r.right - r.left, bottom: r.bottom - r.top };
        AdjustWindowRectEx(&mut wr, style, false, WINDOW_EX_STYLE::default())?;
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("orbit"),
            w!("ORBIT"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            wr.right - wr.left,
            wr.bottom - wr.top,
            None,
            None,
            instance,
            None,
        )?;
        // Black title bar to match the app's theme.
        let dark: BOOL = true.into();
        let _ = windows::Win32::Graphics::Dwm::DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as *const _,
            std::mem::size_of::<BOOL>() as u32,
        );
        Ok(hwnd)
    }
}

fn toggle_fullscreen(hwnd: HWND, s: &mut InputSink) {
    unsafe {
        if let Some((style, rect)) = s.fullscreen.take() {
            SetWindowLongPtrW(hwnd, GWL_STYLE, style.0 as isize);
            let _ = SetWindowPos(hwnd, None, rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top, SWP_FRAMECHANGED | SWP_NOZORDER);
        } else {
            let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
            let mut rect = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rect);
            s.fullscreen = Some((style, rect));
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi);
            let m = mi.rcMonitor;
            SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
            let _ = SetWindowPos(hwnd, HWND_TOP, m.left, m.top, m.right - m.left, m.bottom - m.top, SWP_FRAMECHANGED);
        }
    }
}

fn lparam_xy(l: LPARAM) -> (i32, i32) {
    ((l.0 & 0xFFFF) as i16 as i32, ((l.0 >> 16) & 0xFFFF) as i16 as i32)
}

impl InputSink {
    fn mouse_move(&mut self, x: i32, y: i32) {
        let r = render::fit(self.video_w, self.video_h, self.win_w, self.win_h);
        let (w, h) = ((r.right - r.left).max(2) as i64, (r.bottom - r.top).max(2) as i64);
        let nx = ((x - r.left) as i64 * 65535 / (w - 1)).clamp(0, 65535) as u16;
        let ny = ((y - r.top) as i64 * 65535 / (h - 1)).clamp(0, 65535) as u16;
        self.move_seq = self.move_seq.wrapping_add(1);
        let m = MouseMove { seq: self.move_seq, x: nx, y: ny };
        let _ = self.conn.send_datagram(m.encode().to_vec().into());
    }

    fn button(&mut self, hwnd: HWND, b: u8, down: bool) {
        if down {
            if self.buttons == 0 {
                unsafe { SetCapture(hwnd) };
            }
            self.buttons |= 1 << b;
        } else {
            self.buttons &= !(1 << b);
            if self.buttons == 0 {
                unsafe {
                    let _ = ReleaseCapture();
                }
            }
        }
        let _ = self.ctrl.send(Ctrl::MouseButton { button: b, down });
    }

    fn key(&mut self, scan: u16, extended: bool, down: bool) {
        if down {
            self.keys.insert((scan, extended));
        } else {
            self.keys.remove(&(scan, extended));
        }
        let _ = self.ctrl.send(Ctrl::Key { scan, extended, down });
    }

    /// Focus lost: release everything, or the remote PC would see keys stuck down.
    fn release_all(&mut self) {
        for (scan, extended) in self.keys.drain() {
            let _ = self.ctrl.send(Ctrl::Key { scan, extended, down: false });
        }
        for b in 0..5 {
            if self.buttons & (1 << b) != 0 {
                let _ = self.ctrl.send(Ctrl::MouseButton { button: b, down: false });
            }
        }
        self.buttons = 0;
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let handled = SINK.with(|cell| {
        let Ok(mut guard) = cell.try_borrow_mut() else { return false };
        let Some(s) = guard.as_mut() else { return false };
        let is_input = matches!(msg, WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP | WM_MOUSEWHEEL | WM_MOUSEHWHEEL | WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP);
        if s.view_only && is_input {
            return false;
        }
        match msg {
            WM_MOUSEMOVE => {
                let (x, y) = lparam_xy(lp);
                s.mouse_move(x, y);
            }
            WM_LBUTTONDOWN | WM_LBUTTONUP => s.button(hwnd, 0, msg == WM_LBUTTONDOWN),
            WM_RBUTTONDOWN | WM_RBUTTONUP => s.button(hwnd, 1, msg == WM_RBUTTONDOWN),
            WM_MBUTTONDOWN | WM_MBUTTONUP => s.button(hwnd, 2, msg == WM_MBUTTONDOWN),
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                let which = ((wp.0 >> 16) & 0xFFFF) as u16;
                s.button(hwnd, if which == 1 { 3 } else { 4 }, msg == WM_XBUTTONDOWN);
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let delta = ((wp.0 >> 16) & 0xFFFF) as u16 as i16;
                let _ = s.ctrl.send(Ctrl::Wheel { delta, horizontal: msg == WM_MOUSEHWHEEL });
            }
            WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
                let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
                let vk = wp.0 as u16;
                let ctrl_alt = unsafe { GetKeyState(VK_CONTROL.0 as i32) < 0 && GetKeyState(VK_MENU.0 as i32) < 0 };
                if down && vk == VK_RETURN.0 && ctrl_alt {
                    toggle_fullscreen(hwnd, s); // local shortcut, not forwarded
                } else {
                    let scan = ((lp.0 >> 16) & 0xFF) as u16;
                    let extended = (lp.0 >> 24) & 1 != 0;
                    if scan != 0 {
                        s.key(scan, extended, down);
                    }
                }
            }
            WM_KILLFOCUS => {
                keyhook::KeyHook::focus_lost();
                s.release_all();
                return false;
            }
            WM_SIZE => {
                let (w, h) = ((lp.0 & 0xFFFF) as u32, ((lp.0 >> 16) & 0xFFFF) as u32);
                if w > 0 && h > 0 {
                    s.win_w = w;
                    s.win_h = h;
                    s.resized = Some((w, h));
                }
                return false;
            }
            _ => return false,
        }
        true
    });
    if handled {
        return LRESULT(0);
    }
    unsafe {
        match msg {
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_SETCURSOR if (lp.0 & 0xFFFF) as u32 == HTCLIENT => match CURSOR.with(|c| c.get()) {
                Some(c) => {
                    SetCursor(c);
                    LRESULT(1)
                }
                None => DefWindowProcW(hwnd, msg, wp, lp),
            },
            // Keep Alt / F10 from opening the (nonexistent) window menu.
            WM_SYSCOMMAND if (wp.0 & 0xFFF0) == SC_KEYMENU as usize => LRESULT(0),
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

