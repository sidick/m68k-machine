//! Machine core: the guest-visible address space of the m68k Machine.
//!
//! This crate is `#![no_std]` and has no runtime dependencies. It owns the
//! memory map (proposal `docs/m68k-machine-proposal.md` §6.1) and the
//! devices the OS spins on, but never the CPU: the m68k core is a
//! consumer of this bus, not part of it.
//!
//! # Memory map
//!
//! | Range | Contents |
//! |---|---|
//! | `$000000`-`$07FFFF` | ROM overlay while OVL is asserted (reads only) |
//! | `$000000`-`$1FFFFF` | 2 MB chip RAM, borrowed from the board layer |
//! | `$A00000`-`$BFFFFF` | CIA-A (odd bytes) and CIA-B (even bytes) |
//! | `$DFF000`-`$DFFFFF` | Custom chip registers, [`chipset`] |
//! | `$E00000`-`$E7FFFF` | AROS extended ROM, when a pair is loaded |
//! | `$F80000`-`$FFFFFF` | 512 KB Kickstart ROM window, read-only |
//! | everything else | open bus |
//!
//! Zorro II/III board space (MIRAGE, `hostblk`, Graffity, and now fast
//! RAM -- [`fastram`]) is discovered through [`autoconfig::AutoConfig`]
//! rather than fixed in this table, since AUTOCONFIG is what assigns
//! those boards' addresses at boot; none of it exists unless a board
//! layer attaches the corresponding board. Any address no attached board
//! has claimed falls through to the open-bus rule below, which is
//! exactly what an unpopulated real machine would do at it.
//!
//! # Open-bus rule
//!
//! An unanswered read returns `$FF` per byte (so `$FFFF` for a word,
//! `$FFFFFFFF` for a long), and a write to an unanswered address is
//! silently discarded. This is not a simplification of convenience: it is
//! proposal §6.1's open-bus rule, and it is load-bearing for boot
//! compatibility. Kickstart's Ramsey/Gary/Buster/Gayle/RTC probes are
//! written against Gary's real DTACK-timeout behaviour on a real Amiga
//! bus — they read a value and compare it, they do not rely on a bus
//! error exception — so reproducing "unpopulated address reads as all
//! ones" is what makes those probes fail cleanly instead of hanging or
//! crashing, and makes most of them unnecessary to stub out individually.

// `no_std` for real builds (including the aarch64-unknown-none target the
// board layers need); the `cfg(test)` unit tests below use `std::vec` for
// convenience and only run hosted, so `no_std` is relaxed for `cargo test`.
#![cfg_attr(not(test), no_std)]

pub mod autoconfig;
pub mod blitter;
pub mod block;
pub mod chipset;
pub mod cia;
pub mod cirrus;
pub mod display;
pub mod fastram;
pub mod graffity;
pub mod hostblk;
pub mod input;
pub mod mirage;
pub mod pci;
pub mod pcibridge;
pub mod pktport;
pub mod render;
pub mod rom;
pub mod rtgboard;

use autoconfig::AutoConfig;
use blitter::Blitter;
use block::BlockDevice;
use chipset::Chipset;
use cia::{Cia, CiaId, FloppyDrive, FloppyPresence};
use graffity::Graffity;
use hostblk::Hostblk;
use input::NativeInput;
use mirage::Mirage;
use pcibridge::PciBridge;
use pktport::Pktport;
use rtgboard::{ModeDescriptor, RtgBoard};

/// Size in bytes of the chip RAM region, `$000000`-`$1FFFFF` (2 MB).
///
/// 2 MB matches the ECS Agnus chip RAM ID that Kickstart's memory probe
/// expects (proposal §6.1).
pub const CHIP_RAM_SIZE: usize = 0x0020_0000;

/// First address of the chip RAM region.
pub const CHIP_RAM_BASE: u32 = 0x0000_0000;

/// First address one past the end of the chip RAM region.
pub const CHIP_RAM_END: u32 = CHIP_RAM_BASE + CHIP_RAM_SIZE as u32;

/// Size in bytes of the Kickstart ROM window, `$F80000`-`$FFFFFF` (512 KB).
pub const ROM_WINDOW_SIZE: usize = 0x0008_0000;

/// First address of the Kickstart ROM window.
pub const ROM_BASE: u32 = 0x00F8_0000;

/// First address one past the end of the Kickstart ROM window.
pub const ROM_END: u32 = ROM_BASE + ROM_WINDOW_SIZE as u32;

/// A byte value returned by every open-bus read.
pub const OPEN_BUS_BYTE: u8 = 0xFF;

/// A validated view onto this machine's actual RAM, for a device whose
/// transfer engine moves guest-supplied buffers directly -- `hostblk`'s
/// doorbell descriptor and per-sector transfer buffers are the motivating
/// case -- rather than one byte at a time through
/// [`MachineBus::read_byte`]/[`MachineBus::write_byte`].
///
/// `MachineBus` is the only thing that knows the *whole* memory map (every
/// region any board layer has attached, and where AUTOCONFIG placed each
/// one), so it is the only thing that can legitimately answer "is this
/// guest address actually RAM, and how much of it". A device holding a
/// hardcoded range instead -- `hostblk`'s buffer bounds check currently
/// compares against `0..CHIP_RAM_SIZE` directly -- hardcodes a guess about
/// a memory map that is decided at configuration time, and breaks again
/// the moment a second RAM region exists (fast RAM, here) or an
/// AUTOCONFIG-assigned base moves. This trait exists so a device can ask
/// instead of assuming: [`MachineBus`] implements it once, correctly, and
/// every future RAM region this machine ever grows only needs to be added
/// to that one implementation, not to every device that moves bytes
/// around.
pub trait GuestMemory {
    /// A read-only view of `len` bytes at guest address `addr`, or `None`
    /// if that whole span does not lie entirely within exactly one
    /// attached RAM region -- unmapped, out of bounds, straddling two
    /// regions, or `addr + len` overflowing `u32` all read as "no". Guest
    /// addresses are hostile input (proposal's standing rule, see
    /// `hostblk`'s own module docs): this must clip and reject, never
    /// panic or silently alias one region's bytes onto another's request.
    fn ram_slice(&self, addr: u32, len: u32) -> Option<&[u8]>;

    /// The mutable equivalent of [`GuestMemory::ram_slice`].
    fn ram_slice_mut(&mut self, addr: u32, len: u32) -> Option<&mut [u8]>;
}

/// This machine's actual guest RAM: chip RAM (always present) and fast
/// RAM (only once [`MachineBus::with_fast_ram`] attaches it), split out
/// of [`MachineBus`] so it can implement [`GuestMemory`] on its own.
///
/// The reason this needs to be its own field rather than two fields
/// among [`MachineBus`]'s many others: a device engine's `tick` takes a
/// `&mut dyn GuestMemory` (`hostblk::Hostblk::tick`/
/// `pktport::Pktport::tick`/`pcibridge::PciBridge::tick`), which used to
/// mean handing it the *whole* bus (since `MachineBus` was what
/// implemented `GuestMemory`) -- and `self` cannot be borrowed while
/// `self.hostblk` (say) already is, so `MachineBus::tick` had to lift
/// the device out of its `Option`, call it, and put it straight back,
/// just to break that aliasing for the duration of one call. With RAM
/// living in its own field, `&mut self.ram` and `&mut self.hostblk` are
/// a disjoint field borrow the compiler accepts directly: no `take()`,
/// no put-back, no comment explaining why the gap is unobservable.
///
/// `MachineBus` keeps a delegating [`GuestMemory`] impl (below) so
/// `machine-hosted`'s `screenshot.rs`/`introspect.rs`/`pktvol.rs` — all
/// of which just want "is this a RAM address" — need no change.
pub struct GuestRam<'a> {
    chip_ram: &'a mut [u8; CHIP_RAM_SIZE],
    /// Fast RAM (`fastram`), when the board layer has attached some via
    /// [`MachineBus::with_fast_ram`]. See that method's doc comment for
    /// why a machine with none attached is completely unaffected.
    fast_ram: Option<&'a mut [u8]>,
    /// AUTOCONFIG chain index fast RAM's single board landed at, once
    /// [`MachineBus::with_fast_ram`] has registered it.
    fast_ram_board: Option<usize>,
    /// Fast RAM's placed `(base, len)` window, cached from
    /// `autoconfig.placement(fast_ram_board)` by
    /// [`MachineBus::refresh_windows`] -- `None` until AUTOCONFIG has
    /// actually configured the board ([`MachineBus::with_fast_ram`] alone
    /// only adds it to the chain, it does not place it). Both the bus
    /// fast path ([`MachineBus::fast_region`]/`fast_region_mut`) and
    /// [`Self::fast_ram_span`] check this instead of asking
    /// [`AutoConfig`] to scan, which is the whole point: derived only
    /// from `autoconfig.placement`, never a constant
    /// (`docs/device-ledger.md`, standing rule 2). Refreshed at every
    /// placement mutation site, so it is never stale on a read.
    fast_window: Option<(u32, u32)>,
}

impl<'a> GuestRam<'a> {
    fn new(chip_ram: &'a mut [u8; CHIP_RAM_SIZE]) -> Self {
        Self {
            chip_ram,
            fast_ram: None,
            fast_ram_board: None,
            fast_window: None,
        }
    }

    /// `[addr, addr + len)`'s position within chip RAM, if it lies
    /// entirely inside it. Shared by both [`GuestMemory`] methods below.
    fn chip_ram_span(&self, addr: u32, len: u32) -> Option<core::ops::Range<usize>> {
        let end = addr.checked_add(len)?;
        if end > CHIP_RAM_END {
            return None;
        }
        let start = (addr - CHIP_RAM_BASE) as usize;
        Some(start..start + len as usize)
    }

    /// `[addr, addr + len)`'s position within fast RAM's *real* backing
    /// store, if fast RAM is attached, configured, and the whole span
    /// lies inside both its cached placed window ([`Self::fast_window`])
    /// and the actual buffer a board layer supplied (which may be
    /// shorter than the window -- [`MachineBus::with_fast_ram`]'s doc
    /// comment).
    fn fast_ram_span(&self, addr: u32, len: u32) -> Option<core::ops::Range<usize>> {
        let (base, window_len) = self.fast_window?;
        let end = addr.checked_add(len)?;
        if addr < base || end > base.checked_add(window_len)? {
            return None;
        }
        let start = (addr - base) as usize;
        let end = start + len as usize;
        (end <= self.fast_ram.as_ref()?.len()).then_some(start..end)
    }
}

impl<'a> GuestMemory for GuestRam<'a> {
    fn ram_slice(&self, addr: u32, len: u32) -> Option<&[u8]> {
        if let Some(span) = self.chip_ram_span(addr, len) {
            return Some(&self.chip_ram[span]);
        }
        if let Some(span) = self.fast_ram_span(addr, len) {
            return Some(&self.fast_ram.as_ref()?[span]);
        }
        None
    }

    fn ram_slice_mut(&mut self, addr: u32, len: u32) -> Option<&mut [u8]> {
        if let Some(span) = self.chip_ram_span(addr, len) {
            return Some(&mut self.chip_ram[span]);
        }
        if let Some(span) = self.fast_ram_span(addr, len) {
            return Some(&mut self.fast_ram.as_mut()?[span]);
        }
        None
    }
}

/// The m68k-visible address space of the machine.
///
/// `MachineBus` borrows its backing storage from the board layer rather
/// than owning it: chip RAM as a mutable fixed-size array reference, ROM
/// as a read-only byte slice. This crate has no allocator and no `alloc`
/// dependency, so the board layer is where that storage actually lives
/// (statically, or in whatever arena the platform provides).
///
/// ROM may be shorter than the 512 KB window; a real Kickstart is exactly
/// 512 KB, but this also supports smaller ROM images (e.g. test fixtures)
/// by mirroring the image across the window, matching how a real Amiga's
/// address decoder mirrors an undersized ROM across its window.
pub struct MachineBus<'a> {
    /// Chip RAM and fast RAM, split into their own [`GuestMemory`]
    /// implementer -- see [`GuestRam`]'s own doc comment for why.
    ram: GuestRam<'a>,
    rom: &'a [u8],
    /// AROS extended ROM at `$E00000`, empty when booting a single ROM.
    ext_rom: &'a [u8],

    pub chipset: Chipset,
    pub cia_a: Cia,
    pub cia_b: Cia,
    /// The blitter lives on the bus rather than in [`Chipset`] because
    /// it is the one chipset device that reaches into chip RAM, and the
    /// bus is what owns that.
    pub blitter: Blitter,
    /// Floppy drive status/control, supplying CIA-A PRA bits 2-5 from
    /// CIA-B PRB writes -- see [`cia::FloppyDrive`]'s doc comment. Lives
    /// on the bus, not on either `Cia`, because it is wired between the
    /// two chips (CIA-B drives it, CIA-A reads it back).
    pub floppy: FloppyDrive,
    /// The Zorro AUTOCONFIG chain (§9). Every expansion this machine
    /// offers is discovered through it.
    pub autoconfig: AutoConfig,

    /// The Graffity graphics card, when the board layer has attached
    /// one via [`Self::with_graphics`] or
    /// [`Self::with_graphics_zorro_iii`]. Absent by default -- with no
    /// card, nothing about today's boot path changes, since neither its
    /// AUTOCONFIG board(s) nor this field's routing branch exist.
    graphics: Option<Graffity<'a>>,
    /// AUTOCONFIG chain index for each of the attached card's own board
    /// indices (registration order), `None` where a variant leaves a
    /// slot unused (Zorro III only ever fills index 0). This is the
    /// entire seam between the bus and the card: `MachineBus` knows
    /// nothing about VRAM, registers, or any other aperture -- it only
    /// maps a chain index back to one of the card's board indices and
    /// hands the offset to [`Graffity::read`]/[`Graffity::write`], which
    /// decode from there.
    graphics_boards: [Option<usize>; graffity::MAX_GRAFFITY_BOARDS],

    /// MIRAGE's block plane (see [`mirage`]), when the board layer has
    /// attached at least one unit via [`Self::with_mirage`]. Absent by
    /// default -- with no call to `with_mirage`, neither its AUTOCONFIG
    /// board nor this field's routing branch exist, so a machine with no
    /// MIRAGE attached is unaffected. Kept as MIRAGE's own reference
    /// implementation, off the boot path, per `docs/device-ledger.md`
    /// (`hostblk` is what actually retired Gayle).
    mirage: Option<Mirage<'a>>,
    /// AUTOCONFIG chain index MIRAGE's single board landed at, once
    /// [`Self::with_mirage`] has registered it. The whole seam between
    /// this bus and [`mirage::Mirage`], the same shape as
    /// [`Self::graphics_boards`] but for a card with only one board.
    mirage_board: Option<usize>,

    /// `hostblk`'s doorbell block card (ADR 0003, [`hostblk`]), when the
    /// board layer has attached at least one unit via
    /// [`Self::with_hostblk`]. Absent by default -- with no call to
    /// `with_hostblk`, neither its AUTOCONFIG board nor this field's
    /// routing branch exist, so a machine with no `hostblk` attached is
    /// completely unaffected, the same guarantee [`Self::mirage`] and
    /// [`Self::graphics`] already give.
    hostblk: Option<Hostblk<'a>>,
    /// AUTOCONFIG chain index `hostblk`'s single board landed at, once
    /// [`Self::with_hostblk`] has registered it -- the same seam shape
    /// as [`Self::mirage_board`].
    hostblk_board: Option<usize>,

    // Fast RAM (`fastram`) lives on `self.ram` (a [`GuestRam`]) rather
    // than as fields here -- see [`GuestRam`]'s own doc comment. Absent
    // by default: with no call to [`Self::with_fast_ram`], neither its
    // AUTOCONFIG board nor any routing branch that reaches it exist, so
    // a machine with no fast RAM attached is completely unaffected, and
    // every existing baseline (planar and RTG) stays bit-identical (the
    // same guarantee [`Self::mirage`], [`Self::hostblk`] and
    // [`Self::graphics`] already give).
    /// The native input card ([`input`]), when the board layer has
    /// attached one via [`Self::with_input`]. Absent by default -- with
    /// no call to `with_input`, neither its AUTOCONFIG board nor this
    /// field's routing branch exist, so a machine with no input card
    /// attached is completely unaffected, the same guarantee
    /// [`Self::hostblk`]/[`Self::mirage`]/[`Self::graphics`] already
    /// give (device ledger, "Not built -- decided native-first").
    input: Option<NativeInput>,
    /// AUTOCONFIG chain index the input card's single board landed at,
    /// once [`Self::with_input`] has registered it -- the same seam
    /// shape as [`Self::mirage_board`]/[`Self::hostblk_board`].
    input_board: Option<usize>,

    /// The `pktport` DosPacket transport card (ADR 0004, [`pktport`]),
    /// when the board layer has attached one via [`Self::with_pktport`].
    /// Absent by default -- with no call to `with_pktport`, neither its
    /// AUTOCONFIG board nor this field's routing branch exist, so a
    /// machine with no `pktport` attached is completely unaffected, the
    /// same guarantee [`Self::hostblk`]/[`Self::mirage`]/[`Self::input`]
    /// already give.
    pktport: Option<Pktport<'a>>,
    /// AUTOCONFIG chain index `pktport`'s single board landed at, once
    /// [`Self::with_pktport`] has registered it -- the same seam shape
    /// as [`Self::mirage_board`]/[`Self::hostblk_board`].
    pktport_board: Option<usize>,

    /// The native RTG display board ([`rtgboard`], ADR 0002), when the
    /// board layer has attached one via [`Self::with_rtgboard`]. Absent
    /// by default -- with no call to `with_rtgboard`, neither its
    /// AUTOCONFIG board nor this field's routing branch exist, so a
    /// machine with no RTG board attached is completely unaffected, the
    /// same guarantee every other optional card on this bus gives.
    rtg: Option<RtgBoard<'a>>,
    /// AUTOCONFIG chain index the RTG board's single board landed at,
    /// once [`Self::with_rtgboard`] has registered it -- the same seam
    /// shape as [`Self::mirage_board`]/[`Self::hostblk_board`].
    rtg_board: Option<usize>,

    /// The `pcibridge` Zorro III shim (ADR 0005 stage 1, [`pcibridge`]),
    /// when the board layer has attached one via [`Self::with_pcibridge`].
    /// Absent by default -- a machine with no pcibridge attached is
    /// completely unaffected, the same guarantee every other optional
    /// card on this bus gives.
    pcibridge: Option<PciBridge<'a>>,
    /// AUTOCONFIG chain index `pcibridge`'s single board landed at, once
    /// [`Self::with_pcibridge`] has registered it -- the same seam shape
    /// as [`Self::mirage_board`]/[`Self::hostblk_board`].
    pcibridge_board: Option<usize>,

    /// `pcibridge`'s placed window, the same shape and the same reason,
    /// checked by [`Self::pcibridge_target`].
    pcibridge_window: Option<(u32, u32)>,

    /// While set, the ROM is mirrored over the bottom of the address
    /// space so the CPU's reset vector fetch from `$000000`/`$000004`
    /// lands in ROM. Real hardware does this with Gary, driven by CIA-A
    /// PRA bit 0 (OVL), which is high out of reset; the OS clears it
    /// early in the strap once it no longer needs ROM at zero.
    overlay: bool,

    /// CPU clocks accumulated by [`Self::tick`] since the last
    /// [`Self::flush`], not yet applied to the chipset/CIA state --
    /// `docs/bus-fast-path-plan.md` step 4. `tick` only ever grows this;
    /// `flush` is the one place that drains it, through the exact
    /// per-call code path ([`Self::tick_exact`]), so that arithmetic
    /// stays the single source of truth for what "applying N clocks"
    /// means.
    pending_clocks: u32,
    /// How many more clocks (from the state as of the last flush) can
    /// accumulate before something guest-visible could change on its
    /// own: the next raster line boundary (drives CIA-B TOD, VERTB at
    /// frame wrap, Graffity retrace, and the once-per-line device
    /// engines) and each CIA's next timer underflow or keyboard
    /// handshake step, whichever comes first. [`Self::tick`] flushes
    /// the instant accumulated clocks would reach this, so `tick`
    /// itself is the only place clocks are compared against it;
    /// recomputed at the end of every [`Self::flush`] and by any write
    /// that can move the deadline without going through `flush` (a CIA
    /// register write that starts, stops or reloads a timer, or changes
    /// CRA/CRB). The raster-line source alone bounds this by one line's
    /// clocks (`PAL_COLOUR_CLOCKS_PER_LINE * CPU_CLOCKS_PER_COLOUR_CLOCK`,
    /// 908), which is what keeps the STOP-path resync in
    /// `machine-hosted`'s `run.rs` (and the board `main.rs` loops)
    /// exact: it ticks in one-line slices and checks
    /// `pending_irq_level` after each, and a slice that size can never
    /// undershoot this deadline.
    next_event_clocks: u32,
}

/// The address range the ROM overlay covers while OVL is asserted:
/// `$000000`-`$07FFFF`, the size of one ROM image.
pub const OVERLAY_END: u32 = 0x0008_0000;

/// CIA address decode: the window both chips mirror across.
pub const CIA_BASE: u32 = 0x00A0_0000;
pub const CIA_END: u32 = 0x00C0_0000;

/// Custom chip register window, `$DFF000`-`$DFFFFF`.
pub const CUSTOM_BASE: u32 = 0x00DF_F000;
pub const CUSTOM_END: u32 = 0x00E0_0000;

/// Pins the fast path's chip-RAM branch ([`MachineBus::fast_region`]/
/// [`MachineBus::fast_region_mut`]): both answer for a plain
/// `CHIP_RAM_BASE..CHIP_RAM_END` match with no further exclusion, which
/// is only correct because neither the CIA window nor the custom
/// register window can ever start inside chip RAM's fixed range.
const _: () = assert!(CIA_BASE >= CHIP_RAM_END && CUSTOM_BASE >= CHIP_RAM_END);

/// CIA-A PRA bit 6: joystick/mouse port 0 fire button (pin 6, FIR0),
/// which is where the left mouse button actually lands — not the
/// chipset's `POTGOR`, which carries only the right and middle buttons.
pub const CIA_A_PRA_FIR0: u8 = 1 << 6;

/// CPU clocks per colour clock and per E-clock tick.
///
/// The machine presents a 68040 (proposal §6.2) but drives its timers
/// from the frame clock rather than a cycle-accurate model, so these are
/// ratios chosen to make the guest's notion of time correct, not a
/// claim about real 68040 bus timing.
///
/// The two must stay consistent with each other: the PAL E-clock is the
/// colour clock divided by 5 (3.546895 MHz / 5 = 709.379 kHz, Amiga
/// Hardware Reference Manual), so with 4 CPU clocks per colour clock one
/// E-clock tick is 4 x 5 = 20 CPU clocks. Getting this wrong scales
/// every CIA timer interval -- timer.device calibrates the VBlank rate
/// against the E-clock at boot and derives all of its delays from it, so
/// a doubled divider here made the guest's whole sense of time run half
/// speed.
pub const CPU_CLOCKS_PER_COLOUR_CLOCK: u32 = 4;
pub const CPU_CLOCKS_PER_ECLOCK: u32 = CPU_CLOCKS_PER_COLOUR_CLOCK * 5;

impl<'a> MachineBus<'a> {
    /// Build a bus over caller-owned chip RAM and ROM storage.
    ///
    /// `rom` must be non-empty if any ROM access is expected; an empty ROM
    /// slice makes the ROM window behave as open bus (reads all `$FF`)
    /// since there is nothing to mirror.
    pub fn new(chip_ram: &'a mut [u8; CHIP_RAM_SIZE], rom: &'a [u8]) -> Self {
        let mut bus = Self {
            ram: GuestRam::new(chip_ram),
            rom,
            ext_rom: &[],
            chipset: Chipset::new(),
            cia_a: Cia::new(CiaId::A),
            cia_b: Cia::new(CiaId::B),
            blitter: Blitter::new(),
            // No physical drive: this machine's honest hardware story
            // (proposal §3, §10.3 -- storage is MIRAGE over Zorro III,
            // never a floppy connector), and confirmed against real
            // hardware and Amiberry (both configured with zero drives
            // attached) to still reach Kickstart's no-boot-media screen
            // under both 1.3 and 3.2.3. `with_floppy` overrides this for
            // the `--floppy empty` diagnostic mode.
            floppy: FloppyDrive::new(FloppyPresence::None),
            autoconfig: AutoConfig::new(),
            graphics: None,
            graphics_boards: [None; graffity::MAX_GRAFFITY_BOARDS],
            mirage: None,
            mirage_board: None,
            hostblk: None,
            hostblk_board: None,
            input: None,
            input_board: None,
            pktport: None,
            pktport_board: None,
            rtg: None,
            rtg_board: None,
            pcibridge: None,
            pcibridge_board: None,
            pcibridge_window: None,
            overlay: true,
            pending_clocks: 0,
            // Placeholder, replaced by the `recompute_next_event` call
            // below -- a fresh chipset/pair of CIAs is fully idle, so
            // the real first deadline is the initial raster line
            // boundary, computed the same way any later one is rather
            // than duplicated here.
            next_event_clocks: 1,
        };
        bus.recompute_next_event();
        bus
    }

    /// Recompute [`Self::fast_window`] and [`Self::pcibridge_window`]
    /// from `self.autoconfig`'s current placements. There are exactly two
    /// places a placement can change: the AUTOCONFIG arm of
    /// [`Self::write_byte`] (`AutoConfig::configure`, reached only from
    /// there) and the [`Self::with_fast_ram`]/[`Self::with_pcibridge`]
    /// builders (which register the board but cannot place it --
    /// placement only ever happens later, through a guest write to the
    /// AUTOCONFIG window). Calling this at the end of all three, rather
    /// than threading a generation counter through `AutoConfig`, keeps
    /// the cache exactly as fresh as the placement it mirrors.
    fn refresh_windows(&mut self) {
        self.ram.fast_window = self
            .ram
            .fast_ram_board
            .and_then(|idx| self.autoconfig.placement(idx))
            .map(|p| (p.base, p.size_bytes));
        self.pcibridge_window = self
            .pcibridge_board
            .and_then(|idx| self.autoconfig.placement(idx))
            .map(|p| (p.base, p.size_bytes));

        debug_assert!(
            !self
                .ram
                .fast_window
                .is_some_and(|(base, len)| Self::overlaps_autoconfig_window(base, len)),
            "fast RAM's placed window must never overlap AUTOCONFIG's own"
        );
        debug_assert!(
            !self
                .pcibridge_window
                .is_some_and(|(base, len)| Self::overlaps_autoconfig_window(base, len)),
            "pcibridge's placed window must never overlap AUTOCONFIG's own"
        );
    }

    /// Whether `[base, base + len)` overlaps the AUTOCONFIG window itself
    /// -- checked by [`Self::refresh_windows`] because a board placed
    /// there would make the fast path answer for addresses that must
    /// keep going through [`AutoConfig::read`]/`write` instead (a real
    /// board can never land there either: AUTOCONFIG retires each board
    /// from the window as it is configured).
    fn overlaps_autoconfig_window(base: u32, len: u32) -> bool {
        let end = base.saturating_add(len);
        base < autoconfig::AUTOCONFIG_END && end > autoconfig::AUTOCONFIG_BASE
    }

    /// Attach a disk to MIRAGE unit `unit` (0-7, `mirage` module docs).
    /// Registers MIRAGE's single AUTOCONFIG board the first time this is
    /// called; further calls with other unit numbers just attach more
    /// units to the same card. Never called at all, the card and its
    /// address-space routing simply don't exist (this field's own doc
    /// comment) -- MIRAGE is kept as its own reference implementation,
    /// off the boot path, per `docs/device-ledger.md`.
    pub fn with_mirage(mut self, unit: u8, device: &'a mut dyn BlockDevice) -> Self {
        if self.mirage.is_none() {
            self.mirage_board = self.autoconfig.add_board(Mirage::board_spec());
            self.mirage = Some(Mirage::new());
        }
        if let Some(m) = &mut self.mirage {
            m.attach_unit(unit, device);
        }
        self
    }

    /// Attach a disk to `hostblk` unit `unit` (0-7, [`hostblk::UNIT_COUNT`]),
    /// optionally write-protected. Registers `hostblk`'s single Zorro III
    /// AUTOCONFIG board the first time this is called; further calls with
    /// other unit numbers just attach more units to the same card. Never
    /// called at all, the card and its address-space routing simply don't
    /// exist (this field's own doc comment) -- MIRAGE stays available
    /// regardless, and the two coexist.
    pub fn with_hostblk(
        mut self,
        unit: u8,
        device: &'a mut dyn BlockDevice,
        write_protect: bool,
    ) -> Self {
        if self.hostblk.is_none() {
            self.hostblk_board = self.autoconfig.add_board(Hostblk::board_spec());
            self.hostblk = Some(Hostblk::new());
        }
        if let Some(h) = &mut self.hostblk {
            h.attach_unit(unit, device, write_protect);
        }
        self
    }

    /// Attach a Zorro II Graffity graphics card over caller-owned VRAM
    /// (borrowed, like chip RAM and the ROMs -- this crate has no
    /// allocator) and register its two AUTOCONFIG boards on the chain.
    /// Absent a call to this (or [`Self::with_graphics_zorro_iii`]), the
    /// chain and every address this card would occupy are untouched,
    /// which is what keeps today's boot path identical with no card
    /// attached.
    pub fn with_graphics(self, vram: &'a mut [u8]) -> Self {
        self.attach_graphics(Graffity::new(vram))
    }

    /// Attach a Zorro III Graffity graphics card: the same core, one
    /// AUTOCONFIG board instead of two (`graffity` module docs).
    pub fn with_graphics_zorro_iii(self, vram: &'a mut [u8]) -> Self {
        self.attach_graphics(Graffity::new_zorro_iii(vram))
    }

    /// Register `card`'s boards on the chain, in the order it offers
    /// them, and remember which chain index landed at which of the
    /// card's own board indices -- the entire seam described on
    /// [`Self::graphics_boards`]. Shared by both `with_graphics*`
    /// constructors so neither knows anything about VRAM, registers, or
    /// any other aperture; only [`graffity::Graffity`] does.
    fn attach_graphics(mut self, card: Graffity<'a>) -> Self {
        let specs = card.board_specs();
        let mut chain = [None; graffity::MAX_GRAFFITY_BOARDS];
        for (board, &spec) in specs.as_slice().iter().enumerate() {
            chain[board] = self.autoconfig.add_board(spec);
        }
        self.graphics_boards = chain;
        self.graphics = Some(card);
        self
    }

    /// Attach fast RAM over caller-owned storage (borrowed, like chip RAM
    /// and VRAM -- this crate has no allocator) and register its single
    /// Zorro III AUTOCONFIG board (`fastram::autoconfig_board_spec`) on
    /// the chain. Absent a call to this, the chain and every address this
    /// board would occupy are untouched -- the same "nothing changes
    /// unless attached" guarantee [`Self::with_mirage`]/
    /// [`Self::with_hostblk`] already give, and what lets a caller opt
    /// out of fast RAM entirely (`machine-hosted`'s `--fast-ram-mb 0`)
    /// without perturbing any baseline that predates it.
    /// `mem.len()` need not be one of the extended-table's discrete
    /// sizes; the board declares the next size up and anything past the
    /// real buffer simply reads open bus / discards writes, same as an
    /// undersized VRAM backing store (`fastram` module docs).
    pub fn with_fast_ram(mut self, mem: &'a mut [u8]) -> Self {
        self.ram.fast_ram_board = self
            .autoconfig
            .add_board(fastram::autoconfig_board_spec(mem.len() as u32));
        self.ram.fast_ram = Some(mem);
        // Not yet placed (that happens later, through a guest AUTOCONFIG
        // write) so this is a no-op today, but keeps `fast_window` in
        // sync with `fast_ram_board` at every mutation site rather than
        // only some of them -- see `Self::refresh_windows`.
        self.refresh_windows();
        self
    }

    /// Attach the native input card ([`input`]). Idempotent -- a second
    /// call is a no-op, the same shape [`Self::with_hostblk`] uses for
    /// its own "register the board the first time" check, though this
    /// card never needs a second call for anything (no units, no
    /// backing storage to attach) the way `with_hostblk`/`with_mirage`
    /// do.
    pub fn with_input(mut self) -> Self {
        if self.input.is_none() {
            self.input_board = self.autoconfig.add_board(NativeInput::board_spec());
            self.input = Some(NativeInput::new());
        }
        self
    }

    /// Attach the `pktport` DosPacket transport card (ADR 0004, [`pktport`])
    /// over a caller-owned [`pktport::PacketBackend`] -- borrowed, like
    /// every other card's backing store on this bus (this crate has no
    /// allocator). Registers `pktport`'s single Zorro II AUTOCONFIG board.
    /// Absent a call to this, the chain and every address this card would
    /// occupy are untouched -- the same "nothing changes unless attached"
    /// guarantee [`Self::with_hostblk`]/[`Self::with_input`] already give.
    pub fn with_pktport(mut self, backend: &'a mut dyn pktport::PacketBackend) -> Self {
        self.pktport_board = self.autoconfig.add_board(Pktport::board_spec());
        self.pktport = Some(Pktport::new(backend));
        self
    }

    /// Attach the native RTG display board ([`rtgboard`], ADR 0002) over
    /// caller-owned VRAM (borrowed, like chip RAM and Graffity's VRAM --
    /// this crate has no allocator) and a caller-owned mode catalog --
    /// [`rtgboard`]'s module docs, "Mode advertisement": a rich list for
    /// a host with real modesetting, or a single entry for a fixed-mode
    /// Phase 5 board layer. Absent a call to this, the chain and every
    /// address this board would occupy are untouched, the same "nothing
    /// changes unless attached" guarantee every other optional card here
    /// gives -- this board carries no P96 `.card` driver yet (host side
    /// only, this increment), so attaching it with no guest-side driver
    /// installed is inert but harmless, the same shape `hostblk`'s and
    /// `input`'s own first, driver-less increments took.
    pub fn with_rtgboard(mut self, vram: &'a mut [u8], catalog: &'a [ModeDescriptor]) -> Self {
        self.rtg_board = self.autoconfig.add_board(RtgBoard::board_spec());
        self.rtg = Some(RtgBoard::new(vram, catalog));
        self
    }

    /// Attach the `pcibridge` Zorro III shim (ADR 0005 stage 1,
    /// [`pcibridge`]) over a caller-owned [`pci::PciBackend`] -- borrowed,
    /// like every other card's backing store on this bus (this crate has
    /// no allocator). Registers `pcibridge`'s single Zorro III AUTOCONFIG
    /// board. Absent a call to this, the chain and every address this
    /// card would occupy are untouched -- the same "nothing changes
    /// unless attached" guarantee every other optional card here gives.
    pub fn with_pcibridge(mut self, backend: &'a mut dyn pci::PciBackend) -> Self {
        self.pcibridge_board = self.autoconfig.add_board(PciBridge::board_spec());
        self.pcibridge = Some(PciBridge::new(backend));
        // See `with_fast_ram`'s matching call: a no-op until AUTOCONFIG
        // places the board, but keeps every mutation site consistent.
        self.refresh_windows();
        self
    }

    /// Attach an AROS extended ROM at `$E00000`.
    pub fn with_ext_rom(mut self, ext_rom: &'a [u8]) -> Self {
        self.ext_rom = ext_rom;
        self
    }

    /// Override the default no-drive floppy configuration -- see
    /// [`cia::FloppyPresence`].
    pub fn with_floppy(mut self, presence: FloppyPresence) -> Self {
        self.floppy = FloppyDrive::new(presence);
        self.sync_floppy_status();
        self
    }

    /// Whether the ROM overlay is currently mapped over low memory.
    pub fn overlay(&self) -> bool {
        self.overlay
    }

    /// Borrow the attached Graffity card, if [`Self::with_graphics`] was
    /// called. A caller driving a present path checks
    /// `graphics().and_then(|c| c.decoded_mode())` each frame to decide
    /// whether the card has anything ready to show; `None` here (no card
    /// attached) is indistinguishable from "attached but not yet
    /// programmed" as far as that caller is concerned, both meaning
    /// nothing to present.
    pub fn graphics(&self) -> Option<&Graffity<'a>> {
        self.graphics.as_ref()
    }

    /// Where each of the attached graphics card's own boards was placed
    /// by AUTOCONFIG, in the card's board order, or `None` for a board
    /// this variant does not use (Zorro III uses only index 0) or one
    /// the guest has not configured yet.
    ///
    /// Diagnostics want the *card's* boards, not whatever happens to sit
    /// at the front of the chain. Those were the same thing until fast
    /// RAM was added, and a reporter that probed chain slots 0 and 1
    /// then started attributing the memory board's base to the graphics
    /// card. Nothing depends on a card landing at a particular address --
    /// AUTOCONFIG assigns them dynamically on real hardware too, and this
    /// card genuinely moves when the chain changes -- so the mapping has
    /// to be followed rather than assumed.
    pub fn graphics_board_bases(&self) -> [Option<u32>; graffity::MAX_GRAFFITY_BOARDS] {
        let mut out = [None; graffity::MAX_GRAFFITY_BOARDS];
        for (slot, chain_index) in self.graphics_boards.iter().enumerate() {
            out[slot] = chain_index
                .and_then(|i| self.autoconfig.placement(i))
                .map(|p| p.base);
        }
        out
    }

    /// Borrow the attached MIRAGE card, if [`Self::with_mirage`] was
    /// called. Mutable access is what the board layer (or a test) uses
    /// to call [`mirage::Mirage::notify_media_change`] -- the host-side
    /// stand-in for the management-plane `ATTACH`/`DETACH` commands this
    /// increment doesn't build (`mirage` module docs).
    pub fn mirage(&self) -> Option<&Mirage<'a>> {
        self.mirage.as_ref()
    }

    /// Mutable access to the attached MIRAGE card, see [`Self::mirage`].
    pub fn mirage_mut(&mut self) -> Option<&mut Mirage<'a>> {
        self.mirage.as_mut()
    }

    /// Borrow the attached `hostblk` card, if [`Self::with_hostblk`] was
    /// called -- e.g. to call [`hostblk::Hostblk::notify_media_change`],
    /// the host-side hook standing in for a guest-triggered eject in this
    /// increment (`hostblk` module docs).
    pub fn hostblk(&self) -> Option<&Hostblk<'a>> {
        self.hostblk.as_ref()
    }

    /// Mutable access to the attached `hostblk` card, see
    /// [`Self::hostblk`].
    pub fn hostblk_mut(&mut self) -> Option<&mut Hostblk<'a>> {
        self.hostblk.as_mut()
    }

    /// Where AUTOCONFIG placed `hostblk`'s single Zorro III board, once
    /// `expansion.library` has configured it -- `None` before
    /// [`Self::with_hostblk`] was called, or before the guest has written
    /// the base-address sequence that configures it (`autoconfig`'s
    /// module docs).
    ///
    /// This is what a host-side introspection tool (`machine-hosted`'s
    /// `--inspect`) cross-checks against guest memory to confirm
    /// Kickstart actually accepted this board: our own bus knowing where
    /// it *offered* to place a board proves nothing about whether the
    /// guest's own `ConfigDev` for it agrees -- the same "silent
    /// rejection" trap `device-ledger.md`'s "rule for addresses" section
    /// names for a `MEMLIST` board using the wrong manufacturer ID.
    pub fn hostblk_board_base(&self) -> Option<u32> {
        self.autoconfig
            .placement(self.hostblk_board?)
            .map(|p| p.base)
    }

    /// Borrow the attached native input card, if [`Self::with_input`] was
    /// called.
    pub fn input(&self) -> Option<&NativeInput> {
        self.input.as_ref()
    }

    /// Mutable access to the attached input card, see [`Self::input`] --
    /// what a host-side event source (`machine-hosted`'s `--input-script`)
    /// uses to call [`input::NativeInput::push_key`]/`push_button`/
    /// `push_pointer_motion`.
    pub fn input_mut(&mut self) -> Option<&mut NativeInput> {
        self.input.as_mut()
    }

    /// Where AUTOCONFIG placed the input card's single Zorro III board,
    /// once `expansion.library` has configured it -- `None` before
    /// [`Self::with_input`] was called or before the guest has configured
    /// it. See [`Self::hostblk_board_base`], the same shape.
    pub fn input_board_base(&self) -> Option<u32> {
        self.autoconfig.placement(self.input_board?).map(|p| p.base)
    }

    /// Borrow the attached `pktport` card, if [`Self::with_pktport`] was
    /// called.
    pub fn pktport(&self) -> Option<&Pktport<'a>> {
        self.pktport.as_ref()
    }

    /// Mutable access to the attached `pktport` card, see
    /// [`Self::pktport`].
    pub fn pktport_mut(&mut self) -> Option<&mut Pktport<'a>> {
        self.pktport.as_mut()
    }

    /// Where AUTOCONFIG placed `pktport`'s single Zorro II board, once
    /// `expansion.library` has configured it -- `None` before
    /// [`Self::with_pktport`] was called or before the guest has
    /// configured it. See [`Self::hostblk_board_base`], the same shape.
    pub fn pktport_board_base(&self) -> Option<u32> {
        self.autoconfig
            .placement(self.pktport_board?)
            .map(|p| p.base)
    }

    /// Borrow the attached RTG display board, if [`Self::with_rtgboard`]
    /// was called -- e.g. for a screenshot path to walk its currently
    /// applied mode via [`rtgboard::RtgBoard::current_mode`]/
    /// [`rtgboard::RtgBoard::vram`], the same shape [`Self::graphics`]
    /// offers for the emulated-silicon path.
    pub fn rtgboard(&self) -> Option<&RtgBoard<'a>> {
        self.rtg.as_ref()
    }

    /// Mutable access to the attached RTG board, see [`Self::rtgboard`].
    pub fn rtgboard_mut(&mut self) -> Option<&mut RtgBoard<'a>> {
        self.rtg.as_mut()
    }

    /// Where AUTOCONFIG placed the RTG board's single Zorro III board,
    /// once `expansion.library` has configured it -- `None` before
    /// [`Self::with_rtgboard`] was called or before the guest has
    /// configured it. See [`Self::hostblk_board_base`], the same shape.
    pub fn rtgboard_base(&self) -> Option<u32> {
        self.autoconfig.placement(self.rtg_board?).map(|p| p.base)
    }

    /// Borrow the attached `pcibridge` card, if [`Self::with_pcibridge`]
    /// was called.
    pub fn pcibridge(&self) -> Option<&PciBridge<'a>> {
        self.pcibridge.as_ref()
    }

    /// Mutable access to the attached `pcibridge` card, see
    /// [`Self::pcibridge`].
    pub fn pcibridge_mut(&mut self) -> Option<&mut PciBridge<'a>> {
        self.pcibridge.as_mut()
    }

    /// Where AUTOCONFIG placed `pcibridge`'s single Zorro III board, once
    /// `expansion.library` has configured it -- `None` before
    /// [`Self::with_pcibridge`] was called or before the guest has
    /// configured it. See [`Self::hostblk_board_base`], the same shape.
    pub fn pcibridge_board_base(&self) -> Option<u32> {
        self.autoconfig
            .placement(self.pcibridge_board?)
            .map(|p| p.base)
    }

    /// Advance time by `cpu_clocks`. Call this from the CPU's `sync` hook
    /// so device time and guest time stay in step.
    ///
    /// This only accumulates into [`Self::pending_clocks`] and returns;
    /// the chipset/CIA state is not touched unless the accumulated total
    /// reaches [`Self::next_event_clocks`], in which case [`Self::flush`]
    /// applies it (through [`Self::tick_exact`], the code below -- moved
    /// there unchanged) and recomputes the next deadline
    /// (`docs/bus-fast-path-plan.md` step 4). This is exact, not an
    /// approximation: nothing observable can change between two flushes
    /// by construction of `next_event_clocks` (the nearest raster line
    /// boundary and each CIA's nearest timer underflow/keyboard step),
    /// and every accessor that could read or change that state
    /// (`read_cia`/`write_cia`, `read_custom_word`/`write_custom_word`,
    /// `pending_irq_level`, and `machine-hosted`'s/the boards' readers of
    /// `chipset.frames` through [`Self::frames`]) flushes first. A single
    /// call's `cpu_clocks` can overshoot the deadline (this only checks
    /// once, after adding this call's share) rather than being sliced to
    /// land exactly on it -- `tick_exact` is already correct for an
    /// arbitrarily large batch (the STOP-path resync ticks whole raster
    /// lines at once today, and coarser batches than that crossing
    /// several frames at once were already exact before this), so
    /// flushing everything accumulated so far, however much that is, is
    /// equivalent to flushing in smaller steps as long as nothing
    /// observed the state in between -- which holds here since the
    /// overshoot is bounded by one instruction's own `cpu_clocks` (this
    /// function runs after every retired instruction, so at most one
    /// call's worth ever accumulates past the deadline before the check
    /// below catches it).
    pub fn tick(&mut self, cpu_clocks: u32) {
        self.pending_clocks = self.pending_clocks.saturating_add(cpu_clocks);
        if self.pending_clocks >= self.next_event_clocks {
            self.flush();
        }
    }

    /// Apply every clock accumulated by [`Self::tick`] since the last
    /// flush, through the exact per-call path ([`Self::tick_exact`]), and
    /// recompute [`Self::next_event_clocks`] from the state that leaves.
    /// A no-op when nothing is pending (the common case for every
    /// accessor below that calls this defensively before reading state
    /// `tick` might not have caught up on yet).
    fn flush(&mut self) {
        if self.pending_clocks == 0 {
            return;
        }
        let clocks = core::mem::replace(&mut self.pending_clocks, 0);
        self.tick_exact(clocks);
        self.recompute_next_event();
    }

    /// Recompute [`Self::next_event_clocks`] from the chipset/CIA state
    /// as of the last flush: the nearest of the next raster line
    /// boundary and each CIA's next timer underflow or keyboard
    /// handshake step. Called at the end of every [`Self::flush`], and
    /// by any write that can move the deadline without going through one
    /// (a CIA register write that starts, stops, reloads a timer, or
    /// changes CRA/CRB -- [`Self::write_cia`]).
    fn recompute_next_event(&mut self) {
        let mut deadline = self
            .chipset
            .clocks_until_line_boundary(CPU_CLOCKS_PER_COLOUR_CLOCK);
        if let Some(c) = self.cia_a.clocks_until_event(CPU_CLOCKS_PER_ECLOCK) {
            deadline = deadline.min(c);
        }
        if let Some(c) = self.cia_b.clocks_until_event(CPU_CLOCKS_PER_ECLOCK) {
            deadline = deadline.min(c);
        }
        // Never 0 in practice (every source above reports at least one
        // clock away), but `.max(1)` keeps `tick`'s `>=` check from ever
        // spinning on a zero deadline if that invariant is ever broken --
        // hostile-input-style defence against this crate's own state,
        // not guest input.
        self.next_event_clocks = deadline.max(1);
    }

    /// The exact per-call application of `cpu_clocks`: the whole of what
    /// [`Self::tick`] used to do directly, before step 4's lazy
    /// accumulation. This is what [`Self::flush`] calls, and the single
    /// source of truth the differential test (`lib.rs` `tests` module,
    /// `lazy_tick_matches_eager_tick`) checks the lazy path against by
    /// calling this directly on a second bus every step instead of going
    /// through `tick`/`flush`.
    ///
    /// The MIRAGE, `hostblk`, `pktport` and `pcibridge` engines (and
    /// their `irq_pending()` polls) run once per raster line crossed,
    /// not once per call -- see the comment at their call site below.
    /// Chipset and both CIAs advance on every call regardless.
    fn tick_exact(&mut self, cpu_clocks: u32) {
        let beam = self.chipset.tick(cpu_clocks, CPU_CLOCKS_PER_COLOUR_CLOCK);

        // The CIAs' TOD counters are the OS's wall clock, and each is
        // wired to a different edge of the same frame clock on real
        // hardware: CIA-A counts vertical blanks, CIA-B counts raster
        // lines. Driving both from the beam keeps guest time coherent
        // with VERTB rather than drifting against it.
        // One TOD tick per frame *crossed*, not per call: a host resyncing
        // a STOPped CPU ticks in coarse multi-frame batches, and dropping
        // the extra wraps would run the OS wall clock (and every
        // timer.device delay measured against it) slow.
        for _ in 0..beam.frames_wrapped {
            self.cia_a.tod_tick();
        }
        for _ in 0..beam.lines_started {
            self.cia_b.tod_tick();
        }

        // Graffity's vertical-retrace interrupt rides the same frame
        // clock as the CIAs' TOD counters above, rather than a second
        // clock of its own -- see the `graffity` module docs. A tick
        // spanning several frames (a STOPped CPU resyncing) must signal
        // each boundary crossed, the same "count, not a flag" reasoning
        // `BeamAdvance::frames_wrapped` already documents for the TOD
        // ticks.
        if let Some(card) = &mut self.graphics {
            for _ in 0..beam.frames_wrapped {
                card.signal_vertical_retrace();
            }
            // Zorro's INT2 pin is the same physical, level-triggered
            // line CIA-A already drives on this machine -- Graffity is
            // simply another source pulling it.
            if card.irq_pending() {
                self.chipset.raise_int(chipset::intbit::PORTS);
            }
        }

        // The four deferred-completion engines -- MIRAGE, `hostblk`,
        // `pktport`, and `pcibridge`'s virtio-net rings -- and their
        // `irq_pending()` polls advance once per raster line *crossed*,
        // not once per call (`docs/bus-fast-path-plan.md` step 4.1).
        // Each is already a "deferred completion, one step per call"
        // state machine (module docs), and one line's worth of latency
        // (~908 CPU clocks, ~100 instructions at this machine's
        // timebase) is invisible to a correct guest driver. It is also
        // exactly the grain the STOP-path resync in `machine-hosted`'s
        // `run.rs`/the board `main.rs` hook loops already ticks in
        // (`STOP_TICK_SLICE`, one line per slice), so that resync still
        // services every engine every slice. A normal per-instruction
        // tick spans far less than one line, so gating on
        // `beam.lines_started > 0` skips all four on most calls and
        // still runs each at most once the rare time a call does cross a
        // line (not scaled by how many lines were crossed: the same "at
        // most one step" contract as before).
        //
        // The first attempt at this wedged a real Kickstart 3.2.2 boot
        // after four FFS reads, and was reverted. The cause was a guest
        // driver bug that per-instruction ticking had been hiding:
        // `hostblk.device`'s `BeginIO` never set the request's `ln_Type`
        // to `NT_MESSAGE`, so ROM FFS's reused IORequest still read as
        // `NT_REPLYMSG` from its previous completion, and exec's DoIO
        // returned without waiting whenever the reply had not already
        // landed. See `m68k/hostblk-rom/hostblk-diagrom.s`'s
        // `dev_beginio` header for the trace. The virtio-net real-ROM
        // failure seen at the same time was the net harness counting its
        // reply cooldown in `poll_receive` calls, now counted in guest
        // frames (`machine-hosted`'s `netharness.rs`).
        //
        // Each engine needs a `&mut dyn GuestMemory` view of guest RAM
        // (it asks which addresses are RAM rather than assuming a range,
        // so it can reach fast RAM as well as chip RAM) -- `&mut
        // self.ram` and `&mut self.hostblk` (etc.) are different fields,
        // so the compiler accepts both borrows at once with no
        // `Option::take()`/put-back dance (`GuestRam`'s own doc comment
        // on why that dance existed before this field split).
        //
        // `pcibridge`'s own register file has no engine to advance
        // (config cycles stay synchronous, module docs), but ADR 0005
        // stage 3's virtio-net function behind it does. Ticking it here
        // is also what keeps its `INTx` poll honest: unlike every
        // register-file write path, a used buffer added by this tick can
        // raise the card's `INTx` line with no register write involved
        // at all, the same reason Graffity's card is polled above rather
        // than only checked after a write.
        if beam.lines_started > 0 {
            if let Some(m) = &mut self.mirage {
                m.tick();
                if m.irq_pending() {
                    self.chipset.raise_int(chipset::intbit::PORTS);
                }
            }
            if let Some(h) = &mut self.hostblk {
                h.tick(&mut self.ram);
                if h.irq_pending() {
                    self.chipset.raise_int(chipset::intbit::PORTS);
                }
            }
            if let Some(p) = &mut self.pktport {
                p.tick(&mut self.ram);
                if p.irq_pending() {
                    self.chipset.raise_int(chipset::intbit::PORTS);
                }
            }
            if let Some(dev) = &mut self.pcibridge {
                dev.tick(&mut self.ram);
                if dev.irq_pending() {
                    self.chipset.raise_int(chipset::intbit::PORTS);
                }
            }
        }

        if self.cia_a.tick(cpu_clocks, CPU_CLOCKS_PER_ECLOCK) {
            self.chipset.raise_int(chipset::intbit::PORTS);
        }
        if self.cia_b.tick(cpu_clocks, CPU_CLOCKS_PER_ECLOCK) {
            self.chipset.raise_int(chipset::intbit::EXTER);
        }
    }

    /// Re-raise every level-triggered interrupt source that is still
    /// asserting, independent of ticking.
    ///
    /// `tick_exact` raises PORTS/EXTER from `cia_a`/`cia_b.irq_pending()`
    /// and PORTS from the Graffity card's `irq_pending()` on *every*
    /// call, which is how the eager path kept re-raising a still-pending
    /// source the instant after the guest cleared its `INTREQ` bit but
    /// before the source itself let go (the CIA's `icr_data & icr_mask`
    /// stays nonzero until its own `ICR` register is read, and Graffity
    /// re-acknowledges on its own schedule) -- these are level-triggered
    /// lines, not edge-triggered ones, so `INTREQ` clearing the latch
    /// once is not the same as the source going away. Under step 4 that
    /// per-call poll only runs at a flush, which could be a whole
    /// deadline away, so any write that can either newly enable a
    /// latched source (an ICR mask write) or clear a bit a live source
    /// still drives (an `INTREQ`/`INTENA` write, or a card register
    /// write) must call this itself so the guest observes the
    /// re-assertion at the same instruction boundary the eager path did,
    /// not one deadline later.
    fn reassert_level_irqs(&mut self) {
        if self.cia_a.irq_pending() {
            self.chipset.raise_int(chipset::intbit::PORTS);
        }
        if self.cia_b.irq_pending() {
            self.chipset.raise_int(chipset::intbit::EXTER);
        }
        if let Some(card) = &self.graphics {
            if card.irq_pending() {
                self.chipset.raise_int(chipset::intbit::PORTS);
            }
        }
    }

    /// Test-only entry point to the pre-step-4 behaviour: apply
    /// `cpu_clocks` immediately, every call, never deferring through
    /// [`Self::pending_clocks`]. Exists so the differential test can
    /// drive one bus through this (the old eager path) and another
    /// through [`Self::tick`] (the new lazy path) with the same
    /// clock/access sequence and assert they never disagree -- both
    /// ultimately call the same [`Self::tick_exact`], so this is a
    /// thin wrapper, not a second implementation to keep in sync.
    #[cfg(test)]
    fn tick_eager(&mut self, cpu_clocks: u32) {
        self.tick_exact(cpu_clocks);
    }

    /// Report a mouse movement to the guest.
    pub fn mouse_delta(&mut self, dx: i8, dy: i8) {
        self.chipset.mouse_delta(dx, dy);
    }

    /// Report a mouse button change to the guest.
    ///
    /// The buttons are split across two chips on real hardware and so
    /// they are here: the left button is CIA-A PRA bit 6 (pin 6, FIR0),
    /// while right and middle are `POTGOR` bits. Routing both through
    /// one call keeps that split out of the host's input code.
    pub fn mouse_button(&mut self, button: chipset::MouseButton, pressed: bool) {
        if button == chipset::MouseButton::Left {
            // Active low: the pin is pulled to ground while pressed. This
            // is an input pin (`DDRA` bit 6 is 0), so it belongs in
            // `pra_input`, not the output latch `pra` -- see `Cia::read`.
            if pressed {
                self.cia_a.pra_input &= !CIA_A_PRA_FIR0;
            } else {
                self.cia_a.pra_input |= CIA_A_PRA_FIR0;
            }
        } else {
            self.chipset.mouse_button(button, pressed);
        }
    }

    /// The 68k interrupt level currently being requested, 0 for none.
    ///
    /// Deliberately does *not* flush: `chipset.intena`/`intreq` (what
    /// this reads) only ever change on their own -- without a register
    /// write -- at a deadline `next_event_clocks` already tracks (a
    /// raster line boundary raising VERTB, a CIA timer underflow raising
    /// PORTS/EXTER through `reassert_level_irqs`) or at a register write
    /// (`write_cia`/`write_custom_word`, both of which flush and
    /// re-assert themselves). [`Self::tick`] already flushes the instant
    /// accumulated clocks reach that deadline, so by the time this is
    /// called -- always right after `tick` in every caller
    /// (`machine-hosted`'s `run.rs`, both board `main.rs`) -- the state
    /// is already current. A flush here would cost nothing in
    /// correctness but everything in the point of step 4: this is
    /// called after *every* retired instruction, so flushing
    /// unconditionally here would flush on every instruction regardless
    /// of whether `tick` itself needed to, undoing the deferral.
    pub fn pending_irq_level(&self) -> u8 {
        self.chipset.pending_level()
    }

    /// Frame count since reset. Same non-flushing contract as
    /// [`Self::pending_irq_level`] and for the same reason: `tick`
    /// already flushes at the frame-wrapping deadline before this is
    /// read, and `machine-hosted`'s `run.rs` hook loop and both
    /// bare-metal boards' `main.rs` read this after every retired
    /// instruction, so a flush here would run on every instruction
    /// rather than only the ones that cross a line.
    pub fn frames(&self) -> u64 {
        self.chipset.frames
    }

    /// Backing storage and within-region offset for a *read* at
    /// `address`, if it falls inside a region the fast path can answer
    /// directly without walking the rest of the chain: fast RAM (via
    /// [`Self::fast_window`], bounded by the real buffer a board layer
    /// supplied -- which may be shorter than the declared AUTOCONFIG
    /// window, [`Self::with_fast_ram`]'s doc comment), chip RAM (except
    /// while the ROM overlay is redirecting reads there), or ROM when it
    /// exactly fills its window (an undersized ROM needs
    /// [`rom::read_mirrored`]'s wraparound, which this path does not
    /// implement, so it declines and lets the existing chain below
    /// handle it).
    ///
    /// Returns the *whole* backing slice and an offset into it,
    /// deliberately not pre-sliced to the access width: callers index
    /// with `get(off..off + N)`, so a span that runs past the end of the
    /// real array (fast RAM's window can be larger than its actual
    /// buffer) simply misses and falls through to the byte-granular
    /// chain unchanged, rather than this function having to reason about
    /// widths itself.
    fn fast_region(&self, address: u32) -> Option<(&[u8], usize)> {
        if (CHIP_RAM_BASE..CHIP_RAM_END).contains(&address)
            && !(self.overlay && address < OVERLAY_END)
        {
            return Some((&self.ram.chip_ram[..], (address - CHIP_RAM_BASE) as usize));
        }
        if let Some((base, len)) = self.ram.fast_window {
            if address >= base && address - base < len {
                let mem = self.ram.fast_ram.as_deref()?;
                return Some((mem, (address - base) as usize));
            }
        }
        if self.rom.len() == ROM_WINDOW_SIZE && (ROM_BASE..ROM_END).contains(&address) {
            return Some((self.rom, (address - ROM_BASE) as usize));
        }
        None
    }

    /// The mutable equivalent of [`Self::fast_region`], for *writes*:
    /// fast RAM (same shape) and chip RAM, with no overlay exclusion --
    /// a write under the overlay still lands in chip RAM today (the
    /// overlay only ever redirects reads, [`Self::write_byte`]'s own
    /// comment on why), so the fast path must not decline it either. ROM
    /// is never covered here; it has nothing to write to.
    fn fast_region_mut(&mut self, address: u32) -> Option<(&mut [u8], usize)> {
        if (CHIP_RAM_BASE..CHIP_RAM_END).contains(&address) {
            return Some((
                &mut self.ram.chip_ram[..],
                (address - CHIP_RAM_BASE) as usize,
            ));
        }
        if let Some((base, len)) = self.ram.fast_window {
            if address >= base && address - base < len {
                let mem = self.ram.fast_ram.as_deref_mut()?;
                return Some((mem, (address - base) as usize));
            }
        }
        None
    }

    /// Read one byte. Open-bus addresses return [`OPEN_BUS_BYTE`].
    pub fn read_byte(&mut self, address: u32) -> u8 {
        if let Some((region, off)) = self.fast_region(address) {
            if let Some(&byte) = region.get(off) {
                return byte;
            }
        }

        // Overlay first: while OVL is asserted the ROM answers for low
        // memory ahead of chip RAM.
        if self.overlay && address < OVERLAY_END {
            return rom::read_mirrored(self.rom, 0, address);
        }

        if (CHIP_RAM_BASE..CHIP_RAM_END).contains(&address) {
            self.ram.chip_ram[(address - CHIP_RAM_BASE) as usize]
        } else if AutoConfig::responds_to(address) {
            self.autoconfig.read(address)
        } else if (CIA_BASE..CIA_END).contains(&address) {
            self.read_cia(address)
        } else if (CUSTOM_BASE..CUSTOM_END).contains(&address) {
            // Custom registers are word-wide; a byte access returns the
            // corresponding half of the word.
            let word = self.read_custom_word(address & !1);
            if address & 1 == 0 {
                (word >> 8) as u8
            } else {
                word as u8
            }
        } else if (rom::EXT_ROM_BASE..rom::EXT_ROM_BASE + rom::EXT_ROM_WINDOW_SIZE as u32)
            .contains(&address)
            && !self.ext_rom.is_empty()
        {
            rom::read_mirrored(self.ext_rom, rom::EXT_ROM_BASE, address)
        } else if (ROM_BASE..ROM_END).contains(&address) && !self.rom.is_empty() {
            rom::read_mirrored(self.rom, ROM_BASE, address)
        } else if let Some(idx) = self.autoconfig.board_at(address) {
            // One scan of the chain for every device below, rather than
            // each of the seven arms this used to be (up to seven scans
            // per access, `docs/bus-fast-path-plan.md` §0) calling its
            // own `*_target` helper. `board_at` narrows to at most one
            // candidate index -- boards never overlap -- so which arm
            // matches it in is a lookup, not a race; only Graffity needs
            // more than a plain field comparison, since it can own more
            // than one chain index (`Self::graphics_boards`'s own doc
            // comment).
            //
            // `board_at` only ever returns an index it has actually
            // placed, so `placement` is always `Some` here; the `else`
            // still fails closed to open bus rather than assuming that
            // and indexing unchecked, per the hostile-input rule every
            // guest address is read under.
            let Some(base) = self.autoconfig.placement(idx).map(|p| p.base) else {
                return OPEN_BUS_BYTE;
            };
            let offset = address - base;
            if Some(idx) == self.mirage_board {
                match &mut self.mirage {
                    Some(m) => {
                        let value = m.read(offset);
                        // Mirror the write path's interrupt check on the
                        // read path too: this machine's now-retired Gayle IDE
                        // interface carried the hard-won reminder that a
                        // device whose interrupt can change state on a read
                        // (there, a per-sector refill; here, none currently
                        // does -- `mirage`'s module docs explain why the
                        // fetch moved to `tick()` instead) must not only ever
                        // check after a write, on pain of a silently missed
                        // interrupt.
                        if m.irq_pending() {
                            self.chipset.raise_int(chipset::intbit::PORTS);
                        }
                        value
                    }
                    None => OPEN_BUS_BYTE,
                }
            } else if Some(idx) == self.ram.fast_ram_board {
                match &self.ram.fast_ram {
                    Some(mem) => mem.get(offset as usize).copied().unwrap_or(OPEN_BUS_BYTE),
                    None => OPEN_BUS_BYTE,
                }
            } else if Some(idx) == self.hostblk_board {
                match &self.hostblk {
                    Some(h) => h.read(offset),
                    // `hostblk::Hostblk::read` never asserts an interrupt as
                    // a side effect of reading -- unlike MIRAGE, no
                    // register read here changes engine state (module docs:
                    // discovery registers are pure queries, and the
                    // completion queue only ever drains via
                    // `COMPLETION_ADVANCE`, a write) -- so there is no
                    // read-path interrupt check to mirror here. Still routed
                    // through the same `Option` shape as MIRAGE for
                    // consistency, not because this arm needs it.
                    None => OPEN_BUS_BYTE,
                }
            } else if Some(idx) == self.input_board {
                match &self.input {
                    Some(dev) => {
                        let value = dev.read(offset);
                        // No register read here currently mutates state
                        // (module docs: `EVENT_TYPE`/`EVENT_CODE`/etc. are
                        // pure head-of-queue queries, and the queue only
                        // ever drains via `EVENT_ADVANCE`, a write) -- but
                        // checked anyway, mirroring `hostblk`'s own read
                        // arm, which makes the identical choice for the
                        // identical reason: a device whose interrupt could
                        // ever change state on a read (the now-retired
                        // Gayle IDE interface's per-sector refill) that
                        // *doesn't* check here is how an interrupt goes
                        // silently missing and a multi-request pipeline
                        // stalls until an unrelated interrupt rescues it.
                        if dev.irq_pending() {
                            self.chipset.raise_int(chipset::intbit::PORTS);
                        }
                        value
                    }
                    None => OPEN_BUS_BYTE,
                }
            } else if Some(idx) == self.pktport_board {
                match &self.pktport {
                    // `pktport::Pktport::read` never asserts an interrupt as a
                    // side effect of reading -- every register it exposes is
                    // a pure query (`VERSION`/`CAPACITY`/`VOL_COUNT`, and
                    // `INT_STATUS` itself only changes via `tick` or a write),
                    // so there is no read-path interrupt check to mirror
                    // here, the same reasoning `hostblk`'s own read arm gives.
                    Some(dev) => dev.read(offset),
                    None => OPEN_BUS_BYTE,
                }
            } else if Some(idx) == self.rtg_board {
                match &self.rtg {
                    // No interrupt to check here -- `rtgboard`'s module docs,
                    // "no asynchronous boundary to defer across": mode
                    // programming is synchronous and this board never
                    // asserts INT2 at all.
                    Some(dev) => dev.read(offset),
                    None => OPEN_BUS_BYTE,
                }
            } else if Some(idx) == self.pcibridge_board {
                match &mut self.pcibridge {
                    // No interrupt check needed here even though this card
                    // can now raise INT2 (stage 2's INTx registers): every
                    // register this arm can read is a pure query --
                    // `INTX_STATUS` recomputes itself live rather than being
                    // mutated by a read, and nothing else in the register
                    // file changes `irq_pending()`'s answer on a read path --
                    // the same reasoning `hostblk`'s/`pktport`'s own read
                    // arms give for the identical choice.
                    Some(dev) => dev.read(offset),
                    None => OPEN_BUS_BYTE,
                }
            } else if let Some(board) = self
                .graphics_boards
                .iter()
                .position(|&chain_idx| chain_idx == Some(idx))
            {
                match &mut self.graphics {
                    Some(card) => card.read(board, offset),
                    None => OPEN_BUS_BYTE,
                }
            } else {
                OPEN_BUS_BYTE
            }
        } else {
            OPEN_BUS_BYTE
        }
    }

    /// Whether `address` falls inside `pcibridge`'s configured AUTOCONFIG
    /// window, and if so, the board-relative offset -- the input to
    /// [`pcibridge::PciBridge::read`]/[`pcibridge::PciBridge::write`], and
    /// what [`Self::pcibridge_aperture_sized`] narrows further into an
    /// aperture offset. `None` whenever no card is attached
    /// ([`Self::pcibridge_window`] is `None` until [`Self::with_pcibridge`]
    /// has run *and* AUTOCONFIG has placed the board) or the address
    /// belongs to some other board.
    ///
    /// Subtract-and-compare against the cached window rather than a
    /// `board_at` scan -- this is the one `*_target` helper called from
    /// outside `read_byte`/`write_byte`'s own single-scan device match
    /// ([`Self::pcibridge_aperture_sized`], reached from `read_word`/
    /// `read_long`/`write_word`/`write_long` before those fall back to
    /// byte decomposition), so it still needs to answer in isolation
    /// without a `board_at` result already in hand -- `docs/
    /// bus-fast-path-plan.md` §3.3, the single largest bus item in the
    /// full-device profile before this change.
    fn pcibridge_target(&self, address: u32) -> Option<u32> {
        let (base, len) = self.pcibridge_window?;
        if address < base || address - base >= len {
            return None;
        }
        Some(address - base)
    }

    /// The BAR-aperture-relative offset a naturally-aligned `width`
    /// access at `address` reaches, if `pcibridge` is attached and the
    /// access lies entirely within its aperture (`docs/
    /// pcibridge-protocol.md` §5's landed sized-aperture prerequisite --
    /// virtio's modern spec requires natural-width field access). `None`
    /// in every other case -- misaligned, inside the register file
    /// instead of the aperture, or no card attached -- and the caller
    /// falls through to the ordinary byte-decomposition path in every
    /// one of those, an intentional behavior-preserving fallback (module
    /// docs' "everything else... stays byte-granular"), not merely a
    /// convenience.
    fn pcibridge_aperture_sized(&self, address: u32, width: pci::AccessWidth) -> Option<u32> {
        if !address.is_multiple_of(width.bytes()) {
            return None;
        }
        let offset = self.pcibridge_target(address)?;
        (offset >= pcibridge::APERTURE_BASE_OFFSET)
            .then(|| offset - pcibridge::APERTURE_BASE_OFFSET)
    }

    /// Decode a CIA access. CIA-A occupies odd addresses, CIA-B even
    /// ones, and the register index comes from address bits 8-12.
    fn cia_select(address: u32) -> (bool, u8) {
        let is_cia_a = address & 1 != 0;
        let reg = ((address >> 8) & 0x0F) as u8;
        (is_cia_a, reg)
    }

    fn read_cia(&mut self, address: u32) -> u8 {
        // Every CIA register (timer, TOD, ICR) can change from ticking
        // alone -- catch up before reading, step 4's whole point.
        self.flush();
        let (is_cia_a, reg) = Self::cia_select(address);
        if is_cia_a {
            self.cia_a.read(reg)
        } else {
            self.cia_b.read(reg)
        }
    }

    fn write_cia(&mut self, address: u32, value: u8) {
        // See `read_cia`'s matching comment: flush before the write so
        // it applies to caught-up state, then recompute the deadline
        // after (not just when `flush` itself applied clocks -- this
        // write can retarget a running timer, or start/stop/reload one,
        // any of which moves `next_event_clocks` on its own).
        self.flush();
        let (is_cia_a, reg) = Self::cia_select(address);
        if is_cia_a {
            self.cia_a.write(reg, value);
            // PRA bit 0 drives the ROM overlay; re-read it after every
            // CIA-A write rather than special-casing the register.
            self.overlay = self.cia_a.ovl_asserted();
        } else {
            // Capture PRB's value before the write so the floppy model
            // can edge-detect SELECT/MOTOR/STEP transitions between old
            // and new -- see `FloppyDrive::on_prb_write`.
            let is_prb = reg & 0x0F == cia::reg::PRB;
            let prev_prb = self.cia_b.prb;
            self.cia_b.write(reg, value);
            if is_prb {
                self.floppy.on_prb_write(prev_prb, self.cia_b.prb);
                self.sync_floppy_status();
            }
        }
        self.recompute_next_event();
        // An ICR mask write can newly enable a source whose `icr_data`
        // was already latched (or unmasked and already latched from an
        // event `tick_exact` fired at some earlier flush) -- reassert
        // now rather than waiting for the next deadline, the same
        // reasoning `reassert_level_irqs`'s own doc comment gives.
        self.reassert_level_irqs();
    }

    /// Merge the floppy model's PRA bits (2-5) into CIA-A's input pin
    /// field, leaving the other bits (OVL/LED output latch, mouse fire
    /// buttons) untouched.
    fn sync_floppy_status(&mut self) {
        self.cia_a.pra_input =
            (self.cia_a.pra_input & !cia::FLOPPY_PRA_MASK) | self.floppy.pra_status_bits();
    }

    fn read_custom_word(&mut self, address: u32) -> u16 {
        // VPOSR/VHPOSR/INTENAR/INTREQR (among others) are exactly the
        // beam/interrupt state step 4 defers -- flush first, the same
        // reason `read_cia` does. Blitter registers don't depend on
        // ticking, but this is unconditional rather than narrowed to the
        // non-blitter arm below: a no-op `flush` (the common case) is
        // one field compare, cheaper than the branch to skip it.
        self.flush();
        let offset = (address - CUSTOM_BASE) as u16 & 0x1FE;
        if blitter::reg::is_blitter(offset) {
            return self.blitter.read(offset);
        }
        let value = self.chipset.read(offset);
        // BBUSY/BZERO live in the blitter but are read through DMACONR.
        if offset == chipset::reg::DMACONR {
            return value | self.blitter.dmaconr_bits();
        }
        value
    }

    fn write_custom_word(&mut self, address: u32, value: u16) {
        // See `read_custom_word`'s matching comment.
        self.flush();
        let offset = (address - CUSTOM_BASE) as u16 & 0x1FE;
        if blitter::reg::is_blitter(offset) {
            if self.blitter.write(offset, value) {
                self.run_blitter();
            }
            return;
        }
        self.chipset.write(offset, value);
        // No chipset register write can move `next_event_clocks`'
        // sources (vpos/hpos only change through `tick_exact`, never a
        // register write -- VPOSW/VHPOSW are latched nowhere in
        // `Chipset::write`), so unlike `write_cia` there is nothing to
        // recompute here. But an `INTREQ`/`INTENA` write can clear a bit
        // a still-live level source (a CIA, Graffity) drives, which the
        // eager path's per-call poll would re-raise on the very next
        // instruction -- see `reassert_level_irqs`'s doc comment.
        self.reassert_level_irqs();
    }

    /// Run an armed blit to completion and raise the blitter-finished
    /// interrupt. Synchronous by design — see [`blitter`]'s module docs.
    fn run_blitter(&mut self) {
        if self.blitter.execute(self.ram.chip_ram) {
            self.chipset.raise_int(chipset::intbit::BLIT);
        }
    }

    /// Read one big-endian 16-bit word.
    ///
    /// Custom-chip registers are word-wide devices, so a word access
    /// there is one register read rather than two byte reads — some
    /// registers would otherwise be sampled twice.
    pub fn read_word(&mut self, address: u32) -> u16 {
        if !(self.overlay && address < OVERLAY_END) && (CUSTOM_BASE..CUSTOM_END).contains(&address)
        {
            return self.read_custom_word(address);
        }
        // The fast path, before `pcibridge_aperture_sized` (moved after
        // it, `docs/bus-fast-path-plan.md` §3.3 -- that check used to run
        // first here and was, unconditionally, a full `board_at` scan on
        // every word access, the single largest bus item in the full
        // config's profile). `fast_region` only ever answers for fast
        // RAM/chip RAM/ROM, none of which can overlap `pcibridge`'s
        // aperture, so trying it first changes nothing about which path
        // a given address ultimately takes.
        if let Some((region, off)) = self.fast_region(address) {
            if let Some(bytes) = region.get(off..off + 2) {
                return u16::from_be_bytes([bytes[0], bytes[1]]);
            }
        }
        // `pcibridge`'s BAR aperture, before the byte-decomposition
        // fallback: a naturally-aligned word access becomes ONE width-2
        // backend access rather than two width-1s, the same reason the
        // custom-chip special case above exists -- virtio's modern spec
        // requires natural-width field access (`docs/
        // pcibridge-protocol.md` §5). Only the aperture range takes this
        // path; the register file stays byte-granular through the
        // ordinary fallback below, unchanged.
        if let Some(k) = self.pcibridge_aperture_sized(address, pci::AccessWidth::W16) {
            if let Some(dev) = &mut self.pcibridge {
                return dev.read_aperture_sized(k, pci::AccessWidth::W16) as u16;
            }
        }
        let hi = self.read_byte(address) as u16;
        let lo = self.read_byte(address.wrapping_add(1)) as u16;
        (hi << 8) | lo
    }

    /// Read one big-endian 32-bit longword, composed from four byte reads.
    pub fn read_long(&mut self, address: u32) -> u32 {
        // Fast path first -- see `read_word`'s matching comment on why
        // trying it ahead of `pcibridge_aperture_sized` is safe.
        if let Some((region, off)) = self.fast_region(address) {
            if let Some(bytes) = region.get(off..off + 4) {
                return u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            }
        }
        // Same sized-aperture path as `read_word`, one width-4 backend
        // access instead of four width-1s.
        if let Some(k) = self.pcibridge_aperture_sized(address, pci::AccessWidth::W32) {
            if let Some(dev) = &mut self.pcibridge {
                return dev.read_aperture_sized(k, pci::AccessWidth::W32);
            }
        }
        let hi = self.read_word(address) as u32;
        let lo = self.read_word(address.wrapping_add(2)) as u32;
        (hi << 16) | lo
    }

    /// Write one byte. Writes to chip RAM take effect; writes to ROM or
    /// open bus are silently discarded (real ROM cannot be written, and a
    /// real open-bus write simply has nothing latch it).
    pub fn write_byte(&mut self, address: u32, value: u8) {
        if let Some((region, off)) = self.fast_region_mut(address) {
            if let Some(slot) = region.get_mut(off) {
                *slot = value;
                return;
            }
        }

        // A write under the overlay still reaches chip RAM: the overlay
        // only redirects reads, since there is nothing behind ROM to
        // write to and the OS relies on being able to build its vector
        // table at $000000 before clearing OVL.
        if (CHIP_RAM_BASE..CHIP_RAM_END).contains(&address) {
            self.ram.chip_ram[(address - CHIP_RAM_BASE) as usize] = value;
        } else if AutoConfig::responds_to(address) {
            self.autoconfig.write(address, value);
            // The one AUTOCONFIG mutation site `docs/bus-fast-path-plan.md`
            // §3.1 doesn't cover via a builder: a base-address write here
            // is exactly what `AutoConfig::configure` (and `SHUTUP`'s
            // retirement) can change a placement from, so the cached
            // windows must be recomputed every time, not just when this
            // arm happens to be the one that actually placed something.
            self.refresh_windows();
        } else if (CIA_BASE..CIA_END).contains(&address) {
            self.write_cia(address, value);
        } else if (CUSTOM_BASE..CUSTOM_END).contains(&address) {
            // Byte writes to a word-wide register: merge into the
            // existing value rather than dropping the other half.
            let aligned = address & !1;
            let current = self.read_custom_word(aligned);
            let merged = if address & 1 == 0 {
                (current & 0x00FF) | ((value as u16) << 8)
            } else {
                (current & 0xFF00) | value as u16
            };
            self.write_custom_word(aligned, merged);
        } else if let Some(idx) = self.autoconfig.board_at(address) {
            // See `read_byte`'s matching arm for why this is one scan
            // and one match rather than the seven-helper ladder this
            // used to be.
            let Some(base) = self.autoconfig.placement(idx).map(|p| p.base) else {
                return;
            };
            let offset = address - base;
            if Some(idx) == self.mirage_board {
                if let Some(m) = &mut self.mirage {
                    m.write(offset, value);
                    if m.irq_pending() {
                        self.chipset.raise_int(chipset::intbit::PORTS);
                    }
                }
            } else if Some(idx) == self.ram.fast_ram_board {
                if let Some(mem) = &mut self.ram.fast_ram {
                    if let Some(slot) = mem.get_mut(offset as usize) {
                        *slot = value;
                    }
                }
            } else if Some(idx) == self.hostblk_board {
                if let Some(h) = &mut self.hostblk {
                    h.write(offset, value);
                    // A `DOORBELL` write can never itself raise INT2 --
                    // `hostblk`'s module docs' "Deferred completion" section
                    // is the whole point of this check being a no-op today.
                    // Kept for the same reason MIRAGE checks after
                    // every write rather than only where it currently
                    // matters: a future register (e.g. an immediate-reject
                    // path) raising synchronously must not require
                    // remembering to add this check back in.
                    if h.irq_pending() {
                        self.chipset.raise_int(chipset::intbit::PORTS);
                    }
                }
            } else if Some(idx) == self.input_board {
                if let Some(dev) = &mut self.input {
                    dev.write(offset, value);
                    // `EVENT_ADVANCE`/`INT_STATUS`/`INT_ENABLE` are exactly
                    // the registers that can change `irq_pending()`'s
                    // answer, so this check is load-bearing here (unlike
                    // `hostblk`'s doorbell arm above, kept only for future-
                    // proofing) -- see `input` module docs, "Interrupt
                    // model".
                    if dev.irq_pending() {
                        self.chipset.raise_int(chipset::intbit::PORTS);
                    }
                }
            } else if Some(idx) == self.pktport_board {
                if let Some(dev) = &mut self.pktport {
                    dev.write(offset, value);
                    // `DOORBELL`/`INT_STATUS`/`INT_ENABLE` are exactly the
                    // registers that can change `irq_pending()`'s answer
                    // (`DOORBELL` never synchronously today -- `pktport`'s
                    // module docs, "Deferred completion" -- but kept
                    // unconditional for the same future-proofing reason
                    // `hostblk`'s own doorbell arm gives).
                    if dev.irq_pending() {
                        self.chipset.raise_int(chipset::intbit::PORTS);
                    }
                }
            } else if Some(idx) == self.rtg_board {
                if let Some(dev) = &mut self.rtg {
                    dev.write(offset, value);
                    // No interrupt check here -- see `read_byte`'s matching
                    // arm above.
                }
            } else if Some(idx) == self.pcibridge_board {
                if let Some(dev) = &mut self.pcibridge {
                    dev.write(offset, value);
                    // `INTX_ENABLE`/`INTX_TEST` (and, indirectly, whatever a
                    // config-cycle write did to a device the backend routes
                    // `intx_levels()` through) are exactly the registers that
                    // can change `irq_pending()`'s answer, so this check is
                    // load-bearing here -- the `input` arm above is the
                    // model (`input` module docs, "Interrupt model").
                    if dev.irq_pending() {
                        self.chipset.raise_int(chipset::intbit::PORTS);
                    }
                }
            } else if let Some(board) = self
                .graphics_boards
                .iter()
                .position(|&chain_idx| chain_idx == Some(idx))
            {
                if let Some(card) = &mut self.graphics {
                    card.write(board, offset, value);
                    // Graffity re-acknowledges INT2 on its own schedule
                    // (module docs), so a register write here can leave
                    // it still asserting after the guest's own INTREQ
                    // ack -- see `reassert_level_irqs`'s doc comment for
                    // why this must reassert now rather than at the next
                    // deadline.
                    self.reassert_level_irqs();
                }
            }
        }
        // ROM and open-bus writes: discarded.
    }

    /// Write one big-endian 16-bit word.
    ///
    /// As with [`MachineBus::read_word`], custom-chip registers take a
    /// single word write — decomposing into bytes would apply the
    /// set/clear convention twice on registers like `INTENA`.
    pub fn write_word(&mut self, address: u32, value: u16) {
        if (CUSTOM_BASE..CUSTOM_END).contains(&address) {
            self.write_custom_word(address, value);
            return;
        }
        // Fast path first -- see `read_word`'s matching comment.
        if let Some((region, off)) = self.fast_region_mut(address) {
            if let Some(slot) = region.get_mut(off..off + 2) {
                slot.copy_from_slice(&value.to_be_bytes());
                return;
            }
        }
        // `pcibridge`'s BAR aperture, before the byte-decomposition
        // fallback -- see `read_word`'s matching arm for why.
        if let Some(k) = self.pcibridge_aperture_sized(address, pci::AccessWidth::W16) {
            if let Some(dev) = &mut self.pcibridge {
                dev.write_aperture_sized(k, pci::AccessWidth::W16, value as u32);
                // A sized aperture write cannot change INTx state today --
                // no device behind any BAR has real function logic yet
                // (`crate::pci::VirtioNetStub`'s own module docs), so
                // there is nothing this write could have done to
                // `intx_levels()`'s answer. Checked anyway, the same
                // future-proofing reason `hostblk`'s doorbell arm gives
                // for its own always-false-today check: a future device
                // whose BAR-mapped registers *do* affect its INTx line
                // must not require remembering to add this back in.
                if dev.irq_pending() {
                    self.chipset.raise_int(chipset::intbit::PORTS);
                }
                return;
            }
        }
        self.write_byte(address, (value >> 8) as u8);
        self.write_byte(address.wrapping_add(1), value as u8);
    }

    /// Write one big-endian 32-bit longword, decomposed into two word
    /// writes.
    pub fn write_long(&mut self, address: u32, value: u32) {
        // Fast path first -- see `read_word`'s matching comment.
        if let Some((region, off)) = self.fast_region_mut(address) {
            if let Some(slot) = region.get_mut(off..off + 4) {
                slot.copy_from_slice(&value.to_be_bytes());
                return;
            }
        }
        // Same sized-aperture path as `write_word`, one width-4 backend
        // access instead of decomposing further.
        if let Some(k) = self.pcibridge_aperture_sized(address, pci::AccessWidth::W32) {
            if let Some(dev) = &mut self.pcibridge {
                dev.write_aperture_sized(k, pci::AccessWidth::W32, value);
                // See `write_word`'s matching arm: future-proofing only,
                // nothing behind a BAR can change INTx state yet.
                if dev.irq_pending() {
                    self.chipset.raise_int(chipset::intbit::PORTS);
                }
                return;
            }
        }
        self.write_word(address, (value >> 16) as u16);
        self.write_word(address.wrapping_add(2), value as u16);
    }
}

/// Delegates straight to [`GuestRam`] -- `machine-hosted`'s
/// `screenshot.rs`/`introspect.rs`/`pktvol.rs` (and anything else that
/// just wants "is this a RAM address") keep calling this on
/// [`MachineBus`] unchanged; see [`GuestRam`]'s own doc comment for why
/// RAM moved to its own field.
impl<'a> GuestMemory for MachineBus<'a> {
    fn ram_slice(&self, addr: u32, len: u32) -> Option<&[u8]> {
        self.ram.ram_slice(addr, len)
    }

    fn ram_slice_mut(&mut self, addr: u32, len: u32) -> Option<&mut [u8]> {
        self.ram.ram_slice_mut(addr, len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_bus<'a>(chip_ram: &'a mut [u8; CHIP_RAM_SIZE], rom: &'a [u8]) -> MachineBus<'a> {
        MachineBus::new(chip_ram, rom)
    }

    /// CPU clocks in exactly one raster line -- the grain
    /// `MachineBus::tick` now gates MIRAGE/`hostblk`/`pktport`/
    /// `pcibridge`'s engines on (`beam.lines_started > 0`). Tests that
    /// used to tick by `1` to land a deferred-completion step now tick
    /// by this, since a sub-line tick no longer crosses a line boundary
    /// at all.
    const ONE_LINE_CLOCKS: u32 = chipset::PAL_COLOUR_CLOCKS_PER_LINE * CPU_CLOCKS_PER_COLOUR_CLOCK;

    // Chip RAM is 2 MB: too big for a default test-thread stack, so tests
    // heap-allocate it via `Box` rather than declaring it as a local
    // array. `machine-core` itself never does this (no `alloc`); it's a
    // hosted-test-only convenience.
    fn boxed_chip_ram() -> std::boxed::Box<[u8; CHIP_RAM_SIZE]> {
        // NB: `Box::new([0u8; CHIP_RAM_SIZE])` would build the 2 MB array
        // on the stack before moving it to the heap, which overflows a
        // default test-thread stack. Build a heap-allocated boxed slice
        // (via `vec!`) instead, then convert it to the fixed-size boxed
        // array `MachineBus::new` expects.
        let boxed_slice: std::boxed::Box<[u8]> = std::vec![0u8; CHIP_RAM_SIZE].into_boxed_slice();
        boxed_slice
            .try_into()
            .unwrap_or_else(|_| unreachable!("boxed_slice has exactly CHIP_RAM_SIZE elements"))
    }

    /// One giant `tick` spanning several frames (how the hosted runner
    /// advances a STOPped CPU, only in smaller slices) must give CIA-A's
    /// TOD one tick per frame *crossed*, not one per call: CIA-A TOD
    /// counts vertical blanks and is the OS wall clock, so dropping
    /// wraps runs every timer.device delay measured against it slow.
    #[test]
    fn cia_a_tod_ticks_once_per_frame_even_in_one_giant_tick() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut bus = new_bus(&mut ram, &rom);
        let frame_clocks = chipset::PAL_LINES_PER_FRAME
            * chipset::PAL_COLOUR_CLOCKS_PER_LINE
            * CPU_CLOCKS_PER_COLOUR_CLOCK;
        bus.tick(frame_clocks * 5 + 10);
        assert_eq!(bus.cia_a.tod, 5, "one TOD tick per wrapped frame");
    }

    /// Pin the clock-ratio relationship: the PAL E-clock is the colour
    /// clock divided by 5 (AHRM), so the two `CPU_CLOCKS_PER_*`
    /// constants must always satisfy `eclock = colour_clock * 5` -- one
    /// PAL frame is then exactly `312 * 227 / 5` E-clock ticks. A
    /// doubled E-clock divider once made every CIA timer (and so all of
    /// timer.device's idea of time) run at half speed.
    #[test]
    fn eclock_ratio_is_five_colour_clocks() {
        assert_eq!(CPU_CLOCKS_PER_ECLOCK, CPU_CLOCKS_PER_COLOUR_CLOCK * 5);
        let frame_clocks = chipset::PAL_LINES_PER_FRAME
            * chipset::PAL_COLOUR_CLOCKS_PER_LINE
            * CPU_CLOCKS_PER_COLOUR_CLOCK;
        assert_eq!(
            frame_clocks / CPU_CLOCKS_PER_ECLOCK,
            chipset::PAL_LINES_PER_FRAME * chipset::PAL_COLOUR_CLOCKS_PER_LINE / 5,
        );
    }

    #[test]
    fn open_bus_reads_are_all_ones() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut bus = new_bus(&mut ram, &rom);

        // A representative sample of proposal §6.1's open-bus ranges:
        // slow RAM/unused, RTC, Gary/Ramsey, and the Gayle ID register the
        // A1200 3.2 ROM specifically probes ($DE1000). §11.1 expected
        // that register to read as absent under the open-bus rule, with
        // a two-register stub as the documented fallback if it did not;
        // Gayle briefly grew into a full interface there and has since
        // been retired (`docs/device-ledger.md`), so $DE1000 is open bus
        // again, the same as every other address in this list.
        for &addr in &[
            0x00C0_0000u32,
            0x00D8_0000,
            0x00DE_0000,
            0x00DD_0000,
            0x00DE_1000,
        ] {
            assert_eq!(bus.read_byte(addr), 0xFF, "byte at {addr:#x}");
            assert_eq!(bus.read_word(addr), 0xFFFF, "word at {addr:#x}");
            assert_eq!(bus.read_long(addr), 0xFFFF_FFFF, "long at {addr:#x}");
        }
    }

    #[test]
    fn open_bus_writes_are_discarded() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut bus = new_bus(&mut ram, &rom);

        bus.write_long(0x00DD_0000, 0x1234_5678);
        assert_eq!(bus.read_long(0x00DD_0000), 0xFFFF_FFFF);
    }

    #[test]
    fn chip_ram_roundtrips() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut bus = new_bus(&mut ram, &rom);

        // Low memory is behind the ROM overlay out of reset, so drop OVL
        // before testing chip RAM there (a guest does the same thing
        // early in the strap).
        bus.write_byte(0x00BF_E001, 0x00);
        assert!(!bus.overlay());

        bus.write_byte(0x0000_0010, 0xAB);
        assert_eq!(bus.read_byte(0x0000_0010), 0xAB);

        bus.write_word(0x0010_0000, 0xBEEF);
        assert_eq!(bus.read_word(0x0010_0000), 0xBEEF);

        bus.write_long(CHIP_RAM_END - 4, 0xDEAD_BEEF);
        assert_eq!(bus.read_long(CHIP_RAM_END - 4), 0xDEAD_BEEF);
    }

    #[test]
    fn rom_reads_work_and_writes_are_discarded() {
        let mut ram = boxed_chip_ram();
        let mut rom = [0u8; ROM_WINDOW_SIZE];
        rom[0] = 0x11;
        rom[4] = 0x22;
        let mut bus = new_bus(&mut ram, &rom);

        assert_eq!(bus.read_byte(ROM_BASE), 0x11);
        assert_eq!(bus.read_byte(ROM_BASE + 4), 0x22);

        bus.write_byte(ROM_BASE, 0x99);
        assert_eq!(bus.read_byte(ROM_BASE), 0x11, "ROM write must be discarded");
    }

    #[test]
    fn undersized_rom_mirrors_across_the_window() {
        let mut ram = boxed_chip_ram();
        // A quarter-size ROM image should mirror four times across the
        // 512 KB window, the way a real address decoder ignores the
        // unused high address lines of an undersized ROM.
        let quarter = ROM_WINDOW_SIZE / 4;
        let mut small_rom = alloc_vec_zeroed(quarter);
        small_rom[0] = 0x42;
        let mut bus = new_bus(&mut ram, &small_rom);

        assert_eq!(bus.read_byte(ROM_BASE), 0x42);
        assert_eq!(bus.read_byte(ROM_BASE + quarter as u32), 0x42);
        assert_eq!(bus.read_byte(ROM_BASE + 2 * quarter as u32), 0x42);
        assert_eq!(bus.read_byte(ROM_BASE + 3 * quarter as u32), 0x42);
    }

    // Test-only helper: this crate has no `alloc` dependency, but the
    // hosted test binary links std, so a plain Vec is fine here.
    fn alloc_vec_zeroed(len: usize) -> std::vec::Vec<u8> {
        std::vec![0u8; len]
    }

    #[test]
    fn overlay_maps_rom_at_zero_until_ovl_is_cleared() {
        let mut ram = boxed_chip_ram();
        let mut rom = [0u8; ROM_WINDOW_SIZE];
        rom[0] = 0x11;
        rom[1] = 0x14;
        let mut bus = new_bus(&mut ram, &rom);

        // Out of reset the CPU's vector fetch from $0 must see ROM, not
        // chip RAM -- this is what lets an unmodified ROM boot.
        assert!(bus.overlay());
        assert_eq!(bus.read_word(0x0000_0000), 0x1114);

        // Writes still land in chip RAM underneath the overlay.
        bus.write_word(0x0000_0000, 0xDEAD);
        assert_eq!(
            bus.read_word(0x0000_0000),
            0x1114,
            "overlay still reads ROM"
        );

        // Clearing OVL via CIA-A PRA reveals the chip RAM underneath.
        bus.write_byte(0x00BF_E001, 0x00);
        assert!(!bus.overlay());
        assert_eq!(bus.read_word(0x0000_0000), 0xDEAD);
    }

    #[test]
    fn custom_register_word_access_is_a_single_register_write() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut bus = new_bus(&mut ram, &rom);

        // INTENA uses the set/clear convention; a word write must apply
        // it once, not once per byte half.
        bus.write_word(0x00DF_F09A, 0x8000 | (1 << chipset::intbit::VERTB));
        assert_eq!(
            bus.read_word(0x00DF_F01C),
            1 << chipset::intbit::VERTB,
            "INTENAR should read back the enabled source"
        );
    }

    #[test]
    fn cia_decode_splits_odd_and_even_addresses() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut bus = new_bus(&mut ram, &rom);

        // $BFE001 is CIA-A PRA, $BFD000 is CIA-B PRA.
        bus.write_byte(0x00BF_E001, 0x00);
        bus.write_byte(0x00BF_D000, 0x5A);
        assert_eq!(bus.cia_a.pra, 0x00);
        assert_eq!(bus.cia_b.pra, 0x5A);
    }

    #[test]
    fn vertb_fires_once_per_frame_and_requests_level_3() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut bus = new_bus(&mut ram, &rom);

        // Enable VERTB and the master enable.
        bus.write_word(
            0x00DF_F09A,
            0x8000 | (1 << chipset::intbit::INTEN) | (1 << chipset::intbit::VERTB),
        );
        assert_eq!(bus.pending_irq_level(), 0);

        // One full frame's worth of CPU clocks.
        let frame_clocks = chipset::PAL_LINES_PER_FRAME
            * chipset::PAL_COLOUR_CLOCKS_PER_LINE
            * CPU_CLOCKS_PER_COLOUR_CLOCK;
        bus.tick(frame_clocks);

        assert_eq!(bus.pending_irq_level(), 3, "VERTB is level 3");
    }

    // ---- Graffity routing --------------------------------------------

    /// Configure both of Graffity's boards through the AUTOCONFIG
    /// window, the way `expansion.library` actually would: VRAM
    /// (offered first) gets the low byte, the register window the next.
    fn configure_graffity(bus: &mut MachineBus, vram_base_byte: u8, regs_base_byte: u8) {
        bus.write_byte(
            autoconfig::AUTOCONFIG_BASE + autoconfig::ec::BASEADDRESS,
            vram_base_byte,
        );
        bus.write_byte(
            autoconfig::AUTOCONFIG_BASE + autoconfig::ec::BASEADDRESS,
            regs_base_byte,
        );
    }

    #[test]
    fn no_card_leaves_the_chain_empty_and_the_bus_unchanged() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut bus = new_bus(&mut ram, &rom);

        // Nothing was ever offered to the chain, so it must behave
        // exactly as it does today with no card in the machine at all.
        assert_eq!(bus.autoconfig.board_at(0x0020_0000), None);
        assert_eq!(
            bus.read_byte(autoconfig::AUTOCONFIG_BASE),
            OPEN_BUS_BYTE,
            "AUTOCONFIG window still reads open bus"
        );
        assert_eq!(bus.read_byte(0x0020_0000), OPEN_BUS_BYTE);
        bus.write_byte(0x0020_0000, 0xAB);
        assert_eq!(bus.read_byte(0x0020_0000), OPEN_BUS_BYTE, "still unwritten");
    }

    #[test]
    fn with_graphics_registers_both_boards_vram_first() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut vram = std::vec![0u8; 0x0020_0000]; // 2 MB
        let mut bus = new_bus(&mut ram, &rom).with_graphics(&mut vram);

        // The VRAM aperture (product 34) is offered to the chain before
        // the register window (product 33) -- the Picasso II shape.
        // er_Product is logical byte 1, so its high nybble sits at
        // physical offset 4 (`4*N` for N=1); every nybble but er_Type is
        // complemented (autoconfig module docs).
        assert_eq!(
            bus.read_byte(autoconfig::AUTOCONFIG_BASE + 4) >> 4,
            (!graffity::PRODUCT_VRAM) >> 4,
            "er_Product high nybble for the first board answering is VRAM's"
        );
    }

    #[test]
    fn configured_graffity_routes_vram_and_register_windows_to_the_chip() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut vram = std::vec![0u8; 0x0020_0000]; // 2 MB
        let mut bus = new_bus(&mut ram, &rom).with_graphics(&mut vram);

        // VRAM at $20xxxx, 2 MB long -- so it spans $200000-$3FFFFF.
        // Register window at $50xxxx, well clear of it and of chip RAM,
        // the AUTOCONFIG window, the CIAs and the custom chips.
        configure_graffity(&mut bus, 0x20, 0x50);
        assert_eq!(
            bus.autoconfig.placement(0).map(|p| p.base),
            Some(0x0020_0000)
        );
        assert_eq!(
            bus.autoconfig.placement(1).map(|p| p.base),
            Some(0x0050_0000)
        );

        // A byte in the VRAM window reaches VRAM.
        bus.write_byte(0x0020_1000, 0xCD);
        assert_eq!(bus.read_byte(0x0020_1000), 0xCD);

        // A register access in the register window reaches the chip:
        // CRTC index/data, the same protocol the chip model exercises
        // directly.
        bus.write_byte(0x0050_03D4, 0x0F); // CRTC_INDEX_COLOR
        bus.write_byte(0x0050_03D5, 0x77); // CRTC_DATA_COLOR
        assert_eq!(bus.read_byte(0x0050_03D5), 0x77);

        // An address between the two configured windows -- past VRAM's
        // 2 MB, short of the register window's base -- is still open
        // bus, unaffected by the card being present at all.
        assert_eq!(bus.read_byte(0x0045_0000), OPEN_BUS_BYTE);
    }

    /// End-to-end: an attached, unprogrammed Graffity card must never
    /// raise INT2 across a full frame boundary -- the gating this
    /// feature exists to guarantee, exercised through `tick` rather than
    /// the chip's own unit tests, so a wiring mistake in `tick` itself
    /// (calling the wrong method, or calling it unconditionally) would
    /// be caught here even if `cirrus.rs`'s own tests were fine.
    #[test]
    fn unprogrammed_graffity_never_raises_ports_across_a_frame() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut vram = std::vec![0u8; 0x0020_0000];
        let mut bus = new_bus(&mut ram, &rom).with_graphics(&mut vram);
        configure_graffity(&mut bus, 0x20, 0x50);

        let frame_clocks = chipset::PAL_LINES_PER_FRAME
            * chipset::PAL_COLOUR_CLOCKS_PER_LINE
            * CPU_CLOCKS_PER_COLOUR_CLOCK;
        bus.tick(frame_clocks + 10);

        let ports = 1u16 << chipset::intbit::PORTS;
        assert_eq!(
            bus.read_word(CUSTOM_BASE + chipset::reg::INTREQR as u32) & ports,
            0,
            "CR11 was never touched -- must stay disabled by its own reset value"
        );
    }

    /// End-to-end: once the guest arms CR11 (bit 5 clear, bit 4 set), a
    /// frame boundary crossed through `tick` raises the shared PORTS/INT2
    /// bit, acknowledging it (CRTC clear + `INTREQ`) drops it, and the
    /// next frame re-latches -- level-triggered, not a one-shot, the same
    /// shape every other device sharing this line uses.
    #[test]
    fn armed_graffity_raises_ports_once_per_frame_and_reacknowledges() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut vram = std::vec![0u8; 0x0020_0000];
        let mut bus = new_bus(&mut ram, &rom).with_graphics(&mut vram);
        configure_graffity(&mut bus, 0x20, 0x50);

        // Arm the interrupt: CR11 bit 5 (disable) clear, bit 4 (clear/ack)
        // set.
        bus.write_byte(0x0050_03D4, 0x11); // CRTC_INDEX_COLOR
        bus.write_byte(0x0050_03D5, 0x10); // CRTC_DATA_COLOR

        let frame_clocks = chipset::PAL_LINES_PER_FRAME
            * chipset::PAL_COLOUR_CLOCKS_PER_LINE
            * CPU_CLOCKS_PER_COLOUR_CLOCK;
        bus.tick(frame_clocks + 10);

        let ports = 1u16 << chipset::intbit::PORTS;
        assert_eq!(
            bus.read_word(CUSTOM_BASE + chipset::reg::INTREQR as u32) & ports,
            ports,
            "armed card raises PORTS on the frame boundary"
        );

        // Acknowledge: clear CR11 bit 4, then the chipset's own PORTS
        // latch, the same two-step shape every device sharing this line
        // uses.
        bus.write_byte(0x0050_03D4, 0x11);
        bus.write_byte(0x0050_03D5, 0x00);
        bus.write_word(CUSTOM_BASE + chipset::reg::INTREQ as u32, ports);
        assert_eq!(
            bus.read_word(CUSTOM_BASE + chipset::reg::INTREQR as u32) & ports,
            0,
            "acknowledged"
        );

        // Re-arm (bit 4 back to 1) and cross another frame boundary: the
        // card must re-latch rather than staying quiet forever having
        // been acknowledged once.
        bus.write_byte(0x0050_03D4, 0x11);
        bus.write_byte(0x0050_03D5, 0x10);
        bus.tick(frame_clocks + 10);
        assert_eq!(
            bus.read_word(CUSTOM_BASE + chipset::reg::INTREQR as u32) & ports,
            ports,
            "re-armed and re-latched on the next frame"
        );
    }

    #[test]
    fn with_graphics_zorro_iii_registers_a_single_board() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut vram = std::vec![0u8; 0x0020_0000]; // 2 MB
        let mut bus = new_bus(&mut ram, &rom).with_graphics_zorro_iii(&mut vram);

        // er_Product (logical byte 1) is Graffity's Zorro III identity,
        // and nothing configures a second board behind it.
        assert_eq!(
            bus.read_byte(autoconfig::AUTOCONFIG_BASE + 4) >> 4,
            (!graffity::PRODUCT_Z3) >> 4,
            "er_Product high nybble is the Zorro III window's"
        );
    }

    #[test]
    fn configured_zorro_iii_graffity_routes_its_three_sub_apertures() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut vram = std::vec![0u8; 0x0020_0000]; // 2 MB
        let mut bus = new_bus(&mut ram, &rom).with_graphics_zorro_iii(&mut vram);

        // A 16-bit write to EC_Z3_BASEADDRESS ($44), delivered as two
        // byte writes per `write_word`'s big-endian convention
        // (autoconfig module docs): hi=$40, lo=$00 -> base $40000000.
        bus.write_byte(
            autoconfig::AUTOCONFIG_BASE + autoconfig::ec::Z3_BASEADDRESS,
            0x40,
        );
        bus.write_byte(
            autoconfig::AUTOCONFIG_BASE + autoconfig::ec::Z3_BASEADDRESS + 1,
            0x00,
        );
        assert_eq!(
            bus.autoconfig.placement(0).map(|p| p.base),
            Some(0x4000_0000)
        );

        // VRAM sub-aperture at board offset $C00000.
        bus.write_byte(0x40C0_0004, 0xCD);
        assert_eq!(bus.read_byte(0x40C0_0004), 0xCD);

        // Register sub-aperture at board offset $800000: CRTC index/data,
        // same protocol as the Zorro II register window.
        bus.write_byte(0x4080_03D4, 0x0F); // CRTC_INDEX_COLOR
        bus.write_byte(0x4080_03D5, 0x77); // CRTC_DATA_COLOR
        assert_eq!(bus.read_byte(0x4080_03D5), 0x77);

        // The switch-strobe trap at board offset $400000 accepts writes
        // without disturbing anything else, and a gap between
        // sub-apertures is open bus.
        bus.write_byte(0x4040_0060, 0);
        assert_eq!(bus.read_byte(0x4000_1000), OPEN_BUS_BYTE);
    }

    // ---- MIRAGE wiring ---------------------------------------------------

    /// A tiny in-memory disk for exercising MIRAGE through the bus, kept
    /// local to this section so it's obvious at a glance which device a
    /// given test is wiring up.
    struct MirageDisk {
        sectors: std::vec::Vec<[u8; block::SECTOR_BYTES]>,
    }

    impl MirageDisk {
        fn new(count: usize) -> Self {
            Self {
                sectors: std::vec![[0u8; block::SECTOR_BYTES]; count],
            }
        }
    }

    impl block::BlockDevice for MirageDisk {
        fn sector_count(&self) -> u64 {
            self.sectors.len() as u64
        }
        fn read_sector(&mut self, lba: u64, buf: &mut [u8; block::SECTOR_BYTES]) -> bool {
            match self.sectors.get(lba as usize) {
                Some(s) => {
                    *buf = *s;
                    true
                }
                None => false,
            }
        }
        fn write_sector(&mut self, lba: u64, buf: &[u8; block::SECTOR_BYTES]) -> bool {
            match self.sectors.get_mut(lba as usize) {
                Some(s) => {
                    *s = *buf;
                    true
                }
                None => false,
            }
        }
    }

    #[test]
    fn no_mirage_leaves_the_chain_empty() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let bus = new_bus(&mut ram, &rom);

        // Brief item 8: a machine with no MIRAGE attached is completely
        // unaffected -- no chain entry, no routing branch.
        assert_eq!(bus.mirage_board, None);
        assert!(bus.mirage().is_none());
        assert_eq!(bus.autoconfig.board_at(0x0020_0000), None);
    }

    #[test]
    fn with_mirage_registers_one_zorro_ii_board() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut disk = MirageDisk::new(64);
        let mut bus = new_bus(&mut ram, &rom).with_mirage(0, &mut disk);

        assert!(bus.mirage().is_some());
        assert_eq!(bus.mirage_board, Some(0));
        // er_Manufacturer (logical bytes 4-5) reads back MIRAGE's
        // placeholder ID, complemented per the nybble protocol.
        assert_eq!(
            bus.autoconfig.read(autoconfig::AUTOCONFIG_BASE + 16) >> 4,
            (!(mirage::MANUFACTURER.to_be_bytes()[0])) >> 4
        );
    }

    /// Configure MIRAGE's single board at `base_byte << 16` -- the
    /// Zorro II sequence every board on this bus uses (autoconfig module
    /// docs).
    fn configure_mirage(bus: &mut MachineBus, base_byte: u8) {
        bus.write_byte(
            autoconfig::AUTOCONFIG_BASE + autoconfig::ec::BASEADDRESS,
            base_byte,
        );
    }

    #[test]
    fn configured_mirage_routes_its_register_window_and_ticks_a_transfer_end_to_end() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut disk = MirageDisk::new(64);
        let mut bus = new_bus(&mut ram, &rom).with_mirage(0, &mut disk);
        configure_mirage(&mut bus, 0x20);
        let base = 0x0020_0000u32;

        bus.write_byte(
            base + mirage::reg::INT_ENABLE + 3,
            mirage::int::XFER_COMPLETE,
        );
        bus.write_byte(base + mirage::reg::UNIT_SELECT + 3, 0);
        bus.write_long(base + mirage::reg::LBA, 5);
        bus.write_long(base + mirage::reg::COUNT, 1);
        bus.write_byte(base + mirage::reg::CMD_STATUS + 3, mirage::cmd::WRITE);

        // Not landed yet: the write path's tick check must not fire an
        // interrupt for a command that hasn't reached a tick boundary.
        assert_eq!(
            bus.read_byte(base + mirage::reg::CMD_STATUS + 3),
            mirage::status::BUSY
        );
        let ports = 1u16 << chipset::intbit::PORTS;
        assert_eq!(
            bus.read_word(CUSTOM_BASE + chipset::reg::INTREQR as u32) & ports,
            0
        );

        bus.tick(ONE_LINE_CLOCKS); // MachineBus::tick's MIRAGE arm lands the WritePending step
        assert_eq!(
            bus.read_byte(base + mirage::reg::CMD_STATUS + 3),
            mirage::status::DRQ
        );

        for i in 0u32..block::SECTOR_BYTES as u32 {
            bus.write_long(base + mirage::reg::DATA, (i << 24) | (i << 8));
        }
        assert_eq!(
            bus.read_byte(base + mirage::reg::CMD_STATUS + 3),
            mirage::status::BUSY,
            "full sector queued for commit"
        );

        bus.tick(ONE_LINE_CLOCKS); // commit lands here, on the read path's irq_pending check
        assert_eq!(bus.read_byte(base + mirage::reg::CMD_STATUS + 3), 0);
        assert_ne!(
            bus.read_word(CUSTOM_BASE + chipset::reg::INTREQR as u32) & ports,
            0,
            "the committed write raised PORTS via MachineBus::tick's MIRAGE arm"
        );

        // The bytes really landed in the backing store, independent of
        // the register path above.
        let mut check = [0u8; block::SECTOR_BYTES];
        disk.read_sector(5, &mut check);
        assert_eq!(check[0], 0);
        assert_eq!(check[4], 1);
    }

    /// The step-4.1 contract itself: MIRAGE's (and by the same gated
    /// block, `hostblk`/`pktport`/`pcibridge`'s -- `hostblk`'s is also
    /// pinned directly by `configured_hostblk_routes_its_window_and_
    /// completes_a_transfer_via_int2`) engine must not advance on a
    /// tick that stays within one raster line, and must advance exactly
    /// once a tick crosses a line boundary -- `MachineBus::tick` gates
    /// all four on `beam.lines_started > 0`, not on being called at all.
    #[test]
    fn mirage_engine_does_not_advance_within_a_line_but_does_once_a_line_is_crossed() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut disk = MirageDisk::new(64);
        let mut bus = new_bus(&mut ram, &rom).with_mirage(0, &mut disk);
        configure_mirage(&mut bus, 0x20);
        let base = 0x0020_0000u32;

        bus.write_byte(base + mirage::reg::UNIT_SELECT + 3, 0);
        bus.write_long(base + mirage::reg::LBA, 5);
        bus.write_long(base + mirage::reg::COUNT, 1);
        bus.write_byte(base + mirage::reg::CMD_STATUS + 3, mirage::cmd::WRITE);
        assert_eq!(
            bus.read_byte(base + mirage::reg::CMD_STATUS + 3),
            mirage::status::BUSY,
            "queued, not yet landed"
        );

        // Many ticks, but every one of them stays inside the same raster
        // line (`hpos` starts at 0, so anything under one line's worth of
        // colour clocks never crosses `PAL_COLOUR_CLOCKS_PER_LINE`):
        // `lines_started` is 0 every time, so MIRAGE's `tick()` must
        // never run, and `WritePending` must not have landed.
        for _ in 0..50 {
            bus.tick(ONE_LINE_CLOCKS / 64);
        }
        assert_eq!(
            bus.read_byte(base + mirage::reg::CMD_STATUS + 3),
            mirage::status::BUSY,
            "no line boundary crossed yet: the engine must not have advanced"
        );

        // One more tick that crosses the remaining distance to the next
        // line boundary: `lines_started` becomes 1, and MIRAGE's engine
        // must land its one step, exactly as it would have on the very
        // first call before this batching existed.
        bus.tick(ONE_LINE_CLOCKS);
        assert_eq!(
            bus.read_byte(base + mirage::reg::CMD_STATUS + 3),
            mirage::status::DRQ,
            "a crossed line boundary must advance the engine exactly one step"
        );
    }

    // ---- hostblk wiring: brief item 7 ("all three coexist", "unaffected") -

    #[test]
    fn no_hostblk_leaves_the_chain_empty_and_everything_else_unaffected() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let bus = new_bus(&mut ram, &rom);

        assert_eq!(bus.hostblk_board, None);
        assert!(bus.hostblk().is_none());
        assert_eq!(bus.autoconfig.board_at(0x4000_0000), None);
    }

    #[test]
    fn with_hostblk_registers_one_zorro_iii_board_alongside_mirage() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut mirage_disk = MirageDisk::new(8);
        let mut hostblk_disk = MirageDisk::new(64);
        let mut bus = new_bus(&mut ram, &rom)
            .with_mirage(0, &mut mirage_disk)
            .with_hostblk(0, &mut hostblk_disk, false);

        // Both storage devices coexist: MIRAGE at chain index 0, hostblk
        // at index 1.
        assert!(bus.mirage().is_some());
        assert!(bus.hostblk().is_some());
        assert_eq!(bus.mirage_board, Some(0));
        assert_eq!(bus.hostblk_board, Some(1));
        assert_eq!(
            bus.autoconfig.read(autoconfig::AUTOCONFIG_BASE + 4) >> 4,
            !hostblk::PRODUCT >> 4,
            "hostblk's own product number, not MIRAGE's, answers at chain index 1"
        );
    }

    /// Configure `hostblk`'s single Zorro III board at `base`, via the
    /// two-byte-write sequence to `EC_Z3_BASEADDRESS` (autoconfig module
    /// docs).
    fn configure_hostblk_z3(bus: &mut MachineBus, base: u32) {
        bus.write_byte(
            autoconfig::AUTOCONFIG_BASE + autoconfig::ec::Z3_BASEADDRESS,
            (base >> 24) as u8,
        );
        bus.write_byte(
            autoconfig::AUTOCONFIG_BASE + autoconfig::ec::Z3_BASEADDRESS + 1,
            (base >> 16) as u8,
        );
    }

    /// The point of routing `hostblk` through [`GuestMemory`] rather than
    /// a chip-RAM range: a buffer in fast RAM must work. `hostblk`'s own
    /// unit tests use a flat RAM based at 0, so they cannot show this --
    /// only the real memory map can, which is why this lives here.
    ///
    /// Fast RAM's base is whatever AUTOCONFIG assigned, read back rather
    /// than assumed (`device-ledger.md`, "The rule for addresses").
    #[test]
    fn hostblk_transfers_into_a_fast_ram_buffer_not_just_chip_ram() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut fast = std::vec![0u8; fastram::MIN_SIZE_BYTES as usize];
        let mut disk = MirageDisk::new(64);
        let mut bus = new_bus(&mut ram, &rom)
            .with_hostblk(0, &mut disk, false)
            .with_fast_ram(&mut fast);

        // AUTOCONFIG places one board at a time, so each gets its own
        // base write: hostblk registered first and is configured first,
        // fast RAM second. Where each landed is then read back rather
        // than assumed (`device-ledger.md`, "The rule for addresses") --
        // the test cares that fast RAM is reachable, not where it sits.
        let hb_base = 0x4000_0000u32;
        configure_hostblk_z3(&mut bus, hb_base);
        configure_hostblk_z3(&mut bus, 0x5000_0000);
        let fast_base = bus
            .autoconfig
            .placement(1)
            .map(|p| p.base)
            .expect("fast RAM configured");
        assert!(
            fast_base >= CHIP_RAM_SIZE as u32,
            "fast RAM must sit outside chip RAM for this test to mean anything"
        );

        // Seed a sector on the device, then read it into a buffer that
        // lives in fast RAM. Before this change the descriptor and the
        // buffer both had to be under CHIP_RAM_SIZE, so this failed with
        // BAD_ADDRESS.
        // Write a sector *out of* fast RAM, then read it back *into* a
        // different fast-RAM address, so both directions are proven
        // without needing access to the device behind the card.
        let pattern: std::vec::Vec<u8> =
            (0..block::SECTOR_BYTES).map(|i| (i as u8) ^ 0x5A).collect();
        let desc_addr = fast_base + 0x100;
        let src_addr = fast_base + 0x1000;
        let dst_addr = fast_base + 0x2000;
        for (i, &b) in pattern.iter().enumerate() {
            bus.write_byte(src_addr + i as u32, b);
        }

        let submit = |bus: &mut MachineBus, cmd: u8, buf: u32| {
            bus.write_byte(desc_addr, cmd);
            bus.write_byte(desc_addr + 1, 0);
            bus.write_long(desc_addr + 4, block::SECTOR_BYTES as u32);
            bus.write_long(desc_addr + 8, 0);
            bus.write_long(desc_addr + 12, 0);
            bus.write_long(desc_addr + 16, buf);
            bus.write_long(hb_base + hostblk::reg::DOORBELL, desc_addr);
            bus.tick(ONE_LINE_CLOCKS);
            let err = bus.read_byte(hb_base + hostblk::reg::COMPLETION_ERROR + 3);
            bus.write_byte(hb_base + hostblk::reg::COMPLETION_ADVANCE + 3, 0);
            err
        };

        assert_eq!(
            submit(&mut bus, hostblk::cmd::WRITE, src_addr),
            hostblk::err::OK,
            "a descriptor and source buffer in fast RAM must be reachable"
        );
        assert_eq!(
            submit(&mut bus, hostblk::cmd::READ, dst_addr),
            hostblk::err::OK,
            "a destination buffer in fast RAM must be reachable"
        );
        for (i, &b) in pattern.iter().enumerate() {
            assert_eq!(
                bus.read_byte(dst_addr + i as u32),
                b,
                "sector byte {i} round-tripped through fast RAM"
            );
        }
    }

    #[test]
    fn configured_hostblk_routes_its_window_and_completes_a_transfer_via_int2() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut disk = MirageDisk::new(64);
        let mut bus = new_bus(&mut ram, &rom).with_hostblk(0, &mut disk, false);
        let base = 0x4000_0000u32;
        configure_hostblk_z3(&mut bus, base);
        assert_eq!(bus.autoconfig.placement(0).map(|p| p.base), Some(base));

        bus.write_byte(base + hostblk::reg::INT_ENABLE + 3, 1);

        // Build a descriptor directly in guest chip RAM: WRITE, unit 0,
        // one sector, device offset 0, buffer at 0x2000.
        let desc_addr = 0x1000u32;
        let buf_addr = 0x2000u32;
        let pattern: std::vec::Vec<u8> = (0..block::SECTOR_BYTES as u32).map(|i| i as u8).collect();
        for (i, &b) in pattern.iter().enumerate() {
            bus.write_byte(buf_addr + i as u32, b);
        }
        bus.write_byte(desc_addr, hostblk::cmd::WRITE);
        bus.write_byte(desc_addr + 1, 0); // unit
        bus.write_long(desc_addr + 4, block::SECTOR_BYTES as u32); // length
        bus.write_long(desc_addr + 8, 0); // offset hi
        bus.write_long(desc_addr + 12, 0); // offset lo
        bus.write_long(desc_addr + 16, buf_addr); // buffer

        bus.write_long(base + hostblk::reg::DOORBELL, desc_addr);

        // Not landed yet: doorbell write alone must not complete anything.
        let ports = 1u16 << chipset::intbit::PORTS;
        assert_eq!(
            bus.read_long(base + hostblk::reg::COMPLETION_PTR),
            0,
            "nothing completed before the first tick"
        );
        assert_eq!(
            bus.read_word(CUSTOM_BASE + chipset::reg::INTREQR as u32) & ports,
            0
        );

        // Ticks that stay inside the current raster line must not run
        // the engine: `MachineBus::tick` gates it on a line boundary
        // being crossed (bus-fast-path-plan step 4.1), not on being
        // called at all.
        for _ in 0..50 {
            bus.tick(ONE_LINE_CLOCKS / 64);
        }
        assert_eq!(
            bus.read_long(base + hostblk::reg::COMPLETION_PTR),
            0,
            "no line boundary crossed yet: the engine must not have run"
        );

        bus.tick(ONE_LINE_CLOCKS); // MachineBus::tick's hostblk arm executes the request
        assert_eq!(
            bus.read_long(base + hostblk::reg::COMPLETION_PTR),
            desc_addr
        );
        assert_eq!(
            bus.read_byte(base + hostblk::reg::COMPLETION_ERROR + 3),
            hostblk::err::OK
        );
        assert_ne!(
            bus.read_word(CUSTOM_BASE + chipset::reg::INTREQR as u32) & ports,
            0,
            "the completed write raised PORTS via MachineBus::tick's hostblk arm"
        );

        // The bytes really landed in the backing store, independent of
        // the register path above.
        let mut check = [0u8; block::SECTOR_BYTES];
        disk.read_sector(0, &mut check);
        assert_eq!(&check[..], &pattern[..]);
    }

    // ---- fast RAM wiring ------------------------------------------------

    #[test]
    fn no_fast_ram_leaves_the_chain_empty_and_everything_else_unaffected() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let bus = new_bus(&mut ram, &rom);

        assert_eq!(bus.ram.fast_ram_board, None);
        assert_eq!(bus.autoconfig.board_at(0x4000_0000), None);
    }

    #[test]
    fn with_fast_ram_registers_a_zorro_iii_memlist_board() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut fast = std::vec![0u8; 0x0100_0000]; // 16 MB
        let mut bus = new_bus(&mut ram, &rom).with_fast_ram(&mut fast);

        assert_eq!(bus.ram.fast_ram_board, Some(0));
        assert_eq!(
            bus.autoconfig.read(autoconfig::AUTOCONFIG_BASE + 4) >> 4,
            !fastram::PRODUCT >> 4,
            "fast RAM's own product number answers at chain index 0"
        );
        // er_Type (byte 0, uncomplemented) must carry ERTF_MEMLIST -- the
        // entire point of this board (fastram module docs): without it
        // expansion.library would never add this RAM to the free pool.
        assert_eq!(
            bus.autoconfig.read(autoconfig::AUTOCONFIG_BASE) & (autoconfig::ERTF_MEMLIST << 4),
            autoconfig::ERTF_MEMLIST << 4
        );
    }

    #[test]
    fn configured_fast_ram_routes_reads_and_writes_and_clips_past_the_real_buffer() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut fast = std::vec![0u8; 0x0100_0000]; // 16 MB real backing store
        let mut bus = new_bus(&mut ram, &rom).with_fast_ram(&mut fast);

        let base = 0x4000_0000u32;
        configure_hostblk_z3(&mut bus, base); // same Z3 base-write sequence

        assert_eq!(bus.autoconfig.placement(0).map(|p| p.base), Some(base));

        bus.write_byte(base + 0x1234, 0xAB);
        assert_eq!(bus.read_byte(base + 0x1234), 0xAB);

        // Chip RAM stays untouched by a fast-RAM write at an unrelated
        // address -- the two regions are disjoint address spaces, not
        // aliases of one array.
        assert_eq!(bus.read_byte(0x0000_1234), OPEN_BUS_BYTE.wrapping_sub(0xFF));

        // Right at the end of the real 16 MB buffer: still routes.
        bus.write_byte(base + 0x00FF_FFFF, 0xCD);
        assert_eq!(bus.read_byte(base + 0x00FF_FFFF), 0xCD);
    }

    #[test]
    fn fast_ram_alongside_zorro_ii_graphics_leaves_graffitys_placement_untouched() {
        // Zorro II boards draw from the 24-bit Z2 pool; a Zorro III fast-
        // RAM board draws from the separate Z3 pool at $40000000 and up
        // (autoconfig module docs on Z2 vs Z3 base-write mechanics). They
        // are different chain *entries* answering at different physical
        // windows, so registering fast RAM first must not change which
        // address Graffity's own `EC_BASEADDRESS` byte write lands at --
        // only chain *order* (which board answers when) could do that,
        // and Zorro II configuration addresses Graffity directly via its
        // own byte write regardless of how many Zorro III boards precede
        // it in the chain.
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut fast = std::vec![0u8; 0x0100_0000];
        let mut vram = std::vec![0u8; 0x0020_0000];
        let mut bus = new_bus(&mut ram, &rom)
            .with_fast_ram(&mut fast)
            .with_graphics(&mut vram);

        // Fast RAM is chain index 0 (registered first), Graffity's VRAM
        // and register boards are indices 1 and 2.
        assert_eq!(bus.ram.fast_ram_board, Some(0));

        // Configure fast RAM's Z3 window, then Graffity's two Z2 boards,
        // exactly as `expansion.library` would walk the chain in order.
        configure_hostblk_z3(&mut bus, 0x4000_0000);
        configure_graffity(&mut bus, 0x20, 0x50);

        assert_eq!(
            bus.autoconfig.placement(1).map(|p| p.base),
            Some(0x0020_0000),
            "Graffity VRAM lands exactly where the Z2 byte write says, \
             independent of the Z3 board ahead of it in the chain"
        );
        assert_eq!(
            bus.autoconfig.placement(2).map(|p| p.base),
            Some(0x0050_0000)
        );
    }

    // ---- GuestMemory: the seam `hostblk` needs (brief item 4) -----------

    #[test]
    fn guest_memory_resolves_chip_ram_and_rejects_out_of_range() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut bus = new_bus(&mut ram, &rom);

        GuestMemory::ram_slice_mut(&mut bus, 0x1000, 4)
            .unwrap()
            .copy_from_slice(&[1, 2, 3, 4]);
        assert_eq!(
            GuestMemory::ram_slice(&bus, 0x1000, 4),
            Some(&[1, 2, 3, 4][..])
        );

        // Straddling the end of chip RAM: rejected, not silently clipped.
        assert_eq!(
            GuestMemory::ram_slice(&bus, CHIP_RAM_END - 2, 4),
            None,
            "must not return a short slice or wrap into fast RAM/open bus"
        );
        // A length whose end overflows u32 must not panic.
        assert_eq!(GuestMemory::ram_slice(&bus, u32::MAX - 1, 8), None);
    }

    #[test]
    fn guest_memory_resolves_fast_ram_at_its_autoconfig_placed_base() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut fast = std::vec![0u8; 0x0100_0000]; // 16 MB
        let mut bus = new_bus(&mut ram, &rom).with_fast_ram(&mut fast);

        // Unconfigured yet: no address should resolve, including the
        // address it will eventually land at -- there is no address to
        // hardcode here, which is the entire point.
        assert_eq!(GuestMemory::ram_slice(&bus, 0x4000_0000, 4), None);

        let base = 0x4000_0000u32;
        configure_hostblk_z3(&mut bus, base); // same Z3 write sequence

        GuestMemory::ram_slice_mut(&mut bus, base + 0x2000, 4)
            .unwrap()
            .copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(
            GuestMemory::ram_slice(&bus, base + 0x2000, 4),
            Some(&[0xDE, 0xAD, 0xBE, 0xEF][..])
        );

        // Past the real 16 MB backing store but still inside the
        // declared AUTOCONFIG window: rejected, not a panic.
        assert_eq!(
            GuestMemory::ram_slice(&bus, base + 0x0100_0000 - 2, 4),
            None
        );
        // A span crossing from fast RAM into whatever (if anything)
        // follows it must not be silently accepted as a short read.
        assert_eq!(GuestMemory::ram_slice(&bus, base - 2, 4), None);
    }

    // ---- input card wiring: brief item 4 ("a machine with no input card
    // attached must be completely unaffected") ---------------------------

    #[test]
    fn no_input_card_leaves_the_chain_empty_and_everything_else_unaffected() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let bus = new_bus(&mut ram, &rom);

        assert_eq!(bus.input_board, None);
        assert!(bus.input().is_none());
        assert_eq!(bus.autoconfig.board_at(0x4000_0000), None);
    }

    #[test]
    fn with_input_registers_one_zorro_iii_board_alongside_hostblk() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut disk = MirageDisk::new(64);
        let mut bus = new_bus(&mut ram, &rom)
            .with_hostblk(0, &mut disk, false)
            .with_input();

        assert!(bus.hostblk().is_some());
        assert!(bus.input().is_some());
        assert_eq!(bus.hostblk_board, Some(0));
        assert_eq!(bus.input_board, Some(1));
        assert_eq!(
            bus.autoconfig.read(autoconfig::AUTOCONFIG_BASE + 4) >> 4,
            !input::PRODUCT >> 4,
            "input's own product number, not hostblk's, answers at chain index 1"
        );
    }

    /// Configure a Zorro III board at `base` at whichever chain index is
    /// currently answering -- same two-byte `EC_Z3_BASEADDRESS` sequence
    /// `configure_hostblk_z3` uses, generalised to a name that doesn't
    /// imply it's hostblk-specific.
    fn configure_zorro_iii(bus: &mut MachineBus, base: u32) {
        configure_hostblk_z3(bus, base);
    }

    #[test]
    fn configured_input_card_routes_its_window_and_reports_capacity_and_version() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut bus = new_bus(&mut ram, &rom).with_input();
        let base = 0x4000_0000u32;
        configure_zorro_iii(&mut bus, base);
        assert_eq!(bus.autoconfig.placement(0).map(|p| p.base), Some(base));

        let read_u32 = |bus: &mut MachineBus, off: u32| {
            u32::from_be_bytes([
                bus.read_byte(base + off),
                bus.read_byte(base + off + 1),
                bus.read_byte(base + off + 2),
                bus.read_byte(base + off + 3),
            ])
        };
        assert_eq!(
            read_u32(&mut bus, input::reg::CAPACITY),
            input::QUEUE_CAPACITY as u32
        );
        assert_eq!(
            read_u32(&mut bus, input::reg::VERSION),
            input::PROTOCOL_VERSION
        );
    }

    /// INT2 must be checked on both the write path (pushing a host event
    /// or acking) and the read path (a driver that reads a register right
    /// after a host event landed, with no intervening write of its own),
    /// the same lesson `hostblk`'s and MIRAGE's own wiring tests encode --
    /// this module's docs cite the retired Gayle IDE interface as where
    /// missing the read-path case once turned a file read into a DOS
    /// "object not found".
    #[test]
    fn configured_input_card_raises_int2_seen_on_the_read_path_and_clears_via_ack() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut bus = new_bus(&mut ram, &rom).with_input();
        let base = 0x4000_0000u32;
        configure_zorro_iii(&mut bus, base);

        bus.write_byte(base + input::reg::INT_ENABLE + 3, 1);
        let ports = 1u16 << chipset::intbit::PORTS;
        assert_eq!(
            bus.read_word(CUSTOM_BASE + chipset::reg::INTREQR as u32) & ports,
            0
        );

        // The host pushes an event directly (no bus access at all) --
        // exactly the shape `--input-script` uses.
        assert!(bus.input_mut().unwrap().push_key(0x01, true, 0));

        // Nothing has written to the card's registers since the push, so
        // only a *read* can observe the interrupt becoming pending. If
        // the read arm didn't check `irq_pending()`, INTREQR would still
        // report nothing here.
        let _ = bus.read_byte(base + input::reg::EVENT_TYPE + 3);
        assert_ne!(
            bus.read_word(CUSTOM_BASE + chipset::reg::INTREQR as u32) & ports,
            0,
            "INT2 must be visible via the read path"
        );

        // Draining and acking (both writes) must clear it.
        bus.write_byte(base + input::reg::EVENT_ADVANCE + 3, 0);
        bus.write_byte(base + input::reg::INT_STATUS + 3, 1);
        assert_eq!(
            bus.read_byte(base + input::reg::EVENT_TYPE + 3),
            input::ev::NONE
        );
    }

    // ---- rtgboard wiring: "a machine without the board is unaffected" ----

    #[test]
    fn no_rtgboard_leaves_the_chain_empty_and_everything_else_unaffected() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let bus = new_bus(&mut ram, &rom);

        assert_eq!(bus.rtg_board, None);
        assert!(bus.rtgboard().is_none());
        assert_eq!(bus.autoconfig.board_at(0x4000_0000), None);
    }

    #[test]
    fn with_rtgboard_registers_one_zorro_iii_board_alongside_input() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut vram = std::vec![0u8; 64 * 1024];
        let modes = [rtgboard::ModeDescriptor {
            width: 640,
            height: 480,
            format: rtgboard::format::RGBX_8888,
        }];
        let mut bus = new_bus(&mut ram, &rom)
            .with_input()
            .with_rtgboard(&mut vram, &modes);

        assert!(bus.input().is_some());
        assert!(bus.rtgboard().is_some());
        assert_eq!(bus.input_board, Some(0));
        assert_eq!(bus.rtg_board, Some(1));
        assert_eq!(
            bus.autoconfig.read(autoconfig::AUTOCONFIG_BASE + 4) >> 4,
            !input::PRODUCT >> 4,
            "input's own product number, not rtgboard's, answers at chain index 0"
        );
    }

    /// End-to-end through the real bus (not `rtgboard`'s own flat-RAM
    /// unit tests): a mode committed through the configured AUTOCONFIG
    /// window is visible via the `CUR_*` registers, and a pixel pattern
    /// written into the VRAM aperture round-trips -- proving the register
    /// file and the VRAM aperture share one address window without
    /// aliasing each other once real AUTOCONFIG placement is involved.
    #[test]
    fn configured_rtgboard_programs_a_mode_and_round_trips_vram_through_the_real_bus() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut vram = std::vec![0u8; 640 * 480 * 4 + 4096];
        let modes = [rtgboard::ModeDescriptor {
            width: 640,
            height: 480,
            format: rtgboard::format::RGBX_8888,
        }];
        let mut bus = new_bus(&mut ram, &rom).with_rtgboard(&mut vram, &modes);
        let base = 0x4000_0000u32;
        configure_zorro_iii(&mut bus, base);

        let write_u32 = |bus: &mut MachineBus, addr: u32, value: u32| {
            bus.write_byte(addr, (value >> 24) as u8);
            bus.write_byte(addr + 1, (value >> 16) as u8);
            bus.write_byte(addr + 2, (value >> 8) as u8);
            bus.write_byte(addr + 3, value as u8);
        };
        let read_u32 = |bus: &mut MachineBus, addr: u32| -> u32 {
            u32::from_be_bytes([
                bus.read_byte(addr),
                bus.read_byte(addr + 1),
                bus.read_byte(addr + 2),
                bus.read_byte(addr + 3),
            ])
        };

        write_u32(&mut bus, base + rtgboard::reg::SET_WIDTH, 640);
        write_u32(&mut bus, base + rtgboard::reg::SET_HEIGHT, 480);
        bus.write_byte(
            base + rtgboard::reg::SET_FORMAT + 3,
            rtgboard::format::RGBX_8888,
        );
        write_u32(&mut bus, base + rtgboard::reg::SET_STRIDE, 640 * 4);
        write_u32(&mut bus, base + rtgboard::reg::SET_FB_OFFSET, 0);
        bus.write_byte(base + rtgboard::reg::COMMIT + 3, 0);

        assert_eq!(
            bus.read_byte(base + rtgboard::reg::STATUS + 3),
            rtgboard::status::APPLIED
        );
        assert_eq!(read_u32(&mut bus, base + rtgboard::reg::CUR_WIDTH), 640);
        assert_eq!(read_u32(&mut bus, base + rtgboard::reg::CUR_HEIGHT), 480);

        // A pixel pattern written through the CPU-visible VRAM aperture
        // lands in the board's own VRAM -- the "no accelerator, the
        // guest CPU writes pixels directly" story ADR 0002 verified.
        let vram_addr = base + rtgboard::VRAM_BASE;
        bus.write_byte(vram_addr, 0x11);
        bus.write_byte(vram_addr + 1, 0x22);
        bus.write_byte(vram_addr + 2, 0x33);
        assert_eq!(bus.read_byte(vram_addr), 0x11);
        assert_eq!(bus.read_byte(vram_addr + 1), 0x22);
        assert_eq!(bus.read_byte(vram_addr + 2), 0x33);
        assert_eq!(
            bus.rtgboard().unwrap().vram()[0..3],
            [0x11, 0x22, 0x33],
            "the bus's VRAM aperture and the board's own borrowed slice must be the same bytes"
        );
    }

    // ---- pcibridge wiring: ADR 0005 stage 1's guest-visible half -----------

    /// Configure a Zorro II board at `base_byte << 16` -- the single-byte
    /// `EC_BASEADDRESS` sequence, the shape `with_mirage`'s own tests use.
    fn configure_zorro_ii(bus: &mut MachineBus, base_byte: u8) {
        bus.write_byte(
            autoconfig::AUTOCONFIG_BASE + autoconfig::ec::BASEADDRESS,
            base_byte,
        );
    }

    /// A [`pktport::PacketBackend`] stub for the coexistence test below --
    /// this test only cares that `pktport`'s board lands at a stable
    /// address, never that a real request completes, so a fixed no-op
    /// answer is enough (the same minimal-fixture posture `MirageDisk`
    /// takes for `BlockDevice` above).
    struct StubPacketBackend;

    impl pktport::PacketBackend for StubPacketBackend {
        fn execute(
            &mut self,
            _action: u32,
            _args: [u32; 7],
            _mem: &mut dyn GuestMemory,
        ) -> (u32, u32) {
            (0, 0)
        }
    }

    fn pcibridge_read_u32(bus: &mut MachineBus, addr: u32) -> u32 {
        u32::from_be_bytes([
            bus.read_byte(addr),
            bus.read_byte(addr + 1),
            bus.read_byte(addr + 2),
            bus.read_byte(addr + 3),
        ])
    }

    fn pcibridge_write_u32(bus: &mut MachineBus, addr: u32, value: u32) {
        let b = value.to_be_bytes();
        bus.write_byte(addr, b[0]);
        bus.write_byte(addr + 1, b[1]);
        bus.write_byte(addr + 2, b[2]);
        bus.write_byte(addr + 3, b[3]);
    }

    /// Pack a config address the way a driver would -- see
    /// `pcibridge`'s own unit tests for the identical helper against the
    /// card directly, rather than through the real bus.
    fn pcibridge_pack_cfg_addr(bus_no: u8, device: u8, function: u8, offset: u16) -> u32 {
        (bus_no as u32) << 20 | (device as u32) << 15 | (function as u32) << 12 | offset as u32
    }

    fn pcibridge_stage_and_op(bus: &mut MachineBus, base: u32, addr: u32, width: u8, op: u8) {
        pcibridge_write_u32(bus, base + pcibridge::reg::CFG_ADDR, addr);
        bus.write_byte(base + pcibridge::reg::CFG_WIDTH + 3, width);
        bus.write_byte(base + pcibridge::reg::CFG_OP + 3, op);
    }

    /// End-to-end through the real bus (not `pcibridge`'s own flat unit
    /// tests): a machine with a real [`pci::VirtualPciBus`] behind
    /// `pcibridge` scans bus 0 devices 0..8 exactly the way `pci.library`
    /// enumeration will -- only the two populated slots answer with their
    /// real identity, everything else all-ones -- and then sizes a real
    /// BAR through the card's own registers.
    #[test]
    fn pci_enumeration_walks_through_the_real_bus() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut hostbridge_dev = pci::HostBridge::new();
        let mut net_backend = pci::NullNetBackend;
        let mut net_dev = pci::VirtioNetStub::new(&mut net_backend);
        let mut slots = [
            pci::VirtualSlot {
                bdf: pci::Bdf {
                    bus: 0,
                    device: 0,
                    function: 0,
                },
                device: &mut hostbridge_dev,
            },
            pci::VirtualSlot {
                bdf: pci::Bdf {
                    bus: 0,
                    device: 1,
                    function: 0,
                },
                device: &mut net_dev,
            },
        ];
        let mut vpci = pci::VirtualPciBus::new(&mut slots);
        let mut bus = new_bus(&mut ram, &rom).with_pcibridge(&mut vpci);

        let base = 0x4000_0000u32;
        configure_zorro_iii(&mut bus, base);
        assert_eq!(bus.pcibridge_board_base(), Some(base));

        for device in 0u8..8 {
            pcibridge_stage_and_op(
                &mut bus,
                base,
                pcibridge_pack_cfg_addr(0, device, 0, 0),
                2,
                0,
            );
            assert_eq!(
                bus.read_byte(base + pcibridge::reg::CFG_STATUS + 3),
                pcibridge::status::COMPLETED,
                "device {device} scan must always complete"
            );
            bus.write_byte(base + pcibridge::reg::CFG_STATUS + 3, 0xFF);
            let vendor = pcibridge_read_u32(&mut bus, base + pcibridge::reg::CFG_DATA);
            match device {
                0 => assert_eq!(vendor, pci::HostBridge::VENDOR_ID as u32, "device 0"),
                1 => assert_eq!(vendor, pci::VirtioNetStub::VENDOR_ID as u32, "device 1"),
                _ => assert_eq!(vendor, 0xFFFF, "device {device} must be absent"),
            }
        }

        // Size BAR0 of 00:01.0 through the real bus.
        pcibridge_write_u32(&mut bus, base + pcibridge::reg::CFG_DATA, 0xFFFF_FFFF);
        pcibridge_stage_and_op(&mut bus, base, pcibridge_pack_cfg_addr(0, 1, 0, 0x10), 4, 1);
        bus.write_byte(base + pcibridge::reg::CFG_STATUS + 3, 0xFF);

        pcibridge_stage_and_op(&mut bus, base, pcibridge_pack_cfg_addr(0, 1, 0, 0x10), 4, 0);
        assert_eq!(
            pcibridge_read_u32(&mut bus, base + pcibridge::reg::CFG_DATA),
            0xFFFF_C000
        );
    }

    /// Attach `pcibridge` after the whole existing native chain and prove
    /// nothing that was already there moves: every pre-existing board's
    /// base is identical with and without `pcibridge`, and `pcibridge`
    /// itself lands at its own, distinct address.
    #[test]
    fn pcibridge_coexists_with_the_full_native_chain_without_moving_anyone() {
        // ---- machine A: the native chain alone -----------------------------
        let bases_without_pcibridge = {
            let mut ram = boxed_chip_ram();
            let rom = [0u8; ROM_WINDOW_SIZE];
            let mut disk = MirageDisk::new(64);
            let mut vram = std::vec![0u8; 64 * 1024];
            let modes = [rtgboard::ModeDescriptor {
                width: 640,
                height: 480,
                format: rtgboard::format::RGBX_8888,
            }];
            let mut fast = std::vec![0u8; fastram::MIN_SIZE_BYTES as usize];
            let mut packet_backend = StubPacketBackend;
            let mut bus = new_bus(&mut ram, &rom)
                .with_hostblk(0, &mut disk, false)
                .with_input()
                .with_rtgboard(&mut vram, &modes)
                .with_fast_ram(&mut fast)
                .with_pktport(&mut packet_backend);

            configure_hostblk_z3(&mut bus, 0x4000_0000);
            configure_zorro_iii(&mut bus, 0x5000_0000);
            configure_zorro_iii(&mut bus, 0x6000_0000);
            configure_zorro_iii(&mut bus, 0x7000_0000);
            configure_zorro_ii(&mut bus, 0x20);

            (
                bus.hostblk_board_base(),
                bus.input_board_base(),
                bus.rtgboard_base(),
                bus.autoconfig
                    .placement(bus.ram.fast_ram_board.expect("fast RAM registered"))
                    .map(|p| p.base),
                bus.pktport_board_base(),
            )
        };

        // ---- machine B: the same chain, plus pcibridge last ---------------
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut disk = MirageDisk::new(64);
        let mut vram = std::vec![0u8; 64 * 1024];
        let modes = [rtgboard::ModeDescriptor {
            width: 640,
            height: 480,
            format: rtgboard::format::RGBX_8888,
        }];
        let mut fast = std::vec![0u8; fastram::MIN_SIZE_BYTES as usize];
        let mut packet_backend = StubPacketBackend;
        let mut hostbridge_dev = pci::HostBridge::new();
        let mut slots = [pci::VirtualSlot {
            bdf: pci::Bdf {
                bus: 0,
                device: 0,
                function: 0,
            },
            device: &mut hostbridge_dev,
        }];
        let mut vpci = pci::VirtualPciBus::new(&mut slots);
        let mut bus = new_bus(&mut ram, &rom)
            .with_hostblk(0, &mut disk, false)
            .with_input()
            .with_rtgboard(&mut vram, &modes)
            .with_fast_ram(&mut fast)
            .with_pktport(&mut packet_backend)
            .with_pcibridge(&mut vpci);

        configure_hostblk_z3(&mut bus, 0x4000_0000);
        configure_zorro_iii(&mut bus, 0x5000_0000);
        configure_zorro_iii(&mut bus, 0x6000_0000);
        configure_zorro_iii(&mut bus, 0x7000_0000);
        configure_zorro_ii(&mut bus, 0x20);
        configure_zorro_iii(&mut bus, 0x9000_0000);

        assert_eq!(bus.hostblk_board_base(), bases_without_pcibridge.0);
        assert_eq!(bus.input_board_base(), bases_without_pcibridge.1);
        assert_eq!(bus.rtgboard_base(), bases_without_pcibridge.2);
        assert_eq!(
            bus.autoconfig
                .placement(bus.ram.fast_ram_board.expect("fast RAM registered"))
                .map(|p| p.base),
            bases_without_pcibridge.3
        );
        assert_eq!(bus.pktport_board_base(), bases_without_pcibridge.4);

        let pcibridge_base = bus
            .pcibridge_board_base()
            .expect("pcibridge configured last");
        assert_eq!(pcibridge_base, 0x9000_0000);
        for other in [
            bases_without_pcibridge.0,
            bases_without_pcibridge.1,
            bases_without_pcibridge.2,
            bases_without_pcibridge.3,
            bases_without_pcibridge.4,
        ] {
            assert_ne!(
                Some(pcibridge_base),
                other,
                "pcibridge must land at its own, distinct address"
            );
        }

        // pcibridge answers while hostblk's own registers still answer at
        // its (unmoved) base.
        assert_eq!(
            pcibridge_read_u32(&mut bus, pcibridge_base + pcibridge::reg::VERSION),
            pcibridge::PROTOCOL_VERSION
        );
        let hostblk_base = bases_without_pcibridge.0.unwrap();
        assert_eq!(
            pcibridge_read_u32(&mut bus, hostblk_base + hostblk::reg::VERSION),
            hostblk::PROTOCOL_VERSION
        );
    }

    /// `INTX_TEST` asserted and deasserted through the guest-visible
    /// register file, end to end on the real bus: `INTX_STATUS` reflects
    /// it, `pending_irq_level` shows `PORTS`' level (`2`, `chipset.rs`)
    /// once `INTX_ENABLE` unmasks the line, and deasserting clears it
    /// with no acknowledgement write required at all -- the level-
    /// triggered contract module docs describe, proven the same way
    /// `configured_input_card_raises_int2_seen_on_the_read_path_and_clears_via_ack`
    /// proves `input`'s.
    #[test]
    fn pcibridge_intx_test_is_observable_through_the_register_file_and_raises_int2() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut hostbridge_dev = pci::HostBridge::new();
        let mut slots = [pci::VirtualSlot {
            bdf: pci::Bdf {
                bus: 0,
                device: 0,
                function: 0,
            },
            device: &mut hostbridge_dev,
        }];
        let mut vpci = pci::VirtualPciBus::new(&mut slots);
        let mut bus = new_bus(&mut ram, &rom).with_pcibridge(&mut vpci);
        let base = 0x4000_0000u32;
        configure_zorro_iii(&mut bus, base);

        // Master-enable INTEN and unmask PORTS in INTENA, same shape
        // `vertb_fires_once_per_frame_and_requests_level_3` uses.
        bus.write_word(
            CUSTOM_BASE + chipset::reg::INTENA as u32,
            0x8000 | (1 << chipset::intbit::INTEN) | (1 << chipset::intbit::PORTS),
        );
        assert_eq!(bus.pending_irq_level(), 0);

        // Assert INTX_TEST bit 0 (INTA): visible in INTX_STATUS
        // immediately, but INTX_ENABLE still masks it from INT2.
        pcibridge_write_u32(&mut bus, base + pcibridge::reg::INTX_TEST, 0x1);
        assert_eq!(
            pcibridge_read_u32(&mut bus, base + pcibridge::reg::INTX_STATUS),
            0x1
        );
        assert_eq!(
            bus.pending_irq_level(),
            0,
            "asserted but not enabled must not reach INT2"
        );

        // Unmask it: PORTS (level 2) is now pending, observed the
        // instant the enabling write lands (the write-path check).
        pcibridge_write_u32(&mut bus, base + pcibridge::reg::INTX_ENABLE, 0x1);
        assert_eq!(bus.pending_irq_level(), 2, "PORTS is level 2");

        // Deassert INTX_TEST: `pcibridge`'s own `INTX_STATUS` clears with
        // no write-1-to-clear at all, level-triggered as module docs
        // describe. The chipset's shared `INTREQ` latch is a separate
        // matter, though -- exactly like every other card on this bus,
        // it stays set until the driver acknowledges it (real Amiga
        // hardware: `INTREQ` is a level *latch*, not a live mirror of
        // every source), so `pending_irq_level` only drops once that
        // acknowledgement lands, the same two-step shape `armed_graffity_
        // raises_ports_once_per_frame_and_reacknowledges` proves for
        // Graffity's own PORTS source.
        pcibridge_write_u32(&mut bus, base + pcibridge::reg::INTX_TEST, 0x0);
        assert_eq!(
            pcibridge_read_u32(&mut bus, base + pcibridge::reg::INTX_STATUS),
            0,
            "the card's own status clears immediately, with no ack needed"
        );
        let ports = 1u16 << chipset::intbit::PORTS;
        bus.write_word(CUSTOM_BASE + chipset::reg::INTREQ as u32, ports);
        assert_eq!(
            bus.pending_irq_level(),
            0,
            "acknowledging INTREQ clears it once the source is already quiet"
        );
    }

    /// A [`pci::NetBackend`] for the end-to-end test below: records every
    /// transmitted frame, never has one to inject (this test only drives
    /// tx). Distinct from `pci.rs`'s own `LoopbackNetBackend` -- that one
    /// is private to `pci`'s test module.
    struct StubNetBackend {
        transmitted: std::vec::Vec<std::vec::Vec<u8>>,
    }

    impl pci::NetBackend for StubNetBackend {
        fn transmit(&mut self, frame: &[u8]) {
            self.transmitted.push(frame.to_vec());
        }

        fn poll_receive(&mut self, _buf: &mut [u8]) -> Option<usize> {
            None
        }
    }

    /// Poke one byte of `pcibridge`'s BAR aperture, address-invariant
    /// (module docs' §3/§5): aperture byte `k` is BAR0 byte `k`, so this
    /// is exactly [`pci::VirtioNetStub::bar_write`]`(0, k, W8, byte)` one
    /// level up.
    fn vnet_bar_write_byte(bus: &mut MachineBus, pcibridge_base: u32, bar_off: u32, byte: u8) {
        bus.write_byte(
            pcibridge_base + pcibridge::APERTURE_BASE_OFFSET + bar_off,
            byte,
        );
    }

    fn vnet_bar_write_bytes(bus: &mut MachineBus, pcibridge_base: u32, bar_off: u32, bytes: &[u8]) {
        for (i, b) in bytes.iter().enumerate() {
            vnet_bar_write_byte(bus, pcibridge_base, bar_off + i as u32, *b);
        }
    }

    fn vnet_bar_read_byte(bus: &mut MachineBus, pcibridge_base: u32, bar_off: u32) -> u8 {
        bus.read_byte(pcibridge_base + pcibridge::APERTURE_BASE_OFFSET + bar_off)
    }

    /// This increment's whole point: `docs/pci-library.md` §6's "one
    /// unexercised link" -- stage 2 wired `INTx`-to-`INT2` end to end but
    /// had no device with real function logic to prove it with anything
    /// but the `INTX_TEST` diagnostic. Here, a real virtio-net used-buffer
    /// completion (a genuine tx frame, walked through real guest RAM) is
    /// what raises the line: driven entirely through the register file
    /// and BAR aperture, exactly as a guest driver would, with
    /// `bus.tick()` standing in for the driver's own doorbell-to-
    /// completion latency.
    #[test]
    fn virtio_net_used_buffer_is_visible_end_to_end_through_pcibridges_intx_status() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut backend = StubNetBackend {
            transmitted: std::vec::Vec::new(),
        };
        let pcibridge_base;
        {
            let mut hostbridge_dev = pci::HostBridge::new();
            let mut net_dev = pci::VirtioNetStub::new(&mut backend);
            let mut slots = [
                pci::VirtualSlot {
                    bdf: pci::Bdf {
                        bus: 0,
                        device: 0,
                        function: 0,
                    },
                    device: &mut hostbridge_dev,
                },
                pci::VirtualSlot {
                    bdf: pci::Bdf {
                        bus: 0,
                        device: 1,
                        function: 0,
                    },
                    device: &mut net_dev,
                },
            ];
            let mut vpci = pci::VirtualPciBus::new(&mut slots);
            let mut bus = new_bus(&mut ram, &rom).with_pcibridge(&mut vpci);
            pcibridge_base = 0x4000_0000u32;
            configure_zorro_iii(&mut bus, pcibridge_base);

            // Master-enable INTEN and unmask PORTS, same as the INTX_TEST
            // test above.
            bus.write_word(
                CUSTOM_BASE + chipset::reg::INTENA as u32,
                0x8000 | (1 << chipset::intbit::INTEN) | (1 << chipset::intbit::PORTS),
            );

            // Assign 00:01.0's BAR0 and enable memory-space decode.
            let bar_base = 0x2000_0000u32;
            pcibridge_write_u32(
                &mut bus,
                pcibridge_base + pcibridge::reg::CFG_DATA,
                bar_base,
            );
            pcibridge_stage_and_op(
                &mut bus,
                pcibridge_base,
                pcibridge_pack_cfg_addr(0, 1, 0, 0x10),
                4,
                1,
            );
            pcibridge_write_u32(&mut bus, pcibridge_base + pcibridge::reg::CFG_DATA, 0x02);
            pcibridge_stage_and_op(
                &mut bus,
                pcibridge_base,
                pcibridge_pack_cfg_addr(0, 1, 0, 0x04),
                1,
                1,
            );
            pcibridge_write_u32(
                &mut bus,
                pcibridge_base + pcibridge::reg::APERTURE_BASE,
                bar_base,
            );

            // Feature negotiation through the aperture, byte at a time.
            vnet_bar_write_bytes(&mut bus, pcibridge_base, 0x00, &0u32.to_le_bytes()); // device_feature_select = 0
            let low = {
                let mut v = [0u8; 4];
                for (i, b) in v.iter_mut().enumerate() {
                    *b = vnet_bar_read_byte(&mut bus, pcibridge_base, 0x04 + i as u32);
                }
                u32::from_le_bytes(v)
            };
            vnet_bar_write_bytes(&mut bus, pcibridge_base, 0x08, &0u32.to_le_bytes()); // driver_feature_select = 0
            vnet_bar_write_bytes(&mut bus, pcibridge_base, 0x0C, &low.to_le_bytes());
            vnet_bar_write_bytes(&mut bus, pcibridge_base, 0x00, &1u32.to_le_bytes()); // device_feature_select = 1
            let high = {
                let mut v = [0u8; 4];
                for (i, b) in v.iter_mut().enumerate() {
                    *b = vnet_bar_read_byte(&mut bus, pcibridge_base, 0x04 + i as u32);
                }
                u32::from_le_bytes(v)
            };
            vnet_bar_write_bytes(&mut bus, pcibridge_base, 0x08, &1u32.to_le_bytes()); // driver_feature_select = 1
            vnet_bar_write_bytes(&mut bus, pcibridge_base, 0x0C, &high.to_le_bytes());

            vnet_bar_write_byte(&mut bus, pcibridge_base, 0x14, 1); // ACKNOWLEDGE
            vnet_bar_write_byte(&mut bus, pcibridge_base, 0x14, 1 | 2); // + DRIVER
            vnet_bar_write_byte(&mut bus, pcibridge_base, 0x14, 1 | 2 | 8); // + FEATURES_OK
            let status = vnet_bar_read_byte(&mut bus, pcibridge_base, 0x14);
            assert_eq!(
                status & 8,
                8,
                "FEATURES_OK must stick (VERSION_1 was acked)"
            );
            vnet_bar_write_byte(&mut bus, pcibridge_base, 0x14, status | 4); // + DRIVER_OK
            assert_eq!(
                vnet_bar_read_byte(&mut bus, pcibridge_base, 0x14),
                1 | 2 | 8 | 4,
                "negotiation must reach DRIVER_OK"
            );

            // Select and configure the tx queue (index 1).
            vnet_bar_write_bytes(&mut bus, pcibridge_base, 0x16, &1u16.to_le_bytes()); // queue_select
            vnet_bar_write_bytes(&mut bus, pcibridge_base, 0x18, &8u16.to_le_bytes()); // queue_size
            let desc_addr = 0x1000u32;
            let avail_addr = 0x2000u32;
            let used_addr = 0x3000u32;
            let buf_addr = 0x4000u32;
            vnet_bar_write_bytes(
                &mut bus,
                pcibridge_base,
                0x20,
                &(desc_addr as u64).to_le_bytes(),
            );
            vnet_bar_write_bytes(
                &mut bus,
                pcibridge_base,
                0x28,
                &(avail_addr as u64).to_le_bytes(),
            );
            vnet_bar_write_bytes(
                &mut bus,
                pcibridge_base,
                0x30,
                &(used_addr as u64).to_le_bytes(),
            );
            vnet_bar_write_bytes(&mut bus, pcibridge_base, 0x1C, &1u16.to_le_bytes()); // queue_enable

            // Lay out one tx descriptor: a 12-byte virtio-net header
            // (all zero) followed by a 4-byte "frame".
            let payload = [0xDEu8, 0xAD, 0xBE, 0xEF];
            let mut frame = std::vec![0u8; 12 + payload.len()];
            frame[12..].copy_from_slice(&payload);
            for (i, b) in frame.iter().enumerate() {
                bus.write_byte(buf_addr + i as u32, *b);
            }
            // virtq_desc: addr:u64, len:u32, flags:u16(=0), next:u16(=0).
            for (i, b) in (buf_addr as u64).to_le_bytes().iter().enumerate() {
                bus.write_byte(desc_addr + i as u32, *b);
            }
            for (i, b) in (frame.len() as u32).to_le_bytes().iter().enumerate() {
                bus.write_byte(desc_addr + 8 + i as u32, *b);
            }
            bus.write_byte(desc_addr + 12, 0);
            bus.write_byte(desc_addr + 13, 0);
            bus.write_byte(desc_addr + 14, 0);
            bus.write_byte(desc_addr + 15, 0);
            // avail ring: flags(=0), idx=1, ring[0]=0.
            bus.write_byte(avail_addr, 0);
            bus.write_byte(avail_addr + 1, 0);
            bus.write_byte(avail_addr + 2, 1); // idx low byte
            bus.write_byte(avail_addr + 3, 0);
            bus.write_byte(avail_addr + 4, 0); // ring[0] = 0
            bus.write_byte(avail_addr + 5, 0);

            assert_eq!(
                bus.pending_irq_level(),
                0,
                "nothing pending before any traffic"
            );

            // Drive the engine: MachineBus::tick's pcibridge dance walks
            // the tx ring, hands the frame to the backend, and publishes
            // a used-buffer completion -- see `MachineBus::tick`'s own
            // doc comment on this. A full line's worth of clocks, since
            // the engine now only runs once a line boundary is crossed.
            bus.tick(ONE_LINE_CLOCKS);

            assert_eq!(
                pcibridge_read_u32(&mut bus, pcibridge_base + pcibridge::reg::INTX_STATUS) & 1,
                1,
                "INTA must be visible in INTX_STATUS after a used buffer is added"
            );
            pcibridge_write_u32(&mut bus, pcibridge_base + pcibridge::reg::INTX_ENABLE, 1);
            assert_eq!(
                bus.pending_irq_level(),
                2,
                "PORTS (level 2) must be pending once INTA is unmasked"
            );

            // The ISR read (through the register file's real BAR
            // aperture, not a host-side shortcut) clears both the byte
            // and the line -- but the chipset's own INTREQ latch, like
            // every other card here, stays set until acknowledged.
            let isr = vnet_bar_read_byte(&mut bus, pcibridge_base, 0x2000);
            assert_eq!(isr & 1, 1);
            assert_eq!(
                pcibridge_read_u32(&mut bus, pcibridge_base + pcibridge::reg::INTX_STATUS) & 1,
                0,
                "INTX_STATUS drops the instant the device's own ISR is read"
            );
            let ports = 1u16 << chipset::intbit::PORTS;
            bus.write_word(CUSTOM_BASE + chipset::reg::INTREQ as u32, ports);
            assert_eq!(bus.pending_irq_level(), 0);
        }

        assert_eq!(backend.transmitted.len(), 1);
        assert_eq!(backend.transmitted[0], [0xDEu8, 0xAD, 0xBE, 0xEF]);
    }

    /// A machine that never calls `with_pcibridge` is completely
    /// unaffected -- no chain entry, no routing branch, and its
    /// would-be register addresses stay open bus.
    #[test]
    fn no_pcibridge_leaves_the_chain_empty_and_everything_else_unaffected() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut bus = new_bus(&mut ram, &rom);

        assert_eq!(bus.pcibridge_board, None);
        assert!(bus.pcibridge().is_none());
        assert_eq!(bus.autoconfig.board_at(0x4000_0000), None);

        let base = 0x4000_0000u32;
        assert_eq!(bus.read_byte(base + pcibridge::reg::VERSION), OPEN_BUS_BYTE);
        assert_eq!(
            bus.read_byte(base + pcibridge::reg::CFG_ADDR),
            OPEN_BUS_BYTE
        );
        bus.write_byte(base + pcibridge::reg::CFG_OP + 3, 0);
        assert_eq!(
            bus.read_byte(base + pcibridge::reg::CFG_STATUS),
            OPEN_BUS_BYTE
        );
    }

    // ---- step 3 (`docs/bus-fast-path-plan.md`): the fast path ------------

    /// A word straddling the end of chip RAM (`fast_region`'s slice ends
    /// exactly at `CHIP_RAM_END`, so `get(off..off + 2)` misses and the
    /// access must fall through to the byte-composed path unchanged) must
    /// read identically to composing it from two `read_byte` calls, and
    /// the byte one past chip RAM's end must be open bus -- nothing is
    /// mapped there by default.
    #[test]
    fn word_straddling_chip_rams_end_matches_the_byte_composed_result() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut bus = new_bus(&mut ram, &rom);

        bus.write_byte(CHIP_RAM_END - 1, 0xAB);
        assert_eq!(
            bus.read_byte(CHIP_RAM_END),
            OPEN_BUS_BYTE,
            "one past chip RAM's end, nothing attached there"
        );
        let expected =
            ((bus.read_byte(CHIP_RAM_END - 1) as u16) << 8) | (bus.read_byte(CHIP_RAM_END) as u16);
        assert_eq!(bus.read_word(CHIP_RAM_END - 1), expected);
        assert_eq!(bus.read_word(CHIP_RAM_END - 1), 0xABFF);
    }

    /// The same straddle shape at the end of fast RAM's *real* backing
    /// buffer, which -- unlike chip RAM's fixed array -- is routinely
    /// shorter than the 16 MB window AUTOCONFIG declares for it
    /// (`with_fast_ram`'s doc comment). A long write/read spanning that
    /// boundary must fall through to the byte path exactly like chip
    /// RAM's case above, discarding/open-bussing the bytes past the real
    /// buffer rather than panicking or silently wrapping.
    #[test]
    fn long_straddling_fast_rams_real_buffer_end_matches_the_byte_composed_result() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        // A real buffer far shorter than the 16 MB window every fast-RAM
        // board declares (`fastram::MIN_SIZE_BYTES`) -- the gap between
        // the two is the whole point of this test.
        let mut fast = std::vec![0u8; 8];
        let mut bus = new_bus(&mut ram, &rom).with_fast_ram(&mut fast);
        let base = 0x4000_0000u32;
        configure_hostblk_z3(&mut bus, base);

        bus.write_long(base + 6, 0xAABB_CCDD);
        assert_eq!(
            bus.read_byte(base + 8),
            OPEN_BUS_BYTE,
            "past the real 8-byte buffer, still inside the declared window"
        );
        let expected = ((bus.read_byte(base + 6) as u32) << 24)
            | ((bus.read_byte(base + 7) as u32) << 16)
            | ((bus.read_byte(base + 8) as u32) << 8)
            | (bus.read_byte(base + 9) as u32);
        assert_eq!(bus.read_long(base + 6), expected);
    }

    /// AUTOCONFIG placing fast RAM is the only thing that ever populates
    /// [`MachineBus::fast_window`] (`Self::refresh_windows`'s doc
    /// comment) -- there is no address to hardcode here, which is
    /// `device-ledger.md`'s "rule for addresses" applied to this cache
    /// specifically. Before that write lands, the fast path must not
    /// answer for the address fast RAM will eventually occupy, even
    /// though nothing has changed about the underlying buffer.
    #[test]
    fn placement_populates_fast_window_and_gates_the_fast_path() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut fast = std::vec![0xAAu8; fastram::MIN_SIZE_BYTES as usize];
        let mut bus = new_bus(&mut ram, &rom).with_fast_ram(&mut fast);

        assert_eq!(bus.ram.fast_window, None, "not yet placed");
        let base = 0x4000_0000u32;
        assert_eq!(
            bus.fast_region(base),
            None,
            "fast RAM's eventual base must not be hardcoded into the fast path early"
        );
        assert_eq!(bus.read_byte(base), OPEN_BUS_BYTE);

        configure_hostblk_z3(&mut bus, base);

        assert_eq!(bus.ram.fast_window, Some((base, fastram::MIN_SIZE_BYTES)));
        assert!(
            bus.fast_region(base).is_some(),
            "the fast path now answers for the placed window"
        );
        assert_eq!(bus.read_byte(base), 0xAA);
    }

    /// [`MachineBus::fast_region`] only ever answers for ROM when it
    /// exactly fills its 512 KB window -- an undersized image still needs
    /// [`rom::read_mirrored`]'s wraparound, which the fast path does not
    /// implement, so it must decline and let the existing chain mirror it
    /// (exactly as [`tests::undersized_rom_mirrors_across_the_window`]
    /// already proves end to end; this pins the fast path's own refusal
    /// specifically, so a future change can't silently start answering
    /// wrong for this case instead of falling through).
    #[test]
    fn undersized_rom_is_declined_by_the_fast_path_and_still_mirrors() {
        let mut ram = boxed_chip_ram();
        let quarter = ROM_WINDOW_SIZE / 4;
        let mut small_rom = alloc_vec_zeroed(quarter);
        small_rom[0] = 0x42;
        let bus = new_bus(&mut ram, &small_rom);

        assert_eq!(bus.fast_region(ROM_BASE), None);
        assert_eq!(bus.fast_region(ROM_BASE + quarter as u32), None);
        assert_eq!(bus.fast_region(ROM_END - 1), None);
    }

    /// A word/long-composed-from-bytes reference, deliberately built on
    /// [`MachineBus::read_byte`] rather than reproducing the pre-fast-path
    /// byte chain from scratch. `read_byte` itself now has a fast path
    /// too (`docs/bus-fast-path-plan.md` §3.2's own note on this), so this
    /// is not an independent oracle the way the straddle tests above are
    /// -- it is a fixed reference the width-native paths must agree with,
    /// and the 445 pre-existing unit tests (unchanged by this whole
    /// change, `cargo test -p machine-core`) are what establishes that
    /// `read_byte` itself still answers exactly as the original chain
    /// did.
    fn read_word_ref(bus: &mut MachineBus, addr: u32) -> u16 {
        let hi = bus.read_byte(addr) as u16;
        let lo = bus.read_byte(addr.wrapping_add(1)) as u16;
        (hi << 8) | lo
    }

    fn read_long_ref(bus: &mut MachineBus, addr: u32) -> u32 {
        let hi = read_word_ref(bus, addr) as u32;
        let lo = read_word_ref(bus, addr.wrapping_add(2)) as u32;
        (hi << 16) | lo
    }

    /// Every device this bus can host, attached and configured at once
    /// (exactly [`autoconfig::MAX_BOARDS`] boards -- `with_graphics_zorro_iii`
    /// rather than the two-board Zorro II Graffity variant, to leave room
    /// for `pcibridge` alongside it), walking the whole 32-bit address map
    /// in 4 KB steps plus every fixed region boundary this bus decodes
    /// against. `read_word`/`read_long` must agree with [`read_word_ref`]/
    /// [`read_long_ref`] everywhere -- the fast path in front of the
    /// chain must never change what a single access observes, only how
    /// it gets there.
    #[test]
    fn width_native_reads_agree_with_the_byte_composed_reference_across_the_whole_map() {
        let mut ram = boxed_chip_ram();
        let mut rom = [0u8; ROM_WINDOW_SIZE];
        rom[0] = 0x11;
        let mut mirage_disk = MirageDisk::new(64);
        let mut hostblk_disk = MirageDisk::new(64);
        let mut graffity_vram = std::vec![0u8; 64 * 1024];
        let mut rtg_vram = std::vec![0u8; 64 * 1024];
        let modes = [rtgboard::ModeDescriptor {
            width: 640,
            height: 480,
            format: rtgboard::format::RGBX_8888,
        }];
        let mut fast = std::vec![0u8; fastram::MIN_SIZE_BYTES as usize];
        let mut packet_backend = StubPacketBackend;
        let mut hostbridge_dev = pci::HostBridge::new();
        let mut slots = [pci::VirtualSlot {
            bdf: pci::Bdf {
                bus: 0,
                device: 0,
                function: 0,
            },
            device: &mut hostbridge_dev,
        }];
        let mut vpci = pci::VirtualPciBus::new(&mut slots);

        let mut bus = new_bus(&mut ram, &rom)
            .with_mirage(0, &mut mirage_disk)
            .with_hostblk(0, &mut hostblk_disk, false)
            .with_graphics_zorro_iii(&mut graffity_vram)
            .with_input()
            .with_rtgboard(&mut rtg_vram, &modes)
            .with_fast_ram(&mut fast)
            .with_pktport(&mut packet_backend)
            .with_pcibridge(&mut vpci);

        // Configure every board in exactly the order it was registered
        // above -- AUTOCONFIG only ever lets the *current* board answer
        // (autoconfig module docs).
        configure_zorro_ii(&mut bus, 0x20); // mirage -> $200000
        configure_zorro_iii(&mut bus, 0x4000_0000); // hostblk
        configure_zorro_iii(&mut bus, 0x5000_0000); // graffity (Zorro III)
        configure_zorro_iii(&mut bus, 0x6000_0000); // input
        configure_zorro_iii(&mut bus, 0x7000_0000); // rtgboard
        configure_zorro_iii(&mut bus, 0x8000_0000); // fast_ram
        configure_zorro_ii(&mut bus, 0x50); // pktport -> $500000
        configure_zorro_iii(&mut bus, 0x9000_0000); // pcibridge

        let mut addresses: std::vec::Vec<u32> = std::vec::Vec::new();
        let mut addr: u64 = 0;
        while addr <= u32::MAX as u64 {
            addresses.push(addr as u32);
            addr += 0x1000; // 4 KB steps, per `docs/bus-fast-path-plan.md`'s
                            // differential-test spec
        }
        for edge in [
            0u32,
            OVERLAY_END,
            CHIP_RAM_BASE,
            CHIP_RAM_END,
            0x0020_0000,
            0x0021_0000, // mirage's window
            0x0050_0000,
            0x0051_0000, // pktport's window
            CIA_BASE,
            CIA_END,
            CUSTOM_BASE,
            CUSTOM_END,
            autoconfig::AUTOCONFIG_BASE,
            autoconfig::AUTOCONFIG_END,
            rom::EXT_ROM_BASE,
            rom::EXT_ROM_BASE + rom::EXT_ROM_WINDOW_SIZE as u32,
            ROM_BASE,
            ROM_END,
            0x4000_0000,
            0x5000_0000,
            0x6000_0000,
            0x7000_0000,
            0x8000_0000,
            0x9000_0000,
        ] {
            addresses.push(edge);
            if edge > 0 {
                addresses.push(edge - 1);
            }
            if edge < u32::MAX {
                addresses.push(edge + 1);
            }
        }

        for &addr in &addresses {
            // Word/long comparisons only at even addresses: the custom
            // chip registers decode in 2-byte-aligned pairs
            // (`read_custom_word`'s own `& 0x1FE`), so a genuinely odd
            // address there makes the word-composed path (`read_long`'s
            // two `read_word` calls) and the pure byte-composed
            // reference disagree about *which* register pairing answers
            // -- a real 68k never issues a misaligned word/long access
            // in the first place (address error), so this is a decoding
            // property of that one region, not a fast-path regression.
            // The dedicated straddle tests above already cover the
            // odd-address case where it matters (chip RAM's and fast
            // RAM's own boundaries, neither of which has this
            // ambiguity).
            if !addr.is_multiple_of(2) {
                continue;
            }
            assert_eq!(
                bus.read_word(addr),
                read_word_ref(&mut bus, addr),
                "word at {addr:#010x}"
            );
            assert_eq!(
                bus.read_long(addr),
                read_long_ref(&mut bus, addr),
                "long at {addr:#010x}"
            );
        }
    }

    // ---- Step 4: lazy tick vs. the pre-step-4 eager path -----------

    /// A small, deterministic, dependency-free PRNG (xorshift64*) for the
    /// differential test below -- this crate is `no_std` and
    /// dependency-free even in its `std`-using hosted test code, so
    /// pulling in `rand` for one test is not on the table.
    struct Xorshift64 {
        state: u64,
    }

    impl Xorshift64 {
        fn new(seed: u64) -> Self {
            // xorshift64* requires a non-zero seed.
            Self {
                state: if seed == 0 {
                    0xDEAD_BEEF_CAFE_F00D
                } else {
                    seed
                },
            }
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.state = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn next_u32(&mut self) -> u32 {
            self.next_u64() as u32
        }
    }

    /// A CIA register address for [`MachineBus::read_byte`]/`write_byte`,
    /// the same address decode [`MachineBus::cia_select`] reverses:
    /// CIA-A on odd addresses, CIA-B on even, register index in bits
    /// 8-12.
    fn cia_addr(is_a: bool, reg: u8) -> u32 {
        CIA_BASE + ((reg as u32) << 8) + if is_a { 1 } else { 0 }
    }

    /// Drives two identically-configured buses through the same random
    /// sequence of `tick` clocks and (rare) register accesses -- one
    /// entirely through [`MachineBus::tick_eager`] (the pre-step-4
    /// behaviour: every call applies immediately), the other through the
    /// shipped [`MachineBus::tick`] (accumulate-then-flush-at-a-deadline)
    /// -- and asserts [`MachineBus::pending_irq_level`] and
    /// [`MachineBus::frames`] agree after every single step, not just at
    /// the end. This is the exactness proof `docs/bus-fast-path-plan.md`
    /// step 4 requires: both paths bottom out in the same
    /// [`MachineBus::tick_exact`], so disagreement here means the
    /// deferral itself (the deadline computation, a missing flush on
    /// some access, or a missing `reassert_level_irqs` call) is wrong,
    /// not that the underlying tick arithmetic is.
    ///
    /// Register accesses are deliberately rare (1 in 64 steps): with one
    /// on almost every step (an earlier version of this test), every
    /// access flushes both buses back in step, so the lazy path was
    /// caught up almost continuously and the test could not have caught
    /// a broken deadline computation. The long pure-tick stretches this
    /// produces are the actual point -- once the rare CRA/CRB/latch
    /// writes start a timer with a small latch, it underflows many times
    /// across a run of steps with no register access at all, which is
    /// exactly where `pending_irq_level`/`frames` not flushing (this
    /// task's fix 1) and `next_event_clocks` both have to be right
    /// without any access forcing a catch-up.
    ///
    /// Exercises timer one-shot and continuous mode (`CRA`/`CRB` writes),
    /// latch writes (including small latches, so underflows happen often
    /// across the pure-tick stretches rather than being rare edge
    /// events), ICR mask writes and reads (which also exercises the
    /// read-clears-and-acknowledges path), TOD reads, `VPOSR`/`VHPOSR`
    /// reads, `INTENA` writes, and both `INTREQ` set *and* clear writes
    /// (the clear is what exercises `reassert_level_irqs`: clearing a
    /// bit a still-latched CIA/Graffity source drives must reassert it
    /// at this exact instruction boundary, not at the next deadline).
    /// Four seeds, 400,000 ticks each: 1,600,000 total.
    #[test]
    fn lazy_tick_matches_eager_tick_across_random_sequences() {
        const TICKS_PER_SEED: usize = 400_000;
        const SEEDS: [u64; 4] = [
            0x1234_5678_9ABC_DEF0,
            0x0BAD_F00D_DEAD_BEEF,
            0x2545_F491_4F6C_DD1D,
            0x9E37_79B9_7F4A_7C15,
        ];

        for seed in SEEDS {
            let mut rng = Xorshift64::new(seed);
            let mut ram_lazy = boxed_chip_ram();
            let mut ram_eager = boxed_chip_ram();
            let rom = [0u8; ROM_WINDOW_SIZE];
            let mut lazy = new_bus(&mut ram_lazy, &rom);
            let mut eager = new_bus(&mut ram_eager, &rom);

            for step in 0..TICKS_PER_SEED {
                // Plausible per-retired-instruction cycle counts (a real
                // 68040 instruction is rarely more than a few dozen
                // cycles at this machine's CPU-clock scale).
                let clocks = 1 + (rng.next_u32() % 60);
                lazy.tick(clocks);
                eager.tick_eager(clocks);

                // A register access on only 1 in 64 steps -- see this
                // test's own doc comment for why that matters.
                if rng.next_u32().is_multiple_of(64) {
                    match rng.next_u32() % 13 {
                        0 => {
                            // CRA: START/RUNMODE(one-shot vs continuous)/
                            // INMODE/SPMODE, every bit random.
                            let v = rng.next_u32() as u8;
                            let addr = cia_addr(true, cia::reg::CRA);
                            lazy.write_byte(addr, v);
                            eager.write_byte(addr, v);
                        }
                        1 => {
                            let v = rng.next_u32() as u8;
                            let addr = cia_addr(false, cia::reg::CRB);
                            lazy.write_byte(addr, v);
                            eager.write_byte(addr, v);
                        }
                        2 => {
                            // Latch low byte, CIA-A timer A.
                            let v = rng.next_u32() as u8;
                            let addr = cia_addr(true, cia::reg::TALO);
                            lazy.write_byte(addr, v);
                            eager.write_byte(addr, v);
                        }
                        3 => {
                            // Latch high byte -- masked small so a timer
                            // started with this latch underflows many
                            // times across the following pure-tick
                            // stretch instead of once, right up until
                            // the next rare register access.
                            let v = (rng.next_u32() & 0x03) as u8;
                            let addr = cia_addr(true, cia::reg::TAHI);
                            lazy.write_byte(addr, v);
                            eager.write_byte(addr, v);
                        }
                        4 => {
                            let v = rng.next_u32() as u8;
                            let addr = cia_addr(false, cia::reg::TBLO);
                            lazy.write_byte(addr, v);
                            eager.write_byte(addr, v);
                        }
                        5 => {
                            let v = (rng.next_u32() & 0x03) as u8;
                            let addr = cia_addr(false, cia::reg::TBHI);
                            lazy.write_byte(addr, v);
                            eager.write_byte(addr, v);
                        }
                        6 => {
                            // ICR mask write, CIA-A: SETCLR plus TA/TB/
                            // ALRM/SP/FLG.
                            let v = (rng.next_u32() & 0x9F) as u8;
                            let addr = cia_addr(true, cia::reg::ICR);
                            lazy.write_byte(addr, v);
                            eager.write_byte(addr, v);
                        }
                        7 => {
                            let v = (rng.next_u32() & 0x9F) as u8;
                            let addr = cia_addr(false, cia::reg::ICR);
                            lazy.write_byte(addr, v);
                            eager.write_byte(addr, v);
                        }
                        8 => {
                            // ICR read: read-clears-and-acknowledges, so
                            // this also proves the two paths agree about
                            // *which* sources were pending at this exact
                            // step.
                            let addr = cia_addr(true, cia::reg::ICR);
                            assert_eq!(
                                lazy.read_byte(addr),
                                eager.read_byte(addr),
                                "seed {seed:#x} step {step}: CIA-A ICR read diverged"
                            );
                        }
                        9 => {
                            let addr = cia_addr(false, cia::reg::ICR);
                            assert_eq!(
                                lazy.read_byte(addr),
                                eager.read_byte(addr),
                                "seed {seed:#x} step {step}: CIA-B ICR read diverged"
                            );
                        }
                        10 => {
                            // Full TOD read (TODHI latches, TODLO
                            // releases), alternating CIAs.
                            let is_a = rng.next_u32().is_multiple_of(2);
                            for reg in [cia::reg::TODHI, cia::reg::TODMID, cia::reg::TODLO] {
                                let addr = cia_addr(is_a, reg);
                                assert_eq!(
                                    lazy.read_byte(addr),
                                    eager.read_byte(addr),
                                    "seed {seed:#x} step {step}: TOD read diverged (CIA-{})",
                                    if is_a { 'A' } else { 'B' }
                                );
                            }
                        }
                        11 => match rng.next_u32() % 3 {
                            0 => {
                                let addr = CUSTOM_BASE + chipset::reg::VPOSR as u32;
                                assert_eq!(
                                    lazy.read_word(addr),
                                    eager.read_word(addr),
                                    "seed {seed:#x} step {step}: VPOSR diverged"
                                );
                            }
                            1 => {
                                let addr = CUSTOM_BASE + chipset::reg::VHPOSR as u32;
                                assert_eq!(
                                    lazy.read_word(addr),
                                    eager.read_word(addr),
                                    "seed {seed:#x} step {step}: VHPOSR diverged"
                                );
                            }
                            _ => {
                                // INTENA write, set semantics (bit 15
                                // high): can unmask an already-latched
                                // source.
                                let v = 0x8000 | (rng.next_u32() as u16 & 0x7FFF);
                                let addr = CUSTOM_BASE + chipset::reg::INTENA as u32;
                                lazy.write_word(addr, v);
                                eager.write_word(addr, v);
                            }
                        },
                        _ => {
                            // INTREQ: alternate set (bit 15 high) and
                            // clear (bit 15 low) writes. The clear is
                            // what exercises `reassert_level_irqs` --
                            // clearing a bit a still-latched CIA source
                            // drives must reassert it immediately, not
                            // at the next deadline several ticks away.
                            let set = rng.next_u32().is_multiple_of(2);
                            let bits = (rng.next_u32() as u16) & 0x7FFF;
                            let v = if set { 0x8000 | bits } else { bits };
                            let addr = CUSTOM_BASE + chipset::reg::INTREQ as u32;
                            lazy.write_word(addr, v);
                            eager.write_word(addr, v);
                        }
                    }
                }

                assert_eq!(
                    lazy.pending_irq_level(),
                    eager.pending_irq_level(),
                    "seed {seed:#x} step {step}: pending_irq_level diverged"
                );
                assert_eq!(
                    lazy.frames(),
                    eager.frames(),
                    "seed {seed:#x} step {step}: frames diverged"
                );
            }
        }
    }
}
