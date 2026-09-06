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
    ///
    /// `0` means unlimited: the frame count is never checked, and the run
    /// continues until some other outcome (clean halt, wedge, or
    /// `--max-instructions`) ends it. This exists for `--serial-tcp`
    /// interactive sessions, which have no natural frame count to bound
    /// them by; every other caller (CI included) keeps getting a genuine
    /// bound because `0` is never the default.
    #[arg(long, default_value_t = 6_000)]
    pub max_frames: u64,

    /// Stop after this many retired instructions even if nothing else
    /// ended the run. Guarantees termination for CI independent of
    /// whether the guest ever reaches a frame boundary.
    ///
    /// `0` means unlimited, for the same reason and the same
    /// `--serial-tcp` use case as `--max-frames`'s `0`. Pass both as `0`
    /// for a genuinely unbounded interactive session -- terminate it from
    /// the outside (Ctrl-C, killing the process) instead.
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

    /// Path to an `input_script`-language file of host→guest input-card
    /// events to drive during the run (`KEYDOWN`/`KEYUP`/`MOVE`/
    /// `BUTTONDOWN`/`BUTTONUP`/`SLEEP` directives -- see
    /// `crate::input_script`'s doc comment). Attaches the native input
    /// card (`machine_core::input`) the same way `--hostblk` attaches
    /// `hostblk`: omit this flag and neither the card's AUTOCONFIG board
    /// nor its bus routing exist at all. The card carries a DiagArea boot
    /// ROM (`m68k/input-rom/`) whose driver turns queued events into real
    /// `IND_WRITEEVENT` calls, so events reach Intuition: a scripted
    /// `MOVE` moves the pointer, confirmed both by `--inspect`'s
    /// `IntuitionBase` `MouseX`/`MouseY` report and by the pointer sprite
    /// appearing in a screenshot. See `docs/input-protocol.md`.
    #[arg(long)]
    pub input_script: Option<PathBuf>,

    /// Bind this address (e.g. `127.0.0.1:1234`) and bridge it
    /// bidirectionally onto the guest's serial port: bytes the guest
    /// writes to `SERDAT` go to the connected client, and bytes the
    /// client sends arrive at the guest's `SERDATR` the same way
    /// `--serial-script`'s `SEND` does
    /// (`Chipset::push_serial_in_byte`). See `crate::serial_tcp`'s doc
    /// comment for the full rationale, the client-lifecycle contract (no
    /// client yet / connects mid-run / disconnects and reconnects), and
    /// why TCP was chosen over, say, a PTY.
    ///
    /// A byte stream is all this carries -- nothing here is specific to
    /// any one client. AmiPilot's `WireClient.connect(host, port)` is one
    /// consumer (its own wire protocol is transport-agnostic; a socket is
    /// just one carrier it already supports, on equal footing with a real
    /// serial port), but a plain `nc`, a terminal, or a future debugger
    /// work identically, since only the guest's serial port is being
    /// spoken to.
    ///
    /// Refused together with `--serial-script`: both compete to be the
    /// one source of host->guest bytes, and silently picking a priority
    /// between "a live human/client at a socket" and "a fixed scripted
    /// sequence" would be a worse answer than making the caller choose.
    /// `--serial-log` composes fine with this flag (it tees independently
    /// of where guest output also goes), and guest output still reaches
    /// stdout as always.
    ///
    /// An interactive session run this way has no natural frame or
    /// instruction count to end it at -- pair this with `--max-frames 0
    /// --max-instructions 0` (see their docs) and end the run from
    /// outside (Ctrl-C) when done, or pass real bounds if a timeout is
    /// wanted instead.
    #[arg(long)]
    pub serial_tcp: Option<String>,

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

    /// Path to a disk image to attach to `hostblk` unit 0 (ADR 0003,
    /// `machine_core::hostblk`) -- this machine's own doorbell block
    /// card, and the boot path Gayle IDE retired into
    /// (`docs/device-ledger.md`). Raw sequential sectors -- an `.hdf`
    /// file (a bare RDB-partitioned image with no ADF/DMS-style wrapper)
    /// is exactly this shape; `machine-hosted`'s `FileBlockDevice` works
    /// unchanged against it (that's the point of `hostblk` reusing
    /// `machine_core::block::BlockDevice`). Omit for no `hostblk` card at
    /// all. This board carries a DiagArea boot ROM (`m68k/hostblk-rom/`)
    /// with its own wire-protocol driver and RDB mounter, soaked against
    /// devsoak (`docs/hostblk-soak.md`) -- see `--inspect`'s `hostblk
    /// state:` section for a host-side view of it.
    #[arg(long)]
    pub hostblk: Option<PathBuf>,

    /// Open `--hostblk` for writing rather than the default read-only.
    /// Off by default on purpose: a disk image worth attaching is
    /// typically a licensed-media conversion (`docs/storage.md`) that
    /// took real effort to build and cannot simply be re-downloaded if a
    /// bug corrupts it. A full boot to Workbench never needs to write a
    /// sector; pass this flag once write access is actually wanted (e.g.
    /// testing Kickstart's write path, or letting Workbench persist
    /// state back to the image).
    #[arg(long, default_value_t = false)]
    pub hostblk_writable: bool,

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

    /// Attach fast RAM (`machine_core::fastram`) over heap-allocated
    /// storage, sized in megabytes, and register its single Zorro III
    /// AUTOCONFIG board (`ERTF_MEMLIST` set, so `expansion.library` links
    /// it into the system free-memory list with no driver of ours).
    ///
    /// Defaults to 256 MB, inside proposal §13's intended 256-384 MB
    /// guest footprint: this was previously off by default on the theory
    /// that an extra AUTOCONFIG board could move the base address a
    /// `--graphics-bus 3` Graffity card is assigned (the two share the
    /// Zorro III address pool). That risk was tested rather than assumed
    /// -- with both flags set, Zorro III Graffity still lands at the
    /// same base address and its screenshot baseline stays
    /// byte-identical, because `machine-hosted` always registers
    /// Graffity first (see the registration order comment at this
    /// flag's call site) -- so it no longer justifies defaulting off.
    /// `docs/hostblk-soak.md` records the other half of the reasoning:
    /// `hostblk`'s devsoak run genuinely needs fast RAM to have room for
    /// its concurrent transfer buffers, so leaving this off by default
    /// was actively the wrong default for the machine's own storage
    /// path, not merely a conservative one.
    ///
    /// Pass `0` to disable fast RAM entirely -- e.g. to reproduce a
    /// baseline that predates it, or to isolate a test that specifically
    /// wants to prove behaviour without it. Verify adoption with
    /// `--inspect`'s `MemList` walk, which is the only real evidence the
    /// guest adopted the memory rather than this bus merely answering
    /// for it (`docs/device-ledger.md`'s fast RAM row).
    #[arg(long, default_value_t = 256)]
    pub fast_ram_mb: u32,

    /// Attach the native RTG display board (`machine_core::rtgboard`,
    /// ADR 0002) over heap-allocated VRAM, with a **single** advertised
    /// mode: `WIDTHxHEIGHT`, e.g. `640x480`. Without this flag nothing
    /// about the boot path changes -- the chain and every address this
    /// board would occupy stay untouched
    /// (`MachineBus::with_rtgboard`'s own doc comment).
    ///
    /// A single-entry catalog, not a menu, is the deliberate choice: it
    /// is the honest shape of what a real Phase 5 board (a UEFI GOP
    /// framebuffer fixed at `ExitBootServices`) can actually offer --
    /// `machine_core::rtgboard`'s module docs, "Mode advertisement". A
    /// richer host could advertise more, but nothing in this project
    /// currently negotiates that, and there is no driver yet to consume
    /// it either way (this board's own module docs: host side only,
    /// this increment).
    #[arg(long)]
    pub rtgboard: Option<String>,

    /// Pixel format `--rtgboard`'s single advertised mode uses --
    /// `rgb565`, `rgbx8888` (default) or `bgrx8888`
    /// (`machine_core::rtgboard::format`). Ignored without `--rtgboard`.
    #[arg(long, default_value = "rgbx8888")]
    pub rtgboard_format: RtgFormatArg,

    /// VRAM size, in megabytes, for `--rtgboard`'s board. Must be large
    /// enough to hold one frame of the requested mode at the requested
    /// format (width * height * bytes-per-pixel); `run.rs` refuses to
    /// start rather than silently truncating a framebuffer that would
    /// not fit. Ignored without `--rtgboard`.
    #[arg(long, default_value_t = 8)]
    pub rtgboard_vram_mb: u32,
}

/// CLI surface for [`machine_core::rtgboard::format`] -- kept as a
/// separate type so `clap`'s `ValueEnum` derive doesn't need to live on
/// the foreign `machine-core` constants (same shape as [`FloppyArg`]/
/// [`CpuTypeArg`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum RtgFormatArg {
    Rgb565,
    Rgbx8888,
    Bgrx8888,
}

impl RtgFormatArg {
    pub fn to_format_byte(self) -> u8 {
        match self {
            RtgFormatArg::Rgb565 => machine_core::rtgboard::format::RGB_565,
            RtgFormatArg::Rgbx8888 => machine_core::rtgboard::format::RGBX_8888,
            RtgFormatArg::Bgrx8888 => machine_core::rtgboard::format::BGRX_8888,
        }
    }
}

/// Parse `--rtgboard`'s `WIDTHxHEIGHT` value. A parse failure here is a
/// CLI usage error (`run.rs` exits with a message), not a guest-visible
/// condition -- this never reaches `rtgboard::RtgBoard`, which only ever
/// sees the resulting, already-valid [`machine_core::rtgboard::
/// ModeDescriptor`].
pub fn parse_rtgboard_geometry(spec: &str) -> Result<(u32, u32), String> {
    let (w, h) = spec
        .split_once('x')
        .ok_or_else(|| format!("--rtgboard expects WIDTHxHEIGHT, got {spec:?}"))?;
    let width: u32 = w
        .parse()
        .map_err(|_| format!("--rtgboard: invalid width {w:?}"))?;
    let height: u32 = h
        .parse()
        .map_err(|_| format!("--rtgboard: invalid height {h:?}"))?;
    if width == 0 || height == 0 {
        return Err("--rtgboard: width and height must both be nonzero".to_string());
    }
    Ok((width, height))
}

#[cfg(test)]
mod rtgboard_cli_tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_geometry() {
        assert_eq!(parse_rtgboard_geometry("640x480"), Ok((640, 480)));
    }

    #[test]
    fn rejects_missing_separator() {
        assert!(parse_rtgboard_geometry("640480").is_err());
    }

    #[test]
    fn rejects_zero_dimensions() {
        assert!(parse_rtgboard_geometry("0x480").is_err());
        assert!(parse_rtgboard_geometry("640x0").is_err());
    }

    #[test]
    fn rejects_non_numeric_fields() {
        assert!(parse_rtgboard_geometry("wideXhigh").is_err());
    }
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
