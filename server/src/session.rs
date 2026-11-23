//! One client session: QUIC handshake, the video thread, control messages and input.
//!
//! ```text
//!             ┌────────────── video thread (std) ──────────────┐
//! screen ───► │ capture → encode → backlog check → datagrams   │ ───► client
//!             └────────────────────────────────────────────────┘
//!                       ▲ force_idr (AtomicBool)
//! client ───► ctrl stream task: hello, keyframe requests, keys, buttons, wheel, ping→pong
//! client ───► datagram task: mouse moves → SendInput
//! client ◄─── ctrl writer task ◄── pongs, pointer shape changes (cursor task, every 16 ms)
//! ```

use crate::capture::MonitorInfo;
use crate::input::Input;
use crate::rate::{RateControl, Report};
use crate::video::Pipeline;
use anyhow::{bail, ensure, Context, Result};
use proto::clock::now_us;
use proto::ctrl::{Ctrl, MouseMove, HEADER_LEN};
use proto::stats::{mbit_per_s, Dist, Ticker};
use proto::video::{datagram_count, fragment};
use quinn::{Connection, RecvStream, SendDatagramError, SendStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
    /// Direct or relayed, from the local Tailscale.
    path: proto::tailscale::PathWatcher,
    stop: Arc<AtomicBool>,
    status: Arc<Status>,
}

/// What the server is doing, for a UI to show. Updated by the session and video thread.
#[derive(Default)]
pub struct Status {
    inner: Mutex<StatusInfo>,
}

#[derive(Clone, Default, Debug)]
pub struct StatusInfo {
    /// Address of the connected client, if any.
    pub client: Option<String>,
    /// Latest once-a-second stats line.
    pub stats: Option<String>,
    /// Latest problem (cleared when things work again).
    pub error: Option<String>,
}

impl Status {
    pub fn get(&self) -> StatusInfo {
        self.inner.lock().unwrap().clone()
    }
    fn update(&self, f: impl FnOnce(&mut StatusInfo)) {
        f(&mut self.inner.lock().unwrap());
    }
}

pub async fn serve(endpoint: quinn::Endpoint, settings: Settings, status: Arc<Status>) -> Result<()> {
    let mut current: Option<(Connection, Arc<AtomicBool>, tokio::task::JoinHandle<()>)> = None;
    while let Some(incoming) = endpoint.accept().await {
        let remote = incoming.remote_address();
        if !crate::net::is_tailnet(remote.ip()) {
            eprintln!("refused {remote}: not a Tailscale address");
            incoming.refuse();
            continue;
        }
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
        let (c, st, status) = (conn.clone(), stop.clone(), status.clone());
        let task = tokio::spawn(async move {
            let addr = c.remote_address().to_string();
            status.update(|i| {
                i.client = Some(addr.clone());
                i.stats = None;
                i.error = None;
            });
            let r = session(c.clone(), s, st, status.clone()).await;
            match &r {
                Ok(()) => println!("session ended: {addr}"),
                Err(e) => println!("session ended: {addr}: {e:#}"),
            }
            status.update(|i| {
                if i.client.as_deref() == Some(addr.as_str()) {
                    i.client = None;
                    i.stats = None;
                }
                if let Err(e) = r {
                    i.error = Some(format!("{e:#}"));
                }
            });
        });
        current = Some((conn, stop, task));
    }
    // Endpoint closed (sharing stopped): end the running session too.
    if let Some((old, stop, task)) = current.take() {
        stop.store(true, Ordering::Release);
        old.close(0u32.into(), b"server stopped");
        let _ = task.await;
    }
    Ok(())
}

/// Stops a running server, and knows which endpoint it is using right now.
#[derive(Default)]
pub struct Control {
    stopping: AtomicBool,
    endpoint: Mutex<Option<quinn::Endpoint>>,
}

impl Control {
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(e) = self.endpoint.lock().unwrap().as_ref() {
            e.close(0u32.into(), b"server stopped");
        }
    }
}

/// Like [`serve`], but keeps listening until [`Control::stop`].
///
/// quinn's endpoint can die by itself: its receive task exits on any socket error other than
/// "connection reset" (Windows hands out such errors while network adapters come and go, e.g.
/// right after boot). `accept()` then returns `None` as if sharing had been stopped, and the
/// app would sit at "waiting" with nothing listening. So when `serve` returns without a stop,
/// a fresh endpoint is bound on the same address.
pub async fn serve_until_stopped(mut endpoint: quinn::Endpoint, bind: std::net::SocketAddr, settings: Settings, status: Arc<Status>, ctl: Arc<Control>) -> Result<()> {
    loop {
        *ctl.endpoint.lock().unwrap() = Some(endpoint.clone());
        if ctl.stopping.load(Ordering::Acquire) {
            endpoint.close(0u32.into(), b"server stopped"); // stop raced with the swap above
        }
        serve(endpoint, settings.clone(), status.clone()).await?;
        if ctl.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        eprintln!("listener on {bind} stopped by itself (network error); starting a new one");
        status.update(|i| i.error = Some("listener restarted after a network error".into()));
        ctl.endpoint.lock().unwrap().take(); // release the dead socket so the port is free
        endpoint = loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if ctl.stopping.load(Ordering::Acquire) {
                return Ok(());
            }
            match crate::net::endpoint(bind) {
                Ok(e) => break e,
                Err(e) => {
                    eprintln!("restarting listener: {e:#}; retrying");
                    status.update(|i| i.error = Some(format!("restarting listener: {e:#}")));
                }
            }
        };
    }
}

/// A server running on its own threads, for the UI.
pub struct ServerHandle {
    ctl: Arc<Control>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub status: Arc<Status>,
    pub bind: std::net::SocketAddr,
}

impl ServerHandle {
    pub fn start(settings: Settings, bind: std::net::SocketAddr) -> Result<ServerHandle> {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        let endpoint = {
            let _guard = rt.enter();
            crate::net::endpoint(bind)?
        };
        let status = Arc::new(Status::default());
        let ctl = Arc::new(Control::default());
        let (st, c) = (status.clone(), ctl.clone());
        let thread = std::thread::Builder::new().name("orbit-server".into()).spawn(move || {
            if let Err(e) = rt.block_on(serve_until_stopped(endpoint, bind, settings, st.clone(), c)) {
                st.update(|i| i.error = Some(format!("{e:#}")));
            }
        })?;
        Ok(ServerHandle { ctl, thread: Some(thread), status, bind })
    }

    pub fn stop(mut self) {
        self.ctl.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

async fn read_ctrl(r: &mut RecvStream) -> Result<Ctrl> {
    let mut h = [0u8; HEADER_LEN];
    r.read_exact(&mut h).await?;
    let (tag, len) = Ctrl::parse_header(h);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ctrl::decode(tag, &body)
}

async fn write_ctrl(w: &mut SendStream, c: &Ctrl) -> Result<()> {
    let mut buf = Vec::with_capacity(64);
    c.encode(&mut buf);
    w.write_all(&buf).await?;
    Ok(())
}

async fn session(conn: Connection, s: Settings, stop: Arc<AtomicBool>, status: Arc<Status>) -> Result<()> {
    let (mut send, mut recv) = tokio::time::timeout(Duration::from_secs(5), conn.accept_bi()).await.context("no control stream")??;
    match read_ctrl(&mut recv).await? {
        Ctrl::ClientHello { version } => ensure!(version == proto::VERSION, "client protocol {version}, server {}", proto::VERSION),
        other => bail!("expected hello, got {other:?}"),
    }
    write_ctrl(&mut send, &Ctrl::ServerHello { version: proto::VERSION, width: s.monitor.width as u16, height: s.monitor.height as u16, fps: s.fps as u16 }).await?;

    let shared = Arc::new(Shared { force_idr: AtomicBool::new(true), path: proto::tailscale::PathWatcher::start(conn.remote_address().ip()), stop: stop.clone(), status });
    let video = {
        let (conn, s, shared) = (conn.clone(), s.clone(), shared.clone());
        std::thread::Builder::new().name("video".into()).spawn(move || video_thread(conn, s, shared))?
    };
    let input = Input::new(s.monitor.clone());
    // Everything sent on the control stream goes through this channel to one writer.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Ctrl>();

    let writer = async {
        while let Some(m) = out_rx.recv().await {
            write_ctrl(&mut send, &m).await?;
        }
        Ok(())
    };
    let cursor = async {
        let mut watch = crate::cursor::CursorWatch::new();
        let mut tick = tokio::time::interval(Duration::from_millis(16));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut msgs = Vec::new();
        loop {
            tick.tick().await;
            watch.poll(&mut msgs);
            for m in msgs.drain(..) {
                out_tx.send(m)?;
            }
        }
    };
    let ctrl = async {
        loop {
            match read_ctrl(&mut recv).await? {
                Ctrl::KeyframeRequest => shared.force_idr.store(true, Ordering::Release),
                Ctrl::Ping { client_us } => out_tx.send(Ctrl::Pong { client_us, server_us: now_us() })?,
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
        r = writer => r,
        r = cursor => r,
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
    // Adaptive bitrate: the configured bitrate is the ceiling (see rate.rs).
    let mut rate = RateControl::new(s.bitrate_bps);
    // Allowed bytes waiting in quinn before we stop encoding: ~2 frames at the current bitrate.
    let backlog_limit_for = |bps: u32| (2 * (bps / 8 / s.fps.max(1)) as usize).max(64 * 1024);
    let mut backlog_limit = backlog_limit_for(rate.bps());
    let interval = Duration::from_secs_f64(1.0 / s.fps.max(1) as f64);
    let mut buf = Vec::with_capacity(1 << 20);
    let mut ticker = Ticker::new(Duration::from_secs(1));
    let (mut frames, mut bytes, mut keys, mut dropped) = (0u64, 0u64, 0u64, 0u64);
    // Time spent waiting for the network to drain the send queue (per stats second).
    let mut net_wait = Duration::ZERO;
    let (mut enc_ms, mut bridge_ms) = (Dist::default(), Dist::default());
    let mut lost_prev = conn.stats().path.lost_packets;
    let mut sent_prev = conn.stats().path.sent_packets;

    'pipeline: while !shared.stop.load(Ordering::Acquire) {
        // (Re)create the pipeline. Failures (device lost, driver reset, mode change) restart
        // it instead of ending the session.
        let mut p = match Pipeline::new(s.monitor.clone(), s.fps, rate.video_bps()) {
            Ok(p) => {
                shared.status.update(|i| i.error = None);
                p
            }
            Err(e) => {
                eprintln!("video pipeline failed to start: {e:#}; retrying");
                shared.status.update(|i| i.error = Some(format!("{e:#}")));
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        let mut need_idr = true;
        while !shared.stop.load(Ordering::Acquire) {
            let started = Instant::now();
            // Stats first, so a still screen (no frames to send) still reports 0 fps.
            if let Some(secs) = ticker.tick(started) {
                let st = conn.stats();
                let (fps, mbit, enc, rtt) = (frames as f64 / secs, mbit_per_s(bytes, secs), enc_ms.take(), conn.rtt().as_secs_f64() * 1e3);
                let wait_ms = net_wait.as_secs_f64() * 1e3;
                let lost = st.path.lost_packets - lost_prev;
                let report = Report { rtt: conn.rtt(), sent_packets: st.path.sent_packets - sent_prev, lost_packets: lost, net_wait: net_wait.as_secs_f64() / secs };
                let old_video = rate.video_bps();
                let new_bps = rate.update(&report);
                backlog_limit = backlog_limit_for(new_bps);
                let new_video = rate.video_bps();
                if new_video != old_video {
                    if let Err(e) = p.encoder().set_bitrate(new_video) {
                        eprintln!("set bitrate {new_video}: {e:#}");
                    }
                }
                let (target, fec) = (new_bps as f64 / 1e6, rate.fec_percent());
                println!(
                    "[{}] send fps {fps:5.1} | {mbit:6.2} Mbit/s | rate {target:.1} Mbit/s ({}) | fec {fec}% | encode ms {enc} | {}key {keys} | dropped {dropped} | net wait {wait_ms:.0} ms | rtt {rtt:.1} ms{} | lost pkts {lost}",
                    p.capture_mode(),
                    rate.reason(),
                    if p.bridged() { format!("copy ms {} | ", bridge_ms.take()) } else { String::new() },
                    shared.path.suffix(" "),
                );
                let enc_avg = enc.split('/').next().unwrap_or("-").to_string();
                let mode = match p.capture_mode() {
                    "gdi" => if crate::capture::in_remote_desktop() { "COMPAT CAPTURE (REMOTE DESKTOP) · " } else { "COMPAT CAPTURE · " },
                    _ => "",
                };
                shared.status.update(|i| {
                    i.stats = Some(format!("{mode}{fps:.0} FPS · {mbit:.1} MBIT/S · RATE {target:.1} ({}) · FEC {fec}% · ENCODE {enc_avg} MS · RTT {rtt:.0} MS · NET WAIT {wait_ms:.0} MS · DROPPED {dropped}{}", rate.reason().to_uppercase(), shared.path.suffix(" · ")))
                });
                lost_prev = st.path.lost_packets;
                sent_prev = st.path.sent_packets;
                (frames, bytes, keys, dropped, net_wait) = (0, 0, 0, 0, Duration::ZERO);
            }
            // Backpressure: while quinn still holds more than ~2 frames of unsent data, don't
            // capture or encode at all. Skipping *before* the encoder keeps its reference chain
            // intact (no keyframe needed), and Desktop Duplication merges the changes made
            // meanwhile, so the next capture is still the newest screen.
            let backlog = DATAGRAM_SEND_BUFFER.saturating_sub(conn.datagram_send_buffer_space());
            if backlog > backlog_limit {
                std::thread::sleep(Duration::from_millis(2));
                net_wait += started.elapsed();
                continue;
            }
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
            let wire = datagram_count(buf.len(), max, rate.fec_percent()) * max;
            if backlog > backlog_limit || wire > conn.datagram_send_buffer_space() {
                // Fallback for one frame too big for the queue's free space (the check above
                // normally waits before encoding). Dropping an encoded frame breaks the
                // reference chain, so the next frame must be a keyframe.
                dropped += 1;
                need_idr = true;
                continue;
            }
            let grams = match fragment(frame_id, e.capture_us, e.keyframe, &buf, max, rate.fec_percent()) {
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

            if let Some(rest) = interval.checked_sub(started.elapsed()) {
                std::thread::sleep(rest);
            }
        }
    }
}
