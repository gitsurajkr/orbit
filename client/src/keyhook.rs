//! Sends Win, Alt+Tab, Alt+Esc and Ctrl+Esc to the remote PC instead of this one.
//!
//! Windows handles these shortcuts in the shell, before any window sees them, so the viewer's
//! window procedure can't forward them. A low-level keyboard hook (`WH_KEYBOARD_LL`) runs before
//! the shell: while the viewer window is in the foreground, the hook forwards those keys and
//! swallows them here. Every other key is left alone and reaches the window procedure as usual.
//!
//! - A key whose press was swallowed has its release swallowed (and forwarded) too, even if focus
//!   moved meanwhile, so neither PC ever sees half a key press.
//! - Ctrl+Alt+Del can't be hooked: Windows reserves it.
//! - The hook runs on the viewer thread (it's called from that thread's message loop); it must
//!   return quickly, so it only queues a message.
//! - Keys injected by other software (e.g. AutoHotkey) pass through untouched.

use std::cell::RefCell;
use std::collections::HashSet;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_RWIN, VK_TAB};
use windows::Win32::UI::WindowsAndMessaging::*;

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

struct State {
    hwnd: HWND,
    /// Called for every swallowed key: (scan, extended, down).
    forward: fn(u16, bool, bool),
    /// Keys whose press we swallowed, as (scan, extended).
    swallowed: HashSet<(u16, bool)>,
}

/// Removes the hook when dropped.
pub struct KeyHook(HHOOK);

impl KeyHook {
    /// Installs the hook for `hwnd`'s thread (call on the viewer thread).
    pub fn install(hwnd: HWND, forward: fn(u16, bool, bool)) -> windows::core::Result<KeyHook> {
        STATE.with(|s| *s.borrow_mut() = Some(State { hwnd, forward, swallowed: HashSet::new() }));
        let module = unsafe { GetModuleHandleW(None)? };
        let h = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), module, 0)? };
        Ok(KeyHook(h))
    }

    /// Focus left the viewer: forget swallowed presses (the viewer releases held keys itself).
    pub fn focus_lost() {
        STATE.with(|s| {
            if let Ok(mut s) = s.try_borrow_mut() {
                if let Some(s) = s.as_mut() {
                    s.swallowed.clear();
                }
            }
        });
    }
}

impl Drop for KeyHook {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.0);
        }
        STATE.with(|s| *s.borrow_mut() = None);
    }
}

/// True if Windows would act on this key press itself instead of giving it to the window.
fn is_system_shortcut(vk: u16, alt_down: bool) -> bool {
    let ctrl_down = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0;
    vk == VK_LWIN.0 || vk == VK_RWIN.0 || (vk == VK_TAB.0 && alt_down) || (vk == VK_ESCAPE.0 && (alt_down || ctrl_down))
}

extern "system" fn hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let k = unsafe { &*(lp.0 as *const KBDLLHOOKSTRUCT) };
        let swallow = STATE.with(|s| {
            let Ok(mut s) = s.try_borrow_mut() else { return false };
            let Some(s) = s.as_mut() else { return false };
            if k.flags.0 & LLKHF_INJECTED.0 != 0 {
                return false;
            }
            let up = k.flags.0 & LLKHF_UP.0 != 0;
            let key = ((k.scanCode & 0xFF) as u16, k.flags.0 & LLKHF_EXTENDED.0 != 0);
            if up {
                if !s.swallowed.remove(&key) {
                    return false;
                }
            } else {
                let held = s.swallowed.contains(&key); // auto-repeat of a swallowed key
                let foreground = unsafe { GetForegroundWindow() } == s.hwnd;
                if !held && !(foreground && is_system_shortcut(k.vkCode as u16, k.flags.0 & LLKHF_ALTDOWN.0 != 0)) {
                    return false;
                }
                s.swallowed.insert(key);
            }
            (s.forward)(key.0, key.1, !up);
            true
        });
        if swallow {
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}
