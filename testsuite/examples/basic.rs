#![no_std]
#![no_main]

use testsuite::{entry, exit_success, uart};

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
