// SPDX-License-Identifier: MIT
#include "frame_pump.h"

using Microsoft::WRL::ComPtr;

namespace orbit {

FramePump::FramePump(IDDCX_SWAPCHAIN swapChain, LUID gpu, HANDLE frameReady)
    : swapChain_(swapChain), gpu_(gpu), frameReady_(frameReady), quit_(CreateEventW(nullptr, TRUE, FALSE, nullptr)) {
    thread_ = std::thread(&FramePump::Main, this);
}

FramePump::~FramePump() {
    SetEvent(quit_);
    if (thread_.joinable()) thread_.join();
    CloseHandle(quit_);
}

void FramePump::Main() {
    // Register as multimedia work so the scheduler keeps this thread responsive under load.
    DWORD mmcssTask = 0;
    const HANDLE mmcss = AvSetMmThreadCharacteristicsW(L"Distribution", &mmcssTask);
    Pump();
    WdfObjectDelete(reinterpret_cast<WDFOBJECT>(swapChain_));
    if (mmcss) AvRevertMmThreadCharacteristics(mmcss);
}

void FramePump::Pump() {
    // IddCx needs a D3D device on the GPU it composes on before it hands out frames.
    ComPtr<IDXGIFactory5> factory;
    ComPtr<IDXGIAdapter1> adapter;
    ComPtr<ID3D11Device> device;
    ComPtr<IDXGIDevice> dxgi;
    if (FAILED(CreateDXGIFactory2(0, IID_PPV_ARGS(&factory))) ||
        FAILED(factory->EnumAdapterByLuid(gpu_, IID_PPV_ARGS(&adapter))) ||
        FAILED(D3D11CreateDevice(adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                                 nullptr, 0, D3D11_SDK_VERSION, &device, nullptr, nullptr)) ||
        FAILED(device.As(&dxgi))) {
        return;
    }
    IDARG_IN_SWAPCHAINSETDEVICE use = {};
    use.pDevice = dxgi.Get();
    if (FAILED(IddCxSwapChainSetDevice(swapChain_, &use))) return;

    const HANDLE wakeUps[2] = {frameReady_, quit_};
    for (;;) {
        IDARG_OUT_RELEASEANDACQUIREBUFFER frame = {};
        const HRESULT got = IddCxSwapChainReleaseAndAcquireBuffer(swapChain_, &frame);
        if (got == E_PENDING) {
            // No new frame yet: sleep until there is one (or we're told to stop).
            if (WaitForMultipleObjects(2, wakeUps, FALSE, INFINITE) != WAIT_OBJECT_0) return;
            continue;
        }
        // Any other failure: Windows abandoned this swap chain (mode change, monitor gone) and
        // will assign a new one if the monitor stays.
        if (FAILED(got)) return;
        if (FAILED(IddCxSwapChainFinishedProcessingFrame(swapChain_))) return;
        if (WaitForSingleObject(quit_, 0) == WAIT_OBJECT_0) return;
    }
}

}  // namespace orbit
