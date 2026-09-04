#!/usr/bin/env bash
# Build and run the board-qemu-q35 UEFI application under QEMU's `q35`
# machine with OVMF/edk2 firmware.
#
# Usage: scripts/run-qemu-q35.sh [extra qemu-system-x86_64 args...]
#
# Prints the guest's serial output (COM1) to stdout via `-serial stdio`,
# which is what CI greps for the "PHASE0 BOARD-QEMU-Q35: ..." marker line.
# QEMU is left running (headless, no display) until killed -- the harness
# calling this script is expected to kill it once the marker has been
# seen, or after a timeout.
#
# Firmware: probes a short list of known OVMF/edk2 code-pflash paths
# (Homebrew macOS, common Linux distro paths). Override with the
# OVMF_CODE environment variable if none of those match.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-"$repo_root/target"}"
efi_bin="$target_dir/x86_64-unknown-uefi/debug/board-qemu-q35.efi"
esp_dir="$target_dir/board-qemu-q35-esp"

echo "==> building board-qemu-q35 for x86_64-unknown-uefi" >&2
cargo build -p board-qemu-q35 --target x86_64-unknown-uefi --manifest-path "$repo_root/Cargo.toml"

echo "==> assembling ESP at $esp_dir" >&2
rm -rf "$esp_dir"
mkdir -p "$esp_dir/efi/boot"
cp "$efi_bin" "$esp_dir/efi/boot/bootx64.efi"

# Locate OVMF/edk2 code-only (read-only) firmware. Known paths, in order:
# Homebrew macOS (qemu package), then common Linux distro locations. Ubuntu
# 24.04's `ovmf` package renamed the file to OVMF_CODE_4M.fd (4 MB image,
# still at the same /usr/share/OVMF/ directory) -- probed after the older
# OVMF_CODE.fd name so a system with both prefers the one this script has
# used historically.
known_ovmf_paths=(
    "/opt/homebrew/share/qemu/edk2-x86_64-code.fd"
    "/usr/share/qemu/edk2-x86_64-code.fd"
    "/usr/share/OVMF/OVMF_CODE.fd"
    "/usr/share/OVMF/OVMF_CODE_4M.fd"
    "/usr/share/edk2/ovmf/OVMF_CODE.fd"
    "/usr/share/ovmf/OVMF.fd"
    "/usr/share/qemu/OVMF.fd"
)

ovmf_code="${OVMF_CODE:-}"
if [[ -z "$ovmf_code" ]]; then
    for candidate in "${known_ovmf_paths[@]}"; do
        if [[ -f "$candidate" ]]; then
            ovmf_code="$candidate"
            break
        fi
    done
fi

if [[ -z "$ovmf_code" ]]; then
    echo "error: no OVMF/edk2 code pflash found; set OVMF_CODE=/path/to/firmware.fd" >&2
    exit 1
fi

echo "==> using firmware: $ovmf_code" >&2
echo "==> launching qemu-system-x86_64 -M q35 (headless, -serial stdio)" >&2

exec qemu-system-x86_64 \
    -M q35 \
    -m 512M \
    -display none \
    -serial stdio \
    -drive if=pflash,format=raw,readonly=on,file="$ovmf_code" \
    -drive format=raw,file="fat:rw:$esp_dir" \
    -net none \
    "$@"
