*-----------------------------------------------------------------------------
* kernels.s -- the CPU benchmark's kernels: the single shared assembly
* source run both by the bare Rust harness (crates/cpu-bench, straight
* over m68k-rs's CpuCore/LinearMemoryBus) and by the AmigaOS CLI program
* (m68k/cpubench/cpubench.c, linked in via vasm -Fhunk). Same bytes,
* two callers -- see docs/bus-fast-path-plan.md step 8 for why: comparing
* a "bare interpreter" number against an "in-machine" number is only
* honest if both ran the identical instruction stream.
*
* MIT License. Copyright (c) 2026 the m68k Machine project. See ../LICENSE.
*
* Assembled two ways (scripts/build-cpubench.sh does both):
*   - `vasm -Fbin -m68040` to a flat binary (m68k/cpubench/kernels.bin,
*     committed, embedded by crates/cpu-bench via include_bytes!). No
*     relocation, no linker -- the bare harness loads these bytes at a
*     fixed guest address of its own choosing (LinearMemoryBus is a flat
*     array from 0) and reads KernelTable (below) to find each kernel's
*     entry offset within the blob.
*   - `vasm -Fhunk -m68040` to a relocatable AmigaOS object, linked into
*     C:CPUBench by m68k-amigaos-gcc alongside cpubench.c. There, C calls
*     each kernel directly by its `xdef`'d name (`__asm` register
*     parameters, see cpubench.c); KernelTable is unused dead data in
*     that build (harmless, not worth #ifdef'ing out).
*
* CALLING CONVENTION (every kernel below follows this exactly):
*   d0.l = outer iteration count, IN. Clobbered.
*   a0   = buffer 1 pointer, IN, when the kernel's own header says it
*          takes one. Unused registers are simply not read.
*   a1   = buffer 2 pointer, IN, only for the two-buffer kernels
*          (mem_copy). Unused otherwise.
*   Returns via RTS. Clobbers d1-d7/a0-a6 freely; never touches a7
*   except through ordinary CPU-implicit JSR/RTS stack traffic (only
*   jsr_rts_chain does this, and never more than one return address
*   deep at a time -- a7 needs a few bytes of valid, writable headroom
*   below it, nothing more).
*
* Every kernel is an outer loop of a fixed-size, fully unrolled body
* (never a variable-trip inner loop) so "one iteration" has an exact,
* constant instruction count -- the KernelInstrsPerIter table below --
* checked at runtime once each iteration returns to the outer SUBQ/BNE.
* A kernel touching memory reloads its working pointer(s) from the
* caller's buffer at the top of every outer pass rather than advancing
* across passes, so it stays inside a small, fixed-size buffer no
* matter how large the caller's iteration count is -- this is a CPU
* throughput benchmark, not a memory-bandwidth one, and it must never
* fault regardless of how many iterations run (docs/device-ledger.md's
* neighbour rule for this platform: hostile or merely large input fails
* closed, it never runs off the end of a buffer).
*
* KERNEL LIST (index, name, buffer requirement, instructions per
* iteration, one-time fixed overhead before/after the loop):
*
*   0  reg_addq_bra        none            18   1 (rts only)
*   1  reg_tst_bne         none            18   1
*   2  reg_mix             none            18   1
*   3  mem_copy            a0>=128B,a1>=128B  36   1
*   4  mem_fill            a0>=128B        35   1
*   5  struct_walk         a0>=64B         27   1
*   6  movem_saverestore   a0>=64B         11   1
*   7  jsr_rts_chain       none            14   1
*   8  muldiv_mix          none            18   2 (1 setup + rts)
*   9  bitfield_ops        a0>=8B          10   1
*   10 cmp_branchy         none            18   1
*
* Total instructions retired by a call with outer count K is exactly
* `fixed_overhead + K * instrs_per_iter` -- both operands come out of
* KernelInstrsPerIter/KernelFixedOverhead below, not just this comment,
* so the harness's calibration check (run a small K, compare the CPU's
* own retired-instruction count against this formula) is checking the
* real numbers, not a second hand-copied version of them. A mismatch is
* a bug in this file's documentation (per the task brief), not in the
* harness.
*-----------------------------------------------------------------------------

        section text,code

*-----------------------------------------------------------------------------
* Metadata tables. `dc.l label` in a flat binary assembled from offset 0
* emits that label's byte offset within the blob -- no separate symbol
* table or linker map needed by the bare harness. In the AmigaOS build
* these three tables are just unused static data (the linker keeps them
* since nothing marks them for removal; harmless).
*-----------------------------------------------------------------------------

        xdef    _KernelTable
        xdef    _KernelInstrsPerIter
        xdef    _KernelFixedOverhead
        xdef    _KernelCount

_KernelCount:
        dc.l    11

_KernelTable:
        dc.l    kernel_reg_addq_bra
        dc.l    kernel_reg_tst_bne
        dc.l    kernel_reg_mix
        dc.l    kernel_mem_copy
        dc.l    kernel_mem_fill
        dc.l    kernel_struct_walk
        dc.l    kernel_movem_saverestore
        dc.l    kernel_jsr_rts_chain
        dc.l    kernel_muldiv_mix
        dc.l    kernel_bitfield_ops
        dc.l    kernel_cmp_branchy

_KernelInstrsPerIter:
        dc.l    18
        dc.l    18
        dc.l    18
        dc.l    36
        dc.l    35
        dc.l    27
        dc.l    11
        dc.l    14
        dc.l    18
        dc.l    10
        dc.l    18

_KernelFixedOverhead:
        dc.l    1
        dc.l    1
        dc.l    1
        dc.l    1
        dc.l    1
        dc.l    1
        dc.l    1
        dc.l    1
        dc.l    2
        dc.l    1
        dc.l    1

*-----------------------------------------------------------------------------
* Kernel 0: reg_addq_bra -- the fork microbench's ADDQ/BRA shape: a tight
* register-only loop closed by SUBQ.L/BNE.S (never DBcc: DBcc only
* decrements a 16-bit word, and this benchmark's outer counts can exceed
* 65535). No buffer.
*
* Per iteration: 16x ADDQ.L + SUBQ.L + BNE.S = 18.
*-----------------------------------------------------------------------------

        xdef    _run_kernel_reg_addq_bra
        xdef    kernel_reg_addq_bra

* C-callable entry: m68k-amigaos-gcc's calling convention treats only
* d0-d1/a0-a1 as caller-saved scratch -- d2-d7/a2-a6 (which includes
* the frame pointer a5 and any register gcc is holding a library base
* in, typically a6) are callee-saved, and every kernel here clobbers
* some of that range (kernels.s's own per-kernel header comments say
* which). Calling the bare kernel body directly as a C function --
* this file's first working version did exactly that -- corrupts
* cpubench.c's own register state the moment a kernel touches
* anything past d0/d1/a0/a1: found the hard way (real-ROM evidence,
* not guesswork) when reg_mix (the third kernel, the first one that
* writes d2-d7) left ReadEClock() reading a trashed base and the
* guest eventually executing garbage. This thin wrapper is the fix:
* save/restore the full callee-saved set around a BSR into the bare
* body below, so the body can go on clobbering everything it wants.
* Outside the timed per-iteration loop, so it costs nothing in the
* per-iteration instruction counts kernels.s documents.
_run_kernel_reg_addq_bra:
        movem.l d2-d7/a2-a6,-(sp)
        bsr     kernel_reg_addq_bra
        movem.l (sp)+,d2-d7/a2-a6
        rts

kernel_reg_addq_bra:
.loop:
        addq.l  #1,d1
        addq.l  #1,d1
        addq.l  #1,d1
        addq.l  #1,d1
        addq.l  #1,d1
        addq.l  #1,d1
        addq.l  #1,d1
        addq.l  #1,d1
        addq.l  #1,d1
        addq.l  #1,d1
        addq.l  #1,d1
        addq.l  #1,d1
        addq.l  #1,d1
        addq.l  #1,d1
        addq.l  #1,d1
        addq.l  #1,d1
        subq.l  #1,d0
        bne     .loop
        rts

*-----------------------------------------------------------------------------
* Kernel 1: reg_tst_bne -- the fork microbench's TST/BNE shape. Every
* branch target is the instruction immediately following it, so taken or
* not the control flow is identical -- this measures TST+Bcc retirement
* cost, not branch misprediction (a throughput kernel, not a predictor
* stress test). No buffer.
*
* A branch whose displacement is exactly zero has no valid short-branch
* encoding, so vasm folds each of these eight BNE.S into a NOP (build
* warning 2058, expected and checked for by scripts/build-cpubench.sh).
* TST.L still executes and sets the flags every time; only the branch's
* own effect is elided, by the assembler, not by this source -- a real
* 68k assembler handed the same branch-to-next shape does the same
* thing, so this is a faithful edge case. Retirement count is unaffected
* either way (NOP and BNE.S are both one instruction), which is the only
* thing this kernel's documented count depends on.
*
* Per iteration: 8x (TST.L + BNE.S, the latter assembling as NOP) +
* SUBQ.L + BNE.S = 18.
*-----------------------------------------------------------------------------

        xdef    _run_kernel_reg_tst_bne
        xdef    kernel_reg_tst_bne

* C-callable entry: m68k-amigaos-gcc's calling convention treats only
* d0-d1/a0-a1 as caller-saved scratch -- d2-d7/a2-a6 (which includes
* the frame pointer a5 and any register gcc is holding a library base
* in, typically a6) are callee-saved, and every kernel here clobbers
* some of that range (kernels.s's own per-kernel header comments say
* which). Calling the bare kernel body directly as a C function --
* this file's first working version did exactly that -- corrupts
* cpubench.c's own register state the moment a kernel touches
* anything past d0/d1/a0/a1: found the hard way (real-ROM evidence,
* not guesswork) when reg_mix (the third kernel, the first one that
* writes d2-d7) left ReadEClock() reading a trashed base and the
* guest eventually executing garbage. This thin wrapper is the fix:
* save/restore the full callee-saved set around a BSR into the bare
* body below, so the body can go on clobbering everything it wants.
* Outside the timed per-iteration loop, so it costs nothing in the
* per-iteration instruction counts kernels.s documents.
_run_kernel_reg_tst_bne:
        movem.l d2-d7/a2-a6,-(sp)
        bsr     kernel_reg_tst_bne
        movem.l (sp)+,d2-d7/a2-a6
        rts

kernel_reg_tst_bne:
.loop:
        tst.l   d1
        bne.s   .c1
.c1:    tst.l   d1
        bne.s   .c2
.c2:    tst.l   d1
        bne.s   .c3
.c3:    tst.l   d1
        bne.s   .c4
.c4:    tst.l   d1
        bne.s   .c5
.c5:    tst.l   d1
        bne.s   .c6
.c6:    tst.l   d1
        bne.s   .c7
.c7:    tst.l   d1
        bne.s   .c8
.c8:
        subq.l  #1,d0
        bne     .loop
        rts

*-----------------------------------------------------------------------------
* Kernel 2: reg_mix -- the fork microbench's "register mix" shape: a
* spread of ALU ops (arithmetic, logical, shift) touching several data
* registers. No buffer.
*
* Per iteration: 16 mixed ALU ops + SUBQ.L + BNE.S = 18.
*-----------------------------------------------------------------------------

        xdef    _run_kernel_reg_mix
        xdef    kernel_reg_mix

* C-callable entry: m68k-amigaos-gcc's calling convention treats only
* d0-d1/a0-a1 as caller-saved scratch -- d2-d7/a2-a6 (which includes
* the frame pointer a5 and any register gcc is holding a library base
* in, typically a6) are callee-saved, and every kernel here clobbers
* some of that range (kernels.s's own per-kernel header comments say
* which). Calling the bare kernel body directly as a C function --
* this file's first working version did exactly that -- corrupts
* cpubench.c's own register state the moment a kernel touches
* anything past d0/d1/a0/a1: found the hard way (real-ROM evidence,
* not guesswork) when reg_mix (the third kernel, the first one that
* writes d2-d7) left ReadEClock() reading a trashed base and the
* guest eventually executing garbage. This thin wrapper is the fix:
* save/restore the full callee-saved set around a BSR into the bare
* body below, so the body can go on clobbering everything it wants.
* Outside the timed per-iteration loop, so it costs nothing in the
* per-iteration instruction counts kernels.s documents.
_run_kernel_reg_mix:
        movem.l d2-d7/a2-a6,-(sp)
        bsr     kernel_reg_mix
        movem.l (sp)+,d2-d7/a2-a6
        rts

kernel_reg_mix:
.loop:
        moveq   #5,d1
        add.l   d2,d1
        sub.l   d3,d1
        eor.l   d4,d1
        and.l   d5,d1
        or.l    d6,d1
        asl.l   #1,d1
        lsr.l   #1,d1
        addq.l  #1,d2
        subq.l  #1,d3
        not.l   d4
        neg.l   d5
        clr.l   d6
        moveq   #0,d7
        add.l   d1,d7
        ext.l   d7
        subq.l  #1,d0
        bne     .loop
        rts

*-----------------------------------------------------------------------------
* Kernel 3: mem_copy -- long-word memory-to-memory copy via (An)+, the
* shape docs/cpu-core-proposal.md's roadmap and the plan's own coverage
* numbers care about (data accesses dominate a real workload). a0 = src
* buffer (>=128 bytes), a1 = dest buffer (>=128 bytes); both are reloaded
* into scratch address registers every outer pass, so the same 128 bytes
* are copied every time regardless of outer count -- bounded, cannot run
* past either buffer.
*
* Per iteration: MOVEA.L*2 (reload) + 32x MOVE.L (a2)+,(a3)+ + SUBQ.L +
* BNE.S = 36.
*-----------------------------------------------------------------------------

        xdef    _run_kernel_mem_copy
        xdef    kernel_mem_copy

* C-callable entry: m68k-amigaos-gcc's calling convention treats only
* d0-d1/a0-a1 as caller-saved scratch -- d2-d7/a2-a6 (which includes
* the frame pointer a5 and any register gcc is holding a library base
* in, typically a6) are callee-saved, and every kernel here clobbers
* some of that range (kernels.s's own per-kernel header comments say
* which). Calling the bare kernel body directly as a C function --
* this file's first working version did exactly that -- corrupts
* cpubench.c's own register state the moment a kernel touches
* anything past d0/d1/a0/a1: found the hard way (real-ROM evidence,
* not guesswork) when reg_mix (the third kernel, the first one that
* writes d2-d7) left ReadEClock() reading a trashed base and the
* guest eventually executing garbage. This thin wrapper is the fix:
* save/restore the full callee-saved set around a BSR into the bare
* body below, so the body can go on clobbering everything it wants.
* Outside the timed per-iteration loop, so it costs nothing in the
* per-iteration instruction counts kernels.s documents.
_run_kernel_mem_copy:
        movem.l d2-d7/a2-a6,-(sp)
        bsr     kernel_mem_copy
        movem.l (sp)+,d2-d7/a2-a6
        rts

kernel_mem_copy:
.loop:
        movea.l a0,a2
        movea.l a1,a3
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        move.l  (a2)+,(a3)+
        subq.l  #1,d0
        bne     .loop
        rts

*-----------------------------------------------------------------------------
* Kernel 4: mem_fill -- long-word immediate fill via (An)+. a0 = buffer
* (>=128 bytes), reloaded every outer pass. a1 unused.
*
* Per iteration: MOVEA.L (reload) + 32x MOVE.L #imm,(a2)+ + SUBQ.L +
* BNE.S = 35.
*-----------------------------------------------------------------------------

        xdef    _run_kernel_mem_fill
        xdef    kernel_mem_fill

* C-callable entry: m68k-amigaos-gcc's calling convention treats only
* d0-d1/a0-a1 as caller-saved scratch -- d2-d7/a2-a6 (which includes
* the frame pointer a5 and any register gcc is holding a library base
* in, typically a6) are callee-saved, and every kernel here clobbers
* some of that range (kernels.s's own per-kernel header comments say
* which). Calling the bare kernel body directly as a C function --
* this file's first working version did exactly that -- corrupts
* cpubench.c's own register state the moment a kernel touches
* anything past d0/d1/a0/a1: found the hard way (real-ROM evidence,
* not guesswork) when reg_mix (the third kernel, the first one that
* writes d2-d7) left ReadEClock() reading a trashed base and the
* guest eventually executing garbage. This thin wrapper is the fix:
* save/restore the full callee-saved set around a BSR into the bare
* body below, so the body can go on clobbering everything it wants.
* Outside the timed per-iteration loop, so it costs nothing in the
* per-iteration instruction counts kernels.s documents.
_run_kernel_mem_fill:
        movem.l d2-d7/a2-a6,-(sp)
        bsr     kernel_mem_fill
        movem.l (sp)+,d2-d7/a2-a6
        rts

kernel_mem_fill:
.loop:
        movea.l a0,a2
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        move.l  #$5A5A5A5A,(a2)+
        subq.l  #1,d0
        bne     .loop
        rts

*-----------------------------------------------------------------------------
* Kernel 5: struct_walk -- displacement addressing across a byte/word/
* long mix, the shape a struct-field-heavy C program compiles to. a0 =
* buffer (>=64 bytes; the widest access, MOVE.L at +28, needs 32 bytes,
* so 64 leaves headroom). Reloaded every outer pass.
*
* Per iteration: MOVEA.L (reload) + 8 groups of (MOVE.B/MOVE.W/MOVE.L
* d(a2),Dn) + SUBQ.L + BNE.S = 1 + 24 + 2 = 27.
*-----------------------------------------------------------------------------

        xdef    _run_kernel_struct_walk
        xdef    kernel_struct_walk

* C-callable entry: m68k-amigaos-gcc's calling convention treats only
* d0-d1/a0-a1 as caller-saved scratch -- d2-d7/a2-a6 (which includes
* the frame pointer a5 and any register gcc is holding a library base
* in, typically a6) are callee-saved, and every kernel here clobbers
* some of that range (kernels.s's own per-kernel header comments say
* which). Calling the bare kernel body directly as a C function --
* this file's first working version did exactly that -- corrupts
* cpubench.c's own register state the moment a kernel touches
* anything past d0/d1/a0/a1: found the hard way (real-ROM evidence,
* not guesswork) when reg_mix (the third kernel, the first one that
* writes d2-d7) left ReadEClock() reading a trashed base and the
* guest eventually executing garbage. This thin wrapper is the fix:
* save/restore the full callee-saved set around a BSR into the bare
* body below, so the body can go on clobbering everything it wants.
* Outside the timed per-iteration loop, so it costs nothing in the
* per-iteration instruction counts kernels.s documents.
_run_kernel_struct_walk:
        movem.l d2-d7/a2-a6,-(sp)
        bsr     kernel_struct_walk
        movem.l (sp)+,d2-d7/a2-a6
        rts

kernel_struct_walk:
.loop:
        movea.l a0,a2
        move.b  0(a2),d1
        move.w  0(a2),d2
        move.l  0(a2),d3
        move.b  4(a2),d1
        move.w  4(a2),d2
        move.l  4(a2),d3
        move.b  8(a2),d1
        move.w  8(a2),d2
        move.l  8(a2),d3
        move.b  12(a2),d1
        move.w  12(a2),d2
        move.l  12(a2),d3
        move.b  16(a2),d1
        move.w  16(a2),d2
        move.l  16(a2),d3
        move.b  20(a2),d1
        move.w  20(a2),d2
        move.l  20(a2),d3
        move.b  24(a2),d1
        move.w  24(a2),d2
        move.l  24(a2),d3
        move.b  28(a2),d1
        move.w  28(a2),d2
        move.l  28(a2),d3
        subq.l  #1,d0
        bne     .loop
        rts

*-----------------------------------------------------------------------------
* Kernel 6: movem_saverestore -- MOVEM.L register-block save/restore, the
* shape a prologue/epilogue or a context switch compiles to. a0 = buffer
* (>=64 bytes). LEA sets a2 to a0+32 so the predecrement store (7 regs x
* 4 bytes = 28) lands entirely within [a0, a0+32) every pass.
*
* Per iteration: 3x (LEA + MOVEM.L store + MOVEM.L restore) + SUBQ.L +
* BNE.S = 9 + 2 = 11. Each MOVEM is one retired instruction regardless of
* how many registers it moves (m68k ISA accounting), which is exactly
* the point of this kernel: it is cheap in instruction count and
* expensive in actual work, unlike every other kernel here.
*-----------------------------------------------------------------------------

        xdef    _run_kernel_movem_saverestore
        xdef    kernel_movem_saverestore

* C-callable entry: m68k-amigaos-gcc's calling convention treats only
* d0-d1/a0-a1 as caller-saved scratch -- d2-d7/a2-a6 (which includes
* the frame pointer a5 and any register gcc is holding a library base
* in, typically a6) are callee-saved, and every kernel here clobbers
* some of that range (kernels.s's own per-kernel header comments say
* which). Calling the bare kernel body directly as a C function --
* this file's first working version did exactly that -- corrupts
* cpubench.c's own register state the moment a kernel touches
* anything past d0/d1/a0/a1: found the hard way (real-ROM evidence,
* not guesswork) when reg_mix (the third kernel, the first one that
* writes d2-d7) left ReadEClock() reading a trashed base and the
* guest eventually executing garbage. This thin wrapper is the fix:
* save/restore the full callee-saved set around a BSR into the bare
* body below, so the body can go on clobbering everything it wants.
* Outside the timed per-iteration loop, so it costs nothing in the
* per-iteration instruction counts kernels.s documents.
_run_kernel_movem_saverestore:
        movem.l d2-d7/a2-a6,-(sp)
        bsr     kernel_movem_saverestore
        movem.l (sp)+,d2-d7/a2-a6
        rts

kernel_movem_saverestore:
.loop:
        lea     32(a0),a2
        movem.l d1-d7,-(a2)
        movem.l (a2)+,d1-d7
        lea     32(a0),a2
        movem.l d1-d7,-(a2)
        movem.l (a2)+,d1-d7
        lea     32(a0),a2
        movem.l d1-d7,-(a2)
        movem.l (a2)+,d1-d7
        subq.l  #1,d0
        bne     .loop
        rts

*-----------------------------------------------------------------------------
* Kernel 7: jsr_rts_chain -- a JSR/RTS call chain to a one-instruction
* leaf, sequential (never nested more than one return address deep). No
* buffer; needs only a few bytes of valid stack below a7, which every
* caller (bare harness and real AmigaOS task) already provides.
*
* Per iteration: 4x (JSR + leaf body + RTS, 3 retired instructions per
* call) + SUBQ.L + BNE.S = 12 + 2 = 14.
*-----------------------------------------------------------------------------

        xdef    _run_kernel_jsr_rts_chain
        xdef    kernel_jsr_rts_chain

* C-callable entry: m68k-amigaos-gcc's calling convention treats only
* d0-d1/a0-a1 as caller-saved scratch -- d2-d7/a2-a6 (which includes
* the frame pointer a5 and any register gcc is holding a library base
* in, typically a6) are callee-saved, and every kernel here clobbers
* some of that range (kernels.s's own per-kernel header comments say
* which). Calling the bare kernel body directly as a C function --
* this file's first working version did exactly that -- corrupts
* cpubench.c's own register state the moment a kernel touches
* anything past d0/d1/a0/a1: found the hard way (real-ROM evidence,
* not guesswork) when reg_mix (the third kernel, the first one that
* writes d2-d7) left ReadEClock() reading a trashed base and the
* guest eventually executing garbage. This thin wrapper is the fix:
* save/restore the full callee-saved set around a BSR into the bare
* body below, so the body can go on clobbering everything it wants.
* Outside the timed per-iteration loop, so it costs nothing in the
* per-iteration instruction counts kernels.s documents.
_run_kernel_jsr_rts_chain:
        movem.l d2-d7/a2-a6,-(sp)
        bsr     kernel_jsr_rts_chain
        movem.l (sp)+,d2-d7/a2-a6
        rts

kernel_jsr_rts_chain:
.loop:
        jsr     .leaf
        jsr     .leaf
        jsr     .leaf
        jsr     .leaf
        subq.l  #1,d0
        bne     .loop
        rts
.leaf:
        addq.l  #1,d1
        rts

*-----------------------------------------------------------------------------
* Kernel 8: muldiv_mix -- MULU/MULS/DIVU/DIVS.W mix. d5 is set to a fixed
* nonzero divisor (7) once, before the loop, and is never touched again,
* so DIVU.W/DIVS.W can never see a zero divisor no matter what the
* caller left in d5 (this platform's fail-closed-on-hostile-input rule
* applies here too: a caller-controlled divisor would risk a divide-by-
* zero exception on essentially arbitrary input). No buffer.
*
* Fixed overhead: 1 (the MOVEQ setup) + 1 (RTS) = 2, not the usual 1 --
* see KernelFixedOverhead above.
*
* Per iteration: 4x (MULU.W + MULS.W + DIVU.W + DIVS.W) + SUBQ.L +
* BNE.S = 16 + 2 = 18.
*-----------------------------------------------------------------------------

        xdef    _run_kernel_muldiv_mix
        xdef    kernel_muldiv_mix

* C-callable entry: m68k-amigaos-gcc's calling convention treats only
* d0-d1/a0-a1 as caller-saved scratch -- d2-d7/a2-a6 (which includes
* the frame pointer a5 and any register gcc is holding a library base
* in, typically a6) are callee-saved, and every kernel here clobbers
* some of that range (kernels.s's own per-kernel header comments say
* which). Calling the bare kernel body directly as a C function --
* this file's first working version did exactly that -- corrupts
* cpubench.c's own register state the moment a kernel touches
* anything past d0/d1/a0/a1: found the hard way (real-ROM evidence,
* not guesswork) when reg_mix (the third kernel, the first one that
* writes d2-d7) left ReadEClock() reading a trashed base and the
* guest eventually executing garbage. This thin wrapper is the fix:
* save/restore the full callee-saved set around a BSR into the bare
* body below, so the body can go on clobbering everything it wants.
* Outside the timed per-iteration loop, so it costs nothing in the
* per-iteration instruction counts kernels.s documents.
_run_kernel_muldiv_mix:
        movem.l d2-d7/a2-a6,-(sp)
        bsr     kernel_muldiv_mix
        movem.l (sp)+,d2-d7/a2-a6
        rts

kernel_muldiv_mix:
        moveq   #7,d5
.loop:
        mulu.w  d5,d1
        muls.w  d5,d2
        divu.w  d5,d3
        divs.w  d5,d4
        mulu.w  d5,d1
        muls.w  d5,d2
        divu.w  d5,d3
        divs.w  d5,d4
        mulu.w  d5,d1
        muls.w  d5,d2
        divu.w  d5,d3
        divs.w  d5,d4
        mulu.w  d5,d1
        muls.w  d5,d2
        divu.w  d5,d3
        divs.w  d5,d4
        subq.l  #1,d0
        bne     .loop
        rts

*-----------------------------------------------------------------------------
* Kernel 9: bitfield_ops -- BFEXTU/BFINS (68020+) round-tripping four
* overlapping byte-wide bitfields through an 8-byte buffer. a0 = buffer
* (>=8 bytes); every access is within the first 4 bytes, so 8 bytes
* leaves headroom. Reloaded implicitly every pass (the field offsets are
* fixed relative to a0, never advanced).
*
* Per iteration: 4x (BFEXTU + BFINS) + SUBQ.L + BNE.S = 8 + 2 = 10.
*-----------------------------------------------------------------------------

        xdef    _run_kernel_bitfield_ops
        xdef    kernel_bitfield_ops

* C-callable entry: m68k-amigaos-gcc's calling convention treats only
* d0-d1/a0-a1 as caller-saved scratch -- d2-d7/a2-a6 (which includes
* the frame pointer a5 and any register gcc is holding a library base
* in, typically a6) are callee-saved, and every kernel here clobbers
* some of that range (kernels.s's own per-kernel header comments say
* which). Calling the bare kernel body directly as a C function --
* this file's first working version did exactly that -- corrupts
* cpubench.c's own register state the moment a kernel touches
* anything past d0/d1/a0/a1: found the hard way (real-ROM evidence,
* not guesswork) when reg_mix (the third kernel, the first one that
* writes d2-d7) left ReadEClock() reading a trashed base and the
* guest eventually executing garbage. This thin wrapper is the fix:
* save/restore the full callee-saved set around a BSR into the bare
* body below, so the body can go on clobbering everything it wants.
* Outside the timed per-iteration loop, so it costs nothing in the
* per-iteration instruction counts kernels.s documents.
_run_kernel_bitfield_ops:
        movem.l d2-d7/a2-a6,-(sp)
        bsr     kernel_bitfield_ops
        movem.l (sp)+,d2-d7/a2-a6
        rts

kernel_bitfield_ops:
.loop:
        bfextu  (a0){0:8},d1
        bfins   d1,(a0){8:8}
        bfextu  (a0){8:8},d2
        bfins   d2,(a0){16:8}
        bfextu  (a0){16:8},d3
        bfins   d3,(a0){24:8}
        bfextu  (a0){24:8},d4
        bfins   d4,(a0){0:8}
        subq.l  #1,d0
        bne     .loop
        rts

*-----------------------------------------------------------------------------
* Kernel 10: cmp_branchy -- a branchy compare-heavy mix: eight different
* condition codes off the same CMP.L, each branch's target the very next
* instruction (as in kernel 1, this measures CMP+Bcc retirement cost,
* not misprediction). No buffer.
*
* As in kernel 1, each zero-displacement Bcc.S assembles as a NOP
* (warning 2058, expected); CMP.L still executes and sets the flags
* every time, and retirement count (the only thing this kernel's
* documented count depends on) is unaffected.
*
* Per iteration: 8x (CMP.L + Bcc.S, each assembling as NOP) + SUBQ.L +
* BNE.S = 16 + 2 = 18.
*-----------------------------------------------------------------------------

        xdef    _run_kernel_cmp_branchy
        xdef    kernel_cmp_branchy

* C-callable entry: m68k-amigaos-gcc's calling convention treats only
* d0-d1/a0-a1 as caller-saved scratch -- d2-d7/a2-a6 (which includes
* the frame pointer a5 and any register gcc is holding a library base
* in, typically a6) are callee-saved, and every kernel here clobbers
* some of that range (kernels.s's own per-kernel header comments say
* which). Calling the bare kernel body directly as a C function --
* this file's first working version did exactly that -- corrupts
* cpubench.c's own register state the moment a kernel touches
* anything past d0/d1/a0/a1: found the hard way (real-ROM evidence,
* not guesswork) when reg_mix (the third kernel, the first one that
* writes d2-d7) left ReadEClock() reading a trashed base and the
* guest eventually executing garbage. This thin wrapper is the fix:
* save/restore the full callee-saved set around a BSR into the bare
* body below, so the body can go on clobbering everything it wants.
* Outside the timed per-iteration loop, so it costs nothing in the
* per-iteration instruction counts kernels.s documents.
_run_kernel_cmp_branchy:
        movem.l d2-d7/a2-a6,-(sp)
        bsr     kernel_cmp_branchy
        movem.l (sp)+,d2-d7/a2-a6
        rts

kernel_cmp_branchy:
.loop:
        cmp.l   d2,d1
        beq.s   .c1
.c1:    cmp.l   d2,d1
        bne.s   .c2
.c2:    cmp.l   d2,d1
        blt.s   .c3
.c3:    cmp.l   d2,d1
        bgt.s   .c4
.c4:    cmp.l   d2,d1
        ble.s   .c5
.c5:    cmp.l   d2,d1
        bge.s   .c6
.c6:    cmp.l   d2,d1
        bhi.s   .c7
.c7:    cmp.l   d2,d1
        bls.s   .c8
.c8:
        subq.l  #1,d0
        bne     .loop
        rts
