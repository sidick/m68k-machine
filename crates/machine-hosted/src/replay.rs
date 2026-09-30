//! Record/replay lockstep (`docs/cpu-core-proposal.md` §5.3, C1's `--record`/
//! `--replay` slice) -- see `docs/replay-log.md` for the full design
//! writeup (the log format, the `Bus`-seam decision and why, the
//! fixed-mode-only scope, and the seeded-divergence evidence). This
//! module is the log format plus the two participants: [`Recorder`] (the
//! live `fixed`-mode master, wired into `bus.rs`'s `AddressBus` impl) and
//! [`Player`] (the replayer, driven by `run.rs`'s `run_guest_replay`).
//!
//! # Classification
//!
//! Both share `crate::directmap`'s [`crate::directmap::PageType`] and
//! [`crate::directmap::build_page_table`] -- the exact same classifier
//! the direct map uses, so there is only ever one page-type rule set in
//! this crate, not two that could drift apart. Neither participant uses
//! `DirectMap` itself (no mmap, no reservation): the recorder rebuilds
//! its own table straight from live `MachineBus` state on a fingerprint
//! change (mirroring `DirectMap::sync`'s own trigger), and the player
//! rebuilds its table purely from `ClassificationTransition` events --
//! it has no live devices to ask.
//!
//! # Ordinal
//!
//! One `u64` counter, kept in step by both sides: it starts at 0 and is
//! incremented by exactly one, as the *first* action, at the top of every
//! fixed-mode hook invocation (once per retired instruction) and at the
//! top of the `Stopped` arm's own per-line resync tick. Nothing else
//! bumps it. A bus access tagged with ordinal `k` is one that happened
//! after the `k`-th such bump and before the `(k+1)`-th -- concretely,
//! "the accesses made while retiring the instruction that causes the
//! *next* hook call, plus whatever the hook itself does at ordinal `k`
//! before returning." This is deliberately the same value for "this
//! hook's own IPL/device-write/checkpoint bookkeeping" and "the next
//! instruction's bus accesses", since both happen strictly between one
//! bump and the next. See `docs/replay-log.md` for a worked example.
//!
//! # What is not implemented here (see `docs/replay-log.md` "Scope")
//!
//! Flag masking for architecturally-undefined bits (deferred to a C2
//! cross-core player), ring-bounded logs, bisection with RAM snapshots,
//! and cycle/max-mode recording.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use machine_core::{GuestMemory, MachineBus, CHIP_RAM_BASE, CHIP_RAM_SIZE};

use crate::directmap::{build_page_table, page_of, Fingerprint, PageType, PAGE_COUNT};

/// A snapshot of the CPU's user-visible register state, as carried by a
/// [`Event::Checkpoint`]/[`Event::End`] and compared by [`Player`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Regs {
    pub d: [u32; 8],
    pub a: [u32; 8],
    pub pc: u32,
    pub sr: u16,
}

/// One record/replay log event. See the module doc's "Ordinal" section
/// for what `ordinal` means on each variant.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    IoRead {
        ordinal: u64,
        addr: u32,
        width: u8,
        value: u32,
    },
    IoWrite {
        ordinal: u64,
        addr: u32,
        width: u8,
        value: u32,
    },
    IplChange {
        ordinal: u64,
        level: u8,
    },
    DeviceWrite {
        ordinal: u64,
        addr: u32,
        bytes: Vec<u8>,
    },
    ClassificationTransition {
        ordinal: u64,
        table: Box<[PageType; PAGE_COUNT]>,
    },
    Checkpoint {
        ordinal: u64,
        regs: Regs,
    },
    End {
        ordinal: u64,
        total_instructions: u64,
        regs: Regs,
        /// FNV-1a 64-bit digest of the whole chip-RAM contents at the end
        /// of the run -- the backstop for divergences confined entirely
        /// to RAM the guest CPU never reads back (a pure-output blitter
        /// span, say), which no register-only checkpoint can see. See
        /// `docs/replay-log.md`'s "RAM digest" section: detection is at
        /// End only, not at the corrupting event's own ordinal --
        /// localizing further needs C2's checkpointed-RAM-snapshot
        /// bisection (deferred, `docs/cpu-core-proposal.md` §5.3's own
        /// risk section).
        chip_ram_digest: u64,
        /// Same, over fast RAM's backing buffer when one is attached;
        /// `0` when it is not (mirrors `Header::fast_ram_size`'s own
        /// "attached or not" contract -- not a valid digest of an empty
        /// slice, just "nothing to compare here").
        fast_ram_digest: u64,
    },
}

impl Event {
    /// The ordinal every variant carries -- used by [`Player`] to decide
    /// whether a leading non-I/O event belongs to the current ordinal yet.
    fn ordinal(&self) -> u64 {
        match self {
            Event::IoRead { ordinal, .. }
            | Event::IoWrite { ordinal, .. }
            | Event::IplChange { ordinal, .. }
            | Event::DeviceWrite { ordinal, .. }
            | Event::ClassificationTransition { ordinal, .. }
            | Event::Checkpoint { ordinal, .. }
            | Event::End { ordinal, .. } => *ordinal,
        }
    }
}

const TAG_IO_READ: u8 = 1;
const TAG_IO_WRITE: u8 = 2;
const TAG_IPL_CHANGE: u8 = 3;
const TAG_DEVICE_WRITE: u8 = 4;
const TAG_CLASSIFICATION_TRANSITION: u8 = 5;
const TAG_CHECKPOINT: u8 = 6;
const TAG_END: u8 = 0xFF;

const MAGIC: &[u8; 8] = b"M68KREPL";
const VERSION: u32 = 2;

/// The log header: everything a player needs to validate it is replaying
/// against the same machine shape the recorder used, plus the initial
/// classification table.
#[derive(Clone)]
pub struct Header {
    pub instructions_per_line: u32,
    /// FNV-1a 64-bit hash of the exact ROM bytes `MachineBus` was built
    /// from -- "a hash of the ROM image" (the milestone brief), not a
    /// cryptographic one: this is an identity check against accidentally
    /// replaying one ROM's log against a different ROM, not a security
    /// boundary, so a fast nonstandard-library hash is the right tool and
    /// avoids a new dependency for this one field.
    pub rom_hash: u64,
    pub chip_ram_size: u32,
    pub fast_ram_size: u32,
    pub initial_table: Box<[PageType; PAGE_COUNT]>,
}

/// FNV-1a 64-bit, `docs/replay-log.md`'s "ROM identity" field. Not
/// cryptographic; see [`Header::rom_hash`]'s own doc comment.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// The two whole-RAM digests an [`Event::End`] carries -- chip RAM always
/// (a fixed-size, always-attached region), fast RAM only when attached
/// (`0` otherwise, matching [`Header::fast_ram_size`]'s own "attached or
/// not" contract). Shared by [`Recorder::finish`] (computes what actually
/// happened) and [`Player`]'s own End handling (computes what its replay
/// produced, to compare) -- one implementation, so the two sides can
/// never disagree about *how* a digest is computed, only whether the
/// bytes underneath do.
fn ram_digests(bus: &MachineBus) -> (u64, u64) {
    let chip = bus
        .ram_slice(CHIP_RAM_BASE, CHIP_RAM_SIZE as u32)
        .map(fnv1a64)
        .unwrap_or(0);
    let fast = match bus.fast_ram_window() {
        Some((base, len)) => bus.ram_slice(base, len).map(fnv1a64).unwrap_or(0),
        None => 0,
    };
    (chip, fast)
}

fn write_u8(w: &mut impl Write, v: u8) -> io::Result<()> {
    w.write_all(&[v])
}
fn write_u16(w: &mut impl Write, v: u16) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}
fn write_u32(w: &mut impl Write, v: u32) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}
fn write_u64(w: &mut impl Write, v: u64) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}
fn read_u8(r: &mut impl Read) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}
fn read_u16(r: &mut impl Read) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}
fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}
fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

fn write_table(w: &mut impl Write, table: &[PageType; PAGE_COUNT]) -> io::Result<()> {
    let mut bytes = vec![0u8; PAGE_COUNT];
    for (i, t) in table.iter().enumerate() {
        bytes[i] = t.to_code();
    }
    w.write_all(&bytes)
}

fn read_table(r: &mut impl Read) -> io::Result<Box<[PageType; PAGE_COUNT]>> {
    let mut bytes = vec![0u8; PAGE_COUNT];
    r.read_exact(&mut bytes)?;
    let mut table = Box::new([PageType::Io; PAGE_COUNT]);
    for (i, b) in bytes.iter().enumerate() {
        table[i] = PageType::from_code(*b);
    }
    Ok(table)
}

fn write_regs(w: &mut impl Write, regs: &Regs) -> io::Result<()> {
    for v in regs.d {
        write_u32(w, v)?;
    }
    for v in regs.a {
        write_u32(w, v)?;
    }
    write_u32(w, regs.pc)?;
    write_u16(w, regs.sr)?;
    Ok(())
}

fn read_regs(r: &mut impl Read) -> io::Result<Regs> {
    let mut d = [0u32; 8];
    for v in d.iter_mut() {
        *v = read_u32(r)?;
    }
    let mut a = [0u32; 8];
    for v in a.iter_mut() {
        *v = read_u32(r)?;
    }
    let pc = read_u32(r)?;
    let sr = read_u16(r)?;
    Ok(Regs { d, a, pc, sr })
}

fn write_event(w: &mut impl Write, ev: &Event) -> io::Result<()> {
    match ev {
        Event::IoRead {
            ordinal,
            addr,
            width,
            value,
        } => {
            write_u8(w, TAG_IO_READ)?;
            write_u64(w, *ordinal)?;
            write_u32(w, *addr)?;
            write_u8(w, *width)?;
            write_u32(w, *value)?;
        }
        Event::IoWrite {
            ordinal,
            addr,
            width,
            value,
        } => {
            write_u8(w, TAG_IO_WRITE)?;
            write_u64(w, *ordinal)?;
            write_u32(w, *addr)?;
            write_u8(w, *width)?;
            write_u32(w, *value)?;
        }
        Event::IplChange { ordinal, level } => {
            write_u8(w, TAG_IPL_CHANGE)?;
            write_u64(w, *ordinal)?;
            write_u8(w, *level)?;
        }
        Event::DeviceWrite {
            ordinal,
            addr,
            bytes,
        } => {
            write_u8(w, TAG_DEVICE_WRITE)?;
            write_u64(w, *ordinal)?;
            write_u32(w, *addr)?;
            write_u32(w, bytes.len() as u32)?;
            w.write_all(bytes)?;
        }
        Event::ClassificationTransition { ordinal, table } => {
            write_u8(w, TAG_CLASSIFICATION_TRANSITION)?;
            write_u64(w, *ordinal)?;
            write_table(w, table)?;
        }
        Event::Checkpoint { ordinal, regs } => {
            write_u8(w, TAG_CHECKPOINT)?;
            write_u64(w, *ordinal)?;
            write_regs(w, regs)?;
        }
        Event::End {
            ordinal,
            total_instructions,
            regs,
            chip_ram_digest,
            fast_ram_digest,
        } => {
            write_u8(w, TAG_END)?;
            write_u64(w, *ordinal)?;
            write_u64(w, *total_instructions)?;
            write_regs(w, regs)?;
            write_u64(w, *chip_ram_digest)?;
            write_u64(w, *fast_ram_digest)?;
        }
    }
    Ok(())
}

/// Reads one event, or `Ok(None)` at a clean end-of-stream (no bytes left
/// at all -- a well-formed log always ends with an [`Event::End`] record
/// before that point, so a clean `None` here is only expected *after*
/// having already read one).
fn read_event(r: &mut impl Read) -> io::Result<Option<Event>> {
    let mut tag = [0u8; 1];
    let n = r.read(&mut tag)?;
    if n == 0 {
        return Ok(None);
    }
    let ev = match tag[0] {
        TAG_IO_READ => Event::IoRead {
            ordinal: read_u64(r)?,
            addr: read_u32(r)?,
            width: read_u8(r)?,
            value: read_u32(r)?,
        },
        TAG_IO_WRITE => Event::IoWrite {
            ordinal: read_u64(r)?,
            addr: read_u32(r)?,
            width: read_u8(r)?,
            value: read_u32(r)?,
        },
        TAG_IPL_CHANGE => Event::IplChange {
            ordinal: read_u64(r)?,
            level: read_u8(r)?,
        },
        TAG_DEVICE_WRITE => {
            let ordinal = read_u64(r)?;
            let addr = read_u32(r)?;
            let len = read_u32(r)? as usize;
            let mut bytes = vec![0u8; len];
            r.read_exact(&mut bytes)?;
            Event::DeviceWrite {
                ordinal,
                addr,
                bytes,
            }
        }
        TAG_CLASSIFICATION_TRANSITION => Event::ClassificationTransition {
            ordinal: read_u64(r)?,
            table: read_table(r)?,
        },
        TAG_CHECKPOINT => Event::Checkpoint {
            ordinal: read_u64(r)?,
            regs: read_regs(r)?,
        },
        TAG_END => Event::End {
            ordinal: read_u64(r)?,
            total_instructions: read_u64(r)?,
            regs: read_regs(r)?,
            chip_ram_digest: read_u64(r)?,
            fast_ram_digest: read_u64(r)?,
        },
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("replay log: unknown event tag {other:#04x}"),
            ))
        }
    };
    Ok(Some(ev))
}

/// Collect [`Regs`] from anything implementing [`crate::cpu::HookCpu`] --
/// a free function rather than a method on `Recorder`/`Player` so neither
/// needs to be generic over the CPU type; `run.rs` calls this at the
/// point it has `cpu` in scope (`HookCpu::dar`/`pc`/`sr`).
pub fn regs_of<C: crate::cpu::HookCpu>(cpu: &C) -> Regs {
    let mut d = [0u32; 8];
    let mut a = [0u32; 8];
    for i in 0..8 {
        d[i] = cpu.dar(i);
        a[i] = cpu.dar(8 + i);
    }
    Regs {
        d,
        a,
        pc: cpu.pc(),
        sr: cpu.sr(),
    }
}

/// Open bus's inline read value at `width` bytes -- all-ones, the same
/// constant `directmap.rs`'s own read macros use.
fn open_bus_value(width: u32) -> u32 {
    match width {
        1 => 0xFF,
        2 => 0xFFFF,
        4 => 0xFFFF_FFFF,
        _ => unreachable!("width is always 1, 2 or 4"),
    }
}

fn be_bytes_to_u32(bytes: &[u8]) -> u32 {
    match bytes.len() {
        1 => bytes[0] as u32,
        2 => u16::from_be_bytes([bytes[0], bytes[1]]) as u32,
        4 => u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        _ => unreachable!("width is always 1, 2 or 4"),
    }
}

/// How many ordinals between register checkpoints (`docs/cpu-core-
/// proposal.md` §5.3). A million ordinals is a few boot-frames' worth of
/// instructions at the measured `N=424`/line default, so a full 4400-frame
/// boot logs on the order of 40 checkpoints -- frequent enough to bisect a
/// divergence to a tight window, rare enough not to dominate the log.
pub const DEFAULT_CHECKPOINT_INTERVAL: u64 = 1_000_000;

/// The live `fixed`-mode master (`docs/cpu-core-proposal.md` §5.3's
/// recorder). Constructed once `MachineBus` is fully wired (its table's
/// first build needs the finished bus, same as `DirectMap::new`), wired
/// into `bus.rs`'s `Bus.6` field. All fallible operations return
/// `io::Result` -- `run.rs` treats any of them failing as the "abort the
/// recording loudly" case the milestone brief requires for a device-write
/// overflow, applied here to every I/O error too (a recording that can't
/// be written is exactly as useless as one that silently dropped spans).
pub struct Recorder {
    writer: BufWriter<File>,
    table: Box<[PageType; PAGE_COUNT]>,
    fingerprint: Fingerprint,
    rom_is_full_window: bool,
    ordinal: u64,
    last_ipl: u8,
    checkpoint_interval: u64,
    pub io_read_events: u64,
    pub io_write_events: u64,
    pub ipl_events: u64,
    pub device_write_events: u64,
    pub transition_events: u64,
    pub checkpoint_events: u64,
}

impl Recorder {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        path: &Path,
        bus: &MachineBus,
        rom: &[u8],
        instructions_per_line: u32,
        checkpoint_interval: u64,
    ) -> io::Result<Self> {
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        let rom_is_full_window = rom.len() == machine_core::ROM_WINDOW_SIZE;
        let table = build_page_table(bus, rom_is_full_window, bus.fast_ram_window());
        let fingerprint = Fingerprint::of(bus);
        let chip_ram_size = machine_core::CHIP_RAM_SIZE as u32;
        let fast_ram_size = bus.fast_ram_window().map(|(_, l)| l).unwrap_or(0);

        writer.write_all(MAGIC)?;
        write_u32(&mut writer, VERSION)?;
        write_u32(&mut writer, instructions_per_line)?;
        write_u64(&mut writer, fnv1a64(rom))?;
        write_u32(&mut writer, chip_ram_size)?;
        write_u32(&mut writer, fast_ram_size)?;
        write_table(&mut writer, &table)?;

        Ok(Recorder {
            writer,
            table,
            fingerprint,
            rom_is_full_window,
            ordinal: 0,
            last_ipl: 0,
            checkpoint_interval,
            io_read_events: 0,
            io_write_events: 0,
            ipl_events: 0,
            device_write_events: 0,
            transition_events: 0,
            checkpoint_events: 0,
        })
    }

    #[inline]
    pub fn classify(&self, addr: u32) -> PageType {
        self.table[page_of(addr)]
    }

    #[inline]
    pub fn ordinal(&self) -> u64 {
        self.ordinal
    }

    /// The first action of every hook/Stopped-tick invocation -- see the
    /// module doc's "Ordinal" section.
    #[inline]
    pub fn advance_ordinal(&mut self) {
        self.ordinal += 1;
    }

    pub fn log_io_read(&mut self, addr: u32, width: u8, value: u32) -> io::Result<()> {
        self.io_read_events += 1;
        write_event(
            &mut self.writer,
            &Event::IoRead {
                ordinal: self.ordinal,
                addr,
                width,
                value,
            },
        )
    }

    pub fn log_io_write(&mut self, addr: u32, width: u8, value: u32) -> io::Result<()> {
        self.io_write_events += 1;
        write_event(
            &mut self.writer,
            &Event::IoWrite {
                ordinal: self.ordinal,
                addr,
                width,
                value,
            },
        )
    }

    /// Logs an `IplChange` only when `level` differs from the last one
    /// logged (or the initial `0`) -- matches the design's "on change".
    pub fn log_ipl_if_changed(&mut self, level: u8) -> io::Result<()> {
        if level == self.last_ipl {
            return Ok(());
        }
        self.last_ipl = level;
        self.ipl_events += 1;
        write_event(
            &mut self.writer,
            &Event::IplChange {
                ordinal: self.ordinal,
                level,
            },
        )
    }

    pub fn log_device_write(&mut self, addr: u32, bytes: &[u8]) -> io::Result<()> {
        self.device_write_events += 1;
        write_event(
            &mut self.writer,
            &Event::DeviceWrite {
                ordinal: self.ordinal,
                addr,
                bytes: bytes.to_vec(),
            },
        )
    }

    /// Re-checks the placement/overlay fingerprint (same trigger
    /// discipline as `DirectMap::sync`) and, on change, rebuilds the
    /// table and logs a `ClassificationTransition` carrying the new one.
    pub fn maybe_transition(&mut self, bus: &MachineBus) -> io::Result<()> {
        let fp = Fingerprint::of(bus);
        if fp == self.fingerprint {
            return Ok(());
        }
        self.fingerprint = fp;
        self.table = build_page_table(bus, self.rom_is_full_window, bus.fast_ram_window());
        self.transition_events += 1;
        write_event(
            &mut self.writer,
            &Event::ClassificationTransition {
                ordinal: self.ordinal,
                table: self.table.clone(),
            },
        )
    }

    pub fn maybe_checkpoint(&mut self, regs: Regs) -> io::Result<()> {
        if self.checkpoint_interval == 0 || !self.ordinal.is_multiple_of(self.checkpoint_interval) {
            return Ok(());
        }
        self.checkpoint_events += 1;
        write_event(
            &mut self.writer,
            &Event::Checkpoint {
                ordinal: self.ordinal,
                regs,
            },
        )
    }

    /// Writes the log's final `End` record, including a whole-RAM digest
    /// of `bus`'s current chip/fast RAM contents (`ram_digests`) -- the
    /// milestone's backstop against a divergence confined entirely to RAM
    /// content the guest CPU never reads back, which no register-only
    /// checkpoint could otherwise see (found and named by the seeded
    /// blitter-span test below; see `docs/replay-log.md`'s "RAM digest"
    /// section).
    pub fn finish(
        &mut self,
        total_instructions: u64,
        regs: Regs,
        bus: &MachineBus,
    ) -> io::Result<()> {
        let (chip_ram_digest, fast_ram_digest) = ram_digests(bus);
        write_event(
            &mut self.writer,
            &Event::End {
                ordinal: self.ordinal,
                total_instructions,
                regs,
                chip_ram_digest,
                fast_ram_digest,
            },
        )?;
        self.writer.flush()
    }
}

/// Why a replay diverged -- [`Player`]'s reads/writes/`drain_side_effects`
/// return this instead of the logged value/`()` on a mismatch. `run.rs`
/// reports this and exits nonzero; nothing tries to recover from it
/// mid-run (the milestone brief: "a mismatch is a desync, reported
/// immediately").
#[derive(Clone, Debug, PartialEq)]
pub enum Divergence {
    IoReadMismatch {
        expected_ordinal: u64,
        actual_ordinal: u64,
        expected_addr: u32,
        actual_addr: u32,
        expected_width: u8,
        actual_width: u8,
        pc: u32,
    },
    IoWriteMismatch {
        expected_ordinal: u64,
        actual_ordinal: u64,
        expected_addr: u32,
        actual_addr: u32,
        expected_width: u8,
        actual_width: u8,
        expected_value: u32,
        actual_value: u32,
        pc: u32,
    },
    /// The log had no more events where an I/O access was expected.
    UnexpectedEof { ordinal: u64, addr: u32, pc: u32 },
    /// The next event in the log was not an I/O event at all (e.g. an
    /// `End` arrived early) where one was expected.
    ExpectedIoEvent { ordinal: u64, addr: u32, pc: u32 },
    CheckpointMismatch {
        ordinal: u64,
        expected: Regs,
        actual: Regs,
        first_diff: &'static str,
    },
    FinalStateMismatch {
        expected: Regs,
        actual: Regs,
        first_diff: &'static str,
    },
    RamAccessOutOfBounds {
        ordinal: u64,
        addr: u32,
        width: u8,
        pc: u32,
    },
    /// A whole-RAM digest mismatch at the log's `End` record -- the
    /// backstop for a divergence confined entirely to RAM content the
    /// guest CPU never reads back (see `Event::End::chip_ram_digest`'s
    /// own doc comment, and `docs/replay-log.md`'s "RAM digest" section).
    /// `region` is `"chip"` or `"fast"`. Detected only at End, not at
    /// whatever ordinal actually diverged -- localizing further needs
    /// C2's checkpointed-RAM-snapshot bisection, deferred.
    RamDigestMismatch {
        region: &'static str,
        expected: u64,
        actual: u64,
    },
    /// The log file itself couldn't be read further (truncated, or a
    /// real I/O error) at the point an event was expected.
    LogIoError(String),
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Divergence::IoReadMismatch {
                expected_ordinal,
                actual_ordinal,
                expected_addr,
                actual_addr,
                expected_width,
                actual_width,
                pc,
            } => write!(
                f,
                "IoRead mismatch: expected ordinal={expected_ordinal} addr={expected_addr:#010x} \
                 width={expected_width}, got ordinal={actual_ordinal} addr={actual_addr:#010x} \
                 width={actual_width} (PC {pc:#010x})"
            ),
            Divergence::IoWriteMismatch {
                expected_ordinal,
                actual_ordinal,
                expected_addr,
                actual_addr,
                expected_width,
                actual_width,
                expected_value,
                actual_value,
                pc,
            } => write!(
                f,
                "IoWrite mismatch: expected ordinal={expected_ordinal} addr={expected_addr:#010x} \
                 width={expected_width} value={expected_value:#010x}, got ordinal={actual_ordinal} \
                 addr={actual_addr:#010x} width={actual_width} value={actual_value:#010x} \
                 (PC {pc:#010x})"
            ),
            Divergence::UnexpectedEof { ordinal, addr, pc } => write!(
                f,
                "replay log ended early: expected an I/O event at ordinal={ordinal} \
                 addr={addr:#010x} (PC {pc:#010x})"
            ),
            Divergence::ExpectedIoEvent { ordinal, addr, pc } => write!(
                f,
                "replay log desync: expected an I/O event at ordinal={ordinal} \
                 addr={addr:#010x} (PC {pc:#010x}), found a different event kind"
            ),
            Divergence::CheckpointMismatch {
                ordinal,
                expected,
                actual,
                first_diff,
            } => write!(
                f,
                "checkpoint mismatch at ordinal={ordinal}: first differing register {first_diff} \
                 (expected {expected:?}, actual {actual:?})"
            ),
            Divergence::FinalStateMismatch {
                expected,
                actual,
                first_diff,
            } => write!(
                f,
                "final-state mismatch: first differing register {first_diff} \
                 (expected {expected:?}, actual {actual:?})"
            ),
            Divergence::RamAccessOutOfBounds {
                ordinal,
                addr,
                width,
                pc,
            } => write!(
                f,
                "RAM access out of bounds at ordinal={ordinal} addr={addr:#010x} width={width} \
                 (PC {pc:#010x}) -- classified Ram but not backed by any attached RAM region"
            ),
            Divergence::RamDigestMismatch {
                region,
                expected,
                actual,
            } => write!(
                f,
                "RAM digest mismatch at End: {region} RAM expected={expected:#018x} \
                 actual={actual:#018x}"
            ),
            Divergence::LogIoError(msg) => write!(f, "replay log I/O error: {msg}"),
        }
    }
}

/// What [`Player::drain_side_effects`] applied at the current ordinal.
#[derive(Default)]
pub struct DrainedEffects {
    pub device_writes_applied: u64,
    pub transitions_applied: u64,
    pub ipl: Option<u8>,
    pub checkpoint: Option<(u64, Regs)>,
    pub end: Option<(u64, u64, Regs)>,
}

/// The replayer (`docs/cpu-core-proposal.md` §5.3's player). Holds no
/// live devices at all -- everything it needs comes from the log or from
/// direct `GuestMemory` access on the player's own `MachineBus` (bypassing
/// its possibly-stale overlay state, per the milestone brief).
pub struct Player {
    reader: BufReader<File>,
    header: Header,
    table: Box<[PageType; PAGE_COUNT]>,
    ordinal: u64,
    peeked: Option<Event>,
    /// The first divergence encountered, if any -- sticky, never
    /// overwritten by a later one, so `run.rs` always reports the
    /// *first* desync (the milestone brief: "a single clear report of
    /// the FIRST divergence"). Recorded from deep inside `bus.rs`'s
    /// `AddressBus` impl (which has no `Result`-shaped way to propagate
    /// it directly) and drained by `run.rs`'s outer loop right after
    /// each hook invocation / Stopped-tick.
    divergence: Option<Divergence>,
    pub io_read_events_consumed: u64,
    pub io_write_events_consumed: u64,
    pub ipl_events_applied: u64,
    pub device_writes_applied: u64,
    pub transitions_applied: u64,
    pub checkpoints_compared: u64,
}

impl Player {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "replay log: bad magic (not a machine-hosted record/replay log)",
            ));
        }
        let version = read_u32(&mut reader)?;
        if version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("replay log: unsupported version {version} (expected {VERSION})"),
            ));
        }
        let instructions_per_line = read_u32(&mut reader)?;
        let rom_hash = read_u64(&mut reader)?;
        let chip_ram_size = read_u32(&mut reader)?;
        let fast_ram_size = read_u32(&mut reader)?;
        let initial_table = read_table(&mut reader)?;
        let header = Header {
            instructions_per_line,
            rom_hash,
            chip_ram_size,
            fast_ram_size,
            initial_table,
        };
        let table = header.initial_table.clone();
        Ok(Player {
            reader,
            header,
            table,
            ordinal: 0,
            peeked: None,
            divergence: None,
            io_read_events_consumed: 0,
            io_write_events_consumed: 0,
            ipl_events_applied: 0,
            device_writes_applied: 0,
            transitions_applied: 0,
            checkpoints_compared: 0,
        })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Record a divergence, keeping only the first (see the field's own
    /// doc comment).
    pub fn record_divergence(&mut self, d: Divergence) {
        if self.divergence.is_none() {
            self.divergence = Some(d);
        }
    }

    pub fn divergence(&self) -> Option<&Divergence> {
        self.divergence.as_ref()
    }

    #[inline]
    pub fn classify(&self, addr: u32) -> PageType {
        self.table[page_of(addr)]
    }

    #[inline]
    pub fn ordinal(&self) -> u64 {
        self.ordinal
    }

    #[inline]
    pub fn advance_ordinal(&mut self) {
        self.ordinal += 1;
    }

    fn fill_peek(&mut self) -> io::Result<()> {
        if self.peeked.is_none() {
            self.peeked = read_event(&mut self.reader)?;
        }
        Ok(())
    }

    /// Applies every leading `DeviceWrite`/`ClassificationTransition`
    /// event at or before the *current* ordinal, and reports (without
    /// consuming past it) an `IplChange`/`Checkpoint`/`End` at or before
    /// this ordinal too. `<=`, not `==`: a write-triggered
    /// `ClassificationTransition` (and, in principle, a synchronous
    /// blitter `DeviceWrite`) is logged by `bus.rs`'s write path *during*
    /// the instruction that caused it -- i.e. at the ordinal value as of
    /// *before* the completing hook's own bump -- while everything the
    /// hook body itself logs (IPL, the tick's own device-write drain, a
    /// checkpoint) is logged *after* that same bump, at the new value.
    /// Both kinds can therefore be sitting in the log with an ordinal
    /// strictly less than the one this call just advanced to, and both
    /// must be applied here: an `==`-only check would leave the
    /// pre-bump-ordinal event permanently stuck (`self.ordinal` never
    /// equals that value again, since it only increases) -- a real bug
    /// this project's first end-to-end record/replay run against a real
    /// boot caught (see `docs/replay-log.md`'s "A real bug, found and
    /// fixed" section). I/O events are never at risk of being skipped
    /// over by the `<=` relaxation: they are only ever consumed by
    /// [`Self::expect_io_read`]/[`Self::expect_io_write`], and this loop
    /// stops the instant it peeks one, of any ordinal. Call once per hook
    /// invocation / Stopped-tick, right after [`Self::advance_ordinal`].
    pub fn drain_side_effects(&mut self, bus: &mut MachineBus) -> io::Result<DrainedEffects> {
        let mut out = DrainedEffects::default();
        loop {
            self.fill_peek()?;
            let Some(ev) = &self.peeked else { break };
            if ev.ordinal() > self.ordinal {
                break;
            }
            match ev {
                Event::DeviceWrite { addr, bytes, .. } => {
                    let addr = *addr;
                    let len = bytes.len();
                    if let Some(dst) = bus.ram_slice_mut(addr, len as u32) {
                        dst.copy_from_slice(bytes);
                    }
                    out.device_writes_applied += 1;
                    self.device_writes_applied += 1;
                    self.peeked = None;
                }
                Event::ClassificationTransition { table, .. } => {
                    self.table = table.clone();
                    out.transitions_applied += 1;
                    self.transitions_applied += 1;
                    self.peeked = None;
                }
                Event::IplChange { level, .. } => {
                    out.ipl = Some(*level);
                    self.ipl_events_applied += 1;
                    self.peeked = None;
                }
                Event::Checkpoint { ordinal, regs, .. } => {
                    out.checkpoint = Some((*ordinal, *regs));
                    self.checkpoints_compared += 1;
                    self.peeked = None;
                }
                Event::End {
                    ordinal,
                    total_instructions,
                    regs,
                    chip_ram_digest,
                    fast_ram_digest,
                } => {
                    // Copy every field out of the borrowed event *before*
                    // calling `self.record_divergence` below: that's a
                    // method call needing an exclusive `&mut self`, which
                    // cannot coexist with `ev`'s (and so `self.peeked`'s)
                    // live immutable borrow -- so nothing below this
                    // point may still reference `ordinal`/`regs`/etc
                    // through `ev`.
                    let end_ordinal = *ordinal;
                    let end_total = *total_instructions;
                    let end_regs = *regs;
                    let expected_chip = *chip_ram_digest;
                    let expected_fast = *fast_ram_digest;

                    // Whole-RAM digest check -- the backstop for a
                    // divergence confined entirely to RAM content the
                    // guest CPU never reads back (see
                    // `Event::End::chip_ram_digest`'s own doc comment).
                    // Computed and compared here, before `out.end` is
                    // even handed back to the caller, so a mismatch is
                    // recorded as early as this End record is reached --
                    // "at End", per this divergence kind's own contract,
                    // not at whatever ordinal actually caused it.
                    let (actual_chip, actual_fast) = ram_digests(bus);
                    if expected_chip != actual_chip {
                        self.record_divergence(Divergence::RamDigestMismatch {
                            region: "chip",
                            expected: expected_chip,
                            actual: actual_chip,
                        });
                    } else if expected_fast != actual_fast {
                        self.record_divergence(Divergence::RamDigestMismatch {
                            region: "fast",
                            expected: expected_fast,
                            actual: actual_fast,
                        });
                    }
                    out.end = Some((end_ordinal, end_total, end_regs));
                    self.peeked = None;
                }
                Event::IoRead { .. } | Event::IoWrite { .. } => break,
            }
        }
        Ok(out)
    }

    /// Consumes and validates the next `IoRead` event against what the
    /// CPU actually requested, returning the logged value on a match.
    pub fn expect_io_read(
        &mut self,
        addr: u32,
        width: u8,
        pc: u32,
    ) -> io::Result<Result<u32, Box<Divergence>>> {
        self.fill_peek()?;
        let Some(ev) = self.peeked.take() else {
            return Ok(Err(Box::new(Divergence::UnexpectedEof {
                ordinal: self.ordinal,
                addr,
                pc,
            })));
        };
        match ev {
            Event::IoRead {
                ordinal,
                addr: eaddr,
                width: ewidth,
                value,
            } => {
                self.io_read_events_consumed += 1;
                if ordinal == self.ordinal && eaddr == addr && ewidth == width {
                    Ok(Ok(value))
                } else {
                    Ok(Err(Box::new(Divergence::IoReadMismatch {
                        expected_ordinal: ordinal,
                        actual_ordinal: self.ordinal,
                        expected_addr: eaddr,
                        actual_addr: addr,
                        expected_width: ewidth,
                        actual_width: width,
                        pc,
                    })))
                }
            }
            other => {
                let ordinal = other.ordinal();
                self.peeked = Some(other);
                Ok(Err(Box::new(Divergence::ExpectedIoEvent {
                    ordinal,
                    addr,
                    pc,
                })))
            }
        }
    }

    /// Consumes and validates the next `IoWrite` event; the write itself
    /// is always discarded (no live device to apply it to).
    pub fn expect_io_write(
        &mut self,
        addr: u32,
        width: u8,
        value: u32,
        pc: u32,
    ) -> io::Result<Result<(), Box<Divergence>>> {
        self.fill_peek()?;
        let Some(ev) = self.peeked.take() else {
            return Ok(Err(Box::new(Divergence::UnexpectedEof {
                ordinal: self.ordinal,
                addr,
                pc,
            })));
        };
        match ev {
            Event::IoWrite {
                ordinal,
                addr: eaddr,
                width: ewidth,
                value: evalue,
            } => {
                self.io_write_events_consumed += 1;
                if ordinal == self.ordinal && eaddr == addr && ewidth == width && evalue == value {
                    Ok(Ok(()))
                } else {
                    Ok(Err(Box::new(Divergence::IoWriteMismatch {
                        expected_ordinal: ordinal,
                        actual_ordinal: self.ordinal,
                        expected_addr: eaddr,
                        actual_addr: addr,
                        expected_width: ewidth,
                        actual_width: width,
                        expected_value: evalue,
                        actual_value: value,
                        pc,
                    })))
                }
            }
            other => {
                let ordinal = other.ordinal();
                self.peeked = Some(other);
                Ok(Err(Box::new(Divergence::ExpectedIoEvent {
                    ordinal,
                    addr,
                    pc,
                })))
            }
        }
    }

    /// RAM read for the player's classify-Ram case: `GuestMemory::ram_slice`,
    /// never `MachineBus::read_byte`/etc (whose overlay state is stale in
    /// replay -- no CIA writes are ever applied on this side).
    pub fn read_ram(
        &self,
        bus: &MachineBus,
        addr: u32,
        width: u8,
        ordinal: u64,
        pc: u32,
    ) -> Result<u32, Box<Divergence>> {
        match bus.ram_slice(addr, width as u32) {
            Some(bytes) => Ok(be_bytes_to_u32(bytes)),
            None => Err(Box::new(Divergence::RamAccessOutOfBounds {
                ordinal,
                addr,
                width,
                pc,
            })),
        }
    }

    pub fn write_ram(
        &self,
        bus: &mut MachineBus,
        addr: u32,
        width: u8,
        value: u32,
        ordinal: u64,
        pc: u32,
    ) -> Result<(), Box<Divergence>> {
        match bus.ram_slice_mut(addr, width as u32) {
            Some(dst) => {
                let bytes = value.to_be_bytes();
                dst.copy_from_slice(&bytes[4 - width as usize..]);
                Ok(())
            }
            None => Err(Box::new(Divergence::RamAccessOutOfBounds {
                ordinal,
                addr,
                width,
                pc,
            })),
        }
    }
}

pub fn read_rom_value(bus: &mut MachineBus, addr: u32, width: u8) -> u32 {
    match width {
        1 => bus.read_byte(addr) as u32,
        2 => bus.read_word(addr) as u32,
        4 => bus.read_long(addr),
        _ => unreachable!("width is always 1, 2 or 4"),
    }
}

pub fn open_bus_read(width: u8) -> u32 {
    open_bus_value(width as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use machine_core::{CHIP_RAM_SIZE, ROM_WINDOW_SIZE};

    fn synthetic_bus_and_rom() -> (Box<[u8; CHIP_RAM_SIZE]>, Vec<u8>) {
        let chip: Box<[u8; CHIP_RAM_SIZE]> = vec![0u8; CHIP_RAM_SIZE]
            .into_boxed_slice()
            .try_into()
            .unwrap();
        let rom = vec![0xAAu8; ROM_WINDOW_SIZE];
        (chip, rom)
    }

    #[test]
    fn fnv1a64_is_stable_and_sensitive_to_content() {
        let a = fnv1a64(b"hello");
        let b = fnv1a64(b"hello");
        let c = fnv1a64(b"hellO");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn page_type_code_roundtrips() {
        for t in [
            PageType::Ram,
            PageType::Rom,
            PageType::Io,
            PageType::OpenBus,
        ] {
            assert_eq!(PageType::from_code(t.to_code()), t);
        }
        // An unrecognised code decodes as `Io` (the safe choice), never
        // `Ram`/`Rom`/`OpenBus` -- see `PageType::from_code`'s own doc
        // comment.
        assert_eq!(PageType::from_code(200), PageType::Io);
    }

    #[test]
    fn event_encode_decode_round_trips_every_variant() {
        let mut buf: Vec<u8> = Vec::new();
        let regs = Regs {
            d: [1, 2, 3, 4, 5, 6, 7, 8],
            a: [11, 12, 13, 14, 15, 16, 17, 18],
            pc: 0x00f80000,
            sr: 0x2700,
        };
        let mut table = Box::new([PageType::Io; PAGE_COUNT]);
        table[0] = PageType::Ram;
        table[1] = PageType::Rom;
        table[2] = PageType::OpenBus;

        let events = vec![
            Event::IoRead {
                ordinal: 1,
                addr: 0x00DFF000,
                width: 2,
                value: 0x1234,
            },
            Event::IoWrite {
                ordinal: 2,
                addr: 0x00DFF180,
                width: 4,
                value: 0xDEAD_BEEF,
            },
            Event::IplChange {
                ordinal: 3,
                level: 6,
            },
            Event::DeviceWrite {
                ordinal: 4,
                addr: 0x1000,
                bytes: vec![1, 2, 3, 4, 5],
            },
            Event::ClassificationTransition {
                ordinal: 5,
                table: table.clone(),
            },
            Event::Checkpoint { ordinal: 6, regs },
            Event::End {
                ordinal: 7,
                total_instructions: 12345,
                regs,
                chip_ram_digest: 0xdead_beef_1234_5678,
                fast_ram_digest: 0x0011_2233_4455_6677,
            },
        ];

        for ev in &events {
            write_event(&mut buf, ev).unwrap();
        }

        let mut cursor = std::io::Cursor::new(buf);
        for expected in &events {
            let got = read_event(&mut cursor).unwrap().unwrap();
            assert_eq!(&got, expected);
        }
        assert!(read_event(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn recorder_header_and_player_agree_on_initial_table() {
        let (mut chip, rom) = synthetic_bus_and_rom();
        let bus = MachineBus::new(&mut chip, &rom);

        let dir = std::env::temp_dir();
        let path = dir.join(format!("replay-test-{}.log", std::process::id()));

        {
            let mut rec = Recorder::create(&path, &bus, &rom, 424, DEFAULT_CHECKPOINT_INTERVAL)
                .expect("recorder creation");
            rec.finish(
                0,
                Regs {
                    d: [0; 8],
                    a: [0; 8],
                    pc: 0,
                    sr: 0,
                },
                &bus,
            )
            .unwrap();
        }

        let player = Player::open(&path).expect("player open");
        assert_eq!(player.header().rom_hash, fnv1a64(&rom));
        assert_eq!(player.header().instructions_per_line, 424);
        // Overlay is mapped at construction, so low chip RAM is Io.
        assert_eq!(player.classify(0), PageType::Io);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn player_asserts_io_read_address_and_reports_a_mismatch() {
        let (mut chip, rom) = synthetic_bus_and_rom();
        let bus = MachineBus::new(&mut chip, &rom);
        let dir = std::env::temp_dir();
        let path = dir.join(format!("replay-test-assert-{}.log", std::process::id()));

        {
            let mut rec = Recorder::create(&path, &bus, &rom, 424, DEFAULT_CHECKPOINT_INTERVAL)
                .expect("recorder creation");
            rec.advance_ordinal();
            rec.log_io_read(0x00DFF006, 2, 0x1234).unwrap();
            rec.finish(
                1,
                Regs {
                    d: [0; 8],
                    a: [0; 8],
                    pc: 0,
                    sr: 0,
                },
                &bus,
            )
            .unwrap();
        }

        let mut player = Player::open(&path).unwrap();
        player.advance_ordinal();
        // Correct address: matches.
        let mut player2 = Player::open(&path).unwrap();
        player2.advance_ordinal();
        let ok = player2.expect_io_read(0x00DFF006, 2, 0xDEAD).unwrap();
        assert_eq!(ok, Ok(0x1234));

        // Wrong address: reports a mismatch, not a panic.
        let bad = player.expect_io_read(0x00DFF008, 2, 0xDEAD).unwrap();
        assert!(matches!(bad, Err(ref e) if matches!(**e, Divergence::IoReadMismatch { .. })));

        let _ = std::fs::remove_file(&path);
    }
}
