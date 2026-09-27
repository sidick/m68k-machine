/*-----------------------------------------------------------------------------
 * cpubench.c -- CPUBench, the AmigaOS CLI half of the CPU benchmark
 * (docs/bus-fast-path-plan.md step 8). Runs the eleven kernels in
 * kernels.s (linked in directly, xdef'd by name -- see that file's own
 * header for the calling convention every `run_kernel_*` extern below
 * matches) inside a real Kickstart boot, then runs CoreMark
 * (coremark/, ported freestanding for this platform -- see
 * coremark/PROVENANCE.md) as the realistic mixed workload. Both halves
 * feed docs/bus-fast-path-plan.md step 8's in-machine table, compared
 * against crates/cpu-bench's bare-core numbers on the identical
 * kernels.s bytes.
 *
 * MIT License. Copyright (c) 2026 the m68k Machine project. See ../LICENSE.
 *
 * Built by scripts/build-cpubench.sh (amiga-gcc + vasm, NDK 3.2 headers
 * -- see that script). NOT part of the Rust build.
 *
 * Timing: timer.device's ReadEClock(), per this file's own
 * eclock_delta_usec() below -- the EClock frequency it returns
 * (io_frequency, actually per-call from ReadEClock's return value) is
 * used to convert tick deltas to microseconds, so this works whatever
 * frequency this platform's timer.device reports rather than assuming
 * a real Amiga's ~715909/~709379 Hz.
 *
 * Each kernel is bracketed with Forbid()/Permit() (never Disable(): that
 * would also block interrupts, which this measurement does not need to
 * suppress and which would make the timer tick itself unreliable to
 * read mid-run). CoreMark is NOT Forbid()'d -- see run_coremark()'s own
 * comment for why.
 *
 * Every kernel's iteration count is calibrated (doubled) until the
 * measured wall time reaches roughly one second, per the task's own
 * instruction, mirroring crates/cpu-bench's bare-harness calibration
 * loop exactly (same doubling shape, same accumulate-across-calls
 * design) so the two sides of step 8's ratio table were produced the
 * same way.
 *
 * KNOWN OPEN ISSUE (real-ROM evidence, not yet root-caused): on a real
 * Kickstart 3.2.2 boot, two back-to-back ReadEClock() calls bracketing
 * a single ~3600-instruction kernel call (reg_addq_bra/reg_tst_bne, the
 * two fastest kernels) measure a delta of roughly one real second's
 * worth of EClock ticks (freq correctly 709379 Hz; t1.ev_lo - t0.ev_lo
 * measured ~713,000 in one captured run) -- reproduced identically in
 * both `--cpu-speed cycle` and `--cpu-speed max`, and with Forbid()/
 * Permit() removed entirely, ruling out both the timing mode and this
 * file's own multitasking bracketing as the cause. A few calibration
 * steps later (once the outer iteration count has grown enough that a
 * kernel call takes a real amount of time), the accumulated skew
 * corrupts something further: the run eventually WEDGEs (an F-line
 * trap storm at a PC inside fast RAM, or a tight-loop detector firing
 * at PC 0). This smells like an issue in the emulated CIA's EClock/TOD
 * timer model rather than anything in this file (ReadEClock is used
 * exactly per the RKM: OpenDevice once, read the returned frequency
 * once per call, take a tick delta) -- but was not chased further into
 * crates/machine-core/src/cia.rs, per this project's own standing
 * instruction to stop and report a genuine divergence rather than
 * guess at a machine-core fix from outside it. See the CPU benchmark
 * task's final report for the full evidence trail (debug ReadEClock
 * dumps, cycle-vs-max and Forbid-vs-not comparisons).
 *---------------------------------------------------------------------------*/

#include <exec/types.h>
#include <exec/memory.h>
#include <exec/devices.h>
#include <exec/errors.h>
#include <devices/timer.h>
#include <dos/dos.h>

#include <proto/exec.h>
#include <proto/dos.h>
#include <proto/timer.h>
#include <proto/alib.h>

#include <stdio.h>
#include <string.h>

#include "coremark/coremark_amiga.h"

/*-----------------------------------------------------------------------------
 * Kernel externs. Every kernel in kernels.s follows the same
 * (ULONG d0, void *a0, void *a1) shape regardless of how many buffers it
 * actually reads (kernels.s's own header comment): a uniform C
 * prototype for all eleven means they share one function-pointer type
 * below instead of needing per-arity wrapper shims.
 *---------------------------------------------------------------------------*/

typedef void (*KernelFn)(ULONG iters, void *a0, void *a1);

extern void run_kernel_reg_addq_bra(ULONG iters __asm("d0"));
extern void run_kernel_reg_tst_bne(ULONG iters __asm("d0"));
extern void run_kernel_reg_mix(ULONG iters __asm("d0"));
extern void run_kernel_mem_copy(ULONG iters __asm("d0"), void *a0 __asm("a0"), void *a1 __asm("a1"));
extern void run_kernel_mem_fill(ULONG iters __asm("d0"), void *a0 __asm("a0"));
extern void run_kernel_struct_walk(ULONG iters __asm("d0"), void *a0 __asm("a0"));
extern void run_kernel_movem_saverestore(ULONG iters __asm("d0"), void *a0 __asm("a0"));
extern void run_kernel_jsr_rts_chain(ULONG iters __asm("d0"));
extern void run_kernel_muldiv_mix(ULONG iters __asm("d0"));
extern void run_kernel_bitfield_ops(ULONG iters __asm("d0"), void *a0 __asm("a0"));
extern void run_kernel_cmp_branchy(ULONG iters __asm("d0"));

/* kernels.s's own metadata tables -- the single source of truth for
 * per-iteration instruction counts, read directly out of the linked
 * object rather than duplicated by hand here (the same reasoning
 * crates/cpu-bench's main.rs comment gives for reading them out of the
 * flat binary instead of hardcoding a parallel Rust table). */
extern ULONG KernelCount;
extern ULONG KernelInstrsPerIter[];
extern ULONG KernelFixedOverhead[];

struct KernelDesc {
    const char *name;
    KernelFn fn;
    int buffers; /* 0, 1 (a0 only) or 2 (a0 and a1) */
};

/* Order MUST match kernels.s's _KernelTable exactly -- that file's own
 * header comment is the authoritative list. */
static const struct KernelDesc kKernels[] = {
    {"reg_addq_bra",      (KernelFn)run_kernel_reg_addq_bra,      0},
    {"reg_tst_bne",       (KernelFn)run_kernel_reg_tst_bne,       0},
    {"reg_mix",           (KernelFn)run_kernel_reg_mix,           0},
    {"mem_copy",          (KernelFn)run_kernel_mem_copy,          2},
    {"mem_fill",          (KernelFn)run_kernel_mem_fill,          1},
    {"struct_walk",       (KernelFn)run_kernel_struct_walk,       1},
    {"movem_saverestore", (KernelFn)run_kernel_movem_saverestore, 1},
    {"jsr_rts_chain",     (KernelFn)run_kernel_jsr_rts_chain,     0},
    {"muldiv_mix",        (KernelFn)run_kernel_muldiv_mix,        0},
    {"bitfield_ops",      (KernelFn)run_kernel_bitfield_ops,      1},
    {"cmp_branchy",       (KernelFn)run_kernel_cmp_branchy,       0},
};
#define NUM_KERNELS ((int)(sizeof(kKernels) / sizeof(kKernels[0])))

/* Buffer sizes: generous against every kernel's own documented minimum
 * (the largest is 128 bytes). Allocated once with MEMF_ANY (fast RAM on
 * this platform -- CLAUDE.md's architecture notes list fast RAM as a
 * permanent guest-visible device; MEMF_ANY resolves there when no chip-
 * specific requirement is given) and reused by every kernel. */
#define BUFFER_SIZE 256

struct Device *TimerBase;
static struct MsgPort *gTimerPort;
static struct timerequest *gTimerRequest;

static BPTR gSerHandle = 0;

/*-----------------------------------------------------------------------------
 * Output: one line to both stdout (the CLI's own Output()) and SER:,
 * per the task's own requirement -- serial markers are this platform's
 * standard of positive evidence (CLAUDE.md), and the real-ROM test that
 * drives this program reads them back over the guest's serial port.
 *---------------------------------------------------------------------------*/

void cpubench_emit_line(const char *line)
{
    LONG len = (LONG)strlen(line);
    Write(Output(), (APTR)line, len);
    Write(Output(), (APTR)"\n", 1);
    if (gSerHandle != 0) {
        Write(gSerHandle, (APTR)line, len);
        Write(gSerHandle, (APTR)"\n", 1);
    }
}

static void report_line(const char *line)
{
    cpubench_emit_line(line);
}

/*-----------------------------------------------------------------------------
 * Timer setup/teardown -- the classic timer.device recipe (RKRM
 * Devices, "timer.device"): a private message port and IORequest,
 * OpenDevice(TIMERNAME, UNIT_VBLANK, ...), TimerBase taken from the
 * opened request's own io_Device (ReadEClock's inline stub, <inline/
 * timer.h>, calls through that exact global, never through a library
 * base obtained any other way).
 *---------------------------------------------------------------------------*/

static BOOL timer_open(void)
{
    gTimerPort = CreateMsgPort();
    if (gTimerPort == NULL) {
        return FALSE;
    }
    gTimerRequest = (struct timerequest *)CreateExtIO(gTimerPort, sizeof(struct timerequest));
    if (gTimerRequest == NULL) {
        DeleteMsgPort(gTimerPort);
        gTimerPort = NULL;
        return FALSE;
    }
    if (OpenDevice((STRPTR)TIMERNAME, UNIT_VBLANK, (struct IORequest *)gTimerRequest, 0) != 0) {
        DeleteExtIO((struct IORequest *)gTimerRequest);
        DeleteMsgPort(gTimerPort);
        gTimerRequest = NULL;
        gTimerPort = NULL;
        return FALSE;
    }
    TimerBase = gTimerRequest->tr_node.io_Device;
    return TRUE;
}

static void timer_close(void)
{
    if (gTimerRequest != NULL) {
        CloseDevice((struct IORequest *)gTimerRequest);
        DeleteExtIO((struct IORequest *)gTimerRequest);
        gTimerRequest = NULL;
    }
    if (gTimerPort != NULL) {
        DeleteMsgPort(gTimerPort);
        gTimerPort = NULL;
    }
    TimerBase = NULL;
}

/* Microseconds between two EClockVals, given the tick frequency
 * ReadEClock() returned. `ev_hi` changing mid-measurement would mean a
 * kernel run took long enough to wrap a 32-bit tick counter at this
 * platform's EClock frequency -- for every frequency timer.device on
 * this project has ever reported, that is many minutes, far past any
 * single calibration step below, so this case is treated as "far more
 * than one second" (safe for every caller here, which only ever compares
 * against a one-second target) rather than computed exactly. */
static unsigned long eclock_delta_usec(const struct EClockVal *t0, const struct EClockVal *t1, ULONG freq)
{
    ULONG hi = t1->ev_hi - t0->ev_hi;
    ULONG lo = t1->ev_lo - t0->ev_lo;
    unsigned long long usec;

    if (t1->ev_lo < t0->ev_lo) {
        hi -= 1;
    }
    if (hi != 0 || freq == 0) {
        return 0xFFFFFFFFUL;
    }
    usec = ((unsigned long long)lo * 1000000ULL) / (unsigned long long)freq;
    if (usec > 0xFFFFFFFFULL) {
        return 0xFFFFFFFFUL;
    }
    return (unsigned long)usec;
}

/* Format an integer M-instructions/second rate to three decimal places
 * without floating-point printf (libnix's minimal stdio under
 * -noixemul is not trusted with %f -- see this file's own build notes
 * in scripts/build-cpubench.sh). `instructions` and `usec` are both
 * already known not to overflow a 64-bit product here: usec is capped
 * well under 2^32 by the calibration loop's one-second target. */
static void format_mips(char *out, ULONG instructions, unsigned long usec)
{
    unsigned long long milli_mips;
    if (usec == 0) {
        strcpy(out, "0.000");
        return;
    }
    milli_mips = ((unsigned long long)instructions * 1000ULL) / (unsigned long long)usec;
    sprintf(out, "%lu.%03lu", (unsigned long)(milli_mips / 1000ULL), (unsigned long)(milli_mips % 1000ULL));
}

/*-----------------------------------------------------------------------------
 * Run one kernel, calibrating its iteration count by doubling until the
 * accumulated wall time reaches ~1 second (the task's own instruction),
 * then report the CPUBENCH line. Forbid()/Permit() brackets the whole
 * calibrate-and-measure loop for this kernel, not just the final call:
 * a task switch landing inside the calibration doubling would size the
 * "real" run off a contaminated rate estimate.
 *---------------------------------------------------------------------------*/

#define TARGET_USEC 1000000UL

static void run_and_report_kernel(const struct KernelDesc *desc, ULONG instrs_per_iter, ULONG fixed_overhead,
                                   void *bufA, void *bufB)
{
    ULONG iters = 200;
    ULONG total_instructions = 0;
    unsigned long total_usec = 0;
    char line[160];
    char mips_str[24];
    void *a0 = (desc->buffers >= 1) ? bufA : NULL;
    void *a1 = (desc->buffers >= 2) ? bufB : NULL;

    Forbid();
    while (total_usec < TARGET_USEC) {
        struct EClockVal t0, t1;
        ULONG freq;

        freq = ReadEClock(&t0);
        desc->fn(iters, a0, a1);
        ReadEClock(&t1);

        total_instructions += fixed_overhead + iters * instrs_per_iter;
        total_usec += eclock_delta_usec(&t0, &t1, freq);

        if (iters < (0x7FFFFFFFUL / 2UL)) {
            iters *= 2;
        }
    }
    Permit();

    format_mips(mips_str, total_instructions, total_usec);
    sprintf(line, "CPUBENCH %s %lu %lu %s", desc->name, (unsigned long)total_instructions, total_usec, mips_str);
    report_line(line);
}

/* CoreMark is not Forbid()'d: unlike the eleven short kernels above, a
 * CoreMark run is many seconds long (its own core_portme.c times and
 * scales it to run at least ITERATIONS iterations of real work), and
 * holding Forbid() -- which blocks every other task on the whole
 * system, not just interrupts -- for that long would make this program
 * itself the reason the platform looks unresponsive during the run.
 * core_portme.c's own ReadEClock-based timing is unaffected either way;
 * this only changes whether other tasks can run concurrently. */
static void run_coremark(void)
{
    long iterations_per_sec_milli;
    char line[160];
    char score_line[160];
    char rate_str[24];

    coremark_amiga_run(&iterations_per_sec_milli, score_line, sizeof(score_line));

    rate_str[0] = '-';
    if (iterations_per_sec_milli >= 0) {
        sprintf(rate_str, "%ld.%03ld", iterations_per_sec_milli / 1000L, iterations_per_sec_milli % 1000L);
    } else {
        strcpy(rate_str, "0.000");
    }

    sprintf(line, "CPUBENCH coremark %s", rate_str);
    report_line(line);
    report_line(score_line);
}

int main(void)
{
    void *bufA;
    void *bufB;
    int i;

    gSerHandle = Open((STRPTR) "SER:", MODE_NEWFILE);
    /* SER: failing to open is not fatal -- stdout still carries every
     * line, and the real-ROM test reads stdout through the same
     * scripted-Shell capture the existing Wait-5 test uses. report_line
     * skips SER: writes when gSerHandle is 0. */

    if (!timer_open()) {
        report_line("CPUBENCH ERROR timer.device open failed");
        if (gSerHandle != 0) {
            Close(gSerHandle);
        }
        return RETURN_FAIL;
    }

    bufA = AllocMem(BUFFER_SIZE, MEMF_ANY | MEMF_CLEAR);
    bufB = AllocMem(BUFFER_SIZE, MEMF_ANY | MEMF_CLEAR);
    if (bufA == NULL || bufB == NULL) {
        report_line("CPUBENCH ERROR AllocMem failed");
        if (bufA != NULL) {
            FreeMem(bufA, BUFFER_SIZE);
        }
        if (bufB != NULL) {
            FreeMem(bufB, BUFFER_SIZE);
        }
        timer_close();
        if (gSerHandle != 0) {
            Close(gSerHandle);
        }
        return RETURN_FAIL;
    }

    if ((ULONG)NUM_KERNELS != KernelCount) {
        report_line("CPUBENCH ERROR kernel count mismatch between cpubench.c and kernels.s");
        FreeMem(bufA, BUFFER_SIZE);
        FreeMem(bufB, BUFFER_SIZE);
        timer_close();
        if (gSerHandle != 0) {
            Close(gSerHandle);
        }
        return RETURN_FAIL;
    }

    for (i = 0; i < NUM_KERNELS; i++) {
        run_and_report_kernel(&kKernels[i], KernelInstrsPerIter[i], KernelFixedOverhead[i], bufA, bufB);
    }

    run_coremark();

    report_line("CPUBENCH DONE");

    FreeMem(bufA, BUFFER_SIZE);
    FreeMem(bufB, BUFFER_SIZE);
    timer_close();
    if (gSerHandle != 0) {
        Close(gSerHandle);
    }
    return RETURN_OK;
}
