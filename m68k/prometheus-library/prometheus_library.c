/*
 * prometheus_library.c -- prometheus.library: the guest-side
 * Prometheus-compatible PCI bus library for the `pcibridge` Zorro III
 * shim (docs/pcibridge-protocol.md), implementing the Matay Prometheus
 * SDK 3.0 API (docs/pci-library.md; provenance and licensing firewall
 * there in section 1).
 *
 * A standard Exec AUTOINIT disk library (LIBS:prometheus.library),
 * disk-loaded the same way any application's OpenLibary("prometheus.
 * library", ...) call triggers -- unlike m68k/rtgboard-card's .card
 * form (loaded by rtg.library's own two-phase FindCard/InitCard
 * protocol), a disk library's ROMTag init function (LibInit, below) is
 * the ONLY init hook exec calls: there is no separate "FindCard" phase
 * to defer discovery into, so every step docs/pci-library.md section 2
 * describes -- FindConfigDev, protocol-version check, bus enumeration,
 * BAR assignment, aperture programming -- happens inline inside
 * LibInit here. Everything else (the AUTOINIT/Open-Close-Expunge
 * boilerplate, the SysBase-must-be-defined-not-just-declared reasoning,
 * the CardBase-style "SegList is APTR, not BPTR" note, the FindConfigDev
 * claim-by-clearing-CDF_CONFIGME loop) is translated from
 * m68k/rtgboard-card/rtgboard_card.c, which itself translates it from
 * amirfb.card (~/src/amirfb/src/card/amirfb_card.c, BSD 2-Clause, same
 * owner).
 *
 * Register access idiom (docs/pcibridge-protocol.md section 4,
 * pcibridge_card.h's own file-top comment): u32 registers are four
 * big-endian byte lanes, so a plain 68k move.l at a u32 register's base
 * offset is already correct; byte-wide registers respond only at the
 * LOW byte of their 4-byte slot (offset+3).
 */

#include <exec/types.h>
#include <exec/resident.h>
#include <exec/libraries.h>
#include <exec/execbase.h>
#include <exec/memory.h>
#include <exec/interrupts.h>
#include <hardware/intbits.h>
#include <proto/exec.h>
#include <proto/expansion.h>
#include <libraries/configvars.h>
#include <clib/debug_protos.h>
#include <utility/tagitem.h>
#include <clib/utility_protos.h>

#include <libraries/prometheus.h>

#include "pcibridge_card.h"
#include "prometheus_library.h"

/* SysBase/ExpansionBase: <proto/exec.h>/<proto/expansion.h>'s inline
 * call macros only extern-declare these globals, expecting ixemul/
 * libnix startup code to define and set them -- there is none here
 * (-nostdlib, no startup code), so WE must provide the definitions.
 * SysBase is set in LibInit (identical pattern to rtgboard_card.c);
 * ExpansionBase is opened/closed entirely within the FindConfigDev step
 * below (nothing else in this library needs expansion.library). */
struct ExecBase *SysBase;
struct ExpansionBase *ExpansionBase;

/* utility.library: amiga.lib's own NextTagItem (the library-independent
 * "local copy" form used by proto/utility.h and every other library
 * here) turns out, on this toolchain, to still reference UtilityBase
 * (confirmed by the link failure without it) -- so it must be opened
 * and kept open for the library's lifetime, not just borrowed and
 * closed the way expansion.library is in LibInit. Closed in
 * LibExpungeInternal. */
struct Library *UtilityBase;

/* ---- identity ------------------------------------------------------- */

const char LibName[] = "prometheus.library";

/* Contract question decided, not guessed silently: the task brief gave
 * the id-string CONTENT ("prometheus.library 3.0 (15.9.2026)
 * m68k-machine\r\n") without the "$VER: " lead-in, but every sibling
 * library/card in this project (rtgboard_card.c's own LibIdString) and
 * the AmigaOS version-string convention itself (the string `version`
 * greps for) carry that prefix. Added it here rather than deviating
 * from every other id string in the tree. */
static const char LibIdString[] =
    "$VER: prometheus.library 3.0 (15.9.2026) m68k-machine\r\n";

#define LIBVERSION  3
#define LIBREVISION 0   /* never written explicitly: exec zeroes the
                          * library base before LibInit runs, and REVISION
                          * 0 is exactly that untouched lib_Revision. */

/* ---- PCI config-space layout constants used only during LibInit's own
 * enumeration/BAR-assignment walk (not part of the public API, so not
 * in prometheus_library.h) -------------------------------------------- */

#define PCICFG_VENDOR_DEVICE 0x00 /* u32: device<<16 | vendor */
#define PCICFG_COMMAND       0x04 /* u16 */
#define PCICFG_REVISION      0x08 /* u8 */
#define PCICFG_SUBCLASS      0x0A /* u8 */
#define PCICFG_CLASS         0x0B /* u8 */
#define PCICFG_HEADER_TYPE   0x0E /* u8 */
#define PCICFG_BAR0          0x10 /* u32, BAR indices 0-5 at +4 each */
#define PCICFG_INT_PIN       0x3D /* u8 */

#define PCI_CMD_IO      0x0001U
#define PCI_CMD_MEMORY  0x0002U
#define PCI_CMD_MASTER  0x0004U

#define PCI_HEADER_MULTIFUNCTION 0x80U

#define PCI_BAR_IO_SPACE     0x1U  /* bit 0 */
#define PCI_BAR_TYPE_SHIFT   1
#define PCI_BAR_TYPE_MASK    0x3U  /* bits 2:1 */
#define PCI_BAR_TYPE_64BIT   0x2U  /* bits 2:1 == 0b10 */

/* ---- the config-cycle helper (docs/pcibridge-protocol.md section 4,
 * the heart of this library) -------------------------------------------
 *
 * One helper doing the exact register sequence the protocol document
 * specifies, wrapped in Disable()/Enable() because the register file
 * (CFG_ADDR/CFG_WIDTH/CFG_OP/CFG_STATUS/CFG_DATA) is shared state: any
 * other task (or this task, reentered) touching the same registers
 * mid-cycle would corrupt both cycles.
 *
 * Returns the decoded VALUE (protocol section 3: "carrying register
 * values... never byte images") -- this is the presentation-neutral
 * form used by every internal caller (enumeration, BAR sizing, COMMAND
 * programming); the Prometheus byte-order presentation the public
 * Prm_ReadConfigX/Prm_WriteConfigX functions expose (docs/pci-
 * library.md section 3) is applied entirely by transforming the OFFSET
 * before it ever reaches here -- this function does not know about it
 * and must not be "tidied" to apply any swap. */
static ULONG cfg_cycle(struct PrometheusBase *base,
                       UBYTE bus, UBYTE dev, UBYTE fn,
                       UWORD offset, UBYTE width, UBYTE op, ULONG wdata)
{
    UBYTE *regs = base->RegisterBase;
    ULONG addr = ((ULONG)bus << 20) | ((ULONG)dev << 15) |
                 ((ULONG)fn << 12) | ((ULONG)offset & 0x0FFFUL);
    UBYTE status;
    ULONG result = 0xFFFFFFFFUL;

    Disable();

    if (op == PCIB_OP_WRITE)
        *(volatile ULONG *)(regs + PCIB_CFG_DATA) = wdata;

    *(volatile ULONG *)(regs + PCIB_CFG_ADDR) = addr;
    *(volatile UBYTE *)(regs + PCIB_CFG_WIDTH + 3) = width;
    *(volatile UBYTE *)(regs + PCIB_CFG_OP + 3) = op;

    status = *(volatile UBYTE *)(regs + PCIB_CFG_STATUS + 3);

    if (status & PCIB_STATUS_REJECTED) {
        /* Should be impossible with valid parameters (docs/pci-
         * library.md section 4) -- every internal call site below
         * constructs bus/dev/fn/offset/width from values this
         * library's own enumeration already validated, and REJECTED
         * only fires for reserved-bit/unknown-op/unknown-width/
         * misalignment mistakes a correct driver never makes. Narrate
         * loudly rather than silently returning a plausible-looking
         * value. */
        KPrintF((CONST_STRPTR)"prometheus.library: CFG_OP REJECTED for "
                "%02lx:%02lx.%lx offset $%03lx width %ld op %ld -- "
                "should be impossible with valid parameters\n",
                (ULONG)bus, (ULONG)dev, (ULONG)fn, (ULONG)offset,
                (ULONG)width, (ULONG)op);
        result = 0xFFFFFFFFUL;
    } else if (op == PCIB_OP_READ) {
        result = *(volatile ULONG *)(regs + PCIB_CFG_DATA);
    }

    /* W1C, ungated (docs/pcibridge-protocol.md section 4). */
    *(volatile UBYTE *)(regs + PCIB_CFG_STATUS + 3) = 0xFF;

    Enable();

    return result;
}

/* ---- LibInit's enumeration + BAR assignment helpers ------------------ */

static ULONG board_cpu_addr(struct PrometheusBase *base, ULONG pci)
{
    return (ULONG)base->ApertureCpuBase + (pci - PCIB_PCI_MEM_BASE);
}

/* Sizes and assigns every 32-bit memory BAR of one function, ascending
 * from *next_free (shared across the whole bus scan -- docs/pci-
 * library.md section 2: "allocated ascending... from the PCI memory
 * region"). Returns nothing; records results directly into f->BarPci/
 * BarSize (both left 0 -- the AllocMem'd array starts zeroed -- for
 * every BAR this function leaves unassigned). */
static void assign_bars(struct PrometheusBase *base, struct PciFunction *f,
                        ULONG *next_free)
{
    UWORD cmd;
    int i;

    /* Clear COMMAND memory-decode during sizing (docs/pci-library.md
     * section 2): a live memory-mapped BAR responding to bus traffic
     * mid-sizing-probe would be reading real device state instead of
     * plain address-mask bits back. */
    cmd = (UWORD)cfg_cycle(base, f->Bus, f->Dev, f->Fn,
                           PCICFG_COMMAND, 2, PCIB_OP_READ, 0);
    cfg_cycle(base, f->Bus, f->Dev, f->Fn, PCICFG_COMMAND, 2,
              PCIB_OP_WRITE, (ULONG)(cmd & ~(UWORD)PCI_CMD_MEMORY));

    for (i = 0; i < 6; i++) {
        UWORD off = (UWORD)(PCICFG_BAR0 + i * 4);
        ULONG orig, probed, mask, size, aligned;

        orig = cfg_cycle(base, f->Bus, f->Dev, f->Fn, off, 4,
                         PCIB_OP_READ, 0);

        if (orig & PCI_BAR_IO_SPACE) {
            /* I/O BAR: never assigned (no I/O aperture on this
             * machine, docs/pci-library.md section 2). */
            continue;
        }

        if (((orig >> PCI_BAR_TYPE_SHIFT) & PCI_BAR_TYPE_MASK) ==
            PCI_BAR_TYPE_64BIT) {
            KPrintF((CONST_STRPTR)"prometheus.library: %02lx:%02lx.%lx "
                    "BAR%ld is 64-bit -- unassignable, left "
                    "unassigned\n",
                    (ULONG)f->Bus, (ULONG)f->Dev, (ULONG)f->Fn, (LONG)i);
            i++; /* skip the upper-half index too */
            continue;
        }

        cfg_cycle(base, f->Bus, f->Dev, f->Fn, off, 4,
                  PCIB_OP_WRITE, 0xFFFFFFFFUL);
        probed = cfg_cycle(base, f->Bus, f->Dev, f->Fn, off, 4,
                           PCIB_OP_READ, 0);
        cfg_cycle(base, f->Bus, f->Dev, f->Fn, off, 4, PCIB_OP_WRITE, orig);

        mask = probed & ~0xFUL;
        if (mask == 0)
            continue; /* unimplemented */

        size = (~mask) + 1UL; /* 32-bit arithmetic, per docs/pci-library.md */

        aligned = (*next_free + (size - 1UL)) & ~(size - 1UL);

        if (aligned + size > PCIB_PCI_MEM_BASE + PCIB_PCI_MEM_BYTES ||
            aligned + size < aligned /* defensive: cannot actually wrap
                                       * given 32-bit PCIB_PCI_MEM_BYTES,
                                       * but never trust unchecked
                                       * arithmetic on this platform */) {
            KPrintF((CONST_STRPTR)"prometheus.library: %02lx:%02lx.%lx "
                    "BAR%ld (size $%06lx) would exceed the PCI memory "
                    "region -- left unassigned, never truncated\n",
                    (ULONG)f->Bus, (ULONG)f->Dev, (ULONG)f->Fn, (LONG)i,
                    (ULONG)size);
            continue;
        }

        f->BarPci[i] = aligned;
        f->BarSize[i] = size;
        *next_free = aligned + size;

        cfg_cycle(base, f->Bus, f->Dev, f->Fn, off, 4, PCIB_OP_WRITE, aligned);

        KPrintF((CONST_STRPTR)"prometheus.library: %02lx:%02lx.%lx BAR%ld "
                "-> PCI $%08lx size $%06lx\n",
                (ULONG)f->Bus, (ULONG)f->Dev, (ULONG)f->Fn, (LONG)i,
                (ULONG)aligned, (ULONG)size);
    }

    /* After a function's BARs: COMMAND = memory-decode | bus-master
     * (docs/pci-library.md section 2, taken as the literal final value
     * -- this policy assigns no I/O BARs and this library defines no
     * other COMMAND bit any driver here depends on, so there is
     * nothing worth preserving from the original read). */
    cfg_cycle(base, f->Bus, f->Dev, f->Fn, PCICFG_COMMAND, 2, PCIB_OP_WRITE,
              (ULONG)(PCI_CMD_MEMORY | PCI_CMD_MASTER));
}

/* Records one live function (vendor != 0xFFFF already confirmed by the
 * caller) and immediately assigns its BARs, ascending *next_free
 * across the whole bus scan. Returns FALSE only if the board cap was
 * already reached (enumeration stops, narrated once by the caller). */
static BOOL record_function(struct PrometheusBase *base, UBYTE bus,
                            UBYTE dev, UBYTE fn, ULONG *next_free)
{
    struct PciFunction *f;
    ULONG vendev;

    if (base->BoardCount >= PRM_MAX_BOARDS)
        return FALSE;

    f = &base->Boards[base->BoardCount++];
    f->Bus = bus;
    f->Dev = dev;
    f->Fn = fn;

    vendev = cfg_cycle(base, bus, dev, fn, PCICFG_VENDOR_DEVICE, 4,
                       PCIB_OP_READ, 0);
    f->Vendor = (UWORD)(vendev & 0xFFFFUL);
    f->Device = (UWORD)((vendev >> 16) & 0xFFFFUL);
    f->Revision = (UBYTE)cfg_cycle(base, bus, dev, fn, PCICFG_REVISION, 1,
                                   PCIB_OP_READ, 0);
    f->Class = (UBYTE)cfg_cycle(base, bus, dev, fn, PCICFG_CLASS, 1,
                                PCIB_OP_READ, 0);
    f->SubClass = (UBYTE)cfg_cycle(base, bus, dev, fn, PCICFG_SUBCLASS, 1,
                                   PCIB_OP_READ, 0);
    f->IntPin = (UBYTE)cfg_cycle(base, bus, dev, fn, PCICFG_INT_PIN, 1,
                                 PCIB_OP_READ, 0);
    f->Owner = NULL;

    KPrintF((CONST_STRPTR)"prometheus.library: found %04lx:%04lx at "
            "%02lx:%02lx.%lx\n",
            (ULONG)f->Vendor, (ULONG)f->Device,
            (ULONG)bus, (ULONG)dev, (ULONG)fn);

    assign_bars(base, f, next_free);

    return TRUE;
}

/* Bus 0 enumeration (docs/pci-library.md section 2 point 2 / docs/
 * pcibridge-protocol.md's own "flat bus only" note): devices 0-31,
 * function 0 always probed, functions 1-7 only when the header-type
 * multifunction bit says so. */
static void enumerate_bus0(struct PrometheusBase *base)
{
    ULONG next_free = PCIB_PCI_MEM_BASE;
    UWORD dev;

    for (dev = 0; dev < 32; dev++) {
        ULONG vendev0;
        UWORD vendor0;
        UBYTE header_type;

        vendev0 = cfg_cycle(base, 0, (UBYTE)dev, 0, PCICFG_VENDOR_DEVICE, 4,
                            PCIB_OP_READ, 0);
        vendor0 = (UWORD)(vendev0 & 0xFFFFUL);

        if (vendor0 == 0xFFFF)
            continue; /* absent -- master-abort contract */

        if (!record_function(base, 0, (UBYTE)dev, 0, &next_free)) {
            KPrintF((CONST_STRPTR)"prometheus.library: board cap (%ld) "
                    "reached -- further bus 0 functions are not "
                    "recorded\n", (LONG)PRM_MAX_BOARDS);
            return;
        }

        header_type = (UBYTE)cfg_cycle(base, 0, (UBYTE)dev, 0,
                                       PCICFG_HEADER_TYPE, 1,
                                       PCIB_OP_READ, 0);
        if (!(header_type & PCI_HEADER_MULTIFUNCTION))
            continue;

        {
            UBYTE fn;
            for (fn = 1; fn < 8; fn++) {
                ULONG vendevN = cfg_cycle(base, 0, (UBYTE)dev, fn,
                                          PCICFG_VENDOR_DEVICE, 4,
                                          PCIB_OP_READ, 0);
                if ((UWORD)(vendevN & 0xFFFFUL) == 0xFFFF)
                    continue;

                if (!record_function(base, 0, (UBYTE)dev, fn, &next_free)) {
                    KPrintF((CONST_STRPTR)"prometheus.library: board cap "
                            "(%ld) reached -- further bus 0 functions "
                            "are not recorded\n", (LONG)PRM_MAX_BOARDS);
                    return;
                }
            }
        }
    }
}

/* ---- public API: Prm_FindBoardTagList / Prm_GetBoardAttrsTagList /
 * Prm_SetBoardAttrsTagList (docs/pci-library.md, the FD's a0/a1
 * register convention throughout) -------------------------------------- */

static PCIBoard *Prm_FindBoardTagList(PCIBoard *previous __asm("a0"),
                                      struct TagItem *taglist __asm("a1"),
                                      struct PrometheusBase *base __asm("a6"))
{
    struct PciFunction *prev = (struct PciFunction *)previous;
    ULONG start, i;

    start = prev ? (ULONG)((prev - base->Boards) + 1) : 0;

    for (i = start; i < base->BoardCount; i++) {
        struct PciFunction *cand = &base->Boards[i];
        struct TagItem *tags = taglist;
        struct TagItem *tag;
        BOOL match = TRUE;

        /* NULL or empty taglist matches everything: NextTagItem
         * returns NULL immediately for both, so the loop body never
         * runs and match stays TRUE. */
        while (tags != NULL && (tag = NextTagItem(&tags)) != NULL) {
            ULONG want = tag->ti_Data;

            switch (tag->ti_Tag) {
            case PRM_Vendor:
                if (cand->Vendor != (UWORD)want) match = FALSE;
                break;
            case PRM_Device:
                if (cand->Device != (UWORD)want) match = FALSE;
                break;
            case PRM_Revision:
                if (cand->Revision != (UBYTE)want) match = FALSE;
                break;
            case PRM_Class:
                if (cand->Class != (UBYTE)want) match = FALSE;
                break;
            case PRM_SubClass:
                if (cand->SubClass != (UBYTE)want) match = FALSE;
                break;
            case PRM_SlotNumber:
                if (cand->Dev != (UBYTE)want) match = FALSE;
                break;
            case PRM_FunctionNumber:
                if (cand->Fn != (UBYTE)want) match = FALSE;
                break;
            case PRM_BoardOwner:
                if (cand->Owner != (APTR)want) match = FALSE;
                break;
            default:
                break; /* unrecognised tags are ignored */
            }

            if (!match)
                break;
        }

        if (match)
            return (PCIBoard *)cand;
    }

    return NULL;
}

static ULONG Prm_GetBoardAttrsTagList(PCIBoard *board __asm("a0"),
                                      struct TagItem *taglist __asm("a1"),
                                      struct PrometheusBase *base __asm("a6"))
{
    struct PciFunction *f = (struct PciFunction *)board;
    struct TagItem *tags = taglist;
    struct TagItem *tag;
    ULONG count = 0;

    if (!f || !taglist)
        return 0;

    while ((tag = NextTagItem(&tags)) != NULL) {
        ULONG *dest = (ULONG *)tag->ti_Data;
        ULONG value;
        BOOL recognised = TRUE;

        if (!dest)
            continue; /* NULL ti_Data -> skip entirely */

        switch (tag->ti_Tag) {
        case PRM_Vendor:         value = f->Vendor;   break;
        case PRM_Device:         value = f->Device;   break;
        case PRM_Revision:       value = f->Revision; break;
        case PRM_Class:          value = f->Class;    break;
        case PRM_SubClass:       value = f->SubClass; break;

        case PRM_MemoryAddr0: case PRM_MemoryAddr1: case PRM_MemoryAddr2:
        case PRM_MemoryAddr3: case PRM_MemoryAddr4: case PRM_MemoryAddr5: {
            ULONG idx = tag->ti_Tag - PRM_MemoryAddr0;
            value = f->BarPci[idx] ? board_cpu_addr(base, f->BarPci[idx]) : 0;
            break;
        }
        case PRM_MemorySize0: case PRM_MemorySize1: case PRM_MemorySize2:
        case PRM_MemorySize3: case PRM_MemorySize4: case PRM_MemorySize5: {
            ULONG idx = tag->ti_Tag - PRM_MemorySize0;
            value = f->BarSize[idx];
            break;
        }

        case PRM_ROM_Address:    value = 0; break;
        case PRM_ROM_Size:       value = 0; break;
        case PRM_SlotNumber:     value = f->Dev; break;
        case PRM_FunctionNumber: value = f->Fn; break;
        case PRM_BoardOwner:     value = (ULONG)f->Owner; break;

        default:
            recognised = FALSE;
            value = 0;
            break;
        }

        *dest = value;
        if (recognised)
            count++;
    }

    return count;
}

static ULONG Prm_SetBoardAttrsTagList(PCIBoard *board __asm("a0"),
                                      struct TagItem *taglist __asm("a1"),
                                      struct PrometheusBase *base __asm("a6"))
{
    struct PciFunction *f = (struct PciFunction *)board;
    struct TagItem *tags = taglist;
    struct TagItem *tag;
    ULONG count = 0;

    (void)base;

    if (!f || !taglist)
        return 0;

    while ((tag = NextTagItem(&tags)) != NULL) {
        if (tag->ti_Tag != PRM_BoardOwner)
            continue; /* only PRM_BoardOwner is settable */

        Forbid();
        if (f->Owner == NULL || (APTR)tag->ti_Data == NULL) {
            f->Owner = (APTR)tag->ti_Data;
            count++;
        }
        Permit();
    }

    return count;
}

/* ---- public API: config-space accessors (docs/pci-library.md section
 * 3 -- the byte-order contract that must not be tidied) ----------------
 *
 * The SDK autodoc's own worked example (config longword 0 decodes to
 * $12345678, i.e. vendor $5678 device $1234):
 *
 *   Prm_ReadConfigLong(board, 0)  == $12345678
 *   Prm_ReadConfigWord(board, 0)  == $1234   (the DEVICE id)
 *   Prm_ReadConfigWord(board, 2)  == $5678   (the VENDOR id)
 *   Prm_ReadConfigByte(board, 0)  == $12
 *   Prm_ReadConfigByte(board, 1)  == $34
 *   Prm_ReadConfigByte(board, 2)  == $56
 *   Prm_ReadConfigByte(board, 3)  == $78
 *
 * i.e. each aligned config longword presents as a big-endian image of
 * its decoded value, which reduces to an offset swap within the
 * longword over this shim's value-carrying config cycle:
 *
 *   Prm_ReadConfigLong(b, o) = cfg_read(width 4, o & ~3)
 *   Prm_ReadConfigWord(b, o) = cfg_read(width 2, (o & ~1) ^ 2)
 *   Prm_ReadConfigByte(b, o) = cfg_read(width 1, o ^ 3)
 *
 * writes symmetric. NULL board: reads return all-ones, writes no-op. */

static ULONG Prm_ReadConfigLong(PCIBoard *board __asm("a0"),
                                UBYTE offset __asm("d0"),
                                struct PrometheusBase *base __asm("a6"))
{
    struct PciFunction *f = (struct PciFunction *)board;
    if (!f)
        return 0xFFFFFFFFUL;
    return cfg_cycle(base, f->Bus, f->Dev, f->Fn,
                     (UWORD)(offset & ~3), 4, PCIB_OP_READ, 0);
}

static UWORD Prm_ReadConfigWord(PCIBoard *board __asm("a0"),
                                UBYTE offset __asm("d0"),
                                struct PrometheusBase *base __asm("a6"))
{
    struct PciFunction *f = (struct PciFunction *)board;
    if (!f)
        return 0xFFFF;
    return (UWORD)cfg_cycle(base, f->Bus, f->Dev, f->Fn,
                            (UWORD)((offset & ~1) ^ 2), 2, PCIB_OP_READ, 0);
}

static UBYTE Prm_ReadConfigByte(PCIBoard *board __asm("a0"),
                                UBYTE offset __asm("d0"),
                                struct PrometheusBase *base __asm("a6"))
{
    struct PciFunction *f = (struct PciFunction *)board;
    if (!f)
        return 0xFF;
    return (UBYTE)cfg_cycle(base, f->Bus, f->Dev, f->Fn,
                            (UWORD)(offset ^ 3), 1, PCIB_OP_READ, 0);
}

static VOID Prm_WriteConfigLong(PCIBoard *board __asm("a0"),
                                ULONG data __asm("d0"),
                                UBYTE offset __asm("d1"),
                                struct PrometheusBase *base __asm("a6"))
{
    struct PciFunction *f = (struct PciFunction *)board;
    if (!f)
        return;
    cfg_cycle(base, f->Bus, f->Dev, f->Fn,
             (UWORD)(offset & ~3), 4, PCIB_OP_WRITE, data);
}

static VOID Prm_WriteConfigWord(PCIBoard *board __asm("a0"),
                                UWORD data __asm("d0"),
                                UBYTE offset __asm("d1"),
                                struct PrometheusBase *base __asm("a6"))
{
    struct PciFunction *f = (struct PciFunction *)board;
    if (!f)
        return;
    cfg_cycle(base, f->Bus, f->Dev, f->Fn,
             (UWORD)((offset & ~1) ^ 2), 2, PCIB_OP_WRITE, (ULONG)data);
}

static VOID Prm_WriteConfigByte(PCIBoard *board __asm("a0"),
                                UBYTE data __asm("d0"),
                                UBYTE offset __asm("d1"),
                                struct PrometheusBase *base __asm("a6"))
{
    struct PciFunction *f = (struct PciFunction *)board;
    if (!f)
        return;
    cfg_cycle(base, f->Bus, f->Dev, f->Fn,
             (UWORD)(offset ^ 3), 1, PCIB_OP_WRITE, (ULONG)data);
}

/* ---- public API: interrupts (docs/pci-library.md section 5) ---------- */

static BOOL Prm_AddIntServer(PCIBoard *board __asm("a0"),
                             struct Interrupt *intr __asm("a1"),
                             struct PrometheusBase *base __asm("a6"))
{
    struct PciFunction *f = (struct PciFunction *)board;
    UBYTE pin;

    if (!f || !intr)
        return FALSE;

    AddIntServer(INTB_PORTS, intr);

    pin = f->IntPin;
    if (pin >= 1 && pin <= 4) {
        UBYTE line = (UBYTE)(pin - 1);

        Disable();
        base->IntxRefCount[line]++;
        {
            UBYTE *regs = base->RegisterBase;
            ULONG enable = *(volatile ULONG *)(regs + PCIB_INTX_ENABLE);
            enable |= (1UL << line);
            *(volatile ULONG *)(regs + PCIB_INTX_ENABLE) = enable;
        }
        Enable();
    }
    /* pin 0: added to the chain, no enable bit touched (docs/pci-
     * library.md section 5). */

    return TRUE;
}

static VOID Prm_RemIntServer(PCIBoard *board __asm("a0"),
                             struct Interrupt *intr __asm("a1"),
                             struct PrometheusBase *base __asm("a6"))
{
    struct PciFunction *f = (struct PciFunction *)board;
    UBYTE pin;

    if (!intr)
        return;

    RemIntServer(INTB_PORTS, intr);

    if (!f)
        return;

    pin = f->IntPin;
    if (pin >= 1 && pin <= 4) {
        UBYTE line = (UBYTE)(pin - 1);

        Disable();
        if (base->IntxRefCount[line] > 0) {
            base->IntxRefCount[line]--;
            if (base->IntxRefCount[line] == 0) {
                UBYTE *regs = base->RegisterBase;
                ULONG enable = *(volatile ULONG *)(regs + PCIB_INTX_ENABLE);
                enable &= ~(1UL << line);
                *(volatile ULONG *)(regs + PCIB_INTX_ENABLE) = enable;
            }
        }
        Enable();
    }
}

/* ---- public API: DMA buffers and address translation (docs/pci-
 * library.md section 4) ------------------------------------------------ */

static APTR Prm_AllocDMABuffer(ULONG size __asm("d0"),
                               struct PrometheusBase *base __asm("a6"))
{
    (void)base;

    if (size == 0)
        return NULL;

    size = (size + 3UL) & ~3UL;
    return AllocMem(size, MEMF_PUBLIC);
}

static VOID Prm_FreeDMABuffer(APTR buffer __asm("a0"), ULONG size __asm("d0"),
                              struct PrometheusBase *base __asm("a6"))
{
    (void)base;

    if (!buffer || size == 0)
        return;

    size = (size + 3UL) & ~3UL;
    FreeMem(buffer, size);
}

static APTR Prm_GetPhysicalAddress(APTR addr __asm("d0"),
                                   struct PrometheusBase *base __asm("a6"))
{
    ULONG a = (ULONG)addr;
    ULONG cpu_base = (ULONG)base->ApertureCpuBase;

    if (!addr)
        return NULL;

    if (a >= cpu_base && a < cpu_base + PCIB_APERTURE_BYTES)
        return (APTR)(PCIB_PCI_MEM_BASE + (a - cpu_base));

    /* Identity: guest physical address == PCI bus address for RAM on
     * this machine (docs/pci-library.md section 4). */
    return addr;
}

static APTR Prm_GetVirtualAddress(APTR addr __asm("d0"),
                                  struct PrometheusBase *base __asm("a6"))
{
    ULONG a = (ULONG)addr;
    ULONG cpu_base = (ULONG)base->ApertureCpuBase;

    if (!addr)
        return NULL;

    if (a >= PCIB_PCI_MEM_BASE && a < PCIB_PCI_MEM_BASE + PCIB_PCI_MEM_BYTES)
        return (APTR)(cpu_base + (a - PCIB_PCI_MEM_BASE));

    return addr;
}

/* ---- standard AUTOINIT disk-library boilerplate -----------------------
 * Translated from m68k/rtgboard-card/rtgboard_card.c (itself from
 * amirfb_card.c, BSD 2-Clause, same owner). The difference from
 * rtgboard's .card form: there is no separate FindCard/InitCard phase
 * here, so LibInit does everything docs/pci-library.md section 2
 * describes, narrating every step and failing cleanly (returning NULL,
 * which aborts library creation and lets exec free the base memory it
 * allocated -- "library absent from a machine without --pcibridge is
 * correct").
 */

static struct PrometheusBase *
LibInit(struct PrometheusBase *base __asm("d0"),
        BPTR             seglist __asm("a0"),
        struct ExecBase *sysbase __asm("a6"))
{
    struct ConfigDev *cd = NULL;
    BOOL claimed = FALSE;
    ULONG version;

    SysBase        = sysbase;
    base->ExecBase = sysbase;
    base->SegList  = (APTR)seglist;  /* PrometheusBase.SegList is APTR, not BPTR */

    KPrintF((CONST_STRPTR)"prometheus.library: LibInit entry\n");

    /* Step 1: FindConfigDev, claim CDF_CONFIGME (docs/pci-library.md
     * section 2 point 1; identical claim-loop shape to rtgboard_card.c's
     * FindCard). Never a hardcoded address -- everything below derives
     * from cd->cd_BoardAddr (docs/device-ledger.md's rule). */
    ExpansionBase = (struct ExpansionBase *)
        OpenLibrary((CONST_STRPTR)"expansion.library", 36);
    if (!ExpansionBase) {
        KPrintF((CONST_STRPTR)"prometheus.library: LibInit: "
                "expansion.library open failed\n");
        return NULL;
    }

    while ((cd = FindConfigDev(cd, PCIBRIDGE_MANUFACTURER,
                               PCIBRIDGE_PRODUCT)) != NULL) {
        if (cd->cd_Flags & CDF_CONFIGME) {
            cd->cd_Flags &= ~CDF_CONFIGME;
            claimed = TRUE;
            break;
        }
    }

    if (!claimed) {
        KPrintF((CONST_STRPTR)"prometheus.library: LibInit: no unclaimed "
                "pcibridge found (manufacturer 0x%lx product %ld) -- "
                "normal on a machine without --pcibridge\n",
                (ULONG)PCIBRIDGE_MANUFACTURER, (LONG)PCIBRIDGE_PRODUCT);
        CloseLibrary((struct Library *)ExpansionBase);
        ExpansionBase = NULL;
        return NULL;
    }

    base->RegisterBase = (UBYTE *)cd->cd_BoardAddr;

    CloseLibrary((struct Library *)ExpansionBase);
    ExpansionBase = NULL;

    /* utility.library: needed for the rest of this library's life
     * (NextTagItem, called from every TagList-taking public function),
     * so opened here and kept open, unlike the borrow-and-close
     * expansion.library pattern just above. A core V36+ ROM library --
     * failure here would mean a badly broken Kickstart, not a normal
     * "board absent" case, so it is narrated and fails init exactly
     * like any other step. */
    UtilityBase = OpenLibrary((CONST_STRPTR)"utility.library", 36);
    if (!UtilityBase) {
        KPrintF((CONST_STRPTR)"prometheus.library: LibInit: "
                "utility.library open failed\n");
        return NULL;
    }

    /* Step 2: refuse anything but PCIBRIDGE_PROTOCOL_VERSION (positive
     * rejection, docs/pci-library.md section 2 point 1). */
    version = *(volatile ULONG *)(base->RegisterBase + PCIB_VERSION);
    if (version != PCIBRIDGE_PROTOCOL_VERSION) {
        KPrintF((CONST_STRPTR)"prometheus.library: LibInit: PCIB_VERSION "
                "reports %ld, not %ld -- refusing to drive an "
                "unrecognised protocol revision\n",
                (LONG)version, (LONG)PCIBRIDGE_PROTOCOL_VERSION);
        CloseLibrary(UtilityBase);
        UtilityBase = NULL;
        return NULL;
    }
    KPrintF((CONST_STRPTR)"prometheus.library: LibInit: board at 0x%lx, "
            "PCIB_VERSION %ld confirmed\n",
            (ULONG)cd->cd_BoardAddr, (LONG)version);

    /* Board record storage: allocated once, up front (docs/pci-
     * library.md section 2 point 2's "cap at a fixed maximum"). */
    base->Boards = (struct PciFunction *)
        AllocMem((ULONG)(sizeof(struct PciFunction) * PRM_MAX_BOARDS),
                 MEMF_PUBLIC | MEMF_CLEAR);
    if (!base->Boards) {
        KPrintF((CONST_STRPTR)"prometheus.library: LibInit: AllocMem "
                "failed for the %ld-entry board table\n",
                (LONG)PRM_MAX_BOARDS);
        CloseLibrary(UtilityBase);
        UtilityBase = NULL;
        return NULL;
    }
    base->BoardCount = 0;

    /* Steps 3-4: enumerate bus 0, sizing and assigning every function's
     * BARs as it is found. */
    enumerate_bus0(base);

    /* Step 5: bank the whole allocated PCI memory region behind the
     * aperture, once. */
    *(volatile ULONG *)(base->RegisterBase + PCIB_APERTURE_BASE) =
        PCIB_PCI_MEM_BASE;
    base->ApertureCpuBase = (APTR)(base->RegisterBase + PCIB_APERTURE_OFFSET);

    KPrintF((CONST_STRPTR)"prometheus.library: LibInit: %ld function(s) "
            "recorded, aperture CPU base 0x%lx\n",
            (LONG)base->BoardCount, (ULONG)base->ApertureCpuBase);

    KPrintF((CONST_STRPTR)"prometheus.library: LibInit complete\n");

    return base;
}

static struct PrometheusBase *
LibOpen(struct PrometheusBase *base __asm("a6"))
{
    base->LibBase.lib_OpenCnt++;
    base->LibBase.lib_Flags &= ~LIBF_DELEXP;
    return base;
}

static BPTR
LibExpungeInternal(struct PrometheusBase *base)
{
    BPTR seglist;

    if (base->LibBase.lib_OpenCnt) {
        base->LibBase.lib_Flags |= LIBF_DELEXP;
        return (BPTR)0;
    }

    if (base->Boards)
        FreeMem(base->Boards,
                (ULONG)(sizeof(struct PciFunction) * PRM_MAX_BOARDS));

    if (UtilityBase) {
        CloseLibrary(UtilityBase);
        UtilityBase = NULL;
    }

    seglist = (BPTR)base->SegList;
    Remove((struct Node *)base);
    FreeMem((char *)base - base->LibBase.lib_NegSize,
            (ULONG)(base->LibBase.lib_NegSize + base->LibBase.lib_PosSize));
    return seglist;
}

static BPTR
LibClose(struct PrometheusBase *base __asm("a6"))
{
    if (--base->LibBase.lib_OpenCnt == 0 &&
        (base->LibBase.lib_Flags & LIBF_DELEXP))
        return LibExpungeInternal(base);
    return (BPTR)0;
}

static BPTR
LibExpunge(struct PrometheusBase *base __asm("a6"))
{
    return LibExpungeInternal(base);
}

static ULONG
LibReserved(void)
{
    return 0;
}

/* FD order (docs/pci-library.md's brief, matching the FD's own bias-30
 * LVO grid via inline/prometheus.h): FindBoardTagList, GetBoardAttrs
 * TagList, the six config accessors, SetBoardAttrsTagList, AddIntServer/
 * RemIntServer, AllocDMABuffer/FreeDMABuffer, GetPhysicalAddress,
 * GetVirtualAddress. */
static const APTR FuncTab[] = {
    (APTR)LibOpen,
    (APTR)LibClose,
    (APTR)LibExpunge,
    (APTR)LibReserved,
    (APTR)Prm_FindBoardTagList,     /* -30 */
    (APTR)Prm_GetBoardAttrsTagList, /* -36 */
    (APTR)Prm_ReadConfigLong,       /* -42 */
    (APTR)Prm_ReadConfigWord,       /* -48 */
    (APTR)Prm_ReadConfigByte,       /* -54 */
    (APTR)Prm_WriteConfigLong,      /* -60 */
    (APTR)Prm_WriteConfigWord,      /* -66 */
    (APTR)Prm_WriteConfigByte,      /* -72 */
    (APTR)Prm_SetBoardAttrsTagList, /* -78 */
    (APTR)Prm_AddIntServer,         /* -84 */
    (APTR)Prm_RemIntServer,         /* -90 */
    (APTR)Prm_AllocDMABuffer,       /* -96 */
    (APTR)Prm_FreeDMABuffer,        /* -102 */
    (APTR)Prm_GetPhysicalAddress,   /* -108 */
    (APTR)Prm_GetVirtualAddress,    /* -114 */
    (APTR)-1
};

static const ULONG InitTab[4] = {
    sizeof(struct PrometheusBase),
    (ULONG)FuncTab,
    (ULONG)0,
    (ULONG)LibInit
};

static const struct Resident ROMTag;   /* self-ref for rt_MatchTag */

static const struct Resident ROMTag = {
    RTC_MATCHWORD,
    (struct Resident *)&ROMTag,
    (APTR)(&ROMTag + 1),
    RTF_AUTOINIT,
    LIBVERSION,
    NT_LIBRARY,
    0,
    (char *)LibName,
    (char *)LibIdString,
    (APTR)InitTab
};
