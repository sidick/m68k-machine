/* coremark_amiga.h -- the small glue interface between cpubench.c and
 * this project's AmigaOS CoreMark port (core_portme.c/coremark_amiga.c).
 * Written for this project; not part of the vendored upstream sources
 * (see PROVENANCE.md).
 *
 * MIT License. Copyright (c) 2026 the m68k Machine project. See ../../LICENSE.
 */
#ifndef COREMARK_AMIGA_H
#define COREMARK_AMIGA_H

/* Runs CoreMark to completion (core_main.c's own auto-iteration-count
 * calibration decides how long that takes -- typically ~10 real
 * seconds under this platform's --cpu-speed max, per that file's own
 * "must execute for at least 10 secs" check) and reports:
 *
 *   - *iterations_per_sec_milli: the measured iterations/sec, scaled by
 *     1000 and rounded to the nearest integer (matches cpubench.c's own
 *     float-free reporting convention), or -1 if CoreMark never printed
 *     an "Iterations/Sec" line at all (which would itself indicate a
 *     deeper problem, not just a missing score).
 *   - score_line: CoreMark's own "CoreMark 1.0 : ..." line verbatim, if
 *     the run validated against the standard 2000-byte performance-run
 *     CRCs (core_main.c's own known_id == 3 case) and printed one; an
 *     explanatory line otherwise (see coremark_amiga.c).
 */
void coremark_amiga_run(long *iterations_per_sec_milli, char *score_line, unsigned int score_line_size);

/* Defined in cpubench.c: writes one line to stdout and (if open) SER:,
 * exactly like that file's own report_line. core_portme.c's ee_printf
 * calls this so CoreMark's own printed report reaches the same two
 * places the CPUBENCH lines do, without core_portme.c needing to know
 * about cpubench.c's Output()/SER: handle. */
void cpubench_emit_line(const char *text);

#endif /* COREMARK_AMIGA_H */
