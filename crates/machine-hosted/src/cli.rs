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

    /// After the run ends (for any reason -- limit reached, wedge, clean
    /// halt), print a report on Exec's guest-memory state: whether
    /// `ExecBase` is well-formed, which resident modules initialised,
    /// whether Kickstart guru'd, and the task-ready/task-wait picture.
    /// The only way to tell "healthy and idle" from "stuck" apart for a
    /// stock Kickstart, which (unlike AROS) never narrates over serial
    /// (see `crates/machine-hosted/src/introspect.rs`).
    #[arg(long, default_value_t = false)]
    pub inspect: bool,

    /// Path to a `serial_script`-language file of host→guest serial
    /// input to drive during the run (`SEND`/`WAIT`/`SLEEP`
    /// directives -- see `crate::serial_script`'s doc comment). This is
    /// the machine's only way to hand the guest received bytes at all;
    /// see `Chipset::push_serial_in_byte`.
    #[arg(long)]
    pub serial_script: Option<PathBuf>,

    /// Force a genuine 68k illegal-instruction exception into the guest
    /// this many chipset frames after the ROM overlay first clears
    /// (`MachineBus::overlay`), by calling `CpuCore::take_illegal_exception`
    /// directly from the host rather than waiting for the guest to fetch
    /// a real illegal opcode. This is a deliberate host-driven crash
    /// trigger, not a fault injection bug: it exists to reach Kickstart's
    /// alert/LED-blink loop (and, from there, attempt the documented
    /// serial break-in into ROMWack -- AHRM alert chapter) at a point
    /// where the ROM would otherwise stay healthy and idle. Omit for a
    /// normal boot run.
    #[arg(long)]
    pub trigger_illegal_after_frames: Option<u64>,

    /// Capture a rendered frame (Phase 2's stop-gap planar renderer,
    /// proposal §8.1: copper-walked `BPL`/`DIW`/`DDF`/`COLOR` state, not
    /// P96) to this PNG file once the guest reaches `--screenshot-frame`.
    /// With `--screenshot-every` also set, this becomes the base name for
    /// a numbered sequence instead of a single file. Omit for a normal
    /// run: the renderer is never driven unless a screenshot is asked
    /// for, so a plain boot run pays nothing for it.
    #[arg(long)]
    pub screenshot: Option<PathBuf>,

    /// Which chipset frame (VERTB boundary, `Chipset::frames`) to capture
    /// for `--screenshot`. Default is comfortably inside the default
    /// `--max-frames` while still late enough for Kickstart/AROS to have
    /// had time to program something (or conclusively not have).
    #[arg(long, default_value_t = 300)]
    pub screenshot_frame: u64,

    /// With `--screenshot` set, additionally capture every this many
    /// frames from `--screenshot-frame` onward (through `--max-frames`),
    /// each to its own numbered file (`name-000300.png`,
    /// `name-000350.png`, ...) instead of a single capture -- useful for
    /// watching boot progress frame by frame.
    #[arg(long)]
    pub screenshot_every: Option<u64>,

    /// Path to a disk image to attach to Gayle's IDE port (proposal §11.1,
    /// `machine_core::gayle`). Raw sequential sectors -- an `.hdf` file (a
    /// bare RDB-partitioned image with no ADF/DMS-style wrapper) is
    /// exactly this shape. Omit for no drive at all, this machine's
    /// previous behaviour and still the honest story for the eventual
    /// MIRAGE storage path (proposal §10.3) -- Gayle IDE is bring-up only
    /// (`gayle.rs`'s module doc comment).
    #[arg(long)]
    pub hd: Option<PathBuf>,

    /// Open `--hd` for writing rather than the default read-only. Off by
    /// default on purpose: this is a brand new, so-far-unproven IDE
    /// implementation, and a disk image worth attaching is typically a
    /// licensed-media conversion (`docs/storage.md`) that took real effort
    /// to build and cannot simply be re-downloaded if a bug corrupts it. A
    /// full boot to Workbench never needs to write a sector; pass this
    /// flag once write access is actually wanted (e.g. testing Kickstart's
    /// write path, or letting Workbench persist state back to the image).
    #[arg(long, default_value_t = false)]
    pub hd_writable: bool,

    /// Attach the Graffity graphics card (`machine_core::graffity`) over
    /// heap-allocated VRAM and register its AUTOCONFIG board(s) on the
    /// chain. Without this flag nothing about the boot path changes --
    /// the chain and every address the card would occupy stay exactly as
    /// they are today (`MachineBus::with_graphics`'s own doc comment).
    /// With it, `--screenshot`'s capture path prefers the card's own
    /// `decoded_mode()` framebuffer over the stop-gap planar renderer
    /// once a driver has programmed one (`crate::screenshot`'s RTG
    /// present path). `--graphics-bus` chooses which variant; the
    /// default (`2`) is exactly this flag's original meaning.
    #[arg(long, default_value_t = false)]
    pub graphics: bool,

    /// Which Zorro bus generation `--graphics`'s Graffity card presents:
    /// `2` (default) is the original two-board Zorro II shape (VRAM,
    /// then registers); `3` is the single 16 MB Zorro III window
    /// (`machine_core::graffity` module docs). Ignored without
    /// `--graphics`.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(2..=3))]
    pub graphics_bus: u8,

    /// VRAM size, in megabytes, for `--graphics`'s card. 2 MB matches the
    /// Copperline oracle's `graffityz2`/`graffityz3` configurations this
    /// project checks against. Ignored without `--graphics`.
    #[arg(long, default_value_t = 2)]
    pub graphics_vram_mb: u32,

    /// Floppy drive configuration this machine presents on CIA-A PRA /
    /// CIA-B PRB (proposal §7.2; see `machine_core::cia::FloppyDrive`).
    /// `none` (the default) is this machine's honest hardware story --
    /// storage is MIRAGE over Zorro III (proposal §10.3), never a
    /// floppy connector -- and is confirmed against real hardware and
    /// Amiberry (both with zero drives attached) to still reach
    /// Kickstart's no-boot-media screen. `empty` (a drive present with
    /// no disk in it) is kept only as a diagnostic/compatibility mode,
    /// not a normal configuration for this machine.
    #[arg(long, default_value = "none")]
    pub floppy: FloppyArg,

    /// Record every distinct blitter register combination the guest arms
    /// (proposal §12's "recorded Workbench traces" half of the blitter
    /// differential, `crate::blitter_trace`) to this file, deduplicated
    /// on the fly. Absolute pointers are dropped; what's kept is exactly
    /// what `crates/machine-hosted/tests/blitter_differential.rs` needs
    /// to replay each combination against Copperline with randomised
    /// memory. Omit for a normal run: without this flag nothing about the
    /// boot path changes -- every write still reaches `MachineBus`
    /// unmodified, and the one extra check per register write costs
    /// nothing observable (`blitter_trace.rs`'s module doc comment).
    #[arg(long)]
    pub blitter_trace: Option<PathBuf>,
}

/// CLI surface for [`machine_core::cia::FloppyPresence`] -- kept as a
/// separate type so `clap`'s `ValueEnum` derive doesn't need to live on
/// the foreign `machine-core` type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum FloppyArg {
    /// No physical drive at all -- the default; see the `--floppy` help.
    None,
    /// A drive present with no disk in it -- diagnostic/compatibility
    /// mode only.
    Empty,
}

impl From<FloppyArg> for machine_core::cia::FloppyPresence {
    fn from(arg: FloppyArg) -> Self {
        match arg {
            FloppyArg::None => machine_core::cia::FloppyPresence::None,
            FloppyArg::Empty => machine_core::cia::FloppyPresence::Empty,
        }
    }
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
