# pktport vs hostblk: the ADR 0004 measurement

DiskSpeed 4.2, `DIR SEEK FAST LONG` — the exact protocol of
`~/src/emulator_disk_speed/REPORT.md`'s Copperline matrix — run twice
on **one machine in one boot** (2026-09-08): first against `DH0:`
(guest FFS over `hostblk`, this machine's copperhf-analogue), then
against `PKT0:` (host-side `amiga-ffs` behind the DosPacket transport,
handler autoloaded from the card ROM). Same 68040, same Kickstart
3.2.2, same guest clock; the *ratio* is therefore meaningful however
our machine's clock compares to anyone else's.

Built by `tools/amibake/m68k-machine-diskspeed.toml` (amibake's
`diskspeed` recipe; `mmulibs` for clean cache enable; no AmiPilot, so
`SER:` is free for results). Runner: `--hostblk <image>
--hostblk-writable --pktvol <served> --pktvol-writable --fast-ram-mb
64`, serial captured with `--serial-log`.

## Results

| test | DH0 (guest FFS) | PKT0 (host FS) | ratio |
|---|---:|---:|---:|
| File Create | 472 /s | 1,794 /s | **3.8×** |
| File Open | 661 /s | 1,683 /s | 2.5× |
| Directory Scan | 3,010 /s | 6,304 /s | 2.1× |
| File Delete | 873 /s | 3,185 /s | **3.6×** |
| Seek/Read | 2,254 /s | 4,700 /s | 2.1× |
| 512 B create/write/read | 1.5 / 1.9 / 2.0 MB/s | 3.8 / 3.8 / 3.8 MB/s | ~2× |
| 4 KB create/write/read | 10.0 / 14.7 / 15.3 MB/s | 28.1 / 30.1 / 30.1 MB/s | ~2× |
| 32 KB create/write/read | 29.0 / 73.3 / 76.6 MB/s | 146.7 / 221.6 / 221.9 MB/s | ~3× |
| 256 KB create | 44.9 MB/s | 311.1 MB/s | **6.9×** |
| 256 KB write/read | 219.4 / 223.0 MB/s | see below | **≥ 4.9×** |

## The 256 KB "collapse" that wasn't

DiskSpeed printed 17.7/18.6 MB/s for PKT0's 256 KB write/read — an
apparent catastrophic regression. It is DiskSpeed's own 32-bit byte
counter wrapping: a per-packet trace counted **33,335 reads of 262,144
bytes inside the 8-second window** (`MIN_TEST_TIME 8`, its own source,
which ships in the recipe archive) — 8,738,897,920 bytes, which mod
2³² is 148,963,328, and divided by 8 s is the 18.6 MB/s it printed, to
within the window's rounding. The true guest-measured rate is
**≈ 1.09 GB/s**. The benchmark from 1992 cannot represent how fast the
volume is; every number below 4 GB-per-window (all of DH0's, and
PKT0's other rows) is unaffected.

## Reading the result against the ADR

ADR 0004 predicted "near the HOSTFS end of the range, with the largest
gains in file-create and seek rates", and committed to revisiting if
the number landed near copperhf's instead. **Confirmed**: metadata
rates lead the table (create 3.8×, delete 3.6×), throughput runs
2–5×+, and nothing landed near the copperhf end. The Copperline
matrix's absolute numbers are *not* directly comparable (68030/25
guest there, 68040 here; different machines, different clocks) — the
comparable object is the shape, and the shape matches: removing the
guest-executed filesystem removes a multiple, biggest where metadata
dominates.

Found along the way: `MODE_OLDFILE` (`ACTION_FINDINPUT`) handles are
*not* read-only on AmigaDOS — DiskSpeed's Write test opens existing
files that way and writes through them, and the backend's original
`write: false` aborted the whole pass. Fixed with a regression test
named for the sequence. The first full benchmark run doubling as a
conformance test is exactly why the measurement was worth building.

## Caveats

- One run, not a median of three; the matrix's methodology says three.
  The numbers are far enough apart that the conclusion doesn't hang on
  run-to-run variance, but a CI-grade version should do the medians.
- `--pktvol` served a bare 32 MB DOS\1 image; DH0 is the 64 MB DOS\3
  boot volume. Different variants and fill levels — a stricter run
  would serve the same image both ways.
- DiskSpeed's CPU-availability figures read 0% throughout on this
  machine and were ignored.
