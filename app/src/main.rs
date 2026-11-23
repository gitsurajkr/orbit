//! ORBIT: one exe for both PCs.
//!
//! - `orbit.exe`                          → the launcher (CONNECT / SHARE)
//! - `orbit.exe --connect ADDR [--view-only] [--code 123456] [--accept-new-identity]` → the
//!   viewer window (started by the launcher as a separate process, so the launcher's UI toolkit
//!   and the viewer's D3D/Media Foundation setup never share a thread). Its exit code tells the
//!   launcher why it couldn't connect (see [`viewer_exit_code`]).
//! - `orbit.exe --service` / `--agent` → always-on sharing incl. the lock screen (service.rs);
//!   `--install-service` / `--uninstall-service` (as administrator) turn it on and off.

#![windows_subsystem = "windows"]

mod always_on;
mod autostart;
mod config;
mod icon;
mod pipe;
mod service;
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
    let flag = |f: &str| args.iter().any(|a| a == f);
    if flag("--service") {
        let _ = service::run();
        return;
    }
    if flag("--agent") {
        if let Err(e) = service::run_agent() {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
        return;
    }
    if flag("--service-status") {
        println!("service: {:?}", service::status_detail().map_err(|e| format!("{e:#}")));
        match pipe::call(&serde_json::json!({"cmd": "status"})) {
            Ok(v) => println!("agent: {v}"),
            Err(e) => println!("agent: {e:#}"),
        }
        return;
    }
    if flag("--install-service") || flag("--uninstall-service") {
        let num = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<f64>().ok());
        // Without --fps etc. (the installer), the agent keeps its settings across updates.
        let settings = num("--fps").map(|fps| service::AgentSettings {
            fps: fps as u32,
            mbps: num("--mbps").map_or(20.0, |v| v as f32),
            monitor: num("--monitor").map_or(0, |v| v as usize),
            sharing: false,
            share_at_boot: false,
        });
        let r = if flag("--install-service") { service::install(settings.as_ref()) } else { service::uninstall() };
        if let Err(e) = r {
            eprintln!("{e:#}");
            std::process::exit(1);
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
            clipboard: !view_only && !args.iter().any(|a| a == "--no-clipboard"),
        };
        if let Err(e) = client::run_viewer(addr, view_only, &opts) {
            eprintln!("{e:#}");
            std::process::exit(viewer_exit_code(&e));
        }
        return;
    }
    server::dpi_aware();
    // ORBIT_SECOND_INSTANCE=1: a test copy next to the installed one (development only).
    let _instance = if std::env::var_os("ORBIT_SECOND_INSTANCE").is_some() {
        None
    } else {
        match autostart::single_instance() {
            Some(i) => Some(i),
            None => return, // already running: brought the existing window forward
        }
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
