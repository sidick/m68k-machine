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

/// Why an identified image is not directly usable as this machine's
/// Kickstart. Distinct from `identify()` returning `None`: that means no
/// header could be parsed at all, whereas every variant here comes with
/// a full [`RomInfo`] — the image *was* read, just not one this machine
/// can run as given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unsupported {
    /// Version 34 and below: Kickstart 1.x. This machine hangs storage,
    /// network and display entirely off Zorro III AUTOCONFIG cards
    /// (proposal §9), and Z3 AUTOCONFIG was never present in any
    /// 68000-class Kickstart — expansion.library there configures Zorro
    /// II only (proposal §11.1). No 1.x ROM can reach a usable state on
    /// this machine, so this check is sound in the direction it's used.
    ///
    /// It is *not* sufficient in the other direction: version alone does
    /// not prove Z3 support for anything above this gate. A 3.1 A500 ROM
    /// (V40) is 68000-class and Zorro-II-only, while the A1200 3.1 ROM
    /// (also V40) does Z3 — same version, different machine variant.
    /// Passing this check only means "not provably 1.x-class"; it is not
    /// a positive identification of Z3 capability.
    TooOldForZorroIII,
    /// Valid header and checksum, but no reset vector (`bootable` is
    /// false) — a library/extension ROM meant to be mapped alongside a
    /// main ROM, not booted itself. The AROS extended ROM is exactly
    /// this: expected and benign when loaded with `--ext-rom`, not an
    /// error.
    ExtensionRom,
}

impl Unsupported {
    /// A short, human-readable explanation suitable for a runner to
    /// print directly. `&'static str`, not `String`: this crate has no
    /// `alloc`.
    pub fn reason(self) -> &'static str {
        match self {
            Unsupported::TooOldForZorroIII => {
                "this ROM is Kickstart 1.x (V34 or below). This machine's storage, \
                 network and display are all Zorro III AUTOCONFIG cards, and Zorro \
                 III AUTOCONFIG did not exist in any 68000-class Kickstart, so this \
                 ROM cannot reach a usable state here (see proposal §11.1). Use the \
                 A1200 3.2 ROM instead."
            }
            Unsupported::ExtensionRom => {
                "this ROM has no reset vector, so it is an extension/library ROM \
                 (like the AROS ext ROM), not something the CPU can boot on its \
                 own. Load it with --ext-rom alongside a bootable main ROM instead \
                 of on its own."
            }
        }
    }
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
    /// Image size in bytes, i.e. `image.len()` of the buffer passed to
    /// [`identify`] (the doubled length for a doubled image — see
    /// [`RomInfo::doubled`] — not the logical 256 KB/512 KB the header
    /// declares).
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
    /// True when the buffer was longer than one logical image and every
    /// copy inside it was byte-identical (see [`identify`]'s doubled-image
    /// handling). This is an ordinary distribution form for 256 KB
    /// Kickstarts stored in a 512 KB EEPROM footprint, not corruption —
    /// `rev`/`exec_rev`/etc. above come from the first copy.
    pub doubled: bool,
    /// Whether this image is directly usable as this machine's
    /// Kickstart. `Err` carries the reason; see [`Unsupported`].
    pub supported: Result<(), Unsupported>,
}

/// Identify a ROM image, or return `None` if it has no usable header —
/// not even once byte-swap correction (see [`is_byte_swapped`]) is taken
/// into account by the caller.
///
/// Deliberately tolerant: a failed checksum is reported in [`RomInfo`]
/// rather than rejected, because a user-supplied or hand-patched ROM
/// should still boot and say so, not be silently refused. Likewise an
/// image with no reset vector (the AROS ext ROM) is still identified,
/// just marked `bootable: false` / `supported: Err(ExtensionRom)` rather
/// than treated as broken.
///
/// Header layout: magic longword at offset 0 (also encodes the expected
/// image size), the `JMP`'s target (the boot PC) at offset `$04`, and
/// version/revision words at offset `$0C`. amitools' romtool
/// (`~/src/amitools/amitools/rom`) is the reference implementation and
/// agrees with this machine's ROM set.
///
/// Two layouts beyond a single correctly-sized image are accepted:
///
/// - **Doubled**: the buffer is an exact multiple of the size the magic
///   declares, and every copy inside it is byte-identical. This is how
///   256 KB Kickstarts are ordinarily distributed for a 512 KB EEPROM
///   footprint (the image mirrored twice), not corruption — see
///   [`RomInfo::doubled`]. A multiple whose copies are *not* identical is
///   rejected (`None`): that is genuine corruption, not this layout.
/// - **Byte-swapped**: see [`is_byte_swapped`] and [`unswap_in_place`].
///   `identify()` does not correct this itself (it has no buffer to write
///   a correction into — `image` is `&[u8]`, and this crate has no
///   `alloc` to copy into); a swapped image's header is read via
///   word-swapped accessors just enough to report `rev`/`boot_pc`/
///   `checksum_ok` for diagnostics, with `kind` left [`RomKind::Unknown`]
///   (the id-string scan that would tell Kickstart from AROS apart is not
///   worth reimplementing byte-swap-aware for a value this transient —
///   the caller is expected to correct and re-identify, at which point
///   the normal scan runs). `supported` is always `Ok(())` for a
///   byte-swapped image: swapping is lossless and the caller is expected
///   to apply it (`unswap_in_place`) and call `identify` again to get the
///   authoritative, fully-parsed `RomInfo` for the corrected bytes.
pub fn identify(image: &[u8]) -> Option<RomInfo> {
    let magic = read_u32(image, 0)?;

    if let Some(expected_size) = magic_size(magic) {
        if image.len() == expected_size {
            return Some(build_info(image, image, false));
        }
        // Size disagrees with the magic's own declaration: accept it only
        // if every logical copy inside the buffer is identical (romtool's
        // `check_size` would reject this outright; a doubled distribution
        // image is the one legitimate reason real files look like this).
        if let Some(first_copy) = doubled_copy(image, expected_size) {
            return Some(build_info(image, first_copy, true));
        }
        return None;
    }

    // Header didn't parse directly. Check whether it reads correctly once
    // each 16-bit word's bytes are swapped back — a byte-swapped dump,
    // not a different or corrupt ROM (see `is_byte_swapped`).
    let swapped_magic = swap_header_u32(magic);
    if let Some(expected_size) = magic_size(swapped_magic) {
        if image.len() == expected_size {
            return Some(build_swapped_info(image));
        }
    }
    None
}

/// Whether `image` reads as a valid Kickstart-format header once every
/// 16-bit word's two bytes are swapped back — i.e. whether it is a
/// byte-swapped dump of a real ROM rather than an unrelated or corrupt
/// file. Cheap and side-effect-free: only the first longword is
/// examined, so a caller can check this before deciding whether
/// [`unswap_in_place`] is worth doing.
///
/// A file already reading directly is not reported as swapped even if,
/// coincidentally, its swapped form also happened to look like a header.
pub fn is_byte_swapped(image: &[u8]) -> bool {
    let Some(magic) = read_u32(image, 0) else {
        return false;
    };
    if magic_size(magic).is_some() {
        return false;
    }
    match magic_size(swap_header_u32(magic)) {
        Some(expected_size) => image.len() == expected_size,
        None => false,
    }
}

/// Correct a byte-swapped ROM dump in place: every 16-bit word's two
/// bytes are swapped back to their original order (word positions
/// unchanged). Some dumping paths transpose the two bytes of each 16-bit
/// ROM word in transit; the result is a word-order artefact of *how the
/// image was read*, not a different ROM, so this correction is lossless
/// — applying it twice restores the original bytes exactly (it is its
/// own inverse).
///
/// Typical use: `is_byte_swapped(&buf)` (or a first `identify(&buf)`
/// returning `None`) signals the need, `unswap_in_place(&mut buf)`
/// applies it, then `identify(&buf)` again produces the authoritative,
/// fully-parsed [`RomInfo`] — with the normal id-string `kind` scan,
/// resident-derived `exec_rev`, etc., none of which the swapped-header
/// fast path in [`identify`] attempts.
///
/// Operates in place because this crate has no allocator: there is no
/// way to hand back a corrected copy, only to mutate the caller's own
/// buffer.
pub fn unswap_in_place(image: &mut [u8]) {
    let (chunks, _remainder) = image.as_chunks_mut::<2>();
    for chunk in chunks {
        chunk.swap(0, 1);
    }
}

/// Map a header magic to the image size it declares, or `None` if it
/// isn't one of the two magics this machine recognises.
fn magic_size(magic: u32) -> Option<usize> {
    match magic {
        KICK_MAGIC_512K => Some(512 * 1024),
        KICK_MAGIC_256K => Some(256 * 1024),
        _ => None,
    }
}

/// If `image` is an exact multiple of `expected_size` and every
/// `expected_size`-sized chunk within it is byte-identical, return the
/// first chunk (the one logical copy to parse a header from). Returns
/// `None` when the lengths don't divide evenly or the copies disagree —
/// both cases are left to the caller to treat as unparseable/corrupt,
/// not silently patched over.
fn doubled_copy(image: &[u8], expected_size: usize) -> Option<&[u8]> {
    if expected_size == 0 || !image.len().is_multiple_of(expected_size) {
        return None;
    }
    let copies = image.len() / expected_size;
    if copies < 2 {
        return None;
    }
    let first = &image[..expected_size];
    let all_match = image
        .chunks_exact(expected_size)
        .all(|chunk| chunk == first);
    if all_match {
        Some(first)
    } else {
        None
    }
}

/// Swap the bytes of each 16-bit half of `v` independently, leaving the
/// half-word order unchanged. This is the transform a byte-swapped dump
/// applies to every aligned `u32` header field: unlike [`u32::swap_bytes`]
/// (which would also reverse the two halves against each other), a real
/// byte-swapped ROM only has each *word* transposed by the swapped-endian
/// read path that produced the dump, not the whole longword.
fn swap_header_u32(v: u32) -> u32 {
    let hi = ((v >> 16) as u16).swap_bytes() as u32;
    let lo = (v as u16).swap_bytes() as u32;
    (hi << 16) | lo
}

/// Read a `u16` header field from a byte-swapped image, undoing the swap
/// for that one word. Valid at any even offset, which every header field
/// this module reads happens to be.
fn read_u16_swapped(image: &[u8], off: usize) -> Option<u16> {
    read_u16(image, off).map(u16::swap_bytes)
}

/// Read a `u32` header field from a byte-swapped image (see
/// [`read_u16_swapped`]; a longword is just its two halves swapped
/// independently, per [`swap_header_u32`]).
fn read_u32_swapped(image: &[u8], off: usize) -> Option<u32> {
    read_u32(image, off).map(swap_header_u32)
}

/// Classify support for a fully-parsed (non-byte-swapped) image: the V34
/// Zorro III gate first (see [`Unsupported::TooOldForZorroIII`]), then
/// bootability. Order matters only for which reason wins when both could
/// apply; in practice no 1.x ROM in this machine's set also lacks a reset
/// vector, so it hasn't come up.
fn classify_support(rev: (u16, u16), bootable: bool) -> Result<(), Unsupported> {
    if rev.0 <= 34 {
        Err(Unsupported::TooOldForZorroIII)
    } else if !bootable {
        Err(Unsupported::ExtensionRom)
    } else {
        Ok(())
    }
}

/// Build a [`RomInfo`] for a directly-parsed or doubled image.
/// `header_region` is the one logical copy to read fields from (equal to
/// `full_image` unless `doubled`); `full_image` is what `size` reports.
fn build_info(full_image: &[u8], header_region: &[u8], doubled: bool) -> RomInfo {
    let rev = (
        read_u16(header_region, ROM_REV_OFFSET).unwrap_or(0),
        read_u16(header_region, ROM_REV_OFFSET + 2).unwrap_or(0),
    );
    let header_exec_rev = (
        read_u16(header_region, EXEC_REV_OFFSET).unwrap_or(0),
        read_u16(header_region, EXEC_REV_OFFSET + 2).unwrap_or(0),
    );
    let boot_pc = read_u32(header_region, 4).unwrap_or(0);
    let bootable = read_u16(header_region, MAGIC_RESET_OFFSET) == Some(MAGIC_RESET_OPCODE);
    let checksum_ok = verify_checksum(header_region);
    let exec_rev = find_exec_library_version(header_region).unwrap_or(header_exec_rev);
    let kind = detect_kind(header_region);
    let supported = classify_support(rev, bootable);

    RomInfo {
        kind,
        rev,
        exec_rev,
        size: full_image.len(),
        checksum_ok,
        boot_pc,
        bootable,
        doubled,
        supported,
    }
}

/// Build a [`RomInfo`] for an image only readable via byte-swapped
/// accessors. See [`identify`]'s doc comment for what is and isn't
/// attempted here (no id-string scan, no resident-derived exec_rev) and
/// why `supported` is always `Ok(())`: correction is the caller's job,
/// not a reason to refuse the image.
fn build_swapped_info(image: &[u8]) -> RomInfo {
    let rev = (
        read_u16_swapped(image, ROM_REV_OFFSET).unwrap_or(0),
        read_u16_swapped(image, ROM_REV_OFFSET + 2).unwrap_or(0),
    );
    let exec_rev = (
        read_u16_swapped(image, EXEC_REV_OFFSET).unwrap_or(0),
        read_u16_swapped(image, EXEC_REV_OFFSET + 2).unwrap_or(0),
    );
    let boot_pc = read_u32_swapped(image, 4).unwrap_or(0);
    let bootable = read_u16_swapped(image, MAGIC_RESET_OFFSET) == Some(MAGIC_RESET_OPCODE);
    let checksum_ok = verify_checksum_swapped(image);

    RomInfo {
        kind: RomKind::Unknown,
        rev,
        exec_rev,
        size: image.len(),
        checksum_ok,
        boot_pc,
        bootable,
        doubled: false,
        supported: Ok(()),
    }
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

/// As [`verify_checksum`], but for an image only readable via
/// byte-swapped accessors: each aligned longword is reconstructed with
/// [`read_u32_swapped`] before folding it into the running sum. Because
/// the checksum is a plain sum (order-independent) over the same set of
/// 4-byte-aligned positions either way, this yields exactly the checksum
/// the real (corrected) image would report — it's how
/// [`build_swapped_info`] can give an honest `checksum_ok` for
/// diagnostics without materialising a corrected copy.
fn verify_checksum_swapped(image: &[u8]) -> bool {
    let mut sum: u32 = 0;
    let mut off = 0usize;
    while off + 4 <= image.len() {
        let Some(val) = read_u32_swapped(image, off) else {
            return false;
        };
        let (next, carry) = sum.overflowing_add(val);
        sum = if carry { next.wrapping_add(1) } else { next };
        off += 4;
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
    use std::string::String;
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

    #[test]
    fn version_34_and_below_is_too_old_for_zorro_iii() {
        // Kickstart 1.3, 34.5 — the exact real-world case (KICK13.ROM /
        // kickstart-34.5.rom), synthesised so the gate is tested even
        // without those files present.
        let image = synthetic_256k((34, 5), (34, 2), 0x00FC_00D2);
        let info = identify(&image).expect("valid header");
        assert_eq!(
            info.supported,
            Err(Unsupported::TooOldForZorroIII),
            "V34 must be gated as too old for Zorro III"
        );
    }

    #[test]
    fn version_above_34_is_not_gated_by_age_alone() {
        let image = synthetic_256k((40, 63), (40, 10), 0x00F8_00D2);
        let info = identify(&image).expect("valid header");
        assert_eq!(info.supported, Ok(()));
    }

    #[test]
    fn extension_rom_is_unsupported_but_distinct_from_too_old() {
        let mut image = synthetic_256k((46, 11), (46, 11), 0x00F8_0002);
        image[MAGIC_RESET_OFFSET..MAGIC_RESET_OFFSET + 2].copy_from_slice(&[0, 0]);
        balance_checksum(&mut image);
        let info = identify(&image).expect("header is otherwise valid");
        assert_eq!(info.supported, Err(Unsupported::ExtensionRom));
    }

    #[test]
    fn unsupported_reasons_carry_static_text() {
        assert!(!Unsupported::TooOldForZorroIII.reason().is_empty());
        assert!(!Unsupported::ExtensionRom.reason().is_empty());
    }

    // ---- doubled-image tests ----------------------------------------

    #[test]
    fn doubled_image_is_accepted_and_identified_from_first_copy() {
        let single = synthetic_256k((47, 115), (47, 13), 0x00F8_00D2);
        let mut doubled = single.clone();
        doubled.extend_from_slice(&single);

        let info = identify(&doubled).expect("doubled image should still identify");
        assert!(info.doubled);
        assert_eq!(info.rev, (47, 115));
        assert_eq!(info.exec_rev, (47, 13));
        assert!(info.checksum_ok);
        // `size` reports the buffer actually passed in, not the logical
        // 256 KB the header declares.
        assert_eq!(info.size, 512 * 1024);
    }

    #[test]
    fn non_doubled_image_is_not_marked_doubled() {
        let image = synthetic_256k((47, 115), (47, 13), 0x00F8_00D2);
        let info = identify(&image).expect("valid header");
        assert!(!info.doubled);
    }

    #[test]
    fn mismatched_halves_are_rejected_as_corruption() {
        let single = synthetic_256k((47, 115), (47, 13), 0x00F8_00D2);
        let mut mismatched = single.clone();
        mismatched.extend_from_slice(&single);
        // Corrupt one byte in the second half only: the two copies now
        // disagree, which must NOT be accepted as a doubled image.
        let half = mismatched.len() / 2;
        mismatched[half + 0x1000] ^= 0xFF;
        assert!(
            identify(&mismatched).is_none(),
            "mismatched halves must be rejected, not silently accepted as doubled"
        );
    }

    // ---- byte-swap tests ---------------------------------------------

    #[test]
    fn is_byte_swapped_detects_a_swapped_header() {
        let mut image = synthetic_256k((47, 115), (47, 13), 0x00F8_00D2);
        assert!(!is_byte_swapped(&image));
        unswap_in_place(&mut image);
        assert!(is_byte_swapped(&image));
    }

    #[test]
    fn unswap_in_place_is_its_own_inverse() {
        let original = synthetic_256k((47, 115), (47, 13), 0x00F8_00D2);
        let mut roundtrip = original.clone();
        unswap_in_place(&mut roundtrip);
        assert_ne!(roundtrip, original, "swapping once must change the bytes");
        unswap_in_place(&mut roundtrip);
        assert_eq!(
            roundtrip, original,
            "swapping twice must restore the original bytes exactly"
        );
    }

    #[test]
    fn swapped_image_identifies_supported_and_unswaps_to_original_identification() {
        let original = synthetic_256k((47, 115), (47, 13), 0x00F8_00D2);
        let mut swapped = original.clone();
        unswap_in_place(&mut swapped);

        // identify() alone (no correction yet) can still read the header
        // via the swapped fast path, and never refuses it.
        let swapped_info = identify(&swapped).expect("swapped header still identifies");
        assert_eq!(swapped_info.rev, (47, 115));
        assert_eq!(swapped_info.supported, Ok(()));

        // The intended sequence: detect, correct in place, re-identify.
        assert!(is_byte_swapped(&swapped));
        unswap_in_place(&mut swapped);
        assert_eq!(
            swapped, original,
            "unswapping must byte-for-byte match the source"
        );
        let corrected_info = identify(&swapped).expect("corrected image still identifies");
        assert_eq!(corrected_info.rev, identify(&original).unwrap().rev);
        assert_eq!(
            corrected_info.exec_rev,
            identify(&original).unwrap().exec_rev
        );
        assert_eq!(
            corrected_info.checksum_ok,
            identify(&original).unwrap().checksum_ok
        );
    }

    // ---- real ROM image tests --------------------------------------
    //
    // These exist to check `identify()` against real ROM images, not
    // just the synthetic fixtures built above -- a parser that only
    // ever agrees with its own hand-built test data hasn't proven it
    // agrees with anything a real Amiga ever shipped.
    //
    // This module used to read ten absolute paths into three other
    // checkouts (`~/src/external/Copperline`, `~/src/amirfb`,
    // `~/src/amibake`) and assert an exact revision against whatever it
    // found there. Those checkouts move independently of this project:
    // Copperline's own vendored AROS pair was rebuilt at some point
    // after these assertions were written, and its exec.library
    // advanced from 51.8 to 51.9 -- a change to a fixture we do not
    // control, breaking a test that had nothing to do with it. Every
    // test here now resolves its fixture from a location this project
    // actually owns or controls the update cadence of:
    //
    // - The AROS 68k ROM pair is freely redistributable and vendored in
    //   this repository at `assets/aros/` (see
    //   `assets/aros/PROVENANCE.md`), which is why its tests below run
    //   unconditionally (no file to be missing) and assert exact
    //   revisions: we own this file, so it can only change when we
    //   deliberately refresh it (`scripts/fetch-aros-rom.sh`), in the
    //   same commit as the assertions below.
    // - Every Kickstart image is a vendor's licensed property and
    //   cannot live in this repository. These resolve from
    //   `nondistribution/` (see `nondistribution/README.md`),
    //   overridable per-fixture by an environment variable, and skip
    //   cleanly -- print, don't fail -- when the file is absent, so a
    //   clean clone with no licensed media still goes green. (None of
    //   the specific filenames below are populated in
    //   `nondistribution/` yet; a developer who wants this coverage
    //   drops the file in, or points the environment variable at an
    //   existing copy, and adds a line to that directory's README.)
    //
    // A second, subtler bug lived alongside the rotted paths: a test
    // named for a *release* ("Kickstart 3.1") asserted the exact
    // revision of *one machine's* ROM. Kickstart 3.1 genuinely shipped
    // as both 40.63 (e.g. the A4000) and 40.68 (e.g. the A1200)
    // depending on the machine; neither is more "3.1" than the other.
    // `real_kickstart_3_1`/`real_kickstart_3_2` below now assert only
    // what the name actually implies -- kind, major version,
    // bootability, checksum validity -- and say so in their doc
    // comments. Tests whose name and fixture already pin one specific,
    // individually-built image (`kickstart-47.7.rom`,
    // `kickstart-34.5.rom`, `kickstart-46.143.rom` -- each an
    // intentionally reproducible `tools/amibake` output, not "a 3.x
    // ROM" in the abstract) keep their exact assertions: there the test
    // is that this named file is what it claims to be, which is a
    // legitimate thing to pin.
    //
    // One outright duplicate was found and deleted rather than fixed:
    // `real_kickstart_1_3_doubled_is_too_old_for_zorro_iii` (via
    // `~/src/external/Copperline/test-assets/KICK13.ROM`) and
    // `real_kickstart_34_5_doubled_is_too_old_for_zorro_iii` (via
    // `~/src/amibake/assets/roms/kickstart-34.5.rom`) asserted the
    // identical thing -- same 34.5 image, same doubled layout, same
    // `TooOldForZorroIII` rejection -- against what the old comments
    // already admitted was "the same 34.5 image under a different
    // filename/source". One of the two now stands as
    // `real_kickstart_34_5_doubled_is_too_old_for_zorro_iii` below;
    // nothing is lost by dropping its twin.

    fn load_rom(path: &str) -> Option<Vec<u8>> {
        match std::fs::read(path) {
            Ok(data) => Some(data),
            Err(err) => {
                eprintln!("skipping real-ROM test: {path}: {err}");
                None
            }
        }
    }

    /// Resolve a licensed-ROM fixture this repo cannot carry: an
    /// environment variable if the caller sets one, otherwise
    /// `nondistribution/` at the repo root (see
    /// `nondistribution/README.md`). Mirrors
    /// `crates/machine-hosted/tests/real_rom.rs`'s `fixture()` helper --
    /// same shape, same reasoning: resolving relative to
    /// `CARGO_MANIFEST_DIR` keeps the path stable across machines, so
    /// absence means "this machine has no media" rather than "the path
    /// rotted under someone else's checkout".
    fn nondistribution_fixture(env_var: &str, name: &str) -> String {
        if let Ok(path) = std::env::var(env_var) {
            return path;
        }
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../nondistribution")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    /// The AROS 68k main ROM, vendored in-repo -- see
    /// `assets/aros/PROVENANCE.md`. Freely redistributable, so this
    /// (unlike every Kickstart fixture below) is not optional: if this
    /// path doesn't resolve, the repository itself is incomplete.
    const AROS_MAIN: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/aros/aros-amiga-m68k-rom.bin"
    );
    /// The AROS 68k extended ROM. See [`AROS_MAIN`].
    const AROS_EXT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/aros/aros-amiga-m68k-ext.bin"
    );

    #[test]
    fn real_a1200_kickstart_3_2_2() {
        let rom = nondistribution_fixture("M68K_TEST_KICKSTART", "A1200.47.115.rom");
        let Some(image) = load_rom(&rom) else {
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

    /// `identify()` against the vendored AROS main ROM. Runs
    /// unconditionally -- see the module-level doc comment on why this
    /// fixture, unlike every Kickstart one below, is never expected to
    /// be absent.
    #[test]
    fn real_aros_main_rom() {
        let image = load_rom(AROS_MAIN).expect("AROS main ROM is vendored in-repo (assets/aros/)");
        let info = identify(&image).expect("AROS main ROM should have a valid header");
        assert_eq!(info.kind, RomKind::Aros);
        assert_eq!(info.rev, (46, 12));
        assert_eq!(info.size, 512 * 1024);
        assert!(info.bootable);
        assert!(info.checksum_ok);
        // The header's own exec_rev claim (46, 12) undersells what's
        // actually inside; the resident-derived version is what the
        // running OS reports. Confirmed against this exact vendored
        // file's own `PROVENANCE.md` ("Local verification" section) --
        // 51.9, not the 51.8 an earlier revision of this ROM reported.
        assert_eq!(info.exec_rev, (51, 9));
    }

    /// `identify()` against the vendored AROS extended ROM. See
    /// [`real_aros_main_rom`].
    #[test]
    fn real_aros_ext_rom() {
        let image = load_rom(AROS_EXT).expect("AROS ext ROM is vendored in-repo (assets/aros/)");
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

    /// A Kickstart 3.1 ROM -- deliberately *not* one exact image.
    /// Kickstart 3.1 genuinely shipped as both 40.63 (e.g. the A4000)
    /// and 40.68 (e.g. the A1200) depending on the machine; both are
    /// real, correct 3.1 ROMs, and pinning either one as "the" 3.1
    /// revision would fail on a legitimate image of the other. This
    /// asserts what `identify()` is actually responsible for getting
    /// right for *any* 3.1 image: major version, kind, bootability, and
    /// checksum validity -- not which specific machine it shipped on.
    #[test]
    fn real_kickstart_3_1() {
        let rom = nondistribution_fixture("M68K_TEST_KICKSTART_3_1", "kickstart-3.1.rom");
        let Some(image) = load_rom(&rom) else {
            return;
        };
        let info = identify(&image).expect("Kickstart 3.1 should have a valid header");
        assert_eq!(info.kind, RomKind::Kickstart);
        assert_eq!(info.rev.0, 40, "Kickstart 3.1 is ROM version 40");
        assert!(info.bootable);
        assert!(info.checksum_ok);
        assert_eq!(info.supported, Ok(()));
    }

    /// A Kickstart 3.2 ROM. Same reasoning as [`real_kickstart_3_1`]:
    /// AmigaOS 3.2's point releases (3.2, 3.2.1, 3.2.2, ...) span
    /// several distinct ROM revisions under the one version number 47,
    /// so this asserts the shape any of them must have, not one exact
    /// revision. (`real_a1200_kickstart_3_2_2` above is the deliberately
    /// exact counterpart: it names one specific point release and pins
    /// its one specific file.)
    #[test]
    fn real_kickstart_3_2() {
        let rom = nondistribution_fixture("M68K_TEST_KICKSTART_3_2", "kickstart-3.2.rom");
        let Some(image) = load_rom(&rom) else {
            return;
        };
        let info = identify(&image).expect("Kickstart 3.2 should have a valid header");
        assert_eq!(info.kind, RomKind::Kickstart);
        assert_eq!(info.rev.0, 47, "Kickstart 3.2 is ROM version 47");
        assert!(info.bootable);
        assert!(info.checksum_ok);
        assert_eq!(info.supported, Ok(()));
    }

    /// One specific, individually-built ROM (`tools/amibake`'s
    /// `kickstart-47.7.rom` output) -- unlike the two tests above, this
    /// names one exact reproducible artifact, so pinning its exact
    /// revision is legitimate: the test is "this named file is what it
    /// claims to be", not "any 3.2 ROM has this revision".
    #[test]
    fn real_kickstart_47_7() {
        let rom = nondistribution_fixture("M68K_TEST_KICKSTART_47_7", "kickstart-47.7.rom");
        let Some(image) = load_rom(&rom) else {
            return;
        };
        let info = identify(&image).expect("kickstart-47.7.rom should have a valid header");
        assert_eq!(info.kind, RomKind::Kickstart);
        assert_eq!(info.rev, (47, 96));
        assert_eq!(info.exec_rev, (47, 7));
        assert!(info.bootable);
        assert!(info.checksum_ok);
    }

    /// Kickstart 1.3, ROM revision 34.5 -- one specific, well-known
    /// image (distributed as a 256 KB image doubled to fill a 512 KB
    /// EEPROM footprint, see `doubled_copy`'s doc comment), and this
    /// project's real-world regression case for
    /// [`Unsupported::TooOldForZorroIII`].
    #[test]
    fn real_kickstart_34_5_doubled_is_too_old_for_zorro_iii() {
        let rom = nondistribution_fixture("M68K_TEST_KICKSTART_34_5", "kickstart-34.5.rom");
        let Some(image) = load_rom(&rom) else {
            return;
        };
        assert_eq!(image.len(), 512 * 1024);
        let info = identify(&image).expect("doubled 34.5 image should still identify");
        assert!(
            info.doubled,
            "kickstart-34.5.rom is a 256K image doubled to 512K"
        );
        assert_eq!(info.size, 512 * 1024);
        assert_eq!(info.rev, (34, 5));
        assert_eq!(info.supported, Err(Unsupported::TooOldForZorroIII));
    }

    /// Another individually-built, exactly-named `tools/amibake` output
    /// -- see [`real_kickstart_47_7`]'s doc comment for why an exact
    /// pin is legitimate here.
    #[test]
    fn real_kickstart_46_143_is_byte_swapped_and_corrects_to_a_valid_checksum() {
        let rom = nondistribution_fixture("M68K_TEST_KICKSTART_46_143", "kickstart-46.143.rom");
        let Some(mut image) = load_rom(&rom) else {
            return;
        };

        // As distributed, this file's header only reads correctly once
        // byte-swapped: `identify` on the raw bytes still succeeds (via
        // the swapped fast path) and never refuses the image.
        assert!(is_byte_swapped(&image));
        let swapped_info = identify(&image).expect("swapped header still identifies");
        assert_eq!(swapped_info.rev, (46, 143));
        assert_eq!(swapped_info.supported, Ok(()));

        // The payoff: correcting in place and re-identifying gives a
        // fully-parsed ROM whose *own* checksum validates — proof the
        // correction is right, not merely plausible.
        unswap_in_place(&mut image);
        assert!(
            !is_byte_swapped(&image),
            "corrected image is no longer swapped"
        );
        let info = identify(&image).expect("corrected image should have a valid header");
        assert_eq!(info.kind, RomKind::Kickstart);
        assert_eq!(info.rev, (46, 143));
        assert!(info.checksum_ok, "corrected image's checksum must validate");
        assert!(info.bootable);
        assert_eq!(info.supported, Ok(()));
    }

    #[test]
    fn swapping_a_real_known_good_rom_round_trips_to_identical_identification() {
        // A1200.47.115.rom is already correct (not swapped); prove the
        // swap/unswap machinery is lossless against a real, large,
        // non-synthetic image, not just the small synthetic fixtures.
        let rom = nondistribution_fixture("M68K_TEST_KICKSTART", "A1200.47.115.rom");
        let Some(original) = load_rom(&rom) else {
            return;
        };
        let original_info = identify(&original).expect("valid header");

        let mut roundtrip = original.clone();
        unswap_in_place(&mut roundtrip);
        assert!(is_byte_swapped(&roundtrip));
        unswap_in_place(&mut roundtrip);
        assert_eq!(
            roundtrip, original,
            "double-swapping a real ROM must restore it exactly"
        );

        let roundtrip_info = identify(&roundtrip).expect("valid header after round trip");
        assert_eq!(roundtrip_info.kind, original_info.kind);
        assert_eq!(roundtrip_info.rev, original_info.rev);
        assert_eq!(roundtrip_info.exec_rev, original_info.exec_rev);
        assert_eq!(roundtrip_info.checksum_ok, original_info.checksum_ok);
        assert_eq!(roundtrip_info.bootable, original_info.bootable);
        assert_eq!(roundtrip_info.supported, original_info.supported);
    }
}
