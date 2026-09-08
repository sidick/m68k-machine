//! `machine-hosted`: the Phase 1 hosted runner for the m68k Machine.
//!
//! `machine-core` is `#![no_std]` and cannot link `m68k` (which is not
//! `no_std` as of 0.12.1 -- `docs/phase0-findings.md`), so ROM boot for
//! Phase 1 runs here, as an ordinary `std` binary
//! (`docs/adr-0001-bare-metal-vs-linux-host.md`).

mod blitter_trace;
mod bus;
mod cli;
mod console;
mod hd_image;
mod input_script;
mod introspect;
mod pktvol;
mod rom_image;
mod run;
mod screenshot;
mod serial_script;
mod serial_tcp;

use std::process::ExitCode;

use clap::Parser;

use cli::Args;
use console::Console;

fn main() -> ExitCode {
    let args = Args::parse();

    let mut console = match Console::new(args.serial_log.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("machine-hosted: cannot open --serial-log: {e}");
            return ExitCode::from(3);
        }
    };

    let report = run::run(&args, &mut console);
    let code = report.exit_code();
    console.diag(&format!("PHASE1 HOSTED: {}", report.status_line()));

    ExitCode::from(code)
}
