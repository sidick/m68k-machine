*-----------------------------------------------------------------------------
* input-diagrom.s -- the native input card's DiagArea boot ROM and guest
* driver task.
*
* MIT License. Copyright (c) 2026 the m68k Machine project. See ../LICENSE.
*
* Scope, this increment: a struct Resident with deferred rt_Init (the same
* DiagArea/romtag mechanism m68k/hostblk-rom/hostblk-diagrom.s already
* proved, reused unchanged) whose rt_Init does *not* build a library or
* device -- this card is not itself something the guest opens -- but
* instead hand-builds a Task (AddTask, not the amiga.lib CreateTask()
* helper, which is not an exec.library LVO at all: exec.doc's own INPUTS
* list for AddTask is the whole story) and lets that task, running in its
* own context far later than rt_Init, open input.device and
* intuition.library and install the INT2 interrupt server. This is the
* task/interrupt split docs/input-protocol.md's brief calls for and this
* file's header explains why below. NOT implemented this increment: no
* held-state recovery on EVENT_OVERFLOW (docs/input-protocol.md sec 9's
* future-work note), no IND_ADDEVENT (repeat-aware V47 sibling), no
* IESUBCLASS_TABLET/NEWTABLET support (this card only ever produces pixel
* coordinates).
*
* Assembled with vasm (Motorola syntax) to a flat binary; see
* ../../scripts/build-input-rom.sh. Same two addressing regimes as
* hostblk-diagrom.s, and for the same reasons (that file's own header
* explains them in full; only the differences are repeated here):
*
*   - DiagStart..EndCopy is *copied* by expansion.library into RAM chosen
*     at boot time, so anything in it holding its own address (rt_MatchTag/
*     rt_EndSkip/rt_Name/rt_IdString) needs DiagEntry's runtime patch.
*   - Everything after EndCopy runs *in place*, off this board's own
*     AUTOCONFIG window, so it needs no patching and uses ordinary
*     PC-relative addressing freely -- including TaskEntry/IntHandler/
*     every subroutine below, and the state block's embedded Task and
*     Interrupt structures, which is exactly why rt_Init hands AddTask a
*     PC-relative TaskEntry address computed the same "board base +
*     ROM_BASE" way hostblk-diagrom.s's rt_Init already does for its own
*     device vector table.
*
* Why a task at all, not just an interrupt server (the brief's own
* question, checked rather than taken on trust): an interrupt server
* cannot do device I/O -- OpenDevice/DoIO both eventually call Wait(),
* which is illegal (and on real hardware, fatal) from interrupt context,
* per Autodocs/exec.doc's own Wait() warning and RKRM's task-vs-interrupt
* chapter. IND_WRITEEVENT is exactly a DoIO() call. So the interrupt
* server (IntHandler below) can only ever drain the card's own hardware
* queue into ordinary RAM and Signal() a task -- Signal is one of the few
* calls documented safe from interrupt context -- and the task
* (TaskEntry's main loop) is what actually calls DoIO. This mirrors
* hostblk's own split (IntHandler completes IORequests it never blocks
* on), just with the roles reversed: there the interrupt does the
* "unblocking" work and a caller task already exists; here the interrupt
* still can't block, but the task doing the blocking work (DoIO) has to
* be built from scratch, because rt_Init runs far too early for one to
* exist yet -- see the next paragraph.
*
* Why rt_Init creates a task instead of opening input.device/
* intuition.library directly: RTF_COLDSTART's rt_Init runs during
* Kickstart's cold-start Resident scan, long before input.device (which
* is itself typically not far behind, being needed for the keyboard) or
* intuition.library (which needs Workbench-era boot to be well underway)
* are guaranteed to exist. A task created here does not *run* until
* exec's scheduler dispatches it, at some later point of its own
* choosing -- so TaskEntry is where the "wait until what we need exists"
* logic in the brief actually lives, not rt_Init. TaskEntry's own retry
* loops explain the mechanism (WaitTOF against graphics.library, chosen
* because graphics.library is brought up by Kickstart before Intuition or
* DOS and is therefore safe to OpenLibrary unconditionally at any point a
* task can run at all -- documented nowhere in the NDK's Include_H
* (there is no "boot order" header), so flagged here as a project
* assumption rather than a header citation, unlike every numbered
* structure offset in this file).
*
* Register access convention: identical to hostblk-diagrom.s -- every
* register (docs/input-protocol.md sec 5) is read and written with a
* plain `move.l` addressing its base offset directly, even for the
* byte-wide ones, because crate::input::NativeInput's bus routing
* (crates/machine-core/src/input.rs, `in_slot`/`low_byte`) decomposes any
* CPU access into per-byte calls in address order and defines a
* byte register's three high lanes as discarded-on-write/zero-on-read.
*
* Struct layout and register-convention sources, cited at each site below
* (not repeated in every equ block that draws on the same file):
*   - libraries/configregs.h (NDK 3.2): DiagArea, ExpansionRom da_Config
*     bits -- identical citation hostblk-diagrom.s already uses.
*   - exec/resident.h, exec/nodes.h, exec/lists.h, exec/ports.h,
*     exec/io.h, exec/interrupts.h, exec/tasks.h, exec/memory.h,
*     hardware/intbits.h (NDK 3.2): Resident, Node, List, Message,
*     MsgPort, IORequest/IOStdReq, Interrupt, Task, MEMF_*/NT_*
*     constants, INTB_PORTS.
*   - devices/input.h (NDK 3.2): IND_WRITEEVENT = CMD_NONSTD+2, CMD_NONSTD
*     itself from exec/io.h.
*   - devices/inputevent.h (NDK 3.2): struct InputEvent, struct
*     IEPointerPixel, IECLASS_*/IESUBCLASS_*/IECODE_*/IEQUALIFIER_*
*     constants -- the same header docs/input-protocol.md sec 6-7 already
*     cites for this card's design, now actually consumed by code.
*   - intuition/intuitionbase.h, graphics/view.h (NDK 3.2): struct
*     IntuitionBase (ActiveScreen, MouseX/MouseY), struct View (sized to
*     compute IntuitionBase's own field offsets, since IntuitionBase
*     embeds one by value, not by pointer).
*   - intuition/screens.h (NDK 3.2): struct Screen -- only enough of its
*     leading fields to confirm this file never needs more than the bare
*     Screen* IEPointerPixel wants.
*   - devices/timer.h (NDK 3.2): struct timeval, sized (2 ULONGs) to
*     confirm InputEvent's embedded TimeVal_Type is 8 bytes -- needed only
*     to get ie_TimeStamp's *offset* right; its bytes are always zeroed
*     per docs/input-protocol.md sec 7's citation of the input.device
*     autodoc filling it in on IND_WRITEEVENT.
*   - Autodocs/exec.doc (NDK 3.2): InitResident's non-AUTOINIT rt_Init
*     convention (identical to hostblk-diagrom.s's own citation),
*     AddTask/FindTask/AllocSignal/Wait/Signal/OpenDevice/OpenLibrary/
*     DoIO/CloseDevice's register conventions (inline/exec_protos.h's
*     __reg() annotations are the authoritative source actually read, not
*     the autodoc prose).
*   - Autodocs/graphics.doc (NDK 3.2, via Include_I/lvo/graphics_lib.i for
*     the LVO number): WaitTOF, used only as this file's cheap yield-and-
*     retry primitive -- see the header discussion above.
*
* One constant table in this file is NOT backed by any NDK 3.2 header --
* flagged loudly here and again at its own definition
* (modifier_bit_for_code below), rather than presented as if it were:
* the raw keycode assignments for the eight modifier keys (LSHIFT..
* RAMIGA, $60-$67). Include_H ships no rawkeycodes.h, and the RKRM
* Devices "keyboard.device" chapter's own keyboard-matrix figure --
* the one place this project's licensed reference material draws it --
* is unrecoverable OCR garbage per that skill's own provenance note. The
* values used here are the long-standing, widely published Amiga
* hardware raw-keycode assignment for that row (unchanged since the
* A1000 across every third-party keymap and driver this project's
* author has ever seen), but they are a hardware-convention citation,
* not a header citation, and docs/input-protocol.md sec 7 is corrected
* to say so explicitly.
*-----------------------------------------------------------------------------

* ============================================================================
* input register offsets (docs/input-protocol.md section 5;
* crates/machine-core/src/input.rs's `reg` module)
* ============================================================================
INP_EVENT_TYPE          equ     $00     ; R  byte
INP_EVENT_CODE          equ     $04     ; R  byte
INP_EVENT_QUALIFIER     equ     $08     ; R  u32 (low 16 bits meaningful)
INP_EVENT_X             equ     $0c     ; R  u32, sign-extended i16
INP_EVENT_Y             equ     $10     ; R  u32, sign-extended i16
INP_EVENT_ADVANCE       equ     $14     ; W  byte
INP_EVENT_COUNT         equ     $18     ; R  u32
INP_EVENT_OVERFLOW      equ     $1c     ; R  u32
INP_INT_STATUS          equ     $20     ; RW byte
INP_INT_ENABLE          equ     $24     ; RW byte
INP_CAPACITY            equ     $28     ; R  u32 -- read it, never hardcode
INP_VERSION             equ     $2c     ; R  u32

* crate::input's `ev` module -- crates/machine-core/src/input.rs.
EV_NONE                 equ     0
EV_KEY_DOWN             equ     1
EV_KEY_UP               equ     2
EV_POINTER_MOTION       equ     3
EV_BUTTON_DOWN          equ     4
EV_BUTTON_UP            equ     5

* crates/machine-core/src/input.rs: `pub const ROM_BASE: u32 = 0x1000;`
* for this card -- an independent namespace from hostblk's own ROM_BASE,
* since each is an offset within its *own* board's AUTOCONFIG window
* (file header, hostblk-diagrom.s's identical comment).
INP_ROM_BASE            equ     $1000

* ============================================================================
* AmigaOS structure offsets, hand-derived from the NDK 3.2 headers cited
* in the file header -- vasm has no C struct-layout support, so every
* offset here is a citation, not a guess (project convention,
* hostblk-diagrom.s's identical framing).
* ============================================================================

* exec/nodes.h: struct Node (14 bytes); NT_* type constants.
LN_TYPE         equ     8
LN_NAME         equ     10
NODE_SIZE       equ     14

NT_TASK         equ     1
NT_INTERRUPT    equ     2
NT_MSGPORT      equ     4

* exec/resident.h: struct Resident (26 bytes) -- identical layout
* hostblk-diagrom.s already cites from the same header.
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

* exec/ports.h: struct Message (20 bytes) -- struct Node mn_Node(14),
* MsgPort *mn_ReplyPort(4), UWORD mn_Length(2).
MN_REPLYPORT    equ     14
MN_LENGTH       equ     18
MN_SIZE         equ     20

* exec/ports.h: struct MsgPort (34 bytes) -- struct Node mp_Node(14),
* UBYTE mp_Flags, UBYTE mp_SigBit, void *mp_SigTask, struct List
* mp_MsgList(14). PA_SIGNAL is mp_Flags' "signal mp_SigTask" value.
MP_FLAGS        equ     14
MP_SIGBIT       equ     15
MP_SIGTASK      equ     16
MP_MSGLIST      equ     20
MSGPORT_SIZE    equ     34
PA_SIGNAL       equ     0

* exec/lists.h: struct List (14 bytes) -- identical to hostblk-diagrom.s's
* own citation.
LH_HEAD         equ     0
LH_TAIL         equ     4
LH_TAILPRED     equ     8
LIST_SIZE       equ     14

* exec/io.h: struct IORequest (32 bytes)/IOStdReq (48 bytes) -- identical
* layout hostblk-diagrom.s already cites from the same header.
IO_DEVICE       equ     20
IO_COMMAND      equ     28
IO_FLAGS        equ     30
IO_ERROR        equ     31
IO_ACTUAL       equ     32
IO_LENGTH       equ     36
IO_DATA         equ     40
IOSTD_SIZE      equ     48

* exec/io.h: CMD_NONSTD; devices/input.h: IND_WRITEEVENT = CMD_NONSTD+2.
CMD_NONSTD      equ     9
IND_WRITEEVENT  equ     CMD_NONSTD+2

* exec/interrupts.h: struct Interrupt (22 bytes) -- identical layout
* hostblk-diagrom.s already cites from the same header.
IS_DATA         equ     14
IS_CODE         equ     18
INTERRUPT_SIZE  equ     22

* hardware/intbits.h: this board's INT2 is the CPU-level-2 "PORTS" server
* chain, same as hostblk's -- identical citation.
INTB_PORTS      equ     3

* exec/tasks.h: struct Task (92 bytes) -- struct Node tc_Node(14), UBYTE
* tc_Flags, UBYTE tc_State, BYTE tc_IDNestCnt, BYTE tc_TDNestCnt, ULONG
* tc_SigAlloc, ULONG tc_SigWait, ULONG tc_SigRecvd, ULONG tc_SigExcept,
* UWORD tc_TrapAlloc, UWORD tc_TrapAble, APTR tc_ExceptData, APTR
* tc_ExceptCode, APTR tc_TrapData, APTR tc_TrapCode, APTR tc_SPReg, APTR
* tc_SPLower, APTR tc_SPUpper, VOID(*tc_Switch)(), VOID(*tc_Launch)(),
* struct List tc_MemEntry(14), APTR tc_UserData -- offsets by hand-summing
* the header's field list in order.
TC_SIGALLOC     equ     18
TC_SPREG        equ     54
TC_SPLOWER      equ     58
TC_SPUPPER      equ     62
TC_MEMENTRY     equ     74
TC_USERDATA     equ     88
TASK_SIZE       equ     92

* devices/inputevent.h: struct InputEvent (22 bytes) -- struct InputEvent
* *ie_NextEvent(4), UBYTE ie_Class, UBYTE ie_SubClass, UWORD ie_Code,
* UWORD ie_Qualifier, union{struct{WORD ie_x,ie_y;} | APTR ie_addr |
* ...}(4), TimeVal_Type ie_TimeStamp. TimeVal_Type is `struct timeval`
* (devices/timer.h): two ULONGs (tv_secs/tv_micro), 8 bytes -- sized only
* to get ie_TimeStamp's offset right; every byte of it is always zeroed
* here (file header; docs/input-protocol.md sec 7).
IE_NEXTEVENT    equ     0
IE_CLASS        equ     4
IE_SUBCLASS     equ     5
IE_CODE         equ     6
IE_QUALIFIER    equ     8
IE_POSITION     equ     10
IE_TIMESTAMP    equ     14
INPUTEVENT_SIZE equ     22

* devices/inputevent.h: struct IEPointerPixel -- struct Screen
* *iepp_Screen(4), struct{WORD X,Y;}iepp_Position(4).
IEPP_SCREEN     equ     0
IEPP_X          equ     4
IEPP_Y          equ     6
IEPOINTERPIXEL_SIZE equ 8

* devices/inputevent.h: ie_Class values this driver emits.
IECLASS_RAWKEY          equ     $01
IECLASS_RAWMOUSE        equ     $02
IECLASS_NEWPOINTERPOS   equ     $13

* devices/inputevent.h: ie_SubClass for IECLASS_NEWPOINTERPOS.
IESUBCLASS_PIXEL        equ     $01

* devices/inputevent.h: ie_Code values/bits.
IECODE_UP_PREFIX        equ     $80
IECODE_LBUTTON          equ     $68
IECODE_RBUTTON          equ     $69
IECODE_MBUTTON          equ     $6a
IECODE_NOBUTTON         equ     $ff

* devices/inputevent.h: ie_Qualifier bits this driver sets.
IEQUALIFIER_LSHIFT      equ     $0001
IEQUALIFIER_RSHIFT      equ     $0002
IEQUALIFIER_CAPSLOCK    equ     $0004
IEQUALIFIER_CONTROL     equ     $0008
IEQUALIFIER_LALT        equ     $0010
IEQUALIFIER_RALT        equ     $0020
IEQUALIFIER_LCOMMAND    equ     $0040
IEQUALIFIER_RCOMMAND    equ     $0080
IEQUALIFIER_MIDBUTTON   equ     $1000
IEQUALIFIER_RBUTTON     equ     $2000
IEQUALIFIER_LEFTBUTTON  equ     $4000
IEQUALIFIER_RELATIVEMOUSE equ   $8000

* intuition/intuitionbase.h, graphics/view.h: struct IntuitionBase --
* struct Library LibNode(34), struct View ViewLord(18: ViewPort*(4) +
* LOFCprList*(4) + SHFCprList*(4) + DyOffset(2) + DxOffset(2) +
* Modes(2)), Window *ActiveWindow(4), Screen *ActiveScreen(4), Screen
* *FirstScreen(4), ULONG Flags(4), WORD MouseY(2), WORD MouseX(2), ULONG
* Seconds(4), ULONG Micros(4). Offsets by hand-summing in field order;
* only ActiveScreen is used here (MouseX/MouseY are read from the host
* side, `machine-hosted`'s --inspect, not from this driver).
IB_ACTIVESCREEN equ     56

* exec/memory.h.
MEMF_PUBLIC     equ     (1<<0)
MEMF_CLEAR      equ     (1<<16)

* libraries/configregs.h: struct DiagArea's da_Config bit layout --
* identical citation hostblk-diagrom.s already uses.
DAC_WORDWIDE    equ     $80
DAC_CONFIGTIME  equ     $10

* ============================================================================
* exec.library / graphics.library LVOs (Include_I/lvo/exec_lib.i,
* Include_I/lvo/graphics_lib.i) -- transcribed by hand, same reason the
* struct offsets above are.
* ============================================================================
_LVODisable     equ     -120
_LVOEnable      equ     -126
_LVOAddIntServer equ    -168
_LVOAllocMem    equ     -198
_LVOFreeMem     equ     -210
_LVOAddTask     equ     -282
_LVOFindTask    equ     -294
_LVOWait        equ     -318
_LVOSignal      equ     -324
_LVOAllocSignal equ     -330
_LVOOpenDevice  equ     -444
_LVOCloseDevice equ     -450
_LVODoIO        equ     -456
_LVOOpenLibrary equ     -552
_LVOFindResident equ    -96
_LVOInitResident equ    -102

_LVOWaitTOF     equ     -270    ; graphics.library

* This driver's own private state block, allocated once from rt_Init and
* owned by the task for the life of the machine (never freed -- same
* "ROM-resident for the life of the machine, no unload story" posture
* hostblk-diagrom.s's dev_expunge documents). Offsets past this point are
* this file's own layout, not an OS structure, so no citation beyond
* "chosen here" (hostblk-diagrom.s's DEV_* block makes the same
* disclaimer).
ST_SYSBASE          equ     0       ; APTR
ST_BOARDBASE        equ     4       ; APTR
ST_GFXBASE          equ     8       ; APTR (graphics.library, retry-yield only)
ST_INPUTBASE        equ     12      ; APTR (input.device base, post-OpenDevice)
ST_INTUITIONBASE    equ     16      ; APTR (intuition.library base)
ST_SIG_EVENT_MASK   equ     20      ; ULONG, the ISR-wakes-task signal mask
ST_REPLY_SIGBIT     equ     24      ; UBYTE, the IORequest reply port's signal bit
ST_HELD_QUALIFIER   equ     26      ; UWORD, running held-modifier/button state
                                     ; (docs/input-protocol.md sec 7 --
                                     ; this driver's own bookkeeping, the
                                     ; card's EVENT_QUALIFIER is never
                                     ; trusted for this)
ST_RING_HEAD        equ     28      ; ULONG, consumer index (task-owned)
ST_RING_TAIL        equ     32      ; ULONG, producer index (IntHandler-owned)
ST_RING_COUNT       equ     36      ; ULONG, entries queued (Disable/Enable-protected)
ST_RING_BUF         equ     40      ; RING_CAPACITY*RE_SIZE bytes, the
                                     ; software queue IntHandler drains the
                                     ; card's hardware queue into
RING_CAPACITY       equ     16      ; matches input::QUEUE_CAPACITY -- one
                                     ; interrupt can drain at most this many
                                     ; hardware entries in one pass
RE_TYPE             equ     0       ; UBYTE
RE_CODE             equ     1       ; UBYTE
RE_QUAL             equ     2       ; UWORD (read but never trusted, see above)
RE_X                equ     4       ; UWORD (WORD, sign matters -- see reads below)
RE_Y                equ     6       ; UWORD
RE_SIZE             equ     8
ST_TASK             equ     ST_RING_BUF+(RING_CAPACITY*RE_SIZE)     ; 168
ST_INT              equ     ST_TASK+TASK_SIZE                       ; 260
ST_PORT             equ     ST_INT+INTERRUPT_SIZE                   ; 282
ST_IOREQ            equ     ST_PORT+MSGPORT_SIZE                    ; 316
ST_EVENTBUF         equ     ST_IOREQ+IOSTD_SIZE                     ; 364
ST_PIXBUF           equ     ST_EVENTBUF+INPUTEVENT_SIZE             ; 386
ST_STACKPTR         equ     ST_PIXBUF+IEPOINTERPIXEL_SIZE+2         ; 396 (+2 pad to keep this APTR longword-aligned)
ST_SIZE             equ     ST_STACKPTR+4                           ; 400

STACK_SIZE          equ     4096    ; generous headroom for a small
                                     ; polling task that only ever calls
                                     ; OpenDevice/OpenLibrary/DoIO/Signal/
                                     ; Wait -- Commodore's own minimum
                                     ; guidance (RKRM) is far smaller

*-----------------------------------------------------------------------------
* struct DiagArea (libraries/configregs.h): identical 14-byte layout and
* DiagMarker convention hostblk-diagrom.s already established --
* da_DiagPoint/da_Name are word offsets from DiagStart.
*
* da_BootPoint is NOT zero, even though this card is never a BootNode (it
* carries no bootable media, and AddBootNode is never called for it) --
* hostblk-diagrom.s's own DiagArea test
* (`diag_rom_header_matches_the_documented_diagarea_layout`) recorded the
* empirical fact behind this the hard way: RKRM's "Events At DIAG Time"
* documents that a *zero* da_BootPoint means expansion.library never
* (see docs/input-protocol.md section 14: this and DAC_NEVER are the same
* rule -- the DiagArea is copied only when there is code to run and a time
* to run it, so a board wanting its ROM to run at all needs both, whether
* or not it has anything to do with booting)
* copies the DiagArea into RAM **at all** -- it is not merely "no boot
* routine", it silently cancels DiagEntry too, since DiagEntry only ever
* runs against the copy. So `BootStub` below exists purely to give
* da_BootPoint a non-zero, in-range value; strap can never actually reach
* it (it only calls da_BootPoint for a node on its own BootNode list, and
* this board is never added to one), but the DiagArea copy -- and
* therefore DiagEntry, and therefore this card's whole romtag -- depends
* on the field being non-zero regardless.
*-----------------------------------------------------------------------------
DiagStart:
        dc.b    DAC_WORDWIDE+DAC_CONFIGTIME  ; da_Config -- DAC_CONFIGTIME
                                              ; is load-bearing here for a
                                              ; reason with nothing to do
                                              ; with booting: it also gates
                                              ; whether Kickstart's cold-
                                              ; start scan looks for a
                                              ; romtag in this DiagArea copy
                                              ; at all (hostblk-diagrom.s's
                                              ; own header; confirmed there
                                              ; against real Kickstart 3.2.2)
        dc.b    0                       ; da_Flags -- none defined, must be 0
        dc.w    EndCopy-DiagStart       ; da_Size: bytes copied into RAM
        dc.w    DiagEntry-DiagStart     ; da_DiagPoint
        dc.w    BootStub-DiagStart      ; da_BootPoint -- non-zero only to
                                         ; keep the RAM copy happening; see
                                         ; above -- never actually reached
        dc.w    0                       ; da_Name: no identifier string
        dc.w    0                       ; da_Reserved01 -- must be zero
        dc.w    0                       ; da_Reserved02 -- must be zero

* Diagnostic scratch cell: DiagEntry writes VERSION here, so a host-side
* inspector can confirm DiagEntry ran -- identical mechanism and offset
* (14, right after the DiagArea header) to hostblk-diagrom.s's own
* DiagMarker, asserted by crates/machine-core/src/input.rs's own tests
* against `input::DIAG_MARKER_OFFSET`.
DiagMarker:
        dc.l    0

* Second scratch cell, immediately after DiagMarker (offset 18), written
* only by BootStub. It exists to answer a question this project had left
* open: libraries/configregs.h describes DAC_CONFIGTIME as "call
* da_BootPoint when first configing the device", which reads as
* unconditional, but what was actually observed in Kickstart 3.2.2's
* strap is narrower -- the call is made for a non-floppy BootNode as that
* node's whole boot attempt. This card offers no BootNode, so the two
* readings disagree about whether BootStub ever runs, and nothing
* depended on the answer. A marker costs four bytes and settles it.
BootMarker:
        dc.l    0

* da_BootPoint's target. Required non-zero purely so the DiagArea is
* copied at all (docs/input-protocol.md section 14: DAC_NEVER removes the
* time to run code, a zero da_BootPoint removes the code, and either way
* nothing is copied). Inside the copied region since da_BootPoint, like
* da_DiagPoint, is an offset into the RAM copy rather than into this ROM.
*
* Writes BootMarker so "is this reached?" is answerable rather than
* assumed -- see BootMarker above. A2 is not guaranteed here the way it is
* for DiagEntry, so the RAM copy's base is recovered from this routine's
* own address with a PC-relative LEA instead.
BootStub:
        lea     BootStub(pc),a1
        move.l  #$B007B007,(BootMarker-BootStub)(a1)
        moveq   #1,d0
        rts

*-----------------------------------------------------------------------------
* DiagEntry -- da_DiagPoint. Calling convention (libraries/configregs.h;
* hostblk-diagrom.s's header quotes it in full): A0=board base, A2=RAM
* copy base. Returns D0 non-zero to keep the RAM copy (the VERSION marker
* and the Resident struct both need to live in it permanently). Same
* patch pattern as hostblk-diagrom.s's DiagEntry -- see this file's own
* header for why rt_Init's field is patched with +a0 (board base +
* INP_ROM_BASE) while the four RAM-copy-relative fields are patched +a2.
*-----------------------------------------------------------------------------
DiagEntry:
        move.l  INP_VERSION(a0),d0     ; read this card's own VERSION register
        move.l  d0,(DiagMarker-DiagStart)(a2)  ; proof DiagEntry actually ran
        move.l  a2,d0
        add.l   d0,(RtMatchTag-DiagStart)(a2)
        add.l   d0,(RtEndSkip-DiagStart)(a2)
        add.l   d0,(RtName-DiagStart)(a2)
        add.l   d0,(RtIdString-DiagStart)(a2)
        move.l  a0,d0
        add.l   #INP_ROM_BASE,d0
        add.l   d0,(RtInitField-DiagStart)(a2)  ; rt_Init lives in the
                                                  ; always-mapped window at
                                                  ; boardbase+ROM_BASE
        moveq   #1,d0
        rts

*-----------------------------------------------------------------------------
* struct Resident ("Romtag"; exec/resident.h). rt_Type is NT_TASK -- this
* romtag's entire job is to stand up a Task, unlike hostblk's NT_DEVICE
* (rt_Type has no effect on non-AUTOINIT InitResident's behaviour either
* way, Autodocs/exec.doc -- it is documentation, chosen here to describe
* what this Resident actually produces). rt_Flags is RTF_COLDSTART only
* (non-AUTOINIT): rt_Init is ordinary code called per InitResident's
* non-AUTOINIT convention, not the four-longword MakeLibrary-table shape
* AUTOINIT expects (identical citation to hostblk-diagrom.s).
*-----------------------------------------------------------------------------
Romtag:
        dc.w    RTC_MATCHWORD                   ; rt_MatchWord
RtMatchTag:
        dc.l    Romtag-DiagStart                ; rt_MatchTag (patched: +a2)
RtEndSkip:
        dc.l    EndCopy-DiagStart                ; rt_EndSkip (patched: +a2)
        dc.b    RTF_COLDSTART                    ; rt_Flags
        dc.b    0                                ; rt_Version
        dc.b    NT_TASK                          ; rt_Type
        dc.b    20                               ; rt_Pri
RtName:
        dc.l    TaskName-DiagStart               ; rt_Name (patched: +a2)
RtIdString:
        dc.l    IdString-DiagStart               ; rt_IdString (patched: +a2)
RtInitField:
        dc.l    RtInit-DiagStart                  ; rt_Init (patched: +a0)

TaskName:
        dc.b    "input.card",0
        even
IdString:
        dc.b    "input.card driver 1.0 (2026)",0
        even

EndCopy:

*=============================================================================
* Everything below here executes in place, straight off this board's own
* AUTOCONFIG window -- see the file header for why that means ordinary
* PC-relative addressing (no patching) but rt_Init's own romtag field was
* still patched with (board base + INP_ROM_BASE), not with a2.
*=============================================================================

*-----------------------------------------------------------------------------
* RtInit -- rt_Init, called by InitResident's non-AUTOINIT path
* (Autodocs/exec.doc): D0=0, A0=segList (NULL for a ROM module), A6=
* ExecBase. Allocates this driver's private state block and stack, hand-
* builds a Task around them (AddTask, not amiga.lib's CreateTask() --
* file header explains why that helper isn't available here), and
* returns. Everything that needs input.device/intuition.library to exist
* happens in TaskEntry, once exec actually schedules the new task --
* never here (file header).
*-----------------------------------------------------------------------------
RtInit:
        movem.l d2-d7/a2-a6,-(sp)

        lea     DiagStart(pc),a5
        suba.l  #INP_ROM_BASE,a5        ; a5 = board base (file header)

        move.l  INP_VERSION(a5),d0      ; refuse a protocol version we
        cmp.l   #1,d0                   ; don't know (docs/input-protocol.md
        bne     .fail                   ; section 5)

        move.l  #ST_SIZE,d0
        move.l  #MEMF_PUBLIC+MEMF_CLEAR,d1
        jsr     _LVOAllocMem(a6)
        tst.l   d0
        beq     .fail
        move.l  d0,a3                   ; a3 = state block, kept for the
                                          ; rest of this routine

        move.l  a6,ST_SYSBASE(a3)
        move.l  a5,ST_BOARDBASE(a3)

        move.l  #STACK_SIZE,d0
        move.l  #MEMF_PUBLIC+MEMF_CLEAR,d1
        jsr     _LVOAllocMem(a6)
        tst.l   d0
        beq     .fail_freestate
        move.l  d0,ST_STACKPTR(a3)

        lea     ST_TASK(a3),a4          ; a4 = embedded Task struct
        move.b  #NT_TASK,LN_TYPE(a4)
        moveq   #0,d0
        move.b  d0,LN_TYPE+1(a4)        ; ln_Pri = 0 -- ordinary priority;
                                          ; nothing about this task needs to
                                          ; preempt real-time work, only to
                                          ; be scheduled promptly, which
                                          ; Signal()+Wait() already gives it
        lea     TaskName(pc),a0
        move.l  a0,LN_NAME(a4)

        move.l  #$0000ffff,TC_SIGALLOC(a4)  ; reserve signal bits 0-15 as
                                              ; already-allocated, the same
                                              ; convention amiga.lib's own
                                              ; CreateTask() uses -- so
                                              ; AllocSignal (called from
                                              ; TaskEntry, since it always
                                              ; operates on the *calling*
                                              ; task) can't hand out a bit
                                              ; that collides with a
                                              ; predefined system signal

        move.l  ST_STACKPTR(a3),d0
        move.l  d0,TC_SPLOWER(a4)
        add.l   #STACK_SIZE,d0
        move.l  d0,TC_SPUPPER(a4)
        move.l  d0,TC_SPREG(a4)          ; stack grows down from the top
        move.l  a3,TC_USERDATA(a4)       ; TaskEntry's only way back to the
                                          ; state block -- there is no other
                                          ; argument-passing convention for
                                          ; a freshly AddTask'd task's
                                          ; initial PC

        ; tc_MemEntry empty-list init: exec/lists.h's own "two ghost nodes"
        ; trick, identical to hostblk-diagrom.s's DEV_PENDING init.
        lea     TC_MEMENTRY(a4),a0
        move.l  a0,d0
        addq.l  #4,d0
        move.l  d0,LH_HEAD(a0)
        clr.l   LH_TAIL(a0)
        move.l  a0,LH_TAILPRED(a0)
        clr.b   LH_TAILPRED+4(a0)

        move.l  a4,a1                    ; A1=task
        lea     TaskEntry(pc),a2         ; A2=initialPC (in-place code --
                                          ; ordinary pc-relative, file header)
        suba.l  a3,a3                    ; A3=finalPC=NULL (a3 no longer
                                          ; needed once this call is made)
        jsr     _LVOAddTask(a6)

        moveq   #1,d0
        movem.l (sp)+,d2-d7/a2-a6
        rts

.fail_freestate:
        move.l  #ST_SIZE,d0
        jsr     _LVOFreeMem(a6)
.fail:
        moveq   #0,d0
        movem.l (sp)+,d2-d7/a2-a6
        rts

*-----------------------------------------------------------------------------
* TaskEntry -- the new task's initial PC (AddTask's A2, above). Runs with
* its own stack and no inherited registers at all except what AddTask's
* contract guarantees (none) -- the *only* way it recovers anything from
* rt_Init is via FindTask(NULL)->tc_UserData, set above.
*
* First recovers SysBase from absolute address 4 (ABSEXECBASE) -- the
* universal Amiga convention that SysBase always lives there regardless
* of what called this code (Autodocs' own framing of `4.w`; every Amiga
* task, not just an AUTOCONFIG driver's, relies on this to bootstrap
* itself), since A6 carries nothing meaningful on task entry the way it
* does across a library call.
*-----------------------------------------------------------------------------
TaskEntry:
        move.l  4.w,a6                   ; a6 = SysBase (ABSEXECBASE)
        suba.l  a1,a1                    ; FindTask(NULL) -> this task
        jsr     _LVOFindTask(a6)
        move.l  d0,a2                    ; a2 = this Task (kept: Signal's
                                          ; own A1 argument, and MsgPort's
                                          ; mp_SigTask, both want it)
        move.l  TC_USERDATA(a2),a3       ; a3 = state block -- live for the
                                          ; rest of this task's life
        move.l  a6,ST_SYSBASE(a3)        ; re-assert (already set by
                                          ; rt_Init; harmless, and removes
                                          ; any doubt this task's own
                                          ; SysBase agrees with rt_Init's)

        moveq   #-1,d0
        jsr     _LVOAllocSignal(a6)
        tst.b   d0
        bmi     .halt                    ; no free signal bit at all --
                                          ; should never happen this early
                                          ; with 16 free (tc_SigAlloc,
                                          ; above); nothing safe to do
        moveq   #1,d1
        and.l   #$ff,d0
        lsl.l   d0,d1
        move.l  d1,ST_SIG_EVENT_MASK(a3)

        moveq   #-1,d0
        jsr     _LVOAllocSignal(a6)
        tst.b   d0
        bmi     .halt
        and.l   #$ff,d0
        move.b  d0,ST_REPLY_SIGBIT(a3)

        ; graphics.library: guaranteed available this early (file header)
        ; -- used only for WaitTOF's cheap yield-and-retry below, never
        ; for anything Intuition-shaped.
        lea     GfxName(pc),a1
        moveq   #0,d0
        jsr     _LVOOpenLibrary(a6)
        move.l  d0,ST_GFXBASE(a3)
        beq     .halt                    ; if even this fails, this task
                                          ; has no safe way to retry at all

        ; embedded Interrupt for IntHandler (INT2/PORTS server).
        lea     ST_INT(a3),a0
        move.b  #NT_INTERRUPT,LN_TYPE(a0)
        clr.b   LN_TYPE+1(a0)
        lea     TaskName(pc),a1
        move.l  a1,LN_NAME(a0)
        move.l  a3,IS_DATA(a0)           ; is_Data = state block --
                                          ; IntHandler re-derives everything
                                          ; else from it
        lea     IntHandler(pc),a1
        move.l  a1,IS_CODE(a0)

        ; MsgPort for input.device's replies (IND_WRITEEVENT is DoIO'd
        ; synchronously below, one at a time, so one persistent port and
        ; one persistent IOStdReq suffice -- no per-call allocation).
        lea     ST_PORT(a3),a0
        move.b  #NT_MSGPORT,LN_TYPE(a0)
        clr.b   LN_TYPE+1(a0)
        move.b  #PA_SIGNAL,MP_FLAGS(a0)
        move.b  ST_REPLY_SIGBIT(a3),MP_SIGBIT(a0)
        move.l  a2,MP_SIGTASK(a0)
        lea     MP_MSGLIST(a0),a1
        move.l  a1,d0
        addq.l  #4,d0
        move.l  d0,LH_HEAD(a1)
        clr.l   LH_TAIL(a1)
        move.l  a1,LH_TAILPRED(a1)
        clr.b   LH_TAILPRED+4(a1)

        lea     ST_IOREQ(a3),a4          ; a4 = the one persistent IOStdReq
        lea     ST_PORT(a3),a0
        move.l  a0,MN_REPLYPORT(a4)
        move.w  #IOSTD_SIZE,MN_LENGTH(a4)

.open_input:
        move.l  ST_SYSBASE(a3),a6
        lea     InputDevName(pc),a0
        moveq   #0,d0
        move.l  a4,a1
        moveq   #0,d1
        jsr     _LVOOpenDevice(a6)
        tst.b   d0
        beq     .input_open
        move.l  ST_GFXBASE(a3),a6
        jsr     _LVOWaitTOF(a6)
        bra     .open_input
.input_open:
        move.l  IO_DEVICE(a4),ST_INPUTBASE(a3)

.open_intuition:
        move.l  ST_SYSBASE(a3),a6
        lea     IntuitionName(pc),a1
        moveq   #0,d0
        jsr     _LVOOpenLibrary(a6)
        move.l  d0,ST_INTUITIONBASE(a3)
        bne     .intuition_open
        move.l  ST_GFXBASE(a3),a6
        jsr     _LVOWaitTOF(a6)
        bra     .open_intuition
.intuition_open:

        move.l  ST_SYSBASE(a3),a6
        move.l  #INTB_PORTS,d0
        lea     ST_INT(a3),a1
        jsr     _LVOAddIntServer(a6)

        move.l  ST_BOARDBASE(a3),a5
        moveq   #1,d0
        move.l  d0,(INP_INT_ENABLE)(a5)  ; only now -- both libraries open
                                          ; and the server installed -- does
                                          ; this card's INT2 start arriving

.main_loop:
        move.l  ST_SYSBASE(a3),a6
        move.l  ST_SIG_EVENT_MASK(a3),d0
        jsr     _LVOWait(a6)
        bsr     drain_and_dispatch
        bra     .main_loop

.halt:
        move.l  4.w,a6
        moveq   #0,d0
        jsr     _LVOWait(a6)             ; blocks forever -- documented
        bra     .halt                    ; degenerate path (file header's
                                          ; ".halt" comments above)

*-----------------------------------------------------------------------------
* IntHandler -- INT2 (INTB_PORTS) server. Called with A1 = is_Data = state
* block (set in TaskEntry above). Standard exec interrupt-server contract:
* preserve every register except D0/D1/A0/A1 (identical citation to
* hostblk-diagrom.s's own IntHandler). Drains the card's hardware queue
* into ST_RING_BUF (the software queue) and Signal()s the task -- this
* server does no device I/O itself (file header's "why a task at all"
* section explains why it structurally cannot).
*-----------------------------------------------------------------------------
IntHandler:
        movem.l d2-d7/a2-a6,-(sp)
        move.l  a1,a3
        move.l  ST_SYSBASE(a3),a6
        move.l  ST_BOARDBASE(a3),a5

.drain:
        move.l  ST_RING_COUNT(a3),d0
        cmp.l   #RING_CAPACITY,d0
        bge     .done                    ; software ring full -- stop; the
                                          ; card's own queue stays latched
                                          ; (INT_STATUS ack below is gated
                                          ; on it being empty, so this INT2
                                          ; will simply fire again once the
                                          ; task has drained some room)

        move.l  (INP_EVENT_TYPE)(a5),d1
        beq     .done                    ; EV_NONE -- card's queue is empty

        move.l  (INP_EVENT_CODE)(a5),d2
        move.l  (INP_EVENT_X)(a5),d3
        move.l  (INP_EVENT_Y)(a5),d4

        move.l  ST_RING_TAIL(a3),d5
        lea     ST_RING_BUF(a3),a0
        mulu.w  #RE_SIZE,d5
        adda.l  d5,a0
        move.b  d1,RE_TYPE(a0)
        move.b  d2,RE_CODE(a0)
        move.w  d3,RE_X(a0)
        move.w  d4,RE_Y(a0)

        addq.l  #1,ST_RING_TAIL(a3)
        move.l  ST_RING_TAIL(a3),d5
        cmp.l   #RING_CAPACITY,d5
        blt.s   .nowrap
        clr.l   ST_RING_TAIL(a3)
.nowrap:
        addq.l  #1,ST_RING_COUNT(a3)

        moveq   #0,d5
        move.l  d5,(INP_EVENT_ADVANCE)(a5)  ; pop the head event, reveal next
        bra     .drain

.done:
        moveq   #1,d5
        move.l  d5,(INP_INT_STATUS)(a5)  ; write-1-to-clear, silently
                                          ; ignored unless the card's queue
                                          ; is actually empty
                                          ; (docs/input-protocol.md sec 8)

        tst.l   ST_RING_COUNT(a3)
        beq.s   .nowake
        lea     ST_TASK(a3),a1
        move.l  ST_SIG_EVENT_MASK(a3),d0
        jsr     _LVOSignal(a6)           ; Signal() is safe from interrupt
                                          ; context (Autodocs/exec.doc) --
                                          ; unlike DoIO, which is why this
                                          ; server can do this much and no
                                          ; more (file header)
.nowake:
        movem.l (sp)+,d2-d7/a2-a6
        rts

*-----------------------------------------------------------------------------
* drain_and_dispatch -- bsr'd from TaskEntry's main loop after every wake.
* a3=state, a6=SysBase (both already established by the caller). Pops one
* software-ring entry at a time under Disable()/Enable() (RKRM's standard
* task-vs-interrupt shared-state guidance -- Forbid()/Permit() would not
* suffice, since IntHandler runs at interrupt time regardless of Forbid;
* identical reasoning to hostblk-diagrom.s's submit_or_queue), copies it
* into scratch registers, re-enables, and only then calls dispatch_one --
* so a slow DoIO() never runs with interrupts disabled.
*-----------------------------------------------------------------------------
drain_and_dispatch:
.next:
        jsr     _LVODisable(a6)
        tst.l   ST_RING_COUNT(a3)
        beq     .empty

        move.l  ST_RING_HEAD(a3),d0
        lea     ST_RING_BUF(a3),a0
        mulu.w  #RE_SIZE,d0
        adda.l  d0,a0
        moveq   #0,d2
        move.b  RE_TYPE(a0),d2
        moveq   #0,d3
        move.b  RE_CODE(a0),d3
        move.w  RE_X(a0),d5
        move.w  RE_Y(a0),d6

        addq.l  #1,ST_RING_HEAD(a3)
        move.l  ST_RING_HEAD(a3),d0
        cmp.l   #RING_CAPACITY,d0
        blt.s   .nowrap
        clr.l   ST_RING_HEAD(a3)
.nowrap:
        subq.l  #1,ST_RING_COUNT(a3)
        jsr     _LVOEnable(a6)

        bsr     dispatch_one             ; d2.l=type d3.l=code d5.w=x d6.w=y
        bra     .next
.empty:
        jsr     _LVOEnable(a6)
        rts

*-----------------------------------------------------------------------------
* dispatch_one -- translate one drained card event into a real InputEvent
* and IND_WRITEEVENT it. Input: a3=state, a6=SysBase, d2.l=event type
* (EV_*, zero-extended), d3.l=code (zero-extended), d5.w/d6.w=x/y. This
* card's own EVENT_QUALIFIER is never read at all here -- deliberately:
* docs/input-protocol.md sec 7 documents it as a dumb host pass-through
* that this driver must not trust, in favour of the running
* ST_HELD_QUALIFIER state this routine maintains itself.
*-----------------------------------------------------------------------------
dispatch_one:
        cmp.l   #EV_KEY_DOWN,d2
        beq     .key_down
        cmp.l   #EV_KEY_UP,d2
        beq     .key_up
        cmp.l   #EV_BUTTON_DOWN,d2
        beq     .button_down
        cmp.l   #EV_BUTTON_UP,d2
        beq     .button_up
        cmp.l   #EV_POINTER_MOTION,d2
        beq     .motion
        rts                               ; EV_NONE or unrecognised

.key_down:
        move.b  d3,d0
        bsr     modifier_bit_for_code
        or.w    d0,ST_HELD_QUALIFIER(a3)  ; a key-down's qualifier includes
                                           ; the key's own bit if it is
                                           ; itself a modifier
                                           ; (docs/input-protocol.md sec 7)
        move.w  ST_HELD_QUALIFIER(a3),d7
        move.b  d3,d1
        bra     .rawkey_common
.key_up:
        move.b  d3,d0
        bsr     modifier_bit_for_code
        not.l   d0
        and.w   d0,ST_HELD_QUALIFIER(a3)  ; ...and a key-up's does not,
                                           ; mirroring the button case §7
                                           ; documents explicitly
        move.w  ST_HELD_QUALIFIER(a3),d7
        move.b  d3,d1
        or.b    #IECODE_UP_PREFIX,d1
        bra     .rawkey_common

.rawkey_common:
        lea     ST_EVENTBUF(a3),a0
        clr.l   IE_NEXTEVENT(a0)
        move.b  #IECLASS_RAWKEY,IE_CLASS(a0)
        clr.b   IE_SUBCLASS(a0)
        moveq   #0,d0
        move.b  d1,d0
        move.w  d0,IE_CODE(a0)
        move.w  d7,IE_QUALIFIER(a0)
        clr.w   IE_POSITION(a0)
        clr.w   IE_POSITION+2(a0)
        clr.l   IE_TIMESTAMP(a0)
        clr.l   IE_TIMESTAMP+4(a0)        ; ie_TimeStamp: always zeroed --
                                           ; input.device fills it in on
                                           ; IND_WRITEEVENT (V36+); file
                                           ; header / docs sec 7
        bra     write_event

.button_down:
        bsr     button_qualifier_bit      ; in: d3.b -- out: d0.l
        or.w    d0,ST_HELD_QUALIFIER(a3)
        move.w  ST_HELD_QUALIFIER(a3),d7
        or.w    #IEQUALIFIER_RELATIVEMOUSE,d7  ; load-bearing -- without
                                           ; this a synthetic RAWMOUSE
                                           ; button event's zeroed position
                                           ; reads as an absolute jump to
                                           ; (0,0) (docs sec 7, action.c's
                                           ; "most of a day" bug)
        bsr     rawmouse_code_for_button  ; in: d3.b -- out: d1.b
        bra     .rawmouse_common
.button_up:
        bsr     button_qualifier_bit
        not.l   d0
        and.w   d0,ST_HELD_QUALIFIER(a3)
        move.w  ST_HELD_QUALIFIER(a3),d7
        or.w    #IEQUALIFIER_RELATIVEMOUSE,d7
        bsr     rawmouse_code_for_button
        or.b    #IECODE_UP_PREFIX,d1
        bra     .rawmouse_common

.rawmouse_common:
        lea     ST_EVENTBUF(a3),a0
        clr.l   IE_NEXTEVENT(a0)
        move.b  #IECLASS_RAWMOUSE,IE_CLASS(a0)
        clr.b   IE_SUBCLASS(a0)
        moveq   #0,d0
        move.b  d1,d0
        move.w  d0,IE_CODE(a0)
        move.w  d7,IE_QUALIFIER(a0)
        clr.w   IE_POSITION(a0)           ; zero delta -- a button press
        clr.w   IE_POSITION+2(a0)         ; doesn't move the pointer
        clr.l   IE_TIMESTAMP(a0)
        clr.l   IE_TIMESTAMP+4(a0)
        bra     write_event

.motion:
        move.w  ST_HELD_QUALIFIER(a3),d7
        move.l  ST_INTUITIONBASE(a3),a1
        move.l  IB_ACTIVESCREEN(a1),d0
        beq     .no_screen               ; no screen open yet -- nothing to
                                          ; aim an absolute position at; drop
                                          ; this motion event (harmless --
                                          ; §9's own "a later motion event
                                          ; completely supersedes it" logic
                                          ; already assumes drops like this
                                          ; are fine)
        move.l  d0,a2                    ; a2 = Screen*
        lea     ST_PIXBUF(a3),a0
        move.l  a2,IEPP_SCREEN(a0)
        move.w  d5,IEPP_X(a0)
        move.w  d6,IEPP_Y(a0)

        lea     ST_EVENTBUF(a3),a0
        clr.l   IE_NEXTEVENT(a0)
        move.b  #IECLASS_NEWPOINTERPOS,IE_CLASS(a0)
        move.b  #IESUBCLASS_PIXEL,IE_SUBCLASS(a0)
        move.w  #IECODE_NOBUTTON,IE_CODE(a0)
        move.w  d7,IE_QUALIFIER(a0)
        lea     ST_PIXBUF(a3),a1
        move.l  a1,IE_POSITION(a0)       ; ie_EventAddress (union with ie_x/y)
        clr.l   IE_TIMESTAMP(a0)
        clr.l   IE_TIMESTAMP+4(a0)
        bra     write_event
.no_screen:
        rts

*-----------------------------------------------------------------------------
* write_event -- IND_WRITEEVENT the InputEvent built at ST_EVENTBUF, via
* the one persistent IOStdReq (ST_IOREQ). Synchronous (DoIO): this task
* has nothing else to do while an event is in flight, and the whole point
* of the task/interrupt split (file header) is that this call happens
* here, never from IntHandler.
*-----------------------------------------------------------------------------
write_event:
        lea     ST_IOREQ(a3),a4
        move.w  #IND_WRITEEVENT,IO_COMMAND(a4)
        lea     ST_EVENTBUF(a3),a0
        move.l  a0,IO_DATA(a4)
        move.l  #INPUTEVENT_SIZE,IO_LENGTH(a4)
        clr.b   IO_FLAGS(a4)
        move.l  a4,a1
        jsr     _LVODoIO(a6)
        rts

*-----------------------------------------------------------------------------
* modifier_bit_for_code -- input: d0.b = raw key code. Output: d0.l = the
* matching IEQUALIFIER_* bit if this code is itself a modifier key, else
* 0. See this file's header for why this table is a hardware-convention
* citation, not an NDK header citation -- unlike every other constant in
* this file.
*-----------------------------------------------------------------------------
modifier_bit_for_code:
        cmp.b   #$60,d0
        bne.s   .not_lshift
        move.l  #IEQUALIFIER_LSHIFT,d0
        rts
.not_lshift:
        cmp.b   #$61,d0
        bne.s   .not_rshift
        move.l  #IEQUALIFIER_RSHIFT,d0
        rts
.not_rshift:
        cmp.b   #$62,d0
        bne.s   .not_caps
        move.l  #IEQUALIFIER_CAPSLOCK,d0
        rts
.not_caps:
        cmp.b   #$63,d0
        bne.s   .not_ctrl
        move.l  #IEQUALIFIER_CONTROL,d0
        rts
.not_ctrl:
        cmp.b   #$64,d0
        bne.s   .not_lalt
        move.l  #IEQUALIFIER_LALT,d0
        rts
.not_lalt:
        cmp.b   #$65,d0
        bne.s   .not_ralt
        move.l  #IEQUALIFIER_RALT,d0
        rts
.not_ralt:
        cmp.b   #$66,d0
        bne.s   .not_lcmd
        move.l  #IEQUALIFIER_LCOMMAND,d0
        rts
.not_lcmd:
        cmp.b   #$67,d0
        bne.s   .not_rcmd
        move.l  #IEQUALIFIER_RCOMMAND,d0
        rts
.not_rcmd:
        moveq   #0,d0
        rts

*-----------------------------------------------------------------------------
* button_qualifier_bit -- input: d3.b = button id (input::button --
* 0=LEFT, 1=RIGHT, 2=MIDDLE). Output: d0.l = the matching
* IEQUALIFIER_*BUTTON bit, or 0 if unrecognised (defensive: the card only
* ever sends 0-2 per its own MAX_BUTTON, but this driver doesn't lean on
* that blindly).
*-----------------------------------------------------------------------------
button_qualifier_bit:
        cmp.b   #0,d3
        bne.s   .not_left
        move.l  #IEQUALIFIER_LEFTBUTTON,d0
        rts
.not_left:
        cmp.b   #1,d3
        bne.s   .not_right
        move.l  #IEQUALIFIER_RBUTTON,d0
        rts
.not_right:
        cmp.b   #2,d3
        bne.s   .not_mid
        move.l  #IEQUALIFIER_MIDBUTTON,d0
        rts
.not_mid:
        moveq   #0,d0
        rts

*-----------------------------------------------------------------------------
* rawmouse_code_for_button -- input: d3.b = button id. Output: d1.b =
* IECODE_LBUTTON/RBUTTON/MBUTTON (devices/inputevent.h), or
* IECODE_NOBUTTON if unrecognised.
*-----------------------------------------------------------------------------
rawmouse_code_for_button:
        cmp.b   #0,d3
        bne.s   .not_left
        move.b  #IECODE_LBUTTON,d1
        rts
.not_left:
        cmp.b   #1,d3
        bne.s   .not_right
        move.b  #IECODE_RBUTTON,d1
        rts
.not_right:
        cmp.b   #2,d3
        bne.s   .not_mid
        move.b  #IECODE_MBUTTON,d1
        rts
.not_mid:
        move.b  #IECODE_NOBUTTON,d1
        rts

*-----------------------------------------------------------------------------
* Library/device name strings.
*-----------------------------------------------------------------------------
GfxName:
        dc.b    "graphics.library",0
        even
InputDevName:
        dc.b    "input.device",0
        even
IntuitionName:
        dc.b    "intuition.library",0
        even

* End of file. See this file's header for what this increment does and
* does not implement.
