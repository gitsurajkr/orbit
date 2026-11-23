//! "Always on" sharing that also works on the lock screen and UAC prompts.
//!
//! Windows only lets a process running as SYSTEM see and control the secure desktop (lock /
//! sign-in screen, Ctrl+Alt+Del, UAC). So:
//!
//! ```text
//! ORBIT Service  (orbit.exe --service, SYSTEM, session 0, starts with Windows)
//!   └─ keeps one ORBIT Agent running in the session on the physical screen:
//!      orbit.exe --agent  (SYSTEM token moved into that session)
//!        = the normal sharing server, following the input desktop (server/src/desktop.rs)
//! ```
//!
//! The service itself never touches the screen or the network: it only (re)starts the agent
//! when it exits or another session takes the screen (sign-out, user switch).
//!
//! The installer sets the service up, so every share includes the lock screen. The agent only
//! listens while sharing is on (SHARE / STOP in the app, over the pipe). After a restart,
//! sharing comes back on only if "start ORBIT when Windows starts" is ticked: when Windows
//! shuts down, the service sets `sharing` to `share_at_boot`. (A plain service stop, e.g.
//! during an update, keeps sharing as it was.)
//!
//! The agent keeps its ID certificate, paired devices and settings in `C:\ProgramData\ORBIT`,
//! which only SYSTEM and administrators may open.

use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows_service::service::*;
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

pub const NAME: &str = "ORBIT";
const DISPLAY: &str = "ORBIT Remote Desktop";

/// `C:\ProgramData\ORBIT`: the agent's private folder.
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("ORBIT")
}

/// `C:\ProgramData\ORBIT\logs`: readable by users.
pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

// ───────────────────────────── install / remove ─────────────────────────────

/// Registers and starts the service (needs administrator rights). `settings` become the
/// agent's; this user's ID certificate and paired devices are copied to it, so paired PCs
/// keep connecting without a warning or a new code.
pub fn install(settings: Option<&AgentSettings>) -> Result<()> {
    let exe = std::env::current_exe()?;
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    match settings {
        Some(s) => save_agent_settings(s)?,
        None if !dir.join("agent.cfg").exists() => save_agent_settings(&load_agent_settings())?, // defaults
        None => {} // keep the settings across updates
    }
    if auth::Identity::load_from(&dir).is_err() {
        if let Ok(mine) = auth::Identity::load_or_create() {
            mine.save_in(&dir, true)?;
        }
    }
    auth::Gate::load_in(&dir).merge_from(&auth::Gate::load());
    allow_software_sas();
    // Only SYSTEM and administrators may read the agent's key and device list.
    let ok = std::process::Command::new("icacls")
        .arg(&dir)
        .args(["/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "/grant:r", "*S-1-5-32-544:(OI)(CI)F", "/q"])
        .status()?
        .success();
    if !ok {
        bail!("couldn't restrict {}", dir.display());
    }
    // Logs (status lines only, no secrets) may be read by users, to see what the agent does.
    let logs = log_dir();
    std::fs::create_dir_all(&logs)?;
    let _ = std::process::Command::new("icacls").arg(&logs).args(["/grant", "*S-1-5-32-545:(OI)(CI)RX", "/q"]).status();
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    let info = ServiceInfo {
        name: OsString::from(NAME),
        display_name: OsString::from(DISPLAY),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec![OsString::from("--service")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let svc = match mgr.open_service(NAME, ServiceAccess::all()) {
        Ok(s) => {
            s.change_config(&info)?; // already there (e.g. ORBIT moved): update it
            s
        }
        Err(_) => mgr.create_service(&info, ServiceAccess::all())?,
    };
    svc.set_description("Shares this PC with paired ORBIT devices, including the lock screen and UAC prompts.")?;
    // Restart it if it ever crashes.
    svc.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86_400)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(2) }; 3]),
    })?;
    if svc.query_status()?.current_state != ServiceState::Running {
        svc.start::<&str>(&[])?;
    }
    Ok(())
}

/// Lets services press Ctrl+Alt+Del (`SendSAS`): the SoftwareSASGeneration policy, raised to
/// include services (1) without removing what's already allowed (2 = Ease of Access apps).
fn allow_software_sas() {
    use windows::core::w;
    use windows::Win32::System::Registry::*;
    unsafe {
        let key = w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Policies\\System");
        let mut cur = 0u32;
        let mut size = 4u32;
        let _ = RegGetValueW(HKEY_LOCAL_MACHINE, key, w!("SoftwareSASGeneration"), RRF_RT_REG_DWORD, None, Some(&mut cur as *mut u32 as *mut _), Some(&mut size));
        let want = cur | 1;
        if want != cur {
            let _ = RegSetKeyValueW(HKEY_LOCAL_MACHINE, key, w!("SoftwareSASGeneration"), REG_DWORD.0, Some(&want as *const u32 as *const _), 4);
        }
    }
}

/// Stops and removes the service (needs administrator rights). Its folder stays, so turning
/// it back on keeps the paired devices; they're also copied back to this user's list.
pub fn uninstall() -> Result<()> {
    if data_dir().join("trusted.json").exists() {
        auth::Gate::load().merge_from(&auth::Gate::load_in(&data_dir()));
    }
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let Ok(svc) = mgr.open_service(NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE) else {
        return Ok(()); // not installed
    };
    if svc.query_status()?.current_state != ServiceState::Stopped {
        let _ = svc.stop();
        let t = Instant::now();
        while svc.query_status()?.current_state != ServiceState::Stopped && t.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    svc.delete()?;
    Ok(())
}

/// Installed and running?
pub fn status() -> Option<ServiceState> {
    status_detail().ok()
}

/// The service's state, or why it couldn't be read.
pub fn status_detail() -> Result<ServiceState> {
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).context("opening the service manager")?;
    let svc = mgr.open_service(NAME, ServiceAccess::QUERY_STATUS).context("opening the ORBIT service")?;
    Ok(svc.query_status().context("querying it")?.current_state)
}

// ───────────────────────────── the service ─────────────────────────────

windows_service::define_windows_service!(ffi_service_main, service_main);

/// Entry point for `orbit.exe --service` (only works when started by Windows).
pub fn run() -> Result<()> {
    windows_service::service_dispatcher::start(NAME, ffi_service_main).context("not started by the Service Control Manager")?;
    Ok(())
}

fn service_main(_args: Vec<OsString>) {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let handler = move |c| match c {
        ServiceControl::Shutdown => {
            // Windows is shutting down or restarting: share after it only if asked to.
            let mut cfg = load_agent_settings();
            cfg.sharing = cfg.share_at_boot;
            let _ = save_agent_settings(&cfg);
            s.store(true, Ordering::Release);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Stop => {
            s.store(true, Ordering::Release);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let Ok(status) = service_control_handler::register(NAME, handler) else { return };
    let report = |state, accept| {
        let _ = status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(5),
            process_id: None,
        });
    };
    report(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN);
    supervise(&stop);
    report(ServiceState::Stopped, ServiceControlAccept::empty());
}

/// Keeps one agent running in the console session until `stop`.
fn supervise(stop: &AtomicBool) {
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    // The agent asks for Ctrl+Alt+Del through this event (only the service may press it).
    // Default security: SYSTEM and administrators only.
    let sas = unsafe { CreateEventW(None, false, false, &windows::core::HSTRING::from(server::desktop::SAS_EVENT)) }.ok();
    let mut agent: Option<(u32, spawn::Child)> = None; // (session, process)
    let mut last_try = Instant::now() - Duration::from_secs(60);
    while !stop.load(Ordering::Acquire) {
        let session = spawn::console_session();
        let alive = agent.as_ref().is_some_and(|(s, c)| Some(*s) == session && c.running());
        if !alive {
            if let Some((_, c)) = agent.take() {
                c.kill(); // the screen went to another session
            }
            // Don't spin if it keeps failing (e.g. no session yet during boot).
            if let (Some(sess), true) = (session, last_try.elapsed() >= Duration::from_secs(2)) {
                last_try = Instant::now();
                match spawn::agent_in_session(sess) {
                    Ok(c) => agent = Some((sess, c)),
                    Err(e) => log(&format!("starting the agent in session {sess}: {e:#}")),
                }
            }
        }
        match sas {
            Some(ev) if unsafe { WaitForSingleObject(ev, 500) } == windows::Win32::Foundation::WAIT_OBJECT_0 => {
                log(&format!("Ctrl+Alt+Del: {}", if server::desktop::sas_now() { "sent" } else { "failed" }));
            }
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(500)),
        }
    }
    if let Some((_, c)) = agent {
        c.kill();
    }
}

fn log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_dir().join("service.log")) {
        let _ = writeln!(f, "{} {msg}", unix_now());
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

mod spawn {
    use anyhow::{Context, Result};
    use windows::core::{HSTRING, PWSTR};
    use windows::Win32::Foundation::*;
    use windows::Win32::Security::*;
    use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
    use windows::Win32::System::Threading::*;

    pub struct Child(HANDLE);
    unsafe impl Send for Child {}

    impl Child {
        pub fn running(&self) -> bool {
            unsafe { WaitForSingleObject(self.0, 0) == WAIT_TIMEOUT }
        }
        pub fn kill(self) {
            unsafe {
                let _ = TerminateProcess(self.0, 0);
                let _ = WaitForSingleObject(self.0, 3000);
            }
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// The session on the physical screen, if any.
    pub fn console_session() -> Option<u32> {
        let s = unsafe { WTSGetActiveConsoleSessionId() };
        (s != 0xFFFF_FFFF).then_some(s)
    }

    /// Starts `orbit.exe --agent` as SYSTEM in `session`, on its normal desktop (the agent
    /// follows the input desktop from there).
    pub fn agent_in_session(session: u32) -> Result<Child> {
        unsafe {
            let mut own = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT | TOKEN_ADJUST_SESSIONID, &mut own)?;
            let mut tok = HANDLE::default();
            let dup = DuplicateTokenEx(own, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut tok);
            let _ = CloseHandle(own);
            dup.context("DuplicateTokenEx")?;
            let result = (|| {
                SetTokenInformation(tok, TokenSessionId, &session as *const u32 as *const _, 4).context("moving the token to the session")?;
                let exe = std::env::current_exe()?;
                let mut cmd: Vec<u16> = format!("\"{}\" --agent", exe.display()).encode_utf16().chain([0]).collect();
                let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain([0]).collect();
                let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, lpDesktop: PWSTR(desktop.as_mut_ptr()), ..Default::default() };
                let mut pi = PROCESS_INFORMATION::default();
                CreateProcessAsUserW(tok, &HSTRING::from(exe.as_os_str()), PWSTR(cmd.as_mut_ptr()), None, None, false, CREATE_NO_WINDOW, None, None, &si, &mut pi)
                    .context("CreateProcessAsUser")?;
                let _ = CloseHandle(pi.hThread);
                Ok(Child(pi.hProcess))
            })();
            let _ = CloseHandle(tok);
            result
        }
    }
}

// ───────────────────────────── the agent ─────────────────────────────

/// Saved by the app (SHARE tab): what the agent shares and how.
pub struct AgentSettings {
    pub fps: u32,
    pub mbps: f32,
    pub monitor: usize,
    /// Sharing right now (SHARE / STOP).
    pub sharing: bool,
    /// Share again after Windows restarts ("start ORBIT when Windows starts").
    pub share_at_boot: bool,
}

pub fn save_agent_settings(s: &AgentSettings) -> Result<()> {
    std::fs::create_dir_all(data_dir())?;
    std::fs::write(
        data_dir().join("agent.cfg"),
        format!("fps={}\nmbps={}\nmonitor={}\nsharing={}\nshare_at_boot={}\n", s.fps, s.mbps, s.monitor, s.sharing as u8, s.share_at_boot as u8),
    )?;
    Ok(())
}

pub fn load_agent_settings() -> AgentSettings {
    let mut s = AgentSettings { fps: 60, mbps: 20.0, monitor: 0, sharing: false, share_at_boot: false };
    if let Ok(t) = std::fs::read_to_string(data_dir().join("agent.cfg")) {
        // v0.3.0's service always shared (it was the "start with Windows" switch): keep that.
        if !t.contains("sharing=") {
            (s.sharing, s.share_at_boot) = (true, true);
        }
        for line in t.lines() {
            match line.split_once('=') {
                Some(("fps", v)) => s.fps = v.trim().parse().unwrap_or(60),
                Some(("mbps", v)) => s.mbps = v.trim().parse().unwrap_or(20.0),
                Some(("monitor", v)) => s.monitor = v.trim().parse().unwrap_or(0),
                Some(("sharing", v)) => s.sharing = v.trim() == "1",
                Some(("share_at_boot", v)) => s.share_at_boot = v.trim() == "1",
                _ => {}
            }
        }
    }
    s
}

/// The signed-in user's `Downloads\ORBIT` (received files), or a folder in ProgramData when
/// nobody is signed in.
fn user_downloads(session: u32) -> PathBuf {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
    use windows::Win32::UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    unsafe {
        let mut tok = HANDLE::default();
        if WTSQueryUserToken(session, &mut tok).is_ok() {
            let p = SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, tok);
            let _ = CloseHandle(tok);
            if let Ok(p) = p {
                let s = p.to_string();
                CoTaskMemFree(Some(p.0 as *const _));
                if let Ok(s) = s {
                    return PathBuf::from(s).join("ORBIT");
                }
            }
        }
    }
    data_dir().join("Received")
}

/// `orbit.exe --agent`: shares this session's screen as SYSTEM until killed.
pub fn run_agent() -> Result<()> {
    let dir = data_dir();
    std::fs::create_dir_all(log_dir())?;
    // Log to agent.log (the agent has no console); keep the previous run's log as agent.old.log.
    let log_path = log_dir().join("agent.log");
    let _ = std::fs::rename(&log_path, log_dir().join("agent.old.log"));
    if let Ok(f) = std::fs::File::create(&log_path) {
        use std::os::windows::io::IntoRawHandle;
        use windows::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
        let h = windows::Win32::Foundation::HANDLE(f.into_raw_handle());
        unsafe {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, h);
            let _ = SetStdHandle(STD_ERROR_HANDLE, h);
        }
    }
    let session = spawn::console_session().unwrap_or(0);
    std::env::set_var("ORBIT_HOME", &dir);
    std::env::set_var("ORBIT_DOWNLOADS", user_downloads(session));
    println!("ORBIT agent v{} | session {session} | SYSTEM: {} | input desktop: {:?}", env!("CARGO_PKG_VERSION"), server::desktop::running_as_system(), server::desktop::input_desktop_name());
    server::dpi_aware();
    let identity = auth::Identity::load_or_create_in(&dir, true)?;
    let gate = auth::Gate::load();
    let current: Arc<std::sync::Mutex<Option<Arc<server::Status>>>> = Arc::default();
    let restart = Arc::new(AtomicBool::new(false));
    {
        let (gate, current, restart, id) = (gate.clone(), current.clone(), restart.clone(), auth::short(&identity.fingerprint()));
        std::thread::Builder::new().name("pipe".into()).spawn(move || {
            crate::pipe::serve(|req| agent_request(req, &gate, &current, &restart, &id), || crate::pipe::session_user_sid(session));
        })?;
    }
    loop {
        let cfg = load_agent_settings();
        restart.store(false, Ordering::Release);
        if !cfg.sharing {
            // Not sharing: nothing listens. Wait for SHARE (or a settings change).
            println!("not sharing (press SHARE in ORBIT)");
            current.lock().unwrap().take();
            while !restart.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(250));
            }
            continue;
        }
        let start = || -> Result<server::ServerHandle> {
            let monitor = server::preflight(cfg.monitor)?;
            let settings = server::Settings { monitor, fps: cfg.fps, bitrate_bps: (cfg.mbps * 1e6) as u32, identity: identity.clone(), gate: gate.clone() };
            server::ServerHandle::start(settings, server::net::default_bind())
        };
        match start() {
            Ok(h) => {
                println!("sharing on {} | {} fps, {} Mbit/s, screen {} | ID {}", h.bind, cfg.fps, cfg.mbps, cfg.monitor + 1, auth::short(&identity.fingerprint()));
                *current.lock().unwrap() = Some(h.status.clone());
                let mut last = String::new();
                loop {
                    std::thread::sleep(Duration::from_millis(250));
                    if restart.load(Ordering::Acquire) {
                        println!("settings changed: restarting sharing");
                        current.lock().unwrap().take();
                        h.stop();
                        break;
                    }
                    let st = h.status.get();
                    let line = format!("{:?} {:?} {:?}", st.client_name, st.error, server::desktop::input_desktop_name());
                    if line != last {
                        println!("status: client {:?} | error {:?} | desktop {:?}", st.client_name, st.error, server::desktop::input_desktop_name());
                        last = line;
                    }
                }
            }
            Err(e) => {
                println!("can't share yet: {e:#}; retrying in 5 s");
                for _ in 0..20 {
                    if restart.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
    }
}

/// Answers one request from the app (see pipe.rs).
fn agent_request(req: &serde_json::Value, gate: &auth::Gate, current: &std::sync::Mutex<Option<Arc<server::Status>>>, restart: &AtomicBool, id: &str) -> serde_json::Value {
    use serde_json::json;
    let cmd = req.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
    match cmd {
        "status" => {
            let st = current.lock().unwrap().as_ref().map(|s| s.get()).unwrap_or_default();
            let cfg = load_agent_settings();
            let devices: Vec<_> = gate.devices().iter().map(|d| json!({"name": d.name, "fp": auth::hex(&d.fp), "how": d.how, "paired": d.paired_unix})).collect();
            let pin = gate.pin().map(|(code, left)| json!({"code": code, "left": left.as_secs()}));
            json!({
                "version": env!("CARGO_PKG_VERSION"),
                "id": id,
                "client": st.client, "client_name": st.client_name, "stats": st.stats, "transfer": st.transfer, "error": st.error.map(|e| format!("{e}")),
                "devices": devices, "trust_own": gate.trust_own_account(), "pin": pin,
                "fps": cfg.fps, "mbps": cfg.mbps, "monitor": cfg.monitor,
                "sharing": cfg.sharing, "share_at_boot": cfg.share_at_boot,
                "desktop": server::desktop::input_desktop_name(),
            })
        }
        "remove" => match req.get("fp").and_then(|f| f.as_str()).and_then(auth::unhex) {
            Some(fp) => {
                gate.remove(&fp);
                json!({"ok": true})
            }
            None => json!({"error": "bad fingerprint"}),
        },
        "trust_own" => {
            gate.set_trust_own_account(req.get("on").and_then(|v| v.as_bool()).unwrap_or(true));
            json!({"ok": true})
        }
        "pin_start" => json!({"code": gate.start_pin()}),
        "pin_cancel" => {
            gate.cancel_pin();
            json!({"ok": true})
        }
        "settings" => {
            let mut cfg = load_agent_settings();
            if let Some(v) = req.get("fps").and_then(|v| v.as_u64()) {
                cfg.fps = (v as u32).clamp(15, 240);
            }
            if let Some(v) = req.get("mbps").and_then(|v| v.as_f64()) {
                cfg.mbps = (v as f32).clamp(1.0, 200.0);
            }
            if let Some(v) = req.get("monitor").and_then(|v| v.as_u64()) {
                cfg.monitor = v as usize;
            }
            if let Some(v) = req.get("sharing").and_then(|v| v.as_bool()) {
                cfg.sharing = v;
            }
            if let Some(v) = req.get("share_at_boot").and_then(|v| v.as_bool()) {
                cfg.share_at_boot = v;
            }
            match save_agent_settings(&cfg) {
                Ok(()) => {
                    restart.store(true, Ordering::Release);
                    json!({"ok": true})
                }
                Err(e) => json!({"error": format!("{e:#}")}),
            }
        }
        _ => json!({"error": format!("unknown request {cmd:?}")}),
    }
}
