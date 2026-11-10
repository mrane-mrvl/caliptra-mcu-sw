// Licensed under the Apache-2.0 license

//! ROM patching end to end: boots the patch-capable MCU ROM, with Caliptra ROM
//! and firmware and the MCU runtime, and a patch blob preloaded into MCU SRAM.
//! Run by `cargo xtask rom-patching-test`, which builds the ROMs and patches
//! and passes their directory in `MCU_ROM_PATCHING_DIR`.
//!
//! Each `rom_patching_<case>` test is one case. To add one:
//! 1. Make the source fix, save it with
//!    `git diff > platforms/emulator/rom/rom-patching/<case>.diff`, and revert
//!    it. A case that needs no fix (a hand-made blob) skips this step.
//! 2. Add a `rom_patching_<case>` test below. [`patch`] returns the patch
//!    built for the fix; [`build_error`] returns why the patch builder
//!    refused it instead. [`boot_with_blob`] boots with a blob and steps
//!    until a line the case chooses; pass any extra `TestParams` the case
//!    needs (fuses, OTP, flash, ...).
//! 3. Run `cargo xtask rom-patching-test --case <case>`.

#[cfg(test)]
mod test {
    use crate::test::{start_runtime_hw_model, TestParams};
    use caliptra_mcu_config_emulator::{ROM_PATCH_BLOB_SIZE, ROM_PATCH_BLOB_START};
    use caliptra_mcu_error::McuError;
    use caliptra_mcu_hw_model::McuHwModel;
    use caliptra_mcu_rom_patch::start_header;
    use std::path::{Path, PathBuf};

    /// Cap on one boot, the hw-model's own limit for booting to the runtime.
    const MAX_CYCLES: u64 = 100_000_000;
    /// The MCI `FW_EXTENDED_ERROR_INFO` word the bootstrap writes a patch
    /// error to before halting.
    const PATCH_ERROR_INFO: usize = 1;
    /// The emulator logs every read of that word, so poll it only this often.
    const ERROR_POLL_STEPS: u64 = 4096;
    /// How long to keep the ROM running after it reports a patch error.
    ///
    /// On a patch error the ROM must stop. Today it does: it loops forever
    /// in `fatal()`. Running it a while longer checks that it really stopped:
    /// if a later change lets it continue, it prints its first line from SRAM
    /// within ~85k cycles, and the test catches that line.
    const HALT_CHECK_CYCLES: u64 = 500_000;

    const IGNORE: &str = "run with `cargo xtask rom-patching-test`";

    fn read(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
    }

    fn out_dir() -> PathBuf {
        std::env::var_os("MCU_ROM_PATCHING_DIR")
            .unwrap_or_else(|| panic!("MCU_ROM_PATCHING_DIR is not set; {IGNORE}"))
            .into()
    }

    /// The patch-capable ROM without any case fix.
    fn baseline_rom() -> Vec<u8> {
        read(&out_dir().join("baseline.bin"))
    }

    /// The patch built for `case`'s fix.
    fn patch(case: &str) -> Vec<u8> {
        let dir = out_dir().join(case);
        if let Ok(error) = std::fs::read_to_string(dir.join("build-error.txt")) {
            panic!("the patch builder refused {case}: {error}");
        }
        read(&dir.join("patch.bin"))
    }

    /// Why the patch builder refused `case`'s fix.
    fn build_error(case: &str) -> String {
        let path = out_dir().join(case).join("build-error.txt");
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("expected the patch builder to refuse {case}: {e}"))
    }

    /// Boots the baseline ROM, with the Caliptra ROM and firmware and the MCU
    /// runtime that `params` select, and `blob`, zero-padded, in the patch
    /// blob window. Steps until the UART prints a whole line containing
    /// `until`, the ROM reports a patch error (then `HALT_CHECK_CYCLES` more),
    /// or `MAX_CYCLES` pass. Returns the UART output and the patch error word
    /// (0 if none).
    fn boot_with_blob(params: TestParams, blob: &[u8], until: &str) -> (String, u32) {
        let mut window = blob.to_vec();
        window.resize(ROM_PATCH_BLOB_SIZE as usize, 0);
        let mut hw = start_runtime_hw_model(TestParams {
            custom_mcu_rom: Some(baseline_rom()),
            sram_preload: Some((ROM_PATCH_BLOB_START, window)),
            // The release runtime fits below the patch region (decision 5).
            profile: Some("release"),
            // Step here: otherwise boot() waits for the runtime and drains the
            // UART output.
            rom_only: true,
            ..params
        });

        hw.output().set_search_term(until);
        let mut steps = 0u64;
        hw.step_until(|m| {
            steps += 1;
            m.output().search_matched()
                || (steps.is_multiple_of(ERROR_POLL_STEPS)
                    && m.mci_fw_extended_error_info(PATCH_ERROR_INFO) != 0)
                || m.cycle_count() > MAX_CYCLES
        });
        if hw.output().search_matched() {
            // Let the rest of the matched line reach the UART.
            hw.step_until(|m| m.output().peek().ends_with('\n') || m.cycle_count() > MAX_CYCLES);
        } else if hw.mci_fw_extended_error_info(PATCH_ERROR_INFO) != 0 {
            let end = hw.cycle_count() + HALT_CHECK_CYCLES;
            hw.step_until(|m| m.cycle_count() > end);
        }
        let output = hw.output().take(usize::MAX);
        println!("{output}");
        (output, hw.mci_fw_extended_error_info(PATCH_ERROR_INFO))
    }

    /// Same-length string edit: one Overwrite Instruction RAM operation. The
    /// string is printed by code running from the patched SRAM copy.
    #[test]
    #[ignore = "run with `cargo xtask rom-patching-test`"]
    fn rom_patching_hello_r_to_p() {
        let hello = "[mcu-rom] Hello from ";

        // An all-zero blob holds no patch.
        let (output, error) = boot_with_blob(TestParams::default(), &[], hello);
        assert_eq!(error, 0, "baseline boot hit a patch error");
        assert!(
            output.contains("executing from 4008"),
            "not running from SRAM"
        );
        assert!(output.contains("[mcu-rom] Hello from ROM\n"));

        // Boot through to the firmware handoff: the ROM runs twice, and the
        // second pass (FirmwareBootReset) runs the copy patched on cold boot.
        let (output, error) = boot_with_blob(
            TestParams::default(),
            &patch("hello_r_to_p"),
            "[mcu-rom] Jumping to firmware",
        );
        assert_eq!(error, 0, "patched boot hit a patch error");
        assert!(
            output.contains("executing from 4008"),
            "not running from SRAM"
        );
        assert_eq!(output.matches("[mcu-rom] Hello from POM\n").count(), 2);
        assert!(output.contains("[mcu-rom] Jumping to firmware"));
    }

    /// A longer string moves everything after it, so the shift guard refuses
    /// the fix.
    #[test]
    #[ignore = "run with `cargo xtask rom-patching-test`"]
    fn rom_patching_hello_longer() {
        let error = build_error("hello_longer");
        assert!(error.contains("moves code or data"), "{error}");
    }

    /// A blob whose End Header is missing halts the ROM in its bootstrap,
    /// before it branches to the SRAM copy, with the error code in the patch
    /// error word.
    #[test]
    #[ignore = "run with `cargo xtask rom-patching-test`"]
    fn rom_patching_malformed_end_header() {
        // Two words: a Start Header, then zeros where the End Header belongs.
        let blob = [start_header(2), [0; 4]].concat();
        // The first line the ROM prints from the SRAM copy.
        let entered_sram = "[mcu-rom] executing from ";
        let (output, error) = boot_with_blob(TestParams::default(), &blob, entered_sram);
        assert_eq!(error, McuError::ROM_PATCH_END_HEADER_MISMATCH.0.get());
        assert!(!output.contains(entered_sram), "the ROM did not halt");
    }
}
