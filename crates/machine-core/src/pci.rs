//! `pci` — the host-side PCI abstraction for ADR 0005 stage 1.
//!
//! Implements `docs/adr-0005-networking-virtio-behind-pci-library.md`'s
//! stage 1: "the Zorro III shim that exposes the harness's PCI config/BAR
//! space to the guest, with host-side tests proving enumeration against
//! QEMU's real topology — before any 68k code exists". This file is the
//! host-side half of that sentence: a [`PciBackend`] trait the Zorro III
//! shim card (`pcibridge.rs`, a sibling increment) is written against, a
//! borrowed-backing virtual PCI topology ([`VirtualPciBus`]) that backs it
//! on `machine-hosted` (which has no real PCI bus), and the concrete
//! devices that topology can hold. Stage 2 (`pci.library` itself) and
//! stage 3 (the virtio-net SANA-II driver) are later increments; nothing
//! here is guest-visible register content — that is entirely `pcibridge.rs`'s
//! concern.
//!
//! # Why a trait, not a `machine-hosted`-only struct
//!
//! ADR 0005's whole point is that `pci.library` must be "demonstrably
//! backable by real ECAM" on the bare-metal board crates (`board-qemu-virt`
//! aarch64, `board-qemu-q35` x86-64), which have a real PCIe root complex
//! and therefore need no virtual topology at all — a config access there is
//! just a volatile load/store at a computed ECAM address. [`PciBackend`] is
//! the seam that lets the same shim card code run against either backing
//! without knowing which one it has, the same shape [`crate::pktport::
//! PacketBackend`] gives `pktport.rs` for "a host filesystem service today,
//! something else tomorrow" and [`rtgboard::ModeDescriptor`]'s borrowed
//! catalog gives a mode list "rich here, one entry there" — borrow
//! everything, own nothing, let the board layer decide what is on the
//! other end.
//!
//! # Values, not byte images — and why that is deliberate
//!
//! [`PciBackend::config_read`]/[`PciBackend::mem_read`] return the register
//! as a decoded **integer**, not a byte slice or a byte-swapped image. This
//! is deliberate and is *not* where Prometheus/OpenPCI byte-swap fidelity
//! lives: real PCI config space is a little-endian byte stream regardless
//! of host or guest endianness, and on every host this project targets
//! (aarch64, x86-64 — both little-endian), a same-width volatile load of
//! that stream *is* the correctly decoded integer with no swap required.
//! The guest-facing contract — what byte order a big-endian 68k driver
//! sees crossing the shim's own register window, and the address-invariant
//! behaviour real Prometheus bridges exhibit — is `pcibridge.rs`'s register
//! file and, later, stage 2's `pci.library` accessor functions. Mixing that
//! concern into this trait would make the same byte-swap decision twice in
//! two places that must never disagree; keeping it out means this trait can
//! be verified once, against real ECAM semantics, and the guest-facing swap
//! verified separately against period driver behaviour.
//!
//! # The real-ECAM mapping this trait must support
//!
//! A future bare-metal implementation (not built here; documented so one
//! can be written against this trait without re-deriving the addressing)
//! backs [`PciBackend::config_read`]/[`PciBackend::config_write`] as a
//! single volatile load/store of the requested `width` at:
//!
//! ```text
//! ecam_base + ( (bus as u64) << 20
//!             | (device as u64) << 15
//!             | (function as u64) << 12
//!             | offset as u64 )
//! ```
//!
//! (PCIe base spec, ECAM: 1 MB per bus, 32 KB per device, 4 KB per
//! function's config space — `offset` is that same 12-bit-plus config
//! offset this trait already carries). On a little-endian host
//! (aarch64/x86-64, this project's only board targets) the loaded integer
//! *is* the value this trait returns: no byte swap, because config space is
//! defined little-endian and the host CPU already decodes little-endian
//! memory that way. [`PciBackend::mem_read`]/[`PciBackend::mem_write`] are
//! the same idea one level up: raw loads/stores at whatever address the
//! board layer's own memory map places PCI memory space at (BARs are
//! programmed against that same address space by whatever ran
//! enumeration), with no ECAM-style address folding at all — PCI memory
//! space is just memory.
//!
//! # No allocator, caller supplies everything
//!
//! Same borrowed-backing philosophy as [`crate::pktport::PacketBackend`]
//! and [`rtgboard`]'s mode catalog: [`VirtualPciBus`] borrows its device
//! list (`&'a mut [VirtualSlot<'a>]`) rather than owning a `Vec`, and
//! [`ConfigSpace`] is a fixed 256-byte array, never a growable buffer. This
//! crate has no allocator and no `alloc` dependency (crate root doc
//! comment); nothing in this file changes that.

/// A PCI bus/device/function address.
///
/// `device` is conventionally `< 32` and `function` is conventionally
/// `< 8` (five bits and three bits of the ECAM offset respectively), but
/// neither this type nor anything that consumes it may assume a caller
/// respects that: an out-of-range `device`/`function` simply folds into
/// an address no real device answers at (or, for [`VirtualPciBus`], an
/// exact match nothing in the slot list has), matching real hardware's own
/// master-abort behaviour for an address past a bridge's actual device
/// count. There is no constructor-time validation to bypass, by design.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bdf {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

/// The width of one config-space or memory-space access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessWidth {
    W8,
    W16,
    W32,
}

impl AccessWidth {
    /// How many bytes this width occupies.
    pub fn bytes(self) -> u32 {
        match self {
            AccessWidth::W8 => 1,
            AccessWidth::W16 => 2,
            AccessWidth::W32 => 4,
        }
    }

    /// The width `n` bytes names, or `None` for any other byte count —
    /// hostile input (a driver or test computing a width from an untrusted
    /// size) fails closed rather than guessing the nearest valid width.
    pub fn from_bytes(n: u8) -> Option<Self> {
        match n {
            1 => Some(AccessWidth::W8),
            2 => Some(AccessWidth::W16),
            4 => Some(AccessWidth::W32),
            _ => None,
        }
    }
}

/// What backs the PCI shim on a given host — see the module docs for the
/// full rationale ("Why a trait", "Values, not byte images", "The real-ECAM
/// mapping").
///
/// # Real-ECAM implementation note
///
/// A bare-metal board's implementation of this trait computes, for
/// `config_read`/`config_write`:
///
/// ```text
/// addr = ecam_base + ((bdf.bus as u64) << 20
///                    | (bdf.device as u64) << 15
///                    | (bdf.function as u64) << 12
///                    | offset as u64)
/// ```
///
/// and performs one volatile load/store of `width` at `addr`. On a
/// little-endian host (every board target this project has) the loaded
/// integer is the value to return/the value to write — no byte swap.
/// `mem_read`/`mem_write` are raw physical loads/stores at the address the
/// board layer's own memory map assigns to PCI memory space (however that
/// board remaps or windows it); this trait does no address translation of
/// its own for those two methods.
///
/// # Access contract
///
/// `offset` is an offset into the 4 KB per-function configuration space.
/// The *caller* guarantees `offset` is `width`-aligned and lies within
/// `0..4096`, but an implementation must fail closed (return all-ones on a
/// read, silently discard a write) rather than panic if that is violated —
/// hostile input discipline applies here exactly as it does to every other
/// guest-influenced value in this crate.
///
/// An absent device's config space reads all-ones for every width (real
/// PCI master-abort semantics) — this is exactly the mechanism bus
/// enumeration uses to discover that nothing answers at a `Bdf`.
/// Config-space writes to an absent device, or to a read-only bit of a
/// present one, are simply discarded.
///
/// `mem_read`/`mem_write` use `u64` addresses (not `u32`) so a 64-bit BAR
/// on a real host — [`ConfigSpace`] here only ever builds 32-bit BARs, but
/// a real board's PCIe devices are not so constrained — stays
/// representable without this trait needing a second, wider variant later.
/// An unmapped memory-space read returns all-ones; an unmapped write is
/// discarded.
pub trait PciBackend {
    /// Read `width` from config space at (`bdf`, `offset`). See the trait
    /// docs, "Access contract".
    fn config_read(&mut self, bdf: Bdf, offset: u16, width: AccessWidth) -> u32;

    /// Write `value`, same contract as [`Self::config_read`].
    fn config_write(&mut self, bdf: Bdf, offset: u16, width: AccessWidth, value: u32);

    /// Read from PCI memory space (where BARs live) at `addr`.
    fn mem_read(&mut self, addr: u64, width: AccessWidth) -> u32;

    /// Write `value` to PCI memory space, same contract as
    /// [`Self::mem_read`].
    fn mem_write(&mut self, addr: u64, width: AccessWidth, value: u32);
}

/// One virtual device's config space and BAR-mapped function, as seen by
/// [`VirtualPciBus`].
///
/// A device that has nothing behind its BAR apertures (this file's
/// [`HostBridge`] and [`VirtioNetStub`] — see their own doc comments)
/// simply keeps the default [`Self::bar_read`]/[`Self::bar_write`], which
/// is exactly real hardware's own master-abort-style response to an access
/// nothing claims.
pub trait PciDevice {
    /// Read `width` from this device's own config space at `offset`.
    fn config_read(&mut self, offset: u16, width: AccessWidth) -> u32;

    /// Write `value` to this device's own config space, same contract.
    fn config_write(&mut self, offset: u16, width: AccessWidth, value: u32);

    /// The currently programmed absolute PCI memory-space address window
    /// of BAR `bar`, as `(base, len)`, or `None` if that BAR does not exist
    /// on this device, is not memory-mapped, or memory-space decoding is
    /// currently disabled in this device's own `COMMAND` register.
    /// [`VirtualPciBus`] uses this to route [`PciBackend::mem_read`]/
    /// [`PciBackend::mem_write`] — see the module docs' "command-gating"
    /// note: gating lives here, in the device model, rather than in
    /// [`VirtualPciBus`] itself, because whether memory-space decode is
    /// enabled is state this device alone owns ([`ConfigSpace`]'s
    /// `COMMAND` register).
    fn bar_window(&self, bar: usize) -> Option<(u64, u64)>;

    /// Access into BAR `bar` at `offset` bytes from its base. Default: a
    /// device whose function is a later stage's problem — all-ones, the
    /// same master-abort-shaped answer an absent config-space device
    /// gives.
    fn bar_read(&mut self, bar: usize, offset: u64, width: AccessWidth) -> u32 {
        let _ = (bar, offset, width);
        u32::MAX
    }

    /// Write into BAR `bar`, same default posture as [`Self::bar_read`]:
    /// discarded.
    fn bar_write(&mut self, bar: usize, offset: u64, width: AccessWidth, value: u32) {
        let _ = (bar, offset, width, value);
    }
}

/// One slot on [`VirtualPciBus`]: a fixed [`Bdf`] address paired with the
/// device that answers at it.
pub struct VirtualSlot<'a> {
    pub bdf: Bdf,
    pub device: &'a mut dyn PciDevice,
}

/// A host-side virtual PCI topology — the [`PciBackend`] `machine-hosted`
/// attaches, since it has no real PCI bus of its own (module docs).
///
/// Borrows its whole device list (module docs, "No allocator, caller
/// supplies everything") rather than owning a growable collection; the
/// slot list's length and every slot's [`Bdf`] are fixed for the borrow's
/// lifetime, matching how a real bus's topology is fixed once QEMU (or
/// real firmware) has enumerated it.
pub struct VirtualPciBus<'a> {
    devices: &'a mut [VirtualSlot<'a>],
}

impl<'a> VirtualPciBus<'a> {
    /// Build a bus over a caller-owned slot list.
    pub fn new(devices: &'a mut [VirtualSlot<'a>]) -> Self {
        Self { devices }
    }

    /// The slot whose `Bdf` exactly matches `bdf`, if any — config-space
    /// routing is exact-match only (module docs' "an absent device reads
    /// all-ones" applies to every `Bdf` no slot claims, not just
    /// plausible-looking ones).
    fn find_mut(&mut self, bdf: Bdf) -> Option<&mut dyn PciDevice> {
        for slot in self.devices.iter_mut() {
            if slot.bdf == bdf {
                return Some(&mut *slot.device);
            }
        }
        None
    }
}

impl PciBackend for VirtualPciBus<'_> {
    fn config_read(&mut self, bdf: Bdf, offset: u16, width: AccessWidth) -> u32 {
        match self.find_mut(bdf) {
            Some(dev) => dev.config_read(offset, width),
            None => all_ones(width),
        }
    }

    fn config_write(&mut self, bdf: Bdf, offset: u16, width: AccessWidth, value: u32) {
        if let Some(dev) = self.find_mut(bdf) {
            dev.config_write(offset, width, value);
        }
    }

    fn mem_read(&mut self, addr: u64, width: AccessWidth) -> u32 {
        for slot in self.devices.iter_mut() {
            for bar in 0..6 {
                if let Some((base, len)) = slot.device.bar_window(bar) {
                    // `checked_sub` rather than computing `base + len`:
                    // this alone is what keeps a hostile `base` near
                    // `u64::MAX` from ever needing a sum that could
                    // overflow — there is no addition on this path at all.
                    if let Some(rel) = addr.checked_sub(base) {
                        if rel < len {
                            return slot.device.bar_read(bar, rel, width);
                        }
                    }
                }
            }
        }
        all_ones(width)
    }

    fn mem_write(&mut self, addr: u64, width: AccessWidth, value: u32) {
        for slot in self.devices.iter_mut() {
            for bar in 0..6 {
                if let Some((base, len)) = slot.device.bar_window(bar) {
                    if let Some(rel) = addr.checked_sub(base) {
                        if rel < len {
                            slot.device.bar_write(bar, rel, width, value);
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// One BAR slot's kind, as declared at device construction time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarKind {
    /// This BAR does not exist on this device — [`ConfigSpace`] keeps it
    /// hardwired to `0` and discards every write, the real-hardware
    /// response to probing a BAR a device never implemented.
    None,
    /// A 32-bit, non-prefetchable memory BAR of `size` bytes. `size` must
    /// be a power of two `>= 16` (enforced by `debug_assert!` — the
    /// smallest a real BAR's own address-decode granularity can be,
    /// [`ConfigSpace`]'s doc comment on sizing).
    Mem32 { size: u32 },
}

/// The identity fields [`ConfigSpace::new`] burns into a device's
/// read-only header — everything a real device's PCI header carries
/// *except* `COMMAND`/`STATUS`/BARs/the capabilities pointer, which
/// [`ConfigSpace`] manages itself.
#[derive(Clone, Copy, Debug)]
pub struct ConfigSpaceIdentity {
    pub vendor_id: u16,
    pub device_id: u16,
    pub revision: u8,
    /// Base class (offset `0x0B`) — e.g. `0x02` for network controllers.
    pub class_base: u8,
    /// Sub-class (offset `0x0A`).
    pub class_sub: u8,
    /// Programming interface (offset `0x09`).
    pub prog_if: u8,
    pub subsystem_vendor: u16,
    pub subsystem_id: u16,
    /// `INTx` pin this function uses (`1` = INTA, ... `4` = INTD), or `0`
    /// for "uses no legacy interrupt pin".
    pub interrupt_pin: u8,
}

/// `COMMAND` register bits [`ConfigSpace`] actually implements as
/// read/write — every other bit of `COMMAND`, and all of `STATUS`, is
/// read-only zero (`STATUS`'s capabilities-list bit excepted).
mod command_bits {
    pub const IO_ENABLE: u8 = 1 << 0;
    pub const MEMORY_ENABLE: u8 = 1 << 1;
    pub const BUS_MASTER: u8 = 1 << 2;
    pub const WRITABLE_MASK: u8 = IO_ENABLE | MEMORY_ENABLE | BUS_MASTER;
}

/// `STATUS` register bit: capabilities list present (offset `0x06`, bit
/// `0x10` — i.e. bit 4 of the low byte).
const STATUS_CAP_LIST: u8 = 0x10;

/// Config-space byte offsets this module treats specially — everything not
/// named here is either part of the fixed identity image (read-only after
/// construction) or genuinely unimplemented (reads `0` from the identity
/// image's zeroed default, per real config space's own "reserved reads as
/// zero" convention).
mod off {
    pub const VENDOR_ID: u16 = 0x00;
    pub const DEVICE_ID: u16 = 0x02;
    pub const COMMAND: u16 = 0x04;
    pub const STATUS: u16 = 0x06;
    pub const REVISION: u16 = 0x08;
    pub const PROG_IF: u16 = 0x09;
    pub const CLASS_SUB: u16 = 0x0A;
    pub const CLASS_BASE: u16 = 0x0B;
    pub const HEADER_TYPE: u16 = 0x0E;
    pub const BAR0: u16 = 0x10;
    pub const CAP_POINTER: u16 = 0x34;
    pub const SUBSYSTEM_VENDOR: u16 = 0x2C;
    pub const SUBSYSTEM_ID: u16 = 0x2E;
    pub const INTERRUPT_PIN: u16 = 0x3D;
}

/// Conventional config space is 256 bytes. [`ConfigSpace`] never
/// implements extended (4 KB PCIe) config space — see [`ConfigSpace`]'s
/// doc comment, "Extended config space".
const CONFIG_SPACE_LEN: usize = 256;

/// A reusable config-space model: 256 bytes of PCI type-0 header a
/// concrete device embeds, plus up to six BARs and one raw capability
/// chain.
///
/// # Extended config space
///
/// Offsets `>= 0x100` always read all-ones and discard writes. This
/// machine's first real PCI device arrives behind a conventional
/// (non-PCIe-native) shape — a pcie-to-pci-bridge-style presentation, per
/// ADR 0005 §10.1's AmigaPCI alignment and the Zorro III shim this file
/// backs — so PCIe's extended 4 KB config space simply is not there to
/// answer from; a driver that probes past `0x100` should see exactly what
/// it would see behind a real conventional bridge.
///
/// # BAR sizing, exactly
///
/// A [`BarKind::Mem32`] BAR holds its programmed base in the address bits
/// at and above `log2(size)`; the bits below that are hardwired to `0` —
/// not merely "not yet written", genuinely unwritable, on real hardware
/// and here alike. Writing all-ones and reading back therefore yields
/// `!(size - 1)` (every unwritable low bit reads `0`, matching real
/// hardware's fixed-zero response and, since `size >= 16`, also correctly
/// leaving the low 3-4 type bits at `0b000` — 32-bit, non-prefetchable —
/// with no separate step needed to force them). This is the BIOS/firmware
/// sizing probe stage 2's `pci.library` (and any real driver) performs to
/// learn a BAR's size before ever assigning it an address.
///
/// # Everything else is read-only
///
/// Per this file's brief: every byte of the 256-byte image is read-only
/// after construction *except* `COMMAND`'s three implemented bits and the
/// BAR registers. `STATUS` is included in that: this model does not
/// implement write-1-to-clear for any `STATUS` bit (there are none to
/// clear here — no error conditions this virtual topology can raise), so
/// writes to `STATUS` are simply discarded, documented rather than
/// silently absent.
pub struct ConfigSpace {
    /// The fixed identity image: vendor/device id, class/revision, header
    /// type, subsystem ids, interrupt pin, and (once installed) the raw
    /// capability chain bytes and the capability pointer at `0x34`.
    /// `COMMAND`/`STATUS`/BARs are *not* stored here — they have their own
    /// fields below, because unlike the identity bytes they have real
    /// read/write behaviour beyond "return what construction wrote".
    image: [u8; CONFIG_SPACE_LEN],
    command: u8,
    bar_kinds: [BarKind; 6],
    /// Each BAR's current raw register value: for [`BarKind::Mem32`], the
    /// masked base (module docs, "BAR sizing, exactly"); for
    /// [`BarKind::None`], always `0`.
    bar_regs: [u32; 6],
    caps_installed: bool,
}

impl ConfigSpace {
    /// Build a device's config space from its fixed identity and BAR
    /// declarations. No capability chain yet — call
    /// [`Self::install_capabilities`] afterwards if the device has one.
    pub fn new(identity: ConfigSpaceIdentity, bars: [BarKind; 6]) -> Self {
        for kind in bars {
            if let BarKind::Mem32 { size } = kind {
                debug_assert!(
                    size >= 16 && size.is_power_of_two(),
                    "BAR size must be a power of two >= 16"
                );
            }
        }

        let mut image = [0u8; CONFIG_SPACE_LEN];
        write_u16(&mut image, off::VENDOR_ID, identity.vendor_id);
        write_u16(&mut image, off::DEVICE_ID, identity.device_id);
        image[off::REVISION as usize] = identity.revision;
        image[off::PROG_IF as usize] = identity.prog_if;
        image[off::CLASS_SUB as usize] = identity.class_sub;
        image[off::CLASS_BASE as usize] = identity.class_base;
        image[off::HEADER_TYPE as usize] = 0x00; // type 0, single-function
        write_u16(&mut image, off::SUBSYSTEM_VENDOR, identity.subsystem_vendor);
        write_u16(&mut image, off::SUBSYSTEM_ID, identity.subsystem_id);
        image[off::INTERRUPT_PIN as usize] = identity.interrupt_pin;

        Self {
            image,
            command: 0,
            bar_kinds: bars,
            bar_regs: [0; 6],
            caps_installed: false,
        }
    }

    /// Install a pre-encoded raw capability chain: `bytes` is written
    /// verbatim starting at `first_offset` (the caller has already baked
    /// every `cap_next` pointer and field into it — this method is pure
    /// placement, per this file's brief: "caller provides pre-encoded
    /// bytes placed at chosen offsets"), the capabilities pointer
    /// (offset `0x34`) is set to `first_offset`, and `STATUS`'s
    /// capabilities-list bit is set. Panics (debug-only assert) if the
    /// chain would run past offset `0x100` — a construction-time device
    /// bug, never guest input.
    pub fn install_capabilities(&mut self, first_offset: u16, bytes: &[u8]) {
        let start = first_offset as usize;
        let end = start + bytes.len();
        debug_assert!(
            end <= 0x100,
            "capability chain must stay within conventional config space"
        );
        self.image[start..end].copy_from_slice(bytes);
        self.image[off::CAP_POINTER as usize] = first_offset as u8;
        self.caps_installed = true;
    }

    /// Whether memory-space decoding is currently enabled (`COMMAND` bit
    /// one) — the gate [`Self::bar_window`] applies, and what the module
    /// docs' "command-gating" note refers to.
    fn memory_space_enabled(&self) -> bool {
        self.command & command_bits::MEMORY_ENABLE != 0
    }

    /// The currently programmed absolute window of BAR `bar`, gated on
    /// memory-space enable — see [`PciDevice::bar_window`], which every
    /// concrete device in this file implements by delegating straight
    /// here.
    pub fn bar_window(&self, bar: usize) -> Option<(u64, u64)> {
        if !self.memory_space_enabled() {
            return None;
        }
        match self.bar_kinds.get(bar)? {
            BarKind::None => None,
            BarKind::Mem32 { size } => Some((self.bar_regs[bar] as u64, *size as u64)),
        }
    }

    /// Read `width` bytes at `offset`. See [`Self`]'s doc comment.
    pub fn read(&self, offset: u16, width: AccessWidth) -> u32 {
        let n = width.bytes();
        if offset as usize + n as usize > CONFIG_SPACE_LEN {
            // Covers both "past the 256-byte conventional space" and any
            // hostile, non-width-aligned offset that would straddle its
            // end — fail closed either way, never index out of bounds.
            return all_ones(width);
        }
        let mut value = 0u32;
        for lane in 0..n {
            value |= (self.read_byte(offset + lane as u16) as u32) << (8 * lane);
        }
        value
    }

    /// Write `value`'s low `width` bytes at `offset`. See [`Self`]'s doc
    /// comment.
    pub fn write(&mut self, offset: u16, width: AccessWidth, value: u32) {
        let n = width.bytes();
        if offset as usize + n as usize > CONFIG_SPACE_LEN {
            return;
        }
        for lane in 0..n {
            let byte = (value >> (8 * lane)) as u8;
            self.write_byte(offset + lane as u16, byte);
        }
    }

    fn read_byte(&self, offset: u16) -> u8 {
        match offset {
            off::COMMAND => self.command,
            o if o == off::COMMAND + 1 => 0, // COMMAND high byte: no implemented bits
            off::STATUS => {
                if self.caps_installed {
                    STATUS_CAP_LIST
                } else {
                    0
                }
            }
            o if o == off::STATUS + 1 => 0, // no STATUS bits above bit 7 modelled
            o if self.bar_index(o).is_some() => {
                let (bar, lane) = self.bar_index(o).unwrap();
                (self.bar_regs[bar] >> (8 * lane)) as u8
            }
            o => self.image[o as usize],
        }
    }

    fn write_byte(&mut self, offset: u16, byte: u8) {
        match offset {
            off::COMMAND => {
                self.command = (self.command & !command_bits::WRITABLE_MASK)
                    | (byte & command_bits::WRITABLE_MASK);
            }
            o if self.bar_index(o).is_some() => {
                let (bar, lane) = self.bar_index(o).unwrap();
                self.write_bar_byte(bar, lane, byte);
            }
            // Every other byte (identity fields, STATUS, the capability
            // pointer and chain, reserved space) is read-only, per this
            // struct's doc comment "Everything else is read-only".
            _ => {}
        }
    }

    /// If `offset` falls within one of the six 4-byte BAR registers
    /// (`0x10..0x28`), the BAR index and the byte lane (`0` = least
    /// significant) within it.
    fn bar_index(&self, offset: u16) -> Option<(usize, u32)> {
        if !(off::BAR0..off::BAR0 + 24).contains(&offset) {
            return None;
        }
        let rel = (offset - off::BAR0) as u32;
        Some((rel as usize / 4, rel % 4))
    }

    /// Merge `byte` into BAR `bar`'s raw register at byte lane `lane`,
    /// then reapply BAR write semantics on the merged word — see
    /// [`Self`]'s doc comment, "BAR sizing, exactly". A [`BarKind::None`]
    /// BAR discards every write and stays `0` forever, matching real
    /// hardware's response to a BAR a device never implemented.
    fn write_bar_byte(&mut self, bar: usize, lane: u32, byte: u8) {
        let BarKind::Mem32 { size } = self.bar_kinds[bar] else {
            return;
        };
        let mut bytes = self.bar_regs[bar].to_le_bytes();
        bytes[lane as usize] = byte;
        let merged = u32::from_le_bytes(bytes);
        let mask = !(size - 1);
        self.bar_regs[bar] = merged & mask;
    }
}

/// The all-ones value of exactly `width` bytes, high bits zero — real
/// master-abort semantics are "every bit of the access width reads back
/// set", not "every bit of a `u32` regardless of what width was asked
/// for" (module docs on [`PciBackend`], "the register VALUE ... W8/W16 in
/// the low bits, high bits zero").
fn all_ones(width: AccessWidth) -> u32 {
    match width {
        AccessWidth::W8 => 0xFF,
        AccessWidth::W16 => 0xFFFF,
        AccessWidth::W32 => 0xFFFF_FFFF,
    }
}

fn write_u16(image: &mut [u8; CONFIG_SPACE_LEN], offset: u16, value: u16) {
    let bytes = value.to_le_bytes();
    image[offset as usize] = bytes[0];
    image[offset as usize + 1] = bytes[1];
}

/// The virtual root complex's host bridge, `Bdf` `00:00.0` by convention
/// (the `Bdf` itself is [`VirtualSlot`]'s concern, not this device's —
/// this struct carries no address of its own).
///
/// Vendor `0x1B36`/device `0x0008`: deliberately the same identity QEMU's
/// own PCIe host bridge (`gpex`/`q35`) presents at `00:00.0`, so this
/// virtual topology's root looks exactly like what a real board crate's
/// ECAM will show at the same address — a driver written against one sees
/// the other. No BARs, no capabilities, no legacy interrupt pin: a host
/// bridge function has none of these on real hardware either.
pub struct HostBridge {
    config: ConfigSpace,
}

impl HostBridge {
    pub const VENDOR_ID: u16 = 0x1B36;
    pub const DEVICE_ID: u16 = 0x0008;
    /// Bridge device, host bridge sub-class (PCI class code `0x0600_00`).
    const CLASS_BASE: u8 = 0x06;
    const CLASS_SUB: u8 = 0x00;
    const PROG_IF: u8 = 0x00;

    pub fn new() -> Self {
        let config = ConfigSpace::new(
            ConfigSpaceIdentity {
                vendor_id: Self::VENDOR_ID,
                device_id: Self::DEVICE_ID,
                revision: 0,
                class_base: Self::CLASS_BASE,
                class_sub: Self::CLASS_SUB,
                prog_if: Self::PROG_IF,
                subsystem_vendor: 0,
                subsystem_id: 0,
                interrupt_pin: 0,
            },
            [BarKind::None; 6],
        );
        Self { config }
    }
}

impl Default for HostBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl PciDevice for HostBridge {
    fn config_read(&mut self, offset: u16, width: AccessWidth) -> u32 {
        self.config.read(offset, width)
    }

    fn config_write(&mut self, offset: u16, width: AccessWidth, value: u32) {
        self.config.write(offset, width, value);
    }

    fn bar_window(&self, bar: usize) -> Option<(u64, u64)> {
        self.config.bar_window(bar)
    }
}

/// Byte layout of the OASIS virtio 1.x `struct virtio_pci_cap` this file
/// hand-encodes for [`VirtioNetStub`] — kept as a private module of named
/// field-offset consts rather than a `#[repr(C)]` struct so the encoding
/// is visibly explicit (this crate has no reason to trust the host
/// compiler's layout for a byte format a guest driver parses field by
/// field), matching the "encode these bytes by hand" instruction this
/// increment was scoped under.
mod virtio_cap {
    /// `PCI_CAP_ID_VNDR` — every virtio structure capability uses this.
    pub const CAP_VNDR: u8 = 0x09;

    pub const CFG_TYPE_COMMON: u8 = 1;
    pub const CFG_TYPE_NOTIFY: u8 = 2;
    pub const CFG_TYPE_ISR: u8 = 3;
    pub const CFG_TYPE_DEVICE: u8 = 4;

    /// `sizeof(struct virtio_pci_cap)`: cap_vndr, cap_next, cap_len,
    /// cfg_type, bar, id, padding[2], offset (le32), length (le32).
    pub const BASIC_LEN: u8 = 16;
    /// `NOTIFY_CFG`'s capability additionally carries `notify_off_multiplier`
    /// (le32) after the basic fields.
    pub const NOTIFY_LEN: u8 = 20;

    /// Field byte offsets *within* one capability structure.
    pub const F_CAP_VNDR: usize = 0;
    pub const F_CAP_NEXT: usize = 1;
    pub const F_CAP_LEN: usize = 2;
    pub const F_CFG_TYPE: usize = 3;
    pub const F_BAR: usize = 4;
    pub const F_ID: usize = 5;
    // F_PADDING: usize = 6..8
    pub const F_OFFSET: usize = 8;
    pub const F_LENGTH: usize = 12;
    pub const F_NOTIFY_OFF_MULTIPLIER: usize = 16;
}

/// A config-space-complete, function-less modern virtio-net device (ADR
/// 0005 stage 1's client-to-be for stage 3's SANA-II driver).
///
/// This struct implements exactly the config space and capability chain a
/// real modern virtio-net device presents — enough for a driver's
/// enumeration, capability walk and BAR sizing to succeed precisely as it
/// would against real virtio-net. It implements **no queue, no register
/// behaviour behind BAR0's four capability windows**: [`Self::bar_read`]/
/// [`Self::bar_write`] are the trait's own default (all-ones/discard).
/// Giving this device a working ring is stage 3's SANA-II driver
/// increment's job, once `pci.library` (stage 2) exists to drive it
/// through; this increment only needs the config-space surface real
/// enumeration and capability-walk code exercises, per ADR 0005's own
/// sequencing ("ECAM reachability first, host side").
pub struct VirtioNetStub {
    config: ConfigSpace,
}

impl VirtioNetStub {
    pub const VENDOR_ID: u16 = 0x1AF4;
    /// Modern (non-transitional) virtio-net device ID.
    pub const DEVICE_ID: u16 = 0x1041;
    /// The virtio 1.x spec requires `>= 1` for a non-transitional device.
    const REVISION: u8 = 0x01;
    /// Network controller, ethernet (PCI class code `0x0200_00`).
    const CLASS_BASE: u8 = 0x02;
    const CLASS_SUB: u8 = 0x00;
    const PROG_IF: u8 = 0x00;
    const SUBSYSTEM_VENDOR: u16 = 0x1AF4;
    const SUBSYSTEM_ID: u16 = 0x0001;
    /// `INTA` — modern virtio permits MSI-X, but ADR 0005 defers it in
    /// favour of INTx-style routing for the driver's first increment
    /// (module docs' `PciBackend` reasoning doesn't apply here; this is
    /// the ADR's own driver-discipline choice, §10.1).
    const INTERRUPT_PIN: u8 = 1;

    /// BAR0: 16 KiB, enough to hold all four capability windows below with
    /// their declared lengths (`0x0000..0x4000`).
    pub const BAR0_SIZE: u32 = 16 * 1024;

    /// First byte of the capability chain — chosen the same way
    /// `rtgboard`'s register file leaves room before it: past the fixed
    /// header (which ends at `0x3F`), nothing else claims this space.
    const CAP_COMMON_OFFSET: u16 = 0x40;
    const CAP_NOTIFY_OFFSET: u16 = Self::CAP_COMMON_OFFSET + virtio_cap::BASIC_LEN as u16;
    const CAP_ISR_OFFSET: u16 = Self::CAP_NOTIFY_OFFSET + virtio_cap::NOTIFY_LEN as u16;
    const CAP_DEVICE_OFFSET: u16 = Self::CAP_ISR_OFFSET + virtio_cap::BASIC_LEN as u16;
    const CAPS_END: u16 = Self::CAP_DEVICE_OFFSET + virtio_cap::BASIC_LEN as u16;
    const CAPS_LEN: usize = (Self::CAPS_END - Self::CAP_COMMON_OFFSET) as usize;

    /// BAR0-relative windows for the four capability types (offset,
    /// length), per this file's brief.
    const COMMON_CFG_BAR_OFFSET: u32 = 0x0000;
    const COMMON_CFG_BAR_LEN: u32 = 0x1000;
    const NOTIFY_CFG_BAR_OFFSET: u32 = 0x1000;
    const NOTIFY_CFG_BAR_LEN: u32 = 0x1000;
    /// `notify_off_multiplier`: this device has a single queue notify
    /// address, so any nonzero multiplier is equally valid; `4` matches
    /// common real virtio-pci device layouts (one `le32` per queue slot).
    const NOTIFY_OFF_MULTIPLIER: u32 = 4;
    const ISR_CFG_BAR_OFFSET: u32 = 0x2000;
    const ISR_CFG_BAR_LEN: u32 = 0x1000;
    const DEVICE_CFG_BAR_OFFSET: u32 = 0x3000;
    const DEVICE_CFG_BAR_LEN: u32 = 0x1000;

    pub fn new() -> Self {
        let mut config = ConfigSpace::new(
            ConfigSpaceIdentity {
                vendor_id: Self::VENDOR_ID,
                device_id: Self::DEVICE_ID,
                revision: Self::REVISION,
                class_base: Self::CLASS_BASE,
                class_sub: Self::CLASS_SUB,
                prog_if: Self::PROG_IF,
                subsystem_vendor: Self::SUBSYSTEM_VENDOR,
                subsystem_id: Self::SUBSYSTEM_ID,
                interrupt_pin: Self::INTERRUPT_PIN,
            },
            [
                BarKind::Mem32 {
                    size: Self::BAR0_SIZE,
                },
                BarKind::None,
                BarKind::None,
                BarKind::None,
                BarKind::None,
                BarKind::None,
            ],
        );
        let caps = Self::build_capability_chain();
        config.install_capabilities(Self::CAP_COMMON_OFFSET, &caps);
        Self { config }
    }

    /// Hand-encode the four `virtio_pci_cap` structures at `0x40..0x84`
    /// (module docs, `virtio_cap`): little-endian throughout, `cap_next`
    /// chaining `COMMON_CFG -> NOTIFY_CFG -> ISR_CFG -> DEVICE_CFG -> 0`.
    fn build_capability_chain() -> [u8; Self::CAPS_LEN] {
        let mut buf = [0u8; Self::CAPS_LEN];
        Self::write_cap(
            &mut buf,
            Self::CAP_COMMON_OFFSET,
            Self::CAP_NOTIFY_OFFSET,
            virtio_cap::BASIC_LEN,
            virtio_cap::CFG_TYPE_COMMON,
            Self::COMMON_CFG_BAR_OFFSET,
            Self::COMMON_CFG_BAR_LEN,
            None,
        );
        Self::write_cap(
            &mut buf,
            Self::CAP_NOTIFY_OFFSET,
            Self::CAP_ISR_OFFSET,
            virtio_cap::NOTIFY_LEN,
            virtio_cap::CFG_TYPE_NOTIFY,
            Self::NOTIFY_CFG_BAR_OFFSET,
            Self::NOTIFY_CFG_BAR_LEN,
            Some(Self::NOTIFY_OFF_MULTIPLIER),
        );
        Self::write_cap(
            &mut buf,
            Self::CAP_ISR_OFFSET,
            Self::CAP_DEVICE_OFFSET,
            virtio_cap::BASIC_LEN,
            virtio_cap::CFG_TYPE_ISR,
            Self::ISR_CFG_BAR_OFFSET,
            Self::ISR_CFG_BAR_LEN,
            None,
        );
        Self::write_cap(
            &mut buf,
            Self::CAP_DEVICE_OFFSET,
            0, // last capability: cap_next = 0
            virtio_cap::BASIC_LEN,
            virtio_cap::CFG_TYPE_DEVICE,
            Self::DEVICE_CFG_BAR_OFFSET,
            Self::DEVICE_CFG_BAR_LEN,
            None,
        );
        buf
    }

    /// Encode one `virtio_pci_cap` (module docs, `virtio_cap`) into `buf`
    /// at `at` (an absolute config-space offset; `buf` itself starts at
    /// [`Self::CAP_COMMON_OFFSET`]), with `cap_next` as an absolute offset
    /// too (`0` terminates the chain, matching real PCI capability lists).
    #[allow(clippy::too_many_arguments)]
    fn write_cap(
        buf: &mut [u8],
        at: u16,
        cap_next: u16,
        cap_len: u8,
        cfg_type: u8,
        bar_offset: u32,
        bar_len: u32,
        notify_off_multiplier: Option<u32>,
    ) {
        let base = (at - Self::CAP_COMMON_OFFSET) as usize;
        buf[base + virtio_cap::F_CAP_VNDR] = virtio_cap::CAP_VNDR;
        buf[base + virtio_cap::F_CAP_NEXT] = cap_next as u8;
        buf[base + virtio_cap::F_CAP_LEN] = cap_len;
        buf[base + virtio_cap::F_CFG_TYPE] = cfg_type;
        buf[base + virtio_cap::F_BAR] = 0; // every capability window lives in BAR0
        buf[base + virtio_cap::F_ID] = 0;
        // Two padding bytes at F_ID+1..F_OFFSET are left zeroed.
        buf[base + virtio_cap::F_OFFSET..base + virtio_cap::F_OFFSET + 4]
            .copy_from_slice(&bar_offset.to_le_bytes());
        buf[base + virtio_cap::F_LENGTH..base + virtio_cap::F_LENGTH + 4]
            .copy_from_slice(&bar_len.to_le_bytes());
        if let Some(mult) = notify_off_multiplier {
            buf[base + virtio_cap::F_NOTIFY_OFF_MULTIPLIER
                ..base + virtio_cap::F_NOTIFY_OFF_MULTIPLIER + 4]
                .copy_from_slice(&mult.to_le_bytes());
        }
    }
}

impl Default for VirtioNetStub {
    fn default() -> Self {
        Self::new()
    }
}

impl PciDevice for VirtioNetStub {
    fn config_read(&mut self, offset: u16, width: AccessWidth) -> u32 {
        self.config.read(offset, width)
    }

    fn config_write(&mut self, offset: u16, width: AccessWidth, value: u32) {
        self.config.write(offset, width, value);
    }

    fn bar_window(&self, bar: usize) -> Option<(u64, u64)> {
        self.config.bar_window(bar)
    }
}

/// A small, honestly-named device that exists only to prove the BAR
/// aperture path end to end before any real device has function behind
/// it: one 4 KiB `Mem32` BAR whose reads return a deterministic,
/// offset-derived pattern and whose writes are simply recorded for a test
/// to inspect, rather than acted on.
pub struct TestMemDevice {
    config: ConfigSpace,
    last_write: Option<(u64, AccessWidth, u32)>,
}

impl TestMemDevice {
    /// A synthetic identity — not a real assigned PCI vendor, deliberately
    /// distinguishable from [`HostBridge`]/[`VirtioNetStub`] in bus
    /// enumeration.
    pub const VENDOR_ID: u16 = 0x1234;
    pub const DEVICE_ID: u16 = 0xBEEF;
    pub const BAR0_SIZE: u32 = 4096;

    pub fn new() -> Self {
        let config = ConfigSpace::new(
            ConfigSpaceIdentity {
                vendor_id: Self::VENDOR_ID,
                device_id: Self::DEVICE_ID,
                revision: 0,
                class_base: 0xFF, // "unclassified" / vendor-specific test device
                class_sub: 0x00,
                prog_if: 0x00,
                subsystem_vendor: 0,
                subsystem_id: 0,
                interrupt_pin: 0,
            },
            [
                BarKind::Mem32 {
                    size: Self::BAR0_SIZE,
                },
                BarKind::None,
                BarKind::None,
                BarKind::None,
                BarKind::None,
                BarKind::None,
            ],
        );
        Self {
            config,
            last_write: None,
        }
    }

    /// The most recent `(offset, width, value)` written through BAR0,
    /// or `None` if nothing has been written yet. Host-side test/debug
    /// accessor, not part of the guest-visible surface.
    pub fn last_write(&self) -> Option<(u64, AccessWidth, u32)> {
        self.last_write
    }

    /// The deterministic pattern [`Self::bar_read`] returns for a given
    /// `offset`/`width`: byte lane `i` is `offset.wrapping_add(i) as u8`,
    /// composed little-endian. Purely a fixed, offset-derived function —
    /// nothing here depends on device state, so the same offset always
    /// reads back the same value regardless of what has been written
    /// (writes are recorded, never applied to storage that reads would
    /// reflect — this device has no real backing store to write into).
    fn pattern(offset: u64, width: AccessWidth) -> u32 {
        let low = offset as u8;
        let mut value = 0u32;
        for lane in 0..width.bytes() {
            value |= (low.wrapping_add(lane as u8) as u32) << (8 * lane);
        }
        value
    }
}

impl Default for TestMemDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl PciDevice for TestMemDevice {
    fn config_read(&mut self, offset: u16, width: AccessWidth) -> u32 {
        self.config.read(offset, width)
    }

    fn config_write(&mut self, offset: u16, width: AccessWidth, value: u32) {
        self.config.write(offset, width, value);
    }

    fn bar_window(&self, bar: usize) -> Option<(u64, u64)> {
        self.config.bar_window(bar)
    }

    fn bar_read(&mut self, bar: usize, offset: u64, width: AccessWidth) -> u32 {
        if bar == 0 {
            Self::pattern(offset, width)
        } else {
            u32::MAX
        }
    }

    fn bar_write(&mut self, bar: usize, offset: u64, width: AccessWidth, value: u32) {
        if bar == 0 {
            self.last_write = Some((offset, width, value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal `PciDevice` used only to exercise [`VirtualPciBus`]'s
    /// `mem_read`/`mem_write` containment math with a hostile
    /// `bar_window` no real [`ConfigSpace`]-backed device could ever
    /// produce (a real BAR base tops out at `u32::MAX`) -- proving the
    /// routing math itself never wraps, independent of what any concrete
    /// device could realistically supply.
    struct HostileWindowDevice {
        window: Option<(u64, u64)>,
        hits: u32,
    }

    impl PciDevice for HostileWindowDevice {
        fn config_read(&mut self, _offset: u16, _width: AccessWidth) -> u32 {
            u32::MAX
        }
        fn config_write(&mut self, _offset: u16, _width: AccessWidth, _value: u32) {}
        fn bar_window(&self, bar: usize) -> Option<(u64, u64)> {
            if bar == 0 {
                self.window
            } else {
                None
            }
        }
        fn bar_read(&mut self, _bar: usize, _offset: u64, _width: AccessWidth) -> u32 {
            self.hits += 1;
            0x42
        }
    }

    fn width_of(bytes: u32) -> AccessWidth {
        AccessWidth::from_bytes(bytes as u8).unwrap()
    }

    // ---- AccessWidth -----------------------------------------------------

    #[test]
    fn access_width_bytes_and_from_bytes_round_trip() {
        assert_eq!(AccessWidth::W8.bytes(), 1);
        assert_eq!(AccessWidth::W16.bytes(), 2);
        assert_eq!(AccessWidth::W32.bytes(), 4);
        assert_eq!(AccessWidth::from_bytes(1), Some(AccessWidth::W8));
        assert_eq!(AccessWidth::from_bytes(2), Some(AccessWidth::W16));
        assert_eq!(AccessWidth::from_bytes(4), Some(AccessWidth::W32));
        assert_eq!(AccessWidth::from_bytes(3), None);
        assert_eq!(AccessWidth::from_bytes(0), None);
    }

    // ---- enumeration over VirtualPciBus -----------------------------------

    #[test]
    fn enumeration_reads_vendor_and_device_id_correctly_at_every_width() {
        let mut bridge = HostBridge::new();
        let mut net = VirtioNetStub::new();
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
        let mut bus = VirtualPciBus::new(&mut slots);

        let bridge_bdf = Bdf {
            bus: 0,
            device: 0,
            function: 0,
        };
        let net_bdf = Bdf {
            bus: 0,
            device: 1,
            function: 0,
        };

        // W32 at offset 0: device_id << 16 | vendor_id.
        let combined = bus.config_read(bridge_bdf, 0x00, AccessWidth::W32);
        assert_eq!(combined & 0xFFFF, HostBridge::VENDOR_ID as u32);
        assert_eq!(combined >> 16, HostBridge::DEVICE_ID as u32);

        // W16 vendor/device individually.
        assert_eq!(
            bus.config_read(bridge_bdf, 0x00, AccessWidth::W16),
            HostBridge::VENDOR_ID as u32
        );
        assert_eq!(
            bus.config_read(bridge_bdf, 0x02, AccessWidth::W16),
            HostBridge::DEVICE_ID as u32
        );

        // W8 low/high byte of vendor id.
        assert_eq!(
            bus.config_read(bridge_bdf, 0x00, AccessWidth::W8),
            (HostBridge::VENDOR_ID & 0xFF) as u32
        );
        assert_eq!(
            bus.config_read(bridge_bdf, 0x01, AccessWidth::W8),
            (HostBridge::VENDOR_ID >> 8) as u32
        );

        // Second device, all three widths again.
        assert_eq!(
            bus.config_read(net_bdf, 0x00, AccessWidth::W16),
            VirtioNetStub::VENDOR_ID as u32
        );
        assert_eq!(
            bus.config_read(net_bdf, 0x02, AccessWidth::W16),
            VirtioNetStub::DEVICE_ID as u32
        );
    }

    #[test]
    fn absent_slots_read_all_ones_at_every_width() {
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

        for bdf in [
            Bdf {
                bus: 0,
                device: 2,
                function: 0,
            },
            Bdf {
                bus: 1,
                device: 0,
                function: 0,
            },
            Bdf {
                bus: 0,
                device: 0,
                function: 1,
            },
        ] {
            for width in [AccessWidth::W8, AccessWidth::W16, AccessWidth::W32] {
                assert_eq!(
                    bus.config_read(bdf, 0x00, width),
                    match width {
                        AccessWidth::W8 => 0xFF,
                        AccessWidth::W16 => 0xFFFF,
                        AccessWidth::W32 => 0xFFFF_FFFF,
                    },
                    "absent bdf {bdf:?} width {width:?}"
                );
            }
        }

        // Writes to an absent device are simply discarded, not observable.
        bus.config_write(
            Bdf {
                bus: 0,
                device: 2,
                function: 0,
            },
            0x04,
            AccessWidth::W16,
            0xFFFF,
        );
    }

    // ---- class code / revision --------------------------------------------

    #[test]
    fn class_code_and_revision_readback_matches_packed_layout() {
        let mut net = VirtioNetStub::new();
        let value = net.config_read(0x08, AccessWidth::W32);
        let class = (VirtioNetStub::CLASS_BASE as u32) << 16
            | (VirtioNetStub::CLASS_SUB as u32) << 8
            | VirtioNetStub::PROG_IF as u32;
        assert_eq!(value, (class << 8) | VirtioNetStub::REVISION as u32);
    }

    // ---- BAR sizing --------------------------------------------------------

    #[test]
    fn virtio_net_bar0_sizing_probe_reports_exactly_16kib_masked() {
        let mut net = VirtioNetStub::new();
        net.config_write(0x10, AccessWidth::W32, 0xFFFF_FFFF);
        let readback = net.config_read(0x10, AccessWidth::W32);
        assert_eq!(readback, 0xFFFF_C000);
    }

    #[test]
    fn virtio_net_bar0_restores_a_base_with_low_bits_masked_off() {
        let mut net = VirtioNetStub::new();
        let mask = !(VirtioNetStub::BAR0_SIZE - 1);
        let candidate = 0x1234_5678u32;
        net.config_write(0x10, AccessWidth::W32, candidate);
        let readback = net.config_read(0x10, AccessWidth::W32);
        assert_eq!(readback, candidate & mask);
        assert_ne!(
            readback, candidate,
            "low bits below the BAR size must not be writable"
        );
    }

    #[test]
    fn unimplemented_bars_read_zero_and_stay_zero_after_an_all_ones_write() {
        let mut net = VirtioNetStub::new();
        for bar_offset in [0x14u16, 0x18, 0x1C, 0x20, 0x24] {
            assert_eq!(net.config_read(bar_offset, AccessWidth::W32), 0);
            net.config_write(bar_offset, AccessWidth::W32, 0xFFFF_FFFF);
            assert_eq!(net.config_read(bar_offset, AccessWidth::W32), 0);
        }
    }

    #[test]
    fn w16_and_w8_bar_writes_update_only_the_addressed_bytes() {
        let mut dev = TestMemDevice::new();
        let mask = !(TestMemDevice::BAR0_SIZE - 1);

        // High half-word of BAR0 first, low half stays zero.
        dev.config_write(0x10 + 2, AccessWidth::W16, 0x1234);
        assert_eq!(dev.config_read(0x10, AccessWidth::W32), 0x1234_0000 & mask);

        // Now poke a single byte of the low half; the high half must
        // survive untouched.
        dev.config_write(0x10, AccessWidth::W8, 0x56);
        assert_eq!(dev.config_read(0x10, AccessWidth::W32), 0x1234_0056 & mask);
    }

    // ---- COMMAND / STATUS ---------------------------------------------------

    #[test]
    fn command_register_only_accepts_its_three_documented_bits() {
        let mut net = VirtioNetStub::new();
        net.config_write(0x04, AccessWidth::W16, 0xFFFF);
        // Only IO/MEM/BUS_MASTER (bits 0-2) may be set; everything else,
        // including the whole high byte, reads back zero.
        assert_eq!(net.config_read(0x04, AccessWidth::W16), 0x0007);
    }

    #[test]
    fn status_capabilities_bit_is_set_on_the_stub_and_clear_on_the_bridge() {
        let mut net = VirtioNetStub::new();
        let mut bridge = HostBridge::new();
        assert_eq!(net.config_read(0x06, AccessWidth::W16) & 0x10, 0x10);
        assert_eq!(bridge.config_read(0x06, AccessWidth::W16) & 0x10, 0);
    }

    #[test]
    fn status_writes_are_discarded_no_write_one_to_clear_modelled() {
        let mut net = VirtioNetStub::new();
        let before = net.config_read(0x06, AccessWidth::W16);
        net.config_write(0x06, AccessWidth::W16, 0xFFFF);
        assert_eq!(net.config_read(0x06, AccessWidth::W16), before);
    }

    // ---- capability chain walk ----------------------------------------------

    #[test]
    fn capability_chain_walks_all_four_virtio_structures_correctly() {
        let mut net = VirtioNetStub::new();

        // Status bit set -> walk from the capabilities pointer.
        assert_eq!(net.config_read(0x06, AccessWidth::W16) & 0x10, 0x10);
        let first = net.config_read(0x34, AccessWidth::W8) as u16;
        assert_eq!(first, VirtioNetStub::CAP_COMMON_OFFSET);

        struct Expected {
            cfg_type: u8,
            len: u8,
            bar_offset: u32,
            bar_len: u32,
            notify_mult: Option<u32>,
        }
        let chain = [
            Expected {
                cfg_type: virtio_cap::CFG_TYPE_COMMON,
                len: virtio_cap::BASIC_LEN,
                bar_offset: 0x0000,
                bar_len: 0x1000,
                notify_mult: None,
            },
            Expected {
                cfg_type: virtio_cap::CFG_TYPE_NOTIFY,
                len: virtio_cap::NOTIFY_LEN,
                bar_offset: 0x1000,
                bar_len: 0x1000,
                notify_mult: Some(4),
            },
            Expected {
                cfg_type: virtio_cap::CFG_TYPE_ISR,
                len: virtio_cap::BASIC_LEN,
                bar_offset: 0x2000,
                bar_len: 0x1000,
                notify_mult: None,
            },
            Expected {
                cfg_type: virtio_cap::CFG_TYPE_DEVICE,
                len: virtio_cap::BASIC_LEN,
                bar_offset: 0x3000,
                bar_len: 0x1000,
                notify_mult: None,
            },
        ];

        let mut offset = first;
        for (i, expected) in chain.iter().enumerate() {
            let cap_vndr = net.config_read(offset, AccessWidth::W8);
            assert_eq!(cap_vndr, virtio_cap::CAP_VNDR as u32, "cap {i} vendor id");
            let cap_len = net.config_read(offset + 2, AccessWidth::W8);
            assert_eq!(cap_len, expected.len as u32, "cap {i} length");
            let cfg_type = net.config_read(offset + 3, AccessWidth::W8);
            assert_eq!(cfg_type, expected.cfg_type as u32, "cap {i} cfg_type");
            let bar = net.config_read(offset + 4, AccessWidth::W8);
            assert_eq!(bar, 0, "cap {i} bar index");
            let bar_offset = net.config_read(offset + 8, AccessWidth::W32);
            assert_eq!(bar_offset, expected.bar_offset, "cap {i} bar offset");
            let bar_len = net.config_read(offset + 12, AccessWidth::W32);
            assert_eq!(bar_len, expected.bar_len, "cap {i} bar length");
            if let Some(mult) = expected.notify_mult {
                let read_mult = net.config_read(offset + 16, AccessWidth::W32);
                assert_eq!(read_mult, mult, "cap {i} notify_off_multiplier");
            }

            let cap_next = net.config_read(offset + 1, AccessWidth::W8) as u16;
            if i + 1 < chain.len() {
                assert_ne!(cap_next, 0, "cap {i} must chain to the next capability");
                offset = cap_next;
            } else {
                assert_eq!(cap_next, 0, "the last capability terminates the chain");
            }
        }
    }

    // ---- mem routing --------------------------------------------------------

    #[test]
    fn mem_read_hits_the_device_with_the_correct_bar_relative_offset() {
        let mut dev = TestMemDevice::new();
        // Enable memory space.
        dev.config_write(0x04, AccessWidth::W8, 0x02);
        // Program BAR0's base (aligned to its 4 KiB size).
        let base: u32 = 0x2000_0000;
        dev.config_write(0x10, AccessWidth::W32, base);
        assert_eq!(dev.config_read(0x10, AccessWidth::W32), base);

        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 0,
                function: 0,
            },
            device: &mut dev,
        }];
        let mut bus = VirtualPciBus::new(&mut slots);

        let value = bus.mem_read(base as u64 + 0x10, AccessWidth::W32);
        assert_eq!(value, TestMemDevice::pattern(0x10, AccessWidth::W32));
    }

    #[test]
    fn mem_read_is_all_ones_when_memory_space_is_disabled() {
        let mut dev = TestMemDevice::new();
        // Program the BAR but never enable memory space.
        let base: u32 = 0x2000_0000;
        dev.config_write(0x10, AccessWidth::W32, base);

        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 0,
                function: 0,
            },
            device: &mut dev,
        }];
        let mut bus = VirtualPciBus::new(&mut slots);

        assert_eq!(bus.mem_read(base as u64, AccessWidth::W32), 0xFFFF_FFFF);
    }

    #[test]
    fn mem_read_at_an_unmapped_address_is_all_ones() {
        let mut dev = TestMemDevice::new();
        dev.config_write(0x04, AccessWidth::W8, 0x02);
        let base: u32 = 0x2000_0000;
        dev.config_write(0x10, AccessWidth::W32, base);

        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 0,
                function: 0,
            },
            device: &mut dev,
        }];
        let mut bus = VirtualPciBus::new(&mut slots);

        assert_eq!(
            bus.mem_read(
                base as u64 + TestMemDevice::BAR0_SIZE as u64,
                AccessWidth::W32
            ),
            0xFFFF_FFFF
        );
        assert_eq!(bus.mem_read(0xDEAD_BEEF, AccessWidth::W32), 0xFFFF_FFFF);
    }

    #[test]
    fn mem_write_is_recorded_at_the_correct_bar_relative_offset() {
        let mut dev = TestMemDevice::new();
        dev.config_write(0x04, AccessWidth::W8, 0x02);
        let base: u32 = 0x3000_0000;
        dev.config_write(0x10, AccessWidth::W32, base);

        {
            let mut slots = [VirtualSlot {
                bdf: Bdf {
                    bus: 0,
                    device: 0,
                    function: 0,
                },
                device: &mut dev,
            }];
            let mut bus = VirtualPciBus::new(&mut slots);
            bus.mem_write(base as u64 + 0x20, width_of(4), 0xCAFEBABE);
        }

        assert_eq!(dev.last_write(), Some((0x20, AccessWidth::W32, 0xCAFEBABE)));
    }

    #[test]
    fn a_base_near_u64_max_never_makes_containment_wrap() {
        // No real ConfigSpace BAR can produce this (32-bit register), but
        // VirtualPciBus's own routing math must still never treat a
        // low address as "inside" a window based near u64::MAX purely
        // through address-arithmetic overflow.
        let mut hostile = HostileWindowDevice {
            window: Some((u64::MAX - 10, 100)),
            hits: 0,
        };
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 0,
                function: 0,
            },
            device: &mut hostile,
        }];
        let mut bus = VirtualPciBus::new(&mut slots);

        // A low address must not spuriously match.
        assert_eq!(bus.mem_read(5, AccessWidth::W8), 0xFF);
        // The real window (base..base+100, staying within u64) does match.
        assert_eq!(bus.mem_read(u64::MAX - 5, AccessWidth::W8), 0x42);
    }

    // ---- extended config space / hostile offsets ----------------------------

    #[test]
    fn offsets_at_or_past_0x100_read_all_ones_and_discard_writes() {
        let mut net = VirtioNetStub::new();
        assert_eq!(net.config_read(0x100, AccessWidth::W32), 0xFFFF_FFFF);
        assert_eq!(net.config_read(0xFFF0, AccessWidth::W16), 0xFFFF);
        net.config_write(0x100, AccessWidth::W32, 0x1234_5678);
        assert_eq!(net.config_read(0x100, AccessWidth::W32), 0xFFFF_FFFF);
    }

    #[test]
    fn a_width_that_would_straddle_the_0x100_boundary_fails_closed() {
        let mut net = VirtioNetStub::new();
        // offset 0xFD + W32 spans 0xFD..0x101 -- past the 256-byte space.
        assert_eq!(net.config_read(0xFD, AccessWidth::W32), 0xFFFF_FFFF);
        net.config_write(0xFD, AccessWidth::W32, 0x1111_1111);
        // Confirm nothing was corrupted by peeking a byte inside range.
        assert_eq!(
            net.config_read(0xFD, AccessWidth::W8),
            0, // reserved, untouched byte
        );
    }
}
