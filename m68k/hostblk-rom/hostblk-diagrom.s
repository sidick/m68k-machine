*-----------------------------------------------------------------------------
* hostblk-diagrom.s -- the hostblk card's DiagArea boot ROM, hostblk.device
* exec device driver, and RDB boot-partition mounter.
*
* MIT License. Copyright (c) 2026 the m68k Machine project. See ../LICENSE.
*
* Scope, this increment: a struct Resident with deferred rt_Init (the
* DiagArea mechanism proved in the previous increment stays exactly as it
* was -- DiagEntry still just proves the board is reachable and marks the
* RAM copy); hostblk.device implementing OpenDevice/CloseDevice/BeginIO/
* AbortIO plus CMD_READ/CMD_WRITE/CMD_UPDATE/TD_GETGEOMETRY/TD_CHANGENUM/
* TD_CHANGESTATE/TD_PROTSTATUS/TD_READ64/TD_WRITE64/NSCMD_DEVICEQUERY;
* asynchronous completion via an INT2 interrupt server draining the
* completion queue (docs/hostblk-protocol.md section 5) and self-limiting
* against SUBMIT_CAPACITY (section 8) with a pending-request list rather
* than ever over-submitting; and an RDB partition mounter run from
* rt_Init that calls expansion.library's MakeDosNode/AddBootNode per
* partition. NOT implemented this increment: HD_SCSICMD direct-SCSI CDBs,
* TD_ADDCHANGEINT/TD_EJECT, and FileSystem.resource hunk-loading for a
* non-ROM-resident file system (PFS3/SFS) -- see the file's end-of-file
* summary comment for exactly what that means for the acceptance gate.
*
* Assembled with vasm (Motorola syntax) to a flat binary; see
* ../../scripts/build-hostblk-rom.sh. Two different addressing regimes
* meet in this one file, and each routine's header says which applies:
*
*   - The DiagArea (DiagStart..EndCopy below) is *copied* by
*     expansion.library to a RAM address chosen at boot time (RKRM 3rd
*     ed. "Expansion Library", "Events At DIAG Time"), so anything in it
*     that holds its own address (the Resident's rt_MatchTag/rt_EndSkip/
*     rt_Name/rt_IdString, which point back into itself) needs a runtime
*     patch -- DiagEntry does this, exactly as the previous increment's
*     header already explained for the (then absolute-pointer-free)
*     DiagArea alone.
*
*   - Everything after EndCopy executes *in place*, straight off this
*     board's own AUTOCONFIG bus window -- an AutoConfig board's window is
*     ordinary bus-addressable memory the CPU reads code from directly,
*     the same way it would from an actual ROM chip; nothing there is
*     ever copied anywhere. So it needs no patching at all and uses
*     ordinary compiler-grade PC-relative addressing (bra.w, lea
*     label(pc)) freely -- vasm resolves every such displacement at
*     assemble time, all labels are in this one file, and there is
*     nothing here resembling the cross-object-file `.short symA-symB`
*     trap `~/src/external/Copperline`'s guest/copperhf/entry.s and
*     guest/services/entry.s warn about (that trap is specific to linking
*     separately-assembled/compiled object files under gcc -mpcrel; a
*     single vasm -Fbin source file never hits it -- read for
*     understanding, not copied). One consequence: rt_Init's *target*
*     lives here, in the always-mapped board window, not in the DiagArea
*     RAM copy, so DiagEntry patches rt_Init with the *board base* (a0),
*     not the diag copy address (a2) -- see DiagEntry below.
*
*   Because this file's own DiagStart label sits at this board's ROM_BASE
*   offset (crates/machine-core/src/hostblk.rs) within the AUTOCONFIG
*   window, `lea DiagStart(pc),a0` from *any* routine below computes
*   (board base + ROM_BASE) at runtime -- `suba.l #HB_ROM_BASE,a0` from
*   there recovers the board base, i.e. the register file, with no
*   GetCurrentBinding()/ConfigDev search needed to find it (contrast
*   copperhf's device.c, which does need that search -- its register
*   window and ROM apparently are not both reachable this way). This
*   file still calls GetCurrentBinding() once, in resident_init, but only
*   for the ConfigDev pointer AddBootNode's own INPUTS require.
*
* Register access convention: every hostblk register (docs/hostblk-
* protocol.md section 4) is read and written with a plain `move.l`
* addressing the register's base offset directly, for byte-wide
* registers too. crates/machine-core/src/hostblk.rs's bus routing
* decomposes any CPU access into per-byte `read`/`write` calls in
* address order, and a byte register's three high lanes are defined as
* discarded-on-write/zero-on-read (section 4: "one hot byte per slot"),
* so a `move.l` naturally writes the reserved lanes harmlessly and ends
* with the low-order byte -- for DOORBELL, exactly the byte section 4
* says submits the doorbell. The previous increment's DiagEntry already
* proved this pattern against VERSION.
*
* Struct layout and register-convention sources, cited at each site
* below (not repeated in every equ block that draws on the same file):
*   - libraries/configregs.h, libraries/configvars.h (NDK 3.2): DiagArea,
*     ConfigDev, ExpansionRom.
*   - exec/resident.h, exec/nodes.h, exec/libraries.h, exec/devices.h,
*     exec/ports.h, exec/lists.h, exec/io.h, exec/interrupts.h,
*     exec/memory.h, hardware/intbits.h (NDK 3.2): Resident, Node,
*     Library/Device, Message/MsgPort, List/MinList, IORequest/IOStdReq,
*     Interrupt, MEMF_*/NT_* constants, INTB_PORTS (this board's INT2).
*   - devices/trackdisk.h, devices/newstyle.h (NDK 3.2): TD_*/NSCMD_*
*     command numbers, struct DriveGeometry, struct NSDeviceQueryResult.
*   - devices/hardblocks.h (NDK 3.2): RigidDiskBlock/PartitionBlock, the
*     on-disk RDB structures this file's mounter walks.
*   - dos/filehandler.h (NDK 3.2): DosEnvec, FileSysStartupMsg,
*     DeviceNode -- the structures MakeDosNode builds.
*   - Autodocs/expansion.doc (NDK 3.2): MakeDosNode's parameter-packet
*     shape, AddBootNode/GetCurrentBinding's register conventions.
*   - Autodocs/exec.doc (NDK 3.2): InitResident's non-AUTOINIT rt_Init
*     calling convention (D0=0, A0=segList, A6=ExecBase, D0 result =
*     device base or NULL), MakeLibrary's register conventions, the
*     device Open/Close/BeginIO/AbortIO calling convention (A6=device
*     base, A1=IORequest -- exec/io.h's own BEGINIO/ABORTIO macros show
*     this for BeginIO/AbortIO; Open/Close follow the same A1=IORequest
*     shape universally across every shipped Amiga device).
*   - Amiga ROM Kernel Reference Manual, 3rd ed. (1991), "Expansion
*     Library" chapter, "Events At DIAG Time": the RAM-copy precondition
*     on da_BootPoint (unchanged from the previous increment).
*   - ~/src/external/Copperline's guest/copperhf/ (GPL-3, read for
*     understanding only, nothing copied): the closest existing working
*     example of this exact shape (DiagArea + deferred-init Resident +
*     exec device + RDB mounter for a doorbell-style card). Its
*     README.md documents the ordering constraint this file also follows
*     -- mount partitions and AddBootNode them *before* AddIntServer
*     enables interrupt-driven completion, so the mounter's own
*     synchronous doorbell/completion polling (see chb_mount_all below)
*     never races a completion the interrupt handler would otherwise
*     drain first.
*-----------------------------------------------------------------------------

* ============================================================================
* hostblk register offsets (docs/hostblk-protocol.md section 4;
* crates/machine-core/src/hostblk.rs's `reg` module)
* ============================================================================
HB_UNIT_SELECT        equ     $00     ; W  byte
HB_UNIT_PRESENT        equ     $04     ; R  byte
HB_UNIT_WRITE_PROTECT   equ     $08     ; R  byte
HB_UNIT_CHANGE_COUNT    equ     $0c     ; R  u32
HB_UNIT_SECTORS_HI      equ     $10     ; R  u32
HB_UNIT_SECTORS_LO      equ     $14     ; R  u32
HB_DOORBELL             equ     $18     ; W  u32
HB_SUBMIT_OVERFLOW      equ     $1c     ; R  u32
HB_COMPLETION_PTR       equ     $20     ; R  u32
HB_COMPLETION_ERROR     equ     $24     ; R  byte
HB_COMPLETION_ACTUAL    equ     $28     ; R  u32
HB_COMPLETION_ADVANCE   equ     $2c     ; W  byte
HB_COMPLETION_COUNT     equ     $30     ; R  u32
HB_INT_ENABLE           equ     $34     ; RW byte
HB_SUBMIT_CAPACITY      equ     $38     ; R  u32 -- read it, never hardcode
HB_VERSION              equ     $3c     ; R  u32

* Descriptor command codes and completion error codes (protocol section 3
* and section 6; hostblk.rs's `cmd`/`error` modules).
HB_CMD_READ             equ     1
HB_CMD_WRITE            equ     2
HB_CMD_FLUSH            equ     3

HB_ERR_OK               equ     0
HB_ERR_BAD_UNIT         equ     1
HB_ERR_WRITE_PROTECTED  equ     2
HB_ERR_INVALID_COMMAND  equ     3
HB_ERR_INVALID_LENGTH   equ     4
HB_ERR_MISALIGNED       equ     5
HB_ERR_OUT_OF_RANGE     equ     6
HB_ERR_BAD_ADDRESS      equ     7
HB_ERR_IO_ERROR         equ     8

* 20-byte descriptor layout (protocol section 3), written by the driver,
* never by the host.
DESC_COMMAND    equ     0       ; byte
DESC_UNIT       equ     1       ; byte
* DESC_RESERVED at 2, 2 bytes, must be zero -- no symbol needed, never
* written explicitly (the slot pool is AllocMem'd with MEMF_CLEAR).
DESC_LENGTH     equ     4       ; u32
DESC_OFFSET     equ     8       ; u64 (big-endian: hi at +8, lo at +12)
DESC_BUFFER     equ     16      ; u32
DESC_SIZE       equ     20

* crates/machine-core/src/hostblk.rs: `pub const ROM_BASE: u32 = 0x1000;`
* -- where DIAG_ROM (this file, assembled) is mapped in the board's own
* AUTOCONFIG window. See the file header for how routines below use this
* to recover the board base from their own PC.
HB_ROM_BASE     equ     $1000

* ============================================================================
* AmigaOS structure offsets, hand-derived from the NDK 3.2 C headers cited
* in the file header (this project's convention, matching the previous
* increment and device_layout.h's CHF_DEV_BOARDBASE_OFFSET comment in
* Copperline -- vasm has no C struct-layout support, so every offset here
* is a citation, not a guess).
* ============================================================================

* exec/nodes.h: struct Node (14 bytes) -- ln_Succ.l ln_Pred.l ln_Type.b
* ln_Pri.b ln_Name.l.
LN_TYPE         equ     8
LN_NAME         equ     10
NODE_SIZE       equ     14

NT_INTERRUPT    equ     2
NT_DEVICE       equ     3

* exec/libraries.h: struct Library (34 bytes) -- struct Node lib_Node(14),
* UBYTE lib_Flags, UBYTE lib_pad, UWORD lib_NegSize, UWORD lib_PosSize,
* UWORD lib_Version, UWORD lib_Revision, APTR lib_IdString, ULONG
* lib_Sum, UWORD lib_OpenCnt. exec/devices.h: struct Device is exactly
* struct Library, no extra fields -- so LIB_SIZE below is also this
* device's own base-structure size before its private data starts.
LIB_FLAGS       equ     14
LIB_VERSION     equ     20
LIB_REVISION    equ     22
LIB_IDSTRING    equ     24
LIB_OPENCNT     equ     32
LIB_SIZE        equ     34

LIBF_DELEXP     equ     (1<<3)

* exec/resident.h: struct Resident (26 bytes) -- UWORD rt_MatchWord,
* APTR rt_MatchTag, APTR rt_EndSkip, UBYTE rt_Flags, UBYTE rt_Version,
* UBYTE rt_Type, BYTE rt_Pri, char *rt_Name, char *rt_IdString,
* APTR rt_Init.
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

* exec/ports.h: struct Message (20 bytes) -- struct Node mn_Node(14),
* struct MsgPort *mn_ReplyPort, UWORD mn_Length. struct MsgPort (34
* bytes) -- struct Node mp_Node(14), UBYTE mp_Flags, UBYTE mp_SigBit,
* void *mp_SigTask, struct List mp_MsgList(14).
MN_SIZE         equ     20

* exec/lists.h: struct List (14 bytes) -- Node* lh_Head, Node* lh_Tail,
* Node* lh_TailPred, UBYTE lh_Type, UBYTE l_pad.
LH_HEAD         equ     0
LH_TAIL         equ     4
LH_TAILPRED     equ     8
LIST_SIZE       equ     14

* exec/io.h: struct IORequest (32 bytes, = MN_SIZE + APTR io_Device +
* APTR io_Unit + UWORD io_Command + UBYTE io_Flags + BYTE io_Error).
* struct IOStdReq extends it (48 bytes total) with ULONG io_Actual,
* ULONG io_Length, APTR io_Data, ULONG io_Offset -- io.i's own IO_*
* offsets (transcribed by hand from the same header, since vasm cannot
* consume Include_I's STRUCTURE-macro form directly).
IO_DEVICE       equ     20
IO_UNIT         equ     24
IO_COMMAND      equ     28
IO_FLAGS        equ     30
IO_ERROR        equ     31
IO_ACTUAL       equ     32
IO_LENGTH       equ     36
IO_DATA         equ     40
IO_OFFSET       equ     44
IOSTD_SIZE      equ     48

IOB_QUICK       equ     0

* exec/io.h, devices/trackdisk.h, devices/newstyle.h: device command
* numbers this driver implements.
CMD_RESET       equ     1
CMD_READ        equ     2
CMD_WRITE       equ     3
CMD_UPDATE      equ     4
CMD_CLEAR       equ     5
CMD_FLUSH       equ     8
TD_CHANGENUM    equ     (CMD_FLUSH+1+4)         ; CMD_NONSTD(9)+4 = 13
TD_CHANGESTATE  equ     (CMD_FLUSH+1+5)         ; 14
TD_PROTSTATUS   equ     (CMD_FLUSH+1+6)         ; 15
TD_GETGEOMETRY  equ     (CMD_FLUSH+1+13)        ; 22
TD_READ64       equ     (CMD_FLUSH+1+15)        ; 24
TD_WRITE64      equ     (CMD_FLUSH+1+16)        ; 25
NSCMD_DEVICEQUERY equ   $4000
NSCMD_TD_READ64 equ     $c000
NSCMD_TD_WRITE64 equ    $c001

* exec/errors.h, devices/trackdisk.h: io_Error values this driver
* returns, mapped from hostblk's own completion error codes (protocol
* section 6) by hb_err_table below.
IOERR_OPENFAIL  equ     -1
IOERR_ABORTED   equ     -2
IOERR_NOCMD     equ     -3
IOERR_BADLENGTH equ     -4
IOERR_BADADDRESS equ    -5
TDERR_NotSpecified equ  20
TDERR_WriteProt equ     28
TDERR_SeekError equ     30
TDERR_BadUnitNum equ    32

* devices/trackdisk.h: struct DriveGeometry (32 bytes).
DG_SECTORSIZE   equ     0
DG_TOTALSECTORS equ     4
DG_CYLINDERS    equ     8
DG_CYLSECTORS   equ     12
DG_HEADS        equ     16
DG_TRACKSECTORS equ     20
DG_BUFMEMTYPE   equ     24
DG_DEVICETYPE   equ     28
DG_FLAGS        equ     29
DG_SIZE         equ     32
DG_DIRECT_ACCESS equ    0

* devices/newstyle.h: struct NSDeviceQueryResult (20 bytes read-only
* part used here).
NSDQR_FORMAT    equ     0
NSDQR_SIZEAVAIL equ     4
NSDQR_DEVTYPE   equ     8
NSDQR_DEVSUBTYPE equ    10
NSDQR_SUPPORTED equ     12
NSDQR_SIZE      equ     16      ; devices/newstyle.h: ULONG DevQueryFormat +
                                ; ULONG SizeAvailable + UWORD DeviceType +
                                ; UWORD DeviceSubType + APTR
                                ; SupportedCommands = 16, not 20. Was 20,
                                ; which over-claimed both nsdqr_SizeAvailable
                                ; and io_Actual by four bytes; devsoak
                                ; rejected the reply as malformed and fell
                                ; back to the 32-bit CMD dialect only, so
                                ; TD64/NSD went untested despite being
                                ; implemented.
NSDEVTYPE_TRACKDISK equ 5

* exec/interrupts.h: struct Interrupt (22 bytes) -- struct Node
* is_Node(14), APTR is_Data, VOID (*is_Code)().
IS_DATA         equ     14
IS_CODE         equ     18
INTERRUPT_SIZE  equ     22

* hardware/intbits.h: this board's INT2 is the CPU-level-2 "PORTS"
* server chain -- INTB_PORTS, not a raw CPU interrupt level (AddIntServer
* takes an exec server-chain index, exec/interrupts.h/hardware/
* intbits.h).
INTB_PORTS      equ     3

* exec/memory.h.
MEMF_PUBLIC     equ     (1<<0)
MEMF_CHIP       equ     (1<<1)
MEMF_FAST       equ     (1<<2)
MEMF_CLEAR      equ     (1<<16)

* libraries/configvars.h: struct ConfigDev -- struct Node cd_Node(14),
* UBYTE cd_Flags, UBYTE cd_Pad, struct ExpansionRom cd_Rom(16),
* APTR cd_BoardAddr, ULONG cd_BoardSize, ... . Only cd_BoardAddr is used
* below (to sanity-confirm the PC-relative board-base recovery in
* resident_init).
CD_BOARDADDR    equ     32

* Autodocs/expansion.doc: struct CurrentBinding -- ConfigDev *
* cb_ConfigDev, STRPTR cb_FileName, STRPTR cb_ProductString,
* STRPTR *cb_ToolTypes. GetCurrentBinding(cb, sizeof(cb)) fills as many
* leading fields as fit; only cb_ConfigDev (the first) is used here.
CB_CONFIGDEV    equ     0
CB_SIZE         equ     16

* devices/hardblocks.h: struct RigidDiskBlock (fields used here only --
* the full struct is far larger; every field this mounter reads is
* listed with its real offset, computed by hand-summing the header's
* field list up to that point).
RDB_ID                  equ     0
RDB_PARTITIONLIST        equ     28
IDNAME_RIGIDDISK         equ     $5244534b       ; 'RDSK'
RDB_LOCATION_LIMIT       equ     16

* devices/hardblocks.h: struct PartitionBlock.
* Offsets by hand-summing the header's fields: pb_ID(0) pb_SummedLongs(4)
* pb_ChkSum(8) pb_HostID(12) pb_Next(16) pb_Flags(20) pb_Reserved1[2](24)
* pb_DevFlags(32) pb_DriveName[32](36) pb_Reserved2[15](68)
* pb_Environment[20](128).
PB_ID           equ     0
PB_NEXT         equ     16
PB_FLAGS        equ     20
PB_DRIVENAME    equ     36      ; NOT 32 -- 32 is pb_DevFlags ("preferred
                                 ; flags for OpenDevice", a ULONG whose
                                 ; high byte is 0), which an earlier
                                 ; increment misread as the name's BSTR
                                 ; length byte, silently synthesizing
                                 ; "HB0" for every partition
PB_ENVIRONMENT  equ     128
IDNAME_PARTITION equ    $50415254       ; 'PART'
PBFB_BOOTABLE   equ     0
PBFF_NOMOUNT    equ     2

* dos/filehandler.h: struct DosEnvec index numbers (longwords, matching
* struct PartitionBlock's pb_Environment array one-for-one -- the RDB's
* own documented convention, hardblocks.h's own comment on
* pb_Environment: "environment vector for this partition").
DE_TABLESIZE    equ     0
DE_BOOTPRI      equ     15
DE_DOSTYPE      equ     16

* dos/filehandler.h: MakeDosNode's parameter-packet longword indices
* (Autodocs/expansion.doc's INPUTS list).
PP_DOSNAME      equ     0
PP_DEVNAME      equ     4
PP_UNIT         equ     8
PP_FLAGS        equ     12
PP_ENV          equ     16      ; longword 4 (env table size) onward

* This device's own private data, appended after the standard Device
* (struct Library) fields MakeLibrary allocates -- offsets past LIB_SIZE
* are this file's own layout, not an OS structure, so no citation beyond
* "chosen here".
DEV_SYSBASE     equ     LIB_SIZE                ; APTR ExecBase
DEV_BOARDBASE   equ     DEV_SYSBASE+4           ; APTR, register file base
DEV_EXPBASE     equ     DEV_BOARDBASE+4         ; APTR, expansion.library base
                                                 ; (opened once in resident_init,
                                                 ; never closed -- see its comment)
DEV_CONFIGDEV   equ     DEV_EXPBASE+4           ; APTR, this board's ConfigDev,
                                                 ; from GetCurrentBinding -- kept
                                                 ; only for AddBootNode's INPUTS
DEV_CAPACITY    equ     DEV_CONFIGDEV+4         ; ULONG, cached SUBMIT_CAPACITY
DEV_OUTSTANDING equ     DEV_CAPACITY+4          ; ULONG, slots currently in flight
DEV_SLOTBASE    equ     DEV_OUTSTANDING+4       ; APTR, DEV_CAPACITY*SLOT_SIZE pool
DEV_PENDING     equ     DEV_SLOTBASE+4          ; struct List (14), IORequests
                                                 ; waiting for a free slot
DEV_INT         equ     DEV_PENDING+LIST_SIZE   ; struct Interrupt (22),
                                                 ; embedded here rather than
                                                 ; AllocMem'd separately --
                                                 ; AddIntServer links it in
                                                 ; place, so it must live as
                                                 ; long as the device does,
                                                 ; which this struct already
                                                 ; does by construction
DEV_OPENCALLS   equ     DEV_INT+INTERRUPT_SIZE  ; ULONG, diagnostic: every
                                                 ; dev_open *attempt* (before
                                                 ; any validation), so
                                                 ; --inspect can distinguish
                                                 ; "OpenDevice never called"
                                                 ; from "called and rejected"
DEV_SIZE        equ     DEV_OPENCALLS+4

* One submission slot: a 20-byte descriptor plus the 4-byte pointer to
* the IORequest that owns it (0 = slot free). The descriptor's own
* address (SLOT_DESC offset 0) is exactly what gets written to
* HB_DOORBELL and later read back from HB_COMPLETION_PTR, so a slot's
* address IS its descriptor's address -- no separate translation table.
SLOT_DESC       equ     0
SLOT_OWNER      equ     DESC_SIZE
SLOT_SIZE       equ     SLOT_OWNER+4

* ============================================================================
* exec.library / expansion.library LVOs (Include_I/lvo/exec_lib.i,
* Include_I/lvo/expansion_lib.i -- transcribed by hand for the same
* reason the struct offsets above are; vasm never reads those files, but
* the numbers are exactly theirs, not independently derived).
* ============================================================================
_LVODisable     equ     -120
_LVOEnable      equ     -126
_LVOAddIntServer equ    -168
_LVOAllocMem    equ     -198
_LVOFreeMem     equ     -210
_LVOReplyMsg    equ     -378
_LVOAddDevice   equ     -432
_LVOAddTail     equ     -246
_LVORemove      equ     -252
_LVORemHead     equ     -258

_LVOAddBootNode equ     -36
_LVOGetCurrentBinding equ -138
_LVOMakeDosNode equ     -144

_LVOMakeLibrary equ     -84
_LVOFindResident equ    -96
_LVOInitResident equ    -102
_LVOOpenLibrary equ     -552

* ---- libraries/configregs.h: struct DiagArea's da_Config bit layout ------
* da_Config's two independent bitfields (configregs.h: DAC_BUSWIDTH mask
* $C0, DAC_BOOTTIME mask $30). Unchanged from the previous increment.
DAC_WORDWIDE    equ     $80
DAC_CONFIGTIME  equ     $10

*-----------------------------------------------------------------------------
* struct DiagArea (configregs.h): UBYTE da_Config, UBYTE da_Flags,
* UWORD da_Size, UWORD da_DiagPoint, UWORD da_BootPoint, UWORD da_Name,
* UWORD da_Reserved01, UWORD da_Reserved02 -- 14 bytes. da_DiagPoint/
* da_BootPoint/da_Name are word offsets from DiagStart, computed as label
* differences.
*
* This increment extends da_Size (the "OtherData" RKRM's worked example
* describes) to also cover a struct Resident and its two name strings,
* right after the diagnostic marker the previous increment already used
* -- DiagMarker's own offset (14, right after the 14-byte DiagArea
* header) is unchanged and still asserted by hostblk.rs's own tests, so
* nothing here may move it.
*-----------------------------------------------------------------------------
DiagStart:
        dc.b    DAC_WORDWIDE+DAC_CONFIGTIME  ; da_Config
        dc.b    0                       ; da_Flags -- none defined, must be 0
        dc.w    EndCopy-DiagStart       ; da_Size: bytes copied into RAM
        dc.w    DiagEntry-DiagStart     ; da_DiagPoint
        dc.w    BootEntry-DiagStart     ; da_BootPoint (see DAC_CONFIGTIME,
                                         ; previous increment's header)
        dc.w    0                       ; da_Name: no identifier string yet
        dc.w    0                       ; da_Reserved01 -- must be zero
        dc.w    0                       ; da_Reserved02 -- must be zero

* Diagnostic scratch cell: DiagEntry writes the VERSION register's value
* here so a host-side inspector can confirm DiagEntry ran (previous
* increment's mechanism, unchanged).
DiagMarker:
        dc.l    0

* Diagnostic scratch cell #2 (offset 18 in the RAM copy): BootEntry
* counts its own invocations here, so a host-side inspector can answer
* "does this Kickstart's strap actually call da_BootPoint?" with guest
* evidence instead of folklore (copperhf's entry.s asserts V36+ never
* calls it; nothing in this project had ever tested that).
BootMarker:
        dc.l    0

*-----------------------------------------------------------------------------
* DiagEntry -- da_DiagPoint. Calling convention (configregs.h; previous
* increment's header quotes it in full): A0=board base, A2=RAM copy base.
* Return D0 non-zero to keep the RAM copy (required: both the VERSION
* proof-of-life marker and the Resident struct below need to live in it
* permanently). This code must itself be *inside* da_Size -- da_DiagPoint
* is an offset Kickstart adds to the RAM copy's own address, not this
* ROM's, so DiagEntry has to physically be part of what gets copied
* (hence it comes before EndCopy, right after the marker, exactly where
* the previous increment already had it -- only BootEntry and the
* Resident struct/strings after it are new).
*-----------------------------------------------------------------------------
DiagEntry:
        move.l  HB_VERSION(a0),d0      ; read hostblk's own VERSION register
        move.l  d0,(DiagMarker-DiagStart)(a2)  ; previous increment's proof
                                                ; DiagEntry actually ran
        move.l  a2,d0
        add.l   d0,(RtMatchTag-DiagStart)(a2)
        add.l   d0,(RtEndSkip-DiagStart)(a2)
        add.l   d0,(RtName-DiagStart)(a2)
        add.l   d0,(RtIdString-DiagStart)(a2)
        move.l  a0,d0                   ; rt_Init: board base + ROM_BASE (code
        add.l   #HB_ROM_BASE,d0         ; lives in the mapped window at
        add.l   d0,(RtInitField-DiagStart)(a2)  ; boardbase+ROM_BASE, not at
                                         ; boardbase+0 like the registers --
                                         ; see struct Resident comment below)
        moveq   #1,d0
        rts

*-----------------------------------------------------------------------------
* BootEntry -- da_BootPoint. NOT a placeholder: this Kickstart's (3.2.2,
* V47) strap really does call it, and it is the node's *entire* boot
* attempt. An earlier increment left this a bare RTS on the belief that
* "V36+ never calls da_BootPoint at all -- AddBootNode suffices"
* (copperhf's entry.s asserts the same); BootMarker below disproved that
* with guest evidence (--inspect read it back as 1 after a boot), and
* disassembly of this ROM's own strap module (strap 47.2, code at
* $FC746E/$FC769C) shows the exact gate: for every non-floppy BootNode
* whose bn_Node.ln_Name ConfigDev has ERTF_DIAGVALID set, a non-zero
* diag-copy address in er_Reserved0c, and DAC_CONFIGTIME in the copied
* da_Config, strap (1) moves that BootNode to MountList's head, (2)
* pushes the ConfigDev on the stack C-style and calls da_BootPoint with
* A6=ExecBase (A5=ExpansionBase live too -- strap sets EBB_SILENTSTART
* on it first), then (3) if it *returns*, re-Enqueues the node and moves
* on. A bare RTS therefore made every hostblk boot fail silently, with
* the device never opened -- exactly the "DOS never reads a single
* block" symptom.
*
* The implementation is the RKRM 3rd ed. autoboot convention (Appendix,
* A2091-style boot ROMs; "Expansion Library" chapter): find dos.library's
* Resident and call its rt_Init -- DOS then takes over the machine and
* boots from MountList's head node, which strap just made ours. On
* success that call never returns; if it does (or dos.library is
* missing), fall through to RTS and let strap try the next node.
*-----------------------------------------------------------------------------
BootEntry:
        move.l  a0,-(sp)
        lea     BootMarker(pc),a0        ; both labels live in the RAM copy,
        addq.l  #1,(a0)                   ; so the pc-relative displacement
        move.l  (sp)+,a0                  ; survives the copy unchanged --
                                           ; kept as a permanent diagnostic:
                                           ; --inspect reports this cell as
                                           ; "da_BootPoint call count"
        lea     DosResName(pc),a1
        jsr     _LVOFindResident(a6)     ; A1=name (exec.doc FindResident)
        tst.l   d0
        beq.s   .no_dos
        move.l  d0,a1
        moveq   #0,d1                    ; segList = NULL: ROM module.
        jsr     _LVOInitResident(a6)     ; NOT a raw jsr into rt_Init: exec's
                                          ; InitResident supplies the
                                          ; documented entry conditions
                                          ; (exec.doc: D0=0, A0=segList,
                                          ; A6=ExecBase) -- a first draft
                                          ; jumped rt_Init directly with a
                                          ; stale D0/A0, and DOS came up just
                                          ; far enough to mount the boot
                                          ; volume and then stalled without
                                          ; ever running Startup-Sequence.
                                          ; On success this never returns.
.no_dos:
        rts

*-----------------------------------------------------------------------------
* struct Resident ("Romtag"; exec/resident.h), inside the RAM copy so
* Kickstart's cold-start scan (which walks system memory, not arbitrary
* AutoConfig board space -- see the file header) can find it. rt_Type is
* NT_DEVICE (this board is a device, not a library or DOS handler);
* rt_Flags is RTF_COLDSTART only (not RTF_AUTOINIT: rt_Init here is
* ordinary code, called per InitResident's non-AUTOINIT convention --
* D0=0, A0=segList, A6=ExecBase, D0 result=our device base or NULL --
* not the four-longword MakeLibrary-table shape AUTOINIT expects).
*
* rt_MatchTag/rt_EndSkip/rt_Name/rt_IdString point back into this same
* RAM copy, so DiagEntry patches them with +a2 (the copy's own runtime
* address). rt_Init points into the always-mapped board window instead
* (RtInit, after EndCopy below), so DiagEntry patches it with +a0 (the
* board base DiagPoint was handed) rather than +a2.
*-----------------------------------------------------------------------------
Romtag:
        dc.w    RTC_MATCHWORD                   ; rt_MatchWord
RtMatchTag:
        dc.l    Romtag-DiagStart                ; rt_MatchTag (patched: +a2)
RtEndSkip:
        dc.l    EndCopy-DiagStart                ; rt_EndSkip (patched: +a2)
        dc.b    RTF_COLDSTART                    ; rt_Flags
        dc.b    0                                ; rt_Version
        dc.b    NT_DEVICE                        ; rt_Type
        dc.b    20                               ; rt_Pri
RtName:
        dc.l    DeviceNameString-DiagStart        ; rt_Name (patched: +a2)
RtIdString:
        dc.l    IdString-DiagStart                ; rt_IdString (patched: +a2)
RtInitField:
        dc.l    RtInit-DiagStart                  ; rt_Init (patched: +a0,
                                                    ; see above -- RtInit is
                                                    ; NOT in the RAM copy)

DeviceNameString:
        dc.b    "hostblk.device",0
        even
* Inside the copy region deliberately: BootEntry runs in the RAM copy and
* reaches this PC-relative, so the string must be copied along with it.
DosResName:
        dc.b    "dos.library",0
        even
IdString:
        dc.b    "hostblk.device 1.0 (2026)",0
        even

EndCopy:

*=============================================================================
* Everything below here executes in place, straight off this board's own
* AUTOCONFIG window -- see the file header for why that means ordinary
* PC-relative addressing (no patching) but also why rt_Init was patched
* with (board base + HB_ROM_BASE), not with a2.
*=============================================================================

*-----------------------------------------------------------------------------
* RtInit -- rt_Init, called by InitResident's non-AUTOINIT path
* (Autodocs/exec.doc, "InitResident"): D0=0, A0=segList (NULL for a ROM
* module), A6=ExecBase. Builds hostblk.device with MakeLibrary, wires up
* its own private state (board base, capacity-sized slot pool, pending
* list, embedded Interrupt), mounts every unit's RDB partitions and
* AddBootNode's them (copperhf's README.md: strictly before
* AddIntServer/interrupt-driven completion, so the mounter's own
* synchronous doorbell polling below can never race the interrupt
* handler draining the same completion queue), then finally enables
* INT2 and AddDevice()s it. Returns the device base in D0, or NULL (per
* InitResident's own contract) on any unrecoverable failure.
*-----------------------------------------------------------------------------
RtInit:
        movem.l d2-d7/a2-a6,-(sp)

        lea     DiagStart(pc),a5
        suba.l  #HB_ROM_BASE,a5         ; a5 = board base (file header)

        move.l  HB_VERSION(a5),d0       ; refuse a protocol version we don't
        cmp.l   #1,d0                   ; know (docs/hostblk-protocol.md
        bne     .fail                   ; section 4)

        lea     FuncTable(pc),a0
        suba.l  a1,a1                    ; structure = NULL
        suba.l  a2,a2                    ; init = NULL
        move.l  #DEV_SIZE,d0
        moveq   #0,d1                    ; segList
        jsr     _LVOMakeLibrary(a6)      ; a6 = SysBase throughout (InitResident
        tst.l   d0                       ; handed it to us; every exec LVO
        beq     .fail                    ; below wants it there too)
        move.l  d0,a3                    ; a3 = device base, kept for the rest

        move.l  a6,DEV_SYSBASE(a3)
        move.l  a5,DEV_BOARDBASE(a3)
        clr.l   DEV_EXPBASE(a3)
        clr.l   DEV_CONFIGDEV(a3)

        move.b  #NT_DEVICE,LN_TYPE(a3)
        moveq   #0,d0
        move.b  d0,LN_TYPE+1(a3)         ; ln_Pri = 0
        lea     DevName(pc),a0
        move.l  a0,LN_NAME(a3)
        move.l  #1,d0
        move.w  d0,LIB_VERSION(a3)
        clr.w   LIB_REVISION(a3)
        lea     DevIdString(pc),a0
        move.l  a0,LIB_IDSTRING(a3)
        clr.b   LIB_FLAGS(a3)
        clr.w   LIB_OPENCNT(a3)

        ; Empty-list init for the pending queue (exec/lists.h's own
        ; "two ghost nodes" trick: lh_Head = &lh_Tail, lh_Tail = NULL,
        ; lh_TailPred = &lh_Head -- unchanged by AddTail/RemHead/Remove,
        ; which all already assume exactly this shape).
        lea     DEV_PENDING(a3),a0
        move.l  a0,d0
        addq.l  #4,d0
        move.l  d0,LH_HEAD(a0)
        clr.l   LH_TAIL(a0)
        move.l  a0,LH_TAILPRED(a0)
        clr.b   LH_TAILPRED+4(a0)        ; lh_Type -- unused (NT_UNKNOWN)

        clr.l   DEV_OUTSTANDING(a3)
        clr.l   DEV_OPENCALLS(a3)

        move.l  (HB_SUBMIT_CAPACITY)(a5),d0   ; never hardcode this (protocol
        move.l  d0,DEV_CAPACITY(a3)           ; section 8) -- it sizes the
        tst.l   d0                            ; slot pool below
        beq     .fail                         ; a zero-depth ring can never
                                               ; be used -- treat as fatal
                                               ; rather than silently offering
                                               ; a device that can only ever
                                               ; queue and never submit
        mulu.w  #SLOT_SIZE,d0            ; capacity fits comfortably in a
                                          ; word -- mulu.w's 32-bit result
                                          ; lands back in d0
        move.l  #MEMF_PUBLIC+MEMF_CLEAR,d1  ; MEMF_FAST here would make this
                                          ; device's own construction fail
                                          ; outright on a machine with no
                                          ; fast RAM attached at all --
                                          ; confirmed the hard way (AllocMem
                                          ; returned NULL, RtInit silently
                                          ; bailed, no error anywhere). The
                                          ; protocol only says descriptor/
                                          ; buffer addresses *may* be
                                          ; MEMF_FAST (section 12), never
                                          ; that they must be -- MEMF_PUBLIC
                                          ; is satisfied by whatever memory
                                          ; actually exists.
        jsr     _LVOAllocMem(a6)
        tst.l   d0
        beq     .fail
        move.l  d0,DEV_SLOTBASE(a3)

        ; struct Interrupt (exec/interrupts.h), embedded in the device so
        ; it lives exactly as long as the device does (DEV_INT's own
        ; comment). AddIntServer links it in place -- it is never copied.
        lea     DEV_INT(a3),a0
        move.b  #NT_INTERRUPT,LN_TYPE(a0)
        clr.b   LN_TYPE+1(a0)            ; is_Node.ln_Pri = 0
        lea     DevName(pc),a1
        move.l  a1,LN_NAME(a0)
        move.l  a3,IS_DATA(a0)           ; is_Data = device base -- int_handler
                                          ; re-derives boardbase/sysbase from it
        lea     IntHandler(pc),a1
        move.l  a1,IS_CODE(a0)

        move.b  #NT_DEVICE,LN_TYPE(a3)   ; (LN_TYPE(a3) was already NT_DEVICE
                                          ; from above; re-asserted here only
                                          ; because the Interrupt-struct code
                                          ; just above reused a0 at DEV_INT(a3)
                                          ; -- a3's own lib_Node fields are
                                          ; untouched by that, this line is
                                          ; belt-and-braces, not a fix)

        ; expansion.library: needed for GetCurrentBinding (this board's own
        ; ConfigDev, required by AddBootNode's own INPUTS) and MakeDosNode/
        ; AddBootNode themselves. Opened once and never closed -- it is a
        ; core system library for the life of the machine regardless, so
        ; leaving one open count on it is harmless and simpler than
        ; threading a CloseLibrary through every exit path below.
        lea     ExpName(pc),a1          ; OpenLibrary: A1=libName, D0=version
        moveq   #0,d0                   ; (Include_H/inline/exec_protos.h --
        jsr     _LVOOpenLibrary(a6)     ; NOT A0, unlike most other calls here)
        move.l  d0,DEV_EXPBASE(a3)
        beq     .mount_done              ; no expansion.library -- device
                                          ; still works, just unbootable
                                          ; (AddDevice below still runs)

        move.l  d0,a6                    ; a6 = ExpansionBase for this call
        sub.l   #CB_SIZE,sp
        move.l  sp,a0
        move.l  #CB_SIZE,d0
        jsr     _LVOGetCurrentBinding(a6)
        move.l  (sp),d0                  ; cb_ConfigDev
        add.l   #CB_SIZE,sp
        move.l  DEV_SYSBASE(a3),a6       ; back to SysBase
        move.l  d0,DEV_CONFIGDEV(a3)
        beq     .mount_done              ; no binding -- can't AddBootNode
                                          ; (INPUTS: "Pass a NULL ConfigDev
                                          ; pointer to create a non-bootable
                                          ; node" -- mount_all checks this
                                          ; itself too, this is a fast exit)

        bsr     mount_all
.mount_done:

        move.l  a3,a1
        jsr     _LVOAddDevice(a6)

        move.l  #INTB_PORTS,d0
        lea     DEV_INT(a3),a1
        jsr     _LVOAddIntServer(a6)

        moveq   #1,d0
        move.l  d0,(HB_INT_ENABLE)(a5)   ; only now -- after every unit is
                                          ; mounted -- do completions start
                                          ; arriving through the interrupt
                                          ; path (this file's header, citing
                                          ; copperhf's README.md)

        move.l  a3,d0
        movem.l (sp)+,d2-d7/a2-a6
        rts

.fail:
        moveq   #0,d0
        movem.l (sp)+,d2-d7/a2-a6
        rts

*-----------------------------------------------------------------------------
* MakeLibrary's vector table (Autodocs/exec.doc "MakeLibrary" INPUTS): a
* leading -1 selects word-displacement mode (each entry is a word offset
* from the table's own address, the only form that can be an assemble-
* time constant); the four standard LVOs (Open/Close/Expunge/Reserved)
* come first, exactly like every Amiga library/device, followed by this
* device's own BeginIO/AbortIO. Every entry here is a same-file label
* difference (see file header -- this is a single-object vasm build, so
* there is no cross-object-file `.short symA-symB` hazard to route
* around with trampolines the way copperhf's gcc-linked entry.s must).
*-----------------------------------------------------------------------------
* NO SPACES inside these expressions: vasm's Motorola syntax ends an
* operand at the first blank and silently treats the rest of the line as
* a comment, so `dc.w dev_open - FuncTable` assembles as `dc.w dev_open`
* -- the label's bare section offset -- and MakeLibrary then builds every
* vector pointing FuncTable's own offset *past* the real routine.
* Confirmed by minimal repro against vasm 2.0b, and it was this file's
* actual shipped bug: OpenDevice jumped into the middle of unrelated code
* and hostblk.device could never be opened.
FuncTable:
        dc.w    -1
        dc.w    dev_open-FuncTable
        dc.w    dev_close-FuncTable
        dc.w    dev_expunge-FuncTable
        dc.w    dev_extfunc-FuncTable
        dc.w    dev_beginio-FuncTable
        dc.w    dev_abortio-FuncTable
        dc.w    -1

*-----------------------------------------------------------------------------
* dev_open -- Open vector (LVO -6). Universal Amiga device convention
* (file header): A6=device base, A1=IORequest, D0=unit, D1=flags. Must
* set io_Error and, on failure, io_Device=0 (RKRM); on success it must
* bump lib_OpenCnt itself -- exec does not do this for devices.
*-----------------------------------------------------------------------------
dev_open:
        movem.l d2-d7/a2-a5,-(sp)
        addq.l  #1,DEV_OPENCALLS(a6)     ; diagnostic (DEV_OPENCALLS above)
        cmp.l   #UNIT_COUNT,d0
        bhs     .fail

        move.l  DEV_BOARDBASE(a6),a5
        move.l  d0,(HB_UNIT_SELECT)(a5)
        move.l  (HB_UNIT_PRESENT)(a5),d2
        bra     .checked
.fail:
        move.b  #IOERR_OPENFAIL,IO_ERROR(a1)
        clr.l   IO_DEVICE(a1)
        movem.l (sp)+,d2-d7/a2-a5
        rts
.checked:
        tst.l   d2
        beq     .fail

        move.l  d0,IO_UNIT(a1)           ; this driver never dereferences
                                          ; io_Unit as a struct Unit* -- a raw
                                          ; unit number is all BeginIO needs
                                          ; (file header)
        clr.b   IO_ERROR(a1)
        addq.w  #1,LIB_OPENCNT(a6)
        bclr    #3,LIB_FLAGS(a6)          ; LIBF_DELEXP
        movem.l (sp)+,d2-d7/a2-a5
        rts

*-----------------------------------------------------------------------------
* dev_close -- Close vector (LVO -12). A6=device base, A1=IORequest.
*-----------------------------------------------------------------------------
dev_close:
        subq.w  #1,LIB_OPENCNT(a6)
        clr.l   IO_DEVICE(a1)
        moveq   #0,d0
        rts

*-----------------------------------------------------------------------------
* dev_expunge -- Expunge vector (LVO -18). A6=device base. This device
* has no unload story (it is ROM-resident for the life of the machine)
* and refuses to go away: always returns a NULL seglist and never frees
* anything, the same simplification a number of ROM-resident Amiga
* devices make (documented here rather than silently, per this project's
* review conventions).
*-----------------------------------------------------------------------------
dev_expunge:
        moveq   #0,d0
        rts

*-----------------------------------------------------------------------------
* dev_extfunc -- reserved vector (LVO -24). Never called by anything in
* this OS version; present only because MakeLibrary's vector table
* format requires all four standard slots.
*-----------------------------------------------------------------------------
dev_extfunc:
        moveq   #0,d0
        rts

*-----------------------------------------------------------------------------
* dev_beginio -- BeginIO vector (LVO -30). A6=device base, A1=IORequest
* (exec/io.h's BEGINIO macro convention). Must preserve every register
* except D0/D1/A0/A1 (universal Amiga library/device call contract).
*
* Commands split three ways:
*   - synchronous (TD_GETGEOMETRY/TD_CHANGENUM/TD_CHANGESTATE/
*     TD_PROTSTATUS/NSCMD_DEVICEQUERY/CMD_RESET/CMD_CLEAR): answered from
*     the discovery registers or built in place, no doorbell round trip
*     (protocol section 4: these registers are synchronous by design).
*   - asynchronous (CMD_READ/CMD_WRITE/CMD_UPDATE/CMD_FLUSH/TD_READ64/
*     TD_WRITE64/NSCMD_TD_READ64/NSCMD_TD_WRITE64): IOF_QUICK is always
*     cleared (protocol section 7: the doorbell write never completes in
*     the same bus access) and handed to submit_or_queue, which either
*     occupies a free slot immediately or -- if every slot sized by
*     SUBMIT_CAPACITY is already busy -- queues the request until
*     int_handler frees one (this is this driver's whole answer to
*     protocol section 8's self-limiting requirement: it structurally
*     cannot over-submit, so it needs no devsoak `maxinflight` quirk).
*     Clearing IOF_QUICK on a DoIO() caller is fine: exec's DoIO then
*     waits for the ReplyMsg int_handler posts (exec 47.13's wait loop at
*     $F809D4 polls the request's ln_Type for NT_REPLYMSG). This was
*     re-verified deliberately after a debugging detour: a find_free_slot
*     register clobber (see its header) once made int_handler "reply"
*     garbage instead of the real IORequest, which mimicked a
*     DoIO-needs-IOF_QUICK problem convincingly enough that a synchronous
*     quick-completion path was drafted -- with the clobber fixed, the
*     always-asynchronous form boots Kickstart's ROM FFS to Workbench
*     unmodified, so that extra path was dropped again.
*   - anything else: IOERR_NOCMD.
*-----------------------------------------------------------------------------
dev_beginio:
        movem.l d2-d7/a2-a6,-(sp)
        move.l  a6,a3                     ; a3 = device base for the rest of
                                           ; this routine
        move.l  DEV_SYSBASE(a3),a6
        move.l  DEV_BOARDBASE(a3),a5

        move.w  IO_COMMAND(a1),d0
        cmp.w   #CMD_READ,d0
        beq     .async
        cmp.w   #CMD_WRITE,d0
        beq     .async
        cmp.w   #CMD_UPDATE,d0
        beq     .async
        cmp.w   #CMD_FLUSH,d0
        beq     .async
        cmp.w   #TD_READ64,d0
        beq     .async
        cmp.w   #TD_WRITE64,d0
        beq     .async
        cmp.w   #NSCMD_TD_READ64,d0
        beq     .async
        cmp.w   #NSCMD_TD_WRITE64,d0
        beq     .async

        cmp.w   #CMD_RESET,d0
        beq     .quick_ok
        cmp.w   #CMD_CLEAR,d0
        beq     .quick_ok
        cmp.w   #TD_GETGEOMETRY,d0
        beq     .geometry
        cmp.w   #TD_CHANGENUM,d0
        beq     .changenum
        cmp.w   #TD_CHANGESTATE,d0
        beq     .changestate
        cmp.w   #TD_PROTSTATUS,d0
        beq     .protstatus
        cmp.w   #NSCMD_DEVICEQUERY,d0
        beq     .devicequery
        bra     .nocmd

.async:
        bclr    #IOB_QUICK,IO_FLAGS(a1)   ; task spec: never complete an
        bsr     submit_or_queue           ; async command in the same call
        bra     .out                      ; (submit_or_queue itself replies
                                           ; nothing -- completion, and the
                                           ; ReplyMsg, always happens later,
                                           ; from int_handler)

.quick_ok:
        clr.b   IO_ERROR(a1)
        bra     .maybe_reply

.nocmd:
        move.b  #IOERR_NOCMD,IO_ERROR(a1)
        bra     .maybe_reply

.geometry:
        bsr     do_geometry
        bra     .maybe_reply

.changenum:
        move.l  IO_UNIT(a1),d0
        move.l  d0,(HB_UNIT_SELECT)(a5)
        move.l  (HB_UNIT_CHANGE_COUNT)(a5),d0
        move.l  d0,IO_ACTUAL(a1)
        clr.b   IO_ERROR(a1)
        bra     .maybe_reply

.changestate:
        move.l  IO_UNIT(a1),d0
        move.l  d0,(HB_UNIT_SELECT)(a5)
        move.l  (HB_UNIT_PRESENT)(a5),d0
        tst.l   d0
        beq.s   .cs_absent
        clr.l   IO_ACTUAL(a1)             ; 0 = disk present (RKRM convention)
        bra.s   .cs_done
.cs_absent:
        moveq   #1,d0
        move.l  d0,IO_ACTUAL(a1)
.cs_done:
        clr.b   IO_ERROR(a1)
        bra     .maybe_reply

.protstatus:
        move.l  IO_UNIT(a1),d0
        move.l  d0,(HB_UNIT_SELECT)(a5)
        move.l  (HB_UNIT_WRITE_PROTECT)(a5),d0
        move.l  d0,IO_ACTUAL(a1)
        clr.b   IO_ERROR(a1)
        bra     .maybe_reply

.devicequery:
        bsr     do_devicequery
        bra     .maybe_reply

.maybe_reply:
        btst    #IOB_QUICK,IO_FLAGS(a1)
        bne.s   .out
        jsr     _LVOReplyMsg(a6)
.out:
        movem.l (sp)+,d2-d7/a2-a6
        rts

*-----------------------------------------------------------------------------
* do_geometry -- TD_GETGEOMETRY, called from dev_beginio with a1=ioreq,
* a5=boardbase. devices/trackdisk.h's struct DriveGeometry has no 64-bit
* total-sectors field, so this reports the low 32 bits of sector_count()
* only -- a pre-existing limitation of this particular query, not
* something this driver works around (PFS3/SFS get the real size from
* the RDB partition environment, not this call).
*-----------------------------------------------------------------------------
do_geometry:
        move.l  IO_UNIT(a1),d0
        move.l  d0,(HB_UNIT_SELECT)(a5)
        move.l  IO_DATA(a1),a0
        move.l  #512,DG_SECTORSIZE(a0)
        move.l  (HB_UNIT_SECTORS_LO)(a5),d0
        move.l  d0,DG_TOTALSECTORS(a0)
        move.l  #1,DG_CYLINDERS(a0)       ; no real CHS geometry behind this
        move.l  d0,DG_CYLSECTORS(a0)      ; card -- one giant "cylinder"
        move.l  #1,DG_HEADS(a0)
        move.l  d0,DG_TRACKSECTORS(a0)
        move.l  #MEMF_PUBLIC,DG_BUFMEMTYPE(a0)
        move.b  #DG_DIRECT_ACCESS,DG_DEVICETYPE(a0)
        clr.b   DG_FLAGS(a0)
        clr.w   30(a0)                    ; dg_Reserved
        move.l  #DG_SIZE,IO_ACTUAL(a1)
        clr.b   IO_ERROR(a1)
        rts

*-----------------------------------------------------------------------------
* do_devicequery -- NSCMD_DEVICEQUERY (devices/newstyle.h), called from
* dev_beginio with a1=ioreq. Advertises exactly the commands this driver
* implements (SupportedCmds, static data section below) -- HD_SCSICMD is
* deliberately not in that list (not implemented this increment; see the
* file's end-of-file summary).
*-----------------------------------------------------------------------------
do_devicequery:
        move.l  IO_LENGTH(a1),d0
        cmp.l   #16,d0
        bhs.s   .ok
        move.b  #IOERR_BADLENGTH,IO_ERROR(a1)
        rts
.ok:
        move.l  IO_DATA(a1),a0
        clr.l   NSDQR_FORMAT(a0)
        move.l  #NSDQR_SIZE,NSDQR_SIZEAVAIL(a0)
        move.w  #NSDEVTYPE_TRACKDISK,NSDQR_DEVTYPE(a0)
        clr.w   NSDQR_DEVSUBTYPE(a0)
        lea     SupportedCmds(pc),a2
        move.l  a2,NSDQR_SUPPORTED(a0)
        move.l  #NSDQR_SIZE,IO_ACTUAL(a1)
        clr.b   IO_ERROR(a1)
        rts

*-----------------------------------------------------------------------------
* classify_async -- translate an IORequest's io_Command into a hostblk
* descriptor command byte (d1) and a 64-bit offset's high word (d2).
* Input: a1=ioreq. Used both when a command first arrives (dev_beginio's
* .async case) and when int_handler resubmits a request that had been
* waiting on DEV_PENDING -- in both cases io_Actual still holds whatever
* the caller put there (untouched until a completion actually lands, see
* devices/trackdisk.h's own comment on TD64/NSD's convention: "the
* higher 32 bits are ... in io_Actual", cited in protocol section 13
* too), so recomputing here instead of stashing it once is both simpler
* and always correct.
*-----------------------------------------------------------------------------
classify_async:
        move.w  IO_COMMAND(a1),d0
        moveq   #0,d2
        cmp.w   #CMD_READ,d0
        beq.s   .rd
        cmp.w   #TD_READ64,d0
        beq.s   .rd64
        cmp.w   #NSCMD_TD_READ64,d0
        beq.s   .rd64
        cmp.w   #CMD_WRITE,d0
        beq.s   .wr
        cmp.w   #TD_WRITE64,d0
        beq.s   .wr64
        cmp.w   #NSCMD_TD_WRITE64,d0
        beq.s   .wr64
        moveq   #HB_CMD_FLUSH,d1          ; CMD_UPDATE or CMD_FLUSH
        rts
.rd:
        moveq   #HB_CMD_READ,d1
        rts
.rd64:
        moveq   #HB_CMD_READ,d1
        move.l  IO_ACTUAL(a1),d2
        rts
.wr:
        moveq   #HB_CMD_WRITE,d1
        rts
.wr64:
        moveq   #HB_CMD_WRITE,d1
        move.l  IO_ACTUAL(a1),d2
        rts

*-----------------------------------------------------------------------------
* find_free_slot -- input a3=devbase. Output: a0 = a free slot's address,
* or a0=0 (and Z set) if every slot sized by DEV_CAPACITY is occupied.
* Linear scan: this driver's whole point is that concurrency is bounded
* by SUBMIT_CAPACITY (typically single digits), so an O(capacity) scan
* on every submit is not a real cost next to a host file I/O round trip.
*-----------------------------------------------------------------------------
* Scans with a0 only. It MUST NOT touch a1: every caller's a1 is the
* IORequest being submitted, and an earlier version of this routine
* walked the pool through a1 -- so by the time the caller read
* IO_UNIT(a1)/IO_LENGTH(a1)/... and stored SLOT_OWNER, a1 pointed at the
* just-found *slot*, not the request. The descriptor got garbage, the
* completion's "owner" was the slot's own address, and int_handler then
* faithfully completed and ReplyMsg'd a block of pool memory while the
* real IORequest -- and the FFS process waiting on it -- hung forever.
* That single clobber was the driver's deepest boot-blocking bug, found
* by instruction-tracing FFS's first SendIO'd read.
find_free_slot:
        move.l  DEV_SLOTBASE(a3),a0
        move.l  DEV_CAPACITY(a3),d0
        tst.l   d0
        beq.s   .none
.scan:
        tst.l   SLOT_OWNER(a0)
        beq.s   .found
        add.l   #SLOT_SIZE,a0
        subq.l  #1,d0
        bne.s   .scan
.none:
        suba.l  a0,a0
.found:
        rts

*-----------------------------------------------------------------------------
* submit_or_queue -- the self-limiting core (protocol section 8). Input:
* a1=ioreq, a3=devbase, a5=boardbase, a6=sysbase (all already established
* by the caller -- dev_beginio's .async case and int_handler's drain loop
* both set these up the same way before calling here). Disable()/Enable()
* bracket the shared-state access because int_handler (interrupt level)
* and dev_beginio (task level) both touch the slot pool, DEV_OUTSTANDING
* and DEV_PENDING -- Forbid()/Permit() would not be enough, since
* int_handler runs at interrupt time regardless of Forbid (RKRM's
* standard task-vs-interrupt-shared-state guidance).
*-----------------------------------------------------------------------------
submit_or_queue:
        bsr     classify_async            ; -> d1=desc command, d2=hi offset
                                           ; (before Disable and before
                                           ; find_free_slot, while a1 is
                                           ; untouched)
        jsr     _LVODisable(a6)

        bsr     find_free_slot            ; returns a0; preserves a1 (its
        cmpa.l  #0,a0                     ; header explains why that matters)
        beq.s   .no_slot                  ; TST An is 68020+; this ROM stays
                                           ; 68000-safe (build script's own
                                           ; -m68000 choice)

        move.b  d1,DESC_COMMAND(a0)
        move.l  IO_UNIT(a1),d0
        move.b  d0,DESC_UNIT(a0)
        clr.w   DESC_COMMAND+2(a0)        ; reserved -- must be zero
        move.l  IO_LENGTH(a1),DESC_LENGTH(a0)
        move.l  d2,DESC_OFFSET(a0)
        move.l  IO_OFFSET(a1),DESC_OFFSET+4(a0)
        move.l  IO_DATA(a1),DESC_BUFFER(a0)
        move.l  a1,SLOT_OWNER(a0)
        addq.l  #1,DEV_OUTSTANDING(a3)
        move.l  a0,(HB_DOORBELL)(a5)      ; a plain move.l submits: it writes
                                           ; the reserved lanes harmlessly and
                                           ; ends on the low-order byte, the
                                           ; byte protocol section 4 says
                                           ; actually submits (file header)
        jsr     _LVOEnable(a6)
        rts

.no_slot:
        lea     DEV_PENDING(a3),a0
        jsr     _LVOAddTail(a6)           ; a1 is still the ioreq -- exactly
                                           ; the node AddTail wants (a Node
                                           ; sits at offset 0 of Message,
                                           ; which sits at offset 0 of
                                           ; IORequest)
        jsr     _LVOEnable(a6)
        rts

*-----------------------------------------------------------------------------
* dev_abortio -- AbortIO vector (LVO -36). A6=device base, A1=IORequest.
* Only handles the case this driver can actually do something about: a
* request still sitting on DEV_PENDING (never yet given to the card).
* Once a descriptor has been handed to the doorbell, protocol section 8
* offers no cancel-in-flight mechanism, so (per RKRM's own note that
* AbortIO is a request the device "may or may not grant") this simply
* does nothing for that case -- the request completes normally, later.
*-----------------------------------------------------------------------------
dev_abortio:
        movem.l d2-d7/a2-a5,-(sp)
        move.l  a6,a3
        move.l  DEV_SYSBASE(a3),a6
        jsr     _LVODisable(a6)

        lea     DEV_PENDING(a3),a2
        move.l  LH_HEAD(a2),a0
.scan:
        tst.l   (a0)                      ; ln_Succ; the list-tail sentinel's
        beq.s   .not_found                ; own contents are always 0
                                           ; (exec/lists.h's ghost-node trick)
        cmp.l   a1,a0
        beq.s   .hit
        move.l  (a0),a0
        bra.s   .scan
.hit:
        move.l  a0,d7
        move.l  a0,a1
        jsr     _LVORemove(a6)
        move.l  d7,a1
        move.b  #IOERR_ABORTED,IO_ERROR(a1)
        jsr     _LVOReplyMsg(a6)
.not_found:
        jsr     _LVOEnable(a6)
        movem.l (sp)+,d2-d7/a2-a5
        rts

*-----------------------------------------------------------------------------
* IntHandler -- INT2 (INTB_PORTS) server (docs/hostblk-protocol.md
* section 5's own pseudocode, adapted: COMPLETION_PTR is the descriptor's
* -- i.e. this driver's own slot's -- address, never the IORequest
* itself, so SLOT_OWNER is what recovers the real IORequest). Called
* with A1 = is_Data = device base (set in RtInit above). Standard exec
* interrupt-server contract: preserve every register except D0/D1/A0/A1.
*-----------------------------------------------------------------------------
IntHandler:
        movem.l d2-d7/a2-a6,-(sp)
        move.l  a1,a3
        move.l  DEV_SYSBASE(a3),a6
        move.l  DEV_BOARDBASE(a3),a5

.drain:
        move.l  (HB_COMPLETION_PTR)(a5),d0
        beq     .out
        move.l  d0,a2                     ; a2 = completed slot address

        move.l  (HB_COMPLETION_ERROR)(a5),d1
        move.l  (HB_COMPLETION_ACTUAL)(a5),d2
        clr.l   d0
        move.l  d0,(HB_COMPLETION_ADVANCE)(a5)   ; pop, reveal next

        move.l  SLOT_OWNER(a2),a4         ; a4 = the completed IORequest
        clr.l   SLOT_OWNER(a2)
        subq.l  #1,DEV_OUTSTANDING(a3)

        bsr     hb_err_to_io_error        ; d1 -> d1
        move.b  d1,IO_ERROR(a4)
        move.l  d2,IO_ACTUAL(a4)

        ; Submit anything waiting into the slot that was just freed, before
        ; replying the request that freed it -- keeps the ring as full as
        ; demand allows (protocol section 8: "drain completions promptly").
        lea     DEV_PENDING(a3),a0
        jsr     _LVORemHead(a6)
        tst.l   d0
        beq.s   .no_pending
        move.l  d0,a1
        bsr     submit_or_queue           ; clobbers d0-d2/a0/a1 only; a2-a6
                                           ; (this routine's own state) survive
.no_pending:
        move.l  a4,a1
        jsr     _LVOReplyMsg(a6)
        bra     .drain

.out:
        ; Z-flag contract (Autodocs/exec.doc "AddIntServer": a server
        ; should return with Z clear only if the interrupt was "specifically
        ; for that server, and no one else" -- copperhf's README.md flags
        ; this as its own hard-won gotcha, and warns MOVEM never touches the
        ; CCR, so whatever set the flags last is what the OS sees). This
        ; handler always falls through to here by testing
        ; HB_COMPLETION_PTR==0 (the `beq .out` above), so Z is always SET on
        ; return, regardless of how many completions were actually drained
        ; this call -- deliberately: INTB_PORTS is a shared chain (RKRM),
        ; and this driver has no way to know it is the chain's only member,
        ; so it never claims exclusivity. The cost is every other PORTS
        ; server also gets polled on every hostblk interrupt; the
        ; alternative (falsely claiming Z-clear) risks starving a real
        ; sibling device sharing the line, which is the worse failure.
        movem.l (sp)+,d2-d7/a2-a6
        rts

*-----------------------------------------------------------------------------
* hb_err_to_io_error -- protocol section 6's completion error code (d1,
* 0-8) to a standard AmigaOS io_Error byte (exec/errors.h,
* devices/trackdisk.h), via ErrTable in the static data section below.
*-----------------------------------------------------------------------------
hb_err_to_io_error:
        lea     ErrTable(pc),a0
        move.b  (a0,d1.w),d1
        rts

*=============================================================================
* RDB partition mounter (Autodocs/expansion.doc "MakeDosNode"/
* "AddBootNode"; devices/hardblocks.h's RigidDiskBlock/PartitionBlock).
* Called once from RtInit, before AddIntServer -- see IntHandler's header
* and this file's own header for why that ordering matters. Every read
* here is a synchronous doorbell-write-then-poll round trip (no MsgPort,
* no interrupts yet, exactly one request ever outstanding), the same
* shape copperhf's mounter.c uses for the same reason.
*=============================================================================

* mount_all's stack-frame locals (link a4,#-MA_FRAMESIZE). Using the
* stack rather than AllocMem for these means there is no allocation-
* failure path to handle during boot-time mounting at all.
MA_DESC         equ     -DESC_SIZE                     ; -20
MA_SECBUF       equ     MA_DESC-512                    ; -532
MA_NAMEBUF      equ     MA_SECBUF-32                    ; -564 (pb_DriveName,
                                                         ; BSTR max 31 chars,
                                                         ; converted to a
                                                         ; NUL-terminated
                                                         ; C string here)
MA_PARMPKT      equ     MA_NAMEBUF-(4*4+20*4)           ; -660 (MakeDosNode's
                                                         ; parameter packet:
                                                         ; PP_DOSNAME..PP_FLAGS
                                                         ; then up to 20
                                                         ; environment
                                                         ; longwords)
MA_FRAMESIZE    equ     -MA_PARMPKT                     ; 660

*-----------------------------------------------------------------------------
* mount_all -- input a3=devbase. Walks all 8 units; for each present
* unit, scans blocks 0..RDB_LOCATION_LIMIT-1 for an 'RDSK' signature
* (devices/hardblocks.h: "must exist ... within the first
* RDB_LOCATION_LIMIT blocks"), then walks rdb_PartitionList, mounting
* every partition that doesn't carry PBFF_NOMOUNT.
*-----------------------------------------------------------------------------
mount_all:
        link    a4,#-MA_FRAMESIZE
        move.l  DEV_SYSBASE(a3),a6
        move.l  DEV_BOARDBASE(a3),a5

        moveq   #0,d7                     ; d7 = unit number, 0..7
.unit_loop:
        cmp.b   #UNIT_COUNT,d7
        beq     .done

        move.l  d7,d0
        move.l  d0,(HB_UNIT_SELECT)(a5)
        move.l  (HB_UNIT_PRESENT)(a5),d0
        tst.l   d0
        beq     .next_unit

        moveq   #0,d6                     ; d6 = candidate RDSK block number
.rdsk_scan:
        cmp.l   #RDB_LOCATION_LIMIT,d6
        bge     .next_unit                ; no RDB found on this unit
        move.l  d6,d1
        bsr     read_block                ; unit=d7 (via DESC_UNIT below),
                                           ; block=d1 -> MA_SECBUF(a4); d0=0
                                           ; on success
        tst.l   d0
        bne     .next_unit                ; a read error here means give up
                                           ; on this unit rather than spin
        lea     MA_SECBUF(a4),a2
        cmp.l   #IDNAME_RIGIDDISK,RDB_ID(a2)
        beq.s   .found_rdsk
        addq.l  #1,d6
        bra     .rdsk_scan

.found_rdsk:
        move.l  RDB_PARTITIONLIST(a2),d5  ; d5 = next partition block
                                           ; ($FFFFFFFF terminates the chain)
.part_loop:
        move.l  d5,d0
        not.l   d0
        beq     .next_unit
        move.l  d5,d1
        bsr     read_block
        tst.l   d0
        bne     .next_unit
        lea     MA_SECBUF(a4),a2
        cmp.l   #IDNAME_PARTITION,PB_ID(a2)
        bne     .next_unit                ; corrupt chain -- stop here
        move.l  PB_NEXT(a2),d5            ; capture before mount_partition
                                           ; may reuse this buffer's fields
        move.l  PB_FLAGS(a2),d0
        btst    #1,d0                     ; PBFF_NOMOUNT
        bne     .part_loop
        bsr     mount_partition           ; a2=partition block, a3=devbase,
                                           ; a4=frame, a5=boardbase,
                                           ; a6=sysbase, d7=unit
        bra     .part_loop

.next_unit:
        addq.b  #1,d7
        bra     .unit_loop
.done:
        unlk    a4
        rts

*-----------------------------------------------------------------------------
* read_block -- input a3=devbase (unused directly but kept live by
* caller), a5=boardbase, d7=unit, d1=block number (blocks this mounter
* reads are always small RDB-area numbers, so a plain 32-bit
* block*512 -- computed with lsl.l #9 -- never overflows). Reads 512
* bytes into MA_SECBUF(a4). Output: d0=0 on success, non-zero (the
* hostblk completion error byte) otherwise.
*-----------------------------------------------------------------------------
read_block:
        lea     MA_DESC(a4),a0
        move.b  #HB_CMD_READ,DESC_COMMAND(a0)
        move.b  d7,DESC_UNIT(a0)
        clr.w   DESC_COMMAND+2(a0)
        move.l  #512,DESC_LENGTH(a0)
        clr.l   DESC_OFFSET(a0)
        lsl.l   #8,d1                     ; #9 in one shot needs a 68020+
        lsl.l   #1,d1                     ; register-count form; 8+1=9 stays
                                           ; 68000-safe (immediate shifts are
                                           ; limited to 1..8 on this CPU)
        move.l  d1,DESC_OFFSET+4(a0)
        lea     MA_SECBUF(a4),a1
        move.l  a1,DESC_BUFFER(a0)

        move.l  a0,(HB_DOORBELL)(a5)
.poll:
        move.l  (HB_COMPLETION_PTR)(a5),d0
        beq.s   .poll
        move.l  (HB_COMPLETION_ERROR)(a5),d1
        clr.l   d0
        move.l  d0,(HB_COMPLETION_ADVANCE)(a5)
        move.l  d1,d0
        rts

*-----------------------------------------------------------------------------
* mount_partition -- input a2=partition block (MA_SECBUF(a4)), a3=devbase,
* a4=mount_all's frame, a5=boardbase, a6=sysbase, d7=unit. Preserves
* d5/d7/a2/a3/a4/a5 (mount_all's own loop state); everything else is
* scratch. Builds MakeDosNode's parameter packet directly from
* pb_Environment (hardblocks.h's own documented convention: it *is* a
* DosEnvec-shaped array, one-for-one), then AddBootNode's it.
*-----------------------------------------------------------------------------
mount_partition:
        ; pb_DriveName (BSTR: length byte then chars, not NUL-terminated)
        ; -> a NUL-terminated C string MakeDosNode's parmPkt[0] wants
        ; (Autodocs/expansion.doc's EXAMPLE passes a plain C string).
        moveq   #0,d0
        move.b  PB_DRIVENAME(a2),d0
        cmp.l   #31,d0
        bls.s   .len_ok
        moveq   #31,d0                    ; defend against a corrupt RDB;
                                           ; MA_NAMEBUF is exactly 32 bytes
.len_ok:
        lea     PB_DRIVENAME+1(a2),a0
        lea     MA_NAMEBUF(a4),a1
        move.l  d0,d1
        beq.s   .synth_name           ; empty pb_DriveName: this project's
                                       ; own test RDB has exactly this shape
                                       ; (confirmed against the real image),
                                       ; and a nameless DOS device node is
                                       ; not obviously something DOS's own
                                       ; boot-node validation accepts --
                                       ; synthesize one rather than pass an
                                       ; empty string through unexamined.
.name_copy:
        move.b  (a0)+,(a1)+
        subq.l  #1,d1
        bne.s   .name_copy
        bra.s   .name_done
.synth_name:
        move.b  #'H',(a1)+
        move.b  #'B',(a1)+
        move.b  d7,d0
        add.b   #'0',d0
        move.b  d0,(a1)+
.name_done:
        clr.b   (a1)

        lea     PB_ENVIRONMENT(a2),a0
        move.l  (a0),d0                   ; de_TableSize
        cmp.l   #19,d0                    ; pb_Environment holds 20 longwords
        bls.s   .n_ok                     ; (hardblocks.h) -- clamp against a
        moveq   #19,d0                    ; corrupt RDB overrunning MA_PARMPKT
.n_ok:
        lea     MA_PARMPKT+PP_ENV(a4),a1
        move.l  d0,d1
        addq.l  #1,d1                     ; copy indices 0..N (N+1 longwords)
.env_copy:
        move.l  (a0)+,(a1)+
        subq.l  #1,d1
        bne.s   .env_copy

        lea     MA_NAMEBUF(a4),a0
        move.l  a0,MA_PARMPKT+PP_DOSNAME(a4)
        lea     DevName(pc),a0
        move.l  a0,MA_PARMPKT+PP_DEVNAME(a4)
        move.l  d7,MA_PARMPKT+PP_UNIT(a4)
        clr.l   MA_PARMPKT+PP_FLAGS(a4)

        move.l  a6,-(sp)                  ; save SysBase
        move.l  DEV_EXPBASE(a3),a6
        lea     MA_PARMPKT(a4),a0
        jsr     _LVOMakeDosNode(a6)
        move.l  d0,d6                     ; d6 = DeviceNode* (free to reuse:
                                           ; mount_all's own d6 use, the RDSK
                                           ; scan counter, is long finished
                                           ; by the time mount_partition runs)
        move.l  (sp)+,a6
        tst.l   d6
        beq.s   .out                      ; MakeDosNode OOM -- skip this
                                           ; partition, keep walking the chain

        move.l  a6,-(sp)
        move.l  DEV_EXPBASE(a3),a6
        ; Flags and ConfigDev depend on the partition's own PBFF_BOOTABLE
        ; (hardblocks.h pb_Flags bit 0), exactly as expansion.doc's
        ; AddBootNode INPUTS describe and copperhf's mounter.c (M6 comment)
        ; independently arrived at: a *bootable* partition needs
        ; ADNF_STARTPROC -- strap's boot-time device scan only starts (and
        ; therefore only boots from) handlers it was told to start; without
        ; it the node just sits unreferenced and the strap falls through to
        ; the insert-disk screen (this was the bug: hostblk's node was
        ; complete and correct but added with flags=0, so DOS never opened
        ; hostblk.device at all) -- while a non-bootable one should get
        ; neither the flag nor a ConfigDev ("Pass a NULL ConfigDev pointer
        ; to create a non-bootable node").
        move.l  PB_FLAGS(a2),d0
        btst    #PBFB_BOOTABLE,d0
        beq.s   .not_bootable
        moveq   #1,d1                     ; ADNF_STARTPROC (expansion.h)
        move.l  DEV_CONFIGDEV(a3),a1
        bra.s   .add
.not_bootable:
        moveq   #0,d1
        suba.l  a1,a1                     ; NULL ConfigDev: non-bootable
.add:
        move.b  PB_ENVIRONMENT+(DE_BOOTPRI*4)+3(a2),d0
        ext.w   d0
        ext.l   d0                        ; sign-extend the BYTE bootPri
                                           ; AddBootNode's D0 input wants
        move.l  d6,a0                     ; deviceNode
        jsr     _LVOAddBootNode(a6)
        move.l  (sp)+,a6
.out:
        rts

*=============================================================================
* Static data -- lives permanently in the board's own ROM window (this
* file header explains why that needs no patching). Placed after all
* code so no routine above accidentally falls through into it.
*=============================================================================

DevName:
        dc.b    "hostblk.device",0
        even

DevIdString:
        dc.b    "hostblk.device 1.0 (2026)",0
        even

ExpName:
        dc.b    "expansion.library",0
        even

* devices/newstyle.h: struct NSDeviceQueryResult's nsdqr_SupportedCommands
* -- a 0-terminated UWORD list of every command this driver answers.
* HD_SCSICMD is deliberately absent (not implemented -- file's end-of-
* file summary).
SupportedCmds:
        dc.w    CMD_READ,CMD_WRITE,CMD_UPDATE,CMD_FLUSH,CMD_CLEAR,CMD_RESET
        dc.w    TD_GETGEOMETRY,TD_CHANGENUM,TD_CHANGESTATE,TD_PROTSTATUS
        dc.w    TD_READ64,TD_WRITE64,NSCMD_TD_READ64,NSCMD_TD_WRITE64
        dc.w    NSCMD_DEVICEQUERY
        dc.w    0
        even

* docs/hostblk-protocol.md section 6 order (index = hostblk error code);
* exec/errors.h and devices/trackdisk.h values (this file's equ block).
ErrTable:
        dc.b    0                          ; OK
        dc.b    TDERR_BadUnitNum            ; BAD_UNIT
        dc.b    TDERR_WriteProt             ; WRITE_PROTECTED
        dc.b    IOERR_NOCMD                 ; INVALID_COMMAND
        dc.b    IOERR_BADLENGTH             ; INVALID_LENGTH
        dc.b    IOERR_BADADDRESS            ; MISALIGNED
        dc.b    TDERR_SeekError             ; OUT_OF_RANGE
        dc.b    IOERR_BADADDRESS            ; BAD_ADDRESS
        dc.b    TDERR_NotSpecified          ; IO_ERROR
        even

* docs/hostblk-protocol.md section 4: UNIT_COUNT is 8 (also
* crates/machine-core/src/hostblk.rs's own `UNIT_COUNT`).
UNIT_COUNT      equ     8
