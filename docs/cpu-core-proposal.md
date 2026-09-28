# m68k-machine CPU Core Proposal

**Working title:** `m68k-vp` (virtual processor) — name deferred
**Status:** Draft, September 2026. Not accepted: the ADR changes in §12 are what acceptance would require, and none of them has been made.
**Relates to:** m68k-machine-proposal.md, combined-roadmap.md, bus-fast-path-plan.md, ADR 0001, ADR 0006
**Evidence:** `docs/cpu-core-c0-profile.md` (C0's profile and go/no-go), `docs/cpu-core-poc-results.md` (a C2-shaped prototype of §4.1–§4.4, measured against m68k-rs). Corrections made to this document on 2026-09-28 from those two are marked inline with that date. The status line above is unchanged: prototype evidence is not acceptance.

## 1. Summary

Replace the general-purpose `m68k` crate (m68k-rs) as the mainline CPU core of m68k-machine with a purpose-built, no_std, two-tier core:

1. A **decoder** that turns 68040-class instructions into a small, m68k-shaped intermediate form (the IR).
2. An **IR interpreter** for cold code and a **template JIT** that lowers hot IR blocks to native code, with one emitter per host architecture (aarch64 first, x86-64 second, RISC-V as the proof that a third target is cheap).

m68k-rs is retained as a reference oracle for lockstep differential testing on the Linux backend only. It is never required to be no_std and never ships in a bare-metal image.

The intent is a core that is (a) materially faster than any interpreter, (b) native on every target the machine runs on, including bare-metal RK3588 and UEFI x86, and (c) portable to a new host ISA by writing one emitter.

## 2. Motivation

### 2.1 Speed is the whole budget here

In a chipset emulator the CPU shares the host's time with Denise, Agnus, Paula and the Copper. In this machine there is no chipset: RTG display, no bitplane DMA, idle Zorro cards. CPU emulation *is* the workload, so core speed maps almost directly onto perceived speed. A 3× faster core is roughly a 3× faster machine — not the 1.3× it would be in Copperline.

*Measured, and qualified (2026-09-28, `docs/cpu-core-c0-profile.md`).* C0's profile confirms the premise on the denominator that matters: m68k-rs is **65.7–68.3%** of CPU-busy host time booting and **80.6–80.9%** running a CPU-bound in-guest workload. The claim above is therefore sound **for sustained CPU-bound work**, and the C0 exit below is satisfied.

It does **not** hold for boot time, and the sentence above should not be read as promising that it does. Boot-to-Workbench under ADR 0006's max mode is dominated by wall-clock-paced sleep — 96% of that run's wall clock — because the machine is waiting on real time and on guest frames, not on instruction throughput. A 3–7× core (`docs/cpu-core-poc-results.md`) buys close to nothing there. The user-visible win from this proposal is in sustained foreground work; boot latency is a separate problem whose fix is removing real-time waits at source, not a faster CPU.

*Where the bus stands.* The byte-composed `MachineBus` path that was the likelier bottleneck at the start of this proposal has been replaced: the width-native fast path, LTO, per-line device engines, the lazy CIA/beam tick and ADR 0006's wall-clock-paced max mode are on `main`, with before-and-after numbers in `docs/bus-fast-path-plan.md`. Whether the CPU core is now the majority of host time is the C0 exit question (§8), answered by a max-mode profile, not assumed here.

### 2.2 Differentiation

Built on m68k-rs, the machine reads to an outsider as "Copperline with the chipset removed". That is not what it is, but the shared CPU core makes the assumption natural. A native, JIT-based core that runs bare-metal on ARM and x86 is a different kind of project — closer to Emu68 and Amithlon than to an emulator — and it says so in the architecture, not just in the README.

### 2.3 Bare-metal without a custom crate

The machine already runs on bare metal, and it does so on a personal `no_std` fork of m68k-rs, pinned by `rev` and not upstreamed (`docs/phase0-findings.md`). That fork is a standing liability: every upstream change has to be re-merged by hand, the crate's own JIT stays `std`-only regardless, and the pin means bare-metal CPU semantics can only move when someone deliberately rebases the fork. It was the right Phase 0 decision and it is the wrong thing to build the next five years on.

The techniques that make the proposed core fast (direct address-space mapping, host condition flags, pinned registers, template emission) are the same techniques that make it hardware-native. None of them need an allocator, `std`, or a hosted interpreter design. With this core the fork is retired: m68k-rs returns to being an ordinary, unpinned, hosted-only dev-dependency used as a test oracle, and the bare-metal port becomes "swap the board layer", which is where the porting cost was always meant to sit.

### 2.4 Portability to new hosts

The IR is the layer that makes a third host cheap. Amiga DE (Tao Elate/intent) used a virtual-processor form translated at load time so that one binary ran on every host ISA; Transmeta used an internal form with multi-tier translation. This proposal borrows the shape, not the goal: the IR is a runtime decode format, never a distribution format, and the port cost of a new ISA is one emitter plus the board layer.

## 3. Goals and non-goals

**Goals**

- 68040-class integer core (68020+ addressing modes, supervisor/user, full exception and interrupt model as AmigaOS and AROS use it).
- `#![no_std]` core with a bounded static memory footprint from the first commit.
- Runs on: Linux/KVM (bring-up and desktop board layer), bare-metal RK3588, UEFI x86-64. RISC-V as a later validation of portability.
- Passes the same acceptance gates as the rest of the machine: Kickstart 3.2 and AROS 68k boot to Workbench unattended, on every backend.
- Lockstep parity with m68k-rs over the full boot and a defined workload set before it becomes the mainline core.

**Non-goals**

- 68000/010/020/030 quirk emulation (prefetch, address errors, model-specific timing).
- Cycle accuracy of any kind. Timers derive from the host clock — this is ADR 0006's max mode, and the core slots into that timing model unchanged. This proposal redefines the reproducible mode for tests and gates without cycle tables; see §5.2.
- MMU. Neither OS needs it; debugging tools that do are out of scope.
- A general-purpose optimising compiler above the IR. No SSA, no register allocator, no scheduling.
- Games, chipset code, anything Copperline is for.

## 4. Architecture

```
  68k code ──► Decoder ──► IR block ──┬──► IR interpreter   (cold code, reference tier)
                                      └──► Template JIT ──► native block (hot code)
                                                │
                              emitters: aarch64 │ x86-64 │ riscv64 (later)
```

### 4.1 Decoder

Fetches from the directly-mapped address space, decodes one basic block (up to the next control-transfer or a fixed op limit) into IR. Handles addressing-mode expansion here so the IR never sees an effective-address calculation as a single opaque thing.

### 4.2 IR

A flat list of fixed-width ops over the pinned m68k register file plus a small pool of temporaries (target: 8). Roughly TCG-shaped, but deliberately narrower — the IR may express exactly what 68040 integer instructions do, and nothing else. This is a rule, not a preference; drift toward generality is the main way the layer becomes expensive.

Representative ops:

- `ld{8,16,32} tN ← [An + disp]`, `st{8,16,32}`, with big-endian semantics explicit.
- `add/sub/and/or/eor/neg/not/asl/asr/lsl/lsr/rol/ror/roxl/roxr {8,16,32}` with a **flags-wanted mask** (X N Z V C) attached by the liveness pass.
- `cmp`, `tst`, `btst/bset/bclr/bchg`.
- **Bitfield range ops** — `bfextu/bfexts/bfins/bftst/bfchg/bfclr/bfset/bfffo` (68020+). These need an op shape of their own: `{ op, base, reg, offset, width }`. They fit none of the shapes above — not the dyadic ALU form (no meaningful `src1`/`src2`), not a sized load/store (the value is a sub-byte-aligned range spanning a byte range, not a width-aligned access), and not the single-bit ops they sit next to. Added 2026-09-28 after the C2 prototype (`docs/cpu-core-poc-results.md`) hit them in real encodings and found no shape here that would take them.
- `mul/div` in 16- and 32-bit forms (call-out permitted). **`DIVU`/`DIVS` need conditional writeback:** on quotient overflow the 68k leaves the destination register *unmodified* and sets only V. No other op in this IR has a "computed a result and then did not write it" semantic, so the op format must carry it explicitly rather than leaving it to the emitter. Same provenance as the bitfield note above; the prototype's differential gate never exercised this path (its dividends are all zero), so it is specified here and owed a test at C2, not assumed working.
- `br`, `bcc cond`, `dbcc`, `jsr/rts`, `trap/trapv/chk`, `rte`, `stop`, `movem` (expanded), `cas/tas`.

  *`movem`'s expansion has a measured cost (2026-09-28).* Expanding it to one transfer per register is kept — the passes need to see individual transfers to act on them, and folding the loop inside one op hides the cost rather than removing it — but the prototype measured what it costs an interpreter tier: 11 retired instructions become 46 IR dispatches, and `movem_saverestore` is the one kernel where the prototype loses to m68k-rs's own `run_batch` path (0.63×). The interpreter tier should therefore carry a fast path for the common shape (a small fixed register list, decoded once, executed as a tight loop rather than N generic dispatches). Recorded as a known cost with a named mitigation, not left to be rediscovered at C2.
- `sync_pc`, `checkirq`, `callout id` for anything not worth inlining (FPU, cache control, privileged CR access).

Per-op metadata: source PC, byte length (for exception PC reporting), flags-defined, flags-used.

*Width note.* Register slots and temporaries in the IR are 64 bits wide, and the register file is not hard-coded to 16 entries. This is a type choice, not a feature: the hosts are 64-bit anyway, so it costs nothing, and it keeps the door open for a wider 68k variant later without anything else in this document changing. No 64-bit semantics are implemented or planned here.

*Front-end neutrality.* Four further shape decisions, taken now because each is a field in the op format today and a rewrite of every emitter table later. None adds an op, a pass, or a code path to the 68k work; the 68k front end simply uses the degenerate form of each. If any of them ever shows up as a cost in the 68k profile, drop it — the 68k core takes precedence.

1. **Three-operand ALU ops** (`dst, src1, src2`). The 68k front end always emits `dst == src2`. Emitters handle the two-operand host case (x86) with one move when `dst != src1`, which never happens for 68k input.
2. **Slot-addressed conditions.** A condition-producing op names the slot its flags go to; the flags-wanted mask and the liveness pass are per slot. The 68k front end uses a single slot (the CCR). Nothing else in this document changes.
3. **The register file is declared by the front end**, not the IR: count, which are address-like (for EA descriptors), which are pinned. The 68k front end declares D0–D7/A0–A7, PC, SR.
4. **Exceptions are a call-out** — `raise(vector, pc)` — with entry and frame construction owned by the front end. This is already the design in §4.8; recorded here so it is understood as a front-end boundary rather than a 68k detail.

Explicitly *not* done now: no non-68k ops (fused rotate-and-mask, condition-register field ops, reservation loads), no second decoder, no second register file, no scheduler for more than one core. Those belong to a front end that does not exist yet.

### 4.3 Passes (the only ones)

1. **Flag liveness** — backward scan marking which of XNZVC each op must actually produce. This is where most of the "dead flag" saving comes from and it is far simpler on IR than on raw opcodes.
2. **Peepholes** — `MOVE`+`TST` fusion, constant folding on immediate address arithmetic, `LEA`/`PEA` simplification. *Unvalidated as of 2026-09-28:* none of these three occurs anywhere in the C2 prototype's kernel set, so there is no evidence either way that they earn their decode-time cost. They stay as candidates, not commitments, until measured against a corpus closer to real Kickstart/AROS instruction mixes. The flag-liveness pass above is in the opposite position — measured, and the largest single contributor to the prototype's win.

Nothing else. If a third pass ever seems necessary, that is the signal to measure first.

### 4.4 IR interpreter

Straightforward threaded interpreter over the IR with lazy flag evaluation. Two jobs:

*"Lazy" here means liveness-gated, not deferred-operand (clarified 2026-09-28).* The flags an op computes are decided at decode time by §4.3's backward scan, and the interpreter simply skips the rest. The other scheme commonly called "lazy flags" — storing the last operation's operands and computing flags only when a later op reads them, deferred past decode time — is **not** what is specified here and not what the prototype measured. The distinction matters because the liveness pass accounts for most of the prototype's measured speedup, so a reader must not assume the stronger scheme is the one behind those numbers.

- Runs cold code and everything before a block gets hot (heat counter, threshold tunable; start at "translate on second execution").
- Serves as the on-target reference tier. On bare-metal, where m68k-rs is unavailable, JIT correctness is checked by running suspect blocks through the interpreter and comparing state. This is the same lockstep idea as the m68k-rs oracle, but self-contained.

### 4.5 Template JIT

One emitter per host ISA, implementing a small trait — approximately: emit load/store, emit ALU op with flag mask, emit compare-and-branch, emit call-out, emit block prologue/epilogue, patch chain target. Each emitter is mostly a table from IR op to a fixed instruction sequence with operands patched in.

Host-specific model:

| | aarch64 | x86-64 | riscv64 |
|---|---|---|---|
| Pinned m68k regs | all 16 + PC + base | ~12 (A0–A7, hot Dn); rest in state block | all 16 + PC + base |
| CCR | NZCV in hardware flags, X in a spare reg | hardware flags with borrow fix-up; X in a spare reg | no flag register — compute into regs, liveness pass matters most here |
| Endian swap | `REV`/`REV16` | `MOVBE`/`BSWAP` | Zbb `rev8` if present, else shift sequence |
| I-cache after emit | explicit `DC CVAU` / `IC IVAU` | coherent, nothing needed | `fence.i` |

Blocks are chained to successors once both exist; pending-interrupt checks happen at block boundaries and on backward branches only.

### 4.6 Memory model: direct address-space mapping

The 4 GB m68k address space is mapped at a fixed offset in host virtual memory. A m68k access to RAM is `base + a32`. RAM regions (chip, fast, VRAM, the ROM copy) are backed by real host memory.

**The map follows AUTOCONFIG.** Fast RAM, VRAM and every device window are placed by the guest at boot, so the direct map is built from AUTOCONFIG placements at configuration time, never from constants. This is the project's standing no-hardcoded-addresses rule (addresses are always asked from `MachineBus`), extended to the mapping layer. Placement changes rebuild the affected pages and the page-type table below, and invalidate any translated code whose I/O-site decisions depended on them.

**I/O is found inline, faults are a backstop.** A per-64 KB page-type table (RAM / ROM / I/O / open bus) sits beside the map. The interpreter consults it on every access; it is one load and a compare. The JIT does not fault on I/O as its main path: a signal round trip on Linux costs microseconds, and I/O is not rare here — interrupt handlers touch INTREQ/INTENA, timer.device reads the CIAs, and the RTG, hostblk and pktport drivers poll their registers. Instead:

- an access whose EA is statically known (absolute addressing, which is how most chipset and CIA code is written) is classified at translate time from the page-type table;
- an access through an address register is emitted as a direct access, and if it faults, the fault handler services the access through `MachineBus` and patches that site to an out-of-line I/O call, so each I/O-touching instruction faults at most once;
- on bare metal the fault path is cheap (the board owns the exception vector), but the same patching applies.

Fault cost on the Linux backend is measured at C1 before this split is committed to; if it is lower than expected, the patching can wait.

**Open-bus semantics are preserved.** Unmapped addresses read `$FF` and swallow writes, exactly as `MachineBus` does today; Kickstart's hardware probes depend on those reads succeeding. The page-type table classifies open-bus pages. Reads are served from a shared read-only page of `$FF`, so they take the fast path. Writes take the I/O path — the inline check when the address is static, otherwise fault once and patch the site — and are discarded. One mapping cannot serve both: a writable page would stop reading `$FF` after the first write, and a read-only one faults on every write. Guest writes into the read-only ROM copy are handled the same way. Bus errors are raised only where the real machine would raise them.

Kickstart is copied into RAM at boot and executed from the mapped copy, so ROM takes the fast path.

### 4.7 Translation cache and self-modifying code

- Fixed-size code cache carved from a static region (no heap). Simple bump allocation with whole-cache flush on exhaustion; refine only if measurement says so.
- Lookup keyed by m68k PC; optional cheap hash of the source bytes on lookup as a safety net.
- **Invalidation follows 68040 cache semantics.** A real 040 requires `CACRF`/`CPUSHA`/`CINV` (or `CacheClearU()` in OS terms) before executing modified code, and thirty years of AmigaOS software already does this. The core therefore invalidates on those instructions and does not scan every data write. This is the clearest example of the "limited but faster" trade, and it is legitimate precisely because the machine presents a 040. OS 2.0+ `LoadSeg` and `SetFunction` already flush.
- **Device DMA into code memory is the one case the OS does not cover for us.** The machine's own m68k drivers (hostblk, pktport, virtio-net) must `CacheClearE()` or `CacheClearU()` after DMA completes into a buffer that may hold code, as a real 040 driver must. On acceptance, this rule is recorded in each device's protocol document, not only here.
- The hash-on-lookup safety net is a byte comparison of the block's source against a stored copy on entry, as the fork's trace JIT already does; proven, cheap, and switchable off once the cache rules are trusted.

### 4.8 Exceptions and interrupts

Full 68040 exception model as far as AmigaOS and AROS exercise it: privilege violation, trap, illegal, line-A/line-F, division by zero, CHK, and bus error only where the real machine raises one — unmapped regions are open bus and read `$FF` (§4.6). Stack frame formats 0, 2 and 7 as the OS expects. Interrupt levels 1–7 with the board layer presenting an Emu68-style virtual controller so paravirtual devices raise INT2/INT6 cheaply.

### 4.9 FPU

68881/68040-compatible FPU implemented as a call-out slow path, correct before fast. Never allowed to complicate the integer core.

## 5. Correctness strategy

Correctness is what makes a second core affordable.

### 5.1 Layers

1. **Instruction-level suites** — the SingleStepTests 68000 set, run against the IR interpreter and each JIT emitter. *Open item:* confirm whether a 68020+ set exists. If it does not, the 020+ addressing modes and instructions (scaled index, memory-indirect, bitfields, `CAS`, 32-bit `MUL`/`DIV`) rest on lockstep alone, and a small hand-written 020+ vector set generated from m68k-rs becomes a C2 task.

  *This open item got more pressing on 2026-09-28, not less.* The prototype's own differential gate compared registers, PC and all of guest RAM, but **not SR** — so its flag formulas are unverified by anything — and its kernels never reach the `DIVU`/`DIVS` overflow path specified in §4.2. Those are precisely the two classes this layer exists to catch, and both are invisible to a lockstep run over ordinary code that happens not to exercise them. Resolving whether a 68020+ suite exists is therefore on the critical path to C2, not a footnote to it.
2. **Record/replay lockstep vs m68k-rs** — see §5.3.
3. **Lockstep vs the IR interpreter** — on every target including bare-metal, so emitter bugs are caught where m68k-rs cannot run. Interpreter and JIT share the same core and the same replay log, so this is the same mechanism with a different reference.
4. **Acceptance gates** — Kickstart 3.2 (private runners) and AROS 68k (public CI) boot to Workbench on each backend, in the deterministic mode of §5.2.

Rule: the fast core does not become mainline on a backend until layers 1–3 are clean there.

### 5.2 Deterministic mode without cycle tables

ADR 0006 keeps cycle-budgeted timing as the reproducible mode for frame-count tests and gates. A core without cycle accounting cannot provide that. The replacement is **fixed-instructions-per-line**: the run loop retires exactly N instructions per raster line and advances beam, CIAs and per-line device engines after each batch. While the CPU is stopped, lines advance with no instructions retired, as STOP already does in cycle mode.

N is configurable. Its default is the average number of instructions today's cycle-budgeted mode retires per raster line while the CPU is not stopped, measured at C1. Choosing it that way, rather than modelling some other clock speed, lets the existing frame-count budgets carry over with the least re-baselining. This mode is reproducible on any core, including m68k-rs, which lets the existing gates be re-baselined once under the new definition and then shared by both cores. On acceptance, ADR 0006 is amended to define it (§12).

### 5.3 Record/replay lockstep

Free-running lockstep does not work: the new core has no cycle accounting, so an interrupt lands at a different instruction in each core and the two diverge at the first IRQ. Lockstep is therefore master/replay:

- **m68k-rs is the master** and runs the real machine. It logs, keyed by retired-instruction index: every IPL change; the value returned by every I/O read (anything the page-type table does not classify as RAM/ROM); and **every device write into guest memory** — hostblk and pktport DMA, blitter writes to chip RAM, virtio-net buffers — since the replaying core has its own copy of RAM and must see the same contents at the same point.
- **The new core replays** that log against its own memory: IPL changes are applied at the logged index, I/O reads return the logged values, device memory writes are applied at the logged index, and I/O writes are compared against the master's.
- **Comparison** is of registers, SR and the address and value of every write, at each instruction boundary or at block boundaries with bisection on mismatch. Flag bits the 68k leaves architecturally undefined — after a `DIVU`/`DIVS` overflow, the N/V results of the BCD instructions, and the others the Programmer's Reference Manual marks as undefined — are masked per opcode, or the harness will chase false divergences.
- The log format and the hook points it needs belong in the **C1 CPU trait**, not bolted on at C2: the trait exposes retired-instruction count, a way to inject IPL at an index, and an I/O callback both cores route through.

The same log format serves layer 3 on bare metal, with the interpreter as master.

## 6. no_std discipline

- Core crate is `#![no_std]` from the first commit, with a CI job building it that way regardless of whether it runs anywhere yet.
- All buffers — IR arena per block, code cache, translation table — are bounded and statically sized.
- Debug output goes through a board-provided sink; the core never assumes a console.
- Debug tooling from day one: dump m68k disassembly, IR, and emitted native side by side for any block. Without this, a three-layer pipeline is much harder to debug than a one-layer interpreter.

## 7. Prior art and what is (and isn't) taken

- **Emu68 (MPL-2.0; ideas only, no code taken):** direct address-space mapping, host flags for CCR, pinned registers, cache-semantics invalidation, block chaining, virtual interrupt controller. Not taken: Pi board layer, mailbox drivers, its FPU path. MPL-2.0 is file-level copyleft, so any Emu68 code copied into this project would have to stay in its own MPL-licensed files; taking techniques rather than code avoids that.
- **Amithlon:** direct memory mapping on x86, endian handling, running from a copied ROM. Its JIT was x86-only and closed.
- **QEMU TCG:** the general shape of a thin IR with per-target lowering. Not taken: its generality, its optimiser, its hosted-only design.
- **Amiga DE / Tao Elate:** the idea that a portable intermediate form makes new host ISAs cheap. Not taken: using it as a distribution format.
- **Transmeta CMS:** multi-tier translation (interpret, translate, re-optimise). Not taken: the VLIW-motivated optimiser and speculation machinery; two ordinary out-of-order hosts don't need it.
- **Cranelift:** not used. It cannot run no_std, so it could never reach either real target. It remains a possible third consumer of the IR on Linux if someone wants to measure it.

## 8. Roadmap

Milestones are sequential and each carries the correctness layers from §5.

- **C0 — Bus fast path and baseline.** *Largely complete.* The fast path, LTO, per-line device engines, the lazy CIA/beam tick and max mode are on `main`, with before/after numbers in `docs/bus-fast-path-plan.md`. *Remaining exit: one max-mode profile (no per-instruction hook) from the step 7.2 branch, recorded in the table below. That flamegraph is the go/no-go for everything after it: if m68k-rs frames are not the majority of host time in max mode, stop here.*

  | Measurement (max mode) | Hosted x86-64 | Hosted aarch64 | Rock 5B / KVM |
  |---|---|---|---|
  | KS 3.2 boot to Workbench (s) | (no host) | ~4 s to an idle Workbench (frame ~200, screenshot-confirmed; `docs/bus-fast-path-plan.md` step 7.2). The 109.60 s `WBREADY` figure measured here is the scripted input's fixed frame schedule, not boot time -- see `docs/cpu-core-c0-profile.md` §3 and the supervisor note there. | (no host) |
  | AROS boot to Workbench (s) | (no host) | not measured -- see `docs/cpu-core-c0-profile.md` §5 | (no host) |
  | Idle-loop MIPS | (no host) | ~0.1-0.2 M/s | (no host) |
  | m68k-rs share of host time (%) | (no host) | CPU-busy: boot 65.7-68.3%, CPUBench 80.6-80.9% (majority, both) -- wall-clock: boot 2.6%, CPUBench 13.4-13.6%. See `docs/cpu-core-c0-profile.md` for method, per-workload attribution and the Amdahl implication. | (no host) |

  **C0 result: m68k-rs is the CPU-busy-time majority of max-mode host time on both measured workloads -- go.** Full profile, attribution breakdown and reproduction commands: `docs/cpu-core-c0-profile.md`. That document also flags an open question for §2.1: the measured wall-clock share says boot-to-Workbench time is dominated by wall-clock-paced STOP sleep, not CPU throughput, so a 3-7x core (`docs/cpu-core-poc-results.md`) predicts roughly 2% faster boot, not 3-7x -- the large multiplier applies to sustained CPU-bound foreground work, not to boot time. Raised for the supervisor's judgment, not acted on here.

- **C1 — Core trait, replay log, direct mapping.** Swappable CPU trait in the machine, keeping the fork behind it, with the replay hooks of §5.3 in the trait from the start. Fixed-instructions-per-line deterministic mode (§5.2) implemented and the gates re-baselined under it. Direct address-space mapping on the hosted backend built from AUTOCONFIG placements, with the page-type table and open-bus pages of §4.6. Measure signal-fault cost on Linux here. *Exit: m68k-rs runs through the trait in both timing modes, producing replay logs; gates pass in the new deterministic mode; fault cost recorded.*
- **C2 — Decoder + IR + interpreter.** no_std crate, instruction suites pass (plus the 020+ vector set if §5.1's open item requires it), replay lockstep vs m68k-rs clean over both boots. *Exit: IR interpreter boots both OSes on Linux; speed vs m68k-rs recorded.*
- **Decision point after C2.** The floor is at least twice a 68060; faster is better. If C2's interpreter meets the floor, the machine is acceptable without a JIT, and C3 is scheduled on C2's measured gap and workload evidence rather than as a rescue. If it falls short, C3 is required. The interpreter is a permanent tier either way.
- **C3 — aarch64 emitter.** Template JIT with chaining, cache-semantics invalidation and fault-patched I/O sites. Replay lockstep vs interpreter and vs m68k-rs on Linux/aarch64. *Exit: JIT boots both OSes; measured speed recorded against the floor and against C2.*
- **C4 — Bare-metal RK3588.** (After C3; if C3 is deferred, the interpreter goes bare-metal here first.) Same core, board layer owns page tables and I-cache maintenance. *Exit: both OSes boot unattended bare-metal; lockstep vs interpreter clean.*
- **C5 — x86-64 emitter.** Linux first, then UEFI. *Exit: parity with C3/C4 on x86.*
- **C6 — Mainline switch.** m68k-rs demoted to oracle-only; not linked into shipping images.
- **C7 (optional) — riscv64 emitter.** Exists mainly to prove the port cost of a third ISA is one emitter. Done on QEMU virt; a real board only if one is to hand.

## 9. Risks

- **Scope creep in the IR.** Mitigated by the "68040 semantics only" rule and by refusing a third pass without measurement.
- **Emitter bugs that only appear bare-metal.** Mitigated by the on-target interpreter lockstep; this is the reason the interpreter is a permanent tier, not a stepping stone.
- **Software that modifies code without cache flushes.** Real 040/060 machines break it too; the hash-on-lookup safety net catches the common case; a board-level "paranoid invalidation" switch can exist for diagnosis.
- **x86 register pressure.** Accepted; ~12 pinned registers still leave x86 well ahead of an interpreter.
- **Two cores under two acceptance gates.** Bounded by the mainline rule in §5 and by m68k-rs shrinking to a test dependency.
- **Fault-driven I/O too slow on Linux.** Mitigated by the inline page-type table and one-fault-per-site patching (§4.6), and measured at C1.
- **Replay log size.** Device DMA logging on a full boot is large; the log is streamed to disk on the hosted backend and bounded by a ring with checkpointed RAM snapshots when a divergence window is known.

## 10. Open questions

- Heat threshold and whether to translate straight-line OS code eagerly at boot (Kickstart is small enough to pre-translate).
- Whether the 68040 `MOVE16` and the 060-only instructions should be present for software that probes for them.
- FPU precision model (extended-precision emulation vs host double) — affects some Amiga software noticeably.
- Whether the IR block format should be stable enough to serialise for offline debugging tools, or kept purely in-memory.
- Whether a 68020+ SingleStepTests set exists (§5.1).
- The measured instructions-per-line default for the deterministic mode (§5.2), and whether any existing frame-count test depends on cycle-exact behaviour that the new definition changes.
- Naming.

## 11. Possible future front ends (not planned)

The neutrality decisions in §4.2 exist for these. None is scheduled; each is a separate project that starts, if at all, after C6.

- **PowerPC 603e/604e coprocessor** — the CyberStorm PPC / BlizzardPPC model: a second core sharing the directly-mapped memory, with a mailbox board for a WarpOS/PowerUP-style kernel. Integer ops, loads/stores (including the update forms, which reuse the 68k post-increment write-back mechanism) and branches map onto the existing IR; conditions go to CR0–CR7 and XER via the slot mechanism; the FPU is IEEE double and lowers to native host float ops, so it would be faster than the 68881 path. New work would be: a PPC decoder, a small emulated TLB with software walk for page-table-backed regions (BAT-mapped shared memory needs nothing beyond direct mapping), PPC exception entry, a two-core run loop, and the board. The board need not clone a CyberStorm; a WarpOS-compatible kernel that abstracts the hardware (as the Sonnet project's does for PCI G3/G4 cards, as far as is known) makes a doorbell-style virtual board the cheaper route. PiStorm is reported to be adding PPC support, which is worth watching for kernel and board-interface precedent.
- **68k-64** — see the width note in §4.2 and the separate proposal; deferred until that ISA's encoding audit is done.

## 12. Relation to other project documents

None of the changes below has been made. They are what accepting this proposal would require, and each is made in the commit that accepts it, not before.

- **ADR 0001 (bare metal vs Linux host).** This proposal makes the bare-metal SBC a deliverable, which is option A in all but name. On acceptance, ADR 0001 records that decision as accepted, citing this proposal as the reason, rather than leaving it implied here.
- **ADR 0006 (timing).** On acceptance, amended to define fixed-instructions-per-line as the reproducible mode for tests and gates (§5.2); cycle-budgeted timing remains available for m68k-rs but stops being the definition of "deterministic".
- **m68k-machine-proposal.md.** On acceptance, the "Cranelift as the single JIT upgrade path" line is superseded; Cranelift moves to "optional experiment on Linux". The combined roadmap's Cranelift trace JIT item is replaced by this proposal's C3.
- **Board-layer contract** (machine proposal) gains: building the direct map and page-type table from AUTOCONFIG placements at configuration time and on every placement change; open-bus pages; I-cache maintenance after emission; fault dispatch and site patching for I/O; the virtual interrupt controller.
- **Device protocol documents** (hostblk, pktport, virtionet, rtgboard) each gain the rule that their m68k driver flushes caches after DMA into memory that may hold code (§4.7).
- **bus-fast-path-plan.md** supplies C0's numbers; C1's direct mapping subsumes its RAM fast path.
