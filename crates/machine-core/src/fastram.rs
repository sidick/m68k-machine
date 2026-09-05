//! Fast RAM: 32-bit-addressable guest RAM beyond the 2 MB chip window.
//!
//! `hostblk` (`docs/hostblk-protocol.md`) is the immediate reason this
//! exists — its transfer buffers currently must live in chip RAM, because
//! chip RAM is all this machine has ever had, and a driver allocating with
//! `MEMF_FAST` would get `BAD_ADDRESS`. This module is memory, not a device
//! emulating a particular product (`docs/device-ledger.md`'s "permanent,
//! native" bucket): the storage is caller-owned (borrowed, like chip RAM
//! and VRAM — this crate has no allocator), and this module only decides
//! *how the guest is told the memory exists*.
//!
//! # Why AUTOCONFIG, not a fixed accelerator-style address
//!
//! Real hardware offers fast RAM to unmodified Kickstart two ways: a
//! Zorro AUTOCONFIG memory board with [`crate::autoconfig::ERTF_MEMLIST`]
//! set, which `expansion.library` links into the free-memory list
//! unaided; or a fixed address a specific machine's ROM knows to probe
//! itself (the A3000/A4000's 16 MB of motherboard fast RAM at `$07000000`
//! is the standing example, found by the ROM, not a card). A fixed
//! address needs the *ROM* to know to look there -- an accelerator
//! without a matching motherboard normally supplies that knowledge via
//! its own ROM calling `AddMemList` directly, which this machine has none
//! of, and the A1200 this project actually boots
//! (`nondistribution/A1200.47.115.rom`) has no motherboard fast-RAM slot
//! to probe in the first place.
//!
//! The real-hardware precedent that settles it either way is PiStorm: it
//! hands an unmodified Kickstart tens to hundreds of megabytes of
//! Pi-hosted RAM on exactly this kind of machine (A500/A1200, no
//! motherboard fast RAM), and it does so **over Zorro III AUTOCONFIG**,
//! not a fixed address -- if a fixed range were adoptable by a stock ROM
//! on this class of machine, it would be the simpler thing for PiStorm to
//! rely on, and it doesn't. So [`autoconfig_board_spec`] is this module's
//! only route; there is no fixed-address fallback to reach for later.
//!
//! # Zorro III extended sizing
//!
//! A memory board bigger than 8 MB cannot fit Zorro II's three `er_Type`
//! size bits (which only reach 8 MB, `graffity.rs` module docs), so
//! `er_Flags` bit 5 (`ERFF_EXTENDED`) redirects those bits to a second,
//! 16 MB-1 GB table instead. The NDK 3.2 header (`libraries/configregs.h`)
//! documents that this redirection exists but -- unlike the plain Zorro II
//! table it also spells out -- does not itself enumerate the extended
//! table's contents. The table below is transcribed from the primary
//! source that does, the Zorro III Bus Specification (Commodore/Haynie,
//! chapter 8, "Register Bit Assignments", the `er_Type` bits 2-0 row):
//!
//! | Code | Size    |
//! |------|---------|
//! | 000  | 16 MB   |
//! | 001  | 32 MB   |
//! | 010  | 64 MB   |
//! | 011  | 128 MB  |
//! | 100  | 256 MB  |
//! | 101  | 512 MB  |
//! | 110  | 1 GB    |
//! | 111  | reserved |
//!
//! Code 0 = 16 MB is the one entry this project had already exercised
//! (Graffity's and `hostblk`'s single-window Zorro III boards, both
//! exactly 16 MB and so both able to use code 0 without ever touching the
//! rest of the table); fast RAM is the first board here that needs an
//! arbitrary requested size, which is why the whole table is needed now.

use crate::autoconfig::{BoardSpec, ERTF_MEMLIST, ERT_ZORROIII};

/// **Commodore's own documented placeholder**, not `mirage`'s/`hostblk`'s
/// `0xFFFF`. `libraries/configregs.h`'s own doc comment reserves this
/// specific number for exactly this purpose ("A special 'hacker'
/// Manufacturer ID number is reserved for test use: 2011 ($7DB)"), so
/// unlike `0xFFFF` it is not squatting on a real vendor's ID.
///
/// This is not merely a stylistic choice: **real Kickstart 3.2.2's
/// `expansion.library` silently declines to configure a `MEMLIST` board
/// reporting manufacturer `0xFFFF`** -- confirmed empirically against
/// `nondistribution/A1200.47.115.rom` (the AUTOCONFIG base-address write
/// simply never lands; the board is skipped, exactly as if it had never
/// been offered). Switching to `0x07DB` was what made a real boot adopt
/// this board at all. `mirage`/`hostblk` have not hit this because they
/// don't set `ERTF_MEMLIST` -- whatever scrutiny `expansion.library`
/// applies to a would-be memory board's manufacturer ID evidently doesn't
/// reach an ordinary I/O board's -- but it is worth knowing this exists
/// before either of those boards ever needs `MEMLIST` themselves.
pub const MANUFACTURER: u16 = 0x07DB;

/// This board's product number under [`MANUFACTURER`]. `hostblk` and
/// MIRAGE use `0xFFFF` with their own product numbers, an entirely
/// separate placeholder identity from this one, so there's no collision
/// to avoid here -- picked as a small, easy-to-recognise value in a
/// `FindConfigDev` walk.
pub const PRODUCT: u8 = 2;

/// `er_Flags` bits RKRM `libraries/configregs.h` / the Zorro III Bus
/// Specification define for a Zorro III board (see `graffity.rs`'s module
/// docs, which this mirrors): [`ERFF_ZORRO_III`] must be set on every
/// genuine Zorro III board, and [`ERFF_EXTENDED`] says `er_Type`'s size
/// bits index the extended 16 MB-1 GB table (this module's docs) rather
/// than Zorro II's 64 KB-8 MB one.
const ERFF_ZORRO_III: u8 = 1 << 4;
const ERFF_EXTENDED: u8 = 1 << 5;

/// The extended Zorro III size table (module docs), smallest to largest.
/// `er_Type` bits 2-0 index this table when [`ERFF_EXTENDED`] is set.
const EXTENDED_SIZES: [(u32, u8); 7] = [
    (0x0100_0000, 0), // 16 MB
    (0x0200_0000, 1), // 32 MB
    (0x0400_0000, 2), // 64 MB
    (0x0800_0000, 3), // 128 MB
    (0x1000_0000, 4), // 256 MB
    (0x2000_0000, 5), // 512 MB
    (0x4000_0000, 6), // 1 GB
];

/// The smallest extended-table entry, 16 MB: nothing this small is worth
/// declaring as fast RAM, but this is also the floor the table itself
/// imposes (there is no smaller extended code).
pub const MIN_SIZE_BYTES: u32 = EXTENDED_SIZES[0].0;

/// The largest extended-table entry, 1 GB -- also the largest a single
/// Zorro III board can ever declare, extended table or not.
pub const MAX_SIZE_BYTES: u32 = EXTENDED_SIZES[EXTENDED_SIZES.len() - 1].0;

/// The extended-table code for an aperture of at least `bytes`, rounded up
/// to the nearest of the seven discrete sizes the table can express
/// (capped at 1 GB, code 6, the largest a Zorro III board can claim) --
/// the same "declare at least as much as the real backing store, rounding
/// up costs nothing since anything past the real buffer already reads
/// open bus" reasoning `graffity::zorro_ii_size_code` already documents
/// for the Zorro II table.
fn extended_size_code(bytes: u32) -> u8 {
    for &(size, code) in &EXTENDED_SIZES {
        if bytes <= size {
            return code;
        }
    }
    EXTENDED_SIZES[EXTENDED_SIZES.len() - 1].1
}

fn extended_size_bytes(code: u8) -> u32 {
    EXTENDED_SIZES
        .iter()
        .find(|&&(_, c)| c == code)
        .map(|&(size, _)| size)
        .unwrap_or(MAX_SIZE_BYTES)
}

/// The single `BoardSpec` fast RAM registers on the AUTOCONFIG chain: a
/// Zorro III memory board sized to at least `size_bytes` (rounded up to
/// the nearest [`EXTENDED_SIZES`] entry), with [`ERTF_MEMLIST`] set so
/// `expansion.library` links it into the system free-memory list without
/// any driver of ours -- the entire reason AUTOCONFIG is on the table at
/// all for a board that is nothing but RAM.
pub fn autoconfig_board_spec(size_bytes: u32) -> BoardSpec {
    let code = extended_size_code(size_bytes);
    BoardSpec {
        board_type: ERT_ZORROIII | ERTF_MEMLIST | code,
        product: PRODUCT,
        flags: ERFF_ZORRO_III | ERFF_EXTENDED,
        manufacturer: MANUFACTURER,
        serial: 0,
        init_diag_vec: 0,
        size_bytes: extended_size_bytes(code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoconfig::ERT_ZORROIII as ERT_Z3;

    #[test]
    fn extended_table_matches_the_zorro_iii_spec() {
        assert_eq!(extended_size_code(0x0100_0000), 0, "16 MB");
        assert_eq!(extended_size_code(0x0200_0000), 1, "32 MB");
        assert_eq!(extended_size_code(0x0400_0000), 2, "64 MB");
        assert_eq!(extended_size_code(0x0800_0000), 3, "128 MB");
        assert_eq!(extended_size_code(0x1000_0000), 4, "256 MB");
        assert_eq!(extended_size_code(0x2000_0000), 5, "512 MB");
        assert_eq!(extended_size_code(0x4000_0000), 6, "1 GB");
    }

    #[test]
    fn odd_sizes_round_up_to_the_next_extended_entry() {
        // 100 MB has no exact extended code; must round up to 128 MB, never
        // down (an under-declared window would leave real backing store
        // unreachable through it).
        let spec = autoconfig_board_spec(100 * 0x0010_0000);
        assert_eq!(spec.size_bytes, 0x0800_0000, "rounds up to 128 MB");
    }

    #[test]
    fn oversized_request_caps_at_one_gigabyte() {
        let spec = autoconfig_board_spec(u32::MAX);
        assert_eq!(spec.size_bytes, MAX_SIZE_BYTES);
    }

    #[test]
    fn board_spec_carries_memlist_and_extended_zorro_iii_encoding() {
        // 256 MB, the project's own target figure (proposal §13).
        let spec = autoconfig_board_spec(256 * 0x0010_0000);
        assert_eq!(spec.board_type & 0xC0, ERT_Z3, "Zorro III class bits");
        assert_eq!(
            spec.board_type & ERTF_MEMLIST,
            ERTF_MEMLIST,
            "must be linked into the free-memory list unaided"
        );
        assert_eq!(spec.board_type & 0x07, 4, "256 MB is extended code 4");
        assert_eq!(spec.flags, ERFF_ZORRO_III | ERFF_EXTENDED);
        assert_eq!(spec.manufacturer, MANUFACTURER);
        assert_eq!(spec.product, PRODUCT);
        assert_eq!(spec.size_bytes, 256 * 0x0010_0000);
    }
}
