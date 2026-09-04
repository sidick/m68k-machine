# CI

`.github/workflows/ci.yml` runs on every push to `main` and every pull
request, all on `ubuntu-latest`. Four jobs, matching the Phase 0 roadmap
bullet ("CI skeleton: amiga-gcc toolchain container; both QEMU
harnesses"):

## `test`

Hosted lint/build/test. `cargo fmt --all -- --check`, then
`cargo clippy -p machine-core --all-targets -- -D warnings`, plus clippy
for both board crates with their explicit bare-metal `--target` (they
cannot be built for the host target, see `Cargo.toml`'s workspace-layout
note), then `cargo test -p machine-core` (not `cargo test --workspace`:
the board crates are `no_std` bare-metal binaries with no host runner and
would fail to build for the default target).

Proves: the shared `machine-core` crate builds, lints clean, and its
address-bus/memory-map behaviour is correct, on every commit, in well
under a minute.

## `qemu-virt` and `qemu-q35`

Build the aarch64 (`aarch64-unknown-none`) and x86-64 UEFI
(`x86_64-unknown-uefi`) board layers, boot each under its QEMU harness
(`scripts/run-qemu-virt.sh`, `scripts/run-qemu-q35.sh`), and grep the
guest's serial output for a fixed marker line
(`PHASE0 BOARD-QEMU-VIRT: ALL CHECKS PASSED` /
`PHASE0 BOARD-QEMU-Q35: ALL CHECKS PASSED`) written after the payload's
`MachineBus` self-checks (open bus, chip RAM roundtrip, ROM read +
mirroring) all pass. Neither harness script exits QEMU on its own — the
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
`qemu-system-aarch64`) and gives the marker poll 60s. `qemu-q35` installs
`qemu-system-x86` and `ovmf`, and gives the marker poll 120s (OVMF/UEFI
boot is slower than the aarch64 `virt` machine's bare `-kernel` boot).
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

This is **not** the start of the real m68k-side stack — per
`docs/combined-roadmap.md` and the README's "Related" section, that
(drivers, boot ROMs, DiagArea modules — code that runs *on* the emulated
68k) is planned to live in its own repository, not yet created. This job
only proves the amiga-gcc container itself works and produces valid
output, so the toolchain risk is caught early and doesn't surface for the
first time once the real m68k stack exists to build.

## Phase 1 and beyond

Per the roadmap, Phase 1 adds an actual guest boot: the AROS 68k ROM pair
is freely redistributable, so it becomes the public CI gate — "ROM pair
loads, exec starts, serial output reached" — on both harnesses, joining
this skeleton's jobs. Kickstart 3.2 remains the acceptance target driving
day-to-day development (a stricter chipset test) but runs only on
private/local runners with a user-supplied ROM, since it isn't
redistributable; it is not part of the public GitHub Actions workflow.
The qemu jobs' serial-marker-grep pattern established here (via
`scripts/ci-grep-serial.sh`) is expected to carry forward unchanged for
that gate — only the marker text and the payload producing it change.
