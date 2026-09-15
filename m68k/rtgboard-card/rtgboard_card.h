/*
 * rtgboard_card.h -- constants for rtgboard.card, the Picasso96 driver for
 * the `rtgboard` device (docs/rtgboard-protocol.md).
 *
 * Increment 1 (build skeleton): register offsets, pixel format values,
 * STATUS bits and board identity only. No BoardInfo vector wiring, no
 * per-board state layout yet -- those arrive in increment 2.
 */

#ifndef RTGBOARD_CARD_H
#define RTGBOARD_CARD_H

/* ---- register offsets (docs/rtgboard-protocol.md SS4) --------------------
 * One hot byte per 4-byte-aligned slot, u32 registers as four big-endian
 * byte lanes. Anything not listed reads 0 and discards writes. */

#define RTG_REG_MODE_COUNT    0x00  /* u32, R  -- number of catalog entries */
#define RTG_REG_MODE_INDEX    0x04  /* byte, W -- selects catalog[] entry */
#define RTG_REG_MODE_WIDTH    0x08  /* u32, R  -- catalog[MODE_INDEX].width */
#define RTG_REG_MODE_HEIGHT   0x0C  /* u32, R  -- catalog[MODE_INDEX].height */
#define RTG_REG_MODE_FORMAT   0x10  /* byte, R -- catalog[MODE_INDEX].format */
#define RTG_REG_SET_WIDTH     0x14  /* u32, RW -- candidate width, scratch */
#define RTG_REG_SET_HEIGHT    0x18  /* u32, RW -- candidate height */
#define RTG_REG_SET_FORMAT    0x1C  /* byte, RW -- candidate pixel format (SS6) */
#define RTG_REG_SET_STRIDE    0x20  /* u32, RW -- candidate stride, bytes */
#define RTG_REG_SET_FB_OFFSET 0x24  /* u32, RW -- candidate fb offset in VRAM */
#define RTG_REG_COMMIT        0x28  /* byte, W -- validate + apply SET_* (SS7) */
#define RTG_REG_STATUS        0x2C  /* byte, RW -- write-1-to-clear; see bits below */
#define RTG_REG_CUR_WIDTH     0x30  /* u32, R  -- currently applied width */
#define RTG_REG_CUR_HEIGHT    0x34  /* u32, R  -- currently applied height */
#define RTG_REG_CUR_FORMAT    0x38  /* byte, R -- currently applied format */
#define RTG_REG_CUR_STRIDE    0x3C  /* u32, R  -- currently applied stride */
#define RTG_REG_CUR_FB_OFFSET 0x40  /* u32, R  -- currently applied fb offset */
#define RTG_REG_VRAM_BYTES    0x44  /* u32, R  -- total attached VRAM; read, don't hardcode */
#define RTG_REG_VERSION       0x48  /* u32, R  -- protocol version (1 for this doc) */

/* VRAM aperture starts here, within the same 16 MB AUTOCONFIG window. */
#define RTG_VRAM_BASE_OFFSET  0x100000

/* The whole Zorro III AUTOCONFIG window (docs/rtgboard-protocol.md SS1) --
 * needed here only to derive how much of VRAM_BYTES is actually reachable
 * through this one aperture (RTG_VRAM_BASE_OFFSET onward): at most
 * RTG_WINDOW_BYTES - RTG_VRAM_BASE_OFFSET (0x00F00000, 15 MB), never the
 * board's full VRAM_BYTES if that happens to be larger. */
#define RTG_WINDOW_BYTES      0x01000000

/* ---- pixel formats (docs/rtgboard-protocol.md SS6) ---------------------- */

#define RTG_FMT_RGB_565    0     /* 16 bpp, big-endian RRRRRGGGGGGBBBBB */
#define RTG_FMT_RGBX_8888  1     /* 32 bpp, low->high address = R,G,B,pad */
#define RTG_FMT_BGRX_8888  2     /* 32 bpp, low->high address = B,G,R,pad */
#define RTG_FMT_INVALID    0xFF  /* sentinel only -- never a real mode format */

/* ---- STATUS bits (docs/rtgboard-protocol.md SS4/SS7) --------------------
 * write-1-to-clear; mutually exclusive per commit. */

#define RTG_STATUS_REJECTED (1 << 0)
#define RTG_STATUS_APPLIED  (1 << 1)

/* ---- board identity (docs/rtgboard-protocol.md SS1) ---------------------
 * Matched against FindConfigDev()'s own results, never against a hardcoded
 * memory address -- AUTOCONFIG places the board, the driver only asks the
 * bus (see the project's "no hardcoded memory addresses" discipline). */

#define RTG_BOARD_MANUFACTURER 0x07DB  /* er_Manufacturer, docs SS1 */
#define RTG_BOARD_PRODUCT      4       /* er_Product, docs SS1 */

/* ---- the one blessed mode (increment 2 scope) ---------------------------
 * A three-way coupling, kept in sync BY HAND across all three (no shared
 * code links them -- same discipline amirfb_card.c's g_modes[] documents
 * for its own settings-file/driver split):
 *   1. this driver (SetGC/SetPanning/ResolvePixelClock/GetPixelClock all
 *      assume 640x480 RGB_565 is the mode that will be requested);
 *   2. the host's `--rtgboard WIDTHxHEIGHT` flag (machine-hosted), which
 *      builds the one-entry catalog this board's COMMIT validates against;
 *   3. tools/rtgboard/make_settings.py, whose
 *      Devs:Picasso96Settings entry is what actually gets P96 to request
 *      this geometry in the first place.
 * The board's own COMMIT (docs/rtgboard-protocol.md SS7 check 1) refuses
 * any (width, height, format) that isn't an exact catalog entry -- so a
 * mismatch here doesn't corrupt anything, it just means COMMIT rejects
 * every mode this driver ever proposes. */
#define RTG_BLESSED_WIDTH  640
#define RTG_BLESSED_HEIGHT 480

/* Single source of truth for this board's PixelClock formula, shared by
 * ResolvePixelClock/GetPixelClock (amirfb's AMIRFB_MODE_PIXELCLOCK,
 * issue #51: two independent copies of "width*height*60" drifted apart
 * once a driver had more than one mode -- this board only ever has one,
 * but there's no reason to invite the same mistake). Whatever generates
 * a Devs:Picasso96Settings entry for this board (tools/rtgboard/
 * make_settings.py) must use the identical formula for
 * its own MIHD pixel-clock field, by hand, same coupling as above. */
#define RTG_PIXELCLOCK(w, h) ((ULONG)(w) * (ULONG)(h) * 60)

#endif /* RTGBOARD_CARD_H */
