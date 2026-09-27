/*
Copyright 2018 Embedded Microprocessor Benchmark Consortium (EEMBC)

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.

Original Author: Shay Gal-on
*/
/*
 * core_portme.c -- the AmigaOS/m68k port of CoreMark's per-platform
 * timing, memory and output functions. Written for this project,
 * adapted from upstream's `barebones/core_portme.c` reference port
 * (same function shapes -- start_time/stop_time/get_time/time_in_secs,
 * portable_init/portable_fini, portable_malloc/portable_free -- AmigaOS
 * implementations). See PROVENANCE.md.
 */
#include <exec/types.h>
#include <exec/memory.h>
#include <exec/devices.h>
#include <devices/timer.h>
#include <proto/exec.h>
#include <proto/timer.h>

#include <stdarg.h>
#include <string.h>
#include <stdlib.h>

#include "coremark.h"
#include "core_portme.h"
#include "coremark_amiga.h"

#if VALIDATION_RUN
volatile ee_s32 seed1_volatile = 0x3415;
volatile ee_s32 seed2_volatile = 0x3415;
volatile ee_s32 seed3_volatile = 0x66;
#endif
#if PERFORMANCE_RUN
volatile ee_s32 seed1_volatile = 0x0;
volatile ee_s32 seed2_volatile = 0x0;
volatile ee_s32 seed3_volatile = 0x66;
#endif
#if PROFILE_RUN
volatile ee_s32 seed1_volatile = 0x8;
volatile ee_s32 seed2_volatile = 0x8;
volatile ee_s32 seed3_volatile = 0x8;
#endif
volatile ee_s32 seed4_volatile = ITERATIONS;
volatile ee_s32 seed5_volatile = 0;

ee_u32 default_num_contexts = 1;

/*-----------------------------------------------------------------------------
 * Timing: timer.device's ReadEClock(), via the TimerBase cpubench.c's
 * timer_open() already set up (this file never opens the device
 * itself -- one open, shared with the eleven-kernel harness, so both
 * halves of a CPUBench run come from the same clock).
 *---------------------------------------------------------------------------*/

extern struct Device *TimerBase;

static struct EClockVal start_val, stop_val;
static ULONG            eclock_freq;

void start_time(void)
{
    eclock_freq = ReadEClock(&start_val);
}

void stop_time(void)
{
    ReadEClock(&stop_val);
}

/* Ticks, not microseconds -- time_in_secs (below) does the frequency
 * division, matching CORE_TICKS's documented contract ("may be cpu
 * cycles, milliseconds, or any other value, as long as it can be
 * converted to seconds by time_in_secs"). ev_hi is ignored: at any
 * EClock frequency this project's timer.device has ever reported, a
 * CoreMark run (order of ten real seconds) does not carry a 32-bit
 * low-word tick counter past one wrap, and if it somehow did, the
 * wrapped low-word difference alone would read as an implausibly small
 * elapsed time -- caught by core_main.c's own "<10 secs" validity
 * check rather than silently misreported. */
CORE_TICKS get_time(void)
{
    return (CORE_TICKS)(stop_val.ev_lo - start_val.ev_lo);
}

secs_ret time_in_secs(CORE_TICKS ticks)
{
    if (eclock_freq == 0) {
        return (secs_ret)0;
    }
    return (secs_ret)ticks / (secs_ret)eclock_freq;
}

/*-----------------------------------------------------------------------------
 * Memory: AllocMem/FreeMem (MEMF_ANY -- fast RAM on this platform),
 * never malloc/free -- this is a -noixemul build, so there is no libc
 * heap to call into (CLAUDE.md's toolchain notes; every other AmigaOS
 * C program in m68k/ makes the same choice, see e.g.
 * prometheus_library.c's own AllocMem usage). CoreMark's MEM_MALLOC
 * method calls exactly these two functions and nothing else.
 *---------------------------------------------------------------------------*/

/* AmigaOS's AllocMem/FreeMem pair requires the same size at free time,
 * and CoreMark's MEM_MALLOC path (core_main.c, MULTITHREAD == 1) makes
 * exactly one portable_malloc call per run and frees exactly that
 * pointer -- a one-entry table recording the most recent allocation's
 * size is enough, and is preferred over linking in a general
 * allocator-with-headers for a single call site. */
static void *sLastMallocPtr  = NULL;
static ULONG sLastMallocSize = 0;

void *portable_malloc(ee_size_t size)
{
    sLastMallocPtr  = AllocMem((ULONG)size, MEMF_ANY);
    sLastMallocSize = (ULONG)size;
    return sLastMallocPtr;
}

void portable_free(void *p)
{
    if (p != NULL && p == sLastMallocPtr) {
        FreeMem(p, sLastMallocSize);
        sLastMallocPtr  = NULL;
        sLastMallocSize = 0;
    }
}

void portable_init(core_portable *p, int *argc, char *argv[])
{
    (void)argc;
    (void)argv;
    p->portable_id = 1;
}

void portable_fini(core_portable *p)
{
    p->portable_id = 0;
}

/*-----------------------------------------------------------------------------
 * ee_printf: a small hand-written formatter, not libnix's printf.
 * core_main.c's HAS_FLOAT=1 path is required to get CoreMark's own
 * "CoreMark 1.0 : ..." score line (that whole block is `#if HAS_FLOAT`
 * in core_main.c), which means an ee_printf that can render a `double`
 * -- and this project does not trust -noixemul's minimal libnix stdio
 * with %f (see cpubench.c's own format_mips() comment making the same
 * choice for the eleven-kernel harness). Supports exactly the
 * specifiers core_main.c/core_util.c/core_list_join.c/core_matrix.c/
 * core_state.c actually use (checked by grep across every vendored
 * .c file: %04x, %d, %f, %lu, %s, %u) -- not a general printf.
 *
 * Every call renders one complete line (every call site in the
 * vendored sources ends its format string in "\n", or is immediately
 * followed by one that does) and hands it to cpubench_emit_line() so
 * CoreMark's report reaches stdout and SER: exactly like the eleven
 * kernels' own CPUBENCH lines. It also watches for the two lines
 * coremark_amiga.c needs afterward ("Iterations/Sec" and
 * "CoreMark 1.0") and captures them into the globals core_portme.h
 * declares, rather than cpubench.c re-parsing printed text.
 *---------------------------------------------------------------------------*/

double g_coremark_iterations_per_sec = -1.0;
int    g_coremark_have_score_line    = 0;
char   g_coremark_score_line[160];

static char sLineBuf[200];

static void append_char(unsigned int *pos, char c)
{
    if (*pos < sizeof(sLineBuf) - 1) {
        sLineBuf[*pos] = c;
        *pos += 1;
    }
}

static void append_str(unsigned int *pos, const char *s)
{
    while (*s != '\0') {
        append_char(pos, *s);
        s++;
    }
}

static void append_hex(unsigned int *pos, unsigned long value, int width, int zero_pad)
{
    char           digits[9];
    int            n = 0;
    static const char hexchars[] = "0123456789abcdef";

    if (value == 0) {
        digits[n++] = '0';
    }
    while (value != 0 && n < (int)sizeof(digits)) {
        digits[n++] = hexchars[value & 0xF];
        value >>= 4;
    }
    while (n < width) {
        digits[n++] = zero_pad ? '0' : ' ';
    }
    while (n > 0) {
        append_char(pos, digits[--n]);
    }
}

static void append_udec(unsigned int *pos, unsigned long value)
{
    char digits[12];
    int  n = 0;

    if (value == 0) {
        digits[n++] = '0';
    }
    while (value != 0 && n < (int)sizeof(digits)) {
        digits[n++] = (char)('0' + (value % 10));
        value /= 10;
    }
    while (n > 0) {
        append_char(pos, digits[--n]);
    }
}

static void append_dec(unsigned int *pos, long value)
{
    if (value < 0) {
        append_char(pos, '-');
        /* value == LONG_MIN is not a case any caller here hits
         * (CoreMark's own values are all small counts/sizes), so the
         * plain negation below is safe. */
        append_udec(pos, (unsigned long)(-value));
    } else {
        append_udec(pos, (unsigned long)value);
    }
}

/* Fixed six-decimal rendering, the same precision plain printf's
 * default %f gives. `value` is never negative for anything this port's
 * ee_printf callers pass (iteration rates and second counts). */
static void append_float(unsigned int *pos, double value)
{
    unsigned long whole;
    unsigned long frac;
    double        scaled;

    if (value < 0.0) {
        append_char(pos, '-');
        value = -value;
    }
    whole  = (unsigned long)value;
    scaled = (value - (double)whole) * 1000000.0 + 0.5;
    frac   = (unsigned long)scaled;
    if (frac >= 1000000UL) {
        whole += 1;
        frac -= 1000000UL;
    }
    append_udec(pos, whole);
    append_char(pos, '.');
    {
        /* Zero-padded to six digits. */
        char tmp[8];
        int  n = 0;
        unsigned long f = frac;
        if (f == 0) {
            tmp[n++] = '0';
        }
        while (f != 0 && n < 6) {
            tmp[n++] = (char)('0' + (f % 10));
            f /= 10;
        }
        while (n < 6) {
            tmp[n++] = '0';
        }
        while (n > 0) {
            append_char(pos, tmp[--n]);
        }
    }
}

static void capture_known_lines(const char *line)
{
    if (strncmp(line, "Iterations/Sec", 14) == 0) {
        const char *colon = strchr(line, ':');
        if (colon != NULL) {
            g_coremark_iterations_per_sec = atof(colon + 1);
        }
    } else if (strncmp(line, "CoreMark 1.0", 12) == 0) {
        strncpy(g_coremark_score_line, line, sizeof(g_coremark_score_line) - 1);
        g_coremark_score_line[sizeof(g_coremark_score_line) - 1] = '\0';
        g_coremark_have_score_line = 1;
    }
}

int ee_printf(const char *fmt, ...)
{
    va_list      args;
    unsigned int pos = 0;
    const char  *p   = fmt;

    va_start(args, fmt);
    while (*p != '\0') {
        if (*p == '\n') {
            /* One rendered line ready -- flush it (without CoreMark's
             * own trailing newline; cpubench_emit_line adds its own,
             * matching every other line this program prints). */
            sLineBuf[pos] = '\0';
            capture_known_lines(sLineBuf);
            cpubench_emit_line(sLineBuf);
            pos = 0;
            p++;
            continue;
        }
        if (*p != '%') {
            append_char(&pos, *p);
            p++;
            continue;
        }
        p++; /* skip '%' */
        {
            int zero_pad = 0;
            int width    = 0;
            int is_long  = 0;

            while (*p == '0') {
                zero_pad = 1;
                p++;
            }
            while (*p >= '0' && *p <= '9') {
                width = width * 10 + (*p - '0');
                p++;
            }
            while (*p == 'l') {
                is_long = 1;
                p++;
            }
            switch (*p) {
                case 'd': {
                    long v = is_long ? va_arg(args, long) : (long)va_arg(args, int);
                    append_dec(&pos, v);
                    break;
                }
                case 'u': {
                    unsigned long v = is_long ? va_arg(args, unsigned long)
                                               : (unsigned long)va_arg(args, unsigned int);
                    append_udec(&pos, v);
                    break;
                }
                case 'x': {
                    unsigned long v = is_long ? va_arg(args, unsigned long)
                                               : (unsigned long)va_arg(args, unsigned int);
                    append_hex(&pos, v, width, zero_pad);
                    break;
                }
                case 'f': {
                    double v = va_arg(args, double);
                    append_float(&pos, v);
                    break;
                }
                case 's': {
                    const char *s = va_arg(args, const char *);
                    append_str(&pos, s != NULL ? s : "(null)");
                    break;
                }
                case '%':
                    append_char(&pos, '%');
                    break;
                default:
                    /* Unknown specifier: emit literally so a future
                     * vendored-source change that adds a new one is
                     * visible in the output instead of silently
                     * eaten -- silent failure is this platform's
                     * norm and this formatter does not add to it. */
                    append_char(&pos, '%');
                    if (*p != '\0') {
                        append_char(&pos, *p);
                    }
                    break;
            }
            if (*p != '\0') {
                p++;
            }
        }
    }
    va_end(args);

    if (pos > 0) {
        /* A call whose format string does not end in "\n" -- none of
         * the vendored call sites do this today, but flush whatever
         * was rendered rather than dropping it, in case a future
         * upstream update adds one. */
        sLineBuf[pos] = '\0';
        capture_known_lines(sLineBuf);
        cpubench_emit_line(sLineBuf);
    }
    return (int)pos;
}
