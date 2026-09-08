*-----------------------------------------------------------------------------
* pktport-handler.s -- the guest-side thin AmigaDOS handler stub for the
* pktport card (docs/pktport-protocol.md): the 68k half of ADR 0004's
* DosPacket transport. This is the handler process, not a device driver --
* it never touches trackdisk-style structures at all; every AmigaDOS
* packet arriving at its own MsgPort is translated into a pktport wire
* request per docs/pktport-protocol.md sections 2-5, submitted through the
* card's registers, and the reply translated back.
*
* MIT License. Copyright (c) 2026 the m68k Machine project. See ../LICENSE.
*
* Mount as a standard handler entry (dos/filehandler.h's DeviceNode shape,
* AmigaDOS's "Mount" convention -- amigados-rkrm's handlers-filesystems
* chapter, "Starting a Handler"). Exact Mountlist stanza this file expects
* to be started from (DEVS:Mountlist.pktport in this project's own e2e
* script, scripts/pktport-e2e.sh):
*
*   PKT0:    Handler   = L:pktport-handler
*            GlobVec   = -1
*            StackSize = 8192
*            Startup   = 0
*
* GlobVec = -1 selects the plain C/assembler handler-startup path (not
* BCPL): the startup packet is sent to this process's own pr_MsgPort,
* never delivered in a register (amigados-rkrm's handlers-filesystems
* chapter, "Starting a Handler" -- confirmed against the chapter's own
* worked example, which this file's Start: routine follows). The
* Mountlist keyword is GLOBVEC (case-insensitive), not "GlobalVec" --
* the latter is the *C struct field name* (dol_GlobVec/dn_GlobalVec,
* dos/dosextens.h) the keyword sets, and a real AmigaOS Mount command
* rejects the field name itself with "'GlobalVec' is not a valid
* keyword" (a mistake this file's own e2e run made once and fixed --
* scripts/pktport-e2e.sh's own history). Startup = 0 means dp_Arg2 (the
* FileSysStartupMsg BPTR real filesystems get) carries nothing this
* handler needs -- it never inspects it; every configuration fact it
* needs (protocol version, board base, capacity) comes from the card's
* own registers via expansion.library, per this project's
* no-hardcoded-addresses rule.
*
* Build: vasm -Fhunkexe (see ../../scripts/build-pktport-handler.sh) -- a
* genuine relocatable AmigaDOS load-file, unlike hostblk-diagrom.s's flat
* -Fbin ROM image. That distinction matters for how this file addresses
* its own data: hostblk-diagrom.s uses lea LABEL(pc),aN everywhere because
* its DiagArea gets *copied* to a RAM address chosen at boot time and its
* AUTOCONFIG-window code executes *in place* with no loader involved at
* all -- PC-relative addressing is the only thing that survives either
* situation. A vasm -Fhunkexe file is different: AmigaDOS's LoadSeg()
* relocates every absolute long reference to this file's own labels at
* load time (the hunk format carries relocation records for exactly this
* purpose), so plain absolute addressing of this file's own data labels
* (G_DESC, G_BOARDBASE, ...) is correct and is what ordinary hand-written
* AmigaDOS assembly programs do. No PC-relative addressing appears
* anywhere in this file, deliberately -- it would work but adds nothing a
* loaded, relocated hunk executable needs.
*
* NO SPACES inside dc.w/dc.l label-difference expressions -- this
* project's own vasm pitfall (hostblk-diagrom.s's header, confirmed
* against vasm 2.0b: `dc.w a - b` silently assembles as `dc.w a`, the
* rest of the line read as a comment). This file uses no such
* expressions at all (no jump table built from label differences; the
* action dispatcher below is a plain cmp/beq chain), so the trap does not
* apply here, but is worth restating since it bit this project once.
*
* Completion model: v1 is a synchronous poll on the descriptor's own
* STATUS field (docs/pktport-protocol.md section 2: "the host writes
* RES1/RES2 first, STATUS last... the stub may therefore trust RES1/RES2
* the moment it observes STATUS == 1"). CAPACITY is 1 in this protocol
* version (section 3) and this handler only ever has one packet in flight
* at a time (it processes one DosPacket per iteration of its own message
* loop before submitting the next), so a busy-poll never contends with
* itself. An INT2-driven completion (docs/pktport-protocol.md section 3's
* INT_STATUS/INT_ENABLE registers, unused by this file) plus a separate
* signal-and-Wait() split -- the same task/interrupt split
* m68k/input-rom/input-diagrom.s's retrospective (docs/input-protocol.md
* section 13) describes for that card -- is the documented v1-to-v2
* upgrade path, not built here: it buys the handler process the ability
* to service more than one DOS client concurrently without spinning the
* CPU on STATUS, which a single-outstanding-request v1 protocol has no
* need for yet.
*
* Struct offsets below are hand-transcribed from the NDK 3.2 headers
* named at each equ block, in this project's established style
* (hostblk-diagrom.s's own header explains why: vasm has no C
* struct-layout support, so every offset here is a citation, not a
* guess). Every offset in this file was independently verified against
* the NDK 3.2 dos/dosextens.h, dos/filehandler.h and exec/nodes.h/
* exec/tasks.h/exec/ports.h headers while writing it (not merely copied
* from the task brief that named them) -- struct Task is 92 bytes
* (tc_Node(14) + 4 flag/nest bytes + 4 sig longs(16) + 2 trap words(4) +
* 4 APTR pairs(16) + 3 more APTRs(12) + 2 function ptrs(8) +
* tc_MemEntry List(14) + tc_UserData(4) = 92), which is exactly why
* Process's pr_MsgPort (the first field after the embedded struct Task)
* sits at offset 92 -- confirmed by hand-summing tc_* rather than taken
* on faith.
*
* Ambiguities in docs/pktport-protocol.md this file had to resolve on
* its own (see also this deliverable's report to the supervisor for the
* full reasoning):
*
*   - Name arguments arrive still carrying this mount's own "PKT0:"
*     prefix, not pre-stripped to a bare relative path -- found by
*     tracing a real Kickstart 3.2.2 (dos.library 47.30) run: `Dir PKT0:`
*     sends `ACTION_LOCATE_OBJECT` with `dp_Arg2` = the BSTR `"PKT0:"`
*     itself (not an empty string), and `Echo hello >PKT0:marker` sends
*     `ACTION_FINDOUTPUT` with `dp_Arg3` = `"PKT0:marker"`. Neither
*     `docs/pktport-protocol.md` nor the RKM prose this file's author
*     read beforehand states this either way -- the RKM's own
*     `ACTION_LOCATE_OBJECT` description says `dp_Arg2` is "an absolute
*     path or a path relative to dp_Arg1", which reads as already
*     stripped of any device prefix, but real `dos.library` does not
*     strip it when nothing else about the path needed resolving through
*     `GetDeviceProc()`. `crates/machine-hosted/src/pktvol.rs`'s own
*     `locate_object`/`find_input`/etc. never strip one either (out of
*     this deliverable's reach — machine-hosted's `pktvol.rs` logic is
*     off limits per this task's own rules), so every name argument this
*     file forwards is passed through `strip_colon_prefix` first: a
*     colon can never legally appear anywhere in a real Amiga path
*     component (it is the volume/device separator, reserved), so
*     scanning a name for its first `:` and keeping only what follows is
*     unambiguous and safe for every name this protocol carries — a name
*     with no colon at all (the common case: `Dir` listing an entry by
*     its bare relative name, `CD`ing into a subdirectory, …) is left
*     untouched. This is the one place this file actually reads guest
*     memory content rather than forwarding a BPTR untouched, a real
*     (if narrow) departure from this file's own "thin stub, the backend
*     interprets names" framing elsewhere — necessary because the
*     backend that would otherwise do this interpretation is out of
*     reach for this fix.
*   - ACTION_FINDINPUT/FINDOUTPUT/FINDUPDATE's wire RES1/RES2 convention
*     (section 5's dagger note): success is RES1 = DOSTRUE with the
*     host handle in RES2; failure is RES1 = DOSFALSE with the error in
*     RES2. RES1 is the discriminator -- an earlier revision of the
*     protocol left RES1 always 0 and this file discriminated RES2 by
*     membership in section 6's error-code table, which broke the moment
*     more file handles were opened than the largest error code (the
*     205th handle collides with ERROR_OBJECT_NOT_FOUND); the protocol
*     was fixed at the source instead.
*   - fl_Access on a freshly minted FileLock: only ACTION_LOCATE_OBJECT's
*     wire request actually carries a real access mode (dp_Arg3, the
*     packet's own `mode` argument) -- ACTION_COPY_DIR, ACTION_PARENT and
*     ACTION_CREATE_DIR's wire requests carry only a lock/name, no mode.
*     This file defaults those three to SHARED_LOCK: `PktVolume` never
*     inspects fl_Access at all (crates/machine-hosted/src/pktvol.rs has
*     no read of it anywhere), so this is cosmetic to any code that
*     merely stores and later re-reads it, and SHARED_LOCK is the
*     documented real convention for ACTION_PARENT specifically
*     (ParentDir() is always shared in real AmigaDOS); applying it
*     uniformly rather than inventing three different defaults keeps this
*     file's own logic in one place.
*   - Startup failure error codes: the protocol doc names no specific
*     AmigaDOS error for "the pktport card isn't on the bus" or "its
*     VERSION register doesn't match". This file uses
*     ERROR_DEVICE_NOT_MOUNTED (218, "not mounted" -- section 6's own
*     wording) for a missing card, and ERROR_OBJECT_WRONG_TYPE (212) for
*     a VERSION mismatch (the board that answered isn't the pktport
*     version this file understands) -- both drawn from section 6's own
*     mandatory code list rather than invented.
*-----------------------------------------------------------------------------

* ============================================================================
* exec.library LVOs (Include_I/lvo/exec_lib.i -- transcribed by hand, same
* citation discipline hostblk-diagrom.s uses: vasm never reads that file,
* but every number below is exactly its number, independently confirmed
* against the NDK 3.2 skill's copy of it while writing this file).
* ============================================================================
_LVOFindTask    equ     -294
_LVOWaitPort    equ     -384
_LVOGetMsg      equ     -372
_LVOPutMsg      equ     -366
_LVOReplyMsg    equ     -378
_LVOAllocMem    equ     -198
_LVOFreeMem     equ     -210
_LVOOpenLibrary equ     -552

* expansion.library LVO (Include_I/lvo/expansion_lib.i).
_LVOFindConfigDev equ   -72

* ============================================================================
* AmigaOS structure offsets. Sources cited per block; see this file's
* header for the verification note.
* ============================================================================

* exec/nodes.h: struct Node -- ln_Succ.l ln_Pred.l ln_Type.b ln_Pri.b
* ln_Name.l (14 bytes total). Only ln_Name is used here (to recognise a
* DosPacket-carrying Message: amigados-rkrm's handlers-filesystems
* chapter's own worked example tests `msg->mn_Node.ln_Name` for this
* exact purpose).
LN_NAME         equ     10

* dos/dosextens.h: struct Process -- struct Task pr_Task(92, see this
* file's header for the byte-by-byte sum), struct MsgPort pr_MsgPort
* immediately after it.
PR_MSGPORT      equ     92

* dos/dosextens.h: struct DosPacket -- dp_Link.l dp_Port.l dp_Type.l
* dp_Res1.l dp_Res2.l dp_Arg1.l dp_Arg2.l dp_Arg3.l dp_Arg4.l dp_Arg5.l
* dp_Arg6.l dp_Arg7.l.
DP_LINK         equ     0
DP_PORT         equ     4
DP_TYPE         equ     8
DP_RES1         equ     12
DP_RES2         equ     16
DP_ARG1         equ     20
DP_ARG2         equ     24
DP_ARG3         equ     28
DP_ARG4         equ     32
DP_ARG5         equ     36
DP_ARG6         equ     40
DP_ARG7         equ     44

* dos/dosextens.h: struct FileLock -- fl_Link.l(BPTR) fl_Key.l fl_Access.l
* fl_Task.l(MsgPort*) fl_Volume.l(BPTR).
FL_LINK         equ     0
FL_KEY          equ     4
FL_ACCESS       equ     8
FL_TASK         equ     12
FL_VOLUME       equ     16
FL_SIZE         equ     20

* dos/dosextens.h: struct FileHandle -- fh_Link.l fh_Port.l fh_Type.l
* fh_Buf.l fh_Pos.l fh_End.l fh_Funcs.l fh_Func2.l fh_Func3.l fh_Args.l
* (= fh_Arg1) fh_Arg2.l.
FH_ARG1         equ     36

* dos/filehandler.h: struct DeviceNode -- dn_Next.l(BPTR) dn_Type.l
* dn_Task.l(MsgPort*).
DN_TASK         equ     8

* libraries/configvars.h: struct ConfigDev -- struct Node cd_Node(14),
* UBYTE cd_Flags, UBYTE cd_Pad, struct ExpansionRom cd_Rom(16), APTR
* cd_BoardAddr. Same citation hostblk-diagrom.s's own CD_BOARDADDR uses.
CD_BOARDADDR    equ     32

* exec/memory.h.
MEMF_PUBLIC     equ     (1<<0)
MEMF_CLEAR      equ     (1<<16)

* dos/dos.h: DOSTRUE/DOSFALSE -- amigados-rkrm's own elementary-concepts
* chapter: "DOSTRUE is -1, not 1".
DOSTRUE         equ     -1
DOSFALSE        equ     0

* dos/dos.h locking modes (Locks()'s own `mode` argument, and this file's
* own default for a lock this handler manufactures without one -- see
* this file's header, "fl_Access on a freshly minted FileLock").
SHARED_LOCK     equ     -2

* AmigaDOS error codes this file names directly (docs/pktport-protocol.md
* section 6's own numbering; dos/dos.h's ERROR_* constants).
ERR_NO_FREE_STORE      equ     103
ERR_OBJECT_WRONG_TYPE  equ     212
ERR_DEVICE_NOT_MOUNTED equ     218
ERR_ACTION_NOT_KNOWN   equ     209

* ============================================================================
* pktport card: register offsets (docs/pktport-protocol.md section 3) and
* identity (crates/machine-core/src/pktport.rs's own `MANUFACTURER`/
* `PRODUCT`/`PROTOCOL_VERSION` constants).
* ============================================================================
PKT_VERSION     equ     $00     ; R  u32
PKT_CAPACITY    equ     $04     ; R  u32
PKT_REQ_PTR     equ     $08     ; W  u32 -- every byte lane matters (this
                                ; file's header on the -Fhunkexe/-Fbin
                                ; distinction doesn't change hostblk's own
                                ; pointer-register truncation lesson: a
                                ; single move.l writes all four lanes,
                                ; which is deliberately what submit_and_wait
                                ; below does)
PKT_DOORBELL    equ     $0C     ; W  byte, low lane latches
PKT_INT_STATUS  equ     $10     ; RW byte, unused by this v1 polling stub
PKT_INT_ENABLE  equ     $14     ; RW byte, unused by this v1 polling stub
PKT_VOL_COUNT   equ     $18     ; R  u32, unused (VOL_COUNT is always 1
                                ; this version -- protocol doc section 9)

PKT_MANUFACTURER equ    $07DB
PKT_PRODUCT       equ    5
PKT_PROTOCOL_VERSION equ 1

* One 64-byte request descriptor (docs/pktport-protocol.md section 2).
DESC_ACTION     equ     $00
DESC_ARG1       equ     $04
DESC_ARG2       equ     $08
DESC_ARG3       equ     $0C
DESC_ARG4       equ     $10
DESC_ARG5       equ     $14
DESC_ARG6       equ     $18
DESC_ARG7       equ     $1C
DESC_RES1       equ     $20
DESC_RES2       equ     $24
DESC_STATUS     equ     $28
DESC_SIZE       equ     $40

* Name-normalisation scratch buffer size (this file's header): a BSTR's
* length byte can declare up to 255 content bytes, plus the length byte
* itself.
NAMEBUF_SIZE    equ     256

* Actions (docs/pktport-protocol.md section 5's table; identical values
* to crates/machine-hosted/src/pktvol.rs's own `action` module).
ACT_LOCATE_OBJECT      equ     8
ACT_FREE_LOCK          equ     15      ; 15 per NDK dos/dosextens.h -- 9 is
                                        ; ACTION_RENAME_DISK. This file
                                        ; caught the protocol doc saying 9
                                        ; against real dos.library 47.30;
                                        ; the doc and pktvol.rs now both
                                        ; say 15, so wire and dispatch are
                                        ; one constant again.
ACT_COPY_DIR           equ     19
ACT_PARENT             equ     29
ACT_SAME_LOCK          equ     40
ACT_EXAMINE_OBJECT     equ     23
ACT_EXAMINE_NEXT       equ     24
ACT_INFO               equ     26
ACT_DISK_INFO          equ     25
ACT_FINDINPUT          equ     1005
ACT_FINDOUTPUT         equ     1006
ACT_FINDUPDATE         equ     1004
ACT_END                equ     1007
ACT_READ               equ     82      ; 'R'
ACT_WRITE              equ     87      ; 'W'
ACT_SEEK               equ     1008
ACT_SET_FILE_SIZE      equ     1022
ACT_CREATE_DIR         equ     22
ACT_DELETE_OBJECT      equ     16
ACT_RENAME_OBJECT      equ     17
ACT_SET_PROTECT        equ     21
ACT_SET_COMMENT        equ     28
ACT_SET_DATE           equ     34
ACT_IS_FILESYSTEM      equ     1027
ACT_FLUSH              equ     27

*=============================================================================
* Start -- the program's entry point (the very first byte of this
* -Fhunkexe load file's code hunk). No CLI arguments are read: a handler
* process is started by DOS itself, never by RunCommand.
*=============================================================================
Start:
        move.l  4.w,a6                   ; SysBase
        suba.l  a1,a1                    ; FindTask(NULL) = find ourselves
        jsr     _LVOFindTask(a6)
        move.l  d0,a5                    ; a5 = our Process* (== Task*),
                                          ; kept live for the rest of this
                                          ; program's life -- never reused
                                          ; as scratch anywhere below
        move.l  a5,G_PROC

        ; Receive the handler startup packet (amigados-rkrm's handlers-
        ; filesystems chapter, "Starting a Handler"): this is a plain
        ; assembler entry point, not the BCPL "fake startup" wrapper, so
        ; DOS never hands it to us in a register -- WaitPort/GetMsg it
        ; ourselves, exactly as that chapter's own worked example does.
        lea     PR_MSGPORT(a5),a0
        jsr     _LVOWaitPort(a6)
        lea     PR_MSGPORT(a5),a0
        jsr     _LVOGetMsg(a6)
        move.l  d0,a4                    ; a4 = Message*
        move.l  LN_NAME(a4),a2           ; a2 = the startup DosPacket*,
                                          ; kept live for the rest of Start

        ; dp_Arg3 is a BPTR to the DosList/DeviceNode entry that named us
        ; (amigados-rkrm's own "Handler Startup Packet" table) -- convert
        ; the BPTR to a real address once, up front.
        move.l  DP_ARG3(a2),d0
        move.l  d0,G_DOSLIST_BPTR         ; kept as a *BPTR* (unshifted) --
                                          ; see its own comment for why
        lsl.l   #2,d0
        move.l  d0,a3                    ; a3 = DeviceNode*, kept live for
                                          ; the rest of Start

        ; Find the card. Never a hardcoded address (project rule; the
        ; board base comes from cd_BoardAddr) -- open expansion.library
        ; and walk its ConfigDev chain for our manufacturer/product pair.
        lea     ExpName,a1
        moveq   #0,d0
        jsr     _LVOOpenLibrary(a6)
        tst.l   d0
        beq     startup_fail_no_card
        move.l  d0,G_EXPBASE
        move.l  d0,a6                    ; a6 = ExpansionBase for this call

        suba.l  a0,a0                    ; oldConfigDev = NULL: start the
                                          ; search from the beginning of
                                          ; the ConfigDev list
        move.l  #PKT_MANUFACTURER,d0
        move.l  #PKT_PRODUCT,d1
        jsr     _LVOFindConfigDev(a6)
        tst.l   d0
        beq     startup_fail_no_card

        move.l  d0,a0                    ; a0 = ConfigDev*
        move.l  CD_BOARDADDR(a0),d0
        move.l  d0,G_BOARDBASE

        ; Read VERSION; refuse what we don't know (protocol doc section
        ; 3: "Check it, refuse what you don't know.").
        move.l  d0,a0                    ; a0 = board base
        move.l  (PKT_VERSION)(a0),d0
        cmp.l   #PKT_PROTOCOL_VERSION,d0
        bne     startup_fail_bad_version

        ; Read CAPACITY -- don't assume it (protocol doc section 3). This
        ; handler only ever has one request in flight regardless of what
        ; it reads here (this file's header explains why that's still
        ; correct for a CAPACITY of 1); the value is kept only so a future
        ; --inspect-style tool has somewhere to read it from.
        move.l  G_BOARDBASE,a0
        move.l  (PKT_CAPACITY)(a0),d0
        move.l  d0,G_CAPACITY

        ; One 64-byte descriptor, MEMF_PUBLIC|MEMF_CLEAR. AllocMem's own
        ; documented minimum alignment (8 bytes on every stock Amiga
        ; allocator) already satisfies this protocol's 4-byte alignment
        ; requirement -- no extra alignment work needed here.
        move.l  #DESC_SIZE,d0
        move.l  #(MEMF_PUBLIC+MEMF_CLEAR),d1
        move.l  4.w,a6
        jsr     _LVOAllocMem(a6)
        tst.l   d0
        beq     startup_fail_no_mem
        move.l  d0,G_DESC

        ; Two name-normalisation scratch buffers (this file's header,
        ; "ambiguities... name arguments arrive still carrying this
        ; mount's own prefix") -- NAMEBUF_SIZE covers the longest
        ; possible BSTR (255 content bytes plus its length byte).
        ; RENAME_OBJECT needs both at once (source and destination
        ; names), everything else uses only the first.
        move.l  #NAMEBUF_SIZE,d0
        move.l  #(MEMF_PUBLIC+MEMF_CLEAR),d1
        move.l  4.w,a6
        jsr     _LVOAllocMem(a6)
        tst.l   d0
        beq     startup_fail_no_mem
        move.l  d0,G_NAMEBUF1

        move.l  #NAMEBUF_SIZE,d0
        move.l  #(MEMF_PUBLIC+MEMF_CLEAR),d1
        move.l  4.w,a6
        jsr     _LVOAllocMem(a6)
        tst.l   d0
        beq     startup_fail_no_mem
        move.l  d0,G_NAMEBUF2

        ; Success: route every future packet DOS sends for this mount to
        ; our own port (dn_Task, protocol-independent AmigaDOS
        ; convention -- amigados-rkrm's own "file systems... initialize
        ; dol_Task with their process port"), then reply the startup
        ; packet.
        lea     PR_MSGPORT(a5),a0
        move.l  a0,DN_TASK(a3)

        move.l  #DOSTRUE,DP_RES1(a2)
        clr.l   DP_RES2(a2)
        move.l  DP_PORT(a2),a0
        move.l  DP_LINK(a2),a1
        move.l  4.w,a6
        jsr     _LVOPutMsg(a6)

        bra     request_loop

startup_fail_no_card:
        move.l  #ERR_DEVICE_NOT_MOUNTED,d1
        bra     startup_fail_common
startup_fail_bad_version:
        move.l  #ERR_OBJECT_WRONG_TYPE,d1
        bra     startup_fail_common
startup_fail_no_mem:
        move.l  #ERR_NO_FREE_STORE,d1
startup_fail_common:
        ; dn_Task was never set on this path, so DOS never routes another
        ; packet here -- nothing to clean up beyond replying the startup
        ; packet with failure, per amigados-rkrm's own handler-startup
        ; contract ("If startup failed... release all resources acquired
        ; so far and terminate").
        move.l  #DOSFALSE,DP_RES1(a2)
        move.l  d1,DP_RES2(a2)
        move.l  DP_PORT(a2),a0
        move.l  DP_LINK(a2),a1
        move.l  4.w,a6
        jsr     _LVOPutMsg(a6)
        rts

*-----------------------------------------------------------------------------
* request_loop -- the handler's main processing loop (amigados-rkrm's
* handlers-filesystems chapter, "Handler Main Processing Loop"). Never
* exits: this v1 handler implements no ACTION_DIE/shutdown story, the
* same simplification a number of always-resident Amiga handlers make
* (documented here rather than silently, per this project's review
* conventions) -- the process simply lives for the life of the machine,
* exactly like hostblk.device's own dev_expunge ("this device... refuses
* to go away").
*-----------------------------------------------------------------------------
request_loop:
        move.l  4.w,a6
        move.l  G_PROC,a0
        lea     PR_MSGPORT(a0),a0
        jsr     _LVOWaitPort(a6)
rl_drain:
        move.l  4.w,a6
        move.l  G_PROC,a0
        lea     PR_MSGPORT(a0),a0
        jsr     _LVOGetMsg(a6)
        tst.l   d0
        beq     request_loop            ; nothing left queued -- block again
        move.l  d0,a4
        tst.l   LN_NAME(a4)
        bne.s   rl_is_packet
        ; Not a DosPacket -- an ordinary Exec message arrived at this
        ; port. Reply it untouched rather than drop it (this file's own
        ; hard rule): whatever sent it is blocked waiting for a reply.
        move.l  a4,a1
        move.l  4.w,a6
        jsr     _LVOReplyMsg(a6)
        bra.s   rl_drain
rl_is_packet:
        move.l  LN_NAME(a4),a2
        bsr     handle_packet
        move.l  DP_PORT(a2),a0
        move.l  DP_LINK(a2),a1
        move.l  4.w,a6
        jsr     _LVOPutMsg(a6)
        bra.s   rl_drain

*-----------------------------------------------------------------------------
* handle_packet -- dispatch one DosPacket by dp_Type, submit the
* translated request through the card (docs/pktport-protocol.md sections
* 2-5), and fill in dp_Res1/dp_Res2 on the packet at a2. Input: a2 =
* DosPacket*. Every branch below preserves a2 (and every other of
* d2-d7/a2-a6) -- the standard Amiga library-call convention this file's
* own subroutines also honour, so a2 never needs saving/restoring across
* a bsr.
*
* Register convention through this whole dispatcher and its per-action
* blocks: d0/d1/a0/a1 are always scratch (freely clobbered by any bsr or
* jsr); d3/d4 carry a submitted request's wire RES1/RES2 back from
* submit_and_wait; d2/d5/d6/d7/a2-a6 are preserved across every call this
* file makes, including its own subroutines, so a value stashed in one of
* them survives an arbitrary number of bsr/jsr calls without extra
* bookkeeping.
*-----------------------------------------------------------------------------
handle_packet:
        move.l  DP_TYPE(a2),d0
        cmp.l   #ACT_LOCATE_OBJECT,d0
        beq     hp_locate_object
        cmp.l   #ACT_FREE_LOCK,d0               ; 15 -- see its own
                                                 ; equ comment
        beq     hp_free_lock
        cmp.l   #ACT_COPY_DIR,d0
        beq     hp_copy_dir
        cmp.l   #ACT_PARENT,d0
        beq     hp_parent
        cmp.l   #ACT_SAME_LOCK,d0
        beq     hp_same_lock
        cmp.l   #ACT_EXAMINE_OBJECT,d0
        beq     hp_examine_object
        cmp.l   #ACT_EXAMINE_NEXT,d0
        beq     hp_examine_next
        cmp.l   #ACT_INFO,d0
        beq     hp_info
        cmp.l   #ACT_DISK_INFO,d0
        beq     hp_disk_info
        cmp.l   #ACT_FINDINPUT,d0
        beq     hp_findinput
        cmp.l   #ACT_FINDOUTPUT,d0
        beq     hp_findoutput
        cmp.l   #ACT_FINDUPDATE,d0
        beq     hp_findupdate
        cmp.l   #ACT_END,d0
        beq     hp_end
        cmp.l   #ACT_READ,d0
        beq     hp_read
        cmp.l   #ACT_WRITE,d0
        beq     hp_write
        cmp.l   #ACT_SEEK,d0
        beq     hp_seek
        cmp.l   #ACT_SET_FILE_SIZE,d0
        beq     hp_set_file_size
        cmp.l   #ACT_CREATE_DIR,d0
        beq     hp_create_dir
        cmp.l   #ACT_DELETE_OBJECT,d0
        beq     hp_delete_object
        cmp.l   #ACT_RENAME_OBJECT,d0
        beq     hp_rename_object
        cmp.l   #ACT_SET_PROTECT,d0
        beq     hp_set_protect
        cmp.l   #ACT_SET_COMMENT,d0
        beq     hp_set_comment
        cmp.l   #ACT_SET_DATE,d0
        beq     hp_set_date
        cmp.l   #ACT_IS_FILESYSTEM,d0
        beq     hp_is_filesystem
        cmp.l   #ACT_FLUSH,d0
        beq     hp_flush

        ; Not one of docs/pktport-protocol.md section 5's actions: answer
        ; locally, never forward it (the backend would answer 209 anyway,
        ; but a round trip for a known-unknown wastes the one outstanding
        ; request slot this protocol version has).
        move.l  #DOSFALSE,DP_RES1(a2)
        move.l  #ERR_ACTION_NOT_KNOWN,DP_RES2(a2)
        rts

* ---- lock-taking, lock-returning actions (LOCATE_OBJECT/COPY_DIR/
* PARENT/CREATE_DIR): share finish_lock_result for the reply. ------------

hp_locate_object:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_LOCATE_OBJECT,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG1(a0)
        move.l  DP_ARG2(a2),d0
        move.l  G_NAMEBUF1,a1
        bsr     strip_colon_prefix
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG2(a0)                ; name BSTR, prefix-stripped
        move.l  DP_ARG3(a2),DESC_ARG3(a0)      ; mode, verbatim
        bsr     submit_and_wait
        move.l  DP_ARG3(a2),d1                 ; the requested access mode
                                                ; becomes the new lock's
                                                ; fl_Access
        moveq   #1,d2                          ; never a bare BNULL "success"
        bsr     finish_lock_result
        rts

hp_copy_dir:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_COPY_DIR,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG1(a0)
        bsr     submit_and_wait
        move.l  #SHARED_LOCK,d1                ; no mode arg on the wire --
                                                ; this file's own default,
                                                ; see the file header
        moveq   #1,d2                          ; never a bare BNULL "success"
        bsr     finish_lock_result
        rts

hp_parent:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_PARENT,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG1(a0)
        bsr     submit_and_wait
        move.l  #SHARED_LOCK,d1                ; real AmigaDOS's own
                                                ; documented convention for
                                                ; ParentDir()
        moveq   #0,d2                          ; "no parent" (root) really
                                                ; is a documented bare BNULL
                                                ; success here -- see
                                                ; finish_lock_result's header
        bsr     finish_lock_result
        rts

hp_create_dir:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_CREATE_DIR,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG1(a0)
        move.l  DP_ARG2(a2),d0
        move.l  G_NAMEBUF1,a1
        bsr     strip_colon_prefix
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG2(a0)                ; name BSTR, prefix-stripped
        bsr     submit_and_wait
        move.l  #SHARED_LOCK,d1
        moveq   #1,d2                          ; never a bare BNULL "success"
        bsr     finish_lock_result
        rts

* ---- ACTION_FREE_LOCK: unconditional success, per this file's header. --

hp_free_lock:
        move.l  DP_ARG1(a2),d5                 ; d5 = the guest lock BPTR
                                                ; (or 0), kept across the
                                                ; wire round trip below
        move.l  d5,d0
        bsr     xlate_lock
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_FREE_LOCK,DESC_ACTION(a0)
        move.l  d0,DESC_ARG1(a0)
        bsr     submit_and_wait                ; result deliberately ignored
                                                ; -- see this file's header
        tst.l   d5
        beq.s   .no_struct
        move.l  d5,d0
        lsl.l   #2,d0
        move.l  d0,a0
        move.l  #FL_SIZE,d1
        move.l  4.w,a6
        jsr     _LVOFreeMem(a6)
.no_struct:
        move.l  #DOSTRUE,DP_RES1(a2)
        clr.l   DP_RES2(a2)
        rts

* ---- ACTION_SAME_LOCK: both args translated, result verbatim. ----------

hp_same_lock:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_SAME_LOCK,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG1(a0)
        move.l  DP_ARG2(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG2(a0)
        bsr     submit_and_wait
        move.l  d3,DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

* ---- EXAMINE_OBJECT/EXAMINE_NEXT: lock arg translated, FIB BPTR
* verbatim (the backend resolves it itself), result verbatim. -----------

hp_examine_object:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_EXAMINE_OBJECT,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG1(a0)
        move.l  DP_ARG2(a2),DESC_ARG2(a0)
        bsr     submit_and_wait
        move.l  d3,DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

hp_examine_next:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_EXAMINE_NEXT,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG1(a0)
        move.l  DP_ARG2(a2),DESC_ARG2(a0)
        bsr     submit_and_wait
        move.l  d3,DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

* ---- INFO/DISK_INFO: InfoData BPTR verbatim; INFO also carries a lock
* arg (translated for fidelity, though the backend does not consult it --
* protocol doc section 5's own table still lists it). -------------------

hp_info:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_INFO,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG1(a0)
        move.l  DP_ARG2(a2),DESC_ARG2(a0)
        bsr     submit_and_wait
        move.l  d3,DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

hp_disk_info:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_DISK_INFO,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),DESC_ARG1(a0)
        bsr     submit_and_wait
        move.l  d3,DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

* ---- ACTION_FINDINPUT/FINDOUTPUT/FINDUPDATE (protocol doc section 5's
* dagger note): dp_Arg1 is a guest FileHandle BPTR that never crosses the
* wire (wire ARG1 stays 0, cleared by clear_desc_args); on success the
* host handle -- returned in wire RES2, RES1 = DOSTRUE discriminating
* success (this file's header) -- is stored into fh_Arg1. --------

hp_findinput:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_FINDINPUT,DESC_ACTION(a0)
        move.l  DP_ARG2(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG2(a0)
        move.l  DP_ARG3(a2),d0
        move.l  G_NAMEBUF1,a1
        bsr     strip_colon_prefix
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG3(a0)                ; name BSTR, prefix-stripped
        bsr     submit_and_wait
        bra     hp_find_finish

hp_findoutput:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_FINDOUTPUT,DESC_ACTION(a0)
        move.l  DP_ARG2(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG2(a0)
        move.l  DP_ARG3(a2),d0
        move.l  G_NAMEBUF1,a1
        bsr     strip_colon_prefix
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG3(a0)
        bsr     submit_and_wait
        bra     hp_find_finish

hp_findupdate:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_FINDUPDATE,DESC_ACTION(a0)
        move.l  DP_ARG2(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG2(a0)
        move.l  DP_ARG3(a2),d0
        move.l  G_NAMEBUF1,a1
        bsr     strip_colon_prefix
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG3(a0)
        bsr     submit_and_wait
        ; fall through

hp_find_finish:
        tst.l   d3                             ; RES1 is the discriminator
                                                ; (protocol doc section 5,
                                                ; dagger note): DOSTRUE =
                                                ; success, handle in RES2.
                                                ; Never classify RES2 by
                                                ; value -- handles are
                                                ; monotonic and collide
                                                ; with error codes (205).
        beq.s   .find_err
        move.l  DP_ARG1(a2),d0                 ; our own FileHandle BPTR
        lsl.l   #2,d0
        move.l  d0,a0
        move.l  d4,FH_ARG1(a0)
        move.l  #DOSTRUE,DP_RES1(a2)
        clr.l   DP_RES2(a2)
        rts
.find_err:
        clr.l   DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

* ---- ACTION_END/READ/WRITE/SEEK/SET_FILE_SIZE: dp_Arg1 is *already* a
* copy of fh_Arg1 (confirmed against amigados-rkrm's packet-documentation
* chapter, each of these actions' own table: "dp_Arg1 is a copy from
* fh_Arg1 of the FileHandle structure... Note that it is *not* the
* FileHandle itself" -- a real Kickstart 3.2.2 run's own traced ARG1
* values confirmed this the hard way, see this file's header) -- so it
* is forwarded verbatim, with no dereference at all -- an earlier draft
* of this file wrongly treated dp_Arg1 here as a guest FileHandle BPTR
* needing translation via its own fh_Arg1 field (confusing this
* direction with the opposite one, hp_find_finish's own write *into*
* fh_Arg1 below, which really is the FIND* actions' job). Every other
* arg and the whole result pair are verbatim too. ------------------------

hp_end:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_END,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),DESC_ARG1(a0)      ; fh_Arg1, already verbatim
        bsr     submit_and_wait
        move.l  d3,DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

hp_read:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_READ,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),DESC_ARG1(a0)      ; fh_Arg1, already verbatim
        move.l  DP_ARG2(a2),DESC_ARG2(a0)      ; buffer APTR, verbatim
        move.l  DP_ARG3(a2),DESC_ARG3(a0)      ; length, verbatim
        bsr     submit_and_wait
        move.l  d3,DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

hp_write:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_WRITE,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),DESC_ARG1(a0)      ; fh_Arg1, already verbatim
        move.l  DP_ARG2(a2),DESC_ARG2(a0)
        move.l  DP_ARG3(a2),DESC_ARG3(a0)
        bsr     submit_and_wait
        move.l  d3,DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

hp_seek:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_SEEK,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),DESC_ARG1(a0)      ; fh_Arg1, already verbatim
        move.l  DP_ARG2(a2),DESC_ARG2(a0)      ; position, verbatim
        move.l  DP_ARG3(a2),DESC_ARG3(a0)      ; mode, verbatim
        bsr     submit_and_wait
        move.l  d3,DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

hp_set_file_size:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_SET_FILE_SIZE,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),DESC_ARG1(a0)      ; fh_Arg1, already verbatim
        move.l  DP_ARG2(a2),DESC_ARG2(a0)      ; offset, verbatim
        move.l  DP_ARG3(a2),DESC_ARG3(a0)      ; mode, verbatim
        bsr     submit_and_wait
        move.l  d3,DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

* ---- DELETE_OBJECT/RENAME_OBJECT: lock arg(s) translated, names
* verbatim, result verbatim. ---------------------------------------------

hp_delete_object:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_DELETE_OBJECT,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG1(a0)
        move.l  DP_ARG2(a2),d0
        move.l  G_NAMEBUF1,a1
        bsr     strip_colon_prefix
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG2(a0)
        bsr     submit_and_wait
        move.l  d3,DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

hp_rename_object:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_RENAME_OBJECT,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG1(a0)
        move.l  DP_ARG2(a2),d0                 ; source name
        move.l  G_NAMEBUF1,a1
        bsr     strip_colon_prefix
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG2(a0)
        move.l  DP_ARG3(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG3(a0)
        move.l  DP_ARG4(a2),d0                 ; dest name -- NAMEBUF2, not
        move.l  G_NAMEBUF2,a1                  ; NAMEBUF1: both names are
        bsr     strip_colon_prefix             ; live in the same request
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG4(a0)
        bsr     submit_and_wait
        move.l  d3,DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

* ---- SET_PROTECT/SET_COMMENT/SET_DATE: dp_Arg1 unused by the real
* packet (forwarded verbatim regardless -- the backend ignores it), the
* lock in dp_Arg2 translated, everything else verbatim. ------------------

hp_set_protect:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_SET_PROTECT,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),DESC_ARG1(a0)
        move.l  DP_ARG2(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG2(a0)
        move.l  DP_ARG3(a2),d0
        move.l  G_NAMEBUF1,a1
        bsr     strip_colon_prefix
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG3(a0)                ; name BSTR, prefix-stripped
        move.l  DP_ARG4(a2),DESC_ARG4(a0)      ; protection mask, verbatim
        bsr     submit_and_wait
        move.l  d3,DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

hp_set_comment:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_SET_COMMENT,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),DESC_ARG1(a0)
        move.l  DP_ARG2(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG2(a0)
        move.l  DP_ARG3(a2),d0
        move.l  G_NAMEBUF1,a1
        bsr     strip_colon_prefix
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG3(a0)                ; name BSTR, prefix-stripped
        move.l  DP_ARG4(a2),DESC_ARG4(a0)      ; comment BSTR, verbatim
                                                ; (not a path -- never
                                                ; prefix-stripped)
        bsr     submit_and_wait
        move.l  d3,DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

hp_set_date:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_SET_DATE,DESC_ACTION(a0)
        move.l  DP_ARG1(a2),DESC_ARG1(a0)
        move.l  DP_ARG2(a2),d0
        bsr     xlate_lock
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG2(a0)
        move.l  DP_ARG3(a2),d0
        move.l  G_NAMEBUF1,a1
        bsr     strip_colon_prefix
        move.l  G_DESC,a0
        move.l  d0,DESC_ARG3(a0)                ; name BSTR, prefix-stripped
        move.l  DP_ARG4(a2),DESC_ARG4(a0)      ; DateStamp APTR (a real
                                                ; address already, not a
                                                ; BPTR -- protocol doc
                                                ; section 5's own note),
                                                ; verbatim
        bsr     submit_and_wait
        move.l  d3,DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

* ---- IS_FILESYSTEM/FLUSH: no args at all, result verbatim. -------------

hp_is_filesystem:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_IS_FILESYSTEM,DESC_ACTION(a0)
        bsr     submit_and_wait
        move.l  d3,DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

hp_flush:
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_FLUSH,DESC_ACTION(a0)
        bsr     submit_and_wait
        move.l  d3,DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

*=============================================================================
* Shared subroutines. Convention: d0/d1/a0/a1 are scratch (freely
* clobbered); d2/d3/d4/d5/d6/d7/a2-a6 are always preserved unless a
* routine's own header says otherwise (submit_and_wait's d3/d4 are its
* documented *output*, not preserved input).
*=============================================================================

*-----------------------------------------------------------------------------
* clear_desc_args -- zero the descriptor's ACTION/ARG1..ARG7 (32 bytes)
* ahead of building a new request. The RES1/RES2/STATUS/reserved region
* (protocol doc section 2, offsets 0x20-0x40) is never touched here: it
* starts zero from this file's own MEMF_CLEAR allocation and STATUS is
* re-cleared by submit_and_wait itself (protocol doc's own "clear STATUS
* before each submit"), so nothing here needs to touch it.
*-----------------------------------------------------------------------------
clear_desc_args:
        move.l  G_DESC,a0
        moveq   #0,d0
        move.l  d0,DESC_ACTION(a0)
        move.l  d0,DESC_ARG1(a0)
        move.l  d0,DESC_ARG2(a0)
        move.l  d0,DESC_ARG3(a0)
        move.l  d0,DESC_ARG4(a0)
        move.l  d0,DESC_ARG5(a0)
        move.l  d0,DESC_ARG6(a0)
        move.l  d0,DESC_ARG7(a0)
        rts

*-----------------------------------------------------------------------------
* submit_and_wait -- submit the descriptor already built at G_DESC and
* poll for completion (docs/pktport-protocol.md section 2: STATUS is
* written after RES1/RES2, so RES1/RES2 are trustworthy the instant
* STATUS reads 1 -- this file's header explains why a plain busy-poll is
* correct for v1's single-outstanding-request card). Output: d3 = wire
* RES1, d4 = wire RES2. Clobbers d0/a0/a1.
*-----------------------------------------------------------------------------
submit_and_wait:
        move.l  G_DESC,a0
        clr.l   DESC_STATUS(a0)                ; clear STATUS before each
                                                ; submit (protocol doc
                                                ; section 2's own
                                                ; requirement)
        move.l  G_BOARDBASE,a1
        move.l  a0,d0
        move.l  d0,(PKT_REQ_PTR)(a1)           ; one move.l writes every
                                                ; byte lane -- this file's
                                                ; header, "every byte lane
                                                ; matters here"
        move.b  #1,(PKT_DOORBELL+3)(a1)        ; DOORBELL latches on its
                                                ; low-order byte (protocol
                                                ; doc section 3), the same
                                                ; hot-byte-per-slot
                                                ; convention every native
                                                ; card on this bus uses
.poll:
        move.l  G_DESC,a0
        tst.l   DESC_STATUS(a0)
        beq.s   .poll
        move.l  DESC_RES1(a0),d3
        move.l  DESC_RES2(a0),d4
        rts

*-----------------------------------------------------------------------------
* xlate_lock -- translate a packet's lock argument to its wire handle
* (docs/pktport-protocol.md section 4): 0 stays 0 (the null/root lock);
* anything else is a BPTR to one of this handler's own FileLock structs,
* and the wire carries its fl_Key. Input/output: d0. Clobbers a0.
*-----------------------------------------------------------------------------
xlate_lock:
        tst.l   d0
        beq.s   .null
        lsl.l   #2,d0
        move.l  d0,a0
        move.l  FL_KEY(a0),d0
.null:
        rts

*-----------------------------------------------------------------------------
* strip_colon_prefix -- normalise a name BSTR by dropping everything up
* to and including its first ':' (this file's header, "ambiguities...
* name arguments arrive still carrying this mount's own prefix"). Input:
* d0 = BPTR to the source BSTR (or 0), a1 = a NAMEBUF_SIZE-byte scratch
* buffer (G_NAMEBUF1 or G_NAMEBUF2). Output: d0 = the BPTR to forward on
* the wire -- unchanged if the name is 0, empty, or contains no ':', or
* a1's own BPTR (with a freshly written length byte and copied suffix)
* if a ':' was found. Clobbers d1/d2/a0.
*-----------------------------------------------------------------------------
strip_colon_prefix:
        tst.l   d0
        beq.s   .out                     ; NULL BPTR: nothing to normalise
        move.l  d0,d1
        lsl.l   #2,d1
        move.l  d1,a0                     ; a0 = source BSTR
        moveq   #0,d1
        move.b  (a0)+,d1                  ; d1 = length; a0 -> first char
        beq.s   .out                      ; empty name: nothing to strip
        move.l  d1,d2                     ; d2 = bytes left to scan
.scan:
        tst.l   d2
        beq.s   .out                      ; no ':' anywhere: forward the
                                           ; original BPTR unchanged
        cmpi.b  #':',(a0)
        beq.s   .found
        addq.l  #1,a0
        subq.l  #1,d2
        bra.s   .scan
.found:
        ; a0 points AT the ':'; d2 counts from the ':' to the end of the
        ; string inclusive, so the suffix (bytes strictly after it) is
        ; d2-1 bytes long.
        subq.l  #1,d2                     ; d2 = suffix length
        addq.l  #1,a0                     ; a0 -> first suffix byte
        move.b  d2,(a1)                   ; scratch's own length byte
        move.l  a1,-(sp)                  ; save the buffer's base address
        lea     1(a1),a1                  ; a1 -> scratch+1, the copy target
        tst.l   d2
        beq.s   .copied                   ; empty suffix ("PKT0:" alone):
                                           ; nothing left to copy
.copy:
        move.b  (a0)+,(a1)+
        subq.l  #1,d2
        bne.s   .copy
.copied:
        move.l  (sp)+,a1                  ; a1 = buffer base again
        move.l  a1,d0
        lsr.l   #2,d0                     ; -> BPTR
.out:
        rts

*-----------------------------------------------------------------------------
* finish_lock_result -- shared reply logic for LOCATE_OBJECT/COPY_DIR/
* PARENT/CREATE_DIR (docs/pktport-protocol.md section 4: these actions
* return a new lock as a *host handle* in RES1, or 0 for root/at-root,
* with RES2 = 0 on success or an error code on failure). Input: d3 = wire
* RES1, d4 = wire RES2, d1 = fl_Access to give a freshly allocated lock,
* d2 = root policy (1: allocate a real FileLock even when the host
* handle is 0 -- LOCATE_OBJECT/COPY_DIR/CREATE_DIR; 0: return a bare
* BNULL lock when the host handle is 0 -- ACTION_PARENT only, its
* documented "no parent" result), a2 = DosPacket*. Output:
* DP_RES1(a2)/DP_RES2(a2) set. Clobbers d0/d1/a0/a1.
*-----------------------------------------------------------------------------
finish_lock_result:
        tst.l   d4
        bne.s   .fail
        tst.l   d3
        bne.s   .alloc
        ; RES1 == 0 && RES2 == 0: the host resolved this to the volume
        ; root. Whether that means "return a bare BNULL lock" or "still
        ; allocate a real FileLock naming the root" depends on d2 (the
        ; caller's root policy) -- see this file's header, "a bare 0
        ; lock is genuinely ambiguous with failure in real AmigaDOS".
        ; ACTION_PARENT's "no parent" case (d2 = 0) is the one place a
        ; bare 0 is itself the documented, expected success value; every
        ; other caller (d2 = 1) must not hand the guest a lock a real
        ; AmigaDOS client cannot tell apart from failure.
        tst.l   d2
        bne.s   .alloc
        clr.l   DP_RES1(a2)
        clr.l   DP_RES2(a2)
        rts
.alloc:
        move.l  d3,d0                          ; d0 = the new host handle
        bsr     alloc_filelock_or_fail          ; -> d0 = BPTR or 0,
                                                 ; d1 = 0 or an error code
        tst.l   d1
        bne.s   .allocfail
        move.l  d0,DP_RES1(a2)
        clr.l   DP_RES2(a2)
        rts
.allocfail:
        clr.l   DP_RES1(a2)
        move.l  d1,DP_RES2(a2)
        rts
.fail:
        clr.l   DP_RES1(a2)
        move.l  d4,DP_RES2(a2)
        rts

*-----------------------------------------------------------------------------
* alloc_filelock_or_fail -- allocate and populate one guest-side FileLock
* struct naming a host handle. Input: d0 = host handle (nonzero), d1 =
* fl_Access. Output: on success, d0 = BPTR to the new FileLock, d1 = 0.
* On AllocMem failure, d0 = 0, d1 = ERR_NO_FREE_STORE, and the host
* handle is released via a best-effort ACTION_FREE_LOCK first (this file
* told the backend a lock exists; if it can't be represented to the
* guest, it must not silently leak in the backend's own handle table).
* Clobbers d0/d1/a0/a1/a6.
*-----------------------------------------------------------------------------
alloc_filelock_or_fail:
        movem.l d2/d3,-(sp)
        move.l  d0,d2                          ; d2 = handle
        move.l  d1,d3                          ; d3 = requested fl_Access
        move.l  #FL_SIZE,d0
        move.l  #(MEMF_PUBLIC+MEMF_CLEAR),d1
        move.l  4.w,a6
        jsr     _LVOAllocMem(a6)
        tst.l   d0
        bne.s   .ok
        move.l  d2,d0
        bsr     free_lock_wire
        moveq   #0,d0
        move.l  #ERR_NO_FREE_STORE,d1
        movem.l (sp)+,d2/d3
        rts
.ok:
        move.l  d0,a0                          ; a0 = new FileLock*
        move.l  d2,FL_KEY(a0)
        move.l  G_PROC,a1
        lea     PR_MSGPORT(a1),a1
        move.l  a1,FL_TASK(a0)
        move.l  G_DOSLIST_BPTR,FL_VOLUME(a0)   ; this file's own header,
                                                 ; "ambiguities... resolved
                                                 ; on its own": fl_Volume =
                                                 ; 0 (protocol doc section
                                                 ; 4's literal wording) hung
                                                 ; every real client that
                                                 ; walks a directory --
                                                 ; found by tracing a real
                                                 ; Kickstart 3.2.2 run. A
                                                 ; real BPTR to this
                                                 ; mount's own DosList
                                                 ; entry (amigados-rkrm:
                                                 ; "fl_Volume shall be a
                                                 ; BPTR to the DosList
                                                 ; structure representing
                                                 ; the volume") fixes it.
        move.l  d3,FL_ACCESS(a0)
        ; fl_Link is already 0 from MEMF_CLEAR
        move.l  a0,d0
        lsr.l   #2,d0                          ; -> BPTR
        moveq   #0,d1
        movem.l (sp)+,d2/d3
        rts

*-----------------------------------------------------------------------------
* free_lock_wire -- best-effort ACTION_FREE_LOCK submit with no guest-side
* struct involved; used both by ACTION_FREE_LOCK itself and by
* alloc_filelock_or_fail's OOM cleanup path. Input: d0 = host handle
* (nonzero). Result deliberately ignored. Clobbers d0/a0/a1 and (via
* submit_and_wait) sets d3/d4.
*-----------------------------------------------------------------------------
free_lock_wire:
        move.l  d0,d1
        bsr     clear_desc_args
        move.l  G_DESC,a0
        move.l  #ACT_FREE_LOCK,DESC_ACTION(a0)
        move.l  d1,DESC_ARG1(a0)
        bsr     submit_and_wait
        rts

*=============================================================================
* Static data.
*=============================================================================

ExpName:
        dc.b    "expansion.library",0
        even

* This handler's persistent state. A -Fhunkexe BSS-style zero-initialised
* block would serve equally well, but these are few enough and small
* enough that plain dc.l 0 initialisers (living in the DATA hunk) keep
* this file to the one-hunk-of-everything shape vasm produces by default
* with no explicit SECTION directives, matching this project's existing
* single-file assembly sources.
G_PROC:         dc.l    0       ; our struct Process* (== Task*)
G_EXPBASE:      dc.l    0       ; expansion.library base (opened once at
                                ; startup, never closed -- hostblk-diagrom.
                                ; s's own RtInit documents the same choice
                                ; and the same reasoning: it's a core
                                ; system library for the life of the
                                ; machine regardless)
G_BOARDBASE:    dc.l    0       ; the pktport card's AUTOCONFIG base
G_CAPACITY:     dc.l    0       ; CAPACITY as read at startup (diagnostic
                                ; only -- this handler never submits more
                                ; than one request at a time regardless)
G_DESC:         dc.l    0       ; the one 64-byte request descriptor
G_NAMEBUF1:     dc.l    0       ; name-normalisation scratch buffer
                                ; (NAMEBUF_SIZE bytes) -- strip_colon_prefix
G_NAMEBUF2:     dc.l    0       ; second scratch buffer, for
                                ; ACTION_RENAME_OBJECT's two names
G_DOSLIST_BPTR: dc.l    0       ; this mount's own DosList/DeviceNode
                                ; entry, as the BPTR DOS itself handed us
                                ; in the startup packet's dp_Arg3 (not
                                ; shifted to a real address, unlike a3
                                ; during Start -- fl_Volume wants the raw
                                ; BPTR). Used by alloc_filelock_or_fail;
                                ; see its own comment.
