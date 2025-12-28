//! @test-run: persist
//! @test-validate: expected
//!
//! Panic persistence test that runs in two phases:
//!
//! Phase 1 (fresh start): Panic and dump persist region via UART1.
//! Phase 2 (with snapshot): Read recovered panic and compare against expected output.
//!
//! The xtask runs this twice:
//! 1. First run: captures UART1 (persist region dump).
//! 2. Second run: pre-loads persist region via QEMU loader, compares UART0 to expected.

#![no_std]
#![no_main]

use testsuite::{drain_to_uart, dump_persist_region, entry, exit_success};

#[entry]
fn main() -> ! {
    let mut consumer = defmt_persist::init().unwrap();

    // Check if there's any data to read (indicates recovered buffer).
    let first_read = consumer.read();
    let has_data = !first_read.buf().is_empty();

    if has_data {
        // Phase 2: Read recovered logs and output via UART0.
        defmt::info!("Some text during second run after a panic.");

        first_read.release(0);
        drain_to_uart(&mut consumer);

        exit_success();
    } else {
        // Phase 1: Write logs, dump persist region, then read logs.
        first_read.release(0);

        defmt::info!("Some text before a panic.");

        panic!("Hello from panic message!");
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    defmt::error!("{}", defmt::Display2Format(info));

    dump_persist_region();

    exit_success();
}
