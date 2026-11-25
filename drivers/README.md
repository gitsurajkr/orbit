# ORBIT Display (virtual monitor driver)

A PC with no monitor attached has nothing the GPU draws, so screen capture gets no image. ORBIT
Display is a small Windows driver that gives such a PC a monitor on demand: ORBIT asks for one
when someone connects and the PC has no active display, shares it at full speed, and removes it
afterwards.

- **User-mode** (IddCx, the Indirect Display Driver model): a bug can stop the driver, not crash Windows.
- **On demand only**: no monitor exists until ORBIT opens one; with ORBIT closed there is none.
- **Leases**: ORBIT renews each monitor's lease every second. If ORBIT dies, the driver closes the
  monitor itself within 5 s, so no invisible screen is left for windows to land on.
- **Only SYSTEM and administrators** can open its control interface (the ORBIT service runs as SYSTEM).
- **The encoder's GPU draws it** when ORBIT names one, so capture and encoding need no copy.

## Files

| File | What |
|---|---|
| `orbit-display/protocol.h` | The control interface (one IOCTL; OPEN / CLOSE / KEEPALIVE / STATUS) |
| `orbit-display/virtual_adapter.*` | The adapter: two monitor slots, leases, requests |
| `orbit-display/frame_pump.*` | Keeps each monitor's desktop flowing |
| `orbit-display/edid.*` | The monitor's EDID, with VESA CVT reduced-blanking timings |
| `orbit-display/entry.cpp` | DriverEntry and the WDF/IddCx callbacks |
| `orbit-display/OrbitDisplay.inf` | Installs the driver for the device `Root\OrbitDisplay` |
| `orbit-display/tests/edid_test.cpp` | EDID/CVT checks against VESA reference values |

## Build

```powershell
drivers\build.ps1 -Version 0.3.11
```

Needs Visual Studio 2022 or its Build Tools with the C++ workload. The WDK and Windows SDK come
from NuGet (`packages.config`); nothing else is installed. The result, in `drivers\out\x64`, is
unsigned: the ORBIT installer signs it on the PC it installs on.

The lease and on-the-PC signing ideas came from studying Annex
(github.com/NRanjan-17/annex); the code here is ORBIT's own.
