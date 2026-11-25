// edid.cpp - EDID 1.4 for the virtual monitor, written from the VESA E-EDID 1.4 and CVT 1.2 specs.
// SPDX-License-Identifier: MIT
#include "edid.h"

#include <algorithm>
#include <cmath>
#include <cstring>

namespace orbit {

namespace {

// CVT 1.2, reduced blanking v1 constants.
constexpr double kMinVBlankSeconds = 460e-6;  // RB_MIN_V_BLANK
constexpr uint32_t kHBlankRb = 160;           // RB_H_BLANK
constexpr uint32_t kHSyncRb = 32;             // RB_H_SYNC
constexpr uint32_t kVFrontRb = 3;             // RB_V_FPORCH
constexpr uint32_t kMinVBackPorch = 6;        // MIN_V_BPORCH
constexpr uint32_t kClockStepHz = 250000;     // CLOCK_STEP: 0.25 MHz

/// CVT's V_SYNC width encodes the aspect ratio.
uint32_t VSyncLines(uint32_t w, uint32_t h) {
    auto is = [&](uint32_t a, uint32_t b) { return w * b == h * a; };
    if (is(4, 3)) return 4;
    if (is(16, 9)) return 5;
    if (is(16, 10)) return 6;
    if (is(5, 4) || is(15, 9)) return 7;
    return 10;  // any other aspect ratio
}

/// Writes one 18-byte "display descriptor" holding text (name or serial): tag, then up to 13
/// characters, then a line feed and space padding, as E-EDID 1.4 section 3.10.3 describes.
void TextDescriptor(uint8_t* d, uint8_t tag, const char* text) {
    std::memset(d, 0, 18);
    d[3] = tag;
    size_t n = 0;
    for (; text[n] && n < 13; n++) d[5 + n] = static_cast<uint8_t>(text[n]);
    if (n < 13) {
        d[5 + n++] = '\n';
        while (n < 13) d[5 + n++] = ' ';
    }
}

/// The 18-byte detailed timing descriptor (E-EDID 1.4 section 3.10.2).
void TimingDescriptor(uint8_t* d, const Timing& t, uint32_t widthMm, uint32_t heightMm) {
    const uint32_t clock10k = static_cast<uint32_t>(t.pixelClockHz / 10000);
    d[0] = clock10k & 0xFF;
    d[1] = (clock10k >> 8) & 0xFF;
    d[2] = t.width & 0xFF;
    d[3] = t.hBlank & 0xFF;
    d[4] = static_cast<uint8_t>((((t.width >> 8) & 0x0F) << 4) | ((t.hBlank >> 8) & 0x0F));
    d[5] = t.height & 0xFF;
    d[6] = t.vBlank & 0xFF;
    d[7] = static_cast<uint8_t>((((t.height >> 8) & 0x0F) << 4) | ((t.vBlank >> 8) & 0x0F));
    d[8] = t.hFront & 0xFF;
    d[9] = t.hSync & 0xFF;
    d[10] = static_cast<uint8_t>(((t.vFront & 0x0F) << 4) | (t.vSync & 0x0F));
    d[11] = static_cast<uint8_t>((((t.hFront >> 8) & 0x03) << 6) | (((t.hSync >> 8) & 0x03) << 4) |
                                 (((t.vFront >> 4) & 0x03) << 2) | ((t.vSync >> 4) & 0x03));
    d[12] = widthMm & 0xFF;
    d[13] = heightMm & 0xFF;
    d[14] = static_cast<uint8_t>((((widthMm >> 8) & 0x0F) << 4) | ((heightMm >> 8) & 0x0F));
    d[15] = 0;  // no border
    d[16] = 0;
    d[17] = 0x1A;  // non-interlaced, digital separate sync, H+ V- (CVT-RB polarity)
}

}  // namespace

Timing CvtReducedBlanking(uint32_t width, uint32_t height, uint32_t hz) {
    Timing t{};
    t.width = width;
    t.height = height;
    t.hz = hz ? hz : 60;
    t.vFront = kVFrontRb;
    t.vSync = VSyncLines(width, height);
    // Horizontal period such that the vertical blanking lasts at least kMinVBlankSeconds.
    const double hPeriod = (1.0 / t.hz - kMinVBlankSeconds) / height;  // seconds per line
    uint32_t vBlank = static_cast<uint32_t>(std::floor(kMinVBlankSeconds / hPeriod)) + 1;  // VBI_LINES
    vBlank = std::max(vBlank, kVFrontRb + t.vSync + kMinVBackPorch);
    t.vBlank = vBlank;
    t.hBlank = kHBlankRb;
    t.hSync = kHSyncRb;
    t.hFront = kHBlankRb / 2 - kHSyncRb;  // 48: sync centred in the back half of the blank
    const double total = static_cast<double>(width + t.hBlank) * (height + t.vBlank) * t.hz;
    t.pixelClockHz = static_cast<uint64_t>(std::floor(total / kClockStepHz)) * kClockStepHz;
    return t;
}

std::array<uint8_t, 128> MakeEdid(uint32_t serial, uint32_t width, uint32_t height, uint32_t hz) {
    std::array<uint8_t, 128> e{};
    // 0..7: fixed header.
    static constexpr uint8_t kHeader[8] = {0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00};
    std::memcpy(&e[0], kHeader, sizeof kHeader);

    // 8..9: manufacturer "ORB", three letters of 5 bits each ('A' = 1), big-endian.
    const uint16_t id = static_cast<uint16_t>((('O' - 'A' + 1) << 10) | (('R' - 'A' + 1) << 5) | ('B' - 'A' + 1));
    e[8] = static_cast<uint8_t>(id >> 8);
    e[9] = static_cast<uint8_t>(id);
    // 10..11: product code, little-endian.
    e[10] = 0x01;
    e[11] = 0x0B;
    // 12..15: serial, little-endian.
    for (int i = 0; i < 4; i++) e[12 + i] = static_cast<uint8_t>(serial >> (8 * i));
    e[16] = 0xFF;  // "model year" flag: byte 17 is a model year
    e[17] = 2026 - 1990;
    e[18] = 1;     // EDID version 1
    e[19] = 4;     // revision 4

    // 20: digital input, 8 bits per primary, interface not defined.
    e[20] = 0x80 | (2 << 4);
    // 21..22: screen size in cm, as a 96 DPI panel of this resolution would be (100% scaling).
    const uint32_t widthMm = static_cast<uint32_t>(width * 25.4 / 96.0 + 0.5);
    const uint32_t heightMm = static_cast<uint32_t>(height * 25.4 / 96.0 + 0.5);
    auto centimetres = [](uint32_t mm) { return static_cast<uint8_t>(mm >= 2545 ? 255 : (mm + 5) / 10); };
    e[21] = centimetres(widthMm);
    e[22] = centimetres(heightMm);
    e[23] = 220 - 100;  // gamma 2.20, stored as (gamma * 100) - 100
    // 24: features: RGB 4:4:4, sRGB is the default colour space, the first timing is native,
    // continuous frequency (the range descriptor below holds the limits).
    e[24] = 0x04 | 0x02 | 0x01;

    // 25..34: colour primaries and white point of sRGB (10-bit CIE xy values, packed).
    static constexpr uint8_t kSrgb[10] = {0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54};
    std::memcpy(&e[25], kSrgb, sizeof kSrgb);
    // 35..37: no established timings. 38..53: eight unused standard timing slots.
    for (int i = 38; i < 54; i += 2) {
        e[i] = 0x01;
        e[i + 1] = 0x01;
    }

    // 54..125: four 18-byte descriptors. First: the native mode, if a DTD can express it (its
    // pixel clock field tops out at 655.35 MHz); bigger modes still come through IddCx's mode list.
    Timing native = CvtReducedBlanking(width, height, hz);
    if (native.pixelClockHz / 10000 > 0xFFFF) native = CvtReducedBlanking(width, height, 30);
    if (native.pixelClockHz / 10000 > 0xFFFF) native = CvtReducedBlanking(1920, 1080, 60);
    TimingDescriptor(&e[54], native, widthMm, heightMm);
    TextDescriptor(&e[72], 0xFC, kMonitorName);

    // Range limits (tag 0xFD): 24-240 Hz vertical, 1-255 kHz horizontal, 2550 MHz pixel clock max.
    uint8_t* r = &e[90];
    std::memset(r, 0, 18);
    r[3] = 0xFD;
    r[5] = 24;
    r[6] = 240;
    r[7] = 1;
    r[8] = 255;
    r[9] = 255;
    r[10] = 0x01;  // range limits only, no timing formula
    r[11] = '\n';
    for (int i = 12; i < 18; i++) r[i] = ' ';

    char serialText[9];
    static constexpr char kHex[] = "0123456789ABCDEF";
    for (int i = 0; i < 8; i++) serialText[i] = kHex[(serial >> (28 - 4 * i)) & 0xF];
    serialText[8] = '\0';
    TextDescriptor(&e[108], 0xFF, serialText);

    e[126] = 0;  // no extension blocks
    uint32_t sum = 0;
    for (int i = 0; i < 127; i++) sum += e[i];
    e[127] = static_cast<uint8_t>((256 - (sum & 0xFF)) & 0xFF);  // the block sums to 0 (mod 256)
    return e;
}

uint32_t SerialOf(const uint8_t* edid, size_t size) {
    if (edid == nullptr || size < 16) return 0;
    uint32_t serial = 0;
    for (int i = 3; i >= 0; i--) serial = (serial << 8) | edid[12 + i];
    return serial;
}

}  // namespace orbit
