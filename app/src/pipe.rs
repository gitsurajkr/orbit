//! The private line between the ORBIT app (running as the user) and the ORBIT Agent (running
//! as SYSTEM): a local named pipe, `\\.\pipe\orbit-agent`.
//!
//! One JSON request per connection, one JSON reply:
//! - `{"cmd":"status"}` → who's connected, stats, paired devices, the pairing code, settings
//! - `{"cmd":"remove","fp":"<hex>"}`, `{"cmd":"trust_own","on":true}`
//! - `{"cmd":"pin_start"}`, `{"cmd":"pin_cancel"}`
//! - `{"cmd":"settings","fps":60,"mbps":20,"monitor":0}` (the agent restarts its sharing)
//!
//! Only SYSTEM, administrators and the user signed in on that session may open it (its
//! security descriptor), and remote clients are refused. Whoever can use it can pair devices,
//! so this matters as much as the device list itself.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

pub const NAME: &str = r"\\.\pipe\orbit-agent";

/// App side: sends one request and returns the reply.
pub fn call(req: &Value) -> Result<Value> {
    let mut f = None;
    for _ in 0..10 {
        match std::fs::OpenOptions::new().read(true).write(true).open(NAME) {
            Ok(x) => {
                f = Some(x);
                break;
            }
            // All instances busy for a moment (another request): retry briefly.
            Err(e) if e.raw_os_error() == Some(231) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e).context("the ORBIT service isn't answering"),
        }
    }
    let mut f = f.context("the ORBIT service is busy")?;
    writeln!(f, "{req}")?;
    let mut line = String::new();
    BufReader::new(f).read_line(&mut line)?;
    let v: Value = serde_json::from_str(&line).context("bad reply from the ORBIT service")?;
    if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
        bail!("{e}");
    }
    Ok(v)
}

/// Agent side: answers requests with `handle` until the process ends. `user_sid` gives the SID
/// (string form) of the user signed in on this session, if any, re-read for each connection.
pub fn serve(handle: impl Fn(&Value) -> Value, user_sid: impl Fn() -> Option<String>) {
    use std::os::windows::io::FromRawHandle;
    use windows::core::HSTRING;
    use windows::Win32::Foundation::*;
    use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::{FlushFileBuffers, PIPE_ACCESS_DUPLEX};
    use windows::Win32::System::Pipes::*;
    loop {
        // SYSTEM and administrators: full access; the signed-in user: read + write.
        let sddl = match user_sid() {
            Some(sid) => format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;{sid})"),
            None => "D:P(A;;GA;;;SY)(A;;GA;;;BA)".to_string(),
        };
        let h = unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            if let Err(e) = ConvertStringSecurityDescriptorToSecurityDescriptorW(&HSTRING::from(sddl.as_str()), SDDL_REVISION_1, &mut sd, None) {
                println!("pipe: bad security descriptor {sddl}: {e}");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
            let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: false.into() };
            let h = CreateNamedPipeW(
                &HSTRING::from(NAME),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                Some(&sa),
            );
            let _ = LocalFree(HLOCAL(sd.0));
            h
        };
        if h.is_invalid() {
            println!("pipe: CreateNamedPipe failed: {}", windows::core::Error::from_win32());
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        let connected = unsafe { ConnectNamedPipe(h, None) };
        if connected.is_err() && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
            unsafe {
                let _ = CloseHandle(h);
            }
            continue;
        }
        let file = unsafe { std::fs::File::from_raw_handle(h.0) };
        let mut reader = BufReader::new(&file);
        let mut line = String::new();
        let reply = match reader.read_line(&mut line) {
            Ok(_) => match serde_json::from_str::<Value>(&line) {
                Ok(req) => handle(&req),
                Err(e) => json!({ "error": format!("bad request: {e}") }),
            },
            Err(e) => json!({ "error": format!("{e}") }),
        };
        let mut w = &file;
        let _ = writeln!(w, "{reply}");
        unsafe {
            let _ = FlushFileBuffers(h);
            let _ = DisconnectNamedPipe(h);
        }
        drop(file); // closes the handle
    }
}

/// SID (string form) of the user signed in on `session`, if anyone is.
pub fn session_user_sid(session: u32) -> Option<String> {
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_USER};
    use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
    unsafe {
        let mut tok = HANDLE::default();
        WTSQueryUserToken(session, &mut tok).ok()?;
        let mut buf = [0u8; 256];
        let mut len = 0u32;
        let ok = GetTokenInformation(tok, TokenUser, Some(buf.as_mut_ptr() as *mut _), buf.len() as u32, &mut len).is_ok();
        let _ = CloseHandle(tok);
        if !ok {
            return None;
        }
        let sid = (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid;
        let mut s = windows::core::PWSTR::null();
        ConvertSidToStringSidW(sid, &mut s).ok()?;
        let out = s.to_string().ok();
        let _ = LocalFree(HLOCAL(s.0 as _));
        out
    }
}
