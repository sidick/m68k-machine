//! The serial console: Phase 1's only observable output.
//!
//! Two independent sources feed it — bytes the guest writes to `SERDAT`,
//! and this runner's own diagnostics (progress reports, trace lines, the
//! exit summary) — and they are kept visually distinguishable so a human
//! staring at CI log output can tell "the ROM said this" from "the runner
//! is telling you something about the ROM". `GUEST|` lines are the actual
//! Phase 1 exit evidence; `host  |` lines are everything else.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

/// How many of the most recent raw guest serial bytes [`Console::guest_tail_contains`]
/// searches. Bounded (not the whole run's output) for the same reason
/// `machine_core::chipset`'s ring buffers are bounded: this exists to let
/// a `--serial-script` `WAIT` directive recognise a prompt or banner
/// shortly after it appears, not to replay the whole session -- a few
/// ROMWack lines' worth is generous headroom over the longest string any
/// script here waits for.
const GUEST_TAIL_CAP: usize = 4096;

pub struct Console {
    log: Option<File>,
    /// Bytes accumulated for the current guest output line, flushed on
    /// `\n` or (best-effort) at process exit.
    guest_line: Vec<u8>,
    /// Raw guest bytes, independent of `guest_line`'s line-buffering, so
    /// a `WAIT` for text with no trailing newline (a `_` handshake echo,
    /// a `>` prompt) can still be recognised before the line completes.
    /// See [`GUEST_TAIL_CAP`].
    guest_tail: VecDeque<u8>,
}

impl Console {
    pub fn new(log_path: Option<&Path>) -> io::Result<Self> {
        let log = log_path.map(File::create).transpose()?;
        Ok(Self {
            log,
            guest_line: Vec::new(),
            guest_tail: VecDeque::with_capacity(GUEST_TAIL_CAP),
        })
    }

    /// Whether `needle` appears anywhere in the most recent
    /// [`GUEST_TAIL_CAP`] bytes of guest serial output. Used by
    /// `serial_script`'s `WAIT` directive; a naive windowed scan is fine
    /// at this size and call frequency (once per frame boundary, not per
    /// byte).
    pub fn guest_tail_contains(&self, needle: &[u8]) -> bool {
        if needle.is_empty() || needle.len() > self.guest_tail.len() {
            return false;
        }
        // `VecDeque` isn't contiguous in general; a `Vec` copy keeps the
        // window search simple and this is bounded to GUEST_TAIL_CAP
        // bytes, called a few times a second at most.
        let tail: Vec<u8> = self.guest_tail.iter().copied().collect();
        tail.windows(needle.len()).any(|w| w == needle)
    }

    /// Runner diagnostic: progress reports, trace lines, the exit summary.
    pub fn diag(&mut self, line: &str) {
        self.write_line("host  |", line);
    }

    /// One byte the guest wrote to `SERDAT`.
    ///
    /// Fed from `run.rs`'s `drain_serial`, which drains
    /// `Chipset::take_serial_byte` once per bus tick (both in the normal
    /// per-instruction hook and while the CPU is stopped) -- this is
    /// Phase 1's only observable evidence of reaching the boot menu
    /// (roadmap Phase 1 exit criterion).
    pub fn guest_byte(&mut self, byte: u8) {
        self.guest_tail.push_back(byte);
        if self.guest_tail.len() > GUEST_TAIL_CAP {
            self.guest_tail.pop_front();
        }
        if byte == b'\n' {
            self.flush_guest_line();
            return;
        }
        self.guest_line.push(byte);
        if self.guest_line.len() >= 512 {
            // Guard against an unterminated flood of guest bytes
            // (e.g. a wedged UART loop) filling memory unbounded.
            self.flush_guest_line();
        }
    }

    fn flush_guest_line(&mut self) {
        if self.guest_line.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.guest_line).into_owned();
        self.guest_line.clear();
        self.write_line("GUEST |", &text);
    }

    fn write_line(&mut self, prefix: &str, line: &str) {
        println!("{prefix} {line}");
        if let Some(log) = &mut self.log {
            let _ = writeln!(log, "{prefix} {line}");
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.flush_guest_line();
    }
}
