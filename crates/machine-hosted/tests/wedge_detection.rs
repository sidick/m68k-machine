//! Both-directions evidence for the wedge detector's structural fix
//! (`docs/wedge-detection.md`): a guest stuck in a small *multi-instruction*
//! loop that makes no progress must be caught, not just a guest spinning
//! on one PC. Drives the compiled `machine-hosted` binary (not the
//! library internals) over a tiny synthetic ROM, the same shape
//! `smoke.rs` uses.
//!
//! The loop here is deliberately the same shape as the real freeze this
//! replaces (`docs/wedge-detection.md`, commit `525dc1a`): several
//! instructions, several addresses, an unconditional branch back, no
//! `STOP`, and a data read that never changes anything the loop looks
//! at -- so it never terminates and never repeats one PC consecutively.
//! The pre-change detector (`same_pc_streak`, counting only consecutive
//! identical PCs) is structurally blind to this; see `docs/
//! wedge-detection.md`'s "both directions" section for the comparison
//! actually run against a temporarily restored copy of that code.

use std::fs;
use std::process::Command;

use machine_core::{CHIP_RAM_SIZE, ROM_BASE, ROM_WINDOW_SIZE};

/// `MOVE.W $00000000,D1` -- absolute-long-addressed read into D1. The
/// value read is never used for anything (there is no comparison against
/// it at all): this loop doesn't need to reproduce the real freeze's
/// `VHPOSR`-aliasing *cause*, only its *shape* -- a small loop that
/// genuinely never converges, cycling through more than one PC, which is
/// exactly the case the old single-PC-streak detector could not see. See
/// this file's own module doc comment.
const MOVE_W_ABS_D1: u16 = 0x3239;
const MOVE_W_ABS_D1_ADDR: u32 = 0x0000_0000;
const NOP: u16 = 0x4E71;
/// `BRA.S` with an 8-bit displacement of -10: branches back exactly to
/// the `MOVE.W` above (the loop body is 6 + 2 + 2 = 10 bytes: the
/// absolute-long `MOVE.W`'s opcode word plus its 4-byte address operand,
/// the `NOP`, and this branch itself). `0x00` and `0xFF` are both
/// reserved/edge displacement encodings on some 68k variants -- `-10`
/// avoids either ambiguity.
const BRA_S_BACK_10: u16 = 0x6000 | (0xF6u16);

/// Build a synthetic ROM whose guest program is an infinite three-
/// instruction loop -- no `STOP`, no way out -- spanning 10 bytes of code
/// (well inside `LOOP_WINDOW_SPAN_BYTES`), the same shape as the real
/// `VHPOSR` freeze this replaces.
fn synthetic_wedge_rom() -> Vec<u8> {
    let mut rom = vec![0u8; ROM_WINDOW_SIZE];
    let initial_ssp: u32 = CHIP_RAM_SIZE as u32;
    let program_addr: u32 = ROM_BASE + 8;
    rom[0..4].copy_from_slice(&initial_ssp.to_be_bytes());
    rom[4..8].copy_from_slice(&program_addr.to_be_bytes());

    let mut pc = 8usize;
    rom[pc..pc + 2].copy_from_slice(&MOVE_W_ABS_D1.to_be_bytes());
    pc += 2;
    rom[pc..pc + 4].copy_from_slice(&MOVE_W_ABS_D1_ADDR.to_be_bytes());
    pc += 4;
    rom[pc..pc + 2].copy_from_slice(&NOP.to_be_bytes());
    pc += 2;
    rom[pc..pc + 2].copy_from_slice(&BRA_S_BACK_10.to_be_bytes());

    rom
}

fn write_rom() -> std::path::PathBuf {
    let rom_path = std::env::temp_dir().join(format!(
        "machine-hosted-wedge-{}-{}.rom",
        std::process::id(),
        line!()
    ));
    fs::write(&rom_path, synthetic_wedge_rom()).expect("write synthetic ROM");
    rom_path
}

/// `--cpu-speed cycle` (the default): the three-instruction loop must be
/// caught as a wedge, well before the generous `--max-instructions`
/// fallback below (30,000,000, comfortably above `TIGHT_LOOP_THRESHOLD`'s
/// 20,000,000) -- proving the range-based detector fires on a loop that
/// never repeats one PC consecutively, which is exactly what the old
/// same-PC-streak detector could not do (see this file's module doc).
#[test]
fn cycle_mode_multi_instruction_loop_is_detected_as_a_wedge() {
    let rom_path = write_rom();

    let output = Command::new(env!("CARGO_BIN_EXE_machine-hosted"))
        .arg("--rom")
        .arg(&rom_path)
        .arg("--max-frames")
        .arg("1000000")
        .arg("--max-instructions")
        .arg("30000000")
        .output()
        .expect("run machine-hosted");

    let _ = fs::remove_file(&rom_path);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit 2 (Wedged), got {:?}\nstdout:\n{stdout}",
        output.status.code()
    );
    assert!(
        stdout.contains("WEDGED") && stdout.contains("tight loop in PC range"),
        "expected a range-based wedge report, stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("LIMIT REACHED"),
        "the loop should have been caught as a wedge before either limit, stdout:\n{stdout}"
    );
}

/// Same loop, `--cpu-speed fixed`: `run_guest_fixed` carries its own
/// independent `LoopWindow` (it does not share `run_guest`'s), so this is
/// not redundant with the cycle-mode test above -- it is the second of
/// the two per-instruction-hook run loops the fix applies to.
#[test]
fn fixed_mode_multi_instruction_loop_is_detected_as_a_wedge() {
    let rom_path = write_rom();

    let output = Command::new(env!("CARGO_BIN_EXE_machine-hosted"))
        .arg("--rom")
        .arg(&rom_path)
        .arg("--cpu-speed")
        .arg("fixed")
        .arg("--max-frames")
        .arg("1000000")
        .arg("--max-instructions")
        .arg("30000000")
        .output()
        .expect("run machine-hosted");

    let _ = fs::remove_file(&rom_path);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit 2 (Wedged), got {:?}\nstdout:\n{stdout}",
        output.status.code()
    );
    assert!(
        stdout.contains("WEDGED") && stdout.contains("tight loop in PC range"),
        "expected a range-based wedge report, stdout:\n{stdout}"
    );
}
