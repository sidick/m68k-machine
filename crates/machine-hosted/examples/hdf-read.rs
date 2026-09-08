//! `hdf-read` -- read one file out of an `.hdf` image via `amiga-ffs`,
//! independent of the running machine. This is `scripts/pktport-e2e.sh`'s
//! decisive-proof tool: after the guest writes `PKT0:marker`, this reads
//! it back from the host side, so the proof is a real independent
//! observer rather than a screenshot alone (docs/pktport-protocol.md's
//! whole contract is that guest writes reach the *host* filesystem, not
//! just that the guest's own screen looks right afterward).
//!
//! Mounts read-only the same way `hdf-install`'s `open_writable` does
//! (RDB parse with a bare-volume fallback) -- see that file's module docs
//! for why this duplicates rather than imports that logic.
//!
//! Usage:
//! ```sh
//! cargo run --release -p machine-hosted --example hdf-read -- \
//!     <image.hdf> <amiga-path>
//! ```
//! Prints the file's contents to stdout as raw bytes; exits non-zero
//! (with a message on stderr) if the path does not resolve to a file.

use std::env;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::ExitCode;

use amiga_ffs::{EntryKind, Variant, Volume, DEFAULT_RESERVED};
use amiga_rdb::{Rdb, RdbError};

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

fn split_amiga_path(path: &str) -> Vec<u8> {
    let (prefix, rest) = match path.split_once(':') {
        Some((p, r)) => (Some(p), r),
        None => (None, path),
    };
    let mut components: Vec<&str> = Vec::new();
    if let Some(p) = prefix {
        if !p.is_empty() {
            components.push(p);
        }
    }
    components.extend(rest.split('/').filter(|c| !c.is_empty()));
    components.join("/").into_bytes()
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        return Err(format!(
            "usage: {} <image.hdf> <amiga-path>",
            args.first().map(String::as_str).unwrap_or("hdf-read")
        ));
    }
    let image_path = Path::new(&args[1]);
    let amiga_path = &args[2];
    let full_path = split_amiga_path(amiga_path);

    const BLOCK_SIZE: usize = 512;
    let file =
        File::open(image_path).map_err(|e| format!("opening {}: {e}", image_path.display()))?;
    let total_blocks = file
        .metadata()
        .map_err(|e| format!("stat {}: {e}", image_path.display()))?
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
        Err(e) => return Err(format!("parsing RDB in {}: {e}", image_path.display())),
    };

    medium.base_lba = base_lba;
    medium.len_blocks = Some(len_blocks);

    let mut vol = Volume::open_with(medium, expect, len_blocks, reserved)
        .map_err(|e| format!("opening filesystem in {}: {e}", image_path.display()))?;
    let root = vol.root_lba();

    let entry = vol
        .lookup_path(root, &full_path)
        .map_err(|e| format!("looking up {amiga_path:?}: {e}"))?
        .ok_or_else(|| format!("{amiga_path:?} not found in {}", image_path.display()))?;
    if entry.kind != EntryKind::File {
        return Err(format!("{amiga_path:?} is not a plain file"));
    }

    let chain = vol
        .file_chain(entry.lba)
        .map_err(|e| format!("reading {amiga_path:?}'s chain: {e}"))?;
    let mut buf = vec![0u8; chain.byte_size as usize];
    vol.read_range(&chain, 0, &mut buf)
        .map_err(|e| format!("reading {amiga_path:?}: {e}"))?;

    io::stdout()
        .write_all(&buf)
        .map_err(|e| format!("writing stdout: {e}"))?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hdf-read: {e}");
            ExitCode::FAILURE
        }
    }
}
