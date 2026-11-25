// frame_pump.h - keeps a virtual monitor's desktop flowing.
// SPDX-License-Identifier: MIT
#pragma once

#include "os.h"

#include <atomic>
#include <thread>

namespace orbit {

/// Windows composes the monitor's desktop into a swap chain and waits until the driver has taken
/// each frame. ORBIT reads the pixels itself (Desktop Duplication on that monitor), so the pump
/// only takes every frame and returns it at once. It owns the swap chain: when the pump stops
/// (destructor), the swap chain is deleted, as IddCx requires.
class FramePump {
public:
    FramePump(IDDCX_SWAPCHAIN swapChain, LUID gpu, HANDLE frameReady);
    ~FramePump();
    FramePump(const FramePump&) = delete;
    FramePump& operator=(const FramePump&) = delete;

private:
    void Main();
    /// Returns when the swap chain is gone or the pump is asked to stop.
    void Pump();

    IDDCX_SWAPCHAIN swapChain_;
    LUID gpu_;
    HANDLE frameReady_;  // owned by IddCx
    HANDLE quit_;        // ours
    std::thread thread_;
};

}  // namespace orbit
