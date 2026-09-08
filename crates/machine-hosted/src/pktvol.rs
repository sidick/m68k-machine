//! Host-side filesystem backend for the `pktport` card
//! (`docs/pktport-protocol.md`, ADR 0004): one FFS/OFS volume served out
//! of an `.hdf` image through the published `amiga-rdb` (partition table)
//! and `amiga-ffs` (filesystem) crates.
//!
//! # The seam
//!
//! `docs/pktport-protocol.md` §8 defines the trait this module implements
//! as living in `machine-core` (`no_std`, no `alloc` -- the *trait*, not
//! the implementation): [`machine_core::pktport::PacketBackend`], the
//! seam between the card's request lifecycle and this module's
//! filesystem semantics.
//!
//! # Mounting
//!
//! [`PktVolume::open`] parses the image's RDB with `amiga-rdb` and picks
//! a partition (the first one `amiga-ffs` recognises as a `DOS\0`-`DOS\7`
//! member, or one named by `partition_name`), then hands `amiga-ffs` a
//! partition-relative view of the same file. `amiga-rdb::RdbError::NoRdsk`
//! -- no `RDSK` block in the first 16 blocks -- is not a mount failure:
//! it is how a bare, RDB-less FFS/OFS volume (block 0 is the boot block)
//! is detected, and mounting falls back to the whole file.
//!
//! [`FileMedium`] is the one adapter type this needs: `amiga-rdb` and
//! `amiga-ffs` each define their own `BlockSource`/`BlockSink` pair --
//! same shape, different traits, so one type can implement both without
//! conflict (nothing here ever calls their methods directly; only the
//! generic functions that require one bound or the other do, so there is
//! no ambiguity to resolve). It reads and writes 512-byte device blocks
//! at a fixed offset from the start of the file, and that offset moves
//! exactly once, from 0 (reading the RDB, or the whole disk when there is
//! none) to the chosen partition's `start_lba` (or stays 0 for a bare
//! volume) before `amiga-ffs` ever sees it.
//!
//! # Locks and handles (§4)
//!
//! One handle table, one monotonic non-zero `u32` counter, shared by
//! locks and open files (`Handle::Lock`/`Handle::File`) -- the protocol
//! doc describes them as the same opaque-handle mechanism, and giving
//! them one table means a handle of the wrong kind is caught the same
//! way an unknown one is: a lookup miss, mapped to error 205, never a
//! table index handed back to the guest. Lock `0` is the volume root and
//! is never stored in the table (nothing to free, nothing to look up).
//!
//! # Error mapping (§6)
//!
//! [`map_read_error`], [`map_mutate_error`] and [`map_alloc_error`]
//! translate `amiga-ffs`'s typed errors onto the `RES2` codes §6 lists.
//! The codes §6 says version 1 "must produce correctly" are handled
//! precisely (see each function's own comments for which `amiga-ffs`
//! variant maps to which code and why); everything else -- volume
//! corruption `amiga-ffs`'s read side already refuses to trust
//! (`Checksum`, `ChainCycle`, `OwnKeyMismatch`, ...), which a correctly
//! written volume never produces -- collapses to the closest code in the
//! required set (usually 205, "could not resolve this") rather than
//! inventing a number the protocol doc never asked for. That collapse is
//! documented at each `_ =>` arm below.
//!
//! # Hostility (§7)
//!
//! Every guest pointer -- BSTR name, `FileInfoBlock`, `InfoData`, read/
//! write buffer, `DateStamp` -- goes through [`GuestMemory::ram_slice`]/
//! [`ram_slice_mut`](GuestMemory::ram_slice_mut), never trusted raw. An
//! unreadable name fails with `ERROR_INVALID_COMPONENT_NAME` (210, §7's
//! own wording); an unreadable buffer/FIB/`InfoData`/`DateStamp` fails
//! with `ERROR_BAD_NUMBER` (115, §7's "115-class refusal for buffers").
//! A BSTR whose declared length runs past what the containing span can
//! supply is clipped to what is actually there ([`read_bstr`]'s
//! byte-by-byte fallback), never read past the mapped region.

// `execute` and everything it reaches are only reachable through
// `PacketBackend`, and nothing in `machine-hosted` calls it yet: `run.rs`
// constructs a `PktVolume` and stops at the documented TODO, waiting on
// `machine_core::pktport`'s card (a parallel worker's deliverable) to
// land and start driving it at tick time. Until then this whole surface
// looks dead to the compiler from `main`'s side; it is not -- it is
// exercised in full by this module's own tests, and the supervisor
// removes this allow along with the TODO once the card attaches.
#![allow(dead_code)]

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use amiga_ffs::{
    format as ffs_format, AllocError, Allocator, DateStamp, EntryKind, Error as FfsError,
    FormatError, FormatOptions, MetaUpdate, Metadata, MutateError, Mutator, Variant, Volume,
    DEFAULT_RESERVED, ST_ROOT,
};
use amiga_rdb::{Rdb, RdbError};

use machine_core::pktport::PacketBackend;
use machine_core::GuestMemory;

// ---------------------------------------------------------------------------
// Actions (§5) and DOS booleans
// ---------------------------------------------------------------------------

mod action {
    pub const LOCATE_OBJECT: u32 = 8;
    // 15 per NDK dos/dosextens.h -- 9 is ACTION_RENAME_DISK. The
    // protocol doc originally said 9; the 68k stub caught it against
    // real dos.library 47.30.
    pub const FREE_LOCK: u32 = 15;
    pub const COPY_DIR: u32 = 19;
    pub const PARENT: u32 = 29;
    pub const SAME_LOCK: u32 = 40;
    /// `dos/dosextens.h` -- Arg1: `BOOL` (nonzero = inhibit). Flushes and
    /// invalidates every open handle when inhibiting; remounts from the
    /// medium when un-inhibiting (docs/pktport-protocol.md §5).
    pub const INHIBIT: u32 = 31;
    /// `dos/dosextens.h` -- Arg1: volume-name BSTR, Arg2: dostype. Legal
    /// only while `INHIBIT`ed; re-initializes the medium via
    /// `amiga-ffs::format` (QUICK format only -- see §5).
    pub const FORMAT: u32 = 1020;
    pub const EXAMINE_OBJECT: u32 = 23;
    pub const EXAMINE_NEXT: u32 = 24;
    pub const INFO: u32 = 26;
    pub const DISK_INFO: u32 = 25;
    pub const FINDINPUT: u32 = 1005;
    pub const FINDOUTPUT: u32 = 1006;
    pub const FINDUPDATE: u32 = 1004;
    pub const END: u32 = 1007;
    pub const READ: u32 = 82; // 'R'
    pub const WRITE: u32 = 87; // 'W'
    pub const SEEK: u32 = 1008;
    pub const SET_FILE_SIZE: u32 = 1022;
    pub const CREATE_DIR: u32 = 22;
    pub const DELETE_OBJECT: u32 = 16;
    pub const RENAME_OBJECT: u32 = 17;
    pub const SET_PROTECT: u32 = 21;
    pub const SET_COMMENT: u32 = 28;
    pub const SET_DATE: u32 = 34;
    pub const IS_FILESYSTEM: u32 = 1027;
    pub const FLUSH: u32 = 27;
}

/// `dp_Res1`'s boolean convention.
const DOSTRUE: u32 = 0xFFFF_FFFF;
const DOSFALSE: u32 = 0;
/// `Read`/`Write`'s error convention for `RES1`: not a count.
const READ_WRITE_ERROR: u32 = 0xFFFF_FFFF;

/// `RES2` error codes, §6. Named for the AmigaDOS constant they carry.
mod err {
    pub const NO_FREE_STORE: u32 = 103;
    pub const OBJECT_IN_USE: u32 = 202;
    pub const OBJECT_EXISTS: u32 = 203;
    pub const DIR_NOT_FOUND: u32 = 204;
    pub const OBJECT_NOT_FOUND: u32 = 205;
    pub const ACTION_NOT_KNOWN: u32 = 209;
    pub const INVALID_COMPONENT_NAME: u32 = 210;
    pub const OBJECT_WRONG_TYPE: u32 = 212;
    pub const NOT_VALIDATED: u32 = 213;
    pub const WRITE_PROTECTED: u32 = 214;
    pub const DIRECTORY_NOT_EMPTY: u32 = 216;
    pub const DISK_FULL: u32 = 221;
    pub const WRITE_PROTECTED_FILE: u32 = 223;
    pub const NOT_A_DOS_DISK: u32 = 225;
    pub const NO_MORE_ENTRIES: u32 = 232;
    /// §7's "115-class refusal for buffers" -- an unreadable buffer, FIB,
    /// `InfoData` or `DateStamp` pointer. Not in §6's mandatory table
    /// (which is about filesystem semantics, not hostile pointers), but
    /// named explicitly by §7's own wording.
    pub const BAD_NUMBER: u32 = 115;
}

const FIB_SIZE: u32 = 260;
const INFO_DATA_SIZE: u32 = 36;
/// `dos/dos.h` `ID_WRITE_PROTECTED` / `ID_VALIDATED` -- `id_DiskState`.
const ID_WRITE_PROTECTED: u32 = 80;
const ID_VALIDATED: u32 = 82;

// ---------------------------------------------------------------------------
// FileMedium: the one BlockSource/BlockSink adapter this needs
// ---------------------------------------------------------------------------

/// A `File`-backed medium at a fixed 512-byte device block size, reading
/// and writing at a `base_lba` offset that [`PktVolume::open`] sets once
/// (0 for the whole disk while parsing the RDB, then the chosen
/// partition's `start_lba`, or still 0 for a bare RDB-less volume).
///
/// Implements *both* `amiga_rdb::BlockSource` and `amiga_ffs`'s
/// `BlockSource`/`BlockSink` -- see this module's doc comment for why
/// that is unambiguous despite the identical method names.
struct FileMedium {
    file: File,
    block_size: usize,
    base_lba: u64,
    /// Blocks visible through this medium from `base_lba` onward. `None`
    /// only for the instant between construction and the first RDB
    /// parse; every subsequent read/write sees `Some`.
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

// ---------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------

/// Everything [`PktVolume::open`] can refuse.
#[derive(Debug)]
pub enum OpenError {
    Io(io::Error),
    Rdb(RdbError<io::Error>),
    Ffs(FfsError<io::Error>),
    Mutate(MutateError<io::Error>),
    Alloc(AllocError<io::Error>),
    /// The RDB has no partition `amiga-ffs` recognises as `DOS\0`-`DOS\7`.
    NoFilesystemPartition,
    /// `partition_name` named a drive this RDB does not have.
    PartitionNotFound(String),
}

impl From<io::Error> for OpenError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Rdb(e) => write!(f, "{e}"),
            Self::Ffs(e) => write!(f, "{e}"),
            Self::Mutate(e) => write!(f, "{e}"),
            Self::Alloc(e) => write!(f, "{e}"),
            Self::NoFilesystemPartition => {
                write!(f, "no DOS\\0-DOS\\7 partition found in the RDB")
            }
            Self::PartitionNotFound(name) => {
                write!(f, "no partition named {name:?} in the RDB")
            }
        }
    }
}

impl std::error::Error for OpenError {}

// ---------------------------------------------------------------------------
// Handles (§4)
// ---------------------------------------------------------------------------

enum Handle {
    /// A lock's target: the header block of the directory or file it
    /// names. Never stored for lock 0 -- see this module's doc comment.
    Lock(u64),
    File(OpenFile),
}

struct OpenFile {
    /// The directory this file's entry lives in -- needed because
    /// `amiga_ffs::Mutator`'s write/truncate API is keyed by
    /// `(parent, name)`, not by header block.
    parent_lba: u64,
    name: Vec<u8>,
    /// The file's own header block -- enough for reads, which only need
    /// `Volume::file_chain`.
    header_lba: u64,
    position: u64,
    /// Whether this handle may write (`FINDOUTPUT`/`FINDUPDATE` on a
    /// writable volume). `FINDINPUT` handles are always `false`.
    write: bool,
}

// ---------------------------------------------------------------------------
// PktVolume
// ---------------------------------------------------------------------------

enum Backing {
    ReadOnly {
        vol: Volume<FileMedium>,
        alloc: Allocator<io::Error>,
    },
    Writable(Mutator<FileMedium>),
}

/// One FFS/OFS volume served out of an `.hdf` image, implementing
/// [`PacketBackend`] over `amiga-rdb` + `amiga-ffs`. See this module's
/// doc comment.
pub struct PktVolume {
    backing: Backing,
    handles: HashMap<u32, Handle>,
    next_handle: u32,
    writable: bool,
    /// Where the underlying image lives on the host -- kept so `INHIBIT`
    /// `FALSE` can reopen a fresh [`FileMedium`] and remount from
    /// scratch (`amiga-ffs`'s `Volume`/`Mutator` cache the variant,
    /// parsed root and bitmap at open time; there is no supported way to
    /// re-derive all of that in place after `ACTION_FORMAT` may have
    /// changed the variant itself, so a full remount is the only
    /// correct move -- see the module doc's ACTION_INHIBIT/FORMAT
    /// section).
    path: std::path::PathBuf,
    /// The partition's start block within the image file (0 for a bare
    /// RDB-less volume), captured at open time -- `ACTION_FORMAT` never
    /// moves or resizes the partition, only reinitializes what is inside
    /// it, so this stays valid across a format/remount cycle.
    base_lba: u64,
    /// Blocks in the partition (the filesystem's own extent) -- the
    /// `block_count` a remount and a reformat both need.
    len_blocks: u64,
    /// Blocks reserved at the front, from the RDB's `de_Reserved` (or
    /// [`DEFAULT_RESERVED`] for a bare volume) -- carried across
    /// format/remount exactly like `len_blocks`.
    reserved: u64,
    block_size: usize,
    /// Set by `ACTION_INHIBIT(TRUE)`, cleared by a successful
    /// `ACTION_INHIBIT(FALSE)`. While set, every action except
    /// `INHIBIT`, `FORMAT`, `IS_FILESYSTEM` and `DISK_INFO` refuses with
    /// `ERROR_NOT_A_DOS_DISK` (225) -- see the module doc.
    inhibited: bool,
}

impl PktVolume {
    /// Open `path` as a `pktport` volume.
    ///
    /// `partition_name` selects a drive by `pb_DriveName` when the image
    /// has more than one filesystem partition; `None` takes the first
    /// one `amiga-ffs` recognises (`DOS\0`-`DOS\7`) in on-disk chain
    /// order. An image with no `RDSK` block at all
    /// ([`RdbError::NoRdsk`]) is mounted as a bare volume from block 0 --
    /// see this module's doc comment.
    pub fn open(
        path: &Path,
        writable: bool,
        partition_name: Option<&str>,
    ) -> Result<Self, OpenError> {
        const BLOCK_SIZE: usize = 512;

        let file = OpenOptions::new().read(true).write(writable).open(path)?;
        let total_blocks = file.metadata()?.len() / BLOCK_SIZE as u64;
        let mut medium = FileMedium {
            file,
            block_size: BLOCK_SIZE,
            base_lba: 0,
            len_blocks: Some(total_blocks),
        };

        let (base_lba, len_blocks, expect, reserved) = match Rdb::parse(&mut medium) {
            Ok(rdb) => {
                let chosen = match partition_name {
                    Some(name) => rdb
                        .partitions
                        .iter()
                        .find(|p| p.name == name)
                        .ok_or_else(|| OpenError::PartitionNotFound(name.to_string()))?,
                    None => rdb
                        .partitions
                        .iter()
                        .find(|p| Variant::from_dostype(p.dos_type).is_some())
                        .ok_or(OpenError::NoFilesystemPartition)?,
                };
                // envec longword index 6 is `de_Reserved`
                // (`devices/hardblocks.h`'s `DosEnvec`, table_size(0),
                // size_block(1), sec_org(2), surfaces(3),
                // sectors_per_block(4), blocks_per_track(5), reserved(6),
                // ...); `amiga-rdb`'s `Partition` does not surface it as
                // a named field (only `envec_raw`, deliberately, per its
                // own doc comment), so this indexes the raw table
                // directly. Falls back to `DEFAULT_RESERVED` when the
                // envec is too short to reach it -- `envec_raw` is
                // already clamped to what actually fits, so a short
                // table reads as "not present" here, the same as it does
                // for `amiga-rdb`'s own optional fields.
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
            Err(e) => return Err(OpenError::Rdb(e)),
        };

        medium.base_lba = base_lba;
        medium.len_blocks = Some(len_blocks);

        let mut vol =
            Volume::open_with(medium, expect, len_blocks, reserved).map_err(OpenError::Ffs)?;

        let backing = if writable {
            Backing::Writable(Mutator::open(vol).map_err(OpenError::Mutate)?)
        } else {
            let alloc = Allocator::load(&mut vol).map_err(OpenError::Alloc)?;
            Backing::ReadOnly { vol, alloc }
        };

        Ok(Self {
            backing,
            handles: HashMap::new(),
            next_handle: 1,
            writable,
            path: path.to_path_buf(),
            base_lba,
            len_blocks,
            reserved,
            block_size: BLOCK_SIZE,
            inhibited: false,
        })
    }

    /// Open a fresh [`FileMedium`] at this volume's own path/geometry --
    /// `ACTION_INHIBIT(FALSE)`'s remount step, and nothing else: reusing
    /// the medium already inside `self.backing` is not an option, since
    /// that medium is trapped behind `amiga-ffs`'s owning `Volume`/
    /// `Mutator` with no public way to hand it back out, and even if it
    /// were reachable its cached variant/root/bitmap would still be
    /// stale after `ACTION_FORMAT`.
    fn open_medium(&self) -> io::Result<FileMedium> {
        let file = OpenOptions::new()
            .read(true)
            .write(self.writable)
            .open(&self.path)?;
        Ok(FileMedium {
            file,
            block_size: self.block_size,
            base_lba: self.base_lba,
            len_blocks: Some(self.len_blocks),
        })
    }

    /// `ACTION_INHIBIT(FALSE)`'s remount: reopen the image from disk and
    /// rebuild `self.backing` from scratch, dropping every open handle
    /// (they name structures the old, now-discarded `Volume`/`Mutator`
    /// owned).
    ///
    /// `expect` is `None` -- "believe the disk" -- rather than whatever
    /// variant this volume opened as, deliberately: `ACTION_FORMAT` may
    /// have just written a different `DOS\x` dostype into the boot
    /// block, and re-asserting the old variant here would make every
    /// post-format remount fail with a manufactured `VariantMismatch`.
    fn remount(&mut self) -> Result<(), u32> {
        let medium = self.open_medium().map_err(|_| err::OBJECT_NOT_FOUND)?;
        let mut vol = Volume::open_with(medium, None, self.len_blocks, self.reserved)
            .map_err(|e| map_read_error(&e))?;
        self.backing = if self.writable {
            Backing::Writable(Mutator::open(vol).map_err(|e| map_mutate_error(&e))?)
        } else {
            let alloc = Allocator::load(&mut vol).map_err(|e| map_alloc_error(&e))?;
            Backing::ReadOnly { vol, alloc }
        };
        self.handles.clear();
        Ok(())
    }

    fn volume(&mut self) -> &mut Volume<FileMedium> {
        match &mut self.backing {
            Backing::ReadOnly { vol, .. } => vol,
            Backing::Writable(m) => m.volume(),
        }
    }

    fn mutator_mut(&mut self) -> Option<&mut Mutator<FileMedium>> {
        match &mut self.backing {
            Backing::Writable(m) => Some(m),
            Backing::ReadOnly { .. } => None,
        }
    }

    fn blocks_used(&self) -> u64 {
        match &self.backing {
            Backing::ReadOnly { alloc, .. } => alloc.blocks_used(),
            Backing::Writable(m) => m.allocator().blocks_used(),
        }
    }

    fn alloc_handle(&mut self, h: Handle) -> u32 {
        let id = self.next_handle;
        self.next_handle = self.next_handle.saturating_add(1);
        self.handles.insert(id, h);
        id
    }

    /// Resolve a wire lock value to the header block it names -- `0` is
    /// the volume root, never stored in the table; anything else is a
    /// `Handle::Lock` lookup. `None` for an unknown handle or one that
    /// names a file, not a lock -- both are "unknown handle" per §7.
    fn resolve_lock(&mut self, handle: u32) -> Option<u64> {
        if handle == 0 {
            return Some(self.volume().root_lba());
        }
        match self.handles.get(&handle) {
            Some(Handle::Lock(lba)) => Some(*lba),
            _ => None,
        }
    }

    /// A new lock naming `lba` -- `0` when `lba` is the root, so the
    /// null lock is reused rather than allocating a handle nobody needs
    /// to free.
    fn dup_lock(&mut self, lba: u64) -> u32 {
        let root = self.volume().root_lba();
        if lba == root {
            0
        } else {
            self.alloc_handle(Handle::Lock(lba))
        }
    }

    /// Split a (possibly multi-component) path relative to `dir_lba`
    /// into its final parent directory and leaf name -- what
    /// `amiga_ffs::Mutator`'s `(parent, name)`-keyed API needs from a
    /// packet's single path-carrying name argument.
    fn split_parent(&mut self, dir_lba: u64, path: &[u8]) -> Result<(u64, Vec<u8>), u32> {
        let mut components: Vec<&[u8]> = path
            .split(|&c| c == b'/')
            .filter(|c| !c.is_empty())
            .collect();
        let leaf = match components.pop() {
            Some(l) => l.to_vec(),
            None => return Err(err::INVALID_COMPONENT_NAME),
        };
        let mut here = dir_lba;
        for comp in components {
            match self.volume().lookup(here, comp) {
                Ok(Some(e)) if e.kind.is_directory() => here = e.lba,
                Ok(Some(_)) => return Err(err::OBJECT_WRONG_TYPE),
                Ok(None) => return Err(err::DIR_NOT_FOUND),
                Err(e) => return Err(map_read_error(&e)),
            }
        }
        Ok((here, leaf))
    }

    /// Blocks an entry occupies, for `fib_NumBlocks`: the header alone
    /// for a directory or the root (this crate does not track a
    /// directory's own data-block usage beyond that), or the file's full
    /// chain -- data blocks, extension blocks and the header -- for a
    /// file. `0` if the chain will not even read (a damaged file):
    /// `fib_NumBlocks` is advisory and this is not the place to fail the
    /// whole `EXAMINE`.
    fn count_blocks(&mut self, lba: u64, is_dir: bool) -> u32 {
        if is_dir {
            return 1;
        }
        match self.volume().file_chain(lba) {
            Ok(chain) => (chain.blocks.len() + chain.extensions.len() + 1) as u32,
            Err(_) => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Error mapping (§6)
// ---------------------------------------------------------------------------

/// Map `amiga_ffs::read::Error` onto §6's `RES2` codes.
///
/// The arms §6 requires: `NameTooLong` -> 210 (an invalid name was
/// offered for lookup), `NotADirectory`/`NotHeader` -> 212 (a lock or
/// path component that is not what the caller needed it to be),
/// `UnknownDosType`/`VariantMismatch`/`BadBlockSize` -> 225 (not a DOS
/// disk this crate can read). Everything else is structural volume
/// damage this crate's read side already refuses to trust on its own
/// terms (`Checksum`, `ChainCycle`, `ChainTooLong`, `WrongBlockType`,
/// `OwnKeyMismatch`, `BlockOwnerMismatch`, `DataPointerCount`,
/// `DataPointerHole`, `FileSizeMismatch`, `DataBlockSequence`,
/// `DataBlockSize`, `NotALink`, `LinkTargetMissing`,
/// `DircacheRecordOverflow`, `LbaOutOfRange`, `WrongSecondaryType`,
/// `UnknownSecondaryType`, `VolumeTooSmall`, `UnknownBlockCount`,
/// `SoftLinkNotResolved`) or a transport failure (`Io`) -- none of these
/// have a dedicated code in §6's table, so they collapse to 205 (could
/// not resolve the object), the closest available meaning: a correctly
/// written volume never produces them, and a damaged one refusing to
/// resolve the request it was asked for is honest.
fn map_read_error<E>(e: &FfsError<E>) -> u32 {
    use amiga_ffs::Error::*;
    match e {
        NotADirectory { .. } | NotHeader { .. } => err::OBJECT_WRONG_TYPE,
        NameTooLong { .. } => err::INVALID_COMPONENT_NAME,
        UnknownDosType(_) | VariantMismatch { .. } | BadBlockSize(_) => err::NOT_A_DOS_DISK,
        _ => err::OBJECT_NOT_FOUND,
    }
}

/// Map `amiga_ffs::AllocError` onto §6's codes: `VolumeFull` -> 221 (disk
/// full, exactly what it means), `BitmapInvalid` -> 213 (this is the
/// backend's own version of "mounted a volume `validate()` flagged" --
/// an allocator refusing to hand out blocks from a bitmap mid-update is
/// precisely a not-validated volume). Everything else is an allocator
/// invariant this crate's own write ordering is supposed to make
/// unreachable (`NotCovered`, `DoubleFree`, `NotDurable`, `PageMissing`,
/// `BadBlockSize`) or a transport failure -- 205, same reasoning as
/// `map_read_error`'s catch-all.
fn map_alloc_error<E>(e: &AllocError<E>) -> u32 {
    use amiga_ffs::AllocError::*;
    match e {
        Read(re) => map_read_error(re),
        BitmapInvalid => err::NOT_VALIDATED,
        VolumeFull { .. } => err::DISK_FULL,
        _ => err::OBJECT_NOT_FOUND,
    }
}

/// Map `amiga_ffs::MutateError` onto §6's codes. The arms §6 requires:
/// `DuplicateName` -> 203 (exists), `DirectoryNotEmpty` -> 216,
/// `NotADirectory`/`NotAFile` -> 212 (wrong type), `NotFound` -> 205,
/// name/comment problems (`NameEmpty`, `NameTooLong`, `NameInvalidByte`,
/// `CommentTooLong`) -> 210 (invalid name). `LinkedTo` and
/// `IntoOwnSubtree` are both "this object cannot be touched in its
/// current relationship to something else" -> 202 (object in use), the
/// closest of §6's codes to that shape and not a guess this crate
/// invented: real AmigaDOS filesystems answer both of these same-shaped
/// refusals with `ERROR_OBJECT_IN_USE` too. `IsRoot` -> 212 (the volume
/// root is not a directory *entry*, which is exactly what "wrong type"
/// means here). `FileTooLarge` has no dedicated code in §6's table; 221
/// (disk full) is the closest available meaning -- both say "this
/// backend cannot hold what was asked for". `NotInChain`/
/// `NotInLinkChain` are volume corruption this crate's own writes never
/// produce -> 205, `map_read_error`'s catch-all reasoning.
fn map_mutate_error<E>(e: &MutateError<E>) -> u32 {
    use amiga_ffs::MutateError::*;
    match e {
        Read(re) => map_read_error(re),
        Alloc(ae) => map_alloc_error(ae),
        NameEmpty | NameTooLong { .. } | NameInvalidByte { .. } | CommentTooLong { .. } => {
            err::INVALID_COMPONENT_NAME
        }
        NotADirectory { .. } | NotAFile { .. } | IsRoot { .. } => err::OBJECT_WRONG_TYPE,
        NotFound { .. } => err::OBJECT_NOT_FOUND,
        DuplicateName { .. } => err::OBJECT_EXISTS,
        DirectoryNotEmpty { .. } => err::DIRECTORY_NOT_EMPTY,
        LinkedTo { .. } | IntoOwnSubtree { .. } => err::OBJECT_IN_USE,
        FileTooLarge { .. } => err::DISK_FULL,
        _ => err::OBJECT_NOT_FOUND,
    }
}

/// Map `amiga_ffs::FormatError` onto §6's codes for `ACTION_FORMAT`.
/// None of these have a dedicated §6 code (§6's table is about ordinary
/// filesystem operations, and formatting is not one); the closest
/// available meanings: `NameEmpty`/`NameTooLong`/`NameInvalidByte` ->
/// 210 (invalid name), the same code the read/mutate paths use for
/// their own name problems. `BadBlockSize`/`BadReserved`/
/// `VolumeTooSmall`/`VolumeTooLarge`/`SinkTooSmall` are all "this
/// geometry cannot be formatted at all" -> 225 (not a DOS disk this
/// crate can produce), `map_read_error`'s own reasoning for geometry
/// this crate refuses to trust, applied to the write side. `Io` is a
/// transport failure -> 205, every other mapper's catch-all.
fn map_format_error<E>(e: &FormatError<E>) -> u32 {
    use FormatError::*;
    match e {
        NameEmpty | NameTooLong { .. } | NameInvalidByte { .. } => err::INVALID_COMPONENT_NAME,
        BadBlockSize(_)
        | BadReserved { .. }
        | VolumeTooSmall { .. }
        | VolumeTooLarge { .. }
        | SinkTooSmall { .. } => err::NOT_A_DOS_DISK,
        Io(_) => err::OBJECT_NOT_FOUND,
    }
}

// ---------------------------------------------------------------------------
// Guest memory helpers (§7)
// ---------------------------------------------------------------------------

/// Decode a BSTR (BCPL string: length byte, then that many Latin-1
/// bytes) reached through a BPTR (`addr = bptr << 2`). `None` if even
/// the length byte is unreadable.
///
/// If the declared length runs past what the containing span can supply
/// -- a hostile length byte, or a string that runs off the end of mapped
/// RAM -- this clips to what is actually there rather than failing the
/// whole read, per §7's "BSTR length bytes clipped to what the
/// containing guest RAM span can actually supply".
fn read_bstr(mem: &dyn GuestMemory, bptr: u32) -> Option<Vec<u8>> {
    let addr = bptr.wrapping_mul(4);
    let len = mem.ram_slice(addr, 1)?[0] as usize;
    if let Some(s) = mem.ram_slice(addr.wrapping_add(1), len as u32) {
        return Some(s.to_vec());
    }
    let mut out = Vec::with_capacity(len);
    for i in 0..len as u32 {
        match mem.ram_slice(addr.wrapping_add(1).wrapping_add(i), 1) {
            Some(b) => out.push(b[0]),
            None => break,
        }
    }
    Some(out)
}

/// Write a length-prefixed BCPL string into a fixed-size FIB/entry
/// field, clipped to the field's own capacity (`field.len() - 1` content
/// bytes) -- never the guest's problem, since the field size is this
/// backend's own constant, not something the guest supplied.
fn write_bstr_field(field: &mut [u8], bytes: &[u8]) {
    let cap = field.len().saturating_sub(1);
    let n = bytes.len().min(cap);
    field[0] = n as u8;
    field[1..1 + n].copy_from_slice(&bytes[..n]);
}

/// Fill a `FileInfoBlock` at the offsets `docs/pktport-protocol.md`'s
/// task brief gives (NDK `dos/dos.h`): `fib_DiskKey`(0),
/// `fib_DirEntryType`(4), `fib_FileName` BCPL(8, 108 bytes),
/// `fib_Protection`(116), `fib_EntryType`(120), `fib_Size`(124),
/// `fib_NumBlocks`(128), `fib_Date`(132, 3 longs), `fib_Comment`
/// BCPL(144, 80 bytes), `fib_OwnerUID`(224)/`fib_OwnerGID`(226). Zeroes
/// the whole [`FIB_SIZE`]-byte structure first, so a reused guest buffer
/// never leaks a previous `EXAMINE`'s bytes into a field this call does
/// not set.
#[allow(clippy::too_many_arguments)]
fn fill_fib(
    fib: &mut [u8],
    disk_key: u32,
    dir_entry_type: i32,
    name: &[u8],
    protection: u32,
    size: u32,
    num_blocks: u32,
    date: DateStamp,
    comment: &[u8],
    owner: u32,
) {
    fib.iter_mut().for_each(|b| *b = 0);
    fib[0..4].copy_from_slice(&disk_key.to_be_bytes());
    fib[4..8].copy_from_slice(&(dir_entry_type as u32).to_be_bytes());
    write_bstr_field(&mut fib[8..116], name);
    fib[116..120].copy_from_slice(&protection.to_be_bytes());
    fib[120..124].copy_from_slice(&(dir_entry_type as u32).to_be_bytes());
    fib[124..128].copy_from_slice(&size.to_be_bytes());
    fib[128..132].copy_from_slice(&num_blocks.to_be_bytes());
    fib[132..136].copy_from_slice(&date.days.to_be_bytes());
    fib[136..140].copy_from_slice(&date.mins.to_be_bytes());
    fib[140..144].copy_from_slice(&date.ticks.to_be_bytes());
    write_bstr_field(&mut fib[144..224], comment);
    fib[224..226].copy_from_slice(&((owner >> 16) as u16).to_be_bytes());
    fib[226..228].copy_from_slice(&(owner as u16).to_be_bytes());
}

/// Fill an `InfoData` at the task brief's offsets: `id_NumSoftErrors`(0),
/// `id_UnitNumber`(4), `id_DiskState`(8), `id_NumBlocks`(12),
/// `id_NumBlocksUsed`(16), `id_BytesPerBlock`(20), `id_DiskType`(24),
/// `id_VolumeNode`(28), `id_InUse`(32). `id_UnitNumber` and
/// `id_VolumeNode` are left 0: this backend serves one volume with no
/// unit numbering of its own, and it has no address for the guest-side
/// `DeviceList` node DOS itself owns (the stub's job, not the host's).
fn fill_info_data(
    info: &mut [u8],
    num_blocks: u64,
    num_blocks_used: u64,
    bytes_per_block: u32,
    disk_type: u32,
    writable: bool,
) {
    info.iter_mut().for_each(|b| *b = 0);
    let disk_state = if writable {
        ID_VALIDATED
    } else {
        ID_WRITE_PROTECTED
    };
    info[8..12].copy_from_slice(&disk_state.to_be_bytes());
    info[12..16].copy_from_slice(&(num_blocks as u32).to_be_bytes());
    info[16..20].copy_from_slice(&(num_blocks_used as u32).to_be_bytes());
    info[20..24].copy_from_slice(&bytes_per_block.to_be_bytes());
    info[24..28].copy_from_slice(&disk_type.to_be_bytes());
    info[32..36].copy_from_slice(&DOSTRUE.to_be_bytes());
}

// ---------------------------------------------------------------------------
// PacketBackend
// ---------------------------------------------------------------------------

impl PacketBackend for PktVolume {
    fn execute(&mut self, action: u32, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        self.execute_inner(action, args, mem)
    }
}

impl PktVolume {
    fn execute_inner(
        &mut self,
        action: u32,
        args: [u32; 7],
        mem: &mut dyn GuestMemory,
    ) -> (u32, u32) {
        match action {
            // Legal at all times, inhibited or not -- the only four
            // exempt actions per §5's ACTION_INHIBIT/FORMAT section.
            action::INHIBIT => self.inhibit_action(args),
            action::FORMAT => self.format_action(args, mem),
            action::IS_FILESYSTEM => (DOSTRUE, 0),
            action::DISK_INFO => self.disk_info(args, mem),
            // Everything else refuses outright while inhibited: the
            // handles a lock/file action would resolve may name
            // structures from a `Volume`/`Mutator` that is about to be
            // (or already has been) discarded from under them.
            _ if self.inhibited => (DOSFALSE, err::NOT_A_DOS_DISK),
            action::LOCATE_OBJECT => self.locate_object(args, mem),
            action::FREE_LOCK => self.free_lock(args),
            action::COPY_DIR => self.copy_dir(args),
            action::PARENT => self.parent(args),
            action::SAME_LOCK => self.same_lock(args),
            action::EXAMINE_OBJECT => self.examine_object(args, mem),
            action::EXAMINE_NEXT => self.examine_next(args, mem),
            action::INFO => self.info(args, mem),
            action::FINDINPUT => self.find_input(args, mem),
            action::FINDOUTPUT => self.find_output(args, mem),
            action::FINDUPDATE => self.find_update(args, mem),
            action::END => self.end(args),
            action::READ => self.read_action(args, mem),
            action::WRITE => self.write_action(args, mem),
            action::SEEK => self.seek_action(args),
            action::SET_FILE_SIZE => self.set_file_size(args),
            action::CREATE_DIR => self.create_dir_action(args, mem),
            action::DELETE_OBJECT => self.delete_object(args, mem),
            action::RENAME_OBJECT => self.rename_object(args, mem),
            action::SET_PROTECT => self.set_protect(args, mem),
            action::SET_COMMENT => self.set_comment(args, mem),
            action::SET_DATE => self.set_date(args, mem),
            action::FLUSH => self.flush(),
            _ => (DOSFALSE, err::ACTION_NOT_KNOWN),
        }
    }
}

impl PktVolume {
    fn locate_object(&mut self, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        let dir_lba = match self.resolve_lock(args[0]) {
            Some(l) => l,
            None => return (DOSFALSE, err::OBJECT_NOT_FOUND),
        };
        let name = match read_bstr(mem, args[1]) {
            Some(n) => n,
            None => return (DOSFALSE, err::INVALID_COMPONENT_NAME),
        };
        if name.is_empty() {
            return (self.dup_lock(dir_lba), 0);
        }
        match self.volume().lookup_path(dir_lba, &name) {
            Ok(Some(entry)) => (self.dup_lock(entry.lba), 0),
            Ok(None) => (DOSFALSE, err::OBJECT_NOT_FOUND),
            Err(e) => (DOSFALSE, map_read_error(&e)),
        }
    }

    fn free_lock(&mut self, args: [u32; 7]) -> (u32, u32) {
        let lock = args[0];
        if lock == 0 {
            return (DOSTRUE, 0);
        }
        match self.handles.get(&lock) {
            Some(Handle::Lock(_)) => {
                self.handles.remove(&lock);
                (DOSTRUE, 0)
            }
            _ => (DOSFALSE, err::OBJECT_NOT_FOUND),
        }
    }

    fn copy_dir(&mut self, args: [u32; 7]) -> (u32, u32) {
        match self.resolve_lock(args[0]) {
            Some(lba) => (self.dup_lock(lba), 0),
            None => (DOSFALSE, err::OBJECT_NOT_FOUND),
        }
    }

    fn parent(&mut self, args: [u32; 7]) -> (u32, u32) {
        let lba = match self.resolve_lock(args[0]) {
            Some(l) => l,
            None => return (DOSFALSE, err::OBJECT_NOT_FOUND),
        };
        let root = self.volume().root_lba();
        if lba == root {
            return (0, 0);
        }
        match self.volume().entry_at(lba) {
            Ok(entry) => (self.dup_lock(entry.parent as u64), 0),
            Err(e) => (DOSFALSE, map_read_error(&e)),
        }
    }

    fn same_lock(&mut self, args: [u32; 7]) -> (u32, u32) {
        let a = self.resolve_lock(args[0]);
        let b = self.resolve_lock(args[1]);
        match (a, b) {
            (Some(x), Some(y)) if x == y => (DOSTRUE, 0),
            (Some(_), Some(_)) => (DOSFALSE, 0),
            _ => (DOSFALSE, err::OBJECT_NOT_FOUND),
        }
    }

    fn examine_object(&mut self, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        let lba = match self.resolve_lock(args[0]) {
            Some(l) => l,
            None => return (DOSFALSE, err::OBJECT_NOT_FOUND),
        };
        let root = self.volume().root_lba();
        let (dir_entry_type, name, protection, size, date, comment, owner, is_dir) = if lba == root
        {
            let (name, date) = {
                let r = self.volume().root();
                (r.name.clone(), r.dir_altered)
            };
            (ST_ROOT, name, 0u32, 0u32, date, Vec::new(), 0u32, true)
        } else {
            let entry = match self.volume().entry_at(lba) {
                Ok(e) => e,
                Err(e) => return (DOSFALSE, map_read_error(&e)),
            };
            let comment = match self.volume().comment(&entry) {
                Ok(c) => c,
                Err(e) => return (DOSFALSE, map_read_error(&e)),
            };
            let is_dir = entry.kind.is_directory();
            (
                entry.kind.secondary_type(),
                entry.name,
                entry.protection,
                entry.byte_size,
                entry.date,
                comment,
                entry.owner,
                is_dir,
            )
        };
        let num_blocks = self.count_blocks(lba, is_dir);
        let fib = match mem.ram_slice_mut(args[1].wrapping_mul(4), FIB_SIZE) {
            Some(f) => f,
            None => return (DOSFALSE, err::BAD_NUMBER),
        };
        fill_fib(
            fib,
            0,
            dir_entry_type,
            &name,
            protection,
            size,
            num_blocks,
            date,
            &comment,
            owner,
        );
        (DOSTRUE, 0)
    }

    fn examine_next(&mut self, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        let dir_lba = match self.resolve_lock(args[0]) {
            Some(l) => l,
            None => return (DOSFALSE, err::OBJECT_NOT_FOUND),
        };
        let fib_addr = args[1].wrapping_mul(4);
        let key = match mem.ram_slice(fib_addr, 4) {
            Some(s) => u32::from_be_bytes(s.try_into().expect("ram_slice(_, 4) returns 4 bytes")),
            None => return (DOSFALSE, err::BAD_NUMBER),
        };
        let entries = match self.volume().read_dir(dir_lba) {
            Ok(e) => e,
            Err(e) => return (DOSFALSE, map_read_error(&e)),
        };
        let index = key as usize;
        if index >= entries.len() {
            return (DOSFALSE, err::NO_MORE_ENTRIES);
        }
        let entry = &entries[index];
        let lba = entry.lba;
        let is_dir = entry.kind.is_directory();
        let comment = match self.volume().comment(entry) {
            Ok(c) => c,
            Err(e) => return (DOSFALSE, map_read_error(&e)),
        };
        let (dir_entry_type, name, protection, size, date, owner) = {
            let entry = &entries[index];
            (
                entry.kind.secondary_type(),
                entry.name.clone(),
                entry.protection,
                entry.byte_size,
                entry.date,
                entry.owner,
            )
        };
        let num_blocks = self.count_blocks(lba, is_dir);
        let fib = match mem.ram_slice_mut(fib_addr, FIB_SIZE) {
            Some(f) => f,
            None => return (DOSFALSE, err::BAD_NUMBER),
        };
        fill_fib(
            fib,
            (index + 1) as u32,
            dir_entry_type,
            &name,
            protection,
            size,
            num_blocks,
            date,
            &comment,
            owner,
        );
        (DOSTRUE, 0)
    }

    fn fill_disk_info(&mut self, info_bptr: u32, mem: &mut dyn GuestMemory) -> (u32, u32) {
        let block_count = self.volume().block_count();
        let block_size = self.volume().block_size() as u32;
        let dos_type = self.volume().variant().dostype();
        let used = self.blocks_used();
        let writable = self.writable;
        let info = match mem.ram_slice_mut(info_bptr.wrapping_mul(4), INFO_DATA_SIZE) {
            Some(b) => b,
            None => return (DOSFALSE, err::BAD_NUMBER),
        };
        fill_info_data(info, block_count, used, block_size, dos_type, writable);
        (DOSTRUE, 0)
    }

    fn info(&mut self, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        // The lock arg selects which of a handler's several volumes to
        // describe; this backend serves exactly one (`VOL_COUNT` = 1),
        // so it is not consulted -- `DISK_INFO` already answers the same
        // question for the one volume there is.
        self.fill_disk_info(args[1], mem)
    }

    fn disk_info(&mut self, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        self.fill_disk_info(args[0], mem)
    }

    fn open_for_find(
        &mut self,
        args: [u32; 7],
        mem: &mut dyn GuestMemory,
    ) -> Result<(u64, Vec<u8>), (u32, u32)> {
        let dir_lba = self
            .resolve_lock(args[1])
            .ok_or((DOSFALSE, err::OBJECT_NOT_FOUND))?;
        let name = match read_bstr(mem, args[2]) {
            Some(n) if !n.is_empty() => n,
            _ => return Err((DOSFALSE, err::INVALID_COMPONENT_NAME)),
        };
        self.split_parent(dir_lba, &name)
            .map_err(|code| (DOSFALSE, code))
    }

    fn find_input(&mut self, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        let (parent_lba, leaf) = match self.open_for_find(args, mem) {
            Ok(p) => p,
            Err(res) => return res,
        };
        match self.volume().lookup(parent_lba, &leaf) {
            Ok(Some(entry)) if entry.kind == EntryKind::File => {
                let h = self.alloc_handle(Handle::File(OpenFile {
                    parent_lba,
                    name: leaf,
                    header_lba: entry.lba,
                    position: 0,
                    // MODE_OLDFILE is *not* read-only: AmigaDOS permits
                    // writing through a FINDINPUT handle (DiskSpeed 4.2's
                    // own Write test does exactly this -- Open(...,
                    // MODE_OLDFILE), Seek(0), Write). Volume-level
                    // read-only still refuses in write_action via the
                    // mutator gate.
                    write: self.writable,
                }));
                (DOSTRUE, h)
            }
            Ok(Some(_)) => (DOSFALSE, err::OBJECT_WRONG_TYPE),
            Ok(None) => (DOSFALSE, err::OBJECT_NOT_FOUND),
            Err(e) => (DOSFALSE, map_read_error(&e)),
        }
    }

    fn find_output(&mut self, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        if !self.writable {
            return (DOSFALSE, err::WRITE_PROTECTED);
        }
        let (parent_lba, leaf) = match self.open_for_find(args, mem) {
            Ok(p) => p,
            Err(res) => return res,
        };
        let existing = match self.volume().lookup(parent_lba, &leaf) {
            Ok(e) => e,
            Err(e) => return (DOSFALSE, map_read_error(&e)),
        };
        match existing {
            Some(entry) if entry.kind != EntryKind::File => (DOSFALSE, err::OBJECT_WRONG_TYPE),
            Some(entry) => {
                let mutator = self.mutator_mut().expect("writable checked above");
                match mutator.truncate(parent_lba, &leaf, 0) {
                    Ok(()) => {
                        let h = self.alloc_handle(Handle::File(OpenFile {
                            parent_lba,
                            name: leaf,
                            header_lba: entry.lba,
                            position: 0,
                            write: true,
                        }));
                        (DOSTRUE, h)
                    }
                    Err(e) => (DOSFALSE, map_mutate_error(&e)),
                }
            }
            None => {
                let mutator = self.mutator_mut().expect("writable checked above");
                match mutator.create_file(parent_lba, &leaf, &Metadata::new(), &[]) {
                    Ok(lba) => {
                        let h = self.alloc_handle(Handle::File(OpenFile {
                            parent_lba,
                            name: leaf,
                            header_lba: lba,
                            position: 0,
                            write: true,
                        }));
                        (DOSTRUE, h)
                    }
                    Err(e) => (DOSFALSE, map_mutate_error(&e)),
                }
            }
        }
    }

    fn find_update(&mut self, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        if !self.writable {
            return (DOSFALSE, err::WRITE_PROTECTED);
        }
        let (parent_lba, leaf) = match self.open_for_find(args, mem) {
            Ok(p) => p,
            Err(res) => return res,
        };
        let existing = match self.volume().lookup(parent_lba, &leaf) {
            Ok(e) => e,
            Err(e) => return (DOSFALSE, map_read_error(&e)),
        };
        match existing {
            Some(entry) if entry.kind != EntryKind::File => (DOSFALSE, err::OBJECT_WRONG_TYPE),
            Some(entry) => {
                let h = self.alloc_handle(Handle::File(OpenFile {
                    parent_lba,
                    name: leaf,
                    header_lba: entry.lba,
                    position: 0,
                    write: true,
                }));
                (DOSTRUE, h)
            }
            None => {
                let mutator = self.mutator_mut().expect("writable checked above");
                match mutator.create_file(parent_lba, &leaf, &Metadata::new(), &[]) {
                    Ok(lba) => {
                        let h = self.alloc_handle(Handle::File(OpenFile {
                            parent_lba,
                            name: leaf,
                            header_lba: lba,
                            position: 0,
                            write: true,
                        }));
                        (DOSTRUE, h)
                    }
                    Err(e) => (DOSFALSE, map_mutate_error(&e)),
                }
            }
        }
    }

    fn end(&mut self, args: [u32; 7]) -> (u32, u32) {
        match self.handles.get(&args[0]) {
            Some(Handle::File(_)) => {
                self.handles.remove(&args[0]);
                (DOSTRUE, 0)
            }
            _ => (DOSFALSE, err::OBJECT_NOT_FOUND),
        }
    }

    fn read_action(&mut self, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        let (header_lba, pos) = match self.handles.get(&args[0]) {
            Some(Handle::File(f)) => (f.header_lba, f.position),
            _ => return (DOSFALSE, err::OBJECT_NOT_FOUND),
        };
        let chain = match self.volume().file_chain(header_lba) {
            Ok(c) => c,
            Err(e) => return (READ_WRITE_ERROR, map_read_error(&e)),
        };
        let out = match mem.ram_slice_mut(args[1], args[2]) {
            Some(b) => b,
            None => return (READ_WRITE_ERROR, err::BAD_NUMBER),
        };
        let n = match self.volume().read_range(&chain, pos, out) {
            Ok(n) => n,
            Err(e) => return (READ_WRITE_ERROR, map_read_error(&e)),
        };
        if let Some(Handle::File(f)) = self.handles.get_mut(&args[0]) {
            f.position += n as u64;
        }
        (n as u32, 0)
    }

    fn write_action(&mut self, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        let (parent_lba, name, pos, can_write) = match self.handles.get(&args[0]) {
            Some(Handle::File(f)) => (f.parent_lba, f.name.clone(), f.position, f.write),
            _ => return (DOSFALSE, err::OBJECT_NOT_FOUND),
        };
        if !can_write {
            return (READ_WRITE_ERROR, err::WRITE_PROTECTED_FILE);
        }
        let data = match mem.ram_slice(args[1], args[2]) {
            Some(b) => b.to_vec(),
            None => return (READ_WRITE_ERROR, err::BAD_NUMBER),
        };
        let mutator = match self.mutator_mut() {
            Some(m) => m,
            None => return (READ_WRITE_ERROR, err::WRITE_PROTECTED),
        };
        match mutator.write_file(parent_lba, &name, pos, &data) {
            Ok(_) => {
                let written = data.len() as u64;
                if let Some(Handle::File(f)) = self.handles.get_mut(&args[0]) {
                    f.position += written;
                }
                (written as u32, 0)
            }
            Err(e) => (READ_WRITE_ERROR, map_mutate_error(&e)),
        }
    }

    fn seek_action(&mut self, args: [u32; 7]) -> (u32, u32) {
        let (header_lba, old_pos) = match self.handles.get(&args[0]) {
            Some(Handle::File(f)) => (f.header_lba, f.position),
            _ => return (READ_WRITE_ERROR, err::OBJECT_NOT_FOUND),
        };
        let size = match self.volume().file_chain(header_lba) {
            Ok(c) => c.byte_size as i64,
            Err(e) => return (READ_WRITE_ERROR, map_read_error(&e)),
        };
        let offset = args[1] as i32 as i64;
        let mode = args[2] as i32;
        let base: i64 = match mode {
            -1 => 0,             // OFFSET_BEGINNING
            0 => old_pos as i64, // OFFSET_CURRENT
            1 => size,           // OFFSET_END
            _ => return (READ_WRITE_ERROR, err::BAD_NUMBER),
        };
        // Clamped to [0, size] rather than refused: real AmigaDOS Seek
        // refuses a position past the end, but this protocol version's
        // required-error table (§6) has no seek-specific code to answer
        // that refusal with, and clamping is safe (it can never place the
        // cursor somewhere a subsequent Read/Write would misbehave).
        let new_pos = (base + offset).clamp(0, size) as u64;
        if let Some(Handle::File(f)) = self.handles.get_mut(&args[0]) {
            f.position = new_pos;
        }
        (old_pos as u32, 0)
    }

    fn set_file_size(&mut self, args: [u32; 7]) -> (u32, u32) {
        let (parent_lba, name, header_lba, pos) = match self.handles.get(&args[0]) {
            Some(Handle::File(f)) => (f.parent_lba, f.name.clone(), f.header_lba, f.position),
            _ => return (READ_WRITE_ERROR, err::OBJECT_NOT_FOUND),
        };
        if !self.writable {
            return (READ_WRITE_ERROR, err::WRITE_PROTECTED_FILE);
        }
        let size = match self.volume().file_chain(header_lba) {
            Ok(c) => c.byte_size as i64,
            Err(e) => return (READ_WRITE_ERROR, map_read_error(&e)),
        };
        let offset = args[1] as i32 as i64;
        let mode = args[2] as i32;
        let base: i64 = match mode {
            -1 => 0,
            0 => pos as i64,
            1 => size,
            _ => return (READ_WRITE_ERROR, err::BAD_NUMBER),
        };
        let new_size = (base + offset).max(0) as u64;
        let mutator = match self.mutator_mut() {
            Some(m) => m,
            None => return (READ_WRITE_ERROR, err::WRITE_PROTECTED),
        };
        match mutator.truncate(parent_lba, &name, new_size) {
            Ok(()) => (new_size as u32, 0),
            Err(e) => (READ_WRITE_ERROR, map_mutate_error(&e)),
        }
    }

    fn create_dir_action(&mut self, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        if !self.writable {
            return (0, err::WRITE_PROTECTED);
        }
        let dir_lba = match self.resolve_lock(args[0]) {
            Some(l) => l,
            None => return (0, err::OBJECT_NOT_FOUND),
        };
        let name = match read_bstr(mem, args[1]) {
            Some(n) if !n.is_empty() => n,
            _ => return (0, err::INVALID_COMPONENT_NAME),
        };
        let (parent_lba, leaf) = match self.split_parent(dir_lba, &name) {
            Ok(p) => p,
            Err(code) => return (0, code),
        };
        let mutator = self.mutator_mut().expect("writable checked above");
        match mutator.create_dir(parent_lba, &leaf, &Metadata::new()) {
            Ok(lba) => (self.alloc_handle(Handle::Lock(lba)), 0),
            Err(e) => (0, map_mutate_error(&e)),
        }
    }

    fn delete_object(&mut self, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        if !self.writable {
            return (DOSFALSE, err::WRITE_PROTECTED);
        }
        let dir_lba = match self.resolve_lock(args[0]) {
            Some(l) => l,
            None => return (DOSFALSE, err::OBJECT_NOT_FOUND),
        };
        let name = match read_bstr(mem, args[1]) {
            Some(n) if !n.is_empty() => n,
            _ => return (DOSFALSE, err::INVALID_COMPONENT_NAME),
        };
        let (parent_lba, leaf) = match self.split_parent(dir_lba, &name) {
            Ok(p) => p,
            Err(code) => return (DOSFALSE, code),
        };
        let mutator = self.mutator_mut().expect("writable checked above");
        match mutator.delete(parent_lba, &leaf) {
            Ok(_) => (DOSTRUE, 0),
            Err(e) => (DOSFALSE, map_mutate_error(&e)),
        }
    }

    fn rename_object(&mut self, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        if !self.writable {
            return (DOSFALSE, err::WRITE_PROTECTED);
        }
        let src_dir = match self.resolve_lock(args[0]) {
            Some(l) => l,
            None => return (DOSFALSE, err::OBJECT_NOT_FOUND),
        };
        let src_name = match read_bstr(mem, args[1]) {
            Some(n) if !n.is_empty() => n,
            _ => return (DOSFALSE, err::INVALID_COMPONENT_NAME),
        };
        let dst_dir = match self.resolve_lock(args[2]) {
            Some(l) => l,
            None => return (DOSFALSE, err::OBJECT_NOT_FOUND),
        };
        let dst_name = match read_bstr(mem, args[3]) {
            Some(n) if !n.is_empty() => n,
            _ => return (DOSFALSE, err::INVALID_COMPONENT_NAME),
        };
        let (src_parent, src_leaf) = match self.split_parent(src_dir, &src_name) {
            Ok(p) => p,
            Err(code) => return (DOSFALSE, code),
        };
        let (dst_parent, dst_leaf) = match self.split_parent(dst_dir, &dst_name) {
            Ok(p) => p,
            Err(code) => return (DOSFALSE, code),
        };
        let mutator = self.mutator_mut().expect("writable checked above");
        match mutator.rename(src_parent, &src_leaf, dst_parent, &dst_leaf) {
            Ok(()) => (DOSTRUE, 0),
            Err(e) => (DOSFALSE, map_mutate_error(&e)),
        }
    }

    /// Resolve the target of `SET_PROTECT`/`SET_COMMENT`/`SET_DATE`: an
    /// empty name means "the lock itself" (real AmigaDOS's convention
    /// for these three actions), anything else is a path relative to it.
    fn resolve_meta_target(&mut self, dir_lba: u64, name: &[u8]) -> Result<u64, u32> {
        if name.is_empty() {
            return Ok(dir_lba);
        }
        let (parent, leaf) = self.split_parent(dir_lba, name)?;
        match self.volume().lookup(parent, &leaf) {
            Ok(Some(e)) => Ok(e.lba),
            Ok(None) => Err(err::OBJECT_NOT_FOUND),
            Err(e) => Err(map_read_error(&e)),
        }
    }

    fn set_protect(&mut self, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        if !self.writable {
            return (DOSFALSE, err::WRITE_PROTECTED);
        }
        let dir_lba = match self.resolve_lock(args[1]) {
            Some(l) => l,
            None => return (DOSFALSE, err::OBJECT_NOT_FOUND),
        };
        let name = match read_bstr(mem, args[2]) {
            Some(n) => n,
            None => return (DOSFALSE, err::INVALID_COMPONENT_NAME),
        };
        let target = match self.resolve_meta_target(dir_lba, &name) {
            Ok(t) => t,
            Err(code) => return (DOSFALSE, code),
        };
        let mask = args[3];
        let mutator = self.mutator_mut().expect("writable checked above");
        match mutator.set_metadata(target, &MetaUpdate::new().protection(mask)) {
            Ok(()) => (DOSTRUE, 0),
            Err(e) => (DOSFALSE, map_mutate_error(&e)),
        }
    }

    fn set_comment(&mut self, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        if !self.writable {
            return (DOSFALSE, err::WRITE_PROTECTED);
        }
        let dir_lba = match self.resolve_lock(args[1]) {
            Some(l) => l,
            None => return (DOSFALSE, err::OBJECT_NOT_FOUND),
        };
        let name = match read_bstr(mem, args[2]) {
            Some(n) => n,
            None => return (DOSFALSE, err::INVALID_COMPONENT_NAME),
        };
        let target = match self.resolve_meta_target(dir_lba, &name) {
            Ok(t) => t,
            Err(code) => return (DOSFALSE, code),
        };
        let comment = match read_bstr(mem, args[3]) {
            Some(c) => c,
            None => return (DOSFALSE, err::BAD_NUMBER),
        };
        let mutator = self.mutator_mut().expect("writable checked above");
        match mutator.set_metadata(target, &MetaUpdate::new().comment(&comment)) {
            Ok(()) => (DOSTRUE, 0),
            Err(e) => (DOSFALSE, map_mutate_error(&e)),
        }
    }

    fn set_date(&mut self, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        if !self.writable {
            return (DOSFALSE, err::WRITE_PROTECTED);
        }
        let dir_lba = match self.resolve_lock(args[1]) {
            Some(l) => l,
            None => return (DOSFALSE, err::OBJECT_NOT_FOUND),
        };
        let name = match read_bstr(mem, args[2]) {
            Some(n) => n,
            None => return (DOSFALSE, err::INVALID_COMPONENT_NAME),
        };
        let target = match self.resolve_meta_target(dir_lba, &name) {
            Ok(t) => t,
            Err(code) => return (DOSFALSE, code),
        };
        // `DateStamp APTR`: a real address, not a BPTR (§5's own wording),
        // pointing at three big-endian longs: days, mins, ticks.
        let stamp = match mem.ram_slice(args[3], 12) {
            Some(b) => DateStamp {
                days: u32::from_be_bytes(b[0..4].try_into().expect("4 bytes")),
                mins: u32::from_be_bytes(b[4..8].try_into().expect("4 bytes")),
                ticks: u32::from_be_bytes(b[8..12].try_into().expect("4 bytes")),
            },
            None => return (DOSFALSE, err::BAD_NUMBER),
        };
        let mutator = self.mutator_mut().expect("writable checked above");
        match mutator.set_metadata(target, &MetaUpdate::new().date(stamp)) {
            Ok(()) => (DOSTRUE, 0),
            Err(e) => (DOSFALSE, map_mutate_error(&e)),
        }
    }

    /// `ACTION_FLUSH`: best-effort `sync_data` on the underlying file.
    /// Every write already lands on the medium immediately (`amiga-ffs`
    /// writes through, nothing here buffers), so this has no bookkeeping
    /// of its own to do; a failed `sync_data` (e.g. a device that does
    /// not support it) is not reported as a filesystem error, since the
    /// bytes it would have flushed are already correct on the medium as
    /// far as this backend's own model of it goes.
    fn flush(&mut self) -> (u32, u32) {
        if let Backing::Writable(m) = &mut self.backing {
            let _ = m.volume().source_mut().file.sync_data();
        }
        (DOSTRUE, 0)
    }

    /// `ACTION_INHIBIT`, Arg1 nonzero = inhibit, zero = un-inhibit --
    /// AmigaDOS's own convention is a plain `BOOL`, not the `DOSTRUE`/
    /// `DOSFALSE` result convention, so any nonzero value counts.
    ///
    /// Inhibiting: flushes to durable storage, drops every open handle
    /// (they name structures about to be invalidated by the `FORMAT`
    /// this is almost always in service of), and enters the inhibited
    /// state. Refused with `ERROR_WRITE_PROTECTED` (214) on a read-only
    /// volume -- there is no format this backend could perform after
    /// inhibiting it anyway, and refusing here is the "sensible" refusal
    /// the read-only INHIBIT-for-format flow needs, before a caller ever
    /// reaches `ACTION_FORMAT`'s own (redundant, defence-in-depth) check.
    /// Idempotent: inhibiting an already-inhibited volume just succeeds.
    ///
    /// Un-inhibiting: remounts (see [`Self::remount`]) and leaves the
    /// inhibited state on success. Idempotent the same way: un-inhibiting
    /// a volume that was never inhibited succeeds without remounting.
    fn inhibit_action(&mut self, args: [u32; 7]) -> (u32, u32) {
        let want_inhibit = args[0] != 0;
        if want_inhibit {
            if self.inhibited {
                return (DOSTRUE, 0);
            }
            if !self.writable {
                return (DOSFALSE, err::WRITE_PROTECTED);
            }
            let _ = self.flush();
            self.handles.clear();
            self.inhibited = true;
            (DOSTRUE, 0)
        } else {
            if !self.inhibited {
                return (DOSTRUE, 0);
            }
            match self.remount() {
                Ok(()) => {
                    self.inhibited = false;
                    (DOSTRUE, 0)
                }
                Err(code) => (DOSFALSE, code),
            }
        }
    }

    /// `ACTION_FORMAT`, Arg1 = volume-name BSTR, Arg2 = dostype. Legal
    /// only while `ACTION_INHIBIT(TRUE)`'d -- otherwise
    /// `ERROR_OBJECT_IN_USE` (202), matching real AmigaDOS handlers that
    /// refuse a low-level format on a mounted, un-inhibited volume.
    ///
    /// Only QUICK format is meaningful here: there is no block device
    /// under this handler for a full low-level pass to write to (see
    /// docs/pktport-protocol.md §5's ACTION_INHIBIT/FORMAT section) --
    /// `amiga-ffs::format` *is* the QUICK-format operation, laying down
    /// a fresh empty filesystem without touching anything below the
    /// filesystem layer, which is exactly right since there is nothing
    /// below it to touch.
    ///
    /// Reformats in place through the medium the current (still open)
    /// `Mutator` already holds -- no remount here; that is
    /// `ACTION_INHIBIT(FALSE)`'s job, matching real `C:Format`'s
    /// INHIBIT(TRUE) -> FORMAT -> INHIBIT(FALSE) sequence.
    fn format_action(&mut self, args: [u32; 7], mem: &mut dyn GuestMemory) -> (u32, u32) {
        if !self.inhibited {
            return (DOSFALSE, err::OBJECT_IN_USE);
        }
        if !self.writable {
            return (DOSFALSE, err::WRITE_PROTECTED);
        }
        let name = match read_bstr(mem, args[0]) {
            Some(n) if !n.is_empty() => n,
            _ => return (DOSFALSE, err::INVALID_COMPONENT_NAME),
        };
        let dostype = args[1];
        let variant = match Variant::from_dostype(dostype) {
            Some(v) => v,
            // A dostype this crate cannot format (not a DOS\0-DOS\7
            // member `amiga-ffs::Variant` recognises) -> "not a DOS
            // disk", the same code the read side uses for a dostype it
            // cannot mount.
            None => return (DOSFALSE, err::NOT_A_DOS_DISK),
        };
        let opts = FormatOptions::new(variant, self.len_blocks, &name).reserved(self.reserved);
        let mutator = match self.mutator_mut() {
            Some(m) => m,
            // Unreachable in practice: `self.writable` was just checked
            // and `Backing::Writable` is exactly what `self.writable`
            // means. Kept as a real refusal rather than an `expect`,
            // since panicking a filesystem backend on any input shape is
            // exactly what §7's hostility rules forbid.
            None => return (DOSFALSE, err::WRITE_PROTECTED),
        };
        let medium = mutator.volume().source_mut();
        match ffs_format(medium, &opts) {
            Ok(_) => (DOSTRUE, 0),
            Err(e) => (DOSFALSE, map_format_error(&e)),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use amiga_ffs::{format, populate::Populator, FormatOptions};
    use std::io::Cursor;

    // -- a minimal fake GuestMemory over a Vec<u8>, mirroring the shape
    // machine-core's own hostblk tests use for exactly the same reason:
    // driving the backend without standing up a whole MachineBus. ------

    struct FakeMem(Vec<u8>);

    impl GuestMemory for FakeMem {
        fn ram_slice(&self, addr: u32, len: u32) -> Option<&[u8]> {
            let end = addr.checked_add(len)?;
            if end as usize > self.0.len() {
                return None;
            }
            Some(&self.0[addr as usize..end as usize])
        }
        fn ram_slice_mut(&mut self, addr: u32, len: u32) -> Option<&mut [u8]> {
            let end = addr.checked_add(len)?;
            if end as usize > self.0.len() {
                return None;
            }
            Some(&mut self.0[addr as usize..end as usize])
        }
    }

    const RAM_SIZE: usize = 64 * 1024;
    fn ram() -> FakeMem {
        FakeMem(vec![0u8; RAM_SIZE])
    }

    /// Write a BSTR at `addr` in `ram`, returning the BPTR to pass on the
    /// wire (`addr / 4`; every address used here is 4-aligned).
    fn put_bstr(ram: &mut FakeMem, addr: u32, s: &[u8]) -> u32 {
        assert!(addr.is_multiple_of(4));
        ram.0[addr as usize] = s.len() as u8;
        ram.0[addr as usize + 1..addr as usize + 1 + s.len()].copy_from_slice(s);
        addr / 4
    }

    // -- an in-memory BlockSource/BlockSink for building test volumes,
    // independent of the production FileMedium (test-only). ------------

    struct MemMedium {
        data: Vec<u8>,
        block_size: usize,
    }

    impl MemMedium {
        fn new(blocks: u64, block_size: usize) -> Self {
            Self {
                data: vec![0u8; blocks as usize * block_size],
                block_size,
            }
        }
    }

    impl amiga_ffs::BlockSource for MemMedium {
        type Error = std::convert::Infallible;
        fn block_size(&self) -> usize {
            self.block_size
        }
        fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
            let off = lba as usize * self.block_size;
            buf.copy_from_slice(&self.data[off..off + self.block_size]);
            Ok(())
        }
        fn block_count(&self) -> Option<u64> {
            Some(self.data.len() as u64 / self.block_size as u64)
        }
    }

    impl amiga_ffs::BlockSink for MemMedium {
        type Error = std::convert::Infallible;
        fn block_size(&self) -> usize {
            self.block_size
        }
        fn write_block(&mut self, lba: u64, buf: &[u8]) -> Result<(), Self::Error> {
            let off = lba as usize * self.block_size;
            self.data[off..off + self.block_size].copy_from_slice(buf);
            Ok(())
        }
        fn block_count(&self) -> Option<u64> {
            Some(self.data.len() as u64 / self.block_size as u64)
        }
    }

    const BLOCK_SIZE: usize = 512;
    const BLOCKS: u64 = 2000;
    const BIG_FILE_LEN: usize = BLOCK_SIZE * 3 + 17;

    /// Build a bare (RDB-less) FFS volume with a known layout, write it
    /// to a fresh temp file, and return its path.
    ///
    /// Contents: `readme.txt` (known text), a `sub` directory holding
    /// `nested.txt`, a Latin-1-named file, `secret.txt` with protection
    /// bits and a comment set, and `big.dat` spanning multiple 512-byte
    /// FFS data blocks (no per-block header, so "multiple blocks" is
    /// exactly "more than 512 bytes").
    fn build_test_volume(writable_note: &str) -> std::path::PathBuf {
        let opts = FormatOptions::new(Variant::Ffs, BLOCKS, b"TestVol");
        let medium = MemMedium::new(BLOCKS, BLOCK_SIZE);
        let mut pop = Populator::new(medium, &opts).unwrap();
        let root = pop.root_lba();

        pop.create_file(root, b"readme.txt", &Metadata::new(), b"hello, pktvol")
            .unwrap();

        let sub = pop.create_dir(root, b"sub", &Metadata::new()).unwrap();
        pop.create_file(sub, b"nested.txt", &Metadata::new(), b"nested content")
            .unwrap();

        // Latin-1 name: 'e' with an acute accent (0xE9) is not ASCII and
        // not valid UTF-8 on its own -- exactly the case a reader that
        // assumes UTF-8 mishandles.
        let latin1_name: &[u8] = &[0xE9, b'd', b'i', b't', b'.', b't', b'x', b't'];
        pop.create_file(root, latin1_name, &Metadata::new(), b"accented name")
            .unwrap();

        pop.create_file(
            root,
            b"secret.txt",
            &Metadata::new()
                .protection(amiga_ffs::meta::FIBF_ARCHIVE)
                .comment(b"a test comment"),
            b"secret data",
        )
        .unwrap();

        let big = vec![0x5Au8; BIG_FILE_LEN];
        pop.create_file(root, b"big.dat", &Metadata::new(), &big)
            .unwrap();

        let medium = pop.finish().unwrap();

        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "machine-hosted-pktvol-test-{}-{n}-{writable_note}.hdf",
            std::process::id(),
        ));
        std::fs::write(&path, &medium.data).unwrap();
        path
    }

    fn open_ro() -> (PktVolume, std::path::PathBuf) {
        let path = build_test_volume("ro");
        let vol = PktVolume::open(&path, false, None).unwrap();
        (vol, path)
    }

    fn open_rw() -> (PktVolume, std::path::PathBuf) {
        let path = build_test_volume("rw");
        let vol = PktVolume::open(&path, true, None).unwrap();
        (vol, path)
    }

    fn args1(a1: u32) -> [u32; 7] {
        [a1, 0, 0, 0, 0, 0, 0]
    }
    fn args2(a1: u32, a2: u32) -> [u32; 7] {
        [a1, a2, 0, 0, 0, 0, 0]
    }
    fn args3(a1: u32, a2: u32, a3: u32) -> [u32; 7] {
        [a1, a2, a3, 0, 0, 0, 0]
    }
    fn args4(a1: u32, a2: u32, a3: u32, a4: u32) -> [u32; 7] {
        [a1, a2, a3, a4, 0, 0, 0]
    }

    #[test]
    fn locate_and_examine_root() {
        let (mut vol, path) = open_ro();
        let mut mem = ram();
        let (res1, res2) = vol.execute(action::EXAMINE_OBJECT, args2(0, 0x1000 / 4), &mut mem);
        assert_eq!((res1, res2), (DOSTRUE, 0));
        let fib_addr = 0x1000;
        assert_eq!(mem.0[fib_addr + 4..fib_addr + 8], ST_ROOT.to_be_bytes());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn locate_object_by_path() {
        let (mut vol, path) = open_ro();
        let mut mem = ram();
        let name_bptr = put_bstr(&mut mem, 0x2000, b"sub/nested.txt");
        let (res1, res2) = vol.execute(action::LOCATE_OBJECT, args3(0, name_bptr, 0), &mut mem);
        assert_eq!(res2, 0);
        assert_ne!(res1, 0);

        // EXAMINE the resulting lock and check the FIB fields land at the
        // documented offsets, big-endian, BCPL name.
        let fib_bptr = 0x3000 / 4;
        let (r1, r2) = vol.execute(action::EXAMINE_OBJECT, args2(res1, fib_bptr), &mut mem);
        assert_eq!((r1, r2), (DOSTRUE, 0));
        let fib = 0x3000;
        assert_eq!(mem.0[fib + 8], b"nested.txt".len() as u8);
        assert_eq!(&mem.0[fib + 9..fib + 9 + 10], b"nested.txt");
        assert_eq!(
            u32::from_be_bytes(mem.0[fib + 124..fib + 128].try_into().unwrap()),
            b"nested content".len() as u32
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn locate_object_not_found() {
        let (mut vol, path) = open_ro();
        let mut mem = ram();
        let name_bptr = put_bstr(&mut mem, 0x2000, b"does-not-exist");
        let (res1, res2) = vol.execute(action::LOCATE_OBJECT, args3(0, name_bptr, 0), &mut mem);
        assert_eq!((res1, res2), (DOSFALSE, err::OBJECT_NOT_FOUND));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn examine_next_full_directory_enumeration() {
        let (mut vol, path) = open_ro();
        let mut mem = ram();
        let fib_bptr = 0x4000 / 4;
        let mut names: Vec<Vec<u8>> = Vec::new();
        loop {
            let (res1, res2) = vol.execute(action::EXAMINE_NEXT, args2(0, fib_bptr), &mut mem);
            if res2 == err::NO_MORE_ENTRIES {
                assert_eq!(res1, DOSFALSE);
                break;
            }
            assert_eq!((res1, res2), (DOSTRUE, 0));
            let fib = 0x4000;
            let len = mem.0[fib + 8] as usize;
            names.push(mem.0[fib + 9..fib + 9 + len].to_vec());
        }
        let expected: Vec<Vec<u8>> = vec![
            b"readme.txt".to_vec(),
            b"sub".to_vec(),
            vec![0xE9, b'd', b'i', b't', b'.', b't', b'x', b't'],
            b"secret.txt".to_vec(),
            b"big.dat".to_vec(),
        ];
        assert_eq!(names.len(), expected.len(), "every root entry seen once");
        for name in &expected {
            assert_eq!(names.iter().filter(|n| *n == name).count(), 1, "{name:?}");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn findinput_read_seek_end() {
        let (mut vol, path) = open_ro();
        let mut mem = ram();
        let name_bptr = put_bstr(&mut mem, 0x2000, b"big.dat");
        let (res1, handle) = vol.execute(action::FINDINPUT, args3(0, 0, name_bptr), &mut mem);
        // RES1 is the FIND* success discriminator (protocol doc §5's
        // dagger note) -- pinned here because the first revision left it
        // 0 on success, forcing the stub to classify RES2 by
        // error-table membership, which collides with handle 205.
        assert_eq!(res1, DOSTRUE);
        assert_ne!(handle, 0);

        // A read spanning multiple internal (512-byte) FFS blocks.
        let buf_addr = 0x8000u32;
        let (n, res2) = vol.execute(
            action::READ,
            args3(handle, buf_addr, BIG_FILE_LEN as u32),
            &mut mem,
        );
        assert_eq!(res2, 0);
        assert_eq!(n as usize, BIG_FILE_LEN);
        assert!(mem.0[buf_addr as usize..buf_addr as usize + BIG_FILE_LEN]
            .iter()
            .all(|&b| b == 0x5A));

        // SEEK, all three modes: absolute (OFFSET_BEGINNING = -1),
        // relative (OFFSET_CURRENT = 0), and from the end
        // (OFFSET_END = 1).
        let (old, res2) = vol.execute(
            action::SEEK,
            args3(handle, 10, 0xFFFF_FFFF /* -1 */),
            &mut mem,
        );
        assert_eq!(res2, 0);
        assert_eq!(old, BIG_FILE_LEN as u32);
        let (old, res2) = vol.execute(action::SEEK, args3(handle, 5, 0), &mut mem);
        assert_eq!(res2, 0);
        assert_eq!(old, 10);
        let (old, res2) = vol.execute(action::SEEK, args3(handle, 0, 1), &mut mem);
        assert_eq!(res2, 0);
        assert_eq!(old, 15);

        let (res1, res2) = vol.execute(action::END, args1(handle), &mut mem);
        assert_eq!((res1, res2), (DOSTRUE, 0));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn findoutput_write_then_read_back() {
        let (mut vol, path) = open_rw();
        let mut mem = ram();
        let name_bptr = put_bstr(&mut mem, 0x2000, b"new.txt");
        let (_, handle) = vol.execute(action::FINDOUTPUT, args3(0, 0, name_bptr), &mut mem);
        assert_ne!(handle, 0);

        let data = b"written through pktvol";
        let buf_addr = 0x9000u32;
        mem.0[buf_addr as usize..buf_addr as usize + data.len()].copy_from_slice(data);
        let (n, res2) = vol.execute(
            action::WRITE,
            args3(handle, buf_addr, data.len() as u32),
            &mut mem,
        );
        assert_eq!(res2, 0);
        assert_eq!(n as usize, data.len());
        vol.execute(action::END, args1(handle), &mut mem);

        let name_bptr = put_bstr(&mut mem, 0x2100, b"new.txt");
        let (_, handle) = vol.execute(action::FINDINPUT, args3(0, 0, name_bptr), &mut mem);
        assert_ne!(handle, 0);
        let read_addr = 0xA000u32;
        let (n, res2) = vol.execute(
            action::READ,
            args3(handle, read_addr, data.len() as u32),
            &mut mem,
        );
        assert_eq!(res2, 0);
        assert_eq!(n as usize, data.len());
        assert_eq!(
            &mem.0[read_addr as usize..read_addr as usize + data.len()],
            data
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn delete_object_removes_entry() {
        let (mut vol, path) = open_rw();
        let mut mem = ram();
        let name_bptr = put_bstr(&mut mem, 0x2000, b"readme.txt");
        let (res1, res2) = vol.execute(action::DELETE_OBJECT, args2(0, name_bptr), &mut mem);
        assert_eq!((res1, res2), (DOSTRUE, 0));
        let name_bptr = put_bstr(&mut mem, 0x2100, b"readme.txt");
        let (res1, res2) = vol.execute(action::LOCATE_OBJECT, args3(0, name_bptr, 0), &mut mem);
        assert_eq!((res1, res2), (DOSFALSE, err::OBJECT_NOT_FOUND));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rename_object_moves_and_renames() {
        let (mut vol, path) = open_rw();
        let mut mem = ram();
        let src = put_bstr(&mut mem, 0x2000, b"readme.txt");
        let dst = put_bstr(&mut mem, 0x2100, b"renamed.txt");
        let (res1, res2) = vol.execute(action::RENAME_OBJECT, args4(0, src, 0, dst), &mut mem);
        assert_eq!((res1, res2), (DOSTRUE, 0));
        let old = put_bstr(&mut mem, 0x2200, b"readme.txt");
        assert_eq!(
            vol.execute(action::LOCATE_OBJECT, args3(0, old, 0), &mut mem)
                .1,
            err::OBJECT_NOT_FOUND
        );
        let new = put_bstr(&mut mem, 0x2300, b"renamed.txt");
        assert_eq!(
            vol.execute(action::LOCATE_OBJECT, args3(0, new, 0), &mut mem)
                .1,
            0
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn create_dir_action() {
        let (mut vol, path) = open_rw();
        let mut mem = ram();
        let name_bptr = put_bstr(&mut mem, 0x2000, b"newdir");
        let (res1, res2) = vol.execute(action::CREATE_DIR, args2(0, name_bptr), &mut mem);
        assert_eq!(res2, 0);
        assert_ne!(res1, 0);
        let (r1, r2) = vol.execute(action::EXAMINE_OBJECT, args2(res1, 0x3000 / 4), &mut mem);
        assert_eq!((r1, r2), (DOSTRUE, 0));
        let dir_entry_type = i32::from_be_bytes(mem.0[0x3004..0x3008].try_into().unwrap());
        assert_eq!(dir_entry_type, amiga_ffs::ST_USERDIR);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn set_protect_and_comment_then_examine() {
        let (mut vol, path) = open_rw();
        let mut mem = ram();
        let name_bptr = put_bstr(&mut mem, 0x2000, b"readme.txt");
        let (res1, _) = vol.execute(action::LOCATE_OBJECT, args3(0, name_bptr, 0), &mut mem);
        assert_ne!(res1, 0);

        let name_bptr = put_bstr(&mut mem, 0x2100, b"readme.txt");
        let (r1, r2) = vol.execute(
            action::SET_PROTECT,
            args4(0, 0, name_bptr, amiga_ffs::meta::FIBF_ARCHIVE),
            &mut mem,
        );
        assert_eq!((r1, r2), (DOSTRUE, 0));

        let comment_bptr = put_bstr(&mut mem, 0x2200, b"set via pktvol");
        let name_bptr = put_bstr(&mut mem, 0x2300, b"readme.txt");
        let (r1, r2) = vol.execute(
            action::SET_COMMENT,
            args4(0, 0, name_bptr, comment_bptr),
            &mut mem,
        );
        assert_eq!((r1, r2), (DOSTRUE, 0));

        let (r1, r2) = vol.execute(action::EXAMINE_OBJECT, args2(res1, 0x4000 / 4), &mut mem);
        assert_eq!((r1, r2), (DOSTRUE, 0));
        let fib = 0x4000;
        let protection = u32::from_be_bytes(mem.0[fib + 116..fib + 120].try_into().unwrap());
        assert_eq!(protection, amiga_ffs::meta::FIBF_ARCHIVE);
        let clen = mem.0[fib + 144] as usize;
        assert_eq!(&mem.0[fib + 145..fib + 145 + clen], b"set via pktvol");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn disk_info_is_sane() {
        let (mut vol, path) = open_ro();
        let mut mem = ram();
        let (res1, res2) = vol.execute(action::DISK_INFO, args1(0x5000 / 4), &mut mem);
        assert_eq!((res1, res2), (DOSTRUE, 0));
        let info = 0x5000;
        let num_blocks = u32::from_be_bytes(mem.0[info + 12..info + 16].try_into().unwrap());
        let bytes_per_block = u32::from_be_bytes(mem.0[info + 20..info + 24].try_into().unwrap());
        let disk_type = u32::from_be_bytes(mem.0[info + 24..info + 28].try_into().unwrap());
        assert_eq!(num_blocks, BLOCKS as u32);
        assert_eq!(bytes_per_block, BLOCK_SIZE as u32);
        assert_eq!(disk_type, Variant::Ffs.dostype());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unknown_action_is_209() {
        let (mut vol, path) = open_ro();
        let mut mem = ram();
        let (res1, res2) = vol.execute(0xFFFF_FFFF, [0; 7], &mut mem);
        assert_eq!((res1, res2), (DOSFALSE, err::ACTION_NOT_KNOWN));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unknown_handle_is_205() {
        let (mut vol, path) = open_ro();
        let mut mem = ram();
        let (res1, res2) = vol.execute(action::FREE_LOCK, args1(0xDEAD_BEEF), &mut mem);
        assert_eq!((res1, res2), (DOSFALSE, err::OBJECT_NOT_FOUND));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_only_volume_rejects_write() {
        let (mut vol, path) = open_ro();
        let mut mem = ram();
        let name_bptr = put_bstr(&mut mem, 0x2000, b"new.txt");
        let (_, res2) = vol.execute(action::FINDOUTPUT, args3(0, 0, name_bptr), &mut mem);
        assert_eq!(res2, err::WRITE_PROTECTED);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn hostile_fib_pointer_fails_without_panic() {
        let (mut vol, path) = open_ro();
        let mut mem = ram();
        let (res1, res2) = vol.execute(action::EXAMINE_OBJECT, args2(0, 0xFFFF_FFFF / 4), &mut mem);
        assert_eq!((res1, res2), (DOSFALSE, err::BAD_NUMBER));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn hostile_bstr_at_end_of_ram_is_clipped_without_panic() {
        let (mut vol, path) = open_ro();
        let mut mem = ram();
        // A length byte claiming far more bytes than remain in RAM.
        let addr = RAM_SIZE as u32 - 4;
        mem.0[addr as usize] = 0xFF;
        let (res1, res2) = vol.execute(action::LOCATE_OBJECT, args3(0, addr / 4, 0), &mut mem);
        // Must not panic; whatever it resolved to (almost certainly
        // nothing), the result is a legal (RES1, RES2) pair.
        assert!(res1 == DOSFALSE || res1 != 0);
        assert!(res2 == 0 || res2 == err::OBJECT_NOT_FOUND || res2 == err::INVALID_COMPONENT_NAME);
        std::fs::remove_file(&path).ok();
    }

    /// DiskSpeed 4.2's Write test sequence: Open(MODE_OLDFILE) --
    /// ACTION_FINDINPUT -- then Seek(0, OFFSET_BEGINNING), then Write.
    /// MODE_OLDFILE is *not* read-only on AmigaDOS; treating it as such
    /// aborted DiskSpeed's whole PKT0: pass at the first Write test.
    #[test]
    fn findinput_handle_permits_write_like_mode_oldfile() {
        let (mut vol, path) = open_rw();
        let mut mem = ram();
        let name_bptr = put_bstr(&mut mem, 0x2000, b"readme.txt");
        let (res1, handle) = vol.execute(action::FINDINPUT, args3(0, 0, name_bptr), &mut mem);
        assert_eq!(res1, DOSTRUE);
        // Seek to start (OFFSET_BEGINNING = -1).
        let (_, r2) = vol.execute(action::SEEK, args3(handle, 0, (-1i32) as u32), &mut mem);
        assert_eq!(r2, 0);
        // Write through the MODE_OLDFILE handle.
        let payload = b"OLDFILE-WRITE";
        mem.0[0x3000..0x3000 + payload.len()].copy_from_slice(payload);
        let (w, werr) = vol.execute(
            action::WRITE,
            args3(handle, 0x3000, payload.len() as u32),
            &mut mem,
        );
        assert_eq!(werr, 0, "write through FINDINPUT handle must succeed");
        assert_eq!(w as usize, payload.len());
        vol.execute(action::END, args1(handle), &mut mem);
        std::fs::remove_file(&path).ok();
    }

    /// `nondistribution/aros/aros.hdf` regression: every name
    /// `EXAMINE_NEXT` returns must be non-empty (the bug this whole
    /// backend exists to not have -- a `DOS\7` volume read at the
    /// classic offset comes back with every name empty). Gated on the
    /// file existing; not part of a plain `cargo test` run otherwise.
    #[test]
    fn aros_hdf_root_names_are_nonempty() {
        // Anchored to the workspace via CARGO_MANIFEST_DIR, not the
        // test process's CWD (which is this *crate's* directory under
        // `cargo test` -- a bare relative path here would make this
        // test skip forever while reporting success, the exact silent
        // rot `nondistribution/README.md` records happening once
        // already).
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../nondistribution/aros/aros.hdf");
        let path = path.as_path();
        if !path.exists() {
            eprintln!("skipping aros_hdf_root_names_are_nonempty: {path:?} not present");
            return;
        }
        let mut vol = PktVolume::open(path, false, None).unwrap();
        let mut mem = ram();

        let name_bptr = put_bstr(&mut mem, 0x2000, b"S");
        let (res1, res2) = vol.execute(action::LOCATE_OBJECT, args3(0, name_bptr, 0), &mut mem);
        assert_eq!(res2, 0, "LOCATE \"S\" should succeed");
        let s_lock = res1;

        let fib_bptr = 0x4000 / 4;
        let mut found_startup_sequence = false;
        loop {
            let (r1, r2) = vol.execute(action::EXAMINE_NEXT, args2(s_lock, fib_bptr), &mut mem);
            if r2 == err::NO_MORE_ENTRIES {
                assert_eq!(r1, DOSFALSE);
                break;
            }
            assert_eq!((r1, r2), (DOSTRUE, 0));
            let fib = 0x4000;
            let len = mem.0[fib + 8] as usize;
            assert!(len > 0, "EXAMINE_NEXT returned an empty name");
            let name = &mem.0[fib + 9..fib + 9 + len];
            if name == b"Startup-Sequence" {
                found_startup_sequence = true;
            }
        }
        assert!(found_startup_sequence, "S/Startup-Sequence not found");

        // Also walk the root itself, for the same non-empty-name check.
        let fib_bptr = 0x5000 / 4;
        loop {
            let (r1, r2) = vol.execute(action::EXAMINE_NEXT, args2(0, fib_bptr), &mut mem);
            if r2 == err::NO_MORE_ENTRIES {
                assert_eq!(r1, DOSFALSE);
                break;
            }
            assert_eq!((r1, r2), (DOSTRUE, 0));
            let fib = 0x5000;
            let len = mem.0[fib + 8] as usize;
            assert!(len > 0, "root EXAMINE_NEXT returned an empty name");
        }
    }

    // -- ACTION_INHIBIT / ACTION_FORMAT (docs/pktport-protocol.md §5) --

    #[test]
    fn inhibit_format_uninhibit_cycle_yields_empty_named_volume() {
        let (mut vol, path) = open_rw();
        let mut mem = ram();

        // Sanity: the volume isn't empty before formatting -- readme.txt
        // is one of `build_test_volume`'s fixtures.
        let name_bptr = put_bstr(&mut mem, 0x1000, b"readme.txt");
        let (r1, r2) = vol.execute(action::LOCATE_OBJECT, args3(0, name_bptr, 0), &mut mem);
        assert_eq!(
            (r2, r1 != 0),
            (0, true),
            "fixture readme.txt should exist pre-format"
        );

        let (r1, r2) = vol.execute(action::INHIBIT, args1(1), &mut mem);
        assert_eq!((r1, r2), (DOSTRUE, 0));

        let name_bptr = put_bstr(&mut mem, 0x2000, b"TestVol");
        let dostype = Variant::Ffs.dostype();
        let (r1, r2) = vol.execute(action::FORMAT, args2(name_bptr, dostype), &mut mem);
        assert_eq!((r1, r2), (DOSTRUE, 0));

        let (r1, r2) = vol.execute(action::INHIBIT, args1(0), &mut mem);
        assert_eq!((r1, r2), (DOSTRUE, 0));

        // The volume is now empty: the old fixture file is gone.
        let name_bptr = put_bstr(&mut mem, 0x1000, b"readme.txt");
        let (r1, r2) = vol.execute(action::LOCATE_OBJECT, args3(0, name_bptr, 0), &mut mem);
        assert_eq!((r1, r2), (DOSFALSE, err::OBJECT_NOT_FOUND));

        // The root now reports the freshly formatted name.
        let fib_bptr = 0x3000 / 4;
        let (r1, r2) = vol.execute(action::EXAMINE_OBJECT, args2(0, fib_bptr), &mut mem);
        assert_eq!((r1, r2), (DOSTRUE, 0));
        let fib = 0x3000;
        let len = mem.0[fib + 8] as usize;
        assert_eq!(&mem.0[fib + 9..fib + 9 + len], b"TestVol");

        // Root has no children.
        let fib_bptr2 = 0x4000 / 4;
        let (r1, r2) = vol.execute(action::EXAMINE_NEXT, args2(0, fib_bptr2), &mut mem);
        assert_eq!((r1, r2), (DOSFALSE, err::NO_MORE_ENTRIES));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn inhibit_invalidates_open_handles() {
        let (mut vol, path) = open_rw();
        let mut mem = ram();

        let name_bptr = put_bstr(&mut mem, 0x1000, b"readme.txt");
        let (lock, r2) = vol.execute(action::LOCATE_OBJECT, args3(0, name_bptr, 0), &mut mem);
        assert_eq!(r2, 0);
        assert_ne!(lock, 0);

        let (r1, r2) = vol.execute(action::INHIBIT, args1(1), &mut mem);
        assert_eq!((r1, r2), (DOSTRUE, 0));
        let (r1, r2) = vol.execute(action::INHIBIT, args1(0), &mut mem);
        assert_eq!((r1, r2), (DOSTRUE, 0));

        // The lock handed out before INHIBIT no longer resolves.
        let fib_bptr = 0x2000 / 4;
        let (r1, r2) = vol.execute(action::EXAMINE_OBJECT, args2(lock, fib_bptr), &mut mem);
        assert_eq!((r1, r2), (DOSFALSE, err::OBJECT_NOT_FOUND));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn format_outside_inhibit_is_refused() {
        let (mut vol, path) = open_rw();
        let mut mem = ram();
        let name_bptr = put_bstr(&mut mem, 0x1000, b"TestVol");
        let (r1, r2) = vol.execute(
            action::FORMAT,
            args2(name_bptr, Variant::Ffs.dostype()),
            &mut mem,
        );
        assert_eq!((r1, r2), (DOSFALSE, err::OBJECT_IN_USE));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn inhibited_state_rejects_ordinary_actions_but_not_the_exempt_ones() {
        let (mut vol, path) = open_rw();
        let mut mem = ram();

        let (r1, r2) = vol.execute(action::INHIBIT, args1(1), &mut mem);
        assert_eq!((r1, r2), (DOSTRUE, 0));

        let name_bptr = put_bstr(&mut mem, 0x1000, b"readme.txt");
        let (r1, r2) = vol.execute(action::LOCATE_OBJECT, args3(0, name_bptr, 0), &mut mem);
        assert_eq!((r1, r2), (DOSFALSE, err::NOT_A_DOS_DISK));

        // IS_FILESYSTEM and DISK_INFO stay answerable while inhibited.
        let (r1, _) = vol.execute(action::IS_FILESYSTEM, args1(0), &mut mem);
        assert_eq!(r1, DOSTRUE);
        let info_bptr = 0x2000 / 4;
        let (r1, r2) = vol.execute(action::DISK_INFO, args1(info_bptr), &mut mem);
        assert_eq!((r1, r2), (DOSTRUE, 0));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_only_volume_refuses_inhibit_for_format() {
        let (mut vol, path) = open_ro();
        let mut mem = ram();
        let (r1, r2) = vol.execute(action::INHIBIT, args1(1), &mut mem);
        assert_eq!((r1, r2), (DOSFALSE, err::WRITE_PROTECTED));
        std::fs::remove_file(&path).ok();
    }

    // Silence an unused-import warning when the `Cursor`/`format` items
    // above are not otherwise referenced by every cfg combination.
    #[allow(dead_code)]
    fn _unused(_: Cursor<Vec<u8>>) {
        let _ = format::<MemMedium>;
    }
}
