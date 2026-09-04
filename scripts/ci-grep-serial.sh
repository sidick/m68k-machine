#!/usr/bin/env bash
# CI helper: poll a growing log file for a marker line, then kill the
# process that is writing it.
#
# Shared by the qemu-virt and qemu-q35 CI jobs (both run a QEMU harness in
# the background, redirected to a log file, and need to wait for a
# PHASE0 ... ALL CHECKS PASSED marker without hanging forever if the
# payload never gets there -- see scripts/run-qemu-virt.sh and
# scripts/run-qemu-q35.sh, neither of which exits QEMU on its own).
#
# Usage:
#   scripts/ci-grep-serial.sh <marker> <logfile> <timeout-secs> <pid>
#
# Polls <logfile> once a second (grep -F, fixed string) for <marker>, up
# to <timeout-secs>. On success, kills <pid> and exits 0. On timeout,
# kills <pid>, dumps <logfile> to stderr, and exits 1.
set -euo pipefail

if [ "$#" -ne 4 ]; then
    echo "usage: $0 <marker> <logfile> <timeout-secs> <pid>" >&2
    exit 2
fi

marker="$1"
logfile="$2"
timeout_secs="$3"
pid="$4"

found=0
elapsed=0
while [ "$elapsed" -lt "$timeout_secs" ]; do
    if [ -f "$logfile" ] && grep -qF "$marker" "$logfile"; then
        found=1
        break
    fi
    # If the process already exited (crashed, or QEMU itself failed to
    # start), no point waiting out the rest of the timeout.
    if ! kill -0 "$pid" 2>/dev/null; then
        break
    fi
    sleep 1
    elapsed=$((elapsed + 1))
done

# Best-effort: the payload parks forever once done, so the harness (this
# script) is always the one to end it, marker found or not.
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true

if [ "$found" -eq 1 ]; then
    echo "==> marker found: $marker" >&2
    exit 0
fi

echo "error: marker not seen within ${timeout_secs}s: $marker" >&2
echo "==> captured output ($logfile):" >&2
cat "$logfile" >&2 || true
exit 1
