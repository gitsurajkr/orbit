# ORBIT TODO

## Next
- [x] Auto-start sharing when Windows starts (SHARE tab checkbox; needs Windows auto sign-in).
- [x] Publish the first release with the updater (v0.1.3, then v0.1.4 with the UPDATE button).
- [ ] **Set up the sharing PC for zero-touch restarts** (one-time, on the PC to be controlled):
  - [ ] Install ORBIT v0.1.4+, SHARE tab → tick **START SHARING WHEN WINDOWS STARTS**
  - [ ] Auto sign-in: Microsoft Autologon (learn.microsoft.com/sysinternals/downloads/autologon)
        → enter Windows password → Enable
  - [ ] Tailscale tray → Preferences → **Run unattended**
  - [ ] Settings → Accounts → Sign-in options → require sign-in again → **Never**
  - [ ] Settings → System → Power → sleep → **Never** (screen off is fine)
  - [ ] Laptop only: keep it plugged in; Power Options → closing the lid → **Do nothing** (plugged in)
  - [ ] Test: restart the PC, touch nothing, connect from the other PC with ORBIT
- [ ] Workstation: log Windows Tailscale into the same account as the laptop (it isn't on the
      tailnet yet; `tailscale status` on the laptop should list it as **windows**).
- [ ] First real test 4070S workstation → laptop over Tailscale: record fps / latency / drops,
      verify colors and mouse/keyboard control.
- [ ] First real in-app update: on a PC with v0.1.3, click UPDATE → installs v0.1.4 and reopens.

## Later
- [ ] Show "TAILSCALE RELAY (city) · +xx ms" in ORBIT when the link to the other PC isn't
      direct (from `tailscale status --json`), so high latency explains itself.
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
