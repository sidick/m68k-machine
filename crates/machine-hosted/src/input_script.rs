//! `--input-script`: a small, deterministic host→guest input-card event
//! language, driven one frame boundary at a time from `run.rs` -- the
//! same shape `serial_script.rs` uses for `--serial-script`, adapted for
//! `machine_core::input::NativeInput` instead of the chipset's serial
//! port.
//!
//! # Why a script, not a live input source
//!
//! `machine-hosted` is headless -- screenshots only, no window -- so
//! there is nowhere for a keypress or a mouse click to come from except
//! something scripted. A script file is also what makes this testable in
//! CI at all (the brief's own framing): a fixed, deterministic sequence
//! of key/button/motion events, paced in emulated frames exactly like
//! `--serial-script`, needs no interactive terminal and no real time.
//!
//! # Language
//!
//! One directive per line; blank lines and lines starting with `#` are
//! ignored -- identical convention to `serial_script.rs`.
//!
//! - `KEYDOWN <code>` / `KEYUP <code>` -- push a key transition
//!   ([`machine_core::input::NativeInput::push_key`]). `<code>` is the
//!   raw Amiga key code, decimal or `0x`-prefixed hex, `0x00`-`0x7F`
//!   (`machine_core::input::MAX_RAW_KEYCODE`, per NDK 3.2
//!   `devices/inputevent.h`'s `IECODE_KEY_CODE_LAST`/`IECODE_COMM_CODE_LAST`).
//! - `MOVE <x> <y>` -- push an absolute pointer position
//!   ([`NativeInput::push_pointer_motion`]). `<x>`/`<y>` are signed
//!   decimal, `i16::MIN..=i16::MAX` (`InputEvent::ie_X`/`ie_Y` are
//!   `WORD` -- see `machine_core::input`'s module docs).
//! - `BUTTONDOWN <button>` / `BUTTONUP <button>` -- push a button
//!   transition ([`NativeInput::push_button`]). `<button>` is `LEFT`,
//!   `RIGHT`, `MIDDLE` (case-insensitive) or a numeric button id.
//! - `SLEEP <frames>` -- wait `<frames>` chipset frames unconditionally
//!   before the next directive, identical to `serial_script.rs`'s
//!   `SLEEP`.
//!
//! Every directive except `SLEEP` takes effect immediately (the same
//! tick it is reached) and moves straight on to whatever follows, since
//! none of them can block on guest state the way `serial_script.rs`'s
//! `WAIT` blocks on serial output -- there is no equivalent "wait for a
//! reaction" primitive here because no driver exists yet to produce one
//! (`docs/input-protocol.md` §12 lists that as future work).
//!
//! A directive whose payload [`machine_core::input::NativeInput`] itself
//! rejects (an out-of-range key code, button id, or coordinate) is a
//! **parse-time** error, not a silently dropped event: a script that
//! specifies a coordinate outside `i16`'s range, for instance, could
//! never have described a real `InputEvent`, so it is treated the same
//! way `serial_script.rs` treats an unrecognised directive -- caught
//! before the run even starts, rather than discovered as a mysteriously
//! missing event partway through one.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::Path;

use machine_core::input::NativeInput;
use machine_core::input::{button, MAX_BUTTON, MAX_RAW_KEYCODE};

#[derive(Debug, Clone, Copy)]
enum Directive {
    KeyDown(u8),
    KeyUp(u8),
    Move(i32, i32),
    ButtonDown(u8),
    ButtonUp(u8),
    Sleep(u64),
}

pub struct InputScript {
    remaining: VecDeque<Directive>,
    /// Set only by `SLEEP`; every other directive fires and clears in
    /// the same [`InputScript::tick`] call, so this is the only state
    /// that needs to persist across calls.
    sleep_deadline: Option<u64>,
}

impl InputScript {
    /// Parse a script file. Returns an error only for an unreadable file
    /// or a line that doesn't parse -- see `serial_script::SerialScript::load`'s
    /// doc comment for why that's the right posture (a script the user
    /// actually intended to run should never be silently reinterpreted).
    pub fn load(path: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(path)?;
        Self::parse(&text).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {e}", path.display()),
            )
        })
    }

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
            sleep_deadline: None,
        })
    }

    /// Whether every directive has finished.
    pub fn is_done(&self) -> bool {
        self.sleep_deadline.is_none() && self.remaining.is_empty()
    }

    /// Advance the script by one frame boundary. Call this once per
    /// chipset frame, the same cadence `SerialScript::tick` uses --
    /// input events don't need per-instruction granularity any more than
    /// serial bytes do.
    pub fn tick(&mut self, frame: u64, input: &mut NativeInput) {
        loop {
            if let Some(deadline) = self.sleep_deadline {
                if frame < deadline {
                    return;
                }
                self.sleep_deadline = None;
            }
            let Some(directive) = self.remaining.pop_front() else {
                return;
            };
            match directive {
                Directive::KeyDown(code) => {
                    input.push_key(code, true, 0);
                }
                Directive::KeyUp(code) => {
                    input.push_key(code, false, 0);
                }
                Directive::Move(x, y) => {
                    input.push_pointer_motion(x, y, 0);
                }
                Directive::ButtonDown(id) => {
                    input.push_button(id, true, 0);
                }
                Directive::ButtonUp(id) => {
                    input.push_button(id, false, 0);
                }
                Directive::Sleep(frames) => {
                    self.sleep_deadline = Some(frame.saturating_add(frames));
                }
            }
        }
    }
}

fn parse_line(line: &str) -> Result<Directive, String> {
    let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    match cmd {
        "KEYDOWN" => Ok(Directive::KeyDown(parse_keycode(rest)?)),
        "KEYUP" => Ok(Directive::KeyUp(parse_keycode(rest)?)),
        "MOVE" => {
            let mut parts = rest.split_whitespace();
            let x = parts
                .next()
                .ok_or("MOVE: expected <x> <y>")?
                .parse::<i32>()
                .map_err(|e| format!("MOVE: x: {e}"))?;
            let y = parts
                .next()
                .ok_or("MOVE: expected <x> <y>")?
                .parse::<i32>()
                .map_err(|e| format!("MOVE: y: {e}"))?;
            if parts.next().is_some() {
                return Err("MOVE: too many arguments".to_string());
            }
            if x < i16::MIN as i32 || x > i16::MAX as i32 {
                return Err(format!("MOVE: x {x} out of i16 range (InputEvent::ie_X)"));
            }
            if y < i16::MIN as i32 || y > i16::MAX as i32 {
                return Err(format!("MOVE: y {y} out of i16 range (InputEvent::ie_Y)"));
            }
            Ok(Directive::Move(x, y))
        }
        "BUTTONDOWN" => Ok(Directive::ButtonDown(parse_button(rest)?)),
        "BUTTONUP" => Ok(Directive::ButtonUp(parse_button(rest)?)),
        "SLEEP" => rest
            .parse::<u64>()
            .map(Directive::Sleep)
            .map_err(|e| format!("SLEEP: {e}")),
        other => Err(format!("unknown directive {other:?}")),
    }
}

/// Decimal or `0x`-prefixed hex, checked against
/// [`machine_core::input::MAX_RAW_KEYCODE`] here (at parse time) rather
/// than only at [`NativeInput::push_key`] -- module doc comment's
/// "hostile input fails at parse time, not silently mid-run" rationale.
fn parse_keycode(text: &str) -> Result<u8, String> {
    let code = parse_numeric(text).map_err(|e| format!("key code: {e}"))?;
    if code > MAX_RAW_KEYCODE as u32 {
        return Err(format!(
            "key code {code:#x} exceeds MAX_RAW_KEYCODE ({MAX_RAW_KEYCODE:#x})"
        ));
    }
    Ok(code as u8)
}

fn parse_button(text: &str) -> Result<u8, String> {
    let id = match text.to_ascii_uppercase().as_str() {
        "LEFT" => button::LEFT,
        "RIGHT" => button::RIGHT,
        "MIDDLE" => button::MIDDLE,
        other => {
            let n = parse_numeric(other).map_err(|e| format!("button: {e}"))?;
            if n > MAX_BUTTON as u32 {
                return Err(format!("button id {n} exceeds MAX_BUTTON ({MAX_BUTTON})"));
            }
            n as u8
        }
    };
    Ok(id)
}

fn parse_numeric(text: &str) -> Result<u32, String> {
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).map_err(|e| e.to_string())
    } else {
        text.parse::<u32>().map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peek(input: &NativeInput) -> (u8, u8, u16, i32, i32) {
        use machine_core::input::reg;
        let read_u32 = |base: u32| {
            u32::from_be_bytes([
                input.read(base),
                input.read(base + 1),
                input.read(base + 2),
                input.read(base + 3),
            ])
        };
        (
            input.read(reg::EVENT_TYPE + 3),
            input.read(reg::EVENT_CODE + 3),
            read_u32(reg::EVENT_QUALIFIER) as u16,
            read_u32(reg::EVENT_X) as i32,
            read_u32(reg::EVENT_Y) as i32,
        )
    }

    fn advance(input: &mut NativeInput) {
        use machine_core::input::reg;
        input.write(reg::EVENT_ADVANCE + 3, 0);
    }

    #[test]
    fn keydown_and_keyup_apply_immediately_and_move_on() {
        let mut script = InputScript::parse("KEYDOWN 0x41\nKEYUP 0x41\n").unwrap();
        let mut dev = NativeInput::new();
        script.tick(0, &mut dev);
        assert!(script.is_done());

        use machine_core::input::ev;
        let (ty, code, ..) = peek(&dev);
        assert_eq!(ty, ev::KEY_DOWN);
        assert_eq!(code, 0x41);
        advance(&mut dev);
        let (ty, code, ..) = peek(&dev);
        assert_eq!(ty, ev::KEY_UP);
        assert_eq!(code, 0x41);
    }

    #[test]
    fn move_pushes_absolute_coordinates() {
        let mut script = InputScript::parse("MOVE -5 100\n").unwrap();
        let mut dev = NativeInput::new();
        script.tick(0, &mut dev);
        let (ty, _code, _q, x, y) = peek(&dev);
        assert_eq!(ty, machine_core::input::ev::POINTER_MOTION);
        assert_eq!(x, -5);
        assert_eq!(y, 100);
    }

    #[test]
    fn button_names_and_numeric_ids_both_parse() {
        let script = InputScript::parse("BUTTONDOWN LEFT\nBUTTONUP 1\n").unwrap();
        assert_eq!(script.remaining.len(), 2);
    }

    #[test]
    fn sleep_blocks_until_its_frame_count_elapses() {
        let mut script = InputScript::parse("SLEEP 3\nKEYDOWN 1\n").unwrap();
        let mut dev = NativeInput::new();
        script.tick(0, &mut dev);
        assert!(!script.is_done());
        assert_eq!(dev.read(machine_core::input::reg::EVENT_TYPE + 3), 0);
        script.tick(3, &mut dev);
        assert!(script.is_done());
        assert_eq!(
            dev.read(machine_core::input::reg::EVENT_TYPE + 3),
            machine_core::input::ev::KEY_DOWN
        );
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let script = InputScript::parse("# a comment\n\nSLEEP 1\n").unwrap();
        assert_eq!(script.remaining.len(), 1);
    }

    #[test]
    fn unknown_directive_is_a_parse_error() {
        assert!(InputScript::parse("FROB x").is_err());
    }

    // ---- hostile input: rejected at parse time, not silently mid-run -----

    #[test]
    fn a_keycode_past_the_documented_range_is_a_parse_error() {
        assert!(InputScript::parse("KEYDOWN 0x80").is_err());
        assert!(InputScript::parse("KEYDOWN 999").is_err());
    }

    #[test]
    fn an_out_of_range_coordinate_is_a_parse_error() {
        assert!(InputScript::parse("MOVE 40000 0").is_err());
        assert!(InputScript::parse("MOVE 0 -40000").is_err());
    }

    #[test]
    fn an_unknown_button_id_is_a_parse_error() {
        assert!(InputScript::parse("BUTTONDOWN 99").is_err());
        assert!(InputScript::parse("BUTTONDOWN FROBNICATE").is_err());
    }
}
