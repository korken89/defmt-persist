//! Test for the async-await API.
//!
//! This test verifies that `Consumer::wait_for_data()` works correctly
//! by running a writer and reader task concurrently using join.

#![no_std]
#![no_main]

use defmt_persist::Consumer;
use testsuite::{block_on, entry, exit_success, join, uart, yield_once};

/// Async task that writes log messages with yields in between.
async fn writer_task() {
    // Yield to let reader start waiting.
    yield_once().await;

    defmt::info!("async test: message 1");
    yield_once().await;

    defmt::info!("async test: message 2");
    yield_once().await;

    defmt::info!("async test: message 3");
}

/// Async task that waits for data and reads it.
async fn reader_task(consumer: &mut Consumer<'static>) {
    for _ in 0..3 {
        // Wait for data to be available.
        consumer.wait_for_data().await;

        // Read and output all available data.
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

#[entry]
fn main() -> ! {
    let Some(mut consumer) = defmt_persist::init() else {
        panic!("defmt-persist already initialized (or failed)");
    };

    block_on(join(writer_task(), reader_task(&mut consumer)));

    exit_success();
}
