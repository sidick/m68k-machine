# ADR 0004 — Filesystems run on the host, behind a packet transport

**Status:** accepted, 2026-09-06.

**Context:** ADR-0003 (which named the ceiling this removes);
`docs/storage.md` ("Booting AROS from an AROS-built image");
`~/src/emulator_disk_speed/REPORT.md` (Copperline storage-backend
matrix, the measurements that motivated this);
[AmiPart](https://github.com/ChuckyGang/AmiPart) (MIT — a C RDB
partition editor with filesystem-driver block support that builds both
as a native AmigaOS tool and as a Linux CLI over hdf files: a readable
reference for the RDB/FSHD layer, and same-code-many-hosts proven in
C. Its permissive licensing means `amiga-rdb` can *translate* its
working C — `rdb.c`, `rdbbe.h`'s big-endian handling — rather than
clean-room from the spec, and its reach goes beyond RDB: `ffsresize`/
`pfsresize`/`sfsresize` cannot exist without real knowledge of each
filesystem's internals, so it is a permissive reference for parts of
`amiga-ffs` too); NetBSD (BSD — carries RDB structures in its disk
subsystem and `adosfs`, a read-only AmigaDOS filesystem in C: a second
translatable reference for the read path, independent of AmiPart).

---

## The question

ADR-0003 ended at a known ceiling: with `hostblk` completing requests at
host speed, "the bottleneck is FFS itself — the floor for any design."
The Copperline matrix puts numbers on that floor. Its `copperhf` backend
is `hostblk`'s structural twin — doorbell, INT2, no timing model, real
hardfile — and scores ~107 MB/s and 249 file-creates/s, while its HOSTFS
directory passthrough scores ~421 MB/s and 909 file-creates/s *on the
same host file*. The entire 4x gap is the filesystem executing as
emulated 68k code: bitmap allocation, hash-chain walks, 512-byte
chunking, all interpreted.

Can that layer move to the host without giving up real, portable,
image-backed volumes?

## Decision

**Add a packet-transport card, and put filesystems behind it as host
code.** The natural cut is `dos.library`'s DosPacket interface — the
seam AmigaDOS itself defines for a filesystem. The guest runs a thin
68k handler stub that forwards packets (`ACTION_FINDINPUT`,
`ACTION_READ`, `ACTION_EXAMINE_NEXT`, ...) over a doorbell card; the
host answers them. The transport does not care what answers the
packets, which makes backends interchangeable behind one card:

1. **Host-implemented FFS against an image file — built first.** The
   on-disk format stays standard FFS, so images remain interchangeable
   with xdftool, WinUAE, real machines, and everything amibake already
   builds: existing hdfs mount through it unchanged and get the
   speedup for free. Only the *execution* moves to the host.
2. **Host directory passthrough — later, same transport.** True
   HOSTFS-style live-directory export. Deferred because host-filesystem
   semantics leak through it (name limits, case handling, metadata
   sidecars) and backend 1 delivers the same win without them.

`hostblk` is not displaced. It remains what RDB fidelity, devsoak,
whole-image work, and booting-as-a-real-disk live on. Ledger terms:
both are permanent native devices — one is "a disk", the other is
"a volume".

## The filesystem crates

**One filesystem family per crate — and RDB is not a filesystem, so it
gets its own.** `amiga-rdb` handles the layer beside the filesystems:
RDB, PART, FSHD and LSEG blocks, checksums, DosEnvecs. In: a
`BlockSource` for the whole disk. Out: partitions as extents plus
metadata, and filesystem-driver payloads. Composition with a filesystem
crate is a small adapter — a partition becomes a `BlockSource` that
offsets LBAs into the parent — and neither crate depends on the other;
consumers stack them. This is what lets an external consumer handle
both bare HDFs (one crate) and the far more common RDB-wrapped ones
(both). It also has a customer inside this project before any external
one: FSHD/LSEG *writing* is where the AROS DOS\7 fix lives if the
answer is shipping a long-name filesystem in the RDB, and a host-side
Rust reading of these structures is a third independent implementation
(after the boot ROM's 68k mounter and amitools) for the differential
suite. Small and completely specified, with AmiPart as an MIT reference
for exactly this layer — it comes first, before `amiga-ffs`, which
wants it anyway to reach the partitions inside real fixtures.

FFS and PFS3 share nothing on disk; a crate that pretended otherwise
would have no coherent API. `amiga-ffs` follows:

- Pure on-disk format logic. No DosPacket types, no transport, no
  machine-core dependency — blocks in, directories and files out.
  Everything Amiga-machine-shaped stays in the machine layer.
- Block access behind one trait (`BlockSource`: read/write a 512-byte
  block by LBA), so the same crate serves the handler backend, host
  tooling, and a test suite running against plain memory.
- `no_std` + `alloc`, `machine-core` discipline — nothing in FFS needs
  `std`, and the bare-metal boards should not be locked out for no
  reason.
- MIT/Apache-2.0 dual, workspace member first, own repo when stable.
  There is no permissively-licensed writable FFS in Rust; amitools is
  GPL-2, pfs3aio is BSD-4 68k C. The reuse claim is real.
- All eight DOS\0-DOS\7 variants readable from the start. The AROS
  image dead-end (`docs/storage.md`) was a DOS\7 volume nothing in the
  guest ROM could read; a DOS\3-only crate rebuilds that wall.

**Three entry points, staged by risk.** *Mount existing* and
*create-from-tree* first; *mutate in place* last. Creation is write-path
code with nothing to corrupt — build a fresh image from a host tree,
read it straight back, diff the trees — so it is both the safest write
code to ship first and the best test vector for the read path. It is
also the API external consumers actually want: Copperline creates
OFS/FFS drives dynamically from directories today, and amibake's
dir-to-hdf build is the same operation again. In-place mutation lands
only behind a differential suite: mutate through guest packets, verify
from outside with xdftool (GPL: run as oracle, never copy).
`nondistribution/aros/aros.hdf` (via `tools/amibake/aros.toml`) is a
fully redistributable DOS\7 fixture, so this suite can run in CI where
no licensed image can.

## Alternatives rejected

- **Keep filesystems in the guest, optimise the CPU.** JIT and batch
  execution help wall-clock, but DiskSpeed measures guest-clock work:
  the FFS cost is architectural, not a speed knob.
- **True HOSTFS only.** Fastest path in the matrix, but imports the
  host filesystem's semantics: 30/107-char limits vs host names, case
  handling that differs between APFS and ext4, metadata in `.uaem`-style
  sidecars. Backend 1 keeps Amiga semantics native. Still worth having
  later — hence the shared transport — for the live-edit dev loop.
- **PFS3 format first.** Better filesystem, BSD-4 source available as a
  real reference, and a legitimate future `amiga-pfs3` crate. But it
  forfeits interchange with everything amibake builds today, and it is
  a larger implementation. It becomes attractive when FFS-format
  metadata rates are the measured ceiling, not before.
- **Our own format.** Maximum freedom, zero interchange, all-new
  tooling, and nothing above requires it.

## Consequences

- The DOS\7 problem dissolves for handler-served volumes: the guest's
  ROM filesystem never touches them, so AROS can mount long-name
  volumes regardless of what its 3.1-era ROM understands — provided the
  handler stub itself runs under AROS.
- Boot is sequenced, not solved: first mounted from a `hostblk`-booted
  system (handler in `L:`, mountlist entry), bootable-in-its-own-right
  later (a DiagArea that produces a BootNode whose handler comes from
  card ROM is its own design).
- The handler stub is the largest 68k program the project will have
  shipped — a real process with a message loop, not a DiagArea stub.
- This is the largest single piece of new engineering since `hostblk`
  itself; the read-only staging exists to keep the risk in one place.
- Expected result, to be *measured* against the Copperline matrix with
  DiskSpeed 4.2 under the same protocol (`DIR SEEK FAST LONG`, median
  of 3): near the HOSTFS end of the range, with the largest gains in
  file-create and seek rates. If the measured number lands near
  copperhf's instead, the premise of this ADR is wrong and it should be
  revisited rather than defended.
