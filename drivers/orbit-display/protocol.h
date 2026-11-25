// protocol.h - how ORBIT talks to the ORBIT Display driver.
//
// One control code, IOCTL_ORBIT_DISPLAY. Every request is an OrbitDisplayRequest and every reply
// an OrbitDisplayReply; `Op` says what to do. The Rust side (server/src/vdisplay.rs) mirrors these
// layouts byte for byte: change both together and bump ORBIT_DISPLAY_VERSION.
//
// Lifetime: a monitor lives in a slot. OPEN fills a free slot and starts its lease; KEEPALIVE
// renews the leases of every slot the caller's session owns; a slot whose lease runs out is closed
// by the driver itself. So if ORBIT dies, its monitor goes away within ORBIT_DISPLAY_LEASE_MS
// instead of staying around with windows on it that nobody can see.
//
// SPDX-License-Identifier: MIT
#pragma once

#include <guiddef.h>
#include <winioctl.h>

/// The device interface ORBIT opens: {E5A1F0C2-6B7D-4E38-A2C4-91D3B7F05E6A}.
inline constexpr GUID kOrbitDisplayInterface = {0xe5a1f0c2, 0x6b7d, 0x4e38, {0xa2, 0xc4, 0x91, 0xd3, 0xb7, 0xf0, 0x5e, 0x6a}};

#define ORBIT_DISPLAY_MAGIC 0x4442524Fu  // "ORBD" in memory order
#define ORBIT_DISPLAY_VERSION 1
#define ORBIT_DISPLAY_SLOTS 2
#define ORBIT_DISPLAY_LEASE_MS 5000

#define IOCTL_ORBIT_DISPLAY CTL_CODE(FILE_DEVICE_UNKNOWN, 0xB01, METHOD_BUFFERED, FILE_READ_ACCESS | FILE_WRITE_ACCESS)

enum OrbitDisplayOp : UINT16 {
    ORBIT_OP_OPEN = 1,       // add a monitor: Width x Height @ Hz, drawn by Gpu if given
    ORBIT_OP_CLOSE = 2,      // remove the monitor in Slot
    ORBIT_OP_KEEPALIVE = 3,  // renew every lease of Session
    ORBIT_OP_STATUS = 4,     // just report (OccupiedSlots)
};

/// Where a failed OPEN stopped (OrbitDisplayReply::Step).
enum OrbitDisplayStep : UINT32 {
    ORBIT_STEP_DONE = 0,
    ORBIT_STEP_REQUEST = 1,  // bad magic/version/op/size
    ORBIT_STEP_ADAPTER = 2,  // the virtual adapter isn't up yet
    ORBIT_STEP_SLOT = 3,     // no free slot
    ORBIT_STEP_CREATE = 4,   // IddCxMonitorCreate
    ORBIT_STEP_ARRIVE = 5,   // IddCxMonitorArrival
    ORBIT_STEP_CLOSE = 6,    // CLOSE: no such slot for this session
    ORBIT_STEP_CRASH = 7,    // an exception inside the driver (caught)
};

#pragma pack(push, 4)
struct OrbitDisplayRequest {
    UINT32 Magic;    // ORBIT_DISPLAY_MAGIC
    UINT16 Version;  // ORBIT_DISPLAY_VERSION
    UINT16 Op;       // OrbitDisplayOp
    UINT32 Slot;     // CLOSE
    UINT32 Width;    // OPEN
    UINT32 Height;   // OPEN
    UINT32 Hz;       // OPEN (0 = 60)
    UINT32 Serial;   // OPEN: same serial, same monitor to Windows (it remembers its settings)
    UINT32 GpuLow;   // OPEN: LUID of the GPU that should draw it (0/0 = Windows decides)
    INT32 GpuHigh;
    UINT32 SessionLow;  // random per ORBIT process; owns the slots it opens
    UINT32 SessionHigh;
};

struct OrbitDisplayReply {
    UINT32 Magic;
    INT32 Status;     // NTSTATUS of the operation (0 = success)
    UINT32 Step;      // OrbitDisplayStep where it failed (OPEN/CLOSE)
    UINT32 Slot;      // OPEN: the slot now holding the monitor
    UINT32 GpuSet;    // OPEN: 1 = the GPU was asked for (Windows treats it as a preference)
    UINT32 OccupiedSlots;  // bit i set = slot i holds a monitor
};
#pragma pack(pop)

static_assert(sizeof(OrbitDisplayRequest) == 44, "layout shared with Rust");
static_assert(sizeof(OrbitDisplayReply) == 24, "layout shared with Rust");
