//! Follows Windows' *input desktop*: the normal desktop, or the secure one Windows switches to
//! for the lock / sign-in screen, Ctrl+Alt+Del and UAC prompts.
//!
//! Capture and input injection only work on the desktop the calling thread is attached to, and
//! only a process running as SYSTEM (the ORBIT Agent, started by the ORBIT Service) may attach
//! to the secure desktop. So the capture thread and the input thread call
//! [`follow_input_desktop`] before they work: when the input desktop changed, the thread moves
//! to it. In a normal (non-SYSTEM) process opening the secure desktop is refused, nothing
//! changes, and capture waits as before.

use std::cell::Cell;
use windows::Win32::Foundation::GENERIC_ALL;
use windows::Win32::System::StationsAndDesktops::*;

thread_local! {
    /// The desktop handle this thread switched to last (kept open while attached).
    static ATTACHED: Cell<Option<HDESK>> = const { Cell::new(None) };
}

fn name(h: HDESK) -> String {
    let mut buf = [0u16; 128];
    let mut needed = 0u32;
    let ok = unsafe { GetUserObjectInformationW(windows::Win32::Foundation::HANDLE(h.0), UOI_NAME, Some(buf.as_mut_ptr() as *mut _), std::mem::size_of_val(&buf) as u32, Some(&mut needed)) };
    if ok.is_err() {
        return String::new();
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// Name of the desktop receiving input right now ("Default", "Winlogon", ...), if this process
/// may look at it.
pub fn input_desktop_name() -> Option<String> {
    unsafe {
        let h = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0)).ok()?;
        let n = name(h);
        let _ = CloseDesktop(h);
        Some(n)
    }
}

/// Name of the desktop this thread is attached to.
pub fn thread_desktop_name() -> String {
    unsafe { GetThreadDesktop(windows::Win32::System::Threading::GetCurrentThreadId()).map(name).unwrap_or_default() }
}

/// Attaches this thread to the input desktop if it isn't already. Returns true if it moved.
/// The thread must not own windows or hooks (Windows refuses to move those).
pub fn follow_input_desktop() -> bool {
    unsafe {
        let Ok(h) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0)) else {
            return false; // not allowed (not SYSTEM) or no input desktop right now
        };
        if name(h) == thread_desktop_name() {
            let _ = CloseDesktop(h);
            return false;
        }
        if SetThreadDesktop(h).is_err() {
            let _ = CloseDesktop(h);
            return false;
        }
        if let Some(old) = ATTACHED.with(|a| a.replace(Some(h))) {
            let _ = CloseDesktop(old);
        }
        true
    }
}

/// Presses Ctrl+Alt+Del (`SendSAS` from sas.dll). Windows only allows it for services and
/// SYSTEM processes, and only with the SoftwareSASGeneration policy (set when the ORBIT Service
/// is installed). Returns false where that isn't the case.
pub fn send_ctrl_alt_del() -> bool {
    use windows::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};
    if !running_as_system() {
        return false;
    }
    // Windows accepts SendSAS only from the service process itself, not from the agent it
    // started: ask the service (it waits on this event).
    unsafe {
        if let Ok(ev) = OpenEventW(EVENT_MODIFY_STATE, false, &windows::core::HSTRING::from(SAS_EVENT)) {
            let ok = SetEvent(ev).is_ok();
            let _ = windows::Win32::Foundation::CloseHandle(ev);
            if ok {
                return true;
            }
        }
    }
    sas_now()
}

/// Name of the event the ORBIT Service waits on to press Ctrl+Alt+Del.
pub const SAS_EVENT: &str = r"Global\ORBIT-SAS";

/// Calls `SendSAS` in this process (works in the service process).
pub fn sas_now() -> bool {
    use windows::core::{s, w};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    unsafe {
        let Ok(lib) = LoadLibraryW(w!("sas.dll")) else { return false };
        let Some(f) = GetProcAddress(lib, s!("SendSAS")) else { return false };
        let send: extern "system" fn(windows::Win32::Foundation::BOOL) = std::mem::transmute(f);
        send(false.into()); // false: as a service, not an Ease of Access app
    }
    true
}

/// True if this process runs as the SYSTEM account (the ORBIT Agent).
pub fn running_as_system() -> bool {
    use windows::Win32::Security::*;
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut buf = [0u8; 256];
        let mut len = 0u32;
        let ok = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr() as *mut _), buf.len() as u32, &mut len).is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(token);
        ok && IsWellKnownSid((*(buf.as_ptr() as *const TOKEN_USER)).User.Sid, WinLocalSystemSid).as_bool()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_normal_process_stays_on_its_desktop() {
        let before = thread_desktop_name();
        assert!(!before.is_empty());
        // Unlocked, the input desktop is the one we're on, so nothing moves; locked, a normal
        // process isn't allowed to move. Either way: no switch.
        assert!(!running_as_system());
        assert!(!follow_input_desktop());
        assert_eq!(thread_desktop_name(), before);
    }
}
