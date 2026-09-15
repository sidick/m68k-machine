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

    /// Live `INTA`-`INTD` line levels, bits 0-3 = INTA..INTD, ORed
    /// together across every function behind this backend --
    /// `docs/pcibridge-protocol.md` §8's level-triggered INTx-to-INT2
    /// contract: a line reads asserted for exactly as long as some
    /// device holds it there, no latching, no edge to miss or
    /// acknowledge here (the guest-visible edge, if any, lives entirely
    /// in `pcibridge.rs`'s own `INTX_STATUS`/`INTX_ENABLE` register
    /// pair). Defaulted to "nothing asserted" so a real-ECAM
    /// implementation (this trait's other real backing, module docs)
    /// keeps compiling unchanged until its own board crate wires up
    /// whatever hardware INTx-status register its root complex exposes;
    /// only [`VirtualPciBus`] overrides this, by asking each attached
    /// [`PciDevice`] whether it is currently asserting and which pin it
    /// is wired to.
    fn intx_levels(&mut self) -> u32 {
        0
    }

    /// Advance whatever engine(s) this backend holds by one step against
    /// `mem` -- ADR 0005 stage 3's seam for virtio-net's ring processing
    /// (ring walks need a `&mut dyn GuestMemory` view of the guest's
    /// address space, which config/mem-space accesses alone never carry).
    /// Defaulted to a no-op so a real-ECAM implementation keeps compiling
    /// unchanged: real hardware processes its own rings with no host-side
    /// polling loop at all. Only [`VirtualPciBus`] overrides this, by
    /// forwarding to every attached [`PciDevice`]'s own
    /// [`PciDevice::tick`]. `MachineBus::tick` drives this exactly like
    /// `hostblk`/`pktport`'s own engines (module docs on those cards): the
    /// same lift-out-of-the-`Option`, call, put-back dance, because this
    /// backend is itself borrowed for the bus's whole lifetime and cannot
    /// be borrowed again while `self` (the `GuestMemory` view) is also
    /// borrowed.
    fn tick(&mut self, mem: &mut dyn crate::GuestMemory) {
        let _ = mem;
    }
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

    /// Whether this device is currently asserting its `INTx` line.
    /// Default: never -- every device in this file with no real function
    /// behind it ([`HostBridge`], [`VirtioNetStub`]) keeps this default;
    /// a device with genuine interrupt-generating logic (stage 3's
    /// virtio-net ISR, or this file's own [`IntxTestDevice`] diagnostic)
    /// overrides it. [`VirtualPciBus::intx_levels`] combines this with
    /// the device's own `Interrupt Pin` config byte (offset `0x3D`) to
    /// decide which of the four shared lines it ORs onto.
    fn intx_level(&self) -> bool {
        false
    }

    /// Advance this device's own engine by one step against `mem`, if it
    /// has one -- see [`PciBackend::tick`]'s doc comment. Defaulted to a
    /// no-op; every device in this file with no ring to walk
    /// ([`HostBridge`], [`TestMemDevice`], [`IntxTestDevice`]) keeps the
    /// default, and [`VirtioNetStub`] is the one device that overrides it.
    fn tick(&mut self, mem: &mut dyn crate::GuestMemory) {
        let _ = mem;
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

    /// OR every asserted device's line onto the bit its own `Interrupt
    /// Pin` config byte (offset `0x3D`, `1` = INTA .. `4` = INTD) names,
    /// `0` meaning "uses no legacy interrupt pin" and therefore never
    /// contributing a bit regardless of [`PciDevice::intx_level`]'s
    /// answer -- the routing half of the trait docs' "Live `INTA`-`INTD`
    /// line levels" contract, done here rather than by each device
    /// because which shared line a function's pin routes to is bus
    /// topology, not something a device knows about itself.
    fn intx_levels(&mut self) -> u32 {
        let mut levels = 0u32;
        for slot in self.devices.iter_mut() {
            if !slot.device.intx_level() {
                continue;
            }
            let pin = slot.device.config_read(off::INTERRUPT_PIN, AccessWidth::W8) as u8;
            if (1..=4).contains(&pin) {
                levels |= 1 << (pin - 1);
            }
        }
        levels
    }

    /// Forward to every attached device's own [`PciDevice::tick`] -- see
    /// [`PciBackend::tick`]'s doc comment. Devices with nothing to do
    /// (the default) simply ignore `mem`; only [`VirtioNetStub`] acts on
    /// it.
    fn tick(&mut self, mem: &mut dyn crate::GuestMemory) {
        for slot in self.devices.iter_mut() {
            slot.device.tick(mem);
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

/// A dense `0..3` index for [`AccessWidth`], for the rare case (here,
/// [`TestMemDevice`]'s per-width call counters) where a caller wants one
/// small array slot per width rather than a `match`.
fn width_index(width: AccessWidth) -> usize {
    match width {
        AccessWidth::W8 => 0,
        AccessWidth::W16 => 1,
        AccessWidth::W32 => 2,
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

/// What backs a [`VirtioNetStub`]'s frame traffic: transmit-out and
/// inject-in, borrowed the same way every other seam in this crate is
/// (module docs, "No allocator, caller supplies everything") — not
/// [`crate::pktport::PacketBackend`], deliberately: that trait's shape is
/// a synchronous request/response descriptor (one `execute` call per
/// guest doorbell, whole DOS-packet semantics baked into `args`), while a
/// NIC's traffic is two independent, asynchronous directions with no
/// request/response pairing at all -- a transmitted frame gets no reply,
/// and a received frame arrives with no guest action having asked for it.
/// Forcing that shape through `PacketBackend` would mean inventing a fake
/// request just to get a call site, in both directions. What *is* reused
/// is the seam's spirit: borrow, don't own, and let the frame data live
/// in caller-supplied storage rather than anything this crate allocates.
pub trait NetBackend {
    /// Deliver one transmitted Ethernet frame to the host side, the
    /// virtio-net header already stripped off by [`VirtioNetStub`].
    /// `frame` borrows scratch storage owned by the stub and is valid
    /// only for the duration of this call -- a backend that needs to keep
    /// the bytes must copy them out itself.
    fn transmit(&mut self, frame: &[u8]);

    /// If a frame is waiting to be delivered to the guest, copy it into
    /// `buf` (at least [`VirtioNetStub::MAX_ETH_PAYLOAD`] bytes) and
    /// return its length; `None` if nothing is pending. [`VirtioNetStub`]
    /// calls this only once it has already confirmed the guest has an
    /// available receive descriptor, so a backend never needs to queue a
    /// frame the guest cannot yet accept -- it can hold at most one ready
    /// frame of its own accord (or more, if it chooses; this trait places
    /// no limit on the backend's own queieing policy).
    fn poll_receive(&mut self, buf: &mut [u8]) -> Option<usize>;
}

/// A backend that transmits nothing anywhere and never has a frame to
/// deliver -- `machine-hosted`'s current placeholder until a later stage
/// wires real host network I/O behind [`NetBackend`] (this increment's
/// brief is explicit: host-side virtqueue processing and the seam trait,
/// not a host network path). Also handy for any test that only cares
/// about config-space/BAR/queue-negotiation behaviour and never drives a
/// real frame through the rings.
#[derive(Default)]
pub struct NullNetBackend;

impl NetBackend for NullNetBackend {
    fn transmit(&mut self, _frame: &[u8]) {}

    fn poll_receive(&mut self, _buf: &mut [u8]) -> Option<usize> {
        None
    }
}

/// Split-virtqueue descriptor flags (module docs, virtio 1.x §2.7.5).
mod desc_flags {
    /// This descriptor continues via `next`.
    pub const NEXT: u16 = 1;
    /// Device-writable (a receive buffer), rather than device-readable
    /// (a transmit buffer).
    pub const WRITE: u16 = 2;
    /// Indirect descriptor table -- not supported by this device; seeing
    /// this flag on a chain fails that chain closed (module docs on
    /// [`VirtioNetStub`]'s hostile-input posture).
    pub const INDIRECT: u16 = 4;
}

/// Device-status bits (module docs, virtio 1.x §2.1).
/// Recorded in full even though this device's own logic only ever tests
/// `FEATURES_OK`/`DRIVER_OK`/`DEVICE_NEEDS_RESET` by name (the rest are
/// simply passed through a `device_status` write verbatim) -- a reader
/// checking this device's behaviour against the spec's status-bit table
/// should find the whole table here, not just the bits this file happens
/// to branch on.
#[allow(dead_code)]
mod status_bits {
    pub const ACKNOWLEDGE: u8 = 1;
    pub const DRIVER: u8 = 2;
    pub const DRIVER_OK: u8 = 4;
    pub const FEATURES_OK: u8 = 8;
    pub const DEVICE_NEEDS_RESET: u8 = 64;
    pub const FAILED: u8 = 128;
}

/// Byte offsets within the `COMMON_CFG` window (module docs) -- the
/// modern virtio-pci `struct virtio_pci_common_cfg` layout (virtio 1.x
/// §4.1.4.3), hand-encoded the same way [`virtio_cap`] hand-encodes the
/// capability structures: this crate has no reason to trust the host
/// compiler's layout for a byte format a guest driver parses field by
/// field.
mod common_off {
    pub const DEVICE_FEATURE_SELECT: u32 = 0x00;
    pub const DEVICE_FEATURE: u32 = 0x04;
    pub const DRIVER_FEATURE_SELECT: u32 = 0x08;
    pub const DRIVER_FEATURE: u32 = 0x0C;
    pub const MSIX_CONFIG: u32 = 0x10;
    pub const NUM_QUEUES: u32 = 0x12;
    pub const DEVICE_STATUS: u32 = 0x14;
    pub const CONFIG_GENERATION: u32 = 0x15;
    pub const QUEUE_SELECT: u32 = 0x16;
    pub const QUEUE_SIZE: u32 = 0x18;
    pub const QUEUE_MSIX_VECTOR: u32 = 0x1A;
    pub const QUEUE_ENABLE: u32 = 0x1C;
    pub const QUEUE_NOTIFY_OFF: u32 = 0x1E;
    pub const QUEUE_DESC: u32 = 0x20;
    pub const QUEUE_DRIVER: u32 = 0x28;
    pub const QUEUE_DEVICE: u32 = 0x30;
    // Byte 0x38 and up, to `VirtioNetStub::COMMON_CFG_BAR_LEN`, is not
    // named here at all: it reads `0`/discards writes via
    // `VirtioNetStub::common_read_byte`/`common_write_byte`'s own
    // catch-all arm, the same "reserved reads as zero" posture
    // `ConfigSpace` takes.
}

/// One virtqueue's negotiated shape and ring position -- both queues
/// (module docs, `VirtioNetStub::queues`) use this identically; only
/// which queue index means rx vs tx differs.
#[derive(Clone, Copy)]
struct VirtQueueState {
    /// Driver-selected size, `1..=`[`VirtioNetStub::QUEUE_SIZE_MAX`].
    /// Never `0`: every place that could set it that way clamps instead
    /// (module docs) -- so every modulo against `size` elsewhere in this
    /// file is safe without an extra runtime check at the use site.
    size: u16,
    enabled: bool,
    /// Guest address of the descriptor table.
    desc_addr: u64,
    /// Guest address of the avail (driver-owned) ring.
    driver_addr: u64,
    /// Guest address of the used (device-owned) ring.
    device_addr: u64,
    /// The avail ring index this device has consumed up to -- this
    /// device's own shadow, compared against the guest's live
    /// `avail.idx` each poll; never read back out of guest memory.
    last_avail_idx: u16,
    /// The next slot this device will publish into the used ring, and
    /// the value written to `used.idx` after it does -- this device's
    /// own shadow of state it, not the driver, owns.
    used_idx: u16,
}

impl VirtQueueState {
    /// The state every queue starts in, both at construction and after a
    /// `device_status` reset-to-zero (module docs) -- `size` starts at
    /// the device's maximum, matching a real device's queue_size
    /// register before any driver has negotiated a smaller one down.
    const fn reset(max_size: u16) -> Self {
        Self {
            size: max_size,
            enabled: false,
            desc_addr: 0,
            driver_addr: 0,
            device_addr: 0,
            last_avail_idx: 0,
            used_idx: 0,
        }
    }
}

/// Read a little-endian `u16` from guest memory at `addr`, or `None` if
/// `addr` does not fit a 32-bit guest address or the span is not fully
/// mapped -- see [`GuestMemory::ram_slice`]'s own contract. Free function
/// (not a method) because both directions of ring-walking need it and
/// neither needs `self`.
fn gm_read_u16(mem: &dyn crate::GuestMemory, addr: u64) -> Option<u16> {
    let a = u32::try_from(addr).ok()?;
    let s = mem.ram_slice(a, 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

/// Write a little-endian `u16` into guest memory at `addr`, or `None` on
/// the same failure conditions as [`gm_read_u16`] -- never partially
/// written: [`GuestMemory::ram_slice_mut`] hands back the whole span or
/// nothing.
fn gm_write_u16(mem: &mut dyn crate::GuestMemory, addr: u64, value: u16) -> Option<()> {
    let a = u32::try_from(addr).ok()?;
    let s = mem.ram_slice_mut(a, 2)?;
    s.copy_from_slice(&value.to_le_bytes());
    Some(())
}

/// The `u32` equivalent of [`gm_write_u16`].
fn gm_write_u32(mem: &mut dyn crate::GuestMemory, addr: u64, value: u32) -> Option<()> {
    let a = u32::try_from(addr).ok()?;
    let s = mem.ram_slice_mut(a, 4)?;
    s.copy_from_slice(&value.to_le_bytes());
    Some(())
}

fn lane_u16(value: u16, lane: u32) -> u8 {
    value.to_le_bytes()[lane as usize]
}

fn merge_u16(value: u16, lane: u32, byte: u8) -> u16 {
    let mut bytes = value.to_le_bytes();
    bytes[lane as usize] = byte;
    u16::from_le_bytes(bytes)
}

fn lane_u32_of(value: u32, lane: u32) -> u8 {
    value.to_le_bytes()[lane as usize]
}

fn merge_u32_of(value: u32, lane: u32, byte: u8) -> u32 {
    let mut bytes = value.to_le_bytes();
    bytes[lane as usize] = byte;
    u32::from_le_bytes(bytes)
}

fn lane_u64(value: u64, lane: u32) -> u8 {
    value.to_le_bytes()[lane as usize]
}

fn merge_u64(value: u64, lane: u32, byte: u8) -> u64 {
    let mut bytes = value.to_le_bytes();
    bytes[lane as usize] = byte;
    u64::from_le_bytes(bytes)
}

/// Free-standing (not associated) copies of the handful of
/// [`VirtioNetStub`] constants used to size a fixed-length array, either
/// as a struct field type or an array-repeat expression inside
/// [`VirtioNetStub::new`]/[`VirtioNetStub::build_capability_chain`].
/// Needed only because `rustc` currently refuses `Self::CONST` in an
/// anonymous-constant position (an array length or repeat count) once
/// the type has *any* generic parameter, lifetimes included -- a known
/// limitation, not a semantic difference; [`VirtioNetStub`]'s own
/// associated consts of the same names/values remain the public,
/// documented API and are asserted equal to these in this module's own
/// tests.
const VNET_NUM_QUEUES: usize = 2;
const VNET_HDR_LEN: usize = 12;
const VNET_MAX_ETH_PAYLOAD: usize = 1514;
const VNET_MAX_FRAME_TOTAL: usize = VNET_HDR_LEN + VNET_MAX_ETH_PAYLOAD;
const VNET_CAPS_LEN: usize = virtio_cap::BASIC_LEN as usize * 3 + virtio_cap::NOTIFY_LEN as usize;

/// A config-space-complete, functional modern virtio-net device (ADR 0005
/// stage 3): the enumeration/capability/BAR-sizing surface stage 1 built,
/// now with real register behaviour and virtqueue processing behind
/// BAR0's four capability windows.
///
/// # BAR0 region map
///
/// The four capability windows (module docs on [`Self::COMMON_CFG_BAR_OFFSET`]
/// and siblings) exactly tile all `0x4000` bytes of BAR0 -- there is no
/// unmapped middle space to fall back to all-ones for, unlike a device
/// with room to grow:
///
/// | BAR0 offset | Region | Behaviour |
/// |---|---|---|
/// | `0x0000..0x1000` | `COMMON_CFG` | feature negotiation, status, per-queue registers (below) |
/// | `0x1000..0x2000` | `NOTIFY` | write-only, content ignored, any offset in range accepted; reads `0` |
/// | `0x2000..0x3000` | `ISR` | byte `0` is the read-to-clear ISR status; everything else reads `0` |
/// | `0x3000..0x4000` | `DEVICE_CFG` | bytes `0..6` are the MAC (below); everything else reads `0`, writes discarded |
///
/// A BAR other than `0`, or an offset beyond `0x4000` -- unreachable in
/// practice since [`ConfigSpace::bar_window`] already bounds every access
/// to BAR0's own `0x4000`-byte window -- keeps this trait's all-ones
/// default, same as stage 1.
///
/// # Feature negotiation and the reset dance
///
/// Offers exactly `VIRTIO_F_VERSION_1` (bit 32) and `VIRTIO_NET_F_MAC`
/// (bit 5); nothing else. `device_status` writes implement the spec's
/// reset semantics (`0` resets everything, including every queue back to
/// disabled/max-size) and this device's own fail-closed negotiation
/// guard: a driver that sets `FEATURES_OK` without having acked
/// `VIRTIO_F_VERSION_1` gets `FEATURES_OK` silently cleared right back
/// out of what it wrote (spec 3.1.1's own "re-read and check" contract:
/// the driver discovers the refusal by reading the register back, not
/// through any side channel), and a driver that sets `DRIVER_OK` while
/// `FEATURES_OK` is not (yet) set gets `DRIVER_OK` stripped back out and
/// [`status_bits::DEVICE_NEEDS_RESET`] set instead -- this platform's
/// house style is silent failure, but a stuck, narratable status
/// register beats a device that quietly pretends to be running.
///
/// # Virtqueues: two, split, bounded
///
/// `num_queues` is fixed at [`Self::NUM_QUEUES`] (`2`): queue `0` is rx,
/// queue `1` is tx, matching virtio-net's own convention. Every
/// guest-controlled ring field (descriptor/avail/used addresses,
/// lengths, `next` indices, ring positions) is walked with checked
/// arithmetic and validated against the negotiated queue size before
/// use; anything that fails validation aborts just that poll and sets
/// [`status_bits::DEVICE_NEEDS_RESET`] (module docs above) rather than
/// panicking, indexing out of bounds, or wrapping into a false match --
/// see [`Self::read_desc`], [`Self::process_tx`], [`Self::process_rx`].
/// A descriptor chain is bounded to at most `size` links (a chain whose
/// `next` fields cycle back on themselves, however "validly", cannot
/// loop forever).
///
/// Frames are staged through two fixed-size buffers
/// ([`Self::MAX_FRAME_TOTAL`]/[`Self::MAX_ETH_PAYLOAD`]) rather than
/// anything allocated -- this crate has no allocator (module docs at the
/// top of this file).
pub struct VirtioNetStub<'a> {
    config: ConfigSpace,
    backend: &'a mut dyn NetBackend,

    device_feature_select: u32,
    driver_feature_select: u32,
    /// Bits the driver has acked, masked down to [`Self::OFFERED_FEATURES`]
    /// at the moment they are written (module docs) -- so this is always
    /// a subset of what was offered, never something to re-check later.
    acked_features: u64,
    device_status: u8,
    queue_select: u16,
    queues: [VirtQueueState; VNET_NUM_QUEUES],

    /// ISR status byte: bit 0 set when a used buffer was added since the
    /// last read. Read-to-clear (module docs).
    isr_status: u8,

    /// Shared scratch for one frame at a time, header included -- reused
    /// by both directions since `tick` runs them one after the other,
    /// never concurrently (module docs, [`Self::MAX_FRAME_TOTAL`]).
    scratch: [u8; VNET_MAX_FRAME_TOTAL],
    /// Holds one frame handed back by [`NetBackend::poll_receive`],
    /// header-less (module docs, [`Self::MAX_ETH_PAYLOAD`]).
    rx_frame_buf: [u8; VNET_MAX_ETH_PAYLOAD],

    /// Host-side introspection counters, not registers -- the same
    /// "count it, don't invent a register for it yet" posture
    /// `pktport::Pktport`'s own drop counters take.
    tx_frames: u32,
    tx_dropped: u32,
    rx_frames: u32,
    rx_dropped: u32,
}

impl<'a> VirtioNetStub<'a> {
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

    /// rx is queue `0`, tx is queue `1` -- virtio-net's own convention,
    /// and what [`common_off::NUM_QUEUES`] reports.
    const NUM_QUEUES: u16 = 2;
    const QUEUE_RX: usize = 0;
    const QUEUE_TX: usize = 1;
    /// The largest queue size a driver may negotiate down from, and what
    /// `queue_size` reads back as before any driver has written a
    /// smaller value -- comfortably above any real SANA-II driver's
    /// working set, and small enough that this device's descriptor-chain
    /// loop bound ([`Self::read_desc`]) stays cheap.
    pub const QUEUE_SIZE_MAX: u16 = 256;
    /// A "no vector" MSI-X value -- this device has no MSI-X capability
    /// at all (module docs, `INTERRUPT_PIN`), so [`common_off::MSIX_CONFIG`]/
    /// [`common_off::QUEUE_MSIX_VECTOR`] always read this and ignore
    /// writes; a spec-compliant driver never touches them for a device
    /// with no MSI-X capability to begin with.
    const NO_VECTOR: u16 = 0xFFFF;

    /// `VIRTIO_F_VERSION_1`, bit 32 of the 64-bit feature space (virtio
    /// 1.x §6): required of every non-transitional device, and the one
    /// bit [`Self::write_device_status`]'s `FEATURES_OK` guard actually
    /// checks for.
    const F_VERSION_1: u64 = 1 << 32;
    /// `VIRTIO_NET_F_MAC`, bit 5 (virtio 1.x §5.1.3): the device config
    /// carries a MAC and the driver should use it rather than generating
    /// its own.
    const F_MAC: u64 = 1 << 5;
    /// Exactly the two feature bits this device offers -- nothing else,
    /// per this increment's brief ("Offer `VIRTIO_F_VERSION_1` and
    /// `VIRTIO_NET_F_MAC`"). Every driver-feature-write path masks
    /// against this, so a driver acking a bit this device never offered
    /// simply never sticks (module docs on [`Self::acked_features`]).
    const OFFERED_FEATURES: u64 = Self::F_VERSION_1 | Self::F_MAC;

    /// This device's locally-administered MAC: `02` in the first octet's
    /// low nibble marks it locally administered and unicast (IEEE 802-2014
    /// §8.2.2), never a real assigned OUI -- the deliberate choice every
    /// virtual NIC in this position makes. The remaining bytes spell nothing
    /// significant; chosen once and fixed so a guest driver sees the same
    /// address across every run.
    pub const MAC: [u8; 6] = [0x02, 0x6d, 0x36, 0x4b, 0x00, 0x01];

    /// The largest Ethernet payload (header/FCS excluded) this device
    /// stages at once -- the standard 1500-byte MTU plus the 14-byte
    /// Ethernet header (module docs, "Frames are staged through two
    /// fixed-size buffers"). No jumbo frames, no `VIRTIO_NET_F_MTU`
    /// offered, so a conformant driver never hands this device anything
    /// larger.
    pub const MAX_ETH_PAYLOAD: usize = 1514;
    /// Bytes in a modern (no legacy `num_buffers`-absent variant) virtio-net
    /// packet header (virtio 1.x §5.1.6.1, `struct virtio_net_hdr`, no
    /// merge-buffers layout since that feature is not offered here so the
    /// header stays the base 10 bytes... except this device advertises no
    /// `VIRTIO_NET_F_MRG_RXBUF` either, yet still writes `num_buffers` at
    /// bytes 10-11 as `1`: real devices that don't negotiate merged
    /// buffers still send the 10-byte legacy-shaped header. This device
    /// instead always uses the 12-byte modern layout with `num_buffers`
    /// fixed at `1` and expects the same from a transmitting driver,
    /// documented explicitly here since it is this device's own choice,
    /// not a spec-mandated one -- stage 3's driver-side guest code must
    /// match it exactly (see this file's own report to its caller).
    pub const VIRTIO_NET_HDR_LEN: usize = 12;
    /// The largest single frame (header included) this device stages --
    /// [`Self::VIRTIO_NET_HDR_LEN`] + [`Self::MAX_ETH_PAYLOAD`].
    pub const MAX_FRAME_TOTAL: usize = Self::VIRTIO_NET_HDR_LEN + Self::MAX_ETH_PAYLOAD;

    /// Build a device over a caller-owned [`NetBackend`] -- borrowed, not
    /// boxed, the same shape every other seam in this crate borrows its
    /// backing store (module docs, "No allocator, caller supplies
    /// everything").
    pub fn new(backend: &'a mut dyn NetBackend) -> Self {
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
        Self {
            config,
            backend,
            device_feature_select: 0,
            driver_feature_select: 0,
            acked_features: 0,
            device_status: 0,
            queue_select: 0,
            queues: [
                VirtQueueState::reset(Self::QUEUE_SIZE_MAX),
                VirtQueueState::reset(Self::QUEUE_SIZE_MAX),
            ],
            isr_status: 0,
            scratch: [0u8; VNET_MAX_FRAME_TOTAL],
            rx_frame_buf: [0u8; VNET_MAX_ETH_PAYLOAD],
            tx_frames: 0,
            tx_dropped: 0,
            rx_frames: 0,
            rx_dropped: 0,
        }
    }

    /// Hand-encode the four `virtio_pci_cap` structures at `0x40..0x84`
    /// (module docs, `virtio_cap`): little-endian throughout, `cap_next`
    /// chaining `COMMON_CFG -> NOTIFY_CFG -> ISR_CFG -> DEVICE_CFG -> 0`.
    fn build_capability_chain() -> [u8; VNET_CAPS_LEN] {
        let mut buf = [0u8; VNET_CAPS_LEN];
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

    /// Whether this device is currently in a state where a driver's next
    /// action must be a `device_status` reset -- [`Self::tick`] refuses
    /// to touch the rings while this is true, the same "stop, don't
    /// pretend" posture the register write path already takes.
    fn needs_reset(&self) -> bool {
        self.device_status & status_bits::DEVICE_NEEDS_RESET != 0
    }

    /// Fail this poll closed: the narratable posture this file's brief
    /// asks for, in place of this platform's usual silent-failure norm
    /// (module docs on [`Self`]). Idempotent -- setting the bit twice is
    /// harmless.
    fn fail_closed(&mut self) {
        self.device_status |= status_bits::DEVICE_NEEDS_RESET;
    }

    /// Return every piece of negotiable state to its power-on default --
    /// `device_status` write of `0` (virtio 1.x §2.1: "the driver...
    /// should re-initialize the device") and this struct's own
    /// construction-time state alike.
    fn reset(&mut self) {
        self.device_feature_select = 0;
        self.driver_feature_select = 0;
        self.acked_features = 0;
        self.device_status = 0;
        self.queue_select = 0;
        self.queues = [
            VirtQueueState::reset(Self::QUEUE_SIZE_MAX),
            VirtQueueState::reset(Self::QUEUE_SIZE_MAX),
        ];
        self.isr_status = 0;
        // Frame counters are host-side introspection, not guest-visible
        // state -- deliberately not reset, the same
        // `pktport::Pktport::doorbell_overflow` posture.
    }

    /// Queue `idx`'s size, clamped to `1..=QUEUE_SIZE_MAX` -- the value
    /// every ring-math use site sees, and what a `queue_size` register
    /// read reports, even if the raw stored value is momentarily `0` or
    /// out of range mid-write (module docs on [`Self::common_write_byte`]'s
    /// `QUEUE_SIZE` arm). Never `0`, so every modulo against it
    /// elsewhere in this file is safe.
    fn effective_queue_size(&self, idx: usize) -> u16 {
        self.queues[idx].size.clamp(1, Self::QUEUE_SIZE_MAX)
    }

    /// The currently `queue_select`-ed queue's index, or `None` if the
    /// driver has selected past [`Self::NUM_QUEUES`] -- every
    /// queue-specific register read/write goes through this and treats
    /// `None` as "reads 0, discards writes", the same fail-closed
    /// posture an out-of-range `Bdf` gets elsewhere in this file.
    fn selected_queue(&self) -> Option<usize> {
        let idx = self.queue_select as usize;
        (idx < Self::NUM_QUEUES as usize).then_some(idx)
    }

    /// The 32-bit window of [`Self::OFFERED_FEATURES`] named by
    /// `device_feature_select`, or `0` for any select value beyond `0`/`1`
    /// -- there are no more feature bits to report past bit 63, and this
    /// device only ever offers bits within the first two 32-bit windows
    /// anyway.
    fn device_feature_window(&self) -> u32 {
        match self.device_feature_select {
            0 => Self::OFFERED_FEATURES as u32,
            1 => (Self::OFFERED_FEATURES >> 32) as u32,
            _ => 0,
        }
    }

    /// The 32-bit window of [`Self::acked_features`] named by
    /// `driver_feature_select`, the read-back half of feature negotiation.
    fn driver_feature_window(&self) -> u32 {
        match self.driver_feature_select {
            0 => self.acked_features as u32,
            1 => (self.acked_features >> 32) as u32,
            _ => 0,
        }
    }

    /// Merge one byte into `driver_feature`'s currently-selected 32-bit
    /// window of [`Self::acked_features`], masking the result against
    /// [`Self::OFFERED_FEATURES`] so a driver acking a bit this device
    /// never offered simply never sticks (module docs on
    /// [`Self::acked_features`]). A no-op once `FEATURES_OK` is set
    /// (virtio 1.x: feature negotiation is over at that point) or while
    /// `driver_feature_select` names a window past bit 63.
    fn merge_driver_feature_byte(&mut self, lane: u32, byte: u8) {
        if self.device_status & status_bits::FEATURES_OK != 0 {
            return;
        }
        let sel = self.driver_feature_select;
        if sel > 1 {
            return;
        }
        let shift = 32 * sel as u64;
        let window = (self.acked_features >> shift) as u32;
        let merged = merge_u32_of(window, lane, byte);
        let mask = 0xFFFF_FFFFu64 << shift;
        self.acked_features = (self.acked_features & !mask)
            | (((merged as u64) << shift) & Self::OFFERED_FEATURES & mask);
    }

    /// Apply a `device_status` write: reset-to-zero (module docs,
    /// [`Self::reset`]), or the negotiation guard described in [`Self`]'s
    /// own doc comment ("Feature negotiation and the reset dance").
    fn write_device_status(&mut self, value: u8) {
        if value == 0 {
            self.reset();
            return;
        }
        let mut new_status = value;

        let requesting_features_ok = new_status & status_bits::FEATURES_OK != 0;
        let already_features_ok = self.device_status & status_bits::FEATURES_OK != 0;
        if requesting_features_ok
            && !already_features_ok
            && self.acked_features & Self::F_VERSION_1 == 0
        {
            // Refuse: strip FEATURES_OK back out. The driver discovers
            // this by reading device_status back, exactly as spec 3.1.1
            // describes -- no separate error channel invented here.
            new_status &= !status_bits::FEATURES_OK;
        }

        if new_status & status_bits::DRIVER_OK != 0 && new_status & status_bits::FEATURES_OK == 0 {
            // Refuse DRIVER_OK without FEATURES_OK: fail closed with a
            // narratable, stuck status register rather than silently
            // pretending to run (module docs, [`Self`]).
            new_status = (new_status & !status_bits::DRIVER_OK) | status_bits::DEVICE_NEEDS_RESET;
        }

        self.device_status = new_status;
    }

    /// Read one byte of the `COMMON_CFG` window at `rel` (module docs,
    /// [`common_off`]). Reserved/out-of-range bytes read `0`.
    fn common_read_byte(&self, rel: u32) -> u8 {
        match rel {
            common_off::DEVICE_FEATURE_SELECT..=3 => lane_u32_of(
                self.device_feature_select,
                rel - common_off::DEVICE_FEATURE_SELECT,
            ),
            common_off::DEVICE_FEATURE..=7 => lane_u32_of(
                self.device_feature_window(),
                rel - common_off::DEVICE_FEATURE,
            ),
            common_off::DRIVER_FEATURE_SELECT..=11 => lane_u32_of(
                self.driver_feature_select,
                rel - common_off::DRIVER_FEATURE_SELECT,
            ),
            common_off::DRIVER_FEATURE..=15 => lane_u32_of(
                self.driver_feature_window(),
                rel - common_off::DRIVER_FEATURE,
            ),
            common_off::MSIX_CONFIG..=17 => {
                lane_u16(Self::NO_VECTOR, rel - common_off::MSIX_CONFIG)
            }
            common_off::NUM_QUEUES..=19 => lane_u16(Self::NUM_QUEUES, rel - common_off::NUM_QUEUES),
            common_off::DEVICE_STATUS => self.device_status,
            common_off::CONFIG_GENERATION => 0, // device config never changes post-construction
            common_off::QUEUE_SELECT..=23 => {
                lane_u16(self.queue_select, rel - common_off::QUEUE_SELECT)
            }
            common_off::QUEUE_SIZE..=25 => {
                let size = self
                    .selected_queue()
                    .map_or(0, |i| self.effective_queue_size(i));
                lane_u16(size, rel - common_off::QUEUE_SIZE)
            }
            common_off::QUEUE_MSIX_VECTOR..=27 => {
                lane_u16(Self::NO_VECTOR, rel - common_off::QUEUE_MSIX_VECTOR)
            }
            common_off::QUEUE_ENABLE..=29 => {
                let enable = self
                    .selected_queue()
                    .is_some_and(|i| self.queues[i].enabled);
                lane_u16(enable as u16, rel - common_off::QUEUE_ENABLE)
            }
            common_off::QUEUE_NOTIFY_OFF..=31 => {
                // One `notify_off_multiplier`-sized slot per queue index
                // -- see this device's report on notify semantics.
                let off = self.selected_queue().map_or(0, |i| i as u16);
                lane_u16(off, rel - common_off::QUEUE_NOTIFY_OFF)
            }
            common_off::QUEUE_DESC..=39 => {
                let addr = self
                    .selected_queue()
                    .map_or(0, |i| self.queues[i].desc_addr);
                lane_u64(addr, rel - common_off::QUEUE_DESC)
            }
            common_off::QUEUE_DRIVER..=47 => {
                let addr = self
                    .selected_queue()
                    .map_or(0, |i| self.queues[i].driver_addr);
                lane_u64(addr, rel - common_off::QUEUE_DRIVER)
            }
            common_off::QUEUE_DEVICE..=55 => {
                let addr = self
                    .selected_queue()
                    .map_or(0, |i| self.queues[i].device_addr);
                lane_u64(addr, rel - common_off::QUEUE_DEVICE)
            }
            _ => 0,
        }
    }

    /// Write one byte of the `COMMON_CFG` window at `rel`, same contract
    /// as [`Self::common_read_byte`]. Read-only fields
    /// (`device_feature`, `num_queues`, `config_generation`,
    /// `queue_notify_off`) and out-of-range bytes discard the write.
    fn common_write_byte(&mut self, rel: u32, byte: u8) {
        match rel {
            common_off::DEVICE_FEATURE_SELECT..=3 => {
                self.device_feature_select = merge_u32_of(
                    self.device_feature_select,
                    rel - common_off::DEVICE_FEATURE_SELECT,
                    byte,
                );
            }
            common_off::DRIVER_FEATURE_SELECT..=11 => {
                self.driver_feature_select = merge_u32_of(
                    self.driver_feature_select,
                    rel - common_off::DRIVER_FEATURE_SELECT,
                    byte,
                );
            }
            common_off::DRIVER_FEATURE..=15 => {
                self.merge_driver_feature_byte(rel - common_off::DRIVER_FEATURE, byte);
            }
            common_off::MSIX_CONFIG..=17 => {} // no MSI-X capability; ignored
            common_off::DEVICE_STATUS => self.write_device_status(byte),
            common_off::QUEUE_SELECT..=23 => {
                self.queue_select =
                    merge_u16(self.queue_select, rel - common_off::QUEUE_SELECT, byte);
            }
            common_off::QUEUE_SIZE..=25 => {
                if let Some(i) = self.selected_queue() {
                    if !self.queues[i].enabled {
                        // Not clamped here: a 2-byte write merges one
                        // byte at a time (this function's own contract),
                        // and clamping mid-merge would corrupt the
                        // still-incomplete value the *other* lane is
                        // about to land on top of. Clamped instead at
                        // every point this value is actually used --
                        // [`Self::effective_queue_size`] -- so an
                        // in-flight two-byte write is never observed
                        // half-clamped.
                        self.queues[i].size =
                            merge_u16(self.queues[i].size, rel - common_off::QUEUE_SIZE, byte);
                    }
                }
            }
            common_off::QUEUE_MSIX_VECTOR..=27 => {} // no MSI-X capability; ignored
            common_off::QUEUE_ENABLE..=29 => {
                // Only the low byte (bit 0) is defined; only 0 -> 1 is a
                // legal transition (virtio 1.x §4.1.4.3.2: a driver must
                // not clear queue_enable except via a full device reset).
                if rel == common_off::QUEUE_ENABLE {
                    if let Some(i) = self.selected_queue() {
                        if byte & 1 != 0 && !self.queues[i].enabled {
                            self.queues[i].enabled = true;
                            self.queues[i].last_avail_idx = 0;
                            self.queues[i].used_idx = 0;
                        }
                    }
                }
            }
            common_off::QUEUE_DESC..=39 => {
                if let Some(i) = self.selected_queue() {
                    if !self.queues[i].enabled {
                        self.queues[i].desc_addr =
                            merge_u64(self.queues[i].desc_addr, rel - common_off::QUEUE_DESC, byte);
                    }
                }
            }
            common_off::QUEUE_DRIVER..=47 => {
                if let Some(i) = self.selected_queue() {
                    if !self.queues[i].enabled {
                        self.queues[i].driver_addr = merge_u64(
                            self.queues[i].driver_addr,
                            rel - common_off::QUEUE_DRIVER,
                            byte,
                        );
                    }
                }
            }
            common_off::QUEUE_DEVICE..=55 => {
                if let Some(i) = self.selected_queue() {
                    if !self.queues[i].enabled {
                        self.queues[i].device_addr = merge_u64(
                            self.queues[i].device_addr,
                            rel - common_off::QUEUE_DEVICE,
                            byte,
                        );
                    }
                }
            }
            _ => {} // includes every read-only field: DEVICE_FEATURE,
                    // NUM_QUEUES, CONFIG_GENERATION, QUEUE_NOTIFY_OFF
        }
    }

    /// Compose a `width`-byte read of the `COMMON_CFG` window at `rel`,
    /// the same byte-lane composition [`ConfigSpace::read`] uses.
    fn common_cfg_read(&self, rel: u32, width: AccessWidth) -> u32 {
        let mut value = 0u32;
        for lane in 0..width.bytes() {
            value |= (self.common_read_byte(rel + lane) as u32) << (8 * lane);
        }
        value
    }

    /// The `COMMON_CFG` write equivalent of [`Self::common_cfg_read`].
    fn common_cfg_write(&mut self, rel: u32, width: AccessWidth, value: u32) {
        for lane in 0..width.bytes() {
            let byte = (value >> (8 * lane)) as u8;
            self.common_write_byte(rel + lane, byte);
        }
    }

    /// Mark a used buffer added: ISR bit 0 set, which is also exactly
    /// [`PciDevice::intx_level`]'s condition -- see this device's doc
    /// comment on `INTx`.
    fn raise_used_irq(&mut self) {
        self.isr_status |= 0x1;
    }

    /// Read one descriptor-table entry (virtio 1.x §2.7.5, 16 bytes:
    /// `addr:u64, len:u32, flags:u16, next:u16`) at table `desc_addr`,
    /// index `idx`. `None` on anything hostile: `idx >= size` (never
    /// masked/modulo'd -- a descriptor *index* is not a ring position),
    /// the entry's own address arithmetic overflowing, or the entry's
    /// 16 bytes not lying entirely within mapped guest RAM.
    fn read_desc(
        mem: &dyn crate::GuestMemory,
        desc_addr: u64,
        size: u16,
        idx: u16,
    ) -> Option<(u64, u32, u16, u16)> {
        if idx >= size {
            return None;
        }
        let entry_addr = desc_addr.checked_add(16u64.checked_mul(idx as u64)?)?;
        let a = u32::try_from(entry_addr).ok()?;
        let s = mem.ram_slice(a, 16)?;
        let addr = u64::from_le_bytes(s[0..8].try_into().ok()?);
        let len = u32::from_le_bytes(s[8..12].try_into().ok()?);
        let flags = u16::from_le_bytes(s[12..14].try_into().ok()?);
        let next = u16::from_le_bytes(s[14..16].try_into().ok()?);
        Some((addr, len, flags, next))
    }

    /// The guest address of avail ring position `pos` (module docs on
    /// [`VirtQueueState::last_avail_idx`]) -- `driver_addr + 4 + 2*pos`,
    /// checked throughout since `driver_addr` is guest-controlled.
    fn avail_ring_slot_addr(driver_addr: u64, pos: u16) -> Option<u64> {
        driver_addr
            .checked_add(4)?
            .checked_add(2u64.checked_mul(pos as u64)?)
    }

    /// The guest address of used ring element `pos` -- `device_addr + 4 +
    /// 8*pos`, same checked-arithmetic posture as
    /// [`Self::avail_ring_slot_addr`].
    fn used_ring_elem_addr(device_addr: u64, pos: u16) -> Option<u64> {
        device_addr
            .checked_add(4)?
            .checked_add(8u64.checked_mul(pos as u64)?)
    }

    /// Publish one used-buffer completion for queue `qidx`: the used
    /// element (`id`, `len`) at this device's own shadow `used_idx`, then
    /// bump `used.idx` in guest memory and raise the ISR/`INTx`. `false`
    /// (never a panic) on any hostile `device_addr`/arithmetic failure --
    /// the caller treats that exactly like every other ring failure
    /// (module docs, [`Self::fail_closed`]).
    fn complete_used(
        &mut self,
        mem: &mut dyn crate::GuestMemory,
        qidx: usize,
        desc_id: u16,
        len: u32,
    ) -> bool {
        let (device_addr, used_idx) = {
            let q = &self.queues[qidx];
            (q.device_addr, q.used_idx)
        };
        let size = self.effective_queue_size(qidx);
        let slot = used_idx % size;
        let Some(elem_addr) = Self::used_ring_elem_addr(device_addr, slot) else {
            return false;
        };
        if gm_write_u32(mem, elem_addr, desc_id as u32).is_none() {
            return false;
        }
        if gm_write_u32(mem, elem_addr.wrapping_add(4), len).is_none() {
            return false;
        }
        let new_used_idx = used_idx.wrapping_add(1);
        if gm_write_u16(mem, device_addr.wrapping_add(2), new_used_idx).is_none() {
            return false;
        }
        self.queues[qidx].used_idx = new_used_idx;
        self.raise_used_irq();
        true
    }

    /// Walk a device-readable (tx) descriptor chain starting at `head`,
    /// copying every descriptor's bytes into [`Self::scratch`] in order.
    /// `None` on: a descriptor flagged device-writable or indirect (this
    /// device supports neither on the tx side), the accumulated length
    /// exceeding [`Self::MAX_FRAME_TOTAL`], or anything
    /// [`Self::read_desc`] itself already fails on. The chain is bounded
    /// to at most `size` links, so a `next` cycle cannot loop forever
    /// even though every individual link it visits is individually valid
    /// (module docs on [`Self`], "Virtqueues").
    fn copy_chain_out(
        &mut self,
        mem: &dyn crate::GuestMemory,
        desc_addr: u64,
        size: u16,
        head: u16,
    ) -> Option<usize> {
        let mut total = 0usize;
        let mut idx = head;
        for _ in 0..size {
            let (addr, len, flags, next) = Self::read_desc(mem, desc_addr, size, idx)?;
            if flags & (desc_flags::WRITE | desc_flags::INDIRECT) != 0 {
                return None;
            }
            let a = u32::try_from(addr).ok()?;
            let l = len as usize;
            let new_total = total.checked_add(l)?;
            if new_total > self.scratch.len() {
                return None;
            }
            let src = mem.ram_slice(a, len)?;
            self.scratch[total..new_total].copy_from_slice(src);
            total = new_total;
            if flags & desc_flags::NEXT == 0 {
                return Some(total);
            }
            idx = next;
        }
        // Exhausted the bound without terminating -- a cycle. Fail
        // closed rather than accept a chain longer than the queue itself
        // could legitimately describe.
        None
    }

    /// Advance the tx (queue [`Self::QUEUE_TX`]) engine: for every newly
    /// available descriptor since the last poll, extract the frame
    /// (module docs, [`Self::copy_chain_out`]), strip the
    /// [`Self::VIRTIO_NET_HDR_LEN`]-byte header, hand the remainder to
    /// [`NetBackend::transmit`], and publish a used-buffer completion.
    /// Stops (without panicking) at the first hostile ring content,
    /// setting [`status_bits::DEVICE_NEEDS_RESET`] (module docs,
    /// [`Self::fail_closed`]).
    fn process_tx(&mut self, mem: &mut dyn crate::GuestMemory) {
        if self.needs_reset() || !self.queues[Self::QUEUE_TX].enabled {
            return;
        }
        loop {
            let (driver_addr, desc_addr, last_avail) = {
                let q = &self.queues[Self::QUEUE_TX];
                (q.driver_addr, q.desc_addr, q.last_avail_idx)
            };
            let size = self.effective_queue_size(Self::QUEUE_TX);
            let Some(avail_idx) = gm_read_u16(mem, driver_addr.wrapping_add(2)) else {
                self.fail_closed();
                return;
            };
            if avail_idx == last_avail {
                return;
            }
            let Some(slot_addr) = Self::avail_ring_slot_addr(driver_addr, last_avail % size) else {
                self.fail_closed();
                return;
            };
            let Some(desc_head) = gm_read_u16(mem, slot_addr) else {
                self.fail_closed();
                return;
            };

            let outcome = self.copy_chain_out(mem, desc_addr, size, desc_head);
            let completed = match outcome {
                Some(len) if len >= Self::VIRTIO_NET_HDR_LEN => {
                    self.backend
                        .transmit(&self.scratch[Self::VIRTIO_NET_HDR_LEN..len]);
                    self.tx_frames = self.tx_frames.wrapping_add(1);
                    self.complete_used(mem, Self::QUEUE_TX, desc_head, len as u32)
                }
                _ => {
                    self.tx_dropped = self.tx_dropped.wrapping_add(1);
                    self.complete_used(mem, Self::QUEUE_TX, desc_head, 0)
                }
            };
            if !completed {
                self.fail_closed();
                return;
            }
            self.queues[Self::QUEUE_TX].last_avail_idx = last_avail.wrapping_add(1);
        }
    }

    /// Advance the rx (queue [`Self::QUEUE_RX`]) engine: while the guest
    /// has an available receive descriptor *and* [`NetBackend::poll_receive`]
    /// has a frame ready, prepend the 12-byte header (`num_buffers = 1`,
    /// module docs on [`Self::VIRTIO_NET_HDR_LEN`]) and copy the whole
    /// thing into that descriptor's buffer, then publish a used-buffer
    /// completion. The backend is polled only after a descriptor's
    /// availability is confirmed (module docs on [`NetBackend::poll_receive`]),
    /// so a frame is never popped from the backend only to be dropped for
    /// lack of anywhere to put it -- except when the descriptor itself
    /// turns out too small or not device-writable, in which case this
    /// engine fails closed the same way [`Self::process_tx`] does (the
    /// frame is lost in that case; a hostile-buffer guest gets no more
    /// service until it resets the device).
    fn process_rx(&mut self, mem: &mut dyn crate::GuestMemory) {
        if self.needs_reset() || !self.queues[Self::QUEUE_RX].enabled {
            return;
        }
        loop {
            let (driver_addr, desc_addr, last_avail) = {
                let q = &self.queues[Self::QUEUE_RX];
                (q.driver_addr, q.desc_addr, q.last_avail_idx)
            };
            let size = self.effective_queue_size(Self::QUEUE_RX);
            let Some(avail_idx) = gm_read_u16(mem, driver_addr.wrapping_add(2)) else {
                self.fail_closed();
                return;
            };
            if avail_idx == last_avail {
                return; // no rx buffer available; leave any pending frame for later
            }
            let Some(frame_len) = self.backend.poll_receive(&mut self.rx_frame_buf) else {
                return; // nothing to deliver yet
            };

            let Some(slot_addr) = Self::avail_ring_slot_addr(driver_addr, last_avail % size) else {
                self.fail_closed();
                return;
            };
            let Some(desc_head) = gm_read_u16(mem, slot_addr) else {
                self.fail_closed();
                return;
            };
            let Some((addr, len, flags, _next)) = Self::read_desc(mem, desc_addr, size, desc_head)
            else {
                self.fail_closed();
                return;
            };

            let total = Self::VIRTIO_NET_HDR_LEN + frame_len;
            if flags & desc_flags::WRITE == 0
                || flags & desc_flags::INDIRECT != 0
                || total > (len as usize)
                || total > self.scratch.len()
            {
                self.rx_dropped = self.rx_dropped.wrapping_add(1);
                self.fail_closed();
                return;
            }

            self.scratch[..Self::VIRTIO_NET_HDR_LEN].fill(0);
            self.scratch[10] = 1; // num_buffers (le16), low byte
            self.scratch[11] = 0;
            self.scratch[Self::VIRTIO_NET_HDR_LEN..total]
                .copy_from_slice(&self.rx_frame_buf[..frame_len]);

            let Some(a) = u32::try_from(addr).ok() else {
                self.fail_closed();
                return;
            };
            let Some(dst) = mem.ram_slice_mut(a, total as u32) else {
                self.fail_closed();
                return;
            };
            dst.copy_from_slice(&self.scratch[..total]);

            if !self.complete_used(mem, Self::QUEUE_RX, desc_head, total as u32) {
                self.fail_closed();
                return;
            }
            self.rx_frames = self.rx_frames.wrapping_add(1);
            self.queues[Self::QUEUE_RX].last_avail_idx = last_avail.wrapping_add(1);
        }
    }

    /// Read from BAR0 at `offset` (module docs, "BAR0 region map").
    fn bar0_read(&mut self, offset: u64, width: AccessWidth) -> u32 {
        let rel = offset as u32; // BAR containment already guarantees offset < BAR0_SIZE
        if rel < Self::COMMON_CFG_BAR_LEN {
            self.common_cfg_read(rel, width)
        } else if rel < Self::NOTIFY_CFG_BAR_OFFSET + Self::NOTIFY_CFG_BAR_LEN {
            0 // write-only in this device's own posture; see this file's report
        } else if rel < Self::ISR_CFG_BAR_OFFSET + Self::ISR_CFG_BAR_LEN {
            if rel == Self::ISR_CFG_BAR_OFFSET {
                let v = self.isr_status;
                self.isr_status = 0; // read-to-clear
                v as u32
            } else {
                0
            }
        } else {
            // Compose byte lanes so a word/long read of the MAC region is
            // faithful to natural-width access (virtio 1.x drivers may
            // read device config at any of the spec's allowed widths),
            // exactly as `common_cfg_read` composes its own window.
            let d = rel - Self::DEVICE_CFG_BAR_OFFSET;
            let mut value = 0u32;
            for lane in 0..width.bytes() {
                let byte = Self::MAC.get((d + lane) as usize).copied().unwrap_or(0);
                value |= (byte as u32) << (8 * lane);
            }
            value
        }
    }

    /// Write to BAR0 at `offset` (module docs, "BAR0 region map"). The
    /// notify and device-config windows both discard writes -- notify's
    /// content is never inspected (this file's own report explains why),
    /// and device config is read-only from the driver's own perspective.
    fn bar0_write(&mut self, offset: u64, width: AccessWidth, value: u32) {
        let rel = offset as u32;
        if rel < Self::COMMON_CFG_BAR_LEN {
            self.common_cfg_write(rel, width, value);
        }
        // NOTIFY_CFG and DEVICE_CFG: discarded, per this device's own
        // documented posture above.
    }
}

impl<'a> VirtioNetStub<'a> {
    /// Number of frames [`NetBackend::transmit`] has been handed.
    /// Host-side introspection only, not a register.
    pub fn tx_frames(&self) -> u32 {
        self.tx_frames
    }

    /// Number of tx descriptor chains this device dropped rather than
    /// forwarded (too short to contain even the header, or otherwise
    /// hostile). Host-side introspection only.
    pub fn tx_dropped(&self) -> u32 {
        self.tx_dropped
    }

    /// Number of frames successfully delivered into the guest's rx ring.
    /// Host-side introspection only.
    pub fn rx_frames(&self) -> u32 {
        self.rx_frames
    }

    /// Number of backend-offered rx frames this device could not deliver
    /// (no suitable descriptor). Host-side introspection only.
    pub fn rx_dropped(&self) -> u32 {
        self.rx_dropped
    }

    /// The current `device_status` byte -- host-side introspection (a
    /// guest driver reads the same value through
    /// [`common_off::DEVICE_STATUS`]).
    pub fn device_status(&self) -> u8 {
        self.device_status
    }

    /// The feature bits the driver has successfully acked so far --
    /// host-side introspection.
    pub fn acked_features(&self) -> u64 {
        self.acked_features
    }
}

impl<'a> PciDevice for VirtioNetStub<'a> {
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
            self.bar0_read(offset, width)
        } else {
            u32::MAX
        }
    }

    fn bar_write(&mut self, bar: usize, offset: u64, width: AccessWidth, value: u32) {
        if bar == 0 {
            self.bar0_write(offset, width, value);
        }
    }

    fn intx_level(&self) -> bool {
        self.isr_status & 0x1 != 0
    }

    fn tick(&mut self, mem: &mut dyn crate::GuestMemory) {
        self.process_tx(mem);
        self.process_rx(mem);
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
    /// Number of [`Self::bar_read`] calls seen at each width, indexed by
    /// [`width_index`] -- the stage-2 sized-aperture proof needs to show
    /// a word/long access reaches the backend as ONE access of that
    /// width, not four/two width-1 ones, and counting calls is the only
    /// way to see that from outside `pcibridge.rs`.
    read_counts: [u32; 3],
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
            read_counts: [0; 3],
        }
    }

    /// The most recent `(offset, width, value)` written through BAR0,
    /// or `None` if nothing has been written yet. Host-side test/debug
    /// accessor, not part of the guest-visible surface.
    pub fn last_write(&self) -> Option<(u64, AccessWidth, u32)> {
        self.last_write
    }

    /// How many [`Self::bar_read`] calls have landed at `width` so far --
    /// see [`Self::read_counts`]'s doc comment.
    pub fn read_count(&self, width: AccessWidth) -> u32 {
        self.read_counts[width_index(width)]
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
            self.read_counts[width_index(width)] += 1;
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

/// A minimal device whose only job is asserting/deasserting its `INTx`
/// line on command, wired to whatever pin its constructor chose --
/// proves [`VirtualPciBus::intx_levels`]'s pin-routing math end to end
/// independently of any device with real BAR-backed function, the same
/// role [`TestMemDevice`] plays for the aperture path.
pub struct IntxTestDevice {
    config: ConfigSpace,
    asserted: bool,
}

impl IntxTestDevice {
    /// Build a device wired to `Interrupt Pin` `pin` (`1` = INTA .. `4` =
    /// INTD, `0` = "uses no legacy interrupt pin" -- see
    /// [`ConfigSpaceIdentity::interrupt_pin`]). Starts deasserted.
    pub fn new(pin: u8) -> Self {
        let config = ConfigSpace::new(
            ConfigSpaceIdentity {
                vendor_id: 0x1234,
                device_id: 0xDEAD,
                revision: 0,
                class_base: 0xFF, // vendor-specific test device, like TestMemDevice
                class_sub: 0x00,
                prog_if: 0x00,
                subsystem_vendor: 0,
                subsystem_id: 0,
                interrupt_pin: pin,
            },
            [BarKind::None; 6],
        );
        Self {
            config,
            asserted: false,
        }
    }

    /// Assert or deassert this device's `INTx` line.
    pub fn set_asserted(&mut self, asserted: bool) {
        self.asserted = asserted;
    }
}

impl Default for IntxTestDevice {
    fn default() -> Self {
        Self::new(1)
    }
}

impl PciDevice for IntxTestDevice {
    fn config_read(&mut self, offset: u16, width: AccessWidth) -> u32 {
        self.config.read(offset, width)
    }

    fn config_write(&mut self, offset: u16, width: AccessWidth, value: u32) {
        self.config.write(offset, width, value);
    }

    fn bar_window(&self, bar: usize) -> Option<(u64, u64)> {
        self.config.bar_window(bar)
    }

    fn intx_level(&self) -> bool {
        self.asserted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GuestMemory;

    // ---- fixtures for stage 3's virtqueue tests -----------------------------

    /// Flat guest RAM based at 0 -- the same fixture shape `pktport.rs`'s
    /// own tests use (a distinct newtype per module rather than a shared
    /// `impl GuestMemory for Vec<u8>`, since trait impls are crate-global
    /// regardless of module).
    struct FakeRam(std::vec::Vec<u8>);

    impl FakeRam {
        fn new(size: usize) -> Self {
            Self(std::vec![0u8; size])
        }

        fn write_u16(&mut self, addr: u32, value: u16) {
            self.0[addr as usize..addr as usize + 2].copy_from_slice(&value.to_le_bytes());
        }

        fn write_u32(&mut self, addr: u32, value: u32) {
            self.0[addr as usize..addr as usize + 4].copy_from_slice(&value.to_le_bytes());
        }

        fn write_u64(&mut self, addr: u32, value: u64) {
            self.0[addr as usize..addr as usize + 8].copy_from_slice(&value.to_le_bytes());
        }

        fn read_u16(&self, addr: u32) -> u16 {
            u16::from_le_bytes(self.0[addr as usize..addr as usize + 2].try_into().unwrap())
        }

        /// Write one `virtq_desc` entry (16 bytes: `addr:u64, len:u32,
        /// flags:u16, next:u16`) at descriptor-table index `idx`.
        fn write_desc(&mut self, table: u32, idx: u16, addr: u64, len: u32, flags: u16, next: u16) {
            let base = table + 16 * idx as u32;
            self.write_u64(base, addr);
            self.write_u32(base + 8, len);
            self.write_u16(base + 12, flags);
            self.write_u16(base + 14, next);
        }

        /// Publish avail ring entry `pos` = `desc_head`, then set
        /// `avail.idx` to `pos + 1` -- a driver making exactly one more
        /// descriptor available.
        fn publish_avail(&mut self, avail: u32, pos: u16, desc_head: u16) {
            self.write_u16(avail + 4 + 2 * pos as u32, desc_head);
            self.write_u16(avail + 2, pos + 1);
        }
    }

    impl crate::GuestMemory for FakeRam {
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

    /// A [`NetBackend`] that records every transmitted frame and lets a
    /// test queue up frames for [`NetBackend::poll_receive`] to hand
    /// back -- module docs on [`NetBackend`], "a loopback/recording test
    /// backend".
    #[derive(Default)]
    struct LoopbackNetBackend {
        transmitted: std::vec::Vec<std::vec::Vec<u8>>,
        to_inject: std::vec::Vec<std::vec::Vec<u8>>,
    }

    impl LoopbackNetBackend {
        fn inject(&mut self, frame: &[u8]) {
            self.to_inject.push(frame.to_vec());
        }
    }

    impl NetBackend for LoopbackNetBackend {
        fn transmit(&mut self, frame: &[u8]) {
            self.transmitted.push(frame.to_vec());
        }

        fn poll_receive(&mut self, buf: &mut [u8]) -> Option<usize> {
            if self.to_inject.is_empty() {
                return None;
            }
            let frame = self.to_inject.remove(0);
            buf[..frame.len()].copy_from_slice(&frame);
            Some(frame.len())
        }
    }

    /// Fixed layout for the tests below: descriptor table, avail ring and
    /// used ring for ONE queue, each given generous headroom so a
    /// legitimately-sized chain never collides with the next region --
    /// hostile-address tests deliberately point outside this layout
    /// instead of anywhere in it.
    mod layout {
        pub const DESC: u32 = 0x0000;
        pub const AVAIL: u32 = 0x1000;
        pub const USED: u32 = 0x2000;
        pub const BUF: u32 = 0x3000;
        pub const RAM_SIZE: usize = 0x10000;
    }

    /// Select queue `qidx`, then fully configure it (`queue_size`,
    /// `queue_desc`/`queue_driver`/`queue_device`) and enable it -- the
    /// common setup every ring test below needs, once feature negotiation
    /// (a separate concern, its own tests) is out of the way.
    fn configure_queue(
        net: &mut VirtioNetStub,
        qidx: usize,
        size: u16,
        desc: u32,
        avail: u32,
        used: u32,
    ) {
        net.bar_write(
            0,
            common_off::QUEUE_SELECT as u64,
            AccessWidth::W16,
            qidx as u32,
        );
        net.bar_write(
            0,
            common_off::QUEUE_SIZE as u64,
            AccessWidth::W16,
            size as u32,
        );
        net.bar_write(0, common_off::QUEUE_DESC as u64, AccessWidth::W32, desc);
        net.bar_write(0, common_off::QUEUE_DRIVER as u64, AccessWidth::W32, avail);
        net.bar_write(0, common_off::QUEUE_DEVICE as u64, AccessWidth::W32, used);
        net.bar_write(0, common_off::QUEUE_ENABLE as u64, AccessWidth::W16, 1);
    }

    /// Negotiate exactly what a conformant driver would: ack both offered
    /// features, then walk `ACKNOWLEDGE -> DRIVER -> FEATURES_OK ->
    /// DRIVER_OK`, asserting each step lands the way the spec says it
    /// should (this is also, in effect, this file's own feature-
    /// negotiation test, inlined into every other test's setup rather
    /// than repeated).
    fn negotiate(net: &mut VirtioNetStub) {
        net.bar_write(
            0,
            common_off::DEVICE_FEATURE_SELECT as u64,
            AccessWidth::W32,
            0,
        );
        let low = net.bar_read(0, common_off::DEVICE_FEATURE as u64, AccessWidth::W32);
        net.bar_write(
            0,
            common_off::DRIVER_FEATURE_SELECT as u64,
            AccessWidth::W32,
            0,
        );
        net.bar_write(0, common_off::DRIVER_FEATURE as u64, AccessWidth::W32, low);

        net.bar_write(
            0,
            common_off::DEVICE_FEATURE_SELECT as u64,
            AccessWidth::W32,
            1,
        );
        let high = net.bar_read(0, common_off::DEVICE_FEATURE as u64, AccessWidth::W32);
        net.bar_write(
            0,
            common_off::DRIVER_FEATURE_SELECT as u64,
            AccessWidth::W32,
            1,
        );
        net.bar_write(0, common_off::DRIVER_FEATURE as u64, AccessWidth::W32, high);

        net.bar_write(
            0,
            common_off::DEVICE_STATUS as u64,
            AccessWidth::W8,
            status_bits::ACKNOWLEDGE as u32,
        );
        net.bar_write(
            0,
            common_off::DEVICE_STATUS as u64,
            AccessWidth::W8,
            (status_bits::ACKNOWLEDGE | status_bits::DRIVER) as u32,
        );
        net.bar_write(
            0,
            common_off::DEVICE_STATUS as u64,
            AccessWidth::W8,
            (status_bits::ACKNOWLEDGE | status_bits::DRIVER | status_bits::FEATURES_OK) as u32,
        );
        let after_features_ok = net.bar_read(0, common_off::DEVICE_STATUS as u64, AccessWidth::W8);
        assert_eq!(
            after_features_ok as u8 & status_bits::FEATURES_OK,
            status_bits::FEATURES_OK,
            "VERSION_1 was offered and acked, so FEATURES_OK must stick"
        );
        net.bar_write(
            0,
            common_off::DEVICE_STATUS as u64,
            AccessWidth::W8,
            (after_features_ok as u8 | status_bits::DRIVER_OK) as u32,
        );
        let final_status = net.bar_read(0, common_off::DEVICE_STATUS as u64, AccessWidth::W8) as u8;
        assert_eq!(
            final_status,
            status_bits::ACKNOWLEDGE
                | status_bits::DRIVER
                | status_bits::FEATURES_OK
                | status_bits::DRIVER_OK,
            "a full, honest negotiation must reach DRIVER_OK with nothing else set"
        );
    }

    // ---- feature negotiation -------------------------------------------------

    #[test]
    fn offered_features_are_exactly_version_1_and_mac() {
        let mut backend = LoopbackNetBackend::default();
        let mut net = VirtioNetStub::new(&mut backend);
        net.bar_write(
            0,
            common_off::DEVICE_FEATURE_SELECT as u64,
            AccessWidth::W32,
            0,
        );
        let low = net.bar_read(0, common_off::DEVICE_FEATURE as u64, AccessWidth::W32);
        net.bar_write(
            0,
            common_off::DEVICE_FEATURE_SELECT as u64,
            AccessWidth::W32,
            1,
        );
        let high = net.bar_read(0, common_off::DEVICE_FEATURE as u64, AccessWidth::W32);
        let offered = (low as u64) | ((high as u64) << 32);
        assert_eq!(offered, VirtioNetStub::F_VERSION_1 | VirtioNetStub::F_MAC);
    }

    #[test]
    fn a_full_negotiation_reaches_driver_ok() {
        let mut backend = LoopbackNetBackend::default();
        let mut net = VirtioNetStub::new(&mut backend);
        negotiate(&mut net);
        assert_eq!(
            net.acked_features(),
            VirtioNetStub::F_VERSION_1 | VirtioNetStub::F_MAC
        );
    }

    #[test]
    fn features_ok_is_cleared_when_version_1_was_never_acked() {
        let mut backend = LoopbackNetBackend::default();
        let mut net = VirtioNetStub::new(&mut backend);
        // Ack MAC only (bit 5, select-0 window) -- VERSION_1 (bit 32,
        // select-1 window) is deliberately never acked.
        net.bar_write(
            0,
            common_off::DRIVER_FEATURE_SELECT as u64,
            AccessWidth::W32,
            0,
        );
        net.bar_write(
            0,
            common_off::DRIVER_FEATURE as u64,
            AccessWidth::W32,
            VirtioNetStub::F_MAC as u32,
        );
        net.bar_write(
            0,
            common_off::DEVICE_STATUS as u64,
            AccessWidth::W8,
            (status_bits::ACKNOWLEDGE | status_bits::DRIVER | status_bits::FEATURES_OK) as u32,
        );
        let status = net.bar_read(0, common_off::DEVICE_STATUS as u64, AccessWidth::W8) as u8;
        assert_eq!(
            status & status_bits::FEATURES_OK,
            0,
            "the device must clear FEATURES_OK itself when VERSION_1 was never acked"
        );
    }

    #[test]
    fn driver_ok_without_features_ok_sets_device_needs_reset() {
        let mut backend = LoopbackNetBackend::default();
        let mut net = VirtioNetStub::new(&mut backend);
        // Skip straight to DRIVER_OK -- a driver that never checked
        // FEATURES_OK first.
        net.bar_write(
            0,
            common_off::DEVICE_STATUS as u64,
            AccessWidth::W8,
            (status_bits::ACKNOWLEDGE | status_bits::DRIVER | status_bits::DRIVER_OK) as u32,
        );
        let status = net.bar_read(0, common_off::DEVICE_STATUS as u64, AccessWidth::W8) as u8;
        assert_eq!(
            status & status_bits::DRIVER_OK,
            0,
            "DRIVER_OK must not stick"
        );
        assert_eq!(
            status & status_bits::DEVICE_NEEDS_RESET,
            status_bits::DEVICE_NEEDS_RESET,
            "refusal must be narratable, not silent"
        );
    }

    #[test]
    fn writing_zero_to_device_status_resets_negotiated_state() {
        let mut backend = LoopbackNetBackend::default();
        let mut net = VirtioNetStub::new(&mut backend);
        negotiate(&mut net);
        net.bar_write(0, common_off::DEVICE_STATUS as u64, AccessWidth::W8, 0);
        assert_eq!(net.device_status(), 0);
        assert_eq!(net.acked_features(), 0);
        // queue_size reads back the device's max again post-reset.
        net.bar_write(0, common_off::QUEUE_SELECT as u64, AccessWidth::W16, 1);
        let size = net.bar_read(0, common_off::QUEUE_SIZE as u64, AccessWidth::W16);
        assert_eq!(size, VirtioNetStub::QUEUE_SIZE_MAX as u32);
    }

    #[test]
    fn device_config_carries_the_documented_mac_at_offset_zero() {
        let mut backend = LoopbackNetBackend::default();
        let mut net = VirtioNetStub::new(&mut backend);
        for (i, expected) in VirtioNetStub::MAC.iter().enumerate() {
            let byte = net.bar_read(
                0,
                VirtioNetStub::DEVICE_CFG_BAR_OFFSET as u64 + i as u64,
                AccessWidth::W8,
            );
            assert_eq!(byte, *expected as u32, "MAC byte {i}");
        }
    }

    // ---- tx: frame extraction and used-buffer completion ---------------------

    #[test]
    fn tx_frame_reaches_backend_with_exact_bytes_header_stripped() {
        let mut backend = LoopbackNetBackend::default();
        let payload = [0xAAu8, 0xBB, 0xCC, 0xDD, 0xEE];
        let mut ram = FakeRam::new(layout::RAM_SIZE);
        {
            let mut net = VirtioNetStub::new(&mut backend);
            negotiate(&mut net);
            configure_queue(
                &mut net,
                VirtioNetStub::QUEUE_TX,
                8,
                layout::DESC,
                layout::AVAIL,
                layout::USED,
            );

            let mut frame = std::vec![0u8; VirtioNetStub::VIRTIO_NET_HDR_LEN + payload.len()];
            frame[VirtioNetStub::VIRTIO_NET_HDR_LEN..].copy_from_slice(&payload);
            ram.0[layout::BUF as usize..layout::BUF as usize + frame.len()].copy_from_slice(&frame);
            ram.write_desc(
                layout::DESC,
                0,
                layout::BUF as u64,
                frame.len() as u32,
                0,
                0,
            );
            ram.publish_avail(layout::AVAIL, 0, 0);

            net.tick(&mut ram);

            assert_eq!(net.tx_frames(), 1);
            assert_eq!(net.tx_dropped(), 0);
        }
        assert_eq!(backend.transmitted.len(), 1);
        assert_eq!(backend.transmitted[0], payload);
    }

    #[test]
    fn used_buffer_completion_bumps_used_idx_and_asserts_isr_and_intx() {
        let mut backend = LoopbackNetBackend::default();
        let mut net = VirtioNetStub::new(&mut backend);
        negotiate(&mut net);
        configure_queue(
            &mut net,
            VirtioNetStub::QUEUE_TX,
            8,
            layout::DESC,
            layout::AVAIL,
            layout::USED,
        );
        let mut ram = FakeRam::new(layout::RAM_SIZE);
        let frame = [0u8; VirtioNetStub::VIRTIO_NET_HDR_LEN + 4];
        ram.0[layout::BUF as usize..layout::BUF as usize + frame.len()].copy_from_slice(&frame);
        ram.write_desc(
            layout::DESC,
            0,
            layout::BUF as u64,
            frame.len() as u32,
            0,
            0,
        );
        ram.publish_avail(layout::AVAIL, 0, 0);

        assert!(!net.intx_level(), "nothing asserted before any traffic");
        net.tick(&mut ram);

        assert_eq!(ram.read_u16(layout::USED + 2), 1, "used.idx bumped once");
        assert!(net.intx_level(), "adding a used buffer must assert INTx");
        let isr = net.bar_read(0, VirtioNetStub::ISR_CFG_BAR_OFFSET as u64, AccessWidth::W8);
        assert_eq!(isr & 1, 1, "ISR bit 0 reports the used-buffer notification");
        assert!(
            !net.intx_level(),
            "reading ISR must clear INTx along with the byte"
        );
        let isr_again = net.bar_read(0, VirtioNetStub::ISR_CFG_BAR_OFFSET as u64, AccessWidth::W8);
        assert_eq!(isr_again, 0, "ISR itself reads back cleared");
    }

    // ---- rx: injected frames land in the guest ring ---------------------------

    #[test]
    fn rx_injected_frame_lands_in_guest_ring_with_header_and_exact_bytes() {
        let mut backend = LoopbackNetBackend::default();
        let payload = [0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66];
        backend.inject(&payload);
        let mut net = VirtioNetStub::new(&mut backend);
        negotiate(&mut net);
        configure_queue(
            &mut net,
            VirtioNetStub::QUEUE_RX,
            8,
            layout::DESC,
            layout::AVAIL,
            layout::USED,
        );

        let mut ram = FakeRam::new(layout::RAM_SIZE);
        let buf_len = VirtioNetStub::VIRTIO_NET_HDR_LEN as u32 + payload.len() as u32 + 32;
        ram.write_desc(
            layout::DESC,
            0,
            layout::BUF as u64,
            buf_len,
            desc_flags::WRITE,
            0,
        );
        ram.publish_avail(layout::AVAIL, 0, 0);

        net.tick(&mut ram);

        assert_eq!(net.rx_frames(), 1);
        assert_eq!(net.rx_dropped(), 0);
        let hdr = ram
            .ram_slice(layout::BUF, VirtioNetStub::VIRTIO_NET_HDR_LEN as u32)
            .unwrap();
        assert_eq!(
            hdr,
            [0u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0],
            "num_buffers=1, rest zero"
        );
        let got = ram
            .ram_slice(
                layout::BUF + VirtioNetStub::VIRTIO_NET_HDR_LEN as u32,
                payload.len() as u32,
            )
            .unwrap();
        assert_eq!(got, &payload[..]);
        assert_eq!(ram.read_u16(layout::USED + 2), 1);
        assert!(net.intx_level());
    }

    // ---- hostile rings fail closed, never panic --------------------------------

    #[test]
    fn tx_descriptor_pointing_outside_ram_fails_closed_without_panic() {
        let mut backend = LoopbackNetBackend::default();
        let mut net = VirtioNetStub::new(&mut backend);
        negotiate(&mut net);
        configure_queue(
            &mut net,
            VirtioNetStub::QUEUE_TX,
            8,
            layout::DESC,
            layout::AVAIL,
            layout::USED,
        );
        let mut ram = FakeRam::new(layout::RAM_SIZE);
        // Descriptor buffer address lies entirely outside this RAM. This
        // is a bad *frame*, not a corrupt ring -- the queue's own
        // desc/avail/used tables are all fine -- so this device drops
        // just this descriptor and keeps the ring moving (module docs on
        // `VirtioNetStub::process_tx`'s "completed" match), rather than
        // the harder [`a_hostile_avail_ring_address_fails_closed_without_panic`]
        // structural failure below.
        ram.write_desc(layout::DESC, 0, 0xFFFF_0000, 16, 0, 0);
        ram.publish_avail(layout::AVAIL, 0, 0);

        net.tick(&mut ram); // must not panic

        assert_eq!(
            net.device_status() & status_bits::DEVICE_NEEDS_RESET,
            0,
            "an unreadable descriptor is content, not a ring failure"
        );
        assert_eq!(net.tx_frames(), 0);
        assert_eq!(net.tx_dropped(), 1);
        assert_eq!(ram.read_u16(layout::USED + 2), 1, "the ring still advances");
    }

    #[test]
    fn tx_descriptor_with_a_length_past_the_frame_cap_fails_closed_without_panic() {
        let mut backend = LoopbackNetBackend::default();
        let mut net = VirtioNetStub::new(&mut backend);
        negotiate(&mut net);
        configure_queue(
            &mut net,
            VirtioNetStub::QUEUE_TX,
            8,
            layout::DESC,
            layout::AVAIL,
            layout::USED,
        );
        let mut ram = FakeRam::new(layout::RAM_SIZE);
        // A single descriptor claiming more bytes than this device will
        // ever stage at once -- well within RAM, so this exercises the
        // scratch-buffer cap, not the address check above.
        let huge_len = (VirtioNetStub::MAX_FRAME_TOTAL + 1) as u32;
        ram.write_desc(layout::DESC, 0, layout::BUF as u64, huge_len, 0, 0);
        ram.publish_avail(layout::AVAIL, 0, 0);

        net.tick(&mut ram); // must not panic

        assert_eq!(net.tx_frames(), 0);
        assert_eq!(
            net.tx_dropped(),
            1,
            "an oversized chain is dropped, not forwarded"
        );
    }

    #[test]
    fn a_descriptor_chain_that_cycles_is_bounded_and_dropped_not_looped_forever() {
        let mut backend = LoopbackNetBackend::default();
        let mut net = VirtioNetStub::new(&mut backend);
        negotiate(&mut net);
        configure_queue(
            &mut net,
            VirtioNetStub::QUEUE_TX,
            2,
            layout::DESC,
            layout::AVAIL,
            layout::USED,
        );
        let mut ram = FakeRam::new(layout::RAM_SIZE);
        // desc 0 -> desc 1 -> desc 0 -> ... both individually valid,
        // never terminating.
        ram.write_desc(layout::DESC, 0, layout::BUF as u64, 4, desc_flags::NEXT, 1);
        ram.write_desc(layout::DESC, 1, layout::BUF as u64, 4, desc_flags::NEXT, 0);
        ram.publish_avail(layout::AVAIL, 0, 0);

        net.tick(&mut ram); // must not hang or panic

        assert_eq!(net.tx_frames(), 0);
        assert_eq!(net.tx_dropped(), 1);
        // The ring still advances -- a cyclic chain is a bad *frame*, not
        // a broken ring, so the guest's next descriptor is still served.
        assert_eq!(ram.read_u16(layout::USED + 2), 1);
    }

    #[test]
    fn a_hostile_avail_ring_address_fails_closed_without_panic() {
        let mut backend = LoopbackNetBackend::default();
        let mut net = VirtioNetStub::new(&mut backend);
        negotiate(&mut net);
        // Point the avail ring itself outside RAM -- a corrupt queue
        // registration, not merely a bad descriptor.
        configure_queue(
            &mut net,
            VirtioNetStub::QUEUE_TX,
            8,
            layout::DESC,
            0xFFFF_0000,
            layout::USED,
        );
        let mut ram = FakeRam::new(layout::RAM_SIZE);

        net.tick(&mut ram); // must not panic

        assert_eq!(
            net.device_status() & status_bits::DEVICE_NEEDS_RESET,
            status_bits::DEVICE_NEEDS_RESET
        );
    }

    #[test]
    fn a_descriptor_index_at_or_past_queue_size_fails_closed_without_panic() {
        let mut backend = LoopbackNetBackend::default();
        let mut net = VirtioNetStub::new(&mut backend);
        negotiate(&mut net);
        configure_queue(
            &mut net,
            VirtioNetStub::QUEUE_TX,
            4,
            layout::DESC,
            layout::AVAIL,
            layout::USED,
        );
        let mut ram = FakeRam::new(layout::RAM_SIZE);
        // The avail ring names descriptor index 9, past this queue's
        // size of 4 -- never masked/modulo'd back into range (module
        // docs on `VirtioNetStub::read_desc`). Content, not structure
        // (module docs on the test above): dropped and counted, ring
        // still advances.
        ram.publish_avail(layout::AVAIL, 0, 9);

        net.tick(&mut ram); // must not panic or index out of bounds

        assert_eq!(net.device_status() & status_bits::DEVICE_NEEDS_RESET, 0);
        assert_eq!(net.tx_dropped(), 1);
        assert_eq!(ram.read_u16(layout::USED + 2), 1, "the ring still advances");
    }

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
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        let value = net.config_read(0x08, AccessWidth::W32);
        let class = (VirtioNetStub::CLASS_BASE as u32) << 16
            | (VirtioNetStub::CLASS_SUB as u32) << 8
            | VirtioNetStub::PROG_IF as u32;
        assert_eq!(value, (class << 8) | VirtioNetStub::REVISION as u32);
    }

    // ---- BAR sizing --------------------------------------------------------

    #[test]
    fn virtio_net_bar0_sizing_probe_reports_exactly_16kib_masked() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        net.config_write(0x10, AccessWidth::W32, 0xFFFF_FFFF);
        let readback = net.config_read(0x10, AccessWidth::W32);
        assert_eq!(readback, 0xFFFF_C000);
    }

    #[test]
    fn virtio_net_bar0_restores_a_base_with_low_bits_masked_off() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
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
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
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
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        net.config_write(0x04, AccessWidth::W16, 0xFFFF);
        // Only IO/MEM/BUS_MASTER (bits 0-2) may be set; everything else,
        // including the whole high byte, reads back zero.
        assert_eq!(net.config_read(0x04, AccessWidth::W16), 0x0007);
    }

    #[test]
    fn status_capabilities_bit_is_set_on_the_stub_and_clear_on_the_bridge() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        let mut bridge = HostBridge::new();
        assert_eq!(net.config_read(0x06, AccessWidth::W16) & 0x10, 0x10);
        assert_eq!(bridge.config_read(0x06, AccessWidth::W16) & 0x10, 0);
    }

    #[test]
    fn status_writes_are_discarded_no_write_one_to_clear_modelled() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        let before = net.config_read(0x06, AccessWidth::W16);
        net.config_write(0x06, AccessWidth::W16, 0xFFFF);
        assert_eq!(net.config_read(0x06, AccessWidth::W16), before);
    }

    // ---- capability chain walk ----------------------------------------------

    #[test]
    fn capability_chain_walks_all_four_virtio_structures_correctly() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);

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
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        assert_eq!(net.config_read(0x100, AccessWidth::W32), 0xFFFF_FFFF);
        assert_eq!(net.config_read(0xFFF0, AccessWidth::W16), 0xFFFF);
        net.config_write(0x100, AccessWidth::W32, 0x1234_5678);
        assert_eq!(net.config_read(0x100, AccessWidth::W32), 0xFFFF_FFFF);
    }

    #[test]
    fn a_width_that_would_straddle_the_0x100_boundary_fails_closed() {
        let mut net_backend = NullNetBackend;
        let mut net = VirtioNetStub::new(&mut net_backend);
        // offset 0xFD + W32 spans 0xFD..0x101 -- past the 256-byte space.
        assert_eq!(net.config_read(0xFD, AccessWidth::W32), 0xFFFF_FFFF);
        net.config_write(0xFD, AccessWidth::W32, 0x1111_1111);
        // Confirm nothing was corrupted by peeking a byte inside range.
        assert_eq!(
            net.config_read(0xFD, AccessWidth::W8),
            0, // reserved, untouched byte
        );
    }

    // ---- INTx routing (VirtualPciBus::intx_levels) --------------------------

    #[test]
    fn an_asserted_device_ors_its_line_onto_the_bit_its_pin_names() {
        let mut inta = IntxTestDevice::new(1); // INTA -> bit 0
        let mut intc = IntxTestDevice::new(3); // INTC -> bit 2
        inta.set_asserted(true);
        let mut slots = [
            VirtualSlot {
                bdf: Bdf {
                    bus: 0,
                    device: 0,
                    function: 0,
                },
                device: &mut inta,
            },
            VirtualSlot {
                bdf: Bdf {
                    bus: 0,
                    device: 1,
                    function: 0,
                },
                device: &mut intc,
            },
        ];
        let mut bus = VirtualPciBus::new(&mut slots);
        assert_eq!(bus.intx_levels(), 0b0001, "only INTA's bit is set");
    }

    #[test]
    fn pin_zero_never_asserts_regardless_of_intx_level() {
        let mut dev = IntxTestDevice::new(0); // "uses no legacy interrupt pin"
        dev.set_asserted(true);
        let mut slots = [VirtualSlot {
            bdf: Bdf {
                bus: 0,
                device: 0,
                function: 0,
            },
            device: &mut dev,
        }];
        let mut bus = VirtualPciBus::new(&mut slots);
        assert_eq!(bus.intx_levels(), 0);
    }

    #[test]
    fn multiple_devices_on_the_same_pin_or_onto_one_bit() {
        let mut a = IntxTestDevice::new(2); // INTB -> bit 1
        let mut b = IntxTestDevice::new(2); // same pin, different device
        a.set_asserted(true);
        b.set_asserted(false);
        {
            let mut slots = [
                VirtualSlot {
                    bdf: Bdf {
                        bus: 0,
                        device: 0,
                        function: 0,
                    },
                    device: &mut a,
                },
                VirtualSlot {
                    bdf: Bdf {
                        bus: 0,
                        device: 1,
                        function: 0,
                    },
                    device: &mut b,
                },
            ];
            let mut bus = VirtualPciBus::new(&mut slots);
            assert_eq!(bus.intx_levels(), 0b0010, "one device asserting is enough");
        }

        // Deassert a, assert b instead: still just bit 1 -- the OR does
        // not depend on which of the two devices sharing the pin is the
        // one currently asserting.
        a.set_asserted(false);
        b.set_asserted(true);
        {
            let mut slots = [
                VirtualSlot {
                    bdf: Bdf {
                        bus: 0,
                        device: 0,
                        function: 0,
                    },
                    device: &mut a,
                },
                VirtualSlot {
                    bdf: Bdf {
                        bus: 0,
                        device: 1,
                        function: 0,
                    },
                    device: &mut b,
                },
            ];
            let mut bus = VirtualPciBus::new(&mut slots);
            assert_eq!(bus.intx_levels(), 0b0010);
        }
    }

    #[test]
    fn deasserting_every_device_on_a_line_clears_it() {
        let mut a = IntxTestDevice::new(4); // INTD -> bit 3
        a.set_asserted(true);
        {
            let mut slots = [VirtualSlot {
                bdf: Bdf {
                    bus: 0,
                    device: 0,
                    function: 0,
                },
                device: &mut a,
            }];
            let mut bus = VirtualPciBus::new(&mut slots);
            assert_eq!(bus.intx_levels(), 0b1000);
        }
        a.set_asserted(false);
        {
            let mut slots = [VirtualSlot {
                bdf: Bdf {
                    bus: 0,
                    device: 0,
                    function: 0,
                },
                device: &mut a,
            }];
            let mut bus = VirtualPciBus::new(&mut slots);
            assert_eq!(bus.intx_levels(), 0, "no bit is stuck once deasserted");
        }
    }
}
