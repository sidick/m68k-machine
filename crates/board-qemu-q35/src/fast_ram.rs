//! Fast RAM allocated from UEFI boot services.
//!
//! `MachineBus::with_fast_ram` (`machine-core` has no allocator) borrows
//! caller-owned storage for as long as the bus lives, exactly like chip
//! RAM and VRAM. On this board that storage comes from
//! `uefi::boot::allocate_pages` rather than a static buffer --
//! `board-qemu-q35`'s boot-services allocator is already in play for
//! everything else (`main.rs`'s module doc comment), and 256 MB is far
//! too large for a static `.bss` array or this payload's stack.
//!
//! **Why 256 MB, and why it can fall back:** `machine-hosted --fast-ram-mb`
//! defaults to 256 (`crates/machine-hosted/src/cli.rs`), and
//! `tools/amibake/m68k-machine.toml` declares that as this machine's
//! shape -- so this board matches it when it can. But a UEFI boot-time
//! allocator has none of a hosted process's virtual-memory headroom
//! (real firmware, or QEMU given a small `-m`, may simply not have 256
//! MB of pages free this early), and devsoak's four workers only ever
//! needed on the order of 2 MB of buffers (`docs/hostblk-soak.md`) --
//! so less fast RAM is a real, useful machine, not a failed one.
//! [`allocate`] halves its request on `OUT_OF_RESOURCES`/`NOT_FOUND`
//! down to a floor, and reports exactly what it got (or that it got
//! nothing) rather than failing the whole boot over a RAM board that
//! was never mandatory to begin with (`MachineBus::with_fast_ram`'s own
//! doc comment: omitting it just means the chain and its address space
//! go untouched).

use alloc::string::String;

use uefi::boot::{self, AllocateType, MemoryType};

/// Preferred fast RAM size, matching `machine-hosted --fast-ram-mb`'s
/// default and `tools/amibake/m68k-machine.toml`'s declared machine
/// shape.
const PREFERRED_MB: u32 = 256;

/// Below this, don't bother -- not enough to be worth a Zorro III board
/// and an AUTOCONFIG slot for. `machine_core::fastram`'s own minimum
/// extended-table size is smaller than this, but a few hundred KB of
/// fast RAM buys this board nothing devsoak's ~2 MB working set needs.
const FLOOR_MB: u32 = 1;

/// Outcome of [`allocate`]: the memory (if any), and how it compares to
/// what was asked for -- both worth reporting, per this module's doc
/// comment.
pub struct FastRam {
    pub mem: Option<&'static mut [u8]>,
    pub requested_mb: u32,
    pub got_mb: u32,
}

impl FastRam {
    pub fn describe(&self) -> String {
        if self.mem.is_some() {
            if self.got_mb == self.requested_mb {
                alloc::format!("fast RAM: {} MB allocated from boot services", self.got_mb)
            } else {
                alloc::format!(
                    "fast RAM: {} MB allocated from boot services (wanted {} MB; \
                     halved down until an allocation succeeded)",
                    self.got_mb,
                    self.requested_mb
                )
            }
        } else {
            alloc::format!(
                "fast RAM: none -- boot services could not satisfy even a {FLOOR_MB} MB \
                 request -- continuing with chip RAM only"
            )
        }
    }
}

/// Try to allocate [`PREFERRED_MB`] of fast RAM from UEFI boot services,
/// halving on failure down to [`FLOOR_MB`]. Returns the largest size
/// that succeeded, or `None` if even the floor size failed.
pub fn allocate() -> FastRam {
    let mut size_mb = PREFERRED_MB;
    loop {
        let bytes = (size_mb as usize) * 1024 * 1024;
        let pages = bytes.div_ceil(uefi::boot::PAGE_SIZE);
        match boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, pages) {
            Ok(ptr) => {
                let len = pages * uefi::boot::PAGE_SIZE;
                // SAFETY: `allocate_pages` just returned this pointer as
                // the start of a fresh, exclusively-owned allocation of
                // `len` bytes; nothing else can alias it, and it lives
                // until the program exits (never freed -- this board
                // never calls `ExitBootServices`, so leaking it for the
                // run's duration is fine, same reasoning as the chip RAM
                // and ROM buffers this board never frees either).
                let mem: &'static mut [u8] =
                    unsafe { core::slice::from_raw_parts_mut(ptr.as_ptr(), len) };
                mem.fill(0);
                return FastRam {
                    mem: Some(mem),
                    requested_mb: PREFERRED_MB,
                    got_mb: size_mb,
                };
            }
            Err(_) if size_mb > FLOOR_MB => {
                size_mb = (size_mb / 2).max(FLOOR_MB);
            }
            Err(_) => {
                return FastRam {
                    mem: None,
                    requested_mb: PREFERRED_MB,
                    got_mb: 0,
                };
            }
        }
    }
}
