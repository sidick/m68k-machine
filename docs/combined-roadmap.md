# Roadmap — m68k Machine (working title)

*Sequencing the unified machine (one Rust host, ARM + x86 board layers) to "basically usable". Companion to `m68k-machine-proposal.md` (v2, which supersedes the separate ARM/x86 proposals).*

**Definition of "basically usable"** (per platform): boots unattended from a maintained image to Workbench on RTG, with keyboard, mouse, MIRAGE storage and network, on (a) QEMU/KVM on commodity hardware and (b) at least one bare-metal target — Kickstart 3.2 first, AROS 68k gating from Phase 4.

**Standing note:** there is now one host, so the old flagship/sibling split is gone; "ARM vs x86" is a board-layer question, not a fork. Where phases touch only one platform, that is stated.

---

## Phase 0 — Foundations (small)

- Repo layout: host repo (machine core + board layers, one Cargo workspace), `m68k-stack` (m68k drivers, boot ROMs; MIRAGE-shared pieces live with MIRAGE). Licences per proposal §16, decided now.
- Verify m68k-rs `no_std` interpreter build (the load-bearing assumption); pin version. Skeleton `AddressBus` implementation + hello-world guest instruction under both QEMU harnesses (aarch64-virt, x86-q35).
- Week-one Rock 5B checks (proposal §15): SPL/EL2/KVM in dmesg, VFIO/IOMMU probe, serial console (1.5 Mbaud), fixed 12 V supply.
- CI skeleton: amiga-gcc toolchain container; both QEMU harnesses.

## Phase 1 — Blind boot (medium)

One machine core, exercised on both harnesses from the start (the q35 desktop loop is the faster daily driver; virt runs in CI):

- Chip RAM, open-bus `$FF` rule, spinning chipset registers, both CIAs (incl. keyboard path), frame clock/VERTB, A1200 ROM loading (single ROM + AROS pair), serial console trait per board layer.
- **AROS smoke test enters CI here**: the AROS 68k ROM pair is freely redistributable, so it is the ROM public CI boots on every commit from day one — "ROM pair loads, exec starts, serial output reached". Kickstart 3.2 remains the acceptance target driving development (stricter chipset test), run on private/local runners with a user-supplied ROM. This prevents the secondary OS silently rotting before its Phase 4 gate.
- Verify early risks: Gayle/PCMCIA probes vs open bus; m68k-rs interrupt-delivery and reset semantics against the machine's needs.
- **Exit:** Kickstart 3.2 reaches the strap and idles in the boot menu on both harnesses, observed via serial; AROS smoke test green in CI.

## Phase 2 — Visible Workbench (medium)

- Software blitter: unit tests, then differential vs Copperline (randomised ops + recorded Workbench traces).
- Stop-gap planar renderer to the display-surface trait (virtio-gpu/ramfb on virt; ramfb/GOP-style on q35).
- **Exit:** native-display Workbench renders on both harnesses; blitter passes the Copperline differential.

## Phase 3 — The m68k stack: usable under emulation (large — the heart of the project)

Written once, byte-identical on both platforms:

- Zorro III AUTOCONFIG shim + DiagArea injection (manufacturer ID allocated); `devicetree.resource` (FDT on virt; ACPI→FDT synthesis on q35).
- MIRAGE block card: host backend in the machine core (file/RAM/zero backing; cached mode deferred to Phase 6), m68k driver + RDB-mounting boot ROM. Doubles as MIRAGE spec-hardening.
- `pci.library` (Prometheus API) over each harness's ECAM; virtio-net SANA-II, virtio-input, virtio-gpu; P96 `.card` + SetSwitch.
- amitools-built boot HD (3.2 + P96 + screenmode prefs); unattended-boot CI gate on both harnesses.
- **Exit:** both platforms boot unattended to Workbench on RTG with input, storage, network under QEMU. *"Basically usable (emulated)" — everything after is hardware reach.*
  - **Met 2026-09-15, with the scope said plainly rather than reinterpreted.** The exit exists as one composed real-ROM gate — a single unattended boot of a single image proving Workbench on the rtgboard RTG screen, scripted pointer/click input, the virtio-net first-packet round trip, and a guest storage write read back out of the booted image (`kickstart_3_2_2_a1200_boots_unattended_to_rtg_workbench_with_input_storage_network` in `crates/machine-hosted/tests/real_rom.rs`; `scripts/phase3-gate.sh` is the one command; `docs/ci.md` records what runs where and why the gate is local-only — Kickstart is licensed media). Two words of this line deserve honesty rather than a quiet pass. **"Both platforms":** this phase's own preamble — "written once, byte-identical on both platforms" — is what the gate proves: the whole stack lives in the shared `machine-core`, which both board layers embed byte-identically and public CI builds, lints and boots (Phase 0/1 payloads) for both targets on every commit. What the gate does *not* prove is those board layers booting to Workbench themselves: they have no storage/RTG/input/PCI devices wired (real-ECAM `PciBackend` and the rest are recorded deferrals), and that per-board device bring-up is exactly the "hardware reach" this exit line already sends everything after it to — it moves explicitly to Phase 4 (KVM, VFIO, Prometheus conformance on real ECAM) and Phase 5's ADR-0001 fork, not silently out of Phase 3. **"Under QEMU":** the gate runs the machine core in `machine-hosted` as a host process; QEMU-hosted (and KVM) execution of the *full device set* arrives with Phase 4's "same image under KVM" work, on the same board layers. If either reading was load-bearing for a downstream consumer, Phase 4's exit is where it lands.

## Phase 4 — Real hardware via KVM + AROS gate (medium)

- **Rock 5B (ARM):** same image under KVM; MIRAGE backed by host NVMe; VFIO attempt (NIC first; expect `noiommu` caveats). **First interpreter performance measurements on target silicon** — this is the data for the Emu68-variant question (proposal §5.3): decide, with numbers, whether the escape hatch ever needs building.
- **Any x86 desktop:** same image under KVM; mature VFIO hosts the **PCIe-to-PCI bridge + period cards rig** (Voodoo3/RTL8139/SB128, a PCI USB (OHCI/UHCI) controller card vs shipped P96 `.card`s and era drivers) = Prometheus API conformance for `pci.library`. The USB card doubles as a real-hardware precursor to Phase 6's xHCI HID work (proposal's USB HID row) — worth noting whether a period PCI USB `.device` stack runs unmodified through this library before any xHCI driver of our own is written.
- **Intermediate deliverable — a try-it x86 VM image:** a distributable disk (qcow2 for QEMU/KVM) carrying a minimal Linux + the `machine-hosted` runner + the maintained boot image, booting straight into Workbench — someone with an x86 machine tries this project as a working Amiga without building anything. Licensing shapes it from the start: Kickstart and AmigaOS media cannot ship on it, so the disk ships AROS-bootable with a documented drop-in mechanism for the user's own Kickstart ROM and OS image (the same posture `nondistribution/README.md` takes for the test fixtures). A deliverable marker, not a design.
- **AROS 68k** ROM pair on both; resolve P96-vs-HIDD; AROS joins the CI gate.
- **Exit:** both platforms usable as KVM guests on real hardware; `pci.library` validated against physical devices; AROS boots.

## Phase 5 — Standalone machine (large)

**Fork in the road, decided at entry — see `adr-0001-bare-metal-vs-linux-host.md`.** This phase was originally specified as bare metal on the assumption that the CPU core would build `no_std`. Phase 0 found the published crate does not, and sized the gap at roughly a day or two of mechanical work rather than a wall. **That work has since been done** — the project now uses a `no_std` fork and has run a guest instruction on bare metal — so the CPU-core blocker is gone and what remains of the bare-metal option is the per-board driver work. So the phase opens with a choice between owning the board's drivers (bare metal, below) and borrowing them (a minimal Linux as the hardware layer, the Amithlon model, which deletes most of this phase and collapses Phase 4's KVM stage). The Phase 4 measurements decide it, alongside the Emu68-variant question. Everything in Phases 0–4 is identical either way.

If bare metal, in order of effort and reuse:

1. **x86 UEFI payload** — smallest step (GOP free, ACPI parsing, NVMe + xHCI from the Redox quarry); proves the whole stack on metal; the target most people can try. *"Basically usable" met for x86.*
2. **RK3588 (the Rock 5B)** — U-Boot-derived UART/MMC/GIC/PCIe-RC bring-up; virtio-compatible host backends first, native after. *"Basically usable" met for ARM.*
3. **Pi 5** — mostly glue by design (70–80 % shared with RK3588 per proposal §13); the community target.

## Phase 6 — Comfort and depth (ongoing, by appetite)

- Cached backing mode (read cache → bounded write-back); RAM drive instance; MIRAGE management-plane UX from the running Amiga.
- AHI, host-fs bridge, RTC doorbell cards (AHI low priority — audio is not a personal need).
- **Cranelift trace JIT**: hosted/KVM first, then bare-metal (alloc + W^X plumbing in board layers) — one upgrade, both ISAs.
- PCI GPU stage 2 (m68k modesetting on Radeon/Voodoo); USB HID via xHCI on ARM bare metal; further boards by demand (Pi 4 as an ordinary port, Allwinner as the abstraction proof).
- Docs + first public release: README answering "why not WinUAE" (instant boot, no host OS, real-hardware drivers, open stack, no games by design), supported-hardware list, image downloads.

---

## Why this order

- One host means Phases 1–3 are ~100 % shared minus two thin harness layers; both platforms advance in lockstep for the cost of one.
- Hardware risk is front-loaded where cheap (Phase 0 checks, Phase 4 KVM) and deferred where expensive (bare metal last, after every driver already works under KVM) — late enough that Phase 5 can still choose not to own the board drivers at all.
- The performance question that motivated the old two-host design is answered with measurements at Phase 4, not assumptions at Phase 0 — the Emu68 variant is built only if the numbers demand it.
- The Prometheus conformance rig lands on x86/KVM before any bare metal, so real-Amiga-usable PCI drivers don't wait on either port.
- Every phase exit is a CI-checkable state; Copperline (chipset/blitter) and Emu68-based systems (independent CPU) remain the two oracles.
