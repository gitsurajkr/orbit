//! One client session: QUIC handshake, admission (device list / pairing code), the video
//! thread, control messages and input. Nothing is captured or injected before admission.
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
pub const DATAGRAM_SEND_BUFFER: usize = 4 << 20;

#[derive(Clone)]
pub struct Settings {
    pub monitor: MonitorInfo,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// This PC's certificate and key.
    pub identity: auth::Identity,
    /// Who may connect (shared with the UI, which shows and edits it).
    pub gate: Arc<auth::Gate>,
}

struct Shared {
    force_idr: AtomicBool,
    /// The client lost frames from this id on: invalidate them and send a recovery frame.
    invalidate: Mutex<Option<u32>>,
    /// The screen being shown; the video thread rebuilds its pipeline when `switch` is set.
    monitor: Mutex<MonitorInfo>,
    switch: AtomicBool,
    /// The control stream's outbox (to send VideoFormat).
    out: tokio::sync::mpsc::UnboundedSender<Ctrl>,
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
    /// Its PC name, once it's been let in.
    pub client_name: Option<String>,
    /// Latest once-a-second stats line.
    pub stats: Option<String>,
    /// File transfer in progress, e.g. "RECEIVING clip.mp4 45% · 12.3 MB/s".
    pub transfer: Option<String>,
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
                i.client_name = None;
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
                    i.client_name = None;
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
            match crate::net::endpoint(bind, &settings.identity) {
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
            crate::net::endpoint(bind, &settings.identity)?
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

/// SHA-256 of the certificate the peer showed in the TLS handshake.
fn peer_fingerprint(conn: &Connection) -> Option<auth::Fingerprint> {
    let certs = conn.peer_identity()?.downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>().ok()?;
    certs.first().map(|c| auth::fingerprint(c))
}

/// Tells the viewer why it's refused, then fails the session with the same reason.
async fn deny<T>(send: &mut SendStream, reason: &str) -> Result<T> {
    let _ = write_ctrl(send, &Ctrl::Denied { reason: reason.to_string() }).await;
    let _ = send.finish();
    tokio::time::sleep(Duration::from_millis(200)).await; // let it arrive before the close
    bail!("refused: {reason}")
}

/// Decides whether this viewer may in, and returns the name to show for it:
/// 1. its certificate is on the device list → in;
/// 2. it's on this PC's own Tailscale account (and that's allowed) → added to the list, in;
/// 3. a pairing code is showing on this PC → the viewer proves it knows it → added, in;
/// 4. otherwise → told to pair, refused.
async fn admit(conn: &Connection, send: &mut SendStream, recv: &mut RecvStream, s: &Settings, name: String) -> Result<(String, auth::Fingerprint)> {
    let name = if name.trim().is_empty() { "unnamed PC".to_string() } else { name };
    let Some(fp) = peer_fingerprint(conn) else {
        return deny(send, "this viewer has no ID certificate: update ORBIT").await;
    };
    if let Some(d) = s.gate.trusted(&fp) {
        if d.name != name {
            s.gate.add(&name, fp, &d.how); // keep the list's name current
        }
        return Ok((name, fp));
    }
    let ip = conn.remote_address().ip().to_canonical();
    if s.gate.trust_own_account() {
        let same = ip.is_loopback() || tokio::task::spawn_blocking(move || proto::tailscale::same_account(ip)).await?.unwrap_or(false);
        if same {
            println!("pairing {name} ({}): same Tailscale account", auth::short(&fp));
            s.gate.add(&name, fp, "same Tailscale account");
            return Ok((name, fp));
        }
    }
    let code_active = s.gate.pin().is_some();
    write_ctrl(send, &Ctrl::PairRequired { code_active }).await?;
    if !code_active {
        // The viewer explains what to do and disconnects.
        let _ = tokio::time::timeout(Duration::from_secs(2), read_ctrl(recv)).await;
        bail!("{name} isn't paired (no pairing code was showing)");
    }
    let msg = match read_ctrl(recv).await {
        Ok(Ctrl::PairStart { msg }) => msg,
        Ok(other) => bail!("expected a pairing message, got {other:?}"),
        Err(_) => bail!("{name} isn't paired; it can pair with the code shown on this PC"),
    };
    let Some(code) = s.gate.try_pin() else {
        return deny(send, "the pairing code expired or had too many wrong tries: show a new one on the sharing PC").await;
    };
    let my_fp = s.identity.fingerprint();
    let (reply, key) = auth::pake::respond(&code, &fp, &my_fp, &msg)?;
    write_ctrl(send, &Ctrl::PairReply { msg: reply }).await?;
    match read_ctrl(recv).await? {
        Ctrl::PairProof { proof } if key.check_viewer(&proof) => {}
        Ctrl::PairProof { .. } => return deny(send, "wrong pairing code").await,
        other => bail!("expected a pairing proof, got {other:?}"),
    }
    write_ctrl(send, &Ctrl::PairProof { proof: key.proof_from_sharer().to_vec() }).await?;
    s.gate.cancel_pin();
    println!("paired {name} ({}) with the code", auth::short(&fp));
    s.gate.add(&name, fp, "code");
    Ok((name, fp))
}

async fn session(conn: Connection, s: Settings, stop: Arc<AtomicBool>, status: Arc<Status>) -> Result<()> {
    let (mut send, mut recv) = tokio::time::timeout(Duration::from_secs(5), conn.accept_bi()).await.context("no control stream")??;
    let (name, flags) = match read_ctrl(&mut recv).await? {
        Ctrl::ClientHello { version, flags, name } => {
            ensure!(version == proto::VERSION, "client protocol {version}, server {}: update ORBIT on both PCs", proto::VERSION);
            (name, flags)
        }
        other => bail!("expected hello, got {other:?}"),
    };
    let (name, fp) = tokio::time::timeout(Duration::from_secs(20), admit(&conn, &mut send, &mut recv, &s, name)).await.context("pairing took too long")??;
    println!("let in: {name} ({})", conn.remote_address());
    status.update(|i| i.client_name = Some(name.clone()));
    // Clipboard sync, if the viewer asked for it; runs until this function returns.
    let sync = xfer::start(conn.clone(), flags & proto::ctrl::hello::CLIPBOARD != 0)
        .map_err(|e| eprintln!("clipboard sync unavailable: {e:#}"))
        .ok();
    write_ctrl(&mut send, &Ctrl::ServerHello { version: proto::VERSION, width: s.monitor.width as u16, height: s.monitor.height as u16, fps: s.fps as u16 }).await?;

    // Everything sent on the control stream goes through this channel to one writer.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Ctrl>();
    let mut monitors = crate::capture::list_monitors().unwrap_or_default();
    let mut current = monitors.iter().position(|m| m.device_name == s.monitor.device_name).unwrap_or(0);
    let describe = |ms: &[MonitorInfo], cur: usize| Ctrl::Monitors {
        current: cur as u8,
        list: ms.iter().map(|m| proto::ctrl::MonitorDesc { width: m.width as u16, height: m.height as u16, primary: m.left == 0 && m.top == 0 }).collect(),
    };
    out_tx.send(describe(&monitors, current))?;
    let shared = Arc::new(Shared { force_idr: AtomicBool::new(true), invalidate: Mutex::new(None), monitor: Mutex::new(s.monitor.clone()), switch: AtomicBool::new(false), out: out_tx.clone(), path: proto::tailscale::PathWatcher::start(conn.remote_address().ip()), stop: stop.clone(), status });
    let video = {
        let (conn, s, shared) = (conn.clone(), s.clone(), shared.clone());
        std::thread::Builder::new().name("video".into()).spawn(move || video_thread(conn, s, shared))?
    };
    let input = Input::new(s.monitor.clone());

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
    // Removing the device from the list on this PC ends its session too. Also reports file
    // transfers for the UI.
    let still_trusted = async {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            tick.tick().await;
            let t = sync.as_ref().and_then(|s| s.progress());
            if let Some(t) = &t {
                println!("{t}");
            }
            shared.status.update(|i| i.transfer = t);
            if s.gate.trusted(&fp).is_none() {
                bail!("{name} was removed from the paired devices");
            }
        }
    };
    let ctrl = async {
        loop {
            match read_ctrl(&mut recv).await? {
                Ctrl::KeyframeRequest => shared.force_idr.store(true, Ordering::Release),
                Ctrl::SecureAttention => {
                    let ok = crate::desktop::send_ctrl_alt_del();
                    println!("Ctrl+Alt+Del requested: {}", if ok { "sent" } else { "not possible here (needs ORBIT's always-on service)" });
                }
                Ctrl::RefInvalidate { from } => {
                    let mut pending = shared.invalidate.lock().unwrap();
                    // Several requests before the next frame: repair from the oldest loss.
                    *pending = Some(pending.map_or(from, |p| if from.wrapping_sub(p) as i32 <= 0 { from } else { p }));
                }
                Ctrl::Ping { client_us } => out_tx.send(Ctrl::Pong { client_us, server_us: now_us() })?,
                Ctrl::SelectMonitor { index } => {
                    // Screens may have been plugged in or out since the list was sent.
                    monitors = crate::capture::list_monitors().unwrap_or(monitors);
                    if let Some(m) = monitors.get(index as usize).cloned() {
                        if index as usize != current {
                            println!("showing screen {} ({}x{})", index + 1, m.width, m.height);
                            current = index as usize;
                            input.set_monitor(m.clone());
                            *shared.monitor.lock().unwrap() = m;
                            shared.switch.store(true, Ordering::Release);
                        }
                    }
                    out_tx.send(describe(&monitors, current))?;
                }
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
        r = still_trusted => {
            conn.close(1u32.into(), b"removed from paired devices");
            r
        }
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
    let (mut frames, mut bytes, mut keys, mut dropped, mut repairs) = (0u64, 0u64, 0u64, 0u64, 0u64);
    // Time spent waiting for the network to drain the send queue (per stats second).
    let mut net_wait = Duration::ZERO;
    let (mut enc_ms, mut bridge_ms) = (Dist::default(), Dist::default());
    let mut lost_prev = conn.stats().path.lost_packets;
    let mut sent_prev = conn.stats().path.sent_packets;
    let mut desktop_check = Instant::now();

    'pipeline: while !shared.stop.load(Ordering::Acquire) {
        // (Re)create the pipeline. Failures (device lost, driver reset, mode change) restart
        // it instead of ending the session.
        shared.switch.store(false, Ordering::Release);
        crate::desktop::follow_input_desktop();
        let mon = shared.monitor.lock().unwrap().clone();
        let mut p = match Pipeline::new(mon.clone(), s.fps, rate.video_bps()) {
            Ok(p) => {
                println!(
                    "video: {} {}x{} via {}, {} fps cap, {:.1} Mbit/s, {} reference frames{}",
                    mon.device_name,
                    mon.width,
                    mon.height,
                    p.capture_mode(),
                    s.fps,
                    rate.video_bps() as f64 / 1e6,
                    p.encoder_max_refs(),
                    if p.encoder_rfi() { "" } else { " (no repair support: losses need keyframes)" }
                );
                shared.status.update(|i| i.error = None);
                // Tell the viewer the size before the first frame (the new keyframe).
                let _ = shared.out.send(Ctrl::VideoFormat { width: mon.width as u16, height: mon.height as u16 });
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
        // (frame id, capture time = NVENC timestamp) of the frames sent since the last keyframe
        // that the encoder still holds as references, oldest first.
        let mut refs: std::collections::VecDeque<(u32, u64)> = std::collections::VecDeque::new();
        let max_refs = p.encoder().max_refs() as usize;
        while !shared.stop.load(Ordering::Acquire) {
            if shared.switch.load(Ordering::Acquire) {
                continue 'pipeline; // another screen: rebuild capture + encoder for it
            }
            // GDI capture gets no "desktop changed" error like Desktop Duplication does, so look
            // for a switch (lock screen, UAC; only visible as SYSTEM) every 250 ms.
            if p.capture_mode() == "gdi" && desktop_check.elapsed() >= Duration::from_millis(250) {
                desktop_check = Instant::now();
                if crate::desktop::follow_input_desktop() {
                    continue 'pipeline;
                }
            }
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
                    "[{}] send fps {fps:5.1} | {mbit:6.2} Mbit/s | rate {target:.1} Mbit/s ({}) | fec {fec}% | encode ms {enc} | {}key {keys} | repaired {repairs} | dropped {dropped} | net wait {wait_ms:.0} ms | rtt {rtt:.1} ms{} | lost pkts {lost}",
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
                (frames, bytes, keys, dropped, repairs, net_wait) = (0, 0, 0, 0, 0, Duration::ZERO);
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
            // A loss to repair: invalidate the lost frames and everything sent after them, so the
            // next frame is predicted from the newest frame the client still has. Possible while
            // that frame is still one of the encoder's references; otherwise a keyframe.
            let mut recovery_for = None;
            if let Some(from) = shared.invalidate.lock().unwrap().take() {
                let before = from.wrapping_sub(1);
                let usable = p.encoder().supports_rfi() && !need_idr && refs.iter().any(|&(id, _)| id == before);
                let mut ok = usable;
                if usable {
                    for &(id, ts) in refs.iter().filter(|(id, _)| id.wrapping_sub(from) as i32 >= 0) {
                        if let Err(err) = p.encoder().invalidate(ts) {
                            eprintln!("invalidate frame {id}: {err:#}");
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    refs.retain(|(id, _)| (id.wrapping_sub(from) as i32) < 0);
                    recovery_for = Some(from);
                } else {
                    shared.force_idr.store(true, Ordering::Release);
                }
            }
            let force = shared.force_idr.swap(false, Ordering::AcqRel) | need_idr;
            buf.clear();
            let e = match p.next(50, force, recovery_for.is_some(), &mut buf) {
                Ok(Some(e)) => e,
                Ok(None) => {
                    if force {
                        shared.force_idr.store(true, Ordering::Release); // not served yet
                    }
                    if let Some(from) = recovery_for {
                        shared.invalidate.lock().unwrap().get_or_insert(from); // not served yet
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
                if dropped == 0 {
                    // At most once a second (the counter resets with the stats).
                    println!(
                        "dropped a {} of {} KB: {} KB already queued (limit {} KB), {} KB free",
                        if e.keyframe { "keyframe" } else { "frame" },
                        wire / 1024,
                        backlog / 1024,
                        backlog_limit / 1024,
                        conn.datagram_send_buffer_space() / 1024
                    );
                }
                dropped += 1;
                need_idr = true;
                continue;
            }
            let recovery_for = recovery_for.filter(|_| !e.keyframe);
            let grams = match fragment(frame_id, e.capture_us, e.keyframe, recovery_for, &buf, max, rate.fec_percent()) {
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
            if e.keyframe {
                refs.clear(); // nothing before a keyframe can be referenced
            }
            refs.push_back((frame_id, e.capture_us));
            while refs.len() > max_refs {
                refs.pop_front();
            }
            repairs += recovery_for.is_some() as u64;
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
