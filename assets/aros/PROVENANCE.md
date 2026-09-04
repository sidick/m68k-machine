# Vendored AROS 68k ROM pair

`aros-amiga-m68k-rom.bin` and `aros-amiga-m68k-ext.bin` are the AROS
Research Operating System's m68k ROM pair, vendored into this repository
so `.github/workflows/ci.yml`'s `aros-smoke` job can boot the Phase 1 gate
("AROS smoke test enters CI here", `docs/combined-roadmap.md`) on every
commit with no network access and no dependency on an upstream mirror
staying reachable. `docs/ci.md` explains why this repo vendors rather than
fetches; this file exists so anyone auditing these two binaries can
establish exactly what they are and that shipping them is licensed,
without asking the people who put them here.

## What these files are

| File                          | Size      | Maps at  | Role                          |
|--------------------------------|-----------|----------|-------------------------------|
| `aros-amiga-m68k-rom.bin`     | 524288 B  | `$F80000` | Kickstart-replacement ROM     |
| `aros-amiga-m68k-ext.bin`     | 524288 B  | `$E00000` | Extended ROM                  |

ROM revisions, as reported by `machine-hosted`'s own ROM-identify
diagnostic on boot (which reads each image's embedded ROMTag/version
data): main ROM revision **46.12**, ext ROM revision **46.11** — see
"Local verification" below for the exact diagnostic lines.

## Upstream artifact

Both files are extracted, unmodified, from the **official AROS nightly
build** dated **2026-09-04** (SourceForge's `nightly2` tree names builds
by date, `20260904`):

- Download: `https://sourceforge.net/projects/aros/files/nightly2/20260904/Binaries/AROS-20260904-amiga-m68k-boot-iso.zip`
- That zip contains `AROS-20260904-amiga-m68k-boot-iso/aros-amiga-m68k.iso`
  (ISO9660, 411314176 bytes).
- Inside that ISO: `boot/amiga/aros-rom.bin` and `boot/amiga/aros-ext.bin`
  (renamed here to the WinUAE/FS-UAE `aros-amiga-m68k-{rom,ext}.bin`
  convention that `crates/machine-hosted` and Copperline both use).

This is the **vanilla official nightly build** — not a custom fork or
patch set. (For contrast: `~/src/external/Copperline/assets/aros/`, a
sibling project, vendors a differently-built pair from a development
branch with extra CD32/serial patches for its own emulation-fidelity
goals; this repo deliberately does not use that pair, since Phase 1 only
needs a stock, citable AROS build, and the vanilla nightly was confirmed
locally to produce identical boot behaviour for this repo's purposes —
see "Local verification" below.)

## Checksums

SHA256 of the two files exactly as committed here:

```
cf469f18daa1e82645de1d6cf79ac1b6681983a210fbffb120a955f3f00e7fb9  aros-amiga-m68k-rom.bin
681bc4309fd958a1f12046c9d4477ed80434afc8f319ebe64ee24aa0d5bb943c  aros-amiga-m68k-ext.bin
```

SHA256 of the upstream zip these were extracted from (for re-deriving the
chain from scratch; the zip itself is not vendored, only its two ROM
files):

```
6e6fefd70d9a8943dc4fc221d9335d6562a2dab6ffd833b29cd3985b7719fe0b  AROS-20260904-amiga-m68k-boot-iso.zip
```

## Licence and source-availability notice

AROS is distributed under the **AROS Public License, version 1.1** (APL,
an MPL-derived licence) — full text in `LICENSE` in this directory, also
published at <https://aros.sourceforge.io/license.html>. The APL permits
distributing Covered Code in Executable form (§3.6, "Distribution of
Executable Versions") provided each such distribution is accompanied by a
notice that the Source Code version is available under the licence and a
description of how to obtain it. This file is that notice:

> The Source Code for `aros-amiga-m68k-rom.bin` and
> `aros-amiga-m68k-ext.bin` is available under the AROS Public License
> 1.1 from the AROS project's own repository,
> <https://github.com/aros-development-team/AROS>, and from the AROS
> nightly build system at <https://aros.sourceforge.io/>. The exact
> nightly these binaries were built from is identified above (build date
> `20260904`); AROS nightlies are built directly and reproducibly from
> that repository's `master` branch, so that repository's history is the
> Source Code corresponding to this Executable form.

No modification was made to either file after extraction from the
upstream ISO — this repository is a redistributor of the Executable form
only, not a Contributor under the APL, so no additional Modification
notice applies.

## Local verification

Before vendoring, both files were run through `machine-hosted --release`
bounded at `--max-frames 300` and confirmed to produce the Phase 1 exit
evidence on serial (`docs/ci.md`'s `aros-smoke` section has the full
rationale for the specific markers asserted in CI):

```
host  | main ROM: Aros rev 46.12, exec 51.9, 524288 bytes, checksum ok, boot_pc 0x00f800d8
host  | ext ROM: Aros rev 46.11, exec 46.11, 524288 bytes, checksum ok, boot_pc 0x00f80002
host  | PHASE1 HOSTED: reached overlay-cleared (frame 3, instr 562948, PC 0x00f80324)
GUEST | ROMInfo: 1MiB ROM detected
GUEST | callroms done
GUEST | 00001000: 00001000 - 00200000 00001703 -10 'chip memory'
host  | PHASE1 HOSTED: LIMIT REACHED (max-frames) -- 2584153 instructions, 305 frames, overlay cleared, final PC 0x00fe8316
```

## Refreshing this pair

Do not overwrite these files casually. When a deliberate update is
wanted (e.g. picking up upstream AROS fixes), use
`scripts/fetch-aros-rom.sh` — no longer part of CI, but kept exactly for
this: it downloads a nightly by date, verifies it end-to-end against
pinned checksums, and extracts the same two files by the same path used
here, so the refresh is mechanical and reviewable rather than ad hoc. See
that script's header and `docs/ci.md` for the full procedure. Update the
date, both file checksums, the zip checksum, and the ROM revisions in
this file together with the new binaries, in the same commit.
