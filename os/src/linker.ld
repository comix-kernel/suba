OUTPUT_ARCH(riscv)
ENTRY(_start)
BASE = 0xffffffc000000000;
SECTIONS {
  . = BASE + 0x80200000;
  stext = .;
  .text : AT(ADDR(.text) - BASE) { KEEP(*(.text.entry)) *(.text .text.*) }
  . = ALIGN(4096);
  srodata = .;
  .rodata : AT(ADDR(.rodata) - BASE) { *(.rodata .rodata.* .srodata .srodata.*) }
  . = ALIGN(4096);
  sdata = .;
  .data : AT(ADDR(.data) - BASE) { *(.data .data.* .sdata .sdata.* .got .got.*) }
  . = ALIGN(4096);
  .bss (NOLOAD) : { sbss = .; *(.bss .bss.* .sbss .sbss.* COMMON) ebss = .; }
  . = ALIGN(4096);
  boot_stacks = .;
  . += 2 * 65536;
  task_stacks = .;
  /* Each slot starts with an unmapped page. See runtime::SLOTS. */
  . += 24 * (4096 + 65536);
  heap_start = .;
  . += 2M;
  kernel_end = .;
  /DISCARD/ : { *(.eh_frame*) }
}
