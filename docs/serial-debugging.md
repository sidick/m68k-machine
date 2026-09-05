# Serial debugging: reaching Kickstart's ROMWack over the emulated UART

**Status:** working, reproduced twice. This documents `machine-hosted`'s
break-in into Kickstart 3.2.2's built-in ROMWack debugger, what ROMWack
actually accepts on this ROM, and what that evidence changes (and does
not change) about the Phase 1 exit criterion in
`docs/combined-roadmap.md`.

## What ROMWack is

Every retail Kickstart from 1.x onward has shipped a resident, minimal
machine-code monitor that Alert() drops into when instructed to, reached
by a break-in sequence typed at the keyboard or (as here) sent over the
serial port while the ROM is in its alert/LED-blink loop. The name and
implementation changed twice:

- **1.x–2.x:** no serial monitor by default in most ROM revisions; later
  became "SAD" (Simple Amiga Debugger) territory via third-party tools.
- **3.0–3.9:** shipped SAD, invoked the same way.
- **3.2+ (this project's ROM, 47.115):** SAD was replaced by a debugger
  that announces itself as `rom-wack` in its own banner — this is a
  restoration/rewrite, not the same binary SAD was. It is what this
  document and the `--trigger-illegal-after-frames` / `--serial-script`
  flags in `machine-hosted` actually reach.

pyamigadebug's `RomWack.py`
(<https://github.com/rvalles/pyamigadebug/blob/master/RomWack.py>) is a
1.x/2.x-era reference for the break-in handshake and command set. Some
of it still holds (9600 8N1, a handshake before the ROM accepts
commands); some of it does not — see "What this ROM's ROMWack actually
does" below, which reports what was *observed*, not what that reference
predicts.

## The ROM mechanism

Disassembled from the A1200 47.115 ROM image. The break-in poll lives at
`$F83F9C`–`$F83FD8` and is the **only** absolute `SERDATR` read in the
whole image — Kickstart 3.2.2 does not otherwise touch the serial port
at all outside this loop, which is the load-bearing fact for the Phase 1
discussion below.

```
$f83f9e: MOVE.W #$0174,$DFF032   ; SERPER = 372 = 9600 baud, set by the ROM itself
$f83fa6: MOVEQ #-1,D0
$f83fa8: BSET #1,$BFE001 / DBF D0  ; power LED off, ~65536-iteration delay
$f83fb4: BCLR #1,$BFE001 / DBF D0  ; power LED on, ~65536-iteration delay
$f83fc0: MOVE.W $DFF018,D0       ; poll SERDATR
$f83fc6: MOVE.W #$0800,$DFF09C   ; clear INTREQ bit 11 (RBF)
$f83fce: AND.B #$7F,D0
$f83fd2: CMP.B #$7F,D0           ; DEL?
$f83fd6: DBEQ D1,$f83fa6         ; D1 was set to 5 at $f83f9c
```

Two consequences that explain why an earlier, unscripted attempt at this
break-in failed, and that any script driving it has to account for:

1. **The window is only six blink cycles.** `D1` is loaded with 5 and
   the loop is `DBEQ`, so the ROM polls `SERDATR` at most six times —
   once per LED half-cycle — before giving up and falling through to
   the normal alert display. A single well-timed DEL byte can hit this,
   but nothing in this crate can line a `SEND` up with the guest's
   blink-loop timing in advance. Flooding DEL across the whole window
   (`REPEAT 400 SEND \x7f` — see below) is what makes the break-in
   reliable instead of a matter of luck.
2. **The ROM never checks RBF (receive-buffer-full).** It reads
   `SERDATR` unconditionally and masks with `AND.B #$7F`. The receive
   path only has to put a byte in the low 7 bits for the poll to see it
   — which is also why the chipset's idle `SERDATR` value (`$3800`, low
   byte zero) correctly *fails* to match DEL when nothing has been sent.

This poll is reached **only** from the alert/LED-blink path. Retail
Kickstart does not run it, or touch `SERDAT`/`SERDATR` at all, during a
normal, undisturbed boot — see "The Phase 1 exit criterion" below.

## Reproducing it

```
cargo build -p machine-hosted --release

./target/release/machine-hosted \
  --rom <path to a Kickstart 3.2.x ROM, e.g. A1200 47.115> \
  --trigger-illegal-after-frames 20 \
  --serial-script examples/serial-scripts/romwack-break-in.txt \
  --max-frames 400
```

`--trigger-illegal-after-frames 20` forces a genuine 68k
illegal-instruction exception 20 frames after the ROM overlay first
clears, by calling `CpuCore::take_illegal_exception` directly from the
host (`crates/machine-hosted/src/cli.rs`). This is what opens the
alert/LED-blink loop in the first place; without it, this ROM never
reaches the break-in poll at all. `examples/serial-scripts/romwack-break-in.txt`
is `REPEAT 400 SEND \x7f` — see `crates/machine-hosted/src/serial_script.rs`'s
`REPEAT` directive (added for this script; see "The `REPEAT` directive"
below).

Verified output (reproduced twice, byte-identical both times):

```
host  | PHASE1 HOSTED: forcing illegal-instruction exception at frame 29 (--trigger-illegal-after-frames 20) to reach the alert/LED-blink loop
GUEST | rom-wack
GUEST | PC: 00028C  SR: 2700  USP: 0032B8  SSP: 1FF8B8  XCPT: 8000002F  TASK: 000000
GUEST | DR: 0000007F 0000007F 00001801 8000002F 00000000 0000FF00 FFFFFFFF 00000000
GUEST | AR: 001FF8B4 00003420 00007E3C 00002336 001FF8D4 00F83290 00000200
GUEST | SF: 2700 0000 028C 00BC 001F F92C 00F8 31EC 00F8 31B8 00F8 31F6 00F8 3322 0000
```

`XCPT: 8000002F` is the forced exception's vector (illegal instruction,
vector 4, encoded here as `0x8000002F`); it will differ if a different
fault is injected.

### The `REPEAT` directive

`serial_script`'s language (`crates/machine-hosted/src/serial_script.rs`)
gained `REPEAT <n> <directive>` for this script: it expands at parse
time to `n` copies of the inner directive. The alternative — a script
file of 400 near-identical `SEND \x7f` lines — was rejected as a real
maintenance hazard, not just an aesthetic one: a stray edit to one of
400 duplicate lines is invisible in a diff review. `REPEAT` is
restricted to a single, non-nested inner directive (nesting is a parse
error) because nothing here needs more than that, and it is simpler to
reject speculative generality than to define its semantics ahead of a
use case.

## What this ROM's ROMWack actually does

The pyamigadebug reference is 1.x/2.x-era. Everything below was
*observed* against the 47.115 ROM's `rom-wack`, driven interactively
through `--serial-script`, using `examples/serial-scripts/romwack-interactive-session.txt`
as the base script. Where it agrees with the reference, that's noted;
where it doesn't, the transcript is the authority.

**Handshake.** Confirmed 9600 baud (the ROM sets `SERPER` itself,
`$0174` = 372 = 9600 baud at this machine's clock) and 8N1 framing (this
project's chipset only ever models 8N1 — see `chipset.rs`). Unlike the
pyamigadebug handshake (send `_` until echoed, then `\r`), this ROM
needs no handshake at all once the break-in poll has already been won by
a DEL byte: the `rom-wack` banner and register dump print unprompted,
and the debugger is immediately ready for a command line. There is no
observed `_` prompt character at any point.

**Line echo.** Every typed line is echoed back verbatim before being
acted on — sending `regs\r` produces `GUEST | regs` followed by the
register dump, not just the dump. This matters for a script's `WAIT`
directives: waiting for a command's *output* has to skip past the echo
of the command itself if the output could contain the same text.

**Space is swallowed.** This is the one genuine surprise. Sending
`a b\r` echoes back as `ab` — the literal space character is dropped
somewhere in ROMWack's line editor, not just failing to be echoed but
apparently never making it into the parsed line either (verified with
`--serial-log`, raw bytes inspected in hex: the space byte is sent, and
never appears in the guest's serial output at all). A multi-token
command line (`list 28c 8`) therefore arrives at the ROM as one fused
token (`list28c8`) and is rejected as `unknown symbol`. Whatever this
ROM's argument syntax is, it is not "separate tokens with a literal
space" the way pyamigadebug's reference implies for 1.x/2.x SAD-derived
monitors. This project has not found the real argument separator (colon,
comma, and no separator at all were not tried before this document was
written — flagged as a follow-up, not resolved here).

**Commands.** Sending `?\r` lists this ROM's actual command table:

```
GUEST | alter boot clear fill find go ig limit list regs reset resume set show user
```

Tried and their observed behaviour:

| Command | Behaviour observed |
|---|---|
| `regs` | Re-prints the same `PC:`/`SR:`/`USP:`/`SSP:`/`XCPT:`/`TASK:`/`DR:`/`AR:`/`SF:` block the banner opened with. |
| `<hex>\r` (e.g. `1000\r`) | A bare hex value is treated as a memory address: prints a `list`-style dump line at that address (word-aligned — an odd address like `0xAB` rounds down to `0xAA`), e.g. `00001000 0000 22E2 0000 22DE 0AF6 00F8 0414 0703  .... "...... "..^J......^D^T^G^C`. |
| bare `\r` | Repeats the memory dump at the *current* pointer (initially the faulted PC) — ROMWack appears to track a persistent examine cursor across commands, not just for bare hex entry. |
| `go` / `resume` | Both **resume guest execution** from the faulted PC — this is the one command category that actually changes what the guest does next, everything else only inspects it. See below for what happens after. |
| `list`, `show` (no argument) | Echoed, then no further output within the script's wait window — these commands almost certainly want an argument this project has not yet worked out (see the space-swallowing note above), not that they are broken. |
| `alter`, `clear`, `fill`, `find`, `ig`, `limit`, `reset`, `set`, `user` | Listed by `?` but not tried — out of scope for what this document set out to verify (reaching the debugger and confirming it is genuinely interactive), flagged as a follow-up for whoever next needs write/patch access from ROMWack. |

**After `go`/`resume`:** the guest does not crash again or loop forever
in the alert handler. Instruction execution continues from the faulted
PC, the CPU passes through a brief burst of activity (observed PC
excursion through `$FC648A`), and settles into a `STOP`-instruction idle
state at `$F8159C` — and critically, `--inspect` on that state finds the
same `ExecBase`, the same 45 initialised resident modules (including
`bootmenu` and `strap`), and every task parked in `TaskWait` with none
in `TaskReady`, that an *undisturbed* boot (no `--trigger-illegal-after-frames`
at all) reaches at the same frame count — right down to an identical
final PC (`0x00f8131c`) in one comparison run. In other words: this
ROM's `resume` genuinely un-wedges the machine and lets it converge back
onto the same idle boot-menu state a healthy boot reaches on its own.
This is corroborating evidence for, not a substitute for, the healthy-
idle picture `--inspect` already establishes (`introspect.rs`) — see the
caveat immediately below.

## The caveat: this observes a machine we deliberately crashed

`--trigger-illegal-after-frames` exists specifically to force a fault
that would not otherwise happen. Everything in this document — the
`rom-wack` banner, the register dump, the command exploration, even the
post-`resume` convergence onto the healthy idle state — happens only
because the host injected an illegal-instruction exception the guest
never would have hit on its own. **A stock, undisturbed Kickstart 3.2.2
boot still never writes to `SERDAT`.** ROMWack does not contradict that;
it is simply a different, deliberately-triggered code path that happens
to use the serial port for a reason unrelated to normal boot narration.

Put differently: this document proves Kickstart *can* narrate over
serial under the right (artificial) conditions, and that whatever it
says under those conditions is trustworthy enough to double-check
against `--inspect`'s independent guest-memory evidence (and the two
agree). It does not prove, and should not be read as claiming, that a
healthy boot narrates anything on its own.

## The Phase 1 exit criterion

`docs/combined-roadmap.md`'s Phase 1 exit criterion currently reads:
"Kickstart 3.2 reaches the strap and idles in the boot menu on both
harnesses, **observed via serial**". That was flagged elsewhere as
unachievable on the grounds that retail Kickstart never writes to
`SERDAT`. This document's findings partially overturn the premise (a
stock ROM *can* be made to narrate over serial — via ROMWack, under a
forced fault) but do not overturn the conclusion for the criterion as
worded, because the criterion is implicitly about the *undisturbed*
boot path, and this is not that.

**Recommendation:** do not change the exit criterion to route through a
deliberately-triggered ROMWack session — that would mean asserting
Phase 1 succeeded by first breaking the machine on purpose, which
inverts what the criterion is meant to demonstrate. Instead, replace
"observed via serial" with the evidence this project already has for
the undisturbed path and has now cross-checked twice (once by
`--inspect` alone, once by `--inspect` after a ROMWack `resume` landed
on the same state):

> **Exit:** Kickstart 3.2 reaches the strap and idles in the boot menu
> on both harnesses, evidenced by `--inspect`: a well-formed `ExecBase`,
> all 45 of this ROM's resident modules initialised (including `strap`
> and `bootmenu`), no real alert recorded (`LastAlert` still the
> uninitialised-fill sentinel, not a genuine `Alert()` call), and every
> task parked in `TaskWait` with none in `TaskReady` — sustained across
> repeated frame counts, not a one-shot snapshot. AROS's smoke test
> keeps its serial-narration gate unchanged (AROS does narrate on an
> undisturbed boot); this wording only replaces the Kickstart half.
> ROMWack break-in (`docs/serial-debugging.md`) is documented as
> corroborating evidence that Kickstart's serial path and the
> `--inspect` machinery agree with each other where they can both be
> checked, not as the exit evidence itself.

This keeps the Phase 1 exit criterion honest about what it is actually
observing on Kickstart (guest memory state, not guest narration) while
crediting ROMWack for what it actually demonstrated: that this
project's `--inspect` picture of "healthy and idle" and the ROM's own
internal debugger's picture of the same moment agree.

## Live sessions: `--serial-tcp`

Everything above drives the guest's serial port from a fixed,
pre-written `--serial-script`. `--serial-tcp <addr>` (e.g.
`127.0.0.1:1234`) is the live counterpart: it binds `addr` and bridges
it bidirectionally onto the same `SERDAT`/`SERDATR` wire -- a connected
TCP client sees guest output as it happens and can type input back,
including driving the ROMWack break-in above interactively rather than
from a script. See `crates/machine-hosted/src/serial_tcp.rs`'s module
doc comment for the full threading and client-lifecycle design; the
short version:

- It is a plain byte stream with no framing of its own -- exactly what
  `SERDAT`/`SERDATR` already carry. TCP was chosen for our own reasons
  (nothing beyond `std`; matches the shape Copperline, this project's
  oracle, already exposes its own guest serial as, so the same
  host-side tooling can point at either machine by address alone; no
  optional serial-library dependency), not because any one client
  needs TCP specifically -- AmiPilot's own `WireClient` is equally at
  home over TCP or a real/virtual serial port (`connect`/
  `connect_serial`), and a plain `nc`, a terminal, or a future debugger
  work here identically.
- Refused together with `--serial-script` (both would compete to
  supply host->guest bytes); composes fine with `--serial-log`, and
  guest output still reaches stdout as always.
- No client yet: guest output buffers (bounded, drop-oldest) rather
  than blocking or vanishing. Client connects mid-run: picked up on the
  bridge thread's next poll, no handshake needed. Disconnects: the
  bridge goes back to accepting: a client can reconnect and pick up
  where output buffering left off.
- Host->guest bytes are handed to the guest one per serviced frame,
  the same pace `SerialScript`'s `SEND` uses -- not because of any
  baud model (there isn't one), but because `push_serial_in_byte`
  raises the RBF interrupt once per accepted byte; draining a fast
  client's whole backlog into the guest's 32-byte queue in one gulp
  fires that many interrupts back to back and was confirmed, not just
  suspected, to wedge the ROMWack break-in below into an exception
  storm instead of ever reaching the debugger.
- An interactive session has no natural frame/instruction count to end
  it at: pass `--max-frames 0 --max-instructions 0` (both flags treat
  `0` as unlimited) and end the run from outside (Ctrl-C) instead.

`crates/machine-hosted/tests/real_rom.rs`'s
`kickstart_3_2_2_a1200_romwack_break_in_reaches_the_debugger_over_tcp`
reproduces the break-in above end to end over a real socket (a test
client connects, floods DEL, and watches for the `rom-wack` banner in
the child process's own stdout) -- the sharpest available proof that
this bridge's host->guest direction genuinely crosses a real socket
rather than only its own loopback unit tests
(`crates/machine-hosted/src/serial_tcp.rs`) talking to themselves.
