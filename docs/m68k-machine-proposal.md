# m68k Machine (working title) — Unified Proposal

*A chipset-less Amiga on modern hardware: one Rust host built on the m68k-rs CPU core, with ARM and x86-64 board layers, minimal chipset emulation, virtual Zorro III hardware, unmodified Kickstart 3.2 and AROS 68k.*

Status: proposal v2, September 2026. Supersedes `emu68-machine-proposal.md` and `emu68-machine-x86-proposal.md`, which described the same machine split across an Emu68-based ARM host and an m68k-rs x86 host. This revision inverts that: **m68k-rs is the mainline CPU core on both platforms, in a single Rust host with per-platform board layers**; Emu68 is retained as interface heritage and an optional future high-performance ARM variant. Working title only; "Emu68 machine" is no longer accurate.

---

## 1. Summary

Build a "generic Amiga" machine in the tradition of the DraCo and Amithlon: no Agnus/Denise/Paula, an unmodified Kickstart (or AROS 68k) ROM, and all real I/O provided through standard AmigaOS interfaces (AUTOCONFIG, P96, SANA-II, AHI) by virtual Zorro III cards and, long term, by m68k drivers talking directly to PCIe hardware.

The defining choices:

- **One host, one language.** A single Rust program: `no_std` machine core, per-platform board layers for AArch64 and x86-64. No FFI seam, no C fork to keep in sync, one build system, one set of guest-visible semantics on both ISAs.
- **m68k-rs as the CPU** (the `m68k` crate, MIT): 68000–68060 with FPU and MMU emulation, safe-Rust interpreter, optional Cranelift trace JIT — and Cranelift targets both x86-64 and AArch64, so one JIT upgrade path serves both platforms. The crate is proven at Kickstart scale (Copperline uses it) and is planned for the vamos successor: one core, several consumers.
- **Interpreter-first performance stance.** On modern cores the interpreter lands in 68060-class territory — a fast Amiga, ample for an OS-level machine whose workloads are Workbench, compilers and tooling, not demos. The Cranelift path is an upgrade, not a prerequisite. If ARM ever demands more, Emu68 remains the escape hatch (§5.3).
- **QEMU first, KVM second, bare metal third.** The same guest-independent design runs under QEMU (aarch64 `virt` and x86 `q35`) for development, under KVM on real boards/desktops with VFIO passthrough for hardware bring-up, and finally bare metal. Development, CI and hardware validation share one image per platform.
- **Minimal chipset, honest about scope.** Only the chipset surface the OS *spins on* is emulated (interrupt registers, beam counters, CIAs, a synchronous software blitter). Everything else returns open-bus `$FF`, as an unpopulated real machine would. Games and demos are out of scope by definition.
- **Standard seams, no ROM patching.** Unlike DraCo (patched ROM) and Amithlon (patched image), every host facility is reached through an interface Kickstart and AROS already understand, so both run unmodified.
- **Emu68-compatible m68k conventions.** The m68k-facing mechanisms Emu68 established — `devicetree.resource`, DiagArea board injection, gic400-style interrupt plumbing, 68040.library handling — are adopted as the machine's conventions, so the m68k stack stays portable across this machine, real Emu68/PiStorm systems, and any future Emu68-based variant.
- **Paravirtual first, real hardware later.** Storage uses the MIRAGE card interface (this machine is its second implementation); other virtual cards start as virtio-pci devices behind a Zorro III shim and a Prometheus/OpenPCI-compatible `pci.library`. The same `pci.library` later drives physical PCIe devices from m68k code.

## 2. Goals and non-goals

### Goals

1. Boot unmodified **Kickstart 3.2** (A1200 ROM — the Zorro III-capable expansion.library is required, §11.1) to Workbench on an RTG screen with keyboard, mouse, storage and network, from a pre-built boot HD, with no user interaction.
2. Boot **AROS 68k** (current ROM builds) on the same machine description with the same drivers.
3. Run on **QEMU** (aarch64-virt and x86-q35), **KVM** (ARM boards and x86 desktops), and **bare metal** on x86 UEFI, RK3588-class boards and the Pi 5, from one codebase, without changing the m68k-side drivers.
4. Provide a **`pci.library`** (Prometheus/OpenPCI-compatible) that lets m68k drivers own real PCIe devices — usable equally on real Amigas with PCI bridges.
5. Keep every m68k-side component buildable with the existing amiga-gcc Docker toolchain and testable under Copperline/vamos before it touches the machine.

### Non-goals

- Cycle-accurate or game-capable chipset emulation; anything beam-timed, DMA-contended, copper-tricked or Paula-driven is out of scope.
- Matching Emu68's JIT performance on ARM in the mainline (see §5.3 for the variant path).
- AGA (ECS Agnus reported; AGA is a contained later extension if wanted).
- PowerPC, WarpOS, OS4.
- Replacing P96/AHI/SANA-II with bespoke APIs.

## 3. Background and precedents

**DraCo (MacroSystem, 1995).** 68060, no custom chips: one real CIA, a MacroSystem-patched 3.1 ROM, display exclusively via RTG (Altais). Proved AmigaOS productivity software is chipset-agnostic once graphics go through RTG. Weaknesses: ROM patches, invisible early boot/gurus.

**Amithlon (2001).** x86 PC, minimal Linux as hardware layer, UAE-derived JIT, stock Kickstart image with injected resident modules. Emulated the "must exist" chipset surface so exec/timer/keyboard/gameport ran unmodified; RTG via VESA P96, audio via AHI. Proved the emulated-minimal-chipset route. Weaknesses: proprietary, unmaintained.

**PiStorm / Emu68 (2020–).** Bare-metal m68k JIT on Raspberry Pi as a CPU replacement for real Amigas; its drivers are m68k code on native hardware, with Emu68 supplying the memory map (`devicetree.resource`), interrupts (`gic400.library`) and AUTOCONFIG boards with DiagArea ROMs. Proved the m68k-drivers-on-native-hardware model and the injection mechanism this machine adopts as convention. Its JIT remains the ARM performance benchmark and the basis of the optional variant (§5.3).

**m68k-rs / Copperline (2025–).** The `m68k` crate: full-family interpreter (FPU, MMU), Cranelift trace JIT, HLE trap interception; MIT; validated at Kickstart scale as Copperline's CPU core. This machine is its bus-accurate consumer; the planned vamos successor is its HLE consumer.

This proposal is the Amithlon machine model, built on the PiStorm driver model, running on the Copperline CPU core, with the ROM untouched.

## 4. Architecture overview

```
┌───────────────────────────────────────────────────────────────────┐
│ m68k side (unmodified Kickstart 3.2 or AROS 68k ROM)              │
│  exec/dos/graphics/intuition …  P96 + .card   SANA-II   AHI       │
│  MIRAGE driver + boot ROM   pci.library (Prometheus API)          │
│  devicetree.resource  68040.library  Zorro III DiagArea ROMs      │
├───────────────────────────────────────────────────────────────────┤
│ CPU core: m68k-rs (interpreter; optional Cranelift trace JIT)     │
├───────────────────────────────────────────────────────────────────┤
│ Machine core (no_std Rust, platform-neutral)                      │
│  AddressBus impl: chip RAM · open bus · chipset regs · 2×CIA      │
│  software blitter · frame clock · stop-gap planar renderer        │
│  Zorro III AUTOCONFIG shim · MIRAGE backend · virtio backends     │
│  IRQ routing · DMA-address & cache services                       │
├───────────────────────────────────────────────────────────────────┤
│ Board layers (per platform)                                       │
│  aarch64: qemu-virt │ KVM-virt │ RK3588 │ Pi 5                    │
│  x86-64:  qemu-q35  │ KVM      │ UEFI payload                     │
└───────────────────────────────────────────────────────────────────┘
```

The machine core implements m68k-rs's `AddressBus` trait; that trait boundary is where the entire machine description lives. Board layers provide RAM, console, timers, interrupt delivery, display surface and device backends behind narrow traits. One core owns the CPU loop; spare cores host the renderer, MIRAGE backend and writeback threads.

## 5. CPU strategy

### 5.1 Mainline: m68k-rs interpreter

Guest CPU presented as **68040 + FPU** (68060 presentation a later option), with model-consistent FPU/MMU behaviour from the crate. Big-endian conversion is the crate's job; on x86, TSO ordering and MOVBE make this cheap and safe; on ARM the crate's contract handles ordering. Expected throughput on 2.4 GHz-class cores: 68060-class or better — a fast Amiga by any historical standard, sufficient for the machine's OS/tooling workloads.

### 5.2 Upgrade: Cranelift trace JIT

The crate's `jit` feature targets both x86-64 and AArch64 — one upgrade path for both platforms. Available first in hosted/KVM stages (Cranelift assumes std and page services); bare-metal use needs alloc + W^X plumbing in the board layers and is deliberately late-roadmap. Step-vs-batch equivalence testing (the crate's own mechanism) gates it.

### 5.3 Escape hatch: Emu68 variant

If a real ARM workload ever outgrows the interpreter+Cranelift ceiling, Emu68 (MPL-2.0, C, AArch64-only) can host the same machine as a separate board-target build consuming the machine description — the arrangement the superseded v1 proposal specified. This is explicitly *not maintained* in the mainline; what keeps the door open at near-zero cost is (a) the Emu68-compatible m68k conventions (§6.3) and (b) keeping the machine core free of Rust-only assumptions in its *specification* (the register-level machine description document), even though its implementation is Rust.

## 6. Machine description

### 6.1 Memory map (m68k view)

| Range | Contents | Notes |
|---|---|---|
| `$000000–$1FFFFF` | Chip RAM, 2 MB | Ordinary RAM; 2 MB matches ECS Agnus ID. |
| `$A00000–$BFFFFF` | CIA space | Emulated CIA-A (`$BFE001`), CIA-B (`$BFD000`), mirrors as real hardware. |
| `$C00000–$D7FFFF` | Slow RAM / unused | Open bus (`$FF`). |
| `$D80000–$DCFFFF` | Reserved/RTC | Open bus; battclock probe fails cleanly. |
| `$DE0000–$DEFFFF` | Gary/Ramsey | Open bus. |
| `$DFF000–$DFFFFF` | Custom chips | Emulated subset (§7), rest open bus. |
| `$E00000–$E7FFFF` | Extended ROM | AROS ext ROM when present; else open bus. |
| `$E80000–$E8FFFF` | Zorro II AUTOCONFIG | Z3 boards configure here too. |
| `$F80000–$FFFFFF` | Kickstart ROM | 512 KB Kickstart or AROS main ROM. |
| `$08000000–$0FFFFFFF` | Fast RAM (Z3) | Configurable; 128 MB default. |
| `$40000000–$7FFFFFFF` | Zorro III board space | Virtual cards, PCIe BAR windows. |
| `$FF000000–$FFFFFFFF` | Z3 AUTOCONFIG | Standard. |

**Open-bus rule.** Unanswered reads return `$FF`; writes are discarded; no 68k bus error except where a real Amiga would raise one (Zorro III timeouts). Kickstart's Ramsey/Gary/Buster/Gayle/RTC probes are written against Gary's DTACK-timeout behaviour — they check values, not exceptions — so this rule makes most stubbing unnecessary and the ROM choice forgiving.

### 6.2 CPU presentation

68040 + FPU via m68k-rs (§5.1). 68040.library handling follows the Emu68 convention (injected module wins; SetPatch kept minimal); AROS's own MMU/FPU setup must not conflict with injected modules.

### 6.3 Emu68-compatible conventions (m68k-facing)

Adopted as machine conventions so the m68k stack is portable across this machine, Emu68/PiStorm hardware, and any §5.3 variant:

- **`devicetree.resource`** publishes the hardware description (fed by an FDT on ARM boards, by an FDT synthesized from ACPI on x86 — the m68k side sees one format).
- **DiagArea board injection** (`emu68rom` pattern): every host-provided driver arrives as a Zorro III AUTOCONFIG board with a DiagArea ROM module; HD boot via BootNodes through stock expansion.library.
- **gic400.library-style interrupt plumbing**: host interrupts routed onto emulated INT2/INT6; drivers are ordinary AddIntServer servers.
- **68040.library** injection semantics as on Emu68.

## 7. Minimal chipset emulation

Only registers the OS spins on or reads for identification are implemented; everything else is open bus.

### 7.1 Custom registers (`$DFF000`)

| Register | Behaviour | Why |
|---|---|---|
| `VPOSR`/`VHPOSR` | ECS Agnus ID; free-running beam counter from the frame clock | graphics.library chip ID; beam-position spins |
| `INTENA`/`INTREQ` (+`R`) | Full set/clear semantics; 68k levels 1–6 | Entire OS interrupt model |
| `DMACON`/`DMACONR` | Latch; `BBUSY`/`BZERO` from software blitter | graphics waits on blitter |
| `ADKCON`/`ADKCONR` | Latch | Harmless reads |
| `BLT*` | Software blitter (§7.3) | Planar rendering |
| `COP1LC`/`COP2LC`, `COPJMP1` | Latched; consumed by renderer | Display (§8) |
| `BPL*`, `DIW*`, `DDF*`, `COLOR00–31`, `SPR*` | Latched; consumed by renderer | Display |
| `DSKBYTR`/`DSKLEN`/`DSKPT` | No disk; sink | trackdisk idles |
| `SERDATR`/`SERDAT`/`SERPER` | Sink; TBE set | serial.device idles |
| `POTGOR`/`POTGO`, `JOY0DAT`/`JOY1DAT` | Mouse deltas from host input | gameport.device unmodified |
| `AUD*` | Sink | audio.device opens; AHI is the real path |

**VERTB**: level-3 `INTF_VERTB` at 50 Hz (60 selectable) from the platform timer, shared with the beam counter and renderer so `WaitTOF`, VBlank servers and copper positions agree. One frame clock, designed in from day one.

### 7.2 CIAs

Two 8520s, register-level (not cycle-level) faithful: timers A/B (E-clock 709,379 Hz mapped to the platform timer), TOD + alarm, ICR with INT2/INT6 raising, **CIA-A SP/SDR keyboard handshake** (host HID → Amiga raw keycodes clocked in as hardware does — keyboard.device unmodified, the Amithlon trick), CIA-A PA / CIA-B PB per hardware.

### 7.3 Software blitter

graphics.library uses the blitter for *any* planar bitmap (Text(), offscreen BltBitMap, pointer imagery) even with Workbench on RTG, and P96 intercepts only RTG-bitmap paths; DraCo patched graphics.library, we don't patch, so the blitter must exist. **Synchronous** execution on `BLTSIZE`/`BLTSIZV`: all minterms, channels, shifts, masks, descending, fill, line mode; no timing model; `BBUSY` only during host-side execution; `BZERO` correct; blitter-finished interrupt raised. Pure data-to-data; differentially tested against Copperline before integration (§12).

## 8. Display

### 8.1 Stop-gap planar renderer

For visibility before P96 is installed (early startup menu, boot, gurus, Screenmode prefs) and as the permanent Guru/early-boot display. Deliberately dumb: once per frame walk the copper list linearly (MOVEs honoured, WAITs skipped), render from latched BPL/DIW/DDF/COLOR state — lores/hires, 1–5 planes, interlace, software pointer from sprite 0. No dragging, per-line palettes, HAM, EHB, dual playfield. ~500 lines on a spare core; output to the board layer's display surface. Not extended, ever.

### 8.2 P96 on a virtual framebuffer card (production path)

Zorro III card, large linear BAR + small register window; the `.card` sees a linear framebuffer, host-published mode list, vsync/cursor hooks, SetSwitch for native/RTG. Backing: virtio-gpu/ramfb under QEMU/KVM; GOP or board framebuffer bare-metal. BAR mapped cacheable with explicit flushes at SetPanning/vsync, never device memory.

### 8.3 AROS 68k graphics

Either P96 3.x on AROS (reported workable via the 3.1 ABI — verify) or a thin gfx HIDD over the same linear framebuffer, decided empirically; nothing above the framebuffer changes.

### 8.4 PCI GPUs (later)

Staged: (1) host/firmware-initialised linear framebuffer — any GPU the firmware lights up; (2) m68k modesetting on documented simple silicon (Radeon R100–R500, Matrox G, Voodoo3–5, Permedia) ported from period X.org drivers; (3) newer GPUs stay at stage 1.

**Existing P96 PCI drivers.** Current P96 ships `.card`s for Mediator/Prometheus-era PCI boards (Voodoo3/4/5, Permedia2, ViRGE, Cirrus, Radeon R1xx–R3xx) via `prometheus.library`/`openpci.library`. With a faithful Prometheus-compatible `pci.library` these may load unmodified — stage 2 becomes largely a bus-library problem. PCIe is transparent to the `.card` (same config space/BARs); silicon constraints: legacy PCI cards behind a cheap PCIe-to-PCI bridge (lowest-risk accelerated path), native-PCIe Radeon R4xx plausible with bus-layer patches, Voodoo/Permedia bridge-only. The bus library must reproduce Prometheus's actual byte-swap semantics, not a tidied reinterpretation.

## 9. Virtual Zorro III cards

Per the DiagArea convention (§6.3): one board per function, one DiagArea module each, independently versionable and disable-able from host config; interrupts shared on INT2/INT6; manufacturer number from the Aminet expansion ID list, product IDs per card type. Where an open m68k driver already exists, present its register map (e.g. LIDE-compatible IDE for `lide.device`) instead of inventing one. Non-block cards (AHI, host-fs, RTC) use a minimal doorbell + descriptor protocol; block storage uses MIRAGE (§10.3).

## 10. `pci.library`, virtio, and storage

### 10.1 `pci.library`

API-compatible with **prometheus.library/OpenPCI**: reuses the era's open m68k PCI drivers (SANA-II NICs, AHI cards, P96 `.card`s) and makes every new driver usable on real Amigas with Prometheus/G-REX/Mediator bridges. The library owns: ECAM config access (base via `devicetree.resource`); LE byte-swap accessors and BAR mapping attributes; `GetDMAAddress()`-style bus-address translation (never assumed by drivers); cache maintenance via `CacheClearE()` ranges mapped to host cache ops (the discipline real 040/060 drivers already follow); MSI/INTx → INT2/INT6 routing; 32-bit BARs only, host BAR assigner allocating below the Zorro III window (host apertures above 4 GB remapped into the 68k map).

**Driver discipline:** everything PCIe-specific (ECAM, MSI, 64-bit BARs, link management) lives in the library, never in drivers; drivers assume shared INTx-style interrupts and `CacheClearE()` maintenance so they behave identically on a real 060 behind a non-coherent Zorro III bridge. Occasional testing on physical Prometheus/Mediator hardware is the conformance test for both drivers and the library's fidelity.

**AmigaPCI alignment.** The AmigaPCI project (open-hardware OCS/ECS ATX Amiga with five native PCI 2.3 slots, FPGA bridge, true DMA to all Amiga/PCI address spaces; prototype stage) independently converges on the same ecosystem: A1200 3.2 ROM, lide.device, and per-slot "Prometheus mode" for standard PCI cards alongside an AUTOCONFIG mode for Amiga-specific ones (their hardware analogue of this machine's Zorro shim). Compatibility rules adopted so one driver corpus serves both machines and real Prometheus systems:
- **API**: prometheus.library v2/v3 implemented natively (the design and firmware are open-sourced); openpci.library's existing Prometheus wrapper then runs on top, bringing its driver corpus too. Elbox's closed Mediator pci.library is not an API target.
- **AROS**: implement a small PCI HIDD driver for the machine's ECAM — AROS's official PCI API is its HIDD classes, and AROS's own prometheus.library wrapper over them provides the Prometheus API for free on that OS.
- **Interrupts**: AmigaPCI ORs INTA–D onto INT2, drivers poll their device, bridge holds INT2 until all requests negate — the same level-triggered shared-server contract this library presents.
- **DMA**: the original Prometheus could not bus-master into Amiga memory (period drivers bounce through "DMA memory" in the gfx card window); AmigaPCI and this machine both do true DMA. The library's Prometheus DMA-allocation calls return real host RAM (matching AmigaPCI), while old bounce-buffer drivers continue to work unmodified.
- Byte order via address-invariant byte swapping in the accessors and BAR mappings, as both the original bridge and AmigaPCI implement in hardware.

### 10.2 virtio as the first bus

QEMU's device models are machine-independent: the same virtio-net/input/gpu (and the wider `-device` zoo, conventional-PCI models via a `pcie-pci-bridge` — reproducing the physical bridge-riser topology) attach to both aarch64-virt and x86-q35. Every virtio driver exercises the real `pci.library` endian/DMA/cache paths that physical silicon needs later; under KVM the same drivers run backed by the host's Linux drivers, then individual devices move to VFIO.

### 10.3 Storage: the MIRAGE interface

**The block card family adopts the MIRAGE interface** — the two-plane model (dumb block plane for RDB HDFs + mailbox management plane with async long operations, removable-media semantics, capability negotiation) designed for the Copperline WASM plugin and eventual real hardware. This machine is the interface's second implementation, sharing the m68k driver and RDB-mounting boot ROM unchanged; the DiagArea slot plays the role of controller flash, host config supplies ROM overrides per the hybrid ROM model; `CAP_DMA` is set (host-memcpy data path). The management plane doubles as the machine's primary disk-management UI — HDFs created/attached/detached from the running Amiga. SCSI/ZuluSCSI-style emulation stays out, per the MIRAGE decision. Strategic effect: this machine is MIRAGE's continuous second-host test bed before hardware exists.

**Backing store is a property of the card, not a card type**: file-backed, host-RAM-backed, zero-filled, or file-backed with host-RAM cache, per instance:

- *RAM-backed* (`RAM3:`-style): how memory above the 32-bit line serves the guest — no bank switching or pointer-model changes, the OS never sees an address; validation header → survives guest reboots RAD:-style without consuming guest RAM; doubles as the protocol's best-case benchmark.
- *Cached mode*: a host-side cache **under** the block interface (one device identity, one ordering authority), not a RAM drive syncing to disk. Host crash: honour ordering — the protocol carries flush; completed flush = durable; writes otherwise deferrable (a physical drive's write-cache contract). FFS has no flush discipline, so write-back is opt-in per instance with a bounded dirty window; boot drive defaults write-through. Guest crash: no issue — cache is host state. Concurrent host access: forbidden (exclusive open; host↔guest files go via the host-fs bridge). Staging: read cache first (unconditionally safe, most of the win), bounded write-back second. Implementation: LRU of ~4 MB extents, writeback thread, flush=drain; m68k driver's only change is `CMD_UPDATE`→flush.

**Driver set:**

| Driver | Interface | First backend | Later backend |
|---|---|---|---|
| block device (MIRAGE) | MIRAGE planes, `CAP_DMA` | QEMU | KVM (host NVMe), bare-metal MMC/NVMe/AHCI |
| RAM drive | same card, RAM-backed unit | host RAM > 4 GB | — |
| SANA-II | virtio-net via pci.library | QEMU | KVM, VFIO NIC, bare-metal |
| keyboard/mouse | CIA-A serial + JOYDAT | virtio-input | USB HID (xHCI) |
| P96 `.card` | linear BAR | virtio-gpu/ramfb | GOP/board FB, PCIe GPU |
| AHI | doorbell card | host audio | native (low priority) |
| host-fs bridge | doorbell card | host dir/partition | — |
| RTC | doorbell card | host clock | — |

virtio-blk is kept as a secondary path for raw host disks and as an extra `pci.library` exercise.

### 10.4 On-disk layout and partitioning

The physical medium is shared property (firmware regions, boot partition) and far larger than the guest needs:

**Physical layout:** `[firmware region / ESP] [FAT32 boot partition: host image, config, ROMs] [data partition: HDF images]` — editable from any PC; on x86 the firmware region is the ESP holding the UEFI payload.

**Guest disks are virtual, never the physical disk:** *file-backed HDFs* (default — same images mount in Copperline, WinUAE and amitools; boot HD is an `rdbtool`-built file; snapshots/migration are file copies; bare metal needs a FAT/exFAT driver, wanted for the boot partition anyway); *partition-backed* (Amithlon model — a designated-type MBR/GPT partition presented as a whole disk with its own RDB inside; host tools own the outer layout, HDToolBox the inner); *whole-disk RDB explicitly unsupported* (RDB-beside-MBR is mechanically possible and was the classic Amithlon-era footgun — two mutually ignorant schemes on shared blocks).

**Consequences:** the MIRAGE driver implements TD64/NSD and direct-SCSI from the start; boot selection is host config via the DiagArea boot node — switching boot environments is a text edit.

## 11. OS support

### 11.1 Kickstart 3.2

- **A1200 3.2 ROM.** The A500/A2000 ROM's expansion.library configures Zorro II only; Z3 AUTOCONFIG was never in 68000-class ROMs, and this machine hangs entirely off Z3 (same reasoning as the PiStorm A1200-ROM recommendation). A1200 beats A3000/A4000 (no SuperDMAC/Ramsey scsi.device probing) and adds only Gayle/IDE and PCMCIA probes — expected to fail cleanly under open bus; if not, a two-register stub, not a design change (verify early, §15).
- Software blitter required before Workbench draws; 68040.library per §6.3; SetPatch minimal.
- **Pre-built boot HD** via amitools (`rdbtool`/`xdftool`): 3.2 + P96 + screenmode prefs set to RTG; first boot lands on RTG unattended.

### 11.2 AROS 68k

ROM pair (ext at `$E00000` + main at `$F80000`; loader accepts single ROM or pair). Same chipset surface serves its timer/keyboard/mouse HIDDs, VPOSR read, and expansion/DiagArea boot. amigavideo renders largely on CPU (blitter barely exercised — a reason to keep it simple). Graphics per §8.3.

### 11.3 Driver discipline

All m68k drivers target the 3.1 API surface and NDK (open libraries at v39/40), no 3.2-only calls, no AROS-isms — one driver set for both OSes, and consistent with making the wider tool ecosystem run on both. PCI drivers additionally follow §10.1's real-Amiga portability rules.

## 12. Testing strategy

- **Blitter / renderer**: host-side unit tests; differential against Copperline (randomised ops, recorded Workbench traces; frame-by-frame for the renderer, minus deliberately absent features).
- **CPU**: m68k-rs's own step-vs-batch equivalence and test suites; Copperline's production use as ongoing validation. (Note: Copperline shares this CPU core, so CPU-semantics bugs can't surface as divergence against it; Emu68-based systems are the independent CPU oracle where one is needed.)
- **Drivers**: disk-loadable builds under Copperline/vamos before DiagArea ROMs; AmiPilot for UI-level checks.
- **Machine**: QEMU aarch64-virt and x86-q35 in CI; acceptance = unattended Workbench-on-RTG with input/storage/network on **both platforms × both ROMs** (Kickstart from Phase 3, AROS joining at Phase 4 of the roadmap). AROS, being freely redistributable, is additionally the ROM public CI boots on every commit from Phase 1 (smoke test: ROM pair loads, exec starts, serial reached); Kickstart runs use a user-supplied ROM on private/local runners.
- **Cross-platform differential**: identical guest + boot HD on two board layers — divergence localises to the board layers, since core and CPU are shared.
- **Hardware**: KVM for performance/coherency; VFIO for per-device `pci.library` validation; physical Prometheus/Mediator time as the PCI conformance test.

## 13. Hardware platforms

Bare-metal targets; QEMU/KVM stages run on anything.

| Platform | Role | Notes |
|---|---|---|
| **x86-64 UEFI** | **First bare-metal target** | One target ≈ all PCs of a decade: GOP framebuffer free, ACPI (`acpi` crate) → synthesized FDT for `devicetree.resource`, NVMe/AHCI/xHCI/HDA one driver each (Redox OS, MIT, as the driver quarry), HPET/APIC timers, mature VFIO for the KVM stage. Secure Boot documented off. |
| **RK3588 family** | **Second; the ARM flagship. Rock 5B in hand** | Defined as "RK3588 + vendor U-Boot + DTB", not one board (availability). Public TRM; U-Boot-derived SD/UART/GIC/PCIe-RC bring-up; A76 @ 2.4 GHz. KVM host for the ARM bring-up stage (verify EL2 via SPL vintage). M.2 + lanes suit the bridge/NVMe rig. |
| **Pi 5** | **Third; the community target** | Permanently purchasable. Most I/O in RP1 behind PCIe — arrives via `pci.library` with standard-IP drivers (xHCI, Cadence GEM) shared with RK3588; board-specific residue: BCM2712 start-up, SDHCI, GIC-400, brcmstb PCIe RC, `config.txt` boot (firmware pre-set framebuffer via mailbox). 70–80 % of bare-metal code shared with RK3588. |
| Pi 4 / RK3399 / Allwinner / others | Later, by demand | Pi 4 loses its old advantage (Emu68's existing drivers don't apply to this host); it becomes an ordinary port. Allwinner remains the cheap proof the board-layer abstraction holds. Qualcomm boards (Q6A/Q8B), Orion O6: fast but firmware-owned, thin docs — revisit if their ecosystems mature. |

**Interrupt controllers**: GICv3 (RK3588), GIC-400 (Pi 5), APIC (x86) behind one routing trait. **Memory**: guest footprint ≈ 256–384 MB; 4 GB boards ample; larger RAM serves the RAM-backed/cached MIRAGE instances.

## 14. Roadmap

Maintained separately in `combined-roadmap.md` (Phases 0–6). Summary: shared-core blind boot on both QEMU harnesses → blitter + renderer → full m68k stack and unattended RTG boot under QEMU ("basically usable, emulated") → KVM on Rock 5B and x86 desktops with the period-card conformance rig → bare metal in order x86 UEFI, RK3588, Pi 5 → comfort features (cached mode, management UX, AHI, Cranelift bare-metal, PCI GPU stage 2).

## 15. Risks and open questions

- **m68k-rs bare-metal**: `no_std` status of the interpreter core; Cranelift's std/page-service assumptions (mitigation: interpreter bare-metal, JIT hosted-only initially); single-author crate — pin versions, upstream fixes.
- **Interpreter performance on ARM** is the accepted trade vs Emu68; the §5.3 variant is the documented escape hatch. Measure early under KVM on the Rock 5B so the decision is data-driven.
- **A1200 ROM probes vs open bus**: confirm Gayle ID (`$DE1000`) and PCMCIA detection read as absent under `$FF`; fallback is a two-register stub.
- **VFIO on consumer ARM SoCs**: RK3588 may be `noiommu`-only (pinned identity-mapped guest memory); verify in week one — x86 VFIO maturity is the hedge, and is where the period-card rig lives regardless.
- **P96 on AROS 68k**: assumed workable; fallback gfx HIDD.
- **Cache coherency on real PCIe** dominates bare-metal debugging; `CacheClearE()` discipline enforced from the first driver.
- **Rock 5B practicalities**: SPL vintage vs EL2/KVM; USB-C PD pickiness — use a fixed 12 V supply, mandatory once NVMe + bridge riser raise the power budget.
- **Repo/upstream layout** for the m68k stack (per-project vs one machine repo): deferred, but the MIRAGE-shared pieces live with MIRAGE.

## 16. Licensing

Direction-of-flow layering:

- **Machine core, board layers, and all MIRAGE-shared m68k code: MIT or MPL-2.0** (own/permissive work only) — must remain usable by MIRAGE hardware, Copperline plugins, and any Emu68-based variant (MPL cohabitation).
- **Host binaries** may be GPL-3 if direct reuse from Copperline (GPL-3, third-party) or Circle (GPL-3) is wanted, at the cost of one-way flow; otherwise MPL/MIT keeps everything bidirectional. U-Boot-derived ARM board drivers: check per-file SPDX (GPL-2.0-only is GPL-3-incompatible); isolate in the board layer either way.
- Copperline as test oracle carries no licence implications. The all-Rust host can otherwise be fully MIT/Apache (m68k-rs, uefi-rs, x86_64, acpi, Redox-derived drivers).
- No Kickstart material included; users supply ROMs or use AROS 68k — the fully open configuration.

## Appendix A — Minimum register set

Custom: `VPOSR VHPOSR INTENA INTENAR INTREQ INTREQR DMACON DMACONR ADKCON ADKCONR BLTCON0/1 BLTAFWM BLTALWM BLTxPT BLTxMOD BLTxDAT BLTSIZE BLTSIZV BLTSIZH COP1LC COP2LC COPJMP1 BPLxPT BPLCON0/1/2 BPL1MOD BPL2MOD DIWSTRT DIWSTOP DDFSTRT DDFSTOP COLOR00–31 SPR0PT SPR0POS SPR0CTL SPR0DATA/B DSKBYTR DSKLEN DSKPT SERDAT SERDATR SERPER POTGO POTGOR JOY0DAT JOY1DAT`.

CIA-A/B: `PRA PRB DDRA DDRB TALO TAHI TBLO TBHI TODLO TODMID TODHI SDR ICR CRA CRB`.

Everything else: open bus, `$FF`.
