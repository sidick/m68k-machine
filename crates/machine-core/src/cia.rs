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

    /// Output latch: the last value written to PRA, returned as-is on
    /// read for any bit `DDRA` configures as an output. The 8520 always
    /// latches a PRA/PRB write regardless of direction -- only whether
    /// that latch actually drives the physical pin (and so is what a
    /// read sees) depends on `DDRA`/`DDRB`.
    pub pra: u8,
    pub prb: u8,
    pub ddra: u8,
    pub ddrb: u8,
    /// External pin levels for whichever PRA bits `DDRA` currently
    /// configures as inputs: the mouse-button fire pins and (§ this
    /// module's `FloppyDrive`) the disk status pins. Idle-high (`0xFF`)
    /// out of reset, matching every one of those pins being an
    /// active-low signal nothing is currently pulling down.
    pub pra_input: u8,
    /// External pin levels for `DDRB`-input-configured PRB bits. Nothing
    /// in this machine drives PRB as an input today (every disk-control
    /// line is a CIA output), but the field exists so `read()` applies
    /// the same direction-aware rule to both ports rather than
    /// special-casing PRA.
    pub prb_input: u8,

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
            pra_input: 0xFF,
            prb_input: 0xFF,
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
            // A bit `DDRA`/`DDRB` configures as output (1) reads back the
            // output latch; a bit configured as input (0) reads the
            // actual external pin level instead. Before this, PRA/PRB
            // were plain latches that ignored DDR entirely, which is
            // exactly why the disk-status pins (PRA 2-5, always inputs)
            // could never report anything but whatever software last
            // happened to write there.
            reg::PRA => (self.pra & self.ddra) | (self.pra_input & !self.ddra),
            reg::PRB => (self.prb & self.ddrb) | (self.prb_input & !self.ddrb),
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
                // waiting for a START -- and in one-shot mode it also
                // sets START itself (MOS 8520 datasheet: "In one-shot
                // mode, a write to timer-high will transfer the timer
                // latch to the counter and initiate counting").
                // Kickstart's timer.device relies on the one-shot
                // auto-start for every MICROHZ interval: it programs
                // CRA to one-shot, then writes TALO/TAHI and never
                // touches START -- without this, no timer.device delay
                // ever fires and boot parks before the insert-disk
                // screen.
                if self.cra & CRA_RUNMODE != 0 {
                    self.timer_a = self.latch_a;
                    self.cra |= CRA_START;
                } else if self.cra & CRA_START == 0 {
                    self.timer_a = self.latch_a;
                }
            }
            reg::TBLO => self.latch_b = (self.latch_b & 0xFF00) | value as u16,
            reg::TBHI => {
                self.latch_b = (self.latch_b & 0x00FF) | ((value as u16) << 8);
                // Same one-shot auto-start rule as TAHI.
                if self.crb & CRB_RUNMODE != 0 {
                    self.timer_b = self.latch_b;
                    self.crb |= CRB_START;
                } else if self.crb & CRB_START == 0 {
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

// ---- Floppy drive status (CIA-A PRA 2-5 / CIA-B PRB) -----------------
//
// Disk presence is not sensed through DSKBYTR/DSKLEN/DSKPT at all (the
// proposal's "no disk; sink" for those registers is correct on its own
// terms) -- it is sensed entirely through these CIA port pins, all
// active low (Amiga Hardware Reference Manual's CIA port table):
//
//   CIA-A PRA (inputs): bit 2 DSKCHNG, bit 3 DSKPROT, bit 4 DSKTRACK0,
//   bit 5 DSKRDY.
//   CIA-B PRB (outputs): bit 0 DSKSTEP, bit 1 DSKDIREC, bit 2 DSKSIDE,
//   bits 3-6 DSKSEL0-DSKSEL3, bit 7 DSKMOTOR.
//
// Without a model of these, `trackdisk.device` waits forever for a
// DSKRDY transition that never comes (Phase 1's `--inspect` shows it
// parked on signal 0x400) and Kickstart never reaches its no-boot-media
// screen. Cross-checked against Copperline's `floppy::mod` (GPL-3, read
// for understanding, never copied) for the two behaviours that are not
// obvious from the bit table alone: the motor relay is a latch clocked
// by the drive's own SELECT falling edge (not MTR's level), and an
// external drive answers a motor-off select cycle by shifting its
// 32-bit ID out through DSKRDY, one bit per deselect, MSB first.

/// CIA-B PRB bit for DSKSTEP: active low, and the mechanism moves on the
/// falling edge (a pulse), not the level.
const CIAB_DSKSTEP: u8 = 1 << 0;
/// CIA-B PRB bit for DSKDIREC, latched at the DSKSTEP falling edge: 0 =
/// step inward (toward higher cylinder numbers), 1 = step outward
/// (toward track 0).
const CIAB_DSKDIREC: u8 = 1 << 1;
/// CIA-B PRB bit for DSKSEL0 (df0's select line, active low). This
/// machine models a single drive bay; DSKSEL1-3 (other units in a
/// daisy chain this machine never has) are not wired to anything.
const CIAB_DSKSEL0: u8 = 1 << 3;
/// CIA-B PRB bit for DSKMOTOR: active low, 0 = motor commanded on.
const CIAB_DSKMOTOR: u8 = 1 << 7;

const CIAA_DSKCHANGE: u8 = 1 << 2;
const CIAA_DSKPROT: u8 = 1 << 3;
const CIAA_DSKTRACK0: u8 = 1 << 4;
const CIAA_DSKRDY: u8 = 1 << 5;
/// Mask of the four PRA bits `FloppyDrive` drives. The rest of PRA
/// (OVL, LED, the joystick/mouse fire buttons) is somebody else's
/// concern.
pub const FLOPPY_PRA_MASK: u8 = CIAA_DSKCHANGE | CIAA_DSKPROT | CIAA_DSKTRACK0 | CIAA_DSKRDY;

/// Head travel limit: a real 3.5" DD mechanism has 80 cylinders (0-79).
const MAX_CYLINDER: u8 = 79;

/// Which floppy configuration `FloppyDrive` presents, selectable via
/// `machine-hosted`'s `--floppy` flag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FloppyPresence {
    /// No physical drive at all. This machine's honest hardware story
    /// (proposal §3, §10.3): storage is MIRAGE over Zorro III, this
    /// machine has no floppy connector to begin with. The default --
    /// confirmed against real hardware (an Amiga with no drive attached
    /// still reaches the no-boot-media screen) and against Amiberry
    /// configured the same way, under both Kickstart 1.3 and 3.2.3.
    None,
    /// A drive is present with no disk in it. Kept as a selectable
    /// diagnostic/compatibility mode, not the default: it is a fiction
    /// for a machine with no floppy hardware, and the "no drive"
    /// configuration already reaches the same no-boot-media screen on
    /// real hardware.
    Empty,
}

impl FloppyPresence {
    /// The 32-bit drive ID a DSKRDY shift sequence clocks out while the
    /// motor is off and the drive selected: `$00000000` for no drive
    /// (an unpopulated select line simply never answers), `$FFFFFFFF`
    /// for a standard 3.5" DD drive (Amiga Hardware Reference Manual;
    /// matches Copperline's `STANDARD_EXTERNAL_DRIVE_ID`).
    fn drive_id(self) -> u32 {
        match self {
            FloppyPresence::None => 0x0000_0000,
            FloppyPresence::Empty => 0xFFFF_FFFF,
        }
    }
}

/// A single floppy drive bay (df0), supplying CIA-A PRA bits 2-5 from
/// CIA-B PRB writes. See this module's floppy-status doc comment above
/// for the polarity references and the oracle this was cross-checked
/// against.
pub struct FloppyDrive {
    presence: FloppyPresence,
    motor_on: bool,
    selected: bool,
    /// Head position, 0-79. Only cylinder 0 is externally observable
    /// (DSKTRACK0), but the full range is tracked so a recalibration
    /// seek away from and back to 0 behaves plausibly.
    cylinder: u8,

    /// True while the drive is clocking its 32-bit ID out through
    /// DSKRDY: entered on a motor on->off transition, left on the next
    /// off->on transition. Only reachable under `FloppyPresence::Empty`
    /// -- `None` has no drive, let alone an ID circuit, to run it.
    id_mode: bool,
    /// Next bit to emit, 0-31 MSB first; clamped at 32 once exhausted,
    /// which reads as 0 forever after (a real shift register run past
    /// its length).
    id_bit: u8,
    /// The select-deactivate edge that follows immediately from
    /// entering ID mode is the very edge that carried the motor-off
    /// command; it does not itself clock a bit out. Only later deselect
    /// edges do -- this flag absorbs that first one.
    id_hold_first_edge: bool,
}

impl FloppyDrive {
    pub fn new(presence: FloppyPresence) -> Self {
        Self {
            presence,
            motor_on: false,
            selected: false,
            cylinder: 0,
            id_mode: false,
            id_bit: 0,
            id_hold_first_edge: false,
        }
    }

    pub fn presence(&self) -> FloppyPresence {
        self.presence
    }

    /// Apply a CIA-B PRB write, given the value before and after, and
    /// update motor/select/step/ID-shift state from the edges between
    /// them. A no-op under `FloppyPresence::None`: nothing answers the
    /// select line, so there is no edge behaviour to model.
    pub fn on_prb_write(&mut self, prev: u8, val: u8) {
        if self.presence == FloppyPresence::None {
            return;
        }

        let was_selected = prev & CIAB_DSKSEL0 == 0;
        let selected = val & CIAB_DSKSEL0 == 0;
        self.selected = selected;

        if !was_selected && selected {
            // The motor relay is a latch clocked by the drive's own
            // SELECT falling edge, not the level of MTR: MTR asserted
            // (low) on either side of this edge starts the motor, and
            // only MTR deasserted (high) on both sides stops it. This is
            // why trackdisk deselects, changes MTR, then reselects to
            // change motor state, and why a stray PRB write restoring an
            // old MTR-high shadow value right after starting the motor
            // does not stop it again.
            let motor_next = (prev & CIAB_DSKMOTOR == 0) || (val & CIAB_DSKMOTOR == 0);
            if motor_next {
                self.id_mode = false;
                self.id_bit = 0;
            } else if self.motor_on {
                // An on->off transition: start clocking the ID out.
                self.id_mode = true;
                self.id_bit = 0;
                self.id_hold_first_edge = true;
            }
            self.motor_on = motor_next;
        } else if was_selected && !selected && self.id_mode && !self.motor_on {
            if self.id_hold_first_edge {
                self.id_hold_first_edge = false;
            } else {
                self.id_bit = self.id_bit.saturating_add(1).min(32);
            }
        }

        // DSKSTEP is active low; the mechanism moves on the falling
        // edge, latching whatever DSKDIREC reads in the same write (not
        // the prior PRB state) -- some trackloaders set direction and
        // pulse step in a single write.
        let step_falling_edge = (prev & CIAB_DSKSTEP != 0) && (val & CIAB_DSKSTEP == 0);
        if selected && step_falling_edge {
            let inward = val & CIAB_DSKDIREC == 0;
            if inward {
                self.cylinder = self.cylinder.saturating_add(1).min(MAX_CYLINDER);
            } else {
                self.cylinder = self.cylinder.saturating_sub(1);
            }
        }
    }

    /// The four CIA-A PRA bits (2-5) this drive currently drives,
    /// already in PRA bit position with the active-low convention
    /// applied (bit set = deasserted / idle).
    pub fn pra_status_bits(&self) -> u8 {
        if self.presence == FloppyPresence::None || !self.selected {
            // Unselected, or no drive to answer selection at all: these
            // status lines are shared across every drive bay in the
            // daisy chain and only the selected drive pulls them low, so
            // an unselected (or nonexistent) drive leaves them at their
            // pulled-up idle level -- all four deasserted.
            return FLOPPY_PRA_MASK;
        }

        let mut bits = 0u8;
        // DSKCHNG stays asserted (0) always: this drive never has a
        // disk in it, so there is no insert event to ever clear the
        // change latch.
        // DSKPROT: no disk, nothing to write-protect -- deasserted.
        bits |= CIAA_DSKPROT;
        if self.cylinder != 0 {
            bits |= CIAA_DSKTRACK0;
        }
        let rdy_asserted = self.id_mode && !self.motor_on && self.id_shift_bit();
        if !rdy_asserted {
            bits |= CIAA_DSKRDY;
        }
        bits
    }

    fn id_shift_bit(&self) -> bool {
        shift_id_bit(self.presence.drive_id(), self.id_bit)
    }
}

/// The bit a 32-bit drive-ID shift register presents at `index` (0-31,
/// MSB first; any `index >= 32` reads 0, as a real shift register run
/// past its length would).
fn shift_id_bit(id: u32, index: u8) -> bool {
    if index >= 32 {
        return false;
    }
    id & (1 << (31 - index)) != 0
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

    /// The 8520 auto-start Kickstart's timer.device depends on: with
    /// RUNMODE (one-shot) already set, writing the timer's high byte
    /// loads the counter from the latch *and* sets START itself (MOS
    /// 8520 datasheet: "In one-shot mode, a write to timer-high will
    /// transfer the timer latch to the counter and initiate counting").
    /// The OS programs every MICROHZ interval as "CRA <- one-shot,
    /// TALO, TAHI" and never touches START -- without the auto-start no
    /// timer.device delay ever fires, and Kickstart 3.2 parks before it
    /// ever puts up the insert-disk screen.
    #[test]
    fn tahi_write_in_one_shot_mode_starts_timer_a() {
        let mut cia = Cia::new(CiaId::A);
        cia.icr_mask = 1 << icr::TA;
        cia.write(reg::CRA, CRA_RUNMODE);
        cia.write(reg::TALO, 3);
        assert_eq!(cia.cra & CRA_START, 0, "TALO alone must not start it");
        cia.write(reg::TAHI, 0);
        assert_ne!(
            cia.cra & CRA_START,
            0,
            "TAHI write auto-starts a one-shot timer"
        );
        for i in 0..3 {
            assert!(!advance(&mut cia, 1), "no underflow yet at tick {i}");
        }
        assert!(advance(&mut cia, 1), "one-shot fires after latch+1 ticks");
        assert_eq!(cia.cra & CRA_START, 0, "and stops itself again");
    }

    /// The auto-start is a one-shot-mode behaviour only: in continuous
    /// mode a TAHI write just latches (and reloads a stopped counter),
    /// leaving START to the guest.
    #[test]
    fn tahi_write_in_continuous_mode_does_not_start_timer_a() {
        let mut cia = Cia::new(CiaId::A);
        cia.icr_mask = 1 << icr::TA;
        cia.write(reg::TALO, 3);
        cia.write(reg::TAHI, 0);
        assert_eq!(cia.cra & CRA_START, 0, "continuous mode never auto-starts");
        assert!(!advance(&mut cia, 8), "and the timer is not counting");
    }

    /// Timer B follows the same one-shot auto-start rule as timer A.
    #[test]
    fn tbhi_write_in_one_shot_mode_starts_timer_b() {
        let mut cia = Cia::new(CiaId::B);
        cia.icr_mask = 1 << icr::TB;
        cia.write(reg::CRB, CRB_RUNMODE);
        cia.write(reg::TBLO, 3);
        assert_eq!(cia.crb & CRB_START, 0, "TBLO alone must not start it");
        cia.write(reg::TBHI, 0);
        assert_ne!(
            cia.crb & CRB_START,
            0,
            "TBHI write auto-starts a one-shot timer"
        );
        for i in 0..3 {
            assert!(!advance(&mut cia, 1), "no underflow yet at tick {i}");
        }
        assert!(advance(&mut cia, 1), "one-shot fires after latch+1 ticks");
        assert_eq!(cia.crb & CRB_START, 0, "and stops itself again");
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

    // ---- Port direction semantics -------------------------------------

    #[test]
    fn pra_read_returns_latch_for_output_bits_and_pin_for_input_bits() {
        let mut cia = Cia::new(CiaId::A);
        // Bits 0-3 output, 4-7 input.
        cia.write(reg::DDRA, 0x0F);
        cia.write(reg::PRA, 0xAA);
        cia.pra_input = 0x55;

        // Low nibble comes from the latch (0xAA & 0x0F = 0x0A), high
        // nibble from the pin field (0x55 & 0xF0 = 0x50).
        assert_eq!(cia.read(reg::PRA), 0x5A);
    }

    #[test]
    fn prb_read_applies_the_same_direction_rule_as_pra() {
        let mut cia = Cia::new(CiaId::B);
        cia.write(reg::DDRB, 0xF0);
        cia.write(reg::PRB, 0xFF);
        cia.prb_input = 0x00;

        assert_eq!(cia.read(reg::PRB), 0xF0, "high nibble output, low input");
    }

    #[test]
    fn writing_pra_always_latches_regardless_of_ddr() {
        // The 8520 latches every PRA write whether or not DDRA currently
        // configures that bit as an output; only readback depends on
        // direction. Configure all-input, write, then flip to
        // all-output and confirm the earlier write is still there.
        let mut cia = Cia::new(CiaId::A);
        cia.write(reg::DDRA, 0x00);
        cia.write(reg::PRA, 0x3C);
        cia.write(reg::DDRA, 0xFF);
        assert_eq!(cia.read(reg::PRA), 0x3C);
    }

    // ---- FloppyDrive: presence and port wiring -------------------------

    #[test]
    fn no_drive_reads_idle_regardless_of_selection() {
        let mut drive = FloppyDrive::new(FloppyPresence::None);
        assert_eq!(drive.pra_status_bits(), FLOPPY_PRA_MASK);

        // Select df0, drop the motor -- a real drive would start
        // answering; nothing does here.
        drive.on_prb_write(0xFF, !CIAB_DSKSEL0 & !CIAB_DSKMOTOR);
        assert_eq!(
            drive.pra_status_bits(),
            FLOPPY_PRA_MASK,
            "no drive never drives the status lines low"
        );
    }

    #[test]
    fn unselected_empty_drive_also_reads_idle() {
        let mut drive = FloppyDrive::new(FloppyPresence::Empty);
        // Never select it.
        drive.on_prb_write(0xFF, !CIAB_DSKMOTOR);
        assert_eq!(drive.pra_status_bits(), FLOPPY_PRA_MASK);
    }

    #[test]
    fn empty_drive_always_asserts_dskchng_and_deasserts_dskprot_when_selected() {
        let mut drive = FloppyDrive::new(FloppyPresence::Empty);
        drive.on_prb_write(0xFF, !CIAB_DSKSEL0);
        let bits = drive.pra_status_bits();
        assert_eq!(bits & CIAA_DSKCHANGE, 0, "DSKCHNG asserted: no disk, ever");
        assert_eq!(
            bits & CIAA_DSKPROT,
            CIAA_DSKPROT,
            "DSKPROT deasserted: nothing to protect"
        );
    }

    // ---- FloppyDrive: stepping and track 0 ------------------------------

    fn select_and_step(drive: &mut FloppyDrive, inward: bool, times: u32) {
        // Base: selected (SEL0 low), DIREC high (outward). Clear DIREC
        // for inward -- DSKDIREC low is what `on_prb_write` reads as
        // "inward".
        let mut step_high = !CIAB_DSKSEL0;
        if inward {
            step_high &= !CIAB_DSKDIREC;
        }
        let step_low = step_high & !CIAB_DSKSTEP;
        for _ in 0..times {
            drive.on_prb_write(step_high, step_low); // falling edge: steps
            drive.on_prb_write(step_low, step_high); // rising edge: no-op
        }
    }

    #[test]
    fn stepping_inward_then_outward_reaches_and_leaves_track0() {
        let mut drive = FloppyDrive::new(FloppyPresence::Empty);
        // Select the drive first so DSKTRACK0 is observable at all.
        drive.on_prb_write(0xFF, !CIAB_DSKSEL0);
        assert_eq!(
            drive.pra_status_bits() & CIAA_DSKTRACK0,
            0,
            "starts at cylinder 0"
        );

        select_and_step(&mut drive, true, 5);
        assert_ne!(
            drive.pra_status_bits() & CIAA_DSKTRACK0,
            0,
            "off track 0 after stepping inward"
        );

        select_and_step(&mut drive, false, 10); // more than enough to reach 0
        assert_eq!(
            drive.pra_status_bits() & CIAA_DSKTRACK0,
            0,
            "back at track 0 after stepping outward past it"
        );
    }

    #[test]
    fn step_pulses_while_deselected_do_not_move_the_head() {
        let mut drive = FloppyDrive::new(FloppyPresence::Empty);
        // Deselected throughout (SEL0 stays high).
        drive.on_prb_write(0xFF, !CIAB_DSKSTEP);
        drive.on_prb_write(!CIAB_DSKSTEP, 0xFF);
        drive.on_prb_write(0xFF, !CIAB_DSKSEL0); // now select
        assert_eq!(
            drive.pra_status_bits() & CIAA_DSKTRACK0,
            0,
            "still at cylinder 0: the earlier pulses never reached a selected drive"
        );
    }

    // ---- FloppyDrive: drive-ID shift sequence ---------------------------

    #[test]
    fn shift_id_bit_reads_msb_first() {
        // A pattern that is not the same read forwards and backwards, so
        // ordering is actually being tested: 1000...0001 with a single
        // extra 1 near the top (0x8000_0001 alone can't distinguish MSB-
        // from LSB-first at the interior bits, so also probe one there).
        let id = 0x8100_0001u32; // bits 31, 24, 0 set
        assert!(shift_id_bit(id, 0), "bit 31 (MSB) first");
        assert!(shift_id_bit(id, 7), "bit 24 next");
        assert!(!shift_id_bit(id, 1));
        assert!(shift_id_bit(id, 31), "bit 0 (LSB) last");
        assert!(!shift_id_bit(id, 32), "past the register: reads 0");
        assert!(!shift_id_bit(id, 255), "stays 0, does not wrap or panic");
    }

    /// Deselect the drive, then reselect it, motor held off (DSKMOTOR
    /// high) throughout -- the two-write cycle a real probe uses to
    /// advance the ID shift register by one bit, per the
    /// `FloppyDrive::on_prb_write` doc comment.
    fn deselect_then_reselect(drive: &mut FloppyDrive) {
        let selected = !CIAB_DSKSEL0;
        let deselected = 0xFFu8;
        drive.on_prb_write(selected, deselected);
        drive.on_prb_write(deselected, selected);
    }

    #[test]
    fn empty_drive_shifts_its_32_bit_id_out_through_dskrdy() {
        let mut drive = FloppyDrive::new(FloppyPresence::Empty);
        // Select with motor commanded on (MOTOR bit low) to spin it up,
        // matching a real probe's opening sequence.
        drive.on_prb_write(0xFF, !CIAB_DSKSEL0 & !CIAB_DSKMOTOR);
        assert!(drive.motor_on);

        // Deselect, set MTR high, reselect: the SELECT falling edge on
        // that reselect is what latches the motor off (MTR read high on
        // both sides of it) and arms ID mode. The reselect edge itself
        // is the hold-first-edge case, so DSKRDY is not yet meaningful;
        // only subsequent deselect/reselect cycles clock bits out.
        drive.on_prb_write(!CIAB_DSKSEL0 & !CIAB_DSKMOTOR, 0xFF); // deselect, MTR high
        drive.on_prb_write(0xFF, !CIAB_DSKSEL0); // reselect, MTR stays high
        assert!(!drive.motor_on, "motor latched off on the reselect edge");

        // Immediately after arming, DSKRDY reflects bit 0 of $FFFFFFFF
        // (asserted -- every bit of an all-ones ID is 1), and stays
        // asserted through the register's 32-bit length. $FFFFFFFF keeps
        // DSKRDY asserted at every valid index, so this proves
        // exhaustion, not per-bit ordering (that is
        // `shift_id_bit_reads_msb_first`, below).
        assert_eq!(drive.pra_status_bits() & CIAA_DSKRDY, 0);

        // 33 deselect/reselect cycles walk the register past its end:
        // the first is absorbed by the arming transition itself (see
        // `on_prb_write`'s doc comment on `id_hold_first_edge`), and the
        // other 32 each clock one more bit of the 32-bit register.
        for _ in 0..32 {
            deselect_then_reselect(&mut drive);
            let bits = drive.pra_status_bits();
            assert_eq!(bits & CIAA_DSKRDY, 0, "still within $FFFFFFFF's 32 bits");
        }

        // One cycle further: past the register's length, exhausted, and
        // must stay that way rather than wrapping back to bit 0.
        deselect_then_reselect(&mut drive);
        assert_ne!(
            drive.pra_status_bits() & CIAA_DSKRDY,
            0,
            "exhausted shift register must not keep reporting ready"
        );
        deselect_then_reselect(&mut drive);
        assert_ne!(
            drive.pra_status_bits() & CIAA_DSKRDY,
            0,
            "stays exhausted, does not wrap"
        );
    }

    #[test]
    fn no_drive_id_is_all_zero() {
        assert_eq!(FloppyPresence::None.drive_id(), 0);
        assert_eq!(FloppyPresence::Empty.drive_id(), 0xFFFF_FFFF);
    }
}
