//! `m68k-vp`: a proof-of-concept purpose-built decoder + IR + threaded
//! interpreter, prototyping docs/cpu-core-proposal.md's proposed core
//! through its §4.1-§4.4 (decoder, IR, the flag-liveness and peephole
//! passes, the IR interpreter). Everything else in that document -- JIT
//! emitters, direct AUTOCONFIG memory mapping, replay lockstep,
//! exceptions/interrupts, the FPU, no_std arenas beyond what this PoC
//! needs -- is out of scope here; see docs/cpu-core-poc-results.md for
//! what this crate does and does not show.
//!
//! `#![no_std]`, no allocator: every buffer (the per-block IR arena, the
//! block cache) is a bounded, statically sized array, and guest memory is
//! borrowed from the caller (`mem::VpMemory`) rather than owned --
//! mirrors `machine-core`'s device convention (CLAUDE.md). The `std`
//! feature exists only for test/harness convenience (currently unused by
//! anything but `cargo test`, which pulls in `std` automatically via the
//! `not(any(feature = "std", test))` gate below regardless of the
//! feature flag).
//!
//! Covers exactly the instruction surface and addressing modes
//! `m68k/cpubench/kernels.s`'s assembled blob uses -- not the general
//! 68040 ISA. See `decoder.rs`'s module doc for the exhaustive,
//! verified-against-the-blob list.

#![cfg_attr(not(any(test, feature = "std")), no_std)]

pub mod decoder;
pub mod interp;
pub mod ir;
pub mod mem;
pub mod passes;

pub use interp::{VpCore, VpCpu, VpExit, VpRunResult};
pub use mem::VpMemory;

#[cfg(test)]
mod tests;
