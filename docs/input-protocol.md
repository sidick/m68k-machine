# `input` register and event-queue contract

**Status:** host side implemented (`crates/machine-core/src/input.rs`),
driven from `machine-hosted` via `--input-script`
(`crates/machine-hosted/src/input_script.rs`). **No m68k driver and no
DiagArea boot ROM exist yet** -- this document is written *for* that
future driver, the same way `docs/hostblk-protocol.md` was written before
`hostblk`'s first driver existed, on the theory (borne out there) that
writing the contract down before any 68k code depends on it surfaces
problems while they are still cheap to fix.

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
| type | byte | [`ev::KEY_DOWN`]/`KEY_UP`/`POINTER_MOTION`/`BUTTON_DOWN`/`BUTTON_UP`, or `NONE` (`0`) when the queue is empty |
| code | byte | raw Amiga key code (`0x00`-`0x7F`) or button id (`0`=left, `1`=right, `2`=middle); `0` for pointer motion |
| qualifier | u16 | a caller-supplied modifier mask, passed through verbatim -- see §7 for what a driver must do with it, since this card does **not** compute it |
| x, y | i16 each | absolute pointer position, meaningful only for `POINTER_MOTION` |

## 5. Register map

Same idiom `hostblk.rs` established: one hot byte per 4-byte-aligned
slot, so a driver author sees one convention across every native card on
this machine. Anything not listed reads `0` and discards writes.

| Offset | Register | Width | Access | Notes |
|---|---|---|---|---|
| `0x00` | `EVENT_TYPE` | byte | R | head event's type, `0` if the queue is empty |
| `0x04` | `EVENT_CODE` | byte | R | raw key code or button id; `0` for pointer motion |
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
- **A key/button push that finds the queue full evicts the queued motion
  entry first**, if there is one: freeing a slot by discarding
  already-stale, about-to-be-superseded position data costs nothing the
  guest can observe, and is strictly better than losing a key-up.
- **Only once the queue is full of key/button events with no motion
  entry left to evict** does this card fall back to `hostblk`'s posture:
  drop the incoming event and count it in `EVENT_OVERFLOW`. This is the
  genuinely bad case -- it can drop a key-up -- but it now requires a
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
- `SLEEP <frames>` -- wait unconditionally before the next directive.

An out-of-range key code, button id, or coordinate is a **parse-time**
error (the script fails to load at all), not a silently dropped event --
the same posture `serial_script.rs` takes for an unrecognised directive.
Every directive except `SLEEP` fires immediately and moves on; there is
no `WAIT`-on-guest-output equivalent (`serial_script.rs`'s `WAIT`) yet,
because no driver exists to produce a guest reaction to wait for (§12).

## 11. What this increment does not include

- **No m68k driver, no DiagArea boot ROM.** This card cannot demonstrate
  a keypress reaching Intuition on its own; it exercises the card's
  host-side half only, the same honest limitation `hostblk`'s first
  increment stated. There is therefore no end-to-end guest test in this
  increment -- said plainly, not implied by omission.
- No `IEPointerPixel`/`Screen*` resolution logic (§6) -- host-side code
  has no notion of Intuition's screen list at all, and could not build
  one even for testing purposes without a running guest.
- No held-modifier/held-button bookkeeping (§7) -- this card is a dumb
  pass-through by design; that state belongs to the driver.
- No `IND_ADDEVENT` (V47's repeat-aware sibling of `IND_WRITEEVENT`) --
  this card's queue has no notion of key-repeat timing at all; a driver
  wanting repeat would need to synthesise it itself from raw down/up
  events, same as any driver built against plain `IND_WRITEEVENT`.

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
