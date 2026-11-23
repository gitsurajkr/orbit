//! ORBIT: one exe for both PCs.
//!
//! - `orbit.exe`                          → the launcher (CONNECT / SHARE)
//! - `orbit.exe --connect ADDR [--view-only] [--code 123456] [--accept-new-identity]` → the
//!   viewer window (started by the launcher as a separate process, so the launcher's UI toolkit
//!   and the viewer's D3D/Media Foundation setup never share a thread). Its exit code tells the
//!   launcher why it couldn't connect (see [`viewer_exit_code`]).

#![windows_subsystem = "windows"]

mod autostart;
mod config;
mod icon;
mod tray;
mod ui;
mod update;
mod widgets;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--check-update") {
        match update::check() {
            Ok(Some(r)) => println!("update available: v{} (this is v{})", r.version, update::CURRENT),
            Ok(None) => println!("up to date (v{})", update::CURRENT),
            Err(e) => println!("check failed: {e:#}"),
        }
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--connect") {
        let Some(addr) = args.get(i + 1) else {
            eprintln!("--connect needs an address");
            std::process::exit(2);
        };
        let view_only = args.iter().any(|a| a == "--view-only");
        let opts = client::ConnectOptions {
            code: args.iter().position(|a| a == "--code").and_then(|i| args.get(i + 1)).cloned(),
            accept_new_identity: args.iter().any(|a| a == "--accept-new-identity"),
        };
        if let Err(e) = client::run_viewer(addr, view_only, &opts) {
            eprintln!("{e:#}");
            std::process::exit(viewer_exit_code(&e));
        }
        return;
    }
    server::dpi_aware();
    let Some(_instance) = autostart::single_instance() else {
        return; // already running: brought the existing window forward
    };
    let launched_at_login = args.iter().any(|a| a == "--autostart");
    if let Err(e) = ui::run(launched_at_login) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

/// Viewer exit codes the launcher understands (anything else non-zero is a plain error).
pub mod exit {
    /// Not paired; a code is showing on the sharing PC.
    pub const NEED_CODE: i32 = 10;
    /// Not paired; no code is showing there yet.
    pub const NOT_PAIRED: i32 = 11;
    /// Refused (e.g. wrong code).
    pub const DENIED: i32 = 12;
    /// The sharing PC's ID changed since last time.
    pub const IDENTITY_CHANGED: i32 = 13;
}

fn viewer_exit_code(e: &anyhow::Error) -> i32 {
    match e.downcast_ref::<client::AuthError>() {
        Some(client::AuthError::NeedCode { code_active: true }) => exit::NEED_CODE,
        Some(client::AuthError::NeedCode { code_active: false }) => exit::NOT_PAIRED,
        Some(client::AuthError::Denied(_)) => exit::DENIED,
        Some(client::AuthError::IdentityChanged { .. }) => exit::IDENTITY_CHANGED,
        None => 1,
    }
}
