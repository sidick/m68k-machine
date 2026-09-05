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
/// In line mode this bit is reinterpreted as `SING` (see below) — same
/// bit, different meaning depending on `LINE`.
pub const BLTCON1_DESC: u16 = 1 << 1;
/// `BLTCON1` bit 2: fill carry input.
///
/// NB: an earlier pass of this file (since lost, see module docs) had
/// `EFE` and `FCI` swapped relative to the Hardware Reference Manual —
/// `FCI` is bit 2 and `EFE` is bit 4, not the reverse. Corrected here
/// against both the HRM and the Copperline oracle
/// (`~/src/external/Copperline/src/chipset/blitter.rs`), whose
/// `BLTCON1_FCI`/`BLTCON1_EFE` constants agree with this layout. `IFE`
/// at bit 3 was already right.
pub const BLTCON1_FCI: u16 = 1 << 2;
/// `BLTCON1` bit 3: inclusive fill.
pub const BLTCON1_IFE: u16 = 1 << 3;
/// `BLTCON1` bit 4: exclusive fill.
pub const BLTCON1_EFE: u16 = 1 << 4;
/// `BLTCON1` bit 6: line-mode Bresenham sign status.
pub const BLTCON1_SIGN: u16 = 1 << 6;

/// Line-mode aliases: `BLTCON1` bits 1-4 are reinterpreted when `LINE` is
/// set. Same bit positions as `DESC`/`FCI`/`IFE`/`EFE` above — the
/// hardware overloads them.
///
/// `SING`: single-dot mode (used with a dashed/dotted line pattern).
pub const BLTCON1_SING: u16 = 1 << 1;
/// `AUL` ("always up/left"): direction of the major (every-pixel) axis.
/// 0 = increasing, 1 = decreasing.
pub const BLTCON1_AUL: u16 = 1 << 2;
/// `SUL` ("sometimes up/left"): direction of the minor (conditionally
/// stepped) axis. 0 = increasing, 1 = decreasing.
pub const BLTCON1_SUL: u16 = 1 << 3;
/// `SUD` ("sometimes up/down"): which axis is minor. 0 = minor is X,
/// major is Y. 1 = minor is Y, major is X.
pub const BLTCON1_SUD: u16 = 1 << 4;

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
    pub fn execute(&mut self, ram: &mut [u8; CHIP_RAM_SIZE]) -> bool {
        if !self.pending {
            return false;
        }
        self.pending = false;
        self.zero = true;
        if self.bltcon1 & BLTCON1_LINE != 0 {
            self.execute_line(ram);
        } else {
            self.execute_area(ram);
        }
        true
    }

    /// Area (normal, non-line) mode: minterm combine of A/B/C into D,
    /// with the A/B barrel shifters, the A first/last-word masks,
    /// per-channel modulos, ascending/descending direction, and
    /// inclusive/exclusive fill.
    fn execute_area(&mut self, ram: &mut [u8; CHIP_RAM_SIZE]) {
        let con0 = self.bltcon0;
        let con1 = self.bltcon1;
        let use_a = con0 & BLTCON0_USEA != 0;
        let use_b = con0 & BLTCON0_USEB != 0;
        let use_c = con0 & BLTCON0_USEC != 0;
        let use_d = con0 & BLTCON0_USED != 0;
        let ash = ((con0 >> 12) & 0x000F) as u32;
        let bsh = ((con1 >> 12) & 0x000F) as u32;
        let desc = con1 & BLTCON1_DESC != 0;
        let lf = (con0 & 0x00FF) as u8;
        let ife = con1 & BLTCON1_IFE != 0;
        let efe = con1 & BLTCON1_EFE != 0;
        let fci: u16 = u16::from(con1 & BLTCON1_FCI != 0);
        // Fill is only well-defined in descending mode (HRM ch. 6): the
        // hardware fill sequencer scans each row from the end of memory
        // backwards, which is exactly what descending traversal already
        // does. graphics.library always pairs IFE/EFE with DESC.
        let fill = desc && (ife || efe);

        let step: i32 = if desc { -2 } else { 2 };
        let signed_step = |m: i16| -> i32 {
            if desc {
                -(m as i32)
            } else {
                m as i32
            }
        };
        // Modulos are added to word-aligned pointers, so the hardware
        // ignores bit 0 of the programmed value. Adding it raw put the
        // pointer one byte further out than Copperline's for every odd
        // modulo — 10 divergences, invisible to a test suite that
        // applied the same arithmetic it was checking.
        let amod = signed_step(even(self.modulo[CHAN_A]));
        let bmod = signed_step(even(self.modulo[CHAN_B]));
        let cmod = signed_step(even(self.modulo[CHAN_C]));
        let dmod = signed_step(even(self.modulo[CHAN_D]));

        let mut apt = self.pt[CHAN_A];
        let mut bpt = self.pt[CHAN_B];
        let mut cpt = self.pt[CHAN_C];
        let mut dpt = self.pt[CHAN_D];

        // The barrel shifter carries the previously-processed word of
        // each channel into the next word's shift. It is a shift
        // register, not per-row state: the hardware never clears it
        // between rows, so the first word of every row after the first
        // shifts in bits from the last word of the row before. Declaring
        // these inside the row loop instead cost 21 divergences against
        // Copperline — every shifted case with more than one row — while
        // every unit test passed, since those checked the implementation
        // against the same assumption it was built on.
        let mut a_prev: u16 = 0;
        let mut b_prev: u16 = 0;
        let mut last_d: u16 = 0;

        for _row in 0..self.height_rows {
            let mut fill_state: u16 = fci;

            for word_idx in 0..self.width_words {
                let first = word_idx == 0;
                let last = word_idx + 1 == self.width_words;

                // Channel A: AFWM/ALWM and the shifter apply whether or
                // not USEA is set — a disabled A still contributes
                // BLTADAT through the same masked, shifted pipeline.
                let a_raw = if use_a {
                    let v = read_word(ram, apt);
                    apt = apt.wrapping_add(step as u32);
                    v
                } else {
                    self.dat[CHAN_A]
                };
                let mut a_masked = a_raw;
                if first {
                    a_masked &= self.bltafwm;
                }
                if last {
                    a_masked &= self.bltalwm;
                }
                let a = shift_combine(a_prev, a_masked, ash, desc);
                a_prev = a_masked;

                let b = if use_b {
                    let v = read_word(ram, bpt);
                    bpt = bpt.wrapping_add(step as u32);
                    let shifted = shift_combine(b_prev, v, bsh, desc);
                    b_prev = v;
                    shifted
                } else {
                    // Disabled B contributes BLTBDAT directly, unshifted
                    // — there is no fetched stream to carry a shift
                    // across.
                    self.dat[CHAN_B]
                };

                let c = if use_c {
                    let v = read_word(ram, cpt);
                    cpt = cpt.wrapping_add(step as u32);
                    // A C fetch loads BLTCDAT itself (readable back).
                    self.dat[CHAN_C] = v;
                    v
                } else {
                    self.dat[CHAN_C]
                };

                let mut d = minterm(lf, a, b, c);
                if fill {
                    d = apply_fill(d, &mut fill_state, ife, efe);
                }

                if d != 0 {
                    self.zero = false;
                }
                last_d = d;

                if use_d {
                    write_word(ram, dpt, d);
                    dpt = dpt.wrapping_add(step as u32);
                }
            }

            if use_a {
                apt = apt.wrapping_add(amod as u32);
            }
            if use_b {
                bpt = bpt.wrapping_add(bmod as u32);
            }
            if use_c {
                cpt = cpt.wrapping_add(cmod as u32);
            }
            if use_d {
                dpt = dpt.wrapping_add(dmod as u32);
            }
        }

        self.pt[CHAN_A] = apt;
        self.pt[CHAN_B] = bpt;
        self.pt[CHAN_C] = cpt;
        self.pt[CHAN_D] = dpt;
        self.bltddat = last_d;
    }

    /// Line mode: Bresenham single-pixel-wide line draw. `BLTSIZE`'s
    /// height field carries the pixel count (software always sets the
    /// width field to 2 by convention; the hardware ignores it here).
    ///
    /// Channel usage, per the Hardware Reference Manual:
    /// - A carries the single-bit texture (`BLTADAT`, typically
    ///   `$8000`), masked by `BLTAFWM` and shifted right by the initial
    ///   `ash` (the start pixel's column within its word, `BLTCON0`
    ///   bits 15-12).
    /// - B optionally carries a dashed/dotted line pattern, rotated one
    ///   bit per pixel.
    /// - C/D are the read-modify-write destination; `BLTCPT`/`BLTDPT`
    ///   both point at the pixel's word, `BLTCMOD` is the bitplane's
    ///   bytes-per-row (used to step the major/minor Y axis).
    /// - `BLTAPT`'s low word is the signed Bresenham error accumulator;
    ///   `BLTAMOD`/`BLTBMOD` are the two step deltas (taken when the
    ///   minor axis is/isn't stepped this pixel).
    ///
    /// Octant decode (`BLTCON1` bits 4-2 = `SUD`,`SUL`,`AUL`) and the
    /// accumulator stepping are cross-checked against an independent
    /// Bresenham implementation in the tests below, across several
    /// octants including negative-delta lines.
    ///
    /// Gap: the `SING` single-dot suppression is implemented (locks the
    /// store but still runs the shifter/minterm/BZERO update, per the
    /// oracle), but is not independently cross-checked the way the
    /// octant/accumulator math is — low confidence there relative to
    /// the rest of line mode.
    fn execute_line(&mut self, ram: &mut [u8; CHIP_RAM_SIZE]) {
        let con0 = self.bltcon0;
        let lf = (con0 & 0x00FF) as u8;
        let use_a = con0 & BLTCON0_USEA != 0;
        let use_b = con0 & BLTCON0_USEB != 0;
        let use_c = con0 & BLTCON0_USEC != 0;

        let con1 = self.bltcon1;
        let mut bsh = (con1 >> 12) & 0x000F;
        let sing = con1 & BLTCON1_SING != 0;

        let bplmod = self.modulo[CHAN_C] as i32;
        let amod_step = self.modulo[CHAN_A] as u16;
        let bmod_step = self.modulo[CHAN_B] as u16;

        let mut bpt = self.pt[CHAN_B];
        let mut cpt = self.pt[CHAN_C];
        let mut dpt = self.pt[CHAN_D];
        let mut ash_now = ((con0 >> 12) & 0x000F) as i32;
        let mut acc = self.pt[CHAN_A] as u16;
        let mut sign = con1 & BLTCON1_SIGN != 0;
        let mut one_dot = false;

        let mut bdat = self.dat[CHAN_B].rotate_right(bsh as u32);
        let a_word = self.dat[CHAN_A] & self.bltafwm;

        let npixels = self.height_rows;
        let mut last_d: u16 = 0;

        for _ in 0..npixels {
            if use_b {
                let fetched = read_word(ram, bpt);
                bpt = bpt.wrapping_add((bmod_step as i16 as i32) as u32);
                bdat = fetched.rotate_right(bsh as u32);
            }
            // A SING-suppressed dot only locks the store: the shifter,
            // the minterm, and the BZERO update still run on the full
            // inputs.
            let line_pixel = !sing || !one_dot;
            let a_shifted = a_word >> (ash_now as u32);
            one_dot = true;
            let b_shifted = if bdat & 1 != 0 { 0xFFFF } else { 0 };
            let c = if use_c {
                let v = read_word(ram, cpt);
                self.dat[CHAN_C] = v;
                v
            } else {
                self.dat[CHAN_C]
            };
            let d = minterm(lf, a_shifted, b_shifted, c);

            if !sign {
                ash_now = line_step_sometimes(con1, ash_now, bplmod, &mut cpt, &mut one_dot);
            }
            // The error accumulator only advances with USEA set.
            if use_a {
                acc = acc.wrapping_add(if sign { bmod_step } else { amod_step });
            }
            ash_now = line_step_always(con1, ash_now, bplmod, &mut cpt, &mut one_dot);
            sign = (acc as i16) < 0;

            if d != 0 {
                self.zero = false;
            }
            last_d = d;
            if use_c && line_pixel {
                write_word(ram, dpt, d);
            }
            dpt = cpt;
            bdat = bdat.rotate_left(1);
            bsh = bsh.wrapping_sub(1) & 0x000F;
        }

        let mut con1_out = self.bltcon1 & !BLTCON1_SIGN;
        if (acc as i16) < 0 {
            con1_out |= BLTCON1_SIGN;
        }
        con1_out = (con1_out & 0x0FFF) | (bsh << 12);
        self.bltcon1 = con1_out;
        self.bltcon0 = (self.bltcon0 & 0x0FFF) | ((ash_now as u16 & 0x000F) << 12);
        self.pt[CHAN_B] = bpt;
        self.pt[CHAN_C] = cpt;
        self.pt[CHAN_D] = dpt;
        self.pt[CHAN_A] = (self.pt[CHAN_A] & 0xFFFF_0000) | acc as u32;
        self.bltddat = last_d;
    }
}

/// Mask a guest-controlled blitter pointer into chip RAM. Blitter
/// pointers are hostile input (§ task constraints): a wild pointer must
/// produce garbage output, never a panic. `CHIP_RAM_SIZE` is a power of
/// two, so masking with `SIZE - 1` always lands in bounds; the extra
/// `& !1` keeps the access word-aligned (the hardware ignores pointer
/// bit 0) so `off + 1` is never out of range.
fn chip_addr(ptr: u32) -> usize {
    (ptr as usize) & (CHIP_RAM_SIZE - 1) & !1
}

fn read_word(ram: &[u8; CHIP_RAM_SIZE], ptr: u32) -> u16 {
    let off = chip_addr(ptr);
    u16::from_be_bytes([ram[off], ram[off + 1]])
}

fn write_word(ram: &mut [u8; CHIP_RAM_SIZE], ptr: u32, value: u16) {
    let off = chip_addr(ptr);
    let bytes = value.to_be_bytes();
    ram[off] = bytes[0];
    ram[off + 1] = bytes[1];
}

/// All-bits-parallel evaluation of the 8-bit minterm `lf` on three
/// 16-bit channel words. Deliberately spelled out as eight independent
/// AND-product terms — one per row of the truth table `lf` encodes —
/// rather than any bit-twiddling shortcut, so it is obviously correct
/// against the Hardware Reference Manual's minterm table by inspection.
fn minterm(lf: u8, a: u16, b: u16, c: u16) -> u16 {
    let na = !a;
    let nb = !b;
    let nc = !c;
    let mut d = 0u16;
    if lf & 0b1000_0000 != 0 {
        d |= a & b & c;
    }
    if lf & 0b0100_0000 != 0 {
        d |= a & b & nc;
    }
    if lf & 0b0010_0000 != 0 {
        d |= a & nb & c;
    }
    if lf & 0b0001_0000 != 0 {
        d |= a & nb & nc;
    }
    if lf & 0b0000_1000 != 0 {
        d |= na & b & c;
    }
    if lf & 0b0000_0100 != 0 {
        d |= na & b & nc;
    }
    if lf & 0b0000_0010 != 0 {
        d |= na & nb & c;
    }
    if lf & 0b0000_0001 != 0 {
        d |= na & nb & nc;
    }
    d
}

/// Barrel-shift combine of the previous and current raw channel word.
///
/// Ascending mode produces `(prev:cur) >> n` (low 16 bits): the bottom
/// `n` bits of the previously-processed word (lower address, to the
/// left) fill the top `n` bits of the shifted current word.
///
/// Descending mode produces `(cur:prev) << n` (high 16 bits): the top
/// `n` bits of the previously-processed word (higher address, visited
/// first in descending order, to the right) fill the bottom `n` bits.
fn shift_combine(prev: u16, cur: u16, n: u32, desc: bool) -> u16 {
    if n == 0 {
        return cur;
    }
    if desc {
        let combined = ((cur as u32) << 16) | (prev as u32);
        ((combined << n) >> 16) as u16
    } else {
        let combined = ((prev as u32) << 16) | (cur as u32);
        (combined >> n) as u16
    }
}

/// Inclusive/exclusive fill, applied to the post-minterm D word. Runs
/// across the word from bit 0 (the row's trailing edge, since fill
/// requires descending traversal — see `execute_area`) to bit 15,
/// carrying `fill_state` across word boundaries within a row via the
/// caller.
fn apply_fill(d: u16, fill_state: &mut u16, ife: bool, efe: bool) -> u16 {
    let mut out = d;
    for bit in 0..16u16 {
        let mask = 1u16 << bit;
        if *fill_state != 0 {
            if ife {
                out |= mask;
            } else if efe {
                out ^= mask;
            }
        }
        if d & mask != 0 {
            *fill_state ^= 1;
        }
    }
    out
}

/// The "sometimes" (minor-axis) Bresenham step: taken only when the
/// error accumulator's sign says so. `SUD` picks which axis is minor;
/// `SUL` picks that axis's direction.
fn line_step_sometimes(
    bltcon1: u16,
    ash: i32,
    bplmod: i32,
    cpt: &mut u32,
    one_dot: &mut bool,
) -> i32 {
    if bltcon1 & BLTCON1_SUD != 0 {
        let dy = if bltcon1 & BLTCON1_SUL != 0 { -1 } else { 1 };
        line_step_y(dy, bplmod, cpt, one_dot);
        ash
    } else {
        let dx = if bltcon1 & BLTCON1_SUL != 0 { -1 } else { 1 };
        line_step_x(ash, dx, cpt)
    }
}

/// The "always" (major-axis) Bresenham step: taken every pixel. `SUD`
/// again picks which axis is major (the opposite of the minor one);
/// `AUL` picks that axis's direction.
fn line_step_always(bltcon1: u16, ash: i32, bplmod: i32, cpt: &mut u32, one_dot: &mut bool) -> i32 {
    if bltcon1 & BLTCON1_SUD != 0 {
        let dx = if bltcon1 & BLTCON1_AUL != 0 { -1 } else { 1 };
        line_step_x(ash, dx, cpt)
    } else {
        let dy = if bltcon1 & BLTCON1_AUL != 0 { -1 } else { 1 };
        line_step_y(dy, bplmod, cpt, one_dot);
        ash
    }
}

/// Step one pixel horizontally: advance `ash` (the bit position within
/// the current word) and roll `cpt` to the neighbouring word when it
/// runs off either end.
fn line_step_x(ash: i32, dx: i32, cpt: &mut u32) -> i32 {
    if dx > 0 {
        let n = ash + 1;
        if n > 15 {
            *cpt = cpt.wrapping_add(2);
            0
        } else {
            n
        }
    } else {
        let n = ash - 1;
        if n < 0 {
            *cpt = cpt.wrapping_sub(2);
            15
        } else {
            n
        }
    }
}

/// Step one pixel vertically: advance `cpt` by one bitplane row.
fn line_step_y(dy: i32, bplmod: i32, cpt: &mut u32, one_dot: &mut bool) {
    let delta = if dy > 0 { bplmod } else { -bplmod };
    *cpt = cpt.wrapping_add(delta as u32);
    *one_dot = false;
}

/// Clear bit 0 of a modulo: pointers are word-aligned and the hardware
/// never applies the odd byte.
fn even(modulo: i16) -> i16 {
    modulo & !1
}

fn set_ptr_hi(ptr: &mut u32, value: u16) {
    // The DMA pointer registers are 21 bits wide -- chip RAM tops out at
    // 2 MB (`crate::CHIP_RAM_SIZE`) -- so only bits 4:0 of the high word
    // are implemented and the rest read back as zero. Found by the
    // recorded-Workbench-trace differential: line mode reuses `BLTAPT`
    // as a Bresenham error term, and graphics.library writes one whose
    // high word exceeds five bits, making the truncation observable in
    // register readback where an ordinary address never would.
    *ptr = (*ptr & 0x0000_FFFF) | (((value & 0x001F) as u32) << 16);
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

    // ---- execute() tests -------------------------------------------

    /// A heap-allocated chip RAM buffer, built without ever materialising
    /// the 2 MB array on the stack (mirrors `lib.rs`'s `boxed_chip_ram`).
    fn new_ram() -> std::boxed::Box<[u8; CHIP_RAM_SIZE]> {
        let boxed_slice: std::boxed::Box<[u8]> = std::vec![0u8; CHIP_RAM_SIZE].into_boxed_slice();
        boxed_slice
            .try_into()
            .unwrap_or_else(|_| unreachable!("boxed_slice has exactly CHIP_RAM_SIZE elements"))
    }

    fn poke_word(ram: &mut [u8; CHIP_RAM_SIZE], addr: u32, value: u16) {
        let off = addr as usize;
        let b = value.to_be_bytes();
        ram[off] = b[0];
        ram[off + 1] = b[1];
    }

    fn peek_word(ram: &[u8; CHIP_RAM_SIZE], addr: u32) -> u16 {
        let off = addr as usize;
        u16::from_be_bytes([ram[off], ram[off + 1]])
    }

    /// Minterms verified against a per-bit truth table written from
    /// scratch (indexing straight into the `lf` byte), independent of
    /// `minterm`'s AND/OR-product decomposition, across all 256 `lf`
    /// values and all 8 `(a,b,c)` input combinations.
    #[test]
    fn minterms_match_independent_truth_table() {
        fn truth_table_bit(lf: u8, a: bool, b: bool, c: bool) -> bool {
            // HRM convention: LF bit 7 = (A,B,C)=(1,1,1) ... bit 0 =
            // (0,0,0) — i.e. the bit index *is* the 3-bit (a,b,c) value.
            let idx = ((a as u8) << 2) | ((b as u8) << 1) | (c as u8);
            (lf >> idx) & 1 != 0
        }

        for lf in 0u16..=255 {
            let lf = lf as u8;
            for &a in &[false, true] {
                for &b in &[false, true] {
                    for &c in &[false, true] {
                        let aw = if a { 0xFFFF } else { 0 };
                        let bw = if b { 0xFFFF } else { 0 };
                        let cw = if c { 0xFFFF } else { 0 };
                        let got = minterm(lf, aw, bw, cw);
                        let expected = if truth_table_bit(lf, a, b, c) {
                            0xFFFF
                        } else {
                            0
                        };
                        assert_eq!(got, expected, "lf={lf:#04x} a={a} b={b} c={c}");
                    }
                }
            }
        }
    }

    #[test]
    fn plain_bltbitmap_straight_copy() {
        // The most common real operation: D = A, one bitplane's worth
        // of words, straight across.
        let mut ram = new_ram();
        for i in 0..8u32 {
            poke_word(&mut ram, i * 2, 0x1111 * (i as u16 + 1));
        }
        let mut b = Blitter::new();
        b.write(reg::BLTCON0, 0x0FF0); // USEA|USED, LF = D=A
        b.write(reg::BLTCON1, 0x0000);
        b.write(reg::BLTAFWM, 0xFFFF);
        b.write(reg::BLTALWM, 0xFFFF);
        b.write(reg::BLTAPTH, 0);
        b.write(reg::BLTAPTL, 0);
        b.write(reg::BLTDPTH, 0);
        b.write(reg::BLTDPTL, 100);
        b.write(reg::BLTAMOD, 0);
        b.write(reg::BLTDMOD, 0);
        b.write(reg::BLTSIZE, (1 << 6) | 8);
        assert!(b.execute(&mut ram));

        for i in 0..8u32 {
            assert_eq!(peek_word(&ram, 100 + i * 2), 0x1111 * (i as u16 + 1));
        }
        assert_eq!(b.pt[CHAN_A], 16);
        assert_eq!(b.pt[CHAN_D], 116);
        assert!(!b.zero);
        assert_eq!(b.dmaconr_bits(), 0);
    }

    #[test]
    fn bzero_true_when_every_written_word_is_zero() {
        let mut ram = new_ram();
        let mut b = Blitter::new();
        b.write(reg::BLTCON0, 0x0FF0); // D=A
        b.write(reg::BLTCON1, 0);
        b.write(reg::BLTAFWM, 0xFFFF);
        b.write(reg::BLTALWM, 0xFFFF);
        b.write(reg::BLTDPTH, 0);
        b.write(reg::BLTDPTL, 0);
        b.write(reg::BLTSIZE, (1 << 6) | 4);
        assert!(b.execute(&mut ram));
        assert!(b.zero);
        assert_eq!(b.dmaconr_bits(), DMACONR_BZERO);
    }

    #[test]
    fn shift_a_carries_across_words_ascending() {
        let mut ram = new_ram();
        poke_word(&mut ram, 0, 0x1234);
        poke_word(&mut ram, 2, 0x5678);
        let mut b = Blitter::new();
        b.write(reg::BLTCON0, 0x4FF0); // ash=4, USEA|USED, LF=D=A
        b.write(reg::BLTCON1, 0x0000); // ascending
        b.write(reg::BLTAFWM, 0xFFFF);
        b.write(reg::BLTALWM, 0xFFFF);
        b.write(reg::BLTAPTL, 0);
        b.write(reg::BLTDPTL, 100);
        b.write(reg::BLTSIZE, (1 << 6) | 2);
        assert!(b.execute(&mut ram));

        // Hand-derived: word0 = (0:0x1234)>>4 = 0x0123, word1 =
        // (0x1234:0x5678)>>4 = 0x12345678>>4, low 16 bits = 0x4567.
        assert_eq!(peek_word(&ram, 100), 0x0123);
        assert_eq!(peek_word(&ram, 102), 0x4567);
    }

    #[test]
    fn shift_a_carries_across_words_descending() {
        let mut ram = new_ram();
        // Lower address holds the word processed *second* in descending
        // order; higher address holds the word processed first.
        poke_word(&mut ram, 0, 0x1234);
        poke_word(&mut ram, 2, 0x5678);
        let mut b = Blitter::new();
        b.write(reg::BLTCON0, 0x4FF0); // ash=4, USEA|USED, LF=D=A
        b.write(reg::BLTCON1, BLTCON1_DESC);
        b.write(reg::BLTAFWM, 0xFFFF);
        b.write(reg::BLTALWM, 0xFFFF);
        b.write(reg::BLTAPTL, 2); // starts at the higher address
        b.write(reg::BLTDPTL, 102);
        b.write(reg::BLTSIZE, (1 << 6) | 2);
        assert!(b.execute(&mut ram));

        // Hand-derived (see shift_combine doc comment):
        // iter0 (cur=0x5678,prev=0): combined=0x56780000, <<4 = 0x67800000,
        //   >>16 = 0x6780, written to the higher dest address.
        // iter1 (cur=0x1234,prev=0x5678): combined=0x12345678, <<4 =
        //   0x23456780, >>16 = 0x2345, written to the lower dest address.
        assert_eq!(peek_word(&ram, 102), 0x6780);
        assert_eq!(peek_word(&ram, 100), 0x2345);
    }

    #[test]
    fn first_and_last_word_masks() {
        let mut ram = new_ram();
        poke_word(&mut ram, 0, 0xFFFF);
        poke_word(&mut ram, 2, 0xFFFF);
        poke_word(&mut ram, 4, 0xFFFF);
        let mut b = Blitter::new();
        b.write(reg::BLTCON0, 0x0FF0); // no shift, D=A
        b.write(reg::BLTCON1, 0);
        b.write(reg::BLTAFWM, 0xFF00); // mask off the low byte of word 0
        b.write(reg::BLTALWM, 0x00FF); // mask off the high byte of the last word
        b.write(reg::BLTAPTL, 0);
        b.write(reg::BLTDPTL, 100);
        b.write(reg::BLTSIZE, (1 << 6) | 3);
        assert!(b.execute(&mut ram));

        assert_eq!(peek_word(&ram, 100), 0xFF00, "first word: AFWM only");
        assert_eq!(peek_word(&ram, 102), 0xFFFF, "middle word: unmasked");
        assert_eq!(peek_word(&ram, 104), 0x00FF, "last word: ALWM only");
    }

    #[test]
    fn masks_both_apply_on_a_one_word_row() {
        let mut ram = new_ram();
        poke_word(&mut ram, 0, 0xFFFF);
        let mut b = Blitter::new();
        b.write(reg::BLTCON0, 0x0FF0);
        b.write(reg::BLTCON1, 0);
        b.write(reg::BLTAFWM, 0xFF00);
        b.write(reg::BLTALWM, 0x00FF);
        b.write(reg::BLTAPTL, 0);
        b.write(reg::BLTDPTL, 100);
        b.write(reg::BLTSIZE, (1 << 6) | 1);
        assert!(b.execute(&mut ram));
        // Both masks apply to the single word: 0xFF00 & 0x00FF = 0.
        assert_eq!(peek_word(&ram, 100), 0x0000);
    }

    #[test]
    fn inclusive_fill_without_carry_in() {
        let mut ram = new_ram();
        poke_word(&mut ram, 0, 0x1008); // edges at bit 3 and bit 12
        let mut b = Blitter::new();
        b.write(reg::BLTCON0, 0x0FF0); // D=A
        b.write(reg::BLTCON1, BLTCON1_DESC | BLTCON1_IFE);
        b.write(reg::BLTAFWM, 0xFFFF);
        b.write(reg::BLTALWM, 0xFFFF);
        b.write(reg::BLTAPTL, 0);
        b.write(reg::BLTDPTL, 100);
        b.write(reg::BLTSIZE, (1 << 6) | 1);
        assert!(b.execute(&mut ram));
        // Hand-derived (see apply_fill doc comment / module notes):
        // bits 3..=12 filled inclusive of both edges.
        assert_eq!(peek_word(&ram, 100), 0x1FF8);
    }

    #[test]
    fn exclusive_fill_without_carry_in() {
        let mut ram = new_ram();
        poke_word(&mut ram, 0, 0x1008); // edges at bit 3 and bit 12
        let mut b = Blitter::new();
        b.write(reg::BLTCON0, 0x0FF0); // D=A
        b.write(reg::BLTCON1, BLTCON1_DESC | BLTCON1_EFE);
        b.write(reg::BLTAFWM, 0xFFFF);
        b.write(reg::BLTALWM, 0xFFFF);
        b.write(reg::BLTAPTL, 0);
        b.write(reg::BLTDPTL, 100);
        b.write(reg::BLTSIZE, (1 << 6) | 1);
        assert!(b.execute(&mut ram));
        // Hand-derived: bits 3..=11 set, bit 12 flips off (it's the
        // trailing edge XORed against a still-active fill state).
        assert_eq!(peek_word(&ram, 100), 0x0FF8);
    }

    #[test]
    fn inclusive_fill_carry_in_changes_the_result() {
        // The rectangle-outline case graphics.library generates: a row
        // with only the trailing edge of the shape (the leading edge
        // was on a previous row, or off the left of this word), so
        // whether the interior gets filled depends entirely on FCI.
        let mut ram = new_ram();
        poke_word(&mut ram, 0, 0x1000); // one edge at bit 12
        poke_word(&mut ram, 2, 0x1000);
        let mut b = Blitter::new();
        b.write(reg::BLTCON0, 0x0FF0);
        b.write(reg::BLTAFWM, 0xFFFF);
        b.write(reg::BLTALWM, 0xFFFF);

        // Fill mode is only well-defined descending (see execute_area),
        // so the A pointer decrements after each word; explicitly reset
        // BLTAPTH/BLTDPTH before every sub-blit below rather than
        // relying on whatever the previous descending blit left behind.
        b.write(reg::BLTAPTH, 0);
        b.write(reg::BLTAPTL, 0);
        b.write(reg::BLTDPTH, 0);
        b.write(reg::BLTCON1, BLTCON1_DESC | BLTCON1_IFE);
        b.write(reg::BLTDPTL, 100);
        b.write(reg::BLTSIZE, (1 << 6) | 1);
        assert!(b.execute(&mut ram));
        assert_eq!(
            peek_word(&ram, 100),
            0xF000,
            "no carry-in: fills after the edge only"
        );

        b.write(reg::BLTCON1, BLTCON1_DESC | BLTCON1_IFE | BLTCON1_FCI);
        b.write(reg::BLTAPTH, 0);
        b.write(reg::BLTAPTL, 2);
        b.write(reg::BLTDPTH, 0);
        b.write(reg::BLTDPTL, 102);
        b.write(reg::BLTSIZE, (1 << 6) | 1);
        assert!(b.execute(&mut ram));
        assert_eq!(
            peek_word(&ram, 102),
            0x1FFF,
            "carry-in: the row starts already inside the fill"
        );
    }

    #[test]
    fn descending_overlapping_copy() {
        // The classic overlap-safe scroll technique: copy N words one
        // word forward in memory (dest = src + 2 bytes), which would
        // corrupt data read via an ascending copy but is safe
        // descending, since each word is read before its own address
        // is ever written.
        let mut ram = new_ram();
        for i in 0..8u32 {
            poke_word(&mut ram, i * 2, (i + 1) as u16);
        }
        let mut b = Blitter::new();
        b.write(reg::BLTCON0, 0x0FF0); // D=A
        b.write(reg::BLTCON1, BLTCON1_DESC);
        b.write(reg::BLTAFWM, 0xFFFF);
        b.write(reg::BLTALWM, 0xFFFF);
        b.write(reg::BLTAPTL, 14); // last source word (index 7)
        b.write(reg::BLTDPTL, 16); // last dest word, one word on
        b.write(reg::BLTSIZE, (1 << 6) | 8);
        assert!(b.execute(&mut ram));

        for i in 0..8u32 {
            assert_eq!(peek_word(&ram, 2 + i * 2), (i + 1) as u16, "word {i}");
        }
    }

    #[test]
    fn modulo_handles_negative_values() {
        // Two-word-wide rows, three rows, with a negative D modulo so
        // successive rows overlap by one word instead of packing edge
        // to edge — an ordinary use graphics.library relies on.
        let mut ram = new_ram();
        for i in 0..6u32 {
            poke_word(&mut ram, i * 2, (i + 1) as u16);
        }
        let mut b = Blitter::new();
        b.write(reg::BLTCON0, 0x0FF0); // D=A
        b.write(reg::BLTCON1, 0);
        b.write(reg::BLTAFWM, 0xFFFF);
        b.write(reg::BLTALWM, 0xFFFF);
        b.write(reg::BLTAPTL, 0);
        b.write(reg::BLTAMOD, 0);
        b.write(reg::BLTDPTL, 200);
        b.write(reg::BLTDMOD, (-2i16) as u16);
        b.write(reg::BLTSIZE, (3 << 6) | 2);
        assert!(b.execute(&mut ram));

        // Row step is width*2 + modulo = 4 + (-2) = 2 bytes, so each row
        // starts only 2 bytes after the previous row's start — each row
        // after the first overlaps and overwrites the second word of
        // the row before it. Row 0 writes 200<-1, 202<-2; row 1 (start
        // 202) writes 202<-3 (overwriting), 204<-4; row 2 (start 204)
        // writes 204<-5 (overwriting), 206<-6. Final contents are the
        // last write to each address.
        assert_eq!(peek_word(&ram, 200), 1);
        assert_eq!(peek_word(&ram, 202), 3);
        assert_eq!(peek_word(&ram, 204), 5);
        assert_eq!(peek_word(&ram, 206), 6);
        // Final D pointer: starts at 200, steps by 2 bytes per row for
        // 3 rows.
        assert_eq!(b.pt[CHAN_D], 206);
    }

    #[test]
    fn bounds_safety_wild_pointers_never_panic() {
        let mut ram = new_ram();
        let mut b = Blitter::new();
        b.write(reg::BLTCON0, 0x0FF0);
        b.write(reg::BLTCON1, 0);
        b.write(reg::BLTAFWM, 0xFFFF);
        b.write(reg::BLTALWM, 0xFFFF);
        b.write(reg::BLTAPTH, 0xFFFF);
        b.write(reg::BLTAPTL, 0xFFFE);
        b.write(reg::BLTDPTH, 0x1234);
        b.write(reg::BLTDPTL, 0x5678);
        b.write(reg::BLTAMOD, i16::MIN as u16);
        b.write(reg::BLTDMOD, i16::MAX as u16);
        b.write(reg::BLTSIZE, (4 << 6) | 16);
        // Must not panic, whatever it computes.
        assert!(b.execute(&mut ram));
    }

    /// A standard integer Bresenham, written independently of the
    /// blitter's octant-decode state machine, used as the oracle for
    /// the line-mode tests below.
    fn independent_bresenham(x0: i32, y0: i32, x1: i32, y1: i32) -> std::vec::Vec<(i32, i32)> {
        let dx = (x1 - x0).abs();
        let dy = (y1 - y0).abs();
        let sx: i32 = if x1 > x0 { 1 } else { -1 };
        let sy: i32 = if y1 > y0 { 1 } else { -1 };
        let mut pts = std::vec::Vec::new();
        let (mut x, mut y) = (x0, y0);
        if dx >= dy {
            let mut err = dx / 2;
            for _ in 0..=dx {
                pts.push((x, y));
                err -= dy;
                if err < 0 {
                    y += sy;
                    err += dx;
                }
                x += sx;
            }
        } else {
            let mut err = dy / 2;
            for _ in 0..=dy {
                pts.push((x, y));
                err -= dx;
                if err < 0 {
                    x += sx;
                    err += dy;
                }
                y += sy;
            }
        }
        pts
    }

    /// Draw one line via the blitter's line mode and return the set
    /// pixel coordinates it actually wrote, reading a single-word-wide
    /// (16 px) bitplane back out of chip RAM.
    fn draw_line_get_pixels(x0: i32, y0: i32, x1: i32, y1: i32) -> std::vec::Vec<(i32, i32)> {
        let dx = x1 - x0;
        let dy = y1 - y0;
        let adx = dx.unsigned_abs() as i32;
        let ady = dy.unsigned_abs() as i32;
        let (sud, err0, amod, bmod, npixels, sul, aul) = if adx >= ady {
            (
                true,
                2 * ady - adx,
                2 * (ady - adx),
                2 * ady,
                adx + 1,
                dy < 0,
                dx < 0,
            )
        } else {
            (
                false,
                2 * adx - ady,
                2 * (adx - ady),
                2 * adx,
                ady + 1,
                dx < 0,
                dy < 0,
            )
        };

        const BPLMOD: i32 = 2; // one word (16 px) per row, no gap
        let mut ram = new_ram();
        let mut b = Blitter::new();
        // D = A | C: draw the new pixel, keep whatever was already
        // there, so multiple pixels landing in the same word (a shallow
        // run at a fixed row) don't clobber each other.
        // USEA|USEC (0x0A00) so A supplies the texture bit and C reads
        // back the destination word for the D=A|C minterm (0xFA).
        b.write(reg::BLTCON0, 0x0A00 | 0x00FA | ((x0 as u16 & 0x000F) << 12));
        let mut con1 = BLTCON1_LINE;
        if sud {
            con1 |= BLTCON1_SUD;
        }
        if sul {
            con1 |= BLTCON1_SUL;
        }
        if aul {
            con1 |= BLTCON1_AUL;
        }
        if err0 < 0 {
            con1 |= BLTCON1_SIGN;
        }
        b.write(reg::BLTCON1, con1);
        b.write(reg::BLTAFWM, 0xFFFF);
        b.write(reg::BLTADAT, 0x8000);
        b.write(reg::BLTAMOD, amod as u16);
        b.write(reg::BLTBMOD, bmod as u16);
        b.write(reg::BLTCMOD, BPLMOD as u16);
        let start_addr = (y0 as u32) * (BPLMOD as u32);
        b.write(reg::BLTCPTL, start_addr as u16);
        b.write(reg::BLTDPTL, start_addr as u16);
        b.write(reg::BLTAPTL, (err0 as i16) as u16);
        b.write(reg::BLTSIZE, ((npixels as u16) << 6) | 2);
        assert!(b.execute(&mut ram));

        let mut pixels = std::vec::Vec::new();
        for y in 0..16i32 {
            let word = peek_word(&ram, (y as u32) * (BPLMOD as u32));
            for x in 0..16i32 {
                if word & (0x8000 >> x) != 0 {
                    pixels.push((x, y));
                }
            }
        }
        pixels.sort_unstable();
        pixels
    }

    #[test]
    fn line_mode_matches_independent_bresenham_across_octants() {
        for &(x0, y0, x1, y1) in &[
            (0, 0, 7, 3),
            (7, 3, 0, 0),
            (0, 0, 3, 7),
            (3, 7, 0, 0),
            (2, 2, 9, 9),
            (9, 9, 2, 2),
        ] {
            let mut expected: std::vec::Vec<(i32, i32)> = independent_bresenham(x0, y0, x1, y1);
            expected.sort_unstable();
            let got = draw_line_get_pixels(x0, y0, x1, y1);
            assert_eq!(got, expected, "line ({x0},{y0})-({x1},{y1})");
        }
    }
}
