//! The serial console: Phase 1's only observable output.
//!
//! Two independent sources feed it — bytes the guest writes to `SERDAT`,
//! and this runner's own diagnostics (progress reports, trace lines, the
//! exit summary) — and they are kept visually distinguishable so a human
//! staring at CI log output can tell "the ROM said this" from "the runner
//! is telling you something about the ROM". `GUEST|` lines are the actual
//! Phase 1 exit evidence; `host  |` lines are everything else.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

pub struct Console {
    log: Option<File>,
    /// Bytes accumulated for the current guest output line, flushed on
    /// `\n` or (best-effort) at process exit.
    guest_line: Vec<u8>,
}

impl Console {
    pub fn new(log_path: Option<&Path>) -> io::Result<Self> {
        let log = log_path.map(File::create).transpose()?;
        Ok(Self {
            log,
            guest_line: Vec::new(),
        })
    }

    /// Runner diagnostic: progress reports, trace lines, the exit summary.
    pub fn diag(&mut self, line: &str) {
        self.write_line("host  |", line);
    }

    /// One byte the guest wrote to `SERDAT`.
    ///
    /// # Integration point
    ///
    /// `machine_core::chipset::Chipset` does not yet expose a way to
    /// observe `SERDAT` writes — `Chipset::write` discards them (see
    /// `crates/machine-core/src/chipset.rs`'s `_ => {}` arm), and
    /// `SERDATR`'s read side is hardwired to report "transmit buffer
    /// empty" rather than reflecting any transmitted byte. This method
    /// exists so that once a concurrent worker adds a
    /// `take_serial_byte() -> Option<u8>`-style drain hook to `Chipset`,
    /// wiring it in `main.rs`'s run loop is a one-line change: call it
    /// once per instruction (or once per `tick`) and feed anything it
    /// returns to this method. Nothing calls this method today.
    #[allow(dead_code)]
    pub fn guest_byte(&mut self, byte: u8) {
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
