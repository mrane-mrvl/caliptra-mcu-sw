// Licensed under the Apache-2.0 license

use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::utils::{manifest_file, manifest_file_for_profile};
use crate::{CaliptraBuildArgs, PROJECT_ROOT};
use caliptra_image_crypto::RustCrypto as Crypto;
use caliptra_image_gen::{from_hw_format, ImageGeneratorCrypto};
use caliptra_mcu_config_emulator::{
    ROM_PATCH_BLOB_SIZE, ROM_PATCH_BLOB_START, ROM_PATCH_REGION_START,
};
use caliptra_mcu_firmware_bundler::args::{BuildArgs, Commands, Common, LdArgs};
use caliptra_mcu_firmware_bundler::manifest::RuntimeMemory;

const ROM_PATCHING_FEATURE: &str = "rom-patching";

pub fn rom_build(args: &CaliptraBuildArgs) -> Result<PathBuf> {
    let platform = args.platform;
    let platform_name = platform.unwrap_or("emulator");
    let features = args.features;
    let target_dir = args.target_dir.clone();
    let rom_patching = feature_enabled(features, ROM_PATCHING_FEATURE);

    if rom_patching && platform_name != "emulator" {
        bail!("rom-patching is emulator-only");
    }

    let feature_suffix = match &features {
        Some(f) if !f.is_empty() => format!("-{f}"),
        _ => String::new(),
    };

    let target_name = format!("mcu-rom-{platform_name}");
    let rom = format!("{target_name}{feature_suffix}");
    let common = if rom_patching {
        let profile = args.profile.unwrap_or("release");
        let common = Common {
            manifest: manifest_file_for_profile(platform, false, Some(profile))?,
            target_dir,
            profile: profile.to_string(),
            ..Default::default()
        };
        ensure_patch_copy_does_not_overlap_runtime(&common)?;
        common
    } else {
        Common {
            manifest: manifest_file(platform, false)?,
            target_dir,
            ..Default::default()
        }
    };
    let ld = if rom_patching {
        LdArgs {
            rom_ld_base: Some(
                PROJECT_ROOT
                    .join("firmware-bundler")
                    .join("data")
                    .join("emulator-rom-patching-layout.ld"),
            ),
            ..Default::default()
        }
    } else {
        LdArgs::default()
    };
    let rom_size = rom_size_for_platform(platform_name);
    let rom_binary = common.release_dir().map(|t| t.join(format!("{rom}.bin")))?;
    let build_cmd = Commands::Build {
        common,
        ld,
        build: BuildArgs {
            rom_features: features.filter(|s| !s.is_empty()).map(|s| s.to_string()),
            // --icf=none: disable identical code folding (ICF), which merges functions with
            // identical machine code. Editing one function can make it differ from its twin, so
            // the linker stops folding them and symbols move. The patch builder fails if any
            // section or symbol address or size differs between the baseline and patched ELFs.
            // --orphan-handling=error: fail the link if an input section (e.g. .srodata*) is not
            // placed by the linker script. Otherwise the linker places it silently, possibly
            // outside the patchable copy. Add it to the patching layout to fix.
            rom_link_args: rom_patching.then(|| {
                format!(
                    "--icf=none --orphan-handling=error \
                     --defsym=PATCH_FUNCS_START={ROM_PATCH_REGION_START:#x} \
                     --defsym=PATCH_BLOB_START={ROM_PATCH_BLOB_START:#x} \
                     --defsym=PATCH_BLOB_LENGTH={ROM_PATCH_BLOB_SIZE:#x}"
                )
            }),
            no_default_features: true,
            ..Default::default()
        },
        target: Some(target_name.clone()),
    };

    caliptra_mcu_firmware_bundler::execute(build_cmd)?;
    let bundler_output = rom_binary.with_file_name(format!("{target_name}.bin"));
    if bundler_output != rom_binary {
        // The bundler always writes to the base binary name (e.g.
        // mcu-rom-emulator.bin). When building a feature variant we need to
        // place the output at the feature-suffixed path. Use copy instead of
        // rename so the base binary is preserved for other builds that depend
        // on the default ROM.
        std::fs::copy(&bundler_output, &rom_binary)?;
    }
    assert!(rom_binary.exists(), "{rom_binary:?} does not exist");
    append_rom_digest(&rom_binary, rom_size)?;
    Ok(rom_binary)
}

/// Pad the ROM binary to its full size and append a SHA384 digest of its contents to the end.
pub fn append_rom_digest(binary: &PathBuf, rom_size: usize) -> Result<()> {
    let mut data = std::fs::read(binary)?;
    const DIGEST_SIZE: usize = 48;
    let digest_offset = rom_size - DIGEST_SIZE;
    if data.len() > digest_offset {
        bail!(
            "ROM binary {:?} is {} bytes, which does not leave room for the {}-byte SHA-384 digest within the {}-byte ROM region. Reduce ROM size or increase the platform's rom_size.",
            binary,
            data.len(),
            DIGEST_SIZE,
            rom_size,
        );
    }
    data.resize(rom_size, 0);
    let crypto = Crypto::default();
    let digest = from_hw_format(&crypto.sha384_digest(&data[0..digest_offset])?);
    data[digest_offset..].copy_from_slice(&digest);
    std::fs::write(binary, data)?;
    Ok(())
}

fn feature_enabled(features: Option<&str>, needle: &str) -> bool {
    features
        .unwrap_or_default()
        .split(|c: char| c == ',' || c.is_whitespace())
        .any(|feature| feature == needle)
}

fn ensure_patch_copy_does_not_overlap_runtime(common: &Common) -> Result<()> {
    let manifest = common.manifest()?;
    let RuntimeMemory::Sram(runtime_sram) = manifest.platform.runtime_memory else {
        return Ok(());
    };
    let runtime_end = runtime_sram.offset + runtime_sram.size;
    let patch_region_start = u64::from(ROM_PATCH_REGION_START);
    if runtime_end > patch_region_start {
        bail!(
            "rom-patching requires runtime SRAM to end at or below {patch_region_start:#x}; manifest {} ends at {runtime_end:#x}",
            common.manifest.display()
        );
    }
    Ok(())
}

pub fn rom_size_for_platform(platform: &str) -> usize {
    match platform {
        "fpga" => caliptra_mcu_config_fpga::FPGA_MEMORY_MAP.rom_size as usize,
        _ => caliptra_mcu_config_emulator::EMULATOR_MEMORY_MAP.rom_size as usize,
    }
}

pub fn test_rom_build(args: &CaliptraBuildArgs) -> Result<String> {
    let platform = args.platform.unwrap_or("emulator");
    let fwid = args
        .fwid
        .ok_or_else(|| anyhow::anyhow!("fwid is required for test_rom_build"))?;
    let target_dir = args.target_dir.clone();

    let template_name = if platform == "fpga" {
        "fpga.toml"
    } else {
        "emulator.toml"
    };
    let template_path = PROJECT_ROOT
        .join("hw/model/test-fw/data")
        .join(template_name);
    let template = std::fs::read_to_string(&template_path)?;
    let manifest_contents = template.replace("{{ROM_NAME}}", fwid.crate_name);

    let mut manifest_file = tempfile::NamedTempFile::new()?;
    manifest_file.write_all(manifest_contents.as_bytes())?;
    manifest_file.flush()?;

    let common = Common {
        manifest: manifest_file.path().to_path_buf(),
        target_dir,
        ..Default::default()
    };

    let platform_bin = format!("mcu-test-rom-{}-{}.bin", fwid.crate_name, fwid.bin_name);
    let rom_binary = common.release_dir().map(|t| t.join(&platform_bin))?;

    let mut features = fwid.features().to_vec();
    if !features.contains(&"riscv") {
        features.push("riscv");
    }
    if platform != "emulator" {
        features.push("fpga_realtime");
    }

    let build_cmd = Commands::Build {
        common,
        ld: LdArgs::default(),
        build: BuildArgs {
            rom_features: Some(features.join(",")),
            no_default_features: true,
            ..Default::default()
        },
        target: Some(fwid.crate_name.to_string()),
    };

    caliptra_mcu_firmware_bundler::execute(build_cmd)?;

    // The firmware bundler outputs <crate_name>.bin; rename to our expected convention.
    let bundler_output = rom_binary.with_file_name(format!("{}.bin", fwid.crate_name));
    std::fs::rename(&bundler_output, &rom_binary)?;

    assert!(rom_binary.exists(), "{rom_binary:?} does not exist");
    println!(
        "ROM binary ({}) is at {:?} ({} bytes)",
        platform,
        &rom_binary,
        std::fs::metadata(&rom_binary)?.len()
    );
    let rom_size = rom_size_for_platform(platform);
    append_rom_digest(&rom_binary, rom_size)?;
    Ok(rom_binary.to_string_lossy().to_string())
}
