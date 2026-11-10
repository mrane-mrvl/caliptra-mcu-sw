// Licensed under the Apache-2.0 license

//! `cargo xtask rom-patching-test`: runs the ROM patching integration test
//! cases. A case is a `rom_patching_<case>` test in
//! `tests/integration/src/rom/test_rom_patching.rs`, plus, optionally, a source
//! fix in `<CASES_DIR>/<case>.diff`. For each case with a fix, the driver
//! builds the patch-capable ROM with the fix applied and builds the patch from
//! the baseline ROM to it, then runs the selected tests.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{ensure, Context, Result};
use caliptra_mcu_builder::{CaliptraBuildArgs, PROJECT_ROOT};
use caliptra_mcu_config_emulator::ROM_PATCH_BLOB_SIZE;

/// Where the case fixes live, as diffs against the tree.
const CASES_DIR: &str = "platforms/emulator/rom/rom-patching";
/// Test name prefix that marks a test as a case.
const TEST_PREFIX: &str = "rom_patching_";
const TEST_CRATE: &str = "caliptra-mcu-tests-integration";
/// Every ROM build writes its ELF here, next to the `.bin` `rom_build` returns.
const ROM_ELF: &str = "mcu-rom-emulator";
/// Environment variable the tests read the output directory from.
const OUT_DIR_ENV: &str = "MCU_ROM_PATCHING_DIR";

/// Runs the `selected` cases, or every case if none are selected.
pub(crate) fn run(selected: &[String]) -> Result<()> {
    // Case name -> full test path.
    let tests = list_tests()?;
    let diffs = list_diffs()?;
    for case in &diffs {
        ensure!(
            tests.iter().any(|(name, _)| name == case),
            "{CASES_DIR}/{case}.diff has no {TEST_PREFIX}{case} test"
        );
    }
    let cases: Vec<&(String, String)> = if selected.is_empty() {
        tests.iter().collect()
    } else {
        selected
            .iter()
            .map(|case| {
                tests
                    .iter()
                    .find(|(name, _)| name == case)
                    .with_context(|| {
                        let names: Vec<_> = tests.iter().map(|(name, _)| name.as_str()).collect();
                        format!("no case {case}; cases: {}", names.join(", "))
                    })
            })
            .collect::<Result<_>>()?
    };
    let fixes: Vec<&str> = cases
        .iter()
        .map(|(name, _)| name.as_str())
        .filter(|name| diffs.iter().any(|d| d == name))
        .collect();
    // Every diff, not only the selected ones: a fix a killed run left applied
    // would otherwise end up in the baseline.
    for case in &diffs {
        check_tree(&diff_path(case))?;
    }

    // Start empty, so no case reads a stale patch from an earlier run.
    let out = PROJECT_ROOT.join("target").join("rom-patching");
    if out.exists() {
        std::fs::remove_dir_all(&out)?;
    }
    std::fs::create_dir_all(&out)?;

    println!("Building the baseline ROM");
    let rom = build_rom()?;
    copy(&rom, &out.join("baseline.bin"))?;
    copy(&rom.with_file_name(ROM_ELF), &out.join("baseline.elf"))?;

    for case in &fixes {
        build_case(case, &out)?;
    }

    if !fixes.is_empty() {
        // Leaves target/ holding the baseline ROM again, and proves every
        // revert restored the source.
        println!("Rebuilding the baseline ROM");
        let rom = build_rom()?;
        ensure!(
            std::fs::read(&rom)? == std::fs::read(out.join("baseline.bin"))?,
            "the ROM rebuilt after reverting the case fixes differs from the baseline"
        );
    }

    let status = Command::new("cargo")
        .current_dir(&*PROJECT_ROOT)
        .args(["test", "-p", TEST_CRATE, "--lib", "--"])
        .args(cases.iter().map(|(_, path)| path))
        // One thread, so each case's UART output stays together.
        .args(["--exact", "--ignored", "--nocapture", "--test-threads=1"])
        .env(OUT_DIR_ENV, &out)
        // Build the Caliptra firmware and MCU runtime from source: a bundle's
        // runtime may be a devel build that overlaps the patch region.
        .env_remove("CPTRA_FIRMWARE_BUNDLE")
        .status()?;
    ensure!(status.success(), "ROM patching integration test failed");
    Ok(())
}

/// Builds the ROM with `case`'s fix applied, and the patch from the baseline
/// to it. Writes `<out>/<case>/patch.bin`, or `build-error.txt` if the patch
/// builder refuses the fix: a case can expect either.
fn build_case(case: &str, out: &Path) -> Result<()> {
    let dir = out.join(case);
    std::fs::create_dir_all(&dir)?;
    println!("Building the ROM for case {case}");
    let diff = diff_path(case);
    {
        let _applied = AppliedDiff::apply(&diff)?;
        let rom = build_rom()?;
        copy(&rom.with_file_name(ROM_ELF), &dir.join("modified.elf"))?;
    }
    match caliptra_mcu_builder::rom_patch_build(
        &out.join("baseline.elf"),
        &dir.join("modified.elf"),
    ) {
        Ok(patch) => {
            std::fs::write(dir.join("patch.bin"), &patch.blob)?;
            println!(
                "Case {case}: {} operation(s), {} of {ROM_PATCH_BLOB_SIZE} B",
                patch.op_count,
                patch.blob.len()
            );
        }
        Err(e) => {
            std::fs::write(dir.join("build-error.txt"), format!("{e:#}"))?;
            println!("Case {case}: the patch builder refused the fix");
        }
    }
    Ok(())
}

/// The cases: every ignored test named `rom_patching_<case>`, as
/// (case, full test path).
fn list_tests() -> Result<Vec<(String, String)>> {
    let output = Command::new("cargo")
        .current_dir(&*PROJECT_ROOT)
        .args([
            "test",
            "-p",
            TEST_CRATE,
            "--lib",
            "--",
            "--list",
            "--ignored",
        ])
        .stderr(Stdio::inherit())
        .output()?;
    ensure!(
        output.status.success(),
        "listing the {TEST_CRATE} tests failed"
    );
    // Each test is listed as "<path>: test".
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .filter_map(|line| line.strip_suffix(": test"))
        .filter_map(|path| {
            let name = path.rsplit("::").next()?.strip_prefix(TEST_PREFIX)?;
            Some((name.to_string(), path.to_string()))
        })
        .collect())
}

/// The cases with a fix: the stem of every `.diff` in `CASES_DIR`.
fn list_diffs() -> Result<Vec<String>> {
    let mut cases = Vec::new();
    for entry in std::fs::read_dir(PROJECT_ROOT.join(CASES_DIR))? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "diff") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                cases.push(stem.to_string());
            }
        }
    }
    Ok(cases)
}

fn diff_path(case: &str) -> PathBuf {
    PROJECT_ROOT.join(CASES_DIR).join(format!("{case}.diff"))
}

/// Refuses to start unless `diff` applies cleanly to unmodified files. A
/// diff left applied by a killed run shows up as modified files.
fn check_tree(diff: &Path) -> Result<()> {
    let display = diff.display();
    let numstat = git(&["apply", "--numstat"], diff)?;
    ensure!(numstat.status.success(), "{display} is not a valid diff");
    // `--numstat` prints "<added>\t<deleted>\t<path>" per file.
    let paths: Vec<String> = String::from_utf8(numstat.stdout)?
        .lines()
        .filter_map(|line| line.splitn(3, '\t').nth(2).map(str::to_string))
        .collect();
    let dirty = Command::new("git")
        .current_dir(&*PROJECT_ROOT)
        .args(["diff", "--quiet", "HEAD", "--"])
        .args(&paths)
        .status()?;
    ensure!(
        dirty.success(),
        "{} has uncommitted changes; commit or revert them first. If a killed run \
         left {display} applied, revert it with `git apply -R {display}`",
        paths.join(", ")
    );
    ensure!(
        git(&["apply", "--check"], diff)?.status.success(),
        "{display} does not apply to the current tree"
    );
    Ok(())
}

/// The fix diff, applied to the tree until dropped, including when a build
/// fails or panics.
struct AppliedDiff<'a>(&'a Path);

impl<'a> AppliedDiff<'a> {
    fn apply(diff: &'a Path) -> Result<Self> {
        let output = git(&["apply"], diff)?;
        ensure!(
            output.status.success(),
            "git apply {} failed: {}",
            diff.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(Self(diff))
    }
}

impl Drop for AppliedDiff<'_> {
    fn drop(&mut self) {
        let reverted = git(&["apply", "-R"], self.0).is_ok_and(|o| o.status.success());
        if !reverted {
            eprintln!(
                "Failed to revert {0}. Revert it by hand: git apply -R {0}",
                self.0.display()
            );
        }
    }
}

fn git(args: &[&str], diff: &Path) -> Result<Output> {
    Command::new("git")
        .current_dir(&*PROJECT_ROOT)
        .args(args)
        .arg(diff)
        .output()
        .context("running git")
}

fn build_rom() -> Result<PathBuf> {
    caliptra_mcu_builder::rom_build(&CaliptraBuildArgs {
        features: Some("rom-patching"),
        ..Default::default()
    })
}

fn copy(from: &Path, to: &Path) -> Result<()> {
    std::fs::copy(from, to)
        .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    Ok(())
}
