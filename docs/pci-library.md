# `pci.library` — the Prometheus-compatible bus library

**Status:** ADR 0005 **stage 2**, **landed and proven 2026-09-15** (§7).
Contract and provenance were drafted first, before any implementation
code, and the implementation was then written to them. The deliverable
the ADR calls `pci.library` ships on disk as **`LIBS:prometheus.library`,
version 3** — that is the name openpci.library's Prometheus wrapper
(and every period driver) opens, so an invented filename would defeat
the compatibility the library exists for. "pci.library" remains the
project-internal name for the deliverable as a whole (library plus its
host-side INTx half plus the probe tool).

**Context:** `docs/adr-0005-networking-virtio-behind-pci-library.md`
(the decision and staging); `docs/pcibridge-protocol.md` (the stage-1
shim this library programs — its register contract, and the stage-2
additions this stage lands there); `docs/m68k-machine-proposal.md`
§10.1 (the API commitments: prometheus.library v2/v3 natively, the
openpci wrapper running on top, AmigaPCI alignment);
`docs/device-ledger.md` (the address rule this library's discovery
obeys, and the ledger row this stage adds).

---

## 1. API provenance — where the contract comes from

The prometheus.library API is defined here by the **Matay Prometheus
SDK 3.0** (13.3.2005, © 2000–2005 Matay): `fd/prometheus_lib.fd`,
the full include set (`libraries/prometheus.h`,
`clib/prometheus_protos.h`, pragma/inline/lvo forms), and
`autodocs/prometheus.doc` — the vendor's own developer documentation,
published to enable exactly the third-party driver development this
library re-enables. A complete copy is redistributed inside the
`jeperk/OpenPCI` repository on GitHub
(`PrometheusOpenPci/Prometheus-3.0/`), which is where this project
obtained it; the SDK itself was announced publicly in 2004
(amiga-news.de AN-2004-03-00078) and originally distributed from
matay.pl (now defunct).

What the SDK pins down, and this stage treats as normative:

- **The function set and LVO table.** 15 public functions, FD bias 30:
  `Prm_FindBoardTagList` (-30), `Prm_GetBoardAttrsTagList` (-36), the
  six config accessors (-42…-72), `Prm_SetBoardAttrsTagList` (-78),
  `Prm_AddIntServer`/`Prm_RemIntServer` (-84/-90),
  `Prm_AllocDMABuffer`/`Prm_FreeDMABuffer` (-96/-102),
  `Prm_GetPhysicalAddress` (-108), and V3's `Prm_GetVirtualAddress`
  (**-114**). One SDK erratum recorded rather than propagated: the
  SDK's `lvo/prometheus_lib.i` says `-112` for `Prm_GetVirtualAddress`,
  which breaks the 6-byte LVO grid; the pragma and inline headers —
  the forms compilers actually generated calls from — both say `0x072`
  = -114, and -114 is what this library implements.
- **Register conventions** per the FD file (`a0/a1` for
  board/taglist/interrupt, `d0/d1` for data/offset).
- **Tag values** (`PRM_Vendor` `0x6EDA0000` …
  `PRM_FunctionNumber` `0x6EDA0007`, the `PRM_MemoryAddrX`/
  `PRM_MemorySizeX` last-nybble guarantee, `PRM_BoardOwner`'s
  lock semantics).
- **The byte-order presentation** (§3) — the autodoc documents it with
  worked examples, so it is not inferred from any implementation.

**Licensing firewall, recorded per the project's standing rule:**

- The SDK files are Matay's ("All Rights Reserved" headers). They are
  used as the **interface specification** — function names, register
  assignments, LVO offsets, tag values, documented semantics — which
  is exactly what a vendor SDK is published for. No SDK file is copied
  into this repository; the library's own headers are written fresh
  against the specification (the amirfb/P96 precedent, except that
  P96's SDK was BSD-2 and copyable — this one is reference-only).
- `jeperk/OpenPCI`'s own code (openpci.library sources) is
  **LGPL-2.1**: read as an oracle for how the wrapper calls
  prometheus.library, never copied or translated.
- **AROS**'s prometheus.library reimplementation and PCI HIDDs are
  APL: oracle only, never copied or translated.
- **WinUAE/Amiberry**'s Prometheus bridge emulation (`pci.cpp`) is
  GPL: oracle only — valuable as the second-source check on the
  hardware's byte-lane behaviour, never copied or translated.
- The Prometheus hardware design archive (rastport.com, Grzegorz
  Kraszewski) is CC BY-NC-SA 3.0, and E3B's FireStorm/VHDL releases
  carry their own terms: documentation reference only; nothing from
  either enters this repository.

Everything in this stage's m68k and Rust code is written by this
project against the SDK's documented contract.

## 2. What the library is, on this machine

A gcc-built AUTOINIT disk library (`m68k/prometheus-library/`,
following `m68k/rtgboard-card/`'s shape), programming the `pcibridge`
Zorro III shim (`docs/pcibridge-protocol.md`). At init it:

1. Finds its board the only permitted way: `FindConfigDev()` for
   manufacturer `0x07DB`, product `6`, claiming `CDF_CONFIGME` — never
   a hardcoded address (`docs/device-ledger.md`, the rule for
   addresses). Refuses any card `VERSION` other than `2` (the stage-2
   protocol version — the INTx registers are part of this library's
   contract, so a version-1 card is positively rejected, not limped
   along with).
2. Enumerates bus 0 through config cycles (devices 0–31; functions
   1–7 only where the header-type multifunction bit says so; vendor
   `0xFFFF` = absent, the all-ones master-abort contract). Flat bus
   only: no bridge walking — the virtual topology is flat, real
   Prometheus is a flat 4-slot bus, and stage 3 needs nothing more.
   Recorded as a limitation, not a surprise.
3. **Assigns BARs** (the allocation stage 1 deliberately left to this
   library): each 32-bit memory BAR sized by the standard probe and
   allocated ascending, size-aligned, from the PCI memory region at
   `PCI_MEM_BASE = $20000000`, 8 MB — chosen **below the Zorro III
   window** (proposal §10.1) and clear of every guest-RAM bus address,
   so DMA addresses and BAR addresses can never collide; sized to the
   aperture so a single `APERTURE_BASE = $20000000`, written once at
   init, banks the whole allocated range permanently. A device whose
   BARs cannot fit is left unassigned (attrs read 0) and narrated,
   never silently truncated. I/O BARs are not assigned (no I/O
   aperture exists; real Prometheus drivers use memory BARs).
   Expansion-ROM BARs are not assigned (`PRM_ROM_Address`/`_Size`
   answer 0). After assignment the device's COMMAND register gets
   memory-decode and bus-master enable.
4. Records per-function attributes for `Prm_GetBoardAttrsTagList`:
   `PRM_MemoryAddrX` is the **CPU-visible aperture address**
   (`board base + $800000 + (bar − PCI_MEM_BASE)`, the board base
   asked from the ConfigDev, never assumed), which is what a driver
   pokes — exactly the role the real Prometheus window plays.

## 3. The byte-order contract — the part that must not be tidied

The SDK autodoc defines the presentation with a worked example
(vendor `$5678`, device `$1234` — i.e. config longword 0 is the value
`$12345678`):

```
Prm_ReadConfigLong(board, 0) == $12345678
Prm_ReadConfigWord(board, 0) == $1234     /* the DEVICE id */
Prm_ReadConfigWord(board, 2) == $5678     /* the VENDOR id */
Prm_ReadConfigByte(board, 0) == $12
Prm_ReadConfigByte(board, 1) == $34
Prm_ReadConfigByte(board, 2) == $56
Prm_ReadConfigByte(board, 3) == $78
```

That is: **each aligned config longword is presented as a big-endian
image of its decoded value.** `Prm_ReadConfigWord(0)` returns the
*device* ID even though PCI puts the vendor ID at offset 0 — the
era's warts-and-all contract, and period drivers depend on it. Over
the value-carrying `pcibridge` shim this reduces to an offset swap
within the longword:

```
Prm_ReadConfigLong(b, o)  = cfg_read(width 4, o & ~3)
Prm_ReadConfigWord(b, o)  = cfg_read(width 2, (o & ~1) ^ 2)
Prm_ReadConfigByte(b, o)  = cfg_read(width 1, o ^ 3)
```

writes symmetric. On the machine's virtio-net stub (`1af4:1041`) the
falsifiable consequences the probe tool asserts are:

```
Prm_ReadConfigLong(net, 0) == $10411AF4
Prm_ReadConfigWord(net, 0) == $1041   /* device — would be $1AF4 if tidied */
Prm_ReadConfigWord(net, 2) == $1AF4   /* vendor */
Prm_ReadConfigByte(net, 3) == $F4
```

Any wrong-endian or "corrected" implementation fails at least one of
these with a different concrete value, which is the property stage 2's
bar demands.

**Recorded assumption for the Phase 4 rig:** sub-width offsets are
aligned down (`o & ~1` for words) before the swap. The autodoc only
shows aligned examples; what real bridge hardware does with a
misaligned word offset is unverified. Flagged for the hardware
conformance rig rather than guessed silently.

The BAR aperture keeps stage 1's address-invariance at every width:
byte `k` of the aperture is byte `APERTURE_BASE + k` of PCI memory
space, for byte, word and long accesses alike (the stage-2 sized
plumbing preserves the byte lanes, exactly as the real bridge's
hardware swapping does). Drivers swap multi-byte device fields
themselves, as on real Prometheus — the library's config accessors are
the only place the library swaps for you.

## 4. DMA and cache

- `Prm_AllocDMABuffer(size)` returns real host RAM
  (`AllocMem(MEMF_PUBLIC)`, longword-rounded), per §10.1's AmigaPCI
  alignment: this machine bus-masters into ordinary memory, while
  period bounce-buffer drivers — which allocate here and then
  translate — keep working unmodified.
- `Prm_GetPhysicalAddress(addr)`: on this machine **guest physical
  address = PCI bus address** for all RAM, so RAM addresses translate
  identically; an address inside the CPU-visible aperture window
  translates to its PCI address (`APERTURE_BASE + k`).
  `Prm_GetVirtualAddress` is the inverse (PCI addresses inside the
  banked 8 MB region map back to aperture addresses; RAM identity).
  Divergence recorded: real Prometheus returns NULL for addresses
  outside its own space, because its bridge could not bus-master into
  Amiga RAM; this machine's whole point (§10.1) is that it can, so the
  identity mapping is deliberately more permissive.
- **Cache maintenance:** prometheus.library has no cache API; the
  discipline (§10.1) is drivers calling exec's `CacheClearE()`. On
  this machine the interpreter's memory model is coherent and
  `CacheClearE()` is already correct as a no-op; nothing for this
  library to do beyond not inventing an API. The seam where host cache
  operations attach when a Phase 4/5 host needs them is exec's
  vector, not this library.

## 5. Interrupts — INTx onto INT2

Stage 1 reserved card offsets `$1C`–`$27` for exactly this; stage 2
fills them (register detail in `docs/pcibridge-protocol.md` §4/§8):
`INTX_STATUS` (live INTA–D line levels), `INTX_ENABLE` (mask),
`INTX_TEST` (diagnostic line assertion). The card holds INT2 asserted
while `(STATUS & ENABLE) != 0` — level-triggered, shared, matching
§10.1's AmigaPCI contract (INTA–D ORed onto INT2, bridge holds the
line until all requests negate, drivers poll their device).

`Prm_AddIntServer(board, intr)` adds the driver's `struct Interrupt`
to the `INTB_PORTS` server chain (the autodoc's own contract — with
current hardware everything lands on PORTS) and unmasks the board's
routed line in `INTX_ENABLE`, reference-counted per line so removing
one server does not deafen another. Servers follow the shared-chain
discipline `m68k/hostblk-rom` established (Z-flag set on exit unless
exclusively claimed).

Through stage 2, no device fired INTx; `INTX_TEST` existed so the
routing was provable end to end anyway (§6). **Stage 3 has since landed
a real source**: `virtionet.device`'s own ISR narrates a one-shot marker
the first time the virtio-net function's own `INTx` reaches it through
this identical INT2 path (`docs/virtionet.md` §7/§9) — `INTX_TEST`
remains exactly as provable as before, unaffected.

## 6. The probe tool, and what is proven

`m68k/pciprobe/` (`C:PCIProbe` on the patched image) is real 68k code
exercising only the **public** API — the calls a period driver makes —
narrating `PCIPROBE`-prefixed markers over serial (`KPrintF`):

1. Opens `prometheus.library` v2+, walks the bus with
   `Prm_FindBoardTagList`, prints vendor/device/class/slot/function
   per board via `Prm_GetBoardAttrsTagList`.
2. Finds the virtio-net stub by `PRM_Vendor $1AF4, PRM_Device $1041`
   and asserts every §3 byte-order consequence, printing PASS/FAIL
   with the concrete values.
3. Reads `PRM_MemoryAddr0`/`PRM_MemorySize0`, asserts a real assigned
   BAR (non-zero, 16 KiB, PCI address below the Zorro III window,
   printed for cross-checking against host introspection). **Amended
   for stage 3** (`docs/virtionet.md`, `docs/pcibridge-protocol.md`
   §2/§8/§10): now that the virtio-net function's own logic lives
   behind this BAR, memaddr0 itself no longer answers the master-abort
   value — reading it proves only that this one device responds. The
   master-abort check instead targets `memaddr0 + memsize0`, DERIVED
   from the two values this probe just read back (never a hardcoded
   constant, this file's own address rule): the BAR sits at the policy
   region's own base, so one step past its own claimed 16 KiB still
   sits comfortably inside the 8 MB banked aperture and is unclaimed by
   any board. A second, new check follows it: the MAC read back from
   `DEVICE_CFG` (BAR0 + `$3000`) through the aperture, compared against
   the device's own identity (`02:6D:36:4B:00:01`) — positive evidence
   this probe is talking to the actual virtio-net device, not merely a
   board matching the right vendor/device IDs in config space.
4. Installs an interrupt server via `Prm_AddIntServer`, pokes
   `INTX_TEST` on the card (found via its own `FindConfigDev`, the
   one deliberately out-of-band step, labelled as harness), and
   reports the server observing the line via a real INT2 dispatch —
   then deasserts and removes the server.

**Said plainly:** step 4 proves the routing surface host-to-guest end
to end — a line asserted on the card side arrives as a real
level-triggered INT2, through exec's server chain, into a
Prometheus-installed server, which acknowledges by deassertion. Until
stage 3 this was also true of a PCI *device* raising `INTx` from its
own function logic: no device had function logic yet. **Stage 3 closed
that gap** (`docs/virtionet.md`): `virtionet.device`'s own ISR now
narrates a one-shot marker the first time the device's own `INTx`
(not this probe's `INTX_TEST`) reaches it through the identical INT2
path proven here.

The two amended lines, exactly, on a live device (`docs/
pcibridge-protocol.md` §2, `docs/virtionet.md` §2):

```
PCIPROBE aperture read (master-abort): expected $FFFFFFFF got $FFFFFFFF PASS
PCIPROBE bar0 device mac: expected 02:6D:36:4B:00:01 got 02:6D:36:4B:00:01 PASS
```

BAR consistency is checked from both sides: the guest prints the BAR0
PCI address the library assigned; `--inspect`'s host-side pcibridge
report reads the same BAR through the backend and prints what the
device latched; the real-ROM test asserts they agree.

## 7. Verification

All landed 2026-09-15; test names as evidence — the platform norm is
silent failure, so every claim below is positive evidence, not absence
of complaint.

- **Host half** (`docs/pcibridge-protocol.md` §9 carries the full
  list): 12 new unit/bus tests across `pci.rs`/`pcibridge.rs`/`lib.rs`
  prove INTx pin routing, the three registers' masking and level-live
  (never latched) semantics, `irq_pending` gating, the INT2 chain
  end-to-end through the guest-visible register file
  (`pcibridge_intx_test_is_observable_through_the_register_file_and_raises_int2`
  — including the chipset `INTREQ` latch being a separate,
  acknowledge-required layer), and sized aperture delivery measured as
  exactly one backend access of the natural width via an instrumented
  test device. 430 machine-core + 89 machine-hosted tests green, fmt
  and clippy clean.
- **Guest halves**: `scripts/build-prometheus-library.sh` and
  `scripts/build-pciprobe.sh` build both binaries warning-free and
  positively verify hunk magic, the ROMTag word (library), and the
  marker strings. `scripts/patch-pciprobe-hdf.sh` installs
  `Libs/prometheus.library` + `C/PCIProbe` and prepends `C:PCIProbe`
  to the startup-sequence, reading every changed path back
  byte-compared.
- **End to end, real ROM**
  (`kickstart_3_2_2_a1200_pciprobe_proves_the_prometheus_library_api`,
  `crates/machine-hosted/tests/real_rom.rs`): real Kickstart 3.2.2
  boots the patched HDF with `--pcibridge`; the serial log carries the
  library's init chain (`PCIB_VERSION 2 confirmed`, both topology
  functions found, `00:01.0 BAR0 -> PCI $20000000 size $004000`), all
  seven byte-order assertions passing with the §3 concrete values, the
  BAR range/translation cross-checks, the aperture master-abort
  all-ones read, DMA identity, `observed INTA via INT2 (count 1)`, and
  `PCIPROBE result: ALL PASS` with no ` FAIL` line anywhere. The test
  parses the guest's `PCIPROBE bar0:` PCI address and asserts it equal
  to `--inspect`'s host-side read of what the device latched — both
  measured `$20000000` — and inside the policy region. The full
  real-ROM baseline suite (14 tests: boot, introspection, ROMWack over
  script and TCP, boot screen, HD-to-Workbench, RTG desktop, rtgboard
  first light, input scripting, AROS pair) re-ran green alongside it.

## 8. What stage 3 needs, and is now bound by

- The library file is `LIBS:prometheus.library` v3; stage 3's SANA-II
  driver opens it by that name and uses only public calls.
- The stage-2 card protocol is VERSION `2`: INTx registers at
  `$1C`–`$27`, sized aperture accesses delivered at natural width with
  byte lanes preserved (the §5-of-stage-1 prerequisite, landed here).
- BAR policy: 32-bit memory BARs inside `$20000000`–`$207FFFFF`,
  aperture permanently banked there; `PRM_MemoryAddrX` values are
  CPU-usable directly. DMA: bus address = guest physical address;
  `Prm_AllocDMABuffer`/`Prm_GetPhysicalAddress` are the sanctioned
  path.
- Interrupt discipline: shared INT2 servers, poll your device, deassert
  at the device; `INTX_ENABLE` is the library's, not the driver's.
- Delivery: the library and probe reach the boot image by
  post-generation HDF patching (`scripts/patch-pciprobe-hdf.sh`,
  extending `patch-rtgboard-hdf.sh`'s pattern) — amibake is not
  modified, per the standing instruction.

Stage 3 has since landed against exactly this contract, unchanged: see
`docs/virtionet.md`.
