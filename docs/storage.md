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
