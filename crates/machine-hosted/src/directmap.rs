//! Direct address-space mapping (`docs/cpu-core-proposal.md` §4.6, C1's
//! last slice) on the hosted backend: macOS/POSIX only, built on `libc`.
//!
//! # What this is
//!
//! One 4 GiB anonymous `PROT_NONE` reservation of host virtual address
//! space ([`DirectMap::reservation`]). A guest access at address `a` is
//! served, when the page-type table below says it is safe to, at
//! `reservation_base + a` -- no bounds check beyond that table lookup,
//! no call into [`MachineBus`]. RAM and ROM pages are mapped readable
//! (RAM also writable) inside the reservation; I/O and open-bus pages
//! stay `PROT_NONE` and are never dereferenced by this module's own
//! fast path (§4.6: "I/O is found inline, faults are a backstop" -- this
//! slice does not build the fault-patching half of that sentence, only
//! the inline table).
//!
//! # Page-type table
//!
//! One [`PageType`] per 64 KiB guest page, [`PAGE_COUNT`] entries
//! (`65536 = 2^32 / 2^16`). Invariant: a page typed [`PageType::Ram`] or
//! [`PageType::Rom`] is *always* mapped and accessible in the
//! reservation for as long as that type is current; [`PageType::Io`]
//! and [`PageType::OpenBus`] pages stay `PROT_NONE`.
//!
//! # Classification asks the bus, never hardcodes an AUTOCONFIG address
//!
//! `docs/device-ledger.md`'s standing rule, extended to this table:
//! [`DirectMap::classify`] reads fixed architectural ranges from
//! `machine_core`'s own constants (the same posture `bus.rs::classify`
//! already takes for the `BUS_COVERAGE` diagnostic), but every
//! AUTOCONFIG-placed board -- fast RAM included -- comes from iterating
//! `bus.autoconfig.placement(i)` and comparing against
//! `bus.fast_ram_window()`. Nothing in this module ever compares an
//! address against a constant `machine-hosted` or `machine-core`
//! invented for where a board *usually* ends up.
//!
//! # The RAM aliasing argument (SAFETY, read before touching this file)
//!
//! Chip RAM and fast RAM are `MAP_SHARED` POSIX shared-memory regions
//! ([`ShmRegion`]): one anonymous `shm_open` object per region, opened
//! with a random name, `shm_unlink`-ed immediately (so the name never
//! leaks into the filesystem namespace and the object's lifetime is
//! purely refcounted by open mappings + the fd), `ftruncate`-d to the
//! region's size, then `mmap(MAP_SHARED)`-ed twice: once as the
//! *primary* view (the pointer [`MachineBus`] borrows, exactly the way
//! `run.rs` handed it a `Box`/`Vec` before this module existed), and
//! once more, of the *same* `fd`, `MAP_FIXED` into the reservation at
//! the guest's base address (chip RAM at `CHIP_RAM_BASE` immediately;
//! fast RAM at its placed base once [`MachineBus::fast_ram_window`]
//! reports one). Both mappings back onto the same physical pages, so a
//! write through either view is visible through the other immediately --
//! this is exactly what `MAP_SHARED` guarantees, not something this
//! module has to arrange itself.
//!
//! This is safe under three conditions, all of which hold here:
//!
//! 1. **Single-threaded.** Nothing here races a direct-path read against
//!    a `MachineBus` write on another thread; `machine-hosted`'s run
//!    loops are single-threaded end to end.
//! 2. **The two views are never both live *during* a single logical
//!    access.** A guest instruction either goes through the direct path
//!    (this module) or through `MachineBus` (the primary view, e.g. a
//!    device's own DMA), never both at once -- `Bus`'s adapter methods
//!    pick exactly one of the two per call, never interleave them
//!    within one access. Between calls, both views simply hold the same
//!    bytes, the way any two `mmap`s of one `MAP_SHARED` object do.
//! 3. **Raw-pointer, unaligned accesses on the map side.** The
//!    reservation's second view is read and written through raw
//!    pointers ([`std::ptr::read_unaligned`]/`write_unaligned`), not a
//!    Rust reference -- so there is no aliasing-`&mut` UB even though a
//!    second, independently-addressed view of the same bytes exists.
//!    The primary view *is* a Rust `&mut [u8]` (borrowed out to
//!    `MachineBus`, same as the old `Box`/`Vec`), but by (2) nothing
//!    reads or writes through the reservation's raw pointers while that
//!    `&mut` is concurrently in use by a `MachineBus` call in progress
//!    on the same thread -- and there is only one thread.
//!
//! This is the same shape `bus.rs`'s own `fast_mem()` `SAFETY` comment
//! argues for `FastMem`'s raw pointer into fast RAM's *single* mapping;
//! here there are two mappings of the same pages instead of one, but
//! the "never both touched mid-access, single thread" argument is
//! identical.
//!
//! # ROM
//!
//! Kickstart bytes are immutable after load. `DirectMap::new` maps
//! anonymous `PROT_READ|PROT_WRITE` at `reservation + ROM_BASE`,
//! `memcpy`s the ROM image in, then `mprotect`s the region
//! `PROT_READ`-only. Pages there are only typed [`PageType::Rom`] when
//! the image's length exactly equals `ROM_WINDOW_SIZE`; an undersized
//! ROM needs `machine_core::rom::read_mirrored`'s wraparound, which the
//! direct path does not implement, so those pages are left
//! [`PageType::Io`] instead and every access on them falls through to
//! `MachineBus` (see "Real-ROM evidence" in the module's test comments
//! for why this matters in practice: every ROM this project boots is
//! exactly `ROM_WINDOW_SIZE`, so the direct path *is* exercised on every
//! real-ROM gate, but the fallback exists for a deliberately-undersized
//! test image). The read-only mapping can never `SIGBUS` because the
//! access-rule side (`bus.rs`) never routes a write to a `Rom` page
//! through the reservation -- see "Access rules" there.
//!
//! # Ext ROM window
//!
//! `machine_core::rom::EXT_ROM_BASE..+EXT_ROM_WINDOW_SIZE` is typed
//! [`PageType::Io`] unconditionally, not only when `--ext-rom` was
//! passed. Simpler than threading "was an ext ROM given" through this
//! module, and correct either way: when no ext ROM is attached,
//! `MachineBus` already serves that range as open bus itself (falling
//! through here costs nothing but the fallback dispatch), and when one
//! is attached, `Io` is exactly right.
//!
//! # VRAM is deliberately not `Ram` in this slice
//!
//! §4.6's eventual state maps graphics VRAM (Graffity, `rtgboard`) as
//! `Ram` too. Not done here: those devices have read/write side effects
//! (the planar renderer and the RTG board both read VRAM through
//! `MachineBus`, and a card's own registers share the same AUTOCONFIG
//! window shape as its VRAM in places), so mapping them into the direct
//! path would need a real audit of which sub-ranges are pure storage
//! versus register windows that this slice does not do. Every
//! AUTOCONFIG-placed board other than the one matching
//! `MachineBus::fast_ram_window()` is typed [`PageType::Io`], VRAM
//! included.
//!
//! # Failure posture
//!
//! Any `shm_open`/`mmap`/`mprotect` failure at construction is reported
//! with `eprintln!` and causes the caller (`run.rs`) to fall back to the
//! pre-existing `Box`/`Vec` allocation path with the direct map disabled
//! (`Bus`'s direct-map field stays `None`) -- never a panic mid-run.
//! Every address this module's hot path handles is a hostile-input `u32`
//! (guest-controlled): every arithmetic step uses `checked_add`, and an
//! overflow (an access whose last byte would wrap past `0xFFFF_FFFF`)
//! falls through to `MachineBus`, which already has its own wraparound
//! handling.

use std::ptr::NonNull;

use machine_core::{
    autoconfig, rom as rom_module, MachineBus, CHIP_RAM_BASE, CHIP_RAM_END, CHIP_RAM_SIZE,
    CIA_BASE, CIA_END, CUSTOM_BASE, OVERLAY_END, ROM_BASE, ROM_WINDOW_SIZE,
};

/// Bits of a guest address that select a 64 KiB page.
const PAGE_BITS: u32 = 16;
/// One page-type table entry per 64 KiB of the 32-bit guest address
/// space: `2^32 / 2^16 = 65536`.
pub const PAGE_COUNT: usize = 1 << (32 - PAGE_BITS);
/// The whole 4 GiB guest address space, as a host reservation size.
/// `usize` is 64-bit on every host this module runs on (macOS/POSIX,
/// module docs above); this module is not built for a 32-bit host.
const RESERVATION_LEN: usize = 1usize << 32;

#[inline]
pub(crate) fn page_of(addr: u32) -> usize {
    (addr >> PAGE_BITS) as usize
}

#[inline]
pub(crate) fn page_base(page: usize) -> u32 {
    (page as u32) << PAGE_BITS
}

/// One 64 KiB guest page's classification. See the module docs' "Page-
/// type table" section for the invariant each variant carries.
///
/// This type (and the table-building logic below, [`build_page_table`])
/// is also the classifier `crate::replay`'s recorder and player share
/// (`docs/cpu-core-proposal.md` §5.3, `docs/replay-log.md`): a page-type
/// table is exactly what both need to decide "pass through/RAM/ROM/open
/// bus, or log/replay an I/O access", and it would be wrong to grow a
/// second, independent classifier for the same job this module already
/// does correctly. Neither the direct map's mmap reservation nor its
/// `PROT_NONE` semantics are implied by the type itself -- only
/// [`DirectMap`] attaches that meaning to `Ram`/`Rom` vs `Io`/`OpenBus`;
/// `crate::replay` attaches a different one (serve from `GuestMemory`/log
/// vs pass through/log) to the same four variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageType {
    /// Backed by real, writable host memory in the reservation (chip
    /// RAM, or the AUTOCONFIG board matching
    /// [`MachineBus::fast_ram_window`]).
    Ram,
    /// Backed by real, read-only host memory in the reservation (the
    /// Kickstart copy, only when it exactly fills `ROM_WINDOW_SIZE`).
    Rom,
    /// `PROT_NONE` in the reservation; every access falls through to
    /// [`MachineBus`].
    Io,
    /// `PROT_NONE` in the reservation; reads are served inline as all-
    /// `1` bits and writes are swallowed inline, without touching
    /// [`MachineBus`] at all (`bus.rs`'s access rules).
    OpenBus,
}

impl PageType {
    /// Stable one-byte encoding for `crate::replay`'s log format (the
    /// header's initial table and every `ClassificationTransition`
    /// event). Never reordered -- a recorded log's bytes must decode the
    /// same way regardless of which future variant order this enum ends
    /// up with in source.
    pub fn to_code(self) -> u8 {
        match self {
            PageType::Ram => 0,
            PageType::Rom => 1,
            PageType::Io => 2,
            PageType::OpenBus => 3,
        }
    }

    /// Inverse of [`Self::to_code`]. An unrecognised code (a corrupted or
    /// truncated log, or a future version this build predates) decodes as
    /// [`PageType::Io`] -- the safe, "fall through and ask the log"
    /// choice, never `Ram`/`Rom` (which could otherwise mis-serve a
    /// corrupted table's address as real memory) and never `OpenBus`
    /// (which would silently swallow what might actually be logged
    /// traffic).
    pub fn from_code(code: u8) -> PageType {
        match code {
            0 => PageType::Ram,
            1 => PageType::Rom,
            3 => PageType::OpenBus,
            _ => PageType::Io,
        }
    }
}

/// One anonymous POSIX shared-memory region: a random `shm_open` name,
/// unlinked immediately, `ftruncate`d to `len`, with one `MAP_SHARED`
/// primary view. See the module docs' "The RAM aliasing argument"
/// section for the full safety case; this type owns only the primary
/// view and the fd, not any alias mapped from it into a
/// [`DirectMap`]'s reservation (aliases are `MAP_FIXED` into memory the
/// reservation itself owns, and go away when the reservation is
/// unmapped, not when this region is dropped).
pub struct ShmRegion {
    fd: libc::c_int,
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: `ShmRegion` has no interior mutability of its own and every
// access to `ptr` in this crate happens from the single thread that owns
// the run loop (module docs, condition 1). It is `Send` because the raw
// parts (`fd`, `ptr`) are transferable to another thread that then
// becomes the sole owner. Not `Sync`: the module docs' aliasing
// argument is single-thread-only, not "not accessed concurrently by
// construction from multiple threads."
unsafe impl Send for ShmRegion {}

impl ShmRegion {
    /// Create a new anonymous shared-memory region of exactly `len`
    /// bytes (must already be a multiple of the host page size -- every
    /// caller here sizes in whole megabytes, always a multiple of any
    /// real host page size). Returns `Err` with a human-readable reason
    /// on any failure; never panics (module docs' failure posture).
    pub fn create(len: usize) -> Result<Self, String> {
        // A short, sufficiently-random name: PID plus a monotonic
        // counter, well under macOS's `PSHMNAMLEN` (31 bytes including
        // the leading '/'). `O_EXCL` makes a name collision an error
        // rather than silently reusing someone else's region; astronomically
        // unlikely with a pid+counter name, but checked regardless.
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("/m68kmach{}{:x}\0", std::process::id(), n);

        let fd = unsafe {
            libc::shm_open(
                name.as_ptr() as *const libc::c_char,
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(format!(
                "shm_open({}): {}",
                &name[..name.len() - 1],
                std::io::Error::last_os_error()
            ));
        }
        // Unlink immediately: the object's lifetime from here on is
        // purely "at least one open fd or mapping exists," never a
        // filesystem-namespace name another process could observe.
        unsafe {
            libc::shm_unlink(name.as_ptr() as *const libc::c_char);
        }
        if unsafe { libc::ftruncate(fd, len as libc::off_t) } != 0 {
            let e = std::io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(format!("ftruncate({len}): {e}"));
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            let e = std::io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(format!("mmap primary view ({len} bytes): {e}"));
        }
        // SAFETY: `mmap` succeeded (checked above), so `ptr` is a valid,
        // non-null mapping of `len` bytes.
        let ptr = unsafe { NonNull::new_unchecked(ptr as *mut u8) };
        Ok(ShmRegion { fd, ptr, len })
    }

    /// The primary `MAP_SHARED` view, as the `&mut [u8]` `MachineBus`
    /// borrows -- exactly the role a `Box<[u8; N]>`/`Vec<u8>` played
    /// before this module existed.
    ///
    /// # Safety
    /// The caller must not let this borrow outlive `self`, and must
    /// uphold the module docs' aliasing argument (no concurrent access
    /// through a reservation alias of the same fd while this borrow is
    /// in use, which holds here because both are only ever touched from
    /// `run_guest*`'s single thread, never concurrently within one
    /// access).
    pub unsafe fn as_mut_slice(&mut self) -> &'static mut [u8] {
        // Erasing the lifetime to `'static` here mirrors the shape
        // `run.rs` already had before this module existed: `chip_ram`/
        // `fast_ram` were locals borrowed `&'a mut` into `MachineBus`
        // for the rest of `run()`'s scope, and `ShmRegion` is itself
        // such a local now, living exactly as long as those buffers did
        // (declared before `bus`, dropped after it -- see `run.rs`).
        // Callers must not use this to extend the slice's real lifetime
        // past `self`'s own.
        std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len)
    }

    pub fn fd(&self) -> libc::c_int {
        self.fd
    }
}

impl Drop for ShmRegion {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` describe the mapping this `ShmRegion`
        // created and uniquely owns; nothing else in this crate holds a
        // pointer derived from `ptr` past this point (the primary
        // slice's erased-lifetime borrow is documented as not outliving
        // `self`; reservation aliases are independent mappings of the
        // same underlying shm object and are unaffected by unmapping
        // this one).
        unsafe {
            libc::munmap(self.ptr.as_ptr() as *mut libc::c_void, self.len);
            libc::close(self.fd);
        }
    }
}

/// What changed between two [`DirectMap::sync`] calls that forces a
/// full page-type-table and fast-RAM-alias rebuild (module docs' "Rebuild
/// on placement change"). Deliberately allocation-free and cheap to
/// compare -- a fixed-size array of `Copy` data, not a `Vec`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    overlay: bool,
    placements: [Option<(u32, u32)>; autoconfig::MAX_BOARDS],
    fast_window: Option<(u32, u32)>,
}

impl Fingerprint {
    pub(crate) fn of(bus: &MachineBus) -> Self {
        let mut placements = [None; autoconfig::MAX_BOARDS];
        for (i, slot) in placements.iter_mut().enumerate() {
            *slot = bus.autoconfig.placement(i).map(|c| (c.base, c.size_bytes));
        }
        Fingerprint {
            overlay: bus.overlay(),
            placements,
            fast_window: bus.fast_ram_window(),
        }
    }
}

/// Pure page-type-table construction from `bus`'s current state --
/// factored out of [`DirectMap::rebuild`] (`docs/cpu-core-proposal.md`
/// §5.3's C1 replay harness, `crate::replay`) so both callers -- the
/// direct map, and the recorder/player's own classifier -- run exactly
/// one classification rule set, never two that could quietly drift apart.
/// Allocates and returns a fresh table rather than mutating one in place;
/// [`DirectMap::rebuild`] is now a two-line wrapper around this, and its
/// own behaviour (and its differential/unit test coverage) is unchanged
/// by the refactor.
///
/// `fast_ram_mapped` is the fast-RAM extent to type `Ram`, distinct from
/// `bus.fast_ram_window()`'s bare declared/clamped window: [`DirectMap`]
/// passes its own `fast_alias` (the actually-mapped, 64 KiB-page-rounded
/// mmap alias -- see [`DirectMap::sync`]'s own comment on why a partial
/// trailing page must stay `Io` on that path, since a raw pointer read
/// past the real buffer would be undefined behaviour there). `crate::replay`'s
/// recorder/player have no mmap alias to round -- they serve `Ram` pages
/// through [`machine_core::GuestMemory::ram_slice`], which already clips
/// to the real buffer and returns `None` rather than reading out of
/// bounds -- so they pass `bus.fast_ram_window()` directly, unrounded.
/// Either choice produces the same table when the window's length is
/// already a whole number of 64 KiB pages, which is the common case;
/// they can differ only in the sub-page tail of an unusually-sized fast
/// RAM board, where the direct map's caller-provided extent controls
/// what's safe to type `Ram` and the replay caller's own contract (fail
/// closed via `Option`, never `unsafe`) makes rounding unnecessary.
///
/// Precedence: fixed architectural ranges first, then every
/// AUTOCONFIG-placed board (which can, and for VRAM boards deliberately
/// does, retype a page a fixed rule already touched -- see the
/// CIA/custom/AUTOCONFIG-window ranges below, none of which any real
/// board is ever placed over, so in practice there is no actual overlap
/// on this machine's map, but the precedence is still fixed-then-placed
/// so a hypothetical future overlap resolves predictably).
pub fn build_page_table(
    bus: &MachineBus,
    rom_is_full_window: bool,
    fast_ram_mapped: Option<(u32, u32)>,
) -> Box<[PageType; PAGE_COUNT]> {
    let mut pages = Box::new([PageType::OpenBus; PAGE_COUNT]);

    // Chip RAM, split by the overlay flag: while it's mapped, the
    // low `OVERLAY_END` bytes read ROM and write chip RAM
    // (`MachineBus` handles both sides of that already), so those
    // pages are `Io`, not `Ram`, while overlay is active.
    // `OVERLAY_END` is an exact 64 KiB page boundary (`0x80000` =
    // eight pages), so this split never needs a partial-page case.
    let overlay = bus.overlay();
    for page in page_of(CHIP_RAM_BASE)..page_of(CHIP_RAM_END) {
        let in_overlay = overlay && page_base(page) < OVERLAY_END;
        pages[page] = if in_overlay {
            PageType::Io
        } else {
            PageType::Ram
        };
    }

    for page in page_of(CIA_BASE)..page_of(CIA_END) {
        pages[page] = PageType::Io;
    }

    pages[page_of(CUSTOM_BASE)] = PageType::Io;

    for page in page_of(autoconfig::AUTOCONFIG_BASE)..page_of(autoconfig::AUTOCONFIG_END) {
        pages[page] = PageType::Io;
    }

    // Ext ROM window: `Io` unconditionally (module docs explain why
    // this is fine whether or not `--ext-rom` was actually given).
    let ext_rom_end = rom_module::EXT_ROM_BASE + rom_module::EXT_ROM_WINDOW_SIZE as u32;
    for page in page_of(rom_module::EXT_ROM_BASE)..page_of(ext_rom_end) {
        pages[page] = PageType::Io;
    }

    // ROM window: `Rom` only when the image exactly fills it
    // (module docs' "ROM" section); otherwise `Io`, so a read
    // through the reservation is never attempted and
    // `read_mirrored`'s wraparound is left to `MachineBus`.
    let rom_end = ROM_BASE + ROM_WINDOW_SIZE as u32;
    let rom_type = if rom_is_full_window {
        PageType::Rom
    } else {
        PageType::Io
    };
    for page in page_of(ROM_BASE)..page_of(rom_end) {
        pages[page] = rom_type;
    }

    // AUTOCONFIG-placed boards: iterate every configured slot, never
    // a constant. The board whose base matches
    // `bus.fast_ram_window()` is fast RAM; every other placed board
    // (hostblk, pktport, input, rtgboard, pcibridge, graphics VRAM)
    // is `Io` in this slice (module docs' "VRAM is deliberately not
    // Ram" section).
    let fast_window = bus.fast_ram_window();
    for i in 0..autoconfig::MAX_BOARDS {
        let Some(cfg) = bus.autoconfig.placement(i) else {
            continue;
        };
        let is_fast_ram = fast_window.is_some_and(|(fbase, _)| fbase == cfg.base);
        if is_fast_ram {
            // `Ram` typing must come from `fast_ram_mapped` -- what is
            // *actually* backed right now, per the caller's own contract
            // -- not from `bus.fast_ram_window()`'s declared/clamped
            // extent alone; see this function's own doc comment for why
            // the two callers pass different things here.
            let mapped = fast_ram_mapped
                .filter(|&(base, _)| base == cfg.base)
                .map_or(0, |(_, len)| len);
            let ram_end = cfg.base.saturating_add(mapped);
            for page in page_of(cfg.base)..page_of(ram_end) {
                if page < PAGE_COUNT {
                    pages[page] = PageType::Ram;
                }
            }
            // The remainder of the *declared* window beyond the
            // actually-mapped RAM part -- including a sub-page tail,
            // or (when nothing is mapped at all) the whole window --
            // stays `Io`, matching `MachineBus`'s own clamped-window
            // semantics.
            let decl_end = cfg.base.saturating_add(cfg.size_bytes);
            let io_start_page = page_of(ram_end);
            let io_end_page = page_of(decl_end.saturating_sub(1)).saturating_add(1);
            for page in io_start_page..io_end_page.min(PAGE_COUNT) {
                if pages[page] != PageType::Ram {
                    pages[page] = PageType::Io;
                }
            }
        } else {
            let end = cfg.base.saturating_add(cfg.size_bytes);
            let end_page = page_of(end.saturating_sub(1).max(cfg.base)).saturating_add(1);
            for page in page_of(cfg.base)..end_page.min(PAGE_COUNT) {
                pages[page] = PageType::Io;
            }
        }
    }

    pages
}

/// The direct address-space mapping: a 4 GiB `PROT_NONE` reservation,
/// the page-type table classifying it, and the RAII state needed to
/// remap the fast-RAM alias when AUTOCONFIG placement changes. See the
/// module docs for the full design.
pub struct DirectMap {
    reservation: NonNull<u8>,
    pages: Box<[PageType; PAGE_COUNT]>,
    fast_fd: Option<libc::c_int>,
    /// The fast-RAM alias currently mapped into the reservation
    /// (page-aligned base, page-aligned length), if any -- tracked so a
    /// placement change can `PROT_NONE` exactly that range before
    /// mapping the new one.
    fast_alias: Option<(u32, u32)>,
    rom_is_full_window: bool,
    fingerprint: Fingerprint,
}

// SAFETY: single-threaded use only (module docs, condition 1); the raw
// pointer is only ever dereferenced from the thread that owns the
// `DirectMap`.
unsafe impl Send for DirectMap {}

impl DirectMap {
    /// Build a fresh direct map: reserve the 4 GiB window, alias chip
    /// RAM in immediately, map and protect the ROM copy, and run the
    /// first full classification (module docs' "Rebuild on placement
    /// change": "sync is called once at construction"). `chip_fd` and
    /// `fast_fd` are the shared-memory fds `run.rs` already created for
    /// chip/fast RAM's primary views; `rom` is the exact bytes loaded
    /// into `MachineBus`.
    ///
    /// Returns `Err` with a human-readable reason on any failure
    /// (module docs' failure posture) -- the caller falls back to the
    /// disabled path.
    pub fn new(
        chip_fd: libc::c_int,
        fast_fd: Option<libc::c_int>,
        rom: &[u8],
        bus: &MachineBus,
    ) -> Result<Self, String> {
        let reservation = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                RESERVATION_LEN,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if reservation == libc::MAP_FAILED {
            return Err(format!(
                "reserving 4 GiB guest address space: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: `mmap` succeeded, so this is a valid non-null base of
        // a `RESERVATION_LEN`-byte mapping this `DirectMap` now owns.
        let reservation = unsafe { NonNull::new_unchecked(reservation as *mut u8) };

        let mut dm = DirectMap {
            reservation,
            pages: Box::new([PageType::OpenBus; PAGE_COUNT]),
            fast_fd,
            fast_alias: None,
            rom_is_full_window: rom.len() == ROM_WINDOW_SIZE,
            // A sentinel that cannot equal any real `Fingerprint::of`
            // result on its own is unnecessary: `sync` below always
            // does the first rebuild unconditionally by construction
            // (see its own doc comment), so what this starts as does
            // not matter.
            fingerprint: Fingerprint {
                overlay: false,
                placements: [None; autoconfig::MAX_BOARDS],
                fast_window: None,
            },
        };

        // Chip RAM's alias is fixed for the process's whole lifetime
        // (chip RAM never moves), so it is mapped once here rather than
        // redone on every `sync`.
        if let Err(e) = dm.alias(chip_fd, CHIP_RAM_BASE, CHIP_RAM_SIZE as u32) {
            dm.unmap_reservation();
            return Err(format!("aliasing chip RAM into the reservation: {e}"));
        }

        if let Err(e) = dm.map_rom(rom) {
            dm.unmap_reservation();
            return Err(format!("mapping ROM into the reservation: {e}"));
        }

        // First classification: unconditional, not gated on a
        // fingerprint change (see `sync`'s own doc comment on why the
        // sentinel above doesn't need to be a real non-match).
        dm.rebuild(bus);
        dm.fingerprint = Fingerprint::of(bus);

        Ok(dm)
    }

    fn unmap_reservation(&mut self) {
        unsafe {
            libc::munmap(
                self.reservation.as_ptr() as *mut libc::c_void,
                RESERVATION_LEN,
            );
        }
    }

    /// `MAP_FIXED|MAP_SHARED` of `fd` into the reservation at guest
    /// address `base`, length `len` -- used for both the chip-RAM alias
    /// (once, in [`Self::new`]) and the fast-RAM alias (in [`Self::sync`]
    /// on a placement change).
    fn alias(&self, fd: libc::c_int, base: u32, len: u32) -> Result<(), String> {
        if len == 0 {
            return Ok(());
        }
        let dst = unsafe { self.reservation.as_ptr().add(base as usize) };
        let ptr = unsafe {
            libc::mmap(
                dst as *mut libc::c_void,
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_FIXED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }

    /// Reset `base..base+len` in the reservation back to `PROT_NONE`,
    /// without disturbing the surrounding reservation (a fresh anonymous
    /// `MAP_FIXED` mapping over just that range, not an `munmap` --
    /// `munmap`ing a hole would leave that range available for something
    /// else to claim by coincidence, which a `PROT_NONE` remap avoids).
    fn unalias(&self, base: u32, len: u32) {
        if len == 0 {
            return;
        }
        let dst = unsafe { self.reservation.as_ptr().add(base as usize) };
        unsafe {
            libc::mmap(
                dst as *mut libc::c_void,
                len as usize,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED,
                -1,
                0,
            );
        }
    }

    fn map_rom(&self, rom: &[u8]) -> Result<(), String> {
        let dst = unsafe { self.reservation.as_ptr().add(ROM_BASE as usize) };
        let ptr = unsafe {
            libc::mmap(
                dst as *mut libc::c_void,
                ROM_WINDOW_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let n = rom.len().min(ROM_WINDOW_SIZE);
        unsafe {
            std::ptr::copy_nonoverlapping(rom.as_ptr(), ptr as *mut u8, n);
        }
        if unsafe { libc::mprotect(ptr, ROM_WINDOW_SIZE, libc::PROT_READ) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }

    /// Recompute the fingerprint from `bus` and, if it changed since the
    /// last call (or this is the first call from [`Self::new`]), do a
    /// full rebuild: retype every page and, if fast RAM's placement
    /// changed, remap its alias. Called once at construction and then
    /// after every write `Bus` routes to `MachineBus` (the slow path) --
    /// see `bus.rs`'s access rules for why that trigger point catches
    /// every transition with no missed window.
    pub fn sync(&mut self, bus: &MachineBus) {
        let fp = Fingerprint::of(bus);
        if fp == self.fingerprint {
            return;
        }
        if fp.fast_window != self.fingerprint.fast_window {
            if let Some((base, len)) = self.fast_alias.take() {
                self.unalias(base, len);
            }
            if let (Some(fd), Some((base, len))) = (self.fast_fd, fp.fast_window) {
                // Round the clamped window DOWN to a whole 64 KiB page:
                // a partial trailing page cannot be mapped `Ram` (the
                // reservation only maps whole pages), so any remainder
                // is left `Io` by `rebuild` below and served through
                // `MachineBus` instead.
                let mapped_len = len - (len % (1 << PAGE_BITS));
                if mapped_len > 0 {
                    if let Err(e) = self.alias(fd, base, mapped_len) {
                        eprintln!(
                            "direct map: aliasing fast RAM at {base:#010x} ({mapped_len} bytes) \
                             failed, leaving those pages Io: {e}"
                        );
                    } else {
                        self.fast_alias = Some((base, mapped_len));
                    }
                }
            }
        }
        self.rebuild(bus);
        self.fingerprint = fp;
    }

    /// Full page-type-table rebuild from `bus`'s current state --
    /// delegates to [`build_page_table`], passing `self.fast_alias` as
    /// the "actually mapped" extent (see that function's own doc comment
    /// for why this must be the mmap alias, not the bare declared/clamped
    /// window). No behaviour change from before this was factored out:
    /// this is the same computation, now shared with `crate::replay`.
    fn rebuild(&mut self, bus: &MachineBus) {
        self.pages = build_page_table(bus, self.rom_is_full_window, self.fast_alias);
    }

    #[inline]
    fn page_type(&self, addr: u32) -> PageType {
        self.pages[page_of(addr)]
    }

    #[inline]
    fn reservation_ptr(&self, addr: u32) -> *mut u8 {
        // SAFETY (of the pointer's validity, not yet of dereferencing
        // it): `addr` is a `u32`, so `addr as usize` is always strictly
        // less than `RESERVATION_LEN` (2^32), and `reservation` is a
        // `RESERVATION_LEN`-byte mapping -- so this pointer is always
        // in-bounds of the reservation, whether or not the page it
        // points into happens to be `PROT_NONE`.
        unsafe { self.reservation.as_ptr().add(addr as usize) }
    }

    /// Classify a `width`-byte access at `addr` by its first and last
    /// byte's page. `None` on address overflow (the last byte would
    /// wrap past `0xFFFF_FFFF`) -- callers fall through to `MachineBus`
    /// on `None`, which already handles wraparound.
    #[inline]
    fn access_pages(&self, addr: u32, width: u32) -> Option<(PageType, PageType)> {
        let last = addr.checked_add(width - 1)?;
        Some((self.page_type(addr), self.page_type(last)))
    }
}

/// Outcome of a direct-path read attempt: either the composed value, an
/// open-bus read served inline, or "not handled here" (fall through to
/// `MachineBus`).
#[derive(Debug)]
pub enum ReadOutcome<T> {
    Ram(T),
    OpenBus(T),
    Fallthrough,
}

/// Outcome of a direct-path write attempt.
#[derive(Debug)]
pub enum WriteOutcome {
    Ram,
    OpenBus,
    Fallthrough,
}

macro_rules! read_impl {
    ($name:ident, $t:ty, $width:expr, $openbus:expr) => {
        /// Serve a read of this width through the direct map, if
        /// possible. See the module docs' "Access rules" summary
        /// (mirrored fully in `bus.rs`).
        pub fn $name(&self, addr: u32) -> ReadOutcome<$t> {
            let Some((t0, t1)) = self.access_pages(addr, $width) else {
                return ReadOutcome::Fallthrough;
            };
            let ram_ok = |t: PageType| matches!(t, PageType::Ram | PageType::Rom);
            if ram_ok(t0) && ram_ok(t1) {
                // SAFETY: both the first and last byte's pages are
                // `Ram`/`Rom`, so by the page-type invariant the whole
                // `$width`-byte span (which lies entirely within the
                // reservation, `access_pages`/`checked_add` above) is
                // mapped and readable. Unaligned load: guest accesses
                // are not required to be host-aligned.
                let bytes: [u8; $width as usize] =
                    unsafe { std::ptr::read_unaligned(self.reservation_ptr(addr) as *const _) };
                return ReadOutcome::Ram(<$t>::from_be_bytes(bytes));
            }
            if matches!(t0, PageType::OpenBus) && matches!(t1, PageType::OpenBus) {
                return ReadOutcome::OpenBus($openbus);
            }
            ReadOutcome::Fallthrough
        }
    };
}

macro_rules! write_impl {
    ($name:ident, $t:ty, $width:expr) => {
        /// Serve a write of this width through the direct map, if
        /// possible.
        pub fn $name(&mut self, addr: u32, value: $t) -> WriteOutcome {
            let Some((t0, t1)) = self.access_pages(addr, $width) else {
                return WriteOutcome::Fallthrough;
            };
            if matches!(t0, PageType::Ram) && matches!(t1, PageType::Ram) {
                let bytes = value.to_be_bytes();
                // SAFETY: both pages are `Ram` (never `Rom` -- a write
                // never targets the read-only ROM alias), so the whole
                // span is mapped read-write. Unaligned store, same
                // reasoning as the read side.
                unsafe {
                    std::ptr::write_unaligned(self.reservation_ptr(addr) as *mut _, bytes);
                }
                return WriteOutcome::Ram;
            }
            if matches!(t0, PageType::OpenBus) && matches!(t1, PageType::OpenBus) {
                return WriteOutcome::OpenBus;
            }
            WriteOutcome::Fallthrough
        }
    };
}

impl DirectMap {
    read_impl!(read_byte, u8, 1u32, 0xFFu8);
    read_impl!(read_word, u16, 2u32, 0xFFFFu16);
    read_impl!(read_long, u32, 4u32, 0xFFFF_FFFFu32);
    write_impl!(write_byte, u8, 1u32);
    write_impl!(write_word, u16, 2u32);
    write_impl!(write_long, u32, 4u32);

    /// Exposed for tests: the current type of the page containing
    /// `addr`.
    #[cfg(test)]
    pub fn page_type_at(&self, addr: u32) -> PageType {
        self.page_type(addr)
    }
}

impl Drop for DirectMap {
    fn drop(&mut self) {
        self.unmap_reservation();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machine_core::{CHIP_RAM_SIZE, ROM_WINDOW_SIZE};

    fn synthetic_rom() -> Vec<u8> {
        let mut rom = vec![0u8; ROM_WINDOW_SIZE];
        // A minimal but distinguishable pattern -- these tests never
        // boot it, only read bytes back through the two paths.
        for (i, b) in rom.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        rom
    }

    /// `chip`'s fd, captured before it's borrowed mutably below -- once
    /// `MachineBus` holds the `&mut [u8; CHIP_RAM_SIZE]` borrow, `chip`
    /// itself can no longer be touched (even a read-only `fd()` call)
    /// for as long as that `MachineBus` (or anything built from it) is
    /// alive, so every caller here grabs the fd first.
    fn new_chip_and_bus<'a>(chip: &'a mut ShmRegion, rom: &'a [u8]) -> MachineBus<'a> {
        // SAFETY: test-only, single-threaded, `chip` outlives the
        // returned `MachineBus`.
        let slice: &mut [u8; CHIP_RAM_SIZE] = unsafe { chip.as_mut_slice() }.try_into().unwrap();
        MachineBus::new(slice, rom)
    }

    #[test]
    fn chip_ram_pages_are_ram_and_overlay_pages_are_io() {
        let rom = synthetic_rom();
        let mut chip = ShmRegion::create(CHIP_RAM_SIZE).unwrap();
        let chip_fd = chip.fd();
        let bus = new_chip_and_bus(&mut chip, &rom);
        assert!(bus.overlay(), "MachineBus::new starts with overlay mapped");
        let dm = DirectMap::new(chip_fd, None, &rom, &bus).unwrap();
        assert_eq!(dm.page_type_at(0), PageType::Io, "overlay page");
        assert_eq!(dm.page_type_at(OVERLAY_END), PageType::Ram);
        assert_eq!(dm.page_type_at(CHIP_RAM_END - 1), PageType::Ram);
    }

    #[test]
    fn overlay_clear_retypes_low_chip_pages() {
        let rom = synthetic_rom();
        let mut chip = ShmRegion::create(CHIP_RAM_SIZE).unwrap();
        let chip_fd = chip.fd();
        let mut bus = new_chip_and_bus(&mut chip, &rom);
        let mut dm = DirectMap::new(chip_fd, None, &rom, &bus).unwrap();
        assert_eq!(dm.page_type_at(0), PageType::Io);
        // Clearing the overlay is a real guest action (a CIA-A PRA
        // write, bit 0) -- driven the same way `machine-core`'s own
        // tests do it, never by poking a private field.
        bus.write_byte(0x00BF_E001, 0x00);
        assert!(!bus.overlay());
        dm.sync(&bus);
        assert_eq!(dm.page_type_at(0), PageType::Ram);
        assert_eq!(dm.page_type_at(OVERLAY_END), PageType::Ram);
    }

    #[test]
    fn rom_pages_are_rom_when_image_matches_the_window() {
        let rom = synthetic_rom();
        let mut chip = ShmRegion::create(CHIP_RAM_SIZE).unwrap();
        let chip_fd = chip.fd();
        let bus = new_chip_and_bus(&mut chip, &rom);
        let dm = DirectMap::new(chip_fd, None, &rom, &bus).unwrap();
        assert_eq!(dm.page_type_at(ROM_BASE), PageType::Rom);
        assert_eq!(
            dm.page_type_at(ROM_BASE + ROM_WINDOW_SIZE as u32 - 1),
            PageType::Rom
        );
    }

    #[test]
    fn undersized_rom_stays_io() {
        let mut rom = synthetic_rom();
        rom.truncate(ROM_WINDOW_SIZE / 4);
        let mut chip = ShmRegion::create(CHIP_RAM_SIZE).unwrap();
        let chip_fd = chip.fd();
        let bus = new_chip_and_bus(&mut chip, &rom);
        let dm = DirectMap::new(chip_fd, None, &rom, &bus).unwrap();
        assert_eq!(dm.page_type_at(ROM_BASE), PageType::Io);
    }

    #[test]
    fn open_bus_reads_and_writes_are_inline() {
        let rom = synthetic_rom();
        let mut chip = ShmRegion::create(CHIP_RAM_SIZE).unwrap();
        let chip_fd = chip.fd();
        let bus = new_chip_and_bus(&mut chip, &rom);
        let dm = DirectMap::new(chip_fd, None, &rom, &bus).unwrap();
        // Deep open bus: well past every fixed range and never placed.
        let addr = 0x0050_0000;
        assert_eq!(dm.page_type_at(addr), PageType::OpenBus);
        match dm.read_byte(addr) {
            ReadOutcome::OpenBus(v) => assert_eq!(v, 0xFF),
            _ => panic!("expected OpenBus, got a different outcome (variant only)"),
        }
    }

    #[test]
    fn fast_ram_placement_maps_its_alias_and_clamped_window_leaves_tail_io() {
        let rom = synthetic_rom();
        let mut chip = ShmRegion::create(CHIP_RAM_SIZE).unwrap();
        let chip_fd = chip.fd();
        // A deliberately small backing buffer so the AUTOCONFIG-declared
        // window (rounded up to the board's own size class) is larger
        // than what's actually backed -- exercising the "declared >
        // buffer, tail stays Io" clamp.
        let fast_len = 256 * 1024; // 256 KiB real buffer
        let mut fast = ShmRegion::create(fast_len).unwrap();
        let fast_fd = fast.fd();
        let bus = new_chip_and_bus(&mut chip, &rom);
        // SAFETY: test-only, single-threaded, `fast` outlives `bus`.
        let fast_slice: &mut [u8] = unsafe { fast.as_mut_slice() };
        let mut bus = bus.with_fast_ram(fast_slice);
        let mut dm = DirectMap::new(chip_fd, Some(fast_fd), &rom, &bus).unwrap();

        // Drive AUTOCONFIG the way the guest would: the board is first
        // in the chain (nothing else added here), Zorro III since
        // `fastram`'s spec is a Zorro III board (`with_fast_ram`'s own
        // module docs) -- write the Z3 base address sequence.
        use machine_core::autoconfig::{ec, AUTOCONFIG_BASE};
        // EC_Z3_BASEADDRESS is a 16-bit register delivered as two byte
        // writes (autoconfig.rs's own module docs): high byte at
        // `ec::Z3_BASEADDRESS`, low byte at `+ 1`, which latches and
        // configures. Values chosen to land the board at 0x4000_0000.
        bus.write_byte(AUTOCONFIG_BASE + ec::Z3_BASEADDRESS, 0x40); // hi
        bus.write_byte(AUTOCONFIG_BASE + ec::Z3_BASEADDRESS + 1, 0x00); // lo -> configure
        dm.sync(&bus);

        let base = 0x4000_0000u32;
        assert_eq!(dm.page_type_at(base), PageType::Ram);
        // The clamped, page-rounded RAM region is 256 KiB = 4 pages.
        assert_eq!(dm.page_type_at(base + 256 * 1024 - 1), PageType::Ram);
        // Beyond the real buffer, within the declared window: Io.
        assert_eq!(dm.page_type_at(base + 256 * 1024), PageType::Io);
    }

    /// Degradation path (a) from the review that found this bug: fast
    /// RAM's shm region failed to be created at all (`run.rs` falls
    /// back to a plain `Vec` for it in that case, `fast_fd = None`
    /// reaching `DirectMap::new`/`sync`), but the board is still placed
    /// by AUTOCONFIG (`bus.fast_ram_window()` is `Some`) -- the same
    /// state `sync`'s own `alias()`-failure branch leaves behind, too
    /// (its `eprintln!` says "leaving those pages Io", which was not
    /// true before this fix: `rebuild` typed them `Ram` from the window
    /// regardless of whether an alias existed, so the first guest access
    /// through the direct path dereferenced `PROT_NONE` and crashed the
    /// process instead of falling through to `MachineBus`). With no
    /// alias ever mapped, the placed window must stay `Io` end to end,
    /// and a direct-path read there must report [`ReadOutcome::Fallthrough`],
    /// never touch the reservation.
    #[test]
    fn fast_ram_placed_with_no_shm_region_stays_io_and_falls_through() {
        let rom = synthetic_rom();
        let mut chip = ShmRegion::create(CHIP_RAM_SIZE).unwrap();
        let chip_fd = chip.fd();
        // A real backing buffer still exists on the `MachineBus` side --
        // `with_fast_ram` needs one, and `run.rs`'s own fallback for
        // this exact scenario is a plain `Vec`, not "no fast RAM at
        // all" -- but `DirectMap` never sees an fd for it.
        let mut fast_vec = vec![0u8; 256 * 1024];
        let bus = new_chip_and_bus(&mut chip, &rom);
        let mut bus = bus.with_fast_ram(&mut fast_vec[..]);
        let mut dm = DirectMap::new(chip_fd, None, &rom, &bus).unwrap();

        use machine_core::autoconfig::{ec, AUTOCONFIG_BASE};
        bus.write_byte(AUTOCONFIG_BASE + ec::Z3_BASEADDRESS, 0x40);
        bus.write_byte(AUTOCONFIG_BASE + ec::Z3_BASEADDRESS + 1, 0x00);
        dm.sync(&bus);

        let base = 0x4000_0000u32;
        assert!(
            bus.fast_ram_window().is_some(),
            "the board must actually be placed for this test to exercise the bug"
        );
        assert_eq!(
            dm.page_type_at(base),
            PageType::Io,
            "no alias was ever mapped (fast_fd is None), so this page must not be Ram"
        );
        assert_eq!(dm.page_type_at(base + 256 * 1024 - 1), PageType::Io);
        match dm.read_word(base) {
            ReadOutcome::Fallthrough => {}
            other => panic!(
                "expected Fallthrough (no reservation access), got a Ram/OpenBus outcome instead: \
                 {other:?}"
            ),
        }
    }
}
