//! Board layer: x86-64 UEFI application targeting QEMU's `q35` machine.
//!
//! Phase 0's job for this board is the mirror of `board-qemu-virt`: prove
//! the harness (UEFI boot under OVMF/edk2, serial reachable) and exercise
//! `machine-core`'s `MachineBus` with the same checks the virt board runs,
//! since there is no CPU core to step yet (see
//! `docs/phase0-findings.md` — `m68k` is hosted-only, not `no_std`).
//!
//! Output goes to two places:
//! - the UEFI console (text output protocol), via `uefi::println!`, so a
//!   human watching the display (if any) sees it too;
//! - the COM1 16550 UART at I/O port `0x3F8`, via raw `in`/`out`
//!   instructions. This is the load-bearing path: CI runs QEMU with
//!   `-serial stdio` and greps the captured serial output for the final
//!   marker line, not the (usually headless, `-display none`) console.
//!
//! QEMU's default COM1 needs no initialisation beyond the polled
//! transmit-holding-register-empty wait implemented below; this
//! deliberately skips baud/divisor/LCR setup since it isn't needed under
//! QEMU and keeping the UART driver minimal keeps the bring-up surface
//! small (proposal §13: UEFI is the first bare-metal target precisely
//! because it needs the least platform-specific bring-up).
#![no_main]
#![no_std]

extern crate alloc;

mod present;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::asm;
use core::fmt::Write;
use core::ptr::addr_of_mut;

use m68k::{AddressBus, CpuCore, CpuType, CycleBatchControl, CycleBatchExit};
use machine_core::display::{Framebuffer, MAX_HEIGHT, MAX_WIDTH};
use machine_core::render::Renderer;
use machine_core::{MachineBus, CHIP_RAM_BASE, CHIP_RAM_SIZE, ROM_WINDOW_SIZE};
use uefi::boot::ScopedProtocol;
use uefi::prelude::*;
use uefi::proto::console::gop::{BltOp, BltPixel, BltRegion, GraphicsOutput};

/// COM1 I/O port base, per the standard PC UART memory map.
const COM1_PORT: u16 = 0x3F8;

/// Line Status Register offset from the UART base; bit 5 (`0x20`) is
/// Transmit Holding Register Empty (THRE).
const LSR_OFFSET: u16 = 5;
const LSR_THRE: u8 = 0x20;

/// Read one byte from an x86 I/O port.
///
/// # Safety
/// The caller must ensure `port` is a valid, safe-to-read I/O port for the
/// current platform. This module only ever calls it with the fixed COM1
/// port numbers above, which is always safe to probe under QEMU/OVMF.
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Write one byte to an x86 I/O port.
///
/// # Safety
/// The caller must ensure `port` is a valid, safe-to-write I/O port for the
/// current platform. This module only ever calls it with the fixed COM1
/// port numbers above, which is always safe to drive under QEMU/OVMF.
unsafe fn outb(port: u16, value: u8) {
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
    }
}

/// Write one byte to the COM1 serial port, polling THRE first.
///
/// Polled and blocking: fine for a low-volume Phase 0 hello banner, not
/// meant to be a real serial driver (that lands, if ever needed, with the
/// rest of the chipset in later phases).
fn serial_write_byte(byte: u8) {
    // SAFETY: COM1_PORT + LSR_OFFSET is the fixed, well-known Line Status
    // Register address for the first PC-compatible UART; reading it has no
    // side effects that matter here.
    while unsafe { inb(COM1_PORT + LSR_OFFSET) } & LSR_THRE == 0 {
        core::hint::spin_loop();
    }
    // SAFETY: COM1_PORT is the fixed, well-known transmit register for the
    // first PC-compatible UART.
    unsafe { outb(COM1_PORT, byte) };
}

/// Write a string to the COM1 serial port, translating `\n` to `\r\n` so
/// terminal capture (e.g. `-serial stdio`) renders lines correctly.
fn serial_write_str(s: &str) {
    for byte in s.bytes() {
        if byte == b'\n' {
            serial_write_byte(b'\r');
        }
        serial_write_byte(byte);
    }
}

/// A small `core::fmt::Write` adapter so `write!`/`writeln!` can target the
/// serial port the same way they target the UEFI console.
struct Serial;

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        serial_write_str(s);
        Ok(())
    }
}

/// Print one line to both the UEFI console and COM1 serial.
///
/// The UEFI console write is best-effort: if it fails (no console attached,
/// e.g. `-display none` without a virtual GPU) the banner still reaches
/// serial, which is what CI actually checks.
fn print_line(line: &str) {
    uefi::system::with_stdout(|stdout| {
        let _ = writeln!(stdout, "{line}");
    });
    let _ = writeln!(Serial, "{line}");
}

/// Static backing storage for the `MachineBus` under test: 2 MB of chip
/// RAM and a small test ROM, both `.bss`/`.data`. `machine-core` has no
/// allocator, so it borrows caller-owned storage (see its `MachineBus`
/// doc comment); a UEFI application could use the boot-services allocator
/// instead, but a static buffer avoids allocator setup entirely, which is
/// the simplest thing that works for a Phase 0 smoke test.
static mut CHIP_RAM: [u8; CHIP_RAM_SIZE] = [0; CHIP_RAM_SIZE];

/// Test ROM image: smaller than the 512 KB window, so the bus's
/// undersized-ROM mirroring path gets exercised too. Content is
/// arbitrary; only the read-back and mirroring behaviour is checked.
const TEST_ROM_LEN: usize = ROM_WINDOW_SIZE / 4;
static TEST_ROM: [u8; TEST_ROM_LEN] = build_test_rom();

const fn build_test_rom() -> [u8; TEST_ROM_LEN] {
    let mut rom = [0u8; TEST_ROM_LEN];
    rom[0] = 0x11;
    rom[4] = 0x22;
    rom
}

/// Run the machine-core bus checks, printing PASS/FAIL per check.
///
/// Mirrors `board-qemu-virt`'s checks (and `machine-core`'s own unit
/// tests): open-bus read at an unmapped Gayle-ID-like address, chip RAM
/// write/read roundtrip, ROM read plus undersized-ROM mirroring. There is
/// no CPU core to step (see module doc comment / `phase0-findings.md`),
/// so this is bus-only.
fn run_checks() -> bool {
    // SAFETY: single-threaded, single-call-site access to the static mut
    // buffer; `addr_of_mut!` avoids creating an intermediate `&mut`
    // reference to the whole static before we have exclusive access.
    let chip_ram: &mut [u8; CHIP_RAM_SIZE] = unsafe { &mut *addr_of_mut!(CHIP_RAM) };
    let mut bus = MachineBus::new(chip_ram, &TEST_ROM);

    let mut all_passed = true;

    // This probed $DE1000 until Phase 3, which was the proposal's Gayle ID
    // address -- chosen because nothing answered there, and then Gayle
    // landed and legitimately claimed it (`gayle::GAYLE_ID_BASE`), failing
    // the check on a correctly behaving machine. $C80000 sits in a gap the
    // memory map actually reserves, between CIA_END ($C00000) and Gayle's
    // window, rather than one that merely happens to be empty today.
    const OPEN_BUS_PROBE: u32 = 0x00C8_0000;
    let open_bus_ok = bus.read_long(OPEN_BUS_PROBE) == 0xFFFF_FFFF;
    report_check(
        "open-bus read at $C80000 == $FFFFFFFF",
        open_bus_ok,
        &mut all_passed,
    );

    bus.write_long(0x0010_0000, 0xDEAD_BEEF);
    let chip_ram_ok = bus.read_long(0x0010_0000) == 0xDEAD_BEEF;
    report_check(
        "chip RAM write/read roundtrip",
        chip_ram_ok,
        &mut all_passed,
    );

    let rom_base_ok = bus.read_byte(machine_core::ROM_BASE) == 0x11;
    let rom_offset_ok = bus.read_byte(machine_core::ROM_BASE + 4) == 0x22;
    report_check("ROM read", rom_base_ok && rom_offset_ok, &mut all_passed);

    let mirror_addr = machine_core::ROM_BASE + TEST_ROM_LEN as u32;
    let rom_mirror_ok = bus.read_byte(mirror_addr) == 0x11;
    report_check("ROM mirroring", rom_mirror_ok, &mut all_passed);

    all_passed
}

fn report_check(name: &str, passed: bool, all_passed: &mut bool) {
    if passed {
        print_line(&format_check(name, true));
    } else {
        print_line(&format_check(name, false));
        *all_passed = false;
    }
}

/// Format a `PASS`/`FAIL` line without `alloc`: a small fixed-size buffer
/// and `core::fmt::Write` stand in for `format!`.
fn format_check(name: &str, passed: bool) -> heapless_line::Line {
    let mut line = heapless_line::Line::new();
    let _ = write!(line, "[{}] {name}", if passed { "PASS" } else { "FAIL" });
    line
}

/// Tiny fixed-capacity string buffer, just enough to avoid pulling in
/// `alloc` for a handful of short status lines. Not a general-purpose
/// utility; local to this board's Phase 0 smoke test.
mod heapless_line {
    use core::fmt;
    use core::ops::Deref;

    const CAPACITY: usize = 128;

    pub struct Line {
        buf: [u8; CAPACITY],
        len: usize,
    }

    impl Line {
        pub fn new() -> Self {
            Self {
                buf: [0; CAPACITY],
                len: 0,
            }
        }
    }

    impl fmt::Write for Line {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            let bytes = s.as_bytes();
            let remaining = CAPACITY - self.len;
            let take = bytes.len().min(remaining);
            self.buf[self.len..self.len + take].copy_from_slice(&bytes[..take]);
            self.len += take;
            Ok(())
        }
    }

    impl Deref for Line {
        type Target = str;

        fn deref(&self) -> &str {
            // SAFETY: only ever written via `write_str`, which only copies
            // valid UTF-8 (`&str` bytes) and never splits a multi-byte
            // sequence at the truncation boundary in practice for the
            // short ASCII lines this board prints.
            core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
        }
    }
}

/// Defensively enable SSE state before touching `m68k::CpuCore` — the x86
/// analogue of `board-qemu-virt`'s `_start` enabling FP/SIMD before its
/// first CPU-linked call (see that crate's `main.rs` module doc comment,
/// and the commit that adopted the `no_std` `m68k` fork, for the root
/// cause: `CpuCore::new()`'s ~1.5 KB struct literal lowers to vector
/// (there, NEON; here, SSE) load/store instructions regardless of whether
/// the source touches floating point, so an unready FPU/SIMD state faults
/// or misbehaves the instant that call runs, not at some later, easier-
/// to-diagnose point).
///
/// UEFI firmware is required by spec to leave the CPU in a state where
/// SSE is already usable — the x86-64 UEFI calling convention itself
/// passes some arguments in XMM registers — so this is expected to be a
/// no-op under OVMF/edk2. It is cheap defensive setup, not a workaround
/// for an observed failure: unlike the aarch64 EL1/EL2 trap (which had no
/// vector table to land in and produced a silent hang), a real
/// `#UD`/`#NM` here would be a visible CPU exception, but nothing in this
/// `no_std` payload installs an IDT to report one legibly, so it is worth
/// ruling out up front rather than debugging blind if AROS ever stalls
/// immediately after `CpuCore::new()`.
fn enable_sse() {
    unsafe {
        let mut cr0: u64;
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        cr0 &= !(1 << 2); // clear EM: no x87/SSE emulation
        cr0 |= 1 << 1; // set MP: monitor coprocessor (WAIT/FWAIT traps on TS)
        asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack, preserves_flags));

        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        cr4 |= (1 << 9) | (1 << 10); // OSFXSR, OSXMMEXCPT: required for SSE
        asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack, preserves_flags));
    }
}

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();
    enable_sse();

    print_line("m68k Machine -- board-qemu-q35 -- Phase 0");
    print_line("hello from x86-64 UEFI (QEMU q35 / OVMF)");

    let all_passed = run_checks();

    if all_passed {
        print_line("PHASE0 BOARD-QEMU-Q35: ALL CHECKS PASSED");
    } else {
        print_line("PHASE0 BOARD-QEMU-Q35: CHECKS FAILED");
    }

    boot_aros();

    // Nothing left to do: park the core. The CI/dev harness kills QEMU
    // once it has seen the marker line above rather than waiting for this
    // to return (returning would hand control back to firmware, which
    // would then fall through to a boot menu or reset -- `hlt` looping
    // here is simpler and matches board-qemu-virt).
    loop {
        unsafe { asm!("hlt", options(nomem, nostack)) };
    }
}

/// The AROS 68k ROM pair (proposal §11.2), embedded directly into this
/// binary: there is no filesystem on bare metal to load them from at run
/// time. Freely redistributable (`assets/aros/LICENSE`,
/// `assets/aros/PROVENANCE.md`) — unlike Kickstart, which is why this is
/// the public CI boot target rather than a real Kickstart image.
static AROS_ROM: &[u8] = include_bytes!("../../../assets/aros/aros-amiga-m68k-rom.bin");
static AROS_EXT_ROM: &[u8] = include_bytes!("../../../assets/aros/aros-amiga-m68k-ext.bin");

/// Bounds and pacing for the AROS boot run below. See
/// `board-qemu-virt/src/main.rs`'s identical constants for the
/// measurement behind these numbers (a hosted run of the same ROM pair
/// reaches AROS's post-boot `STOP` idle, PC `$00FE8B88`, by frame ~40-50
/// and a few million instructions) — duplicated here rather than shared
/// because the two boards are independent `no_std` binary crates with no
/// common library to hold them.
const BOOT_MAX_FRAMES: u64 = 200;
const BOOT_MAX_INSTRUCTIONS: u64 = 50_000_000;
const BOOT_RUN_BATCH_CYCLES: i32 = 2_000_000;
const BOOT_PROGRESS_EVERY_FRAMES: u64 = 10;
/// See `board-qemu-virt`'s identical constant's doc comment.
const BOOT_TIGHT_LOOP_THRESHOLD: u64 = 5_000_000;
const BOOT_EXCEPTION_STORM_THRESHOLD: u64 = 1_000;

/// Adapts [`MachineBus`] to `m68k`'s [`AddressBus`] trait. Can't be a
/// blanket `impl` (both traits are foreign to this crate — orphan rule),
/// and can't be shared with `board-qemu-virt`'s identical adapter since
/// the two boards are independent crates.
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

/// Why the AROS boot run ended — see `board-qemu-virt`'s identical enum
/// for the full rationale (not shared for the same independent-crates
/// reason as `Bus` above).
enum BootOutcome {
    CleanHalt,
    LimitReached(&'static str),
    Wedged,
}

/// Buffers guest `SERDAT` bytes into lines, printed `GUEST |`-prefixed —
/// same split `machine-hosted`'s `Console` draws between guest output and
/// host diagnostics. Unlike `board-qemu-virt`'s fixed-capacity
/// equivalent, this can just use `alloc::string::String`: this crate
/// already needs `alloc` for `m68k::CpuCore` (see `Cargo.toml`), so there
/// is no reason to hand-roll a bounded buffer here too.
struct GuestLine {
    buf: String,
}

impl GuestLine {
    fn new() -> Self {
        Self { buf: String::new() }
    }

    fn push(&mut self, byte: u8) {
        match byte {
            b'\n' => self.flush(),
            b'\r' => {}
            _ => self.buf.push(byte as char),
        }
    }

    fn flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        print_line(&format!("GUEST | {}", self.buf));
        self.buf.clear();
    }
}

/// Drain any bytes the guest has written to `SERDAT` since the last call.
fn drain_serial(bus: &mut Bus, guest_line: &mut GuestLine) {
    while let Some(byte) = bus.0.chipset.take_serial_byte() {
        guest_line.push(byte);
    }
}

/// Report what `machine_core::rom::identify` could determine about one
/// embedded ROM image.
fn report_rom_identify(label: &str, image: &[u8]) {
    match machine_core::rom::identify(image) {
        Some(info) => {
            print_line(&format!(
                "{label} ROM: {:?} rev {}.{}, exec {}.{}, {} bytes, checksum {}, boot_pc {:#010x}",
                info.kind,
                info.rev.0,
                info.rev.1,
                info.exec_rev.0,
                info.exec_rev.1,
                info.size,
                if info.checksum_ok { "ok" } else { "FAILED" },
                info.boot_pc,
            ));
        }
        None => {
            print_line(&format!(
                "{label} ROM: unidentified ({} bytes) -- booting anyway",
                image.len()
            ));
        }
    }
}

/// Update the exception-storm tracker; returns `true` once the same kind
/// of exception has fired at the same PC `BOOT_EXCEPTION_STORM_THRESHOLD`
/// times in a row. Identical to `board-qemu-virt`'s helper of the same
/// name.
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
        print_line(&format!(
            "wedge: exception storm: {kind} at PC {pc:#010x} repeated {streak} times"
        ));
        true
    } else {
        false
    }
}

/// Boot the embedded AROS ROM pair on a real `m68k::CpuCore`, reporting
/// progress over serial, and print the `PHASE1 BOARD-QEMU-Q35:` marker
/// line CI greps for once the run ends.
///
/// Drives the CPU exactly as `machine-hosted/src/run.rs` documents (that
/// module's doc comment is the canonical source for this contract) and
/// exactly as `board-qemu-virt`'s `boot_aros` does: `run_for_cycles_with_
/// hook`, ticking the bus and resampling `pending_irq_level` from the
/// hook after every retired instruction, delivering A-line/F-line/TRAP/
/// BKPT/illegal exceptions via the matching `take_*_exception`, and
/// ticking the bus in raster-line slices while the CPU is `Stopped`
/// (`docs/phase0-findings.md`'s STOP/interrupt-resume contract) instead
/// of freezing machine time along with the CPU.
fn boot_aros() {
    print_line("");
    print_line("-- booting AROS 68k on a real m68k::CpuCore --");
    print_line("");

    report_rom_identify("main", AROS_ROM);
    report_rom_identify("ext", AROS_EXT_ROM);

    // Heap-allocate via the `uefi` crate's boot-services-backed allocator
    // (`Cargo.toml`'s `global_allocator` feature): 2 MB is too large for
    // this payload's stack, and unlike `board-qemu-virt` this board
    // already has a real allocator, so there is no need for a bump
    // arena — see this crate's `Cargo.toml` doc comment.
    let mut chip_ram: Box<[u8; CHIP_RAM_SIZE]> =
        match vec![0u8; CHIP_RAM_SIZE].into_boxed_slice().try_into() {
            Ok(b) => b,
            Err(_) => unreachable!("boxed_slice has exactly CHIP_RAM_SIZE elements"),
        };

    let machine_bus = MachineBus::new(&mut chip_ram, AROS_ROM).with_ext_rom(AROS_EXT_ROM);
    let mut bus = Bus(machine_bus);

    let mut cpu = CpuCore::new();
    cpu.set_cpu_type(CpuType::M68040);
    cpu.reset(&mut bus);

    print_line(&format!(
        "reset vector: SSP {:#010x} PC {:#010x} (overlay {})",
        cpu.sp(),
        cpu.pc,
        if bus.0.overlay() { "mapped" } else { "clear" }
    ));

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
                    print_line(&format!(
                        "wedge: tight loop at PC {pc:#010x} ({same_pc_streak} instructions with no progress)"
                    ));
                    hook_wedge = true;
                    return CycleBatchControl::Return;
                }

                let frames = bus.0.chipset.frames;
                if !overlay_was_cleared && !bus.0.overlay() {
                    overlay_was_cleared = true;
                    print_line(&format!(
                        "PHASE1 BOARD-QEMU-Q35: overlay cleared (frame {frames}, instr {total_instructions}, PC {:#010x})",
                        cpu.pc
                    ));
                }
                if frames >= last_progress_frame + BOOT_PROGRESS_EVERY_FRAMES {
                    last_progress_frame = frames;
                    print_line(&format!(
                        "progress: frame {frames}, PC {:#010x}, overlay {}, INTENA {:#06x}, INTREQ {:#06x}",
                        cpu.pc,
                        if bus.0.overlay() { "mapped" } else { "clear" },
                        bus.0.chipset.intena,
                        bus.0.chipset.intreq,
                    ));
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

                // The machine clock must keep advancing while STOPped --
                // see this function's doc comment and
                // `docs/phase0-findings.md`'s STOP section. Tick in
                // raster-line slices, never one `RUN_BATCH_CYCLES` gulp
                // (see `machine-hosted/src/run.rs`'s comment on why a
                // gulp undercounts VERTBs and CIA timer underflows).
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
                    print_line(&format!(
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
    let overlay_word = if overlay_cleared {
        "cleared"
    } else {
        "still mapped"
    };

    print_line("");
    match outcome {
        BootOutcome::CleanHalt => print_line(&format!(
            "PHASE1 BOARD-QEMU-Q35: CPU HALTED CLEANLY -- {total_instructions} instructions, {frames} frames, overlay {overlay_word}, final PC {final_pc:#010x}"
        )),
        BootOutcome::LimitReached(which) => print_line(&format!(
            "PHASE1 BOARD-QEMU-Q35: LIMIT REACHED ({which}) -- {total_instructions} instructions, {frames} frames, overlay {overlay_word}, final PC {final_pc:#010x}"
        )),
        BootOutcome::Wedged => print_line(&format!(
            "PHASE1 BOARD-QEMU-Q35: WEDGED -- {total_instructions} instructions, {frames} frames, overlay {overlay_word}, final PC {final_pc:#010x}"
        )),
    }

    // After -- never before -- the marker line above: CI polls for and
    // then kills QEMU on an earlier line (`ci-grep-serial.sh`'s doc
    // comment / `.github/workflows/ci.yml`), so any new narration has to
    // land after everything CI already asserts (this project's own past
    // CI race, referenced in the task brief this was written against).
    present_to_gop(&mut bus);
}

/// Reconstruct chip RAM's contents through the bus's own bounds-checked
/// read path. `MachineBus` borrows its backing array exclusively for its
/// whole lifetime and exposes no direct slice accessor -- same constraint
/// `machine-hosted`'s `screenshot.rs` documents for its identically-named
/// function -- so reading every byte back out through `bus.0.read_byte`
/// is what avoids aliasing the `&mut` `chip_ram` reference `bus` already
/// holds. Heap-backed (`Vec`), unlike `board-qemu-virt`'s static-buffer
/// equivalent: this board already uses the `uefi` crate's boot-services
/// allocator for `chip_ram` itself (this function's caller), so a second
/// 2 MB heap buffer costs nothing new. One-off cost, called once after
/// the boot run ends, never per emulated frame.
fn snapshot_chip_ram(bus: &mut Bus) -> Vec<u8> {
    (0..CHIP_RAM_SIZE as u32)
        .map(|offset| bus.0.read_byte(CHIP_RAM_BASE + offset))
        .collect()
}

/// Render the boot run's final picture and present it through the UEFI
/// Graphics Output Protocol, if one is available. Narrates the outcome
/// over serial (see this function's call site for why that has to happen
/// after the `PHASE1 BOARD-QEMU-Q35:` marker, not before).
///
/// **Uses `blt()`, not a raw framebuffer write.** GOP's `BltBuffer` format
/// (`gop.rs`'s own doc comment: "BGR 24-bit ... with an 8-bit padding") is
/// fixed regardless of the real framebuffer's `PixelFormat`/stride --
/// firmware does the conversion to whatever the hardware actually wants,
/// including the `PixelFormat::BltOnly` case where there is no
/// CPU-writable framebuffer to honour a stride for at all. That sidesteps
/// exactly the red/blue-swap and stride-vs-width mistakes a raw
/// `frame_buffer()` pointer write would risk, at the cost of one extra
/// buffer copy (`Vec<BltPixel>`, freed at the end of this function) --
/// worth it for a single end-of-boot presentation that is not on any hot
/// path.
fn present_to_gop(bus: &mut Bus) {
    let handle = match uefi::boot::get_handle_for_protocol::<GraphicsOutput>() {
        Ok(h) => h,
        Err(e) => {
            print_line(&format!(
                "display: no GraphicsOutput protocol handle found ({e:?}) -- skipping display presentation"
            ));
            return;
        }
    };
    let mut gop: ScopedProtocol<GraphicsOutput> = match uefi::boot::open_protocol_exclusive::<
        GraphicsOutput,
    >(handle)
    {
        Ok(g) => g,
        Err(e) => {
            print_line(&format!(
                    "display: failed to open GraphicsOutput exclusively ({e:?}) -- skipping display presentation"
                ));
            return;
        }
    };

    let mode = gop.current_mode_info();
    let (host_w, host_h) = mode.resolution();
    print_line(&format!(
        "display: GOP mode {}x{}, stride {} px/scanline, format {:?}",
        host_w,
        host_h,
        mode.stride(),
        mode.pixel_format()
    ));

    let chip_ram = snapshot_chip_ram(bus);
    let mut staging = vec![0u32; MAX_WIDTH * MAX_HEIGHT];
    let mut fb = Framebuffer::new(&mut staging, MAX_WIDTH, MAX_HEIGHT)
        .expect("staging is sized exactly to MAX_WIDTH * MAX_HEIGHT");
    Renderer::new().render(&bus.0.chipset, &chip_ram, &mut fb);

    let mut host_pixels = vec![0xFF00_0000u32; host_w * host_h]; // opaque black letterbox
    present::blit_scaled_centered(
        &mut host_pixels,
        host_w,
        host_h,
        &staging,
        MAX_WIDTH,
        MAX_HEIGHT,
        0xFF00_0000,
    );
    let blt_buffer: Vec<BltPixel> = host_pixels.iter().map(|&p| BltPixel::from(p)).collect();

    let factor = present::scale_factor(host_w, host_h, MAX_WIDTH, MAX_HEIGHT);
    match gop.blt(BltOp::BufferToVideo {
        buffer: &blt_buffer,
        src: BltRegion::Full,
        dest: (0, 0),
        dims: (host_w, host_h),
    }) {
        Ok(()) => print_line(&format!(
            "display: presented via GOP blt -- host {host_w}x{host_h}, amiga canvas {MAX_WIDTH}x{MAX_HEIGHT} at {factor}x integer scale"
        )),
        Err(e) => print_line(&format!("display: GOP blt failed ({e:?})")),
    }
}
