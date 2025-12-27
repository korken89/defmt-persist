//! Persistence test runner: two-phase run with memory snapshot.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

use crate::defmt;
use crate::qemu::{MemoryLoad, run_qemu};
use crate::runner::RunOptions;

/// Address of the persist region in memory.
pub const PERSIST_ADDR: u32 = 0x2000_FC00;

/// Run a persistence test.
///
/// Phase 1: Write logs and capture persist region via UART1.
/// Phase 2: Load snapshot and verify recovered logs match.
pub fn run_persist(elf_path: &PathBuf, opts: &RunOptions) -> Result<bool> {
    // Phase 1: Write logs and capture persist region.
    println!("Phase 1: Writing logs...");
    let phase1 = run_qemu(elf_path, None)?;
    let phase1_semihosting = defmt::decode_output(elf_path, &phase1.semihosting)?;
    let phase1_uart0 = defmt::decode_output(elf_path, &phase1.uart0)?;

    if opts.verbose {
        println!("--- semihosting ---");
        print!("{phase1_semihosting}");
        println!("--- uart ---");
        print!("{phase1_uart0}");
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
    let phase2_semihosting = defmt::decode_output(elf_path, &phase2.semihosting)?;
    let phase2_uart0 = defmt::decode_output(elf_path, &phase2.uart0)?;

    if opts.verbose {
        println!("--- semihosting ---");
        print!("{phase2_semihosting}");
        println!("--- uart ---");
        print!("{phase2_uart0}");
        println!("--- Phase 2 end ---\n");
    }

    // Compare UART0 outputs.
    if phase1_uart0 == phase2_uart0 {
        println!("  PASS: recovered logs match written logs");
        Ok(true)
    } else {
        println!("  FAIL: recovered logs don't match");
        println!("--- phase 1 (written) ---");
        print!("{phase1_uart0}");
        println!("--- phase 2 (recovered) ---");
        print!("{phase2_uart0}");
        Ok(false)
    }
}
