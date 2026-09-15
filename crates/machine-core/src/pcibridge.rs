//! `pcibridge` — the Zorro III shim that exposes the harness's PCI
//! config/BAR space to the guest (ADR 0005 stage 1's guest-visible half).
//!
//! `pci.rs` (a sibling increment) is the host-side abstraction: a
//! [`crate::pci::PciBackend`] trait plus a virtual topology that backs it
//! on `machine-hosted`. This file is the other half of ADR 0005's stage
//! 1 sentence — "the Zorro III shim that exposes the harness's PCI
//! config/BAR space to the guest" — and knows nothing about how the
//! backend is implemented, only how to drive it through the trait.
//!
//! # Register interface: `hostblk`/`rtgboard`'s idiom, no doorbell
//!
//! One hot byte per 4-byte-aligned slot for every single-byte register,
//! u32 registers as four big-endian byte lanes — `rtgboard.rs`'s
//! convention, inherited from `mirage.rs`/`hostblk.rs`. Anything not
//! named in [`reg`] reads `0` and discards writes: unimplemented board
//! space, the same posture `rtgboard.rs` takes (not the wider bus's
//! open-bus `0xFF`, which is `MachineBus`'s answer only when no card is
//! attached at all).
//!
//! Config access here is **synchronous**, exactly like `rtgboard`'s
//! [`reg::COMMIT`]: [`PciBridge::write`]'s [`reg::CFG_OP`] arm validates
//! and performs the access inline, and the result is available the
//! instant the driver reads [`reg::CFG_STATUS`]/[`reg::CFG_DATA`] back.
//! There is no asynchronous boundary here for the same reason
//! `rtgboard.rs`'s module docs give: a config-space read/write against a
//! backend that is itself a plain function call (real ECAM: one volatile
//! load/store; the virtual topology: an array walk) has no latency that
//! would ever need deferring across a tick. And so, unlike `hostblk`'s
//! doorbell-plus-completion-queue shape, **this card raises no
//! interrupt at all** — not yet: see "Why no interrupt exists on this
//! card yet" below.
//!
//! # Register map (stage 2; see `reg`)
//!
//! | Offset | Register | Width | Access |
//! |---|---|---|---|
//! | `0x00` | [`reg::VERSION`] | u32 | R |
//! | `0x04` | [`reg::CFG_ADDR`] | u32 | RW |
//! | `0x08` | [`reg::CFG_WIDTH`] | byte | RW |
//! | `0x0C` | [`reg::CFG_DATA`] | u32 | RW |
//! | `0x10` | [`reg::CFG_OP`] | byte | W |
//! | `0x14` | [`reg::CFG_STATUS`] | byte | RW (write-1-to-clear) |
//! | `0x18` | [`reg::APERTURE_BASE`] | u32 | RW |
//! | `0x1C` | [`reg::INTX_STATUS`] | u32 | R |
//! | `0x20` | [`reg::INTX_ENABLE`] | u32 | RW (bits 0-3 only) |
//! | `0x24` | [`reg::INTX_TEST`] | u32 | RW (bits 0-3 only) |
//! | [`APERTURE_BASE_OFFSET`]`..`[`super::pcibridge::WINDOW_BYTES`] | BAR aperture | byte, word, long | RW |
//!
//! [`reg::CFG_ADDR`] is ECAM-style: bits `[27:20]` are the target bus,
//! `[19:15]` the device, `[14:12]` the function, and `[11:0]` the offset
//! into that function's 4 KB config space — the same folding
//! [`crate::pci::PciBackend`]'s own doc comments describe for a
//! bare-metal ECAM implementation, just carried here as a guest-visible
//! scratch register instead of a hardware address. Bits `[31:28]` are
//! reserved and must be zero; [`reg::CFG_ADDR`] itself is plain
//! read/write scratch with no validation of its own (a driver may stage
//! nonsense into it freely), so those bits are checked only when
//! [`reg::CFG_OP`] actually tries to use the register — see "Hostile
//! input" below.
//!
//! [`reg::CFG_DATA`] carries the *decoded value*, not a byte image: a
//! `W8`/`W16` result lands in the low bits with the high bits forced to
//! zero, matching [`crate::pci::PciBackend`]'s own "values, not byte
//! images" contract that this card is built directly on top of.
//!
//! # Why the byte-order story is clean here
//!
//! [`reg::CFG_DATA`] is a ordinary big-endian register-lane register,
//! exactly like every other u32 register `rtgboard.rs`/`hostblk.rs`
//! expose: a 68k `move.l` from [`reg::CFG_DATA`] gets the correctly
//! decoded config-space value with no swap the driver needs to apply
//! itself, because this card's own register file already presents it
//! big-endian the way every other register on this bus does. This is
//! *not* where Prometheus/OpenPCI's address-invariant byte-swap fidelity
//! lives or is decided — that is stage 2's `pci.library` accessor
//! functions, which see a decoded value plus an explicit width from this
//! card (or from real ECAM) and are free to present *any* byte-order
//! contract to a driver above them, faithfully, because nothing at this
//! layer has thrown away the information (the value, the width) that
//! contract needs. Mixing that decision into this register file would
//! make the same byte-swap call twice, in two places that must never
//! disagree; keeping it out means stage 2 can be verified once, against
//! period driver behaviour, independently of this card.
//!
//! # Why the aperture is banked
//!
//! [`WINDOW_BYTES`] is 16 MB total, and [`APERTURE_BASE_OFFSET`] carves
//! 8 MB of that off as a single BAR aperture window onto PCI memory
//! space, which is up to 4 GB. [`reg::APERTURE_BASE`] slides that 8 MB
//! window to wherever in PCI memory space a driver currently needs to
//! reach — the same reason a real Prometheus system banks a small
//! window rather than mapping the whole of PCI memory space directly:
//! Zorro III's own address space is generous but still finite, and a
//! modern PCI device's BARs can be larger than any single window a
//! 68k expansion card can dedicate to them permanently.
//!
//! [`reg::APERTURE_BASE`] is plain scratch, like [`reg::CFG_ADDR`]: it
//! takes effect on the very next aperture access (no commit step,
//! unlike config cycles), because sliding a window has no invalid state
//! to guard against the way a config op's malformed address/width does.
//!
//! # `INTx` routing onto `INT2` (stage 2)
//!
//! Config cycles themselves are still synchronous (above, unchanged from
//! stage 1) — there is no completion to announce for those. But a PCI
//! *device*'s own `INTx` line is a genuinely asynchronous thing (real
//! hardware: the function raises it whenever its own logic decides to,
//! with no CPU access involved at all), and this card's job is to make
//! that line's live state visible to the guest and OR it onto Zorro's
//! shared `INT2` pin — `docs/pcibridge-protocol.md` §8 / `docs/
//! pci-library.md` §5's contract in full:
//!
//! - [`reg::INTX_STATUS`] is **not stored state**: every read recomputes
//!   `backend.intx_levels() | INTX_TEST`, live, the instant it is read
//!   ([`PciBridge::intx_status`]). Level-triggered means exactly this —
//!   the register tracks the line's *current* condition, with no
//!   write-1-to-clear and no edge to lose, unlike [`reg::CFG_STATUS`]
//!   just above it.
//! - [`reg::INTX_ENABLE`] is the mask a driver (`Prm_AddIntServer`,
//!   `docs/pci-library.md` §5) sets to unmask the line(s) it services.
//! - [`PciBridge::irq_pending`] is `(INTX_STATUS & INTX_ENABLE) != 0` —
//!   the same shared-line, poll-your-device discipline every other card
//!   on this bus uses, computed fresh on every call rather than cached,
//!   because [`crate::pci::PciBackend::intx_levels`] can change with no
//!   register write at all (stage 3's virtio-net ISR will raise/lower it
//!   from its own function logic).
//! - [`reg::INTX_TEST`] exists purely as a diagnostic assertion source:
//!   with no device having real function logic yet (this increment's
//!   virtio-net stub still answers all-ones behind every BAR — module
//!   docs on [`crate::pci::VirtioNetStub`]), there is nothing else that
//!   could exercise the INTA-INTD-to-INT2 wiring end to end before stage
//!   3 exists. It is ORed into [`reg::INTX_STATUS`] alongside whatever
//!   the backend itself reports, so the two sources are indistinguishable
//!   to a driver — deliberately: stage 3's virtio-net ISR replaces
//!   `INTX_TEST` as *a* source without changing anything else about the
//!   contract (`docs/pci-library.md` §6's probe-tool step 4).
//!
//! # Hostile input
//!
//! On any write to [`reg::CFG_OP`]'s hot byte, the checks below run in
//! order and the *first* failure sets [`status::REJECTED`] into
//! [`reg::CFG_STATUS`] and leaves [`reg::CFG_DATA`] completely untouched
//! — this is structural, not merely tested: no code path in
//! [`PciBridge::cfg_op`] ever writes [`reg::CFG_DATA`] until every check
//! below has passed, so "half a config op landed" is not a state this
//! card's own control flow can represent, the same discipline
//! `rtgboard.rs`'s `SET_*`/`CUR_*` staging split documents for its own
//! commit path.
//!
//! 1. The op value itself must be `0` (read) or `1` (write) — any other
//!    value is rejected outright.
//! 2. [`reg::CFG_ADDR`] bits `[31:28]` must be zero — these are reserved
//!    for an addressing extension this version does not define; a guest
//!    that sets them is relying on a contract that does not exist yet,
//!    so the honest answer is rejection, not a silent reinterpretation.
//! 3. [`reg::CFG_WIDTH`] must decode via
//!    [`crate::pci::AccessWidth::from_bytes`] (`1`, `2`, or `4`) — any
//!    other byte count is rejected.
//! 4. The 12-bit offset (bits `[11:0]` of [`reg::CFG_ADDR`]) must be
//!    aligned to the validated width (`offset & (width - 1) == 0`) —
//!    otherwise rejected. Alignment within a 4 KB config space also
//!    structurally guarantees `offset + width <= 4096`, so no separate
//!    bounds check is needed once this one passes: the largest aligned
//!    offset for width 4 is `0xFFC`, and `0xFFC + 4 == 0x1000`.
//!
//! Once every check passes, [`crate::pci::Bdf`] `{ bus: bits[27:20],
//! device: bits[19:15], function: bits[14:12] }` and the 12-bit offset
//! are decoded from [`reg::CFG_ADDR`], and the backend's
//! `config_read`/`config_write` is called. **An absent device is not a
//! rejection.** [`crate::pci::PciBackend`]'s own contract is that an
//! absent device's config space reads all-ones — real PCI master-abort
//! semantics — and that is exactly what a config read against one
//! returns here, with [`status::COMPLETED`] set precisely as it would be
//! for a present device: all-ones *is* the signal bus enumeration relies
//! on to learn a slot is empty, not an error condition this card treats
//! specially. [`status::REJECTED`] means "this card refused to even
//! attempt the access" (a malformed request); [`status::COMPLETED`]
//! means "the backend was asked, and answered" — including "answered
//! all-ones because nothing was there".
//!
//! [`reg::CFG_STATUS`] is write-1-to-clear and **ungated**, the same
//! shape `rtgboard::reg::STATUS` takes and for the identical reason:
//! there is no queue behind it that an early acknowledgement could
//! strand, so a driver may clear it the instant it has read the result.
//!
//! # The BAR aperture, and sized accesses (stage 2)
//!
//! An access at window offset [`APERTURE_BASE_OFFSET`]`+ k` reaches the
//! backend's PCI memory space at address `(APERTURE_BASE as u64) + (k as
//! u64)`. Both operands are zero-extended from a `u32` before the
//! addition, so the sum can never exceed `2 * u32::MAX`, which fits
//! comfortably below `u64::MAX` — this addition is therefore infallible
//! and needs no `checked_add`, unlike [`crate::pci::VirtualPciBus`]'s own
//! window-containment arithmetic (which subtracts instead, guarding a
//! *different* hostile input: an adversarial BAR base near `u64::MAX`,
//! not reachable from this card's own `u32` `APERTURE_BASE`).
//!
//! Byte accesses ([`PciBridge::read`]/[`PciBridge::write`]'s own aperture
//! arm) always reach the backend as a single `W8` access, unchanged from
//! stage 1. [`PciBridge::read_aperture_sized`]/[`PciBridge::
//! write_aperture_sized`] are stage 2's addition: `docs/
//! pcibridge-protocol.md` §5 recorded, from stage 1, that virtio's modern
//! spec requires a driver to access fields at their *natural* width, and
//! that decomposing every word/long access into bytes (as `MachineBus`'s
//! ordinary byte-decomposition path still does everywhere else on this
//! bus) cannot honour that. These two methods perform exactly **one**
//! backend access of the requested width — `MachineBus`'s own
//! `read_word`/`read_long`/`write_word`/`write_long` arms (`lib.rs`) call
//! them only for an access that lies entirely within the aperture and is
//! naturally aligned to its own width; everything else (the register
//! file, a misaligned aperture access) keeps decomposing to bytes exactly
//! as before, unchanged.
//!
//! **Byte-lane swap, and why it is needed even though [`crate::pci::
//! PciBackend`] is value-carrying (§3).** [`crate::pci::PciBackend::
//! mem_read`]/`mem_write` traffic in *decoded little-endian values* (a
//! same-width load on this trait's little-endian real-ECAM backing needs
//! no swap at all — trait docs). But address-invariance (§3, and `docs/
//! pci-library.md` §3) demands that aperture byte `k` be PCI byte
//! `APERTURE_BASE + k` at *every* width, the same guarantee stage 1's
//! byte-by-byte path already gave for free (one byte in, one byte out,
//! nothing to get backwards). A sized access has no such luxury: the
//! backend hands back (or expects) a little-endian-composed integer, and
//! this card's own registers present everything big-endian (module docs,
//! "Why the byte-order story is clean here") — so [`swap_lanes`] performs
//! exactly the byte-lane reversal within `width` that reconciles the two,
//! at read and write alike (writes: swap first, then hand the backend
//! its little-endian value; symmetric). The result: a driver doing a
//! sized word/long aperture access sees byte-for-byte what four/two byte
//! accesses at the same offset would have produced, which is exactly what
//! §5's tests assert.
//!
//! # No `DiagArea`, host side only, this increment
//!
//! Like `input.rs`'s and `rtgboard.rs`'s own first increments, this card
//! carries no boot ROM and no driver yet — [`PciBridge::board_spec`]'s
//! `init_diag_vec` is `0`. Stage 2 (`pci.library`) and stage 3 (the
//! virtio-net SANA-II driver) are later increments that build on top of
//! this guest-visible register file, not blocked on it changing shape.

use crate::autoconfig::{BoardSpec, ERT_ZORROIII};
use crate::pci::{AccessWidth, Bdf, PciBackend};

/// Reuses `hostblk`/`rtgboard`'s reserved manufacturer ID rather than
/// minting a new placeholder — the same NDK 3.2 `libraries/configregs.h`
/// "hacker" ID ($7DB, decimal 2011) reserved for test use (`hostblk.rs`'s
/// module docs carry the full story of why `0xFFFF` was tried and
/// rejected by real Kickstart 3.2.2). Still a stand-in: a real registered
/// number is needed before this ships on hardware.
pub const MANUFACTURER: u16 = crate::hostblk::MANUFACTURER;

/// This card's product number under [`MANUFACTURER`] — distinct from
/// `mirage` (`0`), `hostblk` (`1`), `fastram` (`2`), `input` (`3`),
/// `rtgboard` (`4`) and `pktport` (`5`).
pub const PRODUCT: u8 = 6;

/// The Zorro III AUTOCONFIG window: 16 MB, extended-table code 0 — see
/// `hostblk.rs`/`rtgboard.rs`'s module docs for why a 16 MB Zorro III
/// board's `er_Type` carries no size bits of its own.
pub const WINDOW_BYTES: u32 = 0x0100_0000;

/// `er_Flags` bit 4: this is a genuine Zorro III board.
const ERFF_ZORRO_III: u8 = 1 << 4;
/// `er_Flags` bit 5: `er_Type`'s size bits index the 16 MB-1 GB extended
/// table rather than Zorro II's 64 KB-8 MB one.
const ERFF_EXTENDED: u8 = 1 << 5;

/// The value [`reg::VERSION`] reports. History: `1` was stage 1 (config
/// cycles and the byte-granular BAR aperture, no `INTx`). `2` is stage 2
/// (this increment): the `INTx` registers at `0x1C..0x28` and sized
/// aperture accesses are now part of the contract, so a stage-2 guest
/// library refuses any card that does not answer `2` here — a deliberate
/// breaking bump, not silently tolerated (`docs/pci-library.md` §2).
pub const PROTOCOL_VERSION: u32 = 2;

/// Where the BAR aperture starts within this board's 16 MB AUTOCONFIG
/// window — well clear of the register file (which ends at `0x28`),
/// with room to spare before it for the register file to grow, the same
/// reasoning `rtgboard::VRAM_BASE`'s doc comment gives for its own
/// aperture placement.
pub const APERTURE_BASE_OFFSET: u32 = 0x0080_0000;

/// Register offsets within this board's AUTOCONFIG window. See the
/// module docs for the reasoning behind each register.
pub mod reg {
    /// Protocol version; `1` for this increment. Read-only.
    pub const VERSION: u32 = 0x00;
    /// ECAM-style scratch address: bits `[27:20]` bus, `[19:15]` device,
    /// `[14:12]` function, `[11:0]` offset into that function's 4 KB
    /// config space; bits `[31:28]` reserved (checked only at
    /// [`CFG_OP`] time, module docs' "Hostile input"). Plain read/write
    /// scratch otherwise.
    pub const CFG_ADDR: u32 = 0x04;
    /// Access width in bytes for the next [`CFG_OP`]: `1`, `2`, or `4`
    /// (validated at [`CFG_OP`] time). Byte register, hot at offset+3.
    pub const CFG_WIDTH: u32 = 0x08;
    /// Staged write data (for a config write) / latched read result (for
    /// a config read) — the *decoded value*, not a byte image (module
    /// docs). Read/write.
    pub const CFG_DATA: u32 = 0x0C;
    /// Any write to this byte register's hot byte performs the config
    /// access [`CFG_ADDR`]/[`CFG_WIDTH`] describe: `0` = read (result
    /// latched into [`CFG_DATA`]), `1` = config write (of [`CFG_DATA`]).
    /// Write-only (reads `0`), synchronous like `rtgboard::reg::COMMIT`.
    pub const CFG_OP: u32 = 0x10;
    /// Write-1-to-clear result of the most recent [`CFG_OP`]:
    /// [`super::status::REJECTED`] or [`super::status::COMPLETED`],
    /// mutually exclusive per op. Ungated, like `rtgboard::reg::STATUS`
    /// (module docs).
    pub const CFG_STATUS: u32 = 0x14;
    /// The PCI memory-space address the BAR aperture window currently
    /// maps to, zero-extended to `u64` toward the backend. Plain
    /// scratch: takes effect on the very next aperture access, no commit
    /// step (module docs, "Why the aperture is banked").
    pub const APERTURE_BASE: u32 = 0x18;
    /// Live `INTA`-`INTD` line levels ORed with [`INTX_TEST`]'s own bits
    /// (module docs, "`INTx` routing onto `INT2`"), recomputed on every
    /// read — never stored, never latched. Bits 4-31 always read `0`.
    /// Read-only; writes discarded.
    pub const INTX_STATUS: u32 = 0x1C;
    /// Mask of which [`INTX_STATUS`] bits actually assert `INT2`
    /// (`docs/pci-library.md` §5/§6: `Prm_AddIntServer` unmasks a line
    /// here). Only bits 0-3 are writable; bits 4-31 always read `0` and
    /// discard writes.
    pub const INTX_ENABLE: u32 = 0x20;
    /// Diagnostic line assertion, ORed straight into [`INTX_STATUS`]:
    /// exists so the `INTA`-`INTD`-to-`INT2` routing is provable before
    /// any device has real function logic (module docs) -- stage 3's
    /// virtio-net ISR replaces this as *a* source of asserted bits, not
    /// as the mechanism. Only bits 0-3 are writable; bits 4-31 always
    /// read `0` and discard writes.
    pub const INTX_TEST: u32 = 0x24;
}

/// [`reg::CFG_STATUS`] bit values.
pub mod status {
    /// The most recent [`reg::CFG_OP`] was refused before ever reaching
    /// the backend; [`reg::CFG_DATA`] is unchanged from whatever it was
    /// staged to before the write (module docs, "Hostile input").
    pub const REJECTED: u8 = 1 << 0;
    /// The most recent [`reg::CFG_OP`] reached the backend and it
    /// answered — including "answered all-ones because no device is
    /// there" (module docs: absent-device is not a rejection).
    pub const COMPLETED: u8 = 1 << 1;
}

/// `pcibridge`'s register file and BAR aperture, driving a caller-owned
/// [`PciBackend`]. See the module docs for the protocol this implements.
pub struct PciBridge<'a> {
    backend: &'a mut dyn PciBackend,

    cfg_addr: u32,
    cfg_width: u8,
    cfg_data: u32,
    cfg_status: u8,
    aperture_base: u32,
    /// [`reg::INTX_ENABLE`]'s low nibble; bits 4-7 are always `0` (module
    /// docs, "`INTx` routing onto `INT2`") -- stored as a byte like
    /// [`Self::cfg_width`]/[`Self::cfg_status`] since only 4 bits are
    /// ever live, the same "hot byte, nibble-masked" idiom applied to a
    /// register the protocol still describes as a full `u32`.
    intx_enable: u8,
    /// [`reg::INTX_TEST`]'s low nibble, same storage shape as
    /// [`Self::intx_enable`].
    intx_test: u8,
}

impl<'a> PciBridge<'a> {
    /// Build a bridge over a caller-owned backend (borrowed, like every
    /// other card's backing store on this bus — this crate has no
    /// allocator).
    pub fn new(backend: &'a mut dyn PciBackend) -> Self {
        Self {
            backend,
            cfg_addr: 0,
            cfg_width: 0,
            cfg_data: 0,
            cfg_status: 0,
            aperture_base: 0,
            intx_enable: 0,
            intx_test: 0,
        }
    }

    /// The `BoardSpec` this card registers on the AUTOCONFIG chain: one
    /// Zorro III board, no `DiagArea` (`ERTF_DIAGVALID` unset) — this
    /// increment carries no boot ROM and no driver (module docs, "No
    /// `DiagArea`, host side only, this increment").
    pub fn board_spec() -> BoardSpec {
        BoardSpec {
            board_type: ERT_ZORROIII, // extended-table code 0 == 16 MB
            product: PRODUCT,
            flags: ERFF_ZORRO_III | ERFF_EXTENDED,
            manufacturer: MANUFACTURER,
            serial: 0,
            init_diag_vec: 0,
            size_bytes: WINDOW_BYTES,
        }
    }

    /// Validate and perform the config access [`reg::CFG_ADDR`]/
    /// [`reg::CFG_WIDTH`]/[`reg::CFG_OP`] currently describe. See the
    /// module docs' "Hostile input" for the exact check order; every
    /// check below is independent and the first failure wins,
    /// structurally never touching [`Self::cfg_data`] before all of them
    /// pass.
    fn cfg_op(&mut self, op: u8) {
        // Check 1: op must be 0 (read) or 1 (write).
        if op > 1 {
            self.cfg_status = status::REJECTED;
            return;
        }
        // Check 2: CFG_ADDR bits [31:28] reserved, must be zero.
        if self.cfg_addr & 0xF000_0000 != 0 {
            self.cfg_status = status::REJECTED;
            return;
        }
        // Check 3: CFG_WIDTH must decode to a real access width.
        let Some(width) = AccessWidth::from_bytes(self.cfg_width) else {
            self.cfg_status = status::REJECTED;
            return;
        };
        // Check 4: the 12-bit offset must be width-aligned. This also
        // guarantees offset + width <= 4096 (module docs): the largest
        // aligned offset for width 4 is 0xFFC, and 0xFFC + 4 == 0x1000.
        let offset = (self.cfg_addr & 0x0000_0FFF) as u16;
        if offset as u32 & (width.bytes() - 1) != 0 {
            self.cfg_status = status::REJECTED;
            return;
        }

        let bdf = Bdf {
            bus: ((self.cfg_addr >> 20) & 0xFF) as u8,
            device: ((self.cfg_addr >> 15) & 0x1F) as u8,
            function: ((self.cfg_addr >> 12) & 0x7) as u8,
        };

        if op == 0 {
            // Config read: an absent device answers all-ones (real
            // master-abort semantics) and this still COMPLETES (module
            // docs) -- never REJECTED.
            self.cfg_data = self.backend.config_read(bdf, offset, width);
        } else {
            self.backend.config_write(bdf, offset, width, self.cfg_data);
        }
        self.cfg_status = status::COMPLETED;
    }

    /// Read a byte of the BAR aperture: window offset
    /// [`APERTURE_BASE_OFFSET`]`+ k` reaches the backend's PCI memory
    /// space at `(aperture_base as u64) + (k as u64)` — infallible `u64`
    /// addition of two zero-extended `u32`s (module docs, "The BAR
    /// aperture").
    fn read_aperture(&mut self, k: u32) -> u8 {
        let addr = self.aperture_base as u64 + k as u64;
        self.backend.mem_read(addr, AccessWidth::W8) as u8
    }

    /// Write a byte of the BAR aperture, same addressing as
    /// [`Self::read_aperture`].
    fn write_aperture(&mut self, k: u32, value: u8) {
        let addr = self.aperture_base as u64 + k as u64;
        self.backend.mem_write(addr, AccessWidth::W8, value as u32);
    }

    /// Perform ONE backend access of `width` into the BAR aperture at
    /// window offset [`APERTURE_BASE_OFFSET`]`+ k`, address-invariant at
    /// every width (module docs, "The BAR aperture, and sized accesses").
    /// `MachineBus`'s own `read_word`/`read_long` call this only for a
    /// naturally-aligned access lying entirely within the aperture; every
    /// other case keeps decomposing into [`Self::read_aperture`] byte
    /// calls unchanged.
    pub fn read_aperture_sized(&mut self, k: u32, width: AccessWidth) -> u32 {
        let addr = self.aperture_base as u64 + k as u64;
        swap_lanes(self.backend.mem_read(addr, width), width)
    }

    /// Write ONE backend access of `width` into the BAR aperture, same
    /// addressing and swap as [`Self::read_aperture_sized`] (symmetric:
    /// swap first, then hand the backend its little-endian value).
    pub fn write_aperture_sized(&mut self, k: u32, width: AccessWidth, value: u32) {
        let addr = self.aperture_base as u64 + k as u64;
        self.backend
            .mem_write(addr, width, swap_lanes(value, width));
    }

    /// [`reg::INTX_STATUS`]'s live value: the backend's own asserted
    /// lines ORed with [`Self::intx_test`]'s diagnostic bits, masked to
    /// the four bits this register actually has (module docs, "`INTx`
    /// routing onto `INT2`"). Recomputed every call -- there is no stored
    /// status to go stale.
    fn intx_status(&mut self) -> u8 {
        ((self.backend.intx_levels() | self.intx_test as u32) & 0x0F) as u8
    }

    /// Whether this card is currently holding `INT2` asserted:
    /// `(INTX_STATUS & INTX_ENABLE) != 0`, level-triggered (module docs)
    /// -- `MachineBus` polls this after every register write that could
    /// change the answer, and once per host tick besides (backend state
    /// can change with no register write at all).
    pub fn irq_pending(&mut self) -> bool {
        (self.intx_status() & self.intx_enable) != 0
    }

    /// Advance whatever engine the backend holds by one step -- ADR 0005
    /// stage 3's virtio-net ring processing, forwarded straight through
    /// (module docs on [`PciBackend::tick`]). `pcibridge` itself has no
    /// engine of its own (config cycles stay synchronous, module docs),
    /// so this is pure delegation; [`crate::MachineBus::tick`] drives it
    /// with the same lift-out-of-the-`Option`, call, put-back dance
    /// `hostblk`/`pktport`'s own engines use.
    pub fn tick(&mut self, mem: &mut dyn crate::GuestMemory) {
        self.backend.tick(mem);
    }

    /// Host-side cross-check of a guest-assigned BAR (or any other config
    /// register): reads straight through the backend, bypassing every
    /// guest-visible register ([`reg::CFG_ADDR`]/[`reg::CFG_WIDTH`]/
    /// [`reg::CFG_OP`]/[`reg::CFG_DATA`]) entirely. **Never a path a
    /// guest driver can reach** -- this exists solely for `--inspect` to
    /// print what a device itself latched, so it can be compared against
    /// whatever the guest's own `pci.library`/`pciprobe` reported through
    /// the ordinary config-cycle registers (`docs/pci-library.md` §6,
    /// "BAR consistency is checked from both sides").
    pub fn config_read_for_inspection(
        &mut self,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        width: AccessWidth,
    ) -> u32 {
        self.backend.config_read(
            Bdf {
                bus,
                device,
                function,
            },
            offset,
            width,
        )
    }

    /// Read a byte of the register file, offset from this board's
    /// configured AUTOCONFIG base, or of the BAR aperture above
    /// [`APERTURE_BASE_OFFSET`]. `&mut self`: an aperture read reaches
    /// the backend, which may have its own state (`TestMemDevice`'s
    /// recorded last write, a real device's side effects). See [`reg`]
    /// for the register layout; anything unnamed reads `0` — `rtgboard.
    /// rs`'s posture, not the wider bus's open-bus `0xFF`.
    pub fn read(&mut self, offset: u32) -> u8 {
        match offset {
            o if in_slot(o, reg::VERSION) => byte_of(PROTOCOL_VERSION, o - reg::VERSION),
            o if in_slot(o, reg::CFG_ADDR) => byte_of(self.cfg_addr, o - reg::CFG_ADDR),
            o if in_slot(o, reg::CFG_WIDTH) => low_byte(o, reg::CFG_WIDTH, self.cfg_width),
            o if in_slot(o, reg::CFG_DATA) => byte_of(self.cfg_data, o - reg::CFG_DATA),
            o if in_slot(o, reg::CFG_OP) => 0, // write-only
            o if in_slot(o, reg::CFG_STATUS) => low_byte(o, reg::CFG_STATUS, self.cfg_status),
            o if in_slot(o, reg::APERTURE_BASE) => {
                byte_of(self.aperture_base, o - reg::APERTURE_BASE)
            }
            o if in_slot(o, reg::INTX_STATUS) => low_byte(o, reg::INTX_STATUS, self.intx_status()),
            o if in_slot(o, reg::INTX_ENABLE) => low_byte(o, reg::INTX_ENABLE, self.intx_enable),
            o if in_slot(o, reg::INTX_TEST) => low_byte(o, reg::INTX_TEST, self.intx_test),
            o if (APERTURE_BASE_OFFSET..WINDOW_BYTES).contains(&o) => {
                self.read_aperture(o - APERTURE_BASE_OFFSET)
            }
            _ => 0,
        }
    }

    /// Write a byte of the register file, or of the BAR aperture above
    /// [`APERTURE_BASE_OFFSET`]. See [`Self::read`] for the offset
    /// layout.
    pub fn write(&mut self, offset: u32, value: u8) {
        match offset {
            o if in_slot(o, reg::VERSION) => {} // read-only
            o if in_slot(o, reg::CFG_ADDR) => {
                set_byte_of(&mut self.cfg_addr, o - reg::CFG_ADDR, value)
            }
            o if in_slot(o, reg::CFG_WIDTH) && o - reg::CFG_WIDTH == 3 => {
                self.cfg_width = value;
            }
            o if in_slot(o, reg::CFG_WIDTH) => {}
            o if in_slot(o, reg::CFG_DATA) => {
                set_byte_of(&mut self.cfg_data, o - reg::CFG_DATA, value)
            }
            o if in_slot(o, reg::CFG_OP) && o - reg::CFG_OP == 3 => self.cfg_op(value),
            o if in_slot(o, reg::CFG_OP) => {}
            o if in_slot(o, reg::CFG_STATUS) && o - reg::CFG_STATUS == 3 => {
                // Write-1-to-clear, ungated (module docs: no queue behind
                // this to strand, unlike `input::reg::INT_STATUS`).
                self.cfg_status &= !value;
            }
            o if in_slot(o, reg::CFG_STATUS) => {}
            o if in_slot(o, reg::APERTURE_BASE) => {
                set_byte_of(&mut self.aperture_base, o - reg::APERTURE_BASE, value)
            }
            o if in_slot(o, reg::INTX_STATUS) => {} // read-only, discarded
            o if in_slot(o, reg::INTX_ENABLE) && o - reg::INTX_ENABLE == 3 => {
                // Only bits 0-3 are writable (module docs); the other
                // four bits of this byte are simply never stored, which
                // is what makes them permanently read `0` in `read`
                // above without a separate mask on every read.
                self.intx_enable = value & 0x0F;
            }
            o if in_slot(o, reg::INTX_ENABLE) => {} // the other three bytes: always 0, discarded
            o if in_slot(o, reg::INTX_TEST) && o - reg::INTX_TEST == 3 => {
                self.intx_test = value & 0x0F;
            }
            o if in_slot(o, reg::INTX_TEST) => {}
            o if (APERTURE_BASE_OFFSET..WINDOW_BYTES).contains(&o) => {
                self.write_aperture(o - APERTURE_BASE_OFFSET, value)
            }
            _ => {}
        }
    }
}

/// Byte-lane-reverse `value`'s low `width` bytes, the rest left `0` --
/// see [`PciBridge::read_aperture_sized`]'s doc comment for why this
/// reconciles [`crate::pci::PciBackend`]'s little-endian-decoded values
/// with this card's big-endian register presentation at every sized
/// width. `AccessWidth::W8` is a no-op: a single byte has no lanes to
/// reverse, matching [`PciBridge::read_aperture`]'s own untouched byte
/// path.
fn swap_lanes(value: u32, width: AccessWidth) -> u32 {
    match width {
        AccessWidth::W8 => value,
        AccessWidth::W16 => (value as u16).swap_bytes() as u32,
        AccessWidth::W32 => value.swap_bytes(),
    }
}

/// Whether `offset` falls in the 4-byte-aligned slot starting at `base`.
fn in_slot(offset: u32, base: u32) -> bool {
    (base..base + 4).contains(&offset)
}

/// A single-byte register's value at offset `base + 3` (the low-order
/// byte of the slot), `0` at the other three offsets in the slot --
/// `mirage.rs`/`hostblk.rs`/`rtgboard.rs`'s same convention.
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
    use crate::pci::{
        HostBridge, IntxTestDevice, NullNetBackend, TestMemDevice, VirtioNetStub, VirtualPciBus,
        VirtualSlot,
    };

    fn read_u32(dev: &mut PciBridge, base: u32) -> u32 {
        u32::from_be_bytes([
            dev.read(base),
            dev.read(base + 1),
            dev.read(base + 2),
            dev.read(base + 3),
        ])
    }

    fn write_u32(dev: &mut PciBridge, base: u32, value: u32) {
        let b = value.to_be_bytes();
        dev.write(base, b[0]);
        dev.write(base + 1, b[1]);
        dev.write(base + 2, b[2]);
        dev.write(base + 3, b[3]);
    }

    /// Pack a config address the way a driver would: bus/device/function
    /// folded into the offsets [`super::PciBridge::cfg_op`] expects.
    fn pack_cfg_addr(bus: u8, device: u8, function: u8, offset: u16) -> u32 {
        (bus as u32) << 20 | (device as u32) << 15 | (function as u32) << 12 | offset as u32
    }

    fn stage_and_op(dev: &mut PciBridge, addr: u32, width: u8, op: u8) {
        write_u32(dev, reg::CFG_ADDR, addr);
        dev.write(reg::CFG_WIDTH + 3, width);
        dev.write(reg::CFG_OP + 3, op);
    }

    // ---- VERSION / board identity -----------------------------------------

    #[test]
    fn version_reads_two() {
        let mut bridge = HostBridge::new();
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 0,
                function: 0,
            },
            device: &mut bridge,
        }];
        let mut bus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut bus);
        assert_eq!(read_u32(&mut dev, reg::VERSION), 2);
        assert_eq!(PROTOCOL_VERSION, 2);
    }

    #[test]
    fn board_spec_carries_a_distinct_product_and_no_diagarea_yet() {
        let spec = PciBridge::board_spec();
        assert_eq!(spec.manufacturer, MANUFACTURER);
        assert_eq!(spec.product, PRODUCT);
        assert_ne!(spec.product, crate::mirage::PRODUCT);
        assert_ne!(spec.product, crate::hostblk::PRODUCT);
        assert_ne!(spec.product, crate::fastram::PRODUCT);
        assert_ne!(spec.product, crate::input::PRODUCT);
        assert_ne!(spec.product, crate::rtgboard::PRODUCT);
        assert_ne!(spec.product, crate::pktport::PRODUCT);
        assert_eq!(spec.size_bytes, WINDOW_BYTES);
        assert_eq!(
            spec.init_diag_vec, 0,
            "no DiagArea yet -- host side only, this increment"
        );
    }

    // ---- config read cycle, every width ------------------------------------

    #[test]
    fn a_full_w32_config_read_cycle_reports_device_and_vendor_id() {
        let mut bridge = HostBridge::new();
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        let mut slots = [
            VirtualSlot {
                bdf: Bdf {
                    bus: 0,
                    device: 0,
                    function: 0,
                },
                device: &mut bridge,
            },
            VirtualSlot {
                bdf: Bdf {
                    bus: 0,
                    device: 1,
                    function: 0,
                },
                device: &mut net,
            },
        ];
        let mut vbus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut vbus);

        stage_and_op(&mut dev, pack_cfg_addr(0, 1, 0, 0), 4, 0);

        assert_eq!(dev.read(reg::CFG_STATUS + 3), status::COMPLETED);
        let expected = (VirtioNetStub::DEVICE_ID as u32) << 16 | VirtioNetStub::VENDOR_ID as u32;
        assert_eq!(read_u32(&mut dev, reg::CFG_DATA), expected);
        assert_eq!(expected, 0x1041_1AF4);
    }

    #[test]
    fn a_w16_read_at_offset_zero_reports_vendor_id_with_high_bits_zero() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 1,
                function: 0,
            },
            device: &mut net,
        }];
        let mut vbus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut vbus);

        stage_and_op(&mut dev, pack_cfg_addr(0, 1, 0, 0), 2, 0);
        assert_eq!(dev.read(reg::CFG_STATUS + 3), status::COMPLETED);
        assert_eq!(read_u32(&mut dev, reg::CFG_DATA), 0x1AF4);
    }

    #[test]
    fn a_w8_read_at_offset_one_reports_the_vendor_ids_high_byte() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 1,
                function: 0,
            },
            device: &mut net,
        }];
        let mut vbus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut vbus);

        stage_and_op(&mut dev, pack_cfg_addr(0, 1, 0, 1), 1, 0);
        assert_eq!(dev.read(reg::CFG_STATUS + 3), status::COMPLETED);
        // Config space is a little-endian byte stream (`pci.rs`'s own
        // module docs): offset 1 of vendor id 0x1AF4 is its *high* byte,
        // 0x1A -- the same relationship `pci.rs`'s own
        // `enumeration_reads_vendor_and_device_id_correctly_at_every_width`
        // test pins for `HostBridge`.
        assert_eq!(read_u32(&mut dev, reg::CFG_DATA), 0x1A);
    }

    // ---- absent device: COMPLETED, all-ones, not REJECTED ------------------

    #[test]
    fn an_absent_device_completes_with_all_ones_not_a_rejection() {
        let mut bridge = HostBridge::new();
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 0,
                function: 0,
            },
            device: &mut bridge,
        }];
        let mut vbus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut vbus);

        stage_and_op(&mut dev, pack_cfg_addr(0, 2, 0, 0), 4, 0);
        assert_eq!(dev.read(reg::CFG_STATUS + 3), status::COMPLETED);
        assert_eq!(read_u32(&mut dev, reg::CFG_DATA), 0xFFFF_FFFF);
    }

    // ---- config write cycle -------------------------------------------------

    #[test]
    fn a_config_write_then_read_round_trips_the_command_register() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 1,
                function: 0,
            },
            device: &mut net,
        }];
        let mut vbus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut vbus);

        write_u32(&mut dev, reg::CFG_DATA, 0x0007);
        stage_and_op(&mut dev, pack_cfg_addr(0, 1, 0, 4), 2, 1);
        assert_eq!(dev.read(reg::CFG_STATUS + 3), status::COMPLETED);
        dev.write(reg::CFG_STATUS + 3, 0xFF);

        stage_and_op(&mut dev, pack_cfg_addr(0, 1, 0, 4), 2, 0);
        assert_eq!(dev.read(reg::CFG_STATUS + 3), status::COMPLETED);
        assert_eq!(read_u32(&mut dev, reg::CFG_DATA), 0x0007);
    }

    // ---- BAR sizing through the card ----------------------------------------

    #[test]
    fn bar_sizing_probe_through_the_card_reports_the_masked_size() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 1,
                function: 0,
            },
            device: &mut net,
        }];
        let mut vbus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut vbus);

        write_u32(&mut dev, reg::CFG_DATA, 0xFFFF_FFFF);
        stage_and_op(&mut dev, pack_cfg_addr(0, 1, 0, 0x10), 4, 1);
        assert_eq!(dev.read(reg::CFG_STATUS + 3), status::COMPLETED);
        dev.write(reg::CFG_STATUS + 3, 0xFF);

        stage_and_op(&mut dev, pack_cfg_addr(0, 1, 0, 0x10), 4, 0);
        assert_eq!(read_u32(&mut dev, reg::CFG_DATA), 0xFFFF_C000);
        dev.write(reg::CFG_STATUS + 3, 0xFF);

        let base = 0x2000_0000u32;
        write_u32(&mut dev, reg::CFG_DATA, base);
        stage_and_op(&mut dev, pack_cfg_addr(0, 1, 0, 0x10), 4, 1);
        dev.write(reg::CFG_STATUS + 3, 0xFF);

        stage_and_op(&mut dev, pack_cfg_addr(0, 1, 0, 0x10), 4, 0);
        assert_eq!(
            read_u32(&mut dev, reg::CFG_DATA),
            base & !(VirtioNetStub::BAR0_SIZE - 1)
        );
    }

    // ---- hostile CFG_OP input ------------------------------------------------

    #[test]
    fn an_unknown_op_value_is_rejected_and_leaves_cfg_data_untouched() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 1,
                function: 0,
            },
            device: &mut net,
        }];
        let mut vbus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut vbus);

        write_u32(&mut dev, reg::CFG_DATA, 0xABCD_1234);
        stage_and_op(&mut dev, pack_cfg_addr(0, 1, 0, 0), 4, 2);
        assert_eq!(dev.read(reg::CFG_STATUS + 3), status::REJECTED);
        assert_eq!(read_u32(&mut dev, reg::CFG_DATA), 0xABCD_1234);

        // Clear status and confirm a valid op still works afterwards.
        dev.write(reg::CFG_STATUS + 3, 0xFF);
        stage_and_op(&mut dev, pack_cfg_addr(0, 1, 0, 0), 2, 0);
        assert_eq!(dev.read(reg::CFG_STATUS + 3), status::COMPLETED);
        assert_eq!(read_u32(&mut dev, reg::CFG_DATA), 0x1AF4);
    }

    #[test]
    fn a_reserved_high_bit_in_cfg_addr_is_rejected_and_leaves_cfg_data_untouched() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 1,
                function: 0,
            },
            device: &mut net,
        }];
        let mut vbus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut vbus);

        write_u32(&mut dev, reg::CFG_DATA, 0x5555_5555);
        stage_and_op(&mut dev, pack_cfg_addr(0, 1, 0, 0) | 0x8000_0000, 4, 0);
        assert_eq!(dev.read(reg::CFG_STATUS + 3), status::REJECTED);
        assert_eq!(read_u32(&mut dev, reg::CFG_DATA), 0x5555_5555);

        dev.write(reg::CFG_STATUS + 3, 0xFF);
        stage_and_op(&mut dev, pack_cfg_addr(0, 1, 0, 0), 2, 0);
        assert_eq!(dev.read(reg::CFG_STATUS + 3), status::COMPLETED);
    }

    #[test]
    fn an_invalid_width_is_rejected_and_leaves_cfg_data_untouched() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 1,
                function: 0,
            },
            device: &mut net,
        }];
        let mut vbus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut vbus);

        write_u32(&mut dev, reg::CFG_DATA, 0x1111_1111);
        stage_and_op(&mut dev, pack_cfg_addr(0, 1, 0, 0), 3, 0);
        assert_eq!(dev.read(reg::CFG_STATUS + 3), status::REJECTED);
        assert_eq!(read_u32(&mut dev, reg::CFG_DATA), 0x1111_1111);

        dev.write(reg::CFG_STATUS + 3, 0xFF);
        stage_and_op(&mut dev, pack_cfg_addr(0, 1, 0, 0), 4, 0);
        assert_eq!(dev.read(reg::CFG_STATUS + 3), status::COMPLETED);
    }

    #[test]
    fn a_misaligned_width_four_offset_is_rejected_and_leaves_cfg_data_untouched() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 1,
                function: 0,
            },
            device: &mut net,
        }];
        let mut vbus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut vbus);

        write_u32(&mut dev, reg::CFG_DATA, 0x2222_2222);
        stage_and_op(&mut dev, pack_cfg_addr(0, 1, 0, 2), 4, 0);
        assert_eq!(dev.read(reg::CFG_STATUS + 3), status::REJECTED);
        assert_eq!(read_u32(&mut dev, reg::CFG_DATA), 0x2222_2222);

        dev.write(reg::CFG_STATUS + 3, 0xFF);
        stage_and_op(&mut dev, pack_cfg_addr(0, 1, 0, 0), 4, 0);
        assert_eq!(dev.read(reg::CFG_STATUS + 3), status::COMPLETED);
    }

    // ---- aperture ------------------------------------------------------------

    #[test]
    fn aperture_reads_and_writes_route_to_the_backend_bar() {
        let mut mem = TestMemDevice::new();
        let base = 0x1000_0000u32;
        {
            let mut slots = [VirtualSlot {
                bdf: Bdf {
                    bus: 0,
                    device: 3,
                    function: 0,
                },
                device: &mut mem,
            }];
            let mut vbus = VirtualPciBus::new(&mut slots);
            let mut dev = PciBridge::new(&mut vbus);

            // Enable memory space (COMMAND bit 1) and program BAR0's base
            // through the card's own registers -- full-stack, not by
            // poking the device directly.
            write_u32(&mut dev, reg::CFG_DATA, 0x0002);
            stage_and_op(&mut dev, pack_cfg_addr(0, 3, 0, 0x04), 2, 1);
            dev.write(reg::CFG_STATUS + 3, 0xFF);

            write_u32(&mut dev, reg::CFG_DATA, base);
            stage_and_op(&mut dev, pack_cfg_addr(0, 3, 0, 0x10), 4, 1);
            dev.write(reg::CFG_STATUS + 3, 0xFF);

            write_u32(&mut dev, reg::APERTURE_BASE, base);

            for k in [0u32, 1, 2, 100] {
                assert_eq!(
                    dev.read(APERTURE_BASE_OFFSET + k),
                    k as u8,
                    "pattern byte at k={k}"
                );
            }

            dev.write(APERTURE_BASE_OFFSET + 5, 0x77);
        }
        // Reach through to the raw device to inspect what actually
        // landed -- the card itself has no read-back of `last_write`.
        assert_eq!(
            mem.last_write(),
            Some((5, AccessWidth::W8, 0x77)),
            "aperture write recorded at the correct bar-relative offset"
        );
    }

    /// Enable memory space and program BAR0 to `base` through the card's
    /// own config-cycle registers, then bank the aperture at `base` --
    /// the common setup every sized-aperture test below shares with
    /// [`aperture_reads_and_writes_route_to_the_backend_bar`] above
    /// (full-stack, never poking the device directly).
    fn enable_and_map_bar0(dev: &mut PciBridge, base: u32) {
        write_u32(dev, reg::CFG_DATA, 0x0002);
        stage_and_op(dev, pack_cfg_addr(0, 3, 0, 0x04), 2, 1);
        dev.write(reg::CFG_STATUS + 3, 0xFF);

        write_u32(dev, reg::CFG_DATA, base);
        stage_and_op(dev, pack_cfg_addr(0, 3, 0, 0x10), 4, 1);
        dev.write(reg::CFG_STATUS + 3, 0xFF);

        write_u32(dev, reg::APERTURE_BASE, base);
    }

    #[test]
    fn a_sized_word_aperture_read_equals_the_byte_composed_read_as_one_backend_access() {
        let mut mem = TestMemDevice::new();
        let base = 0x1000_0000u32;
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 3,
                function: 0,
            },
            device: &mut mem,
        }];
        let mut vbus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut vbus);
        enable_and_map_bar0(&mut dev, base);

        let k = 0x10u32;
        // Byte-composed: two separate byte reads, big-endian composed,
        // exactly what the aperture's ordinary byte path already gives.
        let byte_composed = ((dev.read(APERTURE_BASE_OFFSET + k) as u16) << 8)
            | dev.read(APERTURE_BASE_OFFSET + k + 1) as u16;

        let sized = dev.read_aperture_sized(k, AccessWidth::W16) as u16;
        assert_eq!(
            sized, byte_composed,
            "a sized word read must see the same byte lanes byte-by-byte would"
        );
    }

    #[test]
    fn a_sized_word_aperture_read_reaches_the_backend_as_one_width_2_access() {
        let mut mem = TestMemDevice::new();
        let base = 0x1000_0000u32;
        {
            let mut slots = [VirtualSlot {
                bdf: Bdf {
                    bus: 0,
                    device: 3,
                    function: 0,
                },
                device: &mut mem,
            }];
            let mut vbus = VirtualPciBus::new(&mut slots);
            let mut dev = PciBridge::new(&mut vbus);
            enable_and_map_bar0(&mut dev, base);
            let _ = dev.read_aperture_sized(0x10, AccessWidth::W16);
        }

        assert_eq!(
            mem.read_count(AccessWidth::W16),
            1,
            "exactly one width-2 backend access, not two width-1s"
        );
        assert_eq!(mem.read_count(AccessWidth::W8), 0);
        assert_eq!(mem.read_count(AccessWidth::W32), 0);
    }

    #[test]
    fn a_sized_long_aperture_read_reaches_the_backend_as_one_width_4_access() {
        let mut mem = TestMemDevice::new();
        let base = 0x1000_0000u32;
        let byte_composed;
        {
            let mut slots = [VirtualSlot {
                bdf: Bdf {
                    bus: 0,
                    device: 3,
                    function: 0,
                },
                device: &mut mem,
            }];
            let mut vbus = VirtualPciBus::new(&mut slots);
            let mut dev = PciBridge::new(&mut vbus);
            enable_and_map_bar0(&mut dev, base);

            let k = 0x20u32;
            byte_composed = u32::from_be_bytes([
                dev.read(APERTURE_BASE_OFFSET + k),
                dev.read(APERTURE_BASE_OFFSET + k + 1),
                dev.read(APERTURE_BASE_OFFSET + k + 2),
                dev.read(APERTURE_BASE_OFFSET + k + 3),
            ]);
            let sized = dev.read_aperture_sized(k, AccessWidth::W32);
            assert_eq!(
                sized, byte_composed,
                "a sized long read must see the same byte lanes byte-by-byte would"
            );
        }

        assert_eq!(
            mem.read_count(AccessWidth::W32),
            1,
            "exactly one width-4 backend access, not four width-1s"
        );
        assert_eq!(
            mem.read_count(AccessWidth::W8),
            4,
            "the byte-composed check above"
        );
    }

    #[test]
    fn a_sized_aperture_write_round_trips_through_the_swap() {
        let mut mem = TestMemDevice::new();
        let base = 0x1000_0000u32;
        {
            let mut slots = [VirtualSlot {
                bdf: Bdf {
                    bus: 0,
                    device: 3,
                    function: 0,
                },
                device: &mut mem,
            }];
            let mut vbus = VirtualPciBus::new(&mut slots);
            let mut dev = PciBridge::new(&mut vbus);
            enable_and_map_bar0(&mut dev, base);
            dev.write_aperture_sized(0x30, AccessWidth::W32, 0x1122_3344);
        }

        // The backend saw the little-endian-decoded value the guest's
        // big-endian 0x1122_3344 swaps to.
        assert_eq!(
            mem.last_write(),
            Some((0x30, AccessWidth::W32, 0x4433_2211))
        );
    }

    #[test]
    fn aperture_base_near_u32_max_does_not_panic_and_unmapped_reads_are_ff() {
        let mut mem = TestMemDevice::new();
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 3,
                function: 0,
            },
            device: &mut mem,
        }];
        let mut vbus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut vbus);

        // Memory space never enabled, and APERTURE_BASE parked right at
        // the top of u32 range -- the u64 addition must not panic, and
        // an unmapped access must read all-ones (master abort).
        write_u32(&mut dev, reg::APERTURE_BASE, u32::MAX - 5);
        assert_eq!(dev.read(APERTURE_BASE_OFFSET + 10), 0xFF);
        dev.write(APERTURE_BASE_OFFSET + 10, 0x99); // must not panic
    }

    // ---- reserved offsets and the gap ----------------------------------------

    #[test]
    fn intx_registers_only_expose_their_low_order_lane() {
        // Stage 1's reserved `0x1C..0x28` range is now fully claimed by
        // the three INTx registers (module docs) -- this is the
        // replacement for the old "reserved range" coverage: each
        // register's top three byte lanes always read `0` and discard
        // writes, exactly like every other u32 register's non-hot lanes
        // on this card (`CFG_WIDTH`/`CFG_STATUS`'s own "hot at offset+3"
        // shape).
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 1,
                function: 0,
            },
            device: &mut net,
        }];
        let mut vbus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut vbus);

        for base in [reg::INTX_STATUS, reg::INTX_ENABLE, reg::INTX_TEST] {
            for lane in 0..3 {
                let offset = base + lane;
                assert_eq!(dev.read(offset), 0, "register {base:#x} lane {lane}");
                dev.write(offset, 0xFF);
                assert_eq!(
                    dev.read(offset),
                    0,
                    "register {base:#x} lane {lane} after write"
                );
            }
        }
    }

    #[test]
    fn the_gap_between_reserved_and_the_aperture_reads_zero_and_discards_writes() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 1,
                function: 0,
            },
            device: &mut net,
        }];
        let mut vbus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut vbus);

        let gap = 0x1000u32; // past every named register, well before the aperture
        assert!(gap < APERTURE_BASE_OFFSET);
        assert_eq!(dev.read(gap), 0);
        dev.write(gap, 0xFF);
        assert_eq!(dev.read(gap), 0);
    }

    // ---- INTx registers -------------------------------------------------------

    #[test]
    fn intx_enable_and_test_only_keep_their_low_nibble() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 1,
                function: 0,
            },
            device: &mut net,
        }];
        let mut vbus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut vbus);

        write_u32(&mut dev, reg::INTX_ENABLE, 0xFFFF_FFFF);
        assert_eq!(
            read_u32(&mut dev, reg::INTX_ENABLE),
            0x0F,
            "only bits 0-3 are writable"
        );

        write_u32(&mut dev, reg::INTX_TEST, 0xFFFF_FFFF);
        assert_eq!(read_u32(&mut dev, reg::INTX_TEST), 0x0F);
    }

    #[test]
    fn intx_status_tracks_the_backend_live_with_no_latching() {
        let mut dev = IntxTestDevice::new(1); // INTA -> bit 0

        // Before asserting: INTX_STATUS reads 0.
        {
            let mut slots = [VirtualSlot {
                bdf: Bdf {
                    bus: 0,
                    device: 0,
                    function: 0,
                },
                device: &mut dev,
            }];
            let mut vbus = VirtualPciBus::new(&mut slots);
            let mut bridge = PciBridge::new(&mut vbus);
            assert_eq!(read_u32(&mut bridge, reg::INTX_STATUS), 0);
        }

        // Assert the device's line directly, with no register write in
        // sight -- exactly the case module docs call out: stage 3's
        // virtio-net ISR will change `INTX_STATUS`'s answer with no
        // `CFG_OP`/`INTX_ENABLE`/`INTX_TEST` write at all, and this
        // register must track it live, not go stale until poked.
        dev.set_asserted(true);
        {
            let mut slots = [VirtualSlot {
                bdf: Bdf {
                    bus: 0,
                    device: 0,
                    function: 0,
                },
                device: &mut dev,
            }];
            let mut vbus = VirtualPciBus::new(&mut slots);
            let mut bridge = PciBridge::new(&mut vbus);
            assert_eq!(read_u32(&mut bridge, reg::INTX_STATUS), 0b0001);
        }

        // Deassert: the bit clears, with nothing to write-1-to-clear --
        // level-triggered, no latch (module docs).
        dev.set_asserted(false);
        {
            let mut slots = [VirtualSlot {
                bdf: Bdf {
                    bus: 0,
                    device: 0,
                    function: 0,
                },
                device: &mut dev,
            }];
            let mut vbus = VirtualPciBus::new(&mut slots);
            let mut bridge = PciBridge::new(&mut vbus);
            assert_eq!(read_u32(&mut bridge, reg::INTX_STATUS), 0);
        }
    }

    #[test]
    fn irq_pending_is_true_only_while_status_and_enable_are_both_nonzero() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 1,
                function: 0,
            },
            device: &mut net,
        }];
        let mut vbus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut vbus);

        // Nothing enabled, nothing asserted.
        assert!(!dev.irq_pending());

        // INTX_TEST asserts a line, but INTX_ENABLE still masks it.
        write_u32(&mut dev, reg::INTX_TEST, 0x1);
        assert_eq!(read_u32(&mut dev, reg::INTX_STATUS), 0x1);
        assert!(
            !dev.irq_pending(),
            "asserted but not enabled must not raise INT2"
        );

        // Enable that line: now it is pending.
        write_u32(&mut dev, reg::INTX_ENABLE, 0x1);
        assert!(dev.irq_pending());

        // Deassert INTX_TEST: pending clears even though ENABLE is
        // untouched -- the condition, not a latch, is what gates it.
        write_u32(&mut dev, reg::INTX_TEST, 0x0);
        assert!(!dev.irq_pending());
    }

    #[test]
    fn register_file_and_aperture_do_not_alias() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 1,
                function: 0,
            },
            device: &mut net,
        }];
        let mut vbus = VirtualPciBus::new(&mut slots);
        let mut dev = PciBridge::new(&mut vbus);

        write_u32(&mut dev, reg::CFG_ADDR, 0x1122_3344);
        // A register write must not be visible through the aperture path
        // (which routes to the backend's BAR, not this card's own
        // fields) -- reading the aperture at a low offset must not
        // somehow echo CFG_ADDR back.
        assert_ne!(dev.read(APERTURE_BASE_OFFSET), 0x11);
    }
}
