# ADR 0006 — Three timing models: cycle-budgeted, wall-clock-paced for users, fixed-instructions-per-line for gates

**Status:** accepted, 2026-09-27, for the split into two modes and for
the paced mode's timing model below.

**Amended 2026-09-28** to add a third mode, `--cpu-speed fixed`
(fixed-instructions-per-line), and to change what "deterministic" means
in this project. This is the amendment `docs/cpu-core-proposal.md` §12
names as an acceptance-level change; it is made here at the owner's
instruction. Evidence: `docs/deterministic-mode.md` (the mode, the
measured `N`, the gate results and the bug found on the way), commit
`525dc1a`. What changed and what did not is set out under "The
reproducible mode" below. The user-facing default is untouched by this
amendment. The CPU back end the paced mode
defaults to is **decided: the hook-free interpreter stays the default**,
per the measurements in `docs/bus-fast-path-plan.md` step 7.2 -- `batch`
(`run_batch` over a `FastMem` window, with or without the `jit` feature)
is available as an explicit opt-in (`--cpu-backend batch`) with two
measured, open costs (a `Wait 5` timer-latency regression, and a
host-side workaround for a `run_batch` STOP-wake gap the fork itself
does not close); see that section for the numbers, which
were corrected after review (the first busy-MIPS figures were
idle-dominated). This status line
records what was measured, not a re-opened decision.

**Context:** `docs/bus-fast-path-plan.md` (steps 1–4: the host-side
speed work this ADR changes the target of); ADR 0001 (bare metal versus
Linux host, still open); `crates/machine-hosted/src/run.rs` (the run
loop); `MachineBus::tick` and its lazy next-event deadline.

---

## The question

The CPU is budgeted as a 14.19 MHz 68040 against the beam:
`CPU_CLOCKS_PER_COLOUR_CLOCK = 4`, and every retired instruction's
cycle count advances the beam, both CIAs, VERTB, TOD and the device
engines. The guest therefore gets about 4.5 M instructions per *guest*
second however fast the host is. Nothing paces the runner or the boards
to real time, so every host speedup in the fast-path plan so far has
made guest time run faster than wall-clock time: the benchmark boot
covers 30 guest seconds in about 3 real ones. A user never sees that as
speed. They see a machine whose clock runs ten times fast, and a guest
CPU that is exactly as fast, relative to its own timers, as a 14 MHz
040.

What a user wants from a machine like this is the fastest guest the
host can deliver in real time: WinUAE's "fastest possible" CPU setting.
The CPU runs unbudgeted, device time follows the wall clock, and the
CPU gets as many instructions as the host can deliver between device
events. The metric that matters is guest instructions per real second.

But the cycle-budgeted model is also what makes this project's evidence
work. Every real-ROM gate, screenshot baseline and QEMU marker check is
frame-count based and has to reproduce exactly, run to run and machine
to machine. A wall-clock-paced guest cannot give that.

## Decision

Two modes, selected per run, same machine core:

- **Cycle-budgeted** (`--cpu-speed cycle`, the default). Today's
  behaviour, unchanged: the CPU is charged cycles per instruction,
  device time is those cycles, runs are bit-reproducible. *Amended
  2026-09-28:* this mode is still supported and still bit-reproducible
  for m68k-rs, but it is no longer the project's definition of
  "deterministic" — see "The reproducible mode" below. Gates are
  migrating to `fixed`; the QEMU boards stay here for now.
- **Wall-clock-paced, fastest possible** (`--cpu-speed max`, hosted
  runner first). Device time advances by real elapsed host time; the
  CPU is not charged cycles against the beam at all.
- **Fixed-instructions-per-line** (`--cpu-speed fixed`, added
  2026-09-28). The run loop retires exactly `N` instructions per raster
  line, then advances the beam, both CIAs and the per-line device
  engines by one line period; while the CPU is stopped, lines advance
  and nothing retires, exactly as cycle mode's STOP already does. No
  cycle accounting of any kind is involved. **This is the project's
  reproducible mode** — see below.

The machine core does not grow a second timing model. Device time stays
in the same units it has now (14.19 MHz CPU-clock equivalents, so every
device keeps its existing arithmetic) and `MachineBus::tick(clocks)`
stays the only way it advances. The two modes differ only in where the
`clocks` argument comes from: retired-instruction cycles in cycle mode,
real elapsed time × 14.19 MHz in max mode.

## The reproducible mode (amended 2026-09-28)

Before this amendment, "deterministic" in this project meant
cycle-budgeted: the same run retires the same instructions because every
instruction is charged the same cycles. That definition is only
available to a core that *has* cycle tables. `docs/cpu-core-proposal.md`
proposes replacing m68k-rs with a core that deliberately has none, so
the definition had to move or the gates could never be shared between
the two cores.

**`fixed` is now the definition of reproducible for tests and gates.**
`cycle` mode remains supported, unchanged, and remains bit-reproducible
for m68k-rs — it simply stops being what the word means.

Precisely what each offers, because the two are not the same guarantee:

- **`fixed`** is reproducible *across `GuestCpu` implementations*,
  including cycle-table-free ones. Any core that can retire a counted
  number of instructions and report the count can implement it and reach
  the same machine state.
- **`cycle`** is reproducible *for a core with m68k-rs's cycle tables*,
  and additionally models real per-instruction cost. `fixed` does not
  offer that and does not claim to; a delay loop calibrated against real
  instruction timing is not what this mode preserves.

**`N` is measured, not chosen.** Its default is 424, from what cycle
mode actually retires per non-stopped raster line on the
boot-to-Workbench-ready reference workload (424.27 at 4400 frames,
424.96 at 6000, 0.16% apart). Choosing it this way is what lets existing
frame-count budgets carry over instead of being re-baselined from
scratch. `N` is **workload-dependent** — a CPUBench-heavy run measures
527 — so 424 is anchored to that reference workload, not a universal
constant, and is overridable with `--instructions-per-line`.

**Migration status, stated honestly.** 13 of ~27 real-ROM gate
categories have been verified under `fixed` at their existing
`--max-frames`, several matching their `cycle`-mode reference pixel
counts byte for byte. Four combination gates are unverified, and
CPUBench stays `max`-only by design because it self-calibrates against
the wall clock. So this amendment sets the direction and the definition;
it does not claim the migration is finished.

The strongest evidence that the mode is sound is a convergence rather
than a passing test: the reference boot reaches the *identical* final PC
under `fixed` and under `cycle`, at 41,871,237 instructions against
41,894,978 — 0.06% apart, by two timing models sharing no mechanism.

### Named hazard: never advance device time in whole-period lumps

The first implementation of this mode ticked `MachineBus` once per line,
by exactly one line period. That is an exact multiple of the modulus the
beam's horizontal position wraps on, so `hpos` returned to the same
value after every tick and never appeared to move. Kickstart's `VHPOSR`
delay loop polled 870 million times for a threshold it could not
observe, and the boot stopped progressing at frame 1950 while still
retiring instructions and tripping no wedge detector. It was determinis-
tic and wrong, and its own determinism checks passed throughout.

**The general rule, which applies to any future mode or any second
implementation of this one:** device time must not advance in lump sums
equal to a periodic boundary in the device model. Any register whose
period divides the quantum freezes, not just `VHPOSR`. The fix here
distributes a line's clocks across that line's instructions by exact
integer rasterization from the instruction's index within the line —
never from which opcode retired, so it stays cycle-table-free — summing
to exactly one line period per line.

This is recorded as a hazard in the ADR, not only in
`docs/deterministic-mode.md`, because the natural implementation of a
faster batch-oriented `fixed` mode would reintroduce it.

### What this mode does not change

It does not, by itself, resolve max mode's mid-batch IPL-injection gap
for the C2 replay log (`docs/cpu-core-trait.md`). `fixed` mode injects
at an exact retired-instruction index because it runs on the *hooked*
path, not because a raster line is a natural batch-sizing unit. Max
mode's unhooked batches are unaffected, and the C2 replay design still
has to address them.

## The paced mode's timing model

### Device time moves one event at a time

`MachineBus` already knows how many clocks remain until the next event
that can change guest-visible interrupt state on its own: the next
raster line boundary, either CIA's next timer underflow, or the next
keyboard handshake step (the lazy tick's `next_event_clocks`). Max mode
exposes that and never advances device time past it in one step:

1. Read the deadline `d` (clocks) and turn it into a real-time target:
   `t_event = t_device + d / 14.19 MHz`.
2. Run the CPU until the host clock reaches `t_event` (or the CPU stops,
   or the guest requests a boundary, below).
3. Advance device time by exactly `d`, sample `pending_irq_level()`,
   set the CPU's IPL, and go round again.

Advancing one event at a time is not optional. The STOP path in
`run.rs` already learned this the hard way: ticking in large gulps
collapsed distinct CIA underflows into one ICR event, let a freshly
raised VERTB shadow a lower-level CIA interrupt, and ran every
VBlank-counted OS timeout slow. The paced mode keeps the one-event
granularity for the same reasons.

### How interrupt latency is bounded

An interrupt is seen by the CPU at the first batch boundary after it is
raised. There are three sources, each bounded separately:

- **Device events** (VERTB, CIA underflow, line-rate engines): the batch
  ends at the event's real-time target by construction, so the IPL is
  set at the event, give or take one check interval.
- **Check interval.** The CPU cannot read the host clock per
  instruction without giving back the speed this mode exists for, so it
  runs in chunks (hook-free `run_for_cycles` with a chunk budget) and
  checks the clock between chunks. The chunk size is adaptive, sized
  from the measured instruction rate to a target of about 50 µs real
  time. That is the worst-case lateness of any device event, and it is
  well under one raster line of real time (64 µs).
- **Guest writes that raise or unmask an interrupt** (INTREQ and INTENA
  writes, CIA ICR/CRA/CRB writes, any device register write whose
  handler calls `raise_int`): these change `pending_irq_level()` in the
  middle of a chunk, and software interrupts (`Cause()`) depend on them
  being seen immediately. The bus sets a boundary request whenever a
  write changes `pending_irq_level()`, and `run_for_cycles` already ends
  the chunk on `AddressBus::take_boundary_request`. `run_batch` does
  **not** honour boundary requests today, which matters for step 5:
  either the fork learns to, or `run_batch` chunks are kept short enough
  that the check interval above bounds this case too.

### STOP sleeps

In cycle mode a stopped CPU is resynced by ticking device time in
one-line slices, which is correct and cheap because guest time is not
real time. In max mode a stopped CPU **sleeps**: the host parks until
`t_event` for the next device deadline, or until host input arrives
(keyboard, mouse, `--serial-tcp`, stdin), whichever is first, then
advances device time to the real now, one event at a time. Kickstart's
idle loop is a STOP, so an idle Workbench costs the host close to
nothing, which is the behaviour a user expects of a machine that isn't
doing anything.

### When the host can't keep up

A debug build, a loaded host, or a slow board can fall behind real
time: device events come due faster than the CPU can run between them.
Advancing device time to the wall clock regardless would starve the
guest. A VERTB or CIA handler might never finish before the next one
fires, and the machine would spend all its time taking interrupts.

The paced mode therefore guarantees the CPU a **minimum share between
events** and lets guest time **slip** rather than catch up:

- after processing an event, the CPU runs at least a minimum chunk
  before the next event may be processed, even if that event is already
  overdue;
- the backlog of overdue device time is capped (about 100 ms). Beyond
  the cap, the excess is discarded: device time jumps forward to
  wall clock minus the cap, rather than being replayed as a burst of
  back-to-back interrupts.

The result is WinUAE's honest failure mode. On a host that can't keep
up, the guest runs slow and its clock falls behind, but it never livelocks
in its own interrupt handlers. Both numbers are tuning constants, to be
set from measurement, not guessed here.

## What is given up in max mode

- **Cycle fidelity.** The CPU/beam relationship no longer exists: a
  guest can execute any number of instructions per raster line. Code
  that races the beam with the CPU (not the copper) breaks. The planar
  chipset is capped and RTG is the display path, so little that matters
  here does that.
- **CPU-speed-tuned delay loops.** Busy-wait delays that count
  instructions rather than read a timer run as fast as the host allows.
  OS delays use timer.device and CIA timers, which stay on real time;
  games and demos that assume a fixed CPU speed do not, and are not
  what this machine is for.
- **Reproducibility.** Two max-mode runs execute different instruction
  counts per frame and differ in any timing-dependent outcome.
  `--max-frames` means roughly real seconds × 50. Max-mode tests assert
  only order-independent evidence (serial markers, screenshots of a
  settled screen, timer accuracy within a tolerance), never frame
  counts.
- **The per-instruction hook.** Its duties move to chunk boundaries:
  wedge detection samples the PC per chunk against wall clock (the
  tight-loop detector's per-PC instruction count cannot survive),
  instruction and frame limits are checked per chunk, and serial/input
  scripts and screenshots are serviced on frame changes.

## The CPU back end: measured (plan step 7.2)

Max mode is useful on day one with the hook-free interpreter, because
the hook plus per-instruction tick was about 35% of the last profile.
Two further steps are available in the pinned fork and were measured
(plan step 7.2, "Numbers"), on an Apple M3 Pro, one real Kickstart
3.2.2 A1200 boot/HDF pair, `--label interp`/`batch`/`batch+jit`:

- **`run_batch` over a `FastMem` window.** Implemented
  (`--cpu-backend batch`; `MachineBus::fast_ram_window_mut`, never a
  constant). Window coverage, measured with `BUS_COVERAGE=1` over a boot
  plus Shell-open-and-`LIST` activity: **64.1% of instruction fetches
  land in fast RAM, 35.7% in ROM** (data accesses: 93.6%/1.2%). One
  window over fast RAM alone is *not* enough on its own to make this
  workload's hot path mostly in-window -- a third of fetches are
  Kickstart's own ROM-resident code, called constantly by anything
  running from fast RAM. **A ROM shadow in fast RAM is called for by
  this number and was not built** (plan step 7.2's own gate: report,
  don't build, unless the numbers say so -- they do, but building it was
  out of this pass's scope).

  `run_batch` also could not wake a stopped CPU on its own -- a real
  hang, not a latency bound, traced to `run_batch_inner`'s stopped check
  never calling `check_and_service_interrupts` the way `run_for_cycles`/
  `execute` do before their own. Fixed on the host side (a minimal
  `run_for_cycles(bus, 4)` probe after every device-time advance while
  stopped); see the plan's Results for the before/after evidence. What
  the fix does *not* close: the `Wait 5` timer gap is 5.739s under batch
  versus interp's 5.088s (tolerance is ±0.5s around 5s) -- **`run_batch`
  ignoring boundary requests has a measured cost, not just a theoretical
  one**, and closing it further would need either fork support for
  boundary requests inside `run_batch`, or a shorter cycle-aware chunk
  bound than this pass implemented.

  Busy MIPS, measured over the busy part of the boot (the figures first
  recorded here were idle-dominated and are corrected in the plan's 7.2
  results): interp 30.7/29.9, batch 30.0/30.1, batch+jit 31.3/31.1 over
  two runs each. (A separate `slept`-accounting bug in `run_guest_max`,
  unrelated to the idle-window issue above, was found and fixed
  2026-09-28 -- see `docs/bus-fast-path-plan.md` step 7.2's results.
  These particular figures, measured over the narrow active-boot window
  rather than a whole idle-heavy run, were confirmed by that fix to be
  essentially unaffected by it; no correction needed here.) No window
  exists while the 040 MMU is enabled (not exercised here; this machine
  does not yet enable it).

- **The trace JIT** (`jit` feature, Cranelift). Added as an
  off-by-default `machine-hosted` Cargo feature (`m68k/jit` on this
  crate's own dependency only; confirmed inert on both boards via
  `cargo tree -e features`). Compiled traces keep a copy of the exact
  code bytes they were compiled from and compare them on entry, and
  trace stores into their own code range bail. Code that hostblk or
  pktport DMA writes into fast RAM (every LoadSeg) is therefore caught
  on the next entry, **provided device DMA only ever runs between CPU
  batches**, which is true of this design (the engines run inside
  `tick`, never mid-batch) and must stay true. The regression test
  (`crates/machine-hosted/tests/jit_trace_regression.rs`) loads and runs
  a hand-assembled program over fast RAM that previously held a
  different one and confirms the second run executes the new code, not
  a stale compiled trace -- passes. Measured effect on top of batch: about
  +4% busy MIPS over busy boot work (31.3/31.1 against 30.0/30.1), and
  no change to the `Wait 5` gap (5.738s).

**Decision: `interp` stays the default.** Over busy work the three back
ends are within about 4% of each other (about 30 busy MIPS), and only
`interp` has no correctness caveat. The fork's fast paths are therefore
not the route to the speed target; `docs/cpu-core-proposal.md` is the
candidate that is. `batch`/`batch+jit` remain available opt-in
(`--cpu-backend batch`) with the two open costs above disclosed.

## Interaction with ADR 0001

Max mode needs two things from its platform: a monotonic clock with
microsecond resolution, and a way to sleep until a deadline or an input
event.

- **Hosted, and ADR 0001's option B (Linux as firmware):** both come
  from the OS for free.
- **Option A (bare metal):** each board needs a clock source (the
  architected counter on aarch64, TSC or HPET on x86-64), a timer
  interrupt, and WFI/HLT idling with that interrupt and the input
  devices as wake sources. None of that exists on either board today.

The JIT sharpens this. Cranelift is `std`-only, so under option A the
fastest mode is the interpreter; under option B it is the JIT. That
makes max-mode MIPS a new, directly measurable input to ADR 0001's
Phase 4 decision. It does not make the decision. The trigger to record
there is interpreter max-mode MIPS against JIT max-mode MIPS on the
RK3588, and whether the difference is worth the host OS.

The boards stay in cycle mode until ADR 0001 is decided or a board
grows the timer and idle support above.

## Evidence required before max mode is called working

- All 18 real-ROM tests, the pktport proofs and both QEMU boards still
  pass, in cycle mode, unchanged.
- A max-mode boot to Workbench with scripted input and serial markers.
- timer.device keeps real time: a serial script echoes a marker, runs
  `Wait 5` in a Shell and echoes a second marker; the host timestamps
  both and asserts the wall-clock gap is 5 s within a stated tolerance.
- An idle Workbench in max mode uses little host CPU (STOP sleeps).
- Benchmarks in the plan: wall clock to Workbench under pacing, and
  guest instructions per real second, alongside the unpaced numbers.
