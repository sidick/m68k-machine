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
    -no-reboot \
    -serial stdio \
    -kernel "$elf"
