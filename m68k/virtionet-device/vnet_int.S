| vnet_int.S -- INT2 (INTB_PORTS) server entry stub for virtionet.device.
|
| Same shape and same Z-flag discipline as m68k/pciprobe/pciprobe_int.S
| (docs/pci-library.md section 5: shared INT2 servers set Z on exit unless
| exclusively claimed -- this device shares the chain with every other
| card's server, including prometheus.library's own bookkeeping and
| whatever else is on INTB_PORTS, so it never claims exclusivity).
|
| Exec's interrupt server dispatcher calls every node on the INTB_PORTS
| chain with A1 = is_Data (here: &VNetBase, set at Prm_AddIntServer time).
| Preserve every register except D0/D1/A0/A1, per the standard exec
| server register convention.
	.text
	.globl	_vnet_int_server
_vnet_int_server:
	movem.l	d2-d7/a2-a6,-(sp)
	move.l	a1,-(sp)
	jsr	_vnet_isr_handler
	addq.l	#4,sp
	movem.l	(sp)+,d2-d7/a2-a6

	| Shared chain: never claim exclusivity (see file-top comment).
	moveq	#0,d0
	rts
