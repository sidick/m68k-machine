#!/usr/bin/env bash
# Rebuilds m68k/pciprobe/PCIProbe from
# m68k/pciprobe/{pciprobe.c,pciprobe_int.S}, the guest-side probe tool for
# ADR 0005 stage 2 (docs/pci-library.md section 6).
#
# NOT part of the Rust build and NOT run by CI -- this is a real AmigaOS
# CLI program (C:PCIProbe on the patched image, installed and run from a
# shell/startup-sequence like any other command), not something
# crates/machine-core or crates/machine-hosted embeds directly. Run this
# script by hand after editing the sources, and check in the rebuilt
# binary alongside it: mirrors this project's own committed-binary policy
# for m68k/rtgboard-card/rtgboard.card and m68k/pktport-handler/
# pktport-handler (see those scripts' own header comments).
#
# Requires the pinned m68k-amigaos cross-toolchain (bebbo amiga-gcc) on
# PATH or at the paths this project's toolchain notes use (/opt/amiga/bin).
#
# Unlike rtgboard.card (an AUTOINIT library, -nostdlib, no C runtime),
# PCIProbe is an ordinary command: -noixemul selects libnix's C library
# and startup code, and the link line deliberately does NOT pass
# -nostartfiles/-nostdlib -- it needs the standard startup (argc/argv,
# SysBase set from a4, WB-message handling) to run from a shell or
# Startup-Sequence at all.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SRC_DIR="$REPO_ROOT/m68k/pciprobe"
OUT="$SRC_DIR/PCIProbe"

CC="${M68K_CC:-m68k-amigaos-gcc}"
if ! command -v "$CC" >/dev/null 2>&1; then
    if [ -x /opt/amiga/bin/m68k-amigaos-gcc ]; then
        CC=/opt/amiga/bin/m68k-amigaos-gcc
    else
        echo "error: m68k-amigaos-gcc not found on PATH or at /opt/amiga/bin; set \$M68K_CC" >&2
        exit 1
    fi
fi

# -I prometheus-library/include: the fixed proto/prometheus.h etc. seam
# every prometheus.library caller (including a real driver) compiles
# against. -I prometheus-library itself: pcibridge_card.h, the register
# offsets shared between prometheus.library and this probe (fixed seam,
# not edited by this script or its sources).
CFLAGS=(-std=c99 -O2 -Wall -Wextra -mcpu=68020 -noixemul \
        -I"$REPO_ROOT/m68k/prometheus-library/include" \
        -I"$REPO_ROOT/m68k/prometheus-library")

BUILD="$SRC_DIR/.build"
mkdir -p "$BUILD"

"$CC" "${CFLAGS[@]}" -c "$SRC_DIR/pciprobe.c" -o "$BUILD/pciprobe.o"
"$CC" "${CFLAGS[@]}" -c "$SRC_DIR/pciprobe_int.S" -o "$BUILD/pciprobe_int.o"

# debug.lib: KPrintF (clib/debug_protos.h) -- serial markers are treated
# as positive evidence on this platform (see e.g. scripts/
# build-rtgboard-card.sh's identical comment). This toolchain ships it
# under its native Amiga name (ndk/lib/libs/debug.lib, not libdebug.a),
# so it must be named explicitly with -l: rather than plain -ldebug.
CC_PATH="$(command -v "$CC")"
NDK_LIBS_UNRESOLVED="$(dirname "$CC_PATH")/../m68k-amigaos/ndk/lib/libs"
if [ ! -d "$NDK_LIBS_UNRESOLVED" ]; then
    echo "error: NDK libs dir not found at $NDK_LIBS_UNRESOLVED (needed for debug.lib)" >&2
    exit 1
fi
NDK_LIBS_DIR="$(cd "$NDK_LIBS_UNRESOLVED" && pwd)"

# Standard startup and standard link: -noixemul alone (libnix), no
# -nostartfiles/-nostdlib -- this is a normal CLI program, not a library.
# debug.lib's KDoFmt/KMayGetChar/KPutChar reference AbsExecBase/RawDoFmt
# etc. LVOs, which amiga.lib (libamiga.a, this toolchain's usual -lamiga)
# supplies; -lamiga must come after -l:debug.lib on the link line.
"$CC" -noixemul -mcpu=68020 -o "$OUT" \
    "$BUILD/pciprobe.o" "$BUILD/pciprobe_int.o" \
    -L"$NDK_LIBS_DIR" -l:debug.lib -lamiga

# Intermediate .o files are scratch, not part of the committed-binary
# policy above (only PCIProbe itself is checked in) -- clean them up so
# they don't show up as untracked clutter in `git status`.
rm -rf "$BUILD"

# --- Verify positively. Absence of complaint is never success on this
# platform, so check concrete evidence and fail loudly on the first miss.

SIZE=$(wc -c < "$OUT" | tr -d ' ')
echo "built $OUT ($SIZE bytes)"

# (a) hunk magic 0x000003F3 at file start -- a real AmigaDOS load file.
MAGIC=$(od -An -t x1 -N 4 "$OUT" | tr -d ' \n')
if [ "$MAGIC" != "000003f3" ]; then
    echo "error: $OUT does not start with hunk magic 0x000003F3 (got 0x$MAGIC)" >&2
    exit 1
fi
echo "verified: hunk magic 0x000003F3 present at offset 0"

# (b) the marker prefix this tool's own narration contract requires
# (docs/pci-library.md section 6): the real-ROM test greps these exact
# strings, so their presence in the binary is load-bearing, not cosmetic.
RESULT_COUNT=$(strings "$OUT" | grep -c "PCIPROBE result" || true)
if [ "$RESULT_COUNT" -lt 1 ]; then
    echo "error: $OUT does not contain the string \"PCIPROBE result\"" >&2
    exit 1
fi
echo "verified: string \"PCIPROBE result\" present ($RESULT_COUNT occurrence(s))"

echo "all checks passed"
