#!/usr/bin/env bash
# Patches an already-built amibake HDF to install prometheus.library and
# the PCIProbe tool (ADR 0005 stage 2, docs/pci-library.md §6/§8), and to
# run the probe once at boot from the startup-sequence.
#
# WHY A POST-BUILD PATCH, NOT AN AMIBAKE RECIPE: the same reason
# patch-rtgboard-hdf.sh gives -- amibake manifests cannot install loose
# local files today, and the standing instruction for this stage is to
# deliver by post-generation HDF patching without modifying amibake.
#
# Usage: patch-pciprobe-hdf.sh [src.hdf] [out.hdf]
#   src.hdf defaults to $REPO_ROOT/nondistribution/m68k-machine.hdf,
#   overridable via $M68K_TEST_HDF.
#   out.hdf defaults to
#   $REPO_ROOT/nondistribution/m68k-machine-pciprobe.hdf.
#
# Requires xdftool (amitools).
#
# IMAGE LAYOUT (recorded, not assumed -- same note as
# patch-rtgboard-hdf.sh): nondistribution/m68k-machine.hdf is a PLAIN FFS
# HDF, one volume "SYS", no RDB, so paths are used directly with no
# `open part=` indirection.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Precedence: explicit argument beats $M68K_TEST_HDF beats the default
# (patch-rtgboard-hdf.sh records why: an env var silently overriding a
# typed path would be this platform's favourite failure mode).
SRC="${1:-${M68K_TEST_HDF:-$REPO_ROOT/nondistribution/m68k-machine.hdf}}"
OUT="${2:-$REPO_ROOT/nondistribution/m68k-machine-pciprobe.hdf}"

LIB_SRC="$REPO_ROOT/m68k/prometheus-library/prometheus.library"
PROBE_SRC="$REPO_ROOT/m68k/pciprobe/PCIProbe"

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
if [ ! -f "$PROBE_SRC" ]; then
    echo "error: $PROBE_SRC not found" >&2
    echo "       build it first with scripts/build-pciprobe.sh" >&2
    exit 1
fi

echo "==> src:   $SRC"
echo "==> out:   $OUT"
echo "==> lib:   $LIB_SRC"
echo "==> probe: $PROBE_SRC"

# --- Stage --------------------------------------------------------------

cp "$SRC" "$OUT"
echo "==> copied $SRC -> $OUT"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/patch-pciprobe-hdf.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

# --- Patch the OUTPUT image (never the source) --------------------------
#
# 1. LIBS:prometheus.library -- the deliverable itself (the name
#    openpci.library's wrapper and every period driver opens, docs/
#    pci-library.md's own naming decision);
# 2. C:PCIProbe -- the probe tool, runnable from any shell;
# 3. S/Startup-Sequence -- prepend one PCIProbe invocation so a plain
#    unattended boot produces the probe's serial evidence before
#    Workbench loads. Read-modify-write: xdftool has no append, and the
#    original line order must survive byte-for-byte after our one line.
STARTUP_ORIG="$TMP/Startup-Sequence.orig"
STARTUP_NEW="$TMP/Startup-Sequence.new"
"$XDFTOOL" -r "$OUT" read S/Startup-Sequence "$STARTUP_ORIG" >/dev/null

# One line, AmigaDOS LF line ending, run before everything else: the
# probe needs only dos.library + LIBS: (valid from boot) and narrates
# over serial, so the earliest slot is the least entangled one.
printf 'C:PCIProbe\n' > "$STARTUP_NEW"
cat "$STARTUP_ORIG" >> "$STARTUP_NEW"

"$XDFTOOL" "$OUT" \
    write "$LIB_SRC" Libs/prometheus.library \
    + write "$PROBE_SRC" C/PCIProbe \
    + delete S/Startup-Sequence \
    + write "$STARTUP_NEW" S/Startup-Sequence
echo "==> wrote Libs/prometheus.library, C/PCIProbe; prepended C:PCIProbe"
echo "==> to S/Startup-Sequence"

# --- Verify positively. Absence of complaint is never success on this
# platform: read every changed path back and check concrete evidence. ---

echo "==> verifying image contents"

READBACK_LIB="$TMP/prometheus.library.readback"
"$XDFTOOL" -r "$OUT" read Libs/prometheus.library "$READBACK_LIB" >/dev/null
if ! cmp -s "$LIB_SRC" "$READBACK_LIB"; then
    echo "error: read-back Libs/prometheus.library differs from $LIB_SRC" >&2
    exit 1
fi
echo "==>   confirmed: Libs/prometheus.library is byte-identical to $LIB_SRC"

READBACK_PROBE="$TMP/PCIProbe.readback"
"$XDFTOOL" -r "$OUT" read C/PCIProbe "$READBACK_PROBE" >/dev/null
if ! cmp -s "$PROBE_SRC" "$READBACK_PROBE"; then
    echo "error: read-back C/PCIProbe differs from $PROBE_SRC" >&2
    exit 1
fi
echo "==>   confirmed: C/PCIProbe is byte-identical to $PROBE_SRC"

READBACK_STARTUP="$TMP/Startup-Sequence.readback"
"$XDFTOOL" -r "$OUT" read S/Startup-Sequence "$READBACK_STARTUP" >/dev/null
if ! cmp -s "$STARTUP_NEW" "$READBACK_STARTUP"; then
    echo "error: read-back S/Startup-Sequence differs from the patched version" >&2
    exit 1
fi
if [ "$(head -1 "$READBACK_STARTUP")" != "C:PCIProbe" ]; then
    echo "error: S/Startup-Sequence's first line is not 'C:PCIProbe'" >&2
    exit 1
fi
# The original content must survive INTACT below our line -- a truncated
# startup-sequence would boot to a broken system that still greps green.
if ! cmp -s "$STARTUP_ORIG" <(tail -n +2 "$READBACK_STARTUP"); then
    echo "error: original Startup-Sequence content not intact after the prepended line" >&2
    exit 1
fi
echo "==>   confirmed: S/Startup-Sequence = 'C:PCIProbe' + original, intact"

echo "all checks passed"
