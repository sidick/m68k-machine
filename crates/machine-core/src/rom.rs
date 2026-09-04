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

/// The first longword of a 256 KB Kickstart-format ROM (same shape as
/// [`KICK_MAGIC_512K`], smaller size marker). Also the marker an AROS
/// extended ROM's *second half* would carry under "kickety split"
/// (unused by this machine, which loads the ext ROM as one 512 KB image).
pub const KICK_MAGIC_256K: u32 = 0x1111_4EF9;

/// Where a main ROM is mapped.
pub const ROM_BASE: u32 = 0x00F8_0000;
/// Where an extended (AROS) ROM is mapped.
pub const EXT_ROM_BASE: u32 = 0x00E0_0000;
/// Size of the extended ROM window, `$E00000`-`$E7FFFF`.
pub const EXT_ROM_WINDOW_SIZE: usize = 0x0008_0000;

/// Offset of the `(rom_ver, rom_rev)` word pair in the header.
const ROM_REV_OFFSET: usize = 0x0C;
/// Offset of the `(exec_ver, exec_rev)` word pair in the header. Kept as
/// a fallback: it's the ROM's own claim about the exec.library it
/// carries, but see [`find_exec_library_version`] for the more reliable
/// source.
const EXEC_REV_OFFSET: usize = 0x10;
/// Offset of the `RESET` opcode that a bootable image places just before
/// its diagnostic area, per amitools' `check_magic_reset` (romtool is the
/// reference for this whole module; see `~/src/amitools/amitools/rom`).
const MAGIC_RESET_OFFSET: usize = 0xD0;
/// The `RESET` instruction's opcode.
const MAGIC_RESET_OPCODE: u16 = 0x4E70;

/// `RTC_MATCHWORD`: marks the start of an Exec `Resident` structure.
const RESIDENT_MATCHWORD: u16 = 0x4AFC;
/// Offset of `RT_NAME` (a pointer) within a `Resident` structure.
const RESIDENT_NAME_OFFSET: usize = 14;
/// Offset of `RT_IDSTRING` (a pointer) within a `Resident` structure.
const RESIDENT_IDSTRING_OFFSET: usize = 18;
/// Bytes of a `Resident` structure this module actually reads: through
/// the end of `RT_IDSTRING` (offset 18, 4 bytes). `RT_INIT` and beyond
/// are unused here.
const RESIDENT_READ_SIZE: usize = RESIDENT_IDSTRING_OFFSET + 4;

/// Longest run of an id string this module will scan for a `N.N` version
/// pattern, bounding the search in case a corrupt image lacks a NUL
/// terminator.
const ID_STRING_SCAN_CAP: usize = 96;

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
    /// exec.library version and revision. Read from the exec.library
    /// `Resident` structure's id string when one can be located (see
    /// [`find_exec_library_version`]); falls back to the header's own
    /// `(exec_ver, exec_rev)` field otherwise. The two can legitimately
    /// disagree — AROS's header advertises its ROM release (e.g.
    /// `46.12`) while its actual exec.library reports a much newer
    /// internal version (e.g. `51.8`); the resident-derived value is the
    /// one that matches what the running OS reports.
    pub exec_rev: (u16, u16),
    /// Image size in bytes.
    pub size: usize,
    /// Whether the ROM's own checksum validates.
    pub checksum_ok: bool,
    /// Reset PC taken from the image header. Meaningless when
    /// `bootable` is false.
    pub boot_pc: u32,
    /// Whether the image carries a valid reset vector (a `RESET` opcode
    /// at offset `$D0`, per [`MAGIC_RESET_OFFSET`]). False for the AROS
    /// extended ROM, which has a valid header and checksum but nothing
    /// at that address to jump to — it is a library archive to be mapped
    /// alongside a main ROM, not something the CPU resets into.
    pub bootable: bool,
}

/// Identify a ROM image, or return `None` if it has no usable header.
///
/// Deliberately tolerant: a failed checksum is reported in [`RomInfo`]
/// rather than rejected, because a user-supplied or hand-patched ROM
/// should still boot and say so, not be silently refused. Likewise an
/// image with no reset vector (the AROS ext ROM) is still identified,
/// just marked `bootable: false` rather than treated as broken.
///
/// Header layout: magic longword at offset 0 (also encodes the expected
/// image size), the `JMP`'s target (the boot PC) at offset `$04`, and
/// version/revision words at offset `$0C`. amitools' romtool
/// (`~/src/amitools/amitools/rom`) is the reference implementation and
/// agrees with this machine's ROM set.
pub fn identify(image: &[u8]) -> Option<RomInfo> {
    let magic = read_u32(image, 0)?;
    let expected_size = match magic {
        KICK_MAGIC_512K => 512 * 1024,
        KICK_MAGIC_256K => 256 * 1024,
        _ => return None,
    };
    // A real image's magic and length agree; anything else has no usable
    // header (romtool's `check_size` + `check_header`, taken together).
    if image.len() != expected_size {
        return None;
    }

    let rev = (
        read_u16(image, ROM_REV_OFFSET)?,
        read_u16(image, ROM_REV_OFFSET + 2)?,
    );
    let header_exec_rev = (
        read_u16(image, EXEC_REV_OFFSET)?,
        read_u16(image, EXEC_REV_OFFSET + 2)?,
    );
    let boot_pc = read_u32(image, 4)?;
    let bootable = read_u16(image, MAGIC_RESET_OFFSET) == Some(MAGIC_RESET_OPCODE);
    let checksum_ok = verify_checksum(image);
    let exec_rev = find_exec_library_version(image).unwrap_or(header_exec_rev);
    let kind = detect_kind(image);

    Some(RomInfo {
        kind,
        rev,
        exec_rev,
        size: image.len(),
        checksum_ok,
        boot_pc,
        bootable,
    })
}

/// Classify an image by scanning for identifying strings. Version range
/// alone doesn't work (AROS reports `46.x`, which overlaps real
/// Kickstart revisions), so this looks for text instead: AROS ROMs carry
/// "AROS" in their id strings (checked first, since some AROS builds
/// also mention "Kickstart" in compatibility shim text), while Kickstart
/// ROMs carry "Kickstart" (from strings like "Kickstart %ld.%ld" or the
/// early startup screen).
fn detect_kind(image: &[u8]) -> RomKind {
    if contains(image, b"AROS") {
        RomKind::Aros
    } else if contains(image, b"Kickstart") {
        RomKind::Kickstart
    } else {
        RomKind::Unknown
    }
}

/// Verify the ROM's internal checksum: the 32-bit sum of every longword
/// in the image, with end-around carry (the ones'-complement addition a
/// real Amiga's `InitResident` checksum loop uses), must total
/// `$FFFFFFFF`. The checksum longword is itself part of the image and
/// was chosen by the ROM's builder to make the total come out this way,
/// so no separate stored checksum field needs to be read back out.
fn verify_checksum(image: &[u8]) -> bool {
    let mut sum: u32 = 0;
    let (chunks, _remainder) = image.as_chunks::<4>();
    for chunk in chunks {
        let val = u32::from_be_bytes(*chunk);
        let (next, carry) = sum.overflowing_add(val);
        sum = if carry { next.wrapping_add(1) } else { next };
    }
    sum == 0xFFFF_FFFF
}

/// Locate the exec.library `Resident` structure and read its version
/// from the id string, e.g. `"exec 47.13 (1.1.2025)"` -> `(47, 13)`.
///
/// A `Resident` is found by its `RTC_MATCHWORD` (`$4AFC`); at that point
/// `RT_MATCHTAG` should point back at the structure itself, which gives
/// the base address the image is linked against without needing to know
/// where the caller intends to map it. `RT_NAME` is then checked against
/// `"exec.library"` to make sure this is the right resident (a ROM has
/// dozens), and `RT_IDSTRING` is parsed for the first `N.N` pattern.
/// Returns `None` if no such resident is found (e.g. the AROS extended
/// ROM, which carries no exec.library at all), leaving the caller to
/// fall back to the header's own claim.
fn find_exec_library_version(image: &[u8]) -> Option<(u16, u16)> {
    if image.len() < RESIDENT_READ_SIZE {
        return None;
    }
    let mut off = 0usize;
    // Resident structures are word-aligned by convention (they open with
    // 68k code/data), so stepping by 2 is exact, not a shortcut.
    while off + RESIDENT_READ_SIZE <= image.len() {
        if read_u16(image, off) == Some(RESIDENT_MATCHWORD) {
            if let Some(version) = try_parse_exec_resident(image, off) {
                return Some(version);
            }
        }
        off += 2;
    }
    None
}

fn try_parse_exec_resident(image: &[u8], off: usize) -> Option<(u16, u16)> {
    let tag_ptr = read_u32(image, off + 2)?;
    // RT_MATCHTAG == base_addr + off, so this recovers the base address
    // this particular resident (and its name/id strings) are linked
    // against, whatever address the ROM ends up mapped at.
    let base = tag_ptr.wrapping_sub(off as u32);

    let name_ptr = read_u32(image, off + RESIDENT_NAME_OFFSET)?;
    if name_ptr == 0 {
        return None;
    }
    let name_off = name_ptr.wrapping_sub(base) as usize;
    if !str_at_matches(image, name_off, b"exec.library") {
        return None;
    }

    let id_ptr = read_u32(image, off + RESIDENT_IDSTRING_OFFSET)?;
    if id_ptr == 0 {
        return None;
    }
    let id_off = id_ptr.wrapping_sub(base) as usize;
    if id_off >= image.len() {
        return None;
    }
    let cap = core::cmp::min(image.len() - id_off, ID_STRING_SCAN_CAP);
    let window = &image[id_off..id_off + cap];
    let end = window.iter().position(|&b| b == 0).unwrap_or(cap);
    parse_first_version(&window[..end])
}

/// Whether `image[off..]` starts with `expected` followed by a NUL byte
/// (an exact C-string match, not merely a prefix).
fn str_at_matches(image: &[u8], off: usize, expected: &[u8]) -> bool {
    let Some(end) = off.checked_add(expected.len()) else {
        return false;
    };
    if end >= image.len() {
        return false;
    }
    &image[off..end] == expected && image[end] == 0
}

/// Find the first `<digits>.<digits>` run in `s` and parse it as
/// `(major, minor)`. Used to pull a version out of free-form id-string
/// text such as `"exec 47.13 (1.1.2025)"` or
/// `"exec.library amiga-m68k 51.8 (30.8.2026)"` (the latter has a
/// digit-then-letter run, `"m68k"`, that must NOT be mistaken for a
/// match — it isn't followed by a `.` and digits, so it's skipped).
fn parse_first_version(s: &[u8]) -> Option<(u16, u16)> {
    let mut i = 0;
    while i < s.len() {
        if s[i].is_ascii_digit() {
            let start = i;
            while i < s.len() && s[i].is_ascii_digit() {
                i += 1;
            }
            if i < s.len() && s[i] == b'.' {
                let rstart = i + 1;
                let mut j = rstart;
                while j < s.len() && s[j].is_ascii_digit() {
                    j += 1;
                }
                if j > rstart {
                    if let (Some(major), Some(minor)) =
                        (parse_u16(&s[start..i]), parse_u16(&s[rstart..j]))
                    {
                        return Some((major, minor));
                    }
                }
            }
            // Not a match (or the minor half didn't parse): `i` already
            // sits just past the digit run, so the outer loop resumes
            // scanning from there rather than looping forever.
        } else {
            i += 1;
        }
    }
    None
}

fn parse_u16(digits: &[u8]) -> Option<u16> {
    if digits.is_empty() {
        return None;
    }
    let mut val: u16 = 0;
    for &b in digits {
        val = val.checked_mul(10)?.checked_add((b - b'0') as u16)?;
    }
    Some(val)
}

/// Whether `needle` occurs anywhere in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn read_u16(image: &[u8], off: usize) -> Option<u16> {
    let end = off.checked_add(2)?;
    image
        .get(off..end)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn read_u32(image: &[u8], off: usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    image
        .get(off..end)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
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
    use std::vec;
    use std::vec::Vec;

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

    // ---- synthetic-header unit tests -------------------------------

    /// Build a minimal, correctly-sized (256 KB) synthetic image with a
    /// valid header and a checksum that balances to `$FFFFFFFF`, so
    /// individual fields can be tampered with per test.
    fn synthetic_256k(rom_rev: (u16, u16), exec_rev: (u16, u16), boot_pc: u32) -> Vec<u8> {
        let mut image = vec![0u8; 256 * 1024];
        image[0..4].copy_from_slice(&KICK_MAGIC_256K.to_be_bytes());
        image[4..8].copy_from_slice(&boot_pc.to_be_bytes());
        image[ROM_REV_OFFSET..ROM_REV_OFFSET + 2].copy_from_slice(&rom_rev.0.to_be_bytes());
        image[ROM_REV_OFFSET + 2..ROM_REV_OFFSET + 4].copy_from_slice(&rom_rev.1.to_be_bytes());
        image[EXEC_REV_OFFSET..EXEC_REV_OFFSET + 2].copy_from_slice(&exec_rev.0.to_be_bytes());
        image[EXEC_REV_OFFSET + 2..EXEC_REV_OFFSET + 4].copy_from_slice(&exec_rev.1.to_be_bytes());
        image[MAGIC_RESET_OFFSET..MAGIC_RESET_OFFSET + 2]
            .copy_from_slice(&MAGIC_RESET_OPCODE.to_be_bytes());
        balance_checksum(&mut image);
        image
    }

    /// Patch the last longword so the image's checksum comes to
    /// `$FFFFFFFF` (mirrors what a ROM builder does when finalising an
    /// image, and what `write_check_sum` does in amitools).
    fn balance_checksum(image: &mut [u8]) {
        let len = image.len();
        image[len - 4..].copy_from_slice(&[0, 0, 0, 0]);
        let mut sum: u32 = 0;
        let (chunks, _remainder) = image.as_chunks::<4>();
        for chunk in chunks {
            let val = u32::from_be_bytes(*chunk);
            let (next, carry) = sum.overflowing_add(val);
            sum = if carry { next.wrapping_add(1) } else { next };
        }
        let needed = 0xFFFF_FFFFu32.wrapping_sub(sum);
        image[len - 4..].copy_from_slice(&needed.to_be_bytes());
    }

    #[test]
    fn rejects_unrecognised_magic() {
        let mut image = vec![0u8; 256 * 1024];
        image[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        assert!(identify(&image).is_none());
    }

    #[test]
    fn rejects_size_mismatched_with_its_own_magic() {
        // 512K magic but a 256K-sized buffer: header and length disagree.
        let mut image = vec![0u8; 256 * 1024];
        image[0..4].copy_from_slice(&KICK_MAGIC_512K.to_be_bytes());
        assert!(identify(&image).is_none());
    }

    #[test]
    fn parses_magic_size_version_and_boot_pc() {
        let image = synthetic_256k((47, 115), (47, 13), 0x00F8_00D2);
        let info = identify(&image).expect("valid synthetic header");
        assert_eq!(info.rev, (47, 115));
        assert_eq!(info.size, 256 * 1024);
        assert_eq!(info.boot_pc, 0x00F8_00D2);
        assert!(info.bootable);
        assert!(info.checksum_ok);
        // No id strings/residents in this fixture, so exec_rev falls
        // back to the header field.
        assert_eq!(info.exec_rev, (47, 13));
    }

    #[test]
    fn detects_512k_magic_and_size_too() {
        let mut image = vec![0u8; 512 * 1024];
        image[0..4].copy_from_slice(&KICK_MAGIC_512K.to_be_bytes());
        image[MAGIC_RESET_OFFSET..MAGIC_RESET_OFFSET + 2]
            .copy_from_slice(&MAGIC_RESET_OPCODE.to_be_bytes());
        balance_checksum(&mut image);
        let info = identify(&image).expect("valid 512K header");
        assert_eq!(info.size, 512 * 1024);
    }

    #[test]
    fn corrupted_checksum_is_reported_not_rejected() {
        let mut image = synthetic_256k((47, 115), (47, 13), 0x00F8_00D2);
        // Hand-patch a byte elsewhere in the image without rebalancing
        // the checksum, as e.g. a ROM patcher might.
        image[0x1000] ^= 0xFF;
        let info = identify(&image).expect("still a usable header");
        assert!(!info.checksum_ok, "tampered image must fail the checksum");
        // But identification itself is unaffected.
        assert_eq!(info.rev, (47, 115));
    }

    #[test]
    fn missing_reset_vector_is_not_bootable_but_still_identified() {
        let mut image = synthetic_256k((46, 11), (46, 11), 0x00F8_0002);
        // Clear the RESET opcode the ext ROM quirk hinges on.
        image[MAGIC_RESET_OFFSET..MAGIC_RESET_OFFSET + 2].copy_from_slice(&[0, 0]);
        balance_checksum(&mut image);
        let info = identify(&image).expect("header is otherwise valid");
        assert!(!info.bootable);
        assert_eq!(info.rev, (46, 11));
    }

    #[test]
    fn kind_detection_prefers_aros_over_kickstart_marker() {
        let mut image = synthetic_256k((46, 12), (46, 12), 0x00F8_00D8);
        let tail = image.len() - 64;
        image[tail..tail + 23].copy_from_slice(b"AROS ROM says Kickstart");
        balance_checksum(&mut image);
        assert_eq!(identify(&image).unwrap().kind, RomKind::Aros);
    }

    #[test]
    fn kind_detection_finds_kickstart_marker() {
        let mut image = synthetic_256k((47, 115), (47, 13), 0x00F8_00D2);
        let tail = image.len() - 64;
        image[tail..tail + 9].copy_from_slice(b"Kickstart");
        balance_checksum(&mut image);
        assert_eq!(identify(&image).unwrap().kind, RomKind::Kickstart);
    }

    #[test]
    fn kind_defaults_to_unknown_without_identifying_strings() {
        let image = synthetic_256k((99, 1), (99, 1), 0x00F8_0000);
        assert_eq!(identify(&image).unwrap().kind, RomKind::Unknown);
    }

    #[test]
    fn exec_library_resident_version_overrides_header_field() {
        // Build a minimal exec.library Resident by hand: matchword, a
        // self-referential tag pointer, name and id-string pointers.
        let mut image = synthetic_256k((46, 12), (46, 12), 0x00F8_00D8);
        let base = ROM_BASE;
        let resident_off = 0x100usize;
        let name_off = 0x200usize;
        let id_off = 0x210usize;

        image[resident_off..resident_off + 2].copy_from_slice(&RESIDENT_MATCHWORD.to_be_bytes());
        image[resident_off + 2..resident_off + 6]
            .copy_from_slice(&(base + resident_off as u32).to_be_bytes());
        image[resident_off + RESIDENT_NAME_OFFSET..resident_off + RESIDENT_NAME_OFFSET + 4]
            .copy_from_slice(&(base + name_off as u32).to_be_bytes());
        image[resident_off + RESIDENT_IDSTRING_OFFSET..resident_off + RESIDENT_IDSTRING_OFFSET + 4]
            .copy_from_slice(&(base + id_off as u32).to_be_bytes());

        image[name_off..name_off + 13].copy_from_slice(b"exec.library\0");
        let id_string = b"exec.library amiga-m68k 51.8 (30.8.2026)\0";
        image[id_off..id_off + id_string.len()].copy_from_slice(id_string);

        balance_checksum(&mut image);
        let info = identify(&image).expect("valid header");
        // Resident-derived version wins over the header's (46, 12).
        assert_eq!(info.exec_rev, (51, 8));
    }

    // ---- real ROM image tests --------------------------------------
    //
    // These read fixed absolute paths to non-redistributable ROM images
    // that only exist on this developer's machine (or similar external
    // checkouts). They must never fail CI on a machine without them, so
    // each one is guarded by `load_rom`, which logs and returns `None`
    // rather than panicking when the file is absent.

    fn load_rom(path: &str) -> Option<Vec<u8>> {
        match std::fs::read(path) {
            Ok(data) => Some(data),
            Err(err) => {
                eprintln!("skipping real-ROM test: {path}: {err}");
                None
            }
        }
    }

    #[test]
    fn real_a1200_kickstart_3_2_2() {
        let Some(image) =
            load_rom("/Users/simond/src/amirfb/nondistribution/roms/A1200.47.115.rom")
        else {
            return;
        };
        let info = identify(&image).expect("A1200 3.2.2 ROM should have a valid header");
        assert_eq!(info.kind, RomKind::Kickstart);
        assert_eq!(info.rev, (47, 115));
        assert_eq!(info.exec_rev, (47, 13));
        assert_eq!(info.size, 512 * 1024);
        assert_eq!(info.boot_pc, 0x00F8_00D2);
        assert!(info.bootable);
        assert!(info.checksum_ok);
    }

    #[test]
    fn real_aros_main_rom() {
        let Some(image) =
            load_rom("/Users/simond/src/external/Copperline/assets/aros/aros-amiga-m68k-rom.bin")
        else {
            return;
        };
        let info = identify(&image).expect("AROS main ROM should have a valid header");
        assert_eq!(info.kind, RomKind::Aros);
        assert_eq!(info.rev, (46, 12));
        assert_eq!(info.size, 512 * 1024);
        assert!(info.bootable);
        assert!(info.checksum_ok);
        // The header's own exec_rev claim (46, 12) undersells what's
        // actually inside; the resident-derived version is what the
        // running OS reports.
        assert_eq!(info.exec_rev, (51, 8));
    }

    #[test]
    fn real_aros_ext_rom() {
        let Some(image) =
            load_rom("/Users/simond/src/external/Copperline/assets/aros/aros-amiga-m68k-ext.bin")
        else {
            return;
        };
        let info = identify(&image).expect("AROS ext ROM should have a valid header");
        assert_eq!(info.kind, RomKind::Aros);
        assert_eq!(info.rev, (46, 11));
        assert_eq!(info.size, 512 * 1024);
        // The defining quirk: valid header, but no reset vector.
        assert!(!info.bootable);
        assert!(info.checksum_ok);
        // No exec.library resident in the ext ROM: falls back to the
        // header field.
        assert_eq!(info.exec_rev, (46, 11));
    }

    #[test]
    fn real_kickstart_3_1() {
        let Some(image) = load_rom("/Users/simond/src/external/Copperline/test-assets/KICK31.ROM")
        else {
            return;
        };
        let info = identify(&image).expect("Kickstart 3.1 should have a valid header");
        assert_eq!(info.kind, RomKind::Kickstart);
        assert_eq!(info.rev, (40, 63));
        assert_eq!(info.exec_rev, (40, 10));
        assert!(info.bootable);
        assert!(info.checksum_ok);
    }

    #[test]
    fn real_kickstart_3_2() {
        let Some(image) = load_rom("/Users/simond/src/external/Copperline/test-assets/KICK32.ROM")
        else {
            return;
        };
        let info = identify(&image).expect("Kickstart 3.2 should have a valid header");
        assert_eq!(info.kind, RomKind::Kickstart);
        assert_eq!(info.rev, (47, 95));
        assert_eq!(info.exec_rev, (47, 7));
        assert!(info.bootable);
        assert!(info.checksum_ok);
    }

    #[test]
    fn real_kickstart_47_7() {
        let Some(image) = load_rom("/Users/simond/src/amibake/assets/roms/kickstart-47.7.rom")
        else {
            return;
        };
        let info = identify(&image).expect("kickstart-47.7.rom should have a valid header");
        assert_eq!(info.kind, RomKind::Kickstart);
        assert_eq!(info.rev, (47, 96));
        assert_eq!(info.exec_rev, (47, 7));
        assert!(info.bootable);
        assert!(info.checksum_ok);
    }
}
