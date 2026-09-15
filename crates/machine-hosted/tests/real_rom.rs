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

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

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

/// The `rtgboard` P96 `.card` driver's own patched HDF -- a *different*
/// image from `hd_image()` above (that one is built against the Cirrus/
/// Graffity path). Produced from the same `amibake` base by
/// `scripts/build-rtgboard-card.sh` (builds `Libs/Picasso96/rtgboard.card`
/// from `m68k/rtgboard-card/`) followed by `scripts/patch-rtgboard-hdf.sh`
/// (installs the driver, clones the P96 monitor stub as
/// `Devs/Monitors/rtgboard`, removes the Graffity monitor pair, and writes
/// `Prefs/Env-Archive/Sys/ScreenMode.prefs` steering Workbench to
/// `DisplayID 0x60001102` / 16-bit) -- see `docs/rtgboard-protocol.md`.
fn rtgboard_hd_image() -> String {
    fixture("M68K_TEST_RTG_HDF", "m68k-machine-rtgboard.hdf")
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

/// The same ROMWack break-in as
/// `kickstart_3_2_2_a1200_romwack_break_in_reaches_the_debugger` above,
/// but driven over `--serial-tcp` from a real client socket rather than
/// from `--serial-script` -- the sharpest available proof that
/// host->guest bytes genuinely cross the wire this task's brief added,
/// not just that `SerialTcpBridge`'s own unit tests (`crate::serial_tcp`)
/// can talk to themselves over loopback.
///
/// The break-in poll (`docs/serial-debugging.md`) only samples `SERDATR`
/// six times, so hitting it needs a DEL already queued when the guest
/// polls, not sent afterward. Unlike `--serial-script`'s `SEND` (which is
/// paced deliberately, one byte per frame, entirely inside the emulator's
/// own frame-by-frame clock), a byte sent from this test's separate
/// process has to cross real wall-clock time to arrive -- so instead of
/// one precisely-timed send this test does the same thing the script
/// does for the same reason (`romwack-break-in.txt`'s own doc comment):
/// flood DEL continuously from the moment the socket connects, so the
/// guest's 32-byte receive queue is already sitting full of DEL well
/// before frame ~29 (where the break-in poll actually runs -- see the
/// other test's doc comment) regardless of exactly when that lands in
/// real time. This works because this crate has no wall-clock pacing at
/// all: `cargo test`'s debug build reaches frame 29 roughly 1.4s into the
/// run (measured from the sibling test's own "~20s for 400 frames" note),
/// which is ample time for a same-host TCP connection and a flooding
/// thread to be running well ahead of it.
#[test]
#[ignore = "requires a user-supplied Kickstart ROM on disk; run with --ignored"]
fn kickstart_3_2_2_a1200_romwack_break_in_reaches_the_debugger_over_tcp() {
    let rom = kickstart_a1200();
    if !have_fixtures(&[&rom]) {
        return;
    }

    let mut child = Command::new(env!("CARGO_BIN_EXE_machine-hosted"))
        .args([
            "--rom",
            &rom,
            "--trigger-illegal-after-frames",
            "20",
            "--serial-tcp",
            "127.0.0.1:0",
            "--max-frames",
            "500",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn machine-hosted");

    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));

    // `run.rs`'s own diag line for a bound `--serial-tcp` listener:
    // "host  | serial-tcp: listening on 127.0.0.1:<port> -- ...". Read
    // lines until it shows up rather than guessing a port ourselves --
    // `:0` above means the OS chose it.
    let mut addr = None;
    let mut lines = Vec::new();
    for _ in 0..200 {
        let mut line = String::new();
        let n = stdout.read_line(&mut line).expect("read child stdout");
        assert!(
            n > 0,
            "child exited before printing its --serial-tcp address"
        );
        let line = line.trim_end().to_string();
        if let Some(rest) = line.strip_prefix("host  | serial-tcp: listening on ") {
            let addr_str = rest.split(" --").next().unwrap_or(rest).trim();
            addr = Some(addr_str.to_string());
            lines.push(line);
            break;
        }
        lines.push(line);
    }
    let addr = addr.expect("never saw the --serial-tcp listening address in stdout");
    eprintln!("connecting to {addr}");

    let client = TcpStream::connect(&addr).expect("connect to --serial-tcp bridge");
    client
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    // Flood DEL from a dedicated thread for the same reason
    // `romwack-break-in.txt` floods it across 400 scripted frames -- see
    // this test's own doc comment. Stops itself once the main thread
    // below has seen the debugger banner, or after a generous ceiling so
    // a failed break-in doesn't leave a thread spinning forever.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flood_stop = std::sync::Arc::clone(&stop);
    let flooder = std::thread::spawn(move || {
        let mut flood_client = client;
        for _ in 0..20_000 {
            if flood_stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            if flood_client.write_all(&[0x7f]).is_err() {
                return; // guest side closed -- run ended
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });

    // Read the rest of the child's output until it exits (bounded by
    // `--max-frames 500` above regardless of whether the break-in
    // lands), collecting every line for the same assertions the
    // `--serial-script` sibling test makes.
    loop {
        let mut line = String::new();
        match stdout.read_line(&mut line) {
            Ok(0) => break, // EOF: child closed stdout (exiting)
            Ok(_) => lines.push(line.trim_end().to_string()),
            Err(_) => break,
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = flooder.join();
    let status = child.wait().expect("wait for machine-hosted");

    let stdout_text = lines.join("\n");
    eprintln!("exit: {status:?}");
    eprintln!("{stdout_text}");
    assert!(
        stdout_text.contains("GUEST | rom-wack"),
        "expected the ROM's own rom-wack debugger banner, reached over --serial-tcp"
    );
    assert!(
        stdout_text.contains("XCPT: 8000002F"),
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

/// End-to-end proof that `CHAR_DOWN`/`CHAR_UP` reach the guest through
/// `MapANSI()` (`docs/input-protocol.md` sec 15, `m68k/input-rom/
/// input-diagrom.s`'s `char_key_down`/`char_key_up`), not just that the
/// driver task stays alive while they're queued.
///
/// Drives the exact click chain `scripted_double_click_on_the_sys_icon_
/// opens_its_drawer` above already proves reaches Intuition -- SYS: ->
/// System -> Shell -- three double-clicks deep, then types `ECHO Hi` with
/// `TYPE` (7 characters, 14 `CHARDOWN`/`CHARUP` events -- comfortably
/// under `input::QUEUE_CAPACITY` (16), so nothing here exercises the
/// overflow path; a `TYPE` string long enough to overflow it was tried
/// while building this driver and silently lost its tail end, which is
/// real but separate behaviour, not this test's concern) and a real
/// (not `TYPE`-synthesised) `KEYDOWN`/`KEYUP 0x44` for Return -- proving
/// the plain `KEY_DOWN`/`KEY_UP` path in the same run, since AmigaDOS's
/// `Echo` only ever runs if that Return keystroke was real.
///
/// System's icon: canvas (213, 128) -> screen (170, 84), measured the
/// same way `scripted_double_click_on_the_sys_icon_opens_its_drawer`'s
/// own doc comment derives the SYS: icon's. Shell's icon, inside the
/// opened System drawer: canvas (224, 152) -> screen (192, 108).
///
/// The floor below is measured, not guessed, the same posture that
/// test's own comment insists on and for the identical reason: a naive
/// "something changed" assertion would also pass on a click that opened
/// the Shell but typed nothing, or opened the wrong window entirely.
/// Measured on this exact script: the Shell open with an empty prompt is
/// 13,925 non-background pixels; with `ECHO Hi` typed, executed, and its
/// `Hi` echoed back on the line under the prompt, it is 14,073. The floor
/// sits between the two, so "opened a shell but nothing was typed" (or a
/// character MapANSI silently failed to invert, leaving a blank line)
/// fails this test exactly as a wrong-window click would fail the SYS:
/// drawer test above.
#[test]
#[ignore = "requires a user-supplied Kickstart ROM and HD image on disk; run with --ignored"]
fn scripted_typing_in_a_shell_opened_three_double_clicks_deep_is_echoed() {
    let rom = kickstart_a1200();
    let hd = hd_image();
    if !have_fixtures(&[&rom, &hd]) {
        return;
    }

    let script_path =
        std::env::temp_dir().join(format!("machine-hosted-type-{}.input", std::process::id()));
    std::fs::write(
        &script_path,
        "SLEEP 4200\n\
         MOVE 42 73\nSLEEP 10\nBUTTONDOWN LEFT\nBUTTONUP LEFT\nSLEEP 5\nBUTTONDOWN LEFT\nBUTTONUP LEFT\nSLEEP 300\n\
         MOVE 170 84\nSLEEP 10\nBUTTONDOWN LEFT\nBUTTONUP LEFT\nSLEEP 5\nBUTTONDOWN LEFT\nBUTTONUP LEFT\nSLEEP 300\n\
         MOVE 192 108\nSLEEP 10\nBUTTONDOWN LEFT\nBUTTONUP LEFT\nSLEEP 5\nBUTTONDOWN LEFT\nBUTTONUP LEFT\nSLEEP 600\n\
         TYPE \"ECHO Hi\"\nSLEEP 30\nKEYDOWN 0x44\nSLEEP 2\nKEYUP 0x44\nSLEEP 300\n",
    )
    .expect("write input script");

    let path = screenshot_path("shell-type");
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
        "5700",
        "--max-frames",
        "5800",
        "--max-instructions",
        "900000000",
        "--inspect",
    ])
    .unwrap();
    eprintln!("exit: {status:?}");
    eprintln!("{stdout}");

    // Independent, non-visual confirmation alongside the screenshot below
    // (the same "two ways" posture the pointer-motion work used
    // IntuitionBase->MouseX/MouseY for): every one of the 14 CHARDOWN/
    // CHARUP events plus the Return key actually drained out of the
    // card's own queue, and none were dropped as an overflow.
    assert!(
        stdout.contains("EVENT_COUNT 0  EVENT_OVERFLOW 0"),
        "expected the input card's queue fully drained with no drops by the end of the run: {stdout}"
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
    let non_background = rgba.chunks(4).filter(|px| *px != background).count();
    assert!(
        non_background > 14_000,
        "expected 'ECHO Hi' typed, run, and its 'Hi' echoed in the opened Shell window, \
         got {non_background} non-background pixels -- 13,925 would mean the Shell opened \
         but nothing was typed (or MapANSI silently produced nothing); see this test's doc comment"
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

/// First light for the native `rtgboard` P96 `.card` driver
/// (`m68k/rtgboard-card/`, `docs/rtgboard-protocol.md`) -- the same claim
/// as `kickstart_3_2_2_a1200_rtg_workbench_desktop_is_grey_not_blank_or_
/// corrupt` above, but through a disk-loaded AUTOINIT `.card` driver
/// talking to the native `rtgboard` device (`--rtgboard`), not the
/// built-in Cirrus emulation (`--graphics`). Verified 2026-09-15.
///
/// The fixture is `scripts/patch-rtgboard-hdf.sh`'s output: it installs
/// `Libs/Picasso96/rtgboard.card`, replaces `Devs/Picasso96Settings`,
/// clones the P96 monitor stub as `Devs/Monitors/rtgboard` with a
/// `BOARDTYPE=rtgboard` icon, removes the Graffity monitor pair, and writes
/// `Prefs/Env-Archive/Sys/ScreenMode.prefs` steering Workbench to
/// `DisplayID 0x60001102` / 16-bit -- build it with
/// `scripts/build-rtgboard-card.sh` then `scripts/patch-rtgboard-hdf.sh`
/// if `M68K_TEST_RTG_HDF`/`nondistribution/m68k-machine-rtgboard.hdf` is
/// missing.
///
/// `--rtgboard-format rgb565` is not optional set-dressing: the flag
/// defaults to `rgbx8888`, and with the default the board's one-entry
/// catalog holds a format the driver never proposes to `GetCompatible
/// Formats`, so every `SetGC` the driver commits gets rejected (a live
/// instance of the format-catalog coupling `docs/rtgboard-protocol.md`
/// documents). This test asserts the serial log carries no
/// `"committed: REJECTED"` for exactly that reason -- a regression that
/// dropped the flag, or broke the catalog/proposal match some other way,
/// would still produce *a* screenshot (the planar fallback path), just not
/// the right one, which is why the pixel assertions below matter as much
/// as the marker-string ones.
///
/// Two known failure signatures shaped the thresholds here (both observed
/// live while bringing this driver up, not hypothesised):
/// - **`FakeNativeModes` CLUT trap**: without `ScreenMode.prefs` steering,
///   P96 offers Workbench an 8-bit CLUT mode; the driver (which only
///   speaks RGB565) renders 8-bit pen indices and the board scans them
///   back as RGB565, producing a black-dominant, green-tinted screen --
///   measured 21 distinct colours, only 13,637 non-background pixels.
/// - **wrong `DisplayID` / no steering at all**: no `SetGC`/`COMMIT` ever
///   happens and the screenshot is the unrelated 752x576 planar fallback
///   (caught here by the exact `(640, 480)` dimension assert, independent
///   of pixel content).
///
/// The real desktop this test pins measured 640x480, 6 distinct colours,
/// 31,906 non-background pixels, dominant background `[173, 170, 173,
/// 255]` -- mid-grey, but not *exactly* R==G==B: RGB565 packs 5 bits each
/// for R/B and 6 for G, so an 8-bit-per-gun grey that started life as
/// `0xAD` (173) loses its low bit going in and comes back `0xAA` (170) on
/// the 6-bit G channel while R/B round-trip exactly through their 5-bit
/// path -- a quantization artefact of the wire format, not a broken pixel.
/// Floors below sit between the real desktop and the CLUT-broken screen on
/// both axes.
#[test]
#[ignore = "requires a user-supplied Kickstart ROM and the patched rtgboard HDF on disk; run with --ignored"]
fn kickstart_3_2_2_a1200_workbench_renders_through_the_rtgboard_card_driver() {
    let rom = kickstart_a1200();
    let hd = rtgboard_hd_image();
    if !have_fixtures(&[&rom, &hd]) {
        return;
    }

    let path = screenshot_path("kickstart-rtgboard");
    let serial_log_path = std::env::temp_dir().join(format!(
        "machine-hosted-kickstart-rtgboard-{}.serial.log",
        std::process::id()
    ));

    let (status, stdout) = run(&[
        "--rom",
        &rom,
        "--hostblk",
        &hd,
        "--rtgboard",
        "640x480",
        "--rtgboard-format",
        "rgb565",
        "--max-frames",
        "5200",
        "--max-instructions",
        "3000000000",
        "--screenshot",
        path.to_str().unwrap(),
        "--screenshot-frame",
        "5000",
        "--serial-log",
        serial_log_path.to_str().unwrap(),
    ])
    .unwrap();
    eprintln!("exit: {status:?}");
    eprintln!("{stdout}");

    assert!(
        stdout.contains("screenshot: frame 5000"),
        "expected the capture to actually fire by frame 5000"
    );

    let serial_log = std::fs::read_to_string(&serial_log_path)
        .unwrap_or_else(|e| panic!("read serial log {serial_log_path:?}: {e}"));
    for marker in [
        "rtgboard: FindCard: board at",
        "rtgboard: InitCard: rtg.library version",
        "rtgboard: SetGC 640x480 committed: APPLIED",
        "rtgboard: SetPanning: offset 0x0 committed: APPLIED",
    ] {
        assert!(
            serial_log.contains(marker),
            "expected the driver's serial narration to contain {marker:?}; \
             see this test's doc comment for the full expected chain -- \
             serial log:\n{serial_log}"
        );
    }
    assert!(
        !serial_log.contains("committed: REJECTED"),
        "expected no rejected mode/panning commit -- a REJECTED commit here \
         usually means --rtgboard-format rgb565 was dropped or the catalog/ \
         proposal match broke; see this test's doc comment. serial log:\n{serial_log}"
    );

    let (width, height, rgba) = decode_png(&path);
    assert_eq!(
        (width, height),
        (640, 480),
        "an RTG screenshot's dimensions come from the driver-programmed \
         mode -- (752, 576) here would mean SetGC never committed and the \
         capture fell back to the planar path; see this test's doc comment"
    );

    let background = dominant_pixel(&rgba);
    // RGB565's 5-bit R/B vs 6-bit G channels mean an exact R==G==B grey
    // does not round-trip losslessly -- see this test's doc comment
    // (measured on this exact fixture: [173, 170, 173, 255]). A tolerance
    // band rather than exact equality still catches the CLUT trap, whose
    // background is black (near-zero, not merely off by a couple of
    // levels) with a green tint from stray pen-index bits.
    let channels = [background[0], background[1], background[2]];
    let max = *channels.iter().max().unwrap();
    let min = *channels.iter().min().unwrap();
    assert!(
        max - min <= 8,
        "expected a neutral-ish grey background (R/G/B within 8 of each \
         other, allowing for RGB565's 5/6/5-bit quantization), got \
         {background:?} -- see this test's doc comment for the CLUT-trap \
         failure mode this guards against"
    );
    assert!(
        (120..200).contains(&background[0]),
        "expected a mid-grey Workbench background, got {background:?} \
         (measured on this exact fixture: [173, 170, 173, 255])"
    );

    let non_background = rgba.chunks(4).filter(|px| *px != background).count();
    // Real desktop measured 31,906; the FakeNativeModes/CLUT-broken screen
    // measured 13,637 -- see this test's doc comment. The floor sits well
    // above the broken reading.
    assert!(
        non_background >= 20_000,
        "expected the real Workbench desktop through the rtgboard driver, \
         got {non_background} non-background pixels -- 13,637 would mean \
         the FakeNativeModes/CLUT trap fired instead; see this test's doc \
         comment"
    );

    let mut colours: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
    for px in rgba.chunks(4) {
        colours.insert(px);
    }
    // Real desktop measured 6 distinct colours; the CLUT-broken screen
    // measured 21 -- see this test's doc comment.
    assert!(
        colours.len() <= 10,
        "expected a clean few-colour Workbench desktop, got {} distinct \
         colours -- the CLUT-broken screen measured 21; see this test's \
         doc comment",
        colours.len()
    );

    let _ = std::fs::remove_file(&serial_log_path);
}

/// Every `(x, y)` in `rgba` (a `width`-wide RGBA8 image) whose pixel is
/// exact `[239, 69, 66, 255]` -- the red body colour of the Workbench
/// pointer P96 soft-renders into rtgboard VRAM (see this test's own doc
/// comment for why that colour, rather than the pointer's cream highlight
/// or black outline, is the discriminant used here).
fn red_pointer_pixels(width: u32, rgba: &[u8], expected_x: u32, expected_y: u32) -> Vec<(u32, u32)> {
    const RED: [u8; 4] = [239, 69, 66, 255];
    let mut found = Vec::new();
    for (i, px) in rgba.chunks(4).enumerate() {
        if *px == RED {
            let x = (i as u32) % width;
            let y = (i as u32) / width;
            found.push((x, y));
        }
    }
    assert!(
        found.len() >= 20,
        "expected at least 20 red pointer pixels near ({expected_x}, {expected_y}), \
         got {} -- see this test's doc comment (measured: exactly 31 in every capture)",
        found.len()
    );
    for &(x, y) in &found {
        assert!(
            (expected_x..=expected_x + 15).contains(&x) && (expected_y..=expected_y + 15).contains(&y),
            "expected every red pointer pixel within a 16x16 box at \
             ({expected_x}, {expected_y}), but found one at ({x}, {y}) -- this is exactly \
             the 'pointer stale-rendered somewhere else too' failure mode this helper \
             exists to catch; see this test's doc comment"
        );
    }
    found
}

/// `docs/rtgboard-protocol.md` §9's named combination: pointer motion and
/// clicks from the native input card working on a real Picasso96 RTG
/// screen driven by `rtgboard.card`, not the planar renderer or the
/// built-in Cirrus emulation. Verified manually 2026-09-15 by the
/// supervisor; every number in this test is measured from that exact run,
/// deterministic across runs except where noted below.
///
/// Why this combination is non-trivial: `rtgboard.card` declines a
/// hardware sprite (it sets `SoftSpriteFlags = RGBFF_R5G6B5` in its
/// `BoardInfo`, telling P96 "render the pointer image into my framebuffer
/// yourself"), so on this driver the mouse pointer only ever appears on
/// screen if P96's own soft-sprite path actually engages and writes real
/// pixels into rtgboard VRAM -- there is no hardware cursor plane to fall
/// back on. Separately, Intuition has to route each click to the window
/// under the pointer using *its own* idea of where the pointer is
/// (`IntuitionBase->MouseX`/`MouseY`, updated by the input driver resolving
/// `IntuitionBase->ActiveScreen` for each `IECLASS_NEWPOINTERPOS`/
/// `IESUBCLASS_PIXEL` event -- `docs/input-protocol.md` §13) -- on an RTG
/// screen this is a different code path from the planar screens the other
/// scripted-click tests in this file exercise. This test is the first to
/// exercise both at once.
///
/// This distinguishes two failure modes that a plain "the drawer opened"
/// or "some pixels changed" assertion would miss:
/// - **(a) no pointer at all**: the `SoftSpriteFlags`/soft-sprite path
///   silently fails to engage (e.g. the driver's declared format doesn't
///   match what P96 tries to render into) -- zero red pixels anywhere on
///   screen, even though clicks might still happen to land correctly.
/// - **(b) pointer renders but clicks land elsewhere**: the soft-sprite
///   pointer follows `MOVE` correctly (proving pointer-motion plumbing
///   alone works) but Intuition's click routing uses stale or wrong
///   coordinates -- the SYS drawer never opens even though the pointer
///   visibly reached the icon.
///
/// Input script and timeline (frames): Workbench on the RTG screen is up
/// well before frame 5100 (the sibling
/// `kickstart_3_2_2_a1200_workbench_renders_through_the_rtgboard_card_
/// driver` test above captures its desktop at frame 5000). `MOVE 100 100`
/// fires at frame 5100; `MOVE 500 380` at 5250; `MOVE 42 73` (the SYS
/// icon -- the same canvas position the planar double-click test above
/// uses, since the icon sits at the same place on this 640x480 desktop)
/// at 5450; the double-click follows at 5460-5466. The SYS drawer window
/// is fully open within ~35 frames of the click (measured directly: it
/// was already open by frame 5450 in an earlier, mistimed run of this
/// scenario), so the third capture at frame 5640 sees it settled.
///
/// `--screenshot-every 220` alongside `--screenshot-frame 5200` produces
/// three captures at frames 5200, 5420 and 5640 (`screenshot.rs`'s
/// `sequence_path`: the base name gets `-NNNNNN` inserted before its
/// extension), landing respectively just after the first move, just after
/// the second move, and just after the double-click has had time to open
/// the drawer.
///
/// Pointer evidence (assertion 6, via [`red_pointer_pixels`]): the
/// Workbench pointer soft-rendered into rtgboard VRAM is 57 pixels on this
/// fixture -- 31 exact `[239, 69, 66, 255]` (red body), 13 exact
/// `[239, 239, 206, 255]` (cream highlight), 13 black -- and red occurs
/// nowhere else on this desktop, making it the discriminant. Measured
/// bounding boxes put the pointer's hotspot (its top-left tip) at exactly
/// the commanded coordinate: capture 1 (100, 100) has all red pixels
/// within x 100..110, y 100..110; capture 2 (500, 380) within x 500..510,
/// y 380..390; capture 3 (42, 73) within x 42..52, y 73..83.
///
/// Click evidence (assertion 7): exact-white (`[255, 255, 255, 255]`) and
/// exact-black (`[0, 0, 0, 255]`) pixel counts on capture 3. The closed
/// desktop measures white 9,093 / black 6,599; with the SYS drawer open
/// (title bar, borders, six drawer icons with labels) measured white
/// 13,181 / black 11,221 in the pinned run and 13,121 / 11,281 in a second
/// run of the same scenario (~60px run-to-run variation, hence the margin
/// below the lower measurement). Floors of white >= 11,000 and
/// black >= 9,500 sit well above the closed-desktop reading and well below
/// both open-drawer readings, so the RELATIVEMOUSE-trap failure mode
/// (`docs/input-protocol.md`: clicks landing at the screen's top-left
/// corner regardless of pointer position) -- pointer moves correctly but
/// the click opens nothing -- fails loudly rather than passing on a
/// "something changed" floor. Capture 1's white count is also asserted
/// below the closed-desktop measurement, as a control showing these floors
/// genuinely discriminate within this same run rather than always passing.
///
/// Also asserted (assertion 3): `MouseX 42  MouseY 73` (two spaces) shows
/// up verbatim in `--inspect`'s stdout narration. On a planar hires
/// screen `docs/input-protocol.md` §13 documents a legacy 2x-on-hires
/// doubling of `MouseY`; this asserts that does NOT happen here -- an RTG
/// screen reads `MouseY` back exactly as requested -- so a regression that
/// started doubling it on RTG screens too would be caught.
#[test]
#[ignore = "requires a user-supplied Kickstart ROM and the patched rtgboard HDF on disk; run with --ignored"]
fn scripted_pointer_and_double_click_work_on_the_rtgboard_rtg_screen() {
    let rom = kickstart_a1200();
    let hd = rtgboard_hd_image();
    if !have_fixtures(&[&rom, &hd]) {
        return;
    }

    let script_path = std::env::temp_dir().join(format!(
        "machine-hosted-rtgboard-pointer-{}.input",
        std::process::id()
    ));
    std::fs::write(
        &script_path,
        "SLEEP 5100\n\
         MOVE 100 100\nSLEEP 150\n\
         MOVE 500 380\nSLEEP 200\n\
         MOVE 42 73\nSLEEP 10\n\
         BUTTONDOWN LEFT\nBUTTONUP LEFT\nSLEEP 5\n\
         BUTTONDOWN LEFT\nBUTTONUP LEFT\nSLEEP 300\n",
    )
    .expect("write input script");

    let serial_log_path = std::env::temp_dir().join(format!(
        "machine-hosted-rtgboard-pointer-{}.serial.log",
        std::process::id()
    ));

    let base_path = screenshot_path("rtgboard-pointer");
    let (status, stdout) = run(&[
        "--rom",
        &rom,
        "--hostblk",
        &hd,
        "--rtgboard",
        "640x480",
        "--rtgboard-format",
        "rgb565",
        "--input-script",
        script_path.to_str().unwrap(),
        "--screenshot",
        base_path.to_str().unwrap(),
        "--screenshot-frame",
        "5200",
        "--screenshot-every",
        "220",
        "--max-frames",
        "5700",
        "--max-instructions",
        "4200000000",
        "--serial-log",
        serial_log_path.to_str().unwrap(),
        "--inspect",
    ])
    .unwrap();
    eprintln!("exit: {status:?}");
    eprintln!("{stdout}");

    // Assertion 1: the last of the three captures actually fired.
    assert!(
        stdout.contains("screenshot: frame 5640"),
        "expected the final --screenshot-every capture to actually fire by frame 5640"
    );

    // Assertion 2: the same driver-narration markers the sibling rtgboard
    // test above pins, plus no rejected commit.
    let serial_log = std::fs::read_to_string(&serial_log_path)
        .unwrap_or_else(|e| panic!("read serial log {serial_log_path:?}: {e}"));
    for marker in [
        "rtgboard: FindCard: board at",
        "rtgboard: InitCard: rtg.library version",
        "rtgboard: SetGC 640x480 committed: APPLIED",
        "rtgboard: SetPanning: offset 0x0 committed: APPLIED",
    ] {
        assert!(
            serial_log.contains(marker),
            "expected the driver's serial narration to contain {marker:?}; \
             serial log:\n{serial_log}"
        );
    }
    assert!(
        !serial_log.contains("committed: REJECTED"),
        "expected no rejected mode/panning commit; serial log:\n{serial_log}"
    );

    // Assertion 3: MouseY reads back un-doubled on this RTG screen -- see
    // this test's doc comment.
    assert!(
        stdout.contains("MouseX 42  MouseY 73"),
        "expected IntuitionBase->MouseX/MouseY to read back exactly (42, 73) \
         on this RTG screen, un-doubled -- see this test's doc comment: {stdout}"
    );

    // Assertion 4: the input card's queue fully drained with nothing
    // dropped as an overflow.
    assert!(
        stdout.contains("EVENT_COUNT 0  EVENT_OVERFLOW 0"),
        "expected the input card's queue fully drained with no drops by the end of the run: {stdout}"
    );

    // Assertion 5: all three captures decode to exactly 640x480.
    let capture1 = base_path.with_file_name(format!(
        "{}-005200.png",
        base_path.file_stem().unwrap().to_string_lossy()
    ));
    let capture2 = base_path.with_file_name(format!(
        "{}-005420.png",
        base_path.file_stem().unwrap().to_string_lossy()
    ));
    let capture3 = base_path.with_file_name(format!(
        "{}-005640.png",
        base_path.file_stem().unwrap().to_string_lossy()
    ));

    let (width1, height1, rgba1) = decode_png(&capture1);
    let (width2, height2, rgba2) = decode_png(&capture2);
    let (width3, height3, rgba3) = decode_png(&capture3);
    for (width, height) in [(width1, height1), (width2, height2), (width3, height3)] {
        assert_eq!(
            (width, height),
            (640, 480),
            "an RTG screenshot's dimensions come from the driver-programmed mode"
        );
    }

    // Assertion 6: pointer evidence on all three captures -- see
    // [`red_pointer_pixels`] and this test's doc comment for the measured
    // bounding boxes.
    red_pointer_pixels(width1, &rgba1, 100, 100);
    red_pointer_pixels(width2, &rgba2, 500, 380);
    red_pointer_pixels(width3, &rgba3, 42, 73);

    // Assertion 7: click evidence -- the SYS drawer's white/black
    // furniture on capture 3, plus capture 1 as a closed-desktop control.
    // See this test's doc comment for the measured floors and margins.
    const WHITE: [u8; 4] = [255, 255, 255, 255];
    const BLACK: [u8; 4] = [0, 0, 0, 255];
    let white3 = rgba3.chunks(4).filter(|px| *px == WHITE).count();
    let black3 = rgba3.chunks(4).filter(|px| *px == BLACK).count();
    assert!(
        white3 >= 11_000,
        "expected the SYS drawer's white furniture on capture 3, got {white3} \
         white pixels -- 9,093 would mean the closed desktop (click opened \
         nothing); see this test's doc comment"
    );
    assert!(
        black3 >= 9_500,
        "expected the SYS drawer's black furniture/text on capture 3, got \
         {black3} black pixels -- 6,599 would mean the closed desktop (click \
         opened nothing); see this test's doc comment"
    );

    let white1 = rgba1.chunks(4).filter(|px| *px == WHITE).count();
    assert!(
        white1 < 11_000,
        "expected capture 1's closed-desktop white count (measured 9,093) to \
         sit below the open-drawer floor used above, as a control showing \
         that floor genuinely discriminates within this same run -- got \
         {white1}"
    );

    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&serial_log_path);
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
