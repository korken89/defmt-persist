#![no_std]
#![no_main]

use cortex_m_semihosting::hprintln;
use testsuite::{entry, exit_success};

#[entry]
fn main() -> ! {
    hprintln!("Initializing defmt-persist...");

    let consumer = defmt_persist::init();

    match consumer {
        Some(_consumer) => {
            hprintln!("defmt-persist initialized successfully");
        }
        None => {
            hprintln!("defmt-persist already initialized (or failed)");
        }
    }

    hprintln!("Logging a test message...");
    defmt::info!("Hello from defmt-persist!");

    hprintln!("Test complete");
    exit_success();
}
