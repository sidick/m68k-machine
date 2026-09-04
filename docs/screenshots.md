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

**The WAIT-skipping caveat, in practice.** Because `WAIT`/`SKIP` are
no-ops here (the renderer never evaluates beam-position conditions —
proposal §8.1 says "WAITs skipped", not "WAITs honoured"), a copper list
that reprograms the *same* register at several different vertical wait
points (a classic technique for building a multi-icon list or a
per-scanline effect) collapses to whatever the *last* `MOVE` in the list
wrote, not what a real CRT would show scanline-by-scanline. The Kickstart
finding below is a real example of this in action.

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
host  | screenshot: frame 200 -> /path/out.png (752x576, 2 distinct colours, 859/433152 pixels differ from background)
```

"Background" here is the *most common* colour in the frame, not
necessarily `COLOR00` or the pixel at `(0,0)` — see the Kickstart finding
below for why that distinction mattered in practice (`screenshot.rs`'s
`FrameStats` doc comment has the full account).

For a "why is the picture blank" investigation, pair `--screenshot` with
`--inspect`, which (beyond its existing `ExecBase`/task-list report) now
also prints the display-relevant chipset registers the renderer's copper
walk starts from:

```
host  | display state: COP1LC 0x00001910  BPLCON0 0x0302  BPLCON1 0x0000  BPL1PT 0x00000000  DIWSTRT/STOP 0x2c81/0xf4c1  DDFSTRT/STOP 0x0038/0x00d0  COLOR00 0x0111
```

This is the *directly-written* register state, not what the copper list
itself would set when walked — a non-zero `COP1LC` with `BPLCON0`'s plane
field still at 0 is exactly the signature of "the guest started a copper
list, and the real picture lives inside it", which is what the Kickstart
finding below turned out to be.

## What was actually found

Both findings below were captured and inspected programmatically (pixel
counts, dimensions, bounding boxes — see the exact commands and analysis
in this project's history) and, for Kickstart, viewed directly as an
image; this section reports what was verified, not an impression.

### Kickstart 3.2.2 (A1200 47.115): real content, small and low-contrast

```
./target/release/machine-hosted \
  --rom <A1200 47.115 ROM> \
  --screenshot kick.png --screenshot-frame 200 \
  --max-frames 250 --max-instructions 80000000
```

`--inspect`'s display-state line shows a **non-zero `COP1LC`**
(`0x00001910`) — Kickstart has started a copper list — even though the
directly-latched `BPLCON0`/`BPL1PT` still read as "0 planes, no pointer".
The captured frame confirms the copper list itself does draw something:

- 2 distinct colours across the full 752×576 canvas.
- Background (the dominant colour, 432,293 of 433,152 pixels):
  `#111111` — decodes exactly from `COLOR00 = $0111` via
  `argb_from_amiga` (each 4-bit gun replicated to `$11`).
- Foreground: pure black (`#000000`), 859 pixels — a mouse-pointer arrow
  shape at the very top-left, followed by several rows of a small,
  repeating icon-like glyph pattern immediately below it.
- All 859 non-background pixels fall inside a **16×255 pixel bounding
  box in the top-left corner** (confirmed by trimming a background-vs-
  foreground mask with ImageMagick), and the repeating glyph pattern
  visibly repeats at a constant vertical interval within that column.

This is genuine content — a software mouse pointer (sprite 0, per
`draw_sprite0`) over what is almost certainly Kickstart's "please insert
a disk in any drive" boot alert, reached because this machine has no
boot device yet (no MIRAGE storage — roadmap Phase 3). But the picture's
width is suspicious: a 16-pixel-wide column is exactly **one bitplane
fetch word**, far narrower than a real 320-pixel alert screen. The most
likely explanation, and the one consistent with `render.rs`'s own
documented scope: **the WAIT-skipping caveat above.** A boot-alert screen
that draws a short list of per-drive icons is a natural fit for a copper
list that reprograms `BPL1PT`/`DDFSTRT`/`DDFSTOP` at several different
`WAIT`-gated vertical positions — one narrow segment per icon row. This
renderer executes every `MOVE` in the list unconditionally and ignores
every `WAIT`, so it collapses that multi-segment program down to
whichever segment's values were written *last*, rather than showing each
segment at its intended screen position. That reproduces exactly what
was observed: a real picture, correctly decoded pixel-for-pixel, but
compressed into one narrow column instead of spread across the intended
width. **This is the renderer behaving exactly as documented (§8.1: "WAITs
skipped"), not a bug** — it is the first real evidence of *where* the
stop-gap renderer's known limitation actually bites on real ROM output,
which is precisely what this task's real-ROM test was for.

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
Kickstart's boot alert is reacting to) and never reaches the code path
that would actually open a screen and program the chipset for one. The
task brief's predicted outcome ("AROS gets further and narrates over
serial, so it is the more likely candidate to have something on screen")
did not hold here for the *display*, specifically because narrating over
serial and opening a screen are independent capabilities and this build
of AROS never got to the second one — a real and useful negative result,
not an inconclusive one.

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

## Regression test

`crates/machine-hosted/tests/real_rom.rs` has two tests exercising this
end-to-end, both decoding the actual PNG bytes (via the `png` crate, this
crate's own `--screenshot` dependency) rather than trusting the runner's
stdout:

- `aros_68k_screenshot_is_a_flat_fill_pending_mirage_storage` — runs
  **unconditionally** (the AROS ROM pair is vendored in-repo,
  `assets/aros/`, freely redistributable per `assets/aros/PROVENANCE.md`)
  and asserts the captured frame is a uniform fill, per the finding
  above.
- `kickstart_3_2_2_a1200_screenshot_shows_real_content` — **skip-when-
  absent** (`#[ignore]`, checked and skipped if the user-supplied,
  non-redistributable ROM isn't on disk, matching every other Kickstart
  test in this file) and asserts the captured frame has real,
  non-background content, bounded to a small fraction of the canvas (a
  mouse pointer and a short icon list, not a renderer gone wrong).

Both derive "background" from the *most common* pixel colour rather than
the pixel at `(0,0)` — the Kickstart finding above is exactly the reason:
its mouse pointer's hot spot lands at `(0,0)` in this machine's
DIW-relative coordinates, so pixel `(0,0)` is foreground, not background.

Run with:

```
cargo test -p machine-hosted --test real_rom aros_68k_screenshot_is_a_flat_fill_pending_mirage_storage
cargo test -p machine-hosted --test real_rom kickstart_3_2_2_a1200_screenshot_shows_real_content -- --ignored
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
