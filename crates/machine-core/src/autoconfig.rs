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
//! four apart: the high nybble at `offset`, the low nybble at
//! `offset + 4`. Every nybble is **complemented except in `er_Type`**
//! (logical byte 0), which reads true. Getting the inversion or the
//! stride wrong makes a board look like garbage and the OS skips it, so
//! this is the part worth being careful about.
//!
//! # Configuration
//!
//! Writing the base address to `EC_BASEADDRESS` (Zorro II) or
//! `EC_Z3_BASEADDRESS` (Zorro III) configures the current board and
//! retires it from the window, so the next unconfigured board appears at
//! the same addresses. Writing `EC_SHUTUP` retires a board without
//! giving it space. When no boards remain the window reads as open bus,
//! which is how the OS knows the chain has ended.

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
/// `2*N` and `2*N + 4`).
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
// `current` is the chain cursor the read/write protocol walks; it is
// unread until that protocol lands. Remove this attribute along with
// the stubbed `read`/`write`.
#[allow(dead_code)]
pub struct AutoConfig {
    specs: [Option<BoardSpec>; MAX_BOARDS],
    placed: [Option<Configured>; MAX_BOARDS],
    /// Index of the board currently answering in the window, or
    /// `MAX_BOARDS` once the chain is exhausted.
    current: usize,
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
    pub fn read(&mut self, _address: u32) -> u8 {
        // Implemented in the AUTOCONFIG bring-up: the nybble protocol
        // described in this module's docs, over the current board's
        // ExpansionRom, and open bus once the chain is exhausted.
        crate::OPEN_BUS_BYTE
    }

    /// Write a byte to the configuration window.
    pub fn write(&mut self, _address: u32, _value: u8) {
        // Implemented in the AUTOCONFIG bring-up: base-address
        // assignment for Zorro II and III, SHUTUP, and advancing to the
        // next unconfigured board.
    }
}

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
}
