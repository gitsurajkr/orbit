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

## Unattended: share automatically after every restart

On the sharing PC, in ORBIT's SHARE tab, tick **START SHARING WHEN WINDOWS STARTS**. From then on
ORBIT opens minimized at every Windows sign-in and starts sharing by itself (retrying every 5 s
until Tailscale and the network are up). It also resumes sharing after an update.

ORBIT needs a signed-in, unlocked desktop (Windows doesn't let normal apps capture the sign-in
screen). For restarts with nobody at the PC, also set up:

1. **Windows signs in automatically.** Easiest and safest: Microsoft's free
   [Autologon](https://learn.microsoft.com/sysinternals/downloads/autologon) tool (stores the
   password encrypted). Run it, enter the account and password, click Enable.
   (Only do this on a PC in a place you trust: anyone at the keyboard gets in.)
2. **Tailscale connects before sign-in:** Tailscale tray icon → Preferences → **Run unattended**.
3. **Don't let it lock or sleep:** Settings → Accounts → Sign-in options → "If you've been away,
   when should Windows require you to sign in again?" → **Never**; Settings → System → Power →
   Screen and sleep → sleep **Never** (turning the screen off is fine).

## Updates

Every ORBIT checks GitHub Releases at startup and every 30 minutes. When a newer version is out,
a white banner appears: **UPDATE** downloads the installer, verifies its SHA-256, installs it
silently (one admin prompt) and reopens ORBIT. Nothing installs without that click.

To publish a new version (from a clean, committed tree):

```
.\release.ps1 0.1.2
```

It bumps the version, tags `v0.1.2` and pushes; the GitHub workflow builds `ORBIT-Setup.exe` and
publishes the release. Plain pushes to `main` don't trigger updates, only version tags do.

## Build from source

1. Install Rust: https://rustup.rs (MSVC toolchain), plus **Visual Studio Build Tools** with the
   "Desktop development with C++" workload (Rust needs its linker).
2. `cargo build --release` → `target
elease\orbit.exe` (also `orbit-server.exe` /
   `orbit-client.exe`, command-line versions that print stats every second).

LLVM is only needed to regenerate `server/src/nvenc/sys.rs`; it's committed.

Command-line use:

```
orbit-server --list                 # monitors and which GPU drives each
orbit-server --fps 120 --mbps 30    # share (UDP 9000; only Tailscale peers are accepted)
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

- No pairing or auth yet: ORBIT accepts connections only from Tailscale addresses, so anyone on
  your tailnet can connect. Tailscale ACLs are the access control for now. The certificate isn't
  verified.
- Latency depends on Tailscale finding a **direct** path. If `tailscale status` shows the other PC
  with `relay "bom"` (or another city) instead of an address, every packet goes through
  Tailscale's relay server and adds tens of ms. Carrier-grade NAT (an ISP address starting with
  100.64-100.127) is the usual cause; IPv6 on both networks or the same Wi-Fi usually fixes it.
- Adaptive bitrate from RTT, loss and send-queue waits (the BITRATE setting is the ceiling); no FEC yet: a lost packet costs one keyframe.
- One monitor (`--monitor N`); no clipboard; remote cursor shape isn't shown (the client's own
  arrow cursor is used, which is what makes the pointer feel instant).
- Win key, Alt+Tab, Ctrl+Esc are taken by the client's Windows, not forwarded.
- The sharing PC must stay **unlocked**: Windows doesn't let normal apps capture the lock screen
  or UAC prompts (that needs a SYSTEM service; planned). ORBIT waits and resumes on unlock.
- Hybrid laptops as *server*: if the screen is on the integrated GPU, frames are copied to the
  NVIDIA GPU through system memory (a few ms extra).
