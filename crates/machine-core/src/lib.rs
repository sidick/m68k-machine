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
//! Still absent, by phase: the software blitter and renderer (Phase 2),
//! Zorro III board space and fast RAM (Phase 3). Those addresses fall
//! through to the open-bus rule below, which is exactly what an
//! unpopulated real machine would do at them today.
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
pub mod chipset;
pub mod cia;
pub mod cirrus;
pub mod display;
pub mod gayle;
pub mod graffity;
pub mod render;
pub mod rom;

use autoconfig::AutoConfig;
use blitter::Blitter;
use chipset::Chipset;
use cia::{Cia, CiaId, FloppyDrive, FloppyPresence};
use gayle::{BlockDevice, Gayle};
use graffity::Graffity;

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
    chip_ram: &'a mut [u8; CHIP_RAM_SIZE],
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
    /// Gayle and its IDE interface — the bring-up storage device (see
    /// [`gayle`]). Absent unless a board layer attaches one.
    pub gayle: Gayle,
    /// The Zorro AUTOCONFIG chain (§9). Every expansion this machine
    /// offers is discovered through it.
    pub autoconfig: AutoConfig,
    /// The disk behind Gayle's IDE port, supplied by the board layer
    /// since this crate has no file I/O of its own.
    hd: Option<&'a mut dyn BlockDevice>,

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

    /// While set, the ROM is mirrored over the bottom of the address
    /// space so the CPU's reset vector fetch from `$000000`/`$000004`
    /// lands in ROM. Real hardware does this with Gary, driven by CIA-A
    /// PRA bit 0 (OVL), which is high out of reset; the OS clears it
    /// early in the strap once it no longer needs ROM at zero.
    overlay: bool,
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
        Self {
            chip_ram,
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
            gayle: Gayle::new(),
            autoconfig: AutoConfig::new(),
            hd: None,
            graphics: None,
            graphics_boards: [None; graffity::MAX_GRAFFITY_BOARDS],
            overlay: true,
        }
    }

    /// Attach a disk to Gayle's IDE port.
    pub fn with_hd(mut self, hd: &'a mut dyn BlockDevice) -> Self {
        self.hd = Some(hd);
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

    /// Advance time by `cpu_clocks`, ticking the frame clock and both
    /// CIAs. Call this from the CPU's `sync` hook so device time and
    /// guest time stay in step.
    pub fn tick(&mut self, cpu_clocks: u32) {
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

        if self.cia_a.tick(cpu_clocks, CPU_CLOCKS_PER_ECLOCK) {
            self.chipset.raise_int(chipset::intbit::PORTS);
        }
        if self.cia_b.tick(cpu_clocks, CPU_CLOCKS_PER_ECLOCK) {
            self.chipset.raise_int(chipset::intbit::EXTER);
        }
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
    pub fn pending_irq_level(&self) -> u8 {
        self.chipset.pending_level()
    }

    /// Read one byte. Open-bus addresses return [`OPEN_BUS_BYTE`].
    pub fn read_byte(&mut self, address: u32) -> u8 {
        // Overlay first: while OVL is asserted the ROM answers for low
        // memory ahead of chip RAM.
        if self.overlay && address < OVERLAY_END {
            return rom::read_mirrored(self.rom, 0, address);
        }

        if (CHIP_RAM_BASE..CHIP_RAM_END).contains(&address) {
            self.chip_ram[(address - CHIP_RAM_BASE) as usize]
        } else if AutoConfig::responds_to(address) {
            self.autoconfig.read(address)
        } else if Gayle::responds_to(address) {
            // Reborrow the device rather than moving it out: Gayle
            // needs it only for the duration of this access.
            let device: Option<&mut dyn BlockDevice> = match &mut self.hd {
                Some(d) => Some(&mut **d),
                None => None,
            };
            let value = self.gayle.read(address, device);
            // Reads can raise Gayle's interrupt too, not just writes:
            // draining the last word of a block in a multi-sector READ
            // SECTORS makes the drive fetch the next block and assert
            // INTRQ ("data ready"), and that happens inside a data-
            // register *read*. Without this mirror of `write_byte`'s
            // propagation, every per-sector interrupt after the first
            // was lost and scsi.device stalled mid-transfer until an
            // unrelated CIA interrupt rescued it -- or timed out and
            // retried, which is how a multi-sector file read turned into
            // a DOS "object not found" during boot.
            if self.gayle.irq_pending() {
                self.chipset.raise_int(chipset::intbit::PORTS);
            }
            value
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
        } else if let Some((board, offset)) = self.graphics_target(address) {
            match &mut self.graphics {
                Some(card) => card.read(board, offset),
                None => OPEN_BUS_BYTE,
            }
        } else {
            OPEN_BUS_BYTE
        }
    }

    /// Whether `address` falls inside one of the attached Graffity
    /// card's configured AUTOCONFIG windows, and if so, which of the
    /// card's own board indices and the offset within it -- the input
    /// to [`Graffity::read`]/[`Graffity::write`]. `None` whenever no
    /// card is attached (`graphics_boards` is never anything but all
    /// `None` without one) or the address belongs to some other board
    /// entirely. This function is the whole of what `MachineBus` knows
    /// about Graffity's address layout: it carries no notion of VRAM,
    /// registers, or any other aperture, only which chain index maps to
    /// which board index (`Self::graphics_boards`'s own doc comment).
    fn graphics_target(&self, address: u32) -> Option<(usize, u32)> {
        let idx = self.autoconfig.board_at(address)?;
        let base = self.autoconfig.placement(idx)?.base;
        let board = self
            .graphics_boards
            .iter()
            .position(|&chain_idx| chain_idx == Some(idx))?;
        Some((board, address - base))
    }

    /// Decode a CIA access. CIA-A occupies odd addresses, CIA-B even
    /// ones, and the register index comes from address bits 8-12.
    fn cia_select(address: u32) -> (bool, u8) {
        let is_cia_a = address & 1 != 0;
        let reg = ((address >> 8) & 0x0F) as u8;
        (is_cia_a, reg)
    }

    fn read_cia(&mut self, address: u32) -> u8 {
        let (is_cia_a, reg) = Self::cia_select(address);
        if is_cia_a {
            self.cia_a.read(reg)
        } else {
            self.cia_b.read(reg)
        }
    }

    fn write_cia(&mut self, address: u32, value: u8) {
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
    }

    /// Merge the floppy model's PRA bits (2-5) into CIA-A's input pin
    /// field, leaving the other bits (OVL/LED output latch, mouse fire
    /// buttons) untouched.
    fn sync_floppy_status(&mut self) {
        self.cia_a.pra_input =
            (self.cia_a.pra_input & !cia::FLOPPY_PRA_MASK) | self.floppy.pra_status_bits();
    }

    fn read_custom_word(&mut self, address: u32) -> u16 {
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
        let offset = (address - CUSTOM_BASE) as u16 & 0x1FE;
        if blitter::reg::is_blitter(offset) {
            if self.blitter.write(offset, value) {
                self.run_blitter();
            }
            return;
        }
        self.chipset.write(offset, value);
    }

    /// Run an armed blit to completion and raise the blitter-finished
    /// interrupt. Synchronous by design — see [`blitter`]'s module docs.
    fn run_blitter(&mut self) {
        if self.blitter.execute(self.chip_ram) {
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
        let hi = self.read_byte(address) as u16;
        let lo = self.read_byte(address.wrapping_add(1)) as u16;
        (hi << 8) | lo
    }

    /// Read one big-endian 32-bit longword, composed from four byte reads.
    pub fn read_long(&mut self, address: u32) -> u32 {
        let hi = self.read_word(address) as u32;
        let lo = self.read_word(address.wrapping_add(2)) as u32;
        (hi << 16) | lo
    }

    /// Write one byte. Writes to chip RAM take effect; writes to ROM or
    /// open bus are silently discarded (real ROM cannot be written, and a
    /// real open-bus write simply has nothing latch it).
    pub fn write_byte(&mut self, address: u32, value: u8) {
        // A write under the overlay still reaches chip RAM: the overlay
        // only redirects reads, since there is nothing behind ROM to
        // write to and the OS relies on being able to build its vector
        // table at $000000 before clearing OVL.
        if (CHIP_RAM_BASE..CHIP_RAM_END).contains(&address) {
            self.chip_ram[(address - CHIP_RAM_BASE) as usize] = value;
        } else if AutoConfig::responds_to(address) {
            self.autoconfig.write(address, value);
        } else if Gayle::responds_to(address) {
            let device: Option<&mut dyn BlockDevice> = match &mut self.hd {
                Some(d) => Some(&mut **d),
                None => None,
            };
            self.gayle.write(address, value, device);
            // The A1200 wires Gayle's interrupt to INT2 alongside CIA-A.
            if self.gayle.irq_pending() {
                self.chipset.raise_int(chipset::intbit::PORTS);
            }
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
        } else if let Some((board, offset)) = self.graphics_target(address) {
            if let Some(card) = &mut self.graphics {
                card.write(board, offset, value);
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
        self.write_byte(address, (value >> 8) as u8);
        self.write_byte(address.wrapping_add(1), value as u8);
    }

    /// Write one big-endian 32-bit longword, decomposed into two word
    /// writes.
    pub fn write_long(&mut self, address: u32, value: u32) {
        self.write_word(address, (value >> 16) as u16);
        self.write_word(address.wrapping_add(2), value as u16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_bus<'a>(chip_ram: &'a mut [u8; CHIP_RAM_SIZE], rom: &'a [u8]) -> MachineBus<'a> {
        MachineBus::new(chip_ram, rom)
    }

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
        // A1200 3.2 ROM specifically probes.
        // $DE1000 is deliberately absent: that is the Gayle ID
        // register, which now genuinely answers there (see `gayle`).
        // §11.1 expected it to read as absent under the open-bus rule
        // and allowed for a stub if it did not; this is that stub grown
        // into a real interface.
        for &addr in &[0x00C0_0000u32, 0x00D8_0000, 0x00DE_0000, 0x00DD_0000] {
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

    /// A tiny in-memory disk for exercising Gayle through the bus.
    struct MemDisk {
        sectors: std::vec::Vec<[u8; gayle::SECTOR_BYTES]>,
    }

    impl gayle::BlockDevice for MemDisk {
        fn sector_count(&self) -> u64 {
            self.sectors.len() as u64
        }
        fn read_sector(&mut self, lba: u64, buf: &mut [u8; gayle::SECTOR_BYTES]) -> bool {
            self.sectors.get(lba as usize).map(|s| *buf = *s).is_some()
        }
        fn write_sector(&mut self, lba: u64, buf: &[u8; gayle::SECTOR_BYTES]) -> bool {
            match self.sectors.get_mut(lba as usize) {
                Some(s) => {
                    *s = *buf;
                    true
                }
                None => false,
            }
        }
    }

    /// The per-sector "next block ready" interrupt of a multi-sector READ
    /// SECTORS is raised while the guest *reads* the data register (the
    /// drive asserts INTRQ the moment the next block lands in its
    /// buffer), so the bus must sample Gayle's interrupt line on the read
    /// path as well as the write path. When only `write_byte` propagated
    /// it, every per-sector interrupt after the first was lost and
    /// scsi.device stalled mid-transfer -- surfacing as file reads
    /// failing ("object not found") partway through the AmigaOS 3.2.2
    /// Startup-Sequence.
    #[test]
    fn multi_sector_read_raises_ports_interrupt_between_sectors() {
        let mut ram = boxed_chip_ram();
        let rom = [0u8; ROM_WINDOW_SIZE];
        let mut disk = MemDisk {
            sectors: std::vec![[0u8; gayle::SECTOR_BYTES]; 4],
        };
        let mut bus = new_bus(&mut ram, &rom).with_hd(&mut disk);

        // Enable Gayle's IDE interrupt, then issue READ SECTORS for two
        // sectors from LBA 0.
        bus.write_byte(gayle::reg::GAYLE_INTENA, gayle::GAYLE_IRQ_IDE);
        bus.write_byte(gayle::reg::IDE_SELECT, 0xE0); // LBA mode, LBA 27..24 = 0
        bus.write_byte(gayle::reg::IDE_HCYL, 0);
        bus.write_byte(gayle::reg::IDE_LCYL, 0);
        bus.write_byte(gayle::reg::IDE_SECTOR, 0);
        bus.write_byte(gayle::reg::IDE_NSECTOR, 2);
        bus.write_byte(gayle::reg::IDE_STATUS, gayle::cmd::READ_SECTORS);

        let ports = 1u16 << chipset::intbit::PORTS;
        assert_eq!(
            bus.read_word(CUSTOM_BASE + chipset::reg::INTREQR as u32) & ports,
            ports,
            "first block ready raises PORTS via the command write"
        );

        // Acknowledge both latches the way the driver's INT2 server
        // does: read IDE status (drops Gayle's INTRQ) and clear the
        // chipset's PORTS request bit.
        let _ = bus.read_byte(gayle::reg::IDE_STATUS);
        bus.write_word(CUSTOM_BASE + chipset::reg::INTREQ as u32, ports);
        assert_eq!(
            bus.read_word(CUSTOM_BASE + chipset::reg::INTREQR as u32) & ports,
            0
        );

        // Drain the whole first sector through data-register reads. The
        // final read makes Gayle fetch sector two and assert INTRQ --
        // entirely inside `read_byte`.
        for _ in 0..gayle::SECTOR_BYTES / 2 {
            let _ = bus.read_byte(gayle::reg::IDE_DATA);
            let _ = bus.read_byte(gayle::reg::IDE_DATA + 1);
        }
        assert_eq!(
            bus.read_word(CUSTOM_BASE + chipset::reg::INTREQR as u32) & ports,
            ports,
            "the second block's per-sector interrupt must reach the chipset"
        );
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
        // the AUTOCONFIG window, Gayle, the CIAs and the custom chips.
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
}
