//! Everything ORBIT prints also goes to a log file, one timestamped line at a time, so a problem
//! can be diagnosed afterwards from the file alone.
//!
//! How: stdout and stderr are pointed at two pipes. A thread per pipe stamps each line with the
//! local time, appends it to `<name>.log`, and passes it on unchanged to the original stdout /
//! stderr (a console when run from one; the launcher reads the viewer's stderr for its error
//! message). Rust's `println!`/`eprintln!` look the handle up on every write, so this catches
//! every line from every crate without changing them.
//!
//! At [`MAX_BYTES`] the file becomes `<name>.old.log` and a new one starts: at most 2 x 10 MB per
//! log. Files:
//! - `%LOCALAPPDATA%\ORBIT\logs\`: `orbit.log` (the launcher, incl. sharing without the
//!   service), `viewer.log` (the viewer window), `setup.log` (installing the service).
//! - `C:\ProgramData\ORBIT\logs\`: `service.log`, `agent.log` (the always-on service).
//!
//! Tray > "Open logs folder" copies the service's logs next to the user's and opens the folder.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::WriteFile;
use windows::Win32::System::Console::{GetStdHandle, SetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_OUTPUT_HANDLE};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::SystemInformation::GetLocalTime;

/// Size at which a log is rotated.
const MAX_BYTES: u64 = 10 << 20;

struct Logger {
    path: PathBuf,
    file: Mutex<Option<File>>,
    /// (which std handle, the original one, our pipe's write end), to undo in [`finish`].
    redirects: Mutex<Vec<(STD_HANDLE, isize, isize)>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

static LOG: OnceLock<Logger> = OnceLock::new();

/// The user's log folder: `%LOCALAPPDATA%\ORBIT\logs`.
pub fn user_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("APPDATA")).map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("ORBIT").join("logs")
}

/// Starts logging this process to `dir\<name>.log`. `what` describes the process for the
/// first line. Call once, as early as possible; later calls do nothing.
pub fn start(dir: &Path, name: &str, what: &str) {
    if LOG.get().is_some() {
        return;
    }
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(format!("{name}.log"));
    let file = open(&path);
    if file.is_none() {
        return; // can't write there: carry on without a log
    }
    let log = LOG.get_or_init(|| Logger { path, file: Mutex::new(file), redirects: Mutex::default(), threads: Mutex::default() });
    log.line("");
    log.line(&format!(
        "==== ORBIT v{} | {what} | pid {} | Windows {} | args: {}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        windows_version(),
        std::env::args().skip(1).collect::<Vec<_>>().join(" ")
    ));
    for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        redirect(log, which);
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Written straight to the file: the process may end before a pipe thread runs.
        if let Some(log) = LOG.get() {
            log.line(&format!("PANIC on thread '{}': {info}", std::thread::current().name().unwrap_or("?")));
        }
        previous(info);
    }));
}

/// Puts stdout/stderr back and waits (briefly) until every line printed so far is in the file.
/// Call before the process exits, e.g. before `std::process::exit`.
pub fn finish() {
    let Some(log) = LOG.get() else { return };
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    for (which, original, write) in log.redirects.lock().unwrap().drain(..) {
        unsafe {
            let _ = SetStdHandle(which, HANDLE(original as *mut _));
            let _ = CloseHandle(HANDLE(write as *mut _));
        }
    }
    // The threads end when their pipe has no writer left (a child process that inherited the
    // handle could keep it open, hence the time limit).
    let deadline = Instant::now() + Duration::from_millis(500);
    for t in log.threads.lock().unwrap().drain(..) {
        while !t.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// [`finish`], then exits with `code`.
pub fn exit(code: i32) -> ! {
    finish();
    std::process::exit(code)
}

fn open(path: &Path) -> Option<File> {
    OpenOptions::new().create(true).append(true).open(path).ok()
}

impl Logger {
    /// Appends one timestamped line, rotating the file when it's full.
    fn line(&self, text: &str) {
        let mut f = self.file.lock().unwrap();
        if f.as_ref().and_then(|f| f.metadata().ok()).is_some_and(|m| m.len() >= MAX_BYTES) {
            *f = None; // close before renaming
            let old = self.path.with_extension("old.log");
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&self.path, &old);
            *f = open(&self.path);
        }
        if let Some(f) = f.as_mut() {
            // One write per line: nothing is held in a buffer that a crash could lose.
            let _ = f.write_all(format!("{} {text}\r\n", timestamp()).as_bytes());
        }
    }
}

/// Points `which` at a new pipe and starts the thread that logs and forwards what comes out.
fn redirect(log: &'static Logger, which: STD_HANDLE) {
    unsafe {
        let original = GetStdHandle(which).unwrap_or_default();
        let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
        if CreatePipe(&mut read, &mut write, None, 0).is_err() {
            return;
        }
        if SetStdHandle(which, write).is_err() {
            let _ = CloseHandle(read);
            let _ = CloseHandle(write);
            return;
        }
        log.redirects.lock().unwrap().push((which, original.0 as isize, write.0 as isize));
        let (read, original) = (read.0 as isize, original.0 as isize);
        let t = std::thread::Builder::new().name("log".into()).spawn(move || {
            let reader = BufReader::new(File::from_raw_handle(read as *mut _));
            for line in reader.split(b'\n') {
                let Ok(mut line) = line else { break };
                if original != 0 && original != -1 {
                    line.push(b'\n');
                    let _ = WriteFile(HANDLE(original as *mut _), Some(&line), None, None);
                    line.pop();
                }
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                log.line(&String::from_utf8_lossy(&line));
            }
        });
        if let Ok(t) = t {
            log.threads.lock().unwrap().push(t);
        }
    }
}

/// Local time as `2026-11-25 14:03:22.123`.
fn timestamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds)
}

/// e.g. "build 26300.7171" (the real build, not the compatibility version GetVersionEx reports).
fn windows_version() -> String {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ};
    let key = w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    unsafe {
        let mut buf = [0u16; 32];
        let mut len = (buf.len() * 2) as u32;
        let build = (RegGetValueW(HKEY_LOCAL_MACHINE, key, w!("CurrentBuild"), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut len)).is_ok())
            .then(|| String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]));
        let mut ubr = 0u32;
        let mut len = 4u32;
        let ubr = RegGetValueW(HKEY_LOCAL_MACHINE, key, w!("UBR"), RRF_RT_REG_DWORD, None, Some(&mut ubr as *mut u32 as *mut _), Some(&mut len)).is_ok().then_some(ubr);
        match (build, ubr) {
            (Some(b), Some(u)) => format!("build {b}.{u}"),
            (Some(b), None) => format!("build {b}"),
            _ => "?".into(),
        }
    }
}

/// Copies the service's logs (if any) into the user's log folder and opens that folder.
pub fn open_folder() {
    let dir = user_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(entries) = std::fs::read_dir(crate::service::log_dir()) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "log") {
                let _ = std::fs::copy(&p, dir.join(format!("service-{}", e.file_name().to_string_lossy())));
            }
        }
    }
    crate::ui::open_url(&dir.to_string_lossy());
}

#[cfg(test)]
mod tests {
    #[test]
    fn windows_version_is_read() {
        let v = super::windows_version();
        assert!(v.starts_with("build ") && v.contains('.'), "{v}");
    }

    #[test]
    fn timestamp_shape() {
        let t = super::timestamp();
        assert_eq!(t.len(), 23, "{t}");
        assert_eq!(&t[4..5], "-");
        assert_eq!(&t[10..11], " ");
        assert_eq!(&t[19..20], ".");
    }
}
