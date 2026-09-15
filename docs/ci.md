# CI

`.github/workflows/ci.yml` runs on every push to `main` and every pull
request, all on `ubuntu-latest`. Five jobs: the Phase 0 skeleton ("CI
skeleton: amiga-gcc toolchain container; both QEMU harnesses") plus the
Phase 1 `aros-smoke` gate described below. The Phase 3 unattended-boot
gate has its own section at the end of this document, recording exactly
what of it runs where — that split (public runners vs local media) is
deliberate and load-bearing.

## `test`

Hosted lint/build/test. `cargo fmt --all -- --check`, then
`cargo clippy -p machine-core --all-targets -- -D warnings`, plus clippy
for both board crates with their explicit bare-metal `--target` (they
cannot be built for the host target, see `Cargo.toml`'s workspace-layout
note), then `cargo test -p machine-core`, `cargo clippy -p machine-hosted
--all-targets -- -D warnings`, and `cargo test -p machine-hosted` (not
`cargo test --workspace`: the board crates are `no_std` bare-metal
binaries with no host runner and would fail to build for the default
target). `machine-hosted`'s own test suite includes an ignored
`tests/real_rom.rs` that opportunistically boots a real Kickstart or AROS
ROM if a developer happens to have one on disk at a hardcoded local path
(`cargo test -p machine-hosted -- --ignored --nocapture`); it always skips
in CI, since neither ROM exists there — `aros-smoke` below is CI's actual
AROS coverage, sourcing its own ROM pair rather than relying on that path.

Proves: the shared `machine-core` and `machine-hosted` crates build, lint
clean, and their tested behaviour (address-bus/memory-map correctness,
the run loop's limit/wedge/halt logic) is correct, on every commit, in
well under a minute.

## `qemu-virt` and `qemu-q35`

Build the aarch64 (`aarch64-unknown-none`) and x86-64 UEFI
(`x86_64-unknown-uefi`) board layers and boot each under its QEMU
harness (`scripts/run-qemu-virt.sh`, `scripts/run-qemu-q35.sh`).

Each payload does two things. First the `MachineBus` self-checks (open
bus, chip RAM roundtrip, ROM read and mirroring) plus a real
`m68k::CpuCore` stepping guest instructions, ending in
`PHASE0 BOARD-QEMU-VIRT: ALL CHECKS PASSED` / `...-Q35: ...`. Then it
boots the **embedded AROS 68k ROM pair on bare metal** — no host OS
under it — and narrates the guest's own output over the board's serial
port, which is the roadmap's Phase 1 exit ("a ROM boots on both
harnesses, observed via serial").

The jobs assert the whole chain on the finished log with
`scripts/check-serial-markers.sh`: the Phase 0 marker, the
`PHASE1 ... overlay cleared` line, and AROS's own `callroms done` and
chip-memory list. Asserting all four rather than just the last means a
regression in the bus or CPU checks cannot hide behind a successful
boot.

**Poll for the last line you intend to assert.**
`scripts/ci-grep-serial.sh` kills QEMU the moment its marker matches, so
waiting on an earlier line races the rest of the output. Polling
`callroms done` and then asserting the chip-memory list that follows it
passed on virt and failed on q35 for exactly that reason; both now poll
on the chip-memory line itself.

Neither harness script exits QEMU on its own — the
payload parks forever once done — so both jobs run the script in the
background, redirect its output to a log file, and poll that file for the
marker with `scripts/ci-grep-serial.sh <marker> <logfile> <timeout-secs>
<pid>`, a small shared helper: it polls the log once a second, kills the
QEMU process once the marker appears (or the process exits early), and on
timeout kills the process, dumps the captured log to the job's output,
and fails. Both jobs upload the serial log as a build artifact
unconditionally (`if: always()`), so a failure's output is inspectable
without reproducing it locally.

`qemu-virt` installs `qemu-system-arm` (the Ubuntu package providing
`qemu-system-aarch64`) and gives the poll 180s. `qemu-q35` installs
`qemu-system-x86` and `ovmf`, and gives it 240s — OVMF's own firmware
boot runs before the payload starts at all. Both are generous against
local runs of well under a minute, since a whole guest OS boot now sits
inside that budget.
`scripts/run-qemu-q35.sh` probes a short list of known OVMF code-pflash
paths, including Ubuntu 24.04's `/usr/share/OVMF/OVMF_CODE_4M.fd` (the
package renamed `OVMF_CODE.fd` there); override with the `OVMF_CODE` env
var if a future runner image moves it again.

Proves: both board layers link and run correctly on real (emulated)
bare-metal targets — not just `cargo test`'s hosted build — with a
working polled-serial path, on both platforms this project targets.

## `amiga-gcc`

Compiles `m68k/hello/hello.c` — a deliberately trivial, dependency-free
`int main(void) { return 0; }` — with the `amiga-gcc` cross-toolchain
container (`ghcr.io/reinauer/container-amiga-gcc:latest`, which installs
the toolchain at `/opt/amiga` with `m68k-amigaos-gcc` etc. on `PATH`),
then verifies the output is a real AmigaOS hunk executable by checking
its first four bytes equal the hunk magic `0x000003F3`.

This is **not** the build of the real m68k-side stack — that (drivers,
boot ROMs, DiagArea modules — code that runs *on* the emulated 68k) now
lives in this repo under `m68k/`, built locally by `scripts/build-*.sh`
with the binaries committed (see the README's "Related" section for why
in-repo rather than the separate repository originally planned). This
job only proves the amiga-gcc container itself works and produces valid
output, so toolchain rot is caught on every commit rather than at the
next local driver build.

## `aros-smoke`

Phase 1's guest-boot gate: build `machine-hosted --release`, boot the
AROS 68k ROM pair **vendored in-repo** at `assets/aros/` bounded under
`--max-frames`, and assert on the captured serial output. This is the
roadmap's "**AROS smoke test enters CI here**" bullet — AROS is freely
redistributable, so it is the ROM public CI boots on every commit from
Phase 1 on, proving "ROM pair loads, exec starts, serial output reached"
without waiting for Phase 4.

### Why AROS is the public gate and Kickstart is not

Kickstart 3.2 remains the acceptance target that actually drives
day-to-day development — it is the stricter test, since it is real
hardware firmware rather than a from-scratch reimplementation — but it is
not redistributable, so it can only run on private/local runners with a
developer's own dumped ROM (`crates/machine-hosted/tests/real_rom.rs`'s
ignored `kickstart_3_2_2_a1200` test is that path locally; it is not, and
cannot be, part of this public workflow). Left unchecked, that split lets
the AROS side of the machine silently rot for three more phases before
Kickstart 3.2's own Phase 4 gate would ever notice — this job exists
specifically to prevent that.

**The two ROMs' CI status is not currently symmetric even setting
redistribution aside.** Kickstart 3.2 boots further today than AROS does
observably: locally, Kickstart reaches Exec's idle loop, but Phase 1's
chipset/serial-console work up to now has been driven and verified
against the AROS ROM, and Kickstart's boot narrates nothing over the
serial port at the point it currently reaches — so even a from-scratch
Kickstart CI job would have nothing on serial to assert against yet.

**Update: that observability gap has closed.** Phase 2's renderer landed,
and Kickstart's boot screen is drawn rather than printed, so its progress
is now checkable by capturing a frame — see `screenshots.md` and the
`kickstart_3_2_2_a1200_screenshot_shows_the_boot_screen` test. Kickstart
is also reachable over serial through its own ROMWack debugger
(`serial-debugging.md`), though that observes a deliberately faulted
machine rather than a healthy one. AROS remains the public gate for the
reason that has not changed — it is the redistributable ROM — rather than
because it is the only observable one.

### Sourcing the ROM pair: vendored in-repo, not fetched

The AROS 68k ROM pair (`aros-amiga-m68k-rom.bin` at `$F80000`,
`aros-amiga-m68k-ext.bin` at `$E00000`) lives in this repository at
`assets/aros/`, checked in like any other tracked file. It is licensed
under the [AROS Public License 1.1](https://aros.sourceforge.io/license.html)
(APL, MPL-derived), which permits redistribution in Executable form
provided a notice of source availability accompanies it —
`assets/aros/PROVENANCE.md` is that notice, plus the full compliance and
provenance record: the licence and its URL, the exact upstream artifact
(nightly date, SourceForge URL, and the path inside its ISO the files
came from), both files' SHA256, and their ROM revisions. Read that file
alongside this section; it is written so an audit two years from now can
establish exactly what these bytes are and that shipping them was
licensed, without asking anyone.

**Why vendor rather than fetch.** An earlier version of this job fetched
a pinned dated nightly at CI time instead (`scripts/fetch-aros-rom.sh`,
kept but repurposed — see below). That worked, and the licence research
behind it (APL 1.1 permits Executable-form redistribution with a
source-availability notice) is exactly what makes vendoring legitimate
now. But a *pinned fetch* trades one failure mode for another: this gate
exists specifically so AROS cannot silently rot before its Phase 4
obligation (`docs/combined-roadmap.md`), and a pinned SourceForge nightly
will eventually be pruned from upstream's retention window — at which
point the gate starts failing for a reason that has nothing to do with
this repo's code, indistinguishable in the Actions UI from a real
regression until someone reads the log closely. A gate whose failures
need to be triaged as "us or upstream" before anyone can trust them
undercuts the reason it exists. Vendoring the roughly 1 MiB pair buys
reproducibility forever, no network dependency in CI, no `p7zip-full`
install step, and a faster job — at the cost of ~1 MiB of binary in git
history, which is the trade the roadmap's "gate every commit" intent
weighs in favour of.

### Refreshing the vendored ROM pair

Don't hand-edit `assets/aros/*.bin`. `scripts/fetch-aros-rom.sh` is kept
specifically as the reviewable way to do this deliberately — it is no
longer run by CI, only by a human choosing to update the vendored pair
(its own header says so). To refresh:

1. Pick a current AROS nightly date and update `aros_date` and the three
   pinned SHA256 values (`zip_sha256`, `rom_sha256`, `ext_sha256`) at the
   top of `scripts/fetch-aros-rom.sh` — compute them by hand from the new
   `amiga-m68k-boot-iso.zip` and its two extracted ROM files so the diff
   is auditable, not copy-pasted from the script's own (as yet unrun)
   output.
2. Run `scripts/fetch-aros-rom.sh <some-output-dir>`; it downloads,
   verifies against the values from step 1, and extracts the pair.
3. Copy the two output files over `assets/aros/aros-amiga-m68k-rom.bin`
   and `assets/aros/aros-amiga-m68k-ext.bin`.
4. Boot them locally with the same command the CI job runs (below) and
   confirm the three markers still appear.
5. Update `assets/aros/PROVENANCE.md`'s date, both ROM revisions (read
   off `machine-hosted`'s own ROM-identify diagnostic in that boot's
   output), and all three checksums to match.
6. Commit the two binaries, the script's new pin, and `PROVENANCE.md`
   together, so the whole change is one reviewable diff.

### Running it and what it asserts

Unlike the QEMU jobs, `machine-hosted` is not a payload that parks
forever — bounded by `--max-frames`, it always terminates on its own (see
`crates/machine-hosted/src/run.rs`'s `Outcome`), so there is no
background-process-plus-poll step here the way
`scripts/ci-grep-serial.sh` provides for QEMU. The job instead runs it to
completion, tees output to `aros-serial.log` via `--serial-log`, then
checks the log with `scripts/check-serial-markers.sh` (a completed-log
sibling to `ci-grep-serial.sh`'s growing-log-with-a-pid design).

The runner's own exit code cannot be the pass/fail signal by itself:
`Report::exit_code` returns `1` for `Outcome::LimitReached`, and hitting
`--max-frames` is the *expected*, passing outcome at this phase — the
chipset/CIA implementation is still landing, so the guest is not expected
to reach a clean `STOP` with all interrupts masked. The job accepts exit
codes `0` (clean halt) and `1` (bounded run) and treats anything else
(`2` wedged, `3` setup error) as a real failure.

Assertions are three fixed substrings in the serial log, chosen to prove
the full exit criterion — "ROM pair loads, exec starts, serial output
reached" — while staying stable across routine AROS nightly bumps (a
version bump should not spuriously break this job):

- `PHASE1 HOSTED: reached overlay-cleared` — `machine-hosted`'s own
  marker, not AROS's, so it never changes with the guest ROM: the CPU has
  left the ROM overlay and is executing from RAM-mapped exec.
- `GUEST | callroms done` — exec's own boot trace once it has finished
  walking and initialising ROM resident modules; present on every AROS
  nightly that gets this far, and a much more direct "exec started" proxy
  than any address or version string.
- `'chip memory'` — exec's `MemHeader` listing reaching serial, the
  concrete "serial output reached" evidence, without pinning to exact
  address ranges (which would break if the machine's memory map changes)
  or ROM revision numbers (which change every nightly).

`aros-serial.log` is uploaded as a build artifact unconditionally
(`if: always()`, matching the QEMU jobs), and a missing marker makes
`check-serial-markers.sh` dump the whole captured log to the job's output
before failing, so a failure is diagnosable from the Actions UI alone.

## The Phase 3 unattended-boot gate

The roadmap's Phase 3 exit criterion ("boots unattended to Workbench on
RTG with input, storage, network under QEMU") exists as one composed
real-ROM test:
`kickstart_3_2_2_a1200_boots_unattended_to_rtg_workbench_with_input_storage_network`
in `crates/machine-hosted/tests/real_rom.rs` — a SINGLE boot of a single
image proving all four properties at once, composing the three existing
proofs (rtgboard desktop, rtgboard×input pointer/click, virtionet
first-packet round trip) plus a guest storage write read back out of the
booted image with xdftool. The image is built by
`scripts/patch-unattended-hdf.sh`, which chains the rtgboard and
virtionet patch scripts onto the amibake base and adds the one
storage-evidence line; the individual per-feature tests all remain.

### What runs where

**Locally (the gate proper):** `scripts/phase3-gate.sh` is the single
command. It refuses to run without the licensed inputs (a gate that can
silently skip is not a gate — the underlying cargo test, correctly,
skips clean clones without media), rebuilds the composed image fresh
from the base, and runs the one test. This is what a release-gating
machine with the media actually executes.

**Public runners (`test` job):** the gate test cannot run there —
Kickstart 3.2.2 is licensed media that cannot be redistributed to
GitHub's runners, the same split the `aros-smoke` section above records
— but `cargo test -p machine-hosted` compiles it on every commit, and a
dedicated step asserts it stays *listed* (`cargo test --test real_rom
-- --list | grep`), so renaming it away or dropping it fails public CI
even though running it cannot.

**The AROS side stays smoke-level, deliberately.** `aros-smoke` remains
the public boot gate ("ROM pair loads, exec starts, serial output
reached"). An AROS-to-Workbench-on-RTG assertion is NOT part of the
Phase 3 gate: it depends on resolving P96-vs-HIDD, which is Phase 4's
recorded AROS work (`docs/combined-roadmap.md`), and pulling it forward
here would gate Phase 3 on Phase 4's open question.

### Scope: "both platforms" and the board crates

The roadmap's exit line says "both platforms". What this gate proves,
and what it deliberately does not, is recorded as an annotation on that
exit line in `docs/combined-roadmap.md` rather than restated here —
read it there. The short form: the gate runs the shared `machine-core`
through `machine-hosted` (the same core the board layers embed, byte
for byte), while the bare-metal board crates have no storage/RTG/input/
PCI devices wired and their device bring-up (real-ECAM `PciBackend` and
the rest) moves explicitly to the Phase 4/5 hardware work where ADR
0001's decision needs it.
