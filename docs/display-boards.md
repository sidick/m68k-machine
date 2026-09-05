# Display bring-up on the two bare-metal boards

**Status:** working on both boards, verified by screendump. This documents
what `board-qemu-virt` (aarch64, QEMU `virt`) and `board-qemu-q35` (x86-64
UEFI, QEMU `q35`) now do to get AROS's boot screen onto a real framebuffer,
how each was verified, and what was found already broken along the way.
Companion to `docs/screenshots.md`, which covers the same renderer's output
captured by the hosted runner rather than a bare-metal board.

## Scope

Both boards already booted the vendored AROS 68k ROM pair
(`assets/aros/`) on a real `m68k::CpuCore` and narrated progress over
serial (Phase 1). Neither had a display: `machine_core::render::Renderer`
existed and was exercised by `machine-hosted`'s `--screenshot`, but no
board wired it up to real hardware. This closes that gap — display only,
not storage, RTG or input, per this task's brief.

## board-qemu-q35: UEFI GOP

Uses the UEFI Graphics Output Protocol via `uefi::boot::get_handle_for_
protocol::<GraphicsOutput>()` + `open_protocol_exclusive`, from inside
`boot_aros` after the AROS boot run ends (`crates/board-qemu-q35/src/
main.rs`'s `present_to_gop`).

**Presents through `blt()`, not a raw framebuffer pointer write.**
`GraphicsOutput::frame_buffer()` exists and this crate's `uefi` version
supports it, but `blt(BltOp::BufferToVideo)` was chosen instead:
firmware converts the fixed `BltPixel` (BGR24+pad) buffer to whatever the
real mode's `PixelFormat`/stride actually are, including the
`PixelFormat::BltOnly` case where there is no CPU-writable framebuffer at
all. That sidesteps the exact red/blue-swap and stride-vs-width mistakes
the task brief called out as easy to get wrong, at the cost of one
`Vec<BltPixel>` copy per presentation — irrelevant for a single
end-of-boot present that is not on any hot path. `ModeInfo::stride()` and
`::pixel_format()` are both logged regardless (`display: GOP mode ...`),
so a real stride/format mismatch would still show up in the serial log
even though this driver's own code path never depends on getting them
right by hand.

**Verified** under this project's QEMU/OVMF (`scripts/run-qemu-q35.sh`,
unmodified): OVMF's headless default GOP mode is **1280x800, `Bgr`**.
`present::scale_factor(1280, 800, 752, 576)` computes `1x` (both axis
ratios round down to 1), so the picture is centred at native size with a
letterboxed border. A QMP `screendump` taken after the `display:
presented via GOP blt` serial line, decoded and inspected directly:

- **1280x800**, matching the reported GOP mode exactly.
- **14 distinct colours**, **6795/1,024,000 non-background pixels**.
- Visually: AROS's cat's-eyes logo, the AROS wordmark, "Waiting for
  bootable media" and four device icons, centred with black letterboxing
  on both axes — the same picture `docs/screenshots.md` describes for the
  hosted runner's `aros.png`/`aros-final.png` captures.

## board-qemu-virt: `ramfb` over `fw_cfg`

There is no UEFI here — `-kernel` loads the ELF directly, no firmware
underneath — and QEMU virt's only other realistic option, `virtio-gpu`,
needs a full virtqueue driver, which this task's brief explicitly said not
to build without checking in first. `ramfb` (`-device ramfb`) is the
documented alternative: a linear framebuffer configured once via `fw_cfg`,
after which QEMU scans it out on its own.

**Verified available before committing to it**, per the brief's
instruction: `qemu-system-aarch64 -M virt -device ramfb` (this project's
installed QEMU, 11.1.1) instantiates cleanly, and a QMP `screendump`
against that bare configuration produces a real 640x480 black image
before any guest touches it — confirming both that the device exists on
this machine type here and that the pre-configuration state is
inspectable the same way. Only then was the driver written.

**The catch, and why this needed a real driver, not a config flag:**
`ramfb` is normally configured by firmware (EDK2, U-Boot) acting as the
guest's `fw_cfg` client. This board has none. `crates/board-qemu-virt/
src/ramfb.rs` is that client, written from scratch against QEMU's
`docs/specs/fw_cfg.txt` DMA interface (confirmed MMIO base `0x0902_0000`,
size `0x18`, by dumping the `virt` machine's own device tree rather than
assuming it):

1. Select the fixed file-directory selector (`0x19`), walk it one byte at
   a time over the legacy data register, and find `etc/ramfb`'s own
   selector by name.
2. Build a 28-byte `RAMFBCfg` (address, `DRM_FORMAT_XRGB8888` fourcc,
   flags, width, height, stride), all fields big-endian via
   `to_be_bytes()` — plain RAM content the device DMA-reads, no MMIO
   endianness involved.
3. Build a 16-byte `FWCfgDmaAccess` (control encoding `SELECT | WRITE`
   plus the file's selector, length, and this struct's own address) and
   trigger it with one 64-bit write to the DMA address register.
4. Check the control word's `ERROR` bit afterward (QEMU performs the DMA
   synchronously with the triggering write) and report the outcome.

The one genuinely fiddly part is that every `fw_cfg` MMIO register is a
big-endian QEMU memory region, while this core runs little-endian
(`SCTLR_EL1.EE` is never touched) — `ramfb.rs`'s module doc comment
("Endianness") has the derivation; the fix is the same pre-swap
`iowrite32be`/`iowrite64be` uses on little-endian ARM Linux.

Wired into `scripts/run-qemu-virt.sh` via `-device ramfb`; without that
flag the driver reports `RamfbStatus::DeviceNotPresent` over serial and
the board otherwise runs exactly as before (confirmed: `-M virt` with no
`-device ramfb` still boots cleanly, just skips presentation).

**Chosen host resolution: 1600x1200**, deliberately not a multiple of the
Amiga canvas (that would make the integer-scale logic trivially always
pick a round number) — it is a real, independently standard 4:3 mode that
happens to fit `2x` on both axes.

**Verified**: `-trace fw_cfg_select` (this QEMU build has no
`fw_cfg_dma_*`/`ramfb_*` trace points; checked with `-trace help` first)
shows the file directory and `etc/ramfb`'s own selector both being read
during bring-up, and — the real acceptance test — a QMP `screendump`
taken after the run completes (serial line `PHASE1 BOARD-QEMU-VIRT:
LIMIT REACHED ...` followed by `display: presented via ramfb ...`),
decoded and inspected directly:

- **1600x1200**, matching `ramfb::HOST_WIDTH`/`HOST_HEIGHT` — note this
  is already true moments after boot starts (the earlier config write
  succeeded and painted an all-black canvas at the new size), and stays
  true with real content once the boot run's final frame is presented.
- **14 distinct colours**, **27,180/1,920,000 non-background pixels**.
- Visually: the same AROS boot screen as `board-qemu-q35` above, at `2x`
  integer scale (`1504x1152`), centred with a thin (`48px`/`24px`)
  letterboxed border — confirming the scale-then-letterbox logic actually
  exercises a `>1x` path, not just the trivial `1x` case.

## Presentation: integer scale, then letterbox

Per the project owner's correction to this task's original brief (which
had said "never scale, always centre"): integer scaling is free — pixel
replication loses nothing — so the rule is *pick the largest whole-number
factor that fits both axes, then centre; never a fractional factor*.
Implemented identically (and not shared — independent `no_std` crates, no
common library) in each board's own `present.rs`:
`scale_factor(host_w, host_h, src_w, src_h)` takes
`min(host_w/src_w, host_h/src_h).max(1)`; `blit_scaled_centered` fills the
host buffer with a background colour, then replicates each source pixel
into a `factor * factor` block at the centred offset, bounds-checking
every destination write individually so a host framebuffer smaller than
the scaled image on some axis crops rather than panics.

Neither board's `[[bin]]` can run a `cargo test` harness (`test = false`,
`no_std` with a custom `#[panic_handler]` — see each `Cargo.toml`'s doc
comment), so `present.rs`'s logic was instead checked with a throwaway
host-side `rustc` build against the worked examples in its own doc
comment (1920x1080 -> 1x, 1600x1200 -> 2x, a too-small host -> 1x, and the
letterbox offsets for a 3x3-in-5x5 case) before landing, and the real
verification is the screendump evidence above, which exercises it end to
end on the actual target.

## What was found already broken

Both boards' Phase 0 bus smoke check **`open-bus read at $DE1000 ==
$FFFFFFFF`/`open-bus read at $DE1000 == $FFFFFFFF`** currently **fails**
on this project's QEMU (11.1.1), on both `board-qemu-virt` and
`board-qemu-q35`, unrelated to any change in this task — confirmed by
building an unmodified `git worktree` checkout of the commit this task
started from and running it the same way. `machine-core`'s own history
(`git log`) shows Phase 3 added the Gayle IDE interface after these
boards' Phase 0 smoke tests were written; `0x00DE_1000` sits in the
address range Gayle's real hardware occupies, so the likely (not
confirmed — `machine-core` is outside this task's file ownership) cause
is that this address is now legitimately mapped rather than open bus, and
these two dormant Phase 0 checks were never updated to match. Neither
board's `cargo test`/`cargo clippy` gate catches this (both are QEMU-
serial-log checks, not host-run tests), and CI's own
`check-serial-markers.sh` for both `qemu-virt` and `qemu-q35` asserts
`"PHASE0 BOARD-QEMU-{VIRT,Q35}: ALL CHECKS PASSED"`, which this failure
means neither job can currently produce — **if CI's own runners see the
same failure, both bare-metal jobs are red already, independent of this
task's changes.** Not fixed here: `machine-core` is outside this task's
file ownership, and the fix (if the diagnosis above is right) belongs
either in `machine-core`'s memory map or in updating these two stale
smoke checks to a still-genuinely-unmapped probe address.

## Constraints carried forward

- `machine-core` untouched — the `Framebuffer`/`Renderer` seam already
  covered everything this task needed.
- CI's existing serial assertions are unaffected: `ci-grep-serial.sh`
  kills QEMU as soon as it sees `'chip memory'`, which happens well
  before either board's display step runs (the display step only runs
  once the whole bounded boot loop finishes — up to 200 frames / 50M
  instructions later). Every new serial line lands strictly after the
  markers CI already asserts, per the brief's note on the CI race this
  project already hit once (polling for a line that arrives *before* the
  asserted one kills QEMU too early).
- `cargo test --workspace`, `cargo fmt --all -- --check`, and
  `cargo clippy -p <crate> --target <target> -- -D warnings` (per crate,
  matching CI's own invocations) all pass as of this task.
