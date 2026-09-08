#!/usr/bin/env bash
# End-to-end proof of the pktport card's DiagArea autoload path
# (docs/pktport-protocol.md section 1, ADR 0004): unlike scripts/
# pktport-e2e.sh, this script installs NOTHING onto the boot image --
# no L:pktport-handler, no DEVS:Mountlist.pktport, and it never runs
# `Mount PKT0:` at all. The card's own boot ROM
# (m68k/pktport-rom/pktport-diagrom.s) is expected to make PKT0: exist
# and work from cold boot, purely because the card and --pktvol are
# present. This script boots straight to `Dir PKT0:` and an
# `Echo ... >PKT0:marker` write, then -- the decisive step, not vibes --
# reads PKT0:marker back from the HOST side with an independent tool
# (hdf-read) to confirm the guest's write actually reached the served
# image, exactly as scripts/pktport-e2e.sh's own final step does.
#
# Usage: scripts/pktport-rom-e2e.sh [scratch-dir]
# scratch-dir defaults to a fresh mktemp -d. Nothing here touches the
# repo's own nondistribution/ images -- everything is a copy (this
# project's lore: fresh image copies every e2e run).
#
# Requires: nondistribution/A1200.47.115.rom and
# nondistribution/m68k-machine.hdf (see nondistribution/README.md);
# vasm on PATH or at /opt/amiga/bin (scripts/build-pktport-rom.sh);
# a release build of machine-hosted and its examples.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SCRATCH="${1:-$(mktemp -d)}"
mkdir -p "$SCRATCH"
echo "scratch dir: $SCRATCH"

ROM="$REPO_ROOT/nondistribution/A1200.47.115.rom"
BOOT_SRC="$REPO_ROOT/nondistribution/m68k-machine.hdf"
for f in "$ROM" "$BOOT_SRC"; do
    if [ ! -e "$f" ]; then
        echo "FAIL: missing required fixture $f (see nondistribution/README.md)" >&2
        exit 2
    fi
done

BOOT_HDF="$SCRATCH/boot.hdf"
SERVED_HDF="$SCRATCH/served.hdf"
INPUT_SCRIPT="$SCRATCH/pktport-rom.input"
SCREENSHOT="$SCRATCH/pktport-rom-e2e.png"

run_example() {
    local name="$1"
    shift
    cargo run --release -p machine-hosted --example "$name" -- "$@"
}

echo "== 1. building the ROM (embeds the handler blob -- see docs/pktport-protocol.md sec 1) =="
"$SCRIPT_DIR/build-pktport-rom.sh"
# crates/machine-core embeds assets/pktport-rom/pktport-diagrom.bin via
# include_bytes! at COMPILE time, so the machine-hosted binary below must
# be rebuilt after that file changes -- cargo's own dependency tracking
# on include_bytes! handles this, but the build is not skipped here.
cargo build --release -p machine-hosted --examples >/dev/null

echo "== 2. fresh scratch copies -- NOTHING installed onto the boot image =="
cp "$BOOT_SRC" "$BOOT_HDF"
# Deliberately no hdf-install of L:pktport-handler or
# DEVS:Mountlist.pktport here -- this script's entire point is that
# neither is needed any more (docs/pktport-protocol.md sec 1).

echo "== 3. building the served volume (bare, writable DOS\\1, 4 MB) =="
run_example make-ffs-image "$SERVED_HDF" 4

echo "== 4. driving the guest =="
# Same frame-budget reasoning as scripts/pktport-e2e.sh (its own header
# explains the SLEEP/TYPE-chunking choices in full; not repeated here),
# minus the `Mount PKT0: from DEVS:Mountlist.pktport` command and its own
# settle SLEEP entirely -- PKT0: must already exist by the time the Shell
# prompt is reachable, straight to `Dir PKT0:` and the Echo marker write.
cat > "$INPUT_SCRIPT" <<'EOF'
SLEEP 4200
MOVE 42 73
SLEEP 10
BUTTONDOWN LEFT
BUTTONUP LEFT
SLEEP 5
BUTTONDOWN LEFT
BUTTONUP LEFT
SLEEP 300
MOVE 170 84
SLEEP 10
BUTTONDOWN LEFT
BUTTONUP LEFT
SLEEP 5
BUTTONDOWN LEFT
BUTTONUP LEFT
SLEEP 300
MOVE 192 108
SLEEP 10
BUTTONDOWN LEFT
BUTTONUP LEFT
SLEEP 5
BUTTONDOWN LEFT
BUTTONUP LEFT
SLEEP 600
TYPE "Dir PKT0"
SLEEP 40
TYPE ":\n"
SLEEP 3000
TYPE "Echo he"
SLEEP 40
TYPE "llo >PK"
SLEEP 40
TYPE "T0:mark"
SLEEP 40
TYPE "er\n"
SLEEP 1500
EOF

# Frame count: roughly 4200 (settle) + 3*~650 (three double-clicks) + 600
# + (2 chunks*40 + 3000) + (4 chunks*40 + 1500) -- comfortably under
# 12000, matching scripts/pktport-e2e.sh's own generously-measured
# headroom (that script's own comments record the empirical Mount+Dir
# cost this script no longer pays for Mount, but keeps the same Dir/Echo
# settle times since those round trips are unchanged).
MAX_FRAMES=16000
SCREENSHOT_FRAME=15800
MAX_INSTRUCTIONS=3000000000

set +e
OUTPUT=$(cargo run --release -p machine-hosted -- \
    --rom "$ROM" \
    --hostblk "$BOOT_HDF" --hostblk-writable \
    --pktvol "$SERVED_HDF" --pktvol-writable \
    --fast-ram-mb 64 \
    --max-frames "$MAX_FRAMES" \
    --max-instructions "$MAX_INSTRUCTIONS" \
    --input-script "$INPUT_SCRIPT" \
    --screenshot "$SCREENSHOT" \
    --screenshot-frame "$SCREENSHOT_FRAME" \
    --inspect 2>&1)
STATUS=$?
set -e
echo "$OUTPUT"
echo "machine-hosted exit status: $STATUS"
echo "screenshot: $SCREENSHOT"

echo "== 5. decisive proof: read PKT0:marker back from the HOST side =="
HDF_READ_ERR="$SCRATCH/hdf-read-marker.stderr"
if MARKER_CONTENT=$(run_example hdf-read "$SERVED_HDF" "marker" 2>"$HDF_READ_ERR"); then
    echo "PKT0:marker on the host reads: $(printf '%q' "$MARKER_CONTENT")"
    STRIPPED="${MARKER_CONTENT%$'\n'}"
    if [ "$STRIPPED" = "hello" ]; then
        echo "PASS: guest-written PKT0:marker observed by an independent host-side read, content matches \"hello\" (plus Echo's own trailing newline) -- PKT0: worked with NO L:pktport-handler and NO Mountlist entry installed"
        exit 0
    else
        echo "FAIL: PKT0:marker exists but its content is $(printf '%q' "$MARKER_CONTENT"), not \"hello\"" >&2
        exit 1
    fi
else
    echo "FAIL: PKT0:marker was not found on $SERVED_HDF -- $(cat "$HDF_READ_ERR")" >&2
    echo "diagnosis aids: $SCREENSHOT (does the Shell show Dir/Echo actually ran against PKT0:?)," >&2
    echo "                 the --inspect output above (pktport card state, if introspect.rs reports it)" >&2
    exit 1
fi
