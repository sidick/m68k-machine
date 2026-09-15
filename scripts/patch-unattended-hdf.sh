#!/usr/bin/env bash
# Builds the Phase 3 unattended-boot-gate image: a single bootable HDF
# that composes the rtgboard RTG driver, virtionet.device + VNetTest, and
# one line of positive storage evidence, by CHAINING the two existing
# per-feature patch scripts rather than duplicating their logic.
#
# WHY CHAIN, NOT DUPLICATE: scripts/patch-rtgboard-hdf.sh and
# scripts/patch-virtionet-hdf.sh already do the real work -- installing
# their files and positively verifying every changed path -- for their
# own features. Reimplementing that here would give this project two
# sources of truth for the same install steps and the same drift risk
# that pattern already avoids elsewhere in scripts/. Running them as
# stages and adding only this script's own new content (the storage-
# evidence line) keeps exactly one source of truth per feature.
#
# WHY A POST-BUILD PATCH, NOT AN AMIBAKE RECIPE: same standing reason
# scripts/patch-rtgboard-hdf.sh and scripts/patch-virtionet-hdf.sh
# record -- amibake manifests cannot install loose local files today, so
# this project delivers such features by post-generation HDF patching
# instead of touching amibake. See those two scripts' file-top comments
# for the full reasoning; not restated here.
#
# WHY THIS SCRIPT'S OWN LINE EXISTS: the composed unattended-boot gate
# test needs positive storage evidence, not just an implicit "it booted".
# Booting from hostblk alone only shows the machine got as far as
# running a startup-sequence; it says nothing about whether guest-side
# writes to the boot volume actually land and can be read back. Silent
# failure is this platform's norm (the same principle every verify
# block in the two staged scripts already leans on), so this script has
# the guest itself write a marker file and then the test asserts on
# that file by reading it back out of the booted image with xdftool --
# real evidence, not absence of complaint.
#
# Usage: patch-unattended-hdf.sh [src.hdf] [out.hdf]
#   src.hdf defaults to $REPO_ROOT/nondistribution/m68k-machine.hdf,
#   overridable via $M68K_TEST_HDF.
#   out.hdf defaults to
#   $REPO_ROOT/nondistribution/m68k-machine-unattended.hdf.
#
# Requires xdftool (amitools), plus everything the two staged scripts
# themselves require (python3, the built rtgboard.card/prometheus.library/
# virtionet.device/VNetTest artifacts).
#
# IMAGE LAYOUT (recorded, not assumed -- same note as the staged
# scripts): nondistribution/m68k-machine.hdf is a PLAIN FFS HDF, one
# volume "SYS", no RDB, so paths are used directly with no `open part=`
# indirection.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Precedence: explicit argument beats $M68K_TEST_HDF beats the default
# (the staged scripts record why: an env var silently overriding a
# typed path would be this platform's favourite failure mode, a silent
# one).
SRC="${1:-${M68K_TEST_HDF:-$REPO_ROOT/nondistribution/m68k-machine.hdf}}"
OUT="${2:-$REPO_ROOT/nondistribution/m68k-machine-unattended.hdf}"

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

echo "==> src: $SRC"
echo "==> out: $OUT"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/patch-unattended-hdf.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

# --- Stage 1: rtgboard ----------------------------------------------------
# Passed both args explicitly, so patch-rtgboard-hdf.sh's own $M68K_TEST_HDF
# read cannot interfere here even though it is left set in our environment.
# Its own verification narration streams through as part of this script's
# evidence.

echo "==> stage 1: rtgboard (patch-rtgboard-hdf.sh)"
"$SCRIPT_DIR/patch-rtgboard-hdf.sh" "$SRC" "$TMP/stage1-rtgboard.hdf"

# --- Stage 2: virtionet ---------------------------------------------------
# Installs prometheus.library (dependency), virtionet.device, C/VNetTest,
# and prepends C:VNetTest to S/Startup-Sequence.

echo "==> stage 2: virtionet (patch-virtionet-hdf.sh)"
"$SCRIPT_DIR/patch-virtionet-hdf.sh" "$TMP/stage1-rtgboard.hdf" "$OUT"

# --- Stage 3: this script's own work -- storage evidence -----------------
#
# Prepend ONE more line ABOVE C:VNetTest in S/Startup-Sequence, exactly:
#   Echo >SYS:unattended-boot.txt "UNATTENDED-BOOT-STORAGE-OK"
#
# WHY PREPEND, NOT APPEND: this project's real Startup-Sequence ends in
# `EndCLI >NIL:` -- anything appended after that line never runs at all
# (the same trap patch-virtionet-hdf.sh records, confirmed against a
# real patched image before that script's own prepend was settled on).
# The earliest slot is also the least entangled one here: Echo only
# needs dos.library + LIBS:, valid from the very first line.
echo "==> stage 3: storage evidence (this script)"

STARTUP_ORIG="$TMP/Startup-Sequence.orig"
STARTUP_NEW="$TMP/Startup-Sequence.new"
"$XDFTOOL" -r "$OUT" read S/Startup-Sequence "$STARTUP_ORIG" >/dev/null

printf 'Echo >SYS:unattended-boot.txt "UNATTENDED-BOOT-STORAGE-OK"\n' > "$STARTUP_NEW"
cat "$STARTUP_ORIG" >> "$STARTUP_NEW"

"$XDFTOOL" "$OUT" \
    delete S/Startup-Sequence \
    + write "$STARTUP_NEW" S/Startup-Sequence
echo "==> prepended storage-evidence Echo line to S/Startup-Sequence"
echo "==> (above C:VNetTest -- runs first, writes SYS:unattended-boot.txt,"
echo "==> asserted on by the unattended-boot gate test)"

# --- Verify positively. Absence of complaint is never success on this
# platform: read every changed path back and check concrete evidence. ---

echo "==> verifying image contents"

READBACK_STARTUP="$TMP/Startup-Sequence.readback"
"$XDFTOOL" -r "$OUT" read S/Startup-Sequence "$READBACK_STARTUP" >/dev/null

LINE1="$(sed -n '1p' "$READBACK_STARTUP")"
LINE2="$(sed -n '2p' "$READBACK_STARTUP")"
if [ "$LINE1" != 'Echo >SYS:unattended-boot.txt "UNATTENDED-BOOT-STORAGE-OK"' ]; then
    echo "error: S/Startup-Sequence line 1 is not the storage-evidence Echo line" >&2
    echo "       got: $LINE1" >&2
    exit 1
fi
echo "==>   confirmed: S/Startup-Sequence line 1 is the storage-evidence Echo line"

if [ "$LINE2" != "C:VNetTest" ]; then
    echo "error: S/Startup-Sequence line 2 is not 'C:VNetTest'" >&2
    echo "       got: $LINE2" >&2
    exit 1
fi
echo "==>   confirmed: S/Startup-Sequence line 2 is 'C:VNetTest'"

# The remainder (everything from line 2 on) must be byte-identical to
# what stage 2 produced below its own prepended line -- a truncated or
# reordered startup-sequence would boot to a broken system that still
# greps green.
if ! cmp -s "$STARTUP_ORIG" <(tail -n +2 "$READBACK_STARTUP"); then
    echo "error: chained Startup-Sequence content (from stage 2) not intact" >&2
    echo "       below the prepended storage-evidence line" >&2
    exit 1
fi
echo "==>   confirmed: chained Startup-Sequence content intact below our line"

# Spot-check that the chained content from both staged scripts survived
# into $OUT.
RTG_LISTING="$("$XDFTOOL" -r "$OUT" list Libs/Picasso96)"
MONITORS_LISTING="$("$XDFTOOL" -r "$OUT" list Devs/Monitors)"
LIBS_LISTING="$("$XDFTOOL" -r "$OUT" list Libs)"
DEVS_LISTING="$("$XDFTOOL" -r "$OUT" list Devs)"
C_LISTING="$("$XDFTOOL" -r "$OUT" list C)"

for pair in \
    "Libs/Picasso96/rtgboard.card:$RTG_LISTING" \
    "Devs/Monitors/rtgboard:$MONITORS_LISTING" \
    "Libs/prometheus.library:$LIBS_LISTING" \
    "Devs/virtionet.device:$DEVS_LISTING" \
    "C/VNetTest:$C_LISTING"
do
    path="${pair%%:*}"
    listing="${pair#*:}"
    if ! echo "$listing" | grep -q "$(basename "$path")"; then
        echo "error: $path missing from image listing -- chained content did not survive" >&2
        exit 1
    fi
    echo "==>   present: $path"
done

echo "all checks passed"
