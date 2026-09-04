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
  RAM (rwx) : ORIGIN = 0x40080000, LENGTH = 8M
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

  /* 64 KB stack, immediately after .bss; grows down from __stack_top. */
  . = ALIGN(16);
  . = . + 0x10000;
  __stack_top = .;

  /DISCARD/ : {
    *(.comment)
    *(.eh_frame)
    *(.note.*)
    *(.ARM.attributes)
  }
}
