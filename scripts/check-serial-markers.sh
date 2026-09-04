#!/usr/bin/env bash
# CI helper: check a completed log file for one or more required marker
# strings, dumping the log and failing loudly if any are missing.
#
# Sibling to scripts/ci-grep-serial.sh: that one polls a *growing* log
# written by a process that parks forever (the QEMU harnesses), so it
# needs a timeout and a pid to kill. This one is for a process that
# already ran to completion and produced a finished log -- machine-hosted
# under --max-frames/--max-instructions always terminates on its own (see
# crates/machine-hosted/src/run.rs's Outcome::LimitReached), so there is
# nothing to poll or kill; the caller runs it first and then hands the
# resulting log here.
#
# Usage:
#   scripts/check-serial-markers.sh <logfile> <marker>...
#
# Exits 0 if every marker is present (grep -F, fixed string). Exits 1 and
# dumps <logfile> to stderr if the file is missing or any marker is not
# found.
set -euo pipefail

if [ "$#" -lt 2 ]; then
    echo "usage: $0 <logfile> <marker>..." >&2
    exit 2
fi

logfile="$1"
shift

if [ ! -f "$logfile" ]; then
    echo "error: log file not found: $logfile" >&2
    exit 1
fi

missing=0
for marker in "$@"; do
    if ! grep -qF "$marker" "$logfile"; then
        echo "error: marker not found: $marker" >&2
        missing=1
    else
        echo "==> marker found: $marker" >&2
    fi
done

if [ "$missing" -ne 0 ]; then
    echo "==> captured output ($logfile):" >&2
    cat "$logfile" >&2 || true
    exit 1
fi
