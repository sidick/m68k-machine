# `hostblk` register and wire-format contract

**Status:** host side implemented (`crates/machine-core/src/hostblk.rs`);
no m68k driver or boot ROM yet. This document is the contract a driver
author needs; it does not describe a driver that exists today.

**Context:** `docs/adr-0003-native-block-storage-doorbell-not-pio.md`
(the decision this implements); `docs/m68k-machine-proposal.md` §9 (the
doorbell-plus-descriptor pattern this extends); `docs/device-ledger.md`
(where this device sits); `~/src/project-ideas/active/copperhf-device-design-note.md`
§2 (the closest existing sketch of this mechanism — a design note for a
different, also-unbuilt device, not a spec this implements).

---

## 1. Identity

One Zorro III AUTOCONFIG board:

| Field | Value |
|---|---|
| `er_Manufacturer` | `0x07DB` (2011) — the **reserved "hacker" ID** NDK 3.2 `libraries/configregs.h` sets aside for test use. Was `0xFFFF` until the fast RAM work found real Kickstart 3.2.2 *silently rejects* that value for a board it is asked to add to the memory list, with the base-address write never landing and no error anywhere. This card is not a memory board so it never hit that, but carrying an ID the ROM is known to reject is a trap for whoever next sets `ERTF_MEMLIST`. Still a stand-in: a real registered number is needed before this ships on hardware or as a Copperline plugin (ADR 0003 requires MIT licensing for exactly that reuse). |
| `er_Product` | `1` — distinct from `mirage`'s `0`, so the two boards remain distinguishable if both are attached at once. |
| `er_Type` | `ERT_ZORROIII` (extended-table code 0 → 16 MB window; no size bits of its own) |
| `er_Flags` | `ERFF_ZORRO_III \| ERFF_EXTENDED` |
| Window size | 16 MB, of which only the first `0x38` bytes carry registers; the rest is unimplemented board space (reads `0`, writes discarded). |

## 2. The core design choice: a descriptor pointer, not a raw `IORequest` pointer

The doorbell carries the address of a small, fixed 20-byte **descriptor**
that the driver's `BeginIO` builds from the real `IORequest`, not the
`IORequest` address itself.

**What this costs:** one extra translation step per request — the
driver copies `io_Command`/`io_Offset`/`io_Length`/`io_Data` into the
descriptor's layout. A handful of 68k instructions, immaterial next to
a host file I/O round trip.

**What it buys:**

- A clean 64-bit offset field with no `io_Actual`-doubles-as-high-32-bits
  contortion (the shape `copperhf`'s sketch resorts to for TD64/NSD).
  PFS3-DS and SFS address beyond 4 GB through TD64/NSD/direct-SCSI, so
  this is not optional (ADR 0003, `copperhf` §3).
- A host-side parser that never depends on which `exec.library`/
  `dos.library` version is running, or on `IORequest` struct-layout
  history.
- A protocol `machine-core` could unit-test completely before a single
  line of 68k driver code exists — exactly this increment's situation.

The descriptor is **read-only from the host's perspective**: results
never get written back into it. See §5.

## 3. The descriptor

20 bytes, big-endian, no assumed padding (the host reads it byte by
byte, never transmutes):

| Offset | Size | Field | Notes |
|---|---|---|---|
| `0x00` | 1 | `command` | `READ` = 1, `WRITE` = 2, `FLUSH` = 3 |
| `0x01` | 1 | `unit` | 0-7 |
| `0x02` | 2 | reserved | must be zero; ignored by this implementation |
| `0x04` | 4 | `length` | bytes; nonzero multiple of 512; ignored for `FLUSH` |
| `0x08` | 8 | `offset` | byte offset into the unit; multiple of 512; ignored for `FLUSH` |
| `0x10` | 4 | `buffer` | guest RAM address of the transfer buffer; ignored for `FLUSH` |

The driver allocates this in guest RAM (chip RAM only for now -- this
card's own bounds check has not yet been updated to also accept fast
RAM, §12), fills it in, and writes its address to `DOORBELL`.

## 4. Register map

Every single-byte register lives in the low-order byte of its
4-byte-aligned slot (the other three lanes are reserved), the same
"one hot byte per slot" convention `mirage.rs` uses. Anything not
listed below reads `0` and discards writes.

| Offset | Register | Width | Access | Notes |
|---|---|---|---|---|
| `0x00` | `UNIT_SELECT` | byte | W | selects the unit the discovery registers below describe; does not affect an in-flight transfer, which carries its own unit in the descriptor |
| `0x04` | `UNIT_PRESENT` | byte | R | `1` if the selected unit is attached |
| `0x08` | `UNIT_WRITE_PROTECT` | byte | R | `1` if write-protected; `0` for an absent unit |
| `0x0C` | `UNIT_CHANGE_COUNT` | u32 | R | media-change counter; `0` for an absent unit |
| `0x10` | `UNIT_SECTORS_HI` | u32 | R | high 32 bits of `sector_count()` |
| `0x14` | `UNIT_SECTORS_LO` | u32 | R | low 32 bits of `sector_count()` |
| `0x18` | `DOORBELL` | u32 | W | address of a descriptor (§3); submits on the write that completes the low-order byte |
| `0x1C` | `SUBMIT_OVERFLOW` | u32 | R | free-running count of doorbell writes dropped because the submission ring was full |
| `0x20` | `COMPLETION_PTR` | u32 | R | head-of-queue descriptor address, or `0` if empty |
| `0x24` | `COMPLETION_ERROR` | byte | R | head entry's error code (§6) |
| `0x28` | `COMPLETION_ACTUAL` | u32 | R | head entry's residue (bytes actually transferred) |
| `0x2C` | `COMPLETION_ADVANCE` | byte | W | any write pops the head entry, revealing the next one |
| `0x30` | `COMPLETION_COUNT` | u32 | R | entries currently queued (convenience; polling `COMPLETION_PTR != 0` is sufficient) |
| `0x34` | `INT_ENABLE` | byte | RW | `1` raises INT2 while the completion queue is non-empty; `0` (reset value) masks it |
| `0x38` | `SUBMIT_CAPACITY` | u32 | R | depth of the submission ring, in descriptors — **read this, do not hardcode it** (§5) |
| `0x3C` | `VERSION` | u32 | R | protocol version; `1` for this document. A driver should refuse a version it does not know |

Discovery registers (`UNIT_SELECT` through `UNIT_SECTORS_LO`) are
synchronous — no doorbell round trip, since they only read host-side
struct fields. A boot ROM's RDB mounter can walk all 8 units this way
before issuing a single `READ`.

## 5. Result delivery: the completion queue is the only source of truth

Unlike `copperhf`'s sketch (`io_Actual`/`io_Error` written back into the
guest's own `IORequest`), results here **never touch guest memory**.
They ride the completion queue as a `(pointer, error, actual)` triple
the driver reads through registers.

Why: writing into the guest's descriptor would mean writing to an
address this module has not necessarily validated — a bad descriptor
pointer must never become a write target — and would give the driver
two disagreeing sources of truth (registers and guest memory) for the
same result. One source is simpler to reason about and to test.

Driver-side interrupt server shape:

```
while (COMPLETION_PTR != 0) {
    struct IORequest *req = (struct IORequest *)COMPLETION_PTR;
    req->io_Error  = COMPLETION_ERROR;
    req->io_Actual = COMPLETION_ACTUAL;
    write(COMPLETION_ADVANCE, 0);   // pop, reveal next
    ReplyMsg(&req->io_Message);
}
```

`0` is never a legitimate descriptor address in practice (`AllocMem`
never returns it), so it doubles as this device's own "queue empty"
sentinel — `COMPLETION_COUNT` exists only for convenience/diagnostics.

## 6. Error codes and residue

`COMPLETION_ERROR`, one byte:

| Value | Name | Meaning |
|---|---|---|
| 0 | `OK` | success |
| 1 | `BAD_UNIT` | unit out of range or not attached |
| 2 | `WRITE_PROTECTED` | `WRITE` against a write-protected unit |
| 3 | `INVALID_COMMAND` | not `READ`/`WRITE`/`FLUSH` |
| 4 | `INVALID_LENGTH` | zero, or not a multiple of 512 |
| 5 | `MISALIGNED` | `offset` not a multiple of 512 |
| 6 | `OUT_OF_RANGE` | transfer would run past the unit's `sector_count()` |
| 7 | `BAD_ADDRESS` | the descriptor pointer, or the buffer it names, does not lie entirely inside guest RAM, or the descriptor pointer is misaligned |
| 8 | `IO_ERROR` | the backing store failed partway through |

`COMPLETION_ACTUAL` is the residue: whole sectors that landed before
any failure (`0` for every rejection above `OK` except `IO_ERROR`,
which reports real partial progress). A driver can set `io_Actual`
honestly rather than guessing — the gap the MIRAGE design review found
missing from that card's spec.

Every rejection above still produces a completion on a later tick,
carrying an error code — never a hang, never a panic, and the
descriptor pointer, buffer address, and unit number are all treated as
hostile guest input (bounds/alignment checked with `checked_add` before
any device I/O or memory access).

## 7. Deferred completion: the doorbell write does no I/O

Accepting a doorbell write must not complete the request in the same
bus access, per ADR 0003: a slow host disk must not stall the machine,
and a future browser-hosted (OPFS) backend cannot satisfy a read
synchronously at all. `DOORBELL`'s write handler only pushes a raw
pointer onto an in-host submission ring — it does not even read the
descriptor yet. All real work (reading the descriptor, validating it,
moving data, calling the `BlockDevice`) happens in `Hostblk::tick`,
called once per `MachineBus::tick`, which executes **at most one**
submission per call. With `N` requests queued, the `N`th completes on
the `N`th tick at the earliest.

INT2 is checked on both the read and the write path in `lib.rs`'s
routing (mirroring `gayle.rs`'s hard-won lesson: a device whose state
can change without a preceding write must still be checked on reads, or
an interrupt can be silently missed and a multi-request pipeline stalls
until an unrelated interrupt rescues it).

## 8. Queue depths and overflow

Two independent fixed-size rings (no allocator in `machine-core`):

- **Submission ring**, depth reported by `SUBMIT_CAPACITY` (8 in this
  implementation). A doorbell write when full is **dropped**: not
  queued, never executed, no completion produced for it. The driver
  observes this via `SUBMIT_OVERFLOW` incrementing with no matching
  completion, and must not exceed the depth — the same discipline a
  real hardware submission ring demands (the same shape virtio's
  ring-full behaviour takes). Recovery: the driver still holds the
  `IORequest` it never handed off, and can retry the doorbell write once
  earlier requests free a slot.

  **Read `SUBMIT_CAPACITY`; do not hardcode 8.** Because an overrun is
  dropped silently rather than rejected, tracking outstanding requests
  against this depth is the *only* thing standing between a driver and
  lost I/O — which makes the number load-bearing, not informational. A
  bare-metal board with memory to spare may offer a deeper ring, and a
  driver that baked in today's value would under-use it; against a
  shallower one it would lose requests. A unit test asserts the
  advertised depth is exactly where the ring really starts dropping,
  checking the two against each other rather than both against the same
  constant.

- **Completion ring**, 8 entries (`COMPLETION_QUEUE_CAPACITY`).
  **Nothing is ever dropped here.** `tick()` checks the completion
  queue has room *before* popping the next submission; if it doesn't,
  nothing advances that tick. A stalled driver (one that stops draining
  completions) stalls the whole engine after 8 unread completions,
  rather than silently losing one. This is deliberate: a hung pipeline
  is a bug that shows up (no forward progress, easy to reproduce); a
  leaked completion is a bug that doesn't (a task blocked forever on a
  `ReplyMsg` that never comes).

A driver should size its own bookkeeping (how many `IORequest`s it
allows outstanding at once) to stay within `SUBMIT_QUEUE_CAPACITY`,
and drain completions promptly rather than relying on the backpressure
above as a scheduling strategy.

## 9. The transfer engine: whole descriptor per tick

`mirage.rs` advances one sector per tick because its `DATA` register is
a real per-byte FIFO the driver drains by hand. There is no such
register here — the guest never touches a data port at all, which is
the entire point of this card — so `Hostblk::tick` performs one whole
descriptor's transfer, however many sectors long, in a single call.
The guest cannot observe how many ticks a transfer took, only that it
was never zero, so nothing is gained by spreading it across more.

**Not literally zero-copy at the byte level.** `BlockDevice::read_sector`/
`write_sector` take a `&mut [u8; 512]`/`&[u8; 512]` — a fixed-size
array, not an arbitrary slice — because `hostblk` reuses that trait
unchanged (so `machine-hosted`'s `FileBlockDevice` needs no
modification). A guest RAM slice at an arbitrary byte offset cannot be
reborrowed as a fixed-size array in safe Rust, so each sector passes
through one 512-byte stack buffer between the `BlockDevice` call and
guest RAM. This is "direct to guest memory" in the sense ADR 0003
cares about — no CPU-visible register, no per-byte guest bus access,
the guest's own code never moves a byte of payload — not in the sense
of a single memcpy with no intermediate buffer anywhere in the host's
call stack.

## 10. Write completion is not durability

`FLUSH` is a real doorbell-to-completion round trip, but `BlockDevice`
has no flush method to call underneath it (same gap `mirage.rs`
documents), so there is nothing yet to actually order or durably
commit. A host-side write cache would need a real flush added to the
trait; not built speculatively here.

## 11. Removable media

Attach/detach and hot media-change are host-side operations in this
increment, not guest-visible commands. `Hostblk::notify_media_change`
is the hook a board layer calls in place of a guest-triggered eject; it
bumps the unit's change counter, which `UNIT_CHANGE_COUNT` exposes for
`TD_CHANGENUM`-style polling. There is no `TD_ADDCHANGEINT` mechanism
yet — that is driver/interrupt-server work for when the driver exists.

## 12. What this increment does not include

- No m68k driver, no DiagArea boot ROM, no RDB mounter. This card
  cannot boot a machine unaided yet.
- No `TD_ADDCHANGEINT`/change-interrupt delivery to the guest — the
  change counter exists, nothing wakes a waiting task on it yet.
- No write caching, hence no real flush underneath `FLUSH` (§10).

Descriptor and buffer addresses are **no longer restricted to chip
RAM**: the card asks `MachineBus` through `GuestMemory::ram_slice`/
`ram_slice_mut`, which resolves chip RAM and any attached fast RAM
without either range being hardcoded here. A driver may allocate its
buffers `MEMF_FAST`. A span straddling two regions is refused rather
than silently stitched together.

## 13. Acceptance testing the driver

When the driver exists it is tested with **devsoak** (`~/src/devsoak`),
a destructive correctness and soak tester for trackdisk-style AmigaOS
block devices. It runs against known-good drivers — `scsi.device`,
`trackdisk.device`, `lide.device`, `uaehf.device` — so our results are
comparable rather than self-referential, the same discipline the
Copperline blitter differential uses.

Two things it exercises bear directly on the design above.

**Concurrency against the submission ring.** devsoak drives overlapping
reads, writes and housekeeping from several tasks with multiple requests
in flight, for hours. That is precisely what overruns §5's submission
ring, whose depth is `SUBMIT_CAPACITY` and whose overflow behaviour is
to *drop* the doorbell write with no completion ever produced.

This gives a sharp pass criterion. devsoak's quirks database has a
`maxinflight N` action for drivers that cannot cope with unbounded
concurrency. **Our driver should not need one.** If it does, that is not
a devsoak quirk to record — it means the driver is failing to
self-limit against `SUBMIT_CAPACITY`, and the bug is ours.

**The 64-bit dialects.** devsoak follows devtest's convention of the
offset high word in `io_Actual` for TD64/NSD, and tests stale change
counts and unsupported commands. The descriptor deliberately avoids
that overloading on the wire (§2), but the driver still has to *accept*
it from an `IORequest` and translate — so the convention has to be
handled at the boundary rather than designed away.

**Getting it.** A built binary is on Aminet at
`dev/misc/devsoak`, so running it needs no 68k toolchain — only building
it from source does, and that wants bebbo's amiga-gcc, which this
project already uses for the boot ROM. It is BSD 2-Clause, so unlike the
Kickstart ROM and the Workbench image it can simply be installed into
the test HDF (`tools/amibake/m68k-machine.toml`) rather than kept in
`nondistribution/`.

That makes the acceptance run self-contained: boot this machine with
`--hostblk` attached and run devsoak against the driver in-guest, with
no host-side orchestration and nothing to fetch at test time.
