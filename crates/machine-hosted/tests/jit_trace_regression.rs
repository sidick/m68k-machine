//! Regression test for ADR 0006's JIT requirement: "a regression test
//! loads and runs a program over memory that previously held other
//! code."
//!
//! Only compiled with `--features jit` (see this crate's `Cargo.toml`):
//! without the `jit` feature `m68k::CpuCore::run_batch` runs the portable
//! (non-compiling) batch executor, which has no trace cache and so
//! nothing here to regress against.
//!
//! # Why this is not a real-ROM test loading two executables from disk
//!
//! `docs/bus-fast-path-plan.md` step 7.2 asks, ideally, for a real-ROM
//! test that runs two different small executables from disk in sequence
//! into the same memory. That needs a booted Kickstart (to run
//! `expansion.library`'s AUTOCONFIG handshake and `LoadSeg` two HUNK
//! binaries into the same fast-RAM address), which is a much larger,
//! slower proof surface than the property being tested actually needs.
//! The property is narrow and address-level: does the trace JIT notice
//! when the bytes under a compiled trace change? That is fully exercised
//! by driving `m68k::CpuCore::run_batch` directly over a
//! `machine_core::MachineBus` with fast RAM attached (the exact
//! `AddressBus`/`FastMem` seam `--cpu-backend batch` uses), placing fast
//! RAM with the same AUTOCONFIG Z3 base-address handshake
//! `machine-core`'s own unit tests use (`autoconfig::ec::Z3_BASEADDRESS`,
//! never a hardcoded address -- `docs/device-ledger.md`), and writing two
//! hand-assembled programs to the exact same fast-RAM address in
//! sequence. This is the closest honest test that does not require a
//! booted OS, a HUNK loader, and two committed guest binaries just to
//! prove one address-level invalidation property.

#![cfg(feature = "jit")]

use m68k::{AddressBus, CpuCore, CpuType, FastMem};
use machine_core::{autoconfig, MachineBus, CHIP_RAM_SIZE, ROM_WINDOW_SIZE};

/// Thin `AddressBus` adapter, the same shape `machine-hosted`'s own
/// `bus.rs` uses and for the same orphan-rule reason its module doc
/// comment gives: both `MachineBus` and `AddressBus` are foreign to this
/// test binary.
struct TestBus<'a>(MachineBus<'a>);

impl AddressBus for TestBus<'_> {
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

    fn fast_mem(&mut self) -> Option<FastMem> {
        let (base, mem) = self.0.fast_ram_window_mut()?;
        if mem.len() < 4 {
            return None;
        }
        // SAFETY: `mem` borrows fast RAM's own backing buffer (owned by
        // this test function's `fast` local, `MachineBus::with_fast_ram`'s
        // `&'a mut` borrow), clamped to the AUTOCONFIG-placed window
        // (`fast_ram_window_mut`'s own doc comment) -- valid and
        // non-moving for as long as `self.0` lives, which trivially
        // outlives one `run_batch` call. Plain memory, no side effects.
        Some(FastMem {
            ptr: mem.as_mut_ptr(),
            base,
            len: mem.len() as u32,
        })
    }
}

/// Place fast RAM with the Zorro III AUTOCONFIG base-address handshake --
/// the same two-byte write sequence `machine-core`'s own
/// `configure_hostblk_z3` test helper uses. Never a hardcoded address for
/// the *placement itself*: `base` is chosen by the caller and read back
/// afterwards via `fast_ram_window_mut`, exactly as `device-ledger.md`'s
/// standing rule requires -- this only drives the same register writes a
/// real `expansion.library` would.
fn configure_fast_ram_z3(bus: &mut MachineBus, base: u32) {
    bus.write_byte(
        autoconfig::AUTOCONFIG_BASE + autoconfig::ec::Z3_BASEADDRESS,
        (base >> 24) as u8,
    );
    bus.write_byte(
        autoconfig::AUTOCONFIG_BASE + autoconfig::ec::Z3_BASEADDRESS + 1,
        (base >> 16) as u8,
    );
}

/// A tiny hand-assembled m68k program: loop ten times, then write one
/// marker byte to a fixed chip-RAM address, then spin forever. The two
/// programs below share this exact layout and differ only in the marker
/// byte, so writing program B over program A changes not just data but
/// the code bytes a compiled trace over this address range would have
/// validated against.
///
/// ```text
/// 0000  7000        moveq   #0,d0
/// 0002  5280        loop:   addq.l  #1,d0        ; backward-branch target
/// 0004  0C80 0000000A       cmpi.l  #10,d0
/// 000A  66F6        bne.s   loop
/// 000C  13FC 00xx 00100000  move.b  #marker,(MARKER_ADDR).L
/// 0014  60FE        forever: bra.s  forever      ; backward-branch target
/// ```
///
/// The marker address is an absolute *long* (not short) address: it must
/// sit above [`machine_core::OVERLAY_END`] (0x80000) so reading it back
/// afterwards sees chip RAM rather than the ROM overlay redirect
/// (`MachineBus::read_byte`'s own overlay handling) -- absolute short
/// addressing sign-extends to the top of the 32-bit space, nowhere near
/// [`MARKER_ADDR`], so the long form is needed to reach it.
fn program(marker: u8) -> [u8; 22] {
    [
        0x70, 0x00, // moveq #0,d0
        0x52, 0x80, // loop: addq.l #1,d0
        0x0C, 0x80, 0x00, 0x00, 0x00, 0x0A, // cmpi.l #10,d0
        0x66, 0xF6, // bne.s loop
        0x13, 0xFC, // move.b #marker,(MARKER_ADDR).L
        0x00, marker, // immediate source extension (byte in low byte)
        0x00, 0x10, 0x00, 0x00, // destination absolute-long extension: 0x00100000
        0x60, 0xFE, // forever: bra.s forever
    ]
}

const MARKER_ADDR: u32 = 0x0010_0000;

/// Write `bytes` into the bus starting at `addr`, one byte at a time
/// through the ordinary `write_byte` path -- this is test setup (loading
/// code into memory), not a guest access, so there is no need for the
/// wider bus to be involved beyond that.
fn load_bytes(bus: &mut MachineBus, addr: u32, bytes: &[u8]) {
    for (i, &b) in bytes.iter().enumerate() {
        bus.write_byte(addr + i as u32, b);
    }
}

/// Run `program` at fast RAM's placed base until it has clearly finished
/// its loop and settled into the `forever` spin, then return the marker
/// byte it wrote to chip RAM. A generous instruction budget: the loop
/// itself is ~40 instructions and the `forever` spin is meant to run
/// forever, so this only needs enough headroom to guarantee the loop
/// exited and the store executed, not to observe the spin itself.
fn run_program(bus: &mut TestBus, cpu: &mut CpuCore, code_base: u32) -> u8 {
    cpu.pc = code_base;
    cpu.set_sp(CHIP_RAM_SIZE as u32 - 0x100);
    // Clear the marker byte first so a run that failed to store anything
    // (rather than storing a stale value) is also caught, not confused
    // with success.
    bus.0.write_byte(MARKER_ADDR, 0);
    // Budget is generous: the loop is ~40 instructions total, so a run
    // that exhausts 500 instructions has certainly finished it and is
    // spinning in `forever` -- exactly the state needed to read the
    // marker back with confidence, regardless of exactly how the budget
    // was spent (`BatchExit::BudgetExhausted` is the expected, not an
    // error, outcome here).
    cpu.run_batch(bus, 500, &[]);
    bus.0.read_byte(MARKER_ADDR)
}

/// The regression ADR 0006 requires: with the `jit` feature enabled,
/// `run_batch` compiles the ten-iteration loop above into a native trace
/// (`TRACE_HOT_THRESHOLD` is 2 backward-branch hits in the pinned fork,
/// comfortably reached by iteration 3 of 10) keyed by its code address.
/// Overwriting that exact address range with a different program and
/// running again must execute the *new* code -- if the trace cache
/// reused the stale compiled trace instead of revalidating against the
/// live bytes, the second run would report program A's marker instead of
/// program B's.
#[test]
fn jit_trace_cache_does_not_run_stale_code_after_fast_ram_is_overwritten() {
    let mut ram = vec![0u8; CHIP_RAM_SIZE].into_boxed_slice();
    let ram: &mut [u8; CHIP_RAM_SIZE] = (&mut *ram).try_into().expect("exact size");
    let rom = [0u8; ROM_WINDOW_SIZE];
    let mut fast = vec![0u8; 0x10_0000]; // 1 MB, far more than the 22-byte program needs
    let machine_bus = MachineBus::new(ram, &rom).with_fast_ram(&mut fast);
    let mut bus = TestBus(machine_bus);

    let base = 0x4000_0000u32;
    configure_fast_ram_z3(&mut bus.0, base);
    let (code_base, _) = bus
        .0
        .fast_ram_window()
        .expect("fast RAM configured and placed");

    let mut cpu = CpuCore::new();
    cpu.set_cpu_type(CpuType::M68040);

    // Load and run program A ("marker A") at `code_base`, hot enough to
    // get JIT-compiled (the loop runs ten times, well past the 2-hit
    // threshold).
    let program_a = program(0x41); // 'A'
    load_bytes(&mut bus.0, code_base, &program_a);
    let marker_a = run_program(&mut bus, &mut cpu, code_base);
    assert_eq!(
        marker_a, 0x41,
        "program A did not run to completion the first time"
    );

    // Overwrite the exact same address range with program B ("marker
    // B") -- same code layout, different marker -- and run again from
    // the same entry point. A stale trace would still report 'A'.
    let program_b = program(0x42); // 'B'
    load_bytes(&mut bus.0, code_base, &program_b);
    let marker_b = run_program(&mut bus, &mut cpu, code_base);
    assert_eq!(
        marker_b, 0x42,
        "stale JIT trace ran program A's code after fast RAM was overwritten with program B \
         (ADR 0006's JIT regression requirement)"
    );
}
