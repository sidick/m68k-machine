# Device ledger — what is permanent and what is scaffolding

**Status:** living document. Every device this machine presents to the
guest has a row here, and any new one must gain a row in the same change
that introduces it.

**Context:** proposal §5, §8, §10.3; `adr-0001-bare-metal-vs-linux-host.md`;
`adr-0002-rtg-on-generic-display-hardware.md`.

---

## Why this file exists

This project keeps meeting the same fork: a device can be *emulated*,
so unmodified AmigaOS drives it with no software of ours, or it can be
*native*, so the guest talks to something designed for this machine and
we supply the driver. Emulation always wins on immediate progress. It is
how a real Workbench desktop appeared on RTG without a line of 68k code
of our own.

Taken one at a time, each of those choices was right. Taken together
they drift toward writing an Amiga emulator, which is not what this
project is for. `machine-core` is around 13,450 lines, of which roughly
10,400 are device emulation — about 7,700 emulating Amiga silicon and
about 2,700 emulating a 1990s PC graphics chip.

The problem was never that we built them. It is that nothing recorded
which ones were meant to be temporary. Gayle IDE was introduced as a
bring-up device with MIRAGE intended to replace it; that intent lived in
a commit message and in one conversation. Nothing in the repository said
Gayle was provisional, what would retire it, or who decides. This file
is that record, so "we will move off it later" is reviewable rather than
remembered.

## What "chipset-less" actually claims

Worth stating plainly, because the name promises more than is
achievable and the gap is where confusion breeds.

Kickstart 3.2 is unmodified ROM. From reset it pokes CIA registers and
the custom register file long before it reaches DOS, and it will not
boot without them. Those are a **floor**, not drift, and no amount of
native hardware retires them while the goal is running stock Kickstart.

What "chipset-less" claims is that the **display, storage and I/O paths**
need not run through Agnus, Denise and Paula: the display comes from an
RTG board, storage from a native block device, and I/O from real modern
hardware. The chipset survives as a compatibility surface for software
that insists on it, not as the machine's own way of working.

Measured against that, a full planar renderer with copper walking,
sprites and bitplane DMA, plus a blitter verified to 1,245 cases, is
further than the claim requires. It was authorised deliberately — §8.1
was expanded to cover everything needed to run — and it worked. It is
also the clearest example of why this ledger needs to exist.

## The rule for new devices

**Native-first.** A new device is emulated only when bootstrapping
genuinely forces it — that is, when unmodified Kickstart touches it
before any driver of ours could be loaded. "It is quicker" and "P96
already has a driver for it" are reasons to reach for emulation, and
they are exactly the reasons that produced the drift above. They are not
sufficient on their own.

Where emulation is chosen anyway, it lands in the table below as
**bring-up**, with a named successor and a retirement criterion, in the
same change that introduces it.

## The rule for addresses

**Do not assume or hardcode memory locations.** AUTOCONFIG exists to
allocate ranges dynamically, and nothing downstream of it should encode
a guess about where a board landed. Fixed addresses are legitimate only
where the real hardware fixes them — Gayle's register window, the custom
chip registers, the ROM overlay — and those are the permanent rows
below, not new work.

The failure mode is subtle enough to be worth naming, because the
project walked into it once. `hostblk` validated guest addresses against
`0..CHIP_RAM_SIZE`, which was true when chip RAM was the only RAM. When
fast RAM arrived, the tempting fix was to add the second range to the
check — which would have hardcoded *two* assumptions instead of one, and
broken again on the third region, or as soon as an AUTOCONFIG-assigned
base moved.

The right shape is to **ask whatever owns the memory map** rather than
compare against constants. `MachineBus` is the only thing that
legitimately knows the layout, so a device that needs to validate a
guest address asks it. If you find yourself writing an address constant
or a range comparison for something AUTOCONFIG placed, that is the
signal to route the question back rather than to widen the constant.

That seam now exists: `lib.rs` defines a `GuestMemory` trait
(`ram_slice`/`ram_slice_mut`, validated `[addr, addr+len)` views) and
implements it on `MachineBus`, checking chip RAM's fixed range and fast
RAM's AUTOCONFIG-placed one (`self.autoconfig.placement`, never a
hardcoded base) without either range living anywhere but that one
implementation. `hostblk.rs`'s own `0..CHIP_RAM_SIZE` check has not been
changed to use it yet — that file is owned by whoever reviews this
change, per this project's file-ownership convention — but the seam it
would call through is ready: `tick`/`execute`/`read_descriptor`/
`transfer` would take `&(mut) dyn GuestMemory` instead of `ram: &mut
[u8]`, and the `MachineBus::tick` call site would need the `Option::take`
dance (`let Some(mut h) = self.hostblk.take() { h.tick(self); ...
self.hostblk = Some(h); }`) to hand `hostblk` a `&mut dyn GuestMemory`
view of `self` while `self.hostblk` itself is also borrowed — two
disjoint borrows of `self`'s fields, not a conflict, but only once
`hostblk` no longer occupies one of them for the duration.

## The ledger

| Device | Status | Successor | Retires when |
|---|---|---|---|
| CIA 8520 A/B | permanent | — | never; Kickstart drives them from reset |
| Custom registers + interrupts | permanent | — | never; same |
| ROM overlay / mirroring | permanent | — | never; part of the machine |
| Zorro AUTOCONFIG | permanent | — | never; this is how native boards attach |
| Fast RAM (`fastram`) | permanent | — | never; it is memory, not an emulation of a product |
| Floppy "no drive attached" | permanent | — | never; models absence, not a drive |
| Planar renderer | capped | RTG | never fully; stops growing (below) |
| Blitter | capped | RTG | never fully; stops growing (below) |
| Gayle IDE | **bring-up** | `hostblk` (ADR 0003) | `hostblk` boots the same image unaided |
| `hostblk` doorbell card | permanent | — | never; the machine's own storage |
| MIRAGE block plane | permanent, off the boot path | — | never; kept as MIRAGE's reference implementation |
| Cirrus CL-GD542x | **bring-up** | generic virtual board (ADR 0002) | demoted to compatibility tier, not removed |
| Graffity Z2/Z3 | **bring-up** | as Cirrus | as Cirrus |
| Keyboard / mouse | **not built** | native input board | n/a — native-first from the start |

### Permanent

**CIA 8520 A/B, custom registers, interrupts, ROM overlay.** The floor
described above. The ROM overlay in particular is not optional: the
reset vector fetch at `$000000` depends on it.

**Zorro AUTOCONFIG** is in this table but is not emulation of anything
legacy in spirit — it is the standard mechanism by which a board
announces itself, and it is how *native* boards will attach too. It gets
more load-bearing as the machine becomes less emulated, not less.

**Fast RAM** — `fastram.rs`. A Zorro III AUTOCONFIG memory board
(`ERTF_MEMLIST` set) over caller-owned storage, exactly like chip RAM and
VRAM: this crate has no allocator, so it borrows rather than owns. It is
listed as permanent rather than bring-up because it is not standing in
for a piece of silicon this machine lacks — it is memory, the same way
chip RAM is, just reachable a different way. `hostblk` is the immediate
reason it exists (its transfer buffers were confined to the 2 MB chip
window purely because that was all there was), but any `MEMF_FAST`
allocation benefits.

Confirmed adopted by real Kickstart 3.2.2 (`nondistribution/
A1200.47.115.rom`), not merely accepted by this bus: `--inspect`'s
`MemList` walk shows a second `MemHeader` (`FAST|PUBLIC`, matching the
requested size) alongside chip RAM's own entry, and cross-checked byte
-for-byte against Copperline (this project's oracle) booting the same
ROM with its own Zorro III fast RAM — both place `ExecBase` itself at
the identical address once fast RAM is available (`$4000089c` for a
16 MB board), which is a real AmigaOS behaviour (Kickstart relocates
`ExecBase` into fast memory once it exists), not an artefact of either
emulator. See `fastram.rs`'s module docs for the full reasoning trail
(why AUTOCONFIG rather than a fixed accelerator-style address, and the
Zorro III extended-size table), and this file's "rule for addresses"
above for the `hostblk`-facing seam ([`GuestMemory`], implemented on
`MachineBus` in `lib.rs`) this board's arrival made necessary.

Off by default (`machine-hosted --fast-ram-mb N`): registering an extra
AUTOCONFIG board changes the chain, and where fast RAM sits in it
relative to a Zorro III Graffity card (`--graphics-bus 3`) can move that
card's assigned base address and, with it, the RTG screenshot baseline.
`machine-hosted` registers fast RAM *after* `--graphics` specifically so
Graffity is always offered to the chain first regardless of whether
`--fast-ram-mb` is set — confirmed empirically that both the planar and
both RTG (Zorro II and Zorro III) baselines stay byte-identical with the
flag on, at 16 MB through 1 GB.

**Floppy "no drive attached"** models the absence of hardware rather
than the presence of it. It exists because Kickstart hangs on a black
screen without an answer, and matches a real A1200 with no drive fitted.
There is nothing to retire.

**`hostblk` doorbell card.** The machine's own storage, and what Gayle
retires into. A doorbell-plus-descriptor Zorro III card whose transfer
engine runs on the host, moving data directly to and from guest RAM
against the same `BlockDevice` trait Gayle and MIRAGE use, with
asynchronous completion over INT2 via a bounded completion queue — see
ADR 0003 for why the boot path is not PIO, and `docs/hostblk-protocol.md`
for the register/wire-format contract.

*Built so far (host side only):* the register interface (`hostblk.rs`),
the transfer engine, per-unit discovery (attached/size/write-protect/
change-counter — the gap the MIRAGE review found missing), the
submission and completion queues and their overflow/backpressure rules,
and the `machine-hosted --hostblk` CLI flag. **Not yet built:** the m68k
driver and the DiagArea boot ROM that mounts an RDB from it — this
increment cannot boot a machine on its own, only exercise the card's
host-side half end to end.

**MIRAGE block plane** — `mirage.rs`. Native rather than bring-up: a
board designed for this machine and driven by our own m68k code, not a
stand-in for silicon the ROM already knows. It was Gayle's named
successor until ADR 0003 moved the boot path to `hostblk`, on the
grounds that MIRAGE's shape is dictated by a Zorro II bus and an SD
backend that this machine does not have.

It is kept deliberately, off the boot path, because it is MIRAGE's
**reference implementation** — no other exists — and because keeping it
makes ADR 0003 cheap to reverse if a real MIRAGE board is ever built.
Its register model still runs in CI. What is deferred is driver-level
conformance, not the interface.
*Cost of keeping:* ~1,100 lines including tests, carried without being
on any critical path. It also carries a live cost the RFC still needs to
close: §4.1's register sketch is under-specified for a real hardware
target — no unit discovery, no agreed command/status bit encoding, and
an unreviewed choice between a destructive FIFO and a re-readable buffer
behind `DATA`. See `mirage.rs`'s module docs for the full list, which
matters more than usual because this implementation *is* the spec's
reference, not a second opinion on an existing one.

### Capped

**Planar renderer and blitter.** These can never be fully retired: the
boot screen, ROMWack, and any software that drives the chipset directly
all need them, and they run before RTG could possibly be initialised. But
they should stop *growing*. New display work belongs in the RTG path.

Concretely: no cycle-accurate DMA scheduling, no copper fidelity beyond
what booting needs, and no new blitter surface unless real software is
demonstrably broken without it. The 1,245-case differential stays —
it is regression protection for what exists, and it is what caught the
21-bit pointer-register truncation that eight years of correct rendering
would never have surfaced.

### Bring-up

**Gayle IDE** — `gayle.rs`. Introduced to get a real disk under the
machine so Workbench could boot at all, using the ROM's own
`scsi.device` and no 68k code of ours. It works and currently boots
AmigaOS 3.2.2 from an HDF.

*Successor:* `hostblk` (ADR 0003) — a doorbell-plus-descriptor block
card with a host-side data path, our own m68k driver, and an
RDB-mounting boot ROM. This was MIRAGE until ADR 0003 moved the boot
path off PIO.
*Retires when:* `hostblk` boots the same image with no Gayle attached.
*Cost of keeping:* ~1,000 lines, plus an IDE task-file model and its
interrupt semantics that must stay correct forever. The per-sector
INTRQ-on-read bug that cost a debugging cycle is the kind of thing this
device will keep producing.

**Cirrus CL-GD542x and the Graffity boards** — `cirrus.rs`,
`graffity.rs`. Emulating specific 1990s silicon purely so that P96's
shipped `Graffity.card` drives it. This bought a real Workbench desktop
on RTG with nothing installed on the guest.

*Successor:* the generic virtual board in ADR 0002, with our own
`.card`.
*Retires when:* it does not, quite. ADR 0002 keeps it deliberately as
the zero-install compatibility tier — the only path that works against a
completely untouched install. The change is **demotion**: the generic
board becomes the machine's own display path, and Cirrus stops being
where new display work happens.
*Cost of keeping:* ~2,700 lines, including a BitBLT engine, a RAMDAC,
and quirks like SR12's cursor-palette redirect that exist only because
one specific driver uses them.

### Not built — decided native-first

**Keyboard and mouse.** Legacy input via the CIA-A serial handshake and
`JOY0DAT` quadrature counters is roughly half-built already
(`chipset::mouse_delta`, `mouse_button`, and a verified CIA keyboard
encoding), and finishing it would be cheap.

It is deliberately not being finished first. Input is the first device
with **no bootstrap dependency at all** — nothing in early Kickstart
boot needs a mouse or a keyboard, and this machine already reaches
Workbench with neither. It is therefore the first place where
native-first costs nothing, and choosing legacy because it is cheap and
half-built is precisely the reasoning this ledger exists to interrupt.

A native input board carries its driver in its own AUTOCONFIG ROM via
DiagArea injection, so unlike the RTG case it needs nothing installed on
the guest — the "needs a file on disk" objection does not apply here.

Legacy input stays available as a fallback tier, in the way the planar
path is for display, and may still be built if a driver-carrying board
proves fragile: a board that misbehaves leaves the machine unusable
rather than merely less comfortable. But it is no longer the default
answer.

## Sequencing

MIRAGE comes before the input board, for a reason beyond storage. A
native device needs AUTOCONFIG, DiagArea injection, and our own 68k
driver in board ROM — machinery this project has never built. MIRAGE is
already specified, it replaces the most emulated part of the storage
path, and once that machinery exists the input board is largely a reuse
of it. Doing input first would build the same machinery for a smaller
payoff and leave Gayle in place.

## How a device gets retired

1. Its successor boots the same image, verified end to end, not by unit
   tests alone — this project has repeatedly found that self-consistent
   unit tests pass over real bugs.
2. Both paths run side by side for long enough to trust the new one,
   with the old one selectable.
3. The old device's row moves to a "retired" section here, with the
   commit that removed it, rather than being deleted — the record of
   what was scaffolding is worth more than a tidy table.
4. Its tests are removed only with it. Deleting tests ahead of the
   device is how a blitter once vanished from a commit while the
   aggregate test count still looked healthy.
