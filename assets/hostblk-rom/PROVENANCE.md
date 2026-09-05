# `hostblk-diagrom.bin` -- vendored build artifact, not a third-party binary

Unlike `assets/aros/`, this binary is **not** third-party code redistributed
under someone else's license -- it is this project's own MIT-licensed
source (`m68k/hostblk-rom/hostblk-diagrom.s`), pre-assembled and committed
here for the same reason `assets/aros/` vendors AROS: so `cargo build`/
`cargo test` work on a machine with no m68k toolchain installed, and CI
never needs one (`docs/ci.md`).

## What this file is

The hostblk card's DiagArea boot ROM, `hostblk.device` exec driver, and
RDB partition mounter (docs/hostblk-protocol.md, ADR 0003): a small
`struct DiagArea` + `struct Resident` copy region (found by Kickstart's
cold-start "Events At ROMTAG INIT Time" scan, RKRM 3rd ed. "Expansion
Library") followed by the device driver and mounter code, which executes
in place from the board's own AUTOCONFIG window rather than being
copied. See the source file's header comment for the full design and
citation trail.

`crates/machine-core/src/hostblk.rs` embeds this file verbatim via
`include_bytes!` and serves it read-only from the board's own AUTOCONFIG
window, at the offset `hostblk::ROM_BASE` names (kept in sync with this
binary's own internal layout only insofar as `ROM_BASE` is *where* the
blob is mapped -- the blob's own contents are entirely position-independent,
see the source file).

## Rebuilding

```
scripts/build-hostblk-rom.sh
```

Requires `vasmm68k_mot` (this project's toolchain notes: `/opt/amiga/bin`).
Rebuild after any edit to `m68k/hostblk-rom/hostblk-diagrom.s` and commit
the new binary in the same change as the source edit -- a source diff with
no binary diff, or vice versa, means one of the two was forgotten.

## Checksum

SHA256 of the file exactly as committed here (recompute and compare after
a rebuild to confirm the toolchain reproduced the same bytes):

```
e043d75af9360ed468a6935461c409d7dbc765ce0877b22aec403dff57f049ac  hostblk-diagrom.bin
```

## License

MIT, per `m68k/LICENSE` and ADR 0003's requirement that this code stay
usable by real MIRAGE hardware and by Copperline plugins (proposal §16).
