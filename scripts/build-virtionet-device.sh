#!/usr/bin/env bash
# Rebuilds m68k/virtionet-device/virtionet.device from
# m68k/virtionet-device/{libstart.S,vnet_int.S,virtionet_device.c}, the
# SANA-II driver for this machine's modern virtio-net PCI function (ADR
# 0005 stage 3, docs/pci-library.md, docs/pcibridge-protocol.md).
#
# NOT part of the Rust build and NOT run by CI -- this is a real AmigaOS
# AUTOINIT disk device (Devs:virtionet.device, opened by name like any
# other SANA-II driver), not something crates/machine-core or
# crates/machine-hosted embeds directly. Run this script by hand after
# editing the sources, and check in the rebuilt binary alongside it:
# mirrors this project's own committed-binary policy (scripts/build-
# prometheus-library.sh's own header comment, scripts/build-pciprobe.sh).
#
# Requires the pinned m68k-amigaos cross-toolchain (bebbo amiga-gcc) on
# PATH or at the paths this project's toolchain notes use (/opt/amiga/bin).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SRC_DIR="$REPO_ROOT/m68k/virtionet-device"
OUT="$SRC_DIR/virtionet.device"

CC="${M68K_CC:-m68k-amigaos-gcc}"
if ! command -v "$CC" >/dev/null 2>&1; then
    if [ -x /opt/amiga/bin/m68k-amigaos-gcc ]; then
        CC=/opt/amiga/bin/m68k-amigaos-gcc
    else
        echo "error: m68k-amigaos-gcc not found on PATH or at /opt/amiga/bin; set \$M68K_CC" >&2
        exit 1
    fi
fi

# Same proven card/library flags as m68k/prometheus-library's own build
# script: -I for this device's own headers (sana2.h, virtio_pci.h,
# virtionet_device.h) plus the prometheus.library public include tree
# (proto/prometheus.h etc -- the only library this driver's public API
# depends on) and pcibridge_card.h's own directory (not used directly by
# this driver, but kept parallel to pciprobe's -I shape in case a future
# edit needs it; harmless if unused).
CFLAGS=(-std=c99 -O2 -Wall -Wextra -mcpu=68020 -noixemul -fomit-frame-pointer \
        -I"$SRC_DIR" \
        -I"$REPO_ROOT/m68k/prometheus-library/include" \
        -I"$REPO_ROOT/m68k/prometheus-library")

# virtionet.device is an Exec AUTOINIT library/device, not a program: no C
# runtime (same reasoning as prometheus.library's own LDFLAGS comment).
LDFLAGS=(-noixemul -mcpu=68020 -nostartfiles -nostdlib)

BUILD="$SRC_DIR/.build"
mkdir -p "$BUILD"

"$CC" "${CFLAGS[@]}" -c "$SRC_DIR/libstart.S" -o "$BUILD/libstart.o"
"$CC" "${CFLAGS[@]}" -c "$SRC_DIR/vnet_int.S" -o "$BUILD/vnet_int.o"
"$CC" "${CFLAGS[@]}" -c "$SRC_DIR/virtionet_device.c" -o "$BUILD/virtionet_device.o"

# debug.lib: KPrintF (clib/debug_protos.h) -- serial markers are treated as
# positive evidence on this platform. This toolchain ships it under its
# native Amiga name (ndk/lib/libs/debug.lib, not libdebug.a), so it must be
# named explicitly with -l: rather than plain -ldebug.
CC_PATH="$(command -v "$CC")"
NDK_LIBS_UNRESOLVED="$(dirname "$CC_PATH")/../m68k-amigaos/ndk/lib/libs"
if [ ! -d "$NDK_LIBS_UNRESOLVED" ]; then
    echo "error: NDK libs dir not found at $NDK_LIBS_UNRESOLVED (needed for debug.lib)" >&2
    exit 1
fi
NDK_LIBS_DIR="$(cd "$NDK_LIBS_UNRESOLVED" && pwd)"
# debug.lib's KDoFmt/KMayGetChar/KPutChar reference AbsExecBase/RawDoFmt
# etc. LVOs, which amiga.lib (libamiga.a, this toolchain's usual -lamiga)
# supplies; -lamiga must come after -l:debug.lib on the link line. amiga.lib
# also supplies NextTagItem/FindTagItem (utility.library, still referencing
# a plain global UtilityBase on this toolchain -- see virtionet_device.c's
# own comment).
"$CC" "${LDFLAGS[@]}" -o "$OUT" \
    "$BUILD/libstart.o" "$BUILD/vnet_int.o" "$BUILD/virtionet_device.o" \
    -L"$NDK_LIBS_DIR" -l:debug.lib -lamiga

# Intermediate .o files are scratch, not part of the committed-binary policy
# above (only virtionet.device itself is checked in) -- clean them up so
# they don't show up as untracked clutter in `git status`.
rm -rf "$BUILD"

# --- Verify positively. Absence of complaint is never success on this
# platform, so check concrete evidence and fail loudly on the first miss.

SIZE=$(wc -c < "$OUT" | tr -d ' ')
echo "built $OUT ($SIZE bytes)"

# (a) hunk magic 0x000003F3 at file start.
MAGIC=$(od -An -t x1 -N 4 "$OUT" | tr -d ' \n')
if [ "$MAGIC" != "000003f3" ]; then
    echo "error: $OUT does not start with hunk magic 0x000003F3 (got 0x$MAGIC)" >&2
    exit 1
fi
echo "verified: hunk magic 0x000003F3 present at offset 0"

# (b) ROMTag RTC_MATCHWORD (0x4AFC) present somewhere after the first
# hunk's header longwords -- a real AUTOINIT ROMTag, not just a magic
# file. Byte-sequence search (not `od -t x2`, which word-swaps on a
# little-endian host), anchored to an even byte offset (see
# build-prometheus-library.sh's identical comment for why).
if ! od -An -v -t x1 "$OUT" | tr -s ' \n' ' ' | tr -d ' ' | grep -qiE '^([0-9a-f]{4})*4afc'; then
    echo "error: $OUT does not contain RTC_MATCHWORD (0x4AFC) anywhere" >&2
    exit 1
fi
echo "verified: RTC_MATCHWORD (0x4AFC) present"

# (c) the device name string, findable by `strings`.
NAME_COUNT=$(strings "$OUT" | grep -c "virtionet.device" || true)
if [ "$NAME_COUNT" -lt 1 ]; then
    echo "error: $OUT does not contain the string \"virtionet.device\"" >&2
    exit 1
fi
echo "verified: string \"virtionet.device\" present ($NAME_COUNT occurrence(s))"

# (d) the VNETDEV serial-narration prefix, load-bearing for the real-ROM
# test's own evidence-gathering (task brief).
MARKER_COUNT=$(strings "$OUT" | grep -c "VNETDEV" || true)
if [ "$MARKER_COUNT" -lt 1 ]; then
    echo "error: $OUT does not contain the string \"VNETDEV\"" >&2
    exit 1
fi
echo "verified: string \"VNETDEV\" present ($MARKER_COUNT occurrence(s))"

echo "all checks passed"
