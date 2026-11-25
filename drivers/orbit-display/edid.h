// edid.h - the identity card a monitor hands Windows (VESA EDID 1.4, 128 bytes).
// SPDX-License-Identifier: MIT
#pragma once

#include <array>
#include <cstddef>
#include <cstdint>

namespace orbit {

/// Windows shows this name in Settings > Display, and ORBIT finds its monitor by it.
inline constexpr char kMonitorName[] = "ORBIT Display";

struct Timing {
    uint32_t width, height, hz;
    uint32_t hFront, hSync, hBlank;  // pixels
    uint32_t vFront, vSync, vBlank;  // lines
    uint64_t pixelClockHz;
};

/// VESA CVT reduced blanking (v1) timing for a mode: what a real panel of that size would use.
Timing CvtReducedBlanking(uint32_t width, uint32_t height, uint32_t hz);

/// A complete EDID block for a monitor whose native mode is `width` x `height` @ `hz`.
std::array<uint8_t, 128> MakeEdid(uint32_t serial, uint32_t width, uint32_t height, uint32_t hz);

/// The serial number an EDID block carries (bytes 12..15), or 0.
uint32_t SerialOf(const uint8_t* edid, size_t size);

}  // namespace orbit
