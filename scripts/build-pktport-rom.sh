#!/usr/bin/env bash
# Rebuilds assets/pktport-rom/pktport-diagrom.bin from
# m68k/pktport-rom/pktport-diagrom.s (the pktport card's DiagArea boot
# ROM) plus m68k/pktport-handler/pktport-handler.bin (the same handler
# scripts/build-pktport-handler.sh's -Fbin mode produces) -- see
# docs/pktport-protocol.md section 1 and pktport-diagrom.s's own header
# ("ROM layout") for the wire contract this script implements: DiagArea
# and RtInit code first, then a 4-byte big-endian blob length, then the
# handler's flat code blob, byte-for-byte.
#
# NOT part of the Rust build and NOT run by CI: crates/machine-core embeds
# the *committed* output via `include_bytes!` (see pktport.rs), the same
# vendored-binary pattern assets/hostblk-rom/ and assets/input-rom/ use,
# precisely so `cargo build`/`cargo test` work on a machine with no m68k
# toolchain installed. Run this script by hand after editing either
# source, check in the rebuilt .bin, and note in the commit message that
# it changed (a rebuilt ROM with no source diff, or vice versa, is a
# review smell -- hostblk-diagrom's own build script states the same
# rule).
#
# Requires vasm (Motorola syntax m68k backend) on PATH or at the paths
# this project's toolchain notes use (/opt/amiga/bin); Python 3 (for the
# length-patch step -- this project's fixtures/tooling already assume it,
# see scripts/pktport-e2e.sh's own `run_example` helpers for the same
# posture on other tools).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SRC="$REPO_ROOT/m68k/pktport-rom/pktport-diagrom.s"
OUT_DIR="$REPO_ROOT/assets/pktport-rom"
OUT="$OUT_DIR/pktport-diagrom.bin"

VASM="${VASM:-vasmm68k_mot}"
if ! command -v "$VASM" >/dev/null 2>&1; then
    if [ -x /opt/amiga/bin/vasmm68k_mot ]; then
        VASM=/opt/amiga/bin/vasmm68k_mot
    else
        echo "error: vasmm68k_mot not found on PATH or at /opt/amiga/bin; set \$VASM" >&2
        exit 1
    fi
fi

mkdir -p "$OUT_DIR"

echo "== building the handler blob first =="
"$SCRIPT_DIR/build-pktport-handler.sh"
HANDLER_BIN="$REPO_ROOT/m68k/pktport-handler/pktport-handler.bin"
if [ ! -e "$HANDLER_BIN" ]; then
    echo "error: $HANDLER_BIN missing after build-pktport-handler.sh" >&2
    exit 1
fi

echo "== assembling the DiagArea/RtInit ROM code =="
DIAGROM_TMP="$(mktemp)"
trap 'rm -f "$DIAGROM_TMP"' EXIT

# -Fbin, -m68000: identical reasoning to build-hostblk-rom.sh's own
# choice (flat binary, no hunk wrapper, baseline-CPU-safe instructions
# only).
"$VASM" -Fbin -m68000 -o "$DIAGROM_TMP" "$SRC"

DIAGROM_SIZE=$(wc -c < "$DIAGROM_TMP" | tr -d ' ')
if [ "$DIAGROM_SIZE" -lt 4 ]; then
    echo "error: assembled ROM ($DIAGROM_SIZE bytes) too small to hold BlobLenWord" >&2
    exit 1
fi

echo "== patching BlobLenWord and appending the handler blob =="
python3 - "$DIAGROM_TMP" "$HANDLER_BIN" "$OUT" <<'PYEOF'
import sys

diagrom_path, handler_path, out_path = sys.argv[1:4]

with open(diagrom_path, "rb") as f:
    diagrom = bytearray(f.read())
with open(handler_path, "rb") as f:
    handler = f.read()

# pktport-diagrom.s's own header: "BlobLenWord ... must be the LAST thing
# in this source file" -- so its own 4-byte slot is exactly the last 4
# bytes vasm -Fbin emitted.
if len(diagrom) < 4:
    raise SystemExit("assembled ROM too small")
diagrom[-4:] = len(handler).to_bytes(4, "big")

with open(out_path, "wb") as f:
    f.write(diagrom)
    f.write(handler)
PYEOF

SIZE=$(wc -c < "$OUT" | tr -d ' ')
echo "built $OUT ($SIZE bytes: $DIAGROM_SIZE ROM code + $(wc -c < "$HANDLER_BIN" | tr -d ' ') handler blob)"

# Sanity check this script and pktport.rs's embedded-ROM offset math
# agree on where the ROM starts within the board's window -- same
# cross-check build-hostblk-rom.sh already runs for its own ROM_BASE.
if ! grep -q "pub const ROM_BASE: u32 = 0x1000;" "$REPO_ROOT/crates/machine-core/src/pktport.rs"; then
    echo "warning: pktport.rs's ROM_BASE no longer matches this script's expectation (0x1000) -- check pktport.rs and this comment together" >&2
fi
