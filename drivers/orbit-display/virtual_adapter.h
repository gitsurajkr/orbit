// virtual_adapter.h - the ORBIT virtual display adapter: two monitor slots, opened and closed by
// ORBIT, each kept alive by a lease (see protocol.h).
// SPDX-License-Identifier: MIT
#pragma once

#include "edid.h"
#include "frame_pump.h"
#include "os.h"
#include "protocol.h"

#include <array>
#include <memory>
#include <vector>

namespace orbit {

/// One mode a monitor offers: size, refresh rate and the timing that goes with it.
struct Mode {
    uint32_t width, height, hz;
};

class VirtualAdapter {
public:
    explicit VirtualAdapter(WDFDEVICE device);
    ~VirtualAdapter();
    VirtualAdapter(const VirtualAdapter&) = delete;
    VirtualAdapter& operator=(const VirtualAdapter&) = delete;

    /// Starts IddCx's adapter (called on each power-up; only the first call does anything).
    NTSTATUS PowerUp();
    /// IddCx finished starting the adapter.
    void Started(NTSTATUS status);

    /// Runs one control request.
    void Execute(const OrbitDisplayRequest& request, OrbitDisplayReply& reply);

    /// The modes of the monitor with this EDID serial / this IddCx monitor (native first).
    std::vector<Mode> ModesBySerial(uint32_t serial);
    std::vector<Mode> ModesOf(IDDCX_MONITOR monitor);

    /// Windows gave a monitor a swap chain (or took it back).
    void SwapChainAssigned(IDDCX_MONITOR monitor, const IDARG_IN_SETSWAPCHAIN& args);
    void SwapChainUnassigned(IDDCX_MONITOR monitor);

private:
    enum class State { Free, Arriving, Live };

    struct Slot {
        State state = State::Free;
        UINT64 session = 0;
        ULONGLONG leaseEnd = 0;  // GetTickCount64() after which the slot is reclaimed
        UINT32 serial = 0;
        Mode native{};
        std::array<uint8_t, 128> edid{};
        IDDCX_MONITOR monitor = nullptr;
        std::unique_ptr<FramePump> pump;
    };

    /// What a slot held when it was emptied; torn down outside the lock.
    struct Leaving {
        IDDCX_MONITOR monitor = nullptr;
        std::unique_ptr<FramePump> pump;
    };

    void Open(const OrbitDisplayRequest& request, OrbitDisplayReply& reply);
    void Close(const OrbitDisplayRequest& request, OrbitDisplayReply& reply);
    void KeepAlive(UINT64 session);
    UINT32 Occupied();
    /// Asks Windows to draw new swap chains on `gpu`; true if it was asked (0/0 = leave it to Windows).
    bool PreferGpu(LUID gpu);
    void ReapExpiredLeases();
    static void Remove(Leaving leaving);

    WDFDEVICE device_;
    IDDCX_ADAPTER adapter_ = nullptr;
    volatile LONG ready_ = 0;
    LUID gpu_{};  // GPU the adapter was last asked to render on

    SRWLOCK lock_ = SRWLOCK_INIT;  // guards slots_; never held while calling into IddCx
    std::array<Slot, ORBIT_DISPLAY_SLOTS> slots_;

    HANDLE reaperQuit_;
    std::thread reaper_;
};

}  // namespace orbit
