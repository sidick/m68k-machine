#!/usr/bin/env bash
# The Phase 3 unattended-boot gate, as one command: builds the composed
# gate image (scripts/patch-unattended-hdf.sh) and runs the single
# composed real-ROM test that is the roadmap's Phase 3 exit criterion --
# one boot of one image, no interaction, proving Workbench on the
# rtgboard RTG screen, scripted pointer/click input, the virtio-net
# first-packet round trip, and a guest storage write read back out of
# the booted image.
#
# WHY A SCRIPT AND NOT JUST A DOCUMENTED CARGO LINE: the gate test, like
# every real-ROM test, SKIPS cleanly when licensed fixtures are absent --
# correct behaviour for `cargo test` on a clean clone, but fatal for a
# gate, which must never go green by exercising nothing. This script
# checks the licensed inputs exist up front and fails loudly if they
# don't, so a green run always means the boot actually happened.
# Silent failure is this platform's norm; a gate that can silently skip
# is not a gate.
#
# Runs LOCALLY only (docs/ci.md): Kickstart 3.2.2 is licensed media and
# cannot run on public GitHub runners. Public CI compiles the gate test
# and asserts it stays listed (the `test` job); this script is what a
# release-gating machine with the media actually runs.
#
# Usage: phase3-gate.sh
#   Honours $M68K_TEST_HDF (base image), $M68K_KICKSTART_A1200 (ROM) and
#   $M68K_UNATTENDED_HDF (composed image) exactly as the patch script
#   and the test themselves do.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

ROM="${M68K_KICKSTART_A1200:-$REPO_ROOT/nondistribution/A1200.47.115.rom}"
BASE="${M68K_TEST_HDF:-$REPO_ROOT/nondistribution/m68k-machine.hdf}"
OUT="${M68K_UNATTENDED_HDF:-$REPO_ROOT/nondistribution/m68k-machine-unattended.hdf}"

# --- Preconditions: the licensed inputs the test would otherwise
# silently skip on. -------------------------------------------------------

if [ ! -f "$ROM" ]; then
    echo "error: Kickstart ROM not found: $ROM" >&2
    echo "       (licensed media -- see nondistribution/README.md)" >&2
    exit 1
fi
if [ ! -f "$BASE" ]; then
    echo "error: base image not found: $BASE" >&2
    echo "       (build one with amibake first: tools/amibake/m68k-machine.toml)" >&2
    exit 1
fi

# --- Build the composed gate image fresh from the base every run, so a
# stale image can never pass a gate the current guest binaries would
# fail. The patch script does its own positive verification. -------------

echo "==> building the composed gate image"
"$SCRIPT_DIR/patch-unattended-hdf.sh" "$BASE" "$OUT"

# --- Run the gate test. Its assertions are the exit criterion; its own
# doc comment in crates/machine-hosted/tests/real_rom.rs records every
# measured floor. ---------------------------------------------------------

echo "==> running the unattended-boot gate test"
cd "$REPO_ROOT"
cargo test -p machine-hosted --test real_rom -- \
    kickstart_3_2_2_a1200_boots_unattended_to_rtg_workbench_with_input_storage_network \
    --ignored --nocapture

echo "phase 3 gate: PASSED"
