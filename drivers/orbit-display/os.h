// os.h - the Windows / WDF / IddCx headers every driver file needs, in the order they need them.
// SPDX-License-Identifier: MIT
#pragma once

#ifndef NOMINMAX
#define NOMINMAX
#endif
// Microsoft's WDF headers trip two level-4 warnings that the WDK's own driver toolset turns off.
#pragma warning(push)
#pragma warning(disable : 4471 4324)
#include <windows.h>
#include <bugcodes.h>
#include <wudfwdm.h>
#include <wdf.h>
#include <iddcx.h>
#pragma warning(pop)

#include <avrt.h>
#include <d3d11.h>
#include <dxgi1_5.h>
#include <wrl/client.h>

namespace orbit {

/// Scoped exclusive hold of an SRWLOCK.
class Locked {
public:
    explicit Locked(SRWLOCK& lock) : lock_(lock) { AcquireSRWLockExclusive(&lock_); }
    ~Locked() { ReleaseSRWLockExclusive(&lock_); }
    Locked(const Locked&) = delete;
    Locked& operator=(const Locked&) = delete;

private:
    SRWLOCK& lock_;
};

}  // namespace orbit
