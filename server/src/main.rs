use server::{capture, net, session, video};

use anyhow::{Context, Result};
use clap::Parser;
use proto::stats::{mbit_per_s, Dist, Ticker};
use std::io::Write;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(name = "orbit-server", about = "ORBIT server: streams this PC's screen")]
struct Args {
    /// List monitors and which GPU drives each, then exit.
    #[arg(long)]
    list: bool,
    /// Capture + encode locally without networking, printing stats (and optionally --dump).
    #[arg(long)]
    bench: bool,
    /// With --bench: write the raw H.264 stream to this file (play with `ffplay file.h264`).
    #[arg(long)]
    dump: Option<std::path::PathBuf>,
    /// Monitor index from --list.
    #[arg(long, default_value_t = 0)]
    monitor: usize,
    /// Frame rate cap (also the encoder's rate-control frame rate).
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// Video bitrate in Mbit/s.
    #[arg(long, default_value_t = 20.0)]
    mbps: f64,
    /// Address to listen on. Default 0.0.0.0:9000; only Tailscale (and loopback) peers are accepted.
    #[arg(long)]
    bind: Option<std::net::SocketAddr>,
}

fn main() -> Result<()> {
    // Without per-monitor DPI awareness, Desktop Duplication refuses to start on scaled displays.
    server::dpi_aware();
    let args = Args::parse();
    let monitors = capture::list_monitors()?;
    if args.list {
        for (i, m) in monitors.iter().enumerate() {
            println!("[{i}] {} {}x{} at ({},{}) on '{}' (vendor {:04x})", m.device_name, m.width, m.height, m.left, m.top, m.adapter_name, m.vendor);
        }
        return Ok(());
    }
    let mon = monitors.get(args.monitor).cloned().with_context(|| format!("no monitor {} (see --list)", args.monitor))?;
    if args.bench {
        return bench(&args, mon);
    }
    let bind = args.bind.unwrap_or_else(net::default_bind);
    match net::tailscale_ip() {
        Some(ip) => println!("this PC's Tailscale IP: {ip} (type it on the other PC)"),
        None => println!("Tailscale isn't connected yet; ORBIT accepts connections as soon as it is"),
    }
    let settings = session::Settings { monitor: mon.clone(), fps: args.fps, bitrate_bps: (args.mbps * 1e6) as u32 };
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    rt.block_on(async {
        let endpoint = net::endpoint(bind)?;
        println!(
            "orbit-server listening on {bind} | {} {}x{} on '{}' | {} fps cap | {} Mbit/s",
            mon.device_name, mon.width, mon.height, mon.adapter_name, args.fps, args.mbps
        );
        session::serve_until_stopped(endpoint, bind, settings, Default::default(), Default::default()).await
    })
}

fn bench(args: &Args, mon: capture::MonitorInfo) -> Result<()> {
    let mut p = video::Pipeline::new(mon, args.fps, (args.mbps * 1e6) as u32)?;
    let m = p.monitor().clone();
    println!("capturing {} {}x{} on '{}' via {}, {} fps cap, {} Mbit/s", m.device_name, m.width, m.height, m.adapter_name, p.capture_mode(), args.fps, args.mbps);
    let mut dump = args.dump.as_ref().map(std::fs::File::create).transpose()?;
    let interval = Duration::from_secs_f64(1.0 / args.fps as f64);
    let mut buf = Vec::with_capacity(1 << 20);
    let (mut frames, mut bytes, mut keys) = (0u64, 0u64, 0u64);
    let mut enc_ms = Dist::default();
    let mut bridge_ms = Dist::default();
    let mut size_kb = Dist::default();
    let mut ticker = Ticker::new(Duration::from_secs(1));
    let mut first = true;
    loop {
        let started = Instant::now();
        buf.clear();
        if let Some(e) = p.next(100, first, &mut buf)? {
            first = false;
            frames += 1;
            bytes += buf.len() as u64;
            keys += e.keyframe as u64;
            enc_ms.add(e.encode_ms);
            bridge_ms.add(e.bridge_ms);
            size_kb.add(buf.len() as f64 / 1024.0);
            if let Some(f) = dump.as_mut() {
                f.write_all(&buf)?;
            }
            if let Some(rest) = interval.checked_sub(started.elapsed()) {
                std::thread::sleep(rest);
            }
        }
        if let Some(secs) = ticker.tick(Instant::now()) {
            println!(
                "fps {:5.1} | {:6.2} Mbit/s | key {} | encode ms avg/p95/max {} | bridge ms {} | KB/frame {}",
                frames as f64 / secs,
                mbit_per_s(bytes, secs),
                keys,
                enc_ms.take(),
                if p.bridged() { bridge_ms.take() } else { bridge_ms = Dist::default(); "-".into() },
                size_kb.take()
            );
            (frames, bytes, keys) = (0, 0, 0);
        }
    }
}
