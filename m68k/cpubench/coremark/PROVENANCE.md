# CoreMark provenance

`core_main.c`, `core_list_join.c`, `core_matrix.c`, `core_state.c`,
`core_util.c`, `coremark.h` and `LICENSE.md` in this directory are
vendored **byte-for-byte, unmodified**, from EEMBC's upstream CoreMark
repository:

- Source: https://github.com/eembc/coremark
- Commit: `1f483d5b8316753a742cbf5590caf5bd0a4e4777` (`main`, 2025-05-01)
- Fetched: 2026-09-27, via `curl` against `raw.githubusercontent.com`.

Every vendored `.c`/`.h` file carries its own Apache License 2.0
header (Copyright 2018 Embedded Microprocessor Benchmark Consortium
(EEMBC), "Original Author: Shay Gal-on") — that header, not this file,
is the operative license grant for the code. The full license text is
vendored alongside as `LICENSE-APACHE-2.0.txt` (fetched from
`https://www.apache.org/licenses/LICENSE-2.0.txt`, same date).

`LICENSE.md` at this repository's root is a **separate document**: the
COREMARK® Acceptable Use Agreement, which governs use of the
*trademark* ("CoreMark") and the conditions under which a run may be
reported as an official CoreMark score — it is not a code license.
This project vendors it for completeness (the file is part of upstream
CoreMark's own repository layout) but makes no claim of an
EEMBC-validated official run: `cpubench.c`'s CoreMark integration is
an internal engineering measurement for
`docs/bus-fast-path-plan.md` step 8, run and reported the way this
project reports every other benchmark number in that document, not
submitted to or scored by EEMBC. The run does validate against the
standard 2000-byte performance-run CRCs that upstream's own
`core_main.c` checks (`known_id == 3`, "2K performance run parameters
for coremark") before printing the "CoreMark 1.0 : ..." line at all —
see that file's own `main()` for the exact check — so the printed
score, when it appears, reflects a run that reproduces the standard
seeds and data size, whatever else may distinguish it from a
Consortium-submitted result.

## What is NOT vendored

The `core_portme.h`/`core_portme.c` files in this directory are this
project's own AmigaOS/m68k port, **not** vendored from upstream. They
follow the shape of upstream's `barebones/core_portme.{h,c}` reference
port (same function set: `start_time`/`stop_time`/`get_time`/
`time_in_secs`, `portable_init`/`portable_fini`,
`portable_malloc`/`portable_free`, `ee_printf`) but every
implementation is new, written against this platform's own APIs
(`timer.device`'s `ReadEClock()`, `AllocMem`/`FreeMem`, and a
hand-written `ee_printf` — see that file's own header comment for why
libnix's `%f` is not trusted here). `coremark_amiga.h`/
`coremark_amiga.c` are pure glue this project wrote to let `main()` in
`core_main.c` coexist with `cpubench.c`'s own `main()` (see
`scripts/build-cpubench.sh`'s `-Dmain=coremark_main` compile flag) and
to hand the result back as the float-free values `cpubench.c`'s
`CPUBENCH coremark ...` line reports.

## Vendored file list (for a future re-vendor to diff against)

```
core_main.c
core_list_join.c
core_matrix.c
core_state.c
core_util.c
coremark.h
LICENSE.md
LICENSE-APACHE-2.0.txt
```
