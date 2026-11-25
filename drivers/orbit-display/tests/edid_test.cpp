// edid_test.cpp - checks the EDID and CVT code against the VESA specs. Built and run by build.ps1.
// SPDX-License-Identifier: MIT
#include "../edid.h"

#include <algorithm>
#include <cstdio>
#include <cstring>

static int failures = 0;

#define CHECK(cond)                                                     \
    do {                                                                \
        if (!(cond)) {                                                  \
            std::printf("FAILED %s:%d: %s\n", __FILE__, __LINE__, #cond); \
            failures++;                                                 \
        }                                                               \
    } while (0)

/// Reference values from the VESA CVT 1.2 timing generator (reduced blanking).
static void CvtMatchesVesa() {
    const orbit::Timing fhd = orbit::CvtReducedBlanking(1920, 1080, 60);
    CHECK(fhd.width + fhd.hBlank == 2080);
    CHECK(fhd.height + fhd.vBlank == 1111);
    CHECK(fhd.pixelClockHz == 138500000);
    CHECK(fhd.hFront == 48 && fhd.hSync == 32);
    CHECK(fhd.vFront == 3 && fhd.vSync == 5);

    const orbit::Timing qhd = orbit::CvtReducedBlanking(2560, 1440, 60);
    CHECK(qhd.width + qhd.hBlank == 2720);
    CHECK(qhd.height + qhd.vBlank == 1481);
    CHECK(qhd.pixelClockHz == 241500000);

    CHECK(orbit::CvtReducedBlanking(1600, 1200, 60).vSync == 4);  // 4:3
    CHECK(orbit::CvtReducedBlanking(1920, 1200, 60).vSync == 6);  // 16:10
}

static void EdidIsWellFormed() {
    const auto e = orbit::MakeEdid(0x1234ABCD, 1920, 1080, 60);
    static const unsigned char header[8] = {0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00};
    CHECK(std::equal(header, header + 8, e.begin()));
    unsigned sum = 0;
    for (unsigned char b : e) sum += b;
    CHECK(sum % 256 == 0);
    CHECK(e[18] == 1);  // version 1
    CHECK(e[19] == 4);  // revision 4
    // Manufacturer "ORB".
    const unsigned id = (e[8] << 8) | e[9];
    CHECK(((id >> 10) & 31) == 'O' - 'A' + 1 && ((id >> 5) & 31) == 'R' - 'A' + 1 && (id & 31) == 'B' - 'A' + 1);
    CHECK(orbit::SerialOf(e.data(), e.size()) == 0x1234ABCD);
    // Detailed timing: 138.50 MHz, 1920 x 1080 active.
    CHECK(e[54] + (e[55] << 8) == 13850);
    CHECK(e[56] + ((e[58] >> 4) << 8) == 1920);
    CHECK(e[59] + ((e[61] >> 4) << 8) == 1080);
    // Monitor name descriptor.
    CHECK(e[75] == 0xFC && std::memcmp(&e[77], "ORBIT Display\n", 13) == 0);
    // Serial text descriptor.
    CHECK(e[111] == 0xFF && std::memcmp(&e[113], "1234ABCD\n", 9) == 0);
}

static void HugeModesStillFit() {
    // 8K@60 doesn't fit a DTD's 655.35 MHz; the EDID must still be valid (falls back).
    const auto e = orbit::MakeEdid(1, 7680, 4320, 60);
    unsigned sum = 0;
    for (unsigned char b : e) sum += b;
    CHECK(sum % 256 == 0);
    CHECK(e[54] + (e[55] << 8) > 0);
}

int main() {
    CvtMatchesVesa();
    EdidIsWellFormed();
    HugeModesStillFit();
    if (failures == 0) std::printf("edid tests passed\n");
    return failures == 0 ? 0 : 1;
}
