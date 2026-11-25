// entry.cpp - DriverEntry, the WDF device, and the IddCx callbacks, which hand everything to the
// VirtualAdapter.
// SPDX-License-Identifier: MIT
#include "entry.h"

#include "edid.h"
#include "virtual_adapter.h"

#include <atomic>
#include <new>

namespace {

// IddCx calls a few callbacks with nothing that leads back to our device (e.g. parsing an EDID).
// Each driver host runs one ORBIT Display device (UmdfHostProcessSharing is off in the INF), so
// one pointer is enough.
std::atomic<orbit::VirtualAdapter*> g_adapter{nullptr};

/// What Windows needs to know about one mode, with CVT reduced-blanking totals.
DISPLAYCONFIG_VIDEO_SIGNAL_INFO Signal(const orbit::Mode& m) {
    const orbit::Timing t = orbit::CvtReducedBlanking(m.width, m.height, m.hz);
    const UINT32 totalW = m.width + t.hBlank;
    const UINT32 totalH = m.height + t.vBlank;
    DISPLAYCONFIG_VIDEO_SIGNAL_INFO signal{};
    signal.activeSize = {m.width, m.height};
    signal.totalSize = {totalW, totalH};
    signal.pixelRate = static_cast<UINT64>(totalW) * totalH * m.hz;
    signal.hSyncFreq = {totalH * m.hz, 1};  // lines per second
    signal.vSyncFreq = {m.hz, 1};
    signal.AdditionalSignalInfo.videoStandard = 255;  // "other": no broadcast standard
    signal.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
    return signal;
}

/// IddCx's two-call convention for mode lists: with a zero-sized buffer, only report how many;
/// with one too small, fail; otherwise fill it.
template <typename T, typename Fill>
NTSTATUS ReportModes(const std::vector<orbit::Mode>& modes, UINT capacity, T* buffer, UINT& count, Fill fill) {
    count = static_cast<UINT>(modes.size());
    if (capacity == 0) return STATUS_SUCCESS;
    if (capacity < count) return STATUS_BUFFER_TOO_SMALL;
    for (UINT i = 0; i < count; i++) fill(buffer[i], Signal(modes[i]));
    return STATUS_SUCCESS;
}

orbit::VirtualAdapter* AdapterOf(IDDCX_MONITOR monitor) {
    return MonitorContextOf(monitor)->adapter;
}

NTSTATUS OnD0Entry(WDFDEVICE device, WDF_POWER_DEVICE_STATE) {
    return DeviceContextOf(device)->adapter->PowerUp();
}

NTSTATUS OnAdapterInitFinished(IDDCX_ADAPTER, const IDARG_IN_ADAPTER_INIT_FINISHED* args) {
    if (orbit::VirtualAdapter* a = g_adapter.load()) a->Started(args->AdapterInitStatus);
    return STATUS_SUCCESS;
}

NTSTATUS OnParseMonitorDescription(const IDARG_IN_PARSEMONITORDESCRIPTION* in, IDARG_OUT_PARSEMONITORDESCRIPTION* out) {
    orbit::VirtualAdapter* a = g_adapter.load();
    if (a == nullptr || in->MonitorDescription.Type != IDDCX_MONITOR_DESCRIPTION_TYPE_EDID) return STATUS_INVALID_PARAMETER;
    const UINT32 serial = orbit::SerialOf(static_cast<const uint8_t*>(in->MonitorDescription.pData),
                                          in->MonitorDescription.DataSize);
    out->PreferredMonitorModeIdx = 0;
    return ReportModes(a->ModesBySerial(serial), in->MonitorModeBufferInputCount, in->pMonitorModes,
                       out->MonitorModeBufferOutputCount, [](IDDCX_MONITOR_MODE& m, const DISPLAYCONFIG_VIDEO_SIGNAL_INFO& s) {
                           m = {};
                           m.Size = sizeof m;
                           m.Origin = IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR;
                           m.MonitorVideoSignalInfo = s;
                       });
}

NTSTATUS OnGetDefaultModes(IDDCX_MONITOR monitor, const IDARG_IN_GETDEFAULTDESCRIPTIONMODES* in,
                           IDARG_OUT_GETDEFAULTDESCRIPTIONMODES* out) {
    // Only asked for monitors without an EDID (ours always have one); answer sensibly anyway.
    out->PreferredMonitorModeIdx = 0;
    return ReportModes(AdapterOf(monitor)->ModesOf(monitor), in->DefaultMonitorModeBufferInputCount,
                       in->pDefaultMonitorModes, out->DefaultMonitorModeBufferOutputCount,
                       [](IDDCX_MONITOR_MODE& m, const DISPLAYCONFIG_VIDEO_SIGNAL_INFO& s) {
                           m = {};
                           m.Size = sizeof m;
                           m.Origin = IDDCX_MONITOR_MODE_ORIGIN_DRIVER;
                           m.MonitorVideoSignalInfo = s;
                       });
}

NTSTATUS OnQueryTargetModes(IDDCX_MONITOR monitor, const IDARG_IN_QUERYTARGETMODES* in, IDARG_OUT_QUERYTARGETMODES* out) {
    return ReportModes(AdapterOf(monitor)->ModesOf(monitor), in->TargetModeBufferInputCount, in->pTargetModes,
                       out->TargetModeBufferOutputCount, [](IDDCX_TARGET_MODE& m, const DISPLAYCONFIG_VIDEO_SIGNAL_INFO& s) {
                           m = {};
                           m.Size = sizeof m;
                           m.TargetVideoSignalInfo.targetVideoSignalInfo = s;
                           // Every vsync is presented; IddCx rejects a target mode with divider 0.
                           m.TargetVideoSignalInfo.targetVideoSignalInfo.AdditionalSignalInfo.vSyncFreqDivider = 1;
                       });
}

NTSTATUS OnCommitModes(IDDCX_ADAPTER, const IDARG_IN_COMMITMODES*) {
    return STATUS_SUCCESS;  // no hardware to program
}

NTSTATUS OnAssignSwapChain(IDDCX_MONITOR monitor, const IDARG_IN_SETSWAPCHAIN* args) {
    AdapterOf(monitor)->SwapChainAssigned(monitor, *args);
    return STATUS_SUCCESS;
}

NTSTATUS OnUnassignSwapChain(IDDCX_MONITOR monitor) {
    AdapterOf(monitor)->SwapChainUnassigned(monitor);
    return STATUS_SUCCESS;
}

VOID OnDeviceIoControl(WDFDEVICE device, WDFREQUEST request, size_t outLength, size_t inLength, ULONG code) {
    if (code != IOCTL_ORBIT_DISPLAY) {
        WdfRequestComplete(request, STATUS_INVALID_DEVICE_REQUEST);
        return;
    }
    if (inLength < sizeof(OrbitDisplayRequest) || outLength < sizeof(OrbitDisplayReply)) {
        WdfRequestComplete(request, STATUS_BUFFER_TOO_SMALL);
        return;
    }
    void* in = nullptr;
    void* out = nullptr;
    NTSTATUS status = WdfRequestRetrieveInputBuffer(request, sizeof(OrbitDisplayRequest), &in, nullptr);
    if (NT_SUCCESS(status)) status = WdfRequestRetrieveOutputBuffer(request, sizeof(OrbitDisplayReply), &out, nullptr);
    if (!NT_SUCCESS(status)) {
        WdfRequestComplete(request, status);
        return;
    }
    // Buffered I/O: input and output share one buffer, so copy the request out before replying.
    const OrbitDisplayRequest asked = *static_cast<const OrbitDisplayRequest*>(in);
    OrbitDisplayReply reply = {};
    try {
        DeviceContextOf(device)->adapter->Execute(asked, reply);
    } catch (...) {
        // Nothing may throw out of a driver callback: it would take the whole driver host down.
        reply.Magic = ORBIT_DISPLAY_MAGIC;
        reply.Status = STATUS_INTERNAL_ERROR;
        reply.Step = ORBIT_STEP_CRASH;
    }
    *static_cast<OrbitDisplayReply*>(out) = reply;
    // The request itself succeeded; what happened is in the reply.
    WdfRequestCompleteWithInformation(request, STATUS_SUCCESS, sizeof reply);
}

VOID OnDeviceCleanup(WDFOBJECT object) {
    DeviceContext* context = DeviceContextOf(static_cast<WDFDEVICE>(object));
    orbit::VirtualAdapter* expected = context->adapter;
    g_adapter.compare_exchange_strong(expected, nullptr);
    delete context->adapter;
    context->adapter = nullptr;
}

NTSTATUS OnDeviceAdd(WDFDRIVER, PWDFDEVICE_INIT init) {
    WDF_PNPPOWER_EVENT_CALLBACKS power;
    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&power);
    power.EvtDeviceD0Entry = OnD0Entry;
    WdfDeviceInitSetPnpPowerEventCallbacks(init, &power);

    IDD_CX_CLIENT_CONFIG callbacks;
    IDD_CX_CLIENT_CONFIG_INIT(&callbacks);
    callbacks.EvtIddCxDeviceIoControl = OnDeviceIoControl;
    callbacks.EvtIddCxAdapterInitFinished = OnAdapterInitFinished;
    callbacks.EvtIddCxAdapterCommitModes = OnCommitModes;
    callbacks.EvtIddCxParseMonitorDescription = OnParseMonitorDescription;
    callbacks.EvtIddCxMonitorGetDefaultDescriptionModes = OnGetDefaultModes;
    callbacks.EvtIddCxMonitorQueryTargetModes = OnQueryTargetModes;
    callbacks.EvtIddCxMonitorAssignSwapChain = OnAssignSwapChain;
    callbacks.EvtIddCxMonitorUnassignSwapChain = OnUnassignSwapChain;
    NTSTATUS status = IddCxDeviceInitConfig(init, &callbacks);
    if (!NT_SUCCESS(status)) return status;

    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, DeviceContext);
    attributes.EvtCleanupCallback = OnDeviceCleanup;
    WDFDEVICE device = nullptr;
    status = WdfDeviceCreate(&init, &attributes, &device);
    if (!NT_SUCCESS(status)) return status;
    status = IddCxDeviceInitialize(device);
    if (!NT_SUCCESS(status)) return status;

    auto* adapter = new (std::nothrow) orbit::VirtualAdapter(device);
    if (adapter == nullptr) return STATUS_INSUFFICIENT_RESOURCES;
    DeviceContextOf(device)->adapter = adapter;
    g_adapter.store(adapter);
    // Publish the control interface last, once requests can be served.
    return WdfDeviceCreateDeviceInterface(device, &kOrbitDisplayInterface, nullptr);
}

}  // namespace

extern "C" DRIVER_INITIALIZE DriverEntry;

extern "C" NTSTATUS DriverEntry(PDRIVER_OBJECT driver, PUNICODE_STRING registryPath) {
    WDF_DRIVER_CONFIG config;
    WDF_DRIVER_CONFIG_INIT(&config, OnDeviceAdd);
    return WdfDriverCreate(driver, registryPath, WDF_NO_OBJECT_ATTRIBUTES, &config, WDF_NO_HANDLE);
}
