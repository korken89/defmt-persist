//! Corruption test runner: verify buffer handles corrupted persist region.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

use crate::defmt;
use crate::qemu::{MemoryLoad, run_qemu};
use crate::runner::{FAIL, PASS, PERSIST_ADDR, RunOptions};

/// Corruption scenario flags.
#[derive(Debug, Clone, Copy)]
struct CorruptFlags {
    header: bool,
    read: bool,
    write: bool,
}

impl CorruptFlags {
    fn name(&self) -> String {
        let mut parts = Vec::new();
        if self.header {
            parts.push("header");
        }
        if self.read {
            parts.push("read");
        }
        if self.write {
            parts.push("write");
        }
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join("+")
        }
    }

    /// Returns true if any corruption is present.
    fn any(&self) -> bool {
        self.header || self.read || self.write
    }

    /// A list of all combinations.
    fn all_combinations() -> [Self; 8] {
        [
            CorruptFlags {
                header: false,
                read: false,
                write: false,
            },
            CorruptFlags {
                header: true,
                read: false,
                write: false,
            },
            CorruptFlags {
                header: false,
                read: true,
                write: false,
            },
            CorruptFlags {
                header: false,
                read: false,
                write: true,
            },
            CorruptFlags {
                header: true,
                read: true,
                write: false,
            },
            CorruptFlags {
                header: true,
                read: false,
                write: true,
            },
            CorruptFlags {
                header: false,
                read: true,
                write: true,
            },
            CorruptFlags {
                header: true,
                read: true,
                write: true,
            },
        ]
    }
}

/// Apply corruption to a snapshot based on flags.
///
/// Layout (32-bit with ECC padding):
/// - bytes 0-15: header (u128 magic)
/// - bytes 16-19: read index (usize)
/// - bytes 20-23: _pad_read
/// - bytes 24-27: write index (usize)
/// - bytes 28-31: _pad_write
fn apply_corruption(snapshot: &[u8], flags: CorruptFlags) -> Vec<u8> {
    let mut corrupted = snapshot.to_vec();

    if flags.header {
        // Zero a magic byte.
        corrupted[0] = 0;
    }

    if flags.read {
        // Set read index to invalid value.
        corrupted[19] = 0xff;
    }

    if flags.write {
        // Set write index to invalid value.
        corrupted[27] = 0xff;
    }

    corrupted
}

/// Run a corruption test.
///
/// Tests all 8 combinations of header/read/write corruption.
pub fn run_corrupt(elf_path: &PathBuf, opts: &RunOptions) -> Result<bool> {
    // Phase 1: Run normally, capture persist region.
    println!("Phase 1: Normal run to capture persist region...");
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

    // Test all 8 combinations of corruption.
    let scenarios = CorruptFlags::all_combinations();

    let snapshot_file = NamedTempFile::new().context("Failed to create snapshot file")?;
    let mut all_passed = true;

    for (i, flags) in scenarios.iter().enumerate() {
        let corrupted = apply_corruption(&phase1.uart1, *flags);
        fs::write(snapshot_file.path(), &corrupted)?;

        println!("  Scenario {}: corrupt={}", i + 1, flags.name());

        let result = run_qemu(
            elf_path,
            Some(MemoryLoad {
                file: &snapshot_file.path().to_path_buf(),
                addr: PERSIST_ADDR,
            }),
        )?;
        let result_semihosting = defmt::decode_output(elf_path, &result.semihosting)?;
        let result_uart0 = defmt::decode_output(elf_path, &result.uart0)?;

        if opts.verbose {
            println!("    --- semihosting ---");
            print!("{result_semihosting}");
            println!("    --- uart ---");
            print!("{result_uart0}");
        }

        // Check expected behavior using semihosting output:
        // - No corruption: recovery path taken, semihosting empty (no defmt::info!() called).
        // - Any corruption: fresh path taken, semihosting has "fresh buffer" message.
        let passed = if flags.any() {
            // Any corruption should result in fresh path (semihosting has output).
            if !result_semihosting.is_empty() && result_uart0 == phase1_uart0 {
                println!("    {PASS}: buffer reinitialized");
                true
            } else {
                println!("    {FAIL}: expected fresh path");
                println!("    --- semihosting ---");
                print!("{result_semihosting}");
                println!("    --- uart ---");
                print!("{result_uart0}");
                false
            }
        } else {
            // No corruption: recovery path taken (semihosting empty, UART has recovered data).
            if result_semihosting.is_empty() && !result_uart0.is_empty() {
                println!("    {PASS}: recovered data");
                true
            } else {
                println!("    {FAIL}: expected recovery path");
                println!("    --- semihosting ---");
                print!("{result_semihosting}");
                println!("    --- uart ---");
                print!("{result_uart0}");
                false
            }
        };

        if !passed {
            all_passed = false;
        }
    }

    if all_passed {
        println!("  {PASS}: all {} scenarios passed", scenarios.len());
    } else {
        println!("  {FAIL}: some scenarios failed");
    }

    Ok(all_passed)
}
