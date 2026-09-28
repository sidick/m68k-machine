//! Correctness gates (docs/cpu-core-poc-results.md's brief, both
//! mandatory):
//!
//! 1. Retired-instruction parity: calibrate exactly like
//!    `crates/cpu-bench`'s `calibrate` does against
//!    `m68k/cpubench/kernels.s`'s own metadata tables.
//! 2. Differential state check: run the same call on `m68k-vp` and on
//!    m68k-rs's `CpuCore`/`LinearMemoryBus` and assert D0-D7/A0-A7/PC and
//!    the full guest RAM match byte-for-byte.
//!
//! Shares the exact addresses and calling convention `crates/cpu-bench`
//! uses (same blob, same buffers, same stack, same sentinel) so both
//! gates exercise the identical instruction stream that crate measures.

use crate::{VpCore, VpExit, VpMemory};
use m68k::core::memory::{AddressBus, LinearMemoryBus};
use m68k::{CpuCore, CpuType};

static KERNELS_BIN: &[u8] = include_bytes!("../../../m68k/cpubench/kernels.bin");

const BUS_SIZE: usize = 0x0004_0000; // 256 KiB, power of two.
const KERNEL_BASE: u32 = 0x0000_0000;
const BUF_A: u32 = 0x0000_1000;
const BUF_B: u32 = 0x0000_2000;
const STACK_TOP: u32 = 0x0003_FF00;
const RETURN_SENTINEL: u32 = 0;
const CALIBRATION_ITERS: u32 = 97; // matches crates/cpu-bench, not a round number.

struct KernelMeta {
    name: &'static str,
    entry: u32,
    instrs_per_iter: u32,
    fixed_overhead: u32,
    buffers: u8,
}

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

fn parse_kernel_table() -> std::vec::Vec<KernelMeta> {
    let count = read_u32_be(KERNELS_BIN, 0) as usize;
    assert_eq!(
        count,
        KERNEL_NAMES.len(),
        "kernels.bin's _KernelCount does not match this test's KERNEL_NAMES"
    );
    let table_off = 4;
    let instrs_off = table_off + count * 4;
    let fixed_off = instrs_off + count * 4;
    let mut kernels = std::vec::Vec::with_capacity(count);
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

fn new_vp_mem(buf: &mut [u8]) -> VpMemory<'_> {
    let mut mem = VpMemory::new(buf);
    mem.load(KERNEL_BASE, KERNELS_BIN);
    mem
}

fn prepare_vp(core: &mut VpCore, kernel: &KernelMeta, iterations: u32) {
    core.cpu = crate::VpCpu::new();
    core.cpu.pc = kernel.entry;
    core.cpu.d[0] = iterations;
    core.cpu.a[7] = STACK_TOP;
    if kernel.buffers >= 1 {
        core.cpu.a[0] = BUF_A;
    }
    if kernel.buffers >= 2 {
        core.cpu.a[1] = BUF_B;
    }
}

fn fresh_m68k() -> CpuCore {
    let mut cpu = CpuCore::new();
    cpu.set_cpu_type(CpuType::M68040);
    cpu.set_sr(0x2700);
    cpu.set_a(7, STACK_TOP);
    cpu
}

fn prepare_m68k(cpu: &mut CpuCore, kernel: &KernelMeta, iterations: u32) {
    cpu.pc = kernel.entry;
    cpu.set_d(0, iterations);
    if kernel.buffers >= 1 {
        cpu.set_a(0, BUF_A);
    }
    if kernel.buffers >= 2 {
        cpu.set_a(1, BUF_B);
    }
}

/// Gate 1: retired-instruction parity, run for every kernel.
#[test]
fn every_kernel_calibrates_on_vp() {
    let kernels = parse_kernel_table();
    let mut buf = std::vec![0u8; BUS_SIZE];
    let mut mem = new_vp_mem(&mut buf);
    let mut core = VpCore::new();

    for kernel in &kernels {
        prepare_vp(&mut core, kernel, CALIBRATION_ITERS);
        mem.write_u32(STACK_TOP, RETURN_SENTINEL);

        let expected = kernel.fixed_overhead + CALIBRATION_ITERS * kernel.instrs_per_iter;
        let budget = expected + 64;
        let result = core.run(&mut mem, budget, &[RETURN_SENTINEL]);

        assert_eq!(
            result.exit,
            VpExit::WatchedPc(RETURN_SENTINEL),
            "kernel {}: m68k-vp did not stop at its own RTS (exit={:?})",
            kernel.name,
            result.exit
        );
        assert_eq!(
            result.instructions, expected,
            "kernel {}: m68k-vp retired {} instructions over {CALIBRATION_ITERS} outer \
             iterations, but kernels.s's own tables document {expected} \
             (fixed_overhead={} + iterations * instrs_per_iter={})",
            kernel.name, result.instructions, kernel.fixed_overhead, kernel.instrs_per_iter,
        );
    }
}

/// Gate 2: differential state check against m68k-rs, byte-for-byte,
/// after an identical calibration-length run.
#[test]
fn every_kernel_matches_m68k_rs() {
    let kernels = parse_kernel_table();

    for kernel in &kernels {
        // m68k-rs side.
        let mut m68k_bus = LinearMemoryBus::new(BUS_SIZE);
        m68k_bus.load(KERNEL_BASE, KERNELS_BIN);
        let mut m68k_cpu = fresh_m68k();
        prepare_m68k(&mut m68k_cpu, kernel, CALIBRATION_ITERS);
        m68k_bus.write_long(STACK_TOP, RETURN_SENTINEL);
        let expected = kernel.fixed_overhead + CALIBRATION_ITERS * kernel.instrs_per_iter;
        let budget = expected + 64;
        let m68k_result = m68k_cpu.run_batch(&mut m68k_bus, budget, &[RETURN_SENTINEL]);
        assert!(
            matches!(
                m68k_result.exit,
                m68k::core::types::BatchExit::WatchedPc {
                    pc: RETURN_SENTINEL
                }
            ),
            "kernel {}: m68k-rs oracle did not stop at its own RTS (exit={:?})",
            kernel.name,
            m68k_result.exit
        );

        // m68k-vp side.
        let mut vp_buf = std::vec![0u8; BUS_SIZE];
        let mut vp_mem = new_vp_mem(&mut vp_buf);
        let mut vp_core = VpCore::new();
        prepare_vp(&mut vp_core, kernel, CALIBRATION_ITERS);
        vp_mem.write_u32(STACK_TOP, RETURN_SENTINEL);
        let vp_result = vp_core.run(&mut vp_mem, budget, &[RETURN_SENTINEL]);
        assert_eq!(
            vp_result.exit,
            VpExit::WatchedPc(RETURN_SENTINEL),
            "kernel {}: m68k-vp did not stop at its own RTS (exit={:?})",
            kernel.name,
            vp_result.exit
        );

        assert_eq!(
            vp_result.instructions, m68k_result.instructions,
            "kernel {}: retired-instruction count differs between cores",
            kernel.name
        );

        for d in 0..8u8 {
            assert_eq!(
                vp_core.cpu.d[d as usize],
                m68k_cpu.d(d as usize),
                "kernel {}: D{d} differs (vp={:#x} m68k-rs={:#x})",
                kernel.name,
                vp_core.cpu.d[d as usize],
                m68k_cpu.d(d as usize),
            );
        }
        for a in 0..8u8 {
            assert_eq!(
                vp_core.cpu.a[a as usize],
                m68k_cpu.a(a as usize),
                "kernel {}: A{a} differs (vp={:#x} m68k-rs={:#x})",
                kernel.name,
                vp_core.cpu.a[a as usize],
                m68k_cpu.a(a as usize),
            );
        }
        assert_eq!(
            vp_core.cpu.pc, m68k_cpu.pc,
            "kernel {}: PC differs (vp={:#x} m68k-rs={:#x})",
            kernel.name, vp_core.cpu.pc, m68k_cpu.pc
        );
        assert_eq!(
            vp_mem.as_slice(),
            m68k_bus.as_slice(),
            "kernel {}: guest RAM differs byte-for-byte",
            kernel.name
        );
    }
}
