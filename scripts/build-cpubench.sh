#!/usr/bin/env bash
# Rebuilds m68k/cpubench/kernels.bin (flat binary, for crates/cpu-bench)
# and m68k/cpubench/CPUBench (the AmigaOS CLI program, linking
# kernels.s plus cpubench.c plus the vendored/ported CoreMark sources
# in m68k/cpubench/coremark/) from source. docs/bus-fast-path-plan.md
# step 8.
#
# NOT part of the Rust build and NOT run by CI -- CPUBench is a real
# AmigaOS CLI program (C:CPUBench on the patched image), not something
# crates/machine-core or crates/machine-hosted embeds directly, mirroring
# every other scripts/build-*.sh in this project. kernels.bin IS read by
# crates/cpu-bench (via include_bytes!) at Rust build time, but this
# script's job is to (re)produce it from m68k/cpubench/kernels.s by hand
# after an edit, then commit the rebuilt binary -- same committed-binary
# policy as every other assembled artifact here (CLAUDE.md).
#
# Requires vasm (Motorola syntax m68k backend) and the pinned
# m68k-amigaos cross-toolchain (bebbo amiga-gcc), both on PATH or at
# /opt/amiga/bin, and the NDK 3.2 headers at
# ~/src/amiga-gcc/projects/NDK3.2/Include_H.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SRC_DIR="$REPO_ROOT/m68k/cpubench"
COREMARK_DIR="$SRC_DIR/coremark"
OUT="$SRC_DIR/CPUBench"
KERNELS_S="$SRC_DIR/kernels.s"
KERNELS_BIN="$SRC_DIR/kernels.bin"

VASM="${VASM:-vasmm68k_mot}"
if ! command -v "$VASM" >/dev/null 2>&1; then
    if [ -x /opt/amiga/bin/vasmm68k_mot ]; then
        VASM=/opt/amiga/bin/vasmm68k_mot
    else
        echo "error: vasmm68k_mot not found on PATH or at /opt/amiga/bin; set \$VASM" >&2
        exit 1
    fi
fi

CC="${M68K_CC:-m68k-amigaos-gcc}"
if ! command -v "$CC" >/dev/null 2>&1; then
    if [ -x /opt/amiga/bin/m68k-amigaos-gcc ]; then
        CC=/opt/amiga/bin/m68k-amigaos-gcc
    else
        echo "error: m68k-amigaos-gcc not found on PATH or at /opt/amiga/bin; set \$M68K_CC" >&2
        exit 1
    fi
fi

NDK_INCLUDE="${NDK_INCLUDE:-$HOME/src/amiga-gcc/projects/NDK3.2/Include_H}"
if [ ! -d "$NDK_INCLUDE" ]; then
    echo "error: NDK 3.2 headers not found at $NDK_INCLUDE; set \$NDK_INCLUDE" >&2
    exit 1
fi

BUILD="$SRC_DIR/.build"
mkdir -p "$BUILD"

echo "==> 1/4: kernels.bin (flat binary, vasm -Fbin -m68040, for crates/cpu-bench)"
"$VASM" -Fbin -m68040 -quiet -o "$KERNELS_BIN" "$KERNELS_S"
SIZE=$(wc -c < "$KERNELS_BIN" | tr -d ' ')
echo "    built $KERNELS_BIN ($SIZE bytes)"

echo "==> 2/4: kernels.o (relocatable AmigaOS object, vasm -Fhunk -m68040)"
# The -quiet above and this build's own warnings are the same two
# expected "short-branch to following instruction turned into a nop"
# warnings kernels.s's own header documents (kernel 1 and kernel 10) --
# not silenced here so a NEW warning (a sign something else changed)
# stays visible.
"$VASM" -Fhunk -m68040 -o "$BUILD/kernels.o" "$KERNELS_S"

CFLAGS=(-std=c99 -O2 -Wall -Wextra -mcpu=68040 -noixemul \
        -I"$NDK_INCLUDE" -I"$SRC_DIR" -I"$COREMARK_DIR")

echo "==> 3/4: cpubench.c and the CoreMark port"
"$CC" "${CFLAGS[@]}" -c "$SRC_DIR/cpubench.c" -o "$BUILD/cpubench.o"

# core_main.c defines its own `main`, which must coexist with
# cpubench.c's `main` above -- renamed at this compile step only
# (coremark_amiga.c's own `extern int coremark_main(...)` declaration
# is what the rest of this program calls it by; see that file's header
# comment). Every other vendored CoreMark source is compiled unchanged.
"$CC" "${CFLAGS[@]}" -Dmain=coremark_main -c "$COREMARK_DIR/core_main.c" -o "$BUILD/core_main.o"
"$CC" "${CFLAGS[@]}" -c "$COREMARK_DIR/core_list_join.c" -o "$BUILD/core_list_join.o"
"$CC" "${CFLAGS[@]}" -c "$COREMARK_DIR/core_matrix.c" -o "$BUILD/core_matrix.o"
"$CC" "${CFLAGS[@]}" -c "$COREMARK_DIR/core_state.c" -o "$BUILD/core_state.o"
"$CC" "${CFLAGS[@]}" -c "$COREMARK_DIR/core_util.c" -o "$BUILD/core_util.o"
"$CC" "${CFLAGS[@]}" -c "$COREMARK_DIR/core_portme.c" -o "$BUILD/core_portme.o"
"$CC" "${CFLAGS[@]}" -c "$COREMARK_DIR/coremark_amiga.c" -o "$BUILD/coremark_amiga.o"

echo "==> 4/4: link CPUBench"
# -lamiga: CreateMsgPort/CreateExtIO/DeleteExtIO/DeleteMsgPort
# (alib_protos.h helpers, timer.device's classic open recipe --
# cpubench.c's own timer_open()/timer_close()). -lm: libgcc's software
# double routines core_portme.c's ee_printf and CoreMark's own matrix
# workload need on a build without -mcpu=68040's hardware FPU path
# exercised for every double op (kept explicit rather than relying on
# implicit link-time defaults, the same posture build-vnettest.sh's own
# header comment takes for debug.lib).
"$CC" -noixemul -mcpu=68040 -o "$OUT" \
    "$BUILD/cpubench.o" \
    "$BUILD/core_main.o" \
    "$BUILD/core_list_join.o" \
    "$BUILD/core_matrix.o" \
    "$BUILD/core_state.o" \
    "$BUILD/core_util.o" \
    "$BUILD/core_portme.o" \
    "$BUILD/coremark_amiga.o" \
    "$BUILD/kernels.o" \
    -lamiga -lm

rm -rf "$BUILD"

# --- Verify positively. Absence of complaint is never success on this
# platform, so check concrete evidence and fail loudly on the first miss.

SIZE=$(wc -c < "$OUT" | tr -d ' ')
echo "built $OUT ($SIZE bytes)"

MAGIC=$(od -An -t x1 -N 4 "$OUT" | tr -d ' \n')
if [ "$MAGIC" != "000003f3" ]; then
    echo "error: $OUT does not start with hunk magic 0x000003F3 (got 0x$MAGIC)" >&2
    exit 1
fi
echo "verified: hunk magic 0x000003F3 present at offset 0"

RESULT_COUNT=$(strings "$OUT" | grep -c "^CPUBENCH " || true)
if [ "$RESULT_COUNT" -lt 1 ]; then
    echo "error: $OUT does not contain the string \"CPUBENCH \"" >&2
    exit 1
fi
echo "verified: string \"CPUBENCH \" present ($RESULT_COUNT occurrence(s), format-string fragments)"

DONE_COUNT=$(strings "$OUT" | grep -c "^CPUBENCH DONE$" || true)
if [ "$DONE_COUNT" -lt 1 ]; then
    echo "error: $OUT does not contain the literal string \"CPUBENCH DONE\"" >&2
    exit 1
fi
echo "verified: string \"CPUBENCH DONE\" present"

echo "all checks passed"
