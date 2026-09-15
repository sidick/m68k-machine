# ADR 0005 — Networking is virtio-net behind a real `pci.library`

**Status:** accepted, 2026-09-15. Drafted and decided before the
networking work starts, because every other device family has diverged
from the proposal's paravirtual plan by now and networking should
either diverge for the same reasons or record why it does not.

**Context:** proposal §10.1–§10.2 (the `pci.library` and virtio-as-
first-bus plan this ADR re-examines); `docs/device-ledger.md` (the
native-first rule that postdates that plan); ADR 0003 (storage's
divergence from the same plan); ADR 0002 (display's); roadmap Phases
3–4 (the Prometheus conformance rig this decision feeds).

---

## The question

Phase 3's exit requires network under QEMU on both harnesses. Proposal
§10.2 says how: virtio-net presented as virtio-pci behind a Zorro III
shim, driven by a SANA-II driver written against a Prometheus/OpenPCI-
compatible `pci.library` (§10.1).

But that plan predates the device ledger, and the ledger's native-first
rule has since overridden the proposal twice in a row. Storage was
proposed as MIRAGE-over-the-shim and became `hostblk`, a native
doorbell card (ADR 0003). Display was proposed as virtio-gpu/ramfb and
became the native `rtgboard` (ADR 0002). Both times the reasoning was
the same: the paravirtual or emulated interface imported constraints
and complexity from hardware this machine does not have, and a device
designed for what the machine actually is was simpler and faster.

So the honest question is: does networking follow the same arc — a
native doorbell NIC card, a `hostnet` beside `hostblk` — or does the
proposal's virtio-behind-`pci.library` plan survive the scrutiny the
other two did not?

## Why networking is different in kind

For storage and display, the paravirtual bus was scaffolding: nothing
above it cared how blocks or pixels arrived, so the simplest transport
designed for this machine won. Applying that template here would say:
skip PCI entirely, build a doorbell NIC card, write a SANA-II driver
for it, done — probably less total work than a `pci.library` plus a
virtio driver.

The template misses that **`pci.library` is not scaffolding. It is a
stated goal of the project in its own right.** Proposal goal 4: provide
a Prometheus/OpenPCI-compatible `pci.library` that lets m68k drivers
own real PCIe devices — usable equally on real Amigas with PCI
bridges. Phase 4's x86 rig (period PCI cards behind a PCIe-to-PCI
bridge, validated against shipped P96 `.card`s and era drivers) is the
conformance test for it. The AmigaPCI alignment (§10.1) commits to one
driver corpus serving this machine, real Prometheus systems, and that
open-hardware machine. None of that is deliverable by a native NIC
card; all of it is deliverable through exactly the path networking
takes here.

Put the other way round: `hostblk` and `rtgboard` replaced devices
nothing else would ever need. A `hostnet` card would *displace the
machine's only planned consumer-before-Phase-4 of the bus library the
project has promised to build*. The first virtio driver is not an end
in itself — it is how `pci.library`'s endian, DMA-address, cache-
maintenance and interrupt-routing seams get exercised by real m68k
code before any physical silicon is attached (§10.2's own argument,
which still holds).

## Decision

**Networking follows the proposal: a Prometheus/OpenPCI-compatible
`pci.library` over each harness's ECAM, and a SANA-II driver for
virtio-net written against it.** The native-first rule is satisfied,
not overridden: under the ledger's own definitions this *is* the
native path, because PCI is how this machine's real hardware attaches
— on QEMU today, under KVM in Phase 4, and via VFIO or bare-metal PCIe
in Phase 5. Virtio-net is not an emulation of legacy silicon standing
in for something better; it is the first device on the machine's
permanent bus.

The ledger gains rows for both pieces in the change that introduces
them: `pci.library`'s ECAM/shim surface as **permanent** (it becomes
more load-bearing as the machine grows, like AUTOCONFIG), and
virtio-net as **permanent** on the QEMU/KVM tier (under KVM it is the
production NIC, not a stand-in; on bare metal Phase 5 decides between
VFIO-style passthrough and native drivers, which is ADR 0001's
question, not this one's).

Sequencing inside the increment, so the risk lands in one place at a
time:

1. **ECAM reachability first, host side.** The Zorro III shim that
   exposes the harness's PCI config/BAR space to the guest, with
   host-side tests proving enumeration against QEMU's real topology —
   before any 68k code exists, the same "register interface first,
   driver later" shape every card here has taken.
   *Done, 2026-09-15* — `pcibridge` (`docs/pcibridge-protocol.md`), the
   `PciBackend` seam, and enumeration/BAR-sizing/coexistence proven
   through the real bus, with real Kickstart 3.2.2 adopting the board
   (its own `ConfigDev`, verified by introspection). One honest scope
   note against this line's own wording: the enumeration tests run
   against the host-side *virtual* topology (a QEMU-root-shaped host
   bridge plus a config-space-complete virtio-net), because
   `machine-hosted` has no real PCI bus and the QEMU board crates do
   not yet have devices at all; "against QEMU's real topology" becomes
   true when a board crate implements `PciBackend` over its real ECAM —
   the trait documents that mapping, and nothing in stage 2 waits on it.
2. **`pci.library` second, conformance-shaped from day one.** The
   Prometheus API against that shim: config accessors with the
   *actual* byte-swap semantics (not a tidied reinterpretation),
   `GetDMAAddress()`, `CacheClearE()`-mapped maintenance, INTx→INT2
   routing. Its test is period-shaped: the API surface real drivers
   call, exercised by a guest-side probe tool, not only by our own
   driver.
3. **virtio-net + SANA-II driver last**, as the library's first real
   client — modern virtio (not legacy), MSI-X deferred in favour of
   INTx-style routing per §10.1's driver discipline.

## Alternatives rejected

- **A native doorbell NIC (`hostnet`).** Less total work to first
  packet, and the right template everywhere else. Rejected because it
  strands `pci.library` with no consumer until Phase 4, deferring
  exactly the endian/DMA/cache lessons §10.2 exists to surface early —
  and because a SANA-II driver for it serves precisely one machine,
  where the same effort against `pci.library` feeds the shared driver
  corpus. Revisit only if `pci.library` + virtio proves an order of
  magnitude harder than estimated; if a quick interim network is ever
  genuinely needed before then, it would be a bring-up row in the
  ledger with virtio-net as its named successor, per the ledger's own
  rules.
- **SANA-II over virtio-net without a real `pci.library`** (drive the
  virtio rings from a device-specific driver against a hardcoded
  window). Faster to first packet than the full plan while keeping
  virtio, but it violates the address rule (a hardcoded BAR window is
  exactly the constant the ledger forbids), and every line of ring
  handling would be rewritten once the library exists. The library is
  where the reuse lives; skipping it buys nothing durable.
- **Emulating a period NIC (RTL8139/NE2000) for existing Amiga
  drivers.** The Cirrus move again, after the ledger was written to
  stop it: thousands of lines modelling 1990s silicon so a shipped
  driver binary works, on the exact path where the project has
  committed to native drivers and a real bus library. The zero-install
  argument that justified keeping Cirrus does not apply — there is no
  networking equivalent of "boots to a desktop with nothing
  installed"; a driver must be installed either way, so it may as well
  be ours.

## What this costs

- **The most front-loaded increment yet.** Storage's first increment
  was one card; this one is a bus shim, a bus library, and a driver
  before the first packet moves. The sequencing above exists to make
  each stage independently verifiable, and the virtio ring/queue
  formats are well-documented (the OASIS spec) with AROS's own virtio
  drivers available as an independent reference (APL — read for
  understanding, treat like the GPL oracles for copying purposes;
  verify licence before any translation).
- **Prometheus fidelity is a real research cost.** The byte-swap
  semantics must match the real bridge, and the only cheap conformance
  check before Phase 4's hardware rig is period driver source and
  documentation. Getting this wrong quietly is the black-screen class
  of bug in network form.
- **`devicetree.resource` arrives early.** ECAM base discovery pulls a
  slice of Phase 3's devicetree work forward on virt; q35's ACPI→FDT
  synthesis can be stubbed to a fixed table initially, but the seam
  has to exist.

## What it does not cost

- **The doorbell pattern is not abandoned** — it remains the right
  shape for host-service cards (`pktport` proves it), and nothing here
  retires it. The split is: host services get doorbell cards, real
  buses get `pci.library`.
- **No throughput sacrifice that matters.** virtio-net's ring protocol
  is a DMA design; the SANA-II layer above it is the same either way.
  The performance ceiling ADR 0004 chased for storage has no network
  analogue at Phase 3's exit bar ("network works unattended"), and
  ADR 0004's measured lesson — the guest-executed layer dominates —
  applies to the filesystem, not to a NIC driver moving frames.
- **AROS support is not extra work later.** AROS's PCI HIDD wrapper
  over prometheus.library (§10.1) means the same library serves both
  OSes; deciding the HIDD glue's timing belongs to the Phase 4 AROS
  gate.

## Consequences

- Phase 3's remaining exit path is fixed: `pci.library` + virtio-net
  SANA-II, then the unattended-boot CI gate with network up.
- The Phase 4 x86 conformance rig stops being aspirational — the
  library it validates now has a concrete first client and test
  surface.
- virtio-input and virtio-gpu, which §10.2 grouped with virtio-net,
  are **not** pulled along by this decision: native `input` and
  `rtgboard` already won those seats (ADR 0002, and the ledger's input
  entry). The virtio family on this machine is whatever `pci.library`
  needs clients for, not a default.
- If Phase 4's measurements or the Prometheus rig falsify the fidelity
  assumptions (e.g. the byte-swap contract cannot be honoured without
  breaking virtio), that is grounds to reopen this ADR, not to bend
  the library silently.
