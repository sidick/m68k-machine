#!/usr/bin/env bash
# Rebuilds m68k/pktport-handler/pktport-handler from
# m68k/pktport-handler/pktport-handler.s, the pktport card's guest-side
# AmigaDOS handler stub (docs/pktport-protocol.md, ADR 0004).
#
# NOT part of the Rust build and NOT run by CI -- this handler is a real
# AmigaDOS load file (installable at L:pktport-handler, still supported
# for a field-upgrade Mountlist install) AND a flat code blob embedded in
# the pktport card's own boot ROM (m68k/pktport-rom/pktport-diagrom.s,
# built by scripts/build-pktport-rom.sh, which calls this script first).
# Neither output is something crates/machine-core or crates/machine-hosted
# embeds directly. Run this script by hand after editing the assembly
# source, and check in both rebuilt binaries alongside it: mirrors this
# project's own policy for assets/hostblk-rom/hostblk-diagrom.bin
# (committed, not gitignored -- see that file's own build script), so
# that scripts/pktport-e2e.sh, scripts/pktport-rom-e2e.sh and anyone else
# using this handler don't need a working m68k toolchain just to run the
# end-to-end proofs.
#
# Requires vasm (Motorola syntax m68k backend) on PATH or at the paths
# this project's toolchain notes use (/opt/amiga/bin).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SRC="$REPO_ROOT/m68k/pktport-handler/pktport-handler.s"
OUT="$REPO_ROOT/m68k/pktport-handler/pktport-handler"
OUT_BIN="$REPO_ROOT/m68k/pktport-handler/pktport-handler.bin"

VASM="${VASM:-vasmm68k_mot}"
if ! command -v "$VASM" >/dev/null 2>&1; then
    if [ -x /opt/amiga/bin/vasmm68k_mot ]; then
        VASM=/opt/amiga/bin/vasmm68k_mot
    else
        echo "error: vasmm68k_mot not found on PATH or at /opt/amiga/bin; set \$VASM" >&2
        exit 1
    fi
fi

# -Fhunkexe: a standard AmigaDOS load file (hunk EXECUTABLE format, with
# relocation records) -- still installable at L:pktport-handler and still
# LoadSeg()'d by AmigaDOS the ordinary way (pktport-handler.s's own
# header, "Position independence"). -m68000: the same baseline-CPU
# requirement hostblk-diagrom.s's build script states, for the same
# reason -- this handler must run correctly on whatever CPU model the
# guest is configured with.
"$VASM" -Fhunkexe -m68000 -o "$OUT" "$SRC"
chmod +x "$OUT"

# -Fbin: the SAME source, assembled a second time to a flat, headerless
# code blob -- what pktport-diagrom.s actually embeds and hand-loads via
# its own fabricated one-segment seglist. One vasm invocation on the same
# file, rather than extracting the CODE hunk out of the -Fhunkexe output
# above: simpler, and correct for the same reason hostblk-diagrom.s's own
# -Fbin ROM image is correct -- this file's own PIC discipline
# (pktport-handler.s's header, "Position independence"), not anything
# hunk-format-specific.
"$VASM" -Fbin -m68000 -o "$OUT_BIN" "$SRC"

SIZE=$(wc -c < "$OUT" | tr -d ' ')
SIZE_BIN=$(wc -c < "$OUT_BIN" | tr -d ' ')
echo "built $OUT ($SIZE bytes)"
echo "built $OUT_BIN ($SIZE_BIN bytes)"
