# Soaking the `hostblk` driver with devsoak

`docs/hostblk-protocol.md` §13 names devsoak as this driver's acceptance
gate, and `docs/device-ledger.md` makes a devsoak run the condition for
retiring Gayle IDE. This is how the run is set up and what it proved.

## Why an extended image rather than a second unit

devsoak is destructive: it overwrites every sector in the range it is
given. The obvious setup — point it at the boot image — destroys the
filesystem the test is running from, which breaks the test as well as
the disk.

The arrangement used instead is to **extend a copy of the boot image
past its last partition** and aim devsoak at the space beyond. The RDB
is left untouched, so `DH0` still ends at cylinder 575 (sector 18432)
and DOS never looks past it, while the card reports the larger size
because `FileBlockDevice` takes its sector count from the file. No
partition-table surgery, no second unit, and nothing DOS can trip over.

```sh
~/src/amibake/.venv/bin/amibake build tools/amibake/m68k-machine-soak.toml \
  --recipes ~/src/amibake/recipes --assets ~/src/amibake/assets --out out/
dd if=/dev/zero bs=1048576 count=8 >> out/m68k-machine-soak.hdf   # 8 MiB past DH0
```

devsoak and its quirks database are installed by amibake, so nothing is
hand-patched in with `xdftool` any more; only the extension is manual,
because amibake has no way to express "make the image larger than its
last partition". Then run it from the guest's shell, or add a `[[run]]`
entry if an unattended soak is wanted.

### Why the soak needs its own manifest

`tools/amibake/m68k-machine-soak.toml` exists solely because
**AmiPilot and a serial devsoak run cannot share an image.**

devsoak's `-o ser` emits through `RawPutChar`, the ROM debug serial
port, and its own documentation is explicit: *"nothing else may have
serial.device open during a serial run — RawPutChar drives the same
hardware serial.device would use"*. The main manifest autostarts
AmiPilotServer, which holds `serial.device` unit 0 for its wire, so on
that image the two fight over the same Paula registers.

The conflict cannot be dodged by changing devsoak's sink. Serial is the
only way to get its output off this machine — there is no console
capture, so `-o con` would leave the results visible only in a
screenshot. The server has to not be running, which means a separate
image rather than a flag.

AmiPilot is still *installed* on the soak image, so it can be started by
hand for interactive use once no soak is in progress. Only the autostart
differs.

`-r` is in **sectors**, not bytes. `-o ser` sends output to serial,
which the runner already captures, so no log file or screen scraping is
needed. `-y` is required because this machine has no keyboard input yet,
so the destructive-run confirmation cannot be answered. `-K` is
driver-under-test mode: ignore the quirks file entirely.

```sh
./target/release/machine-hosted --rom nondistribution/A1200.47.115.rom \
  --hostblk soak.hdf --hostblk-writable --fast-ram-mb 256 \
  --max-frames 12000 --max-instructions 3000000000
```

## Fast RAM is not optional here

The first run failed with `worker 1: resource allocation failed`. That
was not a driver bug: devsoak's chunk size is 255 sectors (130,560
bytes) and four workers at a queue depth of four need roughly 2 MB of
buffers at once, which does not fit in 2 MB of chip RAM alongside a
booted Workbench. With `--fast-ram-mb 256` the concurrent phase runs.

Worth knowing for its own sake: it means the fast RAM board and the
block card are exercised together, and that the driver really does
accept `MEMF_FAST` buffers — which only works because the card asks the
bus which addresses are RAM instead of comparing against `CHIP_RAM_SIZE`.

## What the run proved

- **`RESULT PASS`**, 0 errors, 4 full matrix passes with 0 failures and
  0 warnings.
- Initial and final audits **clean over 16,384 sectors** — content
  correctness, not merely "the calls returned".
- **`quirks: 0 applied`.** This is the criterion §13 sets out: devsoak
  has a `maxinflight` action for drivers that cannot cope with unbounded
  concurrency, and needing one would have meant the driver was not
  self-limiting. Observed in-flight counts reached 11 against a
  `SUBMIT_CAPACITY` of 8, so the pending-request list really is doing
  its job rather than the load simply staying under the cap.
- All three dialects exercised: `CMD`, `TD64`, `NSD64`.

## A real bug it caught

The first passing run still reported `NSCMD_DEVICEQUERY returned an
unexpected result` and fell back to `dialect enabled: CMD` alone — so
TD64 and NSD64 were implemented but **never tested**, and the run would
have been reported as a pass over a third of the surface.

Cause: `NSDQR_SIZE` was 20. `devices/newstyle.h` gives
`NSDeviceQueryResult` as `ULONG` + `ULONG` + `UWORD` + `UWORD` + `APTR`
= **16**. The driver over-claimed both `nsdqr_SizeAvailable` and
`io_Actual` by four bytes. With that corrected the query is accepted and
all three dialects enable.

This is the value of an external oracle over self-written tests: nothing
we would have written against our own driver would have questioned its
own idea of that structure's size.
