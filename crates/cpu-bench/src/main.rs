//! Bare-core CPU benchmark harness (docs/bus-fast-path-plan.md step 8).
//!
//! Loads `m68k/cpubench/kernels.bin` (the flat-binary assembly of
//! `m68k/cpubench/kernels.s`, vasm `-Fbin -m68040`, committed like every
//! other assembled-and-checked-in artifact this project ships) directly
//! onto `m68k::CpuCore` over `m68k::core::memory::LinearMemoryBus` -- no
//! `machine-core`, no `MachineBus`, no device engines. This is the "how
//! fast is the CPU core alone" number that docs/bus-fast-path-plan.md
//! step 8 compares against the in-machine `CPUBench` numbers (built by
//! `scripts/build-cpubench.sh`, run inside a real Kickstart 3.2.2 boot);
//! the ratio between them is the machine's overhead on identical
//! instructions.
//!
//! Every kernel runs three ways:
//!   - hook-free `run_for_cycles` (the "interp" column);
//!   - `run_batch`, which picks up `LinearMemoryBus`'s `FastMem` window
//!     automatically (the "batch" column, or "batch+jit" when this
//!     crate is built with `--features jit` -- see that feature's own
//!     doc comment in Cargo.toml for why this needs two separate builds
//!     rather than a runtime switch, mirroring how
//!     docs/adr-0006-cycle-budgeted-and-wall-clock-paced-timing.md's own
//!     `interp`/`batch`/`batch+jit` measurements were taken).
//!
//! Before timing, every kernel is calibrated: run for a small, known
//! outer-iteration count with the batch back end's `watch_pcs` stopping
//! it exactly at its own `RTS`, and the CPU's own retired-instruction
//! count is checked against `kernels.s`'s documented
//! `fixed_overhead + iterations * instrs_per_iter` formula (both
//! operands read out of the kernel blob's own metadata tables, not
//! duplicated here). A mismatch is a bug in `kernels.s`'s documentation
//! (per the task this harness was built for) and this program panics
//! loudly rather than reporting a throughput number next to a wrong
//! instructions-retired count -- silent failure is this platform's norm
//! and this harness does not add to it (CLAUDE.md).

use m68k::core::memory::{AddressBus, LinearMemoryBus};
use m68k::{CpuCore, CpuType};
use std::time::{Duration, Instant};

/// The assembled kernel blob, built by `scripts/build-cpubench.sh` from
/// `m68k/cpubench/kernels.s` via `vasm -Fbin -m68040`. Committed like
/// every other vendored-binary artifact in this project (CLAUDE.md).
static KERNELS_BIN: &[u8] = include_bytes!("../../../m68k/cpubench/kernels.bin");

/// Total guest address space for the bare harness. Generous relative to
/// the ~800-byte kernel blob and the small per-kernel buffers; a
/// power-of-two size gets `LinearMemoryBus`'s fast wrap-mask path.
const BUS_SIZE: usize = 0x0004_0000; // 256 KiB

/// Where the kernel blob is loaded. Table offsets inside it (see
/// `KernelMeta::parse`) are relative to this, so kernel entry addresses
/// are simply `KERNEL_BASE + offset`.
const KERNEL_BASE: u32 = 0x0000_0000;

/// Two small scratch buffers, well clear of the kernel blob and each
/// other, sized generously against every kernel's own documented
/// requirement (the largest is 128 bytes).
const BUF_A: u32 = 0x0000_1000;
const BUF_B: u32 = 0x0000_2000;

/// Stack pointer. `jsr_rts_chain` is the only kernel that touches the
/// stack at all (its own internal JSR/RTS pairs, never more than one
/// return address deep at a time), and the calibration run's sentinel
/// return address lives at this address too -- both need a little
/// headroom below the top of the bus, not the top itself.
const STACK_TOP: u32 = 0x0003_FF00;

/// Sentinel "return address" used by calibration runs: `run_batch`'s
/// `watch_pcs` stops execution the instant the kernel's own top-level
/// `RTS` would jump here, without executing whatever (if anything) is
/// mapped there. Address 0 is never a real kernel entry point -- the
/// blob's own metadata tables occupy the first bytes -- matching the
/// convention the fork's own `benches/microbench.rs` uses for the same
/// purpose.
const RETURN_SENTINEL: u32 = 0;

/// One kernel's static metadata, read out of the blob's own tables
/// (`_KernelTable`/`_KernelInstrsPerIter`/`_KernelFixedOverhead` in
/// `kernels.s`) plus the register-convention facts documented in that
/// file's header comment (buffer count, name) -- hardcoded here only
/// because naming and buffer arity are source-level facts vasm has no
/// table for, not because this harness is guessing at anything the
/// binary itself can state authoritatively.
struct KernelMeta {
    name: &'static str,
    entry: u32,
    instrs_per_iter: u32,
    fixed_overhead: u32,
    buffers: u8,
}

/// Names and buffer counts, in exactly the order `kernels.s`'s
/// `_KernelTable` lists them (that file's own header comment has the
/// authoritative list this must track).
const KERNEL_NAMES: [(&str, u8); 11] = [
    ("reg_addq_bra", 0),
    ("reg_tst_bne", 0),
    ("reg_mix", 0),
    ("mem_copy", 2),
    ("mem_fill", 1),
    ("struct_walk", 1),
    ("movem_saverestore", 1),
    ("jsr_rts_chain", 0),
    ("muldiv_mix", 0),
    ("bitfield_ops", 1),
    ("cmp_branchy", 0),
];

fn read_u32_be(bytes: &[u8], offset: usize) -> u32 {
    let slice = &bytes[offset..offset + 4];
    u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]])
}

/// Parse the blob's three metadata tables. Panics (not an error return)
/// on any inconsistency: a malformed benchmark blob is a build bug, and
/// this harness has no caller to hand a `Result` back to.
fn parse_kernel_table() -> Vec<KernelMeta> {
    let count = read_u32_be(KERNELS_BIN, 0) as usize;
    assert_eq!(
        count,
        KERNEL_NAMES.len(),
        "kernels.bin's _KernelCount does not match this harness's KERNEL_NAMES -- \
         kernels.s and main.rs have drifted"
    );

    let table_off = 4;
    let instrs_off = table_off + count * 4;
    let fixed_off = instrs_off + count * 4;

    let mut kernels = Vec::with_capacity(count);
    for (index, (name, buffers)) in KERNEL_NAMES.iter().enumerate() {
        let entry_offset = read_u32_be(KERNELS_BIN, table_off + index * 4);
        let instrs_per_iter = read_u32_be(KERNELS_BIN, instrs_off + index * 4);
        let fixed_overhead = read_u32_be(KERNELS_BIN, fixed_off + index * 4);
        kernels.push(KernelMeta {
            name,
            entry: KERNEL_BASE + entry_offset,
            instrs_per_iter,
            fixed_overhead,
            buffers: *buffers,
        });
    }
    kernels
}

fn new_bus() -> LinearMemoryBus {
    let mut bus = LinearMemoryBus::new(BUS_SIZE);
    bus.load(KERNEL_BASE, KERNELS_BIN);
    bus
}

fn fresh_cpu() -> CpuCore {
    let mut cpu = CpuCore::new();
    cpu.set_cpu_type(CpuType::M68040);
    cpu.set_sr(0x2700);
    cpu.set_a(7, STACK_TOP);
    cpu
}

/// Set up registers for one call into `kernel`: `d0` = outer iteration
/// count, `a0`/`a1` = its buffers (only as many as it documents needing).
fn prepare_call(cpu: &mut CpuCore, kernel: &KernelMeta, iterations: u32) {
    cpu.pc = kernel.entry;
    cpu.set_d(0, iterations);
    if kernel.buffers >= 1 {
        cpu.set_a(0, BUF_A);
    }
    if kernel.buffers >= 2 {
        cpu.set_a(1, BUF_B);
    }
}

/// Calibrate one kernel: run it for a small, known outer count with the
/// batch back end watching for the sentinel return, and check the CPU's
/// own retired-instruction count against `kernels.s`'s documented
/// formula. Panics loudly on a mismatch (see module doc comment).
fn calibrate(bus: &mut LinearMemoryBus, kernel: &KernelMeta) {
    const CALIBRATION_ITERS: u32 = 97; // small, and not a round number --
                                       // a formula that only happens to
                                       // work for a suspiciously tidy
                                       // count is not trusted here.

    let mut cpu = fresh_cpu();
    prepare_call(&mut cpu, kernel, CALIBRATION_ITERS);
    // Sentinel return address the kernel's own top-level RTS will pop.
    bus.write_long(STACK_TOP, RETURN_SENTINEL);

    let expected = kernel.fixed_overhead + CALIBRATION_ITERS * kernel.instrs_per_iter;
    // Generous headroom over `expected` so the watch (not the budget)
    // is what actually stops the batch -- if the budget ran out first,
    // the exit-reason check below catches it.
    let budget = expected + 64;
    let result = cpu.run_batch(bus, budget, &[RETURN_SENTINEL]);

    assert!(
        matches!(
            result.exit,
            m68k::core::types::BatchExit::WatchedPc {
                pc: RETURN_SENTINEL
            }
        ),
        "kernel {}: calibration run did not stop at its own RTS (exit={:?}) -- \
         the kernel does not return control after {CALIBRATION_ITERS} outer \
         iterations the way kernels.s documents",
        kernel.name,
        result.exit
    );
    assert_eq!(
        result.instructions, expected,
        "kernel {}: retired {} instructions over {CALIBRATION_ITERS} outer iterations, \
         but kernels.s's own tables document fixed_overhead={} + iterations * \
         instrs_per_iter={} = {expected} -- kernels.s's documented per-iteration \
         instruction count for this kernel is wrong, fix the .s file's header comment \
         and its _KernelInstrsPerIter/_KernelFixedOverhead entries",
        kernel.name, result.instructions, kernel.fixed_overhead, kernel.instrs_per_iter,
    );
}

/// A measured rate for one kernel/path pair.
struct Measurement {
    path: &'static str,
    instructions: u64,
    elapsed: Duration,
}

impl Measurement {
    fn mips(&self) -> f64 {
        self.instructions as f64 / self.elapsed.as_secs_f64() / 1_000_000.0
    }

    fn ns_per_instr(&self) -> f64 {
        self.elapsed.as_secs_f64() * 1_000_000_000.0 / self.instructions as f64
    }
}

const MIN_MEASURE_SECONDS: f64 = 1.0;

/// Run the hook-free interpreter (`run_for_cycles`) repeatedly with a
/// huge, never-exhausted outer iteration count (`u32::MAX`, so the
/// kernel's own SUBQ/BNE loop never reaches zero and the CPU never
/// executes its RTS or touches the stack) until at least
/// `MIN_MEASURE_SECONDS` of wall clock have elapsed, accumulating
/// retired instructions and elapsed time across calls.
fn measure_interp(bus: &mut LinearMemoryBus, kernel: &KernelMeta) -> Measurement {
    let mut cpu = fresh_cpu();
    prepare_call(&mut cpu, kernel, u32::MAX);

    let mut total_instructions: u64 = 0;
    let mut total_elapsed = Duration::ZERO;
    // Chosen so a single call is on the order of tens of milliseconds
    // even for the cheapest kernels, so the loop below does not spend
    // most of its time on `Instant::now()` overhead.
    let cycle_budget: i32 = 20_000_000;

    while total_elapsed.as_secs_f64() < MIN_MEASURE_SECONDS {
        let start = Instant::now();
        let result = cpu.run_for_cycles(bus, cycle_budget);
        total_elapsed += start.elapsed();
        total_instructions += u64::from(result.instructions);
    }

    Measurement {
        path: "interp",
        instructions: total_instructions,
        elapsed: total_elapsed,
    }
}

/// The `run_batch` path (`FastMem` window picked up automatically from
/// `LinearMemoryBus::fast_mem`). Labeled "batch+jit" when this crate is
/// built with `--features jit` -- see that feature's Cargo.toml comment
/// for why the JIT is measured via a separate build rather than a
/// runtime switch.
fn measure_batch(bus: &mut LinearMemoryBus, kernel: &KernelMeta) -> Measurement {
    let mut cpu = fresh_cpu();
    prepare_call(&mut cpu, kernel, u32::MAX);

    let mut total_instructions: u64 = 0;
    let mut total_elapsed = Duration::ZERO;
    let instr_budget: u32 = 4_000_000;

    while total_elapsed.as_secs_f64() < MIN_MEASURE_SECONDS {
        let start = Instant::now();
        let result = cpu.run_batch(bus, instr_budget, &[]);
        total_elapsed += start.elapsed();
        total_instructions += u64::from(result.instructions);
    }

    Measurement {
        path: batch_label(),
        instructions: total_instructions,
        elapsed: total_elapsed,
    }
}

#[cfg(feature = "jit")]
fn batch_label() -> &'static str {
    "batch+jit"
}

#[cfg(not(feature = "jit"))]
fn batch_label() -> &'static str {
    "batch"
}

fn main() {
    let kernels = parse_kernel_table();

    println!("cpu-bench: bare CpuCore/LinearMemoryBus harness");
    println!(
        "kernels: {} (from m68k/cpubench/kernels.bin, {} bytes)",
        kernels.len(),
        KERNELS_BIN.len()
    );
    println!("build: {}", batch_label());
    println!();

    println!(
        "{:<20} {:<10} {:>14} {:>14} {:>12}",
        "kernel", "path", "M instr/s", "ns/instr", "instructions"
    );
    println!("{}", "-".repeat(74));

    let mut bus = new_bus();

    for kernel in &kernels {
        calibrate(&mut bus, kernel);

        let interp = measure_interp(&mut bus, kernel);
        println!(
            "{:<20} {:<10} {:>14.1} {:>14.2} {:>12}",
            kernel.name,
            interp.path,
            interp.mips(),
            interp.ns_per_instr(),
            interp.instructions
        );

        let batch = measure_batch(&mut bus, kernel);
        println!(
            "{:<20} {:<10} {:>14.1} {:>14.2} {:>12}",
            kernel.name,
            batch.path,
            batch.mips(),
            batch.ns_per_instr(),
            batch.instructions
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kernel's documented per-iteration instruction count must
    /// match what the CPU actually retires. This is the same check
    /// `calibrate` runs before every measurement, exercised here as a
    /// fast, no-timing-required `cargo test -p cpu-bench`.
    #[test]
    fn every_kernel_calibrates() {
        let kernels = parse_kernel_table();
        let mut bus = new_bus();
        for kernel in &kernels {
            calibrate(&mut bus, kernel);
        }
    }

    #[test]
    fn kernel_table_matches_name_list() {
        let kernels = parse_kernel_table();
        assert_eq!(kernels.len(), KERNEL_NAMES.len());
    }
}
