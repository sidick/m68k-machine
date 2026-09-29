// C1 signal-fault cost harness (`docs/cpu-core-proposal.md` §4.6, §8, §9 "Fault-driven
// I/O too slow on Linux").
//
// **This host is macOS (Darwin 25.6.19, Apple M3 Pro), not Linux.** §4.6 says in so many
// words: "Fault cost on the Linux backend is measured at C1 before this split is committed
// to." That Linux measurement is what actually gates the design decision (one-fault-per-site
// patching vs an inline per-access page-type-table check), and it is **still outstanding** --
// nothing here satisfies it. Every number this binary prints is a macOS number and is labelled
// as one. Treat it as a *likely upper bound*, not a substitute for the Linux figure: this
// process measures the `sigaction`/`SA_SIGINFO` path through xnu's Mach-exception-port-backed
// signal delivery, which is a different (and by reputation slower) mechanism than a Linux
// kernel's direct SIGSEGV/SIGBUS delivery off a hardware page-fault trap. If the macOS number
// already looks cheap relative to the inline check, that is *supporting* evidence for the
// fault-patching design; if it looks expensive, it is not by itself disqualifying, because
// Linux's real number could come in lower. Either way, someone still owes this project a Linux
// run of this same harness (or a Linux-native equivalent) before §4.6's split is committed to.
//
// # What each variant measures
//
// 1. `inline-check`   -- the thing faults compete against: a 65536-entry page-type byte table
//                         (one byte per 64 KiB of a simulated 4 GiB guest address space) plus
//                         a bounds-checked RAM read, vs. a raw read with no check.
// 2. `mprotect-toggle` -- the fault-as-backstop shape from §4.6: a write to a PROT_READ host
//                         page faults, the handler mprotects it PROT_READ|PROT_WRITE and
//                         returns (the kernel retries the faulting store), the main loop
//                         mprotects back to PROT_READ, repeat.
// 3. `pc-advance`      -- the shape §4.6 actually specifies for I/O sites: fault, service
//                         through the (simulated) device model, and skip the instruction by
//                         advancing the saved PC in the ucontext, never leaving PROT_READ.
//                         This is the *cheaper* of the two round-trip shapes because it does
//                         zero extra mprotect syscalls, so it is the one to prefer if §4.6's
//                         design proceeds.
// 4. `shared-ff`       -- a functional (not perf) check of §4.6's open-bus read fast path: one
//                         64 KiB, 0xFF-filled shared-memory object mapped read-only at several
//                         aliased addresses.
// 5. `all` (default)   -- runs 1-4 in sequence and prints the "N inline checks per fault"
//                         comparison called for by the task.
//
// # Why batched timing, not per-op
//
// Variant 1 runs >=100M iterations per timed run; timestamping every access would dominate the
// measurement. `std::time::Instant` brackets the whole batch and `std::hint::black_box` keeps
// the optimizer from folding either loop away. Percentiles are only reported for variants 2/3,
// where each individual round trip (tens of thousands of them, not hundreds of millions) is
// cheap to timestamp and store.
//
// # Signal-handler safety
//
// The handler installed here does no allocation and no I/O; it talks to the rest of the
// process purely through `AtomicU8`/`AtomicU32`/`AtomicU64`/`AtomicUsize` statics, all with
// `Ordering::Relaxed` (single signal-handling thread, no cross-thread synchronization needed
// beyond "the write becomes visible to the next read of the same static").
//
// # macOS ucontext/mcontext layout
//
// The `libc` crate (0.2.189, as pinned in this crate's `Cargo.toml`) does not define
// `ucontext_t`/`mcontext_t` for Apple targets at all -- grep its source; they simply aren't
// there. The structs below are hand-declared to match Darwin's
// `<mach/arm/_structs.h>`/`<sys/_types/_ucontext64.h>` layout for arm64: `ucontext_t.uc_mcontext`
// is itself a *pointer* to a `struct __darwin_mcontext64`, which holds an exception-state
// struct followed by the general-register (`__ss`) thread state, which is where `__pc` lives.
// This matches the task brief's own pointer chain: "ucontext_t -> uc_mcontext -> __ss.__pc".
// Field types are borrowed from `libc` (`stack_t`, `sigset_t`, `size_t`, `c_int`) wherever
// possible so `#[repr(C)]` field layout/alignment falls out of the same rules the real struct
// was compiled with, rather than being hand-computed and hard-coded here.

use clap::{Parser, Subcommand};
use libc::{c_int, c_void, sigaction, siginfo_t};
use std::hint::black_box;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::time::Instant;

// ---------------------------------------------------------------------------------------
// Darwin arm64 ucontext/mcontext, hand-declared (see module doc comment for why).
// ---------------------------------------------------------------------------------------

/// `struct __darwin_arm_exception_state64` (`<mach/arm/_structs.h>`).
#[repr(C)]
struct DarwinExceptionState64 {
    far: u64,
    esr: u32,
    exception: u32,
}

/// `struct __darwin_arm_thread_state64` (`<mach/arm/_structs.h>`). `pc` is what variant 3
/// advances by 4 bytes (one AArch64 instruction) to skip the faulting store.
#[repr(C)]
struct DarwinThreadState64 {
    x: [u64; 29],
    fp: u64,
    lr: u64,
    sp: u64,
    pc: u64,
    cpsr: u32,
    pad: u32,
}

/// `struct __darwin_mcontext64`. Only the two leading members are declared -- the trailing
/// NEON state is never touched here, and because this is only ever accessed through a
/// pointer, the struct's own total size (which would need the NEON member to be exact) never
/// matters; only the offsets of `es` and `ss` within it do, and those come first.
#[repr(C)]
struct DarwinMcontext64 {
    es: DarwinExceptionState64,
    ss: DarwinThreadState64,
}

/// `struct __darwin_ucontext` (64-bit). Field types are `libc`'s own so the layout matches
/// what the real header produces on this target.
#[repr(C)]
struct DarwinUcontext {
    uc_onstack: c_int,
    uc_sigmask: libc::sigset_t,
    uc_stack: libc::stack_t,
    uc_link: *mut DarwinUcontext,
    uc_mcsize: libc::size_t,
    uc_mcontext: *mut DarwinMcontext64,
}

// ---------------------------------------------------------------------------------------
// Signal handler plumbing, shared by variants 2, 3 and 4.
// ---------------------------------------------------------------------------------------

const MODE_MPROTECT_TOGGLE: u8 = 0;
const MODE_PC_ADVANCE: u8 = 1;

/// Which of the two handler behaviours is active. Only one variant runs at a time, so a
/// single global handler with a mode switch is simpler and safer than juggling two `sigaction`
/// registrations mid-run.
static FAULT_MODE: AtomicU8 = AtomicU8::new(MODE_MPROTECT_TOGGLE);

/// The page `mprotect-toggle` should widen back to RW on fault. Set before each timed batch.
static PROT_PAGE_ADDR: AtomicUsize = AtomicUsize::new(0);
static PROT_PAGE_LEN: AtomicUsize = AtomicUsize::new(0);

/// Bit 0 = SIGBUS observed, bit 1 = SIGSEGV observed. macOS is documented (and the task brief
/// says so too) to deliver SIGBUS for a protection fault on a mapped-but-unwritable page, but
/// this is recorded rather than assumed, and both signals are handled identically.
static SIGNAL_SEEN: AtomicU32 = AtomicU32::new(0);

/// Total number of times the handler has run, across all variants that use it.
static FAULT_COUNT: AtomicU64 = AtomicU64::new(0);

const SIGBUS_BIT: u32 = 1 << 0;
const SIGSEGV_BIT: u32 = 1 << 1;

/// The installed `SA_SIGINFO` handler. Async-signal-safe: no allocation, no `printf`/`println`,
/// only atomic stores/loads and (in mprotect-toggle mode) a single `mprotect` syscall, which is
/// itself async-signal-safe per POSIX.
extern "C" fn signal_handler(sig: c_int, _info: *mut siginfo_t, ctx: *mut c_void) {
    match sig {
        libc::SIGBUS => {
            SIGNAL_SEEN.fetch_or(SIGBUS_BIT, Ordering::Relaxed);
        }
        libc::SIGSEGV => {
            SIGNAL_SEEN.fetch_or(SIGSEGV_BIT, Ordering::Relaxed);
        }
        _ => {}
    }
    FAULT_COUNT.fetch_add(1, Ordering::Relaxed);

    match FAULT_MODE.load(Ordering::Relaxed) {
        MODE_MPROTECT_TOGGLE => {
            let addr = PROT_PAGE_ADDR.load(Ordering::Relaxed) as *mut c_void;
            let len = PROT_PAGE_LEN.load(Ordering::Relaxed);
            unsafe {
                libc::mprotect(addr, len, libc::PROT_READ | libc::PROT_WRITE);
            }
            // No PC fixup: the kernel re-executes the faulting store once this handler
            // returns, and this time the page is writable, so it succeeds.
        }
        MODE_PC_ADVANCE => unsafe {
            let uc = ctx as *mut DarwinUcontext;
            let mctx = (*uc).uc_mcontext;
            // All AArch64 instructions are 4 bytes; skip the single `str` that faulted.
            (*mctx).ss.pc = (*mctx).ss.pc.wrapping_add(4);
        },
        _ => {}
    }
}

/// Installs `signal_handler` for both SIGBUS and SIGSEGV with SA_SIGINFO. macOS is documented
/// to raise SIGBUS for a write to a read-only *mapping* (as opposed to genuinely unmapped
/// memory, which is SIGSEGV); both are wired up because relying on undocumented-in-practice
/// behaviour here would be exactly the kind of silent-failure risk CLAUDE.md warns about.
fn install_handler() {
    unsafe {
        let mut sa: sigaction = std::mem::zeroed();
        sa.sa_sigaction = signal_handler as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigemptyset(&mut sa.sa_mask);
        assert_eq!(
            libc::sigaction(libc::SIGBUS, &sa, std::ptr::null_mut()),
            0,
            "sigaction(SIGBUS) failed: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(
            libc::sigaction(libc::SIGSEGV, &sa, std::ptr::null_mut()),
            0,
            "sigaction(SIGSEGV) failed: {}",
            std::io::Error::last_os_error()
        );
    }
}

fn signal_seen_label() -> &'static str {
    match SIGNAL_SEEN.load(Ordering::Relaxed) {
        0 => "none observed",
        SIGBUS_BIT => "SIGBUS only",
        SIGSEGV_BIT => "SIGSEGV only",
        _ => "both SIGBUS and SIGSEGV",
    }
}

// ---------------------------------------------------------------------------------------
// Small stats helper for variants 2/3 (cheap to keep every sample; see module doc comment).
// ---------------------------------------------------------------------------------------

struct Stats {
    min_ns: u64,
    mean_ns: f64,
    p50_ns: u64,
    p99_ns: u64,
    n: usize,
}

fn stats(mut samples_ns: Vec<u64>) -> Stats {
    samples_ns.sort_unstable();
    let n = samples_ns.len();
    let sum: u128 = samples_ns.iter().map(|&v| v as u128).sum();
    Stats {
        min_ns: samples_ns[0],
        mean_ns: sum as f64 / n as f64,
        p50_ns: samples_ns[n / 2],
        p99_ns: samples_ns[(n * 99 / 100).min(n - 1)],
        n,
    }
}

impl std::fmt::Display for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "n={} min={}ns mean={:.1}ns p50={}ns p99={}ns",
            self.n, self.min_ns, self.mean_ns, self.p50_ns, self.p99_ns
        )
    }
}

// ---------------------------------------------------------------------------------------
// xorshift32, fixed seed: reproducible pseudo-random address sequence for variant 1.
// ---------------------------------------------------------------------------------------

struct Xorshift32(u32);

impl Xorshift32 {
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
}

// ---------------------------------------------------------------------------------------
// Variant 1: inline page-type check marginal cost.
// ---------------------------------------------------------------------------------------

/// One byte per 64 KiB page over a simulated 4 GiB guest address space: 2^32 / 2^16 = 65536
/// entries, matching §4.6's page-type table exactly.
const GUEST_PAGE_TABLE_LEN: usize = 65536;
const RAM_BUF_LEN: usize = 2 * 1024 * 1024;
const PAGE_TYPE_RAM: u8 = 1;

fn run_inline_check(iterations: u64, quick: bool) {
    println!("== [macOS] variant 1: inline page-type check marginal cost ==");
    if quick {
        println!("   (--quick mode: not a representative measurement)");
    }

    // All entries classified RAM: this measures the *branch-predictable* common case (most
    // guest accesses in practice are RAM), not a worst-case table walk -- the table itself is
    // 64 KiB and stays cache-resident regardless.
    let table = vec![PAGE_TYPE_RAM; GUEST_PAGE_TABLE_LEN];
    let buf = vec![0xABu8; RAM_BUF_LEN];

    // Raw read, no check.
    let mut rng = Xorshift32(0xC0FFEE01);
    let mut checksum: u64 = 0;
    let start = Instant::now();
    for _ in 0..iterations {
        let addr = rng.next();
        let offset = (addr as usize) % RAM_BUF_LEN;
        let byte = unsafe { *buf.get_unchecked(offset) };
        checksum = checksum.wrapping_add(black_box(byte) as u64);
    }
    let raw_elapsed = start.elapsed();
    black_box(checksum);

    // Table load + compare + branch, then the same read.
    let mut rng = Xorshift32(0xC0FFEE01); // same seed: same address sequence
    let mut checksum: u64 = 0;
    let mut open_bus_hits: u64 = 0;
    let start = Instant::now();
    for _ in 0..iterations {
        let addr = rng.next();
        let page = (addr >> 16) as usize;
        let offset = (addr as usize) % RAM_BUF_LEN;
        let kind = unsafe { *table.get_unchecked(page) };
        if black_box(kind) == PAGE_TYPE_RAM {
            let byte = unsafe { *buf.get_unchecked(offset) };
            checksum = checksum.wrapping_add(black_box(byte) as u64);
        } else {
            open_bus_hits += 1;
        }
    }
    let checked_elapsed = start.elapsed();
    black_box((checksum, open_bus_hits));

    let raw_ns_per_access = raw_elapsed.as_secs_f64() * 1e9 / iterations as f64;
    let checked_ns_per_access = checked_elapsed.as_secs_f64() * 1e9 / iterations as f64;
    let delta_ns = checked_ns_per_access - raw_ns_per_access;

    println!(
        "   macOS: {iterations} iterations, raw read = {raw_ns_per_access:.3} ns/access, \
         checked read = {checked_ns_per_access:.3} ns/access, inline-check marginal cost = \
         {delta_ns:.3} ns/access"
    );

    LAST_INLINE_CHECK_MARGINAL_NS.store(delta_ns.max(0.0).to_bits(), Ordering::Relaxed);
}

/// Stashed for the variant-5 comparison, as an f64 bit pattern (there's no `AtomicF64`).
static LAST_INLINE_CHECK_MARGINAL_NS: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------------------
// mmap/page helpers shared by variants 2, 3, 4.
// ---------------------------------------------------------------------------------------

fn host_page_size() -> usize {
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    assert!(n > 0, "sysconf(_SC_PAGESIZE) failed");
    n as usize
}

/// Maps one anonymous page with the given protection. Returns the base address.
fn map_anon_page(len: usize, prot: c_int) -> *mut u8 {
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            prot,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    assert_ne!(
        ptr,
        libc::MAP_FAILED,
        "mmap failed: {}",
        std::io::Error::last_os_error()
    );
    ptr as *mut u8
}

// ---------------------------------------------------------------------------------------
// Variant 2: write-fault round trip via mprotect toggling.
// ---------------------------------------------------------------------------------------

fn run_mprotect_toggle(faults: u64, quick: bool) {
    println!("== [macOS] variant 2: write-fault round trip, mprotect-toggle service ==");
    if quick {
        println!("   (--quick mode: not a representative measurement)");
    }

    let page_len = host_page_size();
    let page = map_anon_page(page_len, libc::PROT_READ);

    PROT_PAGE_ADDR.store(page as usize, Ordering::Relaxed);
    PROT_PAGE_LEN.store(page_len, Ordering::Relaxed);
    FAULT_MODE.store(MODE_MPROTECT_TOGGLE, Ordering::Relaxed);
    SIGNAL_SEEN.store(0, Ordering::Relaxed);
    let faults_before = FAULT_COUNT.load(Ordering::Relaxed);

    install_handler();

    // Primary loop: each iteration faults once (page is PROT_READ going in), the handler
    // widens it to RW and the store retries and succeeds, then the main thread re-protects
    // back to PROT_READ for the next iteration.
    let mut round_trip_ns = Vec::with_capacity(faults as usize);
    for i in 0..faults {
        let start = Instant::now();
        unsafe {
            std::ptr::write_volatile(page, (i & 0xFF) as u8);
        }
        let rc = unsafe { libc::mprotect(page as *mut c_void, page_len, libc::PROT_READ) };
        assert_eq!(rc, 0, "mprotect(PROT_READ) failed mid-loop");
        round_trip_ns.push(start.elapsed().as_nanos() as u64);
    }
    let primary = stats(round_trip_ns);

    // Loop-overhead baseline: structurally identical (one mprotect call + one volatile write
    // per iteration), but the page is *already* RW so the write never faults and the handler
    // never runs. This isolates "signal delivery + handler dispatch + fault retry" from the
    // two syscalls (mprotect x2 -- one explicit here, one inside the handler in the primary
    // loop) and the write itself that both loops pay regardless.
    let rc = unsafe {
        libc::mprotect(
            page as *mut c_void,
            page_len,
            libc::PROT_READ | libc::PROT_WRITE,
        )
    };
    assert_eq!(rc, 0);
    let mut baseline_ns = Vec::with_capacity(faults as usize);
    for i in 0..faults {
        let start = Instant::now();
        unsafe {
            std::ptr::write_volatile(page, (i & 0xFF) as u8);
        }
        let rc = unsafe {
            libc::mprotect(
                page as *mut c_void,
                page_len,
                libc::PROT_READ | libc::PROT_WRITE,
            )
        };
        assert_eq!(rc, 0);
        baseline_ns.push(start.elapsed().as_nanos() as u64);
    }
    let baseline = stats(baseline_ns);

    unsafe {
        libc::munmap(page as *mut c_void, page_len);
    }

    let faults_observed = FAULT_COUNT.load(Ordering::Relaxed) - faults_before;
    println!("   macOS: primary (fault + retry + re-protect): {primary}");
    println!("   macOS: baseline (no fault, same syscall shape): {baseline}");
    println!(
        "   macOS: fault-service cost, loop-overhead subtracted (mean): {:.1} ns",
        primary.mean_ns - baseline.mean_ns
    );
    println!(
        "   macOS: {faults_observed} faults observed (requested {faults}), signal(s) fired: {}",
        signal_seen_label()
    );
    assert_eq!(
        faults_observed, faults,
        "fault count mismatch -- handler did not run exactly once per iteration"
    );

    LAST_FAULT_ROUND_TRIP_NS.store(primary.mean_ns.to_bits(), Ordering::Relaxed);
}

static LAST_FAULT_ROUND_TRIP_NS: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------------------
// Variant 3: PC-advance service (the real I/O-service shape).
// ---------------------------------------------------------------------------------------

/// A single `str` instruction, and nothing else, so the handler's "skip one instruction"
/// PC-advance is skipping exactly the store and nothing adjacent. Written as `asm!` rather
/// than a `write_volatile` in an `#[inline(never)]` function because the task brief flags
/// compiler-generated store forms (e.g. store-pair, or a preceding address computation folded
/// into the faulting instruction) as a real source of flakiness for this technique, and `asm!`
/// makes the emitted instruction sequence explicit and auditable rather than trusted.
#[inline(never)]
fn faulting_store(page: *mut u8, value: u32) {
    unsafe {
        std::arch::asm!(
            "str {val:w}, [{addr}]",
            addr = in(reg) page,
            val = in(reg) value,
            options(nostack, preserves_flags),
        );
    }
}

fn run_pc_advance(faults: u64, quick: bool) -> bool {
    println!(
        "== [macOS] variant 3: PC-advance service (fault, skip instruction, never unprotect) =="
    );
    if quick {
        println!("   (--quick mode: not a representative measurement)");
    }

    let page_len = host_page_size();
    let page = map_anon_page(page_len, libc::PROT_READ);

    FAULT_MODE.store(MODE_PC_ADVANCE, Ordering::Relaxed);
    SIGNAL_SEEN.store(0, Ordering::Relaxed);
    let faults_before = FAULT_COUNT.load(Ordering::Relaxed);

    install_handler();

    // The page is never re-protected: the handler advances PC past the store instead of
    // letting it retry, so the store never actually executes and the page never needs to
    // become writable. This is exactly what §4.6 asks for at fault-patched I/O sites.
    let mut round_trip_ns = Vec::with_capacity(faults as usize);
    let mut flaky = false;
    for i in 0..faults {
        let start = Instant::now();
        faulting_store(page, i as u32);
        round_trip_ns.push(start.elapsed().as_nanos() as u64);
    }

    let faults_observed = FAULT_COUNT.load(Ordering::Relaxed) - faults_before;
    if faults_observed != faults {
        flaky = true;
        println!(
            "   macOS: FLAKY -- expected {faults} faults, observed {faults_observed}. \
             Treating variant 2 (mprotect-toggle) as primary; do not trust this number."
        );
    }

    // If the page did take an unexpected real write (flakiness), reading it back would fault
    // again on a read of a still-PROT_READ page only if unmapped; PROT_READ pages are
    // readable, so this is safe either way and is purely a diagnostic, not a correctness gate.
    unsafe {
        libc::munmap(page as *mut c_void, page_len);
    }

    if !flaky {
        let primary = stats(round_trip_ns);
        println!("   macOS: fault + skip-instruction round trip: {primary}");
        println!(
            "   macOS: {faults_observed} faults observed (requested {faults}), signal(s) fired: {}",
            signal_seen_label()
        );
    }

    !flaky
}

// ---------------------------------------------------------------------------------------
// Variant 4: shared read-only $FF page technique (functional check, not perf).
// ---------------------------------------------------------------------------------------

fn run_shared_ff() {
    println!("== [macOS] variant 4: shared read-only $FF page technique (functional check) ==");

    const OBJ_LEN: usize = 64 * 1024;
    let page_len = host_page_size();
    assert_eq!(
        OBJ_LEN % page_len,
        0,
        "64 KiB shared object must be a whole number of host pages (page size {page_len})"
    );

    let name = format!("/m68k-machine-fault-cost-{}\0", std::process::id());
    let name_ptr = name.as_ptr() as *const libc::c_char;

    let fd = unsafe {
        libc::shm_open(
            name_ptr,
            libc::O_CREAT | libc::O_RDWR | libc::O_EXCL,
            0o600 as libc::c_uint,
        )
    };
    assert!(
        fd >= 0,
        "shm_open failed: {}",
        std::io::Error::last_os_error()
    );
    // Unlink immediately: the mapping keeps the object alive, and this avoids leaking the
    // shm name if the process dies before cleanup, matching the task brief's shape exactly.
    let unlink_rc = unsafe { libc::shm_unlink(name_ptr) };
    assert_eq!(unlink_rc, 0, "shm_unlink failed");

    let trunc_rc = unsafe { libc::ftruncate(fd, OBJ_LEN as libc::off_t) };
    assert_eq!(
        trunc_rc,
        0,
        "ftruncate failed: {}",
        std::io::Error::last_os_error()
    );

    // Fill with 0xFF via a private RW mapping, then drop it -- the shared object's backing
    // pages now read 0xFF everywhere, independent of this mapping's lifetime.
    let fill_ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            OBJ_LEN,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    assert_ne!(fill_ptr, libc::MAP_FAILED, "mmap (fill) failed");
    unsafe {
        std::ptr::write_bytes(fill_ptr as *mut u8, 0xFF, OBJ_LEN);
        libc::munmap(fill_ptr, OBJ_LEN);
    }

    // Reserve a region big enough for several aliases, all PROT_NONE, then punch read-only
    // MAP_FIXED aliases of the same shm object into it at distinct offsets.
    const ALIAS_COUNT: usize = 4;
    let reservation_len = OBJ_LEN * ALIAS_COUNT;
    let reservation = map_anon_page(reservation_len, libc::PROT_NONE);

    let mut aliases = Vec::with_capacity(ALIAS_COUNT);
    for i in 0..ALIAS_COUNT {
        let target = unsafe { reservation.add(i * OBJ_LEN) };
        let mapped = unsafe {
            libc::mmap(
                target as *mut c_void,
                OBJ_LEN,
                libc::PROT_READ,
                libc::MAP_SHARED | libc::MAP_FIXED,
                fd,
                0,
            )
        };
        assert_eq!(
            mapped, target as *mut c_void,
            "MAP_FIXED alias {i} landed at the wrong address"
        );
        aliases.push(target);
    }

    unsafe {
        libc::close(fd);
    }

    let mut all_ff = true;
    for (i, &alias) in aliases.iter().enumerate() {
        let byte = unsafe { std::ptr::read_volatile(alias) };
        let second = unsafe { std::ptr::read_volatile(alias.add(OBJ_LEN - 1)) };
        if byte != 0xFF || second != 0xFF {
            println!(
                "   macOS: FAIL -- alias {i} did not read back 0xFF (got {byte:#x}, {second:#x})"
            );
            all_ff = false;
        }
    }

    // Write attempt: must fault. Reuse the PC-advance handler shape (skip the single `str`)
    // so this doesn't need to unprotect anything or risk corrupting the shared object.
    FAULT_MODE.store(MODE_PC_ADVANCE, Ordering::Relaxed);
    SIGNAL_SEEN.store(0, Ordering::Relaxed);
    let faults_before = FAULT_COUNT.load(Ordering::Relaxed);
    install_handler();
    faulting_store(aliases[0], 0x00);
    let write_faulted = FAULT_COUNT.load(Ordering::Relaxed) - faults_before == 1;

    unsafe {
        libc::munmap(reservation as *mut c_void, reservation_len);
    }

    let pass = all_ff && write_faulted;
    println!(
        "   macOS: {} aliases read 0xFF at both ends; write attempt {} (signal: {})",
        aliases.len(),
        if write_faulted {
            "faulted as expected"
        } else {
            "did NOT fault -- BUG"
        },
        signal_seen_label()
    );
    println!(
        "   macOS: shared-ff technique: {}",
        if pass { "PASS" } else { "FAIL" }
    );
}

// ---------------------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------------------

/// C1 signal-fault cost harness (macOS only, see module doc comment for the Linux caveat).
#[derive(Parser)]
#[command(
    name = "fault_cost",
    about = "macOS signal-fault cost harness for C1 (docs/cpu-core-proposal.md §4.6)"
)]
struct Cli {
    /// Small iteration/fault counts for a smoke test. Not a representative measurement --
    /// use the default (no flag) for anything that will be recorded or compared.
    #[arg(long)]
    quick: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Variant 1 only: inline page-type check marginal cost.
    InlineCheck,
    /// Variant 2 only: write-fault round trip via mprotect toggling.
    MprotectToggle,
    /// Variant 3 only: PC-advance service.
    PcAdvance,
    /// Variant 4 only: shared read-only $FF page functional check.
    SharedFf,
    /// All variants, in order, plus the final comparison. Default if no subcommand is given.
    All,
}

fn main() {
    let cli = Cli::parse();

    let inline_iters: u64 = if cli.quick { 200_000 } else { 200_000_000 };
    let mprotect_faults: u64 = if cli.quick { 200 } else { 100_000 };
    let pc_advance_faults: u64 = if cli.quick { 200 } else { 100_000 };

    println!(
        "fault_cost: macOS 26 / Darwin 25.6, Apple M3 Pro, sigaction signal path \
         (Mach-exception-port path NOT measured). Linux measurement per \
         docs/cpu-core-proposal.md §4.6 is OUTSTANDING -- this is an upper-bound estimate only."
    );
    if cli.quick {
        println!("fault_cost: --quick mode active, numbers below are NOT representative.");
    }

    match cli.command.unwrap_or(Command::All) {
        Command::InlineCheck => run_inline_check(inline_iters, cli.quick),
        Command::MprotectToggle => run_mprotect_toggle(mprotect_faults, cli.quick),
        Command::PcAdvance => {
            run_pc_advance(pc_advance_faults, cli.quick);
        }
        Command::SharedFf => run_shared_ff(),
        Command::All => {
            run_inline_check(inline_iters, cli.quick);
            println!();
            run_mprotect_toggle(mprotect_faults, cli.quick);
            println!();
            let pc_advance_clean = run_pc_advance(pc_advance_faults, cli.quick);
            println!();
            run_shared_ff();
            println!();

            println!("== [macOS] variant 5: the comparison that matters ==");
            let inline_marginal_ns =
                f64::from_bits(LAST_INLINE_CHECK_MARGINAL_NS.load(Ordering::Relaxed));
            let fault_round_trip_ns =
                f64::from_bits(LAST_FAULT_ROUND_TRIP_NS.load(Ordering::Relaxed));
            if inline_marginal_ns > 0.0 {
                let ratio = fault_round_trip_ns / inline_marginal_ns;
                println!(
                    "   macOS 26 / Darwin 25.6, Apple M3 Pro, sigaction signal path \
                     (Mach-exception-port path NOT measured): one mprotect-toggle fault \
                     round trip costs as much as ~{ratio:.0} inline page-type checks on this \
                     host ({fault_round_trip_ns:.1} ns / {inline_marginal_ns:.3} ns)."
                );
            } else {
                println!("   macOS: inline-check marginal cost measured as ~0ns; ratio undefined.");
            }
            if !pc_advance_clean {
                println!(
                    "   macOS: note -- pc-advance variant was flaky this run; the ratio above \
                     uses variant 2 (mprotect-toggle) as specified when that happens."
                );
            }
            println!(
                "   Reminder: this is a macOS figure. docs/cpu-core-proposal.md §4.6 asks for \
                 a Linux number before the fault-vs-inline-check split is committed to; that \
                 measurement is still outstanding."
            );
        }
    }
}
