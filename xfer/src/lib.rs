//! Things the two PCs send each other besides video and input: clipboard items and files.
//!
//! Each item travels on its own one-way QUIC stream: a kind byte, then the content, then the
//! end of the stream. Streams are reliable and independent, so a big image never delays the
//! control stream (keys, mouse buttons), and video datagrams don't wait for it either. Item
//! streams get a lower priority than the control stream.

pub mod clipboard;
pub mod files;
pub mod image;

use anyhow::Result;
use clipboard::{Clip, Setter, Watcher};
use files::{Progress, SharedProgress};
use quinn::{Connection, RecvStream};
use std::path::PathBuf;
use std::sync::Arc;

/// Stream kinds (first byte).
pub mod kind {
    pub const CLIP_TEXT: u8 = 1;
    pub const CLIP_PNG: u8 = 2;
    pub const FILES: u8 = 3;
}

/// Largest item accepted from the other PC.
pub const MAX_ITEM: usize = 64 << 20;

/// Runs the side streams for one connection until it closes, or until dropped.
pub struct Sync {
    _watcher: Option<Watcher>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    drops: tokio::sync::mpsc::UnboundedSender<Vec<PathBuf>>,
    progress: SharedProgress,
}

impl Sync {
    /// Sends files / folders dropped on the viewer to the other PC.
    pub fn drop_files(&self) -> DropSender {
        DropSender(self.drops.clone())
    }

    /// The running transfer, e.g. `"SENDING clip.mp4 45% · 12.3 MB/s"`.
    pub fn progress(&self) -> Option<String> {
        self.progress.text()
    }
}

/// Queues dropped files for sending; usable from any thread.
#[derive(Clone)]
pub struct DropSender(tokio::sync::mpsc::UnboundedSender<Vec<PathBuf>>);

impl DropSender {
    pub fn send(&self, paths: Vec<PathBuf>) {
        let _ = self.0.send(paths);
    }
}

impl Drop for Sync {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Starts clipboard sync and file transfer on `conn` (call inside a tokio runtime). With
/// `clipboard` false, nothing is copied either way (dropped files still go through).
pub fn start(conn: Connection, clipboard: bool) -> Result<Sync> {
    let mut tasks = Vec::new();
    let progress: SharedProgress = Arc::new(Progress::default());
    let (watcher, setter) = if clipboard {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Clip>();
        let w = Watcher::start(move |c| {
            let _ = tx.send(c);
        })?;
        let setter = w.setter();
        let (c, prog) = (conn.clone(), progress.clone());
        tasks.push(tokio::spawn(async move {
            // Copied files are sent in the background; a newer copy cancels them.
            let mut files_task: Option<tokio::task::JoinHandle<()>> = None;
            while let Some(clip) = rx.recv().await {
                if let Some(t) = files_task.take() {
                    t.abort();
                }
                if let Clip::Files(paths) = clip {
                    let (c, prog) = (c.clone(), prog.clone());
                    files_task = Some(tokio::spawn(async move {
                        let p = paths.clone();
                        match tokio::task::spawn_blocking(move || files::total_size(&p)).await {
                            Ok(Ok(n)) if n > files::CLIPBOARD_LIMIT => {
                                return eprintln!("copied files are {:.1} GB: too big to send on copy, drag them onto the viewer instead", n as f64 / 1e9);
                            }
                            Ok(Err(e)) => return eprintln!("copied files: {e:#}"),
                            _ => {}
                        }
                        if let Err(e) = files::send(&c, paths, files::purpose::CLIPBOARD, &prog).await {
                            eprintln!("sending copied files: {e:#}");
                        }
                    }));
                    continue;
                }
                if let Err(e) = send_clip(&c, clip).await {
                    eprintln!("clipboard send: {e:#}");
                    if c.close_reason().is_some() {
                        break;
                    }
                }
            }
        }));
        (Some(w), Some(setter))
    } else {
        (None, None)
    };
    let (drops, mut drop_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<PathBuf>>();
    {
        let (c, prog) = (conn.clone(), progress.clone());
        tasks.push(tokio::spawn(async move {
            while let Some(paths) = drop_rx.recv().await {
                if let Err(e) = files::send(&c, paths, files::purpose::DROP, &prog).await {
                    eprintln!("sending dropped files: {e:#}");
                }
            }
        }));
    }
    let prog = progress.clone();
    tasks.push(tokio::spawn(async move {
        while let Ok(r) = conn.accept_uni().await {
            let (setter, prog) = (setter.clone(), prog.clone());
            tokio::spawn(async move {
                if let Err(e) = receive_item(r, setter, &prog).await {
                    eprintln!("receiving: {e:#}");
                }
            });
        }
    }));
    Ok(Sync { _watcher: watcher, tasks, drops, progress })
}

async fn receive_item(mut r: RecvStream, setter: Option<Setter>, progress: &Progress) -> Result<()> {
    let mut k = [0u8; 1];
    r.read_exact(&mut k).await?;
    let clip = match k[0] {
        kind::FILES => {
            let (purpose, saved) = files::receive(&mut r, &files::inbox()?, progress).await?;
            match purpose {
                files::purpose::CLIPBOARD if setter.is_some() => Clip::Files(saved),
                files::purpose::CLIPBOARD => return Ok(()),
                _ => {
                    if let Some(first) = saved.first() {
                        files::show_in_explorer(first);
                    }
                    return Ok(());
                }
            }
        }
        kind::CLIP_TEXT => Clip::Text(String::from_utf8_lossy(&r.read_to_end(MAX_ITEM).await?).into_owned()),
        kind::CLIP_PNG => Clip::Png(r.read_to_end(MAX_ITEM).await?),
        other => anyhow::bail!("unknown item kind {other}"),
    };
    if let Some(s) = setter {
        s.set(clip);
    }
    Ok(())
}

async fn send_clip(conn: &Connection, clip: Clip) -> Result<()> {
    let (k, body): (u8, &[u8]) = match &clip {
        Clip::Text(t) => (kind::CLIP_TEXT, t.as_bytes()),
        Clip::Png(b) => (kind::CLIP_PNG, b),
        Clip::Files(_) => return Ok(()), // sent by files::send
    };
    let mut s = conn.open_uni().await?;
    s.set_priority(-1)?;
    s.write_all(&[k]).await?;
    s.write_all(body).await?;
    s.finish()?;
    Ok(())
}
