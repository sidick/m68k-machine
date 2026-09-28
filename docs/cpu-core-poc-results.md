# `m68k-vp` C2 proof-of-concept: results

**Status:** PoC evidence for `docs/cpu-core-proposal.md`'s §8 "Decision
point after C2". This document does not decide anything -- it reports
what was built and measured. `docs/cpu-core-proposal.md`,
`docs/bus-fast-path-plan.md` and the ADRs are unedited; the supervisor
decides what these results change in them.

**The one question this answers:** is a purpose-built IR interpreter
materially faster than m68k-rs's interpreter on the same retired
instructions? **Yes, on 10 of 11 kernels, by 1.6x-15x; on the eleventh
(`movem_saverestore`) by only 1.6x, and that kernel's cold-decode number
is actually *slower* than m68k-rs's own `FastMem` batch path.** Both
correctness gates pass on every kernel. Details, including where the
speedup comes from and where it doesn't hold up, follow.

## What was built

- `crates/m68k-vp`: a new workspace member. `#![no_std]`, no allocator
  (guest RAM and every internal buffer -- the per-block IR arena, the
  32-slot block cache -- are bounded, statically sized, and RAM is
  borrowed from the caller, never owned). Confirmed genuinely `no_std`
  by building the lib target for a bare-metal target with no allocator:
  `cargo build -p m68k-vp --lib --target aarch64-unknown-none` succeeds.
  - `src/decoder.rs` -- 68k opcode bytes to IR, one basic block at a
    time.
  - `src/ir.rs` -- the IR op set and the per-block arena.
  - `src/passes.rs` -- flag liveness (backward scan) and one peephole.
  - `src/interp.rs` -- the block-cached threaded interpreter.
  - `src/mem.rs` -- flat, power-of-two, big-endian guest memory,
    mirroring `m68k::core::memory::LinearMemoryBus`'s shape.
  - `src/tests.rs` -- both mandatory correctness gates, permanent
    `cargo test` coverage, with m68k-rs as a `[dev-dependencies]`-only
    oracle (never a runtime dependency of the library).
- `crates/cpu-bench` extended with two more measured paths per kernel,
  `vp-interp` (steady-state, block cache stays warm) and `vp-cold`
  (block cache flushed every measurement chunk), alongside the existing
  `interp`/`batch` columns.

## Correctness gates

Both mandatory, both pass, for all 11 kernels:

1. **Retired-instruction parity** (`crates/m68k-vp/src/tests.rs::every_kernel_calibrates_on_vp`,
   and re-run before every timing measurement by `crates/cpu-bench`'s
   `calibrate_vp`): 97 outer iterations retires exactly
   `fixed_overhead + 97 * instrs_per_iter` per `kernels.s`'s own tables,
   stopping exactly at the kernel's own `rts`.
2. **Differential state check vs m68k-rs**
   (`crates/m68k-vp/src/tests.rs::every_kernel_matches_m68k_rs`): after
   the same 97-iteration run, D0-D7, A0-A7, PC and the *entire* 256 KiB
   guest RAM buffer match m68k-rs's `CpuCore`/`LinearMemoryBus`
   byte-for-byte. No bits are masked.

```
cargo test -p m68k-vp
cargo test -p cpu-bench
```

Both are green as of this PoC. No flag bit is compared anywhere in
either gate (SR is not part of what's asserted, per the task brief) --
`m68k-vp`'s flag computation is implemented to the standard formulas
anyway (see "Spec issues found" below) but is untested by these gates,
which is stated plainly rather than left implicit.

## Results

**Host:** Apple M3 Pro, macOS (Darwin 25.6.0, arm64), rustc/cargo
1.98.1, release profile (workspace `[profile.release]`: `lto = "fat"`,
`codegen-units = 1`).

**Reproduce:**

```
cargo run -p cpu-bench --release
```

Two runs, values below are the average of both (M instr/s):

| kernel | interp | batch | **vp-interp** | **vp-cold** | vp-interp/interp | vp-interp/batch |
|---|---:|---:|---:|---:|---:|---:|
| reg_addq_bra | 104.8 | 179.9 | **332.4** | **147.3** | 3.17x | 1.85x |
| reg_tst_bne | 62.0 | 221.1 | **930.5** | **188.6** | 15.01x | 4.21x |
| reg_mix | 86.0 | 165.7 | **419.8** | **156.8** | 4.88x | 2.53x |
| mem_copy | 52.5 | 126.2 | **205.5** | **112.8** | 3.91x | 1.63x |
| mem_fill | 49.9 | 181.3 | **364.0** | **143.7** | 7.30x | 2.01x |
| struct_walk | 47.5 | 133.1 | **266.9** | **123.8** | 5.62x | 2.00x |
| movem_saverestore | 37.4 | 94.9 | **60.2** | **40.4** | 1.61x | 0.63x |
| jsr_rts_chain | 89.5 | 123.7 | **420.6** | **42.2** | 4.70x | 3.40x |
| muldiv_mix | 90.2 | 140.8 | **610.3** | **175.7** | 6.77x | 4.34x |
| bitfield_ops | 72.6 | 81.8 | **431.4** | **116.2** | 5.95x | 5.27x |
| cmp_branchy | 75.2 | 196.4 | **712.2** | **178.5** | 9.47x | 3.63x |

`interp`/`batch` are m68k-rs's existing columns (unchanged methodology,
`docs/bus-fast-path-plan.md` step 8); this run's build has no `jit`
feature enabled, matching that step's plain (non-Cranelift) `batch`
column, not `batch+jit`. `vp-interp` and `vp-cold` are `m68k-vp`'s two
new columns:

- **`vp-interp`**: steady-state -- the block cache stays warm for the
  whole `MIN_MEASURE_SECONDS` window, the same shape as `interp`/`batch`
  (huge outer iteration count, huge instruction budget per call,
  accumulate until 1 second elapsed). Directly comparable to `interp`:
  both measure "decode/dispatch once, run the hot loop" over identical
  retired instructions.
- **`vp-cold`**: the block cache is flushed before every measurement
  chunk, and each chunk's instruction budget is sized to roughly one
  outer iteration of that kernel -- so every chunk pays full decode cost
  for the same few blocks, never amortized across millions of
  iterations. This is the number the task brief asks for "so the
  steady-state optimism of tiny hot loops is visible next to it".

## What the numbers say

**On 10 of 11 kernels, `vp-interp` beats m68k-rs's `interp` by
1.6x-15x, and beats even m68k-rs's `FastMem`-accelerated `batch` path by
1.6x-5.3x.** The two largest ratios (`reg_tst_bne` 15.0x, `cmp_branchy`
9.5x) are exactly the kernels §4.3 predicts should benefit most from
flag liveness: both are dominated by a flag-producing op (`tst.l`/
`cmp.l`) every iteration whose flags are *always* dead (nothing ever
reads them before the next flag-producing op overwrites the CCR --
`passes.rs`'s module doc walks through why), so `m68k-vp` skips
computing N/Z/V/C on 8 of every 9 flag-producing ops per iteration,
while m68k-rs's general-purpose interpreter computes them every time.
This is the clearest direct evidence in this PoC that §4.3's central
claim -- "this is where most of the dead flag saving comes from" -- is
real and large, not theoretical.

**Supervisor correction to the attribution above: those two ratios are
not flag liveness alone, and neither kernel should be cited as evidence
about real code.** Flag liveness is one of *two* effects compounding in
`reg_tst_bne` and `cmp_branchy`, and the second is larger than the
first. Each of those kernels' 18 retired instructions per iteration is
8 flag-producing ops (`tst.l`/`cmp.l`) whose flags liveness proves dead
-- which, with the flags mask empty, leaves each one a complete no-op --
*plus* the 8 zero-displacement branches vasm assembled as
`lea (An),An`, which the peephole pass folds to `Op::Nop` outright (see
the `kernels.s` documentation finding above, and `passes.rs`). So 16 of
every 18 instructions `m68k-vp` reports retiring in those two kernels
cost it approximately nothing, while m68k-rs genuinely executes all 16.
The retired-instruction counts are correct and both gates pass -- this
is not a measurement error, and no instruction is skipped for counting
purposes -- but an "M instr/s" figure that prices deleted instructions
at full weight is inflated by roughly the same 8/9 ratio, and the 15.0x
and 9.5x are substantially an artifact of a synthetic kernel shape
(`lea (An),An` is an assembler artifact of branch-to-next; it does not
occur in compiled code) rather than a generalizable property of the
design.

The honest statement of the flag-liveness result is therefore the
*other* kernels, where no instructions are deleted: `reg_mix` (4.9x),
`mem_fill` (7.3x), `muldiv_mix` (6.8x), `bitfield_ops` (6.0x). §4.3's
claim still holds and is still large -- it is just 3-7x, not 15x. Any
use of this PoC to inform §8's decision point after C2 should quote the
3-7x band and treat `reg_tst_bne`/`cmp_branchy` as unusable.

**`movem_saverestore` is the outlier, and it is instructive rather than
a fluke.** `vp-interp` beats m68k-rs's `interp` by only 1.6x and loses
outright to m68k-rs's `batch` (0.63x -- `m68k-vp` is *slower* than
m68k-rs's FastMem-accelerated path on this one kernel). The cause is the
task brief's own requirement that `movem` expand into one IR micro-op
per register rather than staying a single op (§4.2): this kernel's
11-real-instruction loop body (`3 * (lea + movem-store + movem-load) +
subq + bne`) expands to 46 IR ops, so the interpreter pays a full op
dispatch for every one of the 21 individual register transfers even
though only 3 `movem`s and a handful of other instructions actually
retire. Per-retired-instruction dispatch overhead is roughly 4x what it
is for a kernel with no expansion. See "Spec issues found" below --
this is a real, measured cost of the expansion requirement, not a
`m68k-vp` bug.

**`jsr_rts_chain`'s cold number (42.2 M instr/s) is the worst `vp-cold`
result and worse than its own `interp` figure, entirely from block
fragmentation.** Every `bsr` ends its containing IR block (`interp.rs`'s
module doc explains why: control transfer is where this PoC's blocks
end, and a call is a control transfer), so the kernel's loop body
(4 `bsr`s, then `subq`/`bne`) decodes as five separate one- or two-op
blocks plus a two-op leaf block, instead of one block with a handful of
inline calls. `vp-interp` hides this completely once all six blocks are
cached (420.6 M instr/s, among the best in the set); `vp-cold` pays full
redecode for all six every chunk. This is a genuine, unhidden limitation
of block-granularity call handling, not a JIT concern -- a template JIT
that inlines small leaf callees would remove it, but that is C3
territory, out of scope here.

**Against the roadmap's stated floor** (§8: "at least twice a 68060"),
every `vp-interp` number here (60-930 M instr/s) clears
`docs/bus-fast-path-plan.md` step 8's scalar-68060 reference figures
(1.6-42.2 M instr/s per kernel, measured under Copperline) by a wide
margin -- but so does m68k-rs's own bare `interp` column already (step
8's own conclusion). This says nothing new about the floor: both
numbers are bare-kernel-loop numbers with zero machine overhead, and
step 8 already established that gap is not where the floor is missed.
What `vp-interp` adds is a *materially larger* margin over that floor on
the same instructions, which is relevant to §8's "faster is better" but
is not itself a machine-level result (see "What this does NOT show").

## Decoder coverage

Verified against the assembled blob (`m68k/cpubench/kernels.bin`) with
Capstone, not against `kernels.s`'s own comments -- see "Spec issues
found" for why that distinction mattered.

**Instructions:** `addq.l`/`subq.l` (immediate 1-8), `moveq`, `movea.l`
(register-direct), `move.{b,w,l}` (register/memory/immediate
combinations the kernels use), `lea`, `add.l`/`sub.l`/`and.l`/`or.l`/
`eor.l`/`cmp.l` (register-direct only), `not.l`, `neg.l`, `tst.l`,
`ext.l` (word-to-long only), `asl.l`/`lsr.l` (immediate count,
register-direct), `mulu.w`/`muls.w`/`divu.w`/`divs.w` (register-direct),
`movem.l` (register-list, predecrement/postincrement), `bfextu`/`bfins`
(register-indirect base, immediate offset/width), `bsr`, `bne`
(byte- and word-displacement forms), `rts`, `nop`.

**Addressing modes:** Dn, An, `(An)`, `(An)+`, `-(An)`, `d16(An)`,
immediate. **`abs.l`/`abs.w` are not implemented** -- verified absent
from every kernel's assembled bytes, so not needed; noted here rather
than silently omitted.

**Not implemented, deliberately, because unused:** `asl`/`lsr` by
register count, `ROx`/`ROXx` rotates, register-indexed bitfield
offset/width, memory-operand shift, `Scc`/`DBcc`, any addressing mode
beyond the six listed, MOVEA with word size (sign extension), any
opcode not enumerated above. All panic loudly (with the offending
opcode word and PC) rather than silently misdecoding -- see
`decoder.rs`'s module doc for why that's the right failure mode for
this scaffolding.

**A `kernels.s` documentation finding, not a `cpu-core-proposal.md`
issue, recorded here because CLAUDE.md says to check code before
trusting docs:** `kernels.s`'s comments for `reg_tst_bne` and
`cmp_branchy` claim vasm "folds" each zero-displacement `bne.s`/`bcc.s`
into a NOP. Disassembling the actual assembled bytes shows this is
false -- vasm instead emits `lea (An),An` (a real instruction, same
size, that changes no architectural state). `m68k-vp`'s decoder handles
it as an ordinary `lea`, and the peephole pass (`passes.rs`) folds the
self-referential case to a true no-op. This cost nothing to work around
once found, but the stale claim would have caused a silent, wrong
decode (an unrecognized-as-Bcc opcode is not what an `lea`-family
pattern check would catch) if the decoder had been written to match the
comment instead of the bytes.

## Spec issues found

Per the project owner's brief: contact with real instruction encodings
is what a PoC is for, and the proposal is a Draft. Recorded here, not
worked around silently, and `docs/cpu-core-proposal.md` itself is
unedited.

**1. `movem`'s required micro-op expansion (§4.2) has a real,
measured interpreter cost, and it's the single worst result in this
PoC.** The task brief requires `movem` to expand into individual
load/store IR ops rather than stay one op. That's implemented
(`decoder.rs::decode_movem`), and it's the direct cause of
`movem_saverestore` being the only kernel where `vp-interp` doesn't
clearly beat m68k-rs: 46 IR-op dispatches for 11 retired instructions
means dispatch overhead dominates. This isn't a case for reversing the
expansion requirement (the liveness/peephole passes need to see
individual register transfers to ever act on them, and a combined op
would just move the cost into a hidden loop inside one op's execution
rather than removing it) -- but it is a concrete data point that a
production interpreter tier will want a fast path for a single common
shape (`movem` with a small, fixed register count decoded once and
executed as a tight native loop rather than N generic op dispatches),
which §4.2/§4.4 don't currently call out. Worth a line in the proposal
either committing to that fast path or explicitly accepting the
dispatch cost.

**2. Bitfield instructions (`bfextu`/`bfins`) don't fit any op shape
§4.2 actually lists.** §4.2's representative op list names `btst`/
`bset`/`bclr`/`bchg` (single-bit) but never mentions the bitfield
*range* instructions (`bfextu`/`bfins`/`bftst`/`bfchg`/`bfclr`/`bfset`/
`bfexts`/`bfffo`) at all, despite these being real 68020+ instructions
this project's own kernel set uses. They don't fit the three-operand
ALU shape (no `src1`/`src2` dyadic form applies), the load/store shape
(the "value" is a sub-byte-aligned bitfield within a byte range, not a
sized load/store), or the single-bit shape already listed. This PoC
needed a dedicated `Op::Bitfield { op, base, reg, offset, width }`
variant with its own field shape. Recommend §4.2 add an explicit
bitfield-op family (with the coordinator's own prediction borne out:
this was one of the "likely candidates" for an awkward fit).

**3. `mulu`/`muls`/`divu`/`divs`'s "conditional writeback" doesn't fit
the ALU op shape either, and it's architecturally real, not an edge
case to paper over.** Real 68k `DIVU`/`DIVS` leave the destination
register **unmodified** on quotient overflow (only V is set) --
verified against the Motorola Programmer's Reference Manual and
implemented that way in `interp.rs::Op::MulDivOp`'s `DivU`/`DivS` arms
(`checked_div`/`checked_rem` returning `None` covers both zero-divisor
and the `i32::MIN / -1` overflow case; the width-overflow check is a
separate guard on the quotient after division succeeds). No other op in
this IR has a "computed a result but conditionally didn't write it"
semantic -- every ALU/shift/load op in §4.2's shape always writes its
destination. §4.2 lists "`mul/div` in 16- and 32-bit forms (call-out
permitted)" without flagging this, and it's the kind of detail that
only surfaces on contact with the real encoding, exactly as the
project owner's brief anticipated. This PoC's kernels never exercise
the overflow path (the fixed divisor is 7 and every dividend starts and
stays at 0 across the whole run -- worth noting as a real gap in what
this PoC's differential gate actually exercises, not just a spec
observation: gate 2 passing here says nothing about the overflow path's
correctness).

**4. §4.3's three named peephole examples don't occur anywhere in this
kernel set.** No `MOVE`+`TST` pair, no immediate address-arithmetic
constant folding opportunity, no `PEA` at all appears in
`m68k/cpubench/kernels.s`. The one peephole this PoC actually
implements and measures (folding the `lea (An),An` self-reference from
the `kernels.s` documentation finding above into a true no-op) is an
artifact of how this specific blob got assembled, not one of §4.3's
examples. This isn't a defect in §4.3 -- it's a scope limit of this
kernel set, which is a synthetic microbenchmark, not representative
guest code -- but it means **this PoC provides no evidence either way**
for whether §4.3's three named peepholes are worth their decode-time
cost on real Kickstart/AROS code. That's an open question this PoC
cannot close; a peephole worth measuring would need a corpus closer to
real guest instruction mixes.

**5. Confirmations -- sections exercised and found sound:**

- **§4.2's three-operand `dst == src2` degenerate ALU form** held up
  with zero friction: every dyadic ALU op the kernels use (`add.l`,
  `sub.l`, `and.l`, `or.l`, `eor.l`, `cmp.l`) decoded cleanly into
  `dst`/`src` fields with `dst == src2` implicit, and the interpreter
  never needed a separate `src1` field. No measurable cost from
  carrying the generality (the field simply isn't there in this PoC's
  op struct, since the 68k front end never needs it) -- consistent with
  §4.2's own "if it costs something, drop it" framing, except here it
  cost nothing to include in the first place because a 68k-only op
  struct just doesn't have the extra field.
- **§4.2's "register file declared by the front end"** decision was
  straightforward: a flat 0..16 unified D0-D7/A0-A7 index (`ir.rs`'s
  `dreg`/`areg` helpers) needed no special-casing anywhere in the
  decoder or interpreter.
- **§4.3's flag liveness pass** is real and large, not marginal --
  see "What the numbers say" above. This is the strongest positive
  result in this PoC.
- **§4.4's IR interpreter with a PC-keyed block cache** decodes each
  kernel's hot loop exactly once (confirmed by the `vp-interp`/
  `vp-cold` gap, which is entirely decode-vs-cached-dispatch cost) and
  is materially faster than m68k-rs's general interpreter on identical
  retired instructions for every kernel except the `movem`-heavy one
  (issue 1, above).
- **§4.2's "slot-addressed conditions" and "exceptions are a call-out"
  neutrality decisions** were not exercised at all by this PoC (one CCR
  slot only; no exceptions modeled) -- neither confirmed nor
  contradicted, just untested.

**6. A terminology note on §4.4's "lazy flag evaluation."** This PoC
implements it as *liveness-gated* flag computation: the backward scan
(§4.3) decides at decode time which flag bits an op must compute, and
the interpreter simply skips the rest. That's different from the fuller
scheme some interpreters call "lazy flags" -- storing the last
operation's operands and computing flags only if a later op reads them,
deferred past decode time. Both are commonly called "lazy"; this PoC
built the cheaper one because the workload's basic blocks each end in
exactly one flag consumer (`bne`, reading Z), so the two schemes
produce the same skip decisions here regardless. §4.4 doesn't
distinguish between them, and given how much of this PoC's win the
liveness pass accounts for (finding 5, above), the distinction is worth
making explicit in the proposal so a future reader doesn't assume the
deferred-operand scheme was measured.

## Corners cut (honest account, for inflation risk)

- **IR op granularity is coarser than §4.2's shape**, for every
  instruction except `movem`. A `MoveMem`/`LoadReg` op resolves its own
  addressing-mode side effects (e.g. `(An)+`'s post-increment) inline
  rather than decomposing into separate address-computation and
  load/store ops the way a strict TCG-style IR would. This reduces
  op-dispatch count relative to the proposal's fuller decomposition, so
  **the interpreter numbers above are somewhat more favorable than a
  fully decomposed IR would produce** -- `movem`'s numbers (the one
  instruction decomposed as specified) are the closer analogue to what
  full decomposition would cost everywhere. This is disclosed, not
  hidden: `ir.rs`'s module doc states the same thing.
- **Flag computation (N/Z/V/C/X formulas) is implemented but untested**
  by either correctness gate (SR isn't part of what's compared, per the
  brief). The formulas are standard and reviewed by hand against the
  Programmer's Reference Manual, but "untested by the differential gate"
  is a real gap, not a formality -- a wrong V/C formula would not be
  caught by anything in this PoC or its `cargo test` coverage.
- **DIVU/DIVS overflow and divide-by-zero paths are never exercised**
  by any kernel (issue 3, above) -- the differential gate's coverage of
  `MulDivOp` is limited to the trivial all-zero-operand path these
  kernels happen to hit.
- **Block cache has no eviction policy or self-modifying-code
  safety net.** Correct for this PoC (the kernel blob is never
  modified), explicitly out of scope for anything beyond that
  (`interp.rs`'s module doc says so), and would need real design work
  before any of this touches guest code that writes to itself.
- **"Threaded interpreter"** here means a `match` over an `Op` enum
  compiled to a jump table by rustc, not computed-goto direct threading
  (safe Rust has no stable equivalent without nightly `asm goto`). This
  is the same interpretation most "threaded interpreter" Rust code
  uses; noted so the term isn't read as a stronger claim than what was
  built.
- **Two runs only**, not a statistically rigorous benchmark protocol.
  The two runs' per-kernel values agree to within a few percent (see
  the raw two-run data this table's averages come from, in this
  document's history/commit), which is the same informal confidence
  level `docs/bus-fast-path-plan.md` step 8 used for its own bare-harness
  numbers.

## What this does NOT show

- **No interrupt/IPL checks or device sync in either path.** Both
  `m68k-vp` and the m68k-rs columns it's compared against run a bare
  kernel loop with no interrupts, no `MachineBus`, no devices at all.
- **Tiny hot loops are the best case for block caching.** Every kernel
  here is a single basic block (or, for `jsr_rts_chain`, a handful of
  tiny ones) re-executed millions of times with a warm cache in
  `vp-interp`. Real guest code -- OS calls, branchy application logic,
  cold paths -- will decode far more blocks relative to how many times
  each runs, closer to `vp-cold`'s profile than `vp-interp`'s for a
  meaningful fraction of execution.
- **None of the proposal's §5 correctness layers exist in this PoC.**
  No SingleStepTests coverage, no record/replay lockstep, no bare-metal
  interpreter-as-oracle layer, no acceptance-gate boot. The two gates
  here (retired-instruction parity, one-shot differential state check)
  are necessary but far short of what §5 requires before this core
  could be trusted on real guest code.
- **This says nothing about whole-machine speed.** Step 8
  (`docs/bus-fast-path-plan.md`) measured machine overhead -- the gap
  between a bare CPU-core loop and the same instructions running inside
  `machine-core` with real bus/device/hook costs -- as a separate
  25-40% (ratios 0.57-0.78, i.e. 22-43% overhead) on top of m68k-rs's
  own bare-interpreter numbers. Nothing in this PoC measures that
  overhead for `m68k-vp`; there is no `machine-core` integration for it
  at all yet. A faster bare CPU core does not by itself imply the same
  proportional speedup once bus/device overhead is added back in.
- **No exceptions, no supervisor mode, no MMU, no FPU, no cache
  instructions, no self-modifying-code invalidation, no JIT.** All
  explicitly out of scope for this PoC per the task brief, and none of
  them exist in `crates/m68k-vp` in any form, not even a stub.
- **11 synthetic microbenchmark kernels are not a representative guest
  workload.** They're the same kernels `docs/bus-fast-path-plan.md` step
  8 used, chosen to isolate CPU-core throughput on specific instruction
  shapes -- not a sample of what Kickstart, AROS or real applications
  actually execute. Finding 4 above (no real peephole opportunities in
  this set) is a direct consequence.
