# Phase 0 findings

## m68k-rs `no_std` support — RESOLVED (see "Resolution" below)

The finding immediately below is kept as it was recorded: it is the
reason the architecture looks the way it does (§5.1's interpreter-first
stance, ADR-0001's Phase 5 fork), and the "how big is the gap, actually?"
sizing at the end of this section is what made the resolution cheap. It
no longer describes the project's present state — see "Resolution".

## m68k-rs `no_std` support: verified FALSE (0.12.1, checked 2026-09-04)

Proposal §15 and roadmap Phase 0 both flag the `m68k` crate's `no_std`
status as the load-bearing assumption to verify before building on it.
It does not hold today.

**Method.** Built the `m68k = "=0.12.1"` crate (from crates.io, source at
`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/m68k-0.12.1`) for
`aarch64-unknown-none`, the target the bare-metal board layers need.

**Result:** the build fails with **1,735 errors**. That number is
misleading and is corrected below — it is cascading fallout from the
missing `#![no_std]` attribute, not a measure of how entangled the crate
is with `std`. Causes:

- No `#![no_std]` attribute anywhere in the crate. The crate links `std`
  unconditionally; it is a hosted-only library as published.
- Unconditional `use std::...` imports in multiple modules, at least:
  - `src/core/decode.rs`
  - `src/core/timing_060.rs`
  - `src/fpu/dd.rs`
- These aren't behind a `std` feature flag that could be turned off; the
  crate has no such feature (`serde`, `jit`, and `trace-profile` are the
  only Cargo features it exposes, none of which touch this).

**Consequence.** `m68k` cannot be a build dependency of a `#![no_std]`
crate today. The interpreter (and by extension the whole CPU core) is
**hosted-only** until upstream adds `no_std` support (tracked upstream at
[github.com/benletchford/m68k-rs](https://github.com/benletchford/m68k-rs);
not yet raised as an issue by this project as of this writing).

**What this means for the roadmap:**

- Proposal §5.1/§15's mitigation — "interpreter bare-metal, JIT
  hosted-only initially" — needs revising: *both* the interpreter and the
  Cranelift JIT are hosted-only for now, not just the JIT. Bare-metal CPU
  execution (Phase 5, x86 UEFI / RK3588 / Pi 5) is blocked on upstream
  `no_std` work landing in `m68k`, or on the project carrying a fork/patch.
- This does not block Phases 0-4: QEMU and KVM board layers run hosted
  (under a normal OS on the host CPU, even though the *guest* board layer
  itself may eventually be bare-metal-shaped on ARM/x86 firmware — the
  relevant distinction is whether the *m68k-rs consumer binary* itself
  runs hosted). The Phase 5 exit criteria ("basically usable" on bare
  metal) will need this resolved or worked around by then.
- `machine-core` (this repo's `crates/machine-core`) is kept
  dependency-free and `#![no_std]` regardless, and does not import `m68k`
  in its normal build — only as a `dev-dependency` for its hosted test
  suite (`tests/hello_guest.rs`). This means `machine-core` itself already
  builds cleanly for `aarch64-unknown-none` today (verified,
  `cargo build -p machine-core --target aarch64-unknown-none`), so none of
  this blocks Phase 0's own exit criteria. It only blocks *combining*
  `machine-core` with an actual m68k-rs `CpuCore` outside a hosted
  process.

**Version pin.** `m68k = "=0.12.1"` (exact pin, not a caret range) in
`crates/machine-core/Cargo.toml`'s `[dev-dependencies]`. Rationale: single-
author crate (Ben Letchford), no `no_std` support yet, and this project
tracks the crate's guest-visible behaviour closely enough that an
unplanned version bump could silently change CPU semantics underneath the
test suite. Bumping the pin should be a deliberate, reviewed step, not an
automatic `cargo update`.

**Re-check trigger.** Re-run this check (`cargo build -p machine-core
--target aarch64-unknown-none` after temporarily adding `m68k` as a real
dependency) whenever the pinned version changes, or periodically against
the upstream `main` branch, to see whether `no_std` support has landed.

## m68k-rs `STOP` and interrupt-resume contract

Learned the hard way in Phase 1, when Kickstart appeared to halt after
2.3M instructions and was in fact idling correctly. Recorded here
because nothing in the crate's docs collects it in one place and the
failure it causes looks like a guest bug rather than a host one.

- `CpuCore::stop(new_sr)` calls `set_sr(new_sr)` — which sets
  `int_mask` from `sr & 0x0700` — then sets the stop bit. `cpu.int_mask`
  is public and pre-shifted into bits 8-10, and is the reliable way to
  tell a genuinely unwakeable `STOP` (mask `0x0700`, since this machine
  never raises level 7) from one waiting for its next interrupt.
- Every `run_for_cycles_with_hook` call services a newly serviceable
  interrupt **before its first fetch**, including waking an
  already-stopped core. That is the resume mechanism.
- **But while the core is stopped, the run loop returns
  `CycleBatchExit::Stopped` without calling the host hook at all.** The
  crate's own doc says the hook "is not called for surfaced traps, STOP,
  interrupt entry, or an already-stopped CPU". So a host that ticks its
  devices only from inside the hook freezes its entire machine the
  moment the guest executes `STOP` — the clock stops, so no interrupt
  can ever be raised, so the core never wakes. The host must tick the
  bus and resample `set_irq` itself, once per `Stopped` exit, before the
  next `run_for_cycles_with_hook` call.
- Relatedly, the hook does not auto-dispatch A-line/F-line/TRAP/BKPT/
  illegal traps; they surface to the host for HLE interception. A
  bus-accurate consumer must call the matching `take_*_exception` to
  deliver the real hardware exception.

### How big is the gap, actually? (measured, correcting the above)

The 1,735-error figure sizes the *symptom*, not the work. Counting actual
`std` references in the crate gives about 30, of which:

- the majority — `std::mem::take`, `std::f64::consts::*`, `std::fmt`,
  `std::cell`, `std::hash`, `std::sync::atomic` — exist verbatim in
  `core` and are a find-and-replace;
- one `std::collections::VecDeque` (`core/decode.rs`) is test-only;
- two `std::sync::OnceLock` uses (`fpu/dd.rs`, `core/timing_060.rs`) need
  a `no_std` once-cell or a `const` table;
- one `std::env::var_os` diagnostic hook (`core/cpu.rs`) needs
  `cfg`-gating;
- roughly 20 `f64` transcendental/rounding calls in the FPU
  (`sin`, `cos`, `sqrt`, `powf`, `floor`, `ceil`, `round`, `trunc`,
  `sinh`, `cosh`, `tanh`, `atan`, `log10`) are the one real chunk: these
  are not in `core` and need `libm`;
- the `std::sync::{Arc, Mutex}` uses are confined to `core/trace_jit.rs`,
  which is the Cranelift JIT — already `std`-only, already feature-gated,
  and out of scope for a `no_std` interpreter.

So converting the **interpreter** (not the JIT) to `no_std` + `alloc` +
`libm` is on the order of a day or two of mechanical work, and it is
upstreamable — Copperline and the planned vamos successor consume the
same crate.

**Consequence for the roadmap:** bare metal is gated by a chore, not a
wall. That changes the Phase 5 calculus enough to be worth recording
separately; see `adr-0001-bare-metal-vs-linux-host.md`, which weighs
doing this conversion against not needing it at all.

## Skeleton `AddressBus` / hello-world guest instruction

Delivered as `crates/machine-core`:

- `MachineBus` implements the proposal §6.1 skeleton memory map: 2 MB chip
  RAM (`$000000`-`$1FFFFF`, borrowed `&mut [u8; CHIP_RAM_SIZE]`), a 512 KB
  Kickstart ROM window (`$F80000`-`$FFFFFF`, borrowed `&[u8]`, mirrors if
  shorter), and open bus everywhere else (reads `$FF`/`$FFFF`/`$FFFFFFFF`,
  writes discarded).
- `cargo test -p machine-core` runs:
  - Unit tests for open-bus reads/writes at representative unmapped
    addresses (`$C00000` slow RAM, `$D80000` RTC, `$DE0000` Gary/Ramsey,
    `$DE1000` Gayle ID), chip RAM read/write roundtrip, ROM read, ROM
    write-is-discarded, and undersized-ROM mirroring.
  - A hosted integration test (`tests/hello_guest.rs`) that builds a real
    `m68k::CpuCore` (`CpuType::M68040`) over a `MachineBus`-backed adapter,
    resets it, and steps `MOVEQ #42,D0` followed by two `NOP`s, asserting
    `D0 == 42` and that `PC` advances correctly at each step.
- `cargo build -p machine-core --target aarch64-unknown-none` succeeds:
  `machine-core` itself is dependency-free `no_std` and compiles for the
  bare-metal target, independent of the `m68k` no_std question above.

## Resolution: the project now carries a `no_std` fork of `m68k`

The chore this document sized above ("bare metal is gated by a chore,
not a wall") is done. The project owner wrote a fork of `m68k` with
`no_std` support and it is adopted: declared once in
`[workspace.dependencies]`, **pinned by `rev`, not by branch** — a branch
moves underneath you, and this project already pins the published crate
exactly for the same reason (a CPU-semantics change should be a
deliberate, reviewed step, not something that arrives silently under the
test suite). No guest-visible behaviour differs from the published
crate: identical pass counts across `machine-core` and `machine-hosted`.

**The fork is not being upstreamed for now.** Say this plainly rather
than let it sit implicit: the project depends on a personal fork of a
single-author crate, on top of already being that crate's only known
bus-accurate `no_std` consumer. That is a real maintenance liability —
it means CPU-core fixes and upstream `m68k` improvements have to be
manually ported into the fork, and the fork itself has exactly one
maintainer. It is recorded here rather than glossed over because the
original finding's "single-author crate — pin versions, upstream fixes"
risk (proposal §15) has, if anything, gotten sharper, not gone away.

`board-qemu-virt` now instantiates a real `CpuCore` over the same ROM
overlay the hosted test uses, resets it, and steps `MOVEQ #42,D0` plus
two `NOP`s under QEMU on `aarch64-unknown-none` — the first guest
instruction this project has executed outside a hosted process.

**Two gotchas found doing the conversion, both general to every future
bare-metal board layer, not specific to this one:**

- **`no_std` + `alloc` still needs a registered `#[global_allocator]`.**
  `step`/`reset` never allocate at runtime, but the crate's decode tables
  reference `Vec`/`Box`, so linking it at all pulls in `alloc` and a
  bare-metal consumer must supply an allocator even though the hot path
  never touches it. A trivial bump allocator suffices.
- **`CpuCore::new()`'s large struct literal gets lowered to SIMD
  instructions despite involving no floating point.** The ~1.5 KB literal
  is large enough that LLVM emits NEON load/stores to initialise it. FP/
  SIMD traps by default at EL1 and EL2 on aarch64, and with no exception
  vector table installed the trap has nowhere to go: the payload stalls
  **silently**, indistinguishable from a hang, at a point that moves
  between debug and release builds (different codegen, different trap
  site). `_start` must enable `CPACR_EL1.FPEN` (and `CPTR_EL2.TFP` when
  started at EL2 — check `CurrentEL` rather than assume) before any code
  path can construct a `CpuCore`. x86 has an analogous requirement
  (`CR0.EM`/`CR0.MP`/`CR4.OSFXSR`) that `board-qemu-q35` will need when it
  reaches the same point.

**Reset-vector detail worth flagging for later phases:** m68k-rs's
`CpuCore::reset` reads the initial SSP/PC from absolute addresses
`$000000`/`$000004` via the bus. On a real Amiga those addresses are ROM
during the reset window only because Gary overlays ROM at `$000000` until
the first appropriate bus access switches it back to chip RAM. This
skeleton bus has no overlay logic yet (out of scope for Phase 0), so
`tests/hello_guest.rs` writes the reset vector directly into chip RAM at
address 0 rather than relying on an overlay. The Gary overlay mechanism
will need implementing in a later phase (Phase 1, "chipset registers") for
real ROM images that expect it.
