//! Board layer for QEMU's `virt` machine (aarch64).
//!
//! Originally a Phase 0 smoke payload that could only exercise
//! `machine-core`'s bus, because the `m68k` crate had no `no_std` support
//! (see `docs/phase0-findings.md`). The project now depends on a personal
//! fork of `m68k` with `no_std` + `alloc` support (pinned by `rev` in the
//! root `Cargo.toml`'s `[workspace.dependencies]`), so this payload also:
//!
//!   1. Prints a hello banner over the PL011 UART at QEMU virt's
//!      UART0 (`0x0900_0000`).
//!   2. Builds a [`machine_core::MachineBus`] over a static 2 MB chip
//!      RAM buffer and a small test ROM, and runs a few checks that
//!      exercise the open-bus rule, chip RAM read/write, and ROM
//!      mirroring — printing PASS/FAIL per check.
//!   3. Instantiates a real `m68k::CpuCore`, resets it through the same
//!      ROM-overlay-at-reset mechanism `tests/hello_guest.rs` exercises
//!      hosted, and steps `MOVEQ #42,D0` followed by two `NOP`s — the
//!      first guest instruction this project has ever executed on bare
//!      metal, rather than under a hosted `cargo test` process.
//!
//! All checks print PASS/FAIL, and a final marker line that CI greps for.
//!
//! Canonical build command (from the workspace root):
//!
//! ```text
//! cargo build -p board-qemu-virt --target aarch64-unknown-none
//! ```
//!
//! Run under QEMU: `scripts/run-qemu-virt.sh`.

#![no_std]
#![no_main]

mod heap;

use core::fmt::Write;
use core::mem::MaybeUninit;
use core::panic::PanicInfo;
use core::ptr::addr_of_mut;

use m68k::{AddressBus, CpuCore, CpuType, CycleBatchControl, CycleBatchExit, StepResult};
use machine_core::{MachineBus, CHIP_RAM_SIZE};

// `_start`: the ELF entry point QEMU's `-kernel` loader jumps to.
//
// Sets the stack pointer, zeroes `.bss` (linker-provided
// `__bss_start`/`__bss_end`), then calls `main`. `main` never returns, but
// the trailing `wfe` loop is a safety net in case it somehow does.
// Also enables FP/SIMD before `main` runs: the `aarch64-unknown-none`
// target has NEON available and LLVM freely lowers plain struct
// construction/array-fill code (e.g. `m68k::CpuCore::new()`, whose ~1.5 KB
// struct literal includes several small fixed-size arrays) to NEON load/
// store instructions, whether or not the source uses floating point.
// FP/SIMD access traps by default at both EL2 (`CPTR_EL2.TFP`) and EL1
// (`CPACR_EL1.FPEN`); with no exception vector table installed, that trap
// has nowhere sane to go, so the payload page-faults into whatever
// `VBAR_EL1`'s reset value (0) points at instead of raising a visible
// error -- which read from the serial log alone looks exactly like an
// ordinary hang partway through `main`, not a fault. Since QEMU virt's
// `-kernel` loader can start the image at EL2 or EL1, this checks
// `CurrentEL` rather than assuming one (an EL2-only register write
// executed while already at EL1 would itself trap).
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "  ldr x0, =__stack_top",
    "  mov sp, x0",
    "  mrs x9, CurrentEL",
    "  and x9, x9, #0xc",
    "  cmp x9, #0x8",
    "  b.ne 1f",
    "  mrs x9, cptr_el2",
    "  bic x9, x9, #(1 << 10)", // clear TFP: don't trap FP/SIMD to EL2
    "  msr cptr_el2, x9",
    "1:",
    "  mrs x9, cpacr_el1",
    "  orr x9, x9, #(3 << 20)", // FPEN = 0b11: don't trap FP/SIMD to EL1
    "  msr cpacr_el1, x9",
    "  isb",
    "  ldr x0, =__bss_start",
    "  ldr x1, =__bss_end",
    "2:",
    "  cmp x0, x1",
    "  b.ge 3f",
    "  str xzr, [x0], #8",
    "  b 2b",
    "3:",
    "  bl main",
    "4:",
    "  wfe",
    "  b 4b",
);

/// QEMU virt's PL011 UART0 base address.
const UART0_BASE: usize = 0x0900_0000;
/// Data register offset: writing a byte here transmits it (polled, no IRQs).
const UARTDR: usize = 0x000;
/// Flag register offset.
const UARTFR: usize = 0x018;
/// Flag register bit 5: transmit FIFO full.
const UARTFR_TXFF: u32 = 1 << 5;

/// A tiny polled-TX handle to QEMU virt's PL011 UART0.
///
/// No initialisation is needed: QEMU's PL011 model starts up already
/// enabled with a sane baud/format for `-serial stdio`, so this only ever
/// writes `DR` after checking `FR.TXFF`.
struct Uart;

impl Uart {
    fn put_byte(&mut self, byte: u8) {
        // SAFETY: UART0_BASE is QEMU virt's fixed PL011 MMIO address; FR
        // and DR are volatile device registers, read/written with the
        // widths the PL011 expects (32-bit accesses).
        unsafe {
            let fr = (UART0_BASE + UARTFR) as *const u32;
            while core::ptr::read_volatile(fr) & UARTFR_TXFF != 0 {}
            let dr = (UART0_BASE + UARTDR) as *mut u32;
            core::ptr::write_volatile(dr, byte as u32);
        }
    }
}

impl Write for Uart {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                self.put_byte(b'\r');
            }
            self.put_byte(byte);
        }
        Ok(())
    }
}

/// `print!`-style helper over the polled UART.
macro_rules! uprintln {
    ($($arg:tt)*) => {{
        let mut uart = Uart;
        let _ = writeln!(uart, $($arg)*);
    }};
}

/// Static backing storage for the [`MachineBus`] under test.
///
/// 2 MB is too large to build on the stack of a bare-metal payload with a
/// 64 KB stack, so it is a zero-initialised `static mut`, which the
/// compiler places in `.bss` (zeroed by `_start` before `main` runs, same
/// as any other `.bss` content — no separate initialisation needed here).
/// Accessed only from `main`, single-threaded, interrupts never enabled,
/// so a raw-pointer-via-`addr_of_mut!` borrow is sound.
static mut CHIP_RAM: MaybeUninit<[u8; CHIP_RAM_SIZE]> = MaybeUninit::uninit();

/// A small test ROM image: 4 bytes, so it mirrors 128x across the 512 KB
/// ROM window (proposal-mandated mirroring behaviour for undersized ROM).
static TEST_ROM: [u8; 4] = [0x11, 0x22, 0x33, 0x44];

/// Second static chip RAM buffer for the CPU-stepping section below, kept
/// separate from the one the bus checks use above so the two sections
/// stay independent of each other's state (and so `TEST_ROM`'s layout,
/// tuned for the mirroring check, doesn't have to also serve as a valid
/// reset vector).
static mut CPU_CHIP_RAM: MaybeUninit<[u8; CHIP_RAM_SIZE]> = MaybeUninit::uninit();

/// Reset vector (initial SSP at offset 0, initial PC at offset 4) plus a
/// tiny guest program: `MOVEQ #42,D0` then two `NOP`s. Mirrors what
/// `crates/machine-core/tests/hello_guest.rs` runs hosted: the CPU's
/// reset fetch from `$000000`/`$000004` lands in this ROM through
/// `MachineBus`'s overlay (asserted by default, same as real Gary out of
/// reset), not by writing the vector into chip RAM directly.
const CPU_PROGRAM_ADDR: u32 = machine_core::ROM_BASE + 8;
static GUEST_ROM: [u8; 14] = {
    let mut rom = [0u8; 14];
    // Initial SSP: top of the 2 MB chip RAM region.
    let ssp = (CHIP_RAM_SIZE as u32).to_be_bytes();
    rom[0] = ssp[0];
    rom[1] = ssp[1];
    rom[2] = ssp[2];
    rom[3] = ssp[3];
    // Initial PC: the program below, 8 bytes into this ROM image.
    let pc = CPU_PROGRAM_ADDR.to_be_bytes();
    rom[4] = pc[0];
    rom[5] = pc[1];
    rom[6] = pc[2];
    rom[7] = pc[3];
    // MOVEQ #42,D0 (0x7000 | 42).
    rom[8] = 0x70;
    rom[9] = 42;
    // NOP (0x4E71), twice.
    rom[10] = 0x4E;
    rom[11] = 0x71;
    rom[12] = 0x4E;
    rom[13] = 0x71;
    rom
};

/// The AROS 68k ROM pair (proposal §11.2), embedded directly into this
/// binary: there is no filesystem on bare metal to load them from at run
/// time. Freely redistributable (`assets/aros/LICENSE`,
/// `assets/aros/PROVENANCE.md`) -- unlike Kickstart, which is why this is
/// the public CI boot target rather than a real Kickstart image.
static AROS_ROM: &[u8] = include_bytes!("../../../assets/aros/aros-amiga-m68k-rom.bin");
static AROS_EXT_ROM: &[u8] = include_bytes!("../../../assets/aros/aros-amiga-m68k-ext.bin");

/// Third static chip RAM buffer, for the AROS boot section below. Kept
/// separate from `CHIP_RAM` (bus checks) and `CPU_CHIP_RAM` (the Phase 0
/// CPU-stepping demo) for the same reason those two are separate from
/// each other: independent state, no risk of one section's writes
/// bleeding into another's.
static mut BOOT_CHIP_RAM: MaybeUninit<[u8; CHIP_RAM_SIZE]> = MaybeUninit::uninit();

/// Adapts [`MachineBus`] to m68k-rs's [`AddressBus`] trait, same shape as
/// the hosted adapter in `tests/hello_guest.rs` (can't share it: the
/// adapter has to live wherever it's used, since both `MachineBus` and
/// `AddressBus` are foreign to this crate and a blanket `impl` would
/// violate the orphan rule from either side).
struct Bus<'a>(MachineBus<'a>);

impl AddressBus for Bus<'_> {
    fn read_byte(&mut self, address: u32) -> u8 {
        self.0.read_byte(address)
    }

    fn read_word(&mut self, address: u32) -> u16 {
        self.0.read_word(address)
    }

    fn read_long(&mut self, address: u32) -> u32 {
        self.0.read_long(address)
    }

    fn write_byte(&mut self, address: u32, value: u8) {
        self.0.write_byte(address, value);
    }

    fn write_word(&mut self, address: u32, value: u16) {
        self.0.write_word(address, value);
    }

    fn write_long(&mut self, address: u32, value: u32) {
        self.0.write_long(address, value);
    }
}

/// Tracks whether every check so far has passed.
struct Checks {
    all_passed: bool,
}

impl Checks {
    fn check(&mut self, name: &str, passed: bool) {
        if passed {
            uprintln!("PASS: {name}");
        } else {
            uprintln!("FAIL: {name}");
            self.all_passed = false;
        }
    }
}

/// Payload entry point, called from `_start` after `.bss` is zeroed.
///
/// `#[no_mangle] extern "C"` so the symbol name matches the `bl main` in
/// `_start`'s assembly.
#[no_mangle]
pub extern "C" fn main() -> ! {
    uprintln!();
    uprintln!("m68k Machine -- board-qemu-virt Phase 0 smoke payload");
    uprintln!("aarch64 / QEMU virt, PL011 UART0 @ {UART0_BASE:#010x}");
    uprintln!();

    // SAFETY: single-threaded bare-metal payload, no interrupts enabled,
    // no other reference to CHIP_RAM exists anywhere else in the program.
    let chip_ram: &mut [u8; CHIP_RAM_SIZE] = unsafe {
        // .bss is already zeroed by _start, so this MaybeUninit is
        // actually fully initialised (to all zero bytes) by the time
        // main runs; assume_init_mut is sound here for that reason.
        (*addr_of_mut!(CHIP_RAM)).assume_init_mut()
    };

    let mut bus = MachineBus::new(chip_ram, &TEST_ROM);
    let mut checks = Checks { all_passed: true };

    // Open-bus rule: an address with nothing mapped (proposal's Gayle ID
    // probe address) reads back as all-ones.
    let open_bus_value = bus.read_long(0x00DE_1000);
    checks.check(
        "open bus $DE1000 reads $FFFFFFFF",
        open_bus_value == 0xFFFF_FFFF,
    );

    // Open-bus writes are silently discarded.
    bus.write_long(0x00DE_1000, 0xDEAD_BEEF);
    let after_write = bus.read_long(0x00DE_1000);
    checks.check(
        "open bus write to $DE1000 is discarded",
        after_write == 0xFFFF_FFFF,
    );

    // Chip RAM write/read roundtrip.
    bus.write_long(0x0010_0000, 0xCAFE_F00D);
    let chip_ram_value = bus.read_long(0x0010_0000);
    checks.check(
        "chip RAM write/read roundtrip at $100000",
        chip_ram_value == 0xCAFE_F00D,
    );

    // ROM read: first 4 bytes of the ROM window should match TEST_ROM.
    let rom_value = bus.read_long(machine_core::ROM_BASE);
    checks.check(
        "ROM read at $F80000 matches test ROM",
        rom_value == 0x1122_3344,
    );

    // ROM mirroring: a 4-byte ROM mirrors every 4 bytes across the 512 KB
    // window, so the window's second word should equal the first.
    let rom_mirror_value = bus.read_long(machine_core::ROM_BASE + 4);
    checks.check(
        "ROM mirrors across the window",
        rom_mirror_value == 0x1122_3344,
    );

    // ROM writes are discarded (read-only).
    bus.write_byte(machine_core::ROM_BASE, 0x99);
    let rom_after_write = bus.read_byte(machine_core::ROM_BASE);
    checks.check("ROM write is discarded", rom_after_write == 0x11);

    uprintln!();
    uprintln!("-- stepping a real m68k::CpuCore on bare metal --");
    uprintln!("(the first guest instruction this project has executed");
    uprintln!(" outside a hosted `cargo test` process)");
    uprintln!();

    // SAFETY: same reasoning as CHIP_RAM above -- single-threaded, no
    // interrupts, no other reference to CPU_CHIP_RAM anywhere else.
    let cpu_chip_ram: &mut [u8; CHIP_RAM_SIZE] =
        unsafe { (*addr_of_mut!(CPU_CHIP_RAM)).assume_init_mut() };

    let cpu_bus = MachineBus::new(cpu_chip_ram, &GUEST_ROM);
    let mut bus = Bus(cpu_bus);

    let mut cpu = CpuCore::new();
    cpu.set_cpu_type(CpuType::M68040);
    cpu.reset(&mut bus);

    checks.check(
        "CPU reset: PC loaded from reset vector",
        cpu.pc == CPU_PROGRAM_ADDR,
    );
    checks.check(
        "CPU reset: SSP loaded from reset vector",
        cpu.sp() == CHIP_RAM_SIZE as u32,
    );

    // Step MOVEQ #42,D0.
    let step = cpu.step(&mut bus);
    checks.check(
        "MOVEQ #42,D0 stepped without a fault",
        matches!(step, StepResult::Ok { .. }),
    );
    checks.check("D0 == 42 after MOVEQ", cpu.d(0) == 42);
    checks.check("PC advanced past MOVEQ", cpu.pc == CPU_PROGRAM_ADDR + 2);

    // Step the two NOPs.
    let mut nops_ok = true;
    for _ in 0..2 {
        let step = cpu.step(&mut bus);
        nops_ok &= matches!(step, StepResult::Ok { .. });
    }
    checks.check("both NOPs stepped without a fault", nops_ok);
    checks.check("PC advanced past both NOPs", cpu.pc == CPU_PROGRAM_ADDR + 6);
    checks.check("D0 still == 42 after the NOPs", cpu.d(0) == 42);

    uprintln!();
    if checks.all_passed {
        uprintln!("PHASE0 BOARD-QEMU-VIRT: ALL CHECKS PASSED (bus + CPU)");
    } else {
        uprintln!("PHASE0 BOARD-QEMU-VIRT: CHECKS FAILED");
    }

    // SAFETY: same reasoning as CHIP_RAM/CPU_CHIP_RAM above -- single-
    // threaded, no interrupts, no other reference to BOOT_CHIP_RAM
    // anywhere else in the program.
    let boot_chip_ram: &mut [u8; CHIP_RAM_SIZE] =
        unsafe { (*addr_of_mut!(BOOT_CHIP_RAM)).assume_init_mut() };
    boot_aros(boot_chip_ram);

    park();
}

/// Bounds and pacing for the AROS boot run below.
///
/// Smaller than `machine-hosted`'s CLI defaults (`--max-frames 6000`,
/// `--max-instructions 200_000_000`): a hosted run of the same ROM pair
/// (`cargo run -p machine-hosted -- --rom ... --ext-rom ...`) reaches
/// AROS's post-boot `STOP` idle (PC `$00FE8B88`, waiting for boot media)
/// by frame ~40-50 and a few million instructions, so this budget is
/// measured headroom over that, not a guess -- generous enough to reach
/// idle and print several progress lines, while keeping a CI run of this
/// payload (m68k-rs interpreting AROS, itself inside QEMU emulating
/// aarch64 -- two layers of emulation) bounded to a sane wall-clock time.
const BOOT_MAX_FRAMES: u64 = 200;
const BOOT_MAX_INSTRUCTIONS: u64 = 50_000_000;
const BOOT_RUN_BATCH_CYCLES: i32 = 2_000_000;
const BOOT_PROGRESS_EVERY_FRAMES: u64 = 10;
/// Instructions retired at an unchanging PC before this is called a wedge
/// rather than a legitimate busy-wait. Scaled down from
/// `machine-hosted`'s 20,000,000 to stay well under `BOOT_MAX_INSTRUCTIONS`
/// while remaining far above one frame's instruction count (so it does not
/// fire on AROS's real VERTB-wait idle loops, which exit via `STOP`, not by
/// spinning -- see the `Stopped` handling below).
const BOOT_TIGHT_LOOP_THRESHOLD: u64 = 5_000_000;
/// Identical CPU exceptions taken back-to-back at the same faulting PC
/// before this is called an exception storm, matching `machine-hosted`.
const BOOT_EXCEPTION_STORM_THRESHOLD: u64 = 1_000;

/// Why the AROS boot run ended, mirroring `machine-hosted`'s
/// `run::Outcome` (not shared: `machine-hosted` is a hosted `std` binary
/// crate, not something this `no_std` board can depend on).
enum BootOutcome {
    /// The guest executed `STOP` with SR's interrupt mask at 7 -- shut out
    /// every maskable level (1-6, all this chipset ever requests), so
    /// nothing can ever resume it. Real Kickstart/AROS never does this at
    /// its idle dispatcher (mask 0, woken by VERTB every frame); this
    /// arm exists for parity with the hosted runner and the synthetic
    /// smoke-test ROM contract it documents, not because AROS is expected
    /// to trigger it.
    CleanHalt,
    /// `BOOT_MAX_FRAMES`/`BOOT_MAX_INSTRUCTIONS` was hit first. This is
    /// the *expected* outcome for AROS at this phase: it idles forever at
    /// its no-boot-media `STOP`, so the run only ever ends by running out
    /// of the frame/instruction budget, not by the guest halting itself.
    LimitReached(&'static str),
    /// A tight loop or exception storm well past what a legitimate idle
    /// wait looks like.
    Wedged,
}

/// Drain any bytes the guest has written to `SERDAT` since the last call,
/// printing each completed line prefixed `GUEST |` so it reads apart from
/// this payload's own `host |`-implicit diagnostic lines -- same split
/// `machine-hosted`'s `Console` draws, reproduced here without `alloc`
/// (`GuestLine` is a fixed-capacity buffer, not a `String`).
fn drain_serial(bus: &mut Bus, guest_line: &mut GuestLine) {
    while let Some(byte) = bus.0.chipset.take_serial_byte() {
        guest_line.push(byte);
    }
}

/// Fixed-capacity line buffer for guest serial output, printed one line at
/// a time via the polled UART. No `alloc` dependency: this board only
/// pulls `alloc` in for `m68k::CpuCore`'s own internals (see `heap.rs`),
/// never for its own code.
struct GuestLine {
    buf: [u8; 256],
    len: usize,
}

impl GuestLine {
    const fn new() -> Self {
        Self {
            buf: [0; 256],
            len: 0,
        }
    }

    /// Feed one guest byte. `\r` is dropped (AROS pairs it with `\n`, and
    /// `uprintln!`/`Uart::write_str` already re-emits `\r\n` for the host
    /// terminal on the `\n` below, so keeping the guest's own `\r` would
    /// double it up); anything past the buffer's capacity is dropped
    /// rather than panicking or wrapping -- a long unterminated line is
    /// diagnostic-window overflow, not a boot-blocking condition.
    fn push(&mut self, byte: u8) {
        match byte {
            b'\n' => self.flush(),
            b'\r' => {}
            _ => {
                if self.len < self.buf.len() {
                    self.buf[self.len] = byte;
                    self.len += 1;
                }
            }
        }
    }

    fn flush(&mut self) {
        if self.len == 0 {
            return;
        }
        let text = core::str::from_utf8(&self.buf[..self.len]).unwrap_or("<non-utf8>");
        uprintln!("GUEST | {text}");
        self.len = 0;
    }
}

/// Report what `machine_core::rom::identify` could determine about one
/// embedded ROM image, same shape as `machine-hosted`'s
/// `rom_image::report_identify` (reimplemented here since that lives in a
/// hosted `std` binary crate this board cannot depend on).
fn report_rom_identify(label: &str, image: &[u8]) {
    match machine_core::rom::identify(image) {
        Some(info) => {
            uprintln!(
                "{label} ROM: {:?} rev {}.{}, exec {}.{}, {} bytes, checksum {}, boot_pc {:#010x}",
                info.kind,
                info.rev.0,
                info.rev.1,
                info.exec_rev.0,
                info.exec_rev.1,
                info.size,
                if info.checksum_ok { "ok" } else { "FAILED" },
                info.boot_pc,
            );
        }
        None => {
            uprintln!(
                "{label} ROM: unidentified ({} bytes) -- booting anyway",
                image.len()
            );
        }
    }
}

/// Boot the embedded AROS ROM pair on a real `m68k::CpuCore`, reporting
/// progress over the polled UART, and print the `PHASE1 BOARD-QEMU-VIRT:`
/// marker line CI greps for once the run ends.
///
/// Drives the CPU exactly as `machine-hosted/src/run.rs` documents driving
/// it must work (that module's doc comment is the canonical source for
/// this contract): `run_for_cycles_with_hook`, ticking the bus and
/// resampling `pending_irq_level` from the hook after every retired
/// instruction, delivering A-line/F-line/TRAP/BKPT/illegal exceptions via
/// the matching `take_*_exception` rather than letting them escape
/// undelivered, and -- the STOP/interrupt-resume contract
/// `docs/phase0-findings.md` documents -- ticking the bus in raster-line
/// slices while the CPU is `Stopped` instead of freezing machine time
/// along with the CPU. Getting the last part wrong reproduces the exact
/// "looks halted, is actually idling" failure that section records.
fn boot_aros(chip_ram: &mut [u8; CHIP_RAM_SIZE]) {
    uprintln!();
    uprintln!("-- booting AROS 68k on a real m68k::CpuCore --");
    uprintln!();

    report_rom_identify("main", AROS_ROM);
    report_rom_identify("ext", AROS_EXT_ROM);

    let machine_bus = MachineBus::new(chip_ram, AROS_ROM).with_ext_rom(AROS_EXT_ROM);
    let mut bus = Bus(machine_bus);

    let mut cpu = CpuCore::new();
    cpu.set_cpu_type(CpuType::M68040);
    cpu.reset(&mut bus);

    uprintln!(
        "reset vector: SSP {:#010x} PC {:#010x} (overlay {})",
        cpu.sp(),
        cpu.pc,
        if bus.0.overlay() { "mapped" } else { "clear" }
    );

    let mut guest_line = GuestLine::new();
    let mut total_instructions: u64 = 0;
    let mut last_progress_frame: u64 = 0;
    let mut overlay_was_cleared = false;
    let mut last_pc: u32 = cpu.pc;
    let mut same_pc_streak: u64 = 0;
    let mut last_exception: Option<(&'static str, u32)> = None;
    let mut exception_streak: u64 = 0;

    let outcome = 'outer: loop {
        let mut hook_wedge = false;
        let mut hook_limit: Option<&'static str> = None;

        let hook_instructions_before = total_instructions;
        let result =
            cpu.run_for_cycles_with_hook(&mut bus, BOOT_RUN_BATCH_CYCLES, |cpu, bus, cycles| {
                bus.0.tick(cycles.max(0) as u32);
                cpu.set_irq(bus.0.pending_irq_level());
                drain_serial(bus, &mut guest_line);

                total_instructions += 1;
                let pc = cpu.ppc;

                if pc == last_pc {
                    same_pc_streak += 1;
                } else {
                    last_pc = pc;
                    same_pc_streak = 1;
                }
                if same_pc_streak >= BOOT_TIGHT_LOOP_THRESHOLD {
                    uprintln!(
                        "wedge: tight loop at PC {pc:#010x} ({same_pc_streak} instructions with no progress)"
                    );
                    hook_wedge = true;
                    return CycleBatchControl::Return;
                }

                let frames = bus.0.chipset.frames;
                if !overlay_was_cleared && !bus.0.overlay() {
                    overlay_was_cleared = true;
                    uprintln!(
                        "PHASE1 BOARD-QEMU-VIRT: overlay cleared (frame {frames}, instr {total_instructions}, PC {:#010x})",
                        cpu.pc
                    );
                }
                if frames >= last_progress_frame + BOOT_PROGRESS_EVERY_FRAMES {
                    last_progress_frame = frames;
                    uprintln!(
                        "progress: frame {frames}, PC {:#010x}, overlay {}, INTENA {:#06x}, INTREQ {:#06x}",
                        cpu.pc,
                        if bus.0.overlay() { "mapped" } else { "clear" },
                        bus.0.chipset.intena,
                        bus.0.chipset.intreq,
                    );
                }

                if total_instructions >= BOOT_MAX_INSTRUCTIONS {
                    hook_limit = Some("max-instructions");
                    return CycleBatchControl::Return;
                }
                if frames >= BOOT_MAX_FRAMES {
                    hook_limit = Some("max-frames");
                    return CycleBatchControl::Return;
                }

                CycleBatchControl::Continue
            });

        if total_instructions < hook_instructions_before + result.instructions as u64 {
            total_instructions = hook_instructions_before + result.instructions as u64;
        }

        if hook_wedge {
            break 'outer BootOutcome::Wedged;
        }
        if let Some(which) = hook_limit {
            break 'outer BootOutcome::LimitReached(which);
        }

        match result.exit {
            CycleBatchExit::BudgetExhausted | CycleBatchExit::BoundaryRequested => {
                continue 'outer;
            }
            CycleBatchExit::Stopped => {
                if cpu.int_mask & 0x0700 == 0x0700 {
                    break 'outer BootOutcome::CleanHalt;
                }

                let frames = bus.0.chipset.frames;
                if frames >= BOOT_MAX_FRAMES {
                    break 'outer BootOutcome::LimitReached("max-frames");
                }

                // The machine clock must keep advancing while STOPped, or
                // the interrupt that would wake it can never fire: see
                // this function's doc comment and
                // `docs/phase0-findings.md`'s STOP section.
                // `run_for_cycles_with_hook`'s hook is not called for an
                // already-stopped CPU, so nothing above ticks the bus
                // while `stopped` stays set -- tick it here instead, in
                // raster-line slices (never one `RUN_BATCH_CYCLES` gulp;
                // see `machine-hosted/src/run.rs`'s comment on why a gulp
                // undercounts VERTBs and CIA timer underflows), stopping
                // at the first slice that leaves an interrupt pending.
                const STOP_TICK_SLICE: u32 = machine_core::chipset::PAL_COLOUR_CLOCKS_PER_LINE
                    * machine_core::CPU_CLOCKS_PER_COLOUR_CLOCK;
                let mut budget = BOOT_RUN_BATCH_CYCLES as u32;
                while budget > 0 {
                    bus.0.tick(STOP_TICK_SLICE.min(budget));
                    budget = budget.saturating_sub(STOP_TICK_SLICE);
                    if bus.0.pending_irq_level() != 0 {
                        break;
                    }
                }
                cpu.set_irq(bus.0.pending_irq_level());
                drain_serial(&mut bus, &mut guest_line);

                let frames = bus.0.chipset.frames;
                if frames >= last_progress_frame + BOOT_PROGRESS_EVERY_FRAMES {
                    last_progress_frame = frames;
                    uprintln!(
                        "progress: frame {frames}, PC {:#010x} (stopped), overlay {}, INTENA {:#06x}, INTREQ {:#06x}",
                        cpu.pc,
                        if bus.0.overlay() { "mapped" } else { "clear" },
                        bus.0.chipset.intena,
                        bus.0.chipset.intreq,
                    );
                }
                continue 'outer;
            }
            CycleBatchExit::AlineTrap { opcode } => {
                let _ = opcode;
                if track_exception(
                    "A-line",
                    cpu.ppc,
                    &mut last_exception,
                    &mut exception_streak,
                ) {
                    break 'outer BootOutcome::Wedged;
                }
                cpu.take_aline_exception(&mut bus);
            }
            CycleBatchExit::FlineTrap { opcode } => {
                let _ = opcode;
                if track_exception(
                    "F-line",
                    cpu.ppc,
                    &mut last_exception,
                    &mut exception_streak,
                ) {
                    break 'outer BootOutcome::Wedged;
                }
                cpu.take_fline_exception(&mut bus);
            }
            CycleBatchExit::TrapInstruction { trap_num } => {
                if track_exception("TRAP", cpu.ppc, &mut last_exception, &mut exception_streak) {
                    break 'outer BootOutcome::Wedged;
                }
                cpu.take_trap_exception(&mut bus, trap_num);
            }
            CycleBatchExit::Breakpoint { bp_num } => {
                let _ = bp_num;
                if track_exception("BKPT", cpu.ppc, &mut last_exception, &mut exception_streak) {
                    break 'outer BootOutcome::Wedged;
                }
                cpu.take_bkpt_exception(&mut bus);
            }
            CycleBatchExit::IllegalInstruction { opcode } => {
                let _ = opcode;
                if track_exception(
                    "illegal",
                    cpu.ppc,
                    &mut last_exception,
                    &mut exception_streak,
                ) {
                    break 'outer BootOutcome::Wedged;
                }
                cpu.take_illegal_exception(&mut bus);
            }
        }
    };

    guest_line.flush();

    let frames = bus.0.chipset.frames;
    let overlay_cleared = !bus.0.overlay();
    let final_pc = cpu.pc;

    uprintln!();
    match outcome {
        BootOutcome::CleanHalt => uprintln!(
            "PHASE1 BOARD-QEMU-VIRT: CPU HALTED CLEANLY -- {total_instructions} instructions, {frames} frames, overlay {}, final PC {final_pc:#010x}",
            if overlay_cleared { "cleared" } else { "still mapped" }
        ),
        BootOutcome::LimitReached(which) => uprintln!(
            "PHASE1 BOARD-QEMU-VIRT: LIMIT REACHED ({which}) -- {total_instructions} instructions, {frames} frames, overlay {}, final PC {final_pc:#010x}",
            if overlay_cleared { "cleared" } else { "still mapped" }
        ),
        BootOutcome::Wedged => uprintln!(
            "PHASE1 BOARD-QEMU-VIRT: WEDGED -- {total_instructions} instructions, {frames} frames, overlay {}, final PC {final_pc:#010x}",
            if overlay_cleared { "cleared" } else { "still mapped" }
        ),
    }
}

/// Update the exception-storm tracker; returns `true` once the same kind
/// of exception has fired at the same PC `BOOT_EXCEPTION_STORM_THRESHOLD`
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
    if *streak >= BOOT_EXCEPTION_STORM_THRESHOLD {
        uprintln!("wedge: exception storm: {kind} at PC {pc:#010x} repeated {streak} times");
        true
    } else {
        false
    }
}

/// Park the core forever in a low-power wait loop.
fn park() -> ! {
    loop {
        unsafe {
            core::arch::asm!("wfe");
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    uprintln!("PANIC");
    park();
}
