/*
 * sana2.h -- SANA-II network device API constants and structures, for
 * virtionet.device (m68k/virtionet-device/) and VNetTest (m68k/vnettest/).
 *
 * PROVENANCE: this project's local NDK 3.2 reference bundle does not
 * carry a devices/sana2.h (SANA-II is specified by Commodore's "Amiga
 * Guide to Network Programming", a document distinct from the RKRM/
 * Autodocs set this project has locally). The values and layouts below
 * were verified against AROS's compiler/include/devices/sana2.h
 * (https://github.com/aros-development-team/AROS, APL) used strictly as
 * a VALUE ORACLE for the published SANA-II standard's own constants and
 * structure layouts -- facts about a public interface, not AROS's
 * expression of them. No text or comments from that file are copied or
 * translated here; this header is written fresh, the same licensing
 * posture docs/pci-library.md section 1 documents for the Matay
 * Prometheus SDK. Unlike an earlier draft of this file, these values are
 * now claimed correct for genuine third-party SANA-II interop at the ABI
 * level (command codes, struct layouts, error codes, tag values, flag
 * bits) -- what remains project-specific is only virtionet.device's own
 * functional scope-down (see its own file-top comment), never this
 * header's numbers.
 *
 * A second, higher-trust source joined later: ~/src/sana2loop (Simon's
 * own loopback.device, BSD 2-Clause, freely copyable -- unlike the
 * AROS/Prometheus SDK licensing firewall, adaptation with attribution is
 * the standing permission here). Its docs/sana2-notes.md and
 * src/loopback_device.c are cited by name at each point this header or
 * virtionet_device.c/vnettest.c adapts something from them (the copy-hook
 * register ABI, the current-vs-factory station-address split, the
 * ios2_StatData vs. ios2_Data distinction, address-array zero-fill).
 */

#ifndef DEVICES_SANA2_H
#define DEVICES_SANA2_H

#include <exec/types.h>
#include <exec/io.h>
#include <exec/errors.h>
#include <utility/tagitem.h>
#include <devices/timer.h> /* struct timeval, for struct Sana2DeviceStats'
                            * LastStart field */

#define SANA2_VERSION 2

/* ---- NONSTD command codes (CMD_NONSTD == 9, exec/io.h). Note the gap
 * at +3/+4 (no command there) and the two commands with fixed hex
 * values entirely outside the CMD_NONSTD numbering. ------------------- */

#define S2_DEVICEQUERY           (CMD_NONSTD+0)
#define S2_GETSTATIONADDRESS     (CMD_NONSTD+1)
#define S2_CONFIGINTERFACE       (CMD_NONSTD+2)
#define S2_ADDMULTICASTADDRESS   (CMD_NONSTD+5)
#define S2_DELMULTICASTADDRESS   (CMD_NONSTD+6)
#define S2_MULTICAST             (CMD_NONSTD+7)
#define S2_BROADCAST             (CMD_NONSTD+8)
#define S2_TRACKTYPE             (CMD_NONSTD+9)
#define S2_UNTRACKTYPE           (CMD_NONSTD+10)
#define S2_GETTYPESTATS          (CMD_NONSTD+11)
#define S2_GETSPECIALSTATS       (CMD_NONSTD+12)
#define S2_GETGLOBALSTATS        (CMD_NONSTD+13)
#define S2_ONEVENT               (CMD_NONSTD+14)
#define S2_READORPHAN            (CMD_NONSTD+15)
#define S2_ONLINE                (CMD_NONSTD+16)
#define S2_OFFLINE               (CMD_NONSTD+17)
#define S2_ADDMULTICASTADDRESSES 0xC000
#define S2_DELMULTICASTADDRESSES 0xC001

/* ---- ios2_Flags (SANA2IOB_ / SANA2IOF_) -- these are HIGH bits of
 * io_Flags, not 0/1. ----------------------------------------------------- */

#define SANA2IOB_RAW   7
#define SANA2IOB_BCAST 6
#define SANA2IOB_MCAST 5
#define SANA2IOB_CRC   4

#define SANA2IOF_RAW   (1 << SANA2IOB_RAW)
#define SANA2IOF_BCAST (1 << SANA2IOB_BCAST)
#define SANA2IOF_MCAST (1 << SANA2IOB_MCAST)
#define SANA2IOF_CRC   (1 << SANA2IOB_CRC)

/* ---- OpenDevice() flags (SANA2OPB_ / SANA2OPF_). This driver does not
 * act on either (no promiscuous-mode support, single-owner-per-station
 * scope already) -- defined here for ABI completeness only. */

#define SANA2OPB_MINE 0
#define SANA2OPB_PROM 1

#define SANA2OPF_MINE (1 << SANA2OPB_MINE)
#define SANA2OPF_PROM (1 << SANA2OPB_PROM)

/* ---- struct IOSana2Req -------------------------------------------------
 * Base is plain struct IORequest (NOT IOStdReq) -- there is no io_Actual
 * on this ABI at all: ios2_DataLength serves BOTH roles (input: max
 * buffer size on CMD_READ / bytes to send on CMD_WRITE; output on
 * CMD_READ completion: actual bytes delivered, overwriting the input
 * value). ios2_SrcAddr/ios2_DstAddr are INLINE fixed-size arrays, not
 * pointers -- always valid storage on the request itself, never NULL. */

#define SANA2_MAX_ADDR_BITS  128
#define SANA2_MAX_ADDR_BYTES (SANA2_MAX_ADDR_BITS / 8)

struct IOSana2Req {
    struct IORequest ios2_Req;
    ULONG   ios2_WireError;
    ULONG   ios2_PacketType;
    UBYTE   ios2_SrcAddr[SANA2_MAX_ADDR_BYTES];
    UBYTE   ios2_DstAddr[SANA2_MAX_ADDR_BYTES];
    ULONG   ios2_DataLength;
    VOID   *ios2_Data;
    VOID   *ios2_StatData;
    VOID   *ios2_BufferManagement;
};

/* ---- struct Sana2DeviceQuery (S2_DEVICEQUERY) --------------------------
 * SizeAvailable: (in) bytes of this struct the CALLER has room for --
 * lets the interface grow over time without breaking old callers/
 * drivers. SizeSupplied: (out) bytes the driver actually filled in
 * (min(sizeof(struct Sana2DeviceQuery) as this driver knows it,
 * SizeAvailable)). AddrFieldSize is a plain UWORD VALUE (bits per
 * hardware address -- 48 for Ethernet), not a pointer. */

struct Sana2DeviceQuery {
    ULONG   SizeAvailable;
    ULONG   SizeSupplied;
    ULONG   DevQueryFormat;
    ULONG   DeviceLevel;
    UWORD   AddrFieldSize;
    ULONG   MTU;
    ULONG   BPS;
    ULONG   HardwareType;
};

#define S2WireType_Ethernet 1

/* ---- struct Sana2DeviceStats (S2_GETGLOBALSTATS, via ios2_StatData --
 * NOT ios2_Data/ios2_DataLength). S2_GETTYPESTATS/S2_GETSPECIALSTATS use
 * their own distinct StatData struct shapes (Sana2PacketTypeStats /
 * Sana2SpecialStatRecord+Header) that this driver does not implement
 * (S2ERR_NOT_SUPPORTED, task brief scope) and so are not reproduced
 * here. */

struct Sana2DeviceStats {
    ULONG PacketsReceived;
    ULONG PacketsSent;
    ULONG BadData;
    ULONG Overruns;
    ULONG Unused;
    ULONG UnknownTypesReceived;
    ULONG Reconfigurations;
    struct timeval LastStart;
};

/* ---- S2ERR_* (io_Error for NONSTD commands) -- note the gaps: no value
 * 2 or 7. ----------------------------------------------------------------*/

#define S2ERR_NO_ERROR      0
#define S2ERR_NO_RESOURCES  1
#define S2ERR_BAD_ARGUMENT  3
#define S2ERR_BAD_STATE     4
#define S2ERR_BAD_ADDRESS   5
#define S2ERR_MTU_EXCEEDED  6
#define S2ERR_NOT_SUPPORTED 8
#define S2ERR_SOFTWARE      9
#define S2ERR_OUTOFSERVICE  10
#define S2ERR_TX_FAILURE    11

/* ---- S2WERR_* (ios2_WireError) -- note there is no value 14. ---------- */

#define S2WERR_GENERIC_ERROR    0
#define S2WERR_NOT_CONFIGURED   1
#define S2WERR_UNIT_ONLINE      2
#define S2WERR_UNIT_OFFLINE     3
#define S2WERR_ALREADY_TRACKED  4
#define S2WERR_NOT_TRACKED      5
#define S2WERR_BUFF_ERROR       6
#define S2WERR_SRC_ADDRESS      7
#define S2WERR_DST_ADDRESS      8
#define S2WERR_BAD_BROADCAST    9
#define S2WERR_BAD_MULTICAST    10
#define S2WERR_MULTICAST_FULL   11
#define S2WERR_BAD_EVENT        12
#define S2WERR_BAD_STATDATA     13
#define S2WERR_IS_CONFIGURED    15
#define S2WERR_NULL_POINTER     16
#define S2WERR_TOO_MANY_RETRIES 17
#define S2WERR_RCVRBLE_HDW_ERR  18

/* ---- S2EVENT_* (S2_ONEVENT ios2_WireError-style event mask). Not acted
 * on by this driver (S2_ONEVENT -> S2ERR_NOT_SUPPORTED, task brief
 * scope) -- defined here for ABI completeness only. */

#define S2EVENT_ERROR      (1 << 0)
#define S2EVENT_TX         (1 << 1)
#define S2EVENT_RX         (1 << 2)
#define S2EVENT_ONLINE     (1 << 3)
#define S2EVENT_OFFLINE    (1 << 4)
#define S2EVENT_BUFF       (1 << 5)
#define S2EVENT_HARDWARE   (1 << 6)
#define S2EVENT_SOFTWARE   (1 << 7)
#define S2EVENT_CONNECT    (1 << 9)
#define S2EVENT_DISCONNECT (1 << 10)

/* ---- buffer-management tags (ios2_BufferManagement, OpenDevice time) -- */

#define S2_Dummy             (TAG_USER + 0xB0000)
#define S2_CopyToBuff        (S2_Dummy+1)
#define S2_CopyFromBuff      (S2_Dummy+2)
#define S2_PacketFilter      (S2_Dummy+3)
#define S2_CopyToBuff16      (S2_Dummy+4)
#define S2_CopyFromBuff16    (S2_Dummy+5)
#define S2_CopyToBuff32      (S2_Dummy+6)
#define S2_CopyFromBuff32    (S2_Dummy+7)
#define S2_DMACopyToBuff32   (S2_Dummy+8)
#define S2_DMACopyFromBuff32 (S2_Dummy+9)

/* This project's own alias, matching the task brief's wording
 * ("SANA2_CopyToBuff/SANA2_CopyFromBuff") -- same numeric tags as the
 * real S2_CopyToBuff/S2_CopyFromBuff above, not a separate namespace. */
#define SANA2_CopyToBuff   S2_CopyToBuff
#define SANA2_CopyFromBuff S2_CopyFromBuff

/* Copy-hook ABI is SPEC-FIXED REGISTER convention, not this driver's
 * choice: BOOL Copy*Buff(APTR to [a0], APTR from [a1], ULONG size [d0]),
 * returning TRUE/FALSE. Verified against ~/src/sana2loop (Simon's own
 * loopback.device, BSD 2-Clause -- docs/sana2-notes.md's "Buffer
 * management hook" section point 4, src/loopback_device.c's
 * `Sana2CopyFunc` typedef): bebbo's gcc honors asm-register annotations
 * on a function-POINTER type at the call site (not just on a function
 * definition), which is exactly what a hook stored via a TagItem and
 * invoked through a plain C function-pointer variable needs -- getting
 * this wrong would corrupt memory silently instead of faulting cleanly.
 * `__asm` (not the reference's bare `asm`) to stay compilable under this
 * project's own `-std=c99` build flags, matching every other
 * register-pinned parameter in this project's m68k sources.
 * CopyFromBuff copies FROM the caller's ios2_Data buffer TO the driver's
 * internal buffer (CMD_WRITE); CopyToBuff copies FROM the driver's
 * internal buffer TO the caller's ios2_Data buffer (CMD_READ
 * completion). */
typedef BOOL (*SANA2_COPY_FUNC)(APTR to __asm("a0"), APTR from __asm("a1"),
                                ULONG size __asm("d0"));

#endif /* DEVICES_SANA2_H */
