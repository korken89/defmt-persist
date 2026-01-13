//! @test-run: persist
//! @test-validate: corrupt
//!
//! Corruption test that verifies the ring buffer handles corrupted persist regions.
//!
//! Phase 1: Write logs and dump persist region (normal operation).
//! Phase 2: Load corrupted snapshot, verify buffer reinitializes (no old data).
//! Phase 3: Verify new logs can be written after recovery.

#![no_std]
#![no_main]

use testsuite::{drain_to_uart, dump_persist_region, entry, exit_failure, exit_success};

#[entry]
fn main() -> ! {
    let metadata = defmt_persist::init().unwrap();
    let mut consumer = metadata.consumer;

    if !consumer.is_empty() {
        // Phase 3: Buffer was recovered (valid snapshot loaded).
        drain_to_uart(&mut consumer);
    } else {
        // Phase 1 or 2: Buffer is empty (fresh init or corruption detected).
        defmt::info!("corrupt test: fresh buffer");
        dump_persist_region();
        drain_to_uart(&mut consumer);
    }

    exit_success();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    defmt::error!("{}", defmt::Display2Format(info));
    exit_failure();
}
