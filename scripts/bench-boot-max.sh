#!/usr/bin/env bash
# --cpu-speed max benchmarks (docs/bus-fast-path-plan.md step 7.3), a
# sibling to bench-boot.sh rather than a mode added to it: that script's
# whole metric (wall clock for a fixed --max-frames) is exactly what max
# mode makes meaningless to compare across configs on its own -- max mode
# paces device time to the wall clock *by design*, so "wall clock for N
# frames" is just N/50 seconds regardless of host speed or config. This
# script's own metric is guest instructions per real second (MIPS), plus
# the cycle-vs-max wall-clock-to-Workbench comparison and an idle-cost
# measurement, all called for by that plan step. bench-boot.sh's own
# 1500-frame cycle-mode rows are untouched by this script's existence.
#
# Usage: bench-boot-max.sh [--no-build] [--label TEXT]
#
# Env vars:
#   M68K_KICKSTART_A1200   path to the A1200 Kickstart ROM
#                          (default: $REPO_ROOT/nondistribution/A1200.47.115.rom)
#   M68K_BENCH_HDF         path to a bootable hostblk image
#                          (default: $REPO_ROOT/nondistribution/m68k-machine.hdf)
#   BOOT_FRAMES            --max-frames for the boot-to-Workbench-ready
#                          comparison (default: 4400 -- an idle Workbench
#                          desktop is up and mouse-interactive by frame
#                          ~4200 on this ROM/HDF pair, the same guest-frame
#                          count scripted-input tests elsewhere in this
#                          repo wait out before clicking; frame count is
#                          guest-time, invariant between cycle and max
#                          mode, which is exactly ADR 0006's point)
#   IDLE_FRAMES            --max-frames for the idle-cost run, started
#                          after BOOT_FRAMES so the guest is already past
#                          boot when the idle window is measured
#                          (default: 1000, ~20s of guest/wall time)
#
# Flags:
#   --no-build   skip `cargo build --release -p machine-hosted`
#   --label TEXT label for the Markdown results row (default: short git hash)
#   --help       print this usage and exit

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

NO_BUILD=0
LABEL=""

usage() {
    sed -n '1,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' | sed '1d'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --no-build) NO_BUILD=1; shift ;;
        --label) LABEL="${2:-}"; shift 2 ;;
        --help|-h) usage; exit 0 ;;
        *) echo "error: unrecognized argument: $1" >&2; usage >&2; exit 1 ;;
    esac
done

ROM="${M68K_KICKSTART_A1200:-$REPO_ROOT/nondistribution/A1200.47.115.rom}"
HDF="${M68K_BENCH_HDF:-$REPO_ROOT/nondistribution/m68k-machine.hdf}"
BOOT_FRAMES="${BOOT_FRAMES:-4400}"
IDLE_FRAMES="${IDLE_FRAMES:-1000}"

if [ ! -f "$ROM" ]; then
    echo "error: Kickstart ROM not found: $ROM" >&2
    echo "       (licensed media -- see nondistribution/README.md; override with M68K_KICKSTART_A1200)" >&2
    exit 1
fi
if [ ! -f "$HDF" ]; then
    echo "error: hostblk image not found: $HDF" >&2
    echo "       (build one with amibake -- see nondistribution/README.md; override with M68K_BENCH_HDF)" >&2
    exit 1
fi

if [ -z "$LABEL" ]; then
    LABEL="$(cd "$REPO_ROOT" && git rev-parse --short HEAD 2>/dev/null || echo unknown)"
fi

BIN="$REPO_ROOT/target/release/machine-hosted"

if [ "$NO_BUILD" -eq 0 ]; then
    echo "==> building machine-hosted (release)" >&2
    (cd "$REPO_ROOT" && cargo build --release -p machine-hosted)
fi
if [ ! -x "$BIN" ]; then
    echo "error: $BIN not found or not executable (build it first, or drop --no-build)" >&2
    exit 1
fi

now() {
    python3 -c 'import time; print(f"{time.time():.9f}")'
}

# run_and_measure NAME ARGS... -- runs machine-hosted, times it end to
# end, and extracts the retired-instruction count from its final status
# line, the same "don't re-derive a count the runner already printed"
# posture bench-boot.sh's run_config takes. Sets RUN_WALL/RUN_INSTR.
run_and_measure() {
    local name="$1"
    shift
    local out
    out="$(mktemp "${TMPDIR:-/tmp}/bench-boot-max.$name.XXXXXX")"

    local start end
    start="$(now)"
    "$BIN" "$@" >"$out" 2>&1 || true
    end="$(now)"
    RUN_WALL="$(python3 -c "print(f'{${end} - ${start}:.3f}')")"

    local line
    line="$(grep -F 'PHASE1 HOSTED:' "$out" | tail -n1 || true)"
    if [ -z "$line" ]; then
        echo "error: config '$name' produced no PHASE1 HOSTED status line" >&2
        tail -n 20 "$out" >&2
        rm -f "$out"
        exit 1
    fi
    echo "  [$name] $line" >&2

    RUN_INSTR="$(echo "$line" | sed -n 's/.*-- \([0-9][0-9]*\) instructions.*/\1/p')"
    if [ -z "$RUN_INSTR" ]; then
        echo "error: could not parse instruction count out of: $line" >&2
        rm -f "$out"
        exit 1
    fi
    RUN_OUT="$out"
}

mips() {
    python3 -c "print(f'{$1 / $2 / 1_000_000:.1f}')"
}

echo "==> wall clock to Workbench-ready ($BOOT_FRAMES frames), cycle mode" >&2
run_and_measure boot-cycle --rom "$ROM" --hostblk "$HDF" --cpu-speed cycle \
    --max-frames "$BOOT_FRAMES" --max-instructions 0
CYCLE_BOOT_WALL="$RUN_WALL"
CYCLE_BOOT_INSTR="$RUN_INSTR"
rm -f "$RUN_OUT"

echo "==> wall clock to Workbench-ready ($BOOT_FRAMES frames), max mode" >&2
run_and_measure boot-max --rom "$ROM" --hostblk "$HDF" --cpu-speed max \
    --max-frames "$BOOT_FRAMES" --max-instructions 0
MAX_BOOT_WALL="$RUN_WALL"
MAX_BOOT_INSTR="$RUN_INSTR"
rm -f "$RUN_OUT"

echo "==> guest MIPS over a fixed idle window ($IDLE_FRAMES further frames), max mode" >&2
run_and_measure idle-max --rom "$ROM" --hostblk "$HDF" --cpu-speed max \
    --max-frames "$((BOOT_FRAMES + IDLE_FRAMES))" --max-instructions 0
IDLE_WALL_TOTAL="$RUN_WALL"
IDLE_INSTR_TOTAL="$RUN_INSTR"
rm -f "$RUN_OUT"
# Instructions/wall time *of the idle window alone*, not the whole run:
# subtract the boot run's own totals so a workload that is almost
# entirely idle Workbench (the common case past boot) doesn't have its
# MIPS figure diluted by the boot's own much busier instruction rate.
IDLE_ONLY_INSTR=$((IDLE_INSTR_TOTAL - MAX_BOOT_INSTR))
IDLE_ONLY_WALL="$(python3 -c "print(f'{${IDLE_WALL_TOTAL} - ${MAX_BOOT_WALL}:.3f}')")"

echo "==> idle host CPU cost over the same idle window, max mode (/usr/bin/time -l)" >&2
IDLE_TIME_OUT="$(mktemp "${TMPDIR:-/tmp}/bench-boot-max.idle-time.XXXXXX")"
/usr/bin/time -l "$BIN" --rom "$ROM" --hostblk "$HDF" --cpu-speed max \
    --max-frames "$((BOOT_FRAMES + IDLE_FRAMES))" --max-instructions 0 \
    >/dev/null 2>"$IDLE_TIME_OUT" || true
IDLE_TIME_LINE="$(grep -E '^\s*[0-9.]+ real\s' "$IDLE_TIME_OUT" || true)"
if [ -z "$IDLE_TIME_LINE" ]; then
    echo "warning: could not parse /usr/bin/time -l output (not on macOS/BSD?); raw:" >&2
    cat "$IDLE_TIME_OUT" >&2
    IDLE_REAL="?"
    IDLE_USER="?"
    IDLE_SYS="?"
else
    IDLE_REAL="$(echo "$IDLE_TIME_LINE" | awk '{print $1}')"
    IDLE_USER="$(echo "$IDLE_TIME_LINE" | awk '{print $3}')"
    IDLE_SYS="$(echo "$IDLE_TIME_LINE" | awk '{print $5}')"
fi
rm -f "$IDLE_TIME_OUT"

BOOT_SPEEDUP="$(python3 -c "print(f'{${CYCLE_BOOT_WALL} / ${MAX_BOOT_WALL}:.2f}')" 2>/dev/null || echo "?")"
IDLE_MIPS="?"
if [ "$IDLE_ONLY_INSTR" -gt 0 ] 2>/dev/null; then
    IDLE_MIPS="$(mips "$IDLE_ONLY_INSTR" "$IDLE_ONLY_WALL")"
fi
CYCLE_MIPS="$(mips "$CYCLE_BOOT_INSTR" "$CYCLE_BOOT_WALL")"
MAX_BOOT_MIPS="$(mips "$MAX_BOOT_INSTR" "$MAX_BOOT_WALL")"

echo
echo "cycle mode, $BOOT_FRAMES frames: wall=${CYCLE_BOOT_WALL}s instructions=${CYCLE_BOOT_INSTR} mips=${CYCLE_MIPS}"
echo "max mode,   $BOOT_FRAMES frames: wall=${MAX_BOOT_WALL}s instructions=${MAX_BOOT_INSTR} mips=${MAX_BOOT_MIPS}"
echo "wall-clock-to-frame-$BOOT_FRAMES speedup, cycle over max: ${BOOT_SPEEDUP}x (expected >1: max mode is wall-clock-paced, cycle mode is not)"
echo "idle window (frames $BOOT_FRAMES..$((BOOT_FRAMES + IDLE_FRAMES))), max mode: wall=${IDLE_ONLY_WALL}s instructions=${IDLE_ONLY_INSTR} mips=${IDLE_MIPS}"
echo "idle host CPU over that window (/usr/bin/time -l): real=${IDLE_REAL}s user=${IDLE_USER}s sys=${IDLE_SYS}s"
echo
echo "| ${LABEL} | cycle ${CYCLE_BOOT_WALL}s (${CYCLE_MIPS} MIPS) | max ${MAX_BOOT_WALL}s (${MAX_BOOT_MIPS} MIPS) | idle max ${IDLE_MIPS} MIPS, user=${IDLE_USER}s+sys=${IDLE_SYS}s over ${IDLE_ONLY_WALL}s real | |"
