//! Client for the ORBIT Display driver (drivers/orbit-display): virtual monitors on demand, for a
//! PC with no monitor attached.
//!
//! [`Driver::open`] connects to the installed driver and keeps this process's lease alive (a
//! KEEPALIVE every second on a background thread). [`Driver::add`] adds a monitor and returns a
//! [`VirtualMonitor`]; dropping it removes the monitor. If this process dies, the driver removes
//! its monitors by itself once the lease runs out (5 s).
//!
//! The request/reply layouts mirror `drivers/orbit-display/protocol.h` byte for byte.

use anyhow::{bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, LUID};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::IO::DeviceIoControl;

/// The driver's control interface (protocol.h: kOrbitDisplayInterface).
pub const INTERFACE: GUID = GUID::from_u128(0xe5a1f0c2_6b7d_4e38_a2c4_91d3b7f05e6a);
/// The device ORBIT creates for the driver (OrbitDisplay.inf).
pub const HARDWARE_ID: &str = r"Root\OrbitDisplay";
/// The monitors' name in Windows (edid.h: kMonitorName).
pub const MONITOR_NAME: &str = "ORBIT Display";

const MAGIC: u32 = 0x4442_524F; // "ORBD"
const VERSION: u16 = 1;
// CTL_CODE(FILE_DEVICE_UNKNOWN, 0xB01, METHOD_BUFFERED, FILE_READ_ACCESS | FILE_WRITE_ACCESS)
const IOCTL_ORBIT_DISPLAY: u32 = (0x22 << 16) | (3 << 14) | (0xB01 << 2);

const OP_OPEN: u16 = 1;
const OP_CLOSE: u16 = 2;
const OP_KEEPALIVE: u16 = 3;
const OP_STATUS: u16 = 4;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Request {
    magic: u32,
    version: u16,
    op: u16,
    slot: u32,
    width: u32,
    height: u32,
    hz: u32,
    serial: u32,
    gpu_low: u32,
    gpu_high: i32,
    session_low: u32,
    session_high: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct Reply {
    magic: u32,
    status: i32,
    step: u32,
    slot: u32,
    gpu_set: u32,
    occupied_slots: u32,
}

const _: () = assert!(std::mem::size_of::<Request>() == 44 && std::mem::size_of::<Reply>() == 24);

/// Where a failed OPEN/CLOSE stopped (protocol.h: OrbitDisplayStep).
fn step_name(step: u32) -> &'static str {
    match step {
        1 => "request rejected (driver/ORBIT version mismatch?)",
        2 => "the virtual adapter isn't started",
        3 => "no free monitor slot",
        4 => "creating the monitor",
        5 => "announcing the monitor to Windows",
        6 => "no such monitor",
        7 => "internal driver error",
        _ => "unknown step",
    }
}

struct Handle(HANDLE);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct Inner {
    handle: Handle,
    session: u64,
    stop: AtomicBool,
}

impl Inner {
    fn call(&self, mut req: Request) -> Result<Reply> {
        req.magic = MAGIC;
        req.version = VERSION;
        req.session_low = self.session as u32;
        req.session_high = (self.session >> 32) as u32;
        let mut reply = Reply::default();
        let mut got = 0u32;
        unsafe {
            DeviceIoControl(
                self.handle.0,
                IOCTL_ORBIT_DISPLAY,
                Some(&req as *const Request as *const _),
                std::mem::size_of::<Request>() as u32,
                Some(&mut reply as *mut Reply as *mut _),
                std::mem::size_of::<Reply>() as u32,
                Some(&mut got),
                None,
            )
            .context("talking to the ORBIT Display driver")?;
        }
        if got as usize != std::mem::size_of::<Reply>() || reply.magic != MAGIC {
            bail!("the ORBIT Display driver sent an unexpected reply ({got} bytes)");
        }
        Ok(reply)
    }
}

/// A connection to the driver. Clones share it; the lease stops when the last one is dropped.
#[derive(Clone)]
pub struct Driver {
    inner: Arc<Inner>,
}

impl Driver {
    /// True if the driver is installed and running (its control interface exists).
    pub fn available() -> bool {
        interface_path().is_some()
    }

    pub fn open() -> Result<Driver> {
        let path = interface_path().context("the ORBIT Display driver isn't installed or isn't running")?;
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .context("opening the ORBIT Display driver (needs administrator or SYSTEM)")?;
        let inner = Arc::new(Inner { handle: Handle(handle), session: random_session(), stop: AtomicBool::new(false) });
        // The lease: renewed every second, well inside the driver's 5 s.
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new().name("vdisplay-lease".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_secs(1));
            let Some(inner) = weak.upgrade() else { return };
            if inner.stop.load(Ordering::Acquire) {
                return;
            }
            let _ = inner.call(Request { op: OP_KEEPALIVE, ..Default::default() });
        })?;
        Ok(Driver { inner })
    }

    /// Adds a monitor whose native mode is `width` x `height` @ `hz`, drawn by the GPU `gpu` if
    /// given. The same `serial` is the same monitor to Windows (it keeps its settings).
    pub fn add(&self, width: u32, height: u32, hz: u32, serial: u32, gpu: Option<LUID>) -> Result<VirtualMonitor> {
        let before = orbit_targets();
        let gpu = gpu.unwrap_or_default();
        let reply = self.inner.call(Request {
            op: OP_OPEN,
            width,
            height,
            hz,
            serial,
            gpu_low: gpu.LowPart,
            gpu_high: gpu.HighPart,
            ..Default::default()
        })?;
        if reply.status != 0 {
            bail!("the ORBIT Display driver couldn't add a {width}x{height}@{hz} monitor: {} failed (0x{:08X})", step_name(reply.step), reply.status as u32);
        }
        Ok(VirtualMonitor { driver: self.clone(), slot: reply.slot, before, closed: false })
    }

    /// Bit i set = slot i holds a monitor (any owner).
    pub fn occupied_slots(&self) -> Result<u32> {
        Ok(self.inner.call(Request { op: OP_STATUS, ..Default::default() })?.occupied_slots)
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// A virtual monitor; removed when dropped.
pub struct VirtualMonitor {
    driver: Driver,
    slot: u32,
    /// ORBIT Display targets that existed before this one was added.
    before: Vec<TargetKey>,
    closed: bool,
}

impl VirtualMonitor {
    pub fn slot(&self) -> u32 {
        self.slot
    }

    /// Waits until Windows shows the monitor and returns its GDI name (`\\.\DISPLAYn`). If Windows
    /// adds it but leaves it off (a saved "show only on 1" layout), the desktop is switched to
    /// Extend once, since a monitor was asked for.
    pub fn wait_active(&self, timeout: Duration) -> Result<String> {
        let start = Instant::now();
        let mut extended = false;
        loop {
            let new: Vec<Target> = orbit_targets_full().into_iter().filter(|t| !self.before.contains(&t.key)).collect();
            if let Some(name) = new.iter().find_map(|t| t.gdi_name.clone()) {
                return Ok(name);
            }
            if !extended && !new.is_empty() && start.elapsed() > Duration::from_secs(2) {
                extended = true;
                let rc = unsafe { SetDisplayConfig(None, None, SDC_APPLY | SDC_TOPOLOGY_EXTEND) };
                println!("virtual display: Windows left the new monitor off; switched to Extend (result {rc})");
            }
            if start.elapsed() > timeout {
                bail!(
                    "the virtual monitor didn't come on within {} s ({})",
                    timeout.as_secs(),
                    if new.is_empty() { "Windows doesn't list it" } else { "Windows lists it but left it off" }
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Removes the monitor now (dropping does the same, ignoring errors).
    pub fn close(mut self) -> Result<()> {
        self.closed = true;
        let reply = self.driver.inner.call(Request { op: OP_CLOSE, slot: self.slot, ..Default::default() })?;
        if reply.status != 0 {
            bail!("removing the virtual monitor: {} (0x{:08X})", step_name(reply.step), reply.status as u32);
        }
        Ok(())
    }
}

impl Drop for VirtualMonitor {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.driver.inner.call(Request { op: OP_CLOSE, slot: self.slot, ..Default::default() });
        }
    }
}

fn random_session() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    h.write_u64(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64));
    h.finish() | 1
}

/// `\\?\...` path of the driver's control interface, if the driver is running.
fn interface_path() -> Option<Vec<u16>> {
    unsafe {
        let set = SetupDiGetClassDevsW(Some(&INTERFACE), PCWSTR::null(), None, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE).ok()?;
        let mut data = SP_DEVICE_INTERFACE_DATA { cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32, ..Default::default() };
        let mut path = None;
        if SetupDiEnumDeviceInterfaces(set, None, &INTERFACE, 0, &mut data).is_ok() {
            let mut needed = 0u32;
            let _ = SetupDiGetDeviceInterfaceDetailW(set, &data, None, 0, Some(&mut needed), None);
            // The detail struct is a u32 size followed by the path; allocate as u32s for alignment.
            let mut buf = vec![0u32; (needed as usize).div_ceil(4) + 1];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &data, Some(detail), needed, None, None).is_ok() {
                let start = std::ptr::addr_of!((*detail).DevicePath) as *const u16;
                let len = (0..).take_while(|&i| *start.add(i) != 0).count();
                let mut p = std::slice::from_raw_parts(start, len).to_vec();
                p.push(0);
                path = Some(p);
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
        path
    }
}

/// A display target Windows knows: adapter LUID + target id.
type TargetKey = (u32, i32, u32);

struct Target {
    key: TargetKey,
    /// `\\.\DISPLAYn` while it's part of the desktop.
    gdi_name: Option<String>,
}

fn orbit_targets() -> Vec<TargetKey> {
    orbit_targets_full().into_iter().map(|t| t.key).collect()
}

fn wide(buf: &[u16]) -> String {
    String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())])
}

/// Every "ORBIT Display" target Windows knows, on or off.
fn orbit_targets_full() -> Vec<Target> {
    let mut found: Vec<Target> = Vec::new();
    unsafe {
        let (mut paths_n, mut modes_n) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut paths_n, &mut modes_n).is_err() {
            return found;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); paths_n as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); modes_n as usize];
        if QueryDisplayConfig(QDC_ALL_PATHS, &mut paths_n, paths.as_mut_ptr(), &mut modes_n, modes.as_mut_ptr(), None).is_err() {
            return found;
        }
        for p in &paths[..paths_n as usize] {
            let t = p.targetInfo;
            let key = (t.adapterId.LowPart, t.adapterId.HighPart, t.id);
            let active = p.flags & windows::Win32::Graphics::Gdi::DISPLAYCONFIG_PATH_ACTIVE != 0;
            let known = found.iter().position(|f| f.key == key);
            if known.is_none() {
                let mut name = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
                name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
                name.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
                name.header.adapterId = t.adapterId;
                name.header.id = t.id;
                if DisplayConfigGetDeviceInfo(&mut name.header) != 0 || wide(&name.monitorFriendlyDeviceName) != MONITOR_NAME {
                    continue;
                }
                found.push(Target { key, gdi_name: None });
            }
            let entry = found.iter_mut().find(|f| f.key == key).expect("just added");
            if active && entry.gdi_name.is_none() {
                let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
                source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
                source.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
                source.header.adapterId = p.sourceInfo.adapterId;
                source.header.id = p.sourceInfo.id;
                if DisplayConfigGetDeviceInfo(&mut source.header) == 0 {
                    entry.gdi_name = Some(wide(&source.viewGdiDeviceName));
                }
            }
        }
    }
    found
}

/// GDI names of the ORBIT Display monitors that are on right now.
pub fn active_monitors() -> Vec<String> {
    orbit_targets_full().into_iter().filter_map(|t| t.gdi_name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_code_matches_ctl_code() {
        // CTL_CODE(0x22, 0xB01, METHOD_BUFFERED=0, FILE_READ_ACCESS|FILE_WRITE_ACCESS=3)
        assert_eq!(IOCTL_ORBIT_DISPLAY, 0x0022_EC04);
    }
}
