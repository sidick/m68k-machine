//! 68k -> IR decoder.
//!
//! Decodes one basic block at a time (§4.2: "up to the next control
//! transfer or a fixed op limit"), covering exactly the opcode encodings
//! `m68k/cpubench/kernels.s` assembles to -- verified against the
//! assembled blob with Capstone, not against the source comments, which
//! turned out to describe something the assembler does not actually do
//! (see docs/cpu-core-poc-results.md's decoder-coverage section: the
//! "folds to NOP" claim for a zero-displacement `bne.s`/`beq.s` etc. is
//! stale -- vasm instead emits `lea (An),An`, decoded here like any other
//! `lea` and turned into a true no-op by the peephole pass, `passes.rs`).
//!
//! Unsupported opcodes panic with the offending word and PC -- this PoC's
//! kernel set is fixed and known, so an unrecognized opcode is a decoder
//! bug or a kernel-set mismatch, not a runtime condition to fail closed
//! on (that rule governs guest-visible devices, CLAUDE.md's
//! device-ledger; this is bench/test scaffolding, and a loud panic beats
//! a silently wrong decode of a benchmark that exists to produce a
//! trustworthy number).

use crate::ir::{
    areg, dreg, AluBin, AluUn, BfOp, Ea, IrBlock, IrInstr, MulDiv, Op, Reg, Size, Source, F_ALL,
};
use crate::mem::VpMemory;

enum DstKind {
    Reg(Reg),
    Ea(Ea),
}

fn decode_src(mem: &VpMemory, cursor: &mut u32, mode: u16, reg: u16, size: Size) -> Source {
    match mode {
        0 => Source::Reg(dreg(reg as u8)),
        1 => Source::Reg(areg(reg as u8)),
        2 => Source::Ea(Ea::Ind(areg(reg as u8))),
        3 => Source::Ea(Ea::PostInc(areg(reg as u8))),
        4 => Source::Ea(Ea::PreDec(areg(reg as u8))),
        5 => {
            let d = mem.read_u16(*cursor) as i16;
            *cursor = cursor.wrapping_add(2);
            Source::Ea(Ea::Disp(areg(reg as u8), d))
        }
        7 if reg == 4 => match size {
            Size::L => {
                let v = mem.read_u32(*cursor);
                *cursor = cursor.wrapping_add(4);
                Source::Imm(v)
            }
            _ => {
                let v = mem.read_u16(*cursor) as u32;
                *cursor = cursor.wrapping_add(2);
                Source::Imm(v & size.mask())
            }
        },
        _ => panic!("m68k-vp decoder: unsupported source EA mode={mode} reg={reg}"),
    }
}

fn decode_dst(mem: &VpMemory, cursor: &mut u32, mode: u16, reg: u16) -> DstKind {
    match mode {
        0 => DstKind::Reg(dreg(reg as u8)),
        1 => DstKind::Reg(areg(reg as u8)),
        2 => DstKind::Ea(Ea::Ind(areg(reg as u8))),
        3 => DstKind::Ea(Ea::PostInc(areg(reg as u8))),
        4 => DstKind::Ea(Ea::PreDec(areg(reg as u8))),
        5 => {
            let d = mem.read_u16(*cursor) as i16;
            *cursor = cursor.wrapping_add(2);
            DstKind::Ea(Ea::Disp(areg(reg as u8), d))
        }
        _ => panic!("m68k-vp decoder: unsupported dest EA mode={mode} reg={reg}"),
    }
}

/// `movem` register-list bit -> unified register index, per addressing
/// mode. Derived in docs/cpu-core-poc-results.md from the assembled
/// blob's own predecrement/postincrement masks: for predecrement the
/// mask is reversed (bit k => register 15-k, A7 first), for every other
/// mode it is direct (bit k => register k, D0 first).
fn movem_reg_for_bit(predec: bool, k: u8) -> Reg {
    if predec {
        15 - k
    } else {
        k
    }
}

fn decode_movem(block: &mut IrBlock, mem: &VpMemory, pc: u32, op1: u16) -> u32 {
    let d = (op1 >> 10) & 1; // 0 = store (reg -> mem), 1 = load (mem -> reg)
    let mode = (op1 >> 3) & 7;
    let areg_num = (op1 & 7) as u8;
    let mask = mem.read_u16(pc.wrapping_add(2));
    let len: u8 = 4;

    let store = d == 0;
    if store {
        assert_eq!(
            mode, 4,
            "m68k-vp decoder: movem store expects -(An) at pc={pc:#x}"
        );
    } else {
        assert_eq!(
            mode, 3,
            "m68k-vp decoder: movem load expects (An)+ at pc={pc:#x}"
        );
    }
    let an = areg(areg_num);

    // Collect the selected registers in real-hardware processing order,
    // then push one micro-op per register (§4.2's expansion requirement
    // for `movem`); only the last one retires (ir.rs's `retire` field).
    let mut selected = [0u8; 16];
    let mut n = 0usize;
    for k in 0..16u8 {
        if mask & (1 << k) != 0 {
            selected[n] = movem_reg_for_bit(store, k);
            n += 1;
        }
    }
    if n == 0 {
        // Degenerate empty register list: still retires as one instruction.
        block.push(IrInstr {
            op: Op::Nop,
            pc,
            len,
            retire: true,
        });
        return u32::from(len);
    }
    for (i, &reg) in selected[..n].iter().enumerate() {
        let retire = i + 1 == n;
        let op = if store {
            Op::MoveMem {
                src: Source::Reg(reg),
                dst: Ea::PreDec(an),
                size: Size::L,
            }
        } else {
            Op::LoadReg {
                dst: reg,
                src: Ea::PostInc(an),
                size: Size::L,
            }
        };
        block.push(IrInstr {
            op,
            pc,
            len,
            retire,
        });
    }
    u32::from(len)
}

fn decode_bitfield(mem: &VpMemory, pc: u32, op1: u16) -> (Op, u32) {
    let sub = (op1 >> 8) & 7;
    let mode = (op1 >> 3) & 7;
    let reg = (op1 & 7) as u8;
    assert_eq!(
        mode, 2,
        "m68k-vp decoder: bitfield base must be (An) at pc={pc:#x}"
    );
    let base = areg(reg);
    let ext = mem.read_u16(pc.wrapping_add(2));
    let dreg_field = ((ext >> 12) & 7) as u8;
    let off_is_reg = (ext >> 11) & 1;
    let width_is_reg = (ext >> 5) & 1;
    assert_eq!(
        off_is_reg, 0,
        "m68k-vp decoder: register-indexed bitfield offset unsupported at pc={pc:#x}"
    );
    assert_eq!(
        width_is_reg, 0,
        "m68k-vp decoder: register-indexed bitfield width unsupported at pc={pc:#x}"
    );
    let offset = ((ext >> 6) & 0x1F) as u8;
    let width_raw = (ext & 0x1F) as u8;
    let width = if width_raw == 0 { 32 } else { width_raw };
    let op = match sub {
        1 => Op::Bitfield {
            op: BfOp::ExtU,
            base,
            reg: dreg(dreg_field),
            offset,
            width,
        },
        7 => Op::Bitfield {
            op: BfOp::Ins,
            base,
            reg: dreg(dreg_field),
            offset,
            width,
        },
        _ => panic!("m68k-vp decoder: unsupported bitfield sub-op {sub} at pc={pc:#x}"),
    };
    (op, 4)
}

fn decode_branch_target(mem: &VpMemory, pc: u32, op1: u16) -> (u32, u32) {
    let disp_byte = (op1 & 0xFF) as u8;
    if disp_byte == 0 {
        let d = mem.read_u16(pc.wrapping_add(2)) as i16 as i32;
        let target = (pc as i32).wrapping_add(2).wrapping_add(d) as u32;
        (target, 4)
    } else {
        let d = (disp_byte as i8) as i32;
        let target = (pc as i32).wrapping_add(2).wrapping_add(d) as u32;
        (target, 2)
    }
}

/// Decode exactly one m68k instruction at `pc`, pushing its IR op(s) into
/// `block`. Returns `(len, is_terminator)`.
fn decode_one(block: &mut IrBlock, mem: &VpMemory, pc: u32) -> (u32, bool) {
    let op1 = mem.read_u16(pc);

    if op1 == 0x4E75 {
        block.push(IrInstr {
            op: Op::Rts,
            pc,
            len: 2,
            retire: true,
        });
        return (2, true);
    }
    if op1 == 0x4E71 {
        block.push(IrInstr::nop_retiring(pc, 2));
        return (2, false);
    }
    if (op1 >> 8) == 0x66 {
        let (target, len) = decode_branch_target(mem, pc, op1);
        block.push(IrInstr {
            op: Op::Bne {
                target,
                fallthrough: pc.wrapping_add(len),
            },
            pc,
            len: len as u8,
            retire: true,
        });
        return (len, true);
    }
    if (op1 >> 8) == 0x61 {
        let (target, len) = decode_branch_target(mem, pc, op1);
        block.push(IrInstr {
            op: Op::Bsr { target },
            pc,
            len: len as u8,
            retire: true,
        });
        return (len, true);
    }

    let nib = op1 >> 12;
    match nib {
        0x5 => {
            let size2 = (op1 >> 6) & 3;
            assert_ne!(
                size2, 3,
                "m68k-vp decoder: Scc/DBcc unsupported at pc={pc:#x}"
            );
            let size = match size2 {
                0 => Size::B,
                1 => Size::W,
                2 => Size::L,
                _ => unreachable!(),
            };
            let mode = (op1 >> 3) & 7;
            let reg = op1 & 7;
            assert_eq!(
                mode, 0,
                "m68k-vp decoder: addq/subq to memory unsupported at pc={pc:#x}"
            );
            let qqq = (op1 >> 9) & 7;
            let imm = if qqq == 0 { 8 } else { qqq as i32 };
            let is_sub = (op1 >> 8) & 1 == 1;
            block.push(IrInstr {
                op: Op::AluImm {
                    op: if is_sub { AluBin::Sub } else { AluBin::Add },
                    dst: dreg(reg as u8),
                    imm,
                    size,
                    flags: F_ALL,
                },
                pc,
                len: 2,
                retire: true,
            });
            (2, false)
        }
        0x7 => {
            assert_eq!(
                (op1 >> 8) & 1,
                0,
                "m68k-vp decoder: malformed moveq at pc={pc:#x}"
            );
            let reg = (op1 >> 9) & 7;
            let imm = (op1 as i8) as i32;
            block.push(IrInstr {
                op: Op::MoveqImm {
                    dst: dreg(reg as u8),
                    imm,
                },
                pc,
                len: 2,
                retire: true,
            });
            (2, false)
        }
        0x1..=0x3 => {
            let size = match nib {
                0x1 => Size::B,
                0x3 => Size::W,
                0x2 => Size::L,
                _ => unreachable!(),
            };
            let dst_reg = (op1 >> 9) & 7;
            let dst_mode = (op1 >> 6) & 7;
            let src_mode = (op1 >> 3) & 7;
            let src_reg = op1 & 7;
            let mut cursor = pc.wrapping_add(2);
            let src = decode_src(mem, &mut cursor, src_mode, src_reg, size);
            let dst = decode_dst(mem, &mut cursor, dst_mode, dst_reg);
            let len = cursor.wrapping_sub(pc);
            let op = match (src, dst) {
                (Source::Reg(s), DstKind::Reg(d)) => {
                    assert_eq!(
                        size,
                        Size::L,
                        "m68k-vp decoder: register-to-register move only implemented for .L at pc={pc:#x}"
                    );
                    Op::RegMove { dst: d, src: s }
                }
                (src_val, DstKind::Ea(ea)) => Op::MoveMem {
                    src: src_val,
                    dst: ea,
                    size,
                },
                (Source::Ea(ea), DstKind::Reg(d)) => {
                    assert!(
                        d < 8,
                        "m68k-vp decoder: load into An unsupported at pc={pc:#x}"
                    );
                    Op::LoadReg {
                        dst: d,
                        src: ea,
                        size,
                    }
                }
                (Source::Imm(_), DstKind::Reg(_)) => {
                    panic!("m68k-vp decoder: move #imm,An/Dn should decode as moveq/movea elsewhere, pc={pc:#x}")
                }
            };
            block.push(IrInstr {
                op,
                pc,
                len: len as u8,
                retire: true,
            });
            (len, false)
        }
        0x4 => {
            if op1 & 0xFFF8 == 0x4A80 {
                let reg = (op1 & 7) as u8;
                block.push(IrInstr {
                    op: Op::AluUnary {
                        op: AluUn::Tst,
                        dst: dreg(reg),
                        size: Size::L,
                        flags: F_ALL,
                    },
                    pc,
                    len: 2,
                    retire: true,
                });
                (2, false)
            } else if op1 & 0xFFF8 == 0x4680 {
                let reg = (op1 & 7) as u8;
                block.push(IrInstr {
                    op: Op::AluUnary {
                        op: AluUn::Not,
                        dst: dreg(reg),
                        size: Size::L,
                        flags: F_ALL,
                    },
                    pc,
                    len: 2,
                    retire: true,
                });
                (2, false)
            } else if op1 & 0xFFF8 == 0x4480 {
                let reg = (op1 & 7) as u8;
                block.push(IrInstr {
                    op: Op::AluUnary {
                        op: AluUn::Neg,
                        dst: dreg(reg),
                        size: Size::L,
                        flags: F_ALL,
                    },
                    pc,
                    len: 2,
                    retire: true,
                });
                (2, false)
            } else if op1 & 0xFFF8 == 0x48C0 {
                // Must be checked before the `movem` mask below: `ext.l`
                // opcodes (0x48C0-0x48C7) sit inside movem's fixed-bit
                // mask too (movem's "mode" field reads as 000, which is
                // never a legal movem addressing mode on real hardware,
                // but the mask alone can't tell the difference).
                let reg = (op1 & 7) as u8;
                block.push(IrInstr {
                    op: Op::AluUnary {
                        op: AluUn::ExtL,
                        dst: dreg(reg),
                        size: Size::L,
                        flags: F_ALL,
                    },
                    pc,
                    len: 2,
                    retire: true,
                });
                (2, false)
            } else if op1 & 0xF1C0 == 0x41C0 {
                let dst = (op1 >> 9) & 7;
                let mode = (op1 >> 3) & 7;
                let reg = op1 & 7;
                let mut cursor = pc.wrapping_add(2);
                let ea = match mode {
                    2 => Ea::Ind(areg(reg as u8)),
                    5 => {
                        let d = mem.read_u16(cursor) as i16;
                        cursor = cursor.wrapping_add(2);
                        Ea::Disp(areg(reg as u8), d)
                    }
                    _ => panic!("m68k-vp decoder: unsupported lea EA mode={mode} at pc={pc:#x}"),
                };
                let len = cursor.wrapping_sub(pc);
                block.push(IrInstr {
                    op: Op::Lea {
                        dst: areg(dst as u8),
                        ea,
                    },
                    pc,
                    len: len as u8,
                    retire: true,
                });
                (len, false)
            } else if op1 & 0xFB80 == 0x4880 {
                let len = decode_movem(block, mem, pc, op1);
                (len, false)
            } else {
                panic!("m68k-vp decoder: unsupported opcode {op1:#06x} at pc={pc:#x}");
            }
        }
        0x8 | 0x9 | 0xB | 0xC | 0xD => {
            let opmode = (op1 >> 6) & 7;
            let mode = (op1 >> 3) & 7;
            let reg_field = (op1 >> 9) & 7;
            let ea_reg = op1 & 7;

            let op = match (nib, opmode) {
                (0xD, 2) => {
                    assert_eq!(mode, 0, "m68k-vp decoder: add to memory unsupported at pc={pc:#x}");
                    Op::AluBinReg {
                        op: AluBin::Add,
                        dst: dreg(reg_field as u8),
                        src: dreg(ea_reg as u8),
                        size: Size::L,
                        flags: F_ALL,
                    }
                }
                (0x9, 2) => {
                    assert_eq!(mode, 0, "m68k-vp decoder: sub to memory unsupported at pc={pc:#x}");
                    Op::AluBinReg {
                        op: AluBin::Sub,
                        dst: dreg(reg_field as u8),
                        src: dreg(ea_reg as u8),
                        size: Size::L,
                        flags: F_ALL,
                    }
                }
                (0x8, 2) => {
                    assert_eq!(mode, 0, "m68k-vp decoder: or to memory unsupported at pc={pc:#x}");
                    Op::AluBinReg {
                        op: AluBin::Or,
                        dst: dreg(reg_field as u8),
                        src: dreg(ea_reg as u8),
                        size: Size::L,
                        flags: F_ALL,
                    }
                }
                (0x8, 3) => Op::MulDivOp {
                    op: MulDiv::DivU,
                    dst: dreg(reg_field as u8),
                    src: dreg(ea_reg as u8),
                },
                (0x8, 7) => Op::MulDivOp {
                    op: MulDiv::DivS,
                    dst: dreg(reg_field as u8),
                    src: dreg(ea_reg as u8),
                },
                (0xC, 2) => {
                    assert_eq!(mode, 0, "m68k-vp decoder: and to memory unsupported at pc={pc:#x}");
                    Op::AluBinReg {
                        op: AluBin::And,
                        dst: dreg(reg_field as u8),
                        src: dreg(ea_reg as u8),
                        size: Size::L,
                        flags: F_ALL,
                    }
                }
                (0xC, 3) => Op::MulDivOp {
                    op: MulDiv::MulU,
                    dst: dreg(reg_field as u8),
                    src: dreg(ea_reg as u8),
                },
                (0xC, 7) => Op::MulDivOp {
                    op: MulDiv::MulS,
                    dst: dreg(reg_field as u8),
                    src: dreg(ea_reg as u8),
                },
                (0xB, 2) => {
                    assert_eq!(mode, 0, "m68k-vp decoder: cmp to memory unsupported at pc={pc:#x}");
                    Op::AluBinReg {
                        op: AluBin::Cmp,
                        dst: dreg(reg_field as u8),
                        src: dreg(ea_reg as u8),
                        size: Size::L,
                        flags: F_ALL,
                    }
                }
                (0xB, 6) => {
                    assert_eq!(mode, 0, "m68k-vp decoder: eor to memory unsupported at pc={pc:#x}");
                    Op::AluBinReg {
                        op: AluBin::Eor,
                        dst: dreg(ea_reg as u8),
                        src: dreg(reg_field as u8),
                        size: Size::L,
                        flags: F_ALL,
                    }
                }
                _ => panic!(
                    "m68k-vp decoder: unsupported opcode {op1:#06x} (nib={nib:#x} opmode={opmode}) at pc={pc:#x}"
                ),
            };
            block.push(IrInstr {
                op,
                pc,
                len: 2,
                retire: true,
            });
            (2, false)
        }
        0xE => {
            if op1 & 0xF8C0 == 0xE8C0 {
                let (op, len) = decode_bitfield(mem, pc, op1);
                block.push(IrInstr {
                    op,
                    pc,
                    len: len as u8,
                    retire: true,
                });
                (len, false)
            } else {
                let count3 = (op1 >> 9) & 7;
                let count = if count3 == 0 { 8 } else { count3 as u8 };
                let dir_left = (op1 >> 8) & 1 == 1;
                let size2 = (op1 >> 6) & 3;
                assert_eq!(
                    size2, 2,
                    "m68k-vp decoder: only .L register shifts implemented at pc={pc:#x}"
                );
                let ir = (op1 >> 5) & 1;
                assert_eq!(
                    ir, 0,
                    "m68k-vp decoder: register-count shifts unsupported at pc={pc:#x}"
                );
                let ty = (op1 >> 3) & 3;
                assert!(
                    ty <= 1,
                    "m68k-vp decoder: only ASx/LSx shifts implemented at pc={pc:#x}"
                );
                let reg = op1 & 7;
                block.push(IrInstr {
                    op: Op::Shift {
                        left: dir_left,
                        arith: ty == 0,
                        dst: dreg(reg as u8),
                        count,
                        size: Size::L,
                        flags: F_ALL,
                    },
                    pc,
                    len: 2,
                    retire: true,
                });
                (2, false)
            }
        }
        _ => panic!("m68k-vp decoder: unsupported opcode {op1:#06x} at pc={pc:#x}"),
    }
}

/// Decode a basic block starting at `entry_pc`: straight-line ops until a
/// control-transfer instruction (`bsr`/`bne`/`rts`), or `ir::MAX_OPS` is
/// hit (§4.2).
pub fn decode_block(mem: &VpMemory, entry_pc: u32) -> IrBlock {
    let mut block = IrBlock::empty();
    block.entry_pc = entry_pc;
    let mut pc = entry_pc;
    loop {
        let (len, terminator) = decode_one(&mut block, mem, pc);
        pc = pc.wrapping_add(len);
        if terminator || block.count >= crate::ir::MAX_OPS {
            break;
        }
    }
    block
}
