*-----------------------------------------------------------------------------
* hostblk-diagrom.s -- the hostblk card's DiagArea boot ROM.
*
* MIT License. Copyright (c) 2026 the m68k Machine project. See ../LICENSE.
*
* Scope: prove Kickstart will run code straight from this board's own
* AUTOCONFIG DiagArea, and that the code can reach the board's own
* registers -- nothing more. It does not implement the hostblk wire
* protocol (docs/hostblk-protocol.md) or an RDB mounter; that is the next
* increment, once this mechanism is confirmed working. `BootEntry` below is
* therefore a placeholder RTS -- see its comment.
*
* Assembled with vasm (Motorola syntax) to a flat binary; see
* ../../scripts/build-hostblk-rom.sh. The whole thing is position-
* independent: every offset a real board would need patched at DiagPoint
* time (RKRM 3rd ed. "Expansion Library", "Events At DIAG Time") is instead
* coded as an assemble-time label difference (`EndCopy-DiagStart` etc), and
* the one runtime pointer this code touches (the VERSION register) is
* reached via the A0 board-base register the OS hands to DiagPoint, not
* via any address baked into the ROM. So there is nothing here that needs
* a runtime patch table at all -- see DiagEntry's comment for why that
* isn't a shortcut, just this increment having no absolute pointers yet
* (no romtag, no boot code, no strings).
*
* Struct layout and register-convention sources, cited at each site below:
*   - libraries/configregs.h (NDK 3.2): struct DiagArea, the da_Config bit
*     definitions, and the DiagPoint calling-convention comment.
*   - Amiga ROM Kernel Reference Manual, 3rd ed. (1991), "Expansion
*     Library" chapter, "Events At DIAG Time": the RAM-copy precondition on
*     da_BootPoint, and the worked DiagArea/DiagEntry example this file's
*     shape follows (independently reproduced, not copied -- this card's
*     DiagEntry has no patch table to run at all, see above).
*   - ~/src/external/Copperline's guest/services/entry.s and guest/
*     copperhf/entry.s (GPL-3, read for understanding only, nothing copied
*     here): two independent, hardware-proven AUTOCONFIG DiagArea ROMs
*     that record a gotcha neither of the two sources above states --
*     "da_Config needs a DAC_BOOTTIME bit or the area is abandoned after
*     one read". A first version of this file used DAC_NEVER (0) and hit
*     exactly that against the real Kickstart 3.2.2 ROM in
*     nondistribution/: the DiagArea's bytes read back correctly (proving
*     this board's own ROM window worked), a ConfigDev was created with
*     DIAGVALID and the right er_InitDiagVec, and expansion.library still
*     never copied the area into RAM at all -- confirmed by a chip-RAM-wide
*     signature scan for the DiagArea's own header bytes, not just an
*     absent er_Reserved0c pointer. DAC_CONFIGTIME below is what fixed it.
*-----------------------------------------------------------------------------

* ---- hostblk register offsets (docs/hostblk-protocol.md, section 4;
* crates/machine-core/src/hostblk.rs's `reg` module) ------------------------
HB_VERSION      equ     $3c     ; u32, protocol version; reg::VERSION

* ---- libraries/configregs.h: struct DiagArea's da_Config bit layout ------
* da_Config's two independent bitfields (configregs.h: DAC_BUSWIDTH mask
* $C0, DAC_BOOTTIME mask $30).
DAC_WORDWIDE    equ     $80     ; 16 bits per access when expansion.library
                                ; copies this area into RAM. This board is a
                                ; plain byte-addressable memory window
                                ; behind the host bus, not a narrow physical
                                ; ROM chip, so there is no nibble- or
                                ; byte-packing to undo -- word-wide is the
                                ; direct, unpacked case.
DAC_CONFIGTIME  equ     $10     ; "call da_BootPoint when first configing
                                ; the device" (configregs.h). Nothing in
                                ; this increment wants da_BootPoint auto-run
                                ; at all -- there is no romtag for ROMTAG-
                                ; INIT-time processing to find, and no
                                ; BootNode for boot-time processing to
                                ; select (see BootEntry below), so
                                ; BootEntry is a placeholder RTS regardless
                                ; of this flag. The flag is set anyway
                                ; because DAC_BOOTTIME being *some* real
                                ; bit, not DAC_NEVER (0), turns out to be
                                ; required for expansion.library to copy
                                ; the DiagArea into RAM at all -- confirmed
                                ; against real Kickstart 3.2.2 the hard way
                                ; (see the file header). da_BootPoint's
                                ; offset must also be non-NULL for the same
                                ; reason (RKRM "Events At DIAG Time"), which
                                ; BootEntry already gives it.

*-----------------------------------------------------------------------------
* struct DiagArea (configregs.h): UBYTE da_Config, UBYTE da_Flags,
* UWORD da_Size, UWORD da_DiagPoint, UWORD da_BootPoint, UWORD da_Name,
* UWORD da_Reserved01, UWORD da_Reserved02 -- 1+1+2+2+2+2+2+2 = 14 bytes.
* da_DiagPoint/da_BootPoint/da_Name are word offsets from DiagStart itself
* (the fields' own doc comments: "where to start for diagnostics", "for
* booting", relative to the structure), computed below as label
* differences so this file needs no hand counting.
*-----------------------------------------------------------------------------
DiagStart:
        dc.b    DAC_WORDWIDE+DAC_CONFIGTIME  ; da_Config
        dc.b    0                       ; da_Flags -- none defined, must be 0
        dc.w    EndCopy-DiagStart       ; da_Size: bytes copied into RAM
        dc.w    DiagEntry-DiagStart     ; da_DiagPoint
        dc.w    BootEntry-DiagStart     ; da_BootPoint (see DAC_CONFIGTIME above)
        dc.w    0                       ; da_Name: no identifier string yet
        dc.w    0                       ; da_Reserved01 -- configregs.h: "must be zero"
        dc.w    0                       ; da_Reserved02 -- configregs.h: "must be zero"

* Not part of struct DiagArea -- RKRM's worked example calls this kind of
* extra room "OtherData": additional bytes inside da_Size that DiagEntry is
* free to use, the same way a real driver would place a pre-built BootNode
* or MakeDosNode packet here for later patching. This card has neither yet,
* so the one cell here is pure diagnostic scratch: DiagEntry below writes
* the VERSION register's value into it, so a host-side inspector can read
* it back out of the RAM copy (found via the ConfigDev this board's own
* er_Reserved0c-0f end up pointing at) as concrete evidence DiagEntry ran
* and reached the board's own registers -- not just that it returned
* success.
DiagMarker:
        dc.l    0

*-----------------------------------------------------------------------------
* DiagEntry -- da_DiagPoint. Calling convention (configregs.h, the comment
* directly above struct DiagArea's calling-convention block; reproduced
* here verbatim rather than only in the file header, since this is the
* code it governs):
*   A7 -- points to at least 2K of stack
*   A6 -- ExecBase
*   A5 -- ExpansionBase
*   A3 -- this board's ConfigDev structure
*   A2 -- base of the diag/init area that was just copied into RAM
*   A0 -- base of this board (i.e. the same address hostblk's registers
*         answer at -- exactly what this routine needs)
*   Return: D0 non-zero for success. Zero causes expansion.library to
*   free the RAM copy and NULL the address it had stashed in the board's
*   ConfigDev copy at er_Reserved0c-0f.
*
* Only A0 and A2 are used. Nothing here holds an absolute pointer that
* needs runtime patching (no romtag, no strings, no pre-built structures),
* so there is no patch-table walk to do -- contrast the RKRM worked
* example, whose DiagEntry exists mainly to run one, for a romtag it does
* have.
*-----------------------------------------------------------------------------
DiagEntry:
        move.l  HB_VERSION(a0),d0      ; read hostblk's own VERSION register
        move.l  d0,(DiagMarker-DiagStart)(a2)
        moveq   #1,d0                  ; non-zero: keep the RAM copy
        rts

*-----------------------------------------------------------------------------
* BootEntry -- da_BootPoint. Placeholder only, per DAC_CONFIGTIME's comment
* above: this board offers no BootNode in this increment (docs/hostblk-
* protocol.md section 12; ADR 0003's "consequences for the driver"), so
* nothing in expansion.library's boot-strap path can select it, and this
* is never actually called. Its offset still has to be non-zero, which the
* RAM copy's very existence depends on (see above). Will become the RDB
* mounter's entry point.
*
* This board also has no Resident/Romtag structure in its copied area (no
* driver to init yet either), so "Events At ROMTAG INIT Time"'s later
* search for one -- gated on this same DAC_CONFIGTIME bit -- finds nothing
* and does nothing further; harmless, and exactly what this increment
* wants.
*-----------------------------------------------------------------------------
BootEntry:
        rts

EndCopy:
