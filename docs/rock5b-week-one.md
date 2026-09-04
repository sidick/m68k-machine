# Rock 5B week-one checklist

Phase 0 hardware checks for the user's Rock 5B (RK3588), per proposal §15
("Risks and open questions") and roadmap Phase 0. These are cheap,
front-loaded checks: confirm the ARM KVM/VFIO story on this specific board
before any board-layer code depends on it. Run on the board's own Linux
(vendor or mainline), not cross-compiled from the host.

None of this blocks Phase 0-3 work (those run under QEMU on any machine);
it unblocks Phase 4 (KVM on real hardware) planning early instead of
finding out at Phase 4 that the SPL vintage doesn't support EL2.

---

## 1. EL2 / KVM support (SPL vintage)

RK3588 boards need TF-A/SPL built with EL2 support to host KVM. Older
vendor SPL/U-Boot builds on some RK3588 boards shipped without it —
verify rather than assume, per proposal §15.

- [ ] Check the kernel booted at EL2, not EL1:
  ```sh
  dmesg | grep -i "el2\|hyp\|kvm"
  ```
  Expected (EL2-capable, good): a line such as
  `CPU: All CPU(s) started at EL2` (or `Kernel entry point is above 4GB`
  boot log context suggesting non-secure hyp mode entry). A line reading
  `CPU: All CPU(s) started at EL1` means the SPL/firmware did not hand off
  at EL2 — KVM will not work until the board's firmware is updated.

- [ ] Check `/dev/kvm` exists and is usable:
  ```sh
  ls -l /dev/kvm
  ```
  Expected: character device present, e.g.
  `crw-rw---- 1 root kvm 10, 232 ... /dev/kvm`. If absent, check
  `dmesg | grep -i kvm` for a specific rejection reason (usually "cannot
  boot in non-secure mode" or a missing `CONFIG_KVM` in the running
  kernel).

- [ ] Confirm the kernel has KVM built in or as a loaded module:
  ```sh
  zgrep -i CONFIG_KVM /proc/config.gz 2>/dev/null || cat /boot/config-$(uname -r) | grep -i CONFIG_KVM
  lsmod | grep kvm
  ```
  Expected: `CONFIG_KVM=y` (or `=m` with the module loaded).

- [ ] If EL2 is not available: note the exact SPL/U-Boot/TF-A version
  (`cat /proc/device-tree/firmware/... 2>/dev/null` or check the vendor
  image release notes) so it's clear whether a firmware update is the fix
  or the board needs replacing for the ARM KVM stage. Record the finding
  here even if negative — this gates Phase 4's Rock 5B KVM work.

**Result:** ______________________________________________

---

## 2. VFIO / IOMMU

Proposal §15 flags that RK3588 may be `noiommu`-only (pinned,
identity-mapped guest memory, no per-device IOMMU protection). Expect
this; it's a known limitation of the RK3588 SoC's PCIe IOMMU support in
mainline Linux as of this writing, not a board defect. x86 VFIO maturity
is the documented hedge (proposal §15) — this check just confirms which
mode is available on this board today.

- [ ] Check for IOMMU-related boot messages:
  ```sh
  dmesg | grep -i iommu
  ```
  Expected on RK3588: little or no IOMMU activity (`rockchip-iommu` may
  appear for a display/media IOMMU, not necessarily the PCIe path), or an
  explicit absence — this is the "likely noiommu-only" case proposal §15
  anticipates. Note whatever actually appears.

- [ ] Check whether the `vfio-pci` driver is available and whether
  `noiommu` mode is required:
  ```sh
  ls /sys/module/vfio/parameters/ 2>/dev/null
  cat /sys/module/vfio/parameters/enable_unsafe_noiommu_mode 2>/dev/null
  modinfo vfio 2>/dev/null | head
  ```
  If `enable_unsafe_noiommu_mode` exists, that's the confirmation this
  board needs the unsafe/pinned path (expected result per proposal §15).

- [ ] Check what PCIe endpoints exist to test VFIO against once needed
  (informational only, for Phase 4 planning — a NIC is the first VFIO
  target per the roadmap):
  ```sh
  lspci -vv
  ```

**Result:** ______________________________________________

---

## 3. Serial console at 1.5 Mbaud

The Rock 5B's debug UART is the primary early-boot/bring-up console for
board-layer work (before any display path exists). Confirm the wiring and
baud rate now, not during Phase 5 bring-up debugging.

- [ ] Identify the debug UART pins from the board's documentation/pinout
  (Rock 5B: 3-pin debug UART header near the GPIO header — TX, RX, GND;
  consult the Radxa wiki for the exact pin numbers, as header layout
  varies slightly by revision).
- [ ] Connect a USB-to-TTL serial adapter (3.3 V logic level — **do not**
  use a 5 V adapter) to TX/RX/GND. Cross TX/RX (board TX → adapter RX and
  vice versa).
- [ ] From the host machine, open the serial console at **1.5 Mbaud**
  (1,500,000 baud, 8N1) — the Rock 5B's default debug UART rate:
  ```sh
  # macOS/Linux, adjust device path as needed
  screen /dev/tty.usbserial-XXXX 1500000
  # or
  picocom -b 1500000 /dev/ttyUSB0
  ```
- [ ] Power on (or reset) the board and confirm boot log text appears
  (U-Boot banner, then kernel log). Expected: readable text starting near
  power-on; garbled text usually means a baud-rate mismatch (double-check
  1.5 Mbaud, not the more common 115200) or a TX/RX swap.

**Result:** ______________________________________________

---

## 4. Fixed 12 V power supply

Proposal §15: "USB-C PD pickiness — use a fixed 12 V supply, mandatory
once NVMe + bridge riser raise the power budget." Confirm the board's
power setup now, before it's under load from an M.2 NVMe drive and a
PCIe bridge/riser (needed for the later hardware-rig work).

- [ ] Confirm the board is powered via the **dedicated 12 V barrel/pin
  header input**, not USB-C PD, if the board supports both. USB-C PD
  negotiation on Rock 5B is known to be inconsistent under sustained load
  with some chargers/cables — a fixed 12 V supply avoids the failure mode
  entirely.
- [ ] Confirm the supply is rated for the board's peak draw with
  NVMe + PCIe bridge/riser attached. Radxa's guidance for the Rock 5B
  under heavy load (NVMe, PCIe devices, USB peripherals) is a 12 V/5 A (60
  W) supply at minimum; check the specific supply's rated current, not
  just its voltage.
- [ ] If currently on USB-C: note this as a to-do to switch before any
  sustained NVMe/PCIe bring-up work (Phase 4's hardware rig), since
  under-voltage brownouts during bring-up are hard to distinguish from
  software bugs.

**Result:** ______________________________________________

---

## Summary

| Check | Status | Notes |
|---|---|---|
| EL2 support (SPL vintage) | ☐ pass ☐ fail ☐ unknown | |
| KVM (`/dev/kvm`) | ☐ pass ☐ fail ☐ unknown | |
| VFIO / IOMMU mode | ☐ full IOMMU ☐ noiommu-only ☐ unknown | |
| Serial console @ 1.5 Mbaud | ☐ working ☐ not working | |
| Fixed 12 V supply in use | ☐ yes ☐ no (on USB-C) | |

Record findings inline above (or replace this table with actual results)
once run on the board. This document gates Phase 4 planning, not Phase 0
exit — Phase 0-3 proceed under QEMU regardless of these results.
