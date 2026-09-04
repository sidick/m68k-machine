//! End-to-end smoke test: drives the compiled `machine-hosted` binary
//! (not the library internals) over a tiny synthetic ROM, so it exercises
//! CLI parsing, ROM loading, the run loop, and exit-code plumbing exactly
//! the way CI will invoke it. Not dependent on a real Kickstart -- see
//! `real_rom.rs` for that, which skips when no ROM is available.

use std::fs;
use std::process::Command;

use machine_core::{CHIP_RAM_SIZE, ROM_BASE, ROM_WINDOW_SIZE};

const MOVEQ_42_D0: u16 = 0x7000 | 42; // MOVEQ #42,D0
const NOP: u16 = 0x4E71;
const STOP: u16 = 0x4E72;
// Supervisor mode, all interrupt levels masked: nothing in this synthetic
// machine will ever wake it, so STOP is a deliberate, permanent halt --
// exactly the "guest signals it is done" case `machine-hosted` treats as
// a clean run (exit 0).
const STOP_SR: u16 = 0x2700;

/// Build a 512 KB Kickstart-shaped ROM image: reset vectors at the start,
/// then `MOVEQ #42,D0`, `NOP`, `STOP #$2700`.
fn synthetic_rom() -> Vec<u8> {
    let mut rom = vec![0u8; ROM_WINDOW_SIZE];
    let initial_ssp: u32 = CHIP_RAM_SIZE as u32;
    let program_addr: u32 = ROM_BASE + 8;
    rom[0..4].copy_from_slice(&initial_ssp.to_be_bytes());
    rom[4..8].copy_from_slice(&program_addr.to_be_bytes());
    rom[8..10].copy_from_slice(&MOVEQ_42_D0.to_be_bytes());
    rom[10..12].copy_from_slice(&NOP.to_be_bytes());
    rom[12..14].copy_from_slice(&STOP.to_be_bytes());
    rom[14..16].copy_from_slice(&STOP_SR.to_be_bytes());
    rom
}

#[test]
fn synthetic_rom_boots_and_halts_cleanly() {
    let rom_path = std::env::temp_dir().join(format!(
        "machine-hosted-smoke-{}-{}.rom",
        std::process::id(),
        line!()
    ));
    fs::write(&rom_path, synthetic_rom()).expect("write synthetic ROM");

    let output = Command::new(env!("CARGO_BIN_EXE_machine-hosted"))
        .arg("--rom")
        .arg(&rom_path)
        .arg("--max-frames")
        .arg("10")
        .arg("--max-instructions")
        .arg("1000")
        .output()
        .expect("run machine-hosted");

    let _ = fs::remove_file(&rom_path);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "expected exit 0 for a clean guest halt, got {:?}\nstdout:\n{stdout}",
        output.status.code()
    );
    assert!(
        stdout.contains("PHASE1 HOSTED: CPU HALTED CLEANLY"),
        "missing expected status line, stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("unidentified"),
        "synthetic ROM has no valid header, so identify() should report it as \
         unidentified rather than fabricate a match; stdout:\n{stdout}"
    );
}
