# ORBIT

Low-latency remote desktop over Tailscale: Windows server with an NVIDIA GPU → Windows client.

```
server: DXGI capture ─► NVENC H.264 (sync, P1 ultra-low-latency, CBR, 1-frame VBV) ─► QUIC datagrams
client: QUIC datagrams ─► reassemble ─► GPU decode (Media Foundation/DXVA) ─► tearing flip-model present
input:  client window ─► mouse moves as datagrams, keys/buttons/wheel on a reliable stream ─► SendInput
```

## Use it: one exe on both PCs

Copy **`orbit.exe`** to both PCs (nothing to install: no runtimes, works on any Windows 10/11 x64).
Both PCs need Tailscale running and signed in to the same tailnet.

1. On the PC you want to control (needs an NVIDIA GPU): open ORBIT → **SHARE** → hit the big
   button. It shows this PC's Tailscale IP (click it to copy).
2. On the other PC: open ORBIT → **CONNECT** → type that IP (or the Tailscale name) → hit the big
   button or Enter. The remote screen opens in its own window; close it to come back.

- Allow ORBIT through Windows Firewall when asked on the sharing PC. If Windows put the Tailscale
  adapter under *Public*, allow public networks too (or set the adapter to Private).
- Run ORBIT **as administrator** on the sharing PC if you need to control admin windows
  (Windows blocks injected input into them otherwise).
- In the viewer: `Ctrl+Alt+Enter` toggles fullscreen. The title bar shows fps and latency.
- Settings and recent addresses live in `%APPDATA%\ORBIT\orbit.cfg`.

## Build from source

1. Install Rust: https://rustup.rs (MSVC toolchain), plus **Visual Studio Build Tools** with the
   "Desktop development with C++" workload (Rust needs its linker).
2. `cargo build --release` → `targetelease\orbit.exe` (also `orbit-server.exe` /
   `orbit-client.exe`, command-line versions that print stats every second).

LLVM is only needed to regenerate `server/src/nvenc/sys.rs`; it's committed.

Command-line use:

```
orbit-server --list                 # monitors and which GPU drives each
orbit-server --fps 120 --mbps 30    # share (listens on this PC's Tailscale IP, port 9000)
orbit-server --bench                # capture + encode only, no network
orbit-client 100.x.y.z [--view-only]
```

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
- The sharing PC must stay **unlocked**: Windows doesn't let normal apps capture the lock screen
  or UAC prompts (that needs a SYSTEM service; planned). ORBIT waits and resumes on unlock.
- Hybrid laptops as *server*: if the screen is on the integrated GPU, frames are copied to the
  NVIDIA GPU through system memory (a few ms extra).
