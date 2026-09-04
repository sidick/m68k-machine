/* Phase 0 toolchain smoke test.
 *
 * This is NOT the start of the real m68k-side stack -- per
 * docs/combined-roadmap.md, the m68k stack (drivers, boot ROMs, DiagArea
 * modules -- code that runs *on* the emulated 68k) is planned to live in
 * its own repository, not yet created. This file exists solely so CI can
 * prove the amiga-gcc cross-toolchain container actually produces a
 * working AmigaOS m68k executable (hunk format, magic 0x000003F3),
 * before that real stack exists to build.
 *
 * Deliberately trivial and dependency-free: no library calls, so it
 * builds the same whether linked with ixemul or -noixemul semantics.
 */
int main(void)
{
    return 0;
}
