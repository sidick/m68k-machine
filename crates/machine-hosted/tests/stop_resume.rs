//! Regression test for the `STOP`-is-not-`HALT` bug (roadmap Phase 1):
//! `run_guest` used to treat `CycleBatchExit::Stopped` as always terminal,
//! which is wrong for any `STOP` that leaves an interrupt level unmasked
//! (SR mask != 7) -- exactly what Kickstart's Exec idle dispatcher does
//! (`STOP #$2000`). `m68k-rs`'s `run_for_cycles_with_hook` never calls the
//! per-instruction hook while the CPU is already stopped (its own doc
//! comment says so), so nothing ticks the bus or resamples the IPL for a
//! stopped CPU unless the host loop does it directly.
//!
//! This drives the compiled binary (not library internals, matching
//! `smoke.rs`) over a synthetic ROM that enables VERTB, executes
//! `STOP #$2000` (mask 0 -- open to every level this chipset can request),
//! and only reaches its own clean-halt `STOP #$2700` from the level-3
//! autovector handler. If the run terminates without ever entering that
//! handler, the bug has regressed: the CPU stayed parked forever because
//! the clock was never advanced while stopped.

use std::fs;
use std::process::Command;

use machine_core::{CHIP_RAM_SIZE, CUSTOM_BASE, ROM_BASE, ROM_WINDOW_SIZE};

const MOVE_W_IMM_ABS_L: u16 = 0x33FC; // MOVE.W #imm16,ABS.L
const MOVEQ_7_D0: u16 = 0x7000 | 7; // MOVEQ #7,D0
const STOP: u16 = 0x4E72;

// SETCLR(0x8000) | INTEN(0x4000) | VERTB(0x0020): master-enable interrupts
// and unmask VERTB specifically, matching `chipset::intbit`.
const INTENA_ENABLE_VERTB: u16 = 0x8000 | 0x4000 | 0x0020;
const INTENA_ADDR: u32 = CUSTOM_BASE + 0x09A;

// Mask 0: open to every interrupt level 1-6 -- what Kickstart's idle
// dispatcher uses, and the case that regressed.
const STOP_SR_WAIT_FOR_IRQ: u16 = 0x2000;
// Mask 7: nothing in this chipset can ever wake this -- the deliberate
// "guest is done" halt `tests/smoke.rs` also relies on.
const STOP_SR_HALT: u16 = 0x2700;

const MAIN_ENTRY_OFFSET: u32 = 0x40;
const HANDLER_OFFSET: u32 = 0x80;
// Level-3 autovector: vector number 24+3=27, offset 27*4.
const AUTOVECTOR_3_OFFSET: usize = 27 * 4;

/// Build a ROM that: enables VERTB, `STOP`s waiting for it, and -- only
/// once the level-3 autovector handler actually runs -- halts cleanly.
fn synthetic_rom() -> Vec<u8> {
    let mut rom = vec![0u8; ROM_WINDOW_SIZE];
    let initial_ssp: u32 = CHIP_RAM_SIZE as u32;
    let main_entry: u32 = ROM_BASE + MAIN_ENTRY_OFFSET;
    let handler_entry: u32 = ROM_BASE + HANDLER_OFFSET;

    rom[0..4].copy_from_slice(&initial_ssp.to_be_bytes());
    rom[4..8].copy_from_slice(&main_entry.to_be_bytes());
    rom[AUTOVECTOR_3_OFFSET..AUTOVECTOR_3_OFFSET + 4].copy_from_slice(&handler_entry.to_be_bytes());

    let main = MAIN_ENTRY_OFFSET as usize;
    rom[main..main + 2].copy_from_slice(&MOVE_W_IMM_ABS_L.to_be_bytes());
    rom[main + 2..main + 4].copy_from_slice(&INTENA_ENABLE_VERTB.to_be_bytes());
    rom[main + 4..main + 8].copy_from_slice(&INTENA_ADDR.to_be_bytes());
    rom[main + 8..main + 10].copy_from_slice(&STOP.to_be_bytes());
    rom[main + 10..main + 12].copy_from_slice(&STOP_SR_WAIT_FOR_IRQ.to_be_bytes());
    // Fallback if control ever returns here without the handler running:
    // halt cleanly rather than looping, so a partial regression still
    // terminates instead of hanging the test.
    rom[main + 12..main + 14].copy_from_slice(&STOP.to_be_bytes());
    rom[main + 14..main + 16].copy_from_slice(&STOP_SR_HALT.to_be_bytes());

    let handler = HANDLER_OFFSET as usize;
    rom[handler..handler + 2].copy_from_slice(&MOVEQ_7_D0.to_be_bytes());
    rom[handler + 2..handler + 4].copy_from_slice(&STOP.to_be_bytes());
    rom[handler + 4..handler + 6].copy_from_slice(&STOP_SR_HALT.to_be_bytes());

    rom
}

#[test]
fn stop_with_unmasked_level_resumes_on_vertb_instead_of_ending_the_run() {
    let rom_path = std::env::temp_dir().join(format!(
        "machine-hosted-stop-resume-{}-{}.rom",
        std::process::id(),
        line!()
    ));
    fs::write(&rom_path, synthetic_rom()).expect("write synthetic ROM");

    // One PAL frame is ~283K CPU clocks and `RUN_BATCH_CYCLES` (the chunk
    // the run loop ticks a stopped CPU by) is far more than that, so the
    // very first tick while stopped can overshoot several frames past the
    // one VERTB needs -- the bound must have headroom past that overshoot
    // or the per-instruction hook's own max-frames check (which does not
    // get the same "let the pending wake land first" deferral) would cut
    // the run off the instant the CPU resumes, before it ever reaches the
    // handler. Old (buggy) behaviour exits after the very first STOP,
    // frame 0, well under this either way.
    let output = Command::new(env!("CARGO_BIN_EXE_machine-hosted"))
        .arg("--rom")
        .arg(&rom_path)
        .arg("--max-frames")
        .arg("20")
        .arg("--max-instructions")
        .arg("1000")
        .output()
        .expect("run machine-hosted");

    let _ = fs::remove_file(&rom_path);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "expected exit 0 (the handler's own clean halt), got {:?}\nstdout:\n{stdout}",
        output.status.code()
    );
    assert!(
        stdout.contains("PHASE1 HOSTED: CPU HALTED CLEANLY"),
        "missing expected status line, stdout:\n{stdout}"
    );
    // PC after a completed `STOP` sits past its own opcode and SR operand
    // word (2 + 2 bytes) -- here, just past the handler's own halting
    // `STOP #$2700` at ROM_BASE + HANDLER_OFFSET + 2. Reaching it is only
    // possible if VERTB actually woke the first STOP and the level-3
    // autovector handler ran; the old (buggy) behaviour reports the
    // *first* STOP's own PC instead and never advances a frame.
    let handler_halt_pc = ROM_BASE + HANDLER_OFFSET + 6;
    assert!(
        stdout.contains(&format!("final PC {handler_halt_pc:#010x}")),
        "run did not reach the interrupt handler's halt -- STOP was \
         treated as terminal instead of waking on VERTB; stdout:\n{stdout}"
    );
}
