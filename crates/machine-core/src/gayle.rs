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

/// Gayle's register file and the IDE task file behind it.
///
/// Holds no backing store of its own: [`Gayle::read`] and
/// [`Gayle::write`] take the [`BlockDevice`] from the caller, so the bus
/// can own the device and hand it down for the duration of an access.
// The private fields below are the state the register implementation
// needs and are unread until it lands, which clippy would otherwise
// reject. Remove this attribute along with the stubbed `read`/`write`.
#[allow(dead_code)]
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

    /// Sector buffer for the transfer in flight, and how far through it
    /// the guest has read or written.
    buffer: [u8; SECTOR_BYTES],
    buffer_pos: usize,
    /// Sectors still to transfer after the current one.
    sectors_left: u8,
    /// Whether the in-flight transfer is a write.
    writing: bool,
}

/// The ID an A1200's Gayle reports, shifted out one bit at a time from
/// the top. Kickstart uses this to decide whether an IDE interface is
/// present at all.
pub const GAYLE_ID: u8 = 0xD0;

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
            buffer: [0; SECTOR_BYTES],
            buffer_pos: 0,
            sectors_left: 0,
            writing: false,
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
    pub fn read(&mut self, _address: u32, _device: Option<&mut dyn BlockDevice>) -> u8 {
        // Implemented in the Gayle bring-up work: the bit-serial ID
        // protocol, the task file, and the PIO data path.
        crate::OPEN_BUS_BYTE
    }

    /// Write a Gayle or IDE register.
    pub fn write(&mut self, _address: u32, _value: u8, _device: Option<&mut dyn BlockDevice>) {
        // Implemented in the Gayle bring-up work, including command
        // execution on a write to the status/command register.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
