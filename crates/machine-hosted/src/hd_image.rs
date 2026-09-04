//! File-backed [`BlockDevice`] for Gayle's IDE port (`--hd <path>`).
//!
//! `machine-core` is `#![no_std]` and has no file I/O of its own --
//! `gayle.rs`'s `BlockDevice` trait doc comment says as much -- so this is
//! the hosted-only "disk" behind it: an ordinary host file, read and
//! written a sector at a time with `Seek`/`Read`/`Write`.
//!
//! Bytes are copied exactly as they sit in the file, in both directions,
//! with no transformation. That is a deliberate match to the Gayle
//! author's handover (`gayle.rs`'s `data_read_hi`/`data_read_lo` doc
//! comment): the CPU-visible byte swap is internal to how the data
//! register hands bytes to the guest, and cancels out for opaque sector
//! bytes, so a `.hdf` file's on-disk layout and this device's `buf`
//! argument are the same bytes in the same order.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use machine_core::gayle::{BlockDevice, SECTOR_BYTES};

/// A disk image backed by a host file.
pub struct FileBlockDevice {
    file: File,
    sector_count: u64,
    writable: bool,
}

impl FileBlockDevice {
    /// Open `path` as a block device.
    ///
    /// Read-only unless `writable` is set -- see `cli.rs`'s `--hd-writable`
    /// doc comment for the full reasoning. Short version: this interface
    /// is brand new and unproven end to end, the images worth attaching
    /// (an amibake conversion of licensed media, `docs/storage.md`) took
    /// real effort to build and are not redistributable if lost, and a
    /// boot that never gets to write a sector is still a fully useful
    /// milestone. Opt-in write access is one flag away when it is actually
    /// needed.
    ///
    /// `sector_count` is truncated down from the file's length -- a
    /// trailing partial sector (should one somehow exist) is simply never
    /// addressable, rather than read as part-garbage.
    pub fn open(path: &Path, writable: bool) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(writable).open(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            file,
            sector_count: len / SECTOR_BYTES as u64,
            writable,
        })
    }

    /// Whether this image was opened for writing.
    pub fn writable(&self) -> bool {
        self.writable
    }
}

impl BlockDevice for FileBlockDevice {
    fn sector_count(&self) -> u64 {
        self.sector_count
    }

    /// `false` (never a panic) for a seek/read failure or an
    /// out-of-range LBA -- `Gayle` turns that into a clean ATA `IDNF`
    /// rather than a host-side fault, exactly as `BlockDevice`'s trait
    /// doc comment specifies. The guest fully controls `lba`.
    fn read_sector(&mut self, lba: u64, buf: &mut [u8; SECTOR_BYTES]) -> bool {
        if lba >= self.sector_count {
            return false;
        }
        self.file
            .seek(SeekFrom::Start(lba * SECTOR_BYTES as u64))
            .and_then(|_| self.file.read_exact(buf))
            .is_ok()
    }

    /// `false` for a read-only image (see `open`'s doc comment), an
    /// out-of-range LBA, or a seek/write failure -- same clean-error
    /// contract as `read_sector`.
    fn write_sector(&mut self, lba: u64, buf: &[u8; SECTOR_BYTES]) -> bool {
        if !self.writable || lba >= self.sector_count {
            return false;
        }
        self.file
            .seek(SeekFrom::Start(lba * SECTOR_BYTES as u64))
            .and_then(|_| self.file.write_all(buf))
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh temp path per call. `cargo test`'s default threaded runner
    /// exercises this module's tests concurrently, and a first attempt at
    /// this helper named the file from the process ID plus a nanosecond
    /// timestamp alone -- close enough in practice for two tests starting
    /// within the same clock tick to collide on one path and corrupt each
    /// other's image mid-test. A process-local atomic counter makes each
    /// call's path unique regardless of timing.
    fn temp_image(bytes: &[u8]) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "machine-hosted-hd-image-test-{}-{n}.img",
            std::process::id(),
        ));
        let mut f = File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        path
    }

    #[test]
    fn sector_count_comes_from_file_length() {
        let path = temp_image(&vec![0u8; SECTOR_BYTES * 3]);
        let dev = FileBlockDevice::open(&path, false).unwrap();
        assert_eq!(dev.sector_count(), 3);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_trailing_partial_sector_is_not_addressable() {
        let path = temp_image(&vec![0u8; SECTOR_BYTES * 2 + 100]);
        let dev = FileBlockDevice::open(&path, false).unwrap();
        assert_eq!(dev.sector_count(), 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_only_by_default_refuses_writes() {
        let path = temp_image(&[0u8; SECTOR_BYTES]);
        let mut dev = FileBlockDevice::open(&path, false).unwrap();
        assert!(!dev.writable());
        assert!(!dev.write_sector(0, &[0xAAu8; SECTOR_BYTES]));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn writable_round_trips_a_sector() {
        let path = temp_image(&[0u8; SECTOR_BYTES]);
        let mut dev = FileBlockDevice::open(&path, true).unwrap();
        assert!(dev.writable());
        let pattern = [0x5Au8; SECTOR_BYTES];
        assert!(dev.write_sector(0, &pattern));
        let mut buf = [0u8; SECTOR_BYTES];
        assert!(dev.read_sector(0, &mut buf));
        assert_eq!(buf, pattern);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn out_of_range_lba_fails_cleanly() {
        let path = temp_image(&[0u8; SECTOR_BYTES]);
        let mut dev = FileBlockDevice::open(&path, true).unwrap();
        let mut buf = [0u8; SECTOR_BYTES];
        assert!(!dev.read_sector(1, &mut buf));
        assert!(!dev.write_sector(1, &buf));
        std::fs::remove_file(&path).ok();
    }
}
