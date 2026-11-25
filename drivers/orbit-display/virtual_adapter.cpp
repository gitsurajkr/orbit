// SPDX-License-Identifier: MIT
#include "virtual_adapter.h"

#include "entry.h"

#include <algorithm>

namespace orbit {

namespace {

constexpr UINT64 Join(UINT32 low, UINT32 high) {
    return (static_cast<UINT64>(high) << 32) | low;
}

bool SameLuid(LUID a, LUID b) {
    return a.LowPart == b.LowPart && a.HighPart == b.HighPart;
}

/// Native mode first, then smaller standard sizes of the same shape class (so a user can still
/// pick one in Settings), each at the native rate and at 60 Hz.
std::vector<Mode> ModeList(const Mode& native) {
    static constexpr Mode kLadder[] = {
        {3840, 2160, 0}, {3440, 1440, 0}, {2560, 1600, 0}, {2560, 1440, 0}, {1920, 1200, 0},
        {1920, 1080, 0}, {1680, 1050, 0}, {1600, 900, 0},  {1440, 900, 0},  {1366, 768, 0},
        {1280, 800, 0},  {1280, 720, 0},  {1024, 768, 0},
    };
    std::vector<Mode> sizes{native};
    for (const Mode& m : kLadder) {
        const bool fits = m.width <= native.width && m.height <= native.height;
        const bool listed = std::any_of(sizes.begin(), sizes.end(),
                                        [&](const Mode& s) { return s.width == m.width && s.height == m.height; });
        if (fits && !listed) sizes.push_back(m);
    }
    std::vector<Mode> modes;
    for (const Mode& s : sizes) {
        modes.push_back({s.width, s.height, native.hz});
        if (native.hz != 60) modes.push_back({s.width, s.height, 60});
    }
    return modes;
}

}  // namespace

VirtualAdapter::VirtualAdapter(WDFDEVICE device)
    : device_(device), reaperQuit_(CreateEventW(nullptr, TRUE, FALSE, nullptr)) {
    reaper_ = std::thread([this] {
        while (WaitForSingleObject(reaperQuit_, 500) == WAIT_TIMEOUT) ReapExpiredLeases();
    });
}

VirtualAdapter::~VirtualAdapter() {
    SetEvent(reaperQuit_);
    if (reaper_.joinable()) reaper_.join();
    CloseHandle(reaperQuit_);
}

NTSTATUS VirtualAdapter::PowerUp() {
    if (adapter_ != nullptr) return STATUS_SUCCESS;

    IDDCX_ENDPOINT_VERSION version = {};
    version.Size = sizeof version;
    version.MajorVer = 1;

    IDDCX_ADAPTER_CAPS caps = {};
    caps.Size = sizeof caps;
    caps.MaxMonitorsSupported = ORBIT_DISPLAY_SLOTS;
    caps.EndPointDiagnostics.Size = sizeof caps.EndPointDiagnostics;
    caps.EndPointDiagnostics.TransmissionType = IDDCX_TRANSMISSION_TYPE_OTHER;
    caps.EndPointDiagnostics.GammaSupport = IDDCX_FEATURE_IMPLEMENTATION_NONE;
    caps.EndPointDiagnostics.pEndPointFriendlyName = L"ORBIT virtual display";
    caps.EndPointDiagnostics.pEndPointManufacturerName = L"ORBIT";
    caps.EndPointDiagnostics.pEndPointModelName = L"ORBIT Display";
    caps.EndPointDiagnostics.pHardwareVersion = &version;
    caps.EndPointDiagnostics.pFirmwareVersion = &version;

    IDARG_IN_ADAPTER_INIT in = {};
    in.WdfDevice = device_;
    in.pCaps = &caps;
    IDARG_OUT_ADAPTER_INIT out = {};
    const NTSTATUS status = IddCxAdapterInitAsync(&in, &out);
    if (NT_SUCCESS(status)) adapter_ = out.AdapterObject;
    return status;
}

void VirtualAdapter::Started(NTSTATUS status) {
    InterlockedExchange(&ready_, NT_SUCCESS(status) ? 1 : 0);
}

void VirtualAdapter::Execute(const OrbitDisplayRequest& request, OrbitDisplayReply& reply) {
    reply.Magic = ORBIT_DISPLAY_MAGIC;
    if (request.Magic != ORBIT_DISPLAY_MAGIC || request.Version != ORBIT_DISPLAY_VERSION) {
        reply.Status = STATUS_REVISION_MISMATCH;
        reply.Step = ORBIT_STEP_REQUEST;
    } else {
        switch (request.Op) {
            case ORBIT_OP_OPEN:
                Open(request, reply);
                break;
            case ORBIT_OP_CLOSE:
                Close(request, reply);
                break;
            case ORBIT_OP_KEEPALIVE:
                KeepAlive(Join(request.SessionLow, request.SessionHigh));
                break;
            case ORBIT_OP_STATUS:
                break;
            default:
                reply.Status = STATUS_INVALID_PARAMETER;
                reply.Step = ORBIT_STEP_REQUEST;
                break;
        }
    }
    reply.OccupiedSlots = Occupied();
}

bool VirtualAdapter::PreferGpu(LUID gpu) {
    if (gpu.LowPart == 0 && gpu.HighPart == 0) return false;
    if (!SameLuid(gpu, gpu_)) {
        IDARG_IN_ADAPTERSETRENDERADAPTER in = {};
        in.PreferredRenderAdapter = gpu;
        IddCxAdapterSetRenderAdapter(adapter_, &in);  // a preference: Windows may still pick another
        gpu_ = gpu;
    }
    return true;
}

void VirtualAdapter::Open(const OrbitDisplayRequest& request, OrbitDisplayReply& reply) {
    if (InterlockedCompareExchange(&ready_, 0, 0) == 0) {
        reply.Status = STATUS_DEVICE_NOT_READY;
        reply.Step = ORBIT_STEP_ADAPTER;
        return;
    }
    const bool sane = request.Width >= 640 && request.Height >= 480 && request.Width <= 7680 &&
                      request.Height <= 4320 && request.Hz <= 240;
    if (!sane) {
        reply.Status = STATUS_INVALID_PARAMETER;
        reply.Step = ORBIT_STEP_REQUEST;
        return;
    }
    // Choosing the GPU only affects swap chains made later, so it comes first.
    LUID gpu = {};
    gpu.LowPart = request.GpuLow;
    gpu.HighPart = request.GpuHigh;
    reply.GpuSet = PreferGpu(gpu) ? 1 : 0;

    // Reserve a slot first: while the monitor is created and announced below, IddCx calls back
    // (modes, swap chain) and those callbacks must already find it.
    UINT32 index = ORBIT_DISPLAY_SLOTS;
    {
        Locked hold(lock_);
        for (UINT32 i = 0; i < ORBIT_DISPLAY_SLOTS; i++) {
            if (slots_[i].state == State::Free) {
                index = i;
                break;
            }
        }
        if (index == ORBIT_DISPLAY_SLOTS) {
            reply.Status = STATUS_TOO_MANY_NODES;
            reply.Step = ORBIT_STEP_SLOT;
            return;
        }
        Slot& s = slots_[index];
        s.state = State::Arriving;
        s.session = Join(request.SessionLow, request.SessionHigh);
        s.leaseEnd = GetTickCount64() + ORBIT_DISPLAY_LEASE_MS;
        s.serial = request.Serial;
        s.native = {request.Width & ~1u, request.Height & ~1u, request.Hz ? request.Hz : 60};
        s.edid = MakeEdid(s.serial, s.native.width, s.native.height, s.native.hz);
    }

    // The slot's EDID stays put (the slot is reserved) while IddCx reads it.
    IDDCX_MONITOR_INFO info = {};
    info.Size = sizeof info;
    info.MonitorType = DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL;
    info.ConnectorIndex = index;
    info.MonitorDescription.Size = sizeof info.MonitorDescription;
    info.MonitorDescription.Type = IDDCX_MONITOR_DESCRIPTION_TYPE_EDID;
    info.MonitorDescription.DataSize = static_cast<UINT>(slots_[index].edid.size());
    info.MonitorDescription.pData = slots_[index].edid.data();
    // Container ID: fixed ORBIT prefix + the serial, so the same serial is the same "device".
    info.MonitorContainerId = {0x9d0c4e27, 0x31a8, 0x4f6b, {0xb5, 0x1e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00}};
    for (int i = 0; i < 4; i++) info.MonitorContainerId.Data4[4 + i] = static_cast<UCHAR>(request.Serial >> (8 * i));

    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, MonitorContext);
    IDARG_IN_MONITORCREATE create = {};
    create.ObjectAttributes = &attributes;
    create.pMonitorInfo = &info;
    IDARG_OUT_MONITORCREATE created = {};
    NTSTATUS status = IddCxMonitorCreate(adapter_, &create, &created);
    if (NT_SUCCESS(status)) {
        MonitorContext* context = MonitorContextOf(created.MonitorObject);
        context->adapter = this;
        context->slot = index;
        {
            Locked hold(lock_);
            slots_[index].monitor = created.MonitorObject;
        }
        IDARG_OUT_MONITORARRIVAL arrived = {};
        status = IddCxMonitorArrival(created.MonitorObject, &arrived);
        if (!NT_SUCCESS(status)) {
            reply.Step = ORBIT_STEP_ARRIVE;
            WdfObjectDelete(reinterpret_cast<WDFOBJECT>(created.MonitorObject));
        }
    } else {
        reply.Step = ORBIT_STEP_CREATE;
    }

    Locked hold(lock_);
    Slot& s = slots_[index];
    if (!NT_SUCCESS(status)) {
        s = Slot{};
        reply.Status = status;
        return;
    }
    s.state = State::Live;
    s.leaseEnd = GetTickCount64() + ORBIT_DISPLAY_LEASE_MS;
    reply.Status = STATUS_SUCCESS;
    reply.Step = ORBIT_STEP_DONE;
    reply.Slot = index;
}

void VirtualAdapter::Close(const OrbitDisplayRequest& request, OrbitDisplayReply& reply) {
    Leaving leaving;
    {
        Locked hold(lock_);
        const UINT64 session = Join(request.SessionLow, request.SessionHigh);
        if (request.Slot >= ORBIT_DISPLAY_SLOTS || slots_[request.Slot].state != State::Live ||
            slots_[request.Slot].session != session) {
            reply.Status = STATUS_NOT_FOUND;
            reply.Step = ORBIT_STEP_CLOSE;
            return;
        }
        Slot& s = slots_[request.Slot];
        leaving.monitor = s.monitor;
        leaving.pump = std::move(s.pump);
        s = Slot{};
    }
    Remove(std::move(leaving));
    reply.Status = STATUS_SUCCESS;
    reply.Step = ORBIT_STEP_DONE;
}

void VirtualAdapter::KeepAlive(UINT64 session) {
    Locked hold(lock_);
    const ULONGLONG until = GetTickCount64() + ORBIT_DISPLAY_LEASE_MS;
    for (Slot& s : slots_)
        if (s.state != State::Free && s.session == session) s.leaseEnd = until;
}

UINT32 VirtualAdapter::Occupied() {
    Locked hold(lock_);
    UINT32 bits = 0;
    for (UINT32 i = 0; i < ORBIT_DISPLAY_SLOTS; i++)
        if (slots_[i].state != State::Free) bits |= 1u << i;
    return bits;
}

void VirtualAdapter::ReapExpiredLeases() {
    std::vector<Leaving> expired;
    {
        Locked hold(lock_);
        const ULONGLONG now = GetTickCount64();
        for (Slot& s : slots_) {
            // Arriving slots belong to an OPEN still in progress; it finishes or fails on its own.
            if (s.state == State::Live && now > s.leaseEnd) {
                expired.push_back({s.monitor, std::move(s.pump)});
                s = Slot{};
            }
        }
    }
    for (Leaving& l : expired) Remove(std::move(l));
}

void VirtualAdapter::Remove(Leaving leaving) {
    // Stop the pump first (it deletes the swap chain), then tell Windows the monitor is gone. Both
    // can call back into the adapter, which is why this runs without the lock.
    leaving.pump.reset();
    if (leaving.monitor != nullptr) IddCxMonitorDeparture(leaving.monitor);
}

std::vector<Mode> VirtualAdapter::ModesBySerial(uint32_t serial) {
    Locked hold(lock_);
    for (const Slot& s : slots_)
        if (s.state != State::Free && s.serial == serial) return ModeList(s.native);
    return ModeList({1920, 1080, 60});
}

std::vector<Mode> VirtualAdapter::ModesOf(IDDCX_MONITOR monitor) {
    const UINT32 index = MonitorContextOf(monitor)->slot;
    Locked hold(lock_);
    if (index < ORBIT_DISPLAY_SLOTS && slots_[index].state != State::Free) return ModeList(slots_[index].native);
    return ModeList({1920, 1080, 60});
}

void VirtualAdapter::SwapChainAssigned(IDDCX_MONITOR monitor, const IDARG_IN_SETSWAPCHAIN& args) {
    const UINT32 index = MonitorContextOf(monitor)->slot;
    std::unique_ptr<FramePump> replaced;
    {
        Locked hold(lock_);
        if (index < ORBIT_DISPLAY_SLOTS && slots_[index].monitor == monitor) {
            replaced = std::move(slots_[index].pump);
            slots_[index].pump = std::make_unique<FramePump>(args.hSwapChain, args.RenderAdapterLuid,
                                                             args.hNextSurfaceAvailable);
            return;  // `replaced` stops after the lock is released
        }
    }
    // A monitor we no longer track: the swap chain must still be deleted.
    WdfObjectDelete(reinterpret_cast<WDFOBJECT>(args.hSwapChain));
}

void VirtualAdapter::SwapChainUnassigned(IDDCX_MONITOR monitor) {
    const UINT32 index = MonitorContextOf(monitor)->slot;
    std::unique_ptr<FramePump> stopping;
    {
        Locked hold(lock_);
        if (index < ORBIT_DISPLAY_SLOTS && slots_[index].monitor == monitor) stopping = std::move(slots_[index].pump);
    }
    // `stopping` is destroyed here, outside the lock.
}

}  // namespace orbit
