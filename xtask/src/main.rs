mod defmt;

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

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

fn run_qemu_raw(elf_path: &PathBuf) -> Result<Vec<u8>> {
    let output = Command::new("qemu-system-arm")
        .arg("-cpu")
        .arg("cortex-m3")
        .arg("-machine")
        .arg("lm3s6965evb")
        .arg("-nographic")
        .arg("-semihosting-config")
        .arg("enable=on,target=native")
        .arg("-kernel")
        .arg(elf_path)
        .stdin(Stdio::null())
        .output()
        .context("Failed to run QEMU")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "QEMU exited with error: {:?}\n{}",
            output.status.code(),
            stderr
        );
    }

    Ok(output.stdout)
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

fn run_test(example: &str, bless: bool) -> Result<bool> {
    let root = project_root();
    let expected_path = root
        .join("testsuite")
        .join("expected")
        .join(format!("{example}.expected"));

    println!("Building '{example}'...");
    let elf_path = build_example(example, false)?;

    println!("Running in QEMU...");
    let raw_output = run_qemu_raw(&elf_path)?;
    let output = defmt::decode_output(&elf_path, &raw_output)?;

    if bless {
        let filename = expected_path.file_name().unwrap().to_string_lossy();
        let status = if expected_path.exists() {
            let existing = fs::read_to_string(&expected_path)?;
            if existing == output {
                "No change"
            } else {
                fs::write(&expected_path, &output)?;
                "Updated"
            }
        } else {
            fs::create_dir_all(expected_path.parent().unwrap())?;
            fs::write(&expected_path, &output)?;
            "Created"
        };
        println!("  {filename}: {status} ");
        Ok(true)
    } else if expected_path.exists() {
        let expected = fs::read_to_string(&expected_path)?;
        if output == expected {
            println!("  PASS");

            Ok(true)
        } else {
            println!("  FAIL: output differs from expected");
            println!("--- expected ---");
            println!("{expected}");
            println!("--- actual ---");
            println!("{output}");

            Ok(false)
        }
    } else {
        println!("  No expected output file, run with --bless to create");
        println!("--- output ---");
        println!("{output}");

        Ok(false)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Qemu { example, release } => {
            println!("Building example '{example}'...");
            let elf_path = build_example(&example, release)?;
            println!("Running in QEMU...");
            let raw_output = run_qemu_raw(&elf_path)?;
            let output = defmt::decode_output(&elf_path, &raw_output)?;
            print!("{output}");
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

            let mut passed = 0;
            let mut failed = 0;

            for example in &examples {
                println!("\n=== Test: {example} ===");
                match run_test(example, bless) {
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
