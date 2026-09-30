/*++

Licensed under the Apache-2.0 license.

File Name:

    main.rs

Abstract:

    File contains main entry point for MCU ROM

--*/

#![cfg_attr(target_arch = "riscv32", no_std)]
#![no_main]

#[cfg(all(target_arch = "riscv32", feature = "rom-patching"))]
mod boot;
#[cfg(target_arch = "riscv32")]
mod io;
#[cfg(target_arch = "riscv32")]
mod riscv;

#[cfg(target_arch = "riscv32")]
mod flash;

#[cfg(target_arch = "riscv32")]
mod mcu_image_verifier;

// `start.s` is included here, not in the patchable `riscv.rs`, to keep the
// non-patchable bootstrap in non-patchable files. START_MTVEC is the trap
// vector it installs before `main`. With rom-patching, `_exception_handler`
// reaches patchable code in SRAM that is not yet copied, so the non-patchable
// bootstrap handler in `boot.rs` is used instead.
#[cfg(all(target_arch = "riscv32", not(feature = "rom-patching")))]
core::arch::global_asm!(
    ".set START_MTVEC, _exception_handler",
    include_str!("start.s")
);
#[cfg(all(target_arch = "riscv32", feature = "rom-patching"))]
core::arch::global_asm!(
    ".set START_MTVEC, _bootstrap_exception_handler",
    include_str!("start.s")
);

#[cfg(target_arch = "riscv32")]
#[no_mangle]
#[cfg_attr(feature = "rom-patching", link_section = ".text.nonpatchable")]
pub extern "C" fn main() -> ! {
    #[cfg(feature = "rom-patching")]
    boot::start();

    #[cfg(not(feature = "rom-patching"))]
    riscv::rom_entry();
}

#[cfg(not(target_arch = "riscv32"))]
#[no_mangle]
pub extern "C" fn main() {
    // no-op on x86 just to keep the build clean
    println!("nop");
}
