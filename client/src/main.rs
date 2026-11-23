use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "orbit-client", about = "ORBIT client: shows and controls a remote PC")]
struct Args {
    /// Server address, e.g. 100.101.102.103 or my-pc (Tailscale name), optionally :port.
    server: String,
    /// Watch only: don't send mouse or keyboard to the server.
    #[arg(long)]
    view_only: bool,
    /// The 6-digit pairing code shown on the server (first connection from a device on another
    /// Tailscale account).
    #[arg(long)]
    code: Option<String>,
    /// Connect even though the server's ID changed since last time (e.g. ORBIT was reinstalled).
    #[arg(long)]
    accept_new_identity: bool,
    /// Don't sync the clipboard with the server.
    #[arg(long)]
    no_clipboard: bool,
}

fn main() {
    let args = Args::parse();
    let opts = client::ConnectOptions { code: args.code, accept_new_identity: args.accept_new_identity, clipboard: !args.no_clipboard && !args.view_only };
    if let Err(e) = client::run_viewer(&args.server, args.view_only, &opts) {
        eprintln!("orbit-client: {e:#}");
        std::process::exit(1);
    }
}
