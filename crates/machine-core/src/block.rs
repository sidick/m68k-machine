//! The block-device seam shared by every storage device this machine has
//! presented: [`SECTOR_BYTES`] and the [`BlockDevice`] trait a device's
//! register model reads and writes sectors through.
//!
//! This lived in `gayle.rs` originally -- Gayle IDE was the machine's
//! first storage device, and this trait was its handover to the host,
//! not a general-purpose abstraction designed up front. It moved here
//! once `hostblk` and `mirage` both needed it too, and it now outlives
//! Gayle itself: `hostblk` retired Gayle (`docs/device-ledger.md`), but
//! `hostblk` and `mirage` both still implement their storage against
//! this same trait, and `machine-hosted`'s `FileBlockDevice` still works
//! unchanged behind either of them.

/// Bytes in one sector. LBA28 throughout: 128 GB is far beyond anything
/// this machine will present, and the OS-side limit is lower still.
pub const SECTOR_BYTES: usize = 512;

/// The backing store behind a block device's register interface.
///
/// `machine-core` has no allocator and no file I/O, so the host supplies
/// the storage: a file-backed image in the hosted runner, an embedded
/// image on a bare-metal board layer. Returning `false` from either
/// method surfaces to the guest as a device error rather than a panic --
/// the guest controls the LBA and must not be able to fault the machine.
pub trait BlockDevice {
    /// Total addressable sectors.
    fn sector_count(&self) -> u64;

    /// Read one sector. `false` reports a device error to the guest.
    fn read_sector(&mut self, lba: u64, buf: &mut [u8; SECTOR_BYTES]) -> bool;

    /// Write one sector. `false` reports a device error to the guest.
    fn write_sector(&mut self, lba: u64, buf: &[u8; SECTOR_BYTES]) -> bool;
}
