//! Adapts [`machine_core::MachineBus`] to `m68k`'s [`AddressBus`] trait.
//!
//! Lives here for the same orphan-rule reason `tests/hello_guest.rs`
//! documents in `machine-core`: both `MachineBus` and `AddressBus` are
//! foreign to this crate, so neither side can carry a blanket `impl`.

use m68k::AddressBus;
use machine_core::MachineBus;

pub struct Bus<'a>(pub MachineBus<'a>);

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
        self.0.write_word(address, value);
    }

    fn write_long(&mut self, address: u32, value: u32) {
        self.0.write_long(address, value);
    }
}
