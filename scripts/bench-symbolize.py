#!/usr/bin/env python3
"""Symbolize a samply profile against the machine-hosted binary.

WHY THIS EXISTS: `samply record` writes its "processed profile" JSON with
addresses relative to the loaded binary, not symbol names -- useful for
samply's own viewer, useless for diffing two profiles by eye or grepping
for a function name in CI output. This script is the aggregator used for
the numbers in docs/bus-fast-path-plan.md §0: self time and inclusive
time per function, as percentages of total samples, computed once and
committed so before/after profiles in that plan are comparable by anyone
without opening the samply UI.

Only stdlib is used deliberately (docs/bus-fast-path-plan.md step 1):
this runs on whatever Python happens to be on a dev machine, no venv.

Symbol names for the main binary's own frames are resolved with macOS's
`atos`; frames outside the main binary (libsystem, the kernel, etc.) are
reported as "[libname]" since they are never the thing being optimized
here. `atos` is macOS-only -- this script has no Linux equivalent today,
matching every other tool in this repo's set (all developed on macOS).

The demangler in `clean()` is a crude, approximate Rust v0 demangler: it
strips `s..._`/`B..._` disambiguator and backref tokens and reads
`<len><ident>` path segments, but it does not implement the full v0
grammar (no generics, no closures, no full backref resolution). Good
enough to turn `_RNvNtCs...9machine_core...read_byte` into a readable
`MachineBus::read_byte`-shaped string; not a substitute for `rustfilt`
on anything precise.
"""

import argparse
import collections
import gzip
import json
import re
import shutil
import subprocess
import sys

# Function-name components that are noise once a symbol has been split
# on "::" -- the crate/module path segments that show up in nearly every
# frame and add nothing beyond what the leaf function name already says.
_NOISE_SEGMENTS = {
    "machine_hosted",
    "machine_core",
    "m68k",
    "core",
    "cpu",
    "memory",
    "Bus",
    "CpuCore",
    "MachineBus",
    "execute",
    "bus",
}


def clean(sym: str) -> str:
    """Best-effort demangle + noise-strip of one atos-resolved symbol.

    `sym` looks like `_RNvNtCs.../read_byte (in machine-hosted) + 123`
    for a Rust v0 mangled name, or an already-plain name (C symbols,
    `atos` giving up) that passes through unchanged (minus the
    "(in ...)" suffix atos appends).
    """
    sym = sym.split(" (in ")[0]
    if not sym.startswith("_R"):
        return sym
    i = 2
    parts = []
    while i < len(sym):
        c = sym[i]
        if c in "sB":
            # Disambiguator (`s..._`) or backref (`B..._`) token: skip to
            # the terminating `_` and move on. Approximate -- a real v0
            # demangler resolves `B` backrefs to an earlier position
            # instead of discarding them.
            j = sym.find("_", i)
            if j < 0:
                break
            i = j + 1
            continue
        if c.isdigit():
            m = re.match(r"[0-9]+", sym[i:])
            n = int(m.group())
            i += len(m.group())
            if i < len(sym) and sym[i] == "u":
                # v0 "disambiguated identifier" (unicode) marker.
                i += 1
            parts.append(sym[i : i + n])
            i += n
            continue
        i += 1
    parts = [p for p in parts if p not in _NOISE_SEGMENTS]
    return "::".join(parts) or sym


def resolve_arch(libs, main_res_lib_index):
    """Pick the `atos -arch` value for the main binary from the profile.

    Falls back to `arm64` (this project's dev machines are all Apple
    Silicon) if the profile doesn't carry an explicit arch string, rather
    than failing -- the plan explicitly allows hardcoding this if
    detecting it isn't easy, so a missing field is not an error.
    """
    try:
        arch = libs[main_res_lib_index].get("arch")
        if arch:
            return arch
    except (IndexError, AttributeError, TypeError):
        pass
    return "arm64"


def main() -> int:
    ap = argparse.ArgumentParser(
        description="Self/inclusive time per function from a samply profile."
    )
    ap.add_argument("profile", help="path to samply's <name>.json.gz")
    ap.add_argument("binary", help="path to the binary the profile was recorded against")
    ap.add_argument("--top", type=int, default=30, help="how many functions to list per table (default: 30)")
    ap.add_argument(
        "--filter",
        default=None,
        help="only list inclusive-time rows whose function name contains this substring",
    )
    args = ap.parse_args()

    if shutil.which("atos") is None:
        print(
            "error: `atos` not found on PATH -- symbolization needs macOS's atos "
            "(Xcode command line tools); this script has no substitute for it.",
            file=sys.stderr,
        )
        return 1

    with gzip.open(args.profile) as f:
        prof = json.load(f)

    thread = prof["threads"][0]
    stack_table = thread["stackTable"]
    frame_table = thread["frameTable"]
    func_table = thread["funcTable"]
    resource_table = thread["resourceTable"]
    libs = prof.get("libs", [])

    # Find the resource index of the main binary by name, rather than
    # assuming it is always resource 1 -- samply's resource ordering is
    # not a stable contract this script should depend on.
    binary_basename = args.binary.rsplit("/", 1)[-1]
    main_res = None
    resource_names = resource_table.get("name", [])
    for res_idx, name_str_idx in enumerate(resource_names):
        # `name` in resourceTable is an index into the thread's
        # stringTable in some samply versions and a literal string in
        # others; handle both.
        name = name_str_idx
        if isinstance(name, int):
            name = thread["stringArray"][name]
        if name and binary_basename in name:
            main_res = res_idx
            break
    if main_res is None:
        print(
            f"error: no resource in this profile matches binary name '{binary_basename}'",
            file=sys.stderr,
        )
        return 1

    lib_for_resource = resource_table.get("lib", [])

    def libname_for_resource(res_idx: int) -> str:
        try:
            lib_idx = lib_for_resource[res_idx]
            if lib_idx is not None and 0 <= lib_idx < len(libs):
                return libs[lib_idx].get("name", "?")
        except (IndexError, TypeError):
            pass
        return "?"

    arch = resolve_arch(libs, lib_for_resource[main_res] if main_res < len(lib_for_resource) else None)

    addrs = sorted(
        {
            frame_table["address"][i]
            for i in range(frame_table["length"])
            if func_table["resource"][frame_table["func"][i]] == main_res
        }
    )
    if not addrs:
        print("error: no frames from the main binary in this profile", file=sys.stderr)
        return 1

    # samply's `address` is an offset relative to the image; atos wants
    # a load address, and 0x100000000 is the fixed Mach-O load base atos
    # assumes for a non-running arm64 binary given via -o (not the real
    # ASLR slide, which atos does not need for a static lookup like this).
    load_base = 0x100000000
    atos_out = subprocess.run(
        ["atos", "-o", args.binary, "-arch", arch] + [hex(load_base + a) for a in addrs],
        capture_output=True,
        text=True,
        check=False,
    ).stdout.splitlines()

    if len(atos_out) != len(addrs):
        print(
            f"error: atos returned {len(atos_out)} lines for {len(addrs)} addresses -- "
            "binary likely doesn't match the profile",
            file=sys.stderr,
        )
        return 1

    sym = {a: clean(o) for a, o in zip(addrs, atos_out)}

    def fname(frame_idx: int) -> str:
        func_idx = frame_table["func"][frame_idx]
        if func_table["resource"][func_idx] == main_res:
            return sym.get(frame_table["address"][frame_idx], "?")
        return "[" + libname_for_resource(func_table["resource"][func_idx]) + "]"

    self_count = collections.Counter()
    incl_count = collections.Counter()
    samples = thread["samples"]
    n_samples = samples["length"]

    for stack_idx in samples["stack"]:
        if stack_idx is None:
            continue
        seen = set()
        first = True
        cur = stack_idx
        while cur is not None:
            f = fname(stack_table["frame"][cur])
            if first:
                self_count[f] += 1
                first = False
            if f not in seen:
                incl_count[f] += 1
                seen.add(f)
            cur = stack_table["prefix"][cur]

    print(f"samples: {n_samples}")
    print()
    print(f"top {args.top} by self time:")
    for name, count in self_count.most_common(args.top):
        print(f"  {count / n_samples * 100:6.2f}%  {name}")
    print()

    incl_items = incl_count.most_common()
    if args.filter:
        incl_items = [(n, c) for n, c in incl_items if args.filter in n]
    print(f"top {args.top} by inclusive time" + (f" (filtered on '{args.filter}')" if args.filter else "") + ":")
    for name, count in incl_items[: args.top]:
        print(f"  {count / n_samples * 100:6.2f}%  {name}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
