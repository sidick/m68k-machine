//! MIRAGE — the image-backed Zorro block storage card, block plane only.
//!
//! Implements `~/src/project-ideas/pending/mirage-hdf-card-proposal.md`
//! §4.1 (the block plane) and nothing else. §4.2's management plane
//! (`IDENTIFY`, `LIST_DIR`, `CREATE`, `ATTACH`, the 4 KB window,
//! `DOORBELL`) is unreviewed RFC surface and is deliberately not built
//! here — see the proposal's own §7/§8 for the open questions that still
//! block it. There is no other MIRAGE implementation anywhere yet
//! (the proposal's "second implementation" framing in
//! `docs/m68k-machine-proposal.md` §10.3 assumed a Copperline plugin
//! would exist first; it does not), so **this module is the reference
//! implementation**, not a second opinion on one. Every place below
//! where §4.1 is silent or ambiguous is called out explicitly, because
//! those choices are what the still-open RFC needs to see, not settled
//! fact.
//!
//! # Why this can't be modelled as "Gayle with different offsets"
//!
//! Gayle answers every register access synchronously, in the same bus
//! cycle that touches it, because it is a stand-in for real IDE
//! hardware wired directly to the CPU's bus. MIRAGE is explicitly
//! designed to also run behind a browser-hosted WASM Zorro plugin
//! (Copperline; see the proposal's §1 and §6), where the backing store
//! is **OPFS file I/O, which is async-only** — a plugin cannot satisfy a
//! block read inside the bus access that requested it, no matter how
//! fast the host disk is. Proposal §3 principle 1 ("hardware-first
//! register spec... nothing may rely on emulator conveniences") and
//! principle 2 ("everything long-running is asynchronous") both point
//! the same direction for a real SD-backed MCU too. So this
//! implementation, even though it *could* satisfy every command
//! synchronously (a host file read is effectively instant), deliberately
//! never does: accepting a command always leaves the device `BUSY` until
//! a later call to [`Mirage::tick`] completes it. A driver — or a test —
//! that reads `DATA` or polls `CMD_STATUS` without first letting a tick
//! land will see `BUSY` and nothing else, exactly as it would against a
//! slow SD card or an async plugin. See [`MachineBus::tick`]'s new
//! MIRAGE arm for where that tick comes from.
//!
//! # Register file
//!
//! Per §4.1's table, four bytes apart, block-plane only:
//!
//! | Offset | Register | This implementation |
//! |---|---|---|
//! | `+0x00` | `UNIT_SELECT` | low byte of the slot; 0-7 selects a unit, anything else is treated as "no unit" (see below) |
//! | `+0x04` | `LBA` | full 32-bit big-endian register, as specified |
//! | `+0x08` | `COUNT` | full 32-bit big-endian register (§4.1 doesn't state a width; see below) |
//! | `+0x0C` | `CMD` / `STATUS` | low byte of the slot: command code on write, `BUSY`/`DRQ`/`ERR` on read |
//! | `+0x10` | `DATA` | any byte touch anywhere in the 4-byte slot pops/pushes one byte of the current sector's staging buffer |
//! | `+0x18` | `INT_ENABLE` | low byte of the slot (see below — moved off `+0x18`'s shared row) |
//! | `+0x1C` | `INT_STATUS` | low byte of the slot, **write-1-to-clear** |
//!
//! `+0x14` (between `DATA` and `INT_ENABLE`) is unimplemented board
//! space: it sits inside MIRAGE's own configured AUTOCONFIG window, so
//! it reads `0`/discards writes rather than the wider bus's open-bus
//! `0xFF` — the same posture `gayle.rs`'s task file takes for the
//! offsets between its own byte-wide registers.
//!
//! ## Gaps §4.1 left open, and what this module chose
//!
//! 1. **Which byte of a register carries a single-byte value.** §4.1
//!    gives register *offsets*, not widths or byte-lane placement, for
//!    `UNIT_SELECT`, `CMD`/`STATUS`, and (by the same shared-row shape)
//!    the interrupt registers. `LBA` is explicitly 32-bit; nothing else
//!    is. This bus decomposes a guest `move.l`/`move.w` into independent
//!    byte accesses (`MachineBus::read_byte`/`write_byte`), so *some*
//!    lane has to be "the" byte for anything narrower than 32 bits, and
//!    §4.1 doesn't say which. This module places every single-byte value
//!    in the low-order byte of its 4-byte-aligned slot (the position a
//!    small integer constant naturally lands in when a compiler emits
//!    `move.l #MIRAGE_CMD_READ, CMD_STATUS(a0)`), with the other three
//!    bytes of the slot reserved (read `0`, writes ignored) — the same
//!    "one hot byte per slot" shape `gayle.rs` already uses for its
//!    task-file registers, just at the opposite end of the word. A real
//!    driver's actual code generation is the thing that should settle
//!    this in the RFC, not a guess made here.
//! 2. **`COUNT`'s width.** §4.1 states `LBA` and `DATA` are 32-bit but
//!    is silent on `COUNT` ("sectors per transfer"). Given the uniform
//!    4-byte register spacing, this module treats it as a full 32-bit
//!    register rather than assuming an 8-bit ATA-style count (which
//!    would also need the ATA `0 == 256` convention `gayle.rs` uses —
//!    nothing here suggests MIRAGE should inherit that quirk). `COUNT ==
//!    0` is rejected outright (`ERR`) rather than defined as "everything"
//!    or "nothing": guest-controlled input gets a clean rejection, not a
//!    guessed special case (`blitter.rs`/`gayle.rs` house style).
//! 3. **`INT_ENABLE`/`INT_STATUS` sharing one table row.** §4.1 lists
//!    both under the single offset `+0x18`, the same shape as the
//!    `CMD`/`STATUS` row directly above it. That sharing works for
//!    `CMD`/`STATUS` because command is write-only and status is
//!    read-only, so one address cleanly serves both directions. Enable
//!    and status don't fit that mould: `INT_ENABLE` needs to be both
//!    read back and written (it's a mask the driver sets once and may
//!    inspect), and `INT_STATUS` needs both a read (pending bits) and a
//!    write (to clear them) of its *own* — overlaying that the
//!    `CMD`/`STATUS` way would mean a write to enable the mask and a
//!    write to acknowledge a pending interrupt could never be told
//!    apart. This module gives each its own 4-byte slot, `INT_ENABLE` at
//!    `+0x18` and `INT_STATUS` at `+0x1C`, which is the smallest change
//!    that makes both directions unambiguous while keeping every other
//!    offset in the table exactly where §4.1 put it.
//! 4. **No version/capability register.** Proposal §10.3 says this
//!    machine sets `CAP_DMA`, and this crate's brief asked for that to
//!    be reflected "in whatever version/capability register §4.1
//!    implies" — but §4.1 defines no such register; capability
//!    negotiation is `IDENTIFY`'s job in §4.2's management plane, which
//!    is explicitly out of scope for this increment. An independent
//!    design review of §4.1 (recorded here because it changed this
//!    module's shape) confirmed the same reading and was explicit that
//!    a DMA register bank — an address register and a doorbell, per
//!    §4.5's own description of the path — must not be invented against
//!    an unreviewed spec. There is nowhere in the block plane to put a
//!    capability bit without inventing a register the RFC hasn't
//!    specified, so this module adds none. `CAP_DMA` is **not
//!    represented anywhere** in this increment, and no DMA register bank
//!    exists — see the module-level "What's not here" section below.
//! 5. **A FIFO depth.** §4.1 says `DATA` is "FIFO-backed" but not how
//!    deep. This module uses exactly one sector (512 bytes, matching
//!    `block::SECTOR_BYTES`): it's the unit `BlockDevice` already deals
//!    in, needs no partial-sector bookkeeping, and keeps the
//!    deferred-completion state machine to one `BUSY`-then-`DRQ` step
//!    per sector. A real controller streaming from SD might use a
//!    shallower FIFO to start moving bytes onto the bus before a whole
//!    sector has arrived; nothing here rules that out, it's simply not
//!    what a from-scratch reference implementation needed to invent.
//! 6. **Command values, status bit positions, and the interrupt bit
//!    layout.** §4.1 names the three status conditions (`BUSY`, `DRQ`,
//!    `ERR`) and the three interrupt sources ("xfer complete", "mgmt
//!    complete", "media change") but assigns no bit numbers or command
//!    opcodes to any of them. This module's [`cmd`], [`status`], and
//!    [`int`] modules are an arbitrary but internally consistent
//!    assignment — again, something the RFC should pin down rather than
//!    something worth guessing precisely here.
//! 7. **A command written while a transfer is already in flight.** Not
//!    addressed by §4.1 at all. This module ignores it (leaves the
//!    in-flight transfer alone) rather than aborting, queueing, or
//!    corrupting state — see [`Mirage::write`]'s `CMD_STATUS` arm.
//! 8. **`FLUSH`'s completion signal.** §4.1 names only "xfer complete"
//!    as an interrupt source for the block plane; there is no distinct
//!    "flush complete" bit. This module raises `int::XFER_COMPLETE` for
//!    a completed `FLUSH` too, on the theory that a flush is a
//!    transfer of zero bytes rather than a fourth kind of event.
//! 9. **Manufacturer/product IDs.** Explicitly "TBD" in the proposal
//!    itself (§4). [`MANUFACTURER`]'s doc comment covers the
//!    placeholder chosen here and why it cannot ship as-is.
//! 10. **Unit discovery is entirely unspecified.** §4.1 gives no way to
//!     ask which of units 0-7 are attached, how large they are, or
//!     whether they are write-protected — all of which a real boot ROM's
//!     RDB mounter needs before it can safely issue a single `READ`. An
//!     independent design review of §4.1 called this out explicitly:
//!     "§4.1 as written is not implementable as a driver target" for
//!     exactly this reason, among others (no defined transfer handshake
//!     or error model either, both addressed elsewhere in this module).
//!     The reviewer suggested an `IDENTIFY_UNIT` command reusing the
//!     existing `CMD`/`DATA` path, which is plausible, but it is a spec
//!     decision for the RFC's author, not this increment. What this
//!     module *does* guarantee, and what brief item 4 actually asked
//!     for: a `READ` or `WRITE` to an unattached unit fails cleanly and
//!     observably (`ERR`, immediately, no hang, no garbage) rather than
//!     blocking — see [`Mirage::do_command`]'s absent-unit arm. Discovery
//!     itself remains a gap; no discovery mechanism is invented here.
//!
//! # What's not here
//!
//! - **The management plane** (§4.2) — by design; see the module intro.
//! - **`CAP_DMA`'s actual transfer path.** §4.5 describes bus-master DMA
//!   as "driver writes Amiga RAM address + LBA + count, rings the
//!   doorbell" — that's a `DOORBELL`/management-plane mechanism this
//!   increment doesn't build, and inventing a block-plane-only DMA
//!   trigger would mean designing new protocol the RFC hasn't reviewed.
//!   So there is no DMA path here at all, PIO only, and `CAP_DMA` is
//!   unset anywhere a guest could observe it (see gap 4 above). This is
//!   the sharpest place where this increment falls short of proposal
//!   §10.3's "`CAP_DMA` is set" for this machine — flagged rather than
//!   guessed at.
//! - **`FLUSH` actually flushing anything.** [`crate::block::BlockDevice`]
//!   (reused here per the brief, so `machine-hosted`'s `FileBlockDevice`
//!   works unchanged) has no flush method — every write it accepts is
//!   already synchronous host file I/O. `FLUSH` here is a real
//!   `BUSY`→tick→complete round trip (so a driver that depends on the
//!   handshake existing sees one), but there is nothing underneath for
//!   it to actually order or durably commit. A host-side write cache
//!   (proposal §10.3's "cached mode") would need a real flush on the
//!   trait; that's future work, not invented speculatively here.
//! - **Timing.** [`TICKS_PER_SECTOR`] is "more than zero, and the same
//!   every time" — enough to force a real asynchronous boundary between
//!   accepting a command and it landing, which is the actual
//!   requirement (§3 principle 2; the Copperline/OPFS async constraint
//!   above). It is not a model of SD or bus timing, which §4.6 gives
//!   rough numbers for but which is squarely a hardware/emulator-timing
//!   concern, not something a register-level unit test can honestly
//!   claim to validate. The *protocol* this state machine expresses
//!   allows `DRQ` to deassert between sectors for an arbitrarily long,
//!   variable time — a 300 ms SD stall mid-transfer must show up as
//!   `BUSY`, not be absorbed as a bus wait-state — and this host's fixed
//!   one-tick delay is simply the absence of a timing model to vary it
//!   by, not an assumption that stalls are short or bounded.
//! - **A DMA register bank.** Covered by gap 4 above: no address
//!   register and no doorbell, because §4.1 has neither and inventing
//!   them here would mean designing unreviewed protocol.
//!
//! # Removable media (§3 principle 3)
//!
//! Attach/detach is management-plane work and out of scope, so there is
//! no guest-visible way to trigger a media change yet. What §4.1
//! actually asks for here — a per-unit change counter and the
//! media-change interrupt source — exists: [`Mirage::notify_media_change`]
//! is the host-side hook a future `with_mirage`-style board layer (or
//! this increment's own tests) call in place of the `ATTACH`/`DETACH`
//! commands that will eventually drive it for real.

use crate::autoconfig::{BoardSpec, ERT_ZORROII};
use crate::block::{BlockDevice, SECTOR_BYTES};

/// **Placeholder, but a *reserved* one.** NDK 3.2
/// `libraries/configregs.h` sets aside manufacturer 2011 (`$7DB`) for
/// exactly this: "A special \"hacker\" Manufacturer ID number is
/// reserved for test use: 2011 ($7DB)."
///
/// This was `0xFFFF` until the fast RAM work found real Kickstart 3.2.2
/// **silently rejects** that value for a board it is asked to add to the
/// memory list -- the AUTOCONFIG base-address write simply never lands,
/// with no error anywhere. This card is not a memory board so it never
/// hit that, but carrying an ID the ROM is known to reject is a trap
/// waiting for whoever next sets `ERTF_MEMLIST` or wonders why a board
/// vanished. Product numbers stay distinct, so sharing the manufacturer
/// with `fastram`/`mirage` costs nothing.
///
/// Still a stand-in: whoever carries this into hardware (or ships it as
/// a Copperline plugin, per ADR 0003's MIT-licensing requirement) needs
/// a real registered manufacturer number.
pub const MANUFACTURER: u16 = 0x07DB;

/// MIRAGE's only product number so far; meaningless while
/// [`MANUFACTURER`] is a placeholder.
pub const PRODUCT: u8 = 0;

/// The Zorro II AUTOCONFIG window: 64 KB, the smallest available size
/// code. The block-plane register file above uses only its first 32
/// bytes (module docs); everything past that, up to the end of this
/// window, is unimplemented board space (`Mirage::read`'s `_` arm).
pub const WINDOW_BYTES: u32 = 0x0001_0000;

/// Register offsets, exactly as §4.1's table gives them (module docs
/// explain the `INT_ENABLE`/`INT_STATUS` split and the `+0x14` gap).
pub mod reg {
    pub const UNIT_SELECT: u32 = 0x00;
    pub const LBA: u32 = 0x04;
    pub const COUNT: u32 = 0x08;
    pub const CMD_STATUS: u32 = 0x0C;
    pub const DATA: u32 = 0x10;
    pub const INT_ENABLE: u32 = 0x18;
    pub const INT_STATUS: u32 = 0x1C;
}

/// `CMD_STATUS`'s write side. Values, bit positions and all of
/// [`status`]/[`int`] are this module's own invention (module docs, gap
/// 6) — §4.1 names the conditions, not their encoding.
pub mod cmd {
    pub const READ: u8 = 1;
    pub const WRITE: u8 = 2;
    pub const FLUSH: u8 = 3;
}

/// `CMD_STATUS`'s read side.
pub mod status {
    pub const BUSY: u8 = 1 << 0;
    pub const DRQ: u8 = 1 << 1;
    pub const ERR: u8 = 1 << 2;
}

/// `INT_ENABLE`/`INT_STATUS` bit positions. `MGMT_COMPLETE` is defined
/// (§4.1 names it as one of the three sources) but never set anywhere
/// in this module — there is no management plane yet to complete
/// anything.
pub mod int {
    pub const XFER_COMPLETE: u8 = 1 << 0;
    pub const MGMT_COMPLETE: u8 = 1 << 1;
    pub const MEDIA_CHANGE: u8 = 1 << 2;
}

/// Units `UNIT_SELECT` can address (§4.1: "0-7").
pub const UNIT_COUNT: usize = 8;

/// How many [`Mirage::tick`] calls a pending operation waits through
/// before landing. Module docs' "Timing" section explains why this is a
/// deliberately unmodelled constant rather than an SD/bus latency
/// figure: it exists purely to guarantee a command never completes in
/// the same bus access that issued it.
const TICKS_PER_SECTOR: u8 = 1;

/// Where a pending block-plane operation is in its `BUSY`⇄`DRQ`
/// handshake. Every `Ready` state was reached through a `Pending` state
/// that waited for at least one [`Mirage::tick`] — see the module docs'
/// explanation of why nothing here may complete synchronously.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// No transfer in progress; `CMD_STATUS` reads `0`.
    Idle,
    /// A `READ` is waiting for `tick()` to fetch the current sector.
    ReadPending,
    /// A sector sits in the FIFO; the guest drains it through `DATA`.
    ReadReady,
    /// A `WRITE` is waiting for `tick()` before the FIFO will accept
    /// this sector's bytes.
    WritePending,
    /// The FIFO is accepting bytes for the current sector through
    /// `DATA`.
    WriteReady,
    /// A full sector sits in the FIFO waiting for `tick()` to commit it
    /// to the device.
    WriteCommitting,
    /// `FLUSH` is waiting for `tick()` to complete (module docs: there
    /// is nothing underneath for it to actually flush yet).
    FlushPending,
}

/// MIRAGE's block plane: one AUTOCONFIG board, up to [`UNIT_COUNT`]
/// units, no allocator (borrowed [`BlockDevice`]s, like `gayle.rs`'s
/// single disk).
pub struct Mirage<'a> {
    units: [Option<&'a mut dyn BlockDevice>; UNIT_COUNT],
    /// Per-unit removable-media change counter (§3 principle 3, §4.1's
    /// media-change interrupt source). Bumped only by
    /// [`Mirage::notify_media_change`] — see the module docs' removable
    /// media section for why nothing guest-visible drives it yet.
    change_count: [u32; UNIT_COUNT],

    unit_select: u8,
    lba: u32,
    count: u32,
    status: u8,
    int_enable: u8,
    int_status: u8,

    state: State,
    /// The unit a transfer targets, snapshotted from `unit_select` when
    /// the command was accepted. Guest-hostile input: a driver
    /// re-writing `UNIT_SELECT` mid-transfer (to poll a different unit's
    /// registers, say) must not redirect an operation already in
    /// flight, so completion always reads back through this rather than
    /// `unit_select` directly.
    active_unit: usize,
    current_lba: u32,
    sectors_left: u32,
    ticks_left: u8,

    /// The current sector's staging area. Implemented today as a
    /// destructive FIFO (`buf_pos` only ever advances; a byte is gone
    /// once read) per §4.1's literal "FIFO-backed" wording. A design
    /// review flagged this as a live spec question: a FIFO can't be
    /// resynchronised after an error (a driver that loses count mid-
    /// sector has no way back to byte 0), whereas a plain addressable
    /// buffer in the window could always be re-read from the start.
    /// Only [`Mirage::data_read`]/[`Mirage::data_write`] touch these two
    /// fields; [`State`], [`Mirage::tick`], and every `begin_*`/
    /// `complete_*` method reason purely in terms of "is a sector ready"
    /// (`DRQ`) and "a sector boundary was crossed", never in terms of a
    /// read/write cursor. So swapping this out for a non-destructive
    /// buffer (drop `buf_pos`, index by an offset the guest supplies
    /// instead) is a change to these two fields and their two
    /// accessors, not to the state machine around them.
    sector_buf: [u8; SECTOR_BYTES],
    buf_pos: usize,
}

impl<'a> Mirage<'a> {
    pub fn new() -> Self {
        Self {
            units: [None, None, None, None, None, None, None, None],
            change_count: [0; UNIT_COUNT],
            unit_select: 0,
            lba: 0,
            count: 0,
            status: 0,
            int_enable: 0,
            int_status: 0,
            state: State::Idle,
            active_unit: 0,
            current_lba: 0,
            sectors_left: 0,
            ticks_left: 0,
            sector_buf: [0; SECTOR_BYTES],
            buf_pos: 0,
        }
    }

    /// The `BoardSpec` this card registers on the AUTOCONFIG chain: one
    /// Zorro II board (§4.1: "Z2 first for the same reason as everything
    /// lide").
    pub fn board_spec() -> BoardSpec {
        BoardSpec {
            board_type: ERT_ZORROII | 0x01, // size code 1 = 64 KB
            product: PRODUCT,
            flags: 0,
            manufacturer: MANUFACTURER,
            serial: 0,
            init_diag_vec: 0,
            size_bytes: WINDOW_BYTES,
        }
    }

    /// Attach a disk to unit `unit`. Out-of-range units are silently
    /// ignored -- this is a host-side/board-layer call, not
    /// guest-controlled, but there is still no sensible ninth unit to
    /// attach to.
    pub fn attach_unit(&mut self, unit: u8, device: &'a mut dyn BlockDevice) {
        if let Some(slot) = self.units.get_mut(unit as usize) {
            *slot = Some(device);
        }
    }

    /// Host-side hook standing in for the management-plane
    /// `ATTACH`/`DETACH` commands (module docs, "Removable media"):
    /// bumps `unit`'s change counter and raises the media-change
    /// interrupt source. Out-of-range units do nothing.
    pub fn notify_media_change(&mut self, unit: usize) {
        if let Some(c) = self.change_count.get_mut(unit) {
            *c = c.wrapping_add(1);
            self.int_status |= int::MEDIA_CHANGE;
        }
    }

    /// `unit`'s current change counter, `0` for an out-of-range unit.
    pub fn change_count(&self, unit: usize) -> u32 {
        self.change_count.get(unit).copied().unwrap_or(0)
    }

    /// Whether MIRAGE is asserting its interrupt, which the bus routes
    /// to INT2 (`PORTS`) the same as Gayle and Graffity (module docs).
    pub fn irq_pending(&self) -> bool {
        self.int_status & self.int_enable != 0
    }

    /// Advance the deferred-completion state machine by one step. Called
    /// once per [`crate::MachineBus::tick`], regardless of how many CPU
    /// clocks that tick spans -- see [`TICKS_PER_SECTOR`]'s doc comment
    /// for why this counts calls, not time.
    pub fn tick(&mut self) {
        if self.ticks_left == 0 {
            return;
        }
        self.ticks_left -= 1;
        if self.ticks_left == 0 {
            match self.state {
                State::ReadPending => self.complete_read_fetch(),
                State::WritePending => self.complete_write_prepare(),
                State::WriteCommitting => self.complete_write_commit(),
                State::FlushPending => self.complete_flush(),
                _ => {}
            }
        }
    }

    /// Read a byte of the register file, offset from this board's
    /// configured AUTOCONFIG base. Offsets §4.1 does not define
    /// (`+0x14`, and anything past `INT_STATUS` up to the end of the 64
    /// KB window) read `0` -- unimplemented board space inside MIRAGE's
    /// own window, not the wider bus's open-bus `0xFF` (module docs).
    pub fn read(&mut self, offset: u32) -> u8 {
        match offset {
            o if in_slot(o, reg::UNIT_SELECT) => low_byte(o, reg::UNIT_SELECT, self.unit_select),
            o if in_slot(o, reg::LBA) => byte_of(self.lba, o - reg::LBA),
            o if in_slot(o, reg::COUNT) => byte_of(self.count, o - reg::COUNT),
            o if in_slot(o, reg::CMD_STATUS) => low_byte(o, reg::CMD_STATUS, self.status),
            o if in_slot(o, reg::DATA) => self.data_read(),
            o if in_slot(o, reg::INT_ENABLE) => low_byte(o, reg::INT_ENABLE, self.int_enable),
            o if in_slot(o, reg::INT_STATUS) => low_byte(o, reg::INT_STATUS, self.int_status),
            _ => 0,
        }
    }

    /// Write a byte of the register file. See [`Mirage::read`] for the
    /// offset layout.
    pub fn write(&mut self, offset: u32, value: u8) {
        match offset {
            o if in_slot(o, reg::UNIT_SELECT) && o - reg::UNIT_SELECT == 3 => {
                self.unit_select = value;
            }
            o if in_slot(o, reg::UNIT_SELECT) => {} // the other three lanes are reserved
            o if in_slot(o, reg::LBA) => set_byte_of(&mut self.lba, o - reg::LBA, value),
            o if in_slot(o, reg::COUNT) => set_byte_of(&mut self.count, o - reg::COUNT, value),
            o if in_slot(o, reg::CMD_STATUS) && o - reg::CMD_STATUS == 3 => {
                self.do_command(value);
            }
            o if in_slot(o, reg::CMD_STATUS) => {}
            o if in_slot(o, reg::DATA) => self.data_write(value),
            o if in_slot(o, reg::INT_ENABLE) && o - reg::INT_ENABLE == 3 => {
                self.int_enable = value;
            }
            o if in_slot(o, reg::INT_ENABLE) => {}
            // Write-1-to-clear, not `gayle::reg::GAYLE_INTREQ`'s
            // write-0-to-clear AND convention: a design review of §4.1
            // (which defines neither) called out that W1C is the safer
            // choice on a shared, level-triggered INT2 line where a
            // driver polls, reads the set bits, and must ack exactly
            // those without a race against a new source setting a bit
            // between the read and the write -- an AND-convention ack
            // would need the driver to reconstruct "everything currently
            // 1" instead of just writing back what it read.
            o if in_slot(o, reg::INT_STATUS) && o - reg::INT_STATUS == 3 => {
                self.int_status &= !value;
            }
            o if in_slot(o, reg::INT_STATUS) => {}
            _ => {}
        }
    }

    // ---- commands ------------------------------------------------------

    fn do_command(&mut self, raw_cmd: u8) {
        // A command written while a transfer is already in flight is
        // ignored rather than aborting or corrupting it -- §4.1 doesn't
        // say what should happen here (module docs, gap 7).
        if self.state != State::Idle {
            return;
        }
        let Some(unit) = self.selected_unit() else {
            return self.fail();
        };
        // Unattached units respond as absent, not as a fault -- brief
        // item 4, same posture as `gayle.rs`'s empty-cable case. A real
        // controller can know "nothing in this slot" without ever
        // touching the SD card, so this is resolved immediately rather
        // than through the tick-driven pipeline below.
        if self.units[unit].is_none() {
            // There is no way for a driver to have asked "is unit N
            // there?" first -- §4.1 defines no discovery mechanism at
            // all (module docs, gap 10). All this module can promise is
            // that finding out the hard way, by issuing a command, fails
            // cleanly and observably rather than hanging.
            return self.fail();
        }
        match raw_cmd {
            cmd::READ => self.begin_read(unit),
            cmd::WRITE => self.begin_write(unit),
            cmd::FLUSH => self.begin_flush(unit),
            _ => self.fail(),
        }
    }

    fn begin_read(&mut self, unit: usize) {
        if self.count == 0 {
            return self.fail(); // gap 2: no sensible meaning for "read zero sectors"
        }
        let total = self.units[unit]
            .as_deref()
            .expect("checked above")
            .sector_count();
        if u64::from(self.lba) + u64::from(self.count) > total {
            return self.fail();
        }
        self.active_unit = unit;
        self.current_lba = self.lba;
        self.sectors_left = self.count;
        self.state = State::ReadPending;
        self.status = status::BUSY;
        self.ticks_left = TICKS_PER_SECTOR;
    }

    fn begin_write(&mut self, unit: usize) {
        if self.count == 0 {
            return self.fail();
        }
        let total = self.units[unit]
            .as_deref()
            .expect("checked above")
            .sector_count();
        if u64::from(self.lba) + u64::from(self.count) > total {
            return self.fail();
        }
        self.active_unit = unit;
        self.current_lba = self.lba;
        self.sectors_left = self.count;
        self.buf_pos = 0;
        self.state = State::WritePending;
        self.status = status::BUSY;
        self.ticks_left = TICKS_PER_SECTOR;
    }

    fn begin_flush(&mut self, unit: usize) {
        self.active_unit = unit;
        self.state = State::FlushPending;
        self.status = status::BUSY;
        self.ticks_left = TICKS_PER_SECTOR;
    }

    /// A command failed validation, or a device operation failed once
    /// attempted: report `ERR` and still raise the completion interrupt
    /// -- a rejected or failed command still *completes*, the same
    /// principle `gayle.rs`'s `fail_transfer` documents for an unknown
    /// ATA command.
    fn fail(&mut self) {
        self.state = State::Idle;
        self.status = status::ERR;
        self.sectors_left = 0;
        self.int_status |= int::XFER_COMPLETE;
    }

    // ---- deferred completion --------------------------------------------

    fn complete_read_fetch(&mut self) {
        let Some(dev) = self.units[self.active_unit].as_deref_mut() else {
            return self.fail();
        };
        if dev.read_sector(u64::from(self.current_lba), &mut self.sector_buf) {
            self.buf_pos = 0;
            self.state = State::ReadReady;
            self.status = status::DRQ;
            self.int_status |= int::XFER_COMPLETE;
        } else {
            self.fail();
        }
    }

    fn complete_write_prepare(&mut self) {
        self.buf_pos = 0;
        self.state = State::WriteReady;
        self.status = status::DRQ;
    }

    fn complete_write_commit(&mut self) {
        let Some(dev) = self.units[self.active_unit].as_deref_mut() else {
            return self.fail();
        };
        if dev.write_sector(u64::from(self.current_lba), &self.sector_buf) {
            self.sectors_left -= 1;
            self.int_status |= int::XFER_COMPLETE;
            if self.sectors_left == 0 {
                self.state = State::Idle;
                self.status = 0;
            } else {
                self.current_lba = self.current_lba.wrapping_add(1);
                self.buf_pos = 0;
                self.state = State::WriteReady;
                self.status = status::DRQ;
            }
        } else {
            self.fail();
        }
    }

    fn complete_flush(&mut self) {
        // Nothing underneath actually flushes yet -- module docs, "What's
        // not here". The handshake itself still completes for real, so a
        // driver depending on it existing sees a genuine BUSY-then-done
        // round trip.
        self.state = State::Idle;
        self.status = 0;
        self.int_status |= int::XFER_COMPLETE;
    }

    // ---- the DATA FIFO ---------------------------------------------------

    fn data_read(&mut self) -> u8 {
        if self.state != State::ReadReady {
            // No data phase active: matches `gayle.rs`'s
            // `data_read_hi`/`data_read_lo` returning 0 while `DRQ` is
            // clear, rather than exposing stale FIFO contents.
            return 0;
        }
        let byte = self.sector_buf[self.buf_pos];
        self.buf_pos += 1;
        if self.buf_pos >= SECTOR_BYTES {
            self.sectors_left -= 1;
            if self.sectors_left == 0 {
                self.state = State::Idle;
                self.status = 0;
            } else {
                // The next sector is *not* fetched inline here -- unlike
                // `gayle.rs`'s `finish_read_block`, which fetches
                // synchronously inside this same read. Doing that here
                // would complete a device operation within the very bus
                // access that triggered it, exactly what the module docs'
                // Copperline/OPFS reasoning rules out. So this goes back
                // to `BUSY` and waits for another `tick()`.
                self.current_lba = self.current_lba.wrapping_add(1);
                self.state = State::ReadPending;
                self.status = status::BUSY;
                self.ticks_left = TICKS_PER_SECTOR;
            }
        }
        byte
    }

    fn data_write(&mut self, value: u8) {
        if self.state != State::WriteReady {
            return;
        }
        self.sector_buf[self.buf_pos] = value;
        self.buf_pos += 1;
        if self.buf_pos >= SECTOR_BYTES {
            self.state = State::WriteCommitting;
            self.status = status::BUSY;
            self.ticks_left = TICKS_PER_SECTOR;
        }
    }

    // ---- addressing ------------------------------------------------------

    /// `unit_select`'s value if it names a real unit (§4.1: "0-7"),
    /// `None` otherwise -- guest-hostile input rejected rather than
    /// masked/wrapped into range (`blitter.rs`/`gayle.rs` house style).
    fn selected_unit(&self) -> Option<usize> {
        let u = self.unit_select as usize;
        (u < UNIT_COUNT).then_some(u)
    }
}

impl Default for Mirage<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `offset` falls in the 4-byte-aligned slot starting at `base`.
fn in_slot(offset: u32, base: u32) -> bool {
    (base..base + 4).contains(&offset)
}

/// A single-byte register's value at offset `base + 3` (the low-order
/// byte of the slot -- module docs, gap 1), `0` at the other three
/// offsets in the slot.
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

/// Replace byte `lane` of a big-endian 32-bit register, leaving the
/// other three bytes untouched -- the same partial-register-write shape
/// `MachineBus::write_byte`'s custom-chip arm already uses.
fn set_byte_of(value: &mut u32, lane: u32, byte: u8) {
    let shift = 8 * (3 - lane);
    *value = (*value & !(0xFFu32 << shift)) | ((byte as u32) << shift);
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- an in-memory `BlockDevice`, matching `gayle.rs`'s test double --

    struct MemDisk {
        sectors: std::vec::Vec<[u8; SECTOR_BYTES]>,
        writable: bool,
    }

    impl MemDisk {
        fn new(count: usize) -> Self {
            Self {
                sectors: std::vec![[0u8; SECTOR_BYTES]; count],
                writable: true,
            }
        }

        fn read_only(count: usize) -> Self {
            let mut d = Self::new(count);
            d.writable = false;
            d
        }
    }

    impl BlockDevice for MemDisk {
        fn sector_count(&self) -> u64 {
            self.sectors.len() as u64
        }

        fn read_sector(&mut self, lba: u64, buf: &mut [u8; SECTOR_BYTES]) -> bool {
            match self.sectors.get(lba as usize) {
                Some(s) => {
                    *buf = *s;
                    true
                }
                None => false,
            }
        }

        fn write_sector(&mut self, lba: u64, buf: &[u8; SECTOR_BYTES]) -> bool {
            if !self.writable {
                return false;
            }
            match self.sectors.get_mut(lba as usize) {
                Some(s) => {
                    *s = *buf;
                    true
                }
                None => false,
            }
        }
    }

    // ---- register-level helpers, mirroring how a driver would poke these

    fn write_reg32(m: &mut Mirage, base: u32, value: u32) {
        m.write(base, (value >> 24) as u8);
        m.write(base + 1, (value >> 16) as u8);
        m.write(base + 2, (value >> 8) as u8);
        m.write(base + 3, value as u8);
    }

    fn read_status(m: &mut Mirage) -> u8 {
        m.read(reg::CMD_STATUS + 3)
    }

    fn select_unit(m: &mut Mirage, unit: u8) {
        m.write(reg::UNIT_SELECT + 3, unit);
    }

    fn set_transfer(m: &mut Mirage, lba: u32, count: u32) {
        write_reg32(m, reg::LBA, lba);
        write_reg32(m, reg::COUNT, count);
    }

    fn issue(m: &mut Mirage, cmd: u8) {
        m.write(reg::CMD_STATUS + 3, cmd);
    }

    fn read_data_byte(m: &mut Mirage) -> u8 {
        m.read(reg::DATA)
    }

    fn write_data_byte(m: &mut Mirage, value: u8) {
        m.write(reg::DATA, value);
    }

    /// Drain the FIFO's 512 bytes through four-longword-shaped accesses
    /// (any byte offset in the `DATA` slot pops one FIFO byte -- module
    /// docs), to also exercise "two 16-bit words per longword" simply by
    /// touching all four lanes per iteration.
    fn read_sector_via_data(m: &mut Mirage) -> std::vec::Vec<u8> {
        let mut out = std::vec::Vec::with_capacity(SECTOR_BYTES);
        for lane in 0..SECTOR_BYTES {
            out.push(m.read(reg::DATA + (lane as u32 % 4)));
        }
        out
    }

    // ---- unit selection --------------------------------------------------

    #[test]
    fn absent_unit_fails_immediately_without_touching_any_device() {
        let mut m = Mirage::new();
        let mut disk = MemDisk::new(4);
        m.attach_unit(1, &mut disk); // unit 0 left absent

        select_unit(&mut m, 0);
        set_transfer(&mut m, 0, 1);
        issue(&mut m, cmd::READ);

        assert_eq!(read_status(&mut m) & status::ERR, status::ERR);
        assert_eq!(
            read_status(&mut m) & status::BUSY,
            0,
            "no pending op at all"
        );
    }

    #[test]
    fn out_of_range_unit_number_is_rejected_not_wrapped() {
        let mut m = Mirage::new();
        let mut disk = MemDisk::new(4);
        m.attach_unit(0, &mut disk);

        select_unit(&mut m, 200); // guest-hostile: nowhere near 0-7
        set_transfer(&mut m, 0, 1);
        issue(&mut m, cmd::READ);

        assert_eq!(read_status(&mut m) & status::ERR, status::ERR);
    }

    // ---- BUSY/DRQ/ERR transitions, and the deferred-completion contract --

    #[test]
    fn a_command_never_completes_before_the_first_tick() {
        let mut m = Mirage::new();
        let mut disk = MemDisk::new(4);
        m.attach_unit(0, &mut disk);
        select_unit(&mut m, 0);
        set_transfer(&mut m, 0, 1);

        issue(&mut m, cmd::READ);
        // Still BUSY, not DRQ: the fetch has not happened yet, and a
        // DATA read this early must not hand back anything.
        assert_eq!(read_status(&mut m), status::BUSY);
        assert_eq!(read_data_byte(&mut m), 0, "no data before the first tick");

        m.tick();
        assert_eq!(
            read_status(&mut m),
            status::DRQ,
            "the tick landed the fetch"
        );
    }

    #[test]
    fn zero_count_is_rejected() {
        let mut m = Mirage::new();
        let mut disk = MemDisk::new(4);
        m.attach_unit(0, &mut disk);
        select_unit(&mut m, 0);
        set_transfer(&mut m, 0, 0);
        issue(&mut m, cmd::READ);
        assert_eq!(read_status(&mut m) & status::ERR, status::ERR);
    }

    #[test]
    fn out_of_range_lba_is_rejected_before_any_tick() {
        let mut m = Mirage::new();
        let mut disk = MemDisk::new(4);
        m.attach_unit(0, &mut disk);
        select_unit(&mut m, 0);
        set_transfer(&mut m, 10, 1); // disk only has 4 sectors
        issue(&mut m, cmd::READ);
        assert_eq!(read_status(&mut m) & status::ERR, status::ERR);
    }

    #[test]
    fn write_to_a_read_only_device_fails_at_commit_time() {
        let mut m = Mirage::new();
        let mut disk = MemDisk::read_only(4);
        m.attach_unit(0, &mut disk);
        select_unit(&mut m, 0);
        set_transfer(&mut m, 0, 1);
        issue(&mut m, cmd::WRITE);
        m.tick(); // WritePending -> WriteReady
        assert_eq!(read_status(&mut m), status::DRQ);
        for _ in 0..SECTOR_BYTES {
            write_data_byte(&mut m, 0xAA);
        }
        assert_eq!(read_status(&mut m), status::BUSY, "queued for commit");
        m.tick(); // the commit itself fails
        assert_eq!(read_status(&mut m) & status::ERR, status::ERR);
    }

    // ---- a full sector round-trips through BlockDevice --------------------

    #[test]
    fn single_sector_write_then_read_round_trips_through_the_block_device() {
        let mut m = Mirage::new();
        let mut disk = MemDisk::new(4);
        m.attach_unit(3, &mut disk);
        m.write(reg::INT_ENABLE + 3, int::XFER_COMPLETE);

        select_unit(&mut m, 3);
        set_transfer(&mut m, 2, 1);
        issue(&mut m, cmd::WRITE);
        assert!(!m.irq_pending(), "no interrupt before anything lands");
        m.tick();
        assert_eq!(read_status(&mut m), status::DRQ);
        let pattern: std::vec::Vec<u8> = (0..SECTOR_BYTES as u32).map(|i| i as u8).collect();
        for &b in &pattern {
            write_data_byte(&mut m, b);
        }
        assert_eq!(
            read_status(&mut m),
            status::BUSY,
            "filled sector awaits commit"
        );
        m.tick();
        assert_eq!(read_status(&mut m), 0, "transfer complete, idle");
        assert!(m.irq_pending(), "commit raised the interrupt");
        m.write(reg::INT_STATUS + 3, int::XFER_COMPLETE); // ack: write-1-to-clear

        select_unit(&mut m, 3);
        set_transfer(&mut m, 2, 1);
        issue(&mut m, cmd::READ);
        m.tick();
        assert_eq!(read_status(&mut m), status::DRQ);
        let got = read_sector_via_data(&mut m);
        assert_eq!(got, pattern);
        assert_eq!(read_status(&mut m), 0);
    }

    #[test]
    fn multi_sector_transfer_needs_one_tick_per_sector() {
        let mut m = Mirage::new();
        let mut disk = MemDisk::new(8);
        m.attach_unit(0, &mut disk);
        select_unit(&mut m, 0);
        set_transfer(&mut m, 0, 3);
        issue(&mut m, cmd::WRITE);

        for sector in 0..3u8 {
            m.tick(); // land this sector's WritePending -> WriteReady
            assert_eq!(read_status(&mut m), status::DRQ, "sector {sector}");
            for _ in 0..SECTOR_BYTES {
                write_data_byte(&mut m, sector);
            }
            assert_eq!(
                read_status(&mut m),
                status::BUSY,
                "awaiting commit {sector}"
            );
            m.tick(); // commit
        }
        assert_eq!(read_status(&mut m), 0, "all three sectors committed");

        // Verify what actually landed in the backing store, independent
        // of the register path above.
        let mut check = [0u8; SECTOR_BYTES];
        disk.read_sector(1, &mut check);
        assert_eq!(check[0], 1);
    }

    // ---- interrupts on both the write path and the read path -------------

    #[test]
    fn interrupt_asserts_on_the_write_commit_path() {
        let mut m = Mirage::new();
        let mut disk = MemDisk::new(2);
        m.attach_unit(0, &mut disk);
        m.write(reg::INT_ENABLE + 3, int::XFER_COMPLETE);
        select_unit(&mut m, 0);
        set_transfer(&mut m, 0, 1);
        issue(&mut m, cmd::WRITE);
        m.tick();
        for _ in 0..SECTOR_BYTES {
            write_data_byte(&mut m, 0);
        }
        assert!(
            !m.irq_pending(),
            "not yet -- still waiting on the commit tick"
        );
        m.tick();
        assert!(m.irq_pending(), "commit landed on this tick");
    }

    #[test]
    fn interrupt_asserts_on_the_read_fetch_path() {
        // The read-side analogue of the write test above: the fetch that
        // makes a sector's data available is what raises the interrupt,
        // and it happens on a `tick()`, not inside a DATA access -- see
        // the module docs on why this differs from `gayle.rs`'s
        // read-triggered per-sector refill.
        let mut m = Mirage::new();
        let mut disk = MemDisk::new(2);
        m.attach_unit(0, &mut disk);
        m.write(reg::INT_ENABLE + 3, int::XFER_COMPLETE);
        select_unit(&mut m, 0);
        set_transfer(&mut m, 0, 1);
        issue(&mut m, cmd::READ);
        assert!(!m.irq_pending());
        m.tick();
        assert!(m.irq_pending(), "the fetch landed on this tick");
    }

    // ---- the change counter -----------------------------------------------

    #[test]
    fn media_change_hook_increments_the_counter_and_raises_its_interrupt() {
        let mut m = Mirage::new();
        m.write(reg::INT_ENABLE + 3, int::MEDIA_CHANGE);
        assert_eq!(m.change_count(2), 0);
        m.notify_media_change(2);
        assert_eq!(m.change_count(2), 1);
        assert!(m.irq_pending());
        m.notify_media_change(2);
        assert_eq!(m.change_count(2), 2);
        // Untouched units are unaffected.
        assert_eq!(m.change_count(3), 0);
    }

    // ---- register file shape -----------------------------------------------

    #[test]
    fn unimplemented_offsets_inside_the_window_read_zero_not_open_bus() {
        let mut m = Mirage::new();
        assert_eq!(m.read(0x14), 0);
        assert_eq!(m.read(0x20), 0);
        m.write(0x14, 0xFF); // must not panic or corrupt anything
        assert_eq!(m.read(0x14), 0);
    }

    #[test]
    fn single_byte_registers_only_answer_at_the_low_order_lane() {
        let mut m = Mirage::new();
        m.write(reg::UNIT_SELECT, 5); // not the low-order byte of the slot
        assert_eq!(
            m.read(reg::UNIT_SELECT + 3),
            0,
            "write to the wrong lane is ignored"
        );
        select_unit(&mut m, 5);
        assert_eq!(m.read(reg::UNIT_SELECT + 3), 5);
        assert_eq!(
            m.read(reg::UNIT_SELECT),
            0,
            "the other three lanes stay reserved"
        );
    }

    #[test]
    fn a_command_written_mid_transfer_is_ignored() {
        let mut m = Mirage::new();
        let mut disk = MemDisk::new(4);
        m.attach_unit(0, &mut disk);
        select_unit(&mut m, 0);
        set_transfer(&mut m, 0, 1);
        issue(&mut m, cmd::READ);
        assert_eq!(read_status(&mut m), status::BUSY);
        issue(&mut m, cmd::FLUSH); // must not disturb the pending READ
        assert_eq!(read_status(&mut m), status::BUSY);
        m.tick();
        assert_eq!(
            read_status(&mut m),
            status::DRQ,
            "the original READ still landed"
        );
    }

    #[test]
    fn board_spec_carries_the_documented_placeholder_identity() {
        let spec = Mirage::board_spec();
        assert_eq!(spec.manufacturer, MANUFACTURER);
        assert_eq!(spec.size_bytes, WINDOW_BYTES);
        assert_eq!(spec.board_type & 0xC0, ERT_ZORROII);
    }
}
