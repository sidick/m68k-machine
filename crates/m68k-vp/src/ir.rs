//! The IR: a flat, fixed-size array of ops per basic block.
//!
//! Follows docs/cpu-core-proposal.md §4.2's shape where the kernel
//! instruction surface (m68k/cpubench/kernels.s) exercises it, and departs
//! from it in a few documented, narrow ways -- see
//! docs/cpu-core-poc-results.md's "Spec issues found" section for the
//! reasoning, not repeated here. In short: this PoC's ops sit at roughly
//! one-m68k-instruction granularity (a `MoveMem`/`LoadReg` op resolves its
//! own addressing-mode side effects, e.g. `(An)+`) rather than the
//! proposal's fully decomposed load/compute/store shape, because decomposing
//! further added op-dispatch count without changing what this PoC measures
//! (interpreter dispatch and flag-liveness cost) -- except for `movem`,
//! which *is* expanded into one micro-op per register exactly as the task
//! brief requires, with `retire` marking only the last micro-op of each
//! source instruction so retired-instruction accounting stays at
//! real-68k-instruction granularity regardless of IR shape (`interp.rs`).
//!
//! Register file: unified index 0..16, 0..8 = D0-D7, 8..16 = A0-A7 -- the
//! front end declares this shape per §4.2's "register file is declared by
//! the front end" neutrality decision. Flags-wanted mask per op (the `flags`
//! field on flag-producing ops) is written conservatively by the decoder
//! (`F_ALL`) and narrowed by the backward liveness pass (`passes.rs`).

/// Registers 0..=7 are D0..=D7; 8..=15 are A0..=A7.
pub type Reg = u8;

#[inline]
pub const fn dreg(n: u8) -> Reg {
    n
}

#[inline]
pub const fn areg(n: u8) -> Reg {
    8 + n
}

/// Operand size. Only what the kernels use: byte/word loads into a
/// register (`struct_walk`), word multiply/divide operands, and long
/// everywhere else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    B,
    W,
    L,
}

impl Size {
    #[inline]
    pub const fn bytes(self) -> u32 {
        match self {
            Size::B => 1,
            Size::W => 2,
            Size::L => 4,
        }
    }

    #[inline]
    pub const fn mask(self) -> u32 {
        match self {
            Size::B => 0xFF,
            Size::W => 0xFFFF,
            Size::L => 0xFFFF_FFFF,
        }
    }

    #[inline]
    pub const fn sign_bit(self) -> u32 {
        match self {
            Size::B => 0x80,
            Size::W => 0x8000,
            Size::L => 0x8000_0000,
        }
    }
}

/// Flags-wanted mask bits, CCR-encoded (matches the real 68k CCR bit
/// positions -- not required for correctness here since the differential
/// gate compares D/A/PC/RAM, not SR, but it costs nothing and makes the
/// liveness pass's output legible against a disassembly).
pub const F_C: u8 = 1 << 0;
pub const F_V: u8 = 1 << 1;
pub const F_Z: u8 = 1 << 2;
pub const F_N: u8 = 1 << 3;
pub const F_X: u8 = 1 << 4;
pub const F_ALL: u8 = F_C | F_V | F_Z | F_N | F_X;
pub const F_NONE: u8 = 0;

/// A memory effective address, already classified by the decoder into one
/// of the modes the kernels use (docs/cpu-core-poc-results.md's decoder
/// coverage section has the exhaustive list). `PostInc`/`PreDec` carry
/// their own address-register side effect; the interpreter applies it as
/// part of resolving the address (`interp.rs::resolve_ea`).
#[derive(Clone, Copy, Debug)]
pub enum Ea {
    /// `(An)`
    Ind(Reg),
    /// `(An)+`
    PostInc(Reg),
    /// `-(An)`
    PreDec(Reg),
    /// `d16(An)`
    Disp(Reg, i16),
}

/// A `MoveMem` source: a resolved memory address, a register's current
/// value, or an immediate materialized at decode time.
#[derive(Clone, Copy, Debug)]
pub enum Source {
    Ea(Ea),
    Reg(Reg),
    Imm(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AluBin {
    Add,
    Sub,
    And,
    Or,
    Eor,
    /// Compares only; does not write `dst`.
    Cmp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AluUn {
    Not,
    Neg,
    Tst,
    /// Sign-extend the low word of `dst` to a long (the only `ext` form
    /// the kernels use -- `ext.w` byte->word never appears).
    ExtL,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MulDiv {
    MulU,
    MulS,
    DivU,
    DivS,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BfOp {
    ExtU,
    Ins,
}

/// One IR op. Kept as a single flat enum (no separate metadata side
/// table) with `pc`/`len`/`retire` carried on the wrapping `IrInstr`
/// (below), per §4.2's "per-op source PC and byte length" requirement.
#[derive(Clone, Copy, Debug)]
pub enum Op {
    /// `moveq #imm,Dn`
    MoveqImm { dst: Reg, imm: i32 },
    /// `movea.l An,An` (register-direct copy; also used for the plain
    /// register-to-register moves this workload never actually needs
    /// beyond movea, kept general for robustness).
    RegMove { dst: Reg, src: Reg },
    /// `move.{b,w,l} <ea>,Dn` -- ea is always a memory operand for the
    /// kernels (`struct_walk`); also used for the per-register load half
    /// of an expanded `movem` restore.
    LoadReg { dst: Reg, src: Ea, size: Size },
    /// `move.{b,w,l} <src>,<ea>` -- src is a memory operand or an
    /// immediate (`mem_copy`, `mem_fill`); also used for the per-register
    /// store half of an expanded `movem` save.
    MoveMem { src: Source, dst: Ea, size: Size },
    /// `lea <ea>,An`
    Lea { dst: Reg, ea: Ea },
    /// `add/sub/and/or/eor/cmp <ea-as-Dn>,Dn` register-direct dyadic ALU,
    /// always in the proposal's degenerate `dst == src2` three-operand
    /// form: `dst = dst OP src` (`Cmp` computes flags only).
    AluBinReg {
        op: AluBin,
        dst: Reg,
        src: Reg,
        size: Size,
        flags: u8,
    },
    /// `addq/subq #imm,Dn`
    AluImm {
        op: AluBin,
        dst: Reg,
        imm: i32,
        size: Size,
        flags: u8,
    },
    /// `not/neg/tst/ext Dn`
    AluUnary {
        op: AluUn,
        dst: Reg,
        size: Size,
        flags: u8,
    },
    /// `asl/lsr #count,Dn` (immediate shift count only -- register-count
    /// shifts never appear in the kernels).
    Shift {
        left: bool,
        arith: bool,
        dst: Reg,
        count: u8,
        size: Size,
        flags: u8,
    },
    /// `mulu.w/muls.w/divu.w/divs.w Dn,Dn`
    MulDivOp { op: MulDiv, dst: Reg, src: Reg },
    /// `bfextu (An){off:width},Dn` / `bfins Dn,(An){off:width}` -- base is
    /// always `(An)` (register-indirect, no displacement) and offset/width
    /// are always immediates in this workload.
    Bitfield {
        op: BfOp,
        base: Reg,
        reg: Reg,
        offset: u8,
        width: u8,
    },
    /// `bsr` -- pushes the return address (this op's own `pc + len`) and
    /// jumps to `target`. Block-terminating.
    Bsr { target: u32 },
    /// `bne` -- block-terminating; `fallthrough` is `pc + len` of this op.
    Bne { target: u32, fallthrough: u32 },
    /// `rts` -- pops the return address off the guest stack into PC.
    /// Block-terminating.
    Rts,
    /// A true no-op that still retires one instruction -- decode target
    /// for the `nop` opcode (0x4E71, never actually emitted by this
    /// benchmark's assembled blob, see decoder.rs) and for the
    /// `lea (An),An` self-reference peephole (passes.rs).
    Nop,
}

/// One slot in an `IrBlock`: the op plus the metadata the proposal
/// requires per op (§4.2: "source PC, byte length"), plus `retire`, this
/// PoC's mechanism for keeping retired-instruction accounting at
/// real-68k-instruction granularity when one instruction (`movem`)
/// expands to several IR ops -- only the last micro-op of a source
/// instruction retires.
#[derive(Clone, Copy, Debug)]
pub struct IrInstr {
    pub op: Op,
    pub pc: u32,
    pub len: u8,
    pub retire: bool,
}

impl IrInstr {
    pub const fn nop_retiring(pc: u32, len: u8) -> Self {
        IrInstr {
            op: Op::Nop,
            pc,
            len,
            retire: true,
        }
    }
}

/// Bounded per-block IR arena. Sized against `movem_saverestore`, the
/// largest expansion in this kernel set: 3 * (1 lea + 7-register store +
/// 7-register load) + subq + bne + rts = 48 micro-ops. 64 leaves headroom
/// without the block cache (`interp.rs`) costing much static memory.
pub const MAX_OPS: usize = 64;

#[derive(Clone, Copy)]
pub struct IrBlock {
    pub instrs: [IrInstr; MAX_OPS],
    pub count: usize,
    pub entry_pc: u32,
}

impl IrBlock {
    pub const fn empty() -> Self {
        IrBlock {
            instrs: [IrInstr::nop_retiring(0, 0); MAX_OPS],
            count: 0,
            entry_pc: 0,
        }
    }

    /// Push one micro-op. Panics on overflow -- a block bigger than
    /// `MAX_OPS` is a decoder/kernel-set mismatch, not a runtime
    /// condition (this PoC's kernel set is fixed and known; a production
    /// core would cap the block and stop cleanly instead).
    pub fn push(&mut self, instr: IrInstr) {
        assert!(
            self.count < MAX_OPS,
            "IR block at pc={:#x} exceeded MAX_OPS={MAX_OPS} -- \
             either a new kernel needs a bigger arena or the decoder \
             looped without hitting a control transfer",
            self.entry_pc
        );
        self.instrs[self.count] = instr;
        self.count += 1;
    }
}
