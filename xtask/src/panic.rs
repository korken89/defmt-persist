//! Panic test runner: two-phase run with expected output comparison.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

use crate::build::project_root;
use crate::defmt;
use crate::persist::PERSIST_ADDR;
use crate::qemu::{MemoryLoad, run_qemu};
use crate::runner::RunOptions;

/// Run a panic test.
///
/// Phase 1: Trigger panic and capture persist region via UART1.
/// Phase 2: Load snapshot and compare recovered logs against expected file.
pub fn run_panic(example: &str, elf_path: &PathBuf, opts: &RunOptions) -> Result<bool> {
    // Phase 1: Trigger panic and capture persist region.
    println!("Phase 1: Triggering panic...");
    let phase1 = run_qemu(elf_path, None)?;

    if opts.verbose {
        let phase1_semihosting = defmt::decode_output(elf_path, &phase1.semihosting)?;
        println!("--- semihosting ---");
        print!("{phase1_semihosting}");
        println!("--- Phase 1 end ---");
    }

    if phase1.uart1.is_empty() {
        println!("  FAIL: no persist region captured in phase 1");
        return Ok(false);
    }

    if opts.verbose {
        println!(
            "Captured {} bytes from persist region\n",
            phase1.uart1.len()
        );
    }

    // Phase 2: Load snapshot and read recovered logs.
    let snapshot_file = NamedTempFile::new().context("Failed to create snapshot file")?;
    fs::write(snapshot_file.path(), &phase1.uart1)?;

    println!("Phase 2: Reading recovered logs...");
    let phase2 = run_qemu(
        elf_path,
        Some(MemoryLoad {
            file: &snapshot_file.path().to_path_buf(),
            addr: PERSIST_ADDR,
        }),
    )?;
    let phase2_uart0 = defmt::decode_output(elf_path, &phase2.uart0)?;

    if opts.verbose {
        println!("--- uart ---");
        print!("{phase2_uart0}");
        println!("--- Phase 2 end ---\n");
    }

    // Compare against expected file.
    let root = project_root();
    let expected_path = root
        .join("testsuite")
        .join("expected")
        .join(format!("{example}.expected"));

    if opts.bless {
        let filename = expected_path.file_name().unwrap().to_string_lossy();
        let status = if expected_path.exists() {
            let existing = fs::read_to_string(&expected_path)?;
            if existing == phase2_uart0 {
                "No change"
            } else {
                fs::write(&expected_path, &phase2_uart0)?;
                "Updated"
            }
        } else {
            fs::create_dir_all(expected_path.parent().unwrap())?;
            fs::write(&expected_path, &phase2_uart0)?;
            "Created"
        };
        println!("  {filename}: {status}");
        Ok(true)
    } else if expected_path.exists() {
        let expected = fs::read_to_string(&expected_path)?;
        if phase2_uart0 == expected {
            println!("  PASS");
            Ok(true)
        } else {
            println!("  FAIL: output differs from expected");
            println!("--- expected ---");
            print!("{expected}");
            println!("--- recovered ---");
            print!("{phase2_uart0}");
            Ok(false)
        }
    } else {
        println!("  No expected output file, run with --bless to create");
        println!("--- recovered output ---");
        print!("{phase2_uart0}");
        Ok(false)
    }
}
