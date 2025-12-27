//! @test-mode: persist
//!
//! Persistence test that runs in two phases:
//!
//! Phase 1 (fresh start): Write logs and dump persist region via UART1
//! Phase 2 (with snapshot): Read recovered logs and output via UART0
//!
//! The xtask runs this twice:
//! 1. First run: captures UART1 (persist region dump)
//! 2. Second run: pre-loads persist region via QEMU loader, captures UART0

#![no_std]
#![no_main]

use testsuite::{dump_persist_region, entry, exit_success, uart};

#[entry]
fn main() -> ! {
    let Some(mut consumer) = defmt_persist::init() else {
        panic!("defmt-persist already initialized (or failed)");
    };

    // Check if there's any data to read (indicates recovered buffer)
    let first_read = consumer.read();
    let has_data = !first_read.buf().is_empty();

    if has_data {
        // Phase 2: Read recovered logs and output via UART0
        uart::write_bytes(first_read.buf());
        first_read.release(0xffffffff);

        loop {
            let data = consumer.read();
            if data.buf().is_empty() {
                break;
            }
            uart::write_bytes(data.buf());
            data.release(0xffffffff);
        }

        exit_success();
    } else {
        // Phase 1: Write logs, dump persist region, then read logs
        first_read.release(0);

        // Test all log levels
        defmt::println!("println: Hello from defmt-persist!");
        defmt::error!("error: This is an error message");
        defmt::warn!("warn: This is a warning message");
        defmt::info!("info: This is an info message");
        defmt::debug!("debug: This is a debug message");
        defmt::trace!("trace: This is a trace message");

        // Dump persist region via UART1 BEFORE reading (reading consumes the data)
        dump_persist_region();

        // Read all written logs and send via UART0 (for comparison with Phase 2)
        loop {
            let data = consumer.read();
            if data.buf().is_empty() {
                break;
            }
            uart::write_bytes(data.buf());
            data.release(0xffffffff);
        }

        exit_success();
    }
}
