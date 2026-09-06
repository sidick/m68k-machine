//! `rtgboard` — the host's native display board (ADR 0002, option B/C).
//!
//! Implements `docs/adr-0002-rtg-on-generic-display-hardware.md`'s
//! "generic virtual board" tier: a linear VRAM aperture plus a small
//! register interface a driver programs a mode through. **Host side
//! only** — no P96 `.card` driver exists yet, exactly the increment
//! shape `hostblk.rs` and `input.rs` each went through first. See
//! `docs/rtgboard-protocol.md` for the register/wire contract a driver
//! author needs; this file explains *why*.
//!
//! # Why this board needs no accelerator
//!
//! ADR 0002's decisive, verified fact: a P96 driver that overrides no
//! render vector still draws a full desktop, because the P96 core
//! installs its own CPU (`*Default`) renderer into every accelerable
//! slot *before* the driver runs, and a driver accelerates by
//! overwriting slots rather than by negotiating. Decline everything and
//! P96 simply draws straight into this board's VRAM with the CPU. So
//! unlike `cirrus.rs` (a BitBLT engine, a RAMDAC, monitor-switch quirks —
//! modelled because Picasso96 ships a driver for that specific silicon),
//! this board carries no blitter, no palette hardware beyond nothing at
//! all, and no acceleration hooks. It needs exactly two things: VRAM the
//! guest CPU can write pixels into directly, and a way to tell the driver
//! what modes exist and to let it choose one.
//!
//! # Mode advertisement: a borrowed catalog, not a negotiation
//!
//! ADR 0002's "Mode setting" section is blunt about Phase 5: under UEFI,
//! GOP modes are enumerated and one is set **before** `ExitBootServices`;
//! afterwards the mode is fixed. The honest interface for that hardware
//! is "exactly these modes and no others" — not a negotiation, not a
//! claim of arbitrary resizability this board cannot back up on real
//! hardware.
//!
//! So [`RtgBoard::new`] takes a **caller-supplied, borrowed** mode
//! catalog (`&'a [ModeDescriptor]`, no allocator in this crate) rather
//! than synthesising one internally. [`reg::MODE_COUNT`]/
//! [`reg::MODE_INDEX`]/[`reg::MODE_WIDTH`]/[`reg::MODE_HEIGHT`]/
//! [`reg::MODE_FORMAT`] let a driver enumerate it before ever attempting
//! [`reg::COMMIT`]. This lets the same board type express both stories
//! ADR 0002 needs to cover with one mechanism:
//!
//! - A Linux-hosted machine with real KMS runtime modesetting: the board
//!   layer hands this board a rich catalog.
//! - A bare-metal Phase 5 board behind a fixed UEFI GOP mode: the board
//!   layer hands this board a **one-entry** catalog — the boot-time mode,
//!   and nothing else — so a driver's enumeration honestly reports "there
//!   is exactly one mode" instead of a richer list this host cannot
//!   deliver on.
//!
//! `machine-hosted`'s `--rtgboard WxHxFORMAT` flag (see that crate) picks
//! the second shape: one requested mode becomes a one-entry catalog,
//! matching what this project's own Phase 5 hardware will actually be
//! able to offer.
//!
//! # Register interface: the `hostblk.rs`/`input.rs` idiom, no doorbell
//!
//! One hot byte per 4-byte-aligned slot for every single-byte register
//! (`hostblk.rs`'s convention, inherited from `mirage.rs`); u32 registers
//! read/write as four big-endian byte lanes the same way. Anything not
//! named in [`reg`] reads `0` and discards writes — unimplemented board
//! space, not the wider bus's open-bus `0xFF`.
//!
//! Unlike `hostblk`'s doorbell-plus-completion-queue shape, mode
//! programming here is **synchronous**: [`RtgBoard::write`]'s
//! [`reg::COMMIT`] arm validates and applies (or rejects) the requested
//! mode inline, and the result is available the instant the driver reads
//! [`reg::STATUS`] back. There is no asynchronous boundary to defer
//! across (`hostblk`'s doorbell exists because a slow host disk, or a
//! future OPFS backend, cannot always answer synchronously; nothing
//! about validating four integers against a borrowed slice has an
//! equivalent latency), and therefore **no interrupt at all** — no
//! `INT_ENABLE`/`INT_STATUS` pair, no `irq_pending()`. A driver commits a
//! mode and reads the outcome back in the same handful of instructions;
//! there is nothing for INT2 to usefully announce.
//!
//! # `SET_*` / `CUR_*`: a staging area, so a rejected mode cannot
//! half-apply
//!
//! The brief's hostile-input list explicitly calls for "an unsupported
//! mode being refused rather than half-applied". This board makes that
//! structural rather than a rule the write path has to remember: a
//! driver stages a candidate mode into the `SET_*` registers
//! ([`reg::SET_WIDTH`]/`SET_HEIGHT`/`SET_FORMAT`/`SET_STRIDE`/
//! `SET_FB_OFFSET`), which are plain read/write scratch cells with no
//! effect on anything, and only [`reg::COMMIT`] ever reads them. On a
//! rejection [`RtgBoard::current`] (the `CUR_*` registers) is untouched —
//! there is no code path that writes it before every check has passed —
//! so "half a mode landed" is not a state this board's own field layout
//! can represent, not merely a case its tests happen to cover.
//!
//! # Hostile input (module docs, matching `hostblk.rs`'s posture)
//!
//! [`RtgBoard::commit`] validates, in order, and rejects with
//! [`reg::STATUS`]'s `REJECTED` bit (never a panic, never a partial
//! apply) on the first failure:
//!
//! - **Unknown mode.** `(width, height, format)` must exactly match one
//!   entry of the caller-supplied catalog — this board does not
//!   interpolate or approximate a requested geometry, per the "express
//!   exactly these modes and no others" reasoning above.
//! - **Stride shorter than a row.** `stride < width * bytes_per_pixel
//!   (format)` (module docs above, "Stride is not width" — ADR 0002 and
//!   the brief both call this out: a real host's `PixelsPerScanLine` is
//!   reported separately from width and is usually padded, so this board
//!   carries stride as its own field rather than deriving it, and still
//!   refuses a stride too short to hold the row it claims to describe).
//!   Checked with [`u32::checked_mul`] so a hostile width/format pair
//!   cannot overflow into passing.
//! - **Framebuffer past the end of VRAM.** `fb_offset + height * stride`
//!   must fit within the attached VRAM's actual length ([`u32::
//!   checked_mul`]/[`checked_add`] throughout, never a bare
//!   multiply/add) — "a framebuffer offset past the end of VRAM" from
//!   the brief's own list, and the same "ask the real backing store,
//!   never assume a range" discipline `hostblk.rs`'s buffer-bounds check
//!   uses via [`crate::GuestMemory`], applied here against this board's
//!   own borrowed slice directly since VRAM is this board's own
//!   aperture, not `MachineBus`'s general RAM.
//!
//! An out-of-range [`reg::MODE_INDEX`] is likewise refused cleanly: the
//! `MODE_WIDTH`/`MODE_HEIGHT`/`MODE_FORMAT` reads for it come back as
//! `0`/`0`/[`format::INVALID`] rather than reading past the catalog
//! slice or panicking.
//!
//! # Pixel format: explicit and unambiguous, on purpose
//!
//! ADR 0002 and the brief both cite the same failure mode: the wrong
//! `RGBFTYPE` (`R8G8B8A8` vs `A8R8G8B8`) gives a black screen with
//! byte-correct VRAM, because two formats can agree on bit depth and
//! disagree only on channel order. [`format`] therefore names byte
//! order explicitly rather than leaving it to a depth-only descriptor:
//! [`format::RGB_565`] is a big-endian 16-bit `RRRRRGGGGGGBBBBB` value;
//! [`format::RGBX_8888`]/[`format::BGRX_8888`] are named after UEFI's
//! own `EFI_GRAPHICS_PIXEL_FORMAT` enum
//! (`PixelRedGreenBlueReserved8BitPerColor`/
//! `PixelBlueGreenRedReserved8BitPerColor`, UEFI Spec §12.9) with the
//! same byte order, low address to high: R,G,B,X or B,G,R,X. Picking
//! GOP's own two formats (rather than inventing a third convention) is
//! deliberate: it is the concrete answer to "what does the guest talk to
//! when the host display is generic" ADR 0002 poses, and it means a
//! future Phase 5 board layer can advertise **exactly** the format its
//! real GOP framebuffer reports, with no translation step for this board
//! to get subtly wrong.
//!
//! # The mouse pointer is not free (driver-side note, recorded here early)
//!
//! With no hardware sprite, P96's core soft-renders the pointer into this
//! board's own framebuffer, and Intuition routes button events by pointer
//! position — so a P96 screen on this board needs the native `input`
//! card's absolute-position events reaching Intuition correctly
//! (`docs/input-protocol.md` §6-§7) before pointer interaction works at
//! all. Nothing to build here (this is driver-side `SoftSpriteFlags`/
//! render-vector work, `docs/adr-0002...md`'s "three costs this ADR
//! missed"), but worth recording in the protocol doc now rather than
//! rediscovering it when the `.card` is written, the way `input.rs`'s own
//! `IECLASS_NEWPOINTERPOS` correction was recorded before it was needed.

use crate::autoconfig::{BoardSpec, ERT_ZORROIII};

/// Reuses `hostblk`/`input`'s reserved manufacturer ID rather than
/// minting a third placeholder — all three are the same NDK 3.2
/// `libraries/configregs.h` "hacker" ID ($7DB, decimal 2011) reserved for
/// test use (`hostblk.rs`'s module docs carry the full story of why
/// `0xFFFF` was tried and rejected by real Kickstart 3.2.2). Still a
/// stand-in: a real registered number is needed before this ships on
/// hardware.
pub const MANUFACTURER: u16 = crate::hostblk::MANUFACTURER;

/// This card's product number under [`MANUFACTURER`] — distinct from
/// `mirage` (`0`), `hostblk` (`1`), `fastram` (`2`) and `input` (`3`).
pub const PRODUCT: u8 = 4;

/// The Zorro III AUTOCONFIG window: 16 MB, extended-table code 0 — see
/// `hostblk.rs`/`graffity.rs`'s module docs for why a 16 MB Zorro III
/// board's `er_Type` carries no size bits of its own.
pub const WINDOW_BYTES: u32 = 0x0100_0000;

/// `er_Flags` bit 4: this is a genuine Zorro III board.
const ERFF_ZORRO_III: u8 = 1 << 4;
/// `er_Flags` bit 5: `er_Type`'s size bits index the 16 MB-1 GB extended
/// table rather than Zorro II's 64 KB-8 MB one.
const ERFF_EXTENDED: u8 = 1 << 5;

/// Where the VRAM aperture starts within this board's own 16 MB
/// AUTOCONFIG window — 1 MB in, well clear of the register file (which
/// ends at [`reg::VERSION`], `0x4C`), with room to spare before it for
/// the register file to grow, the same reasoning `hostblk::ROM_BASE`
/// documents for its own DiagArea placement. VRAM runs from here to the
/// end of the window, so at most `WINDOW_BYTES - VRAM_BASE` (15 MB) is
/// reachable through this aperture; VRAM past that is simply
/// unaddressable, the same posture `graffity.rs` takes for an
/// undersized or oversized backing store.
pub const VRAM_BASE: u32 = 0x0010_0000;

/// The value [`reg::VERSION`] reports.
pub const PROTOCOL_VERSION: u32 = 1;

/// Sentinel [`reg::MODE_FORMAT`]/[`reg::CUR_FORMAT`] value meaning "no
/// such catalog entry" / "no mode currently applied" — safe because every
/// real [`format`] value is small and this crate controls the whole
/// enumeration, unlike a guest-chosen field that might legitimately use
/// any byte value.
pub mod format {
    /// 16 bits per pixel, big-endian `RRRRRGGGGGGBBBBB` — read a VRAM
    /// pixel with `u16::from_be_bytes`.
    pub const RGB_565: u8 = 0;
    /// 32 bits per pixel, byte order low-to-high address = R,G,B,pad —
    /// UEFI `PixelRedGreenBlueReserved8BitPerColor` (module docs).
    pub const RGBX_8888: u8 = 1;
    /// 32 bits per pixel, byte order low-to-high address = B,G,R,pad —
    /// UEFI `PixelBlueGreenRedReserved8BitPerColor` (module docs).
    pub const BGRX_8888: u8 = 2;
    /// "No such mode" / "no mode applied" sentinel — never a real format.
    pub const INVALID: u8 = 0xFF;

    /// Bytes one pixel of `format` occupies in VRAM, or `None` for an
    /// unrecognised format byte (hostile input: a `SET_FORMAT` the
    /// catalog never advertised is caught by [`super::RtgBoard::commit`]
    /// long before this matters for bounds math, but this function itself
    /// never guesses at an unknown value).
    pub fn bytes_per_pixel(format: u8) -> Option<u32> {
        match format {
            RGB_565 => Some(2),
            RGBX_8888 | BGRX_8888 => Some(4),
            _ => None,
        }
    }
}

/// One entry of a board's advertised mode catalog — module docs, "Mode
/// advertisement".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeDescriptor {
    pub width: u32,
    pub height: u32,
    pub format: u8,
}

/// Register offsets within this board's AUTOCONFIG window. See the
/// module docs for the reasoning; `docs/rtgboard-protocol.md` for the
/// full driver-facing contract.
pub mod reg {
    /// Number of entries in the advertised mode catalog. Read-only.
    pub const MODE_COUNT: u32 = 0x00;
    /// Selects which catalog entry [`MODE_WIDTH`]/[`MODE_HEIGHT`]/
    /// [`MODE_FORMAT`] describe. Write-only (reads `0`); out-of-range
    /// values are accepted (never rejected) but make the three read back
    /// as `0`/`0`/[`super::format::INVALID`].
    pub const MODE_INDEX: u32 = 0x04;
    /// `catalog[MODE_INDEX].width`, or `0` if `MODE_INDEX` is out of
    /// range. Read-only.
    pub const MODE_WIDTH: u32 = 0x08;
    /// `catalog[MODE_INDEX].height`, or `0` if out of range. Read-only.
    pub const MODE_HEIGHT: u32 = 0x0C;
    /// `catalog[MODE_INDEX].format`, or [`super::format::INVALID`] if out
    /// of range. Read-only.
    pub const MODE_FORMAT: u32 = 0x10;
    /// Candidate width to apply. Read/write scratch — [`COMMIT`] is the
    /// only register that ever reads it (module docs, "a staging area").
    pub const SET_WIDTH: u32 = 0x14;
    /// Candidate height. Same shape as [`SET_WIDTH`].
    pub const SET_HEIGHT: u32 = 0x18;
    /// Candidate pixel format ([`super::format`]). Same shape as
    /// [`SET_WIDTH`].
    pub const SET_FORMAT: u32 = 0x1C;
    /// Candidate stride, in bytes — **not derivable from width**; a
    /// driver must compute and supply its own (module docs, "Stride is
    /// not width"). Same shape as [`SET_WIDTH`].
    pub const SET_STRIDE: u32 = 0x20;
    /// Candidate framebuffer offset within VRAM, in bytes. Same shape as
    /// [`SET_WIDTH`].
    pub const SET_FB_OFFSET: u32 = 0x24;
    /// Any write attempts to validate and apply the `SET_*` registers as
    /// the new mode (module docs' "Hostile input" list has the exact
    /// checks). Write-only (reads `0`).
    pub const COMMIT: u32 = 0x28;
    /// Write-1-to-clear result of the most recent [`COMMIT`]: bit 0
    /// ([`status::REJECTED`]) or bit 1 ([`status::APPLIED`]), mutually
    /// exclusive per commit. Unlike `input.rs`'s `INT_STATUS`, clearing
    /// this is **not** gated on anything — there is no queue behind it to
    /// strand, so a driver may acknowledge it immediately.
    pub const STATUS: u32 = 0x2C;
    /// The currently applied mode's width, or `0` if no mode has ever
    /// been successfully committed. Read-only.
    pub const CUR_WIDTH: u32 = 0x30;
    /// The currently applied mode's height, or `0`. Read-only.
    pub const CUR_HEIGHT: u32 = 0x34;
    /// The currently applied mode's format, or [`super::format::INVALID`]
    /// if none applied. Read-only.
    pub const CUR_FORMAT: u32 = 0x38;
    /// The currently applied mode's stride, in bytes, or `0`. Read-only.
    pub const CUR_STRIDE: u32 = 0x3C;
    /// The currently applied mode's framebuffer offset within VRAM, or
    /// `0`. Read-only.
    pub const CUR_FB_OFFSET: u32 = 0x40;
    /// Total VRAM attached to this board, in bytes — the "a
    /// `CAPACITY`-style register where a driver would otherwise hardcode
    /// a limit" register the brief asks for (same reasoning as
    /// `hostblk::reg::SUBMIT_CAPACITY`). Read-only.
    pub const VRAM_BYTES: u32 = 0x44;
    /// Protocol version; `1` for `docs/rtgboard-protocol.md`. Read-only.
    pub const VERSION: u32 = 0x48;
}

/// [`reg::STATUS`] bit values.
pub mod status {
    /// The most recent [`reg::COMMIT`] was refused; [`reg::CUR_WIDTH`]
    /// etc. are unchanged from whatever they were before it.
    pub const REJECTED: u8 = 1 << 0;
    /// The most recent [`reg::COMMIT`] succeeded; [`reg::CUR_WIDTH`] etc.
    /// now reflect it.
    pub const APPLIED: u8 = 1 << 1;
}

/// The mode currently applied, if any (module docs, "a staging area").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AppliedMode {
    width: u32,
    height: u32,
    format: u8,
    stride: u32,
    fb_offset: u32,
}

/// `rtgboard`'s register file and VRAM aperture. See the module docs for
/// the protocol this implements and the reasoning behind it.
pub struct RtgBoard<'a> {
    vram: &'a mut [u8],
    catalog: &'a [ModeDescriptor],

    mode_index: u32,
    set_width: u32,
    set_height: u32,
    set_format: u8,
    set_stride: u32,
    set_fb_offset: u32,

    status: u8,
    current: Option<AppliedMode>,
}

impl<'a> RtgBoard<'a> {
    /// Build a board over caller-owned VRAM (borrowed, like every other
    /// aperture in this crate — no allocator) and a caller-owned mode
    /// catalog (module docs, "Mode advertisement": a rich list for a host
    /// with real modesetting, a single entry for a fixed-mode Phase 5
    /// board).
    pub fn new(vram: &'a mut [u8], catalog: &'a [ModeDescriptor]) -> Self {
        Self {
            vram,
            catalog,
            mode_index: 0,
            set_width: 0,
            set_height: 0,
            set_format: format::INVALID,
            set_stride: 0,
            set_fb_offset: 0,
            status: 0,
            current: None,
        }
    }

    /// The `BoardSpec` this card registers on the AUTOCONFIG chain: one
    /// Zorro III board, no DiagArea (`ERTF_DIAGVALID` unset) -- this
    /// increment carries no boot ROM and no driver, per the module docs
    /// and the brief's explicit scope (the same posture `input.rs`'s
    /// first, driver-less increment took).
    pub fn board_spec() -> BoardSpec {
        BoardSpec {
            board_type: ERT_ZORROIII, // extended-table code 0 == 16 MB
            product: PRODUCT,
            flags: ERFF_ZORRO_III | ERFF_EXTENDED,
            manufacturer: MANUFACTURER,
            serial: 0,
            init_diag_vec: 0,
            size_bytes: WINDOW_BYTES,
        }
    }

    /// Borrow this board's VRAM — the host-side hook a screenshot path
    /// uses to walk the currently applied mode's framebuffer (see
    /// [`Self::current_decoded`]), the same shape [`crate::graffity::
    /// Graffity::vram`] offers for the emulated-silicon path.
    pub fn vram(&self) -> &[u8] {
        self.vram
    }

    /// The currently applied mode, as `(width, height, format, stride,
    /// fb_offset)`, or `None` if no [`reg::COMMIT`] has ever succeeded.
    /// Host-side convenience for a present path; not itself part of the
    /// guest-visible register contract (a driver reads the `CUR_*`
    /// registers directly).
    pub fn current_mode(&self) -> Option<(u32, u32, u8, u32, u32)> {
        self.current
            .map(|m| (m.width, m.height, m.format, m.stride, m.fb_offset))
    }

    /// Validate `(width, height, format, stride, fb_offset)` against the
    /// catalog and this board's real VRAM length, and apply it if valid.
    /// Module docs, "Hostile input" — every check is independent and the
    /// first failure wins; [`Self::current`] is untouched on any
    /// rejection.
    fn commit(&mut self) {
        let ok = self.catalog.iter().any(|m| {
            m.width == self.set_width && m.height == self.set_height && m.format == self.set_format
        }) && Self::mode_fits(
            self.set_width,
            self.set_format,
            self.set_stride,
            self.set_fb_offset,
            self.set_height,
            self.vram.len() as u32,
        );

        if ok {
            self.current = Some(AppliedMode {
                width: self.set_width,
                height: self.set_height,
                format: self.set_format,
                stride: self.set_stride,
                fb_offset: self.set_fb_offset,
            });
            self.status = status::APPLIED;
        } else {
            self.status = status::REJECTED;
        }
    }

    /// `true` iff `stride` holds at least one whole row of `width` pixels
    /// at `format`, and `fb_offset + height * stride` fits within
    /// `vram_len` — every arithmetic step through `checked_*` so a
    /// hostile combination can only ever fail closed, never wrap into
    /// looking valid (module docs).
    fn mode_fits(
        width: u32,
        format: u8,
        stride: u32,
        fb_offset: u32,
        height: u32,
        vram_len: u32,
    ) -> bool {
        let Some(bpp) = format::bytes_per_pixel(format) else {
            return false;
        };
        let Some(row_bytes) = width.checked_mul(bpp) else {
            return false;
        };
        if stride < row_bytes {
            return false;
        }
        let Some(span) = height.checked_mul(stride) else {
            return false;
        };
        let Some(end) = fb_offset.checked_add(span) else {
            return false;
        };
        end <= vram_len
    }

    /// Read a byte of the register file, offset from this board's
    /// configured AUTOCONFIG base, or of the VRAM aperture above
    /// [`VRAM_BASE`]. See [`reg`] for the register layout;
    /// [`hostblk::Hostblk::read`](crate::hostblk::Hostblk::read)'s doc
    /// comment for why anything unnamed reads `0` rather than the wider
    /// bus's open-bus `0xFF`.
    pub fn read(&self, offset: u32) -> u8 {
        match offset {
            o if in_slot(o, reg::MODE_COUNT) => {
                byte_of(self.catalog.len() as u32, o - reg::MODE_COUNT)
            }
            o if in_slot(o, reg::MODE_INDEX) => 0, // write-only
            o if in_slot(o, reg::MODE_WIDTH) => byte_of(
                self.catalog_entry().map(|m| m.width).unwrap_or(0),
                o - reg::MODE_WIDTH,
            ),
            o if in_slot(o, reg::MODE_HEIGHT) => byte_of(
                self.catalog_entry().map(|m| m.height).unwrap_or(0),
                o - reg::MODE_HEIGHT,
            ),
            o if in_slot(o, reg::MODE_FORMAT) => low_byte(
                o,
                reg::MODE_FORMAT,
                self.catalog_entry()
                    .map(|m| m.format)
                    .unwrap_or(format::INVALID),
            ),
            o if in_slot(o, reg::SET_WIDTH) => byte_of(self.set_width, o - reg::SET_WIDTH),
            o if in_slot(o, reg::SET_HEIGHT) => byte_of(self.set_height, o - reg::SET_HEIGHT),
            o if in_slot(o, reg::SET_FORMAT) => low_byte(o, reg::SET_FORMAT, self.set_format),
            o if in_slot(o, reg::SET_STRIDE) => byte_of(self.set_stride, o - reg::SET_STRIDE),
            o if in_slot(o, reg::SET_FB_OFFSET) => {
                byte_of(self.set_fb_offset, o - reg::SET_FB_OFFSET)
            }
            o if in_slot(o, reg::COMMIT) => 0, // write-only
            o if in_slot(o, reg::STATUS) => low_byte(o, reg::STATUS, self.status),
            o if in_slot(o, reg::CUR_WIDTH) => byte_of(
                self.current.map(|m| m.width).unwrap_or(0),
                o - reg::CUR_WIDTH,
            ),
            o if in_slot(o, reg::CUR_HEIGHT) => byte_of(
                self.current.map(|m| m.height).unwrap_or(0),
                o - reg::CUR_HEIGHT,
            ),
            o if in_slot(o, reg::CUR_FORMAT) => low_byte(
                o,
                reg::CUR_FORMAT,
                self.current.map(|m| m.format).unwrap_or(format::INVALID),
            ),
            o if in_slot(o, reg::CUR_STRIDE) => byte_of(
                self.current.map(|m| m.stride).unwrap_or(0),
                o - reg::CUR_STRIDE,
            ),
            o if in_slot(o, reg::CUR_FB_OFFSET) => byte_of(
                self.current.map(|m| m.fb_offset).unwrap_or(0),
                o - reg::CUR_FB_OFFSET,
            ),
            o if in_slot(o, reg::VRAM_BYTES) => {
                byte_of(self.vram.len() as u32, o - reg::VRAM_BYTES)
            }
            o if in_slot(o, reg::VERSION) => byte_of(PROTOCOL_VERSION, o - reg::VERSION),
            o if (VRAM_BASE..VRAM_BASE.saturating_add(self.vram.len() as u32)).contains(&o) => {
                self.vram[(o - VRAM_BASE) as usize]
            }
            _ => 0,
        }
    }

    /// Write a byte of the register file, or of the VRAM aperture above
    /// [`VRAM_BASE`]. See [`Self::read`] for the offset layout.
    pub fn write(&mut self, offset: u32, value: u8) {
        match offset {
            o if in_slot(o, reg::MODE_INDEX) && o - reg::MODE_INDEX == 3 => {
                self.mode_index = value as u32;
            }
            o if in_slot(o, reg::MODE_INDEX) => {}
            o if in_slot(o, reg::SET_WIDTH) => {
                set_byte_of(&mut self.set_width, o - reg::SET_WIDTH, value)
            }
            o if in_slot(o, reg::SET_HEIGHT) => {
                set_byte_of(&mut self.set_height, o - reg::SET_HEIGHT, value)
            }
            o if in_slot(o, reg::SET_FORMAT) && o - reg::SET_FORMAT == 3 => {
                self.set_format = value;
            }
            o if in_slot(o, reg::SET_FORMAT) => {}
            o if in_slot(o, reg::SET_STRIDE) => {
                set_byte_of(&mut self.set_stride, o - reg::SET_STRIDE, value)
            }
            o if in_slot(o, reg::SET_FB_OFFSET) => {
                set_byte_of(&mut self.set_fb_offset, o - reg::SET_FB_OFFSET, value)
            }
            o if in_slot(o, reg::COMMIT) && o - reg::COMMIT == 3 => self.commit(),
            o if in_slot(o, reg::COMMIT) => {}
            o if in_slot(o, reg::STATUS) && o - reg::STATUS == 3 => {
                // Write-1-to-clear, ungated (module docs: no queue behind
                // this to strand, unlike `input::reg::INT_STATUS`).
                self.status &= !value;
            }
            o if in_slot(o, reg::STATUS) => {}
            o if (VRAM_BASE..VRAM_BASE.saturating_add(self.vram.len() as u32)).contains(&o) => {
                self.vram[(o - VRAM_BASE) as usize] = value;
            }
            _ => {}
        }
    }

    /// `catalog[mode_index]`, or `None` if `mode_index` is out of range
    /// (module docs, "Hostile input" — the register that names this
    /// index is write-only with no validation of its own, so a bad index
    /// is handled entirely at read time, structurally, rather than by
    /// rejecting the write).
    fn catalog_entry(&self) -> Option<&ModeDescriptor> {
        self.catalog.get(self.mode_index as usize)
    }
}

/// Whether `offset` falls in the 4-byte-aligned slot starting at `base`.
fn in_slot(offset: u32, base: u32) -> bool {
    (base..base + 4).contains(&offset)
}

/// A single-byte register's value at offset `base + 3` (the low-order
/// byte of the slot), `0` at the other three offsets in the slot --
/// `mirage.rs`/`hostblk.rs`/`input.rs`'s same convention.
fn low_byte(offset: u32, base: u32, value: u8) -> u8 {
    if offset - base == 3 {
        value
    } else {
        0
    }
}

/// Byte `lane` (0 = most significant) of a big-endian 32-bit register.
fn byte_of(value: u32, lane: u32) -> u8 {
    (value >> (8 * (3 - lane))) as u8
}

/// Replace byte `lane` of a big-endian 32-bit register, leaving the other
/// three bytes untouched.
fn set_byte_of(value: &mut u32, lane: u32, byte: u8) {
    let shift = 8 * (3 - lane);
    *value = (*value & !(0xFFu32 << shift)) | ((byte as u32) << shift);
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODES: &[ModeDescriptor] = &[
        ModeDescriptor {
            width: 640,
            height: 480,
            format: format::RGBX_8888,
        },
        ModeDescriptor {
            width: 800,
            height: 600,
            format: format::RGB_565,
        },
    ];

    fn read_u32(dev: &RtgBoard, base: u32) -> u32 {
        u32::from_be_bytes([
            dev.read(base),
            dev.read(base + 1),
            dev.read(base + 2),
            dev.read(base + 3),
        ])
    }

    fn select_mode(dev: &mut RtgBoard, index: u8) {
        dev.write(reg::MODE_INDEX + 3, index);
    }

    fn write_u32(dev: &mut RtgBoard, base: u32, value: u32) {
        let b = value.to_be_bytes();
        dev.write(base, b[0]);
        dev.write(base + 1, b[1]);
        dev.write(base + 2, b[2]);
        dev.write(base + 3, b[3]);
    }

    fn commit(dev: &mut RtgBoard) {
        dev.write(reg::COMMIT + 3, 0);
    }

    fn stage_and_commit(
        dev: &mut RtgBoard,
        width: u32,
        height: u32,
        format: u8,
        stride: u32,
        fb_offset: u32,
    ) {
        write_u32(dev, reg::SET_WIDTH, width);
        write_u32(dev, reg::SET_HEIGHT, height);
        dev.write(reg::SET_FORMAT + 3, format);
        write_u32(dev, reg::SET_STRIDE, stride);
        write_u32(dev, reg::SET_FB_OFFSET, fb_offset);
        commit(dev);
    }

    // ---- mode catalog enumeration -----------------------------------------

    #[test]
    fn mode_count_matches_the_supplied_catalog_length() {
        let mut vram = [0u8; 4096];
        let dev = RtgBoard::new(&mut vram, MODES);
        assert_eq!(read_u32(&dev, reg::MODE_COUNT), MODES.len() as u32);
    }

    #[test]
    fn selecting_a_valid_index_reports_that_catalog_entry() {
        let mut vram = [0u8; 4096];
        let mut dev = RtgBoard::new(&mut vram, MODES);
        select_mode(&mut dev, 1);
        assert_eq!(read_u32(&dev, reg::MODE_WIDTH), 800);
        assert_eq!(read_u32(&dev, reg::MODE_HEIGHT), 600);
        assert_eq!(dev.read(reg::MODE_FORMAT + 3), format::RGB_565);
    }

    #[test]
    fn an_out_of_range_index_reads_zeroed_fields_and_a_sentinel_format() {
        let mut vram = [0u8; 4096];
        let mut dev = RtgBoard::new(&mut vram, MODES);
        select_mode(&mut dev, 200);
        assert_eq!(read_u32(&dev, reg::MODE_WIDTH), 0);
        assert_eq!(read_u32(&dev, reg::MODE_HEIGHT), 0);
        assert_eq!(dev.read(reg::MODE_FORMAT + 3), format::INVALID);
    }

    #[test]
    fn version_register_reports_the_documented_protocol_version() {
        let mut vram = [0u8; 4096];
        let dev = RtgBoard::new(&mut vram, MODES);
        assert_eq!(read_u32(&dev, reg::VERSION), PROTOCOL_VERSION);
        assert_ne!(PROTOCOL_VERSION, 0);
    }

    #[test]
    fn vram_bytes_register_matches_the_real_backing_store() {
        let mut vram = [0u8; 12345];
        let dev = RtgBoard::new(&mut vram, MODES);
        assert_eq!(read_u32(&dev, reg::VRAM_BYTES), 12345);
    }

    // ---- mode programming and readback -------------------------------------

    #[test]
    fn a_catalog_mode_with_a_sufficient_stride_and_offset_is_applied() {
        let mut vram = [0u8; 640 * 480 * 4 + 16];
        let mut dev = RtgBoard::new(&mut vram, MODES);
        stage_and_commit(&mut dev, 640, 480, format::RGBX_8888, 640 * 4, 0);

        assert_eq!(dev.read(reg::STATUS + 3), status::APPLIED);
        assert_eq!(read_u32(&dev, reg::CUR_WIDTH), 640);
        assert_eq!(read_u32(&dev, reg::CUR_HEIGHT), 480);
        assert_eq!(dev.read(reg::CUR_FORMAT + 3), format::RGBX_8888);
        assert_eq!(read_u32(&dev, reg::CUR_STRIDE), 640 * 4);
        assert_eq!(read_u32(&dev, reg::CUR_FB_OFFSET), 0);
        assert_eq!(
            dev.current_mode(),
            Some((640, 480, format::RGBX_8888, 640 * 4, 0))
        );
    }

    #[test]
    fn status_write_one_to_clear_is_ungated_unlike_inputs_int_status() {
        let mut vram = [0u8; 640 * 480 * 4];
        let mut dev = RtgBoard::new(&mut vram, MODES);
        stage_and_commit(&mut dev, 640, 480, format::RGBX_8888, 640 * 4, 0);
        assert_eq!(dev.read(reg::STATUS + 3), status::APPLIED);
        dev.write(reg::STATUS + 3, status::APPLIED);
        assert_eq!(
            dev.read(reg::STATUS + 3),
            0,
            "write-1-to-clear must not be gated"
        );
    }

    // ---- stride independent of width ---------------------------------------

    #[test]
    fn a_padded_stride_wider_than_the_minimum_row_is_accepted_and_reported_exactly() {
        // GOP's PixelsPerScanLine is frequently padded past the true
        // width (module docs, "Stride is not width") -- this must be
        // representable and reported back exactly, not silently
        // recomputed from width.
        let padded_stride = 640 * 4 + 256; // padded well past the true row
        let mut vram = [0u8; 480 * (640 * 4 + 256) + 16];
        let mut dev = RtgBoard::new(&mut vram, MODES);
        stage_and_commit(&mut dev, 640, 480, format::RGBX_8888, padded_stride, 0);

        assert_eq!(dev.read(reg::STATUS + 3), status::APPLIED);
        assert_eq!(read_u32(&dev, reg::CUR_STRIDE), padded_stride);
    }

    // ---- unsupported mode refused, not half-applied ------------------------

    #[test]
    fn a_geometry_absent_from_the_catalog_is_rejected() {
        let mut vram = [0u8; 65536];
        let mut dev = RtgBoard::new(&mut vram, MODES);
        stage_and_commit(&mut dev, 1920, 1080, format::RGBX_8888, 1920 * 4, 0);
        assert_eq!(dev.read(reg::STATUS + 3), status::REJECTED);
        assert_eq!(dev.current_mode(), None, "a rejected commit must not apply");
    }

    #[test]
    fn a_rejected_commit_leaves_a_previously_applied_mode_untouched() {
        let mut vram = [0u8; 800 * 600 * 4 + 16];
        let mut dev = RtgBoard::new(&mut vram, MODES);
        stage_and_commit(&mut dev, 640, 480, format::RGBX_8888, 640 * 4, 0);
        assert_eq!(dev.read(reg::STATUS + 3), status::APPLIED);

        // Now stage something invalid and commit again.
        stage_and_commit(&mut dev, 99999, 99999, format::RGBX_8888, 4, 0);
        assert_eq!(dev.read(reg::STATUS + 3), status::REJECTED);
        assert_eq!(
            dev.current_mode(),
            Some((640, 480, format::RGBX_8888, 640 * 4, 0)),
            "a later rejection must not disturb the mode already applied"
        );
    }

    // ---- hostile input: stride shorter than a row --------------------------

    #[test]
    fn a_stride_shorter_than_one_row_is_rejected_cleanly() {
        let mut vram = [0u8; 65536];
        let mut dev = RtgBoard::new(&mut vram, MODES);
        // 640 * 4 = 2560 bytes/row at RGBX_8888; offer far less.
        stage_and_commit(&mut dev, 640, 480, format::RGBX_8888, 100, 0);
        assert_eq!(dev.read(reg::STATUS + 3), status::REJECTED);
        assert_eq!(dev.current_mode(), None);
    }

    // ---- hostile input: framebuffer past the end of VRAM -------------------

    #[test]
    fn a_framebuffer_offset_past_the_end_of_vram_is_rejected_cleanly() {
        let mut vram = [0u8; 640 * 480 * 4]; // exactly one frame, no room to spare
        let mut dev = RtgBoard::new(&mut vram, MODES);
        stage_and_commit(&mut dev, 640, 480, format::RGBX_8888, 640 * 4, 4096);
        assert_eq!(dev.read(reg::STATUS + 3), status::REJECTED);
        assert_eq!(dev.current_mode(), None);
    }

    #[test]
    fn an_overflowing_offset_and_stride_combination_does_not_wrap_into_passing() {
        let mut vram = [0u8; 65536];
        let mut dev = RtgBoard::new(&mut vram, MODES);
        stage_and_commit(
            &mut dev,
            800,
            600,
            format::RGB_565,
            u32::MAX - 10,
            u32::MAX - 10,
        );
        assert_eq!(dev.read(reg::STATUS + 3), status::REJECTED);
        assert_eq!(dev.current_mode(), None);
    }

    #[test]
    fn an_unrecognised_pixel_format_is_rejected_even_if_geometry_matches() {
        let mut vram = [0u8; 65536];
        let mut dev = RtgBoard::new(&mut vram, MODES);
        stage_and_commit(&mut dev, 640, 480, 0x7B, 640 * 4, 0);
        assert_eq!(dev.read(reg::STATUS + 3), status::REJECTED);
    }

    // ---- VRAM aperture reads and writes -------------------------------------

    #[test]
    fn vram_aperture_reads_and_writes_round_trip() {
        let mut vram = [0u8; 4096];
        let mut dev = RtgBoard::new(&mut vram, MODES);
        dev.write(VRAM_BASE, 0xAB);
        dev.write(VRAM_BASE + 1, 0xCD);
        assert_eq!(dev.read(VRAM_BASE), 0xAB);
        assert_eq!(dev.read(VRAM_BASE + 1), 0xCD);
        assert_eq!(dev.vram()[0], 0xAB);
        assert_eq!(dev.vram()[1], 0xCD);
    }

    #[test]
    fn vram_writes_past_the_real_backing_store_are_discarded_not_panics() {
        let mut vram = [0u8; 16];
        let mut dev = RtgBoard::new(&mut vram, MODES);
        dev.write(VRAM_BASE + 1000, 0xFF); // well past the 16-byte store
        assert_eq!(
            dev.read(VRAM_BASE + 1000),
            0,
            "unmapped, reads as unimplemented space"
        );
    }

    #[test]
    fn register_file_and_vram_aperture_do_not_alias() {
        let mut vram = [0u8; 4096];
        let mut dev = RtgBoard::new(&mut vram, MODES);
        write_u32(&mut dev, reg::SET_WIDTH, 0x11223344);
        // A register write must not land in VRAM, which starts well past
        // every register offset used above.
        assert_eq!(dev.vram()[0], 0);
    }

    #[test]
    fn unimplemented_offsets_between_the_register_file_and_vram_read_zero_and_discard_writes() {
        let mut vram = [0u8; 4096];
        let mut dev = RtgBoard::new(&mut vram, MODES);
        let gap = reg::VERSION + 0x40; // past every named register, before VRAM_BASE
        assert!(gap < VRAM_BASE);
        assert_eq!(dev.read(gap), 0);
        dev.write(gap, 0xFF);
        assert_eq!(dev.read(gap), 0);
    }

    // ---- AUTOCONFIG identity -------------------------------------------------

    #[test]
    fn board_spec_carries_a_distinct_product_and_no_diagarea_yet() {
        let spec = RtgBoard::board_spec();
        assert_eq!(spec.manufacturer, MANUFACTURER);
        assert_eq!(spec.product, PRODUCT);
        assert_ne!(spec.product, crate::mirage::PRODUCT);
        assert_ne!(spec.product, crate::hostblk::PRODUCT);
        assert_ne!(spec.product, crate::fastram::PRODUCT);
        assert_ne!(spec.product, crate::input::PRODUCT);
        assert_eq!(spec.size_bytes, WINDOW_BYTES);
        assert_eq!(
            spec.init_diag_vec, 0,
            "no DiagArea yet -- host side only, this increment"
        );
    }
}
