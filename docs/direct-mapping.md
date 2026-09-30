# Direct address-space mapping (C1, final slice)

**Status:** Implemented on the hosted backend; on by default in
release builds, off by default in debug builds (see "Default policy"
below -- the split is measured, deliberate and loud, not an accident).
`docs/cpu-core-proposal.md` §4.6's direct mapping, page-type table and
open-bus pages, plus the fault-cost measurement §4.6 defers to C1 --
measured on **macOS**, not the Linux host §4.6 names; see "The
fault-cost measurement" below for why that distinction is kept loud.
This is the last of C1's sub-slices (`docs/cpu-core-trait.md`,
`docs/deterministic-mode.md` are the first two); the honest C1 exit
accounting is at the end of this document.

**Where:** `crates/machine-hosted/src/directmap.rs` (the map),
`crates/machine-hosted/src/bus.rs` (the access rules and the
differential oracle test), `crates/machine-hosted/src/run.rs` (RAM
allocation and construction), `crates/machine-hosted/src/bin/
fault_cost.rs` (the measurement harness). `--direct-map {auto,on,off}`
selects it; `auto` is the default.

## Why `machine-hosted`, not `machine-core`

Nothing of this belongs in `machine-core`, and nothing was added there
(`git diff` over this slice touches no `machine-core` file, so
CLAUDE.md's QEMU board gate does not apply -- stated explicitly rather
than silently skipped). A direct map is host-virtual-memory machinery:
`mmap`, `mprotect`, `shm_open`, a 4 GiB reservation. `machine-core` is
`no_std` and dependency-free and must stay that way; more to the point,
the map needs nothing *from* it beyond surface that already exists --
`MachineBus::fast_ram_window()`, `bus.overlay()`, the `pub autoconfig`
chain's `placement(i)`, and the fixed architectural constants. Even the
"rebuild on placement change" trigger needed no `machine-core` hook
(see below). The bare-metal boards at C4 will build their equivalent
from page tables the board layer owns, per the proposal's board-layer
contract (§12); that is future work and shares no code with this.

## The map

One 4 GiB anonymous `PROT_NONE` reservation of host virtual address
space. A guest access at address `a` is served at
`reservation_base + a` when the page-type table says it may be. What
is mapped inside it:

- **Chip RAM** at `CHIP_RAM_BASE`, read-write, aliased from the same
  shared-memory object `MachineBus` borrows (below). Mapped once at
  construction; chip RAM never moves.
- **The Kickstart copy** at `ROM_BASE`, `memcpy`ed in and
  `mprotect`ed read-only. §4.6's "Kickstart is copied into RAM at boot
  and executed from the mapped copy, so ROM takes the fast path". Typed
  `Rom` only when the image exactly fills `ROM_WINDOW_SIZE`; an
  undersized image needs `rom::read_mirrored`'s wraparound, so its
  pages stay `Io` and `MachineBus` serves them.
- **Fast RAM** at whatever base AUTOCONFIG assigns it, aliased in by
  `sync()` the moment `MachineBus::fast_ram_window()` first reports a
  placement -- and remapped, or unmapped back to `PROT_NONE`, whenever
  that placement changes.
- Everything else stays `PROT_NONE` and is never dereferenced by the
  direct path.

### Built from AUTOCONFIG placements, never constants

The project's standing rule (`docs/device-ledger.md`), extended to the
mapping layer exactly as §4.6 requires. Fixed architectural ranges
(chip RAM, ROM window, CIA space, the custom-chip page, the AUTOCONFIG
configuration window, the ext-ROM window) come from `machine_core`'s
own constants -- the same posture `bus.rs`'s pre-existing
`BUS_COVERAGE` classifier takes, and correct because those ranges are
silicon, not placement. Every placed board comes from iterating
`bus.autoconfig.placement(i)`; the board whose window matches
`bus.fast_ram_window()` is fast RAM, every other placed board is `Io`.
No code in this slice compares a guest address against a constant for
anything AUTOCONFIG can move (audited explicitly at review, including
the tests: the differential test's `FAST_BASE` is the address the
*test itself assigns* by performing the guest's configuration-window
writes, i.e. the test acting as `expansion.library`, not an assumption
about where a board lands). The audit is re-runnable, not a one-time
claim:

```sh
grep -nE '0x[0-9A-Fa-f]{6,}' \
  crates/machine-hosted/src/directmap.rs \
  crates/machine-hosted/src/bus.rs \
  crates/machine-hosted/src/run.rs \
  crates/machine-hosted/src/cli.rs \
  crates/machine-hosted/src/bin/fault_cost.rs
```

Every hit must be one of: a `machine_core` architectural constant
(chip RAM, ROM window, CIA, custom page, the AUTOCONFIG configuration
window, ext-ROM -- silicon, not placement), an address a *test*
assigns by performing the guest's own configuration-window writes, the
CIA-A PRA register a test pokes to clear the overlay, `0xFFFF_FFFF`
wraparound arithmetic, or (in `fault_cost.rs`) a host-side RNG seed --
that harness contains no guest addresses at all. At review the hits
were exactly those and nothing else; any new hit outside those
classes is a regression against `docs/device-ledger.md`'s standing
rule.

### The RAM backings: shared memory, two views

`MachineBus` borrows its RAM buffers `&'a mut` from the caller and that
API does not change. So when the direct map is enabled, `run.rs`
allocates chip RAM and fast RAM as anonymous POSIX shared-memory
objects (`shm_open` with a random name, `shm_unlink`ed immediately,
`ftruncate`d, `ShmRegion` in `directmap.rs`): the *primary*
`MAP_SHARED` view is what `MachineBus` borrows, exactly as it borrowed
the old `Box`/`Vec`, and the *same fd* is mapped `MAP_SHARED|MAP_FIXED`
a second time into the reservation at the guest base. Both views are
the same physical pages, so a blitter or hostblk-DMA write through
`MachineBus` is visible through the reservation immediately, and vice
versa -- `MAP_SHARED`'s own guarantee, nothing this module arranges.

The safety argument for that aliasing (single-threaded; the two views
are never both touched within one logical access, since each `Bus`
adapter method picks exactly one path per call; raw-pointer unaligned
accesses on the reservation side, never a second Rust `&mut`) is
written out in `directmap.rs`'s module docs. It is the same shape as
`bus.rs`'s pre-existing `fast_mem()` `SAFETY` comment for `FastMem`'s
raw pointer, with two mappings instead of one.

## The page-type table

`RAM / ROM / I/O / open bus`, one byte per 64 KiB page, 65536 entries
(`Box<[PageType; 65536]>`, 64 KiB total -- L2-resident). Consulted
inline on every access: §4.6's "one load and a compare". The cost was
measured rather than asserted -- see variant 1 below: on this host the
check's marginal cost against a raw read is below measurement noise.

**Invariant:** a page typed `Ram` or `Rom` is always mapped and
accessible in the reservation; `Io` and `OpenBus` pages are never
dereferenced. Review found (and fixed, with a regression test first
shown to fail against the unfixed code) one violation: fast RAM's
`Ram` typing was originally derived from the *declared* window rather
than the *actually mapped* alias, so either degradation path -- fast
RAM's shm region failing to allocate while chip RAM's succeeded, or
the `mmap` of the alias itself failing -- would have typed unmapped
pages `Ram` and made the first guest access to fast RAM a fatal
`PROT_NONE` dereference instead of a fallthrough. `rebuild()` now
types `Ram` only from `DirectMap::fast_alias`, the range `sync()`
actually mapped (and `sync()` remaps that alias *before* rebuilding,
so it is always current). The same declared-versus-mapped mismatch
cannot arise elsewhere: chip RAM's and ROM's mappings are made in
`DirectMap::new` and any failure there abandons construction entirely
(map disabled, `MachineBus`-only operation), and every other region is
`Io`/`OpenBus`, which the direct path never dereferences. Fast RAM was
uniquely exposed because it is the one region mapped *after*
construction, on a guest-controlled event.

### Access rules (`bus.rs`)

Per access, classify the page of the first and the last byte
(`checked_add`; overflow falls through -- `MachineBus` already handles
wraparound):

| first/last page types | read | write |
|---|---|---|
| both `Ram` | reservation, unaligned load, `from_be_bytes` | reservation, `to_be_bytes`, unaligned store |
| `Ram`/`Rom` mix or both `Rom` | reservation | fall through (ROM-write discard is `MachineBus`'s) |
| both `OpenBus` | `!0` of the width, inline | swallowed, inline |
| anything else (`Io`, mixed) | fall through to `MachineBus` | fall through to `MachineBus` |

A width-straddling access across two adjacent `Ram` regions (say chip
RAM into Z2 fast RAM at its placed base) is correct by construction:
both regions sit at their guest addresses inside one reservation, so
the straddling load reads exactly the bytes `MachineBus`'s byte-granular
fallback would compose. A straddle where the types differ falls
through, preserving `MachineBus`'s per-byte semantics (e.g. a word
write half in RAM, half on open bus writes one byte and discards one).

### Open bus, exactly preserved

Unmapped reads return `$FF` per byte (`$FFFF`/`$FFFFFFFF` for
word/long) and writes are swallowed -- `machine-core`'s own open-bus
rule, load-bearing for Kickstart's Ramsey/Gary/Gayle probes, served
inline from the table without touching `MachineBus`. §4.6's *mapping*
trick for open-bus reads (a shared read-only page of `$FF` mapped
across open-bus pages, so even a JIT's direct load succeeds without
faulting) is **not** wired into the live map in this slice -- the
interpreter consults the table first, so those pages never fault and
the mapping would be dead weight; C1 keeps them `PROT_NONE`. The
technique itself was validated on this host (variant 4 of the
harness: one 64 KiB `$FF` shm object mapped read-only at several
aliases inside a `PROT_NONE` reservation reads `$FF` everywhere and
faults on write), so C2/C3 can adopt it with the mechanics already
proven. §4.6's "one mapping cannot serve both" split (reads from the
shared page, writes fault/patch) is likewise a JIT-era concern that
the inline table renders moot for the interpreter.

### Rebuild on placement change

`DirectMap::sync(&MachineBus)` compares a small fingerprint -- the
overlay flag, all `MAX_BOARDS` placements, the clamped fast-RAM window
-- and on any change unmaps/remaps the fast-RAM alias and does a
**full** table rebuild (a 64 KiB fill plus a few range passes; it
happens a handful of times during boot, so simplicity beats
incremental bookkeeping). The trigger: `sync` runs once at
construction and then after every *write* the adapter routes to
`MachineBus`. That catches every transition with no missed window,
because nothing else can move a placement: AUTOCONFIG placement and
the CIA-A overlay bit change only via guest writes to `Io` pages
(configuration-window and CIA writes are always slow-path), reads
cannot reconfigure the chain, RAM-page direct writes cannot either,
and device ticks never re-place boards. The overlay transition is the
one non-AUTOCONFIG map change and rides the same fingerprint: while
OVL is asserted the low 512 KiB of chip RAM is typed `Io` (reads
mirror ROM, writes land in chip RAM -- `MachineBus` handles both),
and the CIA write that clears it retypes those eight pages `Ram`.

### Default policy (`--direct-map auto`)

`auto` resolves to **on in release builds, off in debug builds**, and
`run.rs` prints a `direct-map: on/off (...)` diag line naming the
decision and its reason on every run, so the profile split is never
silent. The debug half is measured, not assumed: at opt-level 0 the
map's per-access dispatch (the `Option`/`ReadOutcome` plumbing in
front of every bus access) never inlines, and the map becomes a pure
pessimization -- the full debug-profile `--ignored` suite run that
first included it failed exactly one test,
`max_cpu_speed_boots_to_workbench_and_keeps_real_time_across_wait_5`,
whose MARK1->MARK2 wall-clock gap stretched from 5.109s (map off,
debug) to ~5.54s (map on, debug) against a 5.0s +/- 0.5s bound; the
same gap is 5.100s in release with the map on or off, and
`#[inline(always)]` on the map's helpers recovered nothing (the cost
is the outlined call layers themselves). Rather than loosen a
pre-existing gate or ship a debug pessimization, `auto` keeps debug
builds on the pre-direct-map path. The debug suite still exercises
the map end to end: `fixed_mode_boot_is_identical_with_direct_map_
forced_on` (`tests/real_rom.rs`) boots the HD image under `--direct-map
on` and `off` and asserts byte-identical narration apart from the diag
line itself.

### When the map is off

`--direct-map off`; `auto` under a debug build (above); any
construction failure (falls back with an
`eprintln`, never a panic -- and hostile guest addresses are handled
with checked arithmetic throughout, per the register-file idiom's
fail-closed rule); or automatically whenever an access-intercepting
diagnostic is active (`SERIAL_REG_TRACE`, `--blitter-trace`,
`BUS_COVERAGE` -- same posture `fast_mem()` already takes) or
`--cpu-backend batch` is selected -- these veto even `--direct-map
on`, and the diag line says so when they do. Batch's `FastMem` window over the
primary view and the map's alias view are in principle coherent (same
shared pages), but no gate exercises the combination, so the
conservative posture keeps `batch` byte-identical to its
pre-direct-map behaviour.

## Evidence

- **Differential oracle** (`bus.rs`, `direct_map_differential`): two
  identical machines, direct map on and off, driven through the same
  reads/writes over every region and edge (chip start/end, overlay on
  *and* off, fast base/end/clamp edge and past-the-clamp, ROM ends,
  CIA, custom page, AUTOCONFIG window, deep open bus, `u32`
  wraparound), asserting byte-identical results. Shown to *fail*
  against two seeded bugs (open-bus reads forced to 0; overlay retype
  skipped -- the second initially slipped past it, which is why the
  overlay-ON sweep exists) and against the fast-RAM invariant bug
  above, then pass with the seeds removed. A test never shown to fail
  is not evidence; these were.
- **Unit tests** (`directmap.rs`): classification from a real
  `MachineBus` configured by guest-style config-window writes, overlay
  retyping, clamped-window tails, the degradation paths.
- **Reference figures, all exact and unmoved** (release, serial):
  `fixed` mode 4400 frames = 41,871,237 instructions, final PC
  `0x00f8131c` (identical with the map forced off); `cycle` mode
  41,894,978; the full `--ignored` real-ROM suite passes with the map
  on, including the exact-pixel screenshot gates (13,507 / 15,241 /
  14,073).
- **Speed** (release, serial, back-to-back A/B on the same host and
  session; `--cpu-speed max --cpu-backend interp`, 4400 frames): busy
  MIPS **34.27 / 34.76** with the direct map, **30.77 / 30.86**
  without -- **~+12%**. (The same-day OFF figure sits ~0.8 below the
  step-7.2 record of 31.65; the controlled comparison is the
  same-session A/B, and the documented baselines were not
  re-measured into this table.) The direct map thereby subsumes the
  RAM fast path's role on the hosted interpreter as §12 anticipated,
  though `MachineBus`'s own fast path remains for the boards and for
  `--direct-map off` and for debug builds.

## The fault-cost measurement

§4.6: *"Fault cost on the Linux backend is measured at C1 before this
split is committed to"* -- the split being fault-driven I/O with
one-fault-per-site patching versus the inline page-type check.

**This host is not that host.** These figures are from **macOS 26
(Darwin 25.6.0) on an Apple M3 Pro, via the `sigaction` signal path**
(the Mach-exception-port path was not measured). macOS signal delivery
is backed by Mach exception machinery and is generally *slower* than
Linux's direct `SIGSEGV` path, so treat every number below as a likely
**upper bound** for Linux, not a substitute. **The Linux measurement
§4.6 actually asks for remains outstanding** and belongs to whichever
milestone first runs on a Linux host. Do not record these numbers in
a Linux slot.

**Linux hand-off (reviewed 2026-09-29, C1 close-out).** What the
harness needs before it can be run on a Linux box or KVM guest,
recorded here so whoever has that host does not rediscover it:

- **The gate this bullet recommended now exists** (2026-09-30): the
  measurement lives in `src/bin/fault_cost/darwin_arm64.rs` behind
  `#[cfg(all(target_os = "macos", target_arch = "aarch64"))]`, and
  every other target builds a stub `main` that refuses to run and says
  why. The original claim here that the file "compiles on Linux as-is"
  was **wrong**, and CI proved it on the first push: the variant-3
  faulting store is an AArch64 `str` via `asm!` whose `{val:w}`
  template modifier does not exist on x86-64, so CI's Linux runner
  failed `cargo clippy -p machine-hosted` outright. The trap the old
  text worried about (running with wrong-layout Darwin structs on
  Linux) is now impossible rather than documented; the port work below
  is unchanged, and its landing spot is a sibling module behind the
  matching Linux cfg.
- The Linux/aarch64 port is small: unlike Apple targets, the `libc`
  crate *does* define `ucontext_t` for `linux`/`aarch64`, with an
  inline `uc_mcontext` whose `pc` field is directly assignable -- the
  PC-advance arm becomes
  `(*(ctx as *mut libc::ucontext_t)).uc_mcontext.pc += 4`, a
  cfg-gated helper of a few lines.
- Linux/x86-64 is more work, flagged rather than attempted: the PC
  lives at `uc_mcontext.gregs[libc::REG_RIP]`, and the fixed `+= 4`
  skip is AArch64-only -- x86's variable-length instructions mean the
  harness would need to know (or decode) the faulting store's length.
- Everything else in the harness is portable libc and needs no
  change: `sigaction`/`SA_SIGINFO`, `mmap`/`mprotect`, `MAP_ANON`,
  `sysconf(_SC_PAGESIZE)` (16 KiB pages here, 4 KiB typical on
  Linux -- the harness already reads it at runtime), and both SIGBUS
  and SIGSEGV are already installed with observed-signal reporting,
  so Linux delivering SIGSEGV where macOS delivers SIGBUS is
  reported, not a surprise.

Harness: `crates/machine-hosted/src/bin/fault_cost/` (gated as above); full run is
`cargo build --release -p machine-hosted --bin fault_cost &&
./target/release/fault_cost`, strictly serial. Measured 2026-09-29:

| measurement (macOS, M3 Pro) | result |
|---|---|
| inline page-type check, marginal cost vs raw read (200M iterations) | **below measurement noise** (measured -0.17 ns/access; i.e. ~0) |
| write-fault round trip, PC-advance service (the real I/O-service shape: fault, skip the `str`, never unprotect; 100k faults, none flaky) | **mean 1823.5 ns**, min 916, p50 1792, p99 2042 |
| write-fault round trip, mprotect-toggle service (fault, widen, retry, re-protect; 100k faults; loop-overhead-subtracted) | **mean 2876.4 ns** |
| signal delivered for a protection fault | `SIGBUS` (every variant, never `SIGSEGV`) |
| shared read-only `$FF` page technique | PASS (reads `$FF` at every alias, write faults) |

The two fault figures measure different things and are deliberately
not blended: **PC-advance is the shape §4.6's fault-patched I/O
actually takes** (service the access, skip the instruction, leave the
page protected) and is the number the split decision turns on;
mprotect-toggle is the naive service loop and costs ~1 µs more per
fault on this host. Caveats, recorded next to the numbers rather than
buried: variant 1's table is all-`Ram`, i.e. the branch-predictable
best case for the inline check -- a mixed table with mispredicted
types would cost more than ~0, though the 64 KiB table stays
cache-resident regardless; and the per-iteration timing overhead is
subtracted via a structurally identical no-fault baseline loop.

**What the numbers say for §4.6's split, on this host:** the inline
check is effectively free and one fault costs ~1.8 µs -- on the order
of *thousands* of inline checks (the ratio is unbounded within
measurement noise because the denominator is ~0). So on macOS the
interpreter's inline-table design is unambiguously right, and
fault-driven I/O is affordable only in the one-fault-per-site-then-
patch form §4.6 already prescribes for the JIT -- at ~1.8 µs a fault,
an unpatched I/O site in an interrupt handler polled every frame would
alone cost ~90 µs/s, and chipset-register-heavy code would be far
worse. Whether Linux's cheaper fault path shifts that balance is
exactly what the outstanding Linux measurement is for; nothing here
commits the split either way, which is all §4.6 asks of C1.

## Deliberately not done in this slice

- **VRAM as `Ram`** (§4.6 lists it with the RAM regions): rtgboard and
  Graffity windows are typed `Io`. Mapping VRAM needs an audit of which
  sub-ranges are pure storage versus registers, and the renderer reads
  VRAM through `MachineBus`; deferred, with the fallthrough path
  costing only dispatch.
- **The shared `$FF` open-bus read mapping** in the live map (validated
  standalone, above; a JIT-era need).
- **Fault-driven I/O and site patching** (§4.6 defers the commitment to
  this measurement; the measurement is now recorded, the machinery is
  not built).
- **The Linux fault-cost measurement** (outstanding, above).
- **Bare-metal boards**: they keep their own bespoke `m68k` integration
  untouched; the board-layer direct map is C4's.
- **`--cpu-backend batch` with the map on** (disabled, above).
- Translation-cache invalidation on placement change (§4.6's "invalidate
  any translated code whose I/O-site decisions depended on them") --
  there is no translated code until C2/C3; `sync()`'s fingerprint is
  where that hook will hang.

## C1 exit, assessed honestly

**Superseded (2026-09-30):** this assessment was written at the direct-
mapping slice, when the replay recorder/player did not yet exist and the
gate conversion was partial. Both have since been finished --
`docs/replay-log.md` carries the current item-by-item C1 exit
assessment; the text below is kept as the record of where C1 stood at
this slice.

§8's C1 exit: *"m68k-rs runs through the trait in both timing modes,
producing replay logs; gates pass in the new deterministic mode; fault
cost recorded."*

- **m68k-rs through the trait, both timing modes** -- met
  (`docs/cpu-core-trait.md`): `cycle` and `max` both run through
  `GuestCpu`, and `fixed` besides.
- **"producing replay logs" -- NOT met.** Slice 1 built the replay
  *hooks* (`retired_instructions`, `queue_ipl_injection`, the bus
  conduit) and tested them in isolation; no recorder and no player
  exist, and `docs/cpu-core-trait.md`'s own supervisor correction
  records that a recording bus cannot be substituted without real work
  (`Bus` is concrete, not a trait). No replay log has ever been
  produced.
- **"gates pass in the new deterministic mode" -- partially met.** 13
  of ~27 gate categories were converted and pass under `fixed`,
  several to exact pixel counts; 4 combination gates were argued
  covered but not run; the CPUBench gate cannot convert by design
  (`docs/deterministic-mode.md`).
- **"fault cost recorded" -- met on the wrong host.** Recorded above,
  on macOS, clearly labelled; the Linux figure §4.6 names is
  outstanding.
- **Direct mapping, page-type table, open-bus pages** (C1's own body
  text) -- met, this document.

So C1 is **not complete**: the replay recorder/player and the Linux
fault measurement remain, and the deterministic-mode gate conversion
is partial. What this slice closes is the direct-mapping item in
full, on the hosted backend, live and default-on rather than as
unconsumed machinery.
