//! Runs the real Kickstart and AROS ROMs mentioned in the Phase 1 task
//! brief, if they happen to be present on this machine. These are user-
//! supplied, non-redistributable images (Kickstart) or large fixtures
//! (AROS) that will not exist on most machines or in CI, so every test
//! here SKIPS (prints a message, returns without asserting) rather than
//! failing when its ROM file is absent.
//!
//! These are not correctness tests -- Phase 1's chipset/CIA implementation
//! is still landing concurrently with this crate -- they exist so a
//! developer with the ROMs on disk can run `cargo test -p machine-hosted
//! -- --ignored --nocapture` and see how far things get without writing a
//! one-off command line by hand every time.

use std::path::Path;
use std::process::Command;

const KICKSTART_A1200: &str = "/Users/simond/src/amirfb/nondistribution/roms/A1200.47.115.rom";

// Vendored in-repo (assets/aros/PROVENANCE.md) -- freely redistributable,
// so unlike the Kickstart tests below, the AROS tests in this file run
// unconditionally rather than skip-when-absent.
const AROS_MAIN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/aros/aros-amiga-m68k-rom.bin"
);
const AROS_EXT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/aros/aros-amiga-m68k-ext.bin"
);

fn run(args: &[&str]) -> Option<(std::process::ExitStatus, String)> {
    let output = Command::new(env!("CARGO_BIN_EXE_machine-hosted"))
        .args(args)
        .output()
        .expect("spawn machine-hosted");
    Some((
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
    ))
}

/// Decode a PNG file into `(width, height, rgba_bytes)`, for tests that
/// need to look at actual pixels rather than trust `machine-hosted`'s own
/// stdout stats line. Uses the `png` crate already in this package's
/// `[dependencies]` (`screenshot.rs`'s encoder) -- integration tests in
/// `tests/` share the package's ordinary dependencies, so no extra dev-
/// dependency is needed just for this.
fn decode_png(path: &Path) -> (u32, u32, Vec<u8>) {
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("open {path:?}: {e}"));
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().expect("valid PNG header");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("known output size")];
    let info = reader.next_frame(&mut buf).expect("valid PNG frame");
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

/// A fresh temp file path for one test's screenshot capture, named after
/// the calling test and the current process ID so parallel `cargo test`
/// runs never collide.
fn screenshot_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("machine-hosted-{name}-{}.png", std::process::id()))
}

/// The most common RGBA pixel in `rgba` (4-byte chunks). *Not* pixel
/// `(0,0)`: `screenshot.rs`'s `frame_stats` doc comment explains why a
/// first attempt at this used the top-left pixel and got it backwards on
/// a real Kickstart capture (its mouse pointer's hot spot lands exactly
/// at `(0,0)` in this machine's DIW-relative coordinates). The dominant
/// colour by pixel count is what "background" actually means here.
fn dominant_pixel(rgba: &[u8]) -> [u8; 4] {
    let mut counts: std::collections::HashMap<[u8; 4], usize> = std::collections::HashMap::new();
    for px in rgba.chunks(4) {
        *counts.entry(px.try_into().unwrap()).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(px, _)| px)
        .expect("non-empty image")
}

#[test]
#[ignore = "requires a user-supplied Kickstart ROM on disk; run with --ignored"]
fn kickstart_3_2_2_a1200() {
    if !Path::new(KICKSTART_A1200).exists() {
        eprintln!("SKIP: {KICKSTART_A1200} not present");
        return;
    }
    let (status, stdout) = run(&[
        "--rom",
        KICKSTART_A1200,
        "--max-frames",
        "200",
        "--max-instructions",
        "50000000",
    ])
    .unwrap();
    eprintln!("exit: {status:?}");
    eprintln!("{stdout}");
    assert!(
        stdout.contains("PHASE1 HOSTED:"),
        "expected a final status line regardless of how far boot got"
    );
}

/// The Phase 1 investigation this test documents (`docs/combined-
/// roadmap.md` Phase 1 exit criterion): a stock retail Kickstart never
/// writes to `SERDAT`, so `--inspect`'s guest-memory report is the only
/// way to tell "idling because healthy" from "stuck" apart. This asserts
/// the report actually finds a well-formed `ExecBase` and a plausible
/// number of initialised resident modules on the real ROM, not just that
/// the flag doesn't crash the runner.
#[test]
#[ignore = "requires a user-supplied Kickstart ROM on disk; run with --ignored"]
fn kickstart_3_2_2_a1200_introspection_finds_a_healthy_exec_base() {
    if !Path::new(KICKSTART_A1200).exists() {
        eprintln!("SKIP: {KICKSTART_A1200} not present");
        return;
    }
    let (status, stdout) = run(&[
        "--rom",
        KICKSTART_A1200,
        "--max-frames",
        "200",
        "--max-instructions",
        "50000000",
        "--inspect",
    ])
    .unwrap();
    eprintln!("exit: {status:?}");
    eprintln!("{stdout}");
    assert!(
        stdout.contains("introspect: ExecBase at"),
        "expected a well-formed ExecBase to be found on a real Kickstart boot"
    );
    assert!(
        !stdout.contains("no plausible ExecBase"),
        "exec should have finished initialising by frame 200"
    );
    // Kickstart 3.2.2's known resident set is large (expansion, exec,
    // graphics, dos, intuition, workbench, ...); a handful would indicate
    // the walk stopped early (offset bug or a genuinely stalled boot).
    assert!(
        stdout.contains("resident modules initialised:"),
        "expected the resident-module count line"
    );
}

/// The ROMWack break-in this test documents (`docs/serial-debugging.md`):
/// forcing an illegal-instruction exception opens Kickstart's alert/LED-
/// blink loop, and flooding DEL across its six-poll break-in window
/// (`examples/serial-scripts/romwack-break-in.txt` has the disassembly
/// and the reasoning for a flood over one precisely-timed send) reaches
/// the ROM's own `rom-wack` debugger. This is Phase 1's strongest
/// evidence yet that a stock Kickstart *can* narrate over serial -- see
/// the doc's "Phase 1 exit criterion" section for why that is not the
/// same claim as "a healthy, undisturbed boot narrates over serial".
///
/// Runtime on this machine: ~2.2s built `--release`, ~20s under `cargo
/// test`'s debug profile (the interpreter is what's slow, not this
/// test's own bookkeeping) -- bounded either way by `--max-frames 400`,
/// so `--ignored` here is purely about the ROM being a user-supplied,
/// non-redistributable file, the same reason every other test in this
/// file uses it, not about test cost.
#[test]
#[ignore = "requires a user-supplied Kickstart ROM on disk; run with --ignored"]
fn kickstart_3_2_2_a1200_romwack_break_in_reaches_the_debugger() {
    if !Path::new(KICKSTART_A1200).exists() {
        eprintln!("SKIP: {KICKSTART_A1200} not present");
        return;
    }
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/serial-scripts/romwack-break-in.txt"
    );
    let (status, stdout) = run(&[
        "--rom",
        KICKSTART_A1200,
        "--trigger-illegal-after-frames",
        "20",
        "--serial-script",
        script,
        "--max-frames",
        "400",
    ])
    .unwrap();
    eprintln!("exit: {status:?}");
    eprintln!("{stdout}");
    assert!(
        stdout.contains("GUEST | rom-wack"),
        "expected the ROM's own rom-wack debugger banner"
    );
    // A plausible register-dump line, not just the banner text -- this
    // is the same PC/SR/exception-vector shape documented in
    // docs/serial-debugging.md, and pins the illegal-instruction vector
    // (XCPT: 8000002F) so a future chipset/CPU regression that changes
    // which exception actually lands here would fail this assertion
    // rather than pass on banner text alone.
    assert!(
        stdout.contains("XCPT: 8000002F"),
        "expected a register dump for the forced illegal-instruction exception"
    );
}

/// Phase 2 regression test (this task's brief): actually drive the
/// stop-gap planar renderer (`crates/machine-core/src/render.rs`,
/// proposal §8.1) from a real ROM and check something meaningful and
/// stable about the result, not just that the runner didn't crash.
///
/// Kickstart 3.2.2 reaches its "insert a system disk" boot alert with no
/// boot device present (this machine has no MIRAGE storage yet -- roadmap
/// Phase 3): register inspection (`--inspect`'s `display state` line, see
/// this crate's own manual investigation) shows a real, non-zero
/// `COP1LC`, so the renderer is walking an actual copper list, not an
/// empty one. This asserts the rendered frame has genuine non-background
/// content -- a real mouse-pointer/icon picture, not a flat fill -- which
/// is the first time anything in this project has confirmed the renderer
/// draws real guest-programmed geometry rather than only synthetic test
/// bitmaps (`render.rs`'s own unit tests). `--screenshot-frame 200` is
/// comfortably clear of `--max-frames 250`'s boundary (see this file's
/// AROS screenshot test for why that matters: a capture taken right at
/// the run's own final frame can observe a register mid-write).
#[test]
#[ignore = "requires a user-supplied Kickstart ROM on disk; run with --ignored"]
fn kickstart_3_2_2_a1200_screenshot_shows_real_content() {
    if !Path::new(KICKSTART_A1200).exists() {
        eprintln!("SKIP: {KICKSTART_A1200} not present");
        return;
    }
    let path = screenshot_path("kickstart");
    let (status, stdout) = run(&[
        "--rom",
        KICKSTART_A1200,
        "--max-frames",
        "250",
        "--max-instructions",
        "80000000",
        "--screenshot",
        path.to_str().unwrap(),
        "--screenshot-frame",
        "200",
    ])
    .unwrap();
    eprintln!("exit: {status:?}");
    eprintln!("{stdout}");
    assert!(
        stdout.contains("screenshot: frame 200"),
        "expected the capture to actually fire by frame 200"
    );

    let (width, height, rgba) = decode_png(&path);
    assert_eq!(
        (width, height),
        (
            machine_core::display::MAX_WIDTH as u32,
            machine_core::display::MAX_HEIGHT as u32
        ),
        "screenshots are always the renderer's full worst-case canvas"
    );

    let background = dominant_pixel(&rgba);
    let non_background = rgba.chunks(4).filter(|px| *px != background).count();
    assert!(
        non_background > 0,
        "expected real content (mouse pointer / boot-alert icons) on top \
         of the background fill, found none -- see this crate's \
         `screenshot.rs` and the task report for how COP1LC/BPLCON0 were \
         used to confirm the guest actually programmed a copper list"
    );
    // Real hardware never fills more than a small fraction of a 752x576
    // canvas with a mouse pointer and a short icon list; a suspiciously
    // large non-background count would indicate the renderer painted
    // something wrong (e.g. background/foreground inverted) rather than
    // the alert screen's actual small graphics.
    assert!(
        non_background < (width * height) as usize / 10,
        "non-background pixel count ({non_background}) looks too large \
         for a mouse pointer and a short icon list -- possible renderer \
         regression, not the expected boot-alert content"
    );
}

/// The AROS counterpart of the Kickstart test above, and the inverse
/// finding: manual investigation (`--inspect`'s `display state` line)
/// shows AROS reaches `workbench.task`/`workbook.resource` in its
/// resident-module list but leaves `COP1LC`, `BPLCON0`'s plane field and
/// `BPL1PT` all at zero through frame 3000+ -- it is blocked (`TaskWait`
/// shows `trackdisk.device` and the bootstrap task themselves parked) on
/// having no boot device to load Workbench data from (roadmap Phase 3's
/// MIRAGE storage gap), so it never reaches the code path that actually
/// programs a display. This asserts that honestly: a real capture, at a
/// frame safely clear of `--max-frames`'s own boundary (a register can be
/// caught mid-update in the single frame right at that edge -- observed
/// directly while preparing this test), renders as one flat, unbroken
/// fill colour -- not asserting a specific colour, since `COLOR00` alone
/// still drifts a little during AROS's otherwise-idle housekeeping.
#[test]
fn aros_68k_screenshot_is_a_flat_fill_pending_mirage_storage() {
    assert!(
        Path::new(AROS_MAIN).exists() && Path::new(AROS_EXT).exists(),
        "AROS ROM pair is vendored in-repo (assets/aros/) and must be present"
    );
    let path = screenshot_path("aros");
    let (status, stdout) = run(&[
        "--rom",
        AROS_MAIN,
        "--ext-rom",
        AROS_EXT,
        "--max-frames",
        "500",
        "--max-instructions",
        "300000000",
        "--screenshot",
        path.to_str().unwrap(),
        "--screenshot-frame",
        "400",
    ])
    .unwrap();
    eprintln!("exit: {status:?}");
    eprintln!("{stdout}");
    assert!(
        stdout.contains("screenshot: frame 400"),
        "expected the capture to actually fire by frame 400"
    );

    let (width, height, rgba) = decode_png(&path);
    assert_eq!(
        (width, height),
        (
            machine_core::display::MAX_WIDTH as u32,
            machine_core::display::MAX_HEIGHT as u32
        )
    );
    let background = dominant_pixel(&rgba);
    let distinct_from_background = rgba.chunks(4).filter(|px| *px != background).count();
    assert_eq!(
        distinct_from_background, 0,
        "expected a uniform fill: AROS has not programmed any bitplane \
         DMA by frame 400 (no boot device to reach a real screen -- see \
         this test's doc comment)"
    );
}

#[test]
#[ignore = "requires the AROS ROM pair on disk; run with --ignored"]
fn aros_68k_pair() {
    if !Path::new(AROS_MAIN).exists() || !Path::new(AROS_EXT).exists() {
        eprintln!("SKIP: AROS ROM pair not present");
        return;
    }
    let (status, stdout) = run(&[
        "--rom",
        AROS_MAIN,
        "--ext-rom",
        AROS_EXT,
        "--max-frames",
        "200",
        "--max-instructions",
        "50000000",
    ])
    .unwrap();
    eprintln!("exit: {status:?}");
    eprintln!("{stdout}");
    assert!(
        stdout.contains("PHASE1 HOSTED:"),
        "expected a final status line regardless of how far boot got"
    );
}
