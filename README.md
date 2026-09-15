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

**Phase 3 — complete: "basically usable (emulated)".** One unattended
boot of one maintained image reaches Workbench on the native RTG board
with working input, storage and network — held as a single composed
gate test (`scripts/phase3-gate.sh` is the one command; `docs/ci.md`
records what runs where, since Kickstart is licensed media the public
runners cannot boot). The roadmap's Phase 3 exit line carries a
recorded scope note rather than a quiet pass: the gate proves the
shared `machine-core` end to end, which both bare-metal board layers
embed byte-identically, but those layers' own device bring-up is Phase
4/5 hardware work — see the annotation in `docs/combined-roadmap.md`.

The devices behind that boot, tracked device-by-device in
`docs/device-ledger.md`:

- **Storage:** `hostblk`, a doorbell-plus-descriptor Zorro III block
  card with a host-side transfer engine (ADR 0003), boots Kickstart
  3.2.2 and AROS unassisted via its own DiagArea ROM and RDB mounter
  (`m68k/hostblk-rom/`), verified against devsoak
  (`docs/hostblk-soak.md`) and devtest (`docs/devtest-hostblk.md`).
  Gayle IDE, its bring-up predecessor, has been retired. MIRAGE's block
  plane (`mirage.rs`) is kept off the boot path as MIRAGE's own
  reference implementation.
- **Filesystems run on the host** (ADR 0004): a DosPacket transport
  card (`pktport`) carries AmigaDOS packets to a thin m68k handler
  stub, behind which `amiga-ffs` (host-implemented FFS, one filesystem
  family per crate) and `amiga-rdb` (RDB/partition handling) do the
  real work. Measured against guest-executed FFS over `hostblk` with
  DiskSpeed 4.2: 2–7× faster, largest gains in file-create and -delete
  (`docs/pktport-measurement.md`), confirming the ADR's prediction.
- **Display:** `rtgboard`, a generic native RTG board per ADR 0002
  (rejecting a `uaegfx`-compatible approach as a dead end), drives a
  real Workbench desktop through its own P96 `.card` driver
  (`m68k/rtgboard-card/`, `docs/rtgboard-protocol.md`), screenshot-
  and serial-verified. The emulated Cirrus CL-GD542x/Graffity path
  stays as the zero-install compatibility tier.
- **Input:** the native input card (`input.rs`,
  `docs/input-protocol.md`) delivers pointer motion, clicks and
  keypresses through its own DiagArea driver ROM (`m68k/input-rom/`),
  proven end to end on the rtgboard RTG screen — P96 soft-renders the
  pointer, Intuition routes the clicks, scripted double-clicks open
  real drawers.
- **Network and PCI:** `pcibridge` exposes a real PCI surface (ADR
  0005) behind `pci.library` — the Prometheus API, implemented by
  `LIBS:prometheus.library` (`m68k/prometheus-library/`,
  `docs/pci-library.md`) — and a virtio-net function driven by
  `virtionet.device` (`m68k/virtionet-device/`, SANA-II,
  `docs/virtionet.md`): first-packet round trip and a SanaConform
  conformance pass, all narrated over serial.
- Zorro III AUTOCONFIG (`autoconfig.rs`) and fast RAM (`fastram.rs`,
  on by default at 256 MB) underpin all of the above; addresses are
  always asked from `MachineBus`, never hardcoded.

The CPU also runs on bare metal: the project adopted a `no_std` fork
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
| `crates/machine-core` | `#![no_std]` library: the guest-visible address bus (`MachineBus`), chipset registers, CIAs, software blitter, the stop-gap planar renderer (§8.1), Zorro AUTOCONFIG and fast RAM, and the native `hostblk` (block storage), `pktport` (filesystem transport), `rtgboard` (RTG display) and `input` device cards. |
| `crates/machine-hosted` | Hosted `std` runner: boots a real ROM, drives the guest under `m68k-rs`, hosts `pktvol` (the `amiga-ffs`/`amiga-rdb`-backed filesystem service behind `pktport`), and supports `--screenshot`/`--inspect` for diagnostics (`docs/screenshots.md`). |
| `crates/board-qemu-virt` | aarch64 bare-metal board layer targeting QEMU's `virt` machine (`aarch64-unknown-none`); runs a real `CpuCore` and steps guest instructions with no host OS. |
| `crates/board-qemu-q35` | x86-64 board layer targeting QEMU's `q35` machine. |
| `m68k/` | The m68k-side stack: boot ROMs and drivers that run *on* the emulated CPU, built with amiga-gcc — `hostblk-rom`, `pktport-rom` + `pktport-handler` (the DosPacket handler stub), `input-rom`, and a `hello` smoke test. Built via `scripts/build-*.sh` and amibake recipes under `tools/amibake/`. |
| `docs/` | Design and process documents (proposal, roadmap, ADRs, protocol specs, findings, hardware checklists). |

Bare-metal board crates build for their own targets
(`aarch64-unknown-none`, `x86_64-unknown-uefi`) distinct from
`machine-core`'s and `machine-hosted`'s hosted builds; the workspace
`Cargo.toml` carries a comment on how per-crate targets are built.

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option. `m68k/` carries its own
[MIT licence](m68k/LICENSE), since that code runs on the guest rather
than as part of the host workspace.

## Related

The m68k-side stack (drivers, boot ROMs, DiagArea modules — the code that
runs *on* the emulated 68k, not the Rust host) lives in this repo under
`m68k/`, alongside the Rust host rather than in a separate repository as
originally planned in proposal §16. `amiga-rdb` and `amiga-ffs`, the
host-side RDB and FFS crates behind `pktport`/`pktvol`, are published
separately on crates.io. MIRAGE's own interface and management-plane
work, where it diverges from this machine's `hostblk`/`pktport` path,
still lives alongside MIRAGE per proposal §16.
