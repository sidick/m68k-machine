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
    /// Graphics Cursor Attribute register. Bit 1
    /// (`sr12::CURSOR_PALETTE_SELECT`) is the one this module cares about:
    /// while it is set, DAC-data port accesses address the hardware
    /// cursor's own colour table instead of the main palette.
    pub const SR_CURSOR_ATTR: u8 = 0x12;

    /// CRTC: the chip-ID register the RTG driver reads to identify the
    /// silicon. Read-only.
    pub const CRTC_ID: u8 = 0x27;
    pub const CRTC_LAST_STANDARD: u8 = 0x18;
    /// End Vertical Retrace register: standard VGA CR11, present on every
    /// VGA-derived CRTC including the CL-GD542x. Bits 4-5 gate the
    /// vertical-retrace interrupt this module models -- see
    /// [`crtc11::DISABLE_VERTICAL_INTERRUPT`] and
    /// [`crtc11::CLEAR_VERTICAL_INTERRUPT`].
    pub const CRTC_END_VERTICAL_RETRACE: u8 = 0x11;

    // ---- graphics controller: the CL-GD542x BitBLT engine -------------
    //
    // Offsets and bit layouts below come from the Cirrus Logic
    // CL-GD5426/5428 Technical Reference Manual's BitBLT chapter (the
    // task's designated authority), cross-checked for behaviour — never
    // for register numbers or code text — against the Copperline and
    // Amiberry oracles (module docs' usual policy, and doubly important
    // here since both those oracles are GPL).
    /// Blit width, in bytes, minus one, low 8 bits; the next register
    /// (`+1`) holds bits 8-10 (mask `0x07`) — see `Cirrus542x::blit_width`.
    pub const GR_BLIT_WIDTH_LO: u8 = 0x20;
    /// Blit height, in rows, minus one, low 8 bits; the next register
    /// holds bits 8-9 (mask `0x03`) — see `Cirrus542x::blit_height`.
    pub const GR_BLIT_HEIGHT_LO: u8 = 0x22;
    /// Destination pitch, low 8 bits; the next register holds bits 8-12
    /// (mask `0x1F`) — see `Cirrus542x::blit_pitch`.
    pub const GR_BLIT_DST_PITCH_LO: u8 = 0x24;
    /// Source pitch, same LO/HI shape as the destination pitch.
    pub const GR_BLIT_SRC_PITCH_LO: u8 = 0x26;
    /// Destination address, 21 bits across three consecutive registers
    /// (LO, `+1`, `+2` masked to `0x1F`) — see `Cirrus542x::blit_addr`.
    pub const GR_BLIT_DST_ADDR_LO: u8 = 0x28;
    /// Source address, same 21-bit three-register shape as the
    /// destination.
    pub const GR_BLIT_SRC_ADDR_LO: u8 = 0x2C;
    /// Blit mode: see `blit_mode` for the bit layout.
    pub const GR_BLIT_MODE: u8 = 0x30;
    /// Start trigger / busy status: see `blit_status`.
    pub const GR_BLIT_START_STATUS: u8 = 0x31;
    /// Raster operation code (`apply_rop`'s `rop` argument).
    pub const GR_BLIT_ROP: u8 = 0x32;
    /// Extended mode bits: see `blit_modeext`.
    pub const GR_BLIT_MODE_EXT: u8 = 0x33;
    /// Transparency compare colour, one byte per pixel component
    /// (`GR34..=GR37`).
    pub const GR_BLIT_TRANSPARENT_COMPARE: u8 = 0x34;
    /// Transparency compare mask, one byte per pixel component
    /// (`GR38..=GR3B`); a set mask bit means "don't care" for that bit
    /// of the comparison.
    pub const GR_BLIT_TRANSPARENT_MASK: u8 = 0x38;
    /// Foreground/background colour, one register pair per pixel
    /// component (byte lane), used by solid fill and colour expansion.
    /// The same registers VGA write-mode 0/2's Set/Reset and Colour
    /// Compare paths use for an 8bpp plane — the chip multiplexes them,
    /// same as real silicon; nothing here is blit-specific storage.
    pub const GR_BG0: u8 = 0x00;
    pub const GR_FG0: u8 = 0x01;
    pub const GR_BG1: u8 = 0x10;
    pub const GR_FG1: u8 = 0x11;
    pub const GR_BG2: u8 = 0x12;
    pub const GR_FG2: u8 = 0x13;
    pub const GR_BG3: u8 = 0x14;
    pub const GR_FG3: u8 = 0x15;
}

/// `idx::GR_BLIT_MODE` (GR30) bit flags.
mod blit_mode {
    /// Descending (decrementing) addresses instead of ascending.
    pub const BACKWARDS: u8 = 0x01;
    /// Source comes from the host CPU pushing bytes through the VRAM
    /// aperture rather than from VRAM itself.
    pub const SYSTEM_SOURCE: u8 = 0x04;
    /// Source pixels matching the transparency-compare colour are
    /// skipped (destination left unwritten) instead of blended by the
    /// ROP.
    pub const TRANSPARENT: u8 = 0x08;
    /// Mask for the pixel-width field: `0x00`/`0x10`/`0x20`/`0x30` for
    /// 1/2/3/4 bytes per pixel.
    pub const PIXEL_WIDTH_MASK: u8 = 0x30;
    /// Source is an 8x8 tile read once and repeated, rather than a
    /// linear scan.
    pub const PATTERN: u8 = 0x40;
    /// Source is a monochrome bit stream: each bit selects the
    /// foreground or background colour for one pixel.
    pub const COLOR_EXPAND: u8 = 0x80;
}

/// `idx::GR_BLIT_MODE_EXT` (GR33) bit flags.
mod blit_modeext {
    /// Colour-expand source bits are grouped into 32-bit words rather
    /// than 8-bit bytes (changes the row-padding granularity).
    pub const DWORD_GRANULARITY: u8 = 0x01;
    /// Invert which polarity of an expanded bit is "transparent" under
    /// `blit_mode::TRANSPARENT`.
    pub const COLOR_EXPAND_INVERT: u8 = 0x02;
    /// Colour-expand every source bit as foreground (a solid fill:
    /// `PATTERN | COLOR_EXPAND` with this set paints the whole rectangle
    /// the foreground colour, ignoring the pattern tile entirely).
    pub const SOLID_FILL: u8 = 0x04;
}

/// `idx::GR_BLIT_START_STATUS` (GR31) bit flags.
mod blit_status {
    /// Write 1 to arm and, for anything but a system-source blit, run
    /// the transfer immediately (this engine is synchronous, the same
    /// house style as [`crate::blitter`] — module docs there explain
    /// why: no DMA/timing model, so the guest never observes a
    /// partially-run blit).
    pub const START: u8 = 0x02;
    /// Write 1 to abandon a pending (system-source) transfer.
    pub const RESET: u8 = 0x04;
    /// The bits a status read shows set while a system-source transfer
    /// is still waiting on host data. Matches how Picasso96's Cirrus
    /// driver polls this register (Copperline's oracle, cross-checked,
    /// reports the identical mask).
    pub const BUSY_MASK: u8 = 0x09;
}

/// `idx::SR_CURSOR_ATTR` (SR12) bit flags.
mod sr12 {
    /// When set, `port::DAC_DATA` reads/writes address the hardware
    /// cursor's private colour table (`cursor_palette`, indices 0 and
    /// 0x0F only) instead of the main 256-entry palette. Documented CL-
    /// GD54xx behaviour, and confirmed against Copperline's Cirrus model
    /// (`gd5426.rs`): Picasso96's driver sets this bit, writes the
    /// pointer's background (index 0) and foreground (index 0x0F)
    /// colours, then clears it again before resuming normal palette
    /// programming.
    pub const CURSOR_PALETTE_SELECT: u8 = 0x02;
}

/// `idx::CRTC_END_VERTICAL_RETRACE` (CR11) bit flags -- standard VGA, not
/// a Cirrus extension, so these apply even before the extended registers
/// are unlocked (CR11 is within `idx::CRTC_LAST_STANDARD`). Confirmed
/// against FreeVGA's CRTC register reference and, behaviourally (never
/// for register numbers or code, module docs' usual policy), against
/// Copperline's `picasso2::gd5426` model, which gates its own
/// `vertical_interrupt_enabled` on exactly this bit pair.
mod crtc11 {
    /// Bit 4, active low: write 0 to acknowledge (clear) a latched
    /// vertical-retrace interrupt. Unlike most VGA "write 1 to clear"
    /// conventions, the bit must then be written back to 1 before the
    /// chip will latch the *next* retrace -- while it reads 0 the
    /// interrupt condition stays masked, so a driver's ack sequence
    /// (clear, then re-arm) is what actually re-enables delivery, not a
    /// self-clearing strobe.
    pub const CLEAR_VERTICAL_INTERRUPT: u8 = 0x10;
    /// Bit 5, active high in the inverted sense its name suggests: 1
    /// *disables* the vertical-retrace interrupt, 0 enables it. Combined
    /// with `CLEAR_VERTICAL_INTERRUPT` above, "enabled" is bits 5:4 ==
    /// `0b01` -- both zero (CR11's power-on-reset value) reads as
    /// "enabled" by bit 5 alone but is held masked by bit 4 being clear,
    /// so the register's own reset state is disabled without this model
    /// needing to special-case it.
    pub const DISABLE_VERTICAL_INTERRUPT: u8 = 0x20;
}

/// Upper bound on a system-source (host-fed) blit transfer this model
/// will buffer before running it: `#![no_std]` with no allocator rules
/// out sizing the buffer to the transfer, like the oracles do. Chosen
/// generously against what P96's Cirrus driver actually pushes through
/// this path — glyph and icon colour-expansion, a handful of KB at
/// most — not against the register field's own much larger theoretical
/// range. A transfer that requests more than this is still safe: bytes
/// past the cap are counted towards completion (so the driver's
/// handshake still finishes) but dropped rather than written, and the
/// blit that follows runs against whatever fits — never a panic or an
/// out-of-bounds write.
const SYSTEM_BLIT_CAPACITY: usize = 8192;

/// A guest-programmed blit address is a 21-bit counter on real silicon
/// (three GR registers, the top one masked to 5 bits): a value that
/// overflows or underflows wraps within that field instead of escaping
/// it. This bounds the *register representation*; the actual VRAM access
/// is separately bounds-checked against the real (possibly smaller)
/// backing store via `slice::get`, never indexed raw.
const BLIT_ADDR_MASK: usize = 0x1F_FFFF;

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
    /// The hardware cursor's private 16-entry colour table (only indices
    /// 0x00 and 0x0F are actually meaningful, holding the cursor's
    /// background and foreground colours respectively). SR12 bit 1
    /// (`idx::SR_CURSOR_ATTR`, `sr12::CURSOR_PALETTE_SELECT`) redirects
    /// the DAC-data port here instead of the main 256-entry palette so a
    /// driver can set the pointer colours without disturbing the screen
    /// palette entries at the same index -- see the CL-GD54xx datasheet's
    /// "Hardware Cursor Color" registers. This model never composites the
    /// cursor onto the framebuffer (out of scope, see module doc comment),
    /// so the table exists purely to keep these writes from landing in
    /// `palette` instead.
    cursor_palette: [[u8; 3]; 16],

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

    /// A system-source blit that has been started but is still waiting
    /// for the driver to push its pixel data through the VRAM aperture
    /// (see `blit_mode::SYSTEM_SOURCE` and `SYSTEM_BLIT_CAPACITY`).
    /// `None` the rest of the time, including for every ordinary
    /// (VRAM-to-VRAM or pattern/solid) blit, which this model runs to
    /// completion synchronously inside the register write that starts
    /// it.
    system_blit: Option<SystemBlit>,

    /// Latched vertical-retrace interrupt condition (Input Status 0 bit
    /// 7 at `port::MISC_OUTPUT`/0x3C2). Set by
    /// [`Cirrus542x::signal_vertical_retrace`] only while CR11 has the
    /// interrupt armed (`crtc11` module docs); cleared by a CR11 write
    /// with bit 4 low. Starts `false` and CR11 starts all-zero, so a
    /// freshly reset chip never asserts -- the gating this whole feature
    /// exists to guarantee (see `graffity` module docs and this crate's
    /// end-to-end boot regression).
    vblank_pending: bool,
}

/// State for an in-flight system-source (host-fed) blit: how many bytes
/// the transfer needs, how many have arrived, and the bytes themselves
/// in a fixed buffer (see `SYSTEM_BLIT_CAPACITY` for why it isn't sized
/// to the transfer).
struct SystemBlit {
    expected: usize,
    filled: usize,
    buf: [u8; SYSTEM_BLIT_CAPACITY],
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
            cursor_palette: [[0; 3]; 16],
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
            system_blit: None,
            vblank_pending: false,
        }
    }

    pub fn vram_len(&self) -> usize {
        self.vram.len()
    }

    /// Borrow the whole VRAM backing store, for a caller that wants to
    /// walk a frame's worth of pixels in bulk (an RTG present path) rather
    /// than one byte at a time through [`Cirrus542x::vram_read`]. Bounds
    /// safety is whatever slice indexing already gives a caller — nothing
    /// here does any address translation of its own.
    pub fn vram(&self) -> &[u8] {
        self.vram
    }

    /// Read a byte of VRAM through the linear aperture.
    pub fn vram_read(&self, offset: usize) -> u8 {
        self.vram
            .get(offset)
            .copied()
            .unwrap_or(crate::OPEN_BUS_BYTE)
    }

    /// Write a byte of VRAM through the linear aperture.
    ///
    /// While a system-source blit is armed and waiting for data, *every*
    /// aperture write feeds that transfer instead of touching VRAM,
    /// whatever `offset` is — real hardware routes the whole aperture to
    /// the BitBLT FIFO in this state, and the driver always writes
    /// through a fixed pointer rather than incrementing one of its own,
    /// so the offset it happens to use is not meaningful here either.
    pub fn vram_write(&mut self, offset: usize, value: u8) {
        if self.system_blit.is_some() {
            self.feed_system_blit(value);
            return;
        }
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
        if index == idx::GR_BLIT_START_STATUS {
            // START/RESET are write-only triggers, never latched: only
            // the busy bit is synthesised on readback, same shape as
            // DMACONR's BBUSY in `crate::blitter`.
            let mut value = self.gr[index as usize] & !blit_status::BUSY_MASK;
            if self.system_blit.is_some() {
                value |= blit_status::BUSY_MASK;
            }
            return value;
        }
        self.gr[index as usize]
    }

    fn write_graphics(&mut self, value: u8) {
        let index = self.gr_index & 0x3F;
        if index > 8 && !self.extensions_unlocked() {
            return;
        }
        self.gr[index as usize] = value;
        if index == idx::GR_BLIT_START_STATUS {
            if value & blit_status::RESET != 0 {
                self.system_blit = None;
                self.gr[idx::GR_BLIT_START_STATUS as usize] &= !blit_status::BUSY_MASK;
            } else if value & blit_status::START != 0 {
                self.start_blit();
            }
        }
    }

    // ---- BitBLT engine ---------------------------------------------------

    fn blit_width(&self) -> usize {
        let lo = idx::GR_BLIT_WIDTH_LO as usize;
        ((usize::from(self.gr[lo + 1] & 0x07) << 8) | usize::from(self.gr[lo])) + 1
    }

    fn blit_height(&self) -> usize {
        let lo = idx::GR_BLIT_HEIGHT_LO as usize;
        ((usize::from(self.gr[lo + 1] & 0x03) << 8) | usize::from(self.gr[lo])) + 1
    }

    fn blit_pitch(&self, low: u8) -> usize {
        (usize::from(self.gr[low as usize + 1] & 0x1F) << 8) | usize::from(self.gr[low as usize])
    }

    fn blit_addr(&self, low: u8) -> usize {
        let low = low as usize;
        usize::from(self.gr[low])
            | (usize::from(self.gr[low + 1]) << 8)
            | (usize::from(self.gr[low + 2] & 0x1F) << 16)
    }

    /// The counterpart to `blit_addr`: a completed transfer leaves the
    /// address registers where it stopped rather than reloading them
    /// (real hardware behaviour graphics.library-equivalent Picasso96
    /// drivers rely on — see `execute_blit`'s closing comment), so this
    /// is called at the end of every blit, not just on a fresh program.
    fn set_blit_addr(&mut self, low: u8, addr: usize) {
        let addr = addr & BLIT_ADDR_MASK;
        let low = low as usize;
        self.gr[low] = addr as u8;
        self.gr[low + 1] = (addr >> 8) as u8;
        self.gr[low + 2] = (self.gr[low + 2] & !0x1F) | ((addr >> 16) as u8 & 0x1F);
    }

    fn blit_pixel_bytes(&self) -> usize {
        match self.gr[idx::GR_BLIT_MODE as usize] & blit_mode::PIXEL_WIDTH_MASK {
            0x00 => 1,
            0x10 => 2,
            0x20 => 3,
            _ => 4,
        }
    }

    /// Bytes per row a system-source transfer must supply: a plain
    /// (non-expanded) source pads each row to a 4-byte boundary; a
    /// colour-expanded one packs one bit per pixel and pads to 8 or 32
    /// bits depending on `blit_modeext::DWORD_GRANULARITY`.
    fn system_source_pitch(&self, width: usize) -> usize {
        let mode = self.gr[idx::GR_BLIT_MODE as usize];
        if mode & blit_mode::COLOR_EXPAND == 0 {
            width.next_multiple_of(4)
        } else {
            let pixels = width.div_ceil(self.blit_pixel_bytes());
            let granularity =
                if self.gr[idx::GR_BLIT_MODE_EXT as usize] & blit_modeext::DWORD_GRANULARITY != 0 {
                    32
                } else {
                    8
                };
            pixels.next_multiple_of(granularity) / 8
        }
    }

    fn blit_fg_component(&self, component: usize) -> u8 {
        const FG: [u8; 4] = [idx::GR_FG0, idx::GR_FG1, idx::GR_FG2, idx::GR_FG3];
        self.gr[FG[component.min(3)] as usize]
    }

    fn blit_bg_component(&self, component: usize) -> u8 {
        const BG: [u8; 4] = [idx::GR_BG0, idx::GR_BG1, idx::GR_BG2, idx::GR_BG3];
        self.gr[BG[component.min(3)] as usize]
    }

    /// One source byte for a linear (non-colour-expanded) blit: a
    /// pattern tile read modulo its own 8x8-pixel-row size, a
    /// system-fed byte at `(y, x)` in the host buffer, or a plain VRAM
    /// read walking forwards or backwards from `src_line`. Every path
    /// reads through `slice::get`: `src_start`/`src_line`/`x` are all
    /// guest-controlled and a wild combination must produce a defined
    /// "as if unmapped" byte (0), never panic.
    #[allow(clippy::too_many_arguments)]
    fn linear_blit_source(
        &self,
        system: Option<&[u8]>,
        system_pitch: usize,
        pattern: bool,
        backwards: bool,
        src_line: usize,
        src_start: usize,
        y: usize,
        x: usize,
        pixel_bytes: usize,
    ) -> u8 {
        if pattern {
            let row_bytes = 8 * pixel_bytes;
            let pattern_base = src_start & !3;
            return self
                .vram
                .get(pattern_base + (y & 7) * row_bytes + (x % row_bytes.max(1)))
                .copied()
                .unwrap_or(0);
        }
        if let Some(data) = system {
            return data.get(y * system_pitch + x).copied().unwrap_or(0);
        }
        let src = if backwards {
            src_line.checked_sub(x)
        } else {
            src_line.checked_add(x)
        };
        src.and_then(|at| self.vram.get(at).copied()).unwrap_or(0)
    }

    fn start_blit(&mut self) {
        if self.gr[idx::GR_BLIT_MODE as usize] & blit_mode::SYSTEM_SOURCE != 0 {
            let width = self.blit_width();
            let height = self.blit_height();
            let expected = self
                .system_source_pitch(width)
                .saturating_mul(height)
                .max(1);
            self.system_blit = Some(SystemBlit {
                expected,
                filled: 0,
                buf: [0; SYSTEM_BLIT_CAPACITY],
            });
        } else {
            self.execute_blit(None);
        }
    }

    /// Feed one host byte to a pending system-source transfer, running
    /// the blit once enough have arrived. Bytes past `SYSTEM_BLIT_CAPACITY`
    /// still count towards `expected` (so the driver's write count still
    /// completes the handshake) but are not stored — see
    /// `SYSTEM_BLIT_CAPACITY`'s doc comment.
    fn feed_system_blit(&mut self, byte: u8) {
        let Some(blit) = self.system_blit.as_mut() else {
            return;
        };
        if blit.filled < blit.buf.len() {
            blit.buf[blit.filled] = byte;
        }
        blit.filled += 1;
        if blit.filled < blit.expected {
            return;
        }
        // Copying the (small, fixed-size, `Copy`) buffer out sidesteps
        // holding a `&self` borrow of it across the `&mut self` that
        // `execute_blit` needs to write VRAM.
        let len = blit.expected.min(blit.buf.len());
        let data = blit.buf;
        self.system_blit = None;
        self.execute_blit(Some(&data[..len]));
    }

    /// Run one BitBLT transfer against VRAM: a minterm-free ROP combine
    /// of a source (plain VRAM, an 8x8 pattern tile, a solid fill, a
    /// colour-expanded monochrome stream, or host-fed system memory)
    /// into the destination rectangle, honouring backwards traversal and
    /// source transparency. `system` is `Some` only for a completed
    /// system-source transfer (see `feed_system_blit`); every other mode
    /// reads its source straight out of `self.vram`.
    ///
    /// Ported from the Cirrus datasheet's BitBLT description rather than
    /// any emulator source (module docs), with behaviour cross-checked
    /// against the Copperline and Amiberry oracles — the address-counter
    /// end-of-transfer behaviour in particular (see the comment at the
    /// bottom) is exactly the kind of driver-visible detail a datasheet
    /// alone under-specifies and Picasso96's own CL-GD542x driver
    /// depends on.
    fn execute_blit(&mut self, system: Option<&[u8]>) {
        let width = self.blit_width().min(self.vram.len().max(1));
        let height = self.blit_height();
        let dst_pitch = self.blit_pitch(idx::GR_BLIT_DST_PITCH_LO);
        let src_pitch = self.blit_pitch(idx::GR_BLIT_SRC_PITCH_LO);
        let dst_start = self.blit_addr(idx::GR_BLIT_DST_ADDR_LO);
        let src_start = self.blit_addr(idx::GR_BLIT_SRC_ADDR_LO);
        let mode = self.gr[idx::GR_BLIT_MODE as usize];
        let backwards = mode & blit_mode::BACKWARDS != 0;
        let color_expand = mode & blit_mode::COLOR_EXPAND != 0;
        let pattern = mode & blit_mode::PATTERN != 0;
        let pixel_bytes = self.blit_pixel_bytes();
        let system_pitch = self.system_source_pitch(width);
        let modeext = self.gr[idx::GR_BLIT_MODE_EXT as usize];
        let solid_fill = pattern
            && color_expand
            && mode & blit_mode::TRANSPARENT == 0
            && modeext & blit_modeext::SOLID_FILL != 0;

        // Video-source colour expansion consumes one continuous bit
        // stream across the whole transfer: a row that ends mid-byte
        // rounds up to the next source byte rather than realigning, so
        // it needs its own running address/bit-count independent of the
        // pixel loop below (pattern and system-source expansion instead
        // address their source freshly every row).
        let mut expand_addr = src_start;
        let mut expand_count = 0usize;
        let expand_span = 8 * pixel_bytes;

        for y in 0..height {
            let dst_line = if backwards {
                dst_start.saturating_sub(y.saturating_mul(dst_pitch))
            } else {
                dst_start.saturating_add(y.saturating_mul(dst_pitch))
            };
            let src_line = if backwards {
                src_start.saturating_sub(y.saturating_mul(src_pitch))
            } else {
                src_start.saturating_add(y.saturating_mul(src_pitch))
            };
            for x in 0..width {
                let expand_bit = color_expand.then(|| {
                    let pixel = x / pixel_bytes;
                    if pattern {
                        let byte = self
                            .vram
                            .get(src_start.saturating_add(y & 7))
                            .copied()
                            .unwrap_or(0);
                        (byte >> (7 - (pixel & 7))) & 1
                    } else if let Some(data) = system {
                        let byte = data.get(y * system_pitch + pixel / 8).copied().unwrap_or(0);
                        (byte >> (7 - (pixel & 7))) & 1
                    } else {
                        let byte = self.vram.get(expand_addr).copied().unwrap_or(0);
                        (byte >> (7 - expand_count / pixel_bytes)) & 1
                    }
                });
                if color_expand && !pattern && system.is_none() {
                    expand_count += 1;
                    if expand_count == expand_span {
                        expand_count = 0;
                        expand_addr = if backwards {
                            expand_addr.wrapping_sub(1)
                        } else {
                            expand_addr.wrapping_add(1)
                        };
                    }
                }
                let dst = if backwards {
                    dst_line.checked_sub(x)
                } else {
                    dst_line.checked_add(x)
                };
                let Some(dst) = dst.filter(|at| *at < self.vram.len()) else {
                    continue;
                };
                let component = x % pixel_bytes;
                let source = if solid_fill {
                    self.blit_fg_component(component)
                } else if let Some(bit) = expand_bit {
                    if bit != 0 {
                        self.blit_fg_component(component)
                    } else {
                        self.blit_bg_component(component)
                    }
                } else {
                    self.linear_blit_source(
                        system,
                        system_pitch,
                        pattern,
                        backwards,
                        src_line,
                        src_start,
                        y,
                        x,
                        pixel_bytes,
                    )
                };

                let transparent = if mode & blit_mode::TRANSPARENT == 0 {
                    false
                } else if let Some(bit) = expand_bit {
                    if modeext & blit_modeext::COLOR_EXPAND_INVERT != 0 {
                        bit != 0
                    } else {
                        bit == 0
                    }
                } else if pattern {
                    false
                } else {
                    let pixel_base = x - component;
                    (0..pixel_bytes).all(|c| {
                        let source = self.linear_blit_source(
                            system,
                            system_pitch,
                            false,
                            backwards,
                            src_line,
                            src_start,
                            y,
                            pixel_base + c,
                            pixel_bytes,
                        );
                        let mask = self.gr[idx::GR_BLIT_TRANSPARENT_MASK as usize + c];
                        (source & !mask)
                            == (self.gr[idx::GR_BLIT_TRANSPARENT_COMPARE as usize + c] & !mask)
                    })
                };
                if transparent {
                    continue;
                }
                let dest = self.vram[dst];
                self.vram[dst] = apply_rop(self.gr[idx::GR_BLIT_ROP as usize], source, dest);
            }
            if color_expand && !pattern && system.is_none() && expand_count != 0 {
                expand_count = 0;
                expand_addr = if backwards {
                    expand_addr.wrapping_sub(1)
                } else {
                    expand_addr.wrapping_add(1)
                };
            }
        }

        // The address registers are counters, not latches: a completed
        // transfer leaves them where it stopped rather than where it
        // started. Picasso96's Cirrus driver depends on this to fill a
        // run wider than one tile — it blits one 8-pixel tile, then
        // re-triggers with only width/height reprogrammed to replicate
        // it, expecting the destination counter to have moved on past
        // the tile it just wrote. Reloading the registers here would
        // have the replication overwrite the pixels it just filled
        // rather than continue past them.
        //
        // The destination is always walked byte for byte, so its
        // counter ends a whole rectangle further on. The source is not:
        // only a plain (non-pattern, non-system, non-colour-expanded)
        // copy consumes it in lock-step with the destination. Video
        // colour expansion instead reads one bit per pixel from a
        // continuous stream — `expand_addr` above already tracks where
        // that stream ended. A pattern is re-read from its fixed base
        // every row, and a system-source transfer's data comes from the
        // host, so neither advances the VRAM source counter at all.
        let advance = |from: usize, span: usize| {
            if backwards {
                from.wrapping_sub(span)
            } else {
                from.wrapping_add(span)
            }
        };
        let dst_end = advance(dst_start, height.saturating_sub(1) * dst_pitch + width);
        let src_end = if pattern || system.is_some() {
            src_start
        } else if color_expand {
            expand_addr
        } else {
            advance(src_start, height.saturating_sub(1) * src_pitch + width)
        };
        self.set_blit_addr(idx::GR_BLIT_DST_ADDR_LO, dst_end);
        self.set_blit_addr(idx::GR_BLIT_SRC_ADDR_LO, src_end);
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
        // CR11 bit 4 active low: the guest acknowledges a latched
        // vertical-retrace interrupt by writing it clear (`crtc11`
        // module docs). The shared INT2 line re-latches on the next
        // retrace if still armed -- level-triggered and re-asserted,
        // not a one-shot, same as every other device sharing this line
        // (`lib.rs`).
        if index == idx::CRTC_END_VERTICAL_RETRACE && value & crtc11::CLEAR_VERTICAL_INTERRUPT == 0
        {
            self.vblank_pending = false;
        }
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

    /// Real hardware has no *other* readable input-status-0 bits this
    /// model needs (dot-clock/vsync line sensing); a fixed 0 there is
    /// indistinguishable to a driver that isn't polling real timing,
    /// which none of the scope here does. Bit 7, though, is the
    /// documented vertical-retrace-interrupt-pending flag (module docs),
    /// and is re-derived from CR11 here rather than returning the raw
    /// latch -- a driver that disables the interrupt without
    /// acknowledging it must stop seeing it pending.
    fn input_status_0(&self) -> u8 {
        if self.vblank_pending && self.vertical_interrupt_enabled() {
            0x80
        } else {
            0
        }
    }

    /// CR11 bits 5:4 both read as "enabled": bit 5 (`DISABLE_VERTICAL_
    /// INTERRUPT`) low, and bit 4 (`CLEAR_VERTICAL_INTERRUPT`) high, i.e.
    /// not currently held in its cleared/masked state. See the
    /// [`crtc11`] module docs for why the register's own reset value
    /// (all zero) already fails this and needs no special-casing.
    fn vertical_interrupt_enabled(&self) -> bool {
        self.crtc[idx::CRTC_END_VERTICAL_RETRACE as usize]
            & (crtc11::DISABLE_VERTICAL_INTERRUPT | crtc11::CLEAR_VERTICAL_INTERRUPT)
            == crtc11::CLEAR_VERTICAL_INTERRUPT
    }

    /// Called once per display frame on vertical retrace (module docs:
    /// driven from the chipset's own frame boundary, not a second
    /// clock). Latches the pending flag only while the interrupt is
    /// armed -- an unprogrammed or interrupt-disabled chip never
    /// accumulates a request no one asked for.
    pub fn signal_vertical_retrace(&mut self) {
        if self.vertical_interrupt_enabled() {
            self.vblank_pending = true;
        }
    }

    /// Whether this chip is currently asserting its vertical-retrace
    /// interrupt line -- what the board layer forwards to `lib.rs` for
    /// delivery onto the shared INT2 line, the same shape every other
    /// device sharing that line uses.
    pub fn irq_pending(&self) -> bool {
        self.vblank_pending && self.vertical_interrupt_enabled()
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
        let component = self.dac_component as usize;
        if self.sr[idx::SR_CURSOR_ATTR as usize] & sr12::CURSOR_PALETTE_SELECT != 0 {
            let index = (self.dac_write_index & 0x0F) as usize;
            self.cursor_palette[index][component] = value & 0x3F;
        } else {
            let index = self.dac_write_index as usize;
            self.palette[index][component] = value & 0x3F;
        }
        self.dac_component += 1;
        if self.dac_component == 3 {
            self.dac_component = 0;
            self.dac_write_index = self.dac_write_index.wrapping_add(1);
        }
    }

    fn read_dac_data(&mut self) -> u8 {
        let component = self.dac_component as usize;
        let value = if self.sr[idx::SR_CURSOR_ATTR as usize] & sr12::CURSOR_PALETTE_SELECT != 0 {
            let index = (self.dac_read_index & 0x0F) as usize;
            self.cursor_palette[index][component]
        } else {
            let index = self.dac_read_index as usize;
            self.palette[index][component]
        };
        self.dac_component += 1;
        if self.dac_component == 3 {
            self.dac_component = 0;
            self.dac_read_index = self.dac_read_index.wrapping_add(1);
        }
        value
    }
}

/// The CL-GD542x BitBLT raster-operation encoding: a sparse set of byte
/// codes selecting one of the 16 two-operand Boolean functions of
/// `source` and `dest` (`source` doubling as the pattern/fill byte for
/// pattern operations). Values are the Cirrus Logic Technical Reference
/// Manual's BitBLT ROP table, not derived from either GPL oracle
/// (module docs) — cross-checked behaviourally against Copperline,
/// which flags `0x90` (NOR, `!source & !dest`) and `0xDA` (NAND,
/// `!source | !dest`) as easy to transpose; the test below verifies
/// both independently of this table rather than trusting that warning.
fn apply_rop(rop: u8, source: u8, dest: u8) -> u8 {
    match rop {
        0x00 => 0,
        0x05 => source & dest,
        0x06 => dest,
        0x09 => source & !dest,
        0x0B => !dest,
        0x0D => source,
        0x0E => 0xFF,
        0x50 => !source & dest,
        0x59 => source ^ dest,
        0x6D => source | dest,
        0x90 => !source & !dest,
        0x95 => !(source ^ dest),
        0xAD => source | !dest,
        0xD0 => !source,
        0xD6 => !source | dest,
        0xDA => !source | !dest,
        // Every code P96's driver actually programs is above; an
        // unrecognised one is guest-controlled input (a wild GR32
        // write), so fall back to a plain copy rather than guessing at
        // undocumented silicon behaviour.
        _ => source,
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

    // ---- vertical retrace interrupt -------------------------------------

    #[test]
    fn vertical_retrace_interrupt_is_disabled_out_of_reset() {
        // CR11 starts all-zero (`Cirrus542x::new`), which reads as bit 5
        // (disable) clear -- naively "enabled" -- but bit 4 (clear) is
        // also clear, holding the interrupt masked. A machine that never
        // touches CR11 must never see the vblank interrupt pending, on
        // pain of destabilising a boot this feature is not supposed to
        // touch (`graffity` module docs).
        let mut vram = [0u8; 16];
        let mut c = chip(&mut vram);
        c.signal_vertical_retrace();
        assert!(!c.irq_pending(), "disabled by CR11's own reset value");
        assert_eq!(
            c.reg_read(port::MISC_OUTPUT),
            0,
            "Input Status 0 bit 7 clear"
        );
    }

    #[test]
    fn disable_bit_is_inverted_one_disables_zero_enables() {
        let mut vram = [0u8; 16];
        let mut c = chip(&mut vram);
        // Arm bit 4 (clear/ack bit set) but leave bit 5 (disable) set --
        // the inverted sense under test: 1 must mean disabled, not
        // enabled.
        c.reg_write(port::CRTC_INDEX_COLOR, 0x11);
        c.reg_write(port::CRTC_DATA_COLOR, 0x30);
        c.signal_vertical_retrace();
        assert!(
            !c.irq_pending(),
            "bit 5 set (disable) must suppress the interrupt"
        );

        // Now clear bit 5 (0 == enabled) while keeping bit 4 armed.
        c.reg_write(port::CRTC_INDEX_COLOR, 0x11);
        c.reg_write(port::CRTC_DATA_COLOR, 0x10);
        c.signal_vertical_retrace();
        assert!(c.irq_pending(), "bit 5 clear (enable) must let it through");
        assert_eq!(
            c.reg_read(port::MISC_OUTPUT) & 0x80,
            0x80,
            "Input Status 0 bit 7 reports it"
        );
    }

    #[test]
    fn clear_bit_is_also_inverted_zero_clears_one_leaves_it_armed() {
        let mut vram = [0u8; 16];
        let mut c = chip(&mut vram);
        // Enable (bit 5 clear) and arm (bit 4 set), then latch a retrace.
        c.reg_write(port::CRTC_INDEX_COLOR, 0x11);
        c.reg_write(port::CRTC_DATA_COLOR, 0x10);
        c.signal_vertical_retrace();
        assert!(c.irq_pending());

        // Writing bit 4 back to 1 (still enabled) must not itself clear
        // an already-latched interrupt -- only writing it to 0 does.
        c.reg_write(port::CRTC_INDEX_COLOR, 0x11);
        c.reg_write(port::CRTC_DATA_COLOR, 0x10);
        assert!(c.irq_pending(), "rewriting bit 4 high leaves it latched");

        // Writing bit 4 low (0) is the inverted-sense clear.
        c.reg_write(port::CRTC_INDEX_COLOR, 0x11);
        c.reg_write(port::CRTC_DATA_COLOR, 0x00);
        assert!(!c.irq_pending(), "bit 4 low acknowledges it");

        // The chip re-latches on the next retrace once re-armed, rather
        // than staying permanently disabled by having been cleared once
        // -- the shared INT2 line is level-triggered and re-asserted,
        // not a one-shot (`lib.rs`'s wiring comment).
        c.reg_write(port::CRTC_INDEX_COLOR, 0x11);
        c.reg_write(port::CRTC_DATA_COLOR, 0x10);
        c.signal_vertical_retrace();
        assert!(
            c.irq_pending(),
            "re-armed and re-latched on the next retrace"
        );
    }

    #[test]
    fn signal_without_arming_never_latches() {
        // A driver that leaves CR11 at its power-on value and just lets
        // frames tick past must never accumulate a pending interrupt --
        // this is the exact regression the whole feature must not cause.
        let mut vram = [0u8; 16];
        let mut c = chip(&mut vram);
        for _ in 0..10 {
            c.signal_vertical_retrace();
        }
        assert!(!c.irq_pending());
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

    #[test]
    fn cursor_palette_select_redirects_dac_writes_away_from_the_screen_palette() {
        // CL-GD54xx datasheet: SR12 bit 1 (Cursor Palette Select) steers
        // the DAC-data port to the hardware cursor's private two-colour
        // table instead of the main 256-entry palette, so a driver can
        // set the pointer's colours without touching the screen palette
        // entry at the same index. Picasso96's Cirrus driver does exactly
        // this for pointer index 0 (Copperline's `gd5426.rs` model
        // reproduces the same redirect, cross-checked against ours).
        let mut vram = [0u8; 16];
        let mut c = chip(&mut vram);
        c.reg_write(port::SEQ_INDEX, idx::SR_LOCK);
        c.reg_write(port::SEQ_DATA, idx::SR_LOCK_UNLOCKED);

        // Screen palette entry 0 starts out grey (as Workbench expects).
        c.reg_write(port::DAC_WRITE_INDEX, 0);
        c.reg_write(port::DAC_DATA, 0x2A);
        c.reg_write(port::DAC_DATA, 0x2A);
        c.reg_write(port::DAC_DATA, 0x2A);
        let grey = c.palette_argb(0);

        // Arm the cursor-colour redirect and program the cursor's
        // background (index 0) to red -- same index the driver just used
        // in the main palette, which is exactly the collision this
        // register exists to avoid.
        c.reg_write(port::SEQ_INDEX, idx::SR_CURSOR_ATTR);
        c.reg_write(port::SEQ_DATA, sr12::CURSOR_PALETTE_SELECT);
        c.reg_write(port::DAC_WRITE_INDEX, 0);
        c.reg_write(port::DAC_DATA, 0x3F);
        c.reg_write(port::DAC_DATA, 0x00);
        c.reg_write(port::DAC_DATA, 0x00);

        // The screen palette must be untouched by the cursor-colour write.
        assert_eq!(
            c.palette_argb(0),
            grey,
            "cursor colour write leaked into the screen palette"
        );

        // Reading back through the same redirect must see the cursor
        // colour, not the (unchanged) screen palette entry.
        c.reg_write(port::DAC_READ_INDEX, 0);
        assert_eq!(c.reg_read(port::DAC_DATA), 0x3F);
        assert_eq!(c.reg_read(port::DAC_DATA), 0x00);
        assert_eq!(c.reg_read(port::DAC_DATA), 0x00);

        // Clearing the bit resumes normal screen-palette access at the
        // same index, still showing the original grey.
        c.reg_write(port::SEQ_INDEX, idx::SR_CURSOR_ATTR);
        c.reg_write(port::SEQ_DATA, 0);
        c.reg_write(port::DAC_READ_INDEX, 0);
        assert_eq!(c.reg_read(port::DAC_DATA), 0x2A);
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

    // ---- BitBLT engine ---------------------------------------------------

    fn gr_write(c: &mut Cirrus542x, index: u8, value: u8) {
        c.reg_write(port::GR_INDEX, index);
        c.reg_write(port::GR_DATA, value);
    }

    fn gr_read(c: &mut Cirrus542x, index: u8) -> u8 {
        c.reg_write(port::GR_INDEX, index);
        c.reg_read(port::GR_DATA)
    }

    /// Program every blit register except the raster op (left at its
    /// default of 0, which is why every test that cares about the
    /// result explicitly sets `GR_BLIT_ROP` itself) and does not trigger
    /// the transfer — callers set anything else they need (ROP,
    /// fg/bg colours, transparency compare) before calling
    /// [`trigger_blit`].
    #[allow(clippy::too_many_arguments)]
    fn setup_blit(
        c: &mut Cirrus542x,
        width: usize,
        height: usize,
        dst_pitch: usize,
        src_pitch: usize,
        dst: usize,
        src: usize,
        mode: u8,
    ) {
        unlock(c);
        let w = width - 1;
        let h = height - 1;
        for (index, value) in [
            (idx::GR_BLIT_WIDTH_LO, w as u8),
            (idx::GR_BLIT_WIDTH_LO + 1, (w >> 8) as u8),
            (idx::GR_BLIT_HEIGHT_LO, h as u8),
            (idx::GR_BLIT_HEIGHT_LO + 1, (h >> 8) as u8),
            (idx::GR_BLIT_DST_PITCH_LO, dst_pitch as u8),
            (idx::GR_BLIT_DST_PITCH_LO + 1, (dst_pitch >> 8) as u8),
            (idx::GR_BLIT_SRC_PITCH_LO, src_pitch as u8),
            (idx::GR_BLIT_SRC_PITCH_LO + 1, (src_pitch >> 8) as u8),
            (idx::GR_BLIT_DST_ADDR_LO, dst as u8),
            (idx::GR_BLIT_DST_ADDR_LO + 1, (dst >> 8) as u8),
            (idx::GR_BLIT_DST_ADDR_LO + 2, (dst >> 16) as u8),
            (idx::GR_BLIT_SRC_ADDR_LO, src as u8),
            (idx::GR_BLIT_SRC_ADDR_LO + 1, (src >> 8) as u8),
            (idx::GR_BLIT_SRC_ADDR_LO + 2, (src >> 16) as u8),
            (idx::GR_BLIT_MODE, mode),
        ] {
            gr_write(c, index, value);
        }
    }

    fn trigger_blit(c: &mut Cirrus542x) {
        gr_write(c, idx::GR_BLIT_START_STATUS, blit_status::START);
    }

    fn read_range(c: &Cirrus542x, at: usize, len: usize) -> std::vec::Vec<u8> {
        (at..at + len).map(|i| c.vram_read(i)).collect()
    }

    #[test]
    fn solid_fill_rectangle_including_partial_width() {
        // A 4-wide, 3-tall rectangle inside an 8-byte-pitch buffer: the
        // 4 bytes past each row's width must stay untouched, proving the
        // fill respects width independently of pitch.
        let mut vram = [0u8; 32];
        let mut c = chip(&mut vram);
        unlock(&mut c); // GR33 (MODE_EXT) is an extended register
        gr_write(&mut c, idx::GR_FG0, 0xAB);
        gr_write(&mut c, idx::GR_BLIT_MODE_EXT, blit_modeext::SOLID_FILL);
        setup_blit(
            &mut c,
            4,
            3,
            8,
            0,
            0,
            0,
            blit_mode::PATTERN | blit_mode::COLOR_EXPAND,
        );
        gr_write(&mut c, idx::GR_BLIT_ROP, 0x0D); // D = S: the filled colour, unmixed
        trigger_blit(&mut c);

        for row in 0..3 {
            let base = row * 8;
            assert_eq!(read_range(&c, base, 4), [0xAB; 4], "row {row} filled");
            assert_eq!(
                read_range(&c, base + 4, 4),
                [0; 4],
                "row {row} past the fill width is untouched"
            );
        }
    }

    #[test]
    fn colour_expansion_from_monochrome_source_both_polarities() {
        // One source byte, 8 pixels, video (VRAM) colour expansion: bit
        // 7 (MSB) is pixel 0. Run it twice with complementary source
        // bytes to confirm "1 -> fg, 0 -> bg" isn't a coincidence of one
        // particular bit pattern.
        let mut vram = [0u8; 32];
        let mut c = chip(&mut vram);
        gr_write(&mut c, idx::GR_FG0, 0xEE);
        gr_write(&mut c, idx::GR_BG0, 0x11);
        c.vram_write(16, 0b1100_0001); // pixels: 1,1,0,0,0,0,0,1
        setup_blit(&mut c, 8, 1, 8, 0, 0, 16, blit_mode::COLOR_EXPAND);
        gr_write(&mut c, idx::GR_BLIT_ROP, 0x0D);
        trigger_blit(&mut c);
        assert_eq!(
            read_range(&c, 0, 8),
            [0xEE, 0xEE, 0x11, 0x11, 0x11, 0x11, 0x11, 0xEE]
        );

        c.vram_write(16, 0b0011_1110); // the exact bitwise complement
        setup_blit(&mut c, 8, 1, 8, 0, 0, 16, blit_mode::COLOR_EXPAND);
        gr_write(&mut c, idx::GR_BLIT_ROP, 0x0D);
        trigger_blit(&mut c);
        assert_eq!(
            read_range(&c, 0, 8),
            [0x11, 0x11, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0x11],
            "the complementary source flips every pixel's colour"
        );
    }

    #[test]
    fn screen_to_screen_move_forwards_backwards_and_overlapping() {
        let mut vram = [0u8; 32];
        let mut c = chip(&mut vram);
        for (i, b) in [1u8, 2, 3, 4].into_iter().enumerate() {
            c.vram_write(i, b);
        }
        setup_blit(&mut c, 4, 1, 8, 8, 16, 0, 0);
        gr_write(&mut c, idx::GR_BLIT_ROP, 0x0D);
        trigger_blit(&mut c);
        assert_eq!(read_range(&c, 16, 4), [1, 2, 3, 4], "plain forward move");

        // Overlapping move one byte further into memory: safe only
        // descending (each byte is read before its own address is ever
        // written), the same overlap-safe scroll idiom `blitter.rs`
        // covers for the Amiga blitter.
        for (i, b) in [10u8, 11, 12, 13, 14, 15, 16, 17].into_iter().enumerate() {
            c.vram_write(i, b);
        }
        setup_blit(&mut c, 6, 1, 8, 8, 7, 5, blit_mode::BACKWARDS);
        gr_write(&mut c, idx::GR_BLIT_ROP, 0x0D);
        trigger_blit(&mut c);
        assert_eq!(read_range(&c, 2, 6), [10, 11, 12, 13, 14, 15]);
    }

    #[test]
    fn transparency_skips_matching_source_pixels() {
        let mut vram = [0u8; 32];
        let mut c = chip(&mut vram);
        c.vram_write(0, 0x00); // matches the (default zero) compare colour
        c.vram_write(1, 0x7A); // does not
        c.vram_write(16, 0x55);
        c.vram_write(17, 0x55);
        // Compare colour and mask both left at their power-on zero:
        // "transparent" means "source byte is exactly zero".
        setup_blit(&mut c, 2, 1, 2, 2, 16, 0, blit_mode::TRANSPARENT);
        gr_write(&mut c, idx::GR_BLIT_ROP, 0x0D);
        trigger_blit(&mut c);
        assert_eq!(
            read_range(&c, 16, 2),
            [0x55, 0x7A],
            "the zero source byte left its destination alone"
        );
    }

    #[test]
    fn rops_match_independently_hand_derived_expectations() {
        // source = 0xF0 (1111_0000), dest = 0xCC (1100_1100) — chosen so
        // every one of the four (S,D) bit combinations appears somewhere
        // in the byte, and every expected result below is arithmetic
        // worked out by hand from that fact (AND/OR/XOR/NOT), never by
        // calling `apply_rop` itself: a self-consistent check can't catch
        // a wrong or transposed table (this project has shipped that bug
        // twice — see `apply_rop`'s doc comment).
        let cases: [(u8, u8); 16] = [
            (0x00, 0x00),        // always 0
            (0x05, 0xF0 & 0xCC), // S AND D  = 0xC0
            (0x06, 0xCC),        // D
            (0x09, 0xF0 & 0x33), // S AND NOT D = 0x30
            (0x0B, 0x33),        // NOT D
            (0x0D, 0xF0),        // S
            (0x0E, 0xFF),        // always 1
            (0x50, 0x0F & 0xCC), // NOT S AND D = 0x0C
            (0x59, 0xF0 ^ 0xCC), // S XOR D = 0x3C
            (0x6D, 0xF0 | 0xCC), // S OR D = 0xFC
            (0x90, 0x0F & 0x33), // NOT S AND NOT D = 0x03
            (0x95, 0xC3),        // NOT (S XOR D) = NOT 0x3C = 0xC3
            (0xAD, 0xF0 | 0x33), // S OR NOT D = 0xF3
            (0xD0, 0x0F),        // NOT S
            (0xD6, 0x0F | 0xCC), // NOT S OR D = 0xCF
            (0xDA, 0x0F | 0x33), // NOT S OR NOT D = 0x3F
        ];
        for (rop, expected) in cases {
            assert_eq!(
                apply_rop(rop, 0xF0, 0xCC),
                expected,
                "rop {rop:#04x}: source=0xF0 dest=0xCC"
            );
        }
        // The pair the datasheet table is easiest to transpose: NOR is
        // 0x90, NAND is 0xDA, not the other way around.
        assert_eq!(apply_rop(0x90, 0xFF, 0x00), 0x00, "NOR(1,0) = 0");
        assert_eq!(apply_rop(0x90, 0x00, 0x00), 0xFF, "NOR(0,0) = 1");
        assert_eq!(apply_rop(0xDA, 0xFF, 0xFF), 0x00, "NAND(1,1) = 0");
        assert_eq!(apply_rop(0xDA, 0xFF, 0x00), 0xFF, "NAND(1,0) = 1");
    }

    #[test]
    fn busy_bit_clears_so_a_polling_driver_makes_progress() {
        // An ordinary (VRAM-source) blit runs to completion synchronously
        // inside the register write that starts it — the same house
        // style as `crate::blitter` (module docs) — so the busy bit
        // never has a chance to observably read set.
        let mut vram = [0u8; 32];
        let mut c = chip(&mut vram);
        setup_blit(&mut c, 1, 1, 1, 1, 0, 0, 0);
        trigger_blit(&mut c);
        assert_eq!(
            gr_read(&mut c, idx::GR_BLIT_START_STATUS) & blit_status::BUSY_MASK,
            0,
            "immediate blit: never observably busy"
        );

        // A system-source blit genuinely waits: busy while data is still
        // outstanding, and clearing (letting a polling driver proceed)
        // only once the transfer's full byte count has arrived.
        setup_blit(&mut c, 4, 1, 0, 0, 8, 0, blit_mode::SYSTEM_SOURCE);
        gr_write(&mut c, idx::GR_BLIT_ROP, 0x0D);
        trigger_blit(&mut c);
        assert_ne!(
            gr_read(&mut c, idx::GR_BLIT_START_STATUS) & blit_status::BUSY_MASK,
            0,
            "armed and waiting for host data"
        );
        for byte in [1u8, 2, 3] {
            c.vram_write(0, byte); // address is irrelevant while armed
            assert_ne!(
                gr_read(&mut c, idx::GR_BLIT_START_STATUS) & blit_status::BUSY_MASK,
                0,
                "still short of the 4 bytes this transfer needs"
            );
        }
        c.vram_write(0, 4);
        assert_eq!(
            gr_read(&mut c, idx::GR_BLIT_START_STATUS) & blit_status::BUSY_MASK,
            0,
            "the 4th byte completes the transfer and runs it"
        );
        assert_eq!(read_range(&c, 8, 4), [1, 2, 3, 4]);
    }

    #[test]
    fn bounds_safety_wild_registers_never_panic() {
        // The guest picks every one of these; garbage must clip or wrap,
        // never panic or touch memory outside `vram`.
        let mut vram = [0u8; 16];
        let mut c = chip(&mut vram);
        setup_blit(
            &mut c,
            2048, // maximum width the register field can hold
            1024, // maximum height
            0x1FFF,
            0x1FFF,
            0x1F_FFFF, // maximum 21-bit destination address
            0x1F_FFFF,
            blit_mode::COLOR_EXPAND | blit_mode::TRANSPARENT | blit_mode::BACKWARDS,
        );
        gr_write(&mut c, idx::GR_BLIT_ROP, 0x59);
        trigger_blit(&mut c); // must not panic

        // A system-source transfer whose declared size dwarfs
        // `SYSTEM_BLIT_CAPACITY` must still accept bytes safely — never
        // growing without bound (no allocator) or panicking on an index
        // past the fixed buffer — however far short of completion it
        // still is.
        setup_blit(
            &mut c,
            2048,
            1024,
            0,
            0,
            0,
            0,
            blit_mode::SYSTEM_SOURCE | blit_mode::COLOR_EXPAND,
        );
        trigger_blit(&mut c);
        for _ in 0..(SYSTEM_BLIT_CAPACITY + 1024) {
            c.vram_write(0, 0xFF);
        }
        assert!(
            gr_read(&mut c, idx::GR_BLIT_START_STATUS) & blit_status::BUSY_MASK != 0,
            "still short of this (oversized) transfer's declared byte count"
        );
    }
}
