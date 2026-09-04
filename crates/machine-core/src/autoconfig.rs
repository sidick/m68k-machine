//! Zorro II/III AUTOCONFIG (proposal §9).
//!
//! Every expansion this machine offers the guest arrives this way:
//! `expansion.library` probes the configuration window, reads a board's
//! identity, assigns it an address, and moves on to the next. Nothing —
//! not the Picasso II graphics card, not MIRAGE later — is discoverable
//! without it.
//!
//! # The nybble-encoded read protocol
//!
//! The configuration space is not ordinary memory. A board's 16-byte
//! `ExpansionRom` is presented one **nybble** at a time in the high four
//! bits of a byte, with each logical byte split across two addresses
//! *two* apart: the high nybble at `4*N`, the low nybble at `4*N + 2`,
//! for logical byte `N` (so consecutive logical bytes start four apart —
//! `4*N` and `4*(N+1)` — but the two nybbles of *one* byte are only two
//! apart). This is the layout the AmigaOS ROM Kernel Reference Manual's
//! sample AUTOCONFIG ROM table gives (nibble pairs `$00/$02` -> `er_Type`,
//! `$04/$06` -> `er_Product`, `$08/$0A` -> `er_Flags`, ...) and what
//! Copperline's `zorro.rs` (this project's oracle) implements. Odd
//! addresses, and anything at or past logical byte 16 (physical offset
//! `$40` and up, where the configuration registers below live), float.
//! Every nybble is **complemented except in `er_Type`** (logical byte 0),
//! which reads true. Getting the inversion or the stride wrong makes a
//! board look like garbage and the OS skips it, so this is the part
//! worth being careful about.
//!
//! # Configuration
//!
//! Writing the base address to `EC_BASEADDRESS` (Zorro II) or
//! `EC_Z3_BASEADDRESS` (Zorro III) configures the current board and
//! retires it from the window, so the next unconfigured board appears at
//! the same addresses. Writing `EC_SHUTUP` retires a board without
//! giving it space. When no boards remain the window reads as open bus,
//! which is how the OS knows the chain has ended.
//!
//! A Zorro II base is formed from a single byte write to
//! `EC_BASEADDRESS`: the byte becomes bits 23:16 of the base
//! (`base = byte << 16`), which alone is enough since every Zorro II
//! board size is 64 KiB-aligned or coarser. `EC_BASEADDRESS_LO` is
//! accepted (the ROM writes it, in either order) but does not affect
//! placement — confirmed against Copperline's
//! `low_nibble_then_high_byte_base_write_sequence_configures` test, which
//! writes `EC_BASEADDRESS_LO` first and shows it has no effect on its
//! own. A Zorro III base comes from a 16-bit write to
//! `EC_Z3_BASEADDRESS`, which this bus delivers as two byte writes (high
//! byte at the register, low byte at `+1`, per [`crate::write_word`]'s
//! big-endian convention); the base is `(hi << 24) | (lo << 16)`,
//! confirmed against Copperline's `z3_board_configures_via_word_write_to_44`
//! test.

/// The AUTOCONFIG window, `$E80000`-`$E8FFFF`.
pub const AUTOCONFIG_BASE: u32 = 0x00E8_0000;
pub const AUTOCONFIG_END: u32 = AUTOCONFIG_BASE + 0x0001_0000;

/// `er_Type` board-class bits.
pub const ERT_ZORROII: u8 = 0xC0;
pub const ERT_ZORROIII: u8 = 0x80;

/// `er_Type` flag bits.
/// The board's space should be added to the system free-memory list.
pub const ERTF_MEMLIST: u8 = 1 << 5;
/// The board carries a DiagArea ROM — the mechanism §6.3 adopts for
/// delivering host-provided drivers as Zorro boards.
pub const ERTF_DIAGVALID: u8 = 1 << 4;
/// This board is part of a chain sharing one configuration.
pub const ERTF_CHAINEDCONFIG: u8 = 1 << 3;

/// Offsets within the configuration window, in *physical* byte terms
/// (the nybble protocol above means a logical byte N lives at physical
/// `4*N` and `4*N + 2`).
pub mod ec {
    /// Zorro III base address, written as a word.
    pub const Z3_BASEADDRESS: u32 = 0x44;
    /// Zorro II base address, high byte.
    pub const BASEADDRESS: u32 = 0x48;
    /// Zorro II base address, low byte.
    pub const BASEADDRESS_LO: u32 = 0x4A;
    /// Retire this board without assigning it space.
    pub const SHUTUP: u32 = 0x4C;
}

/// The most boards this machine will ever offer at once. Picasso II
/// alone takes two (a linear VRAM aperture and a register window, both
/// backed by one device), so this is not as generous as it looks.
pub const MAX_BOARDS: usize = 8;

/// A board's AUTOCONFIG identity — what `expansion.library` reads to
/// decide what the board is and how much space to give it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoardSpec {
    /// `er_Type`: class bits (`ERT_*`), flags (`ERTF_*`), and the size
    /// code in the low three bits.
    pub board_type: u8,
    /// `er_Product`: the manufacturer's own product number.
    pub product: u8,
    /// `er_Flags`.
    pub flags: u8,
    /// `er_Manufacturer`: the allocated manufacturer number. Proposal §9
    /// expects this machine's own cards to use an ID from the Aminet
    /// expansion list; a board impersonating real hardware uses that
    /// vendor's.
    pub manufacturer: u16,
    /// `er_SerialNumber`. Real cards distinguish revisions here, which
    /// is how the Picasso II and II+ differ while sharing product IDs.
    pub serial: u32,
    /// `er_InitDiagVec`: offset of the DiagArea, when `ERTF_DIAGVALID`.
    pub init_diag_vec: u16,
    /// How much address space the board wants, in bytes. Must agree with
    /// the size code in `board_type`.
    pub size_bytes: u32,
}

/// Where a board ended up once the OS configured it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Configured {
    pub base: u32,
    pub size_bytes: u32,
}

/// The AUTOCONFIG chain.
pub struct AutoConfig {
    specs: [Option<BoardSpec>; MAX_BOARDS],
    placed: [Option<Configured>; MAX_BOARDS],
    /// Index of the board currently answering in the window, or
    /// `MAX_BOARDS` (or beyond) once the chain is exhausted.
    current: usize,
    /// The high byte of a Zorro III base address, latched by a write to
    /// `EC_Z3_BASEADDRESS` and consumed by the matching write to
    /// `EC_Z3_BASEADDRESS + 1` (see the module docs on how a 16-bit
    /// write becomes two byte writes on this bus). Cleared whenever the
    /// chain advances, so a partial sequence can never bleed into the
    /// next board.
    z3_base_hi: Option<u8>,
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoConfig {
    pub fn new() -> Self {
        Self {
            specs: [None; MAX_BOARDS],
            placed: [None; MAX_BOARDS],
            current: 0,
            z3_base_hi: None,
        }
    }

    /// Offer a board to the chain, in the order the OS will see them.
    /// Returns its index, or `None` when the chain is full.
    pub fn add_board(&mut self, spec: BoardSpec) -> Option<usize> {
        let idx = self.specs.iter().position(|s| s.is_none())?;
        self.specs[idx] = Some(spec);
        Some(idx)
    }

    /// Where the OS put board `index`, if it has configured it yet.
    pub fn placement(&self, index: usize) -> Option<Configured> {
        self.placed.get(index).copied().flatten()
    }

    /// The board owning `address`, if any board has been configured
    /// there. The bus uses this to route an access to the right device.
    pub fn board_at(&self, address: u32) -> Option<usize> {
        (0..MAX_BOARDS).find(|&i| {
            self.placed[i].is_some_and(|p| address >= p.base && address - p.base < p.size_bytes)
        })
    }

    /// True for any address in the configuration window.
    pub fn responds_to(address: u32) -> bool {
        (AUTOCONFIG_BASE..AUTOCONFIG_END).contains(&address)
    }

    /// Read a byte from the configuration window.
    pub fn read(&mut self, address: u32) -> u8 {
        let Some(spec) = self.current_spec() else {
            return crate::OPEN_BUS_BYTE;
        };
        let off = address.wrapping_sub(AUTOCONFIG_BASE);
        // Only even offsets inside the 16-byte nibble-encoded ROM (`0`
        // through `$3E`) carry real data; odd addresses and the
        // configuration-register area at `$40` and up (byte index >= 16)
        // float, same as an unanswered address anywhere else.
        if off & 1 != 0 {
            return crate::OPEN_BUS_BYTE;
        }
        let byte_idx = (off / 4) as usize;
        if byte_idx >= AUTOCONFIG_ROM_BYTES {
            return crate::OPEN_BUS_BYTE;
        }
        let logical = rom_bytes(&spec)[byte_idx];
        // er_Type (logical byte 0) reads true; everything else is
        // complemented.
        let physical = if byte_idx == 0 { logical } else { !logical };
        // `off` is even here (checked above); bit 1 distinguishes the
        // high-nybble address (`4*N`) from the low-nybble one (`4*N + 2`).
        let nibble = if off & 0x2 == 0 {
            physical >> 4
        } else {
            physical & 0x0F
        };
        nibble << 4
    }

    /// Write a byte to the configuration window.
    pub fn write(&mut self, address: u32, value: u8) {
        let Some(spec) = self.current_spec() else {
            return;
        };
        let off = address.wrapping_sub(AUTOCONFIG_BASE);
        let is_zorro_ii = spec.board_type & 0xC0 == ERT_ZORROII;
        let is_zorro_iii = spec.board_type & 0xC0 == ERT_ZORROIII;
        match off {
            ec::Z3_BASEADDRESS if is_zorro_iii => {
                self.z3_base_hi = Some(value);
            }
            off if off == ec::Z3_BASEADDRESS + 1 && is_zorro_iii => {
                if let Some(hi) = self.z3_base_hi.take() {
                    let base = (u32::from(hi) << 24) | (u32::from(value) << 16);
                    self.configure(spec, base);
                }
            }
            ec::BASEADDRESS if is_zorro_ii => {
                self.configure(spec, u32::from(value) << 16);
            }
            ec::BASEADDRESS_LO => {
                // The ROM writes this before or after EC_BASEADDRESS
                // depending on version; a Zorro II base is fully formed
                // by the single byte at EC_BASEADDRESS alone (see the
                // module docs), so this is accepted but inert.
            }
            ec::SHUTUP => {
                self.advance();
            }
            _ => {}
        }
    }

    /// The board currently answering in the window, if the chain has not
    /// been exhausted.
    fn current_spec(&self) -> Option<BoardSpec> {
        self.specs.get(self.current).copied().flatten()
    }

    /// Place the current board at `base` and advance the chain.
    fn configure(&mut self, spec: BoardSpec, base: u32) {
        self.placed[self.current] = Some(Configured {
            base,
            size_bytes: spec.size_bytes,
        });
        self.advance();
    }

    /// Retire the current board -- configured or shut up -- and let the
    /// next unconfigured board answer in the window.
    fn advance(&mut self) {
        self.current += 1;
        self.z3_base_hi = None;
    }
}

/// Build the 16-byte logical `ExpansionRom` image for `spec`, in the
/// struct's natural (un-inverted, un-nibbled) byte order -- what
/// `read`'s nibble encoder draws from.
fn rom_bytes(spec: &BoardSpec) -> [u8; AUTOCONFIG_ROM_BYTES] {
    let mut rom = [0u8; AUTOCONFIG_ROM_BYTES];
    rom[0] = spec.board_type; // er_Type
    rom[1] = spec.product; // er_Product
    rom[2] = spec.flags; // er_Flags
                         // rom[3] -- er_Reserved03, always 0.
    let m = spec.manufacturer.to_be_bytes();
    rom[4] = m[0];
    rom[5] = m[1]; // er_Manufacturer
    rom[6..10].copy_from_slice(&spec.serial.to_be_bytes()); // er_SerialNumber
    let d = spec.init_diag_vec.to_be_bytes();
    rom[10] = d[0];
    rom[11] = d[1]; // er_InitDiagVec
                    // rom[12..16] -- er_ReservedOc..Of, always 0.
    rom
}

/// Logical ROM bytes; see the module docs' nibble-encoding section.
const AUTOCONFIG_ROM_BYTES: usize = 16;

#[cfg(test)]
mod tests {
    use super::*;

    fn a_board() -> BoardSpec {
        BoardSpec {
            board_type: ERT_ZORROII | 0x01,
            product: 11,
            flags: 0,
            manufacturer: 2167,
            serial: 0,
            init_diag_vec: 0,
            size_bytes: 0x0010_0000,
        }
    }

    #[test]
    fn window_is_recognised_and_nothing_else_is() {
        assert!(AutoConfig::responds_to(AUTOCONFIG_BASE));
        assert!(AutoConfig::responds_to(AUTOCONFIG_BASE + 0x4C));
        assert!(!AutoConfig::responds_to(0x00DF_F000));
        assert!(!AutoConfig::responds_to(0x00E0_0000), "ext ROM is not us");
    }

    #[test]
    fn boards_queue_in_offer_order() {
        let mut ac = AutoConfig::new();
        assert_eq!(ac.add_board(a_board()), Some(0));
        assert_eq!(ac.add_board(a_board()), Some(1));
    }

    #[test]
    fn an_unconfigured_board_owns_no_address() {
        let mut ac = AutoConfig::new();
        ac.add_board(a_board());
        assert_eq!(ac.placement(0), None);
        assert_eq!(ac.board_at(0x0020_0000), None);
    }

    // ---- an empty chain must not change today's boot behaviour ---------

    #[test]
    fn empty_chain_reads_as_open_bus_everywhere_in_the_window() {
        let mut ac = AutoConfig::new();
        assert_eq!(ac.read(AUTOCONFIG_BASE), crate::OPEN_BUS_BYTE);
        assert_eq!(ac.read(AUTOCONFIG_BASE + 0x3E), crate::OPEN_BUS_BYTE);
        assert_eq!(ac.read(AUTOCONFIG_BASE + 0x48), crate::OPEN_BUS_BYTE);
        assert_eq!(ac.read(AUTOCONFIG_END - 1), crate::OPEN_BUS_BYTE);
    }

    #[test]
    fn writes_to_an_empty_chain_do_nothing() {
        let mut ac = AutoConfig::new();
        // Must not panic, and must not conjure a placement out of thin air.
        ac.write(AUTOCONFIG_BASE + ec::BASEADDRESS, 0x20);
        ac.write(AUTOCONFIG_BASE + ec::SHUTUP, 0);
        assert_eq!(ac.board_at(0x0020_0000), None);
    }

    // ---- the full nybble sequence, built independently ------------------

    fn richly_specified_board() -> BoardSpec {
        BoardSpec {
            board_type: ERT_ZORROII | ERTF_DIAGVALID | 0x05, // size code 5 = 1M
            product: 0xAB,
            flags: 0x80,
            manufacturer: 0x1234,
            serial: 0xDEAD_BEEF,
            init_diag_vec: 0x0080,
            size_bytes: 0x0010_0000,
        }
    }

    /// The logical (un-inverted, un-nibbled) `ExpansionRom` bytes for
    /// [`richly_specified_board`], built by hand from the struct layout
    /// the RKRM documents (`er_Type, er_Product, er_Flags, reserved,
    /// er_Manufacturer[2], er_SerialNumber[4], er_InitDiagVec[2],
    /// reserved[4]`) -- independent of `rom_bytes`, so this test cannot
    /// agree with a bug in that function.
    fn expected_logical_rom() -> [u8; 16] {
        [
            0xC0 | 0x10 | 0x05, // er_Type: ERT_ZORROII | ERTF_DIAGVALID | size 5
            0xAB,               // er_Product
            0x80,               // er_Flags
            0x00,               // er_Reserved03
            0x12,               // er_Manufacturer hi
            0x34,               // er_Manufacturer lo
            0xDE,               // er_SerialNumber byte 0
            0xAD,               // er_SerialNumber byte 1
            0xBE,               // er_SerialNumber byte 2
            0xEF,               // er_SerialNumber byte 3
            0x00,               // er_InitDiagVec hi
            0x80,               // er_InitDiagVec lo
            0x00,
            0x00,
            0x00,
            0x00, // er_Reserved0c..0f
        ]
    }

    #[test]
    fn nybble_sequence_matches_an_independently_built_expected_rom() {
        let mut ac = AutoConfig::new();
        ac.add_board(richly_specified_board());
        let expected = expected_logical_rom();

        for (byte_idx, &logical) in expected.iter().enumerate() {
            let hi_addr = AUTOCONFIG_BASE + (4 * byte_idx as u32);
            let lo_addr = hi_addr + 2;
            let physical = if byte_idx == 0 { logical } else { !logical };
            let expected_hi = (physical >> 4) << 4;
            let expected_lo = (physical & 0x0F) << 4;
            assert_eq!(ac.read(hi_addr), expected_hi, "byte {byte_idx} high nybble");
            assert_eq!(ac.read(lo_addr), expected_lo, "byte {byte_idx} low nybble");
        }

        // er_Type reads true (byte 0): the nybble must appear untouched,
        // not complemented -- board_type's high nybble is $D
        // (ERT_ZORROII | ERTF_DIAGVALID = $C0 | $10 = $D0).
        assert_eq!(ac.read(AUTOCONFIG_BASE) & 0xF0, 0xD0);

        // Odd addresses float, as does anything at or past the 16-byte
        // ROM (the configuration-register area starts at $40).
        assert_eq!(ac.read(AUTOCONFIG_BASE + 1), crate::OPEN_BUS_BYTE);
        assert_eq!(ac.read(AUTOCONFIG_BASE + 0x3F), crate::OPEN_BUS_BYTE);
        assert_eq!(ac.read(AUTOCONFIG_BASE + 0x40), crate::OPEN_BUS_BYTE);
        assert_eq!(ac.read(AUTOCONFIG_BASE + 0x1000), crate::OPEN_BUS_BYTE);
    }

    // ---- Zorro II configuration -----------------------------------------

    fn zorro_ii_ram(size_bytes: u32) -> BoardSpec {
        BoardSpec {
            board_type: ERT_ZORROII | 0x05, // size code 5 = 1M
            product: 3,
            flags: 0,
            manufacturer: 0x1448,
            serial: 0,
            init_diag_vec: 0,
            size_bytes,
        }
    }

    #[test]
    fn zorro_ii_base_write_places_the_board_and_board_at_resolves_it() {
        let mut ac = AutoConfig::new();
        ac.add_board(zorro_ii_ram(0x0010_0000)); // 1M

        // A single byte write to EC_BASEADDRESS: $20 -> base $200000.
        ac.write(AUTOCONFIG_BASE + ec::BASEADDRESS, 0x20);

        assert_eq!(
            ac.placement(0),
            Some(Configured {
                base: 0x0020_0000,
                size_bytes: 0x0010_0000,
            })
        );
        assert_eq!(ac.board_at(0x0020_0000), Some(0), "start of the window");
        assert_eq!(
            ac.board_at(0x0020_0000 + 0x0010_0000 - 1),
            Some(0),
            "last byte of the window"
        );
        assert_eq!(
            ac.board_at(0x0020_0000 + 0x0010_0000),
            None,
            "one past the end"
        );
        assert_eq!(ac.board_at(0x0010_0000), None, "before the base");
    }

    #[test]
    fn base_address_lo_alone_does_not_configure() {
        // Copperline's oracle test (`low_nibble_then_high_byte_base_write_
        // sequence_configures`) writes EC_BASEADDRESS_LO before
        // EC_BASEADDRESS and shows it has no effect on its own.
        let mut ac = AutoConfig::new();
        ac.add_board(zorro_ii_ram(0x0010_0000));

        ac.write(AUTOCONFIG_BASE + ec::BASEADDRESS_LO, 0x00);
        assert_eq!(ac.placement(0), None, "not yet configured");

        ac.write(AUTOCONFIG_BASE + ec::BASEADDRESS, 0x20);
        assert_eq!(ac.placement(0).map(|p| p.base), Some(0x0020_0000));
    }

    // ---- Zorro III configuration -----------------------------------------

    fn zorro_iii_ram(size_bytes: u32) -> BoardSpec {
        BoardSpec {
            board_type: ERT_ZORROIII, // extended size handling not exercised here
            product: 4,
            flags: 0,
            manufacturer: 0x1448,
            serial: 0,
            init_diag_vec: 0,
            size_bytes,
        }
    }

    #[test]
    fn zorro_iii_base_write_via_44_places_the_board() {
        let mut ac = AutoConfig::new();
        ac.add_board(zorro_iii_ram(0x0100_0000)); // 16M

        // A 16-bit write to EC_Z3_BASEADDRESS arrives as two byte writes:
        // high byte at $44, low byte at $45 (big-endian word convention).
        // hi=$40, lo=$00 -> base = $40000000.
        ac.write(AUTOCONFIG_BASE + ec::Z3_BASEADDRESS, 0x40);
        ac.write(AUTOCONFIG_BASE + ec::Z3_BASEADDRESS + 1, 0x00);

        assert_eq!(
            ac.placement(0),
            Some(Configured {
                base: 0x4000_0000,
                size_bytes: 0x0100_0000,
            })
        );
        assert_eq!(ac.board_at(0x4000_0000), Some(0));
        assert_eq!(ac.board_at(0x40FF_FFFF), Some(0));
        assert_eq!(ac.board_at(0x4100_0000), None);
    }

    // ---- SHUTUP ------------------------------------------------------

    #[test]
    fn shutup_retires_the_board_with_no_placement() {
        let mut ac = AutoConfig::new();
        ac.add_board(zorro_ii_ram(0x0010_0000));

        ac.write(AUTOCONFIG_BASE + ec::SHUTUP, 0);

        assert_eq!(ac.placement(0), None);
        assert_eq!(ac.board_at(0x0020_0000), None);
        // The chain has advanced: the window is now open bus (no more
        // boards), rather than still answering for the shut-up board.
        assert_eq!(ac.read(AUTOCONFIG_BASE), crate::OPEN_BUS_BYTE);
    }

    // ---- two boards in order: the Picasso II shape ------------------

    /// Recover logical ROM byte `byte_idx` from two `read()` calls,
    /// reversing the nybble encoding independently of `rom_bytes`/`read`
    /// themselves -- used to check *which* board's identity currently
    /// answers in the window.
    fn read_logical_byte(ac: &mut AutoConfig, byte_idx: u32) -> u8 {
        let hi = ac.read(AUTOCONFIG_BASE + 4 * byte_idx) >> 4;
        let lo = ac.read(AUTOCONFIG_BASE + 4 * byte_idx + 2) >> 4;
        let physical = (hi << 4) | lo;
        if byte_idx == 0 {
            physical
        } else {
            !physical
        }
    }

    #[test]
    fn second_board_appears_only_after_the_first_is_retired() {
        let mut ac = AutoConfig::new();
        let vram = BoardSpec {
            board_type: ERT_ZORROII | 0x05,
            product: 11, // PICASSO2_PRODUCT_VRAM
            flags: 0,
            manufacturer: 2167,
            serial: 0,
            init_diag_vec: 0,
            size_bytes: 0x0010_0000,
        };
        let regs = BoardSpec {
            board_type: ERT_ZORROII | 0x01,
            product: 12, // PICASSO2_PRODUCT_REGS
            flags: 0,
            manufacturer: 2167,
            serial: 0,
            init_diag_vec: 0,
            size_bytes: 0x0001_0000,
        };
        ac.add_board(vram);
        ac.add_board(regs);

        // Before board 0 configures, the window shows board 0's product
        // (byte index 1 -- er_Product).
        assert_eq!(read_logical_byte(&mut ac, 1), 11);

        ac.write(AUTOCONFIG_BASE + ec::BASEADDRESS, 0x20);
        assert_eq!(ac.placement(0).map(|p| p.base), Some(0x0020_0000));
        assert_eq!(ac.placement(1), None, "board 1 not configured yet");

        // Now the window shows board 1's product -- the same physical
        // addresses, a different board.
        assert_eq!(read_logical_byte(&mut ac, 1), 12);

        ac.write(AUTOCONFIG_BASE + ec::BASEADDRESS, 0xE9);
        assert_eq!(ac.placement(1).map(|p| p.base), Some(0x00E9_0000));

        assert_eq!(ac.board_at(0x0020_0000), Some(0));
        assert_eq!(ac.board_at(0x00E9_0000), Some(1));

        // Chain exhausted.
        assert_eq!(ac.read(AUTOCONFIG_BASE), crate::OPEN_BUS_BYTE);
    }

    // ---- addresses that are not part of the protocol --------------------

    #[test]
    fn non_protocol_addresses_in_the_window_behave_sanely() {
        let mut ac = AutoConfig::new();
        ac.add_board(zorro_ii_ram(0x0010_0000));

        // Deep in the window, well past the ROM and the config registers.
        assert_eq!(ac.read(AUTOCONFIG_BASE + 0x1234), crate::OPEN_BUS_BYTE);
        ac.write(AUTOCONFIG_BASE + 0x1234, 0xFF); // must not panic or configure
        assert_eq!(ac.placement(0), None);

        // A reserved nibble slot inside the ROM region (byte 3, er_Reserved03).
        assert_eq!(ac.read(AUTOCONFIG_BASE + 0x0C), 0xF0); // !0 high nybble
        assert_eq!(ac.read(AUTOCONFIG_BASE + 0x0E), 0xF0); // !0 low nybble
    }
}
