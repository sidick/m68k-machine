# ADR 0003 — Native block storage is a doorbell card, not a PIO one

**Status:** accepted, 2026-09-05. Decided rather than deferred, because
the alternative was already being built and every week of delay adds
driver code written against the wrong interface.

**Context:** proposal §9, §10.3; `device-ledger.md`;
`~/src/project-ideas/pending/mirage-hdf-card-proposal.md`;
`~/src/project-ideas/active/copperhf-device-design-note.md`.

---

## The question

The device ledger's native-first rule says storage should stop being
Gayle IDE and become a device designed for this machine. Proposal §10.3
names MIRAGE as that device, and its §4.1 block plane has been
implemented (`mirage.rs`).

What shape should the boot path's storage device actually take?

## Why MIRAGE turned out to be the wrong first answer

MIRAGE is a good design for the machine it was drawn for. That machine
is not this one.

**Its constraints come from hardware this machine does not have.**
§4.6's throughput table — roughly 1 to 1.5 MB/s on a stock 68000, 2.5 to
3.2 MB/s accelerated — is the Zorro II bus ceiling, and the sector-
granular `DRQ` handshaking, deferred completion and deep write buffering
all exist because a real MIRAGE card is an MCU behind an SD stack on a
3.58 MB/s bus. Here the card is a Rust struct and the data path is a
memcpy at host memory bandwidth. There is no bus.

Adopting that interface would import a physical constraint for no
benefit. That is the mirror image of the drift the device ledger exists
to catch: there, legacy silicon was emulated for convenience; here, we
would be emulating hardware *limitations* for spec fidelity.

**The ceiling is already documented.** The `copperhf` design note states
it: the host copies straight into guest RAM, the only 68k that runs is a
small stub plus the filesystem, and at that point the bottleneck is FFS
itself — the floor for any design. A PIO transfer loop cannot beat that;
it can only approach it.

**MIRAGE concedes the point.** Its §6.2: *"with `CAP_DMA` set, MIRAGE in
fast mode is that mechanism, wearing a hardware-shaped interface."* So
the fast path is the DMA path — and the DMA path is exactly what §4.1
does not specify. A design review found no address register, no
doorbell, no completion or error semantics for it; the only doorbell in
the proposal lives in the deferred management plane. `mirage.rs`
implements the slow path because it is the only one the spec defines.

## Decision

**The boot path's storage device is a doorbell-plus-descriptor block
card with a host-side data path and INT2 completion.** Working name
`hostblk`.

The guest writes a descriptor — or an `IORequest` pointer — to a
doorbell register. The host performs the whole transfer directly against
guest memory and raises INT2 on completion; the driver's interrupt
server replies the request. Completion is asynchronous, because a slow
host disk must not stall the machine and a future WASM or browser host
cannot do synchronous file I/O at all.

**MIRAGE is retained, not discarded.** `mirage.rs` stays as the
specification's reference implementation and as a PIO fallback. It is no
longer the boot path, and Gayle's successor in the ledger becomes
`hostblk`.

## Why this is not an emulator convenience

The distinction matters, because the ledger's whole purpose is to stop
convenience dressing itself as design.

For a real MIRAGE board, bus-master DMA is a genuine engineering
problem — Zorro III bus mastering, Buster revisions, the lot — which is
why §4.5 makes it an optional capability bit.

For *this* machine it is not a shortcut but the honest description of
the hardware. The host really can write into guest RAM: today it is a
memcpy, and on Phase 5 hardware it is NVMe or MMC moving data by DMA.
Zorro-style PIO would be the artificial part, simulating a bottleneck
that exists nowhere in the stack.

## What this costs

- **Two block interfaces to carry.** `mirage.rs` is about 1,100 lines
  including tests and is not on the boot path. Kept deliberately; see
  the ledger.
- **MIRAGE's second-host test-bed role is deferred.** §10.3 argued this
  machine would be MIRAGE's continuous conformance oracle before
  hardware exists. That was a real strategic benefit and this decision
  gives it up for now. The block plane still runs in CI, so the loss is
  of *driver-level* conformance, not of the register model.
- **A second driver eventually.** If a real MIRAGE board is ever built,
  its PIO driver is separate work.

## What it does not cost

- **Image portability is unaffected.** An HDF is an HDF. Only the
  transport differs, so "the same disk works here and on real hardware"
  survives intact.
- **Less 68k code, not more.** A doorbell `BeginIO` writes a pointer and
  returns. A PIO driver needs a transfer loop, stall handling,
  partial-transfer recovery and an abort path. This project has never
  shipped 68k code, and the simpler device is a considerably better
  first one.
- **No architectural novelty.** Proposal §9 already specifies a
  "minimal doorbell + descriptor protocol" for the non-block cards (AHI,
  host-fs, RTC). This extends an existing pattern rather than inventing
  one.
- **MIRAGE's genuinely differentiating half stays available.** Its
  management plane — creating, attaching and detaching HDFs from
  Workbench — is orthogonal to the data path and can layer on later.
  That, not the transfer mechanism, is what made the proposal
  interesting.

## Consequences for the driver

The driver is a small exec device plus a boot-time RDB mounter in the
board's DiagArea ROM, so nothing has to be installed on the guest. It
must still cover the AmigaOS command surface that `copperhf`'s §3
enumerates — TD64, NSD, and direct-SCSI `HD_SCSICMD`, since PFS3-DS and
SFS address beyond 4 GB through it — and removable-media semantics
including `TD_ADDCHANGEINT`.

It must be licensed **MIT**, not merely under this repository's dual
MIT/Apache-2.0, so it stays usable by MIRAGE hardware and by Copperline
plugins per proposal §16.

## Revisiting

Reopen this if a real MIRAGE board is built and driver-level parity
between it and this machine becomes worth more than boot-path
throughput. The block plane is already here, so that reversal is
cheaper than it would otherwise be — which is the main reason for
keeping it.
