//! `hdf-install` -- a minimal dev tool to write one host file into an
//! `.hdf` disk image via `amiga-ffs`, creating any missing parent
//! directories along the way.
//!
//! Built for `scripts/pktport-e2e.sh` (docs/pktport-protocol.md's guest
//! stub proof): getting `L:pktport-handler` and a `DEVS:Mountlist`
//! fragment onto a scratch copy of the project's boot image without a
//! real Amiga (or a running emulator with a CLI) to copy them in by
//! hand. It is not a general-purpose Amiga disk tool -- `amitools`'
//! `xdftool`/`rdbtool` already cover that ground well; this exists only
//! because those work on ADF/plain partition images and this project
//! needs to write into the *filesystem inside* an already-RDB-partitioned
//! `.hdf`, which is exactly the seam `crates/machine-hosted/src/pktvol.rs`
//! already opens for the running machine. This tool duplicates a small
//! slice of that module's own image-opening logic (RDB parse with a
//! bare-volume fallback) rather than importing it: `machine-hosted` ships
//! only a `[[bin]]` target (no library crate), so a Cargo example cannot
//! reach another source file's private items, and standing up a `[lib]`
//! target just to share ~30 lines was judged not worth the churn for a
//! "keep it minimal" dev tool -- see this crate's Cargo.toml for the
//! current target list before assuming otherwise.
//!
//! Usage:
//! ```sh
//! cargo run --release -p machine-hosted --example hdf-install -- \
//!     <image.hdf> <host-file> <amiga-path>
//! ```
//!
//! `<amiga-path>` follows ordinary Amiga path syntax relative to the
//! image's one filesystem: an optional `NAME:` prefix names (and, if
//! missing, creates) a top-level directory -- the common case for this
//! project's own images, whose FFS root already holds `L/`, `DEVS/`,
//! `C/`, `S/`, `Libs/` etc, so `L:pktport-handler` means "the `L`
//! directory at the volume's root" and `DEVS:Mountlist.pktport` means
//! "the `DEVS` directory". Everything after the (optional) `NAME:` is
//! split on `/` as ordinary path components. A leaf that already exists
//! is deleted and recreated with the new content (a plain overwrite, not
//! an in-place truncate/rewrite -- simplest correct behaviour for a tool
//! whose whole job is "make the guest see this file", run at most a
//! handful of times per test setup).

use std::env;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::ExitCode;

use amiga_ffs::{Metadata, Mutator, Variant, Volume, DEFAULT_RESERVED};
use amiga_rdb::{Rdb, RdbError};

/// Same shape as `crates/machine-hosted/src/pktvol.rs`'s own `FileMedium`
/// (see this file's module docs for why it is not imported instead): a
/// `File`-backed medium at a fixed 512-byte device block size, reading
/// and writing at a `base_lba` offset set once after the RDB (or its
/// absence) is known.
struct FileMedium {
    file: File,
    block_size: usize,
    base_lba: u64,
    len_blocks: Option<u64>,
}

impl FileMedium {
    fn abs(&self, lba: u64) -> io::Result<u64> {
        if let Some(len) = self.len_blocks {
            if lba >= len {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "lba out of range",
                ));
            }
        }
        self.base_lba
            .checked_add(lba)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "lba overflow"))
    }

    fn seek_to(&mut self, lba: u64) -> io::Result<()> {
        let abs = self.abs(lba)?;
        let byte_off = abs
            .checked_mul(self.block_size as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "byte offset overflow"))?;
        self.file.seek(SeekFrom::Start(byte_off))?;
        Ok(())
    }
}

impl amiga_rdb::BlockSource for FileMedium {
    type Error = io::Error;
    fn block_size(&self) -> usize {
        self.block_size
    }
    fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        self.seek_to(lba)?;
        self.file.read_exact(buf)
    }
    fn block_count(&self) -> Option<u64> {
        self.len_blocks
    }
}

impl amiga_ffs::BlockSource for FileMedium {
    type Error = io::Error;
    fn block_size(&self) -> usize {
        self.block_size
    }
    fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        self.seek_to(lba)?;
        self.file.read_exact(buf)
    }
    fn block_count(&self) -> Option<u64> {
        self.len_blocks
    }
}

impl amiga_ffs::BlockSink for FileMedium {
    type Error = io::Error;
    fn block_size(&self) -> usize {
        self.block_size
    }
    fn write_block(&mut self, lba: u64, buf: &[u8]) -> io::Result<()> {
        self.seek_to(lba)?;
        self.file.write_all(buf)
    }
    fn block_count(&self) -> Option<u64> {
        self.len_blocks
    }
}

/// Open `path` writable and mount its first `amiga-ffs`-recognised
/// (`DOS\0`-`DOS\7`) partition, or fall back to a bare RDB-less volume
/// from block 0 -- the same fallback `PktVolume::open` documents and
/// relies on (its own module docs: `amiga-rdb::RdbError::NoRdsk` is not a
/// mount failure, it is how a bare FFS/OFS volume is detected).
fn open_writable(path: &Path) -> Result<Mutator<FileMedium>, String> {
    const BLOCK_SIZE: usize = 512;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    let total_blocks = file
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len()
        / BLOCK_SIZE as u64;
    let mut medium = FileMedium {
        file,
        block_size: BLOCK_SIZE,
        base_lba: 0,
        len_blocks: Some(total_blocks),
    };

    let (base_lba, len_blocks, expect, reserved) = match Rdb::parse(&mut medium) {
        Ok(rdb) => {
            let chosen = rdb
                .partitions
                .iter()
                .find(|p| Variant::from_dostype(p.dos_type).is_some())
                .ok_or_else(|| "no DOS\\0-DOS\\7 partition found in the RDB".to_string())?;
            // envec longword index 6 is de_Reserved (devices/hardblocks.h's
            // DosEnvec) -- same read pktvol.rs's own open() does, and the
            // same DEFAULT_RESERVED fallback for a short envec table.
            let reserved = chosen
                .envec_raw
                .get(6)
                .map(|&r| r as u64)
                .unwrap_or(DEFAULT_RESERVED);
            (
                chosen.start_lba,
                chosen.block_len,
                Variant::from_dostype(chosen.dos_type),
                reserved,
            )
        }
        Err(RdbError::NoRdsk) => (0, total_blocks, None, DEFAULT_RESERVED),
        Err(e) => return Err(format!("parsing RDB in {}: {e}", path.display())),
    };

    medium.base_lba = base_lba;
    medium.len_blocks = Some(len_blocks);

    let vol = Volume::open_with(medium, expect, len_blocks, reserved)
        .map_err(|e| format!("opening filesystem in {}: {e}", path.display()))?;
    Mutator::open(vol).map_err(|e| format!("opening {} for writing: {e}", path.display()))
}

/// Split an Amiga path into ordinary components, per this file's module
/// docs: an optional `NAME:` prefix becomes the first component, the
/// remainder is split on `/`.
fn split_amiga_path(path: &str) -> Vec<Vec<u8>> {
    let (prefix, rest) = match path.split_once(':') {
        Some((p, r)) => (Some(p), r),
        None => (None, path),
    };
    let mut parts: Vec<Vec<u8>> = Vec::new();
    if let Some(p) = prefix {
        if !p.is_empty() {
            parts.push(p.as_bytes().to_vec());
        }
    }
    for comp in rest.split('/') {
        if !comp.is_empty() {
            parts.push(comp.as_bytes().to_vec());
        }
    }
    parts
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 4 {
        return Err(format!(
            "usage: {} <image.hdf> <host-file> <amiga-path>",
            args.first().map(String::as_str).unwrap_or("hdf-install")
        ));
    }
    let image_path = Path::new(&args[1]);
    let host_file_path = Path::new(&args[2]);
    let amiga_path = &args[3];

    let data = fs::read(host_file_path)
        .map_err(|e| format!("reading {}: {e}", host_file_path.display()))?;

    let mut components = split_amiga_path(amiga_path);
    let leaf = components
        .pop()
        .ok_or_else(|| format!("{amiga_path:?} names no file (empty path)"))?;

    let mut mutator = open_writable(image_path)?;
    let mut dir_lba = mutator.volume().root_lba();

    for comp in &components {
        match mutator
            .volume()
            .lookup(dir_lba, comp)
            .map_err(|e| format!("looking up {:?}: {e}", String::from_utf8_lossy(comp)))?
        {
            Some(entry) if entry.kind.is_directory() => dir_lba = entry.lba,
            Some(_) => {
                return Err(format!(
                    "{:?} exists in {amiga_path:?}'s path and is not a directory",
                    String::from_utf8_lossy(comp)
                ))
            }
            None => {
                dir_lba = mutator
                    .create_dir(dir_lba, comp, &Metadata::new())
                    .map_err(|e| {
                        format!(
                            "creating directory {:?}: {e}",
                            String::from_utf8_lossy(comp)
                        )
                    })?;
            }
        }
    }

    // A leaf that already exists is replaced outright -- see this file's
    // module docs for why a plain delete-then-create is the right amount
    // of cleverness for this tool.
    if let Some(existing) = mutator
        .volume()
        .lookup(dir_lba, &leaf)
        .map_err(|e| format!("looking up {:?}: {e}", String::from_utf8_lossy(&leaf)))?
    {
        if existing.kind.is_directory() {
            return Err(format!(
                "{:?} already exists in the image as a directory",
                String::from_utf8_lossy(&leaf)
            ));
        }
        mutator.delete(dir_lba, &leaf).map_err(|e| {
            format!(
                "removing existing {:?}: {e}",
                String::from_utf8_lossy(&leaf)
            )
        })?;
    }

    mutator
        .create_file(dir_lba, &leaf, &Metadata::new(), &data)
        .map_err(|e| format!("writing {:?}: {e}", String::from_utf8_lossy(&leaf)))?;

    println!(
        "installed {} ({} bytes) as {amiga_path} in {}",
        host_file_path.display(),
        data.len(),
        image_path.display()
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hdf-install: {e}");
            ExitCode::FAILURE
        }
    }
}
