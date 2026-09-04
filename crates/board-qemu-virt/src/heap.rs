//! A minimal bump allocator, present only because linking the `m68k` fork's
//! `no_std` build pulls in `alloc` (`Vec`/`Box` in its decode tables and
//! `CpuCore`, per the fork's `Cargo.toml` doc comment: "the downstream
//! binary must then supply a `#[global_allocator]`"). The fork's own docs
//! say only `CpuCore::run_batch` allocates at runtime and this payload
//! never calls it (it uses `step`, like `tests/hello_guest.rs` does
//! hosted), but the `alloc` crate's symbols still need *something*
//! registered for the no_std binary to link at all.
//!
//! Never frees: a bump allocator is the right tool for "small, fixed, one-
//! shot smoke payload that never calls `run_batch`" and not for anything
//! longer-lived — there is no free list, so `dealloc` is a no-op and
//! repeated allocate/free cycles would exhaust the arena. That tradeoff is
//! fine here and would not be once a board layer actually runs a guest
//! continuously.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Heap arena size: generous headroom over what `CpuCore`'s decode tables
/// need, still tiny next of the 8 MB RAM region `link.ld` gives this image.
const HEAP_SIZE: usize = 256 * 1024;

#[repr(align(16))]
struct Arena(UnsafeCell<[u8; HEAP_SIZE]>);

// SAFETY: this payload is single-threaded (no other core is started, no
// interrupts are enabled), so a `static` cell accessed only through the
// allocator below is sound despite `UnsafeCell` not being `Sync` by
// default.
unsafe impl Sync for Arena {}

static ARENA: Arena = Arena(UnsafeCell::new([0; HEAP_SIZE]));

/// Bump allocator: hands out increasing offsets from `ARENA`, never
/// reclaims. See the module doc comment for why that is acceptable here.
struct BumpAllocator {
    next: AtomicUsize,
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator {
    next: AtomicUsize::new(0),
};

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let base = ARENA.0.get() as *mut u8 as usize;
        let mut current = self.next.load(Ordering::Relaxed);
        loop {
            let start = (base + current).next_multiple_of(layout.align()) - base;
            let end = start + layout.size();
            if end > HEAP_SIZE {
                return ptr::null_mut();
            }
            match self.next.compare_exchange_weak(
                current,
                end,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return (base + start) as *mut u8,
                Err(observed) => current = observed,
            }
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Bump allocator: nothing to reclaim, see module doc comment.
    }
}
