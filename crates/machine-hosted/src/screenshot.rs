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

use machine_core::display::{DisplaySurface, Framebuffer, MAX_HEIGHT, MAX_WIDTH};
use machine_core::render::{render_rtg, Renderer};
use machine_core::rtgboard::format as rtg_format;
use machine_core::{MachineBus, CHIP_RAM_BASE, CHIP_RAM_SIZE};

use crate::console::Console;

/// Walk `rtgboard`'s currently applied mode and convert each pixel to
/// `0xAARRGGBB`, the same job [`render_rtg`] does for a Graffity/Cirrus
/// framebuffer -- but written fresh here rather than by feeding
/// `render_rtg` a synthesised `cirrus::DecodedMode`, because
/// `rtgboard::format`'s three pixel formats (module docs there: explicit
/// big-endian `RGB_565`, and UEFI-named `RGBX_8888`/`BGRX_8888` with
/// their own byte order) do not correspond to any of `render_rtg`'s four
/// depths, which are little-endian VGA/Cirrus conventions this project's
/// own module docs there admit are "best-effort", not something
/// `rtgboard`'s own explicit-by-design formats should be forced through.
/// `render.rs` is also outside this increment's edit scope.
///
/// Every VRAM access is bounds-checked (`vram.get`, defaulting to `0`
/// past the end) even though [`machine_core::rtgboard::RtgBoard::
/// current_mode`]'s own commit validation already guarantees the
/// described rectangle fits -- the same defence-in-depth [`render_rtg`]
/// applies to its own already-validated `DecodedMode`, cheap insurance
/// against this function ever being handed a mode that didn't actually
/// come from the same board's own `current_mode()`.
fn render_rtgboard(
    width: u32,
    height: u32,
    format: u8,
    stride: u32,
    fb_offset: u32,
    vram: &[u8],
    fb: &mut Framebuffer,
) {
    let width = (width as usize).min(fb.width);
    let height = (height as usize).min(fb.height);

    for y in 0..height {
        let row_start = fb_offset as usize + y * stride as usize;
        for x in 0..width {
            let argb = match format {
                rtg_format::RGB_565 => {
                    let off = row_start + x * 2;
                    let hi = vram.get(off).copied().unwrap_or(0) as u16;
                    let lo = vram.get(off + 1).copied().unwrap_or(0) as u16;
                    let v = (hi << 8) | lo; // big-endian, per `rtgboard::format::RGB_565`
                    let r = (v >> 11) & 0x1F;
                    let g = (v >> 5) & 0x3F;
                    let b = v & 0x1F;
                    let expand5 = |c: u16| -> u32 { ((c as u32) << 3) | ((c as u32) >> 2) };
                    let expand6 = |c: u16| -> u32 { ((c as u32) << 2) | ((c as u32) >> 4) };
                    0xFF00_0000 | (expand5(r) << 16) | (expand6(g) << 8) | expand5(b)
                }
                rtg_format::RGBX_8888 => {
                    let off = row_start + x * 4;
                    let r = vram.get(off).copied().unwrap_or(0) as u32;
                    let g = vram.get(off + 1).copied().unwrap_or(0) as u32;
                    let b = vram.get(off + 2).copied().unwrap_or(0) as u32;
                    0xFF00_0000 | (r << 16) | (g << 8) | b
                }
                rtg_format::BGRX_8888 => {
                    let off = row_start + x * 4;
                    let b = vram.get(off).copied().unwrap_or(0) as u32;
                    let g = vram.get(off + 1).copied().unwrap_or(0) as u32;
                    let r = vram.get(off + 2).copied().unwrap_or(0) as u32;
                    0xFF00_0000 | (r << 16) | (g << 8) | b
                }
                // Unrecognised format: `current_mode()` can only ever
                // hold a format `RtgBoard::commit` already validated
                // against its catalog, so this is unreachable in
                // practice; render black rather than panic if it is ever
                // reached anyway (same defensive posture as an
                // out-of-bounds VRAM read above).
                _ => 0xFF00_0000,
            };
            fb.put(x, y, argb);
        }
    }
}

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
/// Kickstart capture disproved that -- a since-fixed `draw_sprite0` bug
/// (it read sprite 0's height from a stale, unrelated chipset register
/// instead of the sprite's real header in chip RAM -- see `render.rs`'s
/// doc comment on that function) drew a bogus, oversized sprite shape
/// with its top-left corner exactly at `(0,0)` in this machine's
/// DIW-relative coordinates, so `pixels[0]` was that shape's colour, not
/// the real background, and inverted this stat entirely (432293/433152
/// "differ from background" for a picture that was actually 432293
/// pixels of flat fill and 859 of the bogus shape). The dominant colour
/// by pixel count is robust to where on the canvas any drawn content
/// happens to land, real or (as that investigation found) not. Logged
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

/// Render one frame from `bus` and hand it to `surface`, returning stats
/// over what came out.
///
/// **Present-path selection** is `SetSwitch` in effect (task doc comment):
/// when the attached Graffity card (if any, `--graphics`) has a driver-
/// programmed mode (`decoded_mode()` returns `Some`), that RTG framebuffer
/// is what gets captured, walked straight out of VRAM via
/// [`render_rtg`] -- no `chip_ram` snapshot needed at all, since RTG pixel
/// data never touches chip RAM. Otherwise, if the native `rtgboard`
/// (`--rtgboard`) has a mode applied (`current_mode()` returns `Some` --
/// through the register interface described in
/// `docs/rtgboard-protocol.md`, since no driver exists yet to program one
/// from a guest; a host-side test committing a mode directly is exactly
/// how this path is exercised today), that board's VRAM is captured
/// instead, via [`render_rtgboard`]. Graffity is checked first,
/// deliberately, so attaching both at once (not a configuration any
/// baseline uses) cannot change which one Graffity's own baseline
/// captures. Otherwise (neither card attached, or attached but not yet
/// programmed) this falls back to the stop-gap planar renderer exactly as
/// before either card existed, so a run with neither attached is
/// byte-for-byte unaffected by either RTG branch ever having been added.
fn capture(bus: &mut MachineBus, surface: &mut PngSurface) -> FrameStats {
    if let Some(mode) = bus.graphics().and_then(|card| card.decoded_mode()) {
        let width = mode.width as usize;
        let height = mode.height as usize;
        let mut pixels = vec![0u32; width * height];
        {
            // Bounded by `Framebuffer::new`'s own sizing (matches
            // `width`/`height` exactly), so this cannot fail.
            let mut fb = Framebuffer::new(&mut pixels, width, height)
                .expect("scratch buffer sized exactly to the decoded mode's own geometry");
            let card = bus
                .graphics()
                .expect("just matched Some(mode) from this same card above");
            render_rtg(
                &mode,
                card.vram(),
                |index| card.palette_argb(index),
                &mut fb,
            );
        }
        let stats = frame_stats(&pixels);
        surface.present(&pixels, width, height);
        return stats;
    }

    if let Some((width, height, format, stride, fb_offset)) =
        bus.rtgboard().and_then(|card| card.current_mode())
    {
        let w = width as usize;
        let h = height as usize;
        let mut pixels = vec![0u32; w * h];
        {
            let mut fb = Framebuffer::new(&mut pixels, w, h)
                .expect("scratch buffer sized exactly to the applied mode's own geometry");
            let card = bus
                .rtgboard()
                .expect("just matched Some(mode) from this same board above");
            render_rtgboard(
                width,
                height,
                format,
                stride,
                fb_offset,
                card.vram(),
                &mut fb,
            );
        }
        let stats = frame_stats(&pixels);
        surface.present(&pixels, w, h);
        return stats;
    }

    let (width, height) = surface.dimensions();
    let mut pixels = vec![0u32; width * height];
    // `width`/`height` are `MAX_WIDTH`/`MAX_HEIGHT` from `surface`'s own
    // `dimensions()` impl, sized to match `pixels` above by construction,
    // so `Framebuffer::new` cannot actually return `None` here.
    let mut fb = Framebuffer::new(&mut pixels, width, height)
        .expect("scratch buffer sized exactly to surface dimensions");
    let chip_ram = snapshot_chip_ram(bus);
    Renderer::new().render(&bus.chipset, &chip_ram, &mut fb);
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

        let graffity_mode = bus.graphics().and_then(|card| card.decoded_mode());
        let rtgboard_mode = bus.rtgboard().and_then(|card| card.current_mode());
        let stats = capture(bus, &mut self.surface);
        // `capture`'s own present-path priority, mirrored here so the
        // diagnostic line reports whichever geometry actually got
        // captured: Graffity first, then `rtgboard`, then the planar
        // renderer's fixed worst-case canvas.
        let (width, height) = if let Some(m) = graffity_mode {
            (m.width as usize, m.height as usize)
        } else if let Some((w, h, ..)) = rtgboard_mode {
            (w as usize, h as usize)
        } else {
            (MAX_WIDTH, MAX_HEIGHT)
        };
        console.diag(&format!(
            "screenshot: frame {} -> {} ({}x{}, {} distinct colour{}, {}/{} pixels differ from background)",
            self.next_frame,
            target.display(),
            width,
            height,
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
    use machine_core::rtgboard::{format, ModeDescriptor};
    use machine_core::CHIP_RAM_SIZE;

    use super::*;

    // ---- render_rtgboard: a known VRAM pattern comes out as pixels ---------

    #[test]
    fn rgbx8888_pattern_in_vram_decodes_to_the_expected_argb_pixels() {
        // Two pixels, hand-built per `rtgboard::format::RGBX_8888`'s own
        // documented byte order (R,G,B,pad, low to high address).
        let vram = [
            0x11, 0x22, 0x33, 0x00, // pixel 0: R=0x11 G=0x22 B=0x33
            0xAA, 0xBB, 0xCC, 0x00, // pixel 1: R=0xAA G=0xBB B=0xCC
        ];
        let mut pixels = [0u32; 2];
        let mut fb = Framebuffer::new(&mut pixels, 2, 1).unwrap();
        render_rtgboard(2, 1, format::RGBX_8888, 8, 0, &vram, &mut fb);
        assert_eq!(pixels[0], 0xFF11_2233);
        assert_eq!(pixels[1], 0xFFAA_BBCC);
    }

    #[test]
    fn bgrx8888_pattern_uses_the_opposite_channel_order_from_rgbx() {
        let vram = [0x11, 0x22, 0x33, 0x00]; // B=0x11 G=0x22 R=0x33
        let mut pixels = [0u32; 1];
        let mut fb = Framebuffer::new(&mut pixels, 1, 1).unwrap();
        render_rtgboard(1, 1, format::BGRX_8888, 4, 0, &vram, &mut fb);
        assert_eq!(
            pixels[0], 0xFF33_2211,
            "BGRX_8888 must not decode identically to RGBX_8888 (ADR 0002's black-screen warning)"
        );
    }

    #[test]
    fn render_rtgboard_honours_a_stride_wider_than_the_true_row() {
        // Two rows of one RGBX_8888 pixel each, but padded to 16 bytes/row
        // rather than the true 4 -- the second pixel must be read from the
        // padded offset, not immediately after the first.
        let mut vram = [0u8; 32];
        vram[0..4].copy_from_slice(&[0x01, 0x02, 0x03, 0x00]);
        vram[16..20].copy_from_slice(&[0x04, 0x05, 0x06, 0x00]);
        let mut pixels = [0u32; 2];
        let mut fb = Framebuffer::new(&mut pixels, 1, 2).unwrap();
        render_rtgboard(1, 2, format::RGBX_8888, 16, 0, &vram, &mut fb);
        assert_eq!(pixels[0], 0xFF01_0203);
        assert_eq!(
            pixels[1], 0xFF04_0506,
            "must read the second row from the padded stride offset"
        );
    }

    #[test]
    fn render_rtgboard_never_panics_on_a_short_vram_slice() {
        let vram = [0u8; 2]; // far short of what a 4x4 RGBX_8888 frame needs
        let mut pixels = [0u32; 16];
        let mut fb = Framebuffer::new(&mut pixels, 4, 4).unwrap();
        render_rtgboard(4, 4, format::RGBX_8888, 16, 0, &vram, &mut fb); // must not panic
    }

    // ---- end-to-end through the real screenshot present path ---------------

    /// The brief's own ask: "if you can render the board's framebuffer
    /// through the existing screenshot path so a host-side test can prove
    /// a known pattern in VRAM comes out as pixels, that is worth
    /// having." No guest driver exists yet, so the mode here is committed
    /// directly through the register interface (exactly as a host-side
    /// test, not a guest, would) rather than by booting a ROM.
    #[test]
    fn a_mode_committed_through_the_register_interface_captures_through_the_real_present_path() {
        let mut chip_ram: Box<[u8; CHIP_RAM_SIZE]> = vec![0u8; CHIP_RAM_SIZE]
            .into_boxed_slice()
            .try_into()
            .unwrap();
        let rom = [0u8; 0];
        let mut vram = vec![0u8; 4 * 4 * 4];
        let modes = [ModeDescriptor {
            width: 4,
            height: 4,
            format: format::RGBX_8888,
        }];
        let mut bus = MachineBus::new(&mut chip_ram, &rom).with_rtgboard(&mut vram, &modes);
        let base = 0x4000_0000u32;
        // Configure the Zorro III base-address sequence directly -- the
        // same two-byte write this project's `autoconfig` module docs
        // describe, exercised the same way `machine-core`'s own bus tests
        // do (a host-side test standing in for `expansion.library`).
        bus.write_byte(
            machine_core::autoconfig::AUTOCONFIG_BASE
                + machine_core::autoconfig::ec::Z3_BASEADDRESS,
            (base >> 24) as u8,
        );
        bus.write_byte(
            machine_core::autoconfig::AUTOCONFIG_BASE
                + machine_core::autoconfig::ec::Z3_BASEADDRESS
                + 1,
            (base >> 16) as u8,
        );

        let write_u32 = |bus: &mut MachineBus, addr: u32, value: u32| {
            bus.write_byte(addr, (value >> 24) as u8);
            bus.write_byte(addr + 1, (value >> 16) as u8);
            bus.write_byte(addr + 2, (value >> 8) as u8);
            bus.write_byte(addr + 3, value as u8);
        };
        write_u32(&mut bus, base + machine_core::rtgboard::reg::SET_WIDTH, 4);
        write_u32(&mut bus, base + machine_core::rtgboard::reg::SET_HEIGHT, 4);
        bus.write_byte(
            base + machine_core::rtgboard::reg::SET_FORMAT + 3,
            format::RGBX_8888,
        );
        write_u32(&mut bus, base + machine_core::rtgboard::reg::SET_STRIDE, 16);
        write_u32(
            &mut bus,
            base + machine_core::rtgboard::reg::SET_FB_OFFSET,
            0,
        );
        bus.write_byte(base + machine_core::rtgboard::reg::COMMIT + 3, 0);
        assert_eq!(
            bus.read_byte(base + machine_core::rtgboard::reg::STATUS + 3),
            machine_core::rtgboard::status::APPLIED
        );

        // A known pattern: the top-left pixel bright red, everything else
        // black.
        let vram_addr = base + machine_core::rtgboard::VRAM_BASE;
        bus.write_byte(vram_addr, 0xFF); // R
        bus.write_byte(vram_addr + 1, 0x00); // G
        bus.write_byte(vram_addr + 2, 0x00); // B

        let target = std::env::temp_dir().join(format!(
            "rtgboard-screenshot-test-{}.png",
            std::process::id()
        ));
        let mut surface = PngSurface::new(target.clone());
        let stats = capture(&mut bus, &mut surface);

        assert_eq!(
            stats.distinct_colours, 2,
            "one red pixel against a black background"
        );
        assert_eq!(stats.non_background_pixels, 1);
        assert_eq!(stats.total_pixels, 16);

        let _ = std::fs::remove_file(&target);
    }

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
