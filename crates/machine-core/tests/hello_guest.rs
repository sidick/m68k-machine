//! Hosted integration test: run a real guest instruction stream through
//! `MachineBus` and m68k-rs.
//!
//! This is deliberately hosted-only. `machine-core` itself has no
//! dependency on the `m68k` crate (see `docs/phase0-findings.md`: 0.12.1
//! has no `no_std` support), so `m68k` is a dev-dependency and this test
//! wires up a small local adapter that implements `m68k::AddressBus` by
//! forwarding to `MachineBus`'s own byte/word/long methods. The adapter
//! has to live here rather than in the library: `MachineBus` and
//! `AddressBus` are both foreign to this crate, so a blanket `impl` would
//! violate the orphan rule from either side.

use m68k::{AddressBus, CpuCore, CpuType, StepResult};
use machine_core::{MachineBus, CHIP_RAM_SIZE, ROM_BASE};

/// Adapts [`MachineBus`] to m68k-rs's [`AddressBus`] trait.
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

// Opcodes for the tiny guest program placed at the start of ROM.
const MOVEQ_42_D0: u16 = 0x7000 | 42; // MOVEQ #42,D0
const NOP: u16 = 0x4E71;

#[test]
fn hello_guest_moveq_and_nops() {
    // Heap-allocate chip RAM: a 2 MB array built directly on the stack
    // overflows a default test-thread stack (this bit us in the unit
    // tests too; see the `boxed_chip_ram` helper in src/lib.rs).
    let mut chip_ram: Box<[u8; CHIP_RAM_SIZE]> = vec![0u8; CHIP_RAM_SIZE]
        .into_boxed_slice()
        .try_into()
        .unwrap();
    let mut rom = [0u8; machine_core::ROM_WINDOW_SIZE];

    // `CpuCore::reset` reads the initial SSP/PC from absolute addresses
    // $000000/$000004. On real hardware those reads land in ROM because
    // Gary overlays ROM over low memory out of reset; the bus now models
    // that overlay (CIA-A PRA bit 0), so the vectors go at the start of
    // the ROM image where a real Kickstart keeps them, and the CPU picks
    // them up through the overlay exactly as it would on hardware.
    let initial_ssp: u32 = CHIP_RAM_SIZE as u32;
    let program_addr: u32 = ROM_BASE + 8;
    rom[0..4].copy_from_slice(&initial_ssp.to_be_bytes());
    rom[4..8].copy_from_slice(&program_addr.to_be_bytes());

    rom[8..10].copy_from_slice(&MOVEQ_42_D0.to_be_bytes());
    rom[10..12].copy_from_slice(&NOP.to_be_bytes());
    rom[12..14].copy_from_slice(&NOP.to_be_bytes());

    let machine_bus = MachineBus::new(&mut chip_ram, &rom);
    let mut bus = Bus(machine_bus);

    let mut cpu = CpuCore::new();
    cpu.set_cpu_type(CpuType::M68040);
    cpu.reset(&mut bus);

    assert_eq!(cpu.pc, program_addr, "PC should load from the reset vector");
    assert_eq!(
        cpu.sp(),
        initial_ssp,
        "SSP should load from the reset vector"
    );

    // Step MOVEQ #42,D0.
    let step = cpu.step(&mut bus);
    assert!(
        matches!(step, StepResult::Ok { .. }),
        "unexpected step result: {step:?}"
    );
    assert_eq!(cpu.d(0), 42, "D0 should hold the MOVEQ immediate");
    assert_eq!(cpu.pc, program_addr + 2, "PC should advance past MOVEQ");

    // Step the two NOPs.
    for _ in 0..2 {
        let step = cpu.step(&mut bus);
        assert!(
            matches!(step, StepResult::Ok { .. }),
            "unexpected step result: {step:?}"
        );
    }
    assert_eq!(cpu.pc, program_addr + 6, "PC should advance past both NOPs");
    assert_eq!(cpu.d(0), 42, "D0 should still hold 42 after the NOPs");
}
