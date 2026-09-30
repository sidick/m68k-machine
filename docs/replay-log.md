# Record/replay lockstep: the `--record`/`--replay` log (C1 slice)

**Status:** Implemented. `docs/cpu-core-proposal.md` §5.3's record/replay
lockstep, built on the `GuestCpu` trait seam (`docs/cpu-core-trait.md`)
and `--cpu-speed fixed` (`docs/deterministic-mode.md`). This document is
the log format, the design decisions and why, and the evidence (including
the seeded-divergence tests' actual output) that it works.

## Scope: what this slice is, and is not

This is the **self-replay** harness: m68k-rs (`M68kRsCore`) is both master
and player, against its own log. §5.3's eventual use -- a *second* core
(the C2 IR interpreter, or a bare-metal lockstep against the interpreter)
replaying an m68k-rs master's log -- is not built here; nothing in this
design assumes a second core exists, but nothing here would need to
change for one either (see "How a second-core player would differ"
below). Also out of scope, all named explicitly rather than silently
absent:

- **Flag masking for architecturally-undefined bits** (§5.3: "Flag bits
  the 68k leaves architecturally undefined ... are masked per opcode").
  Not needed for self-replay -- the same core produces the same undefined
  bits on both sides by construction -- so this is left as a documented
  gap for whoever builds the C2 cross-core player, not implemented
  speculatively here.
- **Ring-bounded logs.** The writer is a plain streaming append; a very
  long recording produces a very long file. §5.3's own risk section
  anticipates "a ring with checkpointed RAM snapshots when a divergence
  window is known" as a later refinement once one is actually needed.
- **Bisection with RAM snapshots.** Comparison here is per-instruction
  (I/O accesses) and per-checkpoint-interval (registers) -- there is no
  binary-search-to-the-exact-instruction machinery, since the two
  cores/runs being compared are lockstepped at every I/O access already;
  bisection matters more once cross-core flag-masking noise makes some
  register mismatches expected and others real.
- **Cycle/max-mode recording.** `fixed` mode only -- see "Why fixed mode
  only" below.
- **Per-instruction/localized RAM comparison.** Checkpoints compare CPU
  registers only (D0-D7, A0-A7, PC, SR), per §5.3's own definition -- a
  divergence confined entirely to RAM content the guest CPU never reads
  back is invisible to a checkpoint. **This slice does now carry one
  whole-RAM comparison**, added after the first version of case (e)'s
  seeded test found the gap directly: the log's `End` record carries an
  FNV-1a digest of chip RAM and (when attached) fast RAM, recomputed and
  compared by the player at the same point (see "RAM digest" below). That
  closes the "never detected at all" gap this bullet originally described,
  but not the harder problem: the digest says *only* "the final states
  disagree," not where. Localizing a RAM-only divergence to the
  instruction that caused it -- what a real bisection-with-snapshots
  mechanism would give -- is still out of scope; see the "Bisection with
  RAM snapshots" bullet above, which this digest does not attempt to
  replace.

## The `Bus`-seam decision, and why it's the right one for this milestone

`docs/cpu-core-trait.md`'s "How §5.3's replay log maps onto the trait"
section, with the supervisor correction at its end, narrowed this to two
options: (a) add record/replay support *inside* `Bus`
(`crates/machine-hosted/src/bus.rs`, a concrete struct), or (b) make
`GuestCpu` generic over its bus type. (b) is real, load-bearing work for
a different reason -- it is what would let this trait move to
`machine-core` for the bare-metal boards at C4, since a `no_std` core
will never talk to `machine-hosted`'s own `Bus`. That is a board-era
refactor, not a C1 concern: this milestone's brief is explicit that the
seam goes inside `Bus`, and doing otherwise here would be solving a
problem (`GuestCpu` genericity) this milestone doesn't need solved to
ship a working recorder and player. So (a): two new construction-gated
optional fields, `Bus.6: Option<Recorder>` and `Bus.7: Option<Player>`,
the same idiom every other diagnostic/interceptor on `Bus` already uses
(`.1` the blitter trace, `.2` the serial register trace, `.4` bus
coverage, `.5` the direct map).

This is consistent with `cpu-core-trait.md`'s own finding that no new
`GuestCpu`/`HookCpu` callback was needed for the I/O conduit -- every
method that touches guest memory already takes `&mut Bus`, so wrapping
what `Bus` *does* with that access (log it, or serve it from a log) is
exactly the right level, and needed no trait change beyond one small
addition: `HookCpu::sr()`, since §5.3's checkpoints need the full status
register and nothing on the existing trait exposed it (`int_mask` covers
only the bits `run.rs` already inspects, not SR as a whole).

## Why `fixed` mode only

Two independent reasons, both from the milestone brief and both re-
confirmed while building this:

1. **Max mode's IPL-injection gap.** `docs/cpu-core-trait.md`'s "one gap"
   section: max mode's unhooked `run_for_cycles` batches can only resolve
   an IPL injection at a target index *after* the batch that crosses it
   returns, by which point the batch already executed past that index
   using whatever IPL was in effect at batch entry. `fixed` mode never
   uses an unhooked batch at all -- every instruction goes through the
   same hooked `run_for_cycles_with_hook` path `cycle` mode uses -- so
   this gap simply does not exist there.
2. **The device-write-span drain is only exact once per instruction.**
   `MachineBus::device_write_spans()` accumulates since the log was last
   cleared; draining it once per *instruction* (fixed mode's hook, called
   after every retired instruction) is what makes "which span belongs to
   which ordinal" unambiguous. `cycle` mode's hook fires once per
   instruction too, in principle, but this milestone chose not to also
   wire recording into `cycle` mode -- there is no reproducibility
   argument for doing so (cycle mode's own reproducibility comes from its
   cycle tables, not from this log), and `max` mode's coarser, wall-clock-
   paced device ticks would make a per-instruction drain misleading (many
   instructions' worth of device writes would land in one drain, with no
   way to attribute a span to the specific instruction whose access
   caused it). `run.rs` refuses `--record`/`--replay` outside
   `--cpu-speed fixed` with an explicit setup error naming both reasons.

## The ordinal

One `u64` counter, kept in lockstep by both the recorder and the player,
starting at 0. It is incremented by exactly one, as the *first* action,
at two points only:

- the top of every fixed-mode per-instruction hook invocation (so once
  per retired instruction), and
- the top of the `Stopped` arm's own per-line resync tick (so lines still
  advance -- and IPL can still change -- while the CPU is stopped, with
  no instruction retiring at all).

Nothing else bumps it. The subtlety that a real end-to-end run against a
real boot caught (see "A real bug, found and fixed" below) is this:
**not every event is logged at the just-bumped ordinal.** A bus access
(an I/O read/write) happens *during* the execution of the instruction
that is about to retire and trigger the *next* hook call -- i.e. it is
logged using whatever ordinal value is current *before* that next bump.
By contrast, everything the hook body itself does (checking the IPL,
draining device-write spans, taking a checkpoint) runs *after* its own
bump, at the new value. A write-triggered event -- specifically a
`ClassificationTransition`, since that is checked synchronously inside
`bus.rs`'s write path, in the middle of the write that caused it -- is
therefore logged at the *pre-bump* ordinal of the instruction that made
the write, not the post-bump ordinal of the hook call that will run once
that instruction finishes retiring.

Concretely, for the `k`-th hook call:

```
... instruction retires -> hook call k runs:
      ordinal := ordinal + 1        // now k
      tick, set_irq
      log IplChange (if changed)          @ ordinal k
      drain + log DeviceWrite spans       @ ordinal k
      log Checkpoint (if due)             @ ordinal k
... next instruction executes:
      each bus access it makes is logged/served @ ordinal k
      (a write to an Io address may also trigger, mid-access,
       a ClassificationTransition log entry -- @ ordinal k,
       the *not-yet-bumped* value, since the *next* hook call
       (k+1) that would bump it hasn't run yet)
```

Both the recorder and the player's own event consumer
(`Player::drain_side_effects`) therefore apply any `DeviceWrite`/
`ClassificationTransition`/`IplChange`/`Checkpoint` event whose ordinal is
**at or before** the current one (`<=`, not `==`) -- an `==`-only check
would leave the pre-bump-ordinal event permanently stuck in the queue,
since the ordinal only ever increases and never revisits that exact
value. I/O events (`IoRead`/`IoWrite`) are never at risk from this
relaxation: they are only ever consumed by
`Player::expect_io_read`/`expect_io_write`, directly from the bus-access
call site, and `drain_side_effects` stops the instant it peeks one, of
any ordinal.

## Classification

Both the recorder and the player classify addresses with exactly the same
rule `crate::directmap::DirectMap` uses for the direct address-space
mapping (`docs/cpu-core-proposal.md` §4.6): `PageType::{Ram, Rom, Io,
OpenBus}`, one byte per 64 KiB guest page. The table-building logic
(`DirectMap::rebuild`'s body) was factored out into a free function,
`directmap::build_page_table(bus, rom_is_full_window, fast_ram_mapped)`,
returning a fresh table rather than mutating one in place; `DirectMap`
itself is now a two-line wrapper around it, with no behaviour change (its
own differential and unit tests, unmodified, still pass). This is a
deliberate reuse, not a coincidence of similar needs: a page-type table is
exactly what both the direct map and the replay harness need to answer
"pass through, or intercept" for a given address, and building two
independent classifiers for the same job would have been the exact kind
of drift this project's `docs/device-ledger.md` standing rule (ask the
bus, never hardcode) exists to prevent.

One difference between the two callers, both documented on
`build_page_table` itself: `DirectMap` passes its own `fast_alias` (the
actually-mapped, 64 KiB-page-rounded mmap alias) as `fast_ram_mapped`,
since a raw pointer read past the real buffer would be undefined
behaviour on that path. The recorder/player have no mmap alias to round --
they serve `Ram` pages through `GuestMemory::ram_slice`/`ram_slice_mut`,
which already clip to the real buffer and fail closed (`None`) rather
than read out of bounds -- so they pass `bus.fast_ram_window()`'s bare
clamped extent directly, unrounded. The two produce identical tables
whenever the window's length is already a whole number of 64 KiB pages,
which is the case for every configuration this project boots.

**Neither the recorder nor the player uses `DirectMap` itself** (no mmap,
no 4 GiB reservation). The recorder rebuilds its own table straight from
live `MachineBus` state whenever a placement/overlay fingerprint changes
(`Recorder::maybe_transition`, called after every write `bus.rs` routes
to `MachineBus` -- the same trigger discipline `DirectMap::sync` already
uses) and logs a `ClassificationTransition` event carrying a full 64 KiB
table snapshot on each change. The player has no live `MachineBus`
AUTOCONFIG state to ask (see "Administrative writes" below for the one
exception) -- its table comes *purely* from these events, applied in log
order by `Player::drain_side_effects`.

`ClassificationTransition` events are **an addition this slice makes,
not something §5.3 itself lists.** §5.3 describes the recorder logging
"every IPL change; the value returned by every I/O read; and every
device write into guest memory" -- it does not mention that the
classification itself (which addresses count as I/O vs RAM vs ROM) can
change mid-boot and that the player needs to be told when it does. This
is recorded here as a finding, not retrofitted into the proposal
document (which this milestone's brief explicitly says not to edit):
without it, a player with no live devices has no way to learn that, say,
low chip RAM stopped being `Io` (ROM-shadowed reads) and started being
real `Ram` the moment the guest cleared the ROM overlay bit.

A full 64 KiB-page table snapshot per transition was kept rather than an
RLE-compressed delta: transitions are rare (three per boot in every
recording this slice produced -- overlay clear, and AUTOCONFIG placing
each of hostblk/fast RAM), so the simplicity of "just write the whole
table" costs at most a few hundred KB total per boot, nowhere near
dominating the log (see "Full-boot recording" below for the actual size
breakdown).

### Administrative writes: the one place the player *does* still write to its own `MachineBus`

An `Io`-classified write is, by design, compared against the log and then
discarded -- "no live devices" means there is nothing to apply it to.
Two ranges are an exception, found and fixed against a real boot
recording rather than reasoned out in advance (see "A real bug, found and
fixed" below for both):

- **Chip RAM while the overlay is mapped.** `classify` types this `Io`,
  not `Ram`, because *reads* there are redirected to ROM's shadow while
  the overlay is active -- but *writes* always land on the real chip-RAM
  cells underneath regardless of overlay (`machine_core::MachineBus`'s
  own module docs). Discarding a write here would leave those cells zero
  forever, even though the exact same addresses are later retyped `Ram`
  (once overlay clears) and served straight from `GuestMemory` -- silently
  wrong data the moment anything reads back what an overlay-era write
  stored (early boot's own stack and locals, in practice).
- **The AUTOCONFIG config window** (`machine_core::autoconfig::
  AUTOCONFIG_BASE..AUTOCONFIG_END`). The player's classification table
  comes entirely from the log, but `GuestMemory::ram_slice`/`ram_slice_mut`
  -- what serves every `Ram`-classified access -- still consults the
  player's *own* `MachineBus`'s AUTOCONFIG state (`fast_ram_window()`) to
  find fast RAM's placed base. If AUTOCONFIG's own config-space writes
  were discarded like every other `Io` write, the player's bus would
  never learn where fast RAM lives, and a `Ram`-classified fast-RAM access
  would find no backing region at all.

Both ranges are pure address-decode bookkeeping this crate's own code
needs to stay correct on both sides -- not "servicing a live device" in
the sense a CIA timer, a custom-chip register, or a hostblk/pktport
doorbell would be. Replaying the exact same, already-log-validated write
value is safe: it cannot re-trigger any device-side effect, because
neither range names an actual device.

## Recording

`Recorder` (`src/replay.rs`), constructed once `MachineBus` is fully
wired (every `.with_*` board attached), the same point `DirectMap::new`
is built at in a non-recording run. Per bus access, in `bus.rs`'s
`AddressBus` impl:

- **Ram/Rom/OpenBus:** pass through to `MachineBus` unlogged, exactly as
  a plain run would.
- **Io read** (including instruction fetches -- `read_immediate_word`/
  `read_immediate_long` route through the same logging as
  `read_word`/`read_long`, since DiagArea boot ROM code is fetched from
  board windows classified `Io`): perform the real read, then log
  `{ordinal, addr, width, value}`.
- **Io write:** log `{ordinal, addr, width, value}`, perform the write,
  then re-check the classification fingerprint
  (`Recorder::maybe_transition`) -- unconditionally after every write
  `Bus` routes to `MachineBus`, the same trigger `DirectMap::sync` uses,
  not only after writes to addresses that happen to be `Io`.

IPL: in the fixed-mode hook, right after `cpu.set_irq(bus.0.
pending_irq_level())`, `Recorder::log_ipl_if_changed` compares the level
to the last one logged and logs on change only. The same call happens in
the `Stopped` arm's own resync tick.

Device writes: `MachineBus::set_device_write_log` is handed a 65536-entry
`Vec<(u32, u32)>`, allocated once in `run.rs` only when `--record` is
given. After *every* `bus.0.tick(...)` call -- the per-instruction hook's
own tick and the `Stopped` arm's per-line tick -- `run.rs` drains
`device_write_spans()`, reads the current bytes for each span via
`GuestMemory::ram_slice`, logs one `DeviceWrite` event per span, then
clears the log. This drain is **exact only because fixed mode ticks once
per instruction** -- a cycle- or max-mode recorder (not built here) would
need a different attribution scheme, since a batch there can cover many
instructions' worth of device time before the host ever calls back in.
`MachineBus::device_write_log_overflowed()` is checked on every drain;
if set, the recording aborts immediately (`eprintln!` plus
`std::process::exit(4)`) rather than continuing with a silently-incomplete
log -- this was never observed to trigger on any recording in this
milestone's evidence (the 4400-frame reference boot's own device-write
count is well under the 65536-entry capacity between any two drains).

Note on units, since the two device-write sources are easy to conflate:
blitter spans are logged as **chip-RAM offsets**, while
`ram_slice_mut`-granted spans (hostblk/pktport/pcibridge DMA) are logged
as **guest addresses**. They coincide numerically only because chip RAM
sits at guest address 0 -- there is no separate "which kind of span is
this" tag in the log, and none is needed, since both are simply "write
these bytes at this guest address" from the player's point of view.

Checkpoints: every `REPLAY_CHECKPOINT_INTERVAL` ordinals (env-gated, same
"diagnostic/test knob, zero cost unless set" posture as
`MEASURE_INSTR_PER_LINE`/`SERIAL_REG_TRACE`; defaults to
`DEFAULT_CHECKPOINT_INTERVAL = 1_000_000`), the hook logs
`{ordinal, D0-D7, A0-A7, PC, SR}`. `HookCpu::sr()` (new, backed by
`m68k::CpuCore::get_sr()`) is what makes SR available here -- nothing
else in `run.rs` reads it today.

### RAM digest

Register-only checkpoints, by construction, cannot see a divergence that
never touches a register -- a corrupted or missing span of pure RAM
content (drawn by the blitter, say) that the guest CPU never reads back
is invisible to every mechanism described so far. The first version of
the seeded-divergence test for exactly this case (case (e), a dropped
blitter-span `DeviceWrite`) found precisely this gap empirically (see
"The seeded-divergence evidence" below for the full history) -- and named
a fix specific enough to build: **the log's `End` record now carries an
FNV-1a 64-bit digest of chip RAM's whole contents, and of fast RAM's
(when attached; `0` otherwise)**, computed by `Recorder::finish` from
`bus`'s real final RAM and recomputed by the player, from its own
replayed RAM, at the same point (`ram_digests`, one implementation shared
by both sides so they can never disagree about *how* a digest is
computed -- only whether the bytes underneath do). A mismatch is a new
divergence kind, `Divergence::RamDigestMismatch { region, expected,
actual }`, naming which region.

This bumped the log format version (1 -> 2): a version-1 log has no
digest fields and is refused by a version-2 build (`Player::open`'s
existing version check), rather than silently read with garbage digests.

**What this does and does not give you.** It is a real, whole-memory
equality check -- stronger than "every I/O access matched," since I/O
comparison alone says nothing about RAM the CPU only ever writes and
never reads back through it again. It closes the "never detected at all"
gap the first version of case (e) found. It does **not** localize: a
mismatch says the two final RAM states disagree, not where or when they
first diverged. Two consequences follow, both confirmed by testing (see
below): first, a corruption that *is* read back by the guest before the
log ends is still typically caught earlier, by an ordinary I/O or
checkpoint mismatch, since the digest is only ever checked once, at End --
the digest is a backstop, not the primary detector. Second, localizing a
digest-only divergence to the instruction that caused it needs the
bisection-with-checkpointed-RAM-snapshots mechanism this milestone
defers (`docs/cpu-core-proposal.md` §5.3's own risk section); this slice
gives you "somewhere in this run, RAM ended up different," not "at
ordinal N."

Header: magic (`M68KREPL`), format version (`2`), `--instructions-per-line`,
an FNV-1a 64-bit hash of the exact ROM bytes (not cryptographic -- an
identity check against replaying a log against a different ROM, not a
security boundary), chip/fast RAM sizes, and the initial 64 KiB-page
table. End record: final ordinal, total instructions, final
`{D0-D7, A0-A7, PC, SR}`, and the two RAM digests above.

Streaming: a buffered `BufWriter<File>`, one small fixed/TLV-tagged
record per event, written as it happens -- the log is never held in
memory.

## Replay

`Player` (`src/replay.rs`), driven by a new run loop, `run_guest_replay`
(`run.rs`) -- structurally a stripped `run_guest_fixed` with every live-
device concern removed:

- **No live device servicing at all.** `bus.0.tick` is never called,
  `pending_irq_level` is never called, no serial/input scripts run, no
  screenshots are taken. Everything the guest sees comes from the log or
  from direct `GuestMemory` access on the player's own RAM.
- **Same machine construction as the recording** otherwise (same ROM,
  same RAM sizes) -- validated against the log's header at startup: a ROM
  hash or chip-RAM-size mismatch is a setup error, not a silent replay
  against the wrong machine. (The fast-RAM-size check is honest about its
  own limit: at the point both the recorder and the player build their
  header/comparison value, AUTOCONFIG has not yet placed fast RAM on
  either side, so this specific check is currently a trivial `0 == 0`;
  a real fast-RAM-size mismatch is still caught downstream, the moment the
  two sides' classification tables diverge.)
- **Ram-classified access:** served via `GuestMemory::ram_slice`/
  `ram_slice_mut` directly -- never via `MachineBus::read_byte`/
  `write_byte`, whose overlay state is stale in replay (no CIA writes are
  ever applied on this side, "Administrative writes" above being the one
  documented exception).
- **Rom-classified access:** served straight from the player's own
  `MachineBus` (`bus.read_byte`/`word`/`long`) -- safe, since ROM reads
  have no overlay-dependent or other side effect.
- **OpenBus:** `0xFF` inline for reads, swallowed for writes.
- **Io read:** pop the next `IoRead` event; assert `{ordinal, addr,
  width}` match what the CPU actually requested. A mismatch is recorded
  as a [`Divergence`] (see below) and the logged value is still returned
  on a match (or a best-effort `0` on a mismatch, so execution can
  continue long enough for the outer loop to notice and report).
- **Io write:** pop the next `IoWrite` event, compare `{ordinal, addr,
  width, value}`; the write is discarded either way (never applied to a
  live device) except for the two administrative ranges above.
- **Device-write and classification-transition events** are applied by
  `Player::drain_side_effects`, called once per hook invocation /
  `Stopped`-tick right after the ordinal bump -- see "The ordinal" above
  for exactly which events that call picks up. IPL events are applied via
  `cpu.set_irq` at the same point, including while the CPU is stopped (the
  `Stopped` arm advances the ordinal and applies events exactly like the
  master's own resync tick does, just without ever calling into
  `MachineBus`).
- **Divergence reporting.** Since `AddressBus`'s trait methods return
  plain values with no `Result`, a mismatch discovered deep inside
  `bus.rs` is recorded onto the `Player` itself (`record_divergence`,
  sticky -- only the *first* divergence is ever kept) and drained by
  `run_guest_replay`'s outer loop right after the instruction/tick that
  triggered it, which is the earliest point outside the trait
  implementation that can act on it.
- **Clean finish:** at the log's `End` event, `Player::drain_side_effects`
  first recomputes and compares the whole-RAM digests ("RAM digest"
  above; `RamDigestMismatch` on a mismatch, checked before the register
  comparison so a digest problem is what's reported if both would fire),
  then `run_guest_replay` compares final registers once more
  (`FinalStateMismatch` on a mismatch); with no divergence anywhere, the
  run reports `Outcome::ReplayClean` (exit 0) with the total ordinals/
  instructions compared and event counts by kind. Any divergence reuses
  `Outcome::Wedged` (exit 2) with a report of the *first* mismatch:
  ordinal, kind, expected/actual, and PC (`Divergence`'s sticky "first
  only" contract, `Player::record_divergence`'s own doc comment -- a
  mid-run I/O mismatch, when one happens, is always reported instead of a
  later End-digest mismatch, never both).

### How a second-core player would differ

Nothing in `Player`'s own design assumes the replaying core is
`M68kRsCore`: `run_guest_replay<C: GuestCpu>` is already generic over the
CPU type, and every value it reads (`cpu.dar`, `cpu.pc`, `cpu.sr`,
`cpu.set_irq`) comes from the trait, not the concrete type. A C2 IR
interpreter substituted for `C` would need: (1) the flag-masking this
slice deliberately deferred, since an interpreter's choice of undefined-
bit values will differ from m68k-rs's, and a checkpoint comparing raw SR
bits would then report false divergences on exactly the cases §5.3 names
(post-`DIVU`/`DIVS` overflow, BCD instruction flags); (2) nothing else --
the log format, the ordinal scheme, and the classification table are all
core-agnostic already.

## A real bug, found and fixed

Two real bugs surfaced only once this harness was run against a real
486-frame HD boot recording, not against the unit tests (which use a
synthetic NOP ROM with no AUTOCONFIG, no overlay, and no device DMA) --
consistent with CLAUDE.md's own observation that "self-consistent unit
tests have repeatedly passed over real bugs" on this project.

**Bug 1: the `==`-only ordinal check on `drain_side_effects`.** The first
working version applied a pending `DeviceWrite`/`ClassificationTransition`
only when its ordinal exactly equalled the current one. The very first
real recording (a 30-frame boot) diverged within the first few hundred
thousand instructions: `expected an I/O event ... found a different
event kind`. The cause was exactly "The ordinal" section above describes:
a write-triggered `ClassificationTransition` is logged at the *pre-bump*
ordinal of the write's own instruction, which the post-bump `==` check at
the next hook call could never match again. **Fixed** by relaxing the
check to `<=` (apply everything at or before the current ordinal, stop at
the first I/O event of any ordinal) -- see "The ordinal" above for the
full reasoning and why this relaxation cannot accidentally consume an I/O
event out of order.

**Bug 2: discarding writes to two "administrative" `Io` ranges.**
With bug 1 fixed, the same recording progressed much further (past 1
million ordinals) before failing with `RAM access out of bounds ...
classified Ram but not backed by any attached RAM region` at address
`0x5000_0000` -- exactly fast RAM's configured end. The player's *table*
correctly said `Ram` there (from the recorder's own `ClassificationTransition`),
but the player's own `MachineBus` had never actually been told fast RAM
was placed, because the AUTOCONFIG config-space writes that would have
told it were `Io`-classified and (per the original, simpler design)
discarded like every other device write. **Fixed** by the "Administrative
writes" carve-out above: AUTOCONFIG-window and overlay-mapped-chip-RAM
writes are applied for real (in addition to being compared against the
log), since both are pure address-decode bookkeeping rather than a live
device's own state.

With both fixes, a 500-frame HD boot records and replays clean (see next
section).

## Evidence

### Self-replay, 500 frames

```
record: 38243966 ordinals, 203904 io-reads, 72868 io-writes, 12114 ipl-events,
        16348 device-writes, 3 classification-transitions, 38 checkpoints
PHASE1 HOSTED: LIMIT REACHED (max-frames) -- 38178510 instructions, 500 frames,
        overlay cleared, final PC 0x00f8131c

replay: 38243966 ordinals, 203904 io-reads consumed, 72868 io-writes consumed,
        12114 ipl-events, 16348 device-writes, 3 classification-transitions,
        38 checkpoints
PHASE1 HOSTED: REPLAY CLEAN (38243966 ordinals, 203904 io-reads, 72868 io-writes,
        12114 ipl-events, 16348 device-writes, 3 transitions, 38 checkpoints --
        zero divergences) -- 38178510 instructions, 0 frames, overlay still
        mapped, final PC 0x00f8131c
```

Every count matches exactly between the two sides, and the final PC
matches `cycle`/`fixed` mode's own idle-`STOP` PC at earlier boot
milestones. This is also the automated gate,
`record_replay_self_replay_gate` in `tests/real_rom.rs` (`--ignored`),
which additionally checks the recording run reaches the same overlay-
cleared narration milestone a plain (unrecorded) `fixed`-mode boot does.

### The seeded-divergence evidence

Five tests in `tests/real_rom.rs` (`seeded_divergence_{a,b,c,d,e}_*`),
each: record a real 60-frame HD boot, confirm it replays clean, corrupt
the log with a small in-test binary patcher (`replay_gate::Log`, a
from-scratch parser of this crate's own wire format -- deliberately not
sharing code with the real encoder/decoder, since a test that could only
corrupt a log via the same machinery it decodes with would not be testing
anything), then confirm the corrupted log fails to replay cleanly and
reports a divergence. All five pass; actual output below.

**(a) perturbed `IoRead` value** (`seeded_divergence_a_perturbed_io_read_value`):

```
WEDGED (replay divergence: IoRead mismatch: expected ordinal=1008094
addr=0x00dff01c width=2, got ordinal=1008082 addr=0x00dff01c width=2
(PC 0x00000000))
```

Not detected at the corruption's own ordinal -- by design (see
"Replay"/"Io read" above, and `Player::expect_io_read`'s own doc comment):
an `IoRead`'s *value* is the ground truth the player returns, never
checked against anything at the read itself. Detection is indirect: the
corrupted value changed how many times a polling loop iterated, which
shifted every subsequent access to that same address by a few ordinals --
caught the next time the log and the replayed CPU disagree about which
ordinal a familiar address's access lands at. The corruption index (the
11th logged `IoRead`) was chosen empirically: index 0 is the CPU's own
RESET-vector SSP fetch, which real Kickstart discards within its first
few instructions (it programs its own supervisor stack immediately), so
corrupting it was tried first and confirmed to produce **no observable
effect at all** -- a useful negative result in its own right, showing the
harness does not report false positives for a corruption with no real
consequence.

**(b) dropped `IplChange`** (`seeded_divergence_b_dropped_ipl_change`):

```
WEDGED (replay divergence: replay log desync: expected an I/O event at
ordinal=1619468 addr=0x00dff09a (PC 0x00000000), found a different event
kind)
```

**(c) flipped device-write byte** (`seeded_divergence_c_flipped_device_write_byte`):

Targets the 601st (0-indexed 600) of 627 device-writes in the 60-frame
recording -- a 512-byte hostblk DMA span into fast RAM, read back by the
guest almost immediately (sector data feeding filesystem/RDB parsing).
An early chip-RAM (blitter) span was tried first and was **not**
detected within the recording's own frame budget -- consistent with case
(e)'s finding below, and the reason this case specifically targets a
DMA span rather than a blitter one.

```
WEDGED (replay divergence: replay log desync: expected an I/O event at
ordinal=3125933 addr=0x4000124e (PC 0x00000000), found a different event
kind)
```

**What this detects, confirmed against the `RamDigestMismatch` addition
below:** the mid-run desync above, reached and reported well before the
log's `End` record. `Divergence` is sticky (only the *first* mismatch is
ever kept), so this test's actual output never mentions the RAM digest --
the ordinary I/O-comparison mechanism catches this corruption faster than
End is ever reached. The digest is what case (e) below now relies on
instead, for the corruption class (an early, later-overwritten chip-RAM
span) that this mechanism does not catch.

**(d) dropped `ClassificationTransition`** (`seeded_divergence_d_dropped_classification_transition`):

```
WEDGED (replay divergence: IoWrite mismatch: expected ordinal=433474
addr=0x00dff09a width=2 value=0x00007fff, got ordinal=429587
addr=0x00000008 width=4 value=0x00f80492 (PC 0x00000000))
```

Matches the milestone brief's own prediction almost exactly: with the
overlay-clear transition missing, the player keeps treating low chip RAM
as `Io` past the point the real table switched it to `Ram` -- so the very
next access there (`addr=0x00000008`, inside the low chip-RAM range) is
looked up against the log as an `IoRead`/`IoWrite` instead of served from
`GuestMemory`, and finds whatever the log's own next event actually was
(logged against the *correct*, `Ram`-typed address further along),
producing an immediate mismatch.

**(e) dropped blitter-span device-write** (`seeded_divergence_e_dropped_blitter_span_detected_via_ram_digest_at_end`):

**History, then the fix.** The first version of this test found a real
gap: in a headless boot configuration (no `--graphics`/`--rtgboard`, no
screenshot capture), a chip-RAM blitter span is drawn planar bitmap
content (the boot screen's pixels) that nothing in the guest's own CPU-
executed code ever reads back. Register-only checkpoints (§5.3's own
definition: D0-D7/A0-A7/PC/SR, no RAM hash) cannot observe a divergence
that never touches a register, and no later I/O access depends on those
bytes either. Tested exhaustively at the time, not assumed: twelve chip-
RAM-range device-writes sampled across an entire 500-frame boot (out of
15,978 total) were each tried dropped in turn, and none produced an
observable divergence anywhere in the run -- an early span's own bytes
are, in this boot's own draw sequence, usually overwritten again by a
later legitimate blit before the recording ends anyway, so even a whole-
RAM comparison at the very end would have found nothing left to disagree
about for those specific twelve.

That finding motivated the RAM digest ("RAM digest" above) and is kept
here as history, but the test itself now targets the **last** chip-RAM
`DeviceWrite` in the recording (`Log::drop_nth_chip_ram_device_write_from_end(0)`)
rather than an early, sampled one -- the last span has no later blit left
to paper over it, so its absence survives, unmodified, all the way to
`End`. Confirmed detected:

```
WEDGED (replay divergence: RAM digest mismatch at End: chip RAM
expected=0x0c0dd348988c969e actual=0x8b2de3415370778e)
```

The caveat this does not remove, stated in the test's own doc comment
too: detection is *at End*, not at the dropped event's own ordinal -- the
digest says the two final RAM states disagree, not where. Localizing
further needs the checkpointed-RAM-snapshot bisection §5.3's own risk
section defers, not built here.

### Investigating the identical device-write count (500-frame vs 4400-frame)

Both the 500-frame and full 4400-frame recordings (see "Full-boot
recording" below) report exactly **16,348** device-write events -- worth
treating as suspicious rather than assuming it is simply "the boot went
idle," since a re-record at a different frame count proves nothing on its
own (the same output either way). Investigated three ways, none assumed:

**1. Growth curve.** Recorded the same boot at `--max-frames` 100 through
500 (finer-grained 220-480 in steps of 20 around the transition), reading
each recording's own `record:` summary line:

| frames | device-writes | final PC (sampled) |
|---|---|---|
| 100 | 627 | 0x500641ea |
| 200 | 627 | 0x5006411e |
| 220 | 627 | 0x500643d0 |
| 240 | 627 | 0x500663ac |
| 260 | 656 | 0x00fc1d82 |
| 280 | 10,529 | 0x00f8131c |
| 300 | 10,557 | 0x00f8131c |
| 320 | 14,714 | 0x5002b7e6 |
| 340 | 16,348 | 0x00f8131c |
| 360-480 (every 20) | 16,348 | 0x00f8131c |
| 500 | 16,348 | 0x00f8131c |
| 4400 | 16,348 | 0x00f8131c |

The count grows through the boot's active desktop-rendering phase
(frames ~260-340, where the final-PC samples fluctuate because the CPU is
transiently mid-activity rather than settled at the idle `STOP`) and is
flat from 340 onward, all the way to 4400 -- consistent with "this boot
reaches a settled idle state around frame ~340 and does no further
blitter/DMA work afterward," not with a capture failure that stopped
recording partway through. The final-PC column is the same idle-`STOP`
address (`0x00f8131c`) every earlier document in this repo already
identifies as this ROM/HDF pair's settled state.

**2. Positive control: a workload that does device writes after the
machine first goes idle.** A flat count after idle is exactly what "the
boot is idle" predicts, but it is also exactly what "capture silently
stopped" predicts -- so a plain re-record cannot distinguish them.
Recorded the scripted-input double-click boot instead (the exact
`--input-script`/`--max-frames` this crate's own
`scripted_double_click_on_the_sys_icon_opens_its_drawer_fixed_mode` test
uses: a script that sleeps 4200 frames -- well past the ~340-frame idle
point above -- then moves to (42, 73) and double-clicks, opening a
Workbench drawer window, `--max-frames 4650`):

```
record: 46650040 ordinals, 347354 io-reads, 208235 io-writes, 86975 ipl-events,
        21396 device-writes, 4 classification-transitions, 46 checkpoints
PHASE1 HOSTED: LIMIT REACHED (max-frames) -- 45311623 instructions, 4650 frames,
        overlay cleared, final PC 0x00f8131c
```

**21,396 device-writes -- 5,048 more than the plain boot's 16,348** --
from window-opening activity that happens entirely after the machine's
own idle settling. This recording also replays clean (`REPLAY CLEAN`,
zero divergences, all 21,396 device-writes and the 4th classification
transition accounted for). Capture is not silently stopping after the
machine goes idle; the plain boot's flat count really is idleness, not a
failure.

**3. Breaking capture end-to-end and confirming the digest catches it.**
Temporarily commented out the blitter's own span-recording call
(`crates/machine-core/src/blitter.rs`'s `write_word`, the
`log.record(off as u32, 2)` call `docs/cpu-core-proposal.md` §5.3's
device-write log depends on) -- a deliberate, temporary diagnostic edit,
restored immediately after and confirmed byte-identical
(`git diff crates/machine-core/src/blitter.rs` before and after the
experiment compared equal). Rebuilt, then:

- **500-frame recording with blitter capture disabled:** device-write
  count dropped from 16,348 to **370** -- a delta of **15,978**, exactly
  the chip-RAM-range `DeviceWrite` count independently measured earlier
  in this document's case (e) history (twelve of exactly that population
  were sampled there). This confirms the "almost entirely the blitter's
  own chip-RAM spans" claim in "Full-boot recording" below precisely,
  not just approximately: **97.7%** of this boot's device-writes (15,978
  of 16,348) are the blitter's; the remaining 370 are hostblk/pktport/
  pcibridge DMA.
- **Replaying that 500-frame broken-capture recording** diverges --
  earlier than `End`, in fact, via an ordinary `IoRead mismatch` at
  ordinal 36,086,907 (some code path reads back state that depended on
  blitter output the log never carried, well before the run ends). A
  stronger and more immediate result than the seeded test's single-span
  drop, and expected: disabling *every* blitter span, not just one,
  makes it far more likely *something* downstream depends on one of them.
- **Replaying a *shorter*, 60-frame broken-capture recording** (short
  enough that nothing yet reads the missing output back) reproduces the
  exact scenario asked for:

  ```
  WEDGED (replay divergence: RAM digest mismatch at End: chip RAM
  expected=0x0c0dd348988c969e actual=0xf88c44b204e53c93)
  ```

  Confirming both halves of the claim: (a) a real boot recording does
  contain blitter-sourced spans in the first place -- disabling capture
  measurably changes the log and measurably breaks replay, so the seeded
  single-span-drop test above is not vacuous -- and (b) the digest
  detects a silent, total capture failure end to end, not only a single
  dropped event.

After the experiment, `crates/machine-core/src/blitter.rs` was restored
and rebuilt; `cargo test -p machine-core` (466 tests) and the full
`machine-hosted` suite were re-run clean afterward (see "Gate results"
in this milestone's final report).

### Full-boot recording (release build, serial, nothing else running)

Recorded the 4400-frame reference boot (`nondistribution/A1200.47.115.rom`
+ `nondistribution/m68k-machine.hdf`, the same pair
`scripts/bench-boot-max.sh`/`docs/deterministic-mode.md` use as the
boot-to-Workbench-ready reference), replayed it, both under
`--cpu-speed fixed`, release build, nothing else running on the host:

This was run twice: once for the pre-digest log format (version 1,
11,694,233 bytes), and once more after the `Event::End` RAM-digest
rework (version 2, below) -- both against the identical ROM/HDF pair, to
confirm the format change alone does not move the reference figures.

```
record: 43140061 ordinals, 302375 io-reads, 194405 io-writes, 82245 ipl-events,
        16348 device-writes, 3 classification-transitions, 43 checkpoints
PHASE1 HOSTED: LIMIT REACHED (max-frames) -- 41871237 instructions, 4400 frames,
        overlay cleared, final PC 0x00f8131c
recording wall time: 2.15s (real), 2.11s user

replay: 43140061 ordinals, 302375 io-reads consumed, 194405 io-writes consumed,
        82245 ipl-events, 16348 device-writes, 3 classification-transitions,
        43 checkpoints
PHASE1 HOSTED: REPLAY CLEAN (43140061 ordinals, 302375 io-reads, 194405 io-writes,
        82245 ipl-events, 16348 device-writes, 3 transitions, 43 checkpoints --
        zero divergences) -- 41871237 instructions, 0 frames, overlay still
        mapped, final PC 0x00f8131c
replay wall time: 2.01s (real), 1.99s user

log file size: 11,694,249 bytes (version 2, +16 bytes over version 1's
        11,694,233 -- exactly the two new u64 digest fields on the one
        End record every log has exactly one of)
```

**The reference figures did not move, in either version of the log
format:** 41,871,237 instructions and final PC `0x00f8131c` match
`docs/deterministic-mode.md`'s own reference figures for this exact
ROM/HDF/frame-count pair exactly, both times -- recording is
observationally transparent to the boot it records, adding logging (and,
now, one digest computation at the very end) as a side effect rather than
changing what the guest does or how far it gets. `REPLAY CLEAN` with the
digest check active also means something version 1's evidence did not
prove: chip RAM and fast RAM are bit-for-bit identical between the master
and the replayed run at the moment the boot reaches `--max-frames 4400`,
not merely "every I/O access and register checkpoint agreed."

Event-count breakdown (of 43,140,061 total ordinals): I/O reads are the
large majority (302,375), consistent with a boot-to-Workbench-ready
workload being read/poll-heavy (beam-position and status-register waits);
device writes (16,348) and classification transitions (3: the overlay
clearing, then AUTOCONFIG placing hostblk and fast RAM) match the same
three-transition pattern every recording in this document's other
evidence shows. "Investigating the identical device-write count" above
measured the 16,348 precisely rather than just asserting it: 15,978 of
them (97.7%) are the blitter's own chip-RAM spans, the remaining 370 are
hostblk/pktport/pcibridge DMA -- confirming, not just describing, "almost
entirely the blitter's." At 11.15 MiB for a full boot-to-Workbench-ready
run, the log is small enough to keep on disk routinely, not something
that needs the ring-bounded design deferred under "Scope" above.

## Re-running the evidence in this document

```sh
cargo build --release -p machine-hosted

# Self-replay
./target/release/machine-hosted --rom nondistribution/A1200.47.115.rom \
  --hostblk nondistribution/m68k-machine.hdf --cpu-speed fixed \
  --max-frames 500 --max-instructions 0 --record /tmp/replay.log
./target/release/machine-hosted --rom nondistribution/A1200.47.115.rom \
  --hostblk nondistribution/m68k-machine.hdf --cpu-speed fixed \
  --max-frames 0 --max-instructions 0 --replay /tmp/replay.log

# The seeded-divergence tests and the self-replay gate
cargo test --release -p machine-hosted --test real_rom -- --ignored \
  --test-threads=1 record_replay_self_replay_gate seeded_divergence
```

## Recording-off cost, measured

The standing requirement: recording must not slow the machine when off.
Measured at C1 close-out (2026-09-30), release builds, strictly serial,
same host and session, against a clean worktree of the pre-change
baseline (`1d44a3b`, the direct-mapping slice -- i.e. the A/B isolates
exactly this change set: the `Bus.6`/`Bus.7` `Option` checks, the
`GuestRam::ram_slice_mut` span-log branch, and the blitter's per-word
`Option<&mut DeviceWriteLog>` check).

**Primary metric** -- `/usr/bin/time -l` user time over the unpaced
`fixed`-mode 4400-frame reference boot (a planar, blitter-heavy
workload; ~1.5 s of pure CPU, so a small denominator problem does not
arise the way it does for max mode's `busy_mips` self-report), five
alternating A/B pairs per configuration:

| configuration | baseline `1d44a3b` (user s) | with the seam, recording off (user s) | delta |
|---|---|---|---|
| `--direct-map on` (release default) | 1.44, 1.40, 1.44, 1.44, 1.50 (mean 1.444) | 1.43, 1.43, 1.44, 1.48, 1.43 (mean 1.442) | **not distinguishable from zero** |
| `--direct-map off` | 1.58, 1.58, 1.56, 1.56, 1.57 (mean 1.570) | 1.65, 1.65, 1.66, 1.66, 1.67 (mean 1.658) | **+0.088 s, ~+5.6%, consistent in all 5 pairs** |

**Supplementary** -- `scripts/bench-boot-max.sh --backend interp` busy
MIPS (max mode, direct map on; the noisy ~2.8 s busy denominator caveat
from the plan's own tables applies), two alternating pairs: baseline
35.21 / 35.47, with the seam 34.94 / 34.72. Same direction as the
primary metric's zero-to-noise result once the denominator noise is
respected; not treated as evidence of a real map-on cost.

**Reading:** on the release default path (direct map on), the
recording-off cost is not distinguishable from zero -- the player check
sits before the direct-map dispatch but is a single `None` test, and the
recorder check sits on the fall-through (slow) path only. With the
direct map **off**, every bus access pays the player check plus the
recorder check with no fast path to hide behind, and that measures a
real, reproducible **~5-6%** on the fixed-mode boot. Map-off release
configurations are non-default (explicit `--direct-map off`, an active
access-intercepting diagnostic, `--cpu-backend batch`, or direct-map
construction failure -- plus every debug build, where `auto` resolves
off). Recorded as a known, bounded cost rather than silently accepted:
if it matters, the two `Option` fields can be merged into one
`Option<Box<enum>>` dispatch (halving the checks), or the run loops
monomorphized over recording mode (eliminating them); neither was done
here, since the default path measures clean.

## C1 exit, assessed honestly (supersedes `docs/direct-mapping.md`'s)

§8's C1 exit: *"m68k-rs runs through the trait in both timing modes,
producing replay logs; gates pass in the new deterministic mode; fault
cost recorded."* Item by item, at C1 close-out:

- **m68k-rs through the trait, both timing modes** -- met
  (`docs/cpu-core-trait.md`): `cycle` and `max` both run through
  `GuestCpu`, plus `fixed`.
- **"producing replay logs" -- now met, with stated bounds.** A working
  recorder and player exist (`--record`/`--replay`, this document): a
  full 4400-frame reference boot records (11,694,249 bytes, 2.57 s) and
  replays clean with zero divergences; five seeded-divergence tests show
  detection with teeth (perturbed I/O read, dropped IPL change, flipped
  DMA byte, dropped classification transition, dropped blitter span --
  the last via the End-record RAM digest); a deliberate capture-break
  experiment (blitter span recording disabled in a scratch edit) is
  detected end to end. Bounds: `fixed` mode only (by design -- max
  mode's unhooked-batch IPL gap, `docs/cpu-core-trait.md`); self-replay
  only (m68k-rs both sides -- §5.3's cross-core use arrives with C2, and
  per-opcode undefined-flag masking is deferred to then); RAM-only
  divergences are detected at the End digest, not localized.
- **"gates pass in the new deterministic mode" -- met.**
  18 `fixed`-mode sibling gates (every convertible category in
  `real_rom.rs`, including the 4 previously-unrun combination gates and
  one AROS screenshot gate the first sweep missed), all passing at their
  original `--max-frames` with unmoved reference figures; the only two
  tests without siblings are `max`-mode-only by design (CPUBench's
  wall-clock self-calibration; the `WAIT 5` real-time-pacing gate) --
  `docs/deterministic-mode.md`.
- **"fault cost recorded" -- met on the wrong host, unchanged.** macOS
  figures recorded (`docs/direct-mapping.md`); the Linux measurement
  §4.6 names remains outstanding and now carries a written hand-off
  (same document, "Linux hand-off"): the harness is portable libc except
  the Darwin-only ucontext chain, which is a few cfg-gated lines on
  Linux/aarch64 and flagged-larger work on Linux/x86-64. Not measurable
  from this host, and not faked.
- **Direct mapping, page-type table, open-bus pages** -- met
  (`docs/direct-mapping.md`), unchanged.

So C1's exit is met **except** the Linux half of the fault-cost item,
which is host-bound, has a written hand-off, and does not gate C2's
correctness work (it informs C3's JIT I/O-dispatch choice, per §4.6).

**Can C2 start?** Yes. The harness C2 needs is the one that now exists
and is exercised: a master (m68k-rs, `--record`, `fixed` mode) producing
a complete log, and a player shape (`Bus.7` + `run_guest_replay`) whose
`MachineBus`-free access rules are exactly what a `no_std` C2 core will
face. What C2 must add on top, already flagged in "Scope" and "How a
second-core player would differ": implement `GuestCpu`/`HookCpu` for the
new core (including `sr()`), per-opcode undefined-flag masking at the
comparison points, and -- if RAM-only divergence localization turns out
to matter in practice -- checkpointed RAM snapshots with bisection.
