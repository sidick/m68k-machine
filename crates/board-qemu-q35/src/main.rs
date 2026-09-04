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

use core::arch::asm;
use core::fmt::Write;
use core::ptr::addr_of_mut;

use machine_core::{MachineBus, CHIP_RAM_SIZE, ROM_WINDOW_SIZE};
use uefi::prelude::*;

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

    let open_bus_ok = bus.read_long(0x00DE_1000) == 0xFFFF_FFFF;
    report_check(
        "open-bus read at $DE1000 == $FFFFFFFF",
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

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();

    print_line("m68k Machine -- board-qemu-q35 -- Phase 0");
    print_line("hello from x86-64 UEFI (QEMU q35 / OVMF)");

    let all_passed = run_checks();

    if all_passed {
        print_line("PHASE0 BOARD-QEMU-Q35: ALL CHECKS PASSED");
    } else {
        print_line("PHASE0 BOARD-QEMU-Q35: CHECKS FAILED");
    }

    // Nothing left to do: park the core. The CI/dev harness kills QEMU
    // once it has seen the marker line above rather than waiting for this
    // to return (returning would hand control back to firmware, which
    // would then fall through to a boot menu or reset -- `hlt` looping
    // here is simpler and matches board-qemu-virt).
    loop {
        unsafe { asm!("hlt", options(nomem, nostack)) };
    }
}
