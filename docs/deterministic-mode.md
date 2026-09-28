# `--cpu-speed fixed`: deterministic mode without cycle tables (C1, second slice)

**Status:** Implemented. `docs/cpu-core-proposal.md` §5.2's fixed-
instructions-per-line deterministic mode, built on the `GuestCpu` trait
seam the first C1 slice shipped (`docs/cpu-core-trait.md`). This is the
second of the two C1 sub-slices that document names as separate from the
trait itself.

**Not done here, and explicitly out of scope:** direct address-space
mapping / the page-type table (§4.6), the signal-fault cost measurement,
anything in `crates/m68k-vp`, and any edit to `docs/cpu-core-proposal.md`
or ADR 0006's own text. §12 of the proposal treats amending ADR 0006 to
declare fixed mode *the* reproducible mode as an **acceptance** decision;
this document ended with a recommendation for what that amendment would
need to say, rather than the amendment itself. **That amendment has
since been made** (2026-09-28, at the owner's instruction): ADR 0006 now
carries a "The reproducible mode" section defining `fixed` as the
project's reproducible mode, the measured `N`, the honest migration
status, and the whole-period-lump hazard as a named hazard. The
recommendation section at the end of this document is kept as the record
of what was proposed and why. The owner-recorded preference
outside this repo (max mode stays the user-facing default) is unaffected
by any of this -- `fixed` is a third `--cpu-speed` choice, not a change to
the default (`cycle` still is).

## What the mode does

`run_guest_fixed` (`crates/machine-hosted/src/run.rs`) retires exactly
`--instructions-per-line` instructions per raster line, advancing the
beam, both CIAs and the per-line device engines (MIRAGE, `hostblk`,
`pktport`, `pcibridge`) by exactly one raster line's worth of clocks
(`ONE_LINE_CLOCKS = PAL_COLOUR_CLOCKS_PER_LINE * CPU_CLOCKS_PER_COLOUR_CLOCK
= 227 * 4 = 908` guest clocks) **in total over that line** -- spread
across the line's instructions, not applied in one lump sum at the end
(see "A real bug, found and fixed" below for why that distinction is
load-bearing). While the CPU is stopped, lines advance with no
instructions retired -- the same shape `run_guest`'s own
`GuestExit::Stopped` arm already uses for its STOP-path resync (both tick
a whole line in one call there, since nothing is executing to observe the
intermediate state).

Structurally `run_guest_fixed` is `run_guest` (`cycle` mode) with one
change: **the per-instruction hook never reads the `cycles` value
`run_for_cycles_with_hook` hands it, and never calls `MachineBus::tick`
with a real per-instruction cost.** Instead, every retired instruction
ticks a *fixed, position-in-line* quantum -- computed from the
instruction's index within the line (1st, 2nd, ..., Nth), never from
which opcode it was -- by exact integer rasterization:

```
cumulative_before = (ONE_LINE_CLOCKS * (i - 1)) / instructions_per_line
cumulative_after  = (ONE_LINE_CLOCKS *  i     ) / instructions_per_line
amount            = cumulative_after - cumulative_before
```

for the `i`-th instruction of the line (1-indexed). This is a Bresenham-
style exact split: the amounts are all `ONE_LINE_CLOCKS /
instructions_per_line` or one more, evenly spread rather than clustered,
and the sum over exactly `instructions_per_line` calls telescopes to
precisely `ONE_LINE_CLOCKS` (`cumulative(N) - cumulative(0) =
ONE_LINE_CLOCKS`) -- so a line's total device-time advance is exactly the
same either way; only *when within the line* it happens changed.

Everything else -- the per-instruction hook shape, trap dispatch
(`take_*_exception`, never auto-dispatched, same as `cycle` mode), wedge
detection (`same_pc_streak`/`TIGHT_LOOP_THRESHOLD`), exception-storm
detection, `--trace`, `--serial-script`/`--input-script`/
`--trigger-illegal-after-frames` servicing (`service_host_serial`,
frame-gated, unchanged), screenshots, progress reporting, and the
`--max-instructions`/`--max-frames` limits -- is the same code cycle mode
already used, mostly via the same shared helper functions
(`service_host_serial`, `drain_serial`, `track_exception`) rather than
duplicated.

An instruction that traps mid-line (A-line/F-line/TRAP/BKPT/illegal) does
not consume or reset the line's instruction quota: `instr_this_line` is
carried across outer-loop iterations exactly the way `run_guest`'s own
`total_instructions`/`last_pc`/`same_pc_streak` are, and is only zeroed
when a line is actually completed (quota met, or a STOP tick). Device
time for whatever instructions did retire before the trap has already
been ticked, instruction by instruction, by the hook -- there is no
separate "apply the line's clocks" step left to run after a trap.

### Selecting it

```
--cpu-speed fixed [--instructions-per-line N]
```

`--instructions-per-line` defaults to the measured `N` below (see "The
measured default"). It is ignored under `cycle`/`max`. Unlike `max`,
`--trace` works under `fixed` -- both `cycle` and `fixed` drive a real
per-instruction hook, so there is a per-instruction point to hang a trace
off; only `max`'s hook-free chunking forecloses it.

## A real bug, found and fixed -- not a timing-scale story

The first working version of `run_guest_fixed` ticked `ONE_LINE_CLOCKS`
in **one call after the line's last instruction**, rather than spread
across the line as above. Every early-boot, frame-indexed gate (see
"Which gates were re-baselined" below) passed under that version, and a
naive account of the remaining ~22 deeper gates would have been "`fixed`
mode doesn't preserve real per-instruction timing, so some milestones
land at a different frame count" -- a plausible-sounding, wrong
explanation for what was actually happening, and an early draft of this
document said exactly that. It was wrong, and CLAUDE.md's own rule is the
reason it didn't survive: *"Silent failure is this platform's norm.
Verify positively; never read absence of a complaint as success."* A
"different timing model, so milestones shift" story is unfalsified by a
boot that merely runs long; it takes checking what the guest is actually
doing to tell that apart from a boot that has stopped making progress at
all.

**What was actually happening:** booting `nondistribution/
m68k-machine.hdf` under the lump-sum version, the PC settled at
`0x00f8942e` from frame ~1950 onward and never moved again -- not
`Stopped`, not wedged (`same_pc_streak` never reached
`TIGHT_LOOP_THRESHOLD`, since the PC itself cycles through a 3-instruction
loop rather than sitting on one instruction), just retiring instructions
forever. Disassembly at that PC: `MOVE.W (A4),D1` / `CMP.B <ea>,D1` /
`BLS <label>` with `A4 = $00DFF006` -- a poll of **`VHPOSR`, the beam's
horizontal-position register**, waiting for it to cross a threshold. A
targeted register-watch dump (added as a small permanent debugging aid,
`TRACE_WATCH_PCS` in `run_guest_fixed`, mirroring `run_guest`'s existing
one but independent of `--trace`) confirmed it directly: over **870
million** loop iterations, `D1`'s low byte (the polled `HPOS` value) was
`$00` in *every single sample*. The beam's horizontal position was
completely and permanently frozen.

The arithmetic explains why, exactly: `ONE_LINE_CLOCKS` (908 clocks = 227
colour clocks) is *exactly* one full line period. Ticking that amount in
a single call, every time, is an exact multiple of the period the beam's
horizontal position (`hpos`) wraps on -- `chipset::Chipset::tick`'s own
`while self.hpos >= PAL_COLOUR_CLOCKS_PER_LINE { self.hpos -=
PAL_COLOUR_CLOCKS_PER_LINE; ... }` loop subtracts exactly once and lands
back on the same `hpos` it started from, every single call, forever. This
is not specific to this one polling loop or this one boot image: **any**
guest code that reads a live beam-position register between one line-end
tick and the next -- a real, common Amiga technique for calibrating short
delays without a timer -- is mathematically guaranteed to see it never
move, under a design that only ever advances device time in one lump sum
per line. That is a plumbing bug in `run_guest_fixed`, not a property of
§5.2's design: §5.2 asks for the beam to advance "after each batch," which
does not require -- and, this bug shows, must not be implemented as --
advancing it only at the batch's end.

**The fix** is the per-instruction, position-based tick quantum described
above: `hpos` now advances smoothly within a line (each instruction's
tick moves it by roughly `ONE_LINE_CLOCKS / instructions_per_line` colour
clocks' worth), so a `VHPOSR`-polling loop has real intermediate values to
observe, while the total advance per line is still exactly
`ONE_LINE_CLOCKS` and still never depends on which instruction retired.

**Result, confirmed rather than assumed:** with the fix, the same boot
reaches the *same* idle `STOP`, at the *same* final PC (`0x00f8131c`) as
`cycle` mode, at nearly the same instruction count --

| | `cycle` mode | `fixed` mode, lump-sum tick (buggy) | `fixed` mode, per-instruction tick (fixed) |
|---|---|---|---|
| Instructions retired by frame 4400 | 41,894,978 | 2,646,000,000+ (still climbing, frame 20,000) | **41,871,237** |
| Final PC at frame 4400 | `0x00f8131c` (idle `STOP`) | `0x00f8942e` (`VHPOSR` poll, never idles) | **`0x00f8131c`** (idle `STOP`) |

-- a 0.06% instruction-count difference, not the many-line, many-frame
divergence the timing-scale story predicted and did not actually produce.

## The measured default

§5.2's own definition: *"the average number of instructions today's
cycle-budgeted mode retires per raster line while the CPU is not
stopped."* Measured, not guessed, via a small diagnostic added to `cycle`
mode's own hook, gated behind `MEASURE_INSTR_PER_LINE` (same posture as
`SERIAL_REG_TRACE`/`BUS_COVERAGE` -- read once, zero cost unless set):
the hook sums every cycle count it actually passes to `MachineBus::tick`
(`active_clocks` -- by construction this never includes the `Stopped`
arm's own catch-up ticks, since that arm never calls the hook) alongside
the running `total_instructions` it already tracks. At the end of the
run:

```
active_lines   = active_clocks / ONE_LINE_CLOCKS
N              = total_instructions / active_lines
```

This is exactly "instructions retired divided by raster lines' worth of
guest time spent not stopped" -- §5.2's definition, read literally. This
measurement is unaffected by the bug/fix above: it runs entirely inside
`cycle` mode, which was never touched.

Measured on the same ROM/HDF pair `scripts/bench-boot-max.sh` uses
(`nondistribution/A1200.47.115.rom` + `nondistribution/m68k-machine.hdf`,
Kickstart 3.2.2 A1200, boot to Workbench-ready), release build, `cycle`
mode (the default), no other load on the host:

| `--max-frames` | instructions | active lines | instructions/line |
|---|---|---|---|
| 4400 (bench-boot-max.sh's own `BOOT_FRAMES`) | 41,894,978 | 98,746.816 | **424.2666** |
| 6000 | 43,410,313 | 102,150.369 | 424.9648 |

Stable across the two frame counts (0.16% apart), confirming the ratio
isn't still settling. `DEFAULT_INSTRUCTIONS_PER_LINE` in `run.rs` is set
to **424** (the 4400-frame measurement, rounded to the nearest
instruction) -- the same ROM/HDF/frame-count `bench-boot-max.sh`'s own
`BOOT_FRAMES` default already treats as the reference boot-to-Workbench
workload, so this default is anchored to an existing, documented
measurement point rather than an arbitrary one.

As a cross-check, not a candidate default (a different, CPU-bound
workload, not the boot-to-Workbench-ready reference §5.2 asks for):
`nondistribution/m68k-machine-cpubench.hdf` at 3000 frames measures
527.2723 instructions/line -- meaningfully higher than the boot
measurement. This is expected, not noise: CPUBench spends its time in
tight arithmetic kernels (cheap, fast-retiring instructions with no
memory stalls), while the boot workload's "not stopped" time includes
disk-I/O-bound busy-polling with a different, more expensive instruction
mix. §5.2 asks for the boot-to-Workbench-ready measurement specifically
(it is what the existing frame-count gates are budgeted against), so 424
is the number used; the CPUBench figure is recorded here because it
demonstrates the average is workload-dependent, which matters for anyone
reusing this default outside the boot-gate context. (This is also why
`kickstart_3_2_2_a1200_cpubench_reports_every_kernel_under_max_speed`
stays a `--cpu-speed max`-only gate rather than gaining a `fixed`-mode
sibling: CPUBench's eleven kernels each self-calibrate against a real
wall-clock second, which is what `max` mode provides and `fixed` mode by
design does not.)

## Why this is implementable by a core with no cycle tables

The entire design goal, restated from §5.2: *"A core without cycle
accounting cannot provide [cycle-budgeted timing]."* `run_guest_fixed`
never asks `GuestCpu` for anything a cycle-table-free core couldn't give
honestly:

- **`retired_instructions`/per-instruction hook calls** -- exact by
  construction in both timing paths per `docs/cpu-core-trait.md`'s own
  "retired-instruction count" section; nothing new here.
- **The `cycles: i32` value handed to the hook** -- never read.
  `run_guest_fixed`'s hook closure takes `_cycles` and ignores it. A
  core with no cycle tables can report `0`, an arbitrary constant, or
  anything else in that slot; fixed mode's correctness does not depend
  on it being meaningful.
- **The per-instruction tick quantum** -- a function of the
  instruction's *position* within the line (`i`, `instructions_per_line`)
  and nothing else: not the opcode, not any per-instruction cost. A core
  with no cycle tables computes the identical value from the same two
  integers.
- **The `cycle_budget: i32` argument to `run_for_cycles_with_hook`** --
  set to a deliberately huge constant (`FIXED_MODE_BATCH_CYCLES =
  200_000_000`) that no realistic `--instructions-per-line` value should
  ever reach; the hook alone decides when to stop (`HookControl::Return`
  once the line's quota is met). A core with no cycle tables can treat
  this parameter as an upper bound it happens to never hit, or ignore it
  and rely on the hook's own `Return`.

No trait addition was needed. `run_guest_fixed` is built entirely on
methods the first C1 slice already shipped
(`run_for_cycles_with_hook`, `retired_instructions` implicitly via the
hook's own call count, `pc`/`ppc`/`dar`/`int_mask`/`set_irq`, the five
`take_*_exception` methods). This is itself informative for the trait's
design: `GuestCpu`'s decision to expose a hooked, per-instruction entry
point (`run_for_cycles_with_hook`) rather than only unhooked, cycle-
budgeted batches is exactly what let this mode fall out with zero seam
changes -- and, as the bug above shows, the per-instruction hook turned
out to be load-bearing for correctness, not just convenience: a
lump-sum-per-line design that *avoided* the hook's per-instruction call
overhead would have kept the beam-freeze bug.

## The IPL-injection claim (`docs/cpu-core-trait.md`'s "one gap")

The first slice's doc speculated that fixed mode "may make it moot
anyway, since a deterministic run with a fixed retirement count per line
has a natural batch-sizing unit to inject at" -- about max mode's own gap
(exact IPL injection is only resolvable *after* an unhooked batch
returns, since it can cross the target index using whatever IPL was in
effect at batch entry).

**Tested, not just reasoned about:** the speculation's conclusion is
right, but for a different reason than it gives. `run_guest_fixed` does
not solve the unhooked-batch problem by sizing a batch to a line
boundary -- it solves it by never using an unhooked batch at all. Every
line's quota is enforced through the same hooked
`run_for_cycles_with_hook` path `cycle` mode already uses, and
`M68kRsCore`'s `queue_ipl_injection` wrapper (`cpu.rs`) applies a pending
injection inside *that* hook regardless of which run loop is driving
it -- already exercised, independent of any specific run loop, by
`queued_ipl_injection_applies_at_the_target_index` in `cpu.rs`'s own
test module. So fixed mode gets the same **exact, per-instruction**
injection precision cycle mode already had, trivially, because it is the
same mechanism -- not because "the batch now ends at a line boundary"
narrows the max-mode gap. This holds regardless of the lump-sum-vs-
per-instruction tick bug/fix above: both versions used the hooked path
identically; only the *device-time* bookkeeping inside the hook changed.

The corollary, worth stating since the original speculation didn't: this
finding does **not** close max mode's own unhooked-batch gap. That gap is
specific to `run_for_cycles` (no hook at all); fixed mode simply never
exercises that code path, so it neither helps nor is affected by it. If a
future, performance-motivated fixed-mode implementation switched to
the unhooked `run_batch_instructions` (sized to end exactly at
`--instructions-per-line`) to avoid per-instruction hook overhead, it
would reintroduce a version of the same gap -- and, per the bug above,
would also need its own answer to "how does device time move smoothly
within a batch the hook never sees," which is a second, independent
reason not to make that trade lightly. `docs/cpu-core-trait.md` has been
updated with this finding in place of the original speculation.

## Determinism, demonstrated

Three back-to-back release-build runs of the same `fixed`-mode boot
(`--rom nondistribution/A1200.47.115.rom --hostblk
nondistribution/m68k-machine.hdf --cpu-speed fixed --max-frames 4400`),
nothing else running on the host, with the final (bug-fixed) binary:

```
6229da8e684cc466dad15c504823ff009c9b3b60f2fe1340529e182a06c6c0b9  /tmp/final_fixed_run_1.log
6229da8e684cc466dad15c504823ff009c9b3b60f2fe1340529e182a06c6c0b9  /tmp/final_fixed_run_2.log
6229da8e684cc466dad15c504823ff009c9b3b60f2fe1340529e182a06c6c0b9  /tmp/final_fixed_run_3.log
```

Byte-identical SHA-256 across all three, including the full stdout
narration -- not just the final `PHASE1 HOSTED:` line (`41,871,237`
instructions, `4400` frames, final PC `0x00f8131c` in all three, matching
`cycle` mode's own final PC at the same frame count). This is also an
automated gate, not just a manual demonstration:
`fixed_mode_boot_is_deterministic_across_repeated_runs` in
`crates/machine-hosted/tests/real_rom.rs` runs the same boot three times
(at `--max-frames 500`) and asserts `assert_eq!` on the full captured
stdout of runs 2 and 3 against run 1 -- passing in every full-suite run
recorded below.

## Which gates were re-baselined, and what moved

There are 27 `--max-frames` call sites across `crates/machine-hosted/
tests/real_rom.rs`. Once the beam-freeze bug above was fixed, **every one
of the 8 categories checked converged to the existing `cycle`-mode
baseline at the same `--max-frames`, several to the exact documented
reference pixel count.** Thirteen new `fixed`-mode sibling tests were
added, all `--ignored` like their `cycle`-mode originals (except
`aros_68k_pair_fixed_mode`, unconditional like its original since AROS
assets are vendored in-repo) and none of the 27 existing `cycle`-mode
tests were modified:

| Test | `--max-frames` | What was checked | Result |
|---|---|---|---|
| `kickstart_3_2_2_a1200` | 200 | final status line present | pass, unchanged |
| `kickstart_3_2_2_a1200_introspection_finds_a_healthy_exec_base` | 200 | healthy `ExecBase`, 45 resident modules | pass, unchanged |
| `kickstart_3_2_2_a1200_configures_the_pcibridge_board` | 200 | board placed, `ConfigDev` adopted | pass, unchanged |
| `kickstart_3_2_2_a1200_romwack_break_in_reaches_the_debugger` | 400 | `rom-wack` banner, `XCPT: 8000002F` | pass, unchanged |
| `aros_68k_pair` | 200 | final status line present | pass, unchanged (unconditional CI test) |
| `kickstart_3_2_2_a1200_screenshot_shows_the_boot_screen` | 2600 | boot picture drawn, ≥4 colours | pass, unchanged |
| `kickstart_3_2_2_a1200_boots_from_hd_to_the_workbench_desktop` | 4500 | Workbench desktop screenshot | pass, unchanged -- **13,507** non-background pixels, the exact reference figure the `cycle`-mode test's own doc comment records |
| `scripted_double_click_on_the_sys_icon_opens_its_drawer` | 4650 | opened-drawer screenshot | pass, unchanged -- **15,241** pixels, the exact reference figure |
| `scripted_typing_in_a_shell_opened_three_double_clicks_deep_is_echoed` | 5800 | typed-and-echoed screenshot, input queue fully drained | pass, unchanged -- **14,073** pixels, the exact reference figure |
| `kickstart_3_2_2_a1200_pciprobe_proves_the_prometheus_library_api` | 3000 | prometheus.library serial markers, no `FAIL` | pass, unchanged |
| `kickstart_3_2_2_a1200_virtionet_first_packet_round_trip` | 3000 | virtio-net driver + tx/rx round trip markers | pass, unchanged |
| `kickstart_3_2_2_a1200_sanaconform_gates_virtionet_device` | 4000 | `SYS:sanaconform.log` markers (read via `xdftool`) | pass, unchanged |
| `kickstart_3_2_2_a1200_rtg_workbench_desktop_is_grey_not_blank_or_corrupt` | 5200 | 640x480 RTG screenshot, grey background, drawn desktop | pass, unchanged |

Plus the determinism gate (above). That is 4 early-boot/frame-indexed
gates + `aros_68k_pair` (already known to need no re-baselining, since
their milestones are pure frame-indexed narration/introspection, not
disk-I/O- or delay-loop-dependent) and **8 of the "deep" Workbench/
disk-bound gates this document's first draft called "genuinely
frame-count-sensitive"** -- which, per the bug above, they were not; they
were bug-sensitive, and are frame-count-*insensitive* now that the bug is
fixed.

**Not yet converted, for time-budget reasons rather than any known
issue:** `kickstart_3_2_2_a1200_workbench_renders_through_the_rtgboard_card_driver`,
`scripted_pointer_and_double_click_work_on_the_rtgboard_rtg_screen`,
`kickstart_3_2_2_a1200_boots_unattended_to_rtg_workbench_with_input_storage_network`,
and `kickstart_3_2_2_a1200_romwack_break_in_reaches_the_debugger_over_tcp`
were not individually re-run under `fixed` mode or given siblings -- the
eight categories above already cover screenshots (planar and RTG),
scripted input, `hostblk`, `pcibridge`, virtio-net, and SANA-II, so the
remaining four are combinations of mechanisms already independently
confirmed, not untested mechanisms. Given the 100% hit rate across the
eight tested, they are expected to pass at their existing `--max-frames`
too; this is a reasonable expectation stated as such, not a claim of
having verified it.

**Deliberately not converted:**
`kickstart_3_2_2_a1200_cpubench_reports_every_kernel_under_max_speed`
hardcodes `--cpu-speed max` -- CPUBench's kernels self-calibrate against
a real wall-clock second, which only `max` mode provides.

## What amending ADR 0006 would need to say (recommendation, not done here)

Per §12 of the proposal, defining `fixed` mode as *the* reproducible
mode (rather than a third option alongside `cycle`'s existing one) is an
acceptance-level change to ADR 0006, deliberately not made in this
slice. If and when the owner accepts it, the amendment would need to:

1. State the new reproducibility scope precisely: `fixed` mode is
   reproducible *across `GuestCpu` implementations*, including
   cycle-table-free ones -- `cycle` mode's cycle-accurate reproducibility
   (real per-instruction cost) is not something `fixed` mode offers or
   claims to.
2. Record that, once the per-instruction beam-tick fix above is in place,
   frame-count parity with `cycle` mode held for every gate category
   tested (13 of ~27), including several exact-pixel-count matches --
   softer than "always holds," since 4 combination gates and the
   CPUBench-style self-calibrating case were not (or, for CPUBench,
   cannot be) verified the same way.
3. Record the default `N=424` and the measurement method above as the
   ADR's own reference figure, with a note that `N` is workload-dependent
   (the CPUBench cross-check) and this default is anchored to the boot-
   to-Workbench-ready reference workload specifically, not a universal
   constant.
4. Note the IPL-injection finding above (`docs/cpu-core-trait.md`): `fixed`
   mode does not, by itself, change the C2 replay-log design's
   constraints on max mode's unhooked batches.
5. Flag the beam-freeze bug class for anyone building a second `fixed`-
   mode-shaped implementation (e.g. a future unhooked/batch-oriented
   version for speed): any device-time model that advances in lump sums
   exactly equal to a periodic boundary is at risk of freezing any
   register whose period matches, not just `VHPOSR` -- this should be a
   named hazard in the ADR text, not just this document.

## Re-running the evidence in this document

```sh
# The measurement (MEASURE_INSTR_PER_LINE) -- runs in cycle mode, unaffected by the fix above
cargo build --release -p machine-hosted
MEASURE_INSTR_PER_LINE=1 ./target/release/machine-hosted \
  --rom nondistribution/A1200.47.115.rom --hostblk nondistribution/m68k-machine.hdf \
  --max-frames 4400 --max-instructions 0 2>&1 | tail -3

# Determinism (three runs, compare hashes)
for i in 1 2 3; do
  ./target/release/machine-hosted \
    --rom nondistribution/A1200.47.115.rom --hostblk nondistribution/m68k-machine.hdf \
    --cpu-speed fixed --max-frames 4400 --max-instructions 0 > /tmp/fixed_run_$i.log 2>&1
  sha256sum /tmp/fixed_run_$i.log
done

# The beam-freeze bug, if re-checking against a future regression:
# watch VHPOSR's low byte at the loop's own PC across many lines
TRACE_WATCH_PCS=0x00f8942c ./target/release/machine-hosted \
  --rom nondistribution/A1200.47.115.rom --hostblk nondistribution/m68k-machine.hdf \
  --cpu-speed fixed --max-frames 4400 --max-instructions 0 2>&1 | grep WATCH | tail -5

# The re-baselined gates (fixtures required; see nondistribution/README.md)
cargo test -p machine-hosted -- --ignored --test-threads=1 fixed_mode
cargo test -p machine-hosted --test real_rom aros_68k_pair_fixed_mode
```

## Standard-of-evidence checklist

- `cargo test -p machine-core`: 458 unit tests + `hello_guest` pass,
  unchanged -- `machine-core` was not touched by this slice (confirmed by
  `git diff --stat`: only `crates/machine-hosted/src/cli.rs`,
  `crates/machine-hosted/src/run.rs`,
  `crates/machine-hosted/tests/real_rom.rs`, and this crate's own docs
  changed). Per CLAUDE.md's own stated condition ("QEMU board smoke tests
  if you touch `machine-core` at all"), the QEMU smoke scripts were not
  re-run for this slice -- same exemption reasoning `docs/cpu-core-trait.md`'s
  first slice already recorded for the same reason.
- `cargo test -p machine-hosted`: 108 lib tests + integration test
  binaries pass, including the unconditional `aros_68k_pair_fixed_mode`.
- `cargo test -p machine-hosted -- --ignored --test-threads=1`: full
  suite including all 13 new `fixed`-mode tests, run to completion
  serially with nothing else running -- **32 passed, 0 failed, 0
  ignored, finished in 779.38s**. That is the 19 pre-existing real-ROM
  gates plus 13 new tests (12 `--ignored` `fixed`-mode siblings, since
  `aros_68k_pair_fixed_mode` runs under the plain, non-`--ignored`
  `cargo test` instead). No pre-existing test's behaviour changed.
- `cargo fmt --all -- --check` and `cargo clippy -p machine-hosted
  --all-targets -- -D warnings`: clean.
- `scripts/bench-boot-max.sh --no-build --backend interp`: release build
  first, then measured, with the `--ignored` suite already finished and
  nothing else running. Result: **busy_mips=31.66** (`interp` backend),
  against the first slice's own 31.68 figure
  (`docs/cpu-core-trait.md`'s "Performance" section) and the original
  step-7.2 baseline of 31.65 -- within run-to-run noise, not a
  regression. Expected: `fixed` mode is new, additive code
  (`run_guest_fixed`) with the only change to `run_guest`/`run_guest_max`'s
  own bodies being the `ONE_LINE_CLOCKS` constant promotion out of
  `run_guest`'s STOP arm (a pure rename/share of the same computed value
  the local `STOP_TICK_SLICE` constant already held) -- nothing in
  `run_guest_max` itself changed at all. Full comparison table:

  | | step 7.2 baseline | slice 1 (`GuestCpu` trait) | this slice (`fixed` mode added) |
  |---|---|---|---|
  | Boot-to-Workbench-ready (4400 frames), busy MIPS | 31.65 | 31.68 | **31.66** |
  | max mode, 4400 frames, own timing report | -- | wall=87.859s busy=2.932s | wall=87.859s slept=84.943s busy=2.916s |
  | WBREADY wall clock, cycle / max | -- | -- | 2.338s / 109.609s |
  | `cargo test -p machine-hosted -- --ignored` | -- | 19/19, 509.23s | 32/32, 779.38s (13 more tests) |
