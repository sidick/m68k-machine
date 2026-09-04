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
const AROS_MAIN: &str = "/Users/simond/src/external/Copperline/assets/aros/aros-amiga-m68k-rom.bin";
const AROS_EXT: &str = "/Users/simond/src/external/Copperline/assets/aros/aros-amiga-m68k-ext.bin";

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
