//! ORBIT server: screen capture → NVENC → QUIC, plus input injection.
//! Used by the `orbit-server` CLI and by the ORBIT app's SHARE mode.

pub mod capture;
pub mod cursor;
pub mod desktop;
pub mod input;
pub mod net;
pub mod nvenc;
pub mod rate;
pub mod session;
pub mod video;

pub use session::{ServerHandle, Settings, Status, StatusInfo};

/// Checks this PC can share its screen before starting: an NVIDIA GPU (for NVENC) and a monitor.
pub fn preflight(monitor: usize) -> anyhow::Result<capture::MonitorInfo> {
    use anyhow::Context;
    capture::find_adapter(capture::NVIDIA_VENDOR)?.context("no NVIDIA GPU found: this PC can connect to others, but sharing needs NVENC")?;
    let mons = capture::list_monitors()?;
    mons.get(monitor).cloned().with_context(|| format!("monitor {monitor} not found ({} available)", mons.len()))
}

/// Makes this process per-monitor DPI aware (Desktop Duplication needs it on scaled displays).
pub fn dpi_aware() {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}
