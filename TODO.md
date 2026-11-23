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
- [x] Show DIRECT / RELAY (city) / PEER RELAY next to the stats on both sides
      (from `tailscale status --json`, every 5 s), so high latency explains itself.
- [x] Adaptive bitrate (milestone 4): server/src/rate.rs, from RTT, loss and net wait.
- [ ] Tune adaptive bitrate on the real relayed link (recovery after a deep cut is slow).
- [ ] Share the lock screen / UAC prompts (needs a Windows service running as SYSTEM).
- [ ] Pairing / auth (milestone 5): PIN on first connect, verify the server certificate.
- [ ] Clipboard sync.
- [ ] Multi-monitor switching from the viewer.
- [x] Forward Win key / Alt+Tab / Ctrl+Esc (low-level keyboard hook in the viewer).
- [x] Show the remote cursor shape (standard shapes by id, app pointers as images; protocol v3).
- [x] FEC: Reed-Solomon parity per frame, 10–40 % following loss (protocol v2).
- [ ] NVENC reference-frame invalidation, so a frame FEC can't save costs no keyframe either.
- [x] Tray icon (left click opens, right click menu, status tooltip; ─ and ✕ go to the tray).
- [ ] Code-sign the exe/installer (removes the SmartScreen "unknown publisher" warning).
