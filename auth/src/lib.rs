//! Who may connect, and proof of who is who.
//!
//! In plain words:
//! - **ID card** ([`Identity`]): every ORBIT install makes a certificate + private key once and
//!   keeps it (the key encrypted with Windows DPAPI, so the file is useless on another PC or
//!   account). Both sides show theirs in the QUIC/TLS handshake, which proves each side holds
//!   its key. A certificate's SHA-256 is that PC's **fingerprint**.
//! - **Guest list** ([`Gate`], sharing PC): fingerprints of devices allowed in. A listed device
//!   connects straight away; anything else is stopped before any video or input starts.
//! - **Known PCs** ([`KnownPcs`], viewing PC): which fingerprint each address had last time, so
//!   a different PC answering at that address is noticed (like SSH's host keys).
//! - **Getting on the list**: automatically for devices on the same Tailscale account as the
//!   sharing PC (Tailscale already proved that), otherwise with a 6-digit code ([`Gate::start_pin`])
//!   checked by SPAKE2 ([`pake`]): the code is never sent, and someone in the middle gets one
//!   guess per attempt instead of something to brute-force offline.

pub mod pake;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub type Fingerprint = [u8; 32];

/// Where ORBIT keeps its files: `%APPDATA%\ORBIT` (`ORBIT_HOME` overrides it, for tests and a
/// second copy on one PC).
pub fn dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("ORBIT_HOME") {
        return Ok(PathBuf::from(d));
    }
    Ok(PathBuf::from(std::env::var_os("APPDATA").context("APPDATA not set")?).join("ORBIT"))
}

/// SHA-256 of a DER certificate.
pub fn fingerprint(cert_der: &[u8]) -> Fingerprint {
    Sha256::digest(cert_der).into()
}

/// First 8 bytes as `7F3A-91C2-0B5D-E8A4`: enough to compare by eye.
pub fn short(fp: &Fingerprint) -> String {
    fp[..8].chunks(2).map(|c| format!("{:02X}{:02X}", c[0], c[1])).collect::<Vec<_>>().join("-")
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Fingerprint> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// This PC's name, shown in the other PC's guest list.
pub fn device_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown PC".into())
}

/// Writes through a temp file, so a crash never leaves half a file.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ───────────────────────────── ID card ─────────────────────────────

/// This install's certificate and private key (PKCS#8), both DER.
#[derive(Clone)]
pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl Identity {
    pub fn fingerprint(&self) -> Fingerprint {
        fingerprint(&self.cert_der)
    }

    /// Loads `identity.bin`, or creates and saves a new identity the first time.
    pub fn load_or_create() -> Result<Identity> {
        let path = dir()?.join("identity.bin");
        if let Ok(blob) = std::fs::read(&path) {
            match dpapi::unprotect(&blob).and_then(|b| Identity::decode(&b)) {
                Ok(id) => return Ok(id),
                // Copied from another PC or account, or damaged: a new identity means paired
                // PCs will ask again, which is the safe outcome.
                Err(e) => eprintln!("identity.bin unreadable ({e:#}); making a new identity"),
            }
        }
        let c = rcgen::generate_simple_self_signed(vec!["orbit".to_string()])?;
        let id = Identity { cert_der: c.cert.der().to_vec(), key_der: c.key_pair.serialize_der() };
        write_atomic(&path, &dpapi::protect(&id.encode())?).with_context(|| format!("saving {}", path.display()))?;
        Ok(id)
    }

    fn encode(&self) -> Vec<u8> {
        let mut b = (self.cert_der.len() as u32).to_le_bytes().to_vec();
        b.extend_from_slice(&self.cert_der);
        b.extend_from_slice(&self.key_der);
        b
    }

    fn decode(b: &[u8]) -> Result<Identity> {
        let n = u32::from_le_bytes(b.get(..4).context("short")?.try_into()?) as usize;
        let cert = b.get(4..4 + n).context("short")?;
        let key = &b[4 + n..];
        if key.is_empty() {
            bail!("no key");
        }
        Ok(Identity { cert_der: cert.to_vec(), key_der: key.to_vec() })
    }
}

mod dpapi {
    use anyhow::Result;
    use windows::core::w;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};

    fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        unsafe {
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            let _ = LocalFree(HLOCAL(out.pbData as _));
            v
        }
    }

    /// Encrypts for the current Windows user on this PC only.
    pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe { CryptProtectData(&input, w!("ORBIT identity"), None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)? };
        Ok(take(out))
    }

    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe { CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)? };
        Ok(take(out))
    }
}

// ─────────────────────── guest list (sharing PC) ───────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub fp: Fingerprint,
    pub paired_unix: u64,
    /// "same Tailscale account" or "code".
    pub how: String,
}

/// How long a pairing code stays valid, and how many wrong tries it survives.
pub const PIN_LIFETIME: Duration = Duration::from_secs(300);
pub const PIN_TRIES: u32 = 5;

struct Pin {
    code: String,
    until: Instant,
    tries_left: u32,
}

struct GateState {
    devices: Vec<Device>,
    /// Let devices on this PC's own Tailscale account in without a code.
    trust_own_account: bool,
    pin: Option<Pin>,
}

/// The sharing PC's guest list and pairing code, shared by the server and the UI.
pub struct Gate {
    path: PathBuf,
    state: Mutex<GateState>,
}

impl Gate {
    /// Loads `trusted.json` (an empty list if there's none yet).
    pub fn load() -> Arc<Gate> {
        let path = dir().map(|d| d.join("trusted.json")).unwrap_or_else(|_| "trusted.json".into());
        let mut st = GateState { devices: Vec::new(), trust_own_account: true, pin: None };
        if let Some(v) = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) {
            st.trust_own_account = v.get("trust_own_account").and_then(|x| x.as_bool()).unwrap_or(true);
            for d in v.get("devices").and_then(|x| x.as_array()).into_iter().flatten() {
                let Some(fp) = d.get("fingerprint").and_then(|x| x.as_str()).and_then(unhex) else { continue };
                st.devices.push(Device {
                    name: d.get("name").and_then(|x| x.as_str()).unwrap_or("?").to_string(),
                    fp,
                    paired_unix: d.get("paired").and_then(|x| x.as_u64()).unwrap_or(0),
                    how: d.get("how").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                });
            }
        }
        Arc::new(Gate { path, state: Mutex::new(st) })
    }

    fn save(&self, st: &GateState) {
        let devices: Vec<_> = st
            .devices
            .iter()
            .map(|d| serde_json::json!({"name": d.name, "fingerprint": hex(&d.fp), "paired": d.paired_unix, "how": d.how}))
            .collect();
        let v = serde_json::json!({"trust_own_account": st.trust_own_account, "devices": devices});
        if let Err(e) = write_atomic(&self.path, serde_json::to_string_pretty(&v).unwrap_or_default().as_bytes()) {
            eprintln!("saving {}: {e:#}", self.path.display());
        }
    }

    /// The trusted device with this fingerprint, if any.
    pub fn trusted(&self, fp: &Fingerprint) -> Option<Device> {
        self.state.lock().unwrap().devices.iter().find(|d| &d.fp == fp).cloned()
    }

    pub fn devices(&self) -> Vec<Device> {
        self.state.lock().unwrap().devices.clone()
    }

    /// Adds (or renames) a device.
    pub fn add(&self, name: &str, fp: Fingerprint, how: &str) {
        let mut st = self.state.lock().unwrap();
        st.devices.retain(|d| d.fp != fp);
        st.devices.push(Device { name: name.to_string(), fp, paired_unix: now_unix(), how: how.to_string() });
        self.save(&st);
    }

    pub fn remove(&self, fp: &Fingerprint) {
        let mut st = self.state.lock().unwrap();
        st.devices.retain(|d| &d.fp != fp);
        self.save(&st);
    }

    pub fn trust_own_account(&self) -> bool {
        self.state.lock().unwrap().trust_own_account
    }

    pub fn set_trust_own_account(&self, on: bool) {
        let mut st = self.state.lock().unwrap();
        st.trust_own_account = on;
        self.save(&st);
    }

    /// Makes a new random 6-digit pairing code, valid for [`PIN_LIFETIME`].
    pub fn start_pin(&self) -> String {
        let mut b = [0u8; 4];
        getrandom::getrandom(&mut b).expect("no system randomness");
        let code = format!("{:06}", u32::from_le_bytes(b) % 1_000_000);
        self.state.lock().unwrap().pin = Some(Pin { code: code.clone(), until: Instant::now() + PIN_LIFETIME, tries_left: PIN_TRIES });
        code
    }

    pub fn cancel_pin(&self) {
        self.state.lock().unwrap().pin = None;
    }

    /// The active code and its time left, or None (none, expired, or used up).
    pub fn pin(&self) -> Option<(String, Duration)> {
        let mut st = self.state.lock().unwrap();
        let p = st.pin.as_ref()?;
        let left = p.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            st.pin = None;
            return None;
        }
        Some((p.code.clone(), left))
    }

    /// Takes one try at the code: returns it (to run the PIN check with) if it's still valid.
    /// After the last try it's thrown away, so guessing gets [`PIN_TRIES`] chances in a million.
    pub fn try_pin(&self) -> Option<String> {
        self.pin()?;
        let mut st = self.state.lock().unwrap();
        let p = st.pin.as_mut()?;
        p.tries_left -= 1;
        let code = p.code.clone();
        if p.tries_left == 0 {
            st.pin = None;
        }
        Some(code)
    }
}

// ──────────────────────── known PCs (viewing PC) ────────────────────────

/// What the viewer knows about the PCs it connected to before.
pub enum Check {
    /// Never connected to this address: remember it once the connection is allowed.
    New,
    Same,
    /// A different PC (or a reinstalled ORBIT) answers at this address.
    Changed { expected: Fingerprint },
}

pub struct KnownPcs {
    path: PathBuf,
    map: serde_json::Map<String, serde_json::Value>,
}

impl KnownPcs {
    pub fn load() -> KnownPcs {
        let path = dir().map(|d| d.join("known_pcs.json")).unwrap_or_else(|_| "known_pcs.json".into());
        let map = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        KnownPcs { path, map }
    }

    pub fn check(&self, addr: &str, fp: &Fingerprint) -> Check {
        match self.map.get(addr).and_then(|v| v.as_str()).and_then(unhex) {
            None => Check::New,
            Some(k) if &k == fp => Check::Same,
            Some(expected) => Check::Changed { expected },
        }
    }

    pub fn remember(&mut self, addr: &str, fp: &Fingerprint) -> Result<()> {
        self.map.insert(addr.to_string(), serde_json::Value::String(hex(fp)));
        write_atomic(&self.path, serde_json::to_string_pretty(&self.map)?.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("orbit-auth-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    // ORBIT_HOME is process-wide, so all file tests run in one test.
    #[test]
    fn files_round_trip() {
        let home = temp_home("files");
        std::env::set_var("ORBIT_HOME", &home);

        let a = Identity::load_or_create().unwrap();
        let b = Identity::load_or_create().unwrap();
        assert_eq!(a.cert_der, b.cert_der, "the identity is kept, not remade");
        assert_eq!(a.key_der, b.key_der);
        let raw = std::fs::read(home.join("identity.bin")).unwrap();
        assert!(!raw.windows(a.key_der.len()).any(|w| w == a.key_der), "the key is stored encrypted");

        let g = Gate::load();
        let fp = [7u8; 32];
        assert!(g.trusted(&fp).is_none());
        g.add("LAPTOP", fp, "code");
        g.set_trust_own_account(false);
        let g2 = Gate::load();
        assert_eq!(g2.trusted(&fp).unwrap().name, "LAPTOP");
        assert!(!g2.trust_own_account());
        g2.remove(&fp);
        assert!(Gate::load().trusted(&fp).is_none());

        let mut k = KnownPcs::load();
        assert!(matches!(k.check("100.1.2.3", &fp), Check::New));
        k.remember("100.1.2.3", &fp).unwrap();
        let k = KnownPcs::load();
        assert!(matches!(k.check("100.1.2.3", &fp), Check::Same));
        assert!(matches!(k.check("100.1.2.3", &[8; 32]), Check::Changed { .. }));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn pin_runs_out_of_tries() {
        let g = Gate { path: std::env::temp_dir().join("unused.json"), state: Mutex::new(GateState { devices: vec![], trust_own_account: true, pin: None }) };
        assert!(g.try_pin().is_none(), "no code active");
        let code = g.start_pin();
        assert_eq!(code.len(), 6);
        for _ in 0..PIN_TRIES {
            assert_eq!(g.try_pin().as_deref(), Some(code.as_str()));
        }
        assert!(g.try_pin().is_none() && g.pin().is_none(), "used up");
        g.start_pin();
        g.cancel_pin();
        assert!(g.pin().is_none());
    }

    #[test]
    fn fingerprint_formats() {
        let fp = fingerprint(b"x");
        assert_eq!(unhex(&hex(&fp)), Some(fp));
        assert_eq!(short(&fp).len(), 19);
        assert!(unhex("zz").is_none());
    }
}
