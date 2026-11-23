//! Client networking (tokio): connect, check who answered, get let in (pairing), then three tasks:
//! - datagrams: reassemble video frames, gate deltas after a loss, request keyframes
//! - ctrl writer: keys/buttons/wheel/keyframe requests from the window, plus clock pings
//! - ctrl reader: pongs → clock sync, pointer shapes → window thread
//!
//! Complete frames go to the window thread through a channel, and a Win32 event wakes it up
//! immediately (no polling interval).

use anyhow::{bail, ensure, Context, Result};
use proto::clock::{now_us, ClockSync};
use proto::ctrl::{Ctrl, HEADER_LEN};
use proto::video::{keyframe_retry, Event, Frame, KeyframeGate, Reassembler};
use quinn::crypto::rustls::QuicClientConfig;
use quinn::{Connection, RecvStream, SendStream};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::SetEvent;

/// A Win32 event handle that may be signalled from any thread.
#[derive(Clone, Copy)]
pub struct WakeEvent(pub HANDLE);
unsafe impl Send for WakeEvent {}
unsafe impl Sync for WakeEvent {}
impl WakeEvent {
    pub fn signal(&self) {
        unsafe {
            let _ = SetEvent(self.0);
        }
    }
}

#[derive(Default)]
pub struct NetStats {
    pub bytes: AtomicU64,
    pub frames: AtomicU64,
    pub lost: AtomicU64,
    pub skipped: AtomicU64,
    pub keyframe_requests: AtomicU64,
    /// Frames rebuilt by FEC (they'd have been lost without it).
    pub recovered: AtomicU64,
}

pub struct Session {
    pub conn: Connection,
    pub ctrl: UnboundedSender<Ctrl>,
    pub clock: Arc<Mutex<ClockSync>>,
    pub stats: Arc<NetStats>,
    /// Set by the window thread when decoding failed: the gate then waits for a keyframe.
    pub decoder_failed: Arc<AtomicBool>,
    /// Pointer shape messages ([`Ctrl::Cursor`], [`Ctrl::CursorImage`]) for the window thread.
    pub cursor: mpsc::Receiver<Ctrl>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// How long a partly received frame may wait for its missing fragments.
const REASSEMBLY_TIMEOUT: Duration = Duration::from_millis(80);

/// Why the sharing PC didn't let us in, for the app to react to (ask for a code, warn).
#[derive(Debug)]
pub enum AuthError {
    /// Not paired. `code_active`: a code is showing on the sharing PC right now.
    NeedCode { code_active: bool },
    /// Refused, with the sharing PC's reason (e.g. wrong code).
    Denied(String),
    /// A different certificate than last time answers at this address.
    IdentityChanged { addr: String, expected: String, got: String },
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            AuthError::NeedCode { code_active: true } => write!(f, "not paired yet: type the 6-digit code shown on the other PC"),
            AuthError::NeedCode { code_active: false } => {
                write!(f, "not paired yet: on the other PC open ORBIT > SHARE > PAIR NEW DEVICE, then connect again and type the code it shows")
            }
            AuthError::Denied(r) => write!(f, "refused: {r}"),
            AuthError::IdentityChanged { addr, expected, got } => write!(
                f,
                "the PC at {addr} has a different ID than last time (now {got}, was {expected}). Expected if ORBIT was reinstalled there; otherwise don't connect"
            ),
        }
    }
}

impl std::error::Error for AuthError {}

/// How to connect: the pairing code, if the user typed one, and whether a changed ID was
/// accepted by the user.
#[derive(Clone, Debug, Default)]
pub struct ConnectOptions {
    pub code: Option<String>,
    pub accept_new_identity: bool,
}

/// Accepts any server certificate *during* the handshake (TLS still checks the server holds
/// its key); which certificate it is gets checked right after, against the known PCs.
#[derive(Debug)]
struct SkipVerify(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for SkipVerify {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &rustls::pki_types::CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &rustls::pki_types::CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn client_config(id: &auth::Identity) -> Result<quinn::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = rustls::pki_types::CertificateDer::from(id.cert_der.clone());
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(id.key_der.clone());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipVerify(provider)))
        .with_client_auth_cert(vec![cert], key.into())?;
    tls.alpn_protocols = vec![proto::ALPN.to_vec()];
    let mut cfg = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(1)));
    t.max_idle_timeout(Some(Duration::from_secs(5).try_into().unwrap()));
    t.initial_mtu(1200);
    t.mtu_discovery_config(None);
    t.datagram_receive_buffer_size(Some(4 << 20));
    t.datagram_send_buffer_size(64 * 1024);
    // Same congestion control as the server (see server/src/net.rs); here it only paces input.
    t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    cfg.transport_config(Arc::new(t));
    Ok(cfg)
}

async fn read_ctrl(r: &mut RecvStream) -> Result<Ctrl> {
    let mut h = [0u8; HEADER_LEN];
    r.read_exact(&mut h).await?;
    let (tag, len) = Ctrl::parse_header(h);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ctrl::decode(tag, &body)
}

/// The pairing code check, viewer side (see `auth::pake`). Errors if the code is wrong.
async fn pair_with_code(send: &mut SendStream, recv: &mut RecvStream, code: &str, my_fp: &auth::Fingerprint, server_fp: &auth::Fingerprint) -> Result<()> {
    let (state, msg) = auth::pake::start(code, my_fp, server_fp);
    write_ctrl(send, &Ctrl::PairStart { msg }).await?;
    let reply = match read_ctrl(recv).await? {
        Ctrl::PairReply { msg } => msg,
        Ctrl::Denied { reason } => return Err(AuthError::Denied(reason).into()),
        other => bail!("expected a pairing reply, got {other:?}"),
    };
    let key = state.finish(&reply)?;
    write_ctrl(send, &Ctrl::PairProof { proof: key.proof_from_viewer().to_vec() }).await?;
    match read_ctrl(recv).await? {
        Ctrl::PairProof { proof } if key.check_sharer(&proof) => Ok(()),
        // Only a fake sharing PC that doesn't know the code gets here.
        Ctrl::PairProof { .. } => Err(AuthError::Denied("the other PC couldn't prove it knows the code".into()).into()),
        Ctrl::Denied { reason } => Err(AuthError::Denied(reason).into()),
        other => bail!("expected a pairing proof, got {other:?}"),
    }
}

async fn write_ctrl(w: &mut SendStream, c: &Ctrl) -> Result<()> {
    let mut buf = Vec::with_capacity(32);
    c.encode(&mut buf);
    w.write_all(&buf).await?;
    Ok(())
}

pub async fn connect(server: SocketAddr, opts: &ConnectOptions, frames_tx: mpsc::Sender<Frame>, wake: WakeEvent) -> Result<Session> {
    let id = auth::Identity::load_or_create().context("this PC's ID certificate")?;
    let bind: SocketAddr = if server.is_ipv4() { "0.0.0.0:0".parse()? } else { "[::]:0".parse()? };
    let mut endpoint = quinn::Endpoint::client(bind)?;
    endpoint.set_default_client_config(client_config(&id)?);
    let conn = tokio::time::timeout(Duration::from_secs(5), endpoint.connect(server, "orbit")?)
        .await
        .with_context(|| format!("no answer from {server} (is ORBIT sharing on that PC and reachable over Tailscale?)"))??;

    // Is this the same PC as last time at this address?
    let server_fp = conn
        .peer_identity()
        .and_then(|a| a.downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>().ok())
        .and_then(|c| c.first().map(|c| auth::fingerprint(c)))
        .context("the other PC sent no certificate")?;
    let addr_key = server.ip().to_canonical().to_string();
    let mut known = auth::KnownPcs::load();
    if let auth::Check::Changed { expected } = known.check(&addr_key, &server_fp) {
        if !opts.accept_new_identity {
            conn.close(0u32.into(), b"identity changed");
            return Err(AuthError::IdentityChanged { addr: addr_key, expected: auth::short(&expected), got: auth::short(&server_fp) }.into());
        }
    }

    let (mut send, mut recv) = conn.open_bi().await?;
    write_ctrl(&mut send, &Ctrl::ClientHello { version: proto::VERSION, name: auth::device_name() }).await?;
    let hello = async {
        let mut paired = false;
        loop {
            match read_ctrl(&mut recv).await? {
                Ctrl::ServerHello { version, width, height, fps } => {
                    ensure!(version == proto::VERSION, "server protocol {version}, client {}", proto::VERSION);
                    return Ok((width as u32, height as u32, fps as u32));
                }
                Ctrl::PairRequired { code_active } => match (&opts.code, code_active, paired) {
                    (Some(code), true, false) => {
                        pair_with_code(&mut send, &mut recv, code, &id.fingerprint(), &server_fp).await?;
                        paired = true;
                    }
                    _ => return Err(AuthError::NeedCode { code_active }.into()),
                },
                Ctrl::Denied { reason } => return Err(AuthError::Denied(reason).into()),
                other => bail!("expected hello, got {other:?}"),
            }
        }
    };
    let hello: Result<(u32, u32, u32)> = tokio::time::timeout(Duration::from_secs(20), hello).await.context("no hello from server")?;
    let (width, height, fps) = match hello {
        Ok(h) => h,
        Err(e) => {
            conn.close(0u32.into(), b"not let in");
            return Err(e);
        }
    };
    // Let in: remember this PC's ID for next time.
    if let Err(e) = known.remember(&addr_key, &server_fp) {
        eprintln!("couldn't save known PCs: {e:#}");
    }

    let clock = Arc::new(Mutex::new(ClockSync::default()));
    let stats = Arc::new(NetStats::default());
    let decoder_failed = Arc::new(AtomicBool::new(false));
    let (ctrl_tx, mut ctrl_rx) = unbounded_channel::<Ctrl>();

    // ctrl writer + clock pings
    tokio::spawn(async move {
        let mut ping = tokio::time::interval(Duration::from_millis(500));
        loop {
            let msg = tokio::select! {
                m = ctrl_rx.recv() => match m { Some(m) => m, None => break },
                _ = ping.tick() => Ctrl::Ping { client_us: now_us() },
            };
            if write_ctrl(&mut send, &msg).await.is_err() {
                break;
            }
        }
    });

    // ctrl reader: pongs, pointer shapes
    let (cursor_tx, cursor_rx) = mpsc::channel::<Ctrl>();
    {
        let clock = clock.clone();
        tokio::spawn(async move {
            while let Ok(m) = read_ctrl(&mut recv).await {
                match m {
                    Ctrl::Pong { client_us, server_us } => clock.lock().unwrap().add(client_us, server_us, now_us()),
                    m @ (Ctrl::Cursor { .. } | Ctrl::CursorImage { .. }) => {
                        if cursor_tx.send(m).is_err() {
                            break;
                        }
                        wake.signal();
                    }
                    _ => {}
                }
            }
        });
    }

    // video datagrams
    {
        let (conn, stats, failed, ctrl) = (conn.clone(), stats.clone(), decoder_failed.clone(), ctrl_tx.clone());
        tokio::spawn(async move {
            let mut reasm = Reassembler::new(REASSEMBLY_TIMEOUT);
            let mut gate = KeyframeGate::new();
            let mut tick = tokio::time::interval(Duration::from_millis(5));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut events = Vec::new();
            loop {
                tokio::select! {
                    d = conn.read_datagram() => {
                        let Ok(d) = d else { break };
                        if d.first() == Some(&proto::DG_VIDEO) {
                            stats.bytes.fetch_add(d.len() as u64, Ordering::Relaxed);
                            let _ = reasm.push(&d, Instant::now(), &mut events);
                        }
                    }
                    _ = tick.tick() => reasm.expire(Instant::now(), &mut events),
                }
                if failed.swap(false, Ordering::AcqRel) {
                    gate.on_lost();
                }
                for ev in events.drain(..) {
                    match ev {
                        Event::Frame(f) => {
                            stats.frames.fetch_add(1, Ordering::Relaxed);
                            if f.recovered {
                                stats.recovered.fetch_add(1, Ordering::Relaxed);
                            }
                            if gate.admit(f.keyframe) {
                                if frames_tx.send(f).is_err() {
                                    return;
                                }
                                wake.signal();
                            } else {
                                stats.skipped.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Event::Lost { from, to } => {
                            stats.lost.fetch_add(to.wrapping_sub(from) as u64 + 1, Ordering::Relaxed);
                            gate.on_lost();
                        }
                    }
                }
                if gate.should_request(Instant::now(), keyframe_retry(conn.rtt())) {
                    stats.keyframe_requests.fetch_add(1, Ordering::Relaxed);
                    let _ = ctrl.send(Ctrl::KeyframeRequest);
                }
            }
            wake.signal(); // let the window notice the connection ended
        });
    }

    Ok(Session { conn, ctrl: ctrl_tx, clock, stats, decoder_failed, cursor: cursor_rx, width, height, fps })
}
