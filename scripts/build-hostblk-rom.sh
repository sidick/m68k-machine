#!/usr/bin/env bash
# Rebuilds assets/hostblk-rom/hostblk-diagrom.bin from
# m68k/hostblk-rom/hostblk-diagrom.s, the hostblk card's DiagArea boot ROM
# (docs/hostblk-protocol.md, docs/adr-0003-native-block-storage-doorbell-not-pio.md).
#
# NOT part of the Rust build and NOT run by CI: crates/machine-core embeds
# the *committed* binary via `include_bytes!` (see hostblk.rs), the same
# vendored-binary pattern assets/aros/ uses for AROS, precisely so
# `cargo build`/`cargo test` work on a machine with no m68k toolchain
# installed. Run this script by hand after editing the assembly source,
# check in the rebuilt .bin, and note in the commit message that it
# changed (a rebuilt ROM with no source diff, or vice versa, is a review
# smell).
#
# Requires vasm (Motorola syntax m68k backend) on PATH or at the paths
# this project's toolchain notes use (/opt/amiga/bin).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SRC="$REPO_ROOT/m68k/hostblk-rom/hostblk-diagrom.s"
OUT_DIR="$REPO_ROOT/assets/hostblk-rom"
OUT="$OUT_DIR/hostblk-diagrom.bin"

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

# -Fbin: flat binary, no hunk/object wrapper -- this ROM is served as raw
# bytes from hostblk.rs, not linked or loaded by anything that understands
# Amiga executable formats. -m68000: the DiagArea calling convention and
# every instruction this file uses (move.l (d16,An), moveq, rts) are
# baseline 68000, and this code must run correctly on whatever CPU model
# the guest is configured with, including the plain 68000 default.
"$VASM" -Fbin -m68000 -o "$OUT" "$SRC"

SIZE=$(wc -c < "$OUT" | tr -d ' ')
echo "built $OUT ($SIZE bytes)"

# Sanity check this script and hostblk.rs's embedded-ROM offset math agree
# on where the ROM starts within the board's window, so a future edit to
# one without the other fails loudly here rather than as a silent guest
# boot regression. Kept as a grep rather than a hardcoded number, so this
# script never needs editing when the offset does.
if ! grep -q "pub const ROM_BASE: u32 = 0x1000;" "$REPO_ROOT/crates/machine-core/src/hostblk.rs"; then
    echo "warning: hostblk.rs's ROM_BASE no longer matches this script's expectation (0x1000) -- check hostblk.rs and this comment together" >&2
fi
