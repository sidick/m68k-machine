//! `DisplaySurface` for the hosted runner: captures the Phase 2 stop-gap
//! planar renderer's output to PNG files on disk (`--screenshot`/
//! `--screenshot-frame`/`--screenshot-every`), so a human can actually
//! look at what a guest ROM put on screen (proposal §8.1) instead of only
//! inferring it from serial narration or `--inspect`'s `ExecBase` walk.
//!
//! # Why PNG, and why the `png` crate
//!
//! PPM would need no dependency at all, but every image viewer, browser
//! and CI artifact viewer opens PNG for free, while PPM needs a
//! conversion step first; since screenshots exist purely for a human (or
//! this crate's own regression test) to look at, that convenience is
//! worth one small dependency. `png` (the `image-rs` org's encoder/
//! decoder, MIT/Apache-2.0) is pure Rust with no C toolchain requirement,
//! so it keeps this hosted-only binary's build exactly as simple as
//! `clap`'s already is; the alternative, the `image` crate, pulls in
//! encoders/decoders for a dozen formats this project will never use.
//! `machine-hosted` is a `std` binary crate with no bare-metal build to
//! keep lean (unlike `machine-core`), so this only affects the runner's
//! own compile time.
//!
//! # Why a `DisplaySurface`, not just calling `Renderer::render` directly
//!
//! `machine_core::display::DisplaySurface` is the seam board layers are
//! meant to implement (`display.rs`'s doc comment: "a PNG writer in the
//! hosted runner" is its own example). Nothing in the workspace
//! implements it yet -- the board crates don't wire the renderer up --
//! so this is the trait's first real exercise: `dimensions()` sizes the
//! scratch framebuffer the same way a board layer would, and `present()`
//! is where the completed frame actually leaves the renderer's hands.
//!
//! # Reconstructing chip RAM without a `machine-core` change
//!
//! [`Renderer::render`] wants `chip_ram: &[u8]`, but `MachineBus` borrows
//! its backing 2 MB array exclusively for its whole lifetime and exposes
//! no direct slice accessor -- only the bounds-checked `read_byte`/
//! `read_word`/`read_long` used elsewhere in this crate (`introspect.rs`
//! reads the same way). [`snapshot_chip_ram`] below reconstructs the
//! array through that public read path instead. This is `machine-core`
//! surface this crate is not allowed to add to; a `pub fn chip_ram(&self)
//! -> &[u8; CHIP_RAM_SIZE]` accessor on `MachineBus` would let this crate
//! borrow the array directly and skip the copy -- worth adding there if
//! screenshotting becomes a hot path, not needed for the handful of
//! frames `--screenshot`/`--screenshot-every` actually capture.

use std::io::BufWriter;
use std::path::{Path, PathBuf};

use machine_core::chipset::Chipset;
use machine_core::display::{DisplaySurface, Framebuffer, MAX_HEIGHT, MAX_WIDTH};
use machine_core::render::Renderer;
use machine_core::{MachineBus, CHIP_RAM_BASE, CHIP_RAM_SIZE};

use crate::console::Console;

/// Reconstruct chip RAM's contents through the bus's own read path. See
/// this module's doc comment for why a direct slice isn't available.
///
/// **Caveat**: while the ROM overlay is still mapped
/// (`MachineBus::overlay`), reads below `OVERLAY_END` answer with ROM
/// bytes instead of chip RAM's own -- exactly what the CPU itself would
/// see through the bus, so a screenshot taken that early would show the
/// renderer reading ROM data for any bitplane/copper pointer under
/// `$80000`. Every capture in this project's own testing has landed well
/// after Kickstart/AROS clears the overlay in the first handful of
/// frames, so this has not been observed in practice; a real capture
/// this early would be a red flag on its own (nothing has bitplane
/// pointers programmed yet).
///
/// Only ever called at caller-chosen capture frames, never per emulated
/// frame in the hot path -- the 2 MB of per-byte reads through a bounds-
/// checked accessor costs nothing that matters at that frequency.
fn snapshot_chip_ram(bus: &mut MachineBus) -> Vec<u8> {
    (0..CHIP_RAM_SIZE as u32)
        .map(|offset| bus.read_byte(CHIP_RAM_BASE + offset))
        .collect()
}

/// A `DisplaySurface` that writes whatever frame it is handed to a PNG
/// file at a caller-chosen path. One instance is reused across a
/// `--screenshot-every` sequence; [`PngSurface::set_target`] retargets it
/// before each capture so `present` (the trait method) stays a pure
/// hand-off with no path logic of its own.
pub struct PngSurface {
    target: PathBuf,
}

impl PngSurface {
    pub fn new(target: PathBuf) -> Self {
        Self { target }
    }

    pub fn set_target(&mut self, target: PathBuf) {
        self.target = target;
    }
}

impl DisplaySurface for PngSurface {
    /// The renderer's own worst-case geometry (hires PAL interlace plus
    /// overscan, per `display.rs`'s `MAX_WIDTH`/`MAX_HEIGHT` doc
    /// comments) -- capture everything the renderer could possibly
    /// produce rather than guessing the guest's actual mode ahead of the
    /// render. `draw_bitplanes`/`draw_sprite0` clip to this via
    /// `Framebuffer::put`, so a smaller real picture just leaves the rest
    /// of the canvas at the background colour.
    fn dimensions(&self) -> (usize, usize) {
        (MAX_WIDTH, MAX_HEIGHT)
    }

    fn present(&mut self, pixels: &[u32], width: usize, height: usize) {
        if let Err(e) = write_png(&self.target, pixels, width, height) {
            eprintln!(
                "machine-hosted: failed to write screenshot {}: {e}",
                self.target.display()
            );
        }
    }
}

/// Quick, cheap statistics over one rendered frame: distinct colours seen
/// and how many pixels differ from the *most common* colour. "Most
/// common", not `COLOR00`/pixel `(0,0)`: a first attempt at this used
/// `pixels[0]` on the assumption `Renderer::render`'s initial `fb.fill`
/// (background colour) would still be showing there, but a real
/// Kickstart capture disproved that -- its software mouse pointer's hot
/// spot lands exactly at `(0,0)` in this machine's DIW-relative
/// coordinates, so `pixels[0]` was the *foreground* colour, not the
/// background, and inverted this stat entirely (432293/433152 "differ
/// from background" for a picture that is actually 432293 pixels of flat
/// fill and 859 of pointer/icon). The dominant colour by pixel count is
/// robust to where on the canvas the content happens to land. Logged
/// alongside every capture as the "did anything actually get drawn"
/// evidence this task asks for, and asserted on by this crate's real-ROM
/// regression test without that test needing its own PNG decoder.
pub struct FrameStats {
    pub distinct_colours: usize,
    pub non_background_pixels: usize,
    pub total_pixels: usize,
}

fn frame_stats(pixels: &[u32]) -> FrameStats {
    // A `HashMap` over up to MAX_WIDTH*MAX_HEIGHT (~433k) u32s is a
    // one-off cost paid only at capture frames, same rationale as
    // `snapshot_chip_ram`'s per-byte read loop above.
    let mut counts: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for &p in pixels {
        *counts.entry(p).or_insert(0) += 1;
    }
    let background_count = counts.values().copied().max().unwrap_or(0);
    FrameStats {
        distinct_colours: counts.len(),
        non_background_pixels: pixels.len().saturating_sub(background_count),
        total_pixels: pixels.len(),
    }
}

/// Render one frame from `chipset`/`chip_ram` and hand it to `surface`,
/// returning stats over what came out.
fn capture(chipset: &Chipset, chip_ram: &[u8], surface: &mut PngSurface) -> FrameStats {
    let (width, height) = surface.dimensions();
    let mut pixels = vec![0u32; width * height];
    // `width`/`height` are `MAX_WIDTH`/`MAX_HEIGHT` from `surface`'s own
    // `dimensions()` impl, sized to match `pixels` above by construction,
    // so `Framebuffer::new` cannot actually return `None` here.
    let mut fb = Framebuffer::new(&mut pixels, width, height)
        .expect("scratch buffer sized exactly to surface dimensions");
    Renderer::new().render(chipset, chip_ram, &mut fb);
    let stats = frame_stats(&pixels);
    surface.present(&pixels, width, height);
    stats
}

fn write_png(path: &Path, pixels: &[u32], width: usize, height: usize) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    let w = BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    // Framebuffer pixels are 0xAARRGGBB (display.rs); PNG's RGBA8 wants
    // bytes in R,G,B,A order, so unpack rather than reinterpret the u32s
    // (which would also be host-endianness-dependent).
    let mut rgba = Vec::with_capacity(pixels.len() * 4);
    for &p in pixels {
        rgba.push(((p >> 16) & 0xFF) as u8); // R
        rgba.push(((p >> 8) & 0xFF) as u8); // G
        rgba.push((p & 0xFF) as u8); // B
        rgba.push(((p >> 24) & 0xFF) as u8); // A
    }
    writer
        .write_image_data(&rgba)
        .map_err(|e| std::io::Error::other(e.to_string()))
}

/// Insert a zero-padded frame number before `base`'s extension, for
/// `--screenshot-every` sequences: `out.png` at frame 300 becomes
/// `out-000300.png`. A `base` with no extension gets the suffix appended
/// to the file name instead.
fn sequence_path(base: &Path, frame: u64) -> PathBuf {
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match base.extension() {
        Some(ext) => base.with_file_name(format!("{stem}-{frame:06}.{}", ext.to_string_lossy())),
        None => base.with_file_name(format!("{stem}-{frame:06}")),
    }
}

/// Drives `--screenshot`/`--screenshot-frame`/`--screenshot-every` from
/// the run loop: tracks which chipset frame to capture next and writes
/// it out through a [`PngSurface`] once the guest reaches it.
pub struct ScreenshotJob {
    surface: PngSurface,
    base_path: PathBuf,
    /// The next frame to capture, or `u64::MAX` once a one-shot capture
    /// (no `--screenshot-every`) has already fired.
    next_frame: u64,
    every: Option<u64>,
}

impl ScreenshotJob {
    pub fn new(base_path: PathBuf, first_frame: u64, every: Option<u64>) -> Self {
        Self {
            surface: PngSurface::new(base_path.clone()),
            base_path,
            next_frame: first_frame,
            every,
        }
    }

    /// Called once per hook invocation with the chipset's current frame
    /// counter; a plain integer comparison in the overwhelmingly common
    /// case where no capture is due. `max_frame` caps an
    /// `--screenshot-every` sequence at the run's own frame bound so it
    /// cannot schedule a capture past a run that is about to end anyway.
    pub fn maybe_capture(
        &mut self,
        frame: u64,
        max_frame: u64,
        bus: &mut MachineBus,
        console: &mut Console,
    ) {
        if frame < self.next_frame || self.next_frame > max_frame {
            return;
        }

        let target = if self.every.is_some() {
            sequence_path(&self.base_path, self.next_frame)
        } else {
            self.base_path.clone()
        };
        self.surface.set_target(target.clone());

        let chip_ram = snapshot_chip_ram(bus);
        let stats = capture(&bus.chipset, &chip_ram, &mut self.surface);
        console.diag(&format!(
            "screenshot: frame {} -> {} ({}x{}, {} distinct colour{}, {}/{} pixels differ from background)",
            self.next_frame,
            target.display(),
            MAX_WIDTH,
            MAX_HEIGHT,
            stats.distinct_colours,
            if stats.distinct_colours == 1 { "" } else { "s" },
            stats.non_background_pixels,
            stats.total_pixels,
        ));

        match self.every {
            Some(step) if step > 0 => self.next_frame += step,
            _ => self.next_frame = u64::MAX,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_path_inserts_frame_before_extension() {
        assert_eq!(
            sequence_path(Path::new("out.png"), 300),
            PathBuf::from("out-000300.png")
        );
    }

    #[test]
    fn sequence_path_handles_no_extension() {
        assert_eq!(
            sequence_path(Path::new("out"), 42),
            PathBuf::from("out-000042")
        );
    }

    #[test]
    fn sequence_path_preserves_directory() {
        assert_eq!(
            sequence_path(Path::new("/tmp/shots/out.png"), 7),
            PathBuf::from("/tmp/shots/out-000007.png")
        );
    }

    #[test]
    fn frame_stats_counts_a_uniform_frame_as_one_colour() {
        let pixels = vec![0xFF00_0000u32; 16];
        let stats = frame_stats(&pixels);
        assert_eq!(stats.distinct_colours, 1);
        assert_eq!(stats.non_background_pixels, 0);
    }

    #[test]
    fn frame_stats_counts_pixels_that_differ_from_the_first_pixel() {
        // pixels[0] defines the background per frame_stats' doc comment;
        // leave it alone and change two others.
        let mut pixels = vec![0xFF00_0000u32; 16];
        pixels[5] = 0xFFFF_0000;
        pixels[9] = 0xFF00_FF00;
        let stats = frame_stats(&pixels);
        assert_eq!(stats.distinct_colours, 3, "background + 2 changed colours");
        assert_eq!(
            stats.non_background_pixels, 2,
            "only indices 5 and 9 changed"
        );
    }
}
