//! `--serial-tcp`: a live, bidirectional TCP bridge onto the guest's
//! serial port -- the interactive counterpart to `--serial-script`'s
//! scripted host->guest input and `--serial-log`'s guest->host tee.
//!
//! # Why TCP
//!
//! The protocol carried here is nothing but the raw byte stream the
//! guest's serial hardware already produces and consumes
//! (`Chipset::take_serial_byte`/`push_serial_in_byte`) -- there is no
//! framing, handshake, or anything specific to any one client added on
//! top. TCP is simply the carrier chosen for that stream, and the choice
//! is ours, not any particular client's requirement:
//!
//! - It needs nothing beyond `std` (a PTY would mean platform-specific
//!   plumbing for the same result).
//! - Copperline, this project's cycle-exact oracle, already exposes its
//!   own guest serial as a TCP bridge. Matching that shape means the same
//!   host-side tooling (a terminal, a test harness, a debugger client)
//!   can point at either machine's serial port with nothing but an
//!   address change -- useful precisely because Copperline is the oracle
//!   this project checks itself against.
//! - It sidesteps needing an optional serial-port client library
//!   (`pyserial` or equivalent) just to talk to a byte stream that was
//!   never anything but bytes.
//!
//! Because of that, nothing about this bridge is specific to any one
//! consumer. AmiPilot's host-side `WireClient` is the immediate one (its
//! own wire protocol is transport-agnostic -- a plain socket is one
//! carrier among others its client already supports), but a plain
//! `nc 127.0.0.1 1234`, an interactive terminal, or a future debugger
//! client all work here equally, because none of them are being spoken
//! to -- only the guest's serial port is.
//!
//! # Why a background thread, not inline in the frame loop
//!
//! The runner is single-threaded and drives the guest at whatever speed
//! the host CPU allows, with no wall-clock pacing -- a `TcpListener::accept`
//! or a blocking `read`/`write` inline in that loop would stall guest
//! execution on a client that is slow, absent, or never sends anything.
//! [`SerialTcpBridge::start`] instead spawns one thread that owns the
//! listener and the (at most one) connected client, polling both with
//! non-blocking sockets and a short sleep between iterations. The frame
//! loop only ever touches two `Mutex<VecDeque<u8>>` queues shared with
//! that thread, through [`SerialTcpBridge::push_guest_byte`] (never
//! blocks: a full queue drops the oldest byte) and
//! [`SerialTcpBridge::try_recv_host_byte`] (never blocks: an empty queue
//! returns `None`). Locking a `Mutex` for a handful of bytes a frame is
//! not a stall in any sense the brief cares about; blocking on the
//! network is.
//!
//! The background thread is never joined or signalled to stop: it is
//! deliberately a detached, infinite poll loop, and the process exiting
//! (the frame loop ending for any reason) simply tears it down along with
//! every other thread. There is nothing to flush on shutdown that the
//! guest itself would notice -- the socket, if still open, just closes.
//!
//! # Client lifecycle
//!
//! - **No client yet**: guest output accumulates in the bounded,
//!   drop-oldest output queue (see [`OUTPUT_BUF_CAP`]) instead of
//!   blocking or being silently discarded outright -- a client that
//!   connects a few frames late still sees recent narration, not just
//!   whatever the guest says after it arrives. Host input has nowhere to
//!   come from, so nothing happens on that side.
//! - **Client connects mid-run**: the accept loop notices on its next
//!   poll (worst case [`POLL_INTERVAL`] late) and starts flushing the
//!   output queue to it and reading its input immediately -- no
//!   handshake of ours to complete first.
//! - **Client disconnects**: a failed write or a zero-byte read drops
//!   the connection and the thread goes back to accepting. Guest output
//!   produced with no client connected keeps accumulating (bounded, as
//!   above) rather than being lost outright, so a client that reconnects
//!   promptly still sees what it missed.
//! - **Slow client**: writes are non-blocking; a write that would block
//!   re-queues its remainder (preserving order) rather than blocking the
//!   bridge thread (which would, transitively through the output queue
//!   filling and the frame loop still never blocking on *that*, not stall
//!   the guest -- but would stop draining guest output into the queue's
//!   headroom, degrading gracefully into the same drop-oldest behaviour
//!   as "no client").
//!
//! # Input backpressure is deliberately real
//!
//! Unlike guest output (real Paula has no flow control on its transmit
//! side either -- see `Chipset::push_serial_byte`'s own doc comment),
//! host input genuinely should push back: a client that sends faster
//! than the guest drains `SERDATR` must not have its bytes silently
//! dropped, because unlike a debug-narration byte, an input byte the
//! guest never sees can change what it does. So the bridge thread simply
//! stops reading from the socket once [`INPUT_BUF_CAP`] host bytes are
//! queued and undelivered, leaving the rest sitting in the OS's own TCP
//! receive buffer. That is genuine, protocol-free backpressure: the
//! client's own `send`/`write` calls stall exactly as they would against
//! a real, slow UART, and no byte is ever dropped on this side of the
//! wire.
//!
//! That queue is deliberately not drained into the guest as fast as its
//! own receive queue has room, even though nothing here models a baud
//! rate: `run.rs`'s `service_host_serial` hands over at most one byte per
//! serviced frame, the same pace `SerialScript`'s `SEND` already uses.
//! `Chipset::push_serial_in_byte` raises the RBF interrupt once per
//! accepted byte by design (its own doc comment); draining this bridge's
//! queue in one gulp fires that many interrupts back-to-back, which
//! empirically wedged the real Kickstart 3.2.2 ROMWack break-in test into
//! an exception storm instead of ever reaching the debugger. See
//! `service_host_serial`'s own comment for the fuller account.

use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Cap on guest->host bytes buffered while no client is connected (or a
/// connected one isn't draining fast enough). Sized well above a single
/// ROMWack banner/register-dump line -- generous for "a client connected
/// a moment late still sees recent narration", not for replaying an
/// entire session; unbounded buffering here would turn "client never
/// connects" into unbounded memory growth over a long interactive run.
const OUTPUT_BUF_CAP: usize = 4096;

/// Cap on host->guest bytes buffered after the socket but before the
/// guest's own receive queue (`Chipset`'s `SERIAL_IN_BUF_CAP`, 32 bytes)
/// has room for them. Comfortably larger than that queue so a burst (an
/// AmiPilot command line, a pasted paragraph at an interactive prompt)
/// isn't immediately throttled at the socket the instant it outruns the
/// guest's own tiny queue, while still small enough that the backpressure
/// described in this module's doc comment engages promptly rather than
/// masking a genuinely stuck guest for a long time.
const INPUT_BUF_CAP: usize = 512;

/// How long the bridge thread sleeps between poll iterations when there
/// is nothing to do. Bounds how stale "client connected"/"client input
/// arrived" can appear to the frame loop; short enough to feel
/// interactive, long enough that the thread does not spin.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// A live, bidirectional TCP bridge to the guest's serial port. See the
/// module doc comment for the full lifecycle and threading rationale.
pub struct SerialTcpBridge {
    output: Arc<Mutex<VecDeque<u8>>>,
    input: Arc<Mutex<VecDeque<u8>>>,
    local_addr: SocketAddr,
}

impl SerialTcpBridge {
    /// Bind `addr` (e.g. `"127.0.0.1:1234"`) and start the bridge thread.
    /// Binding happens synchronously so a bad address (already in use,
    /// unparseable) is reported as a normal setup error rather than
    /// surfacing later from a background thread; everything past that
    /// (accepting, reading, writing) happens off-thread. `addr` may use
    /// port `0` to let the OS choose a free port -- see
    /// [`SerialTcpBridge::local_addr`] to find out which one it picked
    /// (this is how this module's own test avoids hardcoding a port).
    pub fn start(addr: &str) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;

        let output = Arc::new(Mutex::new(VecDeque::new()));
        let input = Arc::new(Mutex::new(VecDeque::new()));
        let output_for_thread = Arc::clone(&output);
        let input_for_thread = Arc::clone(&input);

        thread::Builder::new()
            .name("serial-tcp".into())
            .spawn(move || poll_loop(listener, output_for_thread, input_for_thread))
            .expect("spawn serial-tcp bridge thread");

        Ok(Self {
            output,
            input,
            local_addr,
        })
    }

    /// The address actually bound -- in particular the real port when
    /// `addr` was given as port `0`.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Queue one guest->host byte. Never blocks: drops the oldest
    /// buffered byte to make room once [`OUTPUT_BUF_CAP`] is reached,
    /// the same "no flow control, drop rather than stall" policy real
    /// unconnected serial hardware (and this crate's own
    /// `Chipset::push_serial_byte`) already has.
    pub fn push_guest_byte(&self, byte: u8) {
        let mut q = self.output.lock().unwrap();
        if q.len() >= OUTPUT_BUF_CAP {
            q.pop_front();
        }
        q.push_back(byte);
    }

    /// Take the next queued host->guest byte, if any. Never blocks --
    /// call once per serviced frame and feed the result to
    /// `Chipset::push_serial_in_byte` only when
    /// `Chipset::serial_in_has_room` says there's room, exactly like
    /// `SerialScript`'s own `SEND` pacing.
    pub fn try_recv_host_byte(&self) -> Option<u8> {
        self.input.lock().unwrap().pop_front()
    }
}

fn poll_loop(
    listener: TcpListener,
    output: Arc<Mutex<VecDeque<u8>>>,
    input: Arc<Mutex<VecDeque<u8>>>,
) {
    let mut client: Option<TcpStream> = None;
    loop {
        if client.is_none() {
            match listener.accept() {
                Ok((stream, _peer)) => {
                    // Best-effort: nothing here is load-bearing enough to
                    // fail the connection over -- a stream that somehow
                    // rejects these still works, just without Nagle
                    // disabled.
                    let _ = stream.set_nonblocking(true);
                    let _ = stream.set_nodelay(true);
                    client = Some(stream);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => {}
            }
        }

        if let Some(stream) = client.as_mut() {
            if !flush_output(stream, &output) {
                client = None;
            }
        }
        if let Some(stream) = client.as_mut() {
            if !fill_input(stream, &input) {
                client = None;
            }
        }

        thread::sleep(POLL_INTERVAL);
    }
}

/// Write as much of the queued guest->host output as the socket accepts
/// right now, re-queuing (at the front, preserving order) whatever a
/// non-blocking write couldn't take. Returns `false` if the connection
/// is gone and should be dropped.
fn flush_output(stream: &mut TcpStream, output: &Arc<Mutex<VecDeque<u8>>>) -> bool {
    let pending: Vec<u8> = {
        let mut q = output.lock().unwrap();
        q.drain(..).collect()
    };
    if pending.is_empty() {
        return true;
    }
    match stream.write(&pending) {
        Ok(n) if n < pending.len() => {
            let mut q = output.lock().unwrap();
            for byte in pending[n..].iter().rev() {
                q.push_front(*byte);
            }
            true
        }
        Ok(_) => true,
        Err(e) if e.kind() == ErrorKind::WouldBlock => {
            let mut q = output.lock().unwrap();
            for byte in pending.iter().rev() {
                q.push_front(*byte);
            }
            true
        }
        Err(_) => false, // broken pipe / reset -- the client is gone
    }
}

/// Read whatever host->guest input the socket has ready into the shared
/// queue, stopping (leaving bytes in the OS's own receive buffer -- see
/// the module doc comment's backpressure section) once [`INPUT_BUF_CAP`]
/// is reached. Returns `false` if the connection is gone and should be
/// dropped.
fn fill_input(stream: &mut TcpStream, input: &Arc<Mutex<VecDeque<u8>>>) -> bool {
    loop {
        {
            let q = input.lock().unwrap();
            if q.len() >= INPUT_BUF_CAP {
                return true; // full: apply backpressure, try again next poll
            }
        }
        let mut buf = [0u8; 256];
        match stream.read(&mut buf) {
            Ok(0) => return false, // client closed the connection
            Ok(n) => input.lock().unwrap().extend(buf[..n].iter().copied()),
            Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream as TestStream;
    use std::time::Instant;

    /// Poll `f` until it returns `true` or `timeout` elapses, sleeping
    /// briefly between attempts -- every assertion here depends on the
    /// bridge's background thread, which runs on its own schedule.
    fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if f() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn bytes_flow_guest_to_host_over_loopback() {
        let bridge = SerialTcpBridge::start("127.0.0.1:0").unwrap();
        let mut client = TestStream::connect(bridge.local_addr()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        for &b in b"rom-wack> " {
            bridge.push_guest_byte(b);
        }

        let mut got = Vec::new();
        let mut buf = [0u8; 32];
        while got.len() < b"rom-wack> ".len() {
            let n = client.read(&mut buf).expect("read from bridge");
            assert!(n > 0, "connection closed before all bytes arrived");
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, b"rom-wack> ");
    }

    #[test]
    fn bytes_flow_host_to_guest_over_loopback() {
        let bridge = SerialTcpBridge::start("127.0.0.1:0").unwrap();
        let mut client = TestStream::connect(bridge.local_addr()).unwrap();
        client.write_all(b"\x7f\r").unwrap();

        let mut received = Vec::new();
        let ok = wait_until(Duration::from_secs(5), || {
            while let Some(b) = bridge.try_recv_host_byte() {
                received.push(b);
            }
            received.len() >= 2
        });
        assert!(ok, "bytes sent by the client never reached the bridge");
        assert_eq!(received, vec![0x7f, b'\r']);
    }

    #[test]
    fn output_buffers_with_no_client_connected_and_flushes_on_connect() {
        let bridge = SerialTcpBridge::start("127.0.0.1:0").unwrap();
        for &b in b"buffered" {
            bridge.push_guest_byte(b);
        }
        // No client yet -- give the bridge thread a moment to prove it
        // isn't discarding the bytes by writing them nowhere.
        thread::sleep(Duration::from_millis(20));

        let mut client = TestStream::connect(bridge.local_addr()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 32];
        while got.len() < b"buffered".len() {
            let n = client.read(&mut buf).expect("read from bridge");
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, b"buffered");
    }

    #[test]
    fn reconnecting_client_gets_a_fresh_session() {
        let bridge = SerialTcpBridge::start("127.0.0.1:0").unwrap();
        {
            let client = TestStream::connect(bridge.local_addr()).unwrap();
            drop(client); // immediate disconnect
        }
        // Give the bridge thread a moment to notice the drop and return
        // to accepting.
        thread::sleep(Duration::from_millis(20));

        let mut client2 = TestStream::connect(bridge.local_addr()).unwrap();
        client2
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        bridge.push_guest_byte(b'!');
        let mut buf = [0u8; 1];
        let n = client2
            .read(&mut buf)
            .expect("read from reconnected client");
        assert_eq!(&buf[..n], b"!");
    }

    #[test]
    fn input_backpressure_stops_reading_once_the_queue_is_full() {
        let bridge = SerialTcpBridge::start("127.0.0.1:0").unwrap();
        let mut client = TestStream::connect(bridge.local_addr()).unwrap();
        // Send more than INPUT_BUF_CAP bytes; the bridge should stop
        // draining the socket once its queue is full rather than growing
        // it unbounded.
        let payload = vec![b'x'; INPUT_BUF_CAP * 2];
        // A background writer thread: the client's own `write` may block
        // once the OS socket buffers and this test's queue both fill,
        // which is exactly the backpressure this test is checking for.
        let writer = thread::spawn(move || {
            let _ = client.write_all(&payload);
        });

        let filled = wait_until(Duration::from_secs(5), || {
            bridge.input.lock().unwrap().len() >= INPUT_BUF_CAP
        });
        assert!(filled, "input queue never reached its cap");
        assert!(
            bridge.input.lock().unwrap().len() <= INPUT_BUF_CAP,
            "input queue grew past its cap -- backpressure did not engage"
        );

        // Drain repeatedly (the bridge thread keeps refilling from the
        // OS's own receive buffer as room opens up) so the writer thread
        // finishes and this test doesn't leave a blocked thread behind.
        wait_until(Duration::from_secs(5), || {
            while bridge.try_recv_host_byte().is_some() {}
            writer.is_finished()
        });
        let _ = writer.join();
    }
}
