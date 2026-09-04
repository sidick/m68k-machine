//! Minimal custom-chip register file (`$DFF000`), proposal §7.1.
//!
//! Only the registers the OS spins on or reads for identification are
//! implemented; everything else falls through to open bus. There is no
//! timing model and no DMA: this is a register file plus a free-running
//! beam counter driven by the frame clock.
//!
//! Phase 1 scope is the "blind boot" subset — enough for exec to start,
//! interrupts to work, and graphics.library to identify the chipset.
//! The software blitter (§7.3) and the stop-gap renderer (§8.1) are
//! Phase 2 and are deliberately absent here.

/// Register offsets within `$DFF000`, as the hardware numbers them.
pub mod reg {
    pub const BLTDDAT: u16 = 0x000;
    pub const DMACONR: u16 = 0x002;
    pub const VPOSR: u16 = 0x004;
    pub const VHPOSR: u16 = 0x006;
    pub const JOY0DAT: u16 = 0x00A;
    pub const JOY1DAT: u16 = 0x00C;
    pub const ADKCONR: u16 = 0x010;
    pub const POTGOR: u16 = 0x016;
    pub const SERDATR: u16 = 0x018;
    pub const DSKBYTR: u16 = 0x01A;
    pub const INTENAR: u16 = 0x01C;
    pub const INTREQR: u16 = 0x01E;
    pub const DSKPTH: u16 = 0x020;
    pub const DSKPTL: u16 = 0x022;
    pub const DSKLEN: u16 = 0x024;
    pub const VPOSW: u16 = 0x02A;
    pub const VHPOSW: u16 = 0x02C;
    pub const SERDAT: u16 = 0x030;
    pub const SERPER: u16 = 0x032;
    pub const POTGO: u16 = 0x034;
    pub const COP1LCH: u16 = 0x080;
    pub const COP1LCL: u16 = 0x082;
    pub const COP2LCH: u16 = 0x084;
    pub const COP2LCL: u16 = 0x086;
    pub const COPJMP1: u16 = 0x088;
    pub const COPJMP2: u16 = 0x08A;
    pub const DIWSTRT: u16 = 0x08E;
    pub const DIWSTOP: u16 = 0x090;
    pub const DDFSTRT: u16 = 0x092;
    pub const DDFSTOP: u16 = 0x094;
    pub const DMACON: u16 = 0x096;
    pub const INTENA: u16 = 0x09A;
    pub const INTREQ: u16 = 0x09C;
    pub const ADKCON: u16 = 0x09E;
    pub const BPL1PTH: u16 = 0x0E0;
    pub const BPLCON0: u16 = 0x100;
    pub const BPLCON1: u16 = 0x102;
    pub const BPLCON2: u16 = 0x104;
    pub const BPL1MOD: u16 = 0x108;
    pub const BPL2MOD: u16 = 0x10A;
    pub const COLOR00: u16 = 0x180;
    pub const COLOR31: u16 = 0x1BE;
}

/// Interrupt bit numbers in `INTENA`/`INTREQ` (Amiga hardware numbering).
pub mod intbit {
    /// CIA-A / ports — 68k level 2.
    pub const PORTS: u16 = 3;
    /// Vertical blank — 68k level 3.
    pub const VERTB: u16 = 5;
    /// CIA-B / external — 68k level 6.
    pub const EXTER: u16 = 13;
    /// Master interrupt enable (INTENA bit 14). Not an interrupt source.
    pub const INTEN: u16 = 14;
    /// Set/clear control bit, shared by INTENA/INTREQ/DMACON writes.
    pub const SETCLR: u16 = 15;
}

/// Frame clock: PAL 50 Hz by default, 312 lines, 227 colour clocks a line.
pub const PAL_LINES_PER_FRAME: u32 = 312;
pub const PAL_COLOUR_CLOCKS_PER_LINE: u32 = 227;

/// ECS Agnus identification bits reported in the high byte of `VPOSR`.
///
/// graphics.library reads this to decide which chipset it is running on;
/// the machine reports ECS Agnus with 2 MB chip RAM (proposal §6.1), not
/// AGA — AGA is an explicitly contained later extension (§2 non-goals).
pub const VPOSR_AGNUS_ID: u16 = 0x2000;

/// The custom-chip register file.
#[derive(Default)]
pub struct Chipset {
    /// Interrupt enable mask (`INTENA`), bit 14 is the master enable.
    pub intena: u16,
    /// Interrupt request latch (`INTREQ`).
    pub intreq: u16,
    /// DMA control latch (`DMACON`). No DMA is performed; `BBUSY`/`BZERO`
    /// are supplied by the Phase 2 blitter and read back as clear here.
    pub dmacon: u16,
    /// Audio/disk control latch (`ADKCON`). Latched, never acted on.
    pub adkcon: u16,

    /// Free-running raster line within the frame, from the frame clock.
    pub vpos: u32,
    /// Free-running horizontal position within the line.
    pub hpos: u32,
    /// Colour-clock remainder carried between `tick` calls.
    carry: u32,
    /// Frames completed since reset; the renderer and VERTB share this.
    pub frames: u64,

    /// Mouse/joystick position registers, fed from host input.
    pub joy0dat: u16,
    pub joy1dat: u16,
    /// Potentiometer register; mouse buttons read back through `POTGOR`.
    pub potgor: u16,
}

impl Chipset {
    pub fn new() -> Self {
        Self {
            // POTGOR reads all-ones when no pot inputs are pulled low,
            // which is what "no buttons pressed" looks like to the OS.
            potgor: 0xFFFF,
            ..Default::default()
        }
    }

    /// Read a register. `offset` is the offset within `$DFF000`, already
    /// masked to the register window by the caller.
    ///
    /// Unimplemented registers return `$FFFF` (open bus), matching the
    /// rule the rest of the machine follows.
    pub fn read(&mut self, offset: u16) -> u16 {
        match offset {
            reg::VPOSR => self.vposr(),
            reg::VHPOSR => self.vhposr(),
            reg::INTENAR => self.intena,
            reg::INTREQR => self.intreq,
            reg::DMACONR => self.dmacon,
            reg::ADKCONR => self.adkcon,
            reg::JOY0DAT => self.joy0dat,
            reg::JOY1DAT => self.joy1dat,
            reg::POTGOR => self.potgor,
            // No disk and no serial hardware: report "idle, nothing to do"
            // so trackdisk.device and serial.device settle instead of
            // spinning. SERDATR bit 13 is TBE (transmit buffer empty).
            reg::SERDATR => 0x2000,
            reg::DSKBYTR => 0x0000,
            _ => 0xFFFF,
        }
    }

    /// Write a register. Unimplemented registers are silently discarded.
    pub fn write(&mut self, offset: u16, value: u16) {
        match offset {
            reg::INTENA => self.intena = apply_setclr(self.intena, value),
            reg::INTREQ => self.intreq = apply_setclr(self.intreq, value),
            reg::DMACON => self.dmacon = apply_setclr(self.dmacon, value),
            reg::ADKCON => self.adkcon = apply_setclr(self.adkcon, value),
            _ => {}
        }
    }

    /// `VPOSR`: Agnus ID in the high byte, bit 0 is vertical position bit 8.
    fn vposr(&self) -> u16 {
        VPOSR_AGNUS_ID | ((self.vpos >> 8) & 1) as u16
    }

    /// `VHPOSR`: vertical position low byte, horizontal position low byte.
    fn vhposr(&self) -> u16 {
        (((self.vpos & 0xFF) << 8) | (self.hpos & 0xFF)) as u16
    }

    /// Raise an interrupt source (used by the CIAs and, later, by cards).
    pub fn raise_int(&mut self, bit: u16) {
        self.intreq |= 1 << bit;
    }

    /// The 68k interrupt level the chipset is currently requesting, 0 for
    /// none. Levels follow the hardware mapping of INTREQ bits to IPL.
    pub fn pending_level(&self) -> u8 {
        if self.intena & (1 << intbit::INTEN) == 0 {
            return 0;
        }
        let active = self.intena & self.intreq;
        if active == 0 {
            return 0;
        }
        // Highest set bit wins; the hardware groups bits into levels 1-6.
        let highest = 15 - active.leading_zeros() as u16;
        match highest {
            0..=2 => 1,
            3..=4 => 2,
            5..=6 => 3,
            7..=8 => 4,
            9..=10 => 5,
            11..=13 => 6,
            _ => 0,
        }
    }

    /// Advance the frame clock by `cpu_clocks` CPU cycles.
    ///
    /// Returns `true` when a frame boundary was crossed, which the caller
    /// turns into VERTB. One clock feeds the beam counter, VERTB and (from
    /// Phase 2) the renderer, so `WaitTOF`, VBlank servers and copper
    /// positions all agree — proposal §7.1.
    pub fn tick(&mut self, cpu_clocks: u32, cpu_clocks_per_colour_clock: u32) -> bool {
        let total = self.carry + cpu_clocks;
        let colour_clocks = total / cpu_clocks_per_colour_clock;
        self.carry = total % cpu_clocks_per_colour_clock;

        let mut wrapped = false;
        self.hpos += colour_clocks;
        while self.hpos >= PAL_COLOUR_CLOCKS_PER_LINE {
            self.hpos -= PAL_COLOUR_CLOCKS_PER_LINE;
            self.vpos += 1;
            if self.vpos >= PAL_LINES_PER_FRAME {
                self.vpos = 0;
                self.frames += 1;
                wrapped = true;
                self.raise_int(intbit::VERTB);
            }
        }
        wrapped
    }
}

/// Apply the Amiga's shared set/clear write convention: bit 15 decides
/// whether the remaining bits are set or cleared in the target register.
fn apply_setclr(current: u16, value: u16) -> u16 {
    let bits = value & 0x7FFF;
    if value & (1 << intbit::SETCLR) != 0 {
        current | bits
    } else {
        current & !bits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setclr_sets_and_clears() {
        let mut c = Chipset::new();
        c.write(reg::INTENA, 0x8000 | (1 << intbit::VERTB));
        assert_eq!(c.intena, 1 << intbit::VERTB);
        c.write(reg::INTENA, 1 << intbit::VERTB);
        assert_eq!(c.intena, 0);
    }

    #[test]
    fn vposr_reports_ecs_agnus() {
        let mut c = Chipset::new();
        assert_eq!(c.read(reg::VPOSR) & 0xFF00, VPOSR_AGNUS_ID);
    }

    #[test]
    fn no_interrupt_without_master_enable() {
        let mut c = Chipset::new();
        c.intreq = 1 << intbit::VERTB;
        c.intena = 1 << intbit::VERTB;
        assert_eq!(c.pending_level(), 0, "master enable is clear");
        c.intena |= 1 << intbit::INTEN;
        assert_eq!(c.pending_level(), 3, "VERTB is level 3");
    }

    #[test]
    fn unimplemented_registers_read_open_bus() {
        let mut c = Chipset::new();
        assert_eq!(c.read(0x0F0), 0xFFFF);
    }
}
