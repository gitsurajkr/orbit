//! One client session: QUIC handshake, the video thread, control messages and input.
//!
//! ```text
//!             ┌────────────── video thread (std) ──────────────┐
//! screen ───► │ capture → encode → backlog check → datagrams   │ ───► client
//!             └────────────────────────────────────────────────┘
//!                       ▲ force_idr (AtomicBool)
//! client ───► ctrl stream task: hello, keyframe requests, keys, buttons, wheel, ping→pong
//! client ───► datagram task: mouse moves → SendInput
//! ```

use crate::capture::MonitorInfo;
use crate::input::Input;
use crate::video::Pipeline;
use anyhow::{bail, ensure, Context, Result};
use proto::clock::now_us;
use proto::ctrl::{Ctrl, MouseMove, HEADER_LEN};
use proto::stats::{mbit_per_s, Dist, Ticker};
use proto::video::{fragment, fragment_count};
use quinn::{Connection, RecvStream, SendDatagramError, SendStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// quinn's datagram queue. Large on purpose so quinn never silently drops from the middle of a
/// frame; our own backlog check (below) is what keeps latency low.
pub const DATAGRAM_SEND_BUFFER: usize = 1 << 20;

#[derive(Clone)]
pub struct Settings {
    pub monitor: MonitorInfo,
    pub fps: u32,
    pub bitrate_bps: u32,
}

struct Shared {
    force_idr: AtomicBool,
    stop: Arc<AtomicBool>,
}

pub async fn serve(endpoint: quinn::Endpoint, settings: Settings) -> Result<()> {
    let mut current: Option<(Connection, Arc<AtomicBool>, tokio::task::JoinHandle<()>)> = None;
    while let Some(incoming) = endpoint.accept().await {
        let conn = match incoming.await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("handshake failed: {e}");
                continue;
            }
        };
        println!("client connected: {}", conn.remote_address());
        // Newest client wins: stop the old session completely (it owns the screen duplication).
        if let Some((old, stop, task)) = current.take() {
            println!("replacing previous session from {}", old.remote_address());
            stop.store(true, Ordering::Release);
            old.close(0u32.into(), b"replaced");
            let _ = task.await;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let s = settings.clone();
        let (c, st) = (conn.clone(), stop.clone());
        let task = tokio::spawn(async move {
            match session(c.clone(), s, st).await {
                Ok(()) => println!("session ended: {}", c.remote_address()),
                Err(e) => println!("session ended: {}: {e:#}", c.remote_address()),
            }
        });
        current = Some((conn, stop, task));
    }
    Ok(())
}

async fn read_ctrl(r: &mut RecvStream) -> Result<Ctrl> {
    let mut h = [0u8; HEADER_LEN];
    r.read_exact(&mut h).await?;
    let (tag, len) = Ctrl::parse_header(h);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ctrl::decode(tag, &body)
}

async fn write_ctrl(w: &mut SendStream, c: Ctrl) -> Result<()> {
    let mut buf = Vec::with_capacity(32);
    c.encode(&mut buf);
    w.write_all(&buf).await?;
    Ok(())
}

async fn session(conn: Connection, s: Settings, stop: Arc<AtomicBool>) -> Result<()> {
    let (mut send, mut recv) = tokio::time::timeout(Duration::from_secs(5), conn.accept_bi()).await.context("no control stream")??;
    match read_ctrl(&mut recv).await? {
        Ctrl::ClientHello { version } => ensure!(version == proto::VERSION, "client protocol {version}, server {}", proto::VERSION),
        other => bail!("expected hello, got {other:?}"),
    }
    write_ctrl(&mut send, Ctrl::ServerHello { version: proto::VERSION, width: s.monitor.width as u16, height: s.monitor.height as u16, fps: s.fps as u16 }).await?;

    let shared = Arc::new(Shared { force_idr: AtomicBool::new(true), stop: stop.clone() });
    let video = {
        let (conn, s, shared) = (conn.clone(), s.clone(), shared.clone());
        std::thread::Builder::new().name("video".into()).spawn(move || video_thread(conn, s, shared))?
    };
    let input = Input::new(s.monitor.clone());

    let ctrl = async {
        loop {
            match read_ctrl(&mut recv).await? {
                Ctrl::KeyframeRequest => shared.force_idr.store(true, Ordering::Release),
                Ctrl::Ping { client_us } => write_ctrl(&mut send, Ctrl::Pong { client_us, server_us: now_us() }).await?,
                c @ (Ctrl::Key { .. } | Ctrl::MouseButton { .. } | Ctrl::Wheel { .. }) => input.handle(c),
                other => bail!("unexpected {other:?}"),
            }
        }
    };
    let dgrams = async {
        loop {
            let d = conn.read_datagram().await?;
            if d.first() == Some(&proto::DG_MOUSE_MOVE) {
                if let Ok(m) = MouseMove::decode(&d) {
                    input.mouse_move(m);
                }
            }
        }
    };
    let result: Result<()> = tokio::select! {
        r = ctrl => r,
        r = dgrams => r,
    };
    stop.store(true, Ordering::Release);
    let _ = tokio::task::spawn_blocking(move || video.join()).await;
    input.release_all();
    // A closed connection is how sessions normally end.
    match result {
        Err(e) if e.downcast_ref::<quinn::ConnectionError>().is_some() || e.downcast_ref::<quinn::ReadExactError>().is_some() => Ok(()),
        r => r,
    }
}

fn video_thread(conn: Connection, s: Settings, shared: Arc<Shared>) {
    let mut frame_id: u32 = 0;
    // Allowed bytes waiting in quinn before we drop a frame instead of queueing it: ~2 frames.
    let frame_budget = (s.bitrate_bps / 8 / s.fps.max(1)) as usize;
    let backlog_limit = (2 * frame_budget).max(64 * 1024);
    let interval = Duration::from_secs_f64(1.0 / s.fps.max(1) as f64);
    let mut buf = Vec::with_capacity(1 << 20);
    let mut ticker = Ticker::new(Duration::from_secs(1));
    let (mut frames, mut bytes, mut keys, mut dropped) = (0u64, 0u64, 0u64, 0u64);
    let (mut enc_ms, mut bridge_ms) = (Dist::default(), Dist::default());
    let mut lost_prev = conn.stats().path.lost_packets;

    'pipeline: while !shared.stop.load(Ordering::Acquire) {
        // (Re)create the pipeline. Failures (device lost, driver reset, mode change) restart
        // it instead of ending the session.
        let mut p = match Pipeline::new(s.monitor.clone(), s.fps, s.bitrate_bps) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("video pipeline failed to start: {e:#}; retrying");
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        let mut need_idr = true;
        while !shared.stop.load(Ordering::Acquire) {
            let started = Instant::now();
            let force = shared.force_idr.swap(false, Ordering::AcqRel) | need_idr;
            buf.clear();
            let e = match p.next(50, force, &mut buf) {
                Ok(Some(e)) => e,
                Ok(None) => {
                    if force {
                        shared.force_idr.store(true, Ordering::Release); // not served yet
                    }
                    continue;
                }
                Err(err) => {
                    eprintln!("video pipeline error: {err:#}; restarting it");
                    std::thread::sleep(Duration::from_millis(200));
                    continue 'pipeline;
                }
            };
            need_idr = false;

            let Some(max) = conn.max_datagram_size() else {
                eprintln!("client doesn't accept datagrams");
                return;
            };
            let backlog = DATAGRAM_SEND_BUFFER.saturating_sub(conn.datagram_send_buffer_space());
            let wire = fragment_count(buf.len(), max) * max;
            if backlog > backlog_limit || wire > conn.datagram_send_buffer_space() {
                // The network isn't keeping up. Sending this frame would only queue it behind
                // older data; drop it and send a fresh keyframe once the queue drains.
                dropped += 1;
                need_idr = true;
                continue;
            }
            let grams = match fragment(frame_id, e.capture_us, e.keyframe, &buf, max) {
                Ok(g) => g,
                Err(err) => {
                    eprintln!("fragment: {err:#}");
                    need_idr = true;
                    continue;
                }
            };
            for g in grams {
                match conn.send_datagram(g.into()) {
                    Ok(()) => {}
                    Err(SendDatagramError::TooLarge) => {
                        // Path MTU shrank mid-frame: drop the frame, not the session.
                        need_idr = true;
                        break;
                    }
                    Err(err) => {
                        eprintln!("send: {err}");
                        return;
                    }
                }
            }
            frame_id = frame_id.wrapping_add(1);
            frames += 1;
            bytes += buf.len() as u64;
            keys += e.keyframe as u64;
            enc_ms.add(e.encode_ms);
            bridge_ms.add(e.bridge_ms);

            if let Some(secs) = ticker.tick(Instant::now()) {
                let st = conn.stats();
                println!(
                    "send fps {:5.1} | {:6.2} Mbit/s | encode ms {} | {}key {} | dropped {} | rtt {:.1} ms | lost pkts {}",
                    frames as f64 / secs,
                    mbit_per_s(bytes, secs),
                    enc_ms.take(),
                    if p.bridged() { format!("bridge ms {} | ", bridge_ms.take()) } else { String::new() },
                    keys,
                    dropped,
                    conn.rtt().as_secs_f64() * 1e3,
                    st.path.lost_packets - lost_prev,
                );
                lost_prev = st.path.lost_packets;
                (frames, bytes, keys, dropped) = (0, 0, 0, 0);
            }
            if let Some(rest) = interval.checked_sub(started.elapsed()) {
                std::thread::sleep(rest);
            }
        }
    }
}
