/*
 * rtgboard_card.c -- rtgboard.card: the Picasso96 driver for the `rtgboard`
 * device (docs/rtgboard-protocol.md).
 *
 * A standard Exec AUTOINIT library, disk-loaded from LIBS:Picasso96/
 * rtgboard.card by rtg.library when a Monitor icon carries a BOARDTYPE=
 * rtgboard tooltype. No DiagArea and no boot ROM are involved -- this
 * board's er_InitDiagVec is 0 and ERTF_DIAGVALID is unset (docs/
 * rtgboard-protocol.md SS1, and docs/rtgboard-driver-scope.md SS1).
 *
 * The AUTOINIT/Open-Close-Expunge boilerplate below is translated
 * directly from amirfb.card (~/src/amirfb/src/card/amirfb_card.c, BSD
 * 2-Clause, same owner) -- same struct CardBase, same LibInit/LibOpen/
 * LibClose/LibExpunge/LibReserved/FuncTab/InitTab/ROMTag shape, same
 * SysBase-must-be-defined-not-just-declared reasoning (no startup code
 * exists under -nostdlib), same "CardBase.SegList is APTR, not BPTR" note.
 *
 * Increment 1 (this file): the build skeleton only. FindCard/InitCard are
 * stubs -- FindCard returns FALSE (increment 2 fills in real AUTOCONFIG
 * board discovery against RTG_BOARD_MANUFACTURER/RTG_BOARD_PRODUCT);
 * InitCard returns TRUE unconditionally (increment 2 fills in the real
 * BoardInfo vector wiring). No board logic, no register access, no VRAM
 * mapping yet.
 */

#include <exec/types.h>
#include <exec/resident.h>
#include <exec/libraries.h>
#include <exec/execbase.h>
#include <exec/memory.h>
#include <proto/exec.h>
#include <clib/debug_protos.h>

#include <boardinfo.h>

#include "rtgboard_card.h"

/* SysBase: consumed by <proto/exec.h>'s inline call macros. That header
 * only extern-declares the global (expecting ixemul/libnix startup code to
 * define and set it); since we build with -nostdlib and no such runtime,
 * WE must provide the actual definition. Set in LibInit -- identical
 * pattern to amirfb_card.c. */
struct ExecBase *SysBase;

/* ---- identity ------------------------------------------------------------ */

const char LibName[] = "rtgboard.card";
static const char LibIdString[] = "$VER: rtgboard.card 0.1 (15.09.2026)\r\n";

#define LIBVERSION  0
#define LIBREVISION 1

/* ---- FindCard / InitCard ---------------------------------------------------
 * Standard AmigaOS library calls: a6 = struct CardBase * (our library
 * base). Unlike the BoardInfo vectors (not wired up until increment 2),
 * these DO get a valid a6.
 */

static BOOL FindCard(struct BoardInfo *bi __asm("a0"),
                     struct CardBase *base __asm("a6"))
{
    (void)bi;
    (void)base;

    KPrintF((CONST_STRPTR)"rtgboard: FindCard (skeleton)\n");

    /* Increment 2 fills this in: FindConfigDev() loop against
     * RTG_BOARD_MANUFACTURER/RTG_BOARD_PRODUCT (rtgboard_card.h), claiming
     * one unclaimed instance by clearing CDB_CONFIGME, and filling in
     * bi->MemoryBase/MemorySize/RegisterBase from the claimed ConfigDev.
     * For now, no board is ever found. */
    return FALSE;
}

static BOOL InitCard(struct BoardInfo *bi __asm("a0"),
                     STRPTR *toolTypes __asm("a1"),
                     struct CardBase *base __asm("a6"))
{
    (void)bi;
    (void)toolTypes;
    (void)base;

    KPrintF((CONST_STRPTR)"rtgboard: InitCard (skeleton)\n");

    /* Increment 2 fills this in: BoardName/BoardType/RGBFormats/mode
     * ceilings and every mandatory BoardInfo vector ADR 0002's "Resolved"
     * section lists (SetGC, SetPanning, SetSwitch, SetDisplay,
     * SetColorArray, SetDAC, CalculateBytesPerRow, CalculateMemory,
     * GetCompatibleFormats, ResolvePixelClock, GetPixelClock, SetClock,
     * SetMemoryMode, WaitVerticalSync) -- this board carries no
     * accelerator, so nothing beyond those fourteen is required (docs/
     * rtgboard-protocol.md SS2). For now, always succeeds with an
     * otherwise-untouched BoardInfo. */
    return TRUE;
}

/* ---- standard AUTOINIT library boilerplate --------------------------------
 * Translated from amirfb_card.c (BSD 2-Clause, same owner).
 */

static struct CardBase *
LibInit(struct CardBase *base    __asm("d0"),
        BPTR             seglist __asm("a0"),
        struct ExecBase *sysbase __asm("a6"))
{
    SysBase        = sysbase;
    base->ExecBase = sysbase;
    base->SegList  = (APTR)seglist;  /* CardBase.SegList is APTR, not BPTR */
    return base;
}

static struct CardBase *
LibOpen(struct CardBase *base __asm("a6"))
{
    base->LibBase.lib_OpenCnt++;
    base->LibBase.lib_Flags &= ~LIBF_DELEXP;
    return base;
}

static BPTR
LibExpungeInternal(struct CardBase *base)
{
    BPTR seglist;

    if (base->LibBase.lib_OpenCnt) {
        base->LibBase.lib_Flags |= LIBF_DELEXP;
        return (BPTR)0;
    }

    seglist = (BPTR)base->SegList;
    Remove((struct Node *)base);
    FreeMem((char *)base - base->LibBase.lib_NegSize,
            (ULONG)(base->LibBase.lib_NegSize + base->LibBase.lib_PosSize));
    return seglist;
}

static BPTR
LibClose(struct CardBase *base __asm("a6"))
{
    if (--base->LibBase.lib_OpenCnt == 0 &&
        (base->LibBase.lib_Flags & LIBF_DELEXP))
        return LibExpungeInternal(base);
    return (BPTR)0;
}

static BPTR
LibExpunge(struct CardBase *base __asm("a6"))
{
    return LibExpungeInternal(base);
}

static ULONG
LibReserved(void)
{
    return 0;
}

static const APTR FuncTab[] = {
    (APTR)LibOpen,
    (APTR)LibClose,
    (APTR)LibExpunge,
    (APTR)LibReserved,
    (APTR)FindCard,     /* -30 */
    (APTR)InitCard,     /* -36 */
    (APTR)-1
};

static const ULONG InitTab[4] = {
    sizeof(struct CardBase),
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
