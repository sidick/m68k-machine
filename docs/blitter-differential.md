# Blitter differential against Copperline

Proposal §12: "Blitter / renderer: host-side unit tests; differential
against Copperline (randomised ops + recorded Workbench traces)." The
unit tests landed first (21 tests in `crates/machine-core/src/blitter.rs`,
including all 256 minterms checked against an independently written truth
table). This document covers the differential half, implemented in
`crates/machine-hosted/tests/blitter_differential.rs`.

**Bottom line: all 1,245 cases agree exactly with Copperline 0.19.0.**
Re-verified against 0.19.0 (the installed stable release as of this
update) rather than assumed carried over from the original 0.18.0 run:
1,245 cases, 0 divergences, an identical result to that original run.
1,245 cases -- 424 randomised (minterms, channel-enable
combinations, shifts, masks, fill modes, modulos/sizes, and line-mode
octants), 30 area-mode and 791 line-mode distinct register signatures
recorded from a real, booted planar Workbench desktop. Two earlier real
divergences were found and reported here during development of the
randomised half (Findings 1 and 2 below); both have since been fixed in
`crates/machine-core/src/blitter.rs` by other work on this project. A
third was found while extending the recorded-traces half to line mode
(Finding 3): `machine-core`'s blitter stores a `BLTxPTH` write's full 16
bits, while Copperline (and, per the address width this is consistent
with, real ECS hardware) only implements the low 5 bits of every pointer
register's high word -- invisible in every other case in this file
because this differential's own scratch-arena addresses never carry a
high word above 5 bits, but real line-mode traffic reuses `BLTAPTH` as a
Bresenham error term rather than an address, and can leave arbitrary
leftover bits there. It does not affect any written pixel data (verified
byte-for-byte in all 8 signatures that hit it) -- see Finding 3 for the
full writeup and the direct isolated confirmation. It has since been
fixed in `set_ptr_hi`, which now masks the high word to five bits, and
the differential passes with the fix in place. The findings stay in this
document as the record of what was
found and how, per this project's convention that a fixed bug's writeup
does not get deleted, just marked resolved, and an open one is not
deleted either.

## Running it

```sh
cargo test -p machine-hosted --test blitter_differential -- --ignored --nocapture
```

Needs `copperline` (0.19.0 tested; originally verified against 0.18.0,
re-verified against 0.19.0 with an identical result -- see the bottom
line above) on `PATH`. Skips cleanly (prints
`SKIP: ...`, does not fail) if it isn't found, the same convention
`crates/machine-hosted/tests/real_rom.rs` uses for its ROM-dependent
tests. It is `#[ignore]`d, so it never runs under a bare `cargo test`.

Two tests live in the file:

- `blitter_differential_against_copperline` — the actual differential.
  1,245 cases (424 randomised + 30 recorded area-mode + 791 recorded
  line-mode, see below), ~4.6s wall clock (measured; dominated by process
  spawn and CCP round trips over loopback TCP, not emulation —
  Copperline's headless mode is unthrottled). **Passes.** It briefly did
  not: the 8 line-mode signatures Finding 3 describes failed until the
  `set_ptr_hi` masking that finding identified was applied.
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

At reset, the low end of chip RAM — exactly the first 1&nbsp;MiB, checked
directly with `mem.write` probes at and below `$00100000` while widening
this arena for the Workbench corpus (`$000FFF00` writes zero bytes,
`$00100000` writes cleanly) — is overlaid with the boot ROM (`OVL`).
Copperline also logged "1MiB ROM detected" for the AROS ROM it boots
headless sessions with, consistent with the same boundary. `mem.write`
under the overlay silently writes zero bytes (its `written` count is less
than the request), and a hijacked `PC` pointed into it runs stray AROS
boot code instead of the test's program — which is exactly what happened
during development: the first attempt placed its program at `$001000`
and instead of running it, Copperline booted into the real AROS ROM
sequence, and the `run_until` call blocked until this was noticed and
killed (`copperline`'s own log showed `ROMInfo:`/`zorro:` boot messages,
the tell). The harness's `ARENA_BASE` (`$00100200`, a 512-byte margin
below the confirmed edge) stays clear of this.

## Recorded Workbench traces

The randomised half above covers a bounded parameter space chosen by
hand; it cannot know what a real OS actually exercises. This half
answers that directly: boot a real planar Workbench desktop, record
every distinct blitter register combination it arms, and replay each one
through the same differential machinery with randomised memory.

**Mechanism: record register combinations, not pixel data.** Capturing
and replaying a blit's actual source pixels would mean snapshotting chip
RAM per operation — far more machinery than the value justifies, and
this differential already remaps every case's pointers into its own
scratch arena regardless of where the real OS put them, randomised or
recorded. So `machine-hosted`'s `--blitter-trace FILE` flag
(`crates/machine-hosted/src/blitter_trace.rs`) instead watches every
write that reaches a blitter register from `crate::bus::Bus`'s
`AddressBus` impl (the one seam in this hosted-only crate that sees every
guest write before it reaches `machine_core::MachineBus`, without
touching `machine-core` itself), and each time a `BLTSIZE` write arms a
blit — the same write `MachineBus::write_custom_word` reacts to by
calling `Blitter::execute` — records `BLTCON0`/`BLTCON1` (channel enables
live in `BLTCON0`'s own bits, so no separate field is needed), the masks,
the four modulos, the three constant-channel data registers, the size,
and whether `BLTCPT`/`BLTDPT` happened to be equal (graphics.library's
read-modify-write idiom, `D = A | C` onto existing content). Absolute
pointers are dropped. A `HashSet` of the recorded signature deduplicates
on the fly, so a multi-thousand-blit boot yields a small corpus rather
than a firehose. The flag is `None` on a plain run, so an unmodified boot
pays one `if let Some` check per register write and nothing else — the
baselines below are unmoved by this flag's mere existence.

**Line mode is recorded and replayed too, as a second, raw shape.**
Earlier revisions of this differential recorded line-mode arms as seen
but never added them to the corpus, reasoning that `BLTAPTL`/`BLTAPTH`
hold the Bresenham error accumulator rather than a memory address, so
replaying a captured line-mode signature through `AreaCase` (which treats
every channel's pointer as an address to remap into the arena) would
silently corrupt the geometry. That reasoning was correct about
`AreaCase` specifically, but not about line mode being unreplayable: on
this boot, 1,368 of the 1,478 total `BLTSIZE` arms observed (92%) were
line mode, so excluding them meant this half of the differential
contributed *no* line coverage at all despite line mode being the
overwhelming majority of what this guest actually does with the blitter.

The fix is a second recorder shape, `LineSignature`
(`blitter_trace.rs`) on the recording side and `LineRawCase`
(`blitter_differential.rs`) on the replay side, that carries every
register **verbatim** rather than remapping addresses:
`BLTAPTH`/`BLTAPTL` (the error term) are replayed byte-for-byte exactly
as captured, never touched; `BLTAMOD`/`BLTBMOD` (the Bresenham corrective
increments) and `BLTCMOD`/`BLTDMOD` (the bitplane's bytes-per-row --
`execute_line` only ever reads `BLTCMOD` for this, per its own doc
comment, but `BLTDMOD` is captured and replayed anyway for fidelity) are
also verbatim. `BLTCPT`/`BLTDPT` are the one pair still remapped into
this differential's own scratch arena (same reasoning as every other
pointer in this file: the harness owns where its memory lives, not the
real OS), reduced to the same `c_eq_d` structural flag `AreaCase` already
uses for its own read-modify-write idiom -- `execute_line`'s doc comment
requires `BLTCPT == BLTDPT` at arm time (it overwrites `BLTDPT` from
`BLTCPT` every pixel rather than stepping it independently), so a
recorded signature with `c_eq_d` false would be outside this
differential's existing line-mode scope, exactly like the randomised
`line_cases`' own `BLTCPT == BLTDPT` convention. Both shapes are replayed
through the same `build_program`/`Ccp` harness every other case in this
file uses -- not a second mechanism, just a second register-shape/
allocation strategy feeding the same pipe.

Because a real line-mode blit can run substantially longer than a random
generator's small hand-picked geometry (this boot's longest recorded line
is 43 pixels, against real bitmap row strides up to 90 bytes/row), each
`LineRawCase` gets a conservative worst-case span -- every one of its
pixels charged a full row-stride move *and* a word-crossing x-shift step,
in whichever direction, since a recorded octant's sign bits are read
from the register, not inferred from endpoints this differential never
had -- and cases that would not fit the ~1&nbsp;MiB scratch arena are
skipped and counted, never silently dropped from the total. On this
corpus, 0 of 791 needed to be skipped for arena bounds (worst-case total
footprint came to about 545&nbsp;KiB, comfortably inside a arena reused
fresh for this phase -- see `blitter_differential.rs`'s comment at the
call site for why reuse is safe here). Two more possible skip reasons are
checked and counted the same way, both currently zero on this corpus:
`BLTCON0`'s `USEB` bit set (a real second pointer, `BLTBPT`, this
recorder drops like every other address -- never observed in this
corpus, so never exercised) and `c_eq_d` false (also never observed).

**Running it.**

```sh
cargo build -p machine-hosted --release
./target/release/machine-hosted --rom nondistribution/A1200.47.115.rom \
  --hostblk nondistribution/m68k-machine.hdf --screenshot /tmp/planar.png \
  --screenshot-frame 4000 --max-frames 4500 --max-instructions 300000000 \
  --blitter-trace /tmp/blitter_trace.txt
```

reaches the same 752×576, 6-distinct-colour, 13507/433152-non-background-
pixel planar Workbench desktop this project's baselines are already
pinned against, unchanged by tracing. Neither the ROM nor the HDF is
redistributable (`nondistribution/README.md`), so the *output* of that
run — deduplicated, pointer-free register signatures, not guest memory or
ROM/disk content — is what's checked in:
`crates/machine-hosted/tests/fixtures/blitter_workbench_corpus.txt`. The
differential test parses it with `include_str!` and folds it into the
same `area_cases` list the randomised generators build, via the same
`AreaCase`/`run_area_case`/`build_program` path — not a second mechanism.
That is why `cargo test` here needs neither the ROM nor the HDF: the
corpus is data, checked in once, replayed with fresh randomised memory on
every run, exactly like every other case in this file.

**What the area-mode corpus holds — a sharp negative result.** Over that
boot: 110 area-mode `BLTSIZE` arms were observed, deduplicating to
**30** distinct signatures. All 30 share one shape: `USED` alone (no
memory-backed source channel active) with minterm `LF=0xF0` (`D = A`, so
a *disabled* A channel's constant `BLTADAT` register becomes the fill
value) — graphics.library's plain rectangle-fill idiom, used repeatedly
for window and icon background clears. They vary only in the modulos,
the constant data values, and width/height. **Not one of the 30 uses a
shifted barrel, a non-identity minterm, `BLTCON1`'s fill mode, or more
than one active channel.** That is narrower than the randomised half's
own coverage (256 minterms, all 16 channel-enable combinations, the full
0–15 shift range) by a wide margin — this project's Workbench desktop,
on this boot, simply never asks the blitter's *area* mode to do most of
what it can do. Whether `BltTemplate` text rendering, gadget rendering,
or a deeper desktop interaction (opening a window, dragging an icon)
would reach shift/mask/multi-channel combinations this captured moment
did not is an open question this differential cannot answer from one
screenshot's worth of boot; the honest reading of this specific corpus is
that a static, just-booted planar Workbench desktop's *area-mode*
blitter usage is extremely repetitive, not that the blitter's other
modes are unused by real software in general (the unit tests and the
randomised half already establish those modes work; this is a statement
about what one real boot happened to exercise, not about `machine-core`'s
coverage).

**What the line-mode corpus holds — the opposite result.** 1,368
line-mode `BLTSIZE` arms were observed — **92% of every blit this boot
armed** — deduplicating to **791** distinct signatures, roughly 26 times
the area-mode corpus's size from a boot that only ran 12.4x as many total
line arms as area arms. Where area mode's real usage turned out narrow,
line mode's is broad:

- **All eight Bresenham octants** (`SUD`/`SUL`/`AUL`) appear, with a real
  skew toward shallow/horizontal-ish octants (`SUD=0`: 443 of 791) over
  steep ones (`SUD=1`: 348) — plausible for a desktop mostly drawing
  horizontal/vertical window borders and text baselines, though this
  differential has no way to confirm that reading from registers alone.
- **Six distinct minterms** appear (`0x0A`, `0x2A`, `0x6A`, `0xCA`,
  `0xEA`, `0xFA`) — including the task brief's expected classic `0x2A`/
  `0xEA` shapes plus four more this boot actually used. This is coverage
  the randomised `line_cases` generator does **not** reach: every
  randomised line case hard-codes one fixed minterm
  (`BLTCON0_USEA|USEC|USED|0x00FA`) regardless of octant, so the other
  five minterms recorded here are exercised nowhere else in this
  differential.
- **`SING`** (single-dot-per-row) is set on 170 of 791 (~21%).
- **The start-x shift nibble** (`BLTCON0` bits 12–15) and **the
  texture-shift nibble** (`BLTCON1` bits 12–15, `bsh`) both span the
  full 0–15 range. The texture-shift nibble is also new coverage versus
  the randomised half: `line_geometry` never sets it at all (it stays 0
  in every randomised case), so the recorded corpus is the only place in
  this differential that nibble's full range is ever driven through real
  hardware.
- **Line lengths** (`BLTSIZE`'s height field, the pixel count) range
  1–43, longer than the randomised `line_cases`' hand-picked endpoints
  (the longest is a 13-pixel diagonal).
- **Row-stride moduli** (`BLTCMOD`, the only modulo `execute_line`
  actually reads for the row step) take real bitmap values — 2, 4, 6, and
  80 bytes/row — against the randomised half's fixed 2-byte-per-row
  16-pixel test canvas (`LINE_BPLMOD`). This is the most concrete "reaches
  combinations the randomised sweep does not" result: real multi-plane,
  hundreds-of-pixels-wide bitmap geometry, not a synthetic minimal canvas.
- **`BLTAPTH`** (the error term's high word, replayed verbatim) is
  nonzero in 8 of the 791 signatures — real leftover register state, not
  always a clean sign-extension of the low word. This is precisely what
  exposed Finding 3 below, and precisely what a from-endpoints
  `LineCase` (which always derives a zero high word) could never reach.
- **What this corpus does *not* reach**: `BLTCON0_USEB` (a real B-channel
  texture memory read) is clear on all 791 -- every line drawn used the
  constant, locally-rotated `BLTBDAT` path, never a real dashed-pattern
  fetch from RAM. And `BLTBDAT` itself is always `0xFFFF` (all-ones, i.e.
  solid, undashed lines) across all 791, so despite the texture-shift
  nibble's full 0–15 spread above, no *visible* dash pattern is actually
  exercised by this corpus — the nibble cycles a rotation amount that
  never has any effect because the pattern it rotates is uniform. Both
  are true negative results about this specific boot, on the model of
  area mode's own negative result above, not a gap in what this
  differential's mechanism could capture.

**Result: 783 of 791 line-mode signatures match Copperline bit-for-bit;
8 hit Finding 3.** All 30 area-mode signatures continue to match exactly,
same as before. The 8 line-mode divergences are the only cases (of 1,245
total) that disagree with the oracle, and every one of them is the exact
same root cause (Finding 3, below) on the exact 8 signatures with a
nonzero `BLTAPTH`; the written pixel data still matches byte-for-byte in
all 8. Every one of the 1,245 cases (not just a sample) was run through
the real hand-assembled-68k-program-on-real-Copperline-CPU path.

## Findings

### 1. Barrel-shift carry does not persist across rows (fixed)

**Status: fixed in `crates/machine-core/src/blitter.rs` since this was
first written.** The differential re-confirms this: `shift_cases`
(0–15 shift × ascending/descending × multi-row) is part of the 424
randomised cases above and all of them now agree with Copperline. Kept
below as the original finding record, per this project's convention of
not deleting a fixed bug's writeup.

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

### 2. Odd `BLTxMOD` values are not masked to even (fixed)

**Status: fixed in `crates/machine-core/src/blitter.rs` since this was
first written.** `modulo_and_size_cases` (which includes odd `amod`/
`dmod` values by construction) is part of the 424 randomised cases above
and all of them now agree with Copperline. Kept below as the original
finding record, per this project's convention of not deleting a fixed
bug's writeup.

**Divergence (as originally found).** Whenever `BLTAMOD` or `BLTDMOD` (this file didn't
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

### 3. `BLTxPTH` pointer registers keep all 16 written bits, not just the low 5 (fixed)

**Status: fixed.** Reported here rather than fixed by the work that
found it, per this file's ownership rules — the implementation under
test must not be edited to make its own differential pass — then fixed
separately on review; see the end of this finding. Found while
extending the recorded-traces half to line mode
(above); every earlier case in this differential, randomised or
recorded, keeps every pointer's high word within the scratch arena's own
5-bit range (`ARENA_CEILING` sits under 2&nbsp;MiB), so this was
invisible until line mode's *verbatim* `BLTAPTH` replay carried a
genuinely arbitrary high word through the harness.

**Divergence.** Of the 791 recorded line-mode signatures, 8 share a
recorded `BLTAPTH` of `0x9999` (the Bresenham error term's high word,
meaningless to the line algorithm itself but still a real register value
this corpus preserved verbatim, per this file's own design). After
running the blit, `machine-core` reports that channel's final pointer as
`0x99990002` (or `...FFFF`, `...0002`, etc., depending on the case's
final low-word error term) — the high word `0x9999` untouched, exactly
as written. Copperline reports `0x00190002` — the same low word, but the
high word reduced to `0x0019`. `0x9999 & 0x001F == 0x0019` exactly.

**Confirmed as a register-write-time effect, not a blit-execution
artifact**, with a standalone probe outside this differential's own
harness: a single `MOVE.W #$9999,$DFF050` (`BLTAPTH`) poke, with **no**
`BLTSIZE` write and no blit ever run, followed immediately by
`custom.dump`, already reads back `0x0019`. The same probe against
`BLTCPTH` (`$DFF048`, poked `0x9999`), `BLTBPTH` (`$DFF04C`, poked
`0xABCD`), and `BLTDPTH` (`$DFF054`, poked `0xFFFF`) read back `0x19`,
`0xD`, and `0x1F` respectively — `0xABCD & 0x1F == 0xD` and
`0xFFFF & 0x1F == 0x1F`, both exact. So this is not specific to `BLTAPT`
or to line mode: **all four of Copperline's `BLTxPTH` registers appear to
implement only their low 5 bits**, consistent with the ECS chipset's
2&nbsp;MiB chip RAM ceiling needing only 21 address bits total (16 in the
low word, 5 more in the high word) — a real hardware address-width limit
this differential had never previously written a large enough high word
to expose, since every other pointer in this file is a remapped address
already confined to that same 21-bit space by construction.

**Minimal repro** (one of the 8 actual corpus hits, `workbench line trace
#418`, register values exactly as replayed):

```
bltcon0 = 0xAB6A   (USEA|USEC|USED, LINE-mode minterm 0x6A, ash=10)
bltcon1 = 0xF057   (LINE, SUD|SUL|AUL octant, SIGN, texture shift=15)
BLTAPTH = 0x9999, BLTAPTL = 0xFFFF   (error term, replayed verbatim)
BLTAMOD = -6, BLTBMOD = 4, BLTCMOD = 2, BLTDMOD = 0
BLTADAT = 0x8000, BLTBDAT = 0xFFFF, BLTCDAT = 0x0000
BLTSIZE: width=2, height=6 (6-pixel line)
```

`machine-core`'s final `BLTAPTH` (read back via `custom.dump`'s
equivalent, `Blitter::pt[CHAN_A]`'s high 16 bits): `0x9999`.
Copperline's: `0x0019`. **The written destination window is
byte-for-byte identical between the two backends in this case and all 7
others that hit this** — `execute_line` only ever consults the low 16
bits of `pt[CHAN_A]` as the signed error accumulator (per its own doc
comment), so this divergence has no effect on any pixel `machine-core`
actually draws. It is a real, reproducible register-readback divergence,
not a rendering bug.

**Judgement: likely a real, previously-unreachable gap in
`machine-core`, not an oracle artefact.** `set_ptr_hi`
(`crates/machine-core/src/blitter.rs`) stores a `BLTxPTH` write's full 16
bits unmasked, for all four channels:

```rust
fn set_ptr_hi(ptr: &mut u32, value: u16) {
    *ptr = (*ptr & 0x0000_FFFF) | ((value as u32) << 16);
}
```

Real graphics.library never has occasion to expose this for `BLTCPT`/
`BLTBPT`/`BLTDPT` in area mode, or for `BLTAPT` in area mode either: every
real chip RAM address already fits in the 21 bits the hardware
implements, so a real high word never carries a bit above 4 in the first
place. Line mode's reuse of `BLTAPT` as an error term is the one place a
register conventionally holding an address can end up with genuinely
arbitrary bits in its high word from a previous, unrelated use of the
same physical register — which is exactly the scenario this recorded
corpus captured 8 real instances of.

**Fixed.** `set_ptr_hi` now masks the high word to five bits, matching
the oracle and the 21-bit pointer width the hardware implements. This
was flagged rather than changed by the work that found it, since the
implementation under test must not be edited to make its own
differential pass; the change was made separately, on review, with the
whole 1,245-case corpus and every rendering baseline re-run afterwards
(all byte-identical). `set_ptr_lo` directly below it already modelled
the companion quirk — the hardware ignoring bit 0 — so this closes a
gap rather than introducing a new kind of special case.

### Everything agrees exactly

All 256 minterms (`minterm_sweep_cases`), all 16 channel-enable
combinations × ascending/descending (`channel_enable_cases`), the full
0–15 A/B barrel shift range × ascending/descending × multi-row
(`shift_cases`, Finding 1's former repro shape), first/last word masks
including the one-word-row case (`mask_cases`), inclusive and exclusive
fill with and without carry-in (`fill_cases`), signed modulos including
odd values (`modulo_and_size_cases`, Finding 2's former repro shape) and
one explicit `C == D` read-modify-write case, the
`BLTSIZE`-zero-fields-mean-maximum cases at width 64, height 1024, and
both at once (`bltsize_zero_field_cases`, up to a genuine 1024×64-word
blit), every line-mode case across octants, `SING`, and the B-channel
texture (`line_cases`, 12 endpoints × `SING` × B-texture = 96 cases), the
30 recorded area-mode Workbench-trace signatures, and 783 of the 791
recorded line-mode Workbench-trace signatures — every one of these 1,237
cases matches Copperline bit-for-bit: written memory, `BZERO`, and every
final channel pointer. (The remaining 8 line-mode signatures match on
written memory and `BZERO` but not on the final `BLTAPT` pointer value —
Finding 3, above.)

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

- **Line mode: only `BLTCPT == BLTDPT`.** `execute_line`'s doc comment in
  `blitter.rs` already documents the simplification this differential
  is scoped around: `machine-core` collapses line mode's C-read/D-write
  micro-cycle lag to a single address, exact when `BLTCPT` and `BLTDPT`
  start equal (the universal graphics.library convention). Every
  `LineCase` in this file sets `BLTCPT = BLTDPT`, and every `LineRawCase`
  requires the recorded `c_eq_d` flag to be true (0 of 791 recorded
  signatures had it false, so nothing was actually excluded by this on
  the current corpus, but the check and its skip-counter stay in place
  for a future re-recording that might). The unequal-pointer case is a
  legitimate gap in this differential's coverage, left for a future pass,
  not silently assumed correct.
- **Recorded line-mode traces never include a real `BLTBPT` texture
  read.** `crate::blitter_trace::BlitterTrace` now records line-mode
  arms (791 distinct signatures on the boot this project's corpus was
  captured from, replayed via `LineRawCase` — see "Recorded Workbench
  traces" above), but every one of them has `BLTCON0_USEB` clear, so this
  differential has never exercised `execute_line`'s real-RAM B-channel
  texture-fetch path (`BLTBPT` stepped by `BLTBMOD` every pixel) against
  the oracle, from either half. `run_line_raw_case` would skip and count
  a `USEB`-set recorded signature rather than mis-replay it (`BLTBPT`
  would need remapping into the arena like any other real address, which
  the current recorder doesn't capture since it wasn't needed for any
  case actually seen) — a real, counted gap if a future re-recording ever
  hits one, not a silent one.
- **ECS split-size (`BLTSIZV`/`BLTSIZH`) path not separately exercised**,
  in either half. Every randomised test case here triggers via the
  classic `BLTSIZE` register (already covering the zero-fields-mean-
  maximum decode this shares with the split path); the trace recorder
  also only watches `BLTSIZE`. `bltsizv_alone_does_not_start_a_blit`, a
  `blitter.rs` unit test, already covers the ECS path's register-arming
  semantics in isolation.
- **Bounded random parameter ranges**, not the full hardware envelope
  (except for the three dedicated `bltsize_zero_field_cases`, which do
  run genuine large — up to 1024×64-word — blits). Widths/heights are
  bounded (roughly 1–6 words/rows for the modulo/size sweep, 1–4 for the
  shift and channel-enable sweeps) and moduli to roughly ±16 bytes,
  chosen so the scratch-arena footprint of a few hundred cases (plus the
  30-signature area-mode Workbench corpus) fits inside the
  ~980&nbsp;KiB of chip RAM available above the boot-ROM overlay
  (measured at exactly 1&nbsp;MiB while widening this arena for the
  Workbench corpus — see `ARENA_BASE`'s doc comment) and below
  `--chipset ECS`'s hard 2&nbsp;MiB `--chip` ceiling (`copperline` itself
  refuses more). This is wide enough to have found all three real
  findings above; it is not exhaustive. The 791-signature line-mode
  corpus gets its *own* fresh arena over the same address range instead
  of sharing this budget (every earlier case has already been read back
  and compared by the time the line-mode phase starts, so reusing the
  addresses costs nothing) — its own worst-case footprint came to about
  545&nbsp;KiB, and 0 of 791 needed skipping for arena bounds on this
  corpus (see "Recorded Workbench traces" above for the skip-accounting
  mechanism, kept in place for a future corpus that might need it).
- **One boot's worth of recorded traces, from one point in the boot
  sequence.** The Workbench corpus is a snapshot of what this specific
  planar desktop, at this specific screenshot frame, happened to arm —
  not a claim that real Amiga software never uses shifts, non-identity
  area-mode minterms, `BLTCON1`'s fill mode, `BLTCON0_USEB`'s real
  line-mode texture read, or a `BLTCPT != BLTDPT` line (the unit tests
  and the randomised half already establish the first three work
  correctly in isolation, and the last two remain untested by either
  half — see "Scope" above). A longer session, window dragging, or
  text-heavy `BltTemplate` usage would likely record a richer corpus;
  re-running `--blitter-trace` at a different point (or for longer) and
  re-checking in the result is the natural way to grow this half's
  coverage without changing any mechanism.
- **Fixed seed (`0xB7171E5D1FF7D1FF`)** for the randomised half, printed
  in every divergence report and in the test's own summary line, so any
  future divergence is reproducible byte-for-byte without rerunning
  blind.

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
