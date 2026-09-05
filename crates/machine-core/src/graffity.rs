//! Graffity — Atéo Concepts' RTG board, this machine's graphics card.
//!
//! Built on the same Cirrus Logic CL-GD542x silicon as the Picasso II
//! (see [`crate::cirrus`] for why the chip and board layers are split),
//! specifically the **CL-GD5428**: the same part the Picasso II+ uses,
//! so [`ChipRevision::Gd5428`]. P96 ships one shared `CirrusGD542X.chip`
//! under thin board-specific `.card` drivers, so nothing in the chip
//! model itself changes between boards, buses, or the two variants
//! below — only the AUTOCONFIG identity and window layout here differ.
//!
//! # Two variants, one device
//!
//! Atéo Concepts sold Graffity as both a Zorro II and a Zorro III card;
//! [`BoardVariant`] selects which at construction. Both wrap the same
//! [`Cirrus542x`] core -- the AUTOCONFIG shape and address decode are
//! the only difference.
//!
//! ## Zorro II: two boards, one device
//!
//! Like the Picasso II, the Zorro II card takes **two** consecutive
//! AUTOCONFIG entries backed by one device: [`PRODUCT_VRAM`] (34) is the
//! linear VRAM aperture, [`PRODUCT_REGS`] (33) the VGA-register and
//! monitor-switch window. The VRAM board is offered to the chain first,
//! matching the Picasso II shape (proposal §9 patterns one board's
//! ROM after another already-deployed one).
//!
//! The register window is [`REGS_WINDOW_BYTES`] — 128 KB, twice the
//! Picasso II's 64 KB — though only the low VGA port range within it is
//! actually wired to anything; the rest floats. Unlike the Picasso II,
//! there is no odd/even port-mirroring quirk: a window offset *is* the
//! VGA port number the chip's `reg_read`/`reg_write` already expect (see
//! the `port` module in [`crate::cirrus`]), so routing is a straight
//! `address - base`.
//!
//! ## Zorro III: one 16 MB window, three sub-apertures
//!
//! The Zorro III card is a **single** AUTOCONFIG board (product
//! [`PRODUCT_Z3`], still manufacturer [`MANUFACTURER`]) claiming one
//! 16 MB window, inside which three fixed sub-apertures live:
//! [`Z3_SWITCH_BASE`] (a 64 KB monitor-switch strobe trap, write-only),
//! [`Z3_REGS_BASE`] (the 64 KB VGA-register window -- half the Zorro
//! II board's 128 KB, since there's no second AUTOCONFIG entry to pad
//! out), and [`Z3_VRAM_BASE`] (linear VRAM, running to the end of the
//! window). There is no VRAM mirror at offset zero -- everything below
//! `Z3_SWITCH_BASE` floats. These offsets are Graffity's own board
//! design, not part of the AUTOCONFIG spec itself, and no public
//! datasheet documents them; they are confirmed against Copperline's
//! `graffity.rs` (`GraffityZ3`), this project's oracle for the board,
//! whose sub-aperture constants and tests this module's Zorro III
//! decode matches byte for byte.
//!
//! A 16 MB Zorro III board cannot use `er_Type`'s three size bits the
//! way Zorro II does (those only reach 8 MB) -- RKRM
//! `libraries/configregs.h` reserves `er_Flags` bit 5 (`ERFF_EXTENDED`)
//! to mean "`er_Type`'s size bits index the 16 MB-1 GB extended table
//! instead", and bit 4 (`ERFF_ZORRO_III`) must also be set for a genuine
//! Zorro III board. 16 MB is extended-table code 0, so `er_Type` carries
//! no size bits of its own here (`ERT_ZORROIII` alone). Confirmed
//! against Copperline's `zorro_iii_size_bits`/`graffity_z3_is_a_single_
//! extended_size_zorro_iii_window` test, which asserts exactly this
//! `er_Type`/`er_Flags` pair for the same 16 MB Graffity Z3 window.
//!
//! Atéo Concepts reused product 33 (the Zorro II register board's
//! number) for the whole Zorro III window rather than allocating a new
//! one -- confirmed against Copperline's `GRAFFITY_Z3_PRODUCT` (also
//! 33) and its own doc comment citing the same reuse. The single
//! `Graffity.card` driver file P96 ships (`Picasso96Install/Libs/
//! Picasso96/Graffity.card`) auto-detects which bus generation it is
//! talking to by testing the `ConfigDev`'s `er_Type` `ERT_ZORROIII` bit
//! once `FindConfigDev` locates manufacturer 2092 / product 33 -- one
//! binary serves both boards, which is also why this machine's `amibake`
//! recipe installs the same `card = "graffity"` package regardless of
//! which variant is attached (`tools/amibake/m68k-machine.toml`).
//!
//! # Simpler than the Picasso II
//!
//! Also unlike the Picasso II, Graffity has **no interrupt-enable latch
//! of its own** — INT2 follows the chip's own vertical-retrace state
//! directly (CR11 bits 4-5, `cirrus::Cirrus542x::signal_vertical_
//! retrace`/`irq_pending`) rather than being gated by a board register.
//! [`Graffity::irq_pending`] forwards the chip's state; `lib.rs` drives
//! [`Graffity::signal_vertical_retrace`] once per frame from the
//! chipset's own frame clock and ORs the result onto the shared INT2
//! line, the same shape it already uses for MIRAGE and `hostblk`. Nor does it model the
//! monitor-switch strobe's
//! actual effect (switching the physical monitor between the chipset's
//! own display and the RTG one) -- writes to it are accepted and
//! discarded, the same simplification the Zorro II register window
//! already made for its corresponding bits before this module existed;
//! this machine's present path keys off [`Cirrus542x::decoded_mode`]
//! alone, not a switch flag.

use crate::autoconfig::{BoardSpec, ERT_ZORROII, ERT_ZORROIII};
use crate::cirrus::{ChipRevision, Cirrus542x, DecodedMode};

/// Atéo Concepts' registered expansion manufacturer ID, shared by both
/// variants.
pub const MANUFACTURER: u16 = 2092;

/// Zorro II linear VRAM aperture.
pub const PRODUCT_VRAM: u8 = 34;
/// Zorro II VGA-register and monitor-switch window.
pub const PRODUCT_REGS: u8 = 33;
/// Zorro III's single AUTOCONFIG identity -- Atéo Concepts reused the
/// Zorro II register board's product number (module docs).
pub const PRODUCT_Z3: u8 = 33;

/// Size of the Zorro II VGA-register aperture: 128 KB, twice the Picasso
/// II's 64 KB, though only the low VGA port range within it is live —
/// the rest floats open bus, same as any other unanswered address.
pub const REGS_WINDOW_BYTES: u32 = 0x0002_0000;

/// Size of Graffity [Zorro III]'s single AUTOCONFIG window.
pub const Z3_WINDOW_BYTES: u32 = 0x0100_0000; // 16 MB

/// Zorro III sub-aperture layout, board-relative within the 16 MB
/// window (module docs -- confirmed against Copperline's `graffity.rs`).
/// The monitor-switch strobe trap: 64 KB, write-only, decoded but not
/// acted on (module docs).
const Z3_SWITCH_BASE: u32 = 0x0040_0000;
const Z3_SWITCH_SIZE: u32 = 0x0001_0000;
/// The real VGA-register window, half the Zorro II board's 128 KB.
const Z3_REGS_BASE: u32 = 0x0080_0000;
const Z3_REGS_SIZE: u32 = 0x0001_0000;
/// Linear VRAM, running from here to the end of the 16 MB window (so at
/// most 4 MB is reachable through this aperture; VRAM past that is
/// simply unaddressable via Zorro III, the same way oversized Zorro II
/// VRAM already reads/writes nothing past the declared aperture).
const Z3_VRAM_BASE: u32 = 0x00C0_0000;

/// `er_Flags` bits RKRM `libraries/configregs.h` defines for a Zorro III
/// board (module docs): the board must assert [`ERFF_ZORRO_III`], and
/// [`ERFF_EXTENDED`] says `er_Type`'s size bits index the extended
/// 16 MB-1 GB table rather than Zorro II's 64 KB-8 MB one.
const ERFF_ZORRO_III: u8 = 1 << 4;
const ERFF_EXTENDED: u8 = 1 << 5;

/// The revision this board always presents: Graffity is built on the
/// CL-GD5428, the same part the Picasso II+ uses.
pub const REVISION: ChipRevision = ChipRevision::Gd5428;

/// Which Graffity bus generation a card presents (module docs). Selected
/// once, at construction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoardVariant {
    ZorroII,
    ZorroIII,
}

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

/// The two `BoardSpec`s Zorro II Graffity registers on the AUTOCONFIG
/// chain, in the order it offers them: VRAM first, then the register
/// window, matching the Picasso II shape.
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

/// The single `BoardSpec` Zorro III Graffity registers: one 16 MB
/// window, `er_Type`/`er_Flags` carrying the extended-size encoding
/// (module docs).
fn zorro_iii_board_spec() -> BoardSpec {
    BoardSpec {
        board_type: ERT_ZORROIII, // extended-table code 0 == 16 MB
        product: PRODUCT_Z3,
        flags: ERFF_ZORRO_III | ERFF_EXTENDED,
        manufacturer: MANUFACTURER,
        serial: 0,
        init_diag_vec: 0,
        size_bytes: Z3_WINDOW_BYTES,
    }
}

/// The most boards any Graffity variant registers at once: Zorro II's
/// two (VRAM, then registers); Zorro III needs only one. `lib.rs` sizes
/// its own chain-index map to this, without knowing why.
pub const MAX_GRAFFITY_BOARDS: usize = 2;

/// A variant's `BoardSpec`s, in AUTOCONFIG offer order. `no_std` with no
/// allocator rules out a `Vec` for a variable board count, so this is a
/// small fixed-capacity array plus a length, the way `machine-core`
/// already sizes everything else in the address-map layer.
pub struct BoardSpecs {
    specs: [BoardSpec; MAX_GRAFFITY_BOARDS],
    len: usize,
}

impl BoardSpecs {
    /// The specs this variant wants registered, in the order the card
    /// offers them to the chain.
    pub fn as_slice(&self) -> &[BoardSpec] {
        &self.specs[..self.len]
    }
}

/// The Graffity board: a [`Cirrus542x`] chip given Graffity's AUTOCONFIG
/// identity and window layout for whichever [`BoardVariant`] it was
/// built as. See the module docs for why the chip model itself carries
/// none of this.
pub struct Graffity<'a> {
    chip: Cirrus542x<'a>,
    variant: BoardVariant,
}

impl<'a> Graffity<'a> {
    /// Build a Zorro II card over caller-owned VRAM (borrowed, like chip
    /// RAM and the ROMs — this crate has no allocator). The variant most
    /// callers and every existing test want, so it keeps the plain name.
    pub fn new(vram: &'a mut [u8]) -> Self {
        Self::with_variant(BoardVariant::ZorroII, vram)
    }

    /// Build a Zorro III card over caller-owned VRAM: the same
    /// [`Cirrus542x`] core, one 16 MB AUTOCONFIG window instead of two
    /// Zorro II boards (module docs).
    pub fn new_zorro_iii(vram: &'a mut [u8]) -> Self {
        Self::with_variant(BoardVariant::ZorroIII, vram)
    }

    fn with_variant(variant: BoardVariant, vram: &'a mut [u8]) -> Self {
        Self {
            chip: Cirrus542x::new(REVISION, vram),
            variant,
        }
    }

    /// The `BoardSpec`s this card wants registered on the chain, for its
    /// variant and the current VRAM size, in offer order.
    pub fn board_specs(&self) -> BoardSpecs {
        match self.variant {
            BoardVariant::ZorroII => {
                let (vram, regs) = board_specs(self.chip.vram_len() as u32);
                BoardSpecs {
                    specs: [vram, regs],
                    len: 2,
                }
            }
            BoardVariant::ZorroIII => {
                let window = zorro_iii_board_spec();
                // The second slot is unused (`len` is 1); fill it with a
                // copy rather than reaching for an `Option`, since
                // `BoardSpec` is already `Copy` and nothing ever reads
                // past `len`.
                BoardSpecs {
                    specs: [window, window],
                    len: 1,
                }
            }
        }
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

    /// Tell the chip a display frame has crossed vertical retrace. The
    /// chip latches its own interrupt-pending flag from this only while
    /// CR11 has the interrupt armed -- see [`Cirrus542x::
    /// signal_vertical_retrace`]. `lib.rs` drives this from the chipset's
    /// existing frame clock (`BeamAdvance::frames_wrapped`) rather than a
    /// second clock of the board's own.
    pub fn signal_vertical_retrace(&mut self) {
        self.chip.signal_vertical_retrace();
    }

    /// Whether the chip is currently asserting its vertical-retrace
    /// interrupt, for `lib.rs` to OR onto the shared INT2 line the same
    /// way it already does for other devices sharing that line (MIRAGE,
    /// `hostblk`).
    pub fn irq_pending(&self) -> bool {
        self.chip.irq_pending()
    }

    /// Read a byte from board `board` -- an index into this card's own
    /// [`BoardSpecs`], in registration order -- at an offset relative to
    /// that board's configured base. The board-index seam: the bus
    /// layer (`lib.rs`) only needs to know which chain index maps to
    /// which of *this* card's boards; every sub-aperture decode (Zorro
    /// III's three windows, Zorro II's two-board split) lives here.
    /// Unmapped or floating offsets read open bus, like any other
    /// unanswered address on this bus.
    pub fn read(&mut self, board: usize, offset: u32) -> u8 {
        match (self.variant, board) {
            (BoardVariant::ZorroII, 0) => self.vram_read(offset),
            (BoardVariant::ZorroII, 1) => self.reg_read(offset),
            (BoardVariant::ZorroIII, 0) => self.zorro_iii_read(offset),
            _ => crate::OPEN_BUS_BYTE,
        }
    }

    /// Write a byte to board `board` at an offset relative to that
    /// board's configured base. See [`Graffity::read`].
    pub fn write(&mut self, board: usize, offset: u32, value: u8) {
        match (self.variant, board) {
            (BoardVariant::ZorroII, 0) => self.vram_write(offset, value),
            (BoardVariant::ZorroII, 1) => self.reg_write(offset, value),
            (BoardVariant::ZorroIII, 0) => self.zorro_iii_write(offset, value),
            _ => {}
        }
    }

    /// Zorro III's single-window sub-aperture read (module docs):
    /// linear VRAM, then the VGA-register window; the write-only switch
    /// strobe and everything else float.
    fn zorro_iii_read(&mut self, offset: u32) -> u8 {
        if let Some(vram_off) = self.zorro_iii_vram_offset(offset) {
            return self.vram_read(vram_off);
        }
        if (Z3_REGS_BASE..Z3_REGS_BASE + Z3_REGS_SIZE).contains(&offset) {
            return self.reg_read(offset - Z3_REGS_BASE);
        }
        crate::OPEN_BUS_BYTE
    }

    fn zorro_iii_write(&mut self, offset: u32, value: u8) {
        if (Z3_SWITCH_BASE..Z3_SWITCH_BASE + Z3_SWITCH_SIZE).contains(&offset) {
            // Monitor-switch strobe: decoded as a distinct sub-aperture
            // (so it never falls through to VRAM or the register
            // window) but not acted on -- module docs.
            return;
        }
        if let Some(vram_off) = self.zorro_iii_vram_offset(offset) {
            self.vram_write(vram_off, value);
            return;
        }
        if (Z3_REGS_BASE..Z3_REGS_BASE + Z3_REGS_SIZE).contains(&offset) {
            self.reg_write(offset - Z3_REGS_BASE, value);
        }
    }

    /// `offset`'s position in VRAM if it falls inside the Zorro III VRAM
    /// sub-aperture *and* the real backing store, `None` otherwise.
    /// `offset` is guest-controlled (any value AUTOCONFIG placed the
    /// board's window at is reachable from the CPU side), so this clips
    /// against the actual VRAM length rather than trusting it -- the
    /// same rule [`Cirrus542x::vram_read`]/`vram_write` already apply
    /// one level down.
    fn zorro_iii_vram_offset(&self, offset: u32) -> Option<u32> {
        let rel = offset.checked_sub(Z3_VRAM_BASE)?;
        (rel < self.chip.vram_len() as u32).then_some(rel)
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

    #[test]
    fn zorro_ii_board_seam_matches_the_direct_chip_calls() {
        // The board-index seam (`read`/`write`) must behave exactly like
        // the direct `vram_*`/`reg_*` calls above -- it's the same
        // routing, just addressed by board index instead of by method.
        let mut backing = [0u8; 256];
        let mut card = Graffity::new(&mut backing);
        card.write(0, 4, 0xAB); // board 0 -- VRAM
        assert_eq!(card.read(0, 4), 0xAB);
        card.write(1, 0x3D4, 0x0F); // board 1 -- registers
        card.write(1, 0x3D5, 0x55);
        assert_eq!(card.read(1, 0x3D5), 0x55);
        // No third board exists for Zorro II.
        assert_eq!(card.read(2, 0), crate::OPEN_BUS_BYTE);
    }

    // ---- Zorro III --------------------------------------------------

    #[test]
    fn zorro_iii_board_spec_carries_the_extended_size_encoding() {
        let mut backing = [0u8; 256];
        let card = Graffity::new_zorro_iii(&mut backing);
        let specs = card.board_specs();
        let slice = specs.as_slice();
        assert_eq!(slice.len(), 1, "one AUTOCONFIG board, not two");
        let window = slice[0];
        assert_eq!(window.manufacturer, MANUFACTURER);
        assert_eq!(window.product, PRODUCT_Z3);
        assert_eq!(window.product, PRODUCT_REGS, "reuses the Z2 regs product");
        assert_eq!(window.size_bytes, 0x0100_0000, "16 MB");
        // `er_Type`: ERT_ZORROIII with no size bits of its own -- 16 MB
        // is extended-table code 0. `er_Flags`: both the "genuine Zorro
        // III" and "extended size table" bits. Confirmed against
        // Copperline's `graffity_z3_is_a_single_extended_size_zorro_iii_
        // window` test (this module's oracle).
        assert_eq!(window.board_type, ERT_ZORROIII);
        assert_eq!(window.flags, ERFF_ZORRO_III | ERFF_EXTENDED);
    }

    #[test]
    fn zorro_iii_registers_one_board_on_the_chain() {
        let mut ac = crate::autoconfig::AutoConfig::new();
        let mut backing = [0u8; 256];
        let card = Graffity::new_zorro_iii(&mut backing);
        let specs = card.board_specs();
        assert_eq!(ac.add_board(specs.as_slice()[0]), Some(0));
    }

    #[test]
    fn zorro_iii_vram_lives_at_the_0xc00000_sub_aperture() {
        let mut backing = [0u8; 16];
        let mut card = Graffity::new_zorro_iii(&mut backing);
        card.write(0, Z3_VRAM_BASE + 4, 0xCD);
        assert_eq!(card.read(0, Z3_VRAM_BASE + 4), 0xCD);
        // Nothing aliases VRAM at offset zero -- unlike the sub-aperture
        // base, low window offsets float.
        assert_eq!(card.read(0, 4), crate::OPEN_BUS_BYTE);
    }

    #[test]
    fn zorro_iii_registers_live_at_the_0x800000_sub_aperture() {
        let mut backing = [0u8; 16];
        let mut card = Graffity::new_zorro_iii(&mut backing);
        card.write(0, Z3_REGS_BASE + 0x3D4, 0x0F); // CRTC_INDEX_COLOR
        card.write(0, Z3_REGS_BASE + 0x3D5, 0x55); // CRTC_DATA_COLOR
        assert_eq!(card.read(0, Z3_REGS_BASE + 0x3D5), 0x55);
    }

    #[test]
    fn zorro_iii_switch_strobe_is_accepted_and_never_reaches_registers() {
        let mut backing = [0u8; 16];
        let mut card = Graffity::new_zorro_iii(&mut backing);
        // A VGA-port-shaped offset inside the switch aperture must not
        // leak through to the register window it happens to resemble.
        card.write(0, Z3_SWITCH_BASE + 0x3D4, 0x0F);
        assert_eq!(card.reg_read(0x3D4), 0);
        // Reading the strobe trap itself floats -- it's write-only.
        assert_eq!(card.read(0, Z3_SWITCH_BASE), crate::OPEN_BUS_BYTE);
    }

    #[test]
    fn zorro_iii_out_of_window_and_past_vram_offsets_are_open_bus() {
        let mut backing = [0u8; 4];
        let mut card = Graffity::new_zorro_iii(&mut backing);
        assert_eq!(
            card.read(0, 0),
            crate::OPEN_BUS_BYTE,
            "before the switch trap"
        );
        assert_eq!(
            card.read(0, Z3_VRAM_BASE + 4),
            crate::OPEN_BUS_BYTE,
            "past the 4-byte backing store"
        );
        // A hostile offset right at the end of the address space must
        // not panic the subtraction in `zorro_iii_vram_offset`.
        assert_eq!(card.read(0, u32::MAX), crate::OPEN_BUS_BYTE);
        card.write(0, u32::MAX, 0xFF); // must not panic either
    }

    #[test]
    fn zorro_iii_board_seam_rejects_any_board_but_zero() {
        let mut backing = [0u8; 16];
        let mut card = Graffity::new_zorro_iii(&mut backing);
        assert_eq!(card.read(1, 0), crate::OPEN_BUS_BYTE);
        card.write(1, 0, 0xFF); // must not panic, must not reach board 0
        assert_eq!(card.read(0, 0), crate::OPEN_BUS_BYTE);
    }
}
