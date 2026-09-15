# Scoping the `rtgboard` P96 `.card` driver

**Status:** scoping note, written 2026-09-15 to open the driver session
below. Executed the same day: the driver landed and reached first light.
See `docs/rtgboard-protocol.md` and `docs/device-ledger.md` for the
verified result; this note is kept as-written for the reasoning that
shaped it.

**Context:** `docs/rtgboard-protocol.md` (the register contract the
driver programs); `docs/adr-0002-rtg-on-generic-display-hardware.md`
("Resolved" section — the callback set and the three carried-forward
costs); `~/src/amirfb` (BSD 2-Clause, the project owner's own **working
P96 virtual card driver**, verified live against real Picasso96
`rtg.library` 40.3945 — the primary reference, freely copyable unlike
the GPL oracles); `~/src/p96-experiment` (`docs/boardinfo-abi.md`,
`docs/rtg-gotchas.md`).

---

## 1. The headline: this driver needs (almost) no DiagArea boilerplate

`hostblk` and `input` both deliver their guest code by DiagArea
injection from card ROM, and the natural assumption was that this
driver reuses that machinery. It does not, in the first increment:

- A P96 card driver is an ordinary **Exec AUTOINIT library on disk**
  (`LIBS:Picasso96/rtgboard.card`), loaded by `rtg.library` when a
  monitor icon in `DEVS:Monitors/` carries a matching `BOARDTYPE`
  tooltype. That is how `Graffity.card` reaches this machine today, and
  how `amirfb.card` reaches its dev VM.
- The board's AUTOCONFIG identity already exists host-side with
  `er_InitDiagVec = 0` (`rtgboard-protocol.md` §1) — nothing about the
  first driver increment changes the host at all.
- The 1,648-line `hostblk-diagrom.s` / 534-line `pktport-diagrom.s`
  conventions (two addressing regimes, the `da_BootPoint` copy gate,
  the vasm `dc.w` spaces trap) are therefore **not needed yet**. They
  become relevant only in a later "card carries its own driver"
  increment, for which `pktport-diagrom.s` is the proven template
  (copy a blob out of the card window, build a seglist) — deliberately
  out of scope here, same as bootability was for pktport's first
  increment.

What *is* reused from the existing m68k stack: the amiga-gcc toolchain
(`/opt/amiga/bin`, the same one that builds `m68k/hello`), the
`scripts/build-*.sh` shape, and amibake as the delivery mechanism (§4).

## 2. What the driver actually is

From ADR 0002's resolved list and amirfb's live verification:

- Two library entry points (`FindCard`, `InitCard`), **fourteen
  mandatory vectors** (`SetGC`, `SetPanning`, `SetSwitch`, `SetDisplay`,
  `SetColorArray`, `SetDAC`, `CalculateBytesPerRow`, `CalculateMemory`,
  `GetCompatibleFormats`, `ResolvePixelClock`, `GetPixelClock`,
  `SetClock`, `SetMemoryMode`, `WaitVerticalSync`), ~20 capability
  fields. **Zero render/sprite/planar vectors** — P96 pre-installs its
  `*Default` CPU renderers before `InitCard` runs, so declining
  everything draws a full desktop (verified live by amirfb).
- Vector-call convention: every vector takes `a0 = struct BoardInfo *`;
  `a6` is **not** a valid library base inside vectors (per-instance
  state lives in `BoardInfo.CardData[16]`). `FindCard`/`InitCard` are
  ordinary library calls and do get `a6`.
- Mapping to this board's registers is direct: `FindCard` locates the
  board via `expansion.library`/`FindConfigDev` (manufacturer `$07DB`,
  product `4`), sets `MemoryBase` = window + `0x100000` and
  `MemorySize` from the `VRAM_BYTES` register (never hardcoded);
  `SetGC` stages `SET_WIDTH/HEIGHT/FORMAT/STRIDE/FB_OFFSET`, writes
  `COMMIT`, checks `STATUS` — synchronous, no interrupt server needed
  at all (protocol §8), which removes the entire INT2 machinery
  `hostblk`'s driver needed.
- Format mapping is a named driver responsibility (protocol §6):
  `RGB_565` → `RGBFB_R5G6B5`, and the two 32bpp formats checked against
  a real `Picasso96API.h` rather than guessed — the black-screen
  channel-order trap is documented in both p96-experiment and the
  protocol doc.

amirfb's `src/card/` is 2,277 lines total (2,004-line `amirfb_card.c`,
a 9-line `libstart.S`, small libc shims) — but much of that is RFB
server plumbing and hard-won history comments. A minimal rtgboard
driver, with amirfb as the template and no network half, is plausibly
**800–1,200 lines of C** plus build glue. amirfb also vendors the P96
SDK headers (`include/p96sdk/` — `boardinfo.h` et al.), solving the
"where do the headers come from" question.

## 3. The trap amirfb already paid for: mode provisioning

The single most valuable finding in amirfb (its issues #2/#28/#44,
confirmed against the official iComp reference driver):

- Building `struct LibResolution`/`ModeInfo` entries and adding them to
  `bi->ResolutionsList` in `InitCard` gets a mode *enumerable and
  openable* — *but the `SetGC`/`SetPanning`/pixel-clock activation
  vectors never fire for it.* Only a mode sourced from
  **`Devs:Picasso96Settings`** activates properly. The iComp SDK's own
  shipped reference driver (`PiccoloSD64.card.asm`) has zero
  `ResolutionsList` references; MNT's ZZ9000/VA2000 drivers ship a
  static settings file in their installers.
- Consequence: this driver ships a **statically generated
  `Picasso96Settings` file** (amirfb's `tools/make_settings.py` is the
  working generator to port or reuse), and never touches
  `ResolutionsList`.
- Second-order trap, also verified live: a settings-file entry reusing
  the same DisplayID base as a stale `ResolutionsList` registration
  hangs boot at an unrenderable requester. Don't do both.
- Also carried from ADR 0002: vector signatures are versioned
  (`SetPanning`/`CalculateBytesPerRow` grew arguments at P96 3.3.1+),
  so the build targets the P96 version amibake actually installs.

## 4. Delivery: three files onto the boot image, via amibake

The amibake `picasso96-3` recipe already installs per-choice
`Picasso96Settings` variants and `DEVS:Monitors/Picasso96`, so the
mechanism exists; what this driver adds is either a small local recipe
or manifest-level file support for:

1. `SYS:Libs/Picasso96/rtgboard.card` — the built driver.
2. A monitor icon (or tooltype edit) with `BOARDTYPE=rtgboard` so
   `rtg.library` loads it.
3. `SYS:Devs/Picasso96Settings` covering this board's modes (§3) —
   replacing, not adding to, the Graffity-generated one on a
   rtgboard-flavoured test image.

**The catalog/settings coupling is the one real design wrinkle.** The
board's mode catalog is a *runtime host flag* (`--rtgboard WIDTHxHEIGHT`,
one entry), while `Picasso96Settings` is a *static file baked into the
image*. They must agree, or the driver advertises a mode the board's
`COMMIT` will refuse. First-increment answer: fix one blessed mode
(e.g. 640x480, matching the existing RTG baselines) in both the amibake
manifest and the runner flags, and record the coupling; a
settings-generated-from-catalog step is a later refinement.

## 5. Open decisions for the driver session

1. **`BoardType`.** Assigned by iComp; amirfb masquerades as
   `BT_uaegfx`, and ADR 0002 notes stock `rtg.library`'s behaviour with
   an unassigned value is unknown. Default: copy amirfb's masquerade,
   record it as a stand-in like the manufacturer ID.
2. **Pixel format for first light.** The board offers `RGB_565` /
   `RGBX_8888` / `BGRX_8888`; amirfb's proven path is R5G6B5 16-bit.
   Default: `RGB_565` first (known-good in the template), 32bpp after.
3. **Source location and language.** `m68k/rtgboard-card/` in C with
   amirfb's `libstart.S` pattern — the project's first gcc-built guest
   *library* (hello is an executable; the ROMs are vasm). Confirm the
   AUTOINIT/ROMTag boilerplate compiles under the pinned
   m68k-amigaos-gcc 6.5.0b.
4. **amibake support.** Whether installing the three files needs a new
   local recipe, or manifest-level `files` support (the same gap the
   2026-09-05 notes flagged for auto-start fragments).

## 6. What first light looks like, and what stays out

Achievable single-session target, mirroring amirfb's own verified
sequence: `P96CheckBoards` reports the board; a screen opens through
`rtgboard.card`; Workbench renders into this board's VRAM; the existing
screenshot present path (which already walks rtgboard VRAM) captures
it as a new baseline. AmiPilot semantic assertions follow once a
desktop is up.

Out of scope for the first increment, recorded so it isn't
rediscovered: the pointer/input interaction (protocol §9 — soft-sprite
pointer plus the native `input` card's absolute events is the
combination test, and the `input` m68k driver doesn't exist yet);
palette/CLUT modes (the board has no palette registers by design);
acceleration (by design); DiagArea/ROM-resident delivery (§1); and
demoting Cirrus/Graffity in the device ledger, which happens only after
this driver is the verified default path.
