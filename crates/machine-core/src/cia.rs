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
//!
//! This is deliberately not cycle-exact (contrast Copperline's CIA model,
//! which reproduces the real 8520's one-E-cycle IRQ pin lag and per-bit
//! CNT shifting): register semantics and interrupt timing are correct to
//! the E-clock tick, which is what register-polling OS code depends on,
//! without modelling the analog pin behaviour underneath it.

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

/// CRA/CRB control bits (proposal §7.2 / 8520 datasheet).
///
/// PBON and OUTMODE are stored for readback but Phase 1 does not model
/// the PB6/PB7 timer-output pins: nothing in the Phase 1 memory map reads
/// CIA port B for anything but the raw register value, so the pulse/toggle
/// side effect on those pins has no observable guest effect yet.
const CRA_START: u8 = 0x01;
const CRA_RUNMODE: u8 = 0x08;
const CRA_LOAD: u8 = 0x10;
/// CRA bit 5: 0 = timer A counts the E-clock, 1 = timer A counts CNT
/// pulses. Nothing in Phase 1 drives the CNT pin, so INMODE=1 simply
/// never counts.
const CRA_INMODE: u8 = 0x20;
/// CRA bit 6: serial port direction. 0 = input (the keyboard shifts
/// bytes into SDR), 1 = output (software is driving the handshake pulse
/// back to the keyboard).
const CRA_SPMODE: u8 = 0x40;

const CRB_START: u8 = 0x01;
const CRB_RUNMODE: u8 = 0x08;
const CRB_LOAD: u8 = 0x10;
/// CRB bits 5-6: timer B input mode. `00` E-clock, `01` CNT (never
/// pulses, so timer B simply never counts), `10` timer-A underflow, `11`
/// timer-A underflow gated by CNT high. CNT idles high when nothing
/// drives it (open-drain, pulled up) and Phase 1 never pulls it low, so
/// the CNT-gated cascade (`11`) is indistinguishable from the ungated one
/// (`10`) here and both are implemented identically.
const CRB_INMODE_MASK: u8 = 0x60;
const CRB_INMODE_SHIFT: u8 = 5;
/// CRB bit 7: TOD register writes target the alarm instead of the
/// counter while this is set.
const CRB_ALARM: u8 = 0x80;

/// The E-clock frequency the CIAs count on, in Hz (PAL).
pub const E_CLOCK_HZ: u32 = 709_379;

/// E-clock ticks the keyboard MCU takes to shift one byte in over
/// KCLK/KDAT before it lands in SDR and raises the SP interrupt.
///
/// Real hardware clocks roughly one bit every ~100 µs; this is a
/// register-level approximation of the ~1 ms an 8-bit byte takes, not a
/// cycle-accurate figure — it only needs to be "a queued keycode doesn't
/// appear in SDR instantaneously" for the handshake to behave like real
/// hardware from software's point of view.
const KEYBOARD_HANDSHAKE_TICKS: u16 = (E_CLOCK_HZ / 1000) as u16;

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
    /// Writing TODHI stops the counter until TODLO is written again.
    tod_stopped: bool,
    /// Edge detector for the alarm comparator: ALRM latches only on the
    /// transition into `tod == tod_alarm`, whether reached by counting or
    /// by a write to either register, not on every tick spent matching.
    tod_matching: bool,

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
    /// E-clock ticks remaining before the next queued keycode lands in
    /// SDR, 0 when no delay is in flight.
    kbd_delay: u16,
    /// True while SDR holds a keycode the OS has not yet acknowledged by
    /// reading SDR. Blocks feeding the next queued code.
    kbd_loaded: bool,

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
            tod_stopped: false,
            // Counter and alarm both reset to zero, already matching, so
            // there is no spurious ALRM the instant TOD starts ticking.
            tod_matching: true,
            icr_mask: 0,
            icr_data: 0,
            sdr: 0,
            keyboard_queue: [0; KEYBOARD_QUEUE_LEN],
            keyboard_head: 0,
            keyboard_len: 0,
            kbd_delay: 0,
            kbd_loaded: false,
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
            reg::SDR => {
                // Reading SDR is what the OS's SP interrupt handler does
                // first, before it pulses CRA SPMODE to send the actual
                // handshake pulse back to the keyboard; by the time that
                // pulse happens the byte has already been read, so this
                // read is what unblocks the next queued keycode.
                self.kbd_loaded = false;
                self.sdr
            }
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
            reg::TAHI => {
                self.latch_a = (self.latch_a & 0x00FF) | ((value as u16) << 8);
                // Real 8520 behaviour software depends on: writing the
                // high byte while the timer is stopped reloads the
                // counter from the latch immediately, rather than
                // waiting for a START.
                if self.cra & CRA_START == 0 {
                    self.timer_a = self.latch_a;
                }
            }
            reg::TBLO => self.latch_b = (self.latch_b & 0xFF00) | value as u16,
            reg::TBHI => {
                self.latch_b = (self.latch_b & 0x00FF) | ((value as u16) << 8);
                if self.crb & CRB_START == 0 {
                    self.timer_b = self.latch_b;
                }
            }
            reg::SDR => self.sdr = value,
            reg::ICR => {
                let bits = value & 0x1F;
                if value & (1 << icr::SETCLR) != 0 {
                    self.icr_mask |= bits;
                } else {
                    self.icr_mask &= !bits;
                }
            }
            reg::CRA => {
                // LOAD (bit 4) is a write-only strobe: force a reload now,
                // but never store the bit itself (it always reads back 0).
                self.cra = value & !CRA_LOAD;
                if value & CRA_LOAD != 0 {
                    self.timer_a = self.latch_a;
                }
            }
            reg::CRB => {
                self.crb = value & !CRB_LOAD;
                if value & CRB_LOAD != 0 {
                    self.timer_b = self.latch_b;
                }
            }
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

    /// Advance TOD by one tick and fire ALRM on a match. The bus/runner
    /// calls this at the rate the hardware wires TOD to: vertical blank
    /// (50 Hz) for CIA-A, horizontal line rate for CIA-B — see this
    /// module's doc comment and the worker report for exactly where.
    pub fn tod_tick(&mut self) {
        if self.tod_stopped {
            return;
        }
        self.tod = self.tod.wrapping_add(1) & 0x00FF_FFFF;
        self.check_tod_alarm();
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

    /// Advance timers and the keyboard serial handshake by whole E-clock
    /// ticks. TOD is not driven from here; see `tod_tick`.
    fn tick_eclock(&mut self, ticks: u32) {
        for _ in 0..ticks {
            self.tick_one_eclock();
        }
    }

    fn tick_one_eclock(&mut self) {
        let mut ta_underflow = false;
        if self.cra & CRA_START != 0 && self.cra & CRA_INMODE == 0 {
            ta_underflow = step_timer(&mut self.timer_a, self.latch_a);
            if ta_underflow {
                self.raise(icr::TA);
                if self.cra & CRA_RUNMODE != 0 {
                    self.cra &= !CRA_START;
                }
            }
        }

        let crb_inmode = (self.crb & CRB_INMODE_MASK) >> CRB_INMODE_SHIFT;
        let tb_counts_this_tick = match crb_inmode {
            0 => true,             // E-clock
            1 => false,            // CNT: never pulses
            2 | 3 => ta_underflow, // timer-A underflow (with/without CNT gate)
            _ => unreachable!("2-bit field"),
        };
        if self.crb & CRB_START != 0 && tb_counts_this_tick {
            let tb_underflow = step_timer(&mut self.timer_b, self.latch_b);
            if tb_underflow {
                self.raise(icr::TB);
                if self.crb & CRB_RUNMODE != 0 {
                    self.crb &= !CRB_START;
                }
            }
        }

        if self.id == CiaId::A {
            self.tick_keyboard();
        }
    }

    /// Feed queued keycodes into SDR through the same shape of delay a
    /// real handshake has: nothing appears in SDR the instant a key
    /// event is queued, and the next byte does not start until the OS
    /// has acknowledged the previous one by reading SDR.
    fn tick_keyboard(&mut self) {
        let input_mode = self.cra & CRA_SPMODE == 0;
        if !input_mode {
            // Software is driving the handshake pulse (SPMODE=1); do not
            // race it with a new byte.
            return;
        }
        if self.kbd_delay == 0 && !self.kbd_loaded && self.keyboard_len > 0 {
            self.kbd_delay = KEYBOARD_HANDSHAKE_TICKS;
        }
        if self.kbd_delay > 0 {
            self.kbd_delay -= 1;
            if self.kbd_delay == 0 {
                if let Some(raw) = self.next_keycode() {
                    self.sdr = encode_keyboard_byte(raw);
                    self.kbd_loaded = true;
                    self.raise(icr::SP);
                }
            }
        }
    }

    fn read_tod(&mut self, reg: u8) -> u8 {
        // Reading TODHI latches the whole 24-bit value so MID/LO reads
        // return a coherent snapshot even if TOD ticks in between; the
        // latch releases only when TODLO is read.
        if reg == reg::TODHI && self.tod_latched.is_none() {
            self.tod_latched = Some(self.tod);
        }
        let value = self.tod_latched.unwrap_or(self.tod);
        let byte = match reg {
            reg::TODLO => value as u8,
            reg::TODMID => (value >> 8) as u8,
            reg::TODHI => (value >> 16) as u8,
            _ => 0,
        };
        if reg == reg::TODLO {
            self.tod_latched = None;
        }
        byte
    }

    fn write_tod(&mut self, reg: u8, value: u8) {
        if self.crb & CRB_ALARM != 0 {
            self.tod_alarm = match reg {
                reg::TODLO => (self.tod_alarm & 0xFFFF00) | value as u32,
                reg::TODMID => (self.tod_alarm & 0xFF00FF) | ((value as u32) << 8),
                reg::TODHI => (self.tod_alarm & 0x00FFFF) | ((value as u32) << 16),
                _ => self.tod_alarm,
            };
        } else {
            self.tod = match reg {
                reg::TODLO => (self.tod & 0xFFFF00) | value as u32,
                reg::TODMID => (self.tod & 0xFF00FF) | ((value as u32) << 8),
                reg::TODHI => (self.tod & 0x00FFFF) | ((value as u32) << 16),
                _ => self.tod,
            };
            // Writing TODHI stops the counter (so a multi-byte write
            // reads/writes a coherent time); writing TODLO is the last
            // byte of that sequence and restarts it.
            match reg {
                reg::TODHI => self.tod_stopped = true,
                reg::TODLO => self.tod_stopped = false,
                _ => {}
            }
        }
        self.check_tod_alarm();
    }

    /// Edge-detect the alarm comparator: ALRM latches only on the
    /// transition into equality, whether reached by ticking or by a
    /// write to either register, not on every tick spent matching.
    fn check_tod_alarm(&mut self) {
        let equal = self.tod == self.tod_alarm;
        if equal && !self.tod_matching {
            self.raise(icr::ALRM);
        }
        self.tod_matching = equal;
    }
}

/// Decrement a timer by one E-clock tick, reloading from `latch` and
/// reporting an underflow when it was already at zero. This is what
/// gives a period of `latch + 1` ticks between underflows: the counter
/// spends `latch` ticks counting down from `latch` to `0`, and the next
/// tick after that is the one that reloads and fires — matching the
/// 8520 datasheet's timer period, not a naive `latch` ticks.
fn step_timer(count: &mut u16, latch: u16) -> bool {
    if *count == 0 {
        *count = latch;
        true
    } else {
        *count -= 1;
        false
    }
}

/// Encode a raw Amiga keycode as the keyboard MCU transmits it: rotated
/// left one bit, then inverted (equivalently `!((raw << 1) | (raw >>
/// 7))`). `keyboard.device` recovers the raw code with `not.b`+`ror.b
/// #1`. Confirmed against Copperline's `chipset::keyboard::on_wire`
/// (`!value.rotate_left(1)`) and matches WinUAE's keybuf handling of the
/// same wire format.
fn encode_keyboard_byte(raw: u8) -> u8 {
    !raw.rotate_left(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Advance a CIA by exactly `ticks` E-clock ticks, bypassing the
    /// CPU-clock/E-clock ratio conversion `tick()` also performs (that
    /// conversion is exercised separately by `MachineBus::tick`).
    fn advance(cia: &mut Cia, ticks: u32) -> bool {
        cia.tick(ticks, 1)
    }

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

    // ---- Timers ----------------------------------------------------

    fn start_timer_a(cia: &mut Cia, latch: u16, oneshot: bool) {
        cia.write(reg::TALO, latch as u8);
        cia.write(reg::TAHI, (latch >> 8) as u8);
        let mut cra = CRA_START;
        if oneshot {
            cra |= CRA_RUNMODE;
        }
        cia.write(reg::CRA, cra);
    }

    #[test]
    fn timer_a_underflows_after_latch_plus_one_ticks() {
        let mut cia = Cia::new(CiaId::A);
        cia.icr_mask = 1 << icr::TA;
        start_timer_a(&mut cia, 3, false);

        for i in 0..3 {
            assert!(!advance(&mut cia, 1), "no underflow yet at tick {i}");
        }
        assert!(advance(&mut cia, 1), "underflow on the 4th tick");
        assert_eq!(cia.timer_a, 3, "reloaded from the latch");
    }

    #[test]
    fn timer_a_continuous_mode_reloads_and_keeps_running() {
        let mut cia = Cia::new(CiaId::A);
        cia.icr_mask = 1 << icr::TA;
        start_timer_a(&mut cia, 1, false);

        advance(&mut cia, 2); // one full underflow
        assert!(cia.cra & CRA_START != 0, "continuous mode stays running");
        let _ = cia.read(reg::ICR);
        advance(&mut cia, 2);
        assert!(cia.irq_pending(), "a second underflow must have happened");
    }

    #[test]
    fn timer_a_one_shot_clears_start_on_underflow() {
        let mut cia = Cia::new(CiaId::A);
        cia.icr_mask = 1 << icr::TA;
        start_timer_a(&mut cia, 1, true);

        assert!(advance(&mut cia, 2));
        assert_eq!(cia.cra & CRA_START, 0, "one-shot clears START on underflow");
        let _ = cia.read(reg::ICR);
        assert!(
            !advance(&mut cia, 10),
            "a stopped one-shot timer must not keep firing"
        );
    }

    #[test]
    fn load_strobe_forces_reload_without_starting() {
        let mut cia = Cia::new(CiaId::A);
        cia.write(reg::TALO, 0x34);
        cia.write(reg::TAHI, 0x12);
        cia.timer_a = 0; // pretend it had already counted down
        cia.write(reg::CRA, CRA_LOAD);

        assert_eq!(cia.timer_a, 0x1234);
        assert_eq!(cia.cra & CRA_START, 0, "LOAD does not also start it");
        assert_eq!(cia.read(reg::CRA) & CRA_LOAD, 0, "LOAD never reads back");
    }

    #[test]
    fn writing_tahi_reloads_when_stopped_but_not_when_running() {
        let mut cia = Cia::new(CiaId::A);
        cia.write(reg::TALO, 0x00);
        cia.write(reg::TAHI, 0x10); // stopped: reloads immediately
        assert_eq!(cia.timer_a, 0x1000);

        cia.write(reg::CRA, CRA_START);
        cia.timer_a = 5;
        cia.write(reg::TAHI, 0x20); // running: latch only, no reload
        assert_eq!(cia.timer_a, 5);
        assert_eq!(cia.latch_a, 0x2000);
    }

    #[test]
    fn timer_a_inmode_cnt_never_counts() {
        // INMODE=1 selects CNT; Phase 1 never pulses CNT, so the timer
        // must simply sit still.
        let mut cia = Cia::new(CiaId::A);
        cia.write(reg::TALO, 5);
        cia.write(reg::TAHI, 0);
        cia.write(reg::CRA, CRA_START | CRA_INMODE);
        advance(&mut cia, 100);
        assert_eq!(cia.timer_a, 5);
    }

    #[test]
    fn timer_b_e_clock_mode_counts_independently_of_timer_a() {
        let mut cia = Cia::new(CiaId::A);
        cia.icr_mask = 1 << icr::TB;
        cia.write(reg::TBLO, 2);
        cia.write(reg::TBHI, 0);
        cia.write(reg::CRB, CRB_START); // INMODE bits 0 = E-clock

        assert!(advance(&mut cia, 3));
        assert_eq!(cia.timer_b, 2, "reloaded from the latch");
    }

    #[test]
    fn timer_b_cascades_from_timer_a_underflow() {
        let mut cia = Cia::new(CiaId::A);
        cia.icr_mask = 1 << icr::TB;
        start_timer_a(&mut cia, 1, false); // A underflows every 2 E-ticks
        cia.write(reg::TBLO, 2);
        cia.write(reg::TBHI, 0);
        // CRB INMODE = 10 (timer-A underflow).
        cia.write(reg::CRB, CRB_START | (2 << CRB_INMODE_SHIFT));

        // Each timer-A underflow is one timer-B count; B needs 3 counts
        // (latch 2 => period 3) to underflow itself, i.e. 3 A-underflows.
        assert_eq!(cia.timer_b, 2, "reloaded from the latch by the TBHI write");
        advance(&mut cia, 2); // 1st A underflow
        assert_eq!(cia.timer_b, 1);
        advance(&mut cia, 2); // 2nd A underflow
        assert_eq!(cia.timer_b, 0);
        assert!(!cia.irq_pending());
        advance(&mut cia, 2); // 3rd A underflow -> B underflows
        assert!(cia.irq_pending());
        assert_eq!(cia.timer_b, 2, "B reloaded from its own latch");
    }

    #[test]
    fn timer_b_cnt_mode_never_counts() {
        let mut cia = Cia::new(CiaId::A);
        cia.write(reg::TBLO, 1);
        cia.write(reg::TBHI, 0);
        cia.write(reg::CRB, CRB_START | (1 << CRB_INMODE_SHIFT)); // INMODE=01: CNT
        advance(&mut cia, 100);
        assert_eq!(cia.timer_b, 1);
    }

    // ---- TOD ---------------------------------------------------------

    #[test]
    fn tod_counts_and_fires_alarm_on_match() {
        let mut cia = Cia::new(CiaId::A);
        cia.icr_mask = 1 << icr::ALRM;
        cia.tod_alarm = 3;

        for _ in 0..2 {
            cia.tod_tick();
            assert!(!cia.irq_pending());
        }
        cia.tod_tick();
        assert!(
            cia.irq_pending(),
            "ALRM fires on the transition into equality"
        );

        let _ = cia.read(reg::ICR);
        cia.tod_tick();
        assert!(
            !cia.irq_pending(),
            "moving past the match must not keep firing"
        );
    }

    #[test]
    fn tod_alarm_does_not_refire_while_stationary() {
        let mut cia = Cia::new(CiaId::A);
        cia.icr_mask = 1 << icr::ALRM;
        cia.write(reg::CRB, CRB_ALARM);
        cia.write(reg::TODLO, 1); // arm the alarm at 1, counter still at 0
        cia.write(reg::CRB, 0); // back to targeting the counter

        cia.tod_tick(); // 0 -> 1: transitions into the match
        assert!(cia.irq_pending());
        let _ = cia.read(reg::ICR);

        // Re-writing the alarm to the same already-matching value must
        // not re-fire (edge-triggered, not level-triggered).
        cia.write(reg::CRB, CRB_ALARM);
        cia.write(reg::TODLO, 1);
        assert!(!cia.irq_pending());
    }

    #[test]
    fn todhi_read_latches_and_todlo_read_releases() {
        let mut cia = Cia::new(CiaId::A);
        cia.tod = 0x010203;

        assert_eq!(cia.read(reg::TODHI), 0x01);
        cia.tod = 0x040506; // ticks in behind the latch
        assert_eq!(cia.read(reg::TODMID), 0x02, "frozen snapshot, not live");
        assert_eq!(cia.read(reg::TODLO), 0x03, "still the frozen snapshot");

        // The latch released on the TODLO read above; a fresh read
        // sequence now sees the live value.
        assert_eq!(cia.read(reg::TODHI), 0x04);
        assert_eq!(cia.read(reg::TODMID), 0x05);
        assert_eq!(cia.read(reg::TODLO), 0x06);
    }

    #[test]
    fn writing_todhi_stops_clock_until_todlo_written() {
        let mut cia = Cia::new(CiaId::A);
        cia.write(reg::TODHI, 0x01);
        cia.tod_tick();
        cia.tod_tick();
        assert_eq!(cia.tod, 0x010000, "stopped after the TODHI write");

        cia.write(reg::TODMID, 0x02);
        cia.write(reg::TODLO, 0x03);
        assert_eq!(cia.tod, 0x010203);
        cia.tod_tick();
        assert_eq!(cia.tod, 0x010204, "TODLO write restarts the clock");
    }

    #[test]
    fn crb_bit7_targets_the_alarm_register_instead_of_the_counter() {
        let mut cia = Cia::new(CiaId::A);
        cia.write(reg::CRB, CRB_ALARM);
        cia.write(reg::TODHI, 0x01);
        cia.write(reg::TODMID, 0x02);
        cia.write(reg::TODLO, 0x03);

        assert_eq!(cia.tod_alarm, 0x010203);
        assert_eq!(cia.tod, 0, "counter untouched while targeting the alarm");
    }

    // ---- Keyboard serial handshake -----------------------------------

    #[test]
    fn keyboard_byte_encoding_matches_hardware_wire_format() {
        // Table-driven against the documented `!((raw << 1) | (raw >> 7))`
        // transform, cross-checked against Copperline's `on_wire`.
        let cases: &[(u8, u8)] = &[
            (0x00, 0xFF),
            (0x01, 0xFD),
            (0x45, 0x75), // a representative Amiga raw keycode
            (0xFF, 0x00),
            (0x80, 0xFE),
        ];
        for &(raw, expected) in cases {
            assert_eq!(encode_keyboard_byte(raw), expected, "raw {raw:#04x}");
        }
    }

    #[test]
    fn keyboard_handshake_feeds_queued_code_after_a_delay() {
        let mut cia = Cia::new(CiaId::A);
        cia.icr_mask = 1 << icr::SP;
        cia.queue_keycode(0x01); // key 1 down

        advance(&mut cia, KEYBOARD_HANDSHAKE_TICKS as u32 - 1);
        assert!(!cia.irq_pending(), "byte must not appear instantly");

        assert!(advance(&mut cia, 1));
        assert_eq!(cia.sdr, encode_keyboard_byte(0x01));
    }

    #[test]
    fn keyboard_handshake_waits_for_sdr_read_before_next_byte() {
        let mut cia = Cia::new(CiaId::A);
        cia.icr_mask = 1 << icr::SP;
        cia.queue_keycode(0x01);
        cia.queue_keycode(0x02);

        advance(&mut cia, KEYBOARD_HANDSHAKE_TICKS as u32);
        assert_eq!(cia.sdr, encode_keyboard_byte(0x01));
        let _ = cia.read(reg::ICR);

        // Without a SDR read acknowledging the first byte, the second
        // must not start loading no matter how long we wait.
        advance(&mut cia, KEYBOARD_HANDSHAKE_TICKS as u32 * 4);
        assert_eq!(cia.sdr, encode_keyboard_byte(0x01));

        // Reading SDR is the acknowledgement that unblocks the next byte.
        let _ = cia.read(reg::SDR);
        advance(&mut cia, KEYBOARD_HANDSHAKE_TICKS as u32);
        assert_eq!(cia.sdr, encode_keyboard_byte(0x02));
    }

    #[test]
    fn keyboard_handshake_pauses_while_spmode_is_output() {
        let mut cia = Cia::new(CiaId::A);
        cia.queue_keycode(0x01);
        cia.write(reg::CRA, CRA_SPMODE); // OS driving the handshake pulse

        advance(&mut cia, KEYBOARD_HANDSHAKE_TICKS as u32 * 4);
        assert_eq!(cia.sdr, 0, "no byte loads while SPMODE is output");

        cia.write(reg::CRA, 0); // back to input mode
        advance(&mut cia, KEYBOARD_HANDSHAKE_TICKS as u32);
        assert_eq!(cia.sdr, encode_keyboard_byte(0x01));
    }

    // ---- ICR masking ---------------------------------------------------

    #[test]
    fn icr_mask_gates_each_source_independently() {
        let cases: &[(u8, u8)] = &[
            (icr::TA, 1 << icr::TA),
            (icr::TB, 1 << icr::TB),
            (icr::ALRM, 1 << icr::ALRM),
            (icr::SP, 1 << icr::SP),
            (icr::FLG, 1 << icr::FLG),
        ];
        for &(bit, mask_bit) in cases {
            let mut cia = Cia::new(CiaId::A);
            cia.raise(bit);
            assert!(
                !cia.irq_pending(),
                "bit {bit} must not request without a mask"
            );
            cia.icr_mask = mask_bit;
            assert!(cia.irq_pending(), "bit {bit} must request once masked in");
        }
    }

    #[test]
    fn icr_write_set_clear_convention() {
        let mut cia = Cia::new(CiaId::A);
        cia.write(reg::ICR, 0x80 | (1 << icr::TA) | (1 << icr::TB));
        assert_eq!(cia.icr_mask, (1 << icr::TA) | (1 << icr::TB));
        cia.write(reg::ICR, 1 << icr::TA);
        assert_eq!(cia.icr_mask, 1 << icr::TB);
    }
}
