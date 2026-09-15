/*
 * libraries/prometheus.h -- prometheus.library public interface.
 *
 * Written fresh for m68k-machine against the interface specification
 * in the Matay Prometheus SDK 3.0 (see docs/pci-library.md section 1
 * for provenance and the licensing firewall). The names and values
 * below are the published API surface period drivers compile against;
 * they must not drift from the SDK's.
 */

#ifndef LIBRARIES_PROMETHEUS_H
#define LIBRARIES_PROMETHEUS_H

#define PROMETHEUSNAME "prometheus.library"
#define PROMETHEUSVERSION 3
#define PROMETHEUSMINVERSION 0

#ifndef PCIBOARD_TYPEDEF
#define PCIBOARD_TYPEDEF
typedef VOID PCIBoard;
#endif /* PCIBOARD_TYPEDEF */

/* Tags for Prm_FindBoardTagList(), Prm_GetBoardAttrsTagList() and
 * Prm_SetBoardAttrsTagList(). 'S' settable, 'G' gettable. */

#define PRM_Vendor         0x6EDA0000 /* [.G] */
#define PRM_Device         0x6EDA0001 /* [.G] */
#define PRM_Revision       0x6EDA0002 /* [.G] */
#define PRM_Class          0x6EDA0003 /* [.G] */
#define PRM_SubClass       0x6EDA0004 /* [.G] */

/* The SDK guarantees the last nybble of PRM_MemoryAddrX and
 * PRM_MemorySizeX equals X. */

#define PRM_MemoryAddr0    0x6EDA0010 /* [.G] */
#define PRM_MemoryAddr1    0x6EDA0011 /* [.G] */
#define PRM_MemoryAddr2    0x6EDA0012 /* [.G] */
#define PRM_MemoryAddr3    0x6EDA0013 /* [.G] */
#define PRM_MemoryAddr4    0x6EDA0014 /* [.G] */
#define PRM_MemoryAddr5    0x6EDA0015 /* [.G] */
#define PRM_ROM_Address    0x6EDA0016 /* [.G] */

#define PRM_MemorySize0    0x6EDA0020 /* [.G] */
#define PRM_MemorySize1    0x6EDA0021 /* [.G] */
#define PRM_MemorySize2    0x6EDA0022 /* [.G] */
#define PRM_MemorySize3    0x6EDA0023 /* [.G] */
#define PRM_MemorySize4    0x6EDA0024 /* [.G] */
#define PRM_MemorySize5    0x6EDA0025 /* [.G] */
#define PRM_ROM_Size       0x6EDA0026 /* [.G] */

/* Tags added in V2 of the library. */

#define PRM_BoardOwner     0x6EDA0005 /* [SG] */
#define PRM_SlotNumber     0x6EDA0006 /* [.G] */
#define PRM_FunctionNumber 0x6EDA0007 /* [.G] */

#endif /* LIBRARIES_PROMETHEUS_H */
