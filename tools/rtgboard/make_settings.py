#!/usr/bin/env python3
"""Build the Picasso96Settings (FORM P96S) file for the `rtgboard` P96
`.card` driver (docs/rtgboard-driver-scope.md §3-§4, docs/
rtgboard-protocol.md).

WHY A STATIC SETTINGS FILE, NOT ResolutionsList: amirfb's own driver work
(its issues #2/#28/#44, confirmed against the official iComp reference
driver) found that a mode built as a `struct LibResolution`/`ModeInfo`
entry and added to `bi->ResolutionsList` in `InitCard` is enumerable and
openable -- but the `SetGC`/`SetPanning`/pixel-clock activation vectors
never fire for it. Only a mode sourced from `Devs:Picasso96Settings`
activates properly (the iComp SDK's own shipped reference driver has zero
`ResolutionsList` references; MNT's ZZ9000/VA2000 drivers ship a static
settings file in their installers). So this driver, like amirfb's, never
touches `ResolutionsList` and instead ships this generated file.

Format verified byte-for-byte against a real Picasso96Mode-written file
(amirfb's docs/p1.2-mode-registration-findings.md) and ported from
amirfb's own tools/make_settings.py (BSD 2-Clause, the project owner's
working generator, verified live against real Picasso96 rtg.library
40.3945).

board_type/RSHD active/MIHD timing defaults below are taken directly from
a real, shipped installer file -- MNT Research's ZZ9000/VA2000 driver
archives (ZZ9000Installer1_6.lha, Devs/Picasso96Settings) both ship a
STATIC settings block (not GUI-generated at install time) under the
BT_uaegfx (14) masquerade, with their own product name as BDNM (not
literally "uaegfx") -- direct real-world precedent for exactly our
situation (an unregistered/masqueraded BoardType needing a working
settings file). RSHD active=1/last_selected=2 and MIHD OpenCount=0/
Active=1 come from that same file.

ONE MODE ONLY: unlike amirfb (multiple resolutions, 8-bit CLUT + 16-bit
per resolution), this board has no palette hardware by design (docs/
rtgboard-protocol.md §11 -- ADR 0002 favours direct-colour formats over
CLUT to avoid the palette-expansion bug class this project already lived
through once). So there is exactly one MODES entry, 16-bit only, and the
multi-depth machinery below is kept (in case a second mode/format is ever
justified) but fed a single (16,) depth tuple.

COUPLING WARNING (docs/rtgboard-driver-scope.md §4): the one mode this
file describes must exactly match:
  1. this MODES table (640x480, depth 16 -> RGBFB_R5G6B5),
  2. the host's `--rtgboard WIDTHxHEIGHT` flag (currently 640x480), and
  3. the driver's own blessed/hardcoded mode in m68k/rtgboard-card/
     rtgboard_card.c (RGB_565, 640x480).
The board's own `COMMIT` register refuses any (width, height, format)
that isn't in ITS catalog (docs/rtgboard-protocol.md §7) -- so all three
places above must agree, or the driver advertises a mode the board will
reject at activation time. There is no code that enforces this agreement
automatically; keep it in sync by hand.

Usage: make_settings.py <output-path> [board-name]
"""
import struct
import sys


def chunk_exact(tag, body, real_len):
    """IFF chunk with a possibly-odd real body length (size field uses the
    unpadded length; the pad byte is appended but not counted)."""
    padded = body if real_len % 2 == 0 else body + b"\x00"
    return tag.encode("ascii") + struct.pack(">I", real_len) + padded


def sthd(board_type, local_ordering, last_selected, name_field):
    name = name_field.encode("ascii")[:29].ljust(30, b"\x00")
    body = struct.pack(">IHh", board_type, local_ordering, last_selected) + name
    assert len(body) == 38
    return chunk_exact("STHD", body, 38)


def bdnm(name):
    # BOARDNAMEMAXCHARS=30, +NUL, even-padded -> 32 observed.
    body = name.encode("ascii")[:31] + b"\x00"
    if len(body) % 2:
        body += b"\x00"
    return chunk_exact("BDNM", body, len(body))


def rshd(display_id, width, height, active, last_selected, flags, name):
    nm = name.encode("ascii")[:21].ljust(22, b"\x00")
    body = struct.pack(">IHHhhH", display_id, width, height, active,
                       last_selected, flags) + nm
    assert len(body) == 36
    return chunk_exact("RSHD", body, 36)


def mihd(width, height, depth, flags, hor_total, hor_blank, hor_sync_start,
         hor_sync_size, ver_total, ver_blank, ver_sync_start, ver_sync_size,
         pixel_clock, pll1=0, pll2=0):
    body = struct.pack(
        ">hhHHBBHHHHBBHHHHBBI",
        0, 1,                            # OpenCount, Active (real captured files: 1, not -1)
        width, height, depth, flags,
        hor_total, hor_blank, hor_sync_start, hor_sync_size, 0, 0,
        ver_total, ver_blank, ver_sync_start, ver_sync_size, pll1, pll2,
        pixel_clock,
    )
    assert len(body) == 34, len(body)
    return chunk_exact("MIHD", body, 34)


def anno(text):
    body = ("\0" + text + "\0").encode("ascii")
    if len(body) % 2:
        body += b"\x00"
    return chunk_exact("ANNO", body, len(body))


# Exactly one entry: this board's one blessed mode. See the COUPLING
# WARNING in the module docstring above -- this table, the host's
# --rtgboard flag, and rtgboard_card.c's own hardcoded mode must all
# agree, or the board's COMMIT register refuses the mode at activation.
#
# depths is kept as a tuple (amirfb's multi-depth-per-resolution
# machinery below iterates it and builds one MIHD per entry) but holds
# only 16 here -- no 8-bit/CLUT entry, because this board has no palette
# hardware by design (docs/rtgboard-protocol.md §11). DisplayID
# 0x60001000 is arbitrary but namespaced the same way amirfb's bases are
# (0x6000_1000 family); nothing else claims it on this machine.
#
# CRTC placeholder shape (HorTotal/VerTotal = dimension+8, trivial
# 2-pixel sync, no real blanking) and flags=0x18 (GMF_HPOLARITY|
# GMF_VPOLARITY) are copied from amirfb's own generator, which confirmed
# them live against real Picasso96 -- not a guess. PixelClock reuses
# amirfb's width*height*60 formula so this file and the driver's own
# ResolvePixelClock/GetPixelClock vectors (once written) never disagree.
MODES = (
    # (width, height, depths, display_id)
    (640, 480, (16,), 0x60001000),
)


def build(board_name, monitor_name="rtgboard", board_type=14):
    """board_type defaults to 14 (BT_uaegfx) -- a deliberate masquerade,
    not a real assigned BoardType. Same posture as this project's own
    0x07DB manufacturer ID stand-in (docs/rtgboard-protocol.md §1): a
    real registered BoardType is needed before this ships on hardware.
    ADR 0002 notes stock rtg.library's behaviour with an unassigned
    BoardType is unknown, so this copies amirfb's (and MNT's ZZ9000/
    VA2000's) proven masquerade rather than risk that (docs/
    rtgboard-driver-scope.md §5.1)."""
    body = b"P96S"
    body += anno("$VER: rtgboard make_settings.py 0.1")
    body += sthd(board_type=board_type, local_ordering=0, last_selected=1,
                name_field=monitor_name)
    body += bdnm(board_name)

    for width, height, depths, display_id in MODES:
        body += rshd(display_id=display_id, width=width, height=height,
                    active=1, last_selected=2, flags=0x0002,
                    name=f"{board_name}:{width}x{height}")
        for depth in depths:
            body += mihd(width=width, height=height, depth=depth, flags=0x18,
                        hor_total=width + 8, hor_blank=0, hor_sync_start=2,
                        hor_sync_size=2, ver_total=height + 8, ver_blank=0,
                        ver_sync_start=2, ver_sync_size=2,
                        pixel_clock=width * height * 60, pll1=0, pll2=1)

    return b"FORM" + struct.pack(">I", len(body)) + body


if __name__ == "__main__":
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    out_path = sys.argv[1]
    # board_name is only the DISPLAYED board/mode name (BDNM + the
    # "<name>:640x480" RSHD string ScreenMode shows) -- NOT the
    # masquerade, which is board_type=14 (BT_uaegfx) in build(). P96
    # matches the settings file to the board by type, not name.
    board_name = sys.argv[2] if len(sys.argv) > 2 else "rtgboard"
    data = build(board_name)
    with open(out_path, "wb") as f:
        f.write(data)
    print(f"wrote {out_path}: {len(data)} bytes, board={board_name!r}")
