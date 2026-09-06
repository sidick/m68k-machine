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
//! - `CHARDOWN <char>` / `CHARUP <char>` -- push a character transition
//!   ([`NativeInput::push_char`]), mirroring `KEYDOWN`/`KEYUP`'s down/up
//!   shape but naming *what was typed* rather than *which key moved* --
//!   see `machine_core::input`'s "Character events" module docs and
//!   `docs/input-protocol.md` §15. `<char>` is either a single literal
//!   character (e.g. `a`, `@`) or a `0x`-prefixed Unicode code point,
//!   checked against [`machine_core::input::latin1_from_char`] at parse
//!   time -- a code point outside Latin-1 (`0x00`-`0xFF`) is a parse
//!   error, the same posture every other hostile input on this line takes.
//!   A literal space or other whitespace can only be written as a code
//!   point (`0x20`), since the directive line is split on whitespace
//!   first.
//! - `TYPE "<string>"` -- the common-case convenience: expands at parse
//!   time to a `CHARDOWN`/`CHARUP` pair for every character in the
//!   double-quoted string, in order, so a script author does not have to
//!   spell out sixteen directives to type "Hello, World!". `\"` and `\\`
//!   are the only recognised escapes (plus `\n`/`\t`, where `\n` emits
//!   **CR** (`0x0D`) because that is what Return is on this platform --
//!   see `parse_type`); every character in
//!   the string is subject to the same Latin-1 check `CHARDOWN` uses, and
//!   any failure is a parse-time error citing the offending character,
//!   not a partially-typed string discovered mid-run. A single character
//!   is exactly `TYPE "x"`, so there's no need for a third form -- kept as
//!   `TYPE` rather than overloading `CHARDOWN`/`CHARUP` because a script
//!   author typing text wants press-*and*-release per character, not a
//!   choice to make for every letter.
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
use machine_core::input::{button, latin1_from_char, MAX_BUTTON, MAX_RAW_KEYCODE};

#[derive(Debug, Clone, Copy)]
enum Directive {
    KeyDown(u8),
    KeyUp(u8),
    Move(i32, i32),
    ButtonDown(u8),
    ButtonUp(u8),
    CharDown(char),
    CharUp(char),
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
            let directives = parse_line(line).map_err(|e| format!("line {}: {e}", lineno + 1))?;
            remaining.extend(directives);
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
    ///
    /// The queue is finite ([`machine_core::input::QUEUE_CAPACITY`]) and
    /// the guest drains it on its own schedule, so a script that pushes
    /// without limit overruns it. Every `push_*` reports that by
    /// returning `false`; this honours the refusal by putting the
    /// directive back and retrying next frame, which is the only reason
    /// a `TYPE` longer than half the queue arrives intact.
    ///
    /// It used to ignore those return values, and the result was the
    /// worst kind of failure: `TYPE "System/Wanderer/Wanderer"` typed
    /// exactly `System/W` into the guest and reported success. Eight
    /// characters, because `TYPE` expands to a down/up *pair* each and
    /// the queue holds sixteen events -- a number that looks like a
    /// plausible answer rather than a truncation, which is what made it
    /// survive as long as it did.
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
            let accepted = match directive {
                Directive::KeyDown(code) => input.push_key(code, true, 0),
                Directive::KeyUp(code) => input.push_key(code, false, 0),
                Directive::Move(x, y) => input.push_pointer_motion(x, y, 0),
                Directive::ButtonDown(id) => input.push_button(id, true, 0),
                Directive::ButtonUp(id) => input.push_button(id, false, 0),
                Directive::CharDown(ch) => input.push_char(ch, true),
                Directive::CharUp(ch) => input.push_char(ch, false),
                Directive::Sleep(frames) => {
                    self.sleep_deadline = Some(frame.saturating_add(frames));
                    true
                }
            };
            if !accepted {
                // Full. Un-pop and let the guest drain a frame's worth;
                // the directive is retried, never skipped.
                self.remaining.push_front(directive);
                return;
            }
        }
    }
}

fn parse_line(line: &str) -> Result<Vec<Directive>, String> {
    let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    match cmd {
        "KEYDOWN" => Ok(vec![Directive::KeyDown(parse_keycode(rest)?)]),
        "KEYUP" => Ok(vec![Directive::KeyUp(parse_keycode(rest)?)]),
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
            Ok(vec![Directive::Move(x, y)])
        }
        "BUTTONDOWN" => Ok(vec![Directive::ButtonDown(parse_button(rest)?)]),
        "BUTTONUP" => Ok(vec![Directive::ButtonUp(parse_button(rest)?)]),
        "CHARDOWN" => Ok(vec![Directive::CharDown(parse_char(rest)?)]),
        "CHARUP" => Ok(vec![Directive::CharUp(parse_char(rest)?)]),
        "TYPE" => parse_type(rest),
        "SLEEP" => rest
            .parse::<u64>()
            .map(|frames| vec![Directive::Sleep(frames)])
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

/// `CHARDOWN`/`CHARUP`'s argument: either a single literal character, or a
/// `0x`-prefixed Unicode code point (needed for whitespace or other
/// characters this directive line's own whitespace-splitting can't carry
/// literally). Checked against Latin-1 representability here, at parse
/// time, rather than only at [`NativeInput::push_char`] -- the same
/// "hostile input fails at parse time" posture [`parse_keycode`] takes.
fn parse_char(text: &str) -> Result<char, String> {
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        let code = u32::from_str_radix(hex, 16).map_err(|e| format!("char: {e}"))?;
        let ch = char::from_u32(code)
            .ok_or_else(|| format!("char: {code:#x} is not a valid Unicode scalar value"))?;
        return latin1_char_or_err(ch);
    }
    let mut chars = text.chars();
    let ch = chars
        .next()
        .ok_or("char: expected a character or 0x<codepoint>")?;
    if chars.next().is_some() {
        return Err(format!(
            "char: expected exactly one character, got {text:?} \
             (use 0x<codepoint> for anything containing whitespace)"
        ));
    }
    latin1_char_or_err(ch)
}

fn latin1_char_or_err(ch: char) -> Result<char, String> {
    if latin1_from_char(ch).is_some() {
        Ok(ch)
    } else {
        Err(format!(
            "char {ch:?} (U+{:04X}) has no Latin-1 representation",
            ch as u32
        ))
    }
}

/// `TYPE "<string>"`: expand a double-quoted string into a `CHARDOWN`/
/// `CHARUP` pair per character, eagerly at parse time -- module docs'
/// rationale for why this exists alongside `CHARDOWN`/`CHARUP` rather than
/// leaving typing text to a run of single-character directives.
fn parse_type(text: &str) -> Result<Vec<Directive>, String> {
    let s = parse_quoted(text)?;
    let mut out = Vec::with_capacity(s.chars().count() * 2);
    for ch in s.chars() {
        let ch = latin1_char_or_err(ch).map_err(|e| format!("TYPE: {e}"))?;
        out.push(Directive::CharDown(ch));
        out.push(Directive::CharUp(ch));
    }
    Ok(out)
}

/// A minimal double-quoted string literal: `\"` and `\\` are the only
/// escapes needed to write a literal quote or backslash, plus `\n`/`\t`
/// for the two whitespace characters most likely to be wanted; anything
/// else after a backslash, an unescaped `"` before the closing quote, or a
/// missing pair of quotes altogether, is a parse error rather than a
/// guess at what was meant.
fn parse_quoted(text: &str) -> Result<String, String> {
    if text.len() < 2 || !text.starts_with('"') || !text.ends_with('"') {
        return Err("TYPE: expected a double-quoted string, e.g. TYPE \"hello\"".to_string());
    }
    let inner = &text[1..text.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Err("TYPE: unescaped '\"' inside the string".to_string()),
            '\\' => match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                // Deliberately CR, not LF. A script author writes
                // `\n` to mean "press Return", and Return on this
                // platform is CR (0x0D) -- the Amiga console submits a
                // line on CR and treats LF as a bare cursor-down. Emitting
                // LF here looks like it works (the cursor moves to the
                // next line) while never submitting anything, which cost
                // an afternoon of chasing a "hang" that was really a
                // command that had simply never been entered.
                Some('n') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some(other) => return Err(format!("TYPE: unknown escape \\{other}")),
                None => return Err("TYPE: trailing backslash".to_string()),
            },
            _ => out.push(c),
        }
    }
    Ok(out)
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

    /// A `TYPE` longer than the queue must arrive whole.
    ///
    /// This is a regression test for a real truncation: driving AROS,
    /// `TYPE "System/Wanderer/Wanderer\n"` put exactly `System/W` on the
    /// guest's command line and the run reported success. `tick` pushed
    /// every directive in one frame and threw away each `push_*`'s
    /// `false`, so the queue's sixteenth event was the last one that
    /// existed -- eight characters, since `TYPE` expands to a down/up
    /// pair each.
    ///
    /// The assertion is deliberately on the *whole* delivered sequence
    /// rather than on a count: a length check would have passed just as
    /// happily on eight characters delivered twice, and what actually
    /// matters is that the guest sees each character once and in order.
    #[test]
    fn type_longer_than_the_queue_is_delivered_whole_and_in_order() {
        use machine_core::input::{ev, QUEUE_CAPACITY};

        const TEXT: &str = "System/Wanderer/Wanderer";
        assert!(
            TEXT.len() * 2 > QUEUE_CAPACITY,
            "test is only meaningful if the script overruns the queue"
        );

        let mut script = InputScript::parse(&format!("TYPE \"{TEXT}\"\n")).unwrap();
        let mut dev = NativeInput::new();

        // Each iteration is one frame: the script pushes what fits, then
        // the guest drains it -- which is exactly the interleaving the
        // old code assumed it could skip.
        let mut got = Vec::new();
        for frame in 0..1000 {
            script.tick(frame, &mut dev);
            loop {
                let (ty, code, ..) = peek(&dev);
                if ty == ev::NONE {
                    break;
                }
                got.push((ty, code));
                advance(&mut dev);
            }
            if script.is_done() {
                break;
            }
        }
        assert!(script.is_done(), "script never drained");

        let want: Vec<(u8, u8)> = TEXT
            .bytes()
            .flat_map(|b| [(ev::CHAR_DOWN, b), (ev::CHAR_UP, b)])
            .collect();
        assert_eq!(got, want);
    }

    /// `\n` must reach the guest as CR, the key that submits a line.
    ///
    /// LF renders identically in a console -- the cursor drops to the
    /// next line either way -- so getting this wrong produces a script
    /// that appears to type its commands and silently never runs any of
    /// them.
    #[test]
    fn type_newline_is_carriage_return_not_line_feed() {
        use machine_core::input::ev;

        let mut script = InputScript::parse("TYPE \"a\\n\"\n").unwrap();
        let mut dev = NativeInput::new();
        script.tick(0, &mut dev);

        let mut got = Vec::new();
        loop {
            let (ty, code, ..) = peek(&dev);
            if ty == ev::NONE {
                break;
            }
            got.push((ty, code));
            advance(&mut dev);
        }
        assert_eq!(
            got,
            vec![
                (ev::CHAR_DOWN, b'a'),
                (ev::CHAR_UP, b'a'),
                (ev::CHAR_DOWN, 0x0D),
                (ev::CHAR_UP, 0x0D),
            ]
        );
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
    fn chardown_and_charup_apply_immediately_and_move_on() {
        let mut script = InputScript::parse("CHARDOWN @\nCHARUP @\n").unwrap();
        let mut dev = NativeInput::new();
        script.tick(0, &mut dev);
        assert!(script.is_done());

        use machine_core::input::ev;
        let (ty, code, q, ..) = peek(&dev);
        assert_eq!(ty, ev::CHAR_DOWN);
        assert_eq!(code, b'@');
        assert_eq!(q, 0, "character events carry no qualifier");
        advance(&mut dev);
        let (ty, code, ..) = peek(&dev);
        assert_eq!(ty, ev::CHAR_UP);
        assert_eq!(code, b'@');
    }

    #[test]
    fn chardown_accepts_a_hex_codepoint_for_whitespace_and_other_chars() {
        let script = InputScript::parse("CHARDOWN 0x20\n").unwrap();
        assert_eq!(script.remaining.len(), 1);
        let mut dev = NativeInput::new();
        let mut script = script;
        script.tick(0, &mut dev);
        assert_eq!(dev.read(machine_core::input::reg::EVENT_CODE + 3), b' ');
    }

    #[test]
    fn type_expands_to_a_chardown_charup_pair_per_character() {
        let script = InputScript::parse("TYPE \"Hi!\"\n").unwrap();
        assert_eq!(script.remaining.len(), 6, "3 characters * (down + up) each");
        let mut dev = NativeInput::new();
        let mut script = script;
        script.tick(0, &mut dev);
        assert!(script.is_done());

        use machine_core::input::ev;
        for expected in *b"HHii!!" {
            let (ty, code, ..) = peek(&dev);
            assert!(ty == ev::CHAR_DOWN || ty == ev::CHAR_UP);
            assert_eq!(code, expected);
            advance(&mut dev);
        }
    }

    #[test]
    fn type_preserves_character_order_including_press_then_release_per_char() {
        let script = InputScript::parse("TYPE \"ab\"\n").unwrap();
        let mut dev = NativeInput::new();
        let mut script = script;
        script.tick(0, &mut dev);

        use machine_core::input::ev;
        let expect = [
            (ev::CHAR_DOWN, b'a'),
            (ev::CHAR_UP, b'a'),
            (ev::CHAR_DOWN, b'b'),
            (ev::CHAR_UP, b'b'),
        ];
        for (ty, code) in expect {
            let (got_ty, got_code, ..) = peek(&dev);
            assert_eq!(got_ty, ty);
            assert_eq!(got_code, code);
            advance(&mut dev);
        }
    }

    #[test]
    fn type_supports_escaped_quote_and_backslash() {
        let script = InputScript::parse("TYPE \"a\\\"b\\\\c\"\n").unwrap();
        // "a\"b\\c" -> a " b \ c -> 5 characters
        assert_eq!(script.remaining.len(), 10);
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

    #[test]
    fn a_character_with_no_latin1_representation_is_a_parse_error() {
        assert!(InputScript::parse("CHARDOWN 0x20AC").is_err(), "EURO SIGN");
        assert!(
            InputScript::parse("CHARDOWN 0x1F600").is_err(),
            "an emoji code point"
        );
        assert!(InputScript::parse("CHARDOWN 0x100").is_err());
        // The full legal range still works.
        assert!(InputScript::parse("CHARDOWN 0xFF").is_ok());
        assert!(InputScript::parse("CHARDOWN 0x00").is_ok());
    }

    #[test]
    fn a_multi_character_chardown_argument_is_a_parse_error() {
        assert!(InputScript::parse("CHARDOWN ab").is_err());
    }

    #[test]
    fn an_empty_chardown_argument_is_a_parse_error() {
        assert!(InputScript::parse("CHARDOWN").is_err());
        assert!(InputScript::parse("CHARDOWN \n").is_err());
    }

    #[test]
    fn an_invalid_hex_codepoint_for_chardown_is_a_parse_error() {
        assert!(InputScript::parse("CHARDOWN 0xZZ").is_err());
    }

    #[test]
    fn type_with_a_non_latin1_character_is_a_parse_error_citing_the_character() {
        let err = match InputScript::parse("TYPE \"caf\u{e9}\u{20ac}\"") {
            Ok(_) => panic!("expected a parse error for a non-Latin-1 character"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("TYPE"),
            "error should identify the directive: {err}"
        );
    }

    #[test]
    fn type_without_matching_quotes_is_a_parse_error() {
        assert!(InputScript::parse("TYPE hello").is_err());
        assert!(InputScript::parse("TYPE \"unterminated").is_err());
        assert!(
            InputScript::parse("TYPE \"\"").is_ok(),
            "an empty string types nothing"
        );
    }

    #[test]
    fn type_rejects_an_unescaped_quote_or_trailing_backslash() {
        assert!(InputScript::parse("TYPE \"a\"b\"").is_err());
        assert!(
            InputScript::parse("TYPE \"a\\\"").is_err(),
            "trailing backslash"
        );
    }

    #[test]
    fn type_rejects_an_unknown_escape_sequence() {
        assert!(InputScript::parse("TYPE \"\\q\"").is_err());
    }
}
