//! End-to-end proof that a Cloanto/Amiga Forever-encoded ROM passed with
//! no `--rom-key` is a clear, actionable refusal -- not a silent boot of
//! the raw XOR'd bytes, and not a panic. `crate::rom_image`'s own unit
//! tests already prove `prepare_bytes` returns this error; this test
//! proves it actually reaches the compiled binary's exit code and stdout
//! the way a user would see it, matching the pattern
//! `tests/real_rom.rs` and `tests/smoke.rs` already use for driving
//! `machine-hosted` as a subprocess.
//!
//! The container built here is synthetic -- eleven bytes of the real
//! `AMIROMTYPE1` magic (public knowledge, not itself licensed data)
//! followed by arbitrary filler -- no real Amiga Forever ROM is read,
//! shipped, or needed for this test to run unconditionally.

use std::fs;
use std::process::Command;

#[test]
fn cloanto_encoded_rom_without_a_key_is_refused_not_silently_booted() {
    let rom_path = std::env::temp_dir().join(format!(
        "machine-hosted-cloanto-{}-{}.rom",
        std::process::id(),
        line!()
    ));
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"AMIROMTYPE1");
    bytes.extend_from_slice(&vec![0xAAu8; 512 * 1024]);
    fs::write(&rom_path, &bytes).expect("write synthetic Cloanto-framed image");

    let output = Command::new(env!("CARGO_BIN_EXE_machine-hosted"))
        .arg("--rom")
        .arg(&rom_path)
        .output()
        .expect("run machine-hosted");

    let _ = fs::remove_file(&rom_path);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // `Report::exit_code`'s `SetupError` arm: 3.
    assert_eq!(
        output.status.code(),
        Some(3),
        "expected exit code 3 (setup error), got {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
    assert!(
        stdout.contains("--rom-key") || stderr.contains("--rom-key"),
        "expected the actionable --rom-key message somewhere in the runner's output\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
