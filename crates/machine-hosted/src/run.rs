//! The run loop: construct the machine, execute the guest, and turn what
//! happened into a diagnosable, CI-greppable result.
//!
//! # The interrupt-delivery risk (roadmap Phase 1)
//!
//! `m68k` 0.12.1's [`m68k::CpuCore`] does not sample the guest's IPL lines
//! on its own; the host is expected to call [`m68k::CpuCore::set_irq`]
//! between instructions and let the core decide whether it's serviceable
//! (`set_irq` just latches `int_level`; `check_interrupts` compares it
//! against the SR mask). The crate ships three families of run loop for
//! this: [`m68k::CpuCore::run_for_cycles`] (no host hook -- IRQ state can
//! only be pushed in before the whole batch runs, which is wrong for a
//! chipset ticking on real time), and the two hook variants,
//! [`m68k::CpuCore::run_for_cycles_with_hook`] and
//! `run_for_cycles_with_boundary_hook`, whose docs are explicit that they
//! exist "to let a host sync devices and IRQ state between instructions".
//!
//! This runner uses `run_for_cycles_with_hook`: its docs guarantee the
//! hook runs after every retired instruction and that "a newly serviceable
//! interrupt is taken before the next instruction" once the hook returns,
//! which is exactly the level-sensitive-IPL contract real 68k hardware
//! implements. `run_for_cycles_with_boundary_hook` additionally reports
//! interrupt-entry boundaries, which nothing here needs to observe. So per
//! hook call: forward the instruction's elapsed cycles into
//! [`MachineBus::tick`], then set the CPU's `int_level` from
//! [`MachineBus::pending_irq_level`]. That leaves interrupt *masking*
//! (SR's I0-I2 vs the requested level, and the level-7 NMI-not-maskable
//! rule) entirely inside `m68k-rs`, which is where the 68k spec puts it.
//!
//! Reset semantics were the other half of the risk. [`m68k::CpuCore::reset`]
//! reads the initial SSP/PC through the bus at `$000000`/`$000004`, which
//! only lands in ROM because `MachineBus` starts with its OVL overlay
//! asserted (see `machine-core`'s `overlay` field) -- exactly matching real
//! Gary/CIA-A behaviour and already exercised by `machine-core`'s own
//! `tests/hello_guest.rs`. Nothing further was needed here.
//!
//! # Traps are not auto-dispatched
//!
//! `run_for_cycles_with_hook` returns control to the host on A-line/F-line/
//! TRAP/BKPT/illegal-instruction rather than taking the hardware exception
//! itself -- that surfacing exists for HLE embedders who want to intercept
//! the trap. This runner does no HLE, so it always takes the real 68k
//! exception via `CpuCore::take_{aline,fline,trap,bkpt,illegal}_exception`
//! and resumes, which is what unmodified hardware would do.
//!
//! # `--cpu-speed max`: a second run loop, not a mode flag on this one
//!
//! `docs/adr-0006-cycle-budgeted-and-wall-clock-paced-timing.md` adds a
//! wall-clock-paced timing model alongside the cycle-budgeted one above.
//! Rather than thread a mode flag through every branch of `run_guest`
//! (the interrupt-delivery contract above, the trap handling above, the
//! `Stopped` resync, the limit/wedge checks), max mode is a separate
//! function, `run_guest_max`, sharing only `drain_serial`/
//! `service_host_serial`/`track_exception` with `run_guest`. `run_guest`
//! itself is untouched by any of this -- cycle mode's `run_for_cycles_with_hook`
//! call, its per-instruction hook, and its `Stopped` handling are exactly
//! what they were before this module gained `run_guest_max`.
//!
//! The two loops differ in what drives the CPU and how device time
//! advances, not in the interrupt or trap contracts above: `run_guest_max`
//! still forwards elapsed cycles into `MachineBus::tick` and sets `int_level`
//! from `pending_irq_level` the same way, still takes every trap for real
//! rather than doing HLE, and STOP is still resynced by ticking device
//! time and resampling the IPL before resuming -- only the *source* of the
//! CPU's cycles (hook-free `run_for_cycles` chunks instead of one
//! `run_for_cycles_with_hook` call) and *how much* device time each tick
//! advances by (a wall-clock-derived deadline instead of the chunk's own
//! retired cycles) change. See `run_guest_max`'s own doc comment for the
//! timing model, and the module-level constants above it
//! (`MAX_MODE_CHUNK_TARGET_US` etc.) for the tuning knobs.
//!
//! The per-instruction hook's other duties -- serial/input scripts,
//! screenshots, the overlay-cleared marker, `--max-instructions`/
//! `--max-frames`, and progress output -- move to `max_chunk_boundary`,
//! called once per chunk instead of once per instruction. Wedge detection
//! changes shape entirely: cycle mode's `TIGHT_LOOP_THRESHOLD` counts
//! instructions at one PC, which has no meaning when the CPU is
//! unbudgeted, so max mode instead samples the PC once per chunk boundary
//! and calls it a wedge after `MAX_MODE_WEDGE_SECONDS` of *wall clock*
//! with the CPU not stopped (a `GuestExit::Stopped` chunk resets the
//! streak rather than extending it -- Kickstart's idle dispatcher legitimately
//! holds the same PC in STOP forever, and this detector must never mistake
//! that for a wedge).
//!
//! `--trace` (and anything else built on the per-instruction hook) has no
//! equivalent in `run_guest_max` and is refused together with
//! `--cpu-speed max` in `run`, before either run loop is reached.
//!
//! # Wedge detection catches a small loop, not just one PC
//!
//! The freeze `docs/wedge-detection.md` documents (commit `525dc1a`, a
//! `--cpu-speed fixed` bug since fixed) sat in a three-instruction
//! `VHPOSR` poll -- `MOVE.W (A4),D1 / CMP.B <ea>,D1 / BLS` -- for 870
//! million iterations, still retiring instructions, making no progress.
//! The wedge detector at the time counted *consecutive identical* PCs;
//! a three-instruction loop never repeats a PC consecutively, so the
//! streak reset on every instruction and never fired. [`LoopWindow`]
//! replaces that: it tracks the smallest PC span containing every
//! recently retired PC, and counts a streak of instructions that stayed
//! inside a tiny span (see [`LOOP_WINDOW_SPAN_BYTES`]) rather than at one
//! address. See `docs/wedge-detection.md` for the full design, the
//! false-positive analysis (why an idle Workbench in `STOP` never trips
//! this), and the both-directions evidence.

use std::path::Path;
use std::time::{Duration, Instant};

use machine_core::block::BlockDevice;
use machine_core::{pci, GuestMemory, MachineBus, CHIP_RAM_SIZE, CPU_CLOCKS_PER_ECLOCK};

use crate::bus::Bus;
use crate::cli::Args;
use crate::console::Console;
use crate::cpu::{GuestBatchResult, GuestCpu, GuestExit, HookControl, HookCpu, M68kRsCore};
use crate::hd_image::FileBlockDevice;
use crate::input_script::InputScript;
use crate::rom_image;
use crate::serial_script::SerialScript;
use crate::serial_tcp::SerialTcpBridge;

/// CPU cycles requested per `run_for_cycles_with_hook` call. Chosen well
/// under `i32::MAX` (so a long-running batch can never overflow the
/// budget accounting) and coarse enough that outer-loop bookkeeping
/// (limit checks, frame-count sampling) stays cheap relative to the
/// per-instruction hook, which is where the real per-instruction work
/// (tick, IRQ sync, trace, wedge detection) happens.
const RUN_BATCH_CYCLES: i32 = 2_000_000;

/// One raster line's worth of guest clocks, in the same CPU-clock-
/// equivalent unit [`MachineBus::tick`] takes -- shared by cycle mode's
/// STOP-path resync (which already ticks in exactly this slice; see
/// `run_guest`'s own comment on why) and `--cpu-speed fixed`'s per-line
/// advance (`run_guest_fixed`, `docs/deterministic-mode.md`). Computed
/// from the same constants every device's arithmetic already uses, never
/// hardcoded, so it can't drift out of step with `machine-core`.
const ONE_LINE_CLOCKS: u32 =
    machine_core::chipset::PAL_COLOUR_CLOCKS_PER_LINE * machine_core::CPU_CLOCKS_PER_COLOUR_CLOCK;

/// `--cpu-speed fixed`'s default `--instructions-per-line` (`docs/cpu-
/// core-proposal.md` §5.2's `N`): the measured average number of
/// instructions `cycle` mode retires per raster line while the CPU is
/// not stopped. Measured on a real Kickstart 3.2 boot to Workbench-ready
/// (the same ROM/HDF pair `scripts/bench-boot-max.sh` uses) -- see
/// `docs/deterministic-mode.md` for the method and the raw numbers.
/// Chosen this way, rather than a round number, so the existing frame-
/// count gates carry over with the least re-baselining (§5.2's own
/// stated reason).
pub const DEFAULT_INSTRUCTIONS_PER_LINE: u32 = 424;

/// `--cpu-speed fixed`'s per-line cycle budget passed to
/// `run_for_cycles_with_hook` -- deliberately huge and, unlike
/// [`RUN_BATCH_CYCLES`], never meant to be reached: the hook alone
/// decides when a line's `--instructions-per-line` quota is met and
/// returns [`HookControl::Return`], so this budget only exists as an
/// `i32` a core's `run_for_cycles_with_hook` signature requires, never
/// consulted by fixed mode's own logic (a cycle-table-free core is free
/// to ignore it entirely, or treat the `cycles` value handed to the hook
/// as always `0`/undefined -- fixed mode never reads it). Large enough
/// that no real instruction mix at any sane `--instructions-per-line`
/// value should ever exhaust it first; if one somehow does, the outer
/// loop just calls `run_for_cycles_with_hook` again without ticking a
/// line (see `run_guest_fixed`'s own handling of that case) rather than
/// mis-advancing device time.
const FIXED_MODE_BATCH_CYCLES: i32 = 200_000_000;

/// `--cpu-speed max` (`docs/adr-0006-cycle-budgeted-and-wall-clock-paced-timing.md`,
/// `run_guest_max`): the adaptive chunk size's real-time target. Bounds
/// the worst-case lateness of the IPL being set after a device event or a
/// guest interrupt-raising write that didn't itself request a boundary --
/// well under one raster line's real time (~64 µs at PAL's line rate).
const MAX_MODE_CHUNK_TARGET_US: f64 = 50.0;

/// `run_guest_max`'s adaptive chunk size never goes below this many CPU
/// cycles, however slow the host measures itself to be -- a floor against
/// a chunk size of zero (which would spin `run_for_cycles` uselessly) and
/// against a single pathologically slow measurement collapsing the chunk
/// size to nothing.
const MAX_MODE_MIN_CHUNK_CYCLES: i32 = 256;

/// `run_guest_max`'s adaptive chunk size never exceeds this many CPU
/// cycles, however fast the host measures itself to be -- a ceiling that
/// keeps a single chunk from running long enough to blow well past
/// `MAX_MODE_CHUNK_TARGET_US`'s latency bound if the rate estimate is
/// ever wrong (e.g. right after a STOP-path resync, before a fresh
/// measurement corrects it).
const MAX_MODE_MAX_CHUNK_CYCLES: i32 = 400_000;

/// `--cpu-backend batch`'s adaptive chunk size floor, in instructions
/// rather than cycles: `run_batch` is instruction-budgeted and reports no
/// cycle count at all (`m68k`'s own doc comment on `run_batch` -- it
/// clobbers `cycles_remaining`), so this chunk-size estimate is derived
/// entirely from wall-clock time per retired instruction, never from
/// cycles or `cycles_remaining` (plan step 7.2's own requirement). Same
/// floor rationale as [`MAX_MODE_MIN_CHUNK_CYCLES`].
const MAX_MODE_MIN_CHUNK_INSTRS: u32 = 64;

/// `--cpu-backend batch`'s adaptive chunk size ceiling, in instructions.
/// Same rationale as [`MAX_MODE_MAX_CHUNK_CYCLES`]: bounds how far a
/// single chunk can overshoot `MAX_MODE_CHUNK_TARGET_US` if the rate
/// estimate is stale.
const MAX_MODE_MAX_CHUNK_INSTRS: u32 = 100_000;

/// `run_guest_max`'s backlog cap (ADR 0006, "When the host can't keep
/// up"): device time is never allowed to fall more than this far behind
/// the wall clock. Beyond it, the excess is discarded (device time jumps
/// forward to wall clock minus this cap) rather than being replayed as a
/// burst of back-to-back interrupts. A tuning constant, not measured
/// against real hardware -- 100 ms is ADR 0006's own starting figure.
const MAX_MODE_BACKLOG_CAP: Duration = Duration::from_millis(100);

/// `run_guest_max`'s STOP-path sleep granularity: the CPU is parked
/// (real-time-idle, not busy-spinning) between checks of the next
/// device deadline and any host input source, in slices no longer than
/// this.
const MAX_MODE_STOP_SLEEP_SLICE: Duration = Duration::from_millis(1);

/// `run_guest_max`'s wedge threshold: the PC, sampled at every chunk
/// boundary, staying confined to one [`LoopWindow`] for this many seconds
/// of *wall clock*, with the CPU not stopped, is called a wedge. Real
/// Kickstart's idle dispatcher always STOPs rather than spinning, so any
/// non-STOP loop that holds a tiny PC range this long is not a legitimate
/// wait -- chosen generously above what any real boot-time busy-wait in
/// this codebase's own tests takes, since max mode has no fixed
/// instructions-per-second to size a count-based threshold from (cycle
/// mode's `TIGHT_LOOP_THRESHOLD`, which this replaces for `--cpu-speed
/// max`).
const MAX_MODE_WEDGE_SECONDS: f64 = 15.0;

/// Report progress roughly once a second of guest (PAL) time.
const PROGRESS_EVERY_FRAMES: u64 = 50;

/// Instructions retired with the PC confined to a [`LoopWindow`] no wider
/// than [`LOOP_WINDOW_SPAN_BYTES`] before this is called a wedge rather
/// than a legitimate busy-wait (real idle loops in Kickstart *do* spin
/// inside a handful of instructions waiting for VERTB or a device flag --
/// this threshold is well above one frame's instruction count so it does
/// not fire on that; see `docs/wedge-detection.md` for the measurement
/// this is checked against across all 32 real-ROM gates, and why STOP,
/// not a bounded spin, is how every legitimate wait in this codebase's
/// tests actually idles).
const TIGHT_LOOP_THRESHOLD: u64 = 20_000_000;

/// [`LoopWindow`]'s span cap, in bytes of PC address space: the window
/// may grow to cover this much code before it's judged "not a tight
/// loop" and reset. Sized generously above the freeze this replaces (a
/// 3-instruction poll spanning well under 16 bytes) while staying much
/// smaller than any real subroutine or basic block in this codebase's
/// ROMs -- `docs/wedge-detection.md` records the reasoning and the
/// alternative sizes considered.
const LOOP_WINDOW_SPAN_BYTES: u32 = 64;

/// Tracks the smallest contiguous PC range containing every recently
/// retired instruction's PC, and how many instructions in a row have
/// landed inside it -- the generalisation of a same-PC streak that
/// catches a small *loop* (several instructions, several addresses), not
/// only a single instruction spinning on itself. See this module's own
/// "Wedge detection catches a small loop, not just one PC" doc section
/// and `docs/wedge-detection.md`.
///
/// [`Self::extend`] is the only way the window changes: a PC that keeps
/// the window's span within [`LOOP_WINDOW_SPAN_BYTES`] widens it (or
/// leaves it alone) and extends the streak; a PC that would need a wider
/// window resets to that PC alone, streak back to 1. Cycle/fixed mode
/// read [`Self::streak`] (an exact instruction count, since their hook
/// runs once per retired instruction); max mode instead uses `extend`'s
/// `bool` return to decide whether to keep or reset its own *wall-clock*
/// timer, since it only samples the PC once per chunk (see
/// `run_guest_max`'s own doc comment on why wedge detection "changes
/// shape entirely" there).
struct LoopWindow {
    lo: u32,
    hi: u32,
    streak: u64,
}

impl LoopWindow {
    fn new(pc: u32) -> Self {
        LoopWindow {
            lo: pc,
            hi: pc,
            streak: 1,
        }
    }

    /// Reset the window to `pc` alone, streak back to 1 -- used wherever
    /// a genuine "wait is over" signal is available (a STOP exit, an
    /// exception taken) even though `pc` itself might coincidentally
    /// match the window's current span.
    fn reset(&mut self, pc: u32) {
        self.lo = pc;
        self.hi = pc;
        self.streak = 1;
    }

    /// Fold one more retired (or chunk-boundary-sampled) PC into the
    /// window. Returns `true` if `pc` fit within [`LOOP_WINDOW_SPAN_BYTES`]
    /// of the window's existing span (the window was widened, or left
    /// alone, and the streak extended); `false` if `pc` was far enough
    /// away that the window was reset to `pc` alone instead.
    fn extend(&mut self, pc: u32) -> bool {
        let lo = self.lo.min(pc);
        let hi = self.hi.max(pc);
        if hi - lo <= LOOP_WINDOW_SPAN_BYTES {
            self.lo = lo;
            self.hi = hi;
            self.streak += 1;
            true
        } else {
            self.reset(pc);
            false
        }
    }
}

/// Identical CPU exceptions taken back-to-back at the same faulting PC
/// before this is called an exception storm.
const EXCEPTION_STORM_THRESHOLD: u64 = 1_000;

/// Why the run ended.
pub enum Outcome {
    /// The guest executed STOP with SR's interrupt mask at 7 -- every
    /// maskable level (1-6, all this chipset ever requests) shut out, so
    /// nothing can ever resume it. Real Kickstart never does this; it is
    /// how the synthetic smoke-test ROM signals "done". Any other STOP
    /// (Kickstart's idle dispatcher uses mask 0) is not this outcome --
    /// see `run_guest`'s handling of `GuestExit::Stopped`.
    CleanHalt,
    /// A bound the caller asked for (`--max-frames`/`--max-instructions`)
    /// was hit before any other outcome. Expected for real ROMs at this
    /// phase, since the chipset/CIA implementations are still landing.
    LimitReached(&'static str),
    /// A wedge was detected: a tight loop at one PC, or an exception
    /// storm, well past what a legitimate idle wait looks like.
    Wedged(String),
    /// Couldn't even get to running (ROM I/O, empty image, etc.).
    SetupError(String),
    /// `--replay` reached the log's `End` event with no divergence
    /// (`docs/cpu-core-proposal.md` §5.3's player finishing clean). Exit
    /// 0, like [`Outcome::CleanHalt`], but reported with the replay-
    /// specific evidence the milestone brief asks for rather than
    /// `CleanHalt`'s halt-specific wording. A divergence, by contrast,
    /// reuses [`Outcome::Wedged`] -- it is exactly that trait's shape
    /// ("something is wrong, stop and report it, exit nonzero"), and
    /// `Wedged`'s `String` payload already carries a free-form reason.
    ReplayClean {
        ordinals: u64,
        io_reads: u64,
        io_writes: u64,
        ipl_events: u64,
        device_writes: u64,
        transitions: u64,
        checkpoints: u64,
    },
}

/// `--cpu-speed max`'s own timing breakdown (plan step 7.2). 7.1's only
/// MIPS figure averaged in time spent asleep in STOP and never measured
/// wall clock to Workbench at all -- this splits wall time into sleep
/// (STOP-path parking, ADR 0006's "STOP sleeps") and busy time, so
/// [`Self::busy`] reports instructions per second of actual CPU
/// execution rather than diluting it by however idle the sampled window
/// happened to be. `None` in cycle mode, which has no sleep concept.
///
/// `slept` is the *measured elapsed time* of each STOP-path sleep (an
/// `Instant::now()` taken right before `std::thread::sleep`, not the
/// nominal slice requested). An earlier version of this struct credited
/// the nominal slice instead; `std::thread::sleep`'s routine overshoot
/// was then silently reclassified as busy time by [`Self::busy`], which
/// on an idle-heavy boot workload overstated busy by about an order of
/// magnitude (self-reported busy 38.2% of wall vs 3.8-4.0% by an
/// independent 1 kHz `samply` profile of the same run -- see
/// `docs/cpu-core-c0-profile.md`). Fixed by measuring elapsed time at
/// the accumulation site in `run_guest_max`.
#[derive(Clone, Copy, Debug)]
pub struct MaxModeTiming {
    pub wall: Duration,
    pub slept: Duration,
}

impl MaxModeTiming {
    /// `wall - slept`, floored at zero. `slept` should never exceed
    /// `wall` -- it is built from `Instant::elapsed()` calls strictly
    /// within the `wall_start..wall_start.elapsed()` window -- so
    /// `saturating_sub` here is underflow-safety, not a place a real
    /// accounting bug should be able to hide quietly. If it ever fires,
    /// something upstream is timing wrong; `debug_assert!` surfaces that
    /// loudly in debug/test builds without costing anything in release.
    pub fn busy(&self) -> Duration {
        debug_assert!(
            self.slept <= self.wall,
            "MaxModeTiming: slept ({:?}) exceeds wall ({:?}) -- timing accounting bug",
            self.slept,
            self.wall,
        );
        self.wall.saturating_sub(self.slept)
    }
}

pub struct Report {
    pub outcome: Outcome,
    pub instructions: u64,
    pub frames: u64,
    pub final_pc: u32,
    pub overlay_cleared: bool,
    pub max_mode_timing: Option<MaxModeTiming>,
}

impl Report {
    /// Process exit code: 0 only for a clean, expected halt. Everything
    /// else is non-zero so a CI runner invoking this with `--max-frames`/
    /// `--max-instructions` set can tell "bounded and fine" apart from
    /// "bounded because it ran out of rope" -- see the CLI docs on those
    /// flags.
    pub fn exit_code(&self) -> u8 {
        match &self.outcome {
            Outcome::CleanHalt => 0,
            Outcome::LimitReached(_) => 1,
            Outcome::Wedged(_) => 2,
            Outcome::SetupError(_) => 3,
            Outcome::ReplayClean { .. } => 0,
        }
    }

    /// The final `PHASE1 HOSTED: ...` line, CI-greppable.
    pub fn status_line(&self) -> String {
        let detail = match &self.outcome {
            Outcome::CleanHalt => "CPU HALTED CLEANLY".to_string(),
            Outcome::LimitReached(which) => format!("LIMIT REACHED ({which})"),
            Outcome::Wedged(reason) => format!("WEDGED ({reason})"),
            Outcome::SetupError(reason) => format!("SETUP ERROR ({reason})"),
            Outcome::ReplayClean {
                ordinals,
                io_reads,
                io_writes,
                ipl_events,
                device_writes,
                transitions,
                checkpoints,
            } => format!(
                "REPLAY CLEAN ({ordinals} ordinals, {io_reads} io-reads, {io_writes} io-writes, \
                 {ipl_events} ipl-events, {device_writes} device-writes, {transitions} \
                 transitions, {checkpoints} checkpoints -- zero divergences)"
            ),
        };
        format!(
            "{detail} -- {} instructions, {} frames, overlay {}, final PC {:#010x}",
            self.instructions,
            self.frames,
            if self.overlay_cleared {
                "cleared"
            } else {
                "still mapped"
            },
            self.final_pc,
        )
    }

    /// `--cpu-speed max`'s end-of-run timing report (plan step 7.2):
    /// total wall time, *measured* time slept in STOP (see
    /// [`MaxModeTiming`]'s doc comment -- this is elapsed sleep time, not
    /// nominal requested slices), busy time, and busy MIPS (instructions
    /// retired / busy seconds). `None` in cycle mode.
    pub fn timing_report(&self) -> Option<String> {
        let timing = self.max_mode_timing?;
        let busy = timing.busy();
        let busy_mips = if busy > Duration::ZERO {
            self.instructions as f64 / busy.as_secs_f64() / 1_000_000.0
        } else {
            0.0
        };
        Some(format!(
            "max-mode timing: wall={:.3}s slept={:.3}s busy={:.3}s busy_mips={busy_mips:.2}",
            timing.wall.as_secs_f64(),
            timing.slept.as_secs_f64(),
            busy.as_secs_f64(),
        ))
    }
}

fn setup_error(console: &mut Console, reason: String) -> Report {
    console.diag(&format!("machine-hosted: {reason}"));
    Report {
        outcome: Outcome::SetupError(reason),
        instructions: 0,
        frames: 0,
        final_pc: 0,
        overlay_cleared: false,
        max_mode_timing: None,
    }
}

fn load_rom(label: &str, path: &Path) -> Result<Vec<u8>, String> {
    let bytes = rom_image::load(path)
        .map_err(|e| format!("reading {label} ROM {}: {e}", path.display()))?;
    if bytes.is_empty() {
        return Err(format!("{label} ROM {} is empty", path.display()));
    }
    Ok(bytes)
}

pub fn run(args: &Args, console: &mut Console) -> Report {
    console.diag("m68k Machine hosted runner -- Phase 1 blind boot");
    console.diag(&format!("main ROM: {}", args.rom.display()));

    // Read once up front, outside either ROM's loading below: both
    // `--rom` and `--ext-rom` can be Cloanto-encoded (in principle;
    // `--ext-rom` being encoded is unusual but not refused specially),
    // and a failure to read a *given* `--rom-key` path is itself a setup
    // error, distinct from "no key was needed at all" -- reported before
    // either ROM's own load so a bad `--rom-key` path is diagnosed
    // immediately rather than only surfacing once a Cloanto image
    // happens to need it.
    let rom_key = match &args.rom_key {
        Some(path) => match std::fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                return setup_error(
                    console,
                    format!("reading --rom-key {}: {e}", path.display()),
                )
            }
        },
        None => None,
    };

    let rom_bytes = match load_rom("main", &args.rom) {
        Ok(b) => b,
        Err(e) => return setup_error(console, e),
    };
    let rom_bytes = match rom_image::prepare(console, "main", rom_bytes, rom_key.as_deref()) {
        Ok(b) => b,
        Err(e) => return setup_error(console, e),
    };

    let ext_rom_bytes = match &args.ext_rom {
        Some(path) => {
            console.diag(&format!("ext ROM: {}", path.display()));
            match load_rom("ext", path) {
                Ok(b) => match rom_image::prepare(console, "ext", b, rom_key.as_deref()) {
                    Ok(b) => Some(b),
                    Err(e) => return setup_error(console, e),
                },
                Err(e) => return setup_error(console, e),
            }
        }
        None => None,
    };

    // The direct map (`docs/cpu-core-proposal.md` §4.6, `crate::directmap`)
    // is disabled by `--no-direct-map` or by any of the three diagnostics
    // that intercept bus accesses -- the same set `bus.rs`'s `Bus.5` doc
    // comment names, read here (not where those diagnostics' own booleans
    // are computed further down) because RAM's allocation strategy below
    // depends on this decision and must be made before `MachineBus`
    // borrows either buffer. `SERIAL_REG_TRACE`/`BUS_COVERAGE` are read
    // once, here, for exactly that reason; `serial_trace_enabled`/
    // `bus_coverage_enabled` further down re-derive the same env vars
    // (cheap, side-effect-free, and keeps each one declared next to the
    // diagnostic it gates, matching the existing style) rather than
    // threading this value back out.
    // `--direct-map auto` (the default) resolves by build profile: the
    // map is a measured ~12% busy-MIPS win in release but a pure
    // pessimization at opt-level 0, where its per-access dispatch never
    // inlines -- enough added guest-execution latency to break the
    // debug-profile suite's Wait-5 wall-clock gate (`docs/
    // direct-mapping.md`, "Default policy"). The split is loud, never
    // silent: the diag lines below say what was decided and why on
    // every run.
    // `--record`/`--replay` (`docs/cpu-core-proposal.md` §5.3, `docs/replay-
    // log.md`): validated up front, before any RAM allocation or bus
    // construction, for the same "diagnose a bad flag combination
    // immediately" reason every other setup-error check in this function
    // runs before the work it would otherwise waste. Both veto the direct
    // map below (`interceptor_active`, extended) for the same reason the
    // three pre-existing interceptors do: the recorder/player must see
    // every access at their own classification granularity, which the
    // direct map's `PROT_NONE` fast path bypasses for `Ram`/`Rom`/`OpenBus`
    // pages.
    let record_active = args.record.is_some();
    let replay_active = args.replay.is_some();
    if record_active && replay_active {
        return setup_error(
            console,
            "--record and --replay cannot both be given -- recording and replaying are \
             mutually exclusive for one run"
                .to_string(),
        );
    }
    if (record_active || replay_active) && args.cpu_speed != crate::cli::CpuSpeed::Fixed {
        return setup_error(
            console,
            format!(
                "{} requires --cpu-speed fixed: only fixed mode can apply a logged IPL/\
                 classification event at an exact retired-instruction index, and only fixed \
                 mode ticks devices once per instruction, which is what makes the device-write \
                 span drain exact (docs/replay-log.md)",
                if record_active {
                    "--record"
                } else {
                    "--replay"
                }
            ),
        );
    }
    if (record_active || replay_active) && args.cpu_backend == crate::cli::CpuBackend::Batch {
        return setup_error(
            console,
            "--record/--replay cannot be combined with --cpu-backend batch".to_string(),
        );
    }
    if (record_active || replay_active) && args.blitter_trace.is_some() {
        return setup_error(
            console,
            "--record/--replay cannot be combined with --blitter-trace".to_string(),
        );
    }
    if (record_active || replay_active) && std::env::var_os("SERIAL_REG_TRACE").is_some() {
        return setup_error(
            console,
            "--record/--replay cannot be combined with SERIAL_REG_TRACE".to_string(),
        );
    }
    if (record_active || replay_active) && std::env::var_os("BUS_COVERAGE").is_some() {
        return setup_error(
            console,
            "--record/--replay cannot be combined with BUS_COVERAGE".to_string(),
        );
    }

    let mode_requested = match args.direct_map {
        crate::cli::DirectMapMode::On => true,
        crate::cli::DirectMapMode::Off => false,
        crate::cli::DirectMapMode::Auto => cfg!(not(debug_assertions)),
    };
    let interceptor_active = args.blitter_trace.is_some()
        || std::env::var_os("SERIAL_REG_TRACE").is_some()
        || std::env::var_os("BUS_COVERAGE").is_some()
        // `--cpu-backend batch` too: its `FastMem` raw-pointer window
        // over fast RAM's primary view and the direct map's alias view
        // are in principle coherent (same `MAP_SHARED` pages), but no
        // gate exercises that combination, and the conservative posture
        // keeps `batch` byte-identical to its pre-direct-map behaviour.
        // Revisit only if batch+direct-map is ever wanted *and* tested.
        || args.cpu_backend == crate::cli::CpuBackend::Batch
        // `--record`/`--replay`: see this function's own validation above.
        || record_active
        || replay_active;
    let direct_map_wanted = mode_requested && !interceptor_active;
    if direct_map_wanted {
        console.diag(&format!(
            "direct-map: on ({})",
            match args.direct_map {
                crate::cli::DirectMapMode::On => "--direct-map on",
                _ => "auto: release build",
            }
        ));
    } else if mode_requested && interceptor_active {
        console.diag(&format!(
            "direct-map: off (an access-intercepting diagnostic, --cpu-backend batch, or \
             --record/--replay is active; those always win -- see --direct-map's own doc \
             comment){}",
            if record_active {
                " (--record is active)"
            } else if replay_active {
                " (--replay is active)"
            } else {
                ""
            }
        ));
    } else {
        console.diag(&format!(
            "direct-map: off ({})",
            match args.direct_map {
                crate::cli::DirectMapMode::Off => "--direct-map off",
                _ => "auto: debug build",
            }
        ));
    }

    // Heap-allocate: a 2 MB array built on the stack overflows a default
    // thread stack (the same pitfall `machine-core`'s own tests document).
    //
    // When the direct map is wanted, chip RAM is backed by a POSIX
    // shared-memory region instead of a plain `Box` (`directmap.rs`'s
    // module docs, "The RAM aliasing argument"): the region's primary
    // `MAP_SHARED` view is exactly the `&mut [u8; CHIP_RAM_SIZE]`
    // `MachineBus` borrows below, unchanged from the `Box` it replaces,
    // and the *same* region is aliased a second time into the direct
    // map's reservation once that's built further down.
    //
    // Exactly one of `chip_ram_box`/`chip_shm` is populated, chosen once
    // here and never changed; both are declared in this outer scope (not
    // inside the `if`) so whichever one is live outlives `bus` the same
    // way the old unconditional `chip_ram: Box<...>` did -- dropped at
    // the end of this function's scope, after every use of the direct
    // map below. `direct_map_enabled` narrows `direct_map_wanted` by
    // whether the shm allocation actually succeeded; the direct map
    // itself is only built (further down) when this is true.
    let mut chip_ram_box: Option<Box<[u8; CHIP_RAM_SIZE]>> = None;
    let mut chip_shm: Option<crate::directmap::ShmRegion> = None;
    let mut direct_map_enabled = direct_map_wanted;
    if direct_map_wanted {
        match crate::directmap::ShmRegion::create(CHIP_RAM_SIZE) {
            Ok(shm) => chip_shm = Some(shm),
            Err(e) => {
                eprintln!(
                    "direct map: creating chip RAM's shared-memory region failed, falling back \
                     to a plain allocation with the direct map disabled: {e}"
                );
                direct_map_enabled = false;
            }
        }
    }
    if chip_shm.is_none() {
        chip_ram_box = Some(
            match vec![0u8; CHIP_RAM_SIZE].into_boxed_slice().try_into() {
                Ok(b) => b,
                Err(_) => unreachable!("boxed_slice has exactly CHIP_RAM_SIZE elements"),
            },
        );
    }
    // SAFETY (the `as_mut_slice` call only): `chip_shm`, when present, is
    // this function's own fresh region; the returned slice is borrowed
    // for the rest of this function's scope, the same lifetime shape the
    // `Box` branch's `&mut *chip_ram_box` has.
    let chip_ram: &mut [u8; CHIP_RAM_SIZE] = match (&mut chip_shm, &mut chip_ram_box) {
        (Some(shm), None) => (unsafe { shm.as_mut_slice() })
            .try_into()
            .unwrap_or_else(|_| unreachable!("ShmRegion::create(CHIP_RAM_SIZE) sized it")),
        (None, Some(b)) => &mut *b,
        _ => unreachable!("exactly one of chip_shm/chip_ram_box is populated above"),
    };

    // Opened before `machine_bus` (which borrows it, `with_hostblk`'s
    // `&'a mut`) and outside the `Option` match below so the file, once
    // opened, lives long enough regardless of which branch runs.
    let mut hostblk_device = match &args.hostblk {
        Some(path) => match FileBlockDevice::open(path, args.hostblk_writable) {
            Ok(dev) => {
                console.diag(&format!(
                    "hostblk: unit 0 = {} ({} sectors, {})",
                    path.display(),
                    dev.sector_count(),
                    if dev.writable() {
                        "read-write"
                    } else {
                        "read-only"
                    }
                ));
                Some(dev)
            }
            Err(e) => {
                return setup_error(
                    console,
                    format!("opening --hostblk {}: {e}", path.display()),
                )
            }
        },
        None => None,
    };

    // `pktport`'s host-side filesystem backend (`docs/pktport-protocol.md`,
    // ADR 0004): opened the same way `hostblk_device` is above -- before
    // `machine_bus`, outside the `Option` match, so it outlives the bus
    // once the card attaches to it. Unlike `hostblk_device` this is not
    // yet wired to a card: `machine_core::pktport` (the card model and
    // its `MachineBus::with_pktport` builder) is a parallel worker's
    // deliverable and has not landed. `PktVolume` is fully constructed
    // and ready here; only the attachment below is a placeholder.
    let mut pktvol_backend = match &args.pktvol {
        Some(path) => match crate::pktvol::PktVolume::open(path, args.pktvol_writable, None) {
            Ok(vol) => {
                console.diag(&format!(
                    "pktvol: {} ({})",
                    path.display(),
                    if args.pktvol_writable {
                        "read-write"
                    } else {
                        "read-only"
                    }
                ));
                Some(vol)
            }
            Err(e) => {
                return setup_error(console, format!("opening --pktvol {}: {e}", path.display()))
            }
        },
        None => None,
    };

    // Heap-allocated, and opened (allocated) outside the `Option` match
    // below for the same lifetime reason `hostblk_device` is:
    // `with_graphics` borrows it `&'a mut`, so it must outlive
    // `machine_bus`. Only
    // allocated at all when `--graphics` is passed -- a plain boot run
    // pays nothing for it, matching `with_graphics`'s own "absent unless
    // attached" contract.
    let mut graphics_vram: Vec<u8> = if args.graphics {
        vec![0u8; (args.graphics_vram_mb as usize) * 1024 * 1024]
    } else {
        Vec::new()
    };

    // Same "only allocated at all when needed" shape as `graphics_vram`
    // above: `--fast-ram-mb 0` (opt-out; the flag itself defaults to 256,
    // `cli.rs`'s own doc comment) must leave the AUTOCONFIG chain -- and
    // so every baseline that predates fast RAM -- untouched, the same
    // guarantee `MachineBus::with_fast_ram`'s doc comment gives.
    //
    // Same shm-vs-plain-allocation split as chip RAM above, for the same
    // reason (`directmap.rs`'s module docs): only attempted at all when
    // fast RAM is actually requested, since a `0`-byte shm region is a
    // pointless syscall round trip `Vec::new()` already handles for
    // free. `fast_shm`'s fd feeds `DirectMap::new`/`sync` below whenever
    // AUTOCONFIG later places this board; the alias itself is not mapped
    // here (fast RAM isn't placed yet at construction time -- that's a
    // guest AUTOCONFIG write, `directmap.rs`'s module docs on
    // `DirectMap::sync`).
    let mut fast_ram_vec: Vec<u8> = Vec::new();
    let mut fast_shm: Option<crate::directmap::ShmRegion> = None;
    if args.fast_ram_mb > 0 {
        let len = args.fast_ram_mb as usize * 1024 * 1024;
        if direct_map_enabled {
            match crate::directmap::ShmRegion::create(len) {
                Ok(shm) => fast_shm = Some(shm),
                Err(e) => {
                    eprintln!(
                        "direct map: creating fast RAM's shared-memory region failed, falling \
                         back to a plain allocation for fast RAM only (chip RAM keeps whatever \
                         its own attempt already chose): {e}"
                    );
                }
            }
        }
        if fast_shm.is_none() {
            fast_ram_vec = vec![0u8; len];
        }
    }
    // SAFETY (the `as_mut_slice` call only): same reasoning as chip RAM's
    // above -- `fast_shm`, when present, is this function's own fresh
    // region, borrowed for the rest of this scope.
    let fast_ram: &mut [u8] = match &mut fast_shm {
        Some(shm) => unsafe { shm.as_mut_slice() },
        None => &mut fast_ram_vec[..],
    };

    // `--rtgboard`'s geometry, parsed up front (a bad `WIDTHxHEIGHT` is a
    // CLI usage error, reported before any allocation or bus construction
    // happens) -- and its VRAM, allocated outside the `Option` handling
    // below for the same `with_rtgboard`-borrows-it-`&'a mut` lifetime
    // reason `graphics_vram`/`fast_ram` are. Only allocated at all when
    // `--rtgboard` is passed, matching every other optional card's "absent
    // unless attached" contract.
    let rtgboard_geometry = match &args.rtgboard {
        Some(spec) => match crate::cli::parse_rtgboard_geometry(spec) {
            Ok(wh) => Some(wh),
            Err(e) => return setup_error(console, e),
        },
        None => None,
    };
    let rtgboard_format = args.rtgboard_format.to_format_byte();
    let mut rtgboard_vram: Vec<u8> = if rtgboard_geometry.is_some() {
        vec![0u8; args.rtgboard_vram_mb as usize * 1024 * 1024]
    } else {
        Vec::new()
    };
    // A single-entry catalog is this flag's whole point (`cli.rs`'s own
    // doc comment on `--rtgboard`): the honest shape of a fixed-mode
    // Phase 5 board, not a menu this host doesn't actually offer.
    let rtgboard_modes: [machine_core::rtgboard::ModeDescriptor; 1] = match rtgboard_geometry {
        Some((width, height)) => [machine_core::rtgboard::ModeDescriptor {
            width,
            height,
            format: rtgboard_format,
        }],
        None => [machine_core::rtgboard::ModeDescriptor {
            width: 0,
            height: 0,
            format: machine_core::rtgboard::format::INVALID,
        }],
    };
    if let Some((width, height)) = rtgboard_geometry {
        let bpp = machine_core::rtgboard::format::bytes_per_pixel(rtgboard_format)
            .expect("RtgFormatArg::to_format_byte only ever produces a recognised format");
        let needed = (width as u64) * (height as u64) * (bpp as u64);
        if needed > rtgboard_vram.len() as u64 {
            return setup_error(
                console,
                format!(
                    "--rtgboard {width}x{height} at this format needs {needed} bytes of VRAM, \
                     but --rtgboard-vram-mb {} only provides {}",
                    args.rtgboard_vram_mb,
                    rtgboard_vram.len()
                ),
            );
        }
    }

    // `pcibridge`'s host-side virtual PCI topology (ADR 0005 stage 1,
    // `machine_core::pci`/`machine_core::pcibridge`): a QEMU-root-shaped
    // host bridge at 00:00.0 and a config-space-complete virtio-net stub
    // at 00:01.0. Neither device allocates on the heap (unlike
    // `graphics_vram`/`fast_ram`/`rtgboard_vram` above), so -- unlike
    // those -- there is no reason to gate their *construction* behind
    // `args.pcibridge`; only *attaching* them to the bus below is
    // conditional. `pcibridge_vpci` still needs to exist in this frame
    // regardless (`with_pcibridge` borrows it `&'a mut`, machine-core has
    // no allocator), the same lifetime shape every other card's backing
    // storage has here.
    let mut pcibridge_hostbridge = pci::HostBridge::new();
    // ADR 0005 stage 3's NetBackend seam (`docs/virtionet.md`): the
    // harness backend simply replaces `NullNetBackend` whenever
    // `pcibridge` is attached -- there is no dedicated flag for this,
    // because there is no real host network path to choose *instead* of
    // it yet (that remains a later increment's job; `docs/virtionet.md`
    // records a real tap/socket backend as deliberately deferred).
    // `net_harness_log` is the reporting handle `--inspect` reads after
    // the run; `netharness.rs`'s own doc comment explains why it must be
    // a separate `Rc<RefCell<_>>` clone rather than a field read straight
    // off `pcibridge_net_backend` (which stays borrowed by the
    // `pcibridge` chain for as long as `bus` is alive).
    let net_harness_log = std::rc::Rc::new(std::cell::RefCell::new(
        crate::netharness::NetHarnessLog::default(),
    ));
    // The harness's reply cooldown is measured in guest frames, not in
    // `poll_receive` calls (`netharness.rs`'s module docs): `run_guest`
    // keeps this current from `chipset.frames`.
    let guest_frames = std::rc::Rc::new(std::cell::Cell::new(0u64));
    let mut pcibridge_net_backend = crate::netharness::HarnessNetBackend::new(
        std::rc::Rc::clone(&net_harness_log),
        std::rc::Rc::clone(&guest_frames),
    );
    let mut pcibridge_netstub = pci::VirtioNetStub::new(&mut pcibridge_net_backend);
    let mut pcibridge_slots = [
        pci::VirtualSlot {
            bdf: pci::Bdf {
                bus: 0,
                device: 0,
                function: 0,
            },
            device: &mut pcibridge_hostbridge,
        },
        pci::VirtualSlot {
            bdf: pci::Bdf {
                bus: 0,
                device: 1,
                function: 0,
            },
            device: &mut pcibridge_netstub,
        },
    ];
    let mut pcibridge_vpci = pci::VirtualPciBus::new(&mut pcibridge_slots);

    // The recorder's device-write span log (`docs/cpu-core-proposal.md`
    // §5.3, `MachineBus::set_device_write_log`): a caller-owned slice,
    // large enough that a real boot essentially never overflows it between
    // two drains (`run_guest_fixed` drains after every tick). Declared
    // here, before `machine_bus` borrows it, for the same lifetime reason
    // every other card's backing storage above is -- only allocated at all
    // when `--record` is given, matching every other "absent unless
    // attached" contract in this function.
    let mut device_write_log_storage: Vec<(u32, u32)> = if record_active {
        vec![(0, 0); 65536]
    } else {
        Vec::new()
    };

    let machine_bus = MachineBus::new(chip_ram, &rom_bytes);
    let machine_bus = match &ext_rom_bytes {
        Some(ext) => machine_bus.with_ext_rom(ext),
        None => machine_bus,
    };
    let machine_bus = machine_bus.with_floppy(args.floppy.into());
    console.diag(&format!("floppy: {:?}", args.floppy));
    let machine_bus = match &mut hostblk_device {
        // Not writable-by-default matters here too: see
        // `--hostblk-writable`'s doc comment.
        Some(dev) => {
            let write_protect = !dev.writable();
            machine_bus.with_hostblk(0, dev, write_protect)
        }
        None => machine_bus,
    };
    let machine_bus = match &mut pktvol_backend {
        // Borrowed for the bus's lifetime, exactly as `hostblk_device`
        // is above -- `with_pktport` takes `&'a mut dyn PacketBackend`
        // (machine-core has no allocator), so the `PktVolume` itself
        // lives in this frame alongside every other card's storage.
        Some(vol) => machine_bus.with_pktport(vol),
        None => machine_bus,
    };
    let machine_bus = if args.graphics {
        console.diag(&format!(
            "graphics: Graffity attached (Zorro {}), {} MB VRAM",
            args.graphics_bus, args.graphics_vram_mb
        ));
        if args.graphics_bus == 3 {
            machine_bus.with_graphics_zorro_iii(&mut graphics_vram)
        } else {
            machine_bus.with_graphics(&mut graphics_vram)
        }
    } else {
        machine_bus
    };
    // Registered *after* `--graphics`, deliberately: Zorro II and Zorro
    // III boards are placed from separate address pools (Zorro II's
    // 24-bit space, Zorro III's own at $40000000 and up), but a Zorro III
    // Graffity (`--graphics-bus 3`) shares fast RAM's pool, and
    // `expansion.library` configures whichever board is *first in the
    // AUTOCONFIG chain* before moving to the next. Fast RAM after
    // Graffity here means Graffity is always offered first regardless of
    // whether `--fast-ram` is set, so its own placement never depends on
    // fast RAM's presence -- confirmed empirically: with both flags set,
    // Zorro III Graffity still lands at the same base address, and
    // `--graphics-bus 3`'s screenshot baseline stays byte-identical. Were
    // this reversed, fast RAM being 128M-Zorro-III-aligned (it must place
    // on its own size's natural boundary, same as Graffity's Z3 window)
    // could easily push Graffity to a different base and move the RTG
    // baseline -- exactly the risk `docs/device-ledger.md`'s fast RAM row
    // and this flag's own doc comment call out.
    let machine_bus = match args.fast_ram_mb {
        mb if mb > 0 => {
            console.diag(&format!(
                "fast-ram: {mb} MB, Zorro III AUTOCONFIG board (ERTF_MEMLIST)"
            ));
            machine_bus.with_fast_ram(fast_ram)
        }
        _ => machine_bus,
    };
    // Same "only exists when asked for" shape as `--hostblk`/`--graphics`:
    // omitting `--input-script` leaves neither the card's AUTOCONFIG
    // board nor the bus's routing branch registered at all, so a plain
    // boot run is completely unaffected (brief's own requirement).
    let machine_bus = if args.input_script.is_some() {
        console.diag("input: native input card attached (Zorro III AUTOCONFIG)");
        machine_bus.with_input()
    } else {
        machine_bus
    };
    // Same "only exists when asked for" shape again: omitting `--rtgboard`
    // leaves this board's AUTOCONFIG window and the bus's routing branch
    // entirely unregistered (`MachineBus::with_rtgboard`'s own doc
    // comment), so every existing baseline (planar and Graffity RTG
    // alike) is untouched by this flag ever having been added. No P96
    // `.card` driver exists yet (`machine_core::rtgboard`'s module docs) --
    // attaching this board with nothing on the guest to program it is
    // inert, the same shape `--hostblk`'s and `--input-script`'s own
    // first, driver-less increments took.
    let machine_bus = if let Some((width, height)) = rtgboard_geometry {
        console.diag(&format!(
            "rtgboard: native RTG board attached (Zorro III AUTOCONFIG), one advertised mode \
             {width}x{height}, {} MB VRAM",
            args.rtgboard_vram_mb
        ));
        machine_bus.with_rtgboard(&mut rtgboard_vram, &rtgboard_modes)
    } else {
        machine_bus
    };
    // Attached *last* among the AUTOCONFIG registrations, deliberately:
    // every board above this line keeps the exact chain position (and so
    // the exact base address) it had before `pcibridge` existed --
    // `pcibridge_coexists_with_the_full_native_chain_without_moving_anyone`
    // in `machine-core`'s own test suite is written against precisely
    // this ordering, the same reasoning `--fast-ram` registering after
    // `--graphics` documents above. This board carries no DiagArea and no
    // driver (this increment's own scope), so attaching it with no guest
    // software is inert but harmless.
    let machine_bus = if args.pcibridge {
        console.diag(
            "pcibridge: Zorro III PCI shim attached (ADR 0005 stage 1) -- host bridge at \
             00:00.0, virtio-net stub (1af4:1041) at 00:01.0",
        );
        machine_bus.with_pcibridge(&mut pcibridge_vpci)
    } else {
        machine_bus
    };
    // Attached last, once every board above has had its chance to place
    // itself -- `set_device_write_log` never affects placement, but this
    // keeps the "every `.with_*` call happens before anything reads the
    // finished bus" ordering the rest of this function already relies on
    // (`DirectMap::new` below, and `Recorder::create` further down, both
    // read `machine_bus` only after this point).
    let mut machine_bus = machine_bus;
    if record_active {
        machine_bus.set_device_write_log(Some(&mut device_write_log_storage));
    }
    let blitter_trace = match &args.blitter_trace {
        Some(path) => match crate::blitter_trace::BlitterTrace::open(path) {
            Ok(t) => {
                console.diag(&format!("blitter-trace: recording to {}", path.display()));
                Some(t)
            }
            Err(e) => {
                return setup_error(
                    console,
                    format!("opening --blitter-trace {}: {e}", path.display()),
                )
            }
        },
        None => None,
    };
    // Read once here rather than on every bus access -- see `bus.rs`'s
    // `Bus.2` doc comment and `docs/bus-fast-path-plan.md` §3.5.
    let serial_trace_enabled = std::env::var_os("SERIAL_REG_TRACE").is_some();
    let cpu_speed_max = args.cpu_speed == crate::cli::CpuSpeed::Max;
    // `BUS_COVERAGE` (plan step 7.2): read once at construction, same
    // "diagnostic gated once, not re-checked per access" posture as
    // `SERIAL_REG_TRACE` above (`docs/bus-fast-path-plan.md` §3.5).
    let bus_coverage_enabled = std::env::var_os("BUS_COVERAGE").is_some();

    // Built last among the pieces `Bus` bundles, once `machine_bus` is
    // fully wired (every `.with_*` board above has already run) --
    // `directmap.rs`'s classification asks `machine_bus.autoconfig`/
    // `fast_ram_window`/`overlay` directly, so it needs the finished
    // bus, not a partially-built one. `direct_map_enabled` already
    // folded in `--no-direct-map` and the three interceptor flags
    // (computed early, before RAM's allocation strategy needed to know
    // this) plus whether chip RAM's own shm region actually came up; a
    // fast-RAM shm failure does not disable the map (`fast_shm`'s own
    // comment above), it just leaves fast RAM `Io`-only until (if ever)
    // a plain allocation is added for it too -- not needed today since
    // this only degrades gracefully to the pre-existing fallthrough
    // path.
    let mut direct_map: Option<crate::directmap::DirectMap> = None;
    if direct_map_enabled {
        let fast_fd = fast_shm.as_ref().map(|s| s.fd());
        let chip_fd = chip_shm
            .as_ref()
            .expect("direct_map_enabled implies chip_shm creation already succeeded")
            .fd();
        match crate::directmap::DirectMap::new(chip_fd, fast_fd, &rom_bytes, &machine_bus) {
            Ok(dm) => direct_map = Some(dm),
            Err(e) => {
                eprintln!("direct map: construction failed, falling back to disabled: {e}");
            }
        }
    }

    // `--record`/`--replay` (`docs/cpu-core-proposal.md` §5.3): built last
    // among `Bus`'s pieces, once `machine_bus` is fully wired, for the
    // same reason the direct map is -- both need to classify from the
    // finished bus's own AUTOCONFIG/overlay state, not a partially-built
    // one. At most one of the two is ever constructed (`run()`'s own
    // validation above refuses giving both flags).
    // Checkpoint interval: `REPLAY_CHECKPOINT_INTERVAL`, gated the same
    // "read once, diagnostic/test knob, zero cost unless set" way as
    // `MEASURE_INSTR_PER_LINE`/`SERIAL_REG_TRACE` -- exists so the seeded-
    // divergence tests (`docs/replay-log.md`) can tighten the interval to
    // get a checkpoint soon after a deliberately corrupted event, without
    // needing a permanent CLI flag for a value production use never wants
    // tuned.
    let checkpoint_interval = std::env::var("REPLAY_CHECKPOINT_INTERVAL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(crate::replay::DEFAULT_CHECKPOINT_INTERVAL);
    let mut recorder: Option<crate::replay::Recorder> = None;
    if let Some(path) = &args.record {
        match crate::replay::Recorder::create(
            path,
            &machine_bus,
            &rom_bytes,
            args.instructions_per_line.max(1),
            checkpoint_interval,
        ) {
            Ok(r) => {
                console.diag(&format!(
                    "record: writing replay log to {} (docs/replay-log.md)",
                    path.display()
                ));
                recorder = Some(r);
            }
            Err(e) => {
                return setup_error(console, format!("opening --record {}: {e}", path.display()))
            }
        }
    }
    let mut player: Option<crate::replay::Player> = None;
    if let Some(path) = &args.replay {
        match crate::replay::Player::open(path) {
            Ok(p) => {
                let rom_hash = crate::replay::fnv1a64(&rom_bytes);
                if p.header().rom_hash != rom_hash {
                    return setup_error(
                        console,
                        format!(
                            "--replay {}: ROM identity mismatch (log recorded against a ROM \
                             hashing to {:#018x}, this run's ROM hashes to {:#018x}) -- replaying \
                             a log against a different ROM than it was recorded with is not \
                             supported",
                            path.display(),
                            p.header().rom_hash,
                            rom_hash
                        ),
                    );
                }
                if p.header().chip_ram_size != machine_core::CHIP_RAM_SIZE as u32 {
                    return setup_error(
                        console,
                        format!(
                            "--replay {}: chip RAM size mismatch (log recorded {} bytes, this \
                             build has {} bytes -- chip RAM size is architecturally fixed, so \
                             this should never happen outside a build skew)",
                            path.display(),
                            p.header().chip_ram_size,
                            machine_core::CHIP_RAM_SIZE
                        ),
                    );
                }
                let fast_ram_size = machine_bus.fast_ram_window().map(|(_, l)| l).unwrap_or(0);
                if p.header().fast_ram_size != fast_ram_size {
                    return setup_error(
                        console,
                        format!(
                            "--replay {}: fast RAM size mismatch (log recorded {} bytes, this \
                             run has {} bytes attached at header-check time -- pass the same \
                             --fast-ram-mb the recording used)",
                            path.display(),
                            p.header().fast_ram_size,
                            fast_ram_size
                        ),
                    );
                }
                console.diag(&format!(
                    "replay: replaying log from {} (docs/replay-log.md; instructions-per-line \
                     recorded as {})",
                    path.display(),
                    p.header().instructions_per_line
                ));
                player = Some(p);
            }
            Err(e) => {
                return setup_error(console, format!("opening --replay {}: {e}", path.display()))
            }
        }
    }

    let mut bus = Bus(
        machine_bus,
        blitter_trace,
        serial_trace_enabled,
        cpu_speed_max,
        bus_coverage_enabled.then(crate::bus::Coverage::default),
        direct_map,
        recorder,
        player,
    );

    // `--cpu-backend batch` needs `--cpu-speed max`: cycle mode's
    // reproducibility and per-instruction cycle accounting run through
    // `run_for_cycles_with_hook`, which `run_batch` cannot provide (it
    // does not maintain cycle accounting -- `m68k`'s own doc comment).
    if args.cpu_backend == crate::cli::CpuBackend::Batch && !cpu_speed_max {
        return setup_error(
            console,
            "--cpu-backend batch requires --cpu-speed max: run_batch is instruction-budgeted \
             and does not maintain cycle accounting, so cycle mode (which needs exact \
             per-instruction cycle charging) cannot use it"
                .to_string(),
        );
    }

    // `--trace` (and `TRACE_WATCH_PCS`, gated on it) is a per-instruction
    // diagnostic driven from `run_guest`'s `run_for_cycles_with_hook`
    // closure -- ADR 0006's whole point in max mode is running the CPU
    // in hook-free `run_for_cycles` chunks instead, so there is no
    // per-instruction point left to hang a trace off. Refused here,
    // before any ROM or device work, rather than silently ignored.
    if cpu_speed_max && args.trace {
        return setup_error(
            console,
            "--trace is incompatible with --cpu-speed max: max mode runs the CPU in hook-free \
             batches with no per-instruction point to trace from (see run.rs's module docs and \
             docs/adr-0006-cycle-budgeted-and-wall-clock-paced-timing.md)"
                .to_string(),
        );
    }

    // Refused rather than silently prioritised: see `--serial-tcp`'s doc
    // comment on `Args` for why picking a winner between "a live client"
    // and "a fixed scripted sequence" would be a worse answer than making
    // the caller choose one host->guest input source.
    if args.serial_tcp.is_some() && args.serial_script.is_some() {
        return setup_error(
            console,
            "--serial-tcp and --serial-script cannot both be given -- both drive host->guest \
             serial input and only one can be the source of it"
                .to_string(),
        );
    }

    let mut serial_script = match &args.serial_script {
        Some(path) => match SerialScript::load(path) {
            Ok(s) => Some(s),
            Err(e) => {
                return setup_error(
                    console,
                    format!("reading --serial-script {}: {e}", path.display()),
                )
            }
        },
        None => None,
    };

    // Bound *outside* the `Option` match, same lifetime reason as
    // `hostblk_device`/`graphics_vram` above -- `SerialTcpBridge::start`
    // spawns its background thread immediately, so the bridge exists
    // (and its thread is polling) for the rest of this function's scope
    // regardless of which branch below runs.
    let serial_tcp = match &args.serial_tcp {
        Some(addr) => match SerialTcpBridge::start(addr) {
            Ok(bridge) => {
                console.diag(&format!(
                    "serial-tcp: listening on {} -- bidirectional bridge to the guest's \
                     serial port (crate::serial_tcp)",
                    bridge.local_addr()
                ));
                Some(bridge)
            }
            Err(e) => return setup_error(console, format!("binding --serial-tcp {addr}: {e}")),
        },
        None => None,
    };

    let mut input_script = match &args.input_script {
        Some(path) => match InputScript::load(path) {
            Ok(s) => Some(s),
            Err(e) => {
                return setup_error(
                    console,
                    format!("reading --input-script {}: {e}", path.display()),
                )
            }
        },
        None => None,
    };

    let mut cpu = M68kRsCore::new();
    cpu.set_cpu_model(args.cpu);
    cpu.reset(&mut bus);

    console.diag(&format!(
        "reset vector: SSP={:#010x} PC={:#010x} (overlay {})",
        cpu.sp(),
        cpu.pc(),
        if bus.0.overlay() { "mapped" } else { "clear" }
    ));

    let report = if replay_active {
        run_guest_replay(console, &mut cpu, &mut bus)
    } else if cpu_speed_max {
        run_guest_max(
            args,
            console,
            &mut cpu,
            &mut bus,
            serial_script.as_mut(),
            input_script.as_mut(),
            serial_tcp.as_ref(),
            &guest_frames,
        )
    } else if args.cpu_speed == crate::cli::CpuSpeed::Fixed {
        run_guest_fixed(
            args,
            console,
            &mut cpu,
            &mut bus,
            serial_script.as_mut(),
            input_script.as_mut(),
            serial_tcp.as_ref(),
            &guest_frames,
        )
    } else {
        run_guest(
            args,
            console,
            &mut cpu,
            &mut bus,
            serial_script.as_mut(),
            input_script.as_mut(),
            serial_tcp.as_ref(),
            &guest_frames,
        )
    };

    if let Some(script) = &serial_script {
        console.diag(&format!(
            "serial-script: {}",
            if script.is_done() {
                "completed"
            } else {
                "did not finish (run ended first -- see --max-frames/--max-instructions)"
            }
        ));
    }
    if let Some(script) = &input_script {
        console.diag(&format!(
            "input-script: {}",
            if script.is_done() {
                "completed"
            } else {
                "did not finish (run ended first -- see --max-frames/--max-instructions)"
            }
        ));
    }

    // Introspection runs after the guest has stopped moving (whatever the
    // reason), reading whatever state it left behind -- see
    // `introspect.rs`'s doc comment for why guest memory is the only
    // evidence available for a stock Kickstart.
    if args.inspect {
        let exec_report = crate::introspect::inspect(&mut bus.0);
        for line in crate::introspect::format_report(&exec_report).lines() {
            console.diag(line);
        }
        console.diag(&crate::introspect::format_display_state(&bus.0.chipset));
        console.diag(&crate::introspect::format_disk_state(&bus.0));
        if args.graphics {
            console.diag(&crate::introspect::format_graphics_state(&bus.0));
        }
        if args.hostblk.is_some() {
            console.diag(&crate::introspect::format_hostblk_state(
                &mut bus.0,
                exec_report.exec_base,
            ));
        }
        if args.input_script.is_some() {
            console.diag(&crate::introspect::format_input_state(&mut bus.0));
        }
        if args.pcibridge {
            console.diag(&crate::introspect::format_pcibridge_state(&mut bus.0));
            console.diag(&net_harness_log.borrow().format());
        }
    }

    if let Some(trace) = bus.1.take() {
        console.diag(&trace.finish());
    }

    if let Some(coverage) = bus.take_coverage() {
        for line in coverage.format().lines() {
            console.diag(line);
        }
    }

    report
}

#[allow(clippy::too_many_arguments)]
fn run_guest<C: GuestCpu>(
    args: &Args,
    console: &mut Console,
    cpu: &mut C,
    bus: &mut Bus,
    mut serial_script: Option<&mut SerialScript>,
    mut input_script: Option<&mut InputScript>,
    serial_tcp: Option<&SerialTcpBridge>,
    guest_frames: &std::cell::Cell<u64>,
) -> Report {
    let mut total_instructions: u64 = 0;
    let mut last_progress_frame: u64 = 0;
    let mut overlay_was_cleared = false;
    // Frame the overlay first cleared, and the last frame the serial
    // script/illegal-instruction trigger were serviced at -- both are
    // `None` until the guest gets that far, matching `overlay_was_cleared`
    // above.
    let mut overlay_cleared_frame: Option<u64> = None;
    let mut last_serviced_frame: Option<u64> = None;
    let mut illegal_triggered = false;

    // `MEASURE_INSTR_PER_LINE` (`docs/deterministic-mode.md`'s
    // measurement method): sums every cycle actually charged to
    // `MachineBus::tick` from *this* hook -- i.e. only while the CPU is
    // actively retiring instructions, never the `Stopped` arm's own
    // catch-up ticks below -- so `active_clocks / ONE_LINE_CLOCKS` is
    // exactly "raster lines' worth of guest time spent not stopped",
    // and `total_instructions / that` is `--cpu-speed fixed`'s `N`
    // (`docs/cpu-core-proposal.md` §5.2's own definition). Gated behind
    // an env var read once, same posture as `SERIAL_REG_TRACE`/
    // `BUS_COVERAGE` (`docs/bus-fast-path-plan.md` §3.5): a plain cycle-
    // mode run pays one extra `u64` add per retired instruction only
    // when this is set.
    let measure_instr_per_line = std::env::var_os("MEASURE_INSTR_PER_LINE").is_some();
    let mut active_clocks: u64 = 0;

    // Tight-loop detector state, shared across hook invocations within one
    // outer-loop batch (recreated each batch since a fresh closure borrows
    // it fresh, but the values themselves persist across batches). See
    // `LoopWindow`'s own doc comment.
    let mut loop_window = LoopWindow::new(cpu.pc());

    let mut last_exception: Option<(&'static str, u32)> = None;
    let mut exception_streak: u64 = 0;

    // Only built when asked for: a plain boot run never touches the
    // renderer, matching `--screenshot`'s doc comment on `Args`.
    let mut screenshot_job = args.screenshot.as_ref().map(|path| {
        crate::screenshot::ScreenshotJob::new(
            path.clone(),
            args.screenshot_frame,
            args.screenshot_every,
        )
    });
    // Last frame `screenshot_job.maybe_capture` was offered, shared
    // between the hook closure below and the `Stopped` branch's own
    // catch-up call -- `maybe_capture` already has its own cheap
    // "nothing to do yet" check, but on a normal run `frames` is
    // unchanged on the overwhelming majority of calls (many instructions
    // per frame), so skipping the call outright when it hasn't changed
    // avoids paying even that check every retired instruction. The
    // same once-per-frame branch keeps `guest_frames` (the net
    // harness's clock) current.
    let mut screenshot_last_frame: Option<u64> = None;

    let outcome = 'outer: loop {
        let trace = args.trace;
        // `--trace`'s disassembler needs only the CPU model, which `args`
        // already carries -- it decodes the 68k instruction stream, not
        // anything specific to the executing `GuestCpu` implementation, so
        // this does not need to go through the trait (see
        // `docs/cpu-core-trait.md`'s note on why disassembly is not a
        // trait method).
        let cpu_type: m68k::CpuType = args.cpu.into();
        let mut hook_wedge: Option<String> = None;
        let mut hook_limit: Option<&'static str> = None;

        let hook_instructions_before = total_instructions;
        let result = cpu.run_for_cycles_with_hook(bus, RUN_BATCH_CYCLES, |cpu, bus, cycles| {
            let clocks = cycles.max(0) as u32;
            bus.0.tick(clocks);
            if measure_instr_per_line {
                active_clocks += clocks as u64;
            }
            cpu.set_irq(bus.0.pending_irq_level());
            drain_serial(bus, console, serial_tcp);

            let frames = bus.0.frames();
            // Only serviced from here, not from the `Stopped` branch's own
            // catch-up tick below: this hook is guaranteed to run with the
            // CPU actively executing (never `stopped`), which is required
            // for `cpu.take_illegal_exception` below to leave the guest in
            // a coherent post-exception state rather than vectoring a
            // still-`stopped` core (`m68k-rs`'s `take_exception` does not
            // itself clear the stop condition). Kickstart's idle `STOP`
            // uses SR mask 0, so VERTB (level 3, every frame) always wakes
            // it and runs this hook at least once per frame -- the same
            // property `docs/phase0-findings.md`'s `STOP` section
            // documents -- so frame-granularity servicing here does not
            // miss frames even though it never runs while stopped.
            //
            // `service_host_serial` keeps its own `last_serviced_frame`
            // check (needed for the STOP-path resync's own frame
            // bookkeeping, and just cheap defence-in-depth), but checking
            // it here too means the many-instructions-per-frame common
            // case skips the whole call -- args, both scripts, the bridge
            // -- rather than a function call that immediately returns.
            if last_serviced_frame != Some(frames) {
                service_host_serial(
                    args,
                    cpu,
                    bus,
                    console,
                    serial_script.as_deref_mut(),
                    input_script.as_deref_mut(),
                    serial_tcp,
                    &mut overlay_cleared_frame,
                    &mut last_serviced_frame,
                    &mut illegal_triggered,
                );
            }

            total_instructions += 1;
            let pc = cpu.ppc();
            // Feed the serial register trace (bus.rs) the PC of the next
            // instruction to execute, so its accesses get attributed --
            // but only when something could read it back: `LAST_PC` exists
            // purely for `trace_serial`'s diagnostic output, so a plain
            // run with `SERIAL_REG_TRACE` unset has no use for storing it
            // on every retired instruction.
            if bus.serial_trace_on() {
                crate::bus::LAST_PC.store(cpu.pc(), std::sync::atomic::Ordering::Relaxed);
            }

            if trace {
                let opcode = bus.0.read_word(pc);
                let (mnemonic, _) = m68k::dasm::disassemble(pc, opcode, cpu_type);
                console.diag(&format!("{pc:#010x}: {opcode:#06x}  {mnemonic}"));
                // Targeted register watch for guest debugging: TRACE_WATCH_PCS
                // is a comma-separated list of hex PCs; when the traced PC
                // matches, dump D0-D2/A0-A2/A6 so packet/signal plumbing can
                // be followed without a full register trace.
                if let Ok(watch) = std::env::var("TRACE_WATCH_PCS") {
                    if watch
                        .split(',')
                        .filter_map(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
                        .any(|w| w == pc)
                    {
                        console.diag(&format!(
                            "  WATCH {pc:#010x}: D0={:#010x} D1={:#010x} D2={:#010x} A0={:#010x} A1={:#010x} A2={:#010x} A6={:#010x} SP={:#010x}",
                            cpu.dar(0), cpu.dar(1), cpu.dar(2),
                            cpu.dar(8), cpu.dar(9), cpu.dar(10), cpu.dar(14), cpu.dar(15),
                        ));
                    }
                }
            }

            loop_window.extend(pc);
            if loop_window.streak >= TIGHT_LOOP_THRESHOLD {
                hook_wedge = Some(format!(
                    "tight loop in PC range {:#010x}-{:#010x} ({} instructions with no progress)",
                    loop_window.lo, loop_window.hi, loop_window.streak
                ));
                return HookControl::Return;
            }

            if frames >= last_progress_frame + PROGRESS_EVERY_FRAMES {
                last_progress_frame = frames;
                console.diag(&format!(
                    "progress: frame {frames}, PC {:#010x}, overlay {}, INTENA {:#06x}, INTREQ {:#06x}",
                    cpu.pc(),
                    if bus.0.overlay() { "mapped" } else { "clear" },
                    bus.0.chipset.intena,
                    bus.0.chipset.intreq,
                ));
            }

            // Only offered to `maybe_capture` once per changed frame --
            // see `screenshot_last_frame`'s own doc comment.
            if screenshot_last_frame != Some(frames) {
                screenshot_last_frame = Some(frames);
                guest_frames.set(frames);
                if let Some(job) = screenshot_job.as_mut() {
                    job.maybe_capture(frames, args.max_frames, &mut bus.0, console);
                }
            }

            if args.max_instructions != 0 && total_instructions >= args.max_instructions {
                hook_limit = Some("max-instructions");
                return HookControl::Return;
            }
            if args.max_frames != 0 && frames >= args.max_frames {
                hook_limit = Some("max-frames");
                return HookControl::Return;
            }

            HookControl::Continue
        });

        // `result.instructions` double-counts against the hook's own
        // running total (the hook increments per call, which is the more
        // precise source since it also covers the instruction that
        // triggered a `Return`); use the hook's count as ground truth and
        // only fall back to `result.instructions` as a sanity floor.
        if total_instructions < hook_instructions_before + result.instructions as u64 {
            total_instructions = hook_instructions_before + result.instructions as u64;
        }

        if !overlay_was_cleared && !bus.0.overlay() {
            overlay_was_cleared = true;
            console.diag(&format!(
                "PHASE1 HOSTED: reached overlay-cleared (frame {}, instr {total_instructions}, PC {:#010x})",
                bus.0.frames(), cpu.pc()
            ));
        }

        if let Some(reason) = hook_wedge {
            break 'outer Outcome::Wedged(reason);
        }
        if let Some(which) = hook_limit {
            break 'outer Outcome::LimitReached(which);
        }

        match result.exit {
            GuestExit::BudgetExhausted | GuestExit::BoundaryRequested => {
                // Just this batch's cycle allowance running out; the outer
                // loop's own limit checks (above) are what actually stop a
                // run. Keep going.
                continue 'outer;
            }
            GuestExit::Stopped => {
                // STOP is not HALT: it loads SR from its operand and
                // suspends fetch until an *unmasked* interrupt arrives
                // (68000UM4 §6.2), then resumes -- it is Kickstart's idle
                // dispatcher parking until the next VERTB/CIA tick, not a
                // request to end the run. SR mask 7 is the one STOP no
                // level 1-6 source (all this chipset ever requests) can
                // ever satisfy, which is exactly the synthetic smoke-test
                // ROM's contract (`tests/smoke.rs`'s `STOP_SR = 0x2700`) --
                // that, and only that, is a real clean halt.
                if cpu.int_mask() & 0x0700 == 0x0700 {
                    break 'outer Outcome::CleanHalt;
                }

                // Check the frame bound *before* ticking rather than
                // after: one tick here can cross several frame boundaries
                // at once (`RUN_BATCH_CYCLES` is far more than one
                // frame's clocks), and the interrupt it raises deserves a
                // chance to actually reach the CPU on the next
                // `run_for_cycles_with_hook` call before this loop gives
                // up on the run. Enforcing the bound here first (rather
                // than after ticking, which could overshoot straight past
                // it) means a run that is genuinely making progress is
                // never cut off mid-wake, while a run that stays stopped
                // forever still terminates -- the *next* time this arm is
                // reached with the CPU still stopped, the bound has
                // caught up.
                let frames = bus.0.frames();
                if args.max_frames != 0 && frames >= args.max_frames {
                    break 'outer Outcome::LimitReached("max-frames");
                }

                // The machine clock must keep advancing while stopped, or
                // the interrupt that would wake it can never fire:
                // `run_for_cycles_with_hook`'s docs are explicit that its
                // hook "is not called for ... an already-stopped CPU", so
                // nothing else in this loop ticks the bus while `stopped`
                // stays set. Tick it here instead, resample the IPL the
                // same way the hook does, and let the outer loop's next
                // `run_for_cycles_with_hook` call resume the CPU --
                // `run_for_cycles_inner` services a newly serviceable
                // interrupt (including waking a stopped core) before its
                // first fetch on every call.
                //
                // Tick in raster-line slices and stop at the first slice
                // that leaves an interrupt pending, never in one
                // `RUN_BATCH_CYCLES` gulp. A gulp that size spans ~7 PAL
                // frames, and a real 68k in STOP wakes the moment IPL
                // rises -- gulping instead delivered one VERTB per gulp
                // (every VBlank-counted OS timeout ran ~7x slow, so
                // Kickstart's insert-disk screen never came up inside any
                // sane frame budget), collapsed distinct CIA timer
                // underflows into one ICR event, and let a freshly raised
                // VERTB shadow a lower-level CIA interrupt at every
                // resync point.
                let mut budget = RUN_BATCH_CYCLES as u32;
                while budget > 0 {
                    bus.0.tick(ONE_LINE_CLOCKS.min(budget));
                    budget = budget.saturating_sub(ONE_LINE_CLOCKS);
                    if bus.0.pending_irq_level() != 0 {
                        break;
                    }
                }
                cpu.set_irq(bus.0.pending_irq_level());
                drain_serial(bus, console, serial_tcp);

                let frames = bus.0.frames();
                if screenshot_last_frame != Some(frames) {
                    screenshot_last_frame = Some(frames);
                    guest_frames.set(frames);
                    if let Some(job) = screenshot_job.as_mut() {
                        job.maybe_capture(frames, args.max_frames, &mut bus.0, console);
                    }
                }
                if frames >= last_progress_frame + PROGRESS_EVERY_FRAMES {
                    last_progress_frame = frames;
                    console.diag(&format!(
                        "progress: frame {frames}, PC {:#010x} (stopped), overlay {}, INTENA {:#06x}, INTREQ {:#06x}",
                        cpu.pc(),
                        if bus.0.overlay() { "mapped" } else { "clear" },
                        bus.0.chipset.intena,
                        bus.0.chipset.intreq,
                    ));
                }
                continue 'outer;
            }
            GuestExit::AlineTrap { opcode } => {
                if track_exception(
                    "A-line",
                    cpu.ppc(),
                    &mut last_exception,
                    &mut exception_streak,
                ) {
                    break 'outer Outcome::Wedged(format!(
                        "exception storm: A-line trap {opcode:#06x} at PC {:#010x} repeated {exception_streak} times",
                        cpu.ppc()
                    ));
                }
                cpu.take_aline_exception(bus);
            }
            GuestExit::FlineTrap { opcode } => {
                if track_exception(
                    "F-line",
                    cpu.ppc(),
                    &mut last_exception,
                    &mut exception_streak,
                ) {
                    break 'outer Outcome::Wedged(format!(
                        "exception storm: F-line trap {opcode:#06x} at PC {:#010x} repeated {exception_streak} times",
                        cpu.ppc()
                    ));
                }
                cpu.take_fline_exception(bus);
            }
            GuestExit::TrapInstruction { trap_num } => {
                if track_exception(
                    "TRAP",
                    cpu.ppc(),
                    &mut last_exception,
                    &mut exception_streak,
                ) {
                    break 'outer Outcome::Wedged(format!(
                        "exception storm: TRAP #{trap_num} at PC {:#010x} repeated {exception_streak} times",
                        cpu.ppc()
                    ));
                }
                cpu.take_trap_exception(bus, trap_num);
            }
            GuestExit::Breakpoint { bp_num } => {
                if track_exception(
                    "BKPT",
                    cpu.ppc(),
                    &mut last_exception,
                    &mut exception_streak,
                ) {
                    break 'outer Outcome::Wedged(format!(
                        "exception storm: BKPT #{bp_num} at PC {:#010x} repeated {exception_streak} times",
                        cpu.ppc()
                    ));
                }
                cpu.take_bkpt_exception(bus);
            }
            GuestExit::IllegalInstruction { opcode } => {
                if track_exception(
                    "illegal",
                    cpu.ppc(),
                    &mut last_exception,
                    &mut exception_streak,
                ) {
                    break 'outer Outcome::Wedged(format!(
                        "exception storm: illegal opcode {opcode:#06x} at PC {:#010x} repeated {exception_streak} times",
                        cpu.ppc()
                    ));
                }
                cpu.take_illegal_exception(bus);
            }
        }
    };

    if measure_instr_per_line {
        let active_lines = active_clocks as f64 / ONE_LINE_CLOCKS as f64;
        let instr_per_line = if active_lines > 0.0 {
            total_instructions as f64 / active_lines
        } else {
            0.0
        };
        console.diag(&format!(
            "MEASURE_INSTR_PER_LINE: {total_instructions} instructions / {active_lines:.3} \
             active lines (not-stopped) = {instr_per_line:.4} instructions/line"
        ));
    }

    Report {
        outcome,
        instructions: total_instructions,
        frames: bus.0.frames(),
        final_pc: cpu.pc(),
        overlay_cleared: !bus.0.overlay(),
        max_mode_timing: None,
    }
}

/// Guest clocks per real second: the same 14.19 MHz-equivalent rate every
/// device's arithmetic already runs on (`CPU_CLOCKS_PER_COLOUR_CLOCK`'s
/// own doc comment), derived rather than hardcoded so it can never drift
/// out of step with `machine-core`'s own constants -- the PAL E-clock
/// (`machine_core::cia::E_CLOCK_HZ`, 709.379 kHz) times
/// `CPU_CLOCKS_PER_ECLOCK` (20) is exactly 14,187,580 Hz.
fn max_mode_clock_hz() -> f64 {
    machine_core::cia::E_CLOCK_HZ as f64 * CPU_CLOCKS_PER_ECLOCK as f64
}

/// Convert a clock count in `MachineBus`'s CPU-clock-equivalent units
/// into the real-time [`Duration`] it represents under
/// [`max_mode_clock_hz`].
fn duration_from_clocks(clocks: u32, clock_hz: f64) -> Duration {
    Duration::from_secs_f64(clocks as f64 / clock_hz)
}

/// `--cpu-speed max`'s run loop: ADR 0006's wall-clock-paced timing model
/// (`docs/adr-0006-cycle-budgeted-and-wall-clock-paced-timing.md`).
/// Device time follows the wall clock one event at a time instead of
/// retired-instruction cycles, and the CPU runs unbudgeted in hook-free
/// [`CpuCore::run_for_cycles`] chunks between device deadlines --
/// `run_guest`'s hook-driven cycle-mode loop is untouched by any of this
/// (this is a separate function, per the plan's own step 7.1).
///
/// The per-instruction hook's duties move to chunk boundaries
/// (`max_chunk_boundary`): serial/input scripts, screenshots, progress,
/// and the `--max-instructions`/`--max-frames` limits are all checked
/// once per chunk rather than once per instruction. Wedge detection
/// becomes wall-clock-based (`MAX_MODE_WEDGE_SECONDS`) rather than
/// instruction-count-based, since max mode has no fixed instructions per
/// second to size a count against. STOP sleeps the host rather than
/// ticking device time in slices (cycle mode's `GuestExit::Stopped`
/// arm), and a host that falls behind real time slips device time rather
/// than bursting through the backlog (`MAX_MODE_BACKLOG_CAP`).
#[allow(clippy::too_many_arguments)]
fn run_guest_max<C: GuestCpu>(
    args: &Args,
    console: &mut Console,
    cpu: &mut C,
    bus: &mut Bus,
    mut serial_script: Option<&mut SerialScript>,
    mut input_script: Option<&mut InputScript>,
    serial_tcp: Option<&SerialTcpBridge>,
    guest_frames: &std::cell::Cell<u64>,
) -> Report {
    let mut total_instructions: u64 = 0;
    let mut overlay_was_cleared = false;
    let mut overlay_cleared_frame: Option<u64> = None;
    let mut last_serviced_frame: Option<u64> = None;
    let mut illegal_triggered = false;

    let mut last_exception: Option<(&'static str, u32)> = None;
    let mut exception_streak: u64 = 0;

    // Same `LoopWindow` cycle/fixed mode use, but sampled once per chunk
    // boundary (`cpu.pc()` after each chunk, not every retired
    // instruction -- max mode has no per-instruction hook to sample
    // from) and checked against wall-clock time rather than an
    // instruction count, since max mode has no fixed instructions per
    // second to size a count-based threshold from.
    let mut loop_window = LoopWindow::new(cpu.pc());
    let mut wedge_since = Instant::now();

    let mut screenshot_job = args.screenshot.as_ref().map(|path| {
        crate::screenshot::ScreenshotJob::new(
            path.clone(),
            args.screenshot_frame,
            args.screenshot_every,
        )
    });
    let mut screenshot_last_frame: Option<u64> = None;
    let mut last_progress_real = Instant::now();

    let backend = args.cpu_backend;
    // Step 7.2's baseline metrics (a gap in 7.1: its only MIPS figure
    // averaged in STOP sleep time and never measured wall clock at all).
    // `slept` accumulates the *measured elapsed time* of the STOP-path
    // sleep below (an `Instant::now()` taken immediately before each
    // `std::thread::sleep` call, not the nominal slice requested --
    // `std::thread::sleep` routinely overshoots, and crediting the
    // request undercounts sleep / overcounts busy; see the comment at the
    // accumulation site). Everything else this function does is "busy" by
    // definition, including the outer loop's own bookkeeping and the
    // serial-tcp bridge poll between the top of this loop and the sleep
    // call -- `Report::timing_report` reports busy MIPS from
    // `wall_start.elapsed() - slept`.
    let wall_start = Instant::now();
    let mut slept = Duration::ZERO;

    let clock_hz = max_mode_clock_hz();
    // The wall-clock instant device time is caught up to. Advanced by
    // exactly one event's worth of real time per iteration (ADR 0006
    // step 3), never reset to `Instant::now()` except by the backlog cap
    // below -- resetting it every iteration would be "catch up", which
    // the ADR explicitly rejects in favour of slipping.
    let mut anchor = Instant::now();
    // Adaptive chunk size in CPU cycles, corrected from the previous
    // chunk's measured rate every iteration -- seeded low (a fast host's
    // first chunk undershoots `MAX_MODE_CHUNK_TARGET_US`, which only
    // costs one extra `run_for_cycles` call before the estimate catches
    // up) rather than high (which could blow the latency bound on a slow
    // host's very first chunk). Used only by the `interp` back end.
    let mut chunk_cycles: i32 = 4_000;
    // The `batch` back end's equivalent, in instructions rather than
    // cycles (`run_batch` reports no cycle count -- `MAX_MODE_MIN_CHUNK_INSTRS`'s
    // own doc comment on why this never touches `cycles_remaining`).
    let mut chunk_instrs: u32 = 1_000;

    let outcome = 'outer: loop {
        let deadline = bus.0.next_event_deadline_clocks();
        let t_event = anchor + duration_from_clocks(deadline, clock_hz);

        let mut hook_wedge: Option<String> = None;
        let mut hook_limit: Option<&'static str> = None;
        let mut stopped_exit = false;

        // Run the CPU in adaptive chunks until the host clock reaches
        // `t_event`, the CPU stops, or a boundary request ends a chunk
        // early (ADR 0006 step 2). The first chunk always runs regardless
        // of whether `t_event` has already passed -- the "minimum CPU
        // share between events" guarantee (ADR 0006, "When the host
        // can't keep up") falls out of this being a do/while rather than
        // a while loop.
        loop {
            let chunk_start = Instant::now();
            let result: GuestBatchResult = match backend {
                crate::cli::CpuBackend::Interp => {
                    let budget =
                        chunk_cycles.clamp(MAX_MODE_MIN_CHUNK_CYCLES, MAX_MODE_MAX_CHUNK_CYCLES);
                    cpu.run_for_cycles(bus, budget)
                }
                crate::cli::CpuBackend::Batch => {
                    let budget =
                        chunk_instrs.clamp(MAX_MODE_MIN_CHUNK_INSTRS, MAX_MODE_MAX_CHUNK_INSTRS);
                    // `run_batch_instructions` reports no cycle count at
                    // all (`.cycles` is always `0`); nothing below reads
                    // it for the `Batch` arm (the chunk-size update below
                    // is instruction-based instead), and device time is
                    // advanced from `deadline`, never from this field, in
                    // both back ends. This is the one `GuestCpu` method
                    // that is not required of a second core -- see its
                    // doc comment and `docs/cpu-core-trait.md`.
                    cpu.run_batch_instructions(bus, budget)
                }
            };
            let chunk_elapsed = chunk_start.elapsed();

            total_instructions += result.instructions as u64;

            // Correct the chunk-size estimate from this chunk's actual
            // rate -- skipped when nothing ran (e.g. an already-stopped
            // CPU's zero-cycle exit), which would corrupt the estimate
            // rather than refine it. Each back end adapts its own chunk
            // unit (cycles for `interp`, instructions for `batch`) from
            // its own measured rate; neither reads the other's budget or
            // `cycles_remaining`.
            match backend {
                crate::cli::CpuBackend::Interp => {
                    if result.cycles > 0 && chunk_elapsed > Duration::ZERO {
                        let cycles_per_us =
                            result.cycles as f64 / chunk_elapsed.as_secs_f64() / 1e6;
                        let target = (cycles_per_us * MAX_MODE_CHUNK_TARGET_US).round();
                        if target.is_finite() {
                            chunk_cycles = (target as i64).clamp(
                                MAX_MODE_MIN_CHUNK_CYCLES as i64,
                                MAX_MODE_MAX_CHUNK_CYCLES as i64,
                            ) as i32;
                        }
                    }
                }
                crate::cli::CpuBackend::Batch => {
                    if result.instructions > 0 && chunk_elapsed > Duration::ZERO {
                        let instrs_per_us =
                            result.instructions as f64 / chunk_elapsed.as_secs_f64() / 1e6;
                        let target = (instrs_per_us * MAX_MODE_CHUNK_TARGET_US).round();
                        if target.is_finite() {
                            chunk_instrs = (target as i64).clamp(
                                MAX_MODE_MIN_CHUNK_INSTRS as i64,
                                MAX_MODE_MAX_CHUNK_INSTRS as i64,
                            ) as u32;
                        }
                    }
                }
            }

            // The IPL must be current before the next fetch regardless of
            // why this chunk ended -- there is no per-instruction hook to
            // do this after every retired instruction the way cycle
            // mode's does, so it happens once per chunk instead (the
            // "check interval" bound on interrupt latency, ADR 0006).
            cpu.set_irq(bus.0.pending_irq_level());
            drain_serial(bus, console, serial_tcp);

            // "With the CPU not stopped" (ADR 0006): a `Stopped` exit
            // retires no instructions and leaves `cpu.pc()` unchanged by
            // definition (Kickstart's idle dispatcher parks on the same
            // STOP over and over, exactly like a real wedge would look),
            // so it must reset the streak rather than extend it -- an
            // idle Workbench sitting in STOP for longer than
            // `MAX_MODE_WEDGE_SECONDS` is the expected, healthy case this
            // detector must never fire on.
            if result.exit == GuestExit::Stopped {
                loop_window.reset(cpu.pc());
                wedge_since = Instant::now();
            } else if loop_window.extend(cpu.pc()) {
                let stuck_for = wedge_since.elapsed();
                if stuck_for.as_secs_f64() >= MAX_MODE_WEDGE_SECONDS {
                    hook_wedge = Some(format!(
                        "tight loop in PC range {:#010x}-{:#010x} ({:.1}s wall clock with no progress)",
                        loop_window.lo,
                        loop_window.hi,
                        stuck_for.as_secs_f64()
                    ));
                }
            } else {
                wedge_since = Instant::now();
            }

            let frames = max_chunk_boundary(
                args,
                console,
                cpu,
                bus,
                serial_script.as_deref_mut(),
                input_script.as_deref_mut(),
                serial_tcp,
                guest_frames,
                &mut screenshot_job,
                &mut overlay_was_cleared,
                &mut overlay_cleared_frame,
                &mut last_serviced_frame,
                &mut illegal_triggered,
                &mut screenshot_last_frame,
                &mut last_progress_real,
                total_instructions,
            );

            if hook_wedge.is_none() {
                if args.max_instructions != 0 && total_instructions >= args.max_instructions {
                    hook_limit = Some("max-instructions");
                } else if args.max_frames != 0 && frames >= args.max_frames {
                    hook_limit = Some("max-frames");
                }
            }

            if hook_wedge.is_some() || hook_limit.is_some() {
                break;
            }

            match result.exit {
                GuestExit::BudgetExhausted => {
                    if Instant::now() >= t_event {
                        break;
                    }
                }
                GuestExit::BoundaryRequested => break,
                GuestExit::Stopped => {
                    stopped_exit = true;
                    break;
                }
                GuestExit::AlineTrap { opcode } => {
                    if track_exception(
                        "A-line",
                        cpu.ppc(),
                        &mut last_exception,
                        &mut exception_streak,
                    ) {
                        hook_wedge = Some(format!(
                            "exception storm: A-line trap {opcode:#06x} at PC {:#010x} repeated {exception_streak} times",
                            cpu.ppc()
                        ));
                        break;
                    }
                    cpu.take_aline_exception(bus);
                    if Instant::now() >= t_event {
                        break;
                    }
                }
                GuestExit::FlineTrap { opcode } => {
                    if track_exception(
                        "F-line",
                        cpu.ppc(),
                        &mut last_exception,
                        &mut exception_streak,
                    ) {
                        hook_wedge = Some(format!(
                            "exception storm: F-line trap {opcode:#06x} at PC {:#010x} repeated {exception_streak} times",
                            cpu.ppc()
                        ));
                        break;
                    }
                    cpu.take_fline_exception(bus);
                    if Instant::now() >= t_event {
                        break;
                    }
                }
                GuestExit::TrapInstruction { trap_num } => {
                    if track_exception(
                        "TRAP",
                        cpu.ppc(),
                        &mut last_exception,
                        &mut exception_streak,
                    ) {
                        hook_wedge = Some(format!(
                            "exception storm: TRAP #{trap_num} at PC {:#010x} repeated {exception_streak} times",
                            cpu.ppc()
                        ));
                        break;
                    }
                    cpu.take_trap_exception(bus, trap_num);
                    if Instant::now() >= t_event {
                        break;
                    }
                }
                GuestExit::Breakpoint { bp_num } => {
                    if track_exception(
                        "BKPT",
                        cpu.ppc(),
                        &mut last_exception,
                        &mut exception_streak,
                    ) {
                        hook_wedge = Some(format!(
                            "exception storm: BKPT #{bp_num} at PC {:#010x} repeated {exception_streak} times",
                            cpu.ppc()
                        ));
                        break;
                    }
                    cpu.take_bkpt_exception(bus);
                    if Instant::now() >= t_event {
                        break;
                    }
                }
                GuestExit::IllegalInstruction { opcode } => {
                    if track_exception(
                        "illegal",
                        cpu.ppc(),
                        &mut last_exception,
                        &mut exception_streak,
                    ) {
                        hook_wedge = Some(format!(
                            "exception storm: illegal opcode {opcode:#06x} at PC {:#010x} repeated {exception_streak} times",
                            cpu.ppc()
                        ));
                        break;
                    }
                    cpu.take_illegal_exception(bus);
                    if Instant::now() >= t_event {
                        break;
                    }
                }
            }
        }

        if let Some(reason) = hook_wedge {
            break 'outer Outcome::Wedged(reason);
        }
        if let Some(which) = hook_limit {
            break 'outer Outcome::LimitReached(which);
        }

        if stopped_exit {
            // See `run_guest`'s matching `GuestExit::Stopped` arm for
            // why SR mask 7 (and only that) is a real clean halt.
            if cpu.int_mask() & 0x0700 == 0x0700 {
                break 'outer Outcome::CleanHalt;
            }
            if args.max_frames != 0 && bus.0.frames() >= args.max_frames {
                break 'outer Outcome::LimitReached("max-frames");
            }

            // STOP sleeps rather than busy-ticking: park the host until
            // this event's deadline or host input, whichever is first, in
            // slices no longer than `MAX_MODE_STOP_SLEEP_SLICE` (ADR
            // 0006's STOP sleep). A delivered serial-tcp byte both raises
            // RBF (waking STOP the moment device time catches up to it)
            // and is queued for the guest to read once resumed -- fed
            // here rather than only from `service_host_serial`'s own
            // running-CPU path, since nothing else drains the bridge
            // while stopped.
            loop {
                let now = Instant::now();
                if now >= t_event {
                    break;
                }
                if let Some(bridge) = serial_tcp {
                    if bus.0.chipset.serial_in_has_room() {
                        if let Some(byte) = bridge.try_recv_host_byte() {
                            bus.0.chipset.push_serial_in_byte(byte);
                            break;
                        }
                    }
                }
                let slice = (t_event - now).min(MAX_MODE_STOP_SLEEP_SLICE);
                let sleep_started = Instant::now();
                std::thread::sleep(slice);
                // Measured elapsed time, not the nominal `slice` requested:
                // `std::thread::sleep` routinely overshoots its request (OS
                // scheduler granularity), and crediting the request instead
                // of the overshoot silently reclassifies that overshoot as
                // "busy" in `MaxModeTiming::busy()` below. That is exactly
                // the bug this comment used to describe as correct --
                // measured on a boot workload, self-reported busy=33.6-33.9s
                // of wall=87.86s (38.2%) against an independent 1 kHz
                // `samply` profile's true CPU-busy of 3.8-4.0%, about an
                // order of magnitude off. See docs/cpu-core-c0-profile.md.
                slept += sleep_started.elapsed();
            }

            // Advance device time event by event up to the real now, the
            // same one-event-at-a-time discipline the running path uses
            // above (never several events in one gulp -- this module's
            // doc comment on why cycle mode's own STOP resync learned
            // that the hard way), capped by the same backlog limit.
            if let Some(behind) = Instant::now().checked_duration_since(anchor) {
                if behind > MAX_MODE_BACKLOG_CAP {
                    anchor = Instant::now() - MAX_MODE_BACKLOG_CAP;
                }
            }
            loop {
                let d = bus.0.next_event_deadline_clocks();
                let this_t_event = anchor + duration_from_clocks(d, clock_hz);
                if this_t_event > Instant::now() {
                    break;
                }
                bus.0.tick(d);
                anchor = this_t_event;
                if bus.0.pending_irq_level() != 0 {
                    break;
                }
            }
            cpu.set_irq(bus.0.pending_irq_level());
            drain_serial(bus, console, serial_tcp);

            // `run_batch` cannot wake a stopped CPU on its own: its own
            // stopped check (the fork's `run_batch_inner`) returns
            // `Stopped` immediately whenever `cpu.stopped != 0`, with no
            // call to the interrupt-driven wake check `run_for_cycles`/
            // `execute` make unconditionally before *their* stopped
            // check. Confirmed empirically before this fix: a max-mode
            // boot under `--cpu-backend batch` parked at Kickstart's idle
            // STOP by frame ~3650 and never advanced again for the rest
            // of a 6500-frame run (final PC frozen, busy MIPS near zero
            // from the outer loop spinning on `Stopped` results). A
            // minimal interpreter probe after every device-time advance
            // gives the core's own wake-on-serviceable-interrupt logic
            // (`stopped_supervisor_check`) the chance `run_batch` never
            // does; it costs nothing when the CPU is not yet serviceable
            // (`execute`'s own stopped branch returns without consuming
            // cycles in that case) and only takes real work exactly when
            // a wake is due, in cycle mode's `GuestExit::Stopped` arm
            // and the interp back end never need this because both drive
            // `run_for_cycles`, which already includes this check.
            if backend == crate::cli::CpuBackend::Batch {
                let wake = cpu.run_for_cycles(bus, 4);
                total_instructions += wake.instructions as u64;
            }
            continue 'outer;
        }

        // Advance device time by exactly this event's deadline (ADR 0006
        // step 3) regardless of which reason above ended the chunk loop --
        // `deadline` was fixed before the chunk loop ran, so a chunk that
        // ended early (a boundary request, or an exception mid-event)
        // still advances device time the same amount a chunk that ran to
        // `t_event` would have.
        bus.0.tick(deadline);
        anchor += duration_from_clocks(deadline, clock_hz);
        // Backlog cap (ADR 0006, "When the host can't keep up"): never
        // let device time fall more than `MAX_MODE_BACKLOG_CAP` behind
        // the wall clock. Discards the excess rather than replaying it.
        if let Some(behind) = Instant::now().checked_duration_since(anchor) {
            if behind > MAX_MODE_BACKLOG_CAP {
                anchor = Instant::now() - MAX_MODE_BACKLOG_CAP;
            }
        }
        cpu.set_irq(bus.0.pending_irq_level());
    };

    Report {
        outcome,
        instructions: total_instructions,
        frames: bus.0.frames(),
        final_pc: cpu.pc(),
        overlay_cleared: !bus.0.overlay(),
        max_mode_timing: Some(MaxModeTiming {
            wall: wall_start.elapsed(),
            slept,
        }),
    }
}

/// `--cpu-speed fixed`'s run loop (`docs/cpu-core-proposal.md` §5.2,
/// `docs/deterministic-mode.md`): retire exactly `args.instructions_per_line`
/// instructions per raster line, advancing the beam, both CIAs and the
/// per-line device engines by exactly one line's worth of clocks
/// ([`ONE_LINE_CLOCKS`]) in total over that line -- never in one lump
/// sum at the line's end. While the CPU is stopped, lines advance with
/// no instructions retired -- the same shape `run_guest`'s own
/// `GuestExit::Stopped` arm already uses for its STOP-path resync (both
/// tick a whole line in one call there, since nothing is executing to
/// observe the intermediate state).
///
/// Structurally this is `run_guest` with one change: the per-instruction
/// hook never calls [`MachineBus::tick`] with a real per-instruction
/// cycle cost (the `cycles` value `run_for_cycles_with_hook` hands the
/// hook is not read at all here). Instead it ticks a *fixed,
/// position-in-line* quantum every instruction -- `ONE_LINE_CLOCKS`
/// split across the line's `instructions_per_line` instructions by
/// exact integer rasterization (the hook's own comment has the
/// arithmetic), so the quantum depends only on which instruction *index*
/// within the line just retired, never on which opcode it was. That is
/// what makes this mode reproducible on a core with no cycle tables:
/// nothing in this function, or in the hook it installs, ever consults
/// an instruction's timing cost.
///
/// **Why not tick once at line end (the first version of this
/// function):** it hangs. Ticking `ONE_LINE_CLOCKS` in a single call
/// after the line's last instruction is an exact multiple of the line
/// period, so the beam's horizontal position (`hpos`, read back via
/// custom chip registers like `VHPOSR`) returns to *exactly* the same
/// value after every such tick -- it can never be observed to move by
/// guest code executing between ticks. Confirmed empirically, not just
/// derived: a real Kickstart 3.2.2 A1200 boot hung at a `MOVE.W
/// (A4),D1` / `CMP.B` / `BLS` beam-position poll (`A4 = $DFF006`,
/// `VHPOSR`) for over 870 million loop iterations with the polled
/// value's low byte pinned at `$00` throughout -- a real, hardware-
/// timing-calibrated delay loop, not a synthetic edge case. Ticking
/// incrementally, spread across the line, gives `hpos` genuine
/// intra-line movement for exactly this kind of code to observe, while
/// still summing to exactly `ONE_LINE_CLOCKS` per line and never reading
/// a per-instruction cycle cost. With this fix, the same boot reaches
/// the same idle `STOP` at the same final PC (`0x00f8131c`) cycle mode
/// reaches, at nearly the same instruction count (`docs/
/// deterministic-mode.md` has the full before/after).
///
/// `instr_this_line` is declared outside the `'outer` loop and carried
/// across iterations exactly the way `run_guest`'s own `total_instructions`/
/// `loop_window` are: a fresh closure borrows it by
/// reference every iteration, but the count itself persists. It is only
/// reset to zero when a line is actually advanced (quota met, or a STOP
/// tick) -- an exception taken mid-line (A-line/F-line/TRAP/BKPT/
/// illegal) does not reset it, so instructions retired before the trap
/// still count toward that same line's quota once execution resumes;
/// device time for whatever instructions did retire before the trap has
/// already been ticked, instruction by instruction, by the hook above.
#[allow(clippy::too_many_arguments)]
fn run_guest_fixed<C: GuestCpu>(
    args: &Args,
    console: &mut Console,
    cpu: &mut C,
    bus: &mut Bus,
    mut serial_script: Option<&mut SerialScript>,
    mut input_script: Option<&mut InputScript>,
    serial_tcp: Option<&SerialTcpBridge>,
    guest_frames: &std::cell::Cell<u64>,
) -> Report {
    let instructions_per_line = args.instructions_per_line.max(1);
    let n64 = instructions_per_line as u64;

    let mut total_instructions: u64 = 0;
    let mut instr_this_line: u32 = 0;
    let mut last_progress_frame: u64 = 0;
    let mut overlay_was_cleared = false;
    let mut overlay_cleared_frame: Option<u64> = None;
    let mut last_serviced_frame: Option<u64> = None;
    let mut illegal_triggered = false;

    // Tight-loop detector: same contract as `run_guest`'s own
    // `loop_window` -- a real idle wait always STOPs rather than
    // spinning in a small PC range (`docs/phase0-findings.md`), so a long
    // streak confined to a tiny range with the CPU actively retiring
    // instructions is a wedge here exactly as it is in cycle mode.
    let mut loop_window = LoopWindow::new(cpu.pc());

    let mut last_exception: Option<(&'static str, u32)> = None;
    let mut exception_streak: u64 = 0;

    let mut screenshot_job = args.screenshot.as_ref().map(|path| {
        crate::screenshot::ScreenshotJob::new(
            path.clone(),
            args.screenshot_frame,
            args.screenshot_every,
        )
    });
    let mut screenshot_last_frame: Option<u64> = None;

    // Ad hoc debugging aid (not present in `run_guest`'s own hook,
    // `TRACE_WATCH_PCS` there is gated on `--trace`): parsed once,
    // independent of `--trace`, so a specific PC's registers can be
    // dumped the first few times it's hit without paying the
    // disassembler's cost on every instruction of a long run.
    let watch_pcs: Option<Vec<u32>> = std::env::var("TRACE_WATCH_PCS").ok().map(|watch| {
        watch
            .split(',')
            .filter_map(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
            .collect()
    });

    let outcome = 'outer: loop {
        let trace = args.trace;
        let cpu_type: m68k::CpuType = args.cpu.into();
        let mut hook_wedge: Option<String> = None;
        let mut hook_limit: Option<&'static str> = None;
        let mut hook_line_done = false;

        let hook_instructions_before = total_instructions;
        let mut hook_record_overflow = false;
        let result = cpu.run_for_cycles_with_hook(
            bus,
            FIXED_MODE_BATCH_CYCLES,
            |cpu, bus, _cycles| {
                // `docs/cpu-core-proposal.md` §5.3's ordinal (`docs/replay-
                // log.md`'s worked example): the *first* action of every
                // hook invocation, mirrored by the identical bump in the
                // `Stopped` arm below. `None` when `--record` wasn't
                // given, so this costs one `Option` check on the hot path
                // -- `bus.6`'s own doc comment's standing contract.
                if let Some(rec) = &mut bus.6 {
                    rec.advance_ordinal();
                }

                // Deliberately not reading `_cycles`: see this function's
                // own doc comment on why fixed mode never consults a
                // per-instruction timing cost.
                //
                // Advance device time by a *fixed, position-in-line*
                // quantum -- never a per-instruction cycle cost --
                // spread evenly across the line's `instructions_per_line`
                // instructions rather than ticked in one lump sum after
                // the last one. This is load-bearing, not cosmetic: see
                // this function's own doc comment's "why not tick once
                // at line end" section for the bug this replaced (ticking
                // exactly `ONE_LINE_CLOCKS` in one call every time made
                // the beam's horizontal position mathematically
                // invariant -- an exact multiple of the line period
                // always returns `hpos` to the same value -- which hangs
                // any guest code polling `VHPOSR` for a threshold, a real
                // Kickstart delay-loop pattern, not a synthetic one).
                // `cumulative_before`/`cumulative_after` is exact
                // integer rasterization (a Bresenham-style split): the
                // per-instruction amount depends only on the
                // instruction's *position* within the line (`i`), so a
                // core with no cycle tables can compute it identically,
                // and the sum across exactly `instructions_per_line`
                // calls is exactly `ONE_LINE_CLOCKS` by construction
                // (the telescoping sum collapses to
                // `cumulative(N) - cumulative(0) = ONE_LINE_CLOCKS`).
                instr_this_line += 1;
                let i = instr_this_line as u64;
                let cumulative_before = (ONE_LINE_CLOCKS as u64 * (i - 1)) / n64;
                let cumulative_after = (ONE_LINE_CLOCKS as u64 * i) / n64;
                let amount = (cumulative_after - cumulative_before) as u32;
                bus.0.tick(amount);
                cpu.set_irq(bus.0.pending_irq_level());

                // Recorder bookkeeping, in the same order §5.3 lists:
                // IPL (on change), then the device-write spans this tick
                // may have produced (hostblk/pktport/pcibridge DMA via
                // `ram_slice_mut`, and the blitter's own synchronous
                // chip-RAM writes -- both drained the same way, since
                // both go through `MachineBus::device_write_spans`), then
                // a checkpoint if this ordinal is due one. Exact only
                // because fixed mode ticks once per instruction
                // (`docs/replay-log.md`'s own caveat for cycle/max mode).
                if bus.6.is_some() {
                    let level = bus.0.pending_irq_level();
                    if let Some(rec) = &mut bus.6 {
                        let _ = rec.log_ipl_if_changed(level);
                    }
                    let spans: Vec<(u32, u32)> = bus.0.device_write_spans().to_vec();
                    for (addr, len) in spans {
                        if let Some(bytes) = bus.0.ram_slice(addr, len) {
                            let bytes = bytes.to_vec();
                            if let Some(rec) = &mut bus.6 {
                                let _ = rec.log_device_write(addr, &bytes);
                            }
                        }
                    }
                    if bus.0.device_write_log_overflowed() {
                        hook_record_overflow = true;
                    }
                    bus.0.clear_device_write_spans();
                    let regs = crate::replay::regs_of(cpu);
                    if let Some(rec) = &mut bus.6 {
                        let _ = rec.maybe_checkpoint(regs);
                    }
                    if hook_record_overflow {
                        return HookControl::Return;
                    }
                }

                drain_serial(bus, console, serial_tcp);

                let frames = bus.0.frames();
                if last_serviced_frame != Some(frames) {
                    service_host_serial(
                        args,
                        cpu,
                        bus,
                        console,
                        serial_script.as_deref_mut(),
                        input_script.as_deref_mut(),
                        serial_tcp,
                        &mut overlay_cleared_frame,
                        &mut last_serviced_frame,
                        &mut illegal_triggered,
                    );
                }

                total_instructions += 1;
                let pc = cpu.ppc();
                if bus.serial_trace_on() {
                    crate::bus::LAST_PC.store(cpu.pc(), std::sync::atomic::Ordering::Relaxed);
                }

                if trace {
                    let opcode = bus.0.read_word(pc);
                    let (mnemonic, _) = m68k::dasm::disassemble(pc, opcode, cpu_type);
                    console.diag(&format!("{pc:#010x}: {opcode:#06x}  {mnemonic}"));
                }
                if let Some(watch) = watch_pcs.as_ref() {
                    if watch.contains(&pc) {
                        console.diag(&format!(
                            "  WATCH {pc:#010x}: D0={:#010x} D1={:#010x} D2={:#010x} A0={:#010x} A1={:#010x} A2={:#010x} A6={:#010x} SP={:#010x}",
                            cpu.dar(0), cpu.dar(1), cpu.dar(2),
                            cpu.dar(8), cpu.dar(9), cpu.dar(10), cpu.dar(14), cpu.dar(15),
                        ));
                    }
                }

                loop_window.extend(pc);
                if loop_window.streak >= TIGHT_LOOP_THRESHOLD {
                    hook_wedge = Some(format!(
                        "tight loop in PC range {:#010x}-{:#010x} ({} instructions with no progress)",
                        loop_window.lo, loop_window.hi, loop_window.streak
                    ));
                    return HookControl::Return;
                }

                if frames >= last_progress_frame + PROGRESS_EVERY_FRAMES {
                    last_progress_frame = frames;
                    console.diag(&format!(
                        "progress: frame {frames}, PC {:#010x}, overlay {}, INTENA {:#06x}, INTREQ {:#06x}",
                        cpu.pc(),
                        if bus.0.overlay() { "mapped" } else { "clear" },
                        bus.0.chipset.intena,
                        bus.0.chipset.intreq,
                    ));
                }

                if screenshot_last_frame != Some(frames) {
                    screenshot_last_frame = Some(frames);
                    guest_frames.set(frames);
                    if let Some(job) = screenshot_job.as_mut() {
                        job.maybe_capture(frames, args.max_frames, &mut bus.0, console);
                    }
                }

                if args.max_instructions != 0 && total_instructions >= args.max_instructions {
                    hook_limit = Some("max-instructions");
                    return HookControl::Return;
                }
                if args.max_frames != 0 && frames >= args.max_frames {
                    hook_limit = Some("max-frames");
                    return HookControl::Return;
                }

                if instr_this_line >= instructions_per_line {
                    hook_line_done = true;
                    return HookControl::Return;
                }

                HookControl::Continue
            },
        );

        if total_instructions < hook_instructions_before + result.instructions as u64 {
            total_instructions = hook_instructions_before + result.instructions as u64;
        }

        // `docs/cpu-core-proposal.md` §5.3's recording contract: an
        // overflowed device-write log is not survivable -- the log would
        // silently be missing writes a replayer needs, which is worse
        // than not recording at all. Abort loudly and immediately, never
        // "keep going with a known-incomplete log" (the milestone brief's
        // own words).
        if hook_record_overflow {
            eprintln!(
                "record: device-write log overflowed (capacity exhausted between two drains) -- \
                 aborting the recording; the log so far is incomplete and must not be replayed"
            );
            std::process::exit(4);
        }

        if !overlay_was_cleared && !bus.0.overlay() {
            overlay_was_cleared = true;
            console.diag(&format!(
                "PHASE1 HOSTED: reached overlay-cleared (frame {}, instr {total_instructions}, PC {:#010x})",
                bus.0.frames(), cpu.pc()
            ));
        }

        if let Some(reason) = hook_wedge {
            break 'outer Outcome::Wedged(reason);
        }
        if let Some(which) = hook_limit {
            break 'outer Outcome::LimitReached(which);
        }

        if hook_line_done {
            // The line's instruction quota was met. Device time has
            // *already* been advanced by exactly `ONE_LINE_CLOCKS` in
            // total -- spread across the line's instructions by the
            // hook's own per-instruction tick above, not applied here in
            // one lump sum (see the hook's own comment on why that
            // matters). Nothing left to do but reset the per-line
            // counter and start the next line.
            instr_this_line = 0;
            continue 'outer;
        }

        match result.exit {
            GuestExit::BudgetExhausted | GuestExit::BoundaryRequested => {
                // The line's quota was not yet met (`hook_line_done` is
                // `false`); this hooked batch simply returned early for
                // some other reason (the generous `FIXED_MODE_BATCH_CYCLES`
                // budget, in practice never reached -- see its own doc
                // comment). Keep accumulating toward this same line's
                // quota -- device time for whatever instructions did
                // retire in this batch has already been ticked by the
                // hook itself, instruction by instruction.
                continue 'outer;
            }
            GuestExit::Stopped => {
                if cpu.int_mask() & 0x0700 == 0x0700 {
                    break 'outer Outcome::CleanHalt;
                }

                let frames = bus.0.frames();
                if args.max_frames != 0 && frames >= args.max_frames {
                    break 'outer Outcome::LimitReached("max-frames");
                }

                // No instructions retired this line (`instr_this_line`
                // is already whatever it was before the CPU stopped --
                // 0 on a fresh line, since STOP only ever exits a hooked
                // batch with nothing retired at all per
                // `run_for_cycles_with_hook`'s own contract). Advance
                // exactly one line anyway, matching §5.2's "while
                // stopped, lines advance with no instructions retired".
                //
                // `docs/cpu-core-proposal.md` §5.3's ordinal also advances
                // here -- this is the *other* of the two bump points the
                // module doc names (`crate::replay`'s "Ordinal" section):
                // IPL changes that arrive purely from a STOP-path resync
                // (no instruction retiring at all) still need a key to be
                // logged/replayed at.
                if let Some(rec) = &mut bus.6 {
                    rec.advance_ordinal();
                }
                bus.0.tick(ONE_LINE_CLOCKS);
                cpu.set_irq(bus.0.pending_irq_level());
                if bus.6.is_some() {
                    let level = bus.0.pending_irq_level();
                    if let Some(rec) = &mut bus.6 {
                        let _ = rec.log_ipl_if_changed(level);
                    }
                    let spans: Vec<(u32, u32)> = bus.0.device_write_spans().to_vec();
                    for (addr, len) in spans {
                        if let Some(bytes) = bus.0.ram_slice(addr, len) {
                            let bytes = bytes.to_vec();
                            if let Some(rec) = &mut bus.6 {
                                let _ = rec.log_device_write(addr, &bytes);
                            }
                        }
                    }
                    let overflowed = bus.0.device_write_log_overflowed();
                    bus.0.clear_device_write_spans();
                    let regs = crate::replay::regs_of(cpu);
                    if let Some(rec) = &mut bus.6 {
                        let _ = rec.maybe_checkpoint(regs);
                    }
                    if overflowed {
                        eprintln!(
                            "record: device-write log overflowed (capacity exhausted between two \
                             drains) -- aborting the recording; the log so far is incomplete and \
                             must not be replayed"
                        );
                        std::process::exit(4);
                    }
                }
                drain_serial(bus, console, serial_tcp);

                let frames = bus.0.frames();
                if screenshot_last_frame != Some(frames) {
                    screenshot_last_frame = Some(frames);
                    guest_frames.set(frames);
                    if let Some(job) = screenshot_job.as_mut() {
                        job.maybe_capture(frames, args.max_frames, &mut bus.0, console);
                    }
                }
                if frames >= last_progress_frame + PROGRESS_EVERY_FRAMES {
                    last_progress_frame = frames;
                    console.diag(&format!(
                        "progress: frame {frames}, PC {:#010x} (stopped), overlay {}, INTENA {:#06x}, INTREQ {:#06x}",
                        cpu.pc(),
                        if bus.0.overlay() { "mapped" } else { "clear" },
                        bus.0.chipset.intena,
                        bus.0.chipset.intreq,
                    ));
                }
                continue 'outer;
            }
            GuestExit::AlineTrap { opcode } => {
                if track_exception(
                    "A-line",
                    cpu.ppc(),
                    &mut last_exception,
                    &mut exception_streak,
                ) {
                    break 'outer Outcome::Wedged(format!(
                        "exception storm: A-line trap {opcode:#06x} at PC {:#010x} repeated {exception_streak} times",
                        cpu.ppc()
                    ));
                }
                cpu.take_aline_exception(bus);
            }
            GuestExit::FlineTrap { opcode } => {
                if track_exception(
                    "F-line",
                    cpu.ppc(),
                    &mut last_exception,
                    &mut exception_streak,
                ) {
                    break 'outer Outcome::Wedged(format!(
                        "exception storm: F-line trap {opcode:#06x} at PC {:#010x} repeated {exception_streak} times",
                        cpu.ppc()
                    ));
                }
                cpu.take_fline_exception(bus);
            }
            GuestExit::TrapInstruction { trap_num } => {
                if track_exception(
                    "TRAP",
                    cpu.ppc(),
                    &mut last_exception,
                    &mut exception_streak,
                ) {
                    break 'outer Outcome::Wedged(format!(
                        "exception storm: TRAP #{trap_num} at PC {:#010x} repeated {exception_streak} times",
                        cpu.ppc()
                    ));
                }
                cpu.take_trap_exception(bus, trap_num);
            }
            GuestExit::Breakpoint { bp_num } => {
                if track_exception(
                    "BKPT",
                    cpu.ppc(),
                    &mut last_exception,
                    &mut exception_streak,
                ) {
                    break 'outer Outcome::Wedged(format!(
                        "exception storm: BKPT #{bp_num} at PC {:#010x} repeated {exception_streak} times",
                        cpu.ppc()
                    ));
                }
                cpu.take_bkpt_exception(bus);
            }
            GuestExit::IllegalInstruction { opcode } => {
                if track_exception(
                    "illegal",
                    cpu.ppc(),
                    &mut last_exception,
                    &mut exception_streak,
                ) {
                    break 'outer Outcome::Wedged(format!(
                        "exception storm: illegal opcode {opcode:#06x} at PC {:#010x} repeated {exception_streak} times",
                        cpu.ppc()
                    ));
                }
                cpu.take_illegal_exception(bus);
            }
        }
    };

    if let Some(rec) = &mut bus.6 {
        let regs = crate::replay::regs_of(cpu);
        if let Err(e) = rec.finish(total_instructions, regs, &bus.0) {
            eprintln!("record: failed to write the log's End record: {e}");
            std::process::exit(4);
        }
        console.diag(&format!(
            "record: {} ordinals, {} io-reads, {} io-writes, {} ipl-events, {} device-writes, \
             {} classification-transitions, {} checkpoints",
            rec.ordinal(),
            rec.io_read_events,
            rec.io_write_events,
            rec.ipl_events,
            rec.device_write_events,
            rec.transition_events,
            rec.checkpoint_events,
        ));
    }

    Report {
        outcome,
        instructions: total_instructions,
        frames: bus.0.frames(),
        final_pc: cpu.pc(),
        overlay_cleared: !bus.0.overlay(),
        max_mode_timing: None,
    }
}

/// First differing field between two [`crate::replay::Regs`], for a
/// divergence report -- `"none"` if they're equal (callers only call this
/// once they already know the two differ, so this arm is defensive, not
/// expected).
fn first_reg_diff(expected: &crate::replay::Regs, actual: &crate::replay::Regs) -> &'static str {
    const DN: [&str; 8] = ["D0", "D1", "D2", "D3", "D4", "D5", "D6", "D7"];
    const AN: [&str; 8] = ["A0", "A1", "A2", "A3", "A4", "A5", "A6", "A7"];
    for (i, name) in DN.iter().enumerate() {
        if expected.d[i] != actual.d[i] {
            return name;
        }
    }
    for (i, name) in AN.iter().enumerate() {
        if expected.a[i] != actual.a[i] {
            return name;
        }
    }
    if expected.pc != actual.pc {
        return "PC";
    }
    if expected.sr != actual.sr {
        return "SR";
    }
    "none"
}

/// The replayer (`docs/cpu-core-proposal.md` §5.3, `docs/replay-log.md`):
/// structurally a stripped [`run_guest_fixed`] with every live-device
/// concern removed. No chipset devices are ever ticked, no IPL is ever
/// derived from live state (`MachineBus::pending_irq_level` is never
/// called), no serial/input scripts run, and no screenshots are taken --
/// everything the guest sees comes from the log (`bus.7`, wired into
/// `bus.rs`'s `AddressBus` impl) or from direct `GuestMemory` access on
/// this run's own RAM.
///
/// The outer loop's only job is to keep calling
/// `run_for_cycles_with_hook` (so the CPU re-executes the exact same
/// instruction stream the recording did) and to stop -- cleanly, at the
/// log's `Event::End`, or with a divergence report -- exactly once one of
/// those two things happens. There is no `--max-frames`/`--max-instructions`
/// bound here: replay is bounded by the log itself, which a real
/// recording always terminates with an `End` record (`Recorder::finish`).
fn run_guest_replay<C: GuestCpu>(console: &mut Console, cpu: &mut C, bus: &mut Bus) -> Report {
    let mut total_instructions: u64 = 0;
    let mut end_info: Option<(u64, u64, crate::replay::Regs)> = None;

    let outcome = 'outer: loop {
        let mut hook_end = false;
        let mut hook_divergence = false;

        let hook_instructions_before = total_instructions;
        let result =
            cpu.run_for_cycles_with_hook(bus, FIXED_MODE_BATCH_CYCLES, |cpu, bus, _cycles| {
                let effects = {
                    let player = bus
                        .7
                        .as_mut()
                        .expect("run_guest_replay is only ever called with a player attached");
                    player.advance_ordinal();
                    match player.drain_side_effects(&mut bus.0) {
                        Ok(e) => e,
                        Err(io_err) => {
                            player.record_divergence(crate::replay::Divergence::LogIoError(
                                io_err.to_string(),
                            ));
                            crate::replay::DrainedEffects::default()
                        }
                    }
                };
                if let Some(level) = effects.ipl {
                    cpu.set_irq(level);
                }
                total_instructions += 1;
                if let Some((ordinal, expected)) = effects.checkpoint {
                    let actual = crate::replay::regs_of(cpu);
                    if expected != actual {
                        let diff = first_reg_diff(&expected, &actual);
                        bus.7.as_mut().unwrap().record_divergence(
                            crate::replay::Divergence::CheckpointMismatch {
                                ordinal,
                                expected,
                                actual,
                                first_diff: diff,
                            },
                        );
                    }
                }
                if let Some(end) = effects.end {
                    end_info = Some(end);
                    hook_end = true;
                    return HookControl::Return;
                }
                if bus.7.as_ref().unwrap().divergence().is_some() {
                    hook_divergence = true;
                    return HookControl::Return;
                }
                HookControl::Continue
            });

        if total_instructions < hook_instructions_before + result.instructions as u64 {
            total_instructions = hook_instructions_before + result.instructions as u64;
        }

        if hook_end {
            break 'outer Outcome::CleanHalt; // overwritten below once we've compared final state
        }
        if hook_divergence {
            break 'outer Outcome::Wedged(format!(
                "replay divergence: {}",
                bus.7.as_ref().unwrap().divergence().unwrap()
            ));
        }

        match result.exit {
            GuestExit::BudgetExhausted | GuestExit::BoundaryRequested => continue 'outer,
            GuestExit::Stopped => {
                if cpu.int_mask() & 0x0700 == 0x0700 {
                    break 'outer Outcome::CleanHalt;
                }
                let player = bus.7.as_mut().expect("player attached");
                player.advance_ordinal();
                let effects = match player.drain_side_effects(&mut bus.0) {
                    Ok(e) => e,
                    Err(io_err) => {
                        player.record_divergence(crate::replay::Divergence::LogIoError(
                            io_err.to_string(),
                        ));
                        crate::replay::DrainedEffects::default()
                    }
                };
                if let Some(level) = effects.ipl {
                    cpu.set_irq(level);
                }
                if let Some(end) = effects.end {
                    end_info = Some(end);
                    break 'outer Outcome::CleanHalt;
                }
                if let Some(d) = bus.7.as_ref().unwrap().divergence() {
                    break 'outer Outcome::Wedged(format!("replay divergence: {d}"));
                }
                continue 'outer;
            }
            GuestExit::AlineTrap { .. } => {
                cpu.take_aline_exception(bus);
            }
            GuestExit::FlineTrap { .. } => {
                cpu.take_fline_exception(bus);
            }
            GuestExit::TrapInstruction { trap_num } => {
                cpu.take_trap_exception(bus, trap_num);
            }
            GuestExit::Breakpoint { .. } => {
                cpu.take_bkpt_exception(bus);
            }
            GuestExit::IllegalInstruction { .. } => {
                cpu.take_illegal_exception(bus);
            }
        }
    };

    // A clean `End` still gets one last check: final registers must match
    // exactly (the milestone brief's "at End, compare final state").
    let outcome = if let Outcome::CleanHalt = outcome {
        if let Some((_, _, expected)) = end_info {
            let actual = crate::replay::regs_of(cpu);
            if expected != actual {
                let diff = first_reg_diff(&expected, &actual);
                if let Some(player) = bus.7.as_mut() {
                    player.record_divergence(crate::replay::Divergence::FinalStateMismatch {
                        expected,
                        actual,
                        first_diff: diff,
                    });
                }
            }
        }
        match bus.7.as_ref().and_then(|p| p.divergence()) {
            Some(d) => Outcome::Wedged(format!("replay divergence: {d}")),
            None => {
                let player = bus.7.as_ref().expect("player attached");
                Outcome::ReplayClean {
                    ordinals: player.ordinal(),
                    io_reads: player.io_read_events_consumed,
                    io_writes: player.io_write_events_consumed,
                    ipl_events: player.ipl_events_applied,
                    device_writes: player.device_writes_applied,
                    transitions: player.transitions_applied,
                    checkpoints: player.checkpoints_compared,
                }
            }
        }
    } else {
        outcome
    };

    if let Some(player) = bus.7.as_ref() {
        console.diag(&format!(
            "replay: {} ordinals, {} io-reads consumed, {} io-writes consumed, {} ipl-events, \
             {} device-writes, {} classification-transitions, {} checkpoints",
            player.ordinal(),
            player.io_read_events_consumed,
            player.io_write_events_consumed,
            player.ipl_events_applied,
            player.device_writes_applied,
            player.transitions_applied,
            player.checkpoints_compared,
        ));
    }

    Report {
        outcome,
        instructions: total_instructions,
        frames: bus.0.frames(),
        final_pc: cpu.pc(),
        overlay_cleared: !bus.0.overlay(),
        max_mode_timing: None,
    }
}

/// `run_guest_max`'s chunk-boundary duties: the per-instruction hook's
/// work (serial/input scripts, screenshots, the overlay-cleared marker,
/// progress output), run once per chunk instead. Returns the current
/// frame count, which the caller already needs for its own limit check
/// right after calling this.
#[allow(clippy::too_many_arguments)]
fn max_chunk_boundary<C: GuestCpu>(
    args: &Args,
    console: &mut Console,
    cpu: &mut C,
    bus: &mut Bus,
    serial_script: Option<&mut SerialScript>,
    input_script: Option<&mut InputScript>,
    serial_tcp: Option<&SerialTcpBridge>,
    guest_frames: &std::cell::Cell<u64>,
    screenshot_job: &mut Option<crate::screenshot::ScreenshotJob>,
    overlay_was_cleared: &mut bool,
    overlay_cleared_frame: &mut Option<u64>,
    last_serviced_frame: &mut Option<u64>,
    illegal_triggered: &mut bool,
    screenshot_last_frame: &mut Option<u64>,
    last_progress_real: &mut Instant,
    total_instructions: u64,
) -> u64 {
    let frames = bus.0.frames();

    if *last_serviced_frame != Some(frames) {
        service_host_serial(
            args,
            cpu,
            bus,
            console,
            serial_script,
            input_script,
            serial_tcp,
            overlay_cleared_frame,
            last_serviced_frame,
            illegal_triggered,
        );
    }

    if !*overlay_was_cleared && !bus.0.overlay() {
        *overlay_was_cleared = true;
        console.diag(&format!(
            "PHASE1 HOSTED: reached overlay-cleared (frame {frames}, instr {total_instructions}, PC {:#010x})",
            cpu.pc()
        ));
    }

    if *screenshot_last_frame != Some(frames) {
        *screenshot_last_frame = Some(frames);
        guest_frames.set(frames);
        if let Some(job) = screenshot_job.as_mut() {
            job.maybe_capture(frames, args.max_frames, &mut bus.0, console);
        }
    }

    // Progress is guest-frame-paced in cycle mode (`PROGRESS_EVERY_FRAMES`)
    // because guest time there has no fixed relationship to the wall
    // clock; in max mode guest and wall time are the same thing by
    // design, so pacing progress by real seconds elapsed is the direct
    // equivalent.
    if last_progress_real.elapsed() >= Duration::from_secs(1) {
        *last_progress_real = Instant::now();
        console.diag(&format!(
            "progress (max): frame {frames}, PC {:#010x}, overlay {}, INTENA {:#06x}, INTREQ {:#06x}",
            cpu.pc(),
            if bus.0.overlay() { "mapped" } else { "clear" },
            bus.0.chipset.intena,
            bus.0.chipset.intreq,
        ));
    }

    frames
}

/// Drain any bytes the guest has written to `SERDAT` since the last call
/// and hand them to the console as guest output.
///
/// Phase 1's only observable evidence of reaching the boot menu is this
/// serial channel (roadmap Phase 1 exit criterion, `docs/combined-
/// roadmap.md`): `Chipset::write` already buffers `SERDAT` bytes
/// (`crates/machine-core/src/chipset.rs`'s `push_serial_byte`/
/// `take_serial_byte`), but nothing drained that buffer before this --
/// `Console::guest_byte` existed and was already wired for exactly this,
/// just never called.
#[inline]
fn drain_serial(bus: &mut Bus, console: &mut Console, serial_tcp: Option<&SerialTcpBridge>) {
    // The overwhelming majority of calls (once per retired instruction)
    // find nothing: guest serial writes are rare compared to instruction
    // throughput. `has_serial_byte` is one field compare, so checking it
    // before ever touching `console`/`serial_tcp` skips the whole drain
    // -- including this call's own stack setup for the loop below -- on
    // every one of those calls.
    if !bus.0.chipset.has_serial_byte() {
        return;
    }
    while let Some(byte) = bus.0.chipset.take_serial_byte() {
        console.guest_byte(byte);
        // Composes with the console tee unconditionally: a connected
        // `--serial-tcp` client sees exactly the same guest bytes stdout/
        // `--serial-log` do, never a filtered or delayed subset.
        if let Some(bridge) = serial_tcp {
            bridge.push_guest_byte(byte);
        }
    }
}

/// Drive the optional `--serial-script`, `--input-script` and
/// `--trigger-illegal-after-frames` once per changed chipset frame. Only
/// called from the per-instruction hook -- see that call site's comment
/// for why the CPU is guaranteed to be actively executing (not
/// `stopped`) there, which `cpu.take_illegal_exception` needs.
#[allow(clippy::too_many_arguments)]
fn service_host_serial<H: HookCpu>(
    args: &Args,
    cpu: &mut H,
    bus: &mut Bus,
    console: &mut Console,
    script: Option<&mut SerialScript>,
    input_script: Option<&mut InputScript>,
    serial_tcp: Option<&SerialTcpBridge>,
    overlay_cleared_frame: &mut Option<u64>,
    last_serviced_frame: &mut Option<u64>,
    illegal_triggered: &mut bool,
) {
    let frame = bus.0.frames();
    if !bus.0.overlay() && overlay_cleared_frame.is_none() {
        *overlay_cleared_frame = Some(frame);
    }

    if let Some(target) = args.trigger_illegal_after_frames {
        if !*illegal_triggered {
            if let Some(cleared) = *overlay_cleared_frame {
                if frame >= cleared.saturating_add(target) {
                    console.diag(&format!(
                        "PHASE1 HOSTED: forcing illegal-instruction exception at frame \
                         {frame} (--trigger-illegal-after-frames {target}) to reach the \
                         alert/LED-blink loop"
                    ));
                    cpu.take_illegal_exception(bus);
                    *illegal_triggered = true;
                }
            }
        }
    }

    if *last_serviced_frame == Some(frame) {
        return; // already serviced this frame
    }
    *last_serviced_frame = Some(frame);
    if let Some(script) = script {
        script.tick(frame, &mut bus.0.chipset, console);
    }
    // `input_script`/`bus.0.input_mut()` are independently optional: a
    // script only ever exists when `--input-script` registered the card
    // (`run`'s own gating), so this can only tick a script against a
    // real card, never a dangling one against no card at all.
    if let (Some(script), Some(dev)) = (input_script, bus.0.input_mut()) {
        script.tick(frame, dev);
    }
    // One byte per frame, exactly `SerialScript`'s own `SEND` pace, and
    // for the same underlying reason even though the two callers arrived
    // at it differently: `SerialScript` throttles itself against
    // `serial_in_has_room` to avoid tripping the chipset's overrun path;
    // this throttles even though room is available, because
    // `push_serial_in_byte` raises the RBF interrupt (68k level 5) once
    // per accepted byte (its own doc comment is explicit this is
    // deliberate, so an interrupt-driven reader doesn't stop after the
    // first byte of several). Draining the whole 32-byte queue in one
    // service call -- which a fast client left unthrottled easily fills,
    // unlike a human typing or `SerialScript`'s own one-directive-at-a-
    // time pace -- fires that many RBF interrupts back-to-back. Confirmed
    // empirically, not theoretically: an unthrottled drain here reliably
    // wedged the real Kickstart 3.2.2 ROMWack break-in test into an
    // exception storm instead of ever reaching the debugger, while this
    // one-byte throttle reaches it the same as the scripted version does.
    if let Some(bridge) = serial_tcp {
        if bus.0.chipset.serial_in_has_room() {
            if let Some(byte) = bridge.try_recv_host_byte() {
                bus.0.chipset.push_serial_in_byte(byte);
            }
        }
    }
}

/// Update the exception-storm tracker; returns `true` once the same kind
/// of exception has fired at the same PC `EXCEPTION_STORM_THRESHOLD`
/// times in a row.
fn track_exception(
    kind: &'static str,
    pc: u32,
    last: &mut Option<(&'static str, u32)>,
    streak: &mut u64,
) -> bool {
    if *last == Some((kind, pc)) {
        *streak += 1;
    } else {
        *last = Some((kind, pc));
        *streak = 1;
    }
    *streak >= EXCEPTION_STORM_THRESHOLD
}
