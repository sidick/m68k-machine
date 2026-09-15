# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A chipset-less Amiga on modern hardware: one Rust host with a `no_std`
machine core, the `m68k` crate (a pinned personal fork — by `rev`, never
`branch`; upstream has no `no_std`) as CPU, booting unmodified Kickstart
3.2 and AROS 68k. Native devices designed for this machine replace
emulated silicon wherever bootstrapping allows. Read
`docs/device-ledger.md` before adding or extending any guest-visible
device — it carries the two standing rules (native-first; never hardcode
AUTOCONFIG-placed addresses — ask `MachineBus`/`GuestMemory`, never
compare against constants) and the permanent/capped/bring-up status of
every device. `docs/combined-roadmap.md` has the phase plan;
`docs/adr-000*.md` record the load-bearing decisions.

## Commands

```sh
cargo test -p machine-core                # unit suite (no fixtures needed)
cargo test -p machine-hosted             # includes real-ROM tests; skip cleanly without fixtures
cargo test -p machine-hosted --test real_rom -- <test_name> --nocapture   # one real-ROM test
cargo fmt --all -- --check               # CI gate
cargo clippy -p machine-core --all-targets -- -D warnings                 # CI gate (also machine-hosted)
cargo clippy -p board-qemu-virt --target aarch64-unknown-none -- -D warnings
cargo clippy -p board-qemu-q35 --target x86_64-unknown-uefi -- -D warnings
```

- **Never `cargo build/test --workspace`**: the board crates are
  bare-metal-only and need explicit `--target` (see the comment in the
  workspace `Cargo.toml`). Build them with `-p <crate> --target <triple>`.
- **Run fmt + clippy before every commit** — CI enforces both and has
  caught missed fmt before.
- Board smoke tests under QEMU: `scripts/run-qemu-virt.sh` /
  `run-qemu-q35.sh`, asserted via `scripts/check-serial-markers.sh`.
  Re-run these after touching `machine-core`; `cargo test` does not
  cover the boards, and they have silently broken before.
- m68k guest code (`m68k/*/`): built by the matching
  `scripts/build-*.sh` (amiga-gcc + vasm at `/opt/amiga/bin`, NDK 3.2
  headers at `~/src/amiga-gcc/projects/NDK3.2/Include_H`). Built
  binaries are committed (see each script's verification steps).
- Test boot images: built by amibake
  (`~/src/amibake/.venv/bin/amibake build tools/amibake/m68k-machine.toml`)
  into `nondistribution/` (licensed media, git-ignored; real-ROM tests
  skip cleanly when absent — see `nondistribution/README.md`). Extra
  files for specific test images are added **post-generation** via
  `scripts/patch-*-hdf.sh` (xdftool), not by modifying amibake.

## Architecture

- `crates/machine-core` — `#![no_std]`, dependency-free, allocator-free
  (borrows all backing storage from the caller). `MachineBus` owns the
  guest memory map; devices are structs it ticks. Guest-visible devices:
  chipset/CIAs/planar renderer/blitter (capped — new display work goes
  to RTG, not here), Zorro III AUTOCONFIG (`autoconfig.rs`), fast RAM,
  `hostblk` (block storage, doorbell + INT2), `pktport` (DosPacket
  transport to host filesystems), `rtgboard` (RTG display), `input`,
  `pcibridge` + `pci.rs` (`PciBackend` seam; `VirtioNetStub` is a real
  virtio-net device model). Register-file idiom: one hot byte per
  4-byte-aligned slot, u32s as four big-endian byte lanes; checked
  arithmetic on every guest-controlled value; hostile input fails
  closed, never panics.
- `crates/machine-hosted` — std runner: boots real ROMs, hosts the
  filesystem service (`pktvol.rs`, over the published `amiga-rdb`/
  `amiga-ffs` crates), net/serial/input backends, `--screenshot`/
  `--inspect` diagnostics. Its `tests/real_rom.rs` is where end-to-end
  proofs live.
- `crates/board-qemu-virt` / `board-qemu-q35` — bare-metal board layers
  (aarch64 / x86-64 UEFI) over the same core.
- `m68k/` — the guest-side stack (MIT, own LICENSE): DiagArea boot ROMs
  in vasm (`hostblk-rom`, `input-rom`, `pktport-rom` — read
  `hostblk-diagrom.s`'s header for the two addressing regimes and the
  vasm `dc.w` no-spaces trap), and gcc-built drivers/libraries
  (`rtgboard-card`, `prometheus-library`, `virtionet-device`). Each
  device has a `docs/*-protocol.md` contract document — the register
  map is specified there, not discovered from code.

## Hard-won project rules

- **Silent failure is this platform's norm.** Verify positively; never
  read absence of a complaint as success. Self-consistent unit tests
  have repeatedly passed over real bugs — end-to-end evidence (serial
  markers, screenshot baselines, real-ROM boots) is the standard for
  "done", and oracles (Copperline — GPL, run but never copy; devsoak/
  devtest; differential tests) are reached for early.
- **Licensing firewall.** GPL/APL/LGPL sources (Copperline, WinUAE,
  AROS, openpci) are read-never-copy oracles. The owner's own BSD-2
  projects are freely copyable and are the preferred references:
  `~/src/amirfb` (working P96 card driver), `~/src/sana2loop` (SANA-II
  device + `docs/sana2-notes.md` + SanaConform), `~/src/amipilot`
  (input injection, object-level GUI automation over `--serial-tcp`).
  Record provenance in docs.
- **Docs move with code.** Ledger rows, protocol docs, and ADR status
  lines are updated in the same commit that makes them true or stale.
  The ledger has twice described finished work as pending — when a
  "not yet built" claim matters, check `m68k/` and the code before
  trusting it.
- Commit style: short narrative subject in the repo's voice (see
  `git log`), body explaining the why.
