# Emu68 design writeups — notes and what they change here

**Provenance and licensing.** Everything below comes from Michal
Schulz's public writing about Emu68 — Patreon posts ("What's new in
Emu68 v1.1" parts 1–2, "A lot of news", "Emu68 1.0"), the v1.1.0-alpha.1
GitHub release notes, and press coverage (amiga-news.de,
generationamiga.com, linuxjedi.co.uk) — gathered 2026-09-30. **No Emu68
source was read.** Emu68 is MPL-2.0 and remains ideas-only under the
project firewall (CLAUDE.md, proposal §7); an author's published
description of his own design is an idea in exactly the sense §7
already uses. Quotes below are from those writeups, not from code.

## 1. The "Dumpster": invalidation ≠ destruction  — bears on §4.7 open item 2

> "Dumpster is a new way of destroying and re-using JIT instruction
> caches in Emu68. Instead of throwing them away right after cache
> flush, they are reused later if the memory of the JIT block has not
> changed."

On a flush, translated blocks are set aside rather than freed; on
next lookup they are revived "after a short verification of a
fingerprint and crc32 checksum" of the source memory. Motivation:
exactly the thrash our §4.7 open item 2 predicts — guests that flush
constantly (classic MacOS under ShapeShifter flushes "many tens of
times per second"; AmigaOS `LoadSeg` flushes on every program load).
Measured effect he reports: macOS 8.1 boot 84 s → 28 s with the
feature on; release notes claim "up to 2x" boot speedup generally.

**What it changes here:** §4.7's cheap flush-everything answer and the
expensive reverse-index answer are not the only options. A third —
*logical* flush with revalidate-on-reuse — is proven in production on
precisely the workload we worried about, and its revalidation
primitive (compare source bytes against a stored fingerprint) is the
same one our fork's trace JIT and the m68k-vp PoC already use on
lookup. Cost profile: no reverse index, no per-write tracking; pay a
checksum on first re-entry after a flush.

## 2. Two-level translation lookup, hot/cold node split — bears on §4.7 lookup design

v1.1 puts the "hot" part of each JIT node — list pointers, m68k
address, host address — in one cache-line-aligned L1 line, and adds a
**4-way set-associative cache (128 sets)** mapping m68k entry address →
host entry address in front of the main hash table. Reported gain:
"negligible to 20–30%" by workload, best on branchy 3D code
(Heretic II). Cheap to specify now for C3: small direct-mapped/
set-associative front cache, hash table behind it, hot fields packed
first.

## 3. Interrupt discovery off the hot path — endorses §4.5/ADR 0006 shape

On PiStorm the IPL line lives across slow GPIO. Moving IPL0 sampling
to a second host core — the JIT loop just tests a flag another core
maintains — "doubled its performance". Our situation differs (deriving
IPL from device state is an in-process read, and max mode already
syncs at batch boundaries), so the direct win is smaller here; the
transferable rule is the shape: **the translated hot loop should only
ever test a cheap already-computed flag**, never compute interrupt
state itself. That is §4.5's block-boundary `checkirq` design; if
device→IPL derivation ever grows expensive, publishing it from a host
thread is the proven escape.

## 4. Return-stack inlining, and its Amiga-specific trap — bears on §4.5 call handling

Emu68 inlines short subroutines using a return stack. First version
"was working great on short test cases but failed with AmigaOS. For
one simple reason - as soon as return address was modified on the real
stack, the generated code failed." The fix keeps the return stack but
verifies against the real guest stack. Lesson recorded for C3: AmigaOS
*does* rewrite return addresses on the live stack (Exec exception
handling, task switching, application tricks), so any RTS-prediction
or call-inlining must validate the guest stack value before using a
predicted target. Our PoC's worst cold-decode number was exactly call
fragmentation (`jsr_rts_chain`, vp-cold 42 M/s), so this is the
optimisation we will eventually want, with its known failure mode
attached in advance.

## 5. Runtime optimisation toggles

EmuControl exposes switches for condition-code-flag optimisation and
inlining range at runtime. Same instinct as our "liveness pass
switchable off once trusted" and the direct map's `--direct-map
{auto,on,off}`: every speed trick ships with a kill switch, which is
also what made his return-stack and flag bugs findable.

## 6. PPC second front end is real, not rumoured

v1.1-alpha ships "an experimental second instance translating PowerPC
to AArch64", disabled by default. Proposal §11's PPC-coprocessor note
cited "PiStorm is reported to be adding PPC support"; that is now
confirmed shipping-but-disabled, and remains worth watching for
kernel/board-interface precedent.

## Sources

- https://www.patreon.com/michal_schulz/posts/whats-new-in-v1-137628236 (v1.1 part 1)
- https://www.patreon.com/posts/whats-new-in-v1-137864826 (v1.1 part 2, Dumpster)
- https://github.com/michalsc/Emu68/releases/tag/v1.1.0-alpha.1
- https://www.patreon.com/posts/lot-of-news-57245175 (IPL second core, return stack)
- https://www.amiga-news.de/en/news/AN-2025-08-00119-EN.html (Dumpster demo, 84s→28s)
- https://www.generationamiga.com/2026/02/11/emu68-alpha-update-marks-major-step-forward-in-emulator-performance/
