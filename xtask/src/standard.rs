//! Standard test runner: single run, compare output against expected file.

use std::fs;
use std::path::PathBuf;

use anyhow::Result;

use crate::build::project_root;
use crate::defmt;
use crate::qemu::run_qemu;
use crate::runner::RunOptions;

/// Run a standard test.
///
/// Executes the example once and compares output against an expected file.
pub fn run_standard(example: &str, elf_path: &PathBuf, opts: &RunOptions) -> Result<bool> {
    println!("Running in QEMU...");
    let output = run_qemu(elf_path, None)?;
    let semihosting = defmt::decode_output(elf_path, &output.semihosting)?;
    let uart0 = defmt::decode_output(elf_path, &output.uart0)?;

    if opts.verbose {
        print!("{semihosting}");
        println!("--- QEMU run end ---");

        if !output.uart0.is_empty() {
            if semihosting != uart0 {
                println!("ERROR: Semihosting and UART output differs");
                println!("--- semihosting ---");
                print!("{semihosting}");
                println!("--- uart ---");
                print!("{uart0}");
                return Ok(false);
            } else {
                println!("PASS: Semihosting and UART output is equal");
            }
        }
        return Ok(true);
    }

    // Test mode: compare against expected file.
    let root = project_root();
    let expected_path = root
        .join("testsuite")
        .join("expected")
        .join(format!("{example}.expected"));

    if opts.bless {
        let filename = expected_path.file_name().unwrap().to_string_lossy();
        let status = if expected_path.exists() {
            let existing = fs::read_to_string(&expected_path)?;
            if existing == semihosting {
                "No change"
            } else {
                fs::write(&expected_path, &semihosting)?;
                "Updated"
            }
        } else {
            fs::create_dir_all(expected_path.parent().unwrap())?;
            fs::write(&expected_path, &semihosting)?;
            "Created"
        };
        println!("  {filename}: {status}");
        Ok(true)
    } else if expected_path.exists() {
        let expected = fs::read_to_string(&expected_path)?;
        if semihosting == expected && uart0 == expected {
            println!("  PASS");
            Ok(true)
        } else {
            println!("  FAIL: output differs from expected");
            println!("--- expected ---");
            print!("{expected}");
            println!("--- semihosting ---");
            print!("{semihosting}");
            println!("--- uart ---");
            print!("{uart0}");
            Ok(false)
        }
    } else {
        println!("  No expected output file, run with --bless to create");
        println!("--- output ---");
        print!("{semihosting}");
        Ok(false)
    }
}
