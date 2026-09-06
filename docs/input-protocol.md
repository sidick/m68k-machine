# `input` register and event-queue contract

**Status:** host side implemented (`crates/machine-core/src/input.rs`),
driven from `machine-hosted` via `--input-script`
(`crates/machine-hosted/src/input_script.rs`), **and now a real m68k
driver and DiagArea boot ROM** (`m68k/input-rom/input-diagrom.s`,
`scripts/build-input-rom.sh`) -- verified end-to-end against a real
Kickstart 3.2.2 boot (`nondistribution/A1200.47.115.rom`): a
`--input-script MOVE` reaches `IntuitionBase->MouseX`/`MouseY` and
visibly moves the pointer sprite on a live Workbench screen. §13 below
records what a real driver found once this contract had to be lived
with, the same feedback loop `docs/hostblk-protocol.md` went through for
`hostblk`'s own driver.

**Context:** `docs/device-ledger.md` ("Not built -- decided
native-first", the "Keyboard / mouse" row this device fills in);
`docs/hostblk-protocol.md` (the sibling document this one's shape and
section numbering follow); `~/src/amipilot/server/src/action.c` (BSD
2-Clause, the project owner's own on-Amiga GUI automation server --
**not** a spec this card implements, but a source of hard-won facts
about how `input.device` injection actually behaves, cited throughout
§6-§8 below; read freely, unlike Copperline).

---

## 1. Identity

One Zorro III AUTOCONFIG board:

| Field | Value |
|---|---|
| `er_Manufacturer` | `0x07DB` (2011) -- the same NDK 3.2 `libraries/configregs.h` reserved "hacker" ID `hostblk` uses (`hostblk.rs`'s module docs carry the full story of why `0xFFFF` was tried and rejected by real Kickstart 3.2.2). This card was never at risk of that specific failure (it doesn't set `ERTF_MEMLIST`), but there is no reason to mint a second placeholder ID when product numbers already keep every board on this bus distinguishable. |
| `er_Product` | `3` -- distinct from `mirage` (`0`), `hostblk` (`1`) and `fastram` (`2`). |
| `er_Type` | `ERT_ZORROIII` (extended-table code 0 -> 16 MB window; no size bits of its own, same as `hostblk`/`fastram`) |
| `er_Flags` | `ERFF_ZORRO_III \| ERFF_EXTENDED` |
| `er_InitDiagVec` | `0` -- **no DiagArea in this increment.** `ERTF_DIAGVALID` is unset. Unlike `hostblk`, which carries a boot ROM proving Kickstart can run code from its DiagArea, this card cannot yet -- see §12. |
| Window size | 16 MB, of which only the first `0x30` bytes carry registers; the rest is unimplemented board space (reads `0`, writes discarded), same posture as `hostblk`/`mirage`. |

## 2. Why native, not the CIA-A/`JOY0DAT` legacy path

`device-ledger.md`'s own "Keyboard / mouse" entry has the full reasoning;
summarised here because it is the reason this card looks the way it
does. Legacy input -- the CIA-A keyboard handshake and `JOY0DAT` mouse
quadrature counters, both already half-built in `machine_core::chipset`
-- is cheap to finish, and finishing it anyway is exactly the reasoning
the ledger exists to interrupt. Input has no bootstrap dependency at
all: nothing in early Kickstart boot needs a mouse or a keyboard, and
this machine already reaches Workbench with neither, so it is the first
device where native-first costs nothing. A native input board also
carries its own driver in its own AUTOCONFIG DiagArea (once §12's future
increment adds one), so unlike the RTG bring-up case it needs nothing
installed on the guest.

## 3. The payoff: absolute pointer position, and why legacy input can't offer it

Quadrature counters are relative-only: a legacy mouse's reported motion
is always a delta, so a host pointer that can move independently of the
guest's notion of where it is (a window manager warping it, a tablet, a
remote session) drifts out of sync and needs grabbing to stay usable.
This card instead carries **absolute** coordinates for pointer motion,
which removes that whole class of problem -- the guest is always told
*where the pointer is*, never asked to integrate a stream of deltas
against a starting point it may not agree with the host about.

**This does not mean the wire event a driver builds is
`IECLASS_POINTERPOS`.** §6 below corrects an initial assumption that it
was.

## 4. The event queue

A bounded, host-to-guest FIFO, `#![no_std]`/no-alloc (`machine-core` has
no allocator): a fixed-capacity ring of 16 entries
(`input::QUEUE_CAPACITY`), each carrying:

| Field | Width | Notes |
|---|---|---|
| type | byte | [`ev::KEY_DOWN`]/`KEY_UP`/`POINTER_MOTION`/`BUTTON_DOWN`/`BUTTON_UP`/`CHAR_DOWN`/`CHAR_UP`, or `NONE` (`0`) when the queue is empty |
| code | byte | raw Amiga key code (`0x00`-`0x7F`), button id (`0`=left, `1`=right, `2`=middle), or a Latin-1 character byte for `CHAR_DOWN`/`CHAR_UP` (§15); `0` for pointer motion |
| qualifier | u16 | a caller-supplied modifier mask, passed through verbatim -- see §7 for what a driver must do with it, since this card does **not** compute it. Always `0` for `CHAR_DOWN`/`CHAR_UP`: a character carries no qualifier of its own, since a driver's `MapANSI()` is what derives one (§15) |
| x, y | i16 each | absolute pointer position, meaningful only for `POINTER_MOTION` |

## 5. Register map

Same idiom `hostblk.rs` established: one hot byte per 4-byte-aligned
slot, so a driver author sees one convention across every native card on
this machine. Anything not listed reads `0` and discards writes.

| Offset | Register | Width | Access | Notes |
|---|---|---|---|---|
| `0x00` | `EVENT_TYPE` | byte | R | head event's type, `0` if the queue is empty |
| `0x04` | `EVENT_CODE` | byte | R | raw key code, button id, or Latin-1 character byte (`CHAR_DOWN`/`CHAR_UP`, §15); `0` for pointer motion |
| `0x08` | `EVENT_QUALIFIER` | u32 | R | qualifier mask, low 16 bits meaningful |
| `0x0C` | `EVENT_X` | u32 | R | absolute X, sign-extended from an `i16` |
| `0x10` | `EVENT_Y` | u32 | R | absolute Y, same shape |
| `0x14` | `EVENT_ADVANCE` | byte | W | any write pops the head event, revealing the next one |
| `0x18` | `EVENT_COUNT` | u32 | R | entries currently queued (convenience; polling `EVENT_TYPE != 0` is sufficient) |
| `0x1C` | `EVENT_OVERFLOW` | u32 | R | free-running count of key/button events dropped because the queue was full (§9) |
| `0x20` | `INT_STATUS` | byte | RW | write-1-to-clear, gated on the queue being empty (§8) |
| `0x24` | `INT_ENABLE` | byte | RW | `1` lets `INT_STATUS` reach INT2; `0` (reset value) masks it |
| `0x28` | `CAPACITY` | u32 | R | queue depth in events -- **read this, do not hardcode it**, same reasoning as `hostblk::reg::SUBMIT_CAPACITY` |
| `0x2C` | `VERSION` | u32 | R | protocol version; `1` for this document |

Driver-side drain loop, the same shape `hostblk-protocol.md` §5
sketches:

```
while (EVENT_TYPE != 0) {
    UBYTE  ty   = EVENT_TYPE;
    UBYTE  code = EVENT_CODE;
    UWORD  qual = EVENT_QUALIFIER;
    WORD   x    = EVENT_X;
    WORD   y    = EVENT_Y;
    /* ... translate (ty, code, qual, x, y) into a real InputEvent,
     * per §6-§7, and WriteInputEvent() it ... */
    write(EVENT_ADVANCE, 0);   /* pop, reveal next */
}
write(INT_STATUS, 1);          /* only actually clears once the queue is empty */
```

## 6. Correction: which `InputEvent` class actually carries an absolute position

**This section exists because the original design brief for this card
was wrong on this point, and the correction arrived after the register
interface and event queue (§4-§5) were already built and tested against
the wrong assumption.** The queue's shape did not need to change; only
what a driver does with `EVENT_X`/`EVENT_Y` does. Verified against NDK
3.2 `devices/inputevent.h` directly, and against `~/src/amipilot`'s
`server/src/action.c` -- already synthesising real `input.device` events,
verified on Copperline, Amiberry and real RTG hardware.

**`IECLASS_RAWMOUSE` motion is relative-delta only.** There is no
absolute `RAWMOUSE` form at all. `inputevent.h`'s own framing matches
this once you notice it: `IECLASS_RAWMOUSE` is documented as "the raw
mouse report from the game port device", and a game port only ever
reports quadrature deltas in the first place -- there was never an
absolute form for this class to have.

**Absolute positioning is `IECLASS_NEWPOINTERPOS` (`0x13`) with
`IESUBCLASS_PIXEL` (`0x01`)**, whose `ie_EventAddress` names a `struct
IEPointerPixel`:

```c
struct IEPointerPixel {
    struct Screen *iepp_Screen;   /* pointer to an open screen */
    struct {                      /* pixel coordinates in iepp_Screen */
        WORD X;
        WORD Y;
    } iepp_Position;
};
```

exactly as RKM's documented `Devices/Dev_examples/Set_Mouse.c` does, and
as `action.c`'s `SendPointerPixel`/`AmipMoveMouseTo` do in practice:

```c
pix.iepp_Screen     = screen;
pix.iepp_Position.X = x;
pix.iepp_Position.Y = y;

ie.ie_Class        = IECLASS_NEWPOINTERPOS;
ie.ie_SubClass     = IESUBCLASS_PIXEL;
ie.ie_Code         = IECODE_NOBUTTON;
ie.ie_EventAddress = (APTR)&pix;
```

Plain `IECLASS_POINTERPOS` (`0x04`, no subclass) is real, and its own
`ie_X`/`ie_Y` genuinely are documented as "the pointer position for the
event" rather than a delta -- the original brief wasn't inventing
anything. But it is Intuition's own **report** class (what a driver's
requested position is reflected back as, once the input chain has
processed it), not the class an external driver **injects** to request
one. `NEWPOINTERPOS`/`PIXEL` is what `action.c` actually writes, and
what RKM's own worked example writes.

**The real design consequence for this card's driver:** absolute
positioning is **screen-relative** and needs a genuine `struct Screen *`,
not a bare X/Y pair. This card's own `EVENT_X`/`EVENT_Y` registers stay
exactly what they were designed to be -- absolute pixel coordinates,
chosen by the host, immune to the drift problem relative deltas have --
but a driver turning them into a real `InputEvent` must first **resolve
which screen** they are relative to (almost certainly
`IntuitionBase->ActiveScreen`, or the front public screen if there is a
reason to prefer that) before it can build the `IEPointerPixel`. That
resolution is 68k driver work with no host-side equivalent: this
increment has no notion of Intuition state at all, and could not
validate a screen pointer even if this card's protocol tried to carry
one. `IEPointerPixel::iepp_Position`'s `X`/`Y` being `WORD` (signed
16-bit) is why `NativeInput::push_pointer_motion` rejects anything
outside `i16::MIN..=i16::MAX` regardless of which class ultimately
carries it -- both `POINTERPOS` and `NEWPOINTERPOS`/`PIXEL` agree on that
range.

## 7. Two more injection details, recorded before they're rediscovered expensively

Both come from `action.c`'s comments, each describing a real bug that
cost real debugging time on a card with the same shape this one has (a
host synthesising `input.device` events with no real hardware behind
them).

**`IEQUALIFIER_RELATIVEMOUSE` is load-bearing on a synthetic `RAWMOUSE`
button event.** The real gameport handler sets this bit on every genuine
mouse event to mark `ie_X`/`ie_Y` as deltas. A synthetic button-only
event built with a zeroed position and *without* this bit reads as an
absolute jump to `(0, 0)` -- `action.c`'s comment records this costing
"most of a day" to diagnose (every click landed at the screen's top-left
corner regardless of where the pointer visibly was). This card's own
`BUTTON_DOWN`/`BUTTON_UP` events carry no position at all (a button press
doesn't move the pointer, so there is nothing for `EVENT_X`/`EVENT_Y` to
say), which means the future driver must remember to set
`IEQUALIFIER_RELATIVEMOUSE` itself when it builds the `RAWMOUSE` event --
this card provides nothing that would remind it.

**`ie_Qualifier` must carry the full currently-held state, not just this
event's own transition.** A key-down's qualifier includes every modifier
currently held (including the key itself, if it is a modifier); a
button-down's qualifier includes that button's own
`IEQUALIFIER_LEFTBUTTON`/`RBUTTON`/`MIDBUTTON` bit, and its matching
button-up's does not. `action.c`'s `SendRawMouseButton`/`StrikeKey` both
maintain this bookkeeping explicitly (`held` accumulated across a
sequence of synthetic keystrokes). **This card's own `EVENT_QUALIFIER`
is a deliberately dumb pass-through** of whatever value the host handed
`NativeInput::push_key`/`push_button` -- it does not track held state on
the guest's behalf. `machine-hosted`'s `--input-script` (§10) does not
track it either; every directive passes qualifier `0`. **A driver is
responsible for maintaining the running held-state itself** and
stamping it onto every `InputEvent` it constructs -- this is genuine
driver-side bookkeeping this card does not, and structurally cannot,
do for it, since only the driver's higher-level model knows what "held"
means across a sequence of events it alone observes in full.

`ie_TimeStamp` is deliberately left zeroed for `IND_WRITEEVENT`;
`action.c`'s comment cites the `input.device` autodoc being explicit that
`IND_WRITEEVENT` itself fills it in (V36+). This card carries no
timestamp field in its own event queue at all -- there would be nothing
for a driver to do with one.

## 8. Interrupt model: a latch, not a level, and why that differs from `hostblk`

`hostblk`'s INT2 is a pure level: `irq_pending()` is `INT_ENABLE != 0 &&
completion queue is non-empty`, and it clears itself the moment the
driver drains the queue via `COMPLETION_ADVANCE`. That works there
because every completion is consumed through the same register that
would otherwise keep the level asserted.

This card instead **latches** `INT_STATUS` to `1` on every successful or
dropped push, independent of `INT_ENABLE` -- the same "the request bit
always latches, the enable bit only gates whether the CPU sees it" split
real Amiga `INTREQ`/`INTENA` already use elsewhere on this machine, even
though this card shares no literal register with them. `INT_STATUS` is
write-1-to-clear rather than self-clearing for a reason `hostblk`'s model
doesn't need to consider: this card also needs to latch on an
**overflow** (§9) so a driver can notice one happened even if it is
polling the queue rather than watching INT2, and an overflow doesn't
correspond to any register a "drain it and the level clears" design
could hang the latch off.

**The write-1-to-clear is itself gated on the queue being empty.** A
write of `1` to `INT_STATUS` while `EVENT_COUNT` is still nonzero is
silently ignored, leaving the latch set. This matters because an
interrupt server that acknowledges before it has fully drained the queue
(a very natural bug -- "pop one event, ack, return") would otherwise
strand every event queued after the one it popped, with no further INT2
ever arriving to say so. Gating the clear on an empty queue makes that
failure mode structurally impossible rather than a rule a driver author
has to remember -- the same "never strand a request silently" instinct
`hostblk-protocol.md` §8 documents for its own completion queue, applied
here to the ack path instead of the depth path.

## 9. Overflow policy: coalesce motion, protect discrete events

The design question this document exists partly to answer, per the same
project convention `hostblk-protocol.md` §8 states: `hostblk`'s
submission ring drops-and-counts on overflow, which is correct there
because the driver still holds the `IORequest` it never handed off and
can retry. **Input has no such retry** -- the host, not the guest,
decides what goes in this queue, and once a key-up event is dropped
there is nobody left holding it to resend. A stuck-down key is a far
worse failure than a dropped keystroke; dropping a pointer-motion event
is nearly harmless, since any later motion event completely supersedes
it.

This card acts on that asymmetry structurally:

- **Pointer motion never grows the queue past one entry.** A new motion
  push overwrites an already-queued motion entry in place rather than
  appending a second one. At most one is ever in flight, so a flood of
  mouse movement between two driver polls costs one queue slot, not one
  per movement.
- **A key/button/character push that finds the queue full evicts the
  queued motion entry first**, if there is one: freeing a slot by
  discarding already-stale, about-to-be-superseded position data costs
  nothing the guest can observe, and is strictly better than losing a
  key-up. `CHAR_DOWN`/`CHAR_UP` (§15) are discrete events exactly like a
  key or button transition and go through this identical path -- a
  character event is protected from eviction by queued motion and can
  itself evict motion, never the reverse.
- **Only once the queue is full of key/button/character events with no
  motion entry left to evict** does this card fall back to `hostblk`'s
  posture: drop the incoming event and count it in `EVENT_OVERFLOW`. This
  is the genuinely bad case -- it can drop a key-up, or equally a
  character release, which leaves a key logically stuck down in the guest
  exactly the same way -- but it now requires a
  driver that has stopped draining the queue entirely for
  `QUEUE_CAPACITY` (16) consecutive discrete events with the host still
  producing more, which is already a broken driver by other measures. A
  future driver can use `EVENT_OVERFLOW` incrementing (and the interrupt
  firing on the drop itself, §8) as the trigger for a recovery action --
  forcing every key it believes is held down back up -- though that
  recovery logic is driver work, not built here.
- Dropped pointer-motion events (the rarer case: the queue full of
  key/button events with no room to append even a *first* motion entry)
  are **not** counted in `EVENT_OVERFLOW` at all, deliberately: that
  counter exists to flag the bad case above, and folding in harmless,
  self-superseding motion drops would make it noisy for no diagnostic
  benefit.

`QUEUE_CAPACITY` is 16 -- double `hostblk`'s queues (8) -- since the
whole point of the policy above is to make discrete-event overflow a
genuinely pathological case rather than a routine one, so the depth
itself can afford to stay modest.


### Coalescing must not reorder

Coalescing merges only into the **newest** queued entry when that entry
is itself a motion — that is, consecutive motion. It deliberately does
not merge into a motion sitting further back.

Merging anywhere would reorder a click relative to the motion that
positioned it. With `[motion(A), button]` queued, folding a later
`motion(B)` into slot 0 makes the driver move to B *before* pressing, so
the click lands where the host never aimed it. "Move to the gadget, then
click" is the entire purpose of an absolute pointer, so this is the
common case rather than a corner one. Consecutive motions genuinely do
supersede each other — nothing observes the intermediate position — and
that is where all the real coalescing pressure is anyway, since motion
arrives in runs.

Eviction is deliberately asymmetric with this. When the queue is
genuinely full of undrained discrete events, *any* queued motion may be
evicted to make room, accepting a possible mispositioned click. That is
the lesser harm: a dropped key-**up** leaves a key stuck down in the
guest indefinitely and corrupts every input after it, where a
mispositioned click is a single wrong action. Coalescing happens
constantly and can always be done without reordering, so it is; eviction
happens only under real pressure and buys correctness where it matters
more.

## 10. Host-side event source: `--input-script`

`machine-hosted` is headless (screenshots only, no window), so there is
nowhere for a keypress or click to come from except something scripted
-- `crates/machine-hosted/src/input_script.rs`, in the spirit of the
existing `--serial-script`. Directives, one per line:

- `KEYDOWN <code>` / `KEYUP <code>` -- decimal or `0x`-hex, `0x00`-`0x7F`.
- `MOVE <x> <y>` -- signed decimal, `i16::MIN..=i16::MAX`.
- `BUTTONDOWN <button>` / `BUTTONUP <button>` -- `LEFT`/`RIGHT`/`MIDDLE`
  (case-insensitive) or a numeric id.
- `CHARDOWN <char>` / `CHARUP <char>` -- push a `CHAR_DOWN`/`CHAR_UP`
  event (§15). `<char>` is a single literal character or a `0x`-prefixed
  Unicode code point, checked against the card's Latin-1 encoding at
  parse time; a literal space or other whitespace needs the code-point
  form since the directive line is itself split on whitespace.
- `TYPE "<string>"` -- expands at parse time to a `CHARDOWN`/`CHARUP`
  pair per character of a double-quoted string, in order -- the common
  case of typing text without spelling out two directives per character.
  `\"`/`\\`/`\n`/`\t` are the only recognised escapes.
- `SLEEP <frames>` -- wait unconditionally before the next directive.

An out-of-range key code, button id, coordinate, or a character with no
Latin-1 representation, is a **parse-time** error (the script fails to
load at all), not a silently dropped event -- the same posture
`serial_script.rs` takes for an unrecognised directive.
Every directive except `SLEEP` fires immediately and moves on; there is
no `WAIT`-on-guest-output equivalent (`serial_script.rs`'s `WAIT`) yet,
because no driver exists to produce a guest reaction to wait for (§12).

## 11. What this increment does not include

The m68k driver and DiagArea boot ROM now exist (§13) and close most of
what this section used to list as missing. What's left:

- No held-state *recovery* on `EVENT_OVERFLOW` -- the driver notices an
  overflow is possible (it maintains its own held-qualifier state
  regardless) but does not implement the "force every key back up" repair
  action §9 sketches as future work; nothing in this increment's testing
  ever drove the queue hard enough to hit that path.
- No `IND_ADDEVENT` (V47's repeat-aware sibling of `IND_WRITEEVENT`) --
  this card's queue has no notion of key-repeat timing at all; a driver
  wanting repeat would need to synthesise it itself from raw down/up
  events, same as any driver built against plain `IND_WRITEEVENT`.
- No `IESUBCLASS_TABLET`/`NEWTABLET` support -- this card only ever
  carries pixel coordinates (§6), so the driver only ever builds
  `IESUBCLASS_PIXEL`.
- No user remap table ahead of `MapANSI()` -- `~/src/amirfb`'s own
  proposal describes one as a later layer with no consumer yet either;
  this driver, like amirfb without it, has no way to reach a
  non-printable key (cursor keys, function keys, Return, Escape, ...)
  from a bare Latin-1 byte, which is exactly why `CHAR_DOWN`/`CHAR_UP`
  and `KEY_DOWN`/`KEY_UP` remain two separate event types rather than
  one -- a script (or a future host input source) that needs Return
  still has to send it as a raw key code, not a character.
- Key-down/key-up and button-down/button-up were exercised as a
  guest-stability smoke test when this driver was first built (the
  driver task survives them, keeps draining, and the machine keeps
  booting), which is not the same claim as "a keystroke arrives
  anywhere" -- §13's retrospective now records that plain `KEY_DOWN`/
  `KEY_UP` **was** independently verified end-to-end once the
  `CHAR_DOWN`/`CHAR_UP` increment needed a real guest fixture to test
  against anyway: a scripted raw `KEYDOWN 0x44`/`KEYUP 0x44` (Return) is
  what actually runs the `ECHO` command in
  `crates/machine-hosted/tests/real_rom.rs`'s `scripted_typing_in_a_
  shell_opened_three_double_clicks_deep_is_echoed`, alongside the typed
  text itself. `IDCMP_RAWKEY`/`IDCMP_MOUSEBUTTONS` reported back from a
  purpose-built guest fixture (§12's original framing) is still not
  built, but a real Shell accepting a real Return keypress is at least
  as convincing for the one case that mattered most to settle.

## 12. Acceptance testing the driver

When a driver exists, the natural end-to-end check is the one
`hostblk-protocol.md` §13 describes for `devsoak`: run against a real
guest task that reports what it received (e.g. via `IDCMP_RAWKEY`/
`IDCMP_MOUSEBUTTONS` on a test window, the same diagnostic shape
`action.c`'s own commit history used to find the `RELATIVEMOUSE` bug),
and confirm the reported code/qualifier/position match what
`--input-script` asked for. No such guest fixture exists in this
repository yet; building one is the natural first task for the driver
increment, not something to invent speculatively here.

## 13. Driver retrospective: what changed once real 68k code depended on this

`m68k/input-rom/input-diagrom.s` is the driver this document was written
for. This section records where the contract above held up unmodified,
and where it didn't -- the same honesty §6 already models for a
correction found *before* any driver existed; this one was found while
writing it.

**Confirmed exactly as designed, no surprises:**

- §4-§5's register map and drain loop needed no changes at all -- the
  driver's `IntHandler` drains it precisely as the pseudocode in §5
  shows, into a small software ring so the interrupt server never blocks
  (next bullet).
- §6's `IECLASS_NEWPOINTERPOS`/`IESUBCLASS_PIXEL`/`IEPointerPixel`
  correction was exactly right, confirmed against `devices/inputevent.h`
  directly while writing the driver, not just cited secondhand.
- §7's `IEQUALIFIER_RELATIVEMOUSE` and full-held-state requirements were
  both real: the driver maintains its own running modifier/button state
  (`ST_HELD_QUALIFIER`) and ignores this card's own `EVENT_QUALIFIER`
  register entirely, exactly as §7 says a driver must.
- §9's overflow/coalescing policy needed no driver-side workaround --
  the driver's own local software ring (16 entries, matching
  `QUEUE_CAPACITY`) never needed a different depth.

**The task/interrupt split, checked rather than assumed:** the brief
that produced this driver asked for the split to be verified, not taken
on faith. It's real: `OpenDevice`/`DoIO` (what `IND_WRITEEVENT` requires)
eventually call `Wait()`, which is documented unsafe from interrupt
context (Autodocs `exec.doc`), so `IntHandler` genuinely cannot do the
`IND_WRITEEVENT` call itself -- it can only drain the card's hardware
queue into RAM and `Signal()` a task (one of the few calls safe from
interrupt context), and a separate task (`TaskEntry`'s main loop) does
the actual `DoIO`. Building that task from `rt_Init` needed
`AddTask` directly, hand-building a `struct Task` -- `CreateTask()` is an
`amiga.lib` helper, not an `exec.library` LVO, so it isn't available to
ROM code with no C runtime linked in.

**A gap in §7 this driver's own construction exposed:** turning a raw
key code into "is this key itself a modifier, and if so which
`IEQUALIFIER_*` bit" needs a raw-keycode-to-modifier table, and **no NDK
3.2 header ships one** (`Include_H` has no `rawkeycodes.h`). The RKRM
Devices "keyboard.device" chapter's own keyboard-matrix figure -- the
one place this project's licensed reference material could have
supplied it -- turned out to be unrecoverable OCR garbage (that skill's
own provenance note: scanned rotated 180°). `modifier_bit_for_code` in
`input-diagrom.s` uses the long-standing, widely published Amiga
hardware assignment ($60-$67 = LSHIFT/RSHIFT/CAPSLOCK/CONTROL/LALT/RALT/
LCOMMAND/RCOMMAND), flagged in that routine's own comment as a
hardware-convention citation, **not** a header citation -- the one place
in the whole driver where that distinction had to be made explicitly.
If §7's held-qualifier bookkeeping is ever found to misfire against a
real Amiga keyboard, this table is the first thing to re-derive from
real hardware or a keymap dump rather than trust.

**`da_BootPoint` must be non-zero for the DiagArea to be copied at
all** -- this is not new information this document got wrong, but a fact
`hostblk`'s own driver increment already discovered the hard way
(`hostblk-diagrom.s`'s DiagArea comment, RKRM "Events At DIAG Time") that
this document never had occasion to restate, since the earlier,
driver-less increment of this card had no DiagArea at all. It matters
here because this card is *never* a `BootNode` (no bootable media), so
a first draft set `da_BootPoint` to 0 as "no boot routine" -- which
would have silently disabled the RAM copy, and with it `DiagEntry`,
the romtag, and the whole driver. `BootStub` (a bare, never-actually-
reached `rts`) exists purely to keep that field non-zero.

**Verification, both ways the brief asked for:**

- **Guest state.** `machine-hosted --inspect` now reports
  `IntuitionBase->MouseX`/`MouseY` (`crates/machine-hosted/src/
  introspect.rs`). Against a real Kickstart 3.2.2 boot to a genuine
  752×576 hires interlaced Workbench screen, `--input-script`'s `MOVE
  300 200` produced `MouseX 300  MouseY 400` -- `MouseY` reads back at
  *exactly* double the requested pixel Y, confirming this document's own
  note above about hires screens without needing to guess at the factor.
  Against a Picasso96 RTG screen whose actual open `Screen` turned out to
  be only 256 lines tall despite the board being configured for 640×480,
  a `MOVE 400 300` came back as `MouseX 400  MouseY 255` -- Intuition
  clamping Y to the real screen's height, not a driver bug (RKM's own
  `IEPointerPixel` doc: "Intuition will try to oblige, but there will be
  restrictions to positioning the pointer over offscreen pixels").
- **Visible effect.** The 752×576 hires screenshot at the `MouseY 400`
  data point above shows the real pointer sprite rendered at
  approximately the requested position on a live Workbench desktop, not
  stuck at the top-left corner or absent -- something that could only
  happen by the event actually reaching Intuition's input chain.

### Retrospective: the `CHAR_DOWN`/`CHAR_UP` (`MapANSI()`) increment

Confirmed exactly as §15 designed it, once written: `MapANSI()`'s
calling convention (`actual = MapANSI(string, count, buffer, length,
keyMap)` in `D0/A0,D0,A1,D1,A2`) matches `Include_H/inline/
keymap_protos.h`'s `__reg()` annotations exactly, the same authoritative
source (register annotations over autodoc prose) this file's header
already insists on for every other LVO call in it. `NUM_MAP_PAIRS = 3`
matches Autodocs `keymap.doc`'s own worked example almost verbatim
(`STIMSIZE`, commented "two dead keys, one key").

**A real bug this verification caught, not a design problem:**
`char_key_up` was missing the `mulu.w #HELD_ENTRY_SIZE,d6` multiply
`char_key_down` has, so it indexed `ST_HELD_TABLE` by the raw character
code instead of by that code's byte offset into the table -- for every
character above `HELD_ENTRY_SIZE` (8), this reads and clears some *other*
character's entry instead of its own. Scripting `TYPE "Hi"` exposed it
immediately: the guest echoed `H]` instead of `Hi`. What actually
happened -- traced by fixing the bug and confirming the symptom
disappeared, not guessed at -- is that `char_key_up('H')` hit the wrong
(unused) entry, found it not held, and returned without ever calling
`send_rawkey_raw`/`qualifier_release` for `H`'s own Shift press: Shift
was pressed to type `H` and then never released, either in this driver's
own `ST_HELD_QUALIFIER` bookkeeping or, more importantly, from real
`input.device`'s point of view. This is exactly the "silent failure"
category this project keeps a running count of (file header, project
convention) -- the driver kept running, the queue kept draining, nothing
crashed or logged an error; the only symptom was wrong text arriving in
the guest, on a card whose whole justification is that the host cannot
independently verify what the guest displays.

An initial, wrong diagnosis is worth recording too, in the same
"corrected in the open" spirit §6 already models: the first attempt at
fixing this assumed `a4`/`d6`/`d7` had failed to survive the
`jsr _LVOMapANSI(a6)` call despite the standard AmigaOS convention this
file already leans on elsewhere (`d2`-`d7`/`a2`-`a6` preserved across a
library call), and added code to re-derive them from `ST_MAPIN` after
the call rather than trust them across it. That fix compiled, changed
nothing observable, and was reverted once the real bug (in
`char_key_up`, not `char_key_down`'s post-`MapANSI()` code at all) was
found -- a reminder that "the convention might not hold here" is a much
more expensive hypothesis to reach for than "re-read the code for a
plain arithmetic omission", and should come second, not first.

**Verification, both ways the brief asked for, after the fix:**

- **Visible effect.** `crates/machine-hosted/tests/real_rom.rs`'s
  `scripted_typing_in_a_shell_opened_three_double_clicks_deep_is_echoed`
  drives the guest three double-clicks deep (`SYS:` → `System` → `Shell`,
  the same click primitives §13's pointer-motion work already proved)
  and types `ECHO Hi` via `TYPE`, followed by a real (not `TYPE`-
  synthesised) `KEYDOWN`/`KEYUP 0x44` for Return. The resulting
  screenshot shows `1.SYS:> ECHO Hi` followed by `Hi` on its own line --
  AmigaDOS's `Echo` command actually ran and printed back exactly what
  was typed, including the Shift-dependent capital `H` immediately
  followed by the unshifted `i` that exposed the bug above.
- **Guest-adjacent state.** `machine-hosted --inspect`'s `input state`
  section (`crates/machine-hosted/src/introspect.rs`) now also reports
  the card's live `EVENT_COUNT`/`EVENT_OVERFLOW`/`INT_STATUS` registers.
  After the run above, `EVENT_COUNT 0  EVENT_OVERFLOW 0` -- every one of
  the 14 `CHARDOWN`/`CHARUP` events `TYPE "ECHO Hi"` expands to, plus the
  Return keypress, was drained by the driver with nothing dropped.

**A queue-capacity nuance worth recording, not a bug:** an early attempt
at this same test used `TYPE "ECHO Hello"` (10 characters, 20 `CHARDOWN`/
`CHARUP` events). `--input-script` fires every event for one script line
in the same tick, before the guest's driver gets to run at all, so all 20
landed in the card's 16-entry queue in one instant -- the last two
characters (`l`, `o`) were dropped by the documented, correct overflow
policy (§9), and the guest echoed `HEL` instead of `Hello`. Nothing on
the driver side misbehaved; this is `--input-script`'s own instant-tick
delivery colliding with a `TYPE` string longer than `QUEUE_CAPACITY / 2`
characters, worth knowing before writing a longer scripted `TYPE` than
this document's own examples use.

## 14. `da_BootPoint` is not about being bootable

Worth stating plainly, because this project recorded it as two unrelated
traps before noticing it is one rule, and the field's name actively
misleads.

Two DiagArea failures were hit while building the storage and input
ROMs, both silent:

- `hostblk`: `da_Config = DAC_WORDWIDE|DAC_NEVER` — the `ConfigDev` was
  built correctly and the ROM read back correctly *live*, but
  `expansion.library` never copied the DiagArea into RAM.
- `input`: `da_BootPoint = 0`, which is honest since this card is never a
  boot node — same outcome, no copy, no `DiagEntry`, no romtag.

They look like separate gotchas. They are the same one. `libraries/
configregs.h` gives the `da_Config` timing bits as *when to call
`da_BootPoint`* — `DAC_CONFIGTIME` is commented "call da_BootPoint when
first configing the device" — and `da_BootPoint` itself as "where to
start". So:

> The DiagArea is copied into RAM only when there is code to run *and* a
> time at which to run it. `DAC_NEVER` removes the time; a zero
> `da_BootPoint` removes the code. Either way there is nothing to copy
> for, so nothing is copied.

The copy exists to make the ROM's code runnable — relocated and
"de-nibbleized" — not as a service to boards that merely want to be
present. **A board that wants its ROM to run at all needs both**, whether
or not it has anything to do with booting. Our input card is never a boot
node and never will be, and it still needs a non-zero `da_BootPoint`
pointing at a stub that is never reached, purely to satisfy this gate.

That loose end is now closed, and the answer is the narrower one.

The header's wording implies `DAC_CONFIGTIME` gets `da_BootPoint` called
at configuration time unconditionally. What Kickstart 3.2.2's strap
actually does (disassembled at `$FC746E` while chasing why `hostblk`
never booted) is narrower: the call is made for a **non-floppy
`BootNode`** whose `ConfigDev` carries `ERTF_DIAGVALID`, a kept diag copy
and `DAC_CONFIGTIME`, as that node's entire boot attempt.

Measured rather than argued. `BootStub` now writes `$B007B007` to a
marker cell at `input::BOOT_MARKER_OFFSET`, reported by `--inspect`. On a
booted machine with the input card attached and no boot node offered, it
reads **zero**: `da_BootPoint` is never called.

So on this Kickstart `DAC_CONFIGTIME` does **not** by itself get code run
at configuration time. A board with no `BootNode` gets its DiagArea
copied and its `da_DiagPoint` called, and that is all -- anything further
must come from a `struct Resident`'s `rt_Init`, which is what both this
ROM and `hostblk`'s do. A future ROM wanting code to run at configuration
time cannot get it this way.

## 15. Typing characters: what AmiRFB solved, and what this card now carries

`~/src/amirfb` (BSD 2-Clause, the project owner's) injects input into a
live AmigaOS from a VNC client, and its `src/amiga/rfb_server.c` solves
the keyboard problem this section originally recorded as a gap in this
card's design. It is a gap no longer: `CHAR_DOWN`/`CHAR_UP` now exist for
exactly the reason this section lays out. The reasoning below is kept
verbatim (it is *why* the feature exists), with a closing note on what is
now built versus what a driver still has to do with it.

**The problem.** This card's other event types carry *raw Amiga
keycodes*. That is fine for a scripted test which can hardcode them, and
wrong for anything else: raw keycodes are **physical key positions**, so
turning "the user typed `@`" into one requires knowing the guest's
*active keymap*, which the host cannot know and should not have to.

**The solution, and it is not a lookup table.** `keymap.library`'s
`MapANSI()` inverts a character into the rawkey-plus-qualifier
combination that produces it under whatever keymap is currently active —
the opposite direction to the more familiar `MapRawKey()`, confirmed to
work that way and available since V36. So the *driver* converts, not the
host, and it self-adapts to the guest's keymap for free.

AmiRFB layers it:

1. **Printable characters** go through `MapANSI()`.
2. **Non-printables** — cursor keys, function keys, Return, Escape, Tab,
   Delete, Backspace, Help, and the modifier keys themselves — use a
   small fixed table, because those *are* keymap-independent by
   construction, being physical positions with no character.

**And qualifiers must be reference-counted.** Two held keys can both
want Shift, so a key-up may not release it. AmiRFB presses the qualifiers
a key needs, holds them, treats auto-repeat as re-sending only the main
rawkey since the qualifiers are already down, and on key-up releases the
main rawkey while decrementing each qualifier — releasing one only when
nothing held still needs it. §7's rule that `ie_Qualifier` carries the
full current state is the *what*; this is the *how*.

Separately, AmiRFB independently uses the same raw keycode assignments
this project's ROM does for keys with no character. That is corroboration
from a second implementation, not a header citation — `Include_H` still
ships no `rawkeycodes.h`, so the caveat in
`m68k/input-rom/input-diagrom.s` stands unchanged.

### What this card now does

`ev::CHAR_DOWN`/`ev::CHAR_UP` (§4-§5) carry a **Latin-1 (ISO-8859-1)
byte** in `EVENT_CODE`. Latin-1 rather than UTF-8 or a wider code unit,
for three reasons:

- It is AmigaOS's own convention — `console.device`/`diskfont.library`
  text and `keymap.library`'s own `KeyMap` tables are already built
  around ISO-8859-1, so the byte this card hands a driver is already the
  byte the driver's own `MapANSI()` call expects, no translation needed.
- It matches AmiRFB's own solution: AmiRFB drives `MapANSI()` from X11
  keysyms, and X11 keysyms `0x20`-`0xFF` are defined to equal the Latin-1
  code point directly (`keysymdef.h`'s own comment: "identical to the
  Latin-1 sets"). A future host-side input source that already speaks
  X11 keysyms needs only a range check, not a lookup table, to feed this
  card.
- One byte keeps `EVENT_CODE` and the fixed-capacity, no-alloc queue
  entry's shape unchanged — a wider encoding would need either multiple
  queue slots per character or a new, wider register.

A Unicode character with no Latin-1 representation (anything above
U+00FF) is **rejected, not substituted**: `NativeInput::push_char`
returns `false` and queues nothing, and `--input-script`'s `CHARDOWN`/
`CHARUP`/`TYPE` reject it at **parse time**, the same "hostile input
fails cleanly" posture §11's other rejections already take. There is no
silent substitute character (`?`, `\u{FFFD}`) — a driver that received
one would type the wrong thing with no indication anything was lost.

**Down and up, not a synthesised tap.** `CHAR_DOWN` and `CHAR_UP` are
separate, independently queueable events, mirroring `KEY_DOWN`/`KEY_UP`'s
shape rather than collapsing a character into one instantaneous
keystroke. This is what lets a driver implement the "press the qualifiers
a character needs, hold them, treat repeat as re-sending only the main
rawkey, release on key-up" scheme AmiRFB uses (above) faithfully, instead
of being handed an event shape that already assumes a tap. A character
event is a **discrete** event for the overflow policy (§9), protected
from eviction by queued motion and able to evict motion itself, exactly
like a key or button event — a dropped `CHAR_UP` would leave a key
logically stuck down in the guest forever, the same failure a dropped
`KEY_UP` causes.

`--input-script` gained three directives to produce them: `CHARDOWN
<char>` / `CHARUP <char>` (§10) for the down/up pair directly, and `TYPE
"<string>"`, which expands at parse time to a `CHARDOWN`/`CHARUP` pair
per character — typing a string being the common case a script author
actually wants, rather than spelling out two directives per letter.

### What a driver still has to do

**This section described future work; §13's retrospective now records
what actually happened building it.** `m68k/input-rom/input-diagrom.s`'s
`char_key_down`/`char_key_up` drain `CHAR_DOWN`/`CHAR_UP`, call
`MapANSI()` (opened via `keymap.library`, the same non-fatal
open-and-retry idiom `input.device`/`intuition.library` already used) to
invert the Latin-1 byte into a rawkey-plus-qualifier combination under
the guest's active keymap, and `qualifier_hold`/`qualifier_release` apply
AmiRFB's press/hold/repeat/release-with-refcounted-qualifiers scheme
described above when building the resulting `IECLASS_RAWKEY`
`InputEvent`s — verified end-to-end against a real Kickstart 3.2.2 boot,
not just assembled and trusted (§13).
