/*
 * clib/prometheus_protos.h -- prometheus.library C prototypes.
 *
 * Written fresh for m68k-machine against the Matay Prometheus SDK 3.0
 * interface specification (docs/pci-library.md section 1). 32-bit
 * integers only.
 *
 * The SDK's varargs stubs (Prm_FindBoardTags and friends) are amiga.lib
 * style link-time stubs; this project's callers use the TagList forms
 * with stack arrays, so no varargs stubs are provided here. Period
 * driver BINARIES are unaffected -- they call LVOs directly.
 */

#ifndef CLIB_PROMETHEUS_PROTOS_H
#define CLIB_PROMETHEUS_PROTOS_H

#ifndef EXEC_INTERRUPTS_H
#include <exec/interrupts.h>
#endif

#ifndef EXEC_TYPES_H
#include <exec/types.h>
#endif

#ifndef LIBRARIES_PROMETHEUS_H
#include <libraries/prometheus.h>
#endif

#ifndef UTILITY_TAGITEM_H
#include <utility/tagitem.h>
#endif

/* --- V1 --- */

PCIBoard *Prm_FindBoardTagList(PCIBoard *previous, struct TagItem *taglist);
ULONG Prm_GetBoardAttrsTagList(PCIBoard *board, struct TagItem *taglist);

/* --- V2 --- */

ULONG Prm_ReadConfigLong(PCIBoard *board, UBYTE offset);
UWORD Prm_ReadConfigWord(PCIBoard *board, UBYTE offset);
UBYTE Prm_ReadConfigByte(PCIBoard *board, UBYTE offset);
VOID Prm_WriteConfigLong(PCIBoard *board, ULONG data, UBYTE offset);
VOID Prm_WriteConfigWord(PCIBoard *board, UWORD data, UBYTE offset);
VOID Prm_WriteConfigByte(PCIBoard *board, UBYTE data, UBYTE offset);
ULONG Prm_SetBoardAttrsTagList(PCIBoard *board, struct TagItem *taglist);
BOOL Prm_AddIntServer(PCIBoard *board, struct Interrupt *intr);
VOID Prm_RemIntServer(PCIBoard *board, struct Interrupt *intr);
APTR Prm_AllocDMABuffer(ULONG size);
VOID Prm_FreeDMABuffer(APTR buffer, ULONG size);
APTR Prm_GetPhysicalAddress(APTR addr);

/* --- V3 --- */

APTR Prm_GetVirtualAddress(APTR addr);

#endif /* CLIB_PROMETHEUS_PROTOS_H */
