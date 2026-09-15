#!/usr/bin/env bash
# Rebuilds m68k/rtgboard-card/rtgboard.card from
# m68k/rtgboard-card/{libstart.S,rtgboard_card.c}, the Picasso96 driver for
# the `rtgboard` device (docs/rtgboard-protocol.md).
#
# NOT part of the Rust build and NOT run by CI -- this is a real AmigaOS
# AUTOINIT library (LIBS:Picasso96/rtgboard.card, loaded by rtg.library via
# a Monitor icon's BOARDTYPE tooltype), not something crates/machine-core or
# crates/machine-hosted embeds directly. Run this script by hand after
# editing the sources, and check in the rebuilt binary alongside it: mirrors
# this project's own committed-binary policy for m68k/pktport-handler/
# pktport-handler (see scripts/build-pktport-handler.sh's own header
# comment) and assets/hostblk-rom/hostblk-diagrom.bin, so that anyone
# testing this driver doesn't need a working m68k toolchain just to run it.
#
# Requires the pinned m68k-amigaos cross-toolchain (bebbo amiga-gcc) on
# PATH or at the paths this project's toolchain notes use (/opt/amiga/bin),
# and the P96 SDK private include files (boardinfo.h et al.), as vendored
# by ~/src/amirfb.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SRC_DIR="$REPO_ROOT/m68k/rtgboard-card"
OUT="$SRC_DIR/rtgboard.card"

CC="${M68K_CC:-m68k-amigaos-gcc}"
if ! command -v "$CC" >/dev/null 2>&1; then
    if [ -x /opt/amiga/bin/m68k-amigaos-gcc ]; then
        CC=/opt/amiga/bin/m68k-amigaos-gcc
    else
        echo "error: m68k-amigaos-gcc not found on PATH or at /opt/amiga/bin; set \$M68K_CC" >&2
        exit 1
    fi
fi

P96SDK_INC="${P96SDK_INC:-$HOME/src/amirfb/include/p96sdk}"
if [ ! -d "$P96SDK_INC" ]; then
    echo "error: P96 SDK include dir not found at $P96SDK_INC; set \$P96SDK_INC" >&2
    exit 1
fi

# Compile flags copied from amirfb.card's own proven card flags
# (~/src/amirfb/Makefile's M68K_CFLAGS), plus -Wextra for this build:
# -std=c99 -O2 -Wall -Wextra -mcpu=68020 -noixemul -fomit-frame-pointer.
CFLAGS=(-std=c99 -O2 -Wall -Wextra -mcpu=68020 -noixemul -fomit-frame-pointer \
        -I"$P96SDK_INC" -I"$SRC_DIR")

# rtgboard.card is an Exec AUTOINIT library, not a program: no C runtime
# (same reasoning as amirfb.card's own LIBLDFLAGS comment).
LDFLAGS=(-noixemul -mcpu=68020 -nostartfiles -nostdlib)

BUILD="$SRC_DIR/.build"
mkdir -p "$BUILD"

"$CC" "${CFLAGS[@]}" -c "$SRC_DIR/libstart.S" -o "$BUILD/libstart.o"
"$CC" "${CFLAGS[@]}" -c "$SRC_DIR/rtgboard_card.c" -o "$BUILD/rtgboard_card.o"

# debug.lib: KPrintF (clib/debug_protos.h), used for the FindCard/InitCard
# entry markers -- same idiom amirfb.card uses (serial markers are treated
# as positive evidence in this project). This toolchain ships it under its
# native Amiga name (ndk/lib/libs/debug.lib, not libdebug.a), so it must be
# named explicitly with -l: rather than plain -ldebug.
CC_PATH="$(command -v "$CC")"
NDK_LIBS_UNRESOLVED="$(dirname "$CC_PATH")/../m68k-amigaos/ndk/lib/libs"
if [ ! -d "$NDK_LIBS_UNRESOLVED" ]; then
    echo "error: NDK libs dir not found at $NDK_LIBS_UNRESOLVED (needed for debug.lib)" >&2
    exit 1
fi
NDK_LIBS_DIR="$(cd "$NDK_LIBS_UNRESOLVED" && pwd)"
# debug.lib's KDoFmt/KMayGetChar/KPutChar reference AbsExecBase/the RawDoFmt
# etc. LVOs, which amiga.lib (libamiga.a, this toolchain's usual -lamiga)
# supplies; -lamiga must come after -l:debug.lib on the link line.
"$CC" "${LDFLAGS[@]}" -o "$OUT" \
    "$BUILD/libstart.o" "$BUILD/rtgboard_card.o" \
    -L"$NDK_LIBS_DIR" -l:debug.lib -lamiga

# Intermediate .o files are scratch, not part of the committed-binary policy
# above (only rtgboard.card itself is checked in) -- clean them up so they
# don't show up as untracked clutter in `git status`.
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
# hunk's header longwords -- a real AUTOINIT ROMTag, not just a magic file.
# Byte-sequence search (not `od -t x2`, which word-swaps on a little-endian
# host and would look for the wrong byte order against this big-endian m68k
# file). The ^([0-9a-f]{4})* prefix anchors the match to an even byte
# offset: exec only recognises a ROMTag on a word boundary, and an
# unanchored search could false-positive on 4a/fc straddling two unrelated
# bytes -- exactly the "self-consistent check passes over a real bug"
# class this platform keeps producing.
if ! od -An -v -t x1 "$OUT" | tr -s ' \n' ' ' | tr -d ' ' | grep -qiE '^([0-9a-f]{4})*4afc'; then
    echo "error: $OUT does not contain RTC_MATCHWORD (0x4AFC) anywhere" >&2
    exit 1
fi
echo "verified: RTC_MATCHWORD (0x4AFC) present"

# (c) the library name string, findable by `strings`.
NAME_COUNT=$(strings "$OUT" | grep -c "rtgboard.card" || true)
if [ "$NAME_COUNT" -lt 1 ]; then
    echo "error: $OUT does not contain the string \"rtgboard.card\"" >&2
    exit 1
fi
echo "verified: string \"rtgboard.card\" present ($NAME_COUNT occurrence(s))"

echo "all checks passed"
