# Storage: attaching a disk to Gayle IDE

`crates/machine-core/src/gayle.rs` implements the A1200's Gayle IDE
interface (ID gate at `$DE1000`, task file at `$DA2000`, LBA28/CHS,
`IDENTIFY`, multi-sector PIO and interrupts). It exists because the A1200
Kickstart already ships a working IDE driver (`scsi.device` reporting
itself as `IDE_scsidisk`, `FileSystem.resource`, FFS), so presenting its
register map gets a mountable, bootable disk with no m68k code of our own
(`gayle.rs`'s module doc comment, proposal §9, §11.1). **MIRAGE (proposal
§10.3) remains the architecture's real storage story** — a Zorro III card
with its own m68k driver — and still has to be built. Gayle IDE is the
bring-up device that lets the rest of the machine be exercised against a
real filesystem and a real Workbench before that chain exists.

This document covers the host side: how a disk image gets attached to
that interface in `machine-hosted`, how the one image this has actually
been tested against was built, and exactly how far a real boot gets with
it today.

## Attaching an image

```
machine-hosted --rom <kickstart.rom> --hd <path-to-image.hdf>
```

`--hd` takes a path to a raw, sector-addressable disk image — an `.hdf`
(a bare RDB-partitioned image, no ADF/DMS-style wrapper) is exactly this
shape. Bytes are read and written to the file exactly as they sit on
disk, sector by sector, with no transformation at this layer: the Gayle
author's handover is explicit that the CPU-visible byte swap (drive
D7-D0 wired to CPU D15-D8) is internal to how the data register hands
bytes to the guest and cancels out for opaque sector bytes, so the
host-side `BlockDevice` (`crates/machine-hosted/src/hd_image.rs`,
`FileBlockDevice`) never needs to know about it. `sector_count` is the
image's true size, truncated down from its byte length; an out-of-range
LBA (or, for writes, a read-only image — see below) is reported to the
guest as a clean ATA `IDNF` error rather than a host-side failure, the
same as `Gayle` does for a rejected multi-sector transfer.

Omitting `--hd` leaves this machine exactly as before: no drive attached
at all, which is the honest hardware story for the eventual MIRAGE path
and is what a plain `machine-hosted --rom ...` still does.

### Read-only by default

**`--hd` opens the image read-only unless `--hd-writable` is also
passed.** This was a deliberate call, not an oversight: `gayle.rs` is a
brand new, so far unproven implementation (14 unit tests, one real disk
boot so far — see below), and the images worth attaching to it are not
throwaway. The one this document is about is a licensed-media conversion
that took real build effort and, per its own licensing (next section),
cannot simply be re-downloaded if a bug in an early IDE write path
corrupts it. A full boot to a real Workbench screen (confirmed below)
never needs to write a single sector. Write access is one flag away —
`--hd-writable` — for whoever actually needs Kickstart's or Workbench's
write path exercised, at which point the risk is a conscious choice
rather than a default.

## The test image

`/private/tmp/.../scratchpad/hdf2/m68k-machine.hdf` (9 MB) is a bootable
AmigaOS 3.2.2 HDF: one RDB, one bootable `DH0:` FFS partition. It was
built with [amibake](../tools/amibake/m68k-machine.toml) from licensed
Amiga OS media:

```
~/src/amibake/.venv/bin/amibake build tools/amibake/m68k-machine.toml
```

`tools/amibake/m68k-machine.toml`'s machine block mirrors what this
machine actually presents to the guest — 68040 with FPU, 2 MB chip RAM,
no fast RAM, no RTG — and includes the `mmulibs` package so
`68040.library` is present (without it, a stock 3.2.2 Startup-Sequence
nags every boot about a missing CPU support library on a machine that
reports a 68040). It emits both an `hdf` (what Gayle IDE serves) and a
`copperline` config, so the identical image can be booted under the
[Copperline](https://codeberg.org/copperline) oracle for comparison —
Copperline models the same built-in Gayle IDE interface.

**Licensing: this image contains licensed AmigaOS 3.2.2 and is treated
exactly like the Kickstart ROM this project already depends on** — never
committed to this repo, never redistributed. Note that, unlike the
Kickstart ROM (which every existing real-ROM test keeps at a path
entirely outside the repo tree, `~/src/amirfb/...`), **`.gitignore` does
not currently exclude an amibake output directory** — it only covers
`target/`, `*.swp` and `.DS_Store`. Build amibake's output somewhere
outside this repo (as the test image above does — a scratch directory,
not a path under this checkout) rather than relying on `.gitignore` to
catch it if built in-tree; this is worth a follow-up `.gitignore` entry
if amibake output is ever built inside the repo as a matter of course.
Anyone reproducing this needs their own licensed AmigaOS 3.2.2 media and
their own amibake checkout;
the test in `crates/machine-hosted/tests/real_rom.rs` that exercises it
skips cleanly (prints a message, asserts nothing) when the image is not
present at its configured path, the same convention every other
real-ROM/real-image test in that file already uses.

## How far it gets, with evidence

Run:

```
cargo build -p machine-hosted --release
./target/release/machine-hosted --rom <A1200 Kickstart 47.115> \
  --hd <the HDF above> \
  --screenshot /tmp/wb.png --screenshot-frame 4000 --max-frames 4500 --inspect
```

**It boots all the way to a live Workbench screen.** In order:

1. **Drive detected.** The ID gate answers `$D1` regardless of a disk
   being attached (unchanged from before Gayle existed); with `--hd`
   given, `IDENTIFY` is issued and answered — `--inspect`'s
   `disk state:` line and the runner's own `hd: <path> (18432 sectors,
   read-only)` startup line confirm the device is attached and its true
   size (18432 x 512 B = 9 MB) is reported.
2. **The RDB is read and `DH0:` mounts.** `--inspect`'s resident-module
   report at the end of a run shows `filesystem` (`fs 47.4`) and
   `FileSystem.resource` initialised, and DOS getting far enough to run
   a startup sequence (next point) is only possible once the partition
   mounted.
3. **The filesystem loads and `Startup-Sequence` runs.** The resident
   list includes `con-handler`, `shell`, `system-startup`, and
   `ram-handler` all initialised — `system-startup` in particular is
   the module that runs `S/Startup-Sequence` from the booted volume.
4. **Workbench draws.** A screenshot taken at frame 4000 (and every 250
   frames out to 5500 — pixel-identical throughout, so this is a
   stable end state, not a snapshot mid-draw) shows a grey Workbench
   screen with an Intuition System Request titled "Please insert
   volume DF0: in any drive", "Retry"/"Cancel" gadgets, two lines of
   body text: 6 distinct colours, ~5,400 non-background pixels out of
   433,152.

**That requester is a correct outcome for this exact image, not a
stall.** This image's own `S/Startup-Sequence` line 17 is
`AddBuffers >NIL: DF0: 15` — an ordinary startup line that references
`DF0:`. `>NIL:` only suppresses the command's own output; the
missing-volume requester is raised by DOS itself whenever `DF0:` is
referenced and no disk is present, independent of `>NIL:`. This machine
defaults to `--floppy none` (no floppy drive at all, proposal
§3/§10.3's honest hardware story, unrelated to Gayle IDE), so `DF0:`
never resolves — exactly as a real A1200 with no floppy drive installed
behaves running the same Startup-Sequence. By the time this requester
can appear at all, the ID gate, RDB, partition mount, filesystem, DOS,
and Intuition/Workbench have every one of them already worked.

Frame timing, for anyone adjusting `--screenshot-frame`: Kickstart's IDE
probe timeout (~1500 frames with no disk responding) does not apply
here — a disk that answers `IDENTIFY` promptly is found well before
that. What takes the extra time past the old no-disk boot-screen test's
frame 2500 is the real disk-boot work itself (RDB scan, filesystem load,
`Startup-Sequence` execution, opening a Workbench screen). Sweeping
`--screenshot-every 250` from frame 4000 to 5500 found the requester
screen already fully drawn and completely pixel-stable across that whole
range, so frame 4000 (with `--max-frames 4500` for margin past the
capture boundary) is what the regression test below uses.

## Regression test

`crates/machine-hosted/tests/real_rom.rs`'s
`kickstart_3_2_2_a1200_boots_from_hd_to_the_insert_df0_requester` drives
the exact run above and asserts on the captured screenshot: the runner
reports attaching the image read-only, the capture actually fires by
frame 4000, the canvas is the renderer's usual full size, more than 3,000
non-background pixels are drawn (comfortably below the ~5,400 actually
observed, so a real regression in "how much of the requester drew" still
fails this), and at least 4 distinct colours are present (grey
background, requester border/title bar, gadget outlines, text). Like
every other test in that file that depends on a real Kickstart ROM or
(now) a real disk image, it is `#[ignore]`d and skips cleanly — printing
which file is missing rather than failing — when the ROM or the image
is not present at its configured path; run explicitly with
`cargo test -p machine-hosted -- --ignored kickstart_3_2_2_a1200_boots_from_hd`.

`crates/machine-hosted/src/hd_image.rs` also carries its own fast,
unconditional unit tests (sector count from file length, a trailing
partial sector not being addressable, read-only refusing writes, a
writable round trip, and a clean out-of-range failure) that run under a
plain `cargo test -p machine-hosted` with no image required.

## Nothing found wrong in `gayle.rs`

This work only added the host-side `BlockDevice` and the `--hd`/
`--hd-writable` flags in `machine-hosted`; `crates/machine-core/**` was
not touched. Nothing observed while driving a real boot through it
looked like a Gayle bug — the interface got a real Kickstart all the way
to a drawn Workbench screen on the first image tried, with no working
around a chipset issue anywhere in this integration.
