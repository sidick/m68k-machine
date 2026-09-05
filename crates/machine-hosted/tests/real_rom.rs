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

/// Resolve a fixture this repo may not carry: an environment variable if
/// the caller sets one, otherwise `nondistribution/` at the repo root.
///
/// These used to be absolute paths into a session-scoped temporary
/// directory. That directory eventually disappeared, and because every
/// test here skips when its fixture is missing, they went on reporting
/// success while exercising nothing at all. Resolving relative to
/// `CARGO_MANIFEST_DIR` keeps the fixtures somewhere stable, so absence
/// again means "this machine has no media" rather than "the path rotted".
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

/// Kickstart 3.2.2 for the A1200. Cloanto's, so it cannot live in this
/// repo -- see `nondistribution/README.md`.
fn kickstart_a1200() -> String {
    fixture("M68K_TEST_KICKSTART", "A1200.47.115.rom")
}

/// A bootable AmigaOS 3.2.2 HDF (RDB, one `DH0` FFS partition) with
/// Picasso96 installed against the Graffity card, built by `tools/amibake`
/// from licensed media -- see `docs/storage.md` for how it was built and
/// why, like the Kickstart ROM above, it cannot live in this repo.
///
/// One image serves both the planar and the RTG test: they differ only by
/// whether the runner is given `--graphics`, not by their media.
fn hd_image() -> String {
    fixture("M68K_TEST_HDF", "m68k-machine.hdf")
}

/// Skip the calling test unless every fixture it needs is present, so a
/// clean clone with no licensed media still goes green.
fn have_fixtures(paths: &[&str]) -> bool {
    for path in paths {
        if !Path::new(path).exists() {
            eprintln!("SKIP: {path} not present -- see nondistribution/README.md");
            return false;
        }
    }
    true
}

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
/// a real Kickstart capture -- a since-fixed sprite-rendering bug drew a
/// bogus shape with its corner exactly at `(0,0)` in this machine's
/// DIW-relative coordinates. The dominant colour by pixel count is what
/// "background" actually means here.
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
    let rom = kickstart_a1200();
    if !have_fixtures(&[&rom]) {
        return;
    }
    let (status, stdout) = run(&[
        "--rom",
        &rom,
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
    let rom = kickstart_a1200();
    if !have_fixtures(&[&rom]) {
        return;
    }
    let (status, stdout) = run(&[
        "--rom",
        &rom,
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
    let rom = kickstart_a1200();
    if !have_fixtures(&[&rom]) {
        return;
    }
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/serial-scripts/romwack-break-in.txt"
    );
    let (status, stdout) = run(&[
        "--rom",
        &rom,
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
/// **Corrected finding** (this test originally asserted the opposite --
/// see the git history and `docs/screenshots.md` for the full account).
/// `--inspect`'s `display state` line shows a real, non-zero `COP1LC`
/// (`0x1910`), but `BPLCON0`'s plane-count field is 0 and `BPL1PT` is
/// null: Kickstart has started a copper list but never enabled any
/// bitplane DMA, because this machine has no boot device yet (no MIRAGE
/// storage -- roadmap Phase 3) and `intuition.library` never opens a
/// real screen. A first attempt at this test found what looked like real
/// content -- a mouse pointer followed by ~239 rows of unrelated
/// glyph-like noise -- but that was a renderer bug, not guest output:
/// `draw_sprite0` was reading sprite 0's height from the chipset's
/// `SPR0CTL` register, which this ROM's copper list never writes (real
/// hardware's sprite DMA loads it autonomously from the sprite list in
/// chip RAM instead), so it read back a stale, unrelated value that
/// decoded to a ~255-line sprite instead of the real header's decoded
/// zero-height (disabled) sprite. Fixed in `render.rs`'s `draw_sprite0`
/// (reads the position/control header from `SPR0PT`/`SPR0PT+2` in chip
/// RAM, matching real sprite-DMA fetch semantics) and pinned by
/// `render.rs`'s own
/// `sprite_height_comes_from_the_real_header_in_ram_not_the_stale_ctl_register`
/// unit test.
///
/// **Corrected finding.** This test previously asserted a uniform fill,
/// which was true only because the machine could not yet get this far.
/// Once CIA one-shot timers started on the timer-high write, strap woke
/// and built the real no-boot-media screen, and the renderer learned to
/// follow COPJMP2 into the second copper list where that screen lives.
/// Kickstart now draws its boot picture with no boot device at all: the
/// checkered ball, the Hyperion banner and the floppy graphic, on a
/// 4-plane hires screen. So this asserts drawn content rather than a
/// specific picture -- pixel-exact matching would break on any legitimate
/// palette or layout change.
/// **Gayle IDE's retirement (`docs/device-ledger.md`) restored the fast
/// path this comment used to describe as history.** While Gayle was
/// attached, Kickstart found an IDE port, probed it, and waited out the
/// standard ~30 second (~1500 frame) timeout before concluding there was
/// no drive, so this test's capture frame had to be pushed well past
/// that. With Gayle gone the ID register is open bus again, Kickstart
/// concludes there is no such interface at all, and the screen reappears
/// at essentially the same frame it did before Gayle ever existed
/// (confirmed directly: distinct content is on screen by frame 200).
/// Frame 2500 is kept anyway rather than reverted to 200, for two
/// reasons found while re-verifying this test after Gayle's removal:
/// first, the picture briefly cycles between two close variants (9 vs 6
/// distinct colours) in the first few hundred frames -- plausibly a
/// blinking element in the boot picture -- and only settles for good
/// once idle; second, without an IDE probe's busy-wait spending cycles,
/// Kickstart's post-boot idle loop turns out to cost noticeably more
/// *instructions* per frame than it used to (confirmed: this fixture
/// needed roughly 34,000 instructions/frame before Gayle's removal and
/// roughly 91,000/frame after), so `--max-instructions` below is raised
/// well past the old value to still comfortably clear frame 2500. Frame
/// 2500 is also still clear of `--max-frames`'s own boundary (see the
/// AROS test for why that matters: a capture taken at the run's final
/// frame can observe a register mid-write).
#[test]
#[ignore = "requires a user-supplied Kickstart ROM on disk; run with --ignored"]
fn kickstart_3_2_2_a1200_screenshot_shows_the_boot_screen() {
    let rom = kickstart_a1200();
    if !have_fixtures(&[&rom]) {
        return;
    }
    let path = screenshot_path("kickstart");
    let (status, stdout) = run(&[
        "--rom",
        &rom,
        "--max-frames",
        "2600",
        "--max-instructions",
        "400000000",
        "--screenshot",
        path.to_str().unwrap(),
        "--screenshot-frame",
        "2500",
    ])
    .unwrap();
    eprintln!("exit: {status:?}");
    eprintln!("{stdout}");
    assert!(
        stdout.contains("screenshot: frame 2500"),
        "expected the capture to actually fire by frame 2500"
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
        non_background > 2000,
        "expected Kickstart's boot picture to be drawn, got {non_background} \
         non-background pixels -- see this test's doc comment"
    );

    let mut colours: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
    for px in rgba.chunks(4) {
        colours.insert(px);
    }
    assert!(
        colours.len() >= 4,
        "expected a multi-colour picture from a 4-plane screen, got {} distinct colours",
        colours.len()
    );
}

/// The AROS counterpart of the Kickstart test above -- and a **corrected
/// finding** (this test originally asserted a flat fill; see the git
/// history). AROS does reach a real display with no boot device at all:
/// by frame ~400 its `dosboot.resource` has put up the "Waiting for
/// bootable media" screen -- the cat-eyes boot logo on a 4-plane hires
/// interlaced screen programmed through a copper list (`COP2LC` sets the
/// full 16-colour palette, `BPLCON0 = $C204`, `DDFSTRT/STOP = $3C/$D0`,
/// `BPL1MOD/BPL2MOD = $50`, and four bitplane pointers), then parks the
/// whole system in `Wait` -- the same idle state the Copperline oracle
/// shows at its logo (both CPUs stopped at the identical exec idle PC,
/// `$00FE8B88`). The earlier flat-fill reading came from inspecting only
/// the raw `BPLCON0`/`BPL1PT` registers, which the *copper list* (not the
/// CPU) programs each frame.
///
/// So this asserts drawn content, not a specific picture: a healthy
/// capture has the logo palette's many distinct colours and thousands of
/// non-background pixels. Chip-RAM comparison against the Copperline
/// oracle (byte-identical bitplanes up to boot-animation timing) shows
/// the guest populates the frame correctly; the capture's exact pixels
/// additionally depend on the stop-gap renderer's DDF model, so the
/// thresholds here are deliberately loose enough to hold both before and
/// after renderer-side DDF fixes, while still failing hard on the old
/// "nothing drawn at all" reading. `--screenshot-frame 400` stays safely
/// clear of `--max-frames 500`'s boundary (a register can be caught
/// mid-update in the single frame right at that edge -- observed directly
/// while preparing the original test).
#[test]
fn aros_68k_screenshot_shows_boot_screen_content_without_boot_media() {
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
    assert!(
        distinct_from_background > 2_000,
        "expected the AROS boot-logo screen to draw real content \
         (got {distinct_from_background} non-background pixels) -- see \
         this test's doc comment"
    );
    let distinct_colours = rgba
        .chunks(4)
        .map(|px| <[u8; 4]>::try_from(px).unwrap())
        .collect::<std::collections::HashSet<_>>()
        .len();
    assert!(
        distinct_colours >= 8,
        "expected the boot logo's 16-colour palette to show up as many \
         distinct colours (got {distinct_colours})"
    );
}

/// Phase 3 regression test (this task's brief, `docs/storage.md`): attach
/// the file-backed `BlockDevice` (`crate::hd_image::FileBlockDevice`,
/// wired via `--hostblk`) to `hostblk` unit 0 and drive a real Kickstart
/// all the way through disk boot.
///
/// **This test originally drove the same boot through Gayle's IDE port
/// (`--hd`).** Gayle IDE is now retired (`docs/device-ledger.md`):
/// `hostblk` boots this identical image unaided, byte-identical to the
/// Gayle capture, and was soaked (`docs/hostblk-soak.md`) before the
/// switch. Only the attach flag changed; the assertions below (including
/// the exact non-background pixel count) are unchanged from the Gayle
/// version of this test.
///
/// **What actually happens, confirmed by inspecting the captured PNG
/// (not just pixel-difference counts):** the RDB is read, `DH0:` mounts,
/// FFS loads it, `Startup-Sequence` runs, and Intuition opens a real
/// Workbench screen with a titled window and the `RAM Disk` and `SYS`
/// icons drawn. The whole storage path -- discovery, RDB, partition,
/// filesystem, DOS, Workbench -- has to work to get here.
///
/// This test previously asserted that boot ended at Intuition's "Please
/// insert volume DF0: in any drive" System Request, which an earlier
/// image raised from an `AddBuffers >NIL: DF0: 15` line in its
/// `Startup-Sequence` -- an ordinary line referencing a drive this
/// machine, like a real A1200 with no floppy fitted, does not have.
/// `tools/amibake` no longer emits that line, so the requester is gone
/// and the desktop is reached directly. The assertions below describe the
/// desktop; the requester was never the interesting part, only the
/// furthest point boot happened to reach.
///
/// Frame timing: unlike the no-disk boot-screen test above (stable by
/// frame 2500), a real disk boot keeps Kickstart busy well past the
/// discovery window into actual filesystem and Workbench work. Sweeping
/// `--screenshot-every` while preparing this test found the desktop
/// already fully drawn and pixel-stable from frame 4000 onward (no change
/// at all through 5500); frame 4000 is used here with `--max-frames 4500`
/// for margin clear of the capture boundary (same reasoning as the AROS
/// test's frame-400-of-500 margin).
/// Input reaches Intuition end to end: a scripted double-click on the
/// `SYS` icon opens its drawer.
///
/// This exercises what a pointer move alone cannot -- button events, the
/// full-held-state `ie_Qualifier` rule, and `IEQUALIFIER_RELATIVEMOUSE`
/// on synthetic `RAWMOUSE` events, which if omitted makes every click
/// land at the screen's top-left corner regardless of pointer position
/// (`docs/input-protocol.md`). A drawer opening is unambiguous and only
/// reachable through Intuition and Workbench.
///
/// Coordinates are Intuition *screen* coordinates, which are not canvas
/// pixels: this Workbench is hires, so `canvas_x = screen_x / 2 + 128`
/// and `canvas_y = screen_y + 44`, derived by moving the pointer to two
/// known positions and measuring where the sprite landed. The `SYS` icon
/// sits at canvas (149, 117), hence screen (42, 73).
#[test]
#[ignore = "requires a user-supplied Kickstart ROM and HD image on disk; run with --ignored"]
fn scripted_double_click_on_the_sys_icon_opens_its_drawer() {
    let rom = kickstart_a1200();
    let hd = hd_image();
    if !have_fixtures(&[&rom, &hd]) {
        return;
    }

    let script_path =
        std::env::temp_dir().join(format!("machine-hosted-click-{}.input", std::process::id()));
    std::fs::write(
        &script_path,
        "SLEEP 4200\nMOVE 42 73\nSLEEP 10\nBUTTONDOWN LEFT\nBUTTONUP LEFT\n\
         SLEEP 5\nBUTTONDOWN LEFT\nBUTTONUP LEFT\nSLEEP 300\n",
    )
    .expect("write input script");

    let path = screenshot_path("sys-drawer");
    let (status, stdout) = run(&[
        "--rom",
        &rom,
        "--hostblk",
        &hd,
        "--input-script",
        script_path.to_str().unwrap(),
        "--screenshot",
        path.to_str().unwrap(),
        "--screenshot-frame",
        "4510",
        "--max-frames",
        "4650",
        "--max-instructions",
        "600000000",
    ])
    .unwrap();
    eprintln!("exit: {status:?}");
    eprintln!("{stdout}");

    let (width, height, rgba) = decode_png(&path);
    assert_eq!(
        (width, height),
        (
            machine_core::display::MAX_WIDTH as u32,
            machine_core::display::MAX_HEIGHT as u32
        )
    );

    let background = dominant_pixel(&rgba);
    let non_background = rgba.chunks(4).filter(|px| *px != background).count();
    // The desktop alone is 13,507 and the desktop plus a parked pointer
    // is 13,564 (both measured). An opened drawer -- its window, title,
    // border and six icons with labels -- measured 15,241. The floor sits
    // well above "the pointer moved but nothing opened", which is exactly
    // the failure this test exists to catch: a click that reaches
    // Intuition but lands in the wrong place still moves the pointer.
    assert!(
        non_background > 14_500,
        "expected the SYS drawer window opened by a scripted double-click,          got {non_background} non-background pixels -- 13,564 would mean the          pointer moved but the click opened nothing; see this test's doc comment"
    );

    let _ = std::fs::remove_file(&script_path);
}

#[test]
#[ignore = "requires a user-supplied Kickstart ROM and HD image on disk; run with --ignored"]
fn kickstart_3_2_2_a1200_boots_from_hd_to_the_workbench_desktop() {
    let rom = kickstart_a1200();
    if !have_fixtures(&[&rom]) {
        return;
    }
    let hd = hd_image();
    if !have_fixtures(&[&hd]) {
        return;
    }
    let path = screenshot_path("kickstart-hd");
    let (status, stdout) = run(&[
        "--rom",
        &rom,
        "--hostblk",
        &hd,
        "--max-frames",
        "4500",
        "--max-instructions",
        "300000000",
        "--screenshot",
        path.to_str().unwrap(),
        "--screenshot-frame",
        "4000",
    ])
    .unwrap();
    eprintln!("exit: {status:?}");
    eprintln!("{stdout}");

    assert!(
        stdout.contains("hostblk:") && stdout.contains("read-only"),
        "expected the runner to report attaching the image read-only by default"
    );
    assert!(
        stdout.contains("screenshot: frame 4000"),
        "expected the capture to actually fire by frame 4000"
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
    // A specific, meaningful floor rather than "some pixels changed": the
    // Workbench screen's title bar, window border and furniture, and the
    // two icons with their labels account for a stable 13,507
    // non-background pixels on this exact image (see this test's doc
    // comment). Comfortably below that would mean something much smaller
    // than a whole desktop drew -- an empty screen, or a bare backdrop
    // with no window on it.
    assert!(
        non_background > 8_000,
        "expected a drawn Workbench desktop -- title bar, window and the \
         RAM Disk and SYS icons -- got {non_background} non-background \
         pixels; see this test's doc comment"
    );

    let mut colours: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
    for px in rgba.chunks(4) {
        colours.insert(px);
    }
    assert!(
        colours.len() >= 4,
        "expected the desktop's title bar, window borders, gadgets, icons \
         and text to show up as several distinct colours over the \
         Workbench grey, got {} distinct colours",
        colours.len()
    );
}

/// RTG regression test: pins the Workbench desktop Picasso96's Cirrus
/// driver paints once it brings up a real 640x480 8bpp RTG screen on the
/// Graffity card (`--graphics`). Before this test there was no coverage at
/// all of the RTG present path (`render_rtg` in `render.rs`, fed by
/// `Cirrus542x::palette_argb` in `cirrus.rs`), so a future change to
/// either could silently blank the desktop or corrupt its palette and
/// nothing would catch it.
///
/// Structure only, not exact pixels -- window furniture, gadget layout and
/// icon placement are free to change with any legitimate driver or asset
/// change. What must always hold for a healthy RTG Workbench desktop:
/// the driver-programmed mode is exactly 640x480 (`decoded_mode`'s
/// geometry, independent of the planar renderer's fixed
/// `MAX_WIDTH`/`MAX_HEIGHT` canvas the non-`--graphics` tests above use);
/// several distinct colours are on screen (a flat single-colour fill is
/// what a blanked or mis-painted screen looks like); and each screen
/// quadrant has some non-background content, since a real desktop has
/// window furniture and icons spread across it rather than bunched in one
/// corner. Frame 5000 (`--max-frames 5200` for margin) is used because
/// `--graphics` runs need many more instructions to reach a stable
/// desktop than the planar boot screen above, and the default 200M
/// instruction cap stops the run around frame 1427 before the desktop is
/// even drawn -- hence the large `--max-instructions` here.
///
/// **Originally driven through `--hd` (Gayle IDE); switched to
/// `--hostblk` once Gayle retired (`docs/device-ledger.md`)** --
/// confirmed byte-identical to the Gayle capture before the switch.
#[test]
#[ignore = "requires a user-supplied Kickstart ROM and HD image on disk; run with --ignored"]
fn kickstart_3_2_2_a1200_rtg_workbench_desktop_is_grey_not_blank_or_corrupt() {
    let rom = kickstart_a1200();
    if !have_fixtures(&[&rom]) {
        return;
    }
    let hd = hd_image();
    if !have_fixtures(&[&hd]) {
        return;
    }
    let path = screenshot_path("kickstart-rtg");
    let (status, stdout) = run(&[
        "--rom",
        &rom,
        "--hostblk",
        &hd,
        "--graphics",
        "--max-frames",
        "5200",
        "--max-instructions",
        "3000000000",
        "--screenshot",
        path.to_str().unwrap(),
        "--screenshot-frame",
        "5000",
    ])
    .unwrap();
    eprintln!("exit: {status:?}");
    eprintln!("{stdout}");

    assert!(
        stdout.contains("screenshot: frame 5000"),
        "expected the capture to actually fire by frame 5000"
    );

    let (width, height, rgba) = decode_png(&path);
    assert_eq!(
        (width, height),
        (640, 480),
        "an RTG screenshot's dimensions come from the driver-programmed \
         mode, not the planar renderer's MAX_WIDTH/MAX_HEIGHT canvas"
    );

    let background = dominant_pixel(&rgba);
    // Documented hardware behaviour, not this implementation's own
    // assumption: the RAMDAC holds six bits per gun (VGA/Cirrus DAC
    // convention), so a genuine grey background pen has R, G and B
    // already equal *before* the renderer's 6-to-8-bit gun expansion --
    // replicating the top two bits (`palette_argb`'s `expand6`) preserves
    // that equality. A background where the channels differ can only mean
    // a non-grey colour landed in the background palette entry, which is
    // exactly this task's original defect (a hardware-cursor colour write
    // landing in the screen palette instead of the cursor's own table).
    assert_eq!(
        (background[0], background[1]),
        (background[1], background[2]),
        "expected a neutral grey background (equal R/G/B), got {background:?} -- \
         a tinted background means something other than the intended pen \
         landed in the background palette entry"
    );

    let non_background = rgba.chunks(4).filter(|px| *px != background).count();
    assert!(
        non_background > 5_000,
        "expected the desktop's window furniture, gadgets and icons to be \
         drawn, got only {non_background} non-background pixels"
    );

    let mut colours: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
    for px in rgba.chunks(4) {
        colours.insert(px);
    }
    assert!(
        colours.len() >= 3,
        "expected window furniture, text and icons to show up as several \
         distinct colours over the background, got {} distinct colours",
        colours.len()
    );

    // Each screen quadrant should have some non-background content: a
    // real desktop's furniture and icons spread across the screen rather
    // than clustering in one corner, which is what a partially-drawn or
    // mis-clipped screen would look like.
    let mut quadrant_has_content = [false; 4];
    for (i, px) in rgba.chunks(4).enumerate() {
        if *px == background {
            continue;
        }
        let x = (i as u32) % width;
        let y = (i as u32) / width;
        let qx = usize::from(x >= width / 2);
        let qy = usize::from(y >= height / 2);
        quadrant_has_content[qy * 2 + qx] = true;
    }
    assert!(
        quadrant_has_content.iter().all(|&has| has),
        "expected non-background content in every screen quadrant, got {quadrant_has_content:?}"
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
