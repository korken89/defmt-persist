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

use testsuite::{dump_persist_region, entry, exit_failure, exit_success, uart};

#[entry]
fn main() -> ! {
    let mut consumer = defmt_persist::init().unwrap();

    // Check if there's any data (indicates recovered buffer).
    let first_read = consumer.read();
    let has_data = !first_read.buf().is_empty();

    if has_data {
        // Phase 3: Buffer was recovered - output what we got.
        // (This happens when valid snapshot is loaded)
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
    } else {
        // Phase 1 or 2: Buffer is empty (fresh init or corruption detected).
        first_read.release(0);

        // Write test logs.
        defmt::info!("corrupt test: fresh buffer");

        // Dump persist region via UART1.
        dump_persist_region();

        // Output logs via UART0.
        loop {
            let data = consumer.read();
            if data.buf().is_empty() {
                break;
            }
            uart::write_bytes(data.buf());
            data.release(0xffffffff);
        }
    }

    exit_success();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    defmt::error!("{}", defmt::Display2Format(info));
    exit_failure();
}
