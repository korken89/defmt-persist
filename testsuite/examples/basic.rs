//! @test-run: single
//! @test-validate: expected

#![no_std]
#![no_main]

use testsuite::{drain_to_uart, entry, exit_failure, exit_success};

#[entry]
fn main() -> ! {
    let mut consumer = defmt_persist::init().unwrap();

    // Test all log levels.
    defmt::println!("println: Hello from defmt-persist!");
    defmt::error!("error: This is an error message");
    defmt::warn!("warn: This is a warning message");
    defmt::info!("info: This is an info message");
    defmt::debug!("debug: This is a debug message");
    defmt::trace!("trace: This is a trace message");

    // Send all logs over UART as well.
    drain_to_uart(&mut consumer);

    exit_success();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    defmt::error!("{}", defmt::Display2Format(info));
    exit_failure();
}
