| pciprobe_int.S -- INT2 (INTB_PORTS) server entry stub for PCIProbe
| (docs/pci-library.md section 6 step 4; register contract in
| docs/pcibridge-protocol.md section 5/8, m68k/prometheus-library/
| pcibridge_card.h).
|
| Exec's interrupt server dispatcher calls every node on the INTB_PORTS
| chain with A1 = is_Data (the struct Interrupt's own is_Data field, set
| in pciprobe.c to &g_intx_state) -- the standard exec server register
| convention m68k/hostblk-rom/hostblk-diagrom.s's own IntHandler already
| documents and this file follows verbatim: preserve every register
| except D0/D1/A0/A1.
|
| The C handler (pciprobe_intx_handler(), pciprobe.c) takes one argument
| (the state pointer) via the ordinary C stack-based calling convention
| this toolchain uses for plain functions -- so A1 is pushed as that
| argument, not passed in a register, before the jsr.
	.text
	.globl	_pciprobe_int_server
_pciprobe_int_server:
	movem.l	d2-d7/a2-a6,-(sp)
	move.l	a1,-(sp)
	jsr	_pciprobe_intx_handler
	addq.l	#4,sp
	movem.l	(sp)+,d2-d7/a2-a6

	| Z-flag contract (Autodocs/exec.doc "AddIntServer": Z clear only if
	| the interrupt was "specifically for that server, and no one
	| else" -- hostblk-diagrom.s's IntHandler comment settles the same
	| question for this project's other shared-PORTS-chain server and
	| is followed here identically). INTB_PORTS is a shared chain
	| (docs/pci-library.md section 5): this server has no way to know
	| it is the chain's only member, so it must never claim exclusivity
	| by returning Z-clear -- doing so would starve every other server
	| sharing INT2 (in principle the CIA server and anything else on
	| PORTS). moveq #0,d0 sets Z immediately before rts unconditionally,
	| regardless of whether pciprobe_intx_handler found its own status
	| bit set this call. The SDK autodoc's return-1-to-claim convention
	| describes an EXCLUSIVE server, which this is not.
	moveq	#0,d0
	rts
