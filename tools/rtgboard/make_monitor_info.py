#!/usr/bin/env python3
"""Build a from-scratch, license-clean rtgboard.info -- the Workbench icon
whose ToolTypes tell Picasso96/rtg.library to load rtgboard.card
(BOARDTYPE=rtgboard) as the monitor driver for this board.

Written from scratch (a plain classic DiskObject with a trivial original
2-plane icon) specifically so nothing here is derived from a third-party
monitor: the ToolTypes are ours, and the icon imagery below is a
hand-drawn box, not a copy of anyone's monitor icon. The DEVS:Monitors/
rtgboard *binary* monitor stub is a separate concern -- scripts/
patch-rtgboard-hdf.sh clones one from the image's own existing Graffity
monitor stub instead; this file supplies only the icon+ToolTypes half,
which is the only half that's actually ours. (Ported from amirfb's own
tools/make_monitor_info.py, BSD 2-Clause.)

Classic .info (icon.library GetDiskObject) on-disk format, big-endian:
  DiskObject(78) [ Magic, Version, Gadget(44), Type, pad, DefaultTool,
                   ToolTypes, CurrentX, CurrentY, DrawerData, ToolWindow,
                   StackSize ]
  then, when the matching pointer field is non-zero:
    Image(20) + planar image data   (Gadget.GadgetRender)
    ToolTypes block                 (DiskObject.ToolTypes)

Usage: make_monitor_info.py <output-path.info>
"""
import struct
import sys

MAGIC = 0xE310
WB_TOOL = 1
GFLG_GADGIMAGE = 0x0004
NO_ICON_POSITION = 0x80000000

ICON_W = 32
ICON_H = 22

# ToolTypes, in order. Active ones are bare KEY=VALUE; "inactive" examples
# are wrapped in parens (icon.library FindToolType then can't match the
# key, so they're documentation the user can enable by removing the
# parens in Workbench's Information window).
#
# BOARDTYPE=rtgboard is what makes rtg.library load LIBS:Picasso96/
# rtgboard.card for this monitor (docs/rtgboard-driver-scope.md §4).
# SettingsFile points it at our static settings file rather than the
# default Devs:Picasso96Settings location's implicit lookup -- being
# explicit here matches this board's whole mode-provisioning story (§3):
# there is exactly one blessed mode, and it lives in one named file.
# FakeNativeModes=Yes is the same mechanism the amibake Graffity image
# uses (tools/amibake/m68k-machine.toml's `fake-native-modes = true`
# comment: "replaces the chip-set screen modes with a 640x480 chunky
# mode on the board, so Workbench opens on RTG from first boot with no
# screenmode-prefs step" -- P96 forcibly enables this on hardware with
# no native Amiga output, which this machine is). Everything amirfb-
# specific (FBID/FBWIDTH/FBPIXEL/PASSWORD/PORT/MAXMEM -- all about a
# software framebuffer/RFB transport this board doesn't have) is
# dropped; BORDERBLANK/IGNOREMASK are kept as inactive documentation
# entries the same way amirfb's icon carries them.
TOOLTYPES = [
    "BOARDTYPE=rtgboard",
    "SettingsFile=SYS:Devs/Picasso96Settings",
    "FakeNativeModes=Yes",
    "(BORDERBLANK=Yes)",
    "(IGNOREMASK=Yes)",
]


def row_bytes(w):
    return ((w + 15) // 16) * 2


def icon_planes():
    """Two planes of a plain original icon: plane 0 a 1px border box with a
    diagonal, plane 1 a light interior fill -- deliberately generic, no
    resemblance to any real monitor icon."""
    rb = row_bytes(ICON_W)
    p0 = bytearray(rb * ICON_H)
    p1 = bytearray(rb * ICON_H)

    def setpx(plane, x, y):
        plane[y * rb + (x >> 3)] |= 0x80 >> (x & 7)

    for y in range(ICON_H):
        for x in range(ICON_W):
            edge = (x == 0 or y == 0 or x == ICON_W - 1 or y == ICON_H - 1)
            diag = (x == y) or (x == ICON_H - 1 - y)
            if edge or diag:
                setpx(p0, x, y)
            elif (x + y) & 3 == 0:
                setpx(p1, x, y)
    return bytes(p0) + bytes(p1)


def build_gadget():
    return struct.pack(
        ">IHHHHHHHIIIIIHI",
        0,              # ga_Next
        0, 0,           # ga_LeftEdge, ga_TopEdge
        ICON_W, ICON_H, # ga_Width, ga_Height
        GFLG_GADGIMAGE, # ga_Flags
        0x0003,         # ga_Activation (RELVERIFY|GADGIMMEDIATE)
        0x0001,         # ga_GadgetType (BOOLGADGET)
        0x0000000C,     # ga_GadgetRender  (non-zero -> Image follows)
        0,              # ga_SelectRender  (no alternate image)
        0,              # ga_GadgetText
        0,              # ga_MutualExclude
        0,              # ga_SpecialInfo
        0,              # ga_GadgetID
        0,              # ga_UserData
    )


def build_diskobject():
    do = struct.pack(">HH", MAGIC, 1)
    do += build_gadget()
    do += struct.pack(">Bx", WB_TOOL)          # do_Type + pad
    do += struct.pack(">I", 0)                 # do_DefaultTool (none)
    do += struct.pack(">I", 0x0000000C)        # do_ToolTypes (non-zero -> block follows)
    do += struct.pack(">ii", NO_ICON_POSITION - (1 << 32), NO_ICON_POSITION - (1 << 32))
    do += struct.pack(">I", 0)                 # do_DrawerData (not a drawer)
    do += struct.pack(">I", 0)                 # do_ToolWindow
    do += struct.pack(">I", 0)                 # do_StackSize
    assert len(do) == 78, len(do)
    return do


def build_image():
    planes = icon_planes()
    img = struct.pack(
        ">hhHHHIBBI",
        0, 0,           # LeftEdge, TopEdge
        ICON_W, ICON_H, # Width, Height
        2,              # Depth
        0x0000000C,     # ImageData (non-zero -> data follows)
        0x03,           # PlanePick (both planes)
        0x00,           # PlaneOnOff
        0,              # NextImage
    )
    assert len(img) == 20, len(img)
    return img + planes


def build_tooltypes():
    out = struct.pack(">I", (len(TOOLTYPES) + 1) * 4)
    for t in TOOLTYPES:
        b = t.encode("ascii") + b"\x00"
        out += struct.pack(">I", len(b)) + b
    return out


def build():
    return build_diskobject() + build_image() + build_tooltypes()


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    data = build()
    with open(sys.argv[1], "wb") as f:
        f.write(data)
    print(f"wrote {sys.argv[1]}: {len(data)} bytes, {len(TOOLTYPES)} tooltypes")
