/*++

Licensed under the Apache-2.0 license.

File Name:

    boot.rs

Abstract:

    Non-patchable ROM bootstrap: copies patchable ROM text into SRAM and
    applies the patch blob to it.

--*/

use caliptra_mcu_config_emulator::{
    EMULATOR_MEMORY_MAP, ROM_PATCH_BLOB_SIZE, ROM_PATCH_BLOB_START,
};
use caliptra_mcu_error::McuError;
use caliptra_mcu_rom_patch::PATCH_OK;

const MCI_BASE: usize = EMULATOR_MEMORY_MAP.mci_offset as usize;
const MCI_RESET_REASON_OFFSET: usize = 0x38;
const MCI_FW_EXTENDED_ERROR_INFO_1_OFFSET: usize = 0x74;
const BOOTSTRAP_EXCEPTION_ERROR: u32 = McuError::ROM_PATCH_BOOTSTRAP_EXCEPTION.0.get();

unsafe extern "C" {
    static PATCHABLE_TEXT_LOAD_START: u8;
    static PATCHABLE_TEXT_START: u8;
    static PATCHABLE_TEXT_END: u8;
    static PATCH_IRAM_START: u8;
    static PATCH_IRAM_END: u8;

    fn _exception_handler();
    fn patchable_entry() -> !;
}

// Bootstrap trap vector, installed by `start.s` (via START_MTVEC). An asm stub
// so it is 4-byte aligned, as `mtvec` requires; a Rust fn is only 2-aligned
// with the C extension.
core::arch::global_asm!(
    ".pushsection .text.nonpatchable, \"ax\"",
    ".balign 4",
    ".global _bootstrap_exception_handler",
    "_bootstrap_exception_handler:",
    "    li a0, {code}",
    "    j {fatal}",
    ".popsection",
    code = const BOOTSTRAP_EXCEPTION_ERROR as i32,
    fatal = sym fatal,
);

#[inline(never)]
#[link_section = ".text.nonpatchable"]
pub fn start() -> ! {
    unsafe {
        if is_cold_boot() {
            copy_patchable_text();
            apply_patch();
        }
        set_mtvec(_exception_handler as *const () as usize);
        fence_i();
        patchable_entry()
    }
}

#[inline(never)]
#[link_section = ".text.nonpatchable"]
unsafe fn is_cold_boot() -> bool {
    let reset_reason = ((MCI_BASE + MCI_RESET_REASON_OFFSET) as *const u32).read_volatile();
    reset_reason == 0
}

#[inline(never)]
#[link_section = ".text.nonpatchable"]
unsafe fn copy_patchable_text() {
    let src = &PATCHABLE_TEXT_LOAD_START as *const u8 as *const u32;
    let dst = &PATCHABLE_TEXT_START as *const u8 as *mut u32;
    let start = &PATCHABLE_TEXT_START as *const u8 as usize;
    let end = &PATCHABLE_TEXT_END as *const u8 as usize;
    let words = (end - start) / core::mem::size_of::<u32>();

    let mut i = 0;
    while i < words {
        dst.add(i).write_volatile(src.add(i).read_volatile());
        i += 1;
    }
}

// Applies the patch blob to the SRAM copy. Data RAM is passed empty while ROM
// `.data` is 0 bytes, so an `Overwrite Data RAM` op is fatal rather than lost
// at the warm reset (see Known Issues in the patching spec).
#[inline(never)]
#[link_section = ".text.nonpatchable"]
unsafe fn apply_patch() {
    let blob = core::slice::from_raw_parts(
        ROM_PATCH_BLOB_START as usize as *const u8,
        ROM_PATCH_BLOB_SIZE as usize,
    );
    let iram_start = &PATCH_IRAM_START as *const u8 as *mut u8;
    let iram_len = &PATCH_IRAM_END as *const u8 as usize - iram_start as usize;
    let iram = core::slice::from_raw_parts_mut(iram_start, iram_len);

    let code = caliptra_mcu_rom_patch::apply(blob, iram, &mut []);
    if code != PATCH_OK {
        fatal(code);
    }
}

#[inline(never)]
#[link_section = ".text.nonpatchable"]
unsafe fn set_mtvec(handler: usize) {
    core::arch::asm!("csrw mtvec, {handler}", handler = in(reg) handler, options(nostack));
}

#[inline(never)]
#[link_section = ".text.nonpatchable"]
unsafe fn fence_i() {
    // Encoding for `fence.i`; use a raw word so this builds even when the
    // target features do not spell out Zifencei.
    core::arch::asm!(".word 0x0000100f", options(nostack));
}

#[inline(never)]
#[link_section = ".text.nonpatchable"]
extern "C" fn fatal(code: u32) -> ! {
    unsafe {
        ((MCI_BASE + MCI_FW_EXTENDED_ERROR_INFO_1_OFFSET) as *mut u32).write_volatile(code);
        core::arch::asm!("1:", "j 1b", options(noreturn));
    }
}
