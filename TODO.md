# ORBIT TODO

## Next
- [x] Auto-start sharing when Windows starts (SHARE tab checkbox; needs Windows auto sign-in).
- [ ] Publish the first release with the updater (v0.1.2's tag build never started; release v0.1.3).
- [ ] First real test 4070S workstation → laptop over Tailscale: record fps / latency / drops,
      verify colors and mouse/keyboard control.

## Later
- [ ] Adaptive bitrate (milestone 4): lower/raise bitrate from drops, loss and RTT
      (`Encoder::set_bitrate` already exists).
- [ ] Share the lock screen / UAC prompts (needs a Windows service running as SYSTEM).
- [ ] Pairing / auth (milestone 5): PIN on first connect, verify the server certificate.
- [ ] Clipboard sync.
- [ ] Multi-monitor switching from the viewer.
- [ ] Forward Win key / Alt+Tab (low-level keyboard hook in the viewer).
- [ ] Show the remote cursor shape (I-beam, resize arrows).
- [ ] FEC or intra-refresh so a lost packet doesn't cost a whole keyframe.
- [ ] Tray icon while sharing.
- [ ] Code-sign the exe/installer (removes the SmartScreen "unknown publisher" warning).
