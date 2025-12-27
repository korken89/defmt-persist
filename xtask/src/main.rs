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
        .arg("build")
        .arg("--example")
        .arg(example)
        .arg("--target")
        .arg("thumbv7m-none-eabi");

    if release {
        cmd.arg("--release");
    }

    let status = cmd
        .status()
        .context("Failed to run cargo build")?;

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

fn run_qemu(elf_path: &PathBuf) -> Result<()> {
    let status = Command::new("qemu-system-arm")
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
        .status()
        .context("Failed to run QEMU")?;

    if !status.success() {
        bail!("QEMU exited with error: {:?}", status.code());
    }

    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Qemu { example, release } => {
            println!("Building example '{example}'...");
            let elf_path = build_example(&example, release)?;
            println!("Running in QEMU...");
            run_qemu(&elf_path)?;
        }
    }

    Ok(())
}
