# Screenshots: seeing the Phase 2 stop-gap renderer's output

**Status:** working, verified against both project ROMs (Kickstart 3.2.2
A1200 and the vendored AROS 68k pair). This documents `machine-hosted`'s
`--screenshot`/`--screenshot-frame`/`--screenshot-every` flags, what the
stop-gap planar renderer they drive can and cannot show, and what was
actually found on each ROM's captured frames — with the pixel-level
evidence behind those findings, not an impression of the PNGs.

**Both ROMs now render their real boot screens, with no boot device
attached at all.** That was not always true of this document: an earlier
pass found both guests producing a flat, contentless fill and attributed
that to having no boot device (Phase 3's MIRAGE storage). The flat fill
was real, but the diagnosis was wrong — see "What was actually found"
below for the corrected story and the renderer bugs it took fixing to
get from flat fill to real picture. The correction is kept visible here
rather than silently rewritten, because it is exactly the kind of trap
this document exists to record: this project's own history has two
separate investigations that were misled by the same `--inspect` line
before the "chipset registers vs. copper list" distinction below was
understood.

## What this is, and what it is not

`crates/machine-core/src/render.rs`'s `Renderer` (proposal §8.1) is the
**stop-gap planar renderer**: a display path that exists for visibility
before P96 is installed — the early startup menu, boot, Gurus, Screenmode
prefs — and as the *permanent* Guru/early-boot display, never meant to
become the production display path; that is P96 on a virtual framebuffer
card (proposal §8.2), landing in Phase 3+. `--screenshot` is Phase 2's
end-to-end test of it: it exercises the copper walk, geometry decode and
palette handling against what an actual OS ROM programs, not against
fixtures this project wrote itself.

**In scope** (§8.1, current wording): everything needed to render an
ordinary screen correctly — vertical `WAIT` honoured (so a copper list's
"planes on for a band, off outside it" idiom actually shows the band, not
its trailing planes-off state), `COPJMP1`/`COPJMP2` followed between
copper lists, the `DMACON` bitplane-DMA gate, `BPLCON1` whole-pixel
scroll, correct hires geometry and fetch-unit arithmetic, interlace as
two interleaved fields, and a software pointer for sprite 0 read from the
real sprite list header in chip RAM. See proposal §8.1 for the full
statement and `render.rs`'s module doc comment for the same line drawn in
code.

**Explicitly out of scope, permanently:** HAM, EHB, dual playfield,
sprite dragging, per-pixel copper effects, `WAIT`'s horizontal position,
`SKIP`'s conditional behaviour, and mid-frame `BPLxPT` reprogramming.
None of this is a bug to fix later.

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
  `VERTB` boundary) to capture. Kickstart's boot screen is stable by
  frame 200; AROS's "Waiting for bootable media" screen is up by frame
  ~400 — see the findings below for how those numbers were arrived at.
  Stay clear of a run's own `--max-frames` boundary: a capture taken on
  the run's final frame can observe a register mid-update (see AROS's
  `COLOR00` finding below).
- `--screenshot-every <n>` — with `--screenshot` set, capture every `n`
  frames from `--screenshot-frame` onward (through `--max-frames`)
  instead of a single frame, each to its own numbered file
  (`out-000300.png`, `out-000400.png`, ...). Useful for watching boot
  progress frame by frame; every capture logs a stats line (see below) so
  a sequence can be scanned from stdout alone before opening any image.

Every capture also logs a diagnostic line to the console:

```
host  | screenshot: frame 200 -> /path/out.png (752x576, 4 distinct colours, 6213/433152 pixels differ from background)
```

"Background" here is the *most common* colour in the frame, not
necessarily `COLOR00` or the pixel at `(0,0)` — an earlier version of
this stat used the top-left pixel and got it backwards on a real capture
that (because of the sprite bug described below) happened to have its
drawn content land exactly at `(0,0)`; see `screenshot.rs`'s `FrameStats`
doc comment for the full account.

For a "why is the picture wrong" investigation, pair `--screenshot` with
`--inspect`, which (beyond its existing `ExecBase`/task-list report) also
prints the display-relevant chipset registers the renderer's copper walk
starts from:

```
host  | display state: COP1LC 0x00001910  COP2LC 0x00035f78  BPLCON0 0x0302  BPLCON1 0x0000  BPL1PT 0x00000000  DIWSTRT/STOP 0x2c81/0xf4c1  DDFSTRT/STOP 0x0038/0x00d0  COLOR00 0x0111  SPR0PT 0x00000000
```

**The trap this line sets, twice found the hard way:** this is the
*directly-written* chipset register state — the CPU-written stub values —
not what the copper *list* itself programs when the renderer's copper
walk actually runs it. Kickstart's boot view is the concrete case: `COP1LC`
points at a small stub whose only job is to strobe `COPJMP2` and hand
over to the real list, which lives at `COP2LC` and is what actually
builds the boot screen frame by frame from the VBlank server. Reading
`BPLCON0`/`BPL1PT` off this line alone — the CPU-written stub state —
gives plane count 0 and a null bitplane pointer, which looks exactly like
"no screen has been opened." It is not: the copper list the renderer
walks builds a real 4-plane picture that this line never shows. Both
`COP1LC` and `COP2LC` are printed for exactly this reason. This line is a
diagnostic for "did the guest program anything at all", not a substitute
for what the copper list itself resolves to; that distinction cost two
separate investigations here (Kickstart's, described in the finding
below, and an earlier draft of this document's flat-fill misdiagnosis
before the copper-list fix existed at all) before it was written down.
`SPR0POS`/`SPR0CTL` are deliberately not printed on this line at all —
see the sprite finding below for why trusting those two registers is
exactly the mistake that produced a real renderer bug.

## What was actually found

Both findings below were captured and inspected programmatically (pixel
counts, dimensions, bounding boxes — see the exact commands and analysis
in this project's history) and viewed directly as images; this section
reports what was verified, not an impression.

### Kickstart 3.2.2 (A1200 47.115): the real boot screen, after three renderer bugs

```
./target/release/machine-hosted \
  --rom <A1200 47.115 ROM> \
  --screenshot kick.png --screenshot-frame 200 \
  --max-frames 250 --max-instructions 80000000
```

The captured frame is Kickstart's real boot picture: the round checkered
ball, the Hyperion banner and the floppy graphic, on a 4-plane hires
screen, matching the Copperline oracle's proportions. Getting there from
the original flat-fill reading took three separate fixes, each pinned by
a regression test:

1. **The renderer never followed `COPJMP2`.** `COP1LC` points at a stub
   that strobes into the real list at `COP2LC`; the original walk started
   and stayed at `COP1LC`, so it only ever saw the stub — and even once
   jump-following was added, the *same* list's trailing `WAIT`/`MOVE
   BPLCON0,$0302` (the standard "blank below the picture" idiom) meant a
   walk that honoured `MOVE`s but skipped `WAIT`s landed on that trailing
   planes-off state rather than the visible band's `$C302`. This is what
   drove §8.1's "WAITs skipped" to "vertical `WAIT` honoured" — see the
   proposal for the current wording.
2. **Hires geometry was scaled by the wrong density.** `DIWSTRT`/
   `DIWSTOP` describe beam position in colour clocks, common to any
   resolution; the renderer scaled it by hires' *data-fetch* density (4
   px/colour clock) instead of a fixed output density, so every hires
   screen rendered at double width and double left offset, overrunning
   the canvas — the ball measured 200×101 (aspect 1.98) and the floppy
   graphic survived only as a 30px sliver at the edge.
3. **A false "content" reading was itself a bug.** A capture that looked
   like a mouse-pointer arrow followed by 239 rows of repeating glyph-like
   marks was not guest output. `draw_sprite0` read sprite 0's height from
   the chipset's `SPR0POS`/`SPR0CTL` registers, but real sprite DMA loads
   those from the sprite list in chip RAM (`SPR0PT`/`SPR0PT+2`)
   autonomously every frame — a DMA engine this machine does not have —
   so the chipset copies were left holding a stale, unrelated value
   (`$FF00`, from an earlier direct CPU write) that decoded to a
   ~255-line sprite. The real header in chip RAM reads `$0000`/`$0000`:
   sprite 0 is genuinely disabled here, and the shape was manufactured by
   the bug, not drawn by Kickstart. Fixed by reading the position/control
   header from chip RAM instead of the chipset registers — the same
   distinction the `--inspect` trap above records.

Regression tests: `sprite_height_comes_from_the_real_header_in_ram_not_the_stale_ctl_register`
(`crates/machine-core/src/render.rs`) programs a genuine one-line sprite
in RAM while leaving the stale chipset register in place and asserts
only the real line draws; `crates/machine-hosted/tests/real_rom.rs`'s
`kickstart_3_2_2_a1200_screenshot_shows_the_boot_screen` (skip-when-absent,
`--ignored`, since the ROM is user-supplied and non-redistributable)
decodes the actual PNG and asserts drawn, multi-colour content rather
than a specific picture, so a legitimate palette or layout change can't
break it.

### AROS 68k: the "Waiting for bootable media" screen, not a stall

```
./target/release/machine-hosted \
  --rom assets/aros/aros-amiga-m68k-rom.bin \
  --ext-rom assets/aros/aros-amiga-m68k-ext.bin \
  --screenshot aros.png --screenshot-frame 400 \
  --max-frames 500 --max-instructions 300000000
```

AROS was never stalling — the earlier flat-fill finding mistook a
renderer gap for a guest one. Both this machine and the Copperline oracle
sit at the identical exec idle PC `$00FE8B88`, with byte-identical serial
narration, in the same healthy "waiting for bootable media" state.
`dosboot.resource` puts up its cat-eyes boot logo, the AROS wordmark,
"Waiting for bootable media" and device icons — a 4-plane hires
interlaced screen (`BPLCON0 = $C204`, `DDFSTRT/STOP = $3C/$D0`,
`BPL1MOD/BPL2MOD = $50`) — with **no boot device at all**, then parks the
whole system in `Wait`.

Two renderer bugs stood between the original flat reading and that
picture:

1. **Fetch width was computed one word short.** Bitplane fetch runs in
   8-colour-clock units; the previous `span/4 + 2` arithmetic is only
   right when `DDFSTOP` lands on a unit boundary. Kickstart's `$D4` does;
   AROS's `$D0` does not, so AROS came out one word short per row — a
   two-byte shortfall that compounds down the frame into diagonal shear.
   Diagnosed by decoding a chip-RAM dump at both widths: 40 words gives
   the pristine cat-eyes logo, 39 reproduces the garbage exactly.
2. **Interlace drew both fields from one pointer set.** AROS's screen is
   interlaced 4-plane hires, and the renderer originally advanced one
   continuous bitplane pointer for the whole frame, so the picture
   appeared twice down the frame with noise between (the pointer running
   past the image into unrelated memory). Fixed by keeping one pointer
   set per field, each advancing by its own stride and modulo — the
   interlace-as-two-interleaved-fields behaviour proposal §8.1 now
   documents as in scope.

One incidental finding worth keeping for future frame-timing work:
`COLOR00` is not perfectly stable even while AROS is otherwise idle — it
read `$0EF9` at every frame sampled from 100–600 in one run, then `$0111`
once that run's *final* frame (601, right at its own `--max-frames`
boundary) was inspected. The picture's *presence* does not flicker; a
register sampled exactly at a run's last frame can be caught mid-update.
The regression test below deliberately avoids asserting a specific colour
for this reason and captures well clear of any `--max-frames` boundary.

Regression test: `crates/machine-hosted/tests/real_rom.rs`'s
`aros_68k_screenshot_shows_boot_screen_content_without_boot_media` runs
unconditionally (the AROS ROM pair is vendored in-repo,
`assets/aros/`, freely redistributable per `assets/aros/PROVENANCE.md`),
decodes the PNG, and asserts thousands of non-background pixels across at
least 8 distinct colours — loose enough to hold across future renderer
changes while still failing hard on the old "nothing drawn at all"
reading.

## Running the tests

```
cargo test -p machine-core render::tests::sprite_height_comes_from_the_real_header_in_ram_not_the_stale_ctl_register
cargo test -p machine-hosted --test real_rom aros_68k_screenshot_shows_boot_screen_content_without_boot_media
cargo test -p machine-hosted --test real_rom kickstart_3_2_2_a1200_screenshot_shows_the_boot_screen -- --ignored
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
