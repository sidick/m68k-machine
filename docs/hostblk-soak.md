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
cp nondistribution/m68k-machine.hdf soak.hdf
dd if=/dev/zero bs=1048576 count=8 >> soak.hdf     # 8 MiB past DH0
xdftool soak.hdf write ~/src/devsoak/devsoak devsoak
# append to S/User-Startup (it already carries the Picasso96 assign):
#   SYS:devsoak hostblk.device 0 -d -r 18432,16384 -t 30s -y -K -o ser
```

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
