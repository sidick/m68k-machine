*-----------------------------------------------------------------------------
* pktport-diagrom.s -- the pktport card's DiagArea boot ROM: no exec
* device, no RDB mounter (that is hostblk-diagrom.s's job for a different
* card) -- this ROM's entire job is to make PKT0: exist in the DosList at
* boot, with its handler process started lazily on first access, with no
* L:pktport-handler file and no Mountlist entry required at all.
*
* MIT License. Copyright (c) 2026 the m68k Machine project. See ../LICENSE.
*
* Scope, this increment: a struct Resident (DAC_CONFIGTIME) whose rt_Init
* (RtInit) copies the guest handler's flat code blob (embedded in this ROM
* image right after the assembled DiagArea+RtInit code -- see "ROM layout"
* below) out of the board's own AUTOCONFIG window into a freshly AllocMem'd
* block, wraps it in a hand-built one-segment BCPL seglist, hand-builds a
* struct DeviceNode naming "PKT0" with dn_SegList pointing at that seglist
* and dn_Task = 0 (so AmigaDOS starts the handler process itself, lazily,
* the first time a client actually touches PKT0: -- expansion.doc's own
* DeviceNode field comment: "If this is null when the node is accessed, a
* task will be started up"), and calls AddBootNode with a NULL ConfigDev
* to register it as a non-bootable DOS node (expansion.doc's own words:
* "Pass a NULL ConfigDev pointer to create a non-bootable node"). NOT
* implemented this increment: making PKT0: itself bootable (no
* da_BootPoint-driven autoboot, no BootNode with a real ConfigDev) --
* docs/pktport-protocol.md section 9 and this file's own end-of-file
* summary comment say so explicitly.
*
* This file follows m68k/hostblk-rom/hostblk-diagrom.s's and
* m68k/input-rom/input-diagrom.s's established conventions throughout
* (DiagArea copy semantics, the two addressing regimes, DAC_CONFIGTIME,
* the "NO SPACES in dc.w expressions" vasm trap) without re-litigating
* them -- read those two files' own headers for the full story. This
* header covers only what is new or different here.
*
* Two addressing regimes meet in this one file, exactly as in
* hostblk-diagrom.s/input-diagrom.s:
*
*   - DiagStart..EndCopy is *copied* by expansion.library to a RAM
*     address chosen at boot time, so anything in it holding its own
*     address (rt_MatchTag/rt_EndSkip/rt_Name/rt_IdString) needs
*     DiagEntry's runtime patch, exactly as both sibling files already
*     explain at length.
*   - Everything after EndCopy (RtInit and everything it calls, including
*     the handler blob itself) executes *in place*, straight off this
*     board's own AUTOCONFIG window, so it uses ordinary PC-relative
*     addressing (`lea LABEL(pc),aN`) freely, needing no patching --
*     this is exactly why the handler blob can simply be read PC-relative
*     out of the ROM window and AllocMem-copied verbatim, with no
*     relocation step of its own (the blob is itself fully
*     position-independent code -- see pktport-handler.s's own header for
*     that half of the story).
*
* da_BootPoint is NOT zero, even though this card is never a BootNode
* itself and strap can never reach da_BootPoint here (this ROM never
* calls AddBootNode with a real ConfigDev, only NULL) -- input-diagrom.s's
* own header already recorded the empirical fact behind this the hard
* way: RKRM's "Events At DIAG Time" / libraries/configregs.h's own
* description of DAC_CONFIGTIME ("call da_BootPoint when first configing
* the device") turns out to gate the DiagArea RAM copy itself, not merely
* whether a boot routine runs -- a *zero* da_BootPoint means
* expansion.library never copies the DiagArea into RAM at all, silently
* cancelling DiagEntry (and therefore this card's whole romtag) along
* with it. So `BootStub` below exists purely to give da_BootPoint a
* non-zero, in-range value; nothing about this file's actual job depends
* on it ever running.
*
* ---- ROM layout (this file's own addition to the sibling convention) ----
*
* This ROM image is [DiagArea copy region][in-place RtInit code and
* subroutines][a 4-byte big-endian blob length][the handler's flat
* -Fbin code blob, byte-for-byte]. The length word's own label
* (BlobLenWord, just below EndCopy's in-place code) is the last symbol in
* this file -- ../../scripts/build-pktport-rom.sh assembles this file
* alone first (producing a ROM image whose last 4 bytes are
* BlobLenWord's placeholder zero), then overwrites those last 4 bytes
* with the real big-endian byte length of
* m68k/pktport-handler/pktport-handler.bin (../../scripts/
* build-pktport-handler.sh's own -Fbin output mode) and appends that
* blob's bytes immediately after. RtInit locates both purely by PC-
* relative reference to BlobLenWord (`lea BlobLenWord(pc),a0`), so this
* works regardless of where expansion.library maps this board's window.
* docs/pktport-protocol.md section 1 documents this same layout as the
* wire contract between this file and the build script.
*
* ---- SegList layout (cited, not guessed) ----
*
* A device node's dn_SegList is a BPTR to a one-segment BCPL seglist in
* the exact shape LoadSeg() itself produces (Autodocs/dos.doc's own
* LoadSeg entry: "chaining together the segments with BPTR's on their
* first words. The end of the chain is indicated by a zero."), extended
* with the standard "length longword immediately before the BPTR's own
* target" convention UnLoadSeg's own contract requires to be able to
* FreeMem() the block it did not itself allocate piece-by-piece
* (Autodocs/dos.doc's UnLoadSeg entry: "Unload a seglist... Overlaid
* segments will have all needed cleanup done" -- this only works if the
* size prefix truthfully describes the whole AllocMem'd block, which is
* exactly how this file writes it, even though nothing in this increment
* ever calls UnLoadSeg on this handler). Concretely, for a BPTR `seg`
* (a byte address `segAddr = seg << 2`):
*
*   segAddr - 4   : ULONG, the *whole* AllocMem'd block's byte length
*                   (header + code), so UnLoadSeg's FreeMem(segAddr-4,
*                   thatLength) is correct if ever exercised
*   segAddr + 0   : BPTR, next segment (0 -- this handler is one segment)
*   segAddr + 4   : the handler's code, copied verbatim from this ROM
*
* SEG_HEADER_SIZE below (8 bytes: the length longword plus the next
* BPTR) is exactly `segAddr - (segAddr - 4)` plus the 4 bytes of `next`
* -- i.e. the AllocMem'd block is `SEG_HEADER_SIZE + blob length` bytes,
* with `segAddr = allocBase + 4`. AllocMem's own documented minimum
* alignment (8 bytes) keeps `allocBase + 4` a legal 4-byte-aligned BPTR
* target without any extra alignment work (this project's own "AllocMem
* is 8-aligned (BPTRs safe)" note).
*
* ---- struct DeviceNode (dos/filehandler.h) -- hand-derived offsets ----
*
* dn_Next.l(BPTR) dn_Type.l(ULONG) dn_Task.l(MsgPort*) dn_Lock.l(BPTR)
* dn_Handler.l(BSTR) dn_StackSize.l(ULONG) dn_Priority.l(LONG)
* dn_Startup.l(BPTR) dn_SegList.l(BPTR) dn_GlobalVec.l(BPTR)
* dn_Name.l(BSTR) -- 11 longs, 44 bytes total. Every field here is a
* plain ULONG-sized slot (Autodocs/expansion.doc's own struct listing),
* so the offsets below are a straight 4-byte-stride hand sum, the same
* citation discipline hostblk-diagrom.s's own struct-offset blocks use.
* dn_Name is a BSTR (a BPTR to a length-prefixed byte string), not a
* BPTR-to-BPTR or a plain C string -- confirmed against
* Autodocs/expansion.doc's own DeviceNode field comment ("the node name,
* e.g. '\3','D','F','3'"), which is exactly the encoding this file writes
* for "PKT0" below.
*
* ---- AddDosNode vs AddBootNode: an ambiguity this file resolves ----
*
* The task brief that named this deliverable's shape said "AddDosNode(0,
* ADNF_STARTPROC?..." -- but Autodocs/expansion.doc's own AddDosNode
* entry says plainly: "This is the old (pre V36) function that works
* just like AddBootNode(). It should only be used if you *MUST* work in
* a 1.3 system." This project's Kickstart target is 3.2.2 (V47), and
* hostblk-diagrom.s already established AddBootNode as this project's
* tool for exactly this situation (cold-start-time DiagArea code adding
* a DOS node before DOS is necessarily running). So this file calls
* AddBootNode, not AddDosNode, passing a NULL ConfigDev to get its
* documented non-bootable-node behaviour (expansion.doc: "Pass a NULL
* ConfigDev pointer to create a non-bootable node") -- exactly matching
* docs/pktport-protocol.md section 9's posture for this increment
* ("auto-MOUNTS, does not make bootable"). flags = 0, deliberately not
* ADNF_STARTPROC: expansion.doc's own words, "Normally the process is
* started only when the device node is first referenced" -- exactly the
* lazy-start behaviour dn_Task = 0 already documents above, and exactly
* what the task brief asked for ("do NOT pass a flag that boots it
* before dos.library is ready").
*
* ---- The PKT0-name-collision posture: resolved without a live DosList
* query ----
*
* AddBootNode's own autodoc documents two cases: "1) If dos is running,
* add a new disk type device immediatly. 2) If dos is not yet running,
* save information for later use by the system." Both cases eventually
* fund through the same underlying dos.library AddDosEntry(), whose own
* autodoc says plainly it "Can fail if it conflicts with an existing
* entry (such as ... another device of the same name)" -- gracefully,
* not by crashing. This file therefore does *not* attempt to query the
* live DosList itself from RtInit (which runs during Exec's cold-start
* resident scan -- a point at which dos.library's own Resident may not
* yet have been initialised, since hostblk-diagrom.s's own BootEntry has
* to explicitly FindResident+InitResident dos.library itself to get an
* early, safe handle on it): whichever of AddBootNode's two documented
* cases actually applies at RtInit's own call site, a name conflict is
* reported back through AddBootNode's own D0 result, which this file
* already treats as an allocation-style failure -- clean rollback (both
* AllocMem'd blocks freed), no node left half-registered, RtInit returns
* NULL either way. This is this file's own "field-upgrade posture": if
* something else (an L:-installed newer setup, mounted first) already
* claims the name PKT0 by the time AddBootNode's own call actually tries
* to register this node, this ROM yields rather than fighting for the
* name -- it does nothing further, silently, from the guest's point of
* view (docs/pktport-protocol.md section 1 records this decision too).
*-----------------------------------------------------------------------------

* ============================================================================
* exec.library / expansion.library LVOs -- transcribed by hand, the same
* citation discipline hostblk-diagrom.s/input-diagrom.s use.
* ============================================================================
_LVOAllocMem            equ     -198
_LVOFreeMem             equ     -210
_LVOOpenLibrary         equ     -552
_LVOAddBootNode         equ     -36

* exec/memory.h.
MEMF_PUBLIC     equ     (1<<0)
MEMF_CLEAR      equ     (1<<16)

* exec/resident.h: struct Resident (26 bytes) -- identical layout to
* hostblk-diagrom.s's own citation.
RT_MATCHWORD    equ     0
RT_MATCHTAG     equ     2
RT_ENDSKIP      equ     6
RT_FLAGS        equ     10
RT_VERSION      equ     11
RT_TYPE         equ     12
RT_PRI          equ     13
RT_NAME         equ     14
RT_IDSTRING     equ     18
RT_INIT         equ     22
RT_SIZE         equ     26

RTC_MATCHWORD   equ     $4afc
RTF_COLDSTART   equ     1

* exec/nodes.h: NT_UNKNOWN -- this Resident stands up no exec Task,
* Device or Library of its own (unlike hostblk's NT_DEVICE or input's
* NT_TASK): its entire product is a DOS DeviceNode, for which exec/
* nodes.h names no better-fitting NT_ constant. rt_Type has no effect on
* non-AUTOINIT InitResident's own behaviour either way (Autodocs/
* exec.doc), so this is documentation of intent, the same posture
* input-diagrom.s's header states for its own (different) choice.
NT_UNKNOWN      equ     0

* libraries/configregs.h: struct DiagArea's da_Config bit layout --
* identical to both sibling files' own citation.
DAC_WORDWIDE    equ     $80
DAC_CONFIGTIME  equ     $10

* crates/machine-core/src/pktport.rs: `pub const ROM_BASE: u32 = 0x1000;`
* -- where this file's assembled image is mapped within the board's own
* AUTOCONFIG window, same convention hostblk.rs/input.rs already use.
PKT_ROM_BASE    equ     $1000

* docs/pktport-protocol.md section 3: VERSION register, read-only, at
* offset 0 of the board's register file (crates/machine-core/src/
* pktport.rs's own `reg::VERSION`).
PKT_VERSION     equ     $00

* dos/filehandler.h: struct DeviceNode -- see this file's header for the
* full field-by-field citation.
DN_NEXT         equ     0
DN_TYPE         equ     4
DN_TASK         equ     8
DN_LOCK         equ     12
DN_HANDLER      equ     16
DN_STACKSIZE    equ     20
DN_PRIORITY     equ     24
DN_STARTUP      equ     28
DN_SEGLIST      equ     32
DN_GLOBALVEC    equ     36
DN_NAME         equ     40
DN_SIZE         equ     44

* This file's own layout choice: the BSTR naming "PKT0" lives right after
* the DeviceNode in the same AllocMem'd block (this file's header, "one
* AllocMem block" reasoning also applies here for the same "AllocMem's
* own alignment already keeps every BPTR in this block legal" reason).
* 8 bytes is generous for a 1-length-byte + 4-char BSTR.
NAME_OFFSET     equ     DN_SIZE
NODE_ALLOC_SIZE equ     NAME_OFFSET+8

* This file's header, "SegList layout": the length-prefix and next-BPTR
* header preceding the handler's copied code.
SEG_HEADER_SIZE equ     8

*-----------------------------------------------------------------------------
* struct DiagArea (libraries/configregs.h): identical 14-byte layout to
* both sibling files. da_DiagPoint/da_BootPoint/da_Name are word offsets
* from DiagStart.
*-----------------------------------------------------------------------------
DiagStart:
        dc.b    DAC_WORDWIDE+DAC_CONFIGTIME  ; da_Config
        dc.b    0                       ; da_Flags -- none defined, must be 0
        dc.w    EndCopy-DiagStart       ; da_Size: bytes copied into RAM
        dc.w    DiagEntry-DiagStart     ; da_DiagPoint
        dc.w    BootStub-DiagStart      ; da_BootPoint -- non-zero only to
                                         ; keep the RAM copy happening (this
                                         ; file's header); never actually
                                         ; reached by strap
        dc.w    0                       ; da_Name: no identifier string
        dc.w    0                       ; da_Reserved01 -- must be zero
        dc.w    0                       ; da_Reserved02 -- must be zero

* Diagnostic scratch cell: DiagEntry writes VERSION here (offset 14,
* right after the DiagArea header -- identical mechanism/offset to both
* sibling files, asserted the same way by this card's own Rust tests).
DiagMarker:
        dc.l    0

* Second scratch cell (offset 18): BootStub counts its own invocations
* here -- input-diagrom.s's own BootMarker convention, restated here for
* the same reason: nothing in this file depends on the answer, but it
* costs four bytes to stop being folklore.
BootMarker:
        dc.l    0

*-----------------------------------------------------------------------------
* BootStub -- da_BootPoint's target. Required non-zero purely so the
* DiagArea is copied at all (this file's header). A2 is not guaranteed
* here the way it is for DiagEntry, so the RAM copy's base is recovered
* from this routine's own address with a PC-relative LEA, exactly as
* input-diagrom.s's own BootStub does.
*-----------------------------------------------------------------------------
BootStub:
        lea     BootStub(pc),a1
        move.l  #$B007B007,(BootMarker-BootStub)(a1)
        moveq   #1,d0
        rts

*-----------------------------------------------------------------------------
* DiagEntry -- da_DiagPoint. Calling convention (configregs.h; both
* sibling files quote it in full): A0=board base, A2=RAM copy base.
* Returns D0 non-zero to keep the RAM copy. Identical patch pattern to
* both sibling files.
*-----------------------------------------------------------------------------
DiagEntry:
        move.l  PKT_VERSION(a0),d0     ; read this card's own VERSION register
        move.l  d0,(DiagMarker-DiagStart)(a2)  ; proof DiagEntry actually ran
        move.l  a2,d0
        add.l   d0,(RtMatchTag-DiagStart)(a2)
        add.l   d0,(RtEndSkip-DiagStart)(a2)
        add.l   d0,(RtName-DiagStart)(a2)
        add.l   d0,(RtIdString-DiagStart)(a2)
        move.l  a0,d0
        add.l   #PKT_ROM_BASE,d0
        add.l   d0,(RtInitField-DiagStart)(a2)  ; rt_Init lives in the
                                                  ; always-mapped window at
                                                  ; boardbase+ROM_BASE
        moveq   #1,d0
        rts

*-----------------------------------------------------------------------------
* struct Resident ("Romtag"; exec/resident.h). rt_Type is NT_UNKNOWN --
* see this file's header. rt_Flags is RTF_COLDSTART only (non-AUTOINIT):
* rt_Init is ordinary code, identical citation to both sibling files.
*-----------------------------------------------------------------------------
Romtag:
        dc.w    RTC_MATCHWORD                   ; rt_MatchWord
RtMatchTag:
        dc.l    Romtag-DiagStart                ; rt_MatchTag (patched: +a2)
RtEndSkip:
        dc.l    EndCopy-DiagStart                ; rt_EndSkip (patched: +a2)
        dc.b    RTF_COLDSTART                    ; rt_Flags
        dc.b    0                                ; rt_Version
        dc.b    NT_UNKNOWN                       ; rt_Type
        dc.b    20                               ; rt_Pri
RtName:
        dc.l    RomName-DiagStart                ; rt_Name (patched: +a2)
RtIdString:
        dc.l    IdString-DiagStart                ; rt_IdString (patched: +a2)
RtInitField:
        dc.l    RtInit-DiagStart                  ; rt_Init (patched: +a0,
                                                    ; NOT in the RAM copy --
                                                    ; see both sibling files)

RomName:
        dc.b    "pktport.rom",0
        even
IdString:
        dc.b    "pktport DiagArea 1.0 (2026)",0
        even

EndCopy:

*=============================================================================
* Everything below here executes in place, straight off this board's own
* AUTOCONFIG window -- see the file header for why that means ordinary
* PC-relative addressing (no patching) but rt_Init's own romtag field was
* still patched with (board base + PKT_ROM_BASE), not with a2.
*=============================================================================

ExpName:
        dc.b    "expansion.library",0
        even

*-----------------------------------------------------------------------------
* RtInit -- rt_Init, called by InitResident's non-AUTOINIT path
* (Autodocs/exec.doc): D0=0, A0=segList (NULL for a ROM module), A6=
* ExecBase. Builds the handler's seglist and DeviceNode (this file's
* header) and calls AddBootNode with a NULL ConfigDev. Always returns
* D0=NULL (this Resident registers nothing in exec's own LibList/
* DeviceList -- its product is a DOS node, a side effect of running, not
* a returned base). Every allocation failure past this point unwinds
* cleanly: no node is ever left half-built or half-registered (this
* file's header, "Guard every allocation failure").
*-----------------------------------------------------------------------------
RtInit:
        movem.l d2-d7/a2-a6,-(sp)

        lea     DiagStart(pc),a5
        suba.l  #PKT_ROM_BASE,a5        ; a5 = board base (file header)

        move.l  PKT_VERSION(a5),d0      ; refuse a protocol version we don't
        cmp.l   #1,d0                   ; know (docs/pktport-protocol.md
        bne     .fail                   ; section 3)

        ; expansion.library: needed for AddBootNode itself. Opened once
        ; and never closed on any path, including failure -- hostblk-
        ; diagrom.s's own RtInit documents the same choice and the same
        ; reasoning (a core system library for the life of the machine
        ; regardless).
        lea     ExpName(pc),a1
        moveq   #0,d0
        jsr     _LVOOpenLibrary(a6)     ; a6 = SysBase, per InitResident's
        tst.l   d0                      ; own entry convention
        beq     .fail
        move.l  d0,a4                   ; a4 = ExpansionBase, kept live

        ; Locate the handler blob (this file's header, "ROM layout"):
        ; PC-relative, in-place -- no patching needed, this code never
        ; moves.
        lea     BlobLenWord(pc),a2
        move.l  (a2),d2                 ; d2 = blob length, kept live
        lea     4(a2),a3                ; a3 = blob bytes (in the ROM
                                         ; window), kept live

        ; Allocate the one-segment seglist block (this file's header,
        ; "SegList layout"): SEG_HEADER_SIZE + blob length.
        move.l  d2,d3
        add.l   #SEG_HEADER_SIZE,d3     ; d3 = total block size, kept live
                                         ; (needed again for the length
                                         ; prefix and for cleanup)
        move.l  d3,d0
        move.l  #(MEMF_PUBLIC+MEMF_CLEAR),d1
        jsr     _LVOAllocMem(a6)
        tst.l   d0
        beq     .fail                   ; nothing allocated yet -- plain fail
        move.l  d0,a2                   ; a2 = seg alloc base, kept live
                                         ; (a2's DiagArea-copy use is long
                                         ; over by the time RtInit runs)

        move.l  d3,(a2)                 ; length prefix = whole block size
        lea     4(a2),a0                ; a0 = segAddr
        clr.l   (a0)                    ; next BPTR = 0 (one segment)
        lea     4(a0),a0                ; a0 = code destination
        ; Byte copy, 32-bit count (not dbf's 16-bit loop count -- a
        ; deliberately safe choice: this blob is small today, but nothing
        ; here should silently miscount if it ever isn't).
        move.l  d2,d4
.copy_loop:
        tst.l   d4
        beq.s   .copy_done
        move.b  (a3)+,(a0)+
        subq.l  #1,d4
        bra.s   .copy_loop
.copy_done:

        ; Allocate the DeviceNode + its inline "PKT0" BSTR.
        move.l  #NODE_ALLOC_SIZE,d0
        move.l  #(MEMF_PUBLIC+MEMF_CLEAR),d1
        jsr     _LVOAllocMem(a6)
        tst.l   d0
        beq     .fail_free_seg
        move.l  d0,a3                   ; a3 = DeviceNode*, kept live

        move.l  #8192,DN_STACKSIZE(a3)
        move.l  #10,DN_PRIORITY(a3)
        move.l  #-1,DN_GLOBALVEC(a3)    ; not a BCPL program (pktport-
                                         ; handler.s's own header: GlobVec
                                         ; = -1 is this project's existing
                                         ; L:-installed handler's own
                                         ; Mountlist convention too)
        ; dn_Next/dn_Type/dn_Task/dn_Lock/dn_Handler/dn_Startup are
        ; already 0 from MEMF_CLEAR -- dn_Task = 0 is exactly what makes
        ; DOS start this handler lazily (this file's header).

        lea     4(a2),a0                ; segAddr again (recomputed, not
        move.l  a0,d0                   ; stashed -- a2 never moved)
        lsr.l   #2,d0                    ; -> BPTR
        move.l  d0,DN_SEGLIST(a3)

        lea     NAME_OFFSET(a3),a0      ; the inline "PKT0" BSTR (file
        move.b  #4,(a0)                 ; header: dn_Name is a BSTR)
        move.b  #'P',1(a0)
        move.b  #'K',2(a0)
        move.b  #'T',3(a0)
        move.b  #'0',4(a0)
        move.l  a0,d0
        lsr.l   #2,d0
        move.l  d0,DN_NAME(a3)

        ; AddBootNode(bootPri=0, flags=0, deviceNode=a3, configDev=NULL)
        ; -- this file's header explains both the AddDosNode-vs-
        ; AddBootNode choice and the NULL ConfigDev (non-bootable node).
        moveq   #0,d0                   ; bootPri
        moveq   #0,d1                   ; flags -- NOT ADNF_STARTPROC
        move.l  a3,a0                   ; deviceNode
        suba.l  a1,a1                   ; configDev = NULL
        move.l  a4,a6                   ; a6 = ExpansionBase for this call
        jsr     _LVOAddBootNode(a6)
        ; a6 is reloaded with SysBase on every path below that needs it
        ; again (both cleanup branches do `move.l 4.w,a6` themselves), so
        ; nothing here restores it -- the success path needs no further
        ; library call at all.

        tst.l   d0
        bne     .done                   ; success: node registered (or
                                         ; safely queued -- AddBootNode's
                                         ; own documented cases), nothing
                                         ; further to do

        ; AddBootNode itself failed (out of memory, or -- this file's
        ; header -- a genuine PKT0 name conflict at the point it actually
        ; tried to register the node): unwind both allocations, leaving
        ; no partial state.
        move.l  4.w,a6
        move.l  a3,a0
        move.l  #NODE_ALLOC_SIZE,d0
        jsr     _LVOFreeMem(a6)
        move.l  a2,a0
        move.l  d3,d0
        jsr     _LVOFreeMem(a6)
        bra     .fail

.fail_free_seg:
        ; DeviceNode allocation failed; the seg block from earlier must
        ; still be freed (file header: "no partial state").
        move.l  4.w,a6
        move.l  a2,a0
        move.l  d3,d0
        jsr     _LVOFreeMem(a6)
        bra     .fail

.done:
.fail:
        moveq   #0,d0
        movem.l (sp)+,d2-d7/a2-a6
        rts

* Blob length + blob bytes (this file's header, "ROM layout"). Must be
* the LAST thing in this source file: ../../scripts/build-pktport-rom.sh
* locates BlobLenWord's own offset as "assembled file length minus 4" and
* patches those 4 bytes, then appends the handler blob right after --
* both only work if nothing in this file follows BlobLenWord. `even`
* guards the move.l reads above against an odd-address CPU exception on
* real 68000 hardware (this project's own "`even` is NOT 4-alignment"
* lore: it rounds to 2, which is all a move.l here structurally needs
* since everything before it is already word-sized or larger, but stated
* explicitly rather than assumed).
        even
BlobLenWord:
        dc.l    0
