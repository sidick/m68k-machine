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
 * Increment 2 (this file): real FindCard/InitCard and every BoardInfo
 * vector docs/rtgboard-protocol.md SS2 lists as mandatory (fourteen
 * vectors) plus amirfb's never-leave-NULL stub set for the rest. This
 * board carries no accelerator (SS2) -- zero render/sprite-render vectors
 * are wired, P96's own *Default CPU renderers draw everything.
 *
 * Register access sizes (design note A): CONFIRMED against crates/
 * machine-core/src/rtgboard.rs's `read`/`write`, which are byte-granular
 * (one call per address, exactly the "one hot byte per 4-byte-aligned
 * slot" idiom hostblk.rs/input.rs/mirage.rs all share) -- the same
 * convention hostblk-diagrom.s's own register-access comment documents
 * and relies on: a plain `move.l` to a u32 register's base offset works
 * correctly (the bus decomposes it into four consecutive byte accesses in
 * address order, matching the register's four big-endian byte lanes), and
 * a byte-wide register (MODE_INDEX/SET_FORMAT/COMMIT/STATUS/CUR_FORMAT/
 * MODE_FORMAT) only responds at the LOW byte of its 4-byte slot (offset
 * +3) -- the other three offsets in the slot discard writes and read as
 * 0. rtg_reg_write8/read8 below fold that "+3" in once, here, rather than
 * asking every call site to remember it.
 */

#include <exec/types.h>
#include <exec/resident.h>
#include <exec/libraries.h>
#include <exec/execbase.h>
#include <exec/memory.h>
#include <proto/exec.h>
#include <proto/expansion.h>
#include <libraries/configvars.h>
#include <clib/debug_protos.h>

#include <boardinfo.h>

#include "rtgboard_card.h"

/* SysBase/ExpansionBase: consumed by <proto/exec.h>'s/<proto/expansion.h>'s
 * inline call macros. Those headers only extern-declare the globals
 * (expecting ixemul/libnix startup code to define and set them); since we
 * build with -nostdlib and no such runtime, WE must provide the actual
 * definitions. SysBase is set in LibInit (identical pattern to
 * amirfb_card.c); ExpansionBase is opened/closed entirely within FindCard
 * (nothing else in this driver needs expansion.library). */
struct ExecBase *SysBase;
struct ExpansionBase *ExpansionBase;

/* ---- identity ------------------------------------------------------------ */

const char LibName[] = "rtgboard.card";
static const char LibIdString[] = "$VER: rtgboard.card 0.2 (15.09.2026)\r\n";

#define LIBVERSION  0
#define LIBREVISION 2

/* ---- register accessors (design note A) -----------------------------------
 * volatile accessors over bi->RegisterBase -- see the file-top comment for
 * the access-size confirmation against rtgboard.rs. */

static inline ULONG rtg_reg_read32(struct BoardInfo *bi, ULONG off)
{
    return *(volatile ULONG *)(bi->RegisterBase + off);
}

static inline void rtg_reg_write32(struct BoardInfo *bi, ULONG off, ULONG val)
{
    *(volatile ULONG *)(bi->RegisterBase + off) = val;
}

/* Byte-wide registers live at the LOW byte of their 4-byte slot (file-top
 * comment) -- fold the "+3" in here so no call site has to remember it. */
static inline UBYTE rtg_reg_read8(struct BoardInfo *bi, ULONG off)
{
    return *(volatile UBYTE *)(bi->RegisterBase + off + 3);
}

static inline void rtg_reg_write8(struct BoardInfo *bi, ULONG off, UBYTE val)
{
    *(volatile UBYTE *)(bi->RegisterBase + off + 3) = val;
}

/* Shared by SetGC (stages the board's own SET_STRIDE) and
 * CalculateBytesPerRow (answers P96's own bytes-per-row query) -- one
 * formula, so the two can never disagree about this board's one real
 * format's row size. docs/rtgboard-protocol.md SS5 warns stride is not
 * generally derivable from width alone; for RTG_FMT_RGB_565 specifically
 * it simply is (2 bytes/pixel, no padding this driver ever requests). */
static UWORD rtg_row_bytes_565(UWORD width)
{
    return (UWORD)(width * 2);
}

/* CalculateBytesPerRow's ModeInfo/height parameters exist from P96 3.3.1+
 * (p96-driver-development skill's version-awareness table). This
 * project's installed P96 (iComp 3.6.2, rtg.library 43.x) HAS them
 * unconditionally (environment facts) -- but amirfb's own version gate is
 * kept anyway as defence for an older core this driver might someday run
 * under, same FindName-rtg.library-under-Forbid pattern (InitCard,
 * below), same gate constant (41, the iComp line's floor -- P96 2.4.0 per
 * the iComp wiki's EnableSoftSprite entry) amirfb_card.c uses. Stored in
 * bi->CardData[0] rather than a driver-global: this project's "no
 * global/static state" discipline (p96-driver-development skill, rule 1)
 * applies cleanly here since there is nothing amirfb's own g_amirfb_rtg_
 * version deviation (issue #52) needed to work around -- no second
 * context (no spawned process) ever needs to reach this value. */
#define RTG_VERSION_CBPR_HAS_MODEINFO 41

/* ---- FindCard --------------------------------------------------------- */

static BOOL FindCard(struct BoardInfo *bi __asm("a0"),
                     struct CardBase *base __asm("a6"))
{
    struct ConfigDev *cd = NULL;
    BOOL claimed = FALSE;

    (void)base;

    ExpansionBase = (struct ExpansionBase *)
        OpenLibrary((CONST_STRPTR)"expansion.library", 36);
    if (!ExpansionBase) {
        KPrintF((CONST_STRPTR)"rtgboard: FindCard: expansion.library open "
                "failed\n");
        return FALSE;
    }

    /* FindConfigDev loop, claiming one unclaimed instance by clearing
     * CDB_CONFIGME (p96-driver-development skill's FindCard guidance;
     * PiccoloSD64.card.asm's own real-Zorro loop follows the identical
     * shape) -- a second rtgboard, if this host ever grows more than one,
     * would then be picked up by a second driver instance rather than
     * colliding with this one. AUTOCONFIG places the board; this driver
     * only ever asks the bus for it (this project's "no hardcoded memory
     * addresses" discipline), never compares against a constant address. */
    while ((cd = FindConfigDev(cd, RTG_BOARD_MANUFACTURER,
                               RTG_BOARD_PRODUCT)) != NULL) {
        if (cd->cd_Flags & CDF_CONFIGME) {
            cd->cd_Flags &= ~CDF_CONFIGME;
            claimed = TRUE;
            break;
        }
    }

    if (!claimed) {
        /* Absent (the normal case on a machine run without --rtgboard) or
         * already claimed by another instance -- either way "no board",
         * not an error. */
        KPrintF((CONST_STRPTR)"rtgboard: FindCard: no unclaimed rtgboard "
                "found (manufacturer 0x%lx product %ld) -- normal on a "
                "machine without --rtgboard\n",
                (ULONG)RTG_BOARD_MANUFACTURER, (LONG)RTG_BOARD_PRODUCT);
        CloseLibrary((struct Library *)ExpansionBase);
        ExpansionBase = NULL;
        return FALSE;
    }

    bi->RegisterBase = (UBYTE *)cd->cd_BoardAddr;
    bi->MemoryBase   = (UBYTE *)cd->cd_BoardAddr + RTG_VRAM_BASE_OFFSET;
    bi->MemoryIOBase = bi->MemoryBase;

    /* Only needed to make the FindConfigDev call above; register access
     * from here on is plain memory-mapped I/O against bi->RegisterBase,
     * no library calls involved. */
    CloseLibrary((struct Library *)ExpansionBase);
    ExpansionBase = NULL;

    /* Positive-evidence version check (this project's discipline: never
     * assume a board is what it claims to be -- confirm). */
    {
        ULONG version = rtg_reg_read32(bi, RTG_REG_VERSION);

        if (version != 1) {
            KPrintF((CONST_STRPTR)"rtgboard: FindCard: VERSION register "
                    "reports %ld, not 1 -- refusing to drive an "
                    "unrecognised protocol revision\n", (LONG)version);
            return FALSE;
        }
    }

    /* VRAM_BYTES clamped to what this one 16 MB AUTOCONFIG window can
     * actually reach: VRAM starts at RTG_VRAM_BASE_OFFSET (1 MB in) and
     * the window ends at RTG_WINDOW_BYTES, so at most RTG_WINDOW_BYTES -
     * RTG_VRAM_BASE_OFFSET (0x00F00000, 15 MB) of it is ever addressable
     * through this aperture, however much VRAM the board reports it has
     * attached (docs/rtgboard-protocol.md SS1). */
    {
        ULONG vram_bytes = rtg_reg_read32(bi, RTG_REG_VRAM_BYTES);
        ULONG max_reachable = RTG_WINDOW_BYTES - RTG_VRAM_BASE_OFFSET;
        ULONG mode_count = rtg_reg_read32(bi, RTG_REG_MODE_COUNT);

        if (vram_bytes > max_reachable)
            vram_bytes = max_reachable;
        bi->MemorySize = vram_bytes;

        KPrintF((CONST_STRPTR)"rtgboard: FindCard: board at 0x%lx, VRAM "
                "%ld bytes (aperture allows up to %ld), %ld catalog "
                "mode(s)\n",
                (ULONG)cd->cd_BoardAddr, (LONG)vram_bytes,
                (LONG)max_reachable, (LONG)mode_count);
    }

    return TRUE;
}

/* ---- vectors InitCard installs (no *Default exists for any of these,
 * per docs/rtgboard-protocol.md SS2 -- this board carries no accelerator,
 * so nothing beyond these fourteen-plus-stubs is required) -------------- */

/* No physical monitor cable to switch between Amiga and RTG output. */
static BOOL RtgSetSwitch(struct BoardInfo *bi __asm("a0"),
                         BOOL enable __asm("d0"))
{
    (void)bi; (void)enable;
    return TRUE;
}

/* No palette/DAC hardware by design (docs/rtgboard-protocol.md SS11) --
 * this board only ever advertises RGBFF_R5G6B5 (direct colour), never
 * RGBFF_CLUT, so these are never meaningfully called; kept as real, empty
 * stubs anyway (never leave a driver-owned vector NULL). */
static void RtgSetColorArray(struct BoardInfo *bi __asm("a0"),
                             UWORD start __asm("d0"),
                             UWORD count __asm("d1"))
{
    (void)bi; (void)start; (void)count;
}

static void RtgSetDAC(struct BoardInfo *bi __asm("a0"),
                      UWORD unused __asm("d0"),
                      RGBFTYPE format __asm("d7"))
{
    (void)bi; (void)unused; (void)format;
}

/* Programs the board's timing (CRTC-equivalent: SET_WIDTH/HEIGHT/FORMAT/
 * STRIDE) via COMMIT -- must NOT set the framebuffer base (P96 will call
 * SetPanning for that; p96-driver-development skill's own division-of-
 * labour note). SET_FB_OFFSET is staged as whatever CUR_FB_OFFSET already
 * is (design note D: this board has no driver-side shadow of the current
 * mode, the CUR_* registers are the single source of truth -- "ask the
 * bus"), so a re-SetGC of the same geometry doesn't move the visible
 * frame out from under a SetPanning that hasn't run yet this activation. */
static void RtgSetGC(struct BoardInfo *bi __asm("a0"),
                     struct ModeInfo *mi __asm("a1"),
                     BOOL border __asm("d0"))
{
    ULONG stride;
    UBYTE status;

    (void)border;

    if (!mi || mi->Width == 0 || mi->Height == 0)
        return;

    stride = rtg_row_bytes_565(mi->Width);

    rtg_reg_write32(bi, RTG_REG_SET_WIDTH, mi->Width);
    rtg_reg_write32(bi, RTG_REG_SET_HEIGHT, mi->Height);
    rtg_reg_write8(bi, RTG_REG_SET_FORMAT, RTG_FMT_RGB_565);
    rtg_reg_write32(bi, RTG_REG_SET_STRIDE, stride);
    rtg_reg_write32(bi, RTG_REG_SET_FB_OFFSET,
                    rtg_reg_read32(bi, RTG_REG_CUR_FB_OFFSET));

    rtg_reg_write8(bi, RTG_REG_COMMIT, 1);
    status = rtg_reg_read8(bi, RTG_REG_STATUS);

    if (status & RTG_STATUS_APPLIED)
        KPrintF((CONST_STRPTR)"rtgboard: SetGC %ldx%ld committed: "
                "APPLIED\n", (LONG)mi->Width, (LONG)mi->Height);
    else
        KPrintF((CONST_STRPTR)"rtgboard: SetGC %ldx%ld committed: "
                "REJECTED\n", (LONG)mi->Width, (LONG)mi->Height);

    rtg_reg_write8(bi, RTG_REG_STATUS, 0xFF);
}

/* Programs the board's display start address -- copies amirfb_card.c's
 * exact signature/parameter order (design note E): register tags make the
 * *calling* convention order-independent, but plain-C function-pointer
 * assignment (bi->SetPanning = RtgSetPanning) is positional, so this MUST
 * match include/p96sdk/boardinfo.h's declared order (d0,d3,d1,d2,d7) or
 * the assignment silently mistypes the pointer. d3 (max_x) was settled
 * live by amirfb (issue #65): two internal references disagreed on
 * whether d3 even exists and what it means; live tracing showed it's the
 * bitmap's rightmost valid column (width-1), unrelated to height, and
 * unused here for the same reason amirfb leaves it unused.
 *
 * Design note D: no driver-side shadow of the current mode -- CUR_WIDTH/
 * HEIGHT/FORMAT/STRIDE are read fresh from the board and re-committed
 * with the new offset. Valid only because SetGC precedes SetPanning
 * within one activation (amirfb confirmed this live, 2026-08-11) -- if
 * CUR_FORMAT reads RTG_FMT_INVALID (nothing has ever been committed),
 * there is nothing to re-commit against, so just store the offset and
 * return, per design note D. */
static void RtgSetPanning(struct BoardInfo *bi __asm("a0"),
                          UBYTE *memory __asm("a1"),
                          UWORD width __asm("d0"),
                          UWORD max_x __asm("d3"),
                          WORD xoffset __asm("d1"),
                          WORD yoffset __asm("d2"),
                          RGBFTYPE format __asm("d7"))
{
    UBYTE cur_format;
    ULONG cur_width, cur_height, cur_stride;
    ULONG offset, frame_bytes;
    UBYTE status;

    (void)max_x; (void)width; (void)format;

    /* Documented driver obligation: copy d1/d2 into bi->XOffset/YOffset,
     * or sprite positioning breaks (p96-driver-development skill,
     * functions-core.md). */
    bi->XOffset = xoffset;
    bi->YOffset = yoffset;

    cur_format = rtg_reg_read8(bi, RTG_REG_CUR_FORMAT);
    if (cur_format == RTG_FMT_INVALID) {
        KPrintF((CONST_STRPTR)"rtgboard: SetPanning: no mode ever applied "
                "(CUR_FORMAT invalid) -- offset stored, no commit\n");
        return;
    }

    if (!memory) {
        KPrintF((CONST_STRPTR)"rtgboard: SetPanning: NULL memory -- not "
                "committing\n");
        return;
    }

    /* P96's allocator can legitimately fall back to system RAM when this
     * board's arena is full (amirfb saw this live) -- only trust an
     * address actually inside bi->MemoryBase..+MemorySize. Guard the
     * pointer comparison before subtracting, so a memory pointer before
     * MemoryBase can't underflow into a bogus small offset. */
    if (memory < bi->MemoryBase) {
        KPrintF((CONST_STRPTR)"rtgboard: SetPanning: bitmap at 0x%lx is "
                "before this board's VRAM (0x%lx) -- not committing\n",
                (ULONG)memory, (ULONG)bi->MemoryBase);
        return;
    }
    offset = (ULONG)(memory - bi->MemoryBase);
    if (offset >= bi->MemorySize) {
        KPrintF((CONST_STRPTR)"rtgboard: SetPanning: bitmap offset 0x%lx "
                "is outside this board's %ld-byte VRAM -- not "
                "committing\n", offset, (LONG)bi->MemorySize);
        return;
    }

    cur_width  = rtg_reg_read32(bi, RTG_REG_CUR_WIDTH);
    cur_height = rtg_reg_read32(bi, RTG_REG_CUR_HEIGHT);
    cur_stride = rtg_reg_read32(bi, RTG_REG_CUR_STRIDE);
    frame_bytes = cur_height * cur_stride;

    if (frame_bytes > bi->MemorySize - offset) {
        KPrintF((CONST_STRPTR)"rtgboard: SetPanning: bitmap at offset "
                "0x%lx (%ld bytes) runs past this board's %ld-byte VRAM "
                "-- not committing\n",
                offset, (LONG)frame_bytes, (LONG)bi->MemorySize);
        return;
    }

    rtg_reg_write32(bi, RTG_REG_SET_WIDTH, cur_width);
    rtg_reg_write32(bi, RTG_REG_SET_HEIGHT, cur_height);
    rtg_reg_write8(bi, RTG_REG_SET_FORMAT, cur_format);
    rtg_reg_write32(bi, RTG_REG_SET_STRIDE, cur_stride);
    rtg_reg_write32(bi, RTG_REG_SET_FB_OFFSET, offset);

    rtg_reg_write8(bi, RTG_REG_COMMIT, 1);
    status = rtg_reg_read8(bi, RTG_REG_STATUS);

    if (status & RTG_STATUS_APPLIED)
        KPrintF((CONST_STRPTR)"rtgboard: SetPanning: offset 0x%lx "
                "committed: APPLIED\n", offset);
    else
        KPrintF((CONST_STRPTR)"rtgboard: SetPanning: offset 0x%lx "
                "committed: REJECTED\n", offset);

    rtg_reg_write8(bi, RTG_REG_STATUS, 0xFF);
}

/* Correctness-critical (docs/rtgboard-protocol.md SS5: "stride is not
 * width", CalculateBytesPerRow is a driver's own formula, never assumed
 * width*bpp by the board) -- real logic, not a stub.
 *
 * Version-gated oversized-displayable-bitmap refusal, exactly like
 * amirfb's issue-#64 logic: this board cannot pan (design note D has no
 * panning concept beyond a single re-committed offset), so a virtual
 * desktop/autoscroll bitmap bigger than its mode would need SetPanning to
 * scroll within it -- and this board's own COMMIT (docs/rtgboard-
 * protocol.md SS7 check 1) refuses any (width, height, format) that isn't
 * an exact catalog entry anyway, so such a bitmap could never actually be
 * displayed. Refusing it here, at allocation time, fails the open
 * cleanly instead of letting P96 build a bitmap this board will only
 * reject later. Gated on bi->CardData[0] (rtg.library's lib_Version,
 * captured in InitCard) the same way amirfb gates its own copy of this
 * logic -- see RTG_VERSION_CBPR_HAS_MODEINFO's own comment above. */
static UWORD RtgCalculateBytesPerRow(struct BoardInfo *bi __asm("a0"),
                                     UWORD width __asm("d0"),
                                     UWORD height_331 __asm("d1"),
                                     struct ModeInfo *mi __asm("a1"),
                                     RGBFTYPE format __asm("d7"))
{
    if (bi->CardData[0] >= RTG_VERSION_CBPR_HAS_MODEINFO && mi) {
        if (width > mi->Width || height_331 > mi->Height) {
            KPrintF((CONST_STRPTR)"rtgboard: CalculateBytesPerRow: "
                    "refusing oversized displayable bitmap %ldx%ld on "
                    "%ldx%ld mode (virtual desktop unsupported -- this "
                    "board's COMMIT would refuse it anyway)\n",
                    (LONG)width, (LONG)height_331,
                    (LONG)mi->Width, (LONG)mi->Height);
            return 0;
        }
    }

    /* Switch on format for robustness (amirfb does the same): this board
     * only ever advertises RGBFF_R5G6B5, so RGBFB_CLUT is never actually
     * requested, but a wrong answer here breaks geometry silently if P96
     * ever asks anyway. */
    switch (format) {
    case RGBFB_CLUT:
        return width;
    default:
        return rtg_row_bytes_565(width);
    }
}

/* No alignment requirement (BIF_NEEDSALIGNMENT is not set) -- identity. */
static APTR RtgCalculateMemory(struct BoardInfo *bi __asm("a0"),
                               APTR memory __asm("a1"),
                               struct RenderInfo *ri __asm("d0"),
                               RGBFTYPE format __asm("d7"))
{
    (void)bi; (void)ri; (void)format;
    return memory;
}

/* One format, no real hardware mode-mixing constraint -- always report
 * the same RGBFormats this board advertises everywhere else. */
static ULONG RtgGetCompatibleFormats(struct BoardInfo *bi __asm("a0"),
                                     RGBFTYPE format __asm("d7"))
{
    (void)format;
    return bi->RGBFormats;
}

/* No physical display to blank/unblank. */
static BOOL RtgSetDisplay(struct BoardInfo *bi __asm("a0"),
                         BOOL enable __asm("d0"))
{
    (void)bi; (void)enable;
    return TRUE;
}

static LONG RtgResolvePixelClock(struct BoardInfo *bi __asm("a0"),
                                 struct ModeInfo *mi __asm("a1"),
                                 ULONG clock __asm("d0"),
                                 RGBFTYPE format __asm("d7"))
{
    (void)bi; (void)clock; (void)format;
    if (mi) {
        mi->pll1.Clock       = 0;
        mi->pll2.ClockDivide = 0;
        mi->PixelClock       = RTG_PIXELCLOCK(mi->Width, mi->Height);
    }
    return 0;
}

static ULONG RtgGetPixelClock(struct BoardInfo *bi __asm("a0"),
                              struct ModeInfo *mi __asm("a1"),
                              ULONG index __asm("d0"),
                              RGBFTYPE format __asm("d7"))
{
    (void)bi; (void)format;
    if (index != 0)
        return 0;
    if (mi)
        return RTG_PIXELCLOCK(mi->Width, mi->Height);
    /* No ModeInfo to derive a real value from -- the nominal blessed-mode
     * clock (640x480@60), computed with the same shared formula so it can
     * never drift from the mi-driven answer above (RTG_PIXELCLOCK's own
     * comment, amirfb issue #51). */
    return RTG_PIXELCLOCK(RTG_BLESSED_WIDTH, RTG_BLESSED_HEIGHT);
}

/* No real PLL to load -- ResolvePixelClock already recorded the (fixed)
 * nominal clock in the ModeInfo.
 *
 * WARNING -- register-preservation contract (p96-driver-development
 * skill, rule 4; iComp P96 driver-development docs): SetMemoryMode/
 * SetWriteMask/SetClearMask/SetReadPlane below are called inline in hot
 * render paths and MUST preserve every CPU register, not just the usual
 * scratch set (d0/d1/a0/a1) an ordinary vector call gets away with
 * clobbering. These five (SetClock included, same contract) are safe
 * today only because they are empty C functions that -O2 compiles down
 * to a bare `rts` touching no registers at all -- confirmed for this
 * exact toolchain/flags via `m68k-amigaos-objdump -d` (see this
 * increment's build report). That is incidental to the source being
 * trivial, not something the compiler is asked to guarantee: if any of
 * these ever needs a real body, plain C compilation is NOT enough to keep
 * the contract -- wrap it in hand-written asm (movem.l save/restore of
 * everything the C body touches) or re-verify the generated code has
 * zero register clobber before trusting it inline. */
static void RtgSetClock(struct BoardInfo *bi __asm("a0"))
{
    (void)bi;
}

/* Single memory access mode: chunky VRAM, no bank switching. */
static void RtgSetMemoryMode(struct BoardInfo *bi __asm("a0"),
                             RGBFTYPE format __asm("d7"))
{
    (void)bi; (void)format;
}

/* Planar write/clear/read-plane controls are meaningless: this board
 * never advertises RGBFB_NONE (planar). */
static void RtgSetWriteMask(struct BoardInfo *bi __asm("a0"),
                            UBYTE mask __asm("d0"))
{
    (void)bi; (void)mask;
}

static void RtgSetClearMask(struct BoardInfo *bi __asm("a0"),
                            UBYTE mask __asm("d0"))
{
    (void)bi; (void)mask;
}

static void RtgSetReadPlane(struct BoardInfo *bi __asm("a0"),
                            UBYTE plane __asm("d0"))
{
    (void)bi; (void)plane;
}

/* No vsync exists on this board (docs/rtgboard-protocol.md SS8: COMMIT is
 * synchronous, there is no interrupt of any kind) -- return immediately.
 * No dos.library is opened by this driver at all; pacing refinement (a
 * real frame-rate throttle, the way amirfb's own WaitVerticalSync blocks
 * on its RFB server's frame cadence) is deferred until something actually
 * needs it. */
static void RtgWaitVerticalSync(struct BoardInfo *bi __asm("a0"),
                                BOOL wait __asm("d0"))
{
    (void)bi; (void)wait;
}

/* MUST be a self-progressing toggle, not one that only advances on
 * another task's progress -- amirfb's own issue #11 live debugging
 * (2026-08-11) found the P96 core busy-polls this vector in a tight loop
 * on the CALLER's task, and a value that only flips when some OTHER,
 * possibly lower-priority task makes progress is a genuine priority-
 * inversion livelock (amirfb's own case: its RFB server task, lower
 * priority, never got scheduled while a higher-priority caller spun on a
 * bit only that server could flip -- a machine-wide wedge, intermittent
 * by parity). This board has no equivalent async producer at all, so the
 * toggle here is trivially safe -- but the shape (self-progressing on
 * every CALL) is copied deliberately, not by accident, since it's the
 * only shape that can never repeat that failure. */
static BOOL RtgGetVSyncState(struct BoardInfo *bi __asm("a0"),
                             BOOL unused __asm("d0"))
{
    static BOOL toggle;
    (void)bi; (void)unused;
    toggle = (BOOL)!toggle;
    return toggle;
}

/* This board has no interrupt of any kind (docs/rtgboard-protocol.md
 * SS8). */
static BOOL RtgSetInterrupt(struct BoardInfo *bi __asm("a0"),
                           BOOL enable __asm("d0"))
{
    (void)bi; (void)enable;
    return FALSE;
}

/* No accelerator, so nothing is ever busy. */
static void RtgWaitBlitter(struct BoardInfo *bi __asm("a0"))
{
    (void)bi;
}

static ULONG RtgGetVBeamPos(struct BoardInfo *bi __asm("a0"))
{
    (void)bi;
    return 0;
}

/* No real monitor power states to enter. */
static void RtgSetDPMSLevel(struct BoardInfo *bi __asm("a0"),
                           ULONG level __asm("d0"))
{
    (void)bi; (void)level;
}

/* No hardware mouse sprite -- SoftSpriteFlags covers this board's one
 * format, so the core routes the pointer through the soft-sprite path
 * instead. Real, harmless stubs rather than NULL in case any of these
 * are ever called anyway (p96-driver-development skill: never leave a
 * driver-owned vector unset). */
static BOOL RtgSetSprite(struct BoardInfo *bi __asm("a0"),
                        BOOL enable __asm("d0"),
                        RGBFTYPE format __asm("d7"))
{
    (void)bi; (void)enable; (void)format;
    return FALSE;
}

static void RtgSetSpritePosition(struct BoardInfo *bi __asm("a0"),
                                WORD x __asm("d0"),
                                WORD y __asm("d1"),
                                RGBFTYPE format __asm("d7"))
{
    (void)bi; (void)x; (void)y; (void)format;
}

static void RtgSetSpriteImage(struct BoardInfo *bi __asm("a0"),
                              RGBFTYPE format __asm("d7"))
{
    (void)bi; (void)format;
}

static void RtgSetSpriteColor(struct BoardInfo *bi __asm("a0"),
                              UBYTE index __asm("d0"),
                              UBYTE red __asm("d1"),
                              UBYTE green __asm("d2"),
                              UBYTE blue __asm("d3"),
                              RGBFTYPE format __asm("d7"))
{
    (void)bi; (void)index; (void)red; (void)green; (void)blue; (void)format;
}

/* Screen dragging (split-scroll) is not in scope; no split position to
 * program. */
static void RtgSetSplitPosition(struct BoardInfo *bi __asm("a0"),
                               SHORT ypos __asm("d0"))
{
    (void)bi; (void)ypos;
}

/* Only meaningful for boards that switch VRAM access mode per format;
 * this board has exactly one format and one allocation pool to reinit. */
static void RtgReInitMemory(struct BoardInfo *bi __asm("a0"),
                           RGBFTYPE format __asm("d7"))
{
    (void)bi; (void)format;
}

/* Fixed-address allocation is only needed for screen dragging (out of
 * scope) -- report failure honestly rather than pretending to honour a
 * target address this driver can't allocate at. */
static APTR RtgAllocCardMemAbs(struct BoardInfo *bi __asm("a0"),
                              ULONG size __asm("d0"),
                              char *target __asm("a1"))
{
    (void)bi; (void)size; (void)target;
    return NULL;
}

/* ---- InitCard ----------------------------------------------------------- */

static BOOL InitCard(struct BoardInfo *bi __asm("a0"),
                     STRPTR *toolTypes __asm("a1"),
                     struct CardBase *base __asm("a6"))
{
    int i;
    UWORD rtg_version = 0;

    (void)toolTypes;
    (void)base;

    KPrintF((CONST_STRPTR)"rtgboard: InitCard entry\n");

    /* rtg.library is our caller, so it's guaranteed to be on the Exec
     * library list right now -- FindName under Forbid rather than
     * OpenLibrary (nothing to hold open: only lib_Version is wanted, and
     * the core can't be expunged out from under a driver it's mid-way
     * through initialising). Same pattern amirfb_card.c uses; gate
     * constant 41 is the iComp line's floor (RTG_VERSION_CBPR_HAS_
     * MODEINFO's own comment, above) -- this project's installed P96
     * (iComp 3.6.2, rtg.library 43.x) always takes the "has ModeInfo"
     * branch, but the gate is kept as defence for an older core. */
    Forbid();
    {
        struct Library *rtg =
            (struct Library *)FindName(&SysBase->LibList,
                                       (STRPTR)"rtg.library");
        if (rtg)
            rtg_version = rtg->lib_Version;
    }
    Permit();
    KPrintF((CONST_STRPTR)"rtgboard: InitCard: rtg.library version %ld\n",
            (LONG)rtg_version);
    bi->CardData[0] = (ULONG)rtg_version;

    bi->BoardName = (char *)LibName;

    /* BT_uaegfx masquerade, deliberate stand-in, not a placeholder left
     * over from testing (environment facts; amirfb issue #28): a card
     * registered under an unrecognised BoardType can never get a
     * Devs:Picasso96Settings file attached to it (AttachSettings/
     * Picasso96Mode both gate on a hardcoded BoardType-name whitelist),
     * and without a settings-attached mode the entire mode-timing/
     * display-activation vector group (SetGC/SetPanning/
     * ResolvePixelClock/GetPixelClock/SetClock) never fires at all --
     * confirmed dead under BT_Prototype7 on both the free P96 40.x
     * baseline and a real iComp P96 3.6.2 install. BT_uaegfx is on that
     * whitelist and is the same masquerade Emu68's VideoCore.card itself
     * uses (precedent, not guesswork) -- same 0x07DB manufacturer-ID
     * stand-in reasoning as rtgboard_card.h's own RTG_BOARD_MANUFACTURER
     * comment. */
    bi->BoardType = BT_uaegfx;

    /* PCT_S3ViRGE/GCT_S3ViRGE, matching Emu68's VideoCore.card, which
     * pairs its own BT_uaegfx masquerade with these same two fields
     * (amirfb issue #47) -- uaegfx is a fully virtual board with no real
     * RAMDAC/chip of its own, so there's no more "authentic" choice here
     * than Emu68's. */
    bi->PaletteChipType        = PCT_S3ViRGE;
    bi->GraphicsControllerType = GCT_S3ViRGE;

    bi->BitsPerCannon = 8;
    bi->Flags         = 0;   /* no hardware sprite/interrupt/blitter */
    bi->MoniSwitch    = 1;
    bi->ChipFlags     = 0;
    bi->CardFlags     = 0;

    /* One blessed mode, one format: RGB_565 (RGBFB_R5G6B5). No CLUT (this
     * board has no palette registers, docs/rtgboard-protocol.md SS11), no
     * 32bpp yet. */
    bi->RGBFormats     = RGBFF_R5G6B5;
    bi->SoftSpriteFlags = RGBFF_R5G6B5;

    for (i = 0; i < MAXMODES; i++) {
        bi->MaxHorValue[i]      = 0;
        bi->MaxVerValue[i]      = 0;
        bi->MaxHorResolution[i] = 0;
        bi->MaxVerResolution[i] = 0;
        bi->PixelClockCount[i]  = 0;
    }

    /* MaxHorValue/MaxVerValue are the max TOTAL (including blanking),
     * which must exceed the displayable MaxHorResolution/MaxVerResolution
     * -- a nominal ~5% overhead for sync/blanking, not a real CRTC total
     * since this board has no real CRTC (amirfb's own reasoning, same
     * +40/+28 nominal figures). Only HICOLOR is populated: the one
     * blessed mode is 16bpp, nothing else is supported. */
    bi->MaxHorValue[HICOLOR]      = RTG_BLESSED_WIDTH + 40;
    bi->MaxVerValue[HICOLOR]      = RTG_BLESSED_HEIGHT + 28;
    bi->MaxHorResolution[HICOLOR] = RTG_BLESSED_WIDTH;
    bi->MaxVerResolution[HICOLOR] = RTG_BLESSED_HEIGHT;
    bi->PixelClockCount[HICOLOR]  = 1;

    /* Bound so Picasso96Mode can't generate a mode this board doesn't
     * support (amirfb issue #45/#48: left unset, these default to
     * 4096x4096 and a "create all the modes" tool then generates a huge,
     * useless settings file). */
    bi->MaxBMWidth  = RTG_BLESSED_WIDTH;
    bi->MaxBMHeight = RTG_BLESSED_HEIGHT;

    bi->MaxMemorySize = bi->MemorySize;
    bi->MaxChunkSize  = bi->MemorySize;
    bi->MemoryClock   = 0;   /* not applicable -- no real PLL */

    bi->SetSwitch             = RtgSetSwitch;
    bi->SetColorArray         = RtgSetColorArray;
    bi->SetDAC                = RtgSetDAC;
    bi->SetGC                 = RtgSetGC;
    bi->SetPanning            = RtgSetPanning;
    bi->CalculateBytesPerRow  = RtgCalculateBytesPerRow;
    bi->CalculateMemory       = RtgCalculateMemory;
    bi->GetCompatibleFormats  = RtgGetCompatibleFormats;
    bi->SetDisplay            = RtgSetDisplay;
    bi->ResolvePixelClock     = RtgResolvePixelClock;
    bi->GetPixelClock         = RtgGetPixelClock;
    bi->SetClock              = RtgSetClock;
    bi->SetMemoryMode         = RtgSetMemoryMode;
    bi->SetWriteMask          = RtgSetWriteMask;
    bi->SetClearMask          = RtgSetClearMask;
    bi->SetReadPlane          = RtgSetReadPlane;
    bi->WaitVerticalSync      = RtgWaitVerticalSync;
    bi->SetInterrupt          = RtgSetInterrupt;
    bi->WaitBlitter           = RtgWaitBlitter;
    bi->GetVSyncState         = RtgGetVSyncState;
    bi->GetVBeamPos           = RtgGetVBeamPos;
    bi->SetDPMSLevel          = RtgSetDPMSLevel;
    bi->SetSprite             = RtgSetSprite;
    bi->SetSpritePosition     = RtgSetSpritePosition;
    bi->SetSpriteImage        = RtgSetSpriteImage;
    bi->SetSpriteColor        = RtgSetSpriteColor;
    bi->SetSplitPosition      = RtgSetSplitPosition;
    bi->ReInitMemory          = RtgReInitMemory;
    bi->AllocCardMemAbs       = RtgAllocCardMemAbs;

    /* No bi->ResolutionsList registration (docs/rtgboard-protocol.md's
     * own brief and this project's ledger both call this out): modes come
     * from Devs:Picasso96Settings, never from a card driver's
     * ResolutionsList -- registering both hangs boot (amirfb confirmed
     * this live). Nothing to do here; the settings file is generated by
     * tools/rtgboard/make_settings.py and installed by
     * scripts/patch-rtgboard-hdf.sh. */

    KPrintF((CONST_STRPTR)"rtgboard: InitCard complete\n");
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
