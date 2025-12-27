#![no_std]
#![no_main]

use testsuite::{entry, exit_success};

#[entry]
fn main() -> ! {
    let consumer = defmt_persist::init();

    match consumer {
        Some(_consumer) => {
            defmt::info!("defmt-persist initialized successfully");
        }
        None => {
            defmt::error!("defmt-persist already initialized (or failed)");
        }
    }

    // Test all log levels
    defmt::println!("println: Hello from defmt-persist!");
    defmt::error!("error: This is an error message");
    defmt::warn!("warn: This is a warning message");
    defmt::info!("info: This is an info message");
    defmt::debug!("debug: This is a debug message");
    defmt::trace!("trace: This is a trace message");

    exit_success();
}
