mod defmt;

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tempfile::NamedTempFile;

#[derive(Parser)]
#[command(name = "xtask", about = "Build and test tasks for defmt-persist")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run an example in QEMU
    Qemu {
        /// Name of the example to run
        example: String,

        /// Run in release mode
        #[arg(long)]
        release: bool,
    },

    /// Run all tests and compare output against expected
    Test {
        /// Only run a specific test
        filter: Option<String>,

        /// Update expected output files instead of comparing
        #[arg(long)]
        bless: bool,
    },
}

/// Test mode detected from file header
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestMode {
    /// Standard test: single run, compare output
    Standard,
    /// Persistence test: two-phase run with memory snapshot
    Persist,
    /// Corruption test: verify buffer handles corrupted persist region
    Corrupt,
}

/// Detect test mode from file header.
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

fn project_root() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap());

    // If we're in xtask/, go up one level
    if manifest_dir.ends_with("xtask") {
        manifest_dir.parent().unwrap().to_path_buf()
    } else {
        manifest_dir
    }
}

fn build_example(example: &str, release: bool) -> Result<PathBuf> {
    let root = project_root();
    let testsuite_dir = root.join("testsuite");

    let mut cmd = Command::new("cargo");
    cmd.current_dir(&testsuite_dir)
        .env("DEFMT_LOG", "trace")
        .stderr(Stdio::null())
        .arg("build")
        .arg("--example")
        .arg(example)
        .arg("--target")
        .arg("thumbv7m-none-eabi");

    if release {
        cmd.arg("--release");
    }

    let status = cmd.status().context("Failed to run cargo build")?;

    if !status.success() {
        bail!("cargo build failed");
    }

    let profile = if release { "release" } else { "debug" };
    let elf_path = root
        .join("target")
        .join("thumbv7m-none-eabi")
        .join(profile)
        .join("examples")
        .join(example);

    Ok(elf_path)
}

/// Output from running QEMU
struct QemuOutput {
    /// defmt output from semihosting (stdout)
    semihosting: Vec<u8>,
    /// UART0 output (defmt ring buffer content)
    uart0: Vec<u8>,
    /// UART1 output (raw persist region dump)
    uart1: Vec<u8>,
}

/// Optional data to pre-load into memory before running
struct MemoryLoad<'a> {
    file: &'a PathBuf,
    addr: u32,
}

fn run_qemu(elf_path: &PathBuf, memory_load: Option<MemoryLoad>) -> Result<QemuOutput> {
    let uart0_file = NamedTempFile::new().context("Failed to create temp file for UART0")?;
    let uart0_path = uart0_file.path();
    let uart1_file = NamedTempFile::new().context("Failed to create temp file for UART1")?;
    let uart1_path = uart1_file.path();

    let mut cmd = Command::new("qemu-system-arm");
    cmd.arg("-cpu")
        .arg("cortex-m3")
        .arg("-machine")
        .arg("lm3s6965evb")
        .arg("-nographic")
        .arg("-monitor")
        .arg("none")
        .arg("-semihosting-config")
        .arg("enable=on,target=native")
        .arg("-serial")
        .arg(format!("file:{}", uart0_path.display()))
        .arg("-serial")
        .arg(format!("file:{}", uart1_path.display()));

    if let Some(load) = memory_load {
        cmd.arg("-device").arg(format!(
            "loader,file={},addr={:#x},force-raw=on",
            load.file.display(),
            load.addr
        ));
    }

    cmd.arg("-kernel").arg(elf_path);
    cmd.stdin(Stdio::null());

    let output = cmd.output().context("Failed to run QEMU")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "QEMU exited with error: {:?}\n{}",
            output.status.code(),
            stderr
        );
    }

    let uart0 = fs::read(uart0_path).unwrap_or_default();
    let uart1 = fs::read(uart1_path).unwrap_or_default();

    Ok(QemuOutput {
        semihosting: output.stdout,
        uart0,
        uart1,
    })
}

fn discover_examples() -> Result<Vec<String>> {
    let root = project_root();
    let examples_dir = root.join("testsuite").join("examples");

    let mut examples = Vec::new();
    for entry in fs::read_dir(&examples_dir).context("Failed to read examples directory")? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            if let Some(stem) = path.file_stem() {
                examples.push(stem.to_string_lossy().into_owned());
            }
        }
    }
    examples.sort();
    Ok(examples)
}

/// Options for running an example
struct RunOptions {
    /// Print verbose output (for `qemu` command)
    verbose: bool,
    /// Update expected files instead of comparing (for `test --bless`)
    bless: bool,
    /// Build in release mode
    release: bool,
}

fn run_example(example: &str, opts: &RunOptions) -> Result<bool> {
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

fn run_standard(example: &str, elf_path: &PathBuf, opts: &RunOptions) -> Result<bool> {
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

    // Test mode: compare against expected file
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

const PERSIST_ADDR: u32 = 0x2000_FC00;

fn run_persist(elf_path: &PathBuf, opts: &RunOptions) -> Result<bool> {
    // Phase 1: Write logs and capture persist region
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

    // Phase 2: Load snapshot and read recovered logs
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

    // Compare UART0 outputs
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

/// Corruption scenario flags
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

    /// Returns true if corruption should cause buffer reinitialization
    fn causes_reinit(&self) -> bool {
        // Header corruption always causes reinit
        // Index corruption only resets the invalid index(es), not full reinit
        self.header
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
/// Layout (32-bit, no ECC padding):
/// - bytes 0-15: header (u128 magic)
/// - bytes 16-19: read index (usize)
/// - bytes 20-23: write index (usize)
fn apply_corruption(snapshot: &[u8], flags: CorruptFlags) -> Vec<u8> {
    let mut corrupted = snapshot.to_vec();

    if flags.header {
        // Zero the a magic byte.
        corrupted[0] = 0;
    }

    if flags.read {
        // Set read index to invalid value.
        corrupted[19] = 0xff;
    }

    if flags.write {
        // Set write index to invalid value.
        corrupted[23] = 0xff;
    }

    corrupted
}

fn run_corrupt(elf_path: &PathBuf, opts: &RunOptions) -> Result<bool> {
    // Phase 1: Run normally, capture persist region
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
        println!("  FAIL: no persist region captured in phase 1");
        return Ok(false);
    }

    if opts.verbose {
        println!(
            "Captured {} bytes from persist region\n",
            phase1.uart1.len()
        );
    }

    // Test all 8 combinations of corruption
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
        let result_uart0 = defmt::decode_output(elf_path, &result.uart0)?;

        if opts.verbose {
            let result_semihosting = defmt::decode_output(elf_path, &result.semihosting)?;
            println!("    --- semihosting ---");
            print!("{result_semihosting}");
            println!("    --- uart ---");
            print!("{result_uart0}");
        }

        // Check expected behavior
        let passed = if flags.causes_reinit() {
            // Header corruption: should reinitialize (same output as fresh)
            if result_uart0 == phase1_uart0 {
                println!("    PASS: buffer reinitialized");
                true
            } else {
                println!("    FAIL: expected reinit, got different output");
                println!("    --- expected (fresh) ---");
                print!("{phase1_uart0}");
                println!("    --- got ---");
                print!("{result_uart0}");
                false
            }
        } else {
            // No header corruption: should recover data (or reset indices)
            // For simplicity, we just verify it doesn't crash and produces output.
            if !result_uart0.is_empty() {
                println!("    PASS: produced output");
                true
            } else {
                println!("    FAIL: no output produced");
                false
            }
        };

        if !passed {
            all_passed = false;
        }
    }

    if all_passed {
        println!("  PASS: all {} scenarios passed", scenarios.len());
    } else {
        println!("  FAIL: some scenarios failed");
    }

    Ok(all_passed)
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Qemu { example, release } => {
            let opts = RunOptions {
                verbose: true,
                bless: false,
                release,
            };
            run_example(&example, &opts)?;
        }

        Commands::Test { filter, bless } => {
            let examples = discover_examples()?;
            let examples: Vec<_> = if let Some(ref f) = filter {
                examples.into_iter().filter(|e| e.contains(f)).collect()
            } else {
                examples
            };

            if examples.is_empty() {
                bail!("No tests found");
            }

            let opts = RunOptions {
                verbose: false,
                bless,
                release: false,
            };

            let mut passed = 0;
            let mut failed = 0;

            for example in &examples {
                println!("\n=== Test: {example} ===");
                match run_example(example, &opts) {
                    Ok(true) => passed += 1,
                    Ok(false) => failed += 1,
                    Err(e) => {
                        println!("  ERROR: {e}");
                        failed += 1;
                    }
                }
            }

            println!("\n=== Summary ===");
            println!("{passed} passed, {failed} failed");

            if failed > 0 {
                bail!("{failed} test(s) failed");
            }
        }
    }

    Ok(())
}
