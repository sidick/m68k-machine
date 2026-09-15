#!/usr/bin/env python3
"""Build ENVARC:Sys/ScreenMode.prefs steering Workbench onto the rtgboard
board's one blessed mode (640x480, 16-bit R5G6B5).

WHY THIS FILE EXISTS (first-light finding, 2026-09-15): the first
end-to-end boot of rtgboard.card used FakeNativeModes=Yes on the monitor
icon -- the same mechanism the amibake Graffity image uses to get
Workbench onto RTG with no prefs step. On this board that produced a
green-on-black, half-width-doubled desktop: FakeNativeModes fabricates
native-substitute modes as 8-bit CLUT screens, Graffity has palette
hardware for that, and this board deliberately has none
(docs/rtgboard-protocol.md SS11) -- so P96's CPU renderer drew 1-byte
chunky pen indices into VRAM the board scanned out as 16-bit RGB565.
The fix is to steer Workbench onto the real 16-bit mode explicitly with
this prefs file, and not to advertise fake native modes at all
(tools/rtgboard/make_monitor_info.py keeps FakeNativeModes inactive).

FORMAT: byte-for-byte the shape of amirfb's known-good template
(tools/screenmode-amirfb-800x600-16bit.prefs -- written by the OS's own
ScreenMode Prefs editor "Save", not hand-built): FORM/PREF containing
PRHD (6 zero bytes) and SCRM (struct ScreenModePrefs: 4 reserved ULONGs,
DisplayID, Width=0xFFFF, Height=0xFFFF meaning "the mode's own default
dimensions", Depth, Control=1).

DISPLAYID: P96 computes a format-specific DisplayID on top of the
settings file's RSHD base -- the low 16 bits are a format code
(amirfb's reference-rtg-gotchas.md: 0x1000 = 8-bit CLUT, 0x1100 family =
16-bit, 0x1302 = 32-bit R8G8B8A8). For amirfb's identical case (RSHD
base 0x60001000, HICOLOR R5G6B5) the OS-written value was 0x60001102;
this board's settings file (tools/rtgboard/make_settings.py) uses the
same base and the same one 16-bit format, so the same computed ID is
used here. If P96 ever computes a different ID, the failure mode is
loud, not silent: Workbench falls back to a native planar mode, no
SetGC/COMMIT ever fires on the board, and the screenshot path captures
752x576 planar output instead of this board's 640x480 -- wrong
dimensions, immediately visible in the e2e assertions.

COUPLING: the DisplayID base and geometry here must agree with
tools/rtgboard/make_settings.py's MODES table (and everything in ITS
coupling warning). Keep them in sync by hand.

Usage: make_screenmode_prefs.py <output-path> [display-id-hex]
"""
import struct
import sys

RTG_DISPLAY_ID = 0x60001102  # base 0x60001000 (make_settings.py) + P96's
                             # computed 16-bit format code, see docstring
RTG_DEPTH = 16


def chunk(tag, body):
    padded = body if len(body) % 2 == 0 else body + b"\x00"
    return tag.encode("ascii") + struct.pack(">I", len(body)) + padded


def build(display_id, depth):
    prhd = chunk("PRHD", b"\x00" * 6)
    scrm = chunk("SCRM", struct.pack(
        ">IIIIIHHHH",
        0, 0, 0, 0,          # smp_Reserved[4]
        display_id,
        0xFFFF, 0xFFFF,      # Width/Height: the mode's own defaults
        depth,
        0x0001,              # Control (matches the OS-written template)
    ))
    body = b"PREF" + prhd + scrm
    return b"FORM" + struct.pack(">I", len(body)) + body


if __name__ == "__main__":
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    display_id = int(sys.argv[2], 16) if len(sys.argv) > 2 else RTG_DISPLAY_ID
    data = build(display_id, RTG_DEPTH)
    with open(sys.argv[1], "wb") as f:
        f.write(data)
    print(f"wrote {sys.argv[1]}: {len(data)} bytes, "
          f"DisplayID=0x{display_id:08x}, depth={RTG_DEPTH}")
