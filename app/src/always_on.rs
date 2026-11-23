//! The app's view of the ORBIT Service (see service.rs), which the installer sets up and which
//! does the sharing when it's there (lock screen and UAC prompts included): is it running, and
//! what it's doing (asked over the pipe, see pipe.rs, once a second on a background thread so
//! the UI never waits). Changes (SHARE / STOP, settings, paired devices) go over the pipe too.

use crate::service;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows_service::service::ServiceState;

pub struct AlwaysOn {
    /// The service is installed and running.
    running: Arc<AtomicBool>,
    /// Latest status from the agent (None until it answers).
    snapshot: Arc<Mutex<Option<Value>>>,
    pub error: Option<String>,
}

/// The agent's paired devices, code and ID.
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
            .name("orbit-service".into())
            .spawn(move || loop {
                let on = service::status() == Some(ServiceState::Running);
                r.store(on, Ordering::Release);
                let snap = if on { crate::pipe::call(&json!({"cmd": "status"})).ok() } else { None };
                *s.lock().unwrap() = snap;
                ctx.request_repaint();
                std::thread::sleep(Duration::from_secs(1));
            })
            .ok();
        AlwaysOn { running, snapshot, error: None }
    }

    /// The service is there to do the sharing.
    pub fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn status(&self) -> Option<Value> {
        self.snapshot.lock().unwrap().clone()
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
        self.error = crate::pipe::call(&req).err().map(|e| format!("{e:#}"));
        if let Ok(v) = crate::pipe::call(&json!({"cmd": "status"})) {
            *self.snapshot.lock().unwrap() = Some(v);
        }
    }
}
