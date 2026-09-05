/* Linker script for the board-qemu-virt Phase 0 payload.
 *
 * Targets QEMU's `virt` machine (`-M virt`): RAM starts at 0x4000_0000.
 * The image is linked at 0x4008_0000 (512 KB into RAM, well clear of any
 * firmware/DTB QEMU may also place low in RAM) and loaded there directly
 * by `-kernel <elf>` (QEMU's aarch64 ELF loader honours the ELF entry
 * point and section addresses; no relocation or MMU setup is needed since
 * this payload runs with the MMU off, as QEMU virt starts at EL1).
 */

ENTRY(_start)

MEMORY
{
  /* 8M was enough for the Phase 0 bus-only payload. Booting AROS embeds
   * both ~512 KB ROM images via `include_bytes!` (.rodata) and adds a
   * third 2 MB chip-RAM `.bss` buffer for the boot run alongside the two
   * Phase 0 checks already keep, which alone pushed total image+.bss
   * past 8M, hence the earlier bump to 16M.
   *
   * Display bring-up (`ramfb.rs`/`present.rs`) adds three more `.bss`
   * buffers: a 2 MB scratch copy of chip RAM (reconstructed through the
   * bus's own read path rather than aliasing the boot run's `&mut`
   * borrow of `BOOT_CHIP_RAM` -- see `main.rs`'s `snapshot_chip_ram` doc
   * comment), a ~1.65 MB staging canvas
   * (`machine_core::display::MAX_WIDTH * MAX_HEIGHT` `u32`s), and a
   * ~7.3 MB host framebuffer at `ramfb::HOST_WIDTH * HOST_HEIGHT` (see
   * that constant's doc comment for why that resolution). Total usage is
   * now ~19-20M; QEMU virt is configured with 512M (`-m 512M` in
   * scripts/run-qemu-virt.sh), so 32M leaves generous headroom without
   * reserving a meaningful fraction of real guest-visible RAM. */
  RAM (rwx) : ORIGIN = 0x40080000, LENGTH = 32M
}

SECTIONS
{
  . = ORIGIN(RAM);

  .text : {
    KEEP(*(.text._start))
    *(.text .text.*)
  } > RAM

  .rodata : {
    *(.rodata .rodata.*)
  } > RAM

  .data : {
    *(.data .data.*)
  } > RAM

  .bss (NOLOAD) : ALIGN(8) {
    __bss_start = .;
    *(.bss .bss.*)
    *(COMMON)
    . = ALIGN(8);
    __bss_end = .;
  } > RAM

  /* 1 MB stack, immediately after .bss; grows down from __stack_top.
   * Was 64 KB in the Phase 0 bus-only payload; that overflowed silently
   * (no MMU guard page on this MMU-off payload, so overflow corrupts
   * whatever memory follows rather than faulting) once this crate started
   * calling into the `m68k` fork's interpreter, whose decode/dispatch and
   * FPU paths carry much deeper, unoptimized-debug-build stack frames
   * than the bus-only checks ever needed. */
  . = ALIGN(16);
  . = . + 0x100000;
  __stack_top = .;

  /DISCARD/ : {
    *(.comment)
    *(.eh_frame)
    *(.note.*)
    *(.ARM.attributes)
  }
}
