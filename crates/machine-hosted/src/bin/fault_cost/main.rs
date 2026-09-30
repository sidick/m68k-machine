//! Entry point for the `fault_cost` measurement binary
//! (`docs/direct-mapping.md`'s fault-cost section; `docs/cpu-core-proposal.md`
//! §4.6). The measurement itself lives in `darwin_arm64.rs` and is exactly
//! what its name says: Darwin/arm64-only -- hand-declared Darwin
//! `ucontext`/`mcontext` structs (libc declares none for Apple targets), an
//! AArch64 `str` via `asm!` as the auditable faulting site, and a fixed
//! 4-byte PC advance in the SIGBUS handler. None of that is portable, and
//! CI's x86-64 Linux runner proved it the hard way (the `{val:w}` asm
//! template modifier does not exist there), which is why this gate exists.
//!
//! On any other target this builds as a stub that says so and exits
//! non-zero, so `cargo clippy --all-targets` passes everywhere while the
//! Linux measurement §4.6 actually asks for remains honestly outstanding --
//! the port work (Linux `ucontext_t` from libc proper, per-arch faulting
//! store and PC step) is the recorded hand-off in `docs/direct-mapping.md`,
//! and its landing spot is a sibling module gated the same way as this one.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod darwin_arm64;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() {
    darwin_arm64::run()
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {
    eprintln!(
        "fault_cost: this measurement is implemented for Darwin/arm64 only; \
         the Linux port is a recorded hand-off (docs/direct-mapping.md, \
         'Fault cost' section). Refusing to print a number for a platform \
         it was not measured on."
    );
    std::process::exit(2);
}
