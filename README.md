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

**Phase 2 — substantively complete.** Both target OSes boot and render
their real boot screens with no boot device attached: Kickstart 3.2.2's
checkered ball, Hyperion banner and floppy graphic; AROS 68k's cat-eyes
logo, wordmark, "Waiting for bootable media" and device icons. See
`docs/screenshots.md` for the evidence and the diagnostic traps found
along the way, and proposal §8.1 for what the stop-gap renderer behind
this does and does not cover.

The CPU now also runs on bare metal: the project adopted a `no_std` fork
of the `m68k` crate (Phase 0's load-bearing finding — see
`docs/phase0-findings.md`), and `board-qemu-virt` steps a real guest
instruction under QEMU on `aarch64-unknown-none`, no host OS involved.
The fork is pinned by `rev` and is not upstreamed, which is a real
maintenance consideration recorded in that document.

One open architectural question came out of Phase 0 and is recorded in
`docs/adr-0001-bare-metal-vs-linux-host.md`: whether the Phase 5 endgame
is bare metal (owning each board's drivers) or a minimal Linux as the
hardware layer (borrowing them, the Amithlon model). The `no_std`
conversion that ADR was waiting on has landed, which strengthens the
bare-metal option, but the decision itself stays deferred to the Phase 4
measurements as planned. Rock 5B hardware pre-checks are tracked in
`docs/rock5b-week-one.md`.

## Workspace layout

| Path | What |
|---|---|
| `crates/machine-core` | `#![no_std]` library: the guest-visible address bus (`MachineBus`), chipset registers, CIAs, software blitter, and the stop-gap planar renderer (§8.1). |
| `crates/machine-hosted` | Hosted `std` runner: boots a real ROM, drives the guest under `m68k-rs`, and supports `--screenshot`/`--inspect` for diagnostics (`docs/screenshots.md`). |
| `crates/board-qemu-virt` | aarch64 bare-metal board layer targeting QEMU's `virt` machine (`aarch64-unknown-none`); runs a real `CpuCore` and steps guest instructions with no host OS. |
| `crates/board-qemu-q35` | x86-64 board layer targeting QEMU's `q35` machine. |
| `docs/` | Design and process documents (proposal, roadmap, findings, hardware checklists). |

Bare-metal board crates build for their own targets
(`aarch64-unknown-none`, `x86_64-unknown-uefi`) distinct from
`machine-core`'s and `machine-hosted`'s hosted builds; the workspace
`Cargo.toml` carries a comment on how per-crate targets are built.

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option.

## Related

The m68k-side stack (drivers, boot ROMs, DiagArea modules — the code that
runs *on* the emulated 68k, not the Rust host) is planned to live in a
separate repository, not yet created. Storage-related pieces shared with
the MIRAGE interface will live alongside MIRAGE per proposal §16.
