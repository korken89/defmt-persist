//! Test runner dispatch and common types.

use std::fs;
use std::path::PathBuf;

use anyhow::Result;

use crate::build::{build_example, project_root};
use crate::corrupt::run_corrupt;
use crate::persist::run_persist;
use crate::standard::run_standard;

/// Test mode detected from file header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestMode {
    /// Standard test: single run, compare output.
    Standard,
    /// Persistence test: two-phase run with memory snapshot.
    Persist,
    /// Corruption test: verify buffer handles corrupted persist region.
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

/// Detect test mode from file header.
///
/// Looks for `@test-mode: <mode>` in the first few lines.
fn detect_test_mode(example_path: &PathBuf) -> TestMode {
    if let Ok(content) = fs::read_to_string(example_path) {
        for line in content.lines().take(10) {
            if let Some(mode) = line.strip_prefix("//! @test-mode:") {
                match mode.trim() {
                    "persist" => return TestMode::Persist,
                    "corrupt" => return TestMode::Corrupt,
                    _ => {}
                }
            }
        }
    }
    TestMode::Standard
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
    let test_mode = detect_test_mode(&example_path);

    println!("Building '{example}'...");
    let elf_path = build_example(example, opts.release)?;

    match test_mode {
        TestMode::Standard => run_standard(example, &elf_path, opts),
        TestMode::Persist => run_persist(&elf_path, opts),
        TestMode::Corrupt => run_corrupt(&elf_path, opts),
    }
}
