//! Graffity — Atéo Concepts' Zorro II RTG board, this machine's
//! graphics card.
//!
//! Built on the same Cirrus Logic CL-GD542x silicon as the Picasso II
//! (see [`crate::cirrus`] for why the chip and board layers are split),
//! specifically the **CL-GD5428**: the same part the Picasso II+ uses,
//! so [`ChipRevision::Gd5428`]. P96 ships one shared `CirrusGD542X.chip`
//! under thin board-specific `.card` drivers, so nothing in the chip
//! model itself changes between boards — only the AUTOCONFIG identity
//! and window layout here differ from a Picasso II.
//!
//! # Two boards, one device
//!
//! Like the Picasso II, the card takes **two** consecutive AUTOCONFIG
//! entries backed by one device: [`PRODUCT_VRAM`] (34) is the linear
//! VRAM aperture, [`PRODUCT_REGS`] (33) the VGA-register and
//! monitor-switch window. The VRAM board is offered to the chain first,
//! matching the Picasso II shape (proposal §9 patterns one board's
//! ROM after another already-deployed one).
//!
//! # Simpler than the Picasso II
//!
//! The register window is [`REGS_WINDOW_BYTES`] — 128 KB, twice the
//! Picasso II's 64 KB — though only the low VGA port range within it is
//! actually wired to anything; the rest floats. Unlike the Picasso II,
//! there is no odd/even port-mirroring quirk: a window offset *is* the
//! VGA port number the chip's `reg_read`/`reg_write` already expect (see
//! the `port` module in [`crate::cirrus`]), so routing is a straight
//! `address - base`.
//!
//! Also unlike the Picasso II, Graffity has **no interrupt-enable latch
//! of its own** — INT2 follows the chip's own vertical-blank state
//! directly rather than being gated by a board register. This module
//! does not raise INT2 at all yet; that is a later step (vblank
//! interrupt delivery), left alone here so it cannot disturb a machine
//! that currently boots.

use crate::autoconfig::{BoardSpec, ERT_ZORROII};
use crate::cirrus::{ChipRevision, Cirrus542x, DecodedMode};

/// Atéo Concepts' registered expansion manufacturer ID.
pub const MANUFACTURER: u16 = 2092;

/// Linear VRAM aperture.
pub const PRODUCT_VRAM: u8 = 34;
/// VGA-register and monitor-switch window.
pub const PRODUCT_REGS: u8 = 33;

/// Size of the VGA-register aperture: 128 KB, twice the Picasso II's
/// 64 KB, though only the low VGA port range within it is live — the
/// rest floats open bus, same as any other unanswered address.
pub const REGS_WINDOW_BYTES: u32 = 0x0002_0000;

/// The revision this board always presents: Graffity is built on the
/// CL-GD5428, the same part the Picasso II+ uses.
pub const REVISION: ChipRevision = ChipRevision::Gd5428;

/// The eight Zorro II AUTOCONFIG aperture sizes (`er_Type` bits 2:0),
/// smallest to largest except that 8 MB is size code 0. RKRM Devices
/// (`ExpansionRom`).
const ZORRO_II_SIZES: [(u32, u8); 8] = [
    (0x0001_0000, 1), // 64 KB
    (0x0002_0000, 2), // 128 KB
    (0x0004_0000, 3), // 256 KB
    (0x0008_0000, 4), // 512 KB
    (0x0010_0000, 5), // 1 MB
    (0x0020_0000, 6), // 2 MB
    (0x0040_0000, 7), // 4 MB
    (0x0080_0000, 0), // 8 MB
];

/// The Zorro II size code for an aperture of at least `bytes`, rounded up
/// to the nearest of the eight discrete sizes AUTOCONFIG can express
/// (capped at 8 MB, the largest a Zorro II board can claim). The VRAM
/// aperture's declared size follows whatever VRAM the board layer
/// actually supplies via `with_graphics`, rather than a number hardcoded
/// here — bytes past the end of the real buffer already read open bus
/// and discard writes, the same as any other short backing store on this
/// bus, so rounding up costs nothing.
fn zorro_ii_size_code(bytes: u32) -> u8 {
    for &(size, code) in &ZORRO_II_SIZES {
        if bytes <= size {
            return code;
        }
    }
    0 // 8 MB: the largest Zorro II aperture, for anything bigger still.
}

fn zorro_ii_size_bytes(code: u8) -> u32 {
    ZORRO_II_SIZES
        .iter()
        .find(|&&(_, c)| c == code)
        .map(|&(size, _)| size)
        .unwrap_or(0x0080_0000)
}

/// The two `BoardSpec`s Graffity registers on the AUTOCONFIG chain, in
/// the order it offers them: VRAM first, then the register window,
/// matching the Picasso II shape.
pub fn board_specs(vram_len: u32) -> (BoardSpec, BoardSpec) {
    let vram_size_code = zorro_ii_size_code(vram_len);
    let vram = BoardSpec {
        board_type: ERT_ZORROII | vram_size_code,
        product: PRODUCT_VRAM,
        flags: 0,
        manufacturer: MANUFACTURER,
        serial: 0,
        init_diag_vec: 0,
        size_bytes: zorro_ii_size_bytes(vram_size_code),
    };
    let regs_size_code = zorro_ii_size_code(REGS_WINDOW_BYTES);
    let regs = BoardSpec {
        board_type: ERT_ZORROII | regs_size_code,
        product: PRODUCT_REGS,
        flags: 0,
        manufacturer: MANUFACTURER,
        serial: 0,
        init_diag_vec: 0,
        size_bytes: REGS_WINDOW_BYTES,
    };
    (vram, regs)
}

/// The Graffity board: a [`Cirrus542x`] chip given Graffity's AUTOCONFIG
/// identity and window layout. See the module docs for why the chip
/// model itself carries none of this.
pub struct Graffity<'a> {
    chip: Cirrus542x<'a>,
}

impl<'a> Graffity<'a> {
    /// Build a card over caller-owned VRAM (borrowed, like chip RAM and
    /// the ROMs — this crate has no allocator).
    pub fn new(vram: &'a mut [u8]) -> Self {
        Self {
            chip: Cirrus542x::new(REVISION, vram),
        }
    }

    /// The two `BoardSpec`s this card wants registered on the chain, for
    /// the current VRAM size.
    pub fn board_specs(&self) -> (BoardSpec, BoardSpec) {
        board_specs(self.chip.vram_len() as u32)
    }

    /// Read a byte of VRAM through the linear aperture, by offset from
    /// the aperture's configured base.
    pub fn vram_read(&self, offset: u32) -> u8 {
        self.chip.vram_read(offset as usize)
    }

    /// Write a byte of VRAM through the linear aperture.
    pub fn vram_write(&mut self, offset: u32, value: u8) {
        self.chip.vram_write(offset as usize, value);
    }

    /// Borrow the whole VRAM backing store, for a bulk RTG present path
    /// (see [`Cirrus542x::vram`]) rather than walking a frame one byte at
    /// a time through [`Graffity::vram_read`].
    pub fn vram(&self) -> &[u8] {
        self.chip.vram()
    }

    /// Read a VGA register. `offset` is the register window offset from
    /// its configured base, which *is* the VGA port number: Graffity's
    /// register window addresses VGA ports directly, unlike the Picasso
    /// II's odd/even mirroring (module docs).
    pub fn reg_read(&mut self, offset: u32) -> u8 {
        self.chip.reg_read(offset)
    }

    /// Write a VGA register.
    pub fn reg_write(&mut self, offset: u32, value: u8) {
        self.chip.reg_write(offset, value);
    }

    /// The mode the driver has programmed, or `None` while the card is
    /// not displaying anything the renderer can present.
    pub fn decoded_mode(&self) -> Option<DecodedMode> {
        self.chip.decoded_mode()
    }

    /// Look up a palette entry, expanded to eight bits per gun.
    pub fn palette_argb(&self, index: u8) -> u32 {
        self.chip.palette_argb(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_specs_carry_the_right_identity_and_sizes() {
        let (vram, regs) = board_specs(0x0020_0000); // 2 MB VRAM
        assert_eq!(vram.manufacturer, 2092);
        assert_eq!(regs.manufacturer, 2092);
        assert_eq!(vram.product, 34, "linear VRAM aperture");
        assert_eq!(regs.product, 33, "VGA-register window");
        assert_eq!(
            regs.size_bytes, 0x0002_0000,
            "128 KB, twice the Picasso II's"
        );
        assert_eq!(vram.size_bytes, 0x0020_0000);
        assert_eq!(vram.board_type & 0xC0, ERT_ZORROII);
        assert_eq!(regs.board_type & 0xC0, ERT_ZORROII);
    }

    #[test]
    fn vram_size_rounds_up_to_the_nearest_zorro_ii_aperture() {
        // A 1.5 MB buffer cannot be expressed exactly; AUTOCONFIG only
        // has eight discrete sizes, so the declared aperture must be at
        // least as big as the real backing store, never smaller.
        let (vram, _) = board_specs(0x0018_0000);
        assert_eq!(vram.size_bytes, 0x0020_0000, "rounds up to 2 MB");
    }

    #[test]
    fn both_boards_register_on_the_chain_vram_first() {
        let mut ac = crate::autoconfig::AutoConfig::new();
        let (vram, regs) = board_specs(0x0020_0000);
        assert_eq!(ac.add_board(vram), Some(0), "VRAM offered first");
        assert_eq!(ac.add_board(regs), Some(1), "then the register window");
    }

    #[test]
    fn vram_and_register_access_round_trip_through_the_chip() {
        let mut backing = [0u8; 256];
        let mut card = Graffity::new(&mut backing);
        card.vram_write(4, 0xAB);
        assert_eq!(card.vram_read(4), 0xAB);

        // CRTC index/data, the same protocol the chip model exercises
        // directly -- confirms the offset that reaches Graffity's window
        // is the bare VGA port number.
        card.reg_write(0x3D4, 0x0F); // CRTC_INDEX_COLOR
        card.reg_write(0x3D5, 0x55); // CRTC_DATA_COLOR
        assert_eq!(card.reg_read(0x3D5), 0x55);
    }
}
