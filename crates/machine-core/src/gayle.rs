//! Gayle and its A1200 IDE interface.
//!
//! This is a deliberate exception to the machine's "chipset-less, all I/O
//! through Zorro III" design (proposal §1, §9). It exists because the
//! A1200 Kickstart already contains a working IDE driver — `scsi.device`
//! reports itself as `IDE_scsidisk 47.4` and initialises on this machine
//! today, alongside `FileSystem.resource` and FFS — so emulating the
//! register interface it already drives gets a mountable, bootable disk
//! with **no m68k code of our own**.
//!
//! MIRAGE (§10.3) remains the architecture's storage story and still has
//! to be built: a Zorro III card with an AUTOCONFIG shim, DiagArea
//! injection and its own m68k driver. Gayle is the bring-up device that
//! lets the rest of the machine be exercised against a real filesystem
//! and a real Workbench before that chain exists — and once MIRAGE
//! lands, this becomes an ordinary compatibility option rather than the
//! path anything depends on.
//!
//! §9 already sanctions the approach in principle: "Where an open m68k
//! driver already exists, present its register map (e.g. LIDE-compatible
//! IDE for `lide.device`) instead of inventing one."
//!
//! §11.1 anticipated the ID register specifically, expecting Gayle to
//! read as absent under the open-bus rule and noting that if it did not,
//! the fallback was "a two-register stub, not a design change". This is
//! that stub, grown into a working interface.

/// Base of the Gayle register window, `$DA0000`-`$DAFFFF`.
pub const GAYLE_BASE: u32 = 0x00DA_0000;
pub const GAYLE_END: u32 = GAYLE_BASE + 0x0001_0000;

/// The Gayle ID register window, `$DE1000`-`$DE1FFF`.
pub const GAYLE_ID_BASE: u32 = 0x00DE_1000;
pub const GAYLE_ID_END: u32 = GAYLE_ID_BASE + 0x0000_1000;

/// IDE task-file registers, as the A1200 maps them. Each sits four bytes
/// apart, and the data register is 16-bit while the rest are byte-wide.
pub mod reg {
    pub const IDE_DATA: u32 = 0x00DA_2000;
    pub const IDE_ERROR: u32 = 0x00DA_2004;
    pub const IDE_NSECTOR: u32 = 0x00DA_2008;
    pub const IDE_SECTOR: u32 = 0x00DA_200C;
    pub const IDE_LCYL: u32 = 0x00DA_2010;
    pub const IDE_HCYL: u32 = 0x00DA_2014;
    pub const IDE_SELECT: u32 = 0x00DA_2018;
    pub const IDE_STATUS: u32 = 0x00DA_201C;

    /// Gayle interrupt status; bit 7 is the IDE interrupt.
    pub const GAYLE_INTREQ: u32 = 0x00DA_9000;
    /// Gayle interrupt enable.
    pub const GAYLE_INTENA: u32 = 0x00DA_A000;
    /// Gayle status/credit register.
    pub const GAYLE_STATUS: u32 = 0x00DA_8000;
}

/// Gayle interrupt-request bit 7: the IDE interrupt.
pub const GAYLE_IRQ_IDE: u8 = 0x80;

/// ATA status register bits.
pub mod status {
    pub const ERR: u8 = 1 << 0;
    pub const DRQ: u8 = 1 << 3;
    pub const DSC: u8 = 1 << 4;
    pub const DRDY: u8 = 1 << 6;
    pub const BSY: u8 = 1 << 7;
}

/// ATA error-register bits (ATA-1 through ATA-8 agree on this layout).
/// `IDNF` ("ID not found") is what an out-of-range or nonexistent
/// sector reports; `ABRT` is what an unrecognised command reports.
pub mod error {
    pub const AMNF: u8 = 1 << 0;
    pub const TK0NF: u8 = 1 << 1;
    pub const ABRT: u8 = 1 << 2;
    pub const MCR: u8 = 1 << 3;
    pub const IDNF: u8 = 1 << 4;
    pub const MC: u8 = 1 << 5;
    pub const UNC: u8 = 1 << 6;
    pub const BBK: u8 = 1 << 7;
}

/// ATA commands this interface answers. Anything else sets `ERR`.
pub mod cmd {
    pub const READ_SECTORS: u8 = 0x20;
    pub const READ_SECTORS_NR: u8 = 0x21;
    pub const WRITE_SECTORS: u8 = 0x30;
    pub const WRITE_SECTORS_NR: u8 = 0x31;
    pub const IDENTIFY: u8 = 0xEC;
    pub const INIT_PARAMS: u8 = 0x91;
    pub const SET_FEATURES: u8 = 0xEF;
}

/// Bytes in one sector. LBA28 throughout: 128 GB is far beyond anything
/// this machine will present, and the OS-side limit is lower still.
pub const SECTOR_BYTES: usize = 512;

/// The backing store behind the IDE interface.
///
/// `machine-core` has no allocator and no file I/O, so the host supplies
/// the storage: a file-backed image in the hosted runner, an embedded
/// image on a bare-metal board layer. Returning `false` from either
/// method surfaces to the guest as an ATA error rather than a panic —
/// the guest controls the LBA and must not be able to fault the machine.
pub trait BlockDevice {
    /// Total addressable sectors.
    fn sector_count(&self) -> u64;

    /// Read one sector. `false` reports a device error to the guest.
    fn read_sector(&mut self, lba: u64, buf: &mut [u8; SECTOR_BYTES]) -> bool;

    /// Write one sector. `false` reports a device error to the guest.
    fn write_sector(&mut self, lba: u64, buf: &[u8; SECTOR_BYTES]) -> bool;
}

/// The ID this Gayle reports, shifted out one bit at a time from the
/// top. Kickstart uses it to decide whether an IDE interface is present
/// at all, and which one: `$D0` is the A600's gate array and **`$D1`
/// the A1200's**. This machine presents an A1200 (proposal §11.1 —
/// the A1200 ROM is the target precisely because it is the
/// Zorro III-capable one), so `$D1` is the correct value. Kickstart's
/// probe accepts either, so getting this wrong is invisible at boot and
/// would only surface in software that distinguishes the two.
pub const GAYLE_ID: u8 = 0xD1;

/// Drive/head select register bit 6: LBA28 addressing rather than CHS.
const SELECT_LBA: u8 = 1 << 6;

/// Drive/head select register bits 0-3: CHS head number, or the top four
/// bits of an LBA28 address.
const SELECT_HEAD_MASK: u8 = 0x0F;

/// Gayle's register file and the IDE task file behind it.
///
/// Holds no backing store of its own: [`Gayle::read`] and
/// [`Gayle::write`] take the [`BlockDevice`] from the caller, so the bus
/// can own the device and hand it down for the duration of an access.
pub struct Gayle {
    /// Bit position of the next ID bit to hand back. Writing the ID
    /// register resets this; each read shifts one bit out of the top.
    id_bit: u8,

    /// Task-file registers.
    pub error: u8,
    pub nsector: u8,
    pub sector: u8,
    pub lcyl: u8,
    pub hcyl: u8,
    pub select: u8,
    pub status: u8,

    /// Gayle interrupt request and enable.
    pub intreq: u8,
    pub intena: u8,

    /// CHS geometry currently in effect: heads and sectors-per-track,
    /// set by INITIALIZE DEVICE PARAMETERS (`cmd::INIT_PARAMS`) and used
    /// to translate CHS task-file addresses to an LBA. Plausible
    /// defaults stand in until a driver programs its own.
    heads: u8,
    spt: u8,

    /// Sector buffer for the transfer in flight, and how far through it
    /// the guest has read or written.
    buffer: [u8; SECTOR_BYTES],
    buffer_pos: usize,
    /// Sectors still to transfer after the current one. A `u16` because
    /// `nsector == 0` means 256 sectors, the standard ATA convention.
    sectors_left: u16,
    /// Whether the in-flight transfer is a write.
    writing: bool,
    /// LBA of the sector currently in `buffer`, advanced as a multi-
    /// sector transfer proceeds and mirrored back into the task-file
    /// registers (in whichever addressing mode is selected) the way a
    /// real drive leaves its position visible after each block.
    current_lba: u32,
}

impl Default for Gayle {
    fn default() -> Self {
        Self::new()
    }
}

impl Gayle {
    pub fn new() -> Self {
        Self {
            id_bit: 0,
            error: 0,
            nsector: 0,
            sector: 0,
            lcyl: 0,
            hcyl: 0,
            select: 0,
            // Ready and seek-complete with nothing in flight, which is
            // what a drive with no command outstanding reports.
            status: status::DRDY | status::DSC,
            intreq: 0,
            intena: 0,
            // A plausible default geometry (16 heads, 63 sectors/track)
            // for CHS addressing before any driver calls INITIALIZE
            // DEVICE PARAMETERS -- matched by `identify`'s reported
            // default geometry.
            heads: 16,
            spt: 63,
            buffer: [0; SECTOR_BYTES],
            buffer_pos: 0,
            sectors_left: 0,
            writing: false,
            current_lba: 0,
        }
    }

    /// Whether Gayle is asserting its interrupt, which the bus routes on
    /// to INT2 (`PORTS`) as the A1200 wires it.
    pub fn irq_pending(&self) -> bool {
        self.intreq & self.intena & GAYLE_IRQ_IDE != 0
    }

    /// True for any address Gayle answers, so the bus can route it here
    /// rather than to the open-bus rule.
    pub fn responds_to(address: u32) -> bool {
        (GAYLE_BASE..GAYLE_END).contains(&address)
            || (GAYLE_ID_BASE..GAYLE_ID_END).contains(&address)
    }

    /// Read a Gayle or IDE register.
    pub fn read(&mut self, address: u32, device: Option<&mut dyn BlockDevice>) -> u8 {
        if (GAYLE_ID_BASE..GAYLE_ID_END).contains(&address) {
            return self.id_shift_read();
        }
        if (reg::IDE_DATA..reg::IDE_DATA + 0x20).contains(&address) {
            return self.read_task_file(address - reg::IDE_DATA, device);
        }
        match address & 0xFFFF_F000 {
            reg::GAYLE_STATUS => {
                // Live IDE interrupt line on bit 7; this machine models
                // no PCMCIA slot, so the rest of the register reads 0
                // rather than the real chip's pulled-up "empty slot"
                // pattern -- Kickstart's PCMCIA probe lives in
                // card.resource, not scsi.device, and is out of scope.
                if self.intreq & GAYLE_IRQ_IDE != 0 {
                    GAYLE_IRQ_IDE
                } else {
                    0
                }
            }
            reg::GAYLE_INTREQ => self.intreq,
            reg::GAYLE_INTENA => self.intena,
            _ => 0,
        }
    }

    /// Write a Gayle or IDE register.
    pub fn write(&mut self, address: u32, value: u8, device: Option<&mut dyn BlockDevice>) {
        if (GAYLE_ID_BASE..GAYLE_ID_END).contains(&address) {
            // Any write resets the shifter, regardless of value.
            self.id_bit = 0;
            return;
        }
        if (reg::IDE_DATA..reg::IDE_DATA + 0x20).contains(&address) {
            self.write_task_file(address - reg::IDE_DATA, value, device);
            return;
        }
        match address & 0xFFFF_F000 {
            // Write-to-clear, AND convention: a bit written 0 clears the
            // latch, a bit written 1 leaves it as it was.
            reg::GAYLE_INTREQ => self.intreq &= value,
            reg::GAYLE_INTENA => self.intena = value,
            _ => {}
        }
    }

    // ---- $DE1000 ID shift register -------------------------------------

    /// Shift the next bit of [`GAYLE_ID`] out from the top (bit 7 first),
    /// returned in bit 7 of the byte -- `$80` for a 1 bit, `$00` for a 0.
    fn id_shift_read(&mut self) -> u8 {
        let bit = (GAYLE_ID >> (7 - self.id_bit)) & 1;
        self.id_bit = (self.id_bit + 1) & 7;
        if bit != 0 {
            0x80
        } else {
            0x00
        }
    }

    // ---- task file -------------------------------------------------------

    /// `offset` is the address already relative to `reg::IDE_DATA`, so
    /// `0x00`/`0x01` are the data register's two bytes and `0x04`,
    /// `0x08`, ... are the byte-wide registers at their usual stride.
    fn read_task_file(&mut self, offset: u32, device: Option<&mut dyn BlockDevice>) -> u8 {
        match offset {
            0x00 => self.data_read_hi(),
            0x01 => self.data_read_lo(device),
            // No selected device: every task-file register but status
            // reads zero (an empty cable has nothing to latch); status
            // itself floats to all-ones -- see `read_status`. This is
            // what lets Kickstart's probe conclude cleanly that no
            // drive answers, the same as it would on real hardware with
            // an empty IDE port.
            0x04 if device.is_some() => self.error,
            0x08 if device.is_some() => self.nsector,
            0x0C if device.is_some() => self.sector,
            0x10 if device.is_some() => self.lcyl,
            0x14 if device.is_some() => self.hcyl,
            0x18 if device.is_some() => self.select,
            0x1C => self.read_status(device),
            _ => 0,
        }
    }

    fn write_task_file(&mut self, offset: u32, value: u8, device: Option<&mut dyn BlockDevice>) {
        match offset {
            0x00 => self.data_write_hi(value),
            0x01 => self.data_write_lo(value, device),
            // The error register doubles as "features" on write (SET
            // FEATURES's sub-code); we don't interpret feature codes,
            // just hold the byte.
            0x04 => self.error = value,
            0x08 => self.nsector = value,
            0x0C => self.sector = value,
            0x10 => self.lcyl = value,
            0x14 => self.hcyl = value,
            0x18 => self.select = value,
            0x1C => self.command(value, device),
            _ => {}
        }
    }

    /// Reading the status register both reports it and acknowledges the
    /// drive's interrupt, as ATA specifies.
    fn read_status(&mut self, device: Option<&mut dyn BlockDevice>) -> u8 {
        self.intreq &= !GAYLE_IRQ_IDE;
        match device {
            Some(_) => self.status,
            // Empty cable: WinUAE and real hardware alike float the
            // status register to all-ones rather than answering 0, so a
            // probe reads "no device" instead of misreading a
            // powered-off drive as present.
            None => 0xFF,
        }
    }

    // ---- PIO data path -----------------------------------------------

    /// The data register is 16-bit; a guest word access becomes two byte
    /// calls at `IDE_DATA` and `IDE_DATA + 1`. `IDE_DATA` (the low
    /// address, delivered first) hands back `buffer[pos]`, the high byte
    /// of the CPU-visible big-endian word; `IDE_DATA + 1` hands back
    /// `buffer[pos + 1]` and advances `pos`.
    ///
    /// This is deliberately a plain sequential copy with no byte
    /// swapping of its own: real Gayle wires the drive's D7-D0 to the
    /// CPU's D15-D8 (Copperline's `gayle.rs`, `identify_block`'s doc
    /// comment, confirmed against the Linux `gayle.c` IDE driver and
    /// cross-checked against the ROM's `scsi.device` disassembly), which
    /// exactly cancels out for opaque sector bytes -- the swap "puts
    /// file bytes back in natural memory order" -- so `buffer` can hold
    /// sector data exactly as a `.hdf` file or a real drive would, and
    /// the two register halves can be handed out in the order they sit
    /// in that buffer. Structured *fields* (as opposed to raw sector
    /// bytes) are a different matter -- see `identify_word` below, where
    /// the hardware swap has to be reproduced explicitly because those
    /// bytes carry ATA-defined numeric meaning rather than being an
    /// opaque stream.
    fn data_read_hi(&mut self) -> u8 {
        if self.writing || self.status & status::DRQ == 0 {
            return 0;
        }
        self.buffer[self.buffer_pos]
    }

    fn data_read_lo(&mut self, device: Option<&mut dyn BlockDevice>) -> u8 {
        if self.writing || self.status & status::DRQ == 0 {
            return 0;
        }
        let byte = self.buffer[self.buffer_pos + 1];
        self.buffer_pos += 2;
        if self.buffer_pos >= SECTOR_BYTES {
            self.finish_read_block(device);
        }
        byte
    }

    fn data_write_hi(&mut self, value: u8) {
        if !self.writing || self.status & status::DRQ == 0 {
            return;
        }
        self.buffer[self.buffer_pos] = value;
    }

    fn data_write_lo(&mut self, value: u8, device: Option<&mut dyn BlockDevice>) {
        if !self.writing || self.status & status::DRQ == 0 {
            return;
        }
        self.buffer[self.buffer_pos + 1] = value;
        self.buffer_pos += 2;
        if self.buffer_pos >= SECTOR_BYTES {
            self.finish_write_block(device);
        }
    }

    /// A read block has just been fully drained by the guest: move on to
    /// the next sector, or complete the transfer.
    fn finish_read_block(&mut self, device: Option<&mut dyn BlockDevice>) {
        self.sectors_left = self.sectors_left.saturating_sub(1);
        if self.sectors_left == 0 {
            self.status &= !(status::DRQ | status::BSY);
            return;
        }
        self.current_lba = self.current_lba.wrapping_add(1);
        self.write_back_position();
        let ok = match device {
            Some(dev) => dev.read_sector(u64::from(self.current_lba), &mut self.buffer),
            None => false,
        };
        if ok {
            self.buffer_pos = 0;
            // A fresh block is ready: this is the per-sector interrupt
            // a multi-sector READ SECTORS raises.
            self.intreq |= GAYLE_IRQ_IDE;
        } else {
            self.fail_transfer(error::IDNF);
        }
    }

    /// A write block has just been fully filled by the guest: commit it
    /// to the device, then move on or complete.
    fn finish_write_block(&mut self, device: Option<&mut dyn BlockDevice>) {
        let ok = match device {
            Some(dev) => dev.write_sector(u64::from(self.current_lba), &self.buffer),
            None => false,
        };
        if !ok {
            self.fail_transfer(error::IDNF);
            return;
        }
        self.sectors_left = self.sectors_left.saturating_sub(1);
        if self.sectors_left == 0 {
            self.status &= !(status::DRQ | status::BSY);
            // Unlike the first block of a PIO-out command (see
            // `do_write_sectors`), every committed block -- including
            // the last -- raises the interrupt: it is what tells the
            // host the drive has accepted the data.
            self.intreq |= GAYLE_IRQ_IDE;
            return;
        }
        self.current_lba = self.current_lba.wrapping_add(1);
        self.write_back_position();
        self.buffer_pos = 0;
        self.intreq |= GAYLE_IRQ_IDE;
    }

    fn fail_transfer(&mut self, code: u8) {
        self.status = status::DRDY | status::DSC | status::ERR;
        self.error = code;
        self.sectors_left = 0;
        self.intreq |= GAYLE_IRQ_IDE;
    }

    // ---- commands ----------------------------------------------------

    fn command(&mut self, cmd: u8, device: Option<&mut dyn BlockDevice>) {
        // No selected device at all: real hardware simply never answers
        // (see `read_status`'s empty-cable case), so there is nothing
        // for a command write to do.
        let Some(dev) = device else { return };
        self.status &= !status::ERR;
        self.error = 0;
        match cmd {
            cmd::IDENTIFY => self.do_identify(dev),
            cmd::READ_SECTORS | cmd::READ_SECTORS_NR => self.do_read_sectors(dev),
            cmd::WRITE_SECTORS | cmd::WRITE_SECTORS_NR => self.do_write_sectors(dev),
            cmd::INIT_PARAMS => self.do_init_params(),
            cmd::SET_FEATURES => self.do_set_features(),
            // Anything else aborts rather than being silently ignored --
            // a dropped command is much harder to diagnose than a clean
            // `ERR`/`ABRT`. Real hardware does the same, and the
            // interrupt still fires: an aborted command still completes.
            _ => self.fail_transfer(error::ABRT),
        }
    }

    fn do_identify(&mut self, dev: &mut dyn BlockDevice) {
        self.buffer = identify_block(dev, self.heads, self.spt);
        self.buffer_pos = 0;
        self.sectors_left = 1;
        self.writing = false;
        self.status = status::DRDY | status::DSC | status::DRQ;
        self.intreq |= GAYLE_IRQ_IDE;
    }

    fn do_read_sectors(&mut self, dev: &mut dyn BlockDevice) {
        let Some(lba) = self.requested_lba() else {
            self.fail_transfer(error::IDNF);
            return;
        };
        let count = self.requested_count();
        if u64::from(lba) + u64::from(count) > dev.sector_count() {
            self.fail_transfer(error::IDNF);
            return;
        }
        if !dev.read_sector(u64::from(lba), &mut self.buffer) {
            self.fail_transfer(error::IDNF);
            return;
        }
        self.current_lba = lba;
        self.sectors_left = count;
        self.writing = false;
        self.buffer_pos = 0;
        self.status = status::DRDY | status::DSC | status::DRQ;
        // The first block is ready immediately; this is a PIO-in
        // command, so the host is expected to wait for the interrupt
        // before reading.
        self.intreq |= GAYLE_IRQ_IDE;
    }

    fn do_write_sectors(&mut self, dev: &mut dyn BlockDevice) {
        let Some(lba) = self.requested_lba() else {
            self.fail_transfer(error::IDNF);
            return;
        };
        let count = self.requested_count();
        if u64::from(lba) + u64::from(count) > dev.sector_count() {
            self.fail_transfer(error::IDNF);
            return;
        }
        self.current_lba = lba;
        self.sectors_left = count;
        self.writing = true;
        self.buffer = [0; SECTOR_BYTES];
        self.buffer_pos = 0;
        self.status = status::DRDY | status::DSC | status::DRQ;
        // PIO-out: no interrupt for the first block -- the host starts
        // writing as soon as it sees DRQ, without waiting for one.
    }

    fn do_init_params(&mut self) {
        // INITIALIZE DEVICE PARAMETERS: heads come from the select
        // register (0-based, so +1), sectors/track from nsector.
        self.heads = (self.select & SELECT_HEAD_MASK) + 1;
        self.spt = if self.nsector == 0 { 1 } else { self.nsector };
        self.status = status::DRDY | status::DSC;
        self.intreq |= GAYLE_IRQ_IDE;
    }

    fn do_set_features(&mut self) {
        // No feature sub-codes are meaningfully modelled; every request
        // trivially succeeds; a real drive completes it immediately too.
        self.status = status::DRDY | status::DSC;
        self.intreq |= GAYLE_IRQ_IDE;
    }

    // ---- addressing ----------------------------------------------------

    /// The LBA a READ/WRITE SECTORS command targets, from the current
    /// task-file registers: LBA28 if `select` bit 6 is set, else CHS
    /// translated through the current `heads`/`spt` geometry. `None` for
    /// an invalid CHS sector number (ATA sector numbers are 1-based; 0
    /// addresses nothing) -- guest-controlled input, so this is a clean
    /// error rather than a wrapping/underflowing computation.
    fn requested_lba(&self) -> Option<u32> {
        if self.select & SELECT_LBA != 0 {
            Some(
                ((u32::from(self.select) & u32::from(SELECT_HEAD_MASK)) << 24)
                    | (u32::from(self.hcyl) << 16)
                    | (u32::from(self.lcyl) << 8)
                    | u32::from(self.sector),
            )
        } else {
            if self.sector == 0 {
                return None;
            }
            let heads = u32::from(self.heads.max(1));
            let spt = u32::from(self.spt.max(1));
            let cyl = (u32::from(self.hcyl) << 8) | u32::from(self.lcyl);
            let head = u32::from(self.select) & u32::from(SELECT_HEAD_MASK);
            Some((cyl * heads + head) * spt + (u32::from(self.sector) - 1))
        }
    }

    /// `nsector == 0` means 256 sectors, the standard ATA convention for
    /// an 8-bit sector count register.
    fn requested_count(&self) -> u16 {
        if self.nsector == 0 {
            256
        } else {
            u16::from(self.nsector)
        }
    }

    /// Mirror `current_lba` back into the task-file registers, in
    /// whichever addressing mode is currently selected -- a real drive
    /// leaves its position visible after each block of a multi-sector
    /// transfer, the same way it accepted the request.
    fn write_back_position(&mut self) {
        if self.select & SELECT_LBA != 0 {
            self.sector = self.current_lba as u8;
            self.lcyl = (self.current_lba >> 8) as u8;
            self.hcyl = (self.current_lba >> 16) as u8;
            self.select = (self.select & !SELECT_HEAD_MASK)
                | ((self.current_lba >> 24) as u8 & SELECT_HEAD_MASK);
        } else {
            let heads = u32::from(self.heads.max(1));
            let spt = u32::from(self.spt.max(1));
            let total_heads = self.current_lba / spt;
            let sector_num = (self.current_lba % spt) + 1;
            let head = total_heads % heads;
            let cyl = total_heads / heads;
            self.sector = sector_num as u8;
            self.lcyl = cyl as u8;
            self.hcyl = (cyl >> 8) as u8;
            self.select = (self.select & !SELECT_HEAD_MASK) | (head as u8 & SELECT_HEAD_MASK);
        }
    }
}

/// Store a 16-bit IDENTIFY field at word index `idx`, low byte first.
///
/// This is the opposite of the natural big-endian pair the data-register
/// read composes (`buffer[pos] << 8 | buffer[pos + 1]`, see
/// `Gayle::data_read_hi`/`data_read_lo`), and that mismatch is
/// deliberate: real Gayle physically swaps the drive's D7-D0 onto the
/// CPU's D15-D8, so every *structured* ATA word (as opposed to opaque
/// sector bytes) arrives at the CPU with its bytes exchanged relative to
/// the value the ATA spec defines. Storing the field low-byte-first here
/// and then reading it out through the ordinary hi/lo register path
/// reproduces exactly that swap without needing any separate swapping
/// logic in the data path itself. `scsi.device`'s IDENTIFY parser
/// depends on seeing that same swapped layout (its word/string helpers
/// un-swap on the way in), which is why getting this backwards would
/// make Kickstart misread every geometry and capacity field even though
/// the ID gate and the task file were otherwise correct.
fn identify_word(buf: &mut [u8; SECTOR_BYTES], idx: usize, val: u16) {
    buf[idx * 2] = val as u8;
    buf[idx * 2 + 1] = (val >> 8) as u8;
}

/// Store an ATA model/serial/firmware string field: ASCII, space-padded,
/// with each character pair byte-swapped the same way `identify_word`
/// swaps numeric fields (ATA's string convention already calls for the
/// first character of a pair to land in the high byte of its word; on
/// top of that, Gayle's hardware swap has to be reproduced the same way
/// it is for numeric fields). `len_words` is the field length in ATA
/// words (2 characters each); `text` longer than that is truncated,
/// shorter is space-padded -- there is no allocator here to build an
/// exact-length string first.
fn identify_string(buf: &mut [u8; SECTOR_BYTES], start_word: usize, len_words: usize, text: &str) {
    let src = text.as_bytes();
    for i in 0..len_words {
        let c0 = src.get(i * 2).copied().unwrap_or(b' ');
        let c1 = src.get(i * 2 + 1).copied().unwrap_or(b' ');
        buf[(start_word + i) * 2] = c1;
        buf[(start_word + i) * 2 + 1] = c0;
    }
}

/// Build a 256-word IDENTIFY DEVICE response: geometry, LBA capacity
/// (from [`BlockDevice::sector_count`]), a model string, and the
/// LBA-supported bit -- everything `scsi.device` reads to decide the
/// drive exists and to compute its size. Doubleword I/O (word 48) is
/// deliberately left clear: this interface only ever answers 16-bit
/// word accesses to the data register (see `Gayle::data_read_hi`'s doc
/// comment), and advertising 32-bit transfers would invite the driver to
/// use `move.l`, which this register map does not support.
fn identify_block(dev: &mut dyn BlockDevice, heads: u8, spt: u8) -> [u8; SECTOR_BYTES] {
    let mut buf = [0u8; SECTOR_BYTES];

    let heads = heads.max(1);
    let spt = spt.max(1);
    let sector_count = dev.sector_count();
    let cylinders = (sector_count / (u64::from(heads) * u64::from(spt)))
        .max(1)
        .min(u64::from(u16::MAX)) as u16;
    let lba_total = sector_count.min(u64::from(u32::MAX)) as u32;
    let current_capacity = u32::from(cylinders) * u32::from(heads) * u32::from(spt);

    identify_word(&mut buf, 0, 0x0040); // fixed, non-removable ATA device
    identify_word(&mut buf, 1, cylinders);
    identify_word(&mut buf, 3, u16::from(heads));
    identify_word(&mut buf, 4, u16::from(spt) * SECTOR_BYTES as u16); // unformatted bytes/track
    identify_word(&mut buf, 5, SECTOR_BYTES as u16); // unformatted bytes/sector
    identify_word(&mut buf, 6, u16::from(spt));
    identify_word(&mut buf, 47, 0x8000); // READ/WRITE MULTIPLE not supported
    identify_word(&mut buf, 49, 0x0200); // bit 9: LBA supported
    identify_word(&mut buf, 53, 0x0001); // words 54-58 are valid
    identify_word(&mut buf, 54, cylinders);
    identify_word(&mut buf, 55, u16::from(heads));
    identify_word(&mut buf, 56, u16::from(spt));
    identify_word(&mut buf, 57, current_capacity as u16);
    identify_word(&mut buf, 58, (current_capacity >> 16) as u16);
    identify_word(&mut buf, 60, lba_total as u16);
    identify_word(&mut buf, 61, (lba_total >> 16) as u16);

    identify_string(&mut buf, 10, 10, "M68KMACHINE000000000"); // serial number
    identify_string(&mut buf, 23, 4, "1.0"); // firmware revision
    identify_string(&mut buf, 27, 20, "m68k Machine Gayle IDE Disk"); // model

    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- an in-memory `BlockDevice` for testing -----------------------

    struct MemDisk {
        sectors: std::vec::Vec<[u8; SECTOR_BYTES]>,
    }

    impl MemDisk {
        fn new(count: usize) -> Self {
            Self {
                sectors: std::vec![[0u8; SECTOR_BYTES]; count],
            }
        }
    }

    impl BlockDevice for MemDisk {
        fn sector_count(&self) -> u64 {
            self.sectors.len() as u64
        }

        fn read_sector(&mut self, lba: u64, buf: &mut [u8; SECTOR_BYTES]) -> bool {
            match self.sectors.get(lba as usize) {
                Some(s) => {
                    *buf = *s;
                    true
                }
                None => false,
            }
        }

        fn write_sector(&mut self, lba: u64, buf: &[u8; SECTOR_BYTES]) -> bool {
            match self.sectors.get_mut(lba as usize) {
                Some(s) => {
                    *s = *buf;
                    true
                }
                None => false,
            }
        }
    }

    fn read_word(g: &mut Gayle, disk: &mut MemDisk) -> u16 {
        let hi = g.read(reg::IDE_DATA, Some(&mut *disk)) as u16;
        let lo = g.read(reg::IDE_DATA + 1, Some(&mut *disk)) as u16;
        (hi << 8) | lo
    }

    fn write_word(g: &mut Gayle, disk: &mut MemDisk, value: u16) {
        g.write(reg::IDE_DATA, (value >> 8) as u8, Some(&mut *disk));
        g.write(reg::IDE_DATA + 1, value as u8, Some(&mut *disk));
    }

    fn set_lba(g: &mut Gayle, disk: &mut MemDisk, lba: u32, count: u8) {
        g.write(
            reg::IDE_SELECT,
            SELECT_LBA | ((lba >> 24) as u8 & SELECT_HEAD_MASK),
            Some(&mut *disk),
        );
        g.write(reg::IDE_HCYL, (lba >> 16) as u8, Some(&mut *disk));
        g.write(reg::IDE_LCYL, (lba >> 8) as u8, Some(&mut *disk));
        g.write(reg::IDE_SECTOR, lba as u8, Some(&mut *disk));
        g.write(reg::IDE_NSECTOR, count, Some(&mut *disk));
    }

    fn read_status(g: &mut Gayle, disk: &mut MemDisk) -> u8 {
        g.read(reg::IDE_STATUS, Some(&mut *disk))
    }

    // ---- existing coverage, preserved --------------------------------

    #[test]
    fn responds_to_its_two_windows_only() {
        assert!(Gayle::responds_to(GAYLE_BASE));
        assert!(Gayle::responds_to(reg::IDE_STATUS));
        assert!(Gayle::responds_to(GAYLE_ID_BASE));
        assert!(
            !Gayle::responds_to(0x00DF_F000),
            "custom chips are not Gayle"
        );
        assert!(!Gayle::responds_to(0x00BF_E001), "CIA-A is not Gayle");
    }

    #[test]
    fn reports_ready_out_of_reset() {
        let g = Gayle::new();
        assert_eq!(g.status & status::DRDY, status::DRDY);
        assert_eq!(g.status & status::BSY, 0, "not busy with nothing to do");
    }

    #[test]
    fn no_interrupt_until_both_request_and_enable() {
        let mut g = Gayle::new();
        g.intreq = GAYLE_IRQ_IDE;
        assert!(!g.irq_pending(), "request alone is not enough");
        g.intena = GAYLE_IRQ_IDE;
        assert!(g.irq_pending());
    }

    // ---- the ID gate ---------------------------------------------------

    #[test]
    fn id_shifts_out_msb_first_and_a_write_restarts_it() {
        let mut g = Gayle::new();
        g.write(GAYLE_ID_BASE, 0xFF, None); // any write resets the shifter
        let bits: std::vec::Vec<u8> = (0..8).map(|_| g.read(GAYLE_ID_BASE, None)).collect();
        // GAYLE_ID = $D1 = 1101_0001, the A1200's gate array (the
        // A600's is $D0 -- see GAYLE_ID's doc comment).
        assert_eq!(bits, [0x80, 0x80, 0x00, 0x80, 0x00, 0x00, 0x00, 0x80]);

        // A fresh write restarts the sequence mid-stream.
        g.write(GAYLE_ID_BASE, 0x00, None);
        assert_eq!(g.read(GAYLE_ID_BASE, None), 0x80);
    }

    #[test]
    fn id_gate_answers_even_with_no_disk_attached() {
        // Gayle the gate array is always present on a real A1200
        // regardless of whether a drive is on the cable -- only the
        // task file behaves as an empty cable (see below).
        let mut g = Gayle::new();
        g.write(GAYLE_ID_BASE, 0, None);
        assert_eq!(g.read(GAYLE_ID_BASE, None), 0x80, "bit 7 of $D1 is 1");
    }

    // ---- IDENTIFY --------------------------------------------------------

    #[test]
    fn identify_reports_geometry_capacity_lba_and_model() {
        let mut g = Gayle::new();
        let mut disk = MemDisk::new(16 * 63 * 4); // 4 cylinders at the default geometry

        g.write(reg::IDE_SELECT, 0xA0, Some(&mut disk));
        g.write(reg::IDE_STATUS, cmd::IDENTIFY, Some(&mut disk));
        assert_eq!(
            read_status(&mut g, &mut disk),
            status::DRDY | status::DSC | status::DRQ
        );

        let mut words = [0u16; 256];
        for w in &mut words {
            // The data-register read is the CPU-visible, hardware-
            // swapped word; un-swap to recover the ATA-defined value,
            // the same way `scsi.device`'s own word helper does.
            *w = read_word(&mut g, &mut disk).swap_bytes();
        }

        assert_eq!(words[1], 4, "cylinders");
        assert_eq!(words[3], 16, "heads");
        assert_eq!(words[6], 63, "sectors per track");
        assert_ne!(words[49] & 0x0200, 0, "LBA capability bit");
        let lba = u32::from(words[60]) | (u32::from(words[61]) << 16);
        assert_eq!(
            lba,
            16 * 63 * 4,
            "LBA capacity from BlockDevice::sector_count"
        );
        // ATA string convention: the first character of each pair lands
        // in the high byte, so a correctly un-swapped word reads as two
        // characters in natural reading order.
        assert_eq!(words[27].to_be_bytes(), *b"m6");

        assert_eq!(
            read_status(&mut g, &mut disk),
            status::DRDY | status::DSC,
            "DRQ clears once the block is fully drained"
        );
    }

    // ---- sector transfers ------------------------------------------------

    #[test]
    fn single_sector_write_then_read_round_trips() {
        let mut g = Gayle::new();
        let mut disk = MemDisk::new(64);
        g.write(reg::GAYLE_INTENA, GAYLE_IRQ_IDE, Some(&mut disk));

        set_lba(&mut g, &mut disk, 5, 1);
        g.write(reg::IDE_STATUS, cmd::WRITE_SECTORS, Some(&mut disk));
        assert!(!g.irq_pending(), "no interrupt before the first block");
        for i in 0..256u32 {
            write_word(&mut g, &mut disk, (i * 3) as u16);
        }
        assert_eq!(g.status, status::DRDY | status::DSC);
        assert!(g.irq_pending(), "the committed block raises the interrupt");
        let _ = read_status(&mut g, &mut disk); // acknowledges it
        assert!(!g.irq_pending(), "status read acknowledges the interrupt");

        set_lba(&mut g, &mut disk, 5, 1);
        g.write(reg::IDE_STATUS, cmd::READ_SECTORS, Some(&mut disk));
        assert!(g.irq_pending(), "data ready raises the interrupt");
        for i in 0..256u32 {
            assert_eq!(read_word(&mut g, &mut disk), (i * 3) as u16, "word {i}");
        }
        assert_eq!(read_status(&mut g, &mut disk), status::DRDY | status::DSC);
    }

    #[test]
    fn multi_sector_read_and_write_advance_across_sectors() {
        let mut g = Gayle::new();
        let mut disk = MemDisk::new(64);

        set_lba(&mut g, &mut disk, 10, 3);
        g.write(reg::IDE_STATUS, cmd::WRITE_SECTORS, Some(&mut disk));
        for sector in 0u32..3 {
            for word in 0u32..256 {
                write_word(&mut g, &mut disk, (sector * 1000 + word) as u16);
            }
        }
        assert_eq!(read_status(&mut g, &mut disk), status::DRDY | status::DSC);

        set_lba(&mut g, &mut disk, 10, 3);
        g.write(reg::IDE_STATUS, cmd::READ_SECTORS, Some(&mut disk));
        for sector in 0u32..3 {
            for word in 0u32..256 {
                let got = read_word(&mut g, &mut disk);
                assert_eq!(
                    got,
                    (sector * 1000 + word) as u16,
                    "sector {sector} word {word}"
                );
            }
        }
        assert_eq!(read_status(&mut g, &mut disk), status::DRDY | status::DSC);

        // Bytes really landed at the right sectors in the backing store.
        let mut check = [0u8; SECTOR_BYTES];
        disk.read_sector(11, &mut check);
        assert_eq!(u16::from_be_bytes([check[0], check[1]]), 1000);
    }

    #[test]
    fn lba_and_chs_addressing_agree_on_the_same_sector() {
        let mut g = Gayle::new();
        let mut disk = MemDisk::new(64);

        // INITIALIZE DEVICE PARAMETERS: 2 heads, 8 sectors/track.
        g.write(reg::IDE_SELECT, 1, Some(&mut disk)); // heads - 1
        g.write(reg::IDE_NSECTOR, 8, Some(&mut disk));
        g.write(reg::IDE_STATUS, cmd::INIT_PARAMS, Some(&mut disk));
        assert_eq!(read_status(&mut g, &mut disk), status::DRDY | status::DSC);

        // C/H/S = 1/1/3 -> LBA (1*2 + 1)*8 + (3-1) = 26.
        g.write(reg::IDE_SELECT, 1, Some(&mut disk)); // head 1, CHS mode
        g.write(reg::IDE_HCYL, 0, Some(&mut disk));
        g.write(reg::IDE_LCYL, 1, Some(&mut disk));
        g.write(reg::IDE_SECTOR, 3, Some(&mut disk));
        g.write(reg::IDE_NSECTOR, 1, Some(&mut disk));
        g.write(reg::IDE_STATUS, cmd::WRITE_SECTORS, Some(&mut disk));
        for word in 0u32..256 {
            write_word(&mut g, &mut disk, word as u16);
        }
        assert_eq!(read_status(&mut g, &mut disk), status::DRDY | status::DSC);

        // The same sector read back via LBA28.
        set_lba(&mut g, &mut disk, 26, 1);
        g.write(reg::IDE_STATUS, cmd::READ_SECTORS, Some(&mut disk));
        for word in 0u32..256 {
            assert_eq!(read_word(&mut g, &mut disk), word as u16, "word {word}");
        }
    }

    // ---- error handling --------------------------------------------------

    #[test]
    fn out_of_range_lba_errors_cleanly_without_touching_the_device() {
        let mut g = Gayle::new();
        let mut disk = MemDisk::new(4);

        set_lba(&mut g, &mut disk, 100, 1);
        g.write(reg::IDE_STATUS, cmd::READ_SECTORS, Some(&mut disk));
        let status = read_status(&mut g, &mut disk);
        assert_eq!(status & status::ERR, status::ERR);
        assert_eq!(status & status::DRQ, 0, "no data phase for a rejected read");
        assert_eq!(g.error & error::IDNF, error::IDNF);
    }

    #[test]
    fn a_multi_sector_read_that_would_run_past_the_end_is_rejected_up_front() {
        // 2 sectors requested from a 3-sector disk starting at sector 2:
        // the second sector (index 3) does not exist. Rejected before
        // any data phase begins, exactly like a request that is out of
        // range outright.
        let mut g = Gayle::new();
        let mut disk = MemDisk::new(3);
        set_lba(&mut g, &mut disk, 2, 2);
        g.write(reg::IDE_STATUS, cmd::READ_SECTORS, Some(&mut disk));
        assert_eq!(g.error & error::IDNF, error::IDNF);
        let status = read_status(&mut g, &mut disk);
        assert_eq!(status & status::ERR, status::ERR);
        assert_eq!(status & status::DRQ, 0);
    }

    #[test]
    fn unknown_command_sets_err_and_aborts() {
        let mut g = Gayle::new();
        let mut disk = MemDisk::new(4);
        g.intena = GAYLE_IRQ_IDE;
        g.write(reg::IDE_STATUS, 0x42, Some(&mut disk)); // not a command we know
        assert!(g.irq_pending(), "an aborted command still completes");
        let status = read_status(&mut g, &mut disk);
        assert_eq!(status & status::ERR, status::ERR);
        assert_eq!(g.error & error::ABRT, error::ABRT);
    }

    // ---- interrupts --------------------------------------------------

    #[test]
    fn read_command_raises_the_interrupt_and_status_read_acknowledges_it() {
        let mut g = Gayle::new();
        let mut disk = MemDisk::new(4);
        g.intena = GAYLE_IRQ_IDE;
        set_lba(&mut g, &mut disk, 0, 1);
        g.write(reg::IDE_STATUS, cmd::READ_SECTORS, Some(&mut disk));
        assert!(g.irq_pending());
        let _ = g.read(reg::IDE_STATUS, Some(&mut disk));
        assert!(!g.irq_pending(), "reading status acknowledges it");
    }

    // ---- no disk attached: must not regress today's boot behaviour ----

    #[test]
    fn no_device_status_floats_and_commands_are_ignored() {
        let mut g = Gayle::new();
        assert_eq!(
            g.read(reg::IDE_STATUS, None),
            0xFF,
            "empty cable floats, matching real hardware and WinUAE"
        );
        assert_eq!(g.read(reg::IDE_ERROR, None), 0, "non-status regs read 0");

        g.write(reg::IDE_STATUS, cmd::IDENTIFY, None);
        assert_eq!(
            g.read(reg::IDE_STATUS, None),
            0xFF,
            "a command write with nothing attached does nothing"
        );
        assert!(!g.irq_pending(), "and never raises an interrupt");
    }
}
