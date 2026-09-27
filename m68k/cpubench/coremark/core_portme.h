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
 * core_portme.h -- the AmigaOS/m68k port of CoreMark's per-platform
 * configuration header, written for this project (see PROVENANCE.md:
 * every *other* file in this directory is vendored byte-for-byte from
 * upstream; this one and core_portme.c are the porting layer CoreMark's
 * own model expects every platform to supply, adapted from the
 * upstream `barebones/` reference port -- same shape, AmigaOS-specific
 * timing/memory/output).
 */
#ifndef CORE_PORTME_H
#define CORE_PORTME_H

/* Configuration : HAS_FLOAT
        The pinned m68k-amigaos-gcc supports `double` (soft-float
   libgcc routines on a plain 68020/68040 build, hardware FPU when the
   68040's own is used) -- CoreMark's official score line
   ("CoreMark 1.0 : ...") only exists under HAS_FLOAT, so this is not
   optional here. */
#ifndef HAS_FLOAT
#define HAS_FLOAT 1
#endif
#ifndef HAS_TIME_H
#define HAS_TIME_H 0
#endif
#ifndef USE_CLOCK
#define USE_CLOCK 0
#endif
#ifndef HAS_STDIO
#define HAS_STDIO 0
#endif
#ifndef HAS_PRINTF
#define HAS_PRINTF 0
#endif

#ifndef COMPILER_VERSION
#ifdef __GNUC__
#define COMPILER_VERSION "GCC" __VERSION__
#else
#define COMPILER_VERSION "m68k-amigaos-gcc"
#endif
#endif
#ifndef COMPILER_FLAGS
#define COMPILER_FLAGS "-O2 -mcpu=68040 -noixemul"
#endif
#ifndef MEM_LOCATION
#define MEM_LOCATION "AllocMem(MEMF_ANY)"
#endif

typedef signed short   ee_s16;
typedef unsigned short ee_u16;
typedef signed int     ee_s32;
typedef double         ee_f32;
typedef unsigned char  ee_u8;
typedef unsigned int   ee_u32;
typedef ee_u32         ee_ptr_int;
typedef unsigned long  ee_size_t;
#ifndef NULL
#define NULL ((void *)0)
#endif

#define align_mem(x) (void *)(4 + (((ee_ptr_int)(x)-1) & ~3))

#define CORETIMETYPE ee_u32
typedef ee_u32 CORE_TICKS;

/* One free-running EClock-tick counter, read via this file's own
   start_time/stop_time/get_time (core_portme.c) over timer.device's
   ReadEClock -- exactly the mechanism this project's task brief calls
   for ("its core_portme uses ReadEClock for timing"), and the same
   mechanism cpubench.c's own eleven-kernel harness uses, so CoreMark's
   number and the kernel numbers come from one clock. */
#ifndef SEED_METHOD
#define SEED_METHOD SEED_VOLATILE
#endif

/* Iterations: 0 means "auto-detect": core_main.c's own main() runs
   iterate() in a calibration loop until at least one second of
   ReadEClock time has passed, then scales up for a ~10 second real
   run -- the same "let the benchmark tell you how long is enough"
   shape as cpubench.c's own per-kernel calibration loop, just written
   by CoreMark's upstream authors instead of by this project. */
#ifndef ITERATIONS
#define ITERATIONS 0
#endif

#ifndef MEM_METHOD
#define MEM_METHOD MEM_MALLOC
#endif

#ifndef MULTITHREAD
#define MULTITHREAD 1
#define USE_PTHREAD 0
#define USE_FORK    0
#define USE_SOCKET  0
#endif

#ifndef MAIN_HAS_NOARGC
#define MAIN_HAS_NOARGC 0
#endif
#ifndef MAIN_HAS_NORETURN
#define MAIN_HAS_NORETURN 0
#endif

extern ee_u32 default_num_contexts;

typedef struct CORE_PORTABLE_S
{
    ee_u8 portable_id;
} core_portable;

void portable_init(core_portable *p, int *argc, char *argv[]);
void portable_fini(core_portable *p);

/* AmigaOS/no-ixemul has no malloc.h `malloc`/`free` under this port's
   MEM_MALLOC choice -- portable_malloc/portable_free (core_portme.c)
   go through AllocMem/FreeMem instead, so declare them here rather
   than pulling in <stdlib.h>. */
void *portable_malloc(ee_size_t size);
void  portable_free(void *p);

#if !defined(PROFILE_RUN) && !defined(PERFORMANCE_RUN) \
    && !defined(VALIDATION_RUN)
#if (TOTAL_DATA_SIZE == 1200)
#define PROFILE_RUN 1
#elif (TOTAL_DATA_SIZE == 2000)
#define PERFORMANCE_RUN 1
#else
#define VALIDATION_RUN 1
#endif
#endif

int ee_printf(const char *fmt, ...);

/* Captured by this port's ee_printf (core_portme.c) from the two lines
   core_main.c's main() always prints -- "Iterations/Sec   : %f" and,
   only on a validated 2000-byte performance run, "CoreMark 1.0 : %f
   ...". cpubench.c reads these after calling coremark_main() instead
   of re-parsing printed text, and reports "no score line printed" if
   the validated line never appeared (e.g. a CRC mismatch on this
   toolchain, which would itself be news). */
extern double g_coremark_iterations_per_sec;
extern int    g_coremark_have_score_line;
extern char   g_coremark_score_line[160];

/* core_main.c's `main` renamed via -Dmain=coremark_main on this
   directory's compile line (scripts/build-cpubench.sh) so it coexists
   with cpubench.c's own `main` -- see that script's own comment. */

#endif /* CORE_PORTME_H */
