/*
 * virtionet_device.h -- internal layout for virtionet.device: the driver's
 * device-base, per-queue ring bookkeeping, and unit structure.
 */

#ifndef VIRTIONET_DEVICE_H
#define VIRTIONET_DEVICE_H

#include <exec/types.h>
#include <exec/devices.h>
#include <exec/lists.h>
#include <exec/interrupts.h>

#include <libraries/prometheus.h>

#include "sana2.h"

/* Own queue-depth cap (docs brief: "read queue_size, take min with your
 * own cap"). Modest on purpose: this driver's whole job at this stage is
 * to prove the protocol end to end, not to sustain wire-speed throughput
 * -- 8 descriptors each on rx and tx is ample for VNetTest's one-frame
 * exchange and leaves headroom for a handful of pipelined reads/writes. */
#define VNET_QUEUE_CAP 8

/* Ethernet frame + virtio-net header sizing (task brief): "each rx
 * buffer must hold a full frame + header (1526+12, round up)". 1526
 * covers a full untagged Ethernet frame with generous slack (14-byte
 * header + 1500 MTU + a little margin, this project's own choice, not a
 * spec-mandated figure); rounded up to a 4-byte multiple purely so the
 * DMA buffer size itself is longword-friendly (Prm_AllocDMABuffer's own
 * granularity, docs/pci-library.md section 4) -- there is no other
 * significance to landing on a 4-byte boundary here. */
#define VNET_FRAME_MAX 1526UL
#define VNET_RXBUF_SIZE (((VNET_FRAME_MAX + VNET_HDR_SIZE) + 3UL) & ~3UL)

#define VNET_ETH_HDR_LEN 14 /* dst(6) + src(6) + ethertype(2) */
#define VNET_ETH_MIN_FRAME 60 /* Ethernet minimum frame, excluding FCS */

/* One virtqueue's DMA-backed rings plus this driver's own bookkeeping.
 * bufPtrs[i]/bufPhys[i] is the per-descriptor data buffer (an rx frame
 * buffer for VNET_QUEUE_RX, a free/in-use tx slot for VNET_QUEUE_TX);
 * pending[i] is only used on the tx queue (the IOSana2Req awaiting
 * completion for descriptor i, NULL if the slot is free). */
struct VNetQueue {
    UWORD qsize;

    APTR descRaw;   /* Prm_AllocDMABuffer() return, for Prm_FreeDMABuffer */
    ULONG descRawSize;
    UBYTE *desc;    /* 16-byte aligned */

    APTR availRaw;
    ULONG availRawSize;
    UBYTE *avail;   /* 2-byte aligned */

    APTR usedRaw;
    ULONG usedRawSize;
    UBYTE *used;    /* 4-byte aligned */

    UWORD avail_idx_shadow; /* next avail slot this driver will fill */
    UWORD used_idx_seen;    /* used->idx last processed */

    UWORD notify_off;    /* raw queue_notify_off from the device */
    APTR  notify_addr;   /* notify_base + notify_off * notify_off_multiplier */

    APTR  bufRaw[VNET_QUEUE_CAP];
    ULONG bufRawSize[VNET_QUEUE_CAP];
    UBYTE *bufPtrs[VNET_QUEUE_CAP];
    APTR  bufPhys[VNET_QUEUE_CAP];

    /* tx only: which IOSana2Req (if any) completes when descriptor i is
     * retired by the device; rx descriptors are always immediately
     * refilled and never leave a request pending against them. */
    struct IOSana2Req *pending[VNET_QUEUE_CAP];
    BOOL slotFree[VNET_QUEUE_CAP]; /* tx only: free-list membership */
};

/* This driver's scope, recorded once here rather than scattered across
 * every place it matters (task brief's own instruction to record scope
 * honestly in the source header comment -- see virtionet_device.c's
 * file-top comment for the full list; this is the structural half):
 *   - ONE opener (unit->opencnt > 1 is refused, narrated).
 *   - ONE pending CMD_READ at a time is matched by simple FIFO head-of-
 *     queue ethertype match (ios2_PacketType, or 0 = accept anything) --
 *     not the full SANA-II multi-listener fan-out semantics (multiple
 *     opens/multiple pending reads of different types all seeing a
 *     matching packet). A second pending CMD_READ is still queued and
 *     will be serviced in turn, but a non-matching head-of-queue read
 *     causes the packet to be dropped rather than tried against the next
 *     queued read -- adequate for VNetTest's single in-flight read, not
 *     a general protocol stack's needs.
 *   - No multicast (S2_ADD/DELMULTICASTADDRESS[ES] -> S2ERR_NOT_SUPPORTED).
 *   - S2_GETGLOBALSTATS: minimal -- struct Sana2DeviceStats via
 *     ios2_StatData (the real ABI's field), the handful of counters
 *     this driver keeps, everything else honestly zeroed. S2_GETTYPE/
 *     SPECIALSTATS: S2ERR_NOT_SUPPORTED (different StatData shapes).
 *   - S2_ONEVENT/S2_TRACKTYPE/S2_UNTRACKTYPE/S2_READORPHAN/S2_MULTICAST:
 *     S2ERR_NOT_SUPPORTED.
 */
struct VNetUnit {
    struct Unit unit;
    UWORD opencnt;

    SANA2_COPY_FUNC copyToBuff;
    SANA2_COPY_FUNC copyFromBuff;

    BOOL configured; /* S2_CONFIGINTERFACE done */
    BOOL online;     /* S2_ONLINE done */

    /* Current-vs-factory station address split (~/src/sana2loop's own
     * docs/sana2-notes.md "S2_CONFIGINTERFACE and the current-vs-factory
     * station address split (M3)"): factoryMac is read once from the
     * virtio device's config space at DevInit and never changes;
     * currentMac starts equal to it (reset at every 0->1 Open, mirroring
     * loopback.device's reset_unit()) and is the only one
     * S2_CONFIGINTERFACE can adopt a new value into. S2_GETSTATIONADDRESS
     * reports ios2_SrcAddr = currentMac, ios2_DstAddr = factoryMac -- the
     * two may diverge after configuration, matching the real ABI. */
    UBYTE factoryMac[6];
    UBYTE currentMac[6];

    struct List pendingReads; /* struct IOSana2Req nodes, CMD_READ only */

    /* Minimal stats (task brief scope-down): counted, never enforced. */
    ULONG packetsReceived;
    ULONG packetsSent;
    ULONG rxDropped;
    ULONG lastStart;
};

struct VNetBase {
    struct Device Device;
    struct ExecBase *ExecBase;
    APTR SegList;

    PCIBoard *Board;
    APTR ApertureAddr;      /* PRM_MemoryAddr0 -- CPU-usable BAR0 base */

    /* capability-walk results (aperture addresses, ApertureAddr + offset) */
    APTR CommonCfg;
    APTR NotifyBase;
    ULONG NotifyOffMultiplier;
    APTR IsrCfg;
    APTR DeviceCfg;

    struct VNetQueue rxq;
    struct VNetQueue txq;

    struct Interrupt IntServer;
    BOOL IntServerAdded;

    /* One-shot positive evidence that the DEVICE'S OWN INTx fired
     * (stage 3's proof, as opposed to stage 2's INTX_TEST diagnostic) --
     * set the first time vnet_isr_handler observes VIRTIO_ISR_QUEUE,
     * narrated exactly once so the ISR stays quiet in steady state. */
    BOOL FirstQueueIrqSeen;

    struct VNetUnit Unit0;

    BOOL Ready; /* DevInit completed the whole dance successfully */
};

#define VNETDEV_VERSION  1
#define VNETDEV_REVISION 0

#endif /* VIRTIONET_DEVICE_H */
