//! QEMU runner for Cortex-M3 emulation.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use tempfile::NamedTempFile;

/// Output from running QEMU.
pub struct QemuOutput {
    /// defmt output from semihosting (stdout).
    pub semihosting: Vec<u8>,
    /// UART0 output (defmt ring buffer content).
    pub uart0: Vec<u8>,
    /// UART1 output (raw persist region dump).
    pub uart1: Vec<u8>,
}

/// Optional data to pre-load into memory before running.
pub struct MemoryLoad<'a> {
    pub file: &'a PathBuf,
    pub addr: u32,
}

/// Run an ELF in QEMU with optional memory pre-loading.
pub fn run_qemu(elf_path: &PathBuf, memory_load: Option<MemoryLoad>) -> Result<QemuOutput> {
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
