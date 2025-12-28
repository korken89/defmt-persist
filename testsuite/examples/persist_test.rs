//! @test-run: persist
//! @test-validate: expected
//!
//! Persistence test that runs in two phases:
//!
//! Phase 1 (fresh start): Write logs and dump persist region via UART1.
//! Phase 2 (with snapshot): Read recovered logs and compare against expected.

#![no_std]
#![no_main]

use testsuite::{drain_to_uart, dump_persist_region, entry, exit_failure, exit_success};

#[entry]
fn main() -> ! {
    let mut consumer = defmt_persist::init().unwrap();

    // Check if there's any data to read (indicates recovered buffer).
    let first_read = consumer.read();
    let has_data = !first_read.buf().is_empty();

    if has_data {
        // Phase 2: Read recovered logs and output via UART0.
        first_read.release(0);
        drain_to_uart(&mut consumer);
        exit_success();
    } else {
        // Phase 1: Write logs, dump persist region, then read logs.
        first_read.release(0);

        // Test all log levels
        defmt::println!("println: Hello from defmt-persist!");
        defmt::error!("error: This is an error message");
        defmt::warn!("warn: This is a warning message");
        defmt::info!("info: This is an info message");
        defmt::debug!("debug: This is a debug message");
        defmt::trace!("trace: This is a trace message");

        // Dump persist region via UART1 BEFORE reading (reading consumes the data).
        dump_persist_region();

        // Read all written logs and send via UART0 (for comparison with Phase 2).
        drain_to_uart(&mut consumer);

        exit_success();
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    defmt::error!("{}", defmt::Display2Format(info));
    exit_failure();
}
