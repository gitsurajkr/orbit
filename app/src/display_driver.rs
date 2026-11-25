//! Installs and removes the ORBIT Display driver (drivers/orbit-display): the virtual monitor
//! for PCs with no monitor attached. Runs as administrator (the installer, or `--display-driver`).
//!
//! Signing without a paid certificate: on install, a code-signing certificate is made on this PC
//! only, the driver's catalog is signed with it, the certificate's public part is added to this
//! PC's trusted roots and publishers, and its private key is deleted. Nothing else can ever be
//! signed with it. Uninstall removes the device, the driver package and that trust.
//!
//! `orbit.exe --display-driver install <folder> | uninstall | status | test [w h hz] [--leave]`

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::time::Duration;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Foundation::{BOOL, HWND};

const HARDWARE_ID: &str = server::vdisplay::HARDWARE_ID;
/// Subject of the per-PC signing certificate; uninstall finds it by this.
const CERT_SUBJECT: &str = "CN=ORBIT Display driver (signed on this PC only)";

pub fn cli(args: &[String]) -> i32 {
    let r = match args.first().map(String::as_str) {
        Some("install") => match args.get(1) {
            Some(dir) => install(Path::new(dir)),
            None => Err(anyhow::anyhow!("install needs the driver folder")),
        },
        Some("uninstall") => uninstall(),
        Some("status") => {
            status();
            Ok(())
        }
        Some("test") => {
            let n = |i: usize, d: u32| args.get(i).and_then(|v| v.parse().ok()).unwrap_or(d);
            test(n(1, 1920), n(2, 1080), n(3, 60), args.iter().any(|a| a == "--leave"))
        }
        _ => Err(anyhow::anyhow!("usage: --display-driver install <folder> | uninstall | status | test [w h hz] [--leave]")),
    };
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("display driver: {e:#}");
            1
        }
    }
}

/// Signs the package in `dir` for this PC, creates the device if needed and installs the driver.
pub fn install(dir: &Path) -> Result<()> {
    let dir = std::fs::canonicalize(dir).with_context(|| format!("{}", dir.display()))?;
    let dir = dir.to_string_lossy().trim_start_matches(r"\\?\").to_string();
    let inf = format!(r"{dir}\OrbitDisplay.inf");
    let cat = format!(r"{dir}\orbitdisplay.cat");
    if !Path::new(&inf).exists() || !Path::new(&cat).exists() {
        bail!("{dir} doesn't hold the ORBIT Display package (OrbitDisplay.inf + orbitdisplay.cat)");
    }
    // A fresh install every time (updates too): the old device and every old package go first, so
    // the new driver is what runs and old versions don't pile up in the driver store.
    if let Err(e) = remove_device_and_packages() {
        println!("display driver: removing the previous install: {e:#} (continuing)");
    }
    sign_for_this_pc(&cat)?;
    println!("display driver: signed for this PC");
    unsafe {
        if devices()?.is_empty() {
            create_device()?;
            println!("display driver: device created");
        }
        let mut reboot = BOOL(0);
        // Non-interactive: a signature problem fails here instead of showing a dialog that would
        // hang a silent install.
        UpdateDriverForPlugAndPlayDevicesW(
            HWND::default(),
            &HSTRING::from(HARDWARE_ID),
            &HSTRING::from(inf.as_str()),
            INSTALLFLAG_FORCE | INSTALLFLAG_NONINTERACTIVE,
            Some(&mut reboot),
        )
        .context("installing the driver")?;
        println!("display driver: installed{}", if reboot.as_bool() { " (Windows wants a restart)" } else { "" });
    }
    Ok(())
}

/// Removes the device, every ORBIT Display package in the driver store, and the signing trust.
pub fn uninstall() -> Result<()> {
    let mut problems = Vec::new();
    if let Err(e) = remove_device_and_packages() {
        problems.push(format!("{e:#}"));
    }
    if let Err(e) = powershell(&format!(
        "Get-ChildItem Cert:\\LocalMachine\\Root, Cert:\\LocalMachine\\TrustedPublisher | Where-Object Subject -eq '{CERT_SUBJECT}' | Remove-Item"
    )) {
        problems.push(format!("removing the signing certificate: {e:#}"));
    }
    if !problems.is_empty() {
        bail!("{}", problems.join("; "));
    }
    println!("display driver: uninstalled");
    Ok(())
}

/// Removes our device(s) and every driver-store package that installs them.
fn remove_device_and_packages() -> Result<()> {
    let mut problems = Vec::new();
    unsafe {
        for (set, info) in devices()? {
            if let Err(e) = DiUninstallDevice(HWND::default(), set.0, &info, 0, None) {
                problems.push(format!("removing the device: {e}"));
            }
        }
    }
    for oem in driver_store_packages() {
        let ok = unsafe { SetupUninstallOEMInfW(&HSTRING::from(oem.as_str()), SUOI_FORCEDELETE, None) };
        if ok.as_bool() {
            println!("display driver: removed {oem} from the driver store");
        } else {
            problems.push(format!("removing {oem}: {}", std::io::Error::last_os_error()));
        }
    }
    if !problems.is_empty() {
        bail!("{}", problems.join("; "));
    }
    Ok(())
}

/// Prints whether the device exists, whether Windows runs it, and what the driver reports.
pub fn status() {
    let devs = unsafe { devices() }.unwrap_or_default();
    if devs.is_empty() {
        println!("display driver: not installed");
        return;
    }
    for (_, info) in &devs {
        let (mut st, mut problem) = (CM_DEVNODE_STATUS_FLAGS(0), CM_PROB(0));
        let r = unsafe { CM_Get_DevNode_Status(&mut st, &mut problem, info.DevInst, 0) };
        let state = if r != CR_SUCCESS {
            "not present (removed or disabled?)".to_string()
        } else if problem.0 != 0 {
            format!("not running: Device Manager problem code {}{}", problem.0, if problem.0 == 52 { " (signature not trusted)" } else { "" })
        } else {
            "running".to_string()
        };
        println!("display driver: device {state}");
    }
    match server::vdisplay::Driver::open().and_then(|d| d.occupied_slots()) {
        Ok(bits) => println!("display driver: answers; monitors open: {}", bits.count_ones()),
        Err(e) => println!("display driver: {e:#}"),
    }
    println!("display driver: ORBIT Display monitors on: {:?}", server::vdisplay::active_monitors());
}

/// Adds a monitor, waits for Windows to switch it on, captures and encodes a few frames from it
/// with ORBIT's normal pipeline, then removes it. `leave`: exit without removing it (the driver's
/// lease must remove it within 5 s; check with `status`).
fn test(width: u32, height: u32, hz: u32, leave: bool) -> Result<()> {
    let driver = server::vdisplay::Driver::open()?;
    // Ask for the NVIDIA GPU, where ORBIT encodes.
    let gpu = server::capture::find_adapter(server::capture::NVIDIA_VENDOR)?
        .and_then(|(i, _)| server::capture::adapter_luid(i).ok());
    println!("test: adding {width}x{height}@{hz} (GPU {})", if gpu.is_some() { "NVIDIA" } else { "Windows' choice" });
    let monitor = driver.add(width, height, hz, 0x0B17_0001, gpu)?;
    println!("test: slot {}", monitor.slot());
    let name = monitor.wait_active(Duration::from_secs(10))?;
    let mon = server::capture::list_monitors()?
        .into_iter()
        .find(|m| m.device_name.eq_ignore_ascii_case(&name))
        .with_context(|| format!("{name} is on but not in the monitor list"))?;
    println!("test: on as {} {}x{} at ({},{}), drawn by '{}'", mon.device_name, mon.width, mon.height, mon.left, mon.top, mon.adapter_name);

    let mut pipeline = server::video::Pipeline::new(mon, 60, 10_000_000)?;
    let mut out = Vec::new();
    let (mut frames, mut keyframes, mut bytes) = (0, 0, 0);
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        out.clear();
        if let Some(e) = pipeline.next(100, frames == 0, false, &mut out)? {
            frames += 1;
            keyframes += e.keyframe as u32;
            bytes += out.len();
        }
    }
    println!("test: capture via {}: {frames} frames ({keyframes} keyframes, {} KB) in 2 s", pipeline.capture_mode(), bytes / 1024);
    if frames == 0 {
        bail!("no frames from the virtual monitor");
    }
    drop(pipeline);

    if leave {
        println!("test: exiting without removing the monitor; the lease should remove it within 5 s");
        std::process::exit(0);
    }
    monitor.close()?;
    std::thread::sleep(Duration::from_millis(1500));
    let still = server::vdisplay::active_monitors();
    if !still.is_empty() {
        bail!("removed, but Windows still shows {still:?}");
    }
    println!("test: removed; passed");
    Ok(())
}

/// Signs `cat` with a certificate made here and trusted only here, whose private key is then
/// deleted. Replaces the trust of any earlier install.
fn sign_for_this_pc(cat: &str) -> Result<()> {
    std::env::set_var("ORBIT_DISPLAY_CAT", cat);
    powershell(&format!(
        r#"
$subject = '{CERT_SUBJECT}'
Get-ChildItem Cert:\LocalMachine\Root, Cert:\LocalMachine\TrustedPublisher | Where-Object Subject -eq $subject | Remove-Item
$cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject $subject -CertStoreLocation Cert:\LocalMachine\My `
  -KeyAlgorithm RSA -KeyLength 3072 -HashAlgorithm SHA256 -KeyExportPolicy NonExportable -NotAfter (Get-Date).AddYears(25)
try {{
  $signed = Set-AuthenticodeSignature -FilePath $env:ORBIT_DISPLAY_CAT -Certificate $cert -HashAlgorithm SHA256
  if (-not $signed.SignerCertificate) {{ throw "signing the catalog failed: $($signed.StatusMessage)" }}
  # Trust only the public part, in the machine's root and publisher stores.
  $public = [System.Security.Cryptography.X509Certificates.X509Certificate2]::new($cert.RawData)
  foreach ($name in 'Root', 'TrustedPublisher') {{
    $store = [System.Security.Cryptography.X509Certificates.X509Store]::new($name, 'LocalMachine')
    $store.Open('ReadWrite'); $store.Add($public); $store.Close()
  }}
}} finally {{
  # The private key never outlives this script.
  Remove-Item "Cert:\LocalMachine\My\$($cert.Thumbprint)" -DeleteKey
}}
$check = Get-AuthenticodeSignature -FilePath $env:ORBIT_DISPLAY_CAT
if ($check.Status -ne 'Valid') {{ throw "the catalog's signature isn't trusted: $($check.StatusMessage)" }}
"#
    ))
}

fn powershell(script: &str) -> Result<()> {
    let ps = std::env::var("SystemRoot").map(|r| format!(r"{r}\System32\WindowsPowerShell\v1.0\powershell.exe")).unwrap_or_else(|_| "powershell.exe".into());
    let out = std::process::Command::new(ps)
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &format!("$ErrorActionPreference = 'Stop'\n{script}")])
        .stdin(std::process::Stdio::null())
        .output()
        .context("running PowerShell")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// A device-info set, destroyed when dropped.
struct DevSet(HDEVINFO);
impl Drop for DevSet {
    fn drop(&mut self) {
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

/// Our devices (present or not), each with the set it came from.
unsafe fn devices() -> Result<Vec<(std::rc::Rc<DevSet>, SP_DEVINFO_DATA)>> {
    let set = std::rc::Rc::new(DevSet(SetupDiGetClassDevsW(Some(&GUID_DEVCLASS_DISPLAY), PCWSTR::null(), HWND::default(), SETUP_DI_GET_CLASS_DEVS_FLAGS(0))?));
    let mut ours = Vec::new();
    for i in 0.. {
        let mut info = SP_DEVINFO_DATA { cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
        if SetupDiEnumDeviceInfo(set.0, i, &mut info).is_err() {
            break;
        }
        let mut buf = [0u8; 512];
        if SetupDiGetDeviceRegistryPropertyW(set.0, &info, SPDRP_HARDWAREID, None, Some(&mut buf), None).is_err() {
            continue;
        }
        let ids: Vec<u16> = buf.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        if String::from_utf16_lossy(&ids).split('\0').any(|id| id.eq_ignore_ascii_case(HARDWARE_ID)) {
            ours.push((set.clone(), info));
        }
    }
    Ok(ours)
}

/// Creates the root-enumerated device `Root\OrbitDisplay` (a virtual adapter has no bus to find
/// it on, so it's made by hand like this).
unsafe fn create_device() -> Result<()> {
    let set = DevSet(SetupDiCreateDeviceInfoList(Some(&GUID_DEVCLASS_DISPLAY), HWND::default())?);
    let mut info = SP_DEVINFO_DATA { cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
    SetupDiCreateDeviceInfoW(set.0, &HSTRING::from("Display"), &GUID_DEVCLASS_DISPLAY, &HSTRING::from("ORBIT Virtual Display Adapter"), HWND::default(), DICD_GENERATE_ID, Some(&mut info))
        .context("creating the device")?;
    // REG_MULTI_SZ: the id, then an empty string.
    let mut id: Vec<u8> = HARDWARE_ID.encode_utf16().chain([0, 0]).flat_map(u16::to_le_bytes).collect();
    id.shrink_to_fit();
    SetupDiSetDeviceRegistryPropertyW(set.0, &mut info, SPDRP_HARDWAREID, Some(&id)).context("setting the device's hardware id")?;
    SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set.0, Some(&info)).context("registering the device")?;
    Ok(())
}

/// `oemNN.inf` names in the driver store that install `Root\OrbitDisplay`.
fn driver_store_packages() -> Vec<String> {
    let dir = std::env::var("SystemRoot").map(|r| format!(r"{r}\INF")).unwrap_or_else(|_| r"C:\Windows\INF".into());
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let lower = name.to_ascii_lowercase();
            if !(lower.starts_with("oem") && lower.ends_with(".inf")) {
                return None;
            }
            // INFs are UTF-16 or ANSI; look for the hardware id in either.
            let bytes = std::fs::read(e.path()).ok()?;
            let text = if bytes.starts_with(&[0xFF, 0xFE]) {
                String::from_utf16_lossy(&bytes[2..].chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect::<Vec<_>>())
            } else {
                String::from_utf8_lossy(&bytes).into_owned()
            };
            text.to_ascii_lowercase().contains(&HARDWARE_ID.to_ascii_lowercase()).then_some(name)
        })
        .collect()
}
