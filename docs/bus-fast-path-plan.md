# Hosted runtime speed: implementation plan

**Status:** plan, 26 September 2026. Supersedes the "MachineBus fast path
— sketch" for sequencing; that sketch's §2 design is adopted as step 3
below. Core-independent: no change to the `m68k` fork.
**Applies to:** `crates/machine-core/src/lib.rs`, `autoconfig.rs`,
`crates/machine-hosted/src/{bus.rs,run.rs}`, root `Cargo.toml`.

## 0. Where the time goes today

Measured on an M3 Pro, release build, Kickstart 3.2.2 (A1200 ROM),
`--cpu 68040` default, `--max-frames 1500 --max-instructions 0`,
profiled with `samply` and symbolized against the binary:

| Config | Instructions | Wall | MIPS |
|---|---|---|---|
| ROM + fast RAM only ("bare") | 136 M | 12.3 s | 11 |
| `--pcibridge --rtgboard 640x480 --hostblk aros.hdf` ("full") | 88 M | 8.7 s | 10 |

Self-time share, bare run: `MachineBus::read_byte` 21.7%,
`dispatch_instruction` 12.9%, `write_byte` 6.2%, hook closure 6.2%,
`AutoConfig::board_at` 4.9%, `Cia::tick` 4.8%, `cycles_040` 4.2%,
`MachineBus::tick` 3.4%, `read_word` 3.2%, `graphics_target` 2.8%,
`trace_serial` 2.3%, `fast_ram_target` 2.2%, `service_host_serial`
2.1%, `drain_serial` 1.7%, `mirage_target` 1.6%.

Full run additions: `board_at` rises to 8.4% (134 of 198 samples from
`read_word` via `pcibridge_aperture_sized`), `MachineBus::tick` is 27.6%
inclusive: memmove from `Option::take()` 6.4%, `intx_levels` 5.5%,
`Cia::tick` 4.8%, virtio-net tick 3.3%, `Hostblk::tick` 1.5%.

Three buckets, in order of size: the bus chain (~45% bare), the
per-instruction tick and hook (~20% bare, ~40% full), the build profile.

## 1. Measurement harness (do first, ships with every step)

Every step below is accepted on numbers, not on tests passing. Silent
failure is this platform's norm; a speedup that isn't measured isn't
one.

1. `scripts/bench-boot.sh`: runs `target/release/machine-hosted` on
   `$M68K_KICKSTART_A1200` for the two configs above at 1500 frames,
   prints wall clock, instruction count and MIPS from the `LIMIT REACHED`
   line. Fails loudly if the ROM is absent (same posture as
   `phase3-gate.sh`). Optional `--profile` flag wraps the run in
   `samply record --save-only`.
2. `scripts/bench-symbolize.py`: the aggregator used for §0 (self and
   inclusive time per function from a samply profile, symbolized with
   `atos`). Committed so before/after profiles are comparable.
3. Record the §0 numbers in this file's "Results" section as the
   baseline. Each subsequent step appends its own row.

Done when: the script reproduces §0 within noise on the untouched tree.

### How to run

```sh
scripts/bench-boot.sh --label baseline
scripts/bench-boot.sh --profile /tmp/bench-profile
```

The first prints the two configs' wall clock, instruction count and
MIPS, plus a Markdown row for the Results table below. The second
additionally records each run with `samply record --save-only` and
symbolizes both profiles with `scripts/bench-symbolize.py` into
`<dir>/{bare,full}.txt`. `--no-build` skips the release rebuild;
`--help` lists every flag. Env vars: `M68K_KICKSTART_A1200` (ROM,
default `nondistribution/A1200.47.115.rom`), `M68K_BENCH_HDF` (hostblk
image, default `nondistribution/aros/aros.hdf`), `FRAMES` (frame count
for both configs, default `1500`).

## 2. Release profile (zero-code, ~10%)

Add to the root `Cargo.toml`:

```toml
[profile.release]
lto = "fat"
codegen-units = 1
```

Measured: bare 12.27 → 10.69 s, full 8.67 → 7.95 s.

Checks: the board crates share the workspace profile. Build both with
their explicit `--target`, run `scripts/run-qemu-virt.sh` and
`run-qemu-q35.sh`, assert markers with `check-serial-markers.sh`. Note
in `docs/ci.md` that release builds are LTO and why. Do **not** add
`panic = "abort"` here (`.cargo/config.toml` explains why).

## 3. Bus fast path (the sketch's §2, with three additions)

### 3.1 Cache placements once, scan never on the hot path

In `MachineBus`, add a small cached-window table refreshed from
`AutoConfig` whenever a placement can change. There are exactly two
mutation sites: `AutoConfig::configure` (reached only through the
AUTOCONFIG arm of `write_byte`) and the `with_*` builders. Do not
thread a generation counter; call `refresh_windows()` at the end of the
`write_byte` AUTOCONFIG arm and at the end of `with_fast_ram`.

```rust
/// Fast RAM's placed window, `None` until expansion.library places it.
fast_window: Option<(u32, u32)>,          // (base, len)
/// pcibridge's placed window, same shape.
pcibridge_window: Option<(u32, u32)>,
```

Both derive from `autoconfig.placement(idx)`; never from a constant
(`docs/device-ledger.md`, standing rule 2). `debug_assert!` in
`refresh_windows` that neither window overlaps `AUTOCONFIG_BASE..END`.

### 3.2 Width-native fast path in front of the chain

Exactly the sketch's §2.2: `fast_region(&self, addr) -> Option<(&[u8],
usize)>` covering fast RAM (via `fast_window`), chip RAM (except reads
under the overlay), and ROM when `rom.len() == ROM_WINDOW_SIZE`;
`fast_region_mut` for writes covering fast RAM and chip RAM (writes
under the overlay still land in chip RAM, as today). `read_{byte,word,
long}` and `write_{byte,word,long}` try it first with
`get(off..off+N)` so an end-of-region straddle falls through to the
existing byte path unchanged.

Pin with `const _: () = assert!(CIA_BASE >= CHIP_RAM_END &&
CUSTOM_BASE >= CHIP_RAM_END);`.

### 3.3 Move `pcibridge_aperture_sized` behind the fast path and off `board_at`

`read_word`/`read_long`/`write_word`/`write_long` currently call
`pcibridge_aperture_sized` (a full `board_at` scan) before anything
else. Reorder it after the fast path, and reimplement
`pcibridge_target` against `pcibridge_window` (subtract-and-compare, no
scan). This is the single largest bus item in the full config and the
sketch does not address it.

### 3.4 One `board_at` per device access

Replace the `graphics_target → mirage_target → fast_ram_target →
hostblk_target → input_target → pktport_target → rtg_target →
pcibridge_target` ladder (up to seven scans per access, and rtgboard
VRAM writes are Workbench drawing) with one `board_at` call, then a
match of the returned index against the stored `*_board` indices.
Behaviour is identical: the chain order today is only meaningful for
Graffity, whose `graphics_boards` lookup keeps its own position check.

### 3.5 Hosted wrapper

`bus.rs`: replace `serial_trace_enabled()`'s `OnceLock` lookup per
access with a `bool` field on `Bus` set at construction. `trace_serial`
becomes `#[inline]` with the bool as its first check.

### Tests for step 3

- Existing: the 43 `lib.rs` unit tests, in particular
  `overlay_maps_rom_at_zero_until_ovl_is_cleared`,
  `undersized_rom_mirrors_across_the_window`,
  `configured_fast_ram_routes_reads_and_writes_and_clips_past_the_real_buffer`,
  `open_bus_*`, and every `configured_*_routes_*` device test, must pass
  unchanged. They are the spec for what the fast path must not swallow.
- New: a straddle test (word at `CHIP_RAM_END - 1`, long at fast RAM
  end - 2) equal to the byte-composed result; a test that a placement
  change (re-running AUTOCONFIG on a fresh bus) invalidates
  `fast_window`; a test that an undersized ROM (not exact fit) still
  mirrors; a differential test that walks every 4 KB of the address map
  comparing fast-path and byte-path results for byte/word/long with all
  devices attached.
- End to end: `cargo test -p machine-hosted` real-ROM suite, both QEMU
  smoke scripts, and `scripts/phase3-gate.sh` where the media exists.

Done when: bare and full rows in Results, and the post-step profile
shows `read_byte` + `board_at` + `*_target` under 10% combined.

## 4. Per-instruction tick and hook

### 4.1 Batch the device engines per raster line (machine-core)

`MachineBus::tick` runs hostblk, pktport, mirage, pcibridge (virtio-net
ring walks) and the two IRQ polls on every retired instruction. Their
contracts are already "deferred completion, one step per call"; one
raster line of latency (~908 CPU clocks, ~100 instructions) is
invisible to the guest and is exactly the grain the STOP-path resync
already uses (`STOP_TICK_SLICE`).

Inside `tick`, accumulate `cpu_clocks` into `engine_carry` and run the
four engines and their `irq_pending()` polls only when
`beam.lines_started > 0`. Chipset and CIA ticks stay per call. Because
this lives in `MachineBus::tick`, both bare-metal boards (which have
their own copies of the hook loop, `board-qemu-virt/src/main.rs:724`,
`board-qemu-q35/src/main.rs:623`) get it without change.

Update the "one step per call" sentences in `hostblk.rs`, `pktport.rs`,
`mirage.rs` and `pcibridge.rs` module docs and in `docs/*-protocol.md`
where they promise per-tick servicing, in the same commit.

Tests: hostblk and pktport unit tests that count ticks-to-completion
must be re-expressed in lines (tick with a line's worth of clocks).
`hostblk-soak`/devsoak and `pktport-e2e.sh` are the acceptance gates.

### 4.2 Stop moving devices through `Option::take()`

Split `MachineBus`'s RAM into a `GuestRam<'a> { chip_ram, fast_ram,
fast_window }` field that implements `GuestMemory` by itself. Device
engines then take `&mut self.ram` while `self.hostblk` is borrowed,
removing the take/put-back (a ~400-byte memmove twice per device per
tick, 6.4% of the full run) and the comments explaining it. `ram_slice`
/`ram_slice_mut` move to `GuestRam`; `MachineBus` keeps a delegating
impl so `screenshot.rs`, `introspect.rs` and `pktvol.rs` are untouched.

### 4.3 CIA: skip the E-clock loop when idle

`Cia::tick_eclock` loops once per E-clock. Add an early return when
neither timer is started (`cra & CRA_START == 0 && crb & CRB_START ==
0`) and the keyboard queue is empty; otherwise unchanged. Cheap and
exact. A full next-event computation is not worth it until the profile
says so.

### 4.4 Hook closure (hosted only)

In `run.rs`'s hook:

- test `bus.0.chipset.frames != last_serviced_frame` at the call site
  before calling `service_host_serial`; the function keeps its own
  check for the STOP path.
- `drain_serial`: `#[inline]`, and check `serial_len == 0` via a cheap
  `has_serial_byte()` before the loop.
- `LAST_PC.store` only when the serial trace bool from 3.5 is set.
- `screenshot_job.maybe_capture` only when `frames` changed.

Mirror the first three in the two board `main.rs` loops where they
exist.

Done when: `MachineBus::tick` inclusive is under 5% in the full run and
the hook closure's self time is under 3%.

## 5. Re-profile and decide the CPU-core question

Superseded as a stand-alone step by step 7: the CPU back-end question
is now asked of the wall-clock-paced mode, where it matters to users,
and answered by step 7's measurements. The two facts recorded here
still hold:

- The fork already has `run_batch` with a `FastMem` window (single
  side-effect-free RAM region, direct host pointer, no bus call per
  access). It has no per-instruction hook and clobbers cycle
  accounting, which conflicts with this machine's cycles-to-beam
  timebase, and the contract forbids any interception in-window, so
  `--blitter-trace` and `SERIAL_REG_TRACE` must disable it.
- `cycles_040` (4%) is the 68040 timing table cost; a `--cpu 68030`
  run is a one-flag experiment to size it.

## 6. Sequencing and ownership

| Step | Size | Crates touched | Gate |
|---|---|---|---|
| 1 harness | S | scripts | reproduces §0 |
| 2 LTO | XS | Cargo.toml, docs/ci.md | QEMU markers |
| 3 bus fast path | M | machine-core, machine-hosted | unit + real-ROM + QEMU + gate |
| 4.1–4.3 tick | M | machine-core (+ protocol docs) | soak, pktport-e2e, QEMU |
| 4.4 hook | S | machine-hosted, boards | real-ROM |
| 5 re-profile | S | docs | numbers (superseded by 7) |
| 7.1 max mode | M | machine-core (deadline, boundary requests), machine-hosted | cycle-mode gates unchanged; max-mode Workbench boot + `Wait 5` timer check |
| 7.2 run_batch/JIT | M | machine-hosted (+ fork if boundary requests needed) | same, plus the DMA-over-code regression test |

Steps 2, 3 and 4 are independent and can go to separate workers, each
on its own branch, each landing with its Results row. Step 3 is the
one that needs the most review: every change to `read_byte`'s chain is
a change to what every device sees, and the unit tests are
self-consistent (they have passed over real bugs before), so the
real-ROM boots and the QEMU markers are the evidence, not `cargo test`.

Commit discipline per `CLAUDE.md`: fmt and clippy (`-D warnings`, both
hosted crates and both board targets) before every commit; ledger,
protocol docs and this file's Results table updated in the commit that
makes them true.

## 7. Wall-clock-paced "fastest possible" mode (ADR 0006)

Change of target. Steps 1-4 made the cycle-budgeted machine faster, but
that machine gives the guest about 4.5 M instructions per *guest*
second however fast the host is, so host speed only makes guest time
outrun the wall clock. `docs/adr-0006-cycle-budgeted-and-wall-clock-paced-timing.md`
records the decision: cycle-budgeted stays the deterministic default
for every test and gate, and a `--cpu-speed max` mode paces device time
to the wall clock and runs the CPU unbudgeted. The metric for this step
is **guest instructions per real second**, not wall clock per 1500
frames.

### 7.1 Hosted `--cpu-speed max`, hook-free interpreter

Implement ADR 0006's timing model in `run.rs`, cycle mode unchanged:

- expose `MachineBus`'s next-event deadline; advance device time one
  event at a time, never past a deadline in one step;
- run the CPU with hook-free `run_for_cycles` in adaptive chunks sized
  to ~50 µs of real time, checking the host clock between chunks;
- the bus requests a batch boundary whenever a write changes
  `pending_irq_level()` (INTREQ/INTENA, CIA, device register writes),
  and the runner sets the IPL at every boundary;
- STOP sleeps until the next deadline's real time or host input;
- minimum CPU share between events and a ~100 ms backlog cap, so a
  host that can't keep up slows the guest instead of livelocking it;
- the hook's duties (wedge detection, limits, serial/input scripts,
  screenshots) move to chunk boundaries.

### 7.2 `run_batch` + `FastMem`, then the JIT

Measure, in max mode, first `run_batch` with a `FastMem` window over
fast RAM, then the same with the fork's `jit` feature. Before relying
on either: measure what share of a Workbench workload's fetches and
data accesses fall in the window (a ROM shadow in fast RAM may be
needed); no window while the 040 MMU is on; `run_batch` ignores
boundary requests, so either the fork learns to or chunks stay short;
and a regression test loads and runs a program over fast RAM that
previously held other code, proving trace validation catches DMA-loaded
code. No JIT on the bare-metal boards (Cranelift is `std`-only).

### 7.3 Benchmarks for this step

Added alongside, not instead of, the existing 1500-frame rows (which
stay the cycle-mode regression yardstick):

- **Guest MIPS per real second**: instructions retired over a fixed
  real-time window of a max-mode Workbench workload, and separately
  over the boot.
- **Wall clock to Workbench** under pacing: host time from start to the
  Workbench-ready serial marker, cycle mode versus max mode.
- **Idle cost**: host CPU time over 30 s of idle Workbench in max mode
  (STOP should sleep, so this should be near zero).
- **Timer accuracy**: the `Wait 5` gap measured on the host, per run.

### 7.4 Evidence

Cycle mode: all 17 real-ROM tests, both pktport proofs, both QEMU
boards through the CI marker checks, unchanged. Max mode: a boot to
Workbench with scripted input and serial markers, and the `Wait 5`
timer check within a stated tolerance, as real-ROM tests gated to max
mode.

## Results

| Step | Bare 1500f | Full 1500f | Notes |
|---|---|---|---|
| baseline | 12.27 s (11.1 MIPS) | 8.67 s (10.2 MIPS) | §0 profile |
| 2 LTO | 11.025 s (12.4 MIPS) | 8.065 s (11.0 MIPS) | landed; `cargo build --release -p machine-hosted`, `--no-build` bench |
| 3 bus fast path, before (this tree, no LTO) | 11.756 s (11.6 MIPS) | 8.390 s (10.5 MIPS) | matches §0 within noise on this machine |
| 3 bus fast path, after | 6.566 s (20.8 MIPS) | 5.805 s (15.2 MIPS) | `read_byte`+`board_at`+`*_target` self time: 0.84%/0% bare, 0.94%/0% full -- both under the 10% done criterion, `board_at`/`*_target` no longer appear in either profile's top 30 at all |
| 2 + 3 on `main`, idle machine | 5.247 s (26.0 MIPS) | 5.154 s (17.2 MIPS) | best of two consecutive runs (bare 5.80/5.25 s, full 5.18/5.15 s); 2.3x / 1.7x over baseline |
| 4 tick+hook | 11.857 s (11.5 MIPS) | 8.285 s (10.7 MIPS) | GuestRam split, CIA idle-skip, hook trims landed; MIRAGE alone batched to once/line. **hostblk/pktport/pcibridge's per-line batching (4.1) had to be reverted**: it reliably wedges a real Kickstart 3.2.2 boot before Startup-Sequence (real-ROM virtio-net test), so `MachineBus::tick`'s full-run inclusive is 19.12%, not under the 5% target -- see step 4's own note at the `tick` call site and the "Un-batch..." commit. Full's hook-closure self is 7.31% (bare 7.75%), also short of the <3% target: the remaining cost is per-instruction bookkeeping (tight-loop/progress/limit checks) 4.4 did not touch, not the trimmed serial/screenshot paths. |
| 2 + 3 + 4 on `main`, idle machine | 5.144 s (26.5 MIPS) | 4.035 s (21.9 MIPS) | 2.4x / 2.1x over baseline. Full-config profile after: `tick` 26.7% inclusive (`Cia::tick` 7.3%, `intx_levels` 5.5%, virtio tick 2.0%), hook closure 16.2% self -- both still above step 4's gates; the unlanded 4.1 batching of hostblk/pktport/pcibridge is the open item |
| 4.1 batching, before (`main` 544ae98, same session) | 4.918 s (27.7 MIPS) | 3.998 s (22.1 MIPS) | best of two, interleaved with the "after" runs below. The machine was not fully idle (1-min load ~3 from system services, not this workload); interleaving keeps the two sides comparable |
| 4.1 batching, after | 4.746 s (28.7 MIPS) | 3.030 s (29.2 MIPS) | hostblk/pktport/pcibridge now ride MIRAGE's once-per-line gate. Full is 24% faster; bare has no devices and moves only within noise. The wedge that forced the earlier revert was a `hostblk.device` bug, not DoIO busy-polling: `BeginIO` never set `ln_Type = NT_MESSAGE`, so ROM FFS's reused IORequest still read as `NT_REPLYMSG` and DoIO returned before the read finished (see `m68k/hostblk-rom/hostblk-diagrom.s`, `dev_beginio`). virtio-net's failure was the net harness's 500,000-call cooldown, now 5 guest frames. Full-config profile: `tick` 14.07% inclusive (`Cia::tick` 8.93% self, `tick` 5.11% self); no hostblk/virtio/`intx_levels` frames remain. **Still not under the 5% target**: what is left is per-call CIA E-clock and beam work, not the device engines. `run_guest` (the hook closure, inlined) is 21.4% self, up in share because the total shrank |
| 4 lazy CIA/beam tick (this branch, `main` c7481b5 base), before | 4.862 s (28.0 MIPS) / 4.617 s (29.5 MIPS) | 2.941 s (30.1 MIPS) / 2.948 s (30.0 MIPS) | two runs on `main` c7481b5's own release build, interleaved with the "after" rows below on a machine under noticeable background load (bare varied 4.09-5.53 s across nominally identical runs on both sides, so wall clock alone is not trustworthy here -- the profile percentages below are the load-bearing numbers). Full-config profile: `tick` 15.25% inclusive, almost all of it `Cia::tick` (10.33% self) -- this is `MachineBus::tick`'s per-call CIA E-clock/beam-advance overhead the plan's step 4 set out to remove, confirmed still present on `main` going into this work |
| 4 lazy CIA/beam tick, after | 4.776 s (28.5 MIPS) / 4.089 s (33.3 MIPS) | 3.273 s (27.0 MIPS) / 2.693 s (32.8 MIPS) | `MachineBus::tick` now only accumulates `pending_clocks` and flushes (applies the exact per-call path, `tick_exact`) when accumulated clocks reach `next_event_clocks` -- the nearest raster line boundary or CIA timer/keyboard deadline -- rather than every retired instruction; every CIA/custom-register accessor flushes first, and a write that can newly assert or clear a still-latched level interrupt (`write_cia`, `write_custom_word`, Graffity's write arm) reasserts it immediately (`reassert_level_irqs`) so guest-visible interrupt timing is unchanged. Proved exact by a 1.6M-step differential test (`lib.rs`, `lazy_tick_matches_eager_tick_across_random_sequences`) against the old eager path, both driven by the same `tick_exact`. `MachineBus::tick`/`tick_exact` are fully inlined away under this workspace's `lto = "fat"`/`codegen-units = 1`, so `flush` is the closest surviving symbol for the whole batched application: full-config profile `flush` 3.41% inclusive (`cia::Cia::tick` 2.40% self, `recompute_next_event` 1.28% self), bare-config `flush` 2.30% inclusive (`cia::Cia::tick` 1.94%) -- both **under the 5% target**, down from 15.25%/10.33% on `main`. Wall clock is noise-dominated on this run (see the "before" row) and does not show a clean win or loss either way, which is expected: step 4.1 had already removed the device-engine share of `tick`'s cost, so this step's win is a profile-share reduction (CIA/beam overhead that was ~10-15% of the full run is now ~1-3%) rather than a further multiple on top of step 4.1's already-large wall-clock gain |
| 4 lazy CIA/beam tick, supervisor re-measure | 4.196 s (32.5 MIPS) vs `main` 4.693 s (29.0 MIPS) | 2.729 s (32.4 MIPS) vs `main` 3.024 s (29.2 MIPS) | best of two, alternating branch and `main` builds run by run; ~11% bare, ~10% full. `tick` inclusive in the full profile is now ~3.4% (`flush`), under the 5% gate |
| 7.1 `--cpu-speed max` | n/a (see below) | n/a (see below) | landed: `MachineBus::next_event_deadline_clocks`/`take_boundary_request`, `run_guest_max` in `machine-hosted`. The bare/full 1500f columns don't apply -- max mode's whole point is that wall clock for a fixed frame count is meaningless to compare across configs (it's `frames/50` regardless of host speed by design), so `scripts/bench-boot-max.sh` measures wall-clock-to-a-fixed-frame-count and guest MIPS instead. See the paragraph below the table for numbers and the evidence checklist. |

### 7.1 results

`scripts/bench-boot-max.sh --label step-7.1`, this machine, boot-to-4400-
frames on the real Kickstart 3.2.2 A1200 ROM/HDF pair (frame 4400 is an
idle Workbench desktop, mouse-interactive -- the same guest-frame count
the scripted-input real-ROM tests wait out before clicking):

| | wall clock | guest instructions | guest MIPS |
|---|---|---|---|
| cycle mode, 4400 frames | 1.614 s | 41,894,978 | 26.0 |
| max mode, 4400 frames | 87.892 s | 96,813,431 | 1.1 |

Wall clock to frame 4400 under max mode is ~54x cycle mode's, and lands
almost exactly on `4400 / 50 = 88 s` -- direct confirmation that device
time is paced to the wall clock, not running unboundedly fast the way
cycle mode's is. Guest MIPS *over the whole boot, wall-clock-averaged* is
1.1, far below cycle mode's 26.0 -- expected and correct, not a
regression: most of those 88 real seconds are STOP-parked (Kickstart's
insert-disk wait, the boot menu, idle dispatcher ticks between device
events), and a correctly-*sleeping* STOP retires no instructions during
that time. A STOP that busy-spun instead would show a guest MIPS figure
much closer to cycle mode's, padded with wasted work -- 1.1 is itself
evidence the sleep path is doing its job, not merely a slower number.

Idle host CPU (`/usr/bin/time -l`, one continuous run to frame 5400 --
4400 boot + 1000 further frames, 20 s, of idle Workbench): **user 3.46 s
+ sys 3.38 s over 107.82 s real, ~6.3% of one core averaged across the
whole run** (boot's own more-active first ~88 s folded in with the last
~20 s of genuine idle). Confirms the idle-cost requirement's spirit --
close to zero, not close to 100% -- but is not an idle-only isolate:
splitting "boot" from "idle" cost needs the runner to report an
intermediate checkpoint from inside one continuous process (e.g. an
`--inspect`-style dump at a chosen frame), which is not implemented and
is left as follow-up.

**A per-window guest-MIPS figure for the idle segment alone was attempted
and abandoned**: `scripts/bench-boot-max.sh`'s first version subtracted a
4400-frame run's own instruction count from a separate 5400-frame run's
count to isolate the last 1000 frames. Both numbers came back from
independent process launches, and max mode's own non-reproducibility
(ADR 0006, "What is given up in max mode") means two separately-launched
boots to the same frame count differ by more instructions (measured:
~750,000 here) than the entire idle window itself likely contains --
the subtraction went negative on both attempts. This is not a bug to fix
so much as the expected shape of an unbudgeted CPU: the fix is measuring
idle MIPS from *one* run's own internal accounting at two points, not
two runs' totals, which is the same follow-up as the paragraph above.

`Wait 5` timer accuracy (`crates/machine-hosted/tests/real_rom.rs`,
`max_cpu_speed_boots_to_workbench_and_keeps_real_time_across_wait_5`):
measured gap **5.093 s**, tolerance ±0.5 s (deviation +0.093 s). Same
test also stands as the required "max-mode boot to Workbench with
scripted input and serial markers" evidence.

Evidence checklist (ADR 0006, "Evidence required before max mode is
called working"):

- [x] All 18 real-ROM tests (17 pre-existing + the new max-mode one),
      both pktport proofs, and both QEMU boards pass in cycle mode,
      unchanged.
- [x] A max-mode boot to Workbench with scripted input and serial
      markers.
- [x] timer.device keeps real time (`Wait 5`, measured 5.093 s).
- [~] An idle Workbench in max mode uses little host CPU -- confirmed at
      whole-run granularity (~6.3% of one core over a window that
      includes boot), not yet isolated to the idle segment alone (see
      above).
- [x] Benchmarks: wall clock to a fixed frame count and guest MIPS, cycle
      vs max, above.

### 7.2 results

> **Supervisor correction (2026-09-27), read this first.** The busy-MIPS,
> idle-host-CPU and `WBREADY` figures in the tables below were measured
> over runs that are mostly idle, and do not measure what their labels
> say:
>
> - **"busy MIPS" of 2-4** is idle-dominated: over an 88 s run in which
>   the guest spends nearly all its time in exec's idle `STOP`, the
>   non-sleeping time is mostly per-wake overhead, not CPU throughput.
>   Measured over the busy part of the boot instead (max mode, hostblk
>   HDF, frames 0-200, two runs each, same M3 Pro), all three back ends
>   run at the same speed:
>
>   | back end | busy MIPS (run 1 / run 2) |
>   |---|---|
>   | interp | 30.65 / 29.88 |
>   | batch | 30.04 / 30.07 |
>   | batch+jit | 31.32 / 31.06 |
>
>   (The JIT binary was checked to be the Cranelift build: 3.8 MB against
>   1.7 MB, 1,438 Cranelift symbols against none.)
> - **Idle host CPU of ~30%** came from a 1000-frame run that includes
>   the whole boot. A max-mode run held at idle Workbench for 30 s after
>   boot costs about 1.1 s of host CPU over those 30 s, roughly 4% of one
>   core, so STOP does sleep.
> - **`WBREADY` at ~109 s** measures the scripted input's fixed frame
>   schedule (clicks and typing timed to guest frames), not boot time. In
>   max mode the hostblk boot reaches an idle Workbench desktop by about
>   frame 200, roughly 4 s of real time (screenshot-confirmed).
>
> Everything else below stands: the coverage numbers, the `run_batch`
> STOP-wake hang and its host-side fix, the `Wait 5` regression under
> `batch`, and the JIT trace-cache regression test.


Machine and load: Apple M3 Pro, 11 logical cores, arm64 macOS. Not
perfectly idle -- some measurements below ran back-to-back or briefly
overlapped a build, noted where it happened; treat single-run figures as
indicative, not noise-free (ADR 0006's own non-reproducibility point
applies doubly here).

**Baseline metrics (part 1).** `run_guest_max`'s end-of-run report now
splits wall time into slept (STOP-path parking) and busy
(instructions/busy-seconds), fixing 7.1's gap (its one MIPS figure
averaged in sleep time). `scripts/bench-boot-max.sh` prints this line and
a new wall-clock-to-`WBREADY` serial-marker measurement (Shell open three
double-clicks deep, `ECHO WBREADY >SER:`, timestamped like the `Wait 5`
test's `MARK1`/`MARK2` -- chosen because stock Kickstart otherwise
narrates nothing over serial to time against). `--label interp`, this
tree, one run each:

| | wall (4400f) | slept | busy | busy MIPS | idle busy MIPS (1000f) | WBREADY cycle | WBREADY max |
|---|---|---|---|---|---|---|---|
| cycle | 1.661s | -- | -- | 25.2 | -- | 6.073s | -- |
| max, interp | 87.859s | 53.812s | 34.047s | **2.67** | **2.20** | -- | 109.614s |

Idle host CPU (interp, `/usr/bin/time -l` over the 1000-frame idle
window): user 3.47s + sys 2.87s over 19.980s real, ~32% of one core.

**Window coverage (part 2).** `BUS_COVERAGE=1` on a cycle-mode run
(faster and deterministic for this purpose; the guest executes the same
code path either timing mode, so the access-pattern conclusion is
mode-independent) of a full boot plus the same three-double-click Shell
open plus `LIST SYS: ALL`, 5300 frames:

| | fast RAM | chip RAM | ROM | other |
|---|---|---|---|---|
| instruction fetches | 64.1% | 0.0% | **35.7%** | 0.2% |
| data accesses | 93.6% | 1.6% | 1.2% | 3.6% |

**This calls for a ROM shadow, per the plan's own gate** -- reported
here, not built: over a third of instruction fetches are ROM-resident
(Kickstart's own code -- Exec, dos.library, the Shell, disk.resource --
called constantly by anything running from fast RAM), so a single
`FastMem` window over fast RAM alone leaves that entire share on the slow
bus path every time. A ROM shadow copy in fast RAM (or a second `FastMem`
window over the ROM range) is the natural next step, not attempted in
this pass.

**`run_batch` + `FastMem` (part 3).** `--cpu-backend batch` initially
**hung**, not merely ran slower: a scripted boot-and-click run parked at
Kickstart's idle `STOP` by frame ~3650 of 6500 and never advanced again
(final PC frozen, busy MIPS collapsed to ~0.1, `WBREADY`/`MARK1`/`MARK2`
never seen). Root cause, found by reading the fork rather than guessing:
`run_batch_inner`'s stopped check (`execute.rs`) is `if self.stopped != 0
{ return Stopped }`, with no call to `check_and_service_interrupts`
first -- unlike `run_for_cycles`/`execute`, which call that check
*before* their own stopped branch, which is what lets a stopped CPU
notice a newly serviceable interrupt and clear `stopped`
(`stopped_supervisor_check`). `run_batch` has no equivalent, so once
stopped it can never wake itself no matter how many times the host calls
`cpu.set_irq()` and retries it.

Fixed entirely on the host side, no fork change: after every device-time
advance in the `Stopped` branch, when the batch back end is active,
probe with a minimal `cpu.run_for_cycles(bus, 4)` call. Costs nothing
when the interrupt still isn't serviceable and does real work -- letting
the core's own wake logic run -- exactly when a wake is due. After the
fix, the same run reaches both markers and "input-script: completed".

Numbers after the fix, `--label batch`, same machine, same script:

| | wall (4400f) | slept | busy | busy MIPS | idle busy MIPS (1000f) | WBREADY cycle | WBREADY max |
|---|---|---|---|---|---|---|---|
| max, batch (plain boot, no script) | 87.859s | 54.385s | 33.474s | 2.00 | 1.74 | 2.017s | 109.436s |

Idle host CPU (batch): user 2.19s + sys 3.59s over 19.973s real, ~29%.

Under the *scripted, interactive* boot-and-click-and-type workload
(`--input-script`, the same one `WBREADY`/`Wait 5` use), batch's busy
MIPS was **4.15** -- higher than any interp figure measured -- while the
plain, no-interaction boot above (mostly idle-`STOP`-waiting, dominated
by ROM-resident code per the coverage numbers) is *slower* than interp
(2.00 vs 2.67). Batch's win, where it exists, tracks how much of the
workload's hot code sits in fast RAM (Workbench/Shell/loaded-command
code) rather than ROM (Kickstart's own idle/boot loops) -- consistent
with the coverage measurement above.

**Interrupt-latency cost, measured, not just bounded.** The `Wait 5` gap
(`MARK1`→`MARK2`, same script as the real-ROM test) under batch: **5.739s**,
measured twice, both **outside** the real-ROM test's ±0.5s tolerance (interp:
5.088s, matching the existing test's own 5.093s within noise). The
adaptive chunk-size estimate (instructions per real µs, never reading
`cycles_remaining`, which `run_batch` clobbers) keeps chunks reasonably
short, but the wake-probe and `run_batch`'s coarser (instruction-, not
cycle-) budgeting together cost more interrupt latency than interp's
cycle-budgeted chunking does. **This is the answer to "does `run_batch`
need the fork to honour boundary requests to be viable": partially --
the STOP-wake gap was fixable on the host side, but the timer-latency
regression above was not chased further and would need either boundary-
request support in the fork or a shorter, cycle-aware chunk bound to
close.**

**JIT (part 4).** `machine-hosted` gained an off-by-default `jit`
feature (`m68k/jit` on this crate's dependency only); `cargo tree -e
features` against both board targets shows `m68k`'s only feature
reaching either is `default` -- confirmed inert for the boards. The
regression test ADR 0006 requires
(`crates/machine-hosted/tests/jit_trace_regression.rs`, `--features
jit`-only) loads a ten-iteration hand-assembled program into fast RAM,
runs it hot enough to JIT-compile (`TRACE_HOT_THRESHOLD` is 2
backward-branch hits in the pinned fork), overwrites the exact same
address range with a different program, and asserts the second run
executes the *new* code -- it does. This is the closest honest
substitute for a real-ROM two-executables-from-disk test (which would
need a booted Kickstart just to `LoadSeg` twice into the same address,
a much larger surface for the same narrow, address-level property); see
that file's own module doc comment for the full reasoning.

`--label batch+jit`, same script and machine:

| | wall (4400f) | slept | busy | busy MIPS | idle busy MIPS (1000f) | WBREADY cycle | WBREADY max |
|---|---|---|---|---|---|---|---|
| max, batch+jit (plain boot, no script) | 87.859s | 54.111s | 33.748s | 2.08 | 1.84 | 1.972s | 109.420s |

Idle host CPU (batch+jit): user 2.33s + sys 3.74s over 19.961s real,
~5.6%. `Wait 5` gap under batch+jit: 5.738s -- essentially unchanged
from plain batch (5.739s). JIT gives a small (~4%) busy-MIPS improvement
over plain batch on this plain-boot workload, still short of interp's
2.67 -- expected, given the coverage numbers: most of this particular
workload's hot code is ROM-resident (never JIT-compiled, since `FastMem`
and the trace cache only ever see fast RAM) or runs cold (boot-time
init code executed once, never hot enough to trace). JIT was not
re-measured against the interactive script (where plain batch already
showed its best result) given the time this pass had left; that
comparison is the natural next measurement.

**Recommendation (corrected).** Keep `interp` the default. Over busy
guest work, neither `run_batch` over a fast-RAM `FastMem` window nor the
fork's Cranelift JIT moves throughput by more than about 4% (table in the
correction above), and `batch` carries two costs `interp` does not: the
`Wait 5` regression (+0.65 s, outside the test's tolerance) and a
host-side STOP-wake probe standing in for something the fork doesn't do.
Coverage explains part of it: 35.7% of fetches are ROM-resident and never
reach the window or the trace cache. But even allowing for that, the
fork's fast paths are not a route to the "at least twice a 68060" floor
(roughly 150-200 M instructions/s; today about 30). That is evidence for
`docs/cpu-core-proposal.md`, whose direct mapping covers ROM, chip and
fast RAM alike, rather than for more tuning of the fork's back ends.

## 8. CPU benchmark: bare core versus in-machine

A direct answer to the question step 5's decision left open: of the gap
between bare m68k-rs (72-178 M instr/s interpreted per the fork's own
microbenchmark) and this machine's measured ~30 busy MIPS (step 7.2),
how much is m68k-rs itself versus device/bus overhead, and how far is
either from "at least twice a 68060"? `m68k/cpubench/kernels.s` is one
vasm source assembled two ways -- a flat binary
(`m68k/cpubench/kernels.bin`, `vasm -Fbin`) for the bare Rust harness
(`crates/cpu-bench`, straight over `m68k::CpuCore`/`LinearMemoryBus`,
no `machine-core` involved at all) and a relocatable object
(`vasm -Fhunk`) linked into `CPUBench`, a real AmigaOS CLI program
(`m68k/cpubench/cpubench.c`) that runs the identical instruction stream
inside a real Kickstart boot. Eleven kernels: the fork microbench's
ADDQ/BRA, TST/BNE and register-mix shapes; a 32-longword memory copy
and fill; a byte/word/long displacement struct walk; MOVEM save/
restore; a JSR/RTS call chain; a MULU/MULS/DIVU/DIVS mix;
BFEXTU/BFINS bitfield round-tripping; and a branchy eight-condition CMP
mix. Every kernel is a fixed-size unrolled body closed by SUBQ.L/BNE
(documented per-iteration instruction count, checked by both harnesses
against the CPU's own retired-instruction count before any timing run
-- all eleven calibrate exactly).

### A wrong turn, corrected

The first pass at `CPUBench` looked like it had found a machine-core
bug: `ReadEClock()` calls bracketing a single ~3600-instruction kernel
call measured about a real second of elapsed EClock ticks, and a few
kernels later the guest WEDGEd outright (an F-line trap storm). It
reproduced identically under `--cpu-speed cycle` and `--cpu-speed max`
and with `Forbid()`/`Permit()` removed entirely, which pointed away
from both the timing mode and this file's own bracketing -- and toward
the emulated CIA's EClock/TOD model. That diagnosis was wrong, and
supervisor review caught it before it went further: `kernels.s`'s
calling convention is `__asm("d0")`/`__asm("a0")`/`__asm("a1")`
register parameters, which are part of a function's real type under
this compiler, not a hint. `cpubench.c` stored each kernel's address in
a `KernelFn fn` field of a per-kernel dispatch table and called
`desc->fn(iters, a0, a1)` uniformly through that pointer -- and a call
made *through a function pointer* uses the pointer's own declared type
to decide how to pass arguments, discarding whatever `__asm` bindings
the pointed-to function actually has. That compiles cleanly, and the
first three kernels (which only read `d0`) even looked correct by
coincidence, but every kernel reading `a0`/`a1` received garbage
pointers. A second, real bug was fixed along the way and is worth
keeping separate: `kernels.s`'s bodies clobber `d2-d7`/`a2-a6`, which
this compiler's ABI treats as callee-saved, so each `_run_kernel_*`
C entry needed its own `movem.l`-save/BSR/`movem.l`-restore wrapper
around the bare kernel body (kept, under the bare `kernel_*` labels,
unchanged for the Rust harness). Fixing the register-preservation bug
alone was not sufficient -- `mem_copy`, the first kernel needing a
*second* register-passed pointer, still hung until the function-pointer
dispatch was replaced with `call_kernel()`, a plain `switch` that calls
each kernel by its real, `__asm`-annotated name. See `cpubench.c`'s own
header comment for the full account, kept there as the intended lesson
for the next person writing guest assembly called from C on this
toolchain.

A related, smaller issue surfaced once the dispatch was fixed:
core_main.c's own `Iterations/Sec` line (a `%f`-formatted expression
dividing an integer by a `time_in_secs()` result) reliably evaluates to
a clean IEEE754 negative zero on this toolchain, reproduced at both
`-O0` and `-O2` -- not this project's bug, and not chased into libgcc's
soft-float routines. `coremark_amiga.c` computes the reported CoreMark
rate from three plain integers (iterations, EClock ticks, EClock
frequency) instead, sidestepping the expression rather than fixing it;
`core_main.c` itself is untouched, so its own printed line still shows
`0.000000`.

### Bare harness (`crates/cpu-bench`, `cargo run -p cpu-bench --release`)

Apple M3 Pro, one build per column (`batch`/`batch+jit` need
`--features jit` to differ from `batch` -- the fork's Cranelift tracing
is a build-time choice, not a runtime switch, matching how ADR 0006's
own `interp`/`batch`/`batch+jit` numbers were taken). Two runs each for
`interp`/`batch`; `batch+jit` measured once (see note below the table).
M instr/s, best of two runs:

| kernel | interp (run 1 / run 2) | batch (run 1 / run 2) | batch+jit |
|---|---|---|---|
| reg_addq_bra | 102.4 / 102.2 | 161.3 / 160.0 | 1022.7 |
| reg_tst_bne | 63.1 / 62.3 | 196.1 / 194.1 | 2589.4 |
| reg_mix | 91.4 / 84.1 | 152.0 / 150.4 | 1275.0 |
| mem_copy | 48.9 / 48.7 | 112.5 / 112.2 | 833.5 |
| mem_fill | 46.2 / 49.1 | 163.1 / 168.8 | 1023.2 |
| struct_walk | 47.9 / 50.1 | 121.4 / 123.6 | 1248.8 |
| movem_saverestore | 35.4 / 37.6 | 91.3 / 89.1 | 1038.4 |
| jsr_rts_chain | 81.7 / 87.0 | 112.0 / 116.8 | 126.6 |
| muldiv_mix | 85.6 / 89.1 | 129.4 / 134.8 | 144.6 |
| bitfield_ops | 68.0 / 70.4 | 74.4 / 76.4 | 82.5 |
| cmp_branchy | 71.6 / 75.2 | 185.3 / 184.1 | 2025.8 |

`batch+jit`'s spread tracks exactly which kernels Cranelift traces:
`jsr_rts_chain`, `muldiv_mix` and `bitfield_ops` stay close to plain
`batch` (JSR/RTS call boundaries, MUL/DIV, and BFEXTU/BFINS trace-
admission gaps the fork's own microbench module docs already flag as
topologies the tracer rejects or only partially compiles), while the
pure register/branch/copy/fill/walk/MOVEM shapes compile to native code
and run at 800-2600 M instr/s -- in the same range the fork's own
`microbench.rs` reports for its comparable self-looping traces.

These bare numbers comfortably clear "at least twice a 68060" (roughly
150-200 M instr/s, `docs/cpu-core-proposal.md` §8) on `interp` alone
for several kernels, and on `batch`/`batch+jit` for nearly all of them.

### In-machine (`CPUBench`, real Kickstart 3.2.2 boot, `--cpu-speed max`, interp back end)

Same machine, load: idle otherwise during each run. Two full boot-and-
benchmark runs (each ~120 real seconds under max mode, matching the
`--max-frames 6000` budget at max mode's ~50 frames/real-second
pacing):

| kernel | run A (M instr/s) | run B (M instr/s) |
|---|---|---|
| reg_addq_bra | 61.7 | 62.2 |
| reg_tst_bne | 41.2 | 48.2 |
| reg_mix | 50.7 | 53.3 |
| mem_copy | 38.0 | 37.9 |
| mem_fill | 34.0 | 35.0 |
| struct_walk | 31.4 | 32.6 |
| movem_saverestore | 23.8 | 26.1 |
| jsr_rts_chain | 48.8 | 51.8 |
| muldiv_mix | 52.4 | 55.9 |
| bitfield_ops | 38.4 | 40.9 |
| cmp_branchy | 50.8 | 52.8 |

CoreMark (the realistic mixed workload, `m68k/cpubench/coremark/`,
Apache-2.0, provenance in that directory's `PROVENANCE.md`): validated
against the standard 2000-byte performance-run CRCs (`core_main.c`'s
own `known_id == 3` check -- `crclist 0xe714`/`crcmatrix 0x1fd7`/
`crcstate 0x8e3a`, all matching), 10 auto-calibrated iterations,
**127.1 / 137.7 iterations/sec** across the same two runs (computed as
described above, from raw ticks/frequency/iteration-count integers).
`core_main.c`'s own `CoreMark 1.0 : ...` line is present but shows
`0.000000` for the reason given above -- the CRC validation, not that
printed number, is this run's evidence of correctness.

`--cpu-backend batch` and `batch`+`jit` (`machine-hosted`'s own `jit`
Cargo feature) were not additionally measured for the in-machine half:
step 7.2 already measured all three back ends against a full real boot
and found them within about 4% of each other over busy work (ADR
0006's corrected numbers), so a second, kernel-level repeat of that
same comparison was not this pass's priority once the dispatch bug ate
most of the time budget available for it. `--cpu-backend batch` is a
plain CLI flag with no rebuild required and is the natural next
measurement for whoever picks this up.

### Ratio table: in-machine interp / bare interp

The machine's overhead on identical instructions, `--cpu-speed max`
against the bare harness's `interp` column (both back ends unbudgeted;
this isolates bus/device/hook overhead from the cycle-budget-vs-
wall-clock question ADR 0006 already answered). Both sides averaged
over their two runs:

| kernel | bare interp (avg M/s) | in-machine interp (avg M/s) | ratio |
|---|---|---|---|
| reg_addq_bra | 102.3 | 62.0 | 0.61 |
| reg_tst_bne | 62.7 | 44.7 | 0.71 |
| reg_mix | 87.8 | 52.0 | 0.59 |
| mem_copy | 48.8 | 37.9 | 0.78 |
| mem_fill | 47.7 | 34.5 | 0.72 |
| struct_walk | 49.0 | 32.0 | 0.65 |
| movem_saverestore | 36.5 | 25.0 | 0.68 |
| jsr_rts_chain | 84.4 | 50.3 | 0.60 |
| muldiv_mix | 87.4 | 54.2 | 0.62 |
| bitfield_ops | 69.2 | 39.7 | 0.57 |
| cmp_branchy | 73.4 | 51.8 | 0.71 |

### What the ratios say

In-machine throughput on identical instructions runs at roughly
57-78% of the bare interpreter's own throughput -- the machine's
bus/device/hook overhead costs on the order of a quarter to two-fifths
on top of m68k-rs's own per-instruction cost, not the order-of-
magnitude gap the ~30-busy-MIPS-vs-150-M-target framing (step 7.2)
might suggest. That framing compared a *mixed real workload* (a boot,
mostly ROM-resident code, much of it ordinary OS work) against a
*best-case bare microbenchmark*; this step's ratio compares the same
tight loops on both sides and finds machine-core's own overhead
reasonably contained. The larger gap to "twice a 68060" is therefore
mostly m68k-rs's own per-instruction interpretation cost, not
`machine-core`'s bus chain around it -- consistent with step 7.2's own
conclusion that `docs/cpu-core-proposal.md`'s direct-mapped decoder/IR
approach, not further tuning of the fork's back ends or this project's
own bus fast path, is the route to that floor. The ratio is not
uniform across kernels (0.57-0.78): `mem_copy`'s narrower gap (0.78)
suggests the fast-path bus work step 3 already did for RAM access
carries over well to a real boot's memory traffic; the register-only
kernels (`reg_mix`, `bitfield_ops`) show the widest gap, consistent
with per-instruction dispatch/hook overhead mattering more when there
is little memory-access cost to amortize it against.

### Copperline 68060 reference (optional deliverable)

Copperline (GPL, run but never copied) supports both a 68060 CPU model
and a specific clock speed (`--cpu 68060`, `--cpu-clock MHZ`, default
50 MHz for that model) -- confirmed via its own configuration docs, so
the capability this step asked about exists. `CPUBench` was retried
there (`copperline --model A1200 --cpu 68060 --fast 8M --run
m68k/cpubench/CPUBench --noaudio --serial stdout --benchmark-until
90`, against the bundled AROS): the program boots and stages, but no
`CPUBENCH` line appeared within 90 emulated seconds, and this was not
investigated further given the time this pass had left after the
dispatch-bug fix (AROS API compatibility for `timer.device`/`Forbid`/
`AllocMem`, or something else in that boot path, are all still open
questions). Per this deliverable's own instruction ("if it can't, say
so and stop there"), that is where this stopped -- no 68060 reference
ratio is recorded.

### Gates run for this step

- `cargo fmt --all -- --check`: clean.
- `cargo clippy -p machine-core --all-targets`, `-p machine-hosted
  --all-targets`, `-p cpu-bench --all-targets` (with and without
  `--features jit`): all clean, `-D warnings`.
- `cargo test -p machine-core`, `-p machine-hosted`: all pass.
- `cargo test -p cpu-bench`: passes (the calibration check; the timed
  `M instr/s` table is `cargo run -p cpu-bench --release`, not a test).
- The new real-ROM test
  (`kickstart_3_2_2_a1200_cpubench_reports_every_kernel_under_max_speed`,
  gated on `M68K_TEST_CPUBENCH_HDF`): passes.
- Full real-ROM suite (`--ignored --test-threads=1`, every fixture
  including `M68K_TEST_CPUBENCH_HDF`): all 19 tests pass.
- Both board crates (`board-qemu-virt --target aarch64-unknown-none`,
  `board-qemu-q35 --target x86_64-unknown-uefi`): clippy-clean,
  unaffected by this step (neither depends on `cpu-bench` or the
  `cpubench` guest sources).
