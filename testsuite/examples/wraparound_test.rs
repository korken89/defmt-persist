//! Test that verifies ring buffer wraparound behavior.
//!
//! Logs 1000 messages to guarantee multiple buffer wraparounds,
//! then reads them all back. With a 1KB buffer, this will wrap
//! around many times.

#![no_std]
#![no_main]

use testsuite::{entry, exit_success, uart};

#[entry]
fn main() -> ! {
    let mut consumer = defmt_persist::init().unwrap();

    // Log 1000 messages - this will wrap around the buffer many times.
    for i in 0..1000u32 {
        defmt::info!("wraparound test: message {}", i);

        // Drain the buffer periodically to avoid overflow.
        if i.is_multiple_of(20) {
            loop {
                let data = consumer.read();
                if data.buf().is_empty() {
                    break;
                }
                uart::write_bytes(data.buf());
                data.release(0xffffffff);
            }
        }
    }

    // Drain any remaining messages.
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
