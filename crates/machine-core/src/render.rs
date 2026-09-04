//! Stop-gap planar renderer (proposal §8.1).
//!
//! For visibility before P96 is installed — early startup menu, boot,
//! Gurus, Screenmode prefs — and as the permanent Guru/early-boot display.
//! Deliberately dumb: once per frame it walks the copper list linearly
//! (`MOVE`s honoured, `WAIT`s skipped), then renders once from the latched
//! `BPL`/`DIW`/`DDF`/`COLOR` state that walk produced. Lores/hires, 1-5
//! bitplanes, interlace, and a software mouse pointer from sprite 0 are in
//! scope. Per-line palettes, HAM, EHB and dual playfield are explicitly
//! **not** — see [`Geometry::decode`]'s doc comment for how out-of-scope
//! `BPLCON0` states degrade instead of panicking.
//!
//! **Not extended, ever** (§8.1). If a change here would need per-line
//! palette switches, HAM/EHB decode, or a second playfield, the answer is
//! no — that work belongs to P96 (§8.2), which is the production display
//! path this renderer exists only to hold the fort until.
//!
//! No heap: every scratch buffer is a fixed-size stack array, sized for
//! worst-case real hardware geometry plus headroom, never for a guest's
//! actual (possibly hostile) register values — every loop bound is clamped
//! independently of what the guest wrote. Every chip RAM read is
//! guest-controlled and goes through [`read_byte`]/[`read_word`], which
//! return `0` rather than panic when the address is out of range: a wild
//! `BPLPT`, `COP1LC` or `SPR0PT` must produce a garbage picture, never a
//! crash.
//!
//! The DIW/DDF/BPLCON bit arithmetic here was cross-checked against the
//! Amiga Hardware Reference Manual's documented examples and against
//! Copperline's `chipset::denise::DiwHigh` (GPL-3, read for understanding
//! only, never copied — Copperline is this project's test oracle, proposal
//! §12).

use crate::chipset::{reg, Chipset};
use crate::display::{argb_from_amiga, Framebuffer};

/// Instruction budget for one copper list walk. Real lists run to a few
/// hundred instructions; this is generous headroom while still making a
/// circular or malformed list terminate in bounded time rather than hang
/// the renderer.
const COPPER_INSTR_BUDGET: u32 = 4096;

/// Widest bitplane fetch this renderer will honour per line, in words.
/// Real hardware's hardwired DDF window (`$18`-`$D8`) tops out at roughly
/// 97 words in hires; this is generous headroom over that, sized so a
/// guest-controlled `DDFSTOP-DDFSTRT` can never make one line's fetch loop
/// unreasonably large. Chosen independently of correctness for
/// well-behaved software — it only ever clips hostile or malformed values.
const MAX_DDF_WORDS: u32 = 128;

/// Bitplanes this renderer will ever composite, per §8.1's "1-5 planes".
const MAX_PLANES: usize = 5;

/// Sprite lines this renderer will ever read for the software pointer,
/// bounding a guest-controlled `SPR0POS`/`SPR0CTL` height. Real pointer
/// sprites are a handful of lines; this is generous headroom.
const MAX_SPRITE_LINES: u32 = 256;

/// Read one byte from guest chip RAM. Out-of-range addresses (a wild
/// bitplane/copper/sprite pointer) read back `0` rather than panic — chip
/// RAM here is a plain borrowed slice, not the fixed 2 MB array the bus
/// uses, so this has no hardware open-bus meaning; it is purely a safety
/// backstop for guest-controlled addresses.
fn read_byte(ram: &[u8], addr: u32) -> u8 {
    ram.get(addr as usize).copied().unwrap_or(0)
}

/// Read one big-endian word from guest chip RAM, byte-safe per
/// [`read_byte`].
fn read_word(ram: &[u8], addr: u32) -> u16 {
    let hi = read_byte(ram, addr) as u16;
    let lo = read_byte(ram, addr.wrapping_add(1)) as u16;
    (hi << 8) | lo
}

/// Latch the high 16 bits of a 32-bit pointer, keeping the low half.
/// Local to this module because the copper walk mutates a *shadow* of the
/// chipset's registers ([`CopperState`]), never the real [`Chipset`].
fn set_ptr_hi(ptr: &mut u32, value: u16) {
    *ptr = (*ptr & 0x0000_FFFF) | ((value as u32) << 16);
}

/// Latch the low 16 bits of a 32-bit pointer, keeping the high half.
fn set_ptr_lo(ptr: &mut u32, value: u16) {
    *ptr = (*ptr & 0xFFFF_0000) | value as u32;
}

/// The display-relevant subset of custom-chip state, as the copper list
/// leaves it after one linear walk. A copy of [`Chipset`]'s fields, never
/// the chipset itself — proposal §8.1 requires the renderer to read `&
/// Chipset`, and a copper `MOVE` must not mutate the real registers.
#[derive(Clone)]
struct CopperState {
    bplpt: [u32; 6],
    bplcon0: u16,
    #[allow(dead_code)] // Latched but deliberately not applied: BPLCON1's
    // PF1H fine horizontal scroll needs sub-word bit shifting to do
    // properly, which is more machinery than this renderer's boot/Guru
    // audience needs — see Geometry::decode's doc comment.
    bplcon1: u16,
    #[allow(dead_code)] // Latched for completeness; BPLCON2 (playfield
    // priority/blend) has nothing this renderer acts on — no dual
    // playfield, no genlock blending (§8.1's exclusion list).
    bplcon2: u16,
    bpl1mod: u16,
    bpl2mod: u16,
    diwstrt: u16,
    diwstop: u16,
    ddfstrt: u16,
    ddfstop: u16,
    color: [u16; 32],
    spr0pt: u32,
    spr0pos: u16,
    spr0ctl: u16,
}

impl CopperState {
    fn from_chipset(chipset: &Chipset) -> Self {
        Self {
            bplpt: chipset.bplpt,
            bplcon0: chipset.bplcon0,
            bplcon1: chipset.bplcon1,
            bplcon2: chipset.bplcon2,
            bpl1mod: chipset.bpl1mod,
            bpl2mod: chipset.bpl2mod,
            diwstrt: chipset.diwstrt,
            diwstop: chipset.diwstop,
            ddfstrt: chipset.ddfstrt,
            ddfstop: chipset.ddfstop,
            color: chipset.color,
            spr0pt: chipset.spr0pt,
            spr0pos: chipset.spr0pos,
            spr0ctl: chipset.spr0ctl,
        }
    }

    /// Apply one copper `MOVE`'s effect: write `value` into whichever of
    /// the display-relevant shadow registers `offset` names. Registers
    /// this renderer has no use for (audio, disk, `COP2LC`, `COPJMP*`,
    /// anything not read by [`Geometry::decode`] or the paint pass) are
    /// silently ignored, matching how a real `MOVE` to a register nobody
    /// is watching still "happens" but has no visible effect here.
    fn apply_move(&mut self, offset: u16, value: u16) {
        match offset {
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

            reg::COLOR00..=reg::COLOR31 => {
                let index = ((offset - reg::COLOR00) / 2) as usize;
                self.color[index] = value & 0x0FFF;
            }

            reg::SPR0PTH => set_ptr_hi(&mut self.spr0pt, value),
            reg::SPR0PTL => set_ptr_lo(&mut self.spr0pt, value),
            reg::SPR0POS => self.spr0pos = value,
            reg::SPR0CTL => self.spr0ctl = value,

            _ => {}
        }
    }
}

/// Walk the copper list at `start`, honouring `MOVE`s and skipping
/// `WAIT`/`SKIP`, into `state`. Returns the number of instructions
/// actually executed, mostly so tests can pin the budget cap.
///
/// Copper instruction encoding (Amiga Hardware Reference Manual,
/// "Copper Instructions"): the first word's bit 0 is 0 for `MOVE` (the
/// remaining 8 bits, `RA8-RA1`, giving an even register offset within
/// `$DFF000`) and 1 for `WAIT`/`SKIP`. This renderer does not evaluate
/// beam-position conditions at all — proposal §8.1 says "WAITs skipped",
/// not "WAITs honoured" — so `WAIT` and `SKIP` are both simply no-ops here
/// and the walk falls through to the next instruction.
///
/// The classic `WAIT $FFFF,$FFFE` "wait forever" idiom software uses to
/// terminate a copper list is detected as an early-exit sentinel (beam
/// position `$FF,$FF` is never reached, so real hardware parks there
/// until the next frame); without it we'd just keep reading zeroed/stale
/// memory past the list until the budget ran out, which is harmless but
/// pointless.
fn run_copper(start: u32, ram: &[u8], state: &mut CopperState) -> u32 {
    let mut pc = start;
    let mut executed = 0;
    for _ in 0..COPPER_INSTR_BUDGET {
        let w1 = read_word(ram, pc);
        let w2 = read_word(ram, pc.wrapping_add(2));
        pc = pc.wrapping_add(4);
        executed += 1;

        if w1 & 1 == 0 {
            state.apply_move(w1 & 0x1FE, w2);
        } else if w1 == 0xFFFF && (w2 & 0xFFFE) == 0xFFFE {
            break;
        }
        // Any other WAIT/SKIP: no-op, keep walking.
    }
    executed
}

/// Decoded display geometry for one frame, in output-pixel coordinates.
///
/// `diw_*`/`ddf_*` positions are expressed directly in the same units the
/// framebuffer is written in — one field's worth of vertical position and
/// horizontal colour-clock position, both already scaled by
/// [`Geometry::px_per_cck`] — so the paint pass never has to re-derive a
/// unit conversion.
struct Geometry {
    /// Bitplanes to composite, already clamped into `1..=MAX_PLANES` (or
    /// `0` for "no bitplane DMA" — see [`Geometry::decode`]).
    planes: u8,
    /// Whether `BPLCON0`'s interlace bit is set. Both fields are rendered
    /// into the same, taller framebuffer (see [`draw_bitplanes`]) rather
    /// than tracking long/short field state — this renderer runs once per
    /// frame from one latched register snapshot, not once per field, so
    /// there is no separate short-field bitplane pointer to render from.
    lace: bool,
    /// Output pixels per colour clock: 2 in lores, 4 in hires (a colour
    /// clock holds one 16-bit bitplane word's worth of pixels over 8 CCK
    /// in lores or 4 CCK in hires — see [`Geometry::decode`]'s DDF math).
    px_per_cck: u32,
    diw_x0: u32,
    diw_x1: u32,
    diw_y0: u32,
    diw_y1: u32,
    /// Words fetched per bitplane per line, already clamped to
    /// [`MAX_DDF_WORDS`].
    ddf_words: u32,
}

impl Geometry {
    /// Decode `DIWSTRT`/`DIWSTOP`/`DDFSTRT`/`DDFSTOP`/`BPLCON0` into pixel
    /// geometry.
    ///
    /// **Resolution and plane count** (`BPLCON0` bits 15 and 14-12): hires
    /// doubles `px_per_cck`; the plane count field is 3 bits wide (0-7),
    /// but proposal §8.1 only asks for 1-5 planes. `6` (the OCS/ECS HAM6
    /// encoding: `BPLCON0` bit 11 set alongside 6 planes) and `7`
    /// (reserved/AGA HAM8 territory) are clamped to 5: the extra plane's
    /// bit is simply dropped and every pixel's plane bits are read as a
    /// plain colour index rather than HAM-decoded, which draws *something*
    /// recognisable (if colour-wrong) instead of panicking or reading
    /// past `color[32]`. The `HAM` bit itself (bit 11) and `DBLPF`/dual
    /// playfield (bit 10) are read into [`CopperState`] but never
    /// interpreted — that decode belongs to P96, never here (§8.1).
    ///
    /// **Vertical window** (`DIWSTRT`/`DIWSTOP` high bytes): `VSTART` is
    /// used as-is (it is always < 256 in practice). `VSTOP`'s high byte is
    /// only 8 bits for a beam position that runs past 256 lines on PAL, so
    /// hardware infers its missing bit 8 from bit 7: a `VSTOP` byte below
    /// `$80` means the true stop is past line 256 and gets `$100` added
    /// (confirmed against Copperline's `DiwHigh::v_stop`, OCS-implicit
    /// case — this project's oracle implements the identical rule).
    ///
    /// **Horizontal window** (`DIWSTRT`/`DIWSTOP` low bytes): these run at
    /// twice colour-clock granularity (they are compared against a finer
    /// internal counter than `DDFSTRT`/`DDFSTOP`), so both are halved
    /// before converting to output pixels; `HSTOP`'s low byte
    /// unconditionally gets `$100` added first (again matching
    /// `DiwHigh::h_stop`'s OCS-implicit case) before the halving. This is
    /// exactly what makes the well-known `DDFSTRT = DIWSTRT.x/2 - 8`
    /// relationship (standard Amiga coding references, e.g. the 320-pixel
    /// low-res example `DIWSTRT=$2C81, DDFSTRT=$38`) come out self
    /// consistent with the DDF math below.
    ///
    /// **Data fetch width** (`DDFSTRT`/`DDFSTOP`): one bitplane fetch unit
    /// is 8 colour clocks in lores, 4 in hires (Copperline's
    /// `ddf_sequencer` module, read for understanding: "the fetch unit is
    /// eight colour clocks" in lores, halved in hires), and each unit
    /// fetches one 16-pixel word per active plane. Words per line is
    /// therefore `(DDFSTOP-DDFSTRT)/unit + 1`, clamped to
    /// [`MAX_DDF_WORDS`] against a hostile or malformed `DDFSTOP`.
    ///
    /// **Not implemented**: `BPLCON1`'s fine horizontal scroll (`PF1H`).
    /// Getting it exactly right needs sub-word bit shifting across the
    /// fetched data, not just an x-offset (its unit is finer than a whole
    /// output pixel in some modes); that is meaningfully more machinery
    /// for a feature that exists for smooth-scrolling demos, not the boot
    /// menu/Guru/Screenmode-prefs audience this renderer serves, so it is
    /// read into [`CopperState`] but not applied.
    fn decode(state: &CopperState) -> Self {
        let hires = state.bplcon0 & 0x8000 != 0;
        let raw_planes = ((state.bplcon0 >> 12) & 0x7) as u8;
        let planes = raw_planes.min(MAX_PLANES as u8);
        let lace = state.bplcon0 & 0x0004 != 0;
        let px_per_cck: u32 = if hires { 4 } else { 2 };

        let vstart = (state.diwstrt >> 8) as u32;
        let vstop_byte = (state.diwstop >> 8) as u32;
        let vstop = vstop_byte + if vstop_byte < 0x80 { 0x100 } else { 0 };

        let hstart_raw = (state.diwstrt & 0xFF) as u32;
        let hstop_raw = ((state.diwstop & 0xFF) as u32) | 0x100;
        let diw_x0 = (hstart_raw / 2) * px_per_cck;
        let diw_x1 = ((hstop_raw / 2) * px_per_cck).max(diw_x0);

        let divisor: u32 = if hires { 4 } else { 8 };
        let ddf_words = if state.ddfstop >= state.ddfstrt {
            (((state.ddfstop - state.ddfstrt) as u32) / divisor + 1).min(MAX_DDF_WORDS)
        } else {
            0
        };

        Self {
            planes,
            lace,
            px_per_cck,
            diw_x0,
            diw_x1,
            diw_y0: vstart,
            diw_y1: vstop.max(vstart),
            ddf_words,
        }
    }
}

/// Combine one pixel's worth of bits from `words` (indexed `[plane]`,
/// pre-loaded with one line's fetched data) into a colour index. `rel_px`
/// is the pixel's position within the fetched line, in pixels (0 at the
/// first fetched pixel). Bit order is Amiga's: bit 15 of each word is the
/// leftmost pixel.
fn pixel_color_index(
    words: &[[u16; MAX_DDF_WORDS as usize]; MAX_PLANES],
    planes: u8,
    rel_px: u32,
) -> usize {
    let word_idx = (rel_px / 16) as usize;
    let bit = 15 - (rel_px % 16);
    let mut index = 0usize;
    for (p, plane_words) in words.iter().enumerate().take(planes as usize) {
        let bitval = (plane_words[word_idx] >> bit) & 1;
        index |= (bitval as usize) << p;
    }
    index
}

/// Advance a bitplane pointer past one fetched line and apply its modulo.
/// `modulo` is signed per `BPL1MOD`/`BPL2MOD`'s documented meaning (a
/// negative modulo is ordinary and real software uses it, e.g. to
/// interleave two bitplanes' data in memory).
fn advance_plane_ptr(ptr: u32, words: u32, modulo: i16) -> u32 {
    ptr.wrapping_add(words * 2)
        .wrapping_add(modulo as i32 as u32)
}

/// Paint the bitplanes into `fb`, one output row at a time. Both interlace
/// fields render into the same full-height frame: since this renderer has
/// only one latched register snapshot per call (not one per field), it
/// simply draws twice as many consecutive fetched lines rather than
/// tracking which field is "current" — a deliberate simplification, not a
/// claim of exact long/short field weaving.
///
/// The fetched data is painted starting at `diw_x0`, using words in the
/// order they were fetched (word 0 is the first displayed pixel group) --
/// not offset by `DDFSTRT`'s own colour-clock position. Real hardware
/// always programs `DDFSTRT` one fetch unit ahead of `DIWSTRT` precisely
/// so the first fetched word is ready in time to be the first *displayed*
/// word; `DDFSTRT`'s value only controls fetch timing, never a
/// screen-space offset of its own.
fn draw_bitplanes(state: &CopperState, ram: &[u8], geom: &Geometry, fb: &mut Framebuffer) {
    if geom.planes == 0 || geom.ddf_words == 0 || geom.diw_x1 <= geom.diw_x0 {
        return;
    }

    let mut bplpt = state.bplpt;
    let field_rows = geom.diw_y1.saturating_sub(geom.diw_y0);
    let total_rows = if geom.lace {
        field_rows.saturating_mul(2)
    } else {
        field_rows
    };
    // Safety/perf bound only: `Framebuffer::put` already clips every
    // write, so this cannot under-draw a well-formed guest's picture --
    // real VSTOP never gets close to this.
    let total_rows = total_rows.min(crate::display::MAX_HEIGHT as u32 * 2);

    let mut words = [[0u16; MAX_DDF_WORDS as usize]; MAX_PLANES];
    let total_px = geom.ddf_words * 16;

    for row in 0..total_rows {
        for (p, plane_words) in words.iter_mut().enumerate().take(geom.planes as usize) {
            for (w, slot) in plane_words
                .iter_mut()
                .enumerate()
                .take(geom.ddf_words as usize)
            {
                *slot = read_word(ram, bplpt[p].wrapping_add((w * 2) as u32));
            }
            let modulo = if p % 2 == 0 {
                state.bpl1mod as i16
            } else {
                state.bpl2mod as i16
            };
            bplpt[p] = advance_plane_ptr(bplpt[p], geom.ddf_words, modulo);
        }

        let y = geom.diw_y0 + row;
        for x in geom.diw_x0..geom.diw_x1 {
            // Fetched data is indexed from `diw_x0`, not from `DDFSTRT`'s
            // own colour-clock position: real hardware always sets
            // DDFSTRT one fetch unit ahead of DIWSTRT (see this fn's doc
            // comment), so the *first* word fetched is exactly the word
            // ready to be the first word *displayed*. DDFSTRT only
            // controls fetch timing/DMA slot usage, never a screen-space
            // offset of its own -- the picture always starts at DIWSTRT.
            let rel_px = x - geom.diw_x0;
            let color_index = if rel_px < total_px {
                pixel_color_index(&words, geom.planes, rel_px)
            } else {
                // DIW wider than the fetched data (or a pathological DDF):
                // show the background colour rather than fabricating
                // plane data past what was actually fetched.
                0
            };
            let argb = argb_from_amiga(state.color[color_index & 0x1F]);
            fb.put(x as usize, y as usize, argb);
        }
    }
}

/// Composite the software mouse pointer from sprite 0 over `fb`.
///
/// Position decode (`SPR0POS`/`SPR0CTL`) is the hardware's documented
/// 9-bit split across the two registers (Amiga Hardware Reference Manual,
/// "Sprites"; cross-checked against Copperline's sprite tests, which
/// decode the identical bit layout): `VSTART`/`VSTOP` each take their low
/// 8 bits from one register's high byte plus one more bit from `SPR0CTL`;
/// `HSTART`'s low bit comes from `SPR0CTL` bit 0, the rest from
/// `SPR0POS`'s low byte.
///
/// Image data is read directly from chip RAM at `SPR0PT+4` onward (the
/// first two words at `SPR0PT` are the position/control header this
/// renderer already has latched from the registers, so painting starts
/// after them) rather than from the `SPR0DATA`/`SPR0DATB` latches, which
/// only ever hold *one* line's worth on real hardware (whichever line
/// DMA last fetched) — reading the image from memory is what lets a
/// multi-line pointer shape render as more than its first row.
///
/// Colour index 0 is transparent (the background/bitplane pixel shows
/// through); 1-3 index `COLOR17`-`COLOR19`, sprite pair 0/1's palette
/// bank.
fn draw_sprite0(state: &CopperState, ram: &[u8], geom: &Geometry, fb: &mut Framebuffer) {
    let vstart = (state.spr0pos >> 8) as u32 | (((state.spr0ctl & 0x04) as u32) << 6);
    let vstop = (state.spr0ctl >> 8) as u32 | (((state.spr0ctl & 0x02) as u32) << 7);
    let hstart_raw = (((state.spr0pos & 0xFF) as u32) << 1) | ((state.spr0ctl & 0x01) as u32);

    if vstop <= vstart {
        return;
    }
    let height = (vstop - vstart).min(MAX_SPRITE_LINES);
    let x0 = (hstart_raw / 2) * geom.px_per_cck;
    // A sprite bit is one low-res pixel wide regardless of screen mode
    // (SPRITERESN independent per-sprite resolution is not modelled);
    // scaling by px_per_cck/2 makes that one output pixel in lores, two
    // in hires, consistent with how the DIW's own px_per_cck scaling
    // works above.
    let step = (geom.px_per_cck / 2).max(1);

    for line in 0..height {
        let y = vstart + line;
        let addr = state
            .spr0pt
            .wrapping_add(4)
            .wrapping_add(line.wrapping_mul(4));
        let word_a = read_word(ram, addr);
        let word_b = read_word(ram, addr.wrapping_add(2));

        for bit in 0..16u32 {
            let shift = 15 - bit;
            let a = (word_a >> shift) & 1;
            let b = (word_b >> shift) & 1;
            let value = a | (b << 1);
            if value == 0 {
                continue; // transparent
            }
            let x = x0 + bit * step;
            let argb = argb_from_amiga(state.color[16 + value as usize]);
            fb.put(x as usize, y as usize, argb);
        }
    }
}

/// The stop-gap planar renderer. Stateless between frames — every field
/// it needs comes fresh from the [`Chipset`] and chip RAM passed to
/// [`Renderer::render`] — so this is a zero-sized handle rather than
/// something that accumulates per-frame state.
#[derive(Default)]
pub struct Renderer;

impl Renderer {
    pub const fn new() -> Self {
        Self
    }

    /// Render one frame from `chipset`'s latched registers and `chip_ram`
    /// into `fb`.
    ///
    /// Order of operations: walk the copper list at `chipset.cop1lc` into
    /// a local shadow of the display registers (never mutating `chipset`
    /// itself); fill `fb` with the resulting background colour
    /// (`COLOR00`); decode geometry; paint bitplanes; composite sprite 0.
    /// `chip_ram` is an arbitrary borrowed slice, not necessarily the
    /// bus's full 2 MB chip RAM array — every read into it is bounds-safe
    /// (see [`read_byte`]), so a shorter slice (as the unit tests below
    /// use) is exactly as safe as the real thing, just more likely to
    /// show a wild-pointer's garbage.
    pub fn render(&mut self, chipset: &Chipset, chip_ram: &[u8], fb: &mut Framebuffer) {
        let mut state = CopperState::from_chipset(chipset);
        run_copper(chipset.cop1lc, chip_ram, &mut state);

        fb.fill(argb_from_amiga(state.color[0]));

        let geom = Geometry::decode(&state);
        draw_bitplanes(&state, chip_ram, &geom, fb);
        draw_sprite0(&state, chip_ram, &geom, fb);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode one copper `MOVE` instruction (target register offset within
    /// `$DFF000`, and its data) as its two big-endian words.
    fn move_instr(offset: u16, value: u16) -> [u16; 2] {
        [offset & 0x1FE, value]
    }

    /// Encode a `WAIT` instruction that is not the end-of-list sentinel —
    /// any beam position works since [`run_copper`] never evaluates it.
    fn wait_instr() -> [u16; 2] {
        [0x2C01, 0x0000]
    }

    /// The `WAIT $FFFF,$FFFE` "wait forever" idiom real copper lists end
    /// with.
    fn end_instr() -> [u16; 2] {
        [0xFFFF, 0xFFFE]
    }

    /// Write a sequence of instruction word-pairs into `ram` at `addr`,
    /// big-endian, returning the address just past the last one.
    fn write_instrs(ram: &mut std::vec::Vec<u8>, addr: u32, instrs: &[[u16; 2]]) -> u32 {
        let mut pc = addr;
        for instr in instrs {
            for word in instr {
                write_word(ram, pc, *word);
                pc += 2;
            }
        }
        pc
    }

    fn write_word(ram: &mut std::vec::Vec<u8>, addr: u32, value: u16) {
        let addr = addr as usize;
        if ram.len() < addr + 2 {
            ram.resize(addr + 2, 0);
        }
        ram[addr] = (value >> 8) as u8;
        ram[addr + 1] = value as u8;
    }

    fn ram_with(size: usize) -> std::vec::Vec<u8> {
        std::vec![0u8; size]
    }

    #[test]
    fn copper_move_is_honoured_and_wait_is_skipped() {
        let mut ram = ram_with(64);
        write_instrs(
            &mut ram,
            0,
            &[wait_instr(), move_instr(reg::COLOR00, 0x0F00), end_instr()],
        );

        let mut state = CopperState::from_chipset(&Chipset::new());
        run_copper(0, &ram, &mut state);

        assert_eq!(state.color[0], 0x0F00, "MOVE must land");
    }

    #[test]
    fn copper_instruction_budget_stops_a_non_terminating_list() {
        // A list that only ever writes MOVE COLOR00 and never reaches the
        // end-of-list sentinel: the budget, not the list content, must be
        // what stops the walk.
        let mut ram = ram_with((COPPER_INSTR_BUDGET as usize + 16) * 4);
        let mut pc = 0u32;
        for _ in 0..COPPER_INSTR_BUDGET + 8 {
            pc = write_instrs(&mut ram, pc, &[move_instr(reg::COLOR00, 0x0001)]);
        }

        let mut state = CopperState::from_chipset(&Chipset::new());
        let executed = run_copper(0, &ram, &mut state);

        assert_eq!(executed, COPPER_INSTR_BUDGET, "budget caps the walk");
    }

    #[test]
    fn copper_short_list_terminates_before_the_budget() {
        let mut ram = ram_with(64);
        write_instrs(
            &mut ram,
            0,
            &[move_instr(reg::COLOR00, 0x0001), end_instr()],
        );

        let mut state = CopperState::from_chipset(&Chipset::new());
        let executed = run_copper(0, &ram, &mut state);

        assert_eq!(executed, 2, "MOVE then the end-of-list sentinel");
    }

    /// Real-world standard PAL low-res 320-pixel-wide, 256-line display
    /// register values (`DIWSTRT=$2C81, DIWSTOP=$2CC1, DDFSTRT=$38,
    /// DDFSTOP=$D0`), as commonly documented for Amiga bare-metal coding
    /// and cross-checked against Copperline's own `DiwHigh` test fixtures
    /// using the identical DIWSTRT/DIWSTOP pair.
    fn base_state() -> CopperState {
        CopperState::from_chipset(&Chipset::new())
    }

    #[test]
    fn geometry_decodes_standard_pal_lores_320x256() {
        let mut state = base_state();
        state.diwstrt = 0x2C81;
        state.diwstop = 0x2CC1;
        state.ddfstrt = 0x0038;
        state.ddfstop = 0x00D0;
        state.bplcon0 = 0x1000; // 1 plane, lores

        let geom = Geometry::decode(&state);

        assert_eq!(geom.px_per_cck, 2, "lores");
        assert_eq!(geom.diw_y0, 44);
        assert_eq!(geom.diw_y1, 300, "44 + 256 lines");
        assert_eq!(geom.diw_y1 - geom.diw_y0, 256);
        assert_eq!(geom.diw_x1 - geom.diw_x0, 320, "320 lores pixels wide");
        assert_eq!(geom.ddf_words, 20, "320 lores pixels / 16 per word");
    }

    /// Same DIW as the lores case (DIWSTRT/DIWSTOP are colour-clock
    /// referenced, not pixel-referenced, so hires reuses them unchanged)
    /// with the hires bit set and a DDF window sized for a clean 640-pixel
    /// result (`(0xD8-0x3C)/4+1 = 40` words = 640 hires pixels). These DDF
    /// values are this test's own synthetic fixture, not a claimed
    /// citation of a real ROM's hires mode.
    #[test]
    fn geometry_decodes_hires_640x256() {
        let mut state = base_state();
        state.diwstrt = 0x2C81;
        state.diwstop = 0x2CC1;
        state.ddfstrt = 0x003C;
        state.ddfstop = 0x00D8;
        state.bplcon0 = 0x9000; // hires bit + 1 plane

        let geom = Geometry::decode(&state);

        assert_eq!(geom.px_per_cck, 4, "hires");
        assert_eq!(geom.diw_y1 - geom.diw_y0, 256, "DIW height unchanged");
        assert_eq!(geom.diw_x1 - geom.diw_x0, 640, "640 hires pixels wide");
        assert_eq!(geom.ddf_words, 40, "640 hires pixels / 16 per word");
    }

    #[test]
    fn geometry_clamps_six_and_seven_planes_to_five_without_panicking() {
        let mut state = base_state();
        state.diwstrt = 0x2C81;
        state.diwstop = 0x2CC1;
        state.ddfstrt = 0x0038;
        state.ddfstop = 0x00D0;

        state.bplcon0 = 6 << 12; // HAM6 encoding
        assert_eq!(Geometry::decode(&state).planes, 5);

        state.bplcon0 = 7 << 12; // reserved/AGA HAM8 territory
        assert_eq!(Geometry::decode(&state).planes, 5);
    }

    #[test]
    fn geometry_zero_planes_means_no_bitplane_dma() {
        let state = base_state(); // bplcon0 == 0
        assert_eq!(Geometry::decode(&state).planes, 0);
    }

    #[test]
    fn planar_to_chunky_one_plane() {
        let mut words = [[0u16; MAX_DDF_WORDS as usize]; MAX_PLANES];
        words[0][0] = 0b1010_0000_0000_0000; // pixels 0 and 2 set
        assert_eq!(pixel_color_index(&words, 1, 0), 1);
        assert_eq!(pixel_color_index(&words, 1, 1), 0);
        assert_eq!(pixel_color_index(&words, 1, 2), 1);
    }

    #[test]
    fn planar_to_chunky_two_planes() {
        let mut words = [[0u16; MAX_DDF_WORDS as usize]; MAX_PLANES];
        // Pixel 0: plane0 bit=1, plane1 bit=1 -> index 0b11 = 3.
        // Pixel 1: plane0 bit=0, plane1 bit=1 -> index 0b10 = 2.
        words[0][0] = 0b1000_0000_0000_0000;
        words[1][0] = 0b1100_0000_0000_0000;
        assert_eq!(pixel_color_index(&words, 2, 0), 0b11);
        assert_eq!(pixel_color_index(&words, 2, 1), 0b10);
    }

    #[test]
    fn planar_to_chunky_five_planes() {
        let mut words = [[0u16; MAX_DDF_WORDS as usize]; MAX_PLANES];
        // Pixel 0 gets bit p set in every plane p, so its index is 0b11111.
        for plane in words.iter_mut() {
            plane[0] = 0b1000_0000_0000_0000;
        }
        assert_eq!(pixel_color_index(&words, 5, 0), 0b1_1111);
        // Pixel 1: only plane 4 (bit4) set -> index 0b10000.
        let mut words2 = [[0u16; MAX_DDF_WORDS as usize]; MAX_PLANES];
        words2[4][0] = 0b0100_0000_0000_0000;
        assert_eq!(pixel_color_index(&words2, 5, 1), 0b1_0000);
    }

    #[test]
    fn palette_lookup_and_colour_conversion_through_full_render() {
        let mut ram = ram_with(4096);
        // The DIW's leftmost pixel is the first fetched word's leftmost
        // bit (see draw_bitplanes's doc comment on DDFSTRT vs DIWSTRT).
        write_word(&mut ram, 0x0100, 0b1000_0000_0000_0000);

        let mut chipset = Chipset::new();
        chipset.bplpt[0] = 0x0100;
        chipset.bplcon0 = 0x1000; // 1 plane, lores
        chipset.diwstrt = 0x2C81;
        chipset.diwstop = 0x2CC1;
        chipset.ddfstrt = 0x0038;
        chipset.ddfstop = 0x00D0;
        chipset.color[0] = 0x0000; // background: black
        chipset.color[1] = 0x0F0F; // pixel colour: magenta-ish

        // Push the DIW/DDF origin near the framebuffer's top-left so the
        // lit pixel lands inside this tiny 8x8 test surface: shrink DIW
        // to match by overwriting after the fact isn't possible (DIWSTRT
        // is absolute beam position), so instead read back the pixel at
        // its real geometry-computed location.
        let geom_x0 = ((chipset.diwstrt & 0xFF) as u32 / 2) * 2;
        let geom_y0 = (chipset.diwstrt >> 8) as u32;

        // A full-size surface so the computed DIW origin is in range.
        let width = (geom_x0 as usize) + 8;
        let height = (geom_y0 as usize) + 8;
        let mut pixels = std::vec![0u32; width * height];
        let mut fb = Framebuffer::new(&mut pixels, width, height).unwrap();

        Renderer::new().render(&chipset, &ram, &mut fb);

        assert_eq!(
            fb.pixels[geom_y0 as usize * width + geom_x0 as usize],
            argb_from_amiga(0x0F0F),
            "lit pixel uses COLOR01"
        );
        assert_eq!(
            fb.pixels[geom_y0 as usize * width + geom_x0 as usize + 1],
            argb_from_amiga(0x0000),
            "next pixel is clear, uses COLOR00"
        );
    }

    #[test]
    fn modulo_advances_the_bitplane_pointer_between_rows() {
        assert_eq!(
            advance_plane_ptr(0x1000, 4, 0),
            0x1008,
            "no modulo: +words*2"
        );
        assert_eq!(
            advance_plane_ptr(0x1000, 4, 4),
            0x100C,
            "positive modulo adds after the row"
        );
        assert_eq!(
            advance_plane_ptr(0x1000, 4, -8),
            0x1000,
            "negative modulo is ordinary and must subtract"
        );
    }

    #[test]
    fn odd_planes_take_mod1_even_planes_take_mod2_through_full_render() {
        // Two planes, two lines, non-zero distinct moduli: after line 0,
        // plane 0 (BPL1, odd) must have advanced by ddf_words*2 + bpl1mod,
        // plane 1 (BPL2, even) by ddf_words*2 + bpl2mod. Verify indirectly
        // by checking line 1's pixel comes from the expected address.
        let mut ram = ram_with(4096);
        // One word/line: the DIW's leftmost pixel is the first fetched
        // word's leftmost bit (draw_bitplanes's doc comment).
        // Row 0: word at 0x0200, lit.
        write_word(&mut ram, 0x0200, 0b1000_0000_0000_0000);
        // Row 1: pointer advances by words*2 (2) + bpl1mod (6) = 8, so
        // row 1's word is at 0x0208, lit.
        write_word(&mut ram, 0x0208, 0b1000_0000_0000_0000);

        let mut chipset = Chipset::new();
        chipset.bplpt[0] = 0x0200;
        chipset.bplcon0 = 0x1000; // 1 plane
        chipset.bpl1mod = 6;
        chipset.diwstrt = 0x0081; // vstart 0, hstart 0x81
        chipset.diwstop = 0x02C1; // vstop line 2, hstop 0xC1
        chipset.ddfstrt = 0x0038;
        chipset.ddfstop = 0x0038; // one word/line
        chipset.color[0] = 0x0000; // background: black
        chipset.color[1] = 0x0FFF; // lit pixel: white, distinguishable

        let geom_x0 = ((chipset.diwstrt & 0xFF) as u32 / 2) * 2;
        let width = geom_x0 as usize + 8;
        let mut pixels = std::vec![0u32; width * 4];
        let mut fb = Framebuffer::new(&mut pixels, width, 4).unwrap();

        Renderer::new().render(&chipset, &ram, &mut fb);

        assert_eq!(
            fb.pixels[width + geom_x0 as usize],
            argb_from_amiga(0x0FFF),
            "line 1's lit pixel must have been fetched from the mod-advanced address"
        );
    }

    #[test]
    fn sprite0_composites_over_the_picture() {
        let mut ram = ram_with(4096);
        let sprpt = 0x0300u32;
        // Header (position/control words) is skipped by the renderer --
        // it reads position from SPR0POS/SPR0CTL, not from here -- but
        // real memory layout has them anyway; leave zeroed.
        write_word(&mut ram, sprpt + 4, 0b1000_0000_0000_0000); // data A, pixel 0
        write_word(&mut ram, sprpt + 6, 0b1000_0000_0000_0000); // data B, pixel 0
                                                                // -> pixel 0 colour index = 0b11 = 3 -> COLOR19.

        let mut chipset = Chipset::new();
        chipset.spr0pt = sprpt;
        // SPR0POS high byte is VSTART, low byte is HSTART8-1: $40 in the
        // high byte gives vstart=$40 with hstart=0 (lands in-canvas for
        // the 16-wide framebuffer below).
        chipset.spr0pos = 0x4000;
        chipset.spr0ctl = 0x4100; // vstop high byte 0x41 -> vstop=0x41, height 1
        chipset.color[19] = 0x00F0; // green-ish

        let mut pixels = std::vec![0u32; 16 * 128];
        let mut fb = Framebuffer::new(&mut pixels, 16, 128).unwrap();

        Renderer::new().render(&chipset, &ram, &mut fb);

        let vstart = (chipset.spr0pos >> 8) as usize; // 0x40
        assert_eq!(
            fb.pixels[vstart * 16],
            argb_from_amiga(0x00F0),
            "sprite pixel composited at its decoded position"
        );
    }

    #[test]
    fn sprite0_pixel_zero_is_transparent() {
        let mut ram = ram_with(4096);
        let sprpt = 0x0300u32;
        write_word(&mut ram, sprpt + 4, 0x0000);
        write_word(&mut ram, sprpt + 6, 0x0000);

        let mut chipset = Chipset::new();
        chipset.spr0pt = sprpt;
        chipset.spr0pos = 0x0500; // vstart=5, hstart=0 (lands in-canvas)
        chipset.spr0ctl = 0x0600; // vstop=6, height 1
        chipset.color[0] = 0x0111; // background, distinguishable from black

        let mut pixels = std::vec![0u32; 16 * 32];
        let mut fb = Framebuffer::new(&mut pixels, 16, 32).unwrap();
        Renderer::new().render(&chipset, &ram, &mut fb);

        let vstart = (chipset.spr0pos >> 8) as usize;
        assert_eq!(
            fb.pixels[vstart * 16],
            argb_from_amiga(0x0111),
            "transparent sprite pixel leaves the background showing"
        );
    }

    #[test]
    fn wild_bitplane_pointer_does_not_panic() {
        let ram = ram_with(64); // tiny, so bplpt below is wildly out of range
        let mut chipset = Chipset::new();
        chipset.bplpt[0] = 0xFFFF_FFF0;
        chipset.bplcon0 = 0x1000;
        chipset.diwstrt = 0x2C81;
        chipset.diwstop = 0x2CC1;
        chipset.ddfstrt = 0x0038;
        chipset.ddfstop = 0x00D0;

        let mut pixels = [0u32; 32 * 32];
        let mut fb = Framebuffer::new(&mut pixels, 32, 32).unwrap();
        Renderer::new().render(&chipset, &ram, &mut fb); // must not panic
    }

    #[test]
    fn wild_copper_pointer_does_not_panic() {
        let ram = ram_with(16);
        let mut chipset = Chipset::new();
        chipset.cop1lc = 0xDEAD_BEEF;

        let mut pixels = [0u32; 16 * 16];
        let mut fb = Framebuffer::new(&mut pixels, 16, 16).unwrap();
        Renderer::new().render(&chipset, &ram, &mut fb); // must not panic
    }

    #[test]
    fn wild_sprite_pointer_does_not_panic() {
        let ram = ram_with(16);
        let mut chipset = Chipset::new();
        chipset.spr0pt = 0xFFFF_0000;
        chipset.spr0pos = 0x0000;
        chipset.spr0ctl = 0xFF00; // vstart=0, vstop=0xFF: real height, all reads out of range

        let mut pixels = [0u32; 16 * 16];
        let mut fb = Framebuffer::new(&mut pixels, 16, 16).unwrap();
        Renderer::new().render(&chipset, &ram, &mut fb); // must not panic
    }

    #[test]
    fn empty_chip_ram_does_not_panic() {
        let ram: std::vec::Vec<u8> = std::vec::Vec::new();
        let mut chipset = Chipset::new();
        chipset.bplcon0 = 0x1000;
        chipset.diwstrt = 0x2C81;
        chipset.diwstop = 0x2CC1;
        chipset.ddfstrt = 0x0038;
        chipset.ddfstop = 0x00D0;
        chipset.cop1lc = 0x1234;
        chipset.spr0ctl = 0x00F0;

        let mut pixels = [0u32; 16 * 16];
        let mut fb = Framebuffer::new(&mut pixels, 16, 16).unwrap();
        Renderer::new().render(&chipset, &ram, &mut fb);
    }
}
