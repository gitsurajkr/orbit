//! Injects the client's mouse and keyboard with SendInput.
//!
//! Keys arrive as hardware scan codes, so the server's keyboard layout decides what character a
//! key makes, just like a physical keyboard plugged into the server.
//! Note: Windows blocks injected input into elevated (admin) windows unless ORBIT itself
//! runs as administrator (or as the ORBIT Agent, as SYSTEM).
//!
//! All injection happens on one dedicated thread ([`Input`] just queues to it): `SendInput`
//! only reaches the desktop the calling thread is attached to, and that thread follows the
//! input desktop (lock screen, UAC) when ORBIT runs as SYSTEM (see desktop.rs).

use crate::capture::MonitorInfo;
use proto::ctrl::{Ctrl, MouseMove};
use std::collections::HashSet;
use std::sync::Mutex;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN};

struct Injector {
    /// The screen being shown (mouse positions are relative to it); changes on a switch.
    mon: Mutex<MonitorInfo>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    last_move_seq: Option<u32>,
    /// Keys and buttons currently held down by the client, released when the session ends so
    /// nothing stays stuck on the server.
    keys: HashSet<(u16, bool)>,
    buttons: HashSet<u8>,
}

fn send(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as _, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn key(scan: u16, extended: bool, down: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn button(b: u8, down: bool) -> Option<INPUT> {
    let (flags, data) = match (b, down) {
        (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (0, false) => (MOUSEEVENTF_LEFTUP, 0),
        (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (3 | 4, true) => (MOUSEEVENTF_XDOWN, if b == 3 { 1 } else { 2 }),
        (3 | 4, false) => (MOUSEEVENTF_XUP, if b == 3 { 1 } else { 2 }),
        _ => return None,
    };
    Some(mouse(0, 0, data, flags))
}

impl Injector {
    fn new(mon: MonitorInfo) -> Injector {
        Injector { mon: Mutex::new(mon), state: Mutex::new(State::default()) }
    }

    pub fn set_monitor(&self, mon: MonitorInfo) {
        *self.mon.lock().unwrap() = mon;
    }

    /// Absolute move. Ignores moves older than the newest applied (datagrams can reorder).
    pub fn mouse_move(&self, m: MouseMove) {
        {
            let mut s = self.state.lock().unwrap();
            if let Some(last) = s.last_move_seq {
                if (m.seq.wrapping_sub(last) as i32) <= 0 {
                    return;
                }
            }
            s.last_move_seq = Some(m.seq);
        }
        // monitor-relative 0..65535 -> desktop pixel -> virtual-desktop normalized 0..65535
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
            )
        };
        let mon = self.mon.lock().unwrap().clone();
        let px = mon.left as i64 + (m.x as i64 * (mon.width as i64 - 1)) / 65535;
        let py = mon.top as i64 + (m.y as i64 * (mon.height as i64 - 1)) / 65535;
        let nx = ((px - vx as i64) * 65535 / (vw as i64 - 1)) as i32;
        let ny = ((py - vy as i64) * 65535 / (vh as i64 - 1)) as i32;
        send(&[mouse(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)]);
    }

    pub fn handle(&self, c: Ctrl) {
        let mut s = self.state.lock().unwrap();
        match c {
            Ctrl::Key { scan, extended, down } => {
                if down {
                    s.keys.insert((scan, extended));
                } else {
                    s.keys.remove(&(scan, extended));
                }
                send(&[key(scan, extended, down)]);
            }
            Ctrl::MouseButton { button: b, down } => {
                if let Some(i) = button(b, down) {
                    if down {
                        s.buttons.insert(b);
                    } else {
                        s.buttons.remove(&b);
                    }
                    send(&[i]);
                }
            }
            Ctrl::Wheel { delta, horizontal } => {
                send(&[mouse(0, 0, delta as i32, if horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL })]);
            }
            _ => {}
        }
    }

    pub fn release_all(&self) {
        let mut s = self.state.lock().unwrap();
        let mut ups: Vec<INPUT> = s.keys.drain().map(|(sc, ext)| key(sc, ext, false)).collect();
        ups.extend(s.buttons.drain().filter_map(|b| button(b, false)));
        if !ups.is_empty() {
            send(&ups);
        }
    }
}

enum Cmd {
    Move(MouseMove),
    Ctrl(Ctrl),
    SetMonitor(MonitorInfo),
    ReleaseAll,
}

/// Queues input for the injection thread; the thread ends when this is dropped.
pub struct Input {
    tx: std::sync::mpsc::Sender<Cmd>,
}

impl Input {
    pub fn new(mon: MonitorInfo) -> Input {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let _ = std::thread::Builder::new().name("input".into()).spawn(move || {
            let inj = Injector::new(mon);
            let mut checked = std::time::Instant::now() - std::time::Duration::from_secs(1);
            while let Ok(cmd) = rx.recv() {
                // Stay on the desktop that receives input (cheap; at most every 50 ms).
                if checked.elapsed() >= std::time::Duration::from_millis(50) {
                    crate::desktop::follow_input_desktop();
                    checked = std::time::Instant::now();
                }
                match cmd {
                    Cmd::Move(m) => inj.mouse_move(m),
                    Cmd::Ctrl(c) => inj.handle(c),
                    Cmd::SetMonitor(m) => inj.set_monitor(m),
                    Cmd::ReleaseAll => inj.release_all(),
                }
            }
        });
        Input { tx }
    }

    pub fn set_monitor(&self, mon: MonitorInfo) {
        let _ = self.tx.send(Cmd::SetMonitor(mon));
    }

    /// Absolute move. Moves older than the newest applied are ignored (datagrams can reorder).
    pub fn mouse_move(&self, m: MouseMove) {
        let _ = self.tx.send(Cmd::Move(m));
    }

    pub fn handle(&self, c: Ctrl) {
        let _ = self.tx.send(Cmd::Ctrl(c));
    }

    pub fn release_all(&self) {
        let _ = self.tx.send(Cmd::ReleaseAll);
    }
}
