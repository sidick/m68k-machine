#!/usr/bin/env bash
# NOT part of CI. This is the provenance-refresh tool for the AROS 68k
# ROM pair vendored at assets/aros/{aros-amiga-m68k-rom.bin,
# aros-amiga-m68k-ext.bin} -- the Phase 1 aros-smoke CI job
# (.github/workflows/ci.yml) boots those vendored files directly and does
# not run this script or touch the network. See docs/ci.md for why the
# pair is vendored rather than fetched at CI time, and
# assets/aros/PROVENANCE.md for the compliance/provenance note that goes
# with the currently-vendored files.
#
# What this script is for: when someone deliberately wants to update the
# vendored pair (e.g. to pick up upstream AROS fixes), hand-editing
# binaries in place is unreviewable and unauditable. This script instead
# downloads one dated AROS nightly, pins its SHA256 end to end (the
# downloaded zip, and each extracted ROM half), and fails loudly --
# distinguishing "network/mirror hiccup" from "upstream artifact actually
# changed" -- so a refresh is a small, checkable diff: bump the date and
# three hashes below, run this script, copy its output over
# assets/aros/*.bin, and update assets/aros/PROVENANCE.md's date,
# revisions and checksums to match, all in one commit.
#
# Usage:
#   scripts/fetch-aros-rom.sh <output-dir>
#
# On success, <output-dir>/aros-amiga-m68k-rom.bin and
# <output-dir>/aros-amiga-m68k-ext.bin exist, verified against the
# checksums pinned below, ready to be copied into assets/aros/. Idempotent:
# if both output files already exist and match their pinned SHA256, the
# download and extraction are skipped.
#
# Requires: curl, unzip, 7z (p7zip-full), sha256sum.
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <output-dir>" >&2
    exit 2
fi

out_dir="$1"
mkdir -p "$out_dir"

# Pinned nightly -- currently the same nightly assets/aros/*.bin were
# vendored from (see assets/aros/PROVENANCE.md). Bumping this (new date +
# all three hashes below, re-derived by actually running this script
# against the new URL, then copying its output over assets/aros/*.bin and
# updating PROVENANCE.md to match) is the deliberate, reviewable way the
# vendored ROM pair changes -- never silently track "latest". See
# docs/ci.md's "Refreshing the vendored ROM pair" for the full procedure.
aros_date="20260904"
zip_url="https://sourceforge.net/projects/aros/files/nightly2/${aros_date}/Binaries/AROS-${aros_date}-amiga-m68k-boot-iso.zip/download"
zip_sha256="6e6fefd70d9a8943dc4fc221d9335d6562a2dab6ffd833b29cd3985b7719fe0b"
rom_sha256="cf469f18daa1e82645de1d6cf79ac1b6681983a210fbffb120a955f3f00e7fb9"
ext_sha256="681bc4309fd958a1f12046c9d4477ed80434afc8f319ebe64ee24aa0d5bb943c"

rom_out="$out_dir/aros-amiga-m68k-rom.bin"
ext_out="$out_dir/aros-amiga-m68k-ext.bin"

verify() {
    local path="$1" expected="$2" label="$3"
    local actual
    actual="$(sha256sum "$path" | awk '{print $1}')"
    if [ "$actual" != "$expected" ]; then
        echo "error: $label checksum mismatch" >&2
        echo "  file:     $path" >&2
        echo "  expected: $expected" >&2
        echo "  actual:   $actual" >&2
        echo "This means the upstream AROS nightly artifact at $zip_url" >&2
        echo "has changed (or been corrupted/tampered with) since this" >&2
        echo "script's checksums were pinned -- it is NOT a transient" >&2
        echo "network error. Re-derive the pinned date and all three" >&2
        echo "SHA256 values in scripts/fetch-aros-rom.sh by hand, confirm" >&2
        echo "the new ROM pair still boots (docs/ci.md), and update them" >&2
        echo "deliberately -- do not just paste in whatever hash is" >&2
        echo "reported here without checking why it changed." >&2
        exit 1
    fi
}

if [ -f "$rom_out" ] && [ -f "$ext_out" ] \
    && [ "$(sha256sum "$rom_out" | awk '{print $1}')" = "$rom_sha256" ] \
    && [ "$(sha256sum "$ext_out" | awk '{print $1}')" = "$ext_sha256" ]; then
    echo "==> AROS ROM pair already present and verified in $out_dir, skipping fetch" >&2
    exit 0
fi

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

zip_path="$work_dir/boot-iso.zip"
echo "==> downloading $zip_url" >&2
curl -sL --fail -o "$zip_path" "$zip_url"
verify "$zip_path" "$zip_sha256" "downloaded nightly zip"

echo "==> extracting ISO from zip" >&2
unzip -q -o "$zip_path" -d "$work_dir/zip-out"
iso_path="$(find "$work_dir/zip-out" -name '*.iso' -print -quit)"
if [ -z "$iso_path" ]; then
    echo "error: no .iso found inside $zip_path" >&2
    exit 1
fi

echo "==> extracting ROM pair from $iso_path" >&2
7z e "$iso_path" -o"$work_dir/rom-out" boot/amiga/aros-rom.bin boot/amiga/aros-ext.bin -r -y >/dev/null

verify "$work_dir/rom-out/aros-rom.bin" "$rom_sha256" "extracted main ROM (aros-rom.bin)"
verify "$work_dir/rom-out/aros-ext.bin" "$ext_sha256" "extracted ext ROM (aros-ext.bin)"

mv "$work_dir/rom-out/aros-rom.bin" "$rom_out"
mv "$work_dir/rom-out/aros-ext.bin" "$ext_out"

echo "==> AROS ROM pair verified and written to $out_dir" >&2
