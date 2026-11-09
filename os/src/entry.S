.section .text.entry
.global _start
_start:
    li a2, 0
    j enter
.global secondary_entry
secondary_entry:
    li a2, 1
enter:
    /* PC-relative until both the identity and high aliases are live. */
    la t0, boot_table
    srli t0, t0, 12
    li t1, 8 << 60
    or t0, t0, t1
    csrw satp, t0
    sfence.vma
    li t1, 0xffffffc000000000
    la t0, high
    or t0, t0, t1
    jr t0
high:
    la sp, boot_stacks
    addi t0, a0, 1
    slli t0, t0, 16
    add sp, sp, t0
    tail boot

.section .data
.balign 4096
boot_table:
    .zero 16
    .quad (0x80000 << 10) | 0xef
    .zero 253 * 8
    .quad 0xc7
    .zero 8
    .quad (0x80000 << 10) | 0xef
    .zero 253 * 8
.text
