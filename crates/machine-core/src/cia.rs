//! The two 8520 CIAs, register-level (not cycle-level) faithful — §7.2.
//!
//! CIA-A lives at `$BFE001` (odd bytes, `A0` high), CIA-B at `$BFD000`
//! (even bytes). Register selection is by address bits 8-12, and both
//! chips mirror across the whole `$A00000-$BFFFFF` window as real
//! hardware does.
//!
//! Phase 1 needs: timers A/B on the E-clock, TOD with alarm, the ICR
//! with its read-clears-and-acknowledges semantics, and the CIA-A serial
//! port used for the keyboard. The keyboard path matters more than it
//! looks: clocking host key events in as the hardware does is what lets
//! keyboard.device run unmodified (the Amithlon trick, §7.2).

/// Register indices within a CIA, as the hardware numbers them.
pub mod reg {
    pub const PRA: u8 = 0x0;
    pub const PRB: u8 = 0x1;
    pub const DDRA: u8 = 0x2;
    pub const DDRB: u8 = 0x3;
    pub const TALO: u8 = 0x4;
    pub const TAHI: u8 = 0x5;
    pub const TBLO: u8 = 0x6;
    pub const TBHI: u8 = 0x7;
    pub const TODLO: u8 = 0x8;
    pub const TODMID: u8 = 0x9;
    pub const TODHI: u8 = 0xA;
    pub const SDR: u8 = 0xC;
    pub const ICR: u8 = 0xD;
    pub const CRA: u8 = 0xE;
    pub const CRB: u8 = 0xF;
}

/// ICR bit numbers.
pub mod icr {
    pub const TA: u8 = 0;
    pub const TB: u8 = 1;
    pub const ALRM: u8 = 2;
    pub const SP: u8 = 3;
    pub const FLG: u8 = 4;
    /// Read-back only: set when any enabled source is requesting.
    pub const IR: u8 = 7;
    /// Write-only set/clear control, same convention as the custom chips.
    pub const SETCLR: u8 = 7;
}

/// The E-clock frequency the CIAs count on, in Hz (PAL).
pub const E_CLOCK_HZ: u32 = 709_379;

/// Which of the two CIAs an instance is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CiaId {
    /// CIA-A: keyboard serial port, OVL/LED on port A, INT2.
    A,
    /// CIA-B: serial/parallel handshake lines, INT6.
    B,
}

/// One 8520.
pub struct Cia {
    pub id: CiaId,

    pub pra: u8,
    pub prb: u8,
    pub ddra: u8,
    pub ddrb: u8,

    /// Timer A/B current values and latches.
    pub timer_a: u16,
    pub timer_b: u16,
    pub latch_a: u16,
    pub latch_b: u16,
    pub cra: u8,
    pub crb: u8,

    /// 24-bit TOD counter and its alarm.
    pub tod: u32,
    pub tod_alarm: u32,
    /// TOD reads latch until the high byte is read; writes set the alarm
    /// when CRB bit 7 is set, otherwise the counter.
    pub tod_latched: Option<u32>,

    /// Interrupt control: `icr_mask` is the enable, `icr_data` the
    /// pending sources. Reading ICR returns the data and clears it.
    pub icr_mask: u8,
    pub icr_data: u8,

    /// Serial data register. On CIA-A this is the keyboard path.
    pub sdr: u8,

    /// Host key events waiting to be clocked in through the serial port.
    keyboard_queue: [u8; KEYBOARD_QUEUE_LEN],
    keyboard_head: usize,
    keyboard_len: usize,

    /// E-clock remainder carried between `tick` calls.
    carry: u32,
}

const KEYBOARD_QUEUE_LEN: usize = 16;

impl Cia {
    pub fn new(id: CiaId) -> Self {
        Self {
            id,
            // CIA-A PRA bit 0 (OVL) is high after reset, which is what
            // puts ROM at $000000 until the OS clears it. See
            // `MachineBus::overlay`.
            pra: if id == CiaId::A { 0x01 } else { 0x00 },
            prb: 0,
            ddra: 0,
            ddrb: 0,
            timer_a: 0xFFFF,
            timer_b: 0xFFFF,
            latch_a: 0xFFFF,
            latch_b: 0xFFFF,
            cra: 0,
            crb: 0,
            tod: 0,
            tod_alarm: 0,
            tod_latched: None,
            icr_mask: 0,
            icr_data: 0,
            sdr: 0,
            keyboard_queue: [0; KEYBOARD_QUEUE_LEN],
            keyboard_head: 0,
            keyboard_len: 0,
            carry: 0,
        }
    }

    /// True while this CIA-A is asserting the ROM overlay (PRA bit 0).
    ///
    /// Meaningless on CIA-B; the bus only consults CIA-A.
    pub fn ovl_asserted(&self) -> bool {
        self.id == CiaId::A && self.pra & 0x01 != 0
    }

    /// Read a register (0-15).
    pub fn read(&mut self, reg: u8) -> u8 {
        match reg & 0x0F {
            reg::PRA => self.pra,
            reg::PRB => self.prb,
            reg::DDRA => self.ddra,
            reg::DDRB => self.ddrb,
            reg::TALO => self.timer_a as u8,
            reg::TAHI => (self.timer_a >> 8) as u8,
            reg::TBLO => self.timer_b as u8,
            reg::TBHI => (self.timer_b >> 8) as u8,
            reg::SDR => self.sdr,
            reg::ICR => {
                // Reading ICR returns pending sources with IR in bit 7,
                // then clears the latch and drops the interrupt line.
                let mut value = self.icr_data;
                if self.icr_data & self.icr_mask != 0 {
                    value |= 1 << icr::IR;
                }
                self.icr_data = 0;
                value
            }
            reg::CRA => self.cra,
            reg::CRB => self.crb,
            // TOD read/latch semantics are the worker's to implement.
            reg::TODLO | reg::TODMID | reg::TODHI => self.read_tod(reg & 0x0F),
            _ => 0xFF,
        }
    }

    /// Write a register (0-15).
    pub fn write(&mut self, reg: u8, value: u8) {
        match reg & 0x0F {
            reg::PRA => self.pra = value,
            reg::PRB => self.prb = value,
            reg::DDRA => self.ddra = value,
            reg::DDRB => self.ddrb = value,
            reg::TALO => self.latch_a = (self.latch_a & 0xFF00) | value as u16,
            reg::TAHI => self.latch_a = (self.latch_a & 0x00FF) | ((value as u16) << 8),
            reg::TBLO => self.latch_b = (self.latch_b & 0xFF00) | value as u16,
            reg::TBHI => self.latch_b = (self.latch_b & 0x00FF) | ((value as u16) << 8),
            reg::SDR => self.sdr = value,
            reg::ICR => {
                let bits = value & 0x1F;
                if value & (1 << icr::SETCLR) != 0 {
                    self.icr_mask |= bits;
                } else {
                    self.icr_mask &= !bits;
                }
            }
            reg::CRA => self.cra = value,
            reg::CRB => self.crb = value,
            reg::TODLO | reg::TODMID | reg::TODHI => self.write_tod(reg & 0x0F, value),
            _ => {}
        }
    }

    /// Raise an ICR source.
    pub fn raise(&mut self, bit: u8) {
        self.icr_data |= 1 << bit;
    }

    /// True while this CIA is requesting an interrupt.
    pub fn irq_pending(&self) -> bool {
        self.icr_data & self.icr_mask != 0
    }

    /// Queue a raw Amiga keycode to be clocked in through the CIA-A
    /// serial port. Dropped if the queue is full (as a real keyboard's
    /// handshake would simply not be serviced yet).
    pub fn queue_keycode(&mut self, raw: u8) {
        if self.keyboard_len < KEYBOARD_QUEUE_LEN {
            let tail = (self.keyboard_head + self.keyboard_len) % KEYBOARD_QUEUE_LEN;
            self.keyboard_queue[tail] = raw;
            self.keyboard_len += 1;
        }
    }

    /// Pop the next queued keycode, if any.
    pub fn next_keycode(&mut self) -> Option<u8> {
        if self.keyboard_len == 0 {
            return None;
        }
        let value = self.keyboard_queue[self.keyboard_head];
        self.keyboard_head = (self.keyboard_head + 1) % KEYBOARD_QUEUE_LEN;
        self.keyboard_len -= 1;
        Some(value)
    }

    /// Advance the CIA by `cpu_clocks` CPU cycles, converting to E-clock
    /// ticks internally. Returns true while an interrupt is requested.
    pub fn tick(&mut self, cpu_clocks: u32, cpu_clocks_per_eclock: u32) -> bool {
        let total = self.carry + cpu_clocks;
        let eclocks = total / cpu_clocks_per_eclock;
        self.carry = total % cpu_clocks_per_eclock;
        if eclocks > 0 {
            self.tick_eclock(eclocks);
        }
        self.irq_pending()
    }

    /// Advance timers/TOD/serial by whole E-clock ticks.
    fn tick_eclock(&mut self, _ticks: u32) {
        // Implemented in Phase 1: timer A/B underflow and reload per
        // CRA/CRB mode bits, TOD counting and alarm compare, and the
        // keyboard serial handshake feeding SDR + the SP interrupt.
    }

    fn read_tod(&mut self, _reg: u8) -> u8 {
        // Implemented in Phase 1: latch-on-high-byte-read semantics.
        0
    }

    fn write_tod(&mut self, _reg: u8, _value: u8) {
        // Implemented in Phase 1: writes target the alarm when CRB bit 7
        // is set, the counter otherwise.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cia_a_asserts_overlay_after_reset() {
        let cia = Cia::new(CiaId::A);
        assert!(cia.ovl_asserted(), "OVL is high out of reset");
    }

    #[test]
    fn cia_b_never_asserts_overlay() {
        let cia = Cia::new(CiaId::B);
        assert!(!cia.ovl_asserted());
    }

    #[test]
    fn clearing_pra_bit0_releases_overlay() {
        let mut cia = Cia::new(CiaId::A);
        cia.write(reg::PRA, 0x00);
        assert!(!cia.ovl_asserted());
    }

    #[test]
    fn reading_icr_clears_it() {
        let mut cia = Cia::new(CiaId::A);
        cia.icr_mask = 1 << icr::TA;
        cia.raise(icr::TA);
        assert!(cia.irq_pending());
        let value = cia.read(reg::ICR);
        assert_eq!(value & (1 << icr::TA), 1 << icr::TA);
        assert_eq!(value & (1 << icr::IR), 1 << icr::IR);
        assert!(!cia.irq_pending(), "ICR read acknowledges");
    }

    #[test]
    fn keyboard_queue_roundtrips() {
        let mut cia = Cia::new(CiaId::A);
        cia.queue_keycode(0x40);
        cia.queue_keycode(0xC0);
        assert_eq!(cia.next_keycode(), Some(0x40));
        assert_eq!(cia.next_keycode(), Some(0xC0));
        assert_eq!(cia.next_keycode(), None);
    }
}
