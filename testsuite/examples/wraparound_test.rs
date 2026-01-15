//! @test-run: single
//! @test-validate: expected
//!
//! Test that verifies ring buffer wraparound behavior.
//!
//! Logs 1000 messages to guarantee multiple buffer wraparounds,
//! then reads them all back. With a 1KB buffer, this will wrap
//! around many times.

#![no_std]
#![no_main]

use testsuite::{drain_to_uart, entry, exit_failure, exit_success};

#[entry]
fn main() -> ! {
    let metadata = defmt_persist::init(|old| old.clone()).unwrap();
    let mut consumer = metadata.consumer;

    // Log 1000 messages - this will wrap around the buffer many times.
    for i in 0..1000u32 {
        defmt::info!("wraparound test: message {}", i);

        // Drain the buffer periodically to avoid overflow.
        if i.is_multiple_of(20) {
            drain_to_uart(&mut consumer);
        }
    }

    // Drain any remaining messages.
    drain_to_uart(&mut consumer);

    exit_success();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    defmt::error!("{}", defmt::Display2Format(info));
    exit_failure();
}
