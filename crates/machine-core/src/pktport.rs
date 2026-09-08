//! `pktport` — the DosPacket transport card of ADR 0004.
//!
//! Implements `docs/pktport-protocol.md`: a doorbell-plus-descriptor Zorro
//! II card whose data path is the guest's own thin handler stub forwarding
//! AmigaDOS packets to a host-side filesystem service ([`PacketBackend`]).
//! It follows `hostblk.rs`'s shape almost exactly — 4-byte register slots
//! with the live byte at offset +3, a doorbell write that only latches
//! (never does I/O inline), deferred completion serviced from
//! [`Pktport::tick`], INT2 on completion — and this file's docs explain
//! only where `pktport` differs from `hostblk`, not the whole shape again.
//! See `docs/pktport-protocol.md` §1-§3 for the register/wire contract a
//! driver author needs.
//!
//! # Registers vs guest memory, and why there is no transfer engine here
//!
//! `hostblk` moves sector payloads directly between guest RAM and a
//! [`crate::block::BlockDevice`], so it owns a real transfer engine (one
//! whole descriptor's worth of I/O per tick). `pktport` carries no payload
//! of its own at all — protocol doc §1: "no data crosses the register
//! window... names, buffers and `FileInfoBlock`s are read and written
//! directly in guest RAM ... the window carries only registers." So this
//! card's entire job at tick time is: read one 64-byte [request
//! descriptor](docs/pktport-protocol.md §2) out of guest RAM, hand its
//! `ACTION`/`ARG1..ARG7` fields to [`PacketBackend::execute`] verbatim
//! (§8: "the card owns request lifecycle... the backend owns semantics"),
//! and write `RES1`/`RES2`/`STATUS` back into that same descriptor. The
//! backend, not this module, is what actually walks guest buffers, BSTRs
//! and `FileInfoBlock`s through `mem` — this module never interprets an
//! `ACTION` value or an arg's meaning, by design (§8's whole point).
//!
//! # One outstanding request, and why that's simpler than `hostblk`'s rings
//!
//! §3: `CAPACITY` is `1` in this version. `hostblk` needs a submission
//! ring plus an independent completion ring because a driver may queue
//! several transfers ahead of the engine draining them, and the
//! *completion* itself has nowhere natural to land except a queue the
//! driver polls through registers. `pktport` has neither problem: with
//! capacity 1 there is at most one submitted-but-not-yet-serviced request
//! at any moment, so a single `Option<u32>` (the request's descriptor
//! pointer) stands in for `hostblk`'s whole submission ring, and the
//! *completion* is never queued in registers at all — it is written
//! straight into the descriptor the driver already holds a pointer to,
//! which is what makes a completion queue unnecessary here. [`Pktport::
//! tick`] both dequeues and fully executes a request in one call, exactly
//! as `hostblk::Hostblk::tick` does for one submission; the difference is
//! there is only ever zero or one to dequeue.
//!
//! A `DOORBELL` write while a request is already latched (§3, "capacity
//! exhausted") is dropped and counted in an **internal** overflow counter
//! ([`Pktport::doorbell_overflow`]) — never a bus register in this
//! version. §3 says this explicitly: "the register can be added to the
//! map when a deeper queue is." Exposing a counter for a queue depth of
//! one would be one register wide of information a driver that respects
//! `CAPACITY` should never need to read; keeping it host-introspection-only
//! for now (a plain accessor, the same shape `mirage::Mirage::
//! change_count` uses) avoids committing to a register offset the next
//! protocol revision would then be stuck with. This is `hostblk`'s
//! `SUBMIT_OVERFLOW`-style posture applied one register short: dropped
//! input is *never* silent, it is just not yet wired to a wire-visible
//! counter because nothing (no deeper queue) exists yet to make reading
//! one useful.
//!
//! # A dropped descriptor is not a completion
//!
//! §7's phrasing is exact and this module follows it exactly: "`RES2 =
//! 209` for an unreadable descriptor is impossible — an unreadable
//! descriptor cannot be completed, so it is dropped and counted." This is
//! a real divergence from `hostblk`, worth calling out because it is easy
//! to get backwards by analogy: `hostblk::Hostblk::execute` *can* still
//! report `err::BAD_ADDRESS` through its completion queue when the
//! descriptor pointer itself is bad, because that queue is independent
//! host-side state the driver polls through registers — nothing about
//! reporting the error requires writing back into the unreadable
//! descriptor itself. `pktport` has no such independent channel: `RES1`,
//! `RES2` and `STATUS` all live *inside* the descriptor this card just
//! failed to read. There is no address left to write an error code to, so
//! [`Pktport::tick`] does not try — a bad descriptor pointer is dropped
//! and counted in [`Pktport::dropped_descriptors`] (a second internal
//! counter, distinct from [`Pktport::doorbell_overflow`] since the two
//! failure modes are diagnostically different: one is "the driver
//! oversubmitted", the other is "the driver handed over a broken
//! pointer"), `INT_STATUS` is left untouched, and no `PacketBackend` call
//! happens at all. A driver that built the descriptor itself and still
//! holds the address it wrote to `REQ_PTR` was never going to get a
//! completion notification it could locate anyway.
//!
//! Everything *inside* a successfully-read descriptor is a different
//! story: an unreadable buffer, name or `FileInfoBlock` named by an *arg*
//! is the backend's problem, not this card's, and gets a real completion
//! (`RES2 = 210`/a `115`-class code, §7) precisely because `RES1`/`RES2`/
//! `STATUS` all live in a descriptor this card *did* manage to read and
//! can therefore write back into.
//!
//! # Full-descriptor bounds check, up front, before the backend runs
//!
//! The descriptor is 64 bytes (§2's table, `0x00`-`0x40`), but this card
//! only needs the first 32 (`ACTION`+`ARG1..ARG7`) to build a
//! [`PacketBackend::execute`] call. [`Pktport::read_descriptor`]
//! nonetheless validates the *whole* 64-byte span up front — `hostblk`'s
//! own hostile-input discipline, "checked before any host I/O runs, so a
//! bad address never causes a partial operation that then can't be
//! delivered" (`hostblk.rs` module docs, "Buffer bounds"), applied to
//! `RES1`/`RES2`/`STATUS`'s write region rather than a transfer buffer.
//! Validating only the first 32 bytes would let a descriptor that reads
//! fine but whose result region (`0x20`-`0x2C`) hangs off the end of RAM
//! slip past the check, run the backend (a real, possibly stateful
//! filesystem operation — a file could actually get created or deleted),
//! and then have nowhere to report the outcome. Checking the full span
//! first means a hostile `REQ_PTR` can never cause a backend call whose
//! result is unreportable.
//!
//! # `INT_STATUS`'s gate: a real condition here, unlike the register file
//! # first suggests
//!
//! §3 cross-references `input.rs`'s gated write-1-to-clear: "ignored while
//! a completed request has not had its `STATUS`... honoured." `input.rs`'s
//! version gates on its event *queue* being non-empty, because acking
//! early there would strand every event still behind the one just popped
//! with no further interrupt to say so. `pktport` has no queue to strand
//! anything in — but it does have exactly the same shape of race in
//! miniature: the window between a `DOORBELL` write latching a request
//! ([`Pktport::submit`]) and the next [`Pktport::tick`] actually servicing
//! it. This module reads that gate as: **the write-1-to-clear is ignored
//! while a request is latched and not yet serviced** ([`Pktport::busy`]).
//! Concretely this makes the ack a no-op during that narrow window rather
//! than a silent bug: any legitimate completion still lands, because
//! `tick` sets `INT_STATUS` again the moment it actually completes the
//! latched request, regardless of what happened to the bit in between. What
//! the gate buys is the same invariant `input.rs`'s buys, restated for a
//! one-deep queue instead of a sixteen-deep one: a driver's ack can never
//! race ahead of the completion it is meant to be acknowledging and leave
//! this card's latch in a state that doesn't reflect reality.
//!
//! # Hostile input
//!
//! Same posture as `hostblk`/`input`, and §7 spells out the specifics: the
//! descriptor pointer and everything the descriptor's `ACTION`/`ARG1..
//! ARG7` name are guest-hostile. This module's own responsibility is
//! narrower than `hostblk`'s (module docs above: it has no transfer engine
//! and never resolves an arg to a buffer itself), so its hostile-input
//! surface is exactly the descriptor pointer:
//!
//! - **Misaligned or out-of-bounds `REQ_PTR`** (unmapped, straddling two
//!   regions, or `ptr + 0x40` overflowing `u32`): dropped and counted, no
//!   backend call — "A dropped descriptor is not a completion" above.
//! - **Everything past the descriptor pointer** (buffer/name/FIB
//!   addresses named by `ARG1..ARG7`) is the [`PacketBackend`]
//!   implementation's responsibility under §7's rules, via the same `mem:
//!   &mut dyn GuestMemory` this card itself used to read the descriptor —
//!   this module never resolves one of those addresses itself, so it
//!   cannot get that resolution wrong.
//! - **This card must never panic on any descriptor content**, mirroring
//!   `hostblk`'s and `input`'s own rule verbatim (§7's last bullet).

use crate::autoconfig::{BoardSpec, ERTF_DIAGVALID, ERT_ZORROII};

/// One filesystem service behind the `pktport` card (protocol doc §8).
/// `machine-hosted` implements this over the published `amiga-rdb` +
/// `amiga-ffs` crates; this module's own tests implement it over a fixture
/// stub. `#![no_std]`/no-alloc here in `machine-core` — only the trait
/// lives in this crate; an implementation that needs to allocate lives
/// wherever allocation is available.
pub trait PacketBackend {
    /// Execute one request. `args` are the descriptor's `ARG1..ARG7`,
    /// verbatim — this card never interprets them (module docs). Guest
    /// memory access (buffers, BSTRs, `FileInfoBlock`s named by `args`)
    /// goes through `mem` under protocol doc §7's hostile-input rules.
    /// Returns `(res1, res2)`, written back into the descriptor's `RES1`/
    /// `RES2` fields by the caller.
    fn execute(
        &mut self,
        action: u32,
        args: [u32; 7],
        mem: &mut dyn crate::GuestMemory,
    ) -> (u32, u32);
}

/// Reuses `hostblk`'s reserved "hacker" manufacturer ID (NDK 3.2
/// `libraries/configregs.h`, `$7DB`/2011) rather than minting a third
/// stand-in — `hostblk.rs`'s own module docs carry the full story of why
/// `0xFFFF` was tried and rejected by real Kickstart 3.2.2. Product
/// numbers keep every board on this bus distinct regardless.
pub const MANUFACTURER: u16 = crate::hostblk::MANUFACTURER;

/// This card's product number under [`MANUFACTURER`] — distinct from
/// `mirage::PRODUCT` (`0`), `hostblk::PRODUCT` (`1`), `fastram::PRODUCT`
/// (`2`), `input::PRODUCT` (`3`) and `rtgboard`'s own product.
pub const PRODUCT: u8 = 5;

/// The Zorro II AUTOCONFIG window: 64 KB, the smallest available size code
/// — protocol doc §1: "Small on purpose: no data crosses the register
/// window." This card's register file (§3) uses only its first `0x1C`
/// bytes; everything past that, up to [`ROM_BASE`] where [`DIAG_ROM`]
/// starts, is unimplemented board space ([`Pktport::read`]'s `_` arm),
/// the same posture `mirage.rs`/`hostblk.rs` take for their own unused
/// tails.
pub const WINDOW_BYTES: u32 = 0x0001_0000;

/// This card's DiagArea boot ROM (`m68k/pktport-rom/pktport-diagrom.s`),
/// assembled to a flat binary by `../../scripts/build-pktport-rom.sh` and
/// committed here — the same vendored-binary pattern
/// [`crate::hostblk::DIAG_ROM`]/[`crate::input::DIAG_ROM`] use, so
/// `cargo build`/`cargo test` need no m68k toolchain. Docs/pktport-
/// protocol.md §1 documents this ROM's own internal layout (DiagArea +
/// `RtInit` code, then a 4-byte length word, then the guest handler's
/// flat code blob `RtInit` copies out at boot).
pub const DIAG_ROM: &[u8] = include_bytes!("../../../assets/pktport-rom/pktport-diagrom.bin");

/// Where [`DIAG_ROM`] is mapped within this board's own AUTOCONFIG
/// window, once configured — the value [`BoardSpec::init_diag_vec`]
/// advertises. Same offset and same independent-namespace reasoning as
/// `hostblk::ROM_BASE`/`input::ROM_BASE` (each card's own ROM starts at
/// its own board-relative `0x1000`, never a machine-wide address —
/// `hostblk-diagrom.s`'s own file header makes the same point).
pub const ROM_BASE: u32 = 0x1000;

/// The protocol version [`reg::VERSION`] reports (protocol doc §3: "Check
/// it, refuse what you don't know.").
pub const PROTOCOL_VERSION: u32 = 1;

/// The value [`reg::CAPACITY`] reports: one outstanding request (protocol
/// doc §3). Read this, don't hardcode it — the same
/// `hostblk::reg::SUBMIT_CAPACITY` reasoning: a future deeper queue would
/// silently under- or over-drive a driver that assumed `1` forever.
pub const REQUEST_CAPACITY: u32 = 1;

/// The value [`reg::VOL_COUNT`] reports: one volume served (protocol doc
/// §3, §9: multiple volumes per card is explicitly out of scope for
/// version 1).
pub const VOLUME_COUNT: u32 = 1;

/// Byte length of one request descriptor as it lies in guest RAM
/// (protocol doc §2's table: `ACTION`+`ARG1..ARG7` (32 bytes) + `RES1`/
/// `RES2`/`STATUS` (12 bytes) + 20 bytes reserved = 64, all big-endian).
/// [`Pktport::read_descriptor`] validates this *whole* span before
/// touching anything, including the trailing result/reserved region it
/// does not read here — module docs, "Full-descriptor bounds check".
const DESC_LEN: u32 = 0x40;

/// Byte offset within a descriptor of `RES1` (protocol doc §2).
const RES1_OFFSET: u32 = 0x20;
/// Byte offset within a descriptor of `RES2` (protocol doc §2).
const RES2_OFFSET: u32 = 0x24;
/// Byte offset within a descriptor of `STATUS` (protocol doc §2).
const STATUS_OFFSET: u32 = 0x28;

/// Register offsets within this card's AUTOCONFIG window (protocol doc
/// §3's table, same offsets, same order). One hot byte per 4-byte-aligned
/// slot — `hostblk.rs`/`mirage.rs`/`input.rs`'s shared convention, adopted
/// here without re-litigating it.
pub mod reg {
    /// Protocol version, `1`. Read-only.
    pub const VERSION: u32 = 0x00;
    /// Outstanding-request capacity, `1` in this version. Read-only —
    /// read it, don't hardcode it (module docs, [`super::REQUEST_CAPACITY`]).
    pub const CAPACITY: u32 = 0x04;
    /// Guest address of the request descriptor. Write-only (reads `0`,
    /// the same "write-only register reads as unimplemented board space"
    /// convention `hostblk::reg::DOORBELL` uses) — all four bytes carry
    /// real address bits; see [`super::Pktport::write`]'s doc comment for
    /// why every lane matters here.
    pub const REQ_PTR: u32 = 0x08;
    /// Submit the descriptor at [`REQ_PTR`]. Latches on the low-order
    /// byte of this slot; work happens at [`super::Pktport::tick`], never
    /// on this write itself (protocol doc §3, "Deferred completion" —
    /// `hostblk.rs`'s same reasoning applies verbatim). Write-only.
    pub const DOORBELL: u32 = 0x0C;
    /// Bit 0: completion pending. Read / write-1-to-clear, gated while a
    /// request is latched and not yet serviced (module docs, "`INT_STATUS`'s
    /// gate").
    pub const INT_STATUS: u32 = 0x10;
    /// Bit 0 enables INT2 delivery. Read/write.
    pub const INT_ENABLE: u32 = 0x14;
    /// Volumes served, `1` in this version. Read-only.
    pub const VOL_COUNT: u32 = 0x18;
}

/// `pktport`'s register file and request lifecycle. See the module docs
/// for the protocol this implements and the reasoning behind it.
///
/// Borrows its [`PacketBackend`] the way `hostblk::Hostblk` borrows its
/// [`crate::block::BlockDevice`]s: `machine-core` has no allocator, so the
/// backend lives wherever the board layer's storage does, and this struct
/// only ever holds a reference to it.
pub struct Pktport<'a> {
    backend: &'a mut dyn PacketBackend,

    /// Accumulates the four bytes of a [`reg::REQ_PTR`] write. Unlike
    /// `hostblk::reg::DOORBELL` (which triggers on its own low-order
    /// byte because the pointer *is* the doorbell payload there),
    /// `REQ_PTR` here is a plain register: every write lands immediately,
    /// on any lane, via [`set_byte_of`] -- **all 32 bits**, deliberately
    /// re-checked by this module's own
    /// `req_ptr_write_keeps_all_32_bits_not_just_the_low_21` test, the
    /// regression class `hostblk`'s own pointer-register history warns
    /// about (this file's brief calls it out by name).
    req_ptr: u32,
    /// The descriptor pointer latched by the most recent accepted
    /// [`reg::DOORBELL`] write, if [`Pktport::tick`] has not yet serviced
    /// it -- this card's entire submission "ring", one deep (module docs,
    /// "One outstanding request").
    pending: Option<u32>,
    /// Doorbell writes dropped because [`Self::pending`] was already
    /// occupied (module docs). Host-introspection only in this protocol
    /// version -- not yet a bus register (protocol doc §3).
    doorbell_overflow: u32,
    /// Descriptors dropped because [`Self::read_descriptor`] could not
    /// read them at all (module docs, "A dropped descriptor is not a
    /// completion"). Distinct from [`Self::doorbell_overflow`]: one
    /// counts an oversubmitting driver, the other a broken pointer.
    dropped_descriptors: u32,

    int_status: u8,
    int_enable: u8,
}

impl<'a> Pktport<'a> {
    /// Build a card over a caller-owned [`PacketBackend`] -- borrowed, not
    /// boxed, the same shape every other card on this bus borrows its
    /// backing store (this crate has no allocator).
    pub fn new(backend: &'a mut dyn PacketBackend) -> Self {
        Self {
            backend,
            req_ptr: 0,
            pending: None,
            doorbell_overflow: 0,
            dropped_descriptors: 0,
            int_status: 0,
            int_enable: 0,
        }
    }

    /// The `BoardSpec` this card registers on the AUTOCONFIG chain: one
    /// Zorro II board, 64 KB, with a DiagArea (`ERTF_DIAGVALID`) pointing
    /// at [`DIAG_ROM`] — this increment's whole point: a machine with
    /// this card and `--pktvol` gets a working `PKT0:` from cold boot,
    /// with no `L:pktport-handler` file and no Mountlist entry
    /// (`m68k/pktport-rom/pktport-diagrom.s`'s own header). Same shape
    /// `hostblk::Hostblk::board_spec`/`input::Input::board_spec` already
    /// use for their own boot ROMs.
    pub fn board_spec() -> BoardSpec {
        BoardSpec {
            board_type: ERT_ZORROII | ERTF_DIAGVALID | 0x01, // size code 1 = 64 KB
            product: PRODUCT,
            flags: 0,
            manufacturer: MANUFACTURER,
            serial: 0,
            init_diag_vec: ROM_BASE as u16, // fits: ROM_BASE (0x1000) << u16::MAX
            size_bytes: WINDOW_BYTES,
        }
    }

    /// Doorbell writes dropped for arriving while a request was already
    /// latched (module docs). Host-side introspection, not a register.
    pub fn doorbell_overflow(&self) -> u32 {
        self.doorbell_overflow
    }

    /// Descriptors dropped because their pointer could not be read at all
    /// (module docs, "A dropped descriptor is not a completion").
    /// Host-side introspection, not a register.
    pub fn dropped_descriptors(&self) -> u32 {
        self.dropped_descriptors
    }

    /// Whether a request is latched and not yet serviced by [`Self::tick`]
    /// -- module docs, "`INT_STATUS`'s gate".
    fn busy(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether `pktport` is asserting its interrupt, which the bus routes
    /// to INT2 (`PORTS`), the same as every other native card here.
    pub fn irq_pending(&self) -> bool {
        self.int_enable & 1 != 0 && self.int_status != 0
    }

    /// Latch a [`reg::DOORBELL`] write: occupy [`Self::pending`] with the
    /// current [`Self::req_ptr`], or drop-and-count if a request is
    /// already latched (module docs). Deliberately does **not** touch
    /// `mem` at all -- reading the descriptor is [`Self::tick`]'s job,
    /// once it is actually about to run (protocol doc §3, "Deferred
    /// completion").
    fn submit(&mut self) {
        if self.pending.is_some() {
            self.doorbell_overflow = self.doorbell_overflow.wrapping_add(1);
        } else {
            self.pending = Some(self.req_ptr);
        }
    }

    /// Advance the engine by one step: service at most one latched
    /// request against `mem` (module docs, "One outstanding request").
    /// Called once per [`crate::MachineBus::tick`], mirroring
    /// `hostblk::Hostblk::tick`/`mirage::Mirage::tick`.
    pub fn tick(&mut self, mem: &mut dyn crate::GuestMemory) {
        let Some(ptr) = self.pending.take() else {
            return;
        };
        let Some((action, args)) = Self::read_descriptor(ptr, mem) else {
            // Module docs, "A dropped descriptor is not a completion":
            // there is no address left to report an error to, so this is
            // dropped and counted, not completed with an error code.
            self.dropped_descriptors = self.dropped_descriptors.wrapping_add(1);
            return;
        };
        let (res1, res2) = self.backend.execute(action, args, mem);
        Self::write_results(mem, ptr, res1, res2);
        // Latches independent of `int_enable` -- the same "the request
        // bit always latches, the enable bit only gates whether the CPU
        // sees it" split `input.rs` documents at length.
        self.int_status = 1;
    }

    /// Read and validate one descriptor out of `mem` at `ptr`: `None` for
    /// a misaligned pointer or a span that doesn't fit entirely inside
    /// mapped guest RAM (module docs, "Full-descriptor bounds check" --
    /// the *whole* [`DESC_LEN`]-byte descriptor is checked, not just the
    /// `ACTION`/`ARG1..ARG7` prefix this function actually reads). On
    /// success, returns `ACTION` and `ARG1..ARG7` verbatim -- this module
    /// never interprets them (module docs).
    fn read_descriptor(ptr: u32, mem: &dyn crate::GuestMemory) -> Option<(u32, [u32; 7])> {
        if !ptr.is_multiple_of(4) {
            return None;
        }
        let d = mem.ram_slice(ptr, DESC_LEN)?;
        let action = u32::from_be_bytes(d[0..4].try_into().unwrap());
        let mut args = [0u32; 7];
        for (i, arg) in args.iter_mut().enumerate() {
            let off = 4 + i * 4;
            *arg = u32::from_be_bytes(d[off..off + 4].try_into().unwrap());
        }
        Some((action, args))
    }

    /// Write `RES1`, then `RES2`, then `STATUS = 1` into the descriptor at
    /// `ptr` -- protocol doc §2: "The host writes `RES1`/`RES2` first,
    /// `STATUS` last... the stub may therefore trust `RES1`/`RES2` the
    /// moment it observes `STATUS == 1`." All three writes target a span
    /// already proven to fit inside guest RAM by [`Self::read_descriptor`]
    /// (module docs, "Full-descriptor bounds check"), so a `None` here
    /// would mean that invariant broke; each write is still guarded
    /// rather than `unwrap`ped; per this crate's "never panic on guest
    /// input" discipline, silently doing nothing is the right fallback
    /// for a case that should be unreachable, not a panic.
    fn write_results(mem: &mut dyn crate::GuestMemory, ptr: u32, res1: u32, res2: u32) {
        if let Some(slot) = mem.ram_slice_mut(ptr + RES1_OFFSET, 4) {
            slot.copy_from_slice(&res1.to_be_bytes());
        }
        if let Some(slot) = mem.ram_slice_mut(ptr + RES2_OFFSET, 4) {
            slot.copy_from_slice(&res2.to_be_bytes());
        }
        if let Some(slot) = mem.ram_slice_mut(ptr + STATUS_OFFSET, 4) {
            slot.copy_from_slice(&1u32.to_be_bytes());
        }
    }

    /// Read a byte of the register file, offset from this board's
    /// configured AUTOCONFIG base. See [`reg`] for the layout; anything
    /// unnamed (including [`reg::REQ_PTR`]/[`reg::DOORBELL`], both
    /// write-only) reads `0` -- unimplemented board space inside this
    /// card's own window, the same posture `hostblk.rs`/`mirage.rs`/
    /// `input.rs` all take.
    pub fn read(&self, offset: u32) -> u8 {
        match offset {
            o if in_slot(o, reg::VERSION) => byte_of(PROTOCOL_VERSION, o - reg::VERSION),
            o if in_slot(o, reg::CAPACITY) => byte_of(REQUEST_CAPACITY, o - reg::CAPACITY),
            o if in_slot(o, reg::INT_STATUS) => low_byte(o, reg::INT_STATUS, self.int_status),
            o if in_slot(o, reg::INT_ENABLE) => low_byte(o, reg::INT_ENABLE, self.int_enable),
            o if in_slot(o, reg::VOL_COUNT) => byte_of(VOLUME_COUNT, o - reg::VOL_COUNT),
            o if (ROM_BASE..ROM_BASE + DIAG_ROM.len() as u32).contains(&o) => {
                DIAG_ROM[(o - ROM_BASE) as usize]
            }
            _ => 0,
        }
    }

    /// Write a byte of the register file. See [`Self::read`] for the
    /// offset layout.
    ///
    /// [`reg::REQ_PTR`] accepts every lane via [`set_byte_of`], not just
    /// the low-order one -- `hostblk.rs`'s module docs record a real bug
    /// class here: an earlier pointer register on this project once
    /// truncated to its low-order lanes and silently dropped the high
    /// bits of a 32-bit guest address. This card's own
    /// `req_ptr_write_keeps_all_32_bits_not_just_the_low_21` test guards
    /// against the same class recurring here.
    pub fn write(&mut self, offset: u32, value: u8) {
        match offset {
            o if in_slot(o, reg::REQ_PTR) => {
                set_byte_of(&mut self.req_ptr, o - reg::REQ_PTR, value)
            }
            o if in_slot(o, reg::DOORBELL) && o - reg::DOORBELL == 3 => self.submit(),
            o if in_slot(o, reg::DOORBELL) => {}
            o if in_slot(o, reg::INT_STATUS) && o - reg::INT_STATUS == 3 => {
                // Write-1-to-clear, gated while a request is latched and
                // not yet serviced -- module docs, "`INT_STATUS`'s gate".
                if value & 1 != 0 && !self.busy() {
                    self.int_status = 0;
                }
            }
            o if in_slot(o, reg::INT_STATUS) => {}
            o if in_slot(o, reg::INT_ENABLE) && o - reg::INT_ENABLE == 3 => {
                self.int_enable = value;
            }
            o if in_slot(o, reg::INT_ENABLE) => {}
            _ => {}
        }
    }
}

/// Whether `offset` falls in the 4-byte-aligned slot starting at `base`.
fn in_slot(offset: u32, base: u32) -> bool {
    (base..base + 4).contains(&offset)
}

/// A single-byte register's value at offset `base + 3` (the low-order
/// byte of the slot), `0` at the other three offsets in the slot --
/// `hostblk.rs`/`mirage.rs`/`input.rs`'s shared convention.
fn low_byte(offset: u32, base: u32, value: u8) -> u8 {
    if offset - base == 3 {
        value
    } else {
        0
    }
}

/// Byte `lane` (0 = most significant) of a big-endian 32-bit register.
fn byte_of(value: u32, lane: u32) -> u8 {
    (value >> (8 * (3 - lane))) as u8
}

/// Replace byte `lane` of a big-endian 32-bit register, leaving the other
/// three bytes untouched.
fn set_byte_of(value: &mut u32, lane: u32, byte: u8) {
    let shift = 8 * (3 - lane);
    *value = (*value & !(0xFFu32 << shift)) | ((byte as u32) << shift);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GuestMemory;

    // ---- a small guest-RAM stand-in --------------------------------------

    /// Flat guest RAM based at 0, the same fixture shape `hostblk.rs`'s
    /// own tests use -- a distinct newtype rather than a second
    /// `impl GuestMemory for Vec<u8>`, since trait impls are crate-global
    /// regardless of module and `hostblk.rs` already defines that one for
    /// its own tests (a second direct impl here would conflict with it).
    struct FakeRam(std::vec::Vec<u8>);

    impl FakeRam {
        fn new(size: usize) -> Self {
            Self(std::vec![0u8; size])
        }
    }

    impl GuestMemory for FakeRam {
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

    /// A [`PacketBackend`] stub that records exactly what it was called
    /// with and answers a fixed, recognisable `(res1, res2)` pair --
    /// enough to prove the card passed `action`/`args` through verbatim
    /// and wrote its answer back at the right offsets, without this test
    /// module needing any real filesystem semantics.
    struct StubBackend {
        calls: u32,
        last_action: Option<u32>,
        last_args: [u32; 7],
        /// If set, returned verbatim; otherwise a fixed recognisable pair.
        answer: Option<(u32, u32)>,
    }

    impl StubBackend {
        fn new() -> Self {
            Self {
                calls: 0,
                last_action: None,
                last_args: [0; 7],
                answer: None,
            }
        }
    }

    impl PacketBackend for StubBackend {
        fn execute(
            &mut self,
            action: u32,
            args: [u32; 7],
            _mem: &mut dyn GuestMemory,
        ) -> (u32, u32) {
            self.calls += 1;
            self.last_action = Some(action);
            self.last_args = args;
            self.answer.unwrap_or((0x1111_2222, 0x3333_4444))
        }
    }

    /// Write a descriptor's `ACTION`/`ARG1..ARG7` (the first 32 bytes) at
    /// `ptr` in `ram`. `RES1`/`RES2`/`STATUS`/reserved are left as
    /// whatever `ram` already held (zeroed by [`FakeRam::new`]), matching
    /// "guest writes 0" for the reserved tail (protocol doc §2).
    fn put_descriptor(ram: &mut FakeRam, ptr: u32, action: u32, args: [u32; 7]) {
        let base = ptr as usize;
        ram.0[base..base + 4].copy_from_slice(&action.to_be_bytes());
        for (i, arg) in args.iter().enumerate() {
            let off = base + 4 + i * 4;
            ram.0[off..off + 4].copy_from_slice(&arg.to_be_bytes());
        }
    }

    fn write_req_ptr(dev: &mut Pktport, ptr: u32) {
        let b = ptr.to_be_bytes();
        dev.write(reg::REQ_PTR, b[0]);
        dev.write(reg::REQ_PTR + 1, b[1]);
        dev.write(reg::REQ_PTR + 2, b[2]);
        dev.write(reg::REQ_PTR + 3, b[3]);
    }

    fn ring_doorbell(dev: &mut Pktport) {
        dev.write(reg::DOORBELL + 3, 0);
    }

    fn read_u32(dev: &Pktport, base: u32) -> u32 {
        u32::from_be_bytes([
            dev.read(base),
            dev.read(base + 1),
            dev.read(base + 2),
            dev.read(base + 3),
        ])
    }

    fn ram_u32(ram: &FakeRam, addr: u32) -> u32 {
        u32::from_be_bytes(ram.0[addr as usize..addr as usize + 4].try_into().unwrap())
    }

    // ---- register reads ----------------------------------------------------

    #[test]
    fn version_register_reports_the_documented_protocol_version() {
        let mut backend = StubBackend::new();
        let dev = Pktport::new(&mut backend);
        assert_eq!(read_u32(&dev, reg::VERSION), PROTOCOL_VERSION);
        assert_ne!(PROTOCOL_VERSION, 0);
    }

    #[test]
    fn capacity_register_reports_one_outstanding_request() {
        let mut backend = StubBackend::new();
        let dev = Pktport::new(&mut backend);
        assert_eq!(read_u32(&dev, reg::CAPACITY), REQUEST_CAPACITY);
        assert_eq!(REQUEST_CAPACITY, 1);
    }

    #[test]
    fn vol_count_register_reports_one_volume() {
        let mut backend = StubBackend::new();
        let dev = Pktport::new(&mut backend);
        assert_eq!(read_u32(&dev, reg::VOL_COUNT), VOLUME_COUNT);
        assert_eq!(VOLUME_COUNT, 1);
    }

    // ---- the full happy path -------------------------------------------------

    #[test]
    fn a_full_request_round_trips_through_the_backend_and_into_the_descriptor() {
        let mut backend = StubBackend::new();
        backend.answer = Some((0xDEAD_BEEF, 0xCAFE_F00D));
        let mut dev = Pktport::new(&mut backend);
        dev.write(reg::INT_ENABLE + 3, 1);

        let mut ram = FakeRam::new(64 * 1024);
        let desc_addr = 0x100u32;
        let args = [1, 2, 3, 4, 5, 6, 7];
        put_descriptor(&mut ram, desc_addr, 8 /* ACTION_LOCATE_OBJECT */, args);

        write_req_ptr(&mut dev, desc_addr);
        assert!(!dev.irq_pending(), "not yet -- still queued");
        ring_doorbell(&mut dev);
        assert!(
            !dev.irq_pending(),
            "the doorbell write itself must do no work"
        );

        dev.tick(&mut ram);

        // RES1, RES2 and STATUS landed at the documented offsets,
        // big-endian, in the descriptor itself -- which only happens if
        // the backend actually ran and answered.
        assert_eq!(ram_u32(&ram, desc_addr + 0x20), 0xDEAD_BEEF, "RES1");
        assert_eq!(ram_u32(&ram, desc_addr + 0x24), 0xCAFE_F00D, "RES2");
        assert_eq!(ram_u32(&ram, desc_addr + 0x28), 1, "STATUS == complete");

        assert_eq!(read_u32(&dev, reg::INT_STATUS), 1);
        assert!(dev.irq_pending());
        assert_eq!(backend.calls, 1, "the backend ran exactly once");
    }

    #[test]
    fn backend_receives_action_and_every_arg_verbatim() {
        let mut backend = StubBackend::new();
        let mut dev = Pktport::new(&mut backend);
        let mut ram = FakeRam::new(64 * 1024);
        let args = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77];
        put_descriptor(&mut ram, 0x200, 0x52 /* 'R' */, args);
        write_req_ptr(&mut dev, 0x200);
        ring_doorbell(&mut dev);
        dev.tick(&mut ram);

        assert_eq!(backend.calls, 1);
        assert_eq!(backend.last_action, Some(0x52));
        assert_eq!(backend.last_args, args);
    }

    // ---- doorbell overflow: capacity 1, two doorbells before a tick -------

    #[test]
    fn a_second_doorbell_before_the_first_ticks_is_counted_as_overflow() {
        let mut backend = StubBackend::new();
        let mut dev = Pktport::new(&mut backend);
        let mut ram = FakeRam::new(64 * 1024);

        let first_addr = 0x100u32;
        let second_addr = 0x200u32;
        put_descriptor(&mut ram, first_addr, 1, [0; 7]);
        put_descriptor(&mut ram, second_addr, 2, [0; 7]);

        write_req_ptr(&mut dev, first_addr);
        ring_doorbell(&mut dev);
        assert_eq!(dev.doorbell_overflow(), 0);

        // A second doorbell before the engine has ticked: the request
        // slot is already occupied, so this one is dropped and counted.
        write_req_ptr(&mut dev, second_addr);
        ring_doorbell(&mut dev);
        assert_eq!(dev.doorbell_overflow(), 1, "dropped, counted");

        dev.tick(&mut ram);

        // Only the first request actually ran.
        assert_eq!(backend.calls, 1);
        assert_eq!(backend.last_action, Some(1), "the first request executed");
        assert_eq!(ram_u32(&ram, first_addr + 0x28), 1, "first STATUS complete");
        assert_eq!(
            ram_u32(&ram, second_addr + 0x28),
            0,
            "second descriptor untouched -- it was never accepted"
        );
    }

    // ---- hostile descriptors --------------------------------------------------

    #[test]
    fn unmapped_req_ptr_is_dropped_and_counted_without_touching_the_backend() {
        let mut backend = StubBackend::new();
        let mut dev = Pktport::new(&mut backend);
        let mut ram = FakeRam::new(64 * 1024);

        write_req_ptr(&mut dev, ram.0.len() as u32); // one past the end
        ring_doorbell(&mut dev);
        dev.tick(&mut ram);
        assert_eq!(dev.dropped_descriptors(), 1);
        assert_eq!(dev.doorbell_overflow(), 0, "not an overflow, a bad pointer");
        assert!(
            !dev.irq_pending(),
            "no completion for an unreachable descriptor"
        );
        assert_eq!(backend.calls, 0, "the backend must never see this request");
    }

    #[test]
    fn a_descriptor_straddling_the_end_of_ram_is_dropped_and_counted() {
        let mut backend = StubBackend::new();
        let mut dev = Pktport::new(&mut backend);
        let mut ram = FakeRam::new(64 * 1024);

        // 4-byte aligned, but the full 64-byte descriptor runs past the
        // end of RAM even though its first bytes are readable.
        let ptr = ram.0.len() as u32 - 0x20;
        write_req_ptr(&mut dev, ptr);
        ring_doorbell(&mut dev);
        dev.tick(&mut ram);
        assert_eq!(dev.dropped_descriptors(), 1);
        assert_eq!(
            backend.calls, 0,
            "a straddling descriptor must never reach the backend"
        );
    }

    #[test]
    fn a_pointer_near_u32_max_does_not_overflow_or_panic() {
        let mut backend = StubBackend::new();
        let mut dev = Pktport::new(&mut backend);
        let mut ram = FakeRam::new(64 * 1024);

        write_req_ptr(&mut dev, 0xFFFF_FFF0);
        ring_doorbell(&mut dev);
        dev.tick(&mut ram); // must not panic
        assert_eq!(dev.dropped_descriptors(), 1);
        assert_eq!(backend.calls, 0);
    }

    #[test]
    fn a_misaligned_req_ptr_is_dropped_and_counted() {
        let mut backend = StubBackend::new();
        let mut dev = Pktport::new(&mut backend);
        let mut ram = FakeRam::new(64 * 1024);
        put_descriptor(&mut ram, 0x104, 1, [0; 7]);

        write_req_ptr(&mut dev, 0x105); // not 4-byte aligned
        ring_doorbell(&mut dev);
        dev.tick(&mut ram);
        assert_eq!(dev.dropped_descriptors(), 1);
        assert_eq!(backend.calls, 0);
    }

    // ---- the write-1-to-clear gate ----------------------------------------

    #[test]
    fn int_status_ack_is_ignored_while_a_request_is_latched_and_unserviced() {
        let mut backend = StubBackend::new();
        let mut dev = Pktport::new(&mut backend);
        let mut ram = FakeRam::new(64 * 1024);
        put_descriptor(&mut ram, 0x100, 1, [0; 7]);

        write_req_ptr(&mut dev, 0x100);
        ring_doorbell(&mut dev); // now busy: latched, not yet ticked
        dev.write(reg::INT_STATUS + 3, 1); // ack attempt while busy
        assert_eq!(
            read_u32(&dev, reg::INT_STATUS),
            0,
            "nothing was pending yet, so this ack is a no-op either way"
        );

        dev.tick(&mut ram); // completes; INT_STATUS latches to 1
        assert_eq!(read_u32(&dev, reg::INT_STATUS), 1);

        // Ack once no request is latched: honoured.
        dev.write(reg::INT_STATUS + 3, 1);
        assert_eq!(read_u32(&dev, reg::INT_STATUS), 0, "ack honoured once idle");
    }

    #[test]
    fn int_status_ack_while_a_second_request_is_mid_flight_is_ignored() {
        let mut backend = StubBackend::new();
        let mut dev = Pktport::new(&mut backend);
        let mut ram = FakeRam::new(64 * 1024);
        put_descriptor(&mut ram, 0x100, 1, [0; 7]);
        put_descriptor(&mut ram, 0x200, 2, [0; 7]);

        write_req_ptr(&mut dev, 0x100);
        ring_doorbell(&mut dev);
        dev.tick(&mut ram); // first completion lands, INT_STATUS = 1

        write_req_ptr(&mut dev, 0x200);
        ring_doorbell(&mut dev); // second request latched, busy again

        dev.write(reg::INT_STATUS + 3, 1); // ack while busy: ignored
        assert_eq!(
            read_u32(&dev, reg::INT_STATUS),
            1,
            "gated -- the first completion's flag must not be clearable \
             while a second request is already in flight"
        );

        dev.tick(&mut ram); // second completion lands too
        assert_eq!(read_u32(&dev, reg::INT_STATUS), 1);
        dev.write(reg::INT_STATUS + 3, 1);
        assert_eq!(read_u32(&dev, reg::INT_STATUS), 0);
    }

    // ---- the card never interprets ACTION -------------------------------------

    #[test]
    fn an_unknown_action_passes_through_untouched_to_the_backend() {
        let mut backend = StubBackend::new();
        backend.answer = Some((0, 209)); // DOSFALSE / ERROR_ACTION_NOT_KNOWN
        let mut dev = Pktport::new(&mut backend);
        let mut ram = FakeRam::new(64 * 1024);
        let bogus_action = 0xFFFF_0000;
        put_descriptor(&mut ram, 0x100, bogus_action, [0; 7]);

        write_req_ptr(&mut dev, 0x100);
        ring_doorbell(&mut dev);
        dev.tick(&mut ram);

        assert_eq!(
            backend.last_action,
            Some(bogus_action),
            "the card must hand the backend the raw action, not reject it itself"
        );
        assert_eq!(ram_u32(&ram, 0x100 + 0x24), 209, "the backend's own answer");
    }

    // ---- 32-bit REQ_PTR: no low-order truncation -------------------------------

    #[test]
    fn req_ptr_write_keeps_all_32_bits_not_just_the_low_21() {
        // Regression for the truncation class `hostblk.rs`'s module docs
        // warn about: a descriptor placed comfortably above 16 MB (2^21
        // bits' worth) must still be addressed correctly.
        let mut backend = StubBackend::new();
        let mut dev = Pktport::new(&mut backend);
        let big_ram_size = 20 * 1024 * 1024; // 20 MB
        let mut ram = FakeRam::new(big_ram_size);
        let desc_addr = 0x0130_0000u32; // ~19.75 MB: past 2^21 (2 MB) many times over
        put_descriptor(&mut ram, desc_addr, 42, [9, 8, 7, 6, 5, 4, 3]);

        write_req_ptr(&mut dev, desc_addr);
        ring_doorbell(&mut dev);
        dev.tick(&mut ram);

        assert_eq!(
            backend.calls, 1,
            "the high-address descriptor must be reached"
        );
        assert_eq!(backend.last_action, Some(42));
        assert_eq!(
            ram_u32(&ram, desc_addr + 0x28),
            1,
            "STATUS complete at the real address"
        );
    }

    // ---- board identity --------------------------------------------------------

    #[test]
    fn board_spec_carries_the_documented_zorro_ii_identity() {
        let spec = Pktport::board_spec();
        assert_eq!(spec.manufacturer, MANUFACTURER);
        assert_eq!(spec.product, PRODUCT);
        assert_eq!(spec.size_bytes, WINDOW_BYTES);
        assert_eq!(spec.board_type & 0xC0, crate::autoconfig::ERT_ZORROII);
    }

    #[test]
    fn unimplemented_offsets_read_zero_not_open_bus() {
        let mut backend = StubBackend::new();
        let dev = Pktport::new(&mut backend);
        assert_eq!(dev.read(reg::VOL_COUNT + 4), 0, "one past VOL_COUNT's slot");
        assert_eq!(
            dev.read(ROM_BASE + DIAG_ROM.len() as u32),
            0,
            "past the end of the ROM, still inside the window"
        );
        assert_eq!(dev.read(reg::REQ_PTR + 3), 0, "REQ_PTR is write-only");
        assert_eq!(dev.read(reg::DOORBELL + 3), 0, "DOORBELL is write-only");
    }

    // ---- DiagArea boot ROM (m68k/pktport-rom/pktport-diagrom.s) -----------
    //
    // Mirrors hostblk.rs's/input.rs's own diag-serving tests -- same
    // structure, same reasoning: prove the ROM is reachable byte-for-byte
    // through the board window, at the offset `BoardSpec::init_diag_vec`
    // actually advertises, and that its own DiagArea header is internally
    // consistent with what expansion.library will do with it.

    #[test]
    fn board_spec_carries_a_diagarea_pointing_at_rom_base() {
        let spec = Pktport::board_spec();
        assert_eq!(
            spec.board_type & ERTF_DIAGVALID,
            ERTF_DIAGVALID,
            "must advertise ERTF_DIAGVALID for expansion.library to look at init_diag_vec at all"
        );
        assert_eq!(
            spec.init_diag_vec, ROM_BASE as u16,
            "init_diag_vec is the board-relative byte address to find the DiagArea -- must point at ROM_BASE"
        );
    }

    #[test]
    fn diag_rom_is_served_byte_for_byte_at_rom_base() {
        let mut backend = StubBackend::new();
        let dev = Pktport::new(&mut backend);
        for (i, &expected) in DIAG_ROM.iter().enumerate() {
            assert_eq!(
                dev.read(ROM_BASE + i as u32),
                expected,
                "byte {i} of DIAG_ROM must be served verbatim through the board window"
            );
        }
    }

    #[test]
    fn diag_rom_header_matches_the_documented_diagarea_layout() {
        // Pulls the same fields expansion.library itself would out of
        // DIAG_ROM the same way hostblk.rs's/input.rs's own equivalent
        // tests do -- libraries/configregs.h's struct DiagArea, 14 bytes:
        // da_Config, da_Flags, da_Size, da_DiagPoint, da_BootPoint,
        // da_Name, da_Reserved01, da_Reserved02.
        let rom = DIAG_ROM;
        assert!(
            rom.len() >= 14,
            "DIAG_ROM must hold at least the DiagArea header"
        );

        const DAC_WORDWIDE: u8 = 0x80;
        const DAC_CONFIGTIME: u8 = 0x10;
        assert_eq!(
            rom[0],
            DAC_WORDWIDE | DAC_CONFIGTIME,
            "da_Config must carry DAC_CONFIGTIME -- both sibling ROMs' own \
             header lore: without it, nothing here would even run at cold-start"
        );
        assert_eq!(rom[1], 0, "da_Flags: none defined, must be 0");

        let da_size = u16::from_be_bytes([rom[2], rom[3]]);
        let da_diag_point = u16::from_be_bytes([rom[4], rom[5]]);
        let da_boot_point = u16::from_be_bytes([rom[6], rom[7]]);
        let da_name = u16::from_be_bytes([rom[8], rom[9]]);
        let da_reserved01 = u16::from_be_bytes([rom[10], rom[11]]);
        let da_reserved02 = u16::from_be_bytes([rom[12], rom[13]]);

        assert!(
            (da_size as usize) <= rom.len(),
            "da_Size (bytes copied into RAM) must not claim more than this ROM holds"
        );
        assert_ne!(
            da_diag_point, 0,
            "a zero da_DiagPoint means 'no diagnostic code'"
        );
        assert!(
            (da_diag_point as usize) < da_size as usize,
            "da_DiagPoint must fall inside the copied region"
        );
        assert_ne!(
            da_boot_point, 0,
            "must be non-zero purely to make expansion.library copy the DiagArea at all \
             (pktport-diagrom.s's own header, citing input-diagrom.s's discovery) -- \
             even though this card is never a real BootNode"
        );
        assert!(
            (da_boot_point as usize) < da_size as usize,
            "da_BootPoint must fall inside the copied region"
        );
        assert_eq!(da_name, 0, "no da_Name identifier string in this increment");
        assert_eq!(da_reserved01, 0);
        assert_eq!(da_reserved02, 0);
    }

    #[test]
    fn diag_rom_carries_the_handler_blob_after_its_own_code() {
        // docs/pktport-protocol.md section 1 / pktport-diagrom.s's own
        // header: the ROM's own code region ends with a 4-byte
        // big-endian blob length (BlobLenWord), immediately followed by
        // that many bytes of the handler's flat -Fbin code, byte-for-
        // byte. Anchored via CARGO_MANIFEST_DIR (this project's own
        // fixture-path lore), not CWD, against the same committed
        // pktport-handler.bin scripts/build-pktport-rom.sh itself
        // appends -- this test fails loudly if the two ever drift apart
        // (a rebuilt ROM with a stale handler blob, or vice versa).
        let handler_bin_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../m68k/pktport-handler/pktport-handler.bin"
        );
        let handler_blob = std::fs::read(handler_bin_path).unwrap_or_else(|e| {
            panic!("{handler_bin_path} must exist (run scripts/build-pktport-handler.sh): {e}")
        });
        assert!(
            !handler_blob.is_empty(),
            "the handler blob must not be empty"
        );

        let rom = DIAG_ROM;
        assert!(
            rom.len() >= handler_blob.len() + 4,
            "DIAG_ROM must hold at least BlobLenWord plus the whole handler blob"
        );
        let blob_start = rom.len() - handler_blob.len();
        assert_eq!(
            &rom[blob_start..],
            &handler_blob[..],
            "the ROM's trailing bytes must be pktport-handler.bin, byte-for-byte"
        );

        let len_field_offset = blob_start - 4;
        let declared_len = u32::from_be_bytes(
            rom[len_field_offset..len_field_offset + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(
            declared_len as usize,
            handler_blob.len(),
            "BlobLenWord must equal the handler blob's real length"
        );
    }
}
