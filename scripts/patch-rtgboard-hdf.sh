#!/usr/bin/env bash
# Patches an already-built amibake HDF to install the `rtgboard` P96
# `.card` driver (docs/rtgboard-driver-scope.md §4, docs/
# rtgboard-protocol.md).
#
# WHY A POST-BUILD PATCH, NOT AN AMIBAKE RECIPE: amibake manifests cannot
# install loose local files today (tools/amibake/m68k-machine.toml only
# has package-level `card =`/`fake-native-modes =` knobs for the
# Graffity-based picasso96-3 package, nothing that drops in an
# out-of-tree driver binary or a generated settings/icon pair). A local
# amibake recipe or manifest-level `files` support is the real fix
# (docs/rtgboard-driver-scope.md §5.4) and is deliberately deferred; this
# script exists so the driver can be exercised on a real image today.
#
# Usage: patch-rtgboard-hdf.sh [src.hdf] [out.hdf]
#   src.hdf defaults to $REPO_ROOT/nondistribution/m68k-machine.hdf,
#   overridable via $M68K_TEST_HDF.
#   out.hdf defaults to
#   $REPO_ROOT/nondistribution/m68k-machine-rtgboard.hdf.
#
# Requires xdftool (amitools) and python3.
#
# IMAGE LAYOUT (recorded here, not assumed): nondistribution/
# m68k-machine.hdf, as built by amibake, is a PLAIN FFS HDF with NO
# Rigid Disk Block partitioning -- one volume named "SYS"
# (DOS3:ffs+intl), verified with `xdftool m68k-machine.hdf list` against
# the real file. Paths inside it are therefore used directly (Devs/...,
# Libs/Picasso96/...), with none of an RDB image's `open part=<name>`
# indirection. If a future image gains RDB partitioning, the xdftool
# invocations below need an `open part=<volume>` prefix -- this script
# does not attempt to detect that automatically.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Precedence: an explicit argument beats $M68K_TEST_HDF beats the default
# (an env var silently overriding a path the caller just typed would be
# this platform's favourite failure mode, a silent one).
SRC="${1:-${M68K_TEST_HDF:-$REPO_ROOT/nondistribution/m68k-machine.hdf}}"
OUT="${2:-$REPO_ROOT/nondistribution/m68k-machine-rtgboard.hdf}"

CARD_SRC="$REPO_ROOT/m68k/rtgboard-card/rtgboard.card"

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

PYTHON3="${PYTHON3:-python3}"
if ! command -v "$PYTHON3" >/dev/null 2>&1; then
    echo "error: python3 not found on PATH; needed to run the settings/icon generators" >&2
    exit 1
fi

# --- Preconditions ------------------------------------------------------

if [ ! -f "$SRC" ]; then
    echo "error: source image not found: $SRC" >&2
    echo "       (build one with amibake first: tools/amibake/m68k-machine.toml)" >&2
    exit 1
fi

if [ ! -f "$CARD_SRC" ]; then
    echo "error: $CARD_SRC not found" >&2
    echo "       build it first with scripts/build-rtgboard-card.sh" >&2
    exit 1
fi

echo "==> src:  $SRC"
echo "==> out:  $OUT"
echo "==> card: $CARD_SRC"

# --- Stage --------------------------------------------------------------

cp "$SRC" "$OUT"
echo "==> copied $SRC -> $OUT"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/patch-rtgboard-hdf.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

SETTINGS="$TMP/Picasso96Settings"
INFO="$TMP/rtgboard.info"
SCREENMODE="$TMP/ScreenMode.prefs"
GRAFFITY_STUB="$TMP/Graffity"

"$PYTHON3" "$SCRIPT_DIR/../tools/rtgboard/make_settings.py" "$SETTINGS" rtgboard
"$PYTHON3" "$SCRIPT_DIR/../tools/rtgboard/make_monitor_info.py" "$INFO"
"$PYTHON3" "$SCRIPT_DIR/../tools/rtgboard/make_screenmode_prefs.py" "$SCREENMODE"
echo "==> generated $SETTINGS, $INFO and $SCREENMODE"

# --- Patch the OUTPUT image (never the source) --------------------------
#
# 1. drop in the built driver;
# 2. replace (not add to) Devs/Picasso96Settings -- both monitors sharing
#    one settings file is exactly the collision docs/rtgboard-driver-
#    scope.md §4 warns about, so Graffity's entries must not survive
#    alongside ours;
# 3. clone the existing Devs/Monitors/Graffity monitor stub binary as
#    Devs/Monitors/rtgboard -- the real P96 monitor stub binary is not
#    redistributable (it's a licensed Village Tronic/iComp artifact), but
#    copying it within the user's own already-licensed image is fine,
#    the same reasoning amirfb's own install script uses for AmiRFBMon;
# 4. drop in our from-scratch icon as Devs/Monitors/rtgboard.info;
# 5. delete the Graffity monitor entries -- a driver-less Graffity
#    monitor (no Graffity.card reference left once we've replaced the
#    settings file) would be a needless variable, and leaving it also
#    recreates the two-monitors-one-settings-file collision from (2).
"$XDFTOOL" "$OUT" \
    write "$CARD_SRC" Libs/Picasso96/rtgboard.card \
    + delete Devs/Picasso96Settings \
    + write "$SETTINGS" Devs/Picasso96Settings \
    + read Devs/Monitors/Graffity "$GRAFFITY_STUB" \
    + write "$GRAFFITY_STUB" Devs/Monitors/rtgboard \
    + write "$INFO" Devs/Monitors/rtgboard.info \
    + delete Devs/Monitors/Graffity \
    + delete Devs/Monitors/Graffity.info \
    + write "$SCREENMODE" Prefs/Env-Archive/Sys/ScreenMode.prefs
echo "==> wrote rtgboard.card, Picasso96Settings, Devs/Monitors/rtgboard{,.info},"
echo "==> ScreenMode.prefs (steers Workbench onto the 16-bit mode -- fake"
echo "==> native modes are CLUT screens this board has no hardware for);"
echo "==> removed Graffity monitor"

# --- Verify positively. Absence of complaint is never success on this
# platform: read every changed path back and check concrete evidence. ---

echo "==> verifying image contents"
MONITORS_LISTING="$("$XDFTOOL" -r "$OUT" list Devs/Monitors)"
PICASSO96_LISTING="$("$XDFTOOL" -r "$OUT" list Libs/Picasso96)"
DEVS_LISTING="$("$XDFTOOL" -r "$OUT" list Devs)"
echo "$MONITORS_LISTING"
echo "$PICASSO96_LISTING"
echo "$DEVS_LISTING"

for pair in \
    "Libs/Picasso96/rtgboard.card:$PICASSO96_LISTING" \
    "Devs/Picasso96Settings:$DEVS_LISTING" \
    "Devs/Monitors/rtgboard:$MONITORS_LISTING" \
    "Devs/Monitors/rtgboard.info:$MONITORS_LISTING"
do
    path="${pair%%:*}"
    listing="${pair#*:}"
    if ! echo "$listing" | grep -q "$(basename "$path")"; then
        echo "error: $path missing from image listing" >&2
        exit 1
    fi
    echo "==>   present: $path"
done

# Scoped to Devs/Monitors only -- Libs/Picasso96/Graffity.card and the
# CirrusGD542X.chip it drives are untouched by this script (out of
# scope: we only replace the *monitor* entries, not the Graffity board
# driver itself), so a whole-image Graffity search would false-positive
# on those.
if echo "$MONITORS_LISTING" | grep -qi "Graffity"; then
    echo "error: Graffity monitor entries still present under Devs/Monitors" >&2
    exit 1
fi
echo "==>   confirmed: no Graffity monitor entries remain under Devs/Monitors"

# The monitor stub deserves more than a listing grep ("rtgboard" in the
# listing also matches rtgboard.info, so the grep above can't tell a
# missing stub from a present one): read it back and byte-compare against
# the Graffity stub we extracted mid-patch -- the copy is only real if
# the bytes round-tripped.
READBACK_STUB="$TMP/rtgboard.monitor.readback"
"$XDFTOOL" -r "$OUT" read Devs/Monitors/rtgboard "$READBACK_STUB" >/dev/null
if ! cmp -s "$GRAFFITY_STUB" "$READBACK_STUB"; then
    echo "error: read-back Devs/Monitors/rtgboard differs from the extracted monitor stub" >&2
    exit 1
fi
echo "==>   confirmed: Devs/Monitors/rtgboard is byte-identical to the original monitor stub"

# Byte-for-byte compare the read-back driver against the source binary.
READBACK_CARD="$TMP/rtgboard.card.readback"
"$XDFTOOL" -r "$OUT" read Libs/Picasso96/rtgboard.card "$READBACK_CARD" >/dev/null
if ! cmp -s "$CARD_SRC" "$READBACK_CARD"; then
    echo "error: read-back rtgboard.card differs from $CARD_SRC" >&2
    exit 1
fi
echo "==>   confirmed: read-back rtgboard.card is byte-identical to $CARD_SRC"

# Settings file: read back and check the FORM....P96S IFF magic.
READBACK_SETTINGS="$TMP/Picasso96Settings.readback"
"$XDFTOOL" -r "$OUT" read Devs/Picasso96Settings "$READBACK_SETTINGS" >/dev/null
MAGIC_FORM=$(od -An -c -N 4 "$READBACK_SETTINGS" | tr -s ' ')
MAGIC_TYPE=$(od -An -c -j 8 -N 4 "$READBACK_SETTINGS" | tr -s ' ')
SETTINGS_SIZE=$(wc -c < "$READBACK_SETTINGS" | tr -d ' ')
if [ "$(od -An -t x1 -N 4 "$READBACK_SETTINGS" | tr -d ' \n')" != "464f524d" ]; then
    echo "error: read-back Picasso96Settings does not start with 'FORM' (magic: $MAGIC_FORM)" >&2
    exit 1
fi
if [ "$(od -An -t x1 -j 8 -N 4 "$READBACK_SETTINGS" | tr -d ' \n')" != "50393653" ]; then
    echo "error: read-back Picasso96Settings is not IFF type 'P96S' (type: $MAGIC_TYPE)" >&2
    exit 1
fi
echo "==>   confirmed: Picasso96Settings starts with FORM....P96S ($SETTINGS_SIZE bytes)"

# ScreenMode.prefs: read back and byte-compare against what we generated.
READBACK_SCREENMODE="$TMP/ScreenMode.prefs.readback"
"$XDFTOOL" -r "$OUT" read Prefs/Env-Archive/Sys/ScreenMode.prefs "$READBACK_SCREENMODE" >/dev/null
if ! cmp -s "$SCREENMODE" "$READBACK_SCREENMODE"; then
    echo "error: read-back ScreenMode.prefs differs from the generated file" >&2
    exit 1
fi
echo "==>   confirmed: ENVARC:Sys/ScreenMode.prefs is byte-identical to the generated file"

echo "all checks passed"
