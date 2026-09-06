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
```

Nothing else. devsoak, its quirks database and devtest are installed by
amibake, and the manifest's `[hdf].scratch = "8M"` reserves a second,
unformatted `DH1` partition at the end of the disk for devsoak to
destroy. `DH0` keeps the system intact.

That replaces an earlier `dd` extension of the built image. Both give
devsoak somewhere safe to write, but the extension left the space
**invisible to the RDB** — raw sectors past the last partition, with a
range that had to be computed by hand from `DH0`'s geometry and would
have silently eaten the filesystem if computed wrong. A real partition
is bounded by the RDB itself.

Verified: the guest boots normally with the scratch partition present,
this project's own RDB mounter offers `DH0` as the boot node and `DH1`
as a non-bootable one, and Workbench shows a third icon reading
`DH1:Uninitialized` — no requester, no stall.

The soak now runs **unattended**: the manifest's `[[run]]` entry starts
it at boot, so building the image and booting it is the whole procedure.

```toml
[[run]]
command = "C:devsoak"
args    = "DH1: -d -t 30s -y -K -o ser"
detach  = true
```

Two things arriving together made autostarting a destructive test
acceptable, and neither alone would have been enough.

**Partition mode.** A positional `DH1:` makes the partition's own extent
the range. devsoak resolves it itself — the run log reads `DH1: =
hostblk.device unit 0, partition sectors 20480..36864` — so there is no
hand-computed range to get wrong, and no way to name one that reaches
past the partition into `DH0`. The arithmetic this replaced was correct
only for one image's geometry and would have silently become wrong the
moment `DH0` grew.

**Confirmation tiers that escalate by what is actually there.** `-y`
skips the prompt for an unrecognisable or merely formatted partition,
but a **live mounted volume** makes devsoak *refuse* the run rather than
proceed. So a misconfigured manifest cannot destroy a filesystem someone
is using; the worst case is a refusal. devsoak also inhibits the
partition for the duration, blocking filesystem access while it writes.

`-K` is driver-under-test mode — ignore the quirks file entirely, since
a quirk entry describing our own driver would be us excusing our own
bug.

### Result

`RESULT PASS`, 0 errors, 4 matrix passes with 0 failures and 0 warnings,
initial and final audits clean over 16,384 sectors, all three dialects
(`CMD`, `TD64`, `NSD64`) exercised, and **`quirks: 0 applied`** — §13's
stated criterion, since needing a `maxinflight` entry would have meant
the driver was not self-limiting against `SUBMIT_CAPACITY`.

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
