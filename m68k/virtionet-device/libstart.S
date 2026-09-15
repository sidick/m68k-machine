| virtionet.device -- run guard. Sits at offset 0 so that if the device
| file is ever executed as a program it returns failure instead of
| crashing. The real entry point is the Resident (ROMTag), which exec
| locates by scanning the hunk.
|
| Copied from m68k/rtgboard-card/libstart.S (itself from amirfb.card's
| src/card/libstart.S, BSD 2-Clause, same owner).
	.text
	.globl	_start
_start:
	moveq	#-1,d0
	rts
