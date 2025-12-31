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
    /// Two-phase run with memory snapshot between phases.
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

/// Run a two-phase persist test.
fn run_persist(example: &str, elf_path: &PathBuf, opts: &RunOptions) -> Result<bool> {
    // Phase 1: Run and capture persist region.
    println!("Phase 1: Running...");
    let phase1 = run_qemu(elf_path, None)?;
    let phase1_uart0 = defmt::decode_output(elf_path, &phase1.uart0)?;

    if opts.verbose {
        let phase1_semihosting = defmt::decode_output(elf_path, &phase1.semihosting)?;
        println!("--- semihosting ---");
        print!("{phase1_semihosting}");
        println!("--- uart ---");
        print!("{phase1_uart0}");
        println!("--- Phase 1 end ---");
    }

    if phase1.uart1.is_empty() {
        println!("  {FAIL}: no persist region captured in phase 1");
        return Ok(false);
    }

    if opts.verbose {
        println!(
            "Captured {} bytes from persist region\n",
            phase1.uart1.len()
        );
    }

    // Phase 2: Load snapshot and run again.
    let snapshot_file = NamedTempFile::new().context("Failed to create snapshot file")?;
    fs::write(snapshot_file.path(), &phase1.uart1)?;

    println!("Phase 2: Running with snapshot...");
    let phase2 = run_qemu(
        elf_path,
        Some(MemoryLoad {
            file: &snapshot_file.path().to_path_buf(),
            addr: PERSIST_ADDR,
        }),
    )?;
    let phase2_uart0 = defmt::decode_output(elf_path, &phase2.uart0)?;

    if opts.verbose {
        let phase2_semihosting = defmt::decode_output(elf_path, &phase2.semihosting)?;
        println!("--- semihosting ---");
        print!("{phase2_semihosting}");
        println!("--- uart ---");
        print!("{phase2_uart0}");
        println!("--- Phase 2 end ---\n");
    }

    let combined = format!("=== Run 1 ===\n{phase1_uart0}\n=== Run 2 ===\n{phase2_uart0}");
    compare_expected(example, &combined, opts)
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
