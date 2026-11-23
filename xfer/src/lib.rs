//! Things the two PCs send each other besides video and input: clipboard now, files later.
//!
//! Each item travels on its own one-way QUIC stream: a kind byte, then the content, then the
//! end of the stream. Streams are reliable and independent, so a big image never delays the
//! control stream (keys, mouse buttons), and video datagrams don't wait for it either. Item
//! streams get a lower priority than the control stream.

pub mod clipboard;
pub mod image;

use anyhow::Result;
use clipboard::{Clip, Watcher};
use quinn::Connection;

/// Stream kinds (first byte).
pub mod kind {
    pub const CLIP_TEXT: u8 = 1;
    pub const CLIP_PNG: u8 = 2;
}

/// Largest item accepted from the other PC.
pub const MAX_ITEM: usize = 64 << 20;

/// Runs the side streams for one connection until it closes, or until dropped.
pub struct Sync {
    _watcher: Option<Watcher>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Sync {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Starts clipboard sync on `conn` (call inside a tokio runtime). With `clipboard` false,
/// nothing is sent and clipboard items from the other side are ignored.
pub fn start(conn: Connection, clipboard: bool) -> Result<Sync> {
    let mut tasks = Vec::new();
    let (watcher, setter) = if clipboard {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Clip>();
        let w = Watcher::start(move |c| {
            let _ = tx.send(c);
        })?;
        let setter = w.setter();
        let c = conn.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(clip) = rx.recv().await {
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
    tasks.push(tokio::spawn(async move {
        while let Ok(mut r) = conn.accept_uni().await {
            let setter = setter.clone();
            tokio::spawn(async move {
                let data = match r.read_to_end(MAX_ITEM + 1).await {
                    Ok(d) => d,
                    Err(e) => return eprintln!("receiving an item: {e}"),
                };
                let Some((&k, body)) = data.split_first() else { return };
                let clip = match k {
                    kind::CLIP_TEXT => Clip::Text(String::from_utf8_lossy(body).into_owned()),
                    kind::CLIP_PNG => Clip::Png(body.to_vec()),
                    other => return eprintln!("unknown item kind {other}"),
                };
                if let Some(s) = setter {
                    s.set(clip);
                }
            });
        }
    }));
    Ok(Sync { _watcher: watcher, tasks })
}

async fn send_clip(conn: &Connection, clip: Clip) -> Result<()> {
    let (k, body): (u8, &[u8]) = match &clip {
        Clip::Text(t) => (kind::CLIP_TEXT, t.as_bytes()),
        Clip::Png(b) => (kind::CLIP_PNG, b),
    };
    let mut s = conn.open_uni().await?;
    s.set_priority(-1)?;
    s.write_all(&[k]).await?;
    s.write_all(body).await?;
    s.finish()?;
    Ok(())
}
