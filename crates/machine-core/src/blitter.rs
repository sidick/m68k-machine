//! Software blitter (proposal §7.3).
//!
//! graphics.library uses the blitter for *any* planar bitmap — `Text()`,
//! offscreen `BltBitMap`, pointer imagery — even with Workbench on RTG,
//! and P96 only intercepts RTG-bitmap paths. DraCo patched
//! graphics.library to avoid this; this machine does not patch, so the
//! blitter has to exist and has to be right.
//!
//! It is **synchronous**: the whole operation runs to completion inside
//! the `BLTSIZE`/`BLTSIZV` write that starts it. There is no timing
//! model and no DMA contention — `BBUSY` is only ever set while the host
//! is executing the blit, which from the guest's point of view means it
//! reads back clear, and any `BBUSY` spin the OS does simply falls
//! through. `BZERO` and the blitter-finished interrupt are real, because
//! the OS acts on both.
//!
//! Deliberately absent: cycle timing, DMA priority, and any notion of
//! the blitter running concurrently with the CPU. Those matter to demos,
//! which are out of scope by §2.

use crate::CHIP_RAM_SIZE;

/// Register offsets within `$DFF000` that belong to the blitter.
pub mod reg {
    /// Blitter destination data, readable.
    pub const BLTDDAT: u16 = 0x000;
    pub const BLTCON0: u16 = 0x040;
    pub const BLTCON1: u16 = 0x042;
    pub const BLTAFWM: u16 = 0x044;
    pub const BLTALWM: u16 = 0x046;
    pub const BLTCPTH: u16 = 0x048;
    pub const BLTCPTL: u16 = 0x04A;
    pub const BLTBPTH: u16 = 0x04C;
    pub const BLTBPTL: u16 = 0x04E;
    pub const BLTAPTH: u16 = 0x050;
    pub const BLTAPTL: u16 = 0x052;
    pub const BLTDPTH: u16 = 0x054;
    pub const BLTDPTL: u16 = 0x056;
    pub const BLTSIZE: u16 = 0x058;
    pub const BLTCON0L: u16 = 0x05A;
    pub const BLTSIZV: u16 = 0x05C;
    pub const BLTSIZH: u16 = 0x05E;
    pub const BLTCMOD: u16 = 0x060;
    pub const BLTBMOD: u16 = 0x062;
    pub const BLTAMOD: u16 = 0x064;
    pub const BLTDMOD: u16 = 0x066;
    pub const BLTCDAT: u16 = 0x070;
    pub const BLTBDAT: u16 = 0x072;
    pub const BLTADAT: u16 = 0x074;

    /// True for any offset the blitter owns, so the bus can route it
    /// here instead of to the general chipset register file.
    pub fn is_blitter(offset: u16) -> bool {
        matches!(offset, BLTDDAT | BLTCON0..=BLTDMOD | BLTCDAT..=BLTADAT)
    }
}

/// Source channel indices, in the order the hardware numbers them.
pub const CHAN_A: usize = 0;
pub const CHAN_B: usize = 1;
pub const CHAN_C: usize = 2;
pub const CHAN_D: usize = 3;

/// `DMACONR` bit 14: blitter busy. Only ever set while the host is
/// mid-blit, so the guest always observes it clear (see module docs).
pub const DMACONR_BBUSY: u16 = 1 << 14;
/// `DMACONR` bit 13: set when the last blit produced all-zero output.
pub const DMACONR_BZERO: u16 = 1 << 13;

/// `BLTCON0` bit 11: use channel A. Bits 8-11 are the channel enables.
pub const BLTCON0_USEA: u16 = 1 << 11;
pub const BLTCON0_USEB: u16 = 1 << 10;
pub const BLTCON0_USEC: u16 = 1 << 9;
pub const BLTCON0_USED: u16 = 1 << 8;

/// `BLTCON1` bit 0: line mode rather than area mode.
pub const BLTCON1_LINE: u16 = 1 << 0;
/// `BLTCON1` bit 1: descending (decrement pointers instead of increment).
pub const BLTCON1_DESC: u16 = 1 << 1;
/// `BLTCON1` bit 3: inclusive fill.
pub const BLTCON1_IFE: u16 = 1 << 3;
/// `BLTCON1` bit 2: exclusive fill.
pub const BLTCON1_EFE: u16 = 1 << 2;
/// `BLTCON1` bit 4: fill carry input.
pub const BLTCON1_FCI: u16 = 1 << 4;

/// The blitter's register file and status.
#[derive(Default)]
pub struct Blitter {
    pub bltcon0: u16,
    pub bltcon1: u16,
    /// First- and last-word masks applied to channel A.
    pub bltafwm: u16,
    pub bltalwm: u16,
    /// Channel pointers, indexed by `CHAN_*`.
    pub pt: [u32; 4],
    /// Per-channel modulo, added to the pointer at the end of each row.
    /// Signed: a negative modulo is ordinary and the OS uses it.
    pub modulo: [i16; 4],
    /// Per-channel data registers.
    pub dat: [u16; 4],
    /// Destination data, read back through `BLTDDAT`.
    pub bltddat: u16,

    /// Blit dimensions, from `BLTSIZE` or the `BLTSIZV`/`BLTSIZH` pair.
    pub width_words: u16,
    pub height_rows: u16,

    /// Set when the last completed blit wrote only zeroes.
    pub zero: bool,
    /// A blit has been armed by a size write and is waiting for the bus
    /// to run it (the bus owns chip RAM; see [`Blitter::execute`]).
    pending: bool,
}

impl Blitter {
    pub fn new() -> Self {
        Self {
            // BZERO reads set before any blit has run, matching a
            // just-reset blitter whose (nonexistent) last result was
            // trivially all-zero.
            zero: true,
            ..Default::default()
        }
    }

    /// Status bits this blitter contributes to a `DMACONR` read.
    pub fn dmaconr_bits(&self) -> u16 {
        if self.zero {
            DMACONR_BZERO
        } else {
            0
        }
    }

    /// Read a blitter register. Only `BLTDDAT` is readable; every other
    /// blitter register is write-only and reads open bus like the rest
    /// of the write-only chipset registers.
    pub fn read(&mut self, offset: u16) -> u16 {
        match offset {
            reg::BLTDDAT => self.bltddat,
            _ => 0xFFFF,
        }
    }

    /// Write a blitter register.
    ///
    /// Returns `true` when the write armed a blit, which is the bus's
    /// cue to call [`Blitter::execute`] — the blitter needs chip RAM and
    /// this struct deliberately does not own it.
    pub fn write(&mut self, offset: u16, value: u16) -> bool {
        match offset {
            reg::BLTCON0 => self.bltcon0 = value,
            reg::BLTCON1 => self.bltcon1 = value,
            // BLTCON0L writes only the low byte of BLTCON0, leaving the
            // minterm and channel-enable bits alone (ECS addition).
            reg::BLTCON0L => self.bltcon0 = (self.bltcon0 & 0xFF00) | (value & 0x00FF),
            reg::BLTAFWM => self.bltafwm = value,
            reg::BLTALWM => self.bltalwm = value,

            reg::BLTAPTH => set_ptr_hi(&mut self.pt[CHAN_A], value),
            reg::BLTAPTL => set_ptr_lo(&mut self.pt[CHAN_A], value),
            reg::BLTBPTH => set_ptr_hi(&mut self.pt[CHAN_B], value),
            reg::BLTBPTL => set_ptr_lo(&mut self.pt[CHAN_B], value),
            reg::BLTCPTH => set_ptr_hi(&mut self.pt[CHAN_C], value),
            reg::BLTCPTL => set_ptr_lo(&mut self.pt[CHAN_C], value),
            reg::BLTDPTH => set_ptr_hi(&mut self.pt[CHAN_D], value),
            reg::BLTDPTL => set_ptr_lo(&mut self.pt[CHAN_D], value),

            reg::BLTAMOD => self.modulo[CHAN_A] = value as i16,
            reg::BLTBMOD => self.modulo[CHAN_B] = value as i16,
            reg::BLTCMOD => self.modulo[CHAN_C] = value as i16,
            reg::BLTDMOD => self.modulo[CHAN_D] = value as i16,

            reg::BLTADAT => self.dat[CHAN_A] = value,
            reg::BLTBDAT => self.dat[CHAN_B] = value,
            reg::BLTCDAT => self.dat[CHAN_C] = value,

            // Writing a size register starts the blit. BLTSIZE packs
            // height in bits 6-15 and width in bits 0-5, with zero in
            // either field meaning the maximum (1024 rows / 64 words) —
            // a hardware quirk the OS relies on.
            reg::BLTSIZE => {
                let h = (value >> 6) & 0x03FF;
                let w = value & 0x003F;
                self.height_rows = if h == 0 { 1024 } else { h };
                self.width_words = if w == 0 { 64 } else { w };
                self.pending = true;
            }
            // ECS split-size registers: BLTSIZV sets height and BLTSIZH
            // sets width *and* starts the blit.
            reg::BLTSIZV => self.height_rows = value & 0x7FFF,
            reg::BLTSIZH => {
                self.width_words = value & 0x07FF;
                self.pending = true;
            }
            _ => {}
        }
        self.pending
    }

    /// Whether a blit is armed and waiting to run.
    pub fn pending(&self) -> bool {
        self.pending
    }

    /// Run the armed blit against chip RAM, updating `zero` and the
    /// channel pointers as the hardware leaves them.
    ///
    /// Returns `true` if a blit actually ran, which the bus turns into
    /// the blitter-finished interrupt (`INTREQ` bit 6).
    pub fn execute(&mut self, _ram: &mut [u8; CHIP_RAM_SIZE]) -> bool {
        if !self.pending {
            return false;
        }
        self.pending = false;

        // Implemented in Phase 2: area mode (all 256 minterms, the four
        // channels, A/B barrel shifts, first/last-word masks, descending
        // direction, inclusive and exclusive fill) and line mode. Must
        // leave `zero` set only when every written word was zero, and
        // leave the channel pointers where the hardware leaves them,
        // since graphics.library reads them back.
        true
    }
}

fn set_ptr_hi(ptr: &mut u32, value: u16) {
    *ptr = (*ptr & 0x0000_FFFF) | ((value as u32) << 16);
}

fn set_ptr_lo(ptr: &mut u32, value: u16) {
    // Chip RAM pointers are word-aligned; the hardware ignores bit 0.
    *ptr = (*ptr & 0xFFFF_0000) | ((value & 0xFFFE) as u32);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bltsize_zero_fields_mean_maximum() {
        let mut b = Blitter::new();
        b.write(reg::BLTSIZE, 0);
        assert_eq!(b.height_rows, 1024);
        assert_eq!(b.width_words, 64);
    }

    #[test]
    fn bltsize_packs_height_and_width() {
        let mut b = Blitter::new();
        b.write(reg::BLTSIZE, (3 << 6) | 5);
        assert_eq!(b.height_rows, 3);
        assert_eq!(b.width_words, 5);
    }

    #[test]
    fn size_write_arms_a_blit() {
        let mut b = Blitter::new();
        assert!(!b.pending());
        assert!(b.write(reg::BLTSIZE, (1 << 6) | 1));
        assert!(b.pending());
    }

    #[test]
    fn bltsizv_alone_does_not_start_a_blit() {
        let mut b = Blitter::new();
        assert!(!b.write(reg::BLTSIZV, 4));
        assert!(!b.pending(), "BLTSIZH is what starts an ECS-sized blit");
        assert!(b.write(reg::BLTSIZH, 2));
        assert!(b.pending());
    }

    #[test]
    fn pointers_ignore_bit_zero() {
        let mut b = Blitter::new();
        b.write(reg::BLTAPTH, 0x0001);
        b.write(reg::BLTAPTL, 0x1235);
        assert_eq!(b.pt[CHAN_A], 0x0001_1234);
    }

    #[test]
    fn bltcon0l_preserves_the_minterm_byte() {
        let mut b = Blitter::new();
        b.write(reg::BLTCON0, 0xABCD);
        b.write(reg::BLTCON0L, 0x0012);
        assert_eq!(b.bltcon0, 0xAB12);
    }

    #[test]
    fn write_only_registers_read_open_bus() {
        let mut b = Blitter::new();
        assert_eq!(b.read(reg::BLTCON0), 0xFFFF);
    }
}
