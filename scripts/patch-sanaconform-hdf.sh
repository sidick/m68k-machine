#!/usr/bin/env bash
# Patches an already-built amibake HDF to install virtionet.device and
# SanaConform (a SANA-II Rev 2/3/4/7 conformance probe), and to run the
# probe once at boot from the startup-sequence. Extends
# scripts/patch-virtionet-hdf.sh's pattern exactly (same reasoning: amibake
# manifests cannot install loose local files today, and the standing
# instruction for this stage is delivery by post-generation HDF patching
# without modifying amibake).
#
# SanaConform provenance: prebuilt m68k binary from
# ~/src/sana2loop/build/SanaConform -- sana2loop is the project owner's own
# hardware-free SANA-II loopback.device project (BSD 2-Clause), freely
# copyable/redistributable here (the amirfb/sana2loop precedent
# docs/virtionet.md §7 already records for this driver's own sana2.h
# provenance). Not built by this repo's own toolchain -- just installed,
# byte-compared, same as every other component this script writes.
#
# DEPENDENCY ON THE VIRTIONET PATCH's OWN DEPENDENCY: virtionet.device
# needs LIBS:prometheus.library (ADR 0005 stage 2), exactly like PCIProbe
# and virtionet.device itself do. Rather than depend on
# scripts/patch-virtionet-hdf.sh's own output image by path (a moving
# target across re-runs), this script installs Libs/prometheus.library
# ITSELF if the source image doesn't already carry it, mirroring how
# patch-virtionet-hdf.sh mirrors patch-pciprobe-hdf.sh's own dependency
# check. If the source image already has it, it is left untouched and
# only read back for the final byte-compare.
#
# NO VNetTest ON THIS IMAGE (deliberate, task brief): SanaConform's own
# self-echo probe transmits frames on its own account, and this image is
# meant to stay a clean single-opener conformance gate, not compose with
# VNetTest's own transmit (which would also break the first-packet
# fixture's "exactly one frame" assertion if the two ever shared an
# image). virtionet.device is single-opener, and SanaConform is the only
# opener on this image, so CONFIG/ONLINE/DEVICEQUERY/etc. all exercise a
# fresh Open -- and the driver's DevInit chain runs at that OpenDevice
# call, so SanaConform alone exercises the full init this gate cares
# about.
#
# Usage: patch-sanaconform-hdf.sh [src.hdf] [out.hdf]
#   src.hdf defaults to $REPO_ROOT/nondistribution/m68k-machine.hdf,
#   overridable via $M68K_TEST_HDF.
#   out.hdf defaults to
#   $REPO_ROOT/nondistribution/m68k-machine-sanaconform.hdf.
#
# Requires xdftool (amitools).
#
# IMAGE LAYOUT (recorded, not assumed -- same note as
# patch-virtionet-hdf.sh): nondistribution/m68k-machine.hdf is a PLAIN FFS
# HDF, one volume "SYS", no RDB, so paths are used directly with no
# `open part=` indirection.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Precedence: explicit argument beats $M68K_TEST_HDF beats the default
# (patch-pciprobe-hdf.sh records why: an env var silently overriding a
# typed path would be this platform's favourite failure mode).
SRC="${1:-${M68K_TEST_HDF:-$REPO_ROOT/nondistribution/m68k-machine.hdf}}"
OUT="${2:-$REPO_ROOT/nondistribution/m68k-machine-sanaconform.hdf}"

LIB_SRC="$REPO_ROOT/m68k/prometheus-library/prometheus.library"
DEV_SRC="$REPO_ROOT/m68k/virtionet-device/virtionet.device"
TOOL_SRC="$HOME/src/sana2loop/build/SanaConform"

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
if [ ! -f "$TOOL_SRC" ]; then
    echo "error: $TOOL_SRC not found" >&2
    echo "       build sana2loop's own SanaConform tool first (see" >&2
    echo "       ~/src/sana2loop's own build instructions)" >&2
    exit 1
fi

echo "==> src:    $SRC"
echo "==> out:    $OUT"
echo "==> lib:    $LIB_SRC"
echo "==> device: $DEV_SRC"
echo "==> tool:   $TOOL_SRC (sana2loop, BSD 2-Clause -- see this script's own"
echo "==>         file-top comment for provenance)"

# --- Stage --------------------------------------------------------------

cp "$SRC" "$OUT"
echo "==> copied $SRC -> $OUT"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/patch-sanaconform-hdf.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

# --- Does the source image already carry prometheus.library? ------------
# (e.g. it IS m68k-machine-virtionet.hdf, or some other image an earlier
# patch already touched). `xdftool list` prints one line per path found;
# grep for the exact leaf name to avoid matching unrelated entries.
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
# 2. Devs:virtionet.device -- the driver under test;
# 3. C:SanaConform -- the probe tool, runnable from any shell;
# 4. S/Startup-Sequence -- PREPEND one SanaConform invocation, mirroring
#    patch-virtionet-hdf.sh's own prepend exactly (an earlier draft of
#    that script appended instead, which is wrong: this project's real
#    Startup-Sequence ends in `EndCLI >NIL:`, and anything appended after
#    that line never runs at all). Redirect the probe's own stdout to
#    SYS:sanaconform.log rather than narrating over serial (unlike
#    VNetTest/PCIProbe): SanaConform is sana2loop's own unmodified tool,
#    with no serial-narration hooks of this project's own, so a redirected
#    Shell log is this gate's own evidence trail instead.
#    CONFIG works here because each fresh Open resets the unit's
#    configured flag (virtionet_device.c's own DevInit-time reseed), and
#    ONLINE brings the unit up for the probe's self-echo/DEVICEQUERY
#    steps -- see this script's own file-top comment for why no VNetTest
#    shares this image.
STARTUP_ORIG="$TMP/Startup-Sequence.orig"
STARTUP_NEW="$TMP/Startup-Sequence.new"
"$XDFTOOL" -r "$OUT" read S/Startup-Sequence "$STARTUP_ORIG" >/dev/null

printf 'C:SanaConform 0 DEVICE virtionet.device CONFIG ONLINE >SYS:sanaconform.log\n' > "$STARTUP_NEW"
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
    + write "$TOOL_SRC" C/SanaConform
    + delete S/Startup-Sequence
    + write "$STARTUP_NEW" S/Startup-Sequence
)

"$XDFTOOL" "$OUT" "${PATCH_ARGS[@]}"
echo "==> wrote Devs/virtionet.device, C/SanaConform; prepended the SanaConform"
echo "==> invocation to S/Startup-Sequence"
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

READBACK_TOOL="$TMP/SanaConform.readback"
"$XDFTOOL" -r "$OUT" read C/SanaConform "$READBACK_TOOL" >/dev/null
if ! cmp -s "$TOOL_SRC" "$READBACK_TOOL"; then
    echo "error: read-back C/SanaConform differs from $TOOL_SRC" >&2
    exit 1
fi
echo "==>   confirmed: C/SanaConform is byte-identical to $TOOL_SRC"

READBACK_STARTUP="$TMP/Startup-Sequence.readback"
"$XDFTOOL" -r "$OUT" read S/Startup-Sequence "$READBACK_STARTUP" >/dev/null
if ! cmp -s "$STARTUP_NEW" "$READBACK_STARTUP"; then
    echo "error: read-back S/Startup-Sequence differs from the patched version" >&2
    exit 1
fi
if [ "$(head -1 "$READBACK_STARTUP")" != "C:SanaConform 0 DEVICE virtionet.device CONFIG ONLINE >SYS:sanaconform.log" ]; then
    echo "error: S/Startup-Sequence's first line is not the expected SanaConform invocation" >&2
    exit 1
fi
# The original content must survive INTACT below our line -- a truncated
# startup-sequence would boot to a broken system that still greps green.
if ! cmp -s "$STARTUP_ORIG" <(tail -n +2 "$READBACK_STARTUP"); then
    echo "error: original Startup-Sequence content not intact below the prepended line" >&2
    exit 1
fi
echo "==>   confirmed: S/Startup-Sequence = SanaConform invocation + original, intact"

echo "all checks passed"
