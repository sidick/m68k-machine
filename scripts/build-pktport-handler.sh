#!/usr/bin/env bash
# Rebuilds m68k/pktport-handler/pktport-handler from
# m68k/pktport-handler/pktport-handler.s, the pktport card's guest-side
# AmigaDOS handler stub (docs/pktport-protocol.md, ADR 0004).
#
# NOT part of the Rust build and NOT run by CI -- this handler is a real
# AmigaDOS load file installed onto a disk image (L:pktport-handler), not
# something crates/machine-core or crates/machine-hosted embeds. Run this
# script by hand after editing the assembly source, and check in the
# rebuilt binary alongside it: mirrors this project's own policy for
# assets/hostblk-rom/hostblk-diagrom.bin (committed, not gitignored --
# see that file's own build script), so that scripts/pktport-e2e.sh and
# anyone else installing this handler onto a test image don't need a
# working m68k toolchain just to run the end-to-end proof.
#
# Requires vasm (Motorola syntax m68k backend) on PATH or at the paths
# this project's toolchain notes use (/opt/amiga/bin).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SRC="$REPO_ROOT/m68k/pktport-handler/pktport-handler.s"
OUT="$REPO_ROOT/m68k/pktport-handler/pktport-handler"

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
# relocation records) -- this handler is LoadSeg()'d by AmigaDOS itself
# when a client mounts PKT0:, unlike hostblk-diagrom.s's flat -Fbin ROM
# image (see pktport-handler.s's own header for why that distinction
# changes how the file addresses its own data). -m68000: the same
# baseline-CPU requirement hostblk-diagrom.s's build script states, for
# the same reason -- this handler must run correctly on whatever CPU
# model the guest is configured with.
"$VASM" -Fhunkexe -m68000 -o "$OUT" "$SRC"

chmod +x "$OUT"

SIZE=$(wc -c < "$OUT" | tr -d ' ')
echo "built $OUT ($SIZE bytes)"
