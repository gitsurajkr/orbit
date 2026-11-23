//! Files and folders between the PCs, streamed straight from disk to disk.
//!
//! One one-way stream per batch:
//! ```text
//! u8  kind = FILES     u8  purpose (DROP / CLIPBOARD)
//! u64 total bytes      u32 entry count
//! entries: u8 is_dir | u16 path length | path (UTF-8, '/'-separated, relative)
//!          | files only: u64 size | the bytes
//! ```
//! The receiver saves into `Downloads\ORBIT`:
//! - each file as `name.part` first, renamed when complete (a broken transfer deletes it), so a
//!   half-received file never looks finished;
//! - a top-level name that already exists becomes `name (1).ext`, `name (2).ext`, ...;
//! - paths are cleaned: no `..`, drive letters, `\`, reserved names (CON, NUL, ...), so nothing
//!   can be written outside that folder.

use anyhow::{bail, ensure, Context, Result};
use quinn::{Connection, RecvStream};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// What the receiver does when a batch is complete.
pub mod purpose {
    /// Dropped on the viewer window: show it in Explorer.
    pub const DROP: u8 = 0;
    /// Copied with Ctrl+C: put it on the clipboard, so Ctrl+V pastes it.
    pub const CLIPBOARD: u8 = 1;
}

/// Copied files bigger than this aren't sent eagerly (drag them onto the viewer instead).
pub const CLIPBOARD_LIMIT: u64 = 4 << 30;
const MAX_ENTRIES: usize = 200_000;
const CHUNK: usize = 64 * 1024;
/// Video waiting in the datagram queue beyond this makes a file transfer pause.
const VIDEO_BACKLOG: usize = 16 * 1024;

/// Transfers in progress, for the stats lines.
#[derive(Default)]
pub struct Progress {
    inner: Mutex<Vec<Transfer>>,
    next_id: std::sync::atomic::AtomicU64,
}

struct Transfer {
    id: u64,
    sending: bool,
    name: String,
    done: u64,
    total: u64,
    started: Instant,
}

impl Progress {
    fn begin(&self, sending: bool, name: String, total: u64) -> u64 {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.lock().unwrap().push(Transfer { id, sending, name, done: 0, total, started: Instant::now() });
        id
    }
    fn with(&self, id: u64, f: impl FnOnce(&mut Transfer)) {
        if let Some(t) = self.inner.lock().unwrap().iter_mut().find(|t| t.id == id) {
            f(t);
        }
    }
    fn add(&self, id: u64, n: u64) {
        self.with(id, |t| t.done += n);
    }
    fn end(&self, id: u64) {
        self.inner.lock().unwrap().retain(|t| t.id != id);
    }

    /// `"SENDING clip.mp4 45% · 12.3 MB/s"` (plus `" (+1 MORE)"` if several run), or None.
    pub fn text(&self) -> Option<String> {
        let g = self.inner.lock().unwrap();
        let t = g.first()?;
        let pct = if t.total == 0 { 100 } else { t.done * 100 / t.total };
        let secs = t.started.elapsed().as_secs_f64().max(0.001);
        let more = if g.len() > 1 { format!(" (+{} MORE)", g.len() - 1) } else { String::new() };
        Some(format!("{} {} {pct}% · {:.1} MB/s{more}", if t.sending { "SENDING" } else { "RECEIVING" }, t.name, t.done as f64 / secs / 1e6))
    }
}

/// Paces a transfer so it only uses bandwidth the video leaves free ("scavenger" traffic):
/// starts slow, speeds up 25% every 250 ms while the link is clean, holds on a little loss
/// (Wi-Fi drops the odd packet), and halves on real congestion: over 2% of packets lost, or
/// video waiting in the datagram queue.
struct Pacer {
    rate: f64, // bytes per second
    max: f64,
    next: Instant,
    checked: Instant,
    lost: u64,
    sent: u64,
    max_free: usize,
}

impl Pacer {
    const START: f64 = 2e6;
    const MIN: f64 = 256e3;

    fn new(conn: &Connection) -> Pacer {
        let max = rate_cap().map_or(f64::INFINITY, |m| m * 1e6);
        let st = conn.stats();
        Pacer { rate: Self::START.min(max), max, next: Instant::now(), checked: Instant::now(), lost: st.path.lost_packets, sent: st.path.sent_packets, max_free: conn.datagram_send_buffer_space() }
    }

    /// Call before sending `n` bytes: waits as long as the current rate requires.
    async fn wait(&mut self, conn: &Connection, n: usize) {
        let now = Instant::now();
        let free = conn.datagram_send_buffer_space();
        self.max_free = self.max_free.max(free);
        let video_waiting = free + VIDEO_BACKLOG < self.max_free;
        if video_waiting || now - self.checked >= std::time::Duration::from_millis(250) {
            let st = conn.stats();
            let (lost, sent) = (st.path.lost_packets - self.lost, st.path.sent_packets - self.sent);
            if video_waiting || lost * 50 > sent.max(1) {
                self.rate = (self.rate * 0.5).max(Self::MIN);
            } else if lost == 0 {
                self.rate = (self.rate * 1.25).min(self.max);
            }
            (self.lost, self.sent) = (st.path.lost_packets, st.path.sent_packets);
            self.checked = now;
        }
        // No bursts after an idle moment: the schedule never lags more than 20 ms behind.
        self.next = self.next.max(now - std::time::Duration::from_millis(20)) + std::time::Duration::from_secs_f64(n as f64 / self.rate);
        if self.next > now {
            tokio::time::sleep_until(self.next.into()).await;
        }
    }
}

/// Optional speed cap for sending, in MB/s (`ORBIT_FILE_RATE`; unset = as fast as the link).
fn rate_cap() -> Option<f64> {
    std::env::var("ORBIT_FILE_RATE").ok()?.parse().ok().filter(|r: &f64| *r > 0.0)
}

struct Entry {
    rel: String,
    src: PathBuf,
    is_dir: bool,
    size: u64,
}

/// Lists everything under `paths` (folders recursively) as relative entries.
fn collect(paths: &[PathBuf]) -> Result<Vec<Entry>> {
    fn walk(src: &Path, rel: String, out: &mut Vec<Entry>) -> Result<()> {
        ensure!(out.len() < MAX_ENTRIES, "too many files");
        let meta = std::fs::metadata(src).with_context(|| format!("{}", src.display()))?;
        if meta.is_dir() {
            out.push(Entry { rel: rel.clone(), src: src.to_path_buf(), is_dir: true, size: 0 });
            let mut kids: Vec<_> = std::fs::read_dir(src)?.filter_map(|e| e.ok()).collect();
            kids.sort_by_key(|e| e.file_name());
            for k in kids {
                let name = k.file_name().to_string_lossy().into_owned();
                walk(&k.path(), format!("{rel}/{name}"), out)?;
            }
        } else {
            out.push(Entry { rel, src: src.to_path_buf(), is_dir: false, size: meta.len() });
        }
        Ok(())
    }
    let mut out = Vec::new();
    for p in paths {
        let name = p.file_name().context("can't send a drive root")?.to_string_lossy().into_owned();
        walk(p, name, &mut out)?;
    }
    Ok(out)
}

/// "clip.mp4" or "clip.mp4 +3 MORE" for a batch.
fn batch_name(tops: &[String]) -> String {
    match tops.len() {
        0 => "files".into(),
        1 => tops[0].clone(),
        n => format!("{} +{} MORE", tops[0], n - 1),
    }
}

/// Total size of the files under `paths`.
pub fn total_size(paths: &[PathBuf]) -> Result<u64> {
    Ok(collect(paths)?.iter().map(|e| e.size).sum())
}

/// Sends files and folders to the other PC.
pub async fn send(conn: &Connection, paths: Vec<PathBuf>, purpose: u8, progress: &Progress) -> Result<()> {
    let entries = tokio::task::spawn_blocking(move || collect(&paths)).await??;
    let total: u64 = entries.iter().map(|e| e.size).sum();
    let tops: Vec<String> = entries.iter().filter(|e| !e.rel.contains('/')).map(|e| e.rel.clone()).collect();
    let id = progress.begin(true, batch_name(&tops), total);
    let mut pacer = Pacer::new(conn);
    let result = async {
        let mut s = conn.open_uni().await?;
        s.set_priority(-2)?; // after the control stream and clipboard items
        let mut head = vec![crate::kind::FILES, purpose];
        head.extend_from_slice(&total.to_le_bytes());
        head.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        s.write_all(&head).await?;
        // Video comes first: QUIC puts datagrams ahead of stream data in each packet, but a big
        // transfer could still fill the congestion window so video waits for room. The pacer
        // keeps the file to what's left over.
        for e in &entries {
            let mut h = vec![e.is_dir as u8];
            h.extend_from_slice(&(e.rel.len() as u16).to_le_bytes());
            h.extend_from_slice(e.rel.as_bytes());
            if !e.is_dir {
                h.extend_from_slice(&e.size.to_le_bytes());
            }
            s.write_all(&h).await?;
            if e.is_dir {
                continue;
            }
            // The disk is read on its own thread: a slow disk must never stall the network tasks
            // (which also carry video and input).
            let (tx, mut chunks) = tokio::sync::mpsc::channel::<std::io::Result<Vec<u8>>>(8);
            let (src, size) = (e.src.clone(), e.size);
            std::thread::spawn(move || {
                let mut f = match std::fs::File::open(&src) {
                    Ok(f) => f,
                    Err(err) => return drop(tx.blocking_send(Err(err))),
                };
                let mut left = size;
                while left > 0 {
                    let mut b = vec![0u8; CHUNK.min(left as usize)];
                    match f.read(&mut b) {
                        Ok(0) => return drop(tx.blocking_send(Err(std::io::ErrorKind::UnexpectedEof.into()))),
                        Ok(n) => {
                            b.truncate(n);
                            left -= n as u64;
                            if tx.blocking_send(Ok(b)).is_err() {
                                return; // transfer cancelled
                            }
                        }
                        Err(err) => return drop(tx.blocking_send(Err(err))),
                    }
                }
            });
            let mut left = e.size;
            while left > 0 {
                let chunk = chunks.recv().await.context("file reader stopped")?.with_context(|| format!("{}", e.src.display()))?;
                let n = chunk.len();
                pacer.wait(conn, n).await;
                s.write_all(&chunk).await?;
                left -= n as u64;
                progress.add(id, n as u64);
            }
        }
        s.finish()?;
        s.stopped().await?; // wait until the other side has it all
        Ok(())
    }
    .await;
    progress.end(id);
    result
}

/// Where received files go: `Downloads\ORBIT` (`ORBIT_DOWNLOADS` overrides it, for tests).
pub fn inbox() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("ORBIT_DOWNLOADS") {
        return Ok(PathBuf::from(d));
    }
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    let downloads = unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None)?;
        let s = p.to_string();
        CoTaskMemFree(Some(p.0 as *const _));
        PathBuf::from(s?)
    };
    Ok(downloads.join("ORBIT"))
}

/// Checks one path from the other PC and splits it into safe components.
pub fn safe_components(rel: &str) -> Result<Vec<String>> {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let mut out = Vec::new();
    for c in rel.split('/') {
        let c = c.trim_end_matches([' ', '.']);
        ensure!(!c.is_empty(), "bad path {rel:?}");
        ensure!(!c.chars().any(|ch| ch < ' ' || "<>:\"\\|?*".contains(ch)), "bad file name {c:?}");
        let stem = c.split('.').next().unwrap_or("").to_ascii_uppercase();
        ensure!(!RESERVED.contains(&stem.as_str()), "reserved file name {c:?}");
        out.push(c.to_string());
    }
    ensure!(!out.is_empty(), "empty path");
    Ok(out)
}

/// `name` in `dir`, or `name (1).ext`, `name (2).ext`, ... if taken.
fn unique(dir: &Path, name: &str, is_dir: bool) -> PathBuf {
    let taken = |p: &Path| p.exists() || p.with_file_name(format!("{}.part", p.file_name().unwrap().to_string_lossy())).exists();
    let first = dir.join(name);
    if !taken(&first) {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if !is_dir && i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !taken(p)).unwrap()
}

async fn read_u8(r: &mut RecvStream) -> Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b).await?;
    Ok(b[0])
}
async fn read_u16(r: &mut RecvStream) -> Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b).await?;
    Ok(u16::from_le_bytes(b))
}
async fn read_u32(r: &mut RecvStream) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).await?;
    Ok(u32::from_le_bytes(b))
}
async fn read_u64(r: &mut RecvStream) -> Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b).await?;
    Ok(u64::from_le_bytes(b))
}

/// Receives one batch (the kind byte already read) into `root`. Returns the purpose and the
/// top-level items as saved.
pub async fn receive(r: &mut RecvStream, root: &Path, progress: &Progress) -> Result<(u8, Vec<PathBuf>)> {
    let purpose = read_u8(r).await?;
    let total = read_u64(r).await?;
    let count = read_u32(r).await? as usize;
    ensure!(count <= MAX_ENTRIES, "too many files");
    std::fs::create_dir_all(root).with_context(|| format!("{}", root.display()))?;
    // Top-level names as sent → where they were saved (renamed if taken).
    let mut tops: HashMap<String, PathBuf> = HashMap::new();
    let mut order: Vec<PathBuf> = Vec::new();
    let id = progress.begin(false, "files".into(), total);
    let result = async {
        for _ in 0..count {
            let is_dir = read_u8(r).await? != 0;
            let len = read_u16(r).await? as usize;
            let mut p = vec![0u8; len];
            r.read_exact(&mut p).await?;
            let comps = safe_components(std::str::from_utf8(&p).context("path isn't UTF-8")?)?;
            let top = match tops.get(&comps[0]) {
                Some(t) => t.clone(),
                None => {
                    ensure!(comps.len() == 1, "{} arrived before its folder", comps.join("/"));
                    let t = unique(root, &comps[0], is_dir);
                    tops.insert(comps[0].clone(), t.clone());
                    order.push(t.clone());
                    let names: Vec<String> = order.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
                    progress.with(id, |x| x.name = batch_name(&names));
                    t
                }
            };
            let dest = comps[1..].iter().fold(top, |p, c| p.join(c));
            if is_dir {
                std::fs::create_dir_all(&dest)?;
                continue;
            }
            let size = read_u64(r).await?;
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let part = dest.with_file_name(format!("{}.part", dest.file_name().unwrap().to_string_lossy()));
            let f = std::fs::File::create(&part).with_context(|| format!("{}", part.display()))?;
            // Written on its own thread, so the disk never stalls the network tasks.
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
            let writer = std::thread::spawn(move || -> std::io::Result<()> {
                let mut f = std::io::BufWriter::with_capacity(CHUNK, f);
                while let Some(b) = rx.blocking_recv() {
                    f.write_all(&b)?;
                }
                f.flush()
            });
            let copied = async {
                let mut left = size;
                while left > 0 {
                    let want = CHUNK.min(left as usize);
                    let mut b = vec![0u8; want];
                    r.read_exact(&mut b).await?;
                    tx.send(b).await.ok().context("writing the file failed")?;
                    left -= want as u64;
                    progress.add(id, want as u64);
                }
                anyhow::Ok(())
            }
            .await;
            drop(tx);
            let written = writer.join().unwrap_or_else(|_| Err(std::io::Error::other("writer panicked")));
            let copied = copied.and_then(|()| written.with_context(|| format!("{}", part.display())));
            if let Err(e) = copied {
                let _ = std::fs::remove_file(&part);
                return Err(e);
            }
            std::fs::rename(&part, &dest)?;
        }
        Ok(())
    }
    .await;
    progress.end(id);
    result?;
    if r.read_to_end(0).await.is_err() {
        bail!("extra data after the files");
    }
    Ok((purpose, order))
}

/// Opens Explorer on the folder of `path`, with it selected.
pub fn show_in_explorer(path: &Path) {
    let _ = std::process::Command::new("explorer.exe").arg(format!("/select,{}", path.display())).spawn();
}

/// Shares one [`Progress`] between tasks.
pub type SharedProgress = Arc<Progress>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_paths() {
        assert_eq!(safe_components("a/b c/d.txt").unwrap(), vec!["a", "b c", "d.txt"]);
        assert_eq!(safe_components("name. ").unwrap(), vec!["name"]);
        for bad in ["../x", "a/../b", "/abs", "C:/x", "a\\b", "", "a//b", "con", "nul.txt", "x/COM1.log", "a/b?"] {
            assert!(safe_components(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn picks_free_names() {
        let d = std::env::temp_dir().join(format!("orbit-unique-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(unique(&d, "a.txt", false), d.join("a.txt"));
        std::fs::write(d.join("a.txt"), b"").unwrap();
        std::fs::write(d.join("a (1).txt.part"), b"").unwrap();
        assert_eq!(unique(&d, "a.txt", false), d.join("a (2).txt"));
        std::fs::create_dir(d.join("folder.v2")).unwrap();
        assert_eq!(unique(&d, "folder.v2", true), d.join("folder.v2 (1)"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
