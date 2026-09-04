# m68k Machine (working title)

A chipset-less Amiga on modern hardware: one Rust host built on the
[`m68k`](https://crates.io/crates/m68k) crate (m68k-rs) as the CPU core,
with `no_std` machine core shared by per-platform board layers for
AArch64 and x86-64. Only the chipset surface the OS actually spins on is
emulated; everything else is open bus. The goal is unmodified **Kickstart
3.2** and **AROS 68k** booting to Workbench on RTG, in the tradition of
the DraCo and Amithlon — no ROM patching, no games/demos support by
design.

See `docs/m68k-machine-proposal.md` for the full design rationale and
`docs/combined-roadmap.md` for the phased plan this repo tracks.

## Status

**Phase 0 — Foundations.** Repo and licensing set up; the `m68k` crate's
`no_std` support verified (currently absent — see
`docs/phase0-findings.md`); a skeleton `no_std` address bus
(`crates/machine-core`) implements the Phase 0 slice of the memory map
(chip RAM, ROM window, open bus) with a hosted test that runs a real guest
instruction through `m68k-rs`. Rock 5B hardware pre-checks are tracked in
`docs/rock5b-week-one.md`.

Nothing here boots a real ROM yet. That starts at Phase 1.

## Workspace layout

| Path | What |
|---|---|
| `crates/machine-core` | `#![no_std]`, dependency-free library: the guest-visible address bus (`MachineBus`) and memory map. No CPU, no chipset registers yet — those land in later phases. |
| `crates/board-qemu-virt` *(planned)* | aarch64 board layer targeting QEMU's `virt` machine. |
| `crates/board-qemu-q35` *(planned)* | x86-64 board layer targeting QEMU's `q35` machine. |
| `docs/` | Design and process documents (proposal, roadmap, findings, hardware checklists). |

The board crates are not yet added by this pass of work; they will build
for bare-metal targets (`aarch64-unknown-none`, `x86_64-unknown-uefi`)
distinct from `machine-core`'s own hosted test build, so the workspace
`Cargo.toml` carries a comment on how per-crate targets are expected to
be built once those crates land.

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option.

## Related

The m68k-side stack (drivers, boot ROMs, DiagArea modules — the code that
runs *on* the emulated 68k, not the Rust host) is planned to live in a
separate repository, not yet created. Storage-related pieces shared with
the MIRAGE interface will live alongside MIRAGE per proposal §16.
