/*
 * pciprobe.c -- PCIProbe: the guest-side probe tool for ADR 0005 stage 2
 * (docs/pci-library.md section 6).
 *
 * A real AmigaOS CLI program (standard libnix startup, -noixemul, no
 * -nostartfiles -- it is installed and run from a shell/startup-sequence
 * like any other command) that exercises only the PUBLIC prometheus.
 * library API a period driver would call, narrating every step over
 * serial via KPrintF so the real-ROM test can grep the guest's own
 * evidence. Every marker line this program prints starts "PCIPROBE " --
 * keep that prefix and the exact strings the host test greps stable; see
 * this file's own end for the complete list. One line per check, each
 * carrying PASS/FAIL and the concrete values either way (this platform's
 * standing rule: absence of complaint is never success), and a single
 * final verdict line: "PCIPROBE result: ALL PASS" only if every check
 * passed, "PCIPROBE result: FAIL" otherwise.
 *
 * Steps, in the order docs/pci-library.md section 6 specifies:
 *   1. OpenLibrary("prometheus.library", 2).
 *   2. Walk every board (Prm_FindBoardTagList/Prm_GetBoardAttrsTagList).
 *   3. Find the virtio-net stub (vendor $1AF4 device $1041).
 *   4. The byte-order assertions (docs/pci-library.md section 3) on it.
 *   5. BAR0 checks, including the raw-config-space cross-check and the
 *      Prm_GetPhysicalAddress/Prm_GetVirtualAddress round trip.
 *   6. The aperture read (expects the master-abort all-ones value).
 *   7. DMA buffer alloc/physaddr-identity/free.
 *   8. INTx end to end: Prm_AddIntServer, the one deliberately
 *      out-of-band harness step (FindConfigDev + a raw INTX_TEST poke),
 *      and Prm_RemIntServer.
 *   9. CloseLibrary, exit RETURN_OK/RETURN_FAIL.
 */

#include <exec/types.h>
#include <exec/nodes.h>
#include <exec/interrupts.h>
#include <exec/libraries.h>
#include <dos/dos.h>
#include <utility/tagitem.h>
#include <libraries/configvars.h>

#include <proto/exec.h>
#include <proto/expansion.h>
#include <proto/prometheus.h>
#include <clib/debug_protos.h>

#include "pcibridge_card.h"

/* The virtio-net stub's identity (docs/pcibridge-protocol.md section 2,
 * docs/pci-library.md section 3) -- this is what the probe filters for,
 * not a board this file invents. */
#define NET_VENDOR 0x1AF4UL
#define NET_DEVICE 0x1041UL

/* BAR policy this probe cross-checks against (docs/pci-library.md
 * section 2/8): 32-bit memory BARs assigned inside this 8 MB PCI
 * memory-space region, below the Zorro III window. */
#define POLICY_REGION_BASE  0x20000000UL
#define POLICY_REGION_BYTES 0x00800000UL
#define POLICY_REGION_END   (POLICY_REGION_BASE + POLICY_REGION_BYTES)

#define NET_BAR0_SIZE 0x4000UL /* 16 KiB (docs/pcibridge-protocol.md SS2) */

#define DMA_BUFFER_BYTES 256UL

/* Bounded, generous busy-wait for the INT2 harness step (docs/
 * pci-library.md section 6 step 4 / the task's own instruction: "a
 * bounded generous loop ~2 million iterations"). */
#define INTX_POLL_LIMIT 2000000UL

/* prometheus.library's own base pointer -- proto/prometheus.h only
 * extern-declares it (the seam every period driver compiles against);
 * this program, like any ordinary caller, supplies the definition and
 * opens the library itself. Standard libnix startup already supplies
 * SysBase, so this program must NOT redefine it (unlike m68k/
 * rtgboard-card's AUTOINIT library, which is -nostdlib and has no
 * startup code to do that for it). */
struct Library *PrometheusBase;

/* expansion.library is only needed for the step 8 harness's own
 * FindConfigDev call (finding the pcibridge card's raw register base is
 * explicitly out-of-band, not part of the public prometheus.library
 * API) -- opened and closed around that one step, mirroring
 * rtgboard_card.c's FindCard. */
struct ExpansionBase *ExpansionBase;

/* Overall verdict: starts true, any FAIL clears it. Checked once at the
 * very end for the single "PCIPROBE result: ..." line. */
static LONG g_all_pass = 1;

/* Shared state between the INT2 server (pciprobe_int.S ->
 * pciprobe_intx_handler(), interrupt context) and main() (task
 * context), busy-waiting on the same object -- both sides must see the
 * other's writes, hence volatile throughout (docs/pci-library.md
 * section 6 step 4's own instruction). regbase is a UBYTE* rather than
 * a ULONG* precisely so every register offset is an explicit byte
 * addend, cast to the access width only at the point of use -- the
 * clearest way to avoid the classic pointer-arithmetic-doubles-the-
 * offset mistake once regbase is repurposed for a 4-byte-wide access.
 */
struct IntxState {
    volatile UBYTE *regbase;
    volatile ULONG counter;
};

static struct IntxState g_intx_state;
static struct Interrupt g_interrupt;

/* Defined in pciprobe_int.S: the actual is_Code entry exec calls on the
 * INTB_PORTS chain (A1 = is_Data). Declared here only so its address can
 * be taken -- it is never called directly from C. */
extern void pciprobe_int_server(void);

/*-------------------------------------------------------------------------
 * pciprobe_intx_handler -- the C half of the INT2 server. Called (from
 * pciprobe_int.S) with the state pointer on the stack, ordinary C
 * calling convention. Runs at interrupt level: touches only the shared
 * state and the pcibridge card's own registers, nothing that could
 * block or allocate.
 *-----------------------------------------------------------------------*/
void pciprobe_intx_handler(struct IntxState *state)
{
    volatile ULONG *status = (volatile ULONG *)(state->regbase + PCIB_INTX_STATUS);
    volatile ULONG *test   = (volatile ULONG *)(state->regbase + PCIB_INTX_TEST);

    if (*status & PCIB_INTX_A) {
        *test = 0;              /* deassert the line -- the acknowledge */
        state->counter++;
    }
}

/*-------------------------------------------------------------------------
 * hex_upper -- format 'v' as exactly 'digits' uppercase hex characters
 * into 'buf' (which must hold digits+1 bytes). Written by hand rather
 * than trusting KPrintF/RawDoFmt's %lx: RawDoFmt only documents a
 * lowercase hex conversion, and docs/pci-library.md section 6's "bar0"
 * line is parsed by the host test against uppercase hex, so this probe
 * does not leave that up to a formatter that was never specified to
 * produce it.
 *-----------------------------------------------------------------------*/
static void hex_upper(ULONG v, int digits, char *buf)
{
    static const char hexchars[] = "0123456789ABCDEF";
    int i;

    buf[digits] = '\0';
    for (i = digits - 1; i >= 0; i--) {
        buf[i] = hexchars[v & 0xFUL];
        v >>= 4;
    }
}

/*-------------------------------------------------------------------------
 * check -- one PASS/FAIL evidence line, expected vs. got, both printed
 * as hex regardless of outcome (this platform's standing rule: report
 * concrete values, not just a verdict). Updates g_all_pass.
 *-----------------------------------------------------------------------*/
static void check(CONST_STRPTR name, ULONG expected, ULONG got)
{
    BOOL pass = (expected == got);

    if (!pass)
        g_all_pass = 0;

    KPrintF((CONST_STRPTR)"PCIPROBE %s: expected $%08lx got $%08lx %s\n",
            name, expected, got,
            pass ? (CONST_STRPTR)"PASS" : (CONST_STRPTR)"FAIL");
}

/*-------------------------------------------------------------------------
 * fail_result -- print the single final verdict line as FAIL and exit
 * RETURN_FAIL. Used by the early-exit paths (library missing, board
 * missing) where later steps cannot run at all.
 *-----------------------------------------------------------------------*/
static int fail_result(void)
{
    KPrintF((CONST_STRPTR)"PCIPROBE result: FAIL\n");
    return RETURN_FAIL;
}

int main(void)
{
    PCIBoard *board;
    PCIBoard *net = NULL;
    LONG board_count = 0;

    /* ---- Step 1: open the library. ------------------------------- */

    PrometheusBase = OpenLibrary((CONST_STRPTR)"prometheus.library", 2);
    if (!PrometheusBase) {
        KPrintF((CONST_STRPTR)"PCIPROBE openlibrary: FAIL "
                "prometheus.library version 2 not available\n");
        return fail_result();
    }
    KPrintF((CONST_STRPTR)"PCIPROBE openlibrary: PASS prometheus.library "
            "opened (version %ld)\n", (LONG)PrometheusBase->lib_Version);

    /* ---- Step 2: walk every board. --------------------------------
     * Each TagItem's ti_Data points at a local ULONG that
     * Prm_GetBoardAttrsTagList fills in -- the "GET" tag convention,
     * as opposed to step 3's filter taglist below, where ti_Data
     * carries the literal value to match. */

    board = NULL;
    while ((board = Prm_FindBoardTagList(board, NULL)) != NULL) {
        ULONG vendor = 0, device = 0, revision = 0, class_ = 0,
              subclass = 0, slot = 0, function = 0;
        struct TagItem attrs[] = {
            { PRM_Vendor,         (ULONG)&vendor   },
            { PRM_Device,         (ULONG)&device   },
            { PRM_Revision,       (ULONG)&revision },
            { PRM_Class,          (ULONG)&class_   },
            { PRM_SubClass,       (ULONG)&subclass },
            { PRM_SlotNumber,     (ULONG)&slot     },
            { PRM_FunctionNumber, (ULONG)&function },
            { TAG_DONE, 0 }
        };

        Prm_GetBoardAttrsTagList(board, attrs);

        KPrintF((CONST_STRPTR)"PCIPROBE board %ld: vendor $%04lx "
                "device $%04lx revision $%02lx class $%02lx "
                "subclass $%02lx slot %ld function %ld\n",
                board_count, vendor, device, revision, class_, subclass,
                (LONG)slot, (LONG)function);

        if (vendor == NET_VENDOR && device == NET_DEVICE)
            net = board;

        board_count++;
    }
    KPrintF((CONST_STRPTR)"PCIPROBE bus walk: %ld board(s) found\n",
            board_count);

    /* ---- Step 3: find the virtio-net stub, by a fresh filtered walk
     * (the tag values here are literal match values, not pointers --
     * the "FIND" convention). The step-2 walk above already located it
     * incidentally; this repeats the lookup the documented way so the
     * filter-taglist path itself is exercised and evidenced. */
    {
        struct TagItem filter[] = {
            { PRM_Vendor, NET_VENDOR },
            { PRM_Device, NET_DEVICE },
            { TAG_DONE, 0 }
        };

        net = Prm_FindBoardTagList(NULL, filter);
    }

    if (!net) {
        KPrintF((CONST_STRPTR)"PCIPROBE find virtio-net: FAIL no board "
                "matched vendor $%04lx device $%04lx\n",
                NET_VENDOR, NET_DEVICE);
        CloseLibrary(PrometheusBase);
        return fail_result();
    }
    KPrintF((CONST_STRPTR)"PCIPROBE find virtio-net: PASS board $%08lx\n",
            (ULONG)net);

    /* ---- Step 4: the byte-order assertions (docs/pci-library.md
     * section 3). Each aligned config longword is presented as a
     * big-endian image of its DECODED value -- so
     * Prm_ReadConfigWord(net, 0) reads back the DEVICE id (it would
     * read the vendor id if this presentation were "tidied" to match
     * real PCI's little-endian byte order, and a wrong-endian
     * implementation fails these with a different concrete value
     * again -- that is the whole point of asserting them here rather
     * than trusting the accessor). */
    check((CONST_STRPTR)"byteorder cfglong@0",
          0x10411AF4UL, Prm_ReadConfigLong(net, 0));
    check((CONST_STRPTR)"byteorder cfgword@0 (device)",
          0x1041UL, (ULONG)Prm_ReadConfigWord(net, 0));
    check((CONST_STRPTR)"byteorder cfgword@2 (vendor)",
          0x1AF4UL, (ULONG)Prm_ReadConfigWord(net, 2));
    check((CONST_STRPTR)"byteorder cfgbyte@0",
          0x10UL, (ULONG)Prm_ReadConfigByte(net, 0));
    check((CONST_STRPTR)"byteorder cfgbyte@1",
          0x41UL, (ULONG)Prm_ReadConfigByte(net, 1));
    check((CONST_STRPTR)"byteorder cfgbyte@2",
          0x1AUL, (ULONG)Prm_ReadConfigByte(net, 2));
    check((CONST_STRPTR)"byteorder cfgbyte@3",
          0xF4UL, (ULONG)Prm_ReadConfigByte(net, 3));

    /* ---- Step 5: BAR0. --------------------------------------------- */
    {
        ULONG memaddr0 = 0, memsize0 = 0;
        struct TagItem bar_attrs[] = {
            { PRM_MemoryAddr0, (ULONG)&memaddr0 },
            { PRM_MemorySize0, (ULONG)&memsize0 },
            { TAG_DONE, 0 }
        };
        ULONG raw_bar0;
        ULONG pci_addr;
        char cpubuf[9], pcibuf[9], sizebuf[5];
        APTR physaddr, virtaddr;

        Prm_GetBoardAttrsTagList(net, bar_attrs);

        if (memaddr0 == 0) {
            g_all_pass = 0;
            KPrintF((CONST_STRPTR)"PCIPROBE bar0 memaddr0: FAIL "
                    "zero (no aperture assigned)\n");
        } else {
            KPrintF((CONST_STRPTR)"PCIPROBE bar0 memaddr0: PASS "
                    "nonzero ($%08lx)\n", memaddr0);
        }
        check((CONST_STRPTR)"bar0 memsize0", NET_BAR0_SIZE, memsize0);

        /* The raw config-space read at 0x10 (BAR0) returns the
         * big-endian-presented longword, which IS the decoded BAR
         * value (docs/pci-library.md section 6 step 5) -- the low 4
         * bits are the memory-BAR type/flags bits, masked off below. */
        raw_bar0 = Prm_ReadConfigLong(net, 0x10);
        pci_addr = raw_bar0 & ~0xFUL;

        hex_upper(memaddr0, 8, cpubuf);
        hex_upper(pci_addr, 8, pcibuf);
        hex_upper(memsize0, 4, sizebuf);
        /* This exact line ("PCIPROBE bar0: cpu $XXXXXXXX pci $XXXXXXXX
         * size $XXXX", uppercase hex) is parsed by the host test and
         * cross-checked against host introspection -- keep its shape
         * stable (docs/pci-library.md section 6). */
        KPrintF((CONST_STRPTR)"PCIPROBE bar0: cpu $%s pci $%s size $%s\n",
                (CONST_STRPTR)cpubuf, (CONST_STRPTR)pcibuf,
                (CONST_STRPTR)sizebuf);

        if (pci_addr >= POLICY_REGION_BASE && pci_addr < POLICY_REGION_END) {
            KPrintF((CONST_STRPTR)"PCIPROBE bar0 pci range: PASS "
                    "$%08lx in [$%08lx,$%08lx)\n",
                    pci_addr, (ULONG)POLICY_REGION_BASE,
                    (ULONG)POLICY_REGION_END);
        } else {
            g_all_pass = 0;
            KPrintF((CONST_STRPTR)"PCIPROBE bar0 pci range: FAIL "
                    "$%08lx not in [$%08lx,$%08lx)\n",
                    pci_addr, (ULONG)POLICY_REGION_BASE,
                    (ULONG)POLICY_REGION_END);
        }

        physaddr = Prm_GetPhysicalAddress((APTR)memaddr0);
        check((CONST_STRPTR)"bar0 physaddr crosscheck",
              pci_addr, (ULONG)physaddr);

        virtaddr = Prm_GetVirtualAddress((APTR)pci_addr);
        check((CONST_STRPTR)"bar0 virtaddr crosscheck",
              memaddr0, (ULONG)virtaddr);

        /* ---- Step 6: the aperture read. The stub has no function
         * behind its BAR, so a real access through the banked window
         * must answer the master-abort all-ones value -- getting that
         * (rather than, say, always-zero or the last CPU bus value)
         * is the positive evidence the aperture is genuinely plumbed
         * through to PCI memory space, not just returning some other
         * constant by accident. */
        {
            volatile ULONG *aperture = (volatile ULONG *)memaddr0;
            ULONG got = *aperture;

            check((CONST_STRPTR)"aperture read (master-abort)",
                  0xFFFFFFFFUL, got);
        }
    }

    /* ---- Step 7: DMA. ------------------------------------------------ */
    {
        APTR buf = Prm_AllocDMABuffer(DMA_BUFFER_BYTES);

        if (!buf) {
            g_all_pass = 0;
            KPrintF((CONST_STRPTR)"PCIPROBE dma alloc: FAIL "
                    "Prm_AllocDMABuffer(%ld) returned NULL\n",
                    (LONG)DMA_BUFFER_BYTES);
        } else {
            APTR physaddr;

            KPrintF((CONST_STRPTR)"PCIPROBE dma alloc: PASS "
                    "buffer $%08lx (%ld bytes)\n",
                    (ULONG)buf, (LONG)DMA_BUFFER_BYTES);

            /* This machine's DMA identity (docs/pci-library.md
             * section 4): guest physical address == PCI bus address
             * for all RAM, so the buffer's own address is expected
             * back unchanged. */
            physaddr = Prm_GetPhysicalAddress(buf);
            check((CONST_STRPTR)"dma physaddr identity",
                  (ULONG)buf, (ULONG)physaddr);

            Prm_FreeDMABuffer(buf, DMA_BUFFER_BYTES);
            KPrintF((CONST_STRPTR)"PCIPROBE dma free: done "
                    "($%08lx, %ld bytes)\n",
                    (ULONG)buf, (LONG)DMA_BUFFER_BYTES);
        }
    }

    /* ---- Step 8: INTx end to end. ------------------------------------ */
    {
        BOOL added;

        g_intx_state.regbase = NULL;
        g_intx_state.counter = 0;

        g_interrupt.is_Node.ln_Type = NT_INTERRUPT;
        g_interrupt.is_Node.ln_Pri  = 0;
        g_interrupt.is_Node.ln_Name = (char *)"PCIProbe";
        g_interrupt.is_Data = (APTR)&g_intx_state;
        g_interrupt.is_Code = (VOID (*)())pciprobe_int_server;

        added = Prm_AddIntServer(net, &g_interrupt);
        if (!added) {
            g_all_pass = 0;
            KPrintF((CONST_STRPTR)"PCIPROBE intx addserver: FAIL "
                    "Prm_AddIntServer returned FALSE\n");
        } else {
            KPrintF((CONST_STRPTR)"PCIPROBE intx addserver: PASS\n");

            /* The ONE deliberately out-of-band harness step (docs/
             * pci-library.md section 6 step 4): finding the pcibridge
             * card's own raw register base via FindConfigDev is NOT
             * part of the public prometheus.library API a real driver
             * would call -- no period driver pokes its host bridge's
             * private diagnostic registers. It exists purely so this
             * test harness can assert an INTA line from the card side
             * without stage 3's virtio-net function logic existing yet
             * (docs/pci-library.md section 5/section 6's own framing:
             * INTX_TEST stands in for a real device raising INTx until
             * stage 3 lands). */
            ExpansionBase = (struct ExpansionBase *)
                OpenLibrary((CONST_STRPTR)"expansion.library", 36);
            if (!ExpansionBase) {
                g_all_pass = 0;
                KPrintF((CONST_STRPTR)"PCIPROBE intx harness: FAIL "
                        "expansion.library open failed\n");
            } else {
                struct ConfigDev *cd = FindConfigDev(NULL,
                        PCIBRIDGE_MANUFACTURER, PCIBRIDGE_PRODUCT);

                if (!cd) {
                    g_all_pass = 0;
                    KPrintF((CONST_STRPTR)"PCIPROBE intx harness: FAIL "
                            "no pcibridge ConfigDev found (manufacturer "
                            "$%04lx product %ld)\n",
                            (ULONG)PCIBRIDGE_MANUFACTURER,
                            (LONG)PCIBRIDGE_PRODUCT);
                } else {
                    volatile ULONG *test_reg;

                    KPrintF((CONST_STRPTR)"PCIPROBE intx harness: found "
                            "pcibridge board at $%08lx (out-of-band "
                            "harness step, not public API)\n",
                            (ULONG)cd->cd_BoardAddr);

                    g_intx_state.regbase = (volatile UBYTE *)cd->cd_BoardAddr;
                    test_reg = (volatile ULONG *)
                            (g_intx_state.regbase + PCIB_INTX_TEST);

                    /* Single ULONG write of 1: asserts INTA on the card
                     * side (docs/pcibridge-protocol.md section 4/8),
                     * which the card ORs onto INT2 -- the same
                     * level-triggered shared-line contract every other
                     * native card here uses. */
                    *test_reg = PCIB_INTX_A;

                    {
                        ULONG i;
                        BOOL observed = FALSE;

                        for (i = 0; i < INTX_POLL_LIMIT; i++) {
                            if (g_intx_state.counter != 0) {
                                observed = TRUE;
                                break;
                            }
                        }

                        if (observed) {
                            KPrintF((CONST_STRPTR)"PCIPROBE intx: "
                                    "observed INTA via INT2 (count %ld)\n",
                                    (LONG)g_intx_state.counter);
                        } else {
                            g_all_pass = 0;
                            KPrintF((CONST_STRPTR)"PCIPROBE intx: FAIL "
                                    "timeout waiting for INT2 dispatch "
                                    "(count %ld after %ld iterations)\n",
                                    (LONG)g_intx_state.counter,
                                    (LONG)INTX_POLL_LIMIT);
                        }
                    }
                }

                CloseLibrary((struct Library *)ExpansionBase);
                ExpansionBase = NULL;
            }

            Prm_RemIntServer(net, &g_interrupt);
            KPrintF((CONST_STRPTR)"PCIPROBE intx remserver: done\n");
        }
    }

    /* ---- Step 9: close the library and report. ------------------- */
    CloseLibrary(PrometheusBase);
    PrometheusBase = NULL;

    if (g_all_pass) {
        KPrintF((CONST_STRPTR)"PCIPROBE result: ALL PASS\n");
        return RETURN_OK;
    }

    return fail_result();
}
