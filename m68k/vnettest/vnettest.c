/*
 * vnettest.c -- VNetTest: the guest-side test tool for ADR 0005 stage 3
 * (virtionet.device). A real AmigaOS CLI program (standard libnix
 * startup, -noixemul, no -nostartfiles/-nostdlib -- installed and run
 * from a shell/startup-sequence like any other command, mirroring
 * m68k/pciprobe/pciprobe.c's own shape and narration style).
 *
 * Exercises only the PUBLIC SANA-II device API a real network stack
 * would use: OpenDevice with buffer-management callbacks,
 * S2_GETSTATIONADDRESS, S2_CONFIGINTERFACE, S2_ONLINE, one CMD_WRITE of a
 * deterministic recognisable frame, then one posted CMD_READ, narrated
 * over serial. This cannot prove end-to-end reception yet (this stage's
 * own definition of done, task brief) -- there is no external peer on
 * this virtual wire to echo the frame back -- so the CMD_READ step
 * narrates whatever it gets (a real frame if one arrives, or a timeout)
 * without treating a timeout as a hard FAIL; every other step is
 * asserted normally.
 *
 * Every marker line starts "VNETTEST ", one line per check carrying
 * PASS/FAIL and concrete values (this platform's rule: absence of
 * complaint is never success), single final verdict line:
 * "VNETTEST result: ALL PASS" or "VNETTEST result: FAIL". See this
 * file's own end for the complete marker vocabulary.
 */

#include <exec/types.h>
#include <exec/errors.h>
#include <exec/memory.h>
#include <exec/nodes.h> /* NT_MESSAGE, for the CMD_READ ln_Type reset below */
#include <dos/dos.h>
#include <proto/exec.h>
#include <proto/dos.h>
#include <clib/debug_protos.h>
#include <clib/alib_protos.h> /* CreateExtIO/DeleteExtIO -- amiga.lib
                               * helpers, not real exec.library LVOs */

#include "sana2.h"

#define VNETTEST_UNIT 0

#define VNET_ETH_HDR_LEN 14
#define VNET_TX_ETHERTYPE 0x88B5U

#define TX_PAYLOAD_PREFIX "M68KVNET-TX-0001"
#define TX_COUNTER_DIGITS 4

/* Bounded, generous busy-wait for the posted CMD_READ (see file-top
 * comment: no external peer exists to guarantee a reply arrives on this
 * virtual wire yet, so this is a best-effort observation window, not a
 * hard requirement). */
#define READ_POLL_LIMIT 2000000UL

struct Device *VNetDeviceBase; /* not used via LVOs -- SANA-II is entirely
                                * IORequest-based, this is kept only for
                                * symmetry/narration, never dereferenced. */

static LONG g_all_pass = 1;

/* ---- buffer-management callbacks (ios2_BufferManagement, task brief:
 * "required for tx/rx to work at all") -- this test tool's own address
 * space has no MMU-protection subtlety to work around, so both hooks are
 * a plain CopyMem, exactly like most simple SANA-II applications.
 *
 * Signature is SPEC-FIXED REGISTER convention, not this tool's choice:
 * BOOL Copy*Buff(APTR to [a0], APTR from [a1], ULONG size [d0]) -- see
 * virtionet_device.h/sana2.h's SANA2_COPY_FUNC comment for the full
 * provenance (~/src/sana2loop, verified bebbo-gcc register-annotated
 * function-pointer behavior). This definition must match that typedef
 * exactly, register-for-register, or the driver would call it with the
 * wrong ABI. */
static BOOL test_copy(APTR to __asm("a0"), APTR from __asm("a1"),
                      ULONG len __asm("d0"))
{
    CopyMem(from, to, len);
    return TRUE;
}

/*-------------------------------------------------------------------------
 * check -- one PASS/FAIL evidence line (docs/pci-library.md section 6's
 * own convention, mirrored here): concrete values printed either way.
 *-----------------------------------------------------------------------*/
static void check(CONST_STRPTR name, ULONG expected, ULONG got)
{
    BOOL pass = (expected == got);

    if (!pass)
        g_all_pass = 0;

    KPrintF((CONST_STRPTR)"VNETTEST %s: expected $%08lx got $%08lx %s\n",
            name, expected, got,
            pass ? (CONST_STRPTR)"PASS" : (CONST_STRPTR)"FAIL");
}

static int fail_result(void)
{
    KPrintF((CONST_STRPTR)"VNETTEST result: FAIL\n");
    return RETURN_FAIL;
}

int main(void)
{
    struct MsgPort *port;
    struct IOSana2Req *io;
    UBYTE dstAddr[6];
    UBYTE srcAddr[6];
    UBYTE stationAddr[6];
    struct TagItem bmTags[3];
    UBYTE txPayload[32];
    LONG txPayloadLen;
    LONG error;
    int i;

    /* ---- step 0: build the deterministic tx payload -------------------
     * "M68KVNET-TX-0001" + a counter -- the task brief's own wording.
     * The counter has no meaningful state to increment across runs (this
     * is a single-shot CLI tool, not a daemon), so it is fixed at the
     * literal "0001" already embedded in the prefix -- recorded here
     * rather than silently duplicating the digits. */
    txPayloadLen = 0;
    for (i = 0; TX_PAYLOAD_PREFIX[i] != '\0'; i++)
        txPayload[txPayloadLen++] = (UBYTE)TX_PAYLOAD_PREFIX[i];

    /* ---- step 1: open the port and IORequest --------------------------- */

    port = CreateMsgPort();
    if (!port) {
        KPrintF((CONST_STRPTR)"VNETTEST createport: FAIL CreateMsgPort "
                "returned NULL\n");
        return fail_result();
    }
    KPrintF((CONST_STRPTR)"VNETTEST createport: PASS\n");

    io = (struct IOSana2Req *)CreateExtIO(port, sizeof(struct IOSana2Req));
    if (!io) {
        KPrintF((CONST_STRPTR)"VNETTEST createextio: FAIL CreateExtIO "
                "returned NULL\n");
        DeleteMsgPort(port);
        return fail_result();
    }
    KPrintF((CONST_STRPTR)"VNETTEST createextio: PASS\n");

    /* ---- step 2: OpenDevice with the buffer-management tags (task
     * brief: "required for tx/rx to work at all") ------------------------ */

    bmTags[0].ti_Tag = SANA2_CopyToBuff;   bmTags[0].ti_Data = (ULONG)test_copy;
    bmTags[1].ti_Tag = SANA2_CopyFromBuff; bmTags[1].ti_Data = (ULONG)test_copy;
    bmTags[2].ti_Tag = TAG_DONE;           bmTags[2].ti_Data = 0;
    io->ios2_BufferManagement = (APTR)bmTags;

    error = OpenDevice((CONST_STRPTR)"virtionet.device", VNETTEST_UNIT,
                       (struct IORequest *)io, 0);
    if (error != 0) {
        KPrintF((CONST_STRPTR)"VNETTEST opendevice: FAIL error %ld -- "
                "normal on a machine without a virtio-net board\n",
                (LONG)error);
        DeleteExtIO((struct IORequest *)io);
        DeleteMsgPort(port);
        return fail_result();
    }
    VNetDeviceBase = io->ios2_Req.io_Device;
    KPrintF((CONST_STRPTR)"VNETTEST opendevice: PASS virtionet.device "
            "unit %ld opened\n", (LONG)VNETTEST_UNIT);

    /* ---- step 3: S2_GETSTATIONADDRESS, print the MAC -------------------
     * Real network stacks call this BEFORE configuring, to learn the
     * default (factory) address first. Real SANA-II ABI convention:
     * this command fills ios2_SrcAddr (current) and ios2_DstAddr
     * (default) directly on the request -- both inline arrays, no
     * ios2_Data buffer involved. */

    io->ios2_Req.io_Command = S2_GETSTATIONADDRESS;
    error = DoIO((struct IORequest *)io);
    check((CONST_STRPTR)"getstationaddress", 0,
          (ULONG)(error | io->ios2_Req.io_Error));
    for (i = 0; i < 6; i++) stationAddr[i] = io->ios2_DstAddr[i];
    KPrintF((CONST_STRPTR)"VNETTEST station address: "
            "%02lx:%02lx:%02lx:%02lx:%02lx:%02lx\n",
            (ULONG)stationAddr[0], (ULONG)stationAddr[1],
            (ULONG)stationAddr[2], (ULONG)stationAddr[3],
            (ULONG)stationAddr[4], (ULONG)stationAddr[5]);

    /* ---- step 4: S2_CONFIGINTERFACE, requesting the address just
     * learned (real SANA-II ABI: the requested address goes in
     * ios2_SrcAddr, an inline array, not ios2_Data). This driver's
     * address is not configurable and always reports back its own
     * factory MAC regardless of what is requested here (see
     * virtionet_device.c's own cmd_configinterface comment) -- this
     * tool still submits the learned default, mirroring what a real
     * stack does, rather than relying on that scope-down. */

    io->ios2_Req.io_Command = S2_CONFIGINTERFACE;
    for (i = 0; i < 6; i++) io->ios2_SrcAddr[i] = stationAddr[i];
    error = DoIO((struct IORequest *)io);
    check((CONST_STRPTR)"configinterface", 0,
          (ULONG)(error | io->ios2_Req.io_Error));

    /* ---- step 5: S2_ONLINE ---------------------------------------------- */

    io->ios2_Req.io_Command = S2_ONLINE;
    error = DoIO((struct IORequest *)io);
    check((CONST_STRPTR)"online", 0, (ULONG)(error | io->ios2_Req.io_Error));

    /* ---- step 6: transmit one deterministic recognisable frame --------- */

    for (i = 0; i < 6; i++) dstAddr[i] = 0xFF; /* broadcast */
    for (i = 0; i < 6; i++) srcAddr[i] = 0;    /* driver fills its own MAC */

    io->ios2_Req.io_Command = CMD_WRITE;
    io->ios2_Data = (APTR)txPayload;
    io->ios2_DataLength = (ULONG)txPayloadLen;
    for (i = 0; i < 6; i++) io->ios2_DstAddr[i] = dstAddr[i];
    for (i = 0; i < 6; i++) io->ios2_SrcAddr[i] = srcAddr[i];
    io->ios2_PacketType = VNET_TX_ETHERTYPE;
    error = DoIO((struct IORequest *)io);
    check((CONST_STRPTR)"cmd_write", 0,
          (ULONG)(error | io->ios2_Req.io_Error));
    KPrintF((CONST_STRPTR)"VNETTEST tx frame: dst FF:FF:FF:FF:FF:FF "
            "ethertype $%04lx payload \"%s\" (%ld bytes)\n",
            (ULONG)VNET_TX_ETHERTYPE, (CONST_STRPTR)TX_PAYLOAD_PREFIX,
            (LONG)txPayloadLen);

    /* ---- step 7: post a CMD_READ, narrate whatever arrives -------------
     * SendIO + a bounded poll on CheckIO rather than DoIO/WaitIO: this
     * step's own result is observational (file-top comment) -- there is
     * no external peer on this virtual wire yet to guarantee a frame
     * ever completes this request, so it must not block forever, and a
     * timeout is reported as evidence, not treated as this tool's own
     * FAIL. */
    {
        UBYTE rxBuffer[1600];
        BOOL gotReply = FALSE;
        ULONG i2;

        io->ios2_Req.io_Command = CMD_READ;
        io->ios2_Data = (APTR)rxBuffer;
        io->ios2_DataLength = sizeof(rxBuffer);
        io->ios2_PacketType = 0; /* accept any ethertype */

        /* Every prior command on this reused `io` went through DoIO
         * (SendIO+WaitIO): WaitIO() removes the replied message from the
         * reply port but does NOT reset io_Message.mn_Node.ln_Type back
         * off NT_REPLYMSG -- real exec.library leaves that for whoever
         * reuses the request next. SendIO() itself does not reset it
         * either (it is a thin wrapper straight onto BeginIO()). This is
         * invisible to every earlier step here because DoIO always
         * WaitIO()s the fresh reply into existence before the next
         * SendIO -- but this step is the first to call SendIO() and then
         * poll CheckIO() directly (this function's own file-top comment:
         * observational, no WaitIO). Left at NT_REPLYMSG from CMD_WRITE's
         * own completion, the very first CheckIO() below would otherwise
         * report "done" before virtionet.device's BeginIO ever runs --
         * confirmed empirically (i2 == 0, stale ln_Type == NT_REPLYMSG,
         * io_Flags == 0, i.e. not even IOF_QUICK). Resetting to
         * NT_MESSAGE before SendIO() is what every other step here gets
         * for free from starting each command with a properly reset
         * request. */
        io->ios2_Req.io_Message.mn_Node.ln_Type = NT_MESSAGE;

        SendIO((struct IORequest *)io);

        for (i2 = 0; i2 < READ_POLL_LIMIT; i2++) {
            if (CheckIO((struct IORequest *)io)) {
                gotReply = TRUE;
                break;
            }
        }

        if (!gotReply) {
            AbortIO((struct IORequest *)io);
            WaitIO((struct IORequest *)io);
            KPrintF((CONST_STRPTR)"VNETTEST cmd_read: observed no frame "
                    "within the poll window (no peer on this virtual "
                    "wire yet -- not a failure at this stage, see this "
                    "tool's own file-top comment)\n");
        } else {
            WaitIO((struct IORequest *)io);

            if (io->ios2_Req.io_Error != 0) {
                KPrintF((CONST_STRPTR)"VNETTEST cmd_read: completed with "
                        "io_Error %ld (wire error %ld)\n",
                        (LONG)io->ios2_Req.io_Error,
                        (LONG)io->ios2_WireError);
            } else {
                ULONG ethertype = io->ios2_PacketType;
                /* No io_Actual on this ABI -- ios2_DataLength itself was
                 * overwritten by the driver with the actual bytes
                 * received (dual-role field, see sana2.h). */
                ULONG payloadLen = io->ios2_DataLength;
                char printable[64];
                ULONG copyN = payloadLen;

                if (copyN > sizeof(printable) - 1)
                    copyN = sizeof(printable) - 1;
                for (i = 0; (ULONG)i < copyN; i++) {
                    UBYTE c = rxBuffer[i];
                    printable[i] = (c >= 0x20 && c < 0x7F) ? (char)c : '.';
                }
                printable[copyN] = '\0';

                KPrintF((CONST_STRPTR)"VNETTEST cmd_read: PASS received "
                        "frame ethertype $%04lx payload \"%s\" (%ld "
                        "bytes)\n",
                        (ULONG)ethertype, (CONST_STRPTR)printable,
                        (LONG)payloadLen);
            }
        }
    }

    /* ---- step 8: S2_OFFLINE, close, report ------------------------------ */

    io->ios2_Req.io_Command = S2_OFFLINE;
    error = DoIO((struct IORequest *)io);
    check((CONST_STRPTR)"offline", 0, (ULONG)(error | io->ios2_Req.io_Error));

    CloseDevice((struct IORequest *)io);
    DeleteExtIO((struct IORequest *)io);
    DeleteMsgPort(port);
    KPrintF((CONST_STRPTR)"VNETTEST closedevice: done\n");

    if (g_all_pass) {
        KPrintF((CONST_STRPTR)"VNETTEST result: ALL PASS\n");
        return RETURN_OK;
    }

    return fail_result();
}

/*
 * Marker vocabulary (every "VNETTEST "-prefixed line this tool can emit,
 * for the real-ROM test to grep against):
 *
 *   VNETTEST createport: PASS/FAIL
 *   VNETTEST createextio: PASS/FAIL
 *   VNETTEST opendevice: PASS/FAIL ...
 *   VNETTEST getstationaddress: expected $... got $... PASS/FAIL
 *   VNETTEST station address: XX:XX:XX:XX:XX:XX
 *   VNETTEST configinterface: expected $... got $... PASS/FAIL
 *   VNETTEST online: expected $... got $... PASS/FAIL
 *   VNETTEST cmd_write: expected $... got $... PASS/FAIL
 *   VNETTEST tx frame: dst FF:FF:FF:FF:FF:FF ethertype $88B5 payload "..." (N bytes)
 *   VNETTEST cmd_read: observed no frame within the poll window (...)
 *   VNETTEST cmd_read: completed with io_Error N (wire error N)
 *   VNETTEST cmd_read: PASS received frame ethertype $... payload "..." (N bytes)
 *   VNETTEST offline: expected $... got $... PASS/FAIL
 *   VNETTEST closedevice: done
 *   VNETTEST result: ALL PASS
 *   VNETTEST result: FAIL
 */
