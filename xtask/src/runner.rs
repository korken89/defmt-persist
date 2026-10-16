//! Test runner dispatch and common types.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

use crate::build::{build_example, project_root};
use crate::corrupt::run_corrupt;
use crate::defmt;
use crate::qemu::{MemoryLoad, run_qemu};

// Colored status strings
pub const PASS: &str = "\x1b[32mPASS\x1b[0m";
pub const FAIL: &str = "\x1b[31mFAIL\x1b[0m";

/// Address of the persist region in memory.
pub const PERSIST_ADDR: u32 = 0x2000_FC00;

/// How to run the test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunMode {
    /// Single QEMU run.
    #[default]
    Single,
    /// Runs with a memory snapshot carried between them.
    Persist,
}

/// How to validate the test results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValidateMode {
    /// Compare output against .expected file.
    #[default]
    Expected,
    /// Run corruption scenarios (persist only, special case).
    Corrupt,
}

/// Options for running an example.
pub struct RunOptions {
    /// Print verbose output (for `qemu` command).
    pub verbose: bool,
    /// Update expected files instead of comparing (for `test --bless`).
    pub bless: bool,
    /// Build in release mode.
    pub release: bool,
}

/// Test configuration parsed from file markers.
struct TestConfig {
    run_mode: RunMode,
    validate_mode: ValidateMode,
}

/// Parse test configuration from file markers.
///
/// Looks for `@test-run: <mode>` and `@test-validate: <mode>` in the first few lines.
fn parse_test_config(example_path: &PathBuf) -> TestConfig {
    let mut config = TestConfig {
        run_mode: RunMode::default(),
        validate_mode: ValidateMode::default(),
    };

    if let Ok(content) = fs::read_to_string(example_path) {
        for line in content.lines().take(15) {
            if let Some(mode) = line.strip_prefix("//! @test-run:") {
                config.run_mode = match mode.trim() {
                    "single" => RunMode::Single,
                    "persist" => RunMode::Persist,
                    _ => RunMode::default(),
                };
            }
            if let Some(mode) = line.strip_prefix("//! @test-validate:") {
                config.validate_mode = match mode.trim() {
                    "expected" => ValidateMode::Expected,
                    "corrupt" => ValidateMode::Corrupt,
                    _ => ValidateMode::default(),
                };
            }
        }
    }

    config
}

/// Run an example with the given options.
///
/// Returns `Ok(true)` if the test passed, `Ok(false)` if it failed.
pub fn run_example(example: &str, opts: &RunOptions) -> Result<bool> {
    let root = project_root();
    let example_path = root
        .join("testsuite")
        .join("examples")
        .join(format!("{example}.rs"));
    let config = parse_test_config(&example_path);

    println!("Building '{example}'...");
    let elf_path = build_example(example, opts.release)?;

    if config.validate_mode == ValidateMode::Corrupt {
        return run_corrupt(&elf_path, opts);
    }

    match config.run_mode {
        RunMode::Single => run_single(example, &elf_path, opts),
        RunMode::Persist => run_persist(example, &elf_path, opts),
    }
}

/// Run a single-phase test.
fn run_single(example: &str, elf_path: &PathBuf, opts: &RunOptions) -> Result<bool> {
    println!("Running in QEMU...");
    let output = run_qemu(elf_path, None)?;
    let semihosting = defmt::decode_output(elf_path, &output.semihosting)?;
    let uart0 = defmt::decode_output(elf_path, &output.uart0)?;

    if opts.verbose {
        println!("--- semihosting ---");
        print!("{semihosting}");
        println!("--- uart ---");
        print!("{uart0}");
        println!("--- end ---");
        return Ok(true);
    }

    if semihosting != uart0 {
        println!("  {FAIL}: semihosting and UART output differ");
        println!("--- semihosting ---");
        print!("{semihosting}");
        println!("--- uart ---");
        print!("{uart0}");
        return Ok(false);
    }

    compare_expected(example, &semihosting, opts)
}

/// Upper bound on runs in a persist test, in case the firmware never stops dumping.
const MAX_PERSIST_RUNS: usize = 5;

/// Run a multi-phase persist test.
///
/// Each run that dumps the persist region is followed by a run with that dump loaded.
fn run_persist(example: &str, elf_path: &PathBuf, opts: &RunOptions) -> Result<bool> {
    let snapshot_file = NamedTempFile::new().context("Failed to create snapshot file")?;
    let snapshot_path = snapshot_file.path().to_path_buf();
    let mut outputs = Vec::new();

    for phase in 1..=MAX_PERSIST_RUNS {
        let output = if phase == 1 {
            println!("Phase 1: Running...");
            run_qemu(elf_path, None)?
        } else {
            println!("Phase {phase}: Running with snapshot...");
            run_qemu(
                elf_path,
                Some(MemoryLoad {
                    file: &snapshot_path,
                    addr: PERSIST_ADDR,
                }),
            )?
        };
        let uart0 = defmt::decode_output(elf_path, &output.uart0)?;

        if opts.verbose {
            let semihosting = defmt::decode_output(elf_path, &output.semihosting)?;
            println!("--- semihosting ---");
            print!("{semihosting}");
            println!("--- uart ---");
            print!("{uart0}");
            println!("--- Phase {phase} end ---\n");
        }
        outputs.push(uart0);

        if output.uart1.is_empty() {
            if phase == 1 {
                println!("  {FAIL}: no persist region captured in phase 1");
                return Ok(false);
            }
            let combined = outputs
                .iter()
                .enumerate()
                .map(|(i, output)| format!("=== Run {} ===\n{output}", i + 1))
                .collect::<Vec<_>>()
                .join("\n");
            return compare_expected(example, &combined, opts);
        }

        if opts.verbose {
            println!(
                "Captured {} bytes from persist region\n",
                output.uart1.len()
            );
        }
        fs::write(&snapshot_path, &output.uart1)?;
    }

    println!("  {FAIL}: persist region still dumped after {MAX_PERSIST_RUNS} runs");
    for (i, output) in outputs.iter().enumerate() {
        println!("--- Run {} ---", i + 1);
        print!("{output}");
    }
    Ok(false)
}

/// Compare output against expected file.
fn compare_expected(example: &str, output: &str, opts: &RunOptions) -> Result<bool> {
    let root = project_root();
    let expected_path = root
        .join("testsuite")
        .join("expected")
        .join(format!("{example}.expected"));

    if opts.bless {
        let filename = expected_path.file_name().unwrap().to_string_lossy();
        let status = if expected_path.exists() {
            let existing = fs::read_to_string(&expected_path)?;
            if existing == output {
                "No change"
            } else {
                fs::write(&expected_path, output)?;
                "Updated"
            }
        } else {
            fs::create_dir_all(expected_path.parent().unwrap())?;
            fs::write(&expected_path, output)?;
            "Created"
        };
        println!("  {filename}: {status}");
        Ok(true)
    } else if expected_path.exists() {
        let expected = fs::read_to_string(&expected_path)?;
        if output == expected {
            println!("  {PASS}");
            Ok(true)
        } else {
            println!("  {FAIL}: output differs from expected");
            println!("--- expected ---");
            print!("{expected}");
            println!("--- actual ---");
            print!("{output}");
            Ok(false)
        }
    } else {
        println!("  No expected output file, run with --bless to create");
        println!("--- output ---");
        print!("{output}");
        Ok(false)
    }
}
