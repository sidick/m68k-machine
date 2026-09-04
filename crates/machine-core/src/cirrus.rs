//! The Cirrus Logic CL-GD542x register model — the chip underneath this
//! machine's graphics cards.
//!
//! Chosen because P96 already ships a `.card` driver for boards built on
//! it and both Copperline and Amiberry model it, so there are two
//! independent oracles to check against. That is the same argument that
//! put Gayle ahead of MIRAGE: present a register map an existing driver
//! already speaks, and no m68k code of ours is needed to get a screen.
//! The proposal's own virtual Zorro III framebuffer card (§8.2) remains
//! the production path and still has to be built.
//!
//! # Deliberately not a VGA card
//!
//! No VGA text mode, no font or attribute machinery, no BIOS. A board
//! built on this chip powers up behind the Amiga's native video and the
//! RTG driver programs a packed-pixel mode from scratch, so text
//! rendering is never exercised. That is a large scope reduction and it
//! is the same call Copperline makes for the same reason.
//!
//! # Chip and board are kept separate
//!
//! [`Cirrus542x`] is the CL-GD5426/5428 register model on its own: the
//! VGA port map, the Cirrus extensions, VRAM and `decoded_mode`. It
//! knows nothing about AUTOCONFIG, product numbers or window layout —
//! P96 itself draws exactly this line, shipping one shared
//! `CirrusGD542X.chip` under thin board-specific `.card` drivers
//! (`PicassoII.card`, `Graffity.card`, ...). A board module (e.g.
//! [`crate::graffity`]) wraps this chip with its own AUTOCONFIG identity
//! and window layout rather than duplicating the register model.

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
    /// the silicon. Chip-level: read out of the CRTC's ID register
    /// (`idx::CRTC_ID`) regardless of which board the chip sits behind.
    pub fn part_id(self) -> u8 {
        match self {
            ChipRevision::Gd5426 => 0x90,
            ChipRevision::Gd5428 => 0x98,
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

impl PixelDepth {
    fn bytes_per_pixel(self) -> u32 {
        match self {
            PixelDepth::Bpp8 => 1,
            PixelDepth::Bpp15 | PixelDepth::Bpp16 => 2,
            PixelDepth::Bpp24 => 3,
        }
    }
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

/// Byte offsets within the register window, equal to the ISA VGA port
/// numbers they carry — this is a straight reflection of the real
/// board's address decode, not an emulator convention. Where two ports
/// alias one register on real VGA (CRTC and Input Status 1 each have a
/// mono- and colour-adapter address), both are honoured identically:
/// the board is always wired for a colour monitor, so nothing depends
/// on the Misc Output Register's I/O Address Select bit choosing
/// between them.
mod port {
    pub const MISC_OUTPUT: u32 = 0x3C2; // write: misc output; read: input status 0
    pub const SEQ_INDEX: u32 = 0x3C4;
    pub const SEQ_DATA: u32 = 0x3C5;
    /// Pixel mask; also the gate for the Cirrus hidden DAC register.
    pub const PEL_MASK: u32 = 0x3C6;
    pub const DAC_READ_INDEX: u32 = 0x3C7; // write: read index; read: DAC state
    pub const DAC_WRITE_INDEX: u32 = 0x3C8;
    pub const DAC_DATA: u32 = 0x3C9;
    pub const MISC_OUTPUT_READ: u32 = 0x3CC;
    pub const GR_INDEX: u32 = 0x3CE;
    pub const GR_DATA: u32 = 0x3CF;
    pub const AC_INDEX_DATA: u32 = 0x3C0;
    pub const AC_DATA_READ: u32 = 0x3C1;
    pub const CRTC_INDEX_MONO: u32 = 0x3B4;
    pub const CRTC_DATA_MONO: u32 = 0x3B5;
    pub const CRTC_INDEX_COLOR: u32 = 0x3D4;
    pub const CRTC_DATA_COLOR: u32 = 0x3D5;
    pub const INPUT_STATUS_1_MONO: u32 = 0x3BA;
    pub const INPUT_STATUS_1_COLOR: u32 = 0x3DA;
}

/// Register indices worth naming, within their respective index spaces.
mod idx {
    /// Sequencer: the Cirrus lock register. Writing the unlock code
    /// (`SR_LOCK_UNLOCKED`, masked by `SR_LOCK_UNLOCK_MASK`) to this index arms the
    /// extended registers (sequencer index > 6, CRTC index > `0x18`,
    /// graphics-controller index > 8); anything else re-locks them.
    pub const SR_LOCK: u8 = 0x06;
    pub const SR_LOCK_UNLOCKED: u8 = 0x12;
    pub const SR_LOCK_LOCKED: u8 = 0x0F;
    pub const SR_LOCK_UNLOCK_MASK: u8 = 0x17;
    /// Extended Sequencer Mode: bit 0 gates the packed-pixel path this
    /// model cares about; bits 1-3 select depth when the hidden DAC is
    /// in its "extended" state (hidden-DAC low nibble `0x0F`).
    pub const SR_EXTENDED_MODE: u8 = 0x07;

    /// CRTC: the chip-ID register the RTG driver reads to identify the
    /// silicon. Read-only.
    pub const CRTC_ID: u8 = 0x27;
    pub const CRTC_LAST_STANDARD: u8 = 0x18;
}

/// The Cirrus CL-GD5426/5428 register model on its own: the VGA port
/// map (sequencer, CRTC, graphics controller, attribute controller),
/// the RAMDAC and its palette, the Cirrus hidden-DAC and lock
/// extensions, and VRAM. Knows nothing about AUTOCONFIG, product
/// numbers, or how many apertures a board built on it exposes — see the
/// module docs on why that line is drawn here.
///
/// VRAM is borrowed rather than owned, like chip RAM on the bus: this
/// crate has no allocator, so the board layer supplies the storage.
pub struct Cirrus542x<'a> {
    revision: ChipRevision,
    vram: &'a mut [u8],

    /// Sequencer, CRTC, graphics-controller and attribute-controller
    /// register files, plus the Cirrus extensions layered on them.
    sr: [u8; 32],
    crtc: [u8; 64],
    gr: [u8; 64],
    ar: [u8; 32],

    /// 256-entry palette, six bits per gun as the RAMDAC holds it.
    palette: [[u8; 3]; 256],

    seq_index: u8,
    crtc_index: u8,
    gr_index: u8,
    /// Attribute-controller index (low five bits) plus the Palette
    /// Address Source bit software sets alongside it; not touched by
    /// the flip-flop itself.
    ar_index: u8,
    /// The attribute controller's index/data flip-flop: false means the
    /// next write to `port::AC_INDEX_DATA` loads the index, true means
    /// it writes data. Toggles on every write; reset to `false` by a
    /// read of either Input Status 1 port, which is what lets a driver
    /// resynchronise after being interrupted mid-sequence.
    ar_next_is_data: bool,

    misc_output: u8,
    pel_mask: u8,
    /// Cirrus hidden DAC: colour-depth selector reached only through
    /// the documented sequence of four consecutive pixel-mask reads
    /// (`hidden_dac_arm` counts them) while the extensions are
    /// unlocked; the read or write that follows hits this register
    /// instead of the pixel mask, and any access to a different port
    /// disarms the sequence.
    hidden_dac: u8,
    hidden_dac_arm: u8,

    dac_write_index: u8,
    dac_read_index: u8,
    /// Which of the three R/G/B writes-or-reads a palette-data access is
    /// on; auto-increments the read or write index after the third.
    dac_component: u8,
    dac_read_mode: bool,
}

impl<'a> Cirrus542x<'a> {
    pub fn new(revision: ChipRevision, vram: &'a mut [u8]) -> Self {
        Self {
            revision,
            vram,
            sr: [0; 32],
            crtc: [0; 64],
            gr: [0; 64],
            ar: [0; 32],
            palette: [[0; 3]; 256],
            seq_index: 0,
            crtc_index: 0,
            gr_index: 0,
            ar_index: 0,
            ar_next_is_data: false,
            misc_output: 0,
            // Power-on default: every plane visible until software
            // narrows it, the same as real RAMDAC reset state.
            pel_mask: 0xFF,
            hidden_dac: 0,
            hidden_dac_arm: 0,
            dac_write_index: 0,
            dac_read_index: 0,
            dac_component: 0,
            dac_read_mode: false,
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

    /// Read a VGA register, by offset within the register window (see
    /// `port`).
    pub fn reg_read(&mut self, offset: u32) -> u8 {
        // The hidden-DAC arming sequence survives only consecutive
        // accesses to the pixel-mask port; it manages its own arm
        // counter and must run before the general disarm below.
        if offset == port::PEL_MASK {
            return self.read_pel_mask_or_hidden_dac();
        }
        self.hidden_dac_arm = 0;
        match offset {
            port::MISC_OUTPUT => self.input_status_0(),
            port::SEQ_INDEX => self.seq_index,
            port::SEQ_DATA => self.read_sequencer(),
            port::DAC_READ_INDEX => {
                if self.dac_read_mode {
                    0x03
                } else {
                    0x00
                }
            }
            port::DAC_WRITE_INDEX => self.dac_write_index,
            port::DAC_DATA => self.read_dac_data(),
            port::MISC_OUTPUT_READ => self.misc_output,
            port::GR_INDEX => self.gr_index,
            port::GR_DATA => self.read_graphics(),
            port::AC_INDEX_DATA => self.ar_index,
            port::AC_DATA_READ => self.ar[(self.ar_index & 0x1F) as usize],
            port::CRTC_INDEX_MONO | port::CRTC_INDEX_COLOR => self.crtc_index,
            port::CRTC_DATA_MONO | port::CRTC_DATA_COLOR => self.read_crtc(),
            port::INPUT_STATUS_1_MONO | port::INPUT_STATUS_1_COLOR => {
                self.ar_next_is_data = false;
                self.input_status_1()
            }
            _ => crate::OPEN_BUS_BYTE,
        }
    }

    /// Write a VGA register.
    pub fn reg_write(&mut self, offset: u32, value: u8) {
        if offset == port::PEL_MASK {
            self.write_pel_mask_or_hidden_dac(value);
            return;
        }
        self.hidden_dac_arm = 0;
        match offset {
            port::MISC_OUTPUT => self.misc_output = value,
            port::SEQ_INDEX => self.seq_index = value,
            port::SEQ_DATA => self.write_sequencer(value),
            port::DAC_READ_INDEX => {
                self.dac_read_index = value;
                self.dac_component = 0;
                self.dac_read_mode = true;
            }
            port::DAC_WRITE_INDEX => {
                self.dac_write_index = value;
                self.dac_component = 0;
                self.dac_read_mode = false;
            }
            port::DAC_DATA => self.write_dac_data(value),
            port::GR_INDEX => self.gr_index = value,
            port::GR_DATA => self.write_graphics(value),
            port::AC_INDEX_DATA => self.write_attribute(value),
            port::CRTC_INDEX_MONO | port::CRTC_INDEX_COLOR => self.crtc_index = value,
            port::CRTC_DATA_MONO | port::CRTC_DATA_COLOR => self.write_crtc(value),
            _ => {}
        }
    }

    /// The mode the driver has programmed, or `None` while the card is
    /// not displaying anything the renderer can present.
    pub fn decoded_mode(&self) -> Option<DecodedMode> {
        // Held in reset: SR0 bits 0-1 are the asynchronous/synchronous
        // reset bits, both clear (not-reset) once the driver has
        // brought the chip up.
        if self.sr[0] & 0x03 != 0x03 {
            return None;
        }
        // SR1 bit 5: Screen Off.
        if self.sr[1] & 0x20 != 0 {
            return None;
        }
        // SR7 bit 0 gates the Cirrus extended (packed-pixel) path this
        // model exists for; without it the chip is in plain VGA
        // planar/text territory, which is out of scope (module docs).
        if self.sr[idx::SR_EXTENDED_MODE as usize] & 0x01 == 0 {
            return None;
        }

        let width = (u32::from(self.crtc[0x01]) + 1) * 8;
        let raw_height = u32::from(self.crtc[0x12])
            | (u32::from(self.crtc[0x07] & 0x02) << 7)
            | (u32::from(self.crtc[0x07] & 0x40) << 3);
        let doublescan = self.crtc[0x09] & 0x80 != 0;
        let height = (raw_height + 1) * if doublescan { 2 } else { 1 };
        if !(16..=4096).contains(&width) || !(16..=4096).contains(&height) {
            return None;
        }

        // CR13 is the offset register in units of 8 bytes; CR1B bit 4
        // is its extension bit 8, doubling the reachable stride.
        let pitch_units = u32::from(self.crtc[0x13]) | (u32::from(self.crtc[0x1B] & 0x10) << 4);
        let stride_bytes = pitch_units * 8;
        if stride_bytes == 0 {
            return None;
        }

        // CR0C/CR0D hold the low 16 bits of the start address, in
        // doubleword units; CR1B bits 0/2/3 extend it to bits 16-18 for
        // VRAM beyond 256K doublewords. CR08 bits 5-6 add sub-doubleword
        // byte panning on top.
        let start_units = u32::from(u16::from_be_bytes([self.crtc[0x0C], self.crtc[0x0D]]))
            | (u32::from(self.crtc[0x1B] & 0x01) << 16)
            | (u32::from(self.crtc[0x1B] & 0x04) << 15)
            | (u32::from(self.crtc[0x1B] & 0x08) << 15);
        let panning = u32::from((self.crtc[0x08] & 0x60) >> 5);
        let start_offset = start_units * 4 + panning;

        // Hidden DAC: bit 7 clear is plain 8bpp palette mode; bit 7 set
        // with bit 6 clear is 15bpp; with both set, the low nibble picks
        // the format, with 0x0F meaning "consult SR7's extended mode
        // bits instead" rather than naming a fixed depth.
        let depth = if self.hidden_dac & 0x80 == 0 {
            PixelDepth::Bpp8
        } else if self.hidden_dac & 0x40 == 0 {
            PixelDepth::Bpp15
        } else {
            match self.hidden_dac & 0x0F {
                0x00 => PixelDepth::Bpp15,
                0x01 => PixelDepth::Bpp16,
                0x05 => PixelDepth::Bpp24,
                0x0F => match self.sr[idx::SR_EXTENDED_MODE as usize] & 0x0E {
                    0x04 => PixelDepth::Bpp24,
                    0x02 | 0x06 => PixelDepth::Bpp16,
                    _ => PixelDepth::Bpp8,
                },
                // No RTG driver programs anything else; guest-controlled
                // input, so an unrecognised code is "not presentable"
                // rather than a guess.
                _ => return None,
            }
        };

        // Reject a programming that doesn't actually fit: a stride
        // narrower than one displayed row, or a scanout that would run
        // past the end of VRAM. The guest controls every register here.
        let row_bytes = width.checked_mul(depth.bytes_per_pixel())?;
        if stride_bytes < row_bytes {
            return None;
        }
        let last_row_start = start_offset.checked_add((height - 1).checked_mul(stride_bytes)?)?;
        let end = last_row_start.checked_add(row_bytes)?;
        if end > self.vram.len() as u32 {
            return None;
        }

        Some(DecodedMode {
            width,
            height,
            depth,
            stride_bytes,
            start_offset,
        })
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

    // ---- sequencer -----------------------------------------------------

    fn extensions_unlocked(&self) -> bool {
        self.sr[idx::SR_LOCK as usize] == idx::SR_LOCK_UNLOCKED
    }

    fn read_sequencer(&self) -> u8 {
        let index = self.seq_index & 0x1F;
        if index == idx::SR_LOCK {
            return self.sr[idx::SR_LOCK as usize];
        }
        if index > idx::SR_LOCK && !self.extensions_unlocked() {
            return crate::OPEN_BUS_BYTE;
        }
        self.sr[index as usize]
    }

    fn write_sequencer(&mut self, value: u8) {
        let index = self.seq_index & 0x1F;
        if index == idx::SR_LOCK {
            self.sr[idx::SR_LOCK as usize] =
                if value & idx::SR_LOCK_UNLOCK_MASK == idx::SR_LOCK_UNLOCKED {
                    idx::SR_LOCK_UNLOCKED
                } else {
                    idx::SR_LOCK_LOCKED
                };
            return;
        }
        if index > idx::SR_LOCK && !self.extensions_unlocked() {
            return;
        }
        self.sr[index as usize] = value;
    }

    // ---- graphics controller --------------------------------------------

    fn read_graphics(&self) -> u8 {
        let index = self.gr_index & 0x3F;
        if index > 8 && !self.extensions_unlocked() {
            return crate::OPEN_BUS_BYTE;
        }
        self.gr[index as usize]
    }

    fn write_graphics(&mut self, value: u8) {
        let index = self.gr_index & 0x3F;
        if index > 8 && !self.extensions_unlocked() {
            return;
        }
        self.gr[index as usize] = value;
    }

    // ---- CRTC ------------------------------------------------------------

    fn read_crtc(&self) -> u8 {
        let index = self.crtc_index & 0x3F;
        if index == idx::CRTC_ID {
            return self.revision.part_id();
        }
        if index > idx::CRTC_LAST_STANDARD && !self.extensions_unlocked() {
            return crate::OPEN_BUS_BYTE;
        }
        self.crtc[index as usize]
    }

    fn write_crtc(&mut self, value: u8) {
        let index = self.crtc_index & 0x3F;
        // The chip-ID register is read-only silicon identity, never a
        // guest-writable byte.
        if index == idx::CRTC_ID {
            return;
        }
        if index > idx::CRTC_LAST_STANDARD && !self.extensions_unlocked() {
            return;
        }
        // CR11 bit 7 write-protects CR00-CR07, except that CR07 bit 4
        // (the vertical-interrupt-clear bit some drivers toggle) always
        // gets through — standard VGA register protection.
        if self.crtc[0x11] & 0x80 != 0 && index <= 7 {
            if index == 7 {
                self.crtc[7] = (self.crtc[7] & !0x10) | (value & 0x10);
            }
            return;
        }
        self.crtc[index as usize] = value;
    }

    // ---- attribute controller --------------------------------------------

    fn write_attribute(&mut self, value: u8) {
        if self.ar_next_is_data {
            self.ar[(self.ar_index & 0x1F) as usize] = value;
        } else {
            self.ar_index = value & 0x3F;
        }
        self.ar_next_is_data = !self.ar_next_is_data;
    }

    // ---- RAMDAC ------------------------------------------------------------

    /// Real hardware has no readable input-status-0 bits this model
    /// needs (dot-clock/vsync sensing); a fixed value is indistinguishable
    /// to a driver that isn't polling real timing, which none of the
    /// scope here does.
    fn input_status_0(&self) -> u8 {
        0
    }

    fn input_status_1(&self) -> u8 {
        0
    }

    fn read_pel_mask_or_hidden_dac(&mut self) -> u8 {
        if !self.extensions_unlocked() {
            self.hidden_dac_arm = 0;
            return self.pel_mask;
        }
        if self.hidden_dac_arm >= 4 {
            self.hidden_dac_arm = 0;
            self.hidden_dac
        } else {
            self.hidden_dac_arm += 1;
            self.pel_mask
        }
    }

    fn write_pel_mask_or_hidden_dac(&mut self, value: u8) {
        if self.extensions_unlocked() && self.hidden_dac_arm >= 4 {
            self.hidden_dac = value;
        } else {
            self.pel_mask = value;
        }
        self.hidden_dac_arm = 0;
    }

    fn write_dac_data(&mut self, value: u8) {
        let index = self.dac_write_index as usize;
        let component = self.dac_component as usize;
        self.palette[index][component] = value & 0x3F;
        self.dac_component += 1;
        if self.dac_component == 3 {
            self.dac_component = 0;
            self.dac_write_index = self.dac_write_index.wrapping_add(1);
        }
    }

    fn read_dac_data(&mut self) -> u8 {
        let index = self.dac_read_index as usize;
        let component = self.dac_component as usize;
        let value = self.palette[index][component];
        self.dac_component += 1;
        if self.dac_component == 3 {
            self.dac_component = 0;
            self.dac_read_index = self.dac_read_index.wrapping_add(1);
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chip(vram: &mut [u8]) -> Cirrus542x<'_> {
        Cirrus542x::new(ChipRevision::Gd5426, vram)
    }

    fn unlock(c: &mut Cirrus542x) {
        c.reg_write(port::SEQ_INDEX, idx::SR_LOCK);
        c.reg_write(port::SEQ_DATA, idx::SR_LOCK_UNLOCKED);
    }

    // ---- chip identity ----------------------------------------------

    #[test]
    fn revisions_report_their_own_part_id() {
        assert_eq!(ChipRevision::Gd5426.part_id(), 0x90);
        assert_eq!(ChipRevision::Gd5428.part_id(), 0x98);
    }

    #[test]
    fn vram_roundtrips_and_clamps() {
        let mut vram = [0u8; 64];
        let mut card = chip(&mut vram);
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
        let mut card = chip(&mut vram);
        // Program it through the real DAC-data sequence rather than
        // poking the field directly.
        card.reg_write(port::DAC_WRITE_INDEX, 1);
        card.reg_write(port::DAC_DATA, 0x3F);
        card.reg_write(port::DAC_DATA, 0x3F);
        card.reg_write(port::DAC_DATA, 0x3F);
        assert_eq!(card.palette_argb(1), 0xFFFF_FFFF);
        assert_eq!(card.palette_argb(0), 0xFF00_0000);
    }

    // ---- sequencer ---------------------------------------------------

    #[test]
    fn sequencer_round_trips_and_gates_extended_registers_on_the_lock() {
        let mut vram = [0u8; 16];
        let mut c = chip(&mut vram);

        // SR0 (standard, always writable) round-trips freely.
        c.reg_write(port::SEQ_INDEX, 0x00);
        c.reg_write(port::SEQ_DATA, 0x03);
        assert_eq!(c.reg_read(port::SEQ_DATA), 0x03);

        // SR0B is an extended register: locked out until the unlock
        // sequence lands, both for reads and writes.
        c.reg_write(port::SEQ_INDEX, 0x0B);
        c.reg_write(port::SEQ_DATA, 0x4A);
        assert_eq!(
            c.reg_read(port::SEQ_DATA),
            crate::OPEN_BUS_BYTE,
            "extended register floats while locked"
        );

        unlock(&mut c);
        c.reg_write(port::SEQ_INDEX, 0x0B);
        c.reg_write(port::SEQ_DATA, 0x4A);
        assert_eq!(c.reg_read(port::SEQ_DATA), 0x4A, "now it takes and holds");

        // Writing anything else back to SR6 re-locks the extensions.
        c.reg_write(port::SEQ_INDEX, idx::SR_LOCK);
        c.reg_write(port::SEQ_DATA, 0x00);
        c.reg_write(port::SEQ_INDEX, 0x0B);
        assert_eq!(c.reg_read(port::SEQ_DATA), crate::OPEN_BUS_BYTE);
    }

    // ---- CRTC ----------------------------------------------------------

    #[test]
    fn crtc_round_trips_protects_cr0_7_and_reports_the_part_id() {
        let mut vram = [0u8; 16];
        let mut c = chip(&mut vram);

        c.reg_write(port::CRTC_INDEX_COLOR, 0x01);
        c.reg_write(port::CRTC_DATA_COLOR, 0x4F);
        assert_eq!(c.reg_read(port::CRTC_DATA_COLOR), 0x4F);
        // The mono-adapter alias reaches the very same register.
        c.reg_write(port::CRTC_INDEX_MONO, 0x01);
        assert_eq!(c.reg_read(port::CRTC_DATA_MONO), 0x4F);

        // Lock CR00-07 via CR11 bit 7, then show CR01 no longer takes...
        c.reg_write(port::CRTC_INDEX_COLOR, 0x11);
        c.reg_write(port::CRTC_DATA_COLOR, 0x80);
        c.reg_write(port::CRTC_INDEX_COLOR, 0x01);
        c.reg_write(port::CRTC_DATA_COLOR, 0x00);
        assert_eq!(c.reg_read(port::CRTC_DATA_COLOR), 0x4F, "still protected");
        // ...while CR07 bit 4 alone still gets through.
        c.reg_write(port::CRTC_INDEX_COLOR, 0x07);
        c.reg_write(port::CRTC_DATA_COLOR, 0x10);
        assert_eq!(c.reg_read(port::CRTC_DATA_COLOR), 0x10);

        // CR27 is read-only chip identity, not a register.
        c.reg_write(port::CRTC_INDEX_COLOR, 0x27);
        assert_eq!(c.reg_read(port::CRTC_DATA_COLOR), 0x90, "GD5426 part id");
        c.reg_write(port::CRTC_DATA_COLOR, 0xAA);
        assert_eq!(c.reg_read(port::CRTC_DATA_COLOR), 0x90, "still the part id");
    }

    #[test]
    fn part_id_follows_the_revision() {
        let mut vram = [0u8; 16];
        let mut c = Cirrus542x::new(ChipRevision::Gd5428, &mut vram);
        c.reg_write(port::CRTC_INDEX_COLOR, 0x27);
        assert_eq!(c.reg_read(port::CRTC_DATA_COLOR), 0x98);
    }

    // ---- graphics controller --------------------------------------------

    #[test]
    fn graphics_controller_round_trips_and_gates_extensions() {
        let mut vram = [0u8; 16];
        let mut c = chip(&mut vram);

        c.reg_write(port::GR_INDEX, 0x00);
        c.reg_write(port::GR_DATA, 0x0F);
        assert_eq!(c.reg_read(port::GR_DATA), 0x0F);

        c.reg_write(port::GR_INDEX, 0x09); // extended (banking)
        c.reg_write(port::GR_DATA, 0x02);
        assert_eq!(c.reg_read(port::GR_DATA), crate::OPEN_BUS_BYTE);

        unlock(&mut c);
        c.reg_write(port::GR_INDEX, 0x09);
        c.reg_write(port::GR_DATA, 0x02);
        assert_eq!(c.reg_read(port::GR_DATA), 0x02);
    }

    // ---- attribute controller: the flip-flop ----------------------------

    #[test]
    fn attribute_controller_flip_flop_and_its_reset_on_status_read() {
        let mut vram = [0u8; 16];
        let mut c = chip(&mut vram);

        // First write loads the index, second writes data.
        c.reg_write(port::AC_INDEX_DATA, 0x05);
        c.reg_write(port::AC_INDEX_DATA, 0x77);
        assert_eq!(c.reg_read(port::AC_DATA_READ), 0x77);

        // Mid-sequence: an index write lands, but before the matching
        // data write, a status-register read resets the flip-flop to
        // index phase, so the *next* write is treated as an index too.
        c.reg_write(port::AC_INDEX_DATA, 0x06); // index phase -> data phase
        let _ = c.reg_read(port::INPUT_STATUS_1_COLOR); // resets to index phase
        c.reg_write(port::AC_INDEX_DATA, 0x08); // treated as an index, not data for AR06
        c.reg_write(port::AC_INDEX_DATA, 0x99); // now data, for AR08
        assert_eq!(c.reg_read(port::AC_DATA_READ), 0x99);
        // AR06 was never actually written by the interrupted sequence.
        c.reg_write(port::AC_INDEX_DATA, 0x06);
        assert_eq!(c.reg_read(port::AC_DATA_READ), 0x00);
    }

    // ---- palette ---------------------------------------------------------

    #[test]
    fn palette_write_sequence_is_three_writes_per_entry_auto_incrementing() {
        let mut vram = [0u8; 16];
        let mut c = chip(&mut vram);

        c.reg_write(port::DAC_WRITE_INDEX, 10);
        c.reg_write(port::DAC_DATA, 0x10); // R of entry 10
        c.reg_write(port::DAC_DATA, 0x20); // G of entry 10
        c.reg_write(port::DAC_DATA, 0x30); // B of entry 10 -> index auto-increments to 11
        c.reg_write(port::DAC_DATA, 0x01); // R of entry 11
        c.reg_write(port::DAC_DATA, 0x02);
        c.reg_write(port::DAC_DATA, 0x03);

        c.reg_write(port::DAC_READ_INDEX, 10);
        assert_eq!(c.reg_read(port::DAC_DATA), 0x10);
        assert_eq!(c.reg_read(port::DAC_DATA), 0x20);
        assert_eq!(c.reg_read(port::DAC_DATA), 0x30);
        assert_eq!(c.reg_read(port::DAC_DATA), 0x01, "auto-incremented to 11");

        let expand = |six: u32| (six << 2) | (six >> 4);
        let expected = 0xFF00_0000 | (expand(0x10) << 16) | (expand(0x20) << 8) | expand(0x30);
        assert_eq!(c.palette_argb(10), expected);
    }

    // ---- hidden DAC --------------------------------------------------------

    #[test]
    fn hidden_dac_needs_the_unlock_and_the_four_pel_mask_read_arming_sequence() {
        let mut vram = [0u8; 16];
        let mut c = chip(&mut vram);

        // Locked: reads just see the pixel mask, never the hidden
        // register, no matter how many times it's read.
        c.reg_write(port::PEL_MASK, 0xAB);
        for _ in 0..6 {
            assert_eq!(c.reg_read(port::PEL_MASK), 0xAB);
        }

        unlock(&mut c);
        c.reg_write(port::PEL_MASK, 0xAB); // restore the pixel mask post-unlock write
        for _ in 0..4 {
            assert_eq!(
                c.reg_read(port::PEL_MASK),
                0xAB,
                "still pixel mask before the 5th read"
            );
        }
        c.reg_write(port::PEL_MASK, 0xC1); // the 5th access hits the hidden DAC
                                           // Re-arm and read it back.
        for _ in 0..4 {
            let _ = c.reg_read(port::PEL_MASK);
        }
        assert_eq!(c.reg_read(port::PEL_MASK), 0xC1);

        // An access to an unrelated port disarms a partial sequence.
        for _ in 0..2 {
            let _ = c.reg_read(port::PEL_MASK);
        }
        let _ = c.reg_read(port::MISC_OUTPUT_READ);
        for _ in 0..4 {
            assert_eq!(c.reg_read(port::PEL_MASK), 0xAB);
        }
        assert_eq!(c.reg_read(port::PEL_MASK), 0xC1);
    }

    // ---- decoded_mode ------------------------------------------------------

    /// Bring a chip up out of reset with the packed-pixel path enabled
    /// and geometry set for `width`x`height`, mirroring what an RTG
    /// driver's mode-set actually programs.
    fn programmed(
        c: &mut Cirrus542x,
        width: u32,
        height: u32,
        stride_bytes: u32,
        start_offset: u32,
    ) {
        unlock(c);
        c.reg_write(port::SEQ_INDEX, 0x00);
        c.reg_write(port::SEQ_DATA, 0x03); // out of reset
        c.reg_write(port::SEQ_INDEX, 0x01);
        c.reg_write(port::SEQ_DATA, 0x00); // screen on
        c.reg_write(port::SEQ_INDEX, idx::SR_EXTENDED_MODE);
        c.reg_write(port::SEQ_DATA, 0x01); // extended path enabled, 8bpp select bits clear

        c.reg_write(port::CRTC_INDEX_COLOR, 0x01);
        c.reg_write(port::CRTC_DATA_COLOR, (width / 8 - 1) as u8);

        let vde = height - 1;
        c.reg_write(port::CRTC_INDEX_COLOR, 0x12);
        c.reg_write(port::CRTC_DATA_COLOR, vde as u8);
        let mut cr07 = 0u8;
        if vde & 0x100 != 0 {
            cr07 |= 0x02;
        }
        if vde & 0x200 != 0 {
            cr07 |= 0x40;
        }
        c.reg_write(port::CRTC_INDEX_COLOR, 0x07);
        c.reg_write(port::CRTC_DATA_COLOR, cr07);

        let pitch_units = stride_bytes / 8;
        c.reg_write(port::CRTC_INDEX_COLOR, 0x13);
        c.reg_write(port::CRTC_DATA_COLOR, pitch_units as u8);
        let mut cr1b = if pitch_units & 0x100 != 0 { 0x10 } else { 0 };

        let start_units = start_offset / 4;
        c.reg_write(port::CRTC_INDEX_COLOR, 0x0C);
        c.reg_write(port::CRTC_DATA_COLOR, (start_units >> 8) as u8);
        c.reg_write(port::CRTC_INDEX_COLOR, 0x0D);
        c.reg_write(port::CRTC_DATA_COLOR, start_units as u8);
        if start_units & 0x1_0000 != 0 {
            cr1b |= 0x01;
        }
        if start_units & 0x2_0000 != 0 {
            cr1b |= 0x04;
        }
        if start_units & 0x4_0000 != 0 {
            cr1b |= 0x08;
        }
        c.reg_write(port::CRTC_INDEX_COLOR, 0x1B);
        c.reg_write(port::CRTC_DATA_COLOR, cr1b);
    }

    #[test]
    fn decoded_mode_640x480x8() {
        let mut vram = [0u8; 640 * 480 + 0x1_0000];
        let mut c = chip(&mut vram);
        // 640x480 clut8: CR01=(640/8)-1=79=0x4F, CR12=(480-1)&0xff=0xDF,
        // no overflow bits needed since 479 < 256... wait 479 > 255, so
        // bit 8 (0x02 in CR07) is set: 479 = 0x1DF, so CR12=0xDF, CR07
        // bit0x02 set. Handled by `programmed`'s own overflow math.
        programmed(&mut c, 640, 480, 640, 0);
        // Hidden DAC left at its power-on 0 -> plain 8bpp palette mode.
        let mode = c.decoded_mode().expect("a programmed 640x480x8 mode");
        assert_eq!(mode.width, 640);
        assert_eq!(mode.height, 480);
        assert_eq!(mode.depth, PixelDepth::Bpp8);
        assert_eq!(mode.stride_bytes, 640);
        assert_eq!(mode.start_offset, 0);
    }

    #[test]
    fn decoded_mode_800x600x16_with_panning() {
        // A hi-colour mode with a start offset that isn't a whole number
        // of displayed rows, the way `SetPanning` can leave it.
        let mut vram = vec![0u8; 800 * 600 * 2 + 8192];
        let mut c = chip(&mut vram);
        let stride = 800 * 2; // 1600 bytes/row at 16bpp
        let start = 4096u32; // an arbitrary panning offset, dword-aligned
        programmed(&mut c, 800, 600, stride as u32, start);
        c.reg_write(port::SEQ_INDEX, idx::SR_EXTENDED_MODE);
        c.reg_write(port::SEQ_DATA, 0x01); // extended path stays enabled
                                           // Hidden DAC 0xC1: bit7 set, bit6 set, low nibble 1 -> 16bpp.
        for _ in 0..4 {
            let _ = c.reg_read(port::PEL_MASK);
        }
        c.reg_write(port::PEL_MASK, 0xC1);

        let mode = c.decoded_mode().expect("a programmed 800x600x16 mode");
        assert_eq!(mode.width, 800);
        assert_eq!(mode.height, 600);
        assert_eq!(mode.depth, PixelDepth::Bpp16);
        assert_eq!(mode.stride_bytes, stride as u32);
        assert_eq!(mode.start_offset, start);
    }

    #[test]
    fn decoded_mode_is_none_out_of_reset() {
        let mut vram = [0u8; 4096];
        let c = chip(&mut vram);
        assert_eq!(
            c.decoded_mode(),
            None,
            "SR0's reset bits are clear at power-on"
        );
    }

    #[test]
    fn decoded_mode_is_none_while_extensions_are_locked_or_screen_is_off() {
        let mut vram = vec![0u8; 640 * 480 + 0x1_0000];
        let mut c = chip(&mut vram);
        programmed(&mut c, 640, 480, 640, 0);

        // Screen Off.
        c.reg_write(port::SEQ_INDEX, 0x01);
        c.reg_write(port::SEQ_DATA, 0x20);
        assert_eq!(c.decoded_mode(), None);
        c.reg_write(port::SEQ_DATA, 0x00);
        assert!(c.decoded_mode().is_some(), "sanity: back on");

        // Extended path disabled.
        c.reg_write(port::SEQ_INDEX, idx::SR_EXTENDED_MODE);
        c.reg_write(port::SEQ_DATA, 0x00);
        assert_eq!(c.decoded_mode(), None);
    }

    #[test]
    fn decoded_mode_does_not_panic_on_nonsense_register_state() {
        // The guest can program any byte into any register; a scanout
        // that would run off the end of a small VRAM must come back
        // `None`, never panic.
        let mut vram = [0u8; 64];
        let mut c = chip(&mut vram);
        unlock(&mut c);
        c.reg_write(port::SEQ_INDEX, 0x00);
        c.reg_write(port::SEQ_DATA, 0x03);
        c.reg_write(port::SEQ_INDEX, idx::SR_EXTENDED_MODE);
        c.reg_write(port::SEQ_DATA, 0x01);
        for (index, value) in [
            (0x01u8, 0xFFu8),
            (0x12, 0xFF),
            (0x07, 0xFF),
            (0x13, 0xFF),
            (0x1B, 0xFF),
            (0x0C, 0xFF),
            (0x0D, 0xFF),
            (0x08, 0xFF),
        ] {
            c.reg_write(port::CRTC_INDEX_COLOR, index);
            c.reg_write(port::CRTC_DATA_COLOR, value);
        }
        for _ in 0..4 {
            let _ = c.reg_read(port::PEL_MASK);
        }
        c.reg_write(port::PEL_MASK, 0xFF); // hidden DAC nibble 0xF w/ SR7 giving Bpp8
        assert_eq!(c.decoded_mode(), None, "runs off a 64-byte VRAM");
    }
}
