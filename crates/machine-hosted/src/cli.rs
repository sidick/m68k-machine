//! Command-line surface. Kept in its own module so `main.rs` stays the
//! run loop, not argument plumbing.

use std::path::PathBuf;

use clap::Parser;

/// Hosted runner for the m68k Machine (Phase 1: blind ROM boot).
///
/// Loads a single Kickstart ROM, or a Kickstart-format main ROM plus an
/// AROS extended ROM pair, onto [`machine_core::MachineBus`] and runs it
/// through an `m68k-rs` `CpuCore`. See `docs/adr-0001-bare-metal-vs-linux-host.md`
/// for why this runner is a hosted `std` binary rather than bare metal.
#[derive(Parser, Debug)]
#[command(name = "machine-hosted", version, about)]
pub struct Args {
    /// Path to the main ROM image (Kickstart, or the AROS main ROM when
    /// paired with `--ext-rom`).
    #[arg(long)]
    pub rom: PathBuf,

    /// Path to an AROS extended ROM image, mapped at $E00000. Omit when
    /// booting a single Kickstart image.
    #[arg(long)]
    pub ext_rom: Option<PathBuf>,

    /// CPU model to present to the guest. Only `68040` is implemented
    /// today (proposal §6.2); the flag exists so the choice is explicit
    /// and future models are a non-breaking addition.
    #[arg(long, default_value = "68040")]
    pub cpu: CpuModel,

    /// Stop after this many chipset frames (VERTB boundaries) even if
    /// nothing else ended the run. Guarantees termination for CI.
    #[arg(long, default_value_t = 6_000)]
    pub max_frames: u64,

    /// Stop after this many retired instructions even if nothing else
    /// ended the run. Guarantees termination for CI independent of
    /// whether the guest ever reaches a frame boundary.
    #[arg(long, default_value_t = 200_000_000)]
    pub max_instructions: u64,

    /// Log every retired instruction: PC, opcode, and a best-effort
    /// disassembly from `m68k::dasm`.
    #[arg(long, default_value_t = false)]
    pub trace: bool,

    /// Tee the serial console (guest output plus runner diagnostics) to
    /// this file in addition to stdout.
    #[arg(long)]
    pub serial_log: Option<PathBuf>,
}

/// The CPU models this runner knows how to select. A thin wrapper around
/// [`m68k::CpuType`] rather than that type directly, so `clap`'s
/// `ValueEnum` derive doesn't need to live on a foreign type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum CpuModel {
    #[value(name = "68040")]
    M68040,
}

impl From<CpuModel> for m68k::CpuType {
    fn from(model: CpuModel) -> Self {
        match model {
            CpuModel::M68040 => m68k::CpuType::M68040,
        }
    }
}
