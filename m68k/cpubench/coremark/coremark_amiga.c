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
 * MIT License. Copyright (c) 2026 the m68k Machine project. See ../../LICENSE.
 */
#include <string.h>

#include "coremark_amiga.h"
#include "core_portme.h"

extern int coremark_main(int argc, char *argv[]);

void coremark_amiga_run(long *iterations_per_sec_milli, char *score_line, unsigned int score_line_size)
{
    char *argv[1];

    argv[0] = "CPUBench-CoreMark";
    g_coremark_iterations_per_sec = -1.0;
    g_coremark_have_score_line    = 0;
    g_coremark_score_line[0]      = '\0';

    coremark_main(1, argv);

    if (g_coremark_iterations_per_sec >= 0.0) {
        double scaled = g_coremark_iterations_per_sec * 1000.0 + 0.5;
        *iterations_per_sec_milli = (long)scaled;
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
