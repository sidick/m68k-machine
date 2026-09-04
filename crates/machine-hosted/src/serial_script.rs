//! `--serial-script`: a small, deterministic host→guest serial input
//! language, driven one frame boundary at a time from `run.rs`.
//!
//! # Why a script rather than a raw `--serial-in` byte dump
//!
//! Handing the guest a fixed blob of bytes up front (the obvious minimal
//! design) cannot express the ROMWack handshake this crate exists to
//! reach: the host has to send `_` and look at what comes back before it
//! knows whether to send more `_`, a `\r`, or a command line. A script
//! that can wait on guest output between sends is the smallest facility
//! that covers both "just inject a DEL to break in" (a one-line script)
//! and the handshake itself, while staying fully scripted -- no
//! interactive terminal, no real time, everything paced in emulated
//! frames -- so a CI-style test can run it non-interactively with a
//! bounded, deterministic timeout instead of an open-ended human session.
//!
//! # Language
//!
//! One directive per line; blank lines and lines starting with `#` are
//! ignored.
//!
//! - `SEND <text>` -- queue bytes for the guest's serial receiver
//!   ([`machine_core::chipset::Chipset::push_serial_in_byte`]). `<text>`
//!   supports `\r`, `\n`, `\t`, `\\` and `\xNN` (hex byte) escapes, so a
//!   raw control byte like DEL (`\x7f`) can be sent without needing a
//!   literal unprintable character in the script file.
//! - `WAIT <text> [max_frames]` -- block the script (not the guest, which
//!   keeps running) until `<text>` appears in the guest's recent serial
//!   output ([`crate::console::Console::guest_tail_contains`]), or
//!   `max_frames` chipset frames pass (default [`DEFAULT_WAIT_FRAMES`]).
//!   A timeout is logged and the script continues rather than aborting --
//!   the brief this module was written against is explicit that the ROM
//!   is the authority on its own protocol, not the documentation a
//!   `WAIT` was written against, so a script that free-runs past an
//!   unmet expectation and reports what actually happened is more useful
//!   than one that just stops.
//! - `SLEEP <frames>` -- wait `<frames>` chipset frames unconditionally,
//!   with no output condition -- for pacing between sends when there is
//!   nothing distinctive to `WAIT` for yet.
//!
//! `SEND` paces itself at one byte per frame boundary and only when
//! [`Chipset::serial_in_has_room`] says the receive queue isn't full, so
//! a script that sends more bytes than the queue holds at once never
//! trips the chipset's own overrun path -- that path exists for a
//! genuine host/guest speed mismatch, not for this driver's own batching.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::Path;

use machine_core::chipset::Chipset;

use crate::console::Console;

/// Default timeout for a `WAIT` directive that doesn't specify one: 20
/// seconds of PAL frame time (50 Hz), generous for a ROM's alert/boot
/// path but still far short of `--max-frames`' own bound, so a script
/// that hangs on one `WAIT` doesn't silently eat the whole run budget.
const DEFAULT_WAIT_FRAMES: u64 = 1000;

#[derive(Debug, Clone)]
enum Directive {
    Send(Vec<u8>),
    Wait { needle: Vec<u8>, max_frames: u64 },
    Sleep(u64),
}

/// One directive currently in progress, with whatever progress state it
/// needs across repeated [`SerialScript::tick`] calls.
enum InFlight {
    Send {
        bytes: VecDeque<u8>,
    },
    Wait {
        needle: Vec<u8>,
        deadline_frame: u64,
    },
    Sleep {
        deadline_frame: u64,
    },
}

pub struct SerialScript {
    remaining: VecDeque<Directive>,
    current: Option<InFlight>,
}

impl SerialScript {
    /// Parse a script file. Returns an error only for an unreadable file
    /// or a line that doesn't parse -- a script the user actually
    /// intended to run should never be silently reinterpreted.
    pub fn load(path: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(path)?;
        Self::parse(&text).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {e}", path.display()),
            )
        })
    }

    /// Parse script text directly (the file-reading half of [`load`]
    /// split out so tests don't need a real file on disk).
    ///
    /// [`load`]: SerialScript::load
    fn parse(text: &str) -> Result<Self, String> {
        let mut remaining = VecDeque::new();
        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let directive = parse_line(line).map_err(|e| format!("line {}: {e}", lineno + 1))?;
            remaining.push_back(directive);
        }
        Ok(Self {
            remaining,
            current: None,
        })
    }

    /// Whether every directive has finished (a `WAIT` that timed out
    /// still counts as finished -- see the module doc comment).
    pub fn is_done(&self) -> bool {
        self.current.is_none() && self.remaining.is_empty()
    }

    /// Advance the script by one frame boundary. Call this once per
    /// chipset frame (not per instruction -- a byte-per-frame pace is
    /// already far faster than any real baud rate this machine doesn't
    /// model, see `chipset.rs`'s `SERPER` doc comment, and calling it
    /// once per instruction would just spin re-checking the same
    /// directive thousands of times between frame boundaries for no
    /// benefit).
    pub fn tick(&mut self, frame: u64, chipset: &mut Chipset, console: &mut Console) {
        loop {
            if self.current.is_none() {
                self.current = match self.remaining.pop_front() {
                    Some(Directive::Send(bytes)) => Some(InFlight::Send {
                        bytes: bytes.into(),
                    }),
                    Some(Directive::Wait { needle, max_frames }) => {
                        if needle.is_empty() || console.guest_tail_contains(&needle) {
                            console.diag(&format!(
                                "serial-script: WAIT already satisfied: {:?}",
                                String::from_utf8_lossy(&needle)
                            ));
                            None
                        } else {
                            Some(InFlight::Wait {
                                needle,
                                deadline_frame: frame.saturating_add(max_frames),
                            })
                        }
                    }
                    Some(Directive::Sleep(frames)) => Some(InFlight::Sleep {
                        deadline_frame: frame.saturating_add(frames),
                    }),
                    None => return,
                };
                if self.current.is_none() {
                    continue; // an already-satisfied WAIT: move straight to the next directive
                }
            }

            match self.current.as_mut() {
                Some(InFlight::Send { bytes }) => {
                    if bytes.is_empty() {
                        self.current = None;
                        continue;
                    }
                    if chipset.serial_in_has_room() {
                        if let Some(b) = bytes.pop_front() {
                            chipset.push_serial_in_byte(b);
                        }
                    }
                    if bytes.is_empty() {
                        // Finished as of this tick -- don't make the caller
                        // wait an extra, otherwise-idle tick just to see
                        // `is_done()` flip.
                        self.current = None;
                    }
                    // At most one byte per frame, whether or not room was
                    // available this time -- come back on the next tick.
                    return;
                }
                Some(InFlight::Wait {
                    needle,
                    deadline_frame,
                }) => {
                    if console.guest_tail_contains(needle) {
                        console.diag(&format!(
                            "serial-script: WAIT matched: {:?}",
                            String::from_utf8_lossy(needle)
                        ));
                        self.current = None;
                        continue;
                    }
                    if frame >= *deadline_frame {
                        console.diag(&format!(
                            "serial-script: WAIT timed out (never saw {:?}) -- \
                             continuing, treating the ROM as authoritative over \
                             the script's expectation",
                            String::from_utf8_lossy(needle)
                        ));
                        self.current = None;
                        continue;
                    }
                    return;
                }
                Some(InFlight::Sleep { deadline_frame }) => {
                    if frame >= *deadline_frame {
                        self.current = None;
                        continue;
                    }
                    return;
                }
                None => return,
            }
        }
    }
}

fn parse_line(line: &str) -> Result<Directive, String> {
    let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim_start();
    match cmd {
        "SEND" => Ok(Directive::Send(unescape(rest)?)),
        "WAIT" => {
            // The text may itself contain spaces once unescaped, but the
            // optional trailing max_frames is a bare integer with no
            // spaces, so split on the last whitespace-separated token
            // only when it parses as one.
            if let Some((text, count)) = rest.rsplit_once(char::is_whitespace) {
                if let Ok(max_frames) = count.trim().parse::<u64>() {
                    return Ok(Directive::Wait {
                        needle: unescape(text.trim_end())?,
                        max_frames,
                    });
                }
            }
            Ok(Directive::Wait {
                needle: unescape(rest)?,
                max_frames: DEFAULT_WAIT_FRAMES,
            })
        }
        "SLEEP" => rest
            .trim()
            .parse::<u64>()
            .map(Directive::Sleep)
            .map_err(|e| format!("SLEEP: {e}")),
        other => Err(format!("unknown directive {other:?}")),
    }
}

/// Expand `\r`, `\n`, `\t`, `\\` and `\xNN` escapes in a script directive's
/// text argument into raw bytes.
fn unescape(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let next = *bytes.get(i + 1).ok_or("trailing backslash")?;
        match next {
            b'r' => {
                out.push(b'\r');
                i += 2;
            }
            b'n' => {
                out.push(b'\n');
                i += 2;
            }
            b't' => {
                out.push(b'\t');
                i += 2;
            }
            b'\\' => {
                out.push(b'\\');
                i += 2;
            }
            b'x' => {
                let hex = text.get(i + 2..i + 4).ok_or("truncated \\xNN escape")?;
                let byte = u8::from_str_radix(hex, 16).map_err(|e| format!("\\x escape: {e}"))?;
                out.push(byte);
                i += 4;
            }
            other => return Err(format!("unknown escape \\{}", other as char)),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_handles_all_escapes() {
        assert_eq!(unescape(r"a\rb\nc\td\\e").unwrap(), b"a\rb\nc\td\\e");
        assert_eq!(unescape(r"\x7f").unwrap(), vec![0x7F]);
        assert!(unescape(r"\q").is_err());
        assert!(unescape(r"\").is_err());
    }

    #[test]
    fn send_paces_one_byte_per_frame_and_respects_queue_room() {
        let mut chipset = Chipset::new();
        let mut console = Console::new(None).unwrap();
        let mut script = SerialScript::parse("SEND ab").unwrap();

        script.tick(0, &mut chipset, &mut console);
        assert_eq!(
            chipset.read(machine_core::chipset::reg::SERDATR) & 0xFF,
            b'a' as u16
        );
        script.tick(1, &mut chipset, &mut console);
        assert_eq!(
            chipset.read(machine_core::chipset::reg::SERDATR) & 0xFF,
            b'b' as u16
        );
        assert!(script.is_done());
    }

    #[test]
    fn wait_matches_immediately_if_already_satisfied() {
        let mut chipset = Chipset::new();
        let mut console = Console::new(None).unwrap();
        console.guest_byte(b'x');
        let mut script = SerialScript::parse("WAIT x").unwrap();
        script.tick(0, &mut chipset, &mut console);
        assert!(script.is_done());
    }

    #[test]
    fn wait_times_out_and_continues_rather_than_hanging() {
        let mut chipset = Chipset::new();
        let mut console = Console::new(None).unwrap();
        let mut script = SerialScript::parse("WAIT nope 2").unwrap();
        script.tick(0, &mut chipset, &mut console); // not yet
        assert!(!script.is_done());
        script.tick(2, &mut chipset, &mut console); // deadline reached
        assert!(script.is_done());
    }

    #[test]
    fn sleep_blocks_until_its_frame_count_elapses() {
        let mut chipset = Chipset::new();
        let mut console = Console::new(None).unwrap();
        let mut script = SerialScript::parse("SLEEP 3").unwrap();
        script.tick(0, &mut chipset, &mut console);
        assert!(!script.is_done());
        script.tick(3, &mut chipset, &mut console);
        assert!(script.is_done());
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let script = SerialScript::parse("# a comment\n\nSLEEP 1\n").unwrap();
        assert_eq!(script.remaining.len(), 1);
    }

    #[test]
    fn unknown_directive_is_a_parse_error() {
        assert!(SerialScript::parse("FROB x").is_err());
    }
}
