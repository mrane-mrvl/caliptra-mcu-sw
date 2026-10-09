// Licensed under the Apache-2.0 license

#![cfg_attr(target_arch = "riscv32", no_std)]

pub mod flash;
use caliptra_mcu_config::{McuMemoryMap, McuStraps, MemoryRegionType};

/// Start of the ROM patching SRAM region (`.patch_funcs`, then the patchable
/// ROM copy). MCU RT must end at or below it. The builder passes it to
/// `firmware-bundler/data/emulator-rom-patching-layout.ld` as `PATCH_FUNCS_START`.
pub const ROM_PATCH_REGION_START: u32 = 0x4008_0000;

/// Fixed SRAM address of the ROM patch blob, shared by the ROM and the test
/// that injects it. The builder passes it and [`ROM_PATCH_BLOB_SIZE`] to the
/// patching layout as `PATCH_BLOB_START`/`PATCH_BLOB_LENGTH`.
pub const ROM_PATCH_BLOB_START: u32 = 0x400F_0000;
/// Size of the ROM patch blob window. `apply` is given all of it.
pub const ROM_PATCH_BLOB_SIZE: u32 = 0x400;

/// Start of the MCI-owned protected region at the top of SRAM; the patch blob
/// must end below it.
const MCI_PROTECTED_REGION_START: u32 = 0x400F_7000;

pub const EMULATOR_MEMORY_MAP: McuMemoryMap = McuMemoryMap {
    rom_offset: 0x8000_0000,
    rom_size: 64 * 1024,
    rom_stack_size: 0x2d00,
    rom_estack_size: 0x200,
    rom_properties: MemoryRegionType::MEMORY,

    dccm_offset: 0x5000_0000,
    dccm_size: 16 * 1024,
    dccm_properties: MemoryRegionType::MEMORY,

    sram_offset: 0x4000_0000,
    // 1 MB SRAM declared.  Default `devel` builds use the full region; `release`
    // builds (selected by `xtask runtime-build --profile release`) use a linker
    // layout that fits in the lower 512 KB to mirror the real device.  Setting
    // the const to the larger value covers both cases at runtime — the release
    // image just doesn't reference the upper half.
    sram_size: 1024 * 1024,
    sram_properties: MemoryRegionType::MEMORY,

    storage_size: 0x400,

    pic_offset: 0x6000_0000,
    pic_properties: MemoryRegionType::MMIO,

    i3c_offset: 0x2000_4000,
    i3c_size: 0x1000,
    i3c_properties: MemoryRegionType::MMIO,

    i3c1_offset: 0x2000_5000,
    i3c1_size: 0x1000,
    i3c1_properties: MemoryRegionType::MMIO,

    mci_offset: 0x2100_0000,
    mci_size: 0xe0_0000,
    mci_properties: MemoryRegionType::MMIO,

    mbox_offset: 0x3002_0000,
    mbox_size: 0x28,
    mbox_properties: MemoryRegionType::MMIO,

    soc_offset: 0x3003_0000,
    soc_size: 0x5e0,
    soc_properties: MemoryRegionType::MMIO,

    otp_offset: 0x7000_0000,
    otp_size: 0x140,
    otp_properties: MemoryRegionType::MMIO,

    lc_offset: 0x7000_0400,
    lc_size: 0x8c,
    lc_properties: MemoryRegionType::MMIO,
};

// The patch region must be a nonzero, 4 KiB-aligned offset inside SRAM: the ROM
// derives the firmware exec-region block count from it (see
// `fw_sram_exec_region_size` in platforms/emulator/rom/src/riscv.rs), and MCI
// cannot express an empty exec region.
const _: () = assert!(ROM_PATCH_REGION_START > EMULATOR_MEMORY_MAP.sram_offset);
const _: () = assert!(
    ROM_PATCH_REGION_START < EMULATOR_MEMORY_MAP.sram_offset + EMULATOR_MEMORY_MAP.sram_size
);
const _: () =
    assert!((ROM_PATCH_REGION_START - EMULATOR_MEMORY_MAP.sram_offset).is_multiple_of(4096));
const _: () = assert!(ROM_PATCH_BLOB_START > ROM_PATCH_REGION_START);
const _: () = assert!(ROM_PATCH_BLOB_START + ROM_PATCH_BLOB_SIZE <= MCI_PROTECTED_REGION_START);

const ACTIVE_I3C: u8 = if cfg!(feature = "active-i3c1") { 1 } else { 0 };

// A nonzero manufacturer serial number provisioned into the emulator's UEID fuse.
pub const EMULATOR_UEID_SERIAL_NUMBER: [u8; 16] = [
    0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10,
];

pub const EMULATOR_MCU_STRAPS: McuStraps = McuStraps {
    active_i3c: ACTIVE_I3C,
    ..McuStraps::default()
};

/// The MRAC value which should be populated for this memory map.  This corresponds to a value
/// utilized within the global start assembly and thus must be unmangled.
#[no_mangle]
pub static MRAC_VALUE: u32 = EMULATOR_MEMORY_MAP.compute_mrac();
