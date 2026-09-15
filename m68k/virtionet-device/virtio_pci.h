/*
 * virtio_pci.h -- virtio 1.x ("modern") PCI transport layout constants:
 * capability structure fields, the common configuration structure, the
 * split virtqueue layout, and virtio-net's own feature bits/config
 * layout/packet header. Reference: OASIS Virtio 1.x specification,
 * sections 4.1 (Virtio Over PCI Bus) and 5.1 (Network Device).
 *
 * Shared by virtionet_device.c and vnettest.c only insofar as vnettest
 * never touches these directly -- it talks to virtionet.device purely
 * through the SANA-II API. This header exists for virtionet_device.c.
 *
 * BYTE ORDER: every multi-byte field described here lives in the BAR0
 * aperture, which is address-invariant (docs/pcibridge-protocol.md
 * section 5) -- a big-endian CPU's natural-width load of a little-endian
 * virtio field comes back byte-swapped. This header only names offsets;
 * virtionet_device.c's rd_le16/rd_le32/wr_le16/wr_le32 helpers do the
 * swap, once, at every access. Config-space fields (the capability walk
 * itself, read via prometheus.library's Prm_ReadConfigByte/Word/Long) are
 * a completely different byte-order regime (docs/pci-library.md section
 * 3) and are not covered by this header's offsets.
 */

#ifndef VIRTIO_PCI_H
#define VIRTIO_PCI_H

/* ---- PCI capability list (config space, walked via prometheus.library)
 * -- docs/pcibridge-protocol.md section 2 / virtio spec 4.1.4 --------- */

#define PCI_CAP_ID_VNDR 0x09U /* vendor-specific capability */

/* Byte offsets WITHIN one virtio_pci_cap capability structure (a plain
 * PCI config-space structure -- read with the p^3/p^2 byte-order-trap-1
 * transforms in virtionet_device.c, never with the aperture helpers). */
#define VIRTIO_PCI_CAP_VNDR_OFF      0x00 /* u8 */
#define VIRTIO_PCI_CAP_NEXT_OFF      0x01 /* u8 */
#define VIRTIO_PCI_CAP_LEN_OFF       0x02 /* u8 */
#define VIRTIO_PCI_CAP_CFGTYPE_OFF   0x03 /* u8 */
#define VIRTIO_PCI_CAP_BAR_OFF       0x04 /* u8 */
/* offsets 0x05-0x07: padding */
#define VIRTIO_PCI_CAP_OFFSET_OFF    0x08 /* u32, plain aligned config long */
#define VIRTIO_PCI_CAP_LENGTH_OFF    0x0C /* u32, plain aligned config long */
/* VIRTIO_PCI_CAP_NOTIFY_CFG only: */
#define VIRTIO_PCI_NOTIFY_CAP_MULT_OFF 0x10 /* u32, plain aligned config long */

#define VIRTIO_PCI_CAP_COMMON_CFG 1
#define VIRTIO_PCI_CAP_NOTIFY_CFG 2
#define VIRTIO_PCI_CAP_ISR_CFG    3
#define VIRTIO_PCI_CAP_DEVICE_CFG 4
#define VIRTIO_PCI_CAP_PCI_CFG    5

/* PCI type 0 header field this driver needs directly (plain config byte,
 * same p^3 transform): capabilities pointer. */
#define PCICFG_CAP_PTR 0x34

/* ---- common configuration structure (accessed through the aperture,
 * every multi-byte field little-endian and swapped by the rd/wr le16/
 * le32 helpers) -- virtio spec 4.1.4.3 ---------------------------------- */

#define VCFG_DEVICE_FEATURE_SELECT 0x00 /* le32, W */
#define VCFG_DEVICE_FEATURE        0x04 /* le32, R */
#define VCFG_DRIVER_FEATURE_SELECT 0x08 /* le32, W */
#define VCFG_DRIVER_FEATURE        0x0C /* le32, W */
#define VCFG_MSIX_CONFIG           0x10 /* le16, RW */
#define VCFG_NUM_QUEUES            0x12 /* le16, R */
#define VCFG_DEVICE_STATUS         0x14 /* u8, RW -- plain byte, no swap */
#define VCFG_CONFIG_GENERATION     0x15 /* u8, R */
#define VCFG_QUEUE_SELECT          0x16 /* le16, W */
#define VCFG_QUEUE_SIZE            0x18 /* le16, RW */
#define VCFG_QUEUE_MSIX_VECTOR     0x1A /* le16, RW */
#define VCFG_QUEUE_ENABLE          0x1C /* le16, RW */
#define VCFG_QUEUE_NOTIFY_OFF      0x1E /* le16, R */
#define VCFG_QUEUE_DESC            0x20 /* le64, W */
#define VCFG_QUEUE_DRIVER          0x28 /* le64, W -- the avail ring */
#define VCFG_QUEUE_DEVICE          0x30 /* le64, W -- the used ring */

/* device_status bits (virtio spec 2.1). */
#define VIRTIO_STATUS_ACKNOWLEDGE 0x01
#define VIRTIO_STATUS_DRIVER      0x02
#define VIRTIO_STATUS_DRIVER_OK   0x04
#define VIRTIO_STATUS_FEATURES_OK 0x08
#define VIRTIO_STATUS_NEEDS_RESET 0x40
#define VIRTIO_STATUS_FAILED      0x80

/* Feature bits this driver negotiates -- selected 32-bit half (select
 * register value) and bit within that half. */
#define VIRTIO_NET_F_MAC_SELECT   0
#define VIRTIO_NET_F_MAC_BIT      5   /* feature bit 5 */
#define VIRTIO_F_VERSION_1_SELECT 1
#define VIRTIO_F_VERSION_1_BIT    0   /* feature bit 32, i.e. bit 0 of select=1 */

/* ---- ISR configuration (1 byte, read-to-clear) -- virtio spec 4.1.4.5 - */

#define VIRTIO_ISR_QUEUE  0x01 /* a used-ring entry became available */
#define VIRTIO_ISR_CONFIG 0x02 /* device configuration changed */

/* ---- device-specific configuration: virtio_net_config (virtio spec 5.1.4,
 * only the fields this driver reads) ------------------------------------ */

#define VNETCFG_MAC              0x00 /* u8[6], plain bytes, no swap */
#define VNETCFG_STATUS           0x06 /* le16 */
#define VNETCFG_MAX_VQ_PAIRS     0x08 /* le16 */
#define VNETCFG_MTU              0x0A /* le16 */

/* ---- virtio-net packet header (virtio spec 5.1.6) --------------------- *
 * VIRTIO_F_VERSION_1 negotiated => the 12-byte v1 header is used
 * unconditionally (num_buffers always present), even without
 * VIRTIO_NET_F_MRG_RXBUF (which this driver does not negotiate -- with
 * MRG_RXBUF absent, num_buffers must be exactly 1 on tx and is read but
 * ignored on rx, since this driver never chains rx buffers). All fields
 * little-endian; this driver writes/reads them via the same swap helpers,
 * even though the header lives in ordinary DMA memory rather than the
 * aperture -- DMA memory holds the device's own little-endian bytes just
 * as the aperture's registers do, so the same helpers apply unchanged. */
#define VNET_HDR_FLAGS       0x00 /* u8 */
#define VNET_HDR_GSO_TYPE    0x01 /* u8 */
#define VNET_HDR_HDR_LEN     0x02 /* le16 */
#define VNET_HDR_GSO_SIZE    0x04 /* le16 */
#define VNET_HDR_CSUM_START  0x06 /* le16 */
#define VNET_HDR_CSUM_OFFSET 0x08 /* le16 */
#define VNET_HDR_NUM_BUFFERS 0x0A /* le16 */
#define VNET_HDR_SIZE        12

#define VNET_GSO_NONE 0

/* ---- split virtqueue layout (virtio spec 2.6) -------------------------- */

#define VRING_DESC_SIZE   16
#define VRING_DESC_ADDR   0  /* le64 */
#define VRING_DESC_LEN    8  /* le32 */
#define VRING_DESC_FLAGS  12 /* le16 */
#define VRING_DESC_NEXT   14 /* le16 */

#define VRING_DESC_F_NEXT  0x0001
#define VRING_DESC_F_WRITE 0x0002

#define VRING_AVAIL_FLAGS 0 /* le16 */
#define VRING_AVAIL_IDX   2 /* le16 */
#define VRING_AVAIL_RING  4 /* le16[qsize], then (if negotiated) le16 used_event -- not negotiated here */

#define VRING_USED_FLAGS 0 /* le16 */
#define VRING_USED_IDX   2 /* le16 */
#define VRING_USED_RING  4 /* struct {le32 id; le32 len;}[qsize], 8 bytes each */
#define VRING_USED_ELEM_SIZE 8
#define VRING_USED_ELEM_ID  0
#define VRING_USED_ELEM_LEN 4

/* DMA alignment the spec requires per ring component (2.6.13): the
 * descriptor table 16 bytes, the driver (avail) area 2 bytes, the device
 * (used) area 4 bytes. Prm_AllocDMABuffer only longword-rounds
 * (docs/pci-library.md section 4), so virtionet_device.c over-allocates
 * and aligns each of these by hand. */
#define VRING_DESC_ALIGN  16
#define VRING_AVAIL_ALIGN 2
#define VRING_USED_ALIGN  4

/* Queue indices this device uses (host side: rx=0, tx=1, num_queues=2 --
 * task brief, matching virtio-net's standard single-queue-pair
 * numbering). */
#define VNET_QUEUE_RX 0
#define VNET_QUEUE_TX 1

#endif /* VIRTIO_PCI_H */
