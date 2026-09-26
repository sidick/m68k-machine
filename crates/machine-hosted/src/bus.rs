//! Adapts [`machine_core::MachineBus`] to `m68k`'s [`AddressBus`] trait.
//!
//! Lives here for the same orphan-rule reason `tests/hello_guest.rs`
//! documents in `machine-core`: both `MachineBus` and `AddressBus` are
//! foreign to this crate, so neither side can carry a blanket `impl`.

use std::sync::atomic::{AtomicU32, Ordering};

use m68k::AddressBus;
use machine_core::MachineBus;

use crate::blitter_trace::BlitterTrace;

/// PC of the instruction currently executing, stored by `run_guest`'s
/// per-instruction hook so the serial register trace below can attribute
/// accesses. Diagnostic only.
pub static LAST_PC: AtomicU32 = AtomicU32::new(0);

const CUSTOM_BASE: u32 = 0x00DF_F000;
const CUSTOM_END: u32 = 0x00E0_0000;

/// Diagnostic trace of every serial-related custom register access
/// (SERDATR/SERDAT/SERPER, plus INTENA/INTREQ writes touching the RBF or
/// TBE bits), gated on the `SERIAL_REG_TRACE` env var -- read once at
/// [`Bus`] construction into [`Bus`]'s own `.2` field rather than through
/// a `OnceLock` re-checked on every single access
/// (`docs/bus-fast-path-plan.md` §3.5: this ran once per byte/word/long
/// access on every guest instruction that touched the bus, whether or
/// not the trace was ever enabled). `enabled` is that cached bool, so
/// this is `#[inline]`: the common case (tracing off) is one branch and
/// nothing else.
#[inline]
fn trace_serial(enabled: bool, kind: &str, address: u32, value: u16) {
    if !enabled || !(CUSTOM_BASE..CUSTOM_END).contains(&address) {
        return;
    }
    let offset = (address - CUSTOM_BASE) as u16 & 0x1FE;
    let name = match offset {
        0x018 => "SERDATR",
        0x030 => "SERDAT",
        0x032 => "SERPER",
        0x09A if value & 0x0801 != 0 => "INTENA",
        0x09C if value & 0x0801 != 0 => "INTREQ",
        _ => return,
    };
    let pc = LAST_PC.load(Ordering::Relaxed);
    eprintln!("SERTRACE {kind} {name} val={value:#06x} pc={pc:#010x}");
}

/// `.1` is `--blitter-trace`'s recorder, `None` on a plain run (the
/// default) -- every write path below pays exactly one `if let Some`
/// check in that case and nothing else (`blitter_trace.rs`'s module doc
/// comment on gating). `.2` is whether `SERIAL_REG_TRACE` was set at
/// construction time (`docs/bus-fast-path-plan.md` §3.5) -- `trace_serial`
/// takes it as its first, cheapest-to-check argument rather than looking
/// it up itself.
pub struct Bus<'a>(pub MachineBus<'a>, pub Option<BlitterTrace>, pub bool);

impl Bus<'_> {
    /// Whether `SERIAL_REG_TRACE` was set at construction (`.2`, read
    /// once in `run.rs` rather than on every call) -- exposed as a
    /// method so `run_guest`'s hook can skip storing
    /// [`LAST_PC`] on every retired instruction when nothing is tracing
    /// (`LAST_PC` exists purely to attribute [`trace_serial`]'s output,
    /// so a plain run with no trace enabled has no use for it at all).
    #[inline]
    pub fn serial_trace_on(&self) -> bool {
        self.2
    }
}

impl AddressBus for Bus<'_> {
    fn read_byte(&mut self, address: u32) -> u8 {
        let value = self.0.read_byte(address);
        trace_serial(self.2, "Rb", address, value as u16);
        value
    }

    fn read_word(&mut self, address: u32) -> u16 {
        let value = self.0.read_word(address);
        trace_serial(self.2, "R", address, value);
        value
    }

    fn read_long(&mut self, address: u32) -> u32 {
        self.0.read_long(address)
    }

    fn write_byte(&mut self, address: u32, value: u8) {
        trace_serial(self.2, "Wb", address, value as u16);
        self.0.write_byte(address, value);
    }

    fn write_word(&mut self, address: u32, value: u16) {
        if let Some(trace) = &mut self.1 {
            trace.observe_word(address, value);
        }
        trace_serial(self.2, "W", address, value);
        self.0.write_word(address, value);
    }

    fn write_long(&mut self, address: u32, value: u32) {
        // Observed as the two word writes `MachineBus::write_long` itself
        // decomposes a long write into -- graphics.library commonly pokes
        // register pairs like `BLTCON0`/`BLTCON1` with one `MOVE.L`, so a
        // recorder that only watched `write_word` would miss those.
        if let Some(trace) = &mut self.1 {
            trace.observe_word(address, (value >> 16) as u16);
            trace.observe_word(address.wrapping_add(2), value as u16);
        }
        trace_serial(self.2, "W", address, (value >> 16) as u16);
        trace_serial(self.2, "W", address.wrapping_add(2), value as u16);
        self.0.write_long(address, value);
    }
}
