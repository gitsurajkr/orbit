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

/// True if the peer at `ip` is logged into the same Tailscale account (user) as this PC.
/// Tailscale proves that cryptographically when a device joins, so ORBIT can trust it.
/// Peers of other users (shared machines, other people on the tailnet) are false.
pub fn parse_same_account(json: &str, ip: IpAddr) -> Option<bool> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let ip = ip.to_canonical().to_string();
    let me = v.get("Self")?.get("UserID")?.as_u64()?;
    let peer = v.get("Peer")?.as_object()?.values().find(|p| {
        p.get("TailscaleIPs").and_then(|a| a.as_array()).is_some_and(|a| a.iter().any(|x| x.as_str() == Some(ip.as_str())))
    })?;
    // Shared-in machines from other tailnets carry the sharer's user, but not this tailnet's.
    if peer.get("ShareeNode").and_then(|x| x.as_bool()) == Some(true) {
        return Some(false);
    }
    Some(peer.get("UserID")?.as_u64()? == me)
}

/// [`parse_same_account`] on live `tailscale status`; None if Tailscale can't be asked.
pub fn same_account(ip: IpAddr) -> Option<bool> {
    parse_same_account(&status_json()?, ip)
}

/// What the local Tailscale says, for the app's hints and device list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// tailscale.exe was found.
    pub installed: bool,
    /// "Running", "NeedsLogin", "Stopped", "Starting", ... (empty if not installed).
    pub state: String,
    /// Where to sign in, when Tailscale asks for it.
    pub auth_url: String,
    /// This PC's Tailscale machine name (as in Tailscale's admin page).
    pub self_name: String,
    pub peers: Vec<Peer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// Machine name, e.g. "workstation".
    pub name: String,
    pub ip: String,
    pub os: String,
    pub online: bool,
    /// Shared with this account from someone else's Tailscale.
    pub shared_in: bool,
}

impl Snapshot {
    pub fn running(&self) -> bool {
        self.state == "Running"
    }

    pub fn peer(&self, ip: IpAddr) -> Option<&Peer> {
        let ip = ip.to_canonical().to_string();
        self.peers.iter().find(|p| p.ip == ip)
    }
}

/// "workstation.tail7bd7fb.ts.net." -> "workstation" (falls back to the host name).
fn machine_name(v: &serde_json::Value) -> String {
    let dns = v.get("DNSName").and_then(|x| x.as_str()).unwrap_or("");
    match dns.split('.').next() {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => v.get("HostName").and_then(|x| x.as_str()).unwrap_or("?").to_string(),
    }
}

pub fn parse_snapshot(json: &str) -> Snapshot {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Snapshot { installed: true, ..Default::default() };
    };
    let str_of = |x: Option<&serde_json::Value>| x.and_then(|x| x.as_str()).unwrap_or("").to_string();
    let mut peers: Vec<Peer> = v
        .get("Peer")
        .and_then(|p| p.as_object())
        .map(|m| {
            m.values()
                .filter_map(|p| {
                    let ip = p.get("TailscaleIPs")?.as_array()?.iter().filter_map(|x| x.as_str()).find(|x| x.contains('.'))?.to_string();
                    Some(Peer {
                        name: machine_name(p),
                        ip,
                        os: str_of(p.get("OS")),
                        online: p.get("Online").and_then(|x| x.as_bool()).unwrap_or(false),
                        shared_in: p.get("ShareeNode").and_then(|x| x.as_bool()).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    peers.sort_by(|a, b| b.online.cmp(&a.online).then(a.name.cmp(&b.name)));
    Snapshot {
        installed: true,
        state: str_of(v.get("BackendState")),
        auth_url: str_of(v.get("AuthURL")),
        self_name: v.get("Self").map(machine_name).unwrap_or_default(),
        peers,
    }
}

/// The local Tailscale's state (runs the CLI, ~50 ms: call it off the UI thread).
pub fn snapshot() -> Snapshot {
    match status_json() {
        Some(j) => parse_snapshot(&j),
        // The CLI exits with an error when the service is stopped: installed but not running.
        None if tailscale_exe().exists() => Snapshot { installed: true, state: "Stopped".into(), ..Default::default() },
        None => Snapshot::default(),
    }
}

fn tailscale_exe() -> std::path::PathBuf {
    std::path::PathBuf::from(r"C:\Program Files\Tailscale\tailscale.exe")
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
    fn snapshot_lists_peers_by_machine_name() {
        let j = r#"{"BackendState":"Running","AuthURL":"","Self":{"DNSName":"omen.tail.ts.net.","HostName":"DESKTOP-1"},"Peer":{
            "a":{"DNSName":"workstation.tail.ts.net.","HostName":"DESKTOP0312","TailscaleIPs":["100.90.182.75","fd7a::1"],"OS":"windows","Online":true},
            "b":{"DNSName":"","HostName":"pi","TailscaleIPs":["fd7a::2","100.1.2.3"],"OS":"linux","Online":false,"ShareeNode":true}}}"#;
        let s = parse_snapshot(j);
        assert!(s.running() && s.installed);
        assert_eq!(s.self_name, "omen");
        assert_eq!(s.peers.len(), 2);
        assert_eq!((s.peers[0].name.as_str(), s.peers[0].ip.as_str(), s.peers[0].online), ("workstation", "100.90.182.75", true), "online first");
        assert_eq!((s.peers[1].name.as_str(), s.peers[1].ip.as_str(), s.peers[1].shared_in), ("pi", "100.1.2.3", true), "falls back to the host name");
        assert_eq!(s.peer("100.90.182.75".parse().unwrap()).map(|p| p.name.as_str()), Some("workstation"));
        let login = parse_snapshot(r#"{"BackendState":"NeedsLogin","AuthURL":"https://login.tailscale.com/a/xyz"}"#);
        assert!(!login.running() && login.auth_url.ends_with("xyz"));
    }

    #[test]
    fn same_account_compares_user_ids() {
        let j = r#"{"Self":{"UserID":5,"TailscaleIPs":["100.1.1.1"]},"Peer":{
            "a":{"TailscaleIPs":["100.2.2.2"],"UserID":5},
            "b":{"TailscaleIPs":["100.3.3.3"],"UserID":6},
            "c":{"TailscaleIPs":["100.4.4.4"],"UserID":5,"ShareeNode":true}}}"#;
        assert_eq!(parse_same_account(j, "100.2.2.2".parse().unwrap()), Some(true));
        assert_eq!(parse_same_account(j, "100.3.3.3".parse().unwrap()), Some(false));
        assert_eq!(parse_same_account(j, "100.4.4.4".parse().unwrap()), Some(false));
        assert_eq!(parse_same_account(j, "100.9.9.9".parse().unwrap()), None);
    }

    #[test]
    fn matches_ipv6_and_v4_mapped_addresses() {
        let s = status("1.2.3.4:1", "bom", "", true);
        assert_eq!(parse_path(&s, "fd7a:115c:a1e0::1".parse().unwrap()), Some(Path::Direct));
        // The server sees IPv4 clients as ::ffff:a.b.c.d on a dual-stack socket.
        assert_eq!(parse_path(&s, "::ffff:100.90.182.75".parse().unwrap()), Some(Path::Direct));
    }
}
