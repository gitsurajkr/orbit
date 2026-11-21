use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "orbit-client", about = "ORBIT client: shows and controls a remote PC")]
struct Args {
    /// Server address, e.g. 100.101.102.103 or my-pc (Tailscale name), optionally :port.
    server: String,
    /// Watch only: don't send mouse or keyboard to the server.
    #[arg(long)]
    view_only: bool,
}

fn main() {
    let args = Args::parse();
    if let Err(e) = client::run_viewer(&args.server, args.view_only) {
        eprintln!("orbit-client: {e:#}");
        std::process::exit(1);
    }
}
