# rdesk — my own remote desktop over Tailscale
Server: native Windows workstation (RTX 4070 Super). Client: my laptop.
Language: Rust. Network: Tailscale (WireGuard handles encryption for now).

## Milestones (do ONE at a time, I review before moving on)
1. JPEG screenshot stream over TCP (xcap capture, minifb viewer)
2. NVENC H.264 encode + hardware decode, QUIC transport (quinn)
3. Mouse/keyboard input back to server (SendInput)
4. Reconnect, drop old frames, adaptive bitrate
5. Auth/pairing, clipboard, multi-monitor

## Rules
- Explain the design before writing code; I want to understand every part
- Small commits, one feature each
- Always send the newest frame, never queue old ones
- Print fps / Mbit/s / latency stats so I can measure progress