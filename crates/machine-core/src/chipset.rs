//! Minimal custom-chip register file (`$DFF000`), proposal §7.1.
//!
//! Only the registers the OS spins on, reads for identification, or writes
//! during boot are implemented; everything else falls through to open bus.
//! There is no timing model and no DMA: this is a register file plus a
//! free-running beam counter driven by the frame clock. Registers that a
//! future renderer or blitter will consume (BPL*, COLOR*, COP*LC, SPR0*)
//! are latched here and read back as open bus on real hardware, exactly as
//! the write-only silicon does — Phase 2 reads the public fields directly
//! rather than through `read()`.
//!
//! Phase 1 scope is the "blind boot" subset — enough for exec to start,
//! interrupts to work, and graphics.library to identify the chipset. The
//! software blitter (§7.3) and the stop-gap renderer (§8.1) are Phase 2
//! and are deliberately absent here.

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
    pub const BPL1PTL: u16 = 0x0E2;
    pub const BPL2PTH: u16 = 0x0E4;
    pub const BPL2PTL: u16 = 0x0E6;
    pub const BPL3PTH: u16 = 0x0E8;
    pub const BPL3PTL: u16 = 0x0EA;
    pub const BPL4PTH: u16 = 0x0EC;
    pub const BPL4PTL: u16 = 0x0EE;
    pub const BPL5PTH: u16 = 0x0F0;
    pub const BPL5PTL: u16 = 0x0F2;
    pub const BPL6PTH: u16 = 0x0F4;
    pub const BPL6PTL: u16 = 0x0F6;
    pub const BPLCON0: u16 = 0x100;
    pub const BPLCON1: u16 = 0x102;
    pub const BPLCON2: u16 = 0x104;
    pub const BPL1MOD: u16 = 0x108;
    pub const BPL2MOD: u16 = 0x10A;
    pub const SPR0PTH: u16 = 0x120;
    pub const SPR0PTL: u16 = 0x122;
    pub const SPR0POS: u16 = 0x140;
    pub const SPR0CTL: u16 = 0x142;
    pub const SPR0DATA: u16 = 0x144;
    pub const SPR0DATB: u16 = 0x146;
    pub const COLOR00: u16 = 0x180;
    pub const COLOR31: u16 = 0x1BE;
}

/// Interrupt bit numbers in `INTENA`/`INTREQ` (Amiga hardware numbering).
///
/// This is the hardware's fixed bit-to-source table (Amiga Hardware
/// Reference Manual, "INTENA, INTENAR" chapter) — not a design choice, and
/// it is what [`Chipset::pending_level`] groups into 68k levels 1-6.
pub mod intbit {
    /// Serial transmit buffer empty — 68k level 1.
    pub const TBE: u16 = 0;
    /// Disk block transfer complete — 68k level 1.
    pub const DSKBLK: u16 = 1;
    /// Software-triggered interrupt — 68k level 1.
    pub const SOFT: u16 = 2;
    /// CIA-A / ports — 68k level 2.
    pub const PORTS: u16 = 3;
    /// Copper — 68k level 3.
    pub const COPER: u16 = 4;
    /// Vertical blank — 68k level 3.
    pub const VERTB: u16 = 5;
    /// Blitter done — 68k level 3.
    pub const BLIT: u16 = 6;
    /// Audio channel 0 done — 68k level 4.
    pub const AUD0: u16 = 7;
    /// Audio channel 1 done — 68k level 4.
    pub const AUD1: u16 = 8;
    /// Audio channel 2 done — 68k level 4.
    pub const AUD2: u16 = 9;
    /// Audio channel 3 done — 68k level 4.
    pub const AUD3: u16 = 10;
    /// Serial receive buffer full — 68k level 5.
    pub const RBF: u16 = 11;
    /// Disk sync match — 68k level 5.
    pub const DSKSYN: u16 = 12;
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

/// NTSC 60 Hz alternative: 262 lines, same colour-clock rate per line.
/// Proposal §7.1: "50 Hz default with 60 selectable".
pub const NTSC_LINES_PER_FRAME: u32 = 262;

/// ECS Agnus identification bits reported in the high byte of `VPOSR`.
///
/// graphics.library reads this to decide which chipset it is running on;
/// the machine reports ECS Agnus with 2 MB chip RAM (proposal §6.1), not
/// AGA — AGA is an explicitly contained later extension (§2 non-goals).
pub const VPOSR_AGNUS_ID: u16 = 0x2000;

/// Capacity of the `SERDAT` host debug-channel ring buffer.
///
/// The guest writing to the Amiga serial port is a plausible debug channel
/// for the hosted runner (there is no real serial hardware behind it, so
/// something has to consume the bytes or they are simply lost). This is a
/// fixed-size ring rather than a `Vec` because this crate has no `alloc`;
/// it is deliberately small since it exists for debug output, not bulk
/// transfer, and a full buffer drops new bytes rather than blocking the
/// guest (there is no flow control to give serial.device to make it wait).
const SERIAL_BUF_CAP: usize = 32;

/// Capacity of the `SERDATR` host→guest receive ring buffer.
///
/// Real Paula hardware holds exactly one received word until the guest
/// (or an interrupt handler) drains it via `SERDATR`; overrun is a
/// single-byte-behind condition. This machine has no wire timing at all
/// -- a host might hand over a whole ROMWack command line's worth of
/// bytes in one call (`Chipset::push_serial_in_byte`) before the guest
/// gets a chance to read any of them -- so a small queue rather than a
/// one-byte latch avoids losing input to that timing gap without
/// pretending to model real baud-rate pacing. Same size as
/// [`SERIAL_BUF_CAP`] for the same reason: generous for line-based debug
/// traffic, not sized for bulk transfer.
const SERIAL_IN_BUF_CAP: usize = 32;

/// How far the beam moved during one [`Chipset::tick`].
///
/// Both figures drive the CIAs' TOD counters, which the OS uses as its
/// wall clock: CIA-A counts frames (vertical blank), CIA-B counts raster
/// lines. Reporting them here keeps one frame clock as the single source
/// of time for the whole machine (§7.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BeamAdvance {
    /// Raster lines started during this tick.
    pub lines_started: u32,
    /// Whether a frame boundary was crossed (VERTB was raised).
    pub frame_wrapped: bool,
}

/// Which mouse button a host input event refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    /// Pin 6 (FIR0) — CIA-A PRA bit 6, not chipset. See
    /// [`Chipset::mouse_button`].
    Left,
    /// Pin 9 (POT0Y) — `POTGOR` bit 10, active low.
    Right,
    /// Pin 5 (POT0X) — `POTGOR` bit 8, active low. Only present on
    /// 3-button mice; most 2-button mice leave this pin unconnected.
    Middle,
}

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
    /// Lines per frame the beam counter wraps at: [`PAL_LINES_PER_FRAME`]
    /// by default, [`NTSC_LINES_PER_FRAME`] after [`Chipset::set_ntsc`].
    lines_per_frame: u32,
    /// Long/short field toggle (`VPOSR` bit 15), flips every frame. Some
    /// ECS-aware software checks this for interlace/genlock framing even
    /// when not itself driving interlace.
    lof: bool,

    /// Mouse/joystick position registers, fed from host input. Format is
    /// the hardware's: high byte is a free-running vertical counter, low
    /// byte a free-running horizontal counter, each wrapping mod 256 —
    /// gameport.device computes deltas by diffing successive reads, it
    /// never reads these as absolute position.
    pub joy0dat: u16,
    pub joy1dat: u16,
    /// Potentiometer register; mouse buttons read back through `POTGOR`.
    pub potgor: u16,
    /// `POTGO` write latch (pot pin direction/start-counting control).
    pub potgo: u16,

    /// Copper pointer latches (`COP1LC`/`COP2LC`), high/low pairs forming
    /// 32-bit chip RAM addresses. No copper runs in Phase 1; Phase 2's
    /// renderer walks the list these point at (§8.1).
    pub cop1lc: u32,
    pub cop2lc: u32,
    /// Whether `COPJMP1`/`COPJMP2` has been strobed since reset. These are
    /// strobe registers on real hardware (any write, value ignored,
    /// restarts the copper at COP1LC/COP2LC); Phase 1 has no copper to
    /// restart, so this just latches that the strobe happened.
    pub copjmp1_hit: bool,
    pub copjmp2_hit: bool,

    /// Bitplane pointers, `BPL1PT`-`BPL6PT`, high/low pairs.
    pub bplpt: [u32; 6],
    pub bplcon0: u16,
    pub bplcon1: u16,
    pub bplcon2: u16,
    pub bpl1mod: u16,
    pub bpl2mod: u16,
    pub diwstrt: u16,
    pub diwstop: u16,
    pub ddfstrt: u16,
    pub ddfstop: u16,
    /// `COLOR00`-`COLOR31`, masked to the 12-bit `$0RGB` the OCS/ECS
    /// palette hardware actually holds (AGA's 8-bit-per-gun extension is
    /// out of scope, proposal §2).
    pub color: [u16; 32],

    /// Sprite 0's registers — the software mouse pointer comes from
    /// sprite 0 per proposal §8.1, so it is the one sprite Phase 1 latches.
    pub spr0pt: u32,
    pub spr0pos: u16,
    pub spr0ctl: u16,
    pub spr0data: u16,
    pub spr0datb: u16,

    /// Disk sink (§7.1): latched but never acted on, so trackdisk.device
    /// idles instead of spinning on real DMA that will never complete.
    pub dskpt: u32,
    pub dsklen: u16,

    /// Serial sink (§7.1): latched but never transmitted on real hardware
    /// terms. `serdat` holds the last word written for inspection.
    pub serdat: u16,
    /// `SERPER` write latch. There is no baud-rate/bit-timing model in
    /// this machine at all -- `SERDAT`/`SERDATR` transfers complete the
    /// instant they're written/read (see `write`'s `SERDAT` arm and
    /// [`Chipset::read_serdatr`]), so there is no shift clock for this
    /// value to actually drive. It is kept purely so a guest that reads
    /// it back (or a future cycle-accurate mode) sees what it wrote; the
    /// documented 9600-baud `SERPER` value (372, `AHRM`) that a ROMWack
    /// host-side client computes never appears anywhere else in this
    /// file, which is why this comment exists.
    pub serper: u16,
    /// Host debug-channel ring buffer fed by `SERDAT` writes, drained by
    /// [`Chipset::take_serial_byte`]. See [`SERIAL_BUF_CAP`].
    serial_buf: [u8; SERIAL_BUF_CAP],
    serial_head: usize,
    serial_len: usize,

    /// Host→guest receive ring buffer for `SERDATR`, fed by
    /// [`Chipset::push_serial_in_byte`] and drained one byte per read by
    /// [`Chipset::read_serdatr`]. See [`SERIAL_IN_BUF_CAP`].
    serial_in_buf: [u8; SERIAL_IN_BUF_CAP],
    serial_in_head: usize,
    serial_in_len: usize,
    /// `SERDATR` bit 15 (OVRUN) latch: set when a host byte arrives while
    /// the receive queue is already full and has to be dropped. Cleared
    /// the next time the guest reads a byte via `SERDATR` -- see
    /// [`Chipset::read_serdatr`].
    serial_in_overrun: bool,
}

impl Chipset {
    pub fn new() -> Self {
        Self {
            // POTGOR reads all-ones when no pot inputs are pulled low,
            // which is what "no buttons pressed" looks like to the OS.
            potgor: 0xFFFF,
            lines_per_frame: PAL_LINES_PER_FRAME,
            ..Default::default()
        }
    }

    /// Switch the frame clock to NTSC (60 Hz, 262 lines). PAL is the
    /// default; proposal §7.1 asks for 60 Hz to be selectable.
    pub fn set_ntsc(&mut self, ntsc: bool) {
        self.lines_per_frame = if ntsc {
            NTSC_LINES_PER_FRAME
        } else {
            PAL_LINES_PER_FRAME
        };
    }

    /// Feed a host mouse motion sample into `JOY0DAT`.
    ///
    /// Real hardware counts quadrature pulses into free-running 8-bit
    /// counters per axis (high byte Y, low byte X) that wrap silently;
    /// gameport.device recovers a delta by subtracting the previous
    /// reading, so wrapping `wrapping_add` here is exactly what the
    /// hardware does, not an approximation of it.
    pub fn mouse_delta(&mut self, dx: i8, dy: i8) {
        let x = (self.joy0dat & 0x00FF) as u8;
        let y = (self.joy0dat >> 8) as u8;
        let x = x.wrapping_add(dx as u8);
        let y = y.wrapping_add(dy as u8);
        self.joy0dat = ((y as u16) << 8) | x as u16;
    }

    /// Feed a host mouse button transition in.
    ///
    /// Only the right and middle buttons are chipset business (`POTGOR`
    /// bits 10 and 8, port 0, active low). The left/primary button is pin
    /// 6 (FIR0), wired to **CIA-A PRA bit 6**, not to any custom register
    /// — `Chipset::mouse_button(Left, ...)` is a deliberate no-op, and the
    /// CIA-A model must set/clear PRA bit 6 itself from the same host
    /// input.
    pub fn mouse_button(&mut self, button: MouseButton, pressed: bool) {
        let bit = match button {
            MouseButton::Left => return,
            MouseButton::Right => 10,
            MouseButton::Middle => 8,
        };
        if pressed {
            self.potgor &= !(1 << bit);
        } else {
            self.potgor |= 1 << bit;
        }
    }

    /// Take the next byte the guest wrote to `SERDAT`, oldest first, or
    /// `None` if the debug channel is empty.
    pub fn take_serial_byte(&mut self) -> Option<u8> {
        if self.serial_len == 0 {
            return None;
        }
        let byte = self.serial_buf[self.serial_head];
        self.serial_head = (self.serial_head + 1) % SERIAL_BUF_CAP;
        self.serial_len -= 1;
        Some(byte)
    }

    fn push_serial_byte(&mut self, byte: u8) {
        if self.serial_len < SERIAL_BUF_CAP {
            let idx = (self.serial_head + self.serial_len) % SERIAL_BUF_CAP;
            self.serial_buf[idx] = byte;
            self.serial_len += 1;
        }
        // Full: drop the byte. This is a best-effort debug channel with no
        // flow control to push back on the guest with, matching how a
        // real, unconnected serial port would simply lose data too.
    }

    /// Hand one byte from the host to the guest's serial receiver.
    ///
    /// This is the receive counterpart of [`Chipset::take_serial_byte`]:
    /// the host (`machine-hosted`'s `--serial-in`, or an injected DEL for
    /// the ROMWack break-in) calls this, and the byte becomes visible to
    /// the guest through `SERDATR` (see [`Chipset::read_serdatr`]).
    ///
    /// Raises the `RBF` interrupt (`intbit::RBF`, 68k level 5, AHRM's
    /// `INTENA`/`INTREQ` chapter) on every accepted byte, not just when
    /// the queue was empty -- real Paula raises RBF once per received
    /// word, and a host that hands over several bytes in one call (e.g.
    /// a whole command line) must not collapse that into a single
    /// interrupt or an interrupt-driven `serial.device` would stop
    /// after the first byte. This is also why the source polling
    /// `SERDATR` directly (Kickstart's crash-loop DEL check, AHRM) and
    /// an interrupt-driven reader both work off the same queue: the
    /// interrupt is a convenience for the latter, not the only path in.
    ///
    /// A full queue sets `OVRUN` and drops the byte -- there is no flow
    /// control to push back on the host with, exactly like
    /// [`Chipset::push_serial_byte`]'s transmit-side sibling.
    pub fn push_serial_in_byte(&mut self, byte: u8) {
        if self.serial_in_len < SERIAL_IN_BUF_CAP {
            let idx = (self.serial_in_head + self.serial_in_len) % SERIAL_IN_BUF_CAP;
            self.serial_in_buf[idx] = byte;
            self.serial_in_len += 1;
            self.raise_int(intbit::RBF);
        } else {
            self.serial_in_overrun = true;
        }
    }

    /// Whether [`Chipset::push_serial_in_byte`] currently has room without
    /// dropping a byte and latching `OVRUN`. A host-side pacer that wants
    /// to hand over more bytes than the queue holds at once (e.g.
    /// `machine-hosted`'s scripted serial input, one command line at a
    /// time) can poll this to throttle itself, rather than leaning on the
    /// drop-and-flag overrun path meant for a genuine host/guest speed
    /// mismatch.
    pub fn serial_in_has_room(&self) -> bool {
        self.serial_in_len < SERIAL_IN_BUF_CAP
    }

    /// Take the oldest queued receive byte, if any. Private: the only
    /// caller is [`Chipset::read_serdatr`], which is where consuming a
    /// byte and presenting `RBF` have to happen atomically together.
    fn pop_serial_in_byte(&mut self) -> Option<u8> {
        if self.serial_in_len == 0 {
            return None;
        }
        let byte = self.serial_in_buf[self.serial_in_head];
        self.serial_in_head = (self.serial_in_head + 1) % SERIAL_IN_BUF_CAP;
        self.serial_in_len -= 1;
        Some(byte)
    }

    /// Read a register. `offset` is the offset within `$DFF000`, already
    /// masked to the register window by the caller.
    ///
    /// Unimplemented registers, and write-only registers (the latches
    /// Phase 2 will consume — copper, bitplane, colour, sprite state),
    /// return `$FFFF` (open bus), matching real hardware: none of those
    /// registers have a read path at all.
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
            // No disk hardware: report "idle, nothing to do" so
            // trackdisk.device settles instead of spinning.
            reg::SERDATR => self.read_serdatr(),
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

            reg::COP1LCH => set_ptr_hi(&mut self.cop1lc, value),
            reg::COP1LCL => set_ptr_lo(&mut self.cop1lc, value),
            reg::COP2LCH => set_ptr_hi(&mut self.cop2lc, value),
            reg::COP2LCL => set_ptr_lo(&mut self.cop2lc, value),
            // Strobes: on real hardware any write here (the value is
            // ignored) restarts the copper at COP1LC/COP2LC. Phase 1 has
            // no copper to restart, so just latch that the strobe fired.
            reg::COPJMP1 => self.copjmp1_hit = true,
            reg::COPJMP2 => self.copjmp2_hit = true,

            reg::BPL1PTH => set_ptr_hi(&mut self.bplpt[0], value),
            reg::BPL1PTL => set_ptr_lo(&mut self.bplpt[0], value),
            reg::BPL2PTH => set_ptr_hi(&mut self.bplpt[1], value),
            reg::BPL2PTL => set_ptr_lo(&mut self.bplpt[1], value),
            reg::BPL3PTH => set_ptr_hi(&mut self.bplpt[2], value),
            reg::BPL3PTL => set_ptr_lo(&mut self.bplpt[2], value),
            reg::BPL4PTH => set_ptr_hi(&mut self.bplpt[3], value),
            reg::BPL4PTL => set_ptr_lo(&mut self.bplpt[3], value),
            reg::BPL5PTH => set_ptr_hi(&mut self.bplpt[4], value),
            reg::BPL5PTL => set_ptr_lo(&mut self.bplpt[4], value),
            reg::BPL6PTH => set_ptr_hi(&mut self.bplpt[5], value),
            reg::BPL6PTL => set_ptr_lo(&mut self.bplpt[5], value),

            reg::BPLCON0 => self.bplcon0 = value,
            reg::BPLCON1 => self.bplcon1 = value,
            reg::BPLCON2 => self.bplcon2 = value,
            reg::BPL1MOD => self.bpl1mod = value,
            reg::BPL2MOD => self.bpl2mod = value,
            reg::DIWSTRT => self.diwstrt = value,
            reg::DIWSTOP => self.diwstop = value,
            reg::DDFSTRT => self.ddfstrt = value,
            reg::DDFSTOP => self.ddfstop = value,

            // COLOR00-COLOR31 are one register every 2 bytes; 12-bit RGB
            // is all OCS/ECS colour hardware holds.
            reg::COLOR00..=reg::COLOR31 => {
                let index = ((offset - reg::COLOR00) / 2) as usize;
                self.color[index] = value & 0x0FFF;
            }

            reg::SPR0PTH => set_ptr_hi(&mut self.spr0pt, value),
            reg::SPR0PTL => set_ptr_lo(&mut self.spr0pt, value),
            reg::SPR0POS => self.spr0pos = value,
            reg::SPR0CTL => self.spr0ctl = value,
            reg::SPR0DATA => self.spr0data = value,
            reg::SPR0DATB => self.spr0datb = value,

            reg::DSKPTH => set_ptr_hi(&mut self.dskpt, value),
            reg::DSKPTL => set_ptr_lo(&mut self.dskpt, value),
            // No disk DMA exists to start; this only ever latches length.
            reg::DSKLEN => self.dsklen = value,

            // A real UART would take many CPU cycles to shift a byte out;
            // there's no UART here; the write "completes" the instant it
            // lands so serial.device's IO_QUICK path never has to wait,
            // and (optionally) the byte is offered to the host as a debug
            // stream instead of being lost.
            reg::SERDAT => {
                self.serdat = value;
                self.push_serial_byte(value as u8);
            }
            reg::SERPER => self.serper = value,
            reg::POTGO => self.potgo = value,

            _ => {}
        }
    }

    /// `VPOSR`: long-frame toggle in bit 15, Agnus ID in the high byte,
    /// bit 0 is vertical position bit 8.
    fn vposr(&self) -> u16 {
        let lof = if self.lof { 1 << 15 } else { 0 };
        lof | VPOSR_AGNUS_ID | ((self.vpos >> 8) & 1) as u16
    }

    /// `VHPOSR`: vertical position low byte, horizontal position low byte.
    fn vhposr(&self) -> u16 {
        (((self.vpos & 0xFF) << 8) | (self.hpos & 0xFF)) as u16
    }

    /// `SERDATR`: `OVRUN` (bit 15), `RBF` (bit 14), `TBE` (bit 13), `TSRE`
    /// (bit 12), `RXD` (bit 11), received data in bits 8-0 (the AHRM's
    /// `SERDATR` chapter; layout cross-checked against Copperline's
    /// `chipset/paula.rs::read_serdatr`, the project's chipset oracle,
    /// which implements the identical bit-for-bit meaning).
    ///
    /// `TBE`/`TSRE` are unconditionally set: a `SERDAT` write "completes"
    /// the instant it lands (see `write`'s `SERDAT` arm) since there is
    /// no shift-register timing model here at all, so neither transmit
    /// stage is ever busy from the guest's point of view. `RXD` is the
    /// raw, two-stage-synchronised input pin, which real hardware can
    /// read as low mid-byte independently of `RBF`; with no bit timing to
    /// derive that from, it is reported high (idle mark state, RS-232's
    /// resting level) rather than fabricate a framing signal that
    /// doesn't exist here -- the same "don't pretend to a fidelity this
    /// machine doesn't model" call this file already makes for `SERPER`.
    ///
    /// Reading this register consumes the oldest queued receive byte
    /// (if any) and reports `RBF` only for that read. Real hardware
    /// instead clears `RBF`/`OVRUN` on the matching `INTREQ` write and
    /// leaves `SERDATR`'s data latched until the next word overwrites it
    /// (Copperline models that precisely because AROS's level-5
    /// dispatcher acks `INTREQ` *before* reading `SERDATR`). This
    /// machine has no interrupt-ack-vs-register-read distinction to
    /// reproduce that against -- `write`'s `INTREQ` arm just clears the
    /// bit, it doesn't know it's "the RBF ack" specifically -- so
    /// read-consumes is the simpler contract that still gives both a
    /// polling reader (Kickstart's crash-loop DEL check) and an
    /// interrupt-driven `serial.device` a genuine byte per read.
    fn read_serdatr(&mut self) -> u16 {
        // TBE, TSRE, RXD: always idle/high, per this method's doc comment.
        let mut v: u16 = (1 << 13) | (1 << 12) | (1 << 11);
        if let Some(byte) = self.pop_serial_in_byte() {
            v |= 1 << 14; // RBF
            v |= byte as u16;
            if self.serial_in_overrun {
                v |= 1 << 15; // OVRUN
            }
        }
        // OVRUN is reported at most once: it acknowledges itself the
        // same read that hands back a byte, matching "the guest caught
        // up with the receiver" rather than requiring a second,
        // separate acknowledgement this model has no register for.
        self.serial_in_overrun = false;
        v
    }

    /// Raise an interrupt source (used by the CIAs and, later, by cards).
    pub fn raise_int(&mut self, bit: u16) {
        self.intreq |= 1 << bit;
    }

    /// The 68k interrupt level the chipset is currently requesting, 0 for
    /// none. Levels follow the hardware's fixed grouping of INTREQ bits
    /// into IPL 1-6 (Amiga Hardware Reference Manual): level 1 is
    /// TBE/DSKBLK/SOFT (bits 0-2), level 2 is PORTS alone (bit 3), level 3
    /// is COPER/VERTB/BLIT (bits 4-6), level 4 is AUD0-3 (bits 7-10),
    /// level 5 is RBF/DSKSYN (bits 11-12), level 6 is EXTER alone (bit
    /// 13). Bit 14 (INTEN) is the master enable, not a source, and is
    /// handled separately below.
    pub fn pending_level(&self) -> u8 {
        if self.intena & (1 << intbit::INTEN) == 0 {
            return 0;
        }
        let active = self.intena & self.intreq & 0x3FFF;
        if active == 0 {
            return 0;
        }
        // Highest set bit wins: a real Amiga presents one combined IPL to
        // the 68k, and the CPU takes whichever level is currently
        // asserted highest, same as this table.
        let highest = 15 - active.leading_zeros() as u16;
        match highest {
            0..=2 => 1,
            3 => 2,
            4..=6 => 3,
            7..=10 => 4,
            11..=12 => 5,
            13 => 6,
            _ => 0,
        }
    }

    /// Advance the frame clock by `cpu_clocks` CPU cycles.
    ///
    /// Returns `true` when a frame boundary was crossed, which the caller
    /// turns into VERTB. One clock feeds the beam counter, VERTB and (from
    /// Phase 2) the renderer, so `WaitTOF`, VBlank servers and copper
    /// positions all agree — proposal §7.1.
    pub fn tick(&mut self, cpu_clocks: u32, cpu_clocks_per_colour_clock: u32) -> BeamAdvance {
        let total = self.carry + cpu_clocks;
        let colour_clocks = total / cpu_clocks_per_colour_clock;
        self.carry = total % cpu_clocks_per_colour_clock;

        let mut advance = BeamAdvance::default();
        self.hpos += colour_clocks;
        while self.hpos >= PAL_COLOUR_CLOCKS_PER_LINE {
            self.hpos -= PAL_COLOUR_CLOCKS_PER_LINE;
            self.vpos += 1;
            advance.lines_started += 1;
            if self.vpos >= self.lines_per_frame {
                self.vpos = 0;
                self.frames += 1;
                self.lof = !self.lof;
                advance.frame_wrapped = true;
                self.raise_int(intbit::VERTB);
            }
        }
        advance
    }
}

/// Latch the high 16 bits of a 32-bit pointer register, keeping the low
/// half. Shared by every `xxxPTH`/`COPxLCH` write (§7.1's pointer pairs).
fn set_ptr_hi(ptr: &mut u32, value: u16) {
    *ptr = (*ptr & 0x0000_FFFF) | ((value as u32) << 16);
}

/// Latch the low 16 bits of a 32-bit pointer register, keeping the high
/// half.
fn set_ptr_lo(ptr: &mut u32, value: u16) {
    *ptr = (*ptr & 0xFFFF_0000) | value as u32;
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
    fn setclr_applies_to_intreq_dmacon_adkcon() {
        type RegAndReader = (u16, fn(&Chipset) -> u16);
        let cases: [RegAndReader; 3] = [
            (reg::INTREQ, |c: &Chipset| c.intreq),
            (reg::DMACON, |c: &Chipset| c.dmacon),
            (reg::ADKCON, |c: &Chipset| c.adkcon),
        ];
        for (write_reg, read_val) in cases {
            let mut c = Chipset::new();
            c.write(write_reg, 0x8000 | 0x0003);
            assert_eq!(read_val(&c), 0x0003, "set via {write_reg:#x}");
            c.write(write_reg, 0x0001);
            assert_eq!(read_val(&c), 0x0002, "clear via {write_reg:#x}");
        }
    }

    #[test]
    fn vposr_reports_ecs_agnus() {
        let mut c = Chipset::new();
        assert_eq!(c.read(reg::VPOSR) & 0x3F00, VPOSR_AGNUS_ID);
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
    fn intenar_and_intreqr_read_back_what_was_written() {
        let mut c = Chipset::new();
        c.write(
            reg::INTENA,
            0x8000 | (1 << intbit::INTEN) | (1 << intbit::PORTS),
        );
        c.write(reg::INTREQ, 0x8000 | (1 << intbit::PORTS));
        assert_eq!(
            c.read(reg::INTENAR),
            (1 << intbit::INTEN) | (1 << intbit::PORTS)
        );
        assert_eq!(c.read(reg::INTREQR), 1 << intbit::PORTS);
    }

    /// Every interrupt source, driven alone, must land on the hardware's
    /// documented 68k level. This is the table proposal-review flagged as
    /// needing bit-by-bit verification against real hardware.
    #[test]
    fn every_interrupt_source_maps_to_its_hardware_level() {
        let cases = [
            (intbit::TBE, 1),
            (intbit::DSKBLK, 1),
            (intbit::SOFT, 1),
            (intbit::PORTS, 2),
            (intbit::COPER, 3),
            (intbit::VERTB, 3),
            (intbit::BLIT, 3),
            (intbit::AUD0, 4),
            (intbit::AUD1, 4),
            (intbit::AUD2, 4),
            (intbit::AUD3, 4),
            (intbit::RBF, 5),
            (intbit::DSKSYN, 5),
            (intbit::EXTER, 6),
        ];
        for (bit, expected_level) in cases {
            let mut c = Chipset::new();
            c.write(reg::INTENA, 0x8000 | (1 << intbit::INTEN) | (1 << bit));
            c.raise_int(bit);
            assert_eq!(c.pending_level(), expected_level, "bit {bit} level");
        }
    }

    #[test]
    fn higher_level_source_wins_when_multiple_are_pending() {
        let mut c = Chipset::new();
        c.write(
            reg::INTENA,
            0x8000 | (1 << intbit::INTEN) | (1 << intbit::TBE) | (1 << intbit::EXTER),
        );
        c.raise_int(intbit::TBE);
        c.raise_int(intbit::EXTER);
        assert_eq!(c.pending_level(), 6);
    }

    #[test]
    fn masked_source_does_not_contribute_a_level() {
        let mut c = Chipset::new();
        c.write(reg::INTENA, 0x8000 | (1 << intbit::INTEN));
        c.raise_int(intbit::VERTB);
        assert_eq!(c.pending_level(), 0, "INTENA bit for VERTB is clear");
    }

    #[test]
    fn unimplemented_registers_read_open_bus() {
        let mut c = Chipset::new();
        // $DFF0F8 sits in real, unused reserved space between the last
        // bitplane pointer (BPL6PTL, $DFF0F6) and BPLCON0 ($DFF100).
        assert_eq!(c.read(0x0F8), 0xFFFF);
    }

    #[test]
    fn write_only_latches_read_back_as_open_bus() {
        let mut c = Chipset::new();
        c.write(reg::BPLCON0, 0x1234);
        c.write(reg::COLOR00, 0x0F0F);
        c.write(reg::COP1LCH, 0xABCD);
        assert_eq!(c.bplcon0, 0x1234, "latched for Phase 2");
        assert_eq!(c.color[0], 0x0F0F & 0x0FFF, "latched for Phase 2");
        assert_eq!(c.read(reg::BPLCON0), 0xFFFF, "no read path on hardware");
        assert_eq!(c.read(reg::COLOR00), 0xFFFF, "no read path on hardware");
    }

    #[test]
    fn cop1lc_cop2lc_latch_as_32_bit_pointers() {
        let mut c = Chipset::new();
        c.write(reg::COP1LCH, 0x0012);
        c.write(reg::COP1LCL, 0x3450);
        assert_eq!(c.cop1lc, 0x0012_3450);

        c.write(reg::COP2LCH, 0x00AB);
        c.write(reg::COP2LCL, 0xCDE0);
        assert_eq!(c.cop2lc, 0x00AB_CDE0);

        // Rewriting only the low half must not disturb the high half.
        c.write(reg::COP1LCL, 0x0000);
        assert_eq!(c.cop1lc, 0x0012_0000);
    }

    #[test]
    fn copjmp_strobes_latch_that_they_fired() {
        let mut c = Chipset::new();
        assert!(!c.copjmp1_hit);
        assert!(!c.copjmp2_hit);
        c.write(reg::COPJMP1, 0); // value is irrelevant; the write is the strobe
        c.write(reg::COPJMP2, 0xFFFF);
        assert!(c.copjmp1_hit);
        assert!(c.copjmp2_hit);
    }

    #[test]
    fn all_six_bitplane_pointers_latch_independently() {
        let mut c = Chipset::new();
        let pairs = [
            (reg::BPL1PTH, reg::BPL1PTL),
            (reg::BPL2PTH, reg::BPL2PTL),
            (reg::BPL3PTH, reg::BPL3PTL),
            (reg::BPL4PTH, reg::BPL4PTL),
            (reg::BPL5PTH, reg::BPL5PTL),
            (reg::BPL6PTH, reg::BPL6PTL),
        ];
        for (i, (hi, lo)) in pairs.iter().enumerate() {
            c.write(*hi, 0x0001 + i as u16);
            c.write(*lo, 0x2000);
        }
        for (i, expected_hi) in (0x0001u32..=0x0006).enumerate() {
            assert_eq!(c.bplpt[i], (expected_hi << 16) | 0x2000, "plane {i}");
        }
    }

    #[test]
    fn colour_palette_round_trips_all_32_entries() {
        let mut c = Chipset::new();
        for i in 0..32u16 {
            c.write(reg::COLOR00 + i * 2, 0x0F00 | i);
        }
        for i in 0..32usize {
            assert_eq!(c.color[i], 0x0F00 | i as u16);
        }
    }

    #[test]
    fn sprite0_registers_latch() {
        let mut c = Chipset::new();
        c.write(reg::SPR0PTH, 0x0001);
        c.write(reg::SPR0PTL, 0x8000);
        c.write(reg::SPR0POS, 0x1122);
        c.write(reg::SPR0CTL, 0x3344);
        c.write(reg::SPR0DATA, 0x5566);
        c.write(reg::SPR0DATB, 0x7788);
        assert_eq!(c.spr0pt, 0x0001_8000);
        assert_eq!(c.spr0pos, 0x1122);
        assert_eq!(c.spr0ctl, 0x3344);
        assert_eq!(c.spr0data, 0x5566);
        assert_eq!(c.spr0datb, 0x7788);
    }

    #[test]
    fn disk_writes_latch_but_start_no_dma() {
        let mut c = Chipset::new();
        c.write(reg::DSKPTH, 0x0001);
        c.write(reg::DSKPTL, 0x0000);
        c.write(reg::DSKLEN, 0x8000 | 100);
        assert_eq!(c.dskpt, 0x0001_0000);
        assert_eq!(c.dsklen, 0x8000 | 100);
        // DSKBYTR still reports idle: nothing was ever "started".
        assert_eq!(c.read(reg::DSKBYTR), 0x0000);
    }

    #[test]
    fn serdatr_reports_transmit_buffer_empty() {
        let mut c = Chipset::new();
        assert_eq!(c.read(reg::SERDATR) & 0x2000, 0x2000);
        c.write(reg::SERDAT, 0x41);
        // A write completes instantly (no UART to wait for); TBE stays set.
        assert_eq!(c.read(reg::SERDATR) & 0x2000, 0x2000);
    }

    #[test]
    fn serdat_writes_are_offered_to_the_host_as_a_byte_stream() {
        let mut c = Chipset::new();
        assert_eq!(c.take_serial_byte(), None);
        c.write(reg::SERDAT, 0x41);
        c.write(reg::SERDAT, 0x42);
        assert_eq!(c.take_serial_byte(), Some(0x41));
        assert_eq!(c.take_serial_byte(), Some(0x42));
        assert_eq!(c.take_serial_byte(), None);
    }

    #[test]
    fn serial_debug_channel_drops_bytes_once_full_rather_than_blocking() {
        let mut c = Chipset::new();
        for i in 0..SERIAL_BUF_CAP + 10 {
            c.write(reg::SERDAT, i as u16);
        }
        let mut count = 0;
        while c.take_serial_byte().is_some() {
            count += 1;
        }
        assert_eq!(count, SERIAL_BUF_CAP, "buffer caps rather than growing");
    }

    #[test]
    fn serdatr_idle_with_no_input_matches_pre_receive_behaviour() {
        // Pins the exact "no regression" value: TBE/TSRE/RXD idle-high,
        // nothing else set, same as the pre-receive-path `0x2000` (masked)
        // this replaced -- serial.device's transmit-side idle check must
        // not start seeing anything new.
        let mut c = Chipset::new();
        assert_eq!(c.read(reg::SERDATR), (1 << 13) | (1 << 12) | (1 << 11));
        assert_eq!(c.read(reg::SERDATR) & 0x2000, 0x2000, "TBE still set");
    }

    #[test]
    fn serdatr_delivers_a_queued_host_byte_and_clears_rbf_after() {
        let mut c = Chipset::new();
        c.push_serial_in_byte(0x7F); // DEL -- the ROMWack break-in byte
        let v = c.read(reg::SERDATR);
        assert_eq!(v & (1 << 14), 1 << 14, "RBF set with a byte queued");
        assert_eq!(v & 0x01FF, 0x7F, "data in the low 9 bits");
        // Consumed: the next read finds the queue empty and RBF clear.
        let v = c.read(reg::SERDATR);
        assert_eq!(v & (1 << 14), 0, "RBF clears once the byte is read");
    }

    #[test]
    fn serdatr_delivers_multiple_queued_bytes_oldest_first() {
        let mut c = Chipset::new();
        c.push_serial_in_byte(b'a');
        c.push_serial_in_byte(b'b');
        assert_eq!(c.read(reg::SERDATR) & 0xFF, b'a' as u16);
        assert_eq!(c.read(reg::SERDATR) & 0xFF, b'b' as u16);
        assert_eq!(c.read(reg::SERDATR) & (1 << 14), 0, "queue now empty");
    }

    #[test]
    fn serdatr_reports_overrun_once_when_the_receive_queue_is_full() {
        let mut c = Chipset::new();
        for i in 0..SERIAL_IN_BUF_CAP + 5 {
            c.push_serial_in_byte(i as u8);
        }
        // Every queued byte reads back; the last 5 pushes were dropped and
        // set OVRUN, which shows up on the very next read...
        let mut overrun_seen = false;
        let mut count = 0;
        loop {
            let v = c.read(reg::SERDATR);
            if v & (1 << 14) == 0 {
                break;
            }
            if v & (1 << 15) != 0 {
                overrun_seen = true;
            }
            count += 1;
        }
        assert_eq!(count, SERIAL_IN_BUF_CAP, "queue caps rather than growing");
        assert!(overrun_seen, "a dropped byte must set OVRUN");
        // ...and OVRUN does not linger once acknowledged.
        assert_eq!(
            c.read(reg::SERDATR) & (1 << 15),
            0,
            "OVRUN clears once reported"
        );
    }

    #[test]
    fn push_serial_in_byte_raises_rbf_interrupt() {
        let mut c = Chipset::new();
        c.write(
            reg::INTENA,
            0x8000 | (1 << intbit::INTEN) | (1 << intbit::RBF),
        );
        assert_eq!(c.pending_level(), 0, "nothing received yet");
        c.push_serial_in_byte(b'_'); // ROMWack's handshake prompt byte
        assert_eq!(c.pending_level(), 5, "RBF is level 5");
        assert_eq!(c.read(reg::INTREQR) & (1 << intbit::RBF), 1 << intbit::RBF);
    }

    #[test]
    fn serial_in_has_room_reflects_queue_occupancy() {
        let mut c = Chipset::new();
        assert!(c.serial_in_has_room());
        for i in 0..SERIAL_IN_BUF_CAP {
            assert!(c.serial_in_has_room(), "room before byte {i}");
            c.push_serial_in_byte(i as u8);
        }
        assert!(!c.serial_in_has_room(), "queue is now full");
        c.read(reg::SERDATR); // drain one byte
        assert!(c.serial_in_has_room());
    }

    #[test]
    fn push_serial_in_byte_raises_rbf_once_per_byte_not_once_per_batch() {
        // An interrupt-driven serial.device must see one RBF-serviceable
        // event per received byte, not a single edge for a whole burst --
        // otherwise a host handing over several bytes at once (a whole
        // ROMWack command line) would only ever wake the driver for the
        // first one.
        let mut c = Chipset::new();
        c.write(
            reg::INTENA,
            0x8000 | (1 << intbit::INTEN) | (1 << intbit::RBF),
        );
        c.push_serial_in_byte(b'a');
        c.write(reg::INTREQ, 1 << intbit::RBF); // ack, as a real handler would
        assert_eq!(c.pending_level(), 0);
        c.push_serial_in_byte(b'b');
        assert_eq!(c.pending_level(), 5, "second byte re-asserts RBF");
    }

    #[test]
    fn mouse_delta_updates_joy0dat_in_hardware_byte_order() {
        let mut c = Chipset::new();
        c.mouse_delta(5, -3);
        let x = (c.joy0dat & 0xFF) as u8;
        let y = (c.joy0dat >> 8) as u8;
        assert_eq!(x, 5);
        assert_eq!(y, (-3i8) as u8);
    }

    #[test]
    fn mouse_delta_counters_wrap_like_hardware() {
        let mut c = Chipset::new();
        c.joy0dat = 0x00FE; // X counter near wraparound
        c.mouse_delta(5, 0);
        assert_eq!(c.joy0dat & 0xFF, 3, "0xFE + 5 wraps to 3 mod 256");
    }

    #[test]
    fn right_and_middle_mouse_buttons_clear_potgor_bits_active_low() {
        let mut c = Chipset::new();
        assert_eq!(c.potgor, 0xFFFF, "nothing pressed at reset");

        c.mouse_button(MouseButton::Right, true);
        assert_eq!(c.potgor & (1 << 10), 0, "right button is POTGOR bit 10");
        c.mouse_button(MouseButton::Right, false);
        assert_ne!(c.potgor & (1 << 10), 0);

        c.mouse_button(MouseButton::Middle, true);
        assert_eq!(c.potgor & (1 << 8), 0, "middle button is POTGOR bit 8");
    }

    #[test]
    fn left_mouse_button_is_not_chipset_business() {
        // Left/primary button is CIA-A PRA bit 6 on real hardware; the
        // chipset must not touch POTGOR (or anything else) for it.
        let mut c = Chipset::new();
        let before = c.potgor;
        c.mouse_button(MouseButton::Left, true);
        assert_eq!(c.potgor, before);
    }

    #[test]
    fn vhposr_composes_from_beam_position() {
        let mut c = Chipset::new();
        c.vpos = 0x12;
        c.hpos = 0x34;
        assert_eq!(c.read(reg::VHPOSR), 0x1234);
    }

    #[test]
    fn beam_advances_across_a_line() {
        let mut c = Chipset::new();
        // One colour clock's worth of CPU cycles.
        let wrapped = c.tick(4, 4).frame_wrapped;
        assert!(!wrapped);
        assert_eq!(c.hpos, 1);
        assert_eq!(c.vpos, 0);
    }

    #[test]
    fn beam_wraps_hpos_into_vpos_at_line_end() {
        let mut c = Chipset::new();
        let clocks = PAL_COLOUR_CLOCKS_PER_LINE * 4; // one full line
        let wrapped = c.tick(clocks, 4).frame_wrapped;
        assert!(!wrapped, "one line is not a full frame");
        assert_eq!(c.hpos, 0);
        assert_eq!(c.vpos, 1);
    }

    #[test]
    fn vertb_fires_exactly_once_per_frame() {
        let mut c = Chipset::new();
        c.write(
            reg::INTENA,
            0x8000 | (1 << intbit::INTEN) | (1 << intbit::VERTB),
        );
        let frame_clocks = PAL_LINES_PER_FRAME * PAL_COLOUR_CLOCKS_PER_LINE * 4;

        let wrapped = c.tick(frame_clocks, 4).frame_wrapped;
        assert!(wrapped);
        assert_eq!(c.frames, 1);
        assert_eq!(c.pending_level(), 3);

        // Clear INTREQ; a second identical tick must raise it again, once.
        c.write(reg::INTREQ, 1 << intbit::VERTB);
        let wrapped = c.tick(frame_clocks, 4).frame_wrapped;
        assert!(wrapped);
        assert_eq!(c.frames, 2);
        assert_eq!(c.pending_level(), 3);
    }

    #[test]
    fn vpos_bit8_appears_in_vposr() {
        let mut c = Chipset::new();
        c.vpos = 256; // bit 8 set
        assert_eq!(c.read(reg::VPOSR) & 1, 1);
        c.vpos = 255;
        assert_eq!(c.read(reg::VPOSR) & 1, 0);
    }

    #[test]
    fn lof_toggles_every_frame() {
        let mut c = Chipset::new();
        let frame_clocks = PAL_LINES_PER_FRAME * PAL_COLOUR_CLOCKS_PER_LINE * 4;
        let first = c.read(reg::VPOSR) & (1 << 15);
        c.tick(frame_clocks, 4);
        let second = c.read(reg::VPOSR) & (1 << 15);
        assert_ne!(first, second, "LOF flips each frame");
        c.tick(frame_clocks, 4);
        let third = c.read(reg::VPOSR) & (1 << 15);
        assert_eq!(first, third, "and flips back");
    }

    /// Pins the property `machine-hosted`'s run loop depends on when the
    /// 68k core is in `STOP`: `m68k-rs`'s `run_for_cycles_with_hook` does
    /// not call the per-instruction hook while already stopped (its own
    /// doc comment says so), so the host ticks the bus directly in one
    /// large chunk instead of one instruction's worth at a time. That
    /// must still raise VERTB every frame boundary crossed, not just the
    /// first — otherwise a STOP that spans multiple frames' worth of
    /// cycles in a single `tick` call would silently swallow every VERTB
    /// but the last.
    #[test]
    fn vertb_fires_for_every_frame_crossed_in_one_large_tick() {
        let mut c = Chipset::new();
        c.write(
            reg::INTENA,
            0x8000 | (1 << intbit::INTEN) | (1 << intbit::VERTB),
        );
        let frame_clocks = PAL_LINES_PER_FRAME * PAL_COLOUR_CLOCKS_PER_LINE * 4;

        // One tick spanning a little over five frames, matching how a
        // stopped CPU is resynced: one multi-frame chunk, not one
        // instruction-hook call per frame.
        let advance = c.tick(frame_clocks * 5 + 10, 4);
        assert_eq!(c.frames, 5, "all five frame boundaries counted");
        assert!(advance.frame_wrapped);
        assert_eq!(
            c.pending_level(),
            3,
            "VERTB must still be pending after a multi-frame tick"
        );
    }

    #[test]
    fn ntsc_frame_is_262_lines() {
        let mut c = Chipset::new();
        c.set_ntsc(true);
        c.write(
            reg::INTENA,
            0x8000 | (1 << intbit::INTEN) | (1 << intbit::VERTB),
        );
        // One line short of a 262-line NTSC frame: must not have wrapped.
        let almost = (NTSC_LINES_PER_FRAME - 1) * PAL_COLOUR_CLOCKS_PER_LINE * 4;
        c.tick(almost, 4);
        assert_eq!(
            c.frames, 0,
            "261 lines have not completed an NTSC frame yet"
        );

        // The 262nd line completes it, well short of PAL's 312.
        let one_more_line = PAL_COLOUR_CLOCKS_PER_LINE * 4;
        c.tick(one_more_line, 4);
        assert_eq!(c.frames, 1);
    }
}
