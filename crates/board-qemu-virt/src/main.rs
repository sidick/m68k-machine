//! Phase 0 smoke payload for QEMU's `virt` machine (aarch64).
//!
//! This is not a real board layer yet — no CPU stepping happens here,
//! because the `m68k` crate has no `no_std` support as of this writing
//! (see `docs/phase0-findings.md`), so it cannot be linked into a
//! bare-metal binary. This payload's job is narrower: prove that
//! `machine-core` links and behaves correctly on real bare-metal aarch64
//! hardware/QEMU (not just under `cargo test`'s hosted build), and that a
//! polled-UART serial path works end to end, by:
//!
//!   1. Printing a hello banner over the PL011 UART at QEMU virt's
//!      UART0 (`0x0900_0000`).
//!   2. Building a [`machine_core::MachineBus`] over a static 2 MB chip
//!      RAM buffer and a small test ROM, and running a few checks that
//!      exercise the open-bus rule, chip RAM read/write, and ROM
//!      mirroring — printing PASS/FAIL per check and a final marker line
//!      that CI greps for.
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

use core::fmt::Write;
use core::mem::MaybeUninit;
use core::panic::PanicInfo;
use core::ptr::addr_of_mut;

use machine_core::{MachineBus, CHIP_RAM_SIZE};

// `_start`: the ELF entry point QEMU's `-kernel` loader jumps to.
//
// Sets the stack pointer, zeroes `.bss` (linker-provided
// `__bss_start`/`__bss_end`), then calls `main`. `main` never returns, but
// the trailing `wfe` loop is a safety net in case it somehow does.
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "  ldr x0, =__stack_top",
    "  mov sp, x0",
    "  ldr x0, =__bss_start",
    "  ldr x1, =__bss_end",
    "1:",
    "  cmp x0, x1",
    "  b.ge 2f",
    "  str xzr, [x0], #8",
    "  b 1b",
    "2:",
    "  bl main",
    "3:",
    "  wfe",
    "  b 3b",
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
    if checks.all_passed {
        uprintln!("PHASE0 BOARD-QEMU-VIRT: ALL CHECKS PASSED");
    } else {
        uprintln!("PHASE0 BOARD-QEMU-VIRT: CHECKS FAILED");
    }

    park();
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
