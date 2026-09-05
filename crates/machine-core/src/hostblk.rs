//! `hostblk` — the native block storage card (ADR 0003).
//!
//! Implements `docs/adr-0003-native-block-storage-doorbell-not-pio.md`:
//! a doorbell-plus-descriptor Zorro III card whose data path is a
//! host-side memcpy directly into and out of guest RAM, with
//! asynchronous completion over INT2. `~/src/project-ideas/active/
//! copperhf-device-design-note.md` §2 is the closest existing sketch of
//! this mechanism; this module follows its shape (doorbell, host does
//! the whole request, completion queue drained by an interrupt server)
//! but is its own protocol, not a port of it — copperhf's note is a
//! design sketch for a different (also unbuilt) device, not a spec this
//! module implements. See `docs/hostblk-protocol.md` for the register
//! and wire-format contract a driver author needs; this file's docs
//! explain *why* each choice was made, not just what it is.
//!
//! This module is the register interface and transfer engine; a real
//! m68k driver for the wire protocol below and an RDB mounter still don't
//! exist. This card does carry a boot ROM now -- [`DIAG_ROM`], a DiagArea
//! (`libraries/configregs.h`) served from this board's own AUTOCONFIG
//! window at [`ROM_BASE`] -- but its job in this increment is narrower
//! than a driver: prove Kickstart will run code straight from this
//! board's DiagArea and that the code can reach this board's own
//! registers. See `m68k/hostblk-rom/hostblk-diagrom.s` for that ROM's
//! source and full design notes, and `docs/hostblk-protocol.md` section
//! 12 for exactly what still doesn't exist (the driver, the mounter).
//!
//! # Descriptor pointer, not a raw `IORequest` pointer
//!
//! `copperhf`'s sketch has the guest ring the doorbell with the address
//! of its own `IORequest`, and has the host parse `io_Command`/
//! `io_Offset`/`io_Length`/`io_Data` directly out of `dos.library`'s
//! struct layout (with `io_Actual` doubling as the high 32 bits of a
//! 64-bit offset for TD64/NSD — a real hack, called out as such in that
//! note). This module does not do that. Instead the doorbell carries the
//! address of a small, fixed-layout [`Descriptor`] the driver's `BeginIO`
//! builds from the real `IORequest`'s fields, host-defined and stable
//! regardless of `exec.library`/`dos.library` struct-layout history.
//!
//! **What this costs:** the driver does one extra translation step —
//! `IORequest` fields in, [`Descriptor`] bytes out — on every `BeginIO`,
//! instead of the host reading `IORequest` offsets directly. That is a
//! handful of 68k instructions per request, immaterial next to a host
//! file I/O round trip.
//!
//! **What it buys:** a clean 64-bit [`Descriptor::offset`] field with no
//! `io_Actual`-doubles-as-hi-32-bits contortion (this crate's brief:
//! "make the descriptor's offset field 64-bit... a 32-bit-only interface
//! would be a design error"); a host-side parser that never depends on
//! which `exec.library`/`dos.library` version the guest is running, so
//! `machine-core`'s tests can build and validate descriptors without
//! reproducing real `IORequest` offsets at all; and a protocol this
//! crate can unit-test completely before a single line of 68k driver
//! code exists, which is exactly this increment's situation.
//!
//! # Registers vs guest memory: who carries what
//!
//! Three things move between guest and host, and each picked the
//! cheapest carrier for what it is:
//!
//! - **The descriptor** (command, unit, length, offset, buffer address)
//!   travels once, by reference: the guest builds it in its own RAM and
//!   only its address crosses the doorbell. Reading it is the host's job
//!   ([`Hostblk::tick`], not the doorbell write itself — see "Deferred
//!   completion" below).
//! - **The payload** (the actual sector data) moves directly between
//!   guest RAM and the [`BlockDevice`] — see "The transfer engine"
//!   below for the one place this isn't literally zero-copy.
//! - **The result** (error code, residue) never touches guest memory at
//!   all. It rides the completion queue as a `(pointer, error, actual)`
//!   triple the driver reads back through registers. This is the
//!   opposite of `copperhf`'s sketch, which writes `io_Actual`/
//!   `io_Error` back into the guest's own `IORequest`. Doing that here
//!   would mean writing to a guest address this module has not
//!   necessarily validated (a corrupt or adversarial descriptor pointer
//!   must never become a write target — see "Hostile input" below), and
//!   would give a driver two disagreeing sources of truth (registers
//!   and guest memory) for the same result. One source — the completion
//!   queue — is simpler to reason about and to test.
//!
//! # Deferred completion, and why the doorbell write does no I/O
//!
//! Per the ADR and `mirage.rs`'s module docs (the same reasoning
//! applies here verbatim): accepting a doorbell write must not complete
//! the request in the same bus access, because a slow host disk must
//! not stall the machine and a future browser-hosted (OPFS) backend
//! cannot satisfy a read synchronously at all. So [`Hostblk::write`]'s
//! `DOORBELL` arm does nothing but push a raw `u32` pointer onto an
//! in-host submission ring — it does not even read the descriptor out
//! of guest memory yet, since that memory access isn't needed until a
//! request is actually executed. All of the work — reading the
//! descriptor, validating it, moving data, calling the [`BlockDevice`]
//! — happens in [`Hostblk::tick`], called once per
//! [`crate::MachineBus::tick`] the same way [`crate::mirage::Mirage::tick`]
//! is. `tick` executes **at most one** submission per call (see "The
//! transfer engine" below for why doing the whole thing per tick, rather
//! than one sector, is the right grain here) — so with `N` requests
//! queued, the `N`th one completes on the `N`th tick at the earliest,
//! never on the doorbell write that queued it.
//!
//! # The submission ring: depth and overflow
//!
//! [`SUBMIT_QUEUE_CAPACITY`] raw pointers, fixed at compile time (no
//! allocator in this crate). A doorbell write when the ring is already
//! full is **dropped**: the pointer is not queued, nothing is executed
//! for it, and no completion is ever produced for that specific
//! doorbell write. The driver observes this by watching
//! `SUBMIT_OVERFLOW` (a free-running counter, [`reg::SUBMIT_OVERFLOW`])
//! increment without a matching completion showing up, and is
//! responsible for not exceeding the ring's depth in the first place —
//! the same discipline a real hardware TX/submission ring demands, and
//! the same shape virtio's own ring-full behaviour takes. Recovery is
//! entirely on the driver: since it still holds the `IORequest` it
//! never handed off, it can just retry the doorbell write once earlier
//! requests have completed and freed a slot. This module never buffers
//! more requests than [`SUBMIT_QUEUE_CAPACITY`] to work around a driver
//! that overruns it — a fixed, honest limit a driver can be tested
//! against beats a soft one that only fails under load nobody tried in
//! CI.
//!
//! # The completion queue: depth and overflow
//!
//! [`COMPLETION_QUEUE_CAPACITY`] `(pointer, error, actual)` triples.
//! Unlike the submission ring, **nothing is ever dropped here**:
//! [`Hostblk::tick`] checks the completion queue has room *before*
//! popping the next submission, and does nothing at all if it doesn't
//! (this is the entire body of the full-queue case — see `tick`'s first
//! line). A request already dequeued is *never* abandoned once the
//! engine has decided to run it, because the room check happens first;
//! a request not yet dequeued simply waits its turn. So the queue can
//! delay progress but never lose a completion the way an overflowing
//! ring might. The cost is throughput under a stalled driver: if the
//! driver never reads a completion, the whole engine stalls behind it
//! after [`COMPLETION_QUEUE_CAPACITY`] requests land unread, rather than
//! silently discarding the excess. That is a deliberate trade: a driver
//! bug that stops draining completions becomes a visible stall (no
//! forward progress, easy to notice and to reproduce in a test) instead
//! of a silently lost `IORequest` that never gets `ReplyMsg`'d and
//! leaves a task blocked forever waiting on it. A hung pipeline is a bug
//! that shows up; a leaked completion is a bug that doesn't.
//!
//! `COMPLETION_PTR` reads `0` when the queue is empty (`0` is never a
//! legitimate descriptor address in practice — no `AllocMem` on
//! AmigaOS returns address `0`, and it doubles as this device's own
//! `NULL`-shaped "nothing here" the way the OS already treats it
//! elsewhere), so a driver's interrupt server can poll
//! `COMPLETION_PTR != 0` as a cheap "is there anything to drain" check
//! without needing `COMPLETION_COUNT` at all; the count register exists
//! purely for convenience/diagnostics.
//!
//! # The transfer engine: whole descriptor per tick, not one sector
//!
//! `mirage.rs` advances its state machine one sector per tick, because
//! its `DATA` register is a real per-byte FIFO a driver drains by hand
//! and the state machine has to track a byte cursor either way. There is
//! no such register here — the whole point of this card (ADR 0003) is
//! that the guest never touches a data port — so there is nothing
//! sector-granular for the state machine to expose, and spreading one
//! descriptor's transfer across many ticks would only add bookkeeping
//! (a partial-transfer cursor, a way to resume it) for no guest-visible
//! benefit: the guest cannot observe *how many* ticks a transfer took,
//! only that it wasn't zero. So [`Hostblk::tick`] performs one whole
//! descriptor's transfer — however many sectors long — in a single
//! call, and only ever pops one submission per tick (so multiple
//! outstanding requests still complete one tick apart, preserving the
//! "never in the same access" guarantee per request without needing
//! per-sector state at all).
//!
//! **Not literally zero-copy at the byte level.** [`BlockDevice::
//! read_sector`]/[`BlockDevice::write_sector`] take a `&mut [u8;
//! SECTOR_BYTES]`/`&[u8; SECTOR_BYTES]` — a fixed-size array, not an
//! arbitrary slice — because reusing that trait unchanged (so
//! `machine-hosted`'s `FileBlockDevice` works with no modification,
//! per this crate's brief) means living within its existing signature.
//! A guest RAM slice at an arbitrary byte offset cannot be reborrowed as
//! `&mut [u8; 512]` in safe Rust, so each sector passes through one
//! 512-byte stack buffer between the [`BlockDevice`] call and the `ram`
//! slice. This is "direct to guest memory" in the sense that actually
//! matters for ADR 0003 — no CPU-visible register, no per-byte guest
//! bus access, the guest's own code never moves a byte of payload — not
//! in the sense of a single memcpy with no intermediate buffer anywhere
//! in the host's own call stack.
//!
//! # Hostile input
//!
//! Every value in a [`Descriptor`] and the pointer to it are guest-
//! supplied and treated as hostile, following `blitter.rs`/`mirage.rs`
//! house style — reject cleanly, never panic, never read or write
//! outside `ram`:
//!
//! - **Descriptor pointer**: must be 4-byte aligned and
//!   `[ptr, ptr + DESC_LEN)` must fit inside `ram`, checked with
//!   [`u32::checked_add`] so a pointer near `u32::MAX` cannot wrap into
//!   looking valid. Failing either yields [`err::BAD_ADDRESS`] with the
//!   descriptor never read at all.
//! - **Unit number**: `desc.unit as usize` must be `< UNIT_COUNT` *and*
//!   attached, or [`err::BAD_UNIT`] — an out-of-range or absent unit is
//!   rejected the same way, since the driver has no way to distinguish
//!   them without a discovery step anyway.
//! - **Command**: anything other than [`cmd::READ`]/[`cmd::WRITE`]/
//!   [`cmd::FLUSH`] is [`err::INVALID_COMMAND`].
//! - **Length**: `0`, or not a multiple of `SECTOR_BYTES`, is
//!   [`err::INVALID_LENGTH`] — this card is sector-granular only, the
//!   same posture `mirage.rs` takes for its `COUNT` register.
//! - **Offset alignment**: not a multiple of `SECTOR_BYTES` is
//!   [`err::MISALIGNED`].
//! - **Device bounds**: `offset/SECTOR_BYTES + length/SECTOR_BYTES` must
//!   not exceed the unit's `sector_count()` (checked via
//!   [`u64::checked_add`]), or [`err::OUT_OF_RANGE`] — this is this
//!   module's "over-long transfer" case.
//! - **Buffer bounds**: `[buffer, buffer + length)` must fit inside
//!   `ram` (again via [`u32::checked_add`]), or [`err::BAD_ADDRESS`] —
//!   "a descriptor pointing outside guest RAM". Checked *before* any
//!   host I/O runs, so a bad buffer address never causes a partial
//!   device read/write that then can't be delivered.
//! - **Write protection**: a `WRITE` against a write-protected unit is
//!   [`err::WRITE_PROTECTED`] without the [`BlockDevice`] being touched.
//! - **Device I/O failure mid-transfer**: [`err::IO_ERROR`], with
//!   `actual` reporting exactly how many whole sectors landed before the
//!   failure — the residue a driver needs to set `io_Actual` honestly
//!   rather than guessing (the brief's explicit callout of what the
//!   MIRAGE review found missing).
//!
//! None of this can be observed as a hang: every rejection above still
//! produces a completion on the next `tick()` (or the current one, in
//! the case of a rejection that needs no [`BlockDevice`] call at all),
//! carrying an error code and `actual = 0` (except the partial-I/O-
//! failure case above), exactly like a successful transfer.
//!
//! # Per-unit discovery
//!
//! Unlike `mirage.rs` §4.1 gap 10 (no way to ask which units exist, how
//! big, or read-only), this is this project's own interface, so
//! discovery is built in from the start: [`reg::UNIT_SELECT`] picks a
//! unit, and [`reg::UNIT_PRESENT`], [`reg::UNIT_WRITE_PROTECT`],
//! [`reg::UNIT_CHANGE_COUNT`], and the [`reg::UNIT_SECTORS_HI`]/
//! [`reg::UNIT_SECTORS_LO`] pair (64-bit sector count, split big-endian
//! across two registers for the same reason [`Descriptor::offset`] is
//! 64-bit) answer synchronously — no doorbell round trip, since querying
//! host-side struct fields needs no [`BlockDevice`] I/O and no
//! asynchronous boundary. A boot ROM's RDB mounter can walk every unit
//! this way before issuing a single `READ`.
//!
//! # Write completion is not durability
//!
//! Same posture as `mirage.rs`: [`cmd::FLUSH`] is a real doorbell-to-
//! completion round trip (so a driver depending on the handshake
//! existing sees one), but [`BlockDevice`] has no flush method to call
//! underneath it, so there is nothing for it to actually order or
//! durably commit yet. A host-side write cache would need a real flush
//! added to the trait; that's `mirage.rs`'s "future work, not invented
//! speculatively here" note, unchanged by this module's existence.
//!
//! # Removable media
//!
//! As with `mirage.rs`: attach/detach and hot media-change are host-side
//! operations, not guest-visible commands, in this increment.
//! [`Hostblk::notify_media_change`] is the hook a future board layer (or
//! a test) calls in place of a guest-triggered eject; it bumps the
//! selected unit's change counter, which [`reg::UNIT_CHANGE_COUNT`]
//! exposes for a driver's `TD_CHANGENUM`-style polling.

use crate::autoconfig::{BoardSpec, ERTF_DIAGVALID, ERT_ZORROIII};
use crate::gayle::{BlockDevice, SECTOR_BYTES};

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

/// This card's product number under [`MANUFACTURER`] — distinct from
/// `mirage::PRODUCT` (`0`) so the two boards remain distinguishable by
/// identity alone if both are ever attached to the same machine (the
/// device ledger keeps MIRAGE around as its own reference
/// implementation, off the boot path, so this is a real possibility).
pub const PRODUCT: u8 = 1;

/// The Zorro III AUTOCONFIG window: 16 MB, "extended-table code 0" —
/// see `graffity.rs`'s module docs for the full explanation of why a
/// 16 MB Zorro III board's `er_Type` carries no size bits of its own,
/// and its `ERFF_ZORRO_III`/`ERFF_EXTENDED` flag pair, which this board
/// reuses verbatim (the same rule applies to any Zorro III board, not
/// just Graffity). The register file above uses only its first `0x38`
/// bytes; everything else in the window is unimplemented board space
/// (`Hostblk::read`'s `_` arm), the same posture `mirage.rs` takes for
/// the unused tail of its own (much smaller) window.
pub const WINDOW_BYTES: u32 = 0x0100_0000;

/// `er_Flags` bit 4: this is a genuine Zorro III board (`graffity.rs`
/// module docs).
const ERFF_ZORRO_III: u8 = 1 << 4;
/// `er_Flags` bit 5: `er_Type`'s size bits index the 16 MB-1 GB extended
/// table rather than Zorro II's 64 KB-8 MB one (`graffity.rs` module
/// docs).
const ERFF_EXTENDED: u8 = 1 << 5;

/// This board's DiagArea boot ROM, assembled from `m68k/hostblk-rom/
/// hostblk-diagrom.s` and vendored at `assets/hostblk-rom/
/// hostblk-diagrom.bin` (`assets/hostblk-rom/PROVENANCE.md`) the same way
/// `assets/aros/` vendors AROS -- `machine-core` has no allocator and no
/// filesystem, and must build with no m68k toolchain present
/// (`scripts/build-hostblk-rom.sh` is a separate, manually-run step, not
/// part of `cargo build`).
///
/// **Scope of this increment:** this ROM proves Kickstart will run code
/// from this board's own DiagArea and that the code can reach this
/// board's own registers (`DiagEntry` in the source file reads
/// [`reg::VERSION`]) -- it does not implement the `hostblk` wire protocol
/// or an RDB mounter. See the source file's header and
/// `docs/hostblk-protocol.md` section 12 for what is and isn't built yet.
///
/// `pub` (not just crate-internal) so a host-side introspection tool can
/// search guest memory for a byte-identical copy of it without hardcoding
/// a duplicate of these bytes itself (`machine-hosted`'s
/// `find_diag_rom_copy_by_signature`) -- one source of truth for what the
/// ROM's own bytes are.
pub const DIAG_ROM: &[u8] = include_bytes!("../../../assets/hostblk-rom/hostblk-diagrom.bin");

/// Where [`DIAG_ROM`] is mapped within this board's own AUTOCONFIG window,
/// once configured -- the value [`BoardSpec::init_diag_vec`] advertises
/// (`libraries/configregs.h`: "This offset is added to the base address
/// of the configured board; the resulting address points to the start of
/// this board's DiagArea", NDK 3.2). Chosen well clear of the register
/// file above (which ends at [`reg::VERSION`], `0x3C`), with room to
/// spare before it for the register file to grow. Kept in sync with
/// `scripts/build-hostblk-rom.sh`'s own sanity check by literal value
/// (that script greps for this exact line) rather than a shared constant,
/// since the two live in different languages with no build-time link
/// between them.
pub const ROM_BASE: u32 = 0x1000;

/// Byte offset, from the start of the DiagArea's RAM copy (the address
/// `expansion.library` hands `DiagEntry` in `A2`, and the same address it
/// stashes in the board's `ConfigDev.cd_Rom.er_Reserved0c..0f`, big-endian
/// -- RKRM 3rd ed. "Expansion Library", "Events At DIAG Time"), of the
/// scratch cell `DiagEntry` writes [`reg::VERSION`]'s value into
/// (`hostblk-diagrom.s`'s `DiagMarker`). Fixed by `struct DiagArea`'s own
/// documented layout (`libraries/configregs.h`): `da_Config`(1) +
/// `da_Flags`(1) + `da_Size`(2) + `da_DiagPoint`(2) + `da_BootPoint`(2) +
/// `da_Name`(2) + `da_Reserved01`(2) + `da_Reserved02`(2) = 14 bytes,
/// which is exactly where `DiagMarker` sits in the source file --
/// cross-checked against the assembled [`DIAG_ROM`] by this file's own
/// `diag_rom_header_matches_the_documented_diagarea_layout` test.
pub const DIAG_MARKER_OFFSET: u32 = 14;

/// Units this card can address. Matches `mirage::UNIT_COUNT`; there is
/// no protocol reason the two must agree, they simply both picked "one
/// byte's worth, comfortably more than this machine will ever attach"
/// (`Descriptor::unit` and `reg::UNIT_SELECT` are both a full byte, so
/// nothing here forces 8 either — it is just a reasonable first bound).
pub const UNIT_COUNT: usize = 8;

/// Outstanding doorbell writes this card holds before rejecting more
/// (module docs, "The submission ring: depth and overflow").
pub const SUBMIT_QUEUE_CAPACITY: usize = 8;

/// Completed requests this card holds before the driver drains one
/// (module docs, "The completion queue: depth and overflow").
pub const COMPLETION_QUEUE_CAPACITY: usize = 8;

/// Byte length of a [`Descriptor`] as it lies in guest RAM: `command`
/// (1), `unit` (1), reserved (2), `length` (4), `offset` (8), `buffer`
/// (4) = 20, all big-endian, no padding relied upon (this module reads
/// it byte by byte, never transmutes).
const DESC_LEN: u32 = 20;

/// Register offsets within this card's AUTOCONFIG window. Every
/// single-byte value lives in the low-order byte of its 4-byte-aligned
/// slot, the other three lanes reserved — the same "one hot byte per
/// slot" convention `mirage.rs` documents at length (gap 1 in its module
/// docs) and adopts here without re-litigating: a `move.l`/`move.b`
/// compiler emission naturally lands a small constant in the low byte
/// of a longword-sized slot.
pub mod reg {
    /// Select which unit the discovery registers below describe. Write
    /// only; does not affect an in-flight or queued transfer, which
    /// carries its own unit number in the [`super::Descriptor`].
    pub const UNIT_SELECT: u32 = 0x00;
    /// `1` if the selected unit is attached, `0` otherwise. Read-only.
    pub const UNIT_PRESENT: u32 = 0x04;
    /// `1` if the selected unit is write-protected. Read-only;
    /// meaningless (reads `0`) when the unit is absent.
    pub const UNIT_WRITE_PROTECT: u32 = 0x08;
    /// The selected unit's media-change counter. Read-only; `0` for an
    /// absent unit and never bumped by anything guest-visible in this
    /// increment (module docs, "Removable media").
    pub const UNIT_CHANGE_COUNT: u32 = 0x0C;
    /// High 32 bits of the selected unit's `sector_count()`. Read-only.
    pub const UNIT_SECTORS_HI: u32 = 0x10;
    /// Low 32 bits of the selected unit's `sector_count()`. Read-only.
    pub const UNIT_SECTORS_LO: u32 = 0x14;
    /// Write the address of a [`super::Descriptor`] here to submit it.
    /// Write-only (reads `0`); see the module docs' "Deferred
    /// completion" section for why this never does I/O inline.
    pub const DOORBELL: u32 = 0x18;
    /// Free-running count of doorbell writes dropped because the
    /// submission ring was full (module docs). Read-only, never resets.
    pub const SUBMIT_OVERFLOW: u32 = 0x1C;
    /// The address supplied at [`DOORBELL`] for the completion queue's
    /// head entry, or `0` if the queue is empty. Read-only.
    pub const COMPLETION_PTR: u32 = 0x20;
    /// The head entry's error code (see [`super::err`]). Read-only;
    /// meaningless while [`COMPLETION_PTR`] reads `0`.
    pub const COMPLETION_ERROR: u32 = 0x24;
    /// The head entry's residue/actual byte count. Read-only.
    pub const COMPLETION_ACTUAL: u32 = 0x28;
    /// Any write pops the completion queue's head entry, revealing the
    /// next one (or emptying it). Write-only.
    pub const COMPLETION_ADVANCE: u32 = 0x2C;
    /// How many completions are currently queued. Read-only; purely a
    /// convenience -- polling `COMPLETION_PTR != 0` is sufficient
    /// (module docs).
    pub const COMPLETION_COUNT: u32 = 0x30;
    /// `1` enables raising INT2 while the completion queue is
    /// non-empty; `0` (the reset value) leaves it masked, the same
    /// enable-gated shape `gayle.rs`/`mirage.rs` both use.
    pub const INT_ENABLE: u32 = 0x34;
    /// Depth of the submission ring, in descriptors. Read-only.
    ///
    /// The driver must not exceed this, because a doorbell write past it
    /// is dropped and produces no completion (module docs). Self-limiting
    /// is therefore the *only* way to use this card safely -- so the
    /// depth has to be discoverable rather than a number the driver
    /// hardcodes. A bare-metal board with memory to spare may well want a
    /// deeper ring than this host's, and a driver that baked in today's
    /// value would silently under-use it at best, and mis-limit against a
    /// shallower one at worst.
    pub const SUBMIT_CAPACITY: u32 = 0x38;
    /// Protocol version, `1` for the interface `docs/hostblk-protocol.md`
    /// describes. Read-only.
    ///
    /// This exists so the *first* incompatible change is survivable: a
    /// driver can refuse a version it does not understand instead of
    /// misreading a reshuffled register file as valid data. No driver
    /// exists yet, so it costs one register now and cannot be added
    /// afterwards without the very version check it provides.
    pub const VERSION: u32 = 0x3C;
}

/// The value [`reg::VERSION`] reports (module docs).
pub const PROTOCOL_VERSION: u32 = 1;

/// [`Descriptor::command`] values.
pub mod cmd {
    pub const READ: u8 = 1;
    pub const WRITE: u8 = 2;
    /// A no-op handshake -- see the module docs' "Write completion is
    /// not durability" section.
    pub const FLUSH: u8 = 3;
}

/// Completion error codes -- what a driver reads back from
/// [`reg::COMPLETION_ERROR`] and maps onto `io_Error`. `0`
/// ([`OK`]) is success; everything else is a specific, named failure a
/// driver can act on rather than guess about (the brief's explicit
/// callout of what the MIRAGE review found missing).
pub mod err {
    /// Completed with no error.
    pub const OK: u8 = 0;
    /// The unit named in the descriptor is out of range or not
    /// attached.
    pub const BAD_UNIT: u8 = 1;
    /// A `WRITE` against a write-protected unit.
    pub const WRITE_PROTECTED: u8 = 2;
    /// `Descriptor::command` is none of [`cmd::READ`]/[`cmd::WRITE`]/
    /// [`cmd::FLUSH`].
    pub const INVALID_COMMAND: u8 = 3;
    /// `Descriptor::length` is `0`, or not a multiple of `SECTOR_BYTES`.
    pub const INVALID_LENGTH: u8 = 4;
    /// `Descriptor::offset` is not a multiple of `SECTOR_BYTES`.
    pub const MISALIGNED: u8 = 5;
    /// The transfer would run past the unit's `sector_count()` --
    /// this module's "over-long transfer" case.
    pub const OUT_OF_RANGE: u8 = 6;
    /// The descriptor pointer, or the buffer it names, does not lie
    /// entirely inside guest RAM (or the descriptor pointer is
    /// misaligned).
    pub const BAD_ADDRESS: u8 = 7;
    /// The `BlockDevice` reported a failure partway through the
    /// transfer; `actual` in the completion entry reports how many
    /// whole sectors landed first.
    pub const IO_ERROR: u8 = 8;
}

/// The fixed-layout structure a doorbell write names by address (module
/// docs, "Descriptor pointer, not a raw `IORequest` pointer"). Parsed
/// out of guest RAM by [`Hostblk::read_descriptor`]; never constructed
/// from raw memory via a transmute, only by explicit big-endian field
/// reads, so struct layout/padding on the host is irrelevant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Descriptor {
    command: u8,
    unit: u8,
    /// Transfer length in bytes. Must be a nonzero multiple of
    /// `SECTOR_BYTES`; ignored for [`cmd::FLUSH`].
    length: u32,
    /// Byte offset into the unit's address space, 64-bit per the
    /// brief's "make the descriptor's offset field 64-bit... a
    /// 32-bit-only interface would be a design error" -- PFS3-DS and
    /// SFS address beyond 4 GB, and this interface must not box the
    /// eventual driver in the way `copperhf`'s io_Actual-doubling hack
    /// does. Must be a multiple of `SECTOR_BYTES`; ignored for
    /// [`cmd::FLUSH`].
    offset: u64,
    /// Guest RAM address of the transfer buffer. Ignored for
    /// [`cmd::FLUSH`].
    buffer: u32,
}

/// One drained-or-pending completion: the pointer the driver submitted,
/// the result of executing it, and the residue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CompletionEntry {
    ptr: u32,
    error: u8,
    actual: u32,
}

/// A fixed-capacity FIFO ring, `#![no_std]`/no-alloc: `machine-core` has
/// no allocator, so both the submission and completion queues are plain
/// arrays with a head/length cursor rather than `VecDeque` (which needs
/// `alloc`).
struct Ring<T, const N: usize> {
    buf: [T; N],
    head: usize,
    len: usize,
}

impl<T: Copy + Default, const N: usize> Ring<T, N> {
    fn new() -> Self {
        Self {
            buf: [T::default(); N],
            head: 0,
            len: 0,
        }
    }

    /// `false` if the ring is already full -- the caller decides what
    /// that means (drop-and-count for the submission ring, "don't even
    /// try yet" for the completion ring; see the module docs).
    fn push(&mut self, value: T) -> bool {
        if self.len == N {
            return false;
        }
        let idx = (self.head + self.len) % N;
        self.buf[idx] = value;
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let value = self.buf[self.head];
        self.head = (self.head + 1) % N;
        self.len -= 1;
        Some(value)
    }

    fn peek(&self) -> Option<&T> {
        if self.len == 0 {
            None
        } else {
            Some(&self.buf[self.head])
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn is_full(&self) -> bool {
        self.len == N
    }
}

/// A single attached unit: the borrowed backing store plus the
/// per-unit state the brief asks for (attached is implicit in `Some`;
/// size comes from `device.sector_count()`; write-protect and change
/// counter are kept here).
struct Unit<'a> {
    device: &'a mut dyn BlockDevice,
    write_protect: bool,
    change_count: u32,
}

/// `hostblk`'s register file and transfer engine. See the module docs
/// for the protocol this implements and the reasoning behind it.
pub struct Hostblk<'a> {
    units: [Option<Unit<'a>>; UNIT_COUNT],
    unit_select: u8,

    /// Accumulates the four bytes of a `DOORBELL` write; submitted once
    /// the low-order lane lands (module docs' "one hot byte per slot"
    /// convention still applies to which lane triggers, even though
    /// every lane of this particular register carries real data).
    doorbell_acc: u32,
    submit_queue: Ring<u32, SUBMIT_QUEUE_CAPACITY>,
    submit_overflow: u32,

    completion_queue: Ring<CompletionEntry, COMPLETION_QUEUE_CAPACITY>,
    int_enable: u8,
}

impl<'a> Hostblk<'a> {
    pub fn new() -> Self {
        Self {
            units: [None, None, None, None, None, None, None, None],
            unit_select: 0,
            doorbell_acc: 0,
            submit_queue: Ring::new(),
            submit_overflow: 0,
            completion_queue: Ring::new(),
            int_enable: 0,
        }
    }

    /// The `BoardSpec` this card registers on the AUTOCONFIG chain: one
    /// Zorro III board, per the brief's item 1 and ADR 0003.
    pub fn board_spec() -> BoardSpec {
        BoardSpec {
            board_type: ERT_ZORROIII | ERTF_DIAGVALID, // extended-table code 0 == 16 MB, plus a DiagArea
            product: PRODUCT,
            flags: ERFF_ZORRO_III | ERFF_EXTENDED,
            manufacturer: MANUFACTURER,
            serial: 0,
            init_diag_vec: ROM_BASE as u16, // fits: ROM_BASE (0x1000) << u16::MAX
            size_bytes: WINDOW_BYTES,
        }
    }

    /// Attach a disk to unit `unit`, optionally write-protected.
    /// Out-of-range units are silently ignored -- host-side/board-layer
    /// call, not guest-controlled, same posture as
    /// `mirage::Mirage::attach_unit`.
    pub fn attach_unit(&mut self, unit: u8, device: &'a mut dyn BlockDevice, write_protect: bool) {
        if let Some(slot) = self.units.get_mut(unit as usize) {
            *slot = Some(Unit {
                device,
                write_protect,
                change_count: 0,
            });
        }
    }

    /// Host-side hook standing in for a guest-triggered eject (module
    /// docs, "Removable media"). Bumps `unit`'s change counter;
    /// out-of-range or absent units do nothing.
    pub fn notify_media_change(&mut self, unit: u8) {
        if let Some(Some(u)) = self.units.get_mut(unit as usize) {
            u.change_count = u.change_count.wrapping_add(1);
        }
    }

    /// Whether `hostblk` is asserting its interrupt, which the bus
    /// routes to INT2 (`PORTS`) the same as Gayle, MIRAGE and Graffity.
    pub fn irq_pending(&self) -> bool {
        self.int_enable != 0 && self.completion_queue.len() > 0
    }

    /// Advance the engine by one step: execute at most one queued
    /// request against `ram`, direct to/from the guest's own memory
    /// (module docs, "The transfer engine"). Called once per
    /// [`crate::MachineBus::tick`], mirroring `mirage::Mirage::tick`.
    pub fn tick(&mut self, mem: &mut dyn crate::GuestMemory) {
        // Back-pressure, not data loss (module docs, "The completion
        // queue"): don't even dequeue a submission unless there is
        // guaranteed room for the completion it will produce.
        if self.completion_queue.is_full() {
            return;
        }
        let Some(ptr) = self.submit_queue.pop() else {
            return;
        };
        let (error, actual) = self.execute(ptr, mem);
        // Room was checked above and nothing has re-entered since, so
        // this cannot fail -- `debug_assert!`, not a silent drop, if
        // that invariant is ever broken by a future edit.
        let pushed = self
            .completion_queue
            .push(CompletionEntry { ptr, error, actual });
        debug_assert!(pushed, "completion room was checked before executing");
    }

    fn execute(&mut self, ptr: u32, mem: &mut dyn crate::GuestMemory) -> (u8, u32) {
        let Some(desc) = Self::read_descriptor(ptr, mem) else {
            return (err::BAD_ADDRESS, 0);
        };
        let Some(unit) = self.unit_index(desc.unit) else {
            return (err::BAD_UNIT, 0);
        };
        match desc.command {
            cmd::READ => self.transfer(unit, &desc, mem, false),
            cmd::WRITE => self.transfer(unit, &desc, mem, true),
            cmd::FLUSH => (err::OK, 0), // module docs: no durability primitive yet
            _ => (err::INVALID_COMMAND, 0),
        }
    }

    /// `desc.unit`'s slot if it names an attached unit, `None`
    /// otherwise -- an absent unit and an out-of-range one are
    /// indistinguishable to the guest without a discovery step, so both
    /// map to the same [`err::BAD_UNIT`] (module docs, "Hostile input").
    fn unit_index(&self, unit: u8) -> Option<usize> {
        let u = unit as usize;
        if u < UNIT_COUNT && self.units[u].is_some() {
            Some(u)
        } else {
            None
        }
    }

    /// Read and validate a [`Descriptor`] out of `ram` at `ptr`.
    /// `None` for a misaligned or out-of-bounds pointer (module docs,
    /// "Hostile input") -- the descriptor's own fields are read
    /// unconditionally, since any bit pattern in `command`/`unit` is
    /// still a well-defined (if possibly-rejected) value, unlike the
    /// pointer itself which can be flatly unreachable.
    fn read_descriptor(ptr: u32, mem: &dyn crate::GuestMemory) -> Option<Descriptor> {
        if !ptr.is_multiple_of(4) {
            return None;
        }
        // Reachability is the bus's question, not this card's: it asks
        // for a validated view rather than comparing against a range of
        // its own. Until fast RAM existed this compared against
        // `CHIP_RAM_SIZE`, which was true only while chip RAM was the
        // only RAM -- see `device-ledger.md`, "The rule for addresses".
        let d = mem.ram_slice(ptr, DESC_LEN)?;
        let command = d[0];
        let unit = d[1];
        // d[2..4]: reserved, ignored.
        let length = u32::from_be_bytes(d[4..8].try_into().unwrap());
        let offset = u64::from_be_bytes(d[8..16].try_into().unwrap());
        let buffer = u32::from_be_bytes(d[16..20].try_into().unwrap());
        Some(Descriptor {
            command,
            unit,
            length,
            offset,
            buffer,
        })
    }

    /// Run a `READ`/`WRITE` (`is_write` selects which) to completion,
    /// validating every guest-supplied field first (module docs,
    /// "Hostile input") and returning `(error, actual_bytes)`.
    fn transfer(
        &mut self,
        unit: usize,
        desc: &Descriptor,
        mem: &mut dyn crate::GuestMemory,
        is_write: bool,
    ) -> (u8, u32) {
        // Safe to index/unwrap: `unit` came from `unit_index`, which
        // only ever returns an index whose slot is `Some`.
        if is_write && self.units[unit].as_ref().unwrap().write_protect {
            return (err::WRITE_PROTECTED, 0);
        }
        if desc.length == 0 || !desc.length.is_multiple_of(SECTOR_BYTES as u32) {
            return (err::INVALID_LENGTH, 0);
        }
        if !desc.offset.is_multiple_of(SECTOR_BYTES as u64) {
            return (err::MISALIGNED, 0);
        }
        let sector_count = desc.length / SECTOR_BYTES as u32;
        let start_lba = desc.offset / SECTOR_BYTES as u64;
        let Some(end_lba) = start_lba.checked_add(u64::from(sector_count)) else {
            return (err::OUT_OF_RANGE, 0);
        };
        let total_sectors = self.units[unit].as_ref().unwrap().device.sector_count();
        if end_lba > total_sectors {
            return (err::OUT_OF_RANGE, 0);
        }
        // One validated view of the whole buffer, taken up front: the
        // bus decides reachability (chip RAM, fast RAM, or whatever a
        // future board adds), and a span straddling two regions is
        // refused rather than silently stitched together.
        let Some(buf) = mem.ram_slice_mut(desc.buffer, desc.length) else {
            return (err::BAD_ADDRESS, 0);
        };

        let mut sector_buf = [0u8; SECTOR_BYTES];
        let mut actual = 0u32;
        for i in 0..sector_count {
            let lba = start_lba + u64::from(i);
            let off = i as usize * SECTOR_BYTES;
            let dev = &mut *self.units[unit].as_mut().unwrap().device;
            if is_write {
                sector_buf.copy_from_slice(&buf[off..off + SECTOR_BYTES]);
                if !dev.write_sector(lba, &sector_buf) {
                    return (err::IO_ERROR, actual);
                }
            } else {
                if !dev.read_sector(lba, &mut sector_buf) {
                    return (err::IO_ERROR, actual);
                }
                buf[off..off + SECTOR_BYTES].copy_from_slice(&sector_buf);
            }
            actual += SECTOR_BYTES as u32;
        }
        (err::OK, actual)
    }

    /// `unit_select`'s value if it names a real unit, `None` otherwise
    /// -- guest-hostile input rejected rather than masked/wrapped into
    /// range (`blitter.rs`/`mirage.rs` house style).
    fn selected_unit(&self) -> Option<&Unit<'a>> {
        let u = self.unit_select as usize;
        if u < UNIT_COUNT {
            self.units[u].as_ref()
        } else {
            None
        }
    }

    /// Read a byte of the register file, offset from this board's
    /// configured AUTOCONFIG base. Anything not named in [`reg`] --
    /// including `DOORBELL` and `COMPLETION_ADVANCE`, both write-only --
    /// reads `0`: unimplemented board space inside this card's own
    /// window, not the wider bus's open-bus `0xFF` (`mirage.rs`'s same
    /// convention).
    pub fn read(&self, offset: u32) -> u8 {
        match offset {
            o if in_slot(o, reg::UNIT_SELECT) => low_byte(o, reg::UNIT_SELECT, self.unit_select),
            o if in_slot(o, reg::UNIT_PRESENT) => {
                low_byte(o, reg::UNIT_PRESENT, self.selected_unit().is_some() as u8)
            }
            o if in_slot(o, reg::UNIT_WRITE_PROTECT) => low_byte(
                o,
                reg::UNIT_WRITE_PROTECT,
                self.selected_unit().is_some_and(|u| u.write_protect) as u8,
            ),
            o if in_slot(o, reg::UNIT_CHANGE_COUNT) => byte_of(
                self.selected_unit().map(|u| u.change_count).unwrap_or(0),
                o - reg::UNIT_CHANGE_COUNT,
            ),
            o if in_slot(o, reg::UNIT_SECTORS_HI) => {
                let sectors = self
                    .selected_unit()
                    .map(|u| u.device.sector_count())
                    .unwrap_or(0);
                byte_of((sectors >> 32) as u32, o - reg::UNIT_SECTORS_HI)
            }
            o if in_slot(o, reg::UNIT_SECTORS_LO) => {
                let sectors = self
                    .selected_unit()
                    .map(|u| u.device.sector_count())
                    .unwrap_or(0);
                byte_of(sectors as u32, o - reg::UNIT_SECTORS_LO)
            }
            o if in_slot(o, reg::SUBMIT_OVERFLOW) => {
                byte_of(self.submit_overflow, o - reg::SUBMIT_OVERFLOW)
            }
            o if in_slot(o, reg::COMPLETION_PTR) => byte_of(
                self.completion_queue.peek().map(|e| e.ptr).unwrap_or(0),
                o - reg::COMPLETION_PTR,
            ),
            o if in_slot(o, reg::COMPLETION_ERROR) => low_byte(
                o,
                reg::COMPLETION_ERROR,
                self.completion_queue.peek().map(|e| e.error).unwrap_or(0),
            ),
            o if in_slot(o, reg::COMPLETION_ACTUAL) => byte_of(
                self.completion_queue.peek().map(|e| e.actual).unwrap_or(0),
                o - reg::COMPLETION_ACTUAL,
            ),
            o if in_slot(o, reg::COMPLETION_COUNT) => byte_of(
                self.completion_queue.len() as u32,
                o - reg::COMPLETION_COUNT,
            ),
            o if in_slot(o, reg::INT_ENABLE) => low_byte(o, reg::INT_ENABLE, self.int_enable),
            o if in_slot(o, reg::SUBMIT_CAPACITY) => {
                byte_of(SUBMIT_QUEUE_CAPACITY as u32, o - reg::SUBMIT_CAPACITY)
            }
            o if in_slot(o, reg::VERSION) => byte_of(PROTOCOL_VERSION, o - reg::VERSION),
            o if (ROM_BASE..ROM_BASE + DIAG_ROM.len() as u32).contains(&o) => {
                DIAG_ROM[(o - ROM_BASE) as usize]
            }
            _ => 0,
        }
    }

    /// Write a byte of the register file. See [`Hostblk::read`] for the
    /// offset layout.
    pub fn write(&mut self, offset: u32, value: u8) {
        match offset {
            o if in_slot(o, reg::UNIT_SELECT) && o - reg::UNIT_SELECT == 3 => {
                self.unit_select = value;
            }
            o if in_slot(o, reg::UNIT_SELECT) => {}
            o if in_slot(o, reg::DOORBELL) => {
                set_byte_of(&mut self.doorbell_acc, o - reg::DOORBELL, value);
                if o - reg::DOORBELL == 3 {
                    self.submit(self.doorbell_acc);
                }
            }
            o if in_slot(o, reg::COMPLETION_ADVANCE) && o - reg::COMPLETION_ADVANCE == 3 => {
                self.completion_queue.pop();
            }
            o if in_slot(o, reg::COMPLETION_ADVANCE) => {}
            o if in_slot(o, reg::INT_ENABLE) && o - reg::INT_ENABLE == 3 => {
                self.int_enable = value;
            }
            o if in_slot(o, reg::INT_ENABLE) => {}
            _ => {}
        }
    }

    /// Queue a raw descriptor pointer for [`Hostblk::tick`] to execute.
    /// Drops it (and counts the drop) if the submission ring is already
    /// full -- module docs, "The submission ring: depth and overflow".
    /// Deliberately does **not** touch `ram` at all: reading the
    /// descriptor is `tick`'s job, once it is actually about to run
    /// (module docs, "Deferred completion").
    fn submit(&mut self, ptr: u32) {
        if !self.submit_queue.push(ptr) {
            self.submit_overflow = self.submit_overflow.wrapping_add(1);
        }
    }
}

impl Default for Hostblk<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `offset` falls in the 4-byte-aligned slot starting at `base`.
fn in_slot(offset: u32, base: u32) -> bool {
    (base..base + 4).contains(&offset)
}

/// A single-byte register's value at offset `base + 3` (the low-order
/// byte of the slot), `0` at the other three offsets in the slot --
/// `mirage.rs`'s same convention.
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
/// other three bytes untouched.
fn set_byte_of(value: &mut u32, lane: u32, byte: u8) {
    let shift = 8 * (3 - lane);
    *value = (*value & !(0xFFu32 << shift)) | ((byte as u32) << shift);
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- an in-memory `BlockDevice` and a small guest-RAM stand-in ------

    struct MemDisk {
        sectors: std::vec::Vec<[u8; SECTOR_BYTES]>,
        fail_after: Option<usize>,
        reads_done: usize,
        writes_done: usize,
    }

    impl MemDisk {
        fn new(count: usize) -> Self {
            Self {
                sectors: std::vec![[0u8; SECTOR_BYTES]; count],
                fail_after: None,
                reads_done: 0,
                writes_done: 0,
            }
        }
    }

    impl BlockDevice for MemDisk {
        fn sector_count(&self) -> u64 {
            self.sectors.len() as u64
        }

        fn read_sector(&mut self, lba: u64, buf: &mut [u8; SECTOR_BYTES]) -> bool {
            if Some(self.reads_done) == self.fail_after {
                return false;
            }
            match self.sectors.get(lba as usize) {
                Some(s) => {
                    *buf = *s;
                    self.reads_done += 1;
                    true
                }
                None => false,
            }
        }

        fn write_sector(&mut self, lba: u64, buf: &[u8; SECTOR_BYTES]) -> bool {
            if Some(self.writes_done) == self.fail_after {
                return false;
            }
            match self.sectors.get_mut(lba as usize) {
                Some(s) => {
                    *s = *buf;
                    self.writes_done += 1;
                    true
                }
                None => false,
            }
        }
    }

    const RAM_SIZE: usize = 64 * 1024;

    /// Flat guest RAM based at 0, so these unit tests can drive the card
    /// without standing a whole `MachineBus` up. It enforces the same
    /// refusals the real implementation does -- an overflowing span, or
    /// one running past the end of the region -- so the `BAD_ADDRESS`
    /// tests below still fail for the reason they claim to. What it
    /// deliberately does *not* model is more than one region; that the
    /// card reaches both chip RAM and fast RAM, and refuses a span
    /// straddling them, is covered by the bus-level tests in `lib.rs`
    /// against the real memory map.
    impl crate::GuestMemory for std::vec::Vec<u8> {
        fn ram_slice(&self, addr: u32, len: u32) -> Option<&[u8]> {
            let end = addr.checked_add(len)?;
            if end as usize > self.len() {
                return None;
            }
            Some(&self[addr as usize..end as usize])
        }

        fn ram_slice_mut(&mut self, addr: u32, len: u32) -> Option<&mut [u8]> {
            let end = addr.checked_add(len)?;
            if end as usize > self.len() {
                return None;
            }
            Some(&mut self[addr as usize..end as usize])
        }
    }

    fn ram() -> std::vec::Vec<u8> {
        std::vec![0u8; RAM_SIZE]
    }

    /// Build a [`Descriptor`]'s 20 bytes at `ptr` in `ram`.
    fn put_descriptor(
        ram: &mut [u8],
        ptr: u32,
        command: u8,
        unit: u8,
        length: u32,
        offset: u64,
        buffer: u32,
    ) {
        let base = ptr as usize;
        ram[base] = command;
        ram[base + 1] = unit;
        ram[base + 2] = 0;
        ram[base + 3] = 0;
        ram[base + 4..base + 8].copy_from_slice(&length.to_be_bytes());
        ram[base + 8..base + 16].copy_from_slice(&offset.to_be_bytes());
        ram[base + 16..base + 20].copy_from_slice(&buffer.to_be_bytes());
    }

    fn ring_doorbell(hb: &mut Hostblk, ptr: u32) {
        let b = ptr.to_be_bytes();
        hb.write(reg::DOORBELL, b[0]);
        hb.write(reg::DOORBELL + 1, b[1]);
        hb.write(reg::DOORBELL + 2, b[2]);
        hb.write(reg::DOORBELL + 3, b[3]);
    }

    fn select_unit(hb: &mut Hostblk, unit: u8) {
        hb.write(reg::UNIT_SELECT + 3, unit);
    }

    #[test]
    fn advertised_submission_depth_is_the_depth_the_ring_actually_drops_at() {
        // A doorbell write past the ring's depth is dropped and produces
        // no completion, so self-limiting is the only safe way to drive
        // this card -- which makes `SUBMIT_CAPACITY` load-bearing rather
        // than informational. A driver that trusts it and is wrong loses
        // requests silently, so the advertised number and the real
        // threshold are checked against each other here rather than each
        // being checked against the same constant.
        let mut disk = MemDisk::new(4);
        let mut hb = Hostblk::new();
        hb.attach_unit(0, &mut disk, false);
        let mut ram = ram();

        let advertised = read_u32(&hb, reg::SUBMIT_CAPACITY);
        assert!(advertised > 0, "a depth of zero would accept nothing");

        // Fill exactly the advertised depth, without ticking so nothing
        // drains: none of these may be dropped.
        for i in 0..advertised {
            put_descriptor(&mut ram, 0x100 + i * 0x40, cmd::FLUSH, 0, 0, 0, 0);
            ring_doorbell(&mut hb, 0x100 + i * 0x40);
        }
        assert_eq!(
            read_u32(&hb, reg::SUBMIT_OVERFLOW),
            0,
            "dropped a submission while still within the advertised depth"
        );

        // One more must be dropped, or the advertised depth understates
        // the ring and a driver would needlessly throttle itself.
        ring_doorbell(&mut hb, 0x9999);
        assert_eq!(
            read_u32(&hb, reg::SUBMIT_OVERFLOW),
            1,
            "accepted a submission past the advertised depth"
        );
    }

    #[test]
    fn version_register_reports_the_documented_protocol_version() {
        // Present so a future incompatible revision can be refused by a
        // driver rather than misread; see reg::VERSION.
        let hb = Hostblk::new();
        assert_eq!(read_u32(&hb, reg::VERSION), PROTOCOL_VERSION);
        assert_ne!(PROTOCOL_VERSION, 0, "0 is reserved for 'no card'");
    }

    fn read_u32(hb: &Hostblk, base: u32) -> u32 {
        u32::from_be_bytes([
            hb.read(base),
            hb.read(base + 1),
            hb.read(base + 2),
            hb.read(base + 3),
        ])
    }

    fn read_u64_pair(hb: &Hostblk, hi_base: u32, lo_base: u32) -> u64 {
        (u64::from(read_u32(hb, hi_base)) << 32) | u64::from(read_u32(hb, lo_base))
    }

    // ---- a read and a write round-trip through a BlockDevice ------------

    #[test]
    fn write_then_read_round_trips_through_the_block_device() {
        let mut disk = MemDisk::new(4);
        let mut hb = Hostblk::new();
        hb.attach_unit(0, &mut disk, false);
        hb.write(reg::INT_ENABLE + 3, 1);

        let mut ram = ram();
        let pattern: std::vec::Vec<u8> = (0..SECTOR_BYTES as u32).map(|i| i as u8).collect();
        let buf_addr = 0x1000u32;
        ram[buf_addr as usize..buf_addr as usize + SECTOR_BYTES].copy_from_slice(&pattern);

        let desc_addr = 0x100u32;
        put_descriptor(
            &mut ram,
            desc_addr,
            cmd::WRITE,
            0,
            SECTOR_BYTES as u32,
            SECTOR_BYTES as u64,
            buf_addr,
        );
        ring_doorbell(&mut hb, desc_addr);
        hb.tick(&mut ram);

        assert_eq!(read_u32(&hb, reg::COMPLETION_PTR), desc_addr);
        assert_eq!(hb.read(reg::COMPLETION_ERROR + 3), err::OK);
        assert_eq!(read_u32(&hb, reg::COMPLETION_ACTUAL), SECTOR_BYTES as u32);
        hb.write(reg::COMPLETION_ADVANCE + 3, 0);

        // Read it back into a different guest address.
        let read_buf_addr = 0x2000u32;
        put_descriptor(
            &mut ram,
            desc_addr,
            cmd::READ,
            0,
            SECTOR_BYTES as u32,
            SECTOR_BYTES as u64,
            read_buf_addr,
        );
        ring_doorbell(&mut hb, desc_addr);
        hb.tick(&mut ram);

        assert_eq!(hb.read(reg::COMPLETION_ERROR + 3), err::OK);
        assert_eq!(
            &ram[read_buf_addr as usize..read_buf_addr as usize + SECTOR_BYTES],
            &pattern[..]
        );
    }

    // ---- deferred completion ---------------------------------------------

    #[test]
    fn a_request_does_not_complete_within_the_doorbell_access() {
        let mut disk = MemDisk::new(4);
        let mut hb = Hostblk::new();
        hb.attach_unit(0, &mut disk, false);
        let mut ram = ram();
        put_descriptor(
            &mut ram,
            0x100,
            cmd::READ,
            0,
            SECTOR_BYTES as u32,
            0,
            0x1000,
        );

        ring_doorbell(&mut hb, 0x100);
        assert_eq!(
            read_u32(&hb, reg::COMPLETION_PTR),
            0,
            "nothing completed yet"
        );
        assert_eq!(hb.read(reg::COMPLETION_COUNT + 3), 0);

        hb.tick(&mut ram);
        assert_eq!(
            read_u32(&hb, reg::COMPLETION_PTR),
            0x100,
            "the tick landed it"
        );
    }

    // ---- the completion queue, including its full case --------------------

    #[test]
    fn completion_queue_backpressures_rather_than_dropping() {
        let mut disk = MemDisk::new(COMPLETION_QUEUE_CAPACITY + 4);
        let mut hb = Hostblk::new();
        hb.attach_unit(0, &mut disk, false);
        let mut ram = ram();

        // Queue and run enough requests to fill the completion queue.
        for i in 0..COMPLETION_QUEUE_CAPACITY {
            let desc_addr = 0x100 + i as u32 * 0x40;
            put_descriptor(
                &mut ram,
                desc_addr,
                cmd::READ,
                0,
                SECTOR_BYTES as u32,
                i as u64 * SECTOR_BYTES as u64,
                0x8000,
            );
            ring_doorbell(&mut hb, desc_addr);
            hb.tick(&mut ram);
        }
        assert_eq!(
            hb.read(reg::COMPLETION_COUNT + 3),
            COMPLETION_QUEUE_CAPACITY as u8
        );

        // One more request, queued but the completion queue has no room.
        let extra_addr = 0x2000u32;
        put_descriptor(
            &mut ram,
            extra_addr,
            cmd::READ,
            0,
            SECTOR_BYTES as u32,
            0,
            0x8000,
        );
        ring_doorbell(&mut hb, extra_addr);
        hb.tick(&mut ram); // must not advance: no room to hold its completion
        assert_eq!(
            hb.read(reg::COMPLETION_COUNT + 3),
            COMPLETION_QUEUE_CAPACITY as u8,
            "stalled behind the full completion queue, nothing lost"
        );

        // Draining one frees a slot and lets the stalled request land.
        hb.write(reg::COMPLETION_ADVANCE + 3, 0);
        hb.tick(&mut ram);

        // Walk the queue looking for the extra request's completion.
        let mut found = false;
        loop {
            let ptr = read_u32(&hb, reg::COMPLETION_PTR);
            if ptr == 0 {
                break;
            }
            if ptr == extra_addr {
                found = true;
            }
            hb.write(reg::COMPLETION_ADVANCE + 3, 0);
        }
        assert!(found, "the stalled request eventually completed");
    }

    #[test]
    fn submission_ring_overflow_is_counted_and_the_doorbell_write_is_dropped() {
        let mut disk = MemDisk::new(4);
        let mut hb = Hostblk::new();
        hb.attach_unit(0, &mut disk, false);
        let mut ram = ram();

        // Fill the submission ring without ticking, so nothing drains.
        for i in 0..SUBMIT_QUEUE_CAPACITY {
            put_descriptor(&mut ram, 0x100 + i as u32 * 0x40, cmd::FLUSH, 0, 0, 0, 0);
            ring_doorbell(&mut hb, 0x100 + i as u32 * 0x40);
        }
        assert_eq!(hb.read(reg::SUBMIT_OVERFLOW + 3), 0);

        ring_doorbell(&mut hb, 0x9999);
        assert_eq!(hb.read(reg::SUBMIT_OVERFLOW + 3), 1, "dropped, counted");
    }

    // ---- per-unit discovery ------------------------------------------------

    #[test]
    fn discovery_reports_size_and_write_protect_per_unit() {
        let mut a = MemDisk::new(1000);
        let mut b = MemDisk::new(2000);
        let mut hb = Hostblk::new();
        hb.attach_unit(0, &mut a, false);
        hb.attach_unit(1, &mut b, true);

        select_unit(&mut hb, 0);
        assert_eq!(hb.read(reg::UNIT_PRESENT + 3), 1);
        assert_eq!(hb.read(reg::UNIT_WRITE_PROTECT + 3), 0);
        assert_eq!(
            read_u64_pair(&hb, reg::UNIT_SECTORS_HI, reg::UNIT_SECTORS_LO),
            1000
        );

        select_unit(&mut hb, 1);
        assert_eq!(hb.read(reg::UNIT_PRESENT + 3), 1);
        assert_eq!(hb.read(reg::UNIT_WRITE_PROTECT + 3), 1);
    }

    // ---- a detached unit -----------------------------------------------------

    #[test]
    fn a_detached_unit_fails_cleanly() {
        let mut disk = MemDisk::new(4);
        let mut hb = Hostblk::new();
        hb.attach_unit(1, &mut disk, false); // unit 0 left absent
        let mut ram = ram();
        put_descriptor(
            &mut ram,
            0x100,
            cmd::READ,
            0,
            SECTOR_BYTES as u32,
            0,
            0x1000,
        );
        ring_doorbell(&mut hb, 0x100);
        hb.tick(&mut ram);
        assert_eq!(hb.read(reg::COMPLETION_ERROR + 3), err::BAD_UNIT);

        // Discovery agrees.
        select_unit(&mut hb, 0);
        assert_eq!(hb.read(reg::UNIT_PRESENT + 3), 0);
    }

    // ---- a descriptor pointing outside RAM -----------------------------------

    #[test]
    fn descriptor_pointer_outside_ram_is_rejected_without_touching_memory() {
        let mut disk = MemDisk::new(4);
        let mut hb = Hostblk::new();
        hb.attach_unit(0, &mut disk, false);
        let mut ram = ram();
        ring_doorbell(&mut hb, RAM_SIZE as u32); // one past the end
        hb.tick(&mut ram);
        assert_eq!(hb.read(reg::COMPLETION_ERROR + 3), err::BAD_ADDRESS);
    }

    #[test]
    fn misaligned_descriptor_pointer_is_rejected() {
        let mut disk = MemDisk::new(4);
        let mut hb = Hostblk::new();
        hb.attach_unit(0, &mut disk, false);
        let mut ram = ram();
        put_descriptor(
            &mut ram,
            0x104,
            cmd::READ,
            0,
            SECTOR_BYTES as u32,
            0,
            0x1000,
        );
        ring_doorbell(&mut hb, 0x105); // not 4-byte aligned
        hb.tick(&mut ram);
        assert_eq!(hb.read(reg::COMPLETION_ERROR + 3), err::BAD_ADDRESS);
    }

    #[test]
    fn buffer_outside_ram_is_rejected_before_any_device_io() {
        let mut disk = MemDisk::new(4);
        let mut hb = Hostblk::new();
        hb.attach_unit(0, &mut disk, false);
        let mut ram = ram();
        put_descriptor(
            &mut ram,
            0x100,
            cmd::READ,
            0,
            SECTOR_BYTES as u32,
            0,
            RAM_SIZE as u32 - 10,
        );
        ring_doorbell(&mut hb, 0x100);
        hb.tick(&mut ram);
        assert_eq!(hb.read(reg::COMPLETION_ERROR + 3), err::BAD_ADDRESS);
    }

    // ---- an over-long transfer -------------------------------------------

    #[test]
    fn transfer_past_the_units_capacity_is_rejected() {
        let mut disk = MemDisk::new(4);
        let mut hb = Hostblk::new();
        hb.attach_unit(0, &mut disk, false);
        let mut ram = ram();
        // 4 sectors on the device; ask for 8.
        put_descriptor(
            &mut ram,
            0x100,
            cmd::READ,
            0,
            8 * SECTOR_BYTES as u32,
            0,
            0x1000,
        );
        ring_doorbell(&mut hb, 0x100);
        hb.tick(&mut ram);
        assert_eq!(hb.read(reg::COMPLETION_ERROR + 3), err::OUT_OF_RANGE);
    }

    #[test]
    fn zero_length_and_misaligned_offset_are_rejected() {
        let mut disk = MemDisk::new(4);
        let mut hb = Hostblk::new();
        hb.attach_unit(0, &mut disk, false);
        let mut ram = ram();

        put_descriptor(&mut ram, 0x100, cmd::READ, 0, 0, 0, 0x1000);
        ring_doorbell(&mut hb, 0x100);
        hb.tick(&mut ram);
        assert_eq!(hb.read(reg::COMPLETION_ERROR + 3), err::INVALID_LENGTH);
        hb.write(reg::COMPLETION_ADVANCE + 3, 0);

        put_descriptor(
            &mut ram,
            0x140,
            cmd::READ,
            0,
            SECTOR_BYTES as u32,
            100,
            0x1000,
        );
        ring_doorbell(&mut hb, 0x140);
        hb.tick(&mut ram);
        assert_eq!(hb.read(reg::COMPLETION_ERROR + 3), err::MISALIGNED);
    }

    #[test]
    fn write_to_a_write_protected_unit_is_rejected_without_touching_the_device() {
        let mut disk = MemDisk::new(4);
        let mut hb = Hostblk::new();
        hb.attach_unit(0, &mut disk, true);
        let mut ram = ram();
        put_descriptor(
            &mut ram,
            0x100,
            cmd::WRITE,
            0,
            SECTOR_BYTES as u32,
            0,
            0x1000,
        );
        ring_doorbell(&mut hb, 0x100);
        hb.tick(&mut ram);
        assert_eq!(hb.read(reg::COMPLETION_ERROR + 3), err::WRITE_PROTECTED);
    }

    #[test]
    fn partial_io_failure_reports_residue() {
        let mut disk = MemDisk::new(4);
        disk.fail_after = Some(2); // succeed for 2 sectors, then fail
        let mut hb = Hostblk::new();
        hb.attach_unit(0, &mut disk, false);
        let mut ram = ram();
        put_descriptor(
            &mut ram,
            0x100,
            cmd::READ,
            0,
            4 * SECTOR_BYTES as u32,
            0,
            0x1000,
        );
        ring_doorbell(&mut hb, 0x100);
        hb.tick(&mut ram);
        assert_eq!(hb.read(reg::COMPLETION_ERROR + 3), err::IO_ERROR);
        assert_eq!(
            read_u32(&hb, reg::COMPLETION_ACTUAL),
            2 * SECTOR_BYTES as u32
        );
    }

    // ---- INT2 on both the read and write paths -----------------------------

    #[test]
    fn interrupt_asserts_on_the_write_completion_path() {
        let mut disk = MemDisk::new(4);
        let mut hb = Hostblk::new();
        hb.attach_unit(0, &mut disk, false);
        hb.write(reg::INT_ENABLE + 3, 1);
        let mut ram = ram();
        put_descriptor(
            &mut ram,
            0x100,
            cmd::WRITE,
            0,
            SECTOR_BYTES as u32,
            0,
            0x1000,
        );
        ring_doorbell(&mut hb, 0x100);
        assert!(!hb.irq_pending(), "not yet -- still queued");
        hb.tick(&mut ram);
        assert!(hb.irq_pending(), "the tick landed the completion");
    }

    #[test]
    fn interrupt_asserts_on_the_read_completion_path() {
        // The read-side analogue of the write test above -- `lib.rs`'s
        // module docs carry a hard-won reminder that a device whose
        // interrupt can change state must be checked on the read path
        // too, on pain of a multi-sector transfer stalling mid-flight
        // (there, an ATA per-sector refill; here, every completion).
        let mut disk = MemDisk::new(4);
        let mut hb = Hostblk::new();
        hb.attach_unit(0, &mut disk, false);
        hb.write(reg::INT_ENABLE + 3, 1);
        let mut ram = ram();
        put_descriptor(
            &mut ram,
            0x100,
            cmd::READ,
            0,
            SECTOR_BYTES as u32,
            0,
            0x1000,
        );
        ring_doorbell(&mut hb, 0x100);
        assert!(!hb.irq_pending());
        hb.tick(&mut ram);
        assert!(hb.irq_pending(), "the fetch landed on this tick");
    }

    #[test]
    fn interrupt_enable_gates_the_line() {
        let mut disk = MemDisk::new(4);
        let mut hb = Hostblk::new();
        hb.attach_unit(0, &mut disk, false);
        let mut ram = ram();
        put_descriptor(
            &mut ram,
            0x100,
            cmd::READ,
            0,
            SECTOR_BYTES as u32,
            0,
            0x1000,
        );
        ring_doorbell(&mut hb, 0x100);
        hb.tick(&mut ram);
        assert!(!hb.irq_pending(), "INT_ENABLE is 0 by default");
    }

    // ---- register file shape -------------------------------------------------

    #[test]
    fn board_spec_carries_the_documented_zorro_iii_identity() {
        let spec = Hostblk::board_spec();
        assert_eq!(spec.manufacturer, MANUFACTURER);
        assert_eq!(spec.size_bytes, WINDOW_BYTES);
        assert_eq!(spec.board_type, ERT_ZORROIII | ERTF_DIAGVALID);
        assert_eq!(spec.flags, ERFF_ZORRO_III | ERFF_EXTENDED);
        assert_eq!(
            spec.init_diag_vec, ROM_BASE as u16,
            "expansion.library adds er_InitDiagVec to the configured base \
             address to find the DiagArea -- must point at ROM_BASE"
        );
    }

    #[test]
    fn unimplemented_offsets_read_zero_not_open_bus() {
        let hb = Hostblk::new();
        assert_eq!(hb.read(0x38 + 4), 0); // one past VERSION's slot, before the ROM
        assert_eq!(
            hb.read(ROM_BASE + DIAG_ROM.len() as u32),
            0,
            "one past the ROM"
        );
        assert_eq!(hb.read(0x1_0000), 0, "deep in the unimplemented window");
    }

    // ---- the DiagArea boot ROM (brief items 1-4) --------------------------

    #[test]
    fn diag_rom_is_served_byte_for_byte_at_rom_base() {
        let hb = Hostblk::new();
        for (i, &expected) in DIAG_ROM.iter().enumerate() {
            assert_eq!(
                hb.read(ROM_BASE + i as u32),
                expected,
                "byte {i} of the embedded DiagArea ROM"
            );
        }
    }

    /// `libraries/configregs.h`'s `struct DiagArea` layout, read directly
    /// out of [`DIAG_ROM`] the same way `expansion.library` would after
    /// copying it into guest RAM -- an independent cross-check that the
    /// assembled ROM's header fields agree with the source file's da_Config/
    /// da_Size/da_DiagPoint/da_BootPoint layout, without re-deriving them
    /// from the source file itself (this test would fail exactly as loudly
    /// if the .s file's field order ever drifted from configregs.h).
    #[test]
    fn diag_rom_header_matches_the_documented_diagarea_layout() {
        let rom = DIAG_ROM;
        let da_config = rom[0];
        let da_flags = rom[1];
        let da_size = u16::from_be_bytes([rom[2], rom[3]]);
        let da_diag_point = u16::from_be_bytes([rom[4], rom[5]]);
        let da_boot_point = u16::from_be_bytes([rom[6], rom[7]]);

        assert_eq!(da_config & 0xC0, 0x80, "DAC_WORDWIDE (configregs.h)");
        assert_eq!(da_flags, 0, "da_Flags: configregs.h defines none");
        // Since the driver increment, da_Size covers only the DiagArea
        // header, the diagnostic marker, and the struct Resident (+ its two
        // name strings) that Kickstart's cold-start scan needs to find in
        // RAM -- NOT the whole assembled ROM. Everything else (rt_Init's
        // real target, the exec device, the RDB mounter) executes straight
        // off this board's own AUTOCONFIG window and is deliberately never
        // copied (m68k/hostblk-rom/hostblk-diagrom.s's file header explains
        // why: the window is ordinary bus-addressable memory the CPU can
        // already run code from directly, so copying it would only cost RAM
        // for no benefit). So da_Size must be strictly less than the full
        // ROM, with room left over for that uncopied code.
        assert!(
            (da_size as usize) < rom.len(),
            "da_Size must cover only the DiagArea/Resident copy region, \
             leaving the exec device and RDB mounter uncopied in the \
             board's own persistent window"
        );
        assert_ne!(
            da_diag_point, 0,
            "a zero da_DiagPoint means 'no diagnostic code'"
        );
        assert_ne!(
            da_boot_point, 0,
            "RKRM 'Events At DIAG Time': a zero da_BootPoint means \
             expansion.library never copies this area into RAM at all"
        );
        // da_DiagPoint/da_BootPoint are offsets *within the copied area*
        // (their own field doc: "relative to the structure"), so they must
        // fall inside da_Size, not merely inside the whole file.
        assert!((da_diag_point as usize) < da_size as usize);
        assert!((da_boot_point as usize) < da_size as usize);

        // DIAG_MARKER_OFFSET must land exactly where DiagEntry's own
        // scratch cell sits: right after the 14-byte DiagArea header, and
        // immediately before DiagEntry's own code (da_DiagPoint).
        assert_eq!(
            DIAG_MARKER_OFFSET, 14,
            "struct DiagArea's documented 14-byte size"
        );
        assert_eq!(
            da_diag_point as u32,
            DIAG_MARKER_OFFSET + 4,
            "DiagEntry's code must start right after the 4-byte marker cell"
        );
    }
}
