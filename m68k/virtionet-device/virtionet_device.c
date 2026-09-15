/*
 * virtionet_device.c -- virtionet.device: a SANA-II network device driving
 * this machine's modern virtio-net PCI function entirely through
 * prometheus.library's PUBLIC API (ADR 0005 stage 3; docs/pci-library.md,
 * docs/pcibridge-protocol.md).
 *
 * A standard Exec AUTOINIT disk device (Devs:virtionet.device), disk-
 * loaded like any other SANA-II driver (OpenDevice("virtionet.device",
 * unit, ...) triggers loading). Same AUTOINIT/Open-Close-Expunge
 * boilerplate shape as m68k/prometheus-library/prometheus_library.c and
 * m68k/rtgboard-card/rtgboard_card.c (itself from amirfb.card, BSD
 * 2-Clause, same owner) -- struct Resident/InitTab/FuncTab, SysBase
 * must-be-defined-not-just-declared (no startup code, -nostdlib),
 * SegList is APTR not BPTR. The device/library FuncTab shapes differ
 * only in their fixed prefix (Open/Close/Expunge/Reserved for both, then
 * BeginIO/AbortIO here instead of a public LVO table -- SANA-II commands
 * are all dispatched inside BeginIO via io_Command, no separate LVOs).
 *
 * ---- SANA-II surface implemented vs. scoped down (recorded here, per
 * task brief, and mirrored in virtionet_device.h's own struct comment) --
 *
 * Implemented: CMD_READ, CMD_WRITE, CMD_FLUSH, S2_DEVICEQUERY,
 * S2_GETSTATIONADDRESS, S2_CONFIGINTERFACE, S2_ONLINE, S2_OFFLINE,
 * S2_BROADCAST (accepted as a no-op -- broadcast is just an ordinary
 * CMD_WRITE addressed FF:FF:FF:FF:FF:FF, nothing here needs special
 * per-frame handling for it).
 *
 * Scoped down, honestly, not silently:
 *   - ONE opener. A second Open of unit 0 is refused with IOERR_UNITBUSY.
 *   - S2_ADD/DELMULTICASTADDRESS[ES]: S2ERR_NOT_SUPPORTED.
 *   - S2_GETGLOBALSTATS: minimal -- fills struct Sana2DeviceStats (via
 *     ios2_StatData, the real ABI's field for it) with the handful of
 *     counters this driver actually keeps (packets in/out, rx drops as
 *     UnknownTypesReceived), everything else honestly zeroed, never a
 *     hard failure. S2_GETSPECIALSTATS/S2_GETTYPESTATS:
 *     S2ERR_NOT_SUPPORTED -- their StatData structs are a different
 *     shape (Sana2SpecialStatRecord/Sana2PacketTypeStats); faking
 *     success against the wrong shape would be worse than refusing.
 *   - S2_ONEVENT: S2ERR_NOT_SUPPORTED (no event tracking at all).
 *   - S2_TRACKTYPE/S2_UNTRACKTYPE/S2_READORPHAN/S2_MULTICAST:
 *     S2ERR_NOT_SUPPORTED.
 *   - CMD_READ matching: one pending-read FIFO, matched against the
 *     incoming frame's ethertype only at the head of the queue (not the
 *     full SANA-II multi-listener fan-out across differently-typed
 *     pending reads) -- see virtionet_device.h's own comment on
 *     struct VNetUnit for the detailed reasoning. Sufficient for
 *     VNetTest's single in-flight read; not a general protocol stack.
 *
 * ---- Byte-order handling (two independent regimes, never confused) ----
 *
 *   1. PCI CONFIG SPACE, walked via prometheus.library's own accessors
 *      (docs/pci-library.md section 3: each aligned config longword is
 *      presented as a big-endian image of its decoded value). To recover
 *      a PLAIN real PCI byte/word at offset p: Prm_ReadConfigByte(b,
 *      p^3) / Prm_ReadConfigWord(b, p^2) -- see cfg_byte()/cfg_word()
 *      below, used only by find_virtio_caps()'s capability walk.
 *      Aligned longs need no offset transform (Prm_ReadConfigLong(b, p)
 *      already returns the plain decoded value, as pciprobe.c's own BAR0
 *      cross-check documents) -- see cfg_long().
 *   2. THE BAR0 APERTURE (docs/pcibridge-protocol.md section 5):
 *      address-invariant, so a natural-width (16/32-bit) CPU load of a
 *      little-endian virtio register comes back byte-swapped as a whole
 *      -- NOT a per-byte-lane puzzle. rd_le16/rd_le32/wr_le16/wr_le32
 *      below do exactly one natural-width volatile access each, then an
 *      arithmetic byte-swap on the result/operand -- never a
 *      byte-decomposed access, because virtio 1.x requires natural-width
 *      accesses to its registers (task brief; docs/pcibridge-protocol.md
 *      section 5 confirms the aperture delivers them). This includes the
 *      ISR status byte (read-to-clear, single access) and every ring
 *      descriptor/avail/used field. Plain single bytes (device_status,
 *      the MAC address bytes in virtio_net_config) need no swap at all.
 */

#include <exec/types.h>
#include <exec/nodes.h>
#include <exec/lists.h>
#include <exec/ports.h>
#include <exec/io.h>
#include <exec/errors.h>
#include <exec/devices.h>
#include <exec/resident.h>
#include <exec/libraries.h>
#include <exec/execbase.h>
#include <exec/interrupts.h>
#include <proto/exec.h>
#include <clib/debug_protos.h>
#include <clib/utility_protos.h>
#include <clib/alib_protos.h> /* NewList() -- an amiga.lib helper, not a
                               * real exec.library LVO (confirmed against
                               * the NDK: clib/exec_protos.h has no
                               * NewList, clib/alib_protos.h does) */
#include <utility/tagitem.h>

#include <libraries/prometheus.h>
#include <proto/prometheus.h>

#include "sana2.h"
#include "virtio_pci.h"
#include "virtionet_device.h"

/* SysBase/PrometheusBase/UtilityBase: <proto/exec.h>/<proto/prometheus.h>'s
 * inline call macros and the clib/utility_protos.h prototypes only
 * extern-declare or plain-declare these globals, expecting either
 * ixemul/libnix startup code (not present -- -nostdlib, no C runtime) or,
 * for utility.library, amiga.lib's own link stubs (which resolve the
 * LVO call against a global of exactly this name) to define/set them.
 * WE must provide the definitions, identical reasoning to
 * prometheus_library.c's own comment on the same three globals. */
struct ExecBase *SysBase;
struct Library *PrometheusBase;
struct Library *UtilityBase;

/* The virtio-net function this driver looks for (docs/pcibridge-
 * protocol.md section 2) -- found via Prm_FindBoardTagList, never a
 * hardcoded address or FindConfigDev. */
#define VNET_PCI_VENDOR 0x1AF4UL
#define VNET_PCI_DEVICE 0x1041UL

/* ---- identity ---------------------------------------------------------- */

const char DevName[] = "virtionet.device";
static const char DevIdString[] =
    "$VER: virtionet.device 1.0 (15.9.2026) m68k-machine\r\n";

/* asm entry point (vnet_int.S) taking the address of this C function. */
void vnet_isr_handler(struct VNetBase *base);
extern void vnet_int_server(void);

/* ---- aperture accessors (byte-order regime 2, file-top comment) -------- */

static UBYTE rd_u8(APTR addr)
{
    return *(volatile UBYTE *)addr;
}

static void wr_u8(APTR addr, UBYTE v)
{
    *(volatile UBYTE *)addr = v;
}

static UWORD rd_le16(APTR addr)
{
    UWORD v = *(volatile UWORD *)addr; /* one natural-width load */
    return (UWORD)(((v & 0x00FFU) << 8) | ((v >> 8) & 0x00FFU));
}

static void wr_le16(APTR addr, UWORD v)
{
    UWORD sw = (UWORD)(((v & 0x00FFU) << 8) | ((v >> 8) & 0x00FFU));
    *(volatile UWORD *)addr = sw; /* one natural-width store */
}

static ULONG rd_le32(APTR addr)
{
    ULONG v = *(volatile ULONG *)addr; /* one natural-width load */
    return ((v & 0x000000FFUL) << 24) | ((v & 0x0000FF00UL) << 8) |
           ((v & 0x00FF0000UL) >> 8)  | ((v & 0xFF000000UL) >> 24);
}

static void wr_le32(APTR addr, ULONG v)
{
    ULONG sw = ((v & 0x000000FFUL) << 24) | ((v & 0x0000FF00UL) << 8) |
               ((v & 0x00FF0000UL) >> 8)  | ((v & 0xFF000000UL) >> 24);
    *(volatile ULONG *)addr = sw; /* one natural-width store */
}

/* 64-bit fields (queue_desc/queue_driver/queue_device) as two independent
 * natural 32-bit accesses -- explicitly permitted by the virtio spec, and
 * the only sane approach on a CPU with no 64-bit registers. Every
 * physical address on this machine fits in 32 bits, so the high half is
 * always 0. */
static void wr_le64(APTR addr, ULONG lo, ULONG hi)
{
    wr_le32(addr, lo);
    wr_le32((APTR)((UBYTE *)addr + 4), hi);
}

/* ---- config-space accessors (byte-order regime 1, file-top comment) ---- */

static UBYTE cfg_byte(PCIBoard *b, UBYTE p)
{
    return Prm_ReadConfigByte(b, (UBYTE)(p ^ 3));
}

static ULONG cfg_long(PCIBoard *b, UBYTE p)
{
    return Prm_ReadConfigLong(b, p);
}

/* ---- capability walk (docs/pcibridge-protocol.md section 2; virtio spec
 * 4.1.4) --------------------------------------------------------------- */

static BOOL find_virtio_caps(struct VNetBase *base, PCIBoard *board,
                             ULONG apertureCpu)
{
    UBYTE cap_ptr = cfg_byte(board, PCICFG_CAP_PTR);
    BOOL got_common = FALSE, got_notify = FALSE, got_isr = FALSE,
         got_device = FALSE;

    while (cap_ptr != 0) {
        UBYTE vndr = cfg_byte(board, (UBYTE)(cap_ptr + VIRTIO_PCI_CAP_VNDR_OFF));
        UBYTE next = cfg_byte(board, (UBYTE)(cap_ptr + VIRTIO_PCI_CAP_NEXT_OFF));

        if (vndr == PCI_CAP_ID_VNDR) {
            UBYTE cfgtype = cfg_byte(board,
                    (UBYTE)(cap_ptr + VIRTIO_PCI_CAP_CFGTYPE_OFF));
            UBYTE bar = cfg_byte(board,
                    (UBYTE)(cap_ptr + VIRTIO_PCI_CAP_BAR_OFF));
            ULONG off = cfg_long(board,
                    (UBYTE)(cap_ptr + VIRTIO_PCI_CAP_OFFSET_OFF));

            if (bar != 0) {
                KPrintF((CONST_STRPTR)"VNETDEV: DevInit: FAIL capability "
                        "type %ld names BAR%ld, only BAR0 exists on this "
                        "device\n", (LONG)cfgtype, (LONG)bar);
                return FALSE;
            }

            switch (cfgtype) {
            case VIRTIO_PCI_CAP_COMMON_CFG:
                base->CommonCfg = (APTR)(apertureCpu + off);
                got_common = TRUE;
                break;
            case VIRTIO_PCI_CAP_NOTIFY_CFG:
                base->NotifyBase = (APTR)(apertureCpu + off);
                base->NotifyOffMultiplier = cfg_long(board,
                        (UBYTE)(cap_ptr + VIRTIO_PCI_NOTIFY_CAP_MULT_OFF));
                got_notify = TRUE;
                break;
            case VIRTIO_PCI_CAP_ISR_CFG:
                base->IsrCfg = (APTR)(apertureCpu + off);
                got_isr = TRUE;
                break;
            case VIRTIO_PCI_CAP_DEVICE_CFG:
                base->DeviceCfg = (APTR)(apertureCpu + off);
                got_device = TRUE;
                break;
            default:
                break; /* PCI_CFG or unrecognised -- not needed here */
            }
        }

        cap_ptr = next;
    }

    if (!got_common || !got_notify || !got_isr || !got_device) {
        KPrintF((CONST_STRPTR)"VNETDEV: DevInit: FAIL capability walk "
                "incomplete (common %ld notify %ld isr %ld device %ld)\n",
                (LONG)got_common, (LONG)got_notify, (LONG)got_isr,
                (LONG)got_device);
        return FALSE;
    }

    KPrintF((CONST_STRPTR)"VNETDEV: DevInit: capabilities common $%08lx "
            "notify $%08lx (multiplier %ld) isr $%08lx device $%08lx\n",
            (ULONG)base->CommonCfg, (ULONG)base->NotifyBase,
            (LONG)base->NotifyOffMultiplier, (ULONG)base->IsrCfg,
            (ULONG)base->DeviceCfg);
    return TRUE;
}

/* ---- virtqueue helpers -------------------------------------------------- */

static void vq_set_desc(struct VNetQueue *q, UWORD idx, APTR phys, ULONG len,
                        UWORD flags)
{
    UBYTE *d = q->desc + (ULONG)idx * VRING_DESC_SIZE;
    wr_le64(d + VRING_DESC_ADDR, (ULONG)phys, 0);
    wr_le32(d + VRING_DESC_LEN, len);
    wr_le16(d + VRING_DESC_FLAGS, flags);
    wr_le16(d + VRING_DESC_NEXT, 0);
}

/* Caller must hold Disable() -- avail_idx_shadow/the avail ring are
 * shared with the ISR's own refill (rx) path. */
static void vq_push_avail(struct VNetQueue *q, UWORD descIdx)
{
    UWORD pos = (UWORD)(q->avail_idx_shadow % q->qsize);
    wr_le16(q->avail + VRING_AVAIL_RING + (ULONG)pos * 2, descIdx);
    q->avail_idx_shadow++;
    wr_le16(q->avail + VRING_AVAIL_IDX, q->avail_idx_shadow);
}

static void vnet_notify(struct VNetQueue *q, UWORD qidx)
{
    wr_le16(q->notify_addr, qidx);
}

/* ---- queue setup (virtio spec 4.1.5.1.3) -------------------------------- */

static BOOL setup_queue(struct VNetBase *base, UWORD qidx, struct VNetQueue *q)
{
    UWORD devsize, qsize;
    ULONG descBytes, availBytes, usedBytes;
    APTR descPhys, availPhys, usedPhys;
    ULONG i;

    wr_le16(base->CommonCfg + VCFG_QUEUE_SELECT, qidx);
    devsize = rd_le16(base->CommonCfg + VCFG_QUEUE_SIZE);
    qsize = (devsize < VNET_QUEUE_CAP) ? devsize : VNET_QUEUE_CAP;
    if (qsize == 0) {
        KPrintF((CONST_STRPTR)"VNETDEV: DevInit: FAIL queue %ld device "
                "queue_size is 0\n", (LONG)qidx);
        return FALSE;
    }
    q->qsize = qsize;

    descBytes  = (ULONG)qsize * VRING_DESC_SIZE;
    availBytes = 4UL + (ULONG)qsize * 2UL;
    usedBytes  = 4UL + (ULONG)qsize * VRING_USED_ELEM_SIZE;

    q->descRawSize  = descBytes  + VRING_DESC_ALIGN  - 1UL;
    q->availRawSize = availBytes + VRING_AVAIL_ALIGN - 1UL;
    q->usedRawSize  = usedBytes  + VRING_USED_ALIGN  - 1UL;

    q->descRaw  = Prm_AllocDMABuffer(q->descRawSize);
    q->availRaw = Prm_AllocDMABuffer(q->availRawSize);
    q->usedRaw  = Prm_AllocDMABuffer(q->usedRawSize);
    if (!q->descRaw || !q->availRaw || !q->usedRaw) {
        KPrintF((CONST_STRPTR)"VNETDEV: DevInit: FAIL queue %ld ring DMA "
                "allocation failed\n", (LONG)qidx);
        return FALSE;
    }

    q->desc  = (UBYTE *)(((ULONG)q->descRaw  + (VRING_DESC_ALIGN  - 1UL)) &
                         ~(ULONG)(VRING_DESC_ALIGN  - 1UL));
    q->avail = (UBYTE *)(((ULONG)q->availRaw + (VRING_AVAIL_ALIGN - 1UL)) &
                         ~(ULONG)(VRING_AVAIL_ALIGN - 1UL));
    q->used  = (UBYTE *)(((ULONG)q->usedRaw  + (VRING_USED_ALIGN  - 1UL)) &
                         ~(ULONG)(VRING_USED_ALIGN  - 1UL));

    for (i = 0; i < descBytes; i++)  q->desc[i]  = 0;
    for (i = 0; i < availBytes; i++) q->avail[i] = 0;
    for (i = 0; i < usedBytes; i++)  q->used[i]  = 0;

    q->avail_idx_shadow = 0;
    q->used_idx_seen = 0;

    descPhys  = Prm_GetPhysicalAddress(q->desc);
    availPhys = Prm_GetPhysicalAddress(q->avail);
    usedPhys  = Prm_GetPhysicalAddress(q->used);

    wr_le64(base->CommonCfg + VCFG_QUEUE_DESC,   (ULONG)descPhys, 0);
    wr_le64(base->CommonCfg + VCFG_QUEUE_DRIVER, (ULONG)availPhys, 0);
    wr_le64(base->CommonCfg + VCFG_QUEUE_DEVICE, (ULONG)usedPhys, 0);
    wr_le16(base->CommonCfg + VCFG_QUEUE_SIZE, qsize);

    q->notify_off = rd_le16(base->CommonCfg + VCFG_QUEUE_NOTIFY_OFF);
    q->notify_addr = (APTR)((UBYTE *)base->NotifyBase +
                            (ULONG)q->notify_off * base->NotifyOffMultiplier);

    wr_le16(base->CommonCfg + VCFG_QUEUE_ENABLE, 1);

    KPrintF((CONST_STRPTR)"VNETDEV: DevInit: queue %ld size %ld (device "
            "offered %ld) desc $%08lx avail $%08lx used $%08lx notify "
            "$%08lx\n",
            (LONG)qidx, (LONG)qsize, (LONG)devsize, (ULONG)descPhys,
            (ULONG)availPhys, (ULONG)usedPhys, (ULONG)q->notify_addr);
    return TRUE;
}

static BOOL init_tx_slots(struct VNetBase *base)
{
    struct VNetQueue *q = &base->txq;
    UWORD i;

    for (i = 0; i < q->qsize; i++) {
        APTR raw = Prm_AllocDMABuffer(VNET_RXBUF_SIZE);
        if (!raw) {
            KPrintF((CONST_STRPTR)"VNETDEV: DevInit: FAIL tx buffer %ld "
                    "allocation failed\n", (LONG)i);
            return FALSE;
        }
        q->bufRaw[i] = raw;
        q->bufRawSize[i] = VNET_RXBUF_SIZE;
        q->bufPtrs[i] = (UBYTE *)raw;
        q->bufPhys[i] = Prm_GetPhysicalAddress(raw);
        q->slotFree[i] = TRUE;
        q->pending[i] = NULL;
    }
    KPrintF((CONST_STRPTR)"VNETDEV: DevInit: allocated %ld tx buffer(s) of "
            "%ld bytes each\n", (LONG)q->qsize, (LONG)VNET_RXBUF_SIZE);
    return TRUE;
}

static BOOL post_rx_buffers(struct VNetBase *base)
{
    struct VNetQueue *q = &base->rxq;
    UWORD i;

    for (i = 0; i < q->qsize; i++) {
        APTR raw = Prm_AllocDMABuffer(VNET_RXBUF_SIZE);
        APTR phys;

        if (!raw) {
            KPrintF((CONST_STRPTR)"VNETDEV: DevInit: FAIL rx buffer %ld "
                    "allocation failed\n", (LONG)i);
            return FALSE;
        }
        q->bufRaw[i] = raw;
        q->bufRawSize[i] = VNET_RXBUF_SIZE;
        q->bufPtrs[i] = (UBYTE *)raw;
        phys = Prm_GetPhysicalAddress(raw);
        q->bufPhys[i] = phys;

        vq_set_desc(q, i, phys, VNET_RXBUF_SIZE, VRING_DESC_F_WRITE);
        vq_push_avail(q, i);
    }

    vnet_notify(q, VNET_QUEUE_RX);
    KPrintF((CONST_STRPTR)"VNETDEV: DevInit: posted %ld rx buffer(s) of "
            "%ld bytes each\n", (LONG)q->qsize, (LONG)VNET_RXBUF_SIZE);
    return TRUE;
}

static void free_queue_dma(struct VNetQueue *q)
{
    UWORD i;
    for (i = 0; i < q->qsize; i++)
        if (q->bufRaw[i])
            Prm_FreeDMABuffer(q->bufRaw[i], q->bufRawSize[i]);
    if (q->descRaw)  Prm_FreeDMABuffer(q->descRaw, q->descRawSize);
    if (q->availRaw) Prm_FreeDMABuffer(q->availRaw, q->availRawSize);
    if (q->usedRaw)  Prm_FreeDMABuffer(q->usedRaw, q->usedRawSize);
}

/* ---- tx slot pool -------------------------------------------------------
 * Disable()/Enable(): the free-list is shared with the ISR's own
 * completion path (vnet_process_tx_used marks a slot free again). */

static LONG vnet_txslot_alloc(struct VNetBase *base)
{
    struct VNetQueue *q = &base->txq;
    UWORD i;
    LONG found = -1;

    Disable();
    for (i = 0; i < q->qsize; i++) {
        if (q->slotFree[i]) {
            q->slotFree[i] = FALSE;
            found = i;
            break;
        }
    }
    Enable();
    return found;
}

static void vnet_txslot_free(struct VNetBase *base, UWORD slot)
{
    Disable();
    base->txq.slotFree[slot] = TRUE;
    base->txq.pending[slot] = NULL;
    Enable();
}

/* ---- pending CMD_READ queue --------------------------------------------
 * Plain exec/lists.h struct List, so Remove()/AddTail() are ordinary exec
 * calls -- no MinList/MinNode layout subtlety to worry about. */

static struct IOSana2Req *pending_head(struct VNetUnit *unit)
{
    struct Node *n = unit->pendingReads.lh_Head;
    if (n->ln_Succ == NULL)
        return NULL; /* n is the list's own tail sentinel: empty */
    return (struct IOSana2Req *)n;
}

/* Fills a 16-byte SANA-II address array from a 6-byte Ethernet MAC,
 * explicit byte-by-byte (not a counting loop): ~/src/sana2loop's own
 * docs/sana2-notes.md warns that a plain fixed-count array-copy loop
 * over a freestanding (-nostdlib) build got recognized by -O2 and
 * replaced with a call to memmove/memset, which doesn't exist in such a
 * build -- a link failure this project's own toolchain (also
 * -nostdlib/-nostartfiles) is equally exposed to. Bytes 6..15 are always
 * zeroed, never left as garbage (sana2-notes: "should be zeroed, not
 * left as garbage" -- SANA2_MAX_ADDR_BYTES is 16 even though Ethernet
 * only uses 6). */
static void fill_sana2_addr(UBYTE *dst, const UBYTE *mac6)
{
    dst[0] = mac6[0]; dst[1] = mac6[1]; dst[2] = mac6[2];
    dst[3] = mac6[3]; dst[4] = mac6[4]; dst[5] = mac6[5];
    dst[6] = 0;  dst[7] = 0;  dst[8] = 0;  dst[9] = 0;
    dst[10] = 0; dst[11] = 0; dst[12] = 0; dst[13] = 0;
    dst[14] = 0; dst[15] = 0;
}

/* ---- interrupt server (vnet_int.S calls this with A1 = &VNetBase) ------ */

static void vnet_process_rx_used(struct VNetBase *base)
{
    struct VNetQueue *q = &base->rxq;
    struct VNetUnit *unit = &base->Unit0;
    UWORD dev_idx = rd_le16(q->used + VRING_USED_IDX);

    while (q->used_idx_seen != dev_idx) {
        UWORD pos = (UWORD)(q->used_idx_seen % q->qsize);
        UBYTE *elem = q->used + VRING_USED_RING +
                      (ULONG)pos * VRING_USED_ELEM_SIZE;
        ULONG descId = rd_le32(elem + VRING_USED_ELEM_ID);
        ULONG len    = rd_le32(elem + VRING_USED_ELEM_LEN);
        UBYTE *buf;

        if (descId >= (ULONG)q->qsize) {
            /* Defensive: a well-behaved device never does this: the used
             * ring is a permutation of descriptors this driver itself
             * posted. Narrated because silent failure is this
             * platform's bug class -- but the loop must still advance,
             * or a single bad entry wedges rx forever. */
            KPrintF((CONST_STRPTR)"VNETDEV: rx ISR: FAIL used descriptor "
                    "id %ld out of range (qsize %ld) -- entry dropped\n",
                    (LONG)descId, (LONG)q->qsize);
            q->used_idx_seen++;
            continue;
        }

        buf = q->bufPtrs[descId];
        unit->packetsReceived++;

        if (len > VNET_HDR_SIZE + VNET_ETH_HDR_LEN) {
            UBYTE *frame = buf + VNET_HDR_SIZE;
            ULONG ethertype = ((ULONG)frame[12] << 8) | frame[13];
            struct IOSana2Req *head = pending_head(unit);

            if (head != NULL &&
                (head->ios2_PacketType == 0 ||
                 head->ios2_PacketType == ethertype)) {
                ULONG payloadLen = len - VNET_HDR_SIZE - VNET_ETH_HDR_LEN;
                /* ios2_DataLength is dual-role on this ABI: on entry it
                 * is the caller's max buffer size, on completion it
                 * becomes the actual bytes transferred -- capture the
                 * max BEFORE overwriting it below. */
                ULONG copyLen = payloadLen;
                ULONG maxLen = head->ios2_DataLength;

                Remove((struct Node *)head);

                if (copyLen > maxLen)
                    copyLen = maxLen;

                /* ios2_SrcAddr/ios2_DstAddr are inline arrays on the
                 * request itself (SANA-II ABI), never NULL -- zero bytes
                 * 6..15 too (fill_sana2_addr's own comment). */
                fill_sana2_addr(head->ios2_DstAddr, frame);
                fill_sana2_addr(head->ios2_SrcAddr, frame + 6);
                head->ios2_PacketType = ethertype;

                /* Hook failure posture per the real ABI (~/src/
                 * sana2loop's docs/sana2-notes.md "Buffer management
                 * hook" point 6 / src/loopback_device.c's own hook call
                 * sites): S2ERR_NO_RESOURCES + S2WERR_BUFF_ERROR, not a
                 * crash or a silently-wrong copy -- an earlier draft of
                 * this driver used S2ERR_BAD_ADDRESS here. */
                if (unit->copyToBuff((APTR)head->ios2_Data,
                                     (APTR)(frame + VNET_ETH_HDR_LEN),
                                     copyLen)) {
                    head->ios2_DataLength = copyLen; /* actual, overwriting
                                                       * the input max */
                    head->ios2_Req.io_Error = 0;
                } else {
                    head->ios2_Req.io_Error = S2ERR_NO_RESOURCES;
                    head->ios2_WireError = S2WERR_BUFF_ERROR;
                }

                ReplyMsg(&head->ios2_Req.io_Message);
            } else {
                unit->rxDropped++;
            }
        } else {
            unit->rxDropped++;
        }

        /* Refill in place -- this driver's rx buffers are pre-posted
         * once at DevInit and recycled forever, never returned to a
         * pool (task brief scope). */
        vq_set_desc(q, (UWORD)descId, q->bufPhys[descId], VNET_RXBUF_SIZE,
                    VRING_DESC_F_WRITE);
        vq_push_avail(q, (UWORD)descId);

        q->used_idx_seen++;
    }

    vnet_notify(q, VNET_QUEUE_RX);
}

static void vnet_process_tx_used(struct VNetBase *base)
{
    struct VNetQueue *q = &base->txq;
    struct VNetUnit *unit = &base->Unit0;
    UWORD dev_idx = rd_le16(q->used + VRING_USED_IDX);

    while (q->used_idx_seen != dev_idx) {
        UWORD pos = (UWORD)(q->used_idx_seen % q->qsize);
        UBYTE *elem = q->used + VRING_USED_RING +
                      (ULONG)pos * VRING_USED_ELEM_SIZE;
        ULONG descId = rd_le32(elem + VRING_USED_ELEM_ID);
        struct IOSana2Req *io;

        if (descId >= (ULONG)q->qsize) {
            KPrintF((CONST_STRPTR)"VNETDEV: tx ISR: FAIL used descriptor "
                    "id %ld out of range (qsize %ld) -- entry dropped\n",
                    (LONG)descId, (LONG)q->qsize);
            q->used_idx_seen++;
            continue;
        }

        io = q->pending[descId];
        if (io != NULL) {
            /* No io_Actual on this ABI (struct IOSana2Req's base is
             * plain struct IORequest) -- ios2_DataLength already holds
             * the bytes sent (it was the input on CMD_WRITE and this
             * ABI has no separate completion-time field for tx). */
            io->ios2_Req.io_Error = 0;
            unit->packetsSent++;
            ReplyMsg(&io->ios2_Req.io_Message);
        }

        q->pending[descId] = NULL;
        q->slotFree[descId] = TRUE;

        q->used_idx_seen++;
    }
}

void vnet_isr_handler(struct VNetBase *base)
{
    UBYTE isr;

    if (!base->Ready)
        return;

    /* Read-to-clear (virtio spec 4.1.4.5): this single access is also
     * what deasserts the level-triggered INTx line at the device (task
     * brief: "the ISR read does it") -- never touch INTX_ENABLE here,
     * that register belongs to prometheus.library (docs/pci-library.md
     * section 5). */
    isr = rd_u8(base->IsrCfg);

    if (isr & VIRTIO_ISR_QUEUE) {
        if (!base->FirstQueueIrqSeen) {
            /* Positive evidence this stage's own bar demands: the
             * DEVICE'S OWN INTx firing (not stage 2's INTX_TEST
             * diagnostic) reaching this handler via INT2. One-shot so
             * the ISR stays quiet in steady state. */
            base->FirstQueueIrqSeen = TRUE;
            KPrintF((CONST_STRPTR)"VNETDEV: isr: first queue interrupt "
                    "observed (device INTx via INT2)\n");
        }
        vnet_process_rx_used(base);
        vnet_process_tx_used(base);
    }
    /* VIRTIO_ISR_CONFIG (bit 1, device config changed): ignored -- this
     * driver never re-reads its config after DevInit (scope). */
}

/* ---- BeginIO command handlers ------------------------------------------- */

static void TermIO(struct IOSana2Req *io)
{
    if (!(io->ios2_Req.io_Flags & IOF_QUICK))
        ReplyMsg(&io->ios2_Req.io_Message);
}

static void cmd_read(struct VNetBase *base, struct VNetUnit *unit,
                     struct IOSana2Req *io)
{
    (void)base;
    io->ios2_Req.io_Flags &= ~IOF_QUICK; /* always completed asynchronously,
                                           * from the ISR */

    if (!unit->configured) {
        io->ios2_Req.io_Error = S2ERR_BAD_STATE;
        io->ios2_WireError = S2WERR_NOT_CONFIGURED;
        TermIO(io);
        return;
    }
    if (!unit->online) {
        io->ios2_Req.io_Error = S2ERR_BAD_STATE;
        io->ios2_WireError = S2WERR_UNIT_OFFLINE;
        TermIO(io);
        return;
    }

    Disable();
    AddTail(&unit->pendingReads, (struct Node *)io);
    Enable();
}

static void cmd_write(struct VNetBase *base, struct VNetUnit *unit,
                      struct IOSana2Req *io)
{
    LONG slot;
    UBYTE *buf;
    ULONG framelen;
    int i;

    io->ios2_Req.io_Flags &= ~IOF_QUICK; /* always completed asynchronously,
                                           * from the ISR */

    if (!unit->configured) {
        io->ios2_Req.io_Error = S2ERR_BAD_STATE;
        io->ios2_WireError = S2WERR_NOT_CONFIGURED;
        TermIO(io);
        return;
    }
    if (!unit->online) {
        io->ios2_Req.io_Error = S2ERR_BAD_STATE;
        io->ios2_WireError = S2WERR_UNIT_OFFLINE;
        TermIO(io);
        return;
    }
    if (io->ios2_DataLength > (VNET_FRAME_MAX - VNET_ETH_HDR_LEN)) {
        io->ios2_Req.io_Error = S2ERR_MTU_EXCEEDED;
        TermIO(io);
        return;
    }
    /* ios2_DstAddr is an inline array on this ABI, never NULL -- no
     * presence check needed (unlike an earlier draft of this driver). */

    slot = vnet_txslot_alloc(base);
    if (slot < 0) {
        io->ios2_Req.io_Error = S2ERR_OUTOFSERVICE;
        io->ios2_WireError = S2WERR_BUFF_ERROR;
        TermIO(io);
        return;
    }

    buf = base->txq.bufPtrs[slot];

    /* virtio-net header (VNET_HDR_SIZE bytes, VERSION_1 layout): no
     * offload requested, num_buffers left 0 (ignored by the device on
     * tx when VIRTIO_NET_F_MRG_RXBUF is not negotiated -- scope). */
    for (i = 0; i < VNET_HDR_SIZE; i++)
        buf[i] = 0;
    framelen = VNET_HDR_SIZE;

    for (i = 0; i < 6; i++)
        buf[framelen + i] = io->ios2_DstAddr[i];
    framelen += 6;
    for (i = 0; i < 6; i++)
        buf[framelen + i] = unit->currentMac[i]; /* always our own CURRENT
                                                   * station address as
                                                   * source, never a
                                                   * caller-supplied
                                                   * ios2_SrcAddr (tx is
                                                   * only reachable once
                                                   * configured, so
                                                   * currentMac is always
                                                   * valid here) */
    framelen += 6;
    buf[framelen]     = (UBYTE)(io->ios2_PacketType >> 8);
    buf[framelen + 1] = (UBYTE)(io->ios2_PacketType & 0xFF);
    framelen += 2;

    /* Hook failure posture per the real ABI (~/src/sana2loop's docs/
     * sana2-notes.md "Buffer management hook" point 6): S2ERR_NO_RESOURCES
     * + S2WERR_BUFF_ERROR, not a crash or a silently-wrong copy -- an
     * earlier draft of this driver used S2ERR_BAD_ADDRESS here. */
    if (io->ios2_DataLength != 0 &&
        !unit->copyFromBuff((APTR)(buf + framelen), io->ios2_Data,
                            io->ios2_DataLength)) {
        vnet_txslot_free(base, (UWORD)slot);
        io->ios2_Req.io_Error = S2ERR_NO_RESOURCES;
        io->ios2_WireError = S2WERR_BUFF_ERROR;
        TermIO(io);
        return;
    }
    framelen += io->ios2_DataLength;

    if (framelen < (ULONG)(VNET_ETH_MIN_FRAME + VNET_HDR_SIZE)) {
        ULONG pad = (ULONG)(VNET_ETH_MIN_FRAME + VNET_HDR_SIZE) - framelen;
        ULONG j;
        for (j = 0; j < pad; j++)
            buf[framelen + j] = 0;
        framelen += pad;
    }

    Disable();
    base->txq.pending[slot] = io;
    vq_set_desc(&base->txq, (UWORD)slot, base->txq.bufPhys[slot], framelen, 0);
    vq_push_avail(&base->txq, (UWORD)slot);
    Enable();

    vnet_notify(&base->txq, VNET_QUEUE_TX);
}

static void cmd_flush(struct VNetBase *base, struct VNetUnit *unit,
                      struct IOSana2Req *io)
{
    struct IOSana2Req *pending;

    (void)base;

    Disable();
    while ((pending = pending_head(unit)) != NULL) {
        Remove((struct Node *)pending);
        pending->ios2_Req.io_Error = IOERR_ABORTED;
        ReplyMsg(&pending->ios2_Req.io_Message);
    }
    Enable();

    io->ios2_Req.io_Error = 0;
}

static void cmd_devicequery(struct VNetBase *base, struct VNetUnit *unit,
                            struct IOSana2Req *io)
{
    /* Real SANA-II ABI: S2_DEVICEQUERY fills struct Sana2DeviceQuery via
     * ios2_StatData, NOT ios2_Data (confirmed against ~/src/sana2loop's
     * docs/sana2-notes.md "Commands implemented" table and
     * src/loopback_device.c's own S2_DEVICEQUERY handler) -- an earlier
     * draft of this driver used the wrong field. */
    struct Sana2DeviceQuery *dq = (struct Sana2DeviceQuery *)io->ios2_StatData;
    struct Sana2DeviceQuery full;
    ULONG have;
    UBYTE *src, *dst;
    ULONG k;

    (void)base;
    (void)unit;

    if (!dq) {
        io->ios2_Req.io_Error = S2ERR_BAD_ARGUMENT;
        return;
    }

    /* SizeAvailable (in): how much of struct Sana2DeviceQuery the CALLER
     * has room for -- lets the interface grow over time without
     * breaking old callers/drivers (real SANA-II ABI convention).
     * SizeSupplied (out): how much THIS driver actually filled in,
     * clamped to whichever is smaller. Filled via a fully-populated
     * local struct then copied byte-for-byte up to 'have', so a
     * caller with an older/smaller struct never receives a partial
     * trailing field this driver doesn't know is truncated. */
    have = dq->SizeAvailable;
    if (have > sizeof(struct Sana2DeviceQuery))
        have = sizeof(struct Sana2DeviceQuery);

    full.SizeAvailable  = sizeof(struct Sana2DeviceQuery);
    full.SizeSupplied   = have;
    full.DevQueryFormat = 0;
    full.DeviceLevel    = 0;
    full.AddrFieldSize  = 48; /* bits -- Ethernet */
    full.MTU            = VNET_FRAME_MAX - VNET_ETH_HDR_LEN;
    /* virtio-net models no real link speed; this is this project's own
     * placeholder figure, not read from any device register. */
    full.BPS            = 1000000000UL;
    full.HardwareType   = S2WireType_Ethernet;

    src = (UBYTE *)&full;
    dst = (UBYTE *)dq;
    for (k = 0; k < have; k++)
        dst[k] = src[k];

    io->ios2_Req.io_Error = 0;
    io->ios2_DataLength = have;
}

static void cmd_getstationaddress(struct VNetBase *base, struct VNetUnit *unit,
                                  struct IOSana2Req *io)
{
    (void)base;

    /* Real SANA-II ABI convention (~/src/sana2loop's docs/sana2-notes.md,
     * "S2_GETSTATIONADDRESS" / "current-vs-factory station address
     * split"): ios2_SrcAddr <- CURRENT active address, ios2_DstAddr <-
     * FACTORY/default address -- the two may diverge once
     * S2_CONFIGINTERFACE has run. */
    fill_sana2_addr(io->ios2_SrcAddr, unit->currentMac);
    fill_sana2_addr(io->ios2_DstAddr, unit->factoryMac);

    io->ios2_Req.io_Error = 0;
}

static void cmd_configinterface(struct VNetBase *base, struct VNetUnit *unit,
                                struct IOSana2Req *io)
{
    (void)base;

    /* Real SANA-II ABI (~/src/sana2loop's docs/sana2-notes.md
     * "S2_CONFIGINTERFACE and the current-vs-factory station address
     * split (M3)"): the FIRST call adopts ios2_SrcAddr's low 6 bytes as
     * the new CURRENT station address (factoryMac never changes because
     * of this); a SECOND call on an already-configured unit fails
     * S2ERR_BAD_STATE/S2WERR_IS_CONFIGURED -- configuring is a one-shot
     * action per unit lifetime, not "last write wins". An earlier draft
     * of this driver ignored the caller's requested address entirely
     * (read ios2_Data, which isn't even the right field) -- this
     * version genuinely honours it, unlike this file's earlier
     * decision to treat the address as fixed. */

    if (unit->configured) {
        io->ios2_Req.io_Error = S2ERR_BAD_STATE;
        io->ios2_WireError = S2WERR_IS_CONFIGURED;
        return;
    }

    unit->currentMac[0] = io->ios2_SrcAddr[0];
    unit->currentMac[1] = io->ios2_SrcAddr[1];
    unit->currentMac[2] = io->ios2_SrcAddr[2];
    unit->currentMac[3] = io->ios2_SrcAddr[3];
    unit->currentMac[4] = io->ios2_SrcAddr[4];
    unit->currentMac[5] = io->ios2_SrcAddr[5];

    unit->configured = TRUE;
    io->ios2_Req.io_Error = 0;

    /* Report the address actually in effect back into ios2_SrcAddr, per
     * the ABI's "actual address used" convention (also zeroing bytes
     * 6..15 of the caller's array, same reasoning as
     * S2_GETSTATIONADDRESS). */
    fill_sana2_addr(io->ios2_SrcAddr, unit->currentMac);

    KPrintF((CONST_STRPTR)"VNETDEV: configinterface: station address "
            "%02lx:%02lx:%02lx:%02lx:%02lx:%02lx\n",
            (ULONG)unit->currentMac[0], (ULONG)unit->currentMac[1],
            (ULONG)unit->currentMac[2], (ULONG)unit->currentMac[3],
            (ULONG)unit->currentMac[4], (ULONG)unit->currentMac[5]);
}

static void cmd_online(struct VNetBase *base, struct VNetUnit *unit,
                       struct IOSana2Req *io)
{
    (void)base;
    if (!unit->configured) {
        io->ios2_Req.io_Error = S2ERR_BAD_STATE;
        io->ios2_WireError = S2WERR_NOT_CONFIGURED;
        return;
    }
    unit->online = TRUE;
    io->ios2_Req.io_Error = 0;
    KPrintF((CONST_STRPTR)"VNETDEV: online: unit 0 online\n");
}

static void cmd_offline(struct VNetBase *base, struct VNetUnit *unit,
                        struct IOSana2Req *io)
{
    (void)base;
    unit->online = FALSE;
    io->ios2_Req.io_Error = 0;
    KPrintF((CONST_STRPTR)"VNETDEV: offline: unit 0 offline\n");
}

/* S2_GETGLOBALSTATS: the real SANA-II ABI carries stats via
 * ios2_StatData (a struct Sana2DeviceStats*), never ios2_Data/
 * ios2_DataLength raw words -- an earlier draft of this driver used the
 * wrong field. Scope (task brief): "minimal" -- filled with the handful
 * of counters this driver actually keeps, everything else honestly
 * zeroed (BadData/Overruns/Unused/UnknownTypesReceived/Reconfigurations,
 * LastStart) rather than fabricated. */
static void cmd_getglobalstats(struct VNetBase *base, struct VNetUnit *unit,
                               struct IOSana2Req *io)
{
    struct Sana2DeviceStats *st = (struct Sana2DeviceStats *)io->ios2_StatData;

    (void)base;

    if (!st) {
        io->ios2_Req.io_Error = S2ERR_BAD_ARGUMENT;
        io->ios2_WireError = S2WERR_NULL_POINTER;
        return;
    }

    st->PacketsReceived = unit->packetsReceived;
    st->PacketsSent = unit->packetsSent;
    st->BadData = 0;
    st->Overruns = 0;
    st->Unused = 0;
    st->UnknownTypesReceived = unit->rxDropped;
    st->Reconfigurations = 0;
    st->LastStart.tv_secs = 0;
    st->LastStart.tv_micro = 0;

    io->ios2_Req.io_Error = 0;
}

static void VNetBeginIO(struct IOSana2Req *io __asm("a1"),
                        struct VNetBase *base __asm("a6"))
{
    struct VNetUnit *unit = (struct VNetUnit *)io->ios2_Req.io_Unit;

    io->ios2_Req.io_Error = 0;
    io->ios2_WireError = 0;

    if (!base->Ready) {
        io->ios2_Req.io_Error = IOERR_OPENFAIL;
        TermIO(io);
        return;
    }

    switch (io->ios2_Req.io_Command) {
    case CMD_READ:
        cmd_read(base, unit, io);
        return;
    case CMD_WRITE:
        cmd_write(base, unit, io);
        return;

    case CMD_FLUSH:
        cmd_flush(base, unit, io);
        break;
    case S2_DEVICEQUERY:
        cmd_devicequery(base, unit, io);
        break;
    case S2_GETSTATIONADDRESS:
        cmd_getstationaddress(base, unit, io);
        break;
    case S2_CONFIGINTERFACE:
        cmd_configinterface(base, unit, io);
        break;
    case S2_ONLINE:
        cmd_online(base, unit, io);
        break;
    case S2_OFFLINE:
        cmd_offline(base, unit, io);
        break;
    case S2_BROADCAST:
        io->ios2_Req.io_Error = 0;
        break;

    case S2_ADDMULTICASTADDRESS:
    case S2_ADDMULTICASTADDRESSES:
    case S2_DELMULTICASTADDRESS:
    case S2_DELMULTICASTADDRESSES:
        io->ios2_Req.io_Error = S2ERR_NOT_SUPPORTED;
        io->ios2_WireError = S2WERR_BAD_MULTICAST;
        break;

    case S2_GETGLOBALSTATS:
        cmd_getglobalstats(base, unit, io);
        break;

    case S2_GETTYPESTATS:
    case S2_GETSPECIALSTATS:
        /* Distinct StatData struct shapes (Sana2PacketTypeStats /
         * Sana2SpecialStatRecord+Header) this driver does not implement
         * -- honest refusal, not a success against the wrong shape
         * (task brief scope). */
        io->ios2_Req.io_Error = S2ERR_NOT_SUPPORTED;
        break;

    case S2_ONEVENT:
    case S2_TRACKTYPE:
    case S2_UNTRACKTYPE:
    case S2_READORPHAN:
    case S2_MULTICAST:
        io->ios2_Req.io_Error = S2ERR_NOT_SUPPORTED;
        break;

    default:
        io->ios2_Req.io_Error = IOERR_NOCMD;
        break;
    }

    TermIO(io);
}

static LONG VNetAbortIO(struct IOSana2Req *io __asm("a1"),
                        struct VNetBase *base __asm("a6"))
{
    struct VNetUnit *unit = (struct VNetUnit *)io->ios2_Req.io_Unit;
    struct Node *n;
    BOOL found = FALSE;

    (void)base;

    if (io->ios2_Req.io_Command != CMD_READ)
        return IOERR_NOCMD; /* only a queued CMD_READ can be aborted here --
                              * tx completes too quickly to be worth an
                              * abort path, and every other command
                              * already completes synchronously in
                              * BeginIO (scope, recorded). */

    Disable();
    for (n = unit->pendingReads.lh_Head; n->ln_Succ != NULL; n = n->ln_Succ) {
        if (n == (struct Node *)io) {
            Remove(n);
            found = TRUE;
            break;
        }
    }
    Enable();

    if (!found)
        return IOERR_NOCMD; /* already completed, or never queued */

    io->ios2_Req.io_Error = IOERR_ABORTED;
    ReplyMsg(&io->ios2_Req.io_Message);
    return 0;
}

/* ---- standard AUTOINIT device boilerplate ------------------------------- */

static APTR find_tag_ptr(struct TagItem *tags, Tag tagval)
{
    struct TagItem *t;

    if (!tags)
        return NULL;

    t = FindTagItem(tagval, tags);
    return t ? (APTR)t->ti_Data : NULL;
}

static BPTR VNetExpungeInternal(struct VNetBase *base)
{
    BPTR seglist;

    if (base->Device.dd_Library.lib_OpenCnt) {
        base->Device.dd_Library.lib_Flags |= LIBF_DELEXP;
        return (BPTR)0;
    }

    if (base->Ready) {
        if (base->IntServerAdded)
            Prm_RemIntServer(base->Board, &base->IntServer);

        free_queue_dma(&base->rxq);
        free_queue_dma(&base->txq);
    }

    if (UtilityBase) {
        CloseLibrary(UtilityBase);
        UtilityBase = NULL;
    }
    if (PrometheusBase) {
        CloseLibrary(PrometheusBase);
        PrometheusBase = NULL;
    }

    seglist = (BPTR)base->SegList;
    Remove((struct Node *)base);
    FreeMem((char *)base - base->Device.dd_Library.lib_NegSize,
            (ULONG)(base->Device.dd_Library.lib_NegSize +
                    base->Device.dd_Library.lib_PosSize));
    return seglist;
}

static VOID VNetOpen(struct IOSana2Req *io __asm("a1"),
                     ULONG unitnum __asm("d0"),
                     ULONG flags __asm("d1"),
                     struct VNetBase *base __asm("a6"))
{
    struct VNetUnit *unit = &base->Unit0;
    struct TagItem *tags;

    (void)flags;

    base->Device.dd_Library.lib_OpenCnt++;

    if (!base->Ready) {
        KPrintF((CONST_STRPTR)"VNETDEV: open: FAIL device not ready (no "
                "virtio-net board found at DevInit -- normal on a "
                "machine without one)\n");
        io->ios2_Req.io_Error = IOERR_OPENFAIL;
        io->ios2_Req.io_Device = NULL;
        base->Device.dd_Library.lib_OpenCnt--;
        return;
    }

    if (unitnum != 0) {
        KPrintF((CONST_STRPTR)"VNETDEV: open: FAIL unit %ld not supported "
                "(unit 0 only)\n", (LONG)unitnum);
        io->ios2_Req.io_Error = IOERR_OPENFAIL;
        io->ios2_Req.io_Device = NULL;
        base->Device.dd_Library.lib_OpenCnt--;
        return;
    }

    if (unit->opencnt != 0) {
        /* Single-opener scope (recorded in this file's own header
         * comment and virtionet_device.h's struct VNetUnit comment). */
        KPrintF((CONST_STRPTR)"VNETDEV: open: FAIL unit 0 already open "
                "(single-opener scope)\n");
        io->ios2_Req.io_Error = IOERR_UNITBUSY;
        io->ios2_Req.io_Device = NULL;
        base->Device.dd_Library.lib_OpenCnt--;
        return;
    }

    tags = (struct TagItem *)io->ios2_BufferManagement;
    unit->copyToBuff   = (SANA2_COPY_FUNC)find_tag_ptr(tags, SANA2_CopyToBuff);
    unit->copyFromBuff = (SANA2_COPY_FUNC)find_tag_ptr(tags, SANA2_CopyFromBuff);

    if (!unit->copyToBuff || !unit->copyFromBuff) {
        KPrintF((CONST_STRPTR)"VNETDEV: open: FAIL SANA2_CopyToBuff/"
                "SANA2_CopyFromBuff not supplied via ios2_BufferManagement "
                "-- required for tx/rx to work at all\n");
        io->ios2_Req.io_Error = IOERR_OPENFAIL;
        io->ios2_Req.io_Device = NULL;
        base->Device.dd_Library.lib_OpenCnt--;
        return;
    }

    /* Per SANA-II convention (~/src/sana2loop's docs/sana2-notes.md
     * "Buffer management hook" point 3): Open() REPLACES
     * ios2_BufferManagement with its own per-open cookie once the tags
     * have been parsed out of it -- the caller is contractually required
     * to copy that cookie into every subsequent IOSana2Req on this open.
     * This driver's single-opener scope means BeginIO never needs to
     * read the cookie back (there is only ever one unit, one opener, no
     * side table to key by it) -- but setting it is what conformance
     * requires regardless of whether this driver's own dispatch happens
     * to need it. The unit itself is a fine cookie (this driver has
     * nothing per-open beyond the unit). */
    io->ios2_BufferManagement = (APTR)unit;

    NewList(&unit->pendingReads);
    unit->opencnt = 1;
    unit->configured = FALSE;
    unit->online = FALSE;
    unit->packetsReceived = 0;
    unit->packetsSent = 0;
    unit->rxDropped = 0;

    /* currentMac (re)seeded from factoryMac at every 0->1 open, mirroring
     * loopback.device's reset_unit() -- explicit byte assignment, not a
     * loop (see fill_sana2_addr's own comment on why). */
    unit->currentMac[0] = unit->factoryMac[0];
    unit->currentMac[1] = unit->factoryMac[1];
    unit->currentMac[2] = unit->factoryMac[2];
    unit->currentMac[3] = unit->factoryMac[3];
    unit->currentMac[4] = unit->factoryMac[4];
    unit->currentMac[5] = unit->factoryMac[5];

    io->ios2_Req.io_Device = (struct Device *)base;
    io->ios2_Req.io_Unit = (struct Unit *)unit;
    io->ios2_Req.io_Error = 0;

    base->Device.dd_Library.lib_Flags &= ~LIBF_DELEXP;

    KPrintF((CONST_STRPTR)"VNETDEV: open: PASS unit 0 opened, MAC "
            "%02lx:%02lx:%02lx:%02lx:%02lx:%02lx\n",
            (ULONG)unit->currentMac[0], (ULONG)unit->currentMac[1],
            (ULONG)unit->currentMac[2], (ULONG)unit->currentMac[3],
            (ULONG)unit->currentMac[4], (ULONG)unit->currentMac[5]);
}

static BPTR VNetClose(struct IOSana2Req *io __asm("a1"),
                      struct VNetBase *base __asm("a6"))
{
    struct VNetUnit *unit = (struct VNetUnit *)io->ios2_Req.io_Unit;

    if (unit) {
        struct IOSana2Req *pending;

        Disable();
        unit->online = FALSE;
        unit->opencnt = 0;
        while ((pending = pending_head(unit)) != NULL) {
            Remove((struct Node *)pending);
            pending->ios2_Req.io_Error = IOERR_ABORTED;
            ReplyMsg(&pending->ios2_Req.io_Message);
        }
        Enable();
    }

    io->ios2_Req.io_Device = NULL;
    base->Device.dd_Library.lib_OpenCnt--;

    KPrintF((CONST_STRPTR)"VNETDEV: close: unit 0 closed\n");

    if (base->Device.dd_Library.lib_OpenCnt == 0 &&
        (base->Device.dd_Library.lib_Flags & LIBF_DELEXP))
        return VNetExpungeInternal(base);

    return (BPTR)0;
}

static BPTR VNetExpunge(struct VNetBase *base __asm("a6"))
{
    return VNetExpungeInternal(base);
}

static ULONG VNetReserved(void)
{
    return 0;
}

static struct VNetBase *
DevInit(struct VNetBase *base __asm("d0"),
        BPTR             seglist __asm("a0"),
        struct ExecBase *sysbase __asm("a6"))
{
    ULONG featDev0, featDev1, featDrv0, featDrv1;
    UBYTE statusReadback;
    struct TagItem findTags[3];
    LONG i;

    SysBase = sysbase;
    base->ExecBase = sysbase;
    base->SegList = (APTR)seglist;

    KPrintF((CONST_STRPTR)"VNETDEV: DevInit entry\n");

    PrometheusBase = OpenLibrary((CONST_STRPTR)"prometheus.library", 2);
    if (!PrometheusBase) {
        KPrintF((CONST_STRPTR)"VNETDEV: DevInit: FAIL prometheus.library "
                "not available\n");
        return NULL;
    }

    UtilityBase = OpenLibrary((CONST_STRPTR)"utility.library", 36);
    if (!UtilityBase) {
        KPrintF((CONST_STRPTR)"VNETDEV: DevInit: FAIL utility.library "
                "open failed\n");
        CloseLibrary(PrometheusBase);
        PrometheusBase = NULL;
        return NULL;
    }

    findTags[0].ti_Tag = PRM_Vendor; findTags[0].ti_Data = VNET_PCI_VENDOR;
    findTags[1].ti_Tag = PRM_Device; findTags[1].ti_Data = VNET_PCI_DEVICE;
    findTags[2].ti_Tag = TAG_DONE;   findTags[2].ti_Data = 0;

    base->Board = Prm_FindBoardTagList(NULL, findTags);
    if (!base->Board) {
        KPrintF((CONST_STRPTR)"VNETDEV: DevInit: FAIL no board matched "
                "vendor $%04lx device $%04lx -- normal on a machine "
                "without one\n",
                (ULONG)VNET_PCI_VENDOR, (ULONG)VNET_PCI_DEVICE);
        goto fail_close_libs;
    }

    {
        ULONG memaddr0 = 0;
        struct TagItem attrs[2];
        attrs[0].ti_Tag = PRM_MemoryAddr0; attrs[0].ti_Data = (ULONG)&memaddr0;
        attrs[1].ti_Tag = TAG_DONE; attrs[1].ti_Data = 0;
        Prm_GetBoardAttrsTagList(base->Board, attrs);
        if (memaddr0 == 0) {
            KPrintF((CONST_STRPTR)"VNETDEV: DevInit: FAIL PRM_MemoryAddr0 "
                    "is zero (no aperture assigned)\n");
            goto fail_close_libs;
        }
        base->ApertureAddr = (APTR)memaddr0;
    }

    KPrintF((CONST_STRPTR)"VNETDEV: DevInit: board $%08lx, aperture "
            "$%08lx\n", (ULONG)base->Board, (ULONG)base->ApertureAddr);

    if (!find_virtio_caps(base, base->Board, (ULONG)base->ApertureAddr))
        goto fail_close_libs;

    /* ---- virtio 1.x init dance (OASIS virtio 1.x spec section 3.1.1) - */

    wr_u8(base->CommonCfg + VCFG_DEVICE_STATUS, 0); /* reset */
    for (i = 0; i < 100000; i++) {
        if (rd_u8(base->CommonCfg + VCFG_DEVICE_STATUS) == 0)
            break;
    }
    if (rd_u8(base->CommonCfg + VCFG_DEVICE_STATUS) != 0) {
        KPrintF((CONST_STRPTR)"VNETDEV: DevInit: FAIL device did not "
                "reset (status still $%02lx)\n",
                (ULONG)rd_u8(base->CommonCfg + VCFG_DEVICE_STATUS));
        goto fail_close_libs;
    }

    wr_u8(base->CommonCfg + VCFG_DEVICE_STATUS, VIRTIO_STATUS_ACKNOWLEDGE);
    wr_u8(base->CommonCfg + VCFG_DEVICE_STATUS,
          VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);

    wr_le32(base->CommonCfg + VCFG_DEVICE_FEATURE_SELECT,
            VIRTIO_NET_F_MAC_SELECT);
    featDev0 = rd_le32(base->CommonCfg + VCFG_DEVICE_FEATURE);
    wr_le32(base->CommonCfg + VCFG_DEVICE_FEATURE_SELECT,
            VIRTIO_F_VERSION_1_SELECT);
    featDev1 = rd_le32(base->CommonCfg + VCFG_DEVICE_FEATURE);

    if (!(featDev1 & (1UL << VIRTIO_F_VERSION_1_BIT))) {
        KPrintF((CONST_STRPTR)"VNETDEV: DevInit: FAIL device does not "
                "offer VIRTIO_F_VERSION_1 (feature word 1 = $%08lx)\n",
                featDev1);
        goto fail_reset;
    }
    if (!(featDev0 & (1UL << VIRTIO_NET_F_MAC_BIT))) {
        KPrintF((CONST_STRPTR)"VNETDEV: DevInit: FAIL device does not "
                "offer VIRTIO_NET_F_MAC (feature word 0 = $%08lx)\n",
                featDev0);
        goto fail_reset;
    }

    featDrv0 = featDev0 & (1UL << VIRTIO_NET_F_MAC_BIT);
    featDrv1 = featDev1 & (1UL << VIRTIO_F_VERSION_1_BIT);

    wr_le32(base->CommonCfg + VCFG_DRIVER_FEATURE_SELECT,
            VIRTIO_NET_F_MAC_SELECT);
    wr_le32(base->CommonCfg + VCFG_DRIVER_FEATURE, featDrv0);
    wr_le32(base->CommonCfg + VCFG_DRIVER_FEATURE_SELECT,
            VIRTIO_F_VERSION_1_SELECT);
    wr_le32(base->CommonCfg + VCFG_DRIVER_FEATURE, featDrv1);

    wr_u8(base->CommonCfg + VCFG_DEVICE_STATUS,
          VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER |
          VIRTIO_STATUS_FEATURES_OK);
    statusReadback = rd_u8(base->CommonCfg + VCFG_DEVICE_STATUS);
    if (!(statusReadback & VIRTIO_STATUS_FEATURES_OK)) {
        KPrintF((CONST_STRPTR)"VNETDEV: DevInit: FAIL device rejected "
                "feature negotiation (status $%02lx after FEATURES_OK)\n",
                (ULONG)statusReadback);
        wr_u8(base->CommonCfg + VCFG_DEVICE_STATUS, VIRTIO_STATUS_FAILED);
        goto fail_close_libs;
    }
    KPrintF((CONST_STRPTR)"VNETDEV: DevInit: features negotiated "
            "(VERSION_1, NET_F_MAC)\n");

    if (!setup_queue(base, VNET_QUEUE_RX, &base->rxq) ||
        !setup_queue(base, VNET_QUEUE_TX, &base->txq)) {
        wr_u8(base->CommonCfg + VCFG_DEVICE_STATUS, VIRTIO_STATUS_FAILED);
        goto fail_close_libs;
    }

    /* factoryMac only -- currentMac is (re)seeded from it at every 0->1
     * Open (VNetOpen), mirroring loopback.device's reset_unit(). */
    for (i = 0; i < 6; i++)
        base->Unit0.factoryMac[i] = rd_u8(base->DeviceCfg + VNETCFG_MAC + i);
    KPrintF((CONST_STRPTR)"VNETDEV: DevInit: MAC "
            "%02lx:%02lx:%02lx:%02lx:%02lx:%02lx\n",
            (ULONG)base->Unit0.factoryMac[0], (ULONG)base->Unit0.factoryMac[1],
            (ULONG)base->Unit0.factoryMac[2], (ULONG)base->Unit0.factoryMac[3],
            (ULONG)base->Unit0.factoryMac[4], (ULONG)base->Unit0.factoryMac[5]);

    if (!init_tx_slots(base) || !post_rx_buffers(base)) {
        wr_u8(base->CommonCfg + VCFG_DEVICE_STATUS, VIRTIO_STATUS_FAILED);
        goto fail_close_libs;
    }

    NewList(&base->Unit0.pendingReads);

    base->IntServer.is_Node.ln_Type = NT_INTERRUPT;
    base->IntServer.is_Node.ln_Pri  = 0;
    base->IntServer.is_Node.ln_Name = (char *)"virtionet.device";
    base->IntServer.is_Data = (APTR)base;
    base->IntServer.is_Code = (VOID (*)())vnet_int_server;

    if (!Prm_AddIntServer(base->Board, &base->IntServer)) {
        KPrintF((CONST_STRPTR)"VNETDEV: DevInit: FAIL Prm_AddIntServer "
                "refused\n");
        wr_u8(base->CommonCfg + VCFG_DEVICE_STATUS, VIRTIO_STATUS_FAILED);
        goto fail_close_libs;
    }
    base->IntServerAdded = TRUE;

    wr_u8(base->CommonCfg + VCFG_DEVICE_STATUS,
          VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER |
          VIRTIO_STATUS_FEATURES_OK | VIRTIO_STATUS_DRIVER_OK);

    base->Ready = TRUE;
    KPrintF((CONST_STRPTR)"VNETDEV: DevInit complete, DRIVER_OK set\n");
    return base;

fail_reset:
    wr_u8(base->CommonCfg + VCFG_DEVICE_STATUS, 0);
fail_close_libs:
    /* Failure-path DMA cleanup is best-effort/partial here, mirroring
     * prometheus_library.c's own LibInit: a DevInit failure is the
     * "board absent or misbehaving" case, not a steady-state path this
     * driver is expected to recover cleanly from, and exec frees the
     * device base memory itself once this returns NULL. */
    if (UtilityBase) { CloseLibrary(UtilityBase); UtilityBase = NULL; }
    if (PrometheusBase) { CloseLibrary(PrometheusBase); PrometheusBase = NULL; }
    return NULL;
}

static const APTR FuncTab[] = {
    (APTR)VNetOpen,
    (APTR)VNetClose,
    (APTR)VNetExpunge,
    (APTR)VNetReserved,
    (APTR)VNetBeginIO,
    (APTR)VNetAbortIO,
    (APTR)-1
};

static const ULONG InitTab[4] = {
    sizeof(struct VNetBase),
    (ULONG)FuncTab,
    (ULONG)0,
    (ULONG)DevInit
};

static const struct Resident ROMTag;   /* self-ref for rt_MatchTag */

static const struct Resident ROMTag = {
    RTC_MATCHWORD,
    (struct Resident *)&ROMTag,
    (APTR)(&ROMTag + 1),
    RTF_AUTOINIT,
    VNETDEV_VERSION,
    NT_DEVICE,
    0,
    (char *)DevName,
    (char *)DevIdString,
    (APTR)InitTab
};
