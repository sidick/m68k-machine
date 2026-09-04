//! ROM images: identification and the two supported layouts.
//!
//! The machine accepts either a single 512 KB Kickstart at `$F80000`
//! (the A1200 3.2 ROM, proposal §11.1) or an AROS 68k pair — main ROM at
//! `$F80000` plus an extended ROM at `$E00000` (§11.2). Identification is
//! advisory: it drives logging and the "which OS am I booting" question,
//! and never gates execution, so an unrecognised image still boots.

/// The first longword of a 512 KB Kickstart-format ROM: a size marker
/// (`$1114`) followed by the opcode of the `JMP` that begins the image.
pub const KICK_MAGIC_512K: u32 = 0x1114_4EF9;

/// Where a main ROM is mapped.
pub const ROM_BASE: u32 = 0x00F8_0000;
/// Where an extended (AROS) ROM is mapped.
pub const EXT_ROM_BASE: u32 = 0x00E0_0000;
/// Size of the extended ROM window, `$E00000`-`$E7FFFF`.
pub const EXT_ROM_WINDOW_SIZE: usize = 0x0008_0000;

/// What kind of image a ROM appears to be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RomKind {
    /// A Commodore/Hyperion Kickstart.
    Kickstart,
    /// An AROS 68k ROM (identified by its version range and strings).
    Aros,
    /// Header parsed, provenance unclear. Still bootable.
    Unknown,
}

/// What identification could determine about an image.
#[derive(Clone, Copy, Debug)]
pub struct RomInfo {
    pub kind: RomKind,
    /// ROM version and revision, e.g. `(47, 115)` for Kickstart 3.2.2.
    pub rev: (u16, u16),
    /// exec.library version and revision.
    pub exec_rev: (u16, u16),
    /// Image size in bytes.
    pub size: usize,
    /// Whether the ROM's own checksum validates.
    pub checksum_ok: bool,
    /// Reset PC taken from the image header.
    pub boot_pc: u32,
}

/// Identify a ROM image, or return `None` if it has no usable header.
///
/// Deliberately tolerant: a failed checksum is reported in [`RomInfo`]
/// rather than rejected, because a user-supplied or hand-patched ROM
/// should still boot and say so, not be silently refused.
pub fn identify(_image: &[u8]) -> Option<RomInfo> {
    // Implemented in Phase 1. Header layout: magic longword at offset 0,
    // version/revision words at offset $0C, and a footer carrying the
    // size and a 32-bit checksum over the image (the sum of all
    // longwords must come to $FFFFFFFF). amitools' romtool
    // (~/src/amitools/amitools/rom) is the reference implementation and
    // agrees with this machine's ROM set.
    None
}

/// Read a byte from a ROM image mapped at `base`, mirroring the image
/// across its window if it is shorter than the window.
///
/// Mirroring matches how a real address decoder ignores the unused high
/// address lines of an undersized ROM, and keeps small test fixtures
/// usable in place of a full 512 KB image.
pub fn read_mirrored(image: &[u8], base: u32, address: u32) -> u8 {
    if image.is_empty() {
        return crate::OPEN_BUS_BYTE;
    }
    image[((address - base) as usize) % image.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirroring_wraps_short_images() {
        let image = [0xAAu8, 0xBB];
        assert_eq!(read_mirrored(&image, ROM_BASE, ROM_BASE), 0xAA);
        assert_eq!(read_mirrored(&image, ROM_BASE, ROM_BASE + 1), 0xBB);
        assert_eq!(read_mirrored(&image, ROM_BASE, ROM_BASE + 2), 0xAA);
    }

    #[test]
    fn empty_image_reads_open_bus() {
        assert_eq!(read_mirrored(&[], ROM_BASE, ROM_BASE), crate::OPEN_BUS_BYTE);
    }
}
