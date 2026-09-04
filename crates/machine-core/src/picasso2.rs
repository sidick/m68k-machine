//! Picasso II — a Village Tronic Zorro II RTG board around a Cirrus
//! Logic CL-GD5426, and this machine's first graphics card.
//!
//! Chosen because P96 already ships a `.card` driver for it and both
//! Copperline and Amiberry model it, so there are two independent
//! oracles to check against. That is the same argument that put Gayle
//! ahead of MIRAGE: present a register map an existing driver already
//! speaks, and no m68k code of ours is needed to get a screen. The
//! proposal's own virtual Zorro III framebuffer card (§8.2) remains the
//! production path and still has to be built.
//!
//! # Two boards, one device
//!
//! The card takes **two** AUTOCONFIG entries: product 11 is the linear
//! VRAM aperture and product 12 the 64 KB VGA-register window. Both are
//! backed by this one device, which is why the AUTOCONFIG chain had to
//! support several boards and shared backing from the start. The
//! original and the II+ share those product numbers and differ only by
//! serial, which is how P96 tells a CL-GD5426 from the II+'s CL-GD5428.
//!
//! # Deliberately not a VGA card
//!
//! No VGA text mode, no font or attribute machinery, no BIOS. The board
//! powers up behind the Amiga's native video and the RTG driver
//! programs a packed-pixel mode from scratch, so text rendering is
//! never exercised. That is a large scope reduction and it is the same
//! call Copperline makes for the same reason.

/// Village Tronic's registered expansion manufacturer ID.
pub const MANUFACTURER: u16 = 2167;

/// Linear VRAM aperture. (Product 13 is the segmented configuration the
/// physical board's jumper selects; unsupported here, as elsewhere.)
pub const PRODUCT_VRAM: u8 = 11;
/// VGA-register and monitor-switch window.
pub const PRODUCT_REGS: u8 = 12;

/// Serial of the original Picasso II (CL-GD5426).
pub const SERIAL_PICASSO2: u32 = 0x0002_0000;
/// Serial of the Picasso II+ (CL-GD5428). P96 distinguishes the
/// revisions by serial, not by product number.
pub const SERIAL_PICASSO2_PLUS: u32 = 0x0010_0000;

/// Size of the VGA-register aperture.
pub const REGS_WINDOW_BYTES: u32 = 0x0001_0000;

/// Which CL-GD542x revision this board presents.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChipRevision {
    /// CL-GD5426, the original Picasso II.
    Gd5426,
    /// CL-GD5428, the Picasso II+.
    Gd5428,
}

impl ChipRevision {
    /// The part ID the chip reports, which the driver reads to identify
    /// the silicon.
    pub fn part_id(self) -> u8 {
        match self {
            ChipRevision::Gd5426 => 0x90,
            ChipRevision::Gd5428 => 0x98,
        }
    }

    /// The AUTOCONFIG serial that goes with this revision.
    pub fn serial(self) -> u32 {
        match self {
            ChipRevision::Gd5426 => SERIAL_PICASSO2,
            ChipRevision::Gd5428 => SERIAL_PICASSO2_PLUS,
        }
    }
}

/// How many bits each pixel occupies in VRAM, once the driver has
/// programmed a mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PixelDepth {
    /// 8 bits per pixel through the palette.
    Bpp8,
    /// 15 bits per pixel, direct colour.
    Bpp15,
    /// 16 bits per pixel, direct colour.
    Bpp16,
    /// 24 bits per pixel, direct colour.
    Bpp24,
}

/// The display mode the driver has programmed, as far as the renderer
/// needs to care: enough to walk VRAM and produce pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecodedMode {
    pub width: u32,
    pub height: u32,
    pub depth: PixelDepth,
    /// Bytes between the start of one displayed row and the next.
    pub stride_bytes: u32,
    /// Byte offset into VRAM of the first displayed pixel — what
    /// `SetPanning` moves.
    pub start_offset: u32,
}

/// The card: its VGA register file, its palette, and its VRAM.
///
/// VRAM is borrowed rather than owned, like chip RAM on the bus: this
/// crate has no allocator, so the board layer supplies the storage.
// The VGA register files below are the state the register
// implementation needs and are unread until it lands, which clippy
// would otherwise reject. Remove this attribute along with the stubbed
// `reg_read`/`reg_write`/`decoded_mode`.
#[allow(dead_code)]
pub struct Picasso2<'a> {
    pub revision: ChipRevision,
    vram: &'a mut [u8],

    /// Sequencer, CRTC, graphics-controller and attribute-controller
    /// register files, plus the Cirrus extensions layered on them.
    sr: [u8; 32],
    crtc: [u8; 64],
    gr: [u8; 64],
    ar: [u8; 32],

    /// 256-entry palette, six bits per gun as the RAMDAC holds it.
    palette: [[u8; 3]; 256],
}

impl<'a> Picasso2<'a> {
    /// Build a card over caller-owned VRAM.
    pub fn new(revision: ChipRevision, vram: &'a mut [u8]) -> Self {
        Self {
            revision,
            vram,
            sr: [0; 32],
            crtc: [0; 64],
            gr: [0; 64],
            ar: [0; 32],
            palette: [[0; 3]; 256],
        }
    }

    pub fn vram_len(&self) -> usize {
        self.vram.len()
    }

    /// Read a byte of VRAM through the linear aperture.
    pub fn vram_read(&self, offset: usize) -> u8 {
        self.vram
            .get(offset)
            .copied()
            .unwrap_or(crate::OPEN_BUS_BYTE)
    }

    /// Write a byte of VRAM through the linear aperture.
    pub fn vram_write(&mut self, offset: usize, value: u8) {
        if let Some(slot) = self.vram.get_mut(offset) {
            *slot = value;
        }
    }

    /// Read a VGA register, by offset within the register window.
    pub fn reg_read(&mut self, _offset: u32) -> u8 {
        // Implemented in the Picasso II bring-up: the VGA port map
        // (sequencer, CRTC, graphics and attribute controllers, the
        // RAMDAC and its palette) plus the Cirrus extensions the RTG
        // driver programs.
        crate::OPEN_BUS_BYTE
    }

    /// Write a VGA register.
    pub fn reg_write(&mut self, _offset: u32, _value: u8) {
        // Implemented in the Picasso II bring-up.
    }

    /// The mode the driver has programmed, or `None` while the card is
    /// not displaying anything the renderer can present.
    pub fn decoded_mode(&self) -> Option<DecodedMode> {
        // Implemented in the Picasso II bring-up: width and height from
        // the CRTC, depth from the Cirrus hidden DAC register, stride
        // from the CRTC offset register, and the panning offset from the
        // CRTC start address.
        None
    }

    /// Look up a palette entry, expanded to eight bits per gun.
    pub fn palette_argb(&self, index: u8) -> u32 {
        let [r, g, b] = self.palette[index as usize];
        // The RAMDAC holds six bits per gun; replicate the top two bits
        // downward so full intensity is actually full, the same
        // reasoning as `display::argb_from_amiga`.
        let expand = |c: u8| -> u32 {
            let c = (c & 0x3F) as u32;
            (c << 2) | (c >> 4)
        };
        0xFF00_0000 | (expand(r) << 16) | (expand(g) << 8) | expand(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_report_their_own_part_id_and_serial() {
        assert_eq!(ChipRevision::Gd5426.part_id(), 0x90);
        assert_eq!(ChipRevision::Gd5428.part_id(), 0x98);
        assert_eq!(ChipRevision::Gd5426.serial(), SERIAL_PICASSO2);
        assert_eq!(ChipRevision::Gd5428.serial(), SERIAL_PICASSO2_PLUS);
    }

    #[test]
    fn vram_roundtrips_and_clamps() {
        let mut vram = [0u8; 64];
        let mut card = Picasso2::new(ChipRevision::Gd5426, &mut vram);
        card.vram_write(0, 0xAB);
        assert_eq!(card.vram_read(0), 0xAB);
        // Past the end reads open bus and a write is discarded, rather
        // than panicking: the guest picks the offset.
        card.vram_write(1_000_000, 0xFF);
        assert_eq!(card.vram_read(1_000_000), crate::OPEN_BUS_BYTE);
    }

    #[test]
    fn palette_full_intensity_is_actually_full() {
        let mut vram = [0u8; 16];
        let mut card = Picasso2::new(ChipRevision::Gd5426, &mut vram);
        card.palette[1] = [0x3F, 0x3F, 0x3F];
        assert_eq!(card.palette_argb(1), 0xFFFF_FFFF);
        assert_eq!(card.palette_argb(0), 0xFF00_0000);
    }
}
