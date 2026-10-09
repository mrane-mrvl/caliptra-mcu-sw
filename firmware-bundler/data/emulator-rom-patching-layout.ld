/* Licensed under the Apache-2.0 license. */

ENTRY(_start)

/*
 * The last 48 bytes of the ROM image are reserved for a SHA-384 digest that is
 * appended by the build step (see `append_rom_digest`). Carve them out of the
 * ROM region so the linker refuses to place code or rodata there.
 */
ROM_DIGEST_SIZE = 48;

/*
 * ROM patching SRAM layout. PATCH_FUNCS_START, PATCH_BLOB_START and
 * PATCH_BLOB_LENGTH are not defined here: the builder passes them as `--defsym`
 * from `caliptra_mcu_config_emulator` (`ROM_PATCH_REGION_START`,
 * `ROM_PATCH_BLOB_START`, `ROM_PATCH_BLOB_SIZE`), so Rust and this layout share
 * one value each.
 */
PATCH_FUNCS_LENGTH = 0x400;
PATCH_TEXT_START = PATCH_FUNCS_START + PATCH_FUNCS_LENGTH;
PATCH_TEXT_LENGTH = 0x10000;
/*
 * Instruction RAM: `.patch_funcs` then the patchable copy, one offset space.
 * `boot.rs` hands this whole window to the patch `apply`.
 */
PATCH_IRAM_START = PATCH_FUNCS_START;
PATCH_IRAM_END = PATCH_TEXT_START + PATCH_TEXT_LENGTH;

/*
 * `.data`/`.bss` are pinned below the top of ROM RAM so their addresses do not
 * shift when stack sizes change. The stacks sit at the bottom and grow down, so
 * an overflow leaves RAM and faults instead of reaching them.
 */
ROM_DATA_RESERVE = 0x400;

MEMORY
{
  ROM         (rx)  : ORIGIN = ROM_START, LENGTH = ROM_LENGTH - ROM_DIGEST_SIZE
  RAM         (rwx) : ORIGIN = RAM_START, LENGTH = RAM_LENGTH /* dedicated SRAM for the ROM stack */
  PATCH_FUNCS (rwx) : ORIGIN = PATCH_FUNCS_START, LENGTH = PATCH_FUNCS_LENGTH
  PATCH_TEXT  (rwx) : ORIGIN = PATCH_TEXT_START, LENGTH = PATCH_TEXT_LENGTH
}

SECTIONS
{
    .text.nonpatchable :
    {
        *(.text.init)
        *(.text.nonpatchable)
        *(.text.nonpatchable.*)
        . = ALIGN(4);
    } > ROM

    /*
     * Constant data read by non-patchable code before the patchable copy exists.
     * `start.s` loads MRAC_VALUE before `boot::start` runs.
     */
    .rodata.nonpatchable :
    {
        *(.rodata.MRAC_VALUE)
        *(.srodata.MRAC_VALUE)
        *(.rodata.nonpatchable)
        *(.rodata.nonpatchable.*)
        . = ALIGN(4);
    } > ROM

    .text :
    {
        PATCHABLE_TEXT_START = .;
        *(.text*)
        *(.rodata*)
        *(.srodata*)
        . = ALIGN(4);
        PATCHABLE_TEXT_END = .;
    } > PATCH_TEXT AT> ROM

    PATCHABLE_TEXT_LOAD_START = LOADADDR(.text);
    PATCHABLE_TEXT_LOAD_END = LOADADDR(.text) + SIZEOF(.text);

    /DISCARD/ :
    {
        *(.eh_frame*)
    }

    .stack (NOLOAD):
    {
        . = ALIGN(4);
        . = . + STACK_SIZE;
        . = ALIGN(4);
    } > RAM

    .estack (NOLOAD):
    {
        . = ALIGN(4);
        . = . + ESTACK_SIZE;
        . = ALIGN(4);
        PROVIDE(ESTACK_START = . );
    } > RAM

    .data (ORIGIN(RAM) + LENGTH(RAM) - ROM_DATA_RESERVE) :
    {
        . = ALIGN(4);
        *(.data*);
        *(.sdata*);
        . = ALIGN(4);
        PROVIDE( GLOBAL_POINTER = . + 0x800 );
        . = ALIGN(4);
    } > RAM AT> ROM

    .bss (NOLOAD) :
    {
        . = ALIGN(4);
        *(.bss*)
        *(.sbss*)
        *(COMMON)
        . = ALIGN(4);
    } > RAM

    _end = . ;

    /* LMA last in ROM so everything before it keeps its ROM offset. */
    .patch_funcs :
    {
        KEEP(*(.patch_funcs))
        KEEP(*(.patch_funcs.*))
        . = ALIGN(4);
    } > PATCH_FUNCS AT> ROM

    /DISCARD/ :
    {
        *(.eh_frame*)
    }

    /*
     * Non-allocated sections, listed so that `--orphan-handling=error` (set by the builder for
     * this layout) only fires on unexpected sections.
     */
    .comment 0 : { *(.comment) }
    .riscv.attributes 0 : { *(.riscv.attributes) }
    .debug_info 0 : { *(.debug_info) }
    .debug_abbrev 0 : { *(.debug_abbrev) }
    .debug_line 0 : { *(.debug_line) }
    .debug_line_str 0 : { *(.debug_line_str) }
    .debug_str 0 : { *(.debug_str) }
    .debug_str_offsets 0 : { *(.debug_str_offsets) }
    .debug_addr 0 : { *(.debug_addr) }
    .debug_loc 0 : { *(.debug_loc) }
    .debug_loclists 0 : { *(.debug_loclists) }
    .debug_ranges 0 : { *(.debug_ranges) }
    .debug_rnglists 0 : { *(.debug_rnglists) }
    .debug_aranges 0 : { *(.debug_aranges) }
    .debug_frame 0 : { *(.debug_frame) }
    .debug_pubnames 0 : { *(.debug_pubnames) }
    .debug_pubtypes 0 : { *(.debug_pubtypes) }
    /* Linker-synthesized; this lld reports them as orphans unless named. */
    .symtab 0 : { *(.symtab) }
    .shstrtab 0 : { *(.shstrtab) }
    .strtab 0 : { *(.strtab) }
}

BSS_START = ADDR(.bss);
BSS_END = BSS_START + SIZEOF(.bss);
DATA_START = ADDR(.data);
DATA_END = DATA_START + SIZEOF(.data);
ROM_DATA_START = LOADADDR(.data);
STACK_TOP = ADDR(.stack) + SIZEOF(.stack);
STACK_ORIGIN = ADDR(.stack);

ASSERT(ADDR(.stack) == ORIGIN(RAM), ".stack must start at ROM RAM so an overflow faults")
ASSERT(ADDR(.estack) + SIZEOF(.estack) <= ADDR(.data), "stacks overlap .data; raise ROM_DATA_RESERVE or shrink the stacks")
ASSERT(PATCH_TEXT_START + PATCH_TEXT_LENGTH <= PATCH_BLOB_START, "patchable ROM copy window overlaps the patch blob")
ASSERT((LOADADDR(.text) % 4) == 0, "patchable ROM text LMA must be word-aligned for the copy loop")
ASSERT(MRAC_VALUE >= ADDR(.rodata.nonpatchable) && MRAC_VALUE < ADDR(.rodata.nonpatchable) + SIZEOF(.rodata.nonpatchable), "MRAC_VALUE must be in ROM-only .rodata.nonpatchable; start.s reads it before the copy")
