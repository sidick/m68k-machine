//! Stop-gap planar renderer (proposal §8.1).
//!
//! For visibility before P96 is installed — early startup menu, boot,
//! Gurus, Screenmode prefs — and as the permanent Guru/early-boot display.
//! Deliberately dumb: once per frame it walks the copper list linearly
//! (`MOVE`s honoured; a `COPJMP1`/`COPJMP2` strobe followed as the jump it
//! is; `WAIT`'s *vertical* position honoured so register state applies
//! from the scanline it names onward — see [`run_copper`]'s and [`Bands`]'s
//! doc comments), then renders each output row from whichever register
//! state was actually in force at that row's vertical position. Lores/
//! hires, 1-5 bitplanes, interlace, and a software mouse pointer from
//! sprite 0 are in scope.
//!
//! **What "honouring WAIT" does and does not mean here.** The scope line
//! is "everything needed to render an ordinary Amiga screen correctly"
//! versus "extras that exist for visual effect" (HAM, EHB, dual
//! playfield, per-line palette *tricks*, sprite dragging, per-pixel copper
//! effects, `SKIP`'s conditional behaviour, `WAIT`'s horizontal position).
//! A real boot/Guru/Screenmode-prefs list commonly does "planes on for a
//! band, off outside it" purely via vertical `WAIT`s — skipping that
//! produced a blank screen against a real Kickstart boot list, which is
//! why vertical `WAIT` moved from "skipped" to "honoured". It is still not
//! a copper simulation: horizontal position is treated as already
//! satisfied, `SKIP` still never skips, and only the vertical mask/compare
//! needed to place band boundaries correctly is implemented — see
//! [`wait_vertical_target`]'s doc comment for exactly how far that goes.
//!
//! **Not extended, ever** (§8.1). If a change here would need HAM/EHB
//! decode, a second playfield, or per-pixel copper effects, the answer is
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
use crate::cirrus::{DecodedMode, PixelDepth};
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

/// Vertically-distinct register-state bands one frame's copper walk will
/// track — see [`Bands`]'s doc comment for what a band is and why a fixed
/// cap is safe here. Real lists like Kickstart 3.2.2's boot list need
/// three: the state in force from line 0 (before any `WAIT`), "planes on"
/// from its `WAIT $63`, "planes off" from its `WAIT $F4`. This is generous
/// headroom over that.
const MAX_BANDS: usize = 16;

/// Output framebuffer pixels per colour clock, for *positioning and
/// sizing* the picture on the canvas -- always 2, regardless of hires/
/// lores. `DIWSTRT`/`DIWSTOP` (and sprite `HSTART`) describe a beam
/// position in colour-clock-referenced hardware timing units, which is
/// identical for the same physical screen area whether that area is
/// being scanned out in hires or lores; only the *density* of bitplane
/// data fetched within that area differs by resolution. A coordinator-
/// reported real-ROM capture caught what happens if the two are
/// conflated: multiplying the DIW's colour-clock position by hires'
/// *data* density (4 px/CCK) as well as lores' scaled every hires screen
/// to physically twice its lores-equivalent width and twice as far from
/// the left edge, overrunning [`crate::display::MAX_WIDTH`] and cropping
/// real content off the right edge, and stretching everything else
/// (a round pointer/logo bitmap rendered as a 2:1-wide ellipse). Pinning
/// this to a single resolution-independent constant is this renderer's
/// chosen fix -- see [`Geometry::decode_band`]'s doc comment for the
/// hires downsampling this implies on the data side.
const OUTPUT_PX_PER_CCK: u32 = 2;

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

/// `DMAEN` (bit 9): the DMA master enable. Without it, nothing on the
/// list below it (bitplane, sprite, disk, audio...) runs regardless of
/// its own individual enable bit.
const DMACON_DMAEN: u16 = 1 << 9;
/// `BPLEN` (bit 8): the bitplane DMA channel's own enable bit.
const DMACON_BPLEN: u16 = 1 << 8;
/// `DMACON`'s set/clear control bit (bit 15) -- same position and meaning
/// as `INTENA`/`INTREQ`'s (Amiga Hardware Reference Manual, "DMACON,
/// DMACONR"; `Chipset::write`'s private `apply_setclr` in `chipset.rs`
/// implements the identical rule for the real register, which this
/// module cannot call directly since it is a shadow, not the chipset).
const DMACON_SETCLR: u16 = 1 << 15;

/// Apply one `MOVE DMACON,value` the same way real hardware does: bit 15
/// of `value` selects whether the remaining bits are OR'd in (set) or
/// AND'd out (clear) of the current latch, rather than replacing it
/// outright -- unlike every other register this renderer shadows, whose
/// `MOVE` is a plain overwrite. Getting this wrong (treating `DMACON`
/// like `BPLCON0`, say) would make a `MOVE DMACON,$8300` (turn bitplane
/// DMA *on*, leaving every other bit alone) instead *replace* the whole
/// latch with exactly those bits, wiping out disk/audio/sprite/blitter
/// enables a real list set earlier and left alone. Bit-for-bit the same
/// rule as `chipset.rs`'s `apply_setclr` (kept in sync deliberately, not
/// shared code, because that one is private to the real `Chipset` and
/// this one operates on the copper walk's shadow).
fn apply_dmacon_setclr(current: u16, value: u16) -> u16 {
    let bits = value & 0x7FFF;
    if value & DMACON_SETCLR != 0 {
        current | bits
    } else {
        current & !bits
    }
}

/// Whether bitplane DMA is actually running, per real hardware's gate:
/// both the master enable (`DMAEN`) and the bitplane channel's own
/// enable (`BPLEN`) must be set (Amiga Hardware Reference Manual,
/// "DMACON, DMACONR"). Without this, `BPLxPT`/`BPLCON0` describe a
/// picture that is never actually fetched or displayed -- the screen
/// shows the background colour regardless of what those registers say,
/// exactly as if no planes were active at all. A real-world case this
/// matters for: a guest (observed with the vendored AROS pair, blocked
/// on `trackdisk.device` with no storage attached) that has programmed
/// `BPLxPT` and `BPLCON0` in preparation for a screen but never actually
/// enabled bitplane DMA -- on real hardware and in every other emulator
/// that screen is flat background; a renderer that draws bitplane data
/// regardless of this gate instead paints whatever uninitialised chip
/// RAM the unused pointers happen to address, which is a rendering bug,
/// not a faithful "the guest hasn't finished booting" picture.
fn bitplane_dma_enabled(dmacon: u16) -> bool {
    dmacon & (DMACON_DMAEN | DMACON_BPLEN) == (DMACON_DMAEN | DMACON_BPLEN)
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
    // audience needs — see Geometry::decode_band's doc comment. No real
    // startup/boot/Guru/Screenmode-prefs list this renderer has been
    // checked against depends on it for correct *positioning* (as opposed
    // to smooth-scrolling flourish); if one ever does, that tips it from
    // "extra" to "load-bearing" and this should be revisited.
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
    /// DMA control latch (`DMACON`), shadowed and updated the same as
    /// every other display register here — see [`apply_dmacon_setclr`]'s
    /// doc comment for why it needs the set/clear write convention rather
    /// than a plain overwrite, and [`bitplane_dma_enabled`] for how it
    /// gates the bitplane paint pass.
    dmacon: u16,
    /// Shadow of `COP1LC`/`COP2LC`, seeded from the real [`Chipset`] and
    /// updated by in-list `MOVE`s to `COP1LCH/L`/`COP2LCH/L` exactly like
    /// every other pointer field here. [`run_copper`] reads these — not
    /// `chipset.cop1lc`/`cop2lc` directly — when it follows a `COPJMP1`/
    /// `COPJMP2` strobe, because real hardware jumps to whatever COP*LC
    /// currently latches, including a value a list set for itself
    /// mid-walk (a stub commonly does exactly this: load COP2LC, then
    /// strobe COPJMP2).
    cop1lc: u32,
    cop2lc: u32,
    /// Only the sprite pointer is shadowed here, deliberately — see
    /// [`draw_sprite0`]'s doc comment for why `SPR0POS`/`SPR0CTL` are
    /// *not* latched into this state the way every other display
    /// register is: this machine has no sprite DMA engine, so those two
    /// registers are only ever whatever a `MOVE` last happened to leave
    /// in them, which for a standard AmigaOS pointer sprite is nothing
    /// at all (real hardware loads them autonomously from the memory
    /// `spr0pt` already points at). Carrying them here anyway — as
    /// `bplcon1`/`bplcon2` above are, "latched for completeness" — would
    /// invite exactly the bug this struct's history already had: reading
    /// a register that looks plausible but is unrelated to the sprite
    /// list the guest actually programmed.
    spr0pt: u32,
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
            dmacon: chipset.dmacon,
            cop1lc: chipset.cop1lc,
            cop2lc: chipset.cop2lc,
            spr0pt: chipset.spr0pt,
        }
    }

    /// Apply one copper `MOVE`'s effect: write `value` into whichever of
    /// the display-relevant shadow registers `offset` names. Registers
    /// this renderer has no use for (audio, disk, anything not read by
    /// [`Geometry::decode_band`] or the paint pass) are silently ignored,
    /// matching how a real `MOVE` to a register nobody is watching still
    /// "happens" but has no visible effect here. `COPJMP1`/`COPJMP2`
    /// themselves are handled by [`run_copper`], not here — a strobe
    /// changes control flow (which `apply_move` has no access to), it
    /// doesn't latch a value.
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

            reg::COP1LCH => set_ptr_hi(&mut self.cop1lc, value),
            reg::COP1LCL => set_ptr_lo(&mut self.cop1lc, value),
            reg::COP2LCH => set_ptr_hi(&mut self.cop2lc, value),
            reg::COP2LCL => set_ptr_lo(&mut self.cop2lc, value),

            reg::BPLCON0 => self.bplcon0 = value,
            reg::BPLCON1 => self.bplcon1 = value,
            reg::BPLCON2 => self.bplcon2 = value,
            reg::BPL1MOD => self.bpl1mod = value,
            reg::BPL2MOD => self.bpl2mod = value,
            reg::DIWSTRT => self.diwstrt = value,
            reg::DIWSTOP => self.diwstop = value,
            reg::DDFSTRT => self.ddfstrt = value,
            reg::DDFSTOP => self.ddfstop = value,
            reg::DMACON => self.dmacon = apply_dmacon_setclr(self.dmacon, value),

            reg::COLOR00..=reg::COLOR31 => {
                let index = ((offset - reg::COLOR00) / 2) as usize;
                self.color[index] = value & 0x0FFF;
            }

            reg::SPR0PTH => set_ptr_hi(&mut self.spr0pt, value),
            reg::SPR0PTL => set_ptr_lo(&mut self.spr0pt, value),
            // SPR0POS/SPR0CTL: deliberately not shadowed -- see
            // CopperState's doc comment on its `spr0pt` field and
            // draw_sprite0's doc comment for why a MOVE to either lands
            // nowhere here (matches how a MOVE to a register nobody is
            // watching still "happens" but has no visible effect, per
            // this fn's own doc comment above).
            _ => {}
        }
    }
}

/// The subset of [`CopperState`] that can meaningfully vary from one
/// vertical band to the next: everything [`Geometry::decode_band`] and
/// the bitplane paint pass read. Control-flow-only fields (`cop1lc`/
/// `cop2lc`) are not display state at all so are never carried per band.
/// `bplpt` is *not* carried per band either, deliberately: on real
/// hardware the bitplane pointer registers are advanced by the fetch
/// hardware itself every line, not by the copper, so [`draw_bitplanes`]
/// threads one continuously-advancing pointer set across every row
/// regardless of band boundaries, exactly mirroring that. A list that
/// explicitly re-`MOVE`s `BPLxPT` mid-frame (a split-screen technique) is
/// not represented by this — that is a deliberate effect a list chooses
/// to do, not something needed to render an ordinary screen correctly, so
/// it stays out per this module's doc comment on scope. `SPR0PT` is
/// likewise not banded, for the separate reason in [`draw_sprite0`]'s doc
/// comment: sprite DMA has no concept of a copper `WAIT` at all.
#[derive(Clone)]
struct BandState {
    bplcon0: u16,
    bplcon1: u16,
    bpl1mod: u16,
    bpl2mod: u16,
    diwstrt: u16,
    diwstop: u16,
    ddfstrt: u16,
    ddfstop: u16,
    color: [u16; 32],
    /// `DMACON`, banded like everything else here: a list can gate
    /// bitplane DMA on/off mid-list the same way it toggles `BPLCON0`'s
    /// plane count (proposal §8.1's own worked example does this with
    /// `BPLCON0`; nothing stops a list doing the same with `DMACON`), so
    /// this must be read from whichever band applies to a given row, not
    /// once from the chipset. See [`bitplane_dma_enabled`].
    dmacon: u16,
}

impl BandState {
    fn from_state(state: &CopperState) -> Self {
        Self {
            bplcon0: state.bplcon0,
            bplcon1: state.bplcon1,
            bpl1mod: state.bpl1mod,
            bpl2mod: state.bpl2mod,
            diwstrt: state.diwstrt,
            diwstop: state.diwstop,
            ddfstrt: state.ddfstrt,
            ddfstop: state.ddfstop,
            color: state.color,
            dmacon: state.dmacon,
        }
    }

    /// Placeholder content for a band slot that has not been finalized
    /// yet — every slot [`Bands`] ever exposes to a caller has been
    /// overwritten by [`Bands::open`] or [`Bands::finalize`] before use,
    /// this only exists so the fixed-size array has something to start
    /// from without needing heap allocation or an `Option`.
    fn zeroed() -> Self {
        Self {
            bplcon0: 0,
            bplcon1: 0,
            bpl1mod: 0,
            bpl2mod: 0,
            diwstrt: 0,
            diwstop: 0,
            ddfstrt: 0,
            ddfstop: 0,
            color: [0; 32],
            dmacon: 0,
        }
    }
}

/// Register state banded by scanline: proposal §8.1's copper walk given a
/// vertical dimension, so a list that reprograms the same registers at
/// different `WAIT` positions (Kickstart 3.2.2's real boot list does
/// exactly this — see this module's doc comment) renders each row from
/// the state actually in force there, instead of collapsing to whichever
/// `MOVE` happened to execute last.
///
/// A fixed-size array of `(start_line, state)` pairs, capped at
/// [`MAX_BANDS`] — this crate has no heap. Band 0 always starts at line 0
/// (`MOVE`s before the first `WAIT` must apply from the top of the frame)
/// and is the only band that exists until the walk crosses a real
/// vertical `WAIT`. Bands are appended, never inserted or reordered:
/// [`run_copper`]'s tracked vertical position only ever moves forward — a
/// `COPJMP` does not reset it, because the beam does not rewind just
/// because the copper's program counter jumped — so `start_line` is
/// monotonically increasing by construction and [`Bands::band_index_for_line`]
/// can rely on that.
///
/// **Overflow.** A list with more genuine vertical `WAIT` transitions than
/// [`MAX_BANDS`] does not panic or lose data: [`Bands::open`] keeps
/// updating the *last* band's snapshot for every `MOVE` that happens after
/// the cap is hit, it just stops opening new bands. The picture degrades
/// to however many distinct bands fit, folding the rest into the last one
/// — exactly the "last MOVE wins" collapse this whole feature exists to
/// avoid, just pushed out past a generous cap instead of happening on
/// list 1.
struct Bands {
    start_line: [u32; MAX_BANDS],
    state: [BandState; MAX_BANDS],
    count: usize,
}

impl Bands {
    fn new() -> Self {
        Self {
            start_line: [0; MAX_BANDS],
            state: core::array::from_fn(|_| BandState::zeroed()),
            count: 1,
        }
    }

    /// Finalize the currently-open band with `state` as it stands right
    /// now, then open a new one starting at `new_start` — unless the cap
    /// is already reached, in which case only the finalize happens (see
    /// this struct's doc comment on overflow). Callers only reach this for
    /// a `WAIT` that actually moves the tracked vertical position forward
    /// (`new_start` strictly greater than the currently-open band's
    /// start) — see [`run_copper`].
    fn open(&mut self, new_start: u32, state: &CopperState) {
        self.state[self.count - 1] = BandState::from_state(state);
        if self.count < MAX_BANDS {
            self.start_line[self.count] = new_start;
            self.count += 1;
        }
    }

    /// Finalize the currently-open band with `state` as it stands at the
    /// end of the walk — it covers everything from its start line to the
    /// bottom of the frame.
    fn finalize(&mut self, state: &CopperState) {
        self.state[self.count - 1] = BandState::from_state(state);
    }

    /// The band covering `line`: the last one whose `start_line` is `<=
    /// line`. `start_line` is monotonically increasing (this struct's doc
    /// comment), so the last match from the front is exactly the band
    /// whose range `line` falls into.
    fn band_index_for_line(&self, line: u32) -> usize {
        self.start_line[..self.count]
            .iter()
            .rposition(|&start| start <= line)
            .unwrap_or(0)
    }
}

/// Resolve the line at which a vertical `WAIT $VP,$VE` (mask honoured) is
/// satisfied, searching forward from `from_line` — never earlier, since
/// the tracked vertical position only ever moves forward within a walk
/// (see [`Bands`]'s doc comment).
///
/// `VE`'s bits mark which of `VP`'s bits the comparison actually checks
/// (Amiga Hardware Reference Manual, "The WAIT Instruction": "for each 0
/// [bit in VE], the comparison ... is always true"), so a genuine match
/// can lie anywhere up to 255 lines ahead of `from_line` — the search is
/// bounded at 256 steps, which is always enough whenever `ve != 0`: the
/// masked pattern `line & ve` is periodic with period dividing 256 in the
/// low 8 bits of the line count, so by the pigeonhole principle every
/// reachable masked value recurs within any 256 consecutive lines. This
/// exactly and exhaustively honours the mask (not an approximation of
/// it) for the 8-bit vertical range `WAIT` itself addresses; it is still
/// not a cycle-exact copper — horizontal position plays no part (proposal
/// §8.1's horizontal-WAIT exclusion), and this is a one-shot static
/// resolution of where a band boundary falls, not a per-cycle simulation.
fn wait_vertical_target(vp: u8, ve: u8, from_line: u32) -> u32 {
    let want = vp & ve;
    (from_line..)
        .take(256)
        .find(|&line| (line as u8) & ve == want)
        .unwrap_or(from_line) // unreachable for ve != 0 (see doc comment)
}

/// Walk the copper list at `start`, honouring `MOVE`s, a `COPJMP1`/
/// `COPJMP2` strobe, and the *vertical* position of `WAIT`, into `state`
/// and `bands`. Returns the number of instructions actually executed,
/// mostly so tests can pin the budget cap.
///
/// Copper instruction encoding (Amiga Hardware Reference Manual,
/// "Copper Instructions"): the first word's bit 0 is 0 for `MOVE` (the
/// remaining 8 bits, `RA8-RA1`, giving an even register offset within
/// `$DFF000`) and 1 for `WAIT`/`SKIP`; both share the same first-word
/// layout (`VP`/`RA8` overlap the same bits, `HP`). Their second words
/// share the same layout too (`VE`/`HE`/`BFD`), distinguished only by bit
/// 0: 0 for `WAIT`, 1 for `SKIP` ("The WAIT Instruction" / "The SKIP
/// Instruction" sections). `SKIP`'s conditional behaviour remains a pure
/// no-op here — proposal §8.1 keeps it an extra, not something an
/// ordinary screen depends on — so only the `w2 & 1 == 0` branch below
/// does anything beyond falling through.
///
/// **The vertical WAIT extension.** `VP` (first word, bits 15-8) and `VE`
/// (second word, bits 15-8) name the target line and its mask. `VE == 0`
/// means the comparison is unconditionally true (HRM, quoted above), so
/// such a `WAIT` is already satisfied the instant it runs — it never
/// opens a new band, same net effect as the old blanket "WAITs skipped"
/// for that case. Otherwise [`wait_vertical_target`] resolves the next
/// line the masked comparison matches at or after the walk's current
/// tracked position; if that is strictly forward of where the walk
/// already is, [`Bands::open`] closes the currently-open band there and
/// opens the next one. `HP`/`HE` play no part (§8.1's horizontal-WAIT
/// exclusion: horizontal position is treated as already satisfied).
///
/// The classic `WAIT $FFFF,$FFFE` "wait forever" idiom software uses to
/// terminate a copper list is detected as an early-exit sentinel (beam
/// position `$FF,$FF` is never reached, so real hardware parks there
/// until the next frame) and checked — and `break`s the walk — *before*
/// the vertical-WAIT logic above ever runs, so it can never be misread as
/// a real vertical `WAIT` and open a spurious band at line `$FF` (255).
///
/// **Following `COPJMP1`/`COPJMP2`**: on real hardware, a `MOVE` to either
/// strobe address (any value — the write itself is the trigger) makes the
/// copper's program counter jump to `COP1LC`/`COP2LC` immediately, same as
/// any other jump. Kickstart 3.2.2's boot screen relies on exactly this:
/// `COP1LC` permanently points at a small stub whose only job is to
/// strobe `COPJMP2`, handing control to the real screen list at `COP2LC`.
/// A walk that stopped at the stub (as an earlier version of this
/// function did) would never see that list at all. This is a genuine
/// control-flow jump, not a data-latching `MOVE`, so it is handled here
/// rather than in [`CopperState::apply_move`], which has no `pc` to
/// redirect. The jump only redirects `pc` — the tracked vertical position
/// (`line`, local to this function) is untouched by it, so a list that
/// jumps mid-frame does not rewind which bands it can still open (see
/// [`Bands`]'s doc comment).
///
/// A jump consumes exactly one instruction's worth of budget like any
/// other instruction (the `executed += 1` above already counts it before
/// this check runs) — a stub that jumps to a list that jumps back is a
/// natural, not even malicious, shape (this Kickstart's own stub is one
/// jump away from being exactly that if list 2 ever strobed `COPJMP1`
/// back), so the walk must still terminate in bounded time rather than
/// hang. `pc` is simply set from the shadow `cop1lc`/`cop2lc`, whatever
/// they currently hold (possibly nonsense a hostile or malformed list
/// wrote); every subsequent read through it goes through
/// [`read_word`]/[`read_byte`], which never panic on an out-of-range
/// address, so an off-the-rails `COP2LC` degrades to reading zeros rather
/// than crashing.
fn run_copper(start: u32, ram: &[u8], state: &mut CopperState, bands: &mut Bands) -> u32 {
    let mut pc = start;
    let mut executed = 0;
    let mut line = 0u32;

    for _ in 0..COPPER_INSTR_BUDGET {
        let w1 = read_word(ram, pc);
        let w2 = read_word(ram, pc.wrapping_add(2));
        pc = pc.wrapping_add(4);
        executed += 1;

        if w1 & 1 == 0 {
            let offset = w1 & 0x1FE;
            state.apply_move(offset, w2);
            match offset {
                reg::COPJMP1 => pc = state.cop1lc,
                reg::COPJMP2 => pc = state.cop2lc,
                _ => {}
            }
        } else if w1 == 0xFFFF && (w2 & 0xFFFE) == 0xFFFE {
            break;
        } else if w2 & 1 == 0 {
            // A genuine WAIT (SKIP has bit 0 of the second word set — see
            // this fn's doc comment).
            let vp = (w1 >> 8) as u8;
            let ve = (w2 >> 8) as u8;
            if ve != 0 {
                let target = wait_vertical_target(vp, ve, line);
                if target > line {
                    bands.open(target, state);
                    line = target;
                }
            }
            // ve == 0: already satisfied, no beam movement, no new band.
        }
        // SKIP: still a pure no-op here (see doc comment above).
    }

    bands.finalize(state);
    executed
}

/// Decoded display geometry for one band, in output-pixel coordinates.
///
/// `diw_*`/`ddf_*` positions are expressed directly in the same units the
/// framebuffer is written in — one field's worth of vertical position and
/// horizontal colour-clock position, both already scaled by
/// [`Geometry::px_per_cck`] — so the paint pass never has to re-derive a
/// unit conversion.
struct Geometry {
    /// Bitplanes to composite, already clamped into `1..=MAX_PLANES` (or
    /// `0` for "no bitplane DMA" — see [`Geometry::decode_band`]).
    planes: u8,
    /// Whether `BPLCON0`'s interlace bit is set. Both fields are woven
    /// into the same, taller framebuffer as interleaved output rows, each
    /// field reading from its own advancing bitplane pointer — see
    /// [`draw_bitplanes`]'s doc comment on interlace for how the odd
    /// field's starting offset and per-field row skip are derived.
    lace: bool,
    /// Output framebuffer pixels per colour clock -- always
    /// [`OUTPUT_PX_PER_CCK`], regardless of hires/lores; see that
    /// constant's doc comment for why this is fixed rather than
    /// resolution-dependent.
    px_per_cck: u32,
    diw_x0: u32,
    diw_x1: u32,
    diw_y0: u32,
    diw_y1: u32,
    /// Words fetched per bitplane per line, already clamped to
    /// [`MAX_DDF_WORDS`].
    ddf_words: u32,
    /// How many *fetched* bits collapse into one output pixel: 1 in
    /// lores (native — one fetched bit already is one output pixel's
    /// worth of physical width), 2 in hires (nearest-neighbour
    /// decimation, since hires fetches twice the bit density the output
    /// canvas has room for at [`OUTPUT_PX_PER_CCK`] — see
    /// [`Geometry::decode_band`]'s doc comment). [`draw_bitplanes`]
    /// multiplies an output-relative pixel index by this to find which
    /// fetched bit to read.
    bits_per_output_px: u32,
    /// Which *fetched* bit the display window's leftmost pixel reads —
    /// the fetch-to-display alignment. `0` for the standard register
    /// relationships (`DDFSTRT = $38`/`$3C` with `DIWSTRT` h `$81`,
    /// no scroll), where the first fetched bit is exactly the first
    /// displayed one; positive when the guest fetches earlier than the
    /// window opens (the early words are fetched but never shown);
    /// negative when the window opens before any fetched data exists
    /// (extreme overscan — those pixels show background). See
    /// [`Geometry::decode_band`]'s doc comment for the derivation and
    /// the real-ROM capture that required it.
    fetch_bit_shift: i32,
}

impl Geometry {
    /// Convenience for callers holding a full [`CopperState`] rather than
    /// one band's [`BandState`] slice of it — delegates to
    /// [`Geometry::decode_band`], which is what
    /// [`draw_bitplanes`]/[`draw_sprite0`] actually call, once per band.
    /// Only this module's own tests hold a bare `CopperState` with no
    /// [`Bands`] around it.
    #[cfg(test)]
    fn decode(state: &CopperState) -> Self {
        Self::decode_band(&BandState::from_state(state))
    }

    /// Decode `DIWSTRT`/`DIWSTOP`/`DDFSTRT`/`DDFSTOP`/`BPLCON0` into pixel
    /// geometry.
    ///
    /// **Resolution and plane count** (`BPLCON0` bits 15 and 14-12): hires
    /// sets `bits_per_output_px` to 2 (see [`OUTPUT_PX_PER_CCK`]'s and
    /// [`Geometry::bits_per_output_px`]'s doc comments for why the output
    /// canvas's own scale, `px_per_cck`, stays fixed instead of doubling);
    /// the plane count field is 3 bits wide (0-7), but proposal §8.1 only
    /// asks for 1-5 planes. `6` (the OCS/ECS HAM6
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
    /// `(DDFSTOP-DDFSTRT)/unit + 1` in lores, but `.../unit + 2` in hires:
    /// hires' DMA fetch pipeline is one word deeper (the standard 640-wide
    /// example `DDFSTRT=$3C, DDFSTOP=$D4` gives `(0xD4-0x3C)/4 + 2 = 40`
    /// words, matching the well-known real value — using lores' `+1` on
    /// the same pair would give 39 and, worse, under-advance the bitplane
    /// pointer by one word every row, which is exactly the "picture
    /// sheared into diagonal streaks" symptom a real-ROM capture caught:
    /// each row's fetch starts two bytes short of where the previous
    /// row's real data actually ended, so the misalignment compounds row
    /// by row instead of being a one-off error. Clamped to
    /// [`MAX_DDF_WORDS`] against a hostile or malformed `DDFSTOP`.
    ///
    /// **Fetch-to-display alignment** (`fetch_bit_shift`): the first
    /// *displayed* bit is not always the first *fetched* bit. Denise
    /// starts showing bitplane data when the beam reaches `DIWSTRT`'s
    /// horizontal position; Agnus starts fetching at `DDFSTRT`. With the
    /// standard relationship (`DDFSTRT = $3C` hires / `$38` lores against
    /// `DIWSTRT` h `$81`, `BPLCON1 = 0`) the pipeline delivers fetched
    /// bit 0 exactly as the window opens, and the picture's placement
    /// moves *linearly* from there: one colour clock of earlier `DDFSTRT`
    /// shows 2 more lores (4 more hires) bits of pre-window data, one
    /// `DIWSTRT` h unit of later window start skips one lores (two hires)
    /// fetched bits, and each `BPLCON1` `PF1H` scroll step delays the
    /// data by one lores pixel, i.e.
    /// `shift = (hstart - $81 - 2*(ddfstrt - std_ddf) - pf1h) * bits_per_output_px`
    /// fetched bits (Copperline's `fetch_origin_native_shift`, read for
    /// understanding, implements exactly this linear model relative to
    /// the same calibrated standard case). Kickstart 47's boot screen is
    /// the real capture that required it: `DIWSTRT h=$95, DDFSTRT=$40,
    /// BPLCON1=$44` gives `shift = (20 - 8 - 4) * 2 = 16` — the first
    /// fetched word (which, with its `BPL1MOD = -6` and 38-word fetch
    /// over 70-byte rows, holds the *previous* row's rightmost floppy-
    /// graphic bytes) is fetched but never displayed. Painting it, as
    /// this function did before the shift existed, put a tan sliver of
    /// the floppy's right edge at the window's left edge, one row down.
    ///
    /// **Partially implemented**: `BPLCON1` enters only through this
    /// whole-output-pixel shift, applied from `PF1H` to *all* planes.
    /// Per-playfield split scroll (`PF2H` differing from `PF1H`) and
    /// sub-output-pixel positioning are not modelled — smooth-scroll
    /// machinery this renderer's boot-screen audience never exercises
    /// (real system screens program both nibbles equal, as Kickstart's
    /// `$44` does).
    fn decode_band(b: &BandState) -> Self {
        let hires = b.bplcon0 & 0x8000 != 0;
        let raw_planes = ((b.bplcon0 >> 12) & 0x7) as u8;
        let planes = raw_planes.min(MAX_PLANES as u8);
        let lace = b.bplcon0 & 0x0004 != 0;
        // Output canvas positioning always uses OUTPUT_PX_PER_CCK (see
        // its doc comment) -- never a resolution-doubled value here.
        let px_per_cck: u32 = OUTPUT_PX_PER_CCK;
        // A hires fetch packs 4 px/CCK worth of bits into the same 16-bit
        // word a lores fetch packs 2 px/CCK into, i.e. hires delivers
        // twice as many bits per unit of physical screen width as the
        // (fixed) output canvas has pixels for. This renderer's chosen
        // reconciliation (a real-ROM shear report's fix, see this fn's
        // doc comment) is nearest-neighbour decimation: keep every other
        // fetched bit, dropping the rest, rather than plotting all of
        // them at the canvas's fixed lores-equivalent density and
        // overrunning it.
        let bits_per_output_px: u32 = if hires { 2 } else { 1 };

        let vstart = (b.diwstrt >> 8) as u32;
        let vstop_byte = (b.diwstop >> 8) as u32;
        let vstop = vstop_byte + if vstop_byte < 0x80 { 0x100 } else { 0 };

        let hstart_raw = (b.diwstrt & 0xFF) as u32;
        let hstop_raw = ((b.diwstop & 0xFF) as u32) | 0x100;
        let diw_x0 = (hstart_raw / 2) * px_per_cck;
        let diw_x1 = ((hstop_raw / 2) * px_per_cck).max(diw_x0);

        // Bitplane fetch runs in 8-colour-clock units, two words per unit
        // per plane in hires and one in lores. A DDFSTOP that falls part
        // way through a unit is still honoured at the following unit
        // boundary, and one further unit drains the pipeline — hence the
        // round-up and the trailing unit.
        //
        // The arithmetic has to be done in units rather than directly in
        // words: dividing the colour-clock span by 4 and adding 2 gives
        // the right answer only when DDFSTOP happens to land on a unit
        // boundary. Kickstart's $D4 does, which is why its screens render
        // correctly under the simpler formula; AROS programs $D0, which
        // does not, and came out one word short per row — a shortfall
        // that compounds down the frame into a diagonal shear.
        let ddf_words = if b.ddfstop >= b.ddfstrt {
            let units = ((b.ddfstop - b.ddfstrt) as u32).div_ceil(8) + 1;
            (units * if hires { 2 } else { 1 }).min(MAX_DDF_WORDS)
        } else {
            0
        };

        // Fetch-to-display alignment, linear relative to the calibrated
        // standard register relationship -- see this fn's doc comment.
        // All three terms are in lores-pixel units (one DIWSTRT h unit)
        // before scaling to fetched bits: DDFSTRT counts whole colour
        // clocks, hence its factor of 2.
        let std_ddf: i32 = if hires { 0x3C } else { 0x38 };
        let pf1h = (b.bplcon1 & 0xF) as i32;
        let fetch_bit_shift = (hstart_raw as i32 - 0x81 - 2 * (b.ddfstrt as i32 - std_ddf) - pf1h)
            * bits_per_output_px as i32;

        Self {
            planes,
            lace,
            px_per_cck,
            diw_x0,
            diw_x1,
            diw_y0: vstart,
            diw_y1: vstop.max(vstart),
            ddf_words,
            bits_per_output_px,
            fetch_bit_shift,
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

/// Paint the bitplanes into `fb`, one output row at a time, using
/// whichever [`BandState`] in `bands` is in force at each row's vertical
/// position (see [`Bands`]'s doc comment for why that can differ from row
/// to row). `lace` is taken as the union across every band (any band
/// requesting interlace doubles the row count for the whole picture)
/// since it is a display-mode bit, not something that varies meaningfully
/// mid-frame.
///
/// **Interlace.** On real hardware an interlaced screen is two distinct
/// passes over chip RAM, not one: the even field starts fetching at
/// `BPLxPT` exactly as programmed, the odd field starts one physical row
/// further into the same buffer, and each field's own modulo is
/// programmed large enough to skip its partner's rows so the two fields'
/// data, read back to back, weave into one full-height picture rather
/// than each independently repeating the same half-height image (Amiga
/// Hardware Reference Manual, interlace/`BPLxMOD` chapters). AROS's
/// no-boot-media screen is the concrete case this fixes: `BPL1MOD`/
/// `BPL2MOD = $50` (80 bytes) on a 40-word (80-byte) row means each
/// field's own per-line advance is `ddf_words*2 (the normal fetch) +
/// modulo` = 160 bytes = two rows' worth, so a single field only ever
/// touches every other memory row -- exactly the classic interlaced
/// layout, where consecutive memory rows hold consecutive *physical* scan
/// lines and alternate between the two fields. The bug this replaces drew
/// `field_rows*2` rows from *one* continuously-advancing pointer, which
/// does not skip anything: it duplicates the first `field_rows` rows'
/// worth of image once, then keeps reading straight on past the picture
/// into whatever chip RAM follows it for the second half -- precisely the
/// "picture twice, noise in between" symptom this fixes.
///
/// [`odd_field_bplpt`] derives the odd field's starting offset from
/// `ddf_words` — the fetched row's own byte width — rather than a
/// hard-coded constant, so it is correct for any screen's row stride, not
/// just AROS's 40-word one.
///
/// **Field order** is fixed (the even field — `BPLxPT` exactly as
/// programmed — always occupies output rows 0, 2, 4, ...; the odd field
/// rows 1, 3, 5, ...) rather than keyed off `Chipset`'s `VPOSR`
/// long/short-field toggle (`lof`). Two reasons: `lof` is a private field
/// of `Chipset` with no accessor, and this renderer's file-ownership
/// boundary for this change is `render.rs` alone, so reading it would
/// mean touching a file outside that boundary for a purely cosmetic
/// choice; and a fixed order is strictly *more* stable for this
/// renderer's actual audience (a boot/Guru screen, effectively static
/// frame to frame) than toggling with `lof` would be — toggling trades a
/// one-row jitter every frame for hardware fidelity that a static picture
/// cannot show the benefit of. If a future caller needs field order to
/// track real `lof` (e.g. for genlock framing), `Chipset` would need a
/// public accessor first; nothing here forecloses that.
///
/// **Vertical extent** is the union of every band's own `DIWSTRT`/
/// `DIWSTOP`-decoded range rather than one band's alone: real lists
/// program `DIWSTRT`/`DIWSTOP` once, before any `WAIT` that matters here,
/// so in practice every band reports the same range and the union is
/// exactly that range regardless of which band happens to be picked.
///
/// **Bitplane pointers are not banded** — see [`BandState`]'s doc comment
/// for why: `bplpt` is seeded once from `state` (the fully-walked
/// [`CopperState`], i.e. wherever `BPLxPT` was last `MOVE`d) and then
/// advanced by this function row by row exactly as real fetch hardware
/// advances it, regardless of which band a given row falls in — one
/// running pointer per field, both seeded from that same `state.bplpt`
/// (the odd field additionally offset by [`odd_field_bplpt`]). A row
/// whose band has `planes == 0`, or whose `DMACON` does not have both
/// `DMAEN` and `BPLEN` set ([`bitplane_dma_enabled`]), does not advance
/// that row's field pointers at all — real bitplane DMA is not fetching
/// during a blanked band, or while the guest hasn't turned bitplane DMA
/// on at all, either — and is instead painted solid with that band's own
/// `COLOR00`, so a later band's background (Kickstart's "planes off,
/// blanking below the picture" band, for instance) shows correctly even
/// though [`Renderer::render`]'s upfront fill only covers band 0's
/// colour. The `DMACON` gate matters beyond Kickstart's screen: a guest
/// that has programmed `BPLxPT`/`BPLCON0` in preparation for a screen but
/// never actually enabled bitplane DMA (observed with the vendored AROS
/// pair, blocked on `trackdisk.device` with no storage attached) must
/// show flat background, matching real hardware and every other
/// emulator, not whatever uninitialised chip RAM those unused pointers
/// happen to address.
///
/// The fetched data is painted starting at `diw_x0`, with the window's
/// leftmost pixel reading fetched bit [`Geometry::fetch_bit_shift`] — 0
/// under the standard `DDFSTRT`/`DIWSTRT` relationship, where the first
/// fetched word arrives exactly as the window opens, but *not* in
/// general: a guest that fetches earlier than the window opens (as
/// Kickstart 47's boot screen does, `DDFSTRT=$40` with `DIWSTRT h=$95`
/// and scroll `$44`) has its leading fetched word(s) discarded by the
/// window comparator on real hardware, never shown. See
/// [`Geometry::decode_band`]'s doc comment for the model and the
/// left-edge artefact that assuming "fetched bit 0 is displayed bit 0"
/// produced.
fn draw_bitplanes(state: &CopperState, ram: &[u8], bands: &Bands, fb: &mut Framebuffer) {
    let mut y0 = u32::MAX;
    let mut y1 = 0u32;
    let mut lace = false;
    for band_state in &bands.state[..bands.count] {
        let g = Geometry::decode_band(band_state);
        if g.diw_y1 > g.diw_y0 {
            y0 = y0.min(g.diw_y0);
            y1 = y1.max(g.diw_y1);
        }
        lace |= g.lace;
    }
    if y0 >= y1 {
        return; // no band ever established a non-empty display window
    }

    let field_rows = y1 - y0;
    let total_rows = if lace {
        field_rows.saturating_mul(2)
    } else {
        field_rows
    };
    // Safety/perf bound only: `Framebuffer::put` already clips every
    // write, so this cannot under-draw a well-formed guest's picture --
    // real VSTOP never gets close to this.
    let total_rows = total_rows.min(crate::display::MAX_HEIGHT as u32 * 2);

    // Two independently-advancing pointer sets, one per field -- see this
    // fn's doc comment on interlace. In the non-interlaced case `field[1]`
    // is simply never touched (every row uses field index 0), so this
    // costs nothing when `lace` is false.
    let mut bplpt = [state.bplpt, state.bplpt];
    if lace {
        let top_band = bands.band_index_for_line(y0);
        let top_geom = Geometry::decode_band(&bands.state[top_band]);
        bplpt[1] = odd_field_bplpt(state.bplpt, top_geom.ddf_words);
    }
    let mut words = [[0u16; MAX_DDF_WORDS as usize]; MAX_PLANES];

    for row in 0..total_rows {
        // Interleave: even output rows are the even field's next line,
        // odd output rows the odd field's next line (see this fn's doc
        // comment on field order). Non-interlaced: `field` is always 0
        // and `field_line` is just `row`, identical to the pre-interlace
        // behaviour.
        let field = if lace { (row & 1) as usize } else { 0 };
        let field_line = if lace { row / 2 } else { row };
        let band_line = y0 + field_line;
        let y = y0 + row;

        let bidx = bands.band_index_for_line(band_line);
        let bstate = &bands.state[bidx];
        let geom = Geometry::decode_band(bstate);

        if geom.planes == 0
            || geom.ddf_words == 0
            || geom.diw_x1 <= geom.diw_x0
            || !bitplane_dma_enabled(bstate.dmacon)
        {
            // This row's band has no bitplane DMA in effect -- paint its
            // own background across the whole row (see this fn's doc
            // comment) and do not advance this field's bplpt: real fetch
            // hardware is not running during this band either.
            let argb = argb_from_amiga(bstate.color[0]);
            for x in 0..fb.width {
                fb.put(x, y as usize, argb);
            }
            continue;
        }

        for (p, plane_words) in words.iter_mut().enumerate().take(geom.planes as usize) {
            for (w, slot) in plane_words
                .iter_mut()
                .enumerate()
                .take(geom.ddf_words as usize)
            {
                *slot = read_word(ram, bplpt[field][p].wrapping_add((w * 2) as u32));
            }
            let modulo = if p % 2 == 0 {
                bstate.bpl1mod as i16
            } else {
                bstate.bpl2mod as i16
            };
            bplpt[field][p] = advance_plane_ptr(bplpt[field][p], geom.ddf_words, modulo);
        }

        let total_px = geom.ddf_words as i64 * 16;
        for x in geom.diw_x0..geom.diw_x1 {
            // `src_px` is the fetched bit this output pixel reads: the
            // window-relative position scaled by `bits_per_output_px`
            // (2 in hires -- nearest-neighbour decimation to fit hires'
            // denser data into the canvas's fixed OUTPUT_PX_PER_CCK
            // scale, see Geometry::decode_band's doc comment), plus the
            // fetch-to-display alignment `fetch_bit_shift` (see this
            // fn's and Geometry::decode_band's doc comments).
            let rel_px = x - geom.diw_x0;
            let src_px =
                rel_px as i64 * geom.bits_per_output_px as i64 + geom.fetch_bit_shift as i64;
            let color_index = if (0..total_px).contains(&src_px) {
                pixel_color_index(&words, geom.planes, src_px as u32)
            } else {
                // Window pixels with no fetched data behind them: before
                // the first fetched bit (window opens ahead of the data,
                // negative src_px) or past the last (DIW wider than the
                // fetch, or a pathological DDF). Show the background
                // colour rather than fabricating plane data.
                0
            };
            let argb = argb_from_amiga(bstate.color[color_index & 0x1F]);
            fb.put(x as usize, y as usize, argb);
        }
    }
}

/// Starting bitplane pointers for interlace's odd field: `even`, each
/// advanced by one row's worth of fetched bytes (`ddf_words` words, i.e.
/// `ddf_words * 2` bytes per plane). Derived from the row's own fetch
/// width rather than a hard-coded constant so it is correct for any
/// screen's row stride (see [`draw_bitplanes`]'s doc comment on
/// interlace for why one row's stride is exactly the right offset: on
/// real hardware the odd field starts fetching one physical row after the
/// even field, and consecutive rows in a normal (row-major) bitmap buffer
/// are exactly one row's byte width apart).
fn odd_field_bplpt(even: [u32; 6], ddf_words: u32) -> [u32; 6] {
    let row_bytes = ddf_words * 2;
    even.map(|ptr| ptr.wrapping_add(row_bytes))
}

/// Composite the software mouse pointer from sprite 0 over `fb`.
///
/// Position decode (`SPR0POS`/`SPR0CTL`) is the hardware's documented
/// 9-bit split across the two registers (Amiga Hardware Reference Manual,
/// "Sprites"; cross-checked against Copperline's sprite tests, which
/// decode the identical bit layout): `VSTART`/`VSTOP` each take their low
/// 8 bits from one register's high byte plus one more bit from `SPR0CTL`;
/// `HSTART`'s low bit comes from `SPR0CTL` bit 0, the rest from
/// `SPR0POS`'s low byte. **The two words this decode reads come from chip
/// RAM at `SPR0PT`/`SPR0PT+2`, not from the chipset's own `SPR0POS`/
/// `SPR0CTL` registers.** On real hardware those registers are not
/// independently programmed for a standard AmigaOS pointer sprite at
/// all: sprite DMA autonomously *loads* them from exactly these two
/// words at the start of each frame, before fetching the image data that
/// follows. This machine has no sprite DMA engine (proposal §7.1:
/// `SPR*` is "latched; consumed by renderer", nothing fetches on its
/// own), so the chipset's `SPR0POS`/`SPR0CTL` fields only ever hold
/// whatever a `MOVE` happened to leave in them — for Kickstart 3.2.2's
/// boot-alert pointer, observed to be nothing at all (`SPR0CTL` sits at
/// a stale, unrelated CPU-written value, never touched by the copper
/// list that sets up the pointer). An earlier version of this function
/// read those chipset registers directly and got exactly that failure
/// mode: `SPR0CTL`'s stale value decoded to a ~255-line `VSTOP`, so the
/// real ~16-line pointer image was drawn correctly for its own rows and
/// then kept going for another 239 rows into whatever chip RAM followed
/// it. Reading the header from the same memory real hardware would have
/// fetched it from is the fix, and also makes this function agree with
/// itself: it was already reading the *image* data from `SPR0PT+4`
/// onward, memory-relative, while reading the *position* from registers
/// that need not correspond to that same memory at all.
///
/// Image data is read directly from chip RAM at `SPR0PT+4` onward (the
/// first two words at `SPR0PT` are the position/control header decoded
/// above, so painting starts after them) rather than from the
/// `SPR0DATA`/`SPR0DATB` latches, which only ever hold *one* line's
/// worth on real hardware (whichever line DMA last fetched) — reading
/// the image from memory is what lets a multi-line pointer shape render
/// as more than its first row.
///
/// **Not banded by vertical WAIT**: sprite DMA has no concept of a copper
/// `WAIT` at all — a real sprite's height/position come entirely from its
/// own header in memory, decoded above, never from where the copper
/// happened to be in its list. This function instead looks up whichever
/// band covers the sprite's own decoded `vstart` line (via
/// [`Bands::band_index_for_line`]) purely to pick the `BPLCON0`
/// (hires/lores, for pixel scaling) and palette in force at that point in
/// the frame — the one place sprite compositing does still depend on
/// banded state.
///
/// Colour index 0 is transparent (the background/bitplane pixel shows
/// through); 1-3 index `COLOR17`-`COLOR19`, sprite pair 0/1's palette
/// bank.
fn draw_sprite0(state: &CopperState, ram: &[u8], bands: &Bands, fb: &mut Framebuffer) {
    let pos_word = read_word(ram, state.spr0pt);
    let ctl_word = read_word(ram, state.spr0pt.wrapping_add(2));
    let vstart = (pos_word >> 8) as u32 | (((ctl_word & 0x04) as u32) << 6);
    let vstop = (ctl_word >> 8) as u32 | (((ctl_word & 0x02) as u32) << 7);
    let hstart_raw = (((pos_word & 0xFF) as u32) << 1) | ((ctl_word & 0x01) as u32);

    if vstop <= vstart {
        return;
    }
    let height = (vstop - vstart).min(MAX_SPRITE_LINES);

    let bidx = bands.band_index_for_line(vstart);
    let bstate = &bands.state[bidx];
    let geom = Geometry::decode_band(bstate);

    let x0 = (hstart_raw / 2) * geom.px_per_cck;
    // A sprite bit is one low-res pixel wide regardless of screen mode
    // (SPRITERESN independent per-sprite resolution is not modelled).
    // Since the whole output canvas is now always at OUTPUT_PX_PER_CCK's
    // fixed lores-equivalent scale (Geometry::decode_band's doc comment
    // on why hires doesn't widen `px_per_cck`), a sprite bit is simply
    // one output pixel, in every screen mode -- no per-resolution
    // scaling needed here at all any more.
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
            let argb = argb_from_amiga(bstate.color[16 + value as usize]);
            fb.put(x as usize, y as usize, argb);
        }
    }
}

/// Present a Graffity/Cirrus RTG framebuffer: proposal §8.2's production
/// display path, in its first-light form (module doc comment of the
/// caller that wires this up, `machine-hosted`'s `screenshot.rs`).
///
/// This is a *board present*, not a copper-driven scanout like
/// [`Renderer::render`] above — there is no copper list, no `WAIT`
/// banding, no bitplanes. [`crate::cirrus::Cirrus542x::decoded_mode`] has
/// already done all the register decoding (geometry, stride, panning
/// start offset, pixel depth) and validated that the described scanout
/// fits inside `vram`; this function's only job is to walk that already-
/// validated rectangle and convert each pixel to `0xAARRGGBB`.
///
/// **`palette` is a closure, not a borrowed `&Cirrus542x`/`&Graffity`**,
/// so this stays generic over whichever board owns the chip rather than
/// coupling `render.rs` to `graffity.rs`'s or `cirrus.rs`'s types beyond
/// what it already imports ([`DecodedMode`]/[`PixelDepth`]) — the same
/// reason [`Renderer::render`] takes `&Chipset` rather than a whole
/// `MachineBus`.
///
/// **Bpp8** (what the oracle's `FakeNativeModes` chunky mode actually
/// programs, and the only depth reachable before P96Mode negotiates
/// something richer): each VRAM byte is a palette index, looked up
/// through `palette` — the whole conversion.
///
/// **Bpp15/16/24: implemented, but unverified against a real driver.**
/// No P96 driver in this project's test corpus has yet been observed to
/// program anything but Bpp8 (see this function's caller's own doc
/// comment), so these three branches are this renderer's best-effort
/// guess at the standard VGA/Cirrus little-endian layouts —
/// **XRGB1555** for 15bpp, **RGB565** for 16bpp (5/6/5 guns, the
/// conventional split when the extra bit goes to green), and packed
/// **BGR888** for 24bpp (byte order low-to-high B,G,R, matching how a
/// VGA-lineage RAMDAC's 24bpp mode is conventionally wired) — never
/// cross-checked against a register capture the way [`Cirrus542x`]'s own
/// decode was. Treat a picture through one of these three paths as
/// plausible, not confirmed.
///
/// Every VRAM access goes through bounds-checked slice indexing
/// (`vram.get`, defaulting to `0` past the end) even though
/// `decoded_mode`'s own contract already guarantees the described
/// rectangle fits — the same defence-in-depth [`read_byte`]/[`read_word`]
/// apply to copper-sourced pointers above, cheap insurance against this
/// function ever being called with a `mode` that didn't actually come
/// from the same VRAM's own `decoded_mode()`.
pub fn render_rtg(
    mode: &DecodedMode,
    vram: &[u8],
    palette: impl Fn(u8) -> u32,
    fb: &mut Framebuffer,
) {
    let width = (mode.width as usize).min(fb.width);
    let height = (mode.height as usize).min(fb.height);

    let expand5 = |c: u16| -> u32 {
        let c = c as u32;
        (c << 3) | (c >> 2)
    };
    let expand6 = |c: u16| -> u32 {
        let c = c as u32;
        (c << 2) | (c >> 4)
    };

    for y in 0..height {
        let row_start = mode.start_offset as usize + y * mode.stride_bytes as usize;
        for x in 0..width {
            let argb = match mode.depth {
                PixelDepth::Bpp8 => {
                    let index = vram.get(row_start + x).copied().unwrap_or(0);
                    palette(index)
                }
                PixelDepth::Bpp15 => {
                    let off = row_start + x * 2;
                    let lo = vram.get(off).copied().unwrap_or(0) as u16;
                    let hi = vram.get(off + 1).copied().unwrap_or(0) as u16;
                    let v = lo | (hi << 8);
                    let r = (v >> 10) & 0x1F;
                    let g = (v >> 5) & 0x1F;
                    let b = v & 0x1F;
                    0xFF00_0000 | (expand5(r) << 16) | (expand5(g) << 8) | expand5(b)
                }
                PixelDepth::Bpp16 => {
                    let off = row_start + x * 2;
                    let lo = vram.get(off).copied().unwrap_or(0) as u16;
                    let hi = vram.get(off + 1).copied().unwrap_or(0) as u16;
                    let v = lo | (hi << 8);
                    let r = (v >> 11) & 0x1F;
                    let g = (v >> 5) & 0x3F;
                    let b = v & 0x1F;
                    0xFF00_0000 | (expand5(r) << 16) | (expand6(g) << 8) | expand5(b)
                }
                PixelDepth::Bpp24 => {
                    let off = row_start + x * 3;
                    let b = vram.get(off).copied().unwrap_or(0) as u32;
                    let g = vram.get(off + 1).copied().unwrap_or(0) as u32;
                    let r = vram.get(off + 2).copied().unwrap_or(0) as u32;
                    0xFF00_0000 | (r << 16) | (g << 8) | b
                }
            };
            fb.put(x, y, argb);
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
    /// itself), following any `COPJMP1`/`COPJMP2` strobe the list itself
    /// executes and honouring each `WAIT`'s vertical position to produce
    /// [`Bands`] of register state (see [`run_copper`]'s doc comment —
    /// Kickstart 3.2.2's real boot screen lives entirely behind one such
    /// jump, with distinct on/off bands either side of its two `WAIT`s);
    /// fill `fb` with band 0's background colour (`COLOR00`, in force
    /// from line 0); paint bitplanes row by row from whichever band
    /// applies to each row; composite sprite 0. `chip_ram` is an
    /// arbitrary borrowed slice, not necessarily the bus's full 2 MB chip
    /// RAM array — every read into it is bounds-safe (see [`read_byte`]),
    /// so a shorter slice (as the unit tests below use) is exactly as
    /// safe as the real thing, just more likely to show a wild-pointer's
    /// garbage.
    pub fn render(&mut self, chipset: &Chipset, chip_ram: &[u8], fb: &mut Framebuffer) {
        let mut state = CopperState::from_chipset(chipset);
        let mut bands = Bands::new();
        run_copper(chipset.cop1lc, chip_ram, &mut state, &mut bands);

        // Background: band 0's COLOR00, the state in force from line 0.
        // draw_bitplanes additionally repaints any row whose own band has
        // no bitplane DMA with that band's own COLOR00 (see its doc
        // comment), so a later band's background still shows correctly
        // even though this upfront fill only covers band 0's.
        fb.fill(argb_from_amiga(bands.state[0].color[0]));

        draw_bitplanes(&state, chip_ram, &bands, fb);
        draw_sprite0(&state, chip_ram, &bands, fb);
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

    /// Encode a `WAIT` instruction whose vertical compare is disabled
    /// (`VE = 0`, so it is unconditionally already satisfied — see
    /// [`run_copper`]'s doc comment) — any beam position works since with
    /// `VE = 0` it is never evaluated.
    fn wait_instr() -> [u16; 2] {
        [0x2C01, 0x0000]
    }

    /// Encode a genuine vertical `WAIT $VP,$VE` (horizontal position/mask
    /// left at 0, i.e. always-satisfied, per §8.1's horizontal-WAIT
    /// exclusion). Second word's bit 0 is 0, marking `WAIT` rather than
    /// `SKIP` (see [`run_copper`]'s doc comment).
    fn wait_v(vp: u8, ve: u8) -> [u16; 2] {
        [((vp as u16) << 8) | 0x0001, (ve as u16) << 8]
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
    fn copper_move_is_honoured_and_a_disabled_wait_is_a_noop() {
        let mut ram = ram_with(64);
        write_instrs(
            &mut ram,
            0,
            &[wait_instr(), move_instr(reg::COLOR00, 0x0F00), end_instr()],
        );

        let mut state = CopperState::from_chipset(&Chipset::new());
        let mut bands = Bands::new();
        run_copper(0, &ram, &mut state, &mut bands);

        assert_eq!(state.color[0], 0x0F00, "MOVE must land");
        assert_eq!(bands.count, 1, "a VE=0 WAIT never opens a band");
    }

    /// `MOVE DMACON,value` must use the set/clear convention (bit 15
    /// selects OR-in vs AND-out), not a plain overwrite -- a `MOVE
    /// DMACON,$8300` (bitplane DMA on) after an earlier `MOVE
    /// DMACON,$8020` (some unrelated channel on) must leave *both* on,
    /// not replace the first with the second.
    #[test]
    fn copper_move_to_dmacon_uses_the_setclr_convention() {
        let mut ram = ram_with(64);
        write_instrs(
            &mut ram,
            0,
            &[
                move_instr(reg::DMACON, 0x8020), // set: some other channel (bit 5)
                move_instr(reg::DMACON, 0x8300), // set: DMAEN | BPLEN, bit 5 untouched
                end_instr(),
            ],
        );

        let mut state = CopperState::from_chipset(&Chipset::new());
        let mut bands = Bands::new();
        run_copper(0, &ram, &mut state, &mut bands);

        assert_eq!(
            state.dmacon, 0x0320,
            "both MOVEs' bits (0x20 and 0x300) must be set, not the second replacing the first"
        );

        // A clear (bit 15 low) must only clear the named bits.
        write_instrs(&mut ram, 8, &[move_instr(reg::DMACON, 0x0200), end_instr()]);
        run_copper(8, &ram, &mut state, &mut bands);
        assert_eq!(
            state.dmacon, 0x0120,
            "clearing DMAEN (0x200) must leave BPLEN (0x100) and the other channel (0x20) set"
        );
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
        let mut bands = Bands::new();
        let executed = run_copper(0, &ram, &mut state, &mut bands);

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
        let mut bands = Bands::new();
        let executed = run_copper(0, &ram, &mut state, &mut bands);

        assert_eq!(executed, 2, "MOVE then the end-of-list sentinel");
    }

    /// The shape this whole feature exists for: Kickstart 3.2.2's real
    /// `COP1LC` is a stub whose only job is to strobe `COPJMP2` and hand
    /// control to the real screen list at `COP2LC` (see this module's doc
    /// comment on [`run_copper`]). `COP2LC` here mimics the CPU having
    /// already latched it via `COP2LCH`/`COP2LCL` writes, which is how
    /// Kickstart actually programs it — the stub list itself contains no
    /// `MOVE` to `COP2LC*`, only the strobe.
    #[test]
    fn copjmp2_follows_to_cop2lc_and_its_moves_take_effect() {
        let mut ram = ram_with(256);
        // Stub list at address 0: strobe COPJMP2 and nothing else.
        write_instrs(&mut ram, 0, &[move_instr(reg::COPJMP2, 0x0000)]);
        // List 2 at 0x40: an ordinary MOVE, then the end-of-list sentinel.
        write_instrs(
            &mut ram,
            0x40,
            &[move_instr(reg::COLOR00, 0x0ABC), end_instr()],
        );

        let mut chipset = Chipset::new();
        chipset.cop2lc = 0x40;
        let mut state = CopperState::from_chipset(&chipset);
        let mut bands = Bands::new();
        let executed = run_copper(0, &ram, &mut state, &mut bands);

        assert_eq!(state.color[0], 0x0ABC, "list 2's MOVE must take effect");
        assert_eq!(executed, 3, "strobe, list 2's MOVE, list 2's end sentinel");
    }

    /// A stub that jumps to a list that jumps back is a natural shape, not
    /// even a hostile one (this Kickstart's own stub is one stray
    /// `COPJMP1` away from being exactly this) — the budget, not list
    /// content, must be what stops it.
    #[test]
    fn copjmp_induced_cycle_terminates_on_budget_not_hang() {
        let mut ram = ram_with(256);
        // List 1 at 0: strobe COPJMP2.
        write_instrs(&mut ram, 0, &[move_instr(reg::COPJMP2, 0x0000)]);
        // List 2 at 0x40: strobe COPJMP1, bouncing back to list 1 forever.
        write_instrs(&mut ram, 0x40, &[move_instr(reg::COPJMP1, 0x0000)]);

        let mut chipset = Chipset::new();
        chipset.cop1lc = 0x0;
        chipset.cop2lc = 0x40;
        let mut state = CopperState::from_chipset(&chipset);
        let mut bands = Bands::new();
        let executed = run_copper(0, &ram, &mut state, &mut bands);

        assert_eq!(
            executed, COPPER_INSTR_BUDGET,
            "budget, not the cycle, stops the walk"
        );
    }

    /// A `COP2LC` pointing at nonsense (never programmed, or corrupted)
    /// must degrade to reading zeros through the existing
    /// [`read_word`]/[`read_byte`] bounds check, exactly like a wild
    /// `COP1LC`/`BPLPT` already does — never panic.
    #[test]
    fn cop2lc_outside_chip_ram_degrades_safely_without_panic() {
        let mut ram = ram_with(16);
        write_instrs(&mut ram, 0, &[move_instr(reg::COPJMP2, 0x0000)]);

        let mut chipset = Chipset::new();
        chipset.cop2lc = 0xFFFF_0000; // wildly out of range
        let mut state = CopperState::from_chipset(&chipset);
        let mut bands = Bands::new();
        let executed = run_copper(0, &ram, &mut state, &mut bands); // must not panic

        assert_eq!(
            executed, COPPER_INSTR_BUDGET,
            "reads-as-zero past the jump never hits the end sentinel, so \
             the budget is what stops it"
        );
    }

    /// Two genuine vertical `WAIT`s must open two more bands beyond the
    /// initial one, each carrying whichever `MOVE`s executed before it —
    /// exactly the shape that makes Kickstart's "planes on for a band,
    /// off outside it" boot list render correctly instead of collapsing
    /// to its last `MOVE`.
    #[test]
    fn two_vertical_waits_produce_two_distinct_bands() {
        let mut ram = ram_with(256);
        write_instrs(
            &mut ram,
            0,
            &[
                move_instr(reg::COLOR00, 0x0111),
                wait_v(0x40, 0xFF),
                move_instr(reg::COLOR00, 0x0222),
                wait_v(0x80, 0xFF),
                move_instr(reg::COLOR00, 0x0333),
                end_instr(),
            ],
        );

        let mut state = CopperState::from_chipset(&Chipset::new());
        let mut bands = Bands::new();
        run_copper(0, &ram, &mut state, &mut bands);

        assert_eq!(bands.count, 3);
        assert_eq!(bands.start_line[0], 0);
        assert_eq!(bands.state[0].color[0], 0x0111);
        assert_eq!(bands.start_line[1], 0x40);
        assert_eq!(bands.state[1].color[0], 0x0222);
        assert_eq!(bands.start_line[2], 0x80);
        assert_eq!(bands.state[2].color[0], 0x0333);
    }

    /// `MOVE`s before the first `WAIT` must be visible from line 0 --
    /// band 0 always starts at line 0 and must carry them.
    #[test]
    fn moves_before_first_wait_apply_from_line_zero() {
        let mut ram = ram_with(64);
        write_instrs(
            &mut ram,
            0,
            &[
                move_instr(reg::BPLCON0, 0x1000),
                wait_v(0x50, 0xFF),
                end_instr(),
            ],
        );
        let mut state = CopperState::from_chipset(&Chipset::new());
        let mut bands = Bands::new();
        run_copper(0, &ram, &mut state, &mut bands);

        assert_eq!(bands.start_line[0], 0);
        assert_eq!(
            bands.state[0].bplcon0, 0x1000,
            "pre-WAIT MOVE lands in band 0, from line 0"
        );
    }

    /// The terminal `WAIT $FFFF,$FFFE` sentinel must not be read as a
    /// real vertical `WAIT` and open a spurious band at line `$FF` (255).
    #[test]
    fn end_of_list_sentinel_does_not_open_a_spurious_band() {
        let mut ram = ram_with(64);
        write_instrs(
            &mut ram,
            0,
            &[move_instr(reg::COLOR00, 0x0ABC), end_instr()],
        );
        let mut state = CopperState::from_chipset(&Chipset::new());
        let mut bands = Bands::new();
        run_copper(0, &ram, &mut state, &mut bands);

        assert_eq!(
            bands.count, 1,
            "the wait-forever sentinel must not be read as a real vertical WAIT"
        );
        assert_eq!(bands.start_line[0], 0);
    }

    /// A `COPJMP` must not rewind the tracked vertical position. If it
    /// wrongly did (resetting the walk's notion of "current line" back to
    /// 0), a second `WAIT` for the *same* line list 1 already reached
    /// would look like a fresh forward move and open a duplicate band at
    /// that same start line; with the fix, it is recognised as already
    /// satisfied (target == the already-established line, not strictly
    /// greater) and opens nothing.
    #[test]
    fn copjmp_does_not_rewind_the_vertical_position() {
        let mut ram = ram_with(256);
        // List 1 at 0: wait for line 0x80, then strobe COPJMP2.
        write_instrs(
            &mut ram,
            0,
            &[wait_v(0x80, 0xFF), move_instr(reg::COPJMP2, 0x0000)],
        );
        // List 2 at 0x40: WAIT for that *same* line 0x80 again, then a
        // MOVE. If COPJMP had rewound the tracked position to 0, this
        // WAIT would recompute a "forward" target of 0x80 and wrongly
        // open a second, duplicate band there.
        write_instrs(
            &mut ram,
            0x40,
            &[
                wait_v(0x80, 0xFF),
                move_instr(reg::COLOR00, 0x0DEF),
                end_instr(),
            ],
        );

        let mut chipset = Chipset::new();
        chipset.cop2lc = 0x40;
        let mut state = CopperState::from_chipset(&chipset);
        let mut bands = Bands::new();
        run_copper(0, &ram, &mut state, &mut bands);

        assert_eq!(
            bands.count, 2,
            "the beam does not rewind, so a repeat WAIT for the same line \
             opens no new (duplicate) band"
        );
        assert_eq!(bands.start_line[1], 0x80);
        assert_eq!(
            bands.state[1].color[0], 0x0DEF,
            "list 2's MOVE still lands, in the one band that's open"
        );
    }

    /// A list with more genuine vertical `WAIT` transitions than
    /// [`MAX_BANDS`] must not panic or overflow the fixed-size arrays --
    /// it degrades to the cap, folding the remaining transitions' `MOVE`s
    /// into the last band (see [`Bands`]'s doc comment on overflow).
    #[test]
    fn band_overflow_degrades_safely_without_panic() {
        let mut ram = ram_with(4096);
        let mut instrs: std::vec::Vec<[u16; 2]> = std::vec::Vec::new();
        let total = MAX_BANDS as u16 + 8;
        for i in 0..total {
            instrs.push(wait_v((i * 4) as u8, 0xFF));
            instrs.push(move_instr(reg::COLOR00, i));
        }
        instrs.push(end_instr());
        write_instrs(&mut ram, 0, &instrs);

        let mut state = CopperState::from_chipset(&Chipset::new());
        let mut bands = Bands::new();
        run_copper(0, &ram, &mut state, &mut bands); // must not panic

        assert_eq!(
            bands.count, MAX_BANDS,
            "band count caps at MAX_BANDS, never overflows the fixed array"
        );
        assert_eq!(
            bands.state[MAX_BANDS - 1].color[0],
            total - 1,
            "overflow MOVEs still land, just folded into the last band"
        );
    }

    /// Regression test for the wider render path: a stub-then-jump list
    /// end to end through [`Renderer::render`], matching the shape of the
    /// real Kickstart 3.2.2 A1200 no-boot-media screen (COP1LC's stub
    /// strobes COPJMP2; the real BPLCON0/geometry lives behind it) — must
    /// not panic and must reflect list 2's state, not the stub's.
    #[test]
    fn render_follows_copjmp2_to_the_real_screen_list() {
        let mut ram = ram_with(4096);
        write_instrs(&mut ram, 0, &[move_instr(reg::COPJMP2, 0x0000)]);
        write_instrs(
            &mut ram,
            0x40,
            &[
                move_instr(reg::BPLCON0, 0x1000), // 1 plane, lores
                move_instr(reg::COLOR00, 0x0123),
                end_instr(),
            ],
        );

        let mut chipset = Chipset::new();
        chipset.cop1lc = 0x0;
        chipset.cop2lc = 0x40;
        // Stub's own BPLCON0 stays zero-planes; only list 2 sets it.
        assert_eq!(chipset.bplcon0, 0);

        let mut pixels = [0u32; 16 * 16];
        let mut fb = Framebuffer::new(&mut pixels, 16, 16).unwrap();
        Renderer::new().render(&chipset, &ram, &mut fb); // must not panic

        assert_eq!(
            fb.pixels[0],
            argb_from_amiga(0x0123),
            "background colour must come from list 2, behind the jump"
        );
    }

    /// A copper list whose bands genuinely differ (planes on/off) must
    /// render each row from the band in force there: the "on" band's row
    /// shows its bitplane data/palette, the "off" band's row shows flat
    /// The `DMACON` gate ([`bitplane_dma_enabled`]): the exact same
    /// `BPLxPT`/`BPLCON0`/data must render as pure background when
    /// bitplane DMA is off, and as the real bitplane content when it's
    /// on. Regression for the AROS finding -- a guest that has
    /// programmed bitplane pointers and a plane count in preparation for
    /// a screen but never actually enabled bitplane DMA (`DMACON`'s
    /// `DMAEN`/`BPLEN`, bits 9/8) must show flat background like real
    /// hardware, not whatever the unused pointers happen to address.
    #[test]
    fn dmacon_gates_bitplane_drawing_same_data_dma_off_vs_on() {
        let mut ram = ram_with(4096);
        // Lit bitplane data at the pointer -- if this is what gets drawn,
        // the DMA gate failed to suppress it.
        write_word(&mut ram, 0x0100, 0b1000_0000_0000_0000);

        let mut chipset = Chipset::new();
        chipset.bplpt[0] = 0x0100;
        chipset.bplcon0 = 0x1000; // 1 plane, lores
        chipset.diwstrt = 0x2C81;
        chipset.diwstop = 0x2CC1;
        chipset.ddfstrt = 0x0038;
        chipset.ddfstop = 0x00D0;
        chipset.color[0] = 0x0000; // background: black
        chipset.color[1] = 0x0F0F; // what the lit pixel would be, if drawn

        let geom_x0 = ((chipset.diwstrt & 0xFF) as u32 / 2) * 2;
        let geom_y0 = (chipset.diwstrt >> 8) as u32;
        let width = (geom_x0 as usize) + 8;
        let height = (geom_y0 as usize) + 8;

        // DMACON off (default: Chipset::new()'s dmacon is 0 -- DMAEN/BPLEN
        // both clear): must render pure background everywhere, including
        // right where the lit bitplane data lives.
        chipset.dmacon = 0x0000;
        let mut pixels_off = std::vec![0u32; width * height];
        let mut fb_off = Framebuffer::new(&mut pixels_off, width, height).unwrap();
        Renderer::new().render(&chipset, &ram, &mut fb_off);
        assert_eq!(
            fb_off.pixels[geom_y0 as usize * width + geom_x0 as usize],
            argb_from_amiga(0x0000),
            "DMACON off: must be background, not the lit bitplane data"
        );

        // Same chipset, same chip RAM -- only DMACON changes: must now
        // show the real bitplane content.
        chipset.dmacon = 0x8300; // SETCLR | DMAEN | BPLEN
        let mut pixels_on = std::vec![0u32; width * height];
        let mut fb_on = Framebuffer::new(&mut pixels_on, width, height).unwrap();
        Renderer::new().render(&chipset, &ram, &mut fb_on);
        assert_eq!(
            fb_on.pixels[geom_y0 as usize * width + geom_x0 as usize],
            argb_from_amiga(0x0F0F),
            "DMACON on: the same data must now show the lit bitplane pixel"
        );
    }

    /// background from that band's own COLOR00 -- the concrete bug this
    /// feature exists to fix (proposal §8.1's old "WAITs skipped" made
    /// this kind of list collapse to whichever MOVE ran last).
    #[test]
    fn on_for_a_band_off_outside_it_renders_each_row_from_its_own_band() {
        let mut ram = ram_with(4096);
        // Bitplane data: the one fetched word is fully lit.
        write_word(&mut ram, 0x0200, 0xFFFF);
        write_instrs(
            &mut ram,
            0,
            &[
                move_instr(reg::DMACON, 0x8300), // SETCLR | DMAEN | BPLEN: bitplane DMA on
                move_instr(reg::BPL1PTH, 0x0000),
                move_instr(reg::BPL1PTL, 0x0200),
                // -2 modulo cancels the +2 (one word) per-row advance, so
                // every row re-fetches the same lit word rather than
                // reading unwritten (zero) RAM past it.
                move_instr(reg::BPL1MOD, 0xFFFE),
                move_instr(reg::DIWSTRT, 0x0081), // vstart 0, hstart 0x81
                // vstop byte 0x8A (>= 0x80) is used as-is, no implicit
                // +0x100 (Geometry::decode_band's doc comment) -- vstop 138.
                move_instr(reg::DIWSTOP, 0x8AC1),
                move_instr(reg::DDFSTRT, 0x0038),
                move_instr(reg::DDFSTOP, 0x0038), // one word/line
                move_instr(reg::COLOR00, 0x0001), // "off" background
                move_instr(reg::COLOR00 + 2, 0x0FFF), // COLOR01: lit-pixel colour
                wait_v(0x05, 0xFF),
                move_instr(reg::BPLCON0, 0x1000), // planes on: 1 plane, lores
                wait_v(0x0A, 0xFF),
                move_instr(reg::BPLCON0, 0x0000), // planes off
                end_instr(),
            ],
        );

        let mut chipset = Chipset::new();
        chipset.cop1lc = 0x0;

        // Same DIW math as Geometry::decode_band: hstart 0x81 halved and
        // scaled by px_per_cck (2, lores) is the DIW's first painted
        // column -- size the test surface around it, as the pre-existing
        // geometry tests above already do.
        let geom_x0 = ((0x81u32 & 0xFF) / 2) * 2;
        let width = geom_x0 as usize + 8;
        let height = 14usize;
        let mut pixels = std::vec![0u32; width * height];
        let mut fb = Framebuffer::new(&mut pixels, width, height).unwrap();
        Renderer::new().render(&chipset, &ram, &mut fb);

        assert_eq!(
            fb.pixels[width + geom_x0 as usize],
            argb_from_amiga(0x0001),
            "row before the WAIT-$05 band must be flat off-band background"
        );
        assert_eq!(
            fb.pixels[6 * width + geom_x0 as usize],
            argb_from_amiga(0x0FFF),
            "row inside the WAIT-$05..$0A band must show the on-band's lit pixel"
        );
        assert_eq!(
            fb.pixels[11 * width + geom_x0 as usize],
            argb_from_amiga(0x0001),
            "row after the WAIT-$0A band must be flat off-band background again"
        );
    }

    /// End-to-end regression for the coordinator-reported shear: a real
    /// Kickstart capture showed the boot artwork as diagonal streaks --
    /// every row present, correctly coloured, but each one offset from
    /// the last, the signature of the bitplane pointer advancing by the
    /// wrong number of bytes per row. A "some non-background pixel exists
    /// in the band" check (as this module's other render tests are)
    /// cannot catch that; this test instead pins *where* row 1's data
    /// comes from relative to row 0's, independently computed from the
    /// hardware formula.
    ///
    /// `BPLCON0 = $C000` (hires, 4 planes) with `DDFSTRT == DDFSTOP =
    /// $3C` (the standard hires start, so the fetch-to-display shift is
    /// zero and stride stays the only thing under test) gives
    /// `ddf_words = (0)/4 + 2 = 2` words = 4 bytes/row/plane
    /// (see [`Geometry::decode_band`]'s doc comment) -- so plane 0's row 1
    /// must be fetched from `BPL1PT + 4`, not `+2` (the old, wrong
    /// lores-shaped formula's answer). Only plane 0 carries real data;
    /// planes 1-3 point at untouched (zero) RAM so every pixel's colour
    /// index is exactly plane 0's bit, keeping which *pixel position* is
    /// lit the only thing under test.
    ///
    /// Marker bits are placed at *output*-pixel positions, not raw
    /// fetched-bit positions: hires downsamples 2 fetched bits to 1
    /// output pixel (nearest-neighbour, keeping only even source
    /// indices -- see [`Geometry::bits_per_output_px`]'s doc comment), so
    /// output pixel 0 reads source bit index 0 (word bit 15) and output
    /// pixel 1 reads source bit index 2 (word bit 13); the intervening
    /// odd index (1, word bit 14) is never sampled and is irrelevant here.
    #[test]
    fn hires_bitplane_row_stride_matches_the_hardware_ddf_formula() {
        let mut ram = ram_with(4096);
        write_word(&mut ram, 0x0200, 0b1000_0000_0000_0000); // row 0: output pixel 0 (src bit 0) lit
        write_word(&mut ram, 0x0204, 0b0010_0000_0000_0000); // row 1 at the CORRECT +4-byte stride: output pixel 1 (src bit 2) lit
                                                             // 0x0202 (the WRONG old +2-byte stride's address) is left
                                                             // unwritten (zero): if that regression reappeared, row 1
                                                             // would read all-zero data there and output pixel 1 would
                                                             // wrongly come out clear instead of lit.

        write_instrs(
            &mut ram,
            0,
            &[
                move_instr(reg::DMACON, 0x8300), // SETCLR | DMAEN | BPLEN: bitplane DMA on
                move_instr(reg::BPL1PTH, 0x0000),
                move_instr(reg::BPL1PTL, 0x0200),
                move_instr(reg::BPL2PTH, 0x0000),
                move_instr(reg::BPL2PTL, 0x0F00), // unwritten (zero) RAM
                move_instr(reg::BPL3PTH, 0x0000),
                move_instr(reg::BPL3PTL, 0x0F00),
                move_instr(reg::BPL4PTH, 0x0000),
                move_instr(reg::BPL4PTL, 0x0F00),
                move_instr(reg::BPLCON0, 0xC000), // hires, 4 planes
                move_instr(reg::DIWSTRT, 0x0081), // vstart 0, hstart 0x81
                move_instr(reg::DIWSTOP, 0x8AC1), // vstop 138 (byte >= 0x80)
                move_instr(reg::DDFSTRT, 0x003C),
                move_instr(reg::DDFSTOP, 0x003C),
                move_instr(reg::COLOR00, 0x0000),
                move_instr(reg::COLOR00 + 2, 0x0FFF), // COLOR01: lit
                end_instr(),
            ],
        );

        let mut chipset = Chipset::new();
        chipset.cop1lc = 0x0;

        // Output canvas scale is always OUTPUT_PX_PER_CCK (2), hires
        // included -- see this fn's doc comment and OUTPUT_PX_PER_CCK's.
        let geom_x0 = ((0x81u32 & 0xFF) / 2) * 2;
        let width = geom_x0 as usize + 8;
        let height = 4usize;
        let mut pixels = std::vec![0u32; width * height];
        let mut fb = Framebuffer::new(&mut pixels, width, height).unwrap();
        Renderer::new().render(&chipset, &ram, &mut fb);

        let lit = argb_from_amiga(0x0FFF);
        let bg = argb_from_amiga(0x0000);

        assert_eq!(
            fb.pixels[geom_x0 as usize], lit,
            "row 0, output pixel 0 (from BPL1PT+0, source bit 0)"
        );
        assert_eq!(
            fb.pixels[width + geom_x0 as usize],
            bg,
            "row 1, output pixel 0 must be clear: row 1's word comes from +4 bytes, not +0"
        );
        assert_eq!(
            fb.pixels[width + geom_x0 as usize + 1],
            lit,
            "row 1, output pixel 1 must be lit: row 1's word came from the correct +4-byte stride"
        );
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
    /// with the hires bit set and the real, standard hires DDF window
    /// (`DDFSTRT=$3C, DDFSTOP=$D4` -- commonly documented for a 640-pixel
    /// hires screen, and the same pair the coordinator's shear report
    /// cited): `(0xD4-0x3C)/4 + 2 = 40` words *fetched* -- but the
    /// *output* width stays 320, identical to the lores case above, and
    /// `px_per_cck` stays 2: this is the coordinator's second real-ROM
    /// finding (a checkered-ball logo rendered as a 2:1-wide ellipse and
    /// a floppy graphic cropped off the right edge) -- plotting hires at
    /// its full 4 px/CCK density onto a canvas sized for lores overran
    /// the buffer and stretched everything horizontally. `bits_per_output_px
    /// == 2` is what reconciles hires' denser fetch with the fixed-width
    /// canvas (see [`OUTPUT_PX_PER_CCK`]'s and
    /// [`Geometry::bits_per_output_px`]'s doc comments).
    #[test]
    fn geometry_decodes_hires_640x256() {
        let mut state = base_state();
        state.diwstrt = 0x2C81;
        state.diwstop = 0x2CC1;
        state.ddfstrt = 0x003C;
        state.ddfstop = 0x00D4;
        state.bplcon0 = 0x9000; // hires bit + 1 plane

        let geom = Geometry::decode(&state);

        assert_eq!(
            geom.px_per_cck, 2,
            "output canvas scale is fixed, not hires-doubled"
        );
        assert_eq!(
            geom.bits_per_output_px, 2,
            "hires: 2 fetched bits collapse per output pixel"
        );
        assert_eq!(geom.diw_y1 - geom.diw_y0, 256, "DIW height unchanged");
        assert_eq!(
            geom.diw_x1 - geom.diw_x0,
            320,
            "output width matches lores' physical width, not hires' native 640"
        );
        assert_eq!(
            geom.ddf_words, 40,
            "(0xD4-0x3C)/4 + 2, the real hires DDF formula -- fetch count is unaffected by output downsampling"
        );
    }

    /// The same physical `DIWSTRT`/`DIWSTOP` window must produce the
    /// *same* output-pixel span in hires as in lores -- the coordinator's
    /// exact framing of the fix ("matching lores physical width"). This
    /// pins the contract independently of any specific register pair.
    #[test]
    fn geometry_hires_diw_width_matches_lores_for_the_same_diwstrt_diwstop() {
        let mut lores = base_state();
        lores.diwstrt = 0x2C81;
        lores.diwstop = 0x2CC1;
        lores.bplcon0 = 0x1000; // lores, 1 plane

        let mut hires = lores.clone();
        hires.bplcon0 = 0x9000; // hires, 1 plane -- same DIWSTRT/DIWSTOP

        let g_lores = Geometry::decode(&lores);
        let g_hires = Geometry::decode(&hires);

        assert_eq!(g_lores.diw_x0, g_hires.diw_x0);
        assert_eq!(g_lores.diw_x1, g_hires.diw_x1);
    }

    /// The hires DDF word-count formula (`/4 + 2`) against a couple of
    /// pairs, cross-checked independently of any full render — this is
    /// the "cheap insurance" unit test the coordinator asked for
    /// alongside the end-to-end stride regression test below.
    #[test]
    fn geometry_hires_ddf_word_count_matches_the_hardware_formula() {
        let mut state = base_state();
        state.bplcon0 = 0x8000; // hires, 0 planes (irrelevant to ddf_words)

        // Kickstart's window, and the one case where a naive
        // span/4 + 2 also happens to be right: $D4 lands exactly on an
        // 8-colour-clock fetch-unit boundary.
        state.ddfstrt = 0x003C;
        state.ddfstop = 0x00D4;
        assert_eq!(
            Geometry::decode(&state).ddf_words,
            40,
            "the standard 640-pixel hires screen"
        );

        // AROS's window. $D0 falls part way through a fetch unit, which
        // is still honoured at the next boundary, so this is also 40
        // words -- not the 39 that span/4 + 2 yields. Getting this one
        // word short under-advances every row's bitplane pointer by two
        // bytes and shears the picture progressively down the frame.
        state.ddfstrt = 0x003C;
        state.ddfstop = 0x00D0;
        assert_eq!(
            Geometry::decode(&state).ddf_words,
            40,
            "a mid-unit DDFSTOP rounds up to the same fetch width"
        );

        // A degenerate but legal window (DDFSTRT == DDFSTOP): hires still
        // fetches 2 words, never 1 -- the lores width must not leak in.
        state.ddfstrt = 0x0038;
        state.ddfstop = 0x0038;
        assert_eq!(Geometry::decode(&state).ddf_words, 2, "one unit, hires");
    }

    /// The same degenerate window in lores must use lores' own `+1`, not
    /// hires' `+2` -- the two constants must each stay on their own side
    /// of the hires bit.
    #[test]
    fn geometry_lores_ddf_word_count_still_uses_the_lores_formula() {
        let mut state = base_state();
        state.bplcon0 = 0x0000; // lores
        state.ddfstrt = 0x0038;
        state.ddfstop = 0x0038;
        assert_eq!(Geometry::decode(&state).ddf_words, 1, "(0)/8 + 1");
    }

    /// The standard register relationships map fetched bit 0 to the
    /// window's leftmost pixel (`fetch_bit_shift == 0`) in both
    /// resolutions -- the alignment every pre-existing test and screen
    /// implicitly assumed, now pinned explicitly.
    #[test]
    fn geometry_standard_ddf_diw_pairs_have_zero_fetch_shift() {
        let mut lores = base_state();
        lores.diwstrt = 0x2C81;
        lores.diwstop = 0x2CC1;
        lores.ddfstrt = 0x0038;
        lores.ddfstop = 0x00D0;
        lores.bplcon0 = 0x1000;
        assert_eq!(Geometry::decode(&lores).fetch_bit_shift, 0, "lores");

        let mut hires = lores.clone();
        hires.ddfstrt = 0x003C;
        hires.ddfstop = 0x00D4;
        hires.bplcon0 = 0x9000;
        assert_eq!(Geometry::decode(&hires).fetch_bit_shift, 0, "hires");
    }

    /// Kickstart 47's real boot-screen registers (captured from the ROM's
    /// own copper list: `DIWSTRT=$1D95, DIWSTOP=$38AD, DDFSTRT=$40,
    /// DDFSTOP=$D0, BPLCON1=$44`, hires): the fetch starts one unit
    /// earlier than the window needs and scroll delays it further, so the
    /// window's leftmost pixel must read fetched bit 16 -- word 0 is
    /// fetched but never displayed. With its 38-word fetch over 70-byte
    /// rows (`BPL1MOD=-6`), word 0 holds the *previous* row's rightmost
    /// bytes; displaying it painted a sliver of the floppy graphic's
    /// right edge at the window's left edge (see
    /// [`Geometry::decode_band`]'s doc comment).
    #[test]
    fn geometry_kickstart47_boot_screen_skips_the_first_fetched_word() {
        let mut state = base_state();
        state.diwstrt = 0x1D95;
        state.diwstop = 0x38AD;
        state.ddfstrt = 0x0040;
        state.ddfstop = 0x00D0;
        state.bplcon1 = 0x0044;
        state.bplcon0 = 0xC302; // hires, 4 planes

        let geom = Geometry::decode(&state);
        assert_eq!(
            geom.fetch_bit_shift, 16,
            "(0x95 - 0x81 - 2*(0x40 - 0x3C) - 4) * 2 fetched bits"
        );
        assert_eq!(
            geom.ddf_words, 38,
            "ceil((0xD0-0x40)/8)+1 units, 2 words each"
        );
        assert_eq!(
            geom.diw_x1 - geom.diw_x0,
            280,
            "560 hires px window, decimated 2:1"
        );
    }

    /// End-to-end pin of the fetch-to-display mapping through a full
    /// render with Kickstart 47's register relationship: the first
    /// *displayed* pixel must come from fetched bit 16 (word 1's MSB),
    /// and word 0's content must never reach the screen.
    #[test]
    fn first_displayed_pixel_comes_from_bit_16_for_kickstart47s_registers() {
        let mut ram = ram_with(4096);
        // Word 0: all-ones -- would paint COLOR01 at the window's left
        // edge if the renderer showed it. Word 1: MSB only.
        write_word(&mut ram, 0x0100, 0xFFFF);
        write_word(&mut ram, 0x0102, 0x8000);

        let mut chipset = Chipset::new();
        chipset.bplpt[0] = 0x0100;
        chipset.bplcon0 = 0x9000; // hires, 1 plane
        chipset.bplcon1 = 0x0044;
        chipset.dmacon = 0x8300; // SETCLR | DMAEN | BPLEN
        chipset.diwstrt = 0x0095; // vstart 0, hstart $95
        chipset.diwstop = 0x01AD; // vstop line 1, hstop $AD
        chipset.ddfstrt = 0x0040;
        chipset.ddfstop = 0x00D0;
        chipset.color[0] = 0x0000;
        chipset.color[1] = 0x0FFF;

        let geom_x0 = ((chipset.diwstrt & 0xFF) as usize / 2) * 2;
        let width = geom_x0 + 8;
        let mut pixels = std::vec![0u32; width];
        let mut fb = Framebuffer::new(&mut pixels, width, 1).unwrap();

        Renderer::new().render(&chipset, &ram, &mut fb);

        assert_eq!(
            fb.pixels[geom_x0],
            argb_from_amiga(0x0FFF),
            "leftmost window pixel reads fetched bit 16 (word 1's MSB), lit"
        );
        assert_eq!(
            fb.pixels[geom_x0 + 1],
            argb_from_amiga(0x0000),
            "next pixel reads bit 18, clear -- word 0's all-ones must not leak in"
        );
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
        chipset.dmacon = 0x8300; // SETCLR | DMAEN | BPLEN: bitplane DMA on
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
        chipset.dmacon = 0x8300; // SETCLR | DMAEN | BPLEN: bitplane DMA on
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
        // Header (position/control words) now lives in RAM, exactly
        // where real sprite DMA would have fetched it from -- see
        // draw_sprite0's doc comment on why this is no longer read from
        // the chipset's SPR0POS/SPR0CTL registers. SPR0POS high byte is
        // VSTART, low byte is HSTART8-1: $40 in the high byte gives
        // vstart=$40 with hstart=0 (lands in-canvas for the 16-wide
        // framebuffer below).
        write_word(&mut ram, sprpt, 0x4000); // position word: vstart=$40
        write_word(&mut ram, sprpt + 2, 0x4100); // control word: vstop=$41, height 1
        write_word(&mut ram, sprpt + 4, 0b1000_0000_0000_0000); // data A, pixel 0
        write_word(&mut ram, sprpt + 6, 0b1000_0000_0000_0000); // data B, pixel 0
                                                                // -> pixel 0 colour index = 0b11 = 3 -> COLOR19.

        let mut chipset = Chipset::new();
        chipset.spr0pt = sprpt;
        chipset.color[19] = 0x00F0; // green-ish

        let mut pixels = std::vec![0u32; 16 * 128];
        let mut fb = Framebuffer::new(&mut pixels, 16, 128).unwrap();

        Renderer::new().render(&chipset, &ram, &mut fb);

        let vstart = 0x40usize;
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
        write_word(&mut ram, sprpt, 0x0500); // position word: vstart=5, hstart=0
        write_word(&mut ram, sprpt + 2, 0x0600); // control word: vstop=6, height 1
        write_word(&mut ram, sprpt + 4, 0x0000);
        write_word(&mut ram, sprpt + 6, 0x0000);

        let mut chipset = Chipset::new();
        chipset.spr0pt = sprpt;
        chipset.color[0] = 0x0111; // background, distinguishable from black

        let mut pixels = std::vec![0u32; 16 * 32];
        let mut fb = Framebuffer::new(&mut pixels, 16, 32).unwrap();
        Renderer::new().render(&chipset, &ram, &mut fb);

        let vstart = 5usize;
        assert_eq!(
            fb.pixels[vstart * 16],
            argb_from_amiga(0x0111),
            "transparent sprite pixel leaves the background showing"
        );
    }

    /// Regression test (Phase 2 real-ROM investigation): pins the
    /// rendered sprite height to the sprite's *real* extent -- decoded
    /// from the position/control words at `SPR0PT` in chip RAM, per
    /// hardware's own sprite-DMA-fetch semantics -- rather than to
    /// `MAX_SPRITE_LINES`, the renderer's hostile-input safety clamp. A
    /// real Kickstart 3.2.2 A1200 boot-alert capture hit exactly this:
    /// the chipset's `SPR0CTL` register held a stale, unrelated value
    /// (`$FF00`, never written by the copper list that actually sets up
    /// the pointer) that decoded to a ~255-line `VSTOP`, so the fix's
    /// predecessor drew the real ~16-line pointer image correctly and
    /// then kept going for another 239 rows into whatever chip RAM
    /// happened to follow it -- capped only by `MAX_SPRITE_LINES`, not
    /// by anything the guest actually programmed. This test's `SPR0CTL`
    /// chipset register is deliberately left at exactly that kind of
    /// implausible value while the *real* header in RAM describes a
    /// genuine one-line sprite, so a regression back to reading the
    /// chipset register would fail it.
    #[test]
    fn sprite_height_comes_from_the_real_header_in_ram_not_the_stale_ctl_register() {
        let mut ram = ram_with(4096);
        let sprpt = 0x0300u32;
        // Real header: vstart=2, vstop=3 -- exactly one line.
        write_word(&mut ram, sprpt, 0x0200);
        write_word(&mut ram, sprpt + 2, 0x0300);
        write_word(&mut ram, sprpt + 4, 0b1000_0000_0000_0000); // data A, pixel 0
        write_word(&mut ram, sprpt + 6, 0x0000); // data B, pixel 0
                                                 // -> pixel 0 colour index = 0b01 = 1 -> COLOR17.
                                                 // A second, would-be data line just past the real one-line
                                                 // sprite: must NOT be drawn if the fix reads height from the
                                                 // real header rather than a stale, much taller SPR0CTL.
        write_word(&mut ram, sprpt + 8, 0b1000_0000_0000_0000);
        write_word(&mut ram, sprpt + 10, 0x0000);

        let mut chipset = Chipset::new();
        chipset.spr0pt = sprpt;
        // Implausibly tall if it were ever (wrongly) used for height:
        // vstop=$FF, far past the real header's vstop=3. Left set and
        // never written by any copper MOVE in this test, matching the
        // real boot capture's "stale register" finding exactly.
        chipset.spr0ctl = 0xFF00;
        chipset.color[0] = 0x0111; // background, distinguishable from black
        chipset.color[17] = 0x0F0F; // sprite pixel colour

        let mut pixels = std::vec![0u32; 16 * 32];
        let mut fb = Framebuffer::new(&mut pixels, 16, 32).unwrap();
        Renderer::new().render(&chipset, &ram, &mut fb);

        assert_eq!(
            fb.pixels[2 * 16],
            argb_from_amiga(0x0F0F),
            "line 2 (the real sprite's only line) must be drawn"
        );
        assert_eq!(
            fb.pixels[3 * 16],
            argb_from_amiga(0x0111),
            "line 3 is past the real sprite's one-line height and must \
             stay background, even though SPR0CTL alone would imply \
             height 255"
        );
    }

    #[test]
    fn wild_bitplane_pointer_does_not_panic() {
        let ram = ram_with(64); // tiny, so bplpt below is wildly out of range
        let mut chipset = Chipset::new();
        chipset.bplpt[0] = 0xFFFF_FFF0;
        chipset.bplcon0 = 0x1000;
        chipset.dmacon = 0x8300; // SETCLR | DMAEN | BPLEN: bitplane DMA on,
                                 // so this test actually exercises the wild-pointer read path
                                 // rather than skipping it via the DMA gate.
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
        // The header itself is now read from RAM at this wild address
        // (out of range -> reads back 0, per read_word/read_byte's
        // open-bus-style fallback), so vstart=vstop=0 and draw_sprite0
        // returns before touching anything else.
        chipset.spr0pt = 0xFFFF_0000;

        let mut pixels = [0u32; 16 * 16];
        let mut fb = Framebuffer::new(&mut pixels, 16, 16).unwrap();
        Renderer::new().render(&chipset, &ram, &mut fb); // must not panic
    }

    #[test]
    fn empty_chip_ram_does_not_panic() {
        let ram: std::vec::Vec<u8> = std::vec::Vec::new();
        let mut chipset = Chipset::new();
        chipset.bplcon0 = 0x1000;
        chipset.dmacon = 0x8300; // SETCLR | DMAEN | BPLEN: bitplane DMA on
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

    /// [`odd_field_bplpt`] must derive its offset from the row's own
    /// fetch width, not a hard-coded constant -- checked at two different
    /// `ddf_words` values (a small, arbitrary one, and 40, AROS's real
    /// 4-plane hires row width) so a hypothetical `+80`-style shortcut
    /// would fail the first case even though it happens to pass the
    /// second.
    #[test]
    fn odd_field_offset_is_derived_from_the_row_stride_not_a_constant() {
        let even: [u32; 6] = [0x1000, 0x2000, 0x3000, 0x4000, 0x5000, 0x6000];

        let odd = odd_field_bplpt(even, 5); // 5 words/row = 10 bytes/row
        assert_eq!(
            odd,
            [0x100A, 0x200A, 0x300A, 0x400A, 0x500A, 0x600A],
            "offset must be exactly one row's byte width (ddf_words*2), for an arbitrary stride"
        );

        let odd_aros = odd_field_bplpt(even, 40); // AROS's real 4-plane hires row: 40 words = 80 bytes
        assert_eq!(
            odd_aros,
            [0x1050, 0x2050, 0x3050, 0x4050, 0x5050, 0x6050],
            "same derivation at AROS's real row width (80 bytes), not a coincidental match"
        );
    }

    /// The concrete bug this feature fixes: an interlaced screen must
    /// render as one woven picture at full vertical resolution, not the
    /// same half-height field drawn twice with the second copy reading
    /// off into unrelated memory. Two planes' worth of chip RAM here (one
    /// per field, laid out as consecutive physical scan lines the way a
    /// real interlaced bitmap is) hold *distinguishable* content -- the
    /// even field's rows are all lit, the odd field's all clear -- so
    /// asserting the two alternate strictly by output-row parity is a
    /// direct check of the interleave itself, not just "some pixel
    /// changed somewhere."
    ///
    /// `BPL1MOD = ddf_words*2` (matching this test's one-word, 2-byte
    /// row) reproduces the AROS shape described in this module's
    /// [`draw_bitplanes`] doc comment: each field's own per-line advance
    /// (`ddf_words*2` fetch + that same modulo) is two rows' worth, so a
    /// single field only touches every other memory row -- exactly
    /// AROS's real `BPL1MOD/BPL2MOD = $50` on its 40-word row, scaled
    /// down to a one-word row so the test fixture stays tiny.
    #[test]
    fn interlaced_screen_alternates_between_its_two_fields_by_output_row() {
        let mut ram = ram_with(4096);
        // Physical scan-line order in memory, one word (2 bytes) apart:
        // even field reads 0x0200, 0x0204, 0x0208, 0x020C (its own line
        // 0..3); odd field reads 0x0202, 0x0206, 0x020A, 0x020E (offset
        // one row in, per odd_field_bplpt). Even rows lit, odd rows
        // clear -- deliberately different so the two fields cannot be
        // confused for one another.
        let lit = 0b1000_0000_0000_0000u16;
        write_word(&mut ram, 0x0200, lit);
        write_word(&mut ram, 0x0202, 0x0000);
        write_word(&mut ram, 0x0204, lit);
        write_word(&mut ram, 0x0206, 0x0000);
        write_word(&mut ram, 0x0208, lit);
        write_word(&mut ram, 0x020A, 0x0000);
        write_word(&mut ram, 0x020C, lit);
        write_word(&mut ram, 0x020E, 0x0000);

        let mut chipset = Chipset::new();
        chipset.bplpt[0] = 0x0200;
        chipset.bplcon0 = 0x1004; // 1 plane, lores, LACE (bit 2) set
        chipset.dmacon = 0x8300; // SETCLR | DMAEN | BPLEN: bitplane DMA on
        chipset.bpl1mod = 2; // ddf_words*2 -- see this fn's doc comment
        chipset.diwstrt = 0x0081; // vstart 0, hstart 0x81
        chipset.diwstop = 0x04C1; // vstop 4 (byte < 0x80 -> +0x100 not needed here... see below)
        chipset.ddfstrt = 0x0038;
        chipset.ddfstop = 0x0038; // one word/line
        chipset.color[0] = 0x0000; // background/clear: black
        chipset.color[1] = 0x0FFF; // lit: white

        let geom_x0 = ((chipset.diwstrt & 0xFF) as u32 / 2) * 2;
        let width = geom_x0 as usize + 8;
        let height = 10usize; // field_rows(4) * 2 = 8 interlaced rows, plus headroom
        let mut pixels = std::vec![0u32; width * height];
        let mut fb = Framebuffer::new(&mut pixels, width, height).unwrap();

        Renderer::new().render(&chipset, &ram, &mut fb);

        let lit_argb = argb_from_amiga(0x0FFF);
        let clear_argb = argb_from_amiga(0x0000);
        for row in 0..8usize {
            let expected = if row % 2 == 0 { lit_argb } else { clear_argb };
            assert_eq!(
                fb.pixels[row * width + geom_x0 as usize],
                expected,
                "output row {row}: even rows must be the even field's lit data, \
                 odd rows the odd field's clear data -- not the same field's \
                 data repeated"
            );
        }
    }

    /// Non-interlaced screens must render exactly as before this change:
    /// `field` stays 0 for every row (a single, sequentially-advancing
    /// bitplane pointer, no odd-field offset applied at all), so three
    /// consecutive rows' worth of distinct data must come back in the
    /// same top-to-bottom order they were written, unlike the interlaced
    /// case above where alternate rows come from a different pointer
    /// entirely.
    #[test]
    fn non_interlaced_screen_reads_rows_sequentially_unaffected_by_the_interlace_path() {
        let mut ram = ram_with(4096);
        // Three distinct one-word rows, 2 bytes (ddf_words*2, no modulo)
        // apart -- the plain sequential layout a non-interlaced screen
        // has always used.
        write_word(&mut ram, 0x0200, 0b1000_0000_0000_0000); // row 0: lit
        write_word(&mut ram, 0x0202, 0b0000_0000_0000_0000); // row 1: clear
        write_word(&mut ram, 0x0204, 0b1000_0000_0000_0000); // row 2: lit

        let mut chipset = Chipset::new();
        chipset.bplpt[0] = 0x0200;
        chipset.bplcon0 = 0x1000; // 1 plane, lores, LACE clear
        chipset.dmacon = 0x8300; // SETCLR | DMAEN | BPLEN: bitplane DMA on
        chipset.bpl1mod = 0;
        chipset.diwstrt = 0x0081; // vstart 0, hstart 0x81
        chipset.diwstop = 0x03C1; // vstop 3
        chipset.ddfstrt = 0x0038;
        chipset.ddfstop = 0x0038; // one word/line
        chipset.color[0] = 0x0000;
        chipset.color[1] = 0x0FFF;

        let geom_x0 = ((chipset.diwstrt & 0xFF) as u32 / 2) * 2;
        let width = geom_x0 as usize + 8;
        let mut pixels = std::vec![0u32; width * 6];
        let mut fb = Framebuffer::new(&mut pixels, width, 6).unwrap();

        Renderer::new().render(&chipset, &ram, &mut fb);

        let lit = argb_from_amiga(0x0FFF);
        let clear = argb_from_amiga(0x0000);
        assert_eq!(fb.pixels[geom_x0 as usize], lit, "row 0 lit, as written");
        assert_eq!(
            fb.pixels[width + geom_x0 as usize],
            clear,
            "row 1 clear, as written -- not the even field's row 1 from an \
             interlace pointer that doesn't exist here"
        );
        assert_eq!(
            fb.pixels[2 * width + geom_x0 as usize],
            lit,
            "row 2 lit, as written"
        );
    }

    // ---- render_rtg ----------------------------------------------------

    #[test]
    fn render_rtg_bpp8_looks_up_the_palette_per_byte() {
        let mode = DecodedMode {
            width: 4,
            height: 2,
            depth: PixelDepth::Bpp8,
            stride_bytes: 4,
            start_offset: 0,
        };
        let vram: std::vec::Vec<u8> = std::vec![0, 1, 2, 3, 3, 2, 1, 0];
        let mut pixels = [0u32; 4 * 2];
        let mut fb = Framebuffer::new(&mut pixels, 4, 2).unwrap();

        let palette = |index: u8| 0xFF00_0000 | ((index as u32) << 16);
        render_rtg(&mode, &vram, palette, &mut fb);

        assert_eq!(fb.pixels[0], 0xFF00_0000);
        assert_eq!(fb.pixels[1], 0xFF01_0000);
        assert_eq!(fb.pixels[2], 0xFF02_0000);
        assert_eq!(fb.pixels[3], 0xFF03_0000);
        // Second row starts at stride_bytes, not width*bpp coincidentally
        // equal to it here -- both are 4, so this also exercises that the
        // row-start math actually uses stride rather than width.
        assert_eq!(fb.pixels[4], 0xFF03_0000);
        assert_eq!(fb.pixels[7], 0xFF00_0000);
    }

    #[test]
    fn render_rtg_honours_a_nonzero_panning_start_offset() {
        let mode = DecodedMode {
            width: 2,
            height: 1,
            depth: PixelDepth::Bpp8,
            stride_bytes: 2,
            start_offset: 10,
        };
        let vram: std::vec::Vec<u8> = std::vec![0xAA; 10]
            .into_iter()
            .chain(std::vec![5u8, 6u8])
            .collect();
        let mut pixels = [0u32; 2];
        let mut fb = Framebuffer::new(&mut pixels, 2, 1).unwrap();

        render_rtg(&mode, &vram, |index| 0xFF00_0000 | index as u32, &mut fb);
        assert_eq!(fb.pixels[0], 0xFF00_0005);
        assert_eq!(fb.pixels[1], 0xFF00_0006);
    }

    #[test]
    fn render_rtg_bpp16_decodes_rgb565_little_endian() {
        let mode = DecodedMode {
            width: 1,
            height: 1,
            depth: PixelDepth::Bpp16,
            stride_bytes: 2,
            start_offset: 0,
        };
        // 0xF800 = pure red at 5/6/5 (R=0x1F, G=0, B=0), little-endian in
        // VRAM: low byte 0x00, high byte 0xF8.
        let vram: std::vec::Vec<u8> = std::vec![0x00, 0xF8];
        let mut pixels = [0u32; 1];
        let mut fb = Framebuffer::new(&mut pixels, 1, 1).unwrap();

        render_rtg(&mode, &vram, |_| 0, &mut fb);
        assert_eq!(fb.pixels[0], 0xFFFF_0000, "full-intensity red");
    }

    #[test]
    fn render_rtg_does_not_panic_when_geometry_runs_past_a_short_vram() {
        // decoded_mode's own contract already rules this out in practice,
        // but render_rtg must not assume it -- a mismatched mode/vram pair
        // must degrade to reading zero, never panic.
        let mode = DecodedMode {
            width: 4,
            height: 4,
            depth: PixelDepth::Bpp24,
            stride_bytes: 12,
            start_offset: 0,
        };
        let vram: std::vec::Vec<u8> = std::vec![0x11; 4];
        let mut pixels = [0u32; 16];
        let mut fb = Framebuffer::new(&mut pixels, 4, 4).unwrap();
        render_rtg(&mode, &vram, |_| 0, &mut fb); // must not panic
    }
}
