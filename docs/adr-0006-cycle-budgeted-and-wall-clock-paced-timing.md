# ADR 0006 — Two timing models: cycle-budgeted by default, wall-clock-paced for users

**Status:** accepted, 2026-09-27, for the split into two modes and for
the paced mode's timing model below. The CPU back end the paced mode
ends up on (hook-free interpreter, `run_batch` over a `FastMem` window,
or the Cranelift trace JIT) is **open**, pending the measurements in
`docs/bus-fast-path-plan.md` step 5.

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
  device time is those cycles, runs are bit-reproducible. All tests,
  real-ROM gates, screenshot baselines and the QEMU boards stay here.
- **Wall-clock-paced, fastest possible** (`--cpu-speed max`, hosted
  runner first). Device time advances by real elapsed host time; the
  CPU is not charged cycles against the beam at all.

The machine core does not grow a second timing model. Device time stays
in the same units it has now (14.19 MHz CPU-clock equivalents, so every
device keeps its existing arithmetic) and `MachineBus::tick(clocks)`
stays the only way it advances. The two modes differ only in where the
`clocks` argument comes from: retired-instruction cycles in cycle mode,
real elapsed time × 14.19 MHz in max mode.

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

## The CPU back end is a separate, measured decision

Max mode is useful on day one with the hook-free interpreter, because
the hook plus per-instruction tick was about 35% of the last profile.
Two further steps are available in the pinned fork and are measured
before either is relied on (plan step 5):

- **`run_batch` over a `FastMem` window.** The core accesses one
  side-effect-free RAM window through a raw pointer, with no bus call.
  It gets one window only. Fast RAM is the natural choice, but Kickstart
  runs from ROM and chip RAM goes through the bus, so the win depends on
  how much of a Workbench workload stays in the window. A shadow copy of
  ROM in fast RAM may be needed. There is no window while the 040 MMU
  is enabled, and no boundary requests (above).
- **The trace JIT** (`jit` feature, Cranelift). Compiled traces keep a
  copy of the exact code bytes they were compiled from and compare them
  on entry, and trace stores into their own code range bail. Code that
  hostblk or pktport DMA writes into fast RAM (every LoadSeg) is
  therefore caught on the next entry, **provided device DMA only ever
  runs between CPU batches**, which is true of this design (the engines
  run inside `tick`, never mid-batch) and must stay true. A regression
  test loads and runs a program over memory that previously held other
  code.

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

- All 17 real-ROM tests, the pktport proofs and both QEMU boards still
  pass, in cycle mode, unchanged.
- A max-mode boot to Workbench with scripted input and serial markers.
- timer.device keeps real time: a serial script echoes a marker, runs
  `Wait 5` in a Shell and echoes a second marker; the host timestamps
  both and asserts the wall-clock gap is 5 s within a stated tolerance.
- An idle Workbench in max mode uses little host CPU (STOP sleeps).
- Benchmarks in the plan: wall clock to Workbench under pacing, and
  guest instructions per real second, alongside the unpaced numbers.
