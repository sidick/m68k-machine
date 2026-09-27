/* coremark_amiga.c -- runs CoreMark's own main() (compiled from
 * upstream's unmodified core_main.c under -Dmain=coremark_main, so it
 * coexists with cpubench.c's own main -- see
 * scripts/build-cpubench.sh) and translates its result into the two
 * values cpubench.c reports: a float-free milli-iterations/sec figure
 * and the "CoreMark 1.0 : ..." score line, both captured by
 * core_portme.c's ee_printf as CoreMark's own report streams past it.
 *
 * Written for this project; not part of the vendored upstream sources
 * (see PROVENANCE.md).
 *
 * The milli-iterations/sec figure is computed HERE, from
 * g_coremark_iterations_done/g_coremark_total_ticks/
 * g_coremark_eclock_freq() (three plain integers, captured from
 * core_main.c's own %lu-printed lines), rather than from
 * g_coremark_iterations_per_sec (a double, captured from core_main.c's
 * own %f-printed "Iterations/Sec" line). Found the hard way: on this
 * toolchain, core_main.c's own expression for that line
 * (`default_num_contexts * results[0].iterations /
 * time_in_secs(total_time)`) reliably evaluates to a clean IEEE754
 * negative zero -- reproduced identically at -O0 and -O2, so not an
 * optimizer artifact, and not a bug in this project's own ee_printf
 * (dumping the raw double's bytes at the call site showed the value
 * itself already broken before formatting, and the *same*
 * `time_in_secs(total_time)` call one line earlier, for "Total time
 * (secs)", returns the correct positive value). This looks like a
 * libgcc soft-float bug specific to this expression shape, not
 * something to chase from a benchmark harness or to fix by editing
 * vendored core_main.c -- computing the same rate from three integers
 * this project's own code already has sidesteps it instead:
 * iterations * freq * 1000 / ticks, in `unsigned long long` to avoid
 * overflowing a 32-bit intermediate.
 *---------------------------------------------------------------------------*/
#include <string.h>

#include "coremark_amiga.h"
#include "core_portme.h"

extern int coremark_main(int argc, char *argv[]);

void coremark_amiga_run(long *iterations_per_sec_milli, char *score_line, unsigned int score_line_size)
{
    char *argv[1];

    argv[0]                        = "CPUBench-CoreMark";
    g_coremark_iterations_per_sec  = -1.0;
    g_coremark_have_score_line     = 0;
    g_coremark_score_line[0]       = '\0';
    g_coremark_total_ticks         = 0;
    g_coremark_iterations_done     = 0;

    coremark_main(1, argv);

    if (g_coremark_iterations_done > 0 && g_coremark_total_ticks > 0 && g_coremark_eclock_freq() > 0) {
        unsigned long long milli = (unsigned long long)g_coremark_iterations_done
                                    * (unsigned long long)g_coremark_eclock_freq() * 1000ULL
                                    / (unsigned long long)g_coremark_total_ticks;
        *iterations_per_sec_milli = (long)milli;
    } else {
        *iterations_per_sec_milli = -1;
    }

    if (g_coremark_have_score_line) {
        strncpy(score_line, g_coremark_score_line, score_line_size - 1);
        score_line[score_line_size - 1] = '\0';
    } else {
        strncpy(score_line,
                "CoreMark score line not printed (CRC did not match the "
                "standard 2000-byte performance-run seeds -- see stdout/SER "
                "above for the full report)",
                score_line_size - 1);
        score_line[score_line_size - 1] = '\0';
    }
}
