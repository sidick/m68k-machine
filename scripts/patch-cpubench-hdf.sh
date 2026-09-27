#!/usr/bin/env bash
# Patches an already-built amibake HDF to install C:CPUBench
# (docs/bus-fast-path-plan.md step 8) and run it once at boot from the
# startup-sequence. Extends scripts/patch-virtionet-hdf.sh's pattern
# exactly (same reasoning: amibake manifests cannot install loose local
# files today, and the standing instruction for this stage is delivery
# by post-generation HDF patching without modifying amibake).
#
# Usage: patch-cpubench-hdf.sh [src.hdf] [out.hdf]
#   src.hdf defaults to $REPO_ROOT/nondistribution/m68k-machine.hdf,
#   overridable via $M68K_TEST_HDF.
#   out.hdf defaults to
#   $REPO_ROOT/nondistribution/m68k-machine-cpubench.hdf.
#
# Requires xdftool (amitools).
#
# IMAGE LAYOUT (recorded, not assumed -- same note as
# patch-virtionet-hdf.sh): nondistribution/m68k-machine.hdf is a PLAIN
# FFS HDF, one volume "SYS", no RDB, so paths are used directly with no
# `open part=` indirection.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Precedence: explicit argument beats $M68K_TEST_HDF beats the default
# (patch-virtionet-hdf.sh records why: an env var silently overriding a
# typed path would be this platform's favourite failure mode).
SRC="${1:-${M68K_TEST_HDF:-$REPO_ROOT/nondistribution/m68k-machine.hdf}}"
OUT="${2:-$REPO_ROOT/nondistribution/m68k-machine-cpubench.hdf}"

CPUBENCH_SRC="$REPO_ROOT/m68k/cpubench/CPUBench"

XDFTOOL="${XDFTOOL:-xdftool}"
if ! command -v "$XDFTOOL" >/dev/null 2>&1; then
    for candidate in \
        "$HOME/.local/bin/xdftool" \
        "$HOME/src/amitools/.venv/bin/xdftool" \
        "$HOME/.venvs/amitools/bin/xdftool"
    do
        if [ -x "$candidate" ]; then
            XDFTOOL="$candidate"
            break
        fi
    done
fi
if ! command -v "$XDFTOOL" >/dev/null 2>&1; then
    echo "error: xdftool not found on PATH, at \$XDFTOOL, or at the usual" >&2
    echo "       amitools venv/pipx locations. Install it with:" >&2
    echo "         pip install amitools" >&2
    exit 1
fi

if [ ! -f "$SRC" ]; then
    echo "error: source image not found: $SRC" >&2
    echo "       (build one with amibake first: tools/amibake/m68k-machine.toml)" >&2
    exit 1
fi
if [ ! -f "$CPUBENCH_SRC" ]; then
    echo "error: $CPUBENCH_SRC not found" >&2
    echo "       build it first with scripts/build-cpubench.sh" >&2
    exit 1
fi

echo "==> src:      $SRC"
echo "==> out:      $OUT"
echo "==> cpubench: $CPUBENCH_SRC"

cp "$SRC" "$OUT"
echo "==> copied $SRC -> $OUT"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/patch-cpubench-hdf.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

# Read-modify-write the Startup-Sequence: prepend a stack bump (CoreMark's
# own auto-iteration calibration and the eleven kernels together are well
# within a normal CLI stack, but this project would rather size it
# explicitly than assume a default that could change under it -- the same
# "ask, don't assume" posture docs/device-ledger.md's standing rules take
# for memory placement, applied here to a different kind of assumption)
# and one CPUBench invocation, ahead of the original content, exactly like
# patch-virtionet-hdf.sh's own prepend (this project's real
# Startup-Sequence ends in `EndCLI >NIL:`, so appending after it would
# never run at all -- confirmed against a real patched image before that
# script's own fix, cited there).
STARTUP_ORIG="$TMP/Startup-Sequence.orig"
STARTUP_NEW="$TMP/Startup-Sequence.new"
"$XDFTOOL" -r "$OUT" read S/Startup-Sequence "$STARTUP_ORIG" >/dev/null

{
    printf 'Stack 100000\n'
    printf 'C:CPUBench\n'
    cat "$STARTUP_ORIG"
} > "$STARTUP_NEW"

"$XDFTOOL" "$OUT" \
    write "$CPUBENCH_SRC" C/CPUBench \
    + delete S/Startup-Sequence \
    + write "$STARTUP_NEW" S/Startup-Sequence
echo "==> wrote C/CPUBench; prepended 'Stack 100000' + 'C:CPUBench' to S/Startup-Sequence"

# --- Verify positively. Absence of complaint is never success on this
# platform: read every changed path back and check concrete evidence. ---

echo "==> verifying image contents"

READBACK_BIN="$TMP/CPUBench.readback"
"$XDFTOOL" -r "$OUT" read C/CPUBench "$READBACK_BIN" >/dev/null
if ! cmp -s "$CPUBENCH_SRC" "$READBACK_BIN"; then
    echo "error: read-back C/CPUBench differs from $CPUBENCH_SRC" >&2
    exit 1
fi
echo "==>   confirmed: C/CPUBench is byte-identical to $CPUBENCH_SRC"

READBACK_STARTUP="$TMP/Startup-Sequence.readback"
"$XDFTOOL" -r "$OUT" read S/Startup-Sequence "$READBACK_STARTUP" >/dev/null
if ! cmp -s "$STARTUP_NEW" "$READBACK_STARTUP"; then
    echo "error: read-back S/Startup-Sequence differs from the patched version" >&2
    exit 1
fi
FIRST_TWO=$(head -2 "$READBACK_STARTUP")
EXPECTED_FIRST_TWO="$(printf 'Stack 100000\nC:CPUBench')"
if [ "$FIRST_TWO" != "$EXPECTED_FIRST_TWO" ]; then
    echo "error: S/Startup-Sequence's first two lines are not 'Stack 100000' / 'C:CPUBench'" >&2
    exit 1
fi
if ! cmp -s "$STARTUP_ORIG" <(tail -n +3 "$READBACK_STARTUP"); then
    echo "error: original Startup-Sequence content not intact below the prepended lines" >&2
    exit 1
fi
echo "==>   confirmed: S/Startup-Sequence = 'Stack 100000' + 'C:CPUBench' + original, intact"

echo "all checks passed"
