# ADR 0002 — How RTG reaches a display we did not choose

**Status:** accepted 2026-09-06 — **option C**: build the virtual board
of option B as this machine's native display path, and keep option A's
Cirrus as the zero-install compatibility tier. A third possibility that
surfaced later, presenting a `uaegfx`-compatible interface so an existing
driver would bind, is **rejected**; see "The uaegfx exemplar is a dead
end" below. The "Unverified" section is now resolved and kept only as the
record of what was once open.

Originally recorded as open, because Phase 3 had just made the
emulated-silicon path work end to end, which is what made the
alternative worth stating precisely; and because one of its consequences
bears on ADR 0001, which remains open.

**Context:** proposal §8.2; roadmap Phases 3, 5; ADR 0001;
`device-ledger.md`, which records the Cirrus card's status as scaffolding
and what this ADR's option B would demote it to.

---

## The question

Phase 3 renders Workbench through a modelled Cirrus CL-GD5428 on a
Graffity board, Zorro II and Zorro III alike. Picasso96's own driver
programs the mode and paints through the chip's BitBLT engine, and this
machine draws the result. Nothing is installed on the guest that a
stock AmigaOS 3.2.2 setup does not already have.

That works because the host display is, today, a PNG file. On real
hardware at Phase 5 it is a UEFI GOP framebuffer or a DRM/KMS surface:
a linear buffer of a width, height, stride and pixel format chosen by
firmware, with no register interface, no blitter, and — under UEFI —
usually no ability to change mode once the machine is running.

So: what does the guest talk to when the host display is generic?

## What Phase 3 established

Worth stating plainly, because it constrains the answer. Picasso96
splits a thin board-specific `.card` over a shared `.chip`, and the
`.chip` is where the work is: on a plain Workbench desktop P96 paints
through the accelerator rather than with CPU writes, so before this
machine had a BitBLT engine, every draw was silently a no-op and VRAM
held nothing but the driver's own memory-sizing pattern.

The corollary matters more than the fix. **The guest-side driver
decides how much hardware the host must model.** A driver that paints
through an accelerator obliges us to implement one.

## Three tiers, sorted by what the guest already has

The tiers do not sort by capability — the generic board is technically
the better of the two RTG paths. They sort by what must be installed on
the guest before they work at all.

| Tier | Needs on the guest | Works when |
|---|---|---|
| Emulated silicon (Cirrus/Graffity) | nothing — P96 ships `Graffity.card` | we model that chip |
| Generic virtual board | our own `.card` | any host display |
| Planar chipset | nothing at all | always |

The third is not a failure mode. If no RTG driver binds, the guest
keeps using the chipset renderer and gets the 752x576 Workbench this
project already tests. That is the floor, it needs nothing from the
guest, and it is already implemented.

## Options

### A. Model more real silicon

Extend what Phase 3 did: keep emulating cards P96 already has drivers
for, and translate their register and blitter traffic onto whatever the
host framebuffer turns out to be.

Keeps the tier-1 property that nothing is installed on the guest, which
is genuinely valuable — it is why this project reached a real Workbench
desktop without writing a line of 68k.

Costs: a chip model per card, forever. Mode setting is limited to what
the modelled silicon can express, which is a poor fit for a host whose
mode was chosen by firmware and may not be changeable. Every driver
that paints through an accelerator obliges us to implement that
accelerator.

### B. A virtual board and our own P96 driver

The `uaegfx` model: rather than emulate real silicon, define a board
that exists only here — a linear framebuffer plus a small register or
mailbox interface — and write the P96 `.card` for it.

The enabling detail is that **P96's acceleration hooks are optional**.
A board that advertises no acceleration gets P96 rendering with the CPU
straight into the framebuffer, and only the mode-setting and
memory-layout side has to exist. That is a far smaller surface than the
BitBLT engine Phase 3 needed, and an emulated 68040 writing to host RAM
is fast enough for a desktop.

Two further properties fall out:

- **Advertise the host's own pixel format** rather than CLUT, and
  scanout becomes a copy with no palette expansion — the path where
  this project's red-backdrop defect lived.
- **The driver is our code**, MIT/Apache-2.0, rather than a dependency
  on a binary P96 happens to ship.

Costs: a 68k build (`~/src/amiga-gcc` with NDK 3.2 is already on the
development machine), and a file the user must install, which forfeits
the tier-1 property.

### C. Offer both, and let the guest choose

Put both boards on the AUTOCONFIG chain. Stock P96 finds manufacturer
2092 and binds `Graffity.card` to the Cirrus board; if our `.card` is
also installed, it binds the generic board too. Each driver claims only
the board it recognises, so the guest self-selects with no probing or
negotiation on our side, and a board that no driver claims simply sits
inert on the chain.

The wrinkle is that both binding at once presents P96 with two boards
and therefore two monitors, which is confusing rather than broken. Gate
it: one board by default, both behind a flag.

## Recommendation

**Option C, with B as the native path and A retained as the
zero-install one.** Not decided here — Phase 5 owns it — but this is
the shape the evidence points at.

The Cirrus work is not superseded by this. It is the compatibility tier,
it is the only tier that works against a completely untouched install,
and building it is what taught this project what P96 actually asks a
board to do.

Build the generic board unaccelerated first. If profiling later shows
CPU rendering is the bottleneck, add acceleration hooks incrementally;
the BitBLT engine already shows what those hooks are asked to do.

## Mode setting, and what it means for ADR 0001

This is the part that reaches outside Phase 5, and it is the reason this
ADR is being written now rather than at Phase 5.

Under UEFI you enumerate GOP modes and set one **before**
`ExitBootServices`. Afterwards the mode is effectively fixed. A
bare-metal board layer would therefore advertise the boot-time mode, or
a small fixed list, and refuse changes at runtime.

Under a Linux host, DRM/KMS gives real runtime modesetting, and a P96
mode change maps onto it directly.

So "can the guest change resolution without rebooting the machine?" is
answered differently by ADR 0001's option A and option B. That is a
user-visible behavioural difference, not an implementation detail, and
ADR 0001 does not currently record it among the costs of option A. It
should be weighed there.

Two smaller consequences, in the same direction:

- **Stride is not width.** GOP reports `PixelsPerScanLine` separately
  and it is frequently padded, so `CalculateBytesPerRow` must return
  the host's stride.
- **Do not let the guest write into the GOP framebuffer directly.** It
  is typically write-combining or uncached across PCIe, and 68k-
  granularity writes into it would be miserable. Keep guest VRAM in
  ordinary RAM and blit per frame.

When a requested mode cannot be delivered, present it at the **largest
integer scale that fits** the real framebuffer in both axes, centred,
with the remainder letterboxed. An integer factor replicates whole
pixels, so a chunky RTG desktop stays exactly as crisp as it started —
it is strictly better than a smaller centred image whenever the host
has the room. Work the factor out per axis and take the smaller of the
two rather than assuming.

What must not happen is a **fractional** scale: that resamples pixel
edges, and Workbench's text is what suffers. Fall back to the next
lower integer factor — 1x, centred at native size — rather than
stretching to fill.

Better still, advertise only modes the host can actually set, so P96
never asks for an impossible one. Scaling and letterboxing are the
safety net for when that list turns out to be wrong.

## Resolved: what was unverified, and what the answers were

All four were closed by material this ADR did not know about when it was
written — `~/src/p96-experiment` (the project owner's own clean-room
study of the P96 ABI, with CC-BY 4.0 SDK facts) and `~/src/amirfb` (BSD
2-Clause, also the owner's: **a working Picasso96 virtual card driver**
that serves a live Workbench over VNC).

- **"That P96 falls back to CPU rendering when a board advertises no
  acceleration."** Confirmed, and the mechanism is not a negotiation at
  all. Every accelerable operation has a `*Default` twin; the core
  installs its own CPU renderer into each and points the main slot at it
  *before the driver runs*, so a driver accelerates by overwriting slots
  rather than by advertising. Decline everything and it simply draws
  through the core. Verified live rather than only documented: AmiRFB's
  card touches no render vectors and works against real Picasso96
  (`rtg.library` 40.3945).
- **The exact `BoardInfo` callback set.** Resolved, and this ADR's
  indicative list was less than half of it. The real minimum is two
  library entry points (`FindCard`, `InitCard`), **fourteen** mandatory
  vectors — `SetGC`, `SetPanning`, `SetSwitch`, `SetDisplay`,
  `SetColorArray`, `SetDAC`, `CalculateBytesPerRow`, `CalculateMemory`,
  `GetCompatibleFormats`, `ResolvePixelClock`, `GetPixelClock`,
  `SetClock`, `SetMemoryMode`, `WaitVerticalSync` — and about twenty
  capability fields. Most are trivial for a virtual board. Zero render,
  sprite or planar-mask vectors are required.
- **GOP behaviour after `ExitBootServices`.** Still genuinely open, and
  still a Phase 5 question rather than one this decision depends on.
- **Redistribution.** Unchanged for our own `.card`, and now decisive
  against the alternative below.

Three costs this ADR missed, worth carrying forward:

- **Vector signatures are versioned.** `SetPanning` and
  `CalculateBytesPerRow` grew arguments at P96 3.3.1+, `AllocCardMem` at
  3.5.0, so a `.card` targets the version actually installed.
- **`BoardType` values are assigned by iComp**, and what stock
  `rtg.library` does with an unassigned one is unknown.
- **The pointer is not free.** With no hardware sprite the core soft-
  renders it, which needs `SoftSpriteFlags` set for our formats — and
  Intuition routes button events by pointer position, so a pointerless
  RTG screen receives no clicks at all.

## The uaegfx exemplar is a dead end

This ADR named "the `uaegfx` model" as the pattern to follow. The
*pattern* — a virtual board with its own driver — stands. The
**exemplar does not**, on three independent grounds, any one of which
would be sufficient.

- **Licence.** `uaegfx.card` is GPLv2+ and, worse, is not a
  redistributable file at all: it is a six-vector library synthesised by
  the emulator's own boot ROM. There is no way for a user to obtain it
  without running WinUAE or Amiberry, and this project could not ship it
  if there were.
- **It is not a register interface.** This ADR imagined "a linear
  framebuffer plus a small register or mailbox interface". In reality
  the card's vectors are line-A trap stubs whose work happens host-side,
  so compatibility would mean synthesising a UAE-style boot ROM,
  adding an A-line trap hook to the CPU core, and implementing the host
  side of the whole vector table — **including the acceleration the card
  does populate**. It does not even avoid the Cirrus-BitBLT class of
  work; it re-buys it in a different costume.
- **The only complete specification is GPL-3 source.** Reading WinUAE or
  Amiberry to implement the host half, then re-expressing it as MIT, is
  exactly the clean-room hazard this project cannot afford.

There is also a difference in kind, not just in cost. `uaegfx` is a
paravirtual escape hatch into UAE's emulator core, so adopting it would
mean emulating *Amiberry* — strictly further from "a display path
designed for this machine" than emulating a Cirrus is.

## Decision triggers

- Phase 5 board bring-up: what the first real display surface actually
  is (GOP mode list, or DRM connector set).
- Whether the CPU-rendered generic board is fast enough on the Rock 5B,
  measured rather than assumed — the same Phase 4 numbers ADR 0001 is
  waiting on.
- Whether ADR 0001 lands on a Linux host, which would make runtime
  modesetting available and reduce the pressure to advertise a fixed
  mode list.
