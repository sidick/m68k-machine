# `rtgboard` register and mode-programming contract

**Status:** host side implemented (`crates/machine-core/src/rtgboard.rs`),
wired into `MachineBus::with_rtgboard` and `machine-hosted`'s `--rtgboard
WIDTHxHEIGHT` flag, with the screenshot present path (`crates/machine-hosted/
src/screenshot.rs`) able to walk this board's VRAM the same way it already
does Graffity's. **A P96 `.card` driver now exists** at
`m68k/rtgboard-card/` — disk-loaded and AUTOINIT'd (there is still no
DiagArea boot ROM; that scope line is unchanged), delivered onto a
bootable image by `scripts/patch-rtgboard-hdf.sh`. This is verified
end to end (2026-09-15): a real Workbench desktop rendered through this
board and this driver, not a host-side test standing in for one --
`crates/machine-hosted/tests/real_rom.rs`'s
`kickstart_3_2_2_a1200_workbench_renders_through_the_rtgboard_card_driver`
pins it as a regression test. §10 records the driver-side facts a future
reader needs.

**Context:** `docs/adr-0002-rtg-on-generic-display-hardware.md` (the
decision this implements — read it first); `docs/device-ledger.md` (where
this device sits, and what it demotes Cirrus/Graffity to); `docs/
hostblk-protocol.md` and `docs/input-protocol.md` (the sibling documents
this one's shape and section numbering follow); `~/src/amirfb` (BSD
2-Clause, a working Picasso96 virtual card driver, whose header comment
is the single most useful thing to read before writing this board's
`.card`); `~/src/p96-experiment` (`docs/boardinfo-abi.md`,
`docs/rtg-gotchas.md`).

---

## 1. Identity

One Zorro III AUTOCONFIG board:

| Field | Value |
|---|---|
| `er_Manufacturer` | `0x07DB` (2011) — the same NDK 3.2 `libraries/configregs.h` reserved "hacker" ID `hostblk`/`input` use. `hostblk.rs`'s module docs carry the full story of why `0xFFFF` was tried and rejected by real Kickstart 3.2.2. Still a stand-in: a real registered number is needed before this ships on hardware. |
| `er_Product` | `4` — distinct from `mirage` (`0`), `hostblk` (`1`), `fastram` (`2`) and `input` (`3`). |
| `er_Type` | `ERT_ZORROIII` (extended-table code 0 → 16 MB window; no size bits of its own, same as every other Zorro III board here) |
| `er_Flags` | `ERFF_ZORRO_III \| ERFF_EXTENDED` |
| `er_InitDiagVec` | `0` — **no DiagArea in this increment.** `ERTF_DIAGVALID` is unset, the same posture `input`'s own first, driver-less increment took. |
| Window size | 16 MB. The register file occupies the first `0x4C` bytes; VRAM starts at offset `0x100000` (1 MB in) and runs to the end of the window — at most 15 MB reachable through this aperture. The gap between the register file and VRAM is unimplemented board space (reads `0`, writes discarded), room for the register file to grow, the same posture `hostblk::ROM_BASE`'s placement documents. |

## 2. Why no accelerator: the enabling fact this board relies on

ADR 0002 states it, and this project has now verified it rather than
assumed it: **a P96 driver that overrides no render vector still draws a
full desktop.** Picasso96's core installs its own CPU (`*Default`)
renderer into every accelerable slot *before* the driver's `InitCard`
runs; a driver accelerates by overwriting slots, not by negotiating.
Decline everything and P96 draws straight into VRAM with the CPU.

So this board carries no BitBLT engine, no RAMDAC, no palette hardware,
no acceleration hooks of any kind — a driver for it needs exactly the
fourteen mandatory `BoardInfo` vectors ADR 0002's "Resolved" section
lists (`FindCard`/`InitCard`, `SetGC`, `SetPanning`, `SetSwitch`,
`SetDisplay`, `SetColorArray`, `SetDAC`, `CalculateBytesPerRow`,
`CalculateMemory`, `GetCompatibleFormats`, `ResolvePixelClock`,
`GetPixelClock`, `SetClock`, `SetMemoryMode`, `WaitVerticalSync`) plus
about twenty capability fields, none of which need to do more than
answer honestly from this board's own register file. Zero render,
sprite or planar-mask vectors are required.

## 3. Mode advertisement: a catalog, not a negotiation

`MachineBus::with_rtgboard` takes a **caller-supplied, borrowed** slice
of `ModeDescriptor { width, height, format }` — the board never
synthesises its own list. This lets the same board type honestly express
two different stories:

- A host with real runtime modesetting (a Linux/DRM board layer, ADR
  0001's option B) hands this board a rich catalog.
- A host with a mode fixed at boot (UEFI GOP after `ExitBootServices`,
  ADR 0001's option A/bare-metal path) hands this board a **one-entry**
  catalog — the boot-time mode, and nothing else.

`machine-hosted --rtgboard WIDTHxHEIGHT` always builds a one-entry
catalog, deliberately: it is the honest shape of what this project's own
Phase 5 hardware will actually be able to offer, not a menu this host
doesn't really have. A driver enumerates the catalog with `MODE_COUNT`/
`MODE_INDEX`/`MODE_WIDTH`/`MODE_HEIGHT`/`MODE_FORMAT` (§4) before ever
attempting `COMMIT`; a `GetCompatibleFormats`/mode-list callback built
against this contract should report **exactly** the catalog contents, no
more and no less — advertising a mode this board will refuse is worse
than advertising nothing, since it invites a driver to hand the guest a
resolution that will never actually apply.

## 4. Register map

Same idiom `hostblk.rs`/`input.rs` established: one hot byte per
4-byte-aligned slot, `u32` registers as four big-endian byte lanes.
Anything not listed reads `0` and discards writes.

| Offset | Register | Width | Access | Notes |
|---|---|---|---|---|
| `0x00` | `MODE_COUNT` | u32 | R | number of catalog entries |
| `0x04` | `MODE_INDEX` | byte | W | selects which catalog entry the three registers below describe |
| `0x08` | `MODE_WIDTH` | u32 | R | `catalog[MODE_INDEX].width`, or `0` if out of range |
| `0x0C` | `MODE_HEIGHT` | u32 | R | `catalog[MODE_INDEX].height`, or `0` if out of range |
| `0x10` | `MODE_FORMAT` | byte | R | `catalog[MODE_INDEX].format`, or `0xFF` (`INVALID`) if out of range |
| `0x14` | `SET_WIDTH` | u32 | RW | candidate width — plain scratch, no effect until `COMMIT` |
| `0x18` | `SET_HEIGHT` | u32 | RW | candidate height |
| `0x1C` | `SET_FORMAT` | byte | RW | candidate pixel format (§6) |
| `0x20` | `SET_STRIDE` | u32 | RW | candidate stride, in bytes — **not derivable from width**, §5 |
| `0x24` | `SET_FB_OFFSET` | u32 | RW | candidate framebuffer offset within VRAM, in bytes |
| `0x28` | `COMMIT` | byte | W | any write validates and applies (or rejects) the `SET_*` registers as the new mode (§7) |
| `0x2C` | `STATUS` | byte | RW | write-1-to-clear; bit 0 = `REJECTED`, bit 1 = `APPLIED` (mutually exclusive per commit); **ungated**, unlike `input::INT_STATUS` — there is no queue behind this to strand |
| `0x30` | `CUR_WIDTH` | u32 | R | the currently applied mode's width, `0` if none ever applied |
| `0x34` | `CUR_HEIGHT` | u32 | R | the currently applied mode's height |
| `0x38` | `CUR_FORMAT` | byte | R | the currently applied mode's format, or `0xFF` if none applied |
| `0x3C` | `CUR_STRIDE` | u32 | R | the currently applied mode's stride |
| `0x40` | `CUR_FB_OFFSET` | u32 | R | the currently applied mode's framebuffer offset |
| `0x44` | `VRAM_BYTES` | u32 | R | total VRAM attached to this board — **read this, do not hardcode a limit** |
| `0x48` | `VERSION` | u32 | R | protocol version; `1` for this document |
| `0x100000` onward | VRAM aperture | — | RW | the linear framebuffer itself; the CPU reads and writes pixels here directly, no register indirection |

Driver-side mode-set shape:

```
/* 1. enumerate */
for (i = 0; i < MODE_COUNT; i++) {
    MODE_INDEX = i;
    /* MODE_WIDTH / MODE_HEIGHT / MODE_FORMAT now describe catalog[i] */
}

/* 2. stage a candidate the driver has decided to use */
SET_WIDTH     = width;
SET_HEIGHT    = height;
SET_FORMAT    = format;
SET_STRIDE    = CalculateBytesPerRow(width, format);  /* driver's own formula, not width * bpp */
SET_FB_OFFSET = 0;   /* or wherever this driver places the visible frame */
COMMIT        = 1;

/* 3. check the result */
if (STATUS & STATUS_APPLIED) {
    /* CUR_* now mirror what was just staged */
} else {
    /* STATUS & STATUS_REJECTED -- nothing changed; CUR_* still describe
     * whatever mode (if any) was applied before this COMMIT */
}
STATUS = 0xFF;   /* write-1-to-clear, ungated */
```

## 5. Stride is not width

ADR 0002 and `docs/adr-0002-rtg-on-generic-display-hardware.md`'s own
citation of GOP both make the same point: `PixelsPerScanLine` (GOP) or
`PixelsPerRow`/`BytesPerRow` (any real host surface) is reported
*separately* from width and is usually padded — alignment, tiling, or
simply "the hardware rounds up". `SET_STRIDE`/`CUR_STRIDE` therefore
carry the host's real value as their own field; nothing in this board
ever derives a stride from width. A driver's `CalculateBytesPerRow`
vector should compute a real answer (its own formula, honouring whatever
alignment this host's actual surface needs) and supply it here, not
assume `width * bytes_per_pixel`.

The one constraint this board enforces is the opposite direction: a
stride *shorter* than one real row (`width * bytes_per_pixel(format)`)
is refused — see §7.

## 6. Pixel format: explicit, because "close" gives a black screen

Both `~/src/p96-experiment`'s `docs/rtg-gotchas.md` and ADR 0002 name the
same failure mode: the wrong `RGBFTYPE` (`R8G8B8A8` vs. `A8R8G8B8`) gives
a black screen with byte-correct VRAM, because two formats can agree on
bit depth and disagree only on channel order — a bug that produces no
error, no crash, just nothing on screen. This board's three formats are
therefore named by byte order, not just depth:

| Value | Name | Layout |
|---|---|---|
| `0` | `RGB_565` | 16 bits/pixel, **big-endian** `RRRRRGGGGGGBBBBB` — read with a 16-bit big-endian load |
| `1` | `RGBX_8888` | 32 bits/pixel, byte order low→high address = R, G, B, pad — UEFI `PixelRedGreenBlueReserved8BitPerColor` (UEFI Spec §12.9) |
| `2` | `BGRX_8888` | 32 bits/pixel, byte order low→high address = B, G, R, pad — UEFI `PixelBlueGreenRedReserved8BitPerColor` |
| `0xFF` | `INVALID` | sentinel only — "no such catalog entry" / "no mode applied", never a real format a driver programs |

Naming the two 32bpp formats after UEFI's own `EFI_GRAPHICS_PIXEL_FORMAT`
enum is deliberate, not decorative: it is the concrete answer to "what
does the guest talk to when the host display is generic" ADR 0002 poses.
A future Phase 5 board layer can advertise **exactly** the format its
real GOP framebuffer reports, with no translation step in the middle for
either side to get subtly wrong. A driver mapping these onto P96's own
`RGBFTYPE` enum should map `RGBX_8888` to `RGBFB_R8G8B8A8` and
`BGRX_8888` to `RGBFB_B8G8R8A8` (or whichever of P96's pairs matches —
check against a real `Picasso96API.h`, the mapping is a driver
responsibility this board's protocol does not perform) rather than
guessing from depth alone.

## 7. Hostile input: what `COMMIT` checks, in order

`COMMIT` validates the staged `SET_*` registers and applies them, or
rejects and leaves `CUR_*` untouched, on the **first** failure below —
never a partial apply, never a panic:

1. **Unknown mode.** `(SET_WIDTH, SET_HEIGHT, SET_FORMAT)` must exactly
   match one catalog entry. This board does not interpolate or
   approximate a requested geometry — §3's "express exactly these modes
   and no others" is the whole point of the catalog existing.
2. **Stride shorter than a row.** `SET_STRIDE < SET_WIDTH *
   bytes_per_pixel(SET_FORMAT)`. Checked with `checked_mul` throughout,
   so a hostile width/format combination cannot overflow into looking
   valid.
3. **Framebuffer past the end of VRAM.** `SET_FB_OFFSET + SET_HEIGHT *
   SET_STRIDE` must fit within the real, attached VRAM length (`checked_
   mul`/`checked_add` throughout) — not `VRAM_BYTES` alone, the actual
   backing store, the same "ask the real backing store, never assume a
   range" discipline `hostblk`'s buffer-bounds check follows.

An unrecognised `SET_FORMAT` (not one of §6's three real values) fails
check 2 immediately (`bytes_per_pixel` is undefined for it), so it can
never reach check 3 by accident.

`MODE_INDEX` past `MODE_COUNT` is handled the same way `hostblk`/`input`
handle an out-of-range unit/index: the three read-only registers that
describe it come back as `0`/`0`/`INVALID` rather than reading past the
catalog or panicking. There is no way to write an invalid `MODE_INDEX`
that corrupts anything, because nothing downstream trusts it for
anything but a bounds-checked lookup.

## 8. Why there is no interrupt, unlike `hostblk`/`input`

Both sibling cards need one: `hostblk`'s doorbell defers real I/O across
ticks (a slow host disk, or a future OPFS backend, cannot always answer
synchronously), and `input`'s queue is filled by the *host*, asynchronous
to whatever the guest happens to be doing. Neither condition holds here.
`COMMIT` validates four integers against a borrowed slice and either
applies them or doesn't, entirely within the one bus access that writes
`COMMIT` itself — there is no asynchronous boundary to defer across, and
therefore nothing for INT2 to usefully announce. A driver commits a mode
and reads `STATUS` back in the very next few instructions; no interrupt
register exists on this board at all.

## 9. The mouse pointer is not free (driver-side, noted here early)

With no hardware sprite, P96's core soft-renders the pointer straight
into this board's own framebuffer, and Intuition routes button events by
where it currently believes the pointer is — so a usable P96 screen on
this board needs the native `input` card's absolute-position events
(`docs/input-protocol.md` §6-§7: `IECLASS_NEWPOINTERPOS`/
`IESUBCLASS_PIXEL`, a resolved `struct Screen *`, `IEQUALIFIER_
RELATIVEMOUSE` on synthetic button events) actually reaching Intuition
first, or a click on an RTG screen driven by this board will land nowhere
useful. This is driver-side work with no host-side equivalent in this
increment — `SoftSpriteFlags` and the render-vector-declines-everything
posture §2 describes both need to agree with whatever pointer shape
Intuition expects — and it is the one point where this board's own
protocol and the input card's protocol are not independent: an RTG
screen on this board plus the native input card was the combination that
would eventually need testing together, once both a `.card` driver and
an `input` driver existed.

That combination is now proven (2026-09-15), not merely built: P96
soft-renders the pointer into rtgboard VRAM and moves it on scripted
`MOVE`s, and a scripted double-click at the SYS icon opens the SYS
drawer on the RTG screen, pinned by
`scripted_pointer_and_double_click_work_on_the_rtgboard_rtg_screen`. See
§10 for the measured evidence.

## 10. Verification

Unit tests in `rtgboard.rs` cover mode programming and readback (a
catalog mode applies and every `CUR_*` register reflects it exactly,
including a padded stride reported unchanged rather than recomputed), an
unsupported mode being refused rather than half-applied (both "never
applied at all" and "a later rejection leaves an earlier successful
commit untouched"), VRAM aperture reads and writes (including that the
register file and the VRAM aperture do not alias each other), and hostile
input failing cleanly: an out-of-range `MODE_INDEX`, a stride shorter
than a row, a framebuffer offset past the end of VRAM, an unrecognised
pixel format, and an offset/stride combination large enough to overflow
`u32` arithmetic if it were not `checked_*` throughout.

`crates/machine-core/src/lib.rs`'s own bus-wiring tests additionally
prove: a machine with no `--rtgboard` is completely unaffected (no chain
entry, no routing branch — the same guarantee `hostblk`/`input` give),
the board coexists with the input card at a distinct AUTOCONFIG chain
index, and a mode committed and a pixel pattern written through the
*real* bus (not `rtgboard.rs`'s own flat-array unit tests) round-trips
correctly — proving the register file and VRAM aperture share one
AUTOCONFIG window without aliasing once real address placement is
involved.

`crates/machine-hosted/src/screenshot.rs` additionally proves the thing
this increment's brief specifically asked for: a known pixel pattern
written into this board's VRAM through the register interface, with no
guest and no driver involved, comes out through the *real* screenshot
present path (`capture()`) as the expected pixels in a captured frame —
one red pixel against a black background, distinct-colour and
non-background-pixel counts both confirming it. This was, at the time it
was written, real evidence the VRAM-to-pixels path this board's eventual
driver would rely on already worked correctly -- superseded now by the
end-to-end driver evidence below, but left in place because it still
independently confirms the same path with no driver involved at all.

**Driver-side verification (2026-09-15).** The P96 `.card` driver
(`m68k/rtgboard-card/`) reached first light: `crates/machine-hosted/
tests/real_rom.rs`'s
`kickstart_3_2_2_a1200_workbench_renders_through_the_rtgboard_card_driver`
boots a patched HDF (`scripts/build-rtgboard-card.sh` then
`scripts/patch-rtgboard-hdf.sh`) with `--rtgboard 640x480 --rtgboard-format
rgb565`, and gets a real 640x480 grey Workbench desktop -- title bar,
`RAM Disk`/`SYS` icons, window chrome -- confirmed both by the driver's
own serial narration (`FindCard`, `InitCard`, `SetGC ... committed:
APPLIED`, `SetPanning ... committed: APPLIED`) and by decoding the
captured screenshot. A few facts worth recording here for whoever next
touches this driver or this register file:

- **`CUR_*` keeps no shadow state.** The driver does not cache the
  applied mode anywhere of its own; §4's `CUR_WIDTH`/`CUR_HEIGHT`/
  `CUR_FORMAT`/`CUR_STRIDE`/`CUR_FB_OFFSET` registers *are* the truth the
  driver reads back after every `COMMIT`, exactly as designed.
- **Byte registers' hot byte, from the 68k side, is `offset+3`.** §4's
  "one hot byte per 4-byte-aligned slot" idiom means a `byte`-width
  register such as `MODE_INDEX` (`0x04`) or `COMMIT` (`0x28`) has to be
  written to the *last* byte of its 4-byte slot (`0x07`, `0x2B`, ...) from
  68k code, not its first -- a big-endian bus fact easy to get backwards
  once when first wiring a driver's register accessors, and silent when
  gotten wrong (the write lands on a byte the register file discards, so
  nothing visibly breaks until the next register read shows stale data).
- **The `FakeNativeModes`/CLUT trap.** The first first-light attempt
  copied the Graffity image's `FakeNativeModes=Yes` monitor tooltype (the
  mechanism that gets Workbench onto RTG with no prefs step there) -- but
  fake native modes are fabricated as 8-bit CLUT screens, Graffity has
  palette hardware for those, and this board deliberately has none. P96
  rendered 8-bit pen indices into the framebuffer and this board scanned
  them straight back out as RGB565 -- a black-dominant, green-tinted
  screen (measured: 21 distinct colours, 13,637/307,200 non-background
  pixels), not a crash or a rejected `COMMIT`. The fix is both halves,
  not either alone: the monitor icon ships `FakeNativeModes` inactive
  (`tools/rtgboard/make_monitor_info.py` -- do not re-enable it), and
  `scripts/patch-rtgboard-hdf.sh` writes `Prefs/Env-Archive/Sys/
  ScreenMode.prefs` pinning Workbench to `DisplayID 0x60001102` (this
  driver's advertised 640x480/16-bit mode) before Workbench ever asks P96
  to pick.
- **`--rtgboard-format rgb565` is a required flag, not a default.**
  `machine-hosted --rtgboard` defaults to `rgbx8888`; with that default
  the board's one-entry catalog holds a format this driver never proposes
  to `GetCompatibleFormats`, so every mode the driver tries to set gets
  rejected (`STATUS & STATUS_REJECTED`, never `APPLIED`) -- a live
  instance of §6's "close gives a black screen" point, except here it is
  a rejected commit rather than a wrong-but-applied one. The real-ROM
  test above asserts the serial log carries no `"committed: REJECTED"`
  for exactly this reason.

**Pointer-and-click combination verified (2026-09-15).** §9's open
combination — the native `input` card driving a pointer and clicks on a
P96 RTG screen backed by this board — is now proven against real
Kickstart 3.2.2, not just built: `crates/machine-hosted/tests/
real_rom.rs`'s
`scripted_pointer_and_double_click_work_on_the_rtgboard_rtg_screen` boots
the patched HDF with `--rtgboard 640x480 --rtgboard-format rgb565
--input-script` and confirms all three links in §9's chain hold. P96
declines the hardware sprite and soft-renders the pointer into this
board's VRAM (`SoftSpriteFlags = RGBFF_R5G6B5`, stub `SetSprite*`
vectors): screenshots at frames 5200 and 5420 show the pointer's
57-pixel, three-colour image entirely inside a 16x16 box at exactly the
commanded `MOVE` coordinate, first (100,100) then (500,380), and nowhere
else on the desktop. `IntuitionBase->ActiveScreen` resolution against
this RTG screen works unmodified — `--inspect` reports `MouseX 42  MouseY
73` exactly as commanded, not doubled the way `input-protocol.md` §13's
legacy hires readback is, with `EVENT_COUNT 0  EVENT_OVERFLOW 0`. And a
scripted double-click at the SYS icon (42,73) opens the SYS drawer window
on the RTG screen, measured by white pixels rising 9,093 → 13,181 and
black 6,599 → 11,221 against a closed desktop — proof the
`IEQUALIFIER_RELATIVEMOUSE` trap did not resurface and that clicks route
by position on this screen exactly as on the legacy one. Neither driver
needed a single change; this was verification of an existing design, not
a fix, with the driver's serial narration staying clean throughout
(`FindCard`/`InitCard`/`SetGC APPLIED`/`SetPanning APPLIED`, zero
`REJECTED` commits).

## 11. What this increment does not include

- No DiagArea boot ROM. The P96 `.card` driver (`m68k/rtgboard-card/`)
  now exists and is disk-loaded/AUTOINIT'd instead -- see the Status
  paragraph above and §10's driver-side verification.
- No palette/CLUT support of any kind. ADR 0002 explicitly favours
  advertising direct-colour formats over CLUT to avoid the palette-
  expansion class of bug this project's own red-backdrop defect lived
  in; this board simply never grew palette registers rather than growing
  and then avoiding them.
- No acceleration hooks (§2) — by design, not as a gap to fill later.
  ADR 0002's plan is to add them only if profiling later shows CPU
  rendering is the bottleneck on real hardware.
- No mode-change-in-flight guest-visible signalling beyond `STATUS`
  itself — there is no equivalent of `TD_ADDCHANGEINT`/an interrupt a
  driver could wait on, per §8's reasoning that none is needed for a
  synchronous operation.
- A guest-visible baseline test now exists
  (`crates/machine-hosted/tests/real_rom.rs`'s
  `kickstart_3_2_2_a1200_workbench_renders_through_the_rtgboard_card_driver`,
  §10), proving a real Workbench desktop through this driver end to end,
  and a second real-ROM test in the same file,
  `scripted_pointer_and_double_click_work_on_the_rtgboard_rtg_screen`,
  now proves §9's pointer-and-click combination the same way. What does
  not yet exist is acceptance testing at the depth of
  `hostblk-protocol.md` §13's devsoak run or `input-protocol.md` §12's
  full guest-fixture plan — these are two baseline screenshot-and-serial-log
  tests, not a soak.
