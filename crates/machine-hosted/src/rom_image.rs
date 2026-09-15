//! Loading Kickstart ROM images from disk, then normalizing and
//! identifying them through the project owner's `amiga-rom` crate
//! (`amiga_rom::Loader`/`amiga_rom::KickRom`, pinned in
//! `[workspace.dependencies]` -- see that pin's own comment for why).
//!
//! Two jobs, in order:
//!
//! - **Normalize** ([`Loader::normalize`]): turn whatever bytes a user
//!   actually has -- a byte-swapped dump, or a Cloanto/Amiga Forever
//!   `AMIROMTYPE1`-framed image -- into the canonical big-endian layout
//!   `MachineBus` expects. This is the behaviour change from the old
//!   `machine_core::rom`-only path: that module could only *report* a
//!   byte-swap (it has no `alloc`, so it had no buffer to write a
//!   correction into), leaving the raw swapped bytes to reach the bus
//!   unchanged -- which could never boot. Here, correction actually
//!   happens before the bytes are handed to `MachineBus::new`.
//! - **Identify** ([`KickRom::info`]): report what the (now-canonical)
//!   image looks like, for the same purely advisory reason
//!   `machine_core::rom`'s own module doc comment gives -- logging and
//!   the "which OS am I booting" question, never a gate on whether the
//!   guest actually runs.
//!
//! `machine_core::rom` remains the allocator-free path for the bare-metal
//! board crates (`board-qemu-virt`/`board-qemu-q35`): it is `no_std`, no
//! `alloc`, by necessity, since those builds have no heap to hand a
//! `Loader::normalize`-style owned `Vec<u8>` out of. `machine-hosted` is a
//! `std` binary with nothing stopping it from doing better, so it uses
//! this richer crate instead of hand-rolling (and keeping in sync) a
//! second copy of the same byte-order/Cloanto logic `machine-core` must
//! keep minimal for a different set of constraints. Neither module
//! replaces the other; they serve different targets.
//!
//! Identification here is tolerant by construction, matching
//! `machine_core::rom::identify`'s own stated posture: an image
//! [`amiga_rom::Loader::detect`] can't classify at all, or one too short
//! for [`KickRom`]'s header fields to resolve, still boots -- it is
//! logged as unidentified, never refused. The one case that *is* refused
//! is a Cloanto-encoded image with no key to decode it: booting the raw
//! XOR'd bytes would not be "tolerant", it would be silently running
//! noise that happens to be the right length, so that case is a clear
//! setup error instead (see [`prepare`]'s doc comment).

use std::fs;
use std::io;
use std::path::Path;

use amiga_rom::{KickRom, Loader, LoaderError, RomEncoding};

use crate::console::Console;

/// Read a ROM image whole. `MachineBus` mirrors undersized images and
/// simply ignores address bits beyond an oversized one, so no padding or
/// truncation is needed here.
pub fn load(path: &Path) -> io::Result<Vec<u8>> {
    fs::read(path)
}

/// The pure result of [`prepare_bytes`]: the bytes to hand to
/// `MachineBus`, plus the diagnostic lines to print. Kept separate from
/// [`Console`] so unit tests can assert on both without constructing one
/// -- [`prepare`] is the thin wrapper that actually writes `lines` to a
/// real `Console`.
#[derive(Debug)]
struct Prepared {
    bytes: Vec<u8>,
    lines: Vec<String>,
}

/// Normalizes `raw` through [`Loader::normalize`] and reports what the
/// result looks like, for one labelled ROM (`"main"`/`"ext"` in
/// `run.rs`). `key`, when given, is a Cloanto/Amiga Forever `rom.key`'s
/// contents (`--rom-key`).
///
/// # Errors
///
/// The only refusal: `raw` is Cloanto-encoded
/// ([`LoaderError::KeyRequired`]/[`LoaderError::InvalidKey`]) and no
/// usable key was supplied. The returned message names `label` and
/// `--rom-key` explicitly, so `run.rs`'s `setup_error` surfaces something
/// a user can act on rather than a bare enum debug print.
///
/// # Never refused
///
/// Every other outcome -- an image [`Loader::detect`] can't classify at
/// all ([`LoaderError::UnknownFormat`]), one whose length doesn't divide
/// evenly for a detected-but-unreorderable byte order
/// ([`LoaderError::UnalignedLength`]), or one that normalizes fine but is
/// too short for [`KickRom`]'s header-field methods to return anything
/// but `None` -- falls back to the same tolerant "unidentified ...
/// booting anyway" report `machine_core::rom::identify` returning `None`
/// used to produce. Identification is advisory; it must never be the
/// reason a tiny synthetic smoke-test ROM, or an odd user image, stops
/// booting.
pub fn prepare(
    console: &mut Console,
    label: &str,
    raw: Vec<u8>,
    key: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let prepared = prepare_bytes(label, raw, key)?;
    for line in &prepared.lines {
        console.diag(line);
    }
    Ok(prepared.bytes)
}

/// The `Console`-free core of [`prepare`] -- see that function's doc
/// comment for the full contract. Split out so unit tests below can
/// assert on the exact bytes and lines produced without a [`Console`] to
/// construct.
fn prepare_bytes(label: &str, raw: Vec<u8>, key: Option<&[u8]>) -> Result<Prepared, String> {
    match Loader::normalize(&raw, key) {
        Ok(normalized) => {
            let mut lines = Vec::new();
            if normalized != raw {
                // The bytes changed, so `Loader::detect` on the original
                // input must have found something to correct -- re-run it
                // (cheap: only the leading bytes are inspected) purely to
                // say *what* kind of correction this was, since
                // `normalize`'s own return value doesn't carry that.
                match Loader::detect(&raw) {
                    Some(RomEncoding::CloantoEncoded) => lines.push(format!(
                        "{label} ROM: Cloanto/Amiga Forever-encoded image decoded using --rom-key"
                    )),
                    // Worth a line of its own: the pre-`amiga-rom` code
                    // here could only *report* a swap, never fix it, so
                    // a swapped dump silently reached the bus unbootable.
                    // The correction itself is the news; that history
                    // stays in this comment, not the user's console.
                    Some(RomEncoding::Raw(order)) => lines.push(format!(
                        "{label} ROM: byte-swapped dump corrected ({order:?} -> canonical order)"
                    )),
                    None => {
                        // Unreachable in practice: `normalize` only ever
                        // changes the bytes after `detect` classified
                        // them as something correctable. No line to add
                        // beyond the identity report below either way.
                    }
                }
            }
            lines.extend(identity_lines(label, &normalized));
            Ok(Prepared {
                bytes: normalized,
                lines,
            })
        }
        Err(LoaderError::KeyRequired) => Err(format!(
            "{label} ROM is Cloanto/Amiga Forever-encoded (starts with the AMIROMTYPE1 \
             container magic) and needs its rom.key to decode -- pass --rom-key \
             <path-to-rom.key> (Amiga Forever's rom.key file) and try again"
        )),
        // Distinct from `KeyRequired`: here a key WAS passed, but it was
        // unusable (empty). Telling this user to "pass --rom-key" would
        // point them at the flag they already used.
        Err(LoaderError::InvalidKey) => Err(format!(
            "{label} ROM is Cloanto/Amiga Forever-encoded, but the file passed via \
             --rom-key is empty/unusable as a rom.key -- point --rom-key at Amiga \
             Forever's actual rom.key file and try again"
        )),
        // "Could not classify this data at all" (`UnknownFormat`), or
        // classified but not a whole number of 4-byte groups to reorder
        // (`UnalignedLength`, which in practice cannot happen for a real
        // ROM dump -- canonical sizes are themselves multiples of 4 --
        // but a caller can still hand in arbitrary bytes): neither gates
        // booting. `_` also covers `MismatchedHiLoLength`/
        // `OddHiLoLength`, which `Loader::normalize` never actually
        // returns (those are `merge_hi_lo`-only, and this module never
        // calls `merge_hi_lo`) -- matched here anyway so a future
        // `amiga-rom` revision adding a new `normalize` error path falls
        // back to this same tolerant behaviour rather than a silent
        // exhaustiveness break.
        Err(_) => {
            let lines = vec![tolerant_line(label, raw.len())];
            Ok(Prepared { bytes: raw, lines })
        }
    }
}

/// The identity report for an already-normalized image: [`KickRom::info`]
/// plus, when it finds any signal, one further
/// [`KickRom::machine_hints`] line. Falls back to [`tolerant_line`]
/// rather than unwrapping when `normalized` is too short for
/// [`KickRom`]'s header-field methods (`rom_rev`/`exec_rev`/`boot_pc`) to
/// return anything but `None` -- the same "never gate booting" tolerance
/// [`prepare_bytes`]'s error arm gives a `Loader::detect` failure.
fn identity_lines(label: &str, normalized: &[u8]) -> Vec<String> {
    let rom = KickRom::new(normalized);
    let info = rom.info();
    let (Some(rom_rev), Some(exec_rev), Some(boot_pc)) =
        (info.rom_rev, info.exec_rev, info.boot_pc)
    else {
        return vec![tolerant_line(label, normalized.len())];
    };

    // `info.is_kick` is the full `is_kick_rom` conjunction (size, header,
    // footer, size field, checksum); it does not require a reset vector
    // (`magic_reset_ok`), so the AROS extended ROM -- a valid Kickstart-
    // format image with no boot vector at all -- still reports
    // "Kickstart" here, matching `is_kick`'s own documented scope rather
    // than reintroducing a bootable-vs-not distinction into this label.
    // The old code's `RomKind::Aros`/`RomKind::Kickstart`/`RomKind::Unknown`
    // string-scan distinction lived in `machine_core::rom`, not here, and
    // is deliberately not reimplemented against this crate (task brief) --
    // losing the literal word "Aros" from this one advisory line is the
    // accepted cost.
    let form = if info.is_kick {
        "Kickstart"
    } else {
        "Kickstart-format (unverified)"
    };

    let mut lines = vec![format!(
        "{label} ROM: {form} rev {}.{}, exec {}.{}, {} bytes, checksum {}, boot_pc {:#010x}, \
         doubled {}",
        rom_rev.0,
        rom_rev.1,
        exec_rev.0,
        exec_rev.1,
        normalized.len(),
        if info.chk_sum_ok { "ok" } else { "FAILED" },
        boot_pc,
        if info.doubled_ok { "yes" } else { "no" },
    )];

    // `machine_hints` is explicitly a heuristic (its own doc comment): an
    // all-`None`/`false` result means "no signal found", not "not a
    // Kickstart ROM", so it earns a line only when it actually found
    // something -- no noise on AROS or synthetic test images, which carry
    // none of the resident names this looks for.
    let hints = rom.machine_hints();
    if hints.named_machine.is_some()
        || hints.has_pcmcia
        || hints.has_ncr_scsi
        || hints.is_aros
        || hints.target_platform.is_some()
    {
        // AROS first: `is_aros` (an `aros.library` resident) is the one
        // signal the crate calls reliable rather than heuristic, and it
        // already forces `has_pcmcia` false upstream -- AROS ships
        // `card.resource` generically, so before the pinned fix this
        // line reported "PCMCIA yes" on the vendored AROS pair this
        // project boots routinely. `target_platform` is AROS's own
        // documented `<platform>-<cpu>` port token (e.g. `amiga-m68k`).
        let machine = if hints.is_aros {
            match hints.target_platform {
                Some(port) => format!("AROS ({})", String::from_utf8_lossy(port)),
                None => "AROS".to_string(),
            }
        } else {
            hints
                .named_machine
                .map(|name| String::from_utf8_lossy(name).into_owned())
                .unwrap_or_else(|| "unknown".to_string())
        };
        lines.push(format!(
            "{label} ROM: machine hints: {machine}, PCMCIA {}, NCR SCSI {} (best-effort, from \
             resident names -- not a confirmed identification)",
            if hints.has_pcmcia { "yes" } else { "no" },
            if hints.has_ncr_scsi { "yes" } else { "no" },
        ));
    }

    lines
}

/// The shared "could not identify, but booting anyway" line -- both
/// [`prepare_bytes`]'s `Loader::detect`-failure arm and
/// [`identity_lines`]'s too-short-to-parse fallback produce exactly this
/// wording, matching the old `machine_core::rom::identify`-returned-`None`
/// line's own tolerant tone.
fn tolerant_line(label: &str, len: usize) -> String {
    format!(
        "{label} ROM: unidentified ({len} bytes) -- amiga_rom couldn't classify or fully parse \
         this image's header; booting anyway"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Resolve a fixture this repo may not carry, the same way
    /// `tests/real_rom.rs`'s `fixture()` helper does: an environment
    /// variable if the caller sets one, otherwise `nondistribution/` at
    /// the repo root (see `nondistribution/README.md`). Duplicated here
    /// rather than shared, since `tests/real_rom.rs` is a separate
    /// integration-test binary this module (compiled into the `main`
    /// binary crate) cannot import from.
    fn fixture(env_var: &str, name: &str) -> String {
        if let Ok(path) = std::env::var(env_var) {
            return path;
        }
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../nondistribution")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    /// Load a fixture, printing a skip notice and returning `None` if
    /// it's absent -- same "skip cleanly" contract every real-ROM test in
    /// this repository follows.
    fn load_fixture(env_var: &str, name: &str) -> Option<Vec<u8>> {
        let path = fixture(env_var, name);
        match fs::read(&path) {
            Ok(data) => Some(data),
            Err(e) => {
                eprintln!("SKIP: {path} not present ({e}) -- see nondistribution/README.md");
                None
            }
        }
    }

    fn prepared_ok(label: &str, raw: Vec<u8>, key: Option<&[u8]>) -> Prepared {
        prepare_bytes(label, raw, key).expect("expected prepare_bytes to succeed")
    }

    // ---- real-fixture tests (skip cleanly when the media is absent) ---

    #[test]
    fn a1200_47_115_normalizes_to_identity_and_reports_the_known_revision() {
        let Some(raw) = load_fixture("M68K_TEST_KICKSTART", "A1200.47.115.rom") else {
            return;
        };
        let original = raw.clone();
        let prepared = prepared_ok("main", raw, None);
        assert_eq!(
            prepared.bytes, original,
            "a plain, correctly-ordered dump must normalize to itself"
        );
        let joined = prepared.lines.join("\n");
        assert!(
            joined.contains("rev 47.115"),
            "expected the known A1200 3.2.2 ROM revision in: {joined}"
        );
        assert!(
            joined.contains("exec 47.13"),
            "expected exec 47.13 in: {joined}"
        );
        assert!(
            joined.contains("checksum ok"),
            "expected a valid checksum in: {joined}"
        );
        assert!(
            joined.contains("doubled no"),
            "expected doubled no in: {joined}"
        );
    }

    /// The key test (task brief): `kickstart-34.5.rom` is Kickstart 1.3's
    /// doubled 512 KB layout (a 256 KB image mirrored twice to fill a
    /// 512 KB EEPROM footprint). `RomInfo::doubled_ok` is the fact
    /// `amiga-rom` 0.3.0 on crates.io cannot report at all (it lacks
    /// `KickRom::check_doubled` entirely) -- this test passing is the
    /// proof that the pinned `rev` dependency, not the stale release,
    /// is actually what got built.
    #[test]
    fn kickstart_34_5_doubled_normalizes_to_identity_and_reports_doubled_yes() {
        let Some(raw) = load_fixture("M68K_TEST_KICKSTART_34_5", "kickstart-34.5.rom") else {
            return;
        };
        let original = raw.clone();
        let prepared = prepared_ok("main", raw, None);
        assert_eq!(
            prepared.bytes, original,
            "a doubled image needs no byte-order correction, only interpretation"
        );
        let joined = prepared.lines.join("\n");
        assert!(
            joined.contains("rev 34.5"),
            "expected rev 34.5 in: {joined}"
        );
        assert!(
            joined.contains("doubled yes"),
            "expected doubled yes -- this is the whole point of this test; got: {joined}"
        );
    }

    #[test]
    fn kickstart_46_143_byte_swapped_dump_is_corrected_and_then_validates() {
        let Some(raw) = load_fixture("M68K_TEST_KICKSTART_46_143", "kickstart-46.143.rom") else {
            return;
        };
        let prepared = prepared_ok("main", raw.clone(), None);
        assert_ne!(
            prepared.bytes, raw,
            "a byte-swapped dump must come out of prepare_bytes corrected, not verbatim"
        );
        let joined = prepared.lines.join("\n");
        assert!(
            joined.contains("corrected"),
            "expected a diag line mentioning the byte-swap correction in: {joined}"
        );
        assert!(
            joined.contains("rev 46.143"),
            "expected rev 46.143 in: {joined}"
        );
        assert!(
            joined.contains("checksum ok"),
            "expected the corrected image's own checksum to validate in: {joined}"
        );
    }

    #[test]
    fn kickstart_3_1_reports_major_version_40_with_a_valid_checksum() {
        let Some(raw) = load_fixture("M68K_TEST_KICKSTART_3_1", "kickstart-3.1.rom") else {
            return;
        };
        let prepared = prepared_ok("main", raw, None);
        let joined = prepared.lines.join("\n");
        assert!(
            joined.contains("rev 40."),
            "expected ROM version 40.x in: {joined}"
        );
        assert!(
            joined.contains("checksum ok"),
            "expected a valid checksum in: {joined}"
        );
    }

    #[test]
    fn kickstart_3_2_reports_major_version_47_with_a_valid_checksum() {
        let Some(raw) = load_fixture("M68K_TEST_KICKSTART_3_2", "kickstart-3.2.rom") else {
            return;
        };
        let prepared = prepared_ok("main", raw, None);
        let joined = prepared.lines.join("\n");
        assert!(
            joined.contains("rev 47."),
            "expected ROM version 47.x in: {joined}"
        );
        assert!(
            joined.contains("checksum ok"),
            "expected a valid checksum in: {joined}"
        );
    }

    #[test]
    fn kickstart_47_7_reports_its_exact_pinned_revision() {
        let Some(raw) = load_fixture("M68K_TEST_KICKSTART_47_7", "kickstart-47.7.rom") else {
            return;
        };
        let prepared = prepared_ok("main", raw, None);
        let joined = prepared.lines.join("\n");
        assert!(
            joined.contains("rev 47.96"),
            "expected rev 47.96 in: {joined}"
        );
        assert!(
            joined.contains("exec 47.7"),
            "expected exec 47.7 in: {joined}"
        );
        assert!(
            joined.contains("checksum ok"),
            "expected a valid checksum in: {joined}"
        );
    }

    // ---- vendored AROS ROMs (always present, unconditional) -----------

    const AROS_MAIN: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/aros/aros-amiga-m68k-rom.bin"
    );
    const AROS_EXT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/aros/aros-amiga-m68k-ext.bin"
    );

    /// Regression guard: `Loader::normalize` must not mangle an image
    /// that is already in canonical byte order and not Cloanto-encoded --
    /// the AROS main ROM comes back byte-identical.
    ///
    /// Also pins the machine-hints AROS fix the `9e123e0` pin exists
    /// for: under the previous rev this exact vendored image reported
    /// "unknown, PCMCIA yes" (AROS ships `card.resource` generically,
    /// which the earlier heuristic misread as a PCMCIA machine); the
    /// fixed heuristic keys off the `aros.library` resident and the
    /// documented `amiga-m68k` port token instead, and forces the
    /// PCMCIA signal off. Verified against this fixture's actual
    /// output, not assumed from upstream's own (different) AROS sample.
    #[test]
    fn aros_main_rom_passes_through_prepare_byte_identical() {
        let raw = fs::read(AROS_MAIN).expect("AROS main ROM is vendored in-repo (assets/aros/)");
        let original = raw.clone();
        let prepared = prepared_ok("main", raw, None);
        assert_eq!(prepared.bytes, original);
        let joined = prepared.lines.join("\n");
        assert!(
            joined.contains("machine hints: AROS (amiga-m68k)"),
            "expected the AROS identity (with its port token) in the hints line: {joined}"
        );
        assert!(
            joined.contains("PCMCIA no"),
            "PCMCIA yes on AROS is the exact false positive the pinned rev fixes: {joined}"
        );
    }

    /// Same regression guard for the AROS *extended* ROM, which
    /// additionally has no reset vector at all -- `identity_lines` must
    /// still produce a normal identity line (not the too-short fallback),
    /// since every header field it reads is well within this image's
    /// 512 KB length.
    #[test]
    fn aros_ext_rom_with_no_reset_vector_passes_through_and_is_still_tolerated() {
        let raw = fs::read(AROS_EXT).expect("AROS ext ROM is vendored in-repo (assets/aros/)");
        let original = raw.clone();
        let prepared = prepared_ok("ext", raw, None);
        assert_eq!(prepared.bytes, original);
        let joined = prepared.lines.join("\n");
        assert!(
            joined.contains("rev 46.11"),
            "expected the ext ROM's own header revision in: {joined}"
        );
    }

    // ---- synthetic Cloanto container (no real Amiga Forever media) ----
    //
    // These build a container by hand rather than reading a real Amiga
    // Forever ROM: no such file ships with, or is fetched by, this
    // project.

    fn make_cloanto_container(payload: &[u8], key: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"AMIROMTYPE1");
        for (i, &b) in payload.iter().enumerate() {
            out.push(b ^ key[i % key.len()]);
        }
        out
    }

    #[test]
    fn cloanto_encoded_image_without_a_key_is_refused_with_an_actionable_message() {
        let key = b"synthetic-test-key";
        let payload = vec![0xAAu8; 64];
        let container = make_cloanto_container(&payload, key);
        let err = prepare_bytes("main", container, None)
            .expect_err("a Cloanto-encoded image with no key must be refused");
        assert!(
            err.contains("--rom-key"),
            "expected the refusal to name --rom-key: {err}"
        );
    }

    #[test]
    fn cloanto_encoded_image_with_the_right_key_decodes_to_the_original_payload() {
        let key = b"synthetic-test-key";
        let payload = vec![0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let container = make_cloanto_container(&payload, key);
        let prepared = prepared_ok("main", container, Some(key));
        assert_eq!(
            prepared.bytes, payload,
            "decoding must XOR-cycle the key over the payload exactly as \
             amiga_rom's decode_cloanto does"
        );
        let joined = prepared.lines.join("\n");
        assert!(
            joined.contains("Cloanto"),
            "expected a diag line noting the Cloanto decode in: {joined}"
        );
    }

    // ---- tolerance for data amiga-rom can't classify at all -----------

    /// Protects smoke-test-sized fixtures (`tests/smoke.rs`'s synthetic
    /// ROM is a full 512 KB image with no recognised boot-vector
    /// signature at all -- its first longword is an initial stack
    /// pointer, not a magic) from ever being refused by identification:
    /// `Loader::detect` returning `None` must fall back to the tolerant
    /// line and pass the bytes through unchanged, never gate booting.
    #[test]
    fn unrecognisable_image_passes_through_unchanged_and_is_tolerated() {
        let raw = vec![0x00u8; 2048];
        let prepared = prepared_ok("main", raw.clone(), None);
        assert_eq!(prepared.bytes, raw);
        let joined = prepared.lines.join("\n");
        assert!(
            joined.contains("unidentified") && joined.contains("booting anyway"),
            "expected the tolerant unidentified line in: {joined}"
        );
    }
}
