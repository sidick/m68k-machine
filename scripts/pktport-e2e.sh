#!/usr/bin/env bash
# End-to-end proof of the pktport DosPacket transport (docs/pktport-
# protocol.md, ADR 0004): builds the guest handler, installs it and a
# Mountlist fragment onto a scratch copy of the project's boot image,
# creates a small writable served volume, boots a real Kickstart 3.2.2
# to a Workbench Shell, scripts `Mount PKT0:`, `Dir PKT0:` and
# `Echo hello >PKT0:marker` from inside the guest, and then -- the
# decisive step, not vibes -- reads `PKT0:marker` back from the HOST
# side with an independent tool (hdf-read) to confirm the guest's write
# actually reached the served image.
#
# Usage: scripts/pktport-e2e.sh [scratch-dir]
# scratch-dir defaults to a fresh mktemp -d. Nothing here touches the
# repo's own nondistribution/ images -- everything is a copy.
#
# Requires: nondistribution/A1200.47.115.rom and
# nondistribution/m68k-machine.hdf (see nondistribution/README.md);
# vasm on PATH or at /opt/amiga/bin (scripts/build-pktport-handler.sh);
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
MOUNTLIST="$SCRATCH/Mountlist.pktport"
INPUT_SCRIPT="$SCRATCH/pktport.input"
SCREENSHOT="$SCRATCH/pktport-e2e.png"

run_example() {
    local name="$1"
    shift
    cargo run --release -p machine-hosted --example "$name" -- "$@"
}

echo "== 1. building the handler and helper tools =="
"$SCRIPT_DIR/build-pktport-handler.sh"
# Built once, quietly, up front: cargo prints its own build-status lines
# to stderr on every invocation whether or not a rebuild is needed, and
# hdf-read's own output (§6 below) must be captured as pure file bytes,
# with nothing from cargo mixed in.
cargo build --release -p machine-hosted --examples >/dev/null

echo "== 2. fresh scratch copies =="
cp "$BOOT_SRC" "$BOOT_HDF"

cat > "$MOUNTLIST" <<'EOF'
PKT0:    Handler   = L:pktport-handler
         GlobVec   = -1
         StackSize = 8192
         Startup   = 0
EOF

echo "== 3. installing the handler and Mountlist fragment =="
run_example hdf-install "$BOOT_HDF" "$REPO_ROOT/m68k/pktport-handler/pktport-handler" "L:pktport-handler"
run_example hdf-install "$BOOT_HDF" "$MOUNTLIST" "DEVS:Mountlist.pktport"

echo "== 4. building the served volume (bare, writable DOS\\1, 4 MB) =="
run_example make-ffs-image "$SERVED_HDF" 4

echo "== 5. driving the guest =="
# Frame budget: SLEEP 4200 reaches a settled Workbench desktop (measured
# in crates/machine-hosted/tests/real_rom.rs's own click tests, against
# this exact boot image); the three double-clicks (SYS: -> System ->
# Shell, same screen coordinates those tests derived and verified) land
# a live Shell prompt by roughly frame 5400. TYPE strings are split into
# <=8-character chunks with SLEEPs between them
# (docs/input-protocol.md sec 13's own retrospective: a --input-script
# TYPE longer than QUEUE_CAPACITY/2 (8) characters delivers every
# CHARDOWN/CHARUP event in one tick and silently overflows the driver's
# 16-entry queue) -- "Mount PKT0: from DEVS:Mountlist.pktport" alone is
# 40 characters and would lose its tail end as one TYPE. TYPE "...\n"
# emits a real CR itself (recent, correct behaviour per this project's
# own notes), so no separate KEYDOWN/KEYUP Return is needed after each
# command line.
#
# Settle SLEEPs are generous and were widened from an initial guess after
# measuring the real cost of a pktport round trip against this served
# image during development: Mount alone (RDB-less, cold) settles well
# inside a few hundred frames, but a full `List`/`Dir` of even two
# entries drives several real LOCATE_OBJECT/EXAMINE_OBJECT/EXAMINE_NEXT/
# PARENT/COPY_DIR/FREE_LOCK round trips end to end and was observed
# taking on the order of 2000+ frames to fully settle -- a short SLEEP
# there let the next command's keystrokes arrive while Dir was still
# running, landing in the console's type-ahead buffer rather than a
# fresh prompt (harmless to correctness, since AmigaDOS still executes
# each line in order once typed, but it very nearly cost this script a
# false negative: with too little settle time before Echo's own TYPE
# chunks, the mouse/keyboard event queue overflowed (16-entry capacity)
# and part of the Echo command's own text was lost).
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
TYPE "Mount PK"
SLEEP 40
TYPE "T0: from"
SLEEP 40
TYPE " DEVS:Mo"
SLEEP 40
TYPE "untlist."
SLEEP 40
TYPE "pktport\n"
SLEEP 800
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

# Frame count: roughly 4200 (settle) + 3*~650 (three double-clicks) +
# 600 + (5 chunks*40 + 800) + (2 chunks*40 + 3000) + (4 chunks*40 +
# 1500) -- comfortably under 12000; --max-frames/--screenshot-frame
# below carry generous margin past that, the same "clear of the capture
# boundary" posture crates/machine-hosted/tests/real_rom.rs's own click
# tests use. Measured empirically (not the original guess): Mount+List
# alone needed on the order of 7700 frames to fully settle against this
# served image (directory listing verified correct by frame 8400 during
# development), so the full Mount+Dir+Echo sequence gets headroom well
# past that.
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

echo "== 6. decisive proof: read PKT0:marker back from the HOST side =="
HDF_READ_ERR="$SCRATCH/hdf-read-marker.stderr"
if MARKER_CONTENT=$(run_example hdf-read "$SERVED_HDF" "marker" 2>"$HDF_READ_ERR"); then
    echo "PKT0:marker on the host reads: $(printf '%q' "$MARKER_CONTENT")"
    # AmigaDOS Echo appends a trailing line by default (NOLINE suppresses
    # it, and this script does not pass NOLINE) -- amigados-command-
    # reference's own ECHO entry: "If the NOLINE option is given, the
    # cursor is not automatically advanced to the next line" implies it
    # *is* advanced (a written newline) otherwise. So the file's real
    # content is "hello\n", not bare "hello" -- stripped here before the
    # comparison, rather than asking the script to pass NOLINE and
    # deviate from the task's own literal command.
    STRIPPED="${MARKER_CONTENT%$'\n'}"
    if [ "$STRIPPED" = "hello" ]; then
        echo "PASS: guest-written PKT0:marker observed by an independent host-side read, content matches \"hello\" (plus Echo's own trailing newline)"
        exit 0
    else
        echo "FAIL: PKT0:marker exists but its content is $(printf '%q' "$MARKER_CONTENT"), not \"hello\"" >&2
        exit 1
    fi
else
    echo "FAIL: PKT0:marker was not found on $SERVED_HDF -- $(cat "$HDF_READ_ERR")" >&2
    echo "diagnosis aids: $SCREENSHOT (does the Shell show the Mount/Dir/Echo commands ran?)," >&2
    echo "                 the --inspect output above (pktport card state, if introspect.rs reports it)" >&2
    exit 1
fi
