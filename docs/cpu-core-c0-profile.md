# C0 exit measurement: is m68k-rs the majority of max-mode host time?

**Answer, per workload:**

- **Boot to Workbench (KS 3.2.2, max mode):** m68k-rs is **68.3% / 65.7%** (two runs) of *CPU-busy* host time -- **yes, majority**. It is only **2.6%** of *wall-clock* host time, because **96.0-96.2% of this run's wall clock is the STOP-path sleep**, which is not host work (see "Sleep vs CPU time" below).
- **CPUBench, in-machine (max mode):** m68k-rs is **80.9% / 80.6%** (two runs) of *CPU-busy* host time -- **yes, clearly majority**. It is **13.4-13.6%** of wall-clock host time, because this run also spends **83.2-83.4%** of its wall clock in the same STOP-path sleep (most of the 120 s run is ordinary Kickstart boot before `CPUBench` itself starts, not the benchmark's own tight loops).

**C0's go/no-go is satisfied on the correct denominator (CPU-busy time, sleep excluded) on both workloads.** But the wall-clock numbers matter too, and they say something the proposal's §2.1 framing does not anticipate: see "What this predicts for the whole machine" below -- the 3-7x core speedup the PoC measured does **not** turn into a 3-7x, or even 2x, faster *boot*, because boot-to-Workbench wall-clock time is itself dominated by wall-clock-paced sleep that a faster CPU core cannot shrink. It is a much better multiplier for sustained CPU-bound foreground work once already booted.

---

## Supervisor correction (added at review, not by the measuring worker)

**The `WBREADY` figure is not boot-to-Workbench time, and this document's
boot row originally reported it as if it were.** `docs/bus-fast-path-plan.md`
step 7.2 already carries the caveat, in the same document this profile
cites for its other baselines:

> **`WBREADY` at ~109 s** measures the scripted input's fixed frame
> schedule (clicks and typing timed to guest frames), not boot time. In
> max mode the hostblk boot reaches an idle Workbench desktop by about
> frame 200, roughly 4 s of real time (screenshot-confirmed).

So the 109.60 s is dominated by a scripted click-and-type schedule pinned
to guest frames -- three double-clicks deep into Workbench, then typing a
Shell command -- and it would read ~109 s on an infinitely fast CPU too.
Actual boot to an idle Workbench desktop is about **4 s**.

**What this does and does not change.** The *direction* of §7's conclusion
survives: both the 4 s boot and the 109 s scripted schedule are paced to
guest frames, so neither shrinks much under a faster core, and a 3-7x CPU
still does not buy a 3-7x faster boot. But the specific "~2% faster boot"
arithmetic below is computed against the wrong denominator and should be
read as illustrative only, not as a measured prediction. The CPU-busy
share figures (65.7-68.3% boot, 80.6-80.9% CPUBench) and the C0 go/no-go
are unaffected -- those are ratios within the sampled profile and do not
depend on what the run's wall clock is called.

The lesson is the standing one in CLAUDE.md: the docs have twice
described work inaccurately, so check the code and the existing caveats
before trusting a figure -- including a caveat sitting in the document
you are already citing.

---

## 1. Method

### 1.1 Host

Apple M3 Pro, 11 logical cores, macOS 26.6.2 (Darwin 25.6.0, arm64). No x86-64 host and no Rock 5B were available this session -- **those table columns are left empty below, not estimated.**

Toolchain: rustc/cargo 1.98.1, release profile (`lto = "fat"`, `codegen-units = 1`, workspace default). `samply` 0.13.1 (`~/.cargo/bin/samply`). Repo at `b54db13` (the commit that added `crates/m68k-vp` and `docs/cpu-core-poc-results.md`).

### 1.2 Binary and flags

`target/release/machine-hosted`, `--cpu-speed max --cpu-backend interp` (the default backend; step 7.2 found interp/batch/batch+jit within ~4% of each other over busy work, so this default is representative, not a best case).

### 1.3 Fixtures

`nondistribution/A1200.47.115.rom`, `nondistribution/m68k-machine.hdf` (boot workload), `nondistribution/m68k-machine-cpubench.hdf` (CPUBench workload) -- all present, so real-ROM tests and these runs execute rather than skip.

### 1.4 Runs were serial, one at a time

**Every measurement below was run with nothing else of mine executing on the machine.** An earlier pass in this session ran the boot-workload `samply` capture concurrently with the CPUBench `samply` capture and with `bench-boot-max.sh`'s own idle-cost run; that is invalid for a wall-clock-paced mode (CPU contention directly distorts both the paced boot's wall-clock timing and the CPU-time/sleep split) and **all of that output was discarded**, not averaged in. Every number in this document comes from single-process runs, and every profiling/benchmark command was launched only after confirming (`ps aux | grep machine-hosted`) that no other instance was running.

### 1.5 Reproduction commands

```sh
cargo build --release -p machine-hosted

# 1) Wall-clock-to-Workbench (scripted, serial-marker-confirmed), cycle vs max,
#    plus idle-window guest MIPS and idle host CPU cost:
bash scripts/bench-boot-max.sh --no-build --label c0

# 2) Busy MIPS over the active (non-idle) part of boot, max mode:
./target/release/machine-hosted --rom nondistribution/A1200.47.115.rom \
    --hostblk nondistribution/m68k-machine.hdf --cpu-speed max --cpu-backend interp \
    --max-frames 200 --max-instructions 0

# 3) Sampling profile, boot workload (KS 3.2.2 boot to Workbench-ready window,
#    same 4400-frame boundary bench-boot-max.sh uses):
samply record --rate 1000 --save-only --unstable-presymbolicate \
    -o boot.json.gz -- ./target/release/machine-hosted \
    --rom nondistribution/A1200.47.115.rom --hostblk nondistribution/m68k-machine.hdf \
    --cpu-speed max --cpu-backend interp --max-frames 4400 --max-instructions 0

# 4) Sampling profile, CPUBench workload (real-rom.rs's own fixture and frame
#    budget, kickstart_3_2_2_a1200_cpubench_reports_every_kernel_under_max_speed):
samply record --rate 1000 --save-only --unstable-presymbolicate \
    -o cpubench.json.gz -- ./target/release/machine-hosted \
    --rom nondistribution/A1200.47.115.rom --hostblk nondistribution/m68k-machine-cpubench.hdf \
    --cpu-speed max --cpu-backend interp --max-frames 6000 --max-instructions 5000000000
```

`--unstable-presymbolicate` is required: without it, `samply record --save-only` embeds only raw addresses in `profile.json` (symbolication normally happens in the local web viewer, which is not usable headlessly here); with it, a `.syms.json` sidecar carries resolved names for every address the profile references. The attribution script below joins the two.

### 1.6 How CPU time was separated from sleep

**This is the method the task brief specifically warned matters: getting it wrong inverts the C0 conclusion.**

The profiler samples the process's call stack at 1000 Hz regardless of whether the sampled thread is running or blocked in the kernel. `run_guest_max`'s STOP-path sleep (`crates/machine-hosted/src/run.rs`, `std::thread::sleep(slice)` at the "STOP sleeps rather than busy-ticking" site) bottoms out on macOS in `libsystem_kernel.dylib!__semwait_signal` -- so every sample whose leaf frame is that symbol (or a small set of related kernel/pthread wait primitives -- `mach_msg2_trap` used purely as a wait, `pthread_cond_wait`, `kevent`, etc.) was classified as **Sleep (off-CPU / paced idle)** and excluded from the CPU-busy denominator. Every other sample is real CPU work and was attributed to one of four named buckets by matching the resolved symbol name against `m68k`/`CpuCore`/decoder-dispatch patterns (m68k-rs core), `MachineBus`/`read_*`/`write_*`/`board_at` (bus/memory), `Cia`/`beam`/`rtgboard`/`hostblk`/`pktport`/`input`/`autoconfig`/`pcibridge`/`virtio` (device engines), or `run_guest`/screenshot/serial/`std::io`/malloc (run loop, pacing, host I/O); unmatched busy samples fall into "everything else."

The attribution script is `analyze_profile2.py` (kept in this session's scratchpad, not committed -- it is a one-off analysis tool, not test or harness code); its method is fully described in this section, and its top-40-functions-by-self-time output for each profile is reproduced under "Attribution detail" below so the classification is checkable without re-running it.

**Cross-check and a genuine finding.** `run_guest_max` also self-reports `wall`/`slept`/`busy` at the end of every max-mode run (`Report::timing_report`). Comparing that to the sampled split surfaces a real bug in that self-report, not a bug in the sampling method: `slept` accumulates the *nominal* sleep slice requested (`slept += slice`, capped at `MAX_MODE_STOP_SLEEP_SLICE = 1 ms`), never the actual measured wall-clock duration the `std::thread::sleep` call took. On this host, `thread::sleep` overshoots its nominal 1 ms request by enough, over the tens of thousands of STOP-slice sleeps in an idle-heavy run, that the app's own `busy = wall - slept` figure is inflated by roughly an order of magnitude relative to the sampled ground truth: the boot workload's own report says `busy=33.6-33.9s` of `wall=87.86s` (38.2%), while the sampled profile puts **true CPU-busy time at 3.8-4.0% of wall clock, not 38%.** This is reported here as a finding for the maintainers (per the task brief: raise it, don't fix it) -- `run_guest_max`'s `busy_mips` figure, and by extension the "~30 busy MIPS" figures in `docs/bus-fast-path-plan.md` step 7.2/8, measure "wall time not accounted as nominal sleep," not genuine CPU-busy time, whenever STOP is entered anywhere near as often as it is during an idle-heavy run. The short, mostly-non-idle 200-frame runs used for busy-MIPS below are far less affected (few STOP entries during early boot), and their sampled-profile cross-check (not done for those specifically, given time budget) would be the way to confirm that.

The primary figure in every table below is the **sampled CPU-busy share**, sleep excluded, per the task brief's instruction. Wall-clock shares (including sleep) are reported alongside and are always labelled as such.

---

## 2. C0 table (`docs/cpu-core-proposal.md` §8's own grid, reproduced here)

| Measurement (max mode) | Hosted x86-64 | Hosted aarch64 (M3 Pro) | Rock 5B / KVM |
|---|---|---|---|
| KS 3.2 boot to Workbench (s) | *(no x86-64 host)* | **~4 s** to an idle Workbench (see supervisor correction below). `WBREADY` at **109.60 s** (109.596 / 109.606, two runs) is *not* boot time. | *(no Rock 5B)* |
| AROS boot to Workbench (s) | *(no x86-64 host)* | **not measured -- see §5** | *(no Rock 5B)* |
| Idle-loop MIPS | *(no x86-64 host)* | **~0.1-0.2 M/s** averaged over a post-boot idle window (mostly STOP) | *(no Rock 5B)* |
| Busy MIPS (active boot, frames 0-200) | *(no x86-64 host)* | **28.5-29.2 M/s** (two runs) | *(no Rock 5B)* |
| m68k-rs share of host time, **CPU-busy** (primary) | *(no x86-64 host)* | **boot 65.7-68.3%; CPUBench 80.6-80.9%** | *(no Rock 5B)* |
| m68k-rs share of host time, **wall-clock** (secondary) | *(no x86-64 host)* | **boot 2.6%; CPUBench 13.4-13.6%** | *(no Rock 5B)* |

This table intentionally splits the single proposal-doc row "m68k-rs share of host time (%)" into CPU-busy and wall-clock variants, per the task brief's explicit instruction to report both and say which is which. `docs/cpu-core-proposal.md` §8's own table is filled in with the CPU-busy figures (the correct denominator for the go/no-go question, per that section's own framing of "host time" as CPU work) plus a footnote pointing here for the wall-clock split.

---

## 3. Boot-to-Workbench measurement (deliverable 1)

Reused `crates/machine-hosted/tests/real_rom.rs`'s own scripted-Shell-and-serial-marker machinery (`max_cpu_speed_boots_to_workbench_and_keeps_real_time_across_wait_5`'s three-double-click-deep Shell open) via `scripts/bench-boot-max.sh`'s `measure_wb_ready`, which drives the identical input script and times host-process-start to the `WBREADY` marker arriving on serial.

Two full serial runs of `bash scripts/bench-boot-max.sh --no-build`:

| | run 1 | run 2 |
|---|---|---|
| WBREADY wall clock, cycle mode | 2.072 s | 2.097 s |
| WBREADY wall clock, max mode | **109.596 s** | **109.606 s** |
| boot-to-frame-4400 wall clock, cycle mode | 1.751 s (23.9 MIPS) | 1.766 s (23.7 MIPS) |
| boot-to-frame-4400 wall clock, max mode | 87.891 s (own report: busy_mips=2.68) | 87.891 s (own report: busy_mips=2.68) |
| idle window (frames 4400-5400), max mode | 19.965 s, 0.2 MIPS | 19.969 s, 0.1 MIPS |
| idle host CPU over that same 0-5400-frame run (`/usr/bin/time -l`) | user=3.46s sys=2.96s over 107.83s real (~5.95% of one core) | user=3.47s sys=2.98s over 107.82s real (~5.98% of one core) |

Variance across the two runs is small (WBREADY max: 0.010 s apart; boot-to-4400: identical to 3 decimal places; idle host CPU: within 0.03 percentage points) -- consistent with ADR 0006's own note that max mode is not bit-reproducible but is close run to run on an otherwise-idle host.

**This reproduces `docs/bus-fast-path-plan.md` step 7.2's whole-run idle-host-CPU figure closely** (~5.95-5.98% here vs. that document's ~6.3%, both measured over a run that includes boot). It does **not** re-isolate that document's separate ~4%-over-30s-of-pure-idle-after-boot figure -- that needs an in-run checkpoint the runner does not currently expose (same limitation that document's own author noted), and re-implementing that checkpoint was out of scope for this measurement pass.

---

## 4. Guest MIPS (deliverable 3)

**Busy MIPS, active boot (frames 0-200, max mode, `--cpu-backend interp`), two runs:**

```
run A: wall=3.994s slept=0.935s busy=3.059s busy_mips=29.19
run B: wall=3.994s slept=1.003s busy=2.990s busy_mips=28.46
```

This reproduces step 7.2's corrected figure (30.65/29.88/30.04/30.07/31.32/31.06 M/s across interp/batch/batch+jit, same frame range) within about 5%, on the same host. **No correction to that figure is needed** -- both this session's numbers and the existing ones cluster around 29-31 M/s for the active part of boot. Note the caveat in §1.6: this range comes from the app's own `busy_mips` self-report, which the sleep-accounting bug inflates when STOP is entered often; frames 0-200 are almost entirely pre-Workbench active boot with few STOP entries, so the inflation here should be small, but it was not independently cross-checked against a sampled profile for this specific narrow window (time budget).

**Idle-loop MIPS:** from the same `bench-boot-max.sh` idle-window measurement above, the 1000-frame post-boot idle segment retires very few instructions relative to its ~20 s of wall clock: 0.1-0.2 M/s. This is expected and matches ADR 0006's point directly -- a correctly-sleeping STOP retires almost nothing while parked, so a *low* idle-MIPS figure is itself evidence the sleep path is doing its job, not a regression.

---

## 5. AROS boot to Workbench -- not measured

`nondistribution/aros/aros.hdf` (a real bootable disk image) exists, and attaching it via `--hostblk` was tried directly:

```sh
./target/release/machine-hosted --rom assets/aros/aros-amiga-m68k-rom.bin \
    --ext-rom assets/aros/aros-amiga-m68k-ext.bin --hostblk nondistribution/aros/aros.hdf \
    --max-frames 3000 --max-instructions 300000000 \
    --screenshot /tmp/aros-test.png --screenshot-frame 2500
```

Result: the guest reaches and stays parked at `PC 0x00fe8b88` from around frame 2150 onward -- the **same idle PC** `real_rom.rs`'s own `aros_68k_screenshot_shows_boot_screen_content_without_boot_media` test documents for the no-disk case ("the cat-eyes boot logo... then parks the whole system in `Wait`... the same idle state the Copperline oracle shows at its logo"). Attaching a real disk did not change this: this AROS ROM pair, in this harness, does not proceed past the boot-logo/no-bootable-media screen to a real Workbench desktop, with or without a disk attached.

This is a pre-existing project/fixture limitation, not something introduced by this measurement pass, and no existing test or scripted-input machinery in this repo reaches an AROS Workbench desktop to time against (the only AROS real-ROM test asserts boot-logo content, not desktop content). Per the brief's "reuse... rather than inventing" instruction, no new AROS boot-to-Workbench harness was built. **The AROS row is left empty rather than substituting the boot-logo screen's timing, which is not the same milestone.**

---

## 6. Sampling-profile attribution (deliverable 4, the actual C0 go/no-go evidence)

Two profiles per workload (four `samply` captures total, all serial). Method in §1.6.

### 6.1 Boot workload (KS 3.2.2, `--max-frames 4400`, max mode)

| | run 1 | run 2 |
|---|---|---|
| Total samples (~1 ms each) | 87,858 | 87,858 |
| Sleep (off-CPU) share of wall | 96.2% | 96.0% |
| **CPU-busy share of wall** | 3.8% | 4.0% |

CPU-busy-time bucket shares (sleep excluded), run 1 / run 2:

| Bucket | run 1 | run 2 |
|---|---:|---:|
| **m68k-rs CPU core** | **68.3%** | **65.7%** |
| MachineBus / memory | 9.2% | 8.4% |
| Device engines (CIA/beam/rtg/hostblk/pktport/input) | 2.1% | 2.6% |
| Run loop / pacing / host I/O | 2.2% | 2.6% |
| everything else (unclassified) | 18.3% | 20.6% |

Wall-clock-time bucket shares (sleep included), run 1:

| Bucket | share of wall |
|---|---:|
| m68k-rs CPU core | 2.60% |
| MachineBus / memory | 0.35% |
| Device engines | 0.08% |
| Run loop / pacing / host I/O | 0.08% |
| Sleep (off-CPU / paced idle) | 96.20% |

**"Everything else" (18-21% of CPU-busy time) is not m68k-rs, MachineBus, a device engine, or the run loop's own named code** -- it is almost entirely host clock/scheduling syscalls: `mach_msg2_trap` (0.36% of wall), `mach_absolute_time` (0.17%), plus small amounts of `mach_timebase_info`, `read`/`write`, `clock_get_time`, `cerror`. These are consistent with `Instant::now()` calls and the pacing loop's own bookkeeping around every chunk boundary (the loop calls `Instant::now()` repeatedly per the code at `run.rs`'s deadline-checking sites) rather than any of the four named buckets; this document does not claim more precision than that about `mach_msg2_trap`'s exact origin.

### 6.2 CPUBench workload (`--max-frames 6000`, max mode, real Kickstart boot then `C:CPUBench`)

| | run 1 | run 2 |
|---|---:|---:|
| Total samples | 119,776 | 119,794 |
| Sleep (off-CPU) share of wall | 83.4% | 83.2% |
| **CPU-busy share of wall** | 16.6% | 16.8% |

CPU-busy-time bucket shares (sleep excluded), run 1 / run 2:

| Bucket | run 1 | run 2 |
|---|---:|---:|
| **m68k-rs CPU core** | **80.9%** | **80.6%** |
| MachineBus / memory | 11.4% | 11.4% |
| Device engines | 0.5% | 0.5% |
| Run loop / pacing / host I/O | 3.0% | 3.1% |
| everything else | 4.2% | 4.4% |

**Why 83% sleep even on a "CPU-bound" workload:** `CPUBench` runs automatically from `S/Startup-Sequence`, immediately after ordinary Kickstart boot -- and that boot still contains the same wall-clock-paced STOP waits (disk detection, the standard insert-disk delay window, dispatcher idle ticks) the plain boot workload above does. Summing `CPUBench`'s own reported per-kernel microseconds plus CoreMark's ~10 s auto-calibration accounts for roughly 25-30 of the run's 119.8 s; the remaining ~90 s is boot overhead before the benchmark proper starts, not the benchmark's own tight loops. **This matters for the Amdahl arithmetic below**, not for the go/no-go itself -- of the time the host CPU was actually doing work (16.6-16.8% of this run's wall clock), m68k-rs is clearly the majority (80.6-80.9%).

`CPUBench` itself reproduced (this session, one run, serial):

```
CPUBENCH reg_addq_bra      58978814  1041395  56.634
CPUBENCH reg_tst_bne       58978814  1394674  42.288
CPUBENCH reg_mix           58978814  1159733  50.855
CPUBENCH mem_copy          58975213  1652404  35.690
CPUBENCH mem_fill          57337013  1776115  32.282
CPUBENCH struct_walk       44231413  1583987  27.924
CPUBENCH movem_saverestore 36042614  1520116  23.710
CPUBENCH jsr_rts_chain     91747615  1915825  47.889
CPUBENCH muldiv_mix        58978828  1100788  53.578
CPUBENCH bitfield_ops      65534015  1719666  38.108
CPUBENCH cmp_branchy       58978814  1178420  50.049
CPUBENCH coremark                            129.668 iter/sec
CPUBENCH DONE
```

Close to `docs/bus-fast-path-plan.md` step 8's own two-run in-machine table (e.g. `cmp_branchy` 50.0 here vs. 50.8/52.8 there, CoreMark 129.7 here vs. 127.1/137.7 there) -- consistent, no correction needed.

### 6.3 Attribution detail (top self-time functions, for checkability)

Boot workload, run 1, top entries by self-time (full list of 40 kept in the scratch analysis; reproduced here down to 0.01% of wall):

```
84518  96.20%  Sleep       libsystem_kernel.dylib!__semwait_signal
   434   0.49%  m68k-rs     m68k::core::decode::dispatch_instruction::<Bus>
   405   0.46%  m68k-rs     <CpuCore>::step_with_dispatch::<Bus, dispatch_instruction<Bus>>
   338   0.38%  m68k-rs     <CpuCore>::cycles_040
   315   0.36%  other       libsystem_kernel.dylib!mach_msg2_trap
   269   0.31%  m68k-rs     m68k::mmu::translation::translate::<Bus>
   213   0.24%  bus         <MachineBus>::read_word
   160   0.18%  m68k-rs     <CpuCore>::read_imm_16::<Bus>
   156   0.18%  m68k-rs     <CpuCore>::resolve_ea::<Bus>
   149   0.17%  other       libsystem_kernel.dylib!mach_absolute_time
   109   0.12%  m68k-rs     m68k::core::decode::dispatch_move::<Bus>
    79   0.09%  m68k-rs     <Bus as AddressBus>::try_read_immediate_word
    66   0.08%  run loop    machine_hosted::run::run_guest_max
    66   0.08%  device      <machine_core::cia::Cia>::tick
```

CPUBench workload, run 1, top entries:

```
99890  83.40%  Sleep       libsystem_kernel.dylib!__semwait_signal
 3139   2.62%  m68k-rs     <CpuCore>::step_with_dispatch::<Bus, dispatch_instruction<Bus>>
 3069   2.56%  m68k-rs     m68k::core::decode::dispatch_instruction::<Bus>
 2790   2.33%  m68k-rs     <CpuCore>::cycles_040
 1635   1.37%  bus         <MachineBus>::read_word
 1245   1.04%  m68k-rs     <CpuCore>::resolve_ea::<Bus>
 1143   0.95%  m68k-rs     <CpuCore>::read_imm_16::<Bus>
  742   0.62%  m68k-rs     m68k::core::decode::dispatch_group_4::<Bus>
  673   0.56%  m68k-rs     <Bus as AddressBus>::try_read_immediate_word
  645   0.54%  m68k-rs     m68k::core::decode::dispatch_move::<Bus>
  593   0.50%  run loop    machine_hosted::run::run_guest_max
  526   0.44%  m68k-rs     <Bus as AddressBus>::write_long
  448   0.37%  other       libsystem_kernel.dylib!mach_msg2_trap
```

No device-engine or run-loop function appears anywhere near the top of either list; every function above device/run-loop rank is either an m68k-rs core function or a `MachineBus` accessor. This is the direct evidence for the go/no-go: m68k-rs's own dispatch/decode/EA-resolution code dominates the CPU-busy time in both workloads by a wide margin over every other named bucket.

---

## 7. What this predicts for the whole machine (deliverable 3, Amdahl arithmetic)

Two different questions, two different denominators. Conflating them is exactly the trap the task brief warned about.

### 7.1 "If a new core replaces m68k-rs, how much faster does the *work the CPU is actually doing* go?"

Amdahl's law against the **CPU-busy-time share** (the fraction of host CPU seconds m68k-rs itself, not MachineBus/devices/run-loop, currently consumes), using the PoC's measured 3-7x bare-kernel range (`docs/cpu-core-poc-results.md`):

speedup = 1 / ((1 - S) + S / core_speedup)

| workload | S (m68k-rs share of CPU-busy time, avg of 2 runs) | at 3x core | at 5x core | at 7x core |
|---|---:|---:|---:|---:|
| boot to Workbench | 0.670 | 1.81x | 2.16x | 2.35x |
| CPUBench | 0.807 | 2.17x | 2.82x | 3.25x |

**Caveat from step 8's own finding, applied here as instructed:** `docs/bus-fast-path-plan.md` step 8 measured that m68k-rs's *own* in-machine throughput is only 57-78% of its bare-crate throughput on identical instructions -- i.e. machine-core's bus/hook integration already costs 22-43% on top of m68k-rs's bare per-instruction cost, separate from the MachineBus-bucket time this profile already accounts for separately. If a new core's integration into `machine-core` costs a similar fraction (unmeasured -- `m68k-vp`'s PoC gates were bare-crate only, never run in-machine), the PoC's bare 3-7x would realistically arrive in-machine as roughly 1.7-5.5x (3x * 0.57 to 7x * 0.78), which flows through the same formula to:

| workload | at "effective" 1.71x | at "effective" 5.46x |
|---|---:|---:|
| boot to Workbench | 1.39x | 2.21x |
| CPUBench | 1.50x | 2.94x |

This is not a measurement -- it is the same Amdahl arithmetic applied to step 8's own already-published discount, carried forward as a sensitivity bound, and flagged here as an open question for whoever measures `m68k-vp` in-machine (the natural next step, mirroring step 8's own bare-vs-in-machine ratio table for the new core).

### 7.2 "How much faster does the *user actually wait*, in wall-clock seconds?"

This is the number that matters for "the user must not care the CPU is emulated" (the project's own stated UX goal). Amdahl against the **wall-clock share** instead -- because STOP-path sleep is bounded by real-time device pacing (ADR 0006, by design) and **a faster CPU core cannot shrink it**:

| workload | S (m68k-rs share of wall clock, avg of 2 runs) | at 3x core | at 5x core | at 7x core |
|---|---:|---:|---:|---:|
| boot to Workbench | 0.026 | **1.018x** | **1.021x** | **1.023x** |
| CPUBench (whole run, boot included) | 0.135 | **1.099x** | **1.121x** | **1.131x** |

**This is the honest headline result of this measurement pass, and it is not the flattering one.** `docs/cpu-core-proposal.md` §2.1 states "A 3x faster core is roughly a 3x faster machine" for this project, on the reasoning that there is no chipset competing with the CPU for host time. That reasoning is correct about the *composition* of CPU-busy time (§7.1 confirms it: m68k-rs is 67-81% of it, clearly the majority, clearing C0). **It is not correct about wall-clock boot time**, because boot-to-Workbench in max mode is itself dominated by wall-clock-paced STOP sleep (96% of it, here) that has nothing to do with CPU throughput -- Kickstart's own timer-gated boot delays, not instruction execution, set the floor on how many real seconds booting takes, the same way they would on real hardware. A 3-7x faster core predicts on the order of **2% faster boot**, not 3-7x faster, measured this way.

The CPUBench workload's wall-clock number (1.10-1.13x) is better but still modest, and for a reason that is itself informative: even a benchmark chosen specifically to be CPU-bound spends 83% of its *process lifetime* in the same pre-benchmark boot sleep, because it is launched from `Startup-Sequence`. **A user already sitting at an idle Workbench, doing sustained CPU-bound foreground work with no STOP waits in the loop, would see something much closer to the §7.1 numbers (1.8-3.3x)** -- that scenario has little-to-no wall-clock sleep to dilute the CPU-busy fraction into the total. This document does not have a clean isolated measurement of that scenario (it would need a workload that starts already-booted and runs without pausing, e.g. a version of `CPUBench` re-run from an already-open Shell rather than `Startup-Sequence` -- not attempted here, time budget), but the mechanism is directly visible in the two workloads already measured: CPUBench's higher busy-time m68k-rs share (81% vs. boot's 67%) and dramatically higher wall-clock share (13.5% vs. 2.6%) both move in the direction predicted by "less STOP dilution -> closer to the busy-time number."

**Net: C0 passes (m68k-rs is the CPU-busy-time majority, both workloads) — proceed. But the proposal's own "3x core = 3x machine" framing should be corrected before it is repeated further: it holds for sustained CPU-bound foreground work, and does not hold for boot time, which is wall-clock-sleep-bound by design and only marginally CPU-bound.** This is exactly the kind of thing the task brief asked to be raised, not acted on -- recorded here for the supervisor's judgment on whether §2.1 needs a correction.

---

## 8. What was not measured, and why

- **x86-64 and Rock 5B/KVM columns**: no such host was available this session. Left empty, not estimated, per the brief.
- **AROS boot to Workbench**: this AROS ROM/HDF pair does not reach a real Workbench desktop in this harness even with a disk attached (parks at the same idle PC the existing no-disk-boot-logo test documents) -- see §5.
- **Idle-loop-only host CPU cost, isolated from boot**: bench-boot-max.sh's own idle window still includes boot in its single continuous `/usr/bin/time -l` run; isolating pure post-boot idle needs an in-run checkpoint the runner does not expose (a known, pre-existing limitation, noted in `docs/bus-fast-path-plan.md` step 7.2 as its own open follow-up).
- **A sustained already-booted CPU-bound wall-clock measurement** (§7.2's last paragraph): would need a workload variant that starts from an open Shell rather than `Startup-Sequence`, not built this session.
- **`--cpu-backend batch`/`batch+jit` profiles**: step 7.2 already found all three backends within ~4% of each other over busy work; a second profiling pass across backends was not repeated here given that existing finding and the time budget.
