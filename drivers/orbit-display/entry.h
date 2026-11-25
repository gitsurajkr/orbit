// entry.h - per-object storage WDF keeps for us.
// SPDX-License-Identifier: MIT
#pragma once

#include "os.h"

namespace orbit {
class VirtualAdapter;
}

/// On the WDF device: the adapter that device runs.
struct DeviceContext {
    orbit::VirtualAdapter* adapter;
};
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(DeviceContext, DeviceContextOf);

/// On each IddCx monitor: which adapter and slot it belongs to, so monitor callbacks need no lookup.
struct MonitorContext {
    orbit::VirtualAdapter* adapter;
    UINT32 slot;
};
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(MonitorContext, MonitorContextOf);
