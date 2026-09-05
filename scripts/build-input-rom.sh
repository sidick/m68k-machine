#!/usr/bin/env bash
# Rebuilds assets/input-rom/input-diagrom.bin from
# m68k/input-rom/input-diagrom.s, the native input card's DiagArea boot ROM
# and guest driver task (docs/input-protocol.md).
#
# NOT part of the Rust build and NOT run by CI: crates/machine-core embeds
# the *committed* binary via `include_bytes!` (see input.rs), the same
# vendored-binary pattern assets/hostblk-rom/ uses for hostblk, precisely so
# `cargo build`/`cargo test` work on a machine with no m68k toolchain
# installed. Run this script by hand after editing the assembly source,
# check in the rebuilt .bin, and note in the commit message that it
# changed (a rebuilt ROM with no source diff, or vice versa, is a review
# smell -- scripts/build-hostblk-rom.sh's own comment says the same).
#
# Requires vasm (Motorola syntax m68k backend) on PATH or at the paths
# this project's toolchain notes use (/opt/amiga/bin).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SRC="$REPO_ROOT/m68k/input-rom/input-diagrom.s"
OUT_DIR="$REPO_ROOT/assets/input-rom"
OUT="$OUT_DIR/input-diagrom.bin"

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

# -Fbin: flat binary, no hunk/object wrapper. -m68000: this ROM must run
# correctly on whatever CPU model the guest is configured with, including
# the plain 68000 default (identical reasoning to
# scripts/build-hostblk-rom.sh).
"$VASM" -Fbin -m68000 -o "$OUT" "$SRC"

SIZE=$(wc -c < "$OUT" | tr -d ' ')
echo "built $OUT ($SIZE bytes)"

# Sanity check this script and input.rs's embedded-ROM offset math agree on
# where the ROM starts within the board's window (scripts/build-hostblk-rom.sh's
# identical check for its own ROM_BASE).
if ! grep -q "pub const ROM_BASE: u32 = 0x1000;" "$REPO_ROOT/crates/machine-core/src/input.rs"; then
    echo "warning: input.rs's ROM_BASE no longer matches this script's expectation (0x1000) -- check input.rs and this comment together" >&2
fi
