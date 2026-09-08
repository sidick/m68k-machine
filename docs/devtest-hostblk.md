# devtest against hostblk.device

devtest 1.9 (amibake's `devtest` recipe; Chris Hooper's block-device
tester, devsoak's named complement) run against `hostblk.device` unit 0
on 2026-09-08, via `tools/amibake/m68k-machine-devtest.toml`. One
category note up front: there is **no pktport run and cannot be one** —
devtest talks trackdisk-style commands at a block *device*, and the
pktport volume has no block layer at all; it is a handler on a
DosPacket transport (ADR 0004's "a disk" vs "a volume" made concrete
by a test tool). The handler-level equivalents are DiskSpeed
(`docs/pktport-measurement.md`) and pktvol's unit suite.

## Conformance map

**Pass:** `NSCMD_DEVICEQUERY`, `CMD_READ`/`CMD_WRITE`, `TD_READ64`/
`TD_WRITE64`, `NSCMD_TD_READ64`/`NSCMD_TD_WRITE64`, `TD_GETGEOMETRY`
(exactly right: 131072 × 512, C=1 H=1 S=131072), `TD_CHANGENUM`,
`TD_CHANGESTATE`, `TD_PROTSTATUS`, `CMD_UPDATE`, `CMD_CLEAR`, the
destructive integrity test, and the read/write benchmarks. The whole
modern surface — consistent with devsoak's earlier acceptance
(`docs/hostblk-soak.md`), now confirmed by the complement tool.

**Refused with `IOERR_NOCMD` (correct for a virtual disk):**
`HD_SCSICMD` and the SCSI inquiry path (no SCSI to pass through — this
is also why `devtest -p` prints "Unknown device type"), `ETD_*`
(no enhanced/label semantics), `TD_RAWREAD`, `TD_MOTOR`,
`TD_GETDRIVETYPE`, `TD_GETNUMTRACKS`, `CMD_START`, `TD_SEEK`.

**`TD_FORMAT`: fixed.** Was refused with `IOERR_NOCMD`; now routed
through the exact same path as `CMD_WRITE` (dos's own "write without
preserving" contract -- on a hard-disk-class device TD_FORMAT *is* a
write, per `m68k/hostblk-rom/hostblk-diagrom.s`'s `dev_beginio`/
`classify_async`). `TD_FORMAT64`/`NSCMD_TD_FORMAT64` came along for
free, riding the existing `TD_WRITE64`/`NSCMD_TD_WRITE64` 64-bit-offset
plumbing (`.wr64` in `classify_async`) -- three extra `cmp.w`/`beq`
pairs, no new code path. `NSCMD_DEVICEQUERY`'s `SupportedCmds` table
now advertises all three truthfully. Confirmed by a real devtest run
against the rebuilt ROM (`docs/devtest-hostblk.md`'s own operational
notes): `TD_FORMAT`, `TD_FORMAT64` and `NSCMD_TD_FORMAT64` all report
`Success`, `ETD_FORMAT`/`NSCMD_ETD_FORMAT64` still correctly refuse
(extended-command variants, deliberately unimplemented, same as every
other `ETD_*`/`NSCMD_ETD_*` entry), and the rest of the conformance map
above reproduces unchanged. The filesystem-level half of the same gap
-- `ACTION_FORMAT`/`ACTION_INHIBIT` in the `pktport` stack, for
`C:Format`'s QUICK path over a served volume -- is `docs/pktport-
protocol.md` §5's own addendum.

## Throughput

Raw device, Zorro III RAM buffers, guest-measured:

| transfer size | read | write |
|---|---:|---:|
| 512 KB | ~9.7 GB/s | ~9.6 GB/s |
| 128 KB | ~2.2 GB/s | ~2.2 GB/s |
| 32 KB | ~374 MB/s | ~369 MB/s |

Against DiskSpeed's ~223 MB/s *filesystem-level* ceiling on the same
device (`docs/pktport-measurement.md`), this is the cleanest possible
demonstration that the block layer was never the bottleneck — the gap
between ~10 GB/s raw and ~223 MB/s through FFS is the guest-executed
filesystem, which is ADR 0004's whole premise measured from a third
angle.

## The butterfly failures: strictness, not breakage

`CMD_READ butterfly average/far/constant` all fail with
`IOERR_BADADDRESS`. Traced to the descriptors: devtest's butterfly
strides are `device_size / iterations`, which is rarely a
sector multiple, so it issues **byte-unaligned offsets** (observed:
8220828, 66437744, 32883312 — mod 512 = 156, 112, 112). The card
refuses them as `MISALIGNED` per `hostblk-protocol.md` §6 and the
driver maps that to `IOERR_BADADDRESS`, exactly as documented.

Classic trackdisk semantics say `io_Offset` must be sector-aligned;
devtest is written by the a4091 driver's maintainer and encodes that
driver's lenient behaviour (it accepts and internally handles
unaligned I/O). So this is a **policy divergence between two defensible
positions**, not a defect: strict refusal (ours, per the classic
contract, with an explicit error) versus leniency (a4091's, kinder to
sloppy callers). Left strict deliberately; if real software ever turns
up sending unaligned block I/O, the decision gets revisited with that
software as the evidence, and the fix would likely be driver-side
bounce-buffering rather than making the card protocol byte-granular.

## Operational notes

- devtest exits non-zero when any test fails, and `S:AmiBake-Startup`
  runs at the shell's default `FailAt 10` — so a manifest `[[run]]`
  sequence *stops at the first devtest with a failing subtest*. The
  runs after `-bb` were driven via `--input-script` with `FailAt 21`
  instead. An amibake `[[run]] failat = N` knob would make this class
  of tool fully manifest-drivable.
- The destructive runs (`-bdy`, `-t -d -y`) target the *boot device*.
  Run them on a copy, last, after all output has reached SER: — one
  master image was destroyed learning this (the tool did exactly what
  its warnings say; the process error was booting the master).
