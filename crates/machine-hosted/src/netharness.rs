//! Harness [`machine_core::pci::NetBackend`] for ADR 0005 stage 3's
//! virtio-net stub (`docs/virtionet.md`): the backend `run.rs` wires up
//! behind `--pcibridge` in place of `machine_core::pci::NullNetBackend`.
//!
//! # What it is, and what it deliberately is not
//!
//! This is not a real host network path -- it never touches a tap
//! device, a socket, or anything outside this process. It exists so a
//! real-ROM boot can prove the whole chain end to end (guest driver up,
//! guest transmits, guest receives) without machine-hosted growing host
//! network I/O this increment's brief does not ask for (`docs/
//! virtionet.md` records a real tap/socket backend as deliberately
//! deferred). It does two things:
//!
//!   1. Records every transmitted frame (already virtio-net-header-
//!      stripped, `NetBackend::transmit`'s own contract), verbatim and in
//!      order, so `--inspect` can report exactly what the guest sent --
//!      the real-ROM test asserts the exact bytes.
//!   2. After the *first* transmitted frame, makes a small, bounded
//!      number of separated attempts to deliver one fixed echo-reply
//!      frame (module doc comment below on why *attempts*, spaced apart,
//!      rather than one delayed delivery or a continuous offer): dst =
//!      the device's own MAC (`VirtioNetStub::MAC`), src = a distinct
//!      locally-administered MAC (`...:00:FE`), ethertype `0x88B5` (the
//!      same experimental ethertype `m68k/vnettest` transmits with),
//!      payload "M68KVNET-RX-REPLY-0001".
//!
//! # Why bounded, *spaced*, repeated attempts
//!
//! Arming only after a transmit -- never before -- guarantees the rings
//! are already up (the guest driver must have gotten through its whole
//! init dance and posted a tx descriptor first). It does *not* guarantee
//! the guest has posted its own receiving request (`virtionet.device`'s
//! `CMD_READ`, a purely software-side FIFO the device itself never
//! observes) by the time this reply is first deliverable: `VirtioNetStub`
//! polls both queues every tick (`tick`'s own body: `process_tx` then
//! `process_rx`), so the very same tick that notices the guest's
//! transmitted frame can *also* already have an rx descriptor available
//! to deliver into -- well before the guest task resumes from its
//! blocking `DoIO` on that transmit, let alone gets around to posting the
//! read that would actually consume the reply. A delivery into that
//! window is received by the driver's rx path with no pending reader to
//! match it against, and is silently dropped.
//!
//! Two earlier drafts of this backend got this wrong in opposite
//! directions, both confirmed empirically against the real-ROM test:
//!
//! - **Delivering once, immediately.** Dropped every time, for exactly
//!   the reason above.
//! - **Delivering once, after a single bounded delay** (counting
//!   `poll_receive` calls, the same "generous busy-wait" idiom
//!   `pciprobe.c`'s `INTX_POLL_LIMIT`/`vnettest.c`'s `READ_POLL_LIMIT`
//!   use). Still dropped, at delays from a few dozen calls up to several
//!   thousand: `virtionet_device.c`'s ISR reads the `ISR` byte once,
//!   right at entry, and does not return to the guest's own task level
//!   until it has drained everything the used ring already holds --
//!   `process_rx`/`process_tx` keep running every tick regardless of
//!   whether the CPU is currently inside that very handler, so a
//!   delayed delivery lands *during* the same still-running interrupt
//!   servicing as often as it lands after the guest returns to task
//!   level and posts `CMD_READ`. No fixed delay-then-single-shot can
//!   reliably tell those two apart from the host side.
//! - **Offering the same frame on *every* subsequent `poll_receive`**,
//!   reasoning that repetition would eventually land in the right
//!   window. Confirmed actively worse: `virtionet_device.c`'s own
//!   rx-completion path reposts a dropped buffer straight back onto the
//!   guest's avail ring before returning, so an always-armed backend
//!   refills the very ring that just fed it, tick after tick, forever --
//!   a genuine livelock (confirmed by instrumented counters: over
//!   20,000 back-to-back deliver-and-drop cycles with no sign of ever
//!   stopping, before being killed).
//!
//! The design that actually works combines both drafts' partial insight
//! without either failure mode: make a delivery attempt, then go
//! completely quiet (no delivery offered at all, `poll_receive` returns
//! `None`) for [`REPLY_ATTEMPT_COOLDOWN_FRAMES`] guest frames before
//! trying again, for at most [`REPLY_MAX_ATTEMPTS`] attempts total. Each
//! attempt that lands in the no-reader window costs the driver exactly
//! one drop-and-repost -- bounded, not self-perpetuating, because
//! nothing new is offered again until the next attempt's own cooldown
//! elapses. The cooldown between attempts is far longer than any
//! plausible stretch of interrupt servicing, so by the next attempt the
//! guest is certainly back at task level; spreading a handful of
//! attempts across a wide span of ticks makes this self-correcting
//! against however long that actually takes, the same spirit as the
//! rejected "keep offering" draft, without its unbounded refill.
//!
//! # Why the cooldown is counted in guest frames, not `poll_receive` calls
//!
//! It used to be counted in calls (500,000 of them). `process_rx` calls
//! `poll_receive` once per `MachineBus::tick`, so that only meant "a few
//! frames" while the bus ticked the card after every instruction. Once
//! `MachineBus::tick` ran the card once per raster line
//! (`docs/bus-fast-path-plan.md` step 4.1), the same count became
//! 500,000 lines, about 1,600 frames per cooldown, past the real-ROM
//! test's own frame budget: the harness, not the device, made the test
//! fail. [`HarnessNetBackend::new`] now takes a guest frame clock that
//! `run.rs` advances from `chipset.frames`, so the spacing is guest time
//! whatever the tick grain is.
//!
//! # Why a shared log, not a plain field
//!
//! `machine_core::pci::VirtioNetStub` borrows its backend `&'a mut`
//! (module docs, "No allocator, caller supplies everything") for as long
//! as the whole `pcibridge` virtual topology is attached to the bus --
//! which in `run.rs` outlives the guest run itself, since `--inspect`
//! reads bus state (including the `pcibridge` section) right after the
//! guest stops. That borrow makes the backend itself unreachable from
//! `run.rs` at reporting time. [`NetHarnessLog`] is instead shared via
//! `Rc<RefCell<_>>`: a clone kept in `run.rs` for reporting is a wholly
//! separate handle from the one moved into this backend, so it can be
//! read after the run even while the backend proper is still borrowed
//! deep inside `pcibridge`'s trait-object chain.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use machine_core::pci::{NetBackend, VirtioNetStub};

/// This harness's fixed echo-reply source MAC: distinct from the
/// device's own (`VirtioNetStub::MAC`, `...:00:01`) so the two are never
/// confused in a hex dump -- `...:00:FE` reads as "the far end of this
/// virtual wire", the same locally-administered range the device's own
/// MAC uses.
const REPLY_SRC_MAC: [u8; 6] = [0x02, 0x6d, 0x36, 0x4b, 0x00, 0xFE];

/// The ethertype `m68k/vnettest` transmits with and accepts on receipt
/// (wildcard `PacketType 0` on its posted `CMD_READ`) -- `docs/
/// virtionet.md`.
const REPLY_ETHERTYPE: [u8; 2] = [0x88, 0xB5];

/// The fixed reply payload `m68k/vnettest`'s `CMD_READ` narrates on
/// success ("VNETTEST cmd_read: PASS ... payload \"M68KVNET-RX-REPLY-0001\"").
const REPLY_PAYLOAD: &[u8] = b"M68KVNET-RX-REPLY-0001";

/// Build the one fixed echo-reply frame this harness ever sends: no
/// virtio-net header (module docs on [`NetBackend::poll_receive`] --
/// `VirtioNetStub` prepends that itself), no Ethernet padding (unlike the
/// guest's own transmitted frame, nothing on this virtual wire enforces a
/// 60-byte minimum on the harness's own reply).
fn build_reply_frame() -> Vec<u8> {
    let mut frame = Vec::with_capacity(6 + 6 + 2 + REPLY_PAYLOAD.len());
    frame.extend_from_slice(&VirtioNetStub::MAC);
    frame.extend_from_slice(&REPLY_SRC_MAC);
    frame.extend_from_slice(&REPLY_ETHERTYPE);
    frame.extend_from_slice(REPLY_PAYLOAD);
    frame
}

/// The shared record [`HarnessNetBackend`] writes to and `run.rs` reads
/// back from -- see this module's doc comment for why this is a separate
/// `Rc<RefCell<_>>`-shared type rather than a field read straight off the
/// backend.
#[derive(Default)]
pub struct NetHarnessLog {
    transmitted: Vec<Vec<u8>>,
}

impl NetHarnessLog {
    /// `--inspect`'s report of this harness's traffic: one line naming
    /// the frame count, then one line per frame giving its index, length,
    /// and full hex bytes -- the real-ROM test asserts exact content, so
    /// this prints everything rather than a summary.
    pub fn format(&self) -> String {
        let mut out = format!(
            "pcibridge net harness: {} frame(s) transmitted by the guest\n",
            self.transmitted.len()
        );
        for (index, frame) in self.transmitted.iter().enumerate() {
            let hex = frame
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            out.push_str(&format!(
                "  frame {index}: {} byte(s): {hex}\n",
                frame.len()
            ));
        }
        out
    }
}

/// Gap between delivery attempts (module doc comment), in guest frames
/// of the clock passed to [`HarnessNetBackend::new`] (module doc comment,
/// "Why the cooldown is counted in guest frames"). 5 PAL frames is 100 ms
/// of guest time, far longer than any plausible stretch of interrupt
/// servicing (the longest one observed was under 5,000 instructions,
/// about a twentieth of a frame at this machine's rate) and about what
/// the old 500,000-call count came to when the card was ticked per
/// instruction. [`REPLY_MAX_ATTEMPTS`] full cooldowns are 25 frames,
/// a small fraction of the real-ROM test's 3,000-frame budget.
const REPLY_ATTEMPT_COOLDOWN_FRAMES: u64 = 5;

/// How many separated attempts (module doc comment) this backend makes
/// before giving up silently. Five cooldowns spread the attempts across
/// 25 guest frames -- generous head room over the handful of attempts
/// this backend has ever actually needed in practice, while still
/// bounded (never an unconditional "keep offering").
const REPLY_MAX_ATTEMPTS: u32 = 5;

/// This backend's state machine: idle until the first transmit, then
/// alternating between a cooldown (silent, `poll_receive` returns `None`)
/// and one delivery attempt, for at most [`REPLY_MAX_ATTEMPTS`] attempts
/// (module doc comment on why bounded, spaced attempts rather than one
/// delayed delivery or a continuous offer).
enum ReplyState {
    Idle,
    Cooldown {
        /// The guest frame at or after which the next attempt is made.
        due_frame: u64,
        attempts_left: u32,
    },
    GaveUp,
}

/// The [`NetBackend`] itself -- see this module's doc comment for the
/// recording/echo-reply behaviour.
pub struct HarnessNetBackend {
    log: Rc<RefCell<NetHarnessLog>>,
    reply: ReplyState,
    frames: Rc<Cell<u64>>,
}

impl HarnessNetBackend {
    /// Build a backend writing into `log` -- `run.rs` keeps its own clone
    /// of the same `Rc` for reporting after the run (module doc comment).
    /// `frames` is the guest frame count, which `run.rs` keeps current
    /// from `chipset.frames`; the reply cooldown is measured against it.
    pub fn new(log: Rc<RefCell<NetHarnessLog>>, frames: Rc<Cell<u64>>) -> Self {
        Self {
            log,
            reply: ReplyState::Idle,
            frames,
        }
    }

    fn next_due(&self) -> u64 {
        self.frames
            .get()
            .saturating_add(REPLY_ATTEMPT_COOLDOWN_FRAMES)
    }
}

impl NetBackend for HarnessNetBackend {
    fn transmit(&mut self, frame: &[u8]) {
        self.log.borrow_mut().transmitted.push(frame.to_vec());
        // Arm on the first frame ever transmitted only (module doc
        // comment: guarantees the rings are up); a guest that transmits
        // more than once still gets the same bounded attempt budget, not
        // a fresh one.
        if let ReplyState::Idle = self.reply {
            self.reply = ReplyState::Cooldown {
                due_frame: self.next_due(),
                attempts_left: REPLY_MAX_ATTEMPTS,
            };
        }
    }

    fn poll_receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        let now = self.frames.get();
        match self.reply {
            ReplyState::Idle | ReplyState::GaveUp => None,
            ReplyState::Cooldown { due_frame, .. } if now < due_frame => None,
            ReplyState::Cooldown { attempts_left, .. } => {
                let attempts_left = attempts_left - 1;
                self.reply = if attempts_left > 0 {
                    ReplyState::Cooldown {
                        due_frame: self.next_due(),
                        attempts_left,
                    }
                } else {
                    ReplyState::GaveUp
                };
                let frame = build_reply_frame();
                if frame.len() > buf.len() {
                    // Cannot happen with this harness's own fixed frame
                    // and VirtioNetStub::MAX_ETH_PAYLOAD's own size, but
                    // fail closed (drop, not panic or truncate) rather
                    // than assume.
                    return None;
                }
                buf[..frame.len()].copy_from_slice(&frame);
                Some(frame.len())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> (HarnessNetBackend, Rc<RefCell<NetHarnessLog>>, Rc<Cell<u64>>) {
        let log = Rc::new(RefCell::new(NetHarnessLog::default()));
        let frames = Rc::new(Cell::new(0));
        let backend = HarnessNetBackend::new(Rc::clone(&log), Rc::clone(&frames));
        (backend, log, frames)
    }

    #[test]
    fn records_transmitted_frames_verbatim_and_in_order() {
        let (mut backend, log, _) = backend();

        backend.transmit(&[1, 2, 3]);
        backend.transmit(&[4, 5]);

        let recorded = log.borrow();
        assert_eq!(recorded.transmitted, vec![vec![1u8, 2, 3], vec![4u8, 5]]);
    }

    /// Run `backend` through one cooldown-then-attempt cycle, asserting
    /// the cooldown stays silent however many times it is polled in
    /// each frame (the point of counting frames rather than polls), and
    /// that the attempt itself returns `Some`. Returns the delivered
    /// length.
    fn expect_one_attempt(
        backend: &mut HarnessNetBackend,
        frames: &Cell<u64>,
        buf: &mut [u8],
    ) -> usize {
        for _ in 0..REPLY_ATTEMPT_COOLDOWN_FRAMES {
            for _ in 0..1000 {
                assert_eq!(backend.poll_receive(buf), None);
            }
            frames.set(frames.get() + 1);
        }
        backend
            .poll_receive(buf)
            .expect("an attempt after its cooldown has elapsed")
    }

    #[test]
    fn delivers_nothing_before_the_first_cooldown_elapses() {
        let (mut backend, _, frames) = backend();
        let mut buf = [0u8; VirtioNetStub::MAX_ETH_PAYLOAD];

        assert_eq!(backend.poll_receive(&mut buf), None);

        backend.transmit(&[0xAA]);
        let len = expect_one_attempt(&mut backend, &frames, &mut buf);
        assert_eq!(&buf[..6], &VirtioNetStub::MAC);
        assert_eq!(&buf[6..12], &REPLY_SRC_MAC);
        assert_eq!(&buf[12..14], &REPLY_ETHERTYPE);
        assert_eq!(&buf[14..len], REPLY_PAYLOAD);
    }

    #[test]
    fn makes_exactly_reply_max_attempts_then_gives_up_silently() {
        let (mut backend, _, frames) = backend();
        let mut buf = [0u8; VirtioNetStub::MAX_ETH_PAYLOAD];

        backend.transmit(&[1]);
        backend.transmit(&[2]); // a second transmit does not reset the budget

        for _ in 0..REPLY_MAX_ATTEMPTS {
            expect_one_attempt(&mut backend, &frames, &mut buf);
        }

        // Budget exhausted: no further attempt, ever, however long polled.
        for _ in 0..(REPLY_ATTEMPT_COOLDOWN_FRAMES * 4) {
            frames.set(frames.get() + 1);
            assert_eq!(backend.poll_receive(&mut buf), None);
        }
    }

    #[test]
    fn exactly_one_attempt_per_elapsed_cooldown_however_often_polled() {
        let (mut backend, _, frames) = backend();
        let mut buf = [0u8; VirtioNetStub::MAX_ETH_PAYLOAD];

        backend.transmit(&[1]);
        frames.set(REPLY_ATTEMPT_COOLDOWN_FRAMES);
        assert!(backend.poll_receive(&mut buf).is_some());
        // Same frame, polled again: the next cooldown has only just begun.
        for _ in 0..1000 {
            assert_eq!(backend.poll_receive(&mut buf), None);
        }
    }

    #[test]
    fn format_reports_index_length_and_full_hex_bytes() {
        let log = NetHarnessLog {
            transmitted: vec![vec![0xDE, 0xAD, 0xBE, 0xEF]],
        };
        let text = log.format();
        assert!(text.contains("1 frame(s)"));
        assert!(text.contains("frame 0: 4 byte(s): de ad be ef"));
    }
}
