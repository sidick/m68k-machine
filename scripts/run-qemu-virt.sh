#!/usr/bin/env bash
# Run the board-qemu-virt Phase 0 smoke payload under QEMU's `virt`
# machine (aarch64).
#
# Usage:
#   scripts/run-qemu-virt.sh [path-to-elf]
#
# With no argument, runs the debug build at
# target/aarch64-unknown-none/debug/board-qemu-virt (build it first with
# `cargo build -p board-qemu-virt --target aarch64-unknown-none`, or pass
# the release path explicitly, e.g.
# target/aarch64-unknown-none/release/board-qemu-virt).
#
# Serial output goes to stdout (`-serial stdio`); the payload prints
# PASS/FAIL lines per check and a final marker line:
#   "PHASE0 BOARD-QEMU-VIRT: ALL CHECKS PASSED"
# CI greps stdout for that exact marker. `-display none` keeps this
# headless; `-no-reboot` stops QEMU from restarting the payload if it ever
# reset-loops instead of parking. This script does not exit QEMU on its
# own -- the payload parks forever (`wfe` loop) once done, so a CI/local
# caller is expected to run it with a timeout and kill it, e.g.:
#
#   ./scripts/run-qemu-virt.sh > out.txt &
#   qemu_pid=$!
#   sleep 5
#   kill "$qemu_pid" 2>/dev/null
#   grep -q "PHASE0 BOARD-QEMU-VIRT: ALL CHECKS PASSED" out.txt
#
# `-device ramfb`: gives the payload a real linear framebuffer to drive
# (see `src/ramfb.rs`'s module doc comment and `docs/display-boards.md`).
# `-display none` still keeps the *host* window headless -- ramfb's
# surface is captured with QEMU's own monitor `screendump` command, not a
# window, exactly as `docs/display-boards.md` does. Without this flag the
# payload still runs and boots correctly; `ramfb.rs`'s `find_file_selector`
# simply reports `DeviceNotPresent` and the payload narrates that over
# serial and carries on (no display step, everything else unaffected) --
# see `main.rs`'s `present_to_ramfb` doc comment.

set -euo pipefail

elf="${1:-target/aarch64-unknown-none/debug/board-qemu-virt}"

if [ ! -f "$elf" ]; then
    echo "error: $elf not found (build it first, see script header)" >&2
    exit 1
fi

exec qemu-system-aarch64 \
    -M virt \
    -cpu cortex-a76 \
    -m 512M \
    -display none \
    -device ramfb \
    -no-reboot \
    -serial stdio \
    -kernel "$elf"
