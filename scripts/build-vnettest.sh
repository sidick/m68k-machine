#!/usr/bin/env bash
# Rebuilds m68k/vnettest/VNetTest from m68k/vnettest/vnettest.c, the
# guest-side test tool for virtionet.device (ADR 0005 stage 3).
#
# NOT part of the Rust build and NOT run by CI -- this is a real AmigaOS
# CLI program (C:VNetTest on the patched image, installed and run from a
# shell/startup-sequence like any other command), not something
# crates/machine-core or crates/machine-hosted embeds directly. Run this
# script by hand after editing the sources, and check in the rebuilt
# binary alongside it: mirrors this project's own committed-binary policy
# (scripts/build-pciprobe.sh's own header comment).
#
# Requires the pinned m68k-amigaos cross-toolchain (bebbo amiga-gcc) on
# PATH or at the paths this project's toolchain notes use (/opt/amiga/bin).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SRC_DIR="$REPO_ROOT/m68k/vnettest"
OUT="$SRC_DIR/VNetTest"

CC="${M68K_CC:-m68k-amigaos-gcc}"
if ! command -v "$CC" >/dev/null 2>&1; then
    if [ -x /opt/amiga/bin/m68k-amigaos-gcc ]; then
        CC=/opt/amiga/bin/m68k-amigaos-gcc
    else
        echo "error: m68k-amigaos-gcc not found on PATH or at /opt/amiga/bin; set \$M68K_CC" >&2
        exit 1
    fi
fi

# -I m68k/virtionet-device: this project's own reconstructed devices/
# sana2.h (see that file's own provenance comment) -- the only header
# VNetTest needs from the driver's own tree, since everything else is
# the ordinary NDK.
CFLAGS=(-std=c99 -O2 -Wall -Wextra -mcpu=68020 -noixemul \
        -I"$REPO_ROOT/m68k/virtionet-device")

BUILD="$SRC_DIR/.build"
mkdir -p "$BUILD"

"$CC" "${CFLAGS[@]}" -c "$SRC_DIR/vnettest.c" -o "$BUILD/vnettest.o"

# debug.lib: KPrintF (clib/debug_protos.h) -- serial markers are treated
# as positive evidence on this platform (scripts/build-pciprobe.sh's own
# identical comment). This toolchain ships it under its native Amiga
# name (ndk/lib/libs/debug.lib, not libdebug.a), so it must be named
# explicitly with -l: rather than plain -ldebug.
CC_PATH="$(command -v "$CC")"
NDK_LIBS_UNRESOLVED="$(dirname "$CC_PATH")/../m68k-amigaos/ndk/lib/libs"
if [ ! -d "$NDK_LIBS_UNRESOLVED" ]; then
    echo "error: NDK libs dir not found at $NDK_LIBS_UNRESOLVED (needed for debug.lib)" >&2
    exit 1
fi
NDK_LIBS_DIR="$(cd "$NDK_LIBS_UNRESOLVED" && pwd)"

# Standard startup and standard link: -noixemul alone (libnix), no
# -nostartfiles/-nostdlib -- this is a normal CLI program, not a device.
# debug.lib's KDoFmt/KMayGetChar/KPutChar reference AbsExecBase/RawDoFmt
# etc. LVOs, which amiga.lib (libamiga.a, this toolchain's usual -lamiga)
# supplies; -lamiga must come after -l:debug.lib on the link line (also
# supplies CreateExtIO/DeleteExtIO, the alib_protos.h helpers this tool
# uses).
"$CC" -noixemul -mcpu=68020 -o "$OUT" \
    "$BUILD/vnettest.o" \
    -L"$NDK_LIBS_DIR" -l:debug.lib -lamiga

# Intermediate .o files are scratch, not part of the committed-binary
# policy above (only VNetTest itself is checked in) -- clean them up so
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
# (this file's own header comment): the real-ROM test greps these exact
# strings, so their presence in the binary is load-bearing, not cosmetic.
RESULT_COUNT=$(strings "$OUT" | grep -c "VNETTEST result" || true)
if [ "$RESULT_COUNT" -lt 1 ]; then
    echo "error: $OUT does not contain the string \"VNETTEST result\"" >&2
    exit 1
fi
echo "verified: string \"VNETTEST result\" present ($RESULT_COUNT occurrence(s))"

echo "all checks passed"
