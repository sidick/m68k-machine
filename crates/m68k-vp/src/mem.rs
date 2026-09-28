//! Flat guest memory, borrowed from the caller.
//!
//! Mirrors `m68k::core::memory::LinearMemoryBus` exactly enough for the
//! two cores to be compared on identical ground (docs/cpu-core-poc-results.md):
//! a power-of-two-sized byte buffer, big-endian guest semantics, addresses
//! wrapped by a mask rather than checked -- no page-type table, no I/O, no
//! faults, because this workload (m68k/cpubench/kernels.s) has none. The
//! buffer is borrowed, never owned: `m68k-vp` is `#![no_std]` with no
//! allocator (crates/m68k-vp/src/lib.rs), so the caller supplies the
//! backing storage, exactly as `machine-core`'s devices borrow theirs
//! (CLAUDE.md's device-ledger convention, extended here to the memory
//! model per docs/cpu-core-proposal.md §4.6's "no allocator" no_std rule).

/// A flat, byte-addressable guest address space over a borrowed,
/// power-of-two-sized buffer.
pub struct VpMemory<'a> {
    buf: &'a mut [u8],
    mask: u32,
}

impl<'a> VpMemory<'a> {
    /// Wrap `buf` as guest memory. `buf.len()` must be a power of two and
    /// fit in a `u32` -- panics otherwise, matching this crate's posture
    /// that a malformed harness setup is a build bug, not a runtime
    /// condition to recover from (this is bench/test scaffolding, not a
    /// guest-facing device -- CLAUDE.md's "hostile input fails closed"
    /// rule governs devices, not this harness).
    pub fn new(buf: &'a mut [u8]) -> Self {
        assert!(
            buf.len().is_power_of_two(),
            "VpMemory requires a power-of-two backing buffer, got {} bytes",
            buf.len()
        );
        assert!(
            u32::try_from(buf.len()).is_ok(),
            "VpMemory backing buffer must fit in a 32-bit address space"
        );
        let mask = (buf.len() - 1) as u32;
        Self { buf, mask }
    }

    #[inline]
    fn idx(&self, addr: u32) -> usize {
        (addr & self.mask) as usize
    }

    /// Copy `data` into memory starting at `addr`, wrapping at the end of
    /// the backing buffer (mirrors `LinearMemoryBus::load`).
    pub fn load(&mut self, addr: u32, data: &[u8]) {
        for (offset, byte) in data.iter().copied().enumerate() {
            let i = self.idx(addr.wrapping_add(offset as u32));
            self.buf[i] = byte;
        }
    }

    #[inline]
    pub fn read_u8(&self, addr: u32) -> u8 {
        self.buf[self.idx(addr)]
    }

    #[inline]
    pub fn write_u8(&mut self, addr: u32, value: u8) {
        let i = self.idx(addr);
        self.buf[i] = value;
    }

    #[inline]
    pub fn read_u16(&self, addr: u32) -> u16 {
        let hi = self.read_u8(addr);
        let lo = self.read_u8(addr.wrapping_add(1));
        u16::from_be_bytes([hi, lo])
    }

    #[inline]
    pub fn write_u16(&mut self, addr: u32, value: u16) {
        let bytes = value.to_be_bytes();
        self.write_u8(addr, bytes[0]);
        self.write_u8(addr.wrapping_add(1), bytes[1]);
    }

    #[inline]
    pub fn read_u32(&self, addr: u32) -> u32 {
        let hi = self.read_u16(addr);
        let lo = self.read_u16(addr.wrapping_add(2));
        (u32::from(hi) << 16) | u32::from(lo)
    }

    #[inline]
    pub fn write_u32(&mut self, addr: u32, value: u32) {
        self.write_u16(addr, (value >> 16) as u16);
        self.write_u16(addr.wrapping_add(2), value as u16);
    }

    /// Borrow the complete backing buffer, for the differential
    /// byte-for-byte comparison against `LinearMemoryBus::as_slice`
    /// (docs/cpu-core-poc-results.md's correctness gate 2).
    pub fn as_slice(&self) -> &[u8] {
        self.buf
    }
}
