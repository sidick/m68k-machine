# ADR 0001 — What the machine runs on at Phase 5

**Status:** open — deliberately deferred to Phase 4/5. Recorded now
because Phase 0 turned up evidence that changes the option set, and
updated now that the option A conversion this ADR weighs has actually
been done (see "Update: the conversion has landed" below). The decision
itself has **not** been made — that stays deferred, deliberately, to the
Phase 4 measurements.

**Context:** proposal §5, §13, §15; roadmap Phase 5.

---

## The question

The proposal assumes the endgame is bare metal: an x86 UEFI payload
first, then RK3588, then Pi 5, each a freestanding `no_std` Rust binary
with the machine core, the CPU core, and hand-written drivers for the
board's storage, input and display.

That assumption has one load-bearing dependency, flagged in proposal §15
and checked in Phase 0: the `m68k` crate must build `no_std`. It does
not (see `phase0-findings.md`). So the question is live: does this
project go bare metal at all, and if so, on what?

## What Phase 0 actually measured

Two findings, and the second corrects the first.

1. `m68k` 0.12.1 does not build for `aarch64-unknown-none`. Confirmed.

2. **The gap is small.** The initial error count (1,735) was cascading
   fallout from the missing `#![no_std]` attribute, not a measure of how
   much of the crate is entangled with `std`. The crate contains roughly
   30 references to `std` in total:

   | Kind | Sites | Difficulty |
   |---|---|---|
   | `std::mem::take`, `std::f64::consts::*`, `std::fmt`, `std::cell`, `std::hash`, `std::sync::atomic` | most of the ~30 | mechanical — all exist in `core` |
   | `std::collections::VecDeque` (`core/decode.rs`) | 1 | test-only |
   | `std::sync::OnceLock` (`fpu/dd.rs`, `core/timing_060.rs`) | 2 | needs a `no_std` once-cell or a `const` table |
   | `std::env::var_os` diagnostic hook (`core/cpu.rs`) | 1 | `cfg`-gate it |
   | `f64::{sin,cos,sqrt,powf,floor,ceil,round,trunc,sinh,cosh,tanh,atan,log10}` in the FPU | ~20 calls | needs `libm`; not in `core` |
   | `std::sync::{Arc,Mutex}` in `core/trace_jit.rs` | several | JIT only, already `std`-only and feature-gated |

   Converting the interpreter (not the JIT) to `no_std` + `alloc` +
   `libm` is on the order of a day or two of mechanical work. It is
   upstreamable, and Copperline and the planned vamos successor would
   benefit from the same change.

This matters because it reframes the decision. "Bare metal" is not
blocked by a wall; it is blocked by a chore. The choice below is
therefore about **which set of drivers this project wants to own**, not
about whether the CPU core can be made to link.

## Update: the conversion has landed

The chore sized above is done. The project owner wrote a `no_std` fork
of `m68k`, pinned by `rev` in `[workspace.dependencies]` (not
upstreamed — see `phase0-findings.md`'s "Resolution" section for that as
a maintenance consideration in its own right), and `board-qemu-virt` now
instantiates a real `CpuCore` and steps a guest instruction
(`MOVEQ #42,D0` plus two `NOP`s) on bare metal under QEMU on
`aarch64-unknown-none` — the first guest instruction this project has
run outside a hosted process.

This does not decide the ADR; it changes what "Option A" now costs.
Before this, option A's cost was "the conversion chore, plus every
driver in Phase 5." The conversion chore is now paid, a guest
instruction has executed, and what remains for option A is **only** the
per-board driver work (§13's UART, MMC/NVMe, GIC, PCIe root complex,
xHCI for USB HID, and a display path per board) — there is no longer a
CPU-core blocker sitting in front of it. That strengthens option A
relative to option B without settling the choice: the Phase 4
measurements this ADR is deferred to are about interpreter throughput
and driver-ownership cost, neither of which this change speaks to.

## Options

### A. Convert `m68k` to `no_std`, keep the bare-metal plan

The proposal as written. **The conversion chore is done** (see "Update"
above) — a fork exists, is pinned, and has run a guest instruction on
bare metal. What remains of this option's cost is every driver in Phase
5 — UART, MMC/NVMe, GIC, PCIe root complex, xHCI for USB HID, and a
display path per board — plus the standing cost of depending on an
unupstreamed personal fork (manual porting of any future upstream fix,
single point of maintenance).

Keeps: instant boot, no host OS, a single self-contained artifact, and
the Emu68-variant door (§5.3) fully open.

One cost this ADR did not originally record, raised by ADR 0002:
**runtime display modesetting** — though weaker than first stated, and
the correction is recorded here rather than quietly dropped.

The original claim was that UEFI fixes the mode at `ExitBootServices`,
so a bare-metal layer must advertise the boot-time mode and refuse
changes. That holds only if the payload exits, and this project's q35
board is a UEFI application that does not: GOP's `SetMode` is available
to it at runtime. So on x86 the gap against a Linux host's DRM/KMS is
much smaller than claimed, and on ARM it is a different question
entirely, since RK3588 does not boot UEFI and has no GOP at all.

What survives is narrower: exiting boot services, which a long-running
bare-metal payload normally wants in order to own the memory map and
silence the firmware's timer and watchdog, costs `SetMode` along with
it. That is a real trade against option A, but it is a choice about when
to exit rather than a property of bare metal.
Whether the guest can change resolution without restarting the machine
is user-visible behaviour rather than an implementation detail, and it
falls out differently under A than under B. See ADR 0002's "Mode
setting, and what it means for ADR 0001".

### B. Linux-as-firmware — the Amithlon model

A stripped kernel plus an initramfs holding the machine binary as PID 1.
The machine stays an ordinary hosted `std` program forever; `no_std` is
never needed anywhere except `machine-core`, which is already
dependency-free and builds for bare-metal targets regardless.

This is the precedent the proposal already cites: Amithlon used a minimal
Linux as its hardware layer (§3), and PiStorm-classic runs its emulator
in Linux userspace, with Emu68 as the bare-metal alternative.

**What it deletes.** Most of Phase 5. The RK3588 "U-Boot-derived
UART/MMC/GIC/PCIe-RC bring-up" and the x86 "NVMe/AHCI/xHCI/HDA one driver
each from the Redox quarry" both stop being this project's work. It also
collapses Phase 4: running the machine on real hardware stops needing
KVM, because the machine is just a process on the board. Cranelift (§5.2)
becomes available on every target immediately, rather than being
late-roadmap pending alloc and W^X plumbing in the board layers.

**What it costs.** The pitch changes: "no host OS" becomes "a kernel you
never interact with", and "instant boot" becomes roughly a second. A
kernel config per board becomes a maintained artifact. Licensing stays
clean — the machine binary is userspace and not a derived work of the
kernel, so §16's MIT/Apache-2.0 layering is unaffected.

**What survives unchanged.** The board-layer trait boundary (§4). Under
Linux the backends are DRM/KMS or fbdev for the display surface, evdev
for input, `O_DIRECT` on a file or block device for MIRAGE, and tap or a
raw socket for SANA-II. Same traits, different implementations — and the
m68k side sees no difference at all.

### C. A Rust unikernel with `std` — Hermit

`x86_64-unknown-hermit` and `aarch64-unknown-hermit` are tier-3 Rust
targets that provide `std`, so the machine keeps the boots-directly story
while remaining an ordinary `std` program.

Weak where it would be needed most: the driver set is essentially virtio
plus limited PCI, aarch64 is the less mature half, and bring-up on
RK3588 or Pi 5 hardware is unlikely to exist. Plausible under QEMU/KVM,
thin on metal. **Unverified** — the tier-3 `std` support claim should be
checked against the current toolchain before this option is taken
seriously.

### D. A libc shim under a custom target (relibc, newlib)

Rejected: more work than option A's conversion chore, for the same
outcome.

## Recommendation

**Still open — deliberately not decided here.** Option B buys roughly a
phase of driver work and a much shorter path to a machine that is
genuinely usable on the Rock 5B, at the cost of a project claim that is
partly marketing. Option A is the honest choice if "no host OS" is the
point of the exercise rather than a nice property of it — and its
conversion chore, which Phase 0 first sized as the deciding cost, is no
longer a cost at all: it is done, and a guest instruction has run on
bare metal because of it (see "Update" above). What option A now costs
is per-board driver work and standing fork maintenance, not a CPU-core
blocker — which makes it more attractive than it was when this ADR was
first written, without being a reason to skip the Phase 4 measurements
this decision is deferred to.

The two are not mutually exclusive over time: B ships, A follows for the
targets that justify it, and the m68k side cannot tell the difference.

## Why this is deferred

Phases 0–4 are byte-identical under every option: a hosted binary, first
under QEMU, then on real hardware. Nothing in the current roadmap depends
on the answer until Phase 5.

By then the Phase 4 interpreter measurements on the Rock 5B will be in
hand — the same data that decides the Emu68-variant question (§5.3). Both
decisions want the same numbers, so both should be made at the same
point, with evidence rather than assumption. That is the pattern the
roadmap already applies to the CPU question ("answered with measurements
at Phase 4, not assumptions at Phase 0"); this ADR extends it to the
platform question.

## Decision triggers

- Phase 4 exit: interpreter throughput on RK3588 under the chosen host.
- Whether `no_std` support lands upstream in `m68k` — **not yet
  resolved**: the project carries its own fork (rev-pinned, not
  upstreamed) rather than waiting on this, so option A no longer needs
  this trigger to fire, but an upstream merge would still retire the
  fork-maintenance cost this ADR now records against option A.
- Whether USB HID and NVMe on ARM bare metal look like work this project
  wants to own, once Phase 4 has shown what the rest of the stack costs.
