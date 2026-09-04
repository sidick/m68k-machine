//! The display surface a board layer provides, and the pixel format the
//! renderer writes into it.
//!
//! `machine-core` has no allocator, so it never owns a framebuffer: the
//! board layer (or the hosted runner) supplies the storage and this
//! crate fills it. That keeps the same renderer usable behind
//! virtio-gpu/ramfb under QEMU, a GOP framebuffer on x86 bare metal, and
//! a PNG writer in the hosted runner, without the renderer knowing which
//! it is talking to (proposal §8).

/// Widest picture the stop-gap renderer will produce: hires with a
/// generous overscan allowance.
pub const MAX_WIDTH: usize = 752;
/// Tallest picture the stop-gap renderer will produce: PAL interlaced
/// with overscan.
pub const MAX_HEIGHT: usize = 576;

/// A borrowed 32-bit framebuffer the renderer draws into.
///
/// Pixels are `0xAARRGGBB` with alpha always `$FF`. That costs a little
/// memory over a paletted surface but means a board layer can hand the
/// buffer straight to virtio-gpu, GOP or a PNG encoder without a
/// conversion pass, which is the common case for every backend this
/// project has.
pub struct Framebuffer<'a> {
    pub pixels: &'a mut [u32],
    pub width: usize,
    pub height: usize,
}

impl<'a> Framebuffer<'a> {
    /// Wrap caller-owned storage. Returns `None` if `pixels` is too
    /// small for the stated geometry, so a board layer cannot quietly
    /// hand over a buffer the renderer would run off the end of.
    pub fn new(pixels: &'a mut [u32], width: usize, height: usize) -> Option<Self> {
        if width == 0 || height == 0 || pixels.len() < width * height {
            return None;
        }
        Some(Self {
            pixels,
            width,
            height,
        })
    }

    pub fn put(&mut self, x: usize, y: usize, argb: u32) {
        if x < self.width && y < self.height {
            self.pixels[y * self.width + x] = argb;
        }
    }

    pub fn fill(&mut self, argb: u32) {
        for p in self.pixels.iter_mut() {
            *p = argb;
        }
    }
}

/// Expand a 12-bit Amiga colour register (`$0RGB`) to `0xAARRGGBB`.
///
/// Each 4-bit gun is replicated into both nibbles of its byte rather
/// than shifted left, so `$F` becomes `$FF` and full-intensity white is
/// actually white — shifting alone would cap every gun at `$F0` and give
/// the whole picture a visible dark cast.
pub const fn argb_from_amiga(color: u16) -> u32 {
    let r = ((color >> 8) & 0x0F) as u32;
    let g = ((color >> 4) & 0x0F) as u32;
    let b = (color & 0x0F) as u32;
    0xFF00_0000 | (r * 0x11) << 16 | (g * 0x11) << 8 | (b * 0x11)
}

/// Where a rendered frame goes once it exists.
///
/// Implemented by each board layer and by the hosted runner. `present`
/// is called at most once per emulated frame, from whatever context owns
/// the renderer.
pub trait DisplaySurface {
    /// The geometry this surface wants. The renderer clips to it.
    fn dimensions(&self) -> (usize, usize);

    /// Hand over a completed frame. `pixels` is `width * height` in
    /// `0xAARRGGBB`, borrowed only for the duration of the call.
    fn present(&mut self, pixels: &[u32], width: usize, height: usize);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_intensity_is_actually_white() {
        assert_eq!(argb_from_amiga(0x0FFF), 0xFFFF_FFFF);
    }

    #[test]
    fn black_stays_black() {
        assert_eq!(argb_from_amiga(0x0000), 0xFF00_0000);
    }

    #[test]
    fn guns_land_in_the_right_bytes() {
        assert_eq!(argb_from_amiga(0x0F00), 0xFFFF_0000);
        assert_eq!(argb_from_amiga(0x00F0), 0xFF00_FF00);
        assert_eq!(argb_from_amiga(0x000F), 0xFF00_00FF);
    }

    #[test]
    fn framebuffer_rejects_undersized_storage() {
        let mut pixels = [0u32; 16];
        assert!(Framebuffer::new(&mut pixels, 4, 4).is_some());
        assert!(Framebuffer::new(&mut pixels, 8, 4).is_none());
        assert!(Framebuffer::new(&mut pixels, 4, 0).is_none());
    }
}
