//! `input` — the host side of the native input card (device ledger,
//! "Keyboard / mouse: not built... native input board").
//!
//! # Why native, not the CIA-A/`JOY0DAT` legacy path
//!
//! `docs/device-ledger.md`'s "Not built — decided native-first" entry
//! records the reasoning this module implements: legacy input (the CIA-A
//! keyboard handshake, `JOY0DAT` mouse quadrature counters -- both
//! already half-built in [`crate::chipset`]: `mouse_delta`, `mouse_button`,
//! and a verified CIA keyboard encoding) is cheap to finish, and
//! finishing it anyway is exactly the reasoning that ledger exists to
//! interrupt. Input has no bootstrap dependency at all -- nothing in
//! early Kickstart boot needs a mouse or a keyboard, and this machine
//! already reaches Workbench with neither -- so it is the first device
//! where native-first costs nothing.
//!
//! This increment builds the host side only: this card's register
//! interface and event queue, plus [`NativeInput::push_key`]/
//! [`NativeInput::push_button`]/[`NativeInput::push_pointer_motion`] as
//! the host-side way to feed it. **No m68k driver and no DiagArea ROM
//! yet** -- those follow the way they did for `hostblk` (register
//! interface and transfer engine first, driver and boot ROM as later
//! increments). There is therefore no end-to-end guest test here, the
//! same honest limitation `hostblk`'s first increment stated.
//!
//! # What an AmigaOS driver gets that legacy input cannot
//!
//! A driver for this card would register with `input.device` and inject
//! events with `IND_WRITEEVENT` (`devices/input.h`, NDK 3.2:
//! `#define IND_WRITEEVENT (CMD_NONSTD+2)`), the same mechanism tablet
//! and remote-control drivers use, carrying a `struct InputEvent`
//! (`devices/inputevent.h`).
//!
//! **Correction to this module's first draft, found before any driver
//! was written (`docs/input-protocol.md` records the same correction for
//! whoever writes one):** the original brief's description --
//! "`IECLASS_POINTERPOS` carries an absolute pointer position" -- is
//! wrong about which class to *inject*, even though the header text is
//! consistent with it in isolation. Verified two ways: against the
//! headers directly, and against `~/src/amipilot`'s `server/src/
//! action.c` (BSD 2-Clause, the project owner's own, already
//! synthesising real `input.device` events verified on Copperline,
//! Amiberry and real RTG hardware) --
//!
//! - **`IECLASS_RAWMOUSE` motion is relative-delta only.** There is no
//!   absolute `RAWMOUSE` form at all (`action.c`'s
//!   `SendPointerPixel`comment, matching `inputevent.h`'s own framing of
//!   `RAWMOUSE` as "the raw mouse report from the game port device" --
//!   a game port only ever reports quadrature deltas in the first
//!   place).
//! - **Absolute positioning is `IECLASS_NEWPOINTERPOS` with
//!   `IESUBCLASS_PIXEL`** (`0x13`/`0x01` in `inputevent.h`), whose
//!   `ie_EventAddress` points at a `struct IEPointerPixel`:
//!   ```text
//!   struct IEPointerPixel {
//!       struct Screen *iepp_Screen;   /* pointer to an open screen */
//!       struct { WORD X; WORD Y; } iepp_Position; /* pixel coordinates in iepp_Screen */
//!   };
//!   ```
//!   exactly as RKM's documented `Devices/Dev_examples/Set_Mouse.c` does,
//!   and as `action.c`'s `SendPointerPixel`/`AmipMoveMouseTo` do in
//!   practice. Plain `IECLASS_POINTERPOS` (`0x04`, no subclass) is real
//!   and its `ie_X`/`ie_Y` genuinely are a position rather than a delta
//!   per the header's own comment -- but it is Intuition's own *report*
//!   class (what a driver's position lands as, once processed), not the
//!   class a driver *injects* to request one. `NEWPOINTERPOS`/`PIXEL`
//!   is what an external driver actually writes.
//!
//! **The real design consequence:** absolute positioning is
//! **screen-relative** and needs a genuine `struct Screen *`, not a
//! bare X/Y pair -- something this card's own register file (a `u32`
//! pair, no notion of a Screen) cannot carry, and was never going to:
//! resolving *which* screen (the active one, almost certainly --
//! `IntuitionBase->ActiveScreen`, or the front public screen) is 68k
//! driver work with no host-side equivalent, since this increment has
//! no notion of Intuition state at all. [`reg::EVENT_X`]/[`reg::EVENT_Y`]
//! still carry the design brief's original intent -- absolute pixel
//! coordinates, chosen by the host, immune to the drift problem relative
//! deltas have -- but a driver turning them into a real `InputEvent`
//! must resolve a `Screen*` to name, not just copy two words across.
//! `docs/input-protocol.md` §6 spells this out for whoever writes that
//! driver, so it isn't rediscovered the hard way.
//!
//! One thing the header confirms regardless of which class carries it:
//! `IEPointerPixel::iepp_Position`'s `X`/`Y` are `WORD` -- **signed
//! 16-bit** -- matching plain `IECLASS_POINTERPOS`'s `ie_X`/`ie_Y`. Both
//! paths agree on the range, which is why [`NativeInput::
//! push_pointer_motion`] rejects anything outside `i16::MIN..=i16::MAX`
//! regardless of which `InputEvent` shape a future driver ultimately
//! builds (see "Hostile input" below).
//!
//! **Two more injection details a driver will need, also recorded in
//! `action.c` the hard way (and now here, before they're rediscovered
//! expensively a second time):**
//!
//! - **`IEQUALIFIER_RELATIVEMOUSE` is load-bearing on a synthetic
//!   `RAWMOUSE` button event.** The real gameport handler sets it on
//!   every genuine mouse event to mark `ie_X`/`ie_Y` as deltas; a
//!   synthetic button-only event built with a zeroed position and
//!   *without* this bit reads as an absolute jump to `(0, 0)` --
//!   `action.c`'s comment records this costing "most of a day" to
//!   diagnose. This card's own [`ev::BUTTON_DOWN`]/[`ev::BUTTON_UP`]
//!   carry no position at all (by design -- a button press doesn't move
//!   the pointer), so the eventual driver must remember to set this bit
//!   itself when it builds the `RAWMOUSE` event, not copy anything this
//!   card provides.
//! - **`ie_Qualifier` must carry the full currently-held state, not
//!   just this event's own transition.** A key-down's qualifier includes
//!   every modifier held *including this key if it is itself a
//!   modifier*; a button-down's qualifier includes that button's
//!   `IEQUALIFIER_LEFTBUTTON`/`RBUTTON`/`MIDBUTTON` bit, its matching
//!   button-up's does not. This card's own [`reg::EVENT_QUALIFIER`] is a
//!   deliberately dumb pass-through of whatever the host handed
//!   [`NativeInput::push_key`]/[`push_button`](NativeInput::push_button)
//!   -- it does not track held state itself -- so a driver is
//!   responsible for maintaining that running state and stamping it
//!   onto every `InputEvent` it builds, not for trusting this register
//!   to already have it. `docs/input-protocol.md` §7 names this
//!   explicitly as driver-side bookkeeping this card does not do.
//!
//! `ie_TimeStamp` is deliberately left zeroed for `IND_WRITEEVENT` --
//! `action.c`'s comment cites the `input.device` autodoc being explicit
//! that `IND_WRITEEVENT` fills it in (V36+) -- so this card carries no
//! timestamp field at all; there would be nothing for a driver to do
//! with one.
//!
//! Quadrature counters (the legacy path) are relative-only, so a legacy
//! mouse drifts out of sync with a host pointer that can move
//! independently (a window manager warping it, a tablet, a remote
//! session) and needs grabbing to stay usable. An absolute report
//! removes that whole class of problem, which is the actual payoff for
//! building this card at all -- unchanged by the correction above; only
//! *which* `InputEvent` class carries it changes.
//!
//! Raw key codes: `devices/inputevent.h` documents
//! `IECODE_KEY_CODE_FIRST`/`_LAST` as `0x00`/`0x77` and
//! `IECODE_COMM_CODE_FIRST`/`_LAST` as `0x78`/`0x7F`, with
//! `IECODE_UP_PREFIX = 0x80` as the bit `input.device` itself sets on a
//! `IECLASS_RAWKEY` event's `ie_Code` to mark a key-up. This card does
//! **not** reuse that bit-7-as-up-flag convention on its own
//! `EVENT_CODE` register -- up/down is this card's own [`ev::KEY_UP`]/
//! [`ev::KEY_DOWN`] event-type byte instead (see "Register map" below) --
//! but the header's `0x00..=0x7F` range for a defined raw key code is
//! exactly why [`MAX_RAW_KEYCODE`] is `0x7F`: a driver building an
//! `IECLASS_RAWKEY` `InputEvent` from this card's `EVENT_CODE` needs a
//! code that still fits in 7 bits once it adds its own up/down bit back
//! on top.
//!
//! `ie_Qualifier` is `UWORD` and the `IEQUALIFIER_*` bits (`LSHIFT`,
//! `RSHIFT`, `CAPSLOCK`, `CONTROL`, `LALT`, `RALT`, ... `RELATIVEMOUSE`)
//! are all within the low 16 bits, so this card's `EVENT_QUALIFIER`
//! register carries the qualifier mask verbatim in its low 16 bits with
//! no translation needed.
//!
//! # Register map: the same idiom `hostblk.rs` established
//!
//! One hot byte per 4-byte-aligned slot (`hostblk.rs`'s convention,
//! itself inherited from `mirage.rs`): a `move.b`/`move.l` compiler
//! emission naturally lands a small constant in the low byte of a
//! longword-sized slot, so a driver author sees one idiom across every
//! native card on this machine rather than three. Anything not named in
//! [`reg`] reads `0` and discards writes, the same "unimplemented board
//! space" posture `hostblk.rs`/`mirage.rs` both take.
//!
//! | Offset | Register | Width | Access | Notes |
//! |---|---|---|---|---|
//! | `0x00` | [`reg::EVENT_TYPE`] | byte | R | head event's type ([`ev`]), `0` ([`ev::NONE`]) if the queue is empty |
//! | `0x04` | [`reg::EVENT_CODE`] | byte | R | raw key code or button id; `0` for pointer motion |
//! | `0x08` | [`reg::EVENT_QUALIFIER`] | u32 | R | qualifier mask, low 16 bits meaningful (`IEQUALIFIER_*`) |
//! | `0x0C` | [`reg::EVENT_X`] | u32 | R | absolute X, sign-extended from the `i16` a driver hands to `ie_X` |
//! | `0x10` | [`reg::EVENT_Y`] | u32 | R | absolute Y, same shape |
//! | `0x14` | [`reg::EVENT_ADVANCE`] | byte | W | any write pops the head event, revealing the next one |
//! | `0x18` | [`reg::EVENT_COUNT`] | u32 | R | entries currently queued (convenience; polling `EVENT_TYPE != 0` is sufficient) |
//! | `0x1C` | [`reg::EVENT_OVERFLOW`] | u32 | R | free-running count of key/button events dropped because the queue was full (see "Overflow policy") |
//! | `0x20` | [`reg::INT_STATUS`] | byte | RW | write-1-to-clear; see "Interrupt model" |
//! | `0x24` | [`reg::INT_ENABLE`] | byte | RW | `1` lets [`reg::INT_STATUS`] reach INT2; `0` (reset value) masks it |
//! | `0x28` | [`reg::CAPACITY`] | u32 | R | queue depth in events -- **read this, do not hardcode it**, same reasoning as `hostblk::reg::SUBMIT_CAPACITY` |
//! | `0x2C` | [`reg::VERSION`] | u32 | R | protocol version; `1` for this document |
//!
//! # Interrupt model: a latch, not a level -- and why that differs from `hostblk`
//!
//! `hostblk`'s INT2 is a pure level: `irq_pending()` is
//! `int_enable != 0 && completion_queue is non-empty`, and it clears
//! itself the moment the driver drains the queue via
//! `COMPLETION_ADVANCE`. That works there because every completion is
//! consumed through the same register that would otherwise keep the
//! level asserted.
//!
//! This card instead latches [`reg::INT_STATUS`] to `1` on **every**
//! successful or dropped push (module-internal [`NativeInput::latch`]),
//! independent of [`reg::INT_ENABLE`] -- the same "the request bit
//! always latches, the enable bit only gates whether the CPU sees it"
//! split real Amiga `INTREQ`/`INTENA` already use elsewhere on this
//! machine ([`crate::chipset`]), even though this card does not share
//! that literal register. `reg::INT_STATUS` is write-1-to-clear rather
//! than self-clearing, for a reason `hostblk`'s model doesn't need to
//! consider: this card also needs to latch on an **overflow** (a dropped
//! key/button event, "Overflow policy" below) so a driver can notice one
//! happened even if it was polling the queue and not just watching INT2
//! -- and an overflow doesn't correspond to any register a "drain it and
//! the level clears" design could hang the latch off.
//!
//! **The write-1-to-clear is itself gated on the queue being empty**
//! ([`NativeInput::write`]'s [`reg::INT_STATUS`] arm): a write of `1`
//! while [`reg::EVENT_COUNT`] is still nonzero is silently ignored,
//! leaving the latch set. Undocumented, this would be a real trap: an
//! interrupt server that acknowledges before it has fully drained the
//! queue (a very natural bug -- "pop one event, ack, return") would
//! strand every event queued after the one it popped, with no further
//! INT2 ever arriving to say so, and no test that only checks "does an
//! ack clear the flag" would catch it. Gating the clear on an empty
//! queue makes that failure mode structurally impossible instead of
//! documenting "drain fully before you ack" as a rule a driver author
//! has to remember -- the same "never strand a request silently" instinct
//! behind `hostblk`'s completion-queue backpressure (`hostblk.rs`
//! module docs, "The completion queue"), applied to the ack path instead
//! of the depth path.
//!
//! # Overflow policy: coalesce motion, protect discrete events
//!
//! **The design question the brief asked to be reasoned about, not
//! guessed.** `hostblk`'s submission ring drops-and-counts on overflow,
//! which is correct there because the driver still holds the
//! `IORequest` it never handed off and can just retry. Input has no such
//! retry: the host, not the guest, decides what goes in this queue, and
//! once a key-up event is dropped there is nobody left holding it to
//! resend. A stuck-down key is a far worse failure than a dropped
//! keystroke -- it corrupts guest state indefinitely rather than just
//! losing one input -- while dropping a pointer-motion event is nearly
//! harmless, since any later motion event completely supersedes it (the
//! guest only ever cares where the pointer *is*, never the path it took
//! to get there).
//!
//! This module acts on that asymmetry structurally, not just by
//! documenting a promise:
//!
//! - **Pointer motion never grows the queue past one entry.**
//!   [`NativeInput::push_pointer_motion`] scans the queue
//!   ([`EventQueue::motion_index`]) for an already-queued
//!   [`ev::POINTER_MOTION`] entry and overwrites it in place
//!   ([`EventQueue::overwrite`]) rather than appending a second one. At
//!   most one motion event is ever in flight, so a flood of mouse
//!   movement between two driver polls costs one queue slot, not one
//!   slot per movement -- which is also why a key/button flood can never
//!   be crowded out by motion the way it could with a single undifferentiated
//!   queue.
//! - **A key/button push that finds the queue full evicts the queued
//!   motion entry first**, if there is one
//!   ([`NativeInput::push_discrete`]): freeing a slot by discarding
//!   already-stale, about-to-be-superseded position data costs nothing
//!   the guest can observe, and it is strictly better than losing a
//!   key-up.
//! - **Only once the queue is full of key/button events with no motion
//!   entry left to evict** does this module fall back to `hostblk`'s
//!   posture: drop the incoming event and count it in
//!   [`reg::EVENT_OVERFLOW`] ([`NativeInput::overflow`]). This is the
//!   genuinely bad case -- it can drop a key-up -- but it now requires a
//!   driver that has stopped draining the queue entirely for
//!   [`QUEUE_CAPACITY`] consecutive discrete events with the host still
//!   producing more, which is already a broken driver by other measures
//!   (a task not servicing its interrupt server at all). The design
//!   accepts this residual risk rather than inventing a synchronous
//!   backpressure mechanism input has no natural equivalent of (unlike
//!   `hostblk`'s doorbell, nothing on the guest side is "waiting for
//!   room" to retry into) -- and the overflow counter, plus the latch
//!   firing on the drop itself (module docs above), gives a future
//!   driver the hook to notice and force every key it believes is held
//!   down back up as a recovery action. That recovery is 68k driver
//!   work, not built here (this increment has no driver at all), but the
//!   host-side hook it would need already exists.
//! - Dropped pointer-motion events (the queue full of key/button events
//!   with no room to append a *first* motion entry) are **not** counted
//!   in [`reg::EVENT_OVERFLOW`] at all -- deliberately: that counter
//!   exists so a driver can detect the bad case above, and folding in
//!   harmless, self-superseding motion drops would make it noisy for no
//!   diagnostic benefit.
//!
//! [`QUEUE_CAPACITY`] is 16 -- one more than `hostblk`'s queues (8),
//! since this module's whole point is to make discrete-event overflow a
//! genuinely pathological case rather than one three fast keystrokes and
//! an unlucky scheduler tick could reach.
//!
//! # No deferred completion, unlike `hostblk`
//!
//! `hostblk`'s doorbell write does no I/O inline because a slow host
//! disk (or a future OPFS backend) cannot always answer synchronously.
//! Nothing about accepting a key press or a pointer position has an
//! equivalent asynchronous boundary -- [`NativeInput::push_key`] and
//! friends are host-called, not guest-triggered, and pushing onto a
//! fixed-size ring is unconditionally fast -- so there is no `tick()`
//! here and no completion queue. The register file below is the entire
//! device.
//!
//! # Hostile input
//!
//! Nothing on this card's *guest*-writable registers can be hostile in
//! the way `hostblk`'s descriptor pointer is -- there is no guest-supplied
//! address here, no length, no offset, nothing this module could be
//! tricked into reading or writing out of bounds. The one register write
//! with any room for misuse, [`reg::INT_STATUS`], already fails cleanly
//! by construction (an ack while the queue is non-empty is simply
//! ignored, "Interrupt model" above) rather than by a validation branch
//! that could be gotten wrong.
//!
//! The real hostile-input surface here is the **host-side** push API,
//! since a bad `--input-script` line (`crate` `machine-hosted`'s job, not
//! this module's) is exactly as "untrusted" from this module's
//! perspective as a guest write would be from `hostblk`'s:
//!
//! - [`NativeInput::push_key`]/[`push_button`](NativeInput::push_button)
//!   reject a code above [`MAX_RAW_KEYCODE`]/[`MAX_BUTTON`] by returning
//!   `false` and touching nothing -- never a panic, never a queued event
//!   with a code a driver couldn't have produced from a real header
//!   constant.
//! - [`NativeInput::push_pointer_motion`] rejects `x`/`y` outside
//!   `i16::MIN..=i16::MAX` the same way, per the header finding above
//!   (`ie_X`/`ie_Y` are `WORD`).
//!
//! `machine-hosted`'s input-script parser (`crate::input_script`, not
//! this module) is the other half of "a bogus event type fails cleanly":
//! an unrecognised directive is a parse error at load time, the same
//! posture `serial_script.rs` already established, rather than this
//! module ever being asked to interpret one.

use crate::autoconfig::{BoardSpec, ERT_ZORROIII};

/// Reuses `hostblk`'s reserved manufacturer ID rather than minting a
/// second placeholder: both are the same NDK 3.2 `libraries/configregs.h`
/// "hacker" ID (`$7DB`, decimal 2011) reserved for test use, and
/// `hostblk.rs`'s own module docs already carry the full story of why
/// `0xFFFF` was tried first and rejected (real Kickstart 3.2.2 silently
/// drops the AUTOCONFIG base-address write for a board asking to join the
/// memory list with that manufacturer -- this card doesn't set
/// `ERTF_MEMLIST` so it was never at risk, but there is no reason to
/// reinvent a second stand-in ID when product numbers already keep every
/// board on this bus distinguishable). Still a stand-in, same caveat as
/// `hostblk::MANUFACTURER`: a real registered number is needed before
/// this ships on hardware.
pub const MANUFACTURER: u16 = crate::hostblk::MANUFACTURER;

/// This card's product number under [`MANUFACTURER`] -- distinct from
/// `mirage::PRODUCT` (`0`), `hostblk::PRODUCT` (`1`) and
/// `fastram::PRODUCT` (`2`), per the brief.
pub const PRODUCT: u8 = 3;

/// The Zorro III AUTOCONFIG window: 16 MB, "extended-table code 0" --
/// see `graffity.rs`'s module docs (or `hostblk.rs`'s, which reuses the
/// same explanation) for why a 16 MB Zorro III board's `er_Type` carries
/// no size bits of its own. This card's register file uses only its
/// first `0x30` bytes; the rest is unimplemented board space, the same
/// posture `hostblk.rs`/`mirage.rs` take for their own unused tails.
pub const WINDOW_BYTES: u32 = 0x0100_0000;

/// `er_Flags` bit 4: this is a genuine Zorro III board (`graffity.rs`/
/// `hostblk.rs` module docs).
const ERFF_ZORRO_III: u8 = 1 << 4;
/// `er_Flags` bit 5: `er_Type`'s size bits index the 16 MB-1 GB extended
/// table rather than Zorro II's 64 KB-8 MB one.
const ERFF_EXTENDED: u8 = 1 << 5;

/// How many events [`NativeInput`] holds before falling back to dropping
/// one -- see the module docs' "Overflow policy". One more than
/// `hostblk`'s queues (8): this module's whole design bends over
/// backwards to make hitting this limit at all a pathological case
/// rather than a routine one, so the depth itself can afford to be
/// modest.
pub const QUEUE_CAPACITY: usize = 16;

/// The value [`reg::VERSION`] reports (module docs).
pub const PROTOCOL_VERSION: u32 = 1;

/// The highest raw key code this card accepts, per NDK 3.2
/// `devices/inputevent.h`: `IECODE_KEY_CODE_LAST` is `0x77` and
/// `IECODE_COMM_CODE_LAST` is `0x7F`, so `0x00..=0x7F` covers every
/// defined raw key code with room for a driver to add its own up/down
/// bit (`IECODE_UP_PREFIX = 0x80`) on top without overflowing a byte.
pub const MAX_RAW_KEYCODE: u8 = 0x7F;

/// [`NativeInput::push_button`]'s button-id namespace. This card invents
/// its own numbering rather than reusing `devices/inputevent.h`'s
/// `IECODE_LBUTTON`/`_RBUTTON`/`_MBUTTON` (`0x68`-`0x6A`): those are
/// `IECLASS_RAWMOUSE` codes, a class this card never emits (no driver
/// exists yet to decide what class/subclass a button press becomes), so
/// picking small dense values here costs nothing and keeps
/// [`reg::EVENT_CODE`] readable during development.
pub mod button {
    pub const LEFT: u8 = 0;
    pub const RIGHT: u8 = 1;
    pub const MIDDLE: u8 = 2;
}

/// The highest button id [`NativeInput::push_button`] accepts.
pub const MAX_BUTTON: u8 = button::MIDDLE;

/// [`reg::EVENT_TYPE`] values.
pub mod ev {
    /// The queue is empty; [`super::reg::EVENT_CODE`]/`EVENT_QUALIFIER`/
    /// `EVENT_X`/`EVENT_Y` all read `0` alongside this.
    pub const NONE: u8 = 0;
    pub const KEY_DOWN: u8 = 1;
    pub const KEY_UP: u8 = 2;
    /// Absolute pointer position -- see the module docs' `InputEvent`
    /// discussion. `EVENT_CODE` is `0` for this event type.
    pub const POINTER_MOTION: u8 = 3;
    pub const BUTTON_DOWN: u8 = 4;
    pub const BUTTON_UP: u8 = 5;
}

/// Register offsets within this card's AUTOCONFIG window. See the module
/// docs' register-map table for width/access/semantics.
pub mod reg {
    pub const EVENT_TYPE: u32 = 0x00;
    pub const EVENT_CODE: u32 = 0x04;
    pub const EVENT_QUALIFIER: u32 = 0x08;
    pub const EVENT_X: u32 = 0x0C;
    pub const EVENT_Y: u32 = 0x10;
    pub const EVENT_ADVANCE: u32 = 0x14;
    pub const EVENT_COUNT: u32 = 0x18;
    pub const EVENT_OVERFLOW: u32 = 0x1C;
    pub const INT_STATUS: u32 = 0x20;
    pub const INT_ENABLE: u32 = 0x24;
    pub const CAPACITY: u32 = 0x28;
    pub const VERSION: u32 = 0x2C;
}

/// One queued event, in the shape the register file reads it back in --
/// not `InputEvent` itself (this module carries no notion of
/// `ie_Class`/`ie_SubClass`/`ie_NextEvent`/timestamps; that translation
/// is 68k driver work that doesn't exist yet).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct QueuedEvent {
    ty: u8,
    code: u8,
    qualifier: u16,
    x: i32,
    y: i32,
}

/// A fixed-capacity FIFO with two extra operations plain `Ring` (as
/// `hostblk.rs` defines it) doesn't need: finding and removing an
/// arbitrary queued element, both used by the overflow policy above.
/// `#![no_std]`/no-alloc, like every other queue on this bus.
struct EventQueue {
    buf: [QueuedEvent; QUEUE_CAPACITY],
    head: usize,
    len: usize,
}

impl EventQueue {
    fn new() -> Self {
        Self {
            buf: [QueuedEvent::default(); QUEUE_CAPACITY],
            head: 0,
            len: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn is_full(&self) -> bool {
        self.len == QUEUE_CAPACITY
    }

    fn physical(&self, logical: usize) -> usize {
        (self.head + logical) % QUEUE_CAPACITY
    }

    fn peek(&self) -> Option<QueuedEvent> {
        (self.len > 0).then(|| self.buf[self.head])
    }

    fn pop(&mut self) -> Option<QueuedEvent> {
        if self.len == 0 {
            return None;
        }
        let value = self.buf[self.head];
        self.head = self.physical(1);
        self.len -= 1;
        Some(value)
    }

    fn push_back(&mut self, event: QueuedEvent) -> bool {
        if self.is_full() {
            return false;
        }
        let idx = self.physical(self.len);
        self.buf[idx] = event;
        self.len += 1;
        true
    }

    /// Logical index of the newest entry if it is a
    /// [`ev::POINTER_MOTION`], for coalescing into.
    ///
    /// Only the *newest* entry qualifies, and that restriction is
    /// load-bearing. Merging into any motion entry anywhere in the queue
    /// reorders a click relative to the motion that positioned it: with
    /// `[motion(A), button]` queued, folding a later `motion(B)` into
    /// slot 0 makes the driver move to B *before* pressing, so the click
    /// lands where the host never aimed it. "Move to the gadget, then
    /// click" is the entire point of an absolute pointer, so that is not
    /// a corner case. Two *consecutive* motions genuinely do supersede
    /// each other -- nothing observes the intermediate position -- which
    /// is where all the real coalescing pressure is anyway, since motion
    /// arrives in runs.
    fn tail_motion_index(&self) -> Option<usize> {
        let last = self.len.checked_sub(1)?;
        (self.buf[self.physical(last)].ty == ev::POINTER_MOTION).then_some(last)
    }

    /// Logical index of the oldest queued motion, for *eviction* when a
    /// discrete event would otherwise be dropped.
    ///
    /// Note the asymmetry with [`Self::tail_motion_index`], which is
    /// deliberate. Coalescing happens constantly and can always be done
    /// without reordering, so it is. Eviction only happens when the
    /// queue is genuinely full of undrained discrete events, and there
    /// the choice is between mispositioning a click and dropping a key
    /// event -- and a dropped key-*up* leaves a key stuck down in the
    /// guest indefinitely, corrupting every input after it, where a
    /// mispositioned click is one wrong action. So under real pressure
    /// this accepts the lesser harm rather than preserving order.
    fn motion_index(&self) -> Option<usize> {
        (0..self.len).find(|&i| self.buf[self.physical(i)].ty == ev::POINTER_MOTION)
    }

    fn overwrite(&mut self, logical: usize, event: QueuedEvent) {
        let idx = self.physical(logical);
        self.buf[idx] = event;
    }

    /// Remove the entry at logical index `at`, shifting every later
    /// entry one slot toward the head. O(`len`), which is fine at
    /// [`QUEUE_CAPACITY`]'s size -- this only ever runs on the rare path
    /// where a motion entry is evicted to make room for a discrete one.
    fn remove(&mut self, at: usize) {
        for i in at..self.len - 1 {
            let from = self.physical(i + 1);
            let to = self.physical(i);
            self.buf[to] = self.buf[from];
        }
        self.len -= 1;
    }
}

/// `input`'s register file and event queue. See the module docs for the
/// protocol this implements and the reasoning behind it.
pub struct NativeInput {
    queue: EventQueue,
    overflow: u32,
    int_status: u8,
    int_enable: u8,
}

impl NativeInput {
    pub fn new() -> Self {
        Self {
            queue: EventQueue::new(),
            overflow: 0,
            int_status: 0,
            int_enable: 0,
        }
    }

    /// The `BoardSpec` this card registers on the AUTOCONFIG chain: one
    /// Zorro III board, no DiagArea (`ERTF_DIAGVALID` unset) -- this
    /// increment carries no boot ROM, per the module docs and the brief's
    /// explicit scope.
    pub fn board_spec() -> BoardSpec {
        BoardSpec {
            board_type: ERT_ZORROIII, // extended-table code 0 == 16 MB
            product: PRODUCT,
            flags: ERFF_ZORRO_III | ERFF_EXTENDED,
            manufacturer: MANUFACTURER,
            serial: 0,
            init_diag_vec: 0,
            size_bytes: WINDOW_BYTES,
        }
    }

    /// Whether `input` is asserting its interrupt, which the bus routes
    /// to INT2 (`PORTS`) the same as every other native card here.
    pub fn irq_pending(&self) -> bool {
        self.int_enable != 0 && self.int_status != 0
    }

    /// Latch [`reg::INT_STATUS`] -- called on every successful push and
    /// every dropped one (module docs, "Interrupt model" and "Overflow
    /// policy").
    fn latch(&mut self) {
        self.int_status = 1;
    }

    /// Queue a key transition. Rejects `keycode > `[`MAX_RAW_KEYCODE`]
    /// by returning `false` and touching nothing else (module docs,
    /// "Hostile input").
    pub fn push_key(&mut self, keycode: u8, down: bool, qualifier: u16) -> bool {
        if keycode > MAX_RAW_KEYCODE {
            return false;
        }
        let ty = if down { ev::KEY_DOWN } else { ev::KEY_UP };
        self.push_discrete(QueuedEvent {
            ty,
            code: keycode,
            qualifier,
            x: 0,
            y: 0,
        })
    }

    /// Queue a button transition. Rejects `button > `[`MAX_BUTTON`] the
    /// same way [`push_key`](Self::push_key) rejects an out-of-range
    /// keycode.
    pub fn push_button(&mut self, button: u8, down: bool, qualifier: u16) -> bool {
        if button > MAX_BUTTON {
            return false;
        }
        let ty = if down { ev::BUTTON_DOWN } else { ev::BUTTON_UP };
        self.push_discrete(QueuedEvent {
            ty,
            code: button,
            qualifier,
            x: 0,
            y: 0,
        })
    }

    /// Queue an absolute pointer position. Rejects `x`/`y` outside
    /// `i16::MIN..=i16::MAX` (module docs' `InputEvent::ie_X`/`ie_Y` are
    /// `WORD` finding) by returning `false` and touching nothing else.
    pub fn push_pointer_motion(&mut self, x: i32, y: i32, qualifier: u16) -> bool {
        if x < i16::MIN as i32 || x > i16::MAX as i32 || y < i16::MIN as i32 || y > i16::MAX as i32
        {
            return false;
        }
        let event = QueuedEvent {
            ty: ev::POINTER_MOTION,
            code: 0,
            qualifier,
            x,
            y,
        };
        if let Some(idx) = self.queue.tail_motion_index() {
            self.queue.overwrite(idx, event);
        } else if !self.queue.push_back(event) {
            // Harmless and deliberately uncounted -- module docs'
            // "Overflow policy" last bullet.
            return false;
        }
        self.latch();
        true
    }

    /// Shared tail of [`push_key`](Self::push_key)/
    /// [`push_button`](Self::push_button): append, evicting a queued
    /// motion entry first if the queue is full, and only failing (drop +
    /// count) if there is no motion entry left to evict. Module docs,
    /// "Overflow policy".
    fn push_discrete(&mut self, event: QueuedEvent) -> bool {
        if !self.queue.push_back(event) {
            if let Some(idx) = self.queue.motion_index() {
                self.queue.remove(idx);
                let pushed = self.queue.push_back(event);
                debug_assert!(pushed, "just evicted a slot for this exact push");
            } else {
                self.overflow = self.overflow.wrapping_add(1);
                self.latch(); // a driver polling only the counter still needs waking (module docs)
                return false;
            }
        }
        self.latch();
        true
    }

    /// Read a byte of the register file, offset from this board's
    /// configured AUTOCONFIG base. See [`reg`] for the layout;
    /// [`hostblk::Hostblk::read`](crate::hostblk::Hostblk::read)'s doc
    /// comment for why anything unnamed reads `0` rather than the wider
    /// bus's open-bus `0xFF`.
    pub fn read(&self, offset: u32) -> u8 {
        let head = self.queue.peek();
        match offset {
            o if in_slot(o, reg::EVENT_TYPE) => {
                low_byte(o, reg::EVENT_TYPE, head.map(|e| e.ty).unwrap_or(ev::NONE))
            }
            o if in_slot(o, reg::EVENT_CODE) => {
                low_byte(o, reg::EVENT_CODE, head.map(|e| e.code).unwrap_or(0))
            }
            o if in_slot(o, reg::EVENT_QUALIFIER) => byte_of(
                u32::from(head.map(|e| e.qualifier).unwrap_or(0)),
                o - reg::EVENT_QUALIFIER,
            ),
            o if in_slot(o, reg::EVENT_X) => {
                byte_of(head.map(|e| e.x).unwrap_or(0) as u32, o - reg::EVENT_X)
            }
            o if in_slot(o, reg::EVENT_Y) => {
                byte_of(head.map(|e| e.y).unwrap_or(0) as u32, o - reg::EVENT_Y)
            }
            o if in_slot(o, reg::EVENT_COUNT) => {
                byte_of(self.queue.len() as u32, o - reg::EVENT_COUNT)
            }
            o if in_slot(o, reg::EVENT_OVERFLOW) => byte_of(self.overflow, o - reg::EVENT_OVERFLOW),
            o if in_slot(o, reg::INT_STATUS) => low_byte(o, reg::INT_STATUS, self.int_status),
            o if in_slot(o, reg::INT_ENABLE) => low_byte(o, reg::INT_ENABLE, self.int_enable),
            o if in_slot(o, reg::CAPACITY) => byte_of(QUEUE_CAPACITY as u32, o - reg::CAPACITY),
            o if in_slot(o, reg::VERSION) => byte_of(PROTOCOL_VERSION, o - reg::VERSION),
            _ => 0,
        }
    }

    /// Write a byte of the register file. See [`Self::read`] for the
    /// offset layout.
    pub fn write(&mut self, offset: u32, value: u8) {
        match offset {
            o if in_slot(o, reg::EVENT_ADVANCE) && o - reg::EVENT_ADVANCE == 3 => {
                self.queue.pop();
            }
            o if in_slot(o, reg::EVENT_ADVANCE) => {}
            o if in_slot(o, reg::INT_STATUS) && o - reg::INT_STATUS == 3 => {
                // Write-1-to-clear, gated on the queue being empty --
                // module docs, "Interrupt model": acknowledging while
                // events remain unread would strand them with no further
                // interrupt to say so, so the write is simply ignored
                // (not queued, not deferred) until the driver has
                // actually drained everything.
                if value & 1 != 0 && self.queue.len() == 0 {
                    self.int_status = 0;
                }
            }
            o if in_slot(o, reg::INT_STATUS) => {}
            o if in_slot(o, reg::INT_ENABLE) && o - reg::INT_ENABLE == 3 => {
                self.int_enable = value;
            }
            o if in_slot(o, reg::INT_ENABLE) => {}
            _ => {}
        }
    }
}

impl Default for NativeInput {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `offset` falls in the 4-byte-aligned slot starting at `base`.
fn in_slot(offset: u32, base: u32) -> bool {
    (base..base + 4).contains(&offset)
}

/// A single-byte register's value at offset `base + 3` (the low-order
/// byte of the slot), `0` at the other three offsets in the slot --
/// `mirage.rs`/`hostblk.rs`'s same convention.
fn low_byte(offset: u32, base: u32, value: u8) -> u8 {
    if offset - base == 3 {
        value
    } else {
        0
    }
}

/// Byte `lane` (0 = most significant) of a big-endian 32-bit register.
fn byte_of(value: u32, lane: u32) -> u8 {
    (value >> (8 * (3 - lane))) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_u32(dev: &NativeInput, base: u32) -> u32 {
        u32::from_be_bytes([
            dev.read(base),
            dev.read(base + 1),
            dev.read(base + 2),
            dev.read(base + 3),
        ])
    }

    fn advance(dev: &mut NativeInput) {
        dev.write(reg::EVENT_ADVANCE + 3, 0);
    }

    fn ack(dev: &mut NativeInput) {
        dev.write(reg::INT_STATUS + 3, 1);
    }

    /// Coalescing must never reorder a click relative to the motion that
    /// positioned it. "Move to the gadget, then click" is the whole point
    /// of an absolute pointer, and it breaks if a later motion is merged
    /// into a slot that sits *before* an already-queued button press: the
    /// driver then moves the pointer to the newer position first and the
    /// click lands somewhere the host never asked for.
    ///
    /// Written from that requirement rather than from the queue's
    /// behaviour -- the coalescing rule and a test derived from it would
    /// otherwise agree with each other and both be wrong.
    #[test]
    fn a_later_motion_never_jumps_ahead_of_an_already_queued_button() {
        let mut dev = NativeInput::new();
        dev.push_pointer_motion(100, 100, 0);
        dev.push_button(button::LEFT, true, 0);
        dev.push_pointer_motion(200, 200, 0);

        // First out must still be the position the click was aimed at.
        assert_eq!(read_u32(&dev, reg::EVENT_TYPE), ev::POINTER_MOTION as u32);
        assert_eq!(read_u32(&dev, reg::EVENT_X), 100, "click position lost");
        assert_eq!(read_u32(&dev, reg::EVENT_Y), 100, "click position lost");
        advance(&mut dev);

        // Then the button, still at that position.
        assert_eq!(read_u32(&dev, reg::EVENT_TYPE), ev::BUTTON_DOWN as u32);
        advance(&mut dev);

        // Only then the later motion.
        assert_eq!(read_u32(&dev, reg::EVENT_TYPE), ev::POINTER_MOTION as u32);
        assert_eq!(read_u32(&dev, reg::EVENT_X), 200);
    }

    // ---- capacity and version discovery ---------------------------------

    #[test]
    fn capacity_register_matches_the_real_queue_depth() {
        let dev = NativeInput::new();
        assert_eq!(read_u32(&dev, reg::CAPACITY), QUEUE_CAPACITY as u32);
    }

    #[test]
    fn version_register_reports_the_documented_protocol_version() {
        let dev = NativeInput::new();
        assert_eq!(read_u32(&dev, reg::VERSION), PROTOCOL_VERSION);
        assert_ne!(PROTOCOL_VERSION, 0);
    }

    #[test]
    fn empty_queue_reads_none_type_and_zeroed_fields() {
        let dev = NativeInput::new();
        assert_eq!(dev.read(reg::EVENT_TYPE + 3), ev::NONE);
        assert_eq!(dev.read(reg::EVENT_CODE + 3), 0);
        assert_eq!(read_u32(&dev, reg::EVENT_QUALIFIER), 0);
        assert_eq!(read_u32(&dev, reg::EVENT_X), 0);
        assert_eq!(read_u32(&dev, reg::EVENT_Y), 0);
        assert_eq!(read_u32(&dev, reg::EVENT_COUNT), 0);
    }

    // ---- queue round trip, key/button events -----------------------------

    #[test]
    fn key_down_then_up_round_trips_through_the_queue_in_order() {
        let mut dev = NativeInput::new();
        assert!(dev.push_key(0x41, true, 0x0008)); // control-qualified
        assert!(dev.push_key(0x41, false, 0));

        assert_eq!(read_u32(&dev, reg::EVENT_COUNT), 2);
        assert_eq!(dev.read(reg::EVENT_TYPE + 3), ev::KEY_DOWN);
        assert_eq!(dev.read(reg::EVENT_CODE + 3), 0x41);
        assert_eq!(read_u32(&dev, reg::EVENT_QUALIFIER), 0x0008);
        advance(&mut dev);

        assert_eq!(dev.read(reg::EVENT_TYPE + 3), ev::KEY_UP);
        assert_eq!(dev.read(reg::EVENT_CODE + 3), 0x41);
        advance(&mut dev);

        assert_eq!(dev.read(reg::EVENT_TYPE + 3), ev::NONE);
        assert_eq!(read_u32(&dev, reg::EVENT_COUNT), 0);
    }

    #[test]
    fn button_events_round_trip_the_same_way() {
        let mut dev = NativeInput::new();
        assert!(dev.push_button(button::LEFT, true, 0));
        assert_eq!(dev.read(reg::EVENT_TYPE + 3), ev::BUTTON_DOWN);
        assert_eq!(dev.read(reg::EVENT_CODE + 3), button::LEFT);
        advance(&mut dev);
        assert!(dev.push_button(button::LEFT, false, 0));
        assert_eq!(dev.read(reg::EVENT_TYPE + 3), ev::BUTTON_UP);
    }

    // ---- absolute pointer coordinates survive intact ----------------------

    #[test]
    fn pointer_motion_carries_absolute_signed_coordinates_intact() {
        let mut dev = NativeInput::new();
        assert!(dev.push_pointer_motion(-1, 32000, 0));
        assert_eq!(dev.read(reg::EVENT_TYPE + 3), ev::POINTER_MOTION);
        assert_eq!(
            dev.read(reg::EVENT_CODE + 3),
            0,
            "no button/key code for motion"
        );
        assert_eq!(read_u32(&dev, reg::EVENT_X), -1i32 as u32);
        assert_eq!(read_u32(&dev, reg::EVENT_Y), 32000);
    }

    #[test]
    fn pointer_motion_coalesces_into_a_single_queue_slot() {
        let mut dev = NativeInput::new();
        assert!(dev.push_pointer_motion(1, 1, 0));
        assert!(dev.push_pointer_motion(2, 2, 0));
        assert!(dev.push_pointer_motion(3, 3, 0));
        assert_eq!(
            read_u32(&dev, reg::EVENT_COUNT),
            1,
            "repeated motion must not grow the queue"
        );
        assert_eq!(read_u32(&dev, reg::EVENT_X), 3);
        assert_eq!(read_u32(&dev, reg::EVENT_Y), 3);
    }

    /// Motion separated by a discrete event must *not* collapse: each
    /// position still has a key or button behind it that was aimed at it.
    ///
    /// This test previously asserted the opposite, under the name
    /// "keeps only the latest motion" -- it was written from the
    /// implementation's coalescing rule rather than from what an absolute
    /// pointer has to guarantee, so the rule and the test agreed with each
    /// other and were both wrong.
    #[test]
    fn motion_separated_by_a_key_is_not_collapsed() {
        let mut dev = NativeInput::new();
        assert!(dev.push_key(0x01, true, 0));
        assert!(dev.push_pointer_motion(10, 10, 0));
        assert!(dev.push_key(0x02, true, 0));
        assert!(dev.push_pointer_motion(20, 20, 0));
        // All four survive: the two motions are not adjacent, so neither
        // supersedes the other.
        assert_eq!(read_u32(&dev, reg::EVENT_COUNT), 4);
        assert_eq!(dev.read(reg::EVENT_TYPE + 3), ev::KEY_DOWN);
        assert_eq!(dev.read(reg::EVENT_CODE + 3), 0x01);
    }

    /// Consecutive motion, on the other hand, is exactly what coalescing
    /// is for: nothing observes the intermediate position.
    #[test]
    fn consecutive_motion_coalesces_into_one_entry() {
        let mut dev = NativeInput::new();
        assert!(dev.push_pointer_motion(10, 10, 0));
        assert!(dev.push_pointer_motion(20, 20, 0));
        assert!(dev.push_pointer_motion(30, 30, 0));
        assert_eq!(read_u32(&dev, reg::EVENT_COUNT), 1);
        assert_eq!(read_u32(&dev, reg::EVENT_X), 30);
    }

    // ---- overflow policy --------------------------------------------------

    #[test]
    fn a_full_queue_of_keys_evicts_pending_motion_rather_than_dropping_the_key() {
        let mut dev = NativeInput::new();
        for i in 0..(QUEUE_CAPACITY as u8 - 1) {
            assert!(dev.push_key(i, true, 0));
        }
        assert!(dev.push_pointer_motion(5, 5, 0));
        assert_eq!(read_u32(&dev, reg::EVENT_COUNT), QUEUE_CAPACITY as u32);

        // One more key must still get in by evicting the motion entry,
        // not by being dropped -- module docs' "Overflow policy".
        assert!(dev.push_key(MAX_RAW_KEYCODE, true, 0));
        assert_eq!(read_u32(&dev, reg::EVENT_OVERFLOW), 0);
        assert_eq!(read_u32(&dev, reg::EVENT_COUNT), QUEUE_CAPACITY as u32);
    }

    #[test]
    fn a_queue_full_of_keys_with_no_motion_to_evict_drops_and_counts() {
        let mut dev = NativeInput::new();
        for i in 0..QUEUE_CAPACITY as u8 {
            assert!(dev.push_key(i, true, 0));
        }
        assert!(
            !dev.push_key(0, false, 0),
            "a genuinely full discrete queue must reject, not silently grow"
        );
        assert_eq!(read_u32(&dev, reg::EVENT_OVERFLOW), 1);
        // Still latched, so a driver polling only the interrupt still
        // gets a chance to notice the overflow (module docs).
        dev.write(reg::INT_ENABLE + 3, 1);
        assert!(dev.irq_pending());
    }

    #[test]
    fn dropped_pointer_motion_is_not_counted_as_overflow() {
        let mut dev = NativeInput::new();
        for i in 0..QUEUE_CAPACITY as u8 {
            assert!(dev.push_key(i, true, 0));
        }
        assert!(!dev.push_pointer_motion(1, 1, 0));
        assert_eq!(
            read_u32(&dev, reg::EVENT_OVERFLOW),
            0,
            "a dropped, self-superseding motion event must not inflate the overflow counter"
        );
    }

    // ---- interrupt asserting and clearing ----------------------------------

    #[test]
    fn interrupt_requires_both_enable_and_a_latched_event() {
        let mut dev = NativeInput::new();
        assert!(dev.push_key(0x01, true, 0));
        assert!(!dev.irq_pending(), "masked while INT_ENABLE is 0");
        dev.write(reg::INT_ENABLE + 3, 1);
        assert!(dev.irq_pending());
    }

    #[test]
    fn write_one_to_int_status_clears_it_once_the_queue_is_drained() {
        let mut dev = NativeInput::new();
        dev.write(reg::INT_ENABLE + 3, 1);
        assert!(dev.push_key(0x01, true, 0));
        assert!(dev.irq_pending());

        advance(&mut dev); // drain the one event
        ack(&mut dev);
        assert!(!dev.irq_pending());
    }

    #[test]
    fn ack_while_events_remain_queued_is_ignored_not_stranded() {
        let mut dev = NativeInput::new();
        dev.write(reg::INT_ENABLE + 3, 1);
        assert!(dev.push_key(0x01, true, 0));
        assert!(dev.push_key(0x02, true, 0));

        ack(&mut dev); // one event still queued -- must not clear
        assert!(
            dev.irq_pending(),
            "acking with unread events queued must not strand them"
        );
        advance(&mut dev);
        ack(&mut dev); // still one queued
        assert!(dev.irq_pending());
        advance(&mut dev);
        ack(&mut dev); // now empty
        assert!(!dev.irq_pending());
    }

    #[test]
    fn int_enable_write_only_the_low_lane_of_its_slot_takes_effect() {
        let mut dev = NativeInput::new();
        dev.write(reg::INT_ENABLE, 1);
        dev.write(reg::INT_ENABLE + 1, 1);
        dev.write(reg::INT_ENABLE + 2, 1);
        assert_eq!(dev.read(reg::INT_ENABLE + 3), 0, "only lane 3 is live");
        dev.write(reg::INT_ENABLE + 3, 1);
        assert_eq!(dev.read(reg::INT_ENABLE + 3), 1);
    }

    // ---- hostile input ------------------------------------------------------

    #[test]
    fn a_keycode_past_the_documented_range_is_rejected_cleanly() {
        let mut dev = NativeInput::new();
        assert!(!dev.push_key(MAX_RAW_KEYCODE + 1, true, 0));
        assert!(!dev.push_key(0xFF, true, 0));
        assert_eq!(read_u32(&dev, reg::EVENT_COUNT), 0);
    }

    #[test]
    fn a_button_id_past_the_documented_range_is_rejected_cleanly() {
        let mut dev = NativeInput::new();
        assert!(!dev.push_button(MAX_BUTTON + 1, true, 0));
        assert_eq!(read_u32(&dev, reg::EVENT_COUNT), 0);
    }

    #[test]
    fn out_of_range_pointer_coordinates_are_rejected_cleanly() {
        let mut dev = NativeInput::new();
        assert!(!dev.push_pointer_motion(i16::MAX as i32 + 1, 0, 0));
        assert!(!dev.push_pointer_motion(0, i16::MIN as i32 - 1, 0));
        assert!(!dev.push_pointer_motion(i32::MAX, i32::MIN, 0));
        assert_eq!(read_u32(&dev, reg::EVENT_COUNT), 0);
        // The full legal range still works.
        assert!(dev.push_pointer_motion(i16::MIN as i32, i16::MAX as i32, 0));
    }

    #[test]
    fn unimplemented_offsets_read_zero_and_discard_writes() {
        let mut dev = NativeInput::new();
        assert_eq!(dev.read(0x100), 0);
        dev.write(0x100, 0xFF); // must not panic
        assert_eq!(dev.read(0x100), 0);
    }
}
