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
   * Phase 0 checks already keep, which alone pushes total image+.bss
   * past 8M. QEMU virt is configured with 512M (`-m 512M` in
   * scripts/run-qemu-virt.sh), so 16M leaves generous headroom over the
   * ~9M actually used without reserving a meaningful fraction of real
   * guest-visible RAM. */
  RAM (rwx) : ORIGIN = 0x40080000, LENGTH = 16M
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
