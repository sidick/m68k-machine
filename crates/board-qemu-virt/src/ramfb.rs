//! A bare-metal driver for QEMU's `ramfb` device (`-device ramfb`), reached
//! through `fw_cfg`'s DMA interface.
//!
//! There is no UEFI GOP here (no UEFI at all: `board-qemu-virt` is a raw
//! `-kernel` ELF load, per `main.rs`'s module doc comment) and QEMU virt has
//! no `virtio-gpu` support wired into this board without a full virtqueue
//! driver, which the task brief this module was written against explicitly
//! rules out building on this board's own initiative. `ramfb` is the
//! documented alternative: a linear framebuffer whose address, format and
//! geometry are configured once, by writing a small control struct through
//! `fw_cfg`, after which QEMU's display code scans it out on its own like
//! any other linear framebuffer -- no register interface, no command
//! ring, nothing to poll or acknowledge per frame.
//!
//! **Verified present**, not assumed: `qemu-system-aarch64 -M virt -device
//! ramfb` instantiates cleanly on this project's installed QEMU (11.1.1)
//! and a QMP `screendump` off that configuration produces a real 640x480
//! image before any guest has configured it, confirming both that the
//! device exists on this machine type and that its default (pre-configuration)
//! surface is inspectable the same way this board's own configured surface
//! is checked in `docs/screenshots.md`-style verification.
//!
//! Normally a firmware payload (EDK2, U-Boot) does this configuration on
//! the guest's behalf via a `ramfb` DRM driver talking to its own `fw_cfg`
//! client code. This board has no firmware underneath it at all -- QEMU's
//! `-kernel` loader jumps straight to `_start` -- so the ELF image itself
//! has to be that client.
//!
//! # `fw_cfg` MMIO layout
//!
//! Confirmed against this project's own QEMU by dumping the `virt` machine's
//! device tree (`qemu-system-aarch64 -M virt ... -machine dumpdtb=...`):
//!
//! ```text
//! fw-cfg@9020000 {
//!     reg = <0x00 0x9020000 0x00 0x18>;
//!     compatible = "qemu,fw-cfg-mmio";
//! };
//! ```
//!
//! 24 bytes (`0x18`), laid out per QEMU's `docs/specs/fw_cfg.txt`:
//! - `+0x00`: Data register (legacy interface; not used here)
//! - `+0x08`: Selector register, 16-bit
//! - `+0x10`: DMA address register, 64-bit -- writing the physical address
//!   of a [`DmaAccess`] block here triggers that transfer
//!
//! # Endianness
//!
//! Every `fw_cfg` MMIO register is modelled by QEMU as big-endian
//! (`DEVICE_BIG_ENDIAN` memory region ops), while this CPU boots and runs
//! with `SCTLR_EL1.EE` clear -- ordinary little-endian data accesses (see
//! `main.rs`'s `_start`, which never touches `EE`). A plain little-endian
//! store of some value `V` therefore arrives at the device byte-reversed
//! from what the guest meant; the fix used throughout this module is the
//! same one Linux's `iowrite32be`/`iowrite64be` use on little-endian ARM:
//! pre-swap the value in the general-purpose register with `V.swap_bytes()`
//! before an ordinary store, so the two reversals cancel and the device
//! sees `V`. Multi-byte *content* fields (inside [`DmaAccess`] and
//! [`RamfbCfg`]) are a different, unrelated concern -- see those structs'
//! doc comments -- because they are plain RAM the device DMAs from, not
//! MMIO register writes, so ordinary `to_be_bytes()` is enough there with
//! no register-endianness math involved.
//!
//! # Verification
//!
//! `-trace fw_cfg_select` (this project's QEMU build has no
//! `fw_cfg_dma_*`/`ramfb_*` trace points, checked with `-trace help`) shows
//! the file-directory selector and the file's own selector both being
//! read during bring-up of this driver; a `qmp screendump` afterwards
//! producing a non-black image at the configured width/height is the
//! stronger and more direct signal that the DMA write actually landed and
//! `ramfb_create_display_surface` ran, so that -- not the trace log -- is
//! this driver's real acceptance test (see `docs/display-boards.md`,
//! written for exactly this).

use core::sync::atomic::{compiler_fence, Ordering};

/// `fw_cfg`'s fixed MMIO base on QEMU's `virt` machine (confirmed by DTB
/// dump above; also the well-known constant QEMU has used for this machine
/// type since `fw_cfg` support was added to `virt`, long before this
/// project existed).
const FWCFG_BASE: usize = 0x0902_0000;
const FWCFG_SELECTOR: usize = FWCFG_BASE + 0x08;
const FWCFG_DATA: usize = FWCFG_BASE;
const FWCFG_DMA_ADDR: usize = FWCFG_BASE + 0x10;

/// Selector for the file directory itself -- fixed by the `fw_cfg` spec,
/// not something QEMU assigns dynamically (unlike a regular file's own
/// selector, which is looked up by name below).
const FW_CFG_FILE_DIR: u16 = 0x19;

/// `FW_CFG_MAX_FILE_PATH`: fixed name field width in a directory entry.
const FW_CFG_MAX_FILE_PATH: usize = 56;

/// `fw_cfg` DMA control bits (`docs/specs/fw_cfg.txt`). `SELECT`'s target
/// selector rides in the control word's top 16 bits, per that same spec.
const FW_CFG_DMA_CTL_ERROR: u32 = 1 << 0;
const FW_CFG_DMA_CTL_SELECT: u32 = 1 << 3;
const FW_CFG_DMA_CTL_WRITE: u32 = 1 << 4;

/// Write a big-endian 16-bit value to an MMIO register. See this module's
/// doc comment ("Endianness") for why the pre-swap is needed and why it is
/// correct on this little-endian core.
fn write_be16(addr: usize, value: u16) {
    // SAFETY: `addr` is always one of this module's fixed `fw_cfg` MMIO
    // register addresses; the write itself is a plain aligned store with
    // no side effects the compiler needs to reorder around beyond the
    // fence already placed at each call site that needs one.
    unsafe { (addr as *mut u16).write_volatile(value.swap_bytes()) };
}

/// Write a big-endian 64-bit value to an MMIO register. See this module's
/// doc comment ("Endianness").
fn write_be64(addr: usize, value: u64) {
    // SAFETY: as `write_be16` above.
    unsafe { (addr as *mut u64).write_volatile(value.swap_bytes()) };
}

/// Read one byte from the legacy data register. Single-byte accesses carry
/// no endianness ambiguity (a lone byte has no internal byte order), and
/// the `fw_cfg` data register auto-advances one byte per read within
/// whatever item is currently selected -- this is how the file directory
/// below is walked, and it is the only place in this module the legacy
/// (non-DMA) interface is used at all.
fn read_data_byte() -> u8 {
    // SAFETY: FWCFG_DATA is a fixed, always-valid `fw_cfg` MMIO register;
    // reading it has the documented side effect of advancing the current
    // item's read cursor, which is exactly what every call site wants.
    unsafe { (FWCFG_DATA as *const u8).read_volatile() }
}

fn select(selector: u16) {
    write_be16(FWCFG_SELECTOR, selector);
}

/// One 64-byte directory entry (`docs/specs/fw_cfg.txt`'s `FWCfgFile`):
/// `size` (4 bytes BE), `select` (2 bytes BE) -- the selector this file is
/// addressed by, distinct from `FW_CFG_FILE_DIR`'s own fixed selector
/// above -- `reserved` (2 bytes, skipped), then a fixed-width, NUL-padded
/// name.
fn read_directory_entry() -> (u32, u16, [u8; FW_CFG_MAX_FILE_PATH]) {
    let mut size_bytes = [0u8; 4];
    for b in &mut size_bytes {
        *b = read_data_byte();
    }
    let mut select_bytes = [0u8; 2];
    for b in &mut select_bytes {
        *b = read_data_byte();
    }
    let _reserved = [read_data_byte(), read_data_byte()];
    let mut name = [0u8; FW_CFG_MAX_FILE_PATH];
    for b in &mut name {
        *b = read_data_byte();
    }
    (
        u32::from_be_bytes(size_bytes),
        u16::from_be_bytes(select_bytes),
        name,
    )
}

/// Walk the `fw_cfg` file directory looking for `target` (an exact,
/// NUL-or-end-terminated match), returning its selector. `None` means this
/// QEMU build's `fw_cfg` never advertised the file at all -- e.g. `ramfb`
/// support compiled out, or the device not attached (`-device ramfb`
/// missing from the command line) -- as opposed to a driver bug further
/// down, which would instead show up as the file being found but the
/// configured surface never appearing.
fn find_file_selector(target: &str) -> Option<u16> {
    select(FW_CFG_FILE_DIR);
    let mut count_bytes = [0u8; 4];
    for b in &mut count_bytes {
        *b = read_data_byte();
    }
    let count = u32::from_be_bytes(count_bytes);

    // Directory size is QEMU's own (bounded by how many devices/ROMs it
    // registered), not guest-controlled -- but a corrupt/garbage count
    // must still not hang this driver, so cap the walk generously above
    // any real QEMU machine's file count.
    const MAX_FILES: u32 = 4096;
    for _ in 0..count.min(MAX_FILES) {
        let (_size, sel, name) = read_directory_entry();
        let name_len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        if &name[..name_len] == target.as_bytes() {
            return Some(sel);
        }
    }
    None
}

/// `fw_cfg` DMA access block (`docs/specs/fw_cfg.txt`): `control` (4 bytes
/// BE), `length` (4 bytes BE), `address` (8 bytes BE) -- the physical
/// address of the buffer this operation reads from or writes into.
/// `#[repr(C)]` with explicit big-endian field *contents* (via
/// `to_be_bytes`, not the struct's memory layout, which is whatever the
/// target picks): this is plain RAM the device DMA-reads on its own, not
/// an MMIO register, so there is no store-instruction endianness to fight
/// here -- only the byte order of the data QEMU expects to find once it
/// reads this struct, which is exactly what `to_be_bytes` produces
/// regardless of the CPU's own endianness.
#[repr(C, align(4))]
struct DmaAccess {
    bytes: [u8; 16],
}

impl DmaAccess {
    fn select_and_write(selector: u16, length: u32, address: u64) -> Self {
        let control = ((selector as u32) << 16) | FW_CFG_DMA_CTL_SELECT | FW_CFG_DMA_CTL_WRITE;
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&control.to_be_bytes());
        bytes[4..8].copy_from_slice(&length.to_be_bytes());
        bytes[8..16].copy_from_slice(&address.to_be_bytes());
        Self { bytes }
    }

    fn control(&self) -> u32 {
        u32::from_be_bytes(self.bytes[0..4].try_into().unwrap())
    }
}

/// `ramfb`'s own `fw_cfg` write-payload (`hw/display/ramfb.c`'s
/// `RAMFBCfg`): 64-bit address, 32-bit DRM fourcc, 32-bit flags (unused),
/// width, height, stride, all big-endian -- see [`DmaAccess`]'s doc
/// comment for why plain `to_be_bytes` is the right (and only) tool here,
/// with no MMIO endianness question involved.
struct RamfbCfg {
    bytes: [u8; 28],
}

/// `DRM_FORMAT_XRGB8888` fourcc ("XR24"): four ASCII bytes packed as a
/// little-endian `u32` per the DRM fourcc convention (`'X' | 'R'<<8 |
/// '2'<<16 | '4'<<24`), which is the standard fourcc value regardless of
/// CPU endianness -- it names a byte *format*, not a machine word. In
/// memory each pixel is bytes `B,G,R,X` low-to-high, which is exactly what
/// an ordinary little-endian 32-bit store of `0xAARRGGBB` (this project's
/// [`machine_core::display::Framebuffer`] pixel format, alpha byte simply
/// unused/ignored as the "X" padding byte) already produces -- so
/// `present`'s blit below can write native `u32`s straight into the
/// framebuffer with no channel reordering, unlike a byte-order-sensitive
/// register write.
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;

impl RamfbCfg {
    fn new(addr: u64, width: u32, height: u32, stride: u32) -> Self {
        let mut bytes = [0u8; 28];
        bytes[0..8].copy_from_slice(&addr.to_be_bytes());
        bytes[8..12].copy_from_slice(&DRM_FORMAT_XRGB8888.to_be_bytes());
        bytes[12..16].copy_from_slice(&0u32.to_be_bytes()); // flags: unused
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        bytes[24..28].copy_from_slice(&stride.to_be_bytes());
        Self { bytes }
    }
}

/// Host framebuffer geometry this board drives `ramfb` at. `1600x1200`
/// (a real, independently-standard 4:3 mode, not picked to make the scale
/// arithmetic trivial) is deliberate: it is more than `2x` the Amiga
/// canvas (`machine_core::display::MAX_WIDTH`/`MAX_HEIGHT`, 752x576) on
/// both axes, so the integer-scale-then-letterbox logic in `main.rs`'s
/// `present` actually exercises a `2x` path end to end rather than always
/// falling back to `1x` -- see the coordinator's correction recorded in
/// `main.rs`'s `scale_factor` doc comment for why `1x`-only would not have
/// been a sufficient test of that logic. `4 * WIDTH * HEIGHT` bytes
/// (`XRGB8888`, 4 bytes/pixel) is 7.5 MiB, comfortably inside the 512 MiB
/// `-m` this board's QEMU invocation already configures.
pub const HOST_WIDTH: u32 = 1600;
pub const HOST_HEIGHT: u32 = 1200;

/// Result of [`init`]: whether `ramfb` was found and configured, so
/// `main.rs` can report a sharp negative over CI's own serial channel
/// rather than silently skipping the display step.
pub enum RamfbStatus {
    /// Configured; frames written to `framebuffer` will be scanned out.
    Ready,
    /// `etc/ramfb` was never in the `fw_cfg` directory -- `-device ramfb`
    /// was not attached, or this QEMU build has no `ramfb` support.
    DeviceNotPresent,
    /// The file was found but the configuring DMA write reported
    /// `FW_CFG_DMA_CTL_ERROR` in its control word after the transfer --
    /// QEMU rejected the configuration (e.g. a fourcc/geometry it does not
    /// support), rather than the device being absent altogether.
    ConfigureFailed,
}

/// Configure `ramfb` to scan out from the `HOST_WIDTH * HOST_HEIGHT` pixel
/// buffer at `fb_addr` (a physical address -- identical to a virtual one
/// here, since this payload never enables the MMU, per `main.rs`'s
/// `_start`). Takes a bare address rather than a borrowed slice
/// deliberately: what `ramfb` actually needs is a value handed to QEMU
/// once and then never touched again through this driver (QEMU reads that
/// memory on its own schedule from then on, per this function's own doc
/// comment below), which a raw address models more honestly than a Rust
/// reference whose borrow-checker lifetime has no counterpart in what the
/// hardware protocol actually requires. `main.rs` owns the buffer itself
/// (a `static`, so its address is valid for the process's whole lifetime)
/// and separately keeps a normal `&mut` to it for writing frames.
///
/// Returns the status so `main.rs` can narrate it over serial either way
/// (CI's existing markers are unaffected either way -- see this module's
/// doc comment on verification and `main.rs`'s note on marker ordering).
///
/// Only ever needs to run once: unlike a real display controller with
/// mode-change or vblank interrupts to field, `ramfb` has no further
/// register interface after this -- QEMU re-reads the configured memory
/// on its own schedule, so presenting a new frame later is just writing
/// into the same buffer again (`main.rs`'s `present`), no re-configuration
/// needed.
pub fn init(fb_addr: u64) -> RamfbStatus {
    let Some(selector) = find_file_selector("etc/ramfb") else {
        return RamfbStatus::DeviceNotPresent;
    };

    let stride = HOST_WIDTH * 4;
    let cfg = RamfbCfg::new(fb_addr, HOST_WIDTH, HOST_HEIGHT, stride);

    // The DMA access block itself must also be plain, addressable RAM the
    // device reads independently of this function's own stack frame
    // still being live -- it is, since the DMA transfer QEMU performs in
    // response to the trigger write below happens synchronously with
    // that MMIO write (QEMU's device model runs the whole DMA operation
    // inline before the store instruction that triggered it retires from
    // the guest's point of view), not asynchronously afterwards.
    let dma = DmaAccess::select_and_write(selector, cfg.bytes.len() as u32, {
        // SAFETY: `cfg` is a local on this function's stack, still live
        // for the rest of this function -- exactly as long as the DMA
        // transfer that reads it takes (see the paragraph above).
        core::ptr::addr_of!(cfg.bytes) as u64
    });

    // Ensure the config struct's own content writes (`RamfbCfg::new`,
    // above) are visible before the DMA trigger below -- caches are off
    // for this whole payload (no MMU, per `main.rs`'s `_start`), so this
    // is a compiler-reordering fence, not a cache-coherence one, but
    // cheap insurance either way.
    compiler_fence(Ordering::SeqCst);

    write_be64(FWCFG_DMA_ADDR, core::ptr::addr_of!(dma.bytes) as u64);

    // QEMU performs the DMA synchronously with the triggering store (see
    // `dma`'s field doc comment above), so `dma.control()` already
    // reflects the outcome by the time execution reaches here.
    if dma.control() & FW_CFG_DMA_CTL_ERROR != 0 {
        return RamfbStatus::ConfigureFailed;
    }

    RamfbStatus::Ready
}
