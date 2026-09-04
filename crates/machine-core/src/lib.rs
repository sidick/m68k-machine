//! Machine core: the guest-visible address space of the m68k Machine.
//!
//! This crate is `#![no_std]` and has no runtime dependencies. It owns the
//! memory map (proposal `docs/m68k-machine-proposal.md` §6.1) and nothing
//! else yet: no chipset registers, no CIAs, no CPU. Those land in later
//! phases; Phase 0's job is a bus that a CPU core can read a reset vector
//! and a handful of instructions from.
//!
//! # Memory map (Phase 0 skeleton)
//!
//! | Range | Contents |
//! |---|---|
//! | `$000000`-`$1FFFFF` | 2 MB chip RAM, borrowed from the board layer |
//! | `$F80000`-`$FFFFFF` | 512 KB Kickstart ROM window, borrowed, read-only |
//! | everything else | open bus |
//!
//! The rest of proposal §6.1 (CIA space, custom chip registers, Zorro III,
//! fast RAM) is not implemented here; those addresses currently fall
//! through to the open-bus rule below, which is exactly what an
//! unpopulated real machine would do at those addresses today.
//!
//! # Open-bus rule
//!
//! An unanswered read returns `$FF` per byte (so `$FFFF` for a word,
//! `$FFFFFFFF` for a long), and a write to an unanswered address is
//! silently discarded. This is not a simplification of convenience: it is
//! proposal §6.1's open-bus rule, and it is load-bearing for boot
//! compatibility. Kickstart's Ramsey/Gary/Buster/Gayle/RTC probes are
//! written against Gary's real DTACK-timeout behaviour on a real Amiga
//! bus — they read a value and compare it, they do not rely on a bus
//! error exception — so reproducing "unpopulated address reads as all
//! ones" is what makes those probes fail cleanly instead of hanging or
//! crashing, and makes most of them unnecessary to stub out individually.

// `no_std` for real builds (including the aarch64-unknown-none target the
// board layers need); the `cfg(test)` unit tests below use `std::vec` for
// convenience and only run hosted, so `no_std` is relaxed for `cargo test`.
#![cfg_attr(not(test), no_std)]

/// Size in bytes of the chip RAM region, `$000000`-`$1FFFFF` (2 MB).
///
/// 2 MB matches the ECS Agnus chip RAM ID that Kickstart's memory probe
/// expects (proposal §6.1).
pub const CHIP_RAM_SIZE: usize = 0x0020_0000;

/// First address of the chip RAM region.
pub const CHIP_RAM_BASE: u32 = 0x0000_0000;

/// First address one past the end of the chip RAM region.
pub const CHIP_RAM_END: u32 = CHIP_RAM_BASE + CHIP_RAM_SIZE as u32;

/// Size in bytes of the Kickstart ROM window, `$F80000`-`$FFFFFF` (512 KB).
pub const ROM_WINDOW_SIZE: usize = 0x0008_0000;

/// First address of the Kickstart ROM window.
pub const ROM_BASE: u32 = 0x00F8_0000;

/// First address one past the end of the Kickstart ROM window.
pub const ROM_END: u32 = ROM_BASE + ROM_WINDOW_SIZE as u32;

/// A byte value returned by every open-bus read.
pub const OPEN_BUS_BYTE: u8 = 0xFF;

/// The m68k-visible address space of the machine.
///
/// `MachineBus` borrows its backing storage from the board layer rather
/// than owning it: chip RAM as a mutable fixed-size array reference, ROM
/// as a read-only byte slice. This crate has no allocator and no `alloc`
/// dependency, so the board layer is where that storage actually lives
/// (statically, or in whatever arena the platform provides).
///
/// ROM may be shorter than the 512 KB window; a real Kickstart is exactly
/// 512 KB, but this also supports smaller ROM images (e.g. test fixtures)
/// by mirroring the image across the window, matching how a real Amiga's
/// address decoder mirrors an undersized ROM across its window.
pub struct MachineBus<'a> {
    chip_ram: &'a mut [u8; CHIP_RAM_SIZE],
    rom: &'a [u8],
}

impl<'a> MachineBus<'a> {
    /// Build a bus over caller-owned chip RAM and ROM storage.
    ///
    /// `rom` must be non-empty if any ROM access is expected; an empty ROM
    /// slice makes the ROM window behave as open bus (reads all `$FF`)
    /// since there is nothing to mirror.
    pub fn new(chip_ram: &'a mut [u8; CHIP_RAM_SIZE], rom: &'a [u8]) -> Self {
        Self { chip_ram, rom }
    }

    /// Read one byte. Open-bus addresses return [`OPEN_BUS_BYTE`].
    pub fn read_byte(&mut self, address: u32) -> u8 {
        if (CHIP_RAM_BASE..CHIP_RAM_END).contains(&address) {
            self.chip_ram[(address - CHIP_RAM_BASE) as usize]
        } else if (ROM_BASE..ROM_END).contains(&address) && !self.rom.is_empty() {
            let offset = (address - ROM_BASE) as usize % self.rom.len();
            self.rom[offset]
        } else {
            OPEN_BUS_BYTE
        }
    }

    /// Read one big-endian 16-bit word, composed from two byte reads.
    pub fn read_word(&mut self, address: u32) -> u16 {
        let hi = self.read_byte(address) as u16;
        let lo = self.read_byte(address.wrapping_add(1)) as u16;
        (hi << 8) | lo
    }

    /// Read one big-endian 32-bit longword, composed from four byte reads.
    pub fn read_long(&mut self, address: u32) -> u32 {
        let hi = self.read_word(address) as u32;
        let lo = self.read_word(address.wrapping_add(2)) as u32;
        (hi << 16) | lo
    }

    /// Write one byte. Writes to chip RAM take effect; writes to ROM or
    /// open bus are silently discarded (real ROM cannot be written, and a
    /// real open-bus write simply has nothing latch it).
    pub fn write_byte(&mut self, address: u32, value: u8) {
        if (CHIP_RAM_BASE..CHIP_RAM_END).contains(&address) {
            self.chip_ram[(address - CHIP_RAM_BASE) as usize] = value;
        }
        // ROM and open-bus writes: discarded.
    }

    /// Write one big-endian 16-bit word, decomposed into two byte writes.
    pub fn write_word(&mut self, address: u32, value: u16) {
        self.write_byte(address, (value >> 8) as u8);
        self.write_byte(address.wrapping_add(1), value as u8);
    }

    /// Write one big-endian 32-bit longword, decomposed into two word
    /// writes.
    pub fn write_long(&mut self, address: u32, value: u32) {
        self.write_word(address, (value >> 16) as u16);
        self.write_word(address.wrapping_add(2), value as u16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_bus<'a>(chip_ram: &'a mut [u8; CHIP_RAM_SIZE], rom: &'a [u8]) -> MachineBus<'a> {
        MachineBus::new(chip_ram, rom)
    }

    // Chip RAM is 2 MB: too big for a default test-thread stack, so tests
    // heap-allocate it via `Box` rather than declaring it as a local
    // array. `machine-core` itself never does this (no `alloc`); it's a
    // hosted-test-only convenience.
    fn boxed_chip_ram() -> std::boxed::Box<[u8; CHIP_RAM_SIZE]> {
        // NB: `Box::new([0u8; CHIP_RAM_SIZE])` would build the 2 MB array
        // on the stack before moving it to the heap, which overflows a
        // default test-thread stack. Build a heap-allocated boxed slice
        // (via `vec!`) instead, then convert it to the fixed-size boxed
        // array `MachineBus::new` expects.
        let boxed_slice: std::boxed::Box<[u8]> = std::vec![0u8; CHIP_RAM_SIZE].into_boxed_slice();
        boxed_slice
            .try_into()
            .unwrap_or_else(|_| unreachable!("boxed_slice has exactly CHIP_RAM_SIZE elements"))
    }

    #[test]
    fn open_bus_reads_are_all_ones() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut bus = new_bus(&mut ram, &rom);

        // A representative sample of proposal §6.1's open-bus ranges:
        // slow RAM/unused, RTC, Gary/Ramsey, and the Gayle ID register the
        // A1200 3.2 ROM specifically probes.
        for &addr in &[0x00C0_0000u32, 0x00D8_0000, 0x00DE_0000, 0x00DE_1000] {
            assert_eq!(bus.read_byte(addr), 0xFF, "byte at {addr:#x}");
            assert_eq!(bus.read_word(addr), 0xFFFF, "word at {addr:#x}");
            assert_eq!(bus.read_long(addr), 0xFFFF_FFFF, "long at {addr:#x}");
        }
    }

    #[test]
    fn open_bus_writes_are_discarded() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut bus = new_bus(&mut ram, &rom);

        bus.write_long(0x00DE_1000, 0x1234_5678);
        assert_eq!(bus.read_long(0x00DE_1000), 0xFFFF_FFFF);
    }

    #[test]
    fn chip_ram_roundtrips() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut bus = new_bus(&mut ram, &rom);

        bus.write_byte(0x0000_0010, 0xAB);
        assert_eq!(bus.read_byte(0x0000_0010), 0xAB);

        bus.write_word(0x0010_0000, 0xBEEF);
        assert_eq!(bus.read_word(0x0010_0000), 0xBEEF);

        bus.write_long(CHIP_RAM_END - 4, 0xDEAD_BEEF);
        assert_eq!(bus.read_long(CHIP_RAM_END - 4), 0xDEAD_BEEF);
    }

    #[test]
    fn rom_reads_work_and_writes_are_discarded() {
        let mut ram = boxed_chip_ram();
        let mut rom = [0u8; ROM_WINDOW_SIZE];
        rom[0] = 0x11;
        rom[4] = 0x22;
        let mut bus = new_bus(&mut ram, &rom);

        assert_eq!(bus.read_byte(ROM_BASE), 0x11);
        assert_eq!(bus.read_byte(ROM_BASE + 4), 0x22);

        bus.write_byte(ROM_BASE, 0x99);
        assert_eq!(bus.read_byte(ROM_BASE), 0x11, "ROM write must be discarded");
    }

    #[test]
    fn undersized_rom_mirrors_across_the_window() {
        let mut ram = boxed_chip_ram();
        // A quarter-size ROM image should mirror four times across the
        // 512 KB window, the way a real address decoder ignores the
        // unused high address lines of an undersized ROM.
        let quarter = ROM_WINDOW_SIZE / 4;
        let mut small_rom = alloc_vec_zeroed(quarter);
        small_rom[0] = 0x42;
        let mut bus = new_bus(&mut ram, &small_rom);

        assert_eq!(bus.read_byte(ROM_BASE), 0x42);
        assert_eq!(bus.read_byte(ROM_BASE + quarter as u32), 0x42);
        assert_eq!(bus.read_byte(ROM_BASE + 2 * quarter as u32), 0x42);
        assert_eq!(bus.read_byte(ROM_BASE + 3 * quarter as u32), 0x42);
    }

    // Test-only helper: this crate has no `alloc` dependency, but the
    // hosted test binary links std, so a plain Vec is fine here.
    fn alloc_vec_zeroed(len: usize) -> std::vec::Vec<u8> {
        std::vec![0u8; len]
    }
}
