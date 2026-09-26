#!/usr/bin/env bash
# Wall-clock/instruction-count benchmark harness for the hosted runner
# (docs/bus-fast-path-plan.md step 1). Every optimization step in that
# plan is accepted on numbers, not on tests passing -- unit tests here
# have repeatedly passed over real bugs, so a speedup that isn't
# measured against a real boot isn't one. This script is the fixed
# yardstick every step's Results row comes from, run before and after
# each change on the same tree.
#
# WHY IT FAILS LOUDLY ON MISSING MEDIA (same posture as
# scripts/phase3-gate.sh): a benchmark that silently runs zero frames
# because the ROM or disk image wasn't found would still print numbers
# -- wrong ones, indistinguishable from a real run without reading the
# log closely. Silent failure is this platform's norm; this script
# checks its licensed inputs up front and refuses to guess.
#
# WHY IT PARSES THE RUNNER'S OWN STATUS LINE INSTEAD OF TIMING RAW
# INSTRUCTIONS ITSELF: `machine-hosted` already counts retired
# instructions and prints them in its final `PHASE1 HOSTED: ...` line
# (crates/machine-hosted/src/run.rs, Report::status_line). Re-deriving
# that count independently would be a second source of truth for the
# same number; asserting the run actually reached the frame limit (the
# `LIMIT REACHED (max-frames)` outcome, not a wedge or a clean halt) is
# the only way to know the timing is comparable across runs.
#
# Usage: bench-boot.sh [--no-build] [--label TEXT] [--profile DIR]
#   FRAMES=1500 bench-boot.sh ...
#
# Env vars:
#   M68K_KICKSTART_A1200   path to the A1200 Kickstart ROM
#                          (default: $REPO_ROOT/nondistribution/A1200.47.115.rom)
#   M68K_BENCH_HDF         path to the hostblk image for the "full" config
#                          (default: $REPO_ROOT/nondistribution/aros/aros.hdf)
#   FRAMES                 --max-frames value for both configs (default: 1500)
#
# Flags:
#   --no-build      skip `cargo build --release -p machine-hosted`; use
#                   whatever is already at target/release/machine-hosted
#   --label TEXT    label for the Markdown results row (default: short
#                   git hash of the current tree)
#   --profile DIR   also record each run with `samply record --save-only`
#                   into DIR/<name>.json.gz, then symbolize it with
#                   scripts/bench-symbolize.py into DIR/<name>.txt
#   --help          print this usage and exit

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

NO_BUILD=0
LABEL=""
PROFILE_DIR=""

usage() {
    sed -n '1,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' | sed '1d'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --no-build)
            NO_BUILD=1
            shift
            ;;
        --label)
            LABEL="${2:-}"
            shift 2
            ;;
        --profile)
            PROFILE_DIR="${2:-}"
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "error: unrecognized argument: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

ROM="${M68K_KICKSTART_A1200:-$REPO_ROOT/nondistribution/A1200.47.115.rom}"
HDF="${M68K_BENCH_HDF:-$REPO_ROOT/nondistribution/aros/aros.hdf}"
FRAMES="${FRAMES:-1500}"

# --- Preconditions: the licensed inputs a run would otherwise silently
# skip on. -----------------------------------------------------------------

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

if [ -n "$PROFILE_DIR" ]; then
    mkdir -p "$PROFILE_DIR"
    if ! command -v samply >/dev/null 2>&1; then
        echo "error: --profile given but samply is not on PATH" >&2
        exit 1
    fi
    if [ ! -x "$SCRIPT_DIR/bench-symbolize.py" ]; then
        echo "error: --profile given but $SCRIPT_DIR/bench-symbolize.py is missing or not executable" >&2
        exit 1
    fi
fi

# now() prints seconds with nanosecond precision; portable across the
# GNU/BSD `date` split this repo already has to deal with (macOS ships
# BSD date, CI may run GNU date) by going through python3 rather than a
# `date` flag that only one of them supports.
now() {
    python3 -c 'import time; print(f"{time.time():.9f}")'
}

# run_config NAME ARGS...
# Runs machine-hosted once (optionally under samply), times it, and
# extracts the instruction count from its "LIMIT REACHED (max-frames)"
# status line. Sets the globals RUN_WALL/RUN_INSTR rather than returning
# through a `$(...)` command substitution: a substitution runs in a
# subshell, and this function's own `exit 1` on a bad run must stop the
# whole script under `set -e`, not just that subshell.
run_config() {
    local name="$1"
    shift
    local out
    out="$(mktemp "${TMPDIR:-/tmp}/bench-boot.$name.XXXXXX")"
    trap 'rm -f "$out"' RETURN

    local start end wall
    start="$(now)"
    if [ -n "$PROFILE_DIR" ]; then
        # samply's exit code mirrors the child's; machine-hosted exits
        # nonzero on Outcome::LimitReached (Report::exit_code), which is
        # the *expected* outcome here, not a failure -- so samply's own
        # exit status is deliberately not checked. What matters is that
        # the profile file and the runner's status line both landed.
        samply record --save-only -o "$PROFILE_DIR/$name.json.gz" -- \
            "$BIN" "$@" >"$out" 2>&1 || true
        if [ ! -f "$PROFILE_DIR/$name.json.gz" ]; then
            echo "error: samply did not write $PROFILE_DIR/$name.json.gz" >&2
            exit 1
        fi
    else
        "$BIN" "$@" >"$out" 2>&1 || true
    fi
    end="$(now)"
    wall="$(python3 -c "print(f'{${end} - ${start}:.3f}')")"

    local line
    line="$(grep -F 'PHASE1 HOSTED: LIMIT REACHED (max-frames)' "$out" || true)"
    if [ -z "$line" ]; then
        echo "error: config '$name' did not reach the frame limit -- not a valid data point" >&2
        echo "----- last 20 lines of output -----" >&2
        tail -n 20 "$out" >&2
        exit 1
    fi

    local instructions
    instructions="$(echo "$line" | sed -n 's/.*-- \([0-9][0-9]*\) instructions.*/\1/p')"
    if [ -z "$instructions" ]; then
        echo "error: could not parse instruction count out of: $line" >&2
        exit 1
    fi

    if [ -n "$PROFILE_DIR" ]; then
        "$SCRIPT_DIR/bench-symbolize.py" "$PROFILE_DIR/$name.json.gz" "$BIN" \
            >"$PROFILE_DIR/$name.txt"
    fi

    RUN_WALL="$wall"
    RUN_INSTR="$instructions"
}

echo "==> running bare (ROM + fast RAM only), --max-frames $FRAMES" >&2
run_config bare --rom "$ROM" --max-frames "$FRAMES" --max-instructions 0
BARE_WALL="$RUN_WALL"
BARE_INSTR="$RUN_INSTR"

echo "==> running full (pcibridge + rtgboard + hostblk), --max-frames $FRAMES" >&2
run_config full --rom "$ROM" --pcibridge --rtgboard 640x480 --hostblk "$HDF" \
    --max-frames "$FRAMES" --max-instructions 0
FULL_WALL="$RUN_WALL"
FULL_INSTR="$RUN_INSTR"

mips() {
    # instructions / wall-seconds / 1e6, 1 decimal place.
    python3 -c "print(f'{$1 / $2 / 1_000_000:.1f}')"
}

BARE_MIPS="$(mips "$BARE_INSTR" "$BARE_WALL")"
FULL_MIPS="$(mips "$FULL_INSTR" "$FULL_WALL")"

echo "bare  wall=${BARE_WALL}s  instructions=${BARE_INSTR}  mips=${BARE_MIPS}  frames=${FRAMES}"
echo "full  wall=${FULL_WALL}s  instructions=${FULL_INSTR}  mips=${FULL_MIPS}  frames=${FRAMES}"
echo
echo "| ${LABEL} | ${BARE_WALL} s (${BARE_MIPS} MIPS) | ${FULL_WALL} s (${FULL_MIPS} MIPS) | |"

if [ -n "$PROFILE_DIR" ]; then
    echo >&2
    echo "==> profiles written: $PROFILE_DIR/bare.{json.gz,txt}, $PROFILE_DIR/full.{json.gz,txt}" >&2
fi
