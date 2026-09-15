#!/usr/bin/env bash
# Patches an already-built amibake HDF to install virtionet.device and
# VNetTest (ADR 0005 stage 3, guest side), and to run the test once at
# boot from the startup-sequence. Extends scripts/patch-pciprobe-hdf.sh's
# pattern exactly (same reasoning: amibake manifests cannot install loose
# local files today, and the standing instruction for this stage is
# delivery by post-generation HDF patching without modifying amibake).
#
# DEPENDENCY ON THE PCIPROBE PATCH: virtionet.device needs
# LIBS:prometheus.library (ADR 0005 stage 2), exactly like PCIProbe does.
# Rather than depend on scripts/patch-pciprobe-hdf.sh's own output image
# by path (a moving target across re-runs), this script installs
# Libs/prometheus.library ITSELF if the source image doesn't already
# carry it -- mirroring how patch-pciprobe-hdf.sh installs its own
# dependency (nothing) and matching the task brief's own instruction
# ("check and either depend on it or install it too, mirroring how that
# script works"). If the source image already has it (e.g. it IS the
# pciprobe-patched image), it is left untouched and only read back for
# the final byte-compare.
#
# Usage: patch-virtionet-hdf.sh [src.hdf] [out.hdf]
#   src.hdf defaults to $REPO_ROOT/nondistribution/m68k-machine.hdf,
#   overridable via $M68K_TEST_HDF.
#   out.hdf defaults to
#   $REPO_ROOT/nondistribution/m68k-machine-virtionet.hdf.
#
# Requires xdftool (amitools).
#
# IMAGE LAYOUT (recorded, not assumed -- same note as
# patch-pciprobe-hdf.sh): nondistribution/m68k-machine.hdf is a PLAIN FFS
# HDF, one volume "SYS", no RDB, so paths are used directly with no
# `open part=` indirection.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Precedence: explicit argument beats $M68K_TEST_HDF beats the default
# (patch-pciprobe-hdf.sh records why: an env var silently overriding a
# typed path would be this platform's favourite failure mode).
SRC="${1:-${M68K_TEST_HDF:-$REPO_ROOT/nondistribution/m68k-machine.hdf}}"
OUT="${2:-$REPO_ROOT/nondistribution/m68k-machine-virtionet.hdf}"

LIB_SRC="$REPO_ROOT/m68k/prometheus-library/prometheus.library"
DEV_SRC="$REPO_ROOT/m68k/virtionet-device/virtionet.device"
TEST_SRC="$REPO_ROOT/m68k/vnettest/VNetTest"

# --- Tool discovery ----------------------------------------------------

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

# --- Preconditions ------------------------------------------------------

if [ ! -f "$SRC" ]; then
    echo "error: source image not found: $SRC" >&2
    echo "       (build one with amibake first: tools/amibake/m68k-machine.toml)" >&2
    exit 1
fi
if [ ! -f "$LIB_SRC" ]; then
    echo "error: $LIB_SRC not found" >&2
    echo "       build it first with scripts/build-prometheus-library.sh" >&2
    exit 1
fi
if [ ! -f "$DEV_SRC" ]; then
    echo "error: $DEV_SRC not found" >&2
    echo "       build it first with scripts/build-virtionet-device.sh" >&2
    exit 1
fi
if [ ! -f "$TEST_SRC" ]; then
    echo "error: $TEST_SRC not found" >&2
    echo "       build it first with scripts/build-vnettest.sh" >&2
    exit 1
fi

echo "==> src:    $SRC"
echo "==> out:    $OUT"
echo "==> lib:    $LIB_SRC"
echo "==> device: $DEV_SRC"
echo "==> test:   $TEST_SRC"

# --- Stage --------------------------------------------------------------

cp "$SRC" "$OUT"
echo "==> copied $SRC -> $OUT"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/patch-virtionet-hdf.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

# --- Does the source image already carry prometheus.library? ------------
# (e.g. it IS m68k-machine-pciprobe.hdf, or some other image the
# pciprobe patch already touched). `xdftool list` prints one line per
# path found; grep for the exact leaf name to avoid matching unrelated
# entries.
NEED_PROMETHEUS_LIB=1
if "$XDFTOOL" -r "$OUT" list Libs 2>/dev/null | awk '{print $1}' | grep -qx "prometheus.library"; then
    NEED_PROMETHEUS_LIB=0
    echo "==> Libs/prometheus.library already present on the source image -- not reinstalling it"
else
    echo "==> Libs/prometheus.library not found on the source image -- installing it (dependency)"
fi

# --- Patch the OUTPUT image (never the source) --------------------------
#
# 1. LIBS:prometheus.library -- only if not already present (dependency,
#    see file-top comment);
# 2. Devs:virtionet.device -- the deliverable itself;
# 3. C:VNetTest -- the test tool, runnable from any shell;
# 4. S/Startup-Sequence -- PREPEND one VNetTest invocation, mirroring
#    patch-pciprobe-hdf.sh's own prepend exactly (an earlier draft of
#    this script appended instead, which is wrong: this project's real
#    Startup-Sequence ends in `EndCLI >NIL:`, and anything appended
#    after that line never runs at all -- confirmed against the actual
#    pciprobe-patched image's Startup-Sequence before fixing this).
#    VNetTest only needs dos.library + LIBS: (valid from boot,
#    virtionet.device loads lazily via its own OpenDevice call), so the
#    earliest slot is the least entangled one here too. Read-modify-
#    write: xdftool has no prepend, and the original line order must
#    survive byte-for-byte below our one added line.
STARTUP_ORIG="$TMP/Startup-Sequence.orig"
STARTUP_NEW="$TMP/Startup-Sequence.new"
"$XDFTOOL" -r "$OUT" read S/Startup-Sequence "$STARTUP_ORIG" >/dev/null

printf 'C:VNetTest\n' > "$STARTUP_NEW"
cat "$STARTUP_ORIG" >> "$STARTUP_NEW"

# Devs/ already exists on every real OS image this project produces (a
# full amibake install carries clipboard.device, serial.device, etc.) --
# `makedir` on an existing directory is a hard xdftool error, not a
# no-op, so only ask for it when genuinely missing (kept for a minimal/
# stripped source image that might not have it at all).
NEED_DEVS_DIR=1
if "$XDFTOOL" -r "$OUT" list "" 2>/dev/null | awk '{print $1}' | grep -qx "Devs"; then
    NEED_DEVS_DIR=0
fi

PATCH_ARGS=()
if [ "$NEED_PROMETHEUS_LIB" -eq 1 ]; then
    PATCH_ARGS+=(write "$LIB_SRC" Libs/prometheus.library +)
fi
if [ "$NEED_DEVS_DIR" -eq 1 ]; then
    PATCH_ARGS+=(makedir Devs +)
fi
PATCH_ARGS+=(
    write "$DEV_SRC" Devs/virtionet.device
    + write "$TEST_SRC" C/VNetTest
    + delete S/Startup-Sequence
    + write "$STARTUP_NEW" S/Startup-Sequence
)

"$XDFTOOL" "$OUT" "${PATCH_ARGS[@]}"
echo "==> wrote Devs/virtionet.device, C/VNetTest; prepended C:VNetTest"
echo "==> to S/Startup-Sequence"
if [ "$NEED_PROMETHEUS_LIB" -eq 1 ]; then
    echo "==> also wrote Libs/prometheus.library (dependency)"
fi

# --- Verify positively. Absence of complaint is never success on this
# platform: read every changed path back and check concrete evidence. ---

echo "==> verifying image contents"

if [ "$NEED_PROMETHEUS_LIB" -eq 1 ]; then
    READBACK_LIB="$TMP/prometheus.library.readback"
    "$XDFTOOL" -r "$OUT" read Libs/prometheus.library "$READBACK_LIB" >/dev/null
    if ! cmp -s "$LIB_SRC" "$READBACK_LIB"; then
        echo "error: read-back Libs/prometheus.library differs from $LIB_SRC" >&2
        exit 1
    fi
    echo "==>   confirmed: Libs/prometheus.library is byte-identical to $LIB_SRC"
else
    READBACK_LIB="$TMP/prometheus.library.readback"
    "$XDFTOOL" -r "$OUT" read Libs/prometheus.library "$READBACK_LIB" >/dev/null
    if ! cmp -s "$LIB_SRC" "$READBACK_LIB"; then
        echo "error: pre-existing Libs/prometheus.library on the source image" >&2
        echo "       differs from $LIB_SRC -- refusing to assume it is the" >&2
        echo "       same build" >&2
        exit 1
    fi
    echo "==>   confirmed: pre-existing Libs/prometheus.library matches $LIB_SRC"
fi

READBACK_DEV="$TMP/virtionet.device.readback"
"$XDFTOOL" -r "$OUT" read Devs/virtionet.device "$READBACK_DEV" >/dev/null
if ! cmp -s "$DEV_SRC" "$READBACK_DEV"; then
    echo "error: read-back Devs/virtionet.device differs from $DEV_SRC" >&2
    exit 1
fi
echo "==>   confirmed: Devs/virtionet.device is byte-identical to $DEV_SRC"

READBACK_TEST="$TMP/VNetTest.readback"
"$XDFTOOL" -r "$OUT" read C/VNetTest "$READBACK_TEST" >/dev/null
if ! cmp -s "$TEST_SRC" "$READBACK_TEST"; then
    echo "error: read-back C/VNetTest differs from $TEST_SRC" >&2
    exit 1
fi
echo "==>   confirmed: C/VNetTest is byte-identical to $TEST_SRC"

READBACK_STARTUP="$TMP/Startup-Sequence.readback"
"$XDFTOOL" -r "$OUT" read S/Startup-Sequence "$READBACK_STARTUP" >/dev/null
if ! cmp -s "$STARTUP_NEW" "$READBACK_STARTUP"; then
    echo "error: read-back S/Startup-Sequence differs from the patched version" >&2
    exit 1
fi
if [ "$(head -1 "$READBACK_STARTUP")" != "C:VNetTest" ]; then
    echo "error: S/Startup-Sequence's first line is not 'C:VNetTest'" >&2
    exit 1
fi
# The original content must survive INTACT below our line -- a truncated
# startup-sequence would boot to a broken system that still greps green.
if ! cmp -s "$STARTUP_ORIG" <(tail -n +2 "$READBACK_STARTUP"); then
    echo "error: original Startup-Sequence content not intact below the prepended line" >&2
    exit 1
fi
echo "==>   confirmed: S/Startup-Sequence = 'C:VNetTest' + original, intact"

echo "all checks passed"
