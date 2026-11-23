//! The app's side of "always on" sharing (the ORBIT Service, see service.rs): is it installed,
//! turning it on and off (with Windows' admin prompt), and what it's doing (asked over the
//! pipe, see pipe.rs, once a second on a background thread so the UI never waits).

use crate::service::{self, AgentSettings};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows_service::service::ServiceState;

type Pending = Arc<Mutex<Option<Result<(), String>>>>;

pub struct AlwaysOn {
    /// The service is installed and running.
    running: Arc<AtomicBool>,
    /// Latest status from the agent (None until it answers).
    snapshot: Arc<Mutex<Option<Value>>>,
    /// An install / uninstall waiting for the admin prompt and its result.
    pending: Option<(bool, Pending)>,
    pub error: Option<String>,
    /// Turning on failed (e.g. the admin prompt was declined): the caller falls back.
    declined: bool,
    turned_off: bool,
}

/// One paired device as the agent reports it.
pub struct DevicesView {
    pub devices: Vec<auth::Device>,
    pub trust_own: bool,
    pub pin: Option<(String, Duration)>,
    pub id: String,
}

impl AlwaysOn {
    pub fn start(ctx: &eframe::egui::Context) -> AlwaysOn {
        let running = Arc::new(AtomicBool::new(service::status() == Some(ServiceState::Running)));
        let snapshot: Arc<Mutex<Option<Value>>> = Arc::default();
        let (r, s, ctx) = (running.clone(), snapshot.clone(), ctx.clone());
        std::thread::Builder::new()
            .name("always-on".into())
            .spawn(move || loop {
                let on = service::status() == Some(ServiceState::Running);
                r.store(on, Ordering::Release);
                let snap = if on { crate::pipe::call(&json!({"cmd": "status"})).ok() } else { None };
                *s.lock().unwrap() = snap;
                ctx.request_repaint();
                std::thread::sleep(Duration::from_secs(1));
            })
            .ok();
        AlwaysOn { running, snapshot, pending: None, error: None, declined: false, turned_off: false }
    }

    pub fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Waiting for the admin prompt: Some(true) turning on, Some(false) turning off.
    pub fn busy(&self) -> Option<bool> {
        self.pending.as_ref().map(|(on, _)| *on)
    }

    pub fn status(&self) -> Option<Value> {
        self.snapshot.lock().unwrap().clone()
    }

    /// Call every frame: picks up the result of a finished install / uninstall.
    pub fn poll(&mut self) {
        let done = self.pending.as_ref().and_then(|(on, p)| p.lock().unwrap().take().map(|r| (*on, r)));
        if let Some((on, r)) = done {
            self.pending = None;
            match r {
                // Show the new state at once; the status thread confirms it within a second.
                // (Waiting for it left the box looking unticked for a moment, inviting a click.)
                Ok(()) => {
                    self.running.store(on, Ordering::Release);
                    self.turned_off = !on;
                }
                Err(e) => {
                    self.declined = on;
                    self.error = Some(e);
                }
            }
        }
    }

    /// True once after the service was turned off (its devices were copied back: reload).
    pub fn take_turned_off(&mut self) -> bool {
        std::mem::take(&mut self.turned_off)
    }

    /// True once after turning on failed.
    pub fn take_declined(&mut self) -> bool {
        std::mem::take(&mut self.declined)
    }

    /// Installs (with `settings`) or removes the service; Windows asks for admin permission.
    pub fn set(&mut self, on: bool, settings: &AgentSettings) {
        if self.pending.is_some() {
            return;
        }
        self.error = None;
        let args: Vec<String> = if on {
            vec!["--install-service".into(), "--fps".into(), settings.fps.to_string(), "--mbps".into(), settings.mbps.to_string(), "--monitor".into(), settings.monitor.to_string()]
        } else {
            vec!["--uninstall-service".into()]
        };
        let slot: Pending = Arc::default();
        let s = slot.clone();
        std::thread::spawn(move || {
            let r = run_elevated(&args).and_then(|code| if code == 0 { Ok(()) } else { Err(format!("the service setup failed (code {code})")) });
            *s.lock().unwrap() = Some(r);
        });
        self.pending = Some((on, slot));
    }

    /// The agent's paired devices, code and ID (None until it answers).
    pub fn devices(&self) -> Option<DevicesView> {
        let v = self.status()?;
        let devices = v["devices"]
            .as_array()?
            .iter()
            .filter_map(|d| {
                Some(auth::Device {
                    name: d["name"].as_str()?.to_string(),
                    fp: auth::unhex(d["fp"].as_str()?)?,
                    paired_unix: d["paired"].as_u64().unwrap_or(0),
                    how: d["how"].as_str().unwrap_or("").to_string(),
                })
            })
            .collect();
        let pin = v["pin"].as_object().and_then(|p| Some((p.get("code")?.as_str()?.to_string(), Duration::from_secs(p.get("left")?.as_u64()?))));
        Some(DevicesView { devices, trust_own: v["trust_own"].as_bool().unwrap_or(true), pin, id: v["id"].as_str().unwrap_or("").to_string() })
    }

    /// Sends a change to the agent, then refreshes the snapshot so the UI shows it at once.
    pub fn command(&mut self, req: Value) {
        if let Err(e) = crate::pipe::call(&req) {
            self.error = Some(format!("{e:#}"));
        }
        if let Ok(v) = crate::pipe::call(&json!({"cmd": "status"})) {
            *self.snapshot.lock().unwrap() = Some(v);
        }
    }
}

/// Runs this exe with `args` as administrator (Windows shows its permission prompt) and waits.
fn run_elevated(args: &[String]) -> Result<u32, String> {
    use windows::core::{w, HSTRING};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let file = HSTRING::from(exe.as_os_str());
    let params = HSTRING::from(args.iter().map(|a| format!("\"{a}\"")).collect::<Vec<_>>().join(" "));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: windows::core::PCWSTR(file.as_ptr()),
        lpParameters: windows::core::PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info).map_err(|_| "Windows' admin permission was declined".to_string())?;
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        Ok(code)
    }
}
