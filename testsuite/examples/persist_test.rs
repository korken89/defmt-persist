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

    if !consumer.is_empty() {
        // Phase 2: Read recovered logs.
        defmt::info!("This message will only be in the second run!");
        drain_to_uart(&mut consumer);
        exit_success();
    } else {
        // Phase 1: Write logs, dump persist region, then drain.
        defmt::println!("println: Hello from defmt-persist!");
        defmt::error!("error: This is an error message");
        defmt::warn!("warn: This is a warning message");
        defmt::info!("info: This is an info message");
        defmt::debug!("debug: This is a debug message");
        defmt::trace!("trace: This is a trace message");

        // Dump BEFORE draining (draining consumes the data).
        dump_persist_region();

        defmt::info!("This message will only be in the first run!");
        drain_to_uart(&mut consumer);

        exit_success();
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    defmt::error!("{}", defmt::Display2Format(info));
    exit_failure();
}
