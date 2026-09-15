# `pcibridge` register and config-cycle contract

**Status:** host side implemented (`crates/machine-core/src/pcibridge.rs`
over `crates/machine-core/src/pci.rs`), wired into
`MachineBus::with_pcibridge` and `machine-hosted`'s `--pcibridge` flag.
This is ADR 0005 **stage 1 only**: the Zorro III shim that exposes a PCI
configuration/BAR space to the guest, proven by host-side enumeration
tests — before any 68k code, any `pci.library` (stage 2), or any
virtio-net driver (stage 3) exists. The register contract below is what
stage 2's library will program; treat changes to it as breaking a
published driver ABI even though no driver exists yet, because the whole
point of landing it first is that the library gets written against it.

**Context:** `docs/adr-0005-networking-virtio-behind-pci-library.md` (the
decision and staging this executes — read it first);
`docs/m68k-machine-proposal.md` §10.1–10.2 (the Prometheus/OpenPCI
contract stage 2 must honour, which constrains what this shim may look
like); `docs/device-ledger.md` (this board's row: permanent — the bus
grows more load-bearing over time, like AUTOCONFIG); `docs/
hostblk-protocol.md`, `docs/rtgboard-protocol.md`, `docs/
input-protocol.md` (the sibling documents whose shape and section
numbering this one follows).

---

## 1. Identity

One Zorro III AUTOCONFIG board:

| Field | Value |
|---|---|
| `er_Manufacturer` | `0x07DB` (2011) — the same NDK 3.2 `libraries/configregs.h` reserved "hacker" ID every native card here uses (`hostblk.rs`'s module docs carry the story of why `0xFFFF` was tried and rejected by real Kickstart 3.2.2). Still a stand-in: a real registered number is needed before this ships on hardware. |
| `er_Product` | `6` — distinct from `mirage` (`0`), `hostblk` (`1`), `fastram` (`2`), `input` (`3`), `rtgboard` (`4`) and `pktport` (`5`). |
| `er_Type` | `ERT_ZORROIII` (extended-table code 0 → 16 MB window, same as every other Zorro III board here) |
| `er_Flags` | `ERFF_ZORRO_III \| ERFF_EXTENDED` |
| `er_InitDiagVec` | `0` — no DiagArea. Stage 2's `pci.library` is disk-loaded first; whether it ever moves into a board ROM is that increment's decision, not this one's. |
| Window size | 16 MB: the register file in the first `0x28` bytes, the BAR aperture as the upper 8 MB (`0x800000`–`0xFFFFFF`), and everything between them unimplemented board space (reads `0`, writes discarded) — room for the register file to grow, the same posture `rtgboard`'s register/VRAM split documents. |

## 2. What backs the shim — the load-bearing decision

The design question this increment existed to settle: **what stands on
the host side of this card, given that the hosts differ in kind.**

- `machine-hosted` (the std runner driving m68k-rs directly) has **no
  real PCI bus at all**. Its backing must be a host-side *virtual* PCI
  topology.
- The bare-metal board crates (`board-qemu-virt` aarch64,
  `board-qemu-q35` x86-64) have **real ECAM** — a genuine PCIe root
  complex whose config space is a memory region, and real BARs behind
  it.

The answer is the `PciBackend` trait (`crates/machine-core/src/pci.rs`):
four methods — config read/write and memory-space read/write, each with
an explicit access width (1/2/4 bytes) — carrying register **values**
(correctly decoded integers), never byte images. The card is written
against the trait and does not know which backing it has; the board
layer decides, the same borrowed-backing philosophy as `hostblk`'s
`BlockDevice`, `pktport`'s `PacketBackend` and `rtgboard`'s mode
catalog.

**machine-hosted's backing** is `pci::VirtualPciBus`: a caller-owned
slot list of virtual devices (no allocator — machine-core is
`no_std`). The topology `--pcibridge` attaches is a root that looks
like what the real boards' ECAM will show: a host bridge at `00:00.0`
wearing QEMU's own root-complex identity (`1b36:0008`), and at
`00:01.0` a **config-space-complete modern virtio-net device** —
vendor/device `1af4:1041`, revision 1 (the virtio 1.x floor for
non-transitional devices), class `020000`, a 16 KiB 32-bit
non-prefetchable BAR0 with genuine sizing semantics, and the four
hand-encoded `virtio_pci_cap` capability structures
(COMMON/NOTIFY/ISR/DEVICE, `notify_off_multiplier` = 4) a real
capability walk expects. Its *function* is deliberately absent — BAR
reads answer all-ones — because enumeration must see a real-shaped
device and stage 3 owns the rings.

**The real-ECAM backing is documented, not built** (the QEMU boards do
not even have storage yet). The trait's own doc comment carries the
mapping a board crate implements when its turn comes: a config access
is one volatile load/store of the requested width at

```text
ecam_base + (bus << 20 | device << 15 | function << 12 | offset)
```

and on a little-endian host (every board target this project has) the
loaded integer *is* the value the trait traffics in — no byte swap
anywhere on that path. `mem_read`/`mem_write` are raw loads/stores at
whatever address the board's memory map gives PCI memory space
(`u64`, so a host aperture above 4 GB — proposal §10.1's "host
apertures above 4 GB remapped" case — stays representable). BAR sizing
probes are ordinary config writes and reads, so nothing stage 2 needs
is outside the trait; the seam is demonstrably ECAM-backable without
being changed.

## 3. Byte order: where the Prometheus contract does and does not live

Proposal §10.1 commits stage 2 to prometheus.library's *actual*
byte-swap semantics, and ADR 0005 names getting that quietly wrong as
the black-screen class of bug in network form. This shim takes a
deliberately clean position underneath that obligation:

- The `PciBackend` trait carries **values with explicit widths**. On
  the trait there is nothing to swap: PCI config space is defined
  little-endian, both host architectures are little-endian, and a
  same-width load already decodes it.
- The card's registers use the same big-endian byte-lane idiom as
  every sibling card (§4), so a 68k `move.l` from `CFG_DATA` yields
  the correctly decoded value directly. A config read of
  `00:01.0` offset `0` through this card gives `$10411AF4`
  (device `<<16 |` vendor) — a value, with no address-invariance
  puzzle in the middle.
- The **Prometheus presentation** — whatever mixture of swapped
  accessors, address-invariant BAR windows and era-faithful quirks the
  real library exhibits — is stage 2's contract with its drivers,
  implemented in the library's own accessor functions. A library
  handed values plus explicit widths can present *any* byte-order
  contract to its callers, including a warts-and-all one; what it
  cannot do is recover information a lossy contract discarded. That is
  why the shim traffics in values: it is the presentation-neutral form.

The one place address-invariance genuinely appears in this increment is
the BAR aperture (§5): byte `k` of the aperture is byte
`APERTURE_BASE + k` of PCI memory space, which is exactly the
address-invariant window a real Prometheus bridge gives onto a card's
memory — a big-endian CPU doing multi-byte loads through it sees
little-endian device registers byte-swapped, and it is the *driver's*
(stage 2/3's) job to swap, exactly as on the real bridge.

## 4. Register map

Same idiom `hostblk.rs`/`input.rs`/`rtgboard.rs` established: one hot
byte per 4-byte-aligned slot, `u32` registers as four big-endian byte
lanes, single-byte registers hot at `offset+3` from the 68k side.
Anything not listed reads `0` and discards writes.

| Offset | Register | Width | Access | Notes |
|---|---|---|---|---|
| `0x00` | `VERSION` | u32 | R | protocol version; `1` for this document |
| `0x04` | `CFG_ADDR` | u32 | RW | ECAM-style target: bits 27–20 bus, 19–15 device, 14–12 function, 11–0 offset into the 4 KB config space. Bits 31–28 reserved, must be zero (checked at `CFG_OP` time — the register itself is plain scratch) |
| `0x08` | `CFG_WIDTH` | byte | RW | access size in bytes: `1`, `2` or `4` (validated at `CFG_OP` time) |
| `0x0C` | `CFG_DATA` | u32 | RW | staged write data / latched read result. Value semantics (§3); a 1- or 2-byte read latches into the low bits with the high bits zero |
| `0x10` | `CFG_OP` | byte | W | `0` = config read (result → `CFG_DATA`), `1` = config write (of `CFG_DATA`). Synchronous: the cycle completes within the write itself, like `rtgboard`'s `COMMIT` |
| `0x14` | `CFG_STATUS` | byte | RW | write-1-to-clear, ungated; bit 0 = `REJECTED`, bit 1 = `COMPLETED`, mutually exclusive per op |
| `0x18` | `APERTURE_BASE` | u32 | RW | PCI memory-space address the BAR aperture maps to. Plain scratch — takes effect on the next aperture access, no commit step |
| `0x1C`–`0x27` | reserved | — | — | reads `0`, writes discarded. Reserved for stage 3's INTx routing registers (`INTX_STATUS`/`INTX_ENABLE`), so the map will not reshuffle when interrupts arrive |
| `0x800000`–`0xFFFFFF` | BAR aperture | — | RW | 8 MB banked window into PCI memory space (§5) |

Driver-side (stage 2) config-cycle shape:

```
CFG_ADDR   = bus<<20 | dev<<15 | fn<<12 | offset;
CFG_WIDTH  = 4;
CFG_OP     = 0;                    /* read */
if (CFG_STATUS & STATUS_COMPLETED)
    value = CFG_DATA;              /* $FFFFFFFF == nothing answered (§6) */
CFG_STATUS = 0xFF;                 /* write-1-to-clear, ungated */
```

## 5. The BAR aperture: banked, address-invariant, byte-granular

The upper 8 MB of the window is a movable pane onto PCI memory space:
an access at window offset `0x800000 + k` becomes a PCI memory-space
access at `APERTURE_BASE + k` (unsigned 64-bit arithmetic; two 32-bit
values, so the sum cannot overflow). Banking via `APERTURE_BASE` is the
same reason the real Prometheus banks a small Zorro window into 4 GB of
PCI space: the window is a scarce resource, the space behind it is not.

Two properties recorded now so stage 2/3 build on them knowingly:

- **Address-invariant** (§3): aperture byte `k` is PCI byte
  `APERTURE_BASE + k`. Multi-byte loads by the big-endian CPU see
  little-endian device registers swapped; the driver swaps, as on real
  Prometheus/AmigaPCI hardware.
- **Byte-granular today.** The Zorro bus hands this card one byte per
  access, so aperture traffic reaches the backend as 1-byte accesses.
  Virtio's modern spec requires drivers to access fields with their
  natural width, so **before stage 3 drives real ring registers through
  this aperture, the card needs the bus's word/long accesses delivered
  as sized accesses** (the `PciBackend` trait already carries widths;
  the gap is bus-to-card plumbing, not the seam). Recorded here as a
  known stage-3 prerequisite rather than discovered then.

Nothing in this increment allocates BAR addresses. Stage 2's library
owns BAR assignment (proposal §10.1: 32-bit BARs, host assigner
allocating below the Zorro III window); this increment only proves the
probe-and-size machinery it will use.

## 6. Config cycles: what `CFG_OP` checks, in order

A write to `CFG_OP`'s hot byte validates and executes, or rejects, on
the **first** failure — never a partial effect, never a panic. On
rejection `CFG_DATA` is untouched (structurally: no code path writes it
before every check passes).

1. **Unknown op.** The value written must be `0` (read) or `1` (write).
2. **Reserved address bits.** `CFG_ADDR` bits 31–28 must be zero — a
   guest setting them is using addressing this version does not define.
3. **Unknown width.** `CFG_WIDTH` must be `1`, `2` or `4`.
4. **Misalignment.** The 12-bit offset must be width-aligned. (Within a
   4 KB space, alignment also guarantees the access cannot cross the
   config-space boundary, so there is no separate range check to get
   wrong.)

Everything that passes validation **completes**, including a cycle
addressed at nothing: an absent bus/device/function reads **all-ones**
(`$FFFFFFFF` for width 4) with `COMPLETED` set — real PCI master-abort
semantics, and deliberately *not* a `REJECTED`. All-ones **is** the
signal enumeration uses to discover absence; a shim that turned it into
an error would be inventing a distinction real hardware does not make,
and stage 2's library (which must behave like a real bridge's) could
not use it.

## 7. Hostile input, beyond `CFG_OP`

- Out-of-range `device` (≥ 32) or `function` (≥ 8) values are
  representable in `CFG_ADDR`'s bit fields by construction (5 and 3
  bits respectively) — there is no way to express an invalid one.
  A `Bdf` no device answers reads all-ones per §6.
- Aperture accesses are bounds-free by construction: any offset in the
  8 MB pane maps to a well-defined 64-bit PCI address, and an address
  nothing claims reads all-ones/discards through the backend's own
  contract. The card performs no address arithmetic that can wrap
  (`u64` sum of two zero-extended `u32`s).
- The virtual topology behind machine-hosted fails closed in the same
  way at its own layer: absent slots all-ones, config offsets past the
  256-byte conventional space all-ones (PCIe extended config space is
  deliberately not modelled — the first real device arrives behind a
  conventional-bridge shape, per proposal §10.2's `pcie-pci-bridge`
  topology), containment math `checked_sub`-based so a hostile BAR
  window near `u64::MAX` cannot wrap into matching.
- Writes to read-only registers (`VERSION`), reserved slots, and the
  unimplemented gap are discarded; reads of write-only slots
  (`CFG_OP`) return `0`.

## 8. Why there is no interrupt on this card yet

The same reasoning as `rtgboard` §8: a config cycle validates a few
integers and calls a synchronous backend — the result is in
`CFG_STATUS`/`CFG_DATA` before the instruction after the `CFG_OP` write
executes, so there is nothing for INT2 to announce. The genuinely
asynchronous thing on a PCI bus — a device raising INTx — belongs to
stage 3 (virtio-net's ISR), routed per proposal §10.1's contract
(INTA–D ORed onto INT2, level-triggered shared-server discipline,
drivers poll their device). The reserved register slots at
`0x1C`–`0x27` are where that lands, and the `PciBackend` trait can
grow a defaulted `intx_level()`-style method without breaking existing
implementations. Nothing else about this card's contract is expected
to change for it.

## 9. Verification

*(Filled in as the increments land; see the test names for the actual
evidence.)*

Unit tests in `pci.rs` prove the backing layer: enumeration reads at
every width, absent-`Bdf` master-abort all-ones, the BAR sizing probe
exact (`0xFFFFC000` for 16 KiB), low BAR bits genuinely unwritable,
COMMAND writable-bits mask, the four-capability virtio chain walked the
way a real driver walks it, memory-space routing gated on COMMAND
memory-enable, and containment math that cannot wrap at `u64::MAX`.

Unit tests in `pcibridge.rs` prove the guest-visible contract: full
config read/write cycles through the register file at all three widths,
BAR sizing through the card's own registers, every `CFG_OP` rejection
case with `CFG_DATA` untouched and recovery after a `W1C` clear,
absent-device cycles completing with all-ones, and the aperture
round-trip against a device with a programmed BAR.

Bus-level tests in `lib.rs` prove it on the real bus: enumeration
walked entirely through `read_byte`/`write_byte` against the card's
AUTOCONFIG-assigned placement, and coexistence — the full native chain
(`hostblk`, `input`, `rtgboard`, `fastram`, `pktport`) placed
byte-identically with and without this board attached last.

Real-ROM verification (2026-09-15):
`kickstart_3_2_2_a1200_configures_the_pcibridge_board`
(`crates/machine-hosted/tests/real_rom.rs`) boots the real Kickstart
3.2.2 A1200 ROM with `--pcibridge --inspect` and confirms
`expansion.library` itself configured the board — positive evidence,
not absence-of-complaint. Measured: AUTOCONFIG placed the board at
`$50000000` (last in the chain, after fast RAM's own Zorro III board,
per the deliberate attach-last ordering `run.rs` documents), and the
guest's own `ConfigDev` for it was found at chip RAM `$000018A0` with
`er_Type $80`, `cd_BoardAddr $50000000`, `cd_BoardSize $01000000` —
Kickstart's own record agreeing with our bus's, with the
healthy-`ExecBase` guard (`$4000089C`) intact. The cheap real-ROM
baselines (`kickstart_3_2_2_a1200`, the introspection test, the
boot-screen screenshot, the AROS boot screen) all re-ran green
alongside it.

## 10. What this increment does not include

- **No `pci.library`, no 68k code of any kind.** Stage 2 builds the
  Prometheus-compatible library against this contract; its
  conformance obligations (the *actual* byte-swap presentation,
  `GetDMAAddress()`, `CacheClearE()` mapping, INTx→INT2) are §10.1's,
  not this document's.
- **No BAR allocation.** The shim proves sizing works; the library owns
  assignment policy (32-bit, below the Zorro III window).
- **No interrupts** (§8) — reserved space exists, nothing is wired.
- **No sized aperture accesses** (§5) — a recorded stage-3
  prerequisite.
- **No virtio function.** The stub answers enumeration, capability
  walks and BAR probes; its BARs have no registers behind them. Rings,
  queues and the SANA-II driver are stage 3.
- **No board-crate wiring.** `board-qemu-virt`/`board-qemu-q35` gain a
  real-ECAM `PciBackend` implementation when they gain devices at all;
  this increment owes them a demonstrably ECAM-backable trait contract
  (§2), which is what landed. `devicetree.resource`-based ECAM base
  discovery (ADR 0005's "arrives early" cost) is likewise deferred to
  the board-crate increment: on machine-hosted there is no ECAM base to
  discover.
