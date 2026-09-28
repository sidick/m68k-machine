# Wedge detection: a small loop, not just one PC

**Status:** Implemented. `crates/machine-hosted/src/run.rs` only --
no `machine-core` change (this is a host-side diagnostic, not a guest-
visible device; QEMU board gates are unaffected and were not re-run).

## The gap this closes

Commit `525dc1a` (`docs/deterministic-mode.md`'s "A real bug, found and
fixed") froze a Kickstart 3.2.2 boot at frame ~1950 under the first
working `--cpu-speed fixed` run loop: the guest sat in a three-
instruction `VHPOSR` poll --

```
MOVE.W (A4),D1   ; A4 = $00DFF006 (VHPOSR)
CMP.B  <ea>,D1
BLS    <label>
```

-- for 870 million iterations, still retiring instructions, making no
progress (`hpos`'s low byte pinned at `$00` in every one of a targeted
`TRACE_WATCH_PCS` dump's samples). `chipset.rs`'s device-time model was
the actual cause (ticking exactly one raster line's clocks per call --
an exact multiple of the period `hpos` wraps on -- pins the derived beam
position forever; `docs/beam-position-on-read.md` covers that fix and
names the general hazard). But the run loop's own wedge detector should
have caught the symptom regardless of the cause, and structurally could
not: it counted `same_pc_streak`, *consecutive identical* PCs, against a
20,000,000-instruction threshold. A three-instruction loop never repeats
a PC consecutively -- the streak reset on every instruction and never
exceeded 1. The follow-up chipset-level fix (`docs/beam-position-on-
read.md`) is provably unable to close this class on its own: sampling a
periodic register only at multiples of its period always yields the same
value, whatever the chipset does underneath. Detection, not correction,
is the honest answer, and detection is what this document is about.

## The design: `LoopWindow`

`crates/machine-hosted/src/run.rs` replaces `last_pc`/`same_pc_streak`
with a small struct, `LoopWindow`, used identically by all three run
loops (`run_guest`, cycle mode; `run_guest_fixed`; `run_guest_max`):

```rust
struct LoopWindow { lo: u32, hi: u32, streak: u64 }
```

`LoopWindow::extend(pc)` folds one more observed PC into the smallest
contiguous range containing every PC seen since the window was last
reset. If `pc` keeps that range's span within `LOOP_WINDOW_SPAN_BYTES`
(64 bytes), the window widens (or is left alone) and its streak count
extends; otherwise the window resets to `pc` alone, streak back to 1.
This is the direct generalisation of the old detector: a loop confined
to a handful of instructions and a handful of addresses now accumulates
one continuous streak across every one of those addresses, instead of
resetting to 1 on every PC change the way `same_pc_streak` did.

Cycle mode and fixed mode read `loop_window.streak` directly against
`TIGHT_LOOP_THRESHOLD` (unchanged at 20,000,000 -- the same value the old
detector used, reused rather than re-derived, since its own
justification -- "well above one frame's instruction count" -- did not
depend on whether the detector was single-PC or range-based). Both run a
real per-instruction hook, so an exact instruction count is available.

Max mode has no per-instruction hook (`run_guest_max`'s own module doc:
wedge detection there "changes shape entirely" because the CPU runs in
unhooked `run_for_cycles`/`run_batch_instructions` chunks). It samples
`cpu.pc()` once per chunk boundary instead, and uses `extend`'s `bool`
return (window widened vs. reset) to decide whether to keep or restart a
*wall-clock* timer (`MAX_MODE_WEDGE_SECONDS`, unchanged at 15.0 seconds)
-- the same structure, coarsened to chunk granularity, which is the only
grain max mode has.

In all three loops, a `GuestExit::Stopped` exit resets the window (in
`run_guest_max`; cycle/fixed mode never call the hook at all while
stopped, so the window is simply untouched and naturally diverges once
the CPU resumes at a different PC). Kickstart's idle dispatcher always
`STOP`s rather than spinning -- this is deliberate and load-bearing, not
incidental: it is the reason a healthy idle Workbench, sitting on the
same `STOP` opcode indefinitely, never accumulates a streak at all.

### Why 64 bytes

The freeze this replaces spans well under 16 bytes (three instructions:
a 6-byte absolute-long `MOVE.W`, a `CMP.B`, a 2-4 byte conditional
branch). 64 bytes is roughly 4x that -- generous enough to cover a
somewhat larger poll loop (say, half a dozen instructions with a wider
addressing mode or two) without having to be re-tuned for every possible
poll-loop shape, while staying a small fraction of the size of any real
subroutine or basic block in this codebase's ROMs (Kickstart/AROS
functions run to hundreds of bytes at minimum). A span this size cannot
accidentally bridge two unrelated basic blocks of real code; it can only
capture a loop that is, by construction, small.

### Why no separate "progress" signal beyond the PC range

The task brief this document answers names several progress-signal
candidates to evaluate: device state advancing, interrupts delivered and
taken, memory writes outside the loop's own working set, frame progress,
guest-visible I/O completing. Each was considered and rejected as either
redundant with the PC-range signal or actively misleading for this
specific failure mode:

- **Frame progress / device state advancing.** Rejected as a
  discriminator. `MachineBus::tick` receives the CPU's own retired
  cycles every hook call, so device time -- and therefore the frame
  counter, CIA timers, and the beam's raw position -- keeps advancing at
  exactly the rate the CPU keeps executing, *including inside the
  original freeze*. That freeze's entire nature was that real device
  time visibly moved while the guest's *derived, sampled-on-read* view
  of it (`VHPOSR`) did not; a signal built from device time passing
  would have been true throughout the wedge, not a discriminator against
  it.
- **Interrupts taken.** Subsumed by the PC-range signal, not additive to
  it. `m68k`'s hooked run loop auto-vectors a newly serviceable interrupt
  before the next fetch; taking one moves the PC to the exception
  handler, which is (by construction, real ROM code) outside any tiny
  poll loop's `LOOP_WINDOW_SPAN_BYTES` span. So a loop that is genuinely
  being interrupted and serviced already breaks its own streak via
  `LoopWindow::extend` returning `false` -- there is nothing a separate
  "was an interrupt taken" counter would add that the range check does
  not already give for free. Conversely, a loop that runs with
  interrupts masked (a real, common boot-time delay-loop pattern) would
  make an interrupt-taken signal permanently silent regardless of
  whether the loop is legitimate or wedged, so it cannot discriminate
  that case either way. **This is not just an argument from first
  principles -- it was checked against the real freeze, with real
  interrupt state live** (see "Direction 2" below): the actual `VHPOSR`
  loop ran with `INTENA`/`INTREQ` both nonzero and unchanging for over a
  hundred frames while the detector's streak climbed uninterrupted to
  the full 20,000,000, which only happens if no interrupt was ever
  actually *taken* during that window (an enabled-but-unserviced
  interrupt is silent to a PC-range signal, exactly as reasoned above --
  and, separately, exactly why an "interrupts taken" counter would have
  told us nothing extra here even if built).
- **Memory writes outside the loop's own working set.** The most
  semantically precise signal available (a pure poll reads and branches;
  a computation that is actually progressing writes results somewhere),
  but not cheap: it requires either bus-level write instrumentation or
  CPU-level per-instruction write tracking on every one of the tens of
  millions of instructions a healthy boot retires, in the hottest path
  in the machine (see "Performance cost" below for how tight that
  budget already is). Rejected on cost, not on the merits of the idea;
  `docs/wedge-detection.md`'s own future-work note (below) records it as
  the strongest candidate if a future false positive ever demands more.
- **Guest-visible I/O completing.** Not observable from `run.rs` at
  useful granularity without introspection machinery `--inspect` already
  pays for only at the end of a run, not per instruction.

What is left, and what this document argues is actually sufficient: the
PC-range generalisation itself, at the *same* threshold the single-PC
detector already used successfully across every real-ROM gate for as
long as this codebase has had one. The threshold's own rationale --
"well above one frame's instruction count," well above any bounded,
STOP-free busy-wait this codebase's tests exercise -- did not depend on
the detector being single-PC; it depended on legitimate waits being
short or being `STOP`-based, both of which remain true. The empirical
check for whether that assumption holds under a *range*-based detector
(rather than the structurally-blind single-PC one that never got to test
it) is the 32-test real-ROM gate below, run with the new detector live.

### `beam_position_may_be_frozen()` -- considered, not wired in

`crates/machine-core/src/chipset.rs`'s `coarse_tick_streak`/
`beam_position_may_be_frozen()` (added by `docs/beam-position-on-
read.md`, commit `e0890e9`) detects the *exact* caller pattern that
caused the original freeze: `Chipset::tick` called repeatedly with each
call covering a whole raster line or more on its own. It was evaluated
as a candidate corroborating signal for the wedge detector and left
unused, deliberately:

Under every run loop as it exists today, `MachineBus::tick` is never
called that way while the CPU is actively retiring instructions. Cycle
mode and fixed mode tick a *per-instruction* amount from the hook (a
handful of CPU clocks, nowhere near a whole line); the only lump-sum,
whole-line-or-more tick calls left anywhere are the `GuestExit::Stopped`
resync paths in `run_guest`/`run_guest_fixed`, and `run_guest_max`'s
per-event ticks -- all of which run with the CPU not actively fetching
new PCs the way a wedged loop would be, or advance by less than a line
per call in the steady state. So `beam_position_may_be_frozen()` would
be checked against state that essentially never reaches its `>= 2`
condition from any of the three run loops' hot paths as they stand
today; wiring it into the per-instruction wedge check would add a branch
and a method call to the hottest path in the machine for a signal that
would almost never fire, and never fires *because of* the general
`LoopWindow` fix rather than instead of it. This is exactly the
"general signal subsumes it" case the task brief called out as the
reason not to wire a specific signal in merely because it exists. It
remains available, tested, and load-bearing for anyone building a future
run loop that reintroduces coarse per-line ticking -- which is precisely
the caller-error class it was built to catch, per its own doc comment.

## Thresholds

| Constant | Value | Unchanged from before this change? |
|---|---|---|
| `TIGHT_LOOP_THRESHOLD` (cycle/fixed mode) | 20,000,000 instructions | Yes -- reused, not re-derived |
| `MAX_MODE_WEDGE_SECONDS` (max mode) | 15.0 seconds wall clock | Yes -- reused, not re-derived |
| `LOOP_WINDOW_SPAN_BYTES` (new) | 64 bytes | New -- see "Why 64 bytes" above |

Neither existing threshold was changed. The only new knob is the span
cap, and its sizing rationale (roughly 4x the real freeze's span, a
small fraction of any real basic block) is independent of any specific
ROM's instruction timing, so it does not need re-tuning if
`TIGHT_LOOP_THRESHOLD`/`MAX_MODE_WEDGE_SECONDS` are ever revisited.

## Both-directions evidence

### Direction 1: it does not fire on healthy guests

All 32 `--ignored` real-ROM tests (`cargo test -p machine-hosted --
--ignored --test-threads=1`), covering long idle-Workbench waits,
scripted mouse/keyboard interaction several drawers deep, disk-I/O-bound
boot busy-polling, `hostblk`/`pktport`/`pcibridge`/virtio-net/SANA-II
traffic, RTG rendering, and both `--cpu-speed max`-only gates (`Wait 5`
real-time-keeping, CPUBench's eleven self-calibrating kernels) -- see
"Gate results" below for the two full runs (debug and release profile)
this claim is based on: 32/32 passed in both, zero false positives.

**What an idle Workbench actually looks like against this signal, and
why that is the real reason there are no false positives, not the gate
count on its own:** a healthy idle Amiga does not busy-poll at all --
Kickstart's idle dispatcher issues `STOP` with the interrupt mask open
(`docs/phase0-findings.md`), which retires *zero* instructions until an
interrupt wakes it. `run_guest`/`run_guest_fixed`'s per-instruction hook
is never called for a stopped core at all (`run_for_cycles_with_hook`'s
own contract), so `loop_window.streak` simply does not advance while the
CPU is parked in `STOP` -- there is no PC for it to sample, so there is
nothing for the window to extend or reset against. It is not that the
idle loop's PC range is being correctly judged "fine" by some progress
check; it never reaches the check in the first place, for as long as the
CPU stays stopped, which for an idle Amiga is effectively always (it
wakes only for VERTB or a CIA/device interrupt, handles it, and goes
straight back to `STOP`). `run_guest_max` samples `cpu.pc()` once per
chunk boundary rather than skipping stopped periods outright, so it
takes the more explicit route to the same place: a `GuestExit::Stopped`
chunk result unconditionally calls `loop_window.reset(cpu.pc())` before
any range/timer check runs (`run_guest_max`'s own code, and its comment
on why -- "Kickstart's idle dispatcher legitimately holds the same PC in
`STOP` forever, and this detector must never mistake that for a wedge").
This is a stronger claim than "the 32 gates happened to pass": it is a
structural guarantee that idle time contributes nothing to the streak in
any of the three run loops, not an empirical absence of counterexamples
in the current test corpus. A guest that busy-polls *without* `STOP` (a
real pattern this codebase's own boot busy-polls disk I/O with) is the
one case this guarantee does not cover for free -- that is what the
32-gate empirical check earns its keep on, and what "Direction 2" below
additionally confirms does not falsely trip even under the exact bug
this detector exists to catch.

### Direction 2: it fires on the real freeze itself, and the old code provably does not

The strongest evidence available is not a reconstruction of the freeze's
*shape* but the freeze *itself*, run against the real ROM/HDF pair that
originally produced it. This was done, not just reasoned about, at the
owner's explicit request after an initial pass here had rested this
argument on a synthetic interrupt-free loop, which does not stress the
concern that actually decides whether this detector works: real code
runs with real interrupt traffic, and `LoopWindow` resets whenever the
PC leaves its span -- exactly what a taken interrupt does. If VERTB or a
CIA timer had been taken anywhere near often enough during the real
freeze, the streak would never have reached 20,000,000 and the detector
would have missed the actual bug it exists to catch.

**Method:** `run_guest_fixed`'s per-instruction Bresenham tick (the
`docs/deterministic-mode.md`/`docs/beam-position-on-read.md` fix) was
temporarily reverted in place to the pre-`525dc1a` lump-sum-at-line-end
tick -- `bus.0.tick(ONE_LINE_CLOCKS)` once on a line's last instruction,
`bus.0.tick(0)` on every other -- with the rest of this change (the
`LoopWindow`-based detector) left untouched. `machine-hosted` was
rebuilt release and run against the real reference workload
(`nondistribution/A1200.47.115.rom` + `nondistribution/m68k-machine.hdf`,
`--cpu-speed fixed --max-frames 25000`, `--max-instructions` at its
default of 200,000,000 -- ten times `TIGHT_LOOP_THRESHOLD`, deliberately
not tightened for this run).

**Result: it fired, on the genuine bug, well inside the safety margin:**

```
PHASE1 HOSTED: WEDGED (tight loop in PC range 0x00f89412-0x00f89430
(20000000 instructions with no progress)) -- 52785260 instructions,
416 frames, overlay cleared, final PC 0x00f8942c
```

This is not merely a freeze that looks similar -- it is the documented
bug, address for address. Disassembling the ROM at that PC range
confirms the identical three-instruction loop `docs/deterministic-
mode.md` names: `0x00f8942c: MOVE.W (A4),D1` (opcode `0x3214`, i.e.
`MOVE.W (A4),D1`) / `0x00f8942e: CMP.B D2,D1` (opcode `0xb202`) /
`0x00f89430: BLS.S -6` (opcode `0x63fa`, branching back to `0x00f8942c`)
-- the exact `VHPOSR`-poll shape (`A4` loaded from `$00DFF006` a few
instructions earlier in the same routine, at `0x00f8940c`-`0x00f89410`)
that `docs/deterministic-mode.md`'s "A real bug, found and fixed"
section describes settling at `0x00f8942e`. The `progress:` lines
leading up to the wedge show real, nonzero `INTENA`/`INTREQ`
(`INTENA 0x202c, INTREQ 0x0068`) held constant across every 50-frame
sample from frame 300 through frame 400 -- interrupt state was live and
pending throughout, not masked out of the picture entirely -- and the
streak still reached the full 20,000,000 without a single reset, which
means no interrupt was ever actually *taken* (vectored into a handler,
which would have moved the PC outside the loop's 64-byte span and reset
the window) during that entire window. This is consistent with the
loop itself running with the CPU's own interrupt mask (SR, not `INTENA`)
raised -- a real, common shape for a hardware calibration delay loop
that wants an uninterrupted count -- and it is exactly the scenario
`docs/wedge-detection.md`'s "interrupts taken" discussion above predicted
would need checking rather than assuming.

This run hit the aliasing bug on its first invocation, around frame 300
-- earlier than `docs/deterministic-mode.md`'s account of the historical
freeze settling permanently around frame 1950 after apparently surviving
several earlier invocations of the same routine. That difference is
expected, not a discrepancy to explain away: the historical freeze and
this reconstruction take the same *lump-sum-tick* defect through the
same routine, but nothing about the reconstruction is required to
reproduce the exact interrupt-phase history of the original session bit
for bit (this run had no floppy, differently-timed disk I/O against the
`hostblk` HDF, and no guarantee of identical instruction interleaving
against device ticks from the first VERTB onward). What matters for this
document's purpose is that the same routine, hit by the same lump-sum
aliasing defect, produced a genuine multi-hundred-frame stall that the
new detector caught well inside its budget -- not that it happened on a
particular frame number.

**With the pre-change code (`same_pc_streak`) against the same real
reconstruction:** the `LoopWindow` change was isolated back out (leaving
only the lump-sum-tick revert in place) and the identical command
re-run. It did **not** wedge. It ran the full default
`--max-instructions 200,000,000` -- ten times `TIGHT_LOOP_THRESHOLD` --
sitting on the identical final PC (`0x00f8942c`) the entire time, and
exited `LIMIT REACHED (max-instructions)` at frame 1529, never having
incremented `same_pc_streak` past 1 (the loop cycles through three
addresses, never repeating one consecutively -- the same structural gap
this document opened with, now shown against the real bug rather than a
stand-in for it).

Both experimental reverts were fully undone afterward (`run.rs` restored
from the working, `LoopWindow`-plus-Bresenham-tick version; diffed
byte-for-byte against the pre-experiment copy to confirm), rebuilt, and
re-verified: `cargo fmt`/`clippy` clean, `wedge_detection.rs` passing,
and the full 32-test real-ROM gate re-run once more under release
profile (32/32, 342.10s) as a final confirmation nothing was left in a
disturbed state.

### Direction 2, secondary: the fast synthetic reconstruction

`crates/machine-hosted/tests/wedge_detection.rs` (unchanged by the above
-- it predates it) reconstructs the *shape* of the real freeze as a
synthetic ROM whose entire guest program is a three-instruction, 10-byte,
unconditional loop --

```
MOVE.W $00000000,D1   ; absolute-long read, result unused
NOP
BRA.S   -10             ; back to the MOVE.W
```

-- with no `STOP` and, deliberately, no interrupt activity at all
(`--fast-ram` is the only board attached; nothing enables or raises an
interrupt), so it never terminates and never repeats one PC
consecutively. This was the first evidence gathered for this document,
before the real-ROM reproduction above -- kept here, not removed, both
because it is now a permanent regression test (~5 seconds, unlike a
multi-minute real-ROM boot) and because it isolates the general
multi-PC-loop property from the interrupt-timing question the real-ROM
run above settles. It should not be read as sufficient on its own: an
interrupt-free loop is the easy case for a detector built on "the PC
left its span," precisely because nothing in it can trigger that escape
hatch. The real-ROM run above is the evidence that the detector holds up
once that escape hatch is actually live.

Run through the compiled `machine-hosted` binary end to end (not a
library-internal unit test), both under `--cpu-speed cycle` (the
default) and `--cpu-speed fixed`, with a generous `--max-instructions
30000000` fallback (50% above `TIGHT_LOOP_THRESHOLD`) as a safety net
that should never be reached if the detector works:

**With this change (current code):** both tests pass. `machine-hosted`
exits 2 (`Wedged`), stdout contains `WEDGED (tight loop in PC range
0x00f80008-0x00f80010 (20000000 instructions with no progress))`, at
exactly 19,999,999/20,000,000 instructions -- well short of the
30,000,000 fallback.

**With the pre-change code (`same_pc_streak`), run the same reconstruction:**
`crates/machine-hosted/src/run.rs` was temporarily reverted to the
pre-change version (`git stash` of this change) and the same two tests
re-run against the rebuilt binary, unmodified. Both **fail** their
`Wedged` assertion: the run instead hits `LIMIT REACHED (max-instructions)`
at the full 30,000,000-instruction fallback, exit code 1, having never
once incremented `same_pc_streak` past 1 (the loop cycles through PCs
`0x00f80008`/`0x00f8000e`/`0x00f80010`, never repeating one
consecutively). The change was then restored (`git stash pop`) and both
tests re-verified passing before continuing.

## Gate results

- `cargo fmt --all -- --check`: clean.
- `cargo clippy -p machine-core --all-targets -- -D warnings`: clean, no
  changes made to `machine-core`.
- `cargo clippy -p machine-hosted --all-targets -- -D warnings`: clean.
- `cargo test -p machine-core`: 460 passed, 0 failed (plus `hello_guest`
  integration test, plus the doc-test harness with 0 doc tests) --
  unaffected by this change (`machine-core` is untouched), included as
  a sanity check since the gate list calls for it explicitly.
- `cargo test -p machine-hosted` (non-ignored): all passing, including
  the two new `wedge_detection.rs` tests and the existing `smoke.rs`/
  `stop_resume.rs`/`rom_key.rs` suites and the two unconditional
  real-ROM tests (`aros_68k_pair_fixed_mode`,
  `aros_68k_screenshot_shows_boot_screen_content_without_boot_media`).
- `cargo test -p machine-hosted -- --ignored --test-threads=1`: run
  **twice, under both build profiles**, deliberately -- see "Profile
  sensitivity" below for why this matters here specifically, not just
  as due diligence:
  - **Debug profile** (`cargo test`'s default, what everyone other than
    a benchmark run uses): **32/32 passed in 802.22s**, matching the
    project's own quoted debug baseline (779-810s) almost exactly.
  - **Release profile** (`cargo test --release`): **32/32 passed in
    342.03s**. This is a legitimate way to execute the same gate but
    **not the same measurement** as the debug baseline above -- faster
    because the guest code the CPU core interprets runs through
    optimized host code, not because fewer instructions ran (cycle
    mode's own instruction count, 41,894,978 at 4400 frames, was
    byte-for-byte identical in both profiles' benchmark runs below).
    Do not compare this wall-clock figure against the 779-810s debug
    baseline as if they measured the same thing.
  - Both runs include every category: long idle-Workbench waits,
    scripted mouse/keyboard interaction several drawers deep,
    disk-I/O-bound boot busy-polling, `hostblk`/`pktport`/`pcibridge`/
    virtio-net/SANA-II traffic, RTG rendering (including the two
    categories `docs/deterministic-mode.md` had left unconverted to
    `fixed` mode for time-budget reasons -- `..._workbench_renders_through_the_rtgboard_card_driver`
    and `scripted_pointer_and_double_click_work_on_the_rtgboard_rtg_screen`
    both passed here), and both `--cpu-speed max`-only gates (`Wait 5`
    real-time-keeping, CPUBench's eleven self-calibrating kernels).
    **Zero false positives in either profile** -- direction 1 of the
    both-directions evidence, run for real rather than assumed from the
    single-PC detector's old track record.
- `fixed_mode_boot_is_deterministic_across_repeated_runs`: included in
  both runs above (it is one of the 32); passed in both.

### Profile sensitivity

Whether the detector is robust across build profiles is a real
question, not a formality, and was checked rather than assumed (the
owner's own note on this point): a per-instruction detector sits in the
hottest path in the machine, and a threshold tuned or validated only
under one profile's timing could plausibly behave differently under the
other's.

- **Cycle/fixed mode (`TIGHT_LOOP_THRESHOLD`, an instruction count):**
  profile-invariant by construction. The threshold counts retired
  instructions, not wall-clock time; a given guest program retires
  exactly the same instructions in exactly the same order regardless of
  how fast the host interprets them, so whether a legitimate workload's
  longest same-range streak crosses 20,000,000 cannot depend on debug
  vs. release. The two full-suite runs above are consistent with this:
  identical instruction counts at identical `--max-frames` in both
  profiles' benchmark runs (see "Performance cost" below).
- **Max mode (`MAX_MODE_WEDGE_SECONDS`, a wall-clock duration):** not
  obviously profile-invariant in the same way, since it is a real-time
  threshold and debug vs. release genuinely execute at different
  speeds. Reasoned robustness: `run_guest_max`'s chunk size is already
  adaptive to real time (`MAX_MODE_CHUNK_TARGET_US`, ~50us/chunk,
  corrected every iteration from the previous chunk's measured rate --
  see `chunk_cycles`/`chunk_instrs`'s own doc comments), so the *rate*
  at which `LoopWindow` samples the PC and checks the wall-clock timer
  stays roughly constant in real time regardless of host speed; only
  how much guest work happens between samples changes. Empirical check:
  both max-mode real-ROM gates
  (`max_cpu_speed_boots_to_workbench_and_keeps_real_time_across_wait_5`,
  `kickstart_3_2_2_a1200_cpubench_reports_every_kernel_under_max_speed`)
  passed under both the debug and release full-suite runs above -- the
  same happy-path check, at two different host execution speeds. This
  is not a deliberate positive-case check under both profiles (see
  "What could not be verified" below for why that specific gap remains),
  but it is evidence against profile sensitivity in the one place a
  wall-clock threshold could plausibly show it: a debug-mode host, an
  order of magnitude slower per instruction, still kept `Wait 5`'s
  real-time contract and CPUBench's self-calibrated kernels working
  without the wedge detector's own timer firing early or late enough to
  matter.

## Performance cost

Measured with `scripts/bench-boot-max.sh --no-build`, release build,
serially, nothing else of mine running, on the same host, immediately
before and after this change (`git stash`/`git stash pop` of
`crates/machine-hosted/src/run.rs` only -- same binary otherwise, same
ROM/HDF, same `--max-frames 4400` boot-to-Workbench-ready workload the
script already uses as its reference). This is an immediate A/B on this
host, not just an after-the-fact number compared against a baseline
quoted from a different run at a different time -- which is what
actually isolates this change's cost from ordinary host-load variance
(the kind of variance this project's own docs have flagged before:
"five workers this session have had results invalidated ... by
concurrent runs").

| | pre-change (baseline) | with this change | delta |
|---|---|---|---|
| cycle mode, 4400 frames, wall clock | 1.668s | 1.682s | +0.8% (noise-level) |
| cycle mode, 4400 frames, instructions | 41,894,978 | 41,894,978 | identical (deterministic) |
| max mode, 4400 frames, busy MIPS (boot window) | 31.20 | 30.95 | -0.8% |
| max mode, frames 4400-5400, busy MIPS (idle window) | 29.88 | 30.61 | +2.4% |
| max mode, WBREADY wall clock | 109.591s | 109.592s | +0.001% |

The idle-window figure moving in the *opposite* direction from the
boot-window figure, by a larger margin than either moved from baseline,
is itself the tell: these are two runs sitting on either side of
ordinary run-to-run noise, not two runs shifted by one systematic cost.
The task brief's own quoted range for this host/workload (31.65-32.16
busy MIPS) brackets both the pre-change and with-change figures measured
here about equally loosely -- consistent with that range itself
reflecting host-load noise at measurement time, not a regression
introduced by this change. **No measurable performance regression**
from `LoopWindow` over `same_pc_streak`: both are O(1), allocation-free,
per-instruction work; `LoopWindow::extend` replaces one integer equality
compare with two `min`/`max` calls and a subtraction against a constant
-- more arithmetic per call, but not a different asymptotic class, and
it shows in the numbers as noise, not a trend.

The struct itself is 12 bytes (two `u32` + a `u64`), stack-resident (one
per run-loop invocation, not per instruction), with no change to the
hook closure's capture-list shape beyond renaming the captured variables
it replaces.

## What could not be verified

- QEMU board gates (`scripts/run-qemu-virt.sh`/`run-qemu-q35.sh`, board
  clippy) were not re-run: `git status`/`git diff --stat` against `main`
  confirm this change touches only `crates/machine-hosted/src/run.rs`
  (modified) and adds `crates/machine-hosted/tests/wedge_detection.rs`
  and this document -- `machine-core` and both board crates are
  untouched. Per CLAUDE.md's own gate list ("QEMU board gates only if
  you touch `machine-core`"), that gate does not apply here; stated
  explicitly rather than skipped silently.
- `run_guest_max`'s `LoopWindow` path (max mode's own chunk-boundary
  sampling) was not exercised by an equivalent fast synthetic
  reconstruction the way cycle/fixed mode were: its wedge threshold is
  15 seconds of *wall clock*, not an instruction count, so a synthetic
  end-to-end reproduction would need to actually run for 15+ real
  seconds (or have the threshold made test-configurable, which it
  currently is not) rather than the ~2.5 seconds either cycle- or
  fixed-mode reconstruction takes. Its correctness rests on code review
  (it is the same `LoopWindow` struct and the same
  reset-on-`Stopped`/extend-else-reset structure as the two
  directly-tested loops, coarsened only in *when* it samples the PC) and
  on the two max-mode real-ROM gates passing, under both build profiles,
  with the new detector live (see "Profile sensitivity" above) -- which
  exercises its happy path (no false positive) at two different host
  speeds, but is not a deliberate positive-case reproduction the way the
  cycle/fixed-mode tests in `wedge_detection.rs` are.
