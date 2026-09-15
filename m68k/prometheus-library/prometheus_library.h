/*
 * prometheus_library.h -- internal (non-public) structures for
 * prometheus.library: the library base and the per-function board
 * record that PCIBoard* opaquely points to.
 *
 * This is NOT part of the fixed seam (pcibridge_card.h, the public
 * include tree): those are the caller-visible/hardware contract and
 * must not change here. This header only exists to share the
 * library's own private layout between prometheus_library.c and
 * nothing else (no caller ever includes it, exactly as
 * rtgboard_card.h's register/struct split is private to
 * rtgboard_card.c's own translation unit, though that one happens to
 * also carry public register offsets -- this one carries none of
 * those, they live in pcibridge_card.h instead).
 */

#ifndef PROMETHEUS_LIBRARY_H
#define PROMETHEUS_LIBRARY_H

#include <exec/types.h>
#include <exec/libraries.h>
#include <exec/execbase.h>

/* Fixed cap on recorded PCI functions (docs/pci-library.md section 2 /
 * this increment's brief): allocated once, up front, in LibInit. Bus 0
 * only, 32 devices * up to 8 functions could in principle exceed this,
 * but every real topology this machine presents (the virtual root
 * bridge plus one virtio-net stub, docs/pcibridge-protocol.md section
 * 2) is nowhere near it -- the cap exists so enumeration can never run
 * away, not because it is expected to bind. */
#define PRM_MAX_BOARDS 32

/* One recorded PCI function -- the opaque object behind every PCIBoard*
 * this library hands to callers. Never exposed by definition to
 * drivers: they only ever see PCIBoard* (typedef VOID in
 * libraries/prometheus.h), exactly like the real SDK. */
struct PciFunction {
    UBYTE Bus;
    UBYTE Dev;
    UBYTE Fn;
    UBYTE IntPin;        /* 0 = none, 1-4 = INTA-D (config offset 0x3D) */

    UWORD Vendor;
    UWORD Device;
    UBYTE Revision;
    UBYTE Class;
    UBYTE SubClass;

    /* BAR assignment (docs/pci-library.md section 2): PCI bus address
     * and byte size for each of the six possible 32-bit memory BARs.
     * 0 in BarPci means "unassigned" -- I/O BARs, 64-bit BARs, ROM
     * BARs and BARs that would not fit are all recorded this way, and
     * PCIB_PCI_MEM_BASE is never 0, so this is an unambiguous
     * sentinel. BarSize is likewise 0 whenever BarPci is 0, matching
     * Prm_GetBoardAttrsTagList's "0 if unassigned" contract exactly. */
    ULONG BarPci[6];
    ULONG BarSize[6];

    /* PRM_BoardOwner (V2 tag): NULL until some driver claims the
     * board via Prm_SetBoardAttrsTagList. */
    APTR Owner;
};

/* The library base -- this library's analogue of rtgboard_card.h's
 * struct CardBase, sized via InitTab[0] and zeroed by exec before
 * LibInit runs (so every field below not explicitly set by LibInit
 * starts at a well-defined 0/NULL). */
struct PrometheusBase {
    struct Library LibBase;

    struct ExecBase *ExecBase;
    APTR SegList;               /* APTR, not BPTR -- see LibInit's comment */

    UBYTE *RegisterBase;        /* pcibridge board's cd_BoardAddr */
    APTR ApertureCpuBase;       /* RegisterBase + PCIB_APERTURE_OFFSET */

    struct PciFunction *Boards; /* AllocMem'd once, PRM_MAX_BOARDS entries */
    ULONG BoardCount;           /* functions actually recorded, <= PRM_MAX_BOARDS */

    /* Per-INTx-line (A-D, index 0-3) reference count (docs/pci-
     * library.md section 5): incremented/decremented under Disable()
     * alongside the PCIB_INTX_ENABLE bit it gates, so removing one
     * server's interest in a shared line never deafens another. */
    ULONG IntxRefCount[4];
};

#endif /* PROMETHEUS_LIBRARY_H */
