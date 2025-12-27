mod build;
mod corrupt;
mod defmt;
mod persist;
mod qemu;
mod runner;
mod standard;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use build::discover_examples;
use runner::{RunOptions, run_example};

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

        /// Run in release mode
        #[arg(long)]
        release: bool,
    },
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

        Commands::Test {
            filter,
            bless,
            release,
        } => {
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
                release,
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
