# Beam position, derived on read

**Status:** Implemented in `crates/machine-core/src/chipset.rs` only. No
other file changed (`git diff --stat` against `main` touches exactly one
file) -- the fix required no caller to change anything, which was itself
a design constraint (see "Rejected designs" below for why a
caller-cooperation design was ruled out, not just avoided).

## The bug this is about

`docs/deterministic-mode.md`'s "A real bug, found and fixed" section is
the full account. Short version: the first working `--cpu-speed fixed`
run loop ticked `MachineBus` once per raster line, by exactly
`ONE_LINE_CLOCKS` (908 CPU clocks = 227 colour clocks) -- an exact
multiple of the period `hpos` wraps on. `Chipset::tick`'s wrap loop
subtracted the line width exactly once and landed back on the same
`hpos` every call, forever. Kickstart's `VHPOSR` delay loop polled 870
million times for a threshold `hpos`'s low byte never reached; the boot
froze at frame ~1950, still retiring instructions, tripping no wedge
detector. **That instance is fixed** -- device time is now distributed
across a line's instructions by exact integer rasterization
(`run_guest_fixed`, `crates/machine-hosted/src/run.rs`) -- and this work
does not touch or weaken that fix.

ADR 0006 names the general hazard: *"any device-time model that advances
in lump sums exactly equal to a periodic boundary is at risk of freezing
any register whose period matches, not just `VHPOSR`."* The bug's actual
cause lived in `run.rs` (a caller choosing how to chunk its ticks), not
in `crates/machine-core/src/chipset.rs`. But `chipset.rs` stored
`vpos`/`hpos` as fields only `tick()` wrote, and `vposr()`/`vhposr()`
returned those stored fields verbatim -- so a beam read was only ever as
current as whatever tick pattern the *caller* happened to use. Any future
caller (a new performance-motivated run loop, a batching change, a
mistake) that reintroduced coarse, period-aligned ticking would
reintroduce the freeze, and nothing in `machine-core` would say so. This
document is about closing that class at the chipset level, and about
being honest, with evidence, about exactly how much of it a chipset-only
design can close.

## The design

`Chipset` no longer stores `vpos: u32` / `hpos: u32` as fields `tick()`
mutates. It stores one running clock, `total_colour_clocks: u64`,
monotonic and never wrapped, incremented by exactly the colour clocks a
`tick()` call represents. `hpos()` and `vpos()` (private helpers;
`vposr()`/`vhposr()`, the register-file idiom's read paths, call them)
derive the wrapped position from it **on every call**:

```rust
fn hpos(&self) -> u32 {
    (self.total_colour_clocks % PAL_COLOUR_CLOCKS_PER_LINE as u64) as u32
}
fn vpos(&self) -> u32 {
    let line_width = PAL_COLOUR_CLOCKS_PER_LINE as u64;
    ((self.total_colour_clocks / line_width) % self.lines_per_frame as u64) as u32
}
```

`tick()` itself becomes closed-form: `lines_started`/`frames_wrapped`
(the `BeamAdvance` a caller uses to drive CIA TOD ticks, VERTB, Graffity
retrace, and the per-line device engines) are computed from
`lines_before`/`lines_after` (`total_colour_clocks / line_width`, before
and after the add) rather than a `while hpos >= LINE { ... }` loop. This
is not just a style change:

- It is O(1) instead of O(lines crossed) -- the old loop, given a single
  call with `cpu_clocks` near `u32::MAX`, would iterate millions of
  times.
- It removes a real overflow risk the old code had: `self.hpos +=
  colour_clocks` (both `u32`) could overflow for a single very large
  `cpu_clocks` value (an unchecked add on a guest-adjacent quantity, the
  kind of thing this module's own header promises "checked arithmetic on
  every guest-controlled value; hostile input fails closed, never
  panics" about). `total_colour_clocks` is `u64` and `saturating_add`,
  so the same input can no longer wrap or panic.
- `lines_started`/`frames_wrapped` are exact for the same reason the old
  loop was exact (both count boundary crossings between two points on
  the same monotonic timeline), confirmed by the existing differential
  test `lazy_tick_matches_eager_tick_across_random_sequences` in
  `crates/machine-core/src/lib.rs` (unchanged, still passing) and by
  every other beam/VERTB/CIA-TOD test in `chipset.rs` and `lib.rs`
  passing unmodified.

`NTSC`'s `set_ntsc` keeps its existing contract: `lines_per_frame` is
read at derivation time, not baked into `total_colour_clocks`, so a
toggle takes effect against wherever the beam currently is -- the same
behaviour the old incrementally-mutated `vpos` field had. (`set_ntsc` is
not wired to anything in `machine-hosted` today; the one test that
exercises it toggles before the first tick, so this equivalence is exact
for every case that actually runs.)

### The detector: `coarse_tick_streak` / `beam_position_may_be_frozen`

A second, additive piece: `Chipset::tick()` now recognises its own
hazard shape. If a single call's `colour_clocks` covers a whole raster
line or more on its own, `coarse_tick_streak` increments; any call that
is *not* that shape resets it to 0. `Chipset::beam_position_may_be_frozen()`
reports `coarse_tick_streak >= 2` -- one coarse call alone is not the
hazard (a legitimate coarse catch-up, e.g. the STOP-path resync's
`STOP_TICK_SLICE`, does this once while the CPU is not executing, so
nothing reads in between), but *repeated* coarse calls, with reads able
to happen in between, is exactly the shape that produced the freeze.

This is pure, `&self`-cheap bookkeeping inside `chipset.rs`; no caller
has to poll it for existing behaviour to be correct, and nothing reads
it today (a future run loop or a debug assertion could). It exists
because of CLAUDE.md's own rule -- *"Silent failure is this platform's
norm. Verify positively; never read absence of a complaint as
success"* -- applied to this specific, previously-silent hazard: turning
"870 million iterations before a human notices" into "flagged after the
second occurrence."

## What derive-on-read fixes, precisely

For any caller that ticks with **sub-line granularity** -- which is the
standing, load-bearing contract for `cycle` and `fixed` mode
(`MachineBus::tick` is called once per retired instruction; see its own
doc comment in `lib.rs`) -- `vposr()`/`vhposr()` now reflect the true
position at the instant of every read, by construction, with zero
caller cooperation: `total_colour_clocks` is simply always current
because it is a plain running total, not a value that has to be
"remembered" to be right. `fine_grained_ticking_shows_the_beam_actually_moving`
(`chipset.rs`) pins this directly against `Chipset`, reproducing
`run_guest_fixed`'s own Bresenham-style split as a caller of `Chipset::tick`
(not copied from `run.rs`; re-derived here as the test's own local
constant `instructions_per_line = 424`, `docs/deterministic-mode.md`'s
measured default) and asserting over 100 distinct `hpos` values are
observed across one line's worth of ticks.

It also removes the specific overflow/O(n) risk named above, for *any*
caller, coarse or fine.

## What derive-on-read cannot fix

This is the part CLAUDE.md's "verify positively" rule requires stating
plainly rather than glossing over.

**Claim tested:** tick `Chipset` directly, repeatedly, in lumps of
exactly `ONE_LINE_CLOCKS` (908 CPU clocks = the exact period `hpos`
wraps on), reading `hpos` after every tick. Does the redesigned chipset
observe the beam moving?

**Result, confirmed empirically, not assumed:** No. `hpos` is
mathematically frozen at the same value on every read, on **both** the
pre-change and post-change chipset. This was checked directly, not
inferred: the pre-change `chipset.rs` (from `git show HEAD`, before this
slice) was temporarily restored to the working tree, seeded with the
same reconstruction, and run:

```
thread '...::whole_line_lump_ticking_hpos_probe' panicked:
EXPECT THIS TO FAIL on original code: hpos samples [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
```

-- ten samples, all zero, exactly the shape of the original 870-million-
iteration freeze. The post-change chipset's `whole_line_lump_ticking_freezes_hpos_but_is_now_detectable`
test asserts and documents the identical result:
`hpos_samples.iter().all(|&h| h == hpos_samples[0])` **passes** (i.e.
the freeze is confirmed present, not fixed) both before and after this
change.

**Why this is not a defect in the design, and not something any
chipset-only fix can close:** `hpos`, faithfully, *is*
`total_colour_clocks % line_width`. If a caller reports elapsed time
only in exact multiples of `line_width`, and only ever reads between
those reports (never during -- there is no "during" from the read
path's point of view, because no clocks were reported as having
elapsed yet), then the beam's true position at every one of those
instants genuinely *is* identical. This is not a modelling gap; it is
the same aliasing a strobe light gives a spinning wheel photographed
once per rotation. A design that reported anything else at those exact
instants would be fabricating a position nobody's guest code, and no
real Amiga, ever actually observed -- which is precisely what CLAUDE.md's
"verify positively" rule (and this module's own "hostile input fails
closed, never panics" -- never *fakes*, either) argues against.

Put differently: `MachineBus::tick` already calls its per-instruction
hook once per retired instruction in `cycle`/`fixed` mode (this is
unchanged, load-bearing, and not weakened here), and the STOP-path
resync's one coarse tick per slice only ever happens while the CPU is
not executing (so nothing reads in between). The *only* way to
reconstruct the frozen pattern today is to bypass that contract
entirely and drive `Chipset::tick()` directly and repeatedly in
whole-line lumps -- which is exactly what the reconstruction test above
does, deliberately, as the worst case. No currently-existing caller in
this codebase does that.

### Narrowing of ADR 0006's named hazard (owner's call, not made here)

Per the task that produced this document: **the hazard is narrowed, not
eliminated**, and this document does not edit ADR 0006 -- that is the
owner's decision to make. What can be said with evidence:

- Eliminated: any caller-side risk from `Chipset` presenting a stale,
  incrementally-mutated `vpos`/`hpos` that could drift out of sync with
  what has actually been ticked, and the `u32` overflow / O(n) loop risk
  a single very large `tick()` call had.
- Narrowed: a caller that ticks with sub-line granularity -- the
  standing contract for `cycle`/`fixed` mode, and the actual fix
  `docs/deterministic-mode.md` shipped -- now gets correct, live beam
  positions from `chipset.rs` with **no caller-side change or
  cooperation required at all** (confirmed: this slice's diff touches
  only `crates/machine-core/src/chipset.rs`; no run loop, board
  `main.rs`, or `MachineBus` code changed).
- Not eliminated, and provably cannot be by any read-time derivation:
  a caller that (a) reports device time exclusively in lumps that are
  exact multiples of the beam's own period, and (b) reads exclusively
  between those lumps, will still observe a frozen `hpos`. This is now
  *detectable* (`Chipset::beam_position_may_be_frozen`) rather than
  silent, which is the improvement actually available at this layer.

## Rejected designs

- **A sub-interval offset the caller supplies on each read.** Requires
  the caller to compute and pass something extra on every read to get
  correct behaviour -- the exact "correctness depends on the caller
  remembering to do something" shape the brief ruled out, since a
  caller that forgets (or a new caller that doesn't know to) reproduces
  the original bug's failure mode exactly.
- **A sync-on-read path that advances device time to "now" before
  answering.** There is no "now" independent of what has been reported
  via `tick()` in `cycle`/`fixed` mode -- these modes have no wall-clock
  reference at all, by design (ADR 0006, determinism). `max` mode does
  have one (it is wall-clock-paced), but deriving beam position from
  real wall-clock time there, and from ticked clocks everywhere else,
  would make `VHPOSR`'s behaviour mode-dependent -- violating "must
  work identically in all three timing modes" -- and would make `max`
  mode's boot non-reproducible in a new way (two runs of identical
  guest code could read different `VHPOSR` values depending on host
  scheduling jitter). Rejected outright, not attempted.
- **Interpolating a fabricated "midpoint" position for large single
  ticks** (report the position half-way through the interval a coarse
  call represents, instead of its end). This would give the hazard
  reconstruction test above a non-frozen answer, but at a real cost:
  it reports a beam position that was never actually the state of the
  chipset at any observed instant, and for genuine per-instruction
  ticking (small `cpu_clocks` per call) it would shift `hpos`'s exact
  value by up to half an instruction's worth of colour clocks on
  *every* read -- risking a change to the guest-observable timing this
  machine has been measured against (the exact instruction-count/PC/
  pixel-count reference figures this document's own evidence section
  reconfirms unchanged). Rejected as fabrication that both violates
  "verify positively" and risks a real regression for no proven gain,
  once the true adversarial case (reads only between exact-period lumps,
  proven above) is already established as unfixable by *any* read-time
  scheme, honest or not.
- **A callback-based `tick()` that lets a caller sample intermediate
  positions during one large call.** Technically feasible and would
  give a caller that opts in a way to reconstruct smooth motion inside
  a coarse batch. Rejected because it is opt-in: a caller that does not
  use the callback (which is every existing caller, and any future one
  that doesn't know to reach for it) gets exactly today's behaviour,
  which is the caller-cooperation shape the brief rules out. The
  `coarse_tick_streak` detector was chosen instead specifically because
  it needs no caller opt-in to provide its (narrower, honest) benefit.

## Provenance

The technique -- store a monotonic elapsed-time counter and derive
wrapped/periodic position quantities from it at read time, rather than
mutating the wrapped quantities incrementally -- is a general,
independently-known pattern (not unique to any one emulator or specific
implementation), applied here from first principles against this
codebase's own existing `tick`/`BeamAdvance` contract. No GPL source
(WinUAE, Amiberry, Copperline, AROS) was read or consulted for this
work, per CLAUDE.md's licensing firewall; Copperline in particular
remains a run-never-copy oracle and was not run for this task either.
The Bresenham-style exact-integer-rasterization split used in
`fine_grained_ticking_shows_the_beam_actually_moving` is this project's
own, already documented and shipped in `docs/deterministic-mode.md`
(`run_guest_fixed`, `crates/machine-hosted/src/run.rs`); the test
re-derives it locally as a `Chipset` caller rather than importing or
copying `run.rs` code.

## Evidence

- `cargo fmt --all -- --check`: clean.
- `cargo clippy -p machine-core --all-targets -- -D warnings`: clean.
- `cargo clippy -p machine-hosted --all-targets -- -D warnings`: clean.
- `cargo clippy -p board-qemu-virt --target aarch64-unknown-none -- -D
  warnings` / `-p board-qemu-q35 --target x86_64-unknown-uefi`: clean.
- `cargo test -p machine-core`: **460 passed** (458 baseline + 2 new
  hazard tests), 0 failed. Includes the unchanged differential test
  `lazy_tick_matches_eager_tick_across_random_sequences`.
- `cargo test -p machine-hosted`: **108 lib tests + integration binaries
  pass** (unchanged), including the unconditional `aros_68k_pair_fixed_mode`.
- `cargo test -p machine-hosted -- --ignored --test-threads=1`: **32
  passed, 0 failed**, 807.55s (baseline: 32/32, 779.38s -- within
  run-to-run noise). Every screenshot-baseline gate passed, which is
  only possible if its exact pixel-count assertion still holds
  (13,507 / 15,241 / 14,073 -- these are `assert_eq!`s inside the tests
  themselves, not something this document re-measured separately).
- Reference figures, reproduced directly (not just inferred from test
  pass/fail): `cycle` mode, `--max-frames 4400`: **41,894,978
  instructions, final PC `0x00f8131c`** (unchanged). `fixed` mode, same
  frame count: **41,871,237 instructions, final PC `0x00f8131c`**
  (unchanged).
- Determinism: `fixed_mode_boot_is_deterministic_across_repeated_runs`
  passed inside the `--ignored` suite above. Additionally, three manual
  back-to-back `--cpu-speed fixed --max-frames 4400` runs, nothing else
  running:
  ```
  6229da8e684cc466dad15c504823ff009c9b3b60f2fe1340529e182a06c6c0b9  fixed_run_1.log
  6229da8e684cc466dad15c504823ff009c9b3b60f2fe1340529e182a06c6c0b9  fixed_run_2.log
  6229da8e684cc466dad15c504823ff009c9b3b60f2fe1340529e182a06c6c0b9  fixed_run_3.log
  ```
  Byte-identical across all three, and identical to the hash
  `docs/deterministic-mode.md` recorded before this change.
- `max` mode, `scripts/bench-boot-max.sh --no-build --backend interp`,
  nothing else running: **busy_mips=32.16** (reference: 31.65-31.68,
  within run-to-run noise). WBREADY wall clock: cycle 2.003s / max
  109.610s (reference: 2.338s / 109.609s cycle/max split from
  `docs/deterministic-mode.md` -- the max figure matches to the
  millisecond).
- QEMU board gates, both run to completion with CI's exact marker
  sequence (`scripts/ci-grep-serial.sh` + `scripts/check-serial-markers.sh`):
  - `board-qemu-virt` (aarch64): `PHASE0 BOARD-QEMU-VIRT: ALL CHECKS
    PASSED`, `PHASE1 BOARD-QEMU-VIRT: overlay cleared`, `GUEST |
    callroms done`, `'chip memory'` -- all found.
  - `board-qemu-q35` (x86-64 UEFI): the same four markers with
    `BOARD-QEMU-Q35` naming -- all found.
- `git diff --stat` against the base commit: **one file changed**,
  `crates/machine-core/src/chipset.rs`. No caller (`MachineBus`,
  `run.rs`, either board `main.rs`) was touched, confirming the fix
  needed no caller cooperation to take effect for every existing,
  passing scenario.

Not independently re-verified in this slice (relied on the `--ignored`
suite's own pass/fail): the exact 13,507 / 15,241 / 14,073 pixel counts
were not separately re-extracted from the screenshots outside the
tests' own `assert_eq!`s -- those tests passing is the evidence, per
CLAUDE.md's standing rule that a passing gate is what "not regressed"
means here, not a substitute for re-deriving the same number by hand.
