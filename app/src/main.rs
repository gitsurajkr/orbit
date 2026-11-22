//! ORBIT: one exe for both PCs.
//!
//! - `orbit.exe`                          → the launcher (CONNECT / SHARE)
//! - `orbit.exe --connect ADDR [--view-only]` → the viewer window (started by the launcher as a
//!   separate process, so the launcher's UI toolkit and the viewer's D3D/Media Foundation setup
//!   never share a thread)

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
        if let Err(e) = client::run_viewer(addr, view_only) {
            eprintln!("{e:#}");
            std::process::exit(1);
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
