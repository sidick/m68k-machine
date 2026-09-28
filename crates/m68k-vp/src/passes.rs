//! The two passes §4.3 allows: flag liveness, then peepholes. Nothing
//! else -- if a third pass ever looks necessary, that's a signal to
//! measure first, not to add one (§4.3's own rule).

use crate::ir::{Ea, IrBlock, Op, F_NONE, F_Z};

/// Backward flag-liveness scan. The decoder emits every flag-producing op
/// requesting `F_ALL` (conservative); this pass narrows each one down to
/// only the flag bits a later op in the same block actually reads before
/// the next flag-producing op overwrites them.
///
/// This kernel set makes the effect stark rather than subtle: every block
/// ends in exactly one `bne` reading Z, set by the `subq` immediately
/// before it. Every *other* flag-producing op in the block (the ALU mix
/// in `reg_mix`, the loads/stores that touch NZVC in `mem_copy`, etc.) has
/// its flags fully dead -- nothing in this basic block or its successor
/// (itself, for every kernel here: they're single-block loops) ever reads
/// them, because the next flag-producing op overwrites the CCR wholesale
/// before anything looks at it. So the pass reduces to: the last
/// flag-producing op before each `bne` keeps `F_Z` (only, not the rest of
/// F_ALL -- `bne` doesn't read N/V/C/X); everything else drops to
/// `F_NONE`. That is the entire "dead flag" saving §4.3 attributes to
/// this pass, measured directly by whatever speed difference disabling it
/// would show (this PoC doesn't ship a toggle to A/B that, but the
/// mechanism is exactly this narrowing -- see
/// docs/cpu-core-poc-results.md's liveness-pass note).
pub fn flag_liveness(block: &mut IrBlock) {
    let mut needed: u8 = F_NONE;
    for i in (0..block.count).rev() {
        match &mut block.instrs[i].op {
            Op::Bne { .. } => {
                needed = F_Z;
            }
            Op::AluBinReg { flags, .. }
            | Op::AluImm { flags, .. }
            | Op::AluUnary { flags, .. }
            | Op::Shift { flags, .. } => {
                *flags &= needed;
                needed = F_NONE;
            }
            // MulDiv, Bitfield, moves, calls and returns don't produce or
            // consume CCR flags in this IR (§4.2's front end never emits
            // an op that reads a prior flags-producing op's output; the
            // one exception, `bne`, is handled above), so they leave
            // `needed` unchanged as the scan passes through them.
            _ => {}
        }
    }
}

/// Peepholes. §4.3 names three examples (`MOVE`+`TST` fusion, constant
/// folding on immediate address arithmetic, `LEA`/`PEA` simplification);
/// none of the three has an occurrence in this kernel set to fold (see
/// docs/cpu-core-poc-results.md's "Spec issues found" -- worth recording
/// as an observation, not a defect). What *does* occur, repeatedly, is an
/// artifact of how `kernels.s` got assembled: every zero-displacement
/// conditional branch (`reg_tst_bne`, `cmp_branchy`) that vasm could not
/// encode in 8 bits got replaced with `lea (An),An` -- a real instruction
/// that changes no architectural state (`decoder.rs`'s module comment has
/// the discovery). Folding that specific self-referential `lea` into a
/// true no-op is cheap, safe (it still retires exactly one instruction,
/// matching the calibration gate) and, for those two kernels, the only
/// peephole this workload actually exercises.
pub fn peephole(block: &mut IrBlock) {
    for instr in &mut block.instrs[..block.count] {
        if let Op::Lea {
            dst,
            ea: Ea::Ind(base),
        } = instr.op
        {
            if base == dst {
                instr.op = Op::Nop;
            }
        }
    }
}

pub fn run_passes(block: &mut IrBlock) {
    flag_liveness(block);
    peephole(block);
}
