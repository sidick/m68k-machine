# Screenshots: seeing the Phase 2 stop-gap renderer's output

**Status:** working, verified against both project ROMs (Kickstart 3.2.2
A1200 and the vendored AROS 68k pair). This documents `machine-hosted`'s
`--screenshot`/`--screenshot-frame`/`--screenshot-every` flags, what the
stop-gap planar renderer they drive can and cannot show, and what was
actually found on each ROM's captured frames — with the pixel-level
evidence behind those findings, not an impression of the PNGs.

## What this is, and what it is not

`crates/machine-core/src/render.rs`'s `Renderer` (proposal §8.1) is the
**stop-gap planar renderer**: a deliberately dumb, ~500-line display path
that exists for visibility before P96 is installed — the early startup
menu, boot, Gurus, Screenmode prefs — and as the *permanent* Guru/early-boot
display, never extended beyond that. It is not, and is never going to
become, the production display path; that is P96 on a virtual framebuffer
card (proposal §8.2), landing in Phase 3+. Until this task, nothing had
ever driven it from a real booting guest — only from hand-built synthetic
bitmaps in `render.rs`'s own unit tests. `--screenshot` is what closes
that gap: it is Phase 2's first real end-to-end test, exercising the
copper walk, geometry decode and palette handling against what an actual
OS ROM programs, not against fixtures this project wrote itself.

**In scope** (§8.1): once per captured frame, walk the copper list at
`COP1LC` linearly (`MOVE`s honoured, `WAIT`/`SKIP` skipped — see the
caveat below), then paint from the resulting latched `BPL`/`DIW`/`DDF`/
`COLOR` state — lores/hires, 1–5 bitplanes, interlace, and a software
mouse pointer from sprite 0.

**Explicitly out of scope, permanently** (§8.1's exclusion list, quoted
directly): "No dragging, per-line palettes, HAM, EHB, dual playfield."
Also not implemented: `BPLCON1`'s fine horizontal scroll (`render.rs`'s
`Geometry::decode` doc comment — the feature exists for smooth-scrolling
demos, not this renderer's boot/Guru/Screenmode-prefs audience). None of
this is a bug to fix later; `render.rs`'s own module doc comment is
explicit: "**Not extended, ever**."

**The WAIT-skipping caveat.** Because `WAIT`/`SKIP` are no-ops here (the
renderer never evaluates beam-position conditions — proposal §8.1 says
"WAITs skipped", not "WAITs honoured"), a copper list that reprograms the
*same* register at several different vertical wait points collapses to
whatever the *last* `MOVE` in the list wrote, not what a real CRT would
show scanline-by-scanline. This is a real, documented limitation of the
renderer — but it is **not** what explains the Kickstart finding below;
an earlier draft of this document mis-attributed a real bug to this
caveat, see that finding's "corrected diagnosis" note for the actual
cause and how it was found.

## Capturing a screenshot

```
cargo build -p machine-hosted --release

./target/release/machine-hosted \
  --rom <path to a ROM> [--ext-rom <path, for the AROS pair>] \
  --screenshot out.png \
  --screenshot-frame 300 \
  --max-frames 500
```

- `--screenshot <path>` — capture to a PNG file. Always the renderer's
  full worst-case canvas, `display::MAX_WIDTH`×`display::MAX_HEIGHT`
  (752×576: hires PAL interlace plus overscan headroom), regardless of
  what the guest actually programmed — a smaller real picture just
  leaves the rest of the canvas at the background colour, the same way
  `Framebuffer::put`'s clipping already works.
- `--screenshot-frame <n>` — which chipset frame (`Chipset::frames`, a
  `VERTB` boundary) to capture. Default `300`: comfortably inside the
  runner's own default `--max-frames 6000` while late enough that
  Kickstart/AROS have had time to program something (or conclusively not
  have) — see the findings below for what "something" turned out to be
  on each ROM.
- `--screenshot-every <n>` — with `--screenshot` set, capture every `n`
  frames from `--screenshot-frame` onward (through `--max-frames`)
  instead of a single frame, each to its own numbered file
  (`out-000300.png`, `out-000400.png`, ...). Useful for watching boot
  progress frame by frame; every capture logs a stats line (see below) so
  a sequence can be scanned from stdout alone before opening any image.

Every capture also logs a diagnostic line to the console:

```
host  | screenshot: frame 200 -> /path/out.png (752x576, 1 distinct colour, 0/433152 pixels differ from background)
```

"Background" here is the *most common* colour in the frame, not
necessarily `COLOR00` or the pixel at `(0,0)` — an earlier version of
this stat used the top-left pixel and got it backwards on a real capture
that (because of the sprite bug the Kickstart finding below documents)
happened to have its drawn content land exactly at `(0,0)`; see
`screenshot.rs`'s `FrameStats` doc comment for the full account.

For a "why is the picture blank" investigation, pair `--screenshot` with
`--inspect`, which (beyond its existing `ExecBase`/task-list report) now
also prints the display-relevant chipset registers the renderer's copper
walk starts from:

```
host  | display state: COP1LC 0x00001910  BPLCON0 0x0302  BPLCON1 0x0000  BPL1PT 0x00000000  DIWSTRT/STOP 0x2c81/0xf4c1  DDFSTRT/STOP 0x0038/0x00d0  COLOR00 0x0111  SPR0PT 0x00000000
```

This is the *directly-written* chipset register state, not what the
copper list itself sets when the renderer walks it — a non-zero `COP1LC`
just means the guest started a copper list, not that the list turns out
to enable anything; read `BPLCON0`'s plane-count field (bits 12-14) and
`BPL1PT` from this line to see whether it actually did (see the
Kickstart finding below, where it did not). `SPR0POS`/`SPR0CTL` are
deliberately not printed here — see the line's own doc comment
(`introspect.rs`) for why trusting those two registers for a sprite is
exactly the mistake that produced the bug documented below.

## What was actually found

Both findings below were captured and inspected programmatically (pixel
counts, dimensions, bounding boxes — see the exact commands and analysis
in this project's history) and, for Kickstart, viewed directly as an
image; this section reports what was verified, not an impression.

### Kickstart 3.2.2 (A1200 47.115): a flat fill — and a real renderer bug found and fixed along the way

```
./target/release/machine-hosted \
  --rom <A1200 47.115 ROM> \
  --screenshot kick.png --screenshot-frame 200 \
  --max-frames 250 --max-instructions 80000000
```

`--inspect`'s display-state line shows a **non-zero `COP1LC`**
(`0x00001910`) — Kickstart has started a copper list — but `BPLCON0`'s
plane-count field (bits 12-14) is 0 and `BPL1PT` is null. Replicating the
renderer's own copper walk (`MOVE`-only, same register offsets) confirmed
this is the copper list's own final state too, not just the pre-walk
register snapshot: the list writes `SPR0PTH`/`SPR0PTL` (final value
`$0000B2B8`) and `SPR0POS` (`$0000`), and ends without ever writing
`BPL1PT` or a non-zero plane count to `BPLCON0` at all. **Kickstart has
not opened a screen at this point** — consistent with Phase 1's finding
that this machine has no boot device yet (no MIRAGE storage — roadmap
Phase 3) and `intuition.library` never gets past that.

**What the captured frame actually contains, correctly rendered:** a
uniform 752×576 fill, 1 distinct colour, 0 pixels differing from the
background — `#111111`, decoding exactly from `COLOR00 = $0111` via
`argb_from_amiga` (each 4-bit gun replicated to `$11`). No mouse pointer,
no icons, nothing else. This matches the AROS finding below almost
exactly, and for the same underlying reason.

**The bug this investigation found and fixed.** A first attempt at this
capture found what looked like real content: a recognisable mouse-pointer
arrow shape at the top-left, followed by ~239 more rows of small
repeating glyph-like marks, all confined to a 16-pixel-wide column (859
non-background pixels total, out of 433,152). That was **not** guest
output — it was `render.rs`'s `draw_sprite0` reading the wrong source
for sprite 0's height. The function decoded `VSTART`/`VSTOP` from the
chipset's `SPR0POS`/`SPR0CTL` registers, but this ROM's copper list never
writes `SPR0CTL` at all — real hardware's sprite DMA loads `SPR0POS`/
`SPR0CTL` autonomously from the sprite list in chip RAM every frame
(a `SPRxPT`-relative position/control header, per the Amiga Hardware
Reference Manual), and this machine has no such DMA engine to keep those
two registers in sync with whatever `SPR0PT` currently points at. The
chipset's `SPR0CTL` was consequently left holding a stale, unrelated
value (`$FF00`, from some earlier direct CPU write with nothing to do
with the pointer sprite), which decoded to `VSTOP=$FF` — a ~255-line
sprite. `draw_sprite0` then painted the *real* pointer image (fetched
correctly from `SPR0PT+4` onward, which is memory-relative and was never
wrong) for its first ~16 real rows, then kept going for 239 more rows
into whatever chip RAM happened to follow it, because nothing bounded the
height to the sprite's *actual* extent. The real position/control header
at `SPR0PT`/`SPR0PT+2` — read directly from chip RAM to check — is
`$0000`/`$0000`: `VSTART=VSTOP=0`, hardware's own null-sprite pattern.
Sprite 0 is genuinely, correctly disabled here; the 255-line shape was
manufactured by the bug, not drawn by Kickstart.

**The fix**: `draw_sprite0` now reads the position/control header from
chip RAM at `SPR0PT`/`SPR0PT+2` — exactly where real sprite DMA would
have fetched it from — instead of from the chipset's `SPR0POS`/`SPR0CTL`
registers, which the renderer's internal `CopperState` no longer even
shadows (removed, rather than left as an attractive nuisance for the next
reader). Pinned by a new unit test in `render.rs`,
`sprite_height_comes_from_the_real_header_in_ram_not_the_stale_ctl_register`,
which programs a genuine one-line sprite in RAM while leaving the
chipset's `SPR0CTL` at an implausibly tall stale value and asserts only
the real line is drawn.

### AROS 68k: genuinely blank, and here is why

```
./target/release/machine-hosted \
  --rom assets/aros/aros-amiga-m68k-rom.bin \
  --ext-rom assets/aros/aros-amiga-m68k-ext.bin \
  --screenshot aros.png --screenshot-frame 400 \
  --max-frames 500 --max-instructions 300000000
```

Every AROS capture taken (frames 100 through 3000+, across several runs)
came back **uniformly one flat colour, 0 pixels differing from the
background, across the full 752×576 canvas.** `--inspect`'s display-state
line explains why directly: `COP1LC 0x00000000`, `BPL1PT 0x00000000`, and
`BPLCON0`'s plane field at 0 — the guest has never started a copper list
or enabled bitplane DMA at all, at any frame tested up to 3000 (over
2.7M instructions retired with essentially no forward progress past that
point).

This is not "the renderer failed to find something" — `--inspect`'s
resident-module and task-list report shows AROS gets meaningfully far:
`workbench.task`, `workbook.resource`, `shell.resource`,
`shellcommands.resource` and more are all initialised, well past the
point of `graphics.library` being available. But `TaskWait` shows the
bootstrap task and `trackdisk.device` both parked, unchanged from frame
~300 through frame 3000+ — AROS is blocked waiting on disk I/O this
machine cannot yet provide (no MIRAGE storage — the same Phase 3 gap
Kickstart is also stuck on) and never reaches the code path that would
actually open a screen and program the chipset for one. Kickstart and
AROS converge on the same result — a flat, contentless frame — for the
same reason, from two independent guests: neither ROM's `intuition.
library`/`graphics.library` equivalent ever gets to open a screen without
a boot device to load one from.

One incidental finding while preparing this: `COLOR00` itself is not
perfectly stable even while everything else is idle — it read `$0EF9` at
every frame sampled from 100–600 in one run, then `$0111` once the same
run's final frame (601, right at its own `--max-frames` boundary) was
inspected. The flat-fill colour drifts a little during AROS's otherwise-
idle housekeeping (a periodic task in `TaskWait` does still wake
occasionally); the *absence* of any bitplane content does not. The
regression test below deliberately avoids asserting a specific colour for
this reason, and captures well clear of any `--max-frames` boundary to
avoid observing a register caught mid-update.

## Regression tests

Two layers, matching where each finding above actually lives:

**The sprite bug itself** is pinned in `crates/machine-core/src/render.rs`:
`sprite_height_comes_from_the_real_header_in_ram_not_the_stale_ctl_register`
programs a genuine one-line sprite in RAM at `SPR0PT` while leaving the
chipset's `SPR0CTL` register at an implausibly tall stale value (`$FF00`,
matching the real boot capture), and asserts the rendered sprite's height
comes from the real header, not from `MAX_SPRITE_LINES` or the stale
register:

```
cargo test -p machine-core render::tests::sprite_height_comes_from_the_real_header_in_ram_not_the_stale_ctl_register
```

**End-to-end real-ROM capture** is exercised in
`crates/machine-hosted/tests/real_rom.rs`, both tests decoding the actual
PNG bytes (via the `png` crate, this crate's own `--screenshot`
dependency) rather than trusting the runner's stdout, and both asserting
the same thing for the same underlying reason — a uniform fill, per the
findings above:

- `aros_68k_screenshot_is_a_flat_fill_pending_mirage_storage` — runs
  **unconditionally** (the AROS ROM pair is vendored in-repo,
  `assets/aros/`, freely redistributable per `assets/aros/PROVENANCE.md`).
- `kickstart_3_2_2_a1200_screenshot_is_a_flat_fill_pending_mirage_storage`
  — **skip-when-absent** (`#[ignore]`, checked and skipped if the
  user-supplied, non-redistributable ROM isn't on disk, matching every
  other Kickstart test in this file).

Both derive "background" from the *most common* pixel colour rather than
the pixel at `(0,0)` — a first attempt at the Kickstart test used
`(0,0)` and got it backwards, because the (buggy, since-fixed) sprite
render happened to put its hot spot exactly there.

Run with:

```
cargo test -p machine-hosted --test real_rom aros_68k_screenshot_is_a_flat_fill_pending_mirage_storage
cargo test -p machine-hosted --test real_rom kickstart_3_2_2_a1200_screenshot_is_a_flat_fill_pending_mirage_storage -- --ignored
```

## Design notes

**Why PNG, and why the `png` crate.** PPM would need no dependency at
all, but every image viewer, browser and CI artifact viewer opens PNG for
free, while PPM needs a conversion step first — worth one small
dependency given screenshots exist purely for a human (or this crate's
own regression test) to look at. `png` (the `image-rs` org, MIT/
Apache-2.0) is pure Rust with no C toolchain requirement, keeping this
hosted-only binary's build exactly as simple as `clap`'s already is; the
alternative, the `image` crate, pulls in codecs for a dozen formats this
project will never use. `machine-hosted` is a `std` binary with no
bare-metal build to keep lean (unlike `machine-core`), so this only
affects the runner's own compile time — see `crates/machine-hosted/
Cargo.toml`'s comment on the dependency.

**Reconstructing chip RAM without a `machine-core` change.** The
renderer wants `chip_ram: &[u8]`, but `MachineBus` borrows its backing
2 MB array exclusively for its whole lifetime and exposes no direct slice
accessor — only the bounds-checked `read_byte`/`read_word`/`read_long`
`introspect.rs` already uses. `screenshot.rs`'s `snapshot_chip_ram`
reconstructs the array through that same public read path instead, at
whatever frames are actually captured (never per-frame in the hot path,
so the 2 MB of per-byte reads costs nothing that matters). This is a
`machine-core` change this crate is not positioned to make itself: a
`pub fn chip_ram(&self) -> &[u8; CHIP_RAM_SIZE]` accessor on `MachineBus`
would let the runner borrow the array directly and skip the copy —
worth adding there if screenshotting ever becomes a hot path (e.g. a
live preview), not needed for the handful of frames `--screenshot`/
`--screenshot-every` actually capture today.
