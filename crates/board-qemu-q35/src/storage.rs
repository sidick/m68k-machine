//! UEFI Block I/O storage: find the disk `hostblk` should be handed, and
//! adapt it to `machine_core::block::BlockDevice`.
//!
//! **Why Block I/O, not virtio-blk:** `board-qemu-q35` is *the UEFI
//! board*, not the x86 board (see this crate's `main.rs` module doc
//! comment) -- it already builds and runs for `aarch64-unknown-uefi`
//! too, and `EFI_BLOCK_IO_PROTOCOL` is the one storage interface every
//! UEFI firmware this board might ever run under -- QEMU today, a Rock
//! 5B tomorrow (`edk2-rk3588`), any SystemReady board after that --
//! implements identically. virtio-blk would only ever serve QEMU.
//!
//! **Why a scan, not an index:** UEFI presents one `BlockIO` handle per
//! whole disk *and* one per logical partition on it, plus the ESP,
//! with nothing in the protocol itself distinguishing "the disk
//! `hostblk` should serve" from "some other block of bytes that
//! happens to answer read requests." This project has been bitten
//! repeatedly by silent wrong-thing-selected failures (`docs/
//! device-ledger.md`'s history), so [`find_rdb_disk`] identifies its
//! target positively -- by the Amiga Rigid Disk Block's own `'RDSK'`
//! signature, the same way `m68k/hostblk-rom/hostblk-diagrom.s`'s own
//! mounter finds an RDB once a unit is attached -- rather than by
//! handle order, size, or partition-type guesswork.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use machine_core::block::{BlockDevice, SECTOR_BYTES};
use uefi::boot::{self, ScopedProtocol, SearchType};
use uefi::proto::media::block::BlockIO;
use uefi::Handle;

/// How many leading 512-byte sectors to scan for the RDB `'RDSK'`
/// signature -- mirrors `hostblk-diagrom.s`'s own mounter
/// (`RDB_LOCATION_LIMIT`), which is itself the documented Amiga RDB
/// convention: an RDB lives somewhere in a disk's first 16 blocks, not
/// necessarily block 0.
const RDB_LOCATION_LIMIT: u64 = 16;

/// `'RDSK'`, the Rigid Disk Block signature, as it appears in the raw
/// bytes of an RDB block's first four bytes (big-endian ASCII --
/// `m68k/hostblk-rom/hostblk-diagrom.s`'s `IDNAME_RIGIDDISK`).
const RDSK_MAGIC: [u8; 4] = *b"RDSK";

/// A UEFI `EFI_BLOCK_IO_PROTOCOL` handle adapted to
/// `machine_core::block::BlockDevice`'s fixed 512-byte-sector contract.
///
/// `hostblk`'s wire protocol and `machine_core::block::SECTOR_BYTES`
/// are both hard-coded to 512 bytes (`docs/hostblk-protocol.md` §3);
/// that is *our* constraint, not an Amiga one -- the RDB itself and
/// later Kickstarts both cope with other native block sizes via
/// `rdb_BlockBytes`/`de_SizeBlock`. So this type is the adapter: it
/// reports whatever the media's own `block_size()` actually is
/// (logged by [`find_rdb_disk`] either way -- a mismatched size
/// working via translation is fine, one silently *not* working is the
/// failure mode worth avoiding), and translates 512-byte
/// `BlockDevice` calls onto the media's real granularity:
///
/// - native block size == 512: direct passthrough, most QEMU/OVMF
///   virtual media.
/// - native block size a multiple of 512 (e.g. 4096-byte NVMe/SSD
///   sectors): each `BlockDevice` sector is a slice of one native
///   block; writes are read-modify-write over the containing native
///   block.
/// - native block size a divisor of 512: each `BlockDevice` sector is
///   an exact whole number of contiguous native blocks -- no partial
///   bytes, so no read-modify-write is needed either direction.
///
/// A media whose block size is neither -- not a clean multiple or
/// divisor of 512 -- cannot be translated without behaving
/// differently on read vs. write boundaries, and [`EfiBlockDevice::new`]
/// refuses it rather than guessing.
pub struct EfiBlockDevice {
    io: ScopedProtocol<BlockIO>,
    media_id: u32,
    native_block_size: u64,
    /// How many 512-byte `BlockDevice` sectors live inside one native
    /// block. `1` when the native block is 512 bytes or smaller.
    sectors_per_native: u64,
    /// How many native blocks make up one 512-byte `BlockDevice`
    /// sector. `1` when the native block is 512 bytes or larger.
    natives_per_sector: u64,
    sector_count: u64,
    /// Scratch buffer sized to one native block, reused across calls --
    /// only ever allocated (and only ever needed) when
    /// `native_block_size > SECTOR_BYTES`, for the read-modify-write
    /// path.
    scratch: Vec<u8>,
}

impl EfiBlockDevice {
    /// Wrap `io` (already known to have media present) as a
    /// [`BlockDevice`], or explain in `Err` why its geometry can't be
    /// translated to 512-byte sectors.
    fn new(io: ScopedProtocol<BlockIO>) -> Result<Self, String> {
        let media = io.media();
        let media_id = media.media_id();
        let native_block_size = media.block_size() as u64;
        let last_block = media.last_block();
        let total_native_blocks = last_block + 1;
        let sector_bytes = SECTOR_BYTES as u64;

        if native_block_size == 0 {
            return Err(String::from("reports a block size of 0"));
        }

        let (sectors_per_native, natives_per_sector) = if native_block_size == sector_bytes {
            (1, 1)
        } else if native_block_size > sector_bytes && native_block_size.is_multiple_of(sector_bytes)
        {
            (native_block_size / sector_bytes, 1)
        } else if native_block_size < sector_bytes && sector_bytes.is_multiple_of(native_block_size)
        {
            (1, sector_bytes / native_block_size)
        } else {
            return Err(format!(
                "block size {native_block_size} is neither a clean multiple nor a clean \
                 divisor of {sector_bytes} -- refusing to guess how to translate it"
            ));
        };

        let sector_count = if natives_per_sector == 1 {
            total_native_blocks * sectors_per_native
        } else {
            total_native_blocks / natives_per_sector
        };

        let scratch = if native_block_size > sector_bytes {
            vec![0u8; native_block_size as usize]
        } else {
            Vec::new()
        };

        Ok(Self {
            io,
            media_id,
            native_block_size,
            sectors_per_native,
            natives_per_sector,
            sector_count,
            scratch,
        })
    }

    /// The media's own native block size, for diagnostics.
    pub fn native_block_size(&self) -> u64 {
        self.native_block_size
    }
}

impl BlockDevice for EfiBlockDevice {
    fn sector_count(&self) -> u64 {
        self.sector_count
    }

    fn read_sector(&mut self, lba: u64, buf: &mut [u8; SECTOR_BYTES]) -> bool {
        if lba >= self.sector_count {
            return false;
        }
        if self.natives_per_sector > 1 {
            // Native block smaller than 512 bytes: the sector is
            // exactly `natives_per_sector` contiguous native blocks,
            // read straight into `buf` -- no partial bytes to splice.
            let native_lba = lba * self.natives_per_sector;
            return self.io.read_blocks(self.media_id, native_lba, buf).is_ok();
        }
        if self.sectors_per_native == 1 {
            // Native block == 512 bytes: direct passthrough.
            return self.io.read_blocks(self.media_id, lba, buf).is_ok();
        }
        // Native block a multiple of 512 bytes: read the containing
        // native block and slice the requested 512 bytes out of it.
        let native_lba = lba / self.sectors_per_native;
        let offset = ((lba % self.sectors_per_native) * SECTOR_BYTES as u64) as usize;
        if self
            .io
            .read_blocks(self.media_id, native_lba, &mut self.scratch)
            .is_err()
        {
            return false;
        }
        buf.copy_from_slice(&self.scratch[offset..offset + SECTOR_BYTES]);
        true
    }

    fn write_sector(&mut self, lba: u64, buf: &[u8; SECTOR_BYTES]) -> bool {
        if lba >= self.sector_count {
            return false;
        }
        if self.natives_per_sector > 1 {
            let native_lba = lba * self.natives_per_sector;
            return self.io.write_blocks(self.media_id, native_lba, buf).is_ok();
        }
        if self.sectors_per_native == 1 {
            return self.io.write_blocks(self.media_id, lba, buf).is_ok();
        }
        // Read-modify-write: splice the 512 bytes into their place
        // inside the containing native block, then write the whole
        // native block back.
        let native_lba = lba / self.sectors_per_native;
        let offset = ((lba % self.sectors_per_native) * SECTOR_BYTES as u64) as usize;
        if self
            .io
            .read_blocks(self.media_id, native_lba, &mut self.scratch)
            .is_err()
        {
            return false;
        }
        self.scratch[offset..offset + SECTOR_BYTES].copy_from_slice(buf);
        self.io
            .write_blocks(self.media_id, native_lba, &self.scratch)
            .is_ok()
    }
}

/// Outcome of [`find_rdb_disk`]: the device, if a positively-identified
/// RDB disk was found, plus a human-readable log of every handle
/// checked and why it was accepted, skipped, or refused. The caller
/// (`main.rs`) prints this unconditionally, so a "found nothing" run
/// says exactly why rather than just booting on with no drive attached
/// and no explanation.
pub struct RdbScan {
    pub device: Option<EfiBlockDevice>,
    pub log: Vec<String>,
}

/// Scan every `EFI_BLOCK_IO_PROTOCOL` handle UEFI presents for the
/// first one carrying an Amiga Rigid Disk Block, and hand it back ready
/// to attach to `MachineBus::with_hostblk`.
///
/// Never guesses: a handle is accepted only on a positive `'RDSK'`
/// match in its first [`RDB_LOCATION_LIMIT`] 512-byte sectors, in
/// enumeration order, first match wins. Handles with no media, an
/// unopenable protocol, or an untranslatable block size are logged and
/// skipped rather than silently ignored. If nothing anywhere matches,
/// `device` comes back `None` and the log says so plainly -- the
/// caller boots on with no drive attached rather than serving
/// something arbitrary.
pub fn find_rdb_disk() -> RdbScan {
    let mut log = Vec::new();

    let handles: Vec<Handle> = match boot::locate_handle_buffer(SearchType::from_proto::<BlockIO>())
    {
        Ok(h) => h.iter().copied().collect(),
        Err(e) => {
            log.push(format!(
                "storage: no EFI_BLOCK_IO_PROTOCOL handles found at all ({e:?})"
            ));
            return RdbScan { device: None, log };
        }
    };

    for (index, handle) in handles.into_iter().enumerate() {
        let io = match boot::open_protocol_exclusive::<BlockIO>(handle) {
            Ok(io) => io,
            Err(e) => {
                log.push(format!(
                    "storage: handle {index}: could not open BlockIO ({e:?}) -- skipped"
                ));
                continue;
            }
        };

        let (media_present, is_partition, block_size) = {
            let media = io.media();
            (
                media.is_media_present(),
                media.is_logical_partition(),
                media.block_size(),
            )
        };
        let kind = if is_partition {
            "logical partition"
        } else {
            "whole disk"
        };

        if !media_present {
            log.push(format!(
                "storage: handle {index}: {kind}, no media present -- skipped"
            ));
            continue;
        }

        let mut device = match EfiBlockDevice::new(io) {
            Ok(dev) => dev,
            Err(reason) => {
                log.push(format!(
                    "storage: handle {index}: {kind}, block size {block_size} bytes -- {reason} -- skipped"
                ));
                continue;
            }
        };

        let scan_limit = RDB_LOCATION_LIMIT.min(device.sector_count());
        let mut found_at = None;
        let mut probe = [0u8; SECTOR_BYTES];
        for lba in 0..scan_limit {
            if !device.read_sector(lba, &mut probe) {
                continue;
            }
            if probe[0..4] == RDSK_MAGIC {
                found_at = Some(lba);
                break;
            }
        }

        match found_at {
            Some(lba) => {
                log.push(format!(
                    "storage: handle {index}: {kind}, native block size {} bytes, \
                     {} 512-byte sectors -- 'RDSK' signature found at sector {lba} -- \
                     serving this to hostblk",
                    device.native_block_size(),
                    device.sector_count(),
                ));
                return RdbScan {
                    device: Some(device),
                    log,
                };
            }
            None => {
                log.push(format!(
                    "storage: handle {index}: {kind}, native block size {} bytes, \
                     {} 512-byte sectors -- no 'RDSK' signature in the first {scan_limit} \
                     sectors -- not a candidate",
                    device.native_block_size(),
                    device.sector_count(),
                ));
            }
        }
    }

    log.push(String::from(
        "storage: no Amiga RDB disk found on any EFI_BLOCK_IO_PROTOCOL handle -- \
         continuing with no drive attached",
    ));
    RdbScan { device: None, log }
}
