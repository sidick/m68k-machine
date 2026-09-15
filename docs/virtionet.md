# `virtio-net` function and `virtionet.device` driver contract

**Status:** ADR 0005 stage 3, landed 2026-09-15. The host-side virtio-net
*function* (`crates/machine-core/src/pci.rs`'s `VirtioNetStub`, wired
behind the `pcibridge` shim's `00:01.0` slot) now has real logic behind
its BAR — stage 2 left it enumeration-only, all-ones behind every
register. The guest side is a SANA-II driver, `virtionet.device`
(`m68k/virtionet-device/`), and a test tool, `C:VNetTest`
(`m68k/vnettest/`), both real 68k code built against
`LIBS:prometheus.library`'s public API only.

**Context:** `docs/adr-0005-networking-virtio-behind-pci-library.md`
(the decision and staging this executes — sequencing item 3);
`docs/pcibridge-protocol.md` (the shim this function's BAR0 lives
behind, and §2/§8/§10's own record of what stage 2 left absent);
`docs/pci-library.md` (the Prometheus-compatible library every access
below goes through, and §6's probe tool, amended this stage — see §9
below); `docs/device-ledger.md` (this device's row).

---

## 1. Identity

Behind `pcibridge`'s `00:01.0` slot (vendor `1af4`, device `1041`,
revision 1 — the virtio 1.x floor for a non-transitional device, class
`020000`): the same config-space-complete stub stage 2 built, now with
its BAR0 registers live rather than answering all-ones.

## 2. BAR0 map

16 KiB, four capability windows tiling it exactly (module docs on
`VirtioNetStub`, `crates/machine-core/src/pci.rs`):

| BAR0 offset | Region | Behaviour |
|---|---|---|
| `0x0000..0x1000` | `COMMON_CFG` | feature negotiation, `device_status`, per-queue registers — `struct virtio_pci_common_cfg`, virtio 1.x §4.1.4.3, hand-encoded field by field |
| `0x1000..0x2000` | `NOTIFY` | write-only, content ignored, any in-range offset accepted; reads `0` |
| `0x2000..0x3000` | `ISR` | byte `0` is the read-to-clear ISR status; everything else reads `0` |
| `0x3000..0x4000` | `DEVICE_CFG` | bytes `0..6` are the MAC (below); everything else reads `0`, writes discarded |

**Notify is a no-op, honestly.** A real virtio device usually treats a
notify write as "go look at the ring now"; this one does not need to,
because `VirtioNetStub::tick` polls both rings (`process_tx` then
`process_rx`) once per `MachineBus::tick`, called every host tick
regardless of whether the guest ever notifies at all. A driver that
writes `NOTIFY` gets exactly what the spec allows it to get — no
acknowledgement, no side effect — and the rings still move, because
polling already covers it. This is recorded here rather than left for a
reader to discover by tracing call graphs: it is a real, deliberate
simplification against real virtio (where a slow/absent notify can
matter for latency), harmless in a machine with no latency budget to
protect yet.

## 3. Feature offer and the reset dance

Offers exactly `VIRTIO_F_VERSION_1` (bit 32) and `VIRTIO_NET_F_MAC` (bit
5) — nothing else, in particular no offload features, no multiqueue, no
control queue. `device_status` writes implement the spec's reset
semantics (`0` resets everything, including every queue back to
disabled/max-size) and a fail-closed negotiation guard: setting
`FEATURES_OK` without having acked `VIRTIO_F_VERSION_1` gets
`FEATURES_OK` silently cleared right back out (spec 3.1.1's own
"re-read and check" contract — the driver discovers the refusal by
reading the register back), and setting `DRIVER_OK` while `FEATURES_OK`
is not set gets `DRIVER_OK` stripped and `DEVICE_NEEDS_RESET` set
instead. This platform's house style is silent failure elsewhere; here a
stuck, narratable status register beats a device that quietly pretends
to be running.

## 4. MAC

`02:6d:36:4b:00:01` — locally administered (bit 1 of the first octet
set), fixed, read from `DEVICE_CFG` bytes `0..6`. `VIRTIO_NET_F_MAC`
tells the driver to use it rather than generate one. On the SANA-II
side this is the **factory** address; the driver keeps the standard
current-vs-factory split (sana2loop's `sana2-notes.md`, its
"`S2_CONFIGINTERFACE` M3" section — §7's provenance paragraph): the
first `S2_CONFIGINTERFACE` adopts the caller's requested
`ios2_SrcAddr` as the *current* address (a second call fails
`S2ERR_BAD_STATE`/`S2WERR_IS_CONFIGURED`), and `S2_GETSTATIONADDRESS`
reports current in `ios2_SrcAddr`, factory in `ios2_DstAddr`.
Transmitted frames carry the current address as source.

## 5. Virtqueues: two, split, bounded, hostile-input hardened

`num_queues` fixed at `2`: queue `0` is rx, queue `1` is tx (virtio-net's
own convention). `QUEUE_SIZE_MAX` is `256`; the negotiated size can be
smaller (`virtionet.device` asks for `8` — §8). Every guest-controlled
ring field — descriptor/avail/used addresses, lengths, `next` indices,
ring positions — is walked with checked arithmetic and validated against
the negotiated queue size before use. Anything that fails validation
(an out-of-range descriptor id, an indirect-descriptor flag this device
never supports, a chain whose `next` fields cycle back on themselves)
aborts just that poll and sets `DEVICE_NEEDS_RESET` rather than
panicking, indexing out of bounds, or wrapping into a false match — this
is the same fail-closed posture §3's reset dance uses, applied to
traffic instead of negotiation. A descriptor chain is bounded to at most
`size` links, so a cyclic chain cannot loop forever.

Frames are staged through fixed-size scratch buffers
(`MAX_FRAME_TOTAL`/`MAX_ETH_PAYLOAD`, 1514-byte Ethernet payload cap)
rather than anything allocated — `machine-core` has no allocator.

## 6. The `NetBackend` seam, and which backend is wired up

`crates/machine-core/src/pci.rs` defines the seam a real host network
path would sit behind:

```rust
pub trait NetBackend {
    fn transmit(&mut self, frame: &[u8]);
    fn poll_receive(&mut self, buf: &mut [u8]) -> Option<usize>;
}
```

`transmit` is handed the guest's frame with the 12-byte virtio-net
header already stripped off; `poll_receive` is asked for a
header-less frame to deliver, called only once the guest already has an
available rx descriptor (module docs on the trait's own methods). Two
directions, no request/response pairing — deliberately not
`crate::pktport::PacketBackend`'s shape (that trait is a synchronous
descriptor per doorbell; a NIC's traffic has no such pairing at all).

**`NullNetBackend`** (`machine-core`) transmits nothing and never has a
frame to deliver — handy for any test that only cares about
config-space/BAR/queue-negotiation behaviour.

**`HarnessNetBackend`** (`crates/machine-hosted/src/netharness.rs`) is
what `run.rs` actually wires up behind `--pcibridge`, unconditionally —
there is no separate flag choosing it over `NullNetBackend`, because
there is no real host network path to choose *instead* of it yet. It:

- records every transmitted frame, verbatim and in order, reported
  through `--inspect` (index, length, full hex bytes — see §9's real-ROM
  test, which asserts the exact bytes);
- after the first transmitted frame, makes a small, bounded number of
  attempts, spaced well apart, to deliver one fixed echo-reply frame:
  dst = the device's own MAC, src `02:6d:36:4b:00:fe`, ethertype
  `0x88b5`, payload `"M68KVNET-RX-REPLY-0001"`.

**Why bounded, spaced attempts, not one delivery.**
`VirtioNetStub::tick` polls tx then rx every host tick, so the very tick
that notices a transmitted frame can already have an rx descriptor ready
to deliver into — well before the guest task resumes from its blocking
`DoIO` on that transmit, let alone posts the `CMD_READ` that would
actually consume the reply (`virtionet.device`'s pending-read match is a
software-side FIFO the device itself never observes). A delivery into
that window is received with no pending reader, and is silently dropped
by the driver's own rx-completion path. Two earlier drafts of this
backend got this wrong in opposite directions, both confirmed
empirically against the real-ROM test, not by inspection:

- **Deliver once, immediately.** Dropped every time, for exactly the
  reason above.
- **Deliver once, after a single bounded delay** — the "long enough
  that a real guest has certainly gotten there" idiom `pciprobe.c`'s
  `INTX_POLL_LIMIT`/`vnettest.c`'s `READ_POLL_LIMIT` already use,
  counted in `poll_receive` calls. Still dropped, at delays from a few
  dozen calls up to several thousand: `virtionet_device.c`'s ISR reads
  the `ISR` byte once, at entry, and does not return to the guest's own
  task level until it has drained everything the used ring already
  holds — `process_tx`/`process_rx` keep running every tick regardless
  of whether the CPU is currently inside that very handler, so a
  delayed delivery lands *during* the same still-running interrupt
  servicing about as often as it lands after the guest actually returns
  to task level and posts `CMD_READ`. No fixed delay-then-single-shot
  reliably tells those two apart from the host side.
- **Offer the same frame on every subsequent poll**, reasoning
  repetition would eventually land in the right window. Confirmed
  actively worse: the driver's drop path reposts the buffer straight
  back onto the guest's avail ring before returning, so an always-armed
  backend refills the very ring that just fed it, tick after tick,
  forever — a genuine livelock, confirmed by instrumented counters (over
  20,000 back-to-back deliver-and-drop cycles with no sign of ever
  stopping, before being killed).

The design that landed makes one delivery attempt, then goes completely
quiet (`poll_receive` returns `None`, nothing offered at all) for a long
cooldown before trying again, for at most a handful of attempts total.
Each attempt that lands in the no-reader window costs the driver exactly
one drop-and-repost — bounded, not self-perpetuating, because nothing
new is offered again until the next attempt's own cooldown elapses. The
cooldown is far longer than any plausible stretch of interrupt
servicing, so by the next attempt the guest is certainly back at task
level; spreading a handful of attempts across a wide span of ticks makes
this self-correcting against however long that actually takes, without
the unbounded refill that broke the "keep offering" draft.

**A genuine guest-side bug this chase surfaced.** With every backend
design above, `C:VNetTest`'s `CMD_READ` step reported a spurious `PASS`
(`ethertype $0000`, garbage payload, `io_Error 0`) even with the backend
delivering *nothing at all* — proving the fault had nothing to do with
timing. Cause: `vnettest.c` reuses one `IOSana2Req` across every SANA-II
command in sequence; every prior command in this test goes through
`DoIO` (`SendIO`+`WaitIO`), and real `exec.library`'s `WaitIO` removes
the replied message from its port but does not reset
`io_Message.mn_Node.ln_Type` off `NT_REPLYMSG` — `SendIO` does not reset
it either (it is a thin wrapper straight onto `BeginIO`). The `CMD_READ`
step is the first to call `SendIO` and then poll `CheckIO` directly
(observational by design, no `WaitIO` — this tool's own file-top
comment), so the very first `CheckIO` saw the *previous* command's
`NT_REPLYMSG` and reported "done" before `virtionet.device`'s `BeginIO`
ever ran. Fixed in `vnettest.c` by resetting
`io->ios2_Req.io_Message.mn_Node.ln_Type = NT_MESSAGE` immediately
before that one `SendIO` call — confirmed by instrumented builds (the
break happened at poll iteration `0`, `ln_Type == NT_REPLYMSG`,
`io_Flags == 0`, i.e. not even `IOF_QUICK`) before the fix, and a correct
match (real ethertype and payload) after it.

**A real tap/socket backend is deliberately deferred.** This increment's
brief was host-side virtqueue processing, the seam trait, and a
first-packet proof — not a host network path. `HarnessNetBackend` proves
the whole chain end to end without machine-hosted growing that
capability yet; a later increment can implement `NetBackend` over a real
host interface without touching `machine-core` or `VirtioNetStub` at
all.

## 7. `virtionet.device`: public-API-only discipline

Same shape as every other card driver here (`prometheus_library.c`,
`rtgboard_card.c`): standard Exec AUTOINIT disk device, disk-loaded
(`Devs:virtionet.device`), `struct Resident`/`InitTab`/`FuncTab`,
`SysBase` defined not merely declared (`-nostdlib`, no startup code).
Talks to the virtio-net function only through `prometheus.library`'s
public API (`Prm_FindBoardTagList`, `Prm_GetBoardAttrsTagList`,
`Prm_AddIntServer`, `Prm_AllocDMABuffer`/`Prm_GetPhysicalAddress`) plus
direct reads/writes through the BAR0 aperture it was handed — no raw
register poking of the pcibridge card itself, unlike `pciprobe.c`'s one
deliberately out-of-band harness step.

**`sana2.h` provenance.** This project's local NDK 3.2 reference bundle
carries no `devices/sana2.h` (SANA-II is specified by Commodore's "Amiga
Guide to Network Programming", a document distinct from the RKRM/
Autodocs set this project has locally). The header's own values and
structure layouts were verified against AROS's
`compiler/include/devices/sana2.h`, used strictly as a value oracle for
the published SANA-II standard's own constants and layouts — facts
about a public interface, not AROS's expression of them; no text or
comments from that file are copied or translated. Same licensing posture
`docs/pci-library.md` §1 documents for the Matay Prometheus SDK. A
second, higher-trust source joined during review: **sana2loop** (the
project owner's own hardware-free SANA-II `loopback.device`, BSD
2-Clause — freely copyable here, the amirfb precedent), whose
`docs/sana2-notes.md` and `loopback_device.c` drove this driver's
conformance corrections: `S2_DEVICEQUERY` filled via `ios2_StatData`,
the copy hooks' spec-fixed register calling convention
(`a0`/`a1`/`d0`, `BOOL` return), `Open()` replacing
`ios2_BufferManagement` with the driver's per-open cookie, hook-failure
posture `S2ERR_NO_RESOURCES`/`S2WERR_BUFF_ERROR`, the
current-vs-factory station-address split (§4), and zero-filling bytes
`6..15` of every 16-byte address array. `sana2.h`'s own header comment
carries the full provenance note.

**Implemented:** `CMD_READ`, `CMD_WRITE`, `CMD_FLUSH`,
`S2_DEVICEQUERY`, `S2_GETSTATIONADDRESS`, `S2_CONFIGINTERFACE`,
`S2_ONLINE`, `S2_OFFLINE`, `S2_BROADCAST` (accepted as a no-op —
broadcast is an ordinary `CMD_WRITE` addressed `FF:FF:FF:FF:FF:FF`, no
special per-frame handling needed).

**Scoped down, honestly, not silently** (`virtionet_device.c`'s own
file-top comment carries this list; mirrored here so it is not only
discoverable by reading 68k C):

- One opener. A second `Open` of unit 0 is refused with
  `IOERR_UNITBUSY`.
- `S2_ADD/DELMULTICASTADDRESS[ES]`: `S2ERR_NOT_SUPPORTED`.
- `S2_GETGLOBALSTATS`: minimal — fills the handful of counters this
  driver actually keeps (packets in/out, rx drops as
  `UnknownTypesReceived`), everything else honestly zeroed, never a
  hard failure. `S2_GETSPECIALSTATS`/`S2_GETTYPESTATS`:
  `S2ERR_NOT_SUPPORTED` — their `StatData` structs are a different
  shape; faking success against the wrong shape would be worse than
  refusing.
- `S2_ONEVENT`: `S2ERR_NOT_SUPPORTED` (no event tracking at all).
- `S2_TRACKTYPE`/`S2_UNTRACKTYPE`/`S2_READORPHAN`/`S2_MULTICAST`:
  `S2ERR_NOT_SUPPORTED`.
- `CMD_READ` matching: one pending-read FIFO, matched against the
  incoming frame's ethertype only at the head of the queue — not the
  full SANA-II multi-listener fan-out across differently-typed pending
  reads. Sufficient for `VNetTest`'s single in-flight read; not a
  general protocol stack.

**The one-shot ISR marker.** `vnet_isr_handler` prints `"VNETDEV: isr:
first queue interrupt observed (device INTx via INT2)"` exactly once —
positive evidence the *device's own* function logic raised `INTx`
through the real INT2 chain, not stage 2's `INTX_TEST` diagnostic poke
(`docs/pci-library.md` §6, `docs/pcibridge-protocol.md` §8's own record
of what was unproven before this stage). The ISR read-to-clear
(`rd_u8(base->IsrCfg)`) is also what deasserts the level-triggered INTx
line at the device; `INTX_ENABLE` stays `prometheus.library`'s to manage
(`docs/pci-library.md` §5), never touched here.

## 8. `C:VNetTest` and delivery

`m68k/vnettest/vnettest.c`: real 68k CLI tool, same startup shape as
`m68k/pciprobe/pciprobe.c`. Exercises only the public SANA-II API a real
network stack would use — `OpenDevice`, `S2_GETSTATIONADDRESS`,
`S2_CONFIGINTERFACE`, `S2_ONLINE`, one `CMD_WRITE` of a deterministic
recognisable frame (dst broadcast, ethertype `0x88b5`, payload
`"M68KVNET-TX-0001"`), then one posted `CMD_READ` (wildcard
`PacketType 0`), narrated over serial with `"VNETTEST "`-prefixed
markers. The `CMD_READ` step is observational by design — a bounded poll
via `CheckIO`, not a hard requirement — because an earlier increment of
this tool had no peer on the wire at all to guarantee a reply; a timeout
there is narrated, not treated as this tool's own `FAIL`. §9's real-ROM
test requires the `PASS` line explicitly, on top of `VNetTest`'s own
lenience, precisely because the harness backend (§6) now supplies that
peer.

`scripts/patch-virtionet-hdf.sh` delivers both onto a bootable image the
same way `scripts/patch-pciprobe-hdf.sh` delivers stage 2's tool: copies
the source HDF, installs `Libs/prometheus.library` (only if not already
present — checked and installed as a dependency, mirroring the
pciprobe script exactly), `Devs/virtionet.device`, `C/VNetTest`, and
prepends `C:VNetTest` to `S/Startup-Sequence` so a plain unattended boot
produces the driver's and tool's serial evidence. Every changed path is
read back and byte-compared, not merely assumed written.

A second, separately patched image, `scripts/patch-sanaconform-hdf.sh` →
`nondistribution/m68k-machine-sanaconform.hdf`, installs the same
`Libs/prometheus.library` + `Devs/virtionet.device` pair alongside
**`C:SanaConform`** — a prebuilt m68k Shell tool from `~/src/sana2loop`
(the project owner's own hardware-free SANA-II `loopback.device`
project, BSD 2-Clause — freely copyable/redistributable here, the same
provenance posture §7 above already records for this driver's own
`sana2.h`) — and prepends `C:SanaConform 0 DEVICE virtionet.device CONFIG
ONLINE >SYS:sanaconform.log` to `S/Startup-Sequence` instead of
`VNetTest`. Deliberately not combined onto one image: `SanaConform`'s own
self-echo probe transmits frames on its own account, and keeping it off
the first-packet fixture preserves that test's "exactly one frame"
assertion; `virtionet.device` is itself single-opener, so `VNetTest` and
`SanaConform` could never usefully share one boot's Open anyway. §9
below records what this gate finds.

## 9. Verification

All landed 2026-09-15; test names as evidence — the platform norm is
silent failure, so every claim below is positive evidence, not absence
of complaint.

- **Guest halves build clean:** `scripts/build-virtionet-device.sh` and
  `scripts/build-vnettest.sh` produce `virtionet.device`/`VNetTest`
  warning-free; `scripts/patch-virtionet-hdf.sh` installs both (plus
  `prometheus.library` as a checked dependency) onto
  `m68k-machine-virtionet.hdf` and reads every changed path back
  byte-identical.
- **Stage 2 regression, re-proven with a live device behind BAR0:**
  `kickstart_3_2_2_a1200_pciprobe_proves_the_prometheus_library_api`
  (`crates/machine-hosted/tests/real_rom.rs`) still passes with two
  lines changed to match a live function existing behind BAR0 now
  (`docs/pci-library.md` §6 records the exact wording): the
  master-abort aperture read now targets `memaddr0 + memsize0` (derived
  from the probed BAR values, never a hardcoded constant — this
  platform's address rule), and a new check,
  `"PCIPROBE bar0 device mac: expected ... got ... PASS"`, reads the MAC
  through the aperture at `BAR0+0x3000` and confirms it matches the
  device's own identity.
- **First-packet round trip, end to end, real ROM:**
  `kickstart_3_2_2_a1200_virtionet_first_packet_round_trip`
  (`crates/machine-hosted/tests/real_rom.rs`) boots real Kickstart
  3.2.2 with the `virtionet`-patched HDF and `--pcibridge`, and asserts,
  as positive evidence: the driver's full `DevInit` chain (capabilities
  found, features negotiated, both queues set up, MAC confirmed,
  `DRIVER_OK` set); the host's own `--inspect` report of exactly one
  transmitted frame, with its full 60-byte content asserted byte for
  byte (dst broadcast, src the device's MAC, ethertype `0x88b5`,
  `"M68KVNET-TX-0001"`, zero-padded to the Ethernet minimum by the
  driver itself); the one-shot device-INTx ISR marker; `VNetTest`'s own
  `"cmd_read: PASS"` line carrying the harness's reply payload
  (`"M68KVNET-RX-REPLY-0001"`); and the tool's `"result: ALL PASS"` with
  no ` FAIL` line anywhere in the serial log.
- **SANA-II conformance gate, `SanaConform` against `virtionet.device`:**
  `kickstart_3_2_2_a1200_sanaconform_gates_virtionet_device`
  (`crates/machine-hosted/tests/real_rom.rs`) boots real Kickstart 3.2.2
  with the `sanaconform`-patched HDF (§8) and `--pcibridge`, copying the
  fixture to a temp path first (the guest writes `SYS:sanaconform.log`,
  so the checked-in fixture is never mutated) and reading that log back
  with `xdftool` after the run. Read `~/src/sana2loop/src/tools/
  sanaconform.c` before touching this test: every probe it runs against a
  real device is bounded — every command other than the self-echo round
  trip's `CMD_WRITE`/`CMD_READ` is a single synchronous `DoIO()`, and
  `virtionet_device.c`'s own `VNetBeginIO` completes every one of those
  immediately (`TermIO()` inline); the self-echo round trip's own wait is
  an explicit bounded poll (`CheckIO()` in a 50-iteration `Delay(1)`
  loop, ~1 second ceiling, then `AbortIO()` — never a raw blocking
  `WaitIO()`). One tool-side quirk found by reading the source: the
  self-echo step's second (write-partner) `OpenDevice()` call is
  hardcoded to `"loopback.device"`, not the `DEVICE` argument
  `SanaConform` was actually invoked with — harmless here (this image
  carries no `loopback.device` at all, so that open always fails fast and
  gracefully) and moot regardless, since `virtionet.device` is itself
  single-opener (`IOERR_UNITBUSY` on a second `Open`, §7 above) and the
  round trip was never going to complete against it either way.

  The test asserts, as positive evidence read back from
  `SYS:sanaconform.log` (no assertion passes on absence alone): the probe
  header naming `virtionet.device` unit 0; `OpenDevice: OK`; `CONFIG:
  configured with the driver's own factory address`; `S2_ONLINE: OK`;
  `S2_DEVICEQUERY: MTU=1512 BPS=1000000000 HardwareType=1` — **1512, not
  the conventional 1500, is this driver's honest MTU claim**
  (`VNET_FRAME_MAX` 1526 minus the 14-byte Ethernet header); the Rev 4
  `RawMTU` line reporting it unsupported (`SizeSupplied=30 bytes,
  pre-Rev-4 driver` — this driver's own `struct Sana2DeviceQuery` carries
  no `RawMTU` field at all); the station address
  `02:6d:36:4b:00:01` (factory == current at a fresh `Open`);
  `S2_GETPEERADDRESS`/`S2_GETDNSADDRESS` both reported `not implemented
  (pre-Rev-4 driver)` (unrecognised command numbers, confirmed against
  `virtionet_device.c`'s `BeginIO` switch falling through to `IOERR_NOCMD`);
  and the self-echo round trip's graceful `skipped (couldn't open a
  second handle on unit 0)` line, for the two independent reasons above.
  `S2_GETSPECIALSTATS` (`S2ERR_NOT_SUPPORTED` on this driver) prints
  nothing on failure in `sanaconform.c`, so there is no corresponding log
  line to pin. Re-running
  `kickstart_3_2_2_a1200_virtionet_first_packet_round_trip` and
  `kickstart_3_2_2_a1200_pciprobe_proves_the_prometheus_library_api`
  alongside this new test confirms no interference between the three
  fixtures.
