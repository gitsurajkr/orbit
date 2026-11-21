# rdesk

Low-latency remote desktop over Tailscale: Windows server with an NVIDIA GPU → Windows client.

```
server: DXGI capture ─► NVENC H.264 (sync, P1 ultra-low-latency, CBR, 1-frame VBV) ─► QUIC datagrams
client: QUIC datagrams ─► reassemble ─► GPU decode (Media Foundation/DXVA) ─► tearing flip-model present
input:  client window ─► mouse moves as datagrams, keys/buttons/wheel on a reliable stream ─► SendInput
```

## Build (on each PC)

1. Install Rust: https://rustup.rs (MSVC toolchain), plus **Visual Studio Build Tools** with the
   "Desktop development with C++" workload (Rust needs its linker).
2. `cargo build --release`

Binaries: `target\release\rdesk-server.exe`, `target\release\rdesk-client.exe`.
(LLVM is only needed to regenerate `server/src/nvenc/sys.rs`; it's committed.)

## Run

On the **server** (the PC with the RTX GPU), with Tailscale running:

```
rdesk-server --list                 # monitors and which GPU drives each
rdesk-server                        # listens on this PC's Tailscale IP, port 9000
rdesk-server --fps 120 --mbps 30    # higher frame rate / bitrate
```

Allow it through Windows Firewall when asked. If Windows put the Tailscale adapter under the
*Public* profile, allow public networks too (or set the adapter to Private).
Run it **as administrator** if you need to control elevated (admin) windows: Windows blocks
injected input into them otherwise.

On the **client** (laptop):

```
rdesk-client 100.x.y.z              # the server's Tailscale IP or MagicDNS name
rdesk-client my-pc --view-only      # watch without sending input
```

`Ctrl+Alt+Enter` toggles fullscreen. Close the window to disconnect.

Benchmark capture + encode alone on the server: `rdesk-server --bench`.

## Reading the stats

Server, once a second:
`send fps | Mbit/s | encode ms avg/p95/max | key (keyframes sent) | dropped | rtt | lost pkts`
- **dropped**: frames skipped because the network was behind by more than ~2 frames (they would
  only have added delay). Steady drops mean `--mbps` is above what the link carries.

Client, once a second:
`fps (recv) | Mbit/s | latency ms | decode ms | frame spread ms | rtt | lost skip keyreq`
- **latency**: server capture → client present, on synchronized clocks. Doesn't include the
  monitor's own scan-out delay.
- **frame spread**: first to last datagram of a frame. Big values mean frames are too large
  for the link.
- **lost / skip / keyreq**: frames lost in transit, deltas skipped while waiting for a keyframe,
  keyframe requests sent.

## Known limits (v1)

- No pairing or auth yet: anyone who can reach the port on your tailnet can connect. Tailscale
  ACLs are the access control for now. The certificate isn't verified.
- Fixed bitrate (no adaptive bitrate yet), no FEC: a lost packet costs one keyframe.
- One monitor (`--monitor N`); no clipboard; remote cursor shape isn't shown (the client's own
  arrow cursor is used, which is what makes the pointer feel instant).
- Win key, Alt+Tab, Ctrl+Esc are taken by the client's Windows, not forwarded.
- Hybrid laptops as *server*: if the screen is on the integrated GPU, frames are copied to the
  NVIDIA GPU through system memory (a few ms extra).
