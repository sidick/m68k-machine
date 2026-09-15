/*
 * pcibridge_card.h -- the guest-side view of the pcibridge Zorro III
 * shim's register file (docs/pcibridge-protocol.md, protocol VERSION 2).
 *
 * Shared by prometheus.library (m68k/prometheus-library/) and the
 * PCIProbe tool (m68k/pciprobe/). Register semantics live in the
 * protocol document; this header only names the offsets and values so
 * the two guest halves cannot drift apart.
 *
 * Register idiom (same as every native card here, see
 * m68k/rtgboard-card/rtgboard_card.c's file-top comment): the bus is
 * byte-granular for the register file, u32 registers are four
 * big-endian byte lanes so a 68k move.l works directly, and byte-wide
 * registers respond only at the LOW byte of their 4-byte slot -- i.e.
 * at offset+3.
 *
 * Never hardcode the board's address: find it with
 * FindConfigDev(PCIBRIDGE_MANUFACTURER, PCIBRIDGE_PRODUCT) and use
 * cd_BoardAddr (docs/device-ledger.md, the rule for addresses).
 */

#ifndef PCIBRIDGE_CARD_H
#define PCIBRIDGE_CARD_H

/* AUTOCONFIG identity (docs/pcibridge-protocol.md section 1). */
#define PCIBRIDGE_MANUFACTURER 0x07DB
#define PCIBRIDGE_PRODUCT      6

/* The protocol version this header describes. prometheus.library
 * refuses any other value read back from PCIB_VERSION: the INTx
 * registers below are part of its contract, so a version-1 card is
 * positively rejected rather than half-used. */
#define PCIBRIDGE_PROTOCOL_VERSION 2

/* Register file offsets from the board base (u32 unless noted). */
#define PCIB_VERSION       0x00 /* R:  protocol version */
#define PCIB_CFG_ADDR      0x04 /* RW: bus<<20 | dev<<15 | fn<<12 | offset */
#define PCIB_CFG_WIDTH     0x08 /* RW byte (+3): access size 1, 2 or 4 */
#define PCIB_CFG_DATA      0x0C /* RW: staged write data / latched result */
#define PCIB_CFG_OP        0x10 /* W  byte (+3): 0 = read, 1 = write */
#define PCIB_CFG_STATUS    0x14 /* RW byte (+3): write-1-to-clear */
#define PCIB_APERTURE_BASE 0x18 /* RW: PCI address the aperture maps to */
#define PCIB_INTX_STATUS   0x1C /* R:  live INTA-D levels, bits 0-3 */
#define PCIB_INTX_ENABLE   0x20 /* RW: INT2 mask for those bits */
#define PCIB_INTX_TEST     0x24 /* RW: diagnostic line assertion, bits 0-3 */

/* CFG_OP values. */
#define PCIB_OP_READ  0
#define PCIB_OP_WRITE 1

/* CFG_STATUS bits (mutually exclusive per op; W1C, ungated). */
#define PCIB_STATUS_REJECTED  0x01
#define PCIB_STATUS_COMPLETED 0x02

/* INTX_STATUS / INTX_ENABLE / INTX_TEST line bits. */
#define PCIB_INTX_A 0x01
#define PCIB_INTX_B 0x02
#define PCIB_INTX_C 0x04
#define PCIB_INTX_D 0x08

/* The BAR aperture: upper 8 MB of the 16 MB board window. An access at
 * board base + PCIB_APERTURE_OFFSET + k is a PCI memory-space access at
 * APERTURE_BASE + k, address-invariant at every width -- multi-byte
 * loads see little-endian device registers byte-swapped, and the
 * DRIVER swaps, exactly as on the real Prometheus bridge. */
#define PCIB_APERTURE_OFFSET 0x800000UL
#define PCIB_APERTURE_BYTES  0x800000UL

/* prometheus.library's BAR allocation policy (docs/pci-library.md
 * section 2): 32-bit memory BARs assigned ascending from this PCI
 * memory-space base, the aperture banked there once at library init.
 * This is a PCI-bus-space policy constant, not a guest address --
 * everything CPU-visible still derives from the ConfigDev. */
#define PCIB_PCI_MEM_BASE  0x20000000UL
#define PCIB_PCI_MEM_BYTES PCIB_APERTURE_BYTES

#endif /* PCIBRIDGE_CARD_H */
