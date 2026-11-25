//! Self-update from GitHub Releases.
//!
//! - `check`: asks GitHub for the latest release of [`REPO`]; if its tag (vX.Y.Z) is newer than
//!   this build, the launcher shows an update banner.
//! - `download`: fetches `ORBIT-Setup.exe` over HTTPS and checks it against the release's
//!   `ORBIT-Setup.exe.sha256` (catches broken or truncated downloads).
//! - `install`: runs the installer silently and quits; the installer replaces orbit.exe and
//!   starts the new version.
//!
//! Nothing is ever installed without the user clicking UPDATE.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const REPO: &str = "gitsurajkr/orbit";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const INSTALLER: &str = "ORBIT-Setup.exe";
const CHECK_EVERY: Duration = Duration::from_secs(30 * 60);
const USER_AGENT: &str = concat!("ORBIT-updater/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    installer_url: String,
    sha256_url: Option<String>,
}

#[derive(Clone, Debug)]
pub enum State {
    Idle,
    /// A manual check is running.
    Checking,
    /// A manual check found nothing newer (shown briefly).
    UpToDate(Instant),
    Available(Release),
    Downloading { version: String },
    Failed(String),
}

pub struct Updater {
    state: Arc<Mutex<State>>,
    done: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    last_check: Option<Instant>,
    /// Set when the installer is downloaded and verified: the UI launches it and quits.
    ready: Arc<Mutex<Option<PathBuf>>>,
    dismissed: Option<String>,
}

fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let mut it = s.trim().trim_start_matches(['v', 'V']).split('.').map(|p| p.split(|c: char| !c.is_ascii_digit()).next().unwrap_or("").parse::<u32>());
    Some((it.next()?.ok()?, it.next().unwrap_or(Ok(0)).ok()?, it.next().unwrap_or(Ok(0)).ok()?))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).timeout_read(Duration::from_secs(30)).user_agent(USER_AGENT).build()
}

/// The latest release, if it's newer than this build.
pub fn check() -> Result<Option<Release>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = match agent().get(&url).set("Accept", "application/vnd.github+json").call() {
        Ok(r) => r,
        // ORBIT always has releases: a 404 means the repository moved (v0.3.7 and older still
        // asked under its old name and silently reported "up to date" forever).
        Err(ureq::Error::Status(404, _)) => bail!("no releases found at github.com/{REPO}"),
        Err(e) => return Err(e).context("checking for updates"),
    };
    let json: serde_json::Value = resp.into_json()?;
    let tag = json["tag_name"].as_str().context("release has no tag")?;
    if !is_newer(tag, CURRENT) {
        return Ok(None);
    }
    let assets = json["assets"].as_array().cloned().unwrap_or_default();
    let find = |name: &str| assets.iter().find(|a| a["name"].as_str() == Some(name)).and_then(|a| a["browser_download_url"].as_str()).map(String::from);
    let installer_url = find(INSTALLER).with_context(|| format!("release {tag} has no {INSTALLER}"))?;
    Ok(Some(Release { version: tag.trim_start_matches('v').to_string(), installer_url, sha256_url: find(&format!("{INSTALLER}.sha256")) }))
}

fn download(rel: &Release, done: &AtomicU64, total: &AtomicU64) -> Result<PathBuf> {
    let resp = agent().get(&rel.installer_url).call().context("downloading the update")?;
    let len: u64 = resp.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0);
    total.store(len, Ordering::Relaxed);
    done.store(0, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("ORBIT-Setup-{}.exe", rel.version));
    let mut file = std::fs::File::create(&path).with_context(|| format!("can't write {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        done.fetch_add(n as u64, Ordering::Relaxed);
    }
    file.flush()?;
    drop(file);
    let got = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>();
    if len > 0 && done.load(Ordering::Relaxed) != len {
        bail!("the download was cut off; try again");
    }
    if let Some(url) = &rel.sha256_url {
        let expected = agent().get(url).call()?.into_string()?;
        let expected = expected.split_whitespace().next().unwrap_or("").to_lowercase();
        if expected != got {
            let _ = std::fs::remove_file(&path);
            bail!("the downloaded installer doesn't match its checksum; not installing it");
        }
    }
    Ok(path)
}

impl Updater {
    pub fn new() -> Updater {
        Updater {
            state: Arc::new(Mutex::new(State::Idle)),
            done: Arc::new(AtomicU64::new(0)),
            total: Arc::new(AtomicU64::new(0)),
            last_check: None,
            ready: Arc::new(Mutex::new(None)),
            dismissed: None,
        }
    }

    pub fn state(&self) -> State {
        let s = self.state.lock().unwrap().clone();
        match &s {
            State::Available(r) if self.dismissed.as_deref() == Some(r.version.as_str()) => State::Idle,
            State::UpToDate(t) if t.elapsed() > Duration::from_secs(4) => State::Idle,
            _ => s,
        }
    }

    /// True if a newer version is known (even if its banner was dismissed).
    pub fn update_known(&self) -> bool {
        matches!(*self.state.lock().unwrap(), State::Available(_))
    }

    /// The UPDATE button: check GitHub now and always report the result.
    pub fn check_now(&mut self, ctx: &eframe::egui::Context) {
        {
            let mut st = self.state.lock().unwrap();
            match &*st {
                State::Checking | State::Downloading { .. } => return,
                State::Available(_) => {
                    // Already known: just bring the banner back.
                    self.dismissed = None;
                    return;
                }
                _ => *st = State::Checking,
            }
        }
        self.dismissed = None;
        self.last_check = Some(Instant::now());
        let (state, ctx) = (self.state.clone(), ctx.clone());
        std::thread::spawn(move || {
            let next = match check() {
                Ok(Some(r)) => State::Available(r),
                Ok(None) => State::UpToDate(Instant::now()),
                Err(e) => State::Failed(format!("{e:#}")),
            };
            *state.lock().unwrap() = next;
            ctx.request_repaint();
            // Repaint again when the "up to date" note should disappear.
            std::thread::sleep(Duration::from_millis(4100));
            ctx.request_repaint();
        });
    }

    pub fn progress(&self) -> (u64, u64) {
        (self.done.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed))
    }

    /// Call every frame; checks GitHub at start and then every 30 minutes, in the background.
    pub fn tick(&mut self, ctx: &eframe::egui::Context) {
        if self.last_check.is_some_and(|t| t.elapsed() < CHECK_EVERY) {
            return;
        }
        if matches!(*self.state.lock().unwrap(), State::Downloading { .. } | State::Checking) {
            return;
        }
        self.last_check = Some(Instant::now());
        let (state, ctx) = (self.state.clone(), ctx.clone());
        std::thread::spawn(move || {
            let next = match check() {
                Ok(Some(r)) => State::Available(r),
                Ok(None) => State::Idle,
                // A failed check (offline, rate-limited) isn't shown; we try again later.
                Err(e) => {
                    eprintln!("update check failed: {e:#}");
                    return;
                }
            };
            let mut st = state.lock().unwrap();
            // Don't overwrite a manual check's result or a download in progress.
            if matches!(*st, State::Idle | State::Available(_) | State::UpToDate(_)) {
                *st = next;
            }
            drop(st);
            ctx.request_repaint();
        });
    }

    pub fn dismiss(&mut self) {
        if let State::Available(r) = &*self.state.lock().unwrap() {
            self.dismissed = Some(r.version.clone());
        }
    }

    pub fn start_download(&mut self, ctx: &eframe::egui::Context) {
        let State::Available(rel) = self.state() else { return };
        *self.state.lock().unwrap() = State::Downloading { version: rel.version.clone() };
        let (state, done, total, ready, ctx) = (self.state.clone(), self.done.clone(), self.total.clone(), self.ready.clone(), ctx.clone());
        std::thread::spawn(move || {
            match download(&rel, &done, &total) {
                Ok(path) => *ready.lock().unwrap() = Some(path),
                Err(e) => *state.lock().unwrap() = State::Failed(format!("{e:#}")),
            }
            ctx.request_repaint();
        });
    }

    /// If the installer is ready: start it (silent, it restarts ORBIT when done) and return true
    /// so the caller can quit.
    pub fn launch_if_ready(&mut self) -> Result<bool> {
        let Some(path) = self.ready.lock().unwrap().take() else { return Ok(false) };
        std::process::Command::new(&path)
            .args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CLOSEAPPLICATIONS"])
            .spawn()
            .with_context(|| format!("couldn't start {}", path.display()))?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        assert!(is_newer("v0.1.2", "0.1.1"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("v1.0", "0.9.9"));
        assert!(!is_newer("v0.1.1", "0.1.1"));
        assert!(!is_newer("v0.1.0", "0.1.1"));
        assert!(!is_newer("garbage", "0.1.1"));
        assert!(is_newer("v0.1.10", "0.1.9"));
    }
}
