//! How Tailscale carries traffic to a peer: direct, or through a relay.
//!
//! A relayed link adds tens of milliseconds, and nothing in ORBIT's own numbers says why the RTT
//! is high. So both sides ask the local Tailscale (`tailscale status --json`) every few seconds
//! and show the answer next to their stats:
//!
//! - `CurAddr` set (an ip:port)  → packets go straight to the peer: **DIRECT**
//! - `PeerRelay` set             → through one of your own Tailscale peer relays: **PEER RELAY**
//! - neither                     → through Tailscale's DERP relay in `Relay` (a city code such
//!                                 as `bom`): **RELAY bom**
//!
//! Tailscale only knows the path while traffic flows (`Active`); an idle peer reads as relayed
//! even if it would go direct, so an inactive peer counts as unknown.
//! Tailscale often starts on a relay and upgrades to direct seconds later, hence the polling.

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Path {
    Direct,
    PeerRelay,
    /// Tailscale DERP relay, with its region code (e.g. "bom").
    Relay(String),
}

impl Path {
    /// Short uppercase label for stats lines.
    pub fn label(&self) -> String {
        match self {
            Path::Direct => "DIRECT".into(),
            Path::PeerRelay => "PEER RELAY".into(),
            Path::Relay(r) if r.is_empty() => "RELAY".into(),
            Path::Relay(r) => format!("RELAY {}", r.to_uppercase()),
        }
    }
}

/// Finds the peer with Tailscale address `ip` in `tailscale status --json` output.
pub fn parse_path(json: &str, ip: IpAddr) -> Option<Path> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let ip = ip.to_canonical().to_string();
    let peer = v.get("Peer")?.as_object()?.values().find(|p| {
        p.get("TailscaleIPs").and_then(|a| a.as_array()).is_some_and(|a| a.iter().any(|x| x.as_str() == Some(ip.as_str())))
    })?;
    if peer.get("Active").and_then(|a| a.as_bool()) != Some(true) {
        return None;
    }
    let s = |k: &str| peer.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    Some(if !s("CurAddr").is_empty() {
        Path::Direct
    } else if !s("PeerRelay").is_empty() {
        Path::PeerRelay
    } else {
        Path::Relay(s("Relay"))
    })
}

/// Runs `tailscale status --json` (no console window) and returns its output.
fn status_json() -> Option<String> {
    let installed = std::path::Path::new(r"C:\Program Files\Tailscale\tailscale.exe");
    let exe = if installed.exists() { installed.as_os_str() } else { std::ffi::OsStr::new("tailscale") };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["status", "--json"]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The current path to `ip`, or `None` if Tailscale isn't installed, the peer isn't found, or
/// there's no traffic to it right now. Takes ~100 ms (runs the CLI): call it off hot paths.
pub fn path_to(ip: IpAddr) -> Option<Path> {
    parse_path(&status_json()?, ip)
}

/// Polls [`path_to`] on its own thread every few seconds until dropped.
pub struct PathWatcher {
    latest: Arc<Mutex<Option<Path>>>,
    stop: Arc<AtomicBool>,
}

impl PathWatcher {
    pub fn start(ip: IpAddr) -> PathWatcher {
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (l, s) = (latest.clone(), stop.clone());
        let _ = std::thread::Builder::new().name("tailscale-path".into()).spawn(move || {
            // Loopback and LAN addresses aren't Tailscale peers: nothing to watch.
            if ip.to_canonical().is_loopback() {
                return;
            }
            while !s.load(Ordering::Acquire) {
                let p = path_to(ip);
                *l.lock().unwrap() = p;
                for _ in 0..50 {
                    if s.load(Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        PathWatcher { latest, stop }
    }

    pub fn get(&self) -> Option<Path> {
        self.latest.lock().unwrap().clone()
    }

    /// `" · DIRECT"`-style suffix for a stats line with `sep` before it, or "" when unknown.
    pub fn suffix(&self, sep: &str) -> String {
        self.get().map(|p| format!("{sep}{}", p.label())).unwrap_or_default()
    }
}

impl Drop for PathWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(cur: &str, relay: &str, peer_relay: &str, active: bool) -> String {
        format!(
            r#"{{"Self":{{"TailscaleIPs":["100.1.1.1"]}},"Peer":{{
                "nodekey:a":{{"TailscaleIPs":["100.9.9.9"],"CurAddr":"","Relay":"fra","PeerRelay":"","Active":true}},
                "nodekey:b":{{"TailscaleIPs":["100.90.182.75","fd7a:115c:a1e0::1"],"CurAddr":"{cur}","Relay":"{relay}","PeerRelay":"{peer_relay}","Active":{active}}}
            }}}}"#
        )
    }

    #[test]
    fn reads_each_path_kind() {
        let ip: IpAddr = "100.90.182.75".parse().unwrap();
        assert_eq!(parse_path(&status("1.2.3.4:41641", "bom", "", true), ip), Some(Path::Direct));
        assert_eq!(parse_path(&status("", "bom", "100.5.5.5:7777", true), ip), Some(Path::PeerRelay));
        assert_eq!(parse_path(&status("", "bom", "", true), ip), Some(Path::Relay("bom".into())));
        assert_eq!(Path::Relay("bom".into()).label(), "RELAY BOM");
    }

    #[test]
    fn idle_unknown_or_garbage_is_none() {
        let ip: IpAddr = "100.90.182.75".parse().unwrap();
        assert_eq!(parse_path(&status("", "bom", "", false), ip), None);
        assert_eq!(parse_path(&status("", "bom", "", true), "100.2.2.2".parse().unwrap()), None);
        assert_eq!(parse_path("not json", ip), None);
    }

    #[test]
    fn matches_ipv6_and_v4_mapped_addresses() {
        let s = status("1.2.3.4:1", "bom", "", true);
        assert_eq!(parse_path(&s, "fd7a:115c:a1e0::1".parse().unwrap()), Some(Path::Direct));
        // The server sees IPv4 clients as ::ffff:a.b.c.d on a dual-stack socket.
        assert_eq!(parse_path(&s, "::ffff:100.90.182.75".parse().unwrap()), Some(Path::Direct));
    }
}
