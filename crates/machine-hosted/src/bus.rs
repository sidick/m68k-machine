//! Adapts [`machine_core::MachineBus`] to `m68k`'s [`AddressBus`] trait.
//!
//! Lives here for the same orphan-rule reason `tests/hello_guest.rs`
//! documents in `machine-core`: both `MachineBus` and `AddressBus` are
//! foreign to this crate, so neither side can carry a blanket `impl`.

use m68k::AddressBus;
use machine_core::MachineBus;

use crate::blitter_trace::BlitterTrace;

/// `.1` is `--blitter-trace`'s recorder, `None` on a plain run (the
/// default) -- every write path below pays exactly one `if let Some`
/// check in that case and nothing else (`blitter_trace.rs`'s module doc
/// comment on gating).
pub struct Bus<'a>(pub MachineBus<'a>, pub Option<BlitterTrace>);

impl AddressBus for Bus<'_> {
    fn read_byte(&mut self, address: u32) -> u8 {
        self.0.read_byte(address)
    }

    fn read_word(&mut self, address: u32) -> u16 {
        self.0.read_word(address)
    }

    fn read_long(&mut self, address: u32) -> u32 {
        self.0.read_long(address)
    }

    fn write_byte(&mut self, address: u32, value: u8) {
        self.0.write_byte(address, value);
    }

    fn write_word(&mut self, address: u32, value: u16) {
        if let Some(trace) = &mut self.1 {
            trace.observe_word(address, value);
        }
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
        self.0.write_long(address, value);
    }
}
