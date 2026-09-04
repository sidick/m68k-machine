# Blitter differential against Copperline

Proposal §12: "Blitter / renderer: host-side unit tests; differential
against Copperline (randomised ops + recorded Workbench traces)." The
unit tests landed first (21 tests in `crates/machine-core/src/blitter.rs`,
including all 256 minterms checked against an independently written truth
table). This document covers the differential half, implemented in
`crates/machine-hosted/tests/blitter_differential.rs`.

**Bottom line: the differential does not pass as of this writing.** It
found two real, reproducible divergences between `machine-core`'s
`Blitter` and Copperline 0.18.0 (the designated oracle), both in
`crates/machine-core/src/blitter.rs`. Per this task's ownership rules
that file is out of scope for this change — **these are reported here for
review, not fixed.** Everything else the differential checks agrees
exactly.

## Running it

```sh
cargo test -p machine-hosted --test blitter_differential -- --ignored --nocapture
```

Needs `copperline` (0.18.0 tested) on `PATH`. Skips cleanly (prints
`SKIP: ...`, does not fail) if it isn't found, the same convention
`crates/machine-hosted/tests/real_rom.rs` uses for its ROM-dependent
tests. It is `#[ignore]`d, so it never runs under a bare `cargo test`.

Two tests live in the file:

- `blitter_differential_against_copperline` — the actual differential.
  424 cases, ~1.7s wall clock (measured; dominated by process spawn and
  ~2,000 CCP round trips over loopback TCP, not emulation — Copperline's
  headless mode is unthrottled).
- `dmacon_gate_is_a_known_divergence_from_hardware` — a small, separate,
  *passing* test that pins the DMACON gap (below) as an intentional,
  understood finding rather than something silently assumed.

`cargo test -p machine-core` and `-p machine-hosted` (no `--ignored`)
stay exactly as fast as before this change; both bare-metal board builds
(`board-qemu-virt` aarch64, `board-qemu-q35` x86_64-uefi) are untouched.

## Mechanism

Copperline's blitter is cycle-exact and DMA-gated; `machine-core`'s is
synchronous and runs whenever a size register is written, ignoring
`DMACON` entirely (this is itself one of the findings below). Seeding
identical chip RAM and reading it back afterwards is straightforward on
both sides — the harder part is *driving the register writes themselves*.

Copperline's control protocol (CCP) `mem.write`/`mem.read` are explicitly
side-effect-free: per Copperline's own `cpu.rs` doc comments, they mutate
or read only RAM-backed regions and do not reach the custom-register I/O
window. There is no `custom.write` RPC either — CCP can *read* custom
registers (`custom.read`/`custom.dump`) but the only way to *write* one
through this protocol is to make the CPU do it, the same way real
hardware and graphics.library do.

So the harness hand-assembles a tiny 68k program per test case:

1. `MOVE.W #imm,ABS.L` pokes for every blitter register this test case
   uses (and `DMACON`, to enable `DMAEN|BLTEN` — see below), ending with
   the `BLTSIZE` write that arms the blit.
2. A `BTST #14,ABS.L` / `BNE.s` spin-wait on `DMACONR` bit 14 (`BBUSY`) —
   the same polling idiom graphics.library uses.
3. A settle margin (64 `NOP`s) — see "Copperline's `BBUSY`/final-write
   ordering" below.
4. A capture sequence for `BLTDDAT` (see "the `BLTDDAT` readback gap"
   below).
5. `BRA.s *` (branches to itself forever) as a halt marker.

The harness sets `PC`/`SR` via CCP's `regs.set` and calls `run_until{"pc":
<halt address>}`, which blocks until the CPU actually reaches the halt
marker — i.e., until the busy-wait has observed real completion, not a
fixed cycle count.

This program is injected **at a headless `--control` session's reset
state**, before any ROM code executes: `regs.set pc=...` overrides the
CPU's reset-vector fetch outright, so the AROS boot ROM built into
Copperline never runs and never touches chip RAM. `SR` is set to `$2700`
(supervisor, all interrupts masked — real hardware's own reset state),
and nothing enables Copper, bitplane, or CIA DMA, so chip RAM is touched
by nothing but the harness's own pokes and the blitter itself between
seeding and readback.

Comparisons: after `run_until` returns, the harness reads back the
destination memory window via `mem.read`, `BZERO` and the final channel
pointers via `custom.dump` (which reflects live register state
correctly — confirmed directly), and `BLTDDAT` via the CPU capture
sequence (see below, and note it's excluded from the pass/fail check for
a documented reason).

### The boot-ROM overlay gotcha

At reset, the low end of chip RAM (empirically, at least the first
1&nbsp;MiB — Copperline logged "1MiB ROM detected" for the AROS ROM it
boots headless sessions with) is overlaid with the boot ROM (`OVL`).
`mem.write` there silently writes zero bytes (its `written` count is
less than the request), and a hijacked `PC` pointed into it runs stray
AROS boot code instead of the test's program — which is exactly what
happened during development: the first attempt placed its program at
`$001000` and instead of running it, Copperline booted into the real
AROS ROM sequence, and the `run_until` call blocked until this was
noticed and killed (`copperline`'s own log showed `ROMInfo:`/`zorro:`
boot messages, the tell). The harness's `ARENA_BASE` (`$00110000`) stays
comfortably clear of this.

## Findings

### 1. Barrel-shift carry does not persist across rows (likely a `machine-core` bug)

**Divergence.** For any A- or B-channel barrel shift with a non-zero
shift amount, on a blit of 2 or more rows, `machine-core`'s output
diverges from Copperline's starting at the first word of the *second*
row onward. The final channel pointers always agree; only the written
data differs, by roughly one shift-amount's worth of stale/missing carry
bits at each row boundary.

**Minimal repro** (from the differential's own output,
`BLTSIZE`/pointers chosen by the harness's arena allocator but the shape
is what matters):

```
bltcon0 = 0x19F0   (ash=1, USEA|USED, LF=0xF0 -> D=A)
bltcon1 = 0x0000   (ascending)
BLTAFWM = BLTALWM = 0xFFFF
width_words = 4, height_rows = 3
BLTAMOD = BLTDMOD = 0
```

i.e. the simplest possible shifted copy (`D = A`, no masks, no modulo)
run over 3 rows. Row 0 and row 1 match between `machine-core` and
Copperline; row 2 (and every row after the first) does not — e.g. one
captured instance differed starting at the first word of row 2, host
byte `0x06` vs. Copperline's `0x86` (a single missing carried bit).

**Judgement: likely a real bug in `machine-core`'s blitter, not a harness
artefact.** `execute_area` in `crates/machine-core/src/blitter.rs`
declares `a_prev`/`b_prev` (the barrel shifter's carry-in from the
previously processed word) *inside* the per-row loop:

```rust
for _row in 0..self.height_rows {
    let mut a_prev: u16 = 0;
    let mut b_prev: u16 = 0;
    ...
```

resetting the carry to 0 at the start of every row. Copperline's output
is consistent with the shifter instead carrying its state continuously
across the *entire* blit (reset once, at the start, not once per row).
The differential's own evidence supports this reading directly: shift
amount 0 never diverges (the carry is multiplied by a zero shift and
never contributes, so a wrong reset is invisible), and every one-row
case (no row boundary exists to expose the bug) also never diverges,
regardless of shift amount. 21 of the 424 cases hit this (every
`shift_cases` case with a non-zero shift *and* `height_rows > 1`, which
is randomised per case).

Real graphics.library usage may or may not ever hit this in practice —
multi-row shifted blits are common (e.g. any shifted rectangle copy),
so this plausibly affects real, non-degenerate blits, not just
adversarial ones.

### 2. Odd `BLTxMOD` values are not masked to even (likely a `machine-core` bug)

**Divergence.** Whenever `BLTAMOD` or `BLTDMOD` (this file didn't
separately exercise `BLTBMOD`/`BLTCMOD`, but the same code path handles
all four identically) is an odd value, `machine-core`'s resulting channel
pointer ends up exactly 1 further from the start than Copperline's — as
if Copperline treats the modulo as if its bit 0 were forced to 0 before
adding it, every row. On multi-row blits the position error compounds
(row *N*'s effective offset is off by *N* bytes), which then also
diverges the written *content*, not just the final pointer.

**Minimal repro** (single row, so only the pointer is affected — cleanest
possible case):

```
bltcon0 = 0x09F0   (USEA|USED, LF=0xF0 -> D=A, no shift)
bltcon1 = 0x0002   (descending)
BLTAFWM = BLTALWM = 0xFFFF
width_words = 4, height_rows = 1
BLTAMOD = -12   (even -- irrelevant here since USEA's pointer doesn't
                 accumulate modulo drift the test cares about)
BLTDMOD = 3     (odd -- this is the one that matters)
```

D pointer starts at `$0016F0D2`; `machine-core` ends at `$0016F0C7`
(moved `-11`, i.e. `-(4*2) + (-3)`, using the modulo exactly as
written); Copperline ends at `$0016F0C8` (moved `-10`, i.e. as if
`BLTDMOD` had been `-2`, not `-3` — `3 & !1 == 2`). A 4-row instance of
the same shape (`BLTAMOD=-9`, `BLTDMOD=15`) shows the pointer diverging
by exactly 4 (one masked bit × 4 rows) and the written data diverging
from the second row onward once the accumulated position error is large
enough to matter.

**Judgement: likely a real bug (or at least a real gap) in
`machine-core`.** `execute_area`'s modulo handling adds the raw signed
16-bit register value unmasked:

```rust
let amod = signed_step(self.modulo[CHAN_A]);
...
apt = apt.wrapping_add(amod as u32);
```

with no `& !1`. Every real blitter pointer register (`BLTxPTL`) is
documented to ignore bit 0 on direct writes (`machine-core` already does
this — `set_ptr_lo` masks with `0xFFFE`), and this differential's
evidence is that the *end product* of a modulo addition is also
word-aligned on the oracle. Whether real 68000-era hardware masks the
modulo register itself, forces the resulting pointer even after every
add, or something else entirely, this differential cannot distinguish —
it only has Copperline's black-box behaviour to compare against, not
independent HRM text or real hardware. In practice this is unlikely to
matter for real graphics.library usage: bitplane row byte-counts (what
moduli are computed from) are always even on the Amiga, so real software
essentially never sets an odd `BLTxMOD`. It is nonetheless a genuine,
reproducible divergence, worth a second opinion before deciding whether
it needs a fix, a mask, or just a code comment. 10 of the 424 cases hit
this (every `modulo_and_size_cases` case with an odd `amod` or `dmod`).

### Everything else agrees exactly

All 256 minterms (`minterm_sweep_cases`), all 16 channel-enable
combinations × ascending/descending (`channel_enable_cases`), first/last
word masks including the one-word-row case (`mask_cases`), inclusive and
exclusive fill with and without carry-in (`fill_cases`), the
`BLTSIZE`-zero-fields-mean-maximum cases at width 64, height 1024, and
both at once (`bltsize_zero_field_cases`, up to a genuine 1024×64-word
blit), and every line-mode case across octants, `SING`, and the B-channel
texture (`line_cases`, 12 endpoints × `SING` × B-texture = 96 cases) —
these all match Copperline bit-for-bit: written memory, `BZERO`, and
every final channel pointer.

## Copperline oracle limitations found while building this

### The `BLTDDAT` readback gap

`custom.read`/`custom.dump` explicitly refuse to read register offset
`$000` (`BLTDDAT`): `"custom register $000 is not readable"`. The
harness worked around this by having the *CPU itself* read `$DFF000` and
store the result to RAM (`Asm::capture_absl_to_absl`), the same as any
real 68k program would. That CPU-driven read does **not** return the
blitter's last-processed word either — it consistently returns
`DMACONR`'s live value, and reading the known-write-only `BLTCON0`
register the same way returns the identical `DMACONR`-shaped value too
(confirmed directly with a small standalone probe: two consecutive
`BLTDDAT` reads bracketing a `BLTCON0` read all returned the same
`DMACONR`-derived value, ruling out an "echoes the last thing read from
the bus" explanation as well as a real latch). So this Copperline build
does not expose a working `BLTDDAT` readback path via CCP or, evidently,
via the CPU bus.

**Consequence for the differential:** `bltddat` is still captured and
printed in every divergence report (for whatever diagnostic value it
has), but it is **excluded from the pass/fail comparison** —
`Outcome::matches` deliberately does not compare it, with the reasoning
recorded in a doc comment at that exact spot in the test file. Comparing
it would fail on essentially every case for a reason that has nothing to
do with `machine-core`'s blitter (which implements `BLTDDAT` correctly
per the Hardware Reference Manual — `bltddat = last_d`), so it would be
noise, not signal. This is the one check the task asked for that this
differential could not actually exercise against this oracle.

### `BBUSY`-clear vs. final-write ordering

`DMACONR` bit 14 (`BBUSY`) is observed to clear a small number of cycles
*before* the blit's very last word is actually committed to chip RAM.
Reading back immediately upon `BBUSY` going clear intermittently missed
the last word of a blit during development (an 8-word, 1-row test copy
was short its final word until a settle margin was added after the
busy-wait). `build_program`'s 64-`NOP` settle margin after the spin-wait
loop exists specifically for this and is not decorative — every case in
the actual differential (including the two real findings above) has this
margin and its results are stable across repeated runs. Whether this
reflects genuine Amiga hardware pipeline behaviour or is specific to
Copperline's scheduling model, this differential cannot say; it is noted
here because a `DMACON`/`BBUSY` gate is a genuine hardware-shaped
behaviour and the settle margin is load-bearing for anyone extending this
harness.

## The DMACON gap (a known, expected divergence — not a bug this differential found)

`crates/machine-core/src/blitter.rs`'s module doc already documents this
as a deliberate simplification: `Blitter::execute` runs whenever
`BLTSIZE` (or the ECS `BLTSIZH`) is written, with no notion of `DMACON`
at all. Real hardware — and Copperline, faithfully — refuses to run the
blitter without `DMACON`'s `DMAEN` and `BLTEN` bits set. Every register
poke this differential's main test performs is preceded by exactly that
`DMACON` write (`build_program`'s `DMACON_ENABLE_BLITTER`, `$8240`) for
exactly this reason — without it, Copperline's blitter never starts and
the busy-wait spins forever.

This is the same class of gap as the renderer's missing bitplane-DMA
gate (a real, previously-found bug). It is pinned by its own small,
separate, **passing** test,
`dmacon_gate_is_a_known_divergence_from_hardware`: it runs the identical
register setup on both backends with `DMACON` left at its post-reset
default (no DMA enabled at all), and asserts that `machine-core` still
runs the blit (writes the source data to the destination) while
Copperline's destination memory stays untouched. Per this task's
ownership rules, this is reported, not fixed, here.

## Scope: what this differential does not do, deliberately

- **No recorded Workbench traces.** Proposal §12 also asks for a
  differential against "recorded Workbench traces." That needs a booted
  Workbench, which needs Phase 3 storage (a boot device) — out of scope
  for Phase 2 and not attempted here. The randomised-ops half of §12 is
  what this file covers.
- **Line mode: only `BLTCPT == BLTDPT`.** `execute_line`'s doc comment in
  `blitter.rs` already documents the simplification this differential
  is scoped around: `machine-core` collapses line mode's C-read/D-write
  micro-cycle lag to a single address, exact when `BLTCPT` and `BLTDPT`
  start equal (the universal graphics.library convention). Every
  `LineCase` in this file sets `BLTCPT = BLTDPT`. The unequal-pointer
  case is a legitimate gap in this differential's coverage, left for a
  future pass, not silently assumed correct.
- **ECS split-size (`BLTSIZV`/`BLTSIZH`) path not separately exercised.**
  Every test case here triggers via the classic `BLTSIZE` register
  (already covering the zero-fields-mean-maximum decode this shares with
  the split path). `bltsizv_alone_does_not_start_a_blit`, a `blitter.rs`
  unit test, already covers the ECS path's register-arming semantics in
  isolation.
- **Bounded random parameter ranges**, not the full hardware envelope
  (except for the three dedicated `bltsize_zero_field_cases`, which do
  run genuine large — up to 1024×64-word — blits). Widths/heights are
  bounded (roughly 1–6 words/rows for the modulo/size sweep, 1–4 for the
  shift and channel-enable sweeps) and moduli to roughly ±16 bytes,
  chosen so a few hundred cases' scratch-arena footprints fit inside the
  ~950&nbsp;KiB of chip RAM available above the boot-ROM overlay and
  below Copperline's 2&nbsp;MiB `--chip` ceiling. This is wide enough to
  have found both real findings above; it is not exhaustive.
- **Fixed seed (`0xB7171E5D1FF7D1FF`)**, printed in every divergence
  report and in the test's own summary line, so any future divergence
  (or a genuine "still passes" run once the two findings above are
  resolved) is reproducible byte-for-byte without rerunning blind.

## Dev-dependency note

`crates/machine-hosted/Cargo.toml` gained one new `[dev-dependencies]`
entry, `serde_json`, used only by this test file to parse Copperline's
JSON-RPC responses (`custom.dump`'s register map in particular has
enough nested structure that hand-rolling a parser would be more code
and more bug surface than pulling in the ecosystem-standard one). It is
dev-only: it does not touch `machine-hosted`'s shipped binary,
`machine-core`, or either bare-metal board build, and does not affect
`cargo test`'s default (non-`--ignored`) run time since this test file
still has to compile either way but is not executed.
