//! `make-ffs-image` -- create a small, bare (RDB-less), writable `DOS\1`
//! (FFS) volume with a few known files, for `scripts/pktport-e2e.sh` to
//! serve behind `--pktvol`.
//!
//! `PktVolume::open` (`crates/machine-hosted/src/pktvol.rs`) mounts an
//! RDB-less image as a bare volume from block 0 (its own module docs:
//! "An image with no RDSK block at all is mounted as a bare volume" --
//! `amiga-rdb::RdbError::NoRdsk` is not a mount failure, it's how that
//! shape is detected), so a fresh formatted-and-populated image with no
//! partition table at all is the simplest possible thing this tool can
//! hand it. Built with `amiga-ffs`'s own `Populator` -- the same
//! bulk-build convenience `crates/machine-hosted/src/pktvol.rs`'s own
//! test suite uses to construct its fixture volumes.
//!
//! Usage:
//! ```sh
//! cargo run --release -p machine-hosted --example make-ffs-image -- \
//!     <output.hdf> [size-mb]
//! ```
//! `size-mb` defaults to 4.

use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::ExitCode;

use amiga_ffs::{populate::Populator, FormatOptions, Metadata, Variant};

const BLOCK_SIZE: usize = 512;

/// A `File`-backed medium sized to exactly `block_count` blocks up
/// front (`set_len`) -- `Populator`/`format` write every block of a
/// fresh volume, so there is no partial-image case to worry about here,
/// unlike `pktvol.rs`'s `FileMedium`, which also has to track an
/// existing image's already-partitioned extent.
struct FileMedium {
    file: File,
    block_count: u64,
}

impl amiga_ffs::BlockSource for FileMedium {
    type Error = io::Error;
    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }
    fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(lba * BLOCK_SIZE as u64))?;
        self.file.read_exact(buf)
    }
    fn block_count(&self) -> Option<u64> {
        Some(self.block_count)
    }
}

impl amiga_ffs::BlockSink for FileMedium {
    type Error = io::Error;
    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }
    fn write_block(&mut self, lba: u64, buf: &[u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(lba * BLOCK_SIZE as u64))?;
        self.file.write_all(buf)
    }
    fn block_count(&self) -> Option<u64> {
        Some(self.block_count)
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 || args.len() > 3 {
        return Err(format!(
            "usage: {} <output.hdf> [size-mb]",
            args.first().map(String::as_str).unwrap_or("make-ffs-image")
        ));
    }
    let out_path = Path::new(&args[1]);
    let size_mb: u64 = match args.get(2) {
        Some(s) => s.parse().map_err(|e| format!("bad size-mb {s:?}: {e}"))?,
        None => 4,
    };
    let block_count = (size_mb * 1024 * 1024) / BLOCK_SIZE as u64;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(out_path)
        .map_err(|e| format!("creating {}: {e}", out_path.display()))?;
    file.set_len(block_count * BLOCK_SIZE as u64)
        .map_err(|e| format!("sizing {}: {e}", out_path.display()))?;
    let medium = FileMedium { file, block_count };

    let opts = FormatOptions::new(Variant::Ffs, block_count, b"PktTest");
    let mut pop = Populator::new(medium, &opts)
        .map_err(|e| format!("formatting {}: {e}", out_path.display()))?;
    let root = pop.root_lba();

    pop.create_file(root, b"marker.txt", &Metadata::new(), b"pristine\n")
        .map_err(|e| format!("writing marker.txt: {e}"))?;
    let sub = pop
        .create_dir(root, b"sub", &Metadata::new())
        .map_err(|e| format!("creating sub: {e}"))?;
    pop.create_file(sub, b"nested.txt", &Metadata::new(), b"nested content\n")
        .map_err(|e| format!("writing sub/nested.txt: {e}"))?;

    pop.finish()
        .map_err(|e| format!("finishing {}: {e}", out_path.display()))?;

    println!(
        "created {} ({size_mb} MB, DOS\\1, bare/RDB-less) with marker.txt and sub/nested.txt",
        out_path.display()
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("make-ffs-image: {e}");
            ExitCode::FAILURE
        }
    }
}
