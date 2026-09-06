# Storage: attaching a disk to `hostblk`

`crates/machine-core/src/hostblk.rs` implements this machine's own
storage device: a doorbell-plus-descriptor Zorro III block card (ADR
0003) with a host-side transfer engine, our own m68k driver, and a
DiagArea boot ROM that mounts an RDB from it. It is the machine's
**permanent** storage path (`docs/device-ledger.md`), not a bring-up
stand-in — the register interface, the m68k driver and the boot ROM are
all designed for this machine rather than borrowed from a piece of
silicon Kickstart already knew how to drive.

**This document used to describe Gayle IDE**, the bring-up device
`hostblk` replaced. Gayle presented the A1200's real IDE register map so
`scsi.device` could drive a disk with no m68k code of ours; it worked,
but it was always scaffolding with a named successor
(`docs/device-ledger.md`). `hostblk` met its retirement criterion —
booting the identical image unaided, byte-identical to the Gayle boot —
and was soaked against devsoak (`docs/hostblk-soak.md`: `RESULT PASS`, 0
errors, clean audits over 16,384 sectors, all three dialects) before
Gayle was actually removed. Gayle's row moved to `device-ledger.md`'s
Retired section rather than being deleted; see that file for the full
account of what removing it touched.

This document covers the host side: how a disk image gets attached to
`hostblk` in `machine-hosted`, how the one image this has actually been
tested against was built, and exactly how far a real boot gets with it
today.

## Attaching an image

```
machine-hosted --rom <kickstart.rom> --hostblk <path-to-image.hdf>
```

`--hostblk` takes a path to a raw, sector-addressable disk image — an
`.hdf` (a bare RDB-partitioned image, no ADF/DMS-style wrapper) is
exactly this shape. Bytes are read and written to the file exactly as
they sit on disk, sector by sector, with no transformation:
`hostblk`'s transfer engine moves guest RAM to and from the backing
store directly, with no byte-swapping data path to reverse (unlike
Gayle's PIO register, which swapped bytes on the way through and relied
on that swap cancelling out for opaque sector data — see
`crates/machine-hosted/src/hd_image.rs`'s module doc comment for that
history). The host-side `BlockDevice` implementation
(`crates/machine-hosted/src/hd_image.rs`, `FileBlockDevice`) is the same
type that served Gayle; it works unchanged behind `hostblk` because both
are built against `machine_core::block::BlockDevice`. `sector_count` is
the image's true size, truncated down from its byte length; an
out-of-range LBA (or, for writes, a read-only image — see below) is
reported to the guest as a clean error rather than a host-side failure.

Omitting `--hostblk` leaves this machine with no drive attached at all.

### Read-only by default

**`--hostblk` opens the image read-only unless `--hostblk-writable` is
also passed.** This was a deliberate call, not an oversight: the images
worth attaching to it are not throwaway. The one this document is about
is a licensed-media conversion that took real build effort and, per its
own licensing (next section), cannot simply be re-downloaded if a bug in
an early write path corrupts it. A full boot to a real Workbench screen
(confirmed below) never needs to write a single sector. Write access is
one flag away — `--hostblk-writable` — for whoever actually needs
Kickstart's or Workbench's write path exercised, at which point the risk
is a conscious choice rather than a default.

**A silent-failure mode this default enables, seen in the field:** a
guest startup script whose command line needs a write to the boot volume
— `SYS:AmiPilotServer SERIAL >SYS:pilot.log` in `S:User-Startup` was the
real case — dies before the program ever runs, because the shell cannot
open the redirect file on a read-only volume. Nothing on the host side
says so, the rest of the boot proceeds normally, and any *stale* log
from an earlier writable session still sitting in the image reads as
proof the program started. Debugging that cost a full serial-stack
investigation before the actual cause surfaced (the whole serial path
was healthy; the guest program simply was not running). If a `--hostblk`
guest is expected to *do* something at boot and doesn't, check the
runner's `hostblk: unit 0 = … (read-only)` startup line before
suspecting the machinery — and treat in-image log files as evidence only
when the run that should have written them was a writable one.
`SERIAL_REG_TRACE=1` (a diagnostic env var on `machine-hosted`) traces
every serial-register access with the guest PC and settles in one run
whether the guest is touching the serial hardware at all.

## The test image

`nondistribution/m68k-machine.hdf` is a bootable AmigaOS 3.2.2 HDF: one
RDB, one bootable `DH0:` FFS partition. It was built with
[amibake](../tools/amibake/m68k-machine.toml) from licensed Amiga OS
media:

```
~/src/amibake/.venv/bin/amibake build tools/amibake/m68k-machine.toml
```

`tools/amibake/m68k-machine.toml`'s machine block mirrors what this
machine actually presents to the guest — 68040 with FPU, 2 MB chip RAM,
256 MB fast RAM by default (proposal §13), no RTG — and includes the
`mmulibs` package so `68040.library` is present (without it, a stock
3.2.2 Startup-Sequence nags every boot about a missing CPU support
library on a machine that reports a 68040). It emits both an `hdf` (what
`hostblk` serves) and a `copperline` config, so the identical image can
be booted under the [Copperline](https://codeberg.org/copperline)
oracle for comparison.

**Licensing: this image contains licensed AmigaOS 3.2.2 and is treated
exactly like the Kickstart ROM this project already depends on** — never
committed to this repo, never redistributed. `.gitignore` does not
currently exclude an amibake output directory — it only covers
`target/`, `*.swp` and `.DS_Store`. Build amibake's output somewhere
outside this repo (a scratch directory, not a path under this checkout)
rather than relying on `.gitignore` to catch it if built in-tree; this
is worth a follow-up `.gitignore` entry if amibake output is ever built
inside the repo as a matter of course. Anyone reproducing this needs
their own licensed AmigaOS 3.2.2 media and their own amibake checkout;
the tests in `crates/machine-hosted/tests/real_rom.rs` that exercise it
skip cleanly (print a message, assert nothing) when the image is not
present at its configured path, the same convention every other
real-ROM/real-image test in that file already uses.

## How far it gets, with evidence

Run:

```
cargo build -p machine-hosted --release
./target/release/machine-hosted --rom <A1200 Kickstart 47.115> \
  --hostblk <the HDF above> \
  --screenshot /tmp/wb.png --screenshot-frame 4000 --max-frames 4500 --inspect
```

**It boots all the way to a live Workbench screen, unaided — no Gayle
attached.** In order:

1. **Card discovered and disk found.** `hostblk`'s DiagArea boot ROM
   (`m68k/hostblk-rom/`) proves Kickstart runs code from the card and
   that code can reach the card's own registers; `--inspect`'s `hostblk
   state:` section and the runner's own `hostblk: unit 0 = <path>
   (18432 sectors, read-only)` startup line confirm the device is
   attached and its true size (18432 x 512 B = 9 MB) is reported.
2. **The RDB is read and `DH0:` mounts.** `--inspect`'s resident-module
   report at the end of a run shows `filesystem` (`fs 47.4`) and
   `FileSystem.resource` initialised, and DOS getting far enough to run
   a startup sequence (next point) is only possible once the partition
   mounted.
3. **The filesystem loads and `Startup-Sequence` runs.** The resident
   list includes `con-handler`, `shell`, `system-startup`, and
   `ram-handler` all initialised — `system-startup` in particular is
   the module that runs `S/Startup-Sequence` from the booted volume.
4. **Workbench draws.** A screenshot taken at frame 4000 shows a grey
   Workbench desktop with a titled window and the `RAM Disk` and `SYS`
   icons drawn: 6 distinct colours, 13,507/433,152 non-background
   pixels — byte-identical to the equivalent Gayle capture taken before
   the switch. The RTG variant (`--graphics`) at frame 5000 is
   640x480, 4 distinct colours, 26,978/307,200 non-background pixels,
   likewise byte-identical to its Gayle counterpart; a Zorro III RTG
   card (`--graphics --graphics-bus 3`) reaches the identical capture
   too.

Frame timing, for anyone adjusting `--screenshot-frame`: `hostblk`
carries no equivalent of Gayle's ~30 second IDE probe timeout — a
correctly-answering card is found immediately, so the extra time past
the no-disk boot-screen test's own timing is the real disk-boot work
itself (RDB scan, filesystem load, `Startup-Sequence` execution, opening
a Workbench screen), not a probe delay. Sweeping `--screenshot-every`
found the desktop already fully drawn and pixel-stable well before frame
4000, so frame 4000 (with `--max-frames 4500` for margin past the
capture boundary) is what the regression tests below use.

## Regression tests

`crates/machine-hosted/tests/real_rom.rs`'s
`kickstart_3_2_2_a1200_boots_from_hd_to_the_workbench_desktop` and
`kickstart_3_2_2_a1200_rtg_workbench_desktop_is_grey_not_blank_or_corrupt`
drive the runs above (planar and RTG) and assert on the captured
screenshots: the runner reports attaching the image read-only, the
capture actually fires at its target frame, the canvas is the expected
size, a floor of non-background pixels is drawn, and several distinct
colours are present. Both were originally written against `--hd`
(Gayle) and switched to `--hostblk` once Gayle retired — only the attach
flag changed; the pixel/colour assertions are unchanged and still pass
against the exact same test image. Like every other test in that file
that depends on a real Kickstart ROM or a real disk image, both are
`#[ignore]`d and skip cleanly — printing which file is missing rather
than failing — when the ROM or the image is not present at its
configured path; run explicitly with
`cargo test -p machine-hosted -- --ignored kickstart_3_2_2_a1200`.

`crates/machine-hosted/src/hd_image.rs` also carries its own fast,
unconditional unit tests (sector count from file length, a trailing
partial sector not being addressable, read-only refusing writes, a
writable round trip, and a clean out-of-range failure) that run under a
plain `cargo test -p machine-hosted` with no image required.

## `hostblk`'s own soak evidence

This document covers attaching an image and what a boot looks like.
`docs/hostblk-soak.md` covers the acceptance gate that actually justified
retiring Gayle: a destructive, concurrent devsoak run against the
driver, independent of any single boot succeeding. See that file for the
full account, including the one real driver bug (`NSDQR_SIZE`
off-by-four) it caught that a self-written test never would have.

## `board-qemu-q35`: real storage on the UEFI board

The bare-metal boards had no storage at all until this increment.
`board-qemu-q35` (`crates/board-qemu-q35/src/storage.rs`,
`crates/board-qemu-q35/src/fast_ram.rs`) now provides `hostblk` with a
real `BlockDevice` over UEFI's `EFI_BLOCK_IO_PROTOCOL`, and fast RAM
allocated from boot services — the same shape `machine-hosted` gives
`hostblk` (a `BlockDevice` plus fast RAM), sourced from firmware
instead of a host file and a process heap.

**Why Block I/O, not virtio-blk:** `board-qemu-q35` is *the UEFI
board*, not the x86 board (ADR 0001) — it already builds and runs for
`aarch64-unknown-uefi`, and `edk2-rk3588` gives a real ARM board
(Rock 5B) the same `EFI_BLOCK_IO_PROTOCOL` surface over SD/eMMC/NVMe.
virtio-blk would only ever serve QEMU.

### The risk checked first: does AROS honour a DiagArea boot ROM?

`hostblk`'s driver and RDB mounter ship in a DiagArea boot ROM
(`m68k/hostblk-rom/`), proven only against Kickstart 3.2.2 before this
increment. Nothing had ever exercised it under AROS, and if AROS
ignored DiagArea boot ROMs the board could serve a disk no guest could
ever read. This was checked with the existing hosted runner —
redistributable AROS ROMs, no board code involved — before any UEFI
code was written:

```
machine-hosted --rom assets/aros/aros-amiga-m68k-rom.bin \
  --ext-rom assets/aros/aros-amiga-m68k-ext.bin \
  --hostblk nondistribution/m68k-machine.hdf \
  --max-frames 150 --max-instructions 0 --inspect
```

**AROS does honour it.** `--inspect`'s dump at the end of that run shows
`hostblk.device` initialised, `DiagMarker` reading back `hostblk`'s own
`VERSION` register (proof `DiagEntry` ran from the card's own DiagArea
and could reach its registers), a mounted `MountList` boot node for
`DH0` with the RDB's `DosEnvec` decoded correctly, `DosList` carrying
both `DH0` and `SYS:`, and a live transfer descriptor in `hostblk`'s
submission slot (`cmd 1 unit 0 len 3584 offset 0x817a00`) — an
in-flight read, not just a discovery probe. This is the same evidence
`docs/hostblk-soak.md`/this file's own Kickstart 3.2.2 boot already
established for that OS; AROS gets exactly as far.

A second, longer hosted run under otherwise identical settings (500
frames instead of 150) reached only `hostblk.device`'s DiagArea
registration (`romtaginit done`) before parking in AROS's own
post-boot `STOP` idle (PC `$00FE8B88`) and advancing no further even
given far more frames and instructions than the 150-frame run needed
to reach a live DH0 mount. Both runs use the same ROMs, image, and CPU
model; the difference tracks which AUTOCONFIG board (fast RAM or
`hostblk`) configures first, which is itself unintentional and not
something either the hosted CLI or this board's `main.rs` currently
control. This looks like a pre-existing AROS boot-scheduling
sensitivity independent of the UEFI board work, not a Block I/O
defect — but it means "AROS mounts the RDB" is proven, not yet
proven *reliable* run-to-run. Worth investigating before leaning on
this board (or the hosted runner) for a repeatable AROS+`hostblk`
regression test.

### Choosing the right disk: scan for the `'RDSK'` signature, never guess

UEFI hands back one `EFI_BLOCK_IO_PROTOCOL` handle per whole disk *and*
one per logical partition on it, plus the ESP itself, with nothing in
the protocol distinguishing "the disk to serve" from any other. Given
this project's history of silent wrong-thing-selected failures
(`docs/device-ledger.md`), `storage::find_rdb_disk` never guesses by
handle order or size: it opens every `BlockIO` handle and scans each
one's first 16 sectors — the documented Amiga RDB search range,
matching `m68k/hostblk-rom/hostblk-diagrom.s`'s own mounter — for the
`'RDSK'` signature. First positive match wins and is handed to
`MachineBus::with_hostblk`; every handle checked, matched or not, is
logged. Confirmed under real QEMU with an ESP (FAT, no RDB) and a
second `-drive ...,if=virtio` disk holding `nondistribution/
m68k-machine.hdf`:

```
storage: handle 0: whole disk, native block size 512 bytes, 1032192 512-byte sectors -- no 'RDSK' signature in the first 16 sectors -- not a candidate
storage: handle 1: whole disk, no media present -- skipped
storage: handle 2: could not open BlockIO (Error { status: INVALID_PARAMETER, data: () }) -- skipped
storage: no Amiga RDB disk found on any EFI_BLOCK_IO_PROTOCOL handle -- continuing with no drive attached
```

(no disk attached: handle 0 is the ESP, correctly passed over; handle 1
is an empty removable-media slot; handle 2 fails to open exclusively,
plausibly a FAT filesystem sub-handle already claimed by
`SimpleFileSystem` — all three logged and skipped, not silently
ignored) versus, with the RDB image attached as a second drive:

```
storage: handle 0: whole disk, native block size 512 bytes, 18432 512-byte sectors -- 'RDSK' signature found at sector 0 -- serving this to hostblk
hostblk: unit 0 attached (read-only)
```

The board then reaches the identical `hostblk.device` DiagArea
registration (`GUEST | Diag board ... InitResident ... 'hostblk.device'`)
the hosted risk check showed, confirming the register-level path (card
discovered, DiagArea entered, registers reachable) works unchanged
under real UEFI firmware. Whether the RDB mount itself completes under
this specific harness before the board's own 200-frame/50M-instruction
Phase 1 boot budget runs out was not confirmed in this run (see the
scheduling-sensitivity note above — a from-scratch run with a much
larger frame budget still only reached the same DiagArea-registration
point before parking, matching the hosted runner's own less
reliable outcome at higher frame counts). "The board finds the disk
and hands it to `hostblk`, and `hostblk`'s driver runs and registers
from the board's DiagArea" is the real, verified result; a completed
boot from it is not yet.

### Block size: our constraint, not an Amiga one

`hostblk`'s wire protocol and `machine_core::block::SECTOR_BYTES` are
both hard-coded to 512 bytes; the RDB itself and later Kickstarts
handle other native block sizes fine via `rdb_BlockBytes`/
`de_SizeBlock`. `storage::EfiBlockDevice` reports the media's real
`block_size()` in every log line regardless of outcome, and adapts
rather than refuses whenever it safely can: block size 512 is a direct
passthrough (the only case seen under QEMU so far); a size that is a
clean multiple of 512 (e.g. 4096-byte NVMe/SSD sectors) is handled by
slicing 512-byte sectors out of the containing native block, with
writes as read-modify-write over that block; a size that cleanly
divides 512 composes a sector from several contiguous native blocks
with no partial-byte splicing needed. Only a size that is neither — not
a clean multiple or divisor of 512 — is refused, with the reported size
named in the log, since translating that case would mean behaving
differently on read vs. write boundaries.

### Fast RAM

`fast_ram::allocate` asks UEFI boot services for 256 MB (matching
`machine-hosted --fast-ram-mb`'s default and
`tools/amibake/m68k-machine.toml`'s declared machine shape), halving
down to a 1 MB floor on `OUT_OF_RESOURCES`/`NOT_FOUND` and reporting
exactly what it got. Under QEMU with `-m 512M` the full 256 MB
allocation succeeds every time (`fast RAM: 256 MB allocated from boot
services`); the fallback path itself is exercised only by construction
(halving loop, unit-testable logic) rather than by an observed
low-memory QEMU run in this increment.

### ROM-from-ESP: left out of this increment

Loading Kickstart from a file on the ESP via
`EFI_SIMPLE_FILE_SYSTEM_PROTOCOL`, falling back to the embedded AROS
ROM when absent, was in scope to consider. It is left out here: it is
an independent concern from the storage path this increment's risk
question was actually about (whether AROS honours a DiagArea boot
ROM), it adds real surface (locating the ESP's own `BlockIO`/
`SimpleFileSystem` handle, a licensed-file-shaped failure mode
distinct from "no RDB disk found," and another way the board's Phase 1
boot can fail to start at all) to a change that must not regress the
x86 CI markers, and nothing in this increment's evidence needed it —
the DiagArea risk check and the disk-selection work both run
end-to-end against the embedded AROS ROM alone. Worth its own
increment rather than folding in here.

## AROS and `hostblk`: it works, and the failure was somewhere else

The bare-metal board work raised a risk worth checking: `hostblk`'s
driver ships in a DiagArea boot ROM proven only against Kickstart 3.2.2,
and AROS is the only guest a board can boot without licensed media. If
AROS ignored DiagArea, a board could serve a disk no guest could read.

It does not. Checked in the hosted runner, which needs no board code:

```sh
machine-hosted --rom assets/aros/aros-amiga-m68k-rom.bin \
  --ext-rom assets/aros/aros-amiga-m68k-ext.bin \
  --hostblk nondistribution/m68k-machine.hdf --inspect
```

AROS reports `Diag board ... InitResident ... 'hostblk.device 1.0
(2026)'`, opens the device (`lib_OpenCnt 1`), accepts the mounter's
`AddBootNode` — the boot node carries an `FSSM` naming `hostblk.device`
unit 0 and a correct `DosEnvec` (`TableSize 16`, `LowCyl 1`, `HighCyl
575`, `DosType 0x444f5303`) — mounts the partition, and issues real
filesystem reads: `--inspect` catches a 3,584-byte read at LBA 16,573,
deep inside `DH0` rather than at a boot block.

**Then it opens a Workbench screen and blocks on a requester reading
"Please insert volume ENV: in any drive".** That is what an earlier
investigation saw as AROS "parking permanently in its post-boot STOP
idle" — it is not idle, it is a modal requester waiting for a click that
never came, and the CPU sits in `STOP` because there is nothing else to
do. Clicking Cancel through the native input card
(`--input-script`, `MOVE 460 144` then a button press) dismisses it and
AROS carries on.

So there is no storage defect. What fails is booting an **AmigaOS 3.2.2
install under AROS**, and the reason is specific rather than mysterious:
**AROS was written for AmigaOS 3.1 compatibility, and 3.2 has diverged.**
3.2 reworked the startup sequence and its environment handling, so a
3.2.2 image's `Startup-Sequence` reaches for an `ENV:` that AROS's
3.1-era conventions never set up. Nothing here is broken; the two halves
simply come from different releases.

That means the fix is a matter of pairing, not of patching. Boot AROS
from an **AROS-built** image, or — if the goal really is AmigaOS under
AROS — from a **3.1** one, which is what AROS was built to run.
`tools/amibake` has recipes for both (`aros68k` and `os3.1.4`).

Two things follow. A board that wants to boot AROS should be given an
**AROS-built image** — `tools/amibake`'s `aros68k` recipe, and AROS being
redistributable means unlike the AmigaOS image that path could run in CI. And a diagnosis of "parked in STOP" is worth distrusting on this
machine now that input exists: a guest waiting on a requester looks
exactly like a guest that has given up, and the two are told apart by
looking at the screen rather than at the CPU.

Also worth noting for anyone reading `--inspect` against AROS: its
`DosList` walk reports empty here even though the volume is plainly
mounted. AROS's `dos.library` is 50.80 and the walk assumes Kickstart's
layout, so that particular line is unreliable against AROS -- the
screenshot and the `hostblk` slot state are the evidence to trust.
