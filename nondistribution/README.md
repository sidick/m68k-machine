# Non-distributable media

Files the test suite needs but that this repository may not carry: Kickstart
ROMs, which are Cloanto's, and AmigaOS hard disk images, which contain
licensed Workbench media. Everything in this directory except this README is
ignored by git, so nothing here can reach a commit by accident.

Nothing here is required to build, and no test *fails* when it is missing —
the tests that use these files skip cleanly instead, so a fresh clone with no
media still goes green. What they need is a path that survives between runs,
which is the point of this directory: it previously lived in a session-scoped
temporary directory, and when that directory vanished the tests silently
skipped forever while still reporting success.

## What goes here

| File | What it is | Where it comes from |
|---|---|---|
| `m68k-machine.hdf` | Bootable AmigaOS 3.2.2 image: RDB, one `DH0` FFS partition, Picasso96 installed against the Graffity card | `tools/amibake/m68k-machine.toml`, built with [amibake](https://github.com/simond/amibake) from licensed floppies — see `docs/storage.md` |
| `A1200.47.115.rom` | Kickstart 3.2.2 for the A1200 | Cloanto Amiga Forever, or an image dumped from hardware you own |

Both are also overridable by environment variable — `M68K_TEST_HDF` and
`M68K_TEST_KICKSTART` — so an existing copy elsewhere on disk can be pointed
at without being duplicated in here.

AROS needs none of this. It is freely redistributable and so is vendored
directly at `assets/aros/`, which is why the AROS tests run unconditionally
while these skip.
