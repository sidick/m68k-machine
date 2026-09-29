# The `GuestCpu` trait (C1)

**Status:** First slice implemented (`docs/cpu-core-proposal.md` §8):
the swappable CPU trait and §5.3's replay hooks. §5.2's fixed-
instructions-per-line deterministic mode is now implemented too, as C1's
second slice -- see `docs/deterministic-mode.md` for that mode itself
(the run loop, the measured default, the re-baselined gates) and the
note inline below on what it settled about this document's own
speculation. Direct address-space mapping and the signal-fault cost
measurement (the rest of C1) are now implemented as C1's third slice --
see `docs/direct-mapping.md` for both (the fault cost is a **macOS**
figure; the Linux measurement §4.6 names is still outstanding), and its
closing section for the honest item-by-item C1 exit accounting.

**Where:** `crates/machine-hosted/src/cpu.rs`.

## Why `machine-hosted`, not `machine-core`

`machine-core` is `#![no_std]`, dependency-free in its normal build, and
allocator-free (`crates/machine-core/Cargo.toml`'s own comment: "the bus
does not need a CPU"). It links `m68k` only as a `dev-dependency`, for
tests. The bare-metal boards (`board-qemu-virt`, `board-qemu-q35`) each
depend on `m68k` directly and drive `m68k::CpuCore` with their own bespoke
`AddressBus` adapters and step loops written well before this trait
existed -- they do not go through `machine-hosted`'s `run.rs` run loops at
all, and this milestone does not touch them or their Cargo.toml
dependencies. Because `cargo test`/`clippy` for `machine-core` were run
and the boards' own crates were not rebuilt or edited (no `machine-core`
change, so the CLAUDE.md gate that requires the QEMU smoke scripts "if you
touch `machine-core` at all" does not apply here), those board smoke tests
were not re-run for this change.

The only code this milestone needs to be swappable *behind* is `run.rs`'s
two run loops (`run_guest`, `run_guest_max`), both `machine-hosted`-only
(`std`, and already coupled to that crate's own `Bus` adapter type). Given
that, putting the trait in `machine-core` would have meant either giving
that crate a real (non-dev) dependency on a CPU crate that nothing else in
it needs, entangling the boards' own separate integration with a trait
designed for a different pair of run loops, or defining the trait with no
caller in `machine-core` at all, speculatively. None of those is "a
refactor that introduces a seam, no behaviour change" -- so the trait
lives next to its only caller, in `machine-hosted`.

## The two traits, and why there are two

```rust
pub trait HookCpu {
    fn pc(&self) -> u32;
    fn ppc(&self) -> u32;
    fn dar(&self, index: usize) -> u32;
    fn int_mask(&self) -> u32;
    fn set_irq(&mut self, level: u8);
    fn take_aline_exception(&mut self, bus: &mut Bus) -> i32;
    fn take_fline_exception(&mut self, bus: &mut Bus) -> i32;
    fn take_trap_exception(&mut self, bus: &mut Bus, trap_num: u8) -> i32;
    fn take_bkpt_exception(&mut self, bus: &mut Bus) -> i32;
    fn take_illegal_exception(&mut self, bus: &mut Bus) -> i32;
}

pub trait GuestCpu: HookCpu {
    type Hook: HookCpu;

    fn new() -> Self;
    fn set_cpu_model(&mut self, model: CpuModel);
    fn reset(&mut self, bus: &mut Bus);
    fn sp(&self) -> u32;

    fn retired_instructions(&self) -> u64;
    fn queue_ipl_injection(&mut self, at_index: u64, level: u8);

    fn run_for_cycles_with_hook<F>(&mut self, bus: &mut Bus, cycle_budget: i32, hook: F) -> GuestBatchResult
    where F: FnMut(&mut Self::Hook, &mut Bus, i32) -> HookControl;
    fn run_for_cycles(&mut self, bus: &mut Bus, cycle_budget: i32) -> GuestBatchResult;

    fn run_batch_instructions(&mut self, bus: &mut Bus, max_instructions: u32) -> GuestBatchResult { .. } // optional, default panics
}
```

`run.rs`'s cycle-mode run loop calls `m68k::CpuCore::run_for_cycles_with_hook`
with a closure that is invoked once per retired instruction, and that
closure receives `&mut CpuCore` -- **not** `&mut` the outer CPU handle
`run_guest` holds, which m68k-rs's own API shape does not expose to the
hook. `M68kRsCore` (this crate's one `GuestCpu` implementation) wraps
`m68k::CpuCore` in a field rather than implementing `GuestCpu` on it
directly, because [`retired_instructions`] and [`queue_ipl_injection`]
need a running-total field the fork itself has no room for and (being
pinned by `rev`, never modified -- CLAUDE.md) never will: `m68k::CpuCore`
only ever reports a *per-batch* retired count. That means the type the
hook is called with (`m68k::CpuCore`) and the type `run_guest` holds
(`M68kRsCore`) are genuinely different types, so the hook's closure
parameter cannot simply be `&mut Self`.

`GuestCpu::Hook` is the associated type that resolves this: `run.rs`'s
generic functions (`run_guest<C: GuestCpu>`, `run_guest_max<C: GuestCpu>`)
hold `cpu: &mut C`, but the hook closure they pass to
`run_for_cycles_with_hook` is typed over `&mut C::Hook`. For `M68kRsCore`,
`Hook = m68k::CpuCore`. `HookCpu` is the trait `C::Hook` is bound by --
deliberately smaller than `GuestCpu`, since a hook only ever needs to read
the instruction that just retired, dispatch at most one exception
(`service_host_serial`'s `--trigger-illegal-after-frames` path, itself
only ever called from inside the hook), and set the IPL before the next
fetch. `GuestCpu: HookCpu` as a supertrait bound means the *outer* handle
`run_guest` holds between batches gets the same surface for free -- the
`AlineTrap`/`FlineTrap`/... match arms after a batch returns call the same
`take_*_exception` methods on `cpu: &mut C`, one definition serving both
call shapes.

A hypothetical second core with no such wrapper -- state and execution
logic on one type -- can set `type Hook = Self` and implement both traits
on that one type; nothing about the split is forced on it.

`HookControl`/`GuestExit`/`GuestBatchResult` are this trait's own types,
not `m68k::CycleBatchControl`/`CycleBatchExit`/`CycleBatchResult` reused.
The shapes are deliberately close (so `M68kRsCore`'s forwarding is a
straight `match`), but keeping them separate means nothing outside
`cpu.rs` needs to depend on `m68k`'s types to satisfy or call the trait --
a no_std C2 core does not need to reach for the fork's crate at all.

## Why the trait is a compile-time seam, not a `dyn` one

`run_guest`/`run_guest_max`/`max_chunk_boundary`/`service_host_serial` are
all `<C: GuestCpu>` generics, monomorphized at their one call site
(`M68kRsCore`) rather than driven through `Box<dyn GuestCpu>` or
`&mut dyn GuestCpu`. `run_for_cycles_with_hook` also has a generic `F`
parameter (the hook closure) and an associated type (`Self::Hook`) in its
signature, neither of which is object-safe, so a `dyn GuestCpu` was never
on the table without either dropping the hook's zero-cost inlining or
giving up the associated-type design above. Given the choice, static
generics were taken: the hot instruction-retirement path
(`run_for_cycles_with_hook`'s hook, called once per retired instruction)
is unaffected by the trait's existence after monomorphization -- there is
no vtable call anywhere in `cpu.rs` or the code it generates. The only
place C1 introduces any indirection at all is the one call from `run.rs`
into `cpu.run_for_cycles_with_hook(...)`/`cpu.run_for_cycles(...)` itself,
once per `RUN_BATCH_CYCLES`/chunk-sized batch -- and that call is a static,
monomorphized function call too, not a virtual one. See "Performance"
below for the measurement confirming this cost is not just theoretically
zero but actually unchanged.

If C2 or C3 ever need runtime CPU selection (e.g. an `--cpu-core` flag
choosing between m68k-rs and the new core at process start, rather than a
build-time choice), that would need `Box<dyn GuestCpu>`-shaped surgery on
top of this -- likely trimming the hook closure to a non-generic
`&mut dyn FnMut(...)` and giving `Hook` a concrete, non-generic shape (or
dropping the `Self::Hook` split by requiring `type Hook = Self`). Nothing
in this milestone needs that, so it is not built.

## How §5.3's replay log maps onto the trait

§5.3: "m68k-rs is the master and runs the real machine. It logs, keyed by
retired-instruction index: every IPL change; the value returned by every
I/O read [...]; and every device write into guest memory [...]. The new
core replays that log against its own memory [...] The log format and the
hook points it needs belong in the C1 CPU trait, not bolted on at C2: the
trait exposes retired-instruction count, a way to inject IPL at an index,
and an I/O callback both cores route through."

- **Retired-instruction count** -- `GuestCpu::retired_instructions`.
  Exact in both timing paths. Cycle mode's hook fires once per retired
  instruction (`M68kRsCore::run_for_cycles_with_hook` increments its
  `retired` field there, before the caller's own hook runs). Max mode's
  unhooked batches still report an *exact* per-batch retired count in
  `m68k::CycleBatchResult::instructions`/`BatchResult::instructions` (see
  the fork's own doc comment on those fields), which
  `run_for_cycles`/`run_batch_instructions` add to the running total after
  each call. So the counter is always correct at a batch boundary in both
  modes.

  **The one gap, raised rather than worked around:** a real replay run
  needs to apply an IPL change *at* a specific retired-instruction index,
  which in max mode's unhooked, unbounded-length batches can only be
  known *after* the batch that crosses it returns -- by which point the
  batch has already executed past that index using whatever IPL was in
  effect at batch entry. Cycle mode's hook fires every instruction, so
  exact mid-batch injection is trivial there (see below); max mode would
  need either to fall back to the hooked path during replay (defeating
  the reason max mode exists) or to size each unhooked batch to end
  exactly at the next queued injection's index (using
  `run_batch_instructions`'s `max_instructions` parameter, or shrinking
  `run_for_cycles`'s cycle budget to an estimate and iterating). This is
  a real design question for whoever builds C2's replay player, not
  something this milestone resolves.

  **Tested at C1's second slice (`docs/deterministic-mode.md`):** the
  speculation above -- that §5.2's fixed-instructions-per-line mode "may
  make it moot anyway" -- turned out to be true, but not for the reason
  it gives. `run_guest_fixed` does not close this gap by sizing an
  *unhooked* batch to end at a line boundary; it simply never needs an
  unhooked batch at all. Each line's `--instructions-per-line` quota is
  enforced by the same hooked `run_for_cycles_with_hook` path cycle mode
  already uses, so `queue_ipl_injection` gets the same exact,
  instruction-granular application in fixed mode that it already had in
  cycle mode (`queued_ipl_injection_applies_at_the_target_index`, this
  crate's own unit test, exercises the mechanism `M68kRsCore` wraps
  around *any* hooked caller, not one specific to a run loop). The
  max-mode gap this bullet describes is specific to `run_for_cycles`'s
  *unhooked* batches, which fixed mode's hooked design never uses in the
  first place -- so this gap is unaffected by fixed mode's existence one
  way or the other. It would only become relevant if a future
  performance-motivated fixed-mode implementation switched to
  `run_batch_instructions` (sized to end exactly at
  `--instructions-per-line`) to avoid the hook's per-instruction
  overhead; even then, exact mid-batch injection at an arbitrary
  retired-instruction index would still require the batch to also end at
  that index, not merely at the next line boundary, so the gap would
  still not be closed in general -- only line-aligned injection targets
  would benefit.

- **Inject an IPL change at an index** -- `GuestCpu::queue_ipl_injection`.
  Implemented and unit-tested on `M68kRsCore`
  (`queued_ipl_injection_applies_at_the_target_index` in `cpu.rs`): queues
  `(index, level)`, and the wrapped hook applies `set_irq(level)` the
  first time `retired >= index`, consuming the queue entry once. **Not**
  wired into `run.rs`'s live hook body -- the milestone's brief is
  explicit that recording/replaying is not required yet, only that the
  trait be able to carry it. Wiring it in for a real replay run would also
  have to solve an ordering problem this milestone does not: `run_guest`'s
  own hook unconditionally calls `cpu.set_irq(bus.0.pending_irq_level())`
  after the injection point in the same hook invocation, which would
  immediately overwrite an injected level with the live device state's
  IPL. A replaying core must stop deriving IPL from live device state
  entirely (there is no real chipset backing a replay run) and defer to
  the queue alone -- a run-mode branch this milestone does not add.

- **An I/O callback both cores route through** -- not a new method. Every
  `GuestCpu`/`HookCpu` method that can touch guest memory already takes
  `&mut Bus`, m68k-rs's own `AddressBus` adapter over `MachineBus`
  (`crates/machine-hosted/src/bus.rs`). That parameter *is* the conduit:
  a record/replay implementation substitutes a wrapping `Bus` that logs
  reads (master) or serves logged values and applies logged device writes
  at the logged index (replayer), without touching `cpu.rs` or either
  `GuestCpu` implementation's execution logic at all. This is why the
  trait does not define a separate callback type -- one already existed,
  and needing a second would have been exactly the kind of method that
  "only looks right for m68k-rs" the milestone brief warns against.

- **Device writes into guest memory at a logged index** -- also a `Bus`
  concern, not a `GuestCpu` one: replaying those is "write these bytes to
  this address," which any `Bus` implementation can do without CPU
  involvement. Nothing in `cpu.rs` needs to change for this part of §5.3
  either.

**Supervisor correction (added at review).** The two bullets above
overstate how ready the `Bus` seam is. `Bus` is a concrete struct
(`crates/machine-hosted/src/bus.rs`, `pub struct Bus<'a>`), not a trait,
and every `GuestCpu` method names it by that concrete type. So a
recording or replaying bus cannot simply be "substituted" for it: doing
that requires either (a) adding record/replay support *inside* `Bus`
itself, or (b) making `GuestCpu` generic over its bus type. Both are
real work, and (b) is the one that also unblocks moving this trait to
`machine-core` for the bare-metal boards at C4 -- a `no_std` core will
not be talking to `machine-hosted`'s `Bus`.

What the bullets get right is that no *new callback method* on
`GuestCpu` is warranted: routing I/O through the bus parameter is the
correct shape, and §5.3's requirement is satisfied in design. What is
not yet true is that it can be swapped in without touching anything.
Read those bullets as "the seam is in the right place", not "the seam
is ready to use".

None of the above is implemented as a working recorder or player --
per the milestone brief, that is deliberately not this slice's job. What
this slice delivers is that the trait's shape does not block it: the two
new methods exist and are tested in isolation, and every execution method
already routes through the one shared conduit the log-replay design
needs.

## What a second (C2) implementation would have to provide

- `HookCpu` and `GuestCpu` for its own CPU state type (or, if it keeps
  state and hook-visible surface on one type, `type Hook = Self` and one
  set of impls).
- `run_for_cycles_with_hook`'s hook contract: called once per retired
  instruction, in program order, with the option to stop early via
  `HookControl::Return`. A batch-oriented core (like a JIT) that cannot
  cheaply call out every instruction would need an interpreter-tier
  fallback for this path, the same way m68k-rs's own JIT (`run_batch`)
  does not implement it and instead only backs the optional
  `run_batch_instructions` method.
- `retired_instructions` as an exact, monotonically increasing counter
  across `reset` boundaries, and `queue_ipl_injection` with pending-index
  semantics matching `M68kRsCore`'s (apply once, at or after the target,
  consuming the entry) -- or better, if the core can offer *exact*
  mid-batch application even in an unhooked path, which would close the
  max-mode gap noted above.
- **Not required:** `run_batch_instructions` (m68k-rs's JIT-specific
  batch path, `--cpu-backend batch`) -- the default implementation panics,
  and only `M68kRsCore` overrides it. A C2 core with no JIT-shaped batch
  entry point simply never gets `--cpu-backend batch` selected against it
  (that CLI flag is validated against `--cpu-speed max` today; extending
  it to gate on which `GuestCpu` is active is a C2/C3-era CLI change, not
  something this milestone had reason to add).
- **Not required:** a disassembler. `--trace` decodes through
  `m68k::dasm::disassemble` directly, driven by `args.cpu` (the CLI's
  `CpuModel`) rather than through the `GuestCpu` instance -- disassembly
  is a property of the 68k instruction encoding, not of which core is
  executing it, so it never needed to be a trait method at all. A C2 core
  decoding the same instruction stream can reuse the same disassembler
  without this trait's involvement, or `--trace` simply stays unavailable
  against a core that does not want to pull in a disassembler.

## Performance

Concern from the milestone brief: "confirm there is no performance
regression from the indirection [...] If a virtual call per batch is
free, say so with numbers; if it costs something, report the cost."

There is no virtual call at all after this change -- see "Why the trait
is a compile-time seam" above; every `GuestCpu`/`HookCpu` call site is
monomorphized to `M68kRsCore`/`m68k::CpuCore` at compile time, and the
per-instruction hook closure is still generic (`F: FnMut(...)`, inlined),
exactly as it was before this change when `run_guest`'s hook closure was
passed straight to `m68k::CpuCore::run_for_cycles_with_hook`. The
functional difference this refactor makes to the compiled interp path is:
one extra `u64` increment and an `Option` check per retired instruction
(the `retired`/`pending_ipl` bookkeeping in `M68kRsCore::run_for_cycles_with_hook`'s
wrapped closure), and one small enum-to-enum `match` per batch/chunk (the
`CycleBatchExit`/`BatchExit` -> `GuestExit` conversion) instead of the
match arms operating directly on m68k-rs's own exit enum.

`scripts/bench-boot-max.sh --no-build --backend interp`, release build,
run alone on an otherwise idle machine after this change (built first,
then measured, per the milestone brief's contention warning), against the
step 7.2 baseline recorded in `docs/bus-fast-path-plan.md` (interp 31.65
busy MIPS, same ROM/HDF pair and machine):

| | baseline (step 7.2, pre-trait) | after `GuestCpu` (this change) |
|---|---|---|
| Boot-to-Workbench-ready (4400 frames), busy MIPS | 31.65 | **31.68** |
| Idle window (frames 4400-5400), busy MIPS | -- | 30.15 |
| Boot wall clock | -- | 87.859s (wall), 2.932s busy |
| `--cpu-speed max --cpu-backend interp`, `cargo test -p machine-hosted -- --ignored` | -- | 19/19 pass, 509.23s |

31.68 vs 31.65 busy MIPS is within run-to-run noise (0.1%), consistent
with "Why the trait is a compile-time seam" above: there is no vtable
call in the generated code, so there was nothing for the trait to cost.
The full `--ignored` real-ROM suite (19 tests, including the CPU-bound
`cpubench` kernel test) passed unchanged through the new trait path in
both cycle mode and max mode's `interp` backend.

## What is deliberately not in this trait yet

- No log recording or replaying -- see "How §5.3's replay log maps onto
  the trait" above.
- Fixed-instructions-per-line deterministic mode (§5.2) is no longer
  missing -- see `docs/deterministic-mode.md`. It needed no changes to
  this trait: `run_guest_fixed` is built entirely on the same
  `run_for_cycles_with_hook`/`retired_instructions`/`queue_ipl_injection`
  surface this slice already shipped.
- No direct address-space mapping / page-type table (§4.6) -- likewise a
  separate sub-slice, and orthogonal to this trait: direct mapping is a
  `Bus`-side optimization (how `Bus` resolves an address), not a
  CPU-trait concern.
- No signal-fault cost measurement.
- No bare-metal board integration. `board-qemu-virt`/`board-qemu-q35`
  still drive `m68k::CpuCore` directly through their own bespoke
  `AddressBus` adapters, untouched by this change. Bringing them onto
  `GuestCpu` (so a future C2/C3 core could be board-tested too) is future
  work and was out of scope here -- see "Why `machine-hosted`, not
  `machine-core`" above for why doing so now would have been premature.
