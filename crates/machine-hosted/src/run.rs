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

use std::path::Path;

use m68k::{CpuCore, CycleBatchControl, CycleBatchExit};

use machine_core::gayle::BlockDevice;
use machine_core::{MachineBus, CHIP_RAM_SIZE};

use crate::bus::Bus;
use crate::cli::Args;
use crate::console::Console;
use crate::hd_image::FileBlockDevice;
use crate::rom_image;
use crate::serial_script::SerialScript;

/// CPU cycles requested per `run_for_cycles_with_hook` call. Chosen well
/// under `i32::MAX` (so a long-running batch can never overflow the
/// budget accounting) and coarse enough that outer-loop bookkeeping
/// (limit checks, frame-count sampling) stays cheap relative to the
/// per-instruction hook, which is where the real per-instruction work
/// (tick, IRQ sync, trace, wedge detection) happens.
const RUN_BATCH_CYCLES: i32 = 2_000_000;

/// Report progress roughly once a second of guest (PAL) time.
const PROGRESS_EVERY_FRAMES: u64 = 50;

/// Instructions retired at an unchanging PC before this is called a
/// wedge rather than a legitimate busy-wait (real idle loops in Kickstart
/// *do* spin on one PC waiting for VERTB -- this threshold is well above
/// one frame's instruction count so it does not fire on that).
const TIGHT_LOOP_THRESHOLD: u64 = 20_000_000;

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
    /// see `run_guest`'s handling of `CycleBatchExit::Stopped`.
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
}

pub struct Report {
    pub outcome: Outcome,
    pub instructions: u64,
    pub frames: u64,
    pub final_pc: u32,
    pub overlay_cleared: bool,
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
        }
    }

    /// The final `PHASE1 HOSTED: ...` line, CI-greppable.
    pub fn status_line(&self) -> String {
        let detail = match &self.outcome {
            Outcome::CleanHalt => "CPU HALTED CLEANLY".to_string(),
            Outcome::LimitReached(which) => format!("LIMIT REACHED ({which})"),
            Outcome::Wedged(reason) => format!("WEDGED ({reason})"),
            Outcome::SetupError(reason) => format!("SETUP ERROR ({reason})"),
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
}

fn setup_error(console: &mut Console, reason: String) -> Report {
    console.diag(&format!("machine-hosted: {reason}"));
    Report {
        outcome: Outcome::SetupError(reason),
        instructions: 0,
        frames: 0,
        final_pc: 0,
        overlay_cleared: false,
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

    let rom_bytes = match load_rom("main", &args.rom) {
        Ok(b) => b,
        Err(e) => return setup_error(console, e),
    };
    rom_image::report_identify(console, "main", &rom_bytes);

    let ext_rom_bytes = match &args.ext_rom {
        Some(path) => {
            console.diag(&format!("ext ROM: {}", path.display()));
            match load_rom("ext", path) {
                Ok(b) => {
                    rom_image::report_identify(console, "ext", &b);
                    Some(b)
                }
                Err(e) => return setup_error(console, e),
            }
        }
        None => None,
    };

    // Heap-allocate: a 2 MB array built on the stack overflows a default
    // thread stack (the same pitfall `machine-core`'s own tests document).
    let mut chip_ram: Box<[u8; CHIP_RAM_SIZE]> =
        match vec![0u8; CHIP_RAM_SIZE].into_boxed_slice().try_into() {
            Ok(b) => b,
            Err(_) => unreachable!("boxed_slice has exactly CHIP_RAM_SIZE elements"),
        };

    // Opened before `machine_bus` (which borrows it, `with_hd`'s `&'a mut`)
    // and outside the `Option` match below so the file, once opened, lives
    // long enough regardless of which branch runs.
    let mut hd_device = match &args.hd {
        Some(path) => match FileBlockDevice::open(path, args.hd_writable) {
            Ok(dev) => {
                console.diag(&format!(
                    "hd: {} ({} sectors, {})",
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
            Err(e) => return setup_error(console, format!("opening --hd {}: {e}", path.display())),
        },
        None => None,
    };

    // Heap-allocated, and opened (allocated) outside the `Option` match
    // below for the same lifetime reason `hd_device` is: `with_graphics`
    // borrows it `&'a mut`, so it must outlive `machine_bus`. Only
    // allocated at all when `--graphics` is passed -- a plain boot run
    // pays nothing for it, matching `with_graphics`'s own "absent unless
    // attached" contract.
    let mut graphics_vram: Vec<u8> = if args.graphics {
        vec![0u8; (args.graphics_vram_mb as usize) * 1024 * 1024]
    } else {
        Vec::new()
    };

    let machine_bus = MachineBus::new(&mut chip_ram, &rom_bytes);
    let machine_bus = match &ext_rom_bytes {
        Some(ext) => machine_bus.with_ext_rom(ext),
        None => machine_bus,
    };
    let machine_bus = machine_bus.with_floppy(args.floppy.into());
    console.diag(&format!("floppy: {:?}", args.floppy));
    let machine_bus = match &mut hd_device {
        Some(dev) => machine_bus.with_hd(dev),
        None => machine_bus,
    };
    let machine_bus = if args.graphics {
        console.diag(&format!(
            "graphics: Graffity attached, {} MB VRAM",
            args.graphics_vram_mb
        ));
        machine_bus.with_graphics(&mut graphics_vram)
    } else {
        machine_bus
    };
    let mut bus = Bus(machine_bus);

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

    let mut cpu = CpuCore::new();
    cpu.set_cpu_type(args.cpu.into());
    cpu.reset(&mut bus);

    console.diag(&format!(
        "reset vector: SSP={:#010x} PC={:#010x} (overlay {})",
        cpu.sp(),
        cpu.pc,
        if bus.0.overlay() { "mapped" } else { "clear" }
    ));

    let report = run_guest(args, console, &mut cpu, &mut bus, serial_script.as_mut());

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
    }

    report
}

fn run_guest(
    args: &Args,
    console: &mut Console,
    cpu: &mut CpuCore,
    bus: &mut Bus,
    mut serial_script: Option<&mut SerialScript>,
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

    // Tight-loop detector state, shared across hook invocations within one
    // outer-loop batch (recreated each batch since a fresh closure borrows
    // it fresh, but the values themselves persist across batches).
    let mut last_pc: u32 = cpu.pc;
    let mut same_pc_streak: u64 = 0;

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

    let outcome = 'outer: loop {
        let trace = args.trace;
        let cpu_type = cpu.cpu_type;
        let mut hook_wedge: Option<String> = None;
        let mut hook_limit: Option<&'static str> = None;

        let hook_instructions_before = total_instructions;
        let result = cpu.run_for_cycles_with_hook(bus, RUN_BATCH_CYCLES, |cpu, bus, cycles| {
            bus.0.tick(cycles.max(0) as u32);
            cpu.set_irq(bus.0.pending_irq_level());
            drain_serial(bus, console);
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
            service_host_serial(
                args,
                cpu,
                bus,
                console,
                serial_script.as_deref_mut(),
                &mut overlay_cleared_frame,
                &mut last_serviced_frame,
                &mut illegal_triggered,
            );

            total_instructions += 1;
            let pc = cpu.ppc;

            if trace {
                let opcode = bus.0.read_word(pc);
                let (mnemonic, _) = m68k::dasm::disassemble(pc, opcode, cpu_type);
                console.diag(&format!("{pc:#010x}: {opcode:#06x}  {mnemonic}"));
            }

            if pc == last_pc {
                same_pc_streak += 1;
            } else {
                last_pc = pc;
                same_pc_streak = 1;
            }
            if same_pc_streak >= TIGHT_LOOP_THRESHOLD {
                hook_wedge = Some(format!(
                    "tight loop at PC {pc:#010x} ({same_pc_streak} instructions with no progress)"
                ));
                return CycleBatchControl::Return;
            }

            let frames = bus.0.chipset.frames;
            if frames >= last_progress_frame + PROGRESS_EVERY_FRAMES {
                last_progress_frame = frames;
                console.diag(&format!(
                    "progress: frame {frames}, PC {:#010x}, overlay {}, INTENA {:#06x}, INTREQ {:#06x}",
                    cpu.pc,
                    if bus.0.overlay() { "mapped" } else { "clear" },
                    bus.0.chipset.intena,
                    bus.0.chipset.intreq,
                ));
            }

            if let Some(job) = screenshot_job.as_mut() {
                job.maybe_capture(frames, args.max_frames, &mut bus.0, console);
            }

            if total_instructions >= args.max_instructions {
                hook_limit = Some("max-instructions");
                return CycleBatchControl::Return;
            }
            if frames >= args.max_frames {
                hook_limit = Some("max-frames");
                return CycleBatchControl::Return;
            }

            CycleBatchControl::Continue
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
                bus.0.chipset.frames, cpu.pc
            ));
        }

        if let Some(reason) = hook_wedge {
            break 'outer Outcome::Wedged(reason);
        }
        if let Some(which) = hook_limit {
            break 'outer Outcome::LimitReached(which);
        }

        match result.exit {
            CycleBatchExit::BudgetExhausted | CycleBatchExit::BoundaryRequested => {
                // Just this batch's cycle allowance running out; the outer
                // loop's own limit checks (above) are what actually stop a
                // run. Keep going.
                continue 'outer;
            }
            CycleBatchExit::Stopped => {
                // STOP is not HALT: it loads SR from its operand and
                // suspends fetch until an *unmasked* interrupt arrives
                // (68000UM4 §6.2), then resumes -- it is Kickstart's idle
                // dispatcher parking until the next VERTB/CIA tick, not a
                // request to end the run. SR mask 7 is the one STOP no
                // level 1-6 source (all this chipset ever requests) can
                // ever satisfy, which is exactly the synthetic smoke-test
                // ROM's contract (`tests/smoke.rs`'s `STOP_SR = 0x2700`) --
                // that, and only that, is a real clean halt.
                if cpu.int_mask & 0x0700 == 0x0700 {
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
                let frames = bus.0.chipset.frames;
                if frames >= args.max_frames {
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
                const STOP_TICK_SLICE: u32 = machine_core::chipset::PAL_COLOUR_CLOCKS_PER_LINE
                    * machine_core::CPU_CLOCKS_PER_COLOUR_CLOCK;
                let mut budget = RUN_BATCH_CYCLES as u32;
                while budget > 0 {
                    bus.0.tick(STOP_TICK_SLICE.min(budget));
                    budget = budget.saturating_sub(STOP_TICK_SLICE);
                    if bus.0.pending_irq_level() != 0 {
                        break;
                    }
                }
                cpu.set_irq(bus.0.pending_irq_level());
                drain_serial(bus, console);

                let frames = bus.0.chipset.frames;
                if let Some(job) = screenshot_job.as_mut() {
                    job.maybe_capture(frames, args.max_frames, &mut bus.0, console);
                }
                if frames >= last_progress_frame + PROGRESS_EVERY_FRAMES {
                    last_progress_frame = frames;
                    console.diag(&format!(
                        "progress: frame {frames}, PC {:#010x} (stopped), overlay {}, INTENA {:#06x}, INTREQ {:#06x}",
                        cpu.pc,
                        if bus.0.overlay() { "mapped" } else { "clear" },
                        bus.0.chipset.intena,
                        bus.0.chipset.intreq,
                    ));
                }
                continue 'outer;
            }
            CycleBatchExit::AlineTrap { opcode } => {
                if track_exception(
                    "A-line",
                    cpu.ppc,
                    &mut last_exception,
                    &mut exception_streak,
                ) {
                    break 'outer Outcome::Wedged(format!(
                        "exception storm: A-line trap {opcode:#06x} at PC {:#010x} repeated {exception_streak} times",
                        cpu.ppc
                    ));
                }
                cpu.take_aline_exception(bus);
            }
            CycleBatchExit::FlineTrap { opcode } => {
                if track_exception(
                    "F-line",
                    cpu.ppc,
                    &mut last_exception,
                    &mut exception_streak,
                ) {
                    break 'outer Outcome::Wedged(format!(
                        "exception storm: F-line trap {opcode:#06x} at PC {:#010x} repeated {exception_streak} times",
                        cpu.ppc
                    ));
                }
                cpu.take_fline_exception(bus);
            }
            CycleBatchExit::TrapInstruction { trap_num } => {
                if track_exception("TRAP", cpu.ppc, &mut last_exception, &mut exception_streak) {
                    break 'outer Outcome::Wedged(format!(
                        "exception storm: TRAP #{trap_num} at PC {:#010x} repeated {exception_streak} times",
                        cpu.ppc
                    ));
                }
                cpu.take_trap_exception(bus, trap_num);
            }
            CycleBatchExit::Breakpoint { bp_num } => {
                if track_exception("BKPT", cpu.ppc, &mut last_exception, &mut exception_streak) {
                    break 'outer Outcome::Wedged(format!(
                        "exception storm: BKPT #{bp_num} at PC {:#010x} repeated {exception_streak} times",
                        cpu.ppc
                    ));
                }
                cpu.take_bkpt_exception(bus);
            }
            CycleBatchExit::IllegalInstruction { opcode } => {
                if track_exception(
                    "illegal",
                    cpu.ppc,
                    &mut last_exception,
                    &mut exception_streak,
                ) {
                    break 'outer Outcome::Wedged(format!(
                        "exception storm: illegal opcode {opcode:#06x} at PC {:#010x} repeated {exception_streak} times",
                        cpu.ppc
                    ));
                }
                cpu.take_illegal_exception(bus);
            }
        }
    };

    Report {
        outcome,
        instructions: total_instructions,
        frames: bus.0.chipset.frames,
        final_pc: cpu.pc,
        overlay_cleared: !bus.0.overlay(),
    }
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
fn drain_serial(bus: &mut Bus, console: &mut Console) {
    while let Some(byte) = bus.0.chipset.take_serial_byte() {
        console.guest_byte(byte);
    }
}

/// Drive the optional `--serial-script` and `--trigger-illegal-after-frames`
/// once per changed chipset frame. Only called from the per-instruction
/// hook -- see that call site's comment for why the CPU is guaranteed to
/// be actively executing (not `stopped`) there, which
/// `cpu.take_illegal_exception` needs.
#[allow(clippy::too_many_arguments)]
fn service_host_serial(
    args: &Args,
    cpu: &mut CpuCore,
    bus: &mut Bus,
    console: &mut Console,
    script: Option<&mut SerialScript>,
    overlay_cleared_frame: &mut Option<u64>,
    last_serviced_frame: &mut Option<u64>,
    illegal_triggered: &mut bool,
) {
    let frame = bus.0.chipset.frames;
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
