//! Start with Windows, and keep a single launcher running.
//!
//! Start with Windows: a value in HKCU\Software\Microsoft\Windows\CurrentVersion\Run that runs
//! `orbit.exe --autostart` at login (no admin rights needed). ORBIT then opens minimized and starts
//! sharing by itself. Note: that needs a logged-in desktop; set Windows to sign in automatically
//! for unattended restarts.
//!
//! Single instance: a named mutex. A second launcher just brings the first one's window forward.

use anyhow::{bail, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE};
use windows::Win32::System::Registry::{RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::FindWindowW;

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE: PCWSTR = w!("ORBIT");

/// Turns "start with Windows" on (pointing at this exe) or off.
pub fn set(enabled: bool) -> Result<()> {
    unsafe {
        if enabled {
            let exe = std::env::current_exe()?;
            let cmd = format!("\"{}\" --autostart", exe.display());
            let wide: Vec<u16> = cmd.encode_utf16().chain(Some(0)).collect();
            let r = RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE, REG_SZ.0, Some(wide.as_ptr() as *const _), (wide.len() * 2) as u32);
            if r != ERROR_SUCCESS {
                bail!("couldn't add ORBIT to Windows startup (error {})", r.0);
            }
        } else {
            let r = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE);
            if r != ERROR_SUCCESS && r != ERROR_FILE_NOT_FOUND {
                bail!("couldn't remove ORBIT from Windows startup (error {})", r.0);
            }
        }
    }
    Ok(())
}

pub fn is_enabled() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE, RRF_RT_REG_SZ, None, None, None) == ERROR_SUCCESS }
}

/// Holds the single-instance mutex for the life of the process.
pub struct SingleInstance(#[allow(dead_code)] HANDLE);

/// Returns None if another ORBIT launcher is already running (after bringing it forward).
pub fn single_instance() -> Option<SingleInstance> {
    unsafe {
        let h = CreateMutexW(None, true, w!("Local\\ORBIT-launcher")).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(hwnd) = FindWindowW(None, w!("ORBIT")) {
                crate::tray::show_window(hwnd); // also brings it out of the tray
            }
            return None;
        }
        Some(SingleInstance(h))
    }
}

#[cfg(test)]
mod tests {
    /// Touches the real HKCU Run key, so it's opt-in: cargo test -p app -- --ignored
    #[test]
    #[ignore]
    fn startup_entry_round_trip() {
        let was = super::is_enabled();
        super::set(true).unwrap();
        assert!(super::is_enabled());
        super::set(false).unwrap();
        assert!(!super::is_enabled());
        super::set(false).unwrap(); // removing twice is fine
        if was {
            super::set(true).unwrap();
        }
    }
}
