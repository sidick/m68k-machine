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
| `m68k-machine.hdf` | Bootable AmigaOS 3.2.2 image: RDB, one `DH0` FFS partition, Picasso96 installed against the Graffity card | `tools/amibake/m68k-machine.toml`, built with [amibake](https://github.com/sidick/amibake) from licensed floppies — see `docs/storage.md` |
| `A1200.47.115.rom` | Kickstart 3.2.2 for the A1200 | Cloanto Amiga Forever, or an image dumped from hardware you own |
| `kickstart-3.1.rom` | Any Kickstart 3.1 image — 40.63 and 40.68 both shipped as 3.1, and the test asserts the major version rather than either one | as above |
| `kickstart-3.2.rom` | Any Kickstart 3.2 image; 3.2, 3.2.1 and 3.2.2 all report version 47 | as above |
| `kickstart-34.5.rom` | Kickstart 1.3, in its doubled 512 KB layout — exercises both the doubling detection and the "too old for Zorro III" path | as above, or `tools/amibake` |
| `kickstart-46.143.rom` | A byte-swapped image, which is why it is here: it is the fixture for the transparent byte-swap path | as above |
| `kickstart-47.7.rom` | Kickstart 3.2's first release | as above |

Each is overridable by environment variable — `M68K_TEST_HDF`,
`M68K_TEST_KICKSTART`, and `M68K_TEST_KICKSTART_3_1` / `_3_2` / `_34_5` /
`_46_143` / `_47_7` — so an existing copy elsewhere on disk can be pointed at
without being duplicated in here.

The Kickstart images beyond the first exist because `machine-core`'s
`rom.rs` checks `identify()` against *real* ROMs rather than synthetic
headers: doubled layouts, byte-swapped images, and version floors are all
things it is easy to get subtly right on a hand-made fixture and wrong on a
real one. Those tests used to read them from absolute paths inside other
checkouts on one developer's machine, which broke the moment those checkouts
moved — hence copies here, under a path this project owns.

AROS needs none of this. It is freely redistributable and so is vendored
directly at `assets/aros/`, which is why the AROS tests run unconditionally
while these skip.
