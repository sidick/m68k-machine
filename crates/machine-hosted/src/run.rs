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
//! with the CPU not stopped (a `CycleBatchExit::Stopped` chunk resets the
//! streak rather than extending it -- Kickstart's idle dispatcher legitimately
//! holds the same PC in STOP forever, and this detector must never mistake
//! that for a wedge).
//!
//! `--trace` (and anything else built on the per-instruction hook) has no
//! equivalent in `run_guest_max` and is refused together with
//! `--cpu-speed max` in `run`, before either run loop is reached.

use std::path::Path;
use std::time::{Duration, Instant};

use m68k::{CpuCore, CycleBatchControl, CycleBatchExit};

use machine_core::block::BlockDevice;
use machine_core::{pci, MachineBus, CHIP_RAM_SIZE, CPU_CLOCKS_PER_ECLOCK};

use crate::bus::Bus;
use crate::cli::Args;
use crate::console::Console;
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

/// `run_guest_max`'s wedge threshold: the same PC sampled at every chunk
/// boundary for this many seconds of *wall clock*, with the CPU not
/// stopped, is called a wedge. Real Kickstart's idle dispatcher always
/// STOPs rather than spinning, so any non-STOP loop that holds one PC
/// this long is not a legitimate wait -- chosen generously above what
/// any real boot-time busy-wait in this codebase's own tests takes, since
/// max mode has no fixed instructions-per-second to size a count-based
/// threshold from (cycle mode's `TIGHT_LOOP_THRESHOLD`, which this
/// replaces for `--cpu-speed max`).
const MAX_MODE_WEDGE_SECONDS: f64 = 15.0;

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

    // Heap-allocate: a 2 MB array built on the stack overflows a default
    // thread stack (the same pitfall `machine-core`'s own tests document).
    let mut chip_ram: Box<[u8; CHIP_RAM_SIZE]> =
        match vec![0u8; CHIP_RAM_SIZE].into_boxed_slice().try_into() {
            Ok(b) => b,
            Err(_) => unreachable!("boxed_slice has exactly CHIP_RAM_SIZE elements"),
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
    let mut fast_ram: Vec<u8> = if args.fast_ram_mb > 0 {
        vec![0u8; args.fast_ram_mb as usize * 1024 * 1024]
    } else {
        Vec::new()
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

    let machine_bus = MachineBus::new(&mut chip_ram, &rom_bytes);
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
            machine_bus.with_fast_ram(&mut fast_ram)
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
    let mut bus = Bus(
        machine_bus,
        blitter_trace,
        serial_trace_enabled,
        cpu_speed_max,
    );

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

    let mut cpu = CpuCore::new();
    cpu.set_cpu_type(args.cpu.into());
    cpu.reset(&mut bus);

    console.diag(&format!(
        "reset vector: SSP={:#010x} PC={:#010x} (overlay {})",
        cpu.sp(),
        cpu.pc,
        if bus.0.overlay() { "mapped" } else { "clear" }
    ));

    let report = if cpu_speed_max {
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

    report
}

#[allow(clippy::too_many_arguments)]
fn run_guest(
    args: &Args,
    console: &mut Console,
    cpu: &mut CpuCore,
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
        let cpu_type = cpu.cpu_type;
        let mut hook_wedge: Option<String> = None;
        let mut hook_limit: Option<&'static str> = None;

        let hook_instructions_before = total_instructions;
        let result = cpu.run_for_cycles_with_hook(bus, RUN_BATCH_CYCLES, |cpu, bus, cycles| {
            bus.0.tick(cycles.max(0) as u32);
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
            let pc = cpu.ppc;
            // Feed the serial register trace (bus.rs) the PC of the next
            // instruction to execute, so its accesses get attributed --
            // but only when something could read it back: `LAST_PC` exists
            // purely for `trace_serial`'s diagnostic output, so a plain
            // run with `SERIAL_REG_TRACE` unset has no use for storing it
            // on every retired instruction.
            if bus.serial_trace_on() {
                crate::bus::LAST_PC.store(cpu.pc, std::sync::atomic::Ordering::Relaxed);
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
                            cpu.dar[0], cpu.dar[1], cpu.dar[2],
                            cpu.dar[8], cpu.dar[9], cpu.dar[10], cpu.dar[14], cpu.dar[15],
                        ));
                    }
                }
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
                return CycleBatchControl::Return;
            }
            if args.max_frames != 0 && frames >= args.max_frames {
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
                bus.0.frames(), cpu.pc
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
        frames: bus.0.frames(),
        final_pc: cpu.pc,
        overlay_cleared: !bus.0.overlay(),
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
/// ticking device time in slices (cycle mode's `CycleBatchExit::Stopped`
/// arm), and a host that falls behind real time slips device time rather
/// than bursting through the backlog (`MAX_MODE_BACKLOG_CAP`).
#[allow(clippy::too_many_arguments)]
fn run_guest_max(
    args: &Args,
    console: &mut Console,
    cpu: &mut CpuCore,
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

    let mut wedge_pc: u32 = cpu.pc;
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
    // host's very first chunk).
    let mut chunk_cycles: i32 = 4_000;

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
            let budget = chunk_cycles.clamp(MAX_MODE_MIN_CHUNK_CYCLES, MAX_MODE_MAX_CHUNK_CYCLES);
            let result = cpu.run_for_cycles(bus, budget);
            let chunk_elapsed = chunk_start.elapsed();

            total_instructions += result.instructions as u64;

            // Correct the chunk-size estimate from this chunk's actual
            // rate -- skipped when nothing ran (e.g. an already-stopped
            // CPU's zero-cycle exit), which would corrupt the estimate
            // rather than refine it.
            if result.cycles > 0 && chunk_elapsed > Duration::ZERO {
                let cycles_per_us = result.cycles as f64 / chunk_elapsed.as_secs_f64() / 1e6;
                let target = (cycles_per_us * MAX_MODE_CHUNK_TARGET_US).round();
                if target.is_finite() {
                    chunk_cycles = (target as i64).clamp(
                        MAX_MODE_MIN_CHUNK_CYCLES as i64,
                        MAX_MODE_MAX_CHUNK_CYCLES as i64,
                    ) as i32;
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
            // retires no instructions and leaves `cpu.pc` unchanged by
            // definition (Kickstart's idle dispatcher parks on the same
            // STOP over and over, exactly like a real wedge would look),
            // so it must reset the streak rather than extend it -- an
            // idle Workbench sitting in STOP for longer than
            // `MAX_MODE_WEDGE_SECONDS` is the expected, healthy case this
            // detector must never fire on.
            if result.exit == CycleBatchExit::Stopped {
                wedge_pc = cpu.pc;
                wedge_since = Instant::now();
            } else if cpu.pc == wedge_pc {
                let stuck_for = wedge_since.elapsed();
                if stuck_for.as_secs_f64() >= MAX_MODE_WEDGE_SECONDS {
                    hook_wedge = Some(format!(
                        "tight loop at PC {:#010x} ({:.1}s wall clock with no progress)",
                        cpu.pc,
                        stuck_for.as_secs_f64()
                    ));
                }
            } else {
                wedge_pc = cpu.pc;
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
                CycleBatchExit::BudgetExhausted => {
                    if Instant::now() >= t_event {
                        break;
                    }
                }
                CycleBatchExit::BoundaryRequested => break,
                CycleBatchExit::Stopped => {
                    stopped_exit = true;
                    break;
                }
                CycleBatchExit::AlineTrap { opcode } => {
                    if track_exception(
                        "A-line",
                        cpu.ppc,
                        &mut last_exception,
                        &mut exception_streak,
                    ) {
                        hook_wedge = Some(format!(
                            "exception storm: A-line trap {opcode:#06x} at PC {:#010x} repeated {exception_streak} times",
                            cpu.ppc
                        ));
                        break;
                    }
                    cpu.take_aline_exception(bus);
                    if Instant::now() >= t_event {
                        break;
                    }
                }
                CycleBatchExit::FlineTrap { opcode } => {
                    if track_exception(
                        "F-line",
                        cpu.ppc,
                        &mut last_exception,
                        &mut exception_streak,
                    ) {
                        hook_wedge = Some(format!(
                            "exception storm: F-line trap {opcode:#06x} at PC {:#010x} repeated {exception_streak} times",
                            cpu.ppc
                        ));
                        break;
                    }
                    cpu.take_fline_exception(bus);
                    if Instant::now() >= t_event {
                        break;
                    }
                }
                CycleBatchExit::TrapInstruction { trap_num } => {
                    if track_exception("TRAP", cpu.ppc, &mut last_exception, &mut exception_streak)
                    {
                        hook_wedge = Some(format!(
                            "exception storm: TRAP #{trap_num} at PC {:#010x} repeated {exception_streak} times",
                            cpu.ppc
                        ));
                        break;
                    }
                    cpu.take_trap_exception(bus, trap_num);
                    if Instant::now() >= t_event {
                        break;
                    }
                }
                CycleBatchExit::Breakpoint { bp_num } => {
                    if track_exception("BKPT", cpu.ppc, &mut last_exception, &mut exception_streak)
                    {
                        hook_wedge = Some(format!(
                            "exception storm: BKPT #{bp_num} at PC {:#010x} repeated {exception_streak} times",
                            cpu.ppc
                        ));
                        break;
                    }
                    cpu.take_bkpt_exception(bus);
                    if Instant::now() >= t_event {
                        break;
                    }
                }
                CycleBatchExit::IllegalInstruction { opcode } => {
                    if track_exception(
                        "illegal",
                        cpu.ppc,
                        &mut last_exception,
                        &mut exception_streak,
                    ) {
                        hook_wedge = Some(format!(
                            "exception storm: illegal opcode {opcode:#06x} at PC {:#010x} repeated {exception_streak} times",
                            cpu.ppc
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
            // See `run_guest`'s matching `CycleBatchExit::Stopped` arm for
            // why SR mask 7 (and only that) is a real clean halt.
            if cpu.int_mask & 0x0700 == 0x0700 {
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
                std::thread::sleep((t_event - now).min(MAX_MODE_STOP_SLEEP_SLICE));
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
        final_pc: cpu.pc,
        overlay_cleared: !bus.0.overlay(),
    }
}

/// `run_guest_max`'s chunk-boundary duties: the per-instruction hook's
/// work (serial/input scripts, screenshots, the overlay-cleared marker,
/// progress output), run once per chunk instead. Returns the current
/// frame count, which the caller already needs for its own limit check
/// right after calling this.
#[allow(clippy::too_many_arguments)]
fn max_chunk_boundary(
    args: &Args,
    console: &mut Console,
    cpu: &mut CpuCore,
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
            cpu.pc
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
            cpu.pc,
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
fn service_host_serial(
    args: &Args,
    cpu: &mut CpuCore,
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
