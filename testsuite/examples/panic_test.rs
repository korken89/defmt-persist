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
    let metadata = defmt_persist::init(|old| old.clone()).unwrap();
    let mut consumer = metadata.consumer;

    if !consumer.is_empty() {
        // Phase 2: Read recovered logs and output via UART0.
        defmt::info!("Some text during second run after a panic.");
        drain_to_uart(&mut consumer);
        exit_success();
    } else {
        // Phase 1: Write logs, dump persist region, then read logs.
        defmt::info!("Some text before a panic, that had time to drain.");
        drain_to_uart(&mut consumer);

        defmt::info!("Some text before a panic, that did NOT have time to drain before panic.");
        panic!("Hello from panic message!");
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    defmt::error!("{}", defmt::Display2Format(info));

    dump_persist_region();

    exit_success();
}
