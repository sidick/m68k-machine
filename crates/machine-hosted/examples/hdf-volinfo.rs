//! `hdf-volinfo` -- print a served `.hdf` image's volume name, dostype and
//! root entry count via `amiga-ffs`, independent of the running machine.
//!
//! This is `scripts/pktport-format-e2e.sh`'s decisive-proof tool, the
//! `ACTION_FORMAT` analogue of `hdf-read`'s role in `scripts/pktport-
//! e2e.sh`: after the guest runs `C:Format ... NAME TestVol QUICK`, this
//! reads the *volume itself* back from the host side to confirm the
//! reformat actually reached the served image -- name, dostype and an
//! empty root, not merely that the guest's own screen looks right
//! afterward.
//!
//! Mounts read-only the same way `hdf-read`'s own `FileMedium` does (RDB
//! parse with a bare-volume fallback) -- see that file's module docs for
//! why this duplicates rather than imports that logic.
//!
//! Usage:
//! ```sh
//! cargo run --release -p machine-hosted --example hdf-volinfo -- <image.hdf>
//! ```
//! Prints `name=<...> dostype=<DOS\N> entries=<count>` to stdout; exits
//! non-zero (with a message on stderr) if the image will not mount.

use std::env;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::process::ExitCode;

use amiga_ffs::{Variant, Volume, DEFAULT_RESERVED};
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

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        return Err(format!(
            "usage: {} <image.hdf>",
            args.first().map(String::as_str).unwrap_or("hdf-volinfo")
        ));
    }
    let image_path = Path::new(&args[1]);

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
        // No expected variant here either: the whole point of this tool
        // is to observe whatever ACTION_FORMAT actually wrote, not to
        // assert the old variant back at it.
        Err(RdbError::NoRdsk) => (0, total_blocks, None, DEFAULT_RESERVED),
        Err(e) => return Err(format!("parsing RDB in {}: {e}", image_path.display())),
    };

    medium.base_lba = base_lba;
    medium.len_blocks = Some(len_blocks);

    let mut vol = Volume::open_with(medium, expect, len_blocks, reserved)
        .map_err(|e| format!("opening filesystem in {}: {e}", image_path.display()))?;
    let root_lba = vol.root_lba();
    let name = String::from_utf8_lossy(&vol.root().name).into_owned();
    let dostype = vol.variant().dostype();
    let entries = vol
        .read_dir(root_lba)
        .map_err(|e| format!("reading root directory: {e}"))?;

    println!(
        "name={name} dostype=DOS\\{} entries={}",
        dostype & 0xFF,
        entries.len()
    );
    for e in &entries {
        println!("  entry: {}", String::from_utf8_lossy(&e.name));
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hdf-volinfo: {e}");
            ExitCode::FAILURE
        }
    }
}
